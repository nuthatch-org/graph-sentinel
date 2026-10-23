<img src="docs/logo.svg" width="72" alt="">

# graph-sentinel

[Support my work <3](https://github.com/sponsors/cargopete)

A read-only sentinel an indexer runs beside graph-node, indexer-agent, indexer-service and
tap-agent. It catches the failures that cost indexers money while every existing monitor shows
green: deployments frozen while reporting healthy, POIs repeated across epochs where the chain had
activity, Rewards Eligibility Oracle shortfalls, and a broken payment path.

Design: [RFC-0001](docs/RFC-0001-indexer-sentinel.md). Progress: [docs/progress-log.md](docs/progress-log.md).

Status: pre-alpha. Five modules run today: deployment liveness, payment checks, the REO meter, the
network view (allocations, POI deadlines, disputes) and an RPC spend guard.

## Try it without an indexer

`demo/` runs a whole simulated indexer stack: graph-node, an RPC, the network subgraph, a TAP
aggregator, and a tap-agent writing indexer-rs's real tables in Postgres. Then break it on cue.

```sh
cd demo && docker compose up -d --build
open http://127.0.0.1:9477/
./incident.sh                    # lists what you can break
./incident.sh freeze             # a deployment stops while graph-node calls it healthy
./incident.sh aggregator-down    # the aggregator serves a Cloudflare page instead of gRPC
```

## Quick start

It is a daemon with its own dashboard. You glance at the page; it messages you on Telegram or
Discord when something needs attention. The CLI exists for scripts and debugging.

Beside an existing Docker indexer stack (the full path, including Telegram and the read-only
Postgres role, is [docs/install.md](docs/install.md)):

```sh
cd deploy/compose
cp graph-sentinel.toml.example graph-sentinel.toml && cp .env.example .env   # then edit both
docker compose up -d
docker compose exec graph-sentinel graph-sentinel doctor
```

A fuller config:

```toml
# graph-sentinel.toml
[sources]
indexer_address = "0x…"
network_subgraph_token_env = "GRAPH_API_KEY"   # a gateway API key

[sources.rpc]
"arbitrum-one" = "https://arb1.arbitrum.io/rpc"

[outputs]
listen = "127.0.0.1:9477"                      # dashboard, /api/snapshot, /metrics
public_url = "https://sentinel.example.org"    # linked from every notification

[[outputs.notify]]
kind = "telegram"
bot_token_env = "TELEGRAM_BOT_TOKEN"
chat_id = "-1001234567890"

[[outputs.notify]]
kind = "discord"
url_env = "DISCORD_WEBHOOK_URL"
min_severity = "critical"
```

From source:

```sh
export GRAPH_SENTINEL_SOURCES__INDEXER_POSTGRES_URL=postgres://readonly@host/indexer  # tap-agent's database
cargo build --release && ./target/release/graph-sentinel
```

On start it posts a short "watching" message to every channel, so you know the wiring works.
After that, it posts when a finding appears, escalates or clears, never on a timer. Channels
are `telegram`, `discord`, `slack` and `generic` (JSON for Alertmanager and the like). Each has
a `min_severity` (`info`, `warn`, `critical`; default `warn`). The page has no login: keep it on
loopback, or put it behind your own proxy.

The REO count comes from TAP receipts, so it is an upper bound on the oracle's: receipts do not
record latency or freshness. When the oracle declines a run the receipts said should pass, the
dashboard and a notification say so. tap-agent deletes receipts once a RAV covers them, so
history starts when graph-sentinel does. Keep it running.

`graph-sentinel doctor` checks every source; `reo`, `reo candidates`, `reo history` and
`status` print the same figures as the page, with `--json`.

## Layout

| Crate | Role |
| --- | --- |
| `graph-sentinel` | binary: daemon, dashboard, notifications, local state, metrics, CLI |
| `sentinel-sources` | Postgres, index-node, network subgraph and RPC clients |
| `sentinel-checks` | the check modules; pure logic, tested against fixtures |
| `sentinel-types` | `Finding`, `Severity`, ids |

## Licence

MIT OR Apache-2.0.
