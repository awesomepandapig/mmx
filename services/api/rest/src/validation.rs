//! Everything an order must pass before it may reach the engine, which trusts what it receives.

use super::decimal;
use super::errors::AppError;
use super::identity::Player;
use super::markets::Markets;
use super::order::{SymbolType, UuidType, uuid_to_u64s};

use sbe::{ord_type_enum::OrdTypeEnum, side_enum::SideEnum};

use serde::Deserialize;

use uuid::Uuid;

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct CreateOrder {
    /// Chosen by the game server, so a retry can't create a second order.
    pub client_order_id: Option<Uuid>,
    pub product_id: String,
    pub side: String,
    pub r#type: String,
    pub size: String,
    pub price: Option<String>,
}

#[derive(Debug)]
pub struct ValidOrder {
    pub cl_ord_id: UuidType,
    pub account: UuidType,
    pub symbol: SymbolType,
    pub side: SideEnum,
    pub ord_type: OrdTypeEnum,
    pub qty_mantissa: i64,
    pub price_mantissa: i64,
}

fn validation_error(message: impl Into<String>) -> AppError {
    AppError::ValidationError(message.into())
}

pub fn validate_order(
    payload: &CreateOrder,
    player: Player,
    markets: &Markets,
    require_client_order_id: bool,
) -> Result<ValidOrder, AppError> {
    let cl_ord_id = match payload.client_order_id {
        Some(id) => uuid_to_u64s(id),
        None if require_client_order_id => {
            return Err(validation_error("client_order_id is required."));
        }
        None => uuid_to_u64s(Uuid::new_v4()),
    };

    let symbol = super::markets::normalize_symbol(&payload.product_id).map_err(validation_error)?;
    let market = markets
        .get(&symbol)
        .ok_or_else(|| validation_error(format!("Market '{symbol}' is not enabled.")))?;

    let side = match payload.side.to_lowercase().as_str() {
        "buy" => SideEnum::Buy,
        "sell" => SideEnum::Sell,
        _ => return Err(AppError::InvalidSide),
    };

    let ord_type = match payload.r#type.to_lowercase().as_str() {
        "limit" => OrdTypeEnum::Limit,
        "market" => OrdTypeEnum::Market,
        _ => return Err(AppError::InvalidOrderType),
    };

    let price_mantissa = match (ord_type, &payload.price) {
        (OrdTypeEnum::Limit, None) => {
            return Err(validation_error("Price is required for limit orders."));
        }
        (OrdTypeEnum::Limit, Some(price)) => {
            let price =
                decimal::parse(price).map_err(|err| validation_error(format!("Price {err}.")))?;
            if price == 0 {
                return Err(validation_error("Price must be positive for limit orders."));
            }
            if price % market.tick_size != 0 {
                return Err(validation_error(format!(
                    "Price must be a multiple of the tick size, {}.",
                    decimal::format(i128::from(market.tick_size))
                )));
            }
            price
        }
        (_, Some(_)) => {
            return Err(validation_error("Price should not be provided for market orders."));
        }
        (_, None) => i64::MIN, // Sentinel for market orders or when price is not applicable
    };

    let qty_mantissa =
        decimal::parse(&payload.size).map_err(|err| validation_error(format!("Size {err}.")))?;
    if qty_mantissa == 0 {
        return Err(validation_error("Size must be greater than 0."));
    }
    if qty_mantissa < market.min_size {
        return Err(validation_error(format!(
            "Size must be at least {}.",
            decimal::format(i128::from(market.min_size))
        )));
    }
    if qty_mantissa > market.max_size {
        return Err(validation_error(format!(
            "Size must be at most {}.",
            decimal::format(i128::from(market.max_size))
        )));
    }

    Ok(ValidOrder {
        cl_ord_id,
        account: player.account(),
        symbol: market.symbol,
        side,
        ord_type,
        qty_mantissa,
        price_mantissa,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKETS: &str = r#"
markets:
  - {symbol: ITEM, tick_size: "0.05", min_size: "1", max_size: "100"}
"#;

    fn limit(price: &str, size: &str) -> CreateOrder {
        CreateOrder {
            client_order_id: Some(Uuid::new_v4()),
            product_id: "item".to_string(),
            side: "buy".to_string(),
            r#type: "limit".to_string(),
            size: size.to_string(),
            price: Some(price.to_string()),
        }
    }

    fn check(order: &CreateOrder) -> Result<ValidOrder, String> {
        let markets = Markets::parse(MARKETS).unwrap();
        validate_order(order, Player(Uuid::nil()), &markets, true).map_err(|err| format!("{err:?}"))
    }

    #[test]
    fn accepts_a_price_on_the_tick() {
        let order = check(&limit("1.05", "2")).unwrap();
        assert_eq!(order.price_mantissa, 105_000_000);
        assert_eq!(order.qty_mantissa, 200_000_000);
        assert_eq!(&order.symbol, b"ITEM\0\0");
    }

    #[test]
    fn refuses_a_price_off_the_tick() {
        assert!(check(&limit("1.01", "2")).unwrap_err().contains("tick size, 0.05"));
    }

    #[test]
    fn the_account_is_the_player() {
        let player = Player(Uuid::new_v4());
        let markets = Markets::parse(MARKETS).unwrap();
        let order = validate_order(&limit("1", "1"), player, &markets, true).unwrap();
        assert_eq!(order.account, uuid_to_u64s(player.0));
    }

    #[test]
    fn enforces_size_bounds() {
        assert!(check(&limit("1", "0.5")).unwrap_err().contains("at least 1"));
        assert!(check(&limit("1", "100.5")).unwrap_err().contains("at most 100"));
        assert!(check(&limit("1", "100")).is_ok());
    }

    #[test]
    fn refuses_zero_and_malformed_decimals() {
        for (price, size) in [("0", "1"), ("1", "0"), ("-1", "1"), ("1", "-1"), ("1e2", "1"), ("1", "x")] {
            assert!(check(&limit(price, size)).is_err(), "{price} {size}");
        }
    }

    #[test]
    fn a_market_must_be_enabled() {
        let mut order = limit("1", "1");
        order.product_id = "OTHER".to_string();
        assert!(check(&order).unwrap_err().contains("not enabled"));
    }

    #[test]
    fn market_orders_have_no_price() {
        let mut order = limit("1", "1");
        order.r#type = "market".to_string();
        assert!(check(&order).unwrap_err().contains("should not be provided"));
        order.price = None;
        assert_eq!(check(&order).unwrap().price_mantissa, i64::MIN);
    }

    #[test]
    fn limit_orders_need_a_price() {
        let mut order = limit("1", "1");
        order.price = None;
        assert!(check(&order).unwrap_err().contains("required"));
    }

    #[test]
    fn side_and_type_are_checked() {
        let mut order = limit("1", "1");
        order.side = "hold".to_string();
        assert!(check(&order).unwrap_err().contains("InvalidSide"));
        let mut order = limit("1", "1");
        order.r#type = "stop".to_string();
        assert!(check(&order).unwrap_err().contains("InvalidOrderType"));
    }

    #[test]
    fn client_order_id_is_required_only_when_asked() {
        let mut order = limit("1", "1");
        order.client_order_id = None;
        assert!(check(&order).unwrap_err().contains("client_order_id"));

        let markets = Markets::parse(MARKETS).unwrap();
        assert!(validate_order(&order, Player(Uuid::nil()), &markets, false).is_ok());
    }
}
