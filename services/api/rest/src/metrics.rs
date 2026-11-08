//! Counters for `/metrics`, in the Prometheus text format. The set is small and fixed, so it needs
//! no client library.

use super::errors::AppError;

use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

#[derive(Clone, Copy)]
enum Reason {
    Invalid,
    RateLimited,
    EngineRejected,
    DuplicateOrder,
    OutcomeUnknown,
    Unavailable,
    Internal,
}

const REASONS: [(Reason, &str); 7] = [
    (Reason::Invalid, "invalid"),
    (Reason::RateLimited, "rate_limited"),
    (Reason::EngineRejected, "engine_rejected"),
    (Reason::DuplicateOrder, "duplicate_order"),
    (Reason::OutcomeUnknown, "outcome_unknown"),
    (Reason::Unavailable, "unavailable"),
    (Reason::Internal, "internal"),
];

fn reason(error: &AppError) -> Reason {
    match error {
        AppError::InvalidSide
        | AppError::InvalidOrderType
        | AppError::ValidationError(_)
        | AppError::JsonDeserializationError(_)
        | AppError::InvalidOrderId(_)
        | AppError::StaleOrderId(_)
        | AppError::FutureOrderId(_) => Reason::Invalid,
        AppError::RateLimited { .. } => Reason::RateLimited,
        AppError::OrderRejected(_) => Reason::EngineRejected,
        AppError::DuplicateOrder(_) => Reason::DuplicateOrder,
        AppError::OrderOutcomeUnknown(_) => Reason::OutcomeUnknown,
        AppError::RecorderBehind
        | AppError::HaltStateUnknown
        | AppError::ClaimUnavailable(_)
        | AppError::ClaimsRecovering { .. } => Reason::Unavailable,
        _ => Reason::Internal,
    }
}

/// Upper bounds in microseconds. An accepted order waits on Valkey and the cluster's commit, so this
/// spans a fast local round trip up to the gateway's 3 second deadline.
const LATENCY_BUCKETS_US: [u64; 14] = [
    100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000,
    1_000_000, 3_000_000,
];

#[derive(Default)]
pub struct Metrics {
    latency_buckets: [AtomicU64; LATENCY_BUCKETS_US.len()],
    latency_count: AtomicU64,
    latency_sum_us: AtomicU64,
    accepted: AtomicU64,
    rejected: [AtomicU64; REASONS.len()],
    halted_rejects: AtomicU64,
    in_flight: AtomicI64,
}

/// Counts an order as in flight until dropped, which also covers a caller hanging up.
pub struct InFlight<'a>(&'a Metrics);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Metrics {
    pub fn in_flight(&self) -> InFlight<'_> {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        InFlight(self)
    }

    pub fn record_order<T>(&self, result: &Result<T, AppError>) {
        let counter = match result {
            Ok(_) => &self.accepted,
            Err(AppError::MarketHalted { .. }) => &self.halted_rejects,
            Err(error) => &self.rejected[reason(error) as usize],
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// How long an accepted order took, from arriving here to the engine's answer.
    pub fn record_latency(&self, took: Duration) {
        let micros = took.as_micros() as u64;
        if let Some(bucket) = LATENCY_BUCKETS_US.iter().position(|bound| micros <= *bound) {
            self.latency_buckets[bucket].fetch_add(1, Ordering::Relaxed);
        }
        self.latency_count.fetch_add(1, Ordering::Relaxed);
        self.latency_sum_us.fetch_add(micros, Ordering::Relaxed);
    }

    fn render_latency(&self, text: &mut String) {
        let _ = writeln!(text, "# TYPE mmx_gateway_order_latency_seconds histogram");
        let mut below = 0;
        for (bucket, bound) in LATENCY_BUCKETS_US.iter().enumerate() {
            below += self.latency_buckets[bucket].load(Ordering::Relaxed);
            let le = *bound as f64 / 1e6;
            let _ = writeln!(
                text,
                "mmx_gateway_order_latency_seconds_bucket{{le=\"{le}\"}} {below}"
            );
        }
        let count = self.latency_count.load(Ordering::Relaxed);
        let _ = writeln!(
            text,
            "mmx_gateway_order_latency_seconds_bucket{{le=\"+Inf\"}} {count}"
        );
        let sum = self.latency_sum_us.load(Ordering::Relaxed) as f64 / 1e6;
        let _ = writeln!(text, "mmx_gateway_order_latency_seconds_sum {sum}");
        let _ = writeln!(text, "mmx_gateway_order_latency_seconds_count {count}");
    }

    pub fn render(&self) -> String {
        let mut text = String::new();
        self.render_latency(&mut text);
        let _ = writeln!(text, "# TYPE mmx_gateway_orders_accepted_total counter");
        let _ = writeln!(
            text,
            "mmx_gateway_orders_accepted_total {}",
            self.accepted.load(Ordering::Relaxed)
        );
        let _ = writeln!(text, "# TYPE mmx_gateway_orders_rejected_total counter");
        for (reason, name) in REASONS {
            let count = self.rejected[reason as usize].load(Ordering::Relaxed);
            let _ = writeln!(
                text,
                "mmx_gateway_orders_rejected_total{{reason=\"{name}\"}} {count}"
            );
        }
        let _ = writeln!(text, "# TYPE mmx_gateway_halted_rejects_total counter");
        let _ = writeln!(
            text,
            "mmx_gateway_halted_rejects_total {}",
            self.halted_rejects.load(Ordering::Relaxed)
        );
        let _ = writeln!(text, "# TYPE mmx_gateway_orders_in_flight gauge");
        let _ = writeln!(
            text,
            "mmx_gateway_orders_in_flight {}",
            self.in_flight.load(Ordering::Relaxed)
        );
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_series_is_present_from_the_start() {
        let text = Metrics::default().render();
        assert!(text.contains("mmx_gateway_orders_accepted_total 0\n"));
        for (_, name) in REASONS {
            assert!(
                text.contains(&format!("{{reason=\"{name}\"}} 0\n")),
                "{name}"
            );
        }
        assert!(text.contains("mmx_gateway_halted_rejects_total 0\n"));
        assert!(text.contains("mmx_gateway_orders_in_flight 0\n"));
    }

    #[test]
    fn each_reason_lands_on_its_own_series() {
        let metrics = Metrics::default();
        metrics.record_order::<()>(&Err(AppError::RateLimited { retry_after_ms: 1 }));
        metrics.record_order::<()>(&Err(AppError::RecorderBehind));
        let text = metrics.render();
        assert!(text.contains("{reason=\"rate_limited\"} 1\n"));
        assert!(text.contains("{reason=\"unavailable\"} 1\n"));
        assert!(text.contains("{reason=\"invalid\"} 0\n"));
    }

    #[test]
    fn latency_buckets_are_cumulative() {
        let metrics = Metrics::default();
        metrics.record_latency(Duration::from_micros(80));
        metrics.record_latency(Duration::from_micros(2_000));
        metrics.record_latency(Duration::from_secs(9));
        let text = metrics.render();
        assert!(text.contains("_bucket{le=\"0.0001\"} 1\n"), "{text}");
        assert!(text.contains("_bucket{le=\"0.0025\"} 2\n"), "{text}");
        assert!(text.contains("_bucket{le=\"3\"} 2\n"), "{text}");
        assert!(text.contains("_bucket{le=\"+Inf\"} 3\n"), "{text}");
        assert!(
            text.contains("mmx_gateway_order_latency_seconds_count 3\n"),
            "{text}"
        );
    }

    #[test]
    fn in_flight_falls_when_the_order_is_done() {
        let metrics = Metrics::default();
        let order = metrics.in_flight();
        assert!(
            metrics
                .render()
                .contains("mmx_gateway_orders_in_flight 1\n")
        );
        drop(order);
        assert!(
            metrics
                .render()
                .contains("mmx_gateway_orders_in_flight 0\n")
        );
    }
}
