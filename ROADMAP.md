# Roadmap

MMX is a self-hosted order book for game economies. Release 1 is done when an agent can follow our
docs alone to install the Helm chart, define a market in `markets.yaml`, connect an OIDC provider and
add a fill webhook that dedupes on `fill_id`.

## How the system fits together

```
game server -> gateway -> engine (Aeron Cluster) -> tape (Aeron Archive)
                                                     |-> outbox -> webhooks to the game
                                                     |-> trade-reporter -> QuestDB
                                                     |-> market-data -> NATS -> websocket
```

These decisions are settled:

- **The engine only matches.** It has no IO, HTTP or validation, trusts everything it receives, and
  keeps no gateway state. New symbols go live when orders arrive, and trees are never freed.
- **The tape is the record.** Aeron Archive output is authoritative, byte-identical on every node, and
  kept forever by default. Everything else is a consumer that can lag behind it.
- **QuestDB is a required query view, not the source of truth.** `GET` orders reads from it, and it's
  rebuilt from the tape if lost. Open-source QuestDB is single-node, and that's fine for this role.
- **Gateways bounce.** Validation, rate limits and enabled markets live there. Only game servers can
  reach gateways, enforced by firewall policy.
- **A POST is a guarantee.** It's held across a failover and answered with the engine's decision. If
  the gateway can't answer, the outcome is unknown: the caller never retries, keeps the funds on hold
  and asks any gateway for the order's status. Callers supply `client_order_id`.
- **Failover** uses the term marker in the report stream to tell which in-flight orders landed.
- **Nothing reaches the game until it's durable.** The outbox syncs to disk before sending a webhook.
  Engine state relies on replication, with per-write disk sync off by default.
- **Escrow lives with the game.** It must always clear for orders that didn't execute, and this must
  never allow a double spend, even if every node dies at once.
- **After a total loss,** trading stops and the book is rebuilt from the tape with no new entries, so
  open orders survive. The admin then reopens entries, or cancels all open orders instead.
- **Rate limits** are per player in Valkey, with defaults that game owners can override.
- **Deployment is Kubernetes-native.** Secrets are plain Kubernetes secrets. Vault injection and gateway
  config are the operator's choice.
- **Generic language only.** No Minecraft, Hypixel or SkyBlock references in new code. Use player, game
  and item.

## Sections

Each section owns its listed paths, so sections can be built in parallel. Sections marked **wave 1** are
in progress.

### 1. Gateway (wave 1)

Owns `services/api/rest`.

- [x] Hold requests across failover and answer with the engine's decision
- [x] Required `client_order_id`, in cluster mode
- [ ] Refactor for composability
- [ ] Return strings instead of `f64`, and omit null fields
- [ ] Request shape checks and validation, including tick size
- [ ] Enabled markets from `markets.yaml`, with hot reload through an explicit reload command
- [ ] Per-player rate limits in Valkey, with defaults and overrides
- [ ] Remove the repeat-order check and `RETRY_WINDOW_SECS`, since callers don't retry
- [ ] Unknown-outcome status: define when a missing order is definitely not landed
- [ ] Cancel an order, and cancel all of a player's orders
- [ ] `GET` orders
- [ ] OIDC, with JWT and token introspection modes, mapping users to accounts

### 2. WebSocket (wave 1)

Owns `services/api/websocket`.

- [ ] Subscriptions by channel and product
- [ ] Heartbeats
- [ ] Close sockets after 60 seconds with no message
- [ ] Cache tickers, refreshed every second
- [ ] Private per-player order feed
- [ ] Handle failures without panicking

### 3. Market data and recording (wave 1)

Owns `services/market-data` and `services/trade-reporter`.

- [ ] Stop `level2` silently dropping updates
- [ ] Candles service, emitting once per second
- [ ] Record orders, trades and candles in QuestDB, including filled orders
- [ ] Rebuild QuestDB from the tape
- [ ] Follow the tape with persistent subscriptions, and resume from a saved position
- [ ] Keep listening instead of crashing, with no panics on the data path
- [ ] Share `messages.rs` instead of duplicating it

### 4. Outbox and webhooks (wave 1)

Owns the new `services/outbox`.

- [ ] Consume the tape from a saved position
- [ ] Sync to disk before sending anything
- [ ] Webhooks for fills, cancels and rejects, so the game can always release escrow
- [ ] Dedupe key `fill_id`, retries and backoff, and a signature the game can verify
- [ ] Replication design, so one disk loss can't block webhooks

### 5. Engine durability (wave 1)

Owns `crates/cluster` and `services/matching-engine`.

- [ ] Scheduled snapshots, and purging of old snapshots and log
- [ ] Reduce the snapshot pause
- [ ] Detect a total loss on startup, and refuse to resume past it
- [ ] Rebuild the book from the tape, with no new entries
- [ ] Admin cancel-all recovery path
- [ ] Remove spike-only flags, and engine unit tests
- [ ] Configurable tape retention, swept to QuestDB by default
- [x] Report a repeat of a resting order as a duplicate, even on a full book
- [ ] Recovery plan for an order that panics every node, since replay hits the same panic
- [ ] Reject a message split across frames instead of panicking; only `sbx-cluster-bench --batch` above about 13 sends one
- [ ] Give a full book its own reject reason and an operator alert
- [ ] Give self-trade cancels their own reason
- [ ] Clear stale TODOs in `publisher.rs`, `types.rs`, `config.rs` and `handler.rs`

### 6. Packaging (wave 1)

Owns the new `deploy/`.

- [ ] Cached multi-arch images for every service
- [ ] Kustomize base with shared memory and pinned CPUs for Aeron
- [ ] Helm chart with UI and demo flags, spread across nodes
- [ ] Local cluster with `container cluster create`
- [ ] CI: build, test and chart install

### 7. Control plane

- [ ] Admin API, used by both the CLI and console
- [ ] `mmx` CLI: status, health, markets and config
- [ ] Console: auth setup, API keys and market editor

### 8. Demo and public face

- [ ] Rename SBX to MMX, and remove game-specific references
- [ ] Retire the `auth` service
- [ ] Demo trading UI with dummy credentials
- [ ] Finish `OrderDrawer.tsx`: live status, order updates and error handling
- [ ] Validate the symbol on the trade page
- [ ] Demo load bots
- [ ] Merge the landing page, using zero cookies
- [ ] Rewrite the docs site, and run the release 1 agent test

### Cross-cutting

- [ ] Consistent logging, traces, config and startup across services
- [ ] `/healthz`, `/readyz` and `/metrics` on every service
- [ ] Investigate the burst drain on the output path

## Later

- Reduce the symbols per engine, with a routing table in the gateway
- Dedicated single-symbol engines
- Gateway selection: geo, latency tables or round robin
- Standby node for snapshots off the live path
- Local rate-limit fallback when Valkey is down
- Snapshot service with NATS streams and gRPC
- MCP servers for players and operators
- Bring-your-own bearer tokens
