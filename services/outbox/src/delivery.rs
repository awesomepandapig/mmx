//! Sends durable events to the game.
//!
//! There's one sender and one batch in flight, so the game sees events in tape order: an order's
//! fills always arrive before the event that closes it. A batch is retried until the game accepts
//! it, because skipping one could strand escrow. The cursor only records progress, so losing it
//! causes repeats, which `fill_id` makes harmless, and never a gap.

use crate::journal::{JournalReader, sync_dir};
use crate::outbox::Shared;
use crate::webhook::{self, Delivery as Body, Event};

use std::fs::{self, File};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CURSOR_FILE: &str = "delivered";
const CURSOR_TMP_FILE: &str = "delivered.tmp";
const CURSOR_LENGTH: usize = 20;
const IDLE: Duration = Duration::from_millis(1);
const WAIT_SLICE: Duration = Duration::from_millis(10);
/// How often to record progress over records the game doesn't hear about.
const CURSOR_PERIOD: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    /// Byte offset into the journal.
    pub offset: u64,
    pub position: i64,
}

impl Cursor {
    /// Reads the saved cursor. Without one, delivery repeats from the start.
    pub fn load(dir: &Path) -> io::Result<Self> {
        let bytes = match fs::read(dir.join(CURSOR_FILE)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        if bytes.len() != CURSOR_LENGTH
            || crc32fast::hash(&bytes[..16]) != u32::from_le_bytes(bytes[16..20].try_into().expect("four bytes"))
        {
            eprintln!("the delivery cursor is damaged, so delivery repeats from the start");
            return Ok(Self::default());
        }
        Ok(Self {
            offset: u64::from_le_bytes(bytes[0..8].try_into().expect("eight bytes")),
            position: i64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes")),
        })
    }

    fn save(self, dir: &Path) -> io::Result<()> {
        let mut bytes = [0u8; CURSOR_LENGTH];
        bytes[0..8].copy_from_slice(&self.offset.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.position.to_le_bytes());
        let crc = crc32fast::hash(&bytes[..16]);
        bytes[16..20].copy_from_slice(&crc.to_le_bytes());

        let tmp = dir.join(CURSOR_TMP_FILE);
        let mut file = File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, dir.join(CURSOR_FILE))?;
        sync_dir(dir)
    }
}

#[derive(Debug, Clone)]
pub struct DeliveryConfig {
    pub url: String,
    pub secret: Vec<u8>,
    /// Most events in one POST.
    pub max_events: usize,
    pub first_retry: Duration,
    pub max_retry: Duration,
    pub timeout: Duration,
}

impl DeliveryConfig {
    pub fn new(url: String, secret: Vec<u8>) -> Self {
        Self {
            url,
            secret,
            max_events: 100,
            first_retry: Duration::from_millis(100),
            max_retry: Duration::from_secs(30),
            timeout: Duration::from_secs(10),
        }
    }
}

pub struct Deliverer {
    agent: ureq::Agent,
    config: DeliveryConfig,
    shared: Arc<Shared>,
    dir: PathBuf,
    reader: JournalReader,
    /// Everything before this has been accepted, or never needed sending.
    done: Cursor,
    saved: Cursor,
    saved_at: Instant,
}

impl Deliverer {
    /// Starts from the saved cursor, refusing a journal that has lost what was already delivered.
    pub fn open(dir: &Path, shared: Arc<Shared>, config: DeliveryConfig) -> io::Result<Self> {
        let cursor = Cursor::load(dir)?;
        if cursor.offset > shared.durable_length() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("delivery reached offset {}, but the journal ends at {}", cursor.offset, shared.durable_length()),
            ));
        }
        shared.set_delivered(cursor.position);

        // A redirect is a failure, not a delivery, and following it would turn the POST into a GET.
        let agent = ureq::AgentBuilder::new().timeout(config.timeout).redirects(0).build();
        Ok(Self {
            agent,
            reader: JournalReader::open(dir, cursor.offset)?,
            dir: dir.to_path_buf(),
            config,
            shared,
            done: cursor,
            saved: cursor,
            saved_at: Instant::now(),
        })
    }

    /// Runs until `Shared::stop`. An error means the disk can't be trusted.
    pub fn run(mut self) -> io::Result<()> {
        while !self.shared.is_stopping() {
            if !self.shared.may_deliver() {
                sleep(WAIT_SLICE);
                continue;
            }

            let (events, end) = self.next_batch()?;
            if !events.is_empty() {
                let body = serde_json::to_vec(&Body { events: &events }).expect("events serialize");
                if !self.send_until_accepted(&body) {
                    // Halted or stopping. Neither clears within this process, and the cursor still
                    // points before this batch, so it's sent again after a restart.
                    continue;
                }
                self.done = end;
                self.shared.set_delivered(end.position);
                self.save_cursor()?;
                continue;
            }

            self.done = end;
            self.shared.set_delivered(end.position);
            if self.saved.offset != self.done.offset && self.saved_at.elapsed() >= CURSOR_PERIOD {
                self.save_cursor()?;
            }
            sleep(IDLE);
        }
        Ok(())
    }

    fn save_cursor(&mut self) -> io::Result<()> {
        self.done.save(&self.dir)?;
        self.saved = self.done;
        self.saved_at = Instant::now();
        Ok(())
    }

    /// Reads durable records up to a full batch, and returns where the batch ends.
    fn next_batch(&mut self) -> io::Result<(Vec<Event>, Cursor)> {
        let limit = self.shared.durable_length();
        let mut events = Vec::new();
        let mut end = self.done;
        while events.len() < self.config.max_events {
            let Some(record) = self.reader.next_below(limit)? else { break };
            end = Cursor { offset: self.reader.offset(), position: record.position };
            events.extend(webhook::event(&record));
        }
        Ok((events, end))
    }

    /// Retries with exponential backoff, and returns false if delivery has to stop instead.
    fn send_until_accepted(&mut self, body: &[u8]) -> bool {
        let mut delay = self.config.first_retry;
        let mut failures = 0;
        loop {
            if self.shared.is_stopping() || !self.shared.may_deliver() {
                return false;
            }
            match self.post(body) {
                Ok(()) => {
                    self.shared.set_consecutive_failures(0);
                    return true;
                }
                Err(reason) => {
                    failures += 1;
                    self.shared.set_consecutive_failures(failures);
                    eprintln!("webhook attempt {failures} failed, retrying in {delay:?}: {reason}");
                }
            }
            self.wait(delay);
            delay = (delay * 2).min(self.config.max_retry);
        }
    }

    fn wait(&self, delay: Duration) {
        let started = Instant::now();
        while started.elapsed() < delay && !self.shared.is_stopping() {
            sleep(WAIT_SLICE.min(delay));
        }
    }

    fn post(&self, body: &[u8]) -> Result<(), String> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock is after the epoch").as_secs();
        let response = self
            .agent
            .post(&self.config.url)
            .set("Content-Type", "application/json")
            .set(webhook::TIMESTAMP_HEADER, &timestamp.to_string())
            .set(webhook::SIGNATURE_HEADER, &webhook::signature(&self.config.secret, timestamp, body))
            .send_bytes(body)
            .map_err(|error| match error {
                ureq::Error::Status(status, _) => format!("status {status}"),
                ureq::Error::Transport(transport) => transport.kind().to_string(),
            })?;

        let status = response.status();
        // Reads the body so the connection can be reused.
        let _ = response.into_string();
        if (200..300).contains(&status) { Ok(()) } else { Err(format!("status {status}")) }
    }
}
