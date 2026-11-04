mod config;
mod handler;
mod orderbook;
mod publisher;
mod side;
mod types;

use config::{create_exclusive_publication, create_subscription, error_handler};

use handler::Handler;
use publisher::Publisher;

use std::collections::VecDeque;
use std::hint::spin_loop;
use std::process;
use std::time::Instant;

use sbe::ReadBuf;
use sbe::message_header_codec::MessageHeaderDecoder;

use tracing::subscriber::set_global_default;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use tracing::{error, info};

const FRAGMENT_LIMIT: usize = 10;

struct LatencyMetrics {
    total_messages: u64,
    total_latency_sum_ns: u128, // Use u128 for sum to prevent overflow with many messages
    recent_latencies_ns: VecDeque<u64>,
    window_size: usize,
}

impl LatencyMetrics {
    fn new(window_size: usize) -> Self {
        LatencyMetrics {
            total_messages: 0,
            total_latency_sum_ns: 0,
            recent_latencies_ns: VecDeque::with_capacity(window_size),
            window_size,
        }
    }

    fn add_latency(&mut self, latency_ns: u64) {
        self.total_messages += 1;
        self.total_latency_sum_ns += u128::from(latency_ns);

        self.recent_latencies_ns.push_back(latency_ns);
        if self.recent_latencies_ns.len() > self.window_size {
            self.recent_latencies_ns.pop_front();
        }
    }

    fn get_total_average(&self) -> f64 {
        if self.total_messages == 0 {
            0.0
        } else {
            self.total_latency_sum_ns as f64 / self.total_messages as f64
        }
    }

    fn get_recent_average(&self) -> f64 {
        if self.recent_latencies_ns.is_empty() {
            0.0
        } else {
            let sum: u64 = self.recent_latencies_ns.iter().sum();
            sum as f64 / self.recent_latencies_ns.len() as f64
        }
    }
}

fn on_message(buffer: &[u8], handler: &mut Handler, metrics: &mut LatencyMetrics) {
    let base_instant = Instant::now();

    let read_buf = ReadBuf::new(buffer);
    let header_decoder: MessageHeaderDecoder<ReadBuf<'_>> =
        MessageHeaderDecoder::default().wrap(read_buf, 0);
    match header_decoder.template_id() {
        1 => {
            handler.process_new_order(header_decoder);
            let delta_ns = base_instant.elapsed().as_nanos() as u64;
            metrics.add_latency(delta_ns);

            // Print statistics periodically, e.g., every 1000 messages or so
            if metrics.total_messages % 1000 == 0 {
                info!(
                    target: "matching_engine",
                    total_messages = metrics.total_messages,
                    last_delta_ns = delta_ns,
                    recent_avg_ns = metrics.get_recent_average(),
                    total_avg_ns = metrics.get_total_average(),
                    "Processing latency metrics"
                );
            }
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

    let publication = create_exclusive_publication(&aeron);
    let subscription = create_subscription(&aeron);

    let publisher = Publisher::new(publication);
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
