//! Turns reports and candles into rows. Prices and quantities stay exact, as `LONG` mantissas with
//! exponent -8.

use questdb::Result;
use questdb::ingress::{Buffer, TimestampNanos};

use sbe::exec_type_enum::ExecTypeEnum;
use sbe::ord_rej_reason_enum::OrdRejReasonEnum;
use sbe::ord_status_enum::OrdStatusEnum;
use sbe::side_enum::SideEnum;

use tape::candle::Candle;
use tape::messages::{ExecutionReportMessage, NULL_DECIMAL, symbol_name};

use uuid::Uuid;

const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// The position table holds one row, which every write replaces, so its time never changes.
const POSITION_ROW_TIME: i64 = 1_000_000_000 * NANOS_PER_SECOND;

fn u64s_to_uuid(uuid: [u64; 2]) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&uuid[0].to_be_bytes());
    bytes[8..16].copy_from_slice(&uuid[1].to_be_bytes());
    Uuid::from_bytes(bytes)
}

fn exec_type_name(exec_type: ExecTypeEnum) -> &'static str {
    match exec_type {
        ExecTypeEnum::New => "new",
        ExecTypeEnum::Trade => "trade",
        ExecTypeEnum::Canceled => "canceled",
        ExecTypeEnum::Rejected => "rejected",
        ExecTypeEnum::NullVal => "unknown",
    }
}

fn ord_status_name(ord_status: OrdStatusEnum) -> &'static str {
    match ord_status {
        OrdStatusEnum::New => "new",
        OrdStatusEnum::PartiallyFilled => "partially_filled",
        OrdStatusEnum::Filled => "filled",
        OrdStatusEnum::Canceled => "canceled",
        OrdStatusEnum::Rejected => "rejected",
        OrdStatusEnum::NullVal => "unknown",
    }
}

fn side_name(side: SideEnum) -> &'static str {
    match side {
        SideEnum::Buy => "buy",
        SideEnum::Sell => "sell",
        _ => "unknown",
    }
}

fn rej_reason_name(reason: OrdRejReasonEnum) -> Option<&'static str> {
    match reason {
        OrdRejReasonEnum::UnknownOrder => Some("unknown_order"),
        OrdRejReasonEnum::DuplicateOrder => Some("duplicate_order"),
        OrdRejReasonEnum::StaleOrder => Some("stale_order"),
        OrdRejReasonEnum::Other => Some("other"),
        OrdRejReasonEnum::NullVal => None,
    }
}

/// One row per report, so an order's latest row is its current state.
pub fn write_order(buffer: &mut Buffer, report: &ExecutionReportMessage) -> Result<()> {
    let symbol = symbol_name(&report.symbol);
    buffer
        .table("orders")?
        .symbol("symbol", &symbol)?
        .symbol("exec_type", exec_type_name(report.exec_type))?
        .symbol("ord_status", ord_status_name(report.ord_status))?
        .symbol("side", side_name(report.side))?;
    if let Some(reason) = rej_reason_name(report.ord_rej_reason) {
        buffer.symbol("ord_rej_reason", reason)?;
    }

    buffer
        .column_str("account", u64s_to_uuid(report.account).to_string())?
        .column_str("cl_ord_id", u64s_to_uuid(report.cl_ord_id).to_string())?
        .column_i64("order_id", report.order_id as i64)?
        .column_i64("exec_id", report.exec_id as i64)?
        .column_i64("ord_qty", report.ord_qty)?
        .column_i64("leaves_qty", report.leaves_qty)?
        .column_i64("cum_qty", report.cum_qty)?;
    if report.price != NULL_DECIMAL {
        buffer.column_i64("price", report.price)?;
    }
    if report.avg_px != NULL_DECIMAL {
        buffer.column_i64("avg_px", report.avg_px)?;
    }

    buffer.at(TimestampNanos::new(report.transact_time as i64))
}

/// One row per side of a match, so filter on one side to count each match once.
pub fn write_trade(buffer: &mut Buffer, report: &ExecutionReportMessage) -> Result<()> {
    let symbol = symbol_name(&report.symbol);
    buffer
        .table("trades")?
        .symbol("symbol", &symbol)?
        .symbol("side", side_name(report.side))?
        .column_str("account", u64s_to_uuid(report.account).to_string())?
        .column_str("cl_ord_id", u64s_to_uuid(report.cl_ord_id).to_string())?
        .column_i64("order_id", report.order_id as i64)?
        .column_i64("exec_id", report.exec_id as i64)?;
    if let Some(match_id) = report.trd_match_id {
        buffer.column_i64("match_id", match_id as i64)?;
    }
    if report.last_px != NULL_DECIMAL {
        buffer.column_i64("price", report.last_px)?;
    }
    if report.last_qty != NULL_DECIMAL {
        buffer.column_i64("quantity", report.last_qty)?;
    }

    buffer.at(TimestampNanos::new(report.transact_time as i64))
}

pub fn write_candle(buffer: &mut Buffer, candle: &Candle) -> Result<()> {
    let symbol = symbol_name(&candle.symbol);
    buffer
        .table("candles")?
        .symbol("symbol", &symbol)?
        .column_i64("open", candle.open)?
        .column_i64("high", candle.high)?
        .column_i64("low", candle.low)?
        .column_i64("close", candle.close)?
        .column_i64("volume", candle.volume)?
        .column_i64("trades", candle.trades as i64)?
        .column_i64("last_match_id", candle.last_match_id as i64)?
        .at(TimestampNanos::new(
            (candle.start as i64).saturating_mul(NANOS_PER_SECOND),
        ))
}

pub fn write_position(buffer: &mut Buffer, position: i64) -> Result<()> {
    buffer
        .table("recorder_position")?
        .column_i64("position", position)?
        .column_ts("updated_at", TimestampNanos::now())?
        .at(TimestampNanos::new(POSITION_ROW_TIME))
}

#[cfg(test)]
mod tests {
    use super::*;

    use tape::messages::sample_report;

    fn order_line(report: &ExecutionReportMessage) -> String {
        let mut buffer = Buffer::new();
        write_order(&mut buffer, report).expect("writes");
        buffer.as_str().to_string()
    }

    fn filled() -> ExecutionReportMessage {
        ExecutionReportMessage {
            exec_type: ExecTypeEnum::Trade,
            ord_status: OrdStatusEnum::Filled,
            trd_match_id: Some(9),
            order_id: 3,
            exec_id: 12,
            transact_time: 5_000,
            last_px: 95,
            last_qty: 10,
            leaves_qty: 0,
            cum_qty: 10,
            avg_px: 95,
            ..sample_report()
        }
    }

    #[test]
    fn filled_orders_are_recorded() {
        let line = order_line(&filled());

        assert!(
            line.starts_with("orders,symbol=ITEM,exec_type=trade,ord_status=filled,side=buy "),
            "{line}"
        );
        assert!(line.contains("order_id=3i"), "{line}");
        assert!(line.contains("exec_id=12i"), "{line}");
        assert!(line.contains("leaves_qty=0i"), "{line}");
        assert!(line.contains("cum_qty=10i"), "{line}");
        assert!(line.contains("avg_px=95i"), "{line}");
        assert!(line.ends_with(" 5000\n"), "{line}");
    }

    #[test]
    fn every_report_type_is_recorded() {
        for (exec_type, ord_status, expected) in [
            (
                ExecTypeEnum::New,
                OrdStatusEnum::New,
                "exec_type=new,ord_status=new",
            ),
            (
                ExecTypeEnum::Trade,
                OrdStatusEnum::PartiallyFilled,
                "exec_type=trade,ord_status=partially_filled",
            ),
            (
                ExecTypeEnum::Canceled,
                OrdStatusEnum::Canceled,
                "exec_type=canceled,ord_status=canceled",
            ),
            (
                ExecTypeEnum::Rejected,
                OrdStatusEnum::Rejected,
                "exec_type=rejected,ord_status=rejected",
            ),
        ] {
            let line = order_line(&ExecutionReportMessage {
                exec_type,
                ord_status,
                ..sample_report()
            });
            assert!(line.contains(expected), "{line}");
        }
    }

    #[test]
    fn rejects_record_their_reason() {
        let report = ExecutionReportMessage {
            exec_type: ExecTypeEnum::Rejected,
            ord_rej_reason: OrdRejReasonEnum::DuplicateOrder,
            ..sample_report()
        };
        assert!(order_line(&report).contains("ord_rej_reason=duplicate_order"));
        assert!(!order_line(&sample_report()).contains("ord_rej_reason"));
    }

    #[test]
    fn null_values_are_left_out() {
        let line = order_line(&ExecutionReportMessage {
            price: NULL_DECIMAL,
            ..sample_report()
        });

        assert!(!line.contains("price="), "{line}");
        assert!(!line.contains("avg_px="), "{line}");
    }

    #[test]
    fn uuids_match_the_gateway() {
        let report = ExecutionReportMessage {
            cl_ord_id: [0x0011223344556677, 0x8899aabbccddeeff],
            ..sample_report()
        };
        assert!(order_line(&report).contains("cl_ord_id=\"00112233-4455-6677-8899-aabbccddeeff\""));
    }

    #[test]
    fn trades_use_the_trade_price() {
        let mut buffer = Buffer::new();
        write_trade(&mut buffer, &filled()).expect("writes");
        let line = buffer.as_str();

        assert!(line.starts_with("trades,symbol=ITEM,side=buy "), "{line}");
        assert!(line.contains("match_id=9i"), "{line}");
        assert!(line.contains("price=95i"), "{line}");
        assert!(line.contains("quantity=10i"), "{line}");
    }

    #[test]
    fn candles_are_stamped_at_their_start() {
        let candle = Candle {
            symbol: *b"ITEM\0\0",
            start: 300,
            open: 1,
            high: 4,
            low: 1,
            close: 2,
            volume: 5,
            trades: 2,
            last_match_id: 7,
        };
        let mut buffer = Buffer::new();
        write_candle(&mut buffer, &candle).expect("writes");
        let line = buffer.as_str();

        assert!(
            line.starts_with("candles,symbol=ITEM open=1i,high=4i,low=1i,close=2i,volume=5i"),
            "{line}"
        );
        assert!(line.ends_with(" 300000000000\n"), "{line}");
    }

    #[test]
    fn a_huge_candle_time_does_not_overflow() {
        let candle = Candle {
            symbol: *b"ITEM\0\0",
            start: u64::MAX / 1_000_000_000,
            open: 1,
            high: 1,
            low: 1,
            close: 1,
            volume: 1,
            trades: 1,
            last_match_id: 1,
        };
        assert!(write_candle(&mut Buffer::new(), &candle).is_ok());
    }

    #[test]
    fn the_position_row_always_has_the_same_time() {
        let line = |position| {
            let mut buffer = Buffer::new();
            write_position(&mut buffer, position).expect("writes");
            buffer.as_str().to_string()
        };

        let suffix = format!(" {POSITION_ROW_TIME}\n");
        assert!(line(1).ends_with(&suffix));
        assert!(line(2).ends_with(&suffix));
    }

    #[test]
    fn a_time_before_the_epoch_is_an_error_rather_than_a_panic() {
        let report = ExecutionReportMessage {
            transact_time: u64::MAX,
            ..sample_report()
        };
        assert!(write_order(&mut Buffer::new(), &report).is_err());
    }
}
