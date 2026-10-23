# graph-sentinel

A read-only sentinel an indexer runs beside graph-node, indexer-agent, indexer-service and
tap-agent. It catches the failures that cost indexers money while every existing monitor shows
green: deployments frozen while reporting healthy, POIs repeated across epochs where the chain had
activity, Rewards Eligibility Oracle shortfalls, and a broken payment path.

Design: [RFC-0001](docs/RFC-0001-indexer-sentinel.md). Progress: [docs/progress-log.md](docs/progress-log.md).

Status: pre-alpha. The REO meter (M1) is being built first, ahead of the oracle's step from 5 to 10
subgraphs per day on 2026-10-27.

## Layout

| Crate | Role |
| --- | --- |
| `graph-sentinel` | binary: config, scheduler, local state, metrics, CLI |
| `sentinel-sources` | Postgres, index-node, network subgraph and RPC clients |
| `sentinel-checks` | the check modules; pure logic, tested against fixtures |
| `sentinel-types` | `Finding`, `Severity`, ids |

## Licence

MIT OR Apache-2.0.
