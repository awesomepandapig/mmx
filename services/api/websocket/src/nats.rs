//! The process's one NATS connection.

use crate::hub::Hub;

use std::sync::Arc;

use async_nats::{Client, ConnectOptions, Event};

use bytes::Bytes;

use futures_util::stream::{self, BoxStream, select_all};
use futures_util::{Stream, StreamExt};

use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

use tracing::{error, info, warn};

pub const SUBJECTS: [&str; 4] = ["l2.*", "ticker.*", "candles.*", "orders.*.*"];

const RETRY_DELAY: Duration = Duration::from_secs(1);

pub enum FeedEvent {
    Message { subject: String, payload: Bytes },
    Status(&'static str),
}

/// Reconnecting and resubscribing are the client's job, and a server that isn't up yet is treated
/// as a lost connection rather than an error.
pub async fn connect(
    url: &str,
) -> Result<(Client, BoxStream<'static, FeedEvent>), Box<dyn std::error::Error + Send + Sync>> {
    let (status_tx, mut status_rx) = mpsc::channel(16);

    let client = ConnectOptions::new()
        .retry_on_initial_connect()
        .max_reconnects(None)
        .event_callback(move |event| {
            let status_tx = status_tx.clone();
            async move {
                let status = match event {
                    Event::Connected => "connected",
                    Event::Disconnected => "disconnected",
                    Event::SlowConsumer(_) => "gap",
                    other => {
                        warn!("NATS: {}", other);
                        return;
                    }
                };
                info!("NATS: {}", status);
                let _ = status_tx.try_send(FeedEvent::Status(status));
            }
        })
        .connect(url)
        .await?;

    let mut feeds: Vec<BoxStream<'static, FeedEvent>> = Vec::new();
    for subject in SUBJECTS {
        let subscriber = client.subscribe(subject).await?;
        feeds.push(
            subscriber
                .map(|message| FeedEvent::Message {
                    subject: message.subject.to_string(),
                    payload: message.payload,
                })
                .boxed(),
        );
    }
    feeds.push(stream::poll_fn(move |cx| status_rx.poll_recv(cx)).boxed());

    Ok((client, select_all(feeds).boxed()))
}

pub async fn pump(mut feed: impl Stream<Item = FeedEvent> + Unpin, hub: &Hub) {
    while let Some(event) = feed.next().await {
        match event {
            FeedEvent::Message { subject, payload } => hub.publish(&subject, &payload),
            FeedEvent::Status(status) => hub.broadcast(status),
        }
    }
}

pub async fn run(url: String, hub: Arc<Hub>) {
    loop {
        info!("Connecting to NATS server at {}", url);
        match connect(&url).await {
            Ok((_client, feed)) => {
                pump(feed, &hub).await;
                warn!("NATS feed ended");
                hub.broadcast("disconnected");
            }
            Err(e) => error!("NATS connection failed: {}", e),
        }
        sleep(RETRY_DELAY).await;
    }
}
