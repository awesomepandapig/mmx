use std::time::{Duration, Instant};

use tools::args::Args;

const USAGE: &str = "sbx-jitter: measure how often the machine takes a core away, with no engine running

    --seconds N    how long to spin (default 30)

Spins reading the clock and counts every gap between two reads. Anything long is time the thread
didn't get: an interrupt, the scheduler or the hypervisor. Run it pinned to the core under test.";

fn main() {
    let args = Args::parse(USAGE);
    let seconds: u64 = args.get("seconds", 30);
    let limits_ns = [1_000u64, 5_000, 10_000, 50_000, 100_000, 1_000_000];
    let mut counts = [0u64; 6];
    let mut longest = 0u64;
    let mut reads = 0u64;

    let started = Instant::now();
    let end = started + Duration::from_secs(seconds);
    let mut last = started;
    while last < end {
        let now = Instant::now();
        let gap = (now - last).as_nanos() as u64;
        last = now;
        reads += 1;
        longest = longest.max(gap);
        for (limit, count) in limits_ns.iter().zip(counts.iter_mut()) {
            if gap >= *limit {
                *count += 1;
            }
        }
    }

    print!("{{\"seconds\":{seconds},\"reads\":{reads},\"longest_ns\":{longest}");
    for (limit, count) in limits_ns.iter().zip(counts.iter()) {
        print!(",\"over_{}us\":{count}", limit / 1000);
    }
    println!("}}");
}
