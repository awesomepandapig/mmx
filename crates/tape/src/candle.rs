//! Candles built from trades, on the time the engine stamped them. Rebuilding from the tape gives
//! the same candles, however fast the tape is read.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::mem;

use sbe::exec_type_enum::ExecTypeEnum;
use sbe::side_enum::SideEnum;

use serde::{Deserialize, Serialize};

use crate::messages::{ExecutionReportMessage, NULL_DECIMAL, SymbolType, symbol_map};

pub const CANDLE_INTERVAL_SECONDS: u64 = 300;

const NANOS_PER_SECOND: u64 = 1_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candle {
    pub symbol: SymbolType,
    /// Seconds since the epoch.
    pub start: u64,
    pub open: i64,
    pub high: i64,
    pub low: i64,
    pub close: i64,
    pub volume: i64,
    pub trades: u64,
    pub last_match_id: u64,
}

impl Candle {
    fn first(symbol: SymbolType, start: u64, price: i64, quantity: i64, match_id: u64) -> Self {
        Self {
            symbol,
            start,
            open: price,
            high: price,
            low: price,
            close: price,
            volume: quantity,
            trades: 1,
            last_match_id: match_id,
        }
    }

    fn add(&mut self, price: i64, quantity: i64, match_id: u64) {
        self.high = self.high.max(price);
        self.low = self.low.min(price);
        self.close = price;
        self.volume = self.volume.saturating_add(quantity);
        self.trades += 1;
        self.last_match_id = match_id;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OpenCandle {
    candle: Candle,
    changed: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CandleBook {
    #[serde(with = "symbol_map")]
    open: HashMap<SymbolType, OpenCandle>,
    closed: Vec<Candle>,
    skip_replayed: bool,
}

impl CandleBook {
    /// Continues from candles saved earlier. Match IDs never reset on the tape, so anything at or
    /// below a candle's last match is already counted, which makes replaying over them safe.
    pub fn resume(candles: impl IntoIterator<Item = Candle>) -> Self {
        let open = candles
            .into_iter()
            .map(|candle| {
                (
                    candle.symbol,
                    OpenCandle {
                        candle,
                        changed: false,
                    },
                )
            })
            .collect();
        Self {
            open,
            closed: Vec::new(),
            skip_replayed: true,
        }
    }

    /// Adds a trade. Both sides of a match report it, so only the buy side counts.
    pub fn apply(&mut self, report: &ExecutionReportMessage) {
        if report.exec_type != ExecTypeEnum::Trade || report.side != SideEnum::Buy {
            return;
        }
        let Some(match_id) = report.trd_match_id else {
            return;
        };
        if report.last_px == NULL_DECIMAL || report.last_qty == NULL_DECIMAL {
            return;
        }

        let seconds = report.transact_time / NANOS_PER_SECOND;
        let start = seconds - seconds % CANDLE_INTERVAL_SECONDS;
        let (price, quantity) = (report.last_px, report.last_qty);

        match self.open.entry(report.symbol) {
            Entry::Occupied(mut entry) => {
                let slot = entry.get_mut();
                if self.skip_replayed && match_id <= slot.candle.last_match_id {
                    return;
                }
                if start > slot.candle.start {
                    let next = Candle::first(report.symbol, start, price, quantity, match_id);
                    self.closed.push(mem::replace(&mut slot.candle, next));
                } else {
                    slot.candle.add(price, quantity, match_id);
                }
                slot.changed = true;
            }
            Entry::Vacant(entry) => {
                let candle = Candle::first(report.symbol, start, price, quantity, match_id);
                entry.insert(OpenCandle {
                    candle,
                    changed: true,
                });
            }
        }
    }

    /// The candles to emit at `now`: every one that closed, every one that changed, and any still
    /// inside its interval, which repeats each second so new subscribers see it.
    pub fn tick(&mut self, now: u64) -> Vec<Candle> {
        let mut due = mem::take(&mut self.closed);
        for slot in self.open.values_mut() {
            if slot.changed || slot.candle.start.saturating_add(CANDLE_INTERVAL_SECONDS) > now {
                slot.changed = false;
                due.push(slot.candle.clone());
            }
        }
        due.sort_unstable_by_key(|candle| (candle.start, candle.symbol));
        due
    }

    /// Everything that closed or changed since the last call.
    pub fn take_changed(&mut self) -> Vec<Candle> {
        let mut due = mem::take(&mut self.closed);
        for slot in self.open.values_mut() {
            if mem::take(&mut slot.changed) {
                due.push(slot.candle.clone());
            }
        }
        due.sort_unstable_by_key(|candle| (candle.start, candle.symbol));
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::messages::sample_report;

    const BUCKET: u64 = 1_500;

    fn trade(
        seconds: u64,
        match_id: u64,
        price: i64,
        quantity: i64,
        side: SideEnum,
    ) -> ExecutionReportMessage {
        ExecutionReportMessage {
            exec_type: ExecTypeEnum::Trade,
            trd_match_id: Some(match_id),
            transact_time: seconds * NANOS_PER_SECOND,
            last_px: price,
            last_qty: quantity,
            side,
            ..sample_report()
        }
    }

    fn pair(book: &mut CandleBook, seconds: u64, match_id: u64, price: i64, quantity: i64) {
        book.apply(&trade(seconds, match_id, price, quantity, SideEnum::Sell));
        book.apply(&trade(seconds, match_id, price, quantity, SideEnum::Buy));
    }

    #[test]
    fn tracks_open_high_low_close_and_volume() {
        let mut book = CandleBook::default();
        pair(&mut book, BUCKET, 1, 100, 2);
        pair(&mut book, BUCKET + 1, 2, 120, 3);
        pair(&mut book, BUCKET + 2, 3, 90, 4);
        pair(&mut book, BUCKET + 299, 4, 110, 5);

        let candles = book.take_changed();
        assert_eq!(candles.len(), 1);
        let candle = &candles[0];
        assert_eq!(candle.start, BUCKET);
        assert_eq!(
            (candle.open, candle.high, candle.low, candle.close),
            (100, 120, 90, 110)
        );
        assert_eq!(candle.volume, 14);
        assert_eq!(candle.trades, 4);
        assert_eq!(candle.last_match_id, 4);
    }

    #[test]
    fn each_match_counts_once() {
        let mut book = CandleBook::default();
        pair(&mut book, BUCKET, 1, 100, 2);
        assert_eq!(book.take_changed()[0].volume, 2);
    }

    #[test]
    fn a_trade_in_the_next_interval_closes_the_candle() {
        let mut book = CandleBook::default();
        pair(&mut book, BUCKET, 1, 100, 2);
        pair(&mut book, BUCKET + 300, 2, 105, 1);

        let candles = book.take_changed();
        assert_eq!(candles.len(), 2);
        assert_eq!((candles[0].start, candles[0].close), (BUCKET, 100));
        assert_eq!((candles[1].start, candles[1].open), (BUCKET + 300, 105));
    }

    #[test]
    fn symbols_keep_separate_candles() {
        let mut book = CandleBook::default();
        pair(&mut book, BUCKET, 1, 100, 2);
        let other = ExecutionReportMessage {
            symbol: *b"OTHER\0",
            ..trade(BUCKET, 2, 500, 1, SideEnum::Buy)
        };
        book.apply(&other);

        assert_eq!(book.take_changed().len(), 2);
    }

    #[test]
    fn only_trades_count() {
        let mut book = CandleBook::default();
        book.apply(&sample_report());
        book.apply(&ExecutionReportMessage {
            last_px: NULL_DECIMAL,
            ..trade(BUCKET, 1, 100, 2, SideEnum::Buy)
        });
        book.apply(&ExecutionReportMessage {
            trd_match_id: None,
            ..trade(BUCKET, 1, 100, 2, SideEnum::Buy)
        });

        assert!(book.take_changed().is_empty());
    }

    #[test]
    fn ticks_repeat_a_current_candle_every_second() {
        let mut book = CandleBook::default();
        pair(&mut book, BUCKET, 1, 100, 2);

        assert_eq!(book.tick(BUCKET + 1).len(), 1);
        assert_eq!(book.tick(BUCKET + 2).len(), 1);
    }

    #[test]
    fn ticks_stop_once_an_idle_candle_is_over() {
        let mut book = CandleBook::default();
        pair(&mut book, BUCKET, 1, 100, 2);

        assert_eq!(
            book.tick(BUCKET + 301).len(),
            1,
            "the final state still goes out once"
        );
        assert!(book.tick(BUCKET + 302).is_empty());
    }

    #[test]
    fn closed_candles_go_out_exactly_once() {
        let mut book = CandleBook::default();
        pair(&mut book, BUCKET, 1, 100, 2);
        pair(&mut book, BUCKET + 300, 2, 105, 1);

        assert_eq!(book.tick(BUCKET + 300).len(), 2);
        assert_eq!(book.tick(BUCKET + 301).len(), 1);
    }

    #[test]
    fn take_changed_ignores_unchanged_candles() {
        let mut book = CandleBook::default();
        pair(&mut book, BUCKET, 1, 100, 2);

        assert_eq!(book.take_changed().len(), 1);
        assert!(book.take_changed().is_empty());
    }

    #[test]
    fn resuming_skips_trades_already_counted() {
        let mut book = CandleBook::default();
        pair(&mut book, BUCKET, 1, 100, 2);
        pair(&mut book, BUCKET + 1, 2, 110, 3);
        let saved = book.take_changed();

        let mut resumed = CandleBook::resume(saved);
        pair(&mut resumed, BUCKET + 1, 2, 110, 3);
        assert!(resumed.take_changed().is_empty());

        pair(&mut resumed, BUCKET + 2, 3, 120, 1);
        let candles = resumed.take_changed();
        assert_eq!(candles[0].volume, 6);
        assert_eq!(candles[0].trades, 3);
    }

    #[test]
    fn rebuilding_the_same_trades_gives_the_same_candles() {
        let build = || {
            let mut book = CandleBook::default();
            pair(&mut book, BUCKET, 1, 100, 2);
            pair(&mut book, BUCKET + 400, 2, 90, 7);
            pair(&mut book, BUCKET + 401, 3, 95, 1);
            book.take_changed()
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn state_survives_a_checkpoint() {
        let mut book = CandleBook::default();
        pair(&mut book, BUCKET, 1, 100, 2);

        let json = serde_json::to_string(&book).expect("serializes");
        let mut restored: CandleBook = serde_json::from_str(&json).expect("deserializes");
        pair(&mut restored, BUCKET + 1, 2, 110, 3);

        assert_eq!(restored.take_changed()[0].volume, 5);
    }
}
