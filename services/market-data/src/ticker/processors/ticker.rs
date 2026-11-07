use sbe::side_enum::SideEnum;

use serde::{Deserialize, Serialize};

use tape::book::OrderBook;
use tape::format::{format_decimal, format_timestamp};
use tape::messages::{ExecutionReportMessage, NULL_DECIMAL};

const NANOS_PER_DAY: u64 = 86_400_000_000_000;

#[derive(Serialize, Debug, Clone)]
pub struct Ticker {
    product_id: String,
    price: String,
    volume_24_h: String,
    low_24_h: String,
    high_24_h: String,
    open_24_h: String,
    low_52_w: String,
    high_52_w: String,
    price_percent_chg_24_h: String,
    best_bid: String,
    best_bid_quantity: String,
    best_ask: String,
    best_ask_quantity: String,
    timestamp: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TickerState {
    product_id: String,
    start_24_hr_ts: u64,
    volume_24_h: i64,
    low_24_h: i64,
    high_24_h: i64,
    open_24_hr: i64,
    low_52_w: i64,
    high_52_w: i64,
    last_price: i64,
}

impl TickerState {
    pub fn new(product_id: String) -> Self {
        Self {
            product_id,
            start_24_hr_ts: 0,
            volume_24_h: 0,
            low_24_h: i64::MAX,
            high_24_h: i64::MIN,
            open_24_hr: 0,
            low_52_w: i64::MAX,
            high_52_w: i64::MIN,
            last_price: 0,
        }
    }

    /// Windows follow the engine's time, so a rebuild from the tape lands on the same values.
    fn check_and_reset_windows(&mut self, current_ts: u64) {
        let day_start = current_ts - current_ts % NANOS_PER_DAY;
        if day_start > self.start_24_hr_ts {
            self.start_24_hr_ts = day_start;
            self.volume_24_h = 0;
            self.low_24_h = i64::MAX;
            self.high_24_h = i64::MIN;
            self.open_24_hr = self.last_price;
        }
    }

    /// Adds a trade. Both sides of a match report it, so only the buy side counts.
    pub fn update_on_match(&mut self, report: &ExecutionReportMessage) {
        if report.side != SideEnum::Buy
            || report.last_px == NULL_DECIMAL
            || report.last_qty == NULL_DECIMAL
        {
            return;
        }
        self.check_and_reset_windows(report.transact_time);

        let price = report.last_px;
        self.last_price = price;
        self.volume_24_h = self.volume_24_h.saturating_add(report.last_qty);

        self.low_24_h = self.low_24_h.min(price);
        self.high_24_h = self.high_24_h.max(price);
        if self.open_24_hr == 0 {
            self.open_24_hr = price;
        }

        self.low_52_w = self.low_52_w.min(price);
        self.high_52_w = self.high_52_w.max(price);
    }

    fn calculate_price_percent_change(&self) -> f64 {
        if self.open_24_hr != 0 && self.last_price != 0 {
            return ((self.last_price as f64 - self.open_24_hr as f64) / self.open_24_hr as f64)
                * 100.0;
        }
        0.0
    }

    pub fn create_ticker(&self, book: &OrderBook, timestamp: u64) -> Ticker {
        let (best_bid, best_bid_quantity) = book
            .get_best_bid()
            .map(|(p, q)| (*p, *q))
            .unwrap_or_default();
        let (best_ask, best_ask_quantity) = book
            .get_best_ask()
            .map(|(p, q)| (*p, *q))
            .unwrap_or_default();

        // Handle cases where min/max haven't been updated yet
        let low_24h = if self.low_24_h == i64::MAX {
            self.last_price
        } else {
            self.low_24_h
        };
        let high_24h = if self.high_24_h == i64::MIN {
            self.last_price
        } else {
            self.high_24_h
        };
        let low_52w = if self.low_52_w == i64::MAX {
            self.last_price
        } else {
            self.low_52_w
        };
        let high_52w = if self.high_52_w == i64::MIN {
            self.last_price
        } else {
            self.high_52_w
        };

        Ticker {
            product_id: self.product_id.clone(),
            price: format_decimal(self.last_price),
            volume_24_h: format_decimal(self.volume_24_h),
            low_24_h: format_decimal(low_24h),
            high_24_h: format_decimal(high_24h),
            open_24_h: format_decimal(self.open_24_hr),
            low_52_w: format_decimal(low_52w),
            high_52_w: format_decimal(high_52w),
            price_percent_chg_24_h: self.calculate_price_percent_change().to_string(),
            best_bid: format_decimal(best_bid),
            best_bid_quantity: format_decimal(best_bid_quantity),
            best_ask: format_decimal(best_ask),
            best_ask_quantity: format_decimal(best_ask_quantity),
            timestamp: format_timestamp(timestamp),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sbe::exec_type_enum::ExecTypeEnum;

    use tape::messages::sample_report;

    const ONE: i64 = 100_000_000;

    fn trade(time: u64, price: i64, quantity: i64, side: SideEnum) -> ExecutionReportMessage {
        ExecutionReportMessage {
            exec_type: ExecTypeEnum::Trade,
            trd_match_id: Some(1),
            transact_time: time,
            last_px: price,
            last_qty: quantity,
            side,
            ..sample_report()
        }
    }

    fn matched(state: &mut TickerState, time: u64, price: i64, quantity: i64) {
        state.update_on_match(&trade(time, price, quantity, SideEnum::Sell));
        state.update_on_match(&trade(time, price, quantity, SideEnum::Buy));
    }

    fn state() -> TickerState {
        TickerState::new("ITEM".to_string())
    }

    #[test]
    fn each_match_counts_once() {
        let mut state = state();
        matched(&mut state, NANOS_PER_DAY, 10 * ONE, 3 * ONE);

        assert_eq!(state.volume_24_h, 3 * ONE);
    }

    #[test]
    fn uses_the_trade_price_rather_than_the_order_limit() {
        let mut state = state();
        let report = ExecutionReportMessage {
            price: 12 * ONE,
            ..trade(NANOS_PER_DAY, 10 * ONE, ONE, SideEnum::Buy)
        };
        state.update_on_match(&report);

        assert_eq!(state.last_price, 10 * ONE);
        assert_eq!(state.high_24_h, 10 * ONE);
    }

    #[test]
    fn a_new_day_resets_the_window_and_opens_at_the_last_price() {
        let mut state = state();
        matched(&mut state, NANOS_PER_DAY, 10 * ONE, ONE);
        matched(&mut state, NANOS_PER_DAY + 1, 20 * ONE, ONE);
        matched(&mut state, 3 * NANOS_PER_DAY, 15 * ONE, ONE);

        assert_eq!(state.open_24_hr, 20 * ONE);
        assert_eq!(state.volume_24_h, ONE);
        assert_eq!(state.high_24_h, 15 * ONE);
        assert_eq!(state.low_24_h, 15 * ONE);
        assert_eq!(state.high_52_w, 20 * ONE);
        assert_eq!(state.low_52_w, 10 * ONE);
    }

    #[test]
    fn replaying_old_trades_lands_on_the_same_state() {
        let build = || {
            let mut state = state();
            matched(&mut state, NANOS_PER_DAY + 5, 10 * ONE, ONE);
            matched(&mut state, 2 * NANOS_PER_DAY + 5, 11 * ONE, ONE);
            state
        };
        assert_eq!(
            serde_json::to_string(&build()).ok(),
            serde_json::to_string(&build()).ok()
        );
        assert_eq!(build().volume_24_h, ONE);
    }

    #[test]
    fn percent_change_compares_with_the_open() {
        let mut state = state();
        matched(&mut state, NANOS_PER_DAY, 10 * ONE, ONE);
        matched(&mut state, NANOS_PER_DAY + 1, 11 * ONE, ONE);

        assert!((state.calculate_price_percent_change() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_ticker_with_no_trades_is_all_zeros() {
        let ticker = state().create_ticker(&OrderBook::new(), 0);

        assert_eq!(ticker.price, "0");
        assert_eq!(ticker.low_24_h, "0");
        assert_eq!(ticker.best_bid, "0");
        assert_eq!(ticker.price_percent_chg_24_h, "0");
    }

    #[test]
    fn shows_the_top_of_the_book() {
        let mut book = OrderBook::new();
        book.apply(&ExecutionReportMessage {
            price: 9 * ONE,
            ord_qty: 2 * ONE,
            ..sample_report()
        });
        book.apply(&ExecutionReportMessage {
            price: 11 * ONE,
            ord_qty: ONE,
            side: SideEnum::Sell,
            ..sample_report()
        });

        let ticker = state().create_ticker(&book, 0);

        assert_eq!(
            (ticker.best_bid.as_str(), ticker.best_bid_quantity.as_str()),
            ("9", "2")
        );
        assert_eq!(
            (ticker.best_ask.as_str(), ticker.best_ask_quantity.as_str()),
            ("11", "1")
        );
    }

    #[test]
    fn missing_prices_are_ignored() {
        let mut state = state();
        state.update_on_match(&trade(NANOS_PER_DAY, NULL_DECIMAL, ONE, SideEnum::Buy));

        assert_eq!(state.volume_24_h, 0);
        assert_eq!(state.last_price, 0);
    }
}
