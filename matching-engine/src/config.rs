use std::env;
use std::process;

use transport::{Aeron, AeronExclusivePublication, AeronSubscription};

use tracing::{error, info};

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
