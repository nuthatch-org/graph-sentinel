//! Module 3: the REO eligibility meter.

pub mod projection;
pub mod ranking;
pub mod rules;

use chrono::{DateTime, Days, NaiveDate, Utc};
use sentinel_types::{Finding, Severity};
use serde::Serialize;
use serde_json::json;

use projection::{Assumption, DailyCounts, Projection, RunVerdict, judge, project, typical};
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
    /// The ledger's verdict on the oracle's most recent posted run, beside what the chain did.
    pub last_posted: Option<PostedRun>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PostedRun {
    pub ledger: RunVerdict,
    pub renewed: bool,
}

impl PostedRun {
    /// The ledger counted enough qualifying days but the oracle did not renew: queries that
    /// earned receipts failed the oracle's latency or freshness bar.
    pub fn overstated(&self) -> bool {
        self.ledger.passes && !self.renewed
    }
}

/// UTC day the oracle's renewal expires on, minus one: the last full day covered.
fn chain_until(c: &OnChain) -> Option<NaiveDate> {
    (c.renewal_time.timestamp() > 0).then(|| {
        (c.renewal_time + chrono::Duration::seconds(c.eligibility_period_secs as i64)).date_naive()
            - Days::new(1)
    })
}

/// Runs the oracle has already posted have a verdict on chain; only later runs are projected.
fn reconcile(p: &mut Projection, c: &OnChain, eligibility_days: i64) {
    let posted = c.last_oracle_update.date_naive();
    p.runs.retain(|v| v.run > posted);
    let projected = p
        .runs
        .iter()
        .filter(|v| v.passes)
        .map(|v| v.run + Days::new(eligibility_days as u64 - 1))
        .max();
    p.eligible_until = chain_until(c).max(projected);
    p.first_failing_run = p.runs.iter().find(|v| !v.passes).map(|v| v.run);
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
    let mut if_traffic_holds = project(counts, rules, today, Assumption::Hold(typical_count), 14);
    let mut if_traffic_stops = project(counts, rules, today, Assumption::Stop, 14);
    let last_posted = on_chain.as_ref().map(|c| {
        reconcile(&mut if_traffic_holds, c, rules.eligibility_days);
        reconcile(&mut if_traffic_stops, c, rules.eligibility_days);
        let posted = c.last_oracle_update.date_naive();
        PostedRun {
            ledger: judge(counts, rules, posted, rules.subgraphs_per_day_at(posted)),
            renewed: c.renewal_time.date_naive() >= posted,
        }
    });
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
        if_traffic_holds,
        if_traffic_stops,
        on_chain,
        last_posted,
    }
}

pub fn day_name(d: NaiveDate) -> String {
    d.format("%-d %b").to_string()
}

/// Turn a report into findings. The chain is authoritative for "ineligible now"; the
/// projection is what warns ahead of time.
pub fn findings(r: &Report, now: DateTime<Utc>) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Some(c) = &r.on_chain
        && !c.eligible
    {
        out.push(Finding::new(
            CHECK,
            "reo_ineligible",
            "indexer",
            Severity::Critical,
            "The oracle has this indexer ineligible. Indexing rewards are withheld until a run renews it.",
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
                "At {} deployments a day the oracle run on {} fails. Serve {} more a day to clear it.",
                r.typical_count,
                day_name(fail),
                r.shortfall_next
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
    if let Some(lp) = r.last_posted.as_ref().filter(|lp| lp.overstated()) {
        out.push(Finding::new(
            CHECK,
            "reo_oracle_disagrees",
            "indexer",
            Severity::Warn,
            format!(
                "Receipts show {} qualifying days for the run on {}, but the oracle did not renew. \
                 Queries are probably failing its latency or freshness bar.",
                lp.ledger.qualifying_days,
                day_name(lp.ledger.run)
            ),
            json!({ "run": lp.ledger.run, "ledger_qualifying_days": lp.ledger.qualifying_days,
                    "subgraphs_per_day": lp.ledger.subgraphs_per_day }),
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
        let kinds: Vec<_> = f.iter().map(|f| f.kind.as_str()).collect();
        assert_eq!(kinds, ["reo_ineligible", "reo_oracle_disagrees"]);
        assert_eq!(f[0].severity, Severity::Critical);
    }

    fn chain(eligible: bool, renewed: &str, posted: &str) -> OnChain {
        let t = |s: &str| format!("{s}T10:00:00Z").parse::<DateTime<Utc>>().unwrap();
        OnChain {
            eligible,
            renewal_time: if renewed.is_empty() {
                DateTime::UNIX_EPOCH
            } else {
                t(renewed)
            },
            eligibility_period_secs: 14 * 86_400,
            last_oracle_update: t(posted),
        }
    }

    #[test]
    fn posted_runs_come_from_the_chain_not_the_projection() {
        let today: NaiveDate = "2026-10-23".parse().unwrap();
        // Plenty of receipts, so the ledger thinks every run passes.
        let counts: DailyCounts = (0..=10).map(|i| (today - Days::new(i), 12)).collect();
        let r = report(
            &counts,
            &Rules::default(),
            today,
            Some(chain(false, "", "2026-10-23")),
        );
        let p = &r.if_traffic_holds;
        // Today's run is posted: the strip starts tomorrow.
        assert_eq!(p.runs.first().unwrap().run.to_string(), "2026-10-24");
        // Never renewed on chain, so cover is only what tomorrow's run would grant.
        assert_eq!(p.eligible_until.unwrap().to_string(), "2026-11-19");
        assert!(r.last_posted.as_ref().unwrap().overstated());
    }

    #[test]
    fn chain_renewal_sets_the_floor_of_cover() {
        let today: NaiveDate = "2026-10-23".parse().unwrap();
        let r = report(
            &DailyCounts::new(),
            &Rules::default(),
            today,
            Some(chain(true, "2026-10-20", "2026-10-22")),
        );
        let p = &r.if_traffic_stops;
        // Renewed 20 Oct 10:00 for 14 days: expires 3 Nov 10:00, last full day 2 Nov.
        assert_eq!(p.eligible_until.unwrap().to_string(), "2026-11-02");
        assert_eq!(p.first_failing_run.unwrap().to_string(), "2026-10-23");
        assert!(
            !r.last_posted.unwrap().overstated(),
            "no receipts: the ledger cannot overstate"
        );
    }
}
