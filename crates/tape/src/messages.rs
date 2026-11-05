use sbe::{
    ReadBuf, exec_type_enum::ExecTypeEnum, execution_report_codec::ExecutionReportDecoder,
    message_header_codec::MessageHeaderDecoder, ord_rej_reason_enum::OrdRejReasonEnum,
    ord_status_enum::OrdStatusEnum, side_enum::SideEnum,
};

pub type UuidType = [u64; 2];
pub type SymbolType = [u8; 6];

/// Prices and quantities with no value, such as the price on a market order.
pub const NULL_DECIMAL: i64 = i64::MIN;

#[derive(Debug, Clone)]
pub struct ExecutionReportMessage {
    pub account: UuidType,
    pub cl_ord_id: UuidType,
    pub trd_match_id: Option<u64>,
    pub order_id: u64,
    pub exec_id: u64,
    pub transact_time: u64,
    pub price: i64,
    pub ord_qty: i64,
    pub last_qty: i64,
    pub last_px: i64,
    pub leaves_qty: i64,
    pub cum_qty: i64,
    pub avg_px: i64,
    pub symbol: SymbolType,
    pub exec_type: ExecTypeEnum,
    pub ord_status: OrdStatusEnum,
    pub ord_rej_reason: OrdRejReasonEnum,
    pub side: SideEnum,
}

pub fn symbol_name(symbol: &SymbolType) -> String {
    String::from_utf8_lossy(symbol)
        .trim_end_matches(['\0', ' '])
        .to_string()
}

/// The padded form of a symbol, or `None` if it doesn't fit.
pub fn symbol_bytes(name: &str) -> Option<SymbolType> {
    let bytes = name.as_bytes();
    if bytes.len() > 6 {
        return None;
    }
    let mut symbol = [0u8; 6];
    symbol[..bytes.len()].copy_from_slice(bytes);
    Some(symbol)
}

pub fn decode_execution_report(
    header_decoder: MessageHeaderDecoder<ReadBuf<'_>>,
) -> ExecutionReportMessage {
    let execution_report_decoder: ExecutionReportDecoder<'_> =
        ExecutionReportDecoder::default().header(header_decoder, 0);

    ExecutionReportMessage {
        account: execution_report_decoder.account(),
        cl_ord_id: execution_report_decoder.cl_ord_id(),
        trd_match_id: execution_report_decoder.trd_match_id(),
        order_id: execution_report_decoder.order_id(),
        exec_id: execution_report_decoder.exec_id(),
        transact_time: execution_report_decoder.transact_time_decoder().time(),
        price: execution_report_decoder.price_decoder().mantissa(),
        ord_qty: execution_report_decoder.order_qty_decoder().mantissa(),
        last_qty: execution_report_decoder.last_qty_decoder().mantissa(),
        last_px: execution_report_decoder.last_px_decoder().mantissa(),
        leaves_qty: execution_report_decoder.leaves_qty_decoder().mantissa(),
        cum_qty: execution_report_decoder.cum_qty_decoder().mantissa(),
        avg_px: execution_report_decoder.avg_px_decoder().mantissa(),
        symbol: execution_report_decoder.symbol(),
        exec_type: execution_report_decoder.exec_type(),
        ord_status: execution_report_decoder.ord_status(),
        ord_rej_reason: execution_report_decoder.ord_rej_reason(),
        side: execution_report_decoder.side(),
    }
}

/// Stores a map keyed by symbol as pairs, because JSON keys have to be strings.
pub mod symbol_map {
    use std::collections::HashMap;

    use serde::de::{Deserialize, Deserializer};
    use serde::ser::{Serialize, Serializer};

    use super::SymbolType;

    pub fn serialize<S: Serializer, V: Serialize>(
        map: &HashMap<SymbolType, V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(map.iter())
    }

    pub fn deserialize<'de, D: Deserializer<'de>, V: Deserialize<'de>>(
        deserializer: D,
    ) -> Result<HashMap<SymbolType, V>, D::Error> {
        let pairs = Vec::<(SymbolType, V)>::deserialize(deserializer)?;
        Ok(pairs.into_iter().collect())
    }
}

/// A resting limit buy for tests to adjust.
#[doc(hidden)]
pub fn sample_report() -> ExecutionReportMessage {
    ExecutionReportMessage {
        account: [0, 1],
        cl_ord_id: [0, 1],
        trd_match_id: None,
        order_id: 1,
        exec_id: 1,
        transact_time: 0,
        price: 100,
        ord_qty: 10,
        last_qty: NULL_DECIMAL,
        last_px: NULL_DECIMAL,
        leaves_qty: 10,
        cum_qty: 0,
        avg_px: NULL_DECIMAL,
        symbol: *b"ITEM\0\0",
        exec_type: ExecTypeEnum::New,
        ord_status: OrdStatusEnum::New,
        ord_rej_reason: OrdRejReasonEnum::NullVal,
        side: SideEnum::Buy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_round_trip_through_their_padded_form() {
        let symbol = symbol_bytes("ITEM").expect("fits");
        assert_eq!(symbol, *b"ITEM\0\0");
        assert_eq!(symbol_name(&symbol), "ITEM");
        assert_eq!(symbol_bytes("TOOLONG"), None);
    }
}
