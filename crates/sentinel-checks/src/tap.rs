//! Module 4: the payment-path canary. Every 2026 payment incident had a signature visible
//! minutes before it cost anything; these rules look for those signatures.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use sentinel_types::{Finding, Severity};
use serde::Serialize;
use serde_json::json;

pub const CHECK: &str = "tap";

/// GraphTallyCollector on Arbitrum One. `receipts_verifier_address_v2` must be this there.
pub const ARBITRUM_COLLECTOR: &str = "0x8f69f5c07477ac46fbc491b1e6d91e2bb0111a9e";

#[derive(Debug, Clone, Serialize)]
pub struct Probe {
    pub sender: String,
    pub endpoint: String,
    /// The endpoint answered as a gRPC server.
    pub ok: bool,
    pub detail: String,
    /// Consecutive failed probes, this one included.
    pub failures: u32,
}

/// Receipts not yet covered by a RAV, per payer. tap-agent deletes receipts once a RAV covers
/// them, so what is still in the table is exactly what is unaggregated.
#[derive(Debug, Clone, Serialize)]
pub struct Unaggregated {
    pub payer: String,
    pub receipts: u64,
    pub grt: f64,
    pub oldest_ns: i64,
    pub newest_ns: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Lint {
    pub chain_id: Option<u64>,
    pub collector: Option<String>,
    pub max_willing_to_lose_grt: Option<f64>,
    pub senders: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Reading {
    pub probes: Vec<Probe>,
    /// Payers with receipts in the last 24 hours.
    pub recent_payers: BTreeSet<String>,
    pub denylist: BTreeSet<String>,
    pub unaggregated: Vec<Unaggregated>,
    /// Available escrow per payer, GRT, where read.
    pub escrow: BTreeMap<String, f64>,
    /// Fees received per payer over the last 24 hours, GRT, where known.
    pub fees_24h: BTreeMap<String, f64>,
    pub lint: Lint,
}

#[derive(Debug, Clone)]
pub struct Rules {
    pub aggregator_failures: u32,
    pub fees_warn_ratio: f64,
    pub fees_critical_ratio: f64,
    /// Oldest unaggregated receipt older than this, while receipts flow, is a stalled RAV.
    pub rav_stall_secs: i64,
    /// Receipts newer than this count as "still flowing".
    pub flowing_secs: i64,
    /// Escrow under this many days of the trailing fees warns.
    pub escrow_days: f64,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            aggregator_failures: 2,
            fees_warn_ratio: 0.6,
            fees_critical_ratio: 0.85,
            rav_stall_secs: 20 * 60,
            flowing_secs: 10 * 60,
            escrow_days: 2.0,
        }
    }
}

fn short(a: &str) -> String {
    let a = a.trim_start_matches("0x");
    if a.len() > 10 {
        format!("0x{}…{}", &a[..4], &a[a.len() - 4..])
    } else {
        format!("0x{a}")
    }
}

/// Lower-case, `0x`-prefixed, so addresses from SQL, config and RPC compare equal.
pub fn norm(a: &str) -> String {
    format!(
        "0x{}",
        a.trim().trim_start_matches("0x").to_ascii_lowercase()
    )
}

/// A config value that is plainly a placeholder: zero, or one nibble repeated.
pub fn placeholder(addr: &str) -> bool {
    let h = norm(addr);
    let h = &h[2..];
    h.len() != 40 || h.chars().all(|c| c == h.chars().next().unwrap_or('0'))
}

pub fn findings(r: &Reading, rules: &Rules, now: DateTime<Utc>) -> Vec<Finding> {
    let mut out = Vec::new();
    let now_ns = now.timestamp_nanos_opt().unwrap_or(0);

    for p in r
        .probes
        .iter()
        .filter(|p| !p.ok && p.failures >= rules.aggregator_failures)
    {
        out.push(Finding::new(
            CHECK,
            "aggregator_down",
            norm(&p.sender),
            Severity::Critical,
            format!(
                "The TAP aggregator for sender {} is not answering as a gRPC server ({}). \
                 tap-agent cannot request RAVs, unaggregated fees will climb, and at the limit \
                 your own tap-agent denylists the sender.",
                short(&p.sender),
                p.detail
            ),
            json!(p),
            now,
        ));
    }

    // Payers sending receipts that the config has no aggregator for are never paid.
    if !r.lint.senders.is_empty() {
        for payer in r
            .recent_payers
            .iter()
            .filter(|p| !r.lint.senders.contains(*p))
        {
            out.push(Finding::new(
                CHECK,
                "unknown_sender",
                payer.clone(),
                Severity::Critical,
                format!(
                    "Receipts from {} arrived in the last 24 hours, but [tap.sender_aggregator_endpoints] \
                     has no entry for it, so those queries will never be paid. Add the sender and restart tap-agent.",
                    short(payer)
                ),
                json!({ "payer": payer }),
                now,
            ));
        }
    }

    for s in &r.denylist {
        out.push(Finding::new(
            CHECK,
            "sender_denylisted",
            s.clone(),
            Severity::Critical,
            format!(
                "tap-agent has denylisted sender {}. The gateway sees errors from you and stops routing \
                 queries; check the aggregator, then clear the row once RAVs flow again.",
                short(s)
            ),
            json!({ "sender": s }),
            now,
        ));
    }

    if let Some(limit) = r.lint.max_willing_to_lose_grt.filter(|l| *l > 0.0) {
        for u in &r.unaggregated {
            let ratio = u.grt / limit;
            if ratio < rules.fees_warn_ratio {
                continue;
            }
            let sev = if ratio >= rules.fees_critical_ratio {
                Severity::Critical
            } else {
                Severity::Warn
            };
            out.push(Finding::new(
                CHECK,
                "fees_near_limit",
                u.payer.clone(),
                sev,
                format!(
                    "{:.4} GRT unaggregated from {} is {:.0}% of max_amount_willing_to_lose_grt ({limit}). \
                     At 100% tap-agent denylists the sender.",
                    u.grt,
                    short(&u.payer),
                    ratio * 100.0
                ),
                json!({ "payer": u.payer, "unaggregated_grt": u.grt, "limit_grt": limit, "ratio": ratio }),
                now,
            ));
        }
    }

    for u in &r.unaggregated {
        let age = (now_ns - u.oldest_ns) / 1_000_000_000;
        let flowing = (now_ns - u.newest_ns) / 1_000_000_000 <= rules.flowing_secs;
        if flowing && age > rules.rav_stall_secs {
            out.push(Finding::new(
                CHECK,
                "rav_stalled",
                u.payer.clone(),
                Severity::Critical,
                format!(
                    "Receipts from {} keep arriving but the oldest unaggregated one is {} min old: \
                     tap-agent has stopped requesting RAVs for this sender. Restarting tap-agent usually clears it.",
                    short(&u.payer),
                    age / 60
                ),
                json!({ "payer": u.payer, "oldest_age_secs": age, "receipts": u.receipts }),
                now,
            ));
        }
    }

    for (payer, bal) in &r.escrow {
        let Some(daily) = r.fees_24h.get(payer).copied().filter(|d| *d > 0.0) else {
            continue;
        };
        if *bal < daily * rules.escrow_days {
            out.push(Finding::new(
                CHECK,
                "escrow_low",
                payer.clone(),
                Severity::Warn,
                format!(
                    "{} has {:.2} GRT in escrow for you, under {} days of the {:.2} GRT it paid in the last day. \
                     The gateway routes less to indexers whose escrow is running out.",
                    short(payer),
                    bal,
                    rules.escrow_days,
                    daily
                ),
                json!({ "payer": payer, "escrow_grt": bal, "fees_24h_grt": daily }),
                now,
            ));
        }
    }

    let l = &r.lint;
    let mut lint = Vec::new();
    if let Some(c) = &l.collector {
        if placeholder(c) {
            lint.push(format!(
                "receipts_verifier_address_v2 is a placeholder ({})",
                short(c)
            ));
        } else if l.chain_id == Some(42161) && norm(c) != ARBITRUM_COLLECTOR {
            lint.push(format!(
                "receipts_verifier_address_v2 is {} but the GraphTallyCollector on Arbitrum One is {}",
                short(c),
                short(ARBITRUM_COLLECTOR)
            ));
        }
    }
    if let Some(m) = l.max_willing_to_lose_grt
        && m < 1.0
    {
        lint.push(format!(
            "max_amount_willing_to_lose_grt is {m}; under 1 GRT, ordinary aggregation lag denylists senders"
        ));
    }
    if l.senders.is_empty() {
        lint.push("[tap.sender_aggregator_endpoints] is empty: no sender can be paid".into());
    }
    for msg in lint {
        out.push(Finding::new(
            CHECK,
            "config_suspect",
            msg.clone(),
            Severity::Warn,
            format!("indexer-rs config: {msg}."),
            json!({}),
            now,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GATEWAY: &str = "0x6aa2ace7ec77c06c1dab1b0afac9d56d9c9d0e50";
    const OTHER: &str = "0xdd6a6f76eb36b873c1c184e8b9b9e762fe216490";
    const S: i64 = 1_000_000_000;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_792_700_000, 0).unwrap()
    }

    fn healthy() -> Reading {
        Reading {
            probes: vec![Probe {
                sender: GATEWAY.into(),
                endpoint: "https://a".into(),
                ok: true,
                detail: "grpc".into(),
                failures: 0,
            }],
            recent_payers: [GATEWAY.to_string()].into(),
            denylist: BTreeSet::new(),
            unaggregated: vec![Unaggregated {
                payer: GATEWAY.into(),
                receipts: 40,
                grt: 0.5,
                oldest_ns: now().timestamp() * S - 120 * S,
                newest_ns: now().timestamp() * S - 5 * S,
            }],
            escrow: [(GATEWAY.to_string(), 500.0)].into(),
            fees_24h: [(GATEWAY.to_string(), 3.0)].into(),
            lint: Lint {
                chain_id: Some(42161),
                collector: Some(ARBITRUM_COLLECTOR.into()),
                max_willing_to_lose_grt: Some(20.0),
                senders: [GATEWAY.to_string()].into(),
            },
        }
    }

    fn kinds(r: &Reading) -> Vec<String> {
        findings(r, &Rules::default(), now())
            .into_iter()
            .map(|f| f.kind)
            .collect()
    }

    #[test]
    fn a_healthy_payment_path_is_quiet() {
        assert!(kinds(&healthy()).is_empty());
    }

    #[test]
    fn the_cloudflare_outage_needs_two_failures() {
        let mut r = healthy();
        r.probes[0].ok = false;
        r.probes[0].failures = 1;
        assert!(kinds(&r).is_empty());
        r.probes[0].failures = 2;
        assert_eq!(kinds(&r), ["aggregator_down"]);
    }

    #[test]
    fn a_rotated_sender_without_an_endpoint_is_caught() {
        let mut r = healthy();
        r.recent_payers.insert(OTHER.into());
        assert_eq!(kinds(&r), ["unknown_sender"]);
    }

    #[test]
    fn denylist_and_fees_near_the_limit() {
        let mut r = healthy();
        r.denylist.insert(GATEWAY.into());
        r.unaggregated[0].grt = 13.0; // 65% of 20
        assert_eq!(kinds(&r), ["sender_denylisted", "fees_near_limit"]);
        r.unaggregated[0].grt = 18.0; // 90%
        let f = findings(&r, &Rules::default(), now());
        assert_eq!(f[1].severity, Severity::Critical);
    }

    #[test]
    fn a_stalled_rav_needs_receipts_still_flowing() {
        let mut r = healthy();
        r.unaggregated[0].oldest_ns = now().timestamp() * S - 3600 * S;
        assert_eq!(kinds(&r), ["rav_stalled"]);
        // Old receipts but nothing recent: traffic stopped, not a stall.
        r.unaggregated[0].newest_ns = now().timestamp() * S - 3000 * S;
        assert!(kinds(&r).is_empty());
    }

    #[test]
    fn low_escrow_against_trailing_fees() {
        let mut r = healthy();
        r.escrow.insert(GATEWAY.into(), 5.0); // under 2 days of 3 GRT/day
        assert_eq!(kinds(&r), ["escrow_low"]);
    }

    #[test]
    fn config_lint_catches_the_2_0_2_placeholder() {
        let mut r = healthy();
        r.lint.collector = Some("0x3333333333333333333333333333333333333333".into());
        r.lint.max_willing_to_lose_grt = Some(0.1);
        let f = findings(&r, &Rules::default(), now());
        let lint: Vec<_> = f.iter().filter(|f| f.kind == "config_suspect").collect();
        assert_eq!(lint.len(), 2);
        assert!(lint[0].summary.contains("placeholder"));
        // 0.5 GRT unaggregated against a 0.1 GRT limit is already past it.
        assert!(
            f.iter()
                .any(|f| f.kind == "fees_near_limit" && f.severity == Severity::Critical)
        );
    }

    #[test]
    fn wrong_collector_on_arbitrum() {
        let mut r = healthy();
        r.lint.collector = Some("0x1234567890123456789012345678901234567890".into());
        assert_eq!(kinds(&r), ["config_suspect"]);
    }
}
