# MMX

**Self-hosted exchange infrastructure for game economies.**

<img width="1263" height="827" alt="image" src="https://github.com/user-attachments/assets/7dd09a4b-3307-43a6-8a9a-795500477d89" />

[Website](https://mmx.exchange) · [Documentation](https://docs.mmx.exchange) · [Demo](https://mmx.exchange/demo) · [Roadmap](ROADMAP.md) · [Guidelines](AGENTS.md)

## What is MMX?

MMX is an open-source trading engine for building player-driven markets in games.

It provides a price-time priority order book, market data streaming, and reliable trade delivery. Games retain full control of player accounts, inventory, and currency while MMX handles high-throughput order matching and execution.

MMX is written in Rust and designed to run on your own infrastructure. Although built with game economies in mind, its matching engine operates on generic accounts, symbols, prices, and quantities.

## Features

- **Price-time priority matching:** Deterministic limit order books with fixed-point prices and quantities.
- **Fault-tolerant execution:** Three-node [Aeron](https://github.com/aeron-io/aeron) Cluster replication with deterministic replay and snapshots.
- **Reliable trade delivery:** Durable outbox, signed webhooks, and stable fill identifiers for idempotent settlement.
- **Real-time market data:** Streaming market events over NATS and WebSockets.
- **Game-controlled settlement:** Keep inventory, currency, authorization, and account management in your own backend.
- **Self-hosted deployment:** Run locally with Docker Compose or deploy to Kubernetes with Helm.
- **Operator tooling:** CLI, admin API, and web console for managing markets, orders, and cluster health.

## Architecture

MMX separates order matching from application authorization and settlement.

Orders enter through the REST gateway and are processed by a replicated matching engine. Execution reports are recorded for replay, delivered to applications through signed webhooks, and published to market data consumers.

The application remains responsible for reserving assets before orders are accepted and applying fills to its own ledger.

See the [architecture documentation](https://docs.mmx.exchange/guides/architecture) for details on replication, recovery, and trade delivery.

## Getting started

### Quickstart with Helm (Kubernetes)

Deploy MMX to Kubernetes using Helm:

```sh
helm install mmx deploy/helm/mmx -n mmx --create-namespace
```

### Local Docker

Run the stack locally with Docker Compose:

```sh
curl -fsSLO https://github.com/awesomepandapig/sbx/releases/latest/download/install.sh
sh install.sh 0.1.0
export PATH="$HOME/.local/bin:$PATH"
mmx local up
```

For detailed guides, configuration, and submitting your first trade, see the [getting started guide](https://docs.mmx.exchange/getting-started).

You can also explore the [interactive demo](https://mmx.exchange/demo) without deploying anything.

## Project status

MMX is under active development.

See the [feature status](https://docs.mmx.exchange/status) and [roadmap](ROADMAP.md) for current capabilities, integration contracts, and planned work.

## Contributing

MMX is open source. For repository structure, guidelines, and commands, see [AGENTS.md](AGENTS.md). For dependencies and local setup, see [STARTUP.md](STARTUP.md).

## License

MMX is dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
