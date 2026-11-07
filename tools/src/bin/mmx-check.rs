use std::fs::File;
use std::io::{BufRead, BufReader};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tools::args::Args;
use tools::check::Checker;
use tools::event::{Event, decode, format_decimal};

const USAGE: &str = "mmx-check: verify the matching engine's invariants (see tools/src/check.rs)

    --file PATH    replay a capture from `mmx-tap --json` instead of reading live
    --idle SECS    stop after this long with no messages, once some have arrived (default 3)

Live mode must start before the engine's first order. Exits 1 if any invariant is violated.";

fn on_error(code: i32, message: &str) {
    eprintln!("aeron error {code}: {message}");
}

fn report(checker: &Checker) -> i32 {
    let stats = &checker.stats;
    println!(
        "orders={} rejected={} trades={} canceled={} cancel_rejects={}",
        stats.orders, stats.rejected, stats.trades, stats.canceled, stats.cancel_rejects
    );
    let qty = |value: i128| format_decimal(i64::try_from(value).unwrap_or(i64::MAX));
    println!(
        "submitted={} filled={} canceled={}",
        qty(stats.submitted_qty),
        qty(stats.filled_qty),
        qty(stats.canceled_qty)
    );

    for violation in &checker.violations {
        println!(
            "VIOLATION {} exec={} order={}: {}",
            violation.rule, violation.exec_id, violation.order_id, violation.detail
        );
    }

    if checker.is_clean() {
        println!("OK: all invariants held");
        0
    } else {
        println!("FAILED: {} violations", checker.violation_count);
        1
    }
}

fn replay(path: &str, checker: &mut Checker) {
    let file = File::open(path).unwrap_or_else(|err| panic!("can't open {path}: {err}"));
    for line in BufReader::new(file).lines() {
        let line = line.expect("failed to read capture");
        let event: Event = serde_json::from_str(&line).expect("invalid event in capture");
        checker.apply(&event);
    }
}

fn watch(idle: Duration, checker: &mut Checker) {
    let aeron = transport::connect(on_error).expect("failed to connect to the media driver");
    let channel = transport::env_or("SUB_CHANNEL", transport::EXECUTIONS_SUB_CHANNEL);
    let subscription = transport::subscription(&aeron, &channel, transport::EXECUTIONS_STREAM)
        .expect("failed to subscribe to the stream");
    eprintln!("watching {channel}");

    let mut seen = 0u64;
    let mut last_message = Instant::now();

    while seen == 0 || last_message.elapsed() < idle {
        let fragments = subscription
            .poll_fn(
                |buffer, _header| {
                    checker.apply(&decode(buffer));
                    seen += 1;
                },
                256,
            )
            .expect("failed to poll");

        if fragments > 0 {
            last_message = Instant::now();
        } else {
            sleep(Duration::from_micros(100));
        }
    }
}

fn main() {
    let args = Args::parse(USAGE);
    let mut checker = Checker::new();

    match args.string("file") {
        Some(path) => replay(&path, &mut checker),
        None => watch(Duration::from_secs(args.get("idle", 3)), &mut checker),
    }

    checker.finish();
    std::process::exit(report(&checker));
}
