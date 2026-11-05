//! `mmx webhook listen`: a stand-in game. It takes webhooks the way a game has to, as described in
//! `services/outbox/DESIGN.md`, so a trade can be watched arriving before any game exists.

use crate::output::{self, line};

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::IntoResponse;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

const TIMESTAMP_HEADER: &str = "x-mmx-timestamp";
const SIGNATURE_HEADER: &str = "x-mmx-signature";
const SIGNATURE_PREFIX: &str = "v1=";
const SIGNATURE_LENGTH: usize = 32;
/// The outbox signs each attempt afresh, so even long retries arrive with a recent timestamp.
const MAX_AGE_SECS: u64 = 300;
/// A refused body is retried forever and holds up every later event, so `WEBHOOK_MAX_EVENTS` set
/// high must not trip a limit here.
const MAX_BODY: usize = 256 * 1024 * 1024;

#[derive(Default, Debug)]
pub struct Tally {
    seen: HashSet<String>,
    pub events: u64,
    pub duplicates: u64,
    pub refused: u64,
}

pub struct Listener {
    secret: Vec<u8>,
    json: bool,
    pub tally: Mutex<Tally>,
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    let pair = |start: usize| u8::from_str_radix(&text[start..start + 2], 16).ok();
    (0..text.len()).step_by(2).map(pair).collect()
}

/// Checks a signature over the exact timestamp and body bytes received, in constant time.
pub fn verify(secret: &[u8], timestamp: &[u8], body: &[u8], signature: &str) -> bool {
    let Some(sent) = signature.strip_prefix(SIGNATURE_PREFIX).and_then(unhex) else {
        return false;
    };
    if sent.len() != SIGNATURE_LENGTH {
        return false;
    }
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(timestamp);
    mac.update(b".");
    mac.update(body);
    mac.verify_slice(&sent).is_ok()
}

/// Checks the sender and the age of a delivery, and returns why it was refused if it was.
fn authenticate(
    secret: &[u8],
    headers: &HeaderMap,
    body: &[u8],
    now: u64,
) -> Result<(), &'static str> {
    let header = |name: &str| headers.get(name).ok_or("missing signature headers");
    let timestamp = header(TIMESTAMP_HEADER)?;
    let signature = header(SIGNATURE_HEADER)?
        .to_str()
        .map_err(|_| "bad signature")?;
    if !verify(secret, timestamp.as_bytes(), body, signature) {
        return Err("bad signature");
    }
    let sent: u64 = timestamp
        .to_str()
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or("bad timestamp")?;
    if sent.abs_diff(now) > MAX_AGE_SECS {
        return Err("timestamp too far from now");
    }
    Ok(())
}

fn field<'a>(event: &'a Value, name: &str) -> &'a str {
    event.get(name).and_then(Value::as_str).unwrap_or("-")
}

fn describe(event: &Value) -> String {
    let flag = event
        .get("terminal")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let kind = field(event, "type");
    let what = match kind {
        "fill" => format!(
            "{} {} @ {}",
            field(event, "side"),
            field(event, "last_qty"),
            field(event, "last_px")
        ),
        "cancel_rejected" => format!(
            "for {} reason={}",
            field(event, "orig_client_order_id"),
            field(event, "reason")
        ),
        "rejected" => format!("{} reason={}", field(event, "side"), field(event, "reason")),
        _ => field(event, "side").to_string(),
    };
    format!(
        "{kind:<15} {:<6} {what} order={} release={} terminal={flag} fill_id={}",
        field(event, "symbol"),
        field(event, "client_order_id"),
        field(event, "release_qty"),
        field(event, "fill_id"),
    )
}

impl Listener {
    pub fn new(secret: Vec<u8>, json: bool) -> Self {
        Self {
            secret,
            json,
            tally: Mutex::default(),
        }
    }

    fn report(&self, event: &Value, tally: &Tally, duplicate: bool) {
        let text = if self.json {
            json!({ "duplicate": duplicate, "events": tally.events, "duplicates": tally.duplicates, "event": event }).to_string()
        } else {
            let label = if duplicate { "duplicate" } else { "webhook" };
            format!(
                "{label:<9} {} [events={} duplicates={}]",
                describe(event),
                tally.events,
                tally.duplicates
            )
        };
        line(&text);
    }

    /// Dedupes per event and not per delivery, because batch boundaries can change between sends.
    fn take(&self, events: &[Value]) {
        let mut tally = self.tally.lock().unwrap_or_else(PoisonError::into_inner);
        for event in events {
            let fresh = tally.seen.insert(field(event, "fill_id").to_string());
            tally.events += u64::from(fresh);
            tally.duplicates += u64::from(!fresh);
            self.report(event, &tally, !fresh);
        }
    }

    fn refuse(&self, status: StatusCode, reason: &str) -> (StatusCode, String) {
        self.tally
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .refused += 1;
        output::note(&format!("refused a delivery: {reason}"));
        (status, reason.to_string())
    }

    fn summary(&self) -> Value {
        let tally = self.tally.lock().unwrap_or_else(PoisonError::into_inner);
        json!({ "events": tally.events, "duplicates": tally.duplicates, "refused": tally.refused })
    }
}

async fn receive(
    State(listener): State<Arc<Listener>>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if method != Method::POST {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            "only POST is supported".to_string(),
        );
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    // Verified over the raw bytes before anything parses them.
    if let Err(reason) = authenticate(&listener.secret, &headers, &body, now) {
        return listener.refuse(StatusCode::UNAUTHORIZED, reason);
    }
    let parsed: Result<Value, _> = serde_json::from_slice(&body);
    let Some(events) = parsed
        .ok()
        .and_then(|delivery| delivery.get("events")?.as_array().cloned())
    else {
        return listener.refuse(StatusCode::BAD_REQUEST, "body has no events list");
    };
    listener.take(&events);
    (StatusCode::OK, "ok".to_string())
}

/// `WEBHOOK_URL` can have any path, so every path is the webhook.
pub fn router(listener: Arc<Listener>) -> Router {
    Router::new()
        .fallback(receive)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(listener)
}

pub async fn listen(bind: IpAddr, port: u16, secret: &str, json: bool) -> Result<(), String> {
    if secret.is_empty() {
        return Err("the webhook secret can't be empty".to_string());
    }
    let address = SocketAddr::new(bind, port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|err| format!("can't bind {address}: {err}"))?;
    output::note(&format!(
        "listening for webhooks on {}",
        listener.local_addr().map_err(|err| err.to_string())?
    ));

    let state = Arc::new(Listener::new(secret.as_bytes().to_vec(), json));
    let stopping = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(stopping)
        .await
        .map_err(|err| err.to_string())?;

    let summary = state.summary();
    output::note(&format!("stopped: {summary}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // From Python's `hmac`, separately from this code, over the format in `webhook::signature`:
    // hmac.new(b"secret", b"1767225600." + BODY, "sha256").hexdigest()
    const BODY: &[u8] =
        br#"{"events":[{"fill_id":"00112233445566778899aabbccddeeff","type":"fill"}]}"#;
    const SIGNATURE: &str = "v1=e8b81b6c43bab45e07b1799795d3a65fec93e6a49ff3841689ebf565669fb272";

    fn headers(timestamp: &str, signature: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(TIMESTAMP_HEADER, timestamp.parse().unwrap());
        headers.insert(SIGNATURE_HEADER, signature.parse().unwrap());
        headers
    }

    #[test]
    fn accepts_the_outbox_signature() {
        assert!(verify(b"secret", b"1767225600", BODY, SIGNATURE));
        assert_eq!(
            authenticate(
                b"secret",
                &headers("1767225600", SIGNATURE),
                BODY,
                1767225600
            ),
            Ok(())
        );
    }

    #[test]
    fn refuses_anything_changed() {
        let timestamp = b"1767225600";
        assert!(!verify(b"other", timestamp, BODY, SIGNATURE));
        assert!(!verify(b"secret", b"1767225601", BODY, SIGNATURE));
        assert!(!verify(
            b"secret",
            timestamp,
            br#"{"events":[]}"#,
            SIGNATURE
        ));
        assert!(!verify(
            b"secret",
            timestamp,
            BODY,
            SIGNATURE.trim_start_matches("v1=")
        ));
        assert!(!verify(b"secret", timestamp, BODY, "v2=00"));
        assert!(!verify(b"secret", timestamp, BODY, "v1=zz"));
        assert!(!verify(b"secret", timestamp, BODY, "v1=é"));
    }

    #[test]
    fn refuses_old_and_future_timestamps() {
        let sent = headers("1767225600", SIGNATURE);
        assert!(authenticate(b"secret", &sent, BODY, 1767225600 + MAX_AGE_SECS).is_ok());
        assert!(authenticate(b"secret", &sent, BODY, 1767225600 + MAX_AGE_SECS + 1).is_err());
        assert!(authenticate(b"secret", &sent, BODY, 1767225600 - MAX_AGE_SECS - 1).is_err());
    }

    #[test]
    fn dedupes_on_fill_id_across_deliveries() {
        let listener = Listener::new(b"secret".to_vec(), true);
        let event = |id: &str| json!({ "fill_id": id, "type": "fill" });
        listener.take(&[event("a"), event("b")]);
        listener.take(&[event("b"), event("c"), event("c")]);
        assert_eq!(
            listener.summary(),
            json!({ "events": 3, "duplicates": 2, "refused": 0 })
        );
    }
}
