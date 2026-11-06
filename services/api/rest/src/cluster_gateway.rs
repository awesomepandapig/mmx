//! Sends orders through the cluster and answers each request with what the engine decided.
//!
//! A request is held until its first report arrives. If the leader changes, orders in flight may
//! or may not have reached the log, so sending pauses until the new term's marker. Every report
//! before it is final, so any order still waiting never reached the log and is safe to resend.
//!
//! That resend is the gateway's own and happens below the ID claim, so it needs no claim of its
//! own. Repeats from callers never get this far, because they stop at the claim.

use super::aeron_handler::{EngineLink, EngineRequest, OrderKey, OrderOutcome, OrderSender, OutboundOrder};
use super::order::UuidType;
use super::recorder::ReportStream;

use std::collections::{HashMap, VecDeque};
use std::env;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cluster::{ClusterClient, OfferResult};

use log::{error, info, warn};

use sbe::ReadBuf;
use sbe::exec_type_enum::ExecTypeEnum;
use sbe::execution_report_codec::ExecutionReportDecoder;
use sbe::message_header_codec::MessageHeaderDecoder;
use sbe::order_cancel_reject_codec::OrderCancelRejectDecoder;
use sbe::ord_rej_reason_enum::OrdRejReasonEnum;

use serde::Serialize;

use tokio::sync::{mpsc, oneshot};
use tokio::sync::mpsc::error::TryRecvError;

use transport::AeronSubscription;

const ORDER_QUEUE_LENGTH: usize = 4096;
const INGRESS_STREAM: i32 = 101;
const EGRESS_STREAM: i32 = 102;
const REPORT_STREAM: i32 = 1002;
const REPORT_TEMPLATE: u16 = 3;
const CANCEL_REJECT_TEMPLATE: u16 = 4;
const TERM_MARKER_TEMPLATE: u16 = 100;
const SWEEP_INTERVAL: Duration = Duration::from_millis(50);
const POSITION_INTERVAL: Duration = Duration::from_millis(5);

/// Where to find the cluster. All three variables hold comma-separated values, one per member.
pub struct ClusterConfig {
    ingress_endpoints: Vec<String>,
    response_channel: String,
    report_channels: Vec<String>,
}

impl ClusterConfig {
    pub fn from_env() -> Option<Self> {
        let list = |name: &str| -> Option<Vec<String>> {
            let value = env::var(name).ok()?;
            Some(value.split(',').map(str::to_string).collect())
        };
        Some(Self {
            ingress_endpoints: list("CLUSTER_INGRESS_ENDPOINTS")?,
            response_channel: env::var("CLUSTER_RESPONSE_CHANNEL").ok()?,
            report_channels: list("CLUSTER_REPORT_CHANNELS")?,
        })
    }
}

/// A request waiting on the engine. Identical requests share one, so each waiter gets the same
/// answer and only one message reaches the engine.
struct PendingOrder {
    sequence: u64,
    cancel_id: Option<UuidType>,
    message: Vec<u8>,
    answers: Vec<oneshot::Sender<OrderOutcome>>,
    deadline: Instant,
}

impl PendingOrder {
    fn answer(self, outcome: impl Fn() -> OrderOutcome) {
        for answer in self.answers {
            let _ = answer.send(outcome());
        }
    }
}

/// What a request is waiting for. A cancel's own ID differs on every attempt, so requests match on
/// the order they're about, which lets a repeated cancel join the attempt already in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Waiting {
    New(OrderKey),
    Cancel(OrderKey),
}

/// What a report means for the requests waiting on it.
#[derive(Debug, PartialEq, Eq)]
enum Decoded {
    Accepted { exec_id: u64, order: OrderKey },
    Rejected { exec_id: u64, order: OrderKey, repeat: bool },
    Canceled { exec_id: u64, order: OrderKey },
    CancelRejected { cancel_id: UuidType },
    Progress { exec_id: u64 },
    TermMarker(i64),
    Other,
}

fn decode(buffer: &[u8]) -> Decoded {
    if buffer.len() < 16 {
        return Decoded::Other;
    }
    match u16::from_le_bytes([buffer[2], buffer[3]]) {
        TERM_MARKER_TEMPLATE => {
            Decoded::TermMarker(i64::from_le_bytes(buffer[8..16].try_into().expect("eight bytes")))
        }
        REPORT_TEMPLATE => {
            let header: MessageHeaderDecoder<ReadBuf<'_>> =
                MessageHeaderDecoder::default().wrap(ReadBuf::new(buffer), 0);
            let report: ExecutionReportDecoder<'_> =
                ExecutionReportDecoder::default().header(header, 0);
            let exec_id = report.exec_id();
            let order = (report.account(), report.cl_ord_id());
            match report.exec_type() {
                ExecTypeEnum::New => Decoded::Accepted { exec_id, order },
                ExecTypeEnum::Rejected => Decoded::Rejected {
                    exec_id,
                    order,
                    repeat: report.ord_rej_reason() == OrdRejReasonEnum::DuplicateOrder,
                },
                ExecTypeEnum::Canceled => Decoded::Canceled { exec_id, order },
                _ => Decoded::Progress { exec_id },
            }
        }
        CANCEL_REJECT_TEMPLATE => {
            let header: MessageHeaderDecoder<ReadBuf<'_>> =
                MessageHeaderDecoder::default().wrap(ReadBuf::new(buffer), 0);
            let reject: OrderCancelRejectDecoder<'_> =
                OrderCancelRejectDecoder::default().header(header, 0);
            Decoded::CancelRejected { cancel_id: reject.cl_ord_id() }
        }
        _ => Decoded::Other,
    }
}

/// The gateway's decisions, kept apart from Aeron so they can be driven directly.
struct Tracker {
    pending: HashMap<Waiting, PendingOrder>,
    unsent: VecDeque<Waiting>,
    /// Finds a cancel from its reject, which doesn't name the account.
    cancels: HashMap<UuidType, OrderKey>,
    next_sequence: u64,
    last_exec_id: u64,
    marker_term: i64,
    awaiting_term: bool,
}

impl Tracker {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
            unsent: VecDeque::new(),
            cancels: HashMap::new(),
            next_sequence: 0,
            last_exec_id: 0,
            marker_term: -1,
            awaiting_term: false,
        }
    }

    fn resolve(&mut self, waiting: Waiting, outcome: impl Fn() -> OrderOutcome) {
        if let Some(order) = self.pending.remove(&waiting) {
            if let Some(cancel_id) = order.cancel_id {
                self.cancels.remove(&cancel_id);
            }
            order.answer(outcome);
        }
    }

    /// Reports arrive once per node, so the exec ID tells which ones are new.
    fn is_new(&mut self, exec_id: u64) -> bool {
        let new = exec_id > self.last_exec_id;
        self.last_exec_id = self.last_exec_id.max(exec_id);
        new
    }

    fn on_report(&mut self, buffer: &[u8]) {
        match decode(buffer) {
            Decoded::TermMarker(term) => self.marker_term = self.marker_term.max(term),
            Decoded::Accepted { exec_id, order } if self.is_new(exec_id) => {
                self.resolve(Waiting::New(order), || OrderOutcome::Accepted);
            }
            // The ID was claimed, so nobody else sent this order. The engine holding it can only
            // mean a resend found the first attempt resting, and the order is working.
            Decoded::Rejected { exec_id, order, repeat: true } if self.is_new(exec_id) => {
                warn!("Cluster: A resend found its order already working");
                self.resolve(Waiting::New(order), || OrderOutcome::Accepted);
            }
            Decoded::Rejected { exec_id, order, repeat: false } if self.is_new(exec_id) => {
                self.resolve(Waiting::New(order), || OrderOutcome::Rejected);
            }
            Decoded::Canceled { exec_id, order } if self.is_new(exec_id) => {
                self.resolve(Waiting::Cancel(order), || OrderOutcome::Canceled);
            }
            // A cancel reject has no exec ID, but its cancel ID is unique and can only resolve once.
            Decoded::CancelRejected { cancel_id } => {
                if let Some(order) = self.cancels.get(&cancel_id).copied() {
                    self.resolve(Waiting::Cancel(order), || OrderOutcome::NotOpen);
                }
            }
            Decoded::Progress { exec_id } => {
                self.is_new(exec_id);
            }
            _ => {}
        }
    }

    fn on_leader_change(&mut self) {
        self.awaiting_term = true;
    }

    /// Returns how many requests will be resent, once the new term has resolved them.
    fn resolve_term(&mut self, leadership_term: i64, leader_connected: bool) -> Option<usize> {
        if !self.awaiting_term || self.marker_term < leadership_term || !leader_connected {
            return None;
        }
        self.awaiting_term = false;
        // Oldest first, so a cancel can't overtake the order it cancels.
        let mut held: Vec<(u64, Waiting)> =
            self.pending.iter().map(|(waiting, order)| (order.sequence, *waiting)).collect();
        held.sort_unstable_by_key(|(sequence, _)| *sequence);
        self.unsent = held.into_iter().map(|(_, waiting)| waiting).collect();
        Some(self.unsent.len())
    }

    fn admit(&mut self, order: OutboundOrder) {
        let OutboundOrder { request, message, send_by, published } = order;
        let (waiting, cancel_id) = match request {
            EngineRequest::New(order) => (Waiting::New(order), None),
            EngineRequest::Cancel { order, cancel_id } => (Waiting::Cancel(order), Some(cancel_id)),
        };

        // A repeated cancel can arrive while the first is still waiting.
        if let Some(existing) = self.pending.get_mut(&waiting) {
            existing.answers.push(published);
            return;
        }

        if let (Some(cancel_id), Waiting::Cancel(order)) = (cancel_id, waiting) {
            self.cancels.insert(cancel_id, order);
        }
        self.next_sequence += 1;
        self.pending.insert(
            waiting,
            PendingOrder {
                sequence: self.next_sequence,
                cancel_id,
                message,
                answers: vec![published],
                deadline: send_by,
            },
        );
        self.unsent.push_back(waiting);
    }

    /// Sends what it can, stopping when `offer` refuses. Returns whether anything was sent. Nothing
    /// goes out past its deadline, because an order's status relies on that. `expire` answers those.
    fn send_unsent(&mut self, now: Instant, mut offer: impl FnMut(&[u8]) -> bool) -> bool {
        if self.awaiting_term {
            return false;
        }
        let mut sent = false;
        while let Some(waiting) = self.unsent.front() {
            let Some(order) = self.pending.get(waiting) else {
                self.unsent.pop_front();
                continue;
            };
            if order.deadline <= now {
                self.unsent.pop_front();
                continue;
            }
            if !offer(&order.message) {
                break;
            }
            sent = true;
            self.unsent.pop_front();
        }
        sent
    }

    fn expire(&mut self, now: Instant) {
        let expired: Vec<Waiting> = self
            .pending
            .iter()
            .filter(|(_, order)| order.deadline <= now)
            .map(|(waiting, _)| *waiting)
            .collect();
        for waiting in &expired {
            self.resolve(*waiting, || OrderOutcome::TimedOut);
        }
        // A repeat of an expired cancel queues again, and mustn't be sent twice.
        if !expired.is_empty() {
            self.unsent.retain(|waiting| !expired.contains(waiting));
        }
    }
}

/// One cluster member as this gateway hears it. Each member publishes its own copy of the report
/// stream, and the copies are byte-identical, so positions compare directly.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    pub member: usize,
    /// Whether this member's stream is reaching the gateway.
    pub connected: bool,
    /// Bytes of the report stream read from this member. `None` until it's first connected.
    pub position: Option<i64>,
}

/// What this gateway knows of the cluster, for operators.
#[derive(Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ClusterView {
    pub term: i64,
    pub leader: Option<usize>,
    pub leader_connected: bool,
    pub leader_changes: u64,
    /// Unix milliseconds of the last change this gateway saw. It's `None` if there's been none
    /// since it started.
    pub last_leader_change_ms: Option<u64>,
    /// The newest exec ID on the report stream, which is how far consumers have to go.
    pub last_exec_id: u64,
    pub members: Vec<MemberView>,
}

/// Aeron handles can't leave their thread, so it copies what it knows here for the admin route.
#[derive(Default)]
pub struct ClusterWatch(RwLock<ClusterView>);

impl ClusterWatch {
    fn publish(&self, view: ClusterView) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = view;
    }

    pub fn view(&self) -> ClusterView {
        self.0.read().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

/// Reads the member behind each report channel, which are listed in member order. A member back from
/// a restart can briefly have two images, so the furthest one speaks for it.
fn member_views(reports: &[AeronSubscription], stream: &ReportStream) -> Vec<MemberView> {
    reports
        .iter()
        .enumerate()
        .map(|(member, subscription)| {
            let mut position = None;
            subscription.for_each_image(|image| {
                let read = image.position();
                position = Some(position.map_or(read, |best: i64| best.max(read)));
                stream.advance_to(read);
            });
            MemberView { member, connected: position.is_some(), position }
        })
        .collect()
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_millis() as u64)
}

fn on_error(code: i32, message: &str) {
    error!("Aeron: Error {}: {}", code, message);
}

pub fn spawn_cluster_gateway(
    config: ClusterConfig,
    stream: Arc<ReportStream>,
    link: Arc<EngineLink>,
    watch: Arc<ClusterWatch>,
) -> Result<OrderSender, String> {
    let (orders, incoming) = mpsc::channel::<OutboundOrder>(ORDER_QUEUE_LENGTH);
    let (started, startup) = std_mpsc::channel();

    thread::Builder::new()
        .name("cluster-gateway".to_string())
        .spawn(move || run(config, stream, link, watch, incoming, started))
        .map_err(|err| err.to_string())?;

    startup.recv().map_err(|err| err.to_string())??;
    Ok(orders)
}

fn run(
    config: ClusterConfig,
    stream: Arc<ReportStream>,
    link: Arc<EngineLink>,
    watch: Arc<ClusterWatch>,
    mut incoming: mpsc::Receiver<OutboundOrder>,
    started: std_mpsc::Sender<Result<(), String>>,
) {
    let setup = (|| {
        let aeron = transport::connect(on_error).map_err(|err| format!("{err:?}"))?;
        // Every node publishes the same stream, so reading them all makes a dead node irrelevant.
        let reports = config
            .report_channels
            .iter()
            .map(|channel| transport::subscription(&aeron, channel, REPORT_STREAM))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("{err:?}"))?;
        let client = ClusterClient::connect(
            &aeron,
            &config.ingress_endpoints,
            INGRESS_STREAM,
            &config.response_channel,
            EGRESS_STREAM,
        )
        .map_err(|err| format!("{err:?}"))?;
        Ok::<_, String>((aeron, reports, client))
    })();

    let (_aeron, reports, mut client) = match setup {
        Ok(ready) => {
            let _ = started.send(Ok(()));
            ready
        }
        Err(err) => {
            let _ = started.send(Err(err));
            return;
        }
    };
    info!("Cluster: Session open, leader is member {:?}", client.leader());

    let mut tracker = Tracker::new();
    let mut leader_changes = client.leader_changes();
    let mut last_sweep = Instant::now();
    let mut last_position = Instant::now();
    let mut last_leader_change_ms = None;

    loop {
        let mut idle = true;

        for subscription in &reports {
            let _ = subscription.poll_fn(
                // Ahead of the answer, so anything that follows it waits for this report too.
                |buffer, header| {
                    stream.advance_to(header.position());
                    tracker.on_report(buffer);
                },
                256,
            );
        }

        if last_position.elapsed() >= POSITION_INTERVAL {
            last_position = Instant::now();
            let members = member_views(&reports, &stream);
            // An order needs a way in and a way to hear the answer.
            let heard = members.iter().any(|member| member.connected);
            link.set(client.is_leader_connected() && heard);
            watch.publish(ClusterView {
                term: client.leadership_term(),
                leader: client.leader(),
                leader_connected: client.is_leader_connected(),
                leader_changes: client.leader_changes(),
                last_leader_change_ms,
                last_exec_id: tracker.last_exec_id,
                members,
            });
        }

        client.poll_egress();
        client.keep_alive();

        if client.leader_changes() != leader_changes {
            leader_changes = client.leader_changes();
            last_leader_change_ms = Some(unix_ms());
            tracker.on_leader_change();
            warn!("Cluster: Leader changed to {:?}, holding orders", client.leader());
        }

        let term = client.leadership_term();
        if let Some(resending) = tracker.resolve_term(term, client.is_leader_connected()) {
            info!("Cluster: Term {} resolved, resending {} orders", term, resending);
        }

        match incoming.try_recv() {
            Ok(order) => {
                idle = false;
                tracker.admit(order);
            }
            Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => {}
        }

        let now = Instant::now();
        if tracker.send_unsent(now, |message| client.offer(message) == OfferResult::Sent) {
            idle = false;
        }

        if last_sweep.elapsed() >= SWEEP_INTERVAL {
            last_sweep = Instant::now();
            tracker.expire(last_sweep);
        }

        if idle {
            thread::sleep(Duration::from_micros(20));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::aeron_handler::REQUEST_DEADLINE;

    use sbe::WriteBuf;
    use sbe::execution_report_codec::ExecutionReportEncoder;
    use sbe::message_header_codec::ENCODED_LENGTH;
    use sbe::order_cancel_reject_codec::OrderCancelRejectEncoder;
    use sbe::ord_status_enum::OrdStatusEnum;

    use tokio::sync::oneshot::error::TryRecvError as AnswerEmpty;

    const PLAYER: UuidType = [7, 7];

    fn report(exec_id: u64, order: OrderKey, exec_type: ExecTypeEnum, reason: OrdRejReasonEnum) -> Vec<u8> {
        let mut buffer = vec![0u8; ENCODED_LENGTH + 130];
        let encoder = ExecutionReportEncoder::default().wrap(WriteBuf::new(&mut buffer), ENCODED_LENGTH);
        let mut encoder = encoder.header(0).parent().unwrap();
        encoder.account(&order.0);
        encoder.cl_ord_id(&order.1);
        encoder.exec_id(exec_id);
        encoder.exec_type(exec_type);
        encoder.ord_status(OrdStatusEnum::New);
        encoder.ord_rej_reason(reason);
        buffer
    }

    fn accepted(exec_id: u64, order: OrderKey) -> Vec<u8> {
        report(exec_id, order, ExecTypeEnum::New, OrdRejReasonEnum::NullVal)
    }

    fn canceled(exec_id: u64, order: OrderKey) -> Vec<u8> {
        report(exec_id, order, ExecTypeEnum::Canceled, OrdRejReasonEnum::NullVal)
    }

    fn cancel_reject(cancel_id: UuidType) -> Vec<u8> {
        let mut buffer = vec![0u8; ENCODED_LENGTH + 43];
        let encoder = OrderCancelRejectEncoder::default().wrap(WriteBuf::new(&mut buffer), ENCODED_LENGTH);
        let mut encoder = encoder.header(0).parent().unwrap();
        encoder.cl_ord_id(&cancel_id);
        buffer
    }

    fn term_marker(term: i64) -> Vec<u8> {
        let mut buffer = vec![0u8; 16];
        buffer[2..4].copy_from_slice(&TERM_MARKER_TEMPLATE.to_le_bytes());
        buffer[8..16].copy_from_slice(&term.to_le_bytes());
        buffer
    }

    fn tracker() -> Tracker {
        Tracker::new()
    }

    fn key(id: u64) -> OrderKey {
        (PLAYER, [id, id])
    }

    fn outbound(request: EngineRequest, tag: u8) -> (OutboundOrder, oneshot::Receiver<OrderOutcome>) {
        let (published, answer) = oneshot::channel();
        let send_by = Instant::now() + REQUEST_DEADLINE;
        (OutboundOrder { request, message: vec![tag], send_by, published }, answer)
    }

    fn admit(tracker: &mut Tracker, request: EngineRequest, tag: u8) -> oneshot::Receiver<OrderOutcome> {
        let (order, answer) = outbound(request, tag);
        tracker.admit(order);
        answer
    }

    fn drain(tracker: &mut Tracker) -> Vec<u8> {
        let mut sent = Vec::new();
        tracker.send_unsent(Instant::now(), |message| {
            sent.push(message[0]);
            true
        });
        sent
    }

    #[test]
    fn answers_with_the_engines_decision() {
        let mut tracker = tracker();
        let mut answer = admit(&mut tracker, EngineRequest::New(key(1)), 1);
        assert_eq!(drain(&mut tracker), [1]);
        assert_eq!(answer.try_recv(), Err(AnswerEmpty::Empty), "held until the first report");

        tracker.on_report(&accepted(1, key(1)));
        assert_eq!(answer.try_recv(), Ok(OrderOutcome::Accepted));
    }

    #[test]
    fn a_reject_is_final() {
        let mut tracker = tracker();
        let mut answer = admit(&mut tracker, EngineRequest::New(key(1)), 1);
        tracker.on_report(&report(1, key(1), ExecTypeEnum::Rejected, OrdRejReasonEnum::Other));
        assert_eq!(answer.try_recv(), Ok(OrderOutcome::Rejected));
    }

    /// A resend never passes through the claim, so it can't be mistaken for a caller's repeat, and
    /// the engine holding the order means the caller's one order is working.
    #[test]
    fn a_resend_that_finds_its_order_working_is_accepted() {
        let mut tracker = tracker();
        let mut answer = admit(&mut tracker, EngineRequest::New(key(1)), 1);
        assert_eq!(drain(&mut tracker), [1]);

        tracker.on_leader_change();
        tracker.on_report(&term_marker(1));
        assert_eq!(tracker.resolve_term(1, true), Some(1));
        assert_eq!(drain(&mut tracker), [1], "the gateway's own resend goes out with no claim");

        tracker.on_report(&report(1, key(1), ExecTypeEnum::Rejected, OrdRejReasonEnum::DuplicateOrder));
        assert_eq!(answer.try_recv(), Ok(OrderOutcome::Accepted));
    }

    #[test]
    fn the_same_report_from_another_node_changes_nothing() {
        let mut tracker = tracker();
        tracker.on_report(&accepted(5, key(1)));
        let mut answer = admit(&mut tracker, EngineRequest::New(key(2)), 2);
        // An old exec ID, as a slower node replays what a faster one already delivered.
        tracker.on_report(&accepted(5, key(2)));
        assert_eq!(answer.try_recv(), Err(AnswerEmpty::Empty));
    }

    #[test]
    fn a_failover_holds_orders_until_the_term_resolves() {
        let mut tracker = tracker();
        let mut landed = admit(&mut tracker, EngineRequest::New(key(1)), 1);
        let mut lost = admit(&mut tracker, EngineRequest::New(key(2)), 2);
        assert_eq!(drain(&mut tracker), [1, 2]);

        tracker.on_leader_change();
        let mut late = admit(&mut tracker, EngineRequest::New(key(3)), 3);
        assert!(drain(&mut tracker).is_empty(), "nothing is sent while the term is unresolved");
        assert_eq!(tracker.resolve_term(1, true), None, "no marker yet");

        tracker.on_report(&accepted(1, key(1)));
        tracker.on_report(&term_marker(1));
        assert_eq!(tracker.resolve_term(1, false), None, "no leader yet");
        assert_eq!(tracker.resolve_term(1, true), Some(2));

        assert_eq!(landed.try_recv(), Ok(OrderOutcome::Accepted));
        assert_eq!(drain(&mut tracker), [2, 3], "only what never reached the log is resent");
        assert_eq!(lost.try_recv(), Err(AnswerEmpty::Empty));
        assert_eq!(late.try_recv(), Err(AnswerEmpty::Empty));
    }

    #[test]
    fn a_stale_marker_does_not_resolve_a_newer_term() {
        let mut tracker = tracker();
        tracker.on_leader_change();
        tracker.on_report(&term_marker(1));
        assert_eq!(tracker.resolve_term(2, true), None);
    }

    #[test]
    fn resending_keeps_a_cancel_behind_its_order() {
        let mut tracker = tracker();
        for id in 1..=8 {
            admit(&mut tracker, EngineRequest::New(key(id)), id as u8);
        }
        let cancel = EngineRequest::Cancel { order: key(1), cancel_id: [9, 9] };
        admit(&mut tracker, cancel, 9);
        drain(&mut tracker);

        tracker.on_leader_change();
        tracker.on_report(&term_marker(0));
        assert_eq!(tracker.resolve_term(0, true), Some(9));
        assert_eq!(drain(&mut tracker), [1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn the_deadline_answers_unknown_and_nothing_is_resent_after_it() {
        let mut tracker = tracker();
        let mut answer = admit(&mut tracker, EngineRequest::New(key(1)), 1);
        drain(&mut tracker);

        tracker.expire(Instant::now() + REQUEST_DEADLINE + Duration::from_millis(1));
        assert_eq!(answer.try_recv(), Ok(OrderOutcome::TimedOut));

        // The order may have landed, so a later failover must leave it alone.
        tracker.on_leader_change();
        tracker.on_report(&term_marker(1));
        assert_eq!(tracker.resolve_term(1, true), Some(0));
        assert!(drain(&mut tracker).is_empty());
    }

    #[test]
    fn an_order_never_goes_out_past_its_deadline() {
        let mut tracker = tracker();
        let mut answer = admit(&mut tracker, EngineRequest::New(key(1)), 1);
        assert!(!tracker.send_unsent(Instant::now(), |_| false), "back pressure keeps it queued");

        let late = Instant::now() + REQUEST_DEADLINE + Duration::from_millis(1);
        assert!(!tracker.send_unsent(late, |_| true), "the sweep hasn't run, and still nothing goes");
        tracker.expire(late);
        assert_eq!(answer.try_recv(), Ok(OrderOutcome::TimedOut));
    }

    #[test]
    fn a_late_resend_does_not_outlive_the_deadline_either() {
        let mut tracker = tracker();
        admit(&mut tracker, EngineRequest::New(key(1)), 1);
        drain(&mut tracker);

        tracker.on_leader_change();
        tracker.on_report(&term_marker(1));
        assert_eq!(tracker.resolve_term(1, true), Some(1));
        let late = Instant::now() + REQUEST_DEADLINE + Duration::from_millis(1);
        assert!(!tracker.send_unsent(late, |_| true));
    }

    #[test]
    fn an_order_queued_too_long_is_not_sent() {
        let mut tracker = tracker();
        let (mut order, mut answer) = outbound(EngineRequest::New(key(1)), 1);
        order.send_by = Instant::now();
        tracker.admit(order);
        assert!(drain(&mut tracker).is_empty());
        tracker.expire(Instant::now());
        assert_eq!(answer.try_recv(), Ok(OrderOutcome::TimedOut));
    }

    #[test]
    fn a_repeated_cancel_after_expiry_goes_once() {
        let mut tracker = tracker();
        let cancel = EngineRequest::Cancel { order: key(1), cancel_id: [9, 9] };
        admit(&mut tracker, cancel, 1);
        assert!(!tracker.send_unsent(Instant::now(), |_| false));
        tracker.expire(Instant::now() + REQUEST_DEADLINE + Duration::from_millis(1));

        admit(&mut tracker, cancel, 1);
        assert_eq!(drain(&mut tracker), [1]);
    }

    #[test]
    fn two_players_can_use_the_same_client_order_id() {
        let mut tracker = tracker();
        let (mine, theirs) = (([1, 1], [5, 5]), ([2, 2], [5, 5]));
        let mut first = admit(&mut tracker, EngineRequest::New(mine), 1);
        let mut second = admit(&mut tracker, EngineRequest::New(theirs), 2);
        assert_eq!(drain(&mut tracker), [1, 2]);

        tracker.on_report(&accepted(1, mine));
        assert_eq!(first.try_recv(), Ok(OrderOutcome::Accepted));
        assert_eq!(second.try_recv(), Err(AnswerEmpty::Empty));
    }

    #[test]
    fn a_cancel_is_answered_by_the_orders_cancel_report() {
        let mut tracker = tracker();
        let cancel = EngineRequest::Cancel { order: key(1), cancel_id: [9, 9] };
        let mut answer = admit(&mut tracker, cancel, 1);
        tracker.on_report(&accepted(1, key(2)));
        tracker.on_report(&canceled(2, key(2)));
        assert_eq!(answer.try_recv(), Err(AnswerEmpty::Empty), "another order's cancel");

        tracker.on_report(&canceled(3, key(1)));
        assert_eq!(answer.try_recv(), Ok(OrderOutcome::Canceled));
        assert!(tracker.cancels.is_empty());
    }

    #[test]
    fn a_cancel_is_answered_by_its_reject() {
        let mut tracker = tracker();
        let cancel = EngineRequest::Cancel { order: key(1), cancel_id: [9, 9] };
        let mut answer = admit(&mut tracker, cancel, 1);

        tracker.on_report(&cancel_reject([8, 8]));
        assert_eq!(answer.try_recv(), Err(AnswerEmpty::Empty), "another cancel's reject");

        tracker.on_report(&cancel_reject([9, 9]));
        assert_eq!(answer.try_recv(), Ok(OrderOutcome::NotOpen));
    }

    #[test]
    fn repeated_cancels_share_one_message() {
        let mut tracker = tracker();
        let mut first = admit(&mut tracker, EngineRequest::Cancel { order: key(1), cancel_id: [8, 8] }, 1);
        let mut second = admit(&mut tracker, EngineRequest::Cancel { order: key(1), cancel_id: [9, 9] }, 2);
        assert_eq!(drain(&mut tracker), [1]);

        tracker.on_report(&canceled(1, key(1)));
        assert_eq!(first.try_recv(), Ok(OrderOutcome::Canceled));
        assert_eq!(second.try_recv(), Ok(OrderOutcome::Canceled));
    }

    #[test]
    fn an_expired_cancel_forgets_its_id() {
        let mut tracker = tracker();
        let mut answer = admit(&mut tracker, EngineRequest::Cancel { order: key(1), cancel_id: [9, 9] }, 1);
        tracker.expire(Instant::now() + REQUEST_DEADLINE + Duration::from_millis(1));
        assert_eq!(answer.try_recv(), Ok(OrderOutcome::TimedOut));
        assert!(tracker.cancels.is_empty());
        tracker.on_report(&cancel_reject([9, 9]));
    }

    #[test]
    fn short_or_unknown_fragments_are_ignored() {
        assert_eq!(decode(&[0; 4]), Decoded::Other);
        let mut buffer = [0u8; 16];
        buffer[2] = 200;
        assert_eq!(decode(&buffer), Decoded::Other);
    }
}
