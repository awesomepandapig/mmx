# Starting SBX locally

How to install everything, start every service, and check that it works.

**Verified on 2026-11-05** on an Apple M4 running macOS 26.3.1, in cluster mode: orders placed
over REST traded on a three-node cluster, reached a game webhook with valid signatures, and showed
up in QuestDB, NATS and the websocket, including while the leader was killed. See
[Run on a cluster](#run-on-a-cluster). On Linux, the workspace compiles on Alpine (arm64), but the
services haven't been run there. Steps marked untested haven't been tried.

## Versions

| Tool | Tested version | Needed for |
|---|---|---|
| Rust | 1.99.1 (older versions fail: `rusteron` needs `std::hint::cold_path`) | Rust services |
| CMake | 4.4.5 (3.30 or newer required) | `rusteron` and the Aeron driver |
| Clang | Apple clang 17 | `rusteron` bindings and the Aeron driver |
| Aeron | 1.54.0 (must match `rusteron-client`) | Media driver |
| Node / npm | 23.11.0 / 10.9.2 | `auth`, `web`, `docs`, `console` |
| Python | 3, standard library only | `tools/scripts/sbx.py` |
| Java | OpenJDK 23 | Cluster nodes |
| Apple `container` | 1.6.1 | Infrastructure on macOS |
| NATS | `nats:2-alpine` | Market data bus |
| QuestDB | `questdb/questdb:8.2.3` | Time-series storage |
| Valkey | `valkey/valkey:8-alpine` | Per-player rate limits, in cluster mode |
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
for `tools/scripts/sbx.py`.

## 3. Build the services

```sh
cd ~/Projects/sbx
cargo build --workspace --release
cargo build -p matching-engine --profile dist --features silent   # cluster mode only
for app in auth web docs console; do (cd "$app" && npm ci); done
(cd console && npm run build)                                     # embedded in `mmx`, so before cargo
cargo build -p mmx --release
```

Without a `console/dist`, `mmx` still builds, and `mmx serve` says the console wasn't built.

Cluster mode also needs Aeron's Java nodes. Download them to `.run/`, or set `AERON_JAR`:

```sh
mkdir -p .run
curl -o .run/aeron-all-1.54.0.jar \
  https://repo1.maven.org/maven2/io/aeron/aeron-all/1.54.0/aeron-all-1.54.0.jar
```

## 4. Start everything

```sh
tools/scripts/sbx.py up        # infrastructure, RAM disk, driver and all Rust services
tools/scripts/sbx.py status
tools/scripts/sbx.py logs matching-engine
tools/scripts/sbx.py down      # add --all to remove the infrastructure containers too
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
macOS has no `/dev/shm`, so it needs a RAM disk, which disappears on reboot. `sbx.py` makes an
8 GB one if there's none. It only uses memory as it fills, and a standalone engine needs under 2 GB:

```sh
diskutil erasevolume HFS+ DevShm `hdiutil attach -nomount ram://$((8 * 1024 * 2048))`
```

Linux uses `/dev/shm/aeron` automatically.

**Rust services, in this order**

```sh
export AERON_DIR=/Volumes/DevShm/aeron RUST_LOG=info   # macOS

../aeron/cppbuild/Release/binaries/aeronmd -Daeron.dir=$AERON_DIR -Daeron.threading.mode=SHARED

SUB_CHANNEL='aeron:udp?endpoint=localhost:40123' SUB_STREAM_ID=1001 \
PUB_CHANNEL='aeron:udp?control=localhost:40456|control-mode=dynamic' PUB_STREAM_ID=1002 \
MAX_ORDERS=1000000 target/release/matching-engine

target/release/market-data
target/release/trade-reporter
target/release/sbx-observer
target/release/api
WS_ADDR=127.0.0.1:8090 target/release/websocket
```

**Order matters.** Without `TAPE_ARCHIVES`, `market-data` reads the live stream with no resume,
so start it before the first order, and restart it with `matching-engine`.

**This Mac:** nginx from the `gtfs` project owns port 8080. `websocket` can still bind
`127.0.0.1:8080` and silently take its localhost traffic, so `sbx.py` uses 8090 here.

### Configuration

| Variable | Used by | Default |
|---|---|---|
| `AERON_DIR` | All Aeron services | `/Volumes/DevShm/aeron` on macOS, `/dev/shm/aeron` on Linux |
| `SUB_CHANNEL`, `SUB_STREAM_ID` | `matching-engine` (required), `market-data`, `trade-reporter` | See `crates/transport/src/lib.rs` |
| `PUB_CHANNEL`, `PUB_STREAM_ID` | `matching-engine` (required), `api` | See `crates/transport/src/lib.rs` |
| `MAX_ORDERS` | `matching-engine` (required), resting orders across all symbols | None |
| `NATS_URL` | `market-data`, `websocket` | `localhost` |
| `QUESTDB_CONF` | `trade-reporter` | `http::addr=localhost:9000;` |
| `WS_ADDR` | `websocket` | `127.0.0.1:8080` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `sbx-observer` | `http://localhost:4318` |
| `RUST_LOG` | `matching-engine`, `api`, `trade-reporter` | `info` for `matching-engine` |

Execution reports use Aeron multi-destination-cast (MDC), where subscribers register
with the engine's control address. To run across nodes, replace `localhost` with the
engine's address in the control parameter, and with each subscriber's own address in
its `endpoint`. Between two VMs, 1,000,000 of 1,000,000 messages arrived, at 0.14M/sec
with default settings on 2-CPU VMs.

## Run on a cluster

This runs the system as it's designed to run: game server, gateway, three-node Aeron Cluster, tape,
and every consumer. It doesn't need Postgres, and it doesn't start `sbx-observer`.

```sh
tools/scripts/sbx.py up --cluster          # carries on from the tape if there is one
tools/scripts/sbx.py up --cluster --fresh  # wipes the tape and everything built from it
tools/scripts/sbx.py status
tools/scripts/sbx.py logs outbox           # also engine0-2, java0-2 and clientdriver
tools/scripts/sbx.py e2e                   # follow real trades to a webhook, with a failover
tools/scripts/sbx.py down                  # add --all to remove the infrastructure containers too
```

`tools/scripts/cluster.py` runs the nodes, and `sbx.py` adds the rest:

| What | Where |
|---|---|
| Client Aeron driver | `AERON_DIR=/Volumes/DevShm/cluster-client` |
| Console | <http://127.0.0.1:8093>, served by `mmx serve` |
| Gateway | <http://localhost:8000>, with markets from `.run/mmx/markets.yaml` |
| Websocket | `ws://127.0.0.1:8090` |
| Outbox | Settings in `tools/local/outbox.env`, then `.run/mmx/outbox.env` if the console saved any, journal in `.run/outbox`, status on <http://127.0.0.1:8092/position> |
| QuestDB | `sbx-cluster-questdb`, console on <http://localhost:9100> |
| Valkey | `sbx-valkey` on port 6379 |
| NATS | `sbx-nats` on port 4222, shared with standalone mode |

The tape is on disk under `.run/cluster`, so stopping and starting the stack keeps resting orders,
ID sequences and undelivered webhooks. QuestDB has no volume, but it's rebuilt from the tape.

QuestDB has its own container because tables made by older builds can't take the current schema,
and `CREATE TABLE IF NOT EXISTS` leaves them as they are. `sbx-questdb` is untouched by cluster mode.

The webhook URL has nothing listening on it except during `e2e`. That's safe: the outbox holds every
event and retries, backing off to 30 seconds. So the first `e2e` delivery can take that long, and it
also receives whatever built up since the last run.

### The console demo

Create a market in the console, send it orders from a small Python client, and watch them arrive.

```sh
tools/scripts/sbx.py up --cluster         # prints the console's address and first password
target/release/mmx webhook listen --port 8095 --secret local-development-secret   # optional game
```

1. Open <http://127.0.0.1:8093>, sign in with the first password and choose a real one. The first
   password is only printed on the first start. `MMX_ADMIN_TOKEN=dev target/release/mmx auth
   reset-password` issues another.
2. Under Markets, choose New market, and create `IRON` with a tick size of `0.01`.
3. Open Activity, then in another terminal trade between two players:

   ```sh
   tools/demo/mmx_client.py --symbol IRON trade
   ```

`MMX_ADMIN_TOKEN` is `dev` here unless you set another. Console edits go to `.run/mmx`, so they don't
touch `tools/local`, which only provides the first copy. `rm -rf .run/mmx` starts the console from
scratch, including its password. `tools/scripts/sbx.py restart outbox` applies a saved webhook.

`console/` has `npm run dev` on port 5190, which proxies the API. Set `MMX_URL` to use a server other
than `http://127.0.0.1:8093`.

To send orders by hand, name a player and a unique `client_order_id`. Both are UUIDs:

```sh
curl -X POST localhost:8000/api/v1/orders -H 'content-type: application/json' \
  -H 'x-player-id: 11111111-1111-4111-8111-111111111111' \
  -d '{"client_order_id":"aaaaaaaa-0000-4000-8000-000000000001","product_id":"ITEM",
       "side":"sell","type":"limit","size":"10","price":"100"}'
```

Settings that `sbx.py` chooses here:

| Setting | Value | Why |
|---|---|---|
| `LEADER_HEARTBEAT_TIMEOUT`, election timeout | `500ms` | A failover then fits inside the gateway's 3-second hold, at about 1.3 seconds here |
| `aeron.image.liveness.timeout` on the client driver | `2s` | Consumers notice a dead node sooner than the default 10 seconds |
| `RETRY_WINDOW_SECS` | `0` | Callers don't retry. With the check on, new orders wait on the recorder for the gateway's first 5 minutes, so a node loss bounces them with `recorder_behind` |

### What `e2e` checks

It needs a running stack, and exits non-zero with a message on the first failure. It's safe to run
again and again, and it cancels anything an earlier failed run left resting.

1. A resting sell and a crossing buy, placed over REST with `client_order_id`, are both accepted.
2. Both fill webhooks arrive with valid signatures. The legs share `match_id`, and each `fill_id` is
   its own 32 hex characters.
3. `GET` shows both orders through QuestDB.
4. Cancelling the rest gives a `canceled` webhook that releases the rest of the order.
5. Level 2, ticker and candle updates appear on NATS and the websocket.
6. It kills the leader while 60 orders are flowing, then places another. Every order must be answered.
7. Each order's webhooks must match how it ended, with none missing and no `fill_id` delivered twice.
8. Trading, webhooks and market data still work, and the killed node catches up with the leader.

`tools/scripts/sbx.py e2e --kill 0` kills node 0 instead. Consumers start on node 0, so this is the
one that makes them move. Stopping the whole stack and bringing it back isn't part of `e2e`.

## 5. Start auth, web and docs

These need `tools/scripts/sbx.py up` first, for Postgres.

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
tools/scripts/sbx.py verify --orders 20000 --rate 5000
```

Useful variations:

```sh
tools/scripts/sbx.py verify --orders 1000000                  # as fast as possible
tools/scripts/sbx.py verify --accounts 1                      # forces self-trade prevention
tools/scripts/sbx.py verify --without tr,market-data        # isolate a bottleneck
tools/scripts/sbx.py verify --capture /tmp/run.jsonl          # keep every event
```

To send one order by hand, with `ITEM` enabled in `services/api/rest/markets.yaml`:

```sh
curl -X POST localhost:8000/api/v1/orders -H 'content-type: application/json' \
  -H 'x-player-id: 11111111-1111-4111-8111-111111111111' \
  -d '{"product_id":"ITEM","side":"buy","type":"limit","size":"10","price":"100"}'
```

`verify` starts `trade-reporter` too, so on a QuestDB with tables from an older build, add
`--without trade-reporter`. `sbx-questdb` on this Mac has such an `orders` table. The cluster check
is under [Run on a cluster](#run-on-a-cluster).

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
tools/scripts/sbx.py observe     # Grafana, Prometheus and an OTLP collector
tools/scripts/sbx.py up
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
| `sbx_observer_lag_seconds` | Time from the engine stamping a report to `sbx-observer` reading it |

`matching-engine` has no telemetry code. `sbx-observer` derives everything from its execution reports,
and currently keeps every order in memory, so it's a development tool for now.

**Engine logging.** `RUST_LOG` sets the level in normal builds, defaulting to `info`.
Release builds with `--features silent` compile out all logging, including errors:

```sh
cargo build -p matching-engine --release --features silent
```

With `RUST_LOG` set to a level above `off`, `tracing-subscriber` prints one warning at
startup about the static limit. Nothing is logged after that.

## Benchmarking

`tools/scripts/bench.py` plays a tape of packed SBE orders into a freshly started engine, on bare
metal, in a `dist` build with logging compiled out. Only the benchmark process is attached. Stop anything else that's busy first, including `tools/scripts/sbx.py down`.

```sh
tools/scripts/bench.py                                # all profiles and modes, 5 repeats
tools/scripts/bench.py --compare m4-mac-mini          # exit 1 on a regression
tools/scripts/bench.py --save-baseline NAME           # write tools/bench/baselines/NAME.json
tools/scripts/bench.py --tape 100m --max-orders 20000000 --profile ipc --modes throughput --repeat 1
tools/scripts/bench.py --engine path/to/binary        # benchmark another build
```

| Mode | Measures |
|---|---|
| `digest` | Hashes every report except timestamps, twice, and fails if they differ. Any change to the engine's behaviour changes this hash. |
| `throughput` | Plays the tape as fast as the engine accepts it. |
| `latency` | Sends 2M orders at a fixed rate and times each from its scheduled send to its first report, so a stall can't hide the queue behind it. |

| Profile | Transport |
|---|---|
| `ipc` | Shared memory on one machine, the closest to the engine's raw speed |
| `udp` | UDP orders and MDC reports, the default deployment |

`--compare` fails if the digest changes, median throughput falls more than 10%, or p99
latency rises more than 25%. A digest change isn't always wrong, but it means the engine's
output changed, so save a new baseline only if that was intended. Baselines only compare
fairly on the same machine.

Tapes live in `.run/tapes`, outside git. `tools/bench/tapes.json` records each tape's generator
arguments and SHA-256, so identical bytes can be rebuilt (10M is 720 MB, 100M is 7.2 GB and
takes about 13 seconds). A tape is 72-byte records end to end, with no header.

Results on the M4 Mac mini, on the `5m` tape with `MAX_ORDERS` at 1M:

| Transport | Orders/sec | Reports/sec | p50 | p99 | p99.99 |
|---|---|---|---|---|---|
| `ipc` | 3.87M | 10.69M | 250ns | 1.5µs | 8.6µs |
| `udp` | 0.74M | 2.03M | 15µs | 45µs | 3.1ms |

Latency is measured at 100K orders/sec. Each order creates about 2.76 reports. The 100M tape
reached 3.41M orders/sec in one run with `--max-orders 20000000`.

`--max-orders` changes the result, so a baseline can only be compared with a run at the same
setting, on the same tape. Throughput and latency runs fail if any order is rejected, because
rejects skip matching and would inflate the score. Digest runs allow them, which means a
digest belongs to one tape and one `--max-orders` value.

`udp` throughput didn't change when the engine got faster, so something other than the engine
limits it. That hasn't been investigated.

Deployed builds of the engine should match the benchmark:

```sh
cargo build -p matching-engine --profile dist --features silent   # target/dist/matching-engine
```

The `dist` profile enables fat LTO. It's separate from `release` so normal rebuilds stay fast.

## Capacity and shared markets

One engine hosts every symbol by default. That's the simplest deployment and uses the least
memory and CPU, but markets share two limits:

- **Throughput.** The engine is single-threaded, so a busy market reduces what the others can
  use.
- **`MAX_ORDERS`.** It counts resting orders across all symbols. A market that fills the book
  causes rejects for every market on that engine.

The engine doesn't police either one. It has no validation by design, so limits per market
belong in `api`.

`MAX_ORDERS` sets the memory budget, which the engine reserves and touches at boot, so a
machine that's too small fails at startup instead of while matching. A full book rejects new
orders and keeps running.

| `MAX_ORDERS` | Memory at boot | Worst-case peak | Boot cost | Orders/sec (scratch run) |
|---|---|---|---|---|
| 1M (default) | 529 MB | 742 MB | 47ms | 3.85M |
| 5M | 1.7 GB | 2.0 GB | 248ms | 3.65M |
| 10M | 3.1 GB | 3.5 GB | 679ms | 3.55M |

About 200 MB of each figure is Aeron's buffers. The worst case is every resting order at a
unique price. Larger books are slower because they spread keys across more memory.

New markets don't need a restart. The engine builds trees the first time it sees a symbol and
never frees them, so enabling a market just means `api` starts accepting its product ID.
Symbols cost little, but `api` must control which ones exist, and going past 65,536 symbols
rehashes a map while matching.

## What doesn't work yet

Confirmed with the tools above:

- **Subscribers slow the engine.** 1M orders at full speed reached about 151K orders/sec
  with every service, and 585K with only the checker. Each order produces about 2.7
  reports, and that run used a shared-mode driver with logging on.
- **A 6 GB RAM disk has no room to spare.** The nodes' report stream has a 64 MB term length in
  `tools/scripts/cluster.py`, so each subscriber holds 192 MB. Consumers need a replay on both sides
  when they start or move node, and usage briefly hit 100% of 6 GB at startup. Aeron then logs
  `insufficient usable storage`, and consumers move to the next node, but a restarting node could
  fail the same way. Use 8 GB, which `sbx.py` makes, although that size hasn't been tried. Settled
  usage is about 4 GB.
- **Consumers replace their Aeron client whenever they change node.** Closing a persistent subscription
  leaves its images mapped until the client closes, which filled a 6 GB disk after 7 failovers and
  stopped a node rejoining. New clients fix that, with usage flat over 8 failovers, but the reason
  the images stay mapped hasn't been found.
- **Reads lag for about 12 seconds when the node a consumer follows dies.** It takes 2 seconds to see
  that the node is gone, and 10 more before the consumer moves. Orders and webhooks aren't affected,
  because the gateway and the outbox don't wait on it. `GET` orders can be stale, and cancel-all can
  answer `recorder_behind` until it catches up. Without the local `aeron.image.liveness.timeout`,
  as in the Helm chart, allow 20 seconds.
- **A killed node can't restart for about 10 seconds.** Its archive's mark file still looks alive, so
  it fails with `active mark file detected`. `sbx.py` and `e2e` wait, but `cluster.py start` doesn't.
- **A consumer following a restarted node gets thrown off it.** The node republishes from its last
  snapshot, so the consumer logs `live stream joined at position ... earlier than last seen position`.
  It moves to another node at the same position, so nothing is lost.
- **A POST answers at the first report.** A crossing order comes back `open` with `filled_size` 0
  even when it filled at once. Fills arrive by webhook, and `GET` catches up shortly after.
- **Valkey checks sometimes miss their 100 ms limit** around a failover on this Mac. Those orders go
  through unchecked, by design.

Also known:

- **The local stack can't detect a total loss.** `cluster.py` doesn't set `DURABLE_POSITION`, so the
  engine logs that it can't detect one. The total-loss paths weren't part of this check.
- **The API has no real auth.** It trusts `x-player-id`, so only game servers may reach it. It binds
  `0.0.0.0:8000`, and the Vite dev servers also bind `0.0.0.0`, so they're reachable from your LAN.
- **`web` hasn't been run against the current websocket.** Its components read the same `l2_data` and
  `tickers` shapes that `websocket` sends, but only from reading the code.

## Future work

- **Choose how many symbols share an engine.** Let operators reduce the symbols on an engine, or
  give a heavy market its own. This needs a routing table in `api` that maps each symbol to an
  engine's stream, plus per-engine channels in `tools/scripts/sbx.py`. The engine itself shouldn't
  need to change, because it matches whatever arrives. It costs at least one more CPU core per
  engine.
- **Per-market limits in `api`,** so one market can't fill a shared book.
- **Gateway selection.** Decide how game servers pick a gateway: geo, latency tables or round robin.
- **Aeron Archive and replay.** Replaying input should rebuild the book, and replaying output
  into `level2` would test it offline. Timestamps will differ, because the engine stamps them
  when it receives each message.

## Resource use

`matching-engine` and `trade-reporter` poll in a tight loop, so each uses a full CPU core even when idle.
`market-data` uses one core per feed, so three. `sbx-observer`, `sbx-check` and `sbx-book` sleep when idle.

## Regenerating SBE codecs

Only needed after changing `crates/sbe/messages.xml`. Not run during this setup, and it needs Java.

```sh
git clone https://github.com/aeron-io/simple-binary-encoding.git
cd simple-binary-encoding
./gradlew clean assemble
java -Dsbe.target.language=Rust -Dsbe.output.dir=<output dir> \
  -jar sbe-all/build/libs/sbe-all-<version>.jar ../sbx/crates/sbe/messages.xml
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
