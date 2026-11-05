//! Test-run measurements, compiled only with the `diagnostics` feature. These read the clock on
//! every message, so they stay out of deployed builds. `DIGEST` hashes every report, `DIGEST_FILE`
//! writes the hash out so that nodes can be compared, and `TIMING_FILE` writes latency figures.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::publisher::Publisher;

/// Times messages on one clock: from the leader stamping them into the log to this node
/// finishing them, and matching alone.
struct Timing {
    path: String,
    matching: Vec<u64>,
    in_to_out: Vec<u64>,
    reported: usize,
    seen_at_last_check: usize,
}

impl Timing {
    fn record(&mut self, matching: u64, in_to_out: u64) {
        if self.matching.len() < self.matching.capacity() {
            self.matching.push(matching);
            self.in_to_out.push(in_to_out);
        }
    }

    fn report(&mut self) {
        if self.matching.len() == self.reported {
            return;
        }
        self.reported = self.matching.len();
        let line = |name: &str, values: &[u64]| {
            let mut sorted = values.to_vec();
            sorted.sort_unstable();
            let at = |q: f64| sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)];
            format!(
                "{name} n={} p50={} p99={} p99.9={} p99.99={} max={}\n",
                sorted.len(), at(0.5), at(0.99), at(0.999), at(0.9999), at(1.0)
            )
        };
        let warm_from = self.in_to_out.len() / 5;
        let mut worst: Vec<(u64, usize)> = self.in_to_out.iter().copied().zip(0..).collect();
        worst.sort_unstable_by(|a, b| b.cmp(a));
        // Stalls show up as runs of neighbours, so report only well separated ones.
        let mut stalls: Vec<(u64, usize)> = Vec::new();
        for (value, index) in worst {
            if stalls.iter().all(|(_, seen)| seen.abs_diff(index) > 20_000) {
                stalls.push((value, index));
                if stalls.len() == 6 {
                    break;
                }
            }
        }
        let text = line("matching_ns", &self.matching)
            + &line("in_to_out_ns", &self.in_to_out)
            + &line("warm_in_to_out_ns", &self.in_to_out[warm_from..])
            + &format!("stalls_us_at_index {:?}\n", stalls.iter().map(|(v, i)| (v / 1000, *i)).collect::<Vec<_>>());
        let _ = std::fs::write(&self.path, text);
    }
}

pub struct Diagnostics {
    timing: Option<Timing>,
    last_timing_report: Instant,
    digest_file: Option<String>,
    last_written: u64,
    last_report: Instant,
}

impl Diagnostics {
    pub fn from_env() -> Self {
        Self {
            timing: std::env::var("TIMING_FILE").ok().map(|path| Timing {
                path,
                matching: Vec::with_capacity(8_000_000),
                in_to_out: Vec::with_capacity(8_000_000),
                reported: 0,
                seen_at_last_check: 0,
            }),
            last_timing_report: Instant::now(),
            digest_file: std::env::var("DIGEST_FILE").ok(),
            last_written: 0,
            last_report: Instant::now(),
        }
    }

    pub fn message_started(&self) -> Option<Instant> {
        self.timing.is_some().then(Instant::now)
    }

    pub fn message_finished(&mut self, started: Option<Instant>, timestamp: i64) {
        if let (Some(timing), Some(started)) = (&mut self.timing, started) {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock is after the epoch").as_nanos() as i64;
            timing.record(started.elapsed().as_nanos() as u64, (now - timestamp).max(0) as u64);
        }
    }

    pub fn report(&mut self, publisher: &Publisher) {
        // Sorting takes long enough to stall matching, so it only runs once traffic has stopped.
        if self.last_timing_report.elapsed().as_secs() >= 2 {
            self.last_timing_report = Instant::now();
            if let Some(timing) = &mut self.timing {
                if timing.matching.len() == timing.seen_at_last_check {
                    timing.report();
                } else {
                    timing.seen_at_last_check = timing.matching.len();
                }
            }
        }

        if self.last_report.elapsed().as_millis() >= 100 {
            self.last_report = Instant::now();
            let state = publisher.state();
            if state.written != self.last_written {
                self.last_written = state.written;
                if let Some(path) = &self.digest_file {
                    let _ = std::fs::write(path, format!("{} {:016x}\n", state.written, state.digest));
                }
            }
        }
    }
}
