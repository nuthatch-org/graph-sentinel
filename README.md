# graph-sentinel

A read-only sentinel an indexer runs beside graph-node, indexer-agent, indexer-service and
tap-agent. It catches the failures that cost indexers money while every existing monitor shows
green: deployments frozen while reporting healthy, POIs repeated across epochs where the chain had
activity, Rewards Eligibility Oracle shortfalls, and a broken payment path.

Design: [RFC-0001](docs/RFC-0001-indexer-sentinel.md). Progress: [docs/progress-log.md](docs/progress-log.md).

Status: pre-alpha. The REO meter (M1) is being built first, ahead of the oracle's step from 5 to 10
subgraphs per day on 2026-10-27.

## Quick start

```sh
cargo build --release
cat > graph-sentinel.toml <<'TOML'
[sources]
indexer_address = "0x…"
network_subgraph_token_env = "GRAPH_API_KEY"   # a gateway API key
[sources.rpc]
"arbitrum-one" = "https://arb1.arbitrum.io/rpc"
TOML
export GRAPH_SENTINEL_SOURCES__INDEXER_POSTGRES_URL=postgres://readonly@host/indexer  # tap-agent's database
./target/release/graph-sentinel doctor
./target/release/graph-sentinel reo
./target/release/graph-sentinel reo candidates
./target/release/graph-sentinel run      # daemon: /metrics on :9477, webhooks, ledger
```

The REO count comes from TAP receipts, so it is an upper bound on the oracle's: receipts do not
record latency or freshness. tap-agent deletes receipts once a RAV covers them, so history is what
graph-sentinel has seen since it started, plus whatever the tables still hold at first run. Keep
the daemon running.

## Layout

| Crate | Role |
| --- | --- |
| `graph-sentinel` | binary: config, scheduler, local state, metrics, CLI |
| `sentinel-sources` | Postgres, index-node, network subgraph and RPC clients |
| `sentinel-checks` | the check modules; pure logic, tested against fixtures |
| `sentinel-types` | `Finding`, `Severity`, ids |

## Licence

MIT OR Apache-2.0.
