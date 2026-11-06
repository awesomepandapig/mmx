//! Drives the real server over HTTP against fake services, built from the shapes in
//! `services/api/rest`, `services/outbox` and `services/trade-reporter`.

mod console;

use crate::auth::{Auth, Managed};
use crate::events::{Activity, Poller};
use crate::feed::Feeds;
use crate::markets::MarketsFile;
use crate::server::{self, AppState};
use crate::store::{ConfigStore, LocalStore};
use crate::upstream::{Filter, Targets, Upstreams};

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Query, Request, State};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, routing::any};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const TOKEN: &str = "test-token";
const FILE: &str = "# Keep me\nmarkets:\n  - symbol: ITEM\n    tick_size: \"0.01\"\n";

async fn spawn(router: Router) -> String {
    crate::upstream::install_tls();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}")
}

/// A port nothing listens on, for a service that's down.
async fn dead_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

#[derive(Debug, Clone, PartialEq)]
struct Call {
    method: String,
    path: String,
    auth: Option<String>,
}

/// A gateway with the reload behaviour of `services/api/rest` and the agreed admin routes. It reads
/// its own file, which may or may not be the one the server edits.
struct FakeGateway {
    file: PathBuf,
    calls: Mutex<Vec<Call>>,
    halted: Mutex<HashSet<String>>,
    /// Markets as of the last reload, as a real gateway keeps them.
    loaded: Mutex<Vec<Value>>,
    fail_reload: bool,
    cancel_status: u16,
}

impl FakeGateway {
    fn new(file: &Path) -> Arc<Self> {
        Self::with(file, false, 200)
    }

    fn with(file: &Path, fail_reload: bool, cancel_status: u16) -> Arc<Self> {
        let gateway = Self {
            file: file.to_path_buf(),
            calls: Mutex::default(),
            halted: Mutex::default(),
            loaded: Mutex::default(),
            fail_reload,
            cancel_status,
        };
        gateway.load();
        Arc::new(gateway)
    }

    fn load(&self) -> Vec<String> {
        let file = MarketsFile::parse(&fs::read_to_string(&self.file).unwrap()).unwrap();
        let enabled: Vec<_> = file
            .markets
            .iter()
            .filter(|entry| entry.is_enabled())
            .collect();
        let default_max = "92233720368.54775807".to_string();
        *self.loaded.lock().unwrap() = enabled
            .iter()
            .map(|entry| {
                json!({
                    "symbol": entry.symbol,
                    "tick_size": entry.tick_size,
                    "min_size": entry.min_size.clone().unwrap_or_else(|| "0.00000001".to_string()),
                    "max_size": entry.max_size.clone().unwrap_or_else(|| default_max.clone()),
                    "enabled": true,
                })
            })
            .collect();
        enabled.iter().map(|entry| entry.symbol.clone()).collect()
    }

    fn market_list(&self) -> Vec<Value> {
        let halted = self.halted.lock().unwrap();
        let mut markets = self.loaded.lock().unwrap().clone();
        for market in &mut markets {
            market["halted"] = json!(halted.contains(market["symbol"].as_str().unwrap()));
        }
        markets
    }

    fn calls_to(&self, ending: &str) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.path.ends_with(ending))
            .cloned()
            .collect()
    }
}

async fn gateway_route(State(gateway): State<Arc<FakeGateway>>, request: Request) -> Response {
    let (method, path) = (request.method().clone(), request.uri().path().to_string());
    let auth = request
        .headers()
        .get("authorization")
        .map(|value| value.to_str().unwrap().to_string());
    gateway.calls.lock().unwrap().push(Call {
        method: method.to_string(),
        path: path.clone(),
        auth,
    });

    let market = path
        .strip_prefix("/api/v1/admin/markets/")
        .map(|rest| rest.split_once('/').unwrap_or((rest, "")));
    if let Some((symbol, action)) = market {
        let known = gateway
            .loaded
            .lock()
            .unwrap()
            .iter()
            .any(|market| market["symbol"] == symbol);
        if !action.is_empty() && !known {
            let body = json!({ "code": "market_not_found", "message": "No such market." });
            return (StatusCode::NOT_FOUND, Json(body)).into_response();
        }
    }
    match (method, path.as_str(), market) {
        (Method::GET, "/readyz", _) => {
            Json(json!({ "ready": true, "checks": { "engine": true } })).into_response()
        }
        (Method::GET, "/api/v1/admin/markets", _) => {
            Json(json!({ "markets": gateway.market_list() })).into_response()
        }
        (Method::POST, "/api/v1/admin/markets/reload", _) => {
            if gateway.fail_reload {
                let body = json!({ "code": "markets_not_reloaded", "message": "The markets file was not applied.", "details": "bad" });
                return (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response();
            }
            Json(json!({ "markets": gateway.load() })).into_response()
        }
        (Method::POST, _, Some((symbol, "halt"))) => {
            gateway.halted.lock().unwrap().insert(symbol.to_string());
            Json(json!({ "symbol": symbol, "tick_size": "0.01", "min_size": "1", "max_size": "2", "enabled": true, "halted": true })).into_response()
        }
        (Method::POST, _, Some((symbol, "resume"))) => {
            gateway.halted.lock().unwrap().remove(symbol);
            Json(json!({ "symbol": symbol, "tick_size": "0.01", "min_size": "1", "max_size": "2", "enabled": true, "halted": false })).into_response()
        }
        (Method::POST, _, Some((_, "cancel-all"))) => {
            let status = StatusCode::from_u16(gateway.cancel_status).unwrap();
            (
                status,
                Json(json!({ "requested": 3, "canceled": 2, "failed": 1, "truncated": false })),
            )
                .into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn fake_gateway(gateway: Arc<FakeGateway>) -> Router {
    Router::new()
        .fallback(any(gateway_route))
        .with_state(gateway)
}

const POSITION: &str = r#"{"durable_position":192000,"durable_exec_id":812,"delivered_position":190080,"verified":true,"halted":null,"consecutive_failures":0}"#;

/// The outbox's status endpoint, which answers in plain text apart from `/position`.
fn fake_outbox(position: Arc<Mutex<String>>) -> Router {
    Router::new()
        .route("/readyz", get(|| async { "ready" }))
        .route(
            "/position",
            get(move || {
                let position = position.clone();
                async move {
                    (
                        [("content-type", "application/json")],
                        position.lock().unwrap().clone(),
                    )
                }
            }),
        )
}

/// `/exec`, which answers rows per the column list that was asked for.
struct FakeQuest {
    queries: Mutex<Vec<String>>,
    orders: Mutex<Vec<Value>>,
    trades: Mutex<Vec<Value>>,
    missing_tables: bool,
}

fn order_row(time: &str, exec_id: i64) -> Value {
    json!([
        time,
        "ITEM",
        "new",
        "new",
        "buy",
        null,
        "00000000-0000-0000-0000-000000000001",
        "00000000-0000-0000-0000-00000000000a",
        5,
        exec_id,
        1_050_000_000i64,
        200_000_000,
        200_000_000,
        0,
        null
    ])
}

fn trade_row(time: &str, exec_id: i64) -> Value {
    json!([
        time,
        "ITEM",
        "buy",
        "00000000-0000-0000-0000-000000000001",
        "00000000-0000-0000-0000-00000000000a",
        77,
        5,
        exec_id,
        1_050_000_000i64,
        100_000_000
    ])
}

async fn quest_route(State(quest): State<Arc<FakeQuest>>, Query(params): Query<Value>) -> Response {
    let query = params["query"].as_str().unwrap_or("").to_string();
    quest.queries.lock().unwrap().push(query.clone());
    if query.contains("build()") {
        return Json(
            json!({ "dataset": [["Build Information: QuestDB 8.2.3, JDK 17, Commit Hash x"]] }),
        )
        .into_response();
    }
    if quest.missing_tables {
        return (StatusCode::BAD_REQUEST, Json(json!({ "query": query, "error": "table does not exist [table=orders]", "position": 0 }))).into_response();
    }
    let source = if query.contains("from orders") {
        &quest.orders
    } else {
        &quest.trades
    };
    if query.contains("sample by") || query.contains("group by") || query.contains("first(") {
        return Json(json!({ "dataset": [] })).into_response();
    }
    let rows = source.lock().unwrap().clone();
    let start = query
        .split("ts >= '")
        .nth(1)
        .and_then(|rest| rest.split('\'').next())
        .map(str::to_string);
    let mut rows: Vec<Value> = rows
        .into_iter()
        .filter(|row| {
            start
                .as_deref()
                .is_none_or(|start| row[0].as_str().unwrap() >= start)
        })
        .collect();
    if query.contains("select count()") {
        return Json(json!({ "dataset": [[rows.len()]] })).into_response();
    }
    if query.contains("desc") {
        rows.reverse();
    }
    if let Some(limit) = query
        .rsplit_once(" limit ")
        .and_then(|(_, limit)| limit.parse::<usize>().ok())
    {
        rows.truncate(limit);
    }
    Json(json!({ "dataset": rows })).into_response()
}

fn quest() -> Arc<FakeQuest> {
    Arc::new(FakeQuest {
        queries: Mutex::default(),
        orders: Mutex::new(vec![order_row("2026-01-01T00:00:00.000001Z", 1)]),
        trades: Mutex::new(vec![trade_row("2026-01-01T00:00:00.000002Z", 2)]),
        missing_tables: false,
    })
}

fn temp_markets(text: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mmx-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("markets.yaml");
    fs::write(&path, text).unwrap();
    path
}

struct World {
    url: String,
    http: reqwest::Client,
    file: PathBuf,
    quest: Arc<FakeQuest>,
    position: Arc<Mutex<String>>,
    up: Arc<Upstreams>,
    state: Arc<AppState>,
    dir: PathBuf,
}

/// Runs a real server, with a token, against the given gateways.
async fn world_with(file: PathBuf, gateways: Vec<String>, quest: Arc<FakeQuest>) -> World {
    let dir = file.with_file_name("state");
    let store = ConfigStore::Local(LocalStore {
        markets_file: file.clone(),
        dir: dir.clone(),
    });
    let mut world = world_on(store, gateways, quest).await;
    world.file = file;
    world.dir = dir;
    world
}

/// The same, on any store.
async fn world_on(store: ConfigStore, gateways: Vec<String>, quest: Arc<FakeQuest>) -> World {
    let position = Arc::new(Mutex::new(POSITION.to_string()));
    let outbox = spawn(fake_outbox(position.clone())).await;
    let questdb = spawn(
        Router::new()
            .route("/exec", get(quest_route))
            .with_state(quest.clone()),
    )
    .await;
    let nats = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nats_address = nats.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Ok((mut stream, _)) = nats.accept().await {
            let _ = stream.write_all(b"INFO {\"version\":\"2.10.0\"}\r\n").await;
        }
    });

    let targets = Targets {
        gateways,
        gateway_dns: None,
        outbox,
        questdb,
        nats: nats_address,
        valkey: None,
    };
    let up = Arc::new(Upstreams::new(targets, Some(TOKEN.to_string())).unwrap());
    let store = Arc::new(store);
    let password = Some(console::FIRST_PASSWORD.to_string());
    let auth = Auth::start(
        store.clone(),
        Some(TOKEN),
        false,
        password,
        Managed::default(),
    )
    .await
    .unwrap();
    let state = Arc::new(AppState {
        feeds: Feeds::new(up.clone()),
        streams: Arc::new(tokio::sync::Semaphore::new(server::MAX_STREAMS)),
        summary: tokio::sync::Mutex::new(None),
        up: up.clone(),
        store,
        auth,
        poll_every: Duration::from_millis(100),
        edits: tokio::sync::Mutex::new(()),
        behind_since: Mutex::new(None),
        outbox_restart: "restart-the-outbox".to_string(),
    });
    let url = spawn(server::router(state.clone())).await;
    World {
        url,
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        file: PathBuf::new(),
        dir: PathBuf::new(),
        state,
        quest,
        position,
        up,
    }
}

async fn world(gateways: &[Arc<FakeGateway>]) -> World {
    let file = gateways[0].file.clone();
    let mut urls = Vec::new();
    for gateway in gateways {
        urls.push(spawn(fake_gateway(gateway.clone())).await);
    }
    world_with(file, urls, quest()).await
}

impl World {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self
            .http
            .request(method, format!("{}/admin/v1{path}", self.url))
            .bearer_auth(TOKEN);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.call(Method::GET, path, None).await
    }

    async fn post(&self, path: &str) -> (u16, Value) {
        self.call(Method::POST, path, None).await
    }

    fn file_text(&self) -> String {
        fs::read_to_string(&self.file).unwrap()
    }
}

#[test]
fn the_command_line_is_well_formed() {
    use clap::CommandFactory;
    crate::Cli::command().debug_assert();
}

#[tokio::test]
async fn the_admin_api_needs_the_token_and_probes_do_not() {
    let world = world(&[FakeGateway::new(&temp_markets(FILE))]).await;
    let status = |token: Option<&'static str>, path: &'static str| {
        let mut request = world.http.get(format!("{}{path}", world.url));
        if let Some(token) = token {
            request = request.header("authorization", token);
        }
        async move { request.send().await.unwrap() }
    };

    let refused = status(None, "/admin/v1/webhooks").await;
    assert_eq!(refused.status(), 401);
    assert_eq!(refused.headers()["www-authenticate"], "Bearer");
    assert_eq!(
        refused.json::<Value>().await.unwrap()["code"],
        "unauthenticated"
    );

    assert_eq!(
        status(Some("Bearer wrong"), "/admin/v1/webhooks")
            .await
            .status(),
        401
    );
    assert_eq!(
        status(Some("Bearer test-toke"), "/admin/v1/webhooks")
            .await
            .status(),
        401
    );
    assert_eq!(
        status(Some("test-token"), "/admin/v1/webhooks")
            .await
            .status(),
        401
    );
    assert_eq!(
        status(Some("Bearer test-token"), "/admin/v1/webhooks")
            .await
            .status(),
        200
    );
    assert_eq!(status(None, "/admin/v1/events").await.status(), 401);

    assert_eq!(status(None, "/healthz").await.status(), 200);
    assert_eq!(status(None, "/readyz").await.status(), 200);
}

#[tokio::test]
async fn readiness_follows_the_markets_file() {
    let world = world(&[FakeGateway::new(&temp_markets(FILE))]).await;
    fs::write(&world.file, "markets: [").unwrap();
    let response = world
        .http
        .get(format!("{}/readyz", world.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({ "ready": false, "checks": { "markets_file": false } })
    );
}

#[tokio::test]
async fn status_reports_each_service_and_a_verdict() {
    let gateway = FakeGateway::new(&temp_markets(FILE));
    let good = world(std::slice::from_ref(&gateway)).await;
    let (code, body) = good.get("/status").await;
    assert_eq!(code, 200);
    assert_eq!(body["verdict"], "ok", "{body}");
    let services = body["services"].as_array().unwrap();
    let names: Vec<&str> = services
        .iter()
        .map(|service| service["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["gateway", "outbox", "questdb", "nats"]);
    assert_eq!(services[0]["checks"]["engine"], true);
    assert_eq!(services[2]["version"], "8.2.3");
    assert_eq!(services[3]["version"], "2.10.0");
    assert_eq!(
        gateway.calls_to("/readyz")[0].auth.as_deref(),
        Some("Bearer test-token")
    );

    // One gateway of two is down, so orders still have somewhere to go.
    let live = spawn(fake_gateway(gateway.clone())).await;
    let partial = world_with(gateway.file.clone(), vec![live, dead_url().await], quest()).await;
    let (_, body) = partial.get("/status").await;
    assert_eq!(body["verdict"], "degraded");
    assert_eq!(body["services"][1]["reachable"], false);
    assert!(body["services"][1]["error"].is_string());

    let none = world_with(gateway.file.clone(), vec![dead_url().await], quest()).await;
    assert_eq!(none.get("/status").await.1["verdict"], "down");
}

#[tokio::test]
async fn markets_combine_the_file_and_each_gateway() {
    let path = temp_markets(FILE);
    let first = FakeGateway::new(&path);
    let second = FakeGateway::new(&path);
    let world = world(&[first.clone(), second]).await;

    let (code, body) = world.get("/markets").await;
    assert_eq!(code, 200);
    let market = &body["markets"][0];
    assert_eq!(market["symbol"], "ITEM");
    assert_eq!(market["in_sync"], true, "{body}");
    assert_eq!(market["halted"], false);
    assert_eq!(market["gateways"].as_array().unwrap().len(), 2);

    // An edit by hand with no reload is exactly what `in_sync` is for.
    fs::write(
        &path,
        "markets:\n  - {symbol: ITEM, tick_size: \"0.05\"}\n  - {symbol: NEW, tick_size: \"1\"}\n",
    )
    .unwrap();
    let (_, body) = world.get("/markets").await;
    let symbols: Vec<&Value> = body["markets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|market| &market["symbol"])
        .collect();
    assert_eq!(symbols, [&json!("ITEM"), &json!("NEW")]);
    assert_eq!(body["markets"][0]["in_sync"], false);
    assert_eq!(body["markets"][1]["in_sync"], false);

    assert_eq!(world.get("/markets/nosuch").await.0, 404);
    assert_eq!(world.get("/markets/item").await.1["symbol"], "ITEM");
    assert_eq!(world.get("/markets/a-b").await.0, 400);
}

#[tokio::test]
async fn setting_a_market_writes_the_file_and_reloads_every_gateway() {
    let path = temp_markets(FILE);
    let (first, second) = (FakeGateway::new(&path), FakeGateway::new(&path));
    let world = world(&[first.clone(), second.clone()]).await;

    let market = json!({ "tick_size": "0.5", "min_size": "1", "max_size": "100" });
    let (code, body) = world.call(Method::PUT, "/markets/gem", Some(market)).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["applied"], true);
    assert_eq!(body["symbol"], "GEM");

    let file = MarketsFile::parse(&world.file_text()).unwrap();
    assert_eq!(file.get("GEM").unwrap().tick_size, "0.5");
    assert!(file.get("ITEM").is_some());
    assert!(world.file_text().starts_with("# Keep me\n"));

    for gateway in [&first, &second] {
        let calls = gateway.calls_to("/reload");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, "POST");
        assert_eq!(calls[0].auth.as_deref(), Some("Bearer test-token"));
    }
    assert_eq!(world.get("/markets/GEM").await.1["in_sync"], true);
}

#[tokio::test]
async fn a_bad_market_touches_nothing() {
    let gateway = FakeGateway::new(&temp_markets(FILE));
    let world = world(std::slice::from_ref(&gateway)).await;

    for body in [
        json!({ "tick_size": "0" }),
        json!({ "tick_size": "1.123456789" }),
        json!({ "tick_size": "1", "min_size": "5", "max_size": "1" }),
        json!({ "tick_size": "1", "halted": true }),
        json!({ "min_size": "1" }),
    ] {
        let (code, reply) = world
            .call(Method::PUT, "/markets/GEM", Some(body.clone()))
            .await;
        assert_eq!(code, 400, "{body}: {reply}");
    }
    assert_eq!(
        world
            .call(
                Method::PUT,
                "/markets/TOOLONG1",
                Some(json!({ "tick_size": "1" }))
            )
            .await
            .0,
        400
    );

    assert_eq!(world.file_text(), FILE);
    assert!(gateway.calls_to("/reload").is_empty());
}

#[tokio::test]
async fn a_failed_reload_is_reported_per_gateway() {
    let path = temp_markets(FILE);
    let (good, bad) = (FakeGateway::new(&path), FakeGateway::with(&path, true, 200));
    let world = world(&[good, bad]).await;

    let (code, body) = world
        .call(
            Method::PUT,
            "/markets/GEM",
            Some(json!({ "tick_size": "1" })),
        )
        .await;
    assert_eq!(code, 502, "{body}");
    assert_eq!(body["written"], true);
    assert_eq!(body["applied"], false);
    assert_eq!(body["gateways"][0]["ok"], true);
    assert_eq!(body["gateways"][1]["ok"], false);
    assert!(
        body["gateways"][1]["error"]
            .as_str()
            .unwrap()
            .contains("markets_not_reloaded"),
        "{body}"
    );
    assert!(world.file_text().contains("GEM"));
}

#[tokio::test]
async fn a_gateway_reading_another_file_does_not_count_as_applied() {
    // What a per-pod ConfigMap looks like: the reload succeeds and changes nothing.
    let (shared, private) = (temp_markets(FILE), temp_markets(FILE));
    let (good, stale) = (FakeGateway::new(&shared), FakeGateway::new(&private));
    let world = world(&[good, stale]).await;

    let (code, body) = world
        .call(
            Method::PUT,
            "/markets/GEM",
            Some(json!({ "tick_size": "1" })),
        )
        .await;
    assert_eq!(code, 502, "{body}");
    assert_eq!(body["applied"], false);
    assert_eq!(body["gateways"][0]["ok"], true);
    assert!(
        body["gateways"][1]["error"]
            .as_str()
            .unwrap()
            .contains("isn't reading the edited file"),
        "{body}"
    );
}

#[tokio::test]
async fn disabling_a_market_expects_the_reload_to_drop_it() {
    let gateway = FakeGateway::new(&temp_markets(FILE));
    let world = world(&[gateway]).await;
    let body = json!({ "tick_size": "0.01", "enabled": false });
    let (code, reply) = world.call(Method::PUT, "/markets/ITEM", Some(body)).await;
    assert_eq!(code, 200, "{reply}");
    assert!(world.file_text().contains("enabled: false"));
}

#[tokio::test]
async fn removing_a_market_edits_the_file_and_reloads() {
    let gateway = FakeGateway::new(&temp_markets(FILE));
    let world = world(std::slice::from_ref(&gateway)).await;

    assert_eq!(
        world.call(Method::DELETE, "/markets/GEM", None).await.0,
        404
    );
    assert!(gateway.calls_to("/reload").is_empty());

    let (code, body) = world.call(Method::DELETE, "/markets/item", None).await;
    assert_eq!(code, 200, "{body}");
    assert!(
        MarketsFile::parse(&world.file_text())
            .unwrap()
            .markets
            .is_empty()
    );
    assert_eq!(gateway.calls_to("/reload").len(), 1);
}

#[tokio::test]
async fn edits_at_the_same_time_all_land() {
    let gateway = FakeGateway::new(&temp_markets(FILE));
    let world = Arc::new(world(&[gateway]).await);
    let mut edits = Vec::new();
    for index in 0..10 {
        let world = world.clone();
        edits.push(tokio::spawn(async move {
            world
                .call(
                    Method::PUT,
                    &format!("/markets/M{index}"),
                    Some(json!({ "tick_size": "1" })),
                )
                .await
                .0
        }));
    }
    for edit in edits {
        assert_eq!(edit.await.unwrap(), 200);
    }
    assert_eq!(
        MarketsFile::parse(&world.file_text())
            .unwrap()
            .markets
            .len(),
        11
    );
}

#[tokio::test]
async fn halt_and_resume_reach_every_gateway() {
    let path = temp_markets(FILE);
    let (first, second) = (FakeGateway::new(&path), FakeGateway::new(&path));
    let world = world(&[first.clone(), second.clone()]).await;

    let (code, body) = world.post("/markets/item/halt").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["gateways"][0]["body"]["halted"], true);
    for gateway in [&first, &second] {
        let call = &gateway.calls_to("/halt")[0];
        assert_eq!(call.path, "/api/v1/admin/markets/ITEM/halt");
        assert_eq!(call.auth.as_deref(), Some("Bearer test-token"));
    }
    assert_eq!(world.get("/markets/ITEM").await.1["halted"], true);

    assert_eq!(world.post("/markets/ITEM/resume").await.0, 200);
    assert_eq!(world.get("/markets/ITEM").await.1["halted"], false);
}

#[tokio::test]
async fn a_partial_halt_fails_and_says_where() {
    let path = temp_markets(FILE);
    let gateway = FakeGateway::new(&path);
    let live = spawn(fake_gateway(gateway.clone())).await;
    let world = world_with(path, vec![live, dead_url().await], quest()).await;

    let (code, body) = world.post("/markets/ITEM/halt").await;
    assert_eq!(code, 502, "{body}");
    assert_eq!(body["ok"], false);
    assert_eq!(body["gateways"][0]["ok"], true);
    assert_eq!(body["gateways"][1]["ok"], false);

    // The one that did halt still shows, so this can't read as running.
    let (_, market) = world.get("/markets/ITEM").await;
    assert_eq!(market["halted"], true);
    assert_eq!(market["in_sync"], false);
}

#[tokio::test]
async fn an_unknown_market_on_every_gateway_stays_a_404() {
    let world = world(&[FakeGateway::new(&temp_markets(FILE))]).await;
    let (code, body) = world.post("/markets/NOPE/halt").await;
    assert_eq!(code, 404, "{body}");
    assert_eq!(body["ok"], false);
}

#[tokio::test]
async fn cancel_all_asks_one_gateway_so_nothing_is_counted_twice() {
    let path = temp_markets(FILE);
    let (first, second) = (FakeGateway::new(&path), FakeGateway::new(&path));
    let world = world(&[first.clone(), second.clone()]).await;

    let (code, body) = world.post("/markets/item/cancel-all").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        (&body["requested"], &body["canceled"], &body["failed"]),
        (&json!(3), &json!(2), &json!(1))
    );
    assert_eq!(
        body["complete"], false,
        "a failed cancel leaves an order open"
    );
    assert_eq!(first.calls_to("/cancel-all").len(), 1);
    assert!(second.calls_to("/cancel-all").is_empty());
}

#[tokio::test]
async fn cancel_all_moves_on_when_a_gateway_cannot_answer() {
    let path = temp_markets(FILE);
    let (broken, healthy) = (
        FakeGateway::with(&path, false, 503),
        FakeGateway::new(&path),
    );
    let urls = vec![
        dead_url().await,
        spawn(fake_gateway(broken.clone())).await,
        spawn(fake_gateway(healthy.clone())).await,
    ];
    let world = world_with(path, urls, quest()).await;

    let (code, body) = world.post("/markets/ITEM/cancel-all").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["canceled"], 2);
    assert_eq!(body["gateways"].as_array().unwrap().len(), 3);
    assert_eq!(healthy.calls_to("/cancel-all").len(), 1);
}

#[tokio::test]
async fn cancel_all_with_no_gateway_is_a_failure() {
    let path = temp_markets(FILE);
    let world = world_with(path, vec![dead_url().await], quest()).await;
    let (code, body) = world.post("/markets/ITEM/cancel-all").await;
    assert_eq!(code, 502, "{body}");
    assert_eq!(body["complete"], false);
}

#[tokio::test]
async fn orders_and_trades_read_questdb_with_filters() {
    let world = world(&[FakeGateway::new(&temp_markets(FILE))]).await;

    let (code, body) = world.get("/orders").await;
    assert_eq!(code, 200, "{body}");
    let order = &body["orders"][0];
    assert_eq!(order["symbol"], "ITEM");
    assert_eq!(order["price"], "10.5");
    assert_eq!(order["size"], "2");
    assert_eq!(order["filled_size"], "0");
    assert_eq!(order["player"], "00000000-0000-0000-0000-000000000001");
    assert!(order.get("avg_price").is_none());

    let (_, body) = world.get("/trades?limit=1").await;
    assert_eq!(body["trades"][0]["size"], "1");
    assert_eq!(body["trades"][0]["match_id"], 77);

    let player = "00000000-0000-0000-0000-000000000001";
    assert_eq!(
        world
            .get(&format!("/orders?symbol=item&player={player}&limit=5"))
            .await
            .0,
        200
    );
    let query = world.quest.queries.lock().unwrap().last().unwrap().clone();
    assert!(
        query.ends_with(&format!(
            "where symbol = 'ITEM' and account = '{player}' order by ts desc limit 5"
        )),
        "{query}"
    );
}

#[tokio::test]
async fn filters_cannot_reach_sql_unchecked() {
    let world = world(&[FakeGateway::new(&temp_markets(FILE))]).await;
    let before = world.quest.queries.lock().unwrap().len();
    for path in [
        "/orders?symbol=x'%20or%201=1",
        "/orders?player=x'%20or%201=1",
        "/orders?limit=0",
        "/orders?limit=1001",
        "/orders?limit=1;drop",
        "/orders?side=x'",
        "/orders?event=x'",
        "/orders?order=x'",
        "/orders?before=x'",
        "/events?side=sideways",
        "/trades?surprise=1",
    ] {
        assert_eq!(world.get(path).await.0, 400, "{path}");
    }
    assert_eq!(world.quest.queries.lock().unwrap().len(), before);
}

#[tokio::test]
async fn activity_before_the_first_write_is_empty_not_an_error() {
    let path = temp_markets(FILE);
    let quest = Arc::new(FakeQuest {
        missing_tables: true,
        ..Arc::into_inner(quest()).unwrap()
    });
    let world = world_with(path, Vec::new(), quest).await;
    assert_eq!(world.get("/orders").await, (200, json!({ "orders": [] })));
}

#[tokio::test]
async fn webhooks_show_the_outbox_position() {
    let world = world(&[FakeGateway::new(&temp_markets(FILE))]).await;
    let (code, body) = world.get("/webhooks").await;
    assert_eq!(code, 200);
    assert_eq!(body["state"], "delivering");
    assert_eq!(body["backlog"], 1920);
    assert_eq!(body["durable_exec_id"], 812);

    *world.position.lock().unwrap() = r#"{"durable_position":10,"durable_exec_id":1,"delivered_position":5,"verified":true,"halted":"the tape differs","consecutive_failures":2}"#.to_string();
    let (_, body) = world.get("/webhooks").await;
    assert_eq!(body["state"], "halted");
    assert_eq!(body["halted"], "the tape differs");
}

#[tokio::test]
async fn webhooks_fail_when_the_outbox_makes_no_sense() {
    let world = world(&[FakeGateway::new(&temp_markets(FILE))]).await;
    *world.position.lock().unwrap() = "not json".to_string();
    let (code, body) = world.get("/webhooks").await;
    assert_eq!(code, 502, "{body}");
    assert_eq!(body["code"], "upstream_failed");
}

fn kinds(activity: &[Activity]) -> Vec<&'static str> {
    activity.iter().map(|item| item.kind).collect()
}

#[tokio::test]
async fn polling_reports_the_current_state_then_only_changes() {
    let gateway = FakeGateway::new(&temp_markets(FILE));
    let world = world(std::slice::from_ref(&gateway)).await;
    let mut poller = Poller::new(
        world.up.clone(),
        Filter::default(),
        false,
        Duration::from_secs(1),
    );

    let first = poller.poll().await;
    for kind in ["service", "market", "webhook", "order", "trade"] {
        assert!(
            kinds(&first).contains(&kind),
            "{kind} missing from {first:?}"
        );
    }
    let order = first.iter().position(|item| item.kind == "order").unwrap();
    let trade = first.iter().position(|item| item.kind == "trade").unwrap();
    assert!(order < trade);

    assert_eq!(poller.poll().await, Vec::new(), "nothing changed");

    world
        .quest
        .orders
        .lock()
        .unwrap()
        .push(order_row("2026-01-01T00:00:01.000000Z", 3));
    world
        .quest
        .trades
        .lock()
        .unwrap()
        .push(trade_row("2026-01-01T00:00:01.000000Z", 4));
    gateway.halted.lock().unwrap().insert("ITEM".to_string());
    *world.position.lock().unwrap() = POSITION.replace("190080", "192000");

    let changes = poller.poll().await;
    assert_eq!(
        kinds(&changes),
        ["market", "webhook", "order", "sample", "trade", "sample"],
        "{changes:?}"
    );
    assert_eq!(changes[0].data["market"]["halted"], true);
    assert_eq!(changes[1].data["state"], "idle");
    assert_eq!(changes[2].data["exec_id"], 3);
    assert_eq!(
        changes[3].data,
        json!({ "table": "orders", "seen": 1, "sent": 1 })
    );

    // The rate falling to zero is news once, and silence after that.
    assert_eq!(kinds(&poller.poll().await), ["sample", "sample"]);
    assert_eq!(poller.poll().await, Vec::new());
}

fn poller(world: &World, filter: Filter) -> Poller {
    Poller::new(world.up.clone(), filter, true, Duration::from_secs(1))
}

#[tokio::test]
async fn the_tape_is_sampled_at_a_fixed_rate_however_fast_it_arrives() {
    let world = world(&[FakeGateway::new(&temp_markets(FILE))]).await;
    let mut poller = poller(&world, Filter::default());
    poller.poll().await;

    // Fifty thousand reports in a second, against a cap of a hundred.
    let burst: Vec<Value> = (10..50_010)
        .map(|id| order_row(&format!("2026-01-01T00:00:01.{id:06}Z"), id))
        .collect();
    world.quest.orders.lock().unwrap().extend(burst);
    let changes = poller.poll().await;

    let orders: Vec<&Activity> = changes.iter().filter(|item| item.kind == "order").collect();
    assert_eq!(orders.len(), crate::events::RATE_CAP);
    assert_eq!(orders.last().unwrap().data["exec_id"], 50_009, "the newest");
    let sample = changes.iter().find(|item| item.kind == "sample").unwrap();
    assert_eq!(sample.data["sent"], crate::events::RATE_CAP);
    assert_eq!(sample.data["seen"], 50_000);

    // It skipped ahead, so the next poll isn't behind.
    world
        .quest
        .orders
        .lock()
        .unwrap()
        .push(order_row("2026-01-01T00:00:02.000000Z", 60_000));
    let next = poller.poll().await;
    let sample = next.iter().find(|item| item.kind == "sample").unwrap();
    assert_eq!(
        sample.data,
        json!({ "table": "orders", "seen": 1, "sent": 1 })
    );
}

#[tokio::test]
async fn a_tape_stream_reads_nothing_but_the_tape() {
    let world = world(&[FakeGateway::new(&temp_markets(FILE))]).await;
    let first = poller(&world, Filter::default()).poll().await;
    assert_eq!(kinds(&first), ["order", "trade"], "{first:?}");
}

#[tokio::test]
async fn filters_reach_every_read_of_the_tape() {
    let world = world(&[FakeGateway::new(&temp_markets(FILE))]).await;
    let player = "00000000-0000-0000-0000-000000000001";
    let id = "00000000-0000-0000-0000-00000000000a";
    let path = format!(
        "/orders?symbol=item&player={player}&side=buy&event=trade&order={id}\
         &before=2026-01-01T00:00:00Z&limit=5"
    );
    assert_eq!(world.get(&path).await.0, 200);
    let query = world.quest.queries.lock().unwrap().last().unwrap().clone();
    assert!(
        query.ends_with(&format!(
            "where symbol = 'ITEM' and account = '{player}' and side = 'buy' \
             and exec_type = 'trade' and cl_ord_id = '{id}' \
             and ts < '2026-01-01T00:00:00Z' order by ts desc limit 5"
        )),
        "{query}"
    );
    assert_eq!(world.get("/orders?order=42").await.0, 200);
    let query = world.quest.queries.lock().unwrap().last().unwrap().clone();
    assert!(query.contains("order_id = 42"), "{query}");

    // Trades have one event, so asking for another finds nothing without a query.
    let before = world.quest.queries.lock().unwrap().len();
    let (code, body) = world.get("/trades?event=canceled").await;
    assert_eq!(code, 200);
    assert_eq!(body["trades"], json!([]));
    assert_eq!(world.quest.queries.lock().unwrap().len(), before);
}

#[tokio::test]
async fn polling_reports_a_failure_once_and_its_recovery() {
    let gateway = FakeGateway::new(&temp_markets(FILE));
    let world = world(&[gateway]).await;
    let mut poller = Poller::new(
        world.up.clone(),
        Filter::default(),
        false,
        Duration::from_secs(1),
    );
    poller.poll().await;

    *world.position.lock().unwrap() = "not json".to_string();
    let failed = poller.poll().await;
    assert_eq!(kinds(&failed), ["source_failed"], "{failed:?}");
    assert_eq!(failed[0].data["source"], "outbox");
    assert_eq!(poller.poll().await, Vec::new());

    *world.position.lock().unwrap() = POSITION.to_string();
    assert_eq!(kinds(&poller.poll().await), ["source_recovered"]);
}

#[tokio::test]
async fn polling_notices_a_market_leaving() {
    let path = temp_markets(FILE);
    let gateway = FakeGateway::new(&path);
    let world = world(std::slice::from_ref(&gateway)).await;
    let mut poller = Poller::new(
        world.up.clone(),
        Filter::default(),
        false,
        Duration::from_secs(1),
    );
    poller.poll().await;

    fs::write(&path, "markets: []\n").unwrap();
    gateway.load();
    let changes = poller.poll().await;
    assert_eq!(kinds(&changes), ["market_removed"]);
    assert_eq!(changes[0].data["symbol"], "ITEM");
}

#[tokio::test]
async fn events_stream_over_http() {
    let world = world(&[FakeGateway::new(&temp_markets(FILE))]).await;
    let mut response = world
        .http
        .get(format!("{}/admin/v1/events", world.url))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");

    let mut buffer = Vec::new();
    let mut seen = HashSet::new();
    let read = async {
        while let Ok(Some(chunk)) = response.chunk().await {
            buffer.extend_from_slice(&chunk);
            seen.extend(
                crate::watch::take_events(&mut buffer)
                    .into_iter()
                    .map(|(kind, _)| kind),
            );
            if seen.contains("order") && seen.contains("trade") && seen.contains("market") {
                return;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("the first poll should arrive");
}

/// Method, target, player header and body.
type OrderRequest = (String, String, Option<String>, Vec<u8>);

/// A gateway that records what a game server's requests look like.
#[derive(Default)]
struct OrderLog {
    requests: Mutex<Vec<OrderRequest>>,
}

async fn order_route(
    State(log): State<Arc<OrderLog>>,
    method: Method,
    uri: Uri,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let player = headers
        .get("x-player-id")
        .map(|value| value.to_str().unwrap().to_string());
    log.requests
        .lock()
        .unwrap()
        .push((method.to_string(), uri.to_string(), player, body.to_vec()));
    if method == Method::POST {
        let id = serde_json::from_slice::<Value>(&body).unwrap()["client_order_id"].clone();
        if id == "00000000-0000-0000-0000-000000000bad" {
            let body = json!({ "code": "outcome_unknown", "message": "The order's outcome isn't known yet." });
            return (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response();
        }
        if id == "00000000-0000-0000-0000-000000000ccc" {
            let body = json!({ "code": "claim_unavailable", "message": "New orders are refused." });
            return (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response();
        }
        let order = json!({ "id": id, "product_id": "ITEM", "side": "buy", "type": "limit", "size": "1", "price": "10", "filled_size": "0", "status": "open" });
        return (StatusCode::CREATED, Json(order)).into_response();
    }
    if method == Method::DELETE {
        return Json(json!({ "id": "x", "status": "canceled" })).into_response();
    }
    Json(json!([])).into_response()
}

#[tokio::test]
async fn orders_go_to_the_gateway_as_a_game_server_sends_them() {
    use crate::client::{NewOrder, Orders};
    use uuid::Uuid;

    let log = Arc::new(OrderLog::default());
    let url = spawn(Router::new().fallback(order_route).with_state(log.clone())).await;
    let player = Uuid::from_u128(1);
    let orders = Orders::new(url, player, true).unwrap();

    let order = |id: Uuid| NewOrder {
        client_order_id: id,
        product_id: "ITEM".to_string(),
        side: "buy".to_string(),
        r#type: "limit".to_string(),
        size: "1".to_string(),
        price: Some("10".to_string()),
    };
    orders.place(&order(Uuid::from_u128(2))).await.unwrap();

    let bad = Uuid::parse_str("00000000-0000-0000-0000-000000000bad").unwrap();
    let error = orders.place(&order(bad)).await.unwrap_err();
    assert!(
        error.contains("Don't send it again") && error.contains(&bad.to_string()),
        "{error}"
    );

    // Nothing was sent, so this order isn't in doubt.
    let refused = Uuid::parse_str("00000000-0000-0000-0000-000000000ccc").unwrap();
    let error = orders.place(&order(refused)).await.unwrap_err();
    assert!(error.contains("New orders are refused"), "{error}");
    assert!(!error.contains("unknown"), "{error}");

    orders.cancel(Uuid::from_u128(2)).await.unwrap();
    orders
        .list(Some("ITEM"), Some("open"), Some(5))
        .await
        .unwrap();

    let requests = log.requests.lock().unwrap();
    assert_eq!(requests.len(), 5, "an unknown outcome must not be retried");
    let (method, uri, sent_player, body) = &requests[0];
    assert_eq!((method.as_str(), uri.as_str()), ("POST", "/api/v1/orders"));
    assert_eq!(sent_player.as_deref(), Some(player.to_string().as_str()));
    let body: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(
        body,
        json!({
            "client_order_id": Uuid::from_u128(2),
            "product_id": "ITEM",
            "side": "buy",
            "type": "limit",
            "size": "1",
            "price": "10",
        })
    );
    assert_eq!(
        requests[3].1,
        format!("/api/v1/orders/{}", Uuid::from_u128(2))
    );
    assert_eq!(
        requests[4].1,
        "/api/v1/orders?product_id=ITEM&status=open&limit=5"
    );
}

#[tokio::test]
async fn the_listener_takes_a_signed_delivery_over_http() {
    use crate::webhook::{Listener, router};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::time::{SystemTime, UNIX_EPOCH};

    let listener = Arc::new(Listener::new(b"secret".to_vec(), true));
    let url = spawn(router(listener.clone())).await;
    let http = reqwest::Client::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let signed = |body: &str, timestamp: &str| {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(timestamp.as_bytes());
        mac.update(b".");
        mac.update(body.as_bytes());
        let digest: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("v1={digest}")
    };
    let send = |body: String, timestamp: String, signature: String| {
        let request = http
            .post(format!("{url}/any/path"))
            .header("X-MMX-Timestamp", timestamp)
            .header("X-MMX-Signature", signature)
            .body(body);
        async move { request.send().await.unwrap().status().as_u16() }
    };

    let body = r#"{"events":[{"fill_id":"a","type":"fill","terminal":true,"release_qty":"0"}]}"#
        .to_string();
    assert_eq!(
        send(body.clone(), now.clone(), signed(&body, &now)).await,
        200
    );
    assert_eq!(
        send(body.clone(), now.clone(), signed(&body, &now)).await,
        200,
        "a repeat is still accepted"
    );
    assert_eq!(
        send(body.clone(), now.clone(), "v1=00".to_string()).await,
        401
    );
    assert_eq!(
        send(body.clone(), "1000".to_string(), signed(&body, "1000")).await,
        401
    );
    let junk = "not json".to_string();
    assert_eq!(
        send(junk.clone(), now.clone(), signed(&junk, &now)).await,
        400
    );
    assert_eq!(http.get(&url).send().await.unwrap().status(), 405);

    let tally = listener.tally.lock().unwrap();
    assert_eq!((tally.events, tally.duplicates, tally.refused), (1, 1, 3));
}
