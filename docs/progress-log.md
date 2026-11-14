# Progress log

Newest first.

## 2026-11-14: first real graph-node (lab on Jimmy)

- Lab: `~/sentinel-lab` on Jimmy, graph-node v0.45.0, eight deployments taken by signal from the
  network subgraph (Arbitrum One, Base, mainnet on GraphOps; key read from `~/osprey-rig/.env`).
  New build runs beside the old one as `graph-sentinel-lab.service` on `100.83.44.63:9478`,
  restart off. The osprey rig was left stopped as found.
- False positive, fixed: syncing deployments commit their head once per write batch
  (`GRAPH_STORE_WRITE_BATCH_DURATION`, 300 s), so the first build called four busy deployments
  frozen inside two minutes and graphman-restarted two. Liveness now reads `deployment_head` from
  graph-node's metrics (`[liveness] graph_node_metrics_url`, defaulting to `[spend]`'s), and
  without it waits out `write_batch` before anything worse than idle. The demo sim publishes
  the metric; e2e still 10 of 10.
- True positives, unstaged: QmdizJ6M's runner died at start (provider lacks `traces`) and the
  index-node has called it healthy since; raised `frozen` within a minute. QmcYjagk failed at
  mainnet 20,586,647 with `impossible combination of entity operations`; not yet investigated.
- Not reproduced: graph-node #6730. Killing every Postgres connection did nothing to v0.45.0;
  a ten-minute exclusive lock on one POI table did not stop processing either.
- Also fixed: `${VAR}` expansion in `[sources.rpc]`; the TAP database falls back to
  `postgres_url` only when `indexer_address` is set (a rig had payments failing every tick);
  `chain_lag` serialised camelCase, so the page showed "NaN of undefined"; the failed remedy
  quoted the whole fatal error.
- Known: the spend guard prices only Alchemy-style providers, so GraphOps reads $0.00.

## 2026-11-14: public; remedies, MCP server, e2e finished

- Repo made public. History scanned first: no keys, tokens or keyed gateway URLs.
- Remedies: `sentinel-checks::remedy` gives every finding kind typed steps (check, command and
  host, config edit, SQL, manual), a risk level and a verify line. In `/api/snapshot` as
  `remedies` by fingerprint, and in the dashboard's "Why?" drawer. Never executed.
- POI findings' evidence now carries the deployment beside each allocation.
- MCP: `POST /mcp` on the dashboard listener (stateless streamable HTTP, cross-origin requests
  refused) and `graph-sentinel mcp [--url]` on stdio. Eight read-only tools over the snapshot.
  Both verified against the demo, the stdio one through `docker compose exec -T`.
- e2e: all ten incidents measured. `rotate-sender` never cleared because the test only stopped
  the receipts; already-received receipts from an unconfigured sender stay unpaid, so the finding
  was right. The test now configures the sender as the recovery.

## 2026-10-23: parked. Payment checks, demo stack, packaging

State when parked:
- **Payment checks (M2), built:** `crates/sentinel-checks/src/tap.rs` (8 tests) and
  `crates/graph-sentinel/src/canary.rs`. Aggregator probed as a gRPC server (HTTP 200 +
  `content-type: application/grpc` + a `grpc-status` header; verified against both live
  aggregators), unknown sender, denylist, fees against `max_amount_willing_to_lose_grt`, RAV stall
  (oldest unaggregated receipt while receipts flow), escrow via `PaymentsEscrow.getBalance` against
  fees from RAV-total snapshots, indexer-rs config lint. Page section "Payments".
- **Demo stack:** `demo/` (`docker compose up -d --build`, `./incident.sh <name>`, `./e2e.sh`).
  `demo/sim/sim.py` impersonates graph-node, an RPC, the network subgraph, a TAP aggregator and
  tap-agent over indexer-rs's vendored migrations. End-to-end so far: `freeze` raised in 15 s,
  cleared in 30 s; `fail` raised in 15 s, cleared in 31 s. The other eight incidents in `e2e.sh`
  were not yet measured when parked.
- **Packaging:** `.github/workflows/release.yml` (multi-arch ghcr image + static binaries; never
  run on GitHub), `deploy/compose/` (smoke-tested), `deploy/helm/` (lint + template only),
  `docs/install.md`.
- **Fixed:** start-up no longer exits when the TAP database is down (retries every 30 s); a missing
  or directory config path is named; empty API-key env counts as unset; `doctor` says why the REO
  oracle check is skipped; SQLite `MAX(0, x)` integer decode; URLs stripped from RPC, IPFS and
  network-subgraph errors (keys live in them).
- **Also:** findings carry a "Why?" drawer with their evidence; "Support my work <3" links to
  GitHub Sponsors (footer, README, `.github/FUNDING.yml`).
- **Not deployed:** Jimmy still runs the build from before the payment checks; it has no TAP
  database, so nothing there changes until it is redeployed.

Next, in order: finish `e2e.sh` and fix whatever it finds; POI checks (Module 2) proven on the
osprey rig; a polish pass (empty states, first run, phone, light mode); then the demo recording.

## 2026-10-23: network view, disputes, Lodestar context

- `[network]`: from the network subgraph, the indexer's provision, allocation, delegation and
  lifetime rewards, every open allocation with its POI deadline (28 days, `maxPOIStaleness`),
  and disputes filed against it. With `fisherman_address`, the disputes that address filed,
  with their cancellable dates. Findings: `poi_due`, `poi_stale`, `dispute_against`,
  `dispute_cancellable`, `dispute_ruled`. A failed read holds findings rather than resolving them.
- Lodestar public endpoints: GRT price in the header, per-chain median lag beside each deployment.
  Lodestar's per-indexer routes (QoS, revenue) are behind its paywall since 2026-10-18 and are
  not used yet.
- Public Arbitrum RPCs proved unusable for event history (rate limits, range caps, archive
  behind tokens), which is why this reads the network subgraph rather than the chain.
- Trap: the network subgraph's `cancellableAt` is garbage for some query disputes; only
  plausible timestamps are kept.

## 2026-10-23: RPC spend guard

- `[spend]` reads graph-node's `endpoint_request` counters (every RPC call, per provider and
  method; v0.41 batches only on a deprecated receipts path), prices billed providers at Alchemy's
  per-method CU and $0.45/M, and keeps a per-day ledger. Warns at 70% or when the projection
  passes the budget; at 100% runs `stop_command` once, raises a critical finding, and runs
  `resume_command` after midnight UTC. While stopped, liveness does not cry "index-node down".
- Tested against a fake metrics endpoint: public providers free, trips once, remembers across a
  restart. Overshoot is at most one tick of spend. The midnight resume is not yet exercised.

## 2026-10-23: liveness (M3), optional modules, Docker image

- Modules switch on from their sources: liveness with `index_node_url`, the REO meter with
  `indexer_address` plus the TAP database. A graph-node with no indexer (a fisherman's rig) runs
  liveness alone.
- Liveness: one index-node read per tick; a deployment behind and still gets an `eth_getLogs`
  probe over its manifest's static addresses and topic0s (graph-node's own topic rule), which
  separates frozen from idle. Block handlers count as always active; templates and non-EVM
  sources can only ever be "not moving". Findings: `frozen`, `stalled`, `stopped`, `failed`,
  `chain_stalled`, `index_node_down`. Optional graphman restart, off by default.
- Page: a status line across modules, a Deployments table worst first; REO sections only when
  the meter runs.
- Dockerfile: static musl, 36 MB image. `rust-version = 1.94` (sqlx 0.9).

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
