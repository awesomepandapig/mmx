mod messages;
mod orderbook;
mod processors;

use messages::{SymbolType, decode_execution_report, symbol_name};
use orderbook::OrderBook;
use processors::execution::process_execution_report;
use processors::ticker::TickerState;

use std::collections::HashMap;
use std::hint::spin_loop;
use std::process;

use sbe::ReadBuf;
use sbe::message_header_codec::MessageHeaderDecoder;

use tokio::sync::mpsc;

use tracing::subscriber::set_global_default;
use tracing::{Level, error, info};
use tracing_subscriber::FmtSubscriber;

const FRAGMENT_LIMIT: usize = 10;

fn aeron_error_handler(code: i32, message: &str) {
    error!("Aeron: Error {}: {}", code, message);
}

#[tokio::main]
async fn main() {
    // Initialize tracing

    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();

    set_global_default(subscriber).unwrap_or_else(|err| {
        error!(target: "setup", kind="tracing_init_failed", error=?err, "Failed to create Tracing subscriber"); // TODO: Should this be a span?
        process::exit(1);
    });

    // --- Aeron and NATS/JetStream Setup ---
    let (_aeron, subscription) = match transport::subscribe_to_executions(aeron_error_handler) {
        Ok(client) => client,
        Err(e) => {
            error!("Aeron: Failed to subscribe: {:?}", e);
            return;
        }
    };

    // Connect to the NATS server
    let nats_url = transport::env_or("NATS_URL", "localhost");
    info!("Connecting to NATS server at {}", nats_url);
    let client = async_nats::connect(nats_url)
        .await
        .expect("nats connection failed"); // TODO: Replace expect with explicit handle and trace

    let (tx, mut rx) = mpsc::channel::<(String, String)>(1024);

    tokio::spawn(async move {
        info!("Async I/O publisher task started.");
        while let Some((subject, message)) = rx.recv().await {
            client
                .publish(subject, message.into())
                .await
                .expect("JetStream publish failed"); // TODO: Add proper error handling/flow control
        }
    });

    let mut markets: HashMap<SymbolType, (OrderBook, TickerState)> = HashMap::new();
    let mut last_seen_id = 0;

    info!("Starting Aeron polling loop...");

    loop {
        let fragments_read = subscription
            .poll_fn(
                |buffer, _header| {
                    let read_buf = ReadBuf::new(buffer);
                    let header_decoder: MessageHeaderDecoder<ReadBuf<'_>> =
                        MessageHeaderDecoder::default().wrap(read_buf, 0);

                    match header_decoder.template_id() {
                        3 => {
                            let report = decode_execution_report(header_decoder);
                            let (book, ticker_state) = markets.entry(report.symbol).or_insert_with(|| {
                                (OrderBook::new(), TickerState::new(symbol_name(&report.symbol)))
                            });
                            if let Some(message) = process_execution_report(book, &mut last_seen_id, ticker_state, &report) {
                                let subject = format!("ticker.{}", symbol_name(&report.symbol));
                                if let Err(e) = tx.try_send((subject, message)) {
                                    // TODO: NOTE: For true lossless, you would implement the "pending_report"
                                    // backpressure logic here, but using the String instead.
                                }
                            }
                        }
                        // Cancel rejects don't change the book.
                        4 => {}
                        template_id => {
                            panic!("incorrect template_id: {}", template_id)
                        }
                    }
                },
                FRAGMENT_LIMIT,
            )
            .expect("Failed to poll Aeron subscription");

        if fragments_read == 0 {
            spin_loop();
        }
    }
}
