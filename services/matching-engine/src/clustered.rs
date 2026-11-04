//! Runs the engine as a replicated state machine inside an Aeron Cluster.
//!
//! Every node applies the same log and produces the same bytes, so nothing here reads a clock or
//! depends on being leader. Each node publishes and records its own output stream, and consumers
//! can follow any node by stream position.

use crate::handler::Handler;
use crate::metrics::LatencyMetrics;
use crate::publisher::{self, Publisher};
use crate::{on_message, types};

const SNAPSHOT_HEADER: usize = 56;
const SNAPSHOT_CHUNK: usize = 60_000;

/// Times messages on one clock: from the leader stamping them into the log to this node
/// finishing them, and matching alone.
struct Timing {
    path: String,
    matching: Vec<u64>,
    in_to_out: Vec<u64>,
    reported: usize,
    seen_at_last_check: usize,
}

impl Timing {
    fn record(&mut self, matching: u64, in_to_out: u64) {
        if self.matching.len() < self.matching.capacity() {
            self.matching.push(matching);
            self.in_to_out.push(in_to_out);
        }
    }

    fn report(&mut self) {
        if self.matching.len() == self.reported {
            return;
        }
        self.reported = self.matching.len();
        let line = |name: &str, values: &[u64]| {
            let mut sorted = values.to_vec();
            sorted.sort_unstable();
            let at = |q: f64| sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)];
            format!(
                "{name} n={} p50={} p99={} p99.9={} p99.99={} max={}\n",
                sorted.len(), at(0.5), at(0.99), at(0.999), at(0.9999), at(1.0)
            )
        };
        let warm_from = self.in_to_out.len() / 5;
        let mut worst: Vec<(u64, usize)> = self.in_to_out.iter().copied().zip(0..).collect();
        worst.sort_unstable_by(|a, b| b.cmp(a));
        // Stalls show up as runs of neighbours, so report only well separated ones.
        let mut stalls: Vec<(u64, usize)> = Vec::new();
        for (value, index) in worst {
            if stalls.iter().all(|(_, seen)| seen.abs_diff(index) > 20_000) {
                stalls.push((value, index));
                if stalls.len() == 6 {
                    break;
                }
            }
        }
        let text = line("matching_ns", &self.matching)
            + &line("in_to_out_ns", &self.in_to_out)
            + &line("warm_in_to_out_ns", &self.in_to_out[warm_from..])
            + &format!("stalls_us_at_index {:?}\n", stalls.iter().map(|(v, i)| (v / 1000, *i)).collect::<Vec<_>>());
        let _ = std::fs::write(&self.path, text);
    }
}

struct ClusteredEngine {
    timing: Option<Timing>,
    last_timing_report: std::time::Instant,
    aeron: transport::Aeron,
    output_channel: String,
    output_position: i64,
    handler: Handler,
    metrics: LatencyMetrics,
    digest_file: Option<String>,
    last_written: u64,
    last_report: std::time::Instant,
}

impl cluster::ClusteredService for ClusteredEngine {
    fn on_session_message(&mut self, timestamp: i64, _log_position: i64, payload: &[u8]) {
        self.handler.publisher.set_cluster_time(timestamp as u64);
        let match_started = self.timing.is_some().then(std::time::Instant::now);
        let mut offset = 0;
        while offset < payload.len() {
            let block_length = u16::from_le_bytes([payload[offset], payload[offset + 1]]) as usize;
            let end = offset + 8 + block_length;
            on_message(&payload[offset..end], &mut self.handler, &mut self.metrics);
            offset = end;
        }

        if let (Some(timing), Some(started)) = (&mut self.timing, match_started) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock is after the epoch")
                .as_nanos() as i64;
            timing.record(started.elapsed().as_nanos() as u64, (now - timestamp).max(0) as u64);
        }
    }

    fn on_role_change(&mut self, role: cluster::Role) {
        eprintln!("role {role:?}");
    }

    fn on_new_leadership_term(&mut self, leadership_term_id: i64) {
        self.handler.publisher.publish_term_marker(leadership_term_id);
    }

    fn on_recovered(&mut self, archive: &cluster::ArchiveClient) {
        let started = std::time::Instant::now();
        let channel = archive.prepare_output(
            &self.output_channel,
            publisher::OUTPUT_STREAM_ID,
            publisher::OUTPUT_SESSION_ID,
            self.output_position,
        );
        let publication = transport::exclusive_publication(&self.aeron, &channel, publisher::OUTPUT_STREAM_ID)
            .expect("failed to add the output publication");
        self.handler.publisher.open_output(publication);
        eprintln!("output open at {} in {:?}", self.output_position, started.elapsed());
    }

    fn on_take_snapshot(&mut self, archive: &cluster::ArchiveClient, write: &mut dyn FnMut(&[u8])) {
        let started = std::time::Instant::now();
        let output_position = self.handler.publisher.flush();
        if std::env::var("NO_OUTPUT").is_err() {
            archive.await_recorded(self.handler.publisher.session_id(), output_position);
        }
        eprintln!("output flushed in {:?}", started.elapsed());

        let handler = &self.handler;
        let (written, digest) = handler.publisher.state();
        let mut header = [0u8; SNAPSHOT_HEADER];
        header[0] = 1;
        for (i, value) in [handler.counter_order_id, handler.counter_exec_id, handler.counter_match_id, written, digest, output_position as u64].iter().enumerate() {
            header[8 + i * 8..16 + i * 8].copy_from_slice(&value.to_le_bytes());
        }
        write(&header);

        let mut chunk = Vec::with_capacity(SNAPSHOT_CHUNK);
        chunk.push(2u8);
        handler.book.for_each_resting(&mut |order| {
            types::encode_order(order, &mut chunk);
            if chunk.len() + types::ORDER_RECORD > SNAPSHOT_CHUNK {
                write(&chunk);
                chunk.truncate(1);
            }
        });
        if chunk.len() > 1 {
            write(&chunk);
        }
    }

    fn on_load_snapshot(&mut self, chunk: &[u8]) {
        match chunk[0] {
            1 => {
                let value = |i: usize| u64::from_le_bytes(chunk[8 + i * 8..16 + i * 8].try_into().expect("eight bytes"));
                self.handler.counter_order_id = value(0);
                self.handler.counter_exec_id = value(1);
                self.handler.counter_match_id = value(2);
                self.handler.publisher.restore(value(3), value(4));
                self.output_position = value(5) as i64;
            }
            _ => {
                for record in chunk[1..].chunks_exact(types::ORDER_RECORD) {
                    let order = types::decode_order(record);
                    if order.side == sbe::side_enum::SideEnum::Buy {
                        self.handler.book.add_bid(&order);
                    } else {
                        self.handler.book.add_ask(&order);
                    }
                }
            }
        }
    }

    fn do_background_work(&mut self) {
        self.handler.publisher.drain();

        // Sorting takes long enough to stall matching, so it only runs once traffic has stopped.
        if self.last_timing_report.elapsed().as_secs() >= 2 {
            self.last_timing_report = std::time::Instant::now();
            if let Some(timing) = &mut self.timing
                && timing.matching.len() == timing.seen_at_last_check
            {
                timing.report();
            } else if let Some(timing) = &mut self.timing {
                timing.seen_at_last_check = timing.matching.len();
            }
        }

        if self.last_report.elapsed().as_millis() >= 100 {
            self.last_report = std::time::Instant::now();
            let (written, digest) = self.handler.publisher.state();
            if written != self.last_written {
                self.last_written = written;
                if let Some(path) = &self.digest_file {
                    let _ = std::fs::write(path, format!("{written} {digest:016x}\n"));
                }
            }
        }
    }
}

pub fn run(aeron: &transport::Aeron) {
    let publisher = Publisher::new(None, true);
    let mut engine = ClusteredEngine {
        timing: std::env::var("TIMING_FILE").ok().map(|path| Timing {
            path,
            matching: Vec::with_capacity(8_000_000),
            in_to_out: Vec::with_capacity(8_000_000),
            reported: 0,
            seen_at_last_check: 0,
        }),
        last_timing_report: std::time::Instant::now(),
        aeron: aeron.clone(),
        output_channel: std::env::var("PUB_CHANNEL").expect("PUB_CHANNEL is required"),
        output_position: 0,
        handler: Handler::new(publisher),
        metrics: LatencyMetrics::new(100_000),
        digest_file: std::env::var("DIGEST_FILE").ok(),
        last_written: 0,
        last_report: std::time::Instant::now(),
    };
    eprintln!("service joining the cluster");
    let config = cluster::NodeConfig {
        cluster_id: 0,
        aeron_dir: transport::aeron_dir(),
        archive_control_channel: std::env::var("ARCHIVE_CONTROL").expect("ARCHIVE_CONTROL is required"),
    };
    cluster::run(aeron, &config, &mut engine);
}
