//! The REO meter as a service: feeds the ledger, then reports and ranks from it.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Days, Utc};
use sentinel_checks::reo::ranking::{Candidate, rank};
use sentinel_checks::reo::rules::Rules;
use sentinel_checks::reo::{self, OnChain, Report};
use sentinel_sources::graph_node::GraphNodeDb;
use sentinel_sources::network::NetworkSubgraph;
use sentinel_sources::rpc::{Reo, Rpc};
use sentinel_sources::tap::{Served, Tables, TapDb, Via};
use sentinel_types::Address;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::store::Store;

pub struct Meter {
    pub store: Store,
    tap: TapDb,
    tables: Tables,
    network: NetworkSubgraph,
    graph_node: Option<GraphNodeDb>,
    oracle: Option<(Rpc, Address)>,
    indexer: Address,
    rules: Rules,
    min_signal_grt: f64,
}

/// How far back a scan looks: one oracle window plus the run day.
const BACKFILL_DAYS: u64 = 29;

impl Meter {
    pub async fn new(cfg: &Config, store: Store) -> Result<Self> {
        let url = cfg
            .sources
            .tap_db_url()
            .context("[sources] needs indexer_postgres_url or postgres_url for the REO meter")?;
        let tap = TapDb::connect(url).await?;
        let tables = tap.tables().await?;
        let graph_node = match &cfg.sources.postgres_url {
            Some(u) => Some(GraphNodeDb::connect(u).await?),
            None => None,
        };
        let oracle = cfg
            .sources
            .rpc
            .get(&cfg.reo.oracle_chain)
            .map(|u| (Rpc::new(u.clone()), cfg.reo.oracle_address.clone()));
        Ok(Meter {
            store,
            tap,
            tables,
            network: NetworkSubgraph::new(
                cfg.sources.network_subgraph_url.clone(),
                cfg.sources.network_subgraph_token(),
            ),
            graph_node,
            oracle,
            indexer: cfg.sources.indexer_address.clone(),
            rules: cfg.reo.rules(),
            min_signal_grt: cfg.reo.min_signal_grt,
        })
    }

    /// Record what the receipt and RAV tables still hold from the last `days` days.
    pub async fn scan(&self, days: u64) -> Result<u64> {
        let now = Utc::now();
        let since = now - chrono::Duration::days(days as i64);
        let since_ns = since.timestamp_nanos_opt().unwrap_or(0);
        let receipts = self.tap.scan_receipts(&self.tables, since_ns).await?;
        let ravs = self.tap.rav_days(&self.tables, since_ns).await?;
        Ok(self.store.record(&receipts, Via::Scan, now).await?
            + self.store.record(&ravs, Via::Rav, now).await?)
    }

    pub async fn backfill(&self) -> Result<u64> {
        self.scan(BACKFILL_DAYS).await
    }

    pub async fn refresh_allocations(&self) -> Result<usize> {
        let map = self.network.allocations(&self.indexer).await?;
        self.store.set_allocations(&map).await?;
        Ok(map.len())
    }

    /// Refresh the allocation map only when the ledger holds allocations it cannot place.
    pub async fn refresh_allocations_if_needed(&self) -> Result<()> {
        if self.store.unmapped().await? > 0 {
            self.refresh_allocations().await?;
        }
        Ok(())
    }

    pub async fn on_chain(&self) -> Result<Option<OnChain>> {
        let Some((rpc, addr)) = &self.oracle else {
            return Ok(None);
        };
        let s = Reo {
            rpc,
            address: addr.clone(),
        }
        .status(&self.indexer)
        .await?;
        Ok(Some(OnChain {
            eligible: s.eligible,
            renewal_time: s.renewal_time,
            eligibility_period_secs: s.eligibility_period_secs,
            last_oracle_update: s.last_oracle_update,
        }))
    }

    pub async fn report(&self) -> Result<Report> {
        let today = Utc::now().date_naive();
        let counts = self
            .store
            .daily_counts(today - Days::new(BACKFILL_DAYS + 1))
            .await?;
        let on_chain = match self.on_chain().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "reading the oracle contract failed");
                None
            }
        };
        let mut r = reo::report(&counts, &self.rules, today, on_chain);
        // The ledger knows only what graph-sentinel has seen; say where its memory starts.
        r.observed_since = self.store.first_day().await?;
        Ok(r)
    }

    pub async fn candidates(&self, limit: usize) -> Result<Vec<Candidate>> {
        let today = Utc::now().date_naive();
        let serving = self
            .store
            .deployments_served_since(today - Days::new(7))
            .await?;
        let (local, chains) = match &self.graph_node {
            Some(g) => (g.deployments().await?, g.chains().await?),
            None => (HashMap::new(), HashSet::new()),
        };
        let pool = self.network.signalled_deployments(1500).await?;
        let candidates = pool
            .into_iter()
            .filter(|d| !serving.contains(&d.deployment))
            .map(|d| {
                let l = local.get(&d.deployment);
                Candidate {
                    synced_locally: l.and_then(|l| l.synced).unwrap_or(false),
                    chain_supported: d.network.as_ref().is_some_and(|n| chains.contains(n)),
                    deployment: d.deployment,
                    network: d.network,
                    signal_grt: d.signal_grt,
                    query_fees_grt: d.query_fees_grt,
                    active_allocations: d.active_allocations,
                    rewards_denied: d.rewards_denied,
                }
            })
            .collect();
        let mut ranked = rank(candidates, self.min_signal_grt);
        ranked.truncate(limit);
        Ok(ranked)
    }

    /// Follow tap-agent's receipt notifications into the ledger until the process ends.
    /// After every reconnect, rescan: notifications sent while disconnected are lost.
    pub async fn follow(&self) {
        loop {
            let (tx, mut rx) = mpsc::channel::<Served>(1024);
            let tap = self.tap.clone();
            let listener = tokio::spawn(async move { tap.listen(tx).await });
            while let Some(s) = rx.recv().await {
                if let Err(e) = self.store.record(&[s], Via::Notify, Utc::now()).await {
                    tracing::warn!(error = %e, "recording a receipt notification failed");
                }
            }
            match listener.await {
                Ok(Err(e)) => tracing::warn!(error = %e, "receipt listener dropped"),
                Err(e) => tracing::warn!(error = %e, "receipt listener panicked"),
                Ok(Ok(())) => {}
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            if let Err(e) = self.scan(2).await {
                tracing::warn!(error = %e, "rescan after reconnect failed");
            }
        }
    }
}

pub fn days_left(r: &Report) -> Option<i64> {
    r.if_traffic_holds
        .eligible_until
        .map(|u| (u - r.today).num_days())
}
