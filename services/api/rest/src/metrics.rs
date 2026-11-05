//! Counters for `/metrics`, in the Prometheus text format. The set is small and fixed, so it needs
//! no client library.

use super::errors::AppError;

use std::fmt::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

#[derive(Clone, Copy)]
enum Reason {
    Invalid,
    RateLimited,
    EngineRejected,
    RepeatOrder,
    OutcomeUnknown,
    Unavailable,
    Internal,
}

const REASONS: [(Reason, &str); 7] = [
    (Reason::Invalid, "invalid"),
    (Reason::RateLimited, "rate_limited"),
    (Reason::EngineRejected, "engine_rejected"),
    (Reason::RepeatOrder, "repeat_order"),
    (Reason::OutcomeUnknown, "outcome_unknown"),
    (Reason::Unavailable, "unavailable"),
    (Reason::Internal, "internal"),
];

fn reason(error: &AppError) -> Reason {
    match error {
        AppError::InvalidSide
        | AppError::InvalidOrderType
        | AppError::ValidationError(_)
        | AppError::JsonDeserializationError(_) => Reason::Invalid,
        AppError::RateLimited { .. } => Reason::RateLimited,
        AppError::OrderRejected(_) => Reason::EngineRejected,
        AppError::RepeatOrder(_) => Reason::RepeatOrder,
        AppError::OrderOutcomeUnknown(_) => Reason::OutcomeUnknown,
        AppError::RecorderBehind | AppError::HaltStateUnknown => Reason::Unavailable,
        _ => Reason::Internal,
    }
}

#[derive(Default)]
pub struct Metrics {
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

    pub fn render(&self) -> String {
        let mut text = String::new();
        let _ = writeln!(text, "# TYPE mmx_gateway_orders_accepted_total counter");
        let _ = writeln!(
            text,
            "mmx_gateway_orders_accepted_total {}",
            self.accepted.load(Ordering::Relaxed)
        );
        let _ = writeln!(text, "# TYPE mmx_gateway_orders_rejected_total counter");
        for (reason, name) in REASONS {
            let count = self.rejected[reason as usize].load(Ordering::Relaxed);
            let _ = writeln!(text, "mmx_gateway_orders_rejected_total{{reason=\"{name}\"}} {count}");
        }
        let _ = writeln!(text, "# TYPE mmx_gateway_halted_rejects_total counter");
        let _ = writeln!(
            text,
            "mmx_gateway_halted_rejects_total {}",
            self.halted_rejects.load(Ordering::Relaxed)
        );
        let _ = writeln!(text, "# TYPE mmx_gateway_orders_in_flight gauge");
        let _ = writeln!(text, "mmx_gateway_orders_in_flight {}", self.in_flight.load(Ordering::Relaxed));
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
            assert!(text.contains(&format!("{{reason=\"{name}\"}} 0\n")), "{name}");
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
    fn in_flight_falls_when_the_order_is_done() {
        let metrics = Metrics::default();
        let order = metrics.in_flight();
        assert!(metrics.render().contains("mmx_gateway_orders_in_flight 1\n"));
        drop(order);
        assert!(metrics.render().contains("mmx_gateway_orders_in_flight 0\n"));
    }
}
