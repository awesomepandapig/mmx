use super::order::UuidType;

use std::hint::spin_loop;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use log::{error, info};

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use transport::{AeronExclusivePublication, AeronOfferError};

/// How long a request may wait to go out and be answered. A cluster gateway stops sending at this
/// deadline, which is what makes the time an order can land bounded.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(3);

const ORDER_QUEUE_LENGTH: usize = 1024;
const BACK_PRESSURE_TIMEOUT: Duration = Duration::from_millis(100);
const LINK_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// Whether the thread that owns the Aeron handles can reach the engine. Handles can't leave that
/// thread, so it publishes the answer here for `/readyz`.
#[derive(Default)]
pub struct EngineLink(AtomicBool);

impl EngineLink {
    pub fn set(&self, connected: bool) {
        self.0.store(connected, Ordering::Relaxed);
    }

    pub fn is_up(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// What happened to a request, once it's certain.
#[derive(Debug, PartialEq, Eq)]
pub enum OrderOutcome {
    /// The engine took the order, or, without a cluster, the order was published.
    Accepted,
    /// The engine refused the order, so it won't execute.
    Rejected,
    Canceled,
    /// There's nothing to cancel, because the order finished or never existed.
    NotOpen,
    /// No answer before the deadline. The request may still have landed, so it must not be sent
    /// again. Its status says what became of it.
    TimedOut,
    Failed(String),
}

/// An order is known to the engine by its account and client order ID.
pub type OrderKey = (UuidType, UuidType);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EngineRequest {
    New(OrderKey),
    /// `cancel_id` is this attempt's own ID, which the engine echoes in a reject.
    Cancel { order: OrderKey, cancel_id: UuidType },
}

pub struct OutboundOrder {
    pub request: EngineRequest,
    pub message: Vec<u8>,
    /// Set before any wait, so that nothing this request queues behind can extend it.
    pub send_by: Instant,
    pub published: oneshot::Sender<OrderOutcome>,
}

pub type OrderSender = mpsc::Sender<OutboundOrder>;

fn error_handler(code: i32, message: &str) {
    error!("Aeron: Error {}: {}", code, message);
}

fn offer(publication: &AeronExclusivePublication, message: &[u8]) -> Result<(), String> {
    let start = Instant::now();

    loop {
        match publication.offer(message) {
            Ok(_) => return Ok(()),
            Err(AeronOfferError::BackPressured | AeronOfferError::AdminAction)
                if start.elapsed() < BACK_PRESSURE_TIMEOUT =>
            {
                spin_loop();
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

/// Aeron handles can't leave the thread that created them, so one thread owns the client and
/// request handlers queue their orders to it.
pub fn spawn_order_publisher(
    channel: &str,
    stream_id: i32,
    link: Arc<EngineLink>,
) -> Result<OrderSender, String> {
    let channel = channel.to_string();
    let runtime = Handle::current();
    let (orders, mut pending) = mpsc::channel::<OutboundOrder>(ORDER_QUEUE_LENGTH);
    let (started, startup) = std_mpsc::channel();

    thread::Builder::new()
        .name("order-publisher".to_string())
        .spawn(move || {
            let client = transport::connect(error_handler).and_then(|aeron| {
                let publication = transport::exclusive_publication(&aeron, &channel, stream_id)?;
                Ok((aeron, publication))
            });

            let (_aeron, publication) = match client {
                Ok(client) => {
                    let _ = started.send(Ok(()));
                    client
                }
                Err(err) => {
                    let _ = started.send(Err(format!("{err:?}")));
                    return;
                }
            };
            info!("Aeron: Created publication {} on stream {}", channel, stream_id);

            loop {
                link.set(publication.is_connected());
                // Waking up now and then keeps the link current while no orders arrive.
                let order = match runtime.block_on(timeout(LINK_CHECK_INTERVAL, pending.recv())) {
                    Ok(Some(order)) => order,
                    Ok(None) => return,
                    Err(_) => continue,
                };
                let outcome = match offer(&publication, &order.message) {
                    Ok(()) => OrderOutcome::Accepted,
                    Err(err) => OrderOutcome::Failed(err),
                };
                let _ = order.published.send(outcome);
            }
        })
        .map_err(|err| err.to_string())?;

    startup.recv().map_err(|err| err.to_string())??;
    Ok(orders)
}
