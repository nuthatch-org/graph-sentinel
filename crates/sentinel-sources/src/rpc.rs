//! Minimal JSON-RPC: enough for chain head and the oracle's view calls.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use sentinel_types::Address;
use serde::Deserialize;
use serde_json::json;

#[derive(Clone)]
pub struct Rpc {
    http: reqwest::Client,
    url: String,
}

#[derive(Deserialize)]
struct Resp {
    result: Option<String>,
    error: Option<serde_json::Value>,
}

impl Rpc {
    pub fn new(url: String) -> Self {
        Rpc {
            http: reqwest::Client::new(),
            url,
        }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> Result<String> {
        let r: Resp = self
            .http
            .post(&self.url)
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await
            .with_context(|| format!("{method} request"))?
            .error_for_status()?
            .json()
            .await?;
        if let Some(e) = r.error {
            bail!("{method}: {e}");
        }
        r.result.with_context(|| format!("{method}: empty result"))
    }

    pub async fn block_number(&self) -> Result<u64> {
        let h = self.call("eth_blockNumber", json!([])).await?;
        Ok(u64::from_str_radix(h.trim_start_matches("0x"), 16)?)
    }

    pub async fn chain_id(&self) -> Result<u64> {
        let h = self.call("eth_chainId", json!([])).await?;
        Ok(u64::from_str_radix(h.trim_start_matches("0x"), 16)?)
    }

    /// Count logs from `addresses` with any of `topics` as topic0 in `[from, to]`. Halves the
    /// range on a provider's range or size complaint; returns the count and the range covered.
    pub async fn count_logs(
        &self,
        from: u64,
        to: u64,
        addresses: &[String],
        topics: &[String],
    ) -> Result<(u64, u64, u64)> {
        let mut to = to.max(from);
        loop {
            let params = json!([{
                "fromBlock": format!("0x{from:x}"),
                "toBlock": format!("0x{to:x}"),
                "address": addresses,
                "topics": [topics],
            }]);
            match self.call_value("eth_getLogs", params).await {
                Ok(v) => return Ok((v.as_array().map_or(0, |a| a.len() as u64), from, to)),
                Err(e) if to > from && is_range_error(&e.to_string()) => {
                    to = from + (to - from) / 2
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn call_value(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        #[derive(Deserialize)]
        struct R {
            result: Option<serde_json::Value>,
            error: Option<serde_json::Value>,
        }
        let r: R = self
            .http
            .post(&self.url)
            .timeout(std::time::Duration::from_secs(20))
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await
            .with_context(|| format!("{method} request"))?
            .error_for_status()?
            .json()
            .await?;
        if let Some(e) = r.error {
            bail!("{method}: {e}");
        }
        r.result.with_context(|| format!("{method}: empty result"))
    }

    /// `eth_call` returning a single 32-byte word as u128 (enough for timestamps and bools).
    async fn word(&self, to: &Address, data: String) -> Result<u128> {
        let r = self
            .call(
                "eth_call",
                json!([{ "to": to.to_string(), "data": data }, "latest"]),
            )
            .await?;
        decode_word(&r)
    }
}

fn is_range_error(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    [
        "range",
        "too many",
        "limit",
        "exceed",
        "10000",
        "query returned more",
        "block range",
        "response size",
    ]
    .iter()
    .any(|k| e.contains(k))
}

fn decode_word(hex: &str) -> Result<u128> {
    let h = hex.trim_start_matches("0x");
    if h.len() != 64 {
        bail!(
            "expected one ABI word, got {} hex chars (is this the right contract?)",
            h.len()
        );
    }
    if !h[..32].bytes().all(|b| b == b'0') {
        bail!("value exceeds u128");
    }
    Ok(u128::from_str_radix(&h[32..], 16)?)
}

fn with_address(selector: &str, a: &Address) -> String {
    format!("0x{selector}{:0>64}", a.hex())
}

/// Read-only view of the RewardsEligibilityOracle (IRewardsEligibilityStatus).
pub struct Reo<'a> {
    pub rpc: &'a Rpc,
    pub address: Address,
}

#[derive(Debug, Clone)]
pub struct ReoStatus {
    pub eligible: bool,
    pub renewal_time: DateTime<Utc>,
    pub eligibility_period_secs: u64,
    pub last_oracle_update: DateTime<Utc>,
}

fn ts(secs: u128) -> DateTime<Utc> {
    DateTime::from_timestamp(secs as i64, 0).unwrap_or(DateTime::UNIX_EPOCH)
}

impl Reo<'_> {
    pub async fn status(&self, indexer: &Address) -> Result<ReoStatus> {
        // Selectors: cast sig "isEligible(address)" etc.
        let eligible = self
            .rpc
            .word(&self.address, with_address("66e305fd", indexer))
            .await?;
        let renewal = self
            .rpc
            .word(&self.address, with_address("d353402d", indexer))
            .await?;
        let period = self.rpc.word(&self.address, "0xd0a5379e".into()).await?;
        let last = self.rpc.word(&self.address, "0xbe626dd2".into()).await?;
        Ok(ReoStatus {
            eligible: eligible != 0,
            renewal_time: ts(renewal),
            eligibility_period_secs: period as u64,
            last_oracle_update: ts(last),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_address_argument() {
        let a: Address = "0xb43B2CCCceadA5292732a8C58ae134AdEFcE09Bb"
            .parse()
            .unwrap();
        assert_eq!(
            with_address("66e305fd", &a),
            "0x66e305fd000000000000000000000000b43b2cccceada5292732a8c58ae134adefce09bb"
        );
    }

    #[test]
    fn decodes_words() {
        let w = format!("0x{:064x}", 1_209_600u64);
        assert_eq!(decode_word(&w).unwrap(), 1_209_600);
        assert!(decode_word("0x").is_err());
    }
}
