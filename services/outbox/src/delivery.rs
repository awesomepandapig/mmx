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
use std::io::{self, ErrorKind, Read, Write};
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
pub const MAX_BATCH_EVENTS: usize = 10_000;
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

#[derive(Debug)]
struct PostError {
    reason: String,
    retry_after: Option<Duration>,
}

impl PostError {
    fn local(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            retry_after: None,
        }
    }
}

fn unix_seconds(now: SystemTime) -> Result<u64, PostError> {
    now.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|_| PostError::local("clock is before the Unix epoch; refusing to sign a webhook"))
}

/// A valid receiver hint is a lower bound, independent of our local exponential-backoff cap.
/// Waiting uses elapsed time rather than adding to Instant, so even large delays are safe and
/// remain interruptible by stop/halt. Invalid hints fall back to local backoff.
fn retry_after(value: Option<&str>, now: SystemTime) -> Option<Duration> {
    let value = value?.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse::<u64>().ok().map(Duration::from_secs);
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or(Duration::ZERO))
}

/// A failed sender must not leave a healthy-looking process without delivery. The binary treats
/// this error as fatal. State is not reused after a caught panic.
pub fn supervise(run: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
        .map_err(|_| io::Error::other("webhook sender panicked; delivery must restart"))?
}

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
            || crc32fast::hash(&bytes[..16])
                != u32::from_le_bytes(bytes[16..20].try_into().expect("four bytes"))
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
        if !(1..=MAX_BATCH_EVENTS).contains(&config.max_events) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("WEBHOOK_MAX_EVENTS must be between 1 and {MAX_BATCH_EVENTS}"),
            ));
        }
        let cursor = Cursor::load(dir)?;
        if cursor.offset > shared.durable_length() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "delivery reached offset {}, but the journal ends at {}",
                    cursor.offset,
                    shared.durable_length()
                ),
            ));
        }
        shared.set_delivered(cursor.position);

        // A redirect is a failure, not a delivery, and following it would turn the POST into a GET.
        let agent = ureq::AgentBuilder::new()
            .timeout(config.timeout)
            .redirects(0)
            .build();
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
                let body =
                    serde_json::to_vec(&Body { events: &events }).map_err(io::Error::other)?;
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
            let Some(record) = self.reader.next_below(limit)? else {
                break;
            };
            end = Cursor {
                offset: self.reader.offset(),
                position: record.position,
            };
            events.extend(webhook::event(&record));
        }
        Ok((events, end))
    }

    /// Retries with exponential backoff, and returns false if delivery has to stop instead.
    fn send_until_accepted(&mut self, body: &[u8]) -> bool {
        let mut delay = self.config.first_retry;
        let mut failures = 0u64;
        loop {
            if self.shared.is_stopping() || !self.shared.may_deliver() {
                return false;
            }
            let wait = match self.post(body) {
                Ok(()) => {
                    self.shared.set_consecutive_failures(0);
                    return true;
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    self.shared.set_consecutive_failures(failures);
                    let wait = error.retry_after.map_or(delay, |hint| hint.max(delay));
                    eprintln!(
                        "webhook attempt {failures} failed, retrying in {wait:?}: {}",
                        error.reason
                    );
                    wait
                }
            };
            self.wait(wait);
            delay = delay.saturating_mul(2).min(self.config.max_retry);
        }
    }

    fn wait(&self, delay: Duration) {
        let started = Instant::now();
        while started.elapsed() < delay && !self.shared.is_stopping() && self.shared.may_deliver() {
            sleep(WAIT_SLICE.min(delay.saturating_sub(started.elapsed())));
        }
    }

    fn post(&self, body: &[u8]) -> Result<(), PostError> {
        let now = SystemTime::now();
        let timestamp = unix_seconds(now)?;
        let response = match self
            .agent
            .post(&self.config.url)
            .set("Content-Type", "application/json")
            .set(webhook::TIMESTAMP_HEADER, &timestamp.to_string())
            .set(
                webhook::SIGNATURE_HEADER,
                &webhook::signature(&self.config.secret, timestamp, body),
            )
            .send_bytes(body)
        {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(error)) => {
                return Err(PostError::local(error.kind().to_string()));
            }
        };

        let status = response.status();
        let hint = retry_after(response.header("Retry-After"), SystemTime::now());
        // The body is not part of the delivery receipt. Bound draining/allocation even when a
        // receiver returns an oversized body; dropping an unread tail closes that connection.
        let _ = io::copy(
            &mut response.into_reader().take(MAX_RESPONSE_BYTES),
            &mut io::sink(),
        );
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(PostError {
                reason: format!("status {status}"),
                retry_after: hint,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, FakeGame, TestDir};
    use crate::outbox::{Outbox, SyncConfig};
    use std::sync::mpsc;

    #[test]
    fn retry_after_accepts_seconds_dates_and_large_interruptible_delays() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(
            retry_after(Some(" 120 "), now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(retry_after(Some("0"), now), Some(Duration::ZERO));
        assert_eq!(
            retry_after(Some(&u64::MAX.to_string()), now),
            Some(Duration::from_secs(u64::MAX))
        );
        let future = httpdate::fmt_http_date(now + Duration::from_secs(90));
        assert_eq!(
            retry_after(Some(&future), now),
            Some(Duration::from_secs(90))
        );
        let past = httpdate::fmt_http_date(now - Duration::from_secs(90));
        assert_eq!(retry_after(Some(&past), now), Some(Duration::ZERO));
        for value in ["", "later", "-1", "+1", "1.5", "184467440737095516160"] {
            assert_eq!(retry_after(Some(value), now), None, "{value}");
        }
    }

    #[test]
    fn clock_errors_and_sender_panics_become_explicit_failures() {
        assert!(unix_seconds(UNIX_EPOCH - Duration::from_secs(1)).is_err());
        assert_eq!(unix_seconds(UNIX_EPOCH).expect("valid clock"), 0);
        assert!(supervise(|| panic!("simulated sender failure")).is_err());
        assert!(supervise(|| Err(io::Error::other("disk failed"))).is_err());
        assert!(supervise(|| Ok(())).is_ok());
    }

    #[test]
    fn invalid_batch_sizes_are_refused_before_delivery_starts() {
        let dir = TestDir::new("batch-validation");
        let outbox = Outbox::open(&dir.0, SyncConfig::default()).expect("journal");
        for size in [0, MAX_BATCH_EVENTS + 1, usize::MAX] {
            let mut config = DeliveryConfig::new("http://127.0.0.1:1/webhook".into(), vec![]);
            config.max_events = size;
            let error = Deliverer::open(&dir.0, outbox.shared(), config)
                .err()
                .expect("invalid size");
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }
    }

    fn queued(dir: &Path) -> Outbox {
        let mut outbox = Outbox::open(
            dir,
            SyncConfig {
                max_pending_bytes: usize::MAX,
                max_delay: Duration::from_secs(3600),
            },
        )
        .expect("journal");
        for (position, frame) in fixture::tape(1) {
            outbox.ingest(position, &frame).expect("frame");
        }
        outbox.sync().expect("sync");
        outbox
    }

    #[test]
    fn receiver_backoff_blocks_cursor_progress_and_stops_promptly() {
        for status in [429, 503] {
            let dir = TestDir::new("receiver-backoff");
            let game = FakeGame::start();
            game.retry_next(1, status, "3600");
            let outbox = queued(&dir.0);
            let shared = outbox.shared();
            let mut config = DeliveryConfig::new(game.url.clone(), b"secret".to_vec());
            config.first_retry = Duration::from_millis(1);
            config.max_retry = Duration::from_millis(5);
            let sender = Deliverer::open(&dir.0, Arc::clone(&shared), config).expect("sender");
            let (finished, result) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                finished.send(sender.run()).expect("result");
            });
            let deadline = Instant::now() + Duration::from_secs(2);
            while game.requests().is_empty() && Instant::now() < deadline {
                sleep(Duration::from_millis(1));
            }
            assert_eq!(game.requests().len(), 1, "initial attempt");
            sleep(Duration::from_millis(40));
            assert_eq!(
                game.requests().len(),
                1,
                "server hint must outlive local backoff cap"
            );
            assert_eq!(
                shared.position().delivered_position,
                0,
                "failed batch cannot advance cursor"
            );
            shared.stop();
            result
                .recv_timeout(Duration::from_secs(1))
                .expect("interrupt backoff")
                .expect("clean stop");
            handle.join().expect("sender");
            assert_eq!(Cursor::load(&dir.0).expect("cursor"), Cursor::default());
        }
    }

    #[test]
    fn retry_uses_identical_events_and_resumes_after_the_server_delay() {
        let dir = TestDir::new("receiver-retry");
        let game = FakeGame::start();
        game.retry_next(1, 429, "1");
        let outbox = queued(&dir.0);
        let shared = outbox.shared();
        let end = outbox.position();
        let mut config = DeliveryConfig::new(game.url.clone(), b"secret".to_vec());
        config.first_retry = Duration::from_millis(1);
        config.max_retry = Duration::from_millis(5);
        let sender = Deliverer::open(&dir.0, Arc::clone(&shared), config).expect("sender");
        let handle = std::thread::spawn(move || sender.run().expect("delivery"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while shared.position().delivered_position < end && Instant::now() < deadline {
            sleep(Duration::from_millis(1));
        }
        shared.stop();
        handle.join().expect("sender");
        assert_eq!(shared.position().delivered_position, end);
        let requests = game.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body, requests[1].body);
        assert!(
            requests[1]
                .received_at
                .duration_since(requests[0].received_at)
                >= Duration::from_secs(1)
        );
        assert!(!requests[0].accepted);
        assert!(requests[1].accepted);
    }
}
