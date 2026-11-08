use crate::ticker::processors::ticker::TickerState;

use sbe::exec_type_enum::ExecTypeEnum;

use tape::book::{OrderBook, track_order_id};
use tape::messages::ExecutionReportMessage;

use tracing::error;

pub fn process_execution_report(
    book: &mut OrderBook,
    last_seen_id: &mut u64,
    ticker_state: &mut TickerState,
    report: &ExecutionReportMessage,
) -> Option<String> {
    track_order_id(last_seen_id, report);
    book.apply(report);

    if report.exec_type != ExecTypeEnum::Trade {
        return None;
    }
    ticker_state.update_on_match(report);

    let ticker = ticker_state.create_ticker(book, report.transact_time);
    match serde_json::to_string(&ticker) {
        Ok(update) => Some(update),
        Err(e) => {
            error!("Failed to serialize Ticker: {}", e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tape::messages::{NULL_DECIMAL, sample_report};

    fn trade() -> ExecutionReportMessage {
        ExecutionReportMessage {
            exec_type: ExecTypeEnum::Trade,
            trd_match_id: Some(1),
            last_px: 100,
            last_qty: 4,
            ..sample_report()
        }
    }

    #[test]
    fn only_trades_publish() {
        let mut book = OrderBook::new();
        let mut state = TickerState::new("ITEM".to_string());
        let mut last_seen = 0;

        assert_eq!(
            process_execution_report(&mut book, &mut last_seen, &mut state, &sample_report()),
            None
        );
        assert!(
            process_execution_report(&mut book, &mut last_seen, &mut state, &trade()).is_some()
        );
    }

    #[test]
    fn a_market_order_trade_moves_the_ticker_but_not_the_book() {
        let mut book = OrderBook::new();
        let mut state = TickerState::new("ITEM".to_string());
        let mut last_seen = 0;
        process_execution_report(&mut book, &mut last_seen, &mut state, &sample_report());
        let market = ExecutionReportMessage {
            price: NULL_DECIMAL,
            ..trade()
        };

        let update = process_execution_report(&mut book, &mut last_seen, &mut state, &market)
            .expect("traded");

        assert!(update.contains(r#""price":"0.000001""#), "{update}");
        assert_eq!(book.bids.get(&100), Some(&10));
    }

    #[test]
    fn a_trade_for_an_unknown_level_still_updates_the_ticker() {
        let mut book = OrderBook::new();
        let mut state = TickerState::new("ITEM".to_string());
        let mut last_seen = 0;

        assert!(
            process_execution_report(&mut book, &mut last_seen, &mut state, &trade()).is_some()
        );
    }
}
