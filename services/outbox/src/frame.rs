//! Decodes one frame of the tape.
//!
//! This follows `tools/src/event.rs`, but keeps the reject reason and ID types that the webhook
//! contract needs, and treats a short frame as unknown rather than reading past its end.

use sbe::ReadBuf;
use sbe::exec_type_enum::ExecTypeEnum;
use sbe::execution_report_codec::{self, ExecutionReportDecoder};
use sbe::message_header_codec::{ENCODED_LENGTH, MessageHeaderDecoder};
use sbe::ord_rej_reason_enum::OrdRejReasonEnum;
use sbe::ord_status_enum::OrdStatusEnum;
use sbe::order_cancel_reject_codec::{self, OrderCancelRejectDecoder};
use sbe::side_enum::SideEnum;

use serde::Serialize;

pub const TERM_MARKER_TEMPLATE: u16 = 100;
const NULL_DECIMAL: i64 = i64::MIN;

pub type Id = [u64; 2];

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecType {
    New,
    Trade,
    Canceled,
    Rejected,
    Unknown,
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrdStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
    Unknown,
}

impl OrdStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Filled | Self::Canceled | Self::Rejected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    UnknownOrder,
    DuplicateOrder,
    StaleOrder,
    Other,
}

impl RejectReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownOrder => "unknown_order",
            Self::DuplicateOrder => "duplicate_order",
            Self::StaleOrder => "stale_order",
            Self::Other => "other",
        }
    }
}

/// Prices and quantities are raw mantissas with exponent -8, as on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    pub exec_id: u64,
    pub order_id: u64,
    pub exec_type: ExecType,
    pub ord_status: OrdStatus,
    pub reject_reason: Option<RejectReason>,
    pub side: Side,
    pub symbol: String,
    pub account: Id,
    pub cl_ord_id: Id,
    pub transact_time: u64,
    pub price: Option<i64>,
    pub ord_qty: i64,
    pub leaves_qty: i64,
    pub cum_qty: i64,
    pub last_qty: Option<i64>,
    pub last_px: Option<i64>,
    pub avg_px: Option<i64>,
    pub match_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    TermMarker,
    Execution(Box<Execution>),
    /// Spends an exec ID but doesn't carry it, and names no account.
    CancelReject {
        cl_ord_id: Id,
        orig_cl_ord_id: Id,
    },
    Unknown(u16),
}

fn decimal(value: i64) -> Option<i64> {
    (value != NULL_DECIMAL).then_some(value)
}

fn side(value: SideEnum) -> Side {
    match value {
        SideEnum::Buy => Side::Buy,
        SideEnum::Sell => Side::Sell,
        SideEnum::NullVal => Side::Unknown,
    }
}

fn exec_type(value: ExecTypeEnum) -> ExecType {
    match value {
        ExecTypeEnum::New => ExecType::New,
        ExecTypeEnum::Trade => ExecType::Trade,
        ExecTypeEnum::Canceled => ExecType::Canceled,
        ExecTypeEnum::Rejected => ExecType::Rejected,
        ExecTypeEnum::NullVal => ExecType::Unknown,
    }
}

fn ord_status(value: OrdStatusEnum) -> OrdStatus {
    match value {
        OrdStatusEnum::New => OrdStatus::New,
        OrdStatusEnum::PartiallyFilled => OrdStatus::PartiallyFilled,
        OrdStatusEnum::Filled => OrdStatus::Filled,
        OrdStatusEnum::Canceled => OrdStatus::Canceled,
        OrdStatusEnum::Rejected => OrdStatus::Rejected,
        OrdStatusEnum::NullVal => OrdStatus::Unknown,
    }
}

fn reject_reason(value: OrdRejReasonEnum) -> Option<RejectReason> {
    match value {
        OrdRejReasonEnum::UnknownOrder => Some(RejectReason::UnknownOrder),
        OrdRejReasonEnum::DuplicateOrder => Some(RejectReason::DuplicateOrder),
        OrdRejReasonEnum::StaleOrder => Some(RejectReason::StaleOrder),
        OrdRejReasonEnum::Other => Some(RejectReason::Other),
        OrdRejReasonEnum::NullVal => None,
    }
}

pub fn template_id(buffer: &[u8]) -> Option<u16> {
    (buffer.len() >= ENCODED_LENGTH).then(|| u16::from_le_bytes([buffer[2], buffer[3]]))
}

pub fn decode(buffer: &[u8]) -> Frame {
    let Some(template_id) = template_id(buffer) else {
        return Frame::Unknown(0);
    };
    let header: MessageHeaderDecoder<ReadBuf<'_>> =
        MessageHeaderDecoder::default().wrap(ReadBuf::new(buffer), 0);

    match template_id {
        TERM_MARKER_TEMPLATE => Frame::TermMarker,
        execution_report_codec::SBE_TEMPLATE_ID
            if buffer.len()
                >= ENCODED_LENGTH + execution_report_codec::SBE_BLOCK_LENGTH as usize =>
        {
            let report: ExecutionReportDecoder<'_> =
                ExecutionReportDecoder::default().header(header, 0);
            Frame::Execution(Box::new(Execution {
                exec_id: report.exec_id(),
                order_id: report.order_id(),
                exec_type: exec_type(report.exec_type()),
                ord_status: ord_status(report.ord_status()),
                reject_reason: reject_reason(report.ord_rej_reason()),
                side: side(report.side()),
                symbol: String::from_utf8_lossy(&report.symbol())
                    .trim_end_matches('\0')
                    .to_string(),
                account: report.account(),
                cl_ord_id: report.cl_ord_id(),
                transact_time: report.transact_time_decoder().time(),
                price: decimal(report.price_decoder().mantissa()),
                ord_qty: report.order_qty_decoder().mantissa(),
                leaves_qty: report.leaves_qty_decoder().mantissa(),
                cum_qty: report.cum_qty_decoder().mantissa(),
                last_qty: decimal(report.last_qty_decoder().mantissa()),
                last_px: decimal(report.last_px_decoder().mantissa()),
                avg_px: decimal(report.avg_px_decoder().mantissa()),
                match_id: report.trd_match_id(),
            }))
        }
        order_cancel_reject_codec::SBE_TEMPLATE_ID
            if buffer.len()
                >= ENCODED_LENGTH + order_cancel_reject_codec::SBE_BLOCK_LENGTH as usize =>
        {
            let reject: OrderCancelRejectDecoder<'_> =
                OrderCancelRejectDecoder::default().header(header, 0);
            Frame::CancelReject {
                cl_ord_id: reject.cl_ord_id(),
                orig_cl_ord_id: reject.orig_cl_ord_id(),
            }
        }
        template_id => Frame::Unknown(template_id),
    }
}
