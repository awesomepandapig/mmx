use crate::types::{Order, OrderKey, SymbolType};

use slab::Slab;
use foldhash::{HashMap, HashMapExt};
use std::collections::BTreeSet;

use sbe::side_enum::SideEnum;

const PRICE_LEVEL_CAPACITY: usize = 1024;

pub struct Limit {
    head_order_idx: Option<usize>,
    tail_order_idx: Option<usize>,
}

impl Limit {
    const fn new() -> Self {
        Self {
            head_order_idx: None,
            tail_order_idx: None,
        }
    }
}

/// The price levels of one symbol.
pub struct Levels {
    pub bids_price_tree: BTreeSet<i64>,
    pub asks_price_tree: BTreeSet<i64>,
    pub bids_price_map: HashMap<i64, Limit>,
    pub asks_price_map: HashMap<i64, Limit>,
}

impl Levels {
    fn new() -> Self {
        Self {
            bids_price_tree: BTreeSet::new(),
            asks_price_tree: BTreeSet::new(),
            bids_price_map: HashMap::with_capacity(PRICE_LEVEL_CAPACITY),
            asks_price_map: HashMap::with_capacity(PRICE_LEVEL_CAPACITY),
        }
    }
}

/// All symbols share one order pool and key map, so `capacity` bounds the whole engine.
pub struct OrderBook {
    pub pool: Slab<Order>,
    pub order_key_map: HashMap<OrderKey, usize>,
    pub levels: HashMap<SymbolType, Levels>,
}

impl OrderBook {
    pub fn new(capacity: usize) -> Self {
        Self {
            pool: Slab::with_capacity(capacity),
            order_key_map: HashMap::new(),
            levels: HashMap::new(),
        }
    }

    pub fn is_full(&self) -> bool {
        self.pool.len() >= self.pool.capacity()
    }

    /// Adds a new buy order to the book.
    pub fn add_bid(&mut self, order: &Order) {
        let order_key = order.key();

        let order_idx = self.pool.insert(*order);
        self.order_key_map.insert(order_key, order_idx);
        let levels = self.levels.entry(order.symbol).or_insert_with(Levels::new);

        if let Some(limit) = levels.bids_price_map.get_mut(&order.price) {
            let tail_idx = limit
                .tail_order_idx
                .expect("Inconsistency: Existing limit cannot be empty"); // TODO: Handle error
            self.pool[tail_idx].next_order_idx = Some(order_idx);
            self.pool[order_idx].prev_order_idx = Some(tail_idx);
            limit.tail_order_idx = Some(order_idx);
        } else {
            let mut new_limit = Limit::new();
            new_limit.head_order_idx = Some(order_idx);
            new_limit.tail_order_idx = Some(order_idx);
            levels.bids_price_map.insert(order.price, new_limit);
            levels.bids_price_tree.insert(order.price);
        }
    }

    /// Adds a new sell order to the book.
    pub fn add_ask(&mut self, order: &Order) {
        let order_key = order.key();
        let order_idx = self.pool.insert(*order);
        self.order_key_map.insert(order_key, order_idx);
        let levels = self.levels.entry(order.symbol).or_insert_with(Levels::new);

        if let Some(limit) = levels.asks_price_map.get_mut(&order.price) {
            let tail_idx = limit
                .tail_order_idx
                .expect("Inconsistency: Existing limit cannot be empty"); // TODO: Handle error
            self.pool[tail_idx].next_order_idx = Some(order_idx);
            self.pool[order_idx].prev_order_idx = Some(tail_idx);
            limit.tail_order_idx = Some(order_idx);
        } else {
            let mut new_limit = Limit::new();
            new_limit.head_order_idx = Some(order_idx);
            new_limit.tail_order_idx = Some(order_idx);
            levels.asks_price_map.insert(order.price, new_limit);
            levels.asks_price_tree.insert(order.price);
        }
    }

    pub fn remove(&mut self, order_key: OrderKey) -> Order {
        // Remove the order key from the order_key_map to get its stable index
        let order_idx = self
            .order_key_map
            .remove(&order_key)
            .expect("Cannot cancel order: order_key not found in order_id_map"); // TODO: Handle error

        // Remove the order from the memory pool
        let order = self.pool.remove(order_idx);

        // Get appropriate structs based on side
        let levels = self
            .levels
            .get_mut(&order.symbol)
            .expect("Data inconsistency: no levels for a resting order's symbol"); // TODO: Handle error
        let (price_map, tree) = if order.side == SideEnum::Buy {
            (&mut levels.bids_price_map, &mut levels.bids_price_tree)
        } else {
            (&mut levels.asks_price_map, &mut levels.asks_price_tree)
        };

        let limit = price_map
            .get_mut(&order.price)
            .expect("Data inconsistency: price level not found for a valid order"); // TODO: Handle error

        // Unlink the order from the doubly-linked list.
        match (order.prev_order_idx, order.next_order_idx) {
            (Some(prev), Some(next)) => {
                // Middle of the list
                self.pool[prev].next_order_idx = Some(next);
                self.pool[next].prev_order_idx = Some(prev);
            }
            (Some(prev), _) => {
                // Tail of the list
                self.pool[prev].next_order_idx = None;
                limit.tail_order_idx = Some(prev);
            }
            (_, Some(next)) => {
                // Head of the list
                self.pool[next].prev_order_idx = None;
                limit.head_order_idx = Some(next);
            }
            (_, _) => {
                // Only order at this price level
                // This is the O(log N) path.
                price_map.remove(&order.price);
                tree.remove(&order.price);
            }
        }

        order
    }

    /// Returns a mutable reference to the best bid order, if one exists.
    /// This allows for in-place modification of the order.
    /// # Time Complexity: O(log N)
    pub fn best_bid(&mut self, symbol: &SymbolType) -> Option<&mut Order> {
        let levels = self.levels.get(symbol)?;
        if let Some(&best_price) = levels.bids_price_tree.last() {
            let head_idx = levels
                .bids_price_map
                .get(&best_price)
                .expect("Inconsistency: sorted_prices has a price that price_map does not") // TODO: Handle error
                .head_order_idx
                .expect("Inconsistency: Price level exists but has no orders"); // TODO: Handle error

            self.pool.get_mut(head_idx)
        } else {
            None
        }
    }

    /// Returns a mutable reference to the best ask/offer order, if one exists.
    /// This allows for in-place modification of the order.
    /// # Time Complexity: O(log N)
    pub fn best_ask(&mut self, symbol: &SymbolType) -> Option<&mut Order> {
        let levels = self.levels.get(symbol)?;
        if let Some(&best_price) = levels.asks_price_tree.first() {
            let head_idx = levels
                .asks_price_map
                .get(&best_price)
                .expect("Inconsistency: sorted_prices has a price that price_map does not") // TODO: Handle error
                .head_order_idx
                .expect("Inconsistency: Price level exists but has no orders"); // TODO: Handle error

            self.pool.get_mut(head_idx)
        } else {
            None
        }
    }
}
