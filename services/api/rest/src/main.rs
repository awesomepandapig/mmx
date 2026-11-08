mod admin;

mod aeron_handler;
use aeron_handler::{EngineLink, OrderSender, spawn_order_publisher};

mod cluster_gateway;
use cluster_gateway::{ClusterConfig, ClusterWatch, spawn_cluster_gateway};

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

mod order_id;
use order_id::{DEFAULT_WINDOW_SECS, OrderIdRules};

mod rate_limit;
use rate_limit::{Decision, Limit, RateLimiter, Unlimited, Valkey};

mod recorder;
use recorder::ReportStream;

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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use log::{error, info, warn};

const DEFAULT_MARKETS_FILE: &str = "services/api/rest/markets.yaml";
const DEFAULT_QUESTDB_URL: &str = "http://localhost:9000";
const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8000";
const DEFAULT_RATE_LIMIT_ORDERS: u64 = 50;
const DEFAULT_RATE_LIMIT_WINDOW_MS: u64 = 1000;
const CLAIMS_POLL: Duration = Duration::from_millis(200);

pub struct AppState {
    orders: OrderSender,
    auth: Arc<dyn Authenticator>,
    markets: MarketRegistry,
    rate_limiter: Arc<dyn RateLimiter>,
    store: Arc<dyn OrderStore>,
    /// These need the cluster, so they're empty without one. With `id_rules` set, an order must
    /// have a fresh UUIDv7 and a confirmed claim, or it isn't sent.
    report_stream: Option<Arc<ReportStream>>,
    id_rules: Option<OrderIdRules>,
    claims_ready: Option<Arc<AtomicBool>>,
    engine: Arc<EngineLink>,
    /// `None` without a cluster.
    cluster: Option<Arc<ClusterWatch>>,
    halts: Arc<Halts>,
    metrics: Metrics,
    admin_token: Option<String>,
}

fn env_number<T: FromStr>(name: &str, default: T) -> Result<T, String> {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| format!("{name} must be a number, got {value:?}")),
        Err(_) => Ok(default),
    }
}

/// Takes the forms the nodes' settings use, because this must be the same value as theirs.
fn parse_duration(text: &str) -> Option<Duration> {
    let number = |digits: &str| digits.parse::<u64>().ok();
    if let Some(digits) = text.strip_suffix("ms") {
        return number(digits).map(Duration::from_millis);
    }
    if let Some(digits) = text.strip_suffix('s') {
        return number(digits).map(Duration::from_secs);
    }
    text.strip_suffix('m')
        .and_then(number)
        .map(|minutes| Duration::from_secs(minutes * 60))
}

/// The session timeout has no default, because a guess that's too short could report a live order
/// as never landed.
fn order_id_rules() -> Result<OrderIdRules, String> {
    let window = env_number("ORDER_ID_WINDOW_SECS", DEFAULT_WINDOW_SECS)?;
    if window == 0 {
        return Err("ORDER_ID_WINDOW_SECS must be above 0".to_string());
    }
    let session_timeout = env::var("CLUSTER_SESSION_TIMEOUT").map_err(|_| {
        "CLUSTER_SESSION_TIMEOUT must match the cluster's session timeout".to_string()
    })?;
    let session_timeout = parse_duration(&session_timeout).ok_or_else(|| {
        format!("CLUSTER_SESSION_TIMEOUT must look like 30s, got {session_timeout:?}")
    })?;
    Ok(OrderIdRules {
        window: Duration::from_secs(window),
        session_timeout,
    })
}

fn rate_limiter(id_rules: Option<OrderIdRules>) -> Result<Arc<dyn RateLimiter>, String> {
    let Ok(url) = env::var("VALKEY_URL") else {
        if id_rules.is_some() {
            return Err(
                "VALKEY_URL is required in cluster mode, because orders are claimed there"
                    .to_string(),
            );
        }
        warn!("Rate limit: VALKEY_URL isn't set, so orders aren't limited");
        return Ok(Arc::new(Unlimited));
    };
    let limit = Limit {
        orders: env_number("RATE_LIMIT_ORDERS", DEFAULT_RATE_LIMIT_ORDERS)?,
        window_ms: env_number("RATE_LIMIT_WINDOW_MS", DEFAULT_RATE_LIMIT_WINDOW_MS)?,
    };
    info!(
        "Rate limit: {} orders per {} ms by default",
        limit.orders, limit.window_ms
    );
    Ok(Arc::new(Valkey::new(&url, limit, id_rules)?))
}

/// Keeps `/readyz` current, and makes a lost epoch start its hold at once rather than at the first
/// order. Whether an order may go is still decided by the script, never by this flag.
fn watch_claims(limiter: Arc<dyn RateLimiter>) -> Arc<AtomicBool> {
    let ready = Arc::new(AtomicBool::new(false));
    let flag = ready.clone();
    tokio::spawn(async move {
        loop {
            let answer = limiter.claims_ready().await;
            let now_ready = answer == Ok(Decision::Allowed);
            let was_ready = flag.swap(now_ready, Ordering::Relaxed);
            match answer {
                Ok(Decision::Allowed) if !was_ready => info!("Claims: Ready for new orders"),
                Ok(Decision::Recovering { retry_after_ms }) if was_ready => {
                    error!("Claims: Valkey lost its data, so new orders wait {retry_after_ms} ms");
                }
                _ => {}
            }
            tokio::time::sleep(CLAIMS_POLL).await;
        }
    });
    ready
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
    info!(
        "Markets: Enabled {:?} from {}",
        markets.current().symbols(),
        markets_file
    );

    let store: Arc<dyn OrderStore> = Arc::new(QuestDb::new(&transport::env_or(
        "QUESTDB_URL",
        DEFAULT_QUESTDB_URL,
    ))?);

    // One read before serving, so a normal start never bounces orders while the list loads.
    let halts = halts()?;
    halts.refresh().await;
    halts.start_polling();

    let engine = Arc::new(EngineLink::default());

    let (orders, report_stream, id_rules, cluster) = match ClusterConfig::from_env()? {
        Some(config) => {
            let id_rules = order_id_rules()?;
            info!(
                "Order IDs: Accepted for {:?}, and an order settles {:?} after its ID",
                id_rules.window,
                id_rules.settled_after()
            );
            let stream = Arc::new(ReportStream::default());
            let watch = Arc::new(ClusterWatch::default());
            let orders =
                spawn_cluster_gateway(config, stream.clone(), engine.clone(), watch.clone())?;
            (orders, Some(stream), Some(id_rules), Some(watch))
        }
        None => {
            let channel = transport::env_or("PUB_CHANNEL", transport::ORDERS_CHANNEL);
            let stream_id = transport::stream_id_or("PUB_STREAM_ID", transport::ORDERS_STREAM);
            (
                spawn_order_publisher(&channel, stream_id, engine.clone())?,
                None,
                None,
                None,
            )
        }
    };

    let rate_limiter = rate_limiter(id_rules)?;
    let claims_ready = id_rules.map(|_| watch_claims(rate_limiter.clone()));

    let shared_state = Arc::new(AppState {
        orders,
        auth: Arc::new(TrustedGameServer),
        markets,
        rate_limiter,
        store,
        report_stream,
        id_rules,
        claims_ready,
        engine,
        cluster,
        halts,
        metrics: Metrics::default(),
        admin_token: admin::admin_token(),
    });

    let app = routes::router(shared_state);

    let address = transport::env_or("BIND_ADDR", DEFAULT_BIND_ADDR);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|e| format!("Server: Failed to bind to {address}: {e}"))?;
    info!("Server: Bound to {address}");

    let startup_duration = start_time.elapsed();
    info!(
        "Startup complete in {:.2?} — ready to accept requests",
        startup_duration
    );

    axum::serve(listener, app)
        .await
        .map_err(|e| format!("Server failed: {e}"))
}

#[tokio::main]
async fn main() {
    // mmx turns on reqwest's bring-your-own-provider TLS, and a workspace build shares that feature
    // with every crate, so without this the first client built here panics.
    let _ = rustls::crypto::ring::default_provider().install_default();
    env_logger::init();

    if let Err(err) = start().await {
        error!("{err}");
        process::exit(1);
    }
}
