//! Runs the engine as a replicated state machine inside an Aeron Cluster.
//!
//! Every node applies the same log and produces the same bytes, so nothing here reads a clock or
//! depends on being leader. Each node publishes and records its own output stream, and consumers
//! can follow any node by stream position.
//!
//! The one exception is deciding to stop. A node that can't continue exits rather than diverging,
//! and `recovery.rs` makes those decisions.

use std::process;

use cluster::{ArchiveClient, Role, SnapshotRequest, TapeExtent, TapeMark};

use crate::admin::{self, AdminCommand};
use crate::config;
use crate::dispatch;
use crate::handler::Handler;
use crate::publisher::{OUTPUT_SESSION_ID, OUTPUT_STREAM_ID, Publisher, RecoveryEvent};
use crate::recovery::{self, EXIT_TOTAL_LOSS, Join, RebuildPlan, RebuildRequest, RecoveryConfig, Startup, TapeFold};
use crate::snapshot;

#[cfg(feature = "diagnostics")]
use crate::diagnostics::Diagnostics;

struct ClusteredEngine {
    aeron: transport::Aeron,
    output_channel: String,
    handler: Handler,
    recovery: RecoveryConfig,
    /// While set, output is held back and the tape is left untouched, because it may hold more
    /// than the log can reproduce.
    held: bool,
    loader: Option<snapshot::Loader>,
    snapshot_wanted: bool,
    tape_swept: Option<i64>,
    #[cfg(feature = "diagnostics")]
    diagnostics: Diagnostics,
}

/// Stops this node. Exiting before the ack is what keeps a leader from opening the log.
fn refuse(reason: &str) -> ! {
    eprintln!("refusing to resume: {reason}");
    process::exit(EXIT_TOTAL_LOSS);
}

fn tape(archive: &ArchiveClient) -> Option<TapeExtent> {
    archive.await_tape_extent(OUTPUT_STREAM_ID, OUTPUT_SESSION_ID)
}

impl ClusteredEngine {
    /// Opens the output stream at `position`, cutting off anything newer on the tape, and resends
    /// `unsent` reports from the ring.
    fn open_output(&mut self, archive: &ArchiveClient, position: i64, unsent: u64) {
        let channel = archive.prepare_output(&self.output_channel, OUTPUT_STREAM_ID, OUTPUT_SESSION_ID, position);
        let publication = transport::exclusive_publication(&self.aeron, &channel, OUTPUT_STREAM_ID)
            .expect("failed to add the output publication");
        self.handler.publisher.open_output(publication, unsent);
        self.held = false;
        eprintln!("output open at {position}, {unsent} reports to resend");
    }

    /// Ends a hold that replay has made safe.
    fn release_output(&mut self, archive: &ArchiveClient) {
        let position = self.handler.publisher.output_position();
        match recovery::output_start(position, tape(archive).map(|tape| tape.stop)) {
            Ok((start, unsent)) => self.open_output(archive, start, unsent),
            Err(reason) => refuse(&reason),
        }
    }

    fn rebuild(&mut self, archive: &ArchiveClient, expected: i64, target: i64) {
        let request = RebuildRequest {
            expected,
            target,
            quiet: self.handler.publisher.quiet_range(),
            durable: self.recovery.durable,
            held: self.held,
            tape: if self.held { tape(archive) } else { None },
        };
        let from = match recovery::plan_rebuild(&request) {
            RebuildPlan::Apply { from } => from,
            RebuildPlan::Ignore(reason) => return eprintln!("ignoring the rebuild: {reason}"),
            RebuildPlan::Refuse(reason) => refuse(&reason),
        };

        let mut fold = TapeFold::seed(&self.handler);
        let mut failure = Ok(());
        let read = archive.read_tape(OUTPUT_STREAM_ID, OUTPUT_SESSION_ID, from, target, &mut |message| {
            if failure.is_ok() {
                failure = fold.apply(message);
            }
        });
        if let Err(reason) = read.and(failure) {
            refuse(&reason);
        }

        fold.install(&mut self.handler);
        self.handler.publisher.rebase(target);
        self.open_output(archive, target, 0);
        if let Err(reason) = fold.settle(&mut self.handler) {
            refuse(&reason);
        }

        let open_orders = self.handler.book.pool.len();
        self.handler.publisher.publish_recovery_marker(RecoveryEvent::Rebuilt, target, open_orders as u64);
        // Replaying the log would need this exact tape again, so make the result stand on its own.
        self.snapshot_wanted = true;
        eprintln!("rebuilt the book from the tape at {target}, {open_orders} open orders");
    }

    fn on_admin(&mut self, archive: &ArchiveClient, message: &[u8]) {
        match AdminCommand::decode(message) {
            Some(AdminCommand::Rebuild { expected, position }) => self.rebuild(archive, expected, position),
            Some(AdminCommand::CancelAll) => {
                let canceled = self.handler.cancel_all();
                let position = self.handler.publisher.output_position();
                self.handler.publisher.publish_recovery_marker(RecoveryEvent::CanceledAll, position, canceled as u64);
                eprintln!("canceled {canceled} open orders");
            }
            Some(AdminCommand::TapeSwept { position }) => {
                self.tape_swept = Some(self.tape_swept.map_or(position, |swept| swept.max(position)));
            }
            None => eprintln!("ignoring a malformed admin command"),
        }
    }
}

impl cluster::ClusteredService for ClusteredEngine {
    fn on_session_message(&mut self, archive: &ArchiveClient, timestamp: i64, _log_position: i64, payload: &[u8]) {
        self.handler.publisher.set_cluster_time(timestamp as u64);
        #[cfg(feature = "diagnostics")]
        let started = self.diagnostics.message_started();

        let mut offset = 0;
        while offset < payload.len() {
            let block_length = u16::from_le_bytes([payload[offset], payload[offset + 1]]) as usize;
            let end = offset + 8 + block_length;
            let message = &payload[offset..end];
            if admin::is_admin(message) {
                self.on_admin(archive, message);
            } else {
                dispatch(message, &mut self.handler);
            }
            offset = end;
        }

        #[cfg(feature = "diagnostics")]
        self.diagnostics.message_finished(started, timestamp);
    }

    fn on_role_change(&mut self, role: Role) {
        eprintln!("role {role:?}");
    }

    fn on_new_leadership_term(&mut self, leadership_term_id: i64) {
        self.handler.publisher.publish_term_marker(leadership_term_id);
    }

    fn on_join_live_log(&mut self, archive: &ArchiveClient, role: Role, _log_position: i64) {
        let Some(durable) = self.recovery.durable.filter(|_| self.held) else { return };
        let recovered = self.handler.publisher.output_position();
        match recovery::at_join(recovered, durable, role == Role::Leader, self.recovery.rebuild) {
            Join::Release => self.release_output(archive),
            Join::Wait => eprintln!("holding output: recovered {recovered}, durable {durable}"),
            Join::Refuse => refuse(&format!(
                "total loss: the log recovered output to {recovered}, but {durable} is durable. Set \
                 RECOVERY_MODE=rebuild, then run: matching-engine admin rebuild {recovered} POSITION"
            )),
        }
    }

    fn on_recovered(&mut self, archive: &ArchiveClient) {
        if let Some(loader) = self.loader.take() {
            loader.finish(&mut self.handler);
        }
        let position = self.handler.publisher.output_position();
        if self.recovery.durable.is_none() {
            eprintln!("DURABLE_POSITION isn't set, so a total loss can't be detected");
        }
        match recovery::at_startup(position, self.recovery) {
            Startup::Open => self.open_output(archive, position, 0),
            Startup::Hold => {
                self.held = true;
                eprintln!("holding output at {position} until replay ends");
            }
        }
    }

    fn on_take_snapshot(&mut self, archive: &ArchiveClient, write: &mut dyn FnMut(&[u8])) -> TapeMark {
        let started = std::time::Instant::now();
        let position = if self.held {
            self.handler.publisher.output_position()
        } else {
            let flushed = self.handler.publisher.flush();
            archive.await_recorded(self.handler.publisher.session_id(), flushed);
            flushed
        };
        eprintln!("output flushed in {:?}", started.elapsed());

        let computed = self.handler.publisher.output_position();
        if computed != position {
            eprintln!("BUG: output is at {position}, but the engine computed {computed}");
        }

        let mut output = self.handler.publisher.state();
        output.position = position;
        snapshot::write(&self.handler, &output, write);
        self.snapshot_wanted = false;
        TapeMark { position, swept: self.tape_swept }
    }

    fn on_load_snapshot(&mut self, chunk: &[u8]) {
        let loader = self.loader.get_or_insert_with(snapshot::Loader::default);
        loader.on_chunk(&mut self.handler, chunk);
    }

    fn snapshot_request(&self) -> SnapshotRequest {
        if self.held {
            SnapshotRequest::Hold
        } else if self.snapshot_wanted {
            SnapshotRequest::Now
        } else {
            SnapshotRequest::Scheduled
        }
    }

    fn do_background_work(&mut self, archive: &ArchiveClient) {
        self.handler.publisher.drain();

        if self.held && !self.recovery.rebuild {
            let position = self.handler.publisher.output_position();
            if self.recovery.durable.is_some_and(|durable| position >= durable) {
                self.release_output(archive);
            }
        }

        #[cfg(feature = "diagnostics")]
        self.diagnostics.report(&self.handler.publisher);
    }
}

pub fn run(aeron: &transport::Aeron) {
    let mut engine = ClusteredEngine {
        aeron: aeron.clone(),
        output_channel: std::env::var("PUB_CHANNEL").expect("PUB_CHANNEL is required"),
        handler: Handler::new(Publisher::new(None, true)),
        recovery: config::recovery_config(),
        held: false,
        loader: None,
        snapshot_wanted: false,
        tape_swept: None,
        #[cfg(feature = "diagnostics")]
        diagnostics: Diagnostics::from_env(),
    };
    eprintln!("service joining the cluster");
    cluster::run(aeron, &config::node_config(), &mut engine);
}
