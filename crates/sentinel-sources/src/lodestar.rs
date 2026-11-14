//! Lodestar's public endpoints: GRT price and per-chain lag across indexers.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct Lodestar {
    http: reqwest::Client,
    url: String,
}

// camelCase in from Lodestar, snake_case out to the page like every other snapshot field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase"))]
pub struct ChainLag {
    pub median_blocks_behind: f64,
    pub sampled_indexers: u32,
    pub lagging_count: u32,
    pub liveness: String,
    pub observed_head: Option<u64>,
    pub head_stalled_for_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Price {
    pub usd: f64,
    pub change_24h: f64,
}

impl Lodestar {
    pub fn new(url: String) -> Self {
        Lodestar {
            http: reqwest::Client::new(),
            url: url.trim_end_matches('/').to_string(),
        }
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        self.http
            .get(format!("{}{path}", self.url))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .with_context(|| format!("lodestar {path}"))?
            .error_for_status()?
            .json()
            .await
            .with_context(|| format!("lodestar {path} response"))
    }

    pub async fn price(&self) -> Result<Price> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct R {
            price: f64,
            #[serde(rename = "change24h")]
            change: Option<f64>,
        }
        let r: R = self.get("/api/price").await?;
        Ok(Price {
            usd: r.price,
            change_24h: r.change.unwrap_or(0.0),
        })
    }

    pub async fn chain_lag(&self) -> Result<HashMap<String, ChainLag>> {
        #[derive(Deserialize)]
        struct R {
            data: D,
        }
        #[derive(Deserialize)]
        struct D {
            chains: HashMap<String, ChainLag>,
        }
        Ok(self.get::<R>("/api/chain-lag").await?.data.chains)
    }
}
