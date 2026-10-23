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

    /// The indexer's open allocations, newest first.
    pub async fn open_allocations(&self, indexer: &Address) -> Result<Vec<OpenAllocation>> {
        #[derive(Deserialize)]
        struct D {
            allocations: Vec<RawAlloc>,
        }
        const Q: &str = "query($indexer: String!, $after: String!) {
            allocations(first: 1000, orderBy: id, where: { indexer: $indexer, status: Active, id_gt: $after }) {
                id allocatedTokens createdAt latestPoiPresentedAt poiCount indexingRewards
                subgraphDeployment { ipfsHash signalledTokens manifest { network } }
            }
        }";
        let mut out = Vec::new();
        let mut after = String::new();
        loop {
            let d: D = self
                .query(Q, json!({ "indexer": indexer.to_string(), "after": after }))
                .await?;
            let n = d.allocations.len();
            for a in d.allocations {
                after = a.id.clone();
                out.push(a.into_open());
            }
            if n < 1000 {
                out.sort_by_key(|a: &OpenAllocation| std::cmp::Reverse(a.created_at));
                return Ok(out);
            }
        }
    }

    /// Disputes naming `who` as indexer or as fisherman, newest first.
    pub async fn disputes(&self, who: &Address, as_fisherman: bool) -> Result<Vec<Dispute>> {
        #[derive(Deserialize)]
        struct D {
            disputes: Vec<RawDispute>,
        }
        let q = if as_fisherman {
            "query($who: String!) { disputes(first: 200, orderBy: createdAt, orderDirection: desc, where: { fisherman: $who }) {
                id type status createdAt closedAt cancellableAt deposit tokensSlashed
                indexer { id } fisherman { id } subgraphDeployment { ipfsHash } } }"
        } else {
            "query($who: String!) { disputes(first: 200, orderBy: createdAt, orderDirection: desc, where: { indexer: $who }) {
                id type status createdAt closedAt cancellableAt deposit tokensSlashed
                indexer { id } fisherman { id } subgraphDeployment { ipfsHash } } }"
        };
        let d: D = self.query(q, json!({ "who": who.to_string() })).await?;
        Ok(d.disputes
            .into_iter()
            .map(RawDispute::into_dispute)
            .collect())
    }

    /// Stake, provision to the SubgraphService and delegation.
    pub async fn indexer_overview(&self, indexer: &Address) -> Result<Option<Overview>> {
        #[derive(Deserialize)]
        struct D {
            indexer: Option<RawIndexer>,
        }
        const Q: &str = "query($id: ID!) { indexer(id: $id) {
            stakedTokens provisionedTokens allocatedTokens thawingTokens delegatedTokens delegatedThawingTokens
            rewardsEarned queryFeesCollected allocationCount
            provisions(first: 5) { dataService { id } tokensProvisioned tokensAllocated tokensThawing thawingUntil }
        } }";
        let d: D = self.query(Q, json!({ "id": indexer.to_string() })).await?;
        Ok(d.indexer.map(RawIndexer::into_overview))
    }
}

fn big(s: &str) -> f64 {
    grt(s)
}

fn secs(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::String(s) => s.parse().ok(),
        v => v.as_i64(),
    }
    .filter(|t| *t > 0)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OpenAllocation {
    pub id: String,
    pub deployment: String,
    pub network: Option<String>,
    pub allocated_grt: f64,
    pub signal_grt: f64,
    pub created_at: i64,
    pub latest_poi_at: Option<i64>,
    pub poi_count: u64,
    pub rewards_grt: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAlloc {
    id: String,
    allocated_tokens: String,
    created_at: serde_json::Value,
    latest_poi_presented_at: serde_json::Value,
    poi_count: serde_json::Value,
    indexing_rewards: String,
    subgraph_deployment: RawAllocDep,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAllocDep {
    ipfs_hash: String,
    signalled_tokens: String,
    manifest: Option<RawManifest>,
}

impl RawAlloc {
    fn into_open(self) -> OpenAllocation {
        OpenAllocation {
            id: self.id,
            deployment: self.subgraph_deployment.ipfs_hash,
            network: self.subgraph_deployment.manifest.and_then(|m| m.network),
            allocated_grt: big(&self.allocated_tokens),
            signal_grt: big(&self.subgraph_deployment.signalled_tokens),
            created_at: secs(&self.created_at).unwrap_or(0),
            latest_poi_at: secs(&self.latest_poi_presented_at),
            poi_count: secs(&self.poi_count).unwrap_or(0) as u64,
            rewards_grt: big(&self.indexing_rewards),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Dispute {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub indexer: String,
    pub fisherman: String,
    pub deployment: Option<String>,
    pub created_at: i64,
    pub closed_at: Option<i64>,
    pub cancellable_at: Option<i64>,
    pub deposit_grt: f64,
    pub slashed_grt: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDispute {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    status: String,
    created_at: serde_json::Value,
    closed_at: serde_json::Value,
    cancellable_at: serde_json::Value,
    deposit: String,
    tokens_slashed: String,
    indexer: IdObj,
    fisherman: IdObj,
    subgraph_deployment: Option<RawDepHash>,
}

#[derive(Deserialize)]
struct IdObj {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDepHash {
    ipfs_hash: String,
}

impl RawDispute {
    fn into_dispute(self) -> Dispute {
        Dispute {
            id: self.id,
            kind: self.kind,
            status: self.status,
            indexer: self.indexer.id,
            fisherman: self.fisherman.id,
            deployment: self.subgraph_deployment.map(|d| d.ipfs_hash),
            created_at: secs(&self.created_at).unwrap_or(0),
            closed_at: secs(&self.closed_at),
            cancellable_at: secs(&self.cancellable_at),
            deposit_grt: big(&self.deposit),
            slashed_grt: big(&self.tokens_slashed),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Overview {
    pub staked_grt: f64,
    pub provisioned_grt: f64,
    pub allocated_grt: f64,
    pub thawing_grt: f64,
    pub delegated_grt: f64,
    pub delegated_thawing_grt: f64,
    pub rewards_earned_grt: f64,
    pub query_fees_grt: f64,
    pub allocation_count: u64,
    pub provision_thawing_until: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIndexer {
    staked_tokens: String,
    provisioned_tokens: String,
    allocated_tokens: String,
    thawing_tokens: String,
    delegated_tokens: String,
    delegated_thawing_tokens: String,
    rewards_earned: String,
    query_fees_collected: String,
    allocation_count: serde_json::Value,
    #[serde(default)]
    provisions: Vec<RawProvision>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawProvision {
    thawing_until: serde_json::Value,
}

impl RawIndexer {
    fn into_overview(self) -> Overview {
        Overview {
            staked_grt: big(&self.staked_tokens),
            provisioned_grt: big(&self.provisioned_tokens),
            allocated_grt: big(&self.allocated_tokens),
            thawing_grt: big(&self.thawing_tokens),
            // The subgraph's delegatedTokens includes what is thawing (kittiwake views note).
            delegated_grt: big(&self.delegated_tokens) - big(&self.delegated_thawing_tokens),
            delegated_thawing_grt: big(&self.delegated_thawing_tokens),
            rewards_earned_grt: big(&self.rewards_earned),
            query_fees_grt: big(&self.query_fees_collected),
            allocation_count: secs(&self.allocation_count).unwrap_or(0) as u64,
            provision_thawing_until: self
                .provisions
                .iter()
                .filter_map(|p| secs(&p.thawing_until))
                .max(),
        }
    }
}
