# Roadmap

MMX is general self-hosted exchange infrastructure with game-focused defaults and SDKs. Release 1 is
done when an agent can follow our docs alone to install the Helm chart, define a market in
`markets.yaml`, connect an OIDC provider and add a fill webhook that dedupes on `fill_id`.

## How the system fits together

```
game server -> gateway -> engine (Aeron Cluster) -> tape (Aeron Archive)
                                                     |-> outbox -> webhooks to the game
                                                     |-> trade-reporter -> QuestDB
                                                     |-> market-data -> NATS -> websocket
```

These decisions are settled:

- **The engine only matches.** It has no IO, HTTP or business/risk validation. Its protocol boundary
  validates complete message envelopes before applying effects. It trusts authorized ingress and
  keeps no gateway state. New symbols go live when orders arrive, and trees are never freed.
- **The tape is the record.** Aeron Archive output is authoritative, byte-identical on every node, and
  kept forever by default. Everything else is a consumer that can lag behind it.
- **QuestDB is a required query view, not the source of truth.** `GET` orders reads from it, and it's
  rebuilt from the tape if lost. Open-source QuestDB is single-node, and that's fine for this role.
- **Gateways are the authenticated edge.** Public clients are first-class; validation, account/risk
  authorization, rate limits and enabled markets live at admission. Internal services trust authorized
  ingress and remain isolated from public access. The current header-only player identity is not the
  production authentication design.
- **Retries preserve one operation.** The required contract binds a stable account-scoped operation
  key to immutable intent. Repeating it cannot create another economic action; changing its payload
  conflicts. Unknown outcomes keep assets reserved until authoritative reconciliation. The current
  expiring boolean claims are a partial implementation, not the complete durable retry protocol.
- **Failover** uses the term marker in the report stream to tell which in-flight orders landed.
- **Data only flows one way.** Consumers derive state from the tape and never feed it back. Archive is
  the source of truth, and it holds the snapshots.
- **Execution notifications follow durability.** Archive sync level 1 is the default, so a commit is
  on a quorum's disks and a total power loss can't undo what the game has seen. The outbox also syncs
  to disk before sending a webhook. Sync 0 is an opt-in for operators who accept that window.
- **Admission/risk integration is general.** The game-focused default validates at the gateway,
  durably records admission intent, asks the game ledger to reserve assets, and waits for durable
  confirmation before submitting to matching. Other providers must establish equivalent configured
  authority. Pre-admission reservations need durable orchestration beyond the execution tape.
  Terminal non-execution releases holds only after possible delayed submissions have been fenced.
- **After a total loss,** trading stops and the book is rebuilt from the tape with no new entries, so
  open orders survive. The admin then reopens entries, or cancels all open orders instead.
- **Rate limits** are per player in Valkey, with defaults that game owners can override.
- **Deployment is Kubernetes-native.** Secrets are plain Kubernetes secrets. Vault injection and gateway
  config are the operator's choice.
- **TLS and exposure are the operator's.** Binaries speak plain HTTP and Aeron inside the cluster. The
  chart creates Services and no Ingress, and the docs list the ports and say plainly that TLS is the
  operator's job. Aeron stays unencrypted for throughput, and the tape isn't sensitive.
- **Gateways always authenticate.** A JWT is on by default, even when only game servers connect, so an
  accidental exposure can't accept orders. OIDC is an optional flag. Players trading directly, such as
  from Python, is a production design requirement, enabled with valid authentication/account/risk
  configuration by default; administrators can disable direct access. Missing required authority must
  refuse admission, not fall back to trusting a public header.
- **No default passwords.** Password auth reads a value from the environment or generates a random one
  at startup. Enabling OIDC can turn passwords off entirely.
- **The console owns markets after install.** `values.yaml` seeds them once. The console reads and writes
  the ConfigMap through the Kubernetes API, as a YAML editor, and an applied ConfigMap shows up there.
  Up to a minute or so to reach gateways is acceptable, because markets change rarely.
- **Snapshots are rare,** because failures are assumed to be rare. Restarts may take minutes, and a
  live pause must pass for a real exchange.
- **Generic language only.** No Minecraft, Hypixel or SkyBlock references in new code. Use player, game
  and item.

## Sections

Each section owns its listed paths, so sections can be built in parallel. Sections marked **wave 1** are
in progress.

### 1. Gateway (wave 1)

Owns `services/api/rest`.

- [x] Hold requests across failover and answer with the engine's decision
- [x] Required `client_order_id`, in cluster mode
- [x] Refactor for composability
- [x] Return strings instead of `f64`, and omit null fields
- [x] Request shape checks and validation, including tick size
- [x] Enabled markets from `markets.yaml`, with hot reload through an explicit reload command
- [x] Per-player rate limits in Valkey, with defaults and overrides
- [x] Remove the repeat-order check and `RETRY_WINDOW_SECS`, since callers don't retry
- [x] Unknown-outcome status: define when a missing order is definitely not landed
- [x] Cancel an order, and cancel all of a player's orders
- [x] `GET` orders
- [ ] JWT validation on by default, including game-server-only deployments
- [ ] Optional OIDC, with token introspection, mapping users to accounts
- [ ] Direct public player access with validated account/risk authority; administrator can disable it
- [x] Atomic claim in Valkey with UUIDv7 IDs, so a repeated ID can never execute twice, on any gateway

### 2. WebSocket (wave 1)

Owns `services/api/websocket`.

- [x] Subscriptions by channel and product
- [x] Heartbeats
- [x] Close sockets after 60 seconds with no message
- [x] Cache tickers, refreshed every second
- [ ] Private per-player order feed: websocket reads `orders.*`, but nothing publishes it yet
- [x] Handle failures without panicking

### 3. Market data and recording (wave 1)

Owns `services/market-data` and `services/trade-reporter`.

- [ ] Stop `level2` silently dropping updates
- [x] Candles service, emitting once per second
- [x] Record orders, trades and candles in QuestDB, including filled orders
- [x] Rebuild QuestDB from the tape
- [x] Follow the tape with persistent subscriptions, and resume from a saved position
- [x] Keep listening instead of crashing, with no panics on the data path
- [x] Share `messages.rs` instead of duplicating it

### 4. Outbox and webhooks (wave 1)

Owns the new `services/outbox`.

- [x] Consume the tape from a saved position
- [x] Sync to disk before sending anything
- [x] Webhooks for fills, cancels and rejects, so the game can always release escrow
- [x] Dedupe key `fill_id`, retries and backoff, and a signature the game can verify
- [ ] Replication design, so one disk loss can't block webhooks
- [x] Honour `429`/`503` and `Retry-After` seconds/date hints with interruptible waits and stable retry event IDs
- [ ] Pause after repeated failures on one batch and alert the operator, so a batch that crashes the game can't crashloop it
- [ ] Optional maximum delivery rate

### 5. Engine durability (wave 1)

Owns `crates/cluster` and `services/matching-engine`.

- [x] Scheduled snapshots, and purging of old snapshots and log
- [ ] Snapshot far less often, and choose the default from a measured replay speed
- [ ] Reduce the snapshot pause, about 230-300ms on the whole cluster, with about 109ms of that in setup
- [ ] Detect a total loss on startup: built, but untested and not set in the chart. Sync 1 makes it a backstop
- [ ] Rebuild the book from the tape, with no new entries: built, untested end to end
- [ ] Admin cancel-all recovery path: built, untested end to end
- [x] Remove spike-only flags, and engine unit tests
- [ ] Configurable tape retention, swept to QuestDB by default: retention exists, sweeping isn't wired
- [x] Report a repeat of a resting order as a duplicate, even on a full book
- [x] Keep held Archive connections alive and reconnect before a snapshot if one has dropped. Unread pings filled a 64 KB term in about 17 minutes, so every timed snapshot panicked every node
- [ ] Recovery plan for an order that panics every node, since replay hits the same panic
- [ ] Reject a message split across frames instead of panicking; only `mmx-cluster-bench --batch` above about 13 sends one
- [ ] Give a full book its own reject reason and an operator alert
- [ ] Give self-trade cancels their own reason
- [ ] Clear stale TODOs in `publisher.rs`, `types.rs`, `config.rs` and `handler.rs`

### 6. Packaging (wave 1)

Owns the new `deploy/`.

- [x] Cached multi-arch images for every service
- [x] Kustomize base with shared memory and pinned CPUs for Aeron
- [x] Helm chart with UI and demo flags, spread across nodes
- [x] Local cluster with `container cluster create`
- [x] CI: build, test and chart install

### 7. Control plane

- [x] Admin API, used by both the CLI and console
- [x] Per-market halt, so operators can stop trading in one market at once, such as during a dupe
- [x] Per-market cancel-all, across every player
- [x] `mmx` CLI: status, health, markets and config
- [x] Console: first-run setup, sessions and OIDC login, and a YAML market editor backed by the Kubernetes API
- [x] Console: roles from provider groups, recent sign-in for destructive actions, a provider set at install, and no public URL setting
- [x] Console: market page with a live book and log, search over thousands of markets, and a sampled activity feed
- [ ] Book snapshots from `market-data`, so opening a market doesn't scan its history on the tape
- [x] Seed the markets ConfigMap only when it's missing, so `helm upgrade` can't overwrite console edits
- [x] `mmx` in the chart, with RBAC limited to one ConfigMap and one Secret by name
- [ ] Console API keys

### 8. Demo and public face

- [ ] Rename MMX to MMX, and remove game-specific references
- [ ] Retire the `auth` service
- [ ] Demo trading UI, with credentials from the environment or generated at startup
- [ ] Finish `OrderDrawer.tsx`: live status, order updates and error handling
- [ ] Validate the symbol on the trade page
- [ ] Demo load bots
- [ ] Merge the landing page, using zero cookies
- [ ] Rewrite the docs site, and run the release 1 agent test

### 9. Engine SDKs

The pitch: you don't build an exchange, and you don't do much work at all. Deploying MMX is simple,
and you plug in an SDK and trade from day one. Each SDK knows the API contract, so a game calls
trade with its own types and the SDK handles the objects and type declarations. Developers can use
the raw APIs or an SDK.

- [ ] Godot SDK
- [ ] Unity SDK
- [ ] Unreal SDK
- [ ] Fee config in the SDKs and escrow helpers, so games can apply fees cleanly. MMX only reports trades, and fees stay in the game database
- [ ] Add SDKs to the docs beside the raw APIs
- [ ] Show the SDK sample on the landing page, replacing the raw API samples
- [ ] End-to-end docs sample in a real game engine, such as Godot
- [ ] Demo video for the landing page hero and the docs

### Cross-cutting

- [ ] Remove every `.expect`, `unwrap` and panic on a runtime path outside the matching engine, with an explicit handler for each invariant. Services never panic unless it's truly unrecoverable. The engine may panic on a real safety violation, because stopping is safer than continuing
- [ ] Consistent logging, traces, config and startup across services
- [ ] `/healthz`, `/readyz` and `/metrics` on every service: gateway has all three, outbox and `mmx` have health only
- [x] Local end-to-end check with `mmx.py e2e`: a signed fill webhook through a leader kill
- [ ] Fix Aeron's persistent subscription leak: carry the patch, report it upstream, then remove the consumers' client replacement
- [ ] Investigate the burst drain on the output path

### Default deployment (plan)

3x the core, and everything else replays from the tape. About 7 binaries, scaling only what's under load.

- [ ] Cluster fixed at 3, gateways and websockets at 2 or more with autoscaling on by default
- [ ] `market-data`, `trade-reporter`, `outbox` and QuestDB at 1, recovering from the tape
- [ ] NATS at 3 in production and 1 locally, with no JetStream
- [ ] Valkey at 3 plain shards chosen by player hash in the gateway, with no replicas, so a claim can't be lost in a failover. Halts and overrides need one fixed home
- [ ] Sizing presets (small, medium, launch) as a single value, with claim memory pre-sized
- [ ] Load-test REST and websocket fan-out before claiming the largest games
- [ ] `level2_batch` websocket channel at 100ms, beside the 5s `ticker_batch`, as the default for game UIs. `level2` stays raw for custom clients, so the API matches a hosted exchange
- [ ] Snapshots only when a client opens a market, so a reconnect costs only the players trading
- [ ] Websocket pods subscribe only to the symbols their clients want, rather than `l2.*`
- [ ] Jittered reconnects in the SDKs, so a pod loss can't cause a reconnect storm

### Direct lane (plan)

- [ ] Authenticated SBE sessions into the cluster for trusted partners, with per-session sequence numbers, skipping REST gateways and Valkey

### From the infra review (plan)

- [ ] Cluster page: members, leader, log position, snapshot age, every consumer's lag, and failed checks named on screen
- [ ] Stale data never looks live: "as of" times, and a market reads "cannot trade" when the engine is down
- [ ] Finer permissions: separate create, update, halt and cancel-all, so developers can add markets without deleting them
- [ ] A separate gateway credential, since the admin token in gateway pods makes a gateway compromise a console admin
- [ ] Refuse to delete a market with open orders, add a global halt with reasons, and make halts survive Valkey loss
- [ ] Last-known-good markets, so a bad ConfigMap can't take down the console or stop gateways starting
- [ ] Chart overrides: `nodeSelector`, tolerations, annotations, labels, `extraEnv`, volumes and security contexts, including bundled dependencies
- [ ] GitOps-safe chart: no `lookup` state, an external control Secret, an unmanaged config mode, and secrets that fit Vault
- [ ] Metrics on every service, alert rules, dashboards, traces and JSON logs
- [ ] Durable audit log, market revision history with rollback, named service accounts and expiring API tokens
- [ ] Gateway `/readyz` as the default probe, probes on consumers, and backup, restore and upgrade docs
- [ ] Tagged, signed releases with an OCI chart

## Later

- Reduce the symbols per engine, with a routing table in the gateway
- Dedicated single-symbol engines
- Gateway selection: geo, latency tables or round robin
- Investigate a standby node for snapshots off the live path. Aeron's is paid, so we'd build our own
- Local rate-limit fallback when Valkey is down
- Snapshot service with NATS streams and gRPC
- MCP servers for players and operators
- Bring-your-own bearer tokens
