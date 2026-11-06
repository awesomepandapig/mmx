//! Drives the whole router in-process, with fakes for the engine, Valkey and QuestDB.

use super::AppState;
use super::aeron_handler::{EngineLink, EngineRequest, OrderOutcome, OutboundOrder};
use super::halts::Halts;
use super::halts::testing::FakeHaltStore;
use super::identity::{PLAYER_HEADER, TrustedGameServer};
use super::markets::MarketRegistry;
use super::metrics::Metrics;
use super::order::{MESSAGE_SIZE, uuid_to_u64s};
use super::order_id::OrderIdRules;
use super::rate_limit::{Claim, Decision, RateLimiter, Unlimited};
use super::recorder::ReportStream;
use super::routes::router;
use super::store::{OpenOrder, OrderQuery, OrderStore, OrdersCommits, RecordedOrder, RecordedStatus};

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

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
const RULES: OrderIdRules =
    OrderIdRules { window: Duration::from_secs(60), session_timeout: Duration::from_secs(30) };
const ADMIN_TOKEN: &str = "test-admin-token";

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
    failing: AtomicBool,
    orders: Mutex<Vec<RecordedOrder>>,
    /// How many reads miss the orders, as when the recorder hasn't reached them yet.
    missed_reads: AtomicUsize,
    queries: Mutex<Vec<OrderQuery>>,
    open_in_market: Mutex<Vec<OpenOrder>>,
    market_queries: Mutex<Vec<(String, usize)>>,
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

    async fn orders(&self, query: &OrderQuery) -> Result<Vec<RecordedOrder>, String> {
        if self.failing.load(Ordering::Relaxed) {
            return Err("down".to_string());
        }
        self.queries.lock().unwrap().push(query.clone());
        let missed = self.missed_reads.try_update(Ordering::Relaxed, Ordering::Relaxed, |left| left.checked_sub(1));
        Ok(if missed.is_ok() { Vec::new() } else { self.orders.lock().unwrap().clone() })
    }

    async fn open_orders_in_market(&self, symbol: &str, limit: usize) -> Result<Vec<OpenOrder>, String> {
        if self.failing.load(Ordering::Relaxed) {
            return Err("down".to_string());
        }
        self.market_queries.lock().unwrap().push((symbol.to_string(), limit));
        Ok(self.open_in_market.lock().unwrap().clone())
    }
}

struct FakeLimiter(Result<Decision, String>);

#[async_trait]
impl RateLimiter for FakeLimiter {
    async fn check(&self, _player: Uuid, _claim: Option<Claim>) -> Result<Decision, String> {
        self.0.clone()
    }
}

/// Claims the way the script does, in one step that a repeat can't get past.
#[derive(Default)]
struct FakeClaims {
    claimed: Mutex<HashSet<(Uuid, Uuid)>>,
    counted: AtomicUsize,
}

#[async_trait]
impl RateLimiter for FakeClaims {
    async fn check(&self, player: Uuid, claim: Option<Claim>) -> Result<Decision, String> {
        let claim = claim.ok_or("the order wasn't claimed")?;
        if !self.claimed.lock().unwrap().insert((player, claim.id)) {
            return Ok(Decision::Duplicate);
        }
        self.counted.fetch_add(1, Ordering::Relaxed);
        Ok(Decision::Allowed)
    }
}

struct Gateway {
    app: Router,
    engine: Arc<FakeEngine>,
    store: Arc<FakeStore>,
    stream: Arc<ReportStream>,
    markets_file: PathBuf,
    player: Uuid,
    link: Arc<EngineLink>,
    halts: Arc<Halts>,
    claims_ready: Arc<AtomicBool>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.markets_file);
    }
}

struct Setup {
    outcomes: Vec<OrderOutcome>,
    limiter: Arc<dyn RateLimiter>,
    clustered: bool,
    halts: Option<Arc<Halts>>,
    admin_token: Option<String>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            outcomes: Vec::new(),
            limiter: Arc::new(Unlimited),
            clustered: true,
            halts: None,
            admin_token: Some(ADMIN_TOKEN.to_string()),
        }
    }
}

fn gateway(setup: Setup) -> Gateway {
    let engine = FakeEngine::answering(setup.outcomes);
    let store = Arc::new(FakeStore::default());
    let stream = Arc::new(ReportStream::default());
    let markets_file = std::env::temp_dir().join(format!("markets-{}.yaml", Uuid::new_v4()));
    fs::write(&markets_file, MARKETS).unwrap();

    let link = Arc::new(EngineLink::default());
    link.set(true);
    let halts = setup.halts.unwrap_or_else(|| Arc::new(Halts::standalone()));
    let claims_ready = Arc::new(AtomicBool::new(true));
    let state = AppState {
        orders: engine.start(),
        auth: Arc::new(TrustedGameServer),
        markets: MarketRegistry::load(&markets_file).unwrap(),
        rate_limiter: setup.limiter,
        store: store.clone(),
        report_stream: setup.clustered.then(|| stream.clone()),
        id_rules: setup.clustered.then_some(RULES),
        claims_ready: setup.clustered.then(|| claims_ready.clone()),
        engine: link.clone(),
        halts: halts.clone(),
        metrics: Metrics::default(),
        admin_token: setup.admin_token,
    };
    Gateway {
        app: router(Arc::new(state)),
        engine,
        store,
        stream,
        markets_file,
        player: Uuid::new_v4(),
        link,
        halts,
        claims_ready,
    }
}

struct Reply {
    status: StatusCode,
    retry_after: Option<String>,
    content_type: Option<String>,
    text: String,
    body: Value,
}

impl Gateway {
    async fn send(&self, method: Method, uri: &str, player: Option<&str>, body: Option<Value>) -> Reply {
        let request = Request::builder().method(method).uri(uri);
        self.dispatch(request, player, body).await
    }

    async fn dispatch(
        &self,
        mut request: axum::http::request::Builder,
        player: Option<&str>,
        body: Option<Value>,
    ) -> Reply {
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
        let content_type = response
            .headers()
            .get("content-type")
            .map(|value| value.to_str().unwrap().to_string());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let text = String::from_utf8_lossy(&bytes).to_string();
        Reply { status, retry_after, content_type, text, body }
    }

    async fn admin_as(&self, method: Method, uri: &str, token: Option<&str>) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        self.dispatch(request, None, None).await
    }

    async fn admin(&self, method: Method, uri: &str) -> Reply {
        self.admin_as(method, uri, Some(ADMIN_TOKEN)).await
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

/// An ID made this long ago.
fn id_aged(age: Duration) -> Uuid {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
    let then = now.saturating_sub(age);
    Uuid::new_v7(uuid::Timestamp::from_unix(uuid::NoContext, then.as_secs(), then.subsec_nanos()))
}

/// An ID old enough that its order can't land any more.
fn settled_id() -> Uuid {
    id_aged(RULES.settled_after() + Duration::from_secs(1))
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
    let id = Uuid::now_v7();
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
        "client_order_id": Uuid::now_v7(), "product_id": "ITEM", "side": "sell", "type": "market", "size": "1",
    });
    let reply = gateway.post(body).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert!(!reply.body.as_object().unwrap().contains_key("price"));
}

#[tokio::test]
async fn the_engine_receives_the_players_account_and_exact_values() {
    let gateway = gateway(Setup::default());
    let id = Uuid::now_v7();
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
    let body = limit_order(Uuid::now_v7());

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
    let good = limit_order(Uuid::now_v7());
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
    let reply = gateway.post(limit_order(Uuid::now_v7())).await;

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
    let mut body = limit_order(Uuid::now_v7());
    body["price"] = json!("1.01");
    assert_eq!(gateway.post(body).await.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn without_a_cluster_valkey_being_down_does_not_stop_trading() {
    let gateway = gateway(Setup {
        clustered: false,
        limiter: Arc::new(FakeLimiter(Err("connection refused".to_string()))),
        ..Default::default()
    });
    assert_eq!(gateway.post(limit_order(Uuid::new_v4())).await.status, StatusCode::CREATED);
}

#[tokio::test]
async fn in_a_cluster_an_unconfirmed_claim_sends_nothing() {
    let gateway = gateway(Setup {
        limiter: Arc::new(FakeLimiter(Err("timed out".to_string()))),
        ..Default::default()
    });
    let id = Uuid::now_v7();
    let reply = gateway.post(limit_order(id)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["code"], "claim_unavailable");
    assert_eq!(reply.body["id"], id.to_string());
    assert!(reply.body["details"].as_str().unwrap().contains("Nothing was sent"));
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test]
async fn cancels_work_while_claims_do_not() {
    let gateway = gateway(Setup {
        outcomes: vec![OrderOutcome::Canceled],
        limiter: Arc::new(FakeLimiter(Err("connection refused".to_string()))),
        ..Default::default()
    });
    assert_eq!(gateway.post(limit_order(Uuid::now_v7())).await.status, StatusCode::SERVICE_UNAVAILABLE);
    let uri = format!("/api/v1/orders/{}", Uuid::now_v7());
    assert_eq!(gateway.call(Method::DELETE, &uri, None).await.status, StatusCode::OK);
    assert_eq!(gateway.engine.sent(), 1, "only the cancel");
}

#[tokio::test]
async fn orders_wait_out_a_lost_claim_store_and_cancels_do_not() {
    let gateway = gateway(Setup {
        outcomes: vec![OrderOutcome::Canceled],
        limiter: Arc::new(FakeLimiter(Ok(Decision::Recovering { retry_after_ms: 2500 }))),
        ..Default::default()
    });
    let reply = gateway.post(limit_order(Uuid::now_v7())).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["code"], "claims_recovering");
    assert_eq!(reply.retry_after.as_deref(), Some("3"));
    assert_eq!(gateway.engine.sent(), 0);

    let uri = format!("/api/v1/orders/{}", Uuid::now_v7());
    assert_eq!(gateway.call(Method::DELETE, &uri, None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_second_post_of_an_id_places_nothing() {
    let claims = Arc::new(FakeClaims::default());
    let gateway = gateway(Setup { limiter: claims.clone(), ..Default::default() });
    let id = Uuid::now_v7();

    assert_eq!(gateway.post(limit_order(id)).await.status, StatusCode::CREATED);
    let reply = gateway.post(limit_order(id)).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.body["code"], "duplicate_order");
    assert_eq!(reply.body["id"], id.to_string());
    assert!(reply.body["details"].as_str().unwrap().contains("placed nothing new"));
    assert_eq!(gateway.engine.sent(), 1);
    assert_eq!(claims.counted.load(Ordering::Relaxed), 1, "a repeat isn't a new order");
}

#[tokio::test]
async fn simultaneous_posts_of_an_id_place_one_order() {
    let gateway = Arc::new(gateway(Setup {
        limiter: Arc::new(FakeClaims::default()),
        ..Default::default()
    }));
    let id = Uuid::now_v7();
    let posts: Vec<_> = (0..16)
        .map(|_| {
            let gateway = gateway.clone();
            tokio::spawn(async move { gateway.post(limit_order(id)).await.status })
        })
        .collect();
    let mut created = 0;
    for post in posts {
        created += usize::from(post.await.unwrap() == StatusCode::CREATED);
    }
    assert_eq!(created, 1);
    assert_eq!(gateway.engine.sent(), 1);
}

#[tokio::test]
async fn two_players_can_each_use_an_id() {
    let gateway = gateway(Setup { limiter: Arc::new(FakeClaims::default()), ..Default::default() });
    let id = Uuid::now_v7();
    let (first, second) = (Uuid::new_v4().to_string(), Uuid::new_v4().to_string());
    let uri = "/api/v1/orders";
    let reply = gateway.send(Method::POST, uri, Some(&first), Some(limit_order(id))).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let reply = gateway.send(Method::POST, uri, Some(&second), Some(limit_order(id))).await;
    assert_eq!(reply.status, StatusCode::CREATED);
}

#[tokio::test]
async fn an_order_id_must_be_a_fresh_uuid_v7() {
    let claims = Arc::new(FakeClaims::default());
    let gateway = gateway(Setup { limiter: claims.clone(), ..Default::default() });
    let future = Uuid::new_v7(uuid::Timestamp::from_unix(uuid::NoContext, 4_000_000_000, 0));
    for (id, code) in [
        (Uuid::new_v4(), "invalid_client_order_id"),
        (Uuid::nil(), "invalid_client_order_id"),
        (id_aged(RULES.window + Duration::from_secs(1)), "stale_client_order_id"),
        (future, "future_client_order_id"),
    ] {
        let reply = gateway.post(limit_order(id)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{code}");
        assert_eq!(reply.body["code"], code);
        assert_eq!(reply.body["id"], id.to_string());
    }
    assert_eq!(gateway.engine.sent(), 0);
    assert_eq!(claims.counted.load(Ordering::Relaxed), 0, "a refused ID spends nothing");
}

#[tokio::test]
async fn valkeys_clock_can_still_refuse_an_id() {
    let gateway = gateway(Setup {
        limiter: Arc::new(FakeLimiter(Ok(Decision::OutOfDate))),
        ..Default::default()
    });
    let reply = gateway.post(limit_order(Uuid::now_v7())).await;
    assert_eq!(reply.body["code"], "stale_client_order_id");
    assert_eq!(gateway.engine.sent(), 0);
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
    let id = Uuid::now_v7();
    let reply = gateway.post(limit_order(id)).await;

    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["id"], id.to_string());
    assert_eq!(reply.body["code"], "outcome_unknown");
    let details = reply.body["details"].as_str().unwrap();
    assert!(details.contains("Do not send this again"), "{details}");
    assert!(details.contains("on hold"), "{details}");
    assert!(details.contains("status"), "{details}");
}

#[tokio::test]
async fn an_engine_refusal_says_so() {
    let gateway = gateway(Setup { outcomes: vec![OrderOutcome::Rejected], ..Default::default() });
    let reply = gateway.post(limit_order(Uuid::now_v7())).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["code"], "order_rejected");
}

#[tokio::test]
async fn placing_an_order_never_touches_questdb() {
    let gateway = gateway(Setup::default());
    gateway.store.failing.store(true, Ordering::Relaxed);
    assert_eq!(gateway.post(limit_order(Uuid::now_v7())).await.status, StatusCode::CREATED);
    assert!(gateway.store.queries.lock().unwrap().is_empty());
}

#[tokio::test]
async fn without_a_cluster_an_id_is_optional_and_any_uuid() {
    let gateway = gateway(Setup { clustered: false, ..Default::default() });
    gateway.store.failing.store(true, Ordering::Relaxed);
    let mut body = limit_order(Uuid::new_v4());
    assert_eq!(gateway.post(body.clone()).await.status, StatusCode::CREATED);
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

impl Gateway {
    async fn status(&self, id: Uuid) -> Reply {
        self.call(Method::GET, &format!("/api/v1/orders/{id}"), None).await
    }

    /// Every report has been read, and the recorder has caught up.
    fn recorder_caught_up(&self) {
        self.stream.advance_to(4096);
        self.store.position.store(4096, Ordering::Relaxed);
    }

    fn cancels_sent(&self) -> usize {
        let received = self.engine.received.lock().unwrap();
        received.iter().filter(|(request, _)| matches!(request, EngineRequest::Cancel { .. })).count()
    }
}

fn answers_marker() -> Setup {
    Setup { outcomes: vec![OrderOutcome::NotOpen], ..Default::default() }
}

#[tokio::test]
async fn without_a_cluster_a_missing_order_is_a_404() {
    let gateway = gateway(Setup { clustered: false, ..Default::default() });
    assert_eq!(gateway.status(Uuid::new_v4()).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_fresh_id_with_no_record_is_unknown_not_a_404() {
    let gateway = gateway(Setup::default());
    gateway.recorder_caught_up();
    let id = Uuid::now_v7();
    let reply = gateway.status(id).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["status"], "unknown");
    assert_eq!(reply.body["settled"], false);
    assert_eq!(reply.body["id"], id.to_string());
    assert!(reply.body["message"].as_str().unwrap().contains("on hold"));
    assert_eq!(gateway.engine.sent(), 0, "nothing needs proving yet");
}

/// The session timeout has passed, but a POST of this ID would still be accepted.
#[tokio::test]
async fn an_id_that_could_still_be_posted_is_unknown() {
    let gateway = gateway(answers_marker());
    gateway.recorder_caught_up();
    for age in [RULES.session_timeout + Duration::from_secs(1), RULES.window, RULES.settled_after()] {
        let reply = gateway.status(id_aged(age - Duration::from_secs(1))).await;
        assert_eq!(reply.body["status"], "unknown", "{age:?}");
    }
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test]
async fn an_id_past_every_bound_with_no_record_never_landed() {
    let gateway = gateway(answers_marker());
    gateway.recorder_caught_up();
    let reply = gateway.status(settled_id()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["status"], "never_landed");
    assert_eq!(reply.body["settled"], true);
    assert_eq!(gateway.cancels_sent(), 1, "the marker went through the log");
}

#[tokio::test]
async fn the_marker_is_an_id_nobody_used_so_it_cancels_nothing() {
    let gateway = gateway(answers_marker());
    gateway.recorder_caught_up();
    let id = settled_id();
    gateway.status(id).await;
    let received = gateway.engine.received.lock().unwrap();
    let EngineRequest::Cancel { order: (account, target), .. } = received[0].0 else {
        panic!("expected a cancel");
    };
    assert_eq!(account, uuid_to_u64s(gateway.player));
    assert_ne!(target, uuid_to_u64s(id), "a status read must not cancel the order it asks about");
}

/// The first read can run ahead of the recorder, so only a read after the marker can say no.
#[tokio::test]
async fn an_order_that_appears_during_the_proof_is_reported() {
    let gateway = gateway(answers_marker());
    gateway.recorder_caught_up();
    let id = settled_id();
    let order = RecordedOrder { id, ..recorded(RecordedStatus::Filled, Some(100_000_000)) };
    *gateway.store.orders.lock().unwrap() = vec![order];
    gateway.store.missed_reads.store(1, Ordering::Relaxed);

    let reply = gateway.status(id).await;
    assert_eq!(reply.body["status"], "filled");
}

#[tokio::test]
async fn a_cluster_that_cannot_answer_the_marker_proves_nothing() {
    for outcome in [OrderOutcome::TimedOut, OrderOutcome::Failed("gone".to_string())] {
        let gateway = gateway(Setup { outcomes: vec![outcome], ..Default::default() });
        gateway.recorder_caught_up();
        assert_eq!(gateway.status(settled_id()).await.body["status"], "unknown");
    }
}

#[tokio::test(start_paused = true)]
async fn a_recorder_behind_the_marker_proves_nothing() {
    let gateway = gateway(answers_marker());
    gateway.stream.advance_to(4096);
    gateway.store.position.store(1024, Ordering::Relaxed);
    assert_eq!(gateway.status(settled_id()).await.body["status"], "unknown");
}

#[tokio::test(start_paused = true)]
async fn rows_behind_their_position_prove_nothing() {
    let gateway = gateway(answers_marker());
    gateway.recorder_caught_up();
    gateway.store.commits_applied.store(3, Ordering::Relaxed);
    gateway.store.commits_made.store(5, Ordering::Relaxed);
    assert_eq!(gateway.status(settled_id()).await.body["status"], "unknown");
}

#[tokio::test]
async fn a_gateway_that_never_read_the_stream_proves_nothing() {
    let gateway = gateway(answers_marker());
    assert_eq!(gateway.status(settled_id()).await.body["status"], "unknown");
}

#[tokio::test]
async fn questdb_being_down_is_unknown() {
    let gateway = gateway(answers_marker());
    gateway.recorder_caught_up();
    gateway.store.failing.store(true, Ordering::Relaxed);
    let reply = gateway.status(settled_id()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["status"], "unknown");
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test]
async fn an_id_with_no_time_in_it_is_unknown() {
    let gateway = gateway(answers_marker());
    gateway.recorder_caught_up();
    assert_eq!(gateway.status(Uuid::new_v4()).await.body["status"], "unknown");
}

#[test]
fn the_never_landed_wait_covers_a_post_at_the_end_of_the_window() {
    assert!(RULES.settled_after() > RULES.window + RULES.session_timeout);
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
    let mut body = limit_order(Uuid::now_v7());
    body["product_id"] = json!("GEM");
    assert_eq!(gateway.post(body.clone()).await.status, StatusCode::BAD_REQUEST);

    fs::write(&gateway.markets_file, "markets:\n  - {symbol: GEM, tick_size: \"0.05\"}\n").unwrap();
    assert_eq!(gateway.post(body.clone()).await.status, StatusCode::BAD_REQUEST, "needs a reload");

    let reply = gateway.admin(Method::POST, "/api/v1/admin/markets/reload").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, json!({"markets": ["GEM"]}));
    assert_eq!(gateway.post(body).await.status, StatusCode::CREATED);

    assert_eq!(gateway.post(limit_order(Uuid::now_v7())).await.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_bad_reload_reports_why_and_keeps_trading() {
    let gateway = gateway(Setup::default());
    fs::write(&gateway.markets_file, "markets: [").unwrap();

    let reply = gateway.admin(Method::POST, "/api/v1/admin/markets/reload").await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["code"], "markets_not_reloaded");
    assert!(reply.body["details"].is_string());

    assert_eq!(gateway.post(limit_order(Uuid::now_v7())).await.status, StatusCode::CREATED);
}

const HALT: &str = "/api/v1/admin/markets/item/halt";
const RESUME: &str = "/api/v1/admin/markets/item/resume";
const CANCEL_ALL: &str = "/api/v1/admin/markets/item/cancel-all";

fn shared_halts(store: &Arc<FakeHaltStore>) -> Arc<Halts> {
    Arc::new(Halts::shared(store.clone()))
}

fn open_order() -> OpenOrder {
    OpenOrder { account: Uuid::new_v4(), id: Uuid::new_v4() }
}

#[tokio::test]
async fn every_admin_route_needs_the_token() {
    let gateway = gateway(Setup::default());
    let routes = [
        (Method::GET, "/api/v1/admin/markets"),
        (Method::POST, "/api/v1/admin/markets/reload"),
        (Method::POST, HALT),
        (Method::POST, RESUME),
        (Method::POST, CANCEL_ALL),
    ];
    for (method, uri) in routes {
        let reply = gateway.admin_as(method.clone(), uri, None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(reply.body["code"], "unauthenticated", "{uri}");

        let reply = gateway.admin_as(method, uri, Some("wrong")).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    assert!(!gateway.halts.is_halted("ITEM"), "a refused halt changes nothing");
    assert!(gateway.store.market_queries.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_player_header_is_no_substitute_for_the_token() {
    let gateway = gateway(Setup::default());
    let reply = gateway.call(Method::POST, HALT, None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn without_a_token_admin_is_off_and_trading_is_not() {
    let gateway = gateway(Setup { admin_token: None, ..Default::default() });
    for token in [None, Some("anything"), Some("")] {
        let reply = gateway.admin_as(Method::POST, HALT, token).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.body["code"], "admin_disabled");
    }
    assert_eq!(gateway.post(limit_order(Uuid::now_v7())).await.status, StatusCode::CREATED);
    assert_eq!(gateway.send(Method::GET, "/healthz", None, None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn probes_need_no_auth() {
    let gateway = gateway(Setup { admin_token: None, ..Default::default() });
    for uri in ["/healthz", "/readyz", "/metrics"] {
        assert_eq!(gateway.send(Method::GET, uri, None, None).await.status, StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn ready_follows_the_engine() {
    let gateway = gateway(Setup::default());
    let reply = gateway.send(Method::GET, "/readyz", None, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.body,
        json!({"ready": true, "checks": {"engine": true, "halts_known": true, "order_claims": true}})
    );

    gateway.link.set(false);
    let reply = gateway.send(Method::GET, "/readyz", None, None).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["ready"], false);
    assert_eq!(reply.body["checks"]["engine"], false);
    assert_eq!(gateway.send(Method::GET, "/healthz", None, None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn ready_shows_valkey_being_down_without_leaving_rotation() {
    let store = Arc::new(FakeHaltStore::default());
    let gateway = gateway(Setup { halts: Some(shared_halts(&store)), ..Default::default() });
    gateway.halts.refresh().await;
    let reply = gateway.send(Method::GET, "/readyz", None, None).await;
    assert_eq!(reply.body["checks"]["valkey"], true);

    store.set_down(true);
    gateway.halts.refresh().await;
    let reply = gateway.send(Method::GET, "/readyz", None, None).await;
    assert_eq!(reply.status, StatusCode::OK, "cancels must keep reaching this gateway");
    assert_eq!(reply.body["ready"], true);
    assert_eq!(reply.body["checks"]["valkey"], false);
    assert_eq!(reply.body["checks"]["halts_known"], true);
}

#[tokio::test]
async fn ready_shows_claims_being_down_without_leaving_rotation() {
    let gateway = gateway(Setup::default());
    gateway.claims_ready.store(false, Ordering::Relaxed);
    let reply = gateway.send(Method::GET, "/readyz", None, None).await;
    assert_eq!(reply.status, StatusCode::OK, "cancels must keep reaching this gateway");
    assert_eq!(reply.body["checks"]["order_claims"], false);
}

#[tokio::test]
async fn without_a_cluster_ready_has_no_claims() {
    let gateway = gateway(Setup { clustered: false, ..Default::default() });
    let reply = gateway.send(Method::GET, "/readyz", None, None).await;
    assert!(reply.body["checks"].get("order_claims").is_none());
}

#[tokio::test]
async fn markets_list_with_strings_and_flags() {
    let gateway = gateway(Setup::default());
    fs::write(
        &gateway.markets_file,
        "markets:\n  - {symbol: ITEM, tick_size: \"0.05\", min_size: \"1\", max_size: \"100\"}\n  - {symbol: OLD, tick_size: \"1\", enabled: false}\n",
    )
    .unwrap();
    gateway.admin(Method::POST, "/api/v1/admin/markets/reload").await;
    gateway.admin(Method::POST, HALT).await;

    let reply = gateway.admin(Method::GET, "/api/v1/admin/markets").await;
    assert_eq!(reply.status, StatusCode::OK);
    let markets = reply.body["markets"].as_array().unwrap();
    assert_eq!(
        markets[0],
        json!({
            "symbol": "ITEM", "tick_size": "0.05", "min_size": "1", "max_size": "100",
            "enabled": true, "halted": true,
        })
    );
    assert_eq!(markets[1]["symbol"], "OLD");
    assert_eq!(markets[1]["enabled"], false);
    assert_eq!(markets[1]["halted"], false);
}

#[tokio::test]
async fn a_halted_market_refuses_new_orders_with_a_stable_code() {
    let gateway = gateway(Setup::default());
    let reply = gateway.admin(Method::POST, HALT).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body["symbol"], "ITEM");
    assert_eq!(reply.body["halted"], true);

    let id = Uuid::now_v7();
    let reply = gateway.post(limit_order(id)).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["code"], "market_halted");
    assert_eq!(reply.body["id"], id.to_string());
    assert_eq!(gateway.engine.sent(), 0);

    let reply = gateway.admin(Method::POST, RESUME).await;
    assert_eq!(reply.body["halted"], false);
    assert_eq!(gateway.post(limit_order(Uuid::now_v7())).await.status, StatusCode::CREATED);
}

#[tokio::test]
async fn a_halt_does_not_touch_other_markets() {
    let gateway = gateway(Setup::default());
    fs::write(
        &gateway.markets_file,
        "markets:\n  - {symbol: ITEM, tick_size: \"0.05\"}\n  - {symbol: GEM, tick_size: \"0.05\"}\n",
    )
    .unwrap();
    gateway.admin(Method::POST, "/api/v1/admin/markets/reload").await;
    gateway.admin(Method::POST, HALT).await;

    let mut body = limit_order(Uuid::now_v7());
    body["product_id"] = json!("GEM");
    assert_eq!(gateway.post(body).await.status, StatusCode::CREATED);
}

#[tokio::test]
async fn cancels_still_work_while_halted() {
    let gateway = gateway(Setup {
        outcomes: vec![OrderOutcome::Canceled, OrderOutcome::Canceled],
        ..Default::default()
    });
    gateway.admin(Method::POST, HALT).await;

    let uri = format!("/api/v1/orders/{}", Uuid::new_v4());
    assert_eq!(gateway.call(Method::DELETE, &uri, None).await.status, StatusCode::OK);

    gateway.stream.advance_to(4096);
    gateway.store.position.store(4096, Ordering::Relaxed);
    *gateway.store.orders.lock().unwrap() = vec![recorded(RecordedStatus::Open, Some(1))];
    let reply = gateway.call(Method::DELETE, "/api/v1/orders", None).await;
    assert_eq!(reply.body["canceled"].as_array().unwrap().len(), 1);
    assert_eq!(gateway.engine.sent(), 2);
}

#[tokio::test]
async fn cancels_work_on_a_gateway_that_never_read_the_halt_list() {
    let store = Arc::new(FakeHaltStore::default());
    store.set_down(true);
    let gateway = gateway(Setup {
        outcomes: vec![OrderOutcome::Canceled],
        halts: Some(shared_halts(&store)),
        ..Default::default()
    });
    gateway.halts.refresh().await;

    let reply = gateway.post(limit_order(Uuid::now_v7())).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["code"], "halt_state_unknown");
    assert_eq!(gateway.engine.sent(), 0);

    let uri = format!("/api/v1/orders/{}", Uuid::new_v4());
    assert_eq!(gateway.call(Method::DELETE, &uri, None).await.status, StatusCode::OK);

    let reply = gateway.send(Method::GET, "/readyz", None, None).await;
    assert_eq!(reply.status, StatusCode::OK, "it must stay reachable for cancels");
    assert_eq!(reply.body["checks"]["halts_known"], false);
    let reply = gateway.admin(Method::GET, "/api/v1/admin/markets").await;
    assert_eq!(reply.body["markets"][0]["halted"], true, "orders are being refused");
}

#[tokio::test]
async fn a_halt_on_one_gateway_reaches_another() {
    let store = Arc::new(FakeHaltStore::default());
    let first = gateway(Setup { halts: Some(shared_halts(&store)), ..Default::default() });
    let second = gateway(Setup { halts: Some(shared_halts(&store)), ..Default::default() });
    first.halts.refresh().await;
    second.halts.refresh().await;

    assert_eq!(first.admin(Method::POST, HALT).await.status, StatusCode::OK);
    second.halts.refresh().await;
    let reply = second.post(limit_order(Uuid::now_v7())).await;
    assert_eq!(reply.body["code"], "market_halted");
    assert_eq!(second.engine.sent(), 0);
}

#[tokio::test(start_paused = true)]
async fn polling_picks_up_a_halt_within_the_promised_time() {
    let store = Arc::new(FakeHaltStore::default());
    let halts = shared_halts(&store);
    halts.refresh().await;
    halts.start_polling();

    shared_halts(&store).halt("ITEM").await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(halts.is_halted("ITEM"));
}

#[tokio::test]
async fn a_halt_valkey_never_heard_is_reported_and_in_force_here() {
    let store = Arc::new(FakeHaltStore::default());
    let gateway = gateway(Setup { halts: Some(shared_halts(&store)), ..Default::default() });
    gateway.halts.refresh().await;
    store.set_down(true);

    let reply = gateway.admin(Method::POST, HALT).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["code"], "halt_not_shared");
    assert_eq!(gateway.post(limit_order(Uuid::now_v7())).await.body["code"], "market_halted");
}

#[tokio::test]
async fn a_resume_valkey_never_heard_keeps_the_market_halted() {
    let store = Arc::new(FakeHaltStore::default());
    let gateway = gateway(Setup { halts: Some(shared_halts(&store)), ..Default::default() });
    gateway.halts.refresh().await;
    gateway.admin(Method::POST, HALT).await;
    store.set_down(true);

    let reply = gateway.admin(Method::POST, RESUME).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["code"], "market_still_halted");
    assert_eq!(gateway.post(limit_order(Uuid::now_v7())).await.body["code"], "market_halted");
}

#[tokio::test]
async fn an_unknown_market_cannot_be_halted() {
    let gateway = gateway(Setup::default());
    let reply = gateway.admin(Method::POST, "/api/v1/admin/markets/typo/halt").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body["code"], "market_not_found");
    assert!(!gateway.halts.is_halted("TYPO"));

    let reply = gateway.admin(Method::POST, "/api/v1/admin/markets/a-b/halt").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_reload_does_not_lift_a_halt() {
    let gateway = gateway(Setup::default());
    gateway.admin(Method::POST, HALT).await;
    gateway.admin(Method::POST, "/api/v1/admin/markets/reload").await;
    assert_eq!(gateway.post(limit_order(Uuid::now_v7())).await.body["code"], "market_halted");
}

/// Lands a halt during the rate limit check, which sits between the order's two halt checks.
struct HaltingLimiter(Arc<Halts>);

#[async_trait]
impl RateLimiter for HaltingLimiter {
    async fn check(&self, _player: Uuid, _claim: Option<Claim>) -> Result<Decision, String> {
        self.0.halt("ITEM").await?;
        Ok(Decision::Allowed)
    }
}

#[tokio::test]
async fn a_halt_landing_during_the_checks_still_stops_the_order() {
    let halts = Arc::new(Halts::standalone());
    let gateway = gateway(Setup {
        limiter: Arc::new(HaltingLimiter(halts.clone())),
        halts: Some(halts),
        ..Default::default()
    });
    let reply = gateway.post(limit_order(Uuid::now_v7())).await;
    assert_eq!(reply.body["code"], "market_halted");
    assert_eq!(gateway.engine.sent(), 0);
}

struct CountingLimiter(AtomicUsize);

#[async_trait]
impl RateLimiter for CountingLimiter {
    async fn check(&self, _player: Uuid, _claim: Option<Claim>) -> Result<Decision, String> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(Decision::Allowed)
    }
}

#[tokio::test]
async fn a_refused_order_does_not_spend_the_limit() {
    let limiter = Arc::new(CountingLimiter(AtomicUsize::new(0)));
    let gateway = gateway(Setup { limiter: limiter.clone(), ..Default::default() });
    gateway.admin(Method::POST, HALT).await;
    gateway.post(limit_order(Uuid::now_v7())).await;
    assert_eq!(limiter.0.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn market_cancel_all_reports_every_answer_and_spans_players() {
    let gateway = gateway(Setup {
        outcomes: vec![OrderOutcome::Canceled, OrderOutcome::NotOpen, OrderOutcome::TimedOut],
        ..Default::default()
    });
    gateway.stream.advance_to(4096);
    gateway.store.position.store(4096, Ordering::Relaxed);
    let orders = [open_order(), open_order(), open_order()];
    *gateway.store.open_in_market.lock().unwrap() = orders.to_vec();

    let reply = gateway.admin(Method::POST, CANCEL_ALL).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, json!({"requested": 3, "canceled": 1, "failed": 1, "truncated": false}));
    assert_eq!(gateway.engine.sent(), 3);
    assert_eq!(*gateway.store.market_queries.lock().unwrap(), [("ITEM".to_string(), 10_000)]);

    for (index, order) in orders.iter().enumerate() {
        let message = gateway.engine.message(index);
        let header: MessageHeaderDecoder<ReadBuf<'_>> =
            MessageHeaderDecoder::default().wrap(ReadBuf::new(&message), 0);
        let cancel: OrderCancelRequestDecoder<'_> = OrderCancelRequestDecoder::default().header(header, 0);
        assert_eq!(cancel.account(), uuid_to_u64s(order.account), "each cancel carries its owner");
        assert_eq!(cancel.orig_cl_ord_id(), uuid_to_u64s(order.id));
    }
}

#[tokio::test]
async fn market_cancel_all_says_when_it_was_truncated() {
    let gateway = gateway(Setup::default());
    gateway.stream.advance_to(0);
    *gateway.store.open_in_market.lock().unwrap() = vec![open_order(); 10_000];

    let reply = gateway.admin(Method::POST, CANCEL_ALL).await;
    assert_eq!(reply.body["requested"], 10_000);
    assert_eq!(reply.body["truncated"], true);
}

#[tokio::test]
async fn market_cancel_all_does_not_halt() {
    let gateway = gateway(Setup::default());
    gateway.stream.advance_to(0);
    gateway.admin(Method::POST, CANCEL_ALL).await;
    assert!(!gateway.halts.is_halted("ITEM"));
    assert_eq!(gateway.post(limit_order(Uuid::now_v7())).await.status, StatusCode::CREATED);
}

#[tokio::test]
async fn market_cancel_all_reaches_a_market_taken_out_of_the_file() {
    let gateway = gateway(Setup { outcomes: vec![OrderOutcome::Canceled], ..Default::default() });
    gateway.stream.advance_to(0);
    *gateway.store.open_in_market.lock().unwrap() = vec![open_order()];

    let reply = gateway.admin(Method::POST, "/api/v1/admin/markets/gone/cancel-all").await;
    assert_eq!(reply.body["canceled"], 1);
}

#[tokio::test(start_paused = true)]
async fn market_cancel_all_waits_for_the_recorder_so_no_order_is_missed() {
    let gateway = gateway(Setup::default());
    gateway.stream.advance_to(4096);
    gateway.store.position.store(1024, Ordering::Relaxed);
    *gateway.store.open_in_market.lock().unwrap() = vec![open_order()];

    let reply = gateway.admin(Method::POST, CANCEL_ALL).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.body["code"], "recorder_behind");
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test]
async fn market_cancel_all_sends_nothing_when_questdb_is_down() {
    let gateway = gateway(Setup::default());
    gateway.stream.advance_to(0);
    gateway.store.failing.store(true, Ordering::Relaxed);
    let reply = gateway.admin(Method::POST, CANCEL_ALL).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(gateway.engine.sent(), 0);
}

#[tokio::test]
async fn without_a_cluster_a_published_cancel_is_not_counted_as_done() {
    let gateway = gateway(Setup { clustered: false, ..Default::default() });
    *gateway.store.open_in_market.lock().unwrap() = vec![open_order()];
    let reply = gateway.admin(Method::POST, CANCEL_ALL).await;
    assert_eq!(reply.body, json!({"requested": 1, "canceled": 0, "failed": 1, "truncated": false}));
}

#[tokio::test]
async fn metrics_count_orders_by_what_happened() {
    let gateway = gateway(Setup {
        outcomes: vec![OrderOutcome::Accepted, OrderOutcome::Rejected],
        ..Default::default()
    });
    gateway.post(limit_order(Uuid::now_v7())).await;
    gateway.post(limit_order(Uuid::now_v7())).await;
    let mut bad = limit_order(Uuid::now_v7());
    bad["price"] = json!("1.01");
    gateway.post(bad).await;
    gateway.admin(Method::POST, HALT).await;
    gateway.post(limit_order(Uuid::now_v7())).await;

    let reply = gateway.send(Method::GET, "/metrics", None, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.content_type.unwrap().starts_with("text/plain; version=0.0.4"));
    assert!(reply.text.contains("mmx_gateway_orders_accepted_total 1\n"), "{}", reply.text);
    assert!(reply.text.contains("{reason=\"engine_rejected\"} 1\n"), "{}", reply.text);
    assert!(reply.text.contains("{reason=\"invalid\"} 1\n"), "{}", reply.text);
    assert!(reply.text.contains("mmx_gateway_halted_rejects_total 1\n"), "{}", reply.text);
    assert!(reply.text.contains("mmx_gateway_orders_in_flight 0\n"), "{}", reply.text);
}

#[test]
fn a_position_never_moves_backwards() {
    let stream = ReportStream::default();
    assert_eq!(stream.position(), -1);
    stream.advance_to(100);
    stream.advance_to(50);
    assert_eq!(stream.position(), 100);
}
