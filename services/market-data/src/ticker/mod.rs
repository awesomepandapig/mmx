mod processors;

use processors::execution::process_execution_report;
use processors::ticker::TickerState;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use tape::book::OrderBook;
use tape::feed::{Processor, Update};
use tape::messages::{ExecutionReportMessage, SymbolType, symbol_map, symbol_name};

#[derive(Default, Serialize, Deserialize)]
pub struct Tickers {
    #[serde(with = "symbol_map")]
    markets: HashMap<SymbolType, (OrderBook, TickerState)>,
    last_seen_id: u64,
}

impl Processor for Tickers {
    const NAME: &'static str = "ticker";
    const REPLAY_STREAM: i32 = 3010;

    fn on_report(&mut self, report: &ExecutionReportMessage, updates: &mut Vec<Update>) {
        let (book, ticker_state) = self.markets.entry(report.symbol).or_insert_with(|| {
            (
                OrderBook::new(),
                TickerState::new(symbol_name(&report.symbol)),
            )
        });
        if let Some(payload) =
            process_execution_report(book, &mut self.last_seen_id, ticker_state, report)
        {
            let subject = format!("ticker.{}", symbol_name(&report.symbol));
            updates.push(Update { subject, payload });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sbe::exec_type_enum::ExecTypeEnum;

    use tape::messages::sample_report;

    fn trade() -> ExecutionReportMessage {
        ExecutionReportMessage {
            exec_type: ExecTypeEnum::Trade,
            trd_match_id: Some(1),
            last_px: 100,
            last_qty: 1,
            ..sample_report()
        }
    }

    #[test]
    fn publishes_on_the_symbols_subject() {
        let mut tickers = Tickers::default();
        let mut updates = Vec::new();
        tickers.on_report(&sample_report(), &mut updates);
        tickers.on_report(&trade(), &mut updates);

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].subject, "ticker.ITEM");
    }

    #[test]
    fn state_survives_a_checkpoint() {
        let mut tickers = Tickers::default();
        tickers.on_report(&sample_report(), &mut Vec::new());

        let json = serde_json::to_string(&tickers).expect("serializes");
        let restored: Tickers = serde_json::from_str(&json).expect("deserializes");

        assert_eq!(restored.last_seen_id, 1);
        assert_eq!(restored.markets[b"ITEM\0\0"].0.bids.get(&100), Some(&10));
    }
}
