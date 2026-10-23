//! The Graph Network subgraph: allocation → deployment, and the deployments worth adding.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use sentinel_types::{Address, DeploymentId};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

#[derive(Clone)]
pub struct NetworkSubgraph {
    http: reqwest::Client,
    url: String,
    bearer: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DeploymentInfo {
    pub deployment: DeploymentId,
    pub network: Option<String>,
    pub signal_grt: f64,
    pub query_fees_grt: f64,
    pub active_allocations: u32,
    pub rewards_denied: bool,
}

fn grt(wei: &str) -> f64 {
    wei.parse::<f64>().unwrap_or(0.0) / 1e18
}

#[derive(Deserialize)]
struct Gql<T> {
    data: Option<T>,
    errors: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDeployment {
    ipfs_hash: String,
    signalled_tokens: String,
    query_fees_amount: String,
    denied_at: i64,
    manifest: Option<RawManifest>,
    indexer_allocations: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct RawManifest {
    network: Option<String>,
}

impl RawDeployment {
    fn into_info(self) -> Option<DeploymentInfo> {
        Some(DeploymentInfo {
            deployment: self.ipfs_hash.parse().ok()?,
            network: self.manifest.and_then(|m| m.network),
            signal_grt: grt(&self.signalled_tokens),
            query_fees_grt: grt(&self.query_fees_amount),
            active_allocations: self.indexer_allocations.len() as u32,
            rewards_denied: self.denied_at != 0,
        })
    }
}

impl NetworkSubgraph {
    pub fn new(url: String, bearer: Option<String>) -> Self {
        NetworkSubgraph {
            http: reqwest::Client::new(),
            url,
            bearer,
        }
    }

    async fn query<T: DeserializeOwned>(&self, query: &str, vars: serde_json::Value) -> Result<T> {
        let mut req = self
            .http
            .post(&self.url)
            .json(&json!({ "query": query, "variables": vars }));
        if let Some(b) = &self.bearer {
            req = req.bearer_auth(b);
        }
        let resp: Gql<T> = req
            .send()
            .await
            .context("network subgraph request")?
            .error_for_status()?
            .json()
            .await
            .context("network subgraph response")?;
        if let Some(e) = resp.errors.filter(|e| !e.is_empty()) {
            bail!("network subgraph errors: {}", serde_json::Value::from(e));
        }
        resp.data.context("network subgraph returned no data")
    }

    pub async fn block_number(&self) -> Result<u64> {
        #[derive(Deserialize)]
        struct D {
            _meta: Meta,
        }
        #[derive(Deserialize)]
        struct Meta {
            block: Block,
        }
        #[derive(Deserialize)]
        struct Block {
            number: u64,
        }
        let d: D = self
            .query("{ _meta { block { number } } }", json!({}))
            .await?;
        Ok(d._meta.block.number)
    }

    /// Every allocation the indexer has ever opened, mapped to its deployment.
    pub async fn allocations(&self, indexer: &Address) -> Result<HashMap<Address, DeploymentId>> {
        #[derive(Deserialize)]
        struct D {
            allocations: Vec<A>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct A {
            id: String,
            subgraph_deployment: S,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct S {
            ipfs_hash: String,
        }
        const Q: &str = "query($indexer: String!, $after: String!) {
            allocations(first: 1000, orderBy: id, where: { indexer: $indexer, id_gt: $after }) {
                id subgraphDeployment { ipfsHash }
            }
        }";
        let mut out = HashMap::new();
        let mut after = String::new();
        loop {
            let d: D = self
                .query(Q, json!({ "indexer": indexer.to_string(), "after": after }))
                .await?;
            let n = d.allocations.len();
            for a in d.allocations {
                after = a.id.clone();
                if let (Ok(id), Ok(dep)) = (a.id.parse(), a.subgraph_deployment.ipfs_hash.parse()) {
                    out.insert(id, dep);
                }
            }
            if n < 1000 {
                return Ok(out);
            }
        }
    }

    /// Deployments with curation signal, most signalled first, for the candidate ranking.
    pub async fn signalled_deployments(&self, limit: usize) -> Result<Vec<DeploymentInfo>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct D {
            subgraph_deployments: Vec<RawDeployment>,
        }
        const Q: &str = "query($skip: Int!) {
            subgraphDeployments(first: 500, skip: $skip, orderBy: signalledTokens,
                                orderDirection: desc, where: { signalledTokens_gt: \"0\" }) {
                ipfsHash signalledTokens queryFeesAmount deniedAt manifest { network }
                indexerAllocations(first: 100, where: { status: Active }) { id }
            }
        }";
        let mut out = Vec::new();
        while out.len() < limit {
            let d: D = self.query(Q, json!({ "skip": out.len() })).await?;
            let n = d.subgraph_deployments.len();
            out.extend(
                d.subgraph_deployments
                    .into_iter()
                    .filter_map(RawDeployment::into_info),
            );
            if n < 500 {
                break;
            }
        }
        out.truncate(limit);
        Ok(out)
    }
}
