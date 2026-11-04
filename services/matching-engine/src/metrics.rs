//! Per-order latency logging. `silent` builds replace this with no-ops, because it reads the
//! clock twice for every order.

#[cfg(not(feature = "silent"))]
pub use timed::LatencyMetrics;

#[cfg(feature = "silent")]
pub use untimed::LatencyMetrics;

#[cfg(not(feature = "silent"))]
mod timed {
    use std::collections::VecDeque;
    use std::time::Instant;

    use tracing::info;

    pub struct LatencyMetrics {
        total_messages: u64,
        total_latency_sum_ns: u128, // Use u128 for sum to prevent overflow with many messages
        recent_latencies_ns: VecDeque<u64>,
        window_size: usize,
    }

    impl LatencyMetrics {
        pub fn new(window_size: usize) -> Self {
            LatencyMetrics {
                total_messages: 0,
                total_latency_sum_ns: 0,
                recent_latencies_ns: VecDeque::with_capacity(window_size),
                window_size,
            }
        }

        pub fn start() -> Instant {
            Instant::now()
        }

        pub fn record(&mut self, started: Instant) {
            let delta_ns = started.elapsed().as_nanos() as u64;
            self.add_latency(delta_ns);

            // Print statistics periodically, e.g., every 1000 messages or so
            if self.total_messages % 1000 == 0 {
                info!(
                    target: "matching_engine",
                    total_messages = self.total_messages,
                    last_delta_ns = delta_ns,
                    recent_avg_ns = self.get_recent_average(),
                    total_avg_ns = self.get_total_average(),
                    "Processing latency metrics"
                );
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
}

#[cfg(feature = "silent")]
mod untimed {
    pub struct LatencyMetrics;

    impl LatencyMetrics {
        pub fn new(_window_size: usize) -> Self {
            Self
        }

        #[inline(always)]
        pub fn start() {}

        #[inline(always)]
        pub fn record(&mut self, _started: ()) {}
    }
}
