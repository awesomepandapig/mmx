mod processors;

use processors::execution::process_execution_report;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use tape::book::OrderBook;
use tape::feed::{Processor, Update};
use tape::messages::{ExecutionReportMessage, SymbolType, symbol_map, symbol_name};

#[derive(Default, Serialize, Deserialize)]
pub struct Level2 {
    #[serde(with = "symbol_map")]
    books: HashMap<SymbolType, OrderBook>,
    last_seen_id: u64,
}

impl Processor for Level2 {
    const NAME: &'static str = "level2";
    const REPLAY_STREAM: i32 = 3000;

    fn on_report(&mut self, report: &ExecutionReportMessage, updates: &mut Vec<Update>) {
        let book = self.books.entry(report.symbol).or_default();
        if let Some(payload) = process_execution_report(book, &mut self.last_seen_id, report) {
            let subject = format!("l2.{}", symbol_name(&report.symbol));
            updates.push(Update { subject, payload });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tape::messages::sample_report;

    #[test]
    fn publishes_on_the_symbols_subject() {
        let mut level2 = Level2::default();
        let mut updates = Vec::new();
        level2.on_report(&sample_report(), &mut updates);

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].subject, "l2.ITEM");
    }

    #[test]
    fn state_survives_a_checkpoint() {
        let mut level2 = Level2::default();
        level2.on_report(&sample_report(), &mut Vec::new());

        let json = serde_json::to_string(&level2).expect("serializes");
        let restored: Level2 = serde_json::from_str(&json).expect("deserializes");

        assert_eq!(restored.last_seen_id, 1);
        assert_eq!(restored.books[b"ITEM\0\0"].bids.get(&100), Some(&10));
    }
}
