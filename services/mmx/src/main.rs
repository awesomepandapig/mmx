//! `mmx`: control and test a running exchange from one command. `mmx serve` is the control plane,
//! and every other subcommand is a client, of it or of a gateway.

mod client;
mod decimal;
mod events;
mod markets;
mod output;
mod server;
mod upstream;
mod watch;
mod webhook;

#[cfg(test)]
mod tests;

use client::{ActivityQuery, Admin, NewOrder, Orders};
use output::line;
use server::AppState;
use upstream::{Targets, Upstreams};

use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing_subscriber::{EnvFilter, FmtSubscriber};
use uuid::Uuid;

const DEFAULT_URL: &str = "http://127.0.0.1:8093";
const TOKEN_ENV: &str = "MMX_ADMIN_TOKEN";
const NATS_PORT: u16 = 4222;
const VALKEY_PORT: u16 = 6379;

#[derive(Parser)]
#[command(
    name = "mmx",
    version,
    about = "Control and test a running MMX exchange"
)]
struct Cli {
    /// Print JSON instead of text
    #[arg(long, global = true)]
    json: bool,

    /// Where `mmx serve` is
    #[arg(long, global = true, env = "MMX_URL", default_value = DEFAULT_URL)]
    url: String,

    #[command(flatten)]
    services: Services,

    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Debug, Clone)]
struct Services {
    /// Gateway base URLs, repeated or comma-separated
    #[arg(
        long = "gateway",
        global = true,
        env = "MMX_GATEWAY_URLS",
        value_delimiter = ',',
        default_value = "http://localhost:8000"
    )]
    gateways: Vec<String>,

    /// Outbox status URL
    #[arg(
        long,
        global = true,
        env = "MMX_OUTBOX_URL",
        default_value = "http://localhost:8092"
    )]
    outbox: String,

    /// QuestDB HTTP URL
    #[arg(
        long,
        global = true,
        env = "QUESTDB_URL",
        default_value = "http://localhost:9000"
    )]
    questdb: String,

    /// NATS address
    #[arg(
        long,
        global = true,
        env = "NATS_URL",
        default_value = "localhost",
        hide_env_values = true
    )]
    nats: String,

    /// Valkey URL. Left out, Valkey isn't checked
    #[arg(long, global = true, env = "VALKEY_URL", hide_env_values = true)]
    valkey: Option<String>,

    /// The markets file, which must be the one gateways read
    #[arg(
        long,
        global = true,
        env = "MARKETS_FILE",
        default_value = "services/api/rest/markets.yaml"
    )]
    markets_file: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Run the control-plane server
    Serve(ServeArgs),
    /// Show every service and an overall verdict
    Status,
    /// Show whether `mmx serve` itself is up
    Health,
    /// Inspect and change markets
    Markets {
        #[command(subcommand)]
        command: MarketsCommand,
    },
    /// Inspect configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Place and inspect orders as a game server would, directly on a gateway
    Order {
        /// The player to act for
        #[arg(long, global = true, env = "MMX_PLAYER")]
        player: Option<Uuid>,

        #[command(subcommand)]
        command: OrderCommand,
    },
    /// Recent order reports across all players, from QuestDB
    Orders(ActivityArgs),
    /// Recent trades across all players, from QuestDB, with one row per side
    Trades(ActivityArgs),
    /// Work with webhooks
    Webhook {
        #[command(subcommand)]
        command: WebhookCommand,
    },
    /// Follow markets, orders, trades and webhook delivery as they change
    Watch,
}

#[derive(Args)]
struct ServeArgs {
    /// Address to listen on. Defaults to port 8093, on all interfaces
    #[arg(long, env = "MMX_BIND")]
    bind: Option<SocketAddr>,

    /// Run with no token, on a loopback address only
    #[arg(long)]
    insecure_local: bool,

    /// How often `/events` polls, in milliseconds
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(100..))]
    poll_ms: u64,
}

#[derive(Args)]
struct ActivityArgs {
    #[arg(long)]
    symbol: Option<String>,
    /// Only this player, by UUID
    #[arg(long)]
    player: Option<Uuid>,
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Subcommand)]
enum MarketsCommand {
    /// List every market, in the file or on a gateway
    List,
    /// Show one market on each gateway
    Get { symbol: String },
    /// Add or replace a market. A limit left out returns to its default
    Set {
        symbol: String,
        #[arg(long)]
        tick_size: String,
        #[arg(long)]
        min_size: Option<String>,
        #[arg(long)]
        max_size: Option<String>,
        /// List it, but refuse its orders
        #[arg(long)]
        disabled: bool,
    },
    /// Remove a market. Resting orders stay in the engine, so cancel them first
    Rm { symbol: String },
    /// Stop accepting orders on every gateway
    Halt { symbol: String },
    /// Accept orders again on every gateway
    Resume { symbol: String },
    /// Cancel every open order in a market. Halt it first, or new orders can arrive behind this
    CancelAll { symbol: String },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Show the configuration in effect, without secrets
    Show,
}

#[derive(Subcommand)]
enum OrderCommand {
    /// Place an order once. It's never retried
    Place {
        #[arg(long)]
        symbol: String,
        #[arg(long)]
        side: String,
        #[arg(long)]
        size: String,
        /// Limit price. Without it, this is a market order
        #[arg(long)]
        price: Option<String>,
        /// Generated if left out
        #[arg(long)]
        client_order_id: Option<Uuid>,
    },
    /// Cancel an order
    Cancel { id: Uuid },
    /// Show an order
    Get { id: Uuid },
    /// List the player's orders
    List {
        #[arg(long)]
        symbol: Option<String>,
        /// `all` or `open`
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Subcommand)]
enum WebhookCommand {
    /// Show outbox delivery status
    Status,
    /// Act as a game: receive webhooks, verify them, dedupe and print them
    Listen {
        #[arg(long)]
        port: u16,
        /// The same value as the outbox's WEBHOOK_SECRET
        #[arg(long, env = "WEBHOOK_SECRET", hide_env_values = true)]
        secret: String,
        #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
        bind: IpAddr,
    },
}

/// Strips a trailing slash and checks the scheme. This build has no TLS, because reqwest's backend
/// needs CMake, so an `https` URL has to fail here and not on the first request.
fn base_url(name: &str, url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');
    if url.starts_with("https://") {
        return Err(format!(
            "{name}: {url}: https isn't supported. Reach it over http inside the network or a tunnel."
        ));
    }
    if !url.starts_with("http://") {
        return Err(format!("{name}: {url} must start with http://"));
    }
    Ok(url.to_string())
}

impl Services {
    fn targets(&self) -> Result<Targets, String> {
        let gateways: Result<Vec<String>, String> = self
            .gateways
            .iter()
            .map(|url| base_url("gateway", url))
            .collect();
        let gateways = gateways?;
        if gateways.is_empty() {
            return Err("at least one gateway is required".to_string());
        }
        Ok(Targets {
            gateways,
            outbox: base_url("outbox", &self.outbox)?,
            questdb: base_url("questdb", &self.questdb)?,
            nats: self.nats.clone(),
            valkey: self.valkey.clone(),
        })
    }
}

fn config_view(cli: &Cli, token: Option<&str>) -> Result<Value, String> {
    let targets = cli.services.targets()?;
    Ok(json!({
        "url": base_url("url", &cli.url)?,
        "gateways": targets.gateways,
        "outbox": targets.outbox,
        "questdb": targets.questdb,
        "nats": upstream::host_port(&targets.nats, NATS_PORT),
        "valkey": targets.valkey.as_deref().map(|url| upstream::host_port(url, VALKEY_PORT)),
        "markets_file": cli.services.markets_file,
        "admin_token": if token.is_some() { "set" } else { "not set" },
    }))
}

fn show_config(cli: &Cli, token: Option<&str>) -> Result<(), String> {
    let view = config_view(cli, token)?;
    if cli.json {
        line(&view.to_string());
    } else if let Some(fields) = view.as_object() {
        for (name, value) in fields {
            line(&format!(
                "{name}: {}",
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_string)
            ));
        }
    }
    Ok(())
}

async fn serve(cli: &Cli, args: &ServeArgs, token: Option<String>) -> Result<(), String> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = FmtSubscriber::builder().with_env_filter(filter).finish();
    tracing::subscriber::set_global_default(subscriber).map_err(|err| err.to_string())?;

    let address = server::listen_address(token.as_deref(), args.insecure_local, args.bind)?;
    let state = AppState {
        up: Arc::new(Upstreams::new(cli.services.targets()?, token.clone())?),
        markets_file: cli.services.markets_file.clone(),
        token: server::hash_token(token.as_deref()),
        poll_every: Duration::from_millis(args.poll_ms),
        edits: Mutex::new(()),
    };
    server::serve(Arc::new(state), address).await
}

async fn run_markets(admin: &Admin, command: &MarketsCommand) -> Result<(), String> {
    match command {
        MarketsCommand::List => admin.markets().await,
        MarketsCommand::Get { symbol } => admin.market(symbol).await,
        MarketsCommand::Set {
            symbol,
            tick_size,
            min_size,
            max_size,
            disabled,
        } => {
            let mut market = json!({ "tick_size": tick_size });
            if let Some(min_size) = min_size {
                market["min_size"] = json!(min_size);
            }
            if let Some(max_size) = max_size {
                market["max_size"] = json!(max_size);
            }
            if *disabled {
                market["enabled"] = json!(false);
            }
            admin.set_market(symbol, &market).await
        }
        MarketsCommand::Rm { symbol } => admin.remove_market(symbol).await,
        MarketsCommand::Halt { symbol } => admin.market_action(symbol, "halt").await,
        MarketsCommand::Resume { symbol } => admin.market_action(symbol, "resume").await,
        MarketsCommand::CancelAll { symbol } => admin.cancel_all(symbol).await,
    }
}

async fn run_order(cli: &Cli, player: Option<Uuid>, command: &OrderCommand) -> Result<(), String> {
    let player = player.ok_or("--player or MMX_PLAYER is required")?;
    let targets = cli.services.targets()?;
    let gateway = targets
        .gateways
        .first()
        .cloned()
        .ok_or("at least one gateway is required")?;
    let orders = Orders::new(gateway, player, cli.json)?;
    match command {
        OrderCommand::Place {
            symbol,
            side,
            size,
            price,
            client_order_id,
        } => {
            let order = NewOrder {
                client_order_id: client_order_id.unwrap_or_else(Uuid::new_v4),
                product_id: symbol.clone(),
                side: side.clone(),
                r#type: if price.is_some() { "limit" } else { "market" }.to_string(),
                size: size.clone(),
                price: price.clone(),
            };
            orders.place(&order).await
        }
        OrderCommand::Cancel { id } => orders.cancel(*id).await,
        OrderCommand::Get { id } => orders.get(*id).await,
        OrderCommand::List {
            symbol,
            status,
            limit,
        } => {
            orders
                .list(symbol.as_deref(), status.as_deref(), *limit)
                .await
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let token = env::var(TOKEN_ENV).ok().filter(|token| !token.is_empty());
    let admin = || Admin::new(base_url("url", &cli.url)?, token.clone(), cli.json);
    let activity = |args: &ActivityArgs| ActivityQuery {
        symbol: args.symbol.clone(),
        player: args.player.map(|player| player.to_string()),
        limit: args.limit,
    };

    match &cli.command {
        Command::Serve(args) => serve(&cli, args, token.clone()).await,
        Command::Status => admin()?.status().await,
        Command::Health => admin()?.health().await,
        Command::Markets { command } => run_markets(&admin()?, command).await,
        Command::Config {
            command: ConfigCommand::Show,
        } => show_config(&cli, token.as_deref()),
        Command::Order { player, command } => run_order(&cli, *player, command).await,
        Command::Orders(args) => admin()?.activity("orders", &activity(args)).await,
        Command::Trades(args) => admin()?.activity("trades", &activity(args)).await,
        Command::Webhook {
            command: WebhookCommand::Status,
        } => admin()?.webhooks().await,
        Command::Webhook {
            command: WebhookCommand::Listen { port, secret, bind },
        } => webhook::listen(*bind, *port, secret, cli.json).await,
        Command::Watch => {
            watch::watch(&base_url("url", &cli.url)?, token.as_deref(), cli.json).await
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            output::note(&format!("mmx: {message}"));
            ExitCode::FAILURE
        }
    }
}
