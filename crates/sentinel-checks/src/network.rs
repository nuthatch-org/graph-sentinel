//! What the network says about this indexer (and this fisherman): stale allocations,
//! disputes, thawing. Pure rules over network-subgraph readings.

use chrono::{DateTime, Utc};
use sentinel_types::{Finding, Severity};
use serde::Serialize;
use serde_json::json;

pub const CHECK: &str = "network";

/// `SubgraphService.maxPOIStaleness()` on Arbitrum One, read 2026-10-23: 28 days. A POI
/// presented later than this after the previous one earns nothing for the gap.
pub const MAX_POI_STALENESS_SECS: i64 = 2_419_200;
/// Warn this long before the deadline.
pub const POI_WARN_SECS: i64 = 7 * 86_400;

#[derive(Debug, Clone, Serialize)]
pub struct AllocationView {
    pub id: String,
    pub deployment: String,
    pub network: Option<String>,
    pub allocated_grt: f64,
    pub rewards_grt: f64,
    pub created_at: i64,
    pub latest_poi_at: Option<i64>,
    /// Seconds until the POI is stale; negative once it is.
    pub poi_deadline_in: i64,
}

/// The clock starts at the last POI, or at creation for an allocation that has never presented.
pub fn poi_deadline_in(created_at: i64, latest_poi_at: Option<i64>, now: i64) -> i64 {
    latest_poi_at.unwrap_or(created_at) + MAX_POI_STALENESS_SECS - now
}

fn short(s: &str) -> String {
    if s.len() > 14 {
        format!("{}…{}", &s[..8], &s[s.len() - 4..])
    } else {
        s.into()
    }
}

fn days(secs: i64) -> String {
    let d = secs as f64 / 86_400.0;
    if d.abs() < 1.0 {
        format!("{} h", (secs / 3600).abs())
    } else {
        format!("{:.0} days", d.abs())
    }
}

pub fn allocation_findings(allocs: &[AllocationView], now: DateTime<Utc>) -> Vec<Finding> {
    let mut out = Vec::new();
    // Zero-token allocations earn nothing either way; leave them out.
    let live: Vec<&AllocationView> = allocs.iter().filter(|a| a.allocated_grt > 0.0).collect();
    let stale: Vec<&&AllocationView> = live.iter().filter(|a| a.poi_deadline_in <= 0).collect();
    let soon: Vec<&&AllocationView> = live
        .iter()
        .filter(|a| a.poi_deadline_in > 0 && a.poi_deadline_in <= POI_WARN_SECS)
        .collect();
    if !stale.is_empty() {
        let grt: f64 = stale.iter().map(|a| a.allocated_grt).sum();
        out.push(Finding::new(
            CHECK,
            "poi_stale",
            "allocations",
            Severity::Critical,
            format!(
                "{} allocation{} ({:.0} GRT) went past the 28-day POI limit, so rewards for the gap are lost. \
                 Present a POI or close {}.",
                stale.len(),
                if stale.len() == 1 { "" } else { "s" },
                grt,
                if stale.len() == 1 { "it" } else { "them" }
            ),
            json!({
                "allocations": stale.iter().map(|a| &a.id).collect::<Vec<_>>(),
                "deployments": stale.iter().map(|a| &a.deployment).collect::<Vec<_>>(),
            }),
            now,
        ));
    }
    if !soon.is_empty() {
        let first = soon
            .iter()
            .min_by_key(|a| a.poi_deadline_in)
            .expect("non-empty");
        out.push(Finding::new(
            CHECK,
            "poi_due",
            "allocations",
            Severity::Warn,
            format!(
                "{} allocation{} must present a POI within 7 days; the first ({} on {}) in {}.",
                soon.len(),
                if soon.len() == 1 { "" } else { "s" },
                short(&first.deployment),
                first.network.as_deref().unwrap_or("?"),
                days(first.poi_deadline_in)
            ),
            json!({
                "allocations": soon.iter().map(|a| &a.id).collect::<Vec<_>>(),
                "deployments": soon.iter().map(|a| &a.deployment).collect::<Vec<_>>(),
            }),
            now,
        ));
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct DisputeView {
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

impl DisputeView {
    pub fn open(&self) -> bool {
        self.status == "Undecided"
    }
}

/// Disputes filed against this indexer: every open one is money at risk.
pub fn against_findings(disputes: &[DisputeView], now: DateTime<Utc>) -> Vec<Finding> {
    disputes
        .iter()
        .filter(|d| d.open())
        .map(|d| {
            Finding::new(
                CHECK,
                "dispute_against",
                d.id.clone(),
                Severity::Critical,
                format!(
                    "{} dispute {} filed against this indexer by {} with a {:.0} GRT bond{}. \
                     The arbitrator rules; answer in the forum thread with your evidence.",
                    d.kind,
                    short(&d.id),
                    short(&d.fisherman),
                    d.deposit_grt,
                    d.deployment
                        .as_ref()
                        .map(|x| format!(" on {}", short(x)))
                        .unwrap_or_default()
                ),
                json!(d),
                now,
            )
        })
        .collect()
}

/// Disputes this fisherman filed: say when one can be cancelled and when one is ruled.
pub fn filed_findings(disputes: &[DisputeView], now: DateTime<Utc>) -> Vec<Finding> {
    let t = now.timestamp();
    let mut out = Vec::new();
    for d in disputes {
        if d.open() && d.cancellable_at.is_some_and(|c| c <= t) {
            out.push(Finding::new(
                CHECK,
                "dispute_cancellable",
                d.id.clone(),
                Severity::Warn,
                format!(
                    "Your {} dispute {} against {} is past its dispute period and still undecided. \
                     Cancelling it returns the {:.0} GRT bond.",
                    d.kind.to_lowercase(),
                    short(&d.id),
                    short(&d.indexer),
                    d.deposit_grt
                ),
                json!(d),
                now,
            ));
        }
        // A ruling in the last week is news; older ones are history.
        if !d.open() && d.closed_at.is_some_and(|c| t - c < 7 * 86_400) {
            out.push(Finding::new(
                CHECK,
                "dispute_ruled",
                d.id.clone(),
                if d.status == "Accepted" {
                    Severity::Info
                } else {
                    Severity::Warn
                },
                format!(
                    "Your dispute {} against {} was {}{}.",
                    short(&d.id),
                    short(&d.indexer),
                    d.status.to_lowercase(),
                    if d.slashed_grt > 0.0 {
                        format!(": {:.0} GRT slashed", d.slashed_grt)
                    } else {
                        String::new()
                    }
                ),
                json!(d),
                now,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_792_700_000;

    fn alloc(grt: f64, created: i64, poi: Option<i64>) -> AllocationView {
        AllocationView {
            id: format!("0x{:040x}", created),
            deployment: "QmZA6BHHHiFGZAawSw4UVPjEBFT87Z3wu3nc7K7rgEZpeV".into(),
            network: Some("chapel".into()),
            allocated_grt: grt,
            rewards_grt: 0.0,
            created_at: created,
            latest_poi_at: poi,
            poi_deadline_in: poi_deadline_in(created, poi, NOW),
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(NOW, 0).unwrap()
    }

    #[test]
    fn poi_clock_runs_from_last_poi_or_creation() {
        assert_eq!(
            poi_deadline_in(NOW - 86_400, None, NOW),
            MAX_POI_STALENESS_SECS - 86_400
        );
        assert_eq!(poi_deadline_in(0, Some(NOW), NOW), MAX_POI_STALENESS_SECS);
    }

    #[test]
    fn stale_and_due_are_separate_findings_and_zero_token_allocations_are_ignored() {
        let day = 86_400;
        let f = allocation_findings(
            &[
                alloc(1_000.0, NOW - 40 * day, Some(NOW - 30 * day)), // stale
                alloc(500.0, NOW - 25 * day, None),                   // due in 3 days
                alloc(0.0, NOW - 90 * day, None),                     // zero tokens
                alloc(800.0, NOW - 5 * day, None),                    // fine
            ],
            now(),
        );
        let kinds: Vec<_> = f.iter().map(|f| (f.kind.as_str(), f.severity)).collect();
        assert_eq!(
            kinds,
            [
                ("poi_stale", Severity::Critical),
                ("poi_due", Severity::Warn)
            ]
        );
        assert!(f[0].summary.starts_with("1 allocation (1000 GRT)"));
        assert!(f[1].summary.ends_with("in 3 days."));
    }

    fn dispute(status: &str, cancellable: Option<i64>, closed: Option<i64>) -> DisputeView {
        DisputeView {
            id: "0x9347e202063c5cc554f9245795d580a01dbaea111ce4f717271e81d051ad7b41".into(),
            kind: "Indexing".into(),
            status: status.into(),
            indexer: "0x0a015d9e17d3978164486473a2ddbb654418db07".into(),
            fisherman: "0x083a086ca2ccec6526cbe97b2841a481f99834af".into(),
            deployment: None,
            created_at: NOW - 10 * 86_400,
            closed_at: closed,
            cancellable_at: cancellable,
            deposit_grt: 10_000.0,
            slashed_grt: 0.0,
        }
    }

    #[test]
    fn open_disputes_against_are_critical() {
        let f = against_findings(
            &[
                dispute("Undecided", None, None),
                dispute("Rejected", None, Some(NOW)),
            ],
            now(),
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Critical);
    }

    #[test]
    fn a_filed_dispute_becomes_cancellable_and_then_ruled() {
        assert!(filed_findings(&[dispute("Undecided", Some(NOW + 100), None)], now()).is_empty());
        let f = filed_findings(&[dispute("Undecided", Some(NOW - 1), None)], now());
        assert_eq!(f[0].kind, "dispute_cancellable");
        assert!(f[0].summary.contains("returns the 10000 GRT bond"));
        let f = filed_findings(&[dispute("Accepted", None, Some(NOW - 3600))], now());
        assert_eq!(
            (f[0].kind.as_str(), f[0].severity),
            ("dispute_ruled", Severity::Info)
        );
        assert!(
            filed_findings(&[dispute("Drawn", None, Some(NOW - 30 * 86_400))], now()).is_empty()
        );
    }
}
