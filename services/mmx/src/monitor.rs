//! Watches the exchange in the background, so `mmx serve` can say when something started rather than
//! only what's true now, and so every viewer shares one set of probes.
//!
//! `mmx` can't read Aeron, so the cluster comes from what each gateway hears: every member publishes
//! its own byte-identical copy of the report stream, and a gateway reads them all. A member counts as
//! up if a gateway hears it. Consumers' lag compares their last exec ID with the newest one seen.

use crate::decimal;
use crate::upstream::{
    ClusterAnswer, Position, ServiceStatus, TapeHead, Upstreams, install_verdict,
};

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_nats::ConnectOptions;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{MissedTickBehavior, interval};
use tracing::warn;

const EVERY: Duration = Duration::from_secs(5);
/// A viewer may read a reading this old. Past it, they take a fresh one.
const FRESH: Duration = Duration::from_secs(2);
/// How far back the latency figures reach.
const LATENCY_WINDOW: Duration = Duration::from_secs(60);
/// A consumer behind the tape and not moving for this long has stopped, rather than being busy.
const STALLED_AFTER: Duration = Duration::from_secs(30);
const STATS_EVERY: Duration = Duration::from_secs(10);
/// Open orders mean reading every order's last state, so this runs rarely and gives up early.
const OPEN_ORDERS_EVERY: Duration = Duration::from_secs(60);
const OPEN_ORDERS_BACKOFF: Duration = Duration::from_secs(300);
const LOG_KEPT: usize = 500;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// `2026-11-06T16:47:36.120Z`, the form QuestDB's timestamps take.
pub fn iso(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let rest = ms % 86_400_000;
    // Howard Hinnant's civil-from-days, so this needs no date library.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct MemberRow {
    pub member: usize,
    pub role: &'static str,
    pub connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    /// Bytes behind the furthest member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lag_bytes: Option<i64>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct ClusterSummary {
    /// `cluster`, `standalone`, or `unknown` when no gateway could say.
    pub mode: &'static str,
    pub members_up: usize,
    pub members_total: usize,
    pub quorum_needed: usize,
    pub quorum: bool,
    pub leader: Option<usize>,
    pub term: i64,
    pub leader_changes: u64,
    /// The last change any gateway saw. Gateways only know what happened since they started.
    pub last_leader_change: Option<String>,
    /// Newest exec ID any gateway heard.
    pub head_exec_id: u64,
    /// How many gateways answered, of how many there are.
    pub gateways_reporting: usize,
    pub gateways_total: usize,
    pub members: Vec<MemberRow>,
}

/// Puts each gateway's view together. A member is up if any gateway hears it, so a gateway that's
/// cut off can't mark the cluster down while another still reaches it.
pub fn merge_cluster(answers: &[ClusterAnswer]) -> ClusterSummary {
    let views: Vec<_> = answers
        .iter()
        .filter_map(|answer| answer.view.as_ref())
        .collect();
    let mode = if !views.is_empty() {
        "cluster"
    } else if answers.iter().any(|answer| answer.standalone) {
        "standalone"
    } else {
        "unknown"
    };
    let total = views
        .iter()
        .map(|view| view.members.len())
        .max()
        .unwrap_or(0);
    let positions: Vec<Option<i64>> = (0..total)
        .map(|member| {
            views
                .iter()
                .filter_map(|view| view.members.get(member))
                .filter(|seen| seen.connected)
                .filter_map(|seen| seen.position)
                .max()
        })
        .collect();
    let connected: Vec<bool> = (0..total)
        .map(|member| {
            views
                .iter()
                .any(|view| view.members.get(member).is_some_and(|seen| seen.connected))
        })
        .collect();
    let term = views.iter().map(|view| view.term).max().unwrap_or(-1);
    let leader = views
        .iter()
        .filter(|view| view.term == term && view.leader_connected)
        .find_map(|view| view.leader);
    let furthest = positions.iter().flatten().max().copied();
    let members = (0..total)
        .map(|member| MemberRow {
            member,
            role: if !connected[member] {
                "unreachable"
            } else if leader == Some(member) {
                "leader"
            } else {
                "follower"
            },
            connected: connected[member],
            position: positions[member],
            lag_bytes: positions[member].zip(furthest).map(|(own, top)| top - own),
        })
        .collect();
    let up = connected.iter().filter(|heard| **heard).count();
    let needed = total / 2 + usize::from(total > 0);
    ClusterSummary {
        mode,
        members_up: up,
        members_total: total,
        quorum_needed: needed,
        quorum: total > 0 && up >= needed,
        leader,
        term,
        leader_changes: views
            .iter()
            .map(|view| view.leader_changes)
            .max()
            .unwrap_or(0),
        last_leader_change: views
            .iter()
            .filter_map(|view| view.last_leader_change_ms)
            .max()
            .map(iso),
        head_exec_id: views
            .iter()
            .map(|view| view.last_exec_id)
            .max()
            .unwrap_or(0),
        gateways_reporting: views.len(),
        gateways_total: answers.len(),
        members,
    }
}

/// Cumulative counters from every gateway's `/metrics`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GatewayCounters {
    /// Upper bound in seconds, and how many orders fell at or under it.
    pub buckets: Vec<(f64, u64)>,
    pub count: u64,
    pub refused: HashMap<String, u64>,
}

impl GatewayCounters {
    pub fn parse(text: &str) -> Self {
        let mut out = Self::default();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let Some((name, value)) = line.rsplit_once(' ') else {
                continue;
            };
            let Ok(value) = value.parse::<f64>() else {
                continue;
            };
            let value = value as u64;
            if let Some(rest) = name.strip_prefix("mmx_gateway_order_latency_seconds_bucket{le=\"")
            {
                if let Ok(bound) = rest.trim_end_matches("\"}").parse::<f64>() {
                    out.buckets.push((bound, value));
                }
            } else if name == "mmx_gateway_order_latency_seconds_count" {
                out.count = value;
            } else if let Some(rest) =
                name.strip_prefix("mmx_gateway_orders_rejected_total{reason=\"")
            {
                out.refused
                    .insert(rest.trim_end_matches("\"}").to_string(), value);
            } else if name == "mmx_gateway_halted_rejects_total" {
                out.refused.insert("halted".to_string(), value);
            }
        }
        out
    }

    fn add(&mut self, other: &Self) {
        if self.buckets.is_empty() {
            self.buckets.clone_from(&other.buckets);
        } else {
            for (mine, theirs) in self.buckets.iter_mut().zip(&other.buckets) {
                mine.1 += theirs.1;
            }
        }
        self.count += other.count;
        for (reason, count) in &other.refused {
            *self.refused.entry(reason.clone()).or_default() += count;
        }
    }

    /// What changed since `older`. A gateway that restarted reads as no change, not a negative.
    fn since(&self, older: &Self) -> Self {
        let buckets = self
            .buckets
            .iter()
            .enumerate()
            .map(|(index, (bound, count))| {
                let before = older.buckets.get(index).map_or(0, |bucket| bucket.1);
                (*bound, count.saturating_sub(before))
            })
            .collect();
        let refused = self
            .refused
            .iter()
            .map(|(reason, count)| {
                let before = older.refused.get(reason).copied().unwrap_or(0);
                (reason.clone(), count.saturating_sub(before))
            })
            .collect();
        Self {
            buckets,
            count: self.count.saturating_sub(older.count),
            refused,
        }
    }
}

/// The value a share of orders fell under, as an upper bound, because a bucket can't say better.
pub fn quantile_ms(counters: &GatewayCounters, share: f64) -> Option<f64> {
    if counters.count == 0 {
        return None;
    }
    let rank = (counters.count as f64 * share).ceil() as u64;
    counters
        .buckets
        .iter()
        .find(|(_, below)| *below >= rank)
        .map(|(bound, _)| bound * 1000.0)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MarketStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_24h: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trades_24h: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_orders: Option<u64>,
    /// Best bid and ask when the market last traded. The ticker only speaks on a trade.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_bid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_ask: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spread: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct TickerMessage {
    #[serde(default)]
    product_id: String,
    #[serde(default)]
    price: String,
    #[serde(default)]
    best_bid: String,
    #[serde(default)]
    best_ask: String,
    #[serde(default)]
    timestamp: String,
}

#[derive(Debug, Clone, Default)]
struct Ticker {
    price: String,
    bid: Option<String>,
    ask: Option<String>,
    time: String,
}

fn spread_of(bid: Option<&str>, ask: Option<&str>) -> Option<String> {
    let (bid, ask) = (decimal::parse(bid?).ok()?, decimal::parse(ask?).ok()?);
    (ask >= bid).then(|| decimal::format(ask - bid))
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub subject: String,
    pub check: String,
    pub detail: String,
    /// `bad` means orders are affected.
    pub severity: &'static str,
    pub since: String,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub time: String,
    pub level: &'static str,
    pub subject: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

/// How far a consumer has got. `behind` is in tape events.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Consumer {
    pub name: &'static str,
    /// `caught_up`, `behind`, `stalled` or `unknown`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
}

/// Tracks one consumer's progress between readings.
#[derive(Default)]
struct Progress {
    exec_id: u64,
    moved: Option<Instant>,
}

impl Progress {
    fn read(&mut self, name: &'static str, exec_id: Option<u64>, head: u64) -> Consumer {
        let Some(exec_id) = exec_id else {
            return Consumer {
                name,
                state: "unknown",
                behind: None,
                exec_id: None,
                last_seen: None,
            };
        };
        if exec_id != self.exec_id || self.moved.is_none() {
            self.exec_id = exec_id;
            self.moved = Some(Instant::now());
        }
        let behind = head.saturating_sub(exec_id);
        let idle = self.moved.is_some_and(|at| at.elapsed() >= STALLED_AFTER);
        Consumer {
            name,
            state: match (behind, idle) {
                (0, _) => "caught_up",
                (_, true) => "stalled",
                (_, false) => "behind",
            },
            behind: Some(behind),
            exec_id: Some(exec_id),
            last_seen: None,
        }
    }
}

#[derive(Default)]
struct Walker {
    conditions: HashMap<(String, String), String>,
    term: Option<(i64, Option<usize>)>,
    window: VecDeque<(Instant, GatewayCounters)>,
    outbox: Progress,
    reporter: Progress,
    stats_at: Option<Instant>,
    open_at: Option<(Instant, Duration)>,
}

pub struct Monitor {
    up: Arc<Upstreams>,
    started: String,
    started_ms: u64,
    reading: RwLock<Option<(Instant, Arc<Value>)>>,
    walker: tokio::sync::Mutex<Walker>,
    log: Mutex<VecDeque<LogEntry>>,
    stats: RwLock<HashMap<String, MarketStats>>,
    tickers: Mutex<HashMap<String, Ticker>>,
    last_ticker: Mutex<Option<u64>>,
}

impl Monitor {
    pub fn new(up: Arc<Upstreams>) -> Arc<Self> {
        Arc::new(Self {
            up,
            started: iso(now_ms()),
            started_ms: now_ms(),
            reading: RwLock::default(),
            walker: tokio::sync::Mutex::default(),
            log: Mutex::default(),
            stats: RwLock::default(),
            tickers: Mutex::default(),
            last_ticker: Mutex::default(),
        })
    }

    /// Keeps watching whether anyone's looking, so a start time is there when someone does.
    pub fn spawn(self: &Arc<Self>) {
        let monitor = self.clone();
        tokio::spawn(async move {
            let mut ticks = interval(EVERY);
            ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                monitor.walk().await;
            }
        });
        let monitor = self.clone();
        tokio::spawn(async move { monitor.follow_tickers().await });
    }

    /// Its own connection, because tickers can run hot and falling behind on them must not make
    /// every open book read the tape again.
    async fn follow_tickers(&self) {
        let url = self.up.targets.nats.clone();
        let client = match ConnectOptions::new()
            .retry_on_initial_connect()
            .max_reconnects(None)
            .connect(url.as_str())
            .await
        {
            Ok(client) => client,
            Err(err) => return warn!("Monitor: can't reach NATS: {err}"),
        };
        let mut stream = match client.subscribe("ticker.*").await {
            Ok(stream) => stream,
            Err(err) => return warn!("Monitor: can't follow tickers: {err}"),
        };
        while let Some(message) = stream.next().await {
            let Ok(ticker) = serde_json::from_slice::<TickerMessage>(&message.payload) else {
                continue;
            };
            let non_empty = |text: String| (!text.is_empty()).then_some(text);
            lock(&self.tickers).insert(
                ticker.product_id,
                Ticker {
                    price: ticker.price,
                    bid: non_empty(ticker.best_bid),
                    ask: non_empty(ticker.best_ask),
                    time: ticker.timestamp,
                },
            );
            *lock(&self.last_ticker) = Some(now_ms());
        }
    }

    /// The newest reading, taken now if the last one is too old.
    pub async fn status(&self) -> Arc<Value> {
        if let Some(reading) = self.current() {
            return reading;
        }
        self.walk().await
    }

    fn current(&self) -> Option<Arc<Value>> {
        let reading = self.reading.read().unwrap_or_else(PoisonError::into_inner);
        reading
            .as_ref()
            .filter(|(at, _)| at.elapsed() < FRESH)
            .map(|(_, value)| value.clone())
    }

    async fn walk(&self) -> Arc<Value> {
        let mut walker = self.walker.lock().await;
        // Someone else may have finished while this waited.
        if let Some(reading) = self.current() {
            return reading;
        }
        let value = Arc::new(self.read(&mut walker).await);
        *self.reading.write().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now(), value.clone()));
        self.refresh_stats(&mut walker).await;
        value
    }

    async fn refresh_stats(&self, walker: &mut Walker) {
        if walker.stats_at.is_some_and(|at| at.elapsed() < STATS_EVERY) {
            return;
        }
        walker.stats_at = Some(Instant::now());
        let due = walker.open_at.is_none_or(|(at, wait)| at.elapsed() >= wait);
        let (rows, open) = tokio::join!(self.up.market_rows(), async {
            if due {
                Some(self.up.open_orders().await)
            } else {
                None
            }
        });
        let previous_open: HashMap<String, u64> = {
            let stats = self.stats.read().unwrap_or_else(PoisonError::into_inner);
            stats
                .iter()
                .filter_map(|(symbol, row)| Some((symbol.clone(), row.open_orders?)))
                .collect()
        };
        let open = match open {
            Some(Ok(counts)) => {
                walker.open_at = Some((Instant::now(), OPEN_ORDERS_EVERY));
                counts
            }
            Some(Err(err)) => {
                warn!("Monitor: open orders: {err}");
                walker.open_at = Some((Instant::now(), OPEN_ORDERS_BACKOFF));
                HashMap::new()
            }
            None => previous_open,
        };
        let Ok(rows) = rows else { return };
        let mut stats: HashMap<String, MarketStats> = rows;
        for (symbol, count) in open {
            stats.entry(symbol).or_default().open_orders = Some(count);
        }
        *self.stats.write().unwrap_or_else(PoisonError::into_inner) = stats;
    }

    /// Stats for each market, with the newest prices the feed has given.
    pub fn stats(&self) -> HashMap<String, MarketStats> {
        let mut stats = self
            .stats
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for (symbol, ticker) in lock(&self.tickers).iter() {
            let row = stats.entry(symbol.clone()).or_default();
            row.last = Some(ticker.price.clone());
            row.last_time = Some(ticker.time.clone());
            row.spread = spread_of(ticker.bid.as_deref(), ticker.ask.as_deref());
            row.best_bid.clone_from(&ticker.bid);
            row.best_ask.clone_from(&ticker.ask);
        }
        stats
    }

    pub fn note(
        &self,
        level: &'static str,
        subject: String,
        message: String,
        actor: Option<String>,
    ) {
        let mut log = lock(&self.log);
        log.push_front(LogEntry {
            time: iso(now_ms()),
            level,
            subject,
            message,
            actor,
        });
        log.truncate(LOG_KEPT);
    }

    /// Newest first, since this server started. It isn't an audit log and doesn't survive a restart.
    pub fn log(&self, limit: usize) -> Value {
        let log = lock(&self.log);
        json!({
            "since": self.started,
            "entries": log.iter().take(limit).collect::<Vec<_>>(),
        })
    }

    async fn latency(&self, walker: &mut Walker) -> Value {
        let texts = self.up.gateway_metrics().await;
        let mut total = GatewayCounters::default();
        for text in &texts {
            total.add(&GatewayCounters::parse(text));
        }
        let now = Instant::now();
        walker.window.push_back((now, total.clone()));
        while walker
            .window
            .front()
            .is_some_and(|(at, _)| now - *at > LATENCY_WINDOW)
        {
            walker.window.pop_front();
        }
        let Some((oldest, base)) = walker.window.front() else {
            return Value::Null;
        };
        let recent = total.since(base);
        let mut refused: Vec<(String, u64)> = recent
            .refused
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(reason, count)| (reason.clone(), *count))
            .collect();
        refused.sort();
        json!({
            "window_secs": (now - *oldest).as_secs(),
            "orders": recent.count,
            "p50_ms": quantile_ms(&recent, 0.5),
            "p99_ms": quantile_ms(&recent, 0.99),
            "refused": refused.iter().map(|(_, count)| count).sum::<u64>(),
            "refused_by_reason": refused.into_iter().collect::<HashMap<_, _>>(),
        })
    }

    fn market_data(&self, head: Option<&TapeHead>) -> ServiceStatus {
        let heard = *lock(&self.last_ticker);
        let mut service = ServiceStatus::new("market-data", &self.up.targets.nats, true);
        service.last_seen = heard.map(iso);
        service.reachable = true;
        // The ticker only speaks on a trade, so silence alone means nothing. It only counts if
        // the tape holds a trade the feed never mentioned.
        let trade_ms = head.and_then(|head| head.newest_trade_ms);
        let patience = STALLED_AFTER.as_millis() as u64;
        // Nothing was heard before this server started, so older trades prove nothing.
        let since = heard.unwrap_or(self.started_ms);
        let missed = trade_ms.is_some_and(|trade| trade > since + patience);
        service.ready = !missed;
        if missed {
            service.error = Some("Trades on the tape aren't reaching the ticker feed.".to_string());
        }
        service
    }

    fn trade_reporter(&self, consumer: &Consumer, head: Option<&TapeHead>) -> ServiceStatus {
        let mut service = ServiceStatus::new("trade-reporter", &self.up.targets.questdb, true);
        service.last_seen = head.and_then(|head| head.time.clone());
        service.reachable = head.is_some();
        service.ready = consumer.state != "stalled" && head.is_some();
        if consumer.state == "stalled" {
            service.error = Some(format!(
                "{} events behind and not moving.",
                consumer.behind.unwrap_or(0)
            ));
        } else if head.is_none() {
            service.error = Some("Can't read QuestDB.".to_string());
        }
        service
    }

    fn engine(cluster: &ClusterSummary, gateways: &[&ServiceStatus]) -> ServiceStatus {
        let target = if cluster.members_total > 0 {
            format!(
                "{} of {} members",
                cluster.members_up, cluster.members_total
            )
        } else {
            "single process".to_string()
        };
        let mut service = ServiceStatus::new("matching-engine", &target, true);
        let links: Vec<bool> = gateways
            .iter()
            .filter_map(|gateway| gateway.checks.as_ref()?.get("engine")?.as_bool())
            .collect();
        service.reachable = !links.is_empty();
        service.ready = links.iter().any(|link| *link);
        if links.is_empty() {
            service.error =
                Some("No gateway answered, so nothing can reach the engine.".to_string());
        } else if !service.ready {
            service.error = Some("No gateway has a working link to the engine.".to_string());
        }
        service
    }

    fn conditions(
        walker: &mut Walker,
        monitor: &Self,
        services: &[ServiceStatus],
        cluster: &ClusterSummary,
        position: Option<&Position>,
    ) -> Vec<Condition> {
        let mut found: Vec<(String, String, String, &'static str)> = Vec::new();
        for service in services
            .iter()
            .filter(|service| !service.ready || service.checks_failed())
        {
            let subject = if service.name == "gateway" {
                format!("gateway {}", service.target)
            } else {
                service.name.clone()
            };
            let failed = service.failed_checks();
            if failed.is_empty() {
                let check = if service.reachable {
                    "ready"
                } else {
                    "reachable"
                };
                let detail = service
                    .error
                    .clone()
                    .unwrap_or_else(|| "No answer.".to_string());
                let severity = if service.required { "bad" } else { "warn" };
                found.push((subject, check.to_string(), detail, severity));
            } else {
                for check in failed {
                    let detail = service.error.clone().unwrap_or_default();
                    found.push((subject.clone(), check, detail, "bad"));
                }
            }
        }
        if cluster.members_total > 0 {
            if !cluster.quorum {
                found.push((
                    "cluster".to_string(),
                    "quorum".to_string(),
                    format!(
                        "{} of {} members, {} needed.",
                        cluster.members_up, cluster.members_total, cluster.quorum_needed
                    ),
                    "bad",
                ));
            }
            for member in cluster.members.iter().filter(|member| !member.connected) {
                found.push((
                    format!("member {}", member.member),
                    "report stream".to_string(),
                    "No gateway can hear this member.".to_string(),
                    "warn",
                ));
            }
        }
        if let Some(position) = position
            && let Some(reason) = &position.halted
            && !found.iter().any(|row| row.0 == "outbox")
        {
            found.push((
                "outbox".to_string(),
                "delivery".to_string(),
                reason.clone(),
                "bad",
            ));
        }

        let now = iso(now_ms());
        let mut rows = Vec::new();
        let mut seen = HashMap::new();
        for (subject, check, detail, severity) in found {
            let key = (subject.clone(), check.clone());
            let since = match walker.conditions.get(&key) {
                Some(since) => since.clone(),
                None => {
                    monitor.note(
                        severity,
                        subject.clone(),
                        format!("{check} failed: {detail}"),
                        None,
                    );
                    now.clone()
                }
            };
            seen.insert(key, since.clone());
            rows.push(Condition {
                subject,
                check,
                detail,
                severity,
                since,
            });
        }
        for (subject, check) in walker.conditions.keys() {
            if !seen.contains_key(&(subject.clone(), check.clone())) {
                monitor.note("info", subject.clone(), format!("{check} recovered"), None);
            }
        }
        walker.conditions = seen;
        rows
    }

    async fn read(&self, walker: &mut Walker) -> Value {
        let (services, answers, position, head) = tokio::join!(
            self.up.status(),
            self.up.cluster_answers(),
            self.up.position(),
            self.up.tape_head(),
        );
        let cluster = merge_cluster(&answers);
        let position = position.ok();
        let head = head.ok();

        let term = (cluster.term, cluster.leader);
        if cluster.term >= 0 && walker.term != Some(term) {
            if walker.term.is_some()
                && let Some(leader) = cluster.leader
            {
                self.note(
                    "warn",
                    "cluster".to_string(),
                    format!("Leader is member {leader}, term {}.", cluster.term),
                    None,
                );
            }
            walker.term = Some(term);
        }

        let tape_head = [
            Some(cluster.head_exec_id),
            position.as_ref().map(|position| position.durable_exec_id),
            head.as_ref().and_then(|head| head.exec_id),
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(0);
        let outbox = walker.outbox.read(
            "outbox",
            position.as_ref().map(|position| position.durable_exec_id),
            tape_head,
        );
        let reporter = walker.reporter.read(
            "trade-reporter",
            head.as_ref().and_then(|head| head.exec_id),
            tape_head,
        );
        let mut market_data = self.market_data(head.as_ref());
        let feed = Consumer {
            name: "market-data",
            state: if market_data.ready {
                "caught_up"
            } else {
                "stalled"
            },
            behind: None,
            exec_id: None,
            last_seen: market_data.last_seen.clone(),
        };

        let gateways: Vec<&ServiceStatus> = services
            .iter()
            .filter(|service| service.name == "gateway")
            .collect();
        let engine = Self::engine(&cluster, &gateways);
        // Tickers only exist once trades do, so a feed that's merely quiet isn't a problem.
        market_data.required = true;
        let reporter_service = self.trade_reporter(&reporter, head.as_ref());
        let mut all = vec![engine];
        let (mut gateways, others): (Vec<_>, Vec<_>) = services
            .into_iter()
            .partition(|service| service.name == "gateway");
        all.append(&mut gateways);
        let mut others = others.into_iter();
        // Outbox first, then the other tape consumers, then what they depend on.
        all.extend(others.next());
        all.push(reporter_service);
        all.push(market_data);
        all.extend(others);

        let conditions = Self::conditions(walker, self, &all, &cluster, position.as_ref());
        let latency = self.latency(walker).await;
        let verdict = install_verdict(&all);
        json!({
            "verdict": verdict,
            "can_trade": verdict != "down",
            "version": env!("CARGO_PKG_VERSION"),
            "as_of": iso(now_ms()),
            "services": all,
            "cluster": cluster,
            "consumers": [outbox, reporter, feed],
            "latency": latency,
            "conditions": conditions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::{GatewayCluster, GatewayMember};

    fn answer(leader: Option<usize>, term: i64, members: &[Option<i64>]) -> ClusterAnswer {
        ClusterAnswer {
            standalone: false,
            view: Some(GatewayCluster {
                term,
                leader,
                leader_connected: leader.is_some(),
                leader_changes: 1,
                last_leader_change_ms: None,
                last_exec_id: 9,
                members: members
                    .iter()
                    .enumerate()
                    .map(|(member, position)| GatewayMember {
                        member,
                        connected: position.is_some(),
                        position: *position,
                    })
                    .collect(),
            }),
        }
    }

    #[test]
    fn dates_print_as_questdb_prints_them() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(1_793_983_656_120), "2026-11-06T16:47:36.120Z");
        assert_eq!(iso(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn a_member_is_up_if_any_gateway_hears_it() {
        let merged = merge_cluster(&[
            answer(Some(1), 4, &[Some(100), None, None]),
            answer(Some(1), 4, &[None, Some(120), None]),
        ]);
        assert_eq!(merged.members_up, 2);
        assert!(merged.quorum);
        assert_eq!(merged.members[0].role, "follower");
        assert_eq!(merged.members[0].lag_bytes, Some(20));
        assert_eq!(merged.members[1].role, "leader");
        assert_eq!(merged.members[2].role, "unreachable");
        assert_eq!(merged.members[2].lag_bytes, None);
    }

    #[test]
    fn quorum_needs_a_majority() {
        let merged = merge_cluster(&[answer(None, 4, &[Some(100), None, None])]);
        assert_eq!(merged.quorum_needed, 2);
        assert!(!merged.quorum);
        assert_eq!(merged.leader, None);
    }

    #[test]
    fn no_answers_means_unknown_rather_than_healthy() {
        let merged = merge_cluster(&[ClusterAnswer {
            standalone: false,
            view: None,
        }]);
        assert_eq!(merged.mode, "unknown");
        assert!(!merged.quorum);
        let standalone = merge_cluster(&[ClusterAnswer {
            standalone: true,
            view: None,
        }]);
        assert_eq!(standalone.mode, "standalone");
    }

    #[test]
    fn the_newest_term_names_the_leader() {
        let merged = merge_cluster(&[
            answer(Some(0), 3, &[Some(1), Some(1), Some(1)]),
            answer(Some(2), 4, &[Some(1), Some(1), Some(1)]),
        ]);
        assert_eq!(merged.leader, Some(2));
        assert_eq!(merged.term, 4);
    }

    #[test]
    fn counters_come_from_the_prometheus_text() {
        let text = "# TYPE x histogram\n\
            mmx_gateway_order_latency_seconds_bucket{le=\"0.001\"} 90\n\
            mmx_gateway_order_latency_seconds_bucket{le=\"0.01\"} 100\n\
            mmx_gateway_order_latency_seconds_bucket{le=\"+Inf\"} 100\n\
            mmx_gateway_order_latency_seconds_count 100\n\
            mmx_gateway_orders_rejected_total{reason=\"invalid\"} 3\n\
            mmx_gateway_halted_rejects_total 2\n";
        let counters = GatewayCounters::parse(text);
        assert_eq!(counters.count, 100);
        assert_eq!(counters.refused["invalid"], 3);
        assert_eq!(counters.refused["halted"], 2);
        assert_eq!(quantile_ms(&counters, 0.5), Some(1.0));
        assert_eq!(quantile_ms(&counters, 0.99), Some(10.0));
        assert_eq!(quantile_ms(&GatewayCounters::default(), 0.5), None);
    }

    #[test]
    fn a_restarted_gateway_counts_as_no_change() {
        let older = GatewayCounters {
            buckets: vec![(0.001, 50)],
            count: 50,
            refused: HashMap::new(),
        };
        let newer = GatewayCounters {
            buckets: vec![(0.001, 5)],
            count: 5,
            refused: HashMap::new(),
        };
        assert_eq!(newer.since(&older).count, 0);
    }

    #[test]
    fn a_consumer_behind_the_tape_is_behind_until_it_stops_moving() {
        let mut progress = Progress::default();
        assert_eq!(progress.read("x", Some(10), 10).state, "caught_up");
        assert_eq!(progress.read("x", Some(12), 20).state, "behind");
        progress.moved = Some(Instant::now() - STALLED_AFTER);
        let stalled = progress.read("x", Some(12), 20);
        assert_eq!(stalled.state, "stalled");
        assert_eq!(stalled.behind, Some(8));
        assert_eq!(progress.read("x", None, 20).state, "unknown");
    }

    #[test]
    fn spread_needs_both_sides() {
        assert_eq!(spread_of(Some("10.5"), Some("11")), Some("0.5".to_string()));
        assert_eq!(spread_of(None, Some("11")), None);
    }
}
