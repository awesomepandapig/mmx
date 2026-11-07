//! The loop shared by the market data services: read reports, turn them into updates and publish
//! those to NATS.
//!
//! Nothing here drops an update. When NATS can't keep up, updates wait in `Pump` and reading stops,
//! so this service falls behind the tape, which is expected and doesn't affect the engine.

use std::collections::VecDeque;
use std::hint::spin_loop;
use std::path::Path;
use std::process;
use std::sync::Once;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_nats::client::PublishErrorKind;
use async_nats::{Client, ConnectOptions, Subject};

use serde::Serialize;
use serde::de::DeserializeOwned;

use tokio::runtime::Builder;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, Receiver, Sender};

use tracing::subscriber::set_global_default;
use tracing::{Level, error, info, warn};
use tracing_subscriber::FmtSubscriber;

use crate::checkpoint::{self, Checkpoint};
use crate::follower::TapeConfig;
use crate::frame::{self, Frame};
use crate::messages::ExecutionReportMessage;
use crate::source::Source;

const FRAGMENT_LIMIT: usize = 100;
const QUEUE_LENGTH: usize = 1024;
const SECOND: Duration = Duration::from_secs(1);
const CHECKPOINT_EVERY: Duration = Duration::from_secs(5);
const WARN_EVERY: Duration = Duration::from_secs(5);
const FULL_QUEUE_WAIT: Duration = Duration::from_millis(1);
const RETRY_AFTER: Duration = Duration::from_millis(100);

#[derive(Debug, PartialEq, Eq)]
pub struct Update {
    pub subject: String,
    pub payload: String,
}

/// A service's state, rebuilt from reports.
pub trait Processor: Default + Serialize + DeserializeOwned {
    const NAME: &'static str;
    /// The first of this service's replay stream IDs. See `TapeConfig::replay_stream`.
    const REPLAY_STREAM: i32;

    fn on_report(&mut self, report: &ExecutionReportMessage, updates: &mut Vec<Update>);

    /// Runs once a second, with seconds since the epoch.
    fn on_second(&mut self, _now: u64, _updates: &mut Vec<Update>) {}
}

/// Holds updates that are ready to publish.
struct Pump<P> {
    state: P,
    /// Rebuilding from the start of the tape stays quiet until it reaches the live stream, so
    /// subscribers don't receive all of history. A resume publishes everything after its
    /// checkpoint: level updates carry absolute quantities, so repeats are harmless.
    publishing: bool,
    updates: Vec<Update>,
    waiting: VecDeque<Update>,
}

impl<P: Processor> Pump<P> {
    fn new(state: P, publishing: bool) -> Self {
        Self {
            state,
            publishing,
            updates: Vec::new(),
            waiting: VecDeque::new(),
        }
    }

    fn on_frame(&mut self, buffer: &[u8]) {
        match frame::parse(buffer) {
            Frame::Report(report) => {
                self.state.on_report(&report, &mut self.updates);
                self.release();
            }
            Frame::CancelReject | Frame::TermMarker(_) => {}
            Frame::Unknown(template_id) => warn!("Skipping unknown template {}", template_id),
            Frame::Malformed => warn!("Skipping a malformed message of {} bytes", buffer.len()),
        }
    }

    fn on_second(&mut self, now: u64) {
        self.state.on_second(now, &mut self.updates);
        self.release();
    }

    fn release(&mut self) {
        if self.publishing {
            self.waiting.extend(self.updates.drain(..));
        } else {
            self.updates.clear();
        }
    }

    /// Hands over what the queue will take, and returns whether it took everything.
    fn offer(&mut self, queue: &Sender<Update>) -> bool {
        while let Some(update) = self.waiting.pop_front() {
            match queue.try_send(update) {
                Ok(()) => {}
                Err(TrySendError::Full(update) | TrySendError::Closed(update)) => {
                    self.waiting.push_front(update);
                    return false;
                }
            }
        }
        true
    }
}

async fn connect(url: &str) -> Client {
    loop {
        match ConnectOptions::new()
            .retry_on_initial_connect()
            .connect(url)
            .await
        {
            Ok(client) => return client,
            Err(e) => {
                error!("NATS: Failed to connect to {}: {}", url, e);
                tokio::time::sleep(RETRY_AFTER).await;
            }
        }
    }
}

async fn publish(client: &Client, update: Update) {
    let subject = Subject::from(update.subject);
    loop {
        match client
            .publish(subject.clone(), update.payload.clone().into())
            .await
        {
            Ok(()) => return,
            Err(e) if e.kind() == PublishErrorKind::Send => {
                warn!("NATS: Retrying a publish to {}: {}", subject, e);
                tokio::time::sleep(RETRY_AFTER).await;
            }
            // It can never succeed, and retrying would stop the whole feed.
            Err(e) => {
                error!("NATS: Dropping an update for {}: {}", subject, e);
                return;
            }
        }
    }
}

async fn publish_updates(url: String, mut queue: Receiver<Update>) {
    info!("Connecting to NATS server at {}", url);
    let client = connect(&url).await;
    info!("Async I/O publisher task started.");
    while let Some(update) = queue.recv().await {
        publish(&client, update).await;
    }
}

fn save<P: Processor>(path: &Path, source: &Source, pump: &Pump<P>) {
    let Some(position) = source.position() else {
        return;
    };
    let checkpoint = Checkpoint::new(position, pump.publishing, &pump.state);
    if let Err(e) = checkpoint::save(path, &checkpoint) {
        error!("Failed to save {}: {}", path.display(), e);
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn init_tracing() {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();

    set_global_default(subscriber).unwrap_or_else(|err| {
        error!(target: "setup", kind="tracing_init_failed", error=?err, "Failed to create Tracing subscriber");
        process::exit(1);
    });
}

/// Runs a service forever. `rusteron` handles aren't `Send`, so polling stays on this thread and
/// NATS runs on the tokio runtime.
pub fn run<P: Processor>() {
    static TRACING: Once = Once::new();
    TRACING.call_once(init_tracing);

    let config = TapeConfig::from_env(P::REPLAY_STREAM).unwrap_or_else(|e| {
        error!("{}", e);
        process::exit(1);
    });
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            error!("Failed to start the runtime: {}", e);
            process::exit(1);
        });

    let (queue, receiver) = mpsc::channel(QUEUE_LENGTH);
    runtime.spawn(publish_updates(
        transport::env_or("NATS_URL", "localhost"),
        receiver,
    ));

    let path = checkpoint::path_for(P::NAME);
    let checkpoint = config.as_ref().and_then(|_| checkpoint::load::<P>(&path));
    let (position, mut pump) = match checkpoint {
        Some(saved) => {
            info!("Resuming from position {}", saved.position);
            (saved.position, Pump::new(saved.state, saved.caught_up))
        }
        None => (0, Pump::new(P::default(), config.is_none())),
    };
    let mut source = Source::open(config, position);

    let mut last_second = Instant::now();
    let mut last_checkpoint = Instant::now();
    let mut last_warning = Instant::now();

    info!("Starting Aeron polling loop...");

    loop {
        if last_checkpoint.elapsed() >= CHECKPOINT_EVERY {
            last_checkpoint = Instant::now();
            save(&path, &source, &pump);
        }

        if !pump.offer(&queue) {
            if last_warning.elapsed() >= WARN_EVERY {
                last_warning = Instant::now();
                warn!(
                    "NATS can't keep up, so reading has paused with {} updates waiting",
                    pump.waiting.len()
                );
            }
            sleep(FULL_QUEUE_WAIT);
            continue;
        }

        let read = source.poll(FRAGMENT_LIMIT, &mut |buffer, _position| {
            pump.on_frame(buffer)
        });

        if !pump.publishing && source.is_live() {
            info!("Reached the live stream, so publishing has started");
            pump.publishing = true;
        }

        if last_second.elapsed() >= SECOND {
            last_second = Instant::now();
            pump.on_second(unix_seconds());
        }

        if read == 0 {
            spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde::Deserialize;

    use crate::messages::sample_report;

    #[derive(Default, Serialize, Deserialize)]
    struct Echo {
        seconds: u64,
    }

    impl Processor for Echo {
        const NAME: &'static str = "echo";
        const REPLAY_STREAM: i32 = 0;

        fn on_report(&mut self, report: &ExecutionReportMessage, updates: &mut Vec<Update>) {
            updates.push(update(&report.order_id.to_string()));
        }

        fn on_second(&mut self, _now: u64, updates: &mut Vec<Update>) {
            self.seconds += 1;
            updates.push(update("second"));
        }
    }

    fn update(payload: &str) -> Update {
        Update {
            subject: "test.ITEM".to_string(),
            payload: payload.to_string(),
        }
    }

    fn report(pump: &mut Pump<Echo>, order_id: u64) {
        let report = ExecutionReportMessage {
            order_id,
            ..sample_report()
        };
        pump.state.on_report(&report, &mut pump.updates);
        pump.release();
    }

    #[test]
    fn a_full_queue_keeps_every_update_in_order() {
        let (queue, mut receiver) = mpsc::channel(2);
        let mut pump = Pump::new(Echo::default(), true);
        for order_id in 1..=5 {
            report(&mut pump, order_id);
        }

        assert!(!pump.offer(&queue));
        assert_eq!(pump.waiting.len(), 3);

        let mut seen = Vec::new();
        loop {
            while let Ok(update) = receiver.try_recv() {
                seen.push(update.payload);
            }
            if pump.offer(&queue) {
                break;
            }
        }
        while let Ok(update) = receiver.try_recv() {
            seen.push(update.payload);
        }
        assert_eq!(seen, ["1", "2", "3", "4", "5"]);
    }

    #[test]
    fn a_closed_queue_loses_nothing_and_does_not_panic() {
        let (queue, receiver) = mpsc::channel(2);
        drop(receiver);
        let mut pump = Pump::new(Echo::default(), true);
        report(&mut pump, 1);

        assert!(!pump.offer(&queue));
        assert_eq!(pump.waiting.len(), 1);
    }

    #[test]
    fn a_rebuild_stays_quiet_until_it_reaches_the_live_stream() {
        let (queue, mut receiver) = mpsc::channel(4);
        let mut pump = Pump::new(Echo::default(), false);
        report(&mut pump, 1);
        pump.on_second(1);
        assert!(pump.offer(&queue));
        assert!(receiver.try_recv().is_err());
        assert_eq!(pump.state.seconds, 1, "state keeps building while quiet");

        pump.publishing = true;
        report(&mut pump, 2);
        assert!(pump.offer(&queue));
        assert_eq!(receiver.try_recv().expect("published").payload, "2");
    }

    #[test]
    fn frames_with_no_updates_are_skipped() {
        let mut pump = Pump::new(Echo::default(), true);
        let mut term_marker = vec![0u8; 16];
        term_marker[2] = 100;
        pump.on_frame(&term_marker);
        let mut cancel_reject = vec![0u8; 8];
        cancel_reject[2] = 4;
        pump.on_frame(&cancel_reject);
        let mut unknown = vec![0u8; 8];
        unknown[2] = 77;
        pump.on_frame(&unknown);
        pump.on_frame(&[1]);

        assert!(pump.waiting.is_empty());
    }
}
