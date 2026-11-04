//! Replays the engine's output and verifies what it promises.
//!
//! The checker rebuilds the book from execution reports alone, so it needs every report from
//! the start of the engine's run.
//!
//! Invariants:
//! - `exec_id` increases by one for every report, including cancel rejects
//! - `order_id` increases, skipping only IDs consumed by cancel requests
//! - each order's quantities add up: `cum + leaves == ord_qty`, and fills reduce `leaves`
//! - `ord_status` agrees with the quantities
//! - nothing happens to an order after it finishes
//! - trades come in pairs: same match, quantity, price and symbol, opposite sides, two accounts
//! - `match_id` increases by one for every pair
//! - trades happen at the resting order's price, and within each order's limit
//! - the resting side is matched in price-time priority
//! - the book is never crossed once an order finishes matching
//! - every unit of quantity is filled, canceled or still resting

use crate::event::{Event, ExecType, Execution, OrdStatus, Side};

use std::collections::{BTreeMap, HashMap, VecDeque};

const MAX_STORED_VIOLATIONS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub rule: &'static str,
    pub exec_id: u64,
    pub order_id: u64,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub orders: u64,
    pub rejected: u64,
    pub trades: u64,
    pub canceled: u64,
    pub cancel_rejects: u64,
    pub submitted_qty: i128,
    pub filled_qty: i128,
    pub canceled_qty: i128,
    pub notional: i128,
    pub last_transact_time: u64,
}

#[derive(Debug)]
struct OrderState {
    side: Side,
    price: Option<i64>,
    ord_qty: i64,
    leaves_qty: i64,
    cum_qty: i64,
    finished: bool,
}

#[derive(Default)]
pub struct Checker {
    started: bool,
    last_exec_id: u64,
    last_order_id: u64,
    consumed_order_ids: u64,
    last_match_id: u64,
    orders: HashMap<u64, OrderState>,
    bids: BTreeMap<i64, VecDeque<u64>>,
    asks: BTreeMap<i64, VecDeque<u64>>,
    aggressor: Option<u64>,
    open_trade: Option<Execution>,
    pub stats: Stats,
    pub violation_count: u64,
    pub violations: Vec<Violation>,
}

impl Checker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_clean(&self) -> bool {
        self.violation_count == 0
    }

    fn violation(&mut self, rule: &'static str, exec_id: u64, order_id: u64, detail: String) {
        self.violation_count += 1;
        if self.violations.len() < MAX_STORED_VIOLATIONS {
            self.violations.push(Violation { rule, exec_id, order_id, detail });
        }
    }

    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::Execution(report) => self.apply_execution(report),
            Event::CancelReject { .. } => {
                // Cancel rejects carry no exec_id, but the engine still spends one on them.
                self.settle(self.last_exec_id, 0);
                self.last_exec_id += 1;
                self.consumed_order_ids += 1;
                self.stats.cancel_rejects += 1;
                self.started = true;
            }
            Event::Unknown { template_id } => {
                self.violation(
                    "known_template",
                    self.last_exec_id,
                    0,
                    format!("unknown template {template_id}"),
                );
            }
            Event::NewOrder { .. } | Event::CancelRequest { .. } => {}
        }
    }

    fn apply_execution(&mut self, report: &Execution) {
        self.check_exec_id(report);
        self.stats.last_transact_time = report.transact_time;

        match report.exec_type {
            ExecType::New => self.on_new(report),
            ExecType::Trade => self.on_trade(report),
            ExecType::Canceled => self.on_canceled(report),
            ExecType::Rejected => self.on_rejected(report),
            ExecType::Unknown => self.violation(
                "known_exec_type",
                report.exec_id,
                report.order_id,
                "unknown exec_type".to_string(),
            ),
        }
    }

    fn check_exec_id(&mut self, report: &Execution) {
        let expected = self.last_exec_id + 1;
        if report.exec_id != expected {
            let hint = if self.started { "" } else { " (started after the engine?)" };
            self.violation(
                "exec_id_sequence",
                report.exec_id,
                report.order_id,
                format!("expected {expected}, got {}{hint}", report.exec_id),
            );
        }
        self.started = true;
        self.last_exec_id = report.exec_id;
    }

    fn check_order_id(&mut self, report: &Execution) {
        let last_allowed = self.last_order_id + 1 + self.consumed_order_ids;
        if report.order_id <= self.last_order_id || report.order_id > last_allowed {
            self.violation(
                "order_id_sequence",
                report.exec_id,
                report.order_id,
                format!(
                    "expected {}..={last_allowed}, got {}",
                    self.last_order_id + 1,
                    report.order_id
                ),
            );
        }
        self.last_order_id = report.order_id;
        self.consumed_order_ids = 0;
    }

    /// Finishes the previous aggressor: an unfilled limit order rests, then the book must be
    /// uncrossed. Called whenever the next message starts.
    fn settle(&mut self, exec_id: u64, order_id: u64) {
        if let Some(open) = self.open_trade.take() {
            self.violation(
                "trade_pairs",
                exec_id,
                open.order_id,
                format!("match {:?} had only one side", open.match_id),
            );
        }

        if let Some(aggressor) = self.aggressor.take() {
            let resting = self.orders.get(&aggressor).and_then(|order| {
                let rests = !order.finished && order.leaves_qty > 0;
                rests.then_some((order.side, order.price))
            });
            match resting {
                Some((Side::Buy, Some(price))) => {
                    self.bids.entry(price).or_default().push_back(aggressor);
                }
                Some((Side::Sell, Some(price))) => {
                    self.asks.entry(price).or_default().push_back(aggressor);
                }
                Some((_, None)) => self.violation(
                    "market_orders_never_rest",
                    exec_id,
                    aggressor,
                    "market order finished with quantity open".to_string(),
                ),
                _ => {}
            }
        }

        if let (Some(bid), Some(ask)) = (self.best_bid(), self.best_ask())
            && bid >= ask
        {
            self.violation(
                "book_uncrossed",
                exec_id,
                order_id,
                format!("best bid {bid} >= best ask {ask}"),
            );
        }
    }

    fn on_new(&mut self, report: &Execution) {
        self.settle(report.exec_id, report.order_id);
        self.check_order_id(report);

        if report.ord_qty <= 0 {
            self.violation(
                "positive_quantity",
                report.exec_id,
                report.order_id,
                format!("ord_qty {}", report.ord_qty),
            );
        }
        if report.leaves_qty != report.ord_qty || report.cum_qty != 0 {
            self.violation(
                "new_order_quantities",
                report.exec_id,
                report.order_id,
                format!(
                    "ord {} leaves {} cum {}",
                    report.ord_qty, report.leaves_qty, report.cum_qty
                ),
            );
        }
        if report.ord_status != OrdStatus::New {
            self.violation(
                "status_matches_quantities",
                report.exec_id,
                report.order_id,
                format!("New report with status {:?}", report.ord_status),
            );
        }

        self.stats.orders += 1;
        self.stats.submitted_qty += i128::from(report.ord_qty);

        let order = OrderState {
            side: report.side,
            price: report.price,
            ord_qty: report.ord_qty,
            leaves_qty: report.leaves_qty,
            cum_qty: report.cum_qty,
            finished: false,
        };
        if self.orders.insert(report.order_id, order).is_some() {
            self.violation(
                "order_id_unique",
                report.exec_id,
                report.order_id,
                "order_id reused".to_string(),
            );
        }
        self.aggressor = Some(report.order_id);
    }

    fn on_rejected(&mut self, report: &Execution) {
        self.settle(report.exec_id, report.order_id);
        self.check_order_id(report);
        self.stats.rejected += 1;
    }

    fn on_trade(&mut self, report: &Execution) {
        self.check_fill(report);

        let (Some(last_qty), Some(last_px)) = (report.last_qty, report.last_px) else {
            self.violation(
                "trade_fields",
                report.exec_id,
                report.order_id,
                "trade without last_qty or last_px".to_string(),
            );
            return;
        };

        match self.open_trade.take() {
            None => {
                if self.aggressor != Some(report.order_id) {
                    self.violation(
                        "aggressor_reported_first",
                        report.exec_id,
                        report.order_id,
                        format!("expected aggressor {:?}", self.aggressor),
                    );
                }
                let match_id = report.match_id.unwrap_or(0);
                if match_id != self.last_match_id + 1 {
                    self.violation(
                        "match_id_sequence",
                        report.exec_id,
                        report.order_id,
                        format!("expected {}, got {match_id}", self.last_match_id + 1),
                    );
                }
                self.last_match_id = match_id;
                self.open_trade = Some(report.clone());
            }
            Some(aggressor) => {
                self.check_pair(&aggressor, report);
                self.check_priority(report, last_px);
                self.stats.trades += 1;
                self.stats.notional += i128::from(last_qty) * i128::from(last_px);
            }
        }
    }

    fn check_fill(&mut self, report: &Execution) {
        let Some(order) = self.orders.get_mut(&report.order_id) else {
            self.violation(
                "known_order",
                report.exec_id,
                report.order_id,
                "trade for an unknown order".to_string(),
            );
            return;
        };

        let last_qty = report.last_qty.unwrap_or(0);
        let last_px = report.last_px.unwrap_or(0);
        let expected_leaves = order.leaves_qty - last_qty;
        let expected_cum = order.cum_qty + last_qty;
        let limit_ok = match (order.side, order.price) {
            (Side::Buy, Some(limit)) => last_px <= limit,
            (Side::Sell, Some(limit)) => last_px >= limit,
            _ => true,
        };
        let finished_already = order.finished;
        let ord_qty = order.ord_qty;
        let status_ok = if expected_leaves == 0 {
            report.ord_status == OrdStatus::Filled
        } else {
            report.ord_status == OrdStatus::PartiallyFilled
        };

        order.leaves_qty = report.leaves_qty;
        order.cum_qty = report.cum_qty;
        order.finished = report.leaves_qty == 0;
        self.stats.filled_qty += i128::from(last_qty);

        let (exec_id, order_id) = (report.exec_id, report.order_id);
        if finished_already {
            self.violation("no_activity_after_finish", exec_id, order_id, "traded".to_string());
        }
        if last_qty <= 0 {
            self.violation("positive_quantity", exec_id, order_id, format!("last_qty {last_qty}"));
        }
        if report.leaves_qty != expected_leaves
            || report.cum_qty != expected_cum
            || report.cum_qty + report.leaves_qty != ord_qty
        {
            self.violation(
                "quantities_add_up",
                exec_id,
                order_id,
                format!(
                    "ord {ord_qty} last {last_qty}: expected leaves {expected_leaves} cum \
                     {expected_cum}, got leaves {} cum {}",
                    report.leaves_qty, report.cum_qty
                ),
            );
        }
        if !limit_ok {
            self.violation("limit_respected", exec_id, order_id, format!("traded at {last_px}"));
        }
        if !status_ok {
            self.violation(
                "status_matches_quantities",
                exec_id,
                order_id,
                format!("{:?} with leaves {}", report.ord_status, report.leaves_qty),
            );
        }
    }

    fn check_pair(&mut self, first: &Execution, second: &Execution) {
        let (exec_id, order_id) = (second.exec_id, second.order_id);
        let same_match = first.match_id == second.match_id
            && first.last_qty == second.last_qty
            && first.last_px == second.last_px;
        if !same_match {
            self.violation(
                "trade_pairs",
                exec_id,
                order_id,
                format!("{:?} does not match {:?}", second.match_id, first.match_id),
            );
        }
        if first.side == second.side {
            self.violation("trade_pairs", exec_id, order_id, "same side".to_string());
        }
        if first.symbol != second.symbol {
            self.violation(
                "same_symbol",
                exec_id,
                order_id,
                format!("{} traded with {}", first.symbol, second.symbol),
            );
        }
        if first.account == second.account {
            self.violation("no_self_trade", exec_id, order_id, first.account.clone());
        }
    }

    /// The second report of a pair is the resting order. It must be first in line at the best
    /// price on its side.
    fn check_priority(&mut self, resting: &Execution, last_px: i64) {
        let (exec_id, order_id) = (resting.exec_id, resting.order_id);
        let best = match resting.side {
            Side::Buy => self.best_bid(),
            Side::Sell => self.best_ask(),
            Side::Unknown => None,
        };
        if resting.price != Some(last_px) {
            self.violation(
                "trade_at_resting_price",
                exec_id,
                order_id,
                format!("price {:?} traded at {last_px}", resting.price),
            );
        }
        if best != resting.price {
            self.violation(
                "price_priority",
                exec_id,
                order_id,
                format!("best is {best:?}, filled {:?}", resting.price),
            );
        }

        let Some(price) = resting.price else { return };
        let levels = if resting.side == Side::Buy { &mut self.bids } else { &mut self.asks };
        let Some(queue) = levels.get_mut(&price) else {
            self.violation("order_in_book", exec_id, order_id, format!("no level at {price}"));
            return;
        };

        let front = queue.front().copied();
        if resting.leaves_qty == 0 {
            queue.retain(|id| *id != order_id);
        }
        if queue.is_empty() {
            levels.remove(&price);
        }
        if front != Some(order_id) {
            self.violation("time_priority", exec_id, order_id, format!("front was {front:?}"));
        }
    }

    fn on_canceled(&mut self, report: &Execution) {
        let is_aggressor = self.aggressor == Some(report.order_id);
        if !is_aggressor {
            self.settle(report.exec_id, report.order_id);
            self.consumed_order_ids += 1;
        } else if let Some(open) = self.open_trade.take() {
            self.violation(
                "trade_pairs",
                report.exec_id,
                open.order_id,
                format!("match {:?} had only one side", open.match_id),
            );
        }

        let Some(order) = self.orders.get_mut(&report.order_id) else {
            self.violation(
                "known_order",
                report.exec_id,
                report.order_id,
                "cancel for an unknown order".to_string(),
            );
            return;
        };

        let finished_already = order.finished;
        let unchanged = order.leaves_qty == report.leaves_qty && order.cum_qty == report.cum_qty;
        let (side, price) = (order.side, order.price);
        order.finished = true;
        self.stats.canceled += 1;
        self.stats.canceled_qty += i128::from(report.leaves_qty);

        if is_aggressor {
            self.aggressor = None;
        } else if let Some(price) = price {
            let levels = if side == Side::Buy { &mut self.bids } else { &mut self.asks };
            if let Some(queue) = levels.get_mut(&price) {
                queue.retain(|id| *id != report.order_id);
                if queue.is_empty() {
                    levels.remove(&price);
                }
            }
        }

        if finished_already {
            self.violation(
                "no_activity_after_finish",
                report.exec_id,
                report.order_id,
                "canceled".to_string(),
            );
        }
        if !unchanged {
            self.violation(
                "cancel_keeps_quantities",
                report.exec_id,
                report.order_id,
                format!("leaves {} cum {}", report.leaves_qty, report.cum_qty),
            );
        }
        if report.ord_status != OrdStatus::Canceled {
            self.violation(
                "status_matches_quantities",
                report.exec_id,
                report.order_id,
                format!("Canceled report with status {:?}", report.ord_status),
            );
        }
    }

    /// Closes out the run and checks that all quantity is accounted for.
    pub fn finish(&mut self) {
        self.settle(self.last_exec_id, 0);

        let resting: i128 = self
            .orders
            .values()
            .filter(|order| !order.finished)
            .map(|order| i128::from(order.leaves_qty))
            .sum();
        let accounted = self.stats.filled_qty + self.stats.canceled_qty + resting;
        if accounted != self.stats.submitted_qty {
            self.violation(
                "quantity_conserved",
                self.last_exec_id,
                0,
                format!(
                    "submitted {} but filled {} + canceled {} + resting {resting}",
                    self.stats.submitted_qty, self.stats.filled_qty, self.stats.canceled_qty
                ),
            );
        }
    }

    pub fn best_bid(&self) -> Option<i64> {
        self.bids.keys().next_back().copied()
    }

    pub fn best_ask(&self) -> Option<i64> {
        self.asks.keys().next().copied()
    }

    fn level_qty(&self, queue: &VecDeque<u64>) -> i128 {
        queue
            .iter()
            .filter_map(|id| self.orders.get(id))
            .map(|order| i128::from(order.leaves_qty))
            .sum()
    }

    /// Returns `(price, quantity, orders)` from the best price outward.
    pub fn top_of_book(&self, side: Side, depth: usize) -> Vec<(i64, i128, usize)> {
        let row = |(price, queue): (&i64, &VecDeque<u64>)| {
            (*price, self.level_qty(queue), queue.len())
        };
        match side {
            Side::Buy => self.bids.iter().rev().take(depth).map(row).collect(),
            _ => self.asks.iter().take(depth).map(row).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: i64 = crate::event::SCALE;

    fn report(exec_id: u64, order_id: u64, exec_type: ExecType, side: Side, price: Option<i64>) -> Execution {
        Execution {
            exec_id,
            order_id,
            exec_type,
            ord_status: OrdStatus::New,
            side,
            symbol: "JSP".to_string(),
            account: format!("account-{order_id}"),
            cl_ord_id: format!("order-{order_id}"),
            transact_time: exec_id,
            price,
            ord_qty: 10 * Q,
            leaves_qty: 10 * Q,
            cum_qty: 0,
            last_qty: None,
            last_px: None,
            match_id: None,
        }
    }

    fn new(exec_id: u64, order_id: u64, side: Side, price: i64) -> Event {
        Event::Execution(report(exec_id, order_id, ExecType::New, side, Some(price)))
    }

    fn fill(exec_id: u64, order_id: u64, side: Side, price: i64, qty: i64, cum: i64, px: i64, match_id: u64) -> Event {
        let mut r = report(exec_id, order_id, ExecType::Trade, side, Some(price));
        r.last_qty = Some(qty * Q);
        r.last_px = Some(px);
        r.match_id = Some(match_id);
        r.cum_qty = cum * Q;
        r.leaves_qty = (10 - cum) * Q;
        r.ord_status = if cum == 10 { OrdStatus::Filled } else { OrdStatus::PartiallyFilled };
        Event::Execution(r)
    }

    fn run(events: &[Event]) -> Checker {
        let mut checker = Checker::new();
        for event in events {
            checker.apply(event);
        }
        checker.finish();
        checker
    }

    fn rules(checker: &Checker) -> Vec<&'static str> {
        checker.violations.iter().map(|v| v.rule).collect()
    }

    fn partial_cross() -> Vec<Event> {
        let mut events = vec![
            new(1, 1, Side::Buy, 100),
            new(2, 2, Side::Sell, 100),
            fill(3, 2, Side::Sell, 100, 4, 4, 100, 1),
            fill(4, 1, Side::Buy, 100, 4, 4, 100, 1),
        ];
        // The sell is for 4, so it fills completely and only the buy rests.
        if let Event::Execution(r) = &mut events[1] {
            r.ord_qty = 4 * Q;
            r.leaves_qty = 4 * Q;
        }
        if let Event::Execution(r) = &mut events[2] {
            r.ord_qty = 4 * Q;
            r.leaves_qty = 0;
            r.ord_status = OrdStatus::Filled;
        }
        events
    }

    #[test]
    fn accepts_a_valid_cross() {
        let checker = run(&partial_cross());
        assert!(checker.is_clean(), "{:?}", checker.violations);
        assert_eq!(checker.stats.trades, 1);
        assert_eq!(checker.top_of_book(Side::Buy, 1), vec![(100, i128::from(6 * Q), 1)]);
        assert!(checker.top_of_book(Side::Sell, 1).is_empty());
    }

    #[test]
    fn accepts_a_market_order_with_no_liquidity() {
        let mut market = report(2, 1, ExecType::Canceled, Side::Buy, None);
        market.ord_status = OrdStatus::Canceled;
        let checker = run(&[
            Event::Execution(report(1, 1, ExecType::New, Side::Buy, None)),
            Event::Execution(market),
        ]);
        assert!(checker.is_clean(), "{:?}", checker.violations);
        assert_eq!(checker.stats.canceled_qty, i128::from(10 * Q));
    }

    #[test]
    fn accepts_order_ids_consumed_by_cancel_rejects() {
        let reject = Event::CancelReject { cl_ord_id: "a".into(), orig_cl_ord_id: "b".into() };
        let checker = run(&[new(1, 1, Side::Buy, 100), reject, new(3, 3, Side::Buy, 99)]);
        assert!(checker.is_clean(), "{:?}", checker.violations);
    }

    #[test]
    fn flags_exec_id_gaps() {
        let checker = run(&[new(1, 1, Side::Buy, 100), new(3, 2, Side::Buy, 99)]);
        assert_eq!(rules(&checker), ["exec_id_sequence"]);
    }

    #[test]
    fn flags_joining_late() {
        let checker = run(&[new(5, 5, Side::Buy, 100)]);
        assert!(checker.violations[0].detail.contains("started after the engine"));
    }

    #[test]
    fn flags_order_id_gaps() {
        let checker = run(&[new(1, 1, Side::Buy, 100), new(2, 3, Side::Buy, 99)]);
        assert_eq!(rules(&checker), ["order_id_sequence"]);
    }

    #[test]
    fn flags_a_crossed_book() {
        let checker = run(&[new(1, 1, Side::Buy, 100), new(2, 2, Side::Sell, 90), new(3, 3, Side::Buy, 1)]);
        assert_eq!(rules(&checker), ["book_uncrossed", "book_uncrossed"].to_vec());
    }

    #[test]
    fn flags_a_one_sided_trade() {
        let mut events = partial_cross();
        events.pop();
        events.push(new(4, 3, Side::Buy, 99));
        assert!(rules(&run(&events)).contains(&"trade_pairs"));
    }

    #[test]
    fn flags_mismatched_pair_quantities() {
        let mut events = partial_cross();
        events[3] = fill(4, 1, Side::Buy, 100, 3, 3, 100, 1);
        assert!(rules(&run(&events)).contains(&"trade_pairs"));
    }

    #[test]
    fn flags_quantities_that_dont_add_up() {
        let mut events = partial_cross();
        if let Event::Execution(r) = &mut events[2] {
            r.leaves_qty = 7 * Q;
        }
        assert!(rules(&run(&events)).contains(&"quantities_add_up"));
    }

    #[test]
    fn flags_a_wrong_status() {
        let mut events = partial_cross();
        if let Event::Execution(r) = &mut events[3] {
            r.ord_status = OrdStatus::Filled;
        }
        assert!(rules(&run(&events)).contains(&"status_matches_quantities"));
    }

    #[test]
    fn flags_a_price_outside_the_limit() {
        let events = vec![
            new(1, 1, Side::Buy, 100),
            new(2, 2, Side::Sell, 100),
            fill(3, 2, Side::Sell, 100, 4, 4, 99, 1),
            fill(4, 1, Side::Buy, 100, 4, 4, 99, 1),
        ];
        let rules = rules(&run(&events));
        assert!(rules.contains(&"limit_respected"), "{rules:?}");
        assert!(rules.contains(&"trade_at_resting_price"), "{rules:?}");
    }

    #[test]
    fn flags_price_priority() {
        let events = vec![
            new(1, 1, Side::Buy, 100),
            new(2, 2, Side::Buy, 101),
            new(3, 3, Side::Sell, 100),
            fill(4, 3, Side::Sell, 100, 4, 4, 100, 1),
            fill(5, 1, Side::Buy, 100, 4, 4, 100, 1),
        ];
        assert!(rules(&run(&events)).contains(&"price_priority"));
    }

    #[test]
    fn flags_time_priority() {
        let events = vec![
            new(1, 1, Side::Buy, 100),
            new(2, 2, Side::Buy, 100),
            new(3, 3, Side::Sell, 100),
            fill(4, 3, Side::Sell, 100, 4, 4, 100, 1),
            fill(5, 2, Side::Buy, 100, 4, 4, 100, 1),
        ];
        assert!(rules(&run(&events)).contains(&"time_priority"));
    }

    #[test]
    fn flags_a_self_trade() {
        let mut events = partial_cross();
        if let Event::Execution(r) = &mut events[3] {
            r.account = "account-2".to_string();
        }
        assert!(rules(&run(&events)).contains(&"no_self_trade"));
    }

    #[test]
    fn flags_trades_across_symbols() {
        let mut events = partial_cross();
        if let Event::Execution(r) = &mut events[3] {
            r.symbol = "OTHER".to_string();
        }
        assert!(rules(&run(&events)).contains(&"same_symbol"));
    }

    #[test]
    fn flags_activity_after_finish() {
        let mut filled = partial_cross();
        filled[2] = fill(3, 2, Side::Sell, 100, 10, 10, 100, 1);
        filled[3] = fill(4, 1, Side::Buy, 100, 10, 10, 100, 1);
        filled.push(new(5, 3, Side::Sell, 100));
        filled.push(fill(6, 3, Side::Sell, 100, 1, 1, 100, 2));
        filled.push(fill(7, 1, Side::Buy, 100, 1, 11, 100, 2));
        assert!(rules(&run(&filled)).contains(&"no_activity_after_finish"));
    }

    #[test]
    fn flags_lost_quantity() {
        let mut events = partial_cross();
        if let Event::Execution(r) = &mut events[1] {
            r.ord_qty = 20 * Q;
            r.leaves_qty = 20 * Q;
        }
        let mut checker = Checker::new();
        for event in &events {
            checker.apply(event);
        }
        checker.stats.submitted_qty += i128::from(Q);
        checker.finish();
        assert!(rules(&checker).contains(&"quantity_conserved"));
    }
}
