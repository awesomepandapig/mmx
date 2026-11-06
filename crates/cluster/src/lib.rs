//! A native Rust clustered service and client for Aeron Cluster.
//!
//! The consensus module, archive and media driver are stock Java processes. This crate speaks the
//! same SBE protocol over Aeron streams that Java's `ClusteredServiceAgent` and `AeronCluster`
//! use, so the engine stays pure Rust. The protocol is internal to Aeron, so it's pinned to the
//! schema below and must be checked on every Aeron upgrade.

use std::hint::spin_loop;
use std::ptr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use rusteron_client::*;

mod retention;
mod schedule;
mod snapshot;

use retention::Housekeeper;
use schedule::SnapshotSchedule;

pub use retention::{Retention, TapeMark, TapeRetention};
pub use schedule::SnapshotPolicy;
pub use snapshot::{ArchiveClient, TapeExtent};

const SCHEMA_ID: u16 = 111;
const SCHEMA_VERSION: u16 = 17;
const PROTOCOL_VERSION: i32 = 3 << 8; // 0.3.0

const NULL_VALUE: i64 = -1;
const NULL_POSITION: i64 = -1;

const CONTROL_TOGGLE_TYPE_ID: i32 = 202;
const COMMIT_POSITION_TYPE_ID: i32 = 203;
const RECOVERY_STATE_TYPE_ID: i32 = 204;

const CONTROL_CHANNEL: &str = "aeron:ipc?term-length=128k";
const SERVICE_STREAM_ID: i32 = 104;
const CONSENSUS_MODULE_STREAM_ID: i32 = 105;
const SERVICE_ID: i32 = 0;

const FRAGMENT_LIMIT: usize = 256;
/// How often a held Archive connection reads what it's been sent.
const ARCHIVE_KEEP_ALIVE: Duration = Duration::from_millis(100);
const KEEP_ALIVE_CHECK_LOOPS: u64 = 1024;
const HEADER_LENGTH: usize = 8;
pub const SESSION_HEADER_LENGTH: usize = 32;

const TEMPLATE_SESSION_MESSAGE: u16 = 1;
const TEMPLATE_SESSION_EVENT: u16 = 2;
const TEMPLATE_CONNECT_REQUEST: u16 = 3;
const TEMPLATE_KEEP_ALIVE: u16 = 5;
const TEMPLATE_NEW_LEADER_EVENT: u16 = 6;
const TEMPLATE_TIMER_EVENT: u16 = 20;
const TEMPLATE_SESSION_OPEN: u16 = 21;
const TEMPLATE_SESSION_CLOSE: u16 = 22;
const TEMPLATE_CLUSTER_ACTION: u16 = 23;
const TEMPLATE_NEW_LEADERSHIP_TERM: u16 = 24;
const TEMPLATE_SERVICE_ACK: u16 = 33;
const TEMPLATE_JOIN_LOG: u16 = 40;
const TEMPLATE_TERMINATION_POSITION: u16 = 42;
const TEMPLATE_REQUEST_SERVICE_ACK: u16 = 108;

const ACTION_SNAPSHOT: i32 = 2;

const TOGGLE_NEUTRAL: i64 = 1;
const TOGGLE_SNAPSHOT: i64 = 4;

const EVENT_OK: i32 = 0;
const EVENT_ERROR: i32 = 1;
const EVENT_REDIRECT: i32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

impl Role {
    fn from_code(code: i32) -> Self {
        match code {
            1 => Self::Candidate,
            2 => Self::Leader,
            _ => Self::Follower,
        }
    }
}

/// What the service wants from the snapshot schedule right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotRequest {
    /// Follow the schedule.
    Scheduled,
    /// Take one as soon as this node leads, whatever the schedule says.
    Now,
    /// Skip the schedule, because the state isn't worth keeping yet.
    Hold,
}

/// The application side of a cluster node. Callbacks run on the service thread, in log order, so
/// every node sees exactly the same sequence.
pub trait ClusteredService {
    fn on_session_message(&mut self, archive: &ArchiveClient, timestamp: i64, log_position: i64, payload: &[u8]);
    fn on_role_change(&mut self, role: Role);
    /// Runs in log order when a leadership term starts. Nothing from an older term can follow,
    /// because the consensus module drops ingress stamped with any other term.
    fn on_new_leadership_term(&mut self, leadership_term_id: i64);
    /// Runs once replay has finished, before this node acknowledges the live log. A leader can't
    /// open the log for new messages until that acknowledgement arrives, so a service that exits
    /// here keeps the cluster from moving past this position.
    fn on_join_live_log(&mut self, archive: &ArchiveClient, role: Role, log_position: i64);
    fn do_background_work(&mut self, _archive: &ArchiveClient) {}
    fn snapshot_request(&self) -> SnapshotRequest {
        SnapshotRequest::Scheduled
    }
    /// Writes the full state in chunks small enough for one Aeron message, and returns where the
    /// snapshot sits on the tape.
    fn on_take_snapshot(&mut self, archive: &ArchiveClient, write: &mut dyn FnMut(&[u8])) -> TapeMark;
    /// Receives the chunks written by `on_take_snapshot`, in order.
    fn on_load_snapshot(&mut self, chunk: &[u8]);
    /// Runs once any snapshot is loaded, before the service joins the log.
    fn on_recovered(&mut self, archive: &ArchiveClient);
}

/// Where the service finds this node's Aeron driver and Archive, and how it looks after them.
#[derive(Clone)]
pub struct NodeConfig {
    pub cluster_id: i32,
    pub aeron_dir: String,
    pub archive_control_channel: String,
    pub snapshots: SnapshotPolicy,
    pub retention: Retention,
}

fn read_i32(buffer: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(buffer[offset..offset + 4].try_into().expect("four bytes"))
}

fn read_i64(buffer: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(buffer[offset..offset + 8].try_into().expect("eight bytes"))
}

fn read_u16(buffer: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(buffer[offset..offset + 2].try_into().expect("two bytes"))
}

fn write_header(buffer: &mut [u8], block_length: u16, template_id: u16) {
    buffer[0..2].copy_from_slice(&block_length.to_le_bytes());
    buffer[2..4].copy_from_slice(&template_id.to_le_bytes());
    buffer[4..6].copy_from_slice(&SCHEMA_ID.to_le_bytes());
    buffer[6..8].copy_from_slice(&SCHEMA_VERSION.to_le_bytes());
}

fn template_of(buffer: &[u8]) -> Option<u16> {
    (buffer.len() >= HEADER_LENGTH && read_u16(buffer, 4) == SCHEMA_ID).then(|| read_u16(buffer, 2))
}

fn find_counter(aeron: &Aeron, wanted: i32, cluster_id: i32, key_offset: usize) -> Option<(i32, Vec<u8>)> {
    let mut found = None;
    aeron.counters_reader().foreach_counter_fn(|_, id, type_id, key, _| {
        if type_id == wanted && key.len() >= key_offset + 4 && read_i32(key, key_offset) == cluster_id {
            found = Some((id, key.to_vec()));
        }
    });
    found
}

fn await_counter(aeron: &Aeron, wanted: i32, cluster_id: i32, key_offset: usize) -> (i32, Vec<u8>) {
    loop {
        if let Some(found) = find_counter(aeron, wanted, cluster_id, key_offset) {
            return found;
        }
        sleep(Duration::from_millis(1));
    }
}

struct ActiveLog {
    log_position: i64,
    max_log_position: i64,
    session_id: i32,
    stream_id: i32,
    role: Role,
    channel: String,
}

impl ActiveLog {
    /// A replay ends at a known position, and only the live log has no end.
    const fn is_live(&self) -> bool {
        self.max_log_position == i64::MAX
    }
}

struct Agent<'a, S: ClusteredService> {
    aeron: &'a Aeron,
    archive: snapshot::ArchiveClient,
    config: &'a NodeConfig,
    service: &'a mut S,
    consensus: AeronPublication,
    control_toggle: &'a AtomicI64,
    schedule: SnapshotSchedule,
    housekeeper: Housekeeper,
    ack_id: i64,
    log_position: i64,
    cluster_time: i64,
    termination_position: i64,
    requested_ack_position: i64,
    active_log: Option<ActiveLog>,
    max_log_position: i64,
    role: Role,
}

impl<S: ClusteredService> Agent<'_, S> {
    fn ack(&mut self, log_position: i64, relevant_id: i64) {
        let mut message = [0u8; HEADER_LENGTH + 36];
        write_header(&mut message, 36, TEMPLATE_SERVICE_ACK);
        message[8..16].copy_from_slice(&log_position.to_le_bytes());
        message[16..24].copy_from_slice(&self.cluster_time.to_le_bytes());
        message[24..32].copy_from_slice(&self.ack_id.to_le_bytes());
        message[32..40].copy_from_slice(&relevant_id.to_le_bytes());
        message[40..44].copy_from_slice(&SERVICE_ID.to_le_bytes());
        self.ack_id += 1;

        while let Err(error) = self.consensus.offer(&message) {
            assert!(error.is_retryable(), "failed to ack the consensus module: {error}");
            spin_loop();
        }
    }

    /// Asks the consensus module for a snapshot, the way `ClusterTool snapshot` does. It only
    /// takes effect once the toggle is neutral, so a request already in progress can't double up.
    /// The consensus module only reads this toggle on the leader.
    fn request_snapshot_if_due(&self) {
        if self.role != Role::Leader {
            return;
        }
        let wanted = match self.service.snapshot_request() {
            SnapshotRequest::Now => true,
            SnapshotRequest::Scheduled => self.schedule.is_due(self.log_position, self.cluster_time),
            SnapshotRequest::Hold => false,
        };
        if wanted {
            let _ = self.control_toggle.compare_exchange(
                TOGGLE_NEUTRAL,
                TOGGLE_SNAPSHOT,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }

    fn on_control_message(&mut self, buffer: &[u8]) {
        let Some(template_id) = template_of(buffer) else { return };
        let body = &buffer[HEADER_LENGTH..];

        match template_id {
            TEMPLATE_JOIN_LOG => {
                let block_length = usize::from(read_u16(buffer, 0));
                let length = read_i32(body, block_length) as usize;
                let start = block_length + 4;
                self.max_log_position = read_i64(body, 0);
                self.active_log = Some(ActiveLog {
                    log_position: read_i64(body, 0),
                    max_log_position: read_i64(body, 8),
                    session_id: read_i32(body, 20),
                    stream_id: read_i32(body, 24),
                    role: Role::from_code(read_i32(body, 32)),
                    channel: String::from_utf8_lossy(&body[start..start + length]).into_owned(),
                });
            }
            TEMPLATE_TERMINATION_POSITION => self.termination_position = read_i64(body, 0),
            TEMPLATE_REQUEST_SERVICE_ACK => self.requested_ack_position = read_i64(body, 0),
            _ => {}
        }
    }

    fn on_log_message(&mut self, buffer: &[u8], position: i64) {
        let Some(template_id) = template_of(buffer) else { return };
        let body = &buffer[HEADER_LENGTH..];
        self.log_position = position;

        match template_id {
            TEMPLATE_SESSION_MESSAGE => {
                self.cluster_time = read_i64(body, 16);
                self.service.on_session_message(
                    &self.archive,
                    self.cluster_time,
                    position,
                    &buffer[SESSION_HEADER_LENGTH..],
                );
            }
            TEMPLATE_TIMER_EVENT | TEMPLATE_SESSION_CLOSE => self.cluster_time = read_i64(body, 16),
            TEMPLATE_SESSION_OPEN => self.cluster_time = read_i64(body, 24),
            TEMPLATE_NEW_LEADERSHIP_TERM => {
                self.cluster_time = read_i64(body, 16);
                self.service.on_new_leadership_term(read_i64(body, 0));
            }
            TEMPLATE_CLUSTER_ACTION => {
                self.cluster_time = read_i64(body, 16);
                if read_i32(body, 24) == ACTION_SNAPSHOT {
                    let log_position = read_i64(body, 8);
                    if !self.archive.is_connected() {
                        eprintln!("the archive connection dropped, reconnecting for this snapshot");
                        self.archive = snapshot::connect(self.config);
                    }
                    let (recording_id, tape, timing) = snapshot::take(&self.archive, self.service);
                    self.ack(log_position, recording_id);
                    self.schedule.on_snapshot(log_position, self.cluster_time);
                    self.housekeeper.on_snapshot(recording_id, log_position, tape);
                    eprintln!("snapshot recording {recording_id}: {timing}");
                }
            }
            _ => {}
        }
    }

    fn join_log(&mut self, active: ActiveLog) -> (AeronSubscription, AeronImage) {
        let channel = std::ffi::CString::new(active.channel.clone()).expect("channel has no nul");
        let subscription = self
            .aeron
            .async_add_subscription(&channel, active.stream_id, Handlers::NONE, Handlers::NONE)
            .expect("failed to subscribe to the log")
            .poll_blocking(Duration::from_secs(15))
            .expect("failed to subscribe to the log");

        let image = loop {
            if let Some(image) = subscription.image_by_session_id(active.session_id) {
                break image;
            }
            sleep(Duration::from_micros(100));
        };

        assert_eq!(image.position(), self.log_position, "the log must be contiguous");
        assert_eq!(active.log_position, self.log_position, "the log must be contiguous");

        self.max_log_position = active.max_log_position;
        if active.is_live() {
            self.service.on_join_live_log(&self.archive, active.role, active.log_position);
        }
        self.ack(active.log_position, NULL_VALUE);

        if self.role != active.role {
            self.role = active.role;
            self.service.on_role_change(active.role);
        }
        (subscription, image)
    }
}

/// Joins the node's consensus module and runs the service until it's told to terminate.
pub fn run<S: ClusteredService>(aeron: &Aeron, config: &NodeConfig, service: &mut S) {
    let cluster_id = config.cluster_id;
    let control = std::ffi::CString::new(CONTROL_CHANNEL).expect("channel has no nul");
    let consensus = aeron
        .async_add_publication(&control, CONSENSUS_MODULE_STREAM_ID)
        .expect("failed to add the consensus publication")
        .poll_blocking(Duration::from_secs(60))
        .expect("failed to add the consensus publication");
    let control_subscription = aeron
        .async_add_subscription(&control, SERVICE_STREAM_ID, Handlers::NONE, Handlers::NONE)
        .expect("failed to add the service subscription")
        .poll_blocking(Duration::from_secs(60))
        .expect("failed to add the service subscription");

    let (commit_counter, _) = await_counter(aeron, COMMIT_POSITION_TYPE_ID, cluster_id, 0);
    let (toggle_counter, _) = await_counter(aeron, CONTROL_TOGGLE_TYPE_ID, cluster_id, 0);
    let (_, recovery) = await_counter(aeron, RECOVERY_STATE_TYPE_ID, cluster_id, 24);
    let reader = aeron.counters_reader();
    let commit_position: *const i64 = reader.addr(commit_counter);
    let toggle_position: *const i64 = reader.addr(toggle_counter);
    // SAFETY: the counter lives in the driver's counters file for the client's lifetime, is
    // aligned for an i64, and every other writer uses atomic operations on it too.
    let control_toggle = unsafe { AtomicI64::from_ptr(toggle_position.cast_mut()) };

    let archive = snapshot::connect(config);
    if read_i64(&recovery, 0) != NULL_VALUE {
        let started = Instant::now();
        snapshot::load(&archive, read_i64(&recovery, 32), service);
        eprintln!("snapshot loaded in {:?}", started.elapsed());
    }
    service.on_recovered(&archive);

    let recovered_position = read_i64(&recovery, 8);
    let recovered_time = read_i64(&recovery, 16);
    let mut agent = Agent {
        aeron,
        archive,
        config,
        service,
        consensus,
        control_toggle,
        schedule: SnapshotSchedule::new(config.snapshots, recovered_position, recovered_time),
        housekeeper: Housekeeper::start(config),
        ack_id: 0,
        log_position: recovered_position,
        cluster_time: recovered_time,
        termination_position: NULL_POSITION,
        requested_ack_position: NULL_POSITION,
        active_log: None,
        max_log_position: 0,
        role: Role::Follower,
    };

    while !agent.consensus.is_connected() {
        sleep(Duration::from_micros(100));
    }
    agent.ack(agent.log_position, aeron.client_id());

    let mut log: Option<(AeronSubscription, AeronImage)> = None;
    let mut loops = 0u64;
    let mut last_keep_alive = Instant::now();

    loop {
        control_subscription
            .poll_fn(|buffer, _header| agent.on_control_message(buffer), 8)
            .expect("failed to poll the service control stream");

        if log.is_none()
            && let Some(active) = agent.active_log.take()
        {
            log = Some(agent.join_log(active));
        }

        if agent.termination_position != NULL_POSITION && agent.log_position >= agent.termination_position {
            agent.ack(agent.log_position, NULL_VALUE);
            return;
        }

        if agent.requested_ack_position != NULL_POSITION && agent.log_position >= agent.requested_ack_position {
            agent.requested_ack_position = NULL_POSITION;
            agent.ack(agent.log_position, NULL_VALUE);
        }

        let mut polled = 0;
        let mut finished = false;
        if let Some((_, image)) = &log {
            // SAFETY: the counter lives in the driver's counters file for the client's lifetime.
            let limit = unsafe { ptr::read_volatile(commit_position) };
            polled = image
                .bounded_poll_fn(
                    |buffer, header| agent.on_log_message(buffer, header.position()),
                    limit,
                    FRAGMENT_LIMIT,
                )
                .expect("failed to poll the log");
            finished = polled == 0
                && (image.position() >= agent.max_log_position
                    || image.is_end_of_stream()
                    || image.is_closed());
        }

        if finished {
            if let Some((_, image)) = log.take() {
                agent.log_position = agent.log_position.max(image.position());
            }
            if agent.role != Role::Follower {
                agent.role = Role::Follower;
                agent.service.on_role_change(Role::Follower);
            }
        }

        agent.service.do_background_work(&agent.archive);
        loops = loops.wrapping_add(1);
        // Checked rarely, because this loop runs between messages.
        if loops.is_multiple_of(KEEP_ALIVE_CHECK_LOOPS) && last_keep_alive.elapsed() >= ARCHIVE_KEEP_ALIVE {
            agent.archive.keep_alive();
            last_keep_alive = Instant::now();
        }
        agent.request_snapshot_if_due();

        if polled == 0 {
            spin_loop();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum OfferResult {
    Sent,
    Retry,
}

/// A cluster session, the Rust equivalent of Java's `AeronCluster`. It offers to whichever member
/// leads, and follows leadership changes announced on its egress stream.
pub struct ClusterClient {
    ingress: Vec<AeronPublication>,
    egress: AeronSubscription,
    response_channel: String,
    response_stream: i32,
    leader: Option<usize>,
    leadership_term_id: i64,
    session_id: i64,
    correlation_id: i64,
    last_keep_alive: Instant,
    leader_changes: u64,
    buffer: Vec<u8>,
}

impl ClusterClient {
    pub fn connect(
        aeron: &Aeron,
        ingress_endpoints: &[String],
        ingress_stream: i32,
        response_channel: &str,
        response_stream: i32,
    ) -> Result<Self, AeronCError> {
        let mut ingress = Vec::new();
        for endpoint in ingress_endpoints {
            let channel = std::ffi::CString::new(format!("aeron:udp?endpoint={endpoint}"))
                .expect("channel has no nul");
            ingress.push(aeron.async_add_publication(&channel, ingress_stream)?.poll_blocking(Duration::from_secs(10))?);
        }

        let channel = std::ffi::CString::new(response_channel).expect("channel has no nul");
        let egress = aeron
            .async_add_subscription(&channel, response_stream, Handlers::NONE, Handlers::NONE)?
            .poll_blocking(Duration::from_secs(10))?;

        let mut client = Self {
            ingress,
            egress,
            response_channel: response_channel.to_string(),
            response_stream,
            leader: None,
            leadership_term_id: NULL_VALUE,
            session_id: NULL_VALUE,
            correlation_id: 1,
            last_keep_alive: Instant::now(),
            leader_changes: 0,
            buffer: vec![0; 4096],
        };

        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last_request = Instant::now() - Duration::from_secs(1);
        while client.session_id == NULL_VALUE {
            assert!(Instant::now() < deadline, "no cluster leader accepted the session");
            if last_request.elapsed() > Duration::from_millis(250) {
                client.send_connect_requests();
                last_request = Instant::now();
            }
            client.poll_egress();
            sleep(Duration::from_millis(1));
        }
        client.leader_changes = 0;
        Ok(client)
    }

    fn send_connect_requests(&mut self) {
        let channel = self.response_channel.as_bytes();
        let mut message = vec![0u8; HEADER_LENGTH + 16 + 4 + channel.len() + 4 + 4];
        write_header(&mut message, 16, TEMPLATE_CONNECT_REQUEST);
        self.correlation_id += 1;
        message[8..16].copy_from_slice(&self.correlation_id.to_le_bytes());
        message[16..20].copy_from_slice(&self.response_stream.to_le_bytes());
        message[20..24].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        message[24..28].copy_from_slice(&(channel.len() as u32).to_le_bytes());
        message[28..28 + channel.len()].copy_from_slice(channel);

        for publication in &self.ingress {
            let _ = publication.offer(&message);
        }
    }

    pub fn poll_egress(&mut self) {
        let mut session_events = Vec::new();
        let mut new_leaders = Vec::new();
        let _ = self.egress.poll_fn(
            |buffer, _header| match template_of(buffer) {
                Some(TEMPLATE_SESSION_EVENT) => {
                    let body = &buffer[HEADER_LENGTH..];
                    session_events.push((
                        read_i64(body, 0),
                        read_i64(body, 16),
                        read_i32(body, 24),
                        read_i32(body, 28),
                    ));
                }
                Some(TEMPLATE_NEW_LEADER_EVENT) => {
                    let body = &buffer[HEADER_LENGTH..];
                    new_leaders.push((read_i64(body, 0), read_i64(body, 8), read_i32(body, 16)));
                }
                _ => {}
            },
            16,
        );

        for (session, term, leader, code) in session_events {
            match code {
                EVENT_OK if self.session_id == NULL_VALUE => {
                    self.session_id = session;
                    self.leadership_term_id = term;
                    self.leader = Some(leader as usize);
                }
                EVENT_REDIRECT | EVENT_ERROR | EVENT_OK => {}
                _ => {}
            }
        }
        for (term, session, leader) in new_leaders {
            if session == self.session_id && term >= self.leadership_term_id {
                self.leadership_term_id = term;
                self.leader = Some(leader as usize);
                self.leader_changes += 1;
            }
        }
    }

    pub fn leader_changes(&self) -> u64 {
        self.leader_changes
    }

    pub fn leadership_term(&self) -> i64 {
        self.leadership_term_id
    }

    pub fn leader(&self) -> Option<usize> {
        self.leader
    }

    pub fn is_leader_connected(&self) -> bool {
        self.leader.is_some_and(|leader| self.ingress[leader].is_connected())
    }

    pub fn offer(&mut self, payload: &[u8]) -> OfferResult {
        let Some(leader) = self.leader else { return OfferResult::Retry };
        let length = SESSION_HEADER_LENGTH + payload.len();
        write_header(&mut self.buffer, 24, TEMPLATE_SESSION_MESSAGE);
        self.buffer[8..16].copy_from_slice(&self.leadership_term_id.to_le_bytes());
        self.buffer[16..24].copy_from_slice(&self.session_id.to_le_bytes());
        self.buffer[24..32].fill(0);
        self.buffer[SESSION_HEADER_LENGTH..length].copy_from_slice(payload);

        match self.ingress[leader].offer(&self.buffer[..length]) {
            Ok(_) => OfferResult::Sent,
            Err(_) => OfferResult::Retry,
        }
    }

    pub fn keep_alive(&mut self) {
        if self.last_keep_alive.elapsed() < Duration::from_secs(1) {
            return;
        }
        let Some(leader) = self.leader else { return };
        let mut message = [0u8; HEADER_LENGTH + 16];
        write_header(&mut message, 16, TEMPLATE_KEEP_ALIVE);
        message[8..16].copy_from_slice(&self.leadership_term_id.to_le_bytes());
        message[16..24].copy_from_slice(&self.session_id.to_le_bytes());
        if self.ingress[leader].offer(&message).is_ok() {
            self.last_keep_alive = Instant::now();
        }
    }
}
