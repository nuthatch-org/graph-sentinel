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
}
