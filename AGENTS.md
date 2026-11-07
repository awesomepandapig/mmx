# Repository Guidelines & Architecture (AGENTS.md)

This is the single authoritative source of truth and guidance for AI agents, developers, and contributors working in this repository.

MMX is an open-source, deterministic, price-time priority trading engine designed for game economies and virtual commodities. It is self-hosted on your own infrastructure (Kubernetes via Helm, Docker Compose, or native binaries), allowing game servers to retain complete ownership over player accounts, asset reservations, and settlement while MMX executes high-throughput order matching.

---

## 1. Project Structure & Module Organization

- **`crates/`** (Shared Rust libraries):
  - `sbe`: SBE message definitions and generated codecs. (Never edit `crates/sbe/src/` directly; edit `crates/sbe/messages.xml` and regenerate).
  - `transport`: High-performance networking wrapper around Aeron C client (`rusteron-client`).
  - `cluster`: Aeron Cluster consensus client and state replication (pinned to Aeron 1.54.0 schema 17).
  - `tape`: Journal format, frame readers, and replay utilities for Aeron Archive event streams.

- **`services/`** (Production services):
  - `matching-engine`: Pure in-memory, single-threaded matching core; runs as an Aeron Cluster replicated state machine.
  - `api` (`rest`): Ingress REST gateway handling validation, rate limits, symbol status, and order admission.
  - `market-data`: Stream processors aggregating L2 order book deltas, tickers, and candles onto NATS subjects.
  - `websocket`: Public streaming WebSocket server delivering market data to browsers and clients.
  - `outbox`: Durable webhook publisher delivering signed fill and execution reports to external game backends.
  - `trade-reporter`: Ingests execution reports and writes persistent trade records into QuestDB.
  - `mmx`: Unified operator daemon hosting the admin REST API, cluster management, and embedded web console (`console/dist`).

- **`tools/`** (Developer tools, benchmarks, and tests):
  - `scripts/mmx.py`: CLI harness to orchestrate the local stack (`up`, `down`, `status`, `logs`, `verify`).
  - `src/bin/`: Utility binaries (`mmx-bench`, `mmx-load`, `mmx-observer`, `mmx-tape`, `mmx-check`, etc.).
  - `browser/`: Playwright smoke tests for the landing page, demo, and documentation.

- **TypeScript Applications** (Independent npm packages, no root workspace):
  - **`web/`**: Public landing page and interactive simulated trading demo (Remix Vite, deployed on Vercel at `mmx.exchange`).
  - **`console/`**: Operator admin UI (React + Vite + Tailwind); embedded directly into the `mmx` binary at compile time.
  - **`docs/`**: Documentation site (Vocs static site generator, deployed on Vercel at `docs.mmx.exchange`).

- **`deploy/`**: Deployment configurations:
  - `helm/mmx`: Production Kubernetes Helm chart.
  - `compose.yaml`: Local multi-container Docker Compose stack.
  - `docker/`: Multi-stage Dockerfile and `docker-bake.hcl`.
  - `kind/`: Local Kubernetes cluster quickstart.
  - `vercel.md`: Vercel project deployment specifications for `web` and `docs`.

- **`auth/`** (Deprecated): Legacy authentication service, replaced by game server bearer delegation and `mmx` OIDC console auth. Untracked and excluded from builds.

---

## 2. Architecture & Data Flow

```text
Game server --(REST HTTP / JSON + x-player-id)--> api (gateway)
api --(Aeron UDP, stream 1001: NewOrderSingle)--> matching-engine (Aeron Cluster)
matching-engine --(Aeron MDC, stream 1002: ExecutionReport)--> [Aeron Archive / Tape]
                                                                        │
                 ┌──────────────────────────────────────────────────────┴──────────────────┐
                 ▼                                                      ▼                  ▼
              outbox                                               market-data       trade-reporter
                 │                                                      │                  │
         (signed webhooks)                                    (NATS subjects)          (QuestDB)
                 ▼                                                      ▼                  ▼
            Game server                                             websocket         Admin/Console
        (idempotent ledger)                                             │
                                                                        ▼
                                                                     Browser
```

### Core Invariants & Guarantees
1. **Zero IO on the Matching Hot Path**: The matching engine runs on a single thread entirely in memory. It does no IO, no HTTP parsing, and no runtime allocations after startup.
2. **Deterministic State Replay**: Aeron Archive output (the tape) is the byte-identical authoritative record across all cluster voters. QuestDB, NATS, and webhooks are derived views.
3. **Fixed-Point Quantities & Prices**: Prices and sizes are represented as `i64` fixed-point mantissas with an exponent of `-8` (e.g., `100.00` is represented as `100_00000000`). `i64::MIN` represents null (such as unpriced market orders).
4. **Player Identifiers**: Account IDs and player IDs are UUIDv7 formatted as `[u64; 2]` internally.
5. **Memory Bounds**: The order book is a BTree per symbol over a shared, pre-allocated order pool and price map. Total memory is strictly bounded by `MAX_ORDERS`. Symbols are initialized on first sight and never deallocated.
6. **Hashing**: Uses `foldhash` for map hashing. Keys are seeded and map iteration order must never be exposed or relied upon.
7. **Time Authority**: The engine stamps timestamps once upon inbound message arrival; matching operations never query system wall clocks.

---

## 3. Development Commands

### Rust Services & Tools
```sh
cargo build --workspace --release              # Build all Rust services and binaries
cargo build -p matching-engine                 # Build specific service
cargo test --workspace                         # Run all workspace unit & integration tests
cargo test -p matching-engine                  # Focused engine tests
cargo fmt --all -- --check                     # Check code formatting
cargo clippy --workspace --all-features        # Run clippy with full CI feature set
```

### Local Development Stack
```sh
tools/scripts/mmx.py up                        # Start local services
tools/scripts/mmx.py down                      # Tear down local services
tools/scripts/mmx.py status                    # Check health of local services
tools/scripts/mmx.py logs matching-engine      # Tail service logs
tools/scripts/mmx.py verify --orders 20000     # Seeded load + invariant + crash test
```
*Note*: Run `tools/scripts/mmx.py verify` after any change to `matching-engine` or `market-data`. It restarts the engine.

### TypeScript / Frontend Applications
Each frontend application (`web`, `console`, `docs`) is an independent package. Navigate to its directory to execute commands:
```sh
cd web     && npm run dev | build | lint | fmt | typecheck
cd console && npm run dev | build | lint | fmt | typecheck
cd docs    && npm run dev | build | typecheck
```
> [!IMPORTANT]
> **Compile `console/` before compiling `mmx`**: `services/mmx` embeds `console/dist` into the Rust binary at compile time. Always run `npm run build` in `console/` before building `mmx`.

---

## 4. Coding Style & Conventions

- **Rust**:
  - Follow standard Rust naming conventions (`snake_case` functions and modules, `PascalCase` types).
  - Use rustfmt with default 4-space indentation.
  - Keep tests in `src/tests.rs` with descriptive snake_case names.
  - SBE messages: update `crates/sbe/messages.xml` and run the generator script. Do not modify `crates/sbe/src` directly.
- **TypeScript & React**:
  - 2-space indentation, semicolons, double quotes matching project Prettier configuration.
  - `PascalCase` for React components; keep components focused and composable.
  - All new routes and UI features must pass `npm run typecheck` and ESLint checks.
- **Documentation & Copy**:
  - Technical writing style: direct, concise, and clear. Avoid fluff or ungrounded claims. "Less is more."
  - Always link commands and paths clearly.

---

## 5. Things That Bite & Operational Gotchas

1. **Market Data Streaming**: `market-data` threads read live Aeron streams. Without `TAPE_ARCHIVES`, they don't resume historical events, so start them before orders begin.
2. **Order ID vs. Exec ID**: New and rejected orders spend an Order ID. *Every* report spends an Exec ID, including cancel rejects.
3. **Console Browser Communication**: The admin console browser interface communicates exclusively with `mmx serve`, which proxies feeds and aggregates status.
4. **Port 8090 on macOS**: Port 8080 is frequently occupied on macOS developer machines; `tools/scripts/mmx.py` defaults the local gateway to port 8090.
5. **Tracing Levels**: Tracing crate's `max_level_*` features are ignored in release builds; use `release_max_level_*`.
6. **QuestDB Concurrency**: Only run one QuestDB recorder instance. Never run a concurrent history rebuild while the recorder pod is active.
7. **Rusteron Client Threading**: Aeron handles from `rusteron-client` are not `Send`. Clients must reside on the thread that uses them.

---

## 6. Commit & Pull Request Guidelines

- Use short, imperative commit subjects (e.g., `Add trade reporter health check`, `Reconcile agent guidelines`).
- Keep commits focused and logically grouped.
- Highlight schema, configuration, or deployment changes in PR descriptions.
