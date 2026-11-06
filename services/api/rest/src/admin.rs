//! The operator's surface: health probes that anything may read, and market controls behind
//! `MMX_ADMIN_TOKEN`. The firewall is the first line, and the token keeps a game server from using
//! these.

use super::AppState;
use super::aeron_handler::OrderOutcome;
use super::decimal;
use super::errors::AppError;
use super::identity::Player;
use super::markets::{Market, normalize_symbol};
use super::recorder::await_recorder;
use super::routes::{MAX_CANCEL_ALL, outcome, submit_cancel};

use std::collections::BTreeMap;
use std::env;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::{
    Json,
    extract::{Path, Request, State, rejection::PathRejection},
    http::{HeaderMap, StatusCode, header::{AUTHORIZATION, CONTENT_TYPE}},
    middleware::Next,
    response::{IntoResponse, Response},
};

use log::{info, warn};

use serde::Serialize;

use subtle::ConstantTimeEq;

const PROMETHEUS_TEXT: &str = "text/plain; version=0.0.4; charset=utf-8";

/// An empty token would let an empty bearer in, so it counts as unset.
pub fn admin_token() -> Option<String> {
    let token = env::var("MMX_ADMIN_TOKEN").ok().filter(|token| !token.is_empty());
    if token.is_none() {
        warn!("Admin: MMX_ADMIN_TOKEN isn't set, so admin routes answer 503");
    }
    token
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let (scheme, token) = headers.get(AUTHORIZATION)?.to_str().ok()?.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

fn authorize(expected: Option<&str>, headers: &HeaderMap) -> Result<(), AppError> {
    let expected = expected.ok_or(AppError::AdminDisabled)?;
    match bearer(headers) {
        // Only the token's length can leak, which is no help in guessing it.
        Some(token) if bool::from(token.as_bytes().ct_eq(expected.as_bytes())) => Ok(()),
        _ => Err(AppError::Unauthenticated("A valid admin token is required.".to_string())),
    }
}

/// Runs before any extractor on an admin route, so nothing is parsed for an unknown caller.
pub async fn require_admin(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    authorize(state.admin_token.as_deref(), request.headers())?;
    Ok(next.run(request).await)
}

#[derive(Serialize, Debug)]
pub struct Health {
    status: &'static str,
}

pub async fn healthz() -> Json<Health> {
    Json(Health { status: "ok" })
}

#[derive(Serialize, Debug)]
pub struct Readiness {
    ready: bool,
    checks: BTreeMap<&'static str, bool>,
}

/// Ready only depends on the engine. A gateway with no halt list or claim store still has to take
/// cancels, so it must stay in rotation, and `checks` shows why it's refusing new orders.
pub async fn readyz(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Readiness>) {
    let engine = state.engine.is_up() && !state.orders.is_closed();

    let mut checks = BTreeMap::from([("engine", engine), ("halts_known", state.halts.is_known())]);
    if let Some(healthy) = state.halts.store_healthy() {
        checks.insert("valkey", healthy);
    }
    if let Some(claims) = &state.claims_ready {
        checks.insert("order_claims", claims.load(Ordering::Relaxed));
    }

    let status = if engine { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, Json(Readiness { ready: engine, checks }))
}

pub async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    ([(CONTENT_TYPE, PROMETHEUS_TEXT)], state.metrics.render()).into_response()
}

#[derive(Serialize, Debug)]
pub struct MarketView {
    symbol: String,
    tick_size: String,
    min_size: String,
    max_size: String,
    enabled: bool,
    halted: bool,
}

fn market_view(state: &AppState, symbol: &str, market: &Market) -> MarketView {
    let text = |mantissa: i64| decimal::format(i128::from(mantissa));
    MarketView {
        symbol: symbol.to_string(),
        tick_size: text(market.tick_size),
        min_size: text(market.min_size),
        max_size: text(market.max_size),
        enabled: market.enabled,
        halted: state.halts.is_halted(symbol),
    }
}

#[derive(Serialize, Debug)]
pub struct MarketList {
    markets: Vec<MarketView>,
}

pub async fn list_markets(State(state): State<Arc<AppState>>) -> Json<MarketList> {
    let markets = state.markets.current();
    let markets = markets.iter().map(|(name, market)| market_view(&state, name, market)).collect();
    Json(MarketList { markets })
}

fn symbol_from(path: Result<Path<String>, PathRejection>) -> Result<String, AppError> {
    let Path(symbol) = path.map_err(|_| AppError::ValidationError("Bad market symbol.".to_string()))?;
    normalize_symbol(&symbol).map_err(AppError::ValidationError)
}

/// A typo must not read as a successful halt, so an unlisted market is an error.
fn listed_market(state: &AppState, symbol: &str) -> Result<Market, AppError> {
    state.markets.current().listed(symbol).copied().ok_or(AppError::MarketNotFound)
}

pub async fn halt_market(
    State(state): State<Arc<AppState>>,
    symbol: Result<Path<String>, PathRejection>,
) -> Result<Json<MarketView>, AppError> {
    let symbol = symbol_from(symbol)?;
    let market = listed_market(&state, &symbol)?;

    if let Err(err) = state.halts.halt(&symbol).await {
        warn!("Admin: {symbol} is halted on this gateway only: {err}");
        return Err(AppError::HaltNotShared(err));
    }
    warn!("Admin: Halted {symbol}");
    Ok(Json(market_view(&state, &symbol, &market)))
}

pub async fn resume_market(
    State(state): State<Arc<AppState>>,
    symbol: Result<Path<String>, PathRejection>,
) -> Result<Json<MarketView>, AppError> {
    let symbol = symbol_from(symbol)?;
    let market = listed_market(&state, &symbol)?;

    if let Err(err) = state.halts.resume(&symbol).await {
        warn!("Admin: {symbol} stays halted: {err}");
        return Err(AppError::MarketStillHalted(err));
    }
    warn!("Admin: Resumed {symbol}");
    Ok(Json(market_view(&state, &symbol, &market)))
}

/// `requested` less `canceled` and `failed` is orders that finished before their cancel arrived.
#[derive(Serialize, Debug, Default)]
pub struct MarketCancelAll {
    requested: usize,
    canceled: usize,
    /// Orders with no confirmed answer, which may still be working. Repeat the request.
    failed: usize,
    /// True if the market may hold more open orders than one request cancels.
    truncated: bool,
}

/// Cancels every player's open orders in a market, the same way `cancel_all_orders` cancels one
/// player's. This never halts: without a halt first, new orders can arrive behind it. The market
/// needn't be listed, because funds can still be resting in one that was taken out of the file.
pub async fn cancel_market_orders(
    State(state): State<Arc<AppState>>,
    symbol: Result<Path<String>, PathRejection>,
) -> Result<Json<MarketCancelAll>, AppError> {
    let symbol = symbol_from(symbol)?;

    if let Some(stream) = &state.report_stream {
        await_recorder(stream, state.store.as_ref()).await?;
    }
    let orders = state.store.open_orders_in_market(&symbol, MAX_CANCEL_ALL).await.map_err(|err| {
        warn!("QuestDB: Failed to read a market's open orders: {err}");
        AppError::RecorderBehind
    })?;

    let mut result = MarketCancelAll {
        requested: orders.len(),
        truncated: orders.len() == MAX_CANCEL_ALL,
        ..Default::default()
    };

    // A failure part way mustn't discard the answers to what was already sent.
    let mut waiting = Vec::with_capacity(orders.len());
    for order in &orders {
        match submit_cancel(&state, Player(order.account), order.id).await {
            Ok(answer) => waiting.push(answer),
            Err(err) => {
                warn!("Admin: Failed to submit a cancel in {symbol}: {err:?}");
                result.failed += 1;
            }
        }
    }

    for answer in waiting {
        match outcome(answer).await {
            Ok(OrderOutcome::Canceled) => result.canceled += 1,
            Ok(OrderOutcome::NotOpen) => {}
            // Without a cluster `Accepted` only means published, so it isn't a confirmed cancel.
            _ => result.failed += 1,
        }
    }

    info!(
        "Admin: Cancel-all in {symbol}: requested {}, canceled {}, failed {}, truncated {}",
        result.requested, result.canceled, result.failed, result.truncated
    );
    Ok(Json(result))
}

#[derive(Serialize, Debug)]
pub struct ReloadedMarkets {
    markets: Vec<String>,
}

/// Applies `markets.yaml` to this gateway. Every gateway needs its own reload. Halts live apart from
/// the file, so a reload can't lift one.
pub async fn reload_markets(
    State(state): State<Arc<AppState>>,
) -> Result<Json<ReloadedMarkets>, AppError> {
    let markets = state.markets.reload().map_err(AppError::MarketsNotReloaded)?;
    Ok(Json(ReloadedMarkets { markets: markets.symbols() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::http::HeaderValue;

    fn with_header(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn only_the_exact_bearer_token_passes() {
        assert!(authorize(Some("secret"), &with_header("Bearer secret")).is_ok());
        assert!(authorize(Some("secret"), &with_header("bearer secret")).is_ok());
        for value in ["Bearer wrong", "Bearer secre", "Bearer secret2", "Basic secret", "secret", "Bearer ", "Bearer"] {
            assert!(authorize(Some("secret"), &with_header(value)).is_err(), "{value}");
        }
        assert!(authorize(Some("secret"), &HeaderMap::new()).is_err());
    }

    #[test]
    fn no_token_means_no_admin_at_all() {
        let refused = authorize(None, &with_header("Bearer anything"));
        assert!(matches!(refused, Err(AppError::AdminDisabled)));
    }
}
