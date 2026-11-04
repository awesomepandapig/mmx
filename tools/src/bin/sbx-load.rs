use std::hint::spin_loop;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sbe::ord_type_enum::OrdTypeEnum;
use sbe::side_enum::SideEnum;

use tools::args::Args;
use tools::event::SCALE;
use tools::wire::{
    CANCEL_LENGTH, CancelOrder, NEW_ORDER_LENGTH, NewOrder, encode_cancel, encode_new_order,
};

use transport::{AeronExclusivePublication, AeronOfferError};

const USAGE: &str = "sbx-load: send a repeatable stream of orders straight to the engine over Aeron

    --orders N        messages to send (default 100000)
    --rate N          messages per second, 0 for as fast as possible (default 0)
    --seed N          random seed; the same seed sends the same orders (default 1)
    --accounts N      distinct accounts; 1 forces self-trade prevention (default 100)
    --symbols LIST    comma-separated symbols, up to 6 characters each (default JSP)
    --mid N           center price (default 100)
    --spread N        prices vary this far either side of mid (default 5)
    --market-pct N    percent of orders that are market orders (default 10)
    --cancel-pct N    percent of messages that cancel an earlier order (default 0)";

/// xorshift64*: small, fast and reproducible, which is all a load generator needs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }
}

fn on_error(code: i32, message: &str) {
    eprintln!("aeron error {code}: {message}");
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos() as u64
}

fn send(publication: &AeronExclusivePublication, message: &[u8]) -> u64 {
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

fn symbol(name: &str) -> [u8; 6] {
    let mut symbol = [0u8; 6];
    assert!(name.len() <= 6, "symbol {name:?} is longer than 6 characters");
    symbol[..name.len()].copy_from_slice(name.as_bytes());
    symbol
}

fn main() {
    let args = Args::parse(USAGE);
    let orders: u64 = args.get("orders", 100_000);
    let rate: u64 = args.get("rate", 0);
    let accounts: u64 = args.get("accounts", 100);
    let mid: i64 = args.get("mid", 100);
    let spread: i64 = args.get("spread", 5);
    let market_pct: u64 = args.get("market-pct", 10);
    let cancel_pct: u64 = args.get("cancel-pct", 0);
    let symbols: Vec<[u8; 6]> = args
        .string("symbols")
        .unwrap_or_else(|| "JSP".to_string())
        .split(',')
        .map(symbol)
        .collect();
    let mut rng = Rng(args.get("seed", 1u64).max(1));

    let aeron = transport::connect(on_error).expect("failed to connect to the media driver");
    let channel = transport::env_or("ORDERS_CHANNEL", transport::ORDERS_CHANNEL);
    let publication = transport::exclusive_publication(&aeron, &channel, transport::ORDERS_STREAM)
        .expect("failed to create the publication");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !publication.is_connected() {
        assert!(Instant::now() < deadline, "the matching engine never connected");
        sleep(Duration::from_millis(5));
    }

    let mut sent = Vec::with_capacity(orders as usize);
    let mut new_order = [0u8; NEW_ORDER_LENGTH];
    let mut cancel = [0u8; CANCEL_LENGTH];
    let mut back_pressured = 0;
    let started = Instant::now();

    for i in 0..orders {
        if rate > 0 {
            let due = Duration::from_nanos(i * 1_000_000_000 / rate);
            while started.elapsed() < due {
                spin_loop();
            }
        }

        let cl_ord_id = [rng.next(), rng.next()];
        let account = [0, rng.below(accounts) + 1];
        let side = if rng.below(2) == 0 { SideEnum::Buy } else { SideEnum::Sell };
        let symbol = symbols[rng.below(symbols.len() as u64) as usize];

        if !sent.is_empty() && rng.below(100) < cancel_pct {
            let (orig_cl_ord_id, account, side): ([u64; 2], [u64; 2], SideEnum) =
                sent[rng.below(sent.len() as u64) as usize];
            encode_cancel(
                &CancelOrder {
                    orig_cl_ord_id,
                    cl_ord_id,
                    account,
                    symbol,
                    side,
                    transact_time: now_ns(),
                },
                &mut cancel,
            );
            back_pressured += send(&publication, &cancel);
            continue;
        }

        let is_market = rng.below(100) < market_pct;
        let offset = rng.below((spread * 2 + 1) as u64) as i64 - spread;
        let price = if is_market { i64::MIN } else { (mid + offset) * SCALE };
        let qty = (rng.below(100) as i64 + 1) * SCALE;

        encode_new_order(
            &NewOrder {
                cl_ord_id,
                account,
                symbol,
                side,
                ord_type: if is_market { OrdTypeEnum::Market } else { OrdTypeEnum::Limit },
                transact_time: now_ns(),
                qty,
                price,
            },
            &mut new_order,
        );
        back_pressured += send(&publication, &new_order);
        sent.push((cl_ord_id, account, side));
    }

    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "sent {orders} messages in {elapsed:.3}s ({:.3}M msgs/s), back pressured {back_pressured} times",
        orders as f64 / elapsed / 1_000_000.0
    );
}
