# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

SBX is an order-book exchange, built for Hypixel SkyBlock and being repositioned as MMX, a game-agnostic self-hosted exchange. See `STARTUP.md` for install steps, startup order and known gaps.

## Commands

```sh
cargo build --workspace --release          # all Rust services and tools
cargo build -p matching-engine             # one service
cargo test --workspace                     # tools and level2 have tests so far
cargo test -p tools flags_time_priority    # one test
cargo clippy --workspace                   # CI runs clippy with --all-features

tools/scripts/sbx.py up | down | status          # run the local stack
tools/scripts/sbx.py logs matching-engine        # follow a service's log
tools/scripts/sbx.py verify --orders 20000 --rate 5000   # seeded load + invariants + crash check
tools/scripts/bench.py --compare m4-mac-mini     # deterministic tape, fails on a regression
```

Run `tools/scripts/sbx.py verify` after changing `matching-engine`, `level2` or `ticker`. It restarts the engine, so don't run it against something you want to keep running.

Day to day, use `cargo test` and a short `verify`. Only run `tools/scripts/bench.py` when asked: it takes minutes, and it isn't part of the normal development loop. Its digest proves the engine's output didn't change, which helps for risky engine refactors. Baselines only compare fairly on the machine that made them, at the same tape and `--max-orders`. Other work on the machine skews throughput.

`auth`, `web` and `docs` are separate npm projects with no root workspace. `web` and `docs` have `npm run dev`, `build`, `lint`, `fmt` and `typecheck`. `auth` has `dev`, `build`, `lint`, `fmt` and `start`. `auth`'s `build` also runs `fmt`, which rewrites files.

## Architecture

```
api --(aeron udp, stream 1001: NewOrderSingle)--> matching-engine
matching-engine --(aeron MDC, stream 1002: ExecutionReport)--> level2, ticker, trade-reporter, observer, sbx-* tools
level2 --(NATS subject "l2.<symbol>")--> websocket --> browser
ticker --(NATS subject "ticker.<symbol>")--> nothing subscribes yet
trade-reporter --> QuestDB (tables are created on first write)
observer --(OTLP)--> collector
auth (better-auth + Express) --> Postgres; not connected to api yet
```

- `matching-engine` is single-threaded and in-memory, so all order and book state is lost on restart. It links no telemetry: `sbx-observer` derives metrics from its output, and `--features silent` compiles out logging and latency timing in release builds.
- `matching-engine` is the hot path, so it has no IO, HTTP or validation. `api` does all bouncing, including which symbols are enabled.
- The book is a BTree per symbol, for sorted prices, over one shared order pool, key map and price map. This design is deliberate. Levels can't outnumber resting orders, so `MAX_ORDERS` bounds memory for every symbol combined. The pool and maps are reserved and touched at boot, so nothing rehashes while matching.
- Trees are built the first time a symbol is seen and never freed, so new markets go live without a restart. A full book rejects orders and keeps running.
- The engine is authoritative for time: it stamps once per inbound message, on receipt. Matching never reads the time.
- The engine uses `foldhash` because SipHash cost about 20% of throughput. If clients ever choose order IDs, the keys become attacker-controlled, so keep it seeded and never expose map iteration order.
- Layout: `crates/` holds libraries (`sbe`, `transport`, `cluster`), `services/` holds what runs in production, and `tools/` holds developer binaries, `scripts/` and `bench/`.
- The architecture is game server -> gateway -> engine -> tape. The engine runs as an Aeron Cluster service (`services/matching-engine/src/clustered.rs`), with stock Java nodes beside it. Aeron Archive output is the authoritative record, byte-identical on every node, and everything else is a consumer that can lag. `tools/scripts/cluster.py` runs three local nodes.
- The `cluster` crate implements Aeron's internal service protocol, pinned to schema 17 of Aeron 1.54.0. Recheck it on every Aeron upgrade.
- The engine only matches and keeps no gateway state. At each new term it writes a marker to the report stream, and a gateway uses it to tell which in-flight orders landed.
- All Aeron code goes through the `transport` crate, built on `rusteron-client`. It wraps Aeron's C client, so the driver must match the bundled Aeron version (1.54.0).
- `rusteron` handles aren't `Send`. Create a client on the thread that uses it; `api` has one thread owning its publication. Don't enable `multi-threaded`, because it would make every service pay for atomic reference counts.
- Messages are SBE. `crates/sbe/messages.xml` is the schema and `crates/sbe/` is generated code, so don't edit `crates/sbe/src` by hand. Template IDs: 1 NewOrderSingle, 2 OrderCancelRequest, 3 ExecutionReport, 4 OrderCancelReject.
- Prices and quantities are `i64` mantissas with exponent -8. `i64::MIN` means null, such as the price on a market order.
- UUIDs travel as `[u64; 2]`, not text.
- `tools/src/check.rs` is the written-down contract for the engine's output. If a change alters what `matching-engine` promises, update the rules and tests there.

## Things that bite

- `level2` and `ticker` keep their own in-memory books and assert that order IDs arrive in sequence. Start them before the first order, and restart them together with `matching-engine`.
- `level2` and `ticker` panic on any template ID other than 3 or 4.
- Only new and rejected orders spend an order ID. Every report spends an exec ID, including cancel rejects.
- See "What doesn't work yet" in `STARTUP.md` for known gaps.
- `tracing`'s `max_level_*` features are ignored in release builds; use `release_max_level_*`.
- `level2` and `ticker` duplicate `messages.rs`, so changes need to be made in both.
- `tests/` is in `.gitignore`, so new top-level test directories won't show up in git without `-f`.
- Port 8080 is used by another project on the owner's Mac. `tools/scripts/sbx.py` uses 8090 there.
