# Progress log

Newest first.

## 2026-10-23: renamed to graph-sentinel; M0 and M1 code complete, unverified against a live stack

- Renamed from lapwing: repo `nuthatch-org/graph-sentinel`, binary `graph-sentinel`, crates
  `sentinel-{types,sources,checks}`, metrics `graph_sentinel_*`, env `GRAPH_SENTINEL_*`.
- Sources: indexer DB (receipt scan, RAV days, LISTEN on tap-agent's channels), graph-node DB
  (split and legacy layouts), network subgraph, index-node, JSON-RPC with the REO view calls.
- Binary: config, sqlite ledger and findings lifecycle, `run`, `doctor`, `status`, `reo`,
  `reo candidates`, `reo history`, `/metrics`, generic and Slack webhooks.
- 24 tests. Not yet run against a real indexer database.

## 2026-10-23: M1 started

- Workspace scaffolded per the RFC's crate layout.
- REO rule model, projection and candidate ranking in `sentinel-checks::reo`, 11 tests, including
  the retroactive step to 10 and the 29-day inclusive window.
- Checked the RFC against upstream; six amendments recorded at the end of the RFC. The two that
  change the design: the oracle has no signal filter, and tap-agent deletes receipts after RAV
  aggregation, so the meter keeps its own ledger.
- Next: Postgres receipts ledger (LISTEN plus scan plus RAVs), network subgraph allocation map,
  on-chain REO read, `graph-sentinel reo` and `graph-sentinel doctor`.
