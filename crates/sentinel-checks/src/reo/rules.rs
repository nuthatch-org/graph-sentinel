use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// One dated value of the oracle's subgraphs-per-day minimum (`MIN_SUBGRAPHS`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub from: NaiveDate,
    pub subgraphs_per_day: u32,
}

/// The oracle's rule, as published in rewards-eligibility-oracle's ELIGIBILITY_CRITERIA.md.
/// Each daily run judges every day in `[run - window_days, run]` against the minimum in force
/// on the run date, so a raised minimum applies retroactively to the whole window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rules {
    pub min_online_days: u32,
    pub window_days: i64,
    pub eligibility_days: i64,
    pub steps: Vec<Step>,
}

impl Default for Rules {
    fn default() -> Self {
        let d = |s: &str| s.parse().expect("static date");
        Rules {
            min_online_days: 5,
            window_days: 28,
            eligibility_days: 14,
            steps: vec![
                Step {
                    from: d("2026-08-25"),
                    subgraphs_per_day: 1,
                },
                Step {
                    from: d("2026-10-06"),
                    subgraphs_per_day: 5,
                },
                Step {
                    from: d("2026-10-27"),
                    subgraphs_per_day: 10,
                },
            ],
        }
    }
}

impl Rules {
    pub fn normalised(mut self) -> Self {
        self.steps.sort_by_key(|s| s.from);
        self
    }

    /// The minimum in force on `day`. Before the first step the oracle did not exist; treat as 1.
    pub fn subgraphs_per_day_at(&self, day: NaiveDate) -> u32 {
        self.steps
            .iter()
            .rev()
            .find(|s| s.from <= day)
            .map_or(1, |s| s.subgraphs_per_day)
    }

    pub fn next_step_after(&self, day: NaiveDate) -> Option<&Step> {
        self.steps.iter().find(|s| s.from > day)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn minimum_follows_schedule() {
        let r = Rules::default();
        assert_eq!(r.subgraphs_per_day_at(d("2026-10-05")), 1);
        assert_eq!(r.subgraphs_per_day_at(d("2026-10-06")), 5);
        assert_eq!(r.subgraphs_per_day_at(d("2026-10-26")), 5);
        assert_eq!(r.subgraphs_per_day_at(d("2026-10-27")), 10);
        assert_eq!(
            r.next_step_after(d("2026-10-23"))
                .unwrap()
                .subgraphs_per_day,
            10
        );
        assert!(r.next_step_after(d("2026-10-27")).is_none());
    }
}
