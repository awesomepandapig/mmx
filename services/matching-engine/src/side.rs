use crate::orderbook::OrderBook;
use crate::types::{Order, SymbolType};

pub struct Buy;
pub struct Sell;

pub trait SideSpecificContext {
    fn get_best_opposite<'a>(book: &'a mut OrderBook, symbol: &SymbolType)
    -> Option<&'a mut Order>;
    fn can_cross(aggressor_price: i64, resting_price: i64) -> bool;
    fn add_to_book(book: &mut OrderBook, order: Order);
}

impl SideSpecificContext for Buy {
    #[inline(always)]
    fn get_best_opposite<'a>(
        book: &'a mut OrderBook,
        symbol: &SymbolType,
    ) -> Option<&'a mut Order> {
        book.best_ask(symbol)
    }

    #[inline(always)]
    fn can_cross(aggressor_price: i64, resting_price: i64) -> bool {
        aggressor_price >= resting_price
    }

    #[inline(always)]
    fn add_to_book(book: &mut OrderBook, order: Order) {
        book.add_bid(&order);
    }
}

impl SideSpecificContext for Sell {
    #[inline(always)]
    fn get_best_opposite<'a>(
        book: &'a mut OrderBook,
        symbol: &SymbolType,
    ) -> Option<&'a mut Order> {
        book.best_bid(symbol)
    }

    #[inline(always)]
    fn can_cross(aggressor_price: i64, resting_price: i64) -> bool {
        aggressor_price <= resting_price
    }

    #[inline(always)]
    fn add_to_book(book: &mut OrderBook, order: Order) {
        book.add_ask(&order);
    }
}
