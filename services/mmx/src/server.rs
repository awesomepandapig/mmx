//! `mmx serve`: the admin API under `/admin/v1`. The CLI and the console are both clients of this.

use crate::events;
use crate::markets::{self, MarketEntry, MarketsFile, normalize_symbol};
use crate::upstream::{
    Filter, GatewayResult, LiveMarket, ORDERS, TRADES, Table, Upstreams, reloaded_symbols, verdict,
};

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

pub const DEFAULT_PORT: u16 = 8093;
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;

pub struct AppState {
    pub up: Arc<Upstreams>,
    pub markets_file: PathBuf,
    /// Hashed, so that comparing doesn't reveal the token's length.
    pub token: Option<[u8; 32]>,
    pub poll_every: Duration,
    /// Keeps edits in order, so two can't overwrite each other or reload out of turn. One server
    /// should own the file.
    pub edits: Mutex<()>,
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

pub fn hash_token(token: Option<&str>) -> Option<[u8; 32]> {
    token.map(digest)
}

/// Works out the listen address and whether a token is required. No token is only allowed on the
/// loopback interface, and only when asked for by name.
pub fn listen_address(
    token: Option<&str>,
    insecure_local: bool,
    bind: Option<SocketAddr>,
) -> Result<SocketAddr, String> {
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT);
    let everywhere = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_PORT);
    if insecure_local {
        let address = bind.unwrap_or(loopback);
        if !address.ip().is_loopback() {
            return Err(format!(
                "--insecure-local only binds a loopback address, not {address}"
            ));
        }
        return Ok(address);
    }
    if token.is_none() {
        return Err(
            "MMX_ADMIN_TOKEN is not set. Set it, or pass --insecure-local for local use."
                .to_string(),
        );
    }
    Ok(bind.unwrap_or(everywhere))
}

#[derive(Serialize, Debug)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

/// Errors have the same shape as the gateway's.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

fn error(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError {
        status,
        code,
        message: message.into(),
    }
}

fn invalid(message: impl Into<String>) -> ApiError {
    error(StatusCode::BAD_REQUEST, "invalid_request", message)
}

fn upstream_failed(message: impl Into<String>) -> ApiError {
    error(StatusCode::BAD_GATEWAY, "upstream_failed", message)
}

fn file_failed(message: String) -> ApiError {
    error(StatusCode::INTERNAL_SERVER_ERROR, "markets_file", message)
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            warn!("{}: {}", self.code, self.message);
        }
        (
            self.status,
            Json(ErrorBody {
                code: self.code,
                message: self.message,
            }),
        )
            .into_response()
    }
}

async fn require_token(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = &state.token else {
        return next.run(request).await;
    };
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let matches = presented.is_some_and(|token| bool::from(digest(token).ct_eq(expected)));
    if !matches {
        let refused = error(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "A valid bearer token is required.",
        );
        let mut response = refused.into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer"),
        );
        return response;
    }
    next.run(request).await
}

pub fn router(state: Arc<AppState>) -> Router {
    let admin = Router::new()
        .route("/status", get(status))
        .route("/markets", get(list_markets))
        .route(
            "/markets/{symbol}",
            get(get_market).put(put_market).delete(delete_market),
        )
        .route("/markets/{symbol}/halt", post(halt))
        .route("/markets/{symbol}/resume", post(resume))
        .route("/markets/{symbol}/cancel-all", post(cancel_all))
        .route("/orders", get(orders))
        .route("/trades", get(trades))
        .route("/webhooks", get(webhooks))
        .route("/events", get(activity))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new()
        .route("/healthz", get(|| async { Json(json!({ "ok": true })) }))
        .route("/readyz", get(ready))
        .nest("/admin/v1", admin)
        .with_state(state)
}

/// Ready means this server can do its own job. It doesn't depend on the services it reports on, or
/// an outage elsewhere would take down the tool used to look into it.
async fn ready(State(state): State<Arc<AppState>>) -> Response {
    let file = markets::read(&state.markets_file).is_ok();
    let status = if file {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({ "ready": file, "checks": { "markets_file": file } })),
    )
        .into_response()
}

async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let services = state.up.status().await;
    Json(
        json!({ "verdict": verdict(&services), "version": env!("CARGO_PKG_VERSION"), "services": services }),
    )
}

#[derive(Serialize, Debug)]
struct GatewayView {
    gateway: String,
    reachable: bool,
    in_sync: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    market: Option<LiveMarket>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// A market as configured, and as each gateway holds it.
#[derive(Serialize, Debug)]
struct MarketView {
    symbol: String,
    tick_size: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_size: Option<String>,
    enabled: bool,
    /// True if any gateway has it halted, so a partly applied halt doesn't read as running.
    halted: bool,
    in_file: bool,
    /// Whether every gateway matches the file, which is false until a reload lands everywhere.
    in_sync: bool,
    gateways: Vec<GatewayView>,
}

/// Compares mantissas, because `0.10` and `0.1` are the same market, and a gateway reports the
/// defaults for limits the file leaves out.
fn gateway_matches(entry: Option<&MarketEntry>, market: Option<&LiveMarket>) -> bool {
    let live_limits = |market: &LiveMarket| {
        markets::limits(
            &market.symbol,
            &market.tick_size,
            market.min_size.as_deref(),
            market.max_size.as_deref(),
        )
    };
    match (entry, market) {
        (Some(entry), Some(market)) if entry.is_enabled() => {
            market.enabled
                && matches!((entry.limits(), live_limits(market)), (Ok(left), Ok(right)) if left == right)
        }
        // Gateways drop disabled markets when they load, so either way of showing one is fine.
        (_, Some(market)) => !market.enabled,
        (Some(entry), None) => !entry.is_enabled(),
        (None, None) => true,
    }
}

type Live = Vec<(String, Result<Vec<LiveMarket>, String>)>;

fn market_view(symbol: &str, file: &MarketsFile, live: &Live) -> MarketView {
    let entry = file.get(symbol);
    let gateways: Vec<GatewayView> = live
        .iter()
        .map(|(gateway, markets)| {
            let (markets, error) = match markets {
                Ok(markets) => (Some(markets), None),
                Err(error) => (None, Some(error.clone())),
            };
            let market =
                markets.and_then(|markets| markets.iter().find(|market| market.symbol == symbol));
            GatewayView {
                gateway: gateway.clone(),
                reachable: markets.is_some(),
                in_sync: markets.is_some() && gateway_matches(entry, market),
                market: market.cloned(),
                error,
            }
        })
        .collect();

    let listed = gateways.iter().find_map(|view| view.market.as_ref());
    let (tick_size, min_size, max_size) = match (entry, listed) {
        (Some(entry), _) => (
            entry.tick_size.clone(),
            entry.min_size.clone(),
            entry.max_size.clone(),
        ),
        (None, Some(market)) => (
            market.tick_size.clone(),
            market.min_size.clone(),
            market.max_size.clone(),
        ),
        (None, None) => Default::default(),
    };
    MarketView {
        symbol: symbol.to_string(),
        tick_size,
        min_size,
        max_size,
        enabled: entry.map_or_else(
            || listed.is_some_and(|market| market.enabled),
            MarketEntry::is_enabled,
        ),
        halted: gateways
            .iter()
            .any(|view| view.market.as_ref().is_some_and(|market| market.halted)),
        in_file: entry.is_some(),
        in_sync: gateways.iter().all(|view| view.in_sync),
        gateways,
    }
}

fn read_file(state: &AppState) -> Result<(MarketsFile, String), ApiError> {
    markets::read(&state.markets_file).map_err(file_failed)
}

fn symbol(raw: &str) -> Result<String, ApiError> {
    normalize_symbol(raw).map_err(invalid)
}

async fn list_markets(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let (file, _) = read_file(&state)?;
    let live = state.up.live_markets().await;

    // A market can be on a gateway and not in the file, which is what's worth seeing here.
    let mut symbols: Vec<&str> = file
        .markets
        .iter()
        .map(|entry| entry.symbol.as_str())
        .collect();
    symbols.extend(
        live.iter()
            .flat_map(|(_, markets)| markets.iter().flatten())
            .map(|market| market.symbol.as_str()),
    );
    symbols.sort_unstable();
    symbols.dedup();

    let views: Vec<MarketView> = symbols
        .iter()
        .map(|symbol| market_view(symbol, &file, &live))
        .collect();
    Ok(Json(json!({ "markets": views })))
}

async fn get_market(
    State(state): State<Arc<AppState>>,
    Path(raw): Path<String>,
) -> Result<Json<MarketView>, ApiError> {
    let symbol = symbol(&raw)?;
    let (file, _) = read_file(&state)?;
    let live = state.up.live_markets().await;
    let view = market_view(&symbol, &file, &live);
    if !view.in_file && view.gateways.iter().all(|gateway| gateway.market.is_none()) {
        return Err(error(
            StatusCode::NOT_FOUND,
            "market_not_found",
            format!("No market {symbol}."),
        ));
    }
    Ok(Json(view))
}

/// Checks that a reload really changed what the gateway serves. A gateway reads its own copy of the
/// file and answers 200 either way, so a copy that isn't the one edited would otherwise go unseen.
fn confirm_reload(result: &mut GatewayResult, symbol: &str, expected: bool) {
    if !result.ok {
        return;
    }
    let listed = reloaded_symbols(&result.body).map(|symbols| symbols.contains(&symbol));
    if listed != Some(expected) {
        result.fail(format!(
            "reloaded, but {symbol} didn't change there, so it isn't reading the edited file"
        ));
    }
}

/// `applied` says every gateway took the edit. The file has changed either way, so a failed gateway
/// needs another reload, not another edit.
async fn reload_everywhere(state: &AppState, symbol: &str, expected: bool) -> Response {
    let mut gateways = state
        .up
        .each_gateway(Method::POST, "/api/v1/admin/markets/reload")
        .await;
    for result in &mut gateways {
        confirm_reload(result, symbol, expected);
    }
    let applied = gateways.iter().all(|result| result.ok);
    let code = if applied {
        StatusCode::OK
    } else {
        StatusCode::BAD_GATEWAY
    };
    let body =
        json!({ "symbol": symbol, "written": true, "applied": applied, "gateways": gateways });
    (code, Json(body)).into_response()
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct MarketBody {
    tick_size: String,
    min_size: Option<String>,
    max_size: Option<String>,
    enabled: Option<bool>,
}

/// Replaces the market, so a limit left out goes back to the gateway's default.
async fn put_market(
    State(state): State<Arc<AppState>>,
    Path(raw): Path<String>,
    body: Result<Json<MarketBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let symbol = symbol(&raw)?;
    let Json(body) = body.map_err(|rejection| invalid(rejection.body_text()))?;
    let entry = MarketEntry {
        symbol: symbol.clone(),
        tick_size: body.tick_size,
        min_size: body.min_size,
        max_size: body.max_size,
        enabled: body.enabled,
    };
    entry.limits().map_err(invalid)?;
    let enabled = entry.is_enabled();

    let _edit = state.edits.lock().await;
    let (mut file, previous) = read_file(&state)?;
    file.upsert(entry);
    markets::write(&state.markets_file, &file, &previous).map_err(file_failed)?;
    info!("Markets: Set {symbol}");
    Ok(reload_everywhere(&state, &symbol, enabled).await)
}

/// Stops new orders only. Orders resting in the market stay in the engine, so cancel them first.
async fn delete_market(
    State(state): State<Arc<AppState>>,
    Path(raw): Path<String>,
) -> Result<Response, ApiError> {
    let symbol = symbol(&raw)?;

    let _edit = state.edits.lock().await;
    let (mut file, previous) = read_file(&state)?;
    if !file.remove(&symbol) {
        return Err(error(
            StatusCode::NOT_FOUND,
            "market_not_found",
            format!("No market {symbol} in the file."),
        ));
    }
    markets::write(&state.markets_file, &file, &previous).map_err(file_failed)?;
    info!("Markets: Removed {symbol}");
    Ok(reload_everywhere(&state, &symbol, false).await)
}

/// A refusal every gateway agrees on, like an unknown market, passes through as it is. Anything
/// partial is a 502, because the gateways no longer agree.
fn rollup(gateways: &[GatewayResult]) -> StatusCode {
    if gateways.iter().all(|result| result.ok) {
        return StatusCode::OK;
    }
    let first = gateways.first().and_then(|result| result.status);
    match first {
        Some(code)
            if (400..500).contains(&code)
                && gateways.iter().all(|result| result.status == first) =>
        {
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY)
        }
        _ => StatusCode::BAD_GATEWAY,
    }
}

async fn forward(state: &AppState, raw: &str, action: &str) -> Result<Response, ApiError> {
    let symbol = symbol(raw)?;
    let path = format!("/api/v1/admin/markets/{symbol}/{action}");
    let gateways = state.up.each_gateway(Method::POST, &path).await;
    let code = rollup(&gateways);
    let body = json!({ "symbol": symbol, "action": action, "ok": code.is_success(), "gateways": gateways });
    Ok((code, Json(body)).into_response())
}

async fn halt(
    State(state): State<Arc<AppState>>,
    Path(raw): Path<String>,
) -> Result<Response, ApiError> {
    forward(&state, &raw, "halt").await
}

async fn resume(
    State(state): State<Arc<AppState>>,
    Path(raw): Path<String>,
) -> Result<Response, ApiError> {
    forward(&state, &raw, "resume").await
}

#[derive(Deserialize, Serialize, Debug, Default)]
struct CancelCounts {
    requested: u64,
    canceled: u64,
    failed: u64,
    truncated: bool,
}

/// `complete` is false if an order may be left open, in which case the request can be repeated.
async fn cancel_all(
    State(state): State<Arc<AppState>>,
    Path(raw): Path<String>,
) -> Result<Response, ApiError> {
    let symbol = symbol(&raw)?;
    let gateways = state.up.cancel_all(&symbol).await;
    let answer = gateways.last().filter(|result| result.ok);
    let counts = answer.map(|result| serde_json::from_value::<CancelCounts>(result.body.clone()));
    let code = match (&counts, gateways.last().and_then(|result| result.status)) {
        (Some(Ok(_)), _) => StatusCode::OK,
        (None, Some(code)) if (400..500).contains(&code) => {
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY)
        }
        _ => StatusCode::BAD_GATEWAY,
    };
    let counts = counts.and_then(Result::ok).unwrap_or_default();
    let complete = code.is_success() && counts.failed == 0 && !counts.truncated;
    let body = json!({
        "symbol": symbol,
        "complete": complete,
        "requested": counts.requested,
        "canceled": counts.canceled,
        "failed": counts.failed,
        "truncated": counts.truncated,
        "gateways": gateways,
    });
    Ok((code, Json(body)).into_response())
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct ActivityParams {
    symbol: Option<String>,
    player: Option<String>,
    limit: Option<usize>,
}

async fn recent(
    state: &AppState,
    table: &Table,
    key: &str,
    params: Result<Query<ActivityParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(params) = params.map_err(|rejection| invalid(rejection.body_text()))?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(invalid(format!("limit must be between 1 and {MAX_LIMIT}.")));
    }
    let player = params.player.as_deref().map(Uuid::parse_str).transpose();
    let filter = Filter {
        symbol: params.symbol.as_deref().map(symbol).transpose()?,
        player: player.map_err(|_| invalid("player must be a UUID."))?,
    };
    let rows = state
        .up
        .recent(table, &filter, limit)
        .await
        .map_err(upstream_failed)?;
    let mut body = Map::new();
    body.insert(key.to_string(), json!(rows));
    Ok(Json(Value::Object(body)))
}

/// Newest first, with one row per execution report, so an order appears once per change.
async fn orders(
    State(state): State<Arc<AppState>>,
    params: Result<Query<ActivityParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    recent(&state, &ORDERS, "orders", params).await
}

/// Newest first, with one row per side of a match.
async fn trades(
    State(state): State<Arc<AppState>>,
    params: Result<Query<ActivityParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    recent(&state, &TRADES, "trades", params).await
}

async fn webhooks(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let position = state.up.position().await.map_err(upstream_failed)?;
    Ok(Json(position.view()))
}

async fn activity(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    events::sse(state.up.clone(), state.poll_every)
}

pub async fn serve(state: Arc<AppState>, address: SocketAddr) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|err| format!("can't bind {address}: {err}"))?;
    if state.token.is_none() {
        warn!("Admin API has no token, on a loopback address only");
    }
    info!("Admin API listening on {address}");
    let stopping = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    axum::serve(listener, router(state))
        .with_graceful_shutdown(stopping)
        .await
        .map_err(|err| format!("server failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tick: &str, enabled: Option<bool>) -> MarketEntry {
        MarketEntry {
            symbol: "ITEM".to_string(),
            tick_size: tick.to_string(),
            min_size: None,
            max_size: None,
            enabled,
        }
    }

    fn live(tick: &str, enabled: bool) -> LiveMarket {
        LiveMarket {
            symbol: "ITEM".to_string(),
            tick_size: tick.to_string(),
            min_size: Some("0.00000001".to_string()),
            max_size: Some("92233720368.54775807".to_string()),
            enabled,
            halted: false,
        }
    }

    #[test]
    fn startup_needs_a_token_unless_local_is_asked_for() {
        assert!(
            listen_address(None, false, None)
                .unwrap_err()
                .contains("MMX_ADMIN_TOKEN")
        );
        assert_eq!(
            listen_address(Some("t"), false, None).unwrap().to_string(),
            "0.0.0.0:8093"
        );
        assert_eq!(
            listen_address(None, true, None).unwrap().to_string(),
            "127.0.0.1:8093"
        );

        let public = "0.0.0.0:9".parse().ok();
        assert!(listen_address(None, true, public).is_err());
        assert!(listen_address(Some("t"), true, public).is_err());
        assert!(listen_address(None, true, "[::1]:9".parse().ok()).is_ok());
    }

    #[test]
    fn sync_compares_values_and_understands_defaults() {
        assert!(gateway_matches(
            Some(&entry("0.10", None)),
            Some(&live("0.1", true))
        ));
        assert!(!gateway_matches(
            Some(&entry("0.2", None)),
            Some(&live("0.1", true))
        ));
        assert!(!gateway_matches(Some(&entry("0.1", None)), None));
        assert!(!gateway_matches(None, Some(&live("0.1", true))));
        assert!(gateway_matches(Some(&entry("0.1", Some(false))), None));
        assert!(gateway_matches(
            Some(&entry("0.1", Some(false))),
            Some(&live("0.1", false))
        ));
        assert!(!gateway_matches(
            Some(&entry("0.1", Some(false))),
            Some(&live("0.1", true))
        ));
    }
}
