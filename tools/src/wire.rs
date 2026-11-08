use sbe::WriteBuf;
use sbe::message_header_codec::ENCODED_LENGTH;
use sbe::new_order_single_codec::{self, NewOrderSingleEncoder};
use sbe::ord_type_enum::OrdTypeEnum;
use sbe::order_cancel_request_codec::{self, OrderCancelRequestEncoder};
use sbe::side_enum::SideEnum;

pub const NEW_ORDER_LENGTH: usize =
    new_order_single_codec::SBE_BLOCK_LENGTH as usize + ENCODED_LENGTH;
pub const CANCEL_LENGTH: usize =
    order_cancel_request_codec::SBE_BLOCK_LENGTH as usize + ENCODED_LENGTH;

pub struct NewOrder {
    pub cl_ord_id: [u64; 2],
    pub account: [u64; 2],
    pub symbol: [u8; 6],
    pub side: SideEnum,
    pub ord_type: OrdTypeEnum,
    pub transact_time: u64,
    pub qty: i64,
    pub price: i64,
}

pub fn encode_new_order(order: &NewOrder, buffer: &mut [u8]) {
    let mut encoder = NewOrderSingleEncoder::default().wrap(WriteBuf::new(buffer), ENCODED_LENGTH);
    let mut header = encoder.header(0);
    encoder = header.parent().expect("header encoder has a parent");

    encoder.cl_ord_id(&order.cl_ord_id);
    encoder.account(&order.account);
    encoder.symbol(&order.symbol);
    encoder.side(order.side);

    let mut time = encoder.transact_time_encoder();
    time.time(order.transact_time);
    encoder = time.parent().expect("time encoder has a parent");

    encoder.ord_type(order.ord_type);

    let mut qty = encoder.order_qty_encoder();
    qty.mantissa(order.qty);
    encoder = qty.parent().expect("qty encoder has a parent");

    let mut price = encoder.price_encoder();
    price.mantissa(order.price);
    price.parent().expect("price encoder has a parent");
}

pub struct CancelOrder {
    pub orig_cl_ord_id: [u64; 2],
    pub cl_ord_id: [u64; 2],
    pub account: [u64; 2],
    pub symbol: [u8; 6],
    pub side: SideEnum,
    pub transact_time: u64,
}

pub fn encode_cancel(cancel: &CancelOrder, buffer: &mut [u8]) {
    let mut encoder =
        OrderCancelRequestEncoder::default().wrap(WriteBuf::new(buffer), ENCODED_LENGTH);
    let mut header = encoder.header(0);
    encoder = header.parent().expect("header encoder has a parent");

    encoder.orig_cl_ord_id(&cancel.orig_cl_ord_id);
    encoder.cl_ord_id(&cancel.cl_ord_id);
    encoder.account(&cancel.account);
    encoder.symbol(&cancel.symbol);
    encoder.side(cancel.side);

    let mut time = encoder.transact_time_encoder();
    time.time(cancel.transact_time);
    time.parent().expect("time encoder has a parent");
}
