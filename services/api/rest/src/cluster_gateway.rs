//! Sends orders through the cluster and answers each request with what the engine decided.
//!
//! A request is held until its first report arrives. If the leader changes, orders in flight may
//! or may not have reached the log, so sending pauses until the new term's marker. Every report
//! before it is final, so any order still waiting never reached the log and is safe to resend.

use super::aeron_handler::{OrderOutcome, OrderSender, OutboundOrder};
use super::order::{MESSAGE_SIZE, UuidType};

use std::collections::{HashMap, VecDeque};
use std::env;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use cluster::{ClusterClient, OfferResult};

use log::{error, info, warn};

use sbe::ReadBuf;
use sbe::exec_type_enum::ExecTypeEnum;
use sbe::execution_report_codec::ExecutionReportDecoder;
use sbe::message_header_codec::MessageHeaderDecoder;

use tokio::sync::{mpsc, oneshot};
use tokio::sync::mpsc::error::TryRecvError;

const ORDER_QUEUE_LENGTH: usize = 4096;
const INGRESS_STREAM: i32 = 101;
const EGRESS_STREAM: i32 = 102;
const REPORT_STREAM: i32 = 1002;
const REPORT_TEMPLATE: u16 = 3;
const TERM_MARKER_TEMPLATE: u16 = 100;
const REQUEST_DEADLINE: Duration = Duration::from_secs(3);

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

struct PendingOrder {
    message: [u8; MESSAGE_SIZE],
    answer: oneshot::Sender<OrderOutcome>,
    deadline: Instant,
}

/// What a report means for the request waiting on it.
enum Decoded {
    Answer { exec_id: u64, cl_ord_id: UuidType, outcome: OrderOutcome },
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
            match report.exec_type() {
                ExecTypeEnum::New => Decoded::Answer {
                    exec_id,
                    cl_ord_id: report.cl_ord_id(),
                    outcome: OrderOutcome::Accepted,
                },
                ExecTypeEnum::Rejected => Decoded::Answer {
                    exec_id,
                    cl_ord_id: report.cl_ord_id(),
                    outcome: OrderOutcome::Rejected,
                },
                _ => Decoded::Progress { exec_id },
            }
        }
        _ => Decoded::Other,
    }
}

fn on_error(code: i32, message: &str) {
    error!("Aeron: Error {}: {}", code, message);
}

pub fn spawn_cluster_gateway(config: ClusterConfig) -> Result<OrderSender, String> {
    let (orders, incoming) = mpsc::channel::<OutboundOrder>(ORDER_QUEUE_LENGTH);
    let (started, startup) = std_mpsc::channel();

    thread::Builder::new()
        .name("cluster-gateway".to_string())
        .spawn(move || run(config, incoming, started))
        .map_err(|err| err.to_string())?;

    startup.recv().map_err(|err| err.to_string())??;
    Ok(orders)
}

fn run(
    config: ClusterConfig,
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

    let mut pending: HashMap<UuidType, PendingOrder> = HashMap::new();
    let mut unsent: VecDeque<UuidType> = VecDeque::new();
    let mut last_exec_id = 0;
    let mut marker_term = -1;
    let mut leader_changes = client.leader_changes();
    let mut awaiting_term = false;
    let mut last_sweep = Instant::now();

    loop {
        let mut idle = true;

        for subscription in &reports {
            let _ = subscription.poll_fn(
                |buffer, _header| match decode(buffer) {
                    Decoded::TermMarker(term) => marker_term = marker_term.max(term),
                    Decoded::Answer { exec_id, cl_ord_id, outcome } if exec_id > last_exec_id => {
                        last_exec_id = exec_id;
                        if let Some(order) = pending.remove(&cl_ord_id) {
                            let _ = order.answer.send(outcome);
                        }
                    }
                    Decoded::Progress { exec_id } if exec_id > last_exec_id => last_exec_id = exec_id,
                    _ => {}
                },
                256,
            );
        }

        client.poll_egress();
        client.keep_alive();

        if client.leader_changes() != leader_changes {
            leader_changes = client.leader_changes();
            awaiting_term = true;
            warn!("Cluster: Leader changed to {:?}, holding orders", client.leader());
        }

        if awaiting_term && marker_term >= client.leadership_term() && client.is_leader_connected() {
            awaiting_term = false;
            unsent = pending.keys().copied().collect();
            info!("Cluster: Term {} resolved, resending {} orders", marker_term, unsent.len());
        }

        match incoming.try_recv() {
            Ok(order) => {
                idle = false;
                pending.insert(
                    order.cl_ord_id,
                    PendingOrder {
                        message: order.message,
                        answer: order.published,
                        deadline: Instant::now() + REQUEST_DEADLINE,
                    },
                );
                unsent.push_back(order.cl_ord_id);
            }
            Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => {}
        }

        if !awaiting_term {
            while let Some(cl_ord_id) = unsent.front() {
                let Some(order) = pending.get(cl_ord_id) else {
                    unsent.pop_front();
                    continue;
                };
                if client.offer(&order.message) != OfferResult::Sent {
                    break;
                }
                idle = false;
                unsent.pop_front();
            }
        }

        if last_sweep.elapsed() >= Duration::from_millis(50) {
            last_sweep = Instant::now();
            let now = Instant::now();
            let expired: Vec<UuidType> = pending
                .iter()
                .filter(|(_, order)| order.deadline <= now)
                .map(|(id, _)| *id)
                .collect();
            for id in expired {
                if let Some(order) = pending.remove(&id) {
                    let _ = order.answer.send(OrderOutcome::TimedOut);
                }
            }
        }

        if idle {
            thread::sleep(Duration::from_micros(20));
        }
    }
}
