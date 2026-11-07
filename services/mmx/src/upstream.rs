//! HTTP and TCP calls to the services `mmx serve` watches: gateways, the outbox, QuestDB, NATS and
//! Valkey.

use crate::decimal;
use crate::monitor::MarketStats;

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::time::Duration;

use futures_util::future::join_all;
use reqwest::{Client, Method, RequestBuilder, redirect};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, lookup_host};
use tokio::time::timeout;
use tracing::warn;
use uuid::Uuid;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// A gateway cancels up to 10,000 orders one by one, and holds requests across a failover.
pub const CANCEL_ALL_TIMEOUT: Duration = Duration::from_secs(120);
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// A busy market has a long history to read when someone starts watching it.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(20);

/// Most book levels a market can have and still be read in one go.
const SNAPSHOT_LEVELS: usize = 100_000;
const NATS_PORT: u16 = 4222;
const VALKEY_PORT: u16 = 6379;
const WEBSOCKET_PORT: u16 = 8090;
const OPEN_ORDERS_TIMEOUT: Duration = Duration::from_secs(3);
/// Enough rows for every market at every order state, at the most markets anyone has tried.
const SUMMARY_ROWS: usize = 30_000;
const BUSIEST: usize = 12;

/// reqwest panics on the first client without this. A workspace build can link two TLS backends, so
/// the choice has to be explicit, and installing twice changes nothing.
pub fn install_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Redirects are off so a bearer token can't follow one anywhere, and proxies are off because these
/// are all internal addresses.
pub fn http_client(request_timeout: Option<Duration>) -> Result<Client, String> {
    install_tls();
    let builder = Client::builder()
        .no_proxy()
        .redirect(redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT);
    let builder = match request_timeout {
        Some(limit) => builder.timeout(limit),
        None => builder,
    };
    builder.build().map_err(|err| err.to_string())
}

/// reqwest's own message leaves out the cause, which is the part an operator needs.
pub fn describe(err: &reqwest::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text = format!("{text}: {cause}");
        source = cause.source();
    }
    text
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// One line for an error reply, from the `code`, `message` and `details` every service here uses.
    pub fn error(&self) -> String {
        let field = |name: &str| self.body.get(name).and_then(Value::as_str);
        let mut text = format!("HTTP {}", self.status);
        for part in [
            field("code"),
            field("message"),
            field("details"),
            self.body.as_str(),
        ]
        .into_iter()
        .flatten()
        {
            if !part.is_empty() {
                text = format!("{text}: {part}");
            }
        }
        text
    }
}

pub async fn send(request: RequestBuilder) -> Result<Reply, String> {
    let response = request.send().await.map_err(|err| describe(&err))?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|err| describe(&err))?;
    let body = match serde_json::from_str(&text) {
        Ok(body) => body,
        Err(_) if text.is_empty() => Value::Null,
        Err(_) => Value::String(text),
    };
    Ok(Reply { status, body })
}

/// What one gateway said, so a partial failure is visible instead of being summed away.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct GatewayResult {
    pub gateway: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub body: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl GatewayResult {
    fn new(gateway: &str, reply: Result<Reply, String>) -> Self {
        match reply {
            Ok(reply) => Self {
                gateway: gateway.to_string(),
                ok: reply.ok(),
                status: Some(reply.status),
                error: (!reply.ok()).then(|| reply.error()),
                body: reply.body,
            },
            Err(error) => Self {
                gateway: gateway.to_string(),
                ok: false,
                status: None,
                body: Value::Null,
                error: Some(error),
            },
        }
    }

    pub fn fail(&mut self, error: String) {
        self.ok = false;
        self.error = Some(error);
    }
}

/// `enabled` and `halted` are booleans in JSON, but the contract says "values as strings", so both
/// spellings are accepted.
fn flag<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    match Value::deserialize(deserializer)? {
        Value::Bool(value) => Ok(value),
        Value::String(text) if text == "true" => Ok(true),
        Value::String(text) if text == "false" => Ok(false),
        other => Err(serde::de::Error::custom(format!(
            "expected a boolean, got {other}"
        ))),
    }
}

/// A market as one gateway holds it right now.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LiveMarket {
    pub symbol: String,
    pub tick_size: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<String>,
    #[serde(deserialize_with = "flag")]
    pub enabled: bool,
    #[serde(deserialize_with = "flag")]
    pub halted: bool,
}

#[derive(Deserialize)]
struct MarketList {
    markets: Vec<LiveMarket>,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ServiceStatus {
    pub name: String,
    pub target: String,
    /// Whether the exchange can't run properly without it, as opposed to running with less.
    pub required: bool,
    pub reachable: bool,
    pub ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When its work was last seen, for services nothing can ask directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
}

impl ServiceStatus {
    pub fn new(name: &str, target: &str, required: bool) -> Self {
        Self {
            name: name.to_string(),
            target: target.to_string(),
            required,
            reachable: false,
            ready: false,
            version: None,
            checks: None,
            error: None,
            last_seen: None,
        }
    }

    fn unreachable(mut self, error: String) -> Self {
        self.error = Some(error);
        self
    }

    /// The boolean checks this service said no to. The outbox's `checks` hold other things.
    pub fn failed_checks(&self) -> Vec<String> {
        if self.name != "gateway" {
            return Vec::new();
        }
        self.checks
            .as_ref()
            .and_then(Value::as_object)
            .map_or_else(Vec::new, |checks| {
                checks
                    .iter()
                    .filter(|(_, passed)| passed.as_bool() == Some(false))
                    .map(|(name, _)| name.clone())
                    .collect()
            })
    }

    pub fn checks_failed(&self) -> bool {
        !self.failed_checks().is_empty()
    }
}

/// `ok` needs everything, and `down` means no gateway can take an order.
pub fn install_verdict(services: &[ServiceStatus]) -> &'static str {
    let can_trade = services
        .iter()
        .any(|service| service.name == "gateway" && service.ready);
    if !can_trade {
        "down"
    } else if services.iter().all(|service| service.ready) {
        "ok"
    } else {
        "degraded"
    }
}

/// `/api/v1/admin/cluster` on a gateway.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GatewayMember {
    pub member: usize,
    pub connected: bool,
    pub position: Option<i64>,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GatewayCluster {
    pub term: i64,
    pub leader: Option<usize>,
    pub leader_connected: bool,
    pub leader_changes: u64,
    pub last_leader_change_ms: Option<u64>,
    pub last_exec_id: u64,
    pub members: Vec<GatewayMember>,
}

/// What one gateway said of the cluster. An older gateway, or one that's down, says nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterAnswer {
    pub standalone: bool,
    pub view: Option<GatewayCluster>,
}

/// The newest row QuestDB holds, which is how far `trade-reporter` has got.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TapeHead {
    pub exec_id: Option<u64>,
    pub time: Option<String>,
    pub newest_trade_ms: Option<u64>,
}

/// Days since 1970 for a civil date, as in Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let shifted = (month + 9) % 12;
    let doy = (153 * shifted + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Milliseconds for a QuestDB timestamp like `2026-11-06T16:38:53.463813Z`.
pub fn parse_time_ms(text: &str) -> Option<u64> {
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let days = days_from_civil(number(0..4)?, number(5..7)?, number(8..10)?);
    let seconds = number(11..13)? * 3600 + number(14..16)? * 60 + number(17..19)?;
    let fraction = text.get(20..23).and_then(|part| part.parse::<i64>().ok());
    u64::try_from((days * 86_400 + seconds) * 1000 + fraction.unwrap_or(0)).ok()
}

/// `GET /position` on the outbox.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub durable_position: i64,
    pub durable_exec_id: u64,
    pub delivered_position: i64,
    pub verified: bool,
    pub halted: Option<String>,
    pub consecutive_failures: u64,
    /// The URL and a fingerprint of the settings it started with, and whether it has any. These
    /// are `None` from an outbox too old to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured: Option<bool>,
}

impl Position {
    /// The outbox reports positions and failures, with no record of single deliveries, so this is
    /// as much as it can say.
    pub fn view(&self) -> Value {
        let backlog = (self.durable_position - self.delivered_position).max(0);
        let state = if self.configured == Some(false) {
            "not_configured"
        } else if self.halted.is_some() {
            "halted"
        } else if !self.verified {
            "verifying"
        } else if self.consecutive_failures > 0 {
            "failing"
        } else if backlog > 0 {
            "delivering"
        } else {
            "idle"
        };
        json!({
            "state": state,
            "durable_position": self.durable_position,
            "durable_exec_id": self.durable_exec_id,
            "delivered_position": self.delivered_position,
            "backlog": backlog,
            "verified": self.verified,
            "halted": self.halted,
            "consecutive_failures": self.consecutive_failures,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plain,
    /// A `LONG` mantissa, shown as a decimal string.
    Decimal,
}

/// A QuestDB table and how its columns are shown. The names shown match the gateway's and the
/// webhook's, and `account` becomes `player`.
pub struct Table {
    name: &'static str,
    columns: &'static [(&'static str, &'static str, Kind)],
}

pub const ORDERS: Table = Table {
    name: "orders",
    columns: &[
        ("ts", "time", Kind::Plain),
        ("symbol", "symbol", Kind::Plain),
        ("exec_type", "exec_type", Kind::Plain),
        ("ord_status", "status", Kind::Plain),
        ("side", "side", Kind::Plain),
        ("ord_rej_reason", "reason", Kind::Plain),
        ("account", "player", Kind::Plain),
        ("cl_ord_id", "client_order_id", Kind::Plain),
        ("order_id", "order_id", Kind::Plain),
        ("exec_id", "exec_id", Kind::Plain),
        ("price", "price", Kind::Decimal),
        ("ord_qty", "size", Kind::Decimal),
        ("leaves_qty", "leaves_size", Kind::Decimal),
        ("cum_qty", "filled_size", Kind::Decimal),
        ("avg_px", "avg_price", Kind::Decimal),
    ],
};

/// One row per side of a match. Both sides share `match_id`.
pub const TRADES: Table = Table {
    name: "trades",
    columns: &[
        ("ts", "time", Kind::Plain),
        ("symbol", "symbol", Kind::Plain),
        ("side", "side", Kind::Plain),
        ("account", "player", Kind::Plain),
        ("cl_ord_id", "client_order_id", Kind::Plain),
        ("match_id", "match_id", Kind::Plain),
        ("order_id", "order_id", Kind::Plain),
        ("exec_id", "exec_id", Kind::Plain),
        ("price", "price", Kind::Decimal),
        ("quantity", "size", Kind::Decimal),
    ],
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "buy" => Ok(Self::Buy),
            "sell" => Ok(Self::Sell),
            other => Err(format!("side must be buy or sell, not {other}.")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }
}

/// The execution types on the tape. Taken from this list, so a value can't reach SQL unchecked.
pub fn event_name(text: &str) -> Result<&'static str, String> {
    const EVENTS: [&str; 5] = ["new", "trade", "canceled", "rejected", "expired"];
    EVENTS
        .into_iter()
        .find(|event| *event == text)
        .ok_or_else(|| format!("Unknown event {text}."))
}

/// How an operator names an order: the game's ID, or the engine's number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKey {
    Client(Uuid),
    Engine(i64),
}

impl OrderKey {
    pub fn parse(text: &str) -> Result<Self, String> {
        if let Ok(id) = Uuid::parse_str(text) {
            return Ok(Self::Client(id));
        }
        text.parse()
            .map(Self::Engine)
            .map_err(|_| "order must be a UUID or an order number.".to_string())
    }
}

/// Filters reach SQL as parsed values, so none can contain a quote.
#[derive(Default, Debug, Clone)]
pub struct Filter {
    pub symbol: Option<String>,
    pub player: Option<Uuid>,
    pub side: Option<Side>,
    pub event: Option<&'static str>,
    pub order: Option<OrderKey>,
    /// Rows older than this `ts`, for paging back.
    pub before: Option<String>,
}

/// A window of the chart, and how finely it's cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Minutes15,
    Hour,
    Hours6,
    Day,
}

impl Range {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "15m" => Some(Self::Minutes15),
            "1h" => Some(Self::Hour),
            "6h" => Some(Self::Hours6),
            "24h" => Some(Self::Day),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Minutes15 => "15m",
            Self::Hour => "1h",
            Self::Hours6 => "6h",
            Self::Day => "24h",
        }
    }

    fn minutes(self) -> u32 {
        match self {
            Self::Minutes15 => 15,
            Self::Hour => 60,
            Self::Hours6 => 360,
            Self::Day => 1440,
        }
    }

    fn summary_bucket_secs(self) -> u32 {
        match self {
            Self::Minutes15 => 10,
            Self::Hour => 60,
            Self::Hours6 => 300,
            Self::Day => 900,
        }
    }

    pub fn bucket_secs(self) -> u32 {
        match self {
            Self::Minutes15 => 10,
            Self::Hour => 30,
            Self::Hours6 => 180,
            Self::Day => 600,
        }
    }
}

/// Which rows of a table a filter can match. Trades have no event column, and every trade row is
/// an execution of type `trade`.
fn where_clause(table: &Table, filter: &Filter, since: Option<&str>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(symbol) = &filter.symbol {
        parts.push(format!("symbol = '{symbol}'"));
    }
    if let Some(player) = filter.player {
        parts.push(format!("account = '{player}'"));
    }
    if let Some(side) = filter.side {
        parts.push(format!("side = '{}'", side.name()));
    }
    match (filter.event, table.name) {
        (Some("trade"), "trades") | (None, _) => {}
        (Some(event), "orders") => parts.push(format!("exec_type = '{event}'")),
        (Some(_), _) => return None,
    }
    match filter.order {
        Some(OrderKey::Client(id)) => parts.push(format!("cl_ord_id = '{id}'")),
        Some(OrderKey::Engine(id)) => parts.push(format!("order_id = {id}")),
        None => {}
    }
    if let Some(time) = &filter.before {
        parts.push(format!("ts < '{time}'"));
    }
    if let Some(time) = since {
        parts.push(format!("ts >= '{time}'"));
    }
    Some(if parts.is_empty() {
        String::new()
    } else {
        format!(" where {}", parts.join(" and "))
    })
}

impl Table {
    fn column_names(&self) -> String {
        let names: Vec<&str> = self.columns.iter().map(|(column, _, _)| *column).collect();
        names.join(", ")
    }

    /// The newest rows, one each. Reading by time stays cheap however long the table gets, unlike
    /// rolling every order up to its latest state, which has to scan it. With `since`, only rows
    /// from then on, and that's how the live view reads: the newest few, however many there were.
    fn newest_sql(&self, filter: &Filter, since: Option<&str>, limit: usize) -> Option<String> {
        let filters = where_clause(self, filter, since)?;
        Some(format!(
            "select {} from {}{filters} order by ts desc limit {limit}",
            self.column_names(),
            self.name
        ))
    }

    fn count_sql(&self, filter: &Filter, since: &str) -> Option<String> {
        let filters = where_clause(self, filter, Some(since))?;
        Some(format!("select count() from {}{filters}", self.name))
    }

    fn row(&self, values: &[Value]) -> Map<String, Value> {
        let mut row = Map::new();
        for ((_, field, kind), value) in self.columns.iter().zip(values) {
            match (kind, value) {
                (_, Value::Null) => {}
                (Kind::Decimal, value) => {
                    if let Some(mantissa) = value.as_i64() {
                        row.insert(field.to_string(), Value::String(decimal::format(mantissa)));
                    }
                }
                (Kind::Plain, value) => {
                    row.insert(field.to_string(), value.clone());
                }
            }
        }
        row
    }
}

/// How a stretch of the tape breaks down. `orders` counts orders arriving, not their later steps.
#[derive(Serialize, Debug, Clone, Default, PartialEq, Eq)]
struct Tally {
    orders: u64,
    trades: u64,
    cancels: u64,
    rejects: u64,
    expired: u64,
    /// Each match shows up once per side in `orders`.
    #[serde(skip)]
    sides: u64,
}

impl Tally {
    fn add(&mut self, kind: &Value, count: &Value) {
        let count = count.as_u64().unwrap_or(0);
        match kind.as_str() {
            Some("new") => self.orders += count,
            Some("canceled") => self.cancels += count,
            Some("rejected") => self.rejects += count,
            Some("expired") => self.expired += count,
            Some("trade") => {
                self.sides += count;
                self.trades = self.sides / 2;
            }
            _ => {}
        }
    }

    fn merge(&mut self, other: &Self) {
        self.orders += other.orders;
        self.trades += other.trades;
        self.cancels += other.cancels;
        self.rejects += other.rejects;
        self.expired += other.expired;
    }
}

/// Strips a scheme, credentials and path, for dialing and so that passwords are never displayed.
pub fn host_port(url: &str, default_port: u16) -> String {
    let url = url.split(',').next().unwrap_or(url).trim();
    let address = url.split_once("://").map_or(url, |(_, rest)| rest);
    let address = address.split('/').next().unwrap_or(address);
    let address = address.rsplit_once('@').map_or(address, |(_, host)| host);
    if address
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    {
        address.to_string()
    } else {
        format!("{address}:{default_port}")
    }
}

/// The version from `select build()`, which reads "Build Information: QuestDB 8.2.3, JDK ...".
fn questdb_version(build: &str) -> String {
    let version = build.split_once("QuestDB ").map_or(build, |(_, rest)| rest);
    version
        .split(',')
        .next()
        .unwrap_or(version)
        .trim()
        .to_string()
}

#[derive(Debug, Clone)]
pub struct Targets {
    pub gateways: Vec<String>,
    /// A headless Service, as `host:port`. Its addresses replace `gateways`, because a normal
    /// Service reaches one pod per call and a reload has to reach all of them.
    pub gateway_dns: Option<String>,
    pub outbox: String,
    pub questdb: String,
    pub nats: String,
    pub valkey: Option<String>,
    /// Nothing else reveals the websocket service, so it's only shown if it's named.
    pub websocket: Option<String>,
}

pub struct Upstreams {
    pub targets: Targets,
    token: Option<String>,
    http: Client,
}

impl Upstreams {
    pub fn new(targets: Targets, token: Option<String>) -> Result<Self, String> {
        Ok(Self {
            targets,
            token,
            http: http_client(Some(REQUEST_TIMEOUT))?,
        })
    }

    /// Every gateway right now. Pods come and go, so this resolves on each call. If the name won't
    /// resolve it's kept as a target, so the failure shows up against it.
    pub async fn gateway_urls(&self) -> Vec<String> {
        let Some(name) = &self.targets.gateway_dns else {
            return self.targets.gateways.clone();
        };
        match lookup_host(name.as_str()).await {
            Ok(addresses) => {
                let mut urls: Vec<String> = addresses
                    .map(|address| format!("http://{address}"))
                    .collect();
                urls.sort_unstable();
                urls.dedup();
                urls
            }
            Err(err) => {
                warn!("Gateways: can't resolve {name}: {err}");
                vec![format!("http://{name}")]
            }
        }
    }

    fn gateway_request(&self, method: Method, gateway: &str, path: &str) -> RequestBuilder {
        let request = self.http.request(method, format!("{gateway}{path}"));
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    pub async fn gateway_call(&self, method: Method, gateway: &str, path: &str) -> GatewayResult {
        GatewayResult::new(
            gateway,
            send(self.gateway_request(method, gateway, path)).await,
        )
    }

    /// Makes the same call on every gateway at once, and keeps each answer.
    pub async fn each_gateway(&self, method: Method, path: &str) -> Vec<GatewayResult> {
        let gateways = self.gateway_urls().await;
        let calls = gateways
            .iter()
            .map(|gateway| self.gateway_call(method.clone(), gateway, path));
        join_all(calls).await
    }

    /// Cancelling is the one action that isn't per gateway. Each gateway reads the same open orders
    /// and cancels them all, so asking a second one would only repeat the work and double the
    /// counts. The next gateway is tried only if the last gave no usable answer, which is safe
    /// because cancelling a finished order changes nothing.
    pub async fn cancel_all(&self, symbol: &str) -> Vec<GatewayResult> {
        let path = format!("/api/v1/admin/markets/{symbol}/cancel-all");
        let mut attempts = Vec::new();
        for gateway in &self.gateway_urls().await {
            let request = self
                .gateway_request(Method::POST, gateway, &path)
                .timeout(CANCEL_ALL_TIMEOUT);
            let result = GatewayResult::new(gateway, send(request).await);
            let answered = matches!(result.status, Some(status) if status < 500);
            attempts.push(result);
            if answered {
                break;
            }
        }
        attempts
    }

    /// Every gateway's markets, or why it couldn't say.
    pub async fn live_markets(&self) -> Vec<(String, Result<Vec<LiveMarket>, String>)> {
        let results = self
            .each_gateway(Method::GET, "/api/v1/admin/markets")
            .await;
        let parse = |result: GatewayResult| match (result.ok, result.error) {
            (true, _) => serde_json::from_value::<MarketList>(result.body)
                .map(|list| list.markets)
                .map_err(|err| format!("unexpected market list: {err}")),
            (false, error) => Err(error.unwrap_or_else(|| "failed".to_string())),
        };
        results
            .into_iter()
            .map(|result| (result.gateway.clone(), parse(result)))
            .collect()
    }

    pub async fn position(&self) -> Result<Position, String> {
        let reply = send(self.http.get(format!("{}/position", self.targets.outbox))).await?;
        if !reply.ok() {
            return Err(reply.error());
        }
        serde_json::from_value(reply.body)
            .map_err(|err| format!("unexpected outbox position: {err}"))
    }

    /// Runs SQL, and returns no rows if the table doesn't exist. Tables are made on first write, so
    /// that only means nothing has happened yet.
    async fn sql(&self, query: &str) -> Result<Vec<Vec<Value>>, String> {
        self.sql_with(query, REQUEST_TIMEOUT).await
    }

    async fn sql_with(&self, query: &str, limit: Duration) -> Result<Vec<Vec<Value>>, String> {
        #[derive(Deserialize)]
        struct Exec {
            #[serde(default)]
            dataset: Vec<Vec<Value>>,
            error: Option<String>,
        }
        let url = format!("{}/exec", self.targets.questdb);
        let request = self.http.get(url).query(&[("query", query)]).timeout(limit);
        let reply = send(request).await?;
        let exec: Exec = serde_json::from_value(reply.body.clone()).map_err(|_| reply.error())?;
        match exec.error {
            Some(error) if error.contains("does not exist") => Ok(Vec::new()),
            Some(error) => Err(error),
            None if !reply.ok() => Err(reply.error()),
            None => Ok(exec.dataset),
        }
    }

    pub async fn recent(
        &self,
        table: &Table,
        filter: &Filter,
        limit: usize,
    ) -> Result<Vec<Map<String, Value>>, String> {
        self.newest(table, filter, None, limit).await
    }

    /// The newest `limit` rows, newest first, from `since` on if it's given.
    pub async fn newest(
        &self,
        table: &Table,
        filter: &Filter,
        since: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Map<String, Value>>, String> {
        let Some(query) = table.newest_sql(filter, since, limit) else {
            return Ok(Vec::new());
        };
        let rows = self.sql(&query).await?;
        Ok(rows.iter().map(|row| table.row(row)).collect())
    }

    /// How many rows match from `since` on. QuestDB answers this from the time index, so it stays
    /// cheap when the rows it counts are too many to read.
    pub async fn count_since(
        &self,
        table: &Table,
        filter: &Filter,
        since: &str,
    ) -> Result<u64, String> {
        let Some(query) = table.count_sql(filter, since) else {
            return Ok(0);
        };
        let rows = self.sql(&query).await?;
        Ok(rows
            .first()
            .and_then(|row| row.first())
            .and_then(Value::as_u64)
            .unwrap_or(0))
    }

    /// The resting book, from the tape: each order's last report, for the orders still working.
    /// This is the query the gateway uses for cancel-all, summed by price. It scans the market's
    /// history, so it runs when someone starts watching a market, and never in a loop.
    pub async fn book_snapshot(&self, symbol: &str) -> Result<Vec<(String, i64, i64)>, String> {
        let query = format!(
            "select side, price, sum(leaves) from (select account, cl_ord_id, order_id, \
             last(side) side, last(price) price, last(leaves_qty) leaves, \
             last(ord_status) ord_status from orders where symbol = '{symbol}' \
             and (ord_rej_reason is null or ord_rej_reason != 'duplicate_order')) \
             where ord_status in ('new', 'partially_filled') and price != null \
             group by side, price limit {SNAPSHOT_LEVELS}"
        );
        let rows = self.sql_with(&query, SNAPSHOT_TIMEOUT).await?;
        Ok(rows
            .iter()
            .filter_map(|row| match row.as_slice() {
                [side, price, size, ..] => {
                    Some((side.as_str()?.to_string(), price.as_i64()?, size.as_i64()?))
                }
                _ => None,
            })
            .collect())
    }

    /// Prices over a window. Each match has one row per side, so buys count each once.
    pub async fn candles(&self, symbol: &str, range: Range) -> Result<Vec<Value>, String> {
        let query = format!(
            "select ts, first(price), max(price), min(price), last(price), sum(quantity) \
             from trades where symbol = '{symbol}' and side = 'buy' \
             and ts > dateadd('m', -{}, now()) sample by {}s",
            range.minutes(),
            range.bucket_secs(),
        );
        let rows = self.sql(&query).await?;
        let price = |value: &Value| value.as_i64().map(decimal::format);
        Ok(rows
            .iter()
            .filter_map(|row| match row.as_slice() {
                [time, open, high, low, close, volume] => Some(json!({
                    "time": time,
                    "open": price(open)?,
                    "high": price(high)?,
                    "low": price(low)?,
                    "close": price(close)?,
                    "volume": price(volume)?,
                })),
                _ => None,
            })
            .collect())
    }

    /// The past 24 hours, and the last price whenever it was.
    pub async fn day_summary(&self, symbol: &str) -> Result<Value, String> {
        let day = format!(
            "select first(price), max(price), min(price), sum(quantity), count() from trades \
             where symbol = '{symbol}' and side = 'buy' and ts > dateadd('h', -24, now())"
        );
        let last = format!(
            "select price, ts from trades where symbol = '{symbol}' and side = 'buy' \
             order by ts desc limit 1"
        );
        let (day, last) = tokio::join!(self.sql(&day), self.sql(&last));
        let (day, last) = (day?, last?);
        let price = |value: Option<&Value>| value.and_then(Value::as_i64).map(decimal::format);
        let row = day.first().map_or(&[][..], Vec::as_slice);
        let newest = last.first().map_or(&[][..], Vec::as_slice);
        Ok(json!({
            "open": price(row.first()),
            "high": price(row.get(1)),
            "low": price(row.get(2)),
            "volume": price(row.get(3)),
            "trades": row.get(4).and_then(Value::as_u64).unwrap_or(0),
            "last": price(newest.first()),
            "last_time": newest.get(1),
        }))
    }

    /// What the exchange did over a window. A row in `orders` is one step in an order's life, not
    /// one order: a `new` row is an order arriving, so that's what's counted as an order. Each match
    /// has a row per side, so trades count the buy side.
    pub async fn summary(&self, range: Range) -> Result<Value, String> {
        let (minutes, bucket) = (range.minutes(), range.summary_bucket_secs());
        let window = format!("ts > dateadd('m', -{minutes}, now())");
        let events =
            format!("select ts, exec_type, count() from orders where {window} sample by {bucket}s");
        let trades = format!(
            "select ts, count() from trades where side = 'buy' and {window} sample by {bucket}s"
        );
        let markets = format!(
            "select symbol, exec_type, count() from orders where {window} limit {SUMMARY_ROWS}"
        );
        let minute = "ts > dateadd('s', -60, now())";
        let recent = format!("select exec_type, count() from orders where {minute}");
        let recent_trades = format!("select count() from trades where side = 'buy' and {minute}");
        let newest = "select ts from orders order by ts desc limit 1";
        let (events, trades, markets, recent, recent_trades, newest) = tokio::join!(
            self.sql(&events),
            self.sql(&trades),
            self.sql(&markets),
            self.sql(&recent),
            self.sql(&recent_trades),
            self.sql(newest),
        );

        let mut series: BTreeMap<String, Tally> = BTreeMap::new();
        for row in events? {
            if let [time, kind, count] = row.as_slice() {
                let time = time.as_str().unwrap_or_default().to_string();
                series.entry(time).or_default().add(kind, count);
            }
        }
        for row in trades? {
            if let [time, count] = row.as_slice() {
                let time = time.as_str().unwrap_or_default().to_string();
                series.entry(time).or_default().trades = count.as_u64().unwrap_or(0);
            }
        }
        let mut totals = Tally::default();
        for tally in series.values() {
            totals.merge(tally);
        }

        let mut by_market: BTreeMap<String, Tally> = BTreeMap::new();
        for row in markets? {
            if let [symbol, kind, count] = row.as_slice() {
                let symbol = symbol.as_str().unwrap_or_default().to_string();
                by_market.entry(symbol).or_default().add(kind, count);
            }
        }
        let mut busiest: Vec<(String, Tally)> = by_market.into_iter().collect();
        busiest.sort_by_key(|(_, tally)| std::cmp::Reverse(tally.orders));
        busiest.truncate(BUSIEST);

        let mut last_minute = Tally::default();
        for row in recent? {
            if let [kind, count] = row.as_slice() {
                last_minute.add(kind, count);
            }
        }
        last_minute.trades = recent_trades?
            .first()
            .and_then(|row| row.first())
            .and_then(Value::as_u64)
            .unwrap_or(0);

        Ok(json!({
            "range": range.name(),
            "bucket_secs": bucket,
            "newest": newest?.first().and_then(|row| row.first()).cloned(),
            "last_minute": last_minute,
            "totals": totals,
            "series": series
                .iter()
                .map(|(time, tally)| json!({ "time": time, "counts": tally }))
                .collect::<Vec<_>>(),
            "busiest": busiest
                .iter()
                .map(|(symbol, tally)| json!({ "symbol": symbol, "counts": tally }))
                .collect::<Vec<_>>(),
        }))
    }

    /// Each gateway's own view of the cluster.
    pub async fn cluster_answers(&self) -> Vec<ClusterAnswer> {
        let results = self
            .each_gateway(Method::GET, "/api/v1/admin/cluster")
            .await;
        results
            .into_iter()
            .map(|result| {
                let standalone = result.ok
                    && result.body.get("mode").and_then(Value::as_str) == Some("standalone");
                let view = result
                    .body
                    .get("view")
                    .filter(|_| result.ok)
                    .and_then(|view| serde_json::from_value(view.clone()).ok());
                ClusterAnswer { standalone, view }
            })
            .collect()
    }

    /// `/metrics` from every gateway that answers.
    pub async fn gateway_metrics(&self) -> Vec<String> {
        self.each_gateway(Method::GET, "/metrics")
            .await
            .into_iter()
            .filter(|result| result.ok)
            .filter_map(|result| result.body.as_str().map(str::to_string))
            .collect()
    }

    pub async fn tape_head(&self) -> Result<TapeHead, String> {
        let order = "select exec_id, ts from orders order by ts desc limit 1";
        let trade = "select ts from trades order by ts desc limit 1";
        let (order, trade) = tokio::join!(self.sql(order), self.sql(trade));
        let order = order?;
        let row = order.first().map_or(&[][..], Vec::as_slice);
        Ok(TapeHead {
            exec_id: row.first().and_then(Value::as_u64),
            time: row.get(1).and_then(Value::as_str).map(str::to_string),
            newest_trade_ms: trade?
                .first()
                .and_then(|row| row.first())
                .and_then(Value::as_str)
                .and_then(parse_time_ms),
        })
    }

    /// Last price and 24 hour totals for every market, read by time, so it's cheap at any tape size.
    pub async fn market_rows(&self) -> Result<HashMap<String, MarketStats>, String> {
        let day = "select symbol, sum(quantity), count() from trades \
                   where side = 'buy' and ts > dateadd('h', -24, now())";
        let last = "select symbol, price, ts from trades where side = 'buy' \
                    latest on ts partition by symbol";
        let (day, last) = tokio::join!(self.sql(day), self.sql(last));
        let mut rows: HashMap<String, MarketStats> = HashMap::new();
        for row in last? {
            if let [symbol, price, time] = row.as_slice()
                && let Some(symbol) = symbol.as_str()
            {
                let stats = rows.entry(symbol.to_string()).or_default();
                stats.last = price.as_i64().map(decimal::format);
                stats.last_time = time.as_str().map(str::to_string);
            }
        }
        for row in day? {
            if let [symbol, volume, count] = row.as_slice()
                && let Some(symbol) = symbol.as_str()
            {
                let stats = rows.entry(symbol.to_string()).or_default();
                stats.volume_24h = volume.as_i64().map(decimal::format);
                stats.trades_24h = count.as_u64();
            }
        }
        Ok(rows)
    }

    /// Orders still working, per market. This reads each order's last row, so it has a short leash.
    pub async fn open_orders(&self) -> Result<HashMap<String, u64>, String> {
        let query = "select symbol, count() from (select symbol, account, cl_ord_id, order_id, \
                     last(ord_status) ord_status from orders where \
                     (ord_rej_reason is null or ord_rej_reason != 'duplicate_order')) \
                     where ord_status in ('new', 'partially_filled')";
        let rows = self.sql_with(query, OPEN_ORDERS_TIMEOUT).await?;
        Ok(rows
            .iter()
            .filter_map(|row| match row.as_slice() {
                [symbol, count] => Some((symbol.as_str()?.to_string(), count.as_u64()?)),
                _ => None,
            })
            .collect())
    }

    async fn gateway_status(&self, gateway: &str) -> ServiceStatus {
        let status = ServiceStatus::new("gateway", gateway, true);
        // Any answer proves the process is up, so `/readyz` covers both checks.
        let result = self.gateway_call(Method::GET, gateway, "/readyz").await;
        let Some(code) = result.status else {
            return status.unreachable(result.error.unwrap_or_default());
        };
        ServiceStatus {
            reachable: true,
            ready: code == 200,
            version: result
                .body
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_string),
            checks: result.body.get("checks").cloned(),
            error: result.error,
            ..status
        }
    }

    async fn outbox_status(&self) -> ServiceStatus {
        let status = ServiceStatus::new("outbox", &self.targets.outbox, true);
        let reply = match send(self.http.get(format!("{}/readyz", self.targets.outbox))).await {
            Ok(reply) => reply,
            Err(error) => return status.unreachable(error),
        };
        // `/readyz` only says no. `/position` says why, including a halt.
        let position = self.position().await.ok();
        let halted = position
            .as_ref()
            .and_then(|position| position.halted.clone());
        let waiting = position
            .as_ref()
            .is_some_and(|position| position.configured == Some(false))
            .then(|| "No webhook is configured yet.".to_string());
        ServiceStatus {
            reachable: true,
            ready: reply.ok(),
            checks: position.map(|position| position.view()),
            error: waiting.or(halted).or((!reply.ok()).then(|| reply.error())),
            ..status
        }
    }

    async fn questdb_status(&self) -> ServiceStatus {
        let status = ServiceStatus::new("questdb", &self.targets.questdb, true);
        match self.sql("select build()").await {
            Ok(rows) => {
                let build = rows
                    .first()
                    .and_then(|row| row.first())
                    .and_then(Value::as_str);
                ServiceStatus {
                    reachable: true,
                    ready: true,
                    version: build.map(questdb_version),
                    ..status
                }
            }
            Err(error) => status.unreachable(error),
        }
    }

    /// NATS sends an `INFO` line on connect, so this needs no client library.
    async fn nats_status(&self) -> ServiceStatus {
        let address = host_port(&self.targets.nats, NATS_PORT);
        let status = ServiceStatus::new("nats", &address, true);
        let probe = async {
            let stream = TcpStream::connect(&address)
                .await
                .map_err(|err| err.to_string())?;
            let mut line = String::new();
            BufReader::new(stream)
                .read_line(&mut line)
                .await
                .map_err(|err| err.to_string())?;
            line.strip_prefix("INFO ")
                .map(str::to_string)
                .ok_or_else(|| "not a NATS greeting".to_string())
        };
        match timeout(PROBE_TIMEOUT, probe).await {
            Ok(Ok(info)) => {
                let info: Value = serde_json::from_str(&info).unwrap_or_default();
                let version = info
                    .get("version")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                ServiceStatus {
                    reachable: true,
                    ready: true,
                    version,
                    ..status
                }
            }
            Ok(Err(error)) => status.unreachable(error),
            Err(_) => status.unreachable("timed out".to_string()),
        }
    }

    /// Valkey is optional: gateways skip rate limits without it, so it's only checked if configured.
    /// A password challenge still proves it's up.
    async fn valkey_status(&self, url: &str) -> ServiceStatus {
        let address = host_port(url, VALKEY_PORT);
        let status = ServiceStatus::new("valkey", &address, false);
        let probe = async {
            let mut stream = TcpStream::connect(&address)
                .await
                .map_err(|err| err.to_string())?;
            stream
                .write_all(b"PING\r\n")
                .await
                .map_err(|err| err.to_string())?;
            let mut line = String::new();
            BufReader::new(stream)
                .read_line(&mut line)
                .await
                .map_err(|err| err.to_string())?;
            Ok::<_, String>(line)
        };
        match timeout(PROBE_TIMEOUT, probe).await {
            Ok(Ok(line)) if line.starts_with("+PONG") || line.starts_with("-NOAUTH") => {
                ServiceStatus {
                    reachable: true,
                    ready: true,
                    ..status
                }
            }
            Ok(Ok(line)) => status.unreachable(format!("unexpected reply: {}", line.trim())),
            Ok(Err(error)) => status.unreachable(error),
            Err(_) => status.unreachable("timed out".to_string()),
        }
    }

    /// Any connection proves it's listening. It has no health route.
    async fn websocket_status(&self, url: &str) -> ServiceStatus {
        let address = host_port(url, WEBSOCKET_PORT);
        let status = ServiceStatus::new("websocket", &address, false);
        match timeout(PROBE_TIMEOUT, TcpStream::connect(&address)).await {
            Ok(Ok(_)) => ServiceStatus {
                reachable: true,
                ready: true,
                ..status
            },
            Ok(Err(error)) => status.unreachable(error.to_string()),
            Err(_) => status.unreachable("timed out".to_string()),
        }
    }

    pub async fn status(&self) -> Vec<ServiceStatus> {
        let urls = self.gateway_urls().await;
        let gateways = urls.iter().map(|gateway| self.gateway_status(gateway));
        let valkey = async {
            match &self.targets.valkey {
                Some(url) => Some(self.valkey_status(url).await),
                None => None,
            }
        };
        let websocket = async {
            match &self.targets.websocket {
                Some(url) => Some(self.websocket_status(url).await),
                None => None,
            }
        };
        let (mut services, outbox, questdb, nats, valkey, websocket) = tokio::join!(
            join_all(gateways),
            self.outbox_status(),
            self.questdb_status(),
            self.nats_status(),
            valkey,
            websocket,
        );
        services.push(outbox);
        services.extend(websocket);
        services.extend([questdb, nats]);
        services.extend(valkey);
        services
    }
}

/// The symbols a reload reply lists, or `None` if it doesn't look like one.
pub fn reloaded_symbols(body: &Value) -> Option<Vec<&str>> {
    body.get("markets")?
        .as_array()?
        .iter()
        .map(Value::as_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_lose_their_credentials() {
        assert_eq!(
            host_port("redis://:secret@cache:6380/0", 6379),
            "cache:6380"
        );
        assert_eq!(host_port("localhost", 4222), "localhost:4222");
        assert_eq!(host_port("nats://a:4223,nats://b:4223", 4222), "a:4223");
        assert_eq!(host_port("nats://user:pw@a", 4222), "a:4222");
    }

    #[test]
    fn sql_has_only_what_it_was_given() {
        let player = Uuid::nil();
        let filter = Filter {
            symbol: Some("ITEM".to_string()),
            player: Some(player),
            ..Filter::default()
        };
        let sql = ORDERS.newest_sql(&filter, None, 5).unwrap();
        assert!(sql.ends_with(&format!(" from orders where symbol = 'ITEM' and account = '{player}' order by ts desc limit 5")), "{sql}");
        assert!(
            TRADES
                .newest_sql(&Filter::default(), None, 1)
                .unwrap()
                .ends_with(" from trades order by ts desc limit 1")
        );
        let since = "2026-01-01T00:00:00.000000Z";
        let sql = ORDERS
            .newest_sql(&Filter::default(), Some(since), 9)
            .unwrap();
        assert!(
            sql.ends_with(&format!(" where ts >= '{since}' order by ts desc limit 9")),
            "{sql}"
        );
        assert_eq!(
            ORDERS.count_sql(&Filter::default(), since).unwrap(),
            format!("select count() from orders where ts >= '{since}'")
        );
    }

    #[test]
    fn rows_show_decimals_and_drop_nulls() {
        let values = [
            json!("t"),
            json!("ITEM"),
            json!("buy"),
            json!("p"),
            json!("c"),
            json!(9),
            json!(1),
            json!(2),
            json!(1_050_000_000),
            Value::Null,
        ];
        let row = TRADES.row(&values);
        assert_eq!(row["price"], "10.5");
        assert_eq!(row["player"], "p");
        assert!(!row.contains_key("size"));
    }

    #[test]
    fn times_read_back_from_questdb() {
        assert_eq!(parse_time_ms("1970-01-01T00:00:01.500000Z"), Some(1500));
        assert_eq!(
            parse_time_ms("2026-11-06T16:47:36.120000Z"),
            Some(1_793_983_656_120)
        );
        assert_eq!(parse_time_ms("not a time"), None);
    }

    #[test]
    fn an_order_counts_once_however_many_steps_it_takes() {
        let mut tally = Tally::default();
        tally.add(&json!("new"), &json!(10));
        tally.add(&json!("trade"), &json!(8));
        tally.add(&json!("canceled"), &json!(2));
        tally.add(&json!("rejected"), &json!(1));
        assert_eq!(tally.orders, 10);
        assert_eq!(tally.trades, 4);
        assert_eq!(tally.cancels, 2);
        assert_eq!(tally.rejects, 1);
    }

    #[test]
    fn reads_the_version_from_a_build_string() {
        assert_eq!(
            questdb_version("Build Information: QuestDB 8.2.3, JDK 17.0.7, Commit Hash abc"),
            "8.2.3"
        );
    }

    #[test]
    fn delivery_state_names_the_worst_thing_first() {
        let position = |halted: Option<&str>, verified, failures, durable| Position {
            durable_position: durable,
            durable_exec_id: 1,
            delivered_position: 100,
            verified,
            halted: halted.map(str::to_string),
            consecutive_failures: failures,
            webhook_url: None,
            config_id: None,
            configured: None,
        };
        assert_eq!(
            position(Some("bad"), false, 3, 200).view()["state"],
            "halted"
        );
        assert_eq!(position(None, false, 0, 200).view()["state"], "verifying");
        assert_eq!(position(None, true, 3, 200).view()["state"], "failing");
        assert_eq!(position(None, true, 0, 200).view()["backlog"], 100);
        assert_eq!(position(None, true, 0, 200).view()["state"], "delivering");
        assert_eq!(position(None, true, 0, 100).view()["state"], "idle");
    }
}
