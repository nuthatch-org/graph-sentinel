//! graph-node's index-node status API.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;

#[derive(Clone)]
pub struct IndexNode {
    http: reqwest::Client,
    url: String,
}

impl IndexNode {
    pub fn new(url: String) -> Self {
        IndexNode {
            http: reqwest::Client::new(),
            url,
        }
    }

    /// Number of deployments the index-node reports on; enough for `doctor`.
    pub async fn deployment_count(&self) -> Result<usize> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct R {
            data: Option<D>,
            errors: Option<serde_json::Value>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct D {
            indexing_statuses: Vec<serde_json::Value>,
        }
        let r: R = self
            .http
            .post(&self.url)
            .json(&json!({ "query": "{ indexingStatuses { subgraph } }" }))
            .send()
            .await
            .context("index-node request")?
            .error_for_status()?
            .json()
            .await?;
        if let Some(e) = r.errors {
            bail!("index-node errors: {e}");
        }
        Ok(r.data
            .context("index-node returned no data")?
            .indexing_statuses
            .len())
    }

    /// Every deployment the index-node knows, flattened to its first chain.
    pub async fn statuses(&self) -> Result<Vec<Status>> {
        #[derive(Deserialize)]
        struct R {
            data: Option<D>,
            errors: Option<serde_json::Value>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct D {
            indexing_statuses: Vec<RawStatus>,
        }
        const Q: &str = "{ indexingStatuses { subgraph synced health paused node
            fatalError { message }
            chains { network latestBlock { number } chainHeadBlock { number } } } }";
        let r: R = self
            .http
            .post(&self.url)
            .json(&json!({ "query": Q }))
            .send()
            .await
            .context("index-node request")?
            .error_for_status()?
            .json()
            .await?;
        if let Some(e) = r.errors {
            bail!("index-node errors: {e}");
        }
        Ok(r.data
            .context("index-node returned no data")?
            .indexing_statuses
            .into_iter()
            .map(RawStatus::flatten)
            .collect())
    }
}

#[derive(Debug, Clone)]
pub struct Status {
    pub deployment: String,
    pub network: String,
    pub health: String,
    pub synced: bool,
    pub paused: bool,
    pub assigned: bool,
    pub latest: Option<u64>,
    pub chain_head: Option<u64>,
    pub fatal_error: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawStatus {
    subgraph: String,
    synced: bool,
    health: String,
    paused: Option<bool>,
    node: Option<String>,
    fatal_error: Option<Message>,
    chains: Vec<RawChain>,
}

#[derive(Deserialize)]
struct Message {
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawChain {
    network: String,
    latest_block: Option<Num>,
    chain_head_block: Option<Num>,
}

#[derive(Deserialize)]
struct Num {
    number: serde_json::Value,
}

impl Num {
    // BigInt arrives as a string; tolerate a number too.
    fn get(&self) -> Option<u64> {
        match &self.number {
            serde_json::Value::String(s) => s.parse().ok(),
            v => v.as_u64(),
        }
    }
}

impl RawStatus {
    fn flatten(self) -> Status {
        let c = self.chains.into_iter().next();
        Status {
            deployment: self.subgraph,
            network: c.as_ref().map(|c| c.network.clone()).unwrap_or_default(),
            health: self.health,
            synced: self.synced,
            paused: self.paused.unwrap_or(false),
            assigned: self.node.is_some(),
            latest: c
                .as_ref()
                .and_then(|c| c.latest_block.as_ref())
                .and_then(Num::get),
            chain_head: c
                .as_ref()
                .and_then(|c| c.chain_head_block.as_ref())
                .and_then(Num::get),
            fatal_error: self.fatal_error.map(|e| e.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_a_v0_41_status() {
        let raw: RawStatus = serde_json::from_value(json!({
            "subgraph": "QmZA6BHHHiFGZAawSw4UVPjEBFT87Z3wu3nc7K7rgEZpeV", "synced": false, "health": "failed",
            "paused": null, "node": "default", "fatalError": { "message": "boom" },
            "chains": [{ "network": "chapel", "latestBlock": { "number": "137159114" }, "chainHeadBlock": { "number": "137160000" } }]
        })).unwrap();
        let s = raw.flatten();
        assert_eq!(
            (s.latest, s.chain_head),
            (Some(137_159_114), Some(137_160_000))
        );
        assert!(s.assigned && !s.paused);
        assert_eq!(s.fatal_error.as_deref(), Some("boom"));
    }
}
