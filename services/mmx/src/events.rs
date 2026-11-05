//! Activity for `GET /admin/v1/events`. Nothing here pushes changes, so this polls the same sources
//! as the other routes and reports what's different. The first poll reports everything, so a new
//! watcher starts with the current state and the last few orders and trades.

use crate::upstream::{
    Filter, LiveMarket, ORDERS, Position, ServiceStatus, TRADES, Table, Upstreams,
};

use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::{Stream, stream};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::time::{Interval, MissedTickBehavior, interval};

/// Probing every service is the most expensive source, and it changes the least.
const STATUS_EVERY: u64 = 5;
const TAIL_ROWS: usize = 10;

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
}

/// Timestamps come back from QuestDB and go into SQL, so nothing else is let through.
fn is_timestamp(text: &str) -> bool {
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
    pub fn new(up: Arc<Upstreams>) -> Self {
        Self {
            up,
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
        let services = async {
            if status_due {
                up.status().await
            } else {
                Vec::new()
            }
        };
        let (live, orders, trades, position, services) = tokio::join!(
            up.live_markets(),
            read_rows(&up, &ORDERS, &self.orders),
            read_rows(&up, &TRADES, &self.trades),
            up.position(),
            services,
        );

        self.note_services(services, &mut out);
        self.note_markets(live, &mut out);
        self.note_position(position, &mut out);

        // Orders first, so that a fill reads after the order it fills.
        if let Some(rows) = self.outcome("questdb orders", orders, &mut out) {
            self.orders.primed = true;
            out.extend(
                self.orders
                    .advance(rows)
                    .into_iter()
                    .map(|row| activity("order", Value::Object(row))),
            );
        }
        if let Some(rows) = self.outcome("questdb trades", trades, &mut out) {
            self.trades.primed = true;
            out.extend(
                self.trades
                    .advance(rows)
                    .into_iter()
                    .map(|row| activity("trade", Value::Object(row))),
            );
        }
        out
    }
}

/// A new watcher gets the tail of the table rather than all of it, then follows from there.
async fn read_rows(
    up: &Upstreams,
    table: &Table,
    cursor: &Cursor,
) -> Result<Vec<Map<String, Value>>, String> {
    if cursor.primed {
        return up.follow(table, cursor.after.as_deref()).await;
    }
    let mut rows = up.recent(table, &Filter::default(), TAIL_ROWS).await?;
    rows.reverse();
    Ok(rows)
}

type Pending = (Poller, VecDeque<Activity>, Interval);

/// One poller per watcher, which stops when the watcher disconnects and drops the stream.
pub fn sse(
    up: Arc<Upstreams>,
    every: Duration,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut ticks = interval(every);
    // A slow poll just delays the next one, rather than causing a burst to catch up.
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let start: Pending = (Poller::new(up), VecDeque::new(), ticks);

    let events = stream::unfold(start, |(mut poller, mut queue, mut ticks)| async move {
        loop {
            if let Some(item) = queue.pop_front() {
                let event = Event::default()
                    .event(item.kind)
                    .data(item.data.to_string());
                return Some((Ok(event), (poller, queue, ticks)));
            }
            ticks.tick().await;
            queue.extend(poller.poll().await);
        }
    });
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
