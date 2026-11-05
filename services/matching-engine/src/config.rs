use std::env;
use std::process;
use std::str::FromStr;

use cluster::{NodeConfig, Retention, SnapshotPolicy, TapeRetention};

use transport::{Aeron, AeronExclusivePublication, AeronSubscription};

use tracing::{error, info};

use crate::publisher::{OUTPUT_SESSION_ID, OUTPUT_STREAM_ID};
use crate::recovery::RecoveryConfig;

const SNAPSHOT_LOG_BYTES: i64 = 1 << 30;
const SNAPSHOT_INTERVAL_SECS: i64 = 3_600;
const SNAPSHOTS_KEPT: usize = 3;

/// The engine's timestamps need the cluster to run `NanosecondClusterClock`.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

pub fn error_handler(code: i32, message: &str) {
    error!(code, message, "Critical Aeron communication failure. Exiting.");
    process::exit(1);
}

fn required_var(name: &str) -> String {
    env::var(name).unwrap_or_else(|err| {
        error!(target: "configuration", variable = name, error = ?err, "Required environment variable not set. Exiting.");
        process::exit(1);
    })
}

fn stream_id(name: &str) -> i32 {
    let value = required_var(name);
    value.parse::<i32>().unwrap_or_else(|err| {
        error!(target: "configuration", variable = name, value = %value, error = ?err, "Failed to parse stream ID as i32. Exiting.");
        process::exit(1);
    })
}

pub fn create_subscription(aeron: &Aeron) -> AeronSubscription {
    let channel = required_var("SUB_CHANNEL");
    let stream_id = stream_id("SUB_STREAM_ID");

    info!(target: "aeron_setup", channel = %channel, stream_id, "Creating Aeron subscription.");
    let subscription = transport::subscription(aeron, &channel, stream_id).unwrap_or_else(|err| {
        error!(target: "aeron_setup", kind = "add_subscription_failed", error = ?err, channel = %channel, stream_id, "Failed to add Aeron subscription. Ensure the Aeron media driver is running. Exiting.");
        process::exit(1);
    });

    info!(target: "aeron_setup", "Aeron subscription created.");
    subscription
}

pub fn create_exclusive_publication(aeron: &Aeron) -> AeronExclusivePublication {
    let channel = required_var("PUB_CHANNEL");
    let stream_id = stream_id("PUB_STREAM_ID");

    info!(target: "aeron_setup", channel = %channel, stream_id, "Creating Aeron publication.");
    let publication =
        transport::exclusive_publication(aeron, &channel, stream_id).unwrap_or_else(|err| {
            error!(target: "aeron_setup", kind = "add_publication_failed", error = ?err, channel = %channel, stream_id, "Failed to add Aeron exclusive publication. Ensure the Aeron media driver is running. Exiting.");
            process::exit(1);
        });

    info!(target: "aeron_setup", "Aeron publication created.");
    publication
}

/// Reads an optional variable, exiting on a bad value. Silently ignoring a limit that controls
/// what gets deleted, or what counts as a total loss, would be worse than not starting.
fn optional_var<T: FromStr>(name: &str) -> Option<T> {
    let value = env::var(name).ok()?;
    Some(value.parse().unwrap_or_else(|_| {
        error!(target: "configuration", variable = name, value = %value, "Failed to parse variable. Exiting.");
        process::exit(1);
    }))
}

pub fn recovery_config() -> RecoveryConfig {
    let mode = env::var("RECOVERY_MODE").ok();
    if mode.as_ref().is_some_and(|mode| mode != "rebuild") {
        error!(target: "configuration", "RECOVERY_MODE only accepts rebuild. Exiting.");
        process::exit(1);
    }
    let config = RecoveryConfig { durable: optional_var("DURABLE_POSITION"), rebuild: mode.is_some() };
    if config.rebuild && config.durable.is_none() {
        error!(target: "configuration", "RECOVERY_MODE=rebuild needs DURABLE_POSITION. Exiting.");
        process::exit(1);
    }
    config
}

pub fn node_config() -> NodeConfig {
    let tape = optional_var("TAPE_RETENTION_BYTES").map(|retain_bytes| TapeRetention {
        stream_id: OUTPUT_STREAM_ID,
        session_id: OUTPUT_SESSION_ID,
        retain_bytes,
    });
    NodeConfig {
        cluster_id: 0,
        aeron_dir: transport::aeron_dir(),
        archive_control_channel: required_var("ARCHIVE_CONTROL"),
        snapshots: SnapshotPolicy {
            log_bytes: optional_var("SNAPSHOT_LOG_BYTES").unwrap_or(SNAPSHOT_LOG_BYTES),
            interval: optional_var("SNAPSHOT_INTERVAL_SECS").unwrap_or(SNAPSHOT_INTERVAL_SECS) * NANOS_PER_SECOND,
        },
        retention: Retention { keep_snapshots: optional_var("SNAPSHOTS_KEPT").unwrap_or(SNAPSHOTS_KEPT), tape },
    }
}

pub fn order_count_max() -> u64 {
    let max_order_str = env::var("MAX_ORDERS").unwrap_or_else(|e| {
        // TODO: ERROR HANDLER
        error!(target: "configuration", variable = "orders_count_max", error = ?e, "Required environment variable for order book max orders not set. Exiting.");
        process::exit(1);
    });
    max_order_str.parse::<u64>().unwrap_or_else(|e| {
        // TODO: ERROR HANDLER
        error!(target: "configuration", variable = "orders_count_max", value = %max_order_str, error = ?e, "Failed to parse orders_count_max as usize. Exiting.");
        process::exit(1);
    })
}
