//! Quantity per price level, rebuilt from execution reports.
//!
//! A report that doesn't fit the book means this consumer missed something, such as joining the
//! stream late. That's worth a warning, but never worth stopping for.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use sbe::exec_type_enum::ExecTypeEnum;
use sbe::side_enum::SideEnum;

use serde::{Deserialize, Serialize};

use tracing::warn;

use crate::messages::{ExecutionReportMessage, NULL_DECIMAL};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OrderBook {
    pub bids: BTreeMap<i64, i64>,
    pub asks: BTreeMap<i64, i64>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies a report, returning the new quantity at its price level if it changed one. Market
    /// orders never rest, so they don't touch the book.
    pub fn apply(&mut self, report: &ExecutionReportMessage) -> Option<i64> {
        if report.price == NULL_DECIMAL {
            return None;
        }
        match report.exec_type {
            ExecTypeEnum::New => self.add_order(report),
            ExecTypeEnum::Trade => self.fill_order(report),
            ExecTypeEnum::Canceled => self.remove_order(report),
            _ => None,
        }
    }

    pub fn add_order(&mut self, report: &ExecutionReportMessage) -> Option<i64> {
        let map = self.side_mut(report)?;
        let quantity = map.entry(report.price).or_insert(0);
        *quantity = quantity.saturating_add(report.ord_qty);
        Some(*quantity)
    }

    pub fn fill_order(&mut self, report: &ExecutionReportMessage) -> Option<i64> {
        if report.last_qty == NULL_DECIMAL {
            warn!("Trade {} has no quantity", report.exec_id);
            return None;
        }
        self.reduce(report, report.last_qty)
    }

    pub fn remove_order(&mut self, report: &ExecutionReportMessage) -> Option<i64> {
        self.reduce(report, report.leaves_qty)
    }

    fn reduce(&mut self, report: &ExecutionReportMessage, quantity: i64) -> Option<i64> {
        let exec_id = report.exec_id;
        let map = self.side_mut(report)?;
        match map.entry(report.price) {
            Entry::Occupied(mut entry) => {
                let level = entry.get_mut();
                if *level < quantity {
                    warn!(
                        "Report {} removes {} at price {} but only {} is resting",
                        exec_id, quantity, report.price, *level
                    );
                }
                *level = level.saturating_sub(quantity).max(0);
                let remaining = *level;
                if remaining == 0 {
                    entry.remove();
                }
                Some(remaining)
            }
            Entry::Vacant(_) => {
                warn!(
                    "Report {} is for an empty level at price {}",
                    exec_id, report.price
                );
                None
            }
        }
    }

    fn side_mut(&mut self, report: &ExecutionReportMessage) -> Option<&mut BTreeMap<i64, i64>> {
        match report.side {
            SideEnum::Buy => Some(&mut self.bids),
            SideEnum::Sell => Some(&mut self.asks),
            _ => {
                warn!("Report {} has no side", report.exec_id);
                None
            }
        }
    }

    pub fn get_best_bid(&self) -> Option<(&i64, &i64)> {
        self.bids.last_key_value()
    }

    pub fn get_best_ask(&self) -> Option<(&i64, &i64)> {
        self.asks.first_key_value()
    }
}

/// Follows the order ID sequence, which new and rejected orders advance by one. A gap means
/// reports were missed, so the books may be wrong until a rebuild from the tape.
pub fn track_order_id(last_seen_id: &mut u64, report: &ExecutionReportMessage) {
    if !matches!(report.exec_type, ExecTypeEnum::New | ExecTypeEnum::Rejected) {
        return;
    }
    let expected = last_seen_id.wrapping_add(1);
    if report.order_id != expected {
        warn!(
            "Received order {} but expected {}",
            report.order_id, expected
        );
    }
    *last_seen_id = report.order_id;
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::messages::sample_report;

    fn new_order(qty: i64) -> ExecutionReportMessage {
        ExecutionReportMessage {
            ord_qty: qty,
            leaves_qty: qty,
            ..sample_report()
        }
    }

    fn trade(qty: i64) -> ExecutionReportMessage {
        ExecutionReportMessage {
            exec_type: ExecTypeEnum::Trade,
            last_qty: qty,
            ..sample_report()
        }
    }

    fn cancel(leaves_qty: i64) -> ExecutionReportMessage {
        ExecutionReportMessage {
            exec_type: ExecTypeEnum::Canceled,
            leaves_qty,
            ..sample_report()
        }
    }

    #[test]
    fn canceling_one_order_keeps_the_rest_of_the_level() {
        let mut book = OrderBook::new();
        book.apply(&new_order(10));
        book.apply(&new_order(4));

        assert_eq!(book.apply(&cancel(10)), Some(4));
        assert_eq!(book.bids.get(&100), Some(&4));
    }

    #[test]
    fn canceling_the_last_order_removes_the_level() {
        let mut book = OrderBook::new();
        book.apply(&new_order(10));
        book.apply(&trade(3));

        assert_eq!(book.apply(&cancel(7)), Some(0));
        assert!(book.bids.is_empty());
    }

    #[test]
    fn market_orders_do_not_touch_the_book() {
        let mut book = OrderBook::new();
        let market = ExecutionReportMessage {
            price: NULL_DECIMAL,
            ..new_order(10)
        };
        assert_eq!(book.apply(&market), None);
        assert!(book.bids.is_empty());
    }

    #[test]
    fn rejects_change_nothing() {
        let mut book = OrderBook::new();
        let reject = ExecutionReportMessage {
            exec_type: ExecTypeEnum::Rejected,
            ..new_order(10)
        };
        assert_eq!(book.apply(&reject), None);
        assert!(book.bids.is_empty());
    }

    #[test]
    fn reports_for_missing_levels_are_skipped_instead_of_panicking() {
        let mut book = OrderBook::new();
        assert_eq!(book.apply(&trade(5)), None);
        assert_eq!(book.apply(&cancel(5)), None);
    }

    #[test]
    fn overfilling_a_level_empties_it_instead_of_panicking() {
        let mut book = OrderBook::new();
        book.apply(&new_order(3));

        assert_eq!(book.apply(&cancel(5)), Some(0));
        assert!(book.bids.is_empty());
    }

    #[test]
    fn a_report_with_no_side_is_skipped_instead_of_panicking() {
        let mut book = OrderBook::new();
        let report = ExecutionReportMessage {
            side: SideEnum::NullVal,
            ..new_order(3)
        };
        assert_eq!(book.apply(&report), None);
    }

    #[test]
    fn best_prices_come_from_the_top_of_each_side() {
        let mut book = OrderBook::new();
        book.apply(&ExecutionReportMessage {
            price: 90,
            ..new_order(1)
        });
        book.apply(&new_order(2));
        book.apply(&ExecutionReportMessage {
            price: 110,
            side: SideEnum::Sell,
            ..new_order(3)
        });
        book.apply(&ExecutionReportMessage {
            price: 120,
            side: SideEnum::Sell,
            ..new_order(4)
        });

        assert_eq!(book.get_best_bid(), Some((&100, &2)));
        assert_eq!(book.get_best_ask(), Some((&110, &3)));
    }

    #[test]
    fn order_id_gaps_resynchronise() {
        let mut last_seen = 0;
        track_order_id(
            &mut last_seen,
            &ExecutionReportMessage {
                order_id: 1,
                ..sample_report()
            },
        );
        assert_eq!(last_seen, 1);
        track_order_id(
            &mut last_seen,
            &ExecutionReportMessage {
                order_id: 5,
                ..sample_report()
            },
        );
        assert_eq!(last_seen, 5);
        let trade = ExecutionReportMessage {
            order_id: 2,
            exec_type: ExecTypeEnum::Trade,
            ..sample_report()
        };
        track_order_id(&mut last_seen, &trade);
        assert_eq!(last_seen, 5);
    }

    #[test]
    fn the_largest_order_id_does_not_overflow() {
        let mut last_seen = u64::MAX;
        track_order_id(
            &mut last_seen,
            &ExecutionReportMessage {
                order_id: 1,
                ..sample_report()
            },
        );
        track_order_id(
            &mut last_seen,
            &ExecutionReportMessage {
                order_id: u64::MAX,
                ..sample_report()
            },
        );
        track_order_id(
            &mut last_seen,
            &ExecutionReportMessage {
                order_id: 7,
                ..sample_report()
            },
        );
        assert_eq!(last_seen, 7);
    }
}
