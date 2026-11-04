use super::order::MESSAGE_SIZE;

use std::hint::spin_loop;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use log::{error, info};

use tokio::sync::{mpsc, oneshot};

use transport::{AeronExclusivePublication, AeronOfferError};

const ORDER_QUEUE_LENGTH: usize = 1024;
const BACK_PRESSURE_TIMEOUT: Duration = Duration::from_millis(100);

pub struct OutboundOrder {
    pub message: [u8; MESSAGE_SIZE],
    pub published: oneshot::Sender<Result<(), String>>,
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
pub fn spawn_order_publisher(channel: &str, stream_id: i32) -> Result<OrderSender, String> {
    let channel = channel.to_string();
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

            while let Some(order) = pending.blocking_recv() {
                let _ = order.published.send(offer(&publication, &order.message));
            }
        })
        .map_err(|err| err.to_string())?;

    startup.recv().map_err(|err| err.to_string())??;
    Ok(orders)
}
