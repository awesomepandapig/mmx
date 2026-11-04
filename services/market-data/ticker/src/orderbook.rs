use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use crate::messages::ExecutionReportMessage;
use sbe::side_enum::SideEnum;

#[derive(Clone, Debug)]
pub struct OrderBook {
    pub bids: BTreeMap<i64, i64>,
    pub asks: BTreeMap<i64, i64>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
        }
    }

    pub fn add_order(&mut self, report: &ExecutionReportMessage) -> i64 {
        debug_assert!(
            report.ord_qty > 0,
            "add_order called with zero or negative ord_qty: {}",
            report.ord_qty
        );

        let map = match report.side {
            SideEnum::Buy => &mut self.bids,
            SideEnum::Sell => &mut self.asks,
            _ => panic!("Unknown side provided: {}", report.side),
        };
        let price = report.price;
        let quantity = map.entry(price).or_insert(0);
        *quantity += report.ord_qty;
        *quantity
    }

    pub fn fill_order(&mut self, report: &ExecutionReportMessage) -> i64 {
        let map = match report.side {
            SideEnum::Buy => &mut self.bids,
            SideEnum::Sell => &mut self.asks,
            _ => panic!("Unknown side provided: {}", report.side),
        };
        let price = report.price;

        match map.entry(price) {
            Entry::Occupied(mut entry) => {
                let current_quantity_at_level = *entry.get();

                // --- Assertions ---
                // Assumption: A fill must be for a positive quantity.
                debug_assert!(
                    report.last_qty > 0,
                    "fill_order called with zero or negative last_qty: {}",
                    report.last_qty
                );

                // Assumption: The book must have enough quantity to satisfy the fill.
                // If this fails, you have "phantom liquidity" - the book thinks it has shares that don't exist.
                debug_assert!(
                    current_quantity_at_level >= report.last_qty,
                    "CRITICAL: fill_order for {} shares but only {} available at price {}",
                    report.last_qty,
                    current_quantity_at_level,
                    price
                );

                let quantity = entry.get_mut();
                *quantity -= report.last_qty;
                if *quantity == 0 {
                    entry.remove();
                    0
                } else {
                    *quantity
                }
            }
            Entry::Vacant(_) => {
                panic!(
                    "Assumption Violation: fill_order called for non-existent price level {} on side {}",
                    price, report.side
                );
            }
        }
    }

    pub fn remove_order(&mut self, report: &ExecutionReportMessage) -> i64 {
        let map = match report.side {
            SideEnum::Buy => &mut self.bids,
            SideEnum::Sell => &mut self.asks,
            _ => panic!("Unknown side provided: {}", report.side),
        };
        let price = report.price;

        match map.entry(price) {
            Entry::Occupied(mut entry) => {
                let quantity_to_remove = report.leaves_qty;

                assert!(
                    *entry.get() >= quantity_to_remove,
                    "CRITICAL: Canceled {} at price {} but only {} is available",
                    quantity_to_remove,
                    price,
                    entry.get()
                );

                let quantity = entry.get_mut();
                *quantity -= quantity_to_remove;
                if *quantity == 0 {
                    entry.remove();
                    0
                } else {
                    *quantity
                }
            }
            Entry::Vacant(_) => {
                panic!(
                    "Assumption Violation: remove_order called for non-existent price level {} on side {}",
                    price, report.side
                );
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