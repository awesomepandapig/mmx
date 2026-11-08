//! The engine's snapshot format. It's written by scanning the order pool rather than walking each
//! queue, because the walk chases a pointer per order and looks up every level. At 1M resting
//! orders that took about 100ms of the pause, and the scan takes about 2ms.
//!
//! The scan loses queue order, so loading sorts by order ID, which is the same order: an order
//! joins its queue once and never moves.

use crate::handler::Handler;
use crate::publisher::OutputState;
use crate::types::{self, ORDER_RECORD, Order};

const HEADER_CHUNK: u8 = 1;
const ORDER_CHUNK: u8 = 2;
const HEADER_LENGTH: usize = 72;
const CHUNK_LENGTH: usize = 60_000;

/// Writes the full state in chunks small enough for one Aeron message.
pub fn write(handler: &Handler, output: &OutputState, write: &mut dyn FnMut(&[u8])) {
    let mut header = [0u8; HEADER_LENGTH];
    header[0] = HEADER_CHUNK;
    let values = [
        handler.counter_order_id,
        handler.counter_exec_id,
        handler.counter_match_id,
        output.written,
        output.digest,
        output.position as u64,
        output.quiet_position as u64,
        output.last_marker_written,
    ];
    for (i, value) in values.iter().enumerate() {
        header[8 + i * 8..16 + i * 8].copy_from_slice(&value.to_le_bytes());
    }
    write(&header);

    let mut chunk = [0u8; CHUNK_LENGTH];
    chunk[0] = ORDER_CHUNK;
    let mut length = 1;
    for order in handler.book.resting() {
        let record: &mut [u8; ORDER_RECORD] = (&mut chunk[length..length + ORDER_RECORD])
            .try_into()
            .expect("a record's length");
        types::encode_order(order, record);
        length += ORDER_RECORD;
        if length + ORDER_RECORD > CHUNK_LENGTH {
            write(&chunk[..length]);
            length = 1;
        }
    }
    if length > 1 {
        write(&chunk[..length]);
    }
}

/// Gathers a snapshot's orders, because the book can only be rebuilt once they're all here.
#[derive(Default)]
pub struct Loader {
    orders: Vec<Order>,
}

impl Loader {
    pub fn on_chunk(&mut self, handler: &mut Handler, chunk: &[u8]) {
        match chunk[0] {
            HEADER_CHUNK => {
                let value = |i: usize| {
                    u64::from_le_bytes(
                        chunk[8 + i * 8..16 + i * 8]
                            .try_into()
                            .expect("eight bytes"),
                    )
                };
                handler.counter_order_id = value(0);
                handler.counter_exec_id = value(1);
                handler.counter_match_id = value(2);
                handler.publisher.restore(OutputState {
                    written: value(3),
                    digest: value(4),
                    position: value(5) as i64,
                    quiet_position: value(6) as i64,
                    last_marker_written: value(7),
                });
            }
            _ => {
                let (records, _) = chunk[1..].as_chunks::<ORDER_RECORD>();
                self.orders
                    .extend(records.iter().map(|record| types::decode_order(record)));
            }
        }
    }

    /// Restores every queue in its original order.
    pub fn finish(mut self, handler: &mut Handler) {
        self.orders
            .sort_unstable_by_key(|order| order.sequence_number);
        for order in &self.orders {
            handler.book.add(order);
        }
    }
}
