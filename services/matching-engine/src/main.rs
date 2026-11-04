mod clustered;
mod config;
mod handler;
mod metrics;
mod orderbook;
mod publisher;
mod side;
mod types;

use config::{create_exclusive_publication, create_subscription, error_handler};

use handler::Handler;
use metrics::LatencyMetrics;
use publisher::Publisher;

use std::hint::spin_loop;
use std::process;

use sbe::ReadBuf;
use sbe::message_header_codec::MessageHeaderDecoder;

use tracing::subscriber::set_global_default;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use tracing::{error, info};

const FRAGMENT_LIMIT: usize = 10;

pub fn on_message(buffer: &[u8], handler: &mut Handler, metrics: &mut LatencyMetrics) {
    let started = LatencyMetrics::start();

    let read_buf = ReadBuf::new(buffer);
    let header_decoder: MessageHeaderDecoder<ReadBuf<'_>> =
        MessageHeaderDecoder::default().wrap(read_buf, 0);
    match header_decoder.template_id() {
        1 => {
            handler.process_new_order(header_decoder);
            metrics.record(started);
        }
        2 => {
            handler.process_cancel_order(header_decoder);
        }
        unknown_id => {
            error!(
                target: "matching_engine",
                template_id = unknown_id,
                "Unknown message template ID received, rejecting message"
            );
        }
    }
}

fn main() -> ! {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = FmtSubscriber::builder().with_env_filter(filter).finish();

    set_global_default(subscriber).unwrap_or_else(|err| {
        error!(target: "setup", kind="tracing_init_failed", error=?err, "Failed to create Tracing subscriber"); // TODO: Should this be a span?
        process::exit(1);
    });

    info!(target: "aeron_setup", aeron_dir = %transport::aeron_dir(), "Connecting to Aeron media driver.");
    let aeron = transport::connect(error_handler).unwrap_or_else(|err| {
        error!(target: "aeron_setup", kind = "aeron_init_failed", error = ?err, "Failed to create Aeron client instance.");
        process::exit(1);
    });

    if std::env::var("CLUSTER_ID").is_ok() {
        clustered::run(&aeron);
        process::exit(0);
    }

    let publication = create_exclusive_publication(&aeron);
    let subscription = create_subscription(&aeron);

    let publisher = Publisher::new(Some(publication), false);
    let mut handler = Handler::new(publisher);
    let mut metrics = LatencyMetrics::new(100_000);

    loop {
        let fragments_read = subscription
            .poll_fn(
                |buffer, _header| on_message(buffer, &mut handler, &mut metrics),
                FRAGMENT_LIMIT,
            )
            .unwrap_or_else(|err| {
                error!(target: "aeron_setup", kind = "poll_failed", error = ?err, "Failed to poll Aeron subscription. Exiting.");
                process::exit(1);
            });

        if fragments_read == 0 {
            spin_loop();
        }
    }
}
