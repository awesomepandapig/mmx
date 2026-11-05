//! The contract with the game: what a webhook says and how it's signed.
//!
//! An event is a pure function of its journal record, so every send of an event carries the same
//! fields and the same `fill_id`, however batches fall, whichever replica sends it, and even if the
//! journal is rebuilt from the tape.

use crate::frame::{self, Execution, ExecType, Frame, Id, OrdStatus, Side};
use crate::journal::Record;
use crate::ledger::REPEAT_OF_OPEN_ORDER;

use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const TIMESTAMP_HEADER: &str = "X-MMX-Timestamp";
pub const SIGNATURE_HEADER: &str = "X-MMX-Signature";

const SCALE: u64 = 100_000_000;

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Fill,
    Canceled,
    Rejected,
    CancelRejected,
}

/// Quantities and prices are decimal strings, so they survive a JSON parser that uses doubles.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub fill_id: String,
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub tape_position: i64,
    pub exec_id: u64,
    /// True when this event closes the escrow held for `client_order_id`.
    pub terminal: bool,
    /// How much of the order's quantity to release from escrow.
    pub release_qty: String,
    pub client_order_id: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orig_client_order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<Side>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<OrdStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_qty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filled_qty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leaves_qty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_qty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_px: Option<String>,
    /// Nanoseconds since the Unix epoch, as a string because they don't fit in a double.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transact_time: Option<String>,
}

#[derive(Serialize)]
pub struct Delivery<'a> {
    pub events: &'a [Event],
}

/// The dedupe key. Hashing the frame, and not just its IDs, means that if a total loss ever reuses
/// an exec ID for something else, the new event can't be mistaken for one already applied. The
/// position separates cancel rejects, which can be byte-identical to each other.
pub fn fill_id(position: i64, frame: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(position.to_le_bytes());
    hash.update(frame);
    hex(&hash.finalize()[..16])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn uuid(id: Id) -> String {
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&id[0].to_be_bytes());
    bytes[8..16].copy_from_slice(&id[1].to_be_bytes());
    Uuid::from_bytes(bytes).to_string()
}

/// Formats a mantissa as a decimal, trimming trailing zeros.
pub fn decimal(mantissa: i64) -> String {
    let sign = if mantissa < 0 { "-" } else { "" };
    let value = mantissa.unsigned_abs();
    let fraction = value % SCALE;
    if fraction == 0 {
        return format!("{sign}{}", value / SCALE);
    }
    let fraction = format!("{fraction:08}");
    format!("{sign}{}.{}", value / SCALE, fraction.trim_end_matches('0'))
}

fn execution_event(record: &Record, report: &Execution) -> Option<Event> {
    let repeat = record.flags & REPEAT_OF_OPEN_ORDER != 0;
    let event_type = match report.exec_type {
        ExecType::Trade => EventType::Fill,
        ExecType::Canceled => EventType::Canceled,
        ExecType::Rejected => EventType::Rejected,
        ExecType::New | ExecType::Unknown => return None,
    };

    // Taken from the fills, so it holds whatever a report says about leaves on a dead order.
    let unfilled = report.ord_qty - report.cum_qty;
    let closes_with_release = matches!(event_type, EventType::Canceled | EventType::Rejected) && !repeat;

    Some(Event {
        fill_id: fill_id(record.position, &record.frame),
        event_type,
        tape_position: record.position,
        exec_id: record.exec_id,
        terminal: report.ord_status.is_terminal() && !repeat,
        release_qty: decimal(if closes_with_release { unfilled } else { 0 }),
        client_order_id: uuid(report.cl_ord_id),
        order_id: Some(report.order_id),
        orig_client_order_id: None,
        account: Some(uuid(report.account)),
        symbol: Some(report.symbol.clone()),
        side: Some(report.side),
        status: Some(report.ord_status),
        reason: report.reject_reason.filter(|_| event_type == EventType::Rejected).map(|reason| reason.as_str()),
        order_qty: Some(decimal(report.ord_qty)),
        filled_qty: Some(decimal(report.cum_qty)),
        leaves_qty: Some(decimal(report.leaves_qty)),
        price: report.price.map(decimal),
        match_id: report.match_id,
        last_qty: report.last_qty.map(decimal),
        last_px: report.last_px.map(decimal),
        avg_px: report.avg_px.map(decimal),
        transact_time: Some(report.transact_time.to_string()),
    })
}

/// Builds the event for a record, or `None` for records that the game doesn't hear about.
pub fn event(record: &Record) -> Option<Event> {
    match frame::decode(&record.frame) {
        Frame::Execution(report) => execution_event(record, &report),
        Frame::CancelReject { cl_ord_id, orig_cl_ord_id } => Some(Event {
            fill_id: fill_id(record.position, &record.frame),
            event_type: EventType::CancelRejected,
            tape_position: record.position,
            exec_id: record.exec_id,
            terminal: false,
            release_qty: decimal(0),
            client_order_id: uuid(cl_ord_id),
            order_id: None,
            orig_client_order_id: Some(uuid(orig_cl_ord_id)),
            account: None,
            symbol: None,
            side: None,
            status: None,
            reason: Some("unknown_order"),
            order_qty: None,
            filled_qty: None,
            leaves_qty: None,
            price: None,
            match_id: None,
            last_qty: None,
            last_px: None,
            avg_px: None,
            transact_time: None,
        }),
        Frame::TermMarker | Frame::Unknown(_) => None,
    }
}

/// HMAC-SHA256 as lowercase hex.
pub fn hmac_hex(secret: &[u8], message: &[&[u8]]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes a key of any length");
    for part in message {
        mac.update(part);
    }
    hex(&mac.finalize().into_bytes())
}

/// The value of the signature header. The timestamp is signed with the exact body bytes, and it's
/// fresh on every attempt, so a game can refuse old replays without refusing long retries.
pub fn signature(secret: &[u8], timestamp: u64, body: &[u8]) -> String {
    format!("v1={}", hmac_hex(secret, &[timestamp.to_string().as_bytes(), b".", body]))
}
