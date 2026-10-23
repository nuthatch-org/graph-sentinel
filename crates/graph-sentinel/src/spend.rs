//! The spend guard as a service: read graph-node's request counters every tick, price the
//! billed providers, and stop graph-node when the day's budget is gone.

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use sentinel_checks::spend::{self, Budget, Counters, Pricing};
use serde::Serialize;

use crate::config::Spend as SpendConfig;
use crate::store::Store;

#[derive(Debug, Clone, Serialize)]
pub struct Line {
    pub provider: String,
    pub method: String,
    pub requests: u64,
    pub usd: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct View {
    pub today_usd: f64,
    pub budget_usd: f64,
    pub projected_usd: f64,
    pub stopped: bool,
    pub billed_providers: Vec<String>,
    /// Today's most expensive provider and method pairs.
    pub top: Vec<Line>,
    /// Oldest first.
    pub days: Vec<(NaiveDate, f64)>,
    /// When the guard's own memory of today starts (its first reading).
    pub counting_since: chrono::DateTime<Utc>,
}

pub struct Guard {
    http: reqwest::Client,
    cfg: SpendConfig,
    pricing: Pricing,
    store: Store,
    last: Option<(Instant, Counters)>,
    started: chrono::DateTime<Utc>,
    /// Shared with liveness: while set, graph-node being down is the guard's doing.
    pub stopped: Arc<AtomicBool>,
}

async fn run(cmd: &str) -> Result<String> {
    let o = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .output()
        .await?;
    let err = String::from_utf8_lossy(&o.stderr)
        .chars()
        .take(200)
        .collect::<String>();
    if o.status.success() {
        Ok(err)
    } else {
        anyhow::bail!("exit {}: {err}", o.status)
    }
}

impl Guard {
    pub async fn new(cfg: SpendConfig, store: Store) -> Result<Self> {
        let pricing = Pricing {
            billed_providers: cfg.billed_providers.clone(),
            usd_per_million_cu: cfg.usd_per_million_cu,
            ..Pricing::default()
        };
        let today = Utc::now().date_naive();
        let stopped = store
            .meta("spend_stopped_day")
            .await?
            .is_some_and(|d| d == today.to_string());
        Ok(Guard {
            http: reqwest::Client::new(),
            cfg,
            pricing,
            store,
            last: None,
            started: Utc::now(),
            stopped: Arc::new(AtomicBool::new(stopped)),
        })
    }

    async fn read(&self) -> Result<Counters> {
        let text = self
            .http
            .get(&self.cfg.graph_node_metrics_url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .context("graph-node metrics")?
            .error_for_status()?
            .text()
            .await?;
        Ok(spend::parse_metrics(&text))
    }

    /// One tick. Returns the page's view and the day's findings.
    pub async fn tick(&mut self) -> Result<(View, Vec<sentinel_types::Finding>)> {
        let now = Utc::now();
        let today = now.date_naive();
        self.resume_if_new_day(today).await;

        // graph-node down (perhaps by our hand) is no spend, not an error.
        let (recent_usd, recent_secs) = match self.read().await {
            Ok(cur) => {
                let out = match &self.last {
                    Some((t, prev)) => {
                        let d = spend::delta(prev, &cur);
                        for ((p, m), n) in &d {
                            let cu = if self.pricing.billed(p) {
                                n * self.pricing.cu(m)
                            } else {
                                0
                            };
                            self.store.add_spend(today, p, m, *n, cu).await?;
                        }
                        (
                            self.pricing.usd(self.pricing.cost_cu(&d)),
                            t.elapsed().as_secs_f64(),
                        )
                    }
                    // The first reading is the baseline: counts before it have no date.
                    None => (0.0, 0.0),
                };
                self.last = Some((Instant::now(), cur));
                out
            }
            Err(e) => {
                tracing::debug!(error = %e, "no metrics this tick");
                (0.0, 0.0)
            }
        };

        let today_cu = self.store.spend_cu(today).await?;
        let today_usd = self.pricing.usd(today_cu);
        let projected = spend::projected(today_usd, recent_usd, recent_secs, now);

        if today_usd >= self.cfg.daily_budget_usd
            && !self.stopped.load(Ordering::SeqCst)
            && let Some(cmd) = &self.cfg.stop_command
        {
            match run(cmd).await {
                Ok(_) => {
                    tracing::warn!(today_usd, "spend budget reached: graph-node stopped");
                    self.stopped.store(true, Ordering::SeqCst);
                    self.store
                        .set_meta("spend_stopped_day", &today.to_string())
                        .await?;
                }
                Err(e) => {
                    tracing::error!(error = %e, "spend budget reached and the stop command failed")
                }
            }
        }

        let budget = Budget {
            daily_usd: self.cfg.daily_budget_usd,
            warn_ratio: self.cfg.warn_ratio,
        };
        let stopped = self.stopped.load(Ordering::SeqCst);
        let found = spend::findings(today_usd, &budget, projected, stopped, now);
        let top = self
            .store
            .spend_lines(today, 8)
            .await?
            .into_iter()
            .map(|(provider, method, requests, cu)| Line {
                provider,
                method,
                requests,
                usd: self.pricing.usd(cu),
            })
            .collect();
        let days = self
            .store
            .spend_days(today, 7)
            .await?
            .into_iter()
            .map(|(d, cu)| (d, self.pricing.usd(cu)))
            .collect();
        Ok((
            View {
                today_usd,
                budget_usd: self.cfg.daily_budget_usd,
                projected_usd: projected,
                stopped,
                billed_providers: self.cfg.billed_providers.clone(),
                top,
                days,
                counting_since: self.started,
            },
            found,
        ))
    }

    async fn resume_if_new_day(&mut self, today: NaiveDate) {
        let Ok(Some(day)) = self.store.meta("spend_stopped_day").await else {
            return;
        };
        if day == today.to_string() {
            return;
        }
        if let Some(cmd) = &self.cfg.resume_command {
            match run(cmd).await {
                Ok(_) => tracing::info!("new UTC day: graph-node started again"),
                Err(e) => {
                    tracing::error!(error = %e, "new UTC day and the resume command failed");
                    return;
                }
            }
        }
        self.stopped.store(false, Ordering::SeqCst);
        let _ = self.store.clear_meta("spend_stopped_day").await;
        // Counters restart with graph-node; start a fresh baseline.
        self.last = None;
    }
}
