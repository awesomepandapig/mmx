//! Routes messages from the single NATS connection to the sockets that asked for them.

use crate::protocol::{ticker_events, valid_id};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};

use tokio::sync::{Notify, mpsc};
use tokio::time::{Duration, interval};

use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Topic {
    Level2(String),
    Ticker(String),
    Candles(String),
    Player(String),
}

/// Events are serialized once here and shared, so fan-out doesn't cost a serialization per socket.
#[derive(Debug, Clone)]
pub enum Delivery {
    Market {
        channel: &'static str,
        events: Arc<str>,
    },
    Order {
        player: Arc<str>,
        product: Arc<str>,
        events: Arc<str>,
    },
    /// A complete frame, outside the sequenced envelope.
    Notice(Arc<str>),
}

struct Subscriber {
    tx: mpsc::Sender<Delivery>,
    overflow: Arc<Notify>,
}

impl Subscriber {
    fn send(&self, delivery: Delivery) {
        // A closed queue means the socket is going away, and it removes itself.
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tx.try_send(delivery) {
            self.overflow.notify_one();
        }
    }
}

#[derive(Default)]
struct Routes {
    connections: HashMap<u64, Subscriber>,
    topics: HashMap<Topic, HashSet<u64>>,
}

impl Routes {
    fn remove(&mut self, id: u64, topic: &Topic) {
        if let Some(ids) = self.topics.get_mut(topic) {
            ids.remove(&id);
            if ids.is_empty() {
                self.topics.remove(topic);
            }
        }
    }
}

pub struct Registration {
    pub id: u64,
    pub rx: mpsc::Receiver<Delivery>,
    pub overflow: Arc<Notify>,
}

/// The newest ticker per product. Updates land in `pending` and readers only see `published`, so
/// sockets never contend with the feed.
#[derive(Default)]
pub struct TickerCache {
    pending: Mutex<HashMap<String, Arc<str>>>,
    published: RwLock<HashMap<String, Arc<str>>>,
}

impl TickerCache {
    fn record(&self, product: &str, ticker: &str) {
        self.pending
            .lock()
            .insert(product.to_string(), ticker.into());
    }

    pub fn refresh(&self) {
        let pending = std::mem::take(&mut *self.pending.lock());
        if !pending.is_empty() {
            self.published.write().extend(pending);
        }
    }

    pub fn get<'a>(&self, products: impl Iterator<Item = &'a String>) -> Vec<Arc<str>> {
        let published = self.published.read();
        products
            .filter_map(|product| published.get(product).cloned())
            .collect()
    }
}

#[derive(Default)]
pub struct Hub {
    next_id: AtomicU64,
    routes: RwLock<Routes>,
    pub tickers: TickerCache,
}

impl Hub {
    pub fn register(&self, queue_capacity: usize) -> Registration {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(queue_capacity);
        let overflow = Arc::new(Notify::new());
        let subscriber = Subscriber {
            tx,
            overflow: overflow.clone(),
        };
        self.routes.write().connections.insert(id, subscriber);
        Registration { id, rx, overflow }
    }

    pub fn deregister<'a>(&self, id: u64, topics: impl Iterator<Item = &'a Topic>) {
        let mut routes = self.routes.write();
        routes.connections.remove(&id);
        for topic in topics {
            routes.remove(id, topic);
        }
    }

    pub fn subscribe(&self, id: u64, topic: Topic) {
        self.routes
            .write()
            .topics
            .entry(topic)
            .or_default()
            .insert(id);
    }

    pub fn unsubscribe(&self, id: u64, topic: &Topic) {
        self.routes.write().remove(id, topic);
    }

    fn deliver(&self, topic: &Topic, delivery: Delivery) {
        let routes = self.routes.read();
        if let Some(ids) = routes.topics.get(topic) {
            for subscriber in ids.iter().filter_map(|id| routes.connections.get(id)) {
                subscriber.send(delivery.clone());
            }
        }
    }

    /// Tells every socket about the feed, since a gap would otherwise look like a quiet market.
    pub fn broadcast(&self, status: &str) {
        let frame = serde_json::json!({ "type": "status", "market_data": status }).to_string();
        let notice = Delivery::Notice(frame.into());
        for subscriber in self.routes.read().connections.values() {
            subscriber.send(notice.clone());
        }
    }

    /// Routes one NATS message. Anything malformed is logged and dropped.
    pub fn publish(&self, subject: &str, payload: &[u8]) {
        let Some(body) = json_object(payload) else {
            warn!("Dropping NATS message with a bad payload on {}", subject);
            return;
        };

        let order = subject
            .strip_prefix("orders.")
            .and_then(|rest| rest.split_once('.'))
            .filter(|(player, product)| valid_id(player) && valid_id(product));
        if let Some((player, product)) = order {
            return self.publish_order(player, product, body);
        }

        // `valid_id` makes the product safe to write into JSON unescaped.
        match subject.split_once('.') {
            Some(("l2", product)) if valid_id(product) => {
                let events = format!(
                    r#"[{{"type":"update","product_id":"{}","updates":[{}]}}]"#,
                    product, body
                );
                self.publish_market(Topic::Level2(product.to_string()), "l2_data", events);
            }
            Some(("ticker", product)) if valid_id(product) => {
                self.tickers.record(product, body);
                let events = ticker_events("update", &[body]);
                self.publish_market(Topic::Ticker(product.to_string()), "ticker", events);
            }
            Some(("candles", product)) if valid_id(product) => {
                let events = format!(
                    r#"[{{"type":"update","product_id":"{}","candles":[{}]}}]"#,
                    product, body
                );
                self.publish_market(Topic::Candles(product.to_string()), "candles", events);
            }
            _ => warn!("Dropping NATS message on unexpected subject {}", subject),
        }
    }

    fn publish_market(&self, topic: Topic, channel: &'static str, events: String) {
        let events = events.into();
        self.deliver(&topic, Delivery::Market { channel, events });
    }

    fn publish_order(&self, player: &str, product: &str, body: &str) {
        let events = format!(
            r#"[{{"type":"update","player_id":"{}","product_id":"{}","updates":[{}]}}]"#,
            player, product, body
        );
        self.deliver(
            &Topic::Player(player.to_string()),
            Delivery::Order {
                player: player.into(),
                product: product.into(),
                events: events.into(),
            },
        );
    }

    #[cfg(test)]
    pub fn connection_count(&self) -> usize {
        self.routes.read().connections.len()
    }

    #[cfg(test)]
    pub fn topic_count(&self) -> usize {
        self.routes.read().topics.len()
    }
}

fn json_object(payload: &[u8]) -> Option<&str> {
    let body = std::str::from_utf8(payload).ok()?;
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value.is_object().then_some(body)
}

pub async fn refresh_tickers(hub: Arc<Hub>, period: Duration) {
    let mut ticker = interval(period);
    loop {
        ticker.tick().await;
        hub.tickers.refresh();
    }
}
