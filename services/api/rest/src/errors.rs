// src/errors.rs (or a similar module)
use axum::{
    Json,
    extract::rejection::{JsonRejection, PathRejection, QueryRejection},
    http::{HeaderValue, StatusCode, header::RETRY_AFTER},
    response::{IntoResponse, Response},
};
use log::error;
use serde::Serialize;
use uuid::Uuid;

#[derive(Debug)]
pub enum AppError {
    InvalidSide,
    InvalidOrderType,
    ValidationError(String),
    JsonDeserializationError(String), // For custom messages from JsonRejection
    InternalServerError(String),
    Unauthenticated(String),
    /// The engine refused the order and it can't execute, so any escrow can be released.
    OrderRejected(Uuid),
    /// Another request claimed this client order ID. This one placed nothing, but that says nothing
    /// about what became of the first, so any escrow stays held until its status is known.
    DuplicateOrder(Uuid),
    /// Not a UUIDv7. Nothing was sent.
    InvalidOrderId(Uuid),
    /// Older than the window, so it could be a replay. Nothing was sent.
    StaleOrderId(Uuid),
    /// Dated further ahead than clocks can differ. Nothing was sent.
    FutureOrderId(Uuid),
    /// The claim couldn't be confirmed, so the order wasn't sent.
    ClaimUnavailable(Uuid),
    /// The claim store lost its data, so new orders wait until no lost claim can matter.
    ClaimsRecovering { id: Uuid, retry_after_ms: u64 },
    OrderOutcomeUnknown(Uuid),
    RateLimited { retry_after_ms: u64 },
    /// The recorder hasn't caught up, so a check that relies on it can't be trusted yet.
    RecorderBehind,
    OrderNotFound,
    /// The markets file wasn't applied, and the gateway keeps the markets it had.
    MarketsNotReloaded(String),
    /// Operators stopped this market. Nothing reached the engine, so any escrow can be released.
    MarketHalted { symbol: String, id: Uuid },
    /// This gateway has never read the halt list, so it can't tell whether the market is open.
    HaltStateUnknown,
    /// `MMX_ADMIN_TOKEN` isn't set.
    AdminDisabled,
    MarketNotFound,
    /// The halt is in force on this gateway, but others haven't been told.
    HaltNotShared(String),
    /// The resume didn't reach the shared list, so the market stays halted everywhere.
    MarketStillHalted(String),
}

#[derive(Serialize)]
struct ErrorResponse {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Uuid>,
}

struct ErrorBody {
    status: StatusCode,
    code: &'static str,
    message: String,
    details: Option<String>,
    id: Option<Uuid>,
}

fn body(status: StatusCode, code: &'static str, message: impl Into<String>) -> ErrorBody {
    ErrorBody { status, code, message: message.into(), details: None, id: None }
}

impl ErrorBody {
    fn details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    fn id(mut self, id: Uuid) -> Self {
        self.id = Some(id);
        self
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let retry_after = match &self {
            // Retry-After is in whole seconds, so round up.
            AppError::RateLimited { retry_after_ms } | AppError::ClaimsRecovering { retry_after_ms, .. } => {
                Some(retry_after_ms.div_ceil(1000).max(1))
            }
            _ => None,
        };

        let response = match self {
            AppError::InvalidSide => {
                body(StatusCode::BAD_REQUEST, "invalid_side", "Invalid side provided.")
                    .details("Allowed values are 'buy' or 'sell'.")
            }
            AppError::InvalidOrderType => {
                body(StatusCode::BAD_REQUEST, "invalid_order_type", "Invalid order type provided.")
                    .details("Allowed values are 'limit' or 'market'.")
            }
            AppError::ValidationError(msg) => body(StatusCode::BAD_REQUEST, "invalid_request", msg),
            AppError::JsonDeserializationError(msg) => {
                body(StatusCode::UNPROCESSABLE_ENTITY, "invalid_body", msg)
            }
            AppError::Unauthenticated(msg) => body(StatusCode::UNAUTHORIZED, "unauthenticated", msg),
            AppError::OrderRejected(id) => {
                body(StatusCode::UNPROCESSABLE_ENTITY, "order_rejected", "The engine rejected this order.")
                    .details("It will not execute.")
                    .id(id)
            }
            AppError::DuplicateOrder(id) => {
                body(StatusCode::CONFLICT, "duplicate_order", "This client order ID has already been used.")
                    .details("This request placed nothing new. Keep any funds on hold and ask for this order's status to learn what became of the first request.")
                    .id(id)
            }
            AppError::InvalidOrderId(id) => {
                body(StatusCode::BAD_REQUEST, "invalid_client_order_id", "client_order_id must be a UUIDv7.")
                    .details("Nothing was sent to the engine.")
                    .id(id)
            }
            AppError::StaleOrderId(id) => {
                body(StatusCode::BAD_REQUEST, "stale_client_order_id", "This client order ID is too old to accept.")
                    .details("Nothing was sent to the engine. To place an order, make a new client_order_id.")
                    .id(id)
            }
            AppError::FutureOrderId(id) => {
                body(StatusCode::BAD_REQUEST, "future_client_order_id", "This client order ID is dated in the future.")
                    .details("Nothing was sent to the engine. Check this clock, then make a new client_order_id.")
                    .id(id)
            }
            AppError::ClaimUnavailable(id) => {
                body(StatusCode::SERVICE_UNAVAILABLE, "claim_unavailable", "New orders are refused because a duplicate can't be ruled out right now.")
                    .details("Nothing was sent to the engine. Cancels still work. To try again, make a new client_order_id.")
                    .id(id)
            }
            AppError::ClaimsRecovering { id, .. } => {
                body(StatusCode::SERVICE_UNAVAILABLE, "claims_recovering", "New orders are paused while the claim store recovers.")
                    .details("Nothing was sent to the engine. Cancels still work. To try again, wait for Retry-After and make a new client_order_id.")
                    .id(id)
            }
            AppError::OrderOutcomeUnknown(id) => {
                body(StatusCode::SERVICE_UNAVAILABLE, "outcome_unknown", "The outcome isn't known yet.")
                    .details("Do not send this again. Keep any funds on hold and ask for this order's status.")
                    .id(id)
            }
            AppError::RateLimited { .. } => {
                body(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "Too many orders for this player.")
            }
            AppError::RecorderBehind => {
                body(StatusCode::SERVICE_UNAVAILABLE, "recorder_behind", "Order history isn't available yet.")
                    .details("Nothing was sent to the engine. Retry the same request.")
            }
            AppError::OrderNotFound => body(StatusCode::NOT_FOUND, "order_not_found", "No such order."),
            AppError::MarketsNotReloaded(msg) => {
                body(StatusCode::UNPROCESSABLE_ENTITY, "markets_not_reloaded", "The markets file was not applied.")
                    .details(msg)
            }
            AppError::MarketHalted { symbol, id } => {
                body(StatusCode::UNPROCESSABLE_ENTITY, "market_halted", format!("Market '{symbol}' is halted."))
                    .details("Nothing was sent to the engine. Cancels still work.")
                    .id(id)
            }
            AppError::HaltStateUnknown => {
                body(StatusCode::SERVICE_UNAVAILABLE, "halt_state_unknown", "This gateway can't tell which markets are halted yet.")
                    .details("Nothing was sent to the engine. Cancels still work.")
            }
            AppError::AdminDisabled => {
                body(StatusCode::SERVICE_UNAVAILABLE, "admin_disabled", "Admin routes are disabled.")
                    .details("Set MMX_ADMIN_TOKEN to enable them.")
            }
            AppError::MarketNotFound => body(StatusCode::NOT_FOUND, "market_not_found", "No such market."),
            AppError::HaltNotShared(msg) => {
                body(StatusCode::SERVICE_UNAVAILABLE, "halt_not_shared", "The market is halted on this gateway only.")
                    .details(format!("This gateway keeps trying to share the halt. Other gateways are still open: {msg}"))
            }
            AppError::MarketStillHalted(msg) => {
                body(StatusCode::SERVICE_UNAVAILABLE, "market_still_halted", "The market was not resumed.")
                    .details(msg)
            }
            AppError::InternalServerError(msg) => {
                // It's good practice to log internal errors on the server
                error!("Internal Server Error: {}", msg);
                // Avoid exposing internal details to the client
                body(StatusCode::INTERNAL_SERVER_ERROR, "internal", "An internal server error occurred.")
            }
        };

        let payload = Json(ErrorResponse {
            code: response.code,
            message: response.message,
            details: response.details,
            id: response.id,
        });

        let mut reply = (response.status, payload).into_response();
        if let Some(seconds) = retry_after {
            reply.headers_mut().insert(RETRY_AFTER, HeaderValue::from(seconds));
        }
        reply
    }
}

// This allows `?` to be used on `Json` extractors if the handler returns `Result<_, AppError>`
impl From<JsonRejection> for AppError {
    fn from(rejection: JsonRejection) -> Self {
        // You can customize the message further based on the type of JsonRejection
        // For example, distinguishing between JsonSyntaxError and JsonDataError
        AppError::JsonDeserializationError(format!(
            "Request payload deserialization error: {}",
            rejection.body_text()
        ))
    }
}

impl From<QueryRejection> for AppError {
    fn from(rejection: QueryRejection) -> Self {
        AppError::ValidationError(rejection.body_text())
    }
}

impl From<PathRejection> for AppError {
    fn from(_: PathRejection) -> Self {
        AppError::ValidationError("Order ID must be a UUID.".to_string())
    }
}
