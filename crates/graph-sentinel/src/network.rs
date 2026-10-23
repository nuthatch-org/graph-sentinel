//! The network module as a service: network subgraph for this indexer and this fisherman,
//! Lodestar's public price and chain lag for context.

use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use sentinel_checks::network::{self, AllocationView, DisputeView};
use sentinel_sources::lodestar::{ChainLag, Lodestar, Price};
use sentinel_sources::network::{Dispute, NetworkSubgraph, Overview};
use sentinel_types::{Address, Finding};
use serde::Serialize;

use crate::config::Config;

#[derive(Debug, Clone, Serialize)]
pub struct IndexerView {
    pub address: String,
    pub overview: Option<Overview>,
    /// Open allocations, nearest POI deadline first.
    pub allocations: Vec<AllocationView>,
    pub disputes: Vec<DisputeView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FishermanView {
    pub address: String,
    pub disputes: Vec<DisputeView>,
}

pub struct Reading {
    pub price: Result<Price>,
    pub chain_lag: Result<HashMap<String, ChainLag>>,
    pub indexer: Option<Result<IndexerView>>,
    pub fisherman: Option<Result<FishermanView>>,
}

pub struct NetworkWatch {
    subgraph: NetworkSubgraph,
    lodestar: Lodestar,
    indexer: Option<Address>,
    fisherman: Option<Address>,
}

fn view(d: Dispute) -> DisputeView {
    // The network subgraph returns nonsense cancellableAt for some query disputes; keep only
    // values that are plausibly timestamps.
    let sane = |t: Option<i64>| t.filter(|t| (1_500_000_000..4_000_000_000).contains(t));
    DisputeView {
        id: d.id,
        kind: d.kind,
        status: d.status,
        indexer: d.indexer,
        fisherman: d.fisherman,
        deployment: d.deployment,
        created_at: d.created_at,
        closed_at: d.closed_at,
        cancellable_at: sane(d.cancellable_at),
        deposit_grt: d.deposit_grt,
        slashed_grt: d.slashed_grt,
    }
}

impl NetworkWatch {
    pub fn new(cfg: &Config) -> Self {
        NetworkWatch {
            subgraph: NetworkSubgraph::new(
                cfg.sources.network_subgraph(),
                cfg.sources.network_subgraph_token(),
            ),
            lodestar: Lodestar::new(cfg.network.lodestar_url.clone()),
            indexer: cfg.sources.indexer_address.clone(),
            fisherman: cfg.network.fisherman_address.clone(),
        }
    }

    async fn indexer_view(&self, a: &Address) -> Result<IndexerView> {
        let now = Utc::now().timestamp();
        let overview = self.subgraph.indexer_overview(a).await?;
        let mut allocations: Vec<AllocationView> = self
            .subgraph
            .open_allocations(a)
            .await?
            .into_iter()
            .map(|x| AllocationView {
                poi_deadline_in: network::poi_deadline_in(x.created_at, x.latest_poi_at, now),
                id: x.id,
                deployment: x.deployment,
                network: x.network,
                allocated_grt: x.allocated_grt,
                rewards_grt: x.rewards_grt,
                created_at: x.created_at,
                latest_poi_at: x.latest_poi_at,
            })
            .collect();
        allocations.sort_by_key(|x| (x.allocated_grt <= 0.0, x.poi_deadline_in));
        let disputes = self
            .subgraph
            .disputes(a, false)
            .await?
            .into_iter()
            .map(view)
            .collect();
        Ok(IndexerView {
            address: a.to_string(),
            overview,
            allocations,
            disputes,
        })
    }

    pub async fn read(&self) -> Reading {
        let indexer = match &self.indexer {
            Some(a) => Some(self.indexer_view(a).await),
            None => None,
        };
        let fisherman = match &self.fisherman {
            Some(a) => Some(
                self.subgraph
                    .disputes(a, true)
                    .await
                    .map(|d| FishermanView {
                        address: a.to_string(),
                        disputes: d.into_iter().map(view).collect(),
                    }),
            ),
            None => None,
        };
        Reading {
            price: self.lodestar.price().await,
            chain_lag: self.lodestar.chain_lag().await,
            indexer,
            fisherman,
        }
    }
}

pub fn findings(r: &Reading) -> Vec<Finding> {
    let now = Utc::now();
    let mut out = Vec::new();
    if let Some(Ok(i)) = &r.indexer {
        out.extend(network::allocation_findings(&i.allocations, now));
        out.extend(network::against_findings(&i.disputes, now));
    }
    if let Some(Ok(f)) = &r.fisherman {
        out.extend(network::filed_findings(&f.disputes, now));
    }
    out
}
