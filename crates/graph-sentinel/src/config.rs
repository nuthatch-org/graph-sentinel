use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use figment::Figment;
use figment::providers::{Env, Format, Toml};
use sentinel_checks::reo::rules::{Rules, Step};
use sentinel_types::{Address, Severity};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub sources: Sources,
    #[serde(default)]
    pub reo: Reo,
    #[serde(default)]
    pub outputs: Outputs,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Sources {
    pub indexer_address: Address,
    /// graph-node's primary shard. Optional for the REO meter; used by candidates and liveness.
    pub postgres_url: Option<String>,
    /// The database tap-agent and indexer-service write to. Defaults to `postgres_url`.
    pub indexer_postgres_url: Option<String>,
    pub index_node_url: Option<String>,
    #[serde(default = "default_network_subgraph")]
    pub network_subgraph_url: String,
    /// Env var holding a bearer token for the network subgraph, if it needs one.
    pub network_subgraph_token_env: Option<String>,
    /// Chain name (as graph-node names it) → RPC URL.
    #[serde(default)]
    pub rpc: HashMap<String, String>,
}

fn default_network_subgraph() -> String {
    "https://gateway.thegraph.com/api/subgraphs/id/DZz4kDTdmzWLWsV373w2bSmoar3umKKH9y82SUKr5qmp"
        .into()
}

impl Sources {
    pub fn tap_db_url(&self) -> Option<&str> {
        self.indexer_postgres_url
            .as_deref()
            .or(self.postgres_url.as_deref())
    }

    pub fn network_subgraph_token(&self) -> Option<String> {
        self.network_subgraph_token_env
            .as_deref()
            .and_then(|v| std::env::var(v).ok())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Reo {
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    #[serde(with = "humantime_serde")]
    pub scan_interval: Duration,
    /// Filters the candidate ranking only. The oracle itself applies no signal filter.
    pub min_signal_grt: f64,
    pub oracle_address: Address,
    pub oracle_chain: String,
    pub min_online_days: u32,
    pub window_days: i64,
    pub eligibility_days: i64,
    pub criteria: Vec<Step>,
}

impl Default for Reo {
    fn default() -> Self {
        let r = Rules::default();
        Reo {
            interval: Duration::from_secs(3600),
            scan_interval: Duration::from_secs(120),
            min_signal_grt: 0.0,
            oracle_address: "0x02753BaE61C08AbD4351Bce7F48524935C2Cc78E"
                .parse()
                .expect("static"),
            oracle_chain: "arbitrum-one".into(),
            min_online_days: r.min_online_days,
            window_days: r.window_days,
            eligibility_days: r.eligibility_days,
            criteria: r.steps,
        }
    }
}

impl Reo {
    pub fn rules(&self) -> Rules {
        Rules {
            min_online_days: self.min_online_days,
            window_days: self.window_days,
            eligibility_days: self.eligibility_days,
            steps: self.criteria.clone(),
        }
        .normalised()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Outputs {
    pub metrics_listen: Option<SocketAddr>,
    pub state_db: PathBuf,
    pub webhook: Vec<Webhook>,
}

impl Default for Outputs {
    fn default() -> Self {
        Outputs {
            metrics_listen: Some(([0, 0, 0, 0], 9477).into()),
            state_db: "graph-sentinel.sqlite".into(),
            webhook: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Webhook {
    #[serde(default)]
    pub kind: WebhookKind,
    pub url: Option<String>,
    pub url_env: Option<String>,
    #[serde(default = "default_min_severity")]
    pub min_severity: Severity,
}

fn default_min_severity() -> Severity {
    Severity::Warn
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WebhookKind {
    #[default]
    Generic,
    Slack,
}

impl Webhook {
    pub fn resolved_url(&self) -> Option<String> {
        self.url
            .clone()
            .or_else(|| self.url_env.as_deref().and_then(|v| std::env::var(v).ok()))
    }
}

/// `graph-sentinel.toml`, overridden by `GRAPH_SENTINEL_SECTION__KEY` env vars, so secrets such as
/// `GRAPH_SENTINEL_SOURCES__POSTGRES_URL` never need to sit in the file.
pub fn load(path: &Path) -> Result<Config> {
    Figment::new()
        .merge(Toml::file(path))
        .merge(Env::prefixed("GRAPH_SENTINEL_").split("__"))
        .extract()
        .with_context(|| format!("loading {}", path.display()))
}

#[cfg(test)]
#[allow(clippy::result_large_err)] // figment::Jail's error type
mod tests {
    use super::*;

    #[test]
    fn minimal_config_is_an_address() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "graph-sentinel.toml",
                "[sources]\nindexer_address = \"0xb43B2CCCceadA5292732a8C58ae134AdEFcE09Bb\"\n",
            )?;
            jail.set_env("GRAPH_SENTINEL_SOURCES__POSTGRES_URL", "postgres://x@y/z");
            let c = load(Path::new("graph-sentinel.toml")).expect("config");
            assert_eq!(c.sources.tap_db_url(), Some("postgres://x@y/z"));
            assert_eq!(
                c.reo
                    .rules()
                    .subgraphs_per_day_at("2026-10-27".parse().unwrap()),
                10
            );
            assert_eq!(c.reo.scan_interval, Duration::from_secs(120));
            Ok(())
        });
    }

    #[test]
    fn criteria_and_durations_override() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "graph-sentinel.toml",
                r#"
                [sources]
                indexer_address = "0xb43B2CCCceadA5292732a8C58ae134AdEFcE09Bb"
                [reo]
                interval = "30m"
                criteria = [{ from = "2026-12-01", subgraphs_per_day = 20 }, { from = "2026-10-06", subgraphs_per_day = 5 }]
                "#,
            )?;
            let c = load(Path::new("graph-sentinel.toml")).expect("config");
            assert_eq!(c.reo.interval, Duration::from_secs(1800));
            let r = c.reo.rules();
            assert_eq!(r.subgraphs_per_day_at("2026-11-01".parse().unwrap()), 5);
            assert_eq!(r.subgraphs_per_day_at("2026-12-01".parse().unwrap()), 20);
            Ok(())
        });
    }
}
