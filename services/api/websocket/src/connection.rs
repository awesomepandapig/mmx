use crate::auth::Player;
use crate::config::Config;
use crate::hub::{Delivery, Hub, Registration, Topic};
use crate::protocol::{
    Action, Channel, Request, envelope, error_frame, now_rfc3339, parse_request, ticker_events,
};

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};

use serde_json::json;

use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error, Message};
use tokio_tungstenite::{WebSocketStream, accept_async_with_config};

use tracing::{debug, error, info, warn};

/// Client messages are subscriptions, so anything larger isn't one.
const MAX_CLIENT_MESSAGE: usize = 64 * 1024;

/// The socket is finished, and anything still pending for it can be dropped.
struct Closed;

struct PlayerSubscription {
    /// `None` means every product.
    products: Option<HashSet<String>>,
    expires_at: u64,
}

struct Connection {
    ws: WebSocketStream<TcpStream>,
    hub: Arc<Hub>,
    config: Arc<Config>,
    id: u64,
    rx: mpsc::Receiver<Delivery>,
    overflow: Arc<Notify>,
    topics: BTreeSet<Topic>,
    ticker_batch: BTreeSet<String>,
    players: BTreeMap<String, PlayerSubscription>,
    heartbeats: bool,
    heartbeat_counter: u64,
    sequence_num: u64,
    idle_deadline: Instant,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.hub.deregister(self.id, self.topics.iter());
    }
}

pub async fn accept_connection(
    peer: SocketAddr,
    stream: TcpStream,
    hub: Arc<Hub>,
    config: Arc<Config>,
) {
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_CLIENT_MESSAGE))
        .max_frame_size(Some(MAX_CLIENT_MESSAGE));

    let handshake = accept_async_with_config(stream, Some(ws_config));
    let ws = match timeout(config.handshake_timeout, handshake).await {
        Ok(Ok(ws)) => ws,
        Ok(Err(e)) => {
            debug!("Handshake with {} failed: {}", peer, e);
            return;
        }
        Err(_) => {
            debug!("Handshake with {} timed out", peer);
            return;
        }
    };

    info!("New WebSocket connection: {}", peer);
    Connection::new(ws, hub, config).run().await;
    info!("WebSocket connection closed: {}", peer);
}

impl Connection {
    fn new(ws: WebSocketStream<TcpStream>, hub: Arc<Hub>, config: Arc<Config>) -> Self {
        let Registration { id, rx, overflow } = hub.register(config.queue_capacity);
        let idle_deadline = Instant::now() + config.idle_timeout;
        Self {
            ws,
            hub,
            config,
            id,
            rx,
            overflow,
            topics: BTreeSet::new(),
            ticker_batch: BTreeSet::new(),
            players: BTreeMap::new(),
            heartbeats: false,
            heartbeat_counter: 0,
            sequence_num: 0,
            idle_deadline,
        }
    }

    async fn run(mut self) {
        let start = Instant::now();
        let mut heartbeat = interval_at(
            start + self.config.heartbeat_interval,
            self.config.heartbeat_interval,
        );
        let mut batch = interval_at(
            start + self.config.ticker_batch_interval,
            self.config.ticker_batch_interval,
        );
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        batch.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            let step = tokio::select! {
                _ = self.overflow.notified() => {
                    warn!("Closing slow connection {}", self.id);
                    return self.close(CloseCode::Again, "slow consumer").await;
                }
                _ = sleep_until(self.idle_deadline) => {
                    return self.close(CloseCode::Away, "idle timeout").await;
                }
                frame = self.ws.next() => match frame {
                    Some(Ok(message)) => self.on_message(message).await,
                    Some(Err(e)) => {
                        log_socket_error(&e);
                        Err(Closed)
                    }
                    None => Err(Closed),
                },
                Some(delivery) = self.rx.recv() => self.on_delivery(delivery).await,
                _ = heartbeat.tick() => self.on_heartbeat().await,
                _ = batch.tick() => self.on_ticker_batch().await,
            };

            if step.is_err() {
                return;
            }
        }
    }

    /// Finishes the closing handshake, so that clients waiting for it don't hang.
    async fn close(&mut self, code: CloseCode, reason: &'static str) {
        let frame = CloseFrame {
            code,
            reason: reason.into(),
        };
        let finish = async {
            if self.ws.close(Some(frame)).await.is_ok() {
                while let Some(Ok(_)) = self.ws.next().await {}
            }
        };
        let _ = timeout(self.config.write_timeout, finish).await;
    }

    async fn write(&mut self, text: String) -> Result<(), Closed> {
        match timeout(self.config.write_timeout, self.ws.send(Message::text(text))).await {
            Ok(Ok(())) => {
                self.idle_deadline = Instant::now() + self.config.idle_timeout;
                Ok(())
            }
            Ok(Err(e)) => {
                log_socket_error(&e);
                Err(Closed)
            }
            Err(_) => {
                warn!("Timed out writing to connection {}", self.id);
                Err(Closed)
            }
        }
    }

    async fn write_events(&mut self, channel: &str, events: &str) -> Result<(), Closed> {
        let frame = envelope(channel, self.sequence_num, events);
        self.sequence_num += 1;
        self.write(frame).await
    }

    async fn write_error(&mut self, message: &str) -> Result<(), Closed> {
        self.write(error_frame(message)).await
    }

    async fn on_message(&mut self, message: Message) -> Result<(), Closed> {
        self.idle_deadline = Instant::now() + self.config.idle_timeout;
        match message {
            Message::Text(text) => match parse_request(&text) {
                Ok(request) => match request.action {
                    Action::Subscribe => self.subscribe(request).await,
                    Action::Unsubscribe => self.unsubscribe(request).await,
                },
                Err(reason) => self.write_error(reason).await,
            },
            Message::Binary(_) => self.write_error("invalid message").await,
            // tungstenite answers pings, and finishes a client's close on the next read.
            Message::Ping(_) | Message::Pong(_) | Message::Close(_) | Message::Frame(_) => Ok(()),
        }
    }

    fn has_room_for(&self, added: usize) -> bool {
        let used = self.topics.len() + self.ticker_batch.len() + usize::from(self.heartbeats);
        used + added <= self.config.max_subscriptions
    }

    async fn subscribe(&mut self, request: Request) -> Result<(), Closed> {
        if request.channel == Channel::User {
            return self.subscribe_player(request).await;
        }
        if !self.has_room_for(request.symbols.len()) {
            return self.write_error("too many subscriptions").await;
        }

        match request.channel {
            Channel::Heartbeats => self.heartbeats = true,
            Channel::TickerBatch => self.ticker_batch.extend(request.symbols.iter().cloned()),
            _ => {
                for product in &request.symbols {
                    let topic = market_topic(request.channel, product);
                    self.hub.subscribe(self.id, topic.clone());
                    self.topics.insert(topic);
                }
            }
        }

        self.write_subscriptions().await?;

        // Live updates wait in the queue behind this, so a cached ticker can't follow a newer one.
        match request.channel {
            Channel::Ticker => self.write_snapshot("ticker", &request.symbols).await,
            Channel::TickerBatch => self.write_snapshot("ticker_batch", &request.symbols).await,
            _ => Ok(()),
        }
    }

    async fn subscribe_player(&mut self, request: Request) -> Result<(), Closed> {
        let player = request.jwt.as_deref().and_then(|token| {
            let tokens = self.config.player_tokens.as_ref()?;
            tokens.verify(token, now_secs())
        });
        let Some(Player { id, expires_at }) = player else {
            return self.write_error("unauthorized").await;
        };

        let products: Option<HashSet<String>> =
            (!request.symbols.is_empty()).then(|| request.symbols.into_iter().collect());

        if let Some(existing) = self.players.get_mut(&id) {
            existing.expires_at = expires_at;
            existing.products = match (existing.products.take(), products) {
                (Some(old), Some(new)) => Some(old.into_iter().chain(new).collect()),
                _ => None,
            };
        } else {
            if !self.has_room_for(1) {
                return self.write_error("too many subscriptions").await;
            }
            let topic = Topic::Player(id.clone());
            self.hub.subscribe(self.id, topic.clone());
            self.topics.insert(topic);
            let subscription = PlayerSubscription {
                products,
                expires_at,
            };
            self.players.insert(id, subscription);
        }

        self.write_subscriptions().await
    }

    async fn unsubscribe(&mut self, request: Request) -> Result<(), Closed> {
        match request.channel {
            Channel::Heartbeats => self.heartbeats = false,
            Channel::TickerBatch => {
                for product in &request.symbols {
                    self.ticker_batch.remove(product);
                }
            }
            // Without a `player_id`, this drops every player on the socket.
            Channel::User => {
                let players: Vec<String> = match request.player_id {
                    Some(player) => vec![player],
                    None => self.players.keys().cloned().collect(),
                };
                for player in players {
                    self.drop_player(player);
                }
            }
            _ => {
                for product in &request.symbols {
                    self.drop_topic(&market_topic(request.channel, product));
                }
            }
        }

        self.write_subscriptions().await
    }

    fn drop_topic(&mut self, topic: &Topic) {
        if self.topics.remove(topic) {
            self.hub.unsubscribe(self.id, topic);
        }
    }

    fn drop_player(&mut self, player: String) {
        self.players.remove(&player);
        self.drop_topic(&Topic::Player(player));
    }

    async fn write_subscriptions(&mut self) -> Result<(), Closed> {
        let mut channels: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        if self.heartbeats {
            channels.entry("heartbeats").or_default();
        }
        for topic in &self.topics {
            let (channel, id) = match topic {
                Topic::Level2(id) => ("level2", id),
                Topic::Ticker(id) => ("ticker", id),
                Topic::Candles(id) => ("candles", id),
                Topic::Player(id) => ("user", id),
            };
            channels.entry(channel).or_default().push(id);
        }
        for product in &self.ticker_batch {
            channels.entry("ticker_batch").or_default().push(product);
        }

        let events = json!([{ "subscriptions": channels }]).to_string();
        self.write_events("subscriptions", &events).await
    }

    async fn write_snapshot(&mut self, channel: &str, products: &[String]) -> Result<(), Closed> {
        let tickers = self.hub.tickers.get(products.iter());
        if tickers.is_empty() {
            return Ok(());
        }
        self.write_events(channel, &ticker_events("snapshot", &tickers))
            .await
    }

    async fn on_delivery(&mut self, delivery: Delivery) -> Result<(), Closed> {
        match delivery {
            Delivery::Market { channel, events } => self.write_events(channel, &events).await,
            Delivery::Notice(frame) => self.write(frame.to_string()).await,
            Delivery::Order {
                player,
                product,
                events,
            } => {
                // Checked per frame, so nothing slips out between a token expiring and the sweep.
                let wanted = self.players.get(&*player).is_some_and(|subscription| {
                    subscription.expires_at > now_secs()
                        && subscription
                            .products
                            .as_ref()
                            .is_none_or(|products| products.contains(&*product))
                });
                if wanted {
                    self.write_events("user", &events).await
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn on_heartbeat(&mut self) -> Result<(), Closed> {
        let now = now_secs();
        let expired: Vec<String> = self
            .players
            .iter()
            .filter(|(_, subscription)| subscription.expires_at <= now)
            .map(|(player, _)| player.clone())
            .collect();
        if !expired.is_empty() {
            for player in expired {
                self.drop_player(player);
            }
            self.write_error("token expired").await?;
            self.write_subscriptions().await?;
        }

        if !self.heartbeats {
            return Ok(());
        }
        self.heartbeat_counter += 1;
        let events = json!([{
            "current_time": now_rfc3339(),
            "heartbeat_counter": self.heartbeat_counter,
        }])
        .to_string();
        self.write_events("heartbeats", &events).await
    }

    async fn on_ticker_batch(&mut self) -> Result<(), Closed> {
        let tickers = self.hub.tickers.get(self.ticker_batch.iter());
        if tickers.is_empty() {
            return Ok(());
        }
        self.write_events("ticker_batch", &ticker_events("update", &tickers))
            .await
    }
}

fn market_topic(channel: Channel, product: &str) -> Topic {
    let product = product.to_string();
    match channel {
        Channel::Ticker => Topic::Ticker(product),
        Channel::Candles => Topic::Candles(product),
        _ => Topic::Level2(product),
    }
}

/// A clock set before 1970 reads as far in the future, which expires every token rather than none.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(u64::MAX, |elapsed| elapsed.as_secs())
}

fn log_socket_error(e: &Error) {
    match e {
        Error::ConnectionClosed | Error::AlreadyClosed | Error::Protocol(_) | Error::Utf8(_) => {}
        e => error!("Error processing connection: {}", e),
    }
}
