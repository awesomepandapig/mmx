//! Decides whether the engine may resume after a restart, and rebuilds the book from the tape when
//! it may not. Nothing here touches Aeron, so all of it runs in unit tests.
//!
//! A total loss means the log came back shorter than what the outside world has already seen:
//! the output position recovered from the log is behind a durable position held elsewhere. Going
//! on would write different bytes over positions a game has acted on, which could double spend.
//!
//! The tape can hold more than the log, so it's left alone until the check passes. Replay runs
//! with output held back, and the check happens before this node acknowledges the live log.

use std::collections::BTreeMap;

use cluster::TapeExtent;

use sbe::ReadBuf;
use sbe::exec_type_enum::ExecTypeEnum;
use sbe::execution_report_codec::ExecutionReportDecoder;
use sbe::message_header_codec::MessageHeaderDecoder;
use sbe::ord_type_enum::OrdTypeEnum;

use crate::handler::Handler;
use crate::publisher::{self, RING_LENGTH};
use crate::types::Order;

/// The exit code when the engine refuses to resume. Restarting won't help until an operator acts.
pub const EXIT_TOTAL_LOSS: i32 = 3;

const REPORT_TEMPLATE: u16 = 3;
const CANCEL_REJECT_TEMPLATE: u16 = 4;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryConfig {
    /// The furthest output position anything outside the cluster has made durable.
    pub durable: Option<i64>,
    /// Lets a leader join the log below the durable position, to wait for a rebuild command.
    pub rebuild: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Startup {
    /// Nothing can go wrong from here, so output opens as usual.
    Open,
    /// The snapshot is behind, and only the end of replay will show if the log covers the gap.
    Hold,
}

pub const fn at_startup(position: i64, config: RecoveryConfig) -> Startup {
    match config.durable {
        Some(durable) if position < durable => Startup::Hold,
        _ => Startup::Open,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Join {
    Release,
    Wait,
    Refuse,
}

/// Runs when replay is over. A follower may just be behind a healthy leader, so only a leader can
/// tell that the cluster lost data. A held follower keeps applying the log and catches up.
pub const fn at_join(recovered: i64, durable: i64, leader: bool, rebuild: bool) -> Join {
    if rebuild {
        // Term markers move this position from here on, so only a command can end the hold.
        Join::Wait
    } else if recovered >= durable {
        Join::Release
    } else if leader {
        Join::Refuse
    } else {
        Join::Wait
    }
}

/// Where output should reopen, and how many reports from the ring must follow. A tape shorter
/// than replay can be topped up from the ring, unless it's fallen too far behind.
pub fn output_start(position: i64, tape_stop: Option<i64>) -> Result<(i64, u64), String> {
    let Some(stop) = tape_stop.filter(|stop| *stop < position) else {
        return Ok((position, 0));
    };
    let missing = publisher::frames_between(stop, position);
    if missing > RING_LENGTH as u64 {
        return Err(format!(
            "the tape stops at {stop}, {missing} reports before {position}, which is more than the ring holds"
        ));
    }
    Ok((stop, missing))
}

#[derive(Debug, PartialEq, Eq)]
pub enum RebuildPlan {
    Apply { from: i64 },
    /// The command doesn't fit this state. Every node reaches the same answer, so it changes
    /// nothing. Ignoring it also matters on replay: exiting would turn a typo in the log into a
    /// crash loop.
    Ignore(String),
    /// This node alone can't follow, so it stops rather than diverging.
    Refuse(String),
}

pub struct RebuildRequest {
    pub expected: i64,
    pub target: i64,
    /// The range of positions that differ only by term markers, which leave the book unchanged.
    pub quiet: (i64, i64),
    pub durable: Option<i64>,
    pub held: bool,
    pub tape: Option<TapeExtent>,
}

pub fn plan_rebuild(request: &RebuildRequest) -> RebuildPlan {
    let (from, current) = request.quiet;
    let (expected, target) = (request.expected, request.target);
    if expected < from || expected > current {
        return RebuildPlan::Ignore(format!("expected {expected}, but the engine is at {from}..{current}"));
    }
    if target <= current {
        return RebuildPlan::Ignore(format!("the engine is already at {current}, past {target}"));
    }
    if let Some(durable) = request.durable.filter(|durable| target < *durable) {
        return RebuildPlan::Ignore(format!("{target} is before the durable position {durable}"));
    }
    if !request.held {
        return RebuildPlan::Refuse("output is open, so restart with DURABLE_POSITION set".to_string());
    }
    match request.tape {
        None => RebuildPlan::Refuse("this node has no tape".to_string()),
        Some(tape) if tape.start > from => {
            RebuildPlan::Refuse(format!("the tape starts at {}, after {from}", tape.start))
        }
        Some(tape) if tape.stop < target => {
            RebuildPlan::Refuse(format!("the tape stops at {}, before {target}", tape.stop))
        }
        Some(_) => RebuildPlan::Apply { from },
    }
}

/// Replays reports onto the state a node recovered, without matching anything.
///
/// The tape can stop anywhere, including in the middle of one inbound message. That message's
/// order is kept apart as `tail` until a later report proves it finished, and `settle` runs it
/// through the normal matching code to finish the job. Matching is deterministic, so this writes
/// the bytes the lost engine would have, and every node writes the same ones.
pub struct TapeFold {
    /// Finished orders still resting, by order ID. Queues are in order ID order, because an order
    /// joins its queue once and never moves.
    live: BTreeMap<u64, Order>,
    tail: Option<Order>,
    /// An aggressor's half of a trade whose resting half never reached the tape.
    half_trade: Option<HalfTrade>,
    order_id: u64,
    exec_id: u64,
    match_id: u64,
    transact_time: u64,
}

#[derive(Debug, Clone, Copy)]
struct HalfTrade {
    match_id: u64,
    quantity: i64,
    price: i64,
}

impl TapeFold {
    pub fn seed(handler: &Handler) -> Self {
        let live = handler
            .book
            .resting()
            .map(|order| {
                let mut order = *order;
                order.prev_order_idx = None;
                order.next_order_idx = None;
                (order.sequence_number, order)
            })
            .collect();
        Self {
            live,
            tail: None,
            half_trade: None,
            order_id: handler.counter_order_id,
            exec_id: handler.counter_exec_id,
            match_id: handler.counter_match_id,
            transact_time: 0,
        }
    }

    pub fn apply(&mut self, message: &[u8]) -> Result<(), String> {
        let header: MessageHeaderDecoder<ReadBuf<'_>> = MessageHeaderDecoder::default().wrap(ReadBuf::new(message), 0);
        match header.template_id() {
            REPORT_TEMPLATE => self.apply_report(ExecutionReportDecoder::default().header(header, 0)),
            CANCEL_REJECT_TEMPLATE => {
                self.finish_tail()?;
                self.exec_id += 1; // Cancel rejects spend an exec ID but don't carry it.
                Ok(())
            }
            // Markers sit between inbound messages and change nothing.
            publisher::TERM_MARKER_TEMPLATE | publisher::RECOVERY_MARKER_TEMPLATE => self.finish_tail(),
            template => Err(format!("the tape holds an unknown template {template}")),
        }
    }

    fn finish_tail(&mut self) -> Result<(), String> {
        if self.half_trade.is_some() {
            return Err("the tape is missing the resting side of a trade".to_string());
        }
        if let Some(order) = self.tail.take().filter(|order| order.leaves_quantity > 0) {
            self.live.insert(order.sequence_number, order);
        }
        Ok(())
    }

    fn apply_report(&mut self, report: ExecutionReportDecoder<'_>) -> Result<(), String> {
        let order_id = report.order_id();
        let is_tail = self.tail.is_some_and(|order| order.sequence_number == order_id);
        self.exec_id = report.exec_id();
        self.transact_time = report.transact_time_decoder().time();

        match report.exec_type() {
            ExecTypeEnum::New => {
                self.finish_tail()?;
                self.order_id = order_id;
                self.tail = Some(new_order(report));
            }
            ExecTypeEnum::Trade => self.apply_trade(report, is_tail)?,
            ExecTypeEnum::Canceled | ExecTypeEnum::Rejected if is_tail => {
                if self.half_trade.is_some() {
                    return Err("the tape is missing the resting side of a trade".to_string());
                }
                self.tail = None;
            }
            ExecTypeEnum::Canceled => {
                self.finish_tail()?;
                self.live.remove(&order_id).ok_or_else(|| format!("order {order_id} was canceled but isn't open"))?;
            }
            ExecTypeEnum::Rejected => {
                self.finish_tail()?;
                self.order_id = self.order_id.max(order_id); // Rejected orders still spend an ID.
            }
            ExecTypeEnum::NullVal => return Err("the tape holds a report with no exec type".to_string()),
        }
        Ok(())
    }

    fn apply_trade(&mut self, report: ExecutionReportDecoder<'_>, is_tail: bool) -> Result<(), String> {
        let order_id = report.order_id();
        let match_id = report.trd_match_id().ok_or("the tape holds a trade with no match ID")?;
        let quantity = report.last_qty_decoder().mantissa();
        let price = report.last_px_decoder().mantissa();
        self.match_id = match_id;

        if is_tail {
            if self.half_trade.is_some() {
                return Err("the tape is missing the resting side of a trade".to_string());
            }
            self.half_trade = Some(HalfTrade { match_id, quantity, price });
            if let Some(order) = &mut self.tail {
                order.fill(quantity, price);
            }
            return Ok(());
        }

        self.half_trade = None;
        let order = self.live.get_mut(&order_id).ok_or_else(|| format!("order {order_id} traded but isn't open"))?;
        order.fill(quantity, price);
        if order.leaves_quantity == 0 {
            self.live.remove(&order_id);
        }
        Ok(())
    }

    /// Puts the recovered state into the engine, leaving the unfinished order out.
    pub fn install(&self, handler: &mut Handler) {
        handler.book.reset();
        for order in self.live.values() {
            handler.book.add(order);
        }
        handler.counter_order_id = self.order_id;
        handler.counter_exec_id = self.exec_id;
        handler.counter_match_id = self.match_id;
    }

    /// Finishes the message the tape stopped inside. Output must be open, because this can write
    /// reports. If the message had finished, this rests its order and writes nothing.
    pub fn settle(self, handler: &mut Handler) -> Result<(), String> {
        handler.publisher.set_cluster_time(self.transact_time);
        let Some(mut order) = self.tail else {
            return Ok(());
        };
        if let Some(trade) = self.half_trade {
            handler.settle_resting_side(order.side, &order.symbol, trade.match_id, trade.quantity, trade.price)?;
        }
        if order.leaves_quantity > 0 {
            handler.resume(&mut order);
        }
        Ok(())
    }
}

fn new_order(report: ExecutionReportDecoder<'_>) -> Order {
    let uuid = |id: [u64; 2]| (u128::from(id[0]) << 64) | u128::from(id[1]);
    let price = report.price_decoder().mantissa();
    let quantity = report.order_qty_decoder().mantissa();
    let mut order = Order::empty();
    order.client_order_id = uuid(report.cl_ord_id());
    order.account = uuid(report.account());
    order.symbol = report.symbol();
    order.side = report.side();
    order.transact_time = report.transact_time_decoder().time();
    order.quantity = quantity;
    order.leaves_quantity = quantity;
    order.price = price;
    order.sequence_number = report.order_id();
    // Only limit orders carry a price on the tape.
    order.r#type = if price == i64::MIN { OrdTypeEnum::Market } else { OrdTypeEnum::Limit };
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    const DURABLE: i64 = 1_000;

    fn config(durable: Option<i64>) -> RecoveryConfig {
        RecoveryConfig { durable, rebuild: false }
    }

    #[test]
    fn startup_holds_only_when_the_snapshot_is_behind() {
        assert_eq!(at_startup(999, config(Some(DURABLE))), Startup::Hold);
        assert_eq!(at_startup(1_000, config(Some(DURABLE))), Startup::Open);
        assert_eq!(at_startup(1_001, config(Some(DURABLE))), Startup::Open);
    }

    #[test]
    fn startup_without_a_durable_position_opens_as_before() {
        assert_eq!(at_startup(0, config(None)), Startup::Open);
    }

    #[test]
    fn a_leader_refuses_below_the_durable_position() {
        assert_eq!(at_join(999, DURABLE, true, false), Join::Refuse);
    }

    #[test]
    fn a_follower_below_the_durable_position_waits() {
        assert_eq!(at_join(0, DURABLE, false, false), Join::Wait);
    }

    #[test]
    fn replay_that_reaches_the_durable_position_resumes() {
        assert_eq!(at_join(1_000, DURABLE, true, false), Join::Release);
        assert_eq!(at_join(1_192, DURABLE, false, false), Join::Release);
    }

    #[test]
    fn rebuild_mode_always_waits_for_the_command() {
        assert_eq!(at_join(999, DURABLE, true, true), Join::Wait);
        assert_eq!(at_join(1_000, DURABLE, true, true), Join::Wait);
    }

    #[test]
    fn output_reopens_where_replay_ended() {
        assert_eq!(output_start(1_920, None), Ok((1_920, 0)));
        assert_eq!(output_start(1_920, Some(1_920)), Ok((1_920, 0)));
        assert_eq!(output_start(1_920, Some(9_600)), Ok((1_920, 0)));
    }

    #[test]
    fn a_short_tape_is_topped_up_from_the_ring() {
        assert_eq!(output_start(1_920, Some(960)), Ok((960, 5)));
    }

    #[test]
    fn a_tape_further_behind_than_the_ring_refuses() {
        let position = (RING_LENGTH as i64 + 1) * publisher::FRAME_LENGTH;
        assert!(output_start(position, Some(0)).is_err());
    }

    fn request() -> RebuildRequest {
        RebuildRequest {
            expected: 960,
            target: 1_920,
            quiet: (768, 960),
            durable: Some(1_920),
            held: true,
            tape: Some(TapeExtent { start: 0, stop: 1_920 }),
        }
    }

    #[test]
    fn a_rebuild_starts_at_the_last_report_not_the_last_marker() {
        assert_eq!(plan_rebuild(&request()), RebuildPlan::Apply { from: 768 });
    }

    #[test]
    fn term_markers_during_the_hold_do_not_invalidate_the_command() {
        let moved = RebuildRequest { quiet: (768, 1_152), ..request() };
        assert_eq!(plan_rebuild(&moved), RebuildPlan::Apply { from: 768 });
    }

    #[test]
    fn a_stale_command_changes_nothing() {
        let stale = RebuildRequest { expected: 192, ..request() };
        assert!(matches!(plan_rebuild(&stale), RebuildPlan::Ignore(_)));
    }

    #[test]
    fn a_repeated_command_changes_nothing() {
        let repeated = RebuildRequest { quiet: (1_920, 1_920), expected: 1_920, ..request() };
        assert!(matches!(plan_rebuild(&repeated), RebuildPlan::Ignore(_)));
    }

    #[test]
    fn a_rebuild_cannot_stop_short_of_the_durable_position() {
        let short = RebuildRequest { target: 1_728, ..request() };
        assert!(matches!(plan_rebuild(&short), RebuildPlan::Ignore(_)));
    }

    #[test]
    fn a_node_with_open_output_cannot_rebuild() {
        let open = RebuildRequest { held: false, ..request() };
        assert!(matches!(plan_rebuild(&open), RebuildPlan::Refuse(_)));
    }

    #[test]
    fn a_node_needs_every_byte_it_would_read() {
        for tape in [
            None,
            Some(TapeExtent { start: 960, stop: 1_920 }),
            Some(TapeExtent { start: 0, stop: 1_728 }),
        ] {
            assert!(matches!(plan_rebuild(&RebuildRequest { tape, ..request() }), RebuildPlan::Refuse(_)));
        }
    }
}
