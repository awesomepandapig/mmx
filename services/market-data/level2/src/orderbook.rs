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

#[cfg(test)]
mod tests {
    use super::*;

    use sbe::exec_type_enum::ExecTypeEnum;
    use sbe::ord_rej_reason_enum::OrdRejReasonEnum;
    use sbe::ord_status_enum::OrdStatusEnum;

    fn report(ord_qty: i64, leaves_qty: i64, last_qty: i64) -> ExecutionReportMessage {
        ExecutionReportMessage {
            account: [0, 1],
            cl_ord_id: [0, 1],
            trd_match_id: None,
            order_id: 1,
            exec_id: 1,
            transact_time: 0,
            price: 100,
            ord_qty,
            last_qty,
            last_px: 100,
            leaves_qty,
            cum_qty: 0,
            avg_px: 0,
            symbol: *b"JSP\0\0\0",
            exec_type: ExecTypeEnum::New,
            ord_status: OrdStatusEnum::New,
            ord_rej_reason: OrdRejReasonEnum::NullVal,
            side: SideEnum::Buy,
        }
    }

    #[test]
    fn canceling_one_order_keeps_the_rest_of_the_level() {
        let mut book = OrderBook::new();
        book.add_order(&report(10, 10, 0));
        book.add_order(&report(4, 4, 0));

        assert_eq!(book.remove_order(&report(10, 10, 0)), 4);
        assert_eq!(book.bids.get(&100), Some(&4));
    }

    #[test]
    fn canceling_the_last_order_removes_the_level() {
        let mut book = OrderBook::new();
        book.add_order(&report(10, 10, 0));
        book.fill_order(&report(10, 7, 3));

        assert_eq!(book.remove_order(&report(10, 7, 0)), 0);
        assert!(book.bids.is_empty());
    }
}
