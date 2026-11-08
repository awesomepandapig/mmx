use sbe::ord_type_enum::OrdTypeEnum;
use sbe::side_enum::SideEnum;

#[derive(Debug, Clone, Copy)]
pub struct Order {
    // Hot fields (accessed frequently during matching) - first cache line (64 bytes)
    pub prev_order_idx: Option<usize>, // 8 bytes -
    pub next_order_idx: Option<usize>, // 8 bytes -

    pub leaves_quantity: i64, // 8 bytes - Remaining quantity to be filled
    pub price: i64,           // 8 bytes - Price for Limit orders
    pub cumulative_quantity: i64, // 8 bytes - Cumulative quantity filled
    pub total_notional: i128, // 16 bytes - Total value of fills
    pub sequence_number: u64, // 8 bytes - Monotonic identifier for order
    pub quantity: i64,        // 8 bytes - Original quantity of the order
    pub side: SideEnum,       // 1 bytes - Buy or Sell
    pub r#type: OrdTypeEnum,  // 1 bytes - Limit or Market
    // 64 bytes total for first cache line

    // Cold fields (rarely accessed during matching) - subsequent cache lines
    pub client_order_id: UuidType, // 16 bytes - Client Order ID
    pub account: UuidType,         // 16 bytes - Account ID
    pub transact_time: u64,        // 8 bytes - Time of transaction from client
    pub symbol: SymbolType,        // 6 bytes - Instrument symbol
}

impl Order {
    pub const fn empty() -> Self {
        Self {
            prev_order_idx: None,
            next_order_idx: None,
            leaves_quantity: 0,
            price: 0,
            cumulative_quantity: 0,
            total_notional: 0,
            sequence_number: 0,
            quantity: 0,
            side: SideEnum::NullVal,
            r#type: OrdTypeEnum::NullVal,
            client_order_id: 0,
            account: 0,
            transact_time: 0,
            symbol: [0; 6],
        }
    }

    pub const fn key(&self) -> OrderKey {
        (self.account, self.client_order_id)
    }

    pub fn fill(&mut self, qty: i64, price: i64) {
        self.cumulative_quantity += qty;
        self.leaves_quantity -= qty;
        self.total_notional += i128::from(qty) * i128::from(price);
    }

    pub fn avg_px(&self) -> i64 {
        if self.cumulative_quantity == 0 {
            return 0;
        }
        let avg = self.total_notional / i128::from(self.cumulative_quantity);
        i64::try_from(avg).expect("avg_px: VWAP out of i64 range — invariant broken") // TODO: NO EXPECTS
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CancelRequest {
    pub original_client_order_id: UuidType, // 16 bytes - Original Client Order ID
    pub client_order_id: UuidType,          // 16 bytes - Client Order ID
    pub account: UuidType,                  // 16 bytes - Account ID
                                            // pub transact_time: u64,                 // 8 bytes - Time of transaction from client
                                            // pub symbol: SymbolType,                 // 6 bytes - Instrument symbol
                                            // pub side: SideEnum, // 1 bytes - Buy or Sell
}

pub const ORDER_RECORD: usize = 104;

/// Writes the fields that define a resting order. Queue links are rebuilt on load.
#[inline(always)]
pub fn encode_order(order: &Order, record: &mut [u8; ORDER_RECORD]) {
    record[0..8].copy_from_slice(&order.price.to_le_bytes());
    record[8..16].copy_from_slice(&order.quantity.to_le_bytes());
    record[16..24].copy_from_slice(&order.leaves_quantity.to_le_bytes());
    record[24..32].copy_from_slice(&order.cumulative_quantity.to_le_bytes());
    record[32..48].copy_from_slice(&order.total_notional.to_le_bytes());
    record[48..56].copy_from_slice(&order.sequence_number.to_le_bytes());
    record[56..72].copy_from_slice(&order.client_order_id.to_le_bytes());
    record[72..88].copy_from_slice(&order.account.to_le_bytes());
    record[88..96].copy_from_slice(&order.transact_time.to_le_bytes());
    record[96..102].copy_from_slice(&order.symbol);
    record[102] = order.side as u8;
    record[103] = order.r#type as u8;
}

pub fn decode_order(record: &[u8]) -> Order {
    let i64_at =
        |at: usize| i64::from_le_bytes(record[at..at + 8].try_into().expect("eight bytes"));
    let u64_at =
        |at: usize| u64::from_le_bytes(record[at..at + 8].try_into().expect("eight bytes"));
    let u128_at =
        |at: usize| u128::from_le_bytes(record[at..at + 16].try_into().expect("sixteen bytes"));
    let mut order = Order::empty();
    order.price = i64_at(0);
    order.quantity = i64_at(8);
    order.leaves_quantity = i64_at(16);
    order.cumulative_quantity = i64_at(24);
    order.total_notional = u128_at(32) as i128;
    order.sequence_number = u64_at(48);
    order.client_order_id = u128_at(56);
    order.account = u128_at(72);
    order.transact_time = u64_at(88);
    order.symbol.copy_from_slice(&record[96..102]);
    order.side = SideEnum::from(record[102]);
    order.r#type = OrdTypeEnum::from(record[103]);
    order
}

pub type UuidType = u128;
pub type SymbolType = [u8; 6];
pub type OrderKey = (UuidType, UuidType);
