use serde::{Deserialize, Serialize};

use tape::candle::{Candle, CandleBook};
use tape::feed::{Processor, Update};
use tape::format::format_decimal;
use tape::messages::{ExecutionReportMessage, symbol_name};

use tracing::error;

#[derive(Serialize)]
struct CandleUpdate {
    start: String,
    open: String,
    high: String,
    low: String,
    close: String,
    volume: String,
    product_id: String,
}

fn format_candle(candle: &Candle) -> Result<Update, serde_json::Error> {
    let product_id = symbol_name(&candle.symbol);
    let subject = format!("candles.{product_id}");
    let payload = serde_json::to_string(&CandleUpdate {
        start: candle.start.to_string(),
        open: format_decimal(candle.open),
        high: format_decimal(candle.high),
        low: format_decimal(candle.low),
        close: format_decimal(candle.close),
        volume: format_decimal(candle.volume),
        product_id,
    })?;
    Ok(Update { subject, payload })
}

#[derive(Default, Serialize, Deserialize)]
pub struct Candles {
    book: CandleBook,
}

impl Processor for Candles {
    const NAME: &'static str = "candles";
    const REPLAY_STREAM: i32 = 3020;

    fn on_report(&mut self, report: &ExecutionReportMessage, _updates: &mut Vec<Update>) {
        self.book.apply(report);
    }

    fn on_second(&mut self, now: u64, updates: &mut Vec<Update>) {
        for candle in self.book.tick(now) {
            match format_candle(&candle) {
                Ok(update) => updates.push(update),
                Err(e) => error!("Failed to serialize a candle: {}", e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tape::messages::sample_report;

    #[test]
    fn emits_the_documented_shape_on_its_subject() {
        let candle = Candle {
            symbol: *b"ITEM\0\0",
            start: 1_745_100_300,
            open: 54_500_000_000,
            high: 54_950_000_000,
            low: 51_100_000_000,
            close: 54_400_000_000,
            volume: 123_400_000_000,
            trades: 3,
            last_match_id: 9,
        };

        let update = format_candle(&candle).expect("serializes");

        assert_eq!(update.subject, "candles.ITEM");
        assert_eq!(
            update.payload,
            r#"{"start":"1745100300","open":"545","high":"549.5","low":"511","close":"544","volume":"1234","product_id":"ITEM"}"#
        );
    }

    #[test]
    fn a_quiet_market_emits_nothing() {
        let mut candles = Candles::default();
        let mut updates = Vec::new();
        candles.on_report(&sample_report(), &mut updates);
        candles.on_second(1, &mut updates);

        assert!(updates.is_empty());
    }
}
