use crate::level2::processors::level2::format_l2_update_json;

use sbe::exec_type_enum::ExecTypeEnum;

use tape::book::{OrderBook, track_order_id};
use tape::format::{format_decimal, format_timestamp};
use tape::messages::ExecutionReportMessage;

use tracing::{error, warn};

pub fn process_execution_report(
    book: &mut OrderBook,
    last_seen_id: &mut u64,
    report: &ExecutionReportMessage,
) -> Option<String> {
    track_order_id(last_seen_id, report);
    if report.exec_type == ExecTypeEnum::Rejected {
        warn!("Rejected order: {}", report.order_id);
    }

    let new_quantity = book.apply(report)?;
    let update = format_l2_update_json(
        report.side.to_string().to_lowercase(),
        format_timestamp(report.transact_time),
        format_decimal(report.price),
        format_decimal(new_quantity),
    );
    match update {
        Ok(update) => Some(update),
        Err(e) => {
            error!("Failed to serialize L2Update: {}", e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sbe::side_enum::SideEnum;

    use tape::messages::{NULL_DECIMAL, sample_report};

    const PRICE: i64 = 150_000_000;

    fn new_order(order_id: u64, quantity: i64) -> ExecutionReportMessage {
        ExecutionReportMessage {
            order_id,
            price: PRICE,
            ord_qty: quantity,
            ..sample_report()
        }
    }

    #[test]
    fn a_new_order_reports_the_level_total() {
        let mut book = OrderBook::new();
        let mut last_seen = 0;
        process_execution_report(&mut book, &mut last_seen, &new_order(1, 100_000_000));
        let update = process_execution_report(&mut book, &mut last_seen, &new_order(2, 50_000_000))
            .expect("the level changed");

        assert_eq!(
            update,
            r#"{"side":"buy","event_time":"1970-01-01T00:00:00.000000000Z","price_level":"1.5","new_quantity":"1.5"}"#
        );
    }

    #[test]
    fn a_fill_that_empties_the_level_reports_zero() {
        let mut book = OrderBook::new();
        let mut last_seen = 0;
        process_execution_report(&mut book, &mut last_seen, &new_order(1, 100_000_000));
        let fill = ExecutionReportMessage {
            exec_type: ExecTypeEnum::Trade,
            last_qty: 100_000_000,
            ..new_order(1, 100_000_000)
        };

        let update =
            process_execution_report(&mut book, &mut last_seen, &fill).expect("the level changed");

        assert!(update.contains(r#""new_quantity":"0""#), "{update}");
    }

    #[test]
    fn market_orders_and_rejects_publish_nothing() {
        let mut book = OrderBook::new();
        let mut last_seen = 0;
        let market = ExecutionReportMessage {
            order_id: 1,
            price: NULL_DECIMAL,
            ..sample_report()
        };
        let reject = ExecutionReportMessage {
            exec_type: ExecTypeEnum::Rejected,
            ..new_order(2, 1)
        };

        assert_eq!(
            process_execution_report(&mut book, &mut last_seen, &market),
            None
        );
        assert_eq!(
            process_execution_report(&mut book, &mut last_seen, &reject),
            None
        );
        assert_eq!(last_seen, 2);
    }

    #[test]
    fn a_trade_for_an_unknown_level_is_skipped_instead_of_panicking() {
        let mut book = OrderBook::new();
        let mut last_seen = 0;
        let fill = ExecutionReportMessage {
            exec_type: ExecTypeEnum::Trade,
            side: SideEnum::Sell,
            last_qty: 1,
            ..new_order(9, 1)
        };

        assert_eq!(
            process_execution_report(&mut book, &mut last_seen, &fill),
            None
        );
    }

    #[test]
    fn a_gap_in_order_ids_keeps_going() {
        let mut book = OrderBook::new();
        let mut last_seen = 0;

        assert!(process_execution_report(&mut book, &mut last_seen, &new_order(7, 1)).is_some());
        assert_eq!(last_seen, 7);
    }
}
