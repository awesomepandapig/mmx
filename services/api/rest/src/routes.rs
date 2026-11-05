use super::AppState;
use super::admin;
use super::aeron_handler::{EngineRequest, OrderOutcome, OutboundOrder};
use super::errors::AppError;
use super::halts::Refusal;
use super::identity::Player;
use super::markets::normalize_symbol;
use super::order::{Order, create_cancel_buffer, create_order_buffer, u64s_to_uuid, uuid_to_u64s};
use super::rate_limit::Decision;
use super::repeat::await_recorder;
use super::store::OrderQuery;
use super::validation::{CreateOrder, ValidOrder, validate_order};

use std::sync::Arc;
use std::time::SystemTime;

use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::StatusCode,
    middleware,
    routing::{get, post},
};

use log::warn;

use serde::{Deserialize, Serialize};

use tokio::sync::oneshot;

use uuid::Uuid;

const DEFAULT_LIST_LIMIT: usize = 100;
const MAX_LIST_LIMIT: usize = 1000;
pub const MAX_CANCEL_ALL: usize = 10_000;

pub fn router(state: Arc<AppState>) -> Router {
    // Every route here shares the one layer, so a new one can't forget its auth.
    let admin_routes = Router::new()
        .route("/api/v1/admin/markets", get(admin::list_markets))
        .route("/api/v1/admin/markets/reload", post(admin::reload_markets))
        .route("/api/v1/admin/markets/{symbol}/halt", post(admin::halt_market))
        .route("/api/v1/admin/markets/{symbol}/resume", post(admin::resume_market))
        .route("/api/v1/admin/markets/{symbol}/cancel-all", post(admin::cancel_market_orders))
        .route_layer(middleware::from_fn_with_state(state.clone(), admin::require_admin));

    Router::new()
        .route("/healthz", get(admin::healthz))
        .route("/readyz", get(admin::readyz))
        .route("/metrics", get(admin::metrics))
        .route("/api/v1/orders", get(list_orders).post(post_order).delete(cancel_all_orders))
        .route("/api/v1/orders/{id}", get(get_order).delete(cancel_order))
        .merge(admin_routes)
        .with_state(state)
}

fn now_ns() -> Result<u64, AppError> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|e| AppError::InternalServerError(format!("Failed to get system time: {}", e)))?;
    Ok(now.as_nanos() as u64)
}

/// Queues a request for the engine. The receiver holds the answer.
async fn submit(
    state: &AppState,
    request: EngineRequest,
    message: Vec<u8>,
) -> Result<oneshot::Receiver<OrderOutcome>, AppError> {
    let (published, result) = oneshot::channel();
    state
        .orders
        .send(OutboundOrder { request, message, published })
        .await
        .map_err(|_| AppError::InternalServerError("Order publisher stopped".to_string()))?;
    Ok(result)
}

pub async fn outcome(result: oneshot::Receiver<OrderOutcome>) -> Result<OrderOutcome, AppError> {
    result
        .await
        .map_err(|_| AppError::InternalServerError("Order publisher stopped".to_string()))
}

/// Only a checked limit can refuse an order. Valkey being down mustn't stop trading, so an error
/// lets the order through.
async fn check_rate_limit(state: &AppState, player: Player) -> Result<(), AppError> {
    match state.rate_limiter.check(player.0).await {
        Ok(Decision::Allowed) => Ok(()),
        Ok(Decision::Limited { retry_after_ms }) => Err(AppError::RateLimited { retry_after_ms }),
        Err(err) => {
            warn!("Rate limit: Letting an order through unchecked: {err}");
            Ok(())
        }
    }
}

/// Only new orders stop here. Cancels must always work, so a player can get funds out of the book.
fn check_halt(state: &AppState, order: &ValidOrder) -> Result<(), AppError> {
    let symbol = String::from_utf8_lossy(&order.symbol);
    let symbol = symbol.trim_end_matches('\0');
    state.halts.check(symbol).map_err(|refusal| match refusal {
        Refusal::Halted => {
            AppError::MarketHalted { symbol: symbol.to_string(), id: u64s_to_uuid(order.cl_ord_id) }
        }
        Refusal::Unknown => AppError::HaltStateUnknown,
    })
}

pub async fn post_order(
    State(state): State<Arc<AppState>>,
    player: Player,
    payload: Result<Json<CreateOrder>, JsonRejection>,
) -> Result<(StatusCode, Json<Order>), AppError> {
    let result = place_order(&state, player, payload).await;
    state.metrics.record_order(&result);
    result
}

async fn place_order(
    state: &AppState,
    player: Player,
    payload: Result<Json<CreateOrder>, JsonRejection>,
) -> Result<(StatusCode, Json<Order>), AppError> {
    let Json(payload) = payload?;

    let markets = state.markets.current();
    let order = validate_order(&payload, player, &markets, state.require_client_order_id)?;
    let id = u64s_to_uuid(order.cl_ord_id);

    // First so that a halted market doesn't spend the player's limit.
    check_halt(state, &order)?;
    check_rate_limit(state, player).await?;
    if let Some(repeats) = &state.repeats {
        repeats.check(player.0, id).await?;
    }
    // The checks above can wait on Valkey and QuestDB, and a halt may have landed meanwhile.
    check_halt(state, &order)?;

    let _in_flight = state.metrics.in_flight();
    let message = create_order_buffer(&order, now_ns()?).to_vec();
    let request = EngineRequest::New((order.account, order.cl_ord_id));
    match outcome(submit(state, request, message).await?).await? {
        OrderOutcome::Accepted => {}
        OrderOutcome::Rejected => return Err(AppError::OrderRejected(id)),
        OrderOutcome::Repeat => return Err(AppError::RepeatOrder(id)),
        OrderOutcome::TimedOut => return Err(AppError::OrderOutcomeUnknown(id)),
        OrderOutcome::Failed(err) => {
            return Err(AppError::InternalServerError(format!("Failed to publish order: {err}")));
        }
        other => {
            return Err(AppError::InternalServerError(format!("Unexpected outcome: {other:?}")));
        }
    }

    Ok((StatusCode::CREATED, Json(Order::accepted(&order, now_ns()?))))
}

#[derive(Serialize, Debug)]
pub struct CancelledOrder {
    id: Uuid,
    status: &'static str,
}

/// Queues a cancel for one of the player's orders. It carries its own ID, which the engine echoes
/// in a reject.
pub async fn submit_cancel(
    state: &AppState,
    player: Player,
    id: Uuid,
) -> Result<oneshot::Receiver<OrderOutcome>, AppError> {
    let (orig_cl_ord_id, cancel_id) = (uuid_to_u64s(id), uuid_to_u64s(Uuid::new_v4()));
    let account = player.account();
    let message = create_cancel_buffer(&orig_cl_ord_id, &cancel_id, &account, now_ns()?).to_vec();
    let request = EngineRequest::Cancel { order: (account, orig_cl_ord_id), cancel_id };
    submit(state, request, message).await
}

pub async fn cancel_order(
    State(state): State<Arc<AppState>>,
    player: Player,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<(StatusCode, Json<CancelledOrder>), AppError> {
    let Path(id) = id?;
    match outcome(submit_cancel(&state, player, id).await?).await? {
        OrderOutcome::Canceled => {
            Ok((StatusCode::OK, Json(CancelledOrder { id, status: "canceled" })))
        }
        // Without a cluster nothing reads the reports, so this only means it was published.
        OrderOutcome::Accepted => {
            Ok((StatusCode::ACCEPTED, Json(CancelledOrder { id, status: "pending" })))
        }
        OrderOutcome::NotOpen => Err(AppError::OrderNotFound),
        OrderOutcome::TimedOut => Err(AppError::OrderOutcomeUnknown(id)),
        other => Err(AppError::InternalServerError(format!("Failed to cancel order: {other:?}"))),
    }
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct CancelAllParams {
    product_id: Option<String>,
}

#[derive(Serialize, Debug, Default)]
pub struct CancelAllResult {
    /// Orders that are now cancelled.
    canceled: Vec<Uuid>,
    /// Orders that finished before their cancel arrived.
    not_open: Vec<Uuid>,
    /// Orders whose cancel has no answer. Retry them, or repeat this request.
    unresolved: Vec<Uuid>,
    /// True if the player has more open orders than one request cancels.
    more: bool,
}

/// The engine only cancels one order at a time, and it can't list a player's orders, so this reads
/// them from the recorder and cancels each one. An order placed after this starts may survive it.
pub async fn cancel_all_orders(
    State(state): State<Arc<AppState>>,
    player: Player,
    params: Result<Query<CancelAllParams>, QueryRejection>,
) -> Result<Json<CancelAllResult>, AppError> {
    let Query(params) = params?;
    let symbol = params.product_id.as_deref().map(normalize_symbol).transpose();
    let symbol = symbol.map_err(AppError::ValidationError)?;

    if let Some(stream) = &state.report_stream {
        await_recorder(stream, state.store.as_ref()).await?;
    }

    let query = OrderQuery {
        account: player.0,
        symbol,
        open_only: true,
        limit: MAX_CANCEL_ALL,
        ..Default::default()
    };
    let orders = state.store.orders(&query).await.map_err(|err| {
        warn!("QuestDB: Failed to read a player's open orders: {err}");
        AppError::RecorderBehind
    })?;

    let mut waiting = Vec::with_capacity(orders.len());
    for order in &orders {
        waiting.push((order.id, submit_cancel(&state, player, order.id).await?));
    }

    let mut result = CancelAllResult { more: orders.len() == MAX_CANCEL_ALL, ..Default::default() };
    for (id, answer) in waiting {
        match outcome(answer).await? {
            OrderOutcome::Canceled => result.canceled.push(id),
            OrderOutcome::NotOpen => result.not_open.push(id),
            _ => result.unresolved.push(id),
        }
    }
    Ok(Json(result))
}

#[derive(Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StatusFilter {
    #[default]
    All,
    Open,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    product_id: Option<String>,
    #[serde(default)]
    status: StatusFilter,
    limit: Option<usize>,
}

/// History comes from the recorder, so a very recent order may not be listed yet.
pub async fn list_orders(
    State(state): State<Arc<AppState>>,
    player: Player,
    params: Result<Query<ListParams>, QueryRejection>,
) -> Result<Json<Vec<Order>>, AppError> {
    let Query(params) = params?;
    let symbol = params.product_id.as_deref().map(normalize_symbol).transpose();
    let symbol = symbol.map_err(AppError::ValidationError)?;
    let limit = params.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if limit == 0 || limit > MAX_LIST_LIMIT {
        return Err(AppError::ValidationError(format!(
            "limit must be between 1 and {MAX_LIST_LIMIT}."
        )));
    }

    let query = OrderQuery {
        account: player.0,
        symbol,
        open_only: params.status == StatusFilter::Open,
        limit,
        ..Default::default()
    };
    let orders = state.store.orders(&query).await.map_err(|err| {
        warn!("QuestDB: Failed to list orders: {err}");
        AppError::RecorderBehind
    })?;
    Ok(Json(orders.into_iter().map(Order::recorded).collect()))
}

pub async fn get_order(
    State(state): State<Arc<AppState>>,
    player: Player,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<Order>, AppError> {
    let Path(id) = id?;
    // The account is part of the query, so another player's order is simply not found.
    let query = OrderQuery { account: player.0, cl_ord_id: Some(id), limit: 1, ..Default::default() };
    let orders = state.store.orders(&query).await.map_err(|err| {
        warn!("QuestDB: Failed to read an order: {err}");
        AppError::RecorderBehind
    })?;
    let order = orders.into_iter().next().ok_or(AppError::OrderNotFound)?;
    Ok(Json(Order::recorded(order)))
}
