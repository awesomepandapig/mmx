//! Activity for `GET /admin/v1/events`. Nothing here pushes changes, so this polls the same sources
//! as the other routes and reports what's different. The first poll reports everything, so a new
//! watcher starts with the current state and the last few orders and trades.
//!
//! The tape is sampled. Each poll reads only the newest rows, up to a fixed rate per table, however
//! fast orders arrive, and skips ahead over the rest, so a watcher never falls behind and the load
//! on QuestDB, this server and the browser doesn't grow with the order rate. A `sample` event says
//! how many rows arrived and how many were sent, so a view can say what it's showing.

use crate::upstream::{
    Filter, LiveMarket, ORDERS, Position, ServiceStatus, TRADES, Table, Upstreams,
};

use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::{Stream, StreamExt, stream};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::{Interval, MissedTickBehavior, interval, sleep};

/// Probing every service is the most expensive source, and it changes the least.
const STATUS_EVERY: u64 = 5;
const TAIL_ROWS: usize = 10;
/// The most rows a watcher is sent per second, per table.
pub const RATE_CAP: usize = 100;
/// How long a stream lasts. A client reconnects by itself, and that checks its session again, so
/// a revoked session can't keep watching.
pub const STREAM_LIFETIME: Duration = Duration::from_secs(15 * 60);

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Activity {
    pub kind: &'static str,
    pub data: Value,
}

fn activity(kind: &'static str, data: Value) -> Activity {
    Activity { kind, data }
}

/// Where a table has been read up to. Rows can share a timestamp, and the query includes the
/// timestamp it resumes from, so rows already sent there are remembered.
#[derive(Default)]
struct Cursor {
    primed: bool,
    after: Option<String>,
    seen: HashSet<i64>,
    /// Whether the last poll found rows, so a view hears when the rate falls to zero.
    active: bool,
}

/// Timestamps come back from QuestDB and go into SQL, so nothing else is let through.
pub fn is_timestamp(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || b"-:.TZ".contains(&byte))
}

impl Cursor {
    /// Keeps the rows that haven't been sent, given oldest first.
    fn advance(&mut self, rows: Vec<Map<String, Value>>) -> Vec<Map<String, Value>> {
        let mut fresh = Vec::new();
        for row in rows {
            let time = row
                .get("time")
                .and_then(Value::as_str)
                .filter(|time| is_timestamp(time));
            let exec_id = row.get("exec_id").and_then(Value::as_i64);
            let (Some(time), Some(exec_id)) = (time, exec_id) else {
                continue;
            };
            if self.after.as_deref() != Some(time) {
                self.after = Some(time.to_string());
                self.seen.clear();
            }
            if self.seen.insert(exec_id) {
                fresh.push(row);
            }
        }
        fresh
    }
}

pub struct Poller {
    up: Arc<Upstreams>,
    filter: Filter,
    /// Leaves out markets, services and webhooks, which a tape view has no use for.
    tape_only: bool,
    /// The most rows to send per poll, per table.
    cap: usize,
    polls: u64,
    orders: Cursor,
    trades: Cursor,
    markets: HashMap<(String, String), LiveMarket>,
    position: Option<Position>,
    services: HashMap<(String, String), ServiceStatus>,
    /// Sources that are failing, so a failure is reported once and not every poll.
    failing: HashMap<String, String>,
}

impl Poller {
    pub fn new(up: Arc<Upstreams>, filter: Filter, tape_only: bool, every: Duration) -> Self {
        let per_poll = RATE_CAP as u128 * every.as_millis() / 1000;
        Self {
            up,
            filter,
            tape_only,
            cap: usize::try_from(per_poll)
                .unwrap_or(RATE_CAP)
                .clamp(1, RATE_CAP),
            polls: 0,
            orders: Cursor::default(),
            trades: Cursor::default(),
            markets: HashMap::new(),
            position: None,
            services: HashMap::new(),
            failing: HashMap::new(),
        }
    }

    fn outcome<T>(
        &mut self,
        source: &str,
        result: Result<T, String>,
        out: &mut Vec<Activity>,
    ) -> Option<T> {
        match result {
            Ok(value) => {
                if self.failing.remove(source).is_some() {
                    out.push(activity("source_recovered", json!({ "source": source })));
                }
                Some(value)
            }
            Err(error) => {
                if self.failing.get(source) != Some(&error) {
                    out.push(activity(
                        "source_failed",
                        json!({ "source": source, "error": error }),
                    ));
                    self.failing.insert(source.to_string(), error);
                }
                None
            }
        }
    }

    fn note_markets(
        &mut self,
        live: Vec<(String, Result<Vec<LiveMarket>, String>)>,
        out: &mut Vec<Activity>,
    ) {
        for (gateway, markets) in live {
            let Some(markets) = self.outcome(&format!("gateway {gateway}"), markets, out) else {
                continue;
            };
            let current: HashSet<String> =
                markets.iter().map(|market| market.symbol.clone()).collect();
            let gone: Vec<String> = self
                .markets
                .keys()
                .filter(|(owner, symbol)| *owner == gateway && !current.contains(symbol))
                .map(|(_, symbol)| symbol.clone())
                .collect();
            for symbol in gone {
                self.markets.remove(&(gateway.clone(), symbol.clone()));
                out.push(activity(
                    "market_removed",
                    json!({ "gateway": gateway, "symbol": symbol }),
                ));
            }
            for market in markets {
                let key = (gateway.clone(), market.symbol.clone());
                if self.markets.get(&key) != Some(&market) {
                    out.push(activity(
                        "market",
                        json!({ "gateway": gateway, "market": market }),
                    ));
                    self.markets.insert(key, market);
                }
            }
        }
    }

    fn note_position(&mut self, position: Result<Position, String>, out: &mut Vec<Activity>) {
        let Some(position) = self.outcome("outbox", position, out) else {
            return;
        };
        // Durable position moves on every report. Only delivery matters here.
        let key = |p: &Position| {
            (
                p.delivered_position,
                p.consecutive_failures,
                p.halted.clone(),
                p.verified,
            )
        };
        if self.position.as_ref().map(key) != Some(key(&position)) {
            out.push(activity("webhook", position.view()));
        }
        self.position = Some(position);
    }

    fn note_services(&mut self, services: Vec<ServiceStatus>, out: &mut Vec<Activity>) {
        for service in services {
            let key = (service.name.clone(), service.target.clone());
            let previous = self.services.get(&key);
            let changed = previous.is_none_or(|old| {
                (old.reachable, old.ready, &old.error)
                    != (service.reachable, service.ready, &service.error)
            });
            if changed {
                out.push(activity("service", json!(service)));
            }
            self.services.insert(key, service);
        }
    }

    pub async fn poll(&mut self) -> Vec<Activity> {
        let mut out = Vec::new();
        let status_due = self.polls.is_multiple_of(STATUS_EVERY);
        self.polls += 1;

        let up = self.up.clone();
        let tape_only = self.tape_only;
        let services = async {
            if status_due && !tape_only {
                up.status().await
            } else {
                Vec::new()
            }
        };
        let live = async {
            if tape_only {
                Vec::new()
            } else {
                up.live_markets().await
            }
        };
        let position = async {
            if tape_only {
                None
            } else {
                Some(up.position().await)
            }
        };
        let cap = self.cap;
        let (live, orders, trades, position, services) = tokio::join!(
            live,
            read_rows(&up, &ORDERS, &self.filter, &self.orders, cap),
            read_rows(&up, &TRADES, &self.filter, &self.trades, cap),
            position,
            services,
        );

        self.note_services(services, &mut out);
        self.note_markets(live, &mut out);
        if let Some(position) = position {
            self.note_position(position, &mut out);
        }

        // Orders first, so that a fill reads after the order it fills.
        if let Some(read) = self.outcome("questdb orders", orders, &mut out) {
            take_rows("orders", "order", &mut self.orders, read, &mut out);
        }
        if let Some(read) = self.outcome("questdb trades", trades, &mut out) {
            take_rows("trades", "trade", &mut self.trades, read, &mut out);
        }
        out
    }
}

/// One poll of a table: the rows read, oldest first, and how many matched in all.
struct Read {
    rows: Vec<Map<String, Value>>,
    matched: u64,
}

/// A new watcher gets the tail of the table rather than all of it, then follows from there. If the
/// read comes back full there are more rows than were read, and only then is it worth counting.
async fn read_rows(
    up: &Upstreams,
    table: &Table,
    filter: &Filter,
    cursor: &Cursor,
    cap: usize,
) -> Result<Read, String> {
    let (since, limit) = match (cursor.primed, cursor.after.as_deref()) {
        (true, Some(after)) => (Some(after), cap),
        _ => (None, TAIL_ROWS),
    };
    let mut rows = up.newest(table, filter, since, limit).await?;
    let matched = match since {
        Some(after) if rows.len() >= limit => up.count_since(table, filter, after).await?,
        _ => rows.len() as u64,
    };
    rows.reverse();
    Ok(Read { rows, matched })
}

fn take_rows(
    name: &'static str,
    kind: &'static str,
    cursor: &mut Cursor,
    read: Read,
    out: &mut Vec<Activity>,
) {
    let first = !cursor.primed;
    cursor.primed = true;
    let repeats = if first { 0 } else { cursor.seen.len() as u64 };
    let fresh = cursor.advance(read.rows);
    let sent = fresh.len() as u64;
    out.extend(
        fresh
            .into_iter()
            .map(|row| activity(kind, Value::Object(row))),
    );
    // The tail a new watcher starts with isn't a measure of anything.
    let seen = read.matched.saturating_sub(repeats).max(sent);
    if !first && (seen > 0 || cursor.active) {
        out.push(activity(
            "sample",
            json!({ "table": name, "seen": seen, "sent": sent }),
        ));
    }
    cursor.active = !first && seen > 0;
}

type Pending = (Poller, VecDeque<Activity>, Interval, OwnedSemaphorePermit);

/// One poller per watcher, which stops when the watcher disconnects and drops the stream. A client
/// that reads slowly holds up its own next poll, which then skips ahead, so nothing piles up here.
pub fn sse(
    poller: Poller,
    every: Duration,
    seat: OwnedSemaphorePermit,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut ticks = interval(every);
    // A slow poll just delays the next one, rather than causing a burst to catch up.
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let start: Pending = (poller, VecDeque::new(), ticks, seat);

    let events = stream::unfold(
        start,
        |(mut poller, mut queue, mut ticks, seat)| async move {
            loop {
                if let Some(item) = queue.pop_front() {
                    let event = Event::default()
                        .event(item.kind)
                        .data(item.data.to_string());
                    return Some((Ok(event), (poller, queue, ticks, seat)));
                }
                ticks.tick().await;
                queue.extend(poller.poll().await);
            }
        },
    )
    .take_until(sleep(STREAM_LIFETIME));
    Sse::new(events).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(time: &str, exec_id: i64) -> Map<String, Value> {
        let value = json!({ "time": time, "exec_id": exec_id });
        value.as_object().unwrap().clone()
    }

    #[test]
    fn a_cursor_sends_each_row_once_even_when_timestamps_are_shared() {
        let mut cursor = Cursor::default();
        assert_eq!(
            cursor
                .advance(vec![row("2026-01-01T00:00:00.000001Z", 1)])
                .len(),
            1
        );

        // The same row comes back because the query includes its timestamp, beside a new twin.
        let fresh = cursor.advance(vec![
            row("2026-01-01T00:00:00.000001Z", 1),
            row("2026-01-01T00:00:00.000001Z", 2),
        ]);
        assert_eq!(fresh, vec![row("2026-01-01T00:00:00.000001Z", 2)]);

        let fresh = cursor.advance(vec![
            row("2026-01-01T00:00:00.000001Z", 2),
            row("2026-01-01T00:00:00.000002Z", 3),
        ]);
        assert_eq!(fresh, vec![row("2026-01-01T00:00:00.000002Z", 3)]);
    }

    #[test]
    fn a_cursor_only_keeps_what_looks_like_a_timestamp() {
        let mut cursor = Cursor::default();
        assert!(cursor.advance(vec![row("x' or 1=1 --", 1)]).is_empty());
        assert_eq!(cursor.after, None);
    }
}
