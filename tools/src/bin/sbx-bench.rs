use std::fs;
use std::hint::spin_loop;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use sbe::ReadBuf;
use sbe::exec_type_enum::ExecTypeEnum;
use sbe::execution_report_codec::ExecutionReportDecoder;
use sbe::message_header_codec::MessageHeaderDecoder;

use serde::Serialize;

use tools::args::Args;
use tools::wire::NEW_ORDER_LENGTH;

use transport::{AeronExclusivePublication, AeronOfferError, AeronSubscription};

const USAGE: &str = "sbx-bench: play a tape into the matching engine and measure what comes back

    --tape PATH               tape from sbx-tape (required)
    --mode MODE               throughput, latency or digest (default throughput)
    --rate N                  orders per second, for latency mode (default 100000)
    --limit N                 play only the first N orders (default all)
    --orders-channel CHANNEL  default aeron:ipc
    --exec-channel CHANNEL    default aeron:ipc

throughput  plays the tape as fast as the engine accepts it
latency     paces the tape and times each order from its scheduled send to its New report, so a
            stall can't hide the queue that builds behind it
digest      hashes every report except timestamps; any change in behaviour changes the hash

Prints one JSON object. The engine must be freshly started: order IDs identify tape records.";

const FRAGMENT_LIMIT: usize = 256;
const DRAIN: Duration = Duration::from_millis(100);
const TRANSACT_TIME_OFFSET: usize = 64;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01b3;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Throughput,
    Latency,
    Digest,
}

#[derive(Default)]
struct Received {
    reports: u64,
    first: Option<Instant>,
    last: Option<Instant>,
    latencies: Vec<u64>,
    digest: u64,
}

#[derive(Serialize)]
struct Output {
    mode: &'static str,
    orders: u64,
    reports: u64,
    seconds: f64,
    orders_per_second: f64,
    reports_per_second: f64,
    back_pressured: u64,
    rate: Option<u64>,
    latency_ns: Option<Percentiles>,
    digest: Option<String>,
}

#[derive(Serialize)]
struct Percentiles {
    p50: u64,
    p90: u64,
    p99: u64,
    p99_9: u64,
    p99_99: u64,
    max: u64,
}

fn on_error(code: i32, message: &str) {
    eprintln!("aeron error {code}: {message}");
}

fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Returns the report's order ID if it acknowledges a new order.
#[inline]
fn acknowledged_order(buffer: &[u8]) -> Option<u64> {
    let header: MessageHeaderDecoder<ReadBuf<'_>> =
        MessageHeaderDecoder::default().wrap(ReadBuf::new(buffer), 0);
    if header.template_id() != 3 {
        return None;
    }
    let report: ExecutionReportDecoder<'_> = ExecutionReportDecoder::default().header(header, 0);
    (report.exec_type() == ExecTypeEnum::New).then(|| report.order_id())
}

fn wait_for_engine(subscription: &AeronSubscription) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while subscription.image_at_index(0).is_none() {
        assert!(Instant::now() < deadline, "the matching engine never connected");
        thread::sleep(Duration::from_millis(1));
    }
}

fn receive(
    channel: &str,
    orders: u64,
    mode: Mode,
    period_ns: u64,
    ready: &AtomicBool,
    start: &std::sync::OnceLock<Instant>,
) -> Received {
    let aeron = transport::connect(on_error).expect("failed to connect to the media driver");
    let subscription = transport::subscription(&aeron, channel, transport::EXECUTIONS_STREAM)
        .expect("failed to subscribe to execution reports");
    wait_for_engine(&subscription);
    ready.store(true, Ordering::Release);

    let mut received = Received { digest: FNV_OFFSET, ..Received::default() };
    if mode == Mode::Latency {
        received.latencies.reserve_exact(orders as usize);
    }

    let mut acknowledged = 0;
    let mut copy = [0u8; 256];
    let mut idle_since = Instant::now();

    loop {
        let fragments = subscription
            .poll_fn(
                |buffer, _header| {
                    received.reports += 1;

                    if mode == Mode::Digest {
                        let length = buffer.len();
                        copy[..length].copy_from_slice(buffer);
                        if length >= TRANSACT_TIME_OFFSET + 8 {
                            copy[TRANSACT_TIME_OFFSET..TRANSACT_TIME_OFFSET + 8].fill(0);
                        }
                        received.digest = fnv1a(received.digest, &copy[..length]);
                    }

                    if let Some(order_id) = acknowledged_order(buffer) {
                        acknowledged += 1;
                        if mode == Mode::Latency {
                            let started = *start.get().expect("orders arrive after the start");
                            let scheduled = (order_id - 1) * period_ns;
                            let elapsed = started.elapsed().as_nanos() as u64;
                            received.latencies.push(elapsed.saturating_sub(scheduled));
                        }
                    }
                },
                FRAGMENT_LIMIT,
            )
            .expect("failed to poll");

        if fragments > 0 {
            let now = Instant::now();
            received.first.get_or_insert(now);
            received.last = Some(now);
            idle_since = now;
        } else if acknowledged == orders && idle_since.elapsed() > DRAIN {
            return received;
        } else {
            spin_loop();
        }
    }
}

fn offer(publication: &AeronExclusivePublication, message: &[u8]) -> u64 {
    let mut back_pressured = 0;
    loop {
        match publication.offer(message) {
            Ok(_) => return back_pressured,
            Err(AeronOfferError::BackPressured | AeronOfferError::AdminAction) => {
                back_pressured += 1;
                spin_loop();
            }
            Err(err) => panic!("failed to publish: {err}"),
        }
    }
}

fn percentiles(latencies: &mut [u64]) -> Percentiles {
    latencies.sort_unstable();
    let at = |quantile: f64| {
        let index = (latencies.len() as f64 * quantile) as usize;
        latencies[index.min(latencies.len() - 1)]
    };
    Percentiles {
        p50: at(0.5),
        p90: at(0.9),
        p99: at(0.99),
        p99_9: at(0.999),
        p99_99: at(0.9999),
        max: latencies[latencies.len() - 1],
    }
}

fn main() {
    let args = Args::parse(USAGE);
    let Some(path) = args.string("tape") else {
        eprintln!("--tape is required\n\n{USAGE}");
        std::process::exit(2);
    };
    let (mode, mode_name) = match args.string("mode").as_deref().unwrap_or("throughput") {
        "throughput" => (Mode::Throughput, "throughput"),
        "latency" => (Mode::Latency, "latency"),
        "digest" => (Mode::Digest, "digest"),
        other => {
            eprintln!("unknown mode {other:?}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let rate: u64 = args.get("rate", 100_000);
    let period_ns = 1_000_000_000 / rate;
    let orders_channel = args.string("orders-channel").unwrap_or_else(|| "aeron:ipc".to_string());
    let exec_channel = args.string("exec-channel").unwrap_or_else(|| "aeron:ipc".to_string());

    // Reading the whole file up front means playback never touches the disk.
    let tape = fs::read(&path).unwrap_or_else(|err| panic!("can't read {path}: {err}"));
    assert_eq!(tape.len() % NEW_ORDER_LENGTH, 0, "tape isn't a whole number of records");
    let orders = args.get("limit", (tape.len() / NEW_ORDER_LENGTH) as u64);
    let tape = &tape[..orders as usize * NEW_ORDER_LENGTH];

    let ready = Arc::new(AtomicBool::new(false));
    let start = Arc::new(std::sync::OnceLock::new());
    let receiver = {
        let (ready, start) = (Arc::clone(&ready), Arc::clone(&start));
        thread::Builder::new()
            .name("receiver".to_string())
            .spawn(move || receive(&exec_channel, orders, mode, period_ns, &ready, &start))
            .expect("failed to start the receiver")
    };

    let aeron = transport::connect(on_error).expect("failed to connect to the media driver");
    let publication =
        transport::exclusive_publication(&aeron, &orders_channel, transport::ORDERS_STREAM)
            .expect("failed to create the publication");

    let deadline = Instant::now() + Duration::from_secs(30);
    while !publication.is_connected() || !ready.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "the matching engine never connected");
        thread::sleep(Duration::from_millis(1));
    }

    let started = Instant::now();
    start.set(started).expect("start is only set once");
    let mut back_pressured = 0;

    for (i, message) in tape.chunks_exact(NEW_ORDER_LENGTH).enumerate() {
        if mode == Mode::Latency {
            let due = Duration::from_nanos(i as u64 * period_ns);
            while started.elapsed() < due {
                spin_loop();
            }
        }
        back_pressured += offer(&publication, message);
    }

    let mut received = receiver.join().expect("the receiver panicked");
    let last = received.last.expect("reports arrived");
    let seconds = last.duration_since(started).as_secs_f64();

    let output = Output {
        mode: mode_name,
        orders,
        reports: received.reports,
        seconds,
        orders_per_second: orders as f64 / seconds,
        reports_per_second: received.reports as f64 / seconds,
        back_pressured,
        rate: (mode == Mode::Latency).then_some(rate),
        latency_ns: (mode == Mode::Latency).then(|| percentiles(&mut received.latencies)),
        digest: (mode == Mode::Digest).then(|| format!("{:016x}", received.digest)),
    };
    println!("{}", serde_json::to_string(&output).expect("output serializes"));
}
