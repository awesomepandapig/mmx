mod admin;

mod aeron_handler;
use aeron_handler::{EngineLink, OrderSender, spawn_order_publisher};

mod cluster_gateway;
use cluster_gateway::{ClusterConfig, spawn_cluster_gateway};

mod decimal;

mod halts;
use halts::{Halts, ValkeyHalts};

mod identity;
use identity::{Authenticator, TrustedGameServer};

mod markets;
use markets::MarketRegistry;

mod metrics;
use metrics::Metrics;

mod order;

mod rate_limit;
use rate_limit::{Limit, RateLimiter, Unlimited, Valkey};

mod repeat;
use repeat::{RepeatGuard, ReportStream};

mod routes;

mod store;
use store::{OrderStore, QuestDb};

mod validation;

mod errors;

#[cfg(test)]
mod tests;

use std::env;
use std::process;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{error, info, warn};

const DEFAULT_MARKETS_FILE: &str = "services/api/rest/markets.yaml";
const DEFAULT_QUESTDB_URL: &str = "http://localhost:9000";
const DEFAULT_RETRY_WINDOW_SECS: u64 = 300;
const DEFAULT_RATE_LIMIT_ORDERS: u64 = 50;
const DEFAULT_RATE_LIMIT_WINDOW_MS: u64 = 1000;

pub struct AppState {
    orders: OrderSender,
    auth: Arc<dyn Authenticator>,
    markets: MarketRegistry,
    rate_limiter: Arc<dyn RateLimiter>,
    store: Arc<dyn OrderStore>,
    /// Both of these need the cluster's report stream, so they're empty without one.
    report_stream: Option<Arc<ReportStream>>,
    repeats: Option<RepeatGuard>,
    require_client_order_id: bool,
    engine: Arc<EngineLink>,
    halts: Arc<Halts>,
    metrics: Metrics,
    admin_token: Option<String>,
}

fn env_number<T: FromStr>(name: &str, default: T) -> Result<T, String> {
    match env::var(name) {
        Ok(value) => value.parse().map_err(|_| format!("{name} must be a number, got {value:?}")),
        Err(_) => Ok(default),
    }
}

fn rate_limiter() -> Result<Arc<dyn RateLimiter>, String> {
    let Ok(url) = env::var("VALKEY_URL") else {
        warn!("Rate limit: VALKEY_URL isn't set, so orders aren't limited");
        return Ok(Arc::new(Unlimited));
    };
    let limit = Limit {
        orders: env_number("RATE_LIMIT_ORDERS", DEFAULT_RATE_LIMIT_ORDERS)?,
        window_ms: env_number("RATE_LIMIT_WINDOW_MS", DEFAULT_RATE_LIMIT_WINDOW_MS)?,
    };
    info!("Rate limit: {} orders per {} ms by default", limit.orders, limit.window_ms);
    Ok(Arc::new(Valkey::new(&url, limit)?))
}

fn halts() -> Result<Arc<Halts>, String> {
    let halts = match env::var("VALKEY_URL") {
        Ok(url) => Halts::shared(Arc::new(ValkeyHalts::new(&url)?)),
        Err(_) => Halts::standalone(),
    };
    Ok(Arc::new(halts))
}

async fn start() -> Result<(), String> {
    let start_time = Instant::now();

    info!("Aeron: Using directory: {:?}", transport::aeron_dir());

    let markets_file = transport::env_or("MARKETS_FILE", DEFAULT_MARKETS_FILE);
    let markets = MarketRegistry::load(&markets_file)?;
    info!("Markets: Enabled {:?} from {}", markets.current().symbols(), markets_file);

    let store: Arc<dyn OrderStore> =
        Arc::new(QuestDb::new(&transport::env_or("QUESTDB_URL", DEFAULT_QUESTDB_URL))?);

    // One read before serving, so a normal start never bounces orders while the list loads.
    let halts = halts()?;
    halts.refresh().await;
    halts.start_polling();

    let engine = Arc::new(EngineLink::default());

    let clustered = ClusterConfig::from_env();
    let require_client_order_id = clustered.is_some();
    let (orders, report_stream, repeats) = match clustered {
        Some(config) => {
            let window = env_number("RETRY_WINDOW_SECS", DEFAULT_RETRY_WINDOW_SECS)?;
            let window = Duration::from_secs(window);
            let stream = Arc::new(ReportStream::default());
            let orders = spawn_cluster_gateway(config, stream.clone(), window, engine.clone())?;
            let repeats = RepeatGuard { stream: stream.clone(), store: store.clone(), window };
            (orders, Some(stream), Some(repeats))
        }
        None => {
            let channel = transport::env_or("PUB_CHANNEL", transport::ORDERS_CHANNEL);
            let stream_id = transport::stream_id_or("PUB_STREAM_ID", transport::ORDERS_STREAM);
            (spawn_order_publisher(&channel, stream_id, engine.clone())?, None, None)
        }
    };

    let shared_state = Arc::new(AppState {
        orders,
        auth: Arc::new(TrustedGameServer),
        markets,
        rate_limiter: rate_limiter()?,
        store,
        report_stream,
        repeats,
        require_client_order_id,
        engine,
        halts,
        metrics: Metrics::default(),
        admin_token: admin::admin_token(),
    });

    let app = routes::router(shared_state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8000")
        .await
        .map_err(|e| format!("Server: Failed to bind to port 8000: {e}"))?;
    info!("Server: Bound to 0.0.0.0:8000");

    let startup_duration = start_time.elapsed();
    info!(
        "Startup complete in {:.2?} — ready to accept requests",
        startup_duration
    );

    axum::serve(listener, app).await.map_err(|e| format!("Server failed: {e}"))
}

#[tokio::main]
async fn main() {
    env_logger::init();

    if let Err(err) = start().await {
        error!("{err}");
        process::exit(1);
    }
}
