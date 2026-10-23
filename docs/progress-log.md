# Progress log

Newest first.

## 2026-10-23: dashboard, Telegram and Discord, lighthouse mark

- Bare `graph-sentinel` runs the daemon and serves its dashboard on `listen` (loopback by
  default): verdict headline, four stat tiles, served-per-day chart against the minimum with a
  14-day projection, the next oracle runs, findings, source health including each notification
  channel, ranked candidates, and a scope panel. One embedded HTML file, self-hosted fonts,
  cargopete house style, light theme, phone width checked.
- Notifications: `[[outputs.notify]]` channels for Telegram, Discord, Slack and generic JSON,
  per-channel `min_severity`, a start-up "watching" message, a digest above five events, the
  dashboard link in every message. Delivery verified against a local capture server, including
  the Telegram request path and HTML escaping; not yet against real Telegram or Discord.
- Posted oracle runs now come from the chain; new `reo_oracle_disagrees` finding.
- Lighthouse mark for the favicon, header and README.

## 2026-10-23: renamed to graph-sentinel; M0 and M1 code complete, unverified against a live stack

- Renamed from lapwing: repo `nuthatch-org/graph-sentinel`, binary `graph-sentinel`, crates
  `sentinel-{types,sources,checks}`, metrics `graph_sentinel_*`, env `GRAPH_SENTINEL_*`.
- Sources: indexer DB (receipt scan, RAV days, LISTEN on tap-agent's channels), graph-node DB
  (split and legacy layouts), network subgraph, index-node, JSON-RPC with the REO view calls.
- Binary: config, sqlite ledger and findings lifecycle, `run`, `doctor`, `status`, `reo`,
  `reo candidates`, `reo history`, `/metrics`, generic and Slack webhooks.
- 24 unit tests, plus `sentinel-sources/tests/tap_db.rs` against Postgres 16 carrying
  indexer-rs's real TAP migrations (set `GRAPH_SENTINEL_TEST_TAP_DB`): scan, day boundaries,
  RAV days, and the LISTEN path via tap-agent's own insert trigger.
- End to end on fixture receipts over real allocation ids, the live network subgraph and the live
  oracle contract: `doctor`, `reo`, `reo history`, `reo candidates`, `run` with `/metrics`,
  findings and a generic webhook all behave as designed.
- Not yet run against a production indexer database or a graph-node database.

## 2026-10-23: M1 started

- Workspace scaffolded per the RFC's crate layout.
- REO rule model, projection and candidate ranking in `sentinel-checks::reo`, 11 tests, including
  the retroactive step to 10 and the 29-day inclusive window.
- Checked the RFC against upstream; six amendments recorded at the end of the RFC. The two that
  change the design: the oracle has no signal filter, and tap-agent deletes receipts after RAV
  aggregation, so the meter keeps its own ledger.
- Next: Postgres receipts ledger (LISTEN plus scan plus RAVs), network subgraph allocation map,
  on-chain REO read, `graph-sentinel reo` and `graph-sentinel doctor`.
