//! Writes and reads service snapshots through this node's local Archive.

use std::ffi::CString;
use std::thread::sleep;
use std::time::Duration;

use rusteron_archive as archive;

use crate::{ClusteredService, NodeConfig};

const SNAPSHOT_CHANNEL: &str = "aeron:ipc?alias=snapshot|term-length=64m|mtu=65504";
const SNAPSHOT_STREAM_ID: i32 = 106;
const REPLAY_STREAM_ID: i32 = 103;
const RECORDING_POSITION_TYPE_ID: i32 = 100;
const TIMEOUT: Duration = Duration::from_secs(30);

fn cstring(value: &str) -> CString {
    CString::new(value).expect("channel has no nul")
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
    /// Records a deterministic output stream from `position`, truncating anything newer that a
    /// previous run wrote, and returns the channel to publish on. The replayed log regenerates
    /// identical bytes from that point.
    pub fn prepare_output(&self, base_channel: &str, stream_id: i32, session_id: i32, position: i64) -> String {
        let channel = format!("{base_channel}|session-id={session_id}");
        let recording_id = self
            .client
            .find_last_matching_recording(0, &cstring(""), stream_id, session_id)
            .expect("failed to search recordings");

        if recording_id < 0 {
            assert_eq!(position, 0, "output recording is missing");
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
pub fn take<S: ClusteredService>(archive_client: &ArchiveClient, service: &mut S) -> i64 {
    let (aeron, client) = (&archive_client.aeron, &archive_client.client);
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

    service.on_take_snapshot(archive_client, &mut |chunk| {
        while let Err(error) = publication.offer(chunk) {
            assert!(error.is_retryable(), "failed to write the snapshot: {error}");
            std::hint::spin_loop();
        }
    });

    let reader = aeron.counters_reader();
    let recorded: *const i64 = reader.addr(counter_id);
    let target = publication.position();
    // SAFETY: the counter exists until the recording stops, which needs this publication closed.
    while unsafe { std::ptr::read_volatile(recorded) } < target {
        sleep(Duration::from_micros(50));
    }
    recording_id
}

pub fn load<S: ClusteredService>(archive_client: &ArchiveClient, recording_id: i64, service: &mut S) {
    let (aeron, client) = (&archive_client.aeron, &archive_client.client);
    let stop_position = client.get_stop_position(recording_id).expect("failed to read the snapshot length");

    let params = archive::AeronArchiveReplayParams::new(-1, 0, 0, stop_position, -1, -1)
        .expect("failed to build the replay parameters");
    let session = client
        .start_replay(recording_id, &cstring("aeron:ipc"), REPLAY_STREAM_ID, &params)
        .expect("failed to replay the snapshot") as i32;

    let channel = format!("aeron:ipc?session-id={session}");
    let subscription = aeron
        .async_add_subscription(&cstring(&channel), REPLAY_STREAM_ID, archive::Handlers::NONE, archive::Handlers::NONE)
        .expect("failed to subscribe to the snapshot")
        .poll_blocking(TIMEOUT)
        .expect("failed to subscribe to the snapshot");

    let image = loop {
        if let Some(image) = subscription.image_by_session_id(session) {
            break image;
        }
        sleep(Duration::from_micros(100));
    };

    while image.position() < stop_position {
        let polled = image
            .poll_fn(|chunk, _header| service.on_load_snapshot(chunk), 64)
            .expect("failed to read the snapshot");
        if polled == 0 {
            std::hint::spin_loop();
        }
    }
}
