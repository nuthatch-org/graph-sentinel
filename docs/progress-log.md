# Progress log

Newest first.

## 2026-10-23: M1 started

- Workspace scaffolded per the RFC's crate layout.
- REO rule model, projection and candidate ranking in `lapwing-checks::reo`, 11 tests, including
  the retroactive step to 10 and the 29-day inclusive window.
- Checked the RFC against upstream; six amendments recorded at the end of the RFC. The two that
  change the design: the oracle has no signal filter, and tap-agent deletes receipts after RAV
  aggregation, so the meter keeps its own ledger.
- Next: Postgres receipts ledger (LISTEN plus scan plus RAVs), network subgraph allocation map,
  on-chain REO read, `lapwing reo` and `lapwing doctor`.
