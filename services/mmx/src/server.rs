//! `mmx serve`: the admin API under `/admin/v1`. The CLI and the console are both clients of this.

use crate::auth::{self, Auth};
use crate::console;
use crate::events;
use crate::markets::{self, MarketEntry, MarketsFile, Problem, normalize_symbol};
use crate::settings;
use crate::store::{ConfigStore, Delivery, MARKETS_KEY, StoreError, text_revision};
use crate::upstream::{
    Filter, GatewayResult, LiveMarket, ORDERS, TRADES, Table, Upstreams, reloaded_symbols, verdict,
};

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;
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
        .route("/markets/reload", post(reload_markets))
        .route(
            "/markets/{symbol}",
            get(get_market).put(put_market).delete(delete_market),
        )
        .route("/markets/{symbol}/halt", post(halt))
        .route("/markets/{symbol}/resume", post(resume))
        .route("/markets/{symbol}/cancel-all", post(cancel_all))
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
            auth::require_admin,
        ));
    // Open to someone holding only the temporary password.
    let setup = Router::new()
        .route("/password", post(auth::change_password))
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

/// Whether one gateway holds exactly what the file says.
fn gateway_in_sync(file: &MarketsFile, markets: &[LiveMarket]) -> bool {
    let listed = file.markets.iter().all(|entry| {
        let market = markets.iter().find(|market| market.symbol == entry.symbol);
        gateway_matches(Some(entry), market)
    });
    listed
        && markets
            .iter()
            .all(|market| gateway_matches(file.get(&market.symbol), Some(market)))
}

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
fn delivery_view(state: &AppState, file: &MarketsFile, live: &Live) -> Value {
    let behind: Vec<&str> = live
        .iter()
        .filter(|(_, markets)| {
            !markets
                .as_ref()
                .is_ok_and(|markets| gateway_in_sync(file, markets))
        })
        .map(|(gateway, _)| gateway.as_str())
        .collect();
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

async fn list_markets(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let (file, _) = read_file(&state).await?;
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
    let delivery = delivery_view(&state, &file, &live);
    Ok(Json(json!({ "markets": views, "delivery": delivery })))
}

async fn get_market(
    State(state): State<Arc<AppState>>,
    Path(raw): Path<String>,
) -> Result<Json<MarketView>, ApiError> {
    let symbol = symbol(&raw)?;
    let (file, _) = read_file(&state).await?;
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

    let (file, _) = match read_file(state).await {
        Ok(read) => read,
        Err(_) => (MarketsFile::default(), String::new()),
    };
    let live = state.up.live_markets().await;
    let applied = live.iter().all(|(_, markets)| {
        markets
            .as_ref()
            .is_ok_and(|markets| gateway_in_sync(&file, markets))
    });
    if applied {
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
                    .is_ok_and(|markets| gateway_in_sync(&file, markets));
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
