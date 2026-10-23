//! Module 3: the REO eligibility meter.

pub mod projection;
pub mod ranking;
pub mod rules;

use chrono::{DateTime, NaiveDate, Utc};
use sentinel_types::{Finding, Severity};
use serde::Serialize;
use serde_json::json;

use projection::{Assumption, DailyCounts, Projection, judge, project, typical};
use rules::Rules;

pub const CHECK: &str = "reo";

/// What the chain says, read from the RewardsEligibilityOracle contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OnChain {
    pub eligible: bool,
    /// Zero when the oracle has never renewed this indexer.
    pub renewal_time: DateTime<Utc>,
    pub eligibility_period_secs: u64,
    pub last_oracle_update: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub today: NaiveDate,
    /// First day graph-sentinel has receipts for; the projection treats earlier days as empty.
    pub observed_since: Option<NaiveDate>,
    pub today_count: u32,
    pub typical_count: u32,
    pub subgraphs_per_day_now: u32,
    pub min_online_days: u32,
    /// Days in today's window that count under today's minimum.
    pub qualifying_days_now: u32,
    /// The same window judged under the next step's minimum.
    pub qualifying_days_next: Option<u32>,
    pub next_step: Option<rules::Step>,
    /// Extra deployments per day needed to meet the next step at the typical count.
    pub shortfall_next: u32,
    pub if_traffic_holds: Projection,
    pub if_traffic_stops: Projection,
    pub on_chain: Option<OnChain>,
}

pub fn report(
    counts: &DailyCounts,
    rules: &Rules,
    today: NaiveDate,
    on_chain: Option<OnChain>,
) -> Report {
    let typical_count = typical(counts, today, 7);
    let next_step = rules.next_step_after(today).cloned();
    let target = next_step
        .as_ref()
        .map_or(rules.subgraphs_per_day_at(today), |s| s.subgraphs_per_day);
    Report {
        today,
        observed_since: counts.keys().next().copied(),
        today_count: counts.get(&today).copied().unwrap_or(0),
        typical_count,
        subgraphs_per_day_now: rules.subgraphs_per_day_at(today),
        min_online_days: rules.min_online_days,
        qualifying_days_now: judge(counts, rules, today, rules.subgraphs_per_day_at(today))
            .qualifying_days,
        qualifying_days_next: next_step
            .as_ref()
            .map(|s| judge(counts, rules, today, s.subgraphs_per_day).qualifying_days),
        shortfall_next: target.saturating_sub(typical_count),
        next_step,
        if_traffic_holds: project(counts, rules, today, Assumption::Hold(typical_count), 14),
        if_traffic_stops: project(counts, rules, today, Assumption::Stop, 14),
        on_chain,
    }
}

/// Turn a report into findings. The chain is authoritative for "ineligible now"; the
/// projection is what warns ahead of time.
pub fn findings(r: &Report, now: DateTime<Utc>) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Some(c) = &r.on_chain
        && !c.eligible {
            out.push(Finding::new(
                CHECK,
                "reo_ineligible",
                "indexer",
                Severity::Critical,
                "REO reports this indexer ineligible: indexing rewards are withheld",
                json!({ "renewal_time": c.renewal_time, "last_oracle_update": c.last_oracle_update }),
                now,
            ));
        }
    let p = &r.if_traffic_holds;
    if let Some(fail) = p.first_failing_run {
        let days_left = p.eligible_until.map(|u| (u - r.today).num_days());
        out.push(Finding::new(
            CHECK,
            "reo_at_risk",
            "indexer",
            Severity::Warn,
            format!(
                "at the current {} deployments/day the oracle run on {fail} fails; \
                 {} more per day needed for the next step",
                r.typical_count, r.shortfall_next
            ),
            json!({
                "first_failing_run": fail,
                "eligible_until": p.eligible_until,
                "days_remaining": days_left,
                "typical_count": r.typical_count,
                "shortfall": r.shortfall_next,
                "next_step": r.next_step,
            }),
            now,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Days;

    #[test]
    fn seven_a_day_before_the_step_to_ten_warns_with_a_shortfall_of_three() {
        let today: NaiveDate = "2026-10-23".parse().unwrap();
        let counts: DailyCounts = (1..=10).map(|i| (today - Days::new(i), 7)).collect();
        let r = report(&counts, &Rules::default(), today, None);
        assert_eq!(r.shortfall_next, 3);
        let f = findings(&r, Utc::now());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, "reo_at_risk");
        assert_eq!(f[0].evidence["first_failing_run"], "2026-10-27");
    }

    #[test]
    fn chain_ineligibility_is_critical() {
        let today: NaiveDate = "2026-10-23".parse().unwrap();
        let counts: DailyCounts = (0..=10).map(|i| (today - Days::new(i), 12)).collect();
        let chain = OnChain {
            eligible: false,
            renewal_time: DateTime::UNIX_EPOCH,
            eligibility_period_secs: 1_209_600,
            last_oracle_update: Utc::now(),
        };
        let f = findings(
            &report(&counts, &Rules::default(), today, Some(chain)),
            Utc::now(),
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Critical);
    }
}
