mod auth;
mod config;
mod connection;
mod hub;
mod nats;
mod protocol;
#[cfg(test)]
mod tests;

use config::Config;
use connection::accept_connection;
use hub::{Hub, refresh_tickers};

use std::process;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::time::{Duration, sleep};

use tracing::Level;
use tracing::subscriber::set_global_default;
use tracing::{error, info};
use tracing_subscriber::FmtSubscriber;

const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

async fn serve(listener: TcpListener, hub: Arc<Hub>, config: Arc<Config>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tokio::spawn(accept_connection(peer, stream, hub.clone(), config.clone()));
            }
            // Usually out of file descriptors, which clears as sockets close.
            Err(e) => {
                error!("Failed to accept a connection: {}", e);
                sleep(ACCEPT_RETRY_DELAY).await;
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();

    set_global_default(subscriber).unwrap_or_else(|err| {
        error!(target: "setup", kind="tracing_init_failed", error=?err, "Failed to create Tracing subscriber");
        process::exit(1);
    });

    let config = Arc::new(Config::from_env());
    let hub = Arc::new(Hub::default());

    let addr = std::env::var("WS_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let listener = match TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(e) => {
            error!("Can't listen on {}: {}", addr, e);
            process::exit(1);
        }
    };
    info!("Listening on: {}", addr);

    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "localhost".to_string());
    tokio::spawn(nats::run(nats_url, hub.clone()));
    tokio::spawn(refresh_tickers(hub.clone(), config.ticker_refresh));

    serve(listener, hub, config).await;
}
