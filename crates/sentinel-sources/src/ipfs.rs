//! Manifests by deployment hash, over the IPFS HTTP API (`/api/v0/cat`).

use anyhow::{Context, Result, bail};

#[derive(Clone)]
pub struct Ipfs {
    http: reqwest::Client,
    url: String,
}

/// A manifest larger than this is not a manifest.
const MAX_BYTES: usize = 4 << 20;

impl Ipfs {
    pub fn new(url: String) -> Self {
        Ipfs {
            http: reqwest::Client::new(),
            url: url.trim_end_matches('/').to_string(),
        }
    }

    pub async fn cat(&self, hash: &str) -> Result<String> {
        let r = self
            .http
            .post(format!("{}/api/v0/cat", self.url))
            .query(&[("arg", hash)])
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.without_url())
            .context("ipfs cat")?;
        let b = r.bytes().await?;
        if b.len() > MAX_BYTES {
            bail!("{hash}: {} bytes is too large for a manifest", b.len());
        }
        String::from_utf8(b.to_vec()).context("manifest is not UTF-8")
    }
}
