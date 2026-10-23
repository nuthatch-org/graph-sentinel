//! Liveness as a service: one index-node read per tick, an RPC probe only for deployments
//! that are behind and still, manifests fetched once and kept.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Instant;

use anyhow::Result;
use chrono::{DateTime, Utc};
use sentinel_checks::liveness::manifest::{self, Activity};
use sentinel_checks::liveness::{self, Observation, Probe, State, Verdict};
use sentinel_sources::index_node::{IndexNode, Status};
use sentinel_sources::ipfs::Ipfs;
use sentinel_sources::rpc::Rpc;
use sentinel_types::{DeploymentId, Finding, Severity};
use serde::Serialize;
use serde_json::json;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::store::Store;

/// One deployment as the page shows it.
#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub deployment: String,
    pub network: String,
    pub health: String,
    pub synced: bool,
    pub latest: Option<u64>,
    pub chain_head: Option<u64>,
    pub behind: Option<u64>,
    pub verdict: Verdict,
    pub last_progress: Option<DateTime<Utc>>,
    pub probe: Option<Probe>,
}

pub struct Liveness {
    node: IndexNode,
    ipfs: Ipfs,
    rpcs: HashMap<String, Rpc>,
    store: Store,
    cfg: crate::config::Liveness,
    states: Mutex<HashMap<String, State>>,
    restarts: Mutex<HashMap<String, Instant>>,
}

impl Liveness {
    pub fn new(cfg: &Config, store: Store) -> Option<Self> {
        let url = cfg.sources.index_node_url.clone()?;
        Some(Liveness {
            node: IndexNode::new(url),
            ipfs: Ipfs::new(cfg.sources.ipfs_url.clone()),
            rpcs: cfg
                .sources
                .rpc
                .iter()
                .map(|(k, v)| (k.clone(), Rpc::new(v.clone())))
                .collect(),
            store,
            cfg: cfg.liveness.clone(),
            states: Mutex::new(HashMap::new()),
            restarts: Mutex::new(HashMap::new()),
        })
    }

    async fn activity(&self, deployment: &str) -> Result<Activity> {
        if let Some(a) = self.store.manifest(deployment).await? {
            return Ok(a);
        }
        let yaml = self.ipfs.cat(deployment).await?;
        let a = manifest::parse(&yaml).map_err(|e| anyhow::anyhow!("manifest: {e}"))?;
        self.store.save_manifest(deployment, &a).await?;
        Ok(a)
    }

    async fn probe(&self, st: &Status, cfg: &liveness::Config) -> Probe {
        let a = match self.activity(&st.deployment).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(deployment = %st.deployment, error = %e, "manifest unavailable");
                return Probe::Unknown;
            }
        };
        if a.always_active() {
            return Probe::AlwaysActive;
        }
        let (Some(rpc), true) = (self.rpcs.get(&st.network), a.probeable()) else {
            return Probe::Unknown;
        };
        let (Some(latest), Some(head)) = (st.latest, st.chain_head) else {
            return Probe::Unknown;
        };
        let to = head.min(latest + cfg.activity_window_blocks);
        let addrs: Vec<String> = a.addresses.iter().cloned().collect();
        let topics: Vec<String> = a.topics.iter().cloned().collect();
        match rpc.count_logs(latest + 1, to, &addrs, &topics).await {
            Ok((n, _, _)) if n > 0 => Probe::Logs(n),
            // Templates mean unseen addresses: zero static logs proves nothing.
            Ok(_) if !a.probe_complete() => Probe::Unknown,
            Ok((n, _, _)) => Probe::Logs(n),
            Err(e) => {
                tracing::warn!(network = %st.network, error = %e, "activity probe failed");
                Probe::Failed
            }
        }
    }

    pub async fn tick(&self) -> Result<(Vec<Row>, Vec<Finding>)> {
        let statuses = self.node.statuses().await?;
        let cfg = self.cfg.check_config();
        let now = Utc::now();
        let mut states = self.states.lock().await;
        let mut rows = Vec::with_capacity(statuses.len());
        let mut findings = Vec::new();
        for st in statuses {
            let o = Observation {
                deployment: st.deployment.clone(),
                network: st.network.clone(),
                health: st.health.clone(),
                synced: st.synced,
                paused: st.paused,
                assigned: st.assigned,
                latest: st.latest,
                chain_head: st.chain_head,
                fatal_error: st.fatal_error.clone(),
            };
            let prev = states.get(&st.deployment).cloned().unwrap_or_default();
            let probe = if liveness::needs_probe(&prev, &o, &cfg) {
                Some(self.probe(&st, &cfg).await)
            } else {
                None
            };
            let s = liveness::step(&prev, &o, probe, &cfg, now);
            if let Some(f) = liveness::finding(&o, &s, probe, now) {
                findings.push(f);
            }
            if s.verdict == Some(Verdict::ConfirmedFrozen) && self.cfg.restart {
                findings.extend(self.restart(&st.deployment, now).await);
            }
            rows.push(Row {
                deployment: st.deployment.clone(),
                network: st.network,
                health: st.health,
                synced: st.synced,
                latest: st.latest,
                chain_head: st.chain_head,
                behind: st
                    .latest
                    .zip(st.chain_head)
                    .map(|(l, h)| h.saturating_sub(l)),
                verdict: s.verdict.unwrap_or(Verdict::Moving),
                last_progress: s.last_progress,
                probe,
            });
            states.insert(st.deployment, s);
        }
        Ok((rows, findings))
    }

    async fn restart(&self, deployment: &str, now: DateTime<Utc>) -> Option<Finding> {
        // The hash goes into a shell command: accept nothing but a well-formed deployment id.
        let id: DeploymentId = deployment.parse().ok()?;
        let mut last = self.restarts.lock().await;
        if last
            .get(deployment)
            .is_some_and(|t| t.elapsed() < self.cfg.restart_cooldown)
        {
            return None;
        }
        last.insert(deployment.to_string(), Instant::now());
        let cmd = self
            .cfg
            .restart_command
            .replace("{deployment}", id.as_str());
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdin(Stdio::null())
            .output()
            .await;
        let (ok, detail) = match out {
            Ok(o) => (
                o.status.success(),
                format!(
                    "exit {}: {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr)
                        .chars()
                        .take(200)
                        .collect::<String>()
                ),
            ),
            Err(e) => (false, e.to_string()),
        };
        tracing::warn!(%deployment, ok, "issued restart: {detail}");
        Some(Finding::new(
            liveness::CHECK,
            "restart_issued",
            deployment,
            if ok {
                Severity::Warn
            } else {
                Severity::Critical
            },
            if ok {
                format!(
                    "Restarted {} with graphman after it froze. Watching whether it moves.",
                    &deployment[..12]
                )
            } else {
                format!(
                    "Tried to restart {} and the command failed: {detail}",
                    &deployment[..12]
                )
            },
            json!({ "command": cmd, "ok": ok, "detail": detail }),
            now,
        ))
    }
}
