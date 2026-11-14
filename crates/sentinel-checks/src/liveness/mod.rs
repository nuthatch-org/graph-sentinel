//! Module 1: liveness. A deployment is frozen when its head stops while the chain gives it
//! work. Each tick is one `step` per deployment; the caller does the I/O.

pub mod manifest;

use chrono::{DateTime, Utc};
use sentinel_types::{Finding, Severity};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const CHECK: &str = "liveness";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Within this many blocks of chain head a still head may simply be caught up.
    pub lag_tolerance_blocks: u64,
    /// Blocks after the head the activity probe looks at.
    pub activity_window_blocks: u64,
    /// Ticks of no movement and no matching logs before "idle" becomes a warning.
    pub idle_patience: u32,
    /// Ticks a chain head may stand still before the chain is called stalled.
    pub chain_patience: u32,
    /// Ticks a head must stand still before anything worse than idle is said. While syncing,
    /// graph-node commits its head only once per write batch (`GRAPH_STORE_WRITE_BATCH_DURATION`,
    /// 300 s by default), so without its in-memory head this must outlast one batch.
    pub min_still_ticks: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            lag_tolerance_blocks: 50,
            activity_window_blocks: 2_000,
            idle_patience: 3,
            chain_patience: 2,
            min_still_ticks: 1,
        }
    }
}

/// One index-node reading for one deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub deployment: String,
    pub network: String,
    pub health: String,
    pub synced: bool,
    pub paused: bool,
    pub assigned: bool,
    pub latest: Option<u64>,
    pub chain_head: Option<u64>,
    pub fatal_error: Option<String>,
}

/// What the activity probe found between the head and the window's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "logs")]
pub enum Probe {
    /// Logs matching the manifest's static addresses and topics.
    Logs(u64),
    /// Block handlers: every block is work.
    AlwaysActive,
    /// Not probeable (non-EVM, no static address, no RPC for the chain).
    Unknown,
    /// The RPC failed; say nothing stronger than "stopped".
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Moving,
    CaughtUp,
    Failed,
    Paused,
    /// Not moving, no evidence of work yet.
    Idle,
    /// Not moving, idle past patience: graph-node should still advance through empty blocks.
    Stalled,
    /// Not moving with work waiting: first sighting.
    Frozen,
    /// Frozen on two consecutive ticks.
    ConfirmedFrozen,
    /// Not moving and the probe could not tell.
    Stopped,
    ChainStalled,
}

impl Verdict {
    /// Worst first, for ordering a table.
    pub fn rank(self) -> u8 {
        match self {
            Verdict::ConfirmedFrozen => 0,
            Verdict::Frozen => 1,
            Verdict::Failed => 2,
            Verdict::ChainStalled => 3,
            Verdict::Stalled => 4,
            Verdict::Stopped => 5,
            Verdict::Idle => 6,
            Verdict::Paused => 7,
            Verdict::CaughtUp => 8,
            Verdict::Moving => 9,
        }
    }
}

/// Per-deployment memory between ticks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub last_latest: Option<u64>,
    pub last_chain_head: Option<u64>,
    pub last_progress: Option<DateTime<Utc>>,
    pub still_ticks: u32,
    pub chain_still_ticks: u32,
    pub frozen_ticks: u32,
    pub idle_ticks: u32,
    pub verdict: Option<Verdict>,
}

/// `deployment_head{deployment="Qm…"}` from graph-node's metrics: the block each deployment
/// has processed, ahead of what the index-node reports while a write batch is open.
pub fn parse_heads(metrics: &str) -> std::collections::HashMap<String, u64> {
    metrics
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("deployment_head{")?;
            let (labels, value) = rest.rsplit_once("} ")?;
            let d = labels.split(',').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                (k.trim() == "deployment").then(|| v.trim().trim_matches('"').to_string())
            })?;
            let v = value.trim().parse::<f64>().ok()?;
            Some((d, v as u64))
        })
        .collect()
}

/// Whether this tick needs the activity probe, so the caller can skip the RPC otherwise.
pub fn needs_probe(prev: &State, o: &Observation, cfg: &Config) -> bool {
    let (Some(latest), Some(head)) = (o.latest, o.chain_head) else {
        return false;
    };
    o.health == "healthy"
        && !o.paused
        && o.assigned
        && prev.last_latest == Some(latest)
        && head.saturating_sub(latest) > cfg.lag_tolerance_blocks
}

pub fn step(
    prev: &State,
    o: &Observation,
    probe: Option<Probe>,
    cfg: &Config,
    now: DateTime<Utc>,
) -> State {
    let mut s = prev.clone();
    let moved = match (prev.last_latest, o.latest) {
        (Some(a), Some(b)) => b > a,
        (None, Some(_)) => true, // first sight counts as progress
        _ => false,
    };
    let chain_moved = match (prev.last_chain_head, o.chain_head) {
        (Some(a), Some(b)) => b > a,
        _ => true,
    };
    s.last_latest = o.latest.or(prev.last_latest);
    s.last_chain_head = o.chain_head.or(prev.last_chain_head);
    s.chain_still_ticks = if chain_moved {
        0
    } else {
        prev.chain_still_ticks + 1
    };

    let verdict = if o.health == "failed" {
        Verdict::Failed
    } else if o.paused || !o.assigned {
        Verdict::Paused
    } else if moved {
        Verdict::Moving
    } else {
        let lag = match (o.latest, o.chain_head) {
            (Some(l), Some(h)) => h.saturating_sub(l),
            _ => u64::MAX,
        };
        if prev.still_ticks + 1 < cfg.min_still_ticks {
            Verdict::Idle
        } else if lag <= cfg.lag_tolerance_blocks {
            if s.chain_still_ticks >= cfg.chain_patience {
                Verdict::ChainStalled
            } else {
                Verdict::CaughtUp
            }
        } else {
            match probe {
                Some(Probe::Logs(n)) if n > 0 => frozen(prev),
                Some(Probe::AlwaysActive) => frozen(prev),
                Some(Probe::Logs(_)) => {
                    if prev.idle_ticks + 1 >= cfg.idle_patience {
                        Verdict::Stalled
                    } else {
                        Verdict::Idle
                    }
                }
                Some(Probe::Unknown) | Some(Probe::Failed) | None => {
                    // Without a witness, wait twice as long as for an idle deployment.
                    if prev.still_ticks + 1 >= cfg.idle_patience * 2 {
                        Verdict::Stopped
                    } else {
                        Verdict::Idle
                    }
                }
            }
        }
    };

    if moved {
        s.last_progress = Some(now);
        s.still_ticks = 0;
    } else {
        s.still_ticks = prev.still_ticks + 1;
    }
    s.frozen_ticks = if matches!(verdict, Verdict::Frozen | Verdict::ConfirmedFrozen) {
        prev.frozen_ticks + 1
    } else {
        0
    };
    s.idle_ticks = if matches!(verdict, Verdict::Idle | Verdict::Stalled)
        && matches!(probe, Some(Probe::Logs(0)))
    {
        prev.idle_ticks + 1
    } else {
        0
    };
    s.verdict = Some(verdict);
    s
}

fn frozen(prev: &State) -> Verdict {
    if prev.frozen_ticks >= 1 {
        Verdict::ConfirmedFrozen
    } else {
        Verdict::Frozen
    }
}

fn short(d: &str) -> String {
    if d.len() > 14 {
        format!("{}…{}", &d[..8], &d[d.len() - 4..])
    } else {
        d.into()
    }
}

pub fn finding(
    o: &Observation,
    s: &State,
    probe: Option<Probe>,
    now: DateTime<Utc>,
) -> Option<Finding> {
    let v = s.verdict?;
    let behind = match (o.latest, o.chain_head) {
        (Some(l), Some(h)) => Some(h.saturating_sub(l)),
        _ => None,
    };
    let since = s.last_progress.map(|t| (now - t).num_minutes());
    let d = short(&o.deployment);
    let evidence = json!({
        "deployment": o.deployment, "network": o.network, "health": o.health,
        "latest": o.latest, "chain_head": o.chain_head, "behind": behind,
        "probe": probe, "minutes_since_progress": since, "fatal_error": o.fatal_error,
    });
    let ago = since.map(|m| {
        if m >= 120 {
            format!("{} h", m / 60)
        } else {
            format!("{m} min")
        }
    });
    let (kind, sev, summary) = match v {
        Verdict::ConfirmedFrozen | Verdict::Frozen => {
            let why = match probe {
                Some(Probe::Logs(n)) => format!("{n} matching events are waiting"),
                _ => "it has block handlers, so every block is work".into(),
            };
            (
                "frozen",
                Severity::Critical,
                format!(
                    "{d} on {} has not moved{} while the index-node reports it healthy; {why}. \
                     graphman restart usually clears it.",
                    o.network,
                    ago.map(|a| format!(" for {a}")).unwrap_or_default()
                ),
            )
        }
        Verdict::Stalled => (
            "stalled",
            Severity::Warn,
            format!(
                "{d} on {} is {} blocks behind and not moving, though no events are pending. \
                 graph-node normally walks the head through empty blocks.",
                o.network,
                behind.unwrap_or(0)
            ),
        ),
        Verdict::Stopped => (
            "stopped",
            Severity::Warn,
            format!(
                "{d} on {} is {} blocks behind and has not moved{}. No RPC witness could say whether work is waiting.",
                o.network,
                behind.unwrap_or(0),
                ago.map(|a| format!(" for {a}")).unwrap_or_default()
            ),
        ),
        Verdict::Failed => (
            "failed",
            Severity::Warn,
            format!(
                "{d} on {} has failed: {}",
                o.network,
                o.fatal_error
                    .as_deref()
                    .unwrap_or("no error message")
                    .chars()
                    .take(240)
                    .collect::<String>()
            ),
        ),
        Verdict::ChainStalled => (
            "chain_stalled",
            Severity::Warn,
            format!(
                "The {} chain head has not moved; every deployment on it is waiting.",
                o.network
            ),
        ),
        _ => return None,
    };
    // A chain stall is one finding per chain, not one per deployment.
    let subject = if v == Verdict::ChainStalled {
        o.network.clone()
    } else {
        o.deployment.clone()
    };
    Some(Finding::new(
        CHECK, kind, subject, sev, summary, evidence, now,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(latest: u64, head: u64) -> Observation {
        Observation {
            deployment: "QmZA6BHHHiFGZAawSw4UVPjEBFT87Z3wu3nc7K7rgEZpeV".into(),
            network: "arbitrum-one".into(),
            health: "healthy".into(),
            synced: true,
            paused: false,
            assigned: true,
            latest: Some(latest),
            chain_head: Some(head),
            fatal_error: None,
        }
    }

    fn run(seq: &[(u64, u64, Option<Probe>)]) -> Vec<Verdict> {
        let cfg = Config::default();
        let mut s = State::default();
        seq.iter()
            .map(|(l, h, p)| {
                s = step(&s, &obs(*l, *h), *p, &cfg, Utc::now());
                s.verdict.unwrap()
            })
            .collect()
    }

    #[test]
    fn moving_then_frozen_then_confirmed() {
        let v = run(&[
            (100, 5_000, None),
            (200, 5_100, None),
            (200, 5_200, Some(Probe::Logs(4))),
            (200, 5_300, Some(Probe::Logs(9))),
        ]);
        assert_eq!(
            v,
            [
                Verdict::Moving,
                Verdict::Moving,
                Verdict::Frozen,
                Verdict::ConfirmedFrozen
            ]
        );
    }

    // Seen on graph-node v0.45.0, 2026-11-14: a syncing deployment's committed head held for
    // four one-minute ticks with work waiting while it processed 216 blocks in memory.
    #[test]
    fn a_committed_head_inside_one_write_batch_is_not_frozen() {
        let cfg = Config {
            min_still_ticks: 5,
            ..Config::default()
        };
        let mut s = State::default();
        let mut v = Vec::new();
        for (i, p) in [None, Some(Probe::Logs(1559))]
            .into_iter()
            .chain([Some(Probe::Logs(1559)); 5])
            .enumerate()
        {
            s = step(
                &s,
                &obs(33_799_448, 57_445_904 + i as u64),
                p,
                &cfg,
                Utc::now(),
            );
            v.push(s.verdict.unwrap());
        }
        assert_eq!(
            v[..5],
            [
                Verdict::Moving,
                Verdict::Idle,
                Verdict::Idle,
                Verdict::Idle,
                Verdict::Idle
            ]
        );
        assert_eq!(v[5], Verdict::Frozen);
        assert_eq!(v[6], Verdict::ConfirmedFrozen);
    }

    #[test]
    fn heads_come_from_graph_nodes_metric() {
        let m = "# TYPE deployment_head gauge\n\
                 deployment_head{deployment=\"QmRmz2CiRy3LqJhfHFJivvzSAXjwbuZobpCq13hjTibaNF\",network=\"base\",shard=\"primary\"} 33799926\n\
                 deployment_status{deployment=\"QmRmz2CiRy3LqJhfHFJivvzSAXjwbuZobpCq13hjTibaNF\",shard=\"\"} 2\n";
        let h = parse_heads(m);
        assert_eq!(h.len(), 1);
        assert_eq!(
            h["QmRmz2CiRy3LqJhfHFJivvzSAXjwbuZobpCq13hjTibaNF"],
            33_799_926
        );
    }

    #[test]
    fn caught_up_is_fine_until_the_chain_itself_stops() {
        let v = run(&[
            (1_000, 1_010, None),
            (1_000, 1_010, None),
            (1_000, 1_010, None),
        ]);
        assert_eq!(
            v,
            [Verdict::Moving, Verdict::CaughtUp, Verdict::ChainStalled]
        );
    }

    #[test]
    fn idle_needs_patience_before_it_warns() {
        let z = Some(Probe::Logs(0));
        let v = run(&[
            (100, 9_000, None),
            (100, 9_100, z),
            (100, 9_200, z),
            (100, 9_300, z),
        ]);
        assert_eq!(
            v,
            [
                Verdict::Moving,
                Verdict::Idle,
                Verdict::Idle,
                Verdict::Stalled
            ]
        );
    }

    #[test]
    fn block_handlers_freeze_without_logs() {
        let v = run(&[(100, 9_000, None), (100, 9_100, Some(Probe::AlwaysActive))]);
        assert_eq!(v[1], Verdict::Frozen);
    }

    #[test]
    fn without_a_witness_it_waits_longer_and_says_less() {
        let u = Some(Probe::Unknown);
        let v = run(&[
            (1, 9_000, None),
            (1, 9_000, u),
            (1, 9_000, u),
            (1, 9_000, u),
            (1, 9_000, u),
            (1, 9_000, u),
            (1, 9_000, u),
        ]);
        assert_eq!(v[5], Verdict::Idle);
        assert_eq!(v[6], Verdict::Stopped);
    }

    #[test]
    fn needs_probe_only_when_behind_and_still() {
        let cfg = Config::default();
        let s = step(&State::default(), &obs(100, 9_000), None, &cfg, Utc::now());
        assert!(needs_probe(&s, &obs(100, 9_100), &cfg));
        assert!(!needs_probe(&s, &obs(101, 9_100), &cfg), "moved");
        assert!(!needs_probe(&s, &obs(100, 130), &cfg), "caught up");
    }

    #[test]
    fn findings_name_the_problem() {
        let cfg = Config::default();
        let s0 = step(&State::default(), &obs(100, 9_000), None, &cfg, Utc::now());
        let s1 = step(
            &s0,
            &obs(100, 9_100),
            Some(Probe::Logs(3)),
            &cfg,
            Utc::now(),
        );
        let f = finding(&obs(100, 9_100), &s1, Some(Probe::Logs(3)), Utc::now()).unwrap();
        assert_eq!(
            (f.kind.as_str(), f.severity),
            ("frozen", Severity::Critical)
        );
        assert!(f.summary.contains("3 matching events are waiting"));
        assert!(
            finding(&obs(100, 9_100), &s0, None, Utc::now()).is_none(),
            "moving is not a finding"
        );
    }
}
