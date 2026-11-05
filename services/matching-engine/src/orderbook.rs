use crate::types::{Order, OrderKey, SymbolType};

use slab::Slab;
use foldhash::{HashMap, HashMapExt};
use std::collections::BTreeSet;

use sbe::side_enum::SideEnum;

/// Symbols are never removed, so going past this rehashes `trees` while matching.
const SYMBOL_CAPACITY: usize = 65_536;

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

/// Identifies one price level. Every level holds at least one resting order, so the book can't
/// have more levels than `capacity`, however many symbols there are.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct LevelKey {
    price: i64,
    symbol: SymbolType,
    is_bid: bool,
}

/// The sorted prices of one symbol. It's built the first time a symbol is seen and never removed.
#[derive(Default)]
pub struct PriceTrees {
    pub bids_price_tree: BTreeSet<i64>,
    pub asks_price_tree: BTreeSet<i64>,
}

/// All symbols share one order pool, key map and price map, so `capacity` bounds the whole engine
/// and adding a symbol only costs its trees.
pub struct OrderBook {
    pub pool: Slab<Order>,
    pub order_key_map: HashMap<OrderKey, usize>,
    price_map: HashMap<LevelKey, Limit>,
    pub trees: HashMap<SymbolType, PriceTrees>,
}

impl OrderBook {
    pub fn new(capacity: usize) -> Self {
        let mut book = Self {
            pool: Slab::with_capacity(capacity),
            order_key_map: HashMap::with_capacity(capacity),
            price_map: HashMap::with_capacity(capacity),
            trees: HashMap::with_capacity(SYMBOL_CAPACITY),
        };
        book.pretouch(capacity);
        book
    }

    /// Writes to the pool and maps so the OS must back them with memory now. A machine that's too
    /// small then fails at boot instead of while matching.
    fn pretouch(&mut self, capacity: usize) {
        for i in 0..capacity {
            self.pool.insert(Order::empty());
            self.order_key_map.insert((i as u128, 0), 0);
            let key = LevelKey { price: i as i64, symbol: [0; 6], is_bid: true };
            self.price_map.insert(key, Limit::new());
        }
        self.pool.clear();
        self.order_key_map.clear();
        self.price_map.clear();
    }

    /// Every resting order, in pool order. That order differs between nodes that loaded a
    /// snapshot and nodes that didn't, so never let it reach the output.
    pub fn resting(&self) -> impl Iterator<Item = &Order> {
        self.pool.iter().map(|(_, order)| order)
    }

    /// Empties the book, keeping its memory and every symbol's trees.
    pub fn reset(&mut self) {
        self.pool.clear();
        self.order_key_map.clear();
        self.price_map.clear();
        for trees in self.trees.values_mut() {
            trees.bids_price_tree.clear();
            trees.asks_price_tree.clear();
        }
    }

    /// Adds an order to the back of its queue.
    pub fn add(&mut self, order: &Order) {
        if order.side == SideEnum::Buy {
            self.add_bid(order);
        } else {
            self.add_ask(order);
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
        let key = LevelKey { price: order.price, symbol: order.symbol, is_bid: true };

        if let Some(limit) = self.price_map.get_mut(&key) {
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
            self.price_map.insert(key, new_limit);
            self.trees.entry(order.symbol).or_default().bids_price_tree.insert(order.price);
        }
    }

    /// Adds a new sell order to the book.
    pub fn add_ask(&mut self, order: &Order) {
        let order_key = order.key();
        let order_idx = self.pool.insert(*order);
        self.order_key_map.insert(order_key, order_idx);
        let key = LevelKey { price: order.price, symbol: order.symbol, is_bid: false };

        if let Some(limit) = self.price_map.get_mut(&key) {
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
            self.price_map.insert(key, new_limit);
            self.trees.entry(order.symbol).or_default().asks_price_tree.insert(order.price);
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

        let is_bid = order.side == SideEnum::Buy;
        let key = LevelKey { price: order.price, symbol: order.symbol, is_bid };

        let limit = self
            .price_map
            .get_mut(&key)
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
                self.price_map.remove(&key);
                let trees = self
                    .trees
                    .get_mut(&order.symbol)
                    .expect("Data inconsistency: no trees for a resting order's symbol"); // TODO: Handle error
                if is_bid {
                    trees.bids_price_tree.remove(&order.price);
                } else {
                    trees.asks_price_tree.remove(&order.price);
                }
            }
        }

        order
    }

    /// Returns a mutable reference to the best bid order, if one exists.
    /// This allows for in-place modification of the order.
    /// # Time Complexity: O(log N)
    pub fn best_bid(&mut self, symbol: &SymbolType) -> Option<&mut Order> {
        let trees = self.trees.get(symbol)?;
        if let Some(&best_price) = trees.bids_price_tree.last() {
            let key = LevelKey { price: best_price, symbol: *symbol, is_bid: true };
            let head_idx = self
                .price_map
                .get(&key)
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
        let trees = self.trees.get(symbol)?;
        if let Some(&best_price) = trees.asks_price_tree.first() {
            let key = LevelKey { price: best_price, symbol: *symbol, is_bid: false };
            let head_idx = self
                .price_map
                .get(&key)
                .expect("Inconsistency: sorted_prices has a price that price_map does not") // TODO: Handle error
                .head_order_idx
                .expect("Inconsistency: Price level exists but has no orders"); // TODO: Handle error

            self.pool.get_mut(head_idx)
        } else {
            None
        }
    }
}
