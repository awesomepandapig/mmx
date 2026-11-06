//! Live market data for `GET /admin/v1/markets/{symbol}/stream`, so a browser only ever talks to
//! `mmx serve`.
//!
//! `market-data` publishes each level that changes on `l2.<symbol>`, with its new total, and keeps
//! no snapshot anyone can ask for. So the first person to watch a market starts one task, which
//! subscribes, reads the resting book once from the tape, and from then on follows the feed. Levels
//! the feed has touched win over the tape, which is always a little behind. Watchers of a market
//! share that task, and it stops with the last of them.
//!
//! Browsers get whole frames: the top of the book, at most five times a second, and only the newest
//! if they read slowly. So what reaches a browser doesn't depend on how fast the market moves.

use crate::decimal;
use crate::events::STREAM_LIFETIME;
use crate::upstream::Upstreams;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use async_nats::{Client, ConnectOptions, Event as NatsEvent};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, watch};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, sleep};
use tracing::{info, warn};

/// Levels sent per side. Past this a person can't read them, and a browser shouldn't have to.
pub const MAX_LEVELS: usize = 40;
const FRAME_EVERY: Duration = Duration::from_millis(200);
/// The tape lags the feed. Reading it this long after subscribing means anything the snapshot
/// missed arrived on the feed instead.
const SNAPSHOT_DELAY: Duration = Duration::from_millis(500);

type Snapshot = Result<Vec<(String, i64, i64)>, String>;

#[derive(Deserialize)]
struct LevelChange {
    side: String,
    price_level: String,
    new_quantity: String,
    #[serde(default)]
    event_time: String,
}

/// What the book's state is worth right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    Loading,
    Complete,
    /// The tape couldn't be read, so only levels that changed since watching began are here.
    Partial,
}

impl Coverage {
    fn name(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Complete => "complete",
            Self::Partial => "partial",
        }
    }
}

#[derive(Debug)]
pub struct Book {
    bids: BTreeMap<i64, i64>,
    asks: BTreeMap<i64, i64>,
    touched: HashSet<(bool, i64)>,
    pub coverage: Coverage,
    as_of: String,
}

impl Default for Book {
    fn default() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            touched: HashSet::new(),
            coverage: Coverage::Loading,
            as_of: String::new(),
        }
    }
}

impl Book {
    fn side(&mut self, buy: bool) -> &mut BTreeMap<i64, i64> {
        if buy { &mut self.bids } else { &mut self.asks }
    }

    fn set(&mut self, buy: bool, price: i64, size: i64) {
        if size > 0 {
            self.side(buy).insert(price, size);
        } else {
            self.side(buy).remove(&price);
        }
    }

    /// One change from the feed. It carries the level's new total, so nothing accumulates.
    fn apply(&mut self, change: &LevelChange) -> bool {
        let (Ok(price), Ok(size)) = (
            decimal::parse(&change.price_level),
            decimal::parse(&change.new_quantity),
        ) else {
            return false;
        };
        let buy = change.side == "buy";
        self.touched.insert((buy, price));
        self.set(buy, price, size);
        self.as_of.clone_from(&change.event_time);
        true
    }

    /// Lays the tape's reading under what the feed has said. Levels the feed changed are newer, and
    /// a level the tape doesn't list and the feed never touched is gone.
    pub fn merge_snapshot(&mut self, levels: Vec<(String, i64, i64)>) {
        let touched = std::mem::take(&mut self.touched);
        self.bids
            .retain(|price, _| touched.contains(&(true, *price)));
        self.asks
            .retain(|price, _| touched.contains(&(false, *price)));
        for (side, price, size) in levels {
            let buy = side == "buy";
            if !touched.contains(&(buy, price)) {
                self.set(buy, price, size);
            }
        }
        self.coverage = Coverage::Complete;
    }

    /// The top of each side, best price first.
    pub fn frame(&self) -> Value {
        let side = |levels: Box<dyn Iterator<Item = (&i64, &i64)> + '_>| {
            levels
                .take(MAX_LEVELS)
                .map(|(price, size)| [decimal::format(*price), decimal::format(*size)])
                .collect::<Vec<_>>()
        };
        json!({
            "bids": side(Box::new(self.bids.iter().rev())),
            "asks": side(Box::new(self.asks.iter())),
            "bid_levels": self.bids.len(),
            "ask_levels": self.asks.len(),
            "coverage": self.coverage.name(),
            "as_of": self.as_of,
        })
    }
}

/// The newest of everything for one market. A slow reader skips straight to this.
#[derive(Default, Clone, Debug)]
struct Latest {
    book: Option<Arc<str>>,
    ticker: Option<Arc<str>>,
    candle: Option<Arc<str>>,
    failed: Option<Arc<str>>,
}

pub struct Feeds {
    up: Arc<Upstreams>,
    client: OnceCell<Client>,
    markets: Mutex<HashMap<String, Weak<watch::Sender<Latest>>>>,
    /// Bumped when the connection dropped or fell behind, so every book reads the tape again.
    gaps: Arc<AtomicU64>,
}

impl Feeds {
    pub fn new(up: Arc<Upstreams>) -> Arc<Self> {
        Arc::new(Self {
            up,
            client: OnceCell::new(),
            markets: Mutex::default(),
            gaps: Arc::default(),
        })
    }

    /// One connection for the process. It connects in the background and keeps trying, so a NATS
    /// that's down only means an empty feed until it's back.
    async fn client(&self) -> Result<&Client, String> {
        let gaps = self.gaps.clone();
        self.client
            .get_or_try_init(|| async move {
                ConnectOptions::new()
                    .retry_on_initial_connect()
                    .max_reconnects(None)
                    .event_callback(move |event| {
                        let gaps = gaps.clone();
                        async move {
                            if matches!(event, NatsEvent::Connected | NatsEvent::SlowConsumer(_)) {
                                gaps.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    })
                    .connect(self.up.targets.nats.as_str())
                    .await
                    .map_err(|err| format!("nats: {err}"))
            })
            .await
    }

    /// Joins the task for this market, starting it if nobody's watching.
    fn join(self: &Arc<Self>, symbol: &str) -> Arc<watch::Sender<Latest>> {
        let mut markets = self.markets.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(sender) = markets.get(symbol).and_then(Weak::upgrade) {
            return sender;
        }
        markets.retain(|_, sender| sender.strong_count() > 0);
        let sender = Arc::new(watch::channel(Latest::default()).0);
        markets.insert(symbol.to_string(), Arc::downgrade(&sender));
        tokio::spawn(follow(
            self.clone(),
            symbol.to_string(),
            Arc::downgrade(&sender),
        ));
        sender
    }
}

fn start_snapshot(feeds: &Arc<Feeds>, symbol: &str, delay: Duration) -> JoinHandle<Snapshot> {
    let (up, symbol) = (feeds.up.clone(), symbol.to_string());
    tokio::spawn(async move {
        sleep(delay).await;
        up.book_snapshot(&symbol).await
    })
}

/// Runs until nobody holds the sender.
async fn follow(feeds: Arc<Feeds>, symbol: String, sender: Weak<watch::Sender<Latest>>) {
    let publish = |change: &dyn Fn(&mut Latest)| {
        if let Some(sender) = sender.upgrade() {
            sender.send_modify(|latest| change(latest));
        }
    };
    let subscribe = async {
        let client = feeds.client().await?;
        let mut streams = Vec::new();
        for subject in ["l2", "ticker", "candles"] {
            streams.push(
                client
                    .subscribe(format!("{subject}.{symbol}"))
                    .await
                    .map_err(|err| format!("nats: {err}"))?,
            );
        }
        Ok::<_, String>(streams)
    };
    let streams = match subscribe.await {
        Ok(streams) => streams,
        Err(message) => {
            warn!("Feed: {symbol}: {message}");
            let failed: Arc<str> = message.into();
            publish(&|latest| latest.failed = Some(failed.clone()));
            return;
        }
    };
    info!("Feed: following {symbol}");
    let mut messages = stream::select_all(streams);

    let mut book = Book::default();
    let mut snapshot = Some(start_snapshot(&feeds, &symbol, SNAPSHOT_DELAY));
    let mut gaps = feeds.gaps.load(Ordering::Relaxed);
    let mut dirty = true;
    let mut ticks = interval(FRAME_EVERY);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let (mut ticker, mut candle): (Option<Arc<str>>, Option<Arc<str>>) = (None, None);
    let mut fresh_text = false;
    let started = Instant::now();

    loop {
        tokio::select! {
            message = messages.next() => {
                let Some(message) = message else { break };
                let Ok(text) = std::str::from_utf8(&message.payload) else { continue };
                match message.subject.split('.').next() {
                    Some("l2") => {
                        if let Ok(change) = serde_json::from_str::<LevelChange>(text) {
                            dirty |= book.apply(&change);
                        }
                    }
                    Some("ticker") => {
                        ticker = Some(text.into());
                        fresh_text = true;
                    }
                    Some("candles") => {
                        candle = Some(text.into());
                        fresh_text = true;
                    }
                    _ => {}
                }
            }
            result = async { snapshot.as_mut().expect("guarded").await }, if snapshot.is_some() => {
                snapshot = None;
                match result {
                    Ok(Ok(levels)) => {
                        book.merge_snapshot(levels);
                        info!("Feed: {symbol} book loaded after {:?}", started.elapsed());
                    }
                    Ok(Err(message)) => {
                        warn!("Feed: {symbol}: can't read the book from the tape: {message}");
                        book.coverage = Coverage::Partial;
                    }
                    Err(err) => {
                        warn!("Feed: {symbol}: {err}");
                        book.coverage = Coverage::Partial;
                    }
                }
                dirty = true;
            }
            _ = ticks.tick() => {
                let Some(sender) = sender.upgrade() else { break };
                let now = feeds.gaps.load(Ordering::Relaxed);
                if now != gaps && snapshot.is_none() {
                    gaps = now;
                    book.touched.clear();
                    snapshot = Some(start_snapshot(&feeds, &symbol, SNAPSHOT_DELAY));
                }
                if dirty || fresh_text {
                    let frame: Arc<str> = book.frame().to_string().into();
                    sender.send_modify(|latest| {
                        latest.book = Some(frame.clone());
                        latest.ticker.clone_from(&ticker);
                        latest.candle.clone_from(&candle);
                    });
                    dirty = false;
                    fresh_text = false;
                }
            }
        }
    }
    if let Some(task) = snapshot {
        task.abort();
    }
    info!("Feed: stopped following {symbol}");
}

fn event(kind: &str, data: &str) -> Result<Event, Infallible> {
    Ok(Event::default().event(kind).data(data))
}

/// One watcher. Each wakes on the newest state, and repeats nothing it already sent.
pub fn sse(
    feeds: Arc<Feeds>,
    symbol: String,
    seat: OwnedSemaphorePermit,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let sender = feeds.join(&symbol);
    let mut changes = sender.subscribe();
    changes.mark_changed();
    let start = (
        sender,
        changes,
        Latest::default(),
        Vec::<Result<Event, Infallible>>::new(),
        seat,
    );

    let events = stream::unfold(
        start,
        |(sender, mut changes, mut sent, mut queue, seat)| async move {
            loop {
                if let Some(item) = queue.pop() {
                    return Some((item, (sender, changes, sent, queue, seat)));
                }
                if changes.changed().await.is_err() {
                    return None;
                }
                let latest = changes.borrow_and_update().clone();
                let differs =
                    |left: &Option<Arc<str>>, right: &Option<Arc<str>>| match (left, right) {
                        (Some(left), Some(right)) => !Arc::ptr_eq(left, right),
                        (Some(_), None) => true,
                        _ => false,
                    };
                if let (true, Some(text)) = (differs(&latest.failed, &sent.failed), &latest.failed)
                {
                    queue.push(event(
                        "source_failed",
                        &json!({ "source": "nats", "error": &**text }).to_string(),
                    ));
                }
                if let (true, Some(text)) = (differs(&latest.candle, &sent.candle), &latest.candle)
                {
                    queue.push(event("candle", text));
                }
                if let (true, Some(text)) = (differs(&latest.ticker, &sent.ticker), &latest.ticker)
                {
                    queue.push(event("ticker", text));
                }
                if let (true, Some(text)) = (differs(&latest.book, &sent.book), &latest.book) {
                    queue.push(event("book", text));
                }
                sent = latest;
            }
        },
    )
    .take_until(sleep(STREAM_LIFETIME));
    Sse::new(events).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(side: &str, price: &str, size: &str) -> LevelChange {
        LevelChange {
            side: side.to_string(),
            price_level: price.to_string(),
            new_quantity: size.to_string(),
            event_time: "t".to_string(),
        }
    }

    fn mantissa(text: &str) -> i64 {
        decimal::parse(text).unwrap()
    }

    #[test]
    fn a_change_sets_a_level_and_zero_removes_it() {
        let mut book = Book::default();
        book.apply(&change("buy", "10", "5"));
        book.apply(&change("buy", "10", "7"));
        book.apply(&change("sell", "11", "1"));
        assert_eq!(book.frame()["bids"], json!([["10", "7"]]));
        book.apply(&change("buy", "10", "0"));
        assert_eq!(book.frame()["bids"], json!([]));
        assert_eq!(book.frame()["asks"], json!([["11", "1"]]));
    }

    #[test]
    fn the_feed_wins_over_a_snapshot_that_is_behind_it() {
        let mut book = Book::default();
        book.apply(&change("buy", "10", "0"));
        book.apply(&change("sell", "12", "9"));
        book.merge_snapshot(vec![
            ("buy".to_string(), mantissa("10"), mantissa("5")),
            ("buy".to_string(), mantissa("9"), mantissa("3")),
            ("sell".to_string(), mantissa("12"), mantissa("1")),
        ]);
        let frame = book.frame();
        assert_eq!(
            frame["bids"],
            json!([["9", "3"]]),
            "10 was emptied on the feed"
        );
        assert_eq!(frame["asks"], json!([["12", "9"]]));
        assert_eq!(frame["coverage"], "complete");
    }

    #[test]
    fn best_prices_come_first_and_frames_are_capped() {
        let mut book = Book::default();
        for price in 1..=500 {
            book.apply(&change("buy", &price.to_string(), "1"));
            book.apply(&change("sell", &(price + 1000).to_string(), "1"));
        }
        let frame = book.frame();
        assert_eq!(frame["bids"].as_array().unwrap().len(), MAX_LEVELS);
        assert_eq!(frame["asks"].as_array().unwrap().len(), MAX_LEVELS);
        assert_eq!(frame["bids"][0][0], "500");
        assert_eq!(frame["asks"][0][0], "1001");
        assert_eq!(frame["bid_levels"], 500);
    }

    #[test]
    fn a_junk_change_changes_nothing() {
        let mut book = Book::default();
        assert!(!book.apply(&change("buy", "not a number", "1")));
        assert!(book.bids.is_empty());
    }
}
