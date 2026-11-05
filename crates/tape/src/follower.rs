//! Follows the recorded report stream with a persistent subscription, which replays from a stream
//! position and then joins the live stream, falling back to replay if it drops off.
//!
//! Every node records the same bytes, so a position means the same thing on all of them. When a
//! node stops answering, the follower moves to the next one at the same position.

use std::ffi::CString;
use std::time::{Duration, Instant};

use rusteron_archive as archive;

use tracing::{error, info, warn};

/// The session every node publishes its reports on.
const REPORT_SESSION: i32 = 1002;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
const RETRY_AFTER: Duration = Duration::from_secs(1);
/// How long a source may stay off the live stream without delivering anything. A quiet tape is
/// normal once live, but not before.
const STALL_AFTER: Duration = Duration::from_secs(10);
/// A gap between polls that means the caller paused, which isn't the source's fault.
const PAUSE_AFTER: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeConfig {
    /// Each node's archive control endpoint.
    pub archives: Vec<String>,
    /// Each node's report stream control endpoint, in the same order.
    pub reports: Vec<String>,
    pub client_host: String,
    /// The first of a range of stream IDs, one per node. Consumers need their own ranges.
    pub replay_stream: i32,
}

impl TapeConfig {
    /// Reads `TAPE_ARCHIVES`, `TAPE_REPORTS` and `CLIENT_HOST`. With no `TAPE_ARCHIVES` there is
    /// no tape to follow, as with a standalone engine, and this returns `None`.
    pub fn from_env(replay_stream: i32) -> Result<Option<Self>, String> {
        let Ok(archives) = std::env::var("TAPE_ARCHIVES") else {
            return Ok(None);
        };
        let reports = std::env::var("TAPE_REPORTS")
            .map_err(|_| "TAPE_REPORTS is required with TAPE_ARCHIVES".to_string())?;
        let client_host = transport::env_or("CLIENT_HOST", "localhost");
        Self::new(&archives, &reports, client_host, replay_stream).map(Some)
    }

    pub fn new(archives: &str, reports: &str, client_host: String, replay_stream: i32) -> Result<Self, String> {
        let archives = endpoints(archives);
        let reports = endpoints(reports);
        if archives.is_empty() || archives.len() != reports.len() {
            return Err(format!(
                "TAPE_ARCHIVES and TAPE_REPORTS need one endpoint per node, got {} and {}",
                archives.len(),
                reports.len()
            ));
        }
        Ok(Self { archives, reports, client_host, replay_stream })
    }

    fn control_request_channel(&self, node: usize) -> String {
        format!("aeron:udp?endpoint={}", self.archives[node])
    }

    fn client_channel(&self) -> String {
        format!("aeron:udp?endpoint={}:0", self.client_host)
    }

    fn live_channel(&self, node: usize) -> String {
        format!(
            "aeron:udp?endpoint={}:0|control={}|session-id={REPORT_SESSION}",
            self.client_host, self.reports[node]
        )
    }

    fn replay_stream(&self, node: usize) -> i32 {
        self.replay_stream + node as i32
    }
}

fn endpoints(list: &str) -> Vec<String> {
    list.split(',')
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .map(str::to_string)
        .collect()
}

/// Position zero means the start of the recording, which is later than zero once old tape is
/// purged.
fn start_position(position: i64) -> i64 {
    if position == 0 {
        archive::PERSISTENT_SUBSCRIPTION_FROM_START
    } else {
        position
    }
}

fn cstring(value: &str) -> Result<CString, String> {
    CString::new(value).map_err(|e| e.to_string())
}

fn text(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

fn archive_client() -> Result<archive::Aeron, String> {
    let context = archive::AeronContext::new().map_err(text)?;
    context.set_dir(&cstring(&transport::aeron_dir())?).map_err(text)?;
    context
        .set_error_handler(Some(|code: i32, message: &str| error!("Aeron: Error {}: {}", code, message)))
        .map_err(text)?;
    let aeron = archive::Aeron::new(&context).map_err(text)?;
    aeron.start().map_err(text)?;
    Ok(aeron)
}

pub struct Follower {
    config: TapeConfig,
    aeron: archive::Aeron,
    subscription: Option<archive::AeronArchivePersistentSubscription>,
    node: usize,
    position: i64,
    retry_at: Instant,
    last_poll: Instant,
    last_progress: Instant,
}

impl Follower {
    /// Connects to the media driver. The first node is opened by the first poll.
    pub fn new(config: TapeConfig, position: i64) -> Result<Self, String> {
        let now = Instant::now();
        Ok(Self {
            config,
            aeron: archive_client()?,
            subscription: None,
            node: 0,
            position,
            retry_at: now,
            last_poll: now,
            last_progress: now,
        })
    }

    fn open(&mut self) -> Result<archive::AeronArchivePersistentSubscription, String> {
        if self.aeron.is_closed() {
            self.aeron = archive_client()?;
        }
        let node = self.node;
        let context = archive::AeronArchiveContext::new().map_err(text)?;
        context.set_aeron(&self.aeron).map_err(text)?;
        context
            .set_control_request_channel(&cstring(&self.config.control_request_channel(node))?)
            .map_err(text)?;
        context
            .set_control_response_channel(&cstring(&self.config.client_channel())?)
            .map_err(text)?;

        let client = archive::AeronArchiveAsyncConnect::new_with_aeron(&context, &self.aeron)
            .map_err(text)?
            .poll_blocking(CONNECT_TIMEOUT)
            .map_err(text)?;
        let recording_id = client
            .find_last_matching_recording(0, &cstring("")?, transport::EXECUTIONS_STREAM, REPORT_SESSION)
            .map_err(text)?;
        if recording_id < 0 {
            return Err("the report stream has no recording yet".to_string());
        }

        archive::PersistentSubscriptionBuilder::new_with_aeron(&context, &self.aeron)
            .map_err(text)?
            .live_channel(&self.config.live_channel(node))
            .map_err(text)?
            .live_stream_id(transport::EXECUTIONS_STREAM)
            .map_err(text)?
            .replay_channel(&self.config.client_channel())
            .map_err(text)?
            .replay_stream_id(self.config.replay_stream(node))
            .map_err(text)?
            .start_position(start_position(self.position))
            .map_err(text)?
            .recording_id(recording_id)
            .map_err(text)?
            .build()
            .map_err(text)
    }

    fn move_to_next_node(&mut self, reason: &str) {
        warn!("Tape: leaving node {} at position {}: {}", self.node, self.position, reason);
        self.subscription = None;
        self.node = (self.node + 1) % self.config.archives.len();
        self.retry_at = Instant::now() + RETRY_AFTER;
    }

    /// Delivers up to `limit` messages, each with the stream position that follows it.
    pub fn poll(&mut self, limit: usize, handler: &mut dyn FnMut(&[u8], i64)) -> i32 {
        let now = Instant::now();
        if now.duration_since(self.last_poll) > PAUSE_AFTER {
            self.last_progress = now;
        }
        self.last_poll = now;

        let Some(subscription) = &self.subscription else {
            if now >= self.retry_at {
                match self.open() {
                    Ok(subscription) => {
                        info!("Tape: following node {} from position {}", self.node, self.position);
                        self.subscription = Some(subscription);
                        self.last_progress = Instant::now();
                    }
                    Err(reason) => self.move_to_next_node(&reason),
                }
            }
            return 0;
        };

        let position = &mut self.position;
        let result = subscription.poll_fn(
            |buffer, header| {
                *position = header.position();
                handler(buffer, *position);
            },
            limit,
        );

        let (read, failure) = match result {
            Ok(read) => (read, None),
            Err(e) => (0, Some(text(e))),
        };
        if read > 0 || subscription.is_live() {
            self.last_progress = now;
        }
        let failure = failure
            .or_else(|| subscription.has_failed().then(|| text(subscription.get_failure_reason())))
            .or_else(|| (now.duration_since(self.last_progress) > STALL_AFTER).then(|| "stalled".to_string()));
        if let Some(reason) = failure {
            self.move_to_next_node(&reason);
        }
        read
    }

    pub fn position(&self) -> i64 {
        self.position
    }

    pub fn is_live(&self) -> bool {
        self.subscription.as_ref().is_some_and(|subscription| subscription.is_live())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> TapeConfig {
        TapeConfig::new("a:8010, b:8011,", "a:40450,b:40451", "me".to_string(), 3000).expect("valid")
    }

    #[test]
    fn endpoints_are_paired_by_node() {
        let config = config();
        assert_eq!(config.archives, ["a:8010", "b:8011"]);
        assert_eq!(config.control_request_channel(1), "aeron:udp?endpoint=b:8011");
        assert_eq!(config.live_channel(1), "aeron:udp?endpoint=me:0|control=b:40451|session-id=1002");
        assert_eq!(config.client_channel(), "aeron:udp?endpoint=me:0");
        assert_eq!(config.replay_stream(1), 3001);
    }

    #[test]
    fn mismatched_endpoint_lists_are_refused() {
        assert!(TapeConfig::new("a:1,b:2", "a:3", "me".to_string(), 3000).is_err());
        assert!(TapeConfig::new("", "", "me".to_string(), 3000).is_err());
    }

    #[test]
    fn position_zero_means_the_start_of_the_recording() {
        assert_eq!(start_position(0), archive::PERSISTENT_SUBSCRIPTION_FROM_START);
        assert_eq!(start_position(4096), 4096);
    }
}
