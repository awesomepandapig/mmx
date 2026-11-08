use std::fs;
use std::hint::spin_loop;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use cluster::{ClusterClient, OfferResult};

use sbe::ReadBuf;
use sbe::exec_type_enum::ExecTypeEnum;
use sbe::execution_report_codec::ExecutionReportDecoder;
use sbe::message_header_codec::MessageHeaderDecoder;

use tools::args::Args;
use tools::wire::NEW_ORDER_LENGTH;

const USAGE: &str =
    "mmx-cluster-bench: measure consensus throughput and latency with minimal client cost

    --tape PATH    tape from mmx-tape (required)
    --limit N      orders to play (default all)
    --rate N       orders per second, 0 for as fast as possible (default 0)
    --batch N      orders packed into one cluster message (default 1)
    --nodes N      cluster members (default 3)
    --source N     node to read reports from (default 0)
    --warmup N     leave the first N orders out of the latency figures (default 0)
    --state-dir D  don't read reports; finish when engines' state files in D stop changing";

/// Member addresses, comma separated, or localhost when they all share a machine.
fn host(i: usize) -> String {
    std::env::var("CLUSTER_HOSTS")
        .ok()
        .and_then(|hosts| hosts.split(',').nth(i).map(str::to_string))
        .unwrap_or_else(|| "localhost".to_string())
}

fn client_host() -> String {
    std::env::var("CLIENT_HOST").unwrap_or_else(|_| "localhost".to_string())
}

fn on_error(code: i32, message: &str) {
    eprintln!("aeron error {code}: {message}");
}

/// Measures consensus plus matching alone, by watching how far each engine has applied the log.
fn no_reports(tape: &[u8], orders: usize, batch: usize, nodes: usize, dir: &str) {
    let aeron = transport::connect(on_error).expect("failed to connect");
    let endpoints: Vec<String> = (0..nodes)
        .map(|i| format!("{}:{}", host(i), 20000 + i))
        .collect();
    let mut client = ClusterClient::connect(
        &aeron,
        &endpoints,
        101,
        &format!("aeron:udp?endpoint={}:20300", client_host()),
        102,
    )
    .expect("failed to open a session");
    thread::sleep(Duration::from_millis(500));

    let started = Instant::now();
    let mut next = 0usize;
    let mut payload = vec![0u8; batch * NEW_ORDER_LENGTH];
    while next < orders {
        let count = batch.min(orders - next);
        let bytes = count * NEW_ORDER_LENGTH;
        payload[..bytes]
            .copy_from_slice(&tape[next * NEW_ORDER_LENGTH..(next + count) * NEW_ORDER_LENGTH]);
        while client.offer(&payload[..bytes]) != OfferResult::Sent {
            client.poll_egress();
            client.keep_alive();
            spin_loop();
        }
        next += count;
        if next % 4096 < batch {
            client.keep_alive();
            client.poll_egress();
        }
    }
    let send_seconds = started.elapsed().as_secs_f64();

    let read = |i: usize| -> u64 {
        fs::read_to_string(format!("{dir}/digest{i}"))
            .ok()
            .and_then(|s| s.split_whitespace().next().and_then(|n| n.parse().ok()))
            .unwrap_or(0)
    };
    let mut last = vec![0u64; nodes];
    let mut changed = vec![Instant::now(); nodes];
    loop {
        for i in 0..nodes {
            let now = read(i);
            if now != last[i] {
                last[i] = now;
                changed[i] = Instant::now();
            }
        }
        if changed
            .iter()
            .all(|at| at.elapsed() > Duration::from_millis(600))
        {
            break;
        }
        client.keep_alive();
        client.poll_egress();
        thread::sleep(Duration::from_millis(10));
    }
    let seconds: Vec<String> = changed
        .iter()
        .map(|at| format!("{:.2}", at.duration_since(started).as_secs_f64()))
        .collect();
    let slowest = changed
        .iter()
        .map(|at| at.duration_since(started).as_secs_f64())
        .fold(0.0, f64::max);
    println!(
        "orders/s={:.0} send_seconds={send_seconds:.2} applied_seconds={seconds:?} reports={last:?}",
        orders as f64 / slowest
    );
}

fn main() {
    let args = Args::parse(USAGE);
    let path = args.string("tape").expect("--tape is required");
    let tape = fs::read(&path).expect("can't read the tape");
    let orders = args.get("limit", (tape.len() / NEW_ORDER_LENGTH) as u64) as usize;
    let rate: u64 = args.get("rate", 0);
    let batch: usize = args.get("batch", 1);
    let nodes: usize = args.get("nodes", 3);
    let source: usize = args.get("source", 0);
    let warmup: u64 = args.get("warmup", 0);
    let period_ns = if rate > 0 { 1_000_000_000 / rate } else { 0 };
    let state_dir = args.string("state-dir");
    if let Some(dir) = state_dir {
        no_reports(&tape, orders, batch, nodes, &dir);
        return;
    }

    let acknowledged = Arc::new(AtomicU64::new(0));
    let reports = Arc::new(AtomicU64::new(0));
    let rejected = Arc::new(AtomicU64::new(0));
    let ready = Arc::new(AtomicBool::new(false));
    let start_ns = Arc::new(AtomicU64::new(0));
    let epoch = Instant::now();

    let receiver = {
        let (acknowledged, reports, rejected, ready, start_ns) = (
            acknowledged.clone(),
            reports.clone(),
            rejected.clone(),
            ready.clone(),
            start_ns.clone(),
        );
        thread::spawn(move || {
            let aeron = transport::connect(on_error).expect("failed to connect");
            let channel = format!(
                "aeron:udp?endpoint={}:0|control={}:{}|session-id=1002",
                client_host(),
                host(source),
                40450 + source
            );
            let subscription =
                transport::subscription(&aeron, &channel, 1002).expect("failed to subscribe");
            ready.store(true, Ordering::Release);

            let mut latencies = Vec::with_capacity(orders);
            let mut last_exec = 0u64;
            let mut done = 0u64;
            let mut finished_at = None;
            while done < orders as u64 {
                let _ = subscription.poll_fn(
                    |buffer, _header| {
                        let header: MessageHeaderDecoder<ReadBuf<'_>> =
                            MessageHeaderDecoder::default().wrap(ReadBuf::new(buffer), 0);
                        if header.template_id() != 3 {
                            return;
                        }
                        let report: ExecutionReportDecoder<'_> =
                            ExecutionReportDecoder::default().header(header, 0);
                        let exec_id = report.exec_id();
                        if exec_id <= last_exec {
                            return;
                        }
                        last_exec = exec_id;
                        reports.fetch_add(1, Ordering::Relaxed);
                        let is_rejected = report.exec_type() == ExecTypeEnum::Rejected;
                        if report.exec_type() == ExecTypeEnum::New || is_rejected {
                            done += 1;
                            acknowledged.store(done, Ordering::Relaxed);
                            if is_rejected {
                                rejected.fetch_add(1, Ordering::Relaxed);
                            }
                            if period_ns > 0 && report.order_id() > warmup {
                                let due = start_ns.load(Ordering::Relaxed)
                                    + (report.order_id() - 1) * period_ns;
                                latencies
                                    .push((epoch.elapsed().as_nanos() as u64).saturating_sub(due));
                            }
                        }
                    },
                    4096,
                );
                if done == orders as u64 {
                    finished_at = Some(Instant::now());
                }
            }
            (latencies, finished_at.expect("the run finished"))
        })
    };

    while !ready.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(1));
    }

    let aeron = transport::connect(on_error).expect("failed to connect");
    let endpoints: Vec<String> = (0..nodes)
        .map(|i| format!("{}:{}", host(i), 20000 + i))
        .collect();
    let mut client = ClusterClient::connect(
        &aeron,
        &endpoints,
        101,
        &format!("aeron:udp?endpoint={}:20300", client_host()),
        102,
    )
    .expect("failed to open a session");

    thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    start_ns.store(epoch.elapsed().as_nanos() as u64, Ordering::Release);
    let mut next = 0usize;
    let mut retries = 0u64;
    let mut payload = vec![0u8; batch * NEW_ORDER_LENGTH];

    while next < orders {
        let count = batch.min(orders - next);
        if period_ns > 0 {
            while (started.elapsed().as_nanos() as u64) < (next + count - 1) as u64 * period_ns {
                spin_loop();
            }
        }
        let bytes = count * NEW_ORDER_LENGTH;
        payload[..bytes]
            .copy_from_slice(&tape[next * NEW_ORDER_LENGTH..(next + count) * NEW_ORDER_LENGTH]);
        loop {
            match client.offer(&payload[..bytes]) {
                OfferResult::Sent => break,
                OfferResult::Retry => {
                    retries += 1;
                    client.poll_egress();
                    client.keep_alive();
                    spin_loop();
                }
            }
        }
        next += count;
        if next % 4096 < batch {
            client.keep_alive();
            client.poll_egress();
        }
    }
    let send_seconds = started.elapsed().as_secs_f64();

    while acknowledged.load(Ordering::Relaxed) < orders as u64 {
        client.keep_alive();
        client.poll_egress();
        thread::sleep(Duration::from_millis(1));
    }
    let (mut latencies, finished) = receiver.join().expect("the receiver panicked");
    let seconds = finished.duration_since(started).as_secs_f64();
    print!(
        "orders/s={:.0} reports/s={:.0} send_seconds={send_seconds:.2} seconds={seconds:.2} ingress_retries={retries} rejected={}",
        orders as f64 / seconds,
        reports.load(Ordering::Relaxed) as f64 / seconds,
        rejected.load(Ordering::Relaxed)
    );
    if !latencies.is_empty() {
        latencies.sort_unstable();
        let at =
            |q: f64| latencies[((latencies.len() as f64 * q) as usize).min(latencies.len() - 1)];
        print!(
            " p50={}us p99={}us p99.9={}us p99.99={}us max={}us samples={}",
            at(0.5) / 1000,
            at(0.99) / 1000,
            at(0.999) / 1000,
            at(0.9999) / 1000,
            at(1.0) / 1000,
            latencies.len()
        );
    }
    println!();
}
