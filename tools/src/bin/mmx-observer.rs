//! Exports exchange metrics over OTLP without touching the engine's hot path.
//!
//! `matching-engine` links no telemetry. This process subscribes to its execution reports and derives
//! everything from there. Set the destination with `OTEL_EXPORTER_OTLP_ENDPOINT`, which
//! defaults to http://localhost:4318.
//!
//! It keeps every order in memory to verify invariants, so it's a development tool for now.

use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opentelemetry::KeyValue;
use opentelemetry::metrics::MeterProvider;
use opentelemetry_otlp::{MetricExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::SdkMeterProvider;

use tools::check::{Checker, Stats};
use tools::event::{Event, SCALE, Side, decode};

const PUBLISH_INTERVAL: Duration = Duration::from_secs(1);

fn on_error(code: i32, message: &str) {
    eprintln!("aeron error {code}: {message}");
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos() as u64
}

fn main() {
    // mmx turns on reqwest's bring-your-own-provider TLS, and a workspace build shares that feature
    // with every crate, so without this the first client built here panics.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let endpoint = transport::env_or("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4318");
    let exporter = MetricExporter::builder()
        .with_http()
        .with_endpoint(format!("{endpoint}/v1/metrics"))
        .build()
        .expect("failed to build the OTLP exporter");
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_service_name("mmx-observer")
                .build(),
        )
        .build();

    let meter = provider.meter("mmx");
    let orders = meter.u64_counter("mmx.orders").build();
    let trades = meter.u64_counter("mmx.trades").build();
    let canceled = meter.u64_counter("mmx.canceled").build();
    let rejected = meter.u64_counter("mmx.rejected").build();
    let cancel_rejects = meter.u64_counter("mmx.cancel_rejects").build();
    let violations = meter.u64_counter("mmx.violations").build();
    let filled = meter.f64_counter("mmx.filled_qty").build();
    let best_bid = meter.f64_gauge("mmx.best_bid").build();
    let best_ask = meter.f64_gauge("mmx.best_ask").build();
    let depth = meter.u64_gauge("mmx.book_levels").build();
    let lag = meter
        .f64_histogram("mmx.observer_lag")
        .with_unit("s")
        .with_description("Time from the engine stamping a report to this process reading it")
        .with_boundaries(vec![
            1e-5, 5e-5, 1e-4, 5e-4, 1e-3, 5e-3, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0,
        ])
        .build();

    let aeron = transport::connect(on_error).expect("failed to connect to the media driver");
    let channel = transport::env_or("SUB_CHANNEL", transport::EXECUTIONS_SUB_CHANNEL);
    let subscription = transport::subscription(&aeron, &channel, transport::EXECUTIONS_STREAM)
        .expect("failed to subscribe to the stream");
    println!("observing {channel}, exporting to {endpoint}");

    let mut checker = Checker::new();
    let mut last = Stats::default();
    let mut last_violations = 0;
    let mut last_publish = Instant::now();

    loop {
        let fragments = subscription
            .poll_fn(
                |buffer, _header| {
                    let event = decode(buffer);
                    if let Event::Execution(report) = &event {
                        let age_ns = now_ns().saturating_sub(report.transact_time);
                        lag.record(age_ns as f64 / 1e9, &[]);
                    }
                    checker.apply(&event);
                },
                256,
            )
            .expect("failed to poll");

        if last_publish.elapsed() >= PUBLISH_INTERVAL {
            let stats = &checker.stats;
            orders.add(stats.orders - last.orders, &[]);
            trades.add(stats.trades - last.trades, &[]);
            canceled.add(stats.canceled - last.canceled, &[]);
            rejected.add(stats.rejected - last.rejected, &[]);
            cancel_rejects.add(stats.cancel_rejects - last.cancel_rejects, &[]);
            filled.add(
                (stats.filled_qty - last.filled_qty) as f64 / SCALE as f64,
                &[],
            );
            violations.add(checker.violation_count - last_violations, &[]);

            for name in checker.symbols() {
                let symbol = [KeyValue::new("symbol", name.clone())];
                if let Some(price) = checker.best_bid(&name) {
                    best_bid.record(price as f64 / SCALE as f64, &symbol);
                }
                if let Some(price) = checker.best_ask(&name) {
                    best_ask.record(price as f64 / SCALE as f64, &symbol);
                }
                let levels = checker.top_of_book(&name, Side::Buy, usize::MAX).len()
                    + checker.top_of_book(&name, Side::Sell, usize::MAX).len();
                depth.record(levels as u64, &symbol);
            }

            last = stats.clone();
            last_violations = checker.violation_count;
            last_publish = Instant::now();
        }

        if fragments == 0 {
            sleep(Duration::from_micros(200));
        }
    }
}
