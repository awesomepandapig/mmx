use std::io::{self, IsTerminal};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tools::args::Args;
use tools::check::Checker;
use tools::event::{Side, decode, format_decimal};

const USAGE: &str = "sbx-book: live order book and activity, rebuilt from execution reports

    --depth N        price levels per side (default 10)
    --interval MS    refresh interval (default 500)

Start it before the engine's first order, or the book will be incomplete.";

fn on_error(code: i32, message: &str) {
    eprintln!("aeron error {code}: {message}");
}

fn render(checker: &Checker, depth: usize, rate: f64) -> String {
    let stats = &checker.stats;
    let qty = |value: i128| format_decimal(i64::try_from(value).unwrap_or(i64::MAX));
    let mut out = format!(
        "orders {}  trades {}  canceled {}  rejected {}  {:.0} msgs/s  violations {}\n\n",
        stats.orders, stats.trades, stats.canceled, stats.rejected, rate, checker.violation_count
    );

    for symbol in checker.symbols() {
        out += &format!("{symbol}\n{:>14} {:>14} {:>7}\n", "PRICE", "QTY", "ORDERS");
        for (price, size, orders) in checker.top_of_book(&symbol, Side::Sell, depth).iter().rev() {
            out += &format!("{:>14} {:>14} {orders:>7}  ask\n", format_decimal(*price), qty(*size));
        }
        out += "  ------------------------------------\n";
        for (price, size, orders) in checker.top_of_book(&symbol, Side::Buy, depth) {
            out += &format!("{:>14} {:>14} {orders:>7}  bid\n", format_decimal(price), qty(size));
        }
        out += "\n";
    }

    for violation in checker.violations.iter().take(5) {
        out += &format!("\nVIOLATION {}: {}", violation.rule, violation.detail);
    }
    out
}

fn main() {
    let args = Args::parse(USAGE);
    let depth = args.get("depth", 10usize);
    let interval = Duration::from_millis(args.get("interval", 500));

    let aeron = transport::connect(on_error).expect("failed to connect to the media driver");
    let channel = transport::env_or("SUB_CHANNEL", transport::EXECUTIONS_SUB_CHANNEL);
    let subscription = transport::subscription(&aeron, &channel, transport::EXECUTIONS_STREAM)
        .expect("failed to subscribe to the stream");

    let live = io::stdout().is_terminal();
    let mut checker = Checker::new();
    let mut last_draw = Instant::now();
    let mut messages = 0u64;

    loop {
        let fragments = subscription
            .poll_fn(
                |buffer, _header| {
                    checker.apply(&decode(buffer));
                    messages += 1;
                },
                256,
            )
            .expect("failed to poll");

        if last_draw.elapsed() >= interval {
            let rate = messages as f64 / last_draw.elapsed().as_secs_f64();
            if live {
                print!("\x1b[2J\x1b[H");
            }
            println!("{}", render(&checker, depth, rate));
            messages = 0;
            last_draw = Instant::now();
        }

        if fragments == 0 {
            sleep(Duration::from_micros(200));
        }
    }
}
