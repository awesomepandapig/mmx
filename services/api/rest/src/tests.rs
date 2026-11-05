//! Drives the whole router in-process, with fakes for the engine, Valkey and QuestDB.

use super::AppState;
use super::aeron_handler::{EngineRequest, OrderOutcome, OutboundOrder};
use super::identity::{PLAYER_HEADER, TrustedGameServer};
use super::markets::MarketRegistry;
use super::order::{MESSAGE_SIZE, uuid_to_u64s};
use super::rate_limit::{Decision, RateLimiter, Unlimited};
use super::repeat::{RepeatGuard, ReportStream};
use super::routes::router;
use super::store::{OrderKey, OrderQuery, OrderStore, OrdersCommits, RecordedOrder, RecordedStatus};

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};

use http_body_util::BodyExt;

use sbe::ReadBuf;
use sbe::message_header_codec::MessageHeaderDecoder;
use sbe::new_order_single_codec::NewOrderSingleDecoder;
use sbe::order_cancel_request_codec::OrderCancelRequestDecoder;
use sbe::side_enum::SideEnum;

use serde_json::{Value, json};

use tokio::sync::mpsc;

use tower::ServiceExt;

use uuid::Uuid;

const MARKETS: &str = "markets:\n  - {symbol: ITEM, tick_size: \"0.05\", min_size: \"1\", max_size: \"100\"}\n";
const HOUR: Duration = Duration::from_secs(3600);

/// Answers each request in turn, and keeps what it was sent.
#[derive(Default)]
struct FakeEngine {
    outcomes: Mutex<VecDeque<OrderOutcome>>,
    received: Mutex<Vec<(EngineRequest, Vec<u8>)>>,
}

impl FakeEngine {
    fn answering(outcomes: Vec<OrderOutcome>) -> Arc<Self> {
        Arc::new(Self { outcomes: Mutex::new(outcomes.into()), ..Default::default() })
    }

    fn sent(&self) -> usize {
        self.received.lock().unwrap().len()
    }

    fn message(&self, index: usize) -> Vec<u8> {
        self.received.lock().unwrap()[index].1.clone()
    }

    fn start(self: &Arc<Self>) -> mpsc::Sender<OutboundOrder> {
        let (orders, mut incoming) = mpsc::channel::<OutboundOrder>(64);
        let engine = self.clone();
        tokio::spawn(async move {
            while let Some(order) = incoming.recv().await {
                engine.received.lock().unwrap().push((order.request, order.message));
                let outcome = engine.outcomes.lock().unwrap().pop_front();
                let _ = order.published.send(outcome.unwrap_or(OrderOutcome::Accepted));
            }
        });
        orders
    }
}

#[derive(Default)]
struct FakeStore {
    position: AtomicI64,
    commits_applied: AtomicI64,
    commits_made: AtomicI64,
    repeat: AtomicBool,
    failing: AtomicBool,
    orders: Mutex<Vec<RecordedOrder>>,
    queries: Mutex<Vec<OrderQuery>>,
    repeat_checks: AtomicUsize,
}

#[async_trait]
impl OrderStore for FakeStore {
    async fn recorder_position(&self) -> Result<i64, String> {
        if self.failing.load(Ordering::Relaxed) {
            return Err("down".to_string());
        }
        Ok(self.position.load(Ordering::Relaxed))
    }

    async fn orders_commits(&self) -> Result<OrdersCommits, String> {
        Ok(OrdersCommits {
            applied: self.commits_applied.load(Ordering::Relaxed),
            made: self.commits_made.load(Ordering::Relaxed),
        })
    }

    async fn order_reached_engine(&self, _key: &OrderKey, _window: Duration) -> Result<bool, String> {
        self.repeat_checks.fetch_add(1, Ordering::Relaxed);
        Ok(self.repeat.load(Ordering::Relaxed))
    }

    async fn orders(&self, query: &OrderQuery) -> Result<Vec<RecordedOrder>, String> {
        if self.failing.load(Ordering::Relaxed) {
            return Err("down".to_string());
        }
        self.queries.lock().unwrap().push(query.clone());
        Ok(self.orders.lock().unwrap().clone())
    }
}

struct FakeLimiter(Result<Decision, String>);

#[async_trait]
impl RateLimiter for FakeLimiter {
    async fn check(&self, _player: Uuid) -> Result<Decision, String> {
        self.0.clone()
    }
}

struct Gateway {
    app: Router,
    engine: Arc<FakeEngine>,
    store: Arc<FakeStore>,
    stream: Arc<ReportStream>,
    markets_file: PathBuf,
    player: Uuid,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.markets_file);
    }
}

struct Setup {
    outcomes: Vec<OrderOutcome>,
    limiter: Arc<dyn RateLimiter>,
    /// How long the retry window is. Memory covers a zero window from the start.
    window: Duration,
    clustered: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Self { outcomes: Vec::new(), limiter: Arc::new(Unlimited), window: Duration::ZERO, clustered: true }
    }
}

fn gateway(setup: Setup) -> Gateway {
    let engine = FakeEngine::answering(setup.outcomes);
    let store = Arc::new(FakeStore::default());
    let stream = Arc::new(ReportStream::default());
    let markets_file = std::env::temp_dir().join(format!("markets-{}.yaml", Uuid::new_v4()));
    fs::write(&markets_file, MARKETS).unwrap();

    let dyn_store: Arc<dyn OrderStore> = store.clone();
    let repeats = setup.clustered.then(|| RepeatGuard {
        stream: stream.clone(),
        store: dyn_store.clone(),
        window: setup.window,
    });
    let state = AppState {
        orders: engine.start(),
        auth: Arc::new(TrustedGameServer),
        markets: MarketRegistry::load(&markets_file).unwrap(),
        rate_limiter: setup.limiter,
        store: dyn_store,
        report_stream: setup.clustered.then(|| stream.clone()),
        repeats,
        require_client_order_id: setup.clustered,
    };
    Gateway {
        app: router(Arc::new(state)),
        engine,
        store,
        stream,
        markets_file,
        player: Uuid::new_v4(),
    }
}

struct Reply {
    status: StatusCode,
    retry_after: Option<String>,
    body: Value,
}

impl Gateway {
    async fn send(&self, method: Method, uri: &str, player: Option<&str>, body: Option<Value>) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(player) = player {
            request = request.header(PLAYER_HEADER, player);
        }
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        };
        let response = self.app.clone().oneshot(request.unwrap()).await.unwrap();
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .map(|value| value.to_str().unwrap().to_string());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Reply { status, retry_after, body }
    }

    async fn call(&self, method: Method, uri: &str, body: Option<Value>) -> Reply {
        let player = self.player.to_string();
        self.send(method, uri, Some(&player), body).await
    }

    async fn post(&self, body: Value) -> Reply {
        self.call(Method::POST, "/api/v1/orders", Some(body)).await
    }
}

fn limit_order(id: Uuid) -> Value {
    json!({
        "client_order_id": id, "product_id": "item", "side": "buy", "type": "limit",
        "size": "2.5", "price": "1.05",
    })
}

fn recorded(status: RecordedStatus, price: Option<i64>) -> RecordedOrder {
    RecordedOrder {
        id: Uuid::new_v4(),
        symbol: "ITEM".to_string(),
        side: SideEnum::Sell,
        status,
        leaves_mantissa: 150_000_000,
        filled_mantissa: 50_000_000,
        price_mantissa: price,
        average_price_mantissa: Some(200_000_000),
        created_at: "2026-11-04T12:00:00.000000Z".to_string(),
        updated_at: "2026-11-04T12:00:01.000000Z".to_string(),
    }
}

#[tokio::test]
async fn an_accepted_order_returns_strings_and_omits_nulls() {
    let gateway = gateway(Setup::default());
    let id = Uuid::new_v4();
    let reply = gateway.post(limit_order(id)).await;

    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.body["id"], id.to_string());
    assert_eq!(reply.body["product_id"], "ITEM");
    assert_eq!(reply.body["price"], "1.05");
    assert_eq!(reply.body["size"], "2.5");
    assert_eq!(reply.body["executed_value"], "0");
    assert_eq!(reply.body["filled_size"], "0");
    assert_eq!(reply.body["status"], "open");
    let fields = reply.body.as_object().unwrap();
    assert!(!fields.contains_key("cancel_after"), "nulls are omitted: {fields:?}");
    assert!(!fields.contains_key("updated_at"), "nulls are omitted: {fields:?}");
}

#[tokio::test]
async fn a_market_order_has_no_price_field() {
    let gateway = gateway(Setup::default());
    let body = json!({
        "client_order_id": Uuid::new_v4(), "product_id": "ITEM", "side": "sell", "type": "market", "size": "1",
    });
    let reply = gateway.post(body).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert!(!reply.body.as_object().unwrap().contains_key("price"));
}

#[tokio::test]
async fn the_engine_receives_the_players_account_and_exact_values() {
    let gateway = gateway(Setup::default());
    let id = Uuid::new_v4();
    gateway.post(limit_order(id)).await;

    let message = gateway.engine.message(0);
    assert_eq!(message.len(), MESSAGE_SIZE);
    let header: MessageHeaderDecoder<ReadBuf<'_>> =
        MessageHeaderDecoder::default().wrap(ReadBuf::new(&message), 0);
    assert_eq!(header.template_id(), 1);
    let order: NewOrderSingleDecoder<'_> = NewOrderSingleDecoder::default().header(header, 0);
    assert_eq!(order.account(), uuid_to_u64s(gateway.player));
    assert_eq!(order.cl_ord_id(), uuid_to_u64s(id));
    assert_eq!(&order.symbol(), b"ITEM\0\0");
    assert_eq!(order.order_qty_decoder().mantissa(), 250_000_000);
    assert_eq!(order.price_decoder().mantissa(), 105_000_000);
}

#[tokio::test]
async fn a_player_is_required() {
    let gateway = gateway(Setup::default());
    let body = limit_order(Uuid::new_v4());

    let reply = gateway.send(Method::POST, "/api/v1/orders", None, Some(body.clone())).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.body["code"], "unauthenticated");

    let reply = gateway.send(Method::POST, "/api/v1/orders", Some("not-a-uuid"), Some(body)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    for (method, uri) in [
        (Method::GET, "/api/v1/orders".to_string()),
        (Method::DELETE, "/api/v1/orders".to_string()),
        (Method::GET, format!("/api/v1/orders/{}", Uuid::new_v4())),
        (Method::DELETE, format!("/api/v1/orders/{}", Uuid::new_v4())),
    ] {
        let reply = gateway.send(method, &uri, None, None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test]
async fn bad_requests_never_reach_the_engine() {
    let gateway = gateway(Setup::default());
    let good = limit_order(Uuid::new_v4());
    let with = |field: &str, value: Value| {
        let mut body = good.clone();
        body[field] = value;
        body
    };

    for (body, status) in [
        (with("price", json!("1.01")), StatusCode::BAD_REQUEST), // Off the tick
        (with("product_id", json!("OTHER")), StatusCode::BAD_REQUEST), // Not enabled
        (with("product_id", json!("A-B")), StatusCode::BAD_REQUEST),
        (with("size", json!("0.5")), StatusCode::BAD_REQUEST), // Below min_size
        (with("size", json!("101")), StatusCode::BAD_REQUEST), // Above max_size
        (with("side", json!("hold")), StatusCode::BAD_REQUEST),
        (with("type", json!("stop")), StatusCode::BAD_REQUEST),
        (with("client_order_id", Value::Null), StatusCode::BAD_REQUEST),
        (with("size", json!(1.5)), StatusCode::UNPROCESSABLE_ENTITY), // Must be a string
        (with("price", json!(1)), StatusCode::UNPROCESSABLE_ENTITY),
        (with("surprise", json!(true)), StatusCode::UNPROCESSABLE_ENTITY),
        (json!({}), StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let reply = gateway.post(body.clone()).await;
        assert_eq!(reply.status, status, "{body}: {}", reply.body);
        assert!(reply.body["message"].is_string(), "errors are JSON: {body}");
    }
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test]
async fn a_limited_player_is_bounced_before_the_engine() {
    let gateway = gateway(Setup {
        limiter: Arc::new(FakeLimiter(Ok(Decision::Limited { retry_after_ms: 1500 }))),
        ..Default::default()
    });
    let reply = gateway.post(limit_order(Uuid::new_v4())).await;

    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(reply.retry_after.as_deref(), Some("2"));
    assert_eq!(reply.body["code"], "rate_limited");
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test]
async fn an_invalid_order_does_not_spend_the_limit() {
    let gateway = gateway(Setup {
        limiter: Arc::new(FakeLimiter(Ok(Decision::Limited { retry_after_ms: 1 }))),
        ..Default::default()
    });
    let mut body = limit_order(Uuid::new_v4());
    body["price"] = json!("1.01");
    assert_eq!(gateway.post(body).await.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn valkey_being_down_does_not_stop_trading() {
    let gateway = gateway(Setup {
        limiter: Arc::new(FakeLimiter(Err("connection refused".to_string()))),
        ..Default::default()
    });
    assert_eq!(gateway.post(limit_order(Uuid::new_v4())).await.status, StatusCode::CREATED);
}

#[tokio::test]
async fn cancels_are_not_rate_limited() {
    let gateway = gateway(Setup {
        outcomes: vec![OrderOutcome::Canceled],
        limiter: Arc::new(FakeLimiter(Ok(Decision::Limited { retry_after_ms: 1 }))),
        ..Default::default()
    });
    let uri = format!("/api/v1/orders/{}", Uuid::new_v4());
    assert_eq!(gateway.call(Method::DELETE, &uri, None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn an_unknown_outcome_is_a_503_with_the_same_id() {
    let gateway = gateway(Setup { outcomes: vec![OrderOutcome::TimedOut], ..Default::default() });
    let id = Uuid::new_v4();
    let reply = gateway.post(limit_order(id)).await;

    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["id"], id.to_string());
    assert_eq!(reply.body["code"], "outcome_unknown");
}

#[tokio::test]
async fn engine_refusals_say_which_kind_they_are() {
    let gateway = gateway(Setup {
        outcomes: vec![OrderOutcome::Rejected, OrderOutcome::Repeat],
        ..Default::default()
    });

    let reply = gateway.post(limit_order(Uuid::new_v4())).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["code"], "order_rejected");

    let id = Uuid::new_v4();
    let reply = gateway.post(limit_order(id)).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["code"], "repeat_order");
    assert_eq!(reply.body["id"], id.to_string());
}

#[tokio::test]
async fn orders_skip_questdb_once_memory_covers_the_window() {
    let gateway = gateway(Setup::default());
    gateway.store.failing.store(true, Ordering::Relaxed);
    assert_eq!(gateway.post(limit_order(Uuid::new_v4())).await.status, StatusCode::CREATED);
    assert_eq!(gateway.store.repeat_checks.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn after_a_start_a_repeat_is_bounced_from_history() {
    let gateway = gateway(Setup { window: HOUR, ..Default::default() });
    gateway.stream.advance_to(4096);
    gateway.store.position.store(4096, Ordering::Relaxed);
    gateway.store.repeat.store(true, Ordering::Relaxed);

    let id = Uuid::new_v4();
    let reply = gateway.post(limit_order(id)).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["code"], "repeat_order");
    assert_eq!(reply.body["id"], id.to_string());
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test]
async fn after_a_start_a_new_order_goes_through() {
    let gateway = gateway(Setup { window: HOUR, ..Default::default() });
    gateway.stream.advance_to(4096);
    gateway.store.position.store(8192, Ordering::Relaxed);

    assert_eq!(gateway.post(limit_order(Uuid::new_v4())).await.status, StatusCode::CREATED);
    assert_eq!(gateway.store.repeat_checks.load(Ordering::Relaxed), 1);
    assert_eq!(gateway.engine.sent(), 1);
}

#[tokio::test]
async fn history_is_not_read_until_the_recorder_catches_up() {
    let gateway = Arc::new(gateway(Setup { window: HOUR, ..Default::default() }));
    gateway.stream.advance_to(4096);
    gateway.store.position.store(1024, Ordering::Relaxed);

    let request = {
        let gateway = gateway.clone();
        tokio::spawn(async move { gateway.post(limit_order(Uuid::new_v4())).await.status })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(gateway.store.repeat_checks.load(Ordering::Relaxed), 0, "the recorder is behind");

    gateway.store.position.store(4096, Ordering::Relaxed);
    assert_eq!(request.await.unwrap(), StatusCode::CREATED);
    assert_eq!(gateway.store.repeat_checks.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn history_waits_for_rows_written_ahead_of_the_position() {
    let gateway = Arc::new(gateway(Setup { window: HOUR, ..Default::default() }));
    gateway.stream.advance_to(4096);
    gateway.store.position.store(4096, Ordering::Relaxed);
    gateway.store.commits_applied.store(3, Ordering::Relaxed);
    gateway.store.commits_made.store(5, Ordering::Relaxed);

    let request = {
        let gateway = gateway.clone();
        tokio::spawn(async move { gateway.post(limit_order(Uuid::new_v4())).await.status })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(gateway.store.repeat_checks.load(Ordering::Relaxed), 0, "rows aren't applied yet");

    // Later commits don't move the goal, or a busy table could never catch up.
    gateway.store.commits_made.store(9, Ordering::Relaxed);
    gateway.store.commits_applied.store(5, Ordering::Relaxed);
    assert_eq!(request.await.unwrap(), StatusCode::CREATED);
    assert_eq!(gateway.store.repeat_checks.load(Ordering::Relaxed), 1);
}

#[tokio::test(start_paused = true)]
async fn a_recorder_that_never_catches_up_sends_nothing() {
    let gateway = gateway(Setup { window: HOUR, ..Default::default() });
    gateway.stream.advance_to(4096);
    gateway.store.position.store(1024, Ordering::Relaxed);

    let reply = gateway.post(limit_order(Uuid::new_v4())).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["code"], "recorder_behind");
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test(start_paused = true)]
async fn questdb_being_down_during_the_window_sends_nothing() {
    let gateway = gateway(Setup { window: HOUR, ..Default::default() });
    gateway.stream.advance_to(4096);
    gateway.store.failing.store(true, Ordering::Relaxed);

    let reply = gateway.post(limit_order(Uuid::new_v4())).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test]
async fn an_unknown_stream_position_cannot_be_checked() {
    let gateway = gateway(Setup { window: HOUR, ..Default::default() });
    let reply = gateway.post(limit_order(Uuid::new_v4())).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test]
async fn an_empty_stream_has_nothing_to_catch_up_to() {
    let gateway = gateway(Setup { window: HOUR, ..Default::default() });
    gateway.stream.advance_to(0);
    assert_eq!(gateway.post(limit_order(Uuid::new_v4())).await.status, StatusCode::CREATED);
}

#[tokio::test]
async fn without_a_cluster_nothing_checks_history() {
    let gateway = gateway(Setup { clustered: false, window: HOUR, ..Default::default() });
    gateway.store.failing.store(true, Ordering::Relaxed);
    let mut body = limit_order(Uuid::new_v4());
    body.as_object_mut().unwrap().remove("client_order_id");
    assert_eq!(gateway.post(body).await.status, StatusCode::CREATED);
}

#[tokio::test]
async fn cancelling_sends_the_owning_account() {
    let gateway = gateway(Setup { outcomes: vec![OrderOutcome::Canceled], ..Default::default() });
    let id = Uuid::new_v4();
    let reply = gateway.call(Method::DELETE, &format!("/api/v1/orders/{id}"), None).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, json!({"id": id, "status": "canceled"}));

    let message = gateway.engine.message(0);
    let header: MessageHeaderDecoder<ReadBuf<'_>> =
        MessageHeaderDecoder::default().wrap(ReadBuf::new(&message), 0);
    assert_eq!(header.template_id(), 2);
    let cancel: OrderCancelRequestDecoder<'_> = OrderCancelRequestDecoder::default().header(header, 0);
    assert_eq!(cancel.orig_cl_ord_id(), uuid_to_u64s(id));
    assert_eq!(cancel.account(), uuid_to_u64s(gateway.player));
    assert_ne!(cancel.cl_ord_id(), uuid_to_u64s(id), "a cancel has its own ID");
}

#[tokio::test]
async fn cancelling_what_is_not_open_is_a_404() {
    let gateway = gateway(Setup { outcomes: vec![OrderOutcome::NotOpen], ..Default::default() });
    let reply = gateway.call(Method::DELETE, &format!("/api/v1/orders/{}", Uuid::new_v4()), None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_cancel_with_no_answer_is_a_503_with_the_same_id() {
    let gateway = gateway(Setup { outcomes: vec![OrderOutcome::TimedOut], ..Default::default() });
    let id = Uuid::new_v4();
    let reply = gateway.call(Method::DELETE, &format!("/api/v1/orders/{id}"), None).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["id"], id.to_string());
}

#[tokio::test]
async fn without_a_cluster_a_cancel_is_only_accepted() {
    let gateway = gateway(Setup { clustered: false, ..Default::default() });
    let reply = gateway.call(Method::DELETE, &format!("/api/v1/orders/{}", Uuid::new_v4()), None).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.body["status"], "pending");
}

#[tokio::test]
async fn order_ids_must_be_uuids() {
    let gateway = gateway(Setup::default());
    for method in [Method::GET, Method::DELETE] {
        let reply = gateway.call(method, "/api/v1/orders/not-a-uuid", None).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(reply.body["message"].is_string());
    }
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test]
async fn cancel_all_cancels_each_open_order_and_reports_each_result() {
    let gateway = gateway(Setup {
        outcomes: vec![OrderOutcome::Canceled, OrderOutcome::NotOpen, OrderOutcome::TimedOut],
        ..Default::default()
    });
    gateway.stream.advance_to(4096);
    gateway.store.position.store(4096, Ordering::Relaxed);
    let orders = vec![
        recorded(RecordedStatus::Open, Some(1)),
        recorded(RecordedStatus::Open, Some(1)),
        recorded(RecordedStatus::Open, Some(1)),
    ];
    let ids: Vec<Uuid> = orders.iter().map(|order| order.id).collect();
    *gateway.store.orders.lock().unwrap() = orders;

    let reply = gateway.call(Method::DELETE, "/api/v1/orders?product_id=item", None).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["canceled"], json!([ids[0]]));
    assert_eq!(reply.body["not_open"], json!([ids[1]]));
    assert_eq!(reply.body["unresolved"], json!([ids[2]]));
    assert_eq!(reply.body["more"], false);
    assert_eq!(gateway.engine.sent(), 3);

    let queries = gateway.store.queries.lock().unwrap();
    assert_eq!(queries[0].account, gateway.player);
    assert_eq!(queries[0].symbol.as_deref(), Some("ITEM"));
    assert!(queries[0].open_only);
}

#[tokio::test(start_paused = true)]
async fn cancel_all_waits_for_the_recorder_so_no_order_is_missed() {
    let gateway = gateway(Setup::default());
    gateway.stream.advance_to(4096);
    gateway.store.position.store(1024, Ordering::Relaxed);
    *gateway.store.orders.lock().unwrap() = vec![recorded(RecordedStatus::Open, Some(1))];

    let reply = gateway.call(Method::DELETE, "/api/v1/orders", None).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["code"], "recorder_behind");
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test]
async fn orders_are_listed_with_strings_and_no_nulls() {
    let gateway = gateway(Setup::default());
    *gateway.store.orders.lock().unwrap() =
        vec![recorded(RecordedStatus::Open, Some(225_000_000)), recorded(RecordedStatus::Canceled, None)];

    let reply = gateway.call(Method::GET, "/api/v1/orders?status=open&product_id=item&limit=5", None).await;

    assert_eq!(reply.status, StatusCode::OK);
    let orders = reply.body.as_array().unwrap();
    assert_eq!(orders[0]["status"], "open");
    assert_eq!(orders[0]["settled"], false);
    assert_eq!(orders[0]["side"], "sell");
    assert_eq!(orders[0]["type"], "limit");
    assert_eq!(orders[0]["price"], "2.25");
    assert_eq!(orders[0]["size"], "2");
    assert_eq!(orders[0]["filled_size"], "0.5");
    assert_eq!(orders[0]["executed_value"], "1");
    assert_eq!(orders[1]["status"], "canceled");
    assert_eq!(orders[1]["settled"], true);
    assert_eq!(orders[1]["type"], "market");
    assert!(!orders[1].as_object().unwrap().contains_key("price"));

    let queries = gateway.store.queries.lock().unwrap();
    assert_eq!(queries[0].account, gateway.player);
    assert!(queries[0].open_only);
    assert_eq!(queries[0].symbol.as_deref(), Some("ITEM"));
    assert_eq!(queries[0].limit, 5);
}

#[tokio::test]
async fn listing_defaults_to_every_status() {
    let gateway = gateway(Setup::default());
    gateway.call(Method::GET, "/api/v1/orders", None).await;
    let queries = gateway.store.queries.lock().unwrap();
    assert!(!queries[0].open_only);
    assert_eq!(queries[0].limit, 100);
}

#[tokio::test]
async fn bad_list_parameters_are_refused() {
    let gateway = gateway(Setup::default());
    for query in ["limit=0", "limit=1001", "limit=x", "status=mystery", "surprise=1", "product_id=a'b"] {
        let reply = gateway.call(Method::GET, &format!("/api/v1/orders?{query}"), None).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query}");
        assert!(reply.body["message"].is_string(), "{query}");
    }
    assert!(gateway.store.queries.lock().unwrap().is_empty());
}

#[tokio::test]
async fn one_order_is_read_for_its_own_player() {
    let gateway = gateway(Setup::default());
    let order = recorded(RecordedStatus::Filled, Some(100_000_000));
    let id = order.id;
    *gateway.store.orders.lock().unwrap() = vec![order];

    let reply = gateway.call(Method::GET, &format!("/api/v1/orders/{id}"), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["id"], id.to_string());
    assert_eq!(reply.body["status"], "filled");

    let queries = gateway.store.queries.lock().unwrap();
    assert_eq!(queries[0].account, gateway.player);
    assert_eq!(queries[0].cl_ord_id, Some(id));
}

#[tokio::test]
async fn a_missing_order_is_a_404() {
    let gateway = gateway(Setup::default());
    let reply = gateway.call(Method::GET, &format!("/api/v1/orders/{}", Uuid::new_v4()), None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reads_fail_cleanly_when_questdb_is_down() {
    let gateway = gateway(Setup::default());
    gateway.store.failing.store(true, Ordering::Relaxed);
    let reply = gateway.call(Method::GET, "/api/v1/orders", None).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["code"], "recorder_behind");
}

#[tokio::test]
async fn a_reload_changes_which_markets_are_open() {
    let gateway = gateway(Setup::default());
    let mut body = limit_order(Uuid::new_v4());
    body["product_id"] = json!("GEM");
    assert_eq!(gateway.post(body.clone()).await.status, StatusCode::BAD_REQUEST);

    fs::write(&gateway.markets_file, "markets:\n  - {symbol: GEM, tick_size: \"0.05\"}\n").unwrap();
    assert_eq!(gateway.post(body.clone()).await.status, StatusCode::BAD_REQUEST, "needs a reload");

    let reply = gateway.call(Method::POST, "/api/v1/admin/markets/reload", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, json!({"markets": ["GEM"]}));
    assert_eq!(gateway.post(body).await.status, StatusCode::CREATED);

    assert_eq!(gateway.post(limit_order(Uuid::new_v4())).await.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_bad_reload_reports_why_and_keeps_trading() {
    let gateway = gateway(Setup::default());
    fs::write(&gateway.markets_file, "markets: [").unwrap();

    let reply = gateway.call(Method::POST, "/api/v1/admin/markets/reload", None).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["code"], "markets_not_reloaded");
    assert!(reply.body["details"].is_string());

    assert_eq!(gateway.post(limit_order(Uuid::new_v4())).await.status, StatusCode::CREATED);
}

#[test]
fn memory_covers_a_window_only_once_it_has_been_reading_for_that_long() {
    let stream = ReportStream::default();
    assert!(stream.covers(Duration::ZERO));
    assert!(!stream.covers(HOUR));

    std::thread::sleep(Duration::from_millis(20));
    assert!(stream.covers(Duration::from_millis(10)));

    stream.covered_since(std::time::Instant::now());
    assert!(!stream.covers(Duration::from_millis(10)), "losing the stream starts again");
}

#[test]
fn a_position_never_moves_backwards() {
    let stream = ReportStream::default();
    assert_eq!(stream.position(), -1);
    stream.advance_to(100);
    stream.advance_to(50);
    assert_eq!(stream.position(), 100);
}
