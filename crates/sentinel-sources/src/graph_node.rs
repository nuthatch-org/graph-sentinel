//! graph-node's primary-shard metadata. Read-only; never the entity tables.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use sentinel_types::DeploymentId;
use sqlx::postgres::{PgPool, PgPoolOptions};

#[derive(Clone)]
pub struct GraphNodeDb {
    pool: PgPool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// v0.39+ (migration 2025-05-13 split_subgraph_deployment): `subgraphs.deployment` + `subgraphs.head`.
    Split,
    /// Earlier releases: `subgraphs.subgraph_deployment`.
    Legacy,
}

#[derive(Debug, Clone)]
pub struct LocalDeployment {
    pub network: String,
    /// Known only for deployments whose metadata lives on the primary shard.
    pub synced: Option<bool>,
}

impl GraphNodeDb {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(url)
            .await
            .context("connecting to the graph-node database")?;
        Ok(GraphNodeDb { pool })
    }

    pub async fn layout(&self) -> Result<Layout> {
        let split: bool =
            sqlx::query_scalar("SELECT to_regclass('subgraphs.deployment') IS NOT NULL")
                .fetch_one(&self.pool)
                .await?;
        Ok(if split { Layout::Split } else { Layout::Legacy })
    }

    /// Chains graph-node is configured for (`public.chains`).
    pub async fn chains(&self) -> Result<HashSet<String>> {
        let v: Vec<String> = sqlx::query_scalar("SELECT name FROM public.chains")
            .fetch_all(&self.pool)
            .await
            .context("reading public.chains")?;
        Ok(v.into_iter().collect())
    }

    pub async fn deployments(&self) -> Result<HashMap<DeploymentId, LocalDeployment>> {
        let synced_sql = match self.layout().await? {
            Layout::Split => "SELECT subgraph, synced_at IS NOT NULL FROM subgraphs.deployment",
            Layout::Legacy => {
                "SELECT deployment, synced_at IS NOT NULL FROM subgraphs.subgraph_deployment"
            }
        };
        let synced: HashMap<String, bool> = sqlx::query_as::<_, (String, bool)>(synced_sql)
            .fetch_all(&self.pool)
            .await
            .context("reading deployment sync state")?
            .into_iter()
            .collect();
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT subgraph, network FROM public.deployment_schemas WHERE active")
                .fetch_all(&self.pool)
                .await
                .context("reading public.deployment_schemas")?;
        Ok(rows
            .into_iter()
            .filter_map(|(h, network)| {
                let synced = synced.get(&h).copied();
                Some((h.parse().ok()?, LocalDeployment { network, synced }))
            })
            .collect())
    }
}
