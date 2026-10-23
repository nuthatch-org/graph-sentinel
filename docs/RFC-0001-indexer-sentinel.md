# RFC-0001: Indexer Sentinel

Oct 22, 2026 · @pete

## Summary

The sentinel (working name: **lapwing**, after the bird that mobs anything near its nest) is a single Rust daemon that an indexer runs beside graph-node, indexer-agent, indexer-service and tap-agent. It reads interfaces the stack already exposes and catches the four failures that cost indexers money in 2026 without anyone noticing: deployments that freeze while reporting healthy, POIs reused across epochs where the chain had activity, Rewards Eligibility Oracle (REO) ineligibility from serving too few qualifying subgraphs, and payment-path breakage (aggregator outages, sender rotations, self-inflicted denylists, frozen RAV requests).

It changes nothing upstream, needs no protocol or peer adoption, and any single indexer can run it today. Inputs are read-only Postgres, the index-node status GraphQL, the network and Epoch Block Oracle subgraphs, the indexer's own RPC and the TAP aggregator endpoints. Outputs are a Prometheus endpoint, webhook alerts and a CLI. One optional write path exists: `graphman restart` on a confirmed-frozen deployment.

Target: a usable first cut in two to three weeks, timed to the REO step from 5 to 10 qualifying subgraphs per day on 2026-10-27.

**Status:** draft. **Scope:** v0.1. **Repo:** nuthatch-org/lapwing (proposed).

## Motivation

Every incident below passed every existing monitor. Grafana showed green, the index-node said `healthy`, allocations closed on schedule, and money was lost anyway. The common thread is that the stack reports its own state, not whether that state is consistent with the chain, the oracle or the payment rail.

| Failure | What happened | Cost | Evidence |
| --- | --- | --- | --- |
| Frozen-but-healthy deployment | A Postgres connection blip kills the deployment runner; the writer is poisoned and never replaced; the status API keeps saying `healthy`. Regression since graph-node v0.42.0; still present in v0.45.0. Fix is `graphman restart`. 2–3 of \~15 deployments per node stop at random (pops.one). | Agent closes allocations on a stale POI, repeatedly. One indexer: twelve consecutive allocations (epochs 700–1007) on one frozen deployment. Dispute exposure plus lost REO days. | [graph-support #51](https://github.com/nightswatchhq/graph-support/issues/51), [graph-node PR #6730](https://github.com/graphprotocol/graph-node/pull/6730) (open), [v0.45.0-patch.1](https://github.com/cargopete/graph-node/releases/tag/v0.45.0-patch.1) |
| POI reuse across active ranges | Repeated POIs are usually legitimate (dormant subgraphs) but some are not: one indexer reused a POI 40× on Polygon across a range where a reference showed activity, carrying 682,044.81 GRT of rewards. Nothing on-chain checks. | Slashing risk if a fisherman notices; rewards earned on no work. | [graph-support #20](https://github.com/nightswatchhq/graph-support/issues/20), [GDR-51](https://forum.thegraph.com/t/request-for-information-about-disputes-gdr-51/6938) |
| REO ineligibility | REO withholds rewards unless the indexer serves qualifying queries (HTTP 200, <5,000 ms, within 50,000 blocks of head, subgraph signal ≥500 GRT) on N distinct subgraphs per day, on ≥5 days in a rolling 28-day window. N went 1→5 on 2026-10-06 and goes to 10 on 2026-10-27; the new N is applied retroactively to the whole window. The dashboard shows status and streak, not N. | Rewards withheld for the whole ineligible period. At launch 47 of 97 indexers had not qualified. | [REO PR #176](https://github.com/graphprotocol/rewards-eligibility-oracle/pull/176), [PR #179](https://github.com/graphprotocol/rewards-eligibility-oracle/pull/179), [graph-support #46](https://github.com/nightswatchhq/graph-support/issues/46) |
| Aggregator outage → self-denylist | Mainnet aggregator served a Cloudflare 526 page to gRPC clients; tap-agent logged `invalid compression flag: 101`; unaggregated fees crossed `max_amount_willing_to_lose_grt`; the indexer's own tap-agent denylisted the gateway sender; gateway saw `BadResponse(400)`. | Hours of zero routing, no fees, no REO queries. | [graph-support #30](https://github.com/nightswatchhq/graph-support/issues/30), [#31](https://github.com/nightswatchhq/graph-support/issues/31) |
| Sender rotation | Foundation rotated the mainnet sender on 2026-09-24. Indexers who did not add it to `[tap.sender_aggregator_endpoints]` kept serving but were never paid; traffic tailed off. | Unpaid queries for days. | [graph-support #44](https://github.com/nightswatchhq/graph-support/issues/44) |
| Frozen receipt ingestion | tap-agent 2.2.1 can silently stop ingesting one sender's receipts; no RAV is ever requested. | Fees never aggregated. | [indexer-rs #1094](https://github.com/graphprotocol/indexer-rs/issues/1094) |
| Bad config defaults | indexer-service 2.0.2 shipped a placeholder `graph_tally_collector_address`; result was `V2 escrow accounts loaded senders=0`. | Zero fees until noticed. | [graph-support #36](https://github.com/nightswatchhq/graph-support/issues/36) |

The upstream fixes are partial and slow. A local daemon that cross-checks each of these against an independent source closes the gap now, and keeps closing it as new failure modes appear.

## Goals and non-goals

**Goals**

1. Detect a frozen deployment within one check interval (default 5 min) and, if configured, restart it.
2. Never let the agent present or close on a suspect POI without the operator having been told first.
3. Tell the operator, every day, exactly how many qualifying subgraphs they served, how many they need under current and announced criteria, and which deployments would be cheapest to add.
4. Detect a broken payment path (aggregator down, unknown sender, denylist entry, stalled RAV requests, misconfigured collector) within minutes, before fees cross the loss threshold.
5. Run with read-only Postgres credentials and no write access to the agent's action queue in v0.1.
6. Single static binary, one TOML file, Prometheus `/metrics`, webhook alerts, works with StakeMachine and Launchpad layouts without patching either.
7. Zero cost at rest: a few queries per deployment per interval against local Postgres and the local RPC.

**Non-goals**

- Replacing Grafana, Foghorn or Lodestar. Lapwing produces signals; those render them.
- Cross-indexer POI comparison. That needs peers (Graphix, Subgraph Radio, both dormant). Lapwing checks the indexer's own POIs against the chain; peer comparison is a later opt-in.
- Allocation planning or optimisation. indexer-tools-v4 and Lodestar's simulator cover that.
- Fixing graph-node. If a frozen runner needs a patch, the patch lives upstream or in `cargopete/graph-node`; lapwing only detects and restarts.
- RPC proxying, caching or block-cache auditing. Separate projects (see the ideas report, items 5 and 6).
- Acting on the chain. No transactions in any version.

## Architecture

One process, one config file, one tick loop. Each check is a module that implements `Check::run(&Ctx) -> Vec<Finding>`; the scheduler runs them on their own intervals, dedupes findings against the last run, persists history to a local sqlite file, and routes new or escalated findings to the outputs.

&#91;embedded content: lapwing architecture · 6 inputs, 4 checks, 4 outputs\]

Everything on the left is read-only. The daemon holds a Postgres role with `SELECT` on `subgraphs.*`, `chain*.*`, the `tap_horizon_*` tables and `scalar_tap_*` if present; it never writes to graph-node's database. The amber box is the only side effect and is gated behind `[liveness] restart = true` plus a confirmation window.

**Crate layout (workspace)**

| Crate | Role | Main deps |
| --- | --- | --- |
| `lapwing` | binary: config, scheduler, outputs, CLI | tokio, clap, figment, axum (metrics), rusqlite |
| `lapwing-sources` | typed clients: Postgres (sqlx), index-node GraphQL, network/EBO subgraph (graphql-client), RPC (alloy), aggregator health (tonic) | sqlx, alloy, tonic, reqwest |
| `lapwing-checks` | the four modules behind one `Check` trait | depends on sources only |
| `lapwing-types` | `Finding`, `Severity`, `DeploymentId`, `Epoch`, shared newtypes | serde |

**Tick model**

- Default intervals: liveness 5 min, POI gate 15 min and on-demand before epoch boundaries, REO meter 1 h (plus a daily rollup at 00:05 UTC to match the oracle's day boundary), TAP canary 2 min.
- A `Finding` has `check`, `subject` (deployment, sender, allocation), `severity` (info / warn / critical), `fingerprint`, `first_seen`, `last_seen`, `evidence: serde_json::Value`.
- Dedupe is by fingerprint. An alert fires on first sight and on severity escalation, and a resolution notice fires when a fingerprint disappears for two consecutive ticks.
- All sources are behind traits so each check is unit-testable against fixtures without a running stack.

## Module 1: liveness

A deployment is frozen when its head stops moving while the chain it indexes has events the deployment should have processed. The module answers that question per deployment every tick, using the deployment's own manifest to decide what "should have processed" means.

**Inputs per deployment**

- `indexingStatuses` from the index-node: `health`, `synced`, `fatalError`, `chains[].latestBlock`, `chains[].chainHeadBlock`, `chains[].earliestBlock`.
- Postgres `subgraphs.subgraph_deployment` (`latest_ethereum_block_number`, `health`, `failed`) and `subgraphs.deployment_schemas` (`id`, `network`, `shard`, `active`) as a cross-check on the GraphQL answer and for assignment to node id.
- The manifest from IPFS (cached on disk by deployment hash): data-source addresses and event topics, `startBlock`, whether it has call handlers, block handlers or dynamic data sources (templates).
- Chain head and `eth_getLogs` counts from the local RPC.

**Algorithm (per deployment, per tick)**

1. Read `latestBlock` and `chainHeadBlock`. If `latestBlock` advanced since the last tick, record progress and stop. Healthy.
2. If it did not advance and `health != healthy`, that is a known failure; emit `info` only (the agent already sees it). Stop.
3. If it did not advance, `health == healthy`, and the deployment is within `lag_tolerance_blocks` (default 50) of chain head, it may simply be caught up. Check whether chain head advanced since the last tick. If chain head also did not move, flag `chain_stalled` at `warn` (the halted-chain case from graph-support #15). Stop.
4. Otherwise the head is behind and not moving. Determine expected activity: run `eth_getLogs` over `[latestBlock+1, min(latestBlock+window, chainHead)]` filtered by the manifest's static addresses and topic0s. `window` defaults to 2,000 blocks and is halved on RPC range errors.
5. If the log count is zero and the manifest has no block handlers and no templates, the deployment may legitimately be idle. Hold the finding as `idle_candidate`. Promote to `warn` only after `idle_patience` (default 3 ticks) with zero logs *and* non-advancing head, because graph-node still has to advance the head pointer through empty blocks.
6. If the log count is non-zero, or the manifest has block handlers (every block is activity), or templates exist (unknown addresses), flag `frozen` at `critical` on the first observation and `confirmed_frozen` on the second consecutive tick.
7. On `confirmed_frozen`, if `[liveness] restart = true`, run `graphman --config <path> restart <deployment>` (or the configured command template), record the action, and suppress further restarts for that deployment for `restart_cooldown` (default 30 min). If the head still does not move after the cooldown, escalate with `restart_ineffective`.

**Why eth\_getLogs and not the Postgres block cache**

The block cache tells you what graph-node has already fetched, which is exactly the thing that stops when the runner dies. The RPC is the independent witness. Cost is one `eth_getLogs` call per stalled deployment per tick, nothing for healthy ones, so the steady state is near zero.

**Edge cases**

- Deployments being copied or grafted show a non-advancing head on the destination during copy. Skip deployments whose `subgraphs.copy_state` row is active.
- Paused deployments (`graphman pause` or agent `never` rules) are skipped if the assignment is paused in `subgraphs.subgraph_deployment_assignment`.
- Non-EVM chains (Firehose-only, Near, Cosmos): the activity probe is not available. Fall back to head-vs-chain-head only, with a longer patience, and say so in the finding.
- Substreams-powered subgraphs were removed in v0.42.0; nothing to do.
- A deployment whose only data source is a template with no static addresses: treat as `unknown_activity`, use head-only logic.

**Findings emitted**

| Finding | Severity | Evidence |
| --- | --- | --- |
| `frozen` | critical | head, chain head, log count, window, manifest summary |
| `confirmed_frozen` | critical | as above plus ticks observed |
| `restart_issued` / `restart_ineffective` | warn / critical | command, exit code, head before and after |
| `chain_stalled` | warn | chain, head, last movement time |
| `idle_candidate` | info | ticks idle, log count |
| `unknown_activity` | info | reason |

## Module 2: POI self-consistency gate

A POI that repeats across epochs is fine when the subgraph had nothing to do, and suspect when the chain shows events the subgraph's data sources would have handled. The module classifies every allocation's current POI into one of those two buckets before the agent acts on it, using only the indexer's own RPC. It does not need any peer's POI.

**Inputs**

- The indexer's open allocations from the network subgraph: `allocations(where: {indexer: $me, status: Active}) { id subgraphDeployment { ipfsHash } createdAtEpoch createdAtBlockNumber poi }` plus the indexer's closed allocations for history.
- Epoch boundaries from the Epoch Block Oracle subgraph: `epoches` → `(epochNumber, startBlock)` per network. The POI block for epoch E on network N is the EBO's start block for E on N, minus whatever offset the agent uses (it uses the EBO block directly).
- `proofOfIndexing(subgraph, blockHash, blockNumber, indexer)` from the index-node for the current epoch block and the previous one. The module also records `publicProofsOfIndexing` for the same blocks for later peer comparison.
- The manifest (shared cache with Module 1).
- `eth_getLogs` counts over `[prevEpochBlock+1, epochBlock]` filtered by the manifest's addresses and topics.

**Classification**

| Case | Condition | Verdict |
| --- | --- | --- |
| Changed | POI(E) ≠ POI(E−1) | `ok` |
| Dormant repeat | POI(E) == POI(E−1), zero matching logs, no block handlers, no templates | `dormant` (info) |
| Suspect repeat | POI(E) == POI(E−1), matching logs > 0 | `suspect` (critical) |
| Suspect repeat, unknown | POI(E) == POI(E−1), block handlers or templates present | `suspect_unknown` (warn) |
| Not computable | index-node returns null POI (head below epoch block, or pruned) | `unavailable` (warn) with reason |

This is the discriminator from graph-support #20 applied to the indexer's own data. It cannot be fooled by a graph-node version change, because it never compares against another indexer.

**Long-lived allocations under Horizon**

The agent presents a POI roughly every 28 days and closes on fee collection. The gate runs on every tick, but it also keeps a per-allocation table of `(epoch, poi, verdict)` so the operator can see the whole run of an allocation, including the twelve-in-a-row pattern that graph-support #51 found after the fact.

**Mode A (v0.1): advisory**

The module emits findings and a per-allocation gauge `lapwing_poi_verdict{allocation,deployment,verdict}`. The operator wires an alert on `suspect` and decides. Nothing is blocked.

**Mode B (v0.3, behind a flag): withhold**

A reverse proxy in front of the index-node's `/graphql` status endpoint, pointed at by the agent's `--graph-node-status-endpoint`. It passes everything through except `proofOfIndexing` queries for allocations whose last verdict is `suspect`; for those it returns `null` with an `extensions.lapwing.reason` field. The agent's behaviour on a null POI must be verified on testnet before this ships (see Open questions). The proxy adds no caching and never touches query traffic, which goes to a different port.

**Pre-close hook**

The agent can be configured to require a healthy POI through its own rules, but there is no hook for an external check. v0.2 adds `lapwing poi check <allocation>` as a CLI command that exits non-zero on `suspect`, so operators who close allocations through scripts or indexer-tools can gate on it.

**Peer comparison (later, opt-in)**

If two or more indexers run lapwing on the same graph-node minor version, they can expose `publicProofsOfIndexing` over HTTPS and compare. This is a separate RFC; v0.1 only records the data needed.

## Module 3: REO eligibility meter

The oracle's rule is simple and the indexer has no way to see where they stand against it. The meter rebuilds the oracle's daily count from local data, projects the rolling window under both the current and the announced criteria, and ranks what to add.

**The rule, as of 2026-10-22** (re-read [ELIGIBILITY\_CRITERIA.md](https://github.com/graphprotocol/rewards-eligibility-oracle) before hard-coding; it changes with \~14 days' notice)

- A query qualifies if: HTTP 200, latency < 5,000 ms, the indexer's head for that deployment within 50,000 blocks of chain head at query time, and the deployment's curation signal ≥ 500 GRT.
- A day qualifies if the indexer served at least one qualifying query on at least `N` distinct deployments. `N` = 5 now, 10 from 2026-10-27, 20 planned.
- An indexer is eligible if ≥5 qualifying days exist in the trailing 28-day window. Eligibility renews daily and lasts 14 days. Each daily run re-judges every day in the window under the criteria in force at run time, so a criteria change applies retroactively.

**What the indexer can see locally**

The oracle judges from the gateway's side. The indexer sees receipts, not gateway-measured latency. So the meter computes two numbers:

1. **Lower bound (receipts only):** distinct deployments with ≥1 receipt in `tap_horizon_receipts` (and `scalar_tap_receipts` if V1 tables remain) for that UTC day, intersected with deployments whose signal ≥500 GRT. This is what the indexer served and was paid for; it ignores latency and freshness.
2. **Estimate (with the query tap):** when Module 5 (the query-path tap, a v0.2 addition) is running, filter further by local latency <4,000 ms (a margin under the oracle's 5,000) and local head within 40,000 blocks (margin under 50,000). This approximates the oracle's view.

Signal per deployment comes from the network subgraph: `subgraphDeployments(where: {ipfsHash_in: [...]}) { ipfsHash signalledTokens }`, cached hourly.

**Receipts query sketch**

```sql
select allocation_id, count(*) as receipts, min(timestamp_ns), max(timestamp_ns)
from tap_horizon_receipts
where timestamp_ns >= $day_start_ns and timestamp_ns < $day_end_ns
group by allocation_id;
```

Allocation id maps to deployment through the network subgraph (cached). Under Horizon the receipt carries `collection_id`; the module handles both column names.

**Projection**

For each day in the trailing 28 and for the next 14, the meter computes `qualifying_days` under the current `N` and under each announced `N` with its effective date. It outputs: eligible now (yes/no), days to ineligibility if nothing changes, and the smallest number of additional deployments needed per day to stay eligible after the next step.

**Candidate ranking**

When the projection shows a shortfall, the meter lists deployments to add, ranked by a cost proxy:

- signal ≥ 500 GRT (hard filter);
- on a chain the indexer already runs (zero new RPC cost);
- fewest existing allocations (less competition for the gateway's selection, which raises the chance of actually receiving queries);
- smallest estimated sync cost from the manifest: `startBlock` recency, number of data sources, no call handlers, no templates;
- already synced locally (an `offchain` rule or a paused deployment) ranks first.

The ranking is advisory. It never allocates.

**Findings and metrics**

| Name | Type | Labels |
| --- | --- | --- |
| `lapwing_reo_qualifying_deployments` | gauge | `day`, `method` (lower\_bound / estimate) |
| `lapwing_reo_qualifying_days_28d` | gauge | `criteria` (current / 2026-10-27 / planned) |
| `lapwing_reo_eligible` | gauge (0/1) | `criteria` |
| `lapwing_reo_shortfall_deployments` | gauge | `criteria` |
| `reo_at_risk` | finding, warn | days remaining, shortfall |
| `reo_ineligible` | finding, critical | first ineligible day |

**Daily rollup**

At 00:05 UTC the meter freezes the previous day's counts into sqlite so a receipts table that is later pruned or rotated does not erase history. `lapwing reo history` prints the last 28 days.

## Module 4: payment-path canary

Every 2026 payment incident had a signature visible minutes before it cost anything: a gRPC endpoint returning HTML, a sender address in receipts that no config file mentions, a denylist row appearing, receipts arriving with no RAV requested. The canary watches for those signatures directly.

**Checks**

1. **Aggregator reachability.** For each entry in `[tap.sender_aggregator_endpoints]`, open a tonic channel and call the `TapAggregator` service's health or a zero-receipt `aggregate_receipts` request with a deliberately invalid signature, expecting a gRPC `InvalidArgument` rather than a transport error. A Cloudflare 526 or any HTTP body arrives as a transport-level failure (`invalid compression flag`, `h2 protocol error`), which is the exact signature of graph-support #30. Emit `aggregator_down` at `critical` after two consecutive failures 60 s apart.
2. **Sender drift.** Query distinct `signer_address` and `sender` (via `tap_horizon_receipts` and the escrow accounts table `tap_horizon_escrow_accounts` or the subgraph-derived senders) seen in the last 24 h. Diff against the configured senders. A sender with receipts and no aggregator endpoint is `unknown_sender` at `critical` with the address and first-seen time. This catches a rotation like 2026-09-24 within one tick.
3. **Denylist.** Poll `tap_horizon_denylist` (and `scalar_tap_denylist`). Any row is `sender_denylisted` at `critical`, with the unaggregated-fees figure at the time. Resolution fires when the row disappears.
4. **Unaggregated fees vs threshold.** Sum unaggregated receipt values per sender from `tap_horizon_receipts` not yet covered by the latest RAV in `tap_horizon_ravs`. Compare to `max_amount_willing_to_lose_grt` from the tap-agent config. Emit `fees_near_limit` at `warn` at 60% and `critical` at 85%. This fires before the agent denylists anyone.
5. **Frozen RAV requester.** Per sender, track `max(timestamp_ns)` of receipts vs `max(updated_at)` of RAVs. If receipts keep arriving but no RAV has been requested for `rav_stall` (default 20 min while receipts flow), emit `rav_stalled` at `critical`. This is the indexer-rs #1094 signature.
6. **Escrow headroom.** Read escrow balance per sender from the PaymentsEscrow contract via RPC on Arbitrum (or the escrow subgraph). Emit `escrow_low` at `warn` when balance < 2× the trailing 24 h fees. Low escrow means the gateway will throttle routing to you.
7. **Config lint.** Parse indexer-service and tap-agent `config.toml`: `graph_tally_collector_address` not a placeholder, `escrow_subgraph` / `network_subgraph` URLs resolve and are synced, `trusted_senders` consistent across the two files, `max_amount_willing_to_lose_grt` not absurdly low. Emit `config_suspect` at `warn`. Run at startup and on file change (inotify).
8. **Service self-check.** Probe indexer-service's own health endpoint and tap-agent's `/metrics`; scrape `tap_unaggregated_fees`, `tap_ravs_created_total` and friends so lapwing can corroborate its Postgres view and alert when tap-agent is simply down.

**What it does not do**

It never modifies the denylist, config files or escrow. If an operator wants auto-remediation (for example removing a denylist row once the aggregator recovers) that is a v0.3 discussion with the same confirmation gating as the liveness restart.

**Metrics**

| Name | Type | Labels |
| --- | --- | --- |
| `lapwing_tap_aggregator_up` | gauge (0/1) | `sender`, `endpoint` |
| `lapwing_tap_unaggregated_grt` | gauge | `sender` |
| `lapwing_tap_unaggregated_ratio` | gauge (0–1 of threshold) | `sender` |
| `lapwing_tap_rav_age_seconds` | gauge | `sender` |
| `lapwing_tap_denylisted` | gauge (0/1) | `sender` |
| `lapwing_tap_escrow_grt` | gauge | `sender` |
| `lapwing_tap_unknown_senders` | gauge | – |

## Configuration and deployment

One TOML file, every section optional except `[sources]`. A missing section disables that module. Secrets come from env vars or files, never inline.

```toml
[sources]
postgres_url = "postgres://lapwing@localhost/graph-node"   # read-only role
index_node_url = "http://graph-node:8030/graphql"
network_subgraph_url = "https://gateway.thegraph.com/network" # or your own
ebo_subgraph_url = "https://gateway.thegraph.com/ebo"
ipfs_url = "https://ipfs.thegraph.com"
indexer_address = "0x…"

[sources.rpc]
"arbitrum-one" = "http://erpc:4000/arbitrum"
"mainnet" = "http://erpc:4000/mainnet"

[liveness]
interval = "5m"
lag_tolerance_blocks = 50
activity_window_blocks = 2000
idle_patience = 3
restart = false
restart_command = "graphman --config /etc/graph-node/config.toml restart {deployment}"
restart_cooldown = "30m"

[poi]
interval = "15m"
mode = "advisory"            # advisory | withhold (v0.3)

[reo]
interval = "1h"
min_signal_grt = 500
criteria = [
  { from = "2026-10-06", subgraphs_per_day = 5 },
  { from = "2026-10-27", subgraphs_per_day = 10 },
]

[tap]
interval = "2m"
indexer_service_config = "/etc/indexer-service/config.toml"
tap_agent_config = "/etc/tap-agent/config.toml"
tap_agent_metrics = "http://tap-agent:7300/metrics"
fees_warn_ratio = 0.6
fees_critical_ratio = 0.85
rav_stall = "20m"

[outputs]
metrics_listen = "0.0.0.0:9477"
state_db = "/var/lib/lapwing/state.sqlite"

[[outputs.webhook]]
kind = "slack"
url_env = "LAPWING_SLACK_URL"
min_severity = "warn"

[[outputs.webhook]]
kind = "generic"
url = "http://alertmanager:9093/api/v2/alerts"
min_severity = "info"
```

**Postgres role**

```sql
create role lapwing login password '…';
grant connect on database "graph-node" to lapwing;
grant usage on schema subgraphs, public to lapwing;
grant select on all tables in schema subgraphs to lapwing;
grant select on tap_horizon_receipts, tap_horizon_ravs, tap_horizon_denylist,
  tap_horizon_escrow_accounts to lapwing;
-- chain schemas (chain1, chain2, …) only if block-cache checks are enabled later
```

On a sharded setup lapwing connects to the primary shard; the `subgraphs` schema lives there and it only needs per-deployment metadata, not entity tables.

**Packaging**

- Static musl binary, `ghcr.io/nuthatch-org/lapwing` image, `linux/amd64` and `linux/arm64`.
- A Helm chart for Launchpad users (one Deployment, one Service, a ServiceMonitor, a ConfigMap, secrets via existing Launchpad conventions) and a docker-compose snippet for StakeMachine-style hosts.
- `lapwing doctor` validates config, tests every source connection, and prints the Postgres grants it is missing. This is the first thing a new operator runs.

**Resource budget**

Target under 50 MB RSS and negligible CPU. Per tick: a handful of cheap Postgres queries, one GraphQL call to the index-node, zero RPC calls unless a deployment is stalled or an epoch boundary is near. The REO rollup is the heaviest query and runs hourly.

## Outputs: metrics, alerts, CLI

The daemon speaks Prometheus, webhooks and a CLI. It does not have a web UI; Grafana, Lodestar or the operator's existing dashboard render the numbers.

**Prometheus**

All gauges above plus `lapwing_findings{check,severity}` (count of active findings), `lapwing_check_duration_seconds{check}`, `lapwing_source_up{source}` and `lapwing_build_info`. A bundled Grafana dashboard JSON and an Alertmanager rules file ship in the repo. Suggested default alerts:

| Alert | Expression | For |
| --- | --- | --- |
| LapwingFrozenDeployment | `lapwing_findings{check="liveness",severity="critical"} > 0` | 0m |
| LapwingSuspectPOI | `lapwing_poi_verdict{verdict="suspect"} == 1` | 0m |
| LapwingREOAtRisk | `lapwing_reo_shortfall_deployments{criteria="next"} > 0` | 1h |
| LapwingAggregatorDown | `lapwing_tap_aggregator_up == 0` | 2m |
| LapwingFeesNearLimit | `lapwing_tap_unaggregated_ratio > 0.6` | 0m |
| LapwingRAVStalled | `lapwing_tap_rav_age_seconds > 1200 and rate(receipts) > 0` | 0m |
| LapwingUnknownSender | `lapwing_tap_unknown_senders > 0` | 0m |

**Webhooks**

Each finding is posted once on first sight, once per severity escalation, once on resolution. Payload is the `Finding` struct as JSON plus a one-line human summary. Slack and Telegram formatters exist; `generic` posts the raw JSON and is what Alertmanager, PagerDuty bridges or a Lodestar ingest endpoint consume.

Rate limiting: at most one webhook per fingerprint per `min_repeat` (default 6 h) unless severity changes. A storm guard caps posts at 30 per minute and collapses the rest into a single summary.

**CLI**

| Command | What it prints |
| --- | --- |
| `lapwing run` | the daemon |
| `lapwing doctor` | source connectivity, grants, config lint |
| `lapwing status` | one table: every active finding, by check and severity |
| `lapwing explain <finding-id>` | the evidence JSON, pretty-printed, and the exact queries and RPC calls that produced it |
| `lapwing liveness <deployment>` | run the liveness check for one deployment now |
| `lapwing poi check <allocation>` | run the POI gate for one allocation; exit 2 on `suspect` (for scripts) |
| `lapwing poi history <allocation>` | `(epoch, poi, verdict)` rows |
| `lapwing reo` | today's count, both methods, projection under each criteria set |
| `lapwing reo candidates` | ranked deployments to add |
| `lapwing reo history` | 28 days |
| `lapwing tap` | aggregator status, senders, unaggregated fees, RAV ages, escrow |

Every command has `--json`. `explain` exists so an operator can paste evidence into a forum thread or a dispute response without reconstructing it.

## Security, permissions, failure modes

Lapwing is a monitor. The design goal is that a compromised or buggy lapwing can at worst produce wrong alerts and, if the operator opted in, restart a deployment that did not need restarting.

**Permissions**

- Postgres: `SELECT` only, on the schemas listed above. No `graph-node` superuser, no entity tables.
- No operator key, no indexer key, no wallet. It reads the chain; it never signs.
- `graphman restart` requires the graph-node config file and a Postgres role with write access. Lapwing does not hold that role itself; it execs the configured command, which runs under whatever credentials the operator gives that command (a wrapper script, a sidecar with its own secret). The restart path is off by default.
- Mode B (withhold proxy) sits on the agent's status-endpoint path. It is a new point of failure for the agent, so it fails open: if lapwing cannot evaluate a POI it passes the query through unchanged. Only a positive `suspect` verdict withholds.
- Webhook URLs come from env or files. Logs never print them.

**Failure modes and what lapwing does**

| Failure | Behaviour |
| --- | --- |
| Postgres unreachable | all checks mark `source_down`, no other findings change state, `lapwing_source_up{source="postgres"}=0` |
| index-node unreachable | liveness falls back to Postgres head only; POI gate reports `unavailable` |
| RPC unreachable or rate-limited | liveness cannot confirm activity → stays at `frozen` (not `confirmed`), no restart; POI gate reports `unavailable` with reason; backoff with jitter |
| Network subgraph stale or down | REO signal filter uses last cached values with an `stale_signal` note; sender list from config only |
| Aggregator check times out | counted as a failure; two consecutive failures → `aggregator_down` |
| sqlite state corrupt | rebuilt from scratch; history lost; one `warn` finding says so |
| Clock skew | all day boundaries are UTC from the host clock; `doctor` checks NTP drift against the RPC's latest block timestamp |
| lapwing itself crashes | systemd / k8s restarts it; findings are reloaded from sqlite so no duplicate first-sight alerts |

**False positives to expect and how they are bounded**

- Liveness on a slow chain with sparse events: bounded by `idle_patience` and by requiring non-zero logs before `critical`.
- POI gate on a subgraph with block handlers: never `critical`, only `suspect_unknown` at `warn`.
- REO lower bound overstating eligibility: it is labelled `lower_bound` and the projection uses the stricter estimate when the query tap is available.
- Sender drift during a planned rotation: the operator adds the new sender to config and the finding resolves on the next tick.

**Privacy**

All data stays on the indexer's host. Nothing is phoned home. If a later version offers opt-in POI sharing, it is a separate flag with its own RFC.

## Implementation plan and milestones

Order is by money at risk divided by effort. The REO meter is smallest and has a hard date, so it ships first even though liveness is the deeper problem.

&#91;embedded content: lapwing build plan · 4 milestones to v0.1, two follow-on releases\]

The dashed line is the REO step to 10 qualifying subgraphs per day. M1 is the only milestone with a deadline; the rest are estimates for one engineer working on this part-time alongside other work.

| Milestone | Contents | Done when |
| --- | --- | --- |
| M0 scaffold + sources | workspace, config, scheduler, sqlite, metrics endpoint, Postgres / index-node / subgraph / RPC clients behind traits, `doctor` | `lapwing doctor` passes against a real StakeMachine or Launchpad host |
| M1 REO meter | receipts rollup, signal cache, projection under multiple criteria, candidate ranking, `reo` CLI | a volunteer indexer's `lapwing reo` number matches their REO dashboard status for the last 7 days |
| M2 TAP canary | aggregator probe, sender drift, denylist, fees ratio, RAV stall, escrow, config lint, `tap` CLI | replaying graph-support #30, #36 and #44 configs produces the expected findings in integration tests |
| M3 liveness | manifest cache, activity probe, state machine, optional restart, `liveness` CLI | a deployment frozen with `pg_terminate_backend` on a test node is flagged within 2 ticks and restarted when enabled |
| v0.1 | M0–M3 plus POI gate advisory mode, Grafana dashboard, Alertmanager rules, Helm chart, compose snippet, README | tagged, image published, two indexers running it |
| v0.2 | query-path tap (transparent proxy on graph-node's query port recording latency, status, attestable flag, block-behind), REO estimate method, `poi check` exit codes | REO estimate within one deployment of the oracle's count on 7 consecutive days for a volunteer |
| v0.3 | withhold proxy for the status endpoint, behind a flag, after testnet verification of agent behaviour on null POI; optional denylist auto-clear with the same gating as restart | testnet run documented in the repo |

**Testing**

- Unit tests per check against fixture structs (no network).
- Integration tests with a docker-compose stack: graph-node v0.45.0, Postgres, a mock aggregator that can be told to return HTML, an anvil chain with a toy subgraph. The frozen-runner case is reproduced by killing the deployment's Postgres backend.
- A `fixtures/incidents/` directory holding anonymised configs and table dumps for each graph-support incident lapwing claims to catch, with a test per incident.

**Validation with real indexers**

Two volunteers (one StakeMachine, one Launchpad) run v0.1 for two weeks in advisory-only mode. The acceptance question is simple: did lapwing say anything true that nothing else said first.

## Open questions and risks

- [ ] **Agent behaviour on a null POI.** Does indexer-agent v0.25.x skip the action, retry, or submit a zero POI when `proofOfIndexing` returns null? Mode B depends on the answer. Needs a testnet run before v0.3; until then Mode B is not offered.
- [ ] **REO criteria drift.** Criteria change about every two weeks. Lapwing ships the current set in config and the repo tracks changes, but an operator who does not update config will get wrong projections. Consider fetching ELIGIBILITY\_CRITERIA.md from GitHub on a schedule and warning when the local table is out of date.
- [ ] **Gateway-side latency is invisible locally.** The REO lower bound ignores latency and freshness; the estimate needs the query tap (v0.2). Until then, operators will see a number that can be higher than the oracle's. The metric name says `lower_bound` and the docs say why, but it will still surprise someone.
- [ ] **Receipt column names.** `tap_horizon_receipts` schema differs between indexer-rs 2.0 and 2.2 (`collection_id` vs `allocation_id`, signer columns). Need a schema probe at startup and a compatibility matrix; test against 2.0.2, 2.1.0, 2.2.1.
- [ ] **Epoch block resolution.** The agent takes the POI block from the EBO; the gate must use exactly the same block or verdicts are meaningless. Confirm by reading the agent's `resolvePOI` path and pinning the logic with a test against a known closed allocation.
- [ ] **Manifest topic extraction.** Event signatures in the manifest are human-readable (`Transfer(indexed address,indexed address,uint256)`); lapwing must canonicalise and keccak them exactly as graph-node does, including tuple and indexed handling. Reuse graph-node's `EventSignature` parsing via a small vendored module or a crate dependency on `graph` if its API is stable enough.
- [ ] **Dynamic data sources.** Templates make the activity probe incomplete. Reading `subgraphs.dynamic_ethereum_contract_data_source` (or the v0.4x equivalent) from Postgres would give the actual address set. Worth doing in v0.2 if operators hit false `suspect_unknown` often.
- [ ] **Restart safety.** `graphman restart` on a deployment that is mid-copy or mid-graft could be harmful. The copy-state check covers copies; confirm the graft case and the behaviour on a deployment with a pending `unassign`.
- [ ] **Name.** Lapwing is a placeholder. Check for collisions in the ecosystem before publishing.
- [ ] **Adoption.** Indexers are tired of sidecars. The pitch has to be one binary, one config, and a finding on day one that saved them something. The REO meter is the day-one finding; make sure `lapwing reo` works with nothing but a Postgres URL and an indexer address.

**Risks**

| Risk | Likelihood | Mitigation |
| --- | --- | --- |
| Upstream fixes the frozen-runner bug soon | medium | liveness still catches halted chains, RPC-starved deployments and future regressions; the module stays |
| REO is redesigned or sunset | low-medium | the meter is one module; the rest stands |
| Schema changes in indexer-rs break the TAP canary | high over 6 months | schema probe + compatibility tests per release; the canary degrades to aggregator and config checks when tables are unknown |
| False `suspect` POI verdicts erode trust | medium | advisory only in v0.1; `explain` shows the logs that triggered it; `suspect_unknown` is never critical |
| Solo maintainer bandwidth | high | keep v0.1 to the four modules; no UI; reuse Lodestar for rendering |

## Appendix: queries, schemas, references

**A. Core types**

```rust
pub enum Severity { Info, Warn, Critical }

pub struct Finding {
    pub check: &'static str,          // "liveness" | "poi" | "reo" | "tap"
    pub kind: &'static str,           // "frozen", "suspect", "reo_at_risk", ...
    pub subject: Subject,             // Deployment(hash) | Allocation(id) | Sender(addr) | Chain(name) | Global
    pub severity: Severity,
    pub fingerprint: String,          // stable across ticks: check + kind + subject
    pub summary: String,              // one line, human
    pub evidence: serde_json::Value,  // everything `explain` prints
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

#[async_trait]
pub trait Check: Send + Sync {
    fn name(&self) -> &'static str;
    fn interval(&self) -> Duration;
    async fn run(&self, ctx: &Ctx) -> Result<Vec<Finding>>;
}
```

**B. index-node queries**

```graphql
{ indexingStatuses {
    subgraph health synced fatalError { message block { number } }
    chains { network earliestBlock { number } latestBlock { number hash } chainHeadBlock { number } }
    node paused
} }
```

```graphql
query($d: String!, $h: Bytes!, $n: Int!, $i: String) {
  proofOfIndexing(subgraph: $d, blockHash: $h, blockNumber: $n, indexer: $i)
}
```

**C. Network subgraph queries**

```graphql
query($me: String!) { allocations(first: 1000, where: {indexer: $me, status: Active}) {
  id createdAtEpoch createdAtBlockNumber poi allocatedTokens
  subgraphDeployment { ipfsHash signalledTokens indexerAllocations { id } }
} }
```

```graphql
query($hashes: [String!]) { subgraphDeployments(first: 1000, where: {ipfsHash_in: $hashes}) {
  ipfsHash signalledTokens indexerAllocations(where: {status: Active}) { id }
} }
```

**D. EBO subgraph**

```graphql
{ epoches(first: 5, orderBy: epochNumber, orderDirection: desc) {
  epochNumber startBlock blockNumbers { network { id } blockNumber }
} }
```

**E. Postgres (subgraphs schema)**

```sql
-- head per deployment with assignment and copy state
select d.deployment, d.latest_ethereum_block_number, d.health, d.failed, d.synced,
       s.network, s.shard, a.node_id, a.paused_at,
       exists(select 1 from subgraphs.copy_state c where c.dst = s.id and c.finished_at is null) as copying
from subgraphs.subgraph_deployment d
join subgraphs.deployment_schemas s on s.id = d.id
left join subgraphs.subgraph_deployment_assignment a on a.id = d.id
where s.active;
```

Column names drift between graph-node releases; the sources crate carries a per-version schema map and `doctor` reports the detected version.

**F. TAP tables (indexer-rs 2.2.x, verify per release)**

- `tap_horizon_receipts(id, signer_address, signature, collection_id, payer, data_service, service_provider, timestamp_ns, nonce, value)`
- `tap_horizon_ravs(collection_id, payer, data_service, service_provider, signature, last, final, value_aggregate, updated_at)`
- `tap_horizon_denylist(sender_address)`
- `tap_horizon_escrow_accounts(...)` if materialised; otherwise the escrow subgraph

**G. References**

- [graph-support #51](https://github.com/nightswatchhq/graph-support/issues/51) frozen-but-healthy deployments; [graph-node PR #6730](https://github.com/graphprotocol/graph-node/pull/6730)
- [graph-support #20](https://github.com/nightswatchhq/graph-support/issues/20) POI reuse analysis and the dormant/suspect discriminator
- [graph-support #15](https://github.com/nightswatchhq/graph-support/issues/15) halted chain looks healthy
- [graph-support #30](https://github.com/nightswatchhq/graph-support/issues/30), [#31](https://github.com/nightswatchhq/graph-support/issues/31) aggregator outage → denylist
- [graph-support #44](https://github.com/nightswatchhq/graph-support/issues/44) sender rotation; [#36](https://github.com/nightswatchhq/graph-support/issues/36) collector address default; [#46](https://github.com/nightswatchhq/graph-support/issues/46) REO counting method
- [indexer-rs #1094](https://github.com/graphprotocol/indexer-rs/issues/1094) frozen receipt ingestion; [#1117](https://github.com/graphprotocol/indexer-rs/issues/1117) escrow snapshot validation; [indexer PR #1247](https://github.com/graphprotocol/indexer/pull/1247) partial RAV settlement
- [rewards-eligibility-oracle](https://github.com/graphprotocol/rewards-eligibility-oracle): [PR #154](https://github.com/graphprotocol/rewards-eligibility-oracle/pull/154), [#176](https://github.com/graphprotocol/rewards-eligibility-oracle/pull/176), [#179](https://github.com/graphprotocol/rewards-eligibility-oracle/pull/179)
- [Horizon upgrade, indexer action required](https://forum.thegraph.com/t/graph-horizon-arbitrum-one-upgrade-indexers-action-required/6763); [Q3 2026 in review](https://thegraph.com/blog/q3-2026-in-review/); [last IOH tl;dr](https://forum.thegraph.com/t/last-ioh-tldr-transition-week-update/7054)
- [graph-node releases](https://github.com/graphprotocol/graph-node/releases) and [NEWS.md](https://github.com/graphprotocol/graph-node/blob/master/NEWS.md); [indexer releases](https://github.com/graphprotocol/indexer/releases)
- Dormant comparison tools: [graphix](https://github.com/edgeandnode/graphix), [subgraph-radio](https://github.com/graphops/subgraph-radio), [graphprotocol-poi-checker](https://github.com/p2p-org/graphprotocol-poi-checker)
- Related nightswatch projects: [lodestar](https://github.com/nightswatchhq/lodestar), [foghorn](https://github.com/nightswatchhq/foghorn), [waxwing](https://github.com/nightswatchhq/waxwing)

## Amendments from implementation

Found while building M1 and checked against upstream source on 2026-10-23. Where these disagree
with the body above, these win.

1. **The oracle has no signal filter.** `bigquery_provider.py` counts distinct deployments with a
   qualifying query (`200 OK`, latency < 5,000 ms, < 50,000 blocks behind). There is no
   `signal >= 500 GRT` term. `[reo] min_signal_grt` now defaults to 0 and only filters the
   candidate ranking.
2. **Receipts give an upper bound, not a lower one.** Every gateway query carries a receipt
   whether or not it qualified, so a receipts count can only overstate the oracle's. The method
   is called `receipts` and documented as an upper bound.
3. **Receipts are deleted once a RAV covers them.** `tap-agent`'s `sender_allocation.rs` deletes
   `tap_horizon_receipts` rows in the RAV's timestamp range, so counting a past day from the table
   undercounts. The meter keeps its own per-day ledger fed by `LISTEN
   tap_horizon_receipt_notification` (tap-agent's own channel, no grant needed), a periodic scan,
   and each RAV's `timestamp_ns`. History begins when lapwing starts.
4. **The window is 29 calendar days.** The run on day D reads `day_partition BETWEEN D-28 AND D`
   inclusive, and runs at about 10:00 UTC, so day D is partial.
5. **TAP tables may live in a different database from graph-node's.** `[sources]` takes
   `indexer_postgres_url` alongside `postgres_url`.
6. **On-chain eligibility is read directly.** `RewardsEligibilityOracle` on Arbitrum One
   (`0x02753BaE61C08AbD4351Bce7F48524935C2Cc78E`) exposes `isEligible(address)`,
   `getEligibilityRenewalTime(address)` and `getLastOracleUpdateTime()`; `lapwing reo` reports
   these beside the projection, and the chain is authoritative for "ineligible now".
