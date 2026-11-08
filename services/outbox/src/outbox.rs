//! Takes frames off the tape and makes them durable, in tape order.

use crate::frame;
use crate::journal::{Journal, Record};
use crate::ledger::Ledger;

use serde::Serialize;

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug)]
pub enum Error {
    /// The disk failed or lost data. Nothing can be trusted, so the process must exit.
    Io(io::Error),
    /// The tape doesn't continue what the journal holds. Delivery has stopped.
    Halted(String),
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "disk failure: {error}"),
            Self::Halted(reason) => write!(f, "halted: {reason}"),
        }
    }
}

/// What the threads share. `durable_length` is the gate: delivery never reads past it.
pub struct Shared {
    durable_length: AtomicU64,
    durable_position: AtomicI64,
    durable_exec_id: AtomicU64,
    delivered_position: AtomicI64,
    verified: AtomicBool,
    halted: Mutex<Option<String>>,
    stopping: AtomicBool,
    consecutive_failures: AtomicU64,
    syncs: AtomicU64,
}

/// The answer to `GET /position`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub durable_position: i64,
    pub durable_exec_id: u64,
    pub delivered_position: i64,
    pub verified: bool,
    pub halted: Option<String>,
    pub consecutive_failures: u64,
}

impl Shared {
    pub fn position(&self) -> Position {
        Position {
            durable_position: self.durable_position.load(Ordering::Acquire),
            durable_exec_id: self.durable_exec_id.load(Ordering::Acquire),
            delivered_position: self.delivered_position.load(Ordering::Acquire),
            verified: self.verified.load(Ordering::Acquire),
            halted: self.halted.lock().expect("no panics while held").clone(),
            consecutive_failures: self.consecutive_failures.load(Ordering::Relaxed),
        }
    }

    pub fn durable_length(&self) -> u64 {
        self.durable_length.load(Ordering::Acquire)
    }

    /// True once this process has seen the tape agree with the journal, and nothing has disagreed
    /// since.
    pub fn may_deliver(&self) -> bool {
        self.verified.load(Ordering::Acquire)
            && self.halted.lock().expect("no panics while held").is_none()
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
    }

    pub fn syncs(&self) -> u64 {
        self.syncs.load(Ordering::Relaxed)
    }

    pub fn set_delivered(&self, position: i64) {
        self.delivered_position.store(position, Ordering::Release);
    }

    pub fn set_consecutive_failures(&self, failures: u64) {
        self.consecutive_failures.store(failures, Ordering::Relaxed);
    }

    fn halt(&self, reason: String) -> Error {
        eprintln!("HALTED: {reason}");
        *self.halted.lock().expect("no panics while held") = Some(reason.clone());
        Error::Halted(reason)
    }
}

#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Flush once this many bytes are pending.
    pub max_pending_bytes: usize,
    /// Flush once the oldest pending record is this old.
    pub max_delay: Duration,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            max_pending_bytes: 64 * 1024,
            max_delay: Duration::from_millis(1),
        }
    }
}

pub struct Outbox {
    journal: Journal,
    ledger: Ledger,
    shared: Arc<Shared>,
    config: SyncConfig,
}

impl Outbox {
    pub fn open(dir: &Path, config: SyncConfig) -> Result<Self, Error> {
        let mut ledger = Ledger::default();
        let journal = Journal::open(dir, |record| {
            ledger.apply(&frame::decode(&record.frame)).map(|_| ())
        })?;

        let shared = Arc::new(Shared {
            durable_length: AtomicU64::new(journal.synced_length()),
            durable_position: AtomicI64::new(journal.position()),
            durable_exec_id: AtomicU64::new(journal.exec_id()),
            delivered_position: AtomicI64::new(0),
            // With nothing journaled there's nothing for the tape to disagree with.
            verified: AtomicBool::new(journal.is_empty()),
            halted: Mutex::new(None),
            stopping: AtomicBool::new(false),
            consecutive_failures: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
        });
        Ok(Self {
            journal,
            ledger,
            shared,
            config,
        })
    }

    pub fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    pub fn open_orders(&self) -> usize {
        self.ledger.open_orders()
    }

    /// Where the journal ends.
    pub fn position(&self) -> i64 {
        self.journal.position()
    }

    /// Where to start reading the tape. It's one frame early, so the first frame read proves the
    /// tape still holds what the journal does.
    pub fn resume_position(&self) -> i64 {
        self.journal.previous_position()
    }

    /// Accepts a frame ending at `position`. It's safe to offer a frame more than once.
    pub fn ingest(&mut self, position: i64, bytes: &[u8]) -> Result<(), Error> {
        if let Some(reason) = self
            .shared
            .halted
            .lock()
            .expect("no panics while held")
            .clone()
        {
            return Err(Error::Halted(reason));
        }

        if position <= self.journal.position() {
            if position == self.journal.position() {
                if self.journal.last_frame() != bytes {
                    return Err(self
                        .shared
                        .halt(format!("the tape at {position} differs from the journal")));
                }
                self.shared.verified.store(true, Ordering::Release);
            }
            return Ok(());
        }

        if !self.shared.verified.load(Ordering::Acquire) {
            let expected = self.journal.position();
            return Err(self.shared.halt(format!(
                "the tape skipped the frame at {expected}, and went on to {position}"
            )));
        }

        let decoded = frame::decode(bytes);
        if let frame::Frame::Unknown(template_id) = decoded {
            eprintln!(
                "journaling a frame with unknown template {template_id} at {position}, which no webhook covers"
            );
        }
        let stamp = self
            .ledger
            .apply(&decoded)
            .map_err(|reason| self.shared.halt(format!("at {position}: {reason}")))?;
        self.journal.append(&Record {
            position,
            exec_id: stamp.exec_id,
            flags: stamp.flags,
            frame: bytes.to_vec(),
        });

        if self.journal.pending_len() >= self.config.max_pending_bytes {
            self.sync()?;
        }
        Ok(())
    }

    /// Flushes pending frames if the oldest has waited long enough. Call this between polls.
    pub fn tick(&mut self) -> Result<(), Error> {
        if self
            .journal
            .pending_since()
            .is_some_and(|since| since.elapsed() >= self.config.max_delay)
        {
            self.sync()?;
        }
        Ok(())
    }

    /// Flushes pending frames, and only then lets delivery see them.
    pub fn sync(&mut self) -> Result<(), Error> {
        if self.journal.pending_len() == 0 {
            return Ok(());
        }
        self.journal.sync()?;
        self.shared.syncs.fetch_add(1, Ordering::Relaxed);
        self.shared
            .durable_position
            .store(self.journal.position(), Ordering::Release);
        self.shared
            .durable_exec_id
            .store(self.journal.exec_id(), Ordering::Release);
        self.shared
            .durable_length
            .store(self.journal.synced_length(), Ordering::Release);
        Ok(())
    }
}
