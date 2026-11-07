//! `mmx watch`: follows `/admin/v1/events` and prints a line per event.

use crate::output::{self, line};
use crate::upstream::{describe, http_client};

use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::time::sleep;

const RECONNECT_DELAY: Duration = Duration::from_secs(1);

fn text<'a>(value: &'a Value, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or("-")
}

/// One line for an event. The same shapes come back from `/orders`, `/trades` and `/webhooks`.
pub fn render(kind: &str, data: &Value) -> String {
    match kind {
        "order" => format!(
            "order   {} {:<6} {:<4} {}/{} {} @ {} filled={} player={} id={}",
            text(data, "time"),
            text(data, "symbol"),
            text(data, "side"),
            text(data, "exec_type"),
            text(data, "status"),
            text(data, "size"),
            data.get("price")
                .and_then(Value::as_str)
                .unwrap_or("market"),
            text(data, "filled_size"),
            text(data, "player"),
            text(data, "client_order_id"),
        ),
        "trade" => format!(
            "trade   {} {:<6} {:<4} {} @ {} match={} player={} id={}",
            text(data, "time"),
            text(data, "symbol"),
            text(data, "side"),
            text(data, "size"),
            text(data, "price"),
            data.get("match_id").unwrap_or(&Value::Null),
            text(data, "player"),
            text(data, "client_order_id"),
        ),
        "webhook" => format!(
            "webhook {} delivered={} backlog={} failures={}{}",
            text(data, "state"),
            data.get("delivered_position").unwrap_or(&Value::Null),
            data.get("backlog").unwrap_or(&Value::Null),
            data.get("consecutive_failures").unwrap_or(&Value::Null),
            data.get("halted")
                .and_then(Value::as_str)
                .map(|reason| format!(" halted: {reason}"))
                .unwrap_or_default(),
        ),
        "market" => {
            let market = data.get("market").unwrap_or(&Value::Null);
            let flag = |name: &str| market.get(name).and_then(Value::as_bool).unwrap_or(false);
            format!(
                "market  {:<6} enabled={} halted={} tick={} on {}",
                text(market, "symbol"),
                flag("enabled"),
                flag("halted"),
                text(market, "tick_size"),
                text(data, "gateway"),
            )
        }
        "market_removed" => format!(
            "market  {:<6} removed on {}",
            text(data, "symbol"),
            text(data, "gateway")
        ),
        "service" => {
            let flag = |name: &str| data.get(name).and_then(Value::as_bool).unwrap_or(false);
            let state = if flag("ready") {
                "ready"
            } else if flag("reachable") {
                "not ready"
            } else {
                "unreachable"
            };
            format!(
                "service {} {state} {}",
                text(data, "name"),
                text(data, "target")
            )
        }
        "sample" => format!(
            "sample  {} sent {} of {}",
            text(data, "table"),
            data.get("sent").unwrap_or(&Value::Null),
            data.get("seen").unwrap_or(&Value::Null),
        ),
        "source_failed" => format!(
            "source  {} failing: {}",
            text(data, "source"),
            text(data, "error")
        ),
        "source_recovered" => format!("source  {} recovered", text(data, "source")),
        other => format!("{other} {data}"),
    }
}

/// Takes every complete event from the buffer, and leaves a partial one for the next chunk. Bytes,
/// not text, because a chunk can end inside a character.
pub fn take_events(buffer: &mut Vec<u8>) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    while let Some(end) = buffer.windows(2).position(|pair| pair == b"\n\n") {
        let frame: Vec<u8> = buffer.drain(..end + 2).collect();
        let frame = String::from_utf8_lossy(&frame);
        let (mut kind, mut data) = ("message", String::new());
        for field in frame.lines() {
            if let Some(value) = field.strip_prefix("event:") {
                kind = value.trim();
            } else if let Some(value) = field.strip_prefix("data:") {
                data.push_str(value.trim_start());
            }
        }
        // Keep-alives are comments, so they carry no data.
        if !data.is_empty() {
            events.push((
                kind.to_string(),
                serde_json::from_str(&data).unwrap_or(Value::String(data)),
            ));
        }
    }
    events
}

fn show(kind: &str, data: &Value, json: bool) {
    if json {
        line(&json!({ "kind": kind, "data": data }).to_string());
    } else {
        line(&render(kind, data));
    }
}

/// Runs until interrupted. It reconnects if a working stream drops, but a first connection that
/// fails is an error, so a script finds out at once.
pub async fn watch(url: &str, token: Option<&str>, json: bool) -> Result<(), String> {
    // A limit on the whole request would cut off the stream.
    let http = http_client(None)?;
    let address = format!("{url}/admin/v1/events");
    let mut connected = false;
    loop {
        let mut request = http.get(&address);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        match request.send().await {
            Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                return Err("the server refused the token. Check MMX_ADMIN_TOKEN.".to_string());
            }
            Ok(mut response) if response.status().is_success() => {
                connected = true;
                let mut buffer = Vec::new();
                while let Ok(Some(chunk)) = response.chunk().await {
                    buffer.extend_from_slice(&chunk);
                    for (kind, data) in take_events(&mut buffer) {
                        if kind != "ready" {
                            show(&kind, &data, json);
                        }
                    }
                }
                output::note("connection lost, reconnecting");
            }
            Ok(response) if !connected => {
                return Err(format!("{address}: HTTP {}", response.status()));
            }
            Err(err) if !connected => {
                return Err(format!("can't reach mmx at {url}: {}", describe(&err)));
            }
            _ => {}
        }
        sleep(RECONNECT_DELAY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_survive_being_split_across_chunks() {
        let mut buffer = b"event: order\ndata: {\"a\"".to_vec();
        assert!(take_events(&mut buffer).is_empty());

        buffer.extend_from_slice(b":1}\n\n: keep-alive\n\nevent: trade\ndata: {}\n\nevent: par");
        let events = take_events(&mut buffer);
        assert_eq!(
            events,
            vec![
                ("order".to_string(), json!({ "a": 1 })),
                ("trade".to_string(), json!({}))
            ]
        );
        assert_eq!(buffer, b"event: par");
    }

    #[test]
    fn every_event_kind_has_a_line() {
        let order = json!({ "time": "t", "symbol": "ITEM", "side": "buy", "exec_type": "new", "status": "new", "size": "2", "price": "10.5", "filled_size": "0", "player": "p", "client_order_id": "c" });
        assert!(render("order", &order).contains("buy  new/new 2 @ 10.5 filled=0 player=p id=c"));
        let trade = json!({ "symbol": "ITEM", "side": "sell", "size": "2", "price": "10.5", "match_id": 7 });
        assert!(render("trade", &trade).contains("2 @ 10.5 match=7"));
        let webhook = json!({ "state": "failing", "delivered_position": 1, "backlog": 2, "consecutive_failures": 3 });
        assert_eq!(
            render("webhook", &webhook),
            "webhook failing delivered=1 backlog=2 failures=3"
        );
        let market = json!({ "gateway": "g", "market": { "symbol": "ITEM", "enabled": true, "halted": true, "tick_size": "1" } });
        assert!(render("market", &market).contains("halted=true tick=1 on g"));
    }
}
