use std::fs::File;
use std::io::{BufWriter, Write};

use sbe::ord_type_enum::OrdTypeEnum;
use sbe::side_enum::SideEnum;

use sha2::{Digest, Sha256};

use tools::args::Args;
use tools::event::SCALE;
use tools::rng::Rng;
use tools::wire::{NEW_ORDER_LENGTH, NewOrder, encode_new_order};

const USAGE: &str = "sbx-tape: generate a benchmark tape of packed SBE NewOrderSingle messages

    --out PATH        file to write (required)
    --orders N        orders to generate (default 10000000)
    --seed N          random seed (default 1)
    --accounts N      distinct accounts (default 1000)
    --symbol NAME     symbol (default JSP)
    --mid N           center price (default 100)
    --spread N        prices vary this far either side of mid (default 5)
    --market-pct N    percent of orders that are market orders (default 10)

A tape has no header: it is fixed-size records end to end, and the same arguments always produce
the same bytes. Prints the tape's SHA-256 when done.";

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

    let mut symbol = [0u8; 6];
    assert!(name.len() <= 6, "symbol {name:?} is longer than 6 characters");
    symbol[..name.len()].copy_from_slice(name.as_bytes());

    let file = File::create(&path).unwrap_or_else(|err| panic!("can't create {path}: {err}"));
    let mut out = BufWriter::with_capacity(1 << 20, file);
    let mut hash = Sha256::new();
    let mut record = [0u8; NEW_ORDER_LENGTH];

    for i in 0..orders {
        let cl_ord_id = [rng.next(), rng.next()];
        let account = [0, rng.below(accounts) + 1];
        let side = if rng.below(2) == 0 { SideEnum::Buy } else { SideEnum::Sell };
        let is_market = rng.below(100) < market_pct;
        let offset = rng.below((spread * 2 + 1) as u64) as i64 - spread;
        let qty = (rng.below(100) as i64 + 1) * SCALE;

        encode_new_order(
            &NewOrder {
                cl_ord_id,
                account,
                symbol,
                side,
                ord_type: if is_market { OrdTypeEnum::Market } else { OrdTypeEnum::Limit },
                // Fixed rather than the wall clock, so the bytes never change.
                transact_time: i,
                qty,
                price: if is_market { i64::MIN } else { (mid + offset) * SCALE },
            },
            &mut record,
        );
        out.write_all(&record).expect("failed to write the tape");
        hash.update(record);
    }
    out.flush().expect("failed to write the tape");

    let digest: String = hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    println!("{digest}");
}
