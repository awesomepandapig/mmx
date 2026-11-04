# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

SBX is an order-book exchange, built for Hypixel SkyBlock and being repositioned as MMX, a game-agnostic self-hosted exchange. See `STARTUP.md` for install steps, startup order and known gaps.

## Commands

```sh
cargo build --workspace --release          # all Rust services and tools
cargo build -p matching-engine             # one service
cargo test --workspace                     # only tools has tests so far
cargo test -p tools flags_time_priority    # one test
cargo clippy --workspace                   # CI runs clippy with --all-features

scripts/sbx.py up | down | status          # run the local stack
scripts/sbx.py logs matching-engine        # follow a service's log
scripts/sbx.py verify --orders 20000 --rate 5000   # seeded load + invariants + crash check
```

Run `scripts/sbx.py verify` after changing `matching-engine`, `level2` or `ticker`. It restarts the engine, so don't run it against something you want to keep running.

`auth`, `web` and `docs` are separate npm projects with no root workspace. `web` and `docs` have `npm run dev`, `build`, `lint`, `fmt` and `typecheck`. `auth` has `dev`, `build`, `lint`, `fmt` and `start`. `auth`'s `build` also runs `fmt`, which rewrites files.

## Architecture

```
api --(aeron udp, stream 1001: NewOrderSingle)--> matching-engine
matching-engine --(aeron MDC, stream 1002: ExecutionReport)--> level2, ticker, trade-reporter, observer, sbx-* tools
level2 --(NATS subject "foo")--> websocket --> browser
ticker --(NATS subject "ticker:JSP")--> nothing subscribes yet
trade-reporter --> QuestDB (tables are created on first write)
observer --(OTLP)--> collector
auth (better-auth + Express) --> Postgres; not connected to api yet
```

- `matching-engine` is single-threaded and in-memory, so all order and book state is lost on restart. It links no telemetry: `observer` derives metrics from its output, and `--features silent` compiles out logging in release builds.
- All Aeron code goes through the `transport` crate, built on `rusteron-client`. It wraps Aeron's C client, so the driver must match the bundled Aeron version (1.54.0).
- `rusteron` handles aren't `Send`. Create a client on the thread that uses it; `api` has one thread owning its publication. Don't enable `multi-threaded`, because it would make every service pay for atomic reference counts.
- Messages are SBE. `messages.xml` is the schema and `sbe/` is generated code, so don't edit `sbe/src` by hand. Template IDs: 1 NewOrderSingle, 2 OrderCancelRequest, 3 ExecutionReport, 4 OrderCancelReject.
- Prices and quantities are `i64` mantissas with exponent -8. `i64::MIN` means null, such as the price on a market order.
- UUIDs travel as `[u64; 2]`, not text.
- `tools/src/check.rs` is the written-down contract for the engine's output. If a change alters what `matching-engine` promises, update the rules and tests there.

## Things that bite

- `level2` and `ticker` keep their own in-memory book and assert that order IDs arrive in sequence. Start them before the first order, and restart them together with `matching-engine`.
- `level2` and `ticker` panic on any template ID other than 3. Any rejected cancel crashes them.
- `matching-engine` has one book for all symbols. See "What doesn't work yet" in `STARTUP.md` for other confirmed bugs.
- `tracing`'s `max_level_*` features are ignored in release builds; use `release_max_level_*`.
- `level2` and `ticker` duplicate `messages.rs`, so changes need to be made in both.
- `old/` holds retired services. It's in `.gitignore` but its files are still tracked. `tests/` is ignored too, so new top-level test directories won't show up in git without `-f`.
- Port 8080 is used by another project on the owner's Mac. `scripts/sbx.py` uses 8090 there.
