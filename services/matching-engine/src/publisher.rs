use crate::types::CancelRequest;
use crate::types::Order;

use std::hint::spin_loop;
use std::time::{SystemTime};

use transport::{AeronExclusivePublication, AeronOfferError};

use sbe::WriteBuf;
use sbe::cxl_rej_reason_enum::CxlRejReasonEnum;
use sbe::cxl_rej_response_to_enum::CxlRejResponseToEnum;
use sbe::exec_type_enum::ExecTypeEnum;
use sbe::execution_report_codec::{ExecutionReportEncoder, SBE_BLOCK_LENGTH};
use sbe::message_header_codec::ENCODED_LENGTH;
use sbe::ord_rej_reason_enum::OrdRejReasonEnum;
use sbe::ord_status_enum::OrdStatusEnum;
use sbe::ord_type_enum::OrdTypeEnum;
use sbe::order_cancel_reject_codec::OrderCancelRejectEncoder;

use tracing::error;

const MAX_MESSAGE_SIZE: usize = SBE_BLOCK_LENGTH as usize + ENCODED_LENGTH;

const FRAME_HEADER_LENGTH: usize = 32;
const FRAME_ALIGNMENT: usize = 32;

/// The tape's frames all have this length, because every message is a whole ring slot.
pub const FRAME_LENGTH: i64 =
    ((FRAME_HEADER_LENGTH + MAX_MESSAGE_SIZE).next_multiple_of(FRAME_ALIGNMENT)) as i64;

/// Must match the `term-length` in `PUB_CHANNEL`.
pub const TERM_LENGTH: i64 = 64 * 1024 * 1024;
const FRAMES_PER_TERM: i64 = TERM_LENGTH / FRAME_LENGTH;

/// The stream position `frames` frames after `position`. A frame that doesn't fit in a term goes
/// at the start of the next one, and Aeron pads the gap.
pub const fn position_after(position: i64, frames: u64) -> i64 {
    let frames = frames as i64;
    let offset = position % TERM_LENGTH;
    let room = (TERM_LENGTH - offset) / FRAME_LENGTH;
    if frames <= room {
        return position + frames * FRAME_LENGTH;
    }
    let beyond = frames - room;
    let whole_terms = (beyond - 1) / FRAMES_PER_TERM;
    let last_term = beyond - whole_terms * FRAMES_PER_TERM;
    position - offset + TERM_LENGTH + whole_terms * TERM_LENGTH + last_term * FRAME_LENGTH
}

/// How many frames lie between two positions on the tape.
pub const fn frames_between(from: i64, to: i64) -> u64 {
    let (from_term, to_term) = (from / TERM_LENGTH, to / TERM_LENGTH);
    let (from_offset, to_offset) = (from % TERM_LENGTH, to % TERM_LENGTH);
    let frames = if from_term == to_term {
        (to_offset - from_offset) / FRAME_LENGTH
    } else {
        (TERM_LENGTH - from_offset) / FRAME_LENGTH
            + (to_term - from_term - 1) * FRAMES_PER_TERM
            + to_offset / FRAME_LENGTH
    };
    frames as u64
}

/// Everything about the output stream that a snapshot has to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputState {
    pub written: u64,
    pub digest: u64,
    pub position: i64,
    pub quiet_position: i64,
    pub last_marker_written: u64,
}

/// What a recovery marker reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryEvent {
    Rebuilt = 1,
    CanceledAll = 2,
}

#[derive(Clone, Copy)]
pub struct Trade {
    pub match_id: u64,
    pub qty: i64,
    pub px: i64,
}

#[derive(Clone, Copy)]
pub struct Reject {
    pub reason: OrdRejReasonEnum,
}

#[derive(Clone, Copy)]
pub enum ExecutionReport {
    New,
    Trade(Trade),
    Cancel,
    Reject(Reject),
}

impl ExecutionReport {
    #[inline(always)]
    pub const fn exec_type(&self) -> ExecTypeEnum {
        match self {
            Self::New => ExecTypeEnum::New,
            Self::Trade(_) => ExecTypeEnum::Trade,
            Self::Cancel => ExecTypeEnum::Canceled,
            Self::Reject(_) => ExecTypeEnum::Rejected,
        }
    }

    #[inline(always)]
    pub const fn ord_status(&self, order: &Order) -> OrdStatusEnum {
        match self {
            Self::New => OrdStatusEnum::New,
            Self::Trade(_) => {
                if order.leaves_quantity == 0 {
                    OrdStatusEnum::Filled
                } else {
                    OrdStatusEnum::PartiallyFilled
                }
            }
            Self::Cancel => OrdStatusEnum::Canceled,
            Self::Reject(_) => OrdStatusEnum::Rejected,
        }
    }

    #[inline(always)]
    pub const fn ord_rej_reason(&self) -> OrdRejReasonEnum {
        match self {
            Self::Reject(r) => r.reason,
            _ => OrdRejReasonEnum::NullVal,
        }
    }

    #[allow(clippy::single_char_lifetime_names)]
    #[inline(always)]
    fn set_optional_fields<'a>(
        self,
        mut encoder: ExecutionReportEncoder<'a>,
        order: &Order,
    ) -> ExecutionReportEncoder<'a> {
        if let Self::Trade(trade) = self {
            encoder.trd_match_id(trade.match_id);
            let encoder = Self::set_last_px(encoder, trade.px);
            let encoder = Self::set_last_qty(encoder, trade.qty);
            Self::set_avg_px(encoder, order.avg_px())
        } else {
            encoder.trd_match_id(u64::MAX);
            let encoder = Self::set_last_px(encoder, i64::MIN);
            let encoder = Self::set_last_qty(encoder, i64::MIN);
            Self::set_avg_px(encoder, i64::MIN)
        }
    }

    #[inline(always)]
    fn set_last_qty(encoder: ExecutionReportEncoder<'_>, qty: i64) -> ExecutionReportEncoder<'_> {
        let mut last_qty_encoder = encoder.last_qty_encoder();
        last_qty_encoder.mantissa(qty);
        last_qty_encoder
            .parent()
            .expect("Failed to get parent after last_qty") // TODO NO EXPECT
    }

    #[inline(always)]
    fn set_last_px(encoder: ExecutionReportEncoder<'_>, px: i64) -> ExecutionReportEncoder<'_> {
        let mut last_px_encoder = encoder.last_px_encoder();
        last_px_encoder.mantissa(px);
        last_px_encoder
            .parent()
            .expect("Failed to get parent after last_px") // TODO NO EXPECT
    }

    #[inline(always)]
    fn set_avg_px(encoder: ExecutionReportEncoder<'_>, avg_px: i64) -> ExecutionReportEncoder<'_> {
        let mut avg_px_encoder = encoder.avg_px_encoder();
        avg_px_encoder.mantissa(avg_px);
        avg_px_encoder
            .parent()
            .expect("Failed to get parent after avg_px") // TODO NO EXPECT
    }
}

pub struct Publisher {
    publication: Option<AeronExclusivePublication>,
    transact_time: u64,
    /// Decouples matching from output back pressure. Every node publishes and records the same
    /// bytes, so a stream position means the same thing on any node.
    ring: Vec<[u8; MAX_MESSAGE_SIZE]>,
    written: u64,
    sent: u64,
    leading: bool,
    /// `written` and the stream position at the last known point, which give the stream position
    /// of any later report without asking Aeron. A held node has no publication to ask.
    base_written: u64,
    base_position: i64,
    /// Where the current run of term markers began, valid if `last_marker_written` is `written`.
    quiet_position: i64,
    last_marker_written: u64,
    clustered: bool,
    #[cfg(feature = "diagnostics")]
    digesting: bool,
    #[cfg(feature = "diagnostics")]
    digest: u64,
}

#[cfg(not(test))]
pub const RING_LENGTH: usize = 1 << 20;
#[cfg(test)]
pub const RING_LENGTH: usize = 1 << 14;

pub const OUTPUT_SESSION_ID: i32 = 1002;
pub const TERM_MARKER_TEMPLATE: u16 = 100;
pub const RECOVERY_MARKER_TEMPLATE: u16 = 101;
pub const OUTPUT_STREAM_ID: i32 = 1002;

impl Publisher {
    pub fn new(publication: Option<AeronExclusivePublication>, clustered: bool) -> Self {
        Self {
            publication,
            transact_time: 0,
            ring: vec![[0; MAX_MESSAGE_SIZE]; RING_LENGTH],
            written: 0,
            sent: 0,
            leading: !clustered,
            base_written: 0,
            base_position: 0,
            quiet_position: 0,
            last_marker_written: u64::MAX,
            clustered,
            #[cfg(feature = "diagnostics")]
            digesting: std::env::var("DIGEST").is_ok(),
            #[cfg(feature = "diagnostics")]
            digest: 0xcbf2_9ce4_8422_2325,
        }
    }

    pub fn set_cluster_time(&mut self, time: u64) {
        self.transact_time = time;
    }

    /// Starts publishing, beginning `unsent` reports back from the newest.
    pub fn open_output(&mut self, publication: AeronExclusivePublication, unsent: u64) {
        self.publication = Some(publication);
        self.sent = self.written - unsent;
        self.leading = true;
    }

    /// Where the next report will land on the stream.
    pub fn output_position(&self) -> i64 {
        position_after(self.base_position, self.written - self.base_written)
    }

    /// The range of positions separated only by term markers, as (start, current). Markers leave
    /// the book alone, so the book at the start of the range is the book at the current position.
    pub fn quiet_range(&self) -> (i64, i64) {
        let current = self.output_position();
        if self.last_marker_written == self.written { (self.quiet_position, current) } else { (current, current) }
    }

    /// Sends everything written, and returns the stream position that follows it.
    pub fn flush(&mut self) -> i64 {
        while self.leading && self.sent < self.written {
            self.drain();
            spin_loop();
        }
        self.publication.as_ref().map_or(0, AeronExclusivePublication::position)
    }

    /// Every report before this marker is final. A gateway that reads it knows that any in-flight
    /// order with no report never reached the log, so resending it is safe.
    pub fn publish_term_marker(&mut self, leadership_term_id: i64) {
        if self.last_marker_written != self.written {
            self.quiet_position = self.output_position();
        }
        let slot = self.next_slot();
        slot.fill(0);
        slot[0..2].copy_from_slice(&8u16.to_le_bytes());
        slot[2..4].copy_from_slice(&TERM_MARKER_TEMPLATE.to_le_bytes());
        slot[8..16].copy_from_slice(&leadership_term_id.to_le_bytes());
        self.commit_slot();
        self.last_marker_written = self.written;
    }

    /// Tells gateways and the control plane that recovery reached this point. Every report before
    /// it is final, and it's written by every node at the same position.
    pub fn publish_recovery_marker(&mut self, event: RecoveryEvent, tape_position: i64, open_orders: u64) {
        let slot = self.next_slot();
        slot.fill(0);
        slot[0..2].copy_from_slice(&24u16.to_le_bytes());
        slot[2..4].copy_from_slice(&RECOVERY_MARKER_TEMPLATE.to_le_bytes());
        slot[8..16].copy_from_slice(&(event as u64).to_le_bytes());
        slot[16..24].copy_from_slice(&tape_position.to_le_bytes());
        slot[24..32].copy_from_slice(&open_orders.to_le_bytes());
        self.commit_slot();
    }

    pub fn session_id(&self) -> i32 {
        OUTPUT_SESSION_ID
    }

    pub fn state(&self) -> OutputState {
        OutputState {
            written: self.written,
            digest: self.digest(),
            position: self.output_position(),
            quiet_position: self.quiet_position,
            last_marker_written: self.last_marker_written,
        }
    }

    #[cfg(feature = "diagnostics")]
    pub const fn digest(&self) -> u64 {
        self.digest
    }

    #[cfg(not(feature = "diagnostics"))]
    pub const fn digest(&self) -> u64 {
        0
    }

    pub fn restore(&mut self, state: OutputState) {
        self.written = state.written;
        self.sent = state.written;
        self.base_written = state.written;
        self.base_position = state.position;
        self.quiet_position = state.quiet_position;
        self.last_marker_written = state.last_marker_written;
        #[cfg(feature = "diagnostics")]
        {
            self.digest = state.digest;
        }
    }

    /// Moves the stream to a position on the tape, after the book was rebuilt from it. Reports
    /// held back since the hold began are dropped, because the tape has what really happened.
    pub fn rebase(&mut self, position: i64) {
        self.sent = self.written;
        self.base_written = self.written;
        self.base_position = position;
        self.last_marker_written = u64::MAX;
    }

    /// Sends what it can without blocking the matching thread.
    pub fn drain(&mut self) {
        while self.leading && self.sent < self.written {
            let index = (self.sent as usize) & (RING_LENGTH - 1);
            let Some(publication) = &self.publication else { return };
            match publication.offer(&self.ring[index]) {
                Ok(_) => self.sent += 1,
                Err(AeronOfferError::NotConnected | AeronOfferError::BackPressured | AeronOfferError::AdminAction) => return,
                Err(err) => {
                    error!("Failed to publish a report: {}", err);
                    return;
                }
            }
        }
    }

    #[inline(always)]
    fn next_slot(&mut self) -> &mut [u8; MAX_MESSAGE_SIZE] {
        while self.leading && self.written - self.sent >= RING_LENGTH as u64 {
            self.drain();
            spin_loop();
        }
        &mut self.ring[(self.written as usize) & (RING_LENGTH - 1)]
    }

    #[inline(always)]
    fn commit_slot(&mut self) {
        #[cfg(feature = "diagnostics")]
        if self.digesting {
            let index = (self.written as usize) & (RING_LENGTH - 1);
            for byte in &self.ring[index] {
                self.digest = (self.digest ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
            }
        }
        self.written += 1;
        self.drain();
    }

    /// Reads the clock once per inbound message, so every report it causes shares a time.
    #[inline(always)]
    pub fn stamp_transact_time(&mut self) {
        if self.clustered {
            return;
        }
        // This cast is safe until year 2554.
        #[allow(clippy::cast_possible_truncation)]
        let transact_time = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("System time is before Unix epoch") // TODO NO EXPECT
            .as_nanos() as u64;
        self.transact_time = transact_time;
    }

    #[cfg(test)]
    pub fn report(&self, index: u64) -> &[u8; MAX_MESSAGE_SIZE] {
        assert!(self.written - index <= RING_LENGTH as u64, "the ring has moved past this report");
        &self.ring[(index as usize) & (RING_LENGTH - 1)]
    }

    #[inline(always)]
    pub fn publish_new_order(&mut self, order: &Order, exec_id: u64) {
        self.publish_execution_report(&ExecutionReport::New, order, exec_id);
    }

    #[inline(always)]
    pub fn publish_trade(&mut self, order: &Order, exec_id: u64, match_id: u64, qty: i64, px: i64) {
        let trade_report = ExecutionReport::Trade(Trade { match_id, qty, px });
        self.publish_execution_report(&trade_report, order, exec_id);
    }

    #[inline(always)]
    pub fn publish_cancel(&mut self, order: &Order, exec_id: u64) {
        self.publish_execution_report(&ExecutionReport::Cancel, order, exec_id);
    }

    #[inline(always)]
    pub fn publish_cancel_reject(
        &mut self,
        req: &CancelRequest,
        exec_id: u64,
        reason: CxlRejReasonEnum,
        response_to: CxlRejResponseToEnum,
    ) {
        let _ = exec_id;
        let slot = self.next_slot();
        slot.fill(0);
        let write_buf = WriteBuf::new(slot);
        let mut encoder = Self::begin_encoding_cancel_reject(write_buf);

        let client_order_id = {
            let id = req.client_order_id;
            [
                (id >> 64) as u64,                   // high bits
                (id & 0xFFFF_FFFF_FFFF_FFFF) as u64, // low bits
            ]
        };
        let original_client_order_id = {
            let id = req.original_client_order_id;
            [
                (id >> 64) as u64,                   // high bits
                (id & 0xFFFF_FFFF_FFFF_FFFF) as u64, // low bits
            ]
        };

        encoder.cl_ord_id(&client_order_id);
        encoder.orig_cl_ord_id(&original_client_order_id);
        encoder.order_id(u64::MAX); // null
        encoder.ord_status(OrdStatusEnum::NullVal);
        encoder.cxl_rej_response_to(response_to);
        encoder.cxl_rej_reason(reason);

        self.commit_slot();
    }

    #[inline(always)]
    pub fn publish_reject(&mut self, order: &Order, exec_id: u64, reason: OrdRejReasonEnum) {
        let reject_report = ExecutionReport::Reject(Reject { reason });
        self.publish_execution_report(&reject_report, order, exec_id);
    }

    #[inline(always)]
    fn publish_execution_report(&mut self, report: &ExecutionReport, order: &Order, exec_id: u64) {
        let transact_time = self.transact_time;
        let slot = self.next_slot();
        let write_buf = WriteBuf::new(slot);
        let mut encoder = Self::begin_encoding_execution_report(write_buf);

        Self::set_common_fields(&mut encoder, order, exec_id);
        encoder = Self::set_composite_fields(encoder, order, transact_time);
        encoder.exec_type(report.exec_type());
        encoder.ord_status(report.ord_status(order));
        encoder.ord_rej_reason(report.ord_rej_reason());
        report.set_optional_fields(encoder, order);

        self.commit_slot();
    }

    #[inline(always)]
    fn begin_encoding_execution_report(write_buf: WriteBuf<'_>) -> ExecutionReportEncoder<'_> {
        let encoder = ExecutionReportEncoder::default().wrap(write_buf, ENCODED_LENGTH);
        encoder
            .header(0)
            .parent()
            .expect("Failed to create encoder header") // TODO NO EXPECT
    }

    #[inline(always)]
    fn begin_encoding_cancel_reject(write_buf: WriteBuf<'_>) -> OrderCancelRejectEncoder<'_> {
        let encoder = OrderCancelRejectEncoder::default().wrap(write_buf, ENCODED_LENGTH);
        encoder
            .header(0)
            .parent()
            .expect("Failed to create encoder header") // TODO NO EXPECT
    }

    #[inline(always)]
    fn set_common_fields(encoder: &mut ExecutionReportEncoder<'_>, order: &Order, exec_id: u64) {
        let client_order_id = {
            let id = order.client_order_id;
            [
                (id >> 64) as u64,                   // high bits
                (id & 0xFFFF_FFFF_FFFF_FFFF) as u64, // low bits
            ]
        };
        let account = {
            let id = order.account;
            [
                (id >> 64) as u64,                   // high bits
                (id & 0xFFFF_FFFF_FFFF_FFFF) as u64, // low bits
            ]
        };

        encoder.cl_ord_id(&client_order_id);
        encoder.account(&account);
        encoder.order_id(order.sequence_number);
        encoder.exec_id(exec_id);
        encoder.symbol(&order.symbol);
        encoder.side(order.side);
    }

    #[allow(clippy::single_char_lifetime_names)]
    #[inline(always)]
    fn set_composite_fields<'a>(
        encoder: ExecutionReportEncoder<'a>,
        order: &Order,
        transact_time: u64,
    ) -> ExecutionReportEncoder<'a> {
        let encoder = Self::set_transact_time(encoder, transact_time);
        let encoder = Self::set_leaves_qty(encoder, order.leaves_quantity);
        let encoder = Self::set_cum_qty(encoder, order.cumulative_quantity);
        let encoder = Self::set_order_qty(encoder, order.quantity);

        // Only set price for limit orders
        if order.r#type == OrdTypeEnum::Limit {
            Self::set_price(encoder, order.price)
        } else {
            Self::set_price(encoder, i64::MIN)
        }
    }

    #[inline(always)]
    fn set_transact_time(
        encoder: ExecutionReportEncoder<'_>,
        transact_time: u64,
    ) -> ExecutionReportEncoder<'_> {
        let mut time_encoder = encoder.transact_time_encoder();
        time_encoder.time(transact_time);
        time_encoder
            .parent()
            .expect("Failed to get parent after transact_time") // TODO NO EXPECT
    }

    #[inline(always)]
    fn set_order_qty(encoder: ExecutionReportEncoder<'_>, qty: i64) -> ExecutionReportEncoder<'_> {
        let mut qty_encoder = encoder.order_qty_encoder();
        qty_encoder.mantissa(qty);
        qty_encoder
            .parent()
            .expect("Failed to get parent after order_qty") // TODO NO EXPECT
    }

    #[inline(always)]
    fn set_leaves_qty(encoder: ExecutionReportEncoder<'_>, qty: i64) -> ExecutionReportEncoder<'_> {
        let mut qty_encoder = encoder.leaves_qty_encoder();
        qty_encoder.mantissa(qty);
        qty_encoder
            .parent()
            .expect("Failed to get parent after leaves_qty") // TODO NO EXPECT
    }

    #[inline(always)]
    fn set_cum_qty(encoder: ExecutionReportEncoder<'_>, qty: i64) -> ExecutionReportEncoder<'_> {
        let mut qty_encoder = encoder.cum_qty_encoder();
        qty_encoder.mantissa(qty);
        qty_encoder
            .parent()
            .expect("Failed to get parent after cum_qty") // TODO NO EXPECT
    }

    #[inline(always)]
    fn set_price(encoder: ExecutionReportEncoder<'_>, price: i64) -> ExecutionReportEncoder<'_> {
        let mut price_encoder = encoder.price_encoder();
        price_encoder.mantissa(price);
        price_encoder
            .parent()
            .expect("Failed to get parent after price") // TODO NO EXPECT
    }
}
