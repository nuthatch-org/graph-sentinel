//! RPC spend: graph-node's own per-provider, per-method request counters priced at the
//! provider's compute-unit rates, accumulated per UTC day against a budget.

use std::collections::BTreeMap;

use chrono::{DateTime, Timelike, Utc};
use sentinel_types::{Finding, Severity};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const CHECK: &str = "spend";

/// (provider label, method) → requests since graph-node started.
pub type Counters = BTreeMap<(String, String), u64>;

/// Read `endpoint_request{conn_type="rpc",provider,req_type,result}` from graph-node's metrics.
/// Success and failure both count: a provider bills most errors too.
pub fn parse_metrics(text: &str) -> Counters {
    let mut out = Counters::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("endpoint_request{") else {
            continue;
        };
        let Some((labels, value)) = rest.rsplit_once("} ") else {
            continue;
        };
        let label = |k: &str| {
            labels.split(',').find_map(|kv| {
                let (key, v) = kv.split_once('=')?;
                (key.trim() == k).then(|| v.trim().trim_matches('"').to_string())
            })
        };
        if label("conn_type").as_deref() != Some("rpc") {
            continue;
        }
        let (Some(provider), Some(method)) = (label("provider"), label("req_type")) else {
            continue;
        };
        let Ok(v) = value.trim().parse::<f64>() else {
            continue;
        };
        *out.entry((provider, method)).or_default() += v as u64;
    }
    out
}

/// Requests since the previous reading. A counter that went backwards means graph-node
/// restarted, so its whole current value is new.
pub fn delta(prev: &Counters, cur: &Counters) -> Counters {
    cur.iter()
        .map(|(k, &v)| {
            let p = prev.get(k).copied().unwrap_or(0);
            (k.clone(), if v >= p { v - p } else { v })
        })
        .filter(|(_, v)| *v > 0)
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pricing {
    /// Provider labels containing any of these are billed; the rest are free (public RPCs).
    pub billed_providers: Vec<String>,
    pub usd_per_million_cu: f64,
    pub method_cu: BTreeMap<String, u64>,
    /// Methods missing from the table are priced at this, high on purpose.
    pub default_cu: u64,
}

impl Default for Pricing {
    fn default() -> Self {
        // Alchemy, verified against invoices 2026-09: these divide the billed CU exactly.
        let method_cu = [
            ("eth_getBlockByNumber", 20),
            ("eth_getBlockByHash", 20),
            ("eth_getBlockReceipts", 20),
            ("eth_getTransactionReceipt", 20),
            ("eth_getLogs", 60),
            ("eth_blockNumber", 10),
            ("eth_call", 26),
            ("eth_getBalance", 20),
            ("eth_getCode", 20),
            ("eth_chainId", 0),
            ("net_version", 0),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        Pricing {
            billed_providers: vec!["alchemy".into()],
            usd_per_million_cu: 0.45,
            method_cu,
            default_cu: 40,
        }
    }
}

impl Pricing {
    pub fn billed(&self, provider: &str) -> bool {
        let p = provider.to_ascii_lowercase();
        self.billed_providers
            .iter()
            .any(|b| p.contains(&b.to_ascii_lowercase()))
    }

    pub fn cu(&self, method: &str) -> u64 {
        self.method_cu
            .get(method)
            .copied()
            .unwrap_or(self.default_cu)
    }

    /// Billed compute units in a set of request counts.
    pub fn cost_cu(&self, requests: &Counters) -> u64 {
        requests
            .iter()
            .filter(|((p, _), _)| self.billed(p))
            .map(|((_, m), n)| n * self.cu(m))
            .sum()
    }

    pub fn usd(&self, cu: u64) -> f64 {
        cu as f64 / 1e6 * self.usd_per_million_cu
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Budget {
    pub daily_usd: f64,
    pub warn_ratio: f64,
}

/// Today's spend against the budget, as findings. `stopped` says whether the guard acted.
pub fn findings(
    today_usd: f64,
    budget: &Budget,
    projected_usd: f64,
    stopped: bool,
    now: DateTime<Utc>,
) -> Vec<Finding> {
    let ratio = today_usd / budget.daily_usd;
    let ev = json!({ "today_usd": today_usd, "budget_usd": budget.daily_usd, "projected_usd": projected_usd, "stopped": stopped });
    if ratio >= 1.0 {
        let summary = if stopped {
            format!(
                "RPC spend reached ${today_usd:.2} of the ${:.2} daily budget, so graph-node was stopped. It starts again after midnight UTC.",
                budget.daily_usd
            )
        } else {
            format!(
                "RPC spend reached ${today_usd:.2} of the ${:.2} daily budget.",
                budget.daily_usd
            )
        };
        vec![Finding::new(
            CHECK,
            "budget_exceeded",
            "rpc",
            Severity::Critical,
            summary,
            ev,
            now,
        )]
    } else if ratio >= budget.warn_ratio || projected_usd > budget.daily_usd {
        vec![Finding::new(
            CHECK,
            "budget_warning",
            "rpc",
            Severity::Warn,
            format!(
                "RPC spend is ${today_usd:.2} of ${:.2} today and heading for ${projected_usd:.2} at this rate.",
                budget.daily_usd
            ),
            ev,
            now,
        )]
    } else {
        vec![]
    }
}

/// Where today ends at the rate seen over the last reading window.
pub fn projected(today_usd: f64, recent_usd: f64, recent_secs: f64, now: DateTime<Utc>) -> f64 {
    if recent_secs <= 0.0 {
        return today_usd;
    }
    let left = (86_400 - now.time().num_seconds_from_midnight() as i64).max(0) as f64;
    today_usd + recent_usd / recent_secs * left
}

#[cfg(test)]
mod tests {
    use super::*;

    const METRICS: &str = r#"# HELP endpoint_request successfull request
# TYPE endpoint_request counter
endpoint_request{conn_type="rpc",provider="mainnet-alchemy",req_type="eth_getBlockByNumber",result="success"} 1000
endpoint_request{conn_type="rpc",provider="mainnet-alchemy",req_type="eth_getBlockByNumber",result="failure"} 10
endpoint_request{conn_type="rpc",provider="mainnet-alchemy",req_type="eth_getLogs",result="success"} 50
endpoint_request{conn_type="rpc",provider="chapel-public",req_type="eth_getBlockByNumber",result="success"} 99999
endpoint_request{conn_type="firehose",provider="x",req_type="",result="success"} 5
"#;

    #[test]
    fn parses_graph_node_endpoint_counters() {
        let c = parse_metrics(METRICS);
        assert_eq!(
            c[&("mainnet-alchemy".into(), "eth_getBlockByNumber".into())],
            1010
        );
        assert_eq!(c.len(), 3, "firehose is not RPC");
    }

    #[test]
    fn only_billed_providers_cost() {
        let p = Pricing::default();
        let c = parse_metrics(METRICS);
        // 1010 × 20 + 50 × 60; the public provider is free.
        assert_eq!(p.cost_cu(&c), 23_200);
        assert!((p.usd(1_000_000) - 0.45).abs() < 1e-9);
    }

    #[test]
    fn a_restart_counts_from_zero() {
        let k = ("a-alchemy".to_string(), "eth_call".to_string());
        let prev: Counters = [(k.clone(), 500)].into();
        assert_eq!(delta(&prev, &[(k.clone(), 700)].into())[&k], 200);
        assert_eq!(delta(&prev, &[(k.clone(), 30)].into())[&k], 30);
    }

    #[test]
    fn budget_findings() {
        let b = Budget {
            daily_usd: 5.0,
            warn_ratio: 0.7,
        };
        let now = Utc::now();
        assert!(findings(1.0, &b, 2.0, false, now).is_empty());
        assert_eq!(findings(1.0, &b, 6.0, false, now)[0].kind, "budget_warning");
        assert_eq!(findings(3.6, &b, 4.0, false, now)[0].kind, "budget_warning");
        let f = findings(5.01, &b, 9.0, true, now);
        assert_eq!(
            (f[0].kind.as_str(), f[0].severity),
            ("budget_exceeded", Severity::Critical)
        );
        assert!(f[0].summary.contains("graph-node was stopped"));
    }

    #[test]
    fn projection_extends_the_recent_rate_to_midnight() {
        let noon: DateTime<Utc> = "2026-10-23T12:00:00Z".parse().unwrap();
        // $0.10 in the last hour, 12 hours left.
        assert!((projected(1.0, 0.10, 3600.0, noon) - 2.2).abs() < 1e-9);
    }
}
