//! Runs the real server on an ephemeral port. NATS is either a fake feed, or for the client's own
//! behaviour, a fake server that speaks just enough of the wire protocol.

use crate::auth::{Player, PlayerTokens};
use crate::config::Config;
use crate::hub::Hub;
use crate::nats::{self, FeedEvent, SUBJECTS, pump};
use crate::protocol::{Action, Channel, parse_request};
use crate::serve;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use futures_util::stream::poll_fn;
use futures_util::{SinkExt, StreamExt};

use serde_json::{Value, json};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const WAIT: Duration = Duration::from_secs(5);
/// Long enough for a frame to cross loopback, when asserting that nothing arrives.
const QUIET: Duration = Duration::from_millis(150);

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn tokens() -> PlayerTokens {
    PlayerTokens::new(SECRET).unwrap()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn player_token(player: &str) -> String {
    token_expiring(player, now_secs() + 3600)
}

fn token_expiring(player: &str, exp: u64) -> String {
    let claims = json!({ "sub": player, "exp": exp }).to_string();
    tokens().sign(r#"{"alg":"HS256","typ":"JWT"}"#, &claims)
}

fn quiet_config() -> Config {
    Config {
        idle_timeout: Duration::from_secs(30),
        heartbeat_interval: Duration::from_secs(30),
        ticker_refresh: Duration::from_secs(3600),
        ticker_batch_interval: Duration::from_secs(30),
        player_tokens: Some(tokens()),
        ..Config::default()
    }
}

struct Server {
    addr: SocketAddr,
    hub: Arc<Hub>,
    feed: mpsc::UnboundedSender<FeedEvent>,
}

impl Server {
    async fn start(config: Config) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hub = Arc::new(Hub::default());

        let (feed, mut rx) = mpsc::unbounded_channel();
        let feed_hub = hub.clone();
        tokio::spawn(async move {
            pump(poll_fn(move |cx| rx.poll_recv(cx)), &feed_hub).await;
        });
        tokio::spawn(serve(listener, hub.clone(), Arc::new(config)));

        Self { addr, hub, feed }
    }

    async fn connect(&self) -> Client {
        let url = format!("ws://{}", self.addr);
        connect_async(url).await.unwrap().0
    }

    fn publish(&self, subject: &str, payload: &str) {
        self.publish_bytes(subject, payload.as_bytes());
    }

    fn publish_bytes(&self, subject: &str, payload: &[u8]) {
        let message = FeedEvent::Message {
            subject: subject.to_string(),
            payload: Bytes::copy_from_slice(payload),
        };
        self.feed.send(message).unwrap();
    }
}

async fn send(client: &mut Client, message: Value) {
    client
        .send(Message::text(message.to_string()))
        .await
        .unwrap();
}

async fn recv(client: &mut Client) -> Value {
    loop {
        let frame = timeout(WAIT, client.next())
            .await
            .expect("timed out waiting for a frame")
            .expect("socket ended")
            .expect("socket failed");
        match frame {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

async fn assert_silent(client: &mut Client) {
    if let Ok(frame) = timeout(QUIET, client.next()).await {
        panic!("expected no frame, got {frame:?}");
    }
}

/// Subscribes and waits for the acknowledgement, which means routing is in place.
async fn subscribe(client: &mut Client, channel: &str, products: &[&str]) {
    let request = json!({ "type": "subscribe", "channel": channel, "product_ids": products });
    send(client, request).await;
    assert_eq!(recv(client).await["channel"], "subscriptions");
}

async fn subscribe_player(client: &mut Client, token: &str, products: &[&str]) -> Value {
    let request =
        json!({ "type": "subscribe", "channel": "user", "product_ids": products, "jwt": token });
    send(client, request).await;
    recv(client).await
}

/// Reads to the end of the socket and returns the server's close frame.
async fn expect_close(client: &mut Client) -> (CloseCode, String) {
    let mut close = None;
    loop {
        match timeout(WAIT, client.next()).await.expect("close timed out") {
            Some(Ok(Message::Close(frame))) => {
                let frame = frame.expect("close frame had no code");
                close = Some((frame.code, frame.reason.to_string()));
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => panic!("close didn't complete cleanly: {e}"),
            None => return close.expect("socket ended without a close frame"),
        }
    }
}

async fn wait_for_clock(secs: u64) {
    while now_secs() < secs {
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "condition never became true");
        sleep(Duration::from_millis(5)).await;
    }
}

#[test]
fn defaults_match_the_roadmap() {
    let config = Config::default();
    assert_eq!(config.idle_timeout, Duration::from_secs(60));
    assert_eq!(config.heartbeat_interval, Duration::from_secs(1));
    assert_eq!(config.ticker_refresh, Duration::from_secs(1));
    assert!(config.player_tokens.is_none());
}

#[test]
fn parses_the_web_client_shape() {
    let request =
        parse_request(r#"{"type":"subscribe","product_ids":["ABC","XYZ"],"channel":"level2"}"#)
            .unwrap();
    assert_eq!(request.action, Action::Subscribe);
    assert_eq!(request.channel, Channel::Level2);
    assert_eq!(request.product_ids, ["ABC", "XYZ"]);
}

#[test]
fn rejects_bad_requests() {
    for (text, reason) in [
        ("", "invalid message"),
        ("not json", "invalid message"),
        ("[]", "invalid message"),
        ("null", "invalid message"),
        (r#"{"type":"subscribe"}"#, "invalid message"),
        (
            r#"{"type":"subscribe","channel":"level2","product_ids":"ABC"}"#,
            "invalid message",
        ),
        (
            r#"{"type":"subscribe","channel":"level2","product_ids":[1]}"#,
            "invalid message",
        ),
        (
            r#"{"type":"nope","channel":"level2"}"#,
            "unknown message type",
        ),
        (
            r#"{"type":"subscribe","channel":"nope"}"#,
            "unknown channel",
        ),
        (
            r#"{"type":"subscribe","channel":"level2"}"#,
            "product_ids required",
        ),
        (
            r#"{"type":"subscribe","channel":"level2","product_ids":[]}"#,
            "product_ids required",
        ),
        (
            r#"{"type":"subscribe","channel":"level2","product_ids":["A.B"]}"#,
            "invalid product_id",
        ),
        (
            r#"{"type":"subscribe","channel":"level2","product_ids":["*"]}"#,
            "invalid product_id",
        ),
        (
            r#"{"type":"subscribe","channel":"level2","product_ids":["a\"b"]}"#,
            "invalid product_id",
        ),
    ] {
        assert_eq!(parse_request(text).unwrap_err(), reason, "{text}");
    }

    let deep = "[".repeat(100_000);
    assert!(parse_request(&deep).is_err());
}

#[test]
fn verifies_player_tokens() {
    let now = 1_000;
    let claims = r#"{"sub":"player-1","exp":2000}"#;
    let header = r#"{"alg":"HS256"}"#;

    assert_eq!(
        tokens().verify(&tokens().sign(header, claims), now),
        Some(Player {
            id: "player-1".to_string(),
            expires_at: 2000
        })
    );

    let other_key = PlayerTokens::new(&[b'x'; 32]).unwrap();
    let rejected = [
        ("wrong key", other_key.sign(header, claims)),
        (
            "expired",
            tokens().sign(header, r#"{"sub":"player-1","exp":1000}"#),
        ),
        ("no exp", tokens().sign(header, r#"{"sub":"player-1"}"#)),
        ("no sub", tokens().sign(header, r#"{"exp":2000}"#)),
        (
            "fractional exp",
            tokens().sign(header, r#"{"sub":"player-1","exp":2000.5}"#),
        ),
        (
            "wildcard player",
            tokens().sign(header, r#"{"sub":"*","exp":2000}"#),
        ),
        (
            "subject escape",
            tokens().sign(header, r#"{"sub":"a.b","exp":2000}"#),
        ),
        ("alg none", tokens().sign(r#"{"alg":"none"}"#, claims)),
        ("alg sha512", tokens().sign(r#"{"alg":"HS512"}"#, claims)),
        ("empty", String::new()),
        ("not a token", "garbage".to_string()),
        ("dots", "..".to_string()),
    ];
    for (name, token) in rejected {
        assert_eq!(tokens().verify(&token, now), None, "{name}");
    }

    // The claims can't be swapped under a valid signature, and it can't be removed.
    let good = tokens().sign(header, claims);
    let forged = tokens().sign(header, r#"{"sub":"player-2","exp":2000}"#);
    let parts: Vec<&str> = good.split('.').collect();
    let forged_parts: Vec<&str> = forged.split('.').collect();
    let swapped = format!("{}.{}.{}", parts[0], forged_parts[1], parts[2]);
    assert_eq!(tokens().verify(&swapped, now), None);
    let unsigned = format!("{}.{}.", parts[0], parts[1]);
    assert_eq!(tokens().verify(&unsigned, now), None);
    assert_eq!(tokens().verify(&format!("{good}.extra"), now), None);
}

#[test]
fn refuses_short_secrets() {
    assert!(PlayerTokens::new(b"").is_none());
    assert!(PlayerTokens::new(&[b'x'; 31]).is_none());
}

#[tokio::test]
async fn sends_only_the_channels_and_products_requested() {
    let server = Server::start(quiet_config()).await;
    let mut wanted = server.connect().await;
    let mut other_product = server.connect().await;
    let mut other_channel = server.connect().await;
    let mut no_subscription = server.connect().await;

    subscribe(&mut wanted, "level2", &["ABC"]).await;
    subscribe(&mut other_product, "level2", &["XYZ"]).await;
    subscribe(&mut other_channel, "ticker", &["ABC"]).await;

    server.publish("l2.ABC", r#"{"side":"buy","price_level":"1"}"#);

    let frame = recv(&mut wanted).await;
    assert_eq!(frame["channel"], "l2_data");
    assert_eq!(frame["client_id"], "");
    assert_eq!(frame["sequence_num"], 1);
    assert!(frame["timestamp"].is_string());
    assert_eq!(
        frame["events"],
        json!([{
            "type": "update",
            "product_id": "ABC",
            "updates": [{ "side": "buy", "price_level": "1" }],
        }])
    );

    assert_silent(&mut other_product).await;
    assert_silent(&mut other_channel).await;
    assert_silent(&mut no_subscription).await;
}

#[tokio::test]
async fn acknowledges_subscriptions_and_stops_after_unsubscribing() {
    let server = Server::start(quiet_config()).await;
    let mut client = server.connect().await;

    send(
        &mut client,
        json!({ "type": "subscribe", "channel": "level2", "product_ids": ["ABC", "XYZ"] }),
    )
    .await;
    let ack = recv(&mut client).await;
    assert_eq!(ack["sequence_num"], 0);
    assert_eq!(
        ack["events"][0]["subscriptions"],
        json!({ "level2": ["ABC", "XYZ"] })
    );

    // Subscribing twice doesn't deliver twice.
    subscribe(&mut client, "level2", &["ABC"]).await;
    server.publish("l2.ABC", "{}");
    assert_eq!(recv(&mut client).await["channel"], "l2_data");
    assert_silent(&mut client).await;

    send(
        &mut client,
        json!({ "type": "unsubscribe", "channel": "level2", "product_ids": ["ABC"] }),
    )
    .await;
    let ack = recv(&mut client).await;
    assert_eq!(
        ack["events"][0]["subscriptions"],
        json!({ "level2": ["XYZ"] })
    );

    server.publish("l2.ABC", "{}");
    assert_silent(&mut client).await;
    server.publish("l2.XYZ", "{}");
    assert_eq!(recv(&mut client).await["events"][0]["product_id"], "XYZ");
}

#[tokio::test]
async fn sequence_numbers_run_across_channels() {
    let server = Server::start(quiet_config()).await;
    let mut client = server.connect().await;
    subscribe(&mut client, "level2", &["ABC"]).await;
    subscribe(&mut client, "candles", &["ABC"]).await;

    server.publish("l2.ABC", "{}");
    server.publish("candles.ABC", r#"{"open":"1"}"#);
    assert_eq!(recv(&mut client).await["sequence_num"], 2);

    let candle = recv(&mut client).await;
    assert_eq!(candle["sequence_num"], 3);
    assert_eq!(candle["channel"], "candles");
    assert_eq!(candle["events"][0]["candles"], json!([{ "open": "1" }]));
}

#[tokio::test]
async fn heartbeats_count_up_and_keep_a_quiet_socket_open() {
    let server = Server::start(Config {
        idle_timeout: Duration::from_millis(300),
        heartbeat_interval: Duration::from_millis(50),
        ..quiet_config()
    })
    .await;
    let mut client = server.connect().await;

    send(
        &mut client,
        json!({ "type": "subscribe", "channel": "heartbeats" }),
    )
    .await;
    assert_eq!(
        recv(&mut client).await["events"][0]["subscriptions"],
        json!({ "heartbeats": [] })
    );

    // This runs several idle timeouts past the point the socket would otherwise close.
    for expected in 1..=20 {
        let frame = recv(&mut client).await;
        assert_eq!(frame["channel"], "heartbeats");
        assert_eq!(frame["events"][0]["heartbeat_counter"], expected);
        assert!(frame["events"][0]["current_time"].is_string());
    }
}

#[tokio::test]
async fn closes_a_socket_with_no_messages() {
    let server = Server::start(Config {
        idle_timeout: Duration::from_millis(100),
        ..quiet_config()
    })
    .await;
    let mut client = server.connect().await;
    let started = Instant::now();

    let (code, reason) = expect_close(&mut client).await;
    assert_eq!(code, CloseCode::Away);
    assert_eq!(reason, "idle timeout");
    assert!(started.elapsed() >= Duration::from_millis(90));
    wait_for(|| server.hub.connection_count() == 0).await;
}

#[tokio::test]
async fn messages_from_the_client_reset_the_idle_timer() {
    let idle_timeout = Duration::from_millis(300);
    let server = Server::start(Config {
        idle_timeout,
        ..quiet_config()
    })
    .await;
    let mut client = server.connect().await;

    // The server sends no messages here, so only these pings can hold the socket open. Reading
    // the pong each time means an early close fails the test.
    let started = Instant::now();
    for _ in 0..6 {
        sleep(Duration::from_millis(100)).await;
        client.send(Message::Ping(Bytes::new())).await.unwrap();
        let reply = timeout(WAIT, client.next()).await.unwrap();
        assert!(matches!(reply, Some(Ok(Message::Pong(_)))), "{reply:?}");
    }
    assert!(started.elapsed() > idle_timeout * 2);

    let (code, _) = expect_close(&mut client).await;
    assert_eq!(code, CloseCode::Away);
}

#[tokio::test]
async fn completes_a_close_from_the_client() {
    let server = Server::start(quiet_config()).await;
    let mut client = server.connect().await;
    subscribe(&mut client, "level2", &["ABC"]).await;
    assert_eq!(server.hub.topic_count(), 1);

    client.close(None).await.unwrap();
    loop {
        match timeout(WAIT, client.next()).await.expect("close hung") {
            None => break,
            Some(Ok(_)) => {}
            Some(Err(e)) => panic!("close didn't complete cleanly: {e}"),
        }
    }

    wait_for(|| server.hub.connection_count() == 0 && server.hub.topic_count() == 0).await;
}

#[tokio::test]
async fn abandoned_sockets_leave_no_routes_behind() {
    let server = Server::start(quiet_config()).await;
    let mut client = server.connect().await;
    subscribe(&mut client, "level2", &["ABC"]).await;
    drop(client);

    wait_for(|| server.hub.connection_count() == 0 && server.hub.topic_count() == 0).await;
    server.publish("l2.ABC", "{}");
}

#[tokio::test]
async fn caches_tickers_until_the_next_refresh() {
    let server = Server::start(quiet_config()).await;
    server.publish("ticker.ABC", r#"{"product_id":"ABC","price":"1"}"#);
    wait_for(|| {
        server.hub.tickers.refresh();
        !server
            .hub
            .tickers
            .get([String::from("ABC")].iter())
            .is_empty()
    })
    .await;

    // A newer ticker isn't visible until the cache refreshes.
    server.publish("ticker.ABC", r#"{"product_id":"ABC","price":"2"}"#);
    sleep(QUIET).await;

    let mut client = server.connect().await;
    subscribe(&mut client, "ticker", &["ABC", "NONE"]).await;
    let snapshot = recv(&mut client).await;
    assert_eq!(snapshot["channel"], "ticker");
    assert_eq!(snapshot["events"][0]["type"], "snapshot");
    assert_eq!(snapshot["events"][0]["tickers"][0]["price"], "1");
    assert_eq!(
        snapshot["events"][0]["tickers"].as_array().unwrap().len(),
        1
    );

    server.hub.tickers.refresh();
    let mut client = server.connect().await;
    subscribe(&mut client, "ticker", &["ABC"]).await;
    let snapshot = recv(&mut client).await;
    assert_eq!(snapshot["events"][0]["tickers"][0]["price"], "2");
}

#[tokio::test]
async fn a_product_with_no_ticker_sends_no_snapshot() {
    let server = Server::start(quiet_config()).await;
    let mut client = server.connect().await;
    subscribe(&mut client, "ticker", &["NONE"]).await;
    assert_silent(&mut client).await;
}

#[tokio::test]
async fn refreshes_the_ticker_cache_on_a_timer() {
    let hub = Arc::new(Hub::default());
    tokio::spawn(crate::hub::refresh_tickers(
        hub.clone(),
        Duration::from_millis(20),
    ));
    hub.publish("ticker.ABC", br#"{"price":"1"}"#);
    let products = [String::from("ABC")];
    wait_for(|| !hub.tickers.get(products.iter()).is_empty()).await;
}

#[tokio::test]
async fn the_ticker_channel_streams_every_update() {
    let server = Server::start(quiet_config()).await;
    let mut client = server.connect().await;
    subscribe(&mut client, "ticker", &["ABC"]).await;

    for price in ["1", "2"] {
        let ticker = json!({ "product_id": "ABC", "price": price }).to_string();
        server.publish("ticker.ABC", &ticker);
        let frame = recv(&mut client).await;
        assert_eq!(frame["channel"], "ticker");
        assert_eq!(frame["events"][0]["type"], "update");
        assert_eq!(frame["events"][0]["tickers"][0]["price"], price);
    }
}

#[tokio::test]
async fn ticker_batch_sends_the_cache_on_its_interval() {
    let server = Server::start(Config {
        ticker_refresh: Duration::from_millis(10),
        ticker_batch_interval: Duration::from_millis(50),
        ..quiet_config()
    })
    .await;
    tokio::spawn(crate::hub::refresh_tickers(
        server.hub.clone(),
        Duration::from_millis(10),
    ));

    let mut client = server.connect().await;
    subscribe(&mut client, "ticker_batch", &["ABC", "XYZ"]).await;
    assert_silent(&mut client).await;

    // Three updates to one product arrive as a single entry, and other products stay out.
    for price in ["1", "2", "3"] {
        let ticker = json!({ "product_id": "ABC", "price": price }).to_string();
        server.publish("ticker.ABC", &ticker);
    }
    server.publish("ticker.OTHER", r#"{"product_id":"OTHER"}"#);

    let frame = recv(&mut client).await;
    assert_eq!(frame["channel"], "ticker_batch");
    assert_eq!(frame["events"][0]["type"], "update");
    assert_eq!(
        frame["events"][0]["tickers"],
        json!([{ "product_id": "ABC", "price": "3" }])
    );
}

#[tokio::test]
async fn bad_client_input_is_answered_and_leaves_the_socket_working() {
    let server = Server::start(quiet_config()).await;
    let mut client = server.connect().await;

    for (frame, reason) in [
        (Message::text("not json"), "invalid message"),
        (Message::text("{}"), "invalid message"),
        (Message::binary(vec![0xff, 0x00]), "invalid message"),
        (
            Message::text(r#"{"type":"subscribe","channel":"nope"}"#),
            "unknown channel",
        ),
    ] {
        client.send(frame).await.unwrap();
        let reply = recv(&mut client).await;
        assert_eq!(reply, json!({ "type": "error", "message": reason }));
    }

    subscribe(&mut client, "level2", &["ABC"]).await;
    server.publish("l2.ABC", "{}");
    assert_eq!(recv(&mut client).await["channel"], "l2_data");
}

#[tokio::test]
async fn oversized_frames_only_cost_that_socket() {
    let server = Server::start(quiet_config()).await;
    let mut greedy = server.connect().await;

    let _ = greedy.send(Message::text("x".repeat(128 * 1024))).await;
    wait_for(|| server.hub.connection_count() == 0).await;

    let mut client = server.connect().await;
    subscribe(&mut client, "level2", &["ABC"]).await;
}

#[tokio::test]
async fn garbage_before_the_handshake_is_harmless() {
    let server = Server::start(quiet_config()).await;

    let mut raw = TcpStream::connect(server.addr).await.unwrap();
    raw.write_all(b"\x00\x01 this is not http\r\n\r\n")
        .await
        .unwrap();
    drop(raw);
    drop(TcpStream::connect(server.addr).await.unwrap());

    let mut client = server.connect().await;
    subscribe(&mut client, "level2", &["ABC"]).await;
}

#[tokio::test]
async fn a_stalled_handshake_is_dropped() {
    let server = Server::start(Config {
        handshake_timeout: Duration::from_millis(50),
        ..quiet_config()
    })
    .await;

    let mut raw = TcpStream::connect(server.addr).await.unwrap();
    let mut buffer = [0; 1];
    let read = timeout(WAIT, raw.read(&mut buffer)).await.unwrap();
    assert!(matches!(read, Ok(0) | Err(_)));
}

#[tokio::test]
async fn bad_nats_messages_are_dropped() {
    let server = Server::start(quiet_config()).await;
    let mut client = server.connect().await;
    subscribe(&mut client, "level2", &["ABC"]).await;

    server.publish("l2.ABC", "not json");
    server.publish("l2.ABC", "[1,2]");
    server.publish("l2.ABC", "");
    server.publish_bytes("l2.ABC", &[0xff, 0xfe]);
    server.publish("l2.", "{}");
    server.publish("l2", "{}");
    server.publish("l2.A.B", "{}");
    server.publish("orders.ABC", "{}");
    server.publish("orders.a.b.c", "{}");
    server.publish("orders..b", "{}");
    server.publish("nothing.ABC", "{}");
    server.publish("ticker.ABC", "not json");

    server.publish("l2.ABC", r#"{"good":true}"#);
    let frame = recv(&mut client).await;
    assert_eq!(frame["events"][0]["updates"], json!([{ "good": true }]));
    assert_eq!(frame["sequence_num"], 1);

    server.hub.tickers.refresh();
    let products = [String::from("ABC")];
    assert!(server.hub.tickers.get(products.iter()).is_empty());
}

#[tokio::test]
async fn a_client_that_falls_behind_is_closed_rather_than_skipped() {
    let server = Server::start(Config {
        queue_capacity: 1,
        ..quiet_config()
    })
    .await;
    let mut client = server.connect().await;
    subscribe(&mut client, "level2", &["ABC"]).await;

    // The feed runs on this runtime, so these all queue before the socket's task can drain any.
    tokio::task::yield_now().await;
    let hub = server.hub.clone();
    for _ in 0..50 {
        hub.publish("l2.ABC", b"{}");
    }

    let (code, reason) = expect_close(&mut client).await;
    assert_eq!(code, CloseCode::Again);
    assert_eq!(reason, "slow consumer");
}

#[tokio::test]
async fn caps_subscriptions_per_socket() {
    let server = Server::start(Config {
        max_subscriptions: 2,
        ..quiet_config()
    })
    .await;
    let mut client = server.connect().await;

    send(
        &mut client,
        json!({ "type": "subscribe", "channel": "level2", "product_ids": ["A", "B", "C"] }),
    )
    .await;
    assert_eq!(recv(&mut client).await["message"], "too many subscriptions");
    assert_eq!(server.hub.topic_count(), 0);

    subscribe(&mut client, "level2", &["A", "B"]).await;
}

#[tokio::test]
async fn the_user_channel_refuses_anything_but_a_valid_token() {
    let server = Server::start(quiet_config()).await;
    let mut client = server.connect().await;

    let other_key = PlayerTokens::new(&[b'x'; 32]).unwrap();
    let claims = json!({ "sub": "player-1", "exp": now_secs() + 3600 }).to_string();
    let requests = [
        json!({ "type": "subscribe", "channel": "user" }),
        json!({ "type": "subscribe", "channel": "user", "player_id": "player-1" }),
        json!({ "type": "subscribe", "channel": "user", "jwt": "" }),
        json!({ "type": "subscribe", "channel": "user", "jwt": "garbage" }),
        json!({ "type": "subscribe", "channel": "user",
                "jwt": other_key.sign(r#"{"alg":"HS256"}"#, &claims) }),
        json!({ "type": "subscribe", "channel": "user",
                "jwt": tokens().sign(r#"{"alg":"none"}"#, &claims) }),
        json!({ "type": "subscribe", "channel": "user",
                "jwt": token_expiring("player-1", now_secs() - 1) }),
    ];
    for request in requests {
        send(&mut client, request).await;
        let reply = recv(&mut client).await;
        assert_eq!(reply, json!({ "type": "error", "message": "unauthorized" }));
    }

    server.publish("orders.player-1.ABC", r#"{"id":"secret"}"#);
    assert_silent(&mut client).await;
    assert_eq!(server.hub.topic_count(), 0);
}

#[tokio::test]
async fn the_user_channel_is_off_without_a_secret() {
    let server = Server::start(Config {
        player_tokens: None,
        ..quiet_config()
    })
    .await;
    let mut client = server.connect().await;

    let reply = subscribe_player(&mut client, &player_token("player-1"), &[]).await;
    assert_eq!(reply["message"], "unauthorized");
}

#[tokio::test]
async fn players_only_see_their_own_orders() {
    let server = Server::start(quiet_config()).await;
    let mut alice = server.connect().await;
    let mut bob = server.connect().await;
    let mut market_only = server.connect().await;

    let ack = subscribe_player(&mut alice, &player_token("alice"), &[]).await;
    assert_eq!(
        ack["events"][0]["subscriptions"],
        json!({ "user": ["alice"] })
    );
    subscribe_player(&mut bob, &player_token("bob"), &[]).await;
    subscribe(&mut market_only, "level2", &["ABC"]).await;

    server.publish("orders.alice.ABC", r#"{"id":"order-1","status":"done"}"#);

    let frame = recv(&mut alice).await;
    assert_eq!(frame["channel"], "user");
    assert_eq!(
        frame["events"],
        json!([{
            "type": "update",
            "player_id": "alice",
            "product_id": "ABC",
            "updates": [{ "id": "order-1", "status": "done" }],
        }])
    );

    assert_silent(&mut bob).await;
    assert_silent(&mut market_only).await;
}

#[tokio::test]
async fn filters_a_players_orders_by_product() {
    let server = Server::start(quiet_config()).await;
    let mut client = server.connect().await;
    subscribe_player(&mut client, &player_token("alice"), &["ABC"]).await;

    server.publish("orders.alice.XYZ", r#"{"id":"hidden"}"#);
    server.publish("orders.alice.ABC", r#"{"id":"shown"}"#);
    let frame = recv(&mut client).await;
    assert_eq!(frame["events"][0]["updates"][0]["id"], "shown");
    assert_eq!(frame["sequence_num"], 1);

    // A second subscription adds products.
    subscribe_player(&mut client, &player_token("alice"), &["XYZ"]).await;
    server.publish("orders.alice.XYZ", r#"{"id":"now shown"}"#);
    let frame = recv(&mut client).await;
    assert_eq!(frame["events"][0]["updates"][0]["id"], "now shown");
}

#[tokio::test]
async fn one_socket_can_carry_several_players() {
    let server = Server::start(quiet_config()).await;
    let mut game_server = server.connect().await;
    subscribe_player(&mut game_server, &player_token("alice"), &[]).await;
    subscribe_player(&mut game_server, &player_token("bob"), &[]).await;

    server.publish("orders.bob.ABC", "{}");
    let frame = recv(&mut game_server).await;
    assert_eq!(frame["events"][0]["player_id"], "bob");

    send(
        &mut game_server,
        json!({ "type": "unsubscribe", "channel": "user", "player_id": "bob" }),
    )
    .await;
    let ack = recv(&mut game_server).await;
    assert_eq!(
        ack["events"][0]["subscriptions"],
        json!({ "user": ["alice"] })
    );

    server.publish("orders.bob.ABC", "{}");
    assert_silent(&mut game_server).await;
    server.publish("orders.alice.ABC", "{}");
    assert_eq!(
        recv(&mut game_server).await["events"][0]["player_id"],
        "alice"
    );

    send(
        &mut game_server,
        json!({ "type": "unsubscribe", "channel": "user" }),
    )
    .await;
    assert_eq!(
        recv(&mut game_server).await["events"][0]["subscriptions"],
        json!({})
    );
    assert_eq!(server.hub.topic_count(), 0);
}

#[tokio::test]
async fn unsubscribing_another_player_does_nothing() {
    let server = Server::start(quiet_config()).await;
    let mut alice = server.connect().await;
    let mut bob = server.connect().await;
    subscribe_player(&mut alice, &player_token("alice"), &[]).await;
    subscribe_player(&mut bob, &player_token("bob"), &[]).await;

    send(
        &mut bob,
        json!({ "type": "unsubscribe", "channel": "user", "player_id": "alice" }),
    )
    .await;
    recv(&mut bob).await;

    server.publish("orders.alice.ABC", "{}");
    assert_eq!(recv(&mut alice).await["channel"], "user");
    assert_silent(&mut bob).await;
}

#[tokio::test]
async fn the_private_feed_ends_when_the_token_expires() {
    let server = Server::start(Config {
        heartbeat_interval: Duration::from_millis(20),
        ..quiet_config()
    })
    .await;
    let mut client = server.connect().await;

    // `exp` has whole-second resolution, so this expires one to two seconds from now.
    let token = token_expiring("alice", now_secs() + 2);
    subscribe_player(&mut client, &token, &[]).await;
    server.publish("orders.alice.ABC", "{}");
    assert_eq!(recv(&mut client).await["channel"], "user");

    let expired = recv(&mut client).await;
    assert_eq!(
        expired,
        json!({ "type": "error", "message": "token expired" })
    );
    let ack = recv(&mut client).await;
    assert_eq!(ack["events"][0]["subscriptions"], json!({}));

    server.publish("orders.alice.ABC", "{}");
    assert_silent(&mut client).await;

    // A fresh token picks the feed back up.
    subscribe_player(&mut client, &player_token("alice"), &[]).await;
    server.publish("orders.alice.ABC", "{}");
    assert_eq!(recv(&mut client).await["channel"], "user");
}

#[tokio::test]
async fn an_expired_token_gets_nothing_before_the_sweep_runs() {
    // The sweep runs on the heartbeat timer, which never fires during this test.
    let server = Server::start(quiet_config()).await;
    let mut client = server.connect().await;

    let token = token_expiring("alice", now_secs() + 2);
    subscribe_player(&mut client, &token, &[]).await;
    wait_for_clock(now_secs() + 2).await;

    server.publish("orders.alice.ABC", "{}");
    assert_silent(&mut client).await;
}

#[tokio::test]
async fn a_bad_nats_url_doesnt_spam_sockets() {
    let server = Server::start(quiet_config()).await;
    let mut client = server.connect().await;
    subscribe(&mut client, "level2", &["ABC"]).await;

    tokio::spawn(nats::run("not a url::".to_string(), server.hub.clone()));
    assert_silent(&mut client).await;
}

#[tokio::test]
async fn feed_status_reaches_every_socket() {
    let server = Server::start(quiet_config()).await;
    let mut client = server.connect().await;

    wait_for(|| server.hub.connection_count() == 1).await;
    server.feed.send(FeedEvent::Status("disconnected")).unwrap();
    assert_eq!(
        recv(&mut client).await,
        json!({ "type": "status", "market_data": "disconnected" })
    );
}

/// Speaks just enough of the NATS protocol for the real client: it answers the handshake, records
/// each connection's subscriptions and sends messages on request.
struct FakeNats {
    url: String,
    connections: mpsc::UnboundedReceiver<FakeNatsConnection>,
}

struct FakeNatsConnection {
    write: tokio::net::tcp::OwnedWriteHalf,
    subscriptions: mpsc::UnboundedReceiver<(String, String)>,
}

impl FakeNats {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("nats://{}", listener.local_addr().unwrap());
        let (tx, connections) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (read, mut write) = stream.into_split();
                write
                    .write_all(br#"INFO {"server_id":"fake","server_name":"fake","version":"2.10.0","go":"go","host":"127.0.0.1","port":4222,"headers":true,"max_payload":1048576,"proto":1}"#)
                    .await
                    .unwrap();
                write.write_all(b"\r\n").await.unwrap();

                let (subscribed, subscriptions) = mpsc::unbounded_channel();
                tokio::spawn(answer_client(read, subscribed));
                let connection = FakeNatsConnection {
                    write,
                    subscriptions,
                };
                if tx.send(connection).is_err() {
                    return;
                }
            }
        });

        Self { url, connections }
    }

    async fn next_connection(&mut self) -> FakeNatsConnection {
        timeout(WAIT, self.connections.recv())
            .await
            .expect("the client never connected")
            .unwrap()
    }
}

/// Pings are answered through a side channel, since the write half belongs to the test.
async fn answer_client(
    read: tokio::net::tcp::OwnedReadHalf,
    subscribed: mpsc::UnboundedSender<(String, String)>,
) {
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut fields = line.split_whitespace();
        if fields.next() == Some("SUB") {
            let subject = fields.next().unwrap().to_string();
            let sid = fields.next_back().unwrap().to_string();
            let _ = subscribed.send((subject, sid));
        } else if line == "PING" {
            let _ = subscribed.send(("PING".to_string(), String::new()));
        }
    }
}

impl FakeNatsConnection {
    /// Answers pings until every subject is subscribed, and returns the sid for each one.
    async fn read_subscriptions(&mut self) -> Vec<(String, String)> {
        let mut seen = Vec::new();
        while seen.len() < SUBJECTS.len() {
            let (subject, sid) = timeout(WAIT, self.subscriptions.recv())
                .await
                .expect("the client never subscribed")
                .unwrap();
            if subject == "PING" {
                self.write.write_all(b"PONG\r\n").await.unwrap();
            } else {
                seen.push((subject, sid));
            }
        }
        seen.sort();
        seen
    }

    async fn publish(&mut self, subject: &str, sid: &str, payload: &str) {
        let frame = format!(
            "MSG {} {} {}\r\n{}\r\n",
            subject,
            sid,
            payload.len(),
            payload
        );
        self.write.write_all(frame.as_bytes()).await.unwrap();
    }
}

fn sid_for<'a>(subscriptions: &'a [(String, String)], subject: &str) -> &'a str {
    let found = subscriptions.iter().find(|(name, _)| name == subject);
    &found.unwrap().1
}

fn sorted_subjects() -> Vec<String> {
    let mut subjects = SUBJECTS.map(String::from).to_vec();
    subjects.sort();
    subjects
}

#[tokio::test]
async fn shares_one_nats_connection_between_sockets() {
    let mut fake = FakeNats::start().await;
    let server = Server::start(quiet_config()).await;
    tokio::spawn(nats::run(fake.url.clone(), server.hub.clone()));

    let mut connection = fake.next_connection().await;
    let subscriptions = connection.read_subscriptions().await;
    let subjects: Vec<String> = subscriptions.iter().map(|(s, _)| s.clone()).collect();
    assert_eq!(subjects, sorted_subjects());

    let mut clients = Vec::new();
    for _ in 0..3 {
        let mut client = server.connect().await;
        subscribe(&mut client, "level2", &["ABC"]).await;
        clients.push(client);
    }

    let sid = sid_for(&subscriptions, "l2.*");
    connection
        .publish("l2.ABC", sid, r#"{"price_level":"1"}"#)
        .await;
    for client in &mut clients {
        let frame = recv(client).await;
        assert_eq!(frame["channel"], "l2_data");
        assert_eq!(frame["events"][0]["product_id"], "ABC");
    }

    // Sockets don't open connections of their own.
    assert!(timeout(QUIET, fake.connections.recv()).await.is_err());
}

#[tokio::test]
async fn survives_losing_nats_and_resubscribes() {
    let mut fake = FakeNats::start().await;
    let server = Server::start(quiet_config()).await;
    tokio::spawn(nats::run(fake.url.clone(), server.hub.clone()));

    let mut connection = fake.next_connection().await;
    let subscriptions = connection.read_subscriptions().await;

    let mut client = server.connect().await;
    subscribe(&mut client, "level2", &["ABC"]).await;
    let sid = sid_for(&subscriptions, "l2.*").to_string();
    connection.publish("l2.ABC", &sid, "{}").await;
    assert_eq!(recv(&mut client).await["channel"], "l2_data");

    drop(connection);
    let disconnected = json!({ "type": "status", "market_data": "disconnected" });
    assert_eq!(recv(&mut client).await, disconnected);

    let mut connection = fake.next_connection().await;
    let subscriptions = connection.read_subscriptions().await;
    let connected = json!({ "type": "status", "market_data": "connected" });
    assert_eq!(recv(&mut client).await, connected);

    let sid = sid_for(&subscriptions, "l2.*");
    connection.publish("l2.ABC", sid, "{}").await;
    assert_eq!(recv(&mut client).await["channel"], "l2_data");
}

#[tokio::test]
async fn serves_clients_with_no_nats_at_all() {
    // Binding and dropping leaves a port that refuses connections.
    let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("nats://{}", unused.local_addr().unwrap());
    drop(unused);

    let server = Server::start(Config {
        heartbeat_interval: Duration::from_millis(20),
        ..quiet_config()
    })
    .await;
    tokio::spawn(nats::run(url, server.hub.clone()));
    tokio::spawn(nats::run("not a url::".to_string(), server.hub.clone()));

    let mut client = server.connect().await;
    subscribe(&mut client, "level2", &["ABC"]).await;
    send(
        &mut client,
        json!({ "type": "subscribe", "channel": "heartbeats" }),
    )
    .await;
    assert_eq!(recv(&mut client).await["channel"], "subscriptions");
    assert_eq!(recv(&mut client).await["channel"], "heartbeats");
}
