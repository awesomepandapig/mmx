use super::decimal;
use super::store::{RecordedOrder, RecordedStatus};
use super::validation::ValidOrder;

use sbe::WriteBuf;
use sbe::message_header_codec::ENCODED_LENGTH;
use sbe::new_order_single_codec::{NewOrderSingleEncoder, SBE_BLOCK_LENGTH};
use sbe::ord_type_enum::OrdTypeEnum;
use sbe::order_cancel_request_codec::{self, OrderCancelRequestEncoder};
use sbe::side_enum::SideEnum;
use uuid::Uuid;

use chrono::{DateTime, SecondsFormat};

use serde::Serialize;

pub const MESSAGE_SIZE: usize = SBE_BLOCK_LENGTH as usize + ENCODED_LENGTH;
pub const CANCEL_MESSAGE_SIZE: usize =
    order_cancel_request_codec::SBE_BLOCK_LENGTH as usize + ENCODED_LENGTH;

/// What the API returns for an order. Decimals are strings, so a client never has to round them,
/// and a field with no value is left out.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct Order {
    pub id: String,
    pub symbol: String,
    pub side: String,
    pub r#type: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub size: String,
    pub filled_size: String,
    pub executed_value: String,
    pub status: String,
    pub settled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_after: Option<String>,
}

pub fn format_timestamp_ns(timestamp_ns: u64) -> String {
    let secs = (timestamp_ns / 1_000_000_000) as i64;
    let nanos = (timestamp_ns % 1_000_000_000) as u32;

    DateTime::from_timestamp(secs, nanos)
        .unwrap_or(DateTime::UNIX_EPOCH) // Default to epoch if out of range
        .to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub fn uuid_to_u64s(uuid: Uuid) -> UuidType {
    let bytes = uuid.into_bytes(); // [u8; 16]
    let high = u64::from_be_bytes(bytes[0..8].try_into().unwrap());
    let low = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
    [high, low]
}

pub fn u64s_to_uuid(uuid: UuidType) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&uuid[0].to_be_bytes());
    bytes[8..16].copy_from_slice(&uuid[1].to_be_bytes());
    Uuid::from_bytes(bytes)
}

fn side_name(side: SideEnum) -> String {
    match side {
        SideEnum::Buy => "buy".to_string(),
        SideEnum::Sell => "sell".to_string(),
        _ => "unknown".to_string(),
    }
}

impl Order {
    /// An order the engine just accepted.
    pub fn accepted(order: &ValidOrder, timestamp_ns: u64) -> Self {
        Order {
            id: u64s_to_uuid(order.cl_ord_id).to_string(),
            symbol: String::from_utf8_lossy(&order.symbol)
                .trim_end_matches('\0')
                .to_string(),
            side: side_name(order.side),
            r#type: match order.ord_type {
                OrdTypeEnum::Limit => "limit".to_string(),
                OrdTypeEnum::Market => "market".to_string(),
                _ => "unknown".to_string(),
            },
            created_at: format_timestamp_ns(timestamp_ns),
            updated_at: None,
            size: decimal::format(i128::from(order.qty_mantissa)),
            filled_size: "0".to_string(),
            executed_value: "0".to_string(),
            status: "open".to_string(),
            settled: false,
            price: (order.price_mantissa != i64::MIN)
                .then(|| decimal::format(i128::from(order.price_mantissa))),
            cancel_after: None,
        }
    }

    /// An order as the recorder last saw it.
    pub fn recorded(order: RecordedOrder) -> Self {
        let (status, settled) = match order.status {
            RecordedStatus::Open => ("open", false),
            RecordedStatus::Filled => ("filled", true),
            RecordedStatus::Canceled => ("canceled", true),
            RecordedStatus::Rejected => ("rejected", true),
        };
        let filled = order.filled_mantissa;
        Order {
            id: order.id.to_string(),
            symbol: order.symbol,
            side: side_name(order.side),
            // The engine only leaves the price null on market orders.
            r#type: if order.price_mantissa.is_some() {
                "limit"
            } else {
                "market"
            }
            .to_string(),
            created_at: order.created_at,
            updated_at: Some(order.updated_at),
            size: decimal::format(i128::from(order.leaves_mantissa) + i128::from(filled)),
            filled_size: decimal::format(i128::from(filled)),
            executed_value: decimal::format(decimal::notional(
                filled,
                order.average_price_mantissa.unwrap_or(0),
            )),
            status: status.to_string(),
            settled,
            price: order
                .price_mantissa
                .map(|price| decimal::format(i128::from(price))),
            cancel_after: None,
        }
    }
}

pub type UuidType = [u64; 2];
pub type SymbolType = [u8; 6];

pub fn create_order_buffer(order: &ValidOrder, timestamp_ns: u64) -> [u8; MESSAGE_SIZE] {
    let mut buffer = [0u8; MESSAGE_SIZE];
    let write_buf = WriteBuf::new(&mut buffer[..]);

    let mut order_encoder = NewOrderSingleEncoder::default().wrap(write_buf, ENCODED_LENGTH);

    let mut header_composite_encoder = order_encoder.header(0);
    order_encoder = header_composite_encoder
        .parent()
        .expect("Failed to retrieve parent encoder after SBE header encoding");

    order_encoder.cl_ord_id(&order.cl_ord_id);
    order_encoder.account(&order.account);
    order_encoder.symbol(&order.symbol);
    order_encoder.side(order.side);

    let mut transact_time_encoder = order_encoder.transact_time_encoder();
    transact_time_encoder.time(timestamp_ns);
    order_encoder = transact_time_encoder
        .parent()
        .expect("Failed to retrieve parent encoder after transact_time encoding");

    order_encoder.ord_type(order.ord_type);

    let mut order_qty_encoder = order_encoder.order_qty_encoder();
    order_qty_encoder.mantissa(order.qty_mantissa);
    order_encoder = order_qty_encoder
        .parent()
        .expect("Failed to retrieve parent encoder after order_qty encoding");

    let mut price_encoder_composite = order_encoder.price_encoder();
    price_encoder_composite.mantissa(order.price_mantissa);
    price_encoder_composite
        .parent()
        .expect("Failed to retrieve parent encoder after price encoding");

    buffer
}

/// The engine finds an order by account and client order ID alone. The symbol and side are left
/// empty, so cancelling doesn't depend on knowing them.
pub fn create_cancel_buffer(
    orig_cl_ord_id: &UuidType,
    cl_ord_id: &UuidType,
    account: &UuidType,
    timestamp_ns: u64,
) -> [u8; CANCEL_MESSAGE_SIZE] {
    let mut buffer = [0u8; CANCEL_MESSAGE_SIZE];
    let write_buf = WriteBuf::new(&mut buffer[..]);

    let mut encoder = OrderCancelRequestEncoder::default().wrap(write_buf, ENCODED_LENGTH);
    let mut header = encoder.header(0);
    encoder = header
        .parent()
        .expect("Failed to retrieve parent encoder after SBE header encoding");

    encoder.orig_cl_ord_id(orig_cl_ord_id);
    encoder.cl_ord_id(cl_ord_id);
    encoder.account(account);
    encoder.symbol(&[0; 6]);
    encoder.side(SideEnum::NullVal);

    let mut transact_time_encoder = encoder.transact_time_encoder();
    transact_time_encoder.time(timestamp_ns);
    transact_time_encoder
        .parent()
        .expect("Failed to retrieve parent encoder after transact_time encoding");

    buffer
}
