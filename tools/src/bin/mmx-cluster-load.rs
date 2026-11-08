use std::ffi::CString;
use std::fs;
use std::hint::spin_loop;
use std::time::{Duration, Instant};

use cluster::{ClusterClient, OfferResult};

use rusteron_archive as archive;

use tools::args::Args;
use tools::check::Checker;
use tools::event::{Event, ExecType, decode};
use tools::wire::NEW_ORDER_LENGTH;

const USAGE: &str = "mmx-cluster-load: play a tape through consensus, surviving leader changes

    --tape PATH    tape from mmx-tape (required)
    --limit N      orders to play (default all)
    --rate N       orders per second, 0 for as fast as possible (default 0)
    --nodes N      cluster members (default 3)
    --source N     node to read reports from (default 0)

Tracks every order to its report, dedupes reports on exec_id, and checks all invariants.";

const TERM_MARKER_TEMPLATE: u16 = 100;
const QUIET: Duration = Duration::from_millis(300);
const ROTATE_AFTER: Duration = Duration::from_secs(2);

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

fn archive_client() -> archive::Aeron {
    let context = archive::AeronContext::new().expect("failed to create a client context");
    context
        .set_dir(&CString::new(transport::aeron_dir()).expect("no nul"))
        .expect("invalid Aeron dir");
    context
        .set_error_handler(Some(|code: i32, message: &str| {
            eprintln!("archive client error {code}: {message}")
        }))
        .expect("failed to set the error handler");
    let aeron = archive::Aeron::new(&context).expect("failed to create a client");
    aeron.start().expect("failed to start the client");
    aeron
}

fn cstring(value: &str) -> CString {
    CString::new(value).expect("no nul")
}

/// Follows a node's recorded report stream from `position`, or `None` if the node is unreachable.
fn open_source(
    aeron: &archive::Aeron,
    node: usize,
    position: i64,
) -> Option<archive::AeronArchivePersistentSubscription> {
    let context = archive::AeronArchiveContext::new().ok()?;
    context.set_aeron(aeron).ok()?;
    context
        .set_control_request_channel(&cstring(&format!(
            "aeron:udp?endpoint={}:{}",
            host(node),
            8010 + node
        )))
        .ok()?;
    context
        .set_control_response_channel(&cstring(&format!("aeron:udp?endpoint={}:0", client_host())))
        .ok()?;

    let client = archive::AeronArchiveAsyncConnect::new_with_aeron(&context, aeron)
        .ok()?
        .poll_blocking(Duration::from_millis(400))
        .ok()?;
    let recording_id = client
        .find_last_matching_recording(0, &cstring(""), 1002, 1002)
        .ok()?;
    if recording_id < 0 {
        return None;
    }

    archive::PersistentSubscriptionBuilder::new_with_aeron(&context, aeron)
        .ok()?
        .live_channel(&format!(
            "aeron:udp?endpoint={}:0|control={}:{}|session-id=1002",
            client_host(),
            host(node),
            40450 + node
        ))
        .ok()?
        .live_stream_id(1002)
        .ok()?
        .replay_channel(&format!("aeron:udp?endpoint={}:0", client_host()))
        .ok()?
        .replay_stream_id(2002 + node as i32)
        .ok()?
        .start_position(position)
        .ok()?
        .recording_id(recording_id)
        .ok()?
        .build()
        .ok()
}

fn main() {
    let args = Args::parse(USAGE);
    let path = args.string("tape").expect("--tape is required");
    let rate: u64 = args.get("rate", 0);
    let nodes: usize = args.get("nodes", 3);
    let tape = fs::read(&path).expect("can't read the tape");
    let orders = args.get("limit", (tape.len() / NEW_ORDER_LENGTH) as u64) as usize;
    let period_ns = if rate > 0 { 1_000_000_000 / rate } else { 0 };

    let aeron = transport::connect(on_error).expect("failed to connect to the media driver");
    let archive_aeron = archive_client();
    let mut source = args.get("source", 0usize) % nodes;
    let mut position = 0i64;
    let mut reports_from = open_source(&archive_aeron, source, position);
    let mut rotated_at = Instant::now();
    let mut rotations = 0u64;

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
    .expect("failed to open a cluster session");
    eprintln!("session open, leader is member {:?}", client.leader());

    let mut checker = Checker::new();
    let mut last_exec_id = 0u64;
    let mut duplicates = 0u64;
    let mut gaps = 0u64;
    let mut reports = 0u64;
    let mut acknowledged = 0usize;
    let mut next = 0usize;
    let mut latencies: Vec<u64> = Vec::with_capacity(orders);
    let mut sent_at = vec![0u64; orders];
    let mut resent_below = 0usize;

    let started = Instant::now();
    let mut last_report = Instant::now();
    let mut changes_seen = client.leader_changes();
    let mut failovers: Vec<u128> = Vec::new();
    let mut marker_term = -1i64;
    let mut unresolved_since: Option<Instant> = None;
    let mut lost = 0u64;

    // Keep reading after the last order, because its trades arrive after its acknowledgement.
    while acknowledged < orders || last_report.elapsed() < QUIET {
        let now_ns = started.elapsed().as_nanos() as u64;

        if unresolved_since.is_none() {
            let mut burst = 0;
            while next < orders
                && burst < 64
                && (period_ns == 0 || now_ns >= next as u64 * period_ns)
            {
                let record = &tape[next * NEW_ORDER_LENGTH..(next + 1) * NEW_ORDER_LENGTH];
                if client.offer(record) != OfferResult::Sent {
                    break;
                }
                sent_at[next] = if period_ns == 0 {
                    now_ns
                } else {
                    next as u64 * period_ns
                };
                next += 1;
                burst += 1;
            }
        }

        if let Some(subscription) = &reports_from {
            let _ = subscription.poll_fn(
                |buffer, header| {
                    position = header.position();
                    if buffer.len() >= 16
                        && u16::from_le_bytes([buffer[2], buffer[3]]) == TERM_MARKER_TEMPLATE
                    {
                        marker_term =
                            i64::from_le_bytes(buffer[8..16].try_into().expect("eight bytes"));
                        return;
                    }
                    let event = decode(buffer);
                    let Event::Execution(report) = &event else {
                        return;
                    };
                    if report.exec_id <= last_exec_id {
                        duplicates += 1;
                        return;
                    }
                    if report.exec_id != last_exec_id + 1 {
                        gaps += report.exec_id - last_exec_id - 1;
                    }
                    last_exec_id = report.exec_id;
                    reports += 1;
                    last_report = Instant::now();
                    if matches!(report.exec_type, ExecType::New | ExecType::Rejected) {
                        let index = report.order_id as usize - 1;
                        acknowledged = acknowledged.max(index + 1);
                        if index >= resent_below {
                            let elapsed = started.elapsed().as_nanos() as u64;
                            latencies.push(elapsed.saturating_sub(sent_at[index]));
                        }
                    }
                    checker.apply(&event);
                },
                256,
            );
        }

        // A dead source goes quiet, so move to the next node at the same stream position.
        let failed = reports_from
            .as_ref()
            .is_none_or(|subscription| subscription.has_failed());
        let quiet = last_report.elapsed() > ROTATE_AFTER
            && (next > acknowledged || unresolved_since.is_some());
        if (failed || quiet) && rotated_at.elapsed() > ROTATE_AFTER {
            source = (source + 1) % nodes;
            rotations += 1;
            eprintln!("reading reports from node {source} at position {position}");
            reports_from = open_source(&archive_aeron, source, position);
            rotated_at = Instant::now();
        }

        client.poll_egress();
        client.keep_alive();

        // A leader change leaves in-flight orders unknown, so stop sending until the new term's
        // marker arrives. Every report before it is final, so orders with no report never reached
        // the log and resending them can't execute anything twice.
        if client.leader_changes() != changes_seen {
            changes_seen = client.leader_changes();
            unresolved_since.get_or_insert(last_report);
            eprintln!(
                "leader changed to {:?}, awaiting term {}",
                client.leader(),
                client.leadership_term()
            );
        }
        if let Some(since) = unresolved_since
            && marker_term >= client.leadership_term()
            && client.is_leader_connected()
        {
            failovers.push(since.elapsed().as_millis());
            eprintln!(
                "term {marker_term} resolved: resending from order {acknowledged}, {} were lost",
                next - acknowledged
            );
            lost += (next - acknowledged) as u64;
            resent_below = next.max(resent_below);
            next = acknowledged;
            unresolved_since = None;
        }

        spin_loop();
    }

    let elapsed = started.elapsed().as_secs_f64() - QUIET.as_secs_f64();
    checker.finish();
    latencies.sort_unstable();
    let at = |q: f64| {
        latencies
            .get(((latencies.len() as f64 * q) as usize).min(latencies.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    };
    println!(
        "orders={orders} reports={reports} seconds={elapsed:.3} orders_per_second={:.0} duplicates={duplicates} gaps={gaps} failovers_ms={failovers:?}",
        orders as f64 / elapsed
    );
    println!(
        "latency_ns p50={} p99={} p99.9={} p99.99={} max={}",
        at(0.5),
        at(0.99),
        at(0.999),
        at(0.9999),
        at(1.0)
    );
    println!(
        "trades={} violations={} last_exec_id={last_exec_id} source_rotations={rotations} resent={lost}",
        checker.stats.trades, checker.violation_count
    );
    for violation in checker.violations.iter().take(5) {
        println!(
            "VIOLATION {} exec={} order={}: {}",
            violation.rule, violation.exec_id, violation.order_id, violation.detail
        );
    }
}
