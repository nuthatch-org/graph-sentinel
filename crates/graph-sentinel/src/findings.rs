//! Finding lifecycle: fire on first sight and on escalation, resolve after two quiet ticks.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sentinel_types::Finding;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq)]
pub struct Stored {
    pub finding: Finding,
    pub active: bool,
    /// Consecutive ticks the finding has been absent while active.
    pub missed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    New,
    Escalated,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub transition: Transition,
    pub finding: Finding,
}

const RESOLVE_AFTER: u32 = 2;

/// Merge one tick's findings for a check into its stored state.
pub fn apply(
    prev: Vec<Stored>,
    current: Vec<Finding>,
    now: DateTime<Utc>,
) -> (Vec<Stored>, Vec<Event>) {
    let mut by_fp: HashMap<String, Stored> = prev
        .into_iter()
        .map(|s| (s.finding.fingerprint.clone(), s))
        .collect();
    let mut events = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for mut f in current {
        seen.insert(f.fingerprint.clone());
        match by_fp.get_mut(&f.fingerprint) {
            Some(s) if s.active => {
                let escalated = f.severity > s.finding.severity;
                f.first_seen = s.finding.first_seen;
                f.last_seen = now;
                s.finding = f;
                s.missed = 0;
                if escalated {
                    events.push(Event {
                        transition: Transition::Escalated,
                        finding: s.finding.clone(),
                    });
                }
            }
            _ => {
                f.first_seen = now;
                f.last_seen = now;
                events.push(Event {
                    transition: Transition::New,
                    finding: f.clone(),
                });
                by_fp.insert(
                    f.fingerprint.clone(),
                    Stored {
                        finding: f,
                        active: true,
                        missed: 0,
                    },
                );
            }
        }
    }

    for s in by_fp.values_mut() {
        if s.active && !seen.contains(&s.finding.fingerprint) {
            s.missed += 1;
            if s.missed >= RESOLVE_AFTER {
                s.active = false;
                events.push(Event {
                    transition: Transition::Resolved,
                    finding: s.finding.clone(),
                });
            }
        }
    }
    (by_fp.into_values().collect(), events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_types::Severity;
    use serde_json::json;

    fn f(sev: Severity) -> Finding {
        Finding::new(
            "reo",
            "reo_at_risk",
            "indexer",
            sev,
            "x",
            json!({}),
            Utc::now(),
        )
    }

    fn kinds(e: &[Event]) -> Vec<Transition> {
        e.iter().map(|e| e.transition).collect()
    }

    #[test]
    fn lifecycle() {
        let now = Utc::now();
        let (s, e) = apply(vec![], vec![f(Severity::Warn)], now);
        assert_eq!(kinds(&e), [Transition::New]);

        let (s, e) = apply(s, vec![f(Severity::Warn)], now);
        assert!(e.is_empty(), "a repeat is quiet");

        let (s, e) = apply(s, vec![f(Severity::Critical)], now);
        assert_eq!(kinds(&e), [Transition::Escalated]);

        let (s, e) = apply(s, vec![], now);
        assert!(e.is_empty(), "one quiet tick is not a resolution");
        let (s, e) = apply(s, vec![], now);
        assert_eq!(kinds(&e), [Transition::Resolved]);
        assert!(!s[0].active);

        let (_, e) = apply(s, vec![f(Severity::Warn)], now);
        assert_eq!(kinds(&e), [Transition::New], "a recurrence fires again");
    }

    #[test]
    fn a_flap_inside_the_resolve_window_stays_one_finding() {
        let now = Utc::now();
        let (s, _) = apply(vec![], vec![f(Severity::Warn)], now);
        let (s, _) = apply(s, vec![], now);
        let (s, e) = apply(s, vec![f(Severity::Warn)], now);
        assert!(e.is_empty());
        assert_eq!(s[0].missed, 0);
    }
}
