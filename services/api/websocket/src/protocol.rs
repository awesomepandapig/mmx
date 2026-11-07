use chrono::{SecondsFormat, Utc};
use serde::Deserialize;

const MAX_ID_LEN: usize = 64;

#[derive(Deserialize)]
struct RawRequest {
    #[serde(rename = "type")]
    kind: String,
    channel: String,
    #[serde(default)]
    symbols: Vec<String>,
    jwt: Option<String>,
    player_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Subscribe,
    Unsubscribe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Heartbeats,
    Level2,
    Ticker,
    TickerBatch,
    Candles,
    User,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub action: Action,
    pub channel: Channel,
    pub symbols: Vec<String>,
    /// The token that identifies a player when subscribing to `user`.
    pub jwt: Option<String>,
    /// Which player to drop when unsubscribing from `user`. It can only remove this socket's own
    /// subscriptions, so it needs no token.
    pub player_id: Option<String>,
}

/// Symbols and player IDs are NATS subject tokens and are written into JSON unescaped, so they
/// can't contain `.`, `*`, `>` or quotes.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn parse_request(text: &str) -> Result<Request, &'static str> {
    let raw: RawRequest = serde_json::from_str(text).map_err(|_| "invalid message")?;

    let action = match raw.kind.as_str() {
        "subscribe" => Action::Subscribe,
        "unsubscribe" => Action::Unsubscribe,
        _ => return Err("unknown message type"),
    };
    let channel = match raw.channel.as_str() {
        "heartbeats" => Channel::Heartbeats,
        "level2" => Channel::Level2,
        "ticker" => Channel::Ticker,
        "ticker_batch" => Channel::TickerBatch,
        "candles" => Channel::Candles,
        "user" => Channel::User,
        _ => return Err("unknown channel"),
    };

    if !raw.symbols.iter().all(|id| valid_id(id)) {
        return Err("invalid symbol");
    }
    let needs_symbols = !matches!(channel, Channel::Heartbeats | Channel::User);
    if needs_symbols && raw.symbols.is_empty() {
        return Err("symbols required");
    }

    Ok(Request {
        action,
        channel,
        symbols: raw.symbols,
        jwt: raw.jwt,
        player_id: raw.player_id,
    })
}

pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// `events` must already be a JSON array.
pub fn envelope(channel: &str, sequence_num: u64, events: &str) -> String {
    format!(
        r#"{{"channel":"{}","client_id":"","timestamp":"{}","sequence_num":{},"events":{}}}"#,
        channel,
        now_rfc3339(),
        sequence_num,
        events
    )
}

pub fn error_frame(message: &str) -> String {
    serde_json::json!({ "type": "error", "message": message }).to_string()
}

/// `tickers` must already be JSON objects.
pub fn ticker_events<S: AsRef<str>>(kind: &str, tickers: &[S]) -> String {
    let tickers: Vec<&str> = tickers.iter().map(AsRef::as_ref).collect();
    format!(
        r#"[{{"type":"{}","tickers":[{}]}}]"#,
        kind,
        tickers.join(",")
    )
}
