# Starting SBX locally

How to install everything, start every service, and check that it works.

**Verified on 2026-11-03** on an Apple M4 running macOS 26.3.1: seeded orders went
through the engine, `level2`, NATS and `websocket`, and `trade-reporter` wrote them to QuestDB. On Linux,
the workspace compiles on Alpine (arm64), but the services haven't been run there.
Steps marked untested haven't been tried.

## Versions

| Tool | Tested version | Needed for |
|---|---|---|
| Rust | 1.99.1 (older versions fail: `rusteron` needs `std::hint::cold_path`) | Rust services |
| CMake | 4.4.5 (3.30 or newer required) | `rusteron` and the Aeron driver |
| Clang | Apple clang 17 | `rusteron` bindings and the Aeron driver |
| Aeron | 1.54.0 (must match `rusteron-client`) | Media driver |
| Node / npm | 23.11.0 / 10.9.2 | `auth`, `web`, `docs` |
| Python | 3, standard library only | `scripts/sbx.py` |
| Apple `container` | 1.6.1 | Infrastructure on macOS |
| NATS | `nats:2-alpine` | Market data bus |
| QuestDB | `questdb/questdb:8.2.3` | Time-series storage |
| Postgres | `postgres:16-alpine` | Auth database |
| Grafana LGTM | `grafana/otel-lgtm:latest` | Optional metrics viewer |

## 1. Install tools

**macOS**

```sh
xcode-select --install
brew install cmake rust node
```

Install Apple `container` from <https://github.com/apple/container/releases>, then run
`container system start`.

**Linux, Debian 13 or newer (untested)**

Debian 12's CMake is too old.

```sh
sudo apt-get update
sudo apt-get install -y build-essential clang libclang-dev cmake git curl pkg-config \
  uuid-dev libbsd-dev zlib1g-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup component add rustfmt
# Install Node 23 with your preferred version manager, and Docker Engine.
```

Homebrew also works on Linux, so `brew install cmake rust node` is an alternative.

**Alpine builds**

Tested in a `rust:alpine` image:

```sh
apk add build-base cmake clang-dev linux-headers util-linux-dev libbsd-dev zlib-dev perl pkgconf bash
rustup component add rustfmt
export RUSTFLAGS="-C target-feature=-crt-static"
```

`rusteron` needs `rustfmt` for its generated bindings. Without `RUSTFLAGS`, the build
fails with `Dynamic loading not supported` while loading libclang.

## 2. Build the Aeron driver

This also builds Aeron's own inspection tools.

```sh
cd ~/Projects
git clone --depth 1 --branch 1.54.0 https://github.com/aeron-io/aeron.git aeron
cd aeron
cmake -S . -B cppbuild/Release -DCMAKE_BUILD_TYPE=Release \
  -DAERON_TESTS=OFF -DAERON_SYSTEM_TESTS=OFF -DAERON_BUILD_SAMPLES=ON \
  -DBUILD_AERON_ARCHIVE_API=OFF -DAERON_BUILD_DOCUMENTATION=OFF
cmake --build cppbuild/Release --target aeronmd AeronStat ErrorStat LossStat DriverTool -j 8
```

Binaries land in `cppbuild/Release/binaries`. If you clone elsewhere, set `AERON_BIN`
for `scripts/sbx.py`.

## 3. Build the services

```sh
cd ~/Projects/sbx
cargo build --workspace --release
for app in auth web docs; do (cd "$app" && npm ci); done
```

## 4. Start everything

```sh
scripts/sbx.py up        # infrastructure, RAM disk, driver and all Rust services
scripts/sbx.py status
scripts/sbx.py logs matching-engine
scripts/sbx.py down      # add --all to remove the infrastructure containers too
```

Logs and PID files go in `.run/`. `sbx.py` runs these steps for you:

**Infrastructure**

```sh
container run -d --name sbx-nats -p 127.0.0.1:4222:4222 docker.io/library/nats:2-alpine
container run -d --name sbx-questdb -p 127.0.0.1:9000:9000 -p 127.0.0.1:9009:9009 docker.io/questdb/questdb:8.2.3
container run -d --name sbx-postgres -p 127.0.0.1:5432:5432 \
  -e POSTGRES_USER=sbx -e POSTGRES_PASSWORD=sbx -e POSTGRES_DB=sbx \
  docker.io/library/postgres:16-alpine
```

The same flags work with `docker run` (untested). These containers have no volumes, so
their data is lost when they're removed.

**Shared memory.** Aeron's clients and driver communicate through memory-mapped files.
macOS has no `/dev/shm`, so it needs a 2 GB RAM disk, which disappears on reboot:

```sh
diskutil erasevolume HFS+ DevShm `hdiutil attach -nomount ram://$((2048 * 2048))`
```

Linux uses `/dev/shm/aeron` automatically.

**Rust services, in this order**

```sh
export AERON_DIR=/Volumes/DevShm/aeron RUST_LOG=info   # macOS

../aeron/cppbuild/Release/binaries/aeronmd -Daeron.dir=$AERON_DIR -Daeron.threading.mode=SHARED

SUB_CHANNEL='aeron:udp?endpoint=localhost:40123' SUB_STREAM_ID=1001 \
PUB_CHANNEL='aeron:udp?control=localhost:40456|control-mode=dynamic' PUB_STREAM_ID=1002 \
MAX_ORDERS=10000000 target/release/matching-engine

target/release/level2
target/release/ticker
target/release/trade-reporter
target/release/observer
target/release/api
WS_ADDR=127.0.0.1:8090 target/release/websocket
```

**Order matters.** `level2` and `ticker` expect order IDs to start at 1 and keep their
book in memory, so start them before the first order, and restart them with `matching-engine`.

**This Mac:** nginx from the `gtfs` project owns port 8080. `websocket` can still bind
`127.0.0.1:8080` and silently take its localhost traffic, so `sbx.py` uses 8090 here.

### Configuration

| Variable | Used by | Default |
|---|---|---|
| `AERON_DIR` | All Aeron services | `/Volumes/DevShm/aeron` on macOS, `/dev/shm/aeron` on Linux |
| `SUB_CHANNEL`, `SUB_STREAM_ID` | `matching-engine` (required), `level2`, `ticker`, `trade-reporter` | See `transport/src/lib.rs` |
| `PUB_CHANNEL`, `PUB_STREAM_ID` | `matching-engine` (required), `api` | See `transport/src/lib.rs` |
| `MAX_ORDERS` | `matching-engine` (required), resting orders across all symbols | None |
| `NATS_URL` | `level2`, `ticker`, `websocket` | `localhost` |
| `QUESTDB_CONF` | `trade-reporter` | `http::addr=localhost:9000;` |
| `WS_ADDR` | `websocket` | `127.0.0.1:8080` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `observer` | `http://localhost:4318` |
| `RUST_LOG` | `matching-engine`, `api`, `trade-reporter` | `info` for `matching-engine` |

Execution reports use Aeron multi-destination-cast (MDC), where subscribers register
with the engine's control address. To run across nodes, replace `localhost` with the
engine's address in the control parameter, and with each subscriber's own address in
its `endpoint`. Between two VMs, 1,000,000 of 1,000,000 messages arrived, at 0.14M/sec
with default settings on 2-CPU VMs.

## 5. Start auth, web and docs

These need `scripts/sbx.py up` first, for Postgres.

```sh
# Auth on port 8001. Run from auth/.
export DATABASE_URL=postgres://sbx:sbx@localhost:5432/sbx \
  DOMAIN=localhost:5173 \
  BETTER_AUTH_URL=http://localhost:8001/api/auth \
  BETTER_AUTH_SECRET=$(openssl rand -hex 32) \
  DISCORD_CLIENT_ID=local-placeholder \
  DISCORD_CLIENT_SECRET=local-placeholder \
  HYPIXEL_API_KEY=local-placeholder
npx drizzle-kit push --force   # first run, and after schema changes
npm run dev

# Web on port 5173. Run from web/.
VITE_WS_URL=ws://localhost:8090 npm run dev

# Docs on port 5174. Run from docs/.
VITE_WS_URL=ws://localhost:8090 npx remix vite:dev --port 5174
```

The placeholder values let `auth` boot, but Discord login and in-game name verification
need real credentials.

`web` reads `VITE_DOMAIN`, `VITE_API_URL`, `VITE_AUTH_URL`, `VITE_WS_URL` and
`VITE_DOCS_URL`. `docs` reads `VITE_DOMAIN`, `VITE_API_URL` and `VITE_WS_URL`. They're
baked in at build time, so set them for `npm run build` too.

## 6. Check it works

`verify` restarts the engine, starts the invariant checker, sends seeded orders straight
to the engine, and reports violations and crashed services. It exits non-zero on either.

```sh
scripts/sbx.py verify --orders 20000 --rate 5000
```

Useful variations:

```sh
scripts/sbx.py verify --orders 1000000                  # as fast as possible
scripts/sbx.py verify --accounts 1                      # forces self-trade prevention
scripts/sbx.py verify --without tr,level2,ticker        # isolate a bottleneck
scripts/sbx.py verify --capture /tmp/run.jsonl          # keep every event
```

To send one order by hand:

```sh
curl -X POST localhost:8000/api/v1/orders -H 'content-type: application/json' \
  -d '{"product_id":"JSP","side":"buy","type":"limit","size":10,"price":100}'
```

## Tools

Run these from the repo root with `AERON_DIR` set. All take `--help`.

| Tool | Use |
|---|---|
| `target/release/sbx-tap` | Print messages live. Add `--stream orders` for the input stream, or `--json` to capture. |
| `target/release/sbx-check` | Check invariants live, or replay a capture with `--file`. The rules are listed at the top of `tools/src/check.rs`. |
| `target/release/sbx-book` | Live book and activity. |
| `target/release/sbx-load` | Seeded load. The same seed sends the same orders. |
| `~/Projects/aeron/cppbuild/Release/binaries/AeronStat` | Live driver counters, including back pressure and connection counts. Pass `-d $AERON_DIR`. It keeps refreshing until you stop it. |
| `~/Projects/aeron/cppbuild/Release/binaries/ErrorStat` | The driver's error log. |
| `~/Projects/aeron/cppbuild/Release/binaries/LossStat` | Loss reports. |

`sbx-check` and `sbx-book` rebuild state from every report, so start them before the
engine's first order. `sbx-check` needs to see the whole run.

## Observability

```sh
scripts/sbx.py observe     # Grafana, Prometheus and an OTLP collector
scripts/sbx.py up
```

Grafana is on <http://localhost:3000> and Prometheus on <http://localhost:9090>. On a
headless box, reach them with `ssh -L 3000:localhost:3000 <host>`. No dashboards exist
yet, so query metrics directly:

| Metric | Meaning |
|---|---|
| `sbx_orders_total`, `sbx_trades_total`, `sbx_canceled_total`, `sbx_rejected_total`, `sbx_cancel_rejects_total` | Activity |
| `sbx_filled_qty_total` | Filled quantity, counted for both sides of each trade |
| `sbx_violations_total` | Invariant violations; this should stay at 0 |
| `sbx_best_bid`, `sbx_best_ask`, `sbx_book_levels` | Book state |
| `sbx_observer_lag_seconds` | Time from the engine stamping a report to `observer` reading it |

`matching-engine` has no telemetry code. `observer` derives everything from its execution reports,
and currently keeps every order in memory, so it's a development tool for now.

**Engine logging.** `RUST_LOG` sets the level in normal builds, defaulting to `info`.
Release builds with `--features silent` compile out all logging, including errors:

```sh
cargo build -p matching-engine --release --features silent
```

With `RUST_LOG` set to a level above `off`, `tracing-subscriber` prints one warning at
startup about the static limit. Nothing is logged after that.

## Benchmarking

`scripts/bench.py` plays a tape of packed SBE orders into a freshly started engine, on bare
metal, in a release build with logging compiled out. Only the benchmark process
is attached. Stop anything else that's busy first, including `scripts/sbx.py down`.

```sh
scripts/bench.py                                # all profiles and modes, 5 repeats
scripts/bench.py --compare m4-mac-mini          # exit 1 on a regression
scripts/bench.py --save-baseline NAME           # write bench/baselines/NAME.json
scripts/bench.py --tape 100m --profile ipc --modes throughput --repeat 1
scripts/bench.py --engine path/to/binary        # benchmark another build
```

| Mode | Measures |
|---|---|
| `digest` | Hashes every report except timestamps, twice, and fails if they differ. Any change to the engine's behaviour changes this hash. |
| `throughput` | Plays the tape as fast as the engine accepts it. |
| `latency` | Sends 2M orders at a fixed rate and times each from its scheduled send to its `New` report, so a stall can't hide the queue behind it. |

| Profile | Transport |
|---|---|
| `ipc` | Shared memory on one machine, the closest to the engine's raw speed |
| `udp` | UDP orders and MDC reports, the default deployment |

`--compare` fails if the digest changes, median throughput falls more than 10%, or p99
latency rises more than 25%. A digest change isn't always wrong, but it means the engine's
output changed, so save a new baseline only if that was intended. Baselines only compare
fairly on the same machine.

Tapes live in `.run/tapes`, outside git. `bench/tapes.json` records each tape's generator
arguments and SHA-256, so identical bytes can be rebuilt (10M is 720 MB, 100M is 7.2 GB and
takes about 13 seconds). A tape is 72-byte records end to end, with no header.

Results on the M4 Mac mini, with the engine built as `dist` with `silent`:

| Transport | Orders/sec | Reports/sec | p50 | p99 | p99.99 |
|---|---|---|---|---|---|
| `ipc` | 4.04M | 11.15M | 208ns | 1.5µs | 0.9ms |
| `udp` | 0.73M | 2.00M | 14µs | 48µs | 1.7ms |

Latency is measured at 100K orders/sec. Throughput is the 10M tape, where each order creates
about 2.76 reports; the 100M tape reached 3.72M orders/sec.

`udp` throughput didn't change when the engine got faster, so something other than the engine
limits it. That hasn't been investigated.

Deployed builds of the engine should match the benchmark:

```sh
cargo build -p matching-engine --profile dist --features silent   # target/dist/matching-engine
```

The `dist` profile enables fat LTO. It's separate from `release` so normal rebuilds stay fast.

## What doesn't work yet

Confirmed with the tools above:

- **Subscribers slow the engine.** 1M orders at full speed reached about 151K orders/sec
  with every service, and 585K with only the checker. Each order produces about 2.7
  reports, and that run used a shared-mode driver with logging on.

Also known:

- **Any symbol creates a market.** The API only checks that it's alphanumeric and at most 6
  characters, and each new symbol allocates price levels that are never freed. Markets should
  come from configuration.

- `trade-reporter` skips filled orders, because it only saves `New`, `PartiallyFilled` and `Canceled`.
- `web`'s ticker expects `event.tickers`, but `websocket` sends every client `l2_data`.
- `websocket` doesn't complete the WebSocket close, so clients that wait for it hang.
- `GET /api/v1/orders` does nothing, and there's no cancel route. The engine cancels correctly,
  but a cancel must name the owning account, and the API has no auth to establish one.
- The API has no auth and invents a random account per order. It binds `0.0.0.0:8000`,
  and the Vite dev servers also bind `0.0.0.0`, so they're reachable from your LAN.

## Resource use

`matching-engine`, `level2`, `ticker` and `trade-reporter` poll in a tight loop, so each uses a full CPU core
even when idle. `observer`, `sbx-check` and `sbx-book` sleep when idle.

## Regenerating SBE codecs

Only needed after changing `messages.xml`. Not run during this setup, and it needs Java.

```sh
git clone https://github.com/aeron-io/simple-binary-encoding.git
cd simple-binary-encoding
./gradlew clean assemble
java -Dsbe.target.language=Rust -Dsbe.output.dir=<output dir> \
  -jar sbe-all/build/libs/sbe-all-<version>.jar ../sbx/messages.xml
```

Check how the output path maps onto `sbe/src/` before overwriting anything. The version
that produced the checked-in codecs isn't recorded.

## Containerizing the services (planned)

No service images exist yet.

- **Watch disk space.** Rust build images are large: a few test images filled about
  19 GB on this Mac. Check `df -h /` and remove old images with `container image delete`.
- **Choose how services share Aeron.** Each driver needs its clients on the same
  `/dev/shm`. Apple `container` gives each container its own VM, so a driver and its
  clients must share a container there. Docker on Linux and OrbStack run containers on
  one kernel, so a shared `/dev/shm` should work (untested).
- **Keep Rust rebuilds fast** with a dependency-only layer (for example `cargo-chef`)
  and a BuildKit cache mount for the cargo registry and `target/`.
