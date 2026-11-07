use std::io::{self, BufWriter, IsTerminal, Write};
use std::thread::sleep;
use std::time::Duration;

use tools::args::Args;
use tools::event::{Event, ExecType, decode, format_decimal};

const USAGE: &str = "mmx-tap: print messages from an Aeron stream as they happen

    --stream executions|orders   stream to read (default executions)
    --channel CHANNEL            override the Aeron channel
    --json                       print JSON lines, for capturing and replaying with mmx-check

Start it before the matching engine to capture a complete run.";

fn on_error(code: i32, message: &str) {
    eprintln!("aeron error {code}: {message}");
}

fn optional(value: Option<i64>) -> String {
    value.map_or_else(|| "-".to_string(), format_decimal)
}

fn describe(event: &Event) -> String {
    match event {
        Event::NewOrder {
            symbol,
            side,
            price,
            qty,
            ..
        } => {
            format!(
                "order  {symbol} {side:?} {} @ {}",
                format_decimal(*qty),
                optional(*price)
            )
        }
        Event::CancelRequest { orig_cl_ord_id, .. } => format!("cancel {orig_cl_ord_id}"),
        Event::Execution(r) => {
            let fill = match r.exec_type {
                ExecType::Trade => format!(
                    " filled {} @ {} match={}",
                    optional(r.last_qty),
                    optional(r.last_px),
                    r.match_id.unwrap_or(0)
                ),
                _ => String::new(),
            };
            format!(
                "exec={:<8} order={:<8} {:<9} {:<16} {} {:?} {} @ {} leaves={} cum={}{fill}",
                r.exec_id,
                r.order_id,
                format!("{:?}", r.exec_type),
                format!("{:?}", r.ord_status),
                r.symbol,
                r.side,
                format_decimal(r.ord_qty),
                optional(r.price),
                format_decimal(r.leaves_qty),
                format_decimal(r.cum_qty),
            )
        }
        Event::CancelReject { orig_cl_ord_id, .. } => format!("cancel rejected {orig_cl_ord_id}"),
        Event::Unknown { template_id } => format!("unknown template {template_id}"),
    }
}

fn main() {
    let args = Args::parse(USAGE);
    let stream = args
        .string("stream")
        .unwrap_or_else(|| "executions".to_string());
    let (default_channel, stream_id) = match stream.as_str() {
        "executions" => (
            transport::EXECUTIONS_SUB_CHANNEL,
            transport::EXECUTIONS_STREAM,
        ),
        "orders" => (transport::ORDERS_CHANNEL, transport::ORDERS_STREAM),
        other => {
            eprintln!("unknown stream {other:?}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let channel = args
        .string("channel")
        .unwrap_or_else(|| default_channel.to_string());
    let json = args.flag("json");

    let aeron = transport::connect(on_error).expect("failed to connect to the media driver");
    let subscription = transport::subscription(&aeron, &channel, stream_id)
        .expect("failed to subscribe to the stream");
    eprintln!("tapping {stream} on {channel}");

    let live = io::stdout().is_terminal();
    let mut out = BufWriter::new(io::stdout());

    loop {
        let fragments = subscription
            .poll_fn(
                |buffer, _header| {
                    let event = decode(buffer);
                    let line = if json {
                        serde_json::to_string(&event).expect("events serialize")
                    } else {
                        describe(&event)
                    };
                    if writeln!(out, "{line}").is_err() {
                        std::process::exit(0);
                    }
                },
                256,
            )
            .expect("failed to poll");

        if fragments == 0 {
            if out.flush().is_err() {
                std::process::exit(0);
            }
            sleep(Duration::from_micros(if live { 200 } else { 1000 }));
        }
    }
}
