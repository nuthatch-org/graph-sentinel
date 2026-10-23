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
    pub liveness: Liveness,
    /// RPC spend guard; on when `graph_node_metrics_url` is set.
    pub spend: Option<Spend>,
    #[serde(default)]
    pub network: Network,
    #[serde(default)]
    pub outputs: Outputs,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Sources {
    /// Needed for the REO meter. A graph-node without an indexer (a fisherman's rig) leaves it out.
    pub indexer_address: Option<Address>,
    /// graph-node's primary shard. Optional for the REO meter; used by candidates and liveness.
    pub postgres_url: Option<String>,
    /// The database tap-agent and indexer-service write to. Defaults to `postgres_url`.
    pub indexer_postgres_url: Option<String>,
    /// Enables liveness.
    pub index_node_url: Option<String>,
    #[serde(default = "default_ipfs")]
    pub ipfs_url: String,
    #[serde(default = "default_network_subgraph")]
    pub network_subgraph_url: String,
    /// Env var holding a bearer token for the network subgraph, if it needs one.
    pub network_subgraph_token_env: Option<String>,
    /// Take the URL from another tool's TOML, `path#dotted.key`, read in place, e.g.
    /// `/home/pepe/osprey/osprey.toml#network_subgraph.url`. Wins over `network_subgraph_url`.
    pub network_subgraph_url_from_toml: Option<String>,
    /// Chain name (as graph-node names it) → RPC URL.
    #[serde(default)]
    pub rpc: HashMap<String, String>,
}

fn default_ipfs() -> String {
    "https://ipfs.thegraph.com".into()
}

fn default_network_subgraph() -> String {
    "https://gateway.thegraph.com/api/subgraphs/id/DZz4kDTdmzWLWsV373w2bSmoar3umKKH9y82SUKr5qmp"
        .into()
}

impl Config {
    pub fn reo_enabled(&self) -> bool {
        self.sources.indexer_address.is_some() && self.sources.tap_db_url().is_some()
    }

    pub fn network_enabled(&self) -> bool {
        self.network.enabled
            && (self.sources.indexer_address.is_some() || self.network.fisherman_address.is_some())
    }

    pub fn spend_enabled(&self) -> bool {
        self.spend.is_some()
    }

    pub fn liveness_enabled(&self) -> bool {
        self.liveness.enabled && self.sources.index_node_url.is_some()
    }

    pub fn display_name(&self) -> String {
        self.outputs
            .name
            .clone()
            .or_else(|| self.sources.indexer_address.as_ref().map(|a| a.to_string()))
            .unwrap_or_else(|| "graph-node".into())
    }
}

impl Sources {
    pub fn tap_db_url(&self) -> Option<&str> {
        self.indexer_postgres_url
            .as_deref()
            .or(self.postgres_url.as_deref())
    }

    /// The network subgraph URL, resolving `network_subgraph_url_from_toml` when set.
    pub fn network_subgraph(&self) -> String {
        self.network_subgraph_url_from_toml
            .as_deref()
            .and_then(|spec| {
                let (path, key) = spec.split_once('#')?;
                match Figment::from(Toml::file(path)).extract_inner::<String>(key) {
                    Ok(u) => Some(u),
                    Err(e) => {
                        tracing::warn!(%path, %key, error = %e, "network_subgraph_url_from_toml unreadable");
                        None
                    }
                }
            })
            .unwrap_or_else(|| self.network_subgraph_url.clone())
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
pub struct Liveness {
    pub enabled: bool,
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    pub lag_tolerance_blocks: u64,
    pub activity_window_blocks: u64,
    pub idle_patience: u32,
    pub chain_patience: u32,
    /// Run `restart_command` on a deployment frozen two ticks running. Off by default.
    pub restart: bool,
    /// `{deployment}` is replaced with the IPFS hash.
    pub restart_command: String,
    #[serde(with = "humantime_serde")]
    pub restart_cooldown: Duration,
}

impl Default for Liveness {
    fn default() -> Self {
        let c = sentinel_checks::liveness::Config::default();
        Liveness {
            enabled: true,
            interval: Duration::from_secs(300),
            lag_tolerance_blocks: c.lag_tolerance_blocks,
            activity_window_blocks: c.activity_window_blocks,
            idle_patience: c.idle_patience,
            chain_patience: c.chain_patience,
            restart: false,
            restart_command: "graphman --config /etc/graph-node/config.toml restart {deployment}"
                .into(),
            restart_cooldown: Duration::from_secs(1800),
        }
    }
}

impl Liveness {
    pub fn check_config(&self) -> sentinel_checks::liveness::Config {
        sentinel_checks::liveness::Config {
            lag_tolerance_blocks: self.lag_tolerance_blocks,
            activity_window_blocks: self.activity_window_blocks,
            idle_patience: self.idle_patience,
            chain_patience: self.chain_patience,
        }
    }
}

/// What the network says about this indexer, and about disputes this fisherman filed.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Network {
    pub enabled: bool,
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// Watch disputes filed from this address.
    pub fisherman_address: Option<Address>,
    /// Public price and per-chain lag; the per-indexer routes need a subscription.
    pub lodestar_url: String,
}

impl Default for Network {
    fn default() -> Self {
        Network {
            enabled: true,
            interval: Duration::from_secs(600),
            fisherman_address: None,
            lodestar_url: "https://api.lodestar-dashboard.com".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Spend {
    /// graph-node's Prometheus endpoint (port 8040 inside the container).
    pub graph_node_metrics_url: String,
    pub daily_budget_usd: f64,
    #[serde(default = "default_warn_ratio")]
    pub warn_ratio: f64,
    #[serde(default = "default_billed")]
    pub billed_providers: Vec<String>,
    #[serde(default = "default_cu_price")]
    pub usd_per_million_cu: f64,
    #[serde(default = "default_spend_interval", with = "humantime_serde")]
    pub interval: Duration,
    /// Run once when the day's budget is reached, e.g. `docker compose stop graph-node`.
    pub stop_command: Option<String>,
    /// Run after midnight UTC if the guard stopped graph-node.
    pub resume_command: Option<String>,
}

fn default_warn_ratio() -> f64 {
    0.7
}
fn default_billed() -> Vec<String> {
    vec!["alchemy".into()]
}
fn default_cu_price() -> f64 {
    0.45
}
fn default_spend_interval() -> Duration {
    Duration::from_secs(60)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Outputs {
    /// What this sentinel watches, in messages and the page header. Defaults to the indexer.
    pub name: Option<String>,
    /// Dashboard, `/api/snapshot` and `/metrics`. Loopback by default: the page has no login.
    #[serde(alias = "metrics_listen")]
    pub listen: Option<SocketAddr>,
    /// Where the indexer reaches the dashboard, linked from every notification.
    pub public_url: Option<String>,
    pub state_db: PathBuf,
    /// Post a short "now watching" message to every channel at start-up.
    pub announce_start: bool,
    #[serde(alias = "webhook")]
    pub notify: Vec<Channel>,
}

impl Default for Outputs {
    fn default() -> Self {
        Outputs {
            name: None,
            listen: Some(([127, 0, 0, 1], 9477).into()),
            public_url: None,
            state_db: "graph-sentinel.sqlite".into(),
            announce_start: true,
            notify: Vec::new(),
        }
    }
}

/// One place notifications go. Secrets can come from `*_env` instead of the file.
#[derive(Debug, Clone, Deserialize)]
pub struct Channel {
    pub kind: ChannelKind,
    /// Discord, Slack or generic webhook URL.
    pub url: Option<String>,
    pub url_env: Option<String>,
    /// Telegram.
    pub bot_token: Option<String>,
    pub bot_token_env: Option<String>,
    /// A file holding only the token, which keeps it out of the config and the environment.
    pub bot_token_file: Option<PathBuf>,
    pub chat_id: Option<String>,
    #[serde(default = "default_telegram_api")]
    pub telegram_api: String,
    #[serde(default = "default_min_severity")]
    pub min_severity: Severity,
}

fn default_telegram_api() -> String {
    "https://api.telegram.org".into()
}

fn default_min_severity() -> Severity {
    Severity::Warn
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    Telegram,
    Discord,
    Slack,
    Generic,
}

impl ChannelKind {
    pub fn name(self) -> &'static str {
        match self {
            ChannelKind::Telegram => "telegram",
            ChannelKind::Discord => "discord",
            ChannelKind::Slack => "slack",
            ChannelKind::Generic => "webhook",
        }
    }
}

fn from_env(v: &Option<String>, env: &Option<String>) -> Option<String> {
    v.clone()
        .or_else(|| env.as_deref().and_then(|e| std::env::var(e).ok()))
        .filter(|s| !s.is_empty())
}

impl Channel {
    pub fn resolved_url(&self) -> Option<String> {
        from_env(&self.url, &self.url_env)
    }

    pub fn resolved_bot_token(&self) -> Option<String> {
        from_env(&self.bot_token, &self.bot_token_env).or_else(|| {
            self.bot_token_file
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
        })
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
    fn notify_channels_parse_with_the_old_key() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "graph-sentinel.toml",
                r#"
                [sources]
                indexer_address = "0xb43B2CCCceadA5292732a8C58ae134AdEFcE09Bb"
                [[outputs.webhook]]
                kind = "telegram"
                bot_token_env = "TG_TOKEN"
                chat_id = "-1001"
                [[outputs.webhook]]
                kind = "discord"
                url = "https://discord.example/hook"
                min_severity = "critical"
                "#,
            )?;
            jail.set_env("TG_TOKEN", "123:abc");
            let c = load(Path::new("graph-sentinel.toml")).expect("config");
            assert_eq!(c.outputs.notify.len(), 2);
            assert_eq!(
                c.outputs.notify[0].resolved_bot_token().as_deref(),
                Some("123:abc")
            );
            assert_eq!(c.outputs.notify[1].min_severity, Severity::Critical);
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
