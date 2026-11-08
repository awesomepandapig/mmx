use sbe::ReadBuf;
use sbe::exec_type_enum::ExecTypeEnum;
use sbe::execution_report_codec::ExecutionReportDecoder;
use sbe::message_header_codec::MessageHeaderDecoder;
use sbe::new_order_single_codec::NewOrderSingleDecoder;
use sbe::ord_status_enum::OrdStatusEnum;
use sbe::order_cancel_reject_codec::OrderCancelRejectDecoder;
use sbe::order_cancel_request_codec::OrderCancelRequestDecoder;
use sbe::side_enum::SideEnum;

use serde::{Deserialize, Serialize};

pub const SCALE: i64 = 100_000_000;
const NULL_DECIMAL: i64 = i64::MIN;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecType {
    New,
    Trade,
    Canceled,
    Rejected,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrdStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
    Unknown,
}

/// Prices and quantities are raw mantissas with exponent -8, as on the wire.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    pub exec_id: u64,
    pub order_id: u64,
    pub exec_type: ExecType,
    pub ord_status: OrdStatus,
    pub side: Side,
    pub symbol: String,
    pub account: String,
    pub cl_ord_id: String,
    pub transact_time: u64,
    pub price: Option<i64>,
    pub ord_qty: i64,
    pub leaves_qty: i64,
    pub cum_qty: i64,
    pub last_qty: Option<i64>,
    pub last_px: Option<i64>,
    pub match_id: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    NewOrder {
        cl_ord_id: String,
        account: String,
        symbol: String,
        side: Side,
        price: Option<i64>,
        qty: i64,
    },
    CancelRequest {
        cl_ord_id: String,
        orig_cl_ord_id: String,
        account: String,
    },
    Execution(Execution),
    CancelReject {
        cl_ord_id: String,
        orig_cl_ord_id: String,
    },
    Unknown {
        template_id: u16,
    },
}

fn id(value: [u64; 2]) -> String {
    format!("{:016x}{:016x}", value[0], value[1])
}

fn symbol(value: [u8; 6]) -> String {
    String::from_utf8_lossy(&value)
        .trim_end_matches('\0')
        .to_string()
}

fn decimal(value: i64) -> Option<i64> {
    (value != NULL_DECIMAL).then_some(value)
}

fn side(value: SideEnum) -> Side {
    match value {
        SideEnum::Buy => Side::Buy,
        SideEnum::Sell => Side::Sell,
        _ => Side::Unknown,
    }
}

fn exec_type(value: ExecTypeEnum) -> ExecType {
    match value {
        ExecTypeEnum::New => ExecType::New,
        ExecTypeEnum::Trade => ExecType::Trade,
        ExecTypeEnum::Canceled => ExecType::Canceled,
        ExecTypeEnum::Rejected => ExecType::Rejected,
        _ => ExecType::Unknown,
    }
}

fn ord_status(value: OrdStatusEnum) -> OrdStatus {
    match value {
        OrdStatusEnum::New => OrdStatus::New,
        OrdStatusEnum::PartiallyFilled => OrdStatus::PartiallyFilled,
        OrdStatusEnum::Filled => OrdStatus::Filled,
        OrdStatusEnum::Canceled => OrdStatus::Canceled,
        OrdStatusEnum::Rejected => OrdStatus::Rejected,
        _ => OrdStatus::Unknown,
    }
}

pub fn decode(buffer: &[u8]) -> Event {
    let header: MessageHeaderDecoder<ReadBuf<'_>> =
        MessageHeaderDecoder::default().wrap(ReadBuf::new(buffer), 0);

    match header.template_id() {
        1 => {
            let order: NewOrderSingleDecoder<'_> =
                NewOrderSingleDecoder::default().header(header, 0);
            Event::NewOrder {
                cl_ord_id: id(order.cl_ord_id()),
                account: id(order.account()),
                symbol: symbol(order.symbol()),
                side: side(order.side()),
                price: decimal(order.price_decoder().mantissa()),
                qty: order.order_qty_decoder().mantissa(),
            }
        }
        2 => {
            let cancel: OrderCancelRequestDecoder<'_> =
                OrderCancelRequestDecoder::default().header(header, 0);
            Event::CancelRequest {
                cl_ord_id: id(cancel.cl_ord_id()),
                orig_cl_ord_id: id(cancel.orig_cl_ord_id()),
                account: id(cancel.account()),
            }
        }
        3 => {
            let report: ExecutionReportDecoder<'_> =
                ExecutionReportDecoder::default().header(header, 0);
            Event::Execution(Execution {
                exec_id: report.exec_id(),
                order_id: report.order_id(),
                exec_type: exec_type(report.exec_type()),
                ord_status: ord_status(report.ord_status()),
                side: side(report.side()),
                symbol: symbol(report.symbol()),
                account: id(report.account()),
                cl_ord_id: id(report.cl_ord_id()),
                transact_time: report.transact_time_decoder().time(),
                price: decimal(report.price_decoder().mantissa()),
                ord_qty: report.order_qty_decoder().mantissa(),
                leaves_qty: report.leaves_qty_decoder().mantissa(),
                cum_qty: report.cum_qty_decoder().mantissa(),
                last_qty: decimal(report.last_qty_decoder().mantissa()),
                last_px: decimal(report.last_px_decoder().mantissa()),
                match_id: report.trd_match_id(),
            })
        }
        4 => {
            let reject: OrderCancelRejectDecoder<'_> =
                OrderCancelRejectDecoder::default().header(header, 0);
            Event::CancelReject {
                cl_ord_id: id(reject.cl_ord_id()),
                orig_cl_ord_id: id(reject.orig_cl_ord_id()),
            }
        }
        template_id => Event::Unknown { template_id },
    }
}

/// Formats a mantissa as a decimal, trimming trailing zeros.
pub fn format_decimal(mantissa: i64) -> String {
    let sign = if mantissa < 0 { "-" } else { "" };
    let value = mantissa.unsigned_abs();
    let scale = SCALE as u64;
    let fraction = value % scale;
    if fraction == 0 {
        return format!("{sign}{}", value / scale);
    }
    let fraction = format!("{fraction:08}");
    format!("{sign}{}.{}", value / scale, fraction.trim_end_matches('0'))
}
