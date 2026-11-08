use std::fs::File;
use std::io::{BufWriter, Write};

use sbe::ord_type_enum::OrdTypeEnum;
use sbe::side_enum::SideEnum;

use sha2::{Digest, Sha256};

use tools::args::Args;
use tools::event::SCALE;
use tools::rng::Rng;
use tools::wire::{
    CANCEL_LENGTH, CancelOrder, NEW_ORDER_LENGTH, NewOrder, encode_cancel, encode_new_order,
};

const USAGE: &str = "mmx-tape: generate a benchmark tape of packed SBE NewOrderSingle messages

    --out PATH        file to write (required)
    --orders N        orders to generate (default 10000000)
    --seed N          random seed (default 1)
    --accounts N      distinct accounts (default 1000)
    --symbol NAME     symbol (default JSP)
    --mid N           center price (default 100)
    --spread N        prices vary this far either side of mid (default 5)
    --market-pct N    percent of orders that are market orders (default 10)
    --symbols N       distinct symbols (default 1)
    --cancel-pct N    percent of messages that cancel a recent order (default 0)
    --unknown-cancel-pct N  percent that cancel an order that never existed (default 0)
    --duplicate-pct N percent that repeat a recent order's ID (default 0)

A tape has no header: it is SBE messages end to end, and the same arguments always produce the
same bytes. Prints the tape's SHA-256 when done.";

/// Orders that cancels and repeats pick from.
const RECENT_ORDERS: usize = 4096;

#[derive(Clone, Copy)]
struct Recent {
    cl_ord_id: [u64; 2],
    account: [u64; 2],
    symbol: [u8; 6],
    side: SideEnum,
}

fn main() {
    let args = Args::parse(USAGE);
    let Some(path) = args.string("out") else {
        eprintln!("--out is required\n\n{USAGE}");
        std::process::exit(2);
    };
    let orders: u64 = args.get("orders", 10_000_000);
    let accounts: u64 = args.get("accounts", 1000);
    let mid: i64 = args.get("mid", 100);
    let spread: i64 = args.get("spread", 5);
    let market_pct: u64 = args.get("market-pct", 10);
    let name = args.string("symbol").unwrap_or_else(|| "JSP".to_string());
    let mut rng = Rng::new(args.get("seed", 1));

    let symbols: u64 = args.get("symbols", 1);
    let cancel_pct: u64 = args.get("cancel-pct", 0);
    let unknown_cancel_pct: u64 = args.get("unknown-cancel-pct", 0);
    let duplicate_pct: u64 = args.get("duplicate-pct", 0);
    let mixed = cancel_pct + unknown_cancel_pct + duplicate_pct > 0;
    assert!(
        cancel_pct + unknown_cancel_pct + duplicate_pct <= 100,
        "the percentages add up to more than 100"
    );

    let symbol_names: Vec<[u8; 6]> = (0..symbols)
        .map(|i| {
            if symbols == 1 {
                name.clone()
            } else {
                format!("S{i}")
            }
        })
        .map(|name| {
            let mut symbol = [0u8; 6];
            assert!(
                name.len() <= 6,
                "symbol {name:?} is longer than 6 characters"
            );
            symbol[..name.len()].copy_from_slice(name.as_bytes());
            symbol
        })
        .collect();

    let file = File::create(&path).unwrap_or_else(|err| panic!("can't create {path}: {err}"));
    let mut out = BufWriter::with_capacity(1 << 20, file);
    let mut hash = Sha256::new();
    let mut record = [0u8; NEW_ORDER_LENGTH];
    let mut cancel = [0u8; CANCEL_LENGTH];
    let mut recent: Vec<Recent> = Vec::with_capacity(RECENT_ORDERS);

    for i in 0..orders {
        let roll = if mixed { rng.below(100) } else { 100 };
        let repeat = roll >= cancel_pct + unknown_cancel_pct
            && roll < cancel_pct + unknown_cancel_pct + duplicate_pct;
        if roll < cancel_pct + unknown_cancel_pct && !recent.is_empty() {
            let target = recent[rng.below(recent.len() as u64) as usize];
            encode_cancel(
                &CancelOrder {
                    orig_cl_ord_id: if roll < cancel_pct {
                        target.cl_ord_id
                    } else {
                        [rng.next(), rng.next()]
                    },
                    cl_ord_id: [rng.next(), rng.next()],
                    account: target.account,
                    symbol: target.symbol,
                    side: target.side,
                    transact_time: i,
                },
                &mut cancel,
            );
            out.write_all(&cancel).expect("failed to write the tape");
            hash.update(cancel);
            continue;
        }

        let earlier =
            (repeat && !recent.is_empty()).then(|| recent[rng.below(recent.len() as u64) as usize]);
        let cl_ord_id = earlier.map_or_else(|| [rng.next(), rng.next()], |order| order.cl_ord_id);
        let account = earlier.map_or_else(|| [0, rng.below(accounts) + 1], |order| order.account);
        let side = if rng.below(2) == 0 {
            SideEnum::Buy
        } else {
            SideEnum::Sell
        };
        let is_market = rng.below(100) < market_pct;
        let offset = rng.below((spread * 2 + 1) as u64) as i64 - spread;
        let qty = (rng.below(100) as i64 + 1) * SCALE;
        let symbol = symbol_names[if symbols > 1 {
            rng.below(symbols) as usize
        } else {
            0
        }];

        encode_new_order(
            &NewOrder {
                cl_ord_id,
                account,
                symbol,
                side,
                ord_type: if is_market {
                    OrdTypeEnum::Market
                } else {
                    OrdTypeEnum::Limit
                },
                // Fixed rather than the wall clock, so the bytes never change.
                transact_time: i,
                qty,
                price: if is_market {
                    i64::MIN
                } else {
                    (mid + offset) * SCALE
                },
            },
            &mut record,
        );
        out.write_all(&record).expect("failed to write the tape");
        hash.update(record);

        if mixed && earlier.is_none() {
            let order = Recent {
                cl_ord_id,
                account,
                symbol,
                side,
            };
            if recent.len() < RECENT_ORDERS {
                recent.push(order);
            } else {
                recent[(i % RECENT_ORDERS as u64) as usize] = order;
            }
        }
    }
    out.flush().expect("failed to write the tape");

    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!("{digest}");
}
