use std::collections::BTreeMap;

use chrono::{Days, NaiveDate};
use serde::Serialize;

use super::rules::Rules;

/// Distinct deployments served per UTC day. A day absent from the map served nothing.
pub type DailyCounts = BTreeMap<NaiveDate, u32>;

/// How future days are filled in when projecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "per_day")]
pub enum Assumption {
    /// Every future day serves this many deployments.
    Hold(u32),
    /// No more qualifying traffic from today on.
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunVerdict {
    pub run: NaiveDate,
    pub subgraphs_per_day: u32,
    pub qualifying_days: u32,
    pub passes: bool,
}

/// What the oracle's run on `run` would decide, given per-day counts and the minimum `n`.
pub fn judge(counts: &DailyCounts, rules: &Rules, run: NaiveDate, n: u32) -> RunVerdict {
    let start = run - Days::new(rules.window_days as u64);
    let qualifying_days = counts.range(start..=run).filter(|(_, c)| **c >= n).count() as u32;
    RunVerdict {
        run,
        subgraphs_per_day: n,
        qualifying_days,
        passes: qualifying_days >= rules.min_online_days,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Projection {
    pub today: NaiveDate,
    pub assumption: Assumption,
    /// Each run from today to the horizon, judged under the published schedule.
    pub runs: Vec<RunVerdict>,
    /// Last day covered by a renewal, counting renewals we can compute (today - 13 onward).
    pub eligible_until: Option<NaiveDate>,
    /// The first projected run that fails, if any.
    pub first_failing_run: Option<NaiveDate>,
}

/// Run the oracle forward from today under the schedule. Past runs inside the eligibility
/// period are recomputed from the same counts, so history graph-sentinel never saw counts as zero.
pub fn project(
    counts: &DailyCounts,
    rules: &Rules,
    today: NaiveDate,
    assumption: Assumption,
    horizon_days: u64,
) -> Projection {
    let mut filled = counts.clone();
    if let Assumption::Hold(per_day) = assumption {
        for i in 1..=horizon_days {
            filled.insert(today + Days::new(i), per_day);
        }
        // Today is partial: never project it lower than what is already observed.
        let t = filled.entry(today).or_insert(0);
        *t = (*t).max(per_day);
    }

    let lookback = (rules.eligibility_days - 1).max(0) as u64;
    let mut eligible_until = None;
    let mut runs = Vec::new();
    let mut first_failing_run = None;
    for i in 0..=(lookback + horizon_days) {
        let run = today - Days::new(lookback) + Days::new(i);
        let v = judge(&filled, rules, run, rules.subgraphs_per_day_at(run));
        if v.passes {
            eligible_until = Some(run + Days::new(rules.eligibility_days as u64 - 1));
        }
        if run >= today {
            if !v.passes && first_failing_run.is_none() {
                first_failing_run = Some(run);
            }
            runs.push(v);
        }
    }
    Projection {
        today,
        assumption,
        runs,
        eligible_until,
        first_failing_run,
    }
}

/// The typical daily count: the median of the last `days` complete days before `today`.
pub fn typical(counts: &DailyCounts, today: NaiveDate, days: u64) -> u32 {
    let mut v: Vec<u32> = (1..=days)
        .map(|i| counts.get(&(today - Days::new(i))).copied().unwrap_or(0))
        .collect();
    v.sort_unstable();
    v[v.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn run_of(start: &str, values: &[u32]) -> DailyCounts {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| (d(start) + Days::new(i as u64), *v))
            .collect()
    }

    #[test]
    fn window_is_inclusive_of_both_ends() {
        let r = Rules::default();
        // 28 days back plus the run day itself: 29 calendar days.
        let mut c = DailyCounts::new();
        c.insert(d("2026-09-25"), 7); // run - 28: inside
        c.insert(d("2026-09-24"), 7); // run - 29: outside
        c.insert(d("2026-10-23"), 7); // run day: inside
        let v = judge(&c, &r, d("2026-10-23"), 5);
        assert_eq!(v.qualifying_days, 2);
    }

    #[test]
    fn step_to_ten_is_retroactive() {
        let r = Rules::default();
        // Seven days at 7 deployments: comfortably eligible at N=5.
        let c = run_of("2026-10-16", &[7, 7, 7, 7, 7, 7, 7]);
        assert!(judge(&c, &r, d("2026-10-22"), 5).passes);
        // The run on 10-27 re-judges the same days at N=10 and none of them count.
        let p = project(&c, &r, d("2026-10-23"), Assumption::Hold(7), 14);
        let on_27 = p.runs.iter().find(|v| v.run == d("2026-10-27")).unwrap();
        assert_eq!(on_27.subgraphs_per_day, 10);
        assert_eq!(on_27.qualifying_days, 0);
        assert_eq!(p.first_failing_run, Some(d("2026-10-27")));
        // Last passing run 10-26, plus a 14-day period.
        assert_eq!(p.eligible_until, Some(d("2026-11-08")));
    }

    #[test]
    fn holding_above_the_new_minimum_stays_eligible() {
        let r = Rules::default();
        let c = run_of("2026-10-16", &[12; 8]);
        let p = project(&c, &r, d("2026-10-23"), Assumption::Hold(12), 14);
        assert_eq!(p.first_failing_run, None);
        assert_eq!(p.eligible_until, Some(d("2026-11-19")));
    }

    #[test]
    fn stopping_traffic_runs_out_after_the_window() {
        let r = Rules {
            steps: vec![],
            ..Rules::default()
        }; // N = 1 throughout
        let c = run_of("2026-10-19", &[3, 3, 3, 3, 3]); // five qualifying days, 10-19..10-23
        let p = project(&c, &r, d("2026-10-23"), Assumption::Stop, 40);
        // The run on 11-16 still sees 10-19 (= run - 28); the run on 11-17 does not.
        let last_pass = p.runs.iter().filter(|v| v.passes).map(|v| v.run).max();
        assert_eq!(last_pass, Some(d("2026-11-16")));
        assert_eq!(p.first_failing_run, Some(d("2026-11-17")));
        assert_eq!(p.eligible_until, Some(d("2026-11-29")));
    }

    #[test]
    fn partial_today_is_not_projected_downwards() {
        let r = Rules {
            steps: vec![],
            ..Rules::default()
        };
        let mut c = run_of("2026-10-19", &[1, 1, 1, 1]);
        c.insert(d("2026-10-23"), 9);
        let p = project(&c, &r, d("2026-10-23"), Assumption::Hold(0), 0);
        assert!(p.runs[0].passes);
    }

    #[test]
    fn typical_is_the_median_of_complete_days() {
        let c = run_of("2026-10-16", &[4, 9, 6, 6, 1, 8, 6, 30]); // 10-23 is today, ignored
        assert_eq!(typical(&c, d("2026-10-23"), 7), 6);
        assert_eq!(typical(&DailyCounts::new(), d("2026-10-23"), 7), 0);
    }
}
