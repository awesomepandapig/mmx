//! `mmx serve`: the admin API under `/admin/v1`. The CLI and the console are both clients of this.

use crate::auth::{self, Auth, Caller};
use crate::console;
use crate::events;
use crate::feed::{self, Feeds};
use crate::markets::{self, MarketEntry, MarketsFile, Problem, normalize_symbol};
use crate::monitor::{MarketStats, Monitor};
use crate::settings;
use crate::store::{ConfigStore, Delivery, MARKETS_KEY, StoreError, text_revision};
use crate::upstream::{
    Filter, GatewayResult, LiveMarket, ORDERS, Range, Side, TRADES, Table, Upstreams,
    parse_time_ms, reloaded_symbols,
};

use std::cmp::Ordering;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, Semaphore};
use tracing::{info, warn};
use uuid::Uuid;

pub const DEFAULT_PORT: u16 = 8093;
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;
/// How often gateways are checked against the store when the kubelet delivers the file.
const RECONCILE_EVERY: Duration = Duration::from_secs(5);
/// The kubelet usually takes about a minute. Past this, something is wrong.
const DELIVERY_STALLED: Duration = Duration::from_secs(180);
/// Shown to operators as what to expect.
const DELIVERY_EXPECTED_SECS: u64 = 90;
/// Open streams across every browser. Each one costs a few queries a second, so this is what keeps
/// the load on QuestDB bounded.
pub const MAX_STREAMS: usize = 64;
/// One page of the market list.
const MAX_PAGE: usize = 500;
/// `/activity/summary` scans a window of the tape, so every viewer shares one reading.
const SUMMARY_FRESH: Duration = Duration::from_secs(4);
/// The most lines `/activity/log` gives at once.
const MAX_LOG: usize = 500;

pub struct AppState {
    pub up: Arc<Upstreams>,
    pub store: Arc<ConfigStore>,
    pub auth: Arc<Auth>,
    pub poll_every: Duration,
    /// Keeps edits in order, so two can't overwrite each other or reload out of turn. Across
    /// replicas, the store's conditional writes do the same job.
    pub edits: Mutex<()>,
    /// When gateways were first seen to differ from the store.
    pub behind_since: std::sync::Mutex<Option<Instant>>,
    /// What an operator runs to restart the outbox here.
    pub outbox_restart: String,
    /// Live market data, one subscription per market being watched.
    pub feeds: Arc<Feeds>,
    pub streams: Arc<Semaphore>,
    /// One reading per window, shared by every viewer.
    pub summary: Mutex<HashMap<&'static str, (Instant, Value)>>,
    pub monitor: Arc<Monitor>,
}

/// One seat for one open stream. It's held for as long as the stream is.
pub fn stream_seat(state: &AppState) -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
    state.streams.clone().try_acquire_owned().map_err(|_| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "too_many_streams",
            "Too many live views are open. Close some and try again.",
        )
    })
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

pub fn error(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError {
        status,
        code,
        message: message.into(),
    }
}

pub fn invalid(message: impl Into<String>) -> ApiError {
    error(StatusCode::BAD_REQUEST, "invalid_request", message)
}

pub fn upstream_failed(message: impl Into<String>) -> ApiError {
    error(StatusCode::BAD_GATEWAY, "upstream_failed", message)
}

fn file_failed(message: String) -> ApiError {
    error(StatusCode::INTERNAL_SERVER_ERROR, "markets_file", message)
}

pub fn store_failed(err: StoreError) -> ApiError {
    match err {
        StoreError::Conflict => error(
            StatusCode::CONFLICT,
            "conflict",
            "Something else changed this first. Reload and try again.",
        ),
        StoreError::Failed(message) => file_failed(message),
    }
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

pub fn router(state: Arc<AppState>) -> Router {
    let admin = Router::new()
        .route("/status", get(status))
        .route("/markets", get(list_markets))
        .route("/activity/summary", get(activity_summary))
        .route("/activity/log", get(activity_log))
        .route("/markets/reload", post(reload_markets))
        .route(
            "/markets/{symbol}",
            get(get_market).put(put_market).delete(delete_market),
        )
        .route("/markets/{symbol}/halt", post(halt))
        .route("/markets/{symbol}/resume", post(resume))
        .route("/markets/{symbol}/cancel-all", post(cancel_all))
        .route("/markets/{symbol}/history", get(market_history))
        .route("/markets/{symbol}/stream", get(market_stream))
        .route(
            "/config/markets",
            get(get_markets_file).put(put_markets_file),
        )
        .route("/config/markets/validate", post(validate_markets_file))
        .route("/orders", get(orders))
        .route("/trades", get(trades))
        .route("/webhooks", get(webhooks))
        .route(
            "/webhooks/config",
            get(settings::webhook_config).put(settings::put_webhook_config),
        )
        .route("/webhooks/secret", post(settings::rotate_webhook_secret))
        .route("/events", get(activity))
        .route(
            "/settings",
            get(settings::get_settings).put(settings::put_settings),
        )
        .route("/settings/oidc", axum::routing::put(settings::put_oidc))
        .route("/settings/oidc/test", post(settings::test_oidc))
        .route("/sessions/revoke", post(auth::revoke_sessions))
        .route("/password/reset", post(auth::reset_password))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_role,
        ));
    // Open to someone holding only the temporary password.
    let setup = Router::new()
        .route("/password", post(auth::change_password))
        .route("/reauth", post(auth::reauth))
        .route("/logout", post(auth::logout))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_signed_in,
        ));
    let public = Router::new()
        .route("/session", get(auth::current_session))
        .route("/login", post(auth::login))
        .route("/oidc/start", get(auth::oidc_start))
        .route("/oidc/callback", get(auth::oidc_callback));
    Router::new()
        .route("/healthz", get(|| async { Json(json!({ "ok": true })) }))
        .route("/readyz", get(ready))
        .nest("/admin/v1", admin.merge(setup).merge(public))
        .fallback(console::serve)
        .with_state(state)
}

/// Ready means this server can do its own job. It doesn't depend on the services it reports on, or
/// an outage elsewhere would take down the tool used to look into it.
async fn ready(State(state): State<Arc<AppState>>) -> Response {
    let file = read_file(&state).await.is_ok();
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
    Json(state.monitor.status().await.as_ref().clone())
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
    #[serde(flatten)]
    stats: MarketStats,
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

type LiveByName<'a> = HashMap<&'a str, &'a LiveMarket>;

/// Each gateway's markets by symbol, and the file by symbol, because every view of every market
/// looks them up. With thousands of symbols, scanning the lists each time is too slow.
struct LiveIndex<'a> {
    gateways: Vec<(&'a str, Result<LiveByName<'a>, &'a str>)>,
    file: HashMap<&'a str, &'a MarketEntry>,
}

impl<'a> LiveIndex<'a> {
    fn new(file: &'a MarketsFile, live: &'a Live) -> Self {
        let gateways = live
            .iter()
            .map(|(gateway, markets)| {
                let markets = match markets {
                    Ok(markets) => Ok(markets
                        .iter()
                        .map(|market| (market.symbol.as_str(), market))
                        .collect()),
                    Err(error) => Err(error.as_str()),
                };
                (gateway.as_str(), markets)
            })
            .collect();
        let file = file
            .markets
            .iter()
            .map(|entry| (entry.symbol.as_str(), entry))
            .collect();
        Self { gateways, file }
    }

    /// Whether one gateway holds exactly what the file says.
    fn in_sync(&self, markets: &HashMap<&str, &LiveMarket>) -> bool {
        self.file
            .iter()
            .all(|(symbol, entry)| gateway_matches(Some(entry), markets.get(symbol).copied()))
            && markets.iter().all(|(symbol, market)| {
                gateway_matches(self.file.get(symbol).copied(), Some(market))
            })
    }

    /// The gateways that don't hold what the file says, or can't say.
    fn behind(&self) -> Vec<&str> {
        self.gateways
            .iter()
            .filter(|(_, markets)| !markets.as_ref().is_ok_and(|markets| self.in_sync(markets)))
            .map(|(gateway, _)| *gateway)
            .collect()
    }
}

/// Whether one gateway holds exactly what the file says.
fn gateway_in_sync(file: &MarketsFile, gateway: &str, markets: &[LiveMarket]) -> bool {
    let live = vec![(gateway.to_string(), Ok(markets.to_vec()))];
    LiveIndex::new(file, &live).behind().is_empty()
}

fn market_view(symbol: &str, index: &LiveIndex, stats: &HashMap<String, MarketStats>) -> MarketView {
    let entry = index.file.get(symbol).copied();
    let gateways: Vec<GatewayView> = index
        .gateways
        .iter()
        .map(|(gateway, markets)| {
            let (markets, error) = match markets {
                Ok(markets) => (Some(markets), None),
                Err(error) => (None, Some(error.to_string())),
            };
            let market = markets.and_then(|markets| markets.get(symbol).copied());
            GatewayView {
                gateway: gateway.to_string(),
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
        stats: stats.get(symbol).cloned().unwrap_or_default(),
    }
}

fn markets_text(data: &crate::store::Data) -> String {
    data.get(MARKETS_KEY)
        .cloned()
        .unwrap_or_else(|| "markets: []\n".to_string())
}

async fn read_file(state: &AppState) -> Result<(MarketsFile, String), ApiError> {
    let doc = state.store.config().await.map_err(store_failed)?;
    let text = markets_text(&doc.data);
    let file = MarketsFile::parse(&text).map_err(file_failed)?;
    Ok((file, text))
}

/// Applies an edit to the stored file. It's re-run on a fresh copy if something else wrote first.
async fn edit_file<F>(state: &AppState, edit: F) -> Result<(), ApiError>
where
    F: Fn(&mut MarketsFile),
{
    state
        .store
        .update_config(|data| {
            let previous = markets_text(data);
            let mut file = MarketsFile::parse(&previous).map_err(StoreError::Failed)?;
            edit(&mut file);
            let text = markets::render(&file, &previous).map_err(StoreError::Failed)?;
            data.insert(MARKETS_KEY.to_string(), text);
            Ok(())
        })
        .await
        .map_err(store_failed)?;
    Ok(())
}

/// How far the stored file has got. In Kubernetes a gap is normal for a minute or so, and only
/// becomes a problem if it lasts.
fn delivery_view(state: &AppState, index: &LiveIndex) -> Value {
    let behind = index.behind();
    let mode = state.store.info().delivery;
    let mut since = state
        .behind_since
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if behind.is_empty() {
        *since = None;
        return json!({ "mode": mode, "state": "in_sync", "behind": behind });
    }
    let waited = since.get_or_insert_with(Instant::now).elapsed();
    let phase = match mode {
        Delivery::Immediate => "out_of_sync",
        Delivery::Eventual if waited < DELIVERY_STALLED => "reaching_gateways",
        Delivery::Eventual => "stalled",
    };
    json!({
        "mode": mode,
        "state": phase,
        "behind": behind,
        "waited_secs": waited.as_secs(),
        "expected_secs": DELIVERY_EXPECTED_SECS,
    })
}

fn symbol(raw: &str) -> Result<String, ApiError> {
    normalize_symbol(raw).map_err(invalid)
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct MarketQuery {
    /// Part of a symbol.
    q: Option<String>,
    /// `open`, `halted`, `disabled` or `out_of_sync`.
    state: Option<String>,
    /// `symbol`, `state`, `last`, `volume`, `trades`, `last_trade`, `spread` or `open_orders`.
    /// Symbol is the default.
    sort: Option<String>,
    /// `asc` or `desc`.
    dir: Option<String>,
    /// Left out, the whole list comes back.
    limit: Option<usize>,
    offset: Option<usize>,
}

/// What a market's numbers sort by. A market with no number comes last in either direction, so
/// markets that never traded don't bury the ones that did.
fn stat_key(view: &MarketView, sort: &str) -> Option<f64> {
    let stats = &view.stats;
    let number = |text: &Option<String>| text.as_deref()?.parse::<f64>().ok();
    match sort {
        "last" => number(&stats.last),
        "volume" => number(&stats.volume_24h),
        "spread" => number(&stats.spread),
        "trades" => stats.trades_24h.map(|count| count as f64),
        "open_orders" => stats.open_orders.map(|count| count as f64),
        "last_trade" => stats
            .last_time
            .as_deref()
            .and_then(parse_time_ms)
            .map(|time| time as f64),
        _ => None,
    }
}

const STAT_SORTS: [&str; 6] = ["last", "volume", "spread", "trades", "open_orders", "last_trade"];

fn state_of(view: &MarketView) -> &'static str {
    if view.halted {
        "halted"
    } else if view.enabled {
        "open"
    } else {
        "disabled"
    }
}

/// Searches and pages on the server, because a browser shouldn't hold thousands of markets, or a
/// gateway's copy of each, to show fifty.
async fn list_markets(
    State(state): State<Arc<AppState>>,
    query: Result<Query<MarketQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(|rejection| invalid(rejection.body_text()))?;
    if let Some(wanted) = query.state.as_deref()
        && !["open", "halted", "disabled", "out_of_sync"].contains(&wanted)
    {
        return Err(invalid(format!("Unknown state {wanted}.")));
    }
    if query.limit.is_some_and(|limit| limit > MAX_PAGE) {
        return Err(invalid(format!("limit must be at most {MAX_PAGE}.")));
    }
    let (file, _) = read_file(&state).await?;
    let live = state.up.live_markets().await;
    let index = LiveIndex::new(&file, &live);
    let stats = state.monitor.stats();

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

    let needle = query.q.as_deref().unwrap_or("").trim().to_uppercase();
    let mut views: Vec<MarketView> = symbols
        .iter()
        .filter(|symbol| symbol.contains(&needle))
        .map(|symbol| market_view(symbol, &index, &stats))
        .collect();

    // Counted before the state filter, so the filter's own buttons can show them.
    let count = |test: &dyn Fn(&MarketView) -> bool| views.iter().filter(|view| test(view)).count();
    let counts = json!({
        "all": views.len(),
        "open": count(&|view| state_of(view) == "open"),
        "halted": count(&|view| view.halted),
        "disabled": count(&|view| state_of(view) == "disabled"),
        "out_of_sync": count(&|view| !view.in_sync),
    });
    let delivery = delivery_view(&state, &index);

    if let Some(wanted) = query.state.as_deref() {
        views.retain(|view| match wanted {
            "out_of_sync" => !view.in_sync,
            other => state_of(view) == other,
        });
    }
    let descending = query.dir.as_deref() == Some("desc");
    let sort = query.sort.as_deref().unwrap_or("symbol");
    if STAT_SORTS.contains(&sort) {
        views.sort_by(|left, right| match (stat_key(left, sort), stat_key(right, sort)) {
            (Some(left), Some(right)) if descending => right.total_cmp(&left),
            (Some(left), Some(right)) => left.total_cmp(&right),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        });
    } else {
        if sort == "state" {
            views.sort_by(|left, right| state_of(left).cmp(state_of(right)));
        } else if !needle.is_empty() {
            // What someone typed the start of comes first, then what merely contains it.
            views.sort_by_key(|view| !view.symbol.starts_with(&needle));
        }
        if descending {
            views.reverse();
        }
    }
    let total = views.len();
    let offset = query.offset.unwrap_or(0);
    let page: Vec<MarketView> = views
        .into_iter()
        .skip(offset)
        .take(query.limit.unwrap_or(usize::MAX))
        .collect();
    Ok(Json(json!({
        "markets": page,
        "total": total,
        "counts": counts,
        "delivery": delivery,
    })))
}

async fn get_market(
    State(state): State<Arc<AppState>>,
    Path(raw): Path<String>,
) -> Result<Json<MarketView>, ApiError> {
    let symbol = symbol(&raw)?;
    let (file, _) = read_file(&state).await?;
    let live = state.up.live_markets().await;
    let view = market_view(
        &symbol,
        &LiveIndex::new(&file, &live),
        &state.monitor.stats(),
    );
    if !view.in_file && view.gateways.iter().all(|gateway| gateway.market.is_none()) {
        return Err(error(
            StatusCode::NOT_FOUND,
            "market_not_found",
            format!("No market {symbol}."),
        ));
    }
    Ok(Json(view))
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct HistoryParams {
    range: Option<String>,
}

/// Prices for the chart, and the day's numbers, from the tape.
async fn market_history(
    State(state): State<Arc<AppState>>,
    Path(raw): Path<String>,
    params: Result<Query<HistoryParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let symbol = symbol(&raw)?;
    let Query(params) = params.map_err(|rejection| invalid(rejection.body_text()))?;
    let range = Range::parse(params.range.as_deref().unwrap_or("1h"))
        .ok_or_else(|| invalid("range must be 15m, 1h, 6h or 24h."))?;
    let (candles, day) = tokio::join!(
        state.up.candles(&symbol, range),
        state.up.day_summary(&symbol)
    );
    let candles = candles.map_err(upstream_failed)?;
    Ok(Json(json!({
        "range": range.name(),
        "bucket_secs": range.bucket_secs(),
        "candles": candles,
        "day": day.map_err(upstream_failed)?,
    })))
}

async fn market_stream(
    State(state): State<Arc<AppState>>,
    Path(raw): Path<String>,
) -> Result<Response, ApiError> {
    let symbol = symbol(&raw)?;
    let seat = stream_seat(&state)?;
    Ok(feed::sse(state.feeds.clone(), symbol, seat).into_response())
}

/// How busy the exchange is, for the overview.
#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct SummaryParams {
    range: Option<String>,
}

async fn activity_summary(
    State(state): State<Arc<AppState>>,
    params: Result<Query<SummaryParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(params) = params.map_err(|rejection| invalid(rejection.body_text()))?;
    let wanted = params.range.as_deref().unwrap_or("1h");
    let range = Range::parse(wanted).ok_or_else(|| invalid(format!("Unknown range {wanted}.")))?;
    let mut cached = state.summary.lock().await;
    if let Some((at, value)) = cached.get(range.name())
        && at.elapsed() < SUMMARY_FRESH
    {
        return Ok(Json(value.clone()));
    }
    let value = state.up.summary(range).await.map_err(upstream_failed)?;
    cached.insert(range.name(), (Instant::now(), value.clone()));
    Ok(Json(value))
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct LogParams {
    limit: Option<usize>,
}

/// What `mmx serve` has seen happen, newest first.
async fn activity_log(
    State(state): State<Arc<AppState>>,
    params: Result<Query<LogParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(params) = params.map_err(|rejection| invalid(rejection.body_text()))?;
    let limit = params.limit.unwrap_or(MAX_LOG).min(MAX_LOG);
    Ok(Json(state.monitor.log(limit)))
}

/// Checks that a reload really changed what the gateway serves. A gateway reads its own copy of the
/// file and answers 200 either way, so a copy that isn't the one edited would otherwise go unseen.
fn confirm_reload(result: &mut GatewayResult, expected: &[(String, bool)]) {
    if !result.ok {
        return;
    }
    let listed = reloaded_symbols(&result.body);
    let wrong = expected.iter().find(|(symbol, enabled)| {
        listed
            .as_ref()
            .map(|symbols| symbols.contains(&symbol.as_str()))
            != Some(*enabled)
    });
    if let Some((symbol, _)) = wrong {
        result.fail(format!(
            "reloaded, but {symbol} didn't change there, so it isn't reading the edited file"
        ));
    }
}

/// `applied` says every gateway took the edit. The store has changed either way, so a failed gateway
/// needs another reload, not another edit. In Kubernetes the reload usually comes before the kubelet
/// has delivered the file, which isn't a failure: the reconciler keeps going until it lands.
async fn reload_everywhere(state: &AppState, expected: &[(String, bool)]) -> (StatusCode, Value) {
    let mut gateways = state
        .up
        .each_gateway(Method::POST, "/api/v1/admin/markets/reload")
        .await;
    if state.store.info().delivery == Delivery::Immediate {
        for result in &mut gateways {
            confirm_reload(result, expected);
        }
        let applied = gateways.iter().all(|result| result.ok);
        let (code, phase) = if applied {
            (StatusCode::OK, "applied")
        } else {
            (StatusCode::BAD_GATEWAY, "failed")
        };
        let body =
            json!({ "written": true, "applied": applied, "state": phase, "gateways": gateways });
        return (code, body);
    }

    let (file, _) = read_file(state).await.unwrap_or_default();
    let live = state.up.live_markets().await;
    if LiveIndex::new(&file, &live).behind().is_empty() {
        let body =
            json!({ "written": true, "applied": true, "state": "applied", "gateways": gateways });
        return (StatusCode::OK, body);
    }
    let body = json!({
        "written": true,
        "applied": false,
        "state": "reaching_gateways",
        "expected_secs": DELIVERY_EXPECTED_SECS,
        "gateways": gateways,
    });
    (StatusCode::ACCEPTED, body)
}

fn edit_response(symbol: &str, (code, mut body): (StatusCode, Value)) -> Response {
    body["symbol"] = json!(symbol);
    (code, Json(body)).into_response()
}

/// In Kubernetes, nothing else tells a gateway the ConfigMap changed. Reloading is harmless, so any
/// gateway that differs from the store is asked to, including after a `kubectl apply`.
pub fn spawn_reconciler(state: Arc<AppState>) {
    if state.store.info().delivery != Delivery::Eventual {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(RECONCILE_EVERY).await;
            let Ok((file, _)) = read_file(&state).await else {
                continue;
            };
            for (gateway, markets) in state.up.live_markets().await {
                let in_sync = markets
                    .as_ref()
                    .is_ok_and(|markets| gateway_in_sync(&file, &gateway, markets));
                if !in_sync {
                    let path = "/api/v1/admin/markets/reload";
                    let result = state.up.gateway_call(Method::POST, &gateway, path).await;
                    if let Some(error) = result.error {
                        warn!("Markets: {gateway} didn't reload: {error}");
                    }
                }
            }
        }
    });
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
    edit_file(&state, |file| file.upsert(entry.clone())).await?;
    info!("Markets: Set {symbol}");
    let outcome = reload_everywhere(&state, &[(symbol.clone(), enabled)]).await;
    Ok(edit_response(&symbol, outcome))
}

/// Stops new orders only. Orders resting in the market stay in the engine, so cancel them first.
async fn delete_market(
    State(state): State<Arc<AppState>>,
    Path(raw): Path<String>,
) -> Result<Response, ApiError> {
    let symbol = symbol(&raw)?;

    let _edit = state.edits.lock().await;
    let (file, _) = read_file(&state).await?;
    if file.get(&symbol).is_none() {
        return Err(error(
            StatusCode::NOT_FOUND,
            "market_not_found",
            format!("No market {symbol} in the file."),
        ));
    }
    edit_file(&state, |file| {
        file.remove(&symbol);
    })
    .await?;
    info!("Markets: Removed {symbol}");
    let outcome = reload_everywhere(&state, &[(symbol.clone(), false)]).await;
    Ok(edit_response(&symbol, outcome))
}

/// Asks every gateway to read the file again, for after a hand edit.
async fn reload_markets(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let (file, _) = read_file(&state).await?;
    let expected: Vec<(String, bool)> = file
        .markets
        .iter()
        .map(|entry| (entry.symbol.clone(), entry.is_enabled()))
        .collect();
    let (code, body) = reload_everywhere(&state, &expected).await;
    Ok((code, Json(body)).into_response())
}

/// The file as stored, even if it no longer parses: a `kubectl apply` can put anything there, and
/// the editor is how an operator fixes it.
async fn get_markets_file(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let doc = state.store.config().await.map_err(store_failed)?;
    let text = markets_text(&doc.data);
    let info = state.store.info();
    let problem = markets::check(&text).err();
    Ok(Json(json!({
        "yaml": text,
        "revision": text_revision(&text),
        "valid": problem.is_none(),
        "problem": problem,
        "backend": info.backend,
        "location": info.config,
        "delivery": info.delivery,
    })))
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct MarketsFileBody {
    yaml: String,
    #[serde(default)]
    revision: Option<String>,
}

fn problem_response(problem: &Problem) -> ApiError {
    let message = match problem.line {
        Some(line) => format!("Line {line}: {}", problem.message),
        None => problem.message.clone(),
    };
    error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_markets", message)
}

/// Names what a save would do, and warns about what stays behind in the engine.
async fn validate_markets_file(
    State(state): State<Arc<AppState>>,
    body: Result<Json<MarketsFileBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| invalid(rejection.body_text()))?;
    let new = match markets::check(&body.yaml) {
        Ok(new) => new,
        Err(problem) => return Ok(Json(json!({ "valid": false, "problem": problem }))),
    };
    let doc = state.store.config().await.map_err(store_failed)?;
    let current = MarketsFile::parse(&markets_text(&doc.data)).unwrap_or_default();
    let changes = markets::changes(&current, &new);
    let mut warnings = Vec::new();
    if !changes.removed.is_empty() {
        warnings.push(format!(
            "Removing {} stops new orders. Open orders stay in the engine, so cancel them first.",
            changes.removed.join(", ")
        ));
    }
    if !changes.changed.is_empty() {
        warnings.push(format!(
            "Changing {} doesn't touch open orders, which keep their old price and size.",
            changes.changed.join(", ")
        ));
    }
    Ok(Json(
        json!({ "valid": true, "changes": changes, "warnings": warnings }),
    ))
}

/// Saves the text exactly as written, comments included, if nothing has changed since it was read.
async fn put_markets_file(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<auth::Caller>,
    body: Result<Json<MarketsFileBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body.map_err(|rejection| invalid(rejection.body_text()))?;
    let new = markets::check(&body.yaml).map_err(|problem| problem_response(&problem))?;

    let _edit = state.edits.lock().await;
    let mut doc = state.store.config().await.map_err(store_failed)?;
    let current = markets_text(&doc.data);
    if body
        .revision
        .is_some_and(|revision| revision != text_revision(&current))
    {
        return Err(error(
            StatusCode::CONFLICT,
            "markets_changed",
            "The markets changed since you opened this. Reload to see them, then edit again.",
        ));
    }
    let old = MarketsFile::parse(&current).unwrap_or_default();
    // Dropping a line from the file deletes a market, just as the delete route does.
    if !markets::changes(&old, &new).removed.is_empty() {
        auth::check_recent(&caller)?;
    }
    doc.data.insert(MARKETS_KEY.to_string(), body.yaml.clone());
    state.store.put_config(&doc).await.map_err(store_failed)?;
    info!("Markets: Saved the whole file");

    // Markets that left the file are expected to be gone, so they're checked too.
    let mut expected: Vec<(String, bool)> = new
        .markets
        .iter()
        .map(|entry| (entry.symbol.clone(), entry.is_enabled()))
        .collect();
    expected.extend(
        old.markets
            .iter()
            .filter(|entry| new.get(&entry.symbol).is_none())
            .map(|entry| (entry.symbol.clone(), false)),
    );
    let (code, mut outcome) = reload_everywhere(&state, &expected).await;
    outcome["revision"] = json!(text_revision(&body.yaml));
    Ok((code, Json(outcome)).into_response())
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

async fn forward(
    state: &AppState,
    raw: &str,
    action: &str,
    caller: &Caller,
) -> Result<Response, ApiError> {
    let symbol = symbol(raw)?;
    let path = format!("/api/v1/admin/markets/{symbol}/{action}");
    let gateways = state.up.each_gateway(Method::POST, &path).await;
    let code = rollup(&gateways);
    let level = if action == "halt" { "warn" } else { "info" };
    let done = if code.is_success() { "" } else { " (not on every gateway)" };
    let verb = if action == "halt" { "Halted" } else { "Resumed" };
    state.monitor.note(
        level,
        format!("market {symbol}"),
        format!("{verb}{done}"),
        Some(caller.name().to_string()),
    );
    let body = json!({ "symbol": symbol, "action": action, "ok": code.is_success(), "gateways": gateways });
    Ok((code, Json(body)).into_response())
}

async fn halt(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Path(raw): Path<String>,
) -> Result<Response, ApiError> {
    forward(&state, &raw, "halt", &caller).await
}

async fn resume(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Path(raw): Path<String>,
) -> Result<Response, ApiError> {
    forward(&state, &raw, "resume", &caller).await
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
    Extension(caller): Extension<Caller>,
    Path(raw): Path<String>,
) -> Result<Response, ApiError> {
    let symbol = symbol(&raw)?;
    let gateways = state.up.cancel_all(&symbol).await;
    state.monitor.note(
        "warn",
        format!("market {symbol}"),
        "Canceled all open orders".to_string(),
        Some(caller.name().to_string()),
    );
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

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct ActivityParams {
    symbol: Option<String>,
    player: Option<String>,
    side: Option<String>,
    /// An execution type, such as `new`, `trade` or `canceled`.
    event: Option<String>,
    /// A client order ID, or the engine's order number.
    order: Option<String>,
    /// Pages back through older rows, as the `time` of the last row seen.
    before: Option<String>,
    limit: Option<usize>,
    /// For `/events`: `tape` leaves out markets, services and webhooks.
    scope: Option<String>,
}

/// Every value reaches SQL parsed or from a fixed list, so none can contain a quote.
pub fn tape_filter(params: &ActivityParams) -> Result<Filter, ApiError> {
    let player = params.player.as_deref().filter(|text| !text.is_empty());
    let side = params.side.as_deref().filter(|text| !text.is_empty());
    let event = params.event.as_deref().filter(|text| !text.is_empty());
    let order = params
        .order
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    Ok(Filter {
        symbol: params
            .symbol
            .as_deref()
            .filter(|text| !text.is_empty())
            .map(symbol)
            .transpose()?,
        player: player
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| invalid("player must be a UUID."))?,
        side: side.map(Side::parse).transpose().map_err(invalid)?,
        event: event
            .map(crate::upstream::event_name)
            .transpose()
            .map_err(invalid)?,
        order: order
            .map(crate::upstream::OrderKey::parse)
            .transpose()
            .map_err(invalid)?,
        before: params
            .before
            .as_deref()
            .map(|time| {
                events::is_timestamp(time)
                    .then(|| time.to_string())
                    .ok_or_else(|| invalid("before must be a timestamp."))
            })
            .transpose()?,
    })
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
    let filter = tape_filter(&params)?;
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

async fn activity(
    State(state): State<Arc<AppState>>,
    params: Result<Query<ActivityParams>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(params) = params.map_err(|rejection| invalid(rejection.body_text()))?;
    let filter = tape_filter(&params)?;
    let tape_only = params.scope.as_deref() == Some("tape");
    let seat = stream_seat(&state)?;
    let poller = events::Poller::new(state.up.clone(), filter, tape_only, state.poll_every);
    Ok(events::sse(poller, state.poll_every, seat).into_response())
}

/// Kubernetes stops a pod with SIGTERM.
async fn stopping() {
    let interrupted = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminated = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminated = std::future::pending::<()>();
    tokio::select! {
        () = interrupted => {}
        () = terminated => {}
    }
}

pub async fn serve(state: Arc<AppState>, address: SocketAddr) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|err| format!("can't bind {address}: {err}"))?;
    if !state.auth.has_token() {
        warn!("Admin API has no token, on a loopback address only");
    }
    info!("Admin API listening on {address}");
    let service = router(state).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, service)
        .with_graceful_shutdown(stopping())
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
