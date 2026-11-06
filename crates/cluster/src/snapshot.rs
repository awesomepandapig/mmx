//! Writes and reads service snapshots through this node's local Archive.

use std::ffi::CString;
use std::fmt;
use std::thread::{sleep, yield_now};
use std::time::{Duration, Instant};

use rusteron_archive as archive;

use crate::{ClusteredService, NodeConfig, TapeMark};

const SNAPSHOT_CHANNEL: &str = "aeron:ipc?alias=snapshot|term-length=64m|mtu=65504";
pub(crate) const SNAPSHOT_STREAM_ID: i32 = 106;
const REPLAY_STREAM_ID: i32 = 103;
const TAPE_REPLAY_STREAM_ID: i32 = 110;
const RECORDING_POSITION_TYPE_ID: i32 = 100;
const TIMEOUT: Duration = Duration::from_secs(30);

fn cstring(value: &str) -> CString {
    CString::new(value).expect("channel has no nul")
}

/// One recording, as the Archive describes it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Recording {
    pub(crate) id: i64,
    pub(crate) start: i64,
    /// Negative while the recording is active.
    pub(crate) stop: i64,
    pub(crate) term_length: i32,
    pub(crate) segment_length: i32,
}

/// The part of the tape this node still holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapeExtent {
    pub start: i64,
    pub stop: i64,
}

/// Where one snapshot's pause went. The Archive setup and the recorder can only be measured on a
/// running cluster, so every snapshot reports them.
pub struct SnapshotTiming {
    setup: Duration,
    write: Duration,
    recorded: Duration,
}

impl fmt::Display for SnapshotTiming {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "took {:?} (setup {:?}, state {:?}, recorder {:?})",
            self.setup + self.write + self.recorded,
            self.setup,
            self.write,
            self.recorded
        )
    }
}

/// A long-lived connection to this node's Archive, so snapshots don't pay to reconnect.
pub struct ArchiveClient {
    aeron: archive::Aeron,
    client: archive::AeronArchive,
}

pub fn connect(config: &NodeConfig) -> ArchiveClient {
    let context = archive::AeronContext::new().expect("failed to create a client context");
    context.set_dir(&cstring(&config.aeron_dir)).expect("invalid Aeron dir");
    context
        .set_error_handler(Some(|code: i32, message: &str| eprintln!("archive client error {code}: {message}")))
        .expect("failed to set the error handler");
    let aeron = archive::Aeron::new(&context).expect("failed to create a client");
    aeron.start().expect("failed to start the client");

    let archive_context = archive::AeronArchiveContext::new().expect("failed to create an archive context");
    archive_context.set_aeron(&aeron).expect("failed to set the client");
    archive_context
        .set_control_request_channel(&cstring(&config.archive_control_channel))
        .expect("invalid control channel");
    archive_context
        .set_control_response_channel(&cstring("aeron:udp?endpoint=localhost:0"))
        .expect("invalid response channel");

    let client = archive::AeronArchiveAsyncConnect::new_with_aeron(&archive_context, &aeron)
        .expect("failed to connect to the archive")
        .poll_blocking(TIMEOUT)
        .expect("failed to connect to the archive");
    ArchiveClient { aeron, client }
}

impl ArchiveClient {
    /// Reads whatever the Archive has sent. It pings every client each second over a 64 KB term, so
    /// a client that never reads fills that term in about 17 minutes, and the Archive then closes
    /// the session. Holding one connection open means this has to run for as long as it's held.
    pub(crate) fn keep_alive(&self) {
        // An error means the session is gone, which `is_connected` reports where it matters.
        let _ = self.client.poll_for_recording_signals();
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.client.get_control_response_subscription().is_connected()
    }

    /// Records a deterministic output stream from `position`, truncating anything newer that a
    /// previous run wrote, and returns the channel to publish on. The replayed log regenerates
    /// identical bytes from that point.
    pub fn prepare_output(&self, base_channel: &str, stream_id: i32, session_id: i32, position: i64) -> String {
        let channel = format!("{base_channel}|session-id={session_id}");
        let recording_id = self
            .client
            .find_last_matching_recording(0, &cstring(""), stream_id, session_id)
            .expect("failed to search recordings");

        // A replacement node has no tape of its own, so its tape starts wherever it joined.
        if recording_id < 0 {
            self.client
                .start_recording(&cstring(&channel), stream_id, archive::SOURCE_LOCATION_LOCAL, false)
                .expect("failed to record the output stream");
        } else {
            let stop = self.client.get_stop_position(recording_id).expect("failed to read the recording");
            assert!(stop >= position, "output recording stops at {stop}, before {position}");
            if stop > position {
                self.client.truncate_recording(recording_id, position).expect("failed to truncate the output");
            }
            self.client
                .extend_recording(recording_id, &cstring(&channel), stream_id, archive::SOURCE_LOCATION_LOCAL, false)
                .expect("failed to extend the output recording");
        }

        let term_length = 64_i64 * 1024 * 1024;
        format!(
            "{channel}|init-term-id=0|term-id={}|term-offset={}",
            position / term_length,
            position % term_length
        )
    }

    /// Returns what's left of the tape, or none if this node never recorded one. A restarted
    /// engine can get here before the Archive has noticed its old publication is gone, and the
    /// tape has no stop position until then.
    pub fn await_tape_extent(&self, stream_id: i32, session_id: i32) -> Option<TapeExtent> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let recording = self.find_recording(stream_id, session_id)?;
            if recording.stop >= 0 {
                return Some(TapeExtent { start: recording.start, stop: recording.stop });
            }
            assert!(Instant::now() < deadline, "the tape is still being recorded");
            sleep(Duration::from_millis(10));
        }
    }

    /// Reads the tape from `from` up to `to`, one message at a time.
    pub fn read_tape(
        &self,
        stream_id: i32,
        session_id: i32,
        from: i64,
        to: i64,
        on_message: &mut dyn FnMut(&[u8]),
    ) -> Result<(), String> {
        let recording = self.find_recording(stream_id, session_id).ok_or("this node has no tape")?;
        replay(self, recording.id, from, to, TAPE_REPLAY_STREAM_ID, on_message)
    }

    pub(crate) fn find_recording(&self, stream_id: i32, session_id: i32) -> Option<Recording> {
        let id = self.client.find_last_matching_recording(0, &cstring(""), stream_id, session_id).ok()?;
        if id < 0 {
            return None;
        }
        let mut found = None;
        let mut count = 0;
        self.client
            .list_recording_fn(&mut count, id, |descriptor| found = Some(Recording::from(&descriptor)))
            .ok()?;
        found
    }

    /// Lists every recording of a stream, oldest first.
    pub(crate) fn recordings(&self, stream_id: i32) -> Result<Vec<Recording>, archive::AeronArchiveError> {
        const PAGE: i32 = 256;
        let mut recordings = Vec::new();
        let mut from = 0;
        loop {
            let (mut count, mut page) = (0, Vec::new());
            self.client.list_recordings_for_uri_fn(&mut count, from, PAGE, &cstring(""), stream_id, |descriptor| {
                page.push(Recording::from(&descriptor));
            })?;
            let Some(last) = page.last() else { return Ok(recordings) };
            from = last.id + 1;
            let finished = page.len() < PAGE as usize;
            recordings.append(&mut page);
            if finished {
                return Ok(recordings);
            }
        }
    }

    pub(crate) fn purge_recording(&self, recording_id: i64) -> Result<i64, archive::AeronArchiveError> {
        self.client.purge_recording(recording_id)
    }

    pub(crate) fn purge_segments(&self, recording_id: i64, start: i64) -> Result<i64, archive::AeronArchiveError> {
        self.client.purge_segments(recording_id, start)
    }

    /// Waits until the output stream is recorded up to `position`.
    pub fn await_recorded(&self, session_id: i32, position: i64) {
        let (_, counter_id) = await_recording(&self.aeron, session_id);
        let reader = self.aeron.counters_reader();
        let recorded: *const i64 = reader.addr(counter_id);
        // SAFETY: the output recording stays active for the life of the publication.
        while unsafe { std::ptr::read_volatile(recorded) } < position {
            std::hint::spin_loop();
        }
    }
}

impl From<&archive::AeronArchiveRecordingDescriptor> for Recording {
    fn from(descriptor: &archive::AeronArchiveRecordingDescriptor) -> Self {
        Self {
            id: descriptor.recording_id(),
            start: descriptor.start_position(),
            stop: descriptor.stop_position(),
            term_length: descriptor.term_buffer_length(),
            segment_length: descriptor.segment_file_length(),
        }
    }
}

/// Finds the live recording of `session_id`, returning its recording ID and counter ID.
fn await_recording(aeron: &archive::Aeron, session_id: i32) -> (i64, i32) {
    loop {
        let mut found = None;
        aeron.counters_reader().foreach_counter_fn(|_, id, type_id, key, _| {
            if type_id == RECORDING_POSITION_TYPE_ID
                && i32::from_le_bytes(key[8..12].try_into().expect("four bytes")) == session_id
            {
                found = Some((i64::from_le_bytes(key[0..8].try_into().expect("eight bytes")), id));
            }
        });
        if let Some(found) = found {
            return found;
        }
        sleep(Duration::from_micros(100));
    }
}

/// Returns the recording that holds the snapshot, for the consensus module's recording log.
pub fn take<S: ClusteredService>(archive_client: &ArchiveClient, service: &mut S) -> (i64, TapeMark, SnapshotTiming) {
    let (aeron, client) = (&archive_client.aeron, &archive_client.client);
    let started = Instant::now();
    let publication = aeron
        .async_add_exclusive_publication(&cstring(SNAPSHOT_CHANNEL), SNAPSHOT_STREAM_ID)
        .expect("failed to add the snapshot publication")
        .poll_blocking(TIMEOUT)
        .expect("failed to add the snapshot publication");

    let session_id = publication.get_constants().expect("failed to read the publication").session_id();
    let channel = format!("{SNAPSHOT_CHANNEL}|session-id={session_id}");
    client
        .start_recording(&cstring(&channel), SNAPSHOT_STREAM_ID, archive::SOURCE_LOCATION_LOCAL, true)
        .expect("failed to start the snapshot recording");
    let (recording_id, counter_id) = await_recording(aeron, session_id);

    let setup = started.elapsed();
    let started = Instant::now();
    let tape = service.on_take_snapshot(archive_client, &mut |chunk| {
        while let Err(error) = publication.offer(chunk) {
            assert!(error.is_retryable(), "failed to write the snapshot: {error}");
            std::hint::spin_loop();
        }
    });

    let write = started.elapsed();
    let started = Instant::now();
    let reader = aeron.counters_reader();
    let recorded: *const i64 = reader.addr(counter_id);
    let target = publication.position();
    // SAFETY: the counter exists until the recording stops, which needs this publication closed.
    while unsafe { std::ptr::read_volatile(recorded) } < target {
        yield_now();
    }
    (recording_id, tape, SnapshotTiming { setup, write, recorded: started.elapsed() })
}

pub fn load<S: ClusteredService>(archive_client: &ArchiveClient, recording_id: i64, service: &mut S) {
    let stop_position =
        archive_client.client.get_stop_position(recording_id).expect("failed to read the snapshot length");
    replay(archive_client, recording_id, 0, stop_position, REPLAY_STREAM_ID, &mut |chunk| {
        service.on_load_snapshot(chunk);
    })
    .expect("failed to replay the snapshot");
}

/// Passes each message of a recording between two positions to `on_message`.
fn replay(
    archive_client: &ArchiveClient,
    recording_id: i64,
    from: i64,
    to: i64,
    stream_id: i32,
    on_message: &mut dyn FnMut(&[u8]),
) -> Result<(), String> {
    let (aeron, client) = (&archive_client.aeron, &archive_client.client);
    let params = archive::AeronArchiveReplayParams::new(-1, 0, from, to - from, -1, -1).map_err(|e| e.to_string())?;
    let session =
        client.start_replay(recording_id, &cstring("aeron:ipc"), stream_id, &params).map_err(|e| e.to_string())? as i32;

    let channel = format!("aeron:ipc?session-id={session}");
    let subscription = aeron
        .async_add_subscription(&cstring(&channel), stream_id, archive::Handlers::NONE, archive::Handlers::NONE)
        .map_err(|e| e.to_string())?
        .poll_blocking(TIMEOUT)
        .map_err(|e| e.to_string())?;

    let image = loop {
        if let Some(image) = subscription.image_by_session_id(session) {
            break image;
        }
        sleep(Duration::from_micros(100));
    };

    while image.position() < to {
        let polled = image.poll_fn(|message, _header| on_message(message), 64).map_err(|e| e.to_string())?;
        if polled == 0 {
            std::hint::spin_loop();
        }
    }
    Ok(())
}
