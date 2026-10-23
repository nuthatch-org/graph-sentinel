mod config;
mod dashboard;
mod findings;
mod liveness;
mod meter;
mod notify;
mod outputs;
mod spend;
mod store;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use sentinel_checks::reo::{self, OnChain, Report};
use sentinel_sources::graph_node::GraphNodeDb;
use sentinel_sources::index_node::IndexNode;
use sentinel_sources::network::NetworkSubgraph;
use sentinel_sources::rpc::Rpc;
use sentinel_sources::tap::TapDb;
use tokio::sync::RwLock;

use crate::config::Config;
use crate::dashboard::Snapshot;
use crate::meter::Meter;
use crate::notify::Notifier;
use crate::outputs::Metrics;
use crate::store::Store;

#[derive(Parser)]
#[command(version, about = "Read-only sentinel for Graph indexers")]
struct Cli {
    #[arg(
        short,
        long,
        env = "GRAPH_SENTINEL_CONFIG",
        default_value = "graph-sentinel.toml",
        global = true
    )]
    config: PathBuf,
    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,
    /// With no command, run the daemon and its dashboard.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon and serve the dashboard (the default).
    Run,
    /// Test every configured source and say what is missing.
    Doctor,
    /// Every active finding.
    Status,
    /// REO eligibility: today's count, the projection, and what the chain says.
    Reo {
        #[command(subcommand)]
        sub: Option<ReoCmd>,
    },
}

#[derive(Subcommand)]
enum ReoCmd {
    /// Deployments to add, cheapest and likeliest to be queried first.
    Candidates {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Deployments served per day.
    History {
        #[arg(long, default_value_t = 28)]
        days: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "graph_sentinel=info,warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let cfg = config::load(&cli.config)?;
    match cli.cmd.unwrap_or(Cmd::Run) {
        Cmd::Run => run(cfg).await,
        Cmd::Doctor => doctor(&cfg).await,
        Cmd::Status => status(&cfg, cli.json).await,
        Cmd::Reo { sub } => reo_cmd(&cfg, sub, cli.json).await,
    }
}

async fn open_meter(cfg: &Config) -> Result<Meter> {
    let indexer = cfg
        .sources
        .indexer_address
        .clone()
        .context("the REO meter needs [sources] indexer_address")?;
    let store = Store::open(&cfg.outputs.state_db).await?;
    Meter::new(cfg, store, indexer).await
}

async fn reo_cmd(cfg: &Config, sub: Option<ReoCmd>, json: bool) -> Result<()> {
    let m = open_meter(cfg).await?;
    // A one-shot invocation still records what the tables hold, so it is useful on day one.
    m.backfill().await?;
    m.refresh_allocations_if_needed().await?;
    match sub {
        None => {
            let r = m.report().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print_report(&r);
            }
        }
        Some(ReoCmd::Candidates { limit }) => {
            let c = m.candidates(limit).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&c)?);
            } else {
                println!(
                    "{:<48} {:<14} {:>10} {:>12} {:>6}  local  chain",
                    "deployment", "network", "signal", "fees (GRT)", "allocs"
                );
                for c in c {
                    println!(
                        "{:<48} {:<14} {:>10.0} {:>12.0} {:>6}  {:<5}  {}",
                        c.deployment.as_str(),
                        c.network.as_deref().unwrap_or("?"),
                        c.signal_grt,
                        c.query_fees_grt,
                        c.active_allocations,
                        if c.synced_locally { "yes" } else { "-" },
                        if c.chain_supported { "yes" } else { "-" },
                    );
                }
            }
        }
        Some(ReoCmd::History { days }) => {
            let h = m.store.history(days, Utc::now().date_naive()).await?;
            if json {
                let v: Vec<_> = h
                    .iter()
                    .map(|(d, deps)| serde_json::json!({ "day": d, "count": deps.len(), "deployments": deps }))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                for (d, deps) in h {
                    println!("{d}  {:>3}", deps.len());
                }
            }
        }
    }
    Ok(())
}

fn print_report(r: &Report) {
    println!(
        "today ({})          {} deployments served so far",
        r.today, r.today_count
    );
    println!("typical day (median of 7)   {}", r.typical_count);
    println!(
        "minimum now                 {} per day; {} qualifying days in the window (need {})",
        r.subgraphs_per_day_now, r.qualifying_days_now, r.min_online_days
    );
    if let (Some(s), Some(q)) = (&r.next_step, r.qualifying_days_next) {
        println!(
            "from {}              {} per day; today's window would hold {} qualifying days; short by {} per day",
            s.from, s.subgraphs_per_day, q, r.shortfall_next
        );
    }
    let p = &r.if_traffic_holds;
    match (p.first_failing_run, p.eligible_until) {
        (None, Some(u)) => println!("if traffic holds            eligible through {u} at least"),
        (Some(f), Some(u)) => {
            println!("if traffic holds            run on {f} fails; covered until {u}")
        }
        (Some(f), None) => {
            println!("if traffic holds            run on {f} fails; no renewal in view")
        }
        (None, None) => {}
    }
    if let Some(f) = r.if_traffic_stops.first_failing_run {
        println!("if traffic stops            run on {f} fails");
    }
    if let Some(c) = &r.on_chain {
        let renewed = if c.renewal_time.timestamp() == 0 {
            "never".to_string()
        } else {
            c.renewal_time.to_rfc3339()
        };
        println!(
            "on chain                    eligible = {}; last renewed {renewed}; oracle last posted {}",
            c.eligible, c.last_oracle_update
        );
    }
    match r.observed_since {
        Some(d) => {
            println!("\nreceipts method: an upper bound on the oracle's count. Ledger starts {d}.")
        }
        None => println!("\nno receipts seen yet; the ledger fills as tap-agent stores them."),
    }
}

async fn status(cfg: &Config, json: bool) -> Result<()> {
    let store = Store::open(&cfg.outputs.state_db).await?;
    let mut f = store.active_findings().await?;
    f.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.check.cmp(&b.check)));
    if json {
        println!("{}", serde_json::to_string_pretty(&f)?);
    } else if f.is_empty() {
        println!("no active findings");
    } else {
        for f in f {
            println!(
                "{:<8} {:<6} {:<16} {}",
                f.severity, f.check, f.kind, f.summary
            );
        }
    }
    Ok(())
}

async fn doctor(cfg: &Config) -> Result<()> {
    fn line(name: &str, r: Result<String>) -> bool {
        match r {
            Ok(s) => {
                println!("ok    {name:<18} {s}");
                true
            }
            Err(e) => {
                println!("FAIL  {name:<18} {e:#}");
                false
            }
        }
    }
    let mut ok = true;

    if let Some(u) = &cfg.sources.postgres_url {
        ok &= line(
            "graph-node db",
            async {
                let g = GraphNodeDb::connect(u).await?;
                let layout = g.layout().await?;
                let chains = g.chains().await?;
                let deps = g.deployments().await?;
                Ok(format!(
                    "{layout:?} layout, {} chains, {} deployments",
                    chains.len(),
                    deps.len()
                ))
            }
            .await,
        );
    } else {
        println!(
            "skip  graph-node db      postgres_url not set (candidates lose the local columns)"
        );
    }

    match cfg.sources.tap_db_url() {
        Some(u) => ok &= line("indexer db", async {
            let t = TapDb::connect(u).await?.tables().await?;
            if !t.horizon_receipts && !t.legacy_receipts {
                anyhow::bail!("no tap_horizon_receipts or scalar_tap_receipts: wrong database, or missing grants");
            }
            Ok(format!(
                "horizon receipts {}, ravs {}; legacy receipts {}, ravs {}",
                t.horizon_receipts, t.horizon_ravs, t.legacy_receipts, t.legacy_ravs
            ))
        }.await),
        None => println!("skip  indexer db         no indexer_postgres_url: the REO meter is off"),
    }

    if let Some(u) = &cfg.sources.index_node_url {
        ok &= line(
            "index-node",
            async {
                Ok(format!(
                    "{} deployments",
                    IndexNode::new(u.clone()).deployment_count().await?
                ))
            }
            .await,
        );
        ok &= line(
            "ipfs",
            async {
                let i = sentinel_sources::ipfs::Ipfs::new(cfg.sources.ipfs_url.clone());
                let first = IndexNode::new(u.clone())
                    .statuses()
                    .await?
                    .into_iter()
                    .next();
                match first {
                    Some(st) => {
                        let a = sentinel_checks::liveness::manifest::parse(
                            &i.cat(&st.deployment).await?,
                        )
                        .map_err(|e| anyhow::anyhow!(e))?;
                        Ok(format!(
                            "manifest of {} read: {} addresses, {} topics",
                            &st.deployment[..12],
                            a.addresses.len(),
                            a.topics.len()
                        ))
                    }
                    None => Ok("no deployments to read a manifest for".into()),
                }
            }
            .await,
        );
    } else {
        println!("skip  index-node         no index_node_url: liveness is off");
    }

    if let Some(indexer) = &cfg.sources.indexer_address {
        ok &= line(
            "network subgraph",
            async {
                let n = NetworkSubgraph::new(
                    cfg.sources.network_subgraph_url.clone(),
                    cfg.sources.network_subgraph_token(),
                );
                let block = n.block_number().await?;
                let allocs = n.allocations(indexer).await?;
                Ok(format!(
                    "block {block}, {} allocations for {indexer}",
                    allocs.len()
                ))
            }
            .await,
        );
    }

    for (chain, url) in &cfg.sources.rpc {
        ok &= line(
            &format!("rpc {chain}"),
            async {
                let r = Rpc::new(url.clone());
                Ok(format!(
                    "chain id {}, head {}",
                    r.chain_id().await?,
                    r.block_number().await?
                ))
            }
            .await,
        );
    }

    if cfg.reo_enabled() && cfg.sources.rpc.contains_key(&cfg.reo.oracle_chain) {
        ok &= line(
            "reo oracle",
            async {
                let m = open_meter(cfg).await?;
                let c = m.on_chain().await?.expect("rpc present");
                Ok(format!(
                    "eligible = {}, oracle last posted {}",
                    c.eligible, c.last_oracle_update
                ))
            }
            .await,
        );
    } else {
        println!(
            "skip  reo oracle         no [sources.rpc] entry for {:?}",
            cfg.reo.oracle_chain
        );
    }

    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

const CANDIDATES_EVERY: Duration = Duration::from_secs(6 * 3600);
const ORACLE_EVERY: Duration = Duration::from_secs(600);

/// What every module task needs to publish its results.
#[derive(Clone)]
struct Shared {
    store: Store,
    metrics: Arc<Metrics>,
    notifier: Arc<Notifier>,
    snapshot: Arc<RwLock<Snapshot>>,
}

impl Shared {
    async fn source(&self, name: &str, r: &Result<String, String>) {
        self.metrics.source(&name.replace(' ', "_"), r.is_ok());
        self.snapshot.write().await.set_source(name, r);
    }

    /// Merge one check's findings, notify on transitions, refresh the page's list.
    async fn findings(&self, check: &str, current: Vec<sentinel_types::Finding>) -> Result<()> {
        let now = Utc::now();
        let prev = self.store.findings(check).await?;
        let (stored, events) = findings::apply(prev, current, now);
        self.store.save_findings(&stored).await?;
        self.notifier.send(&events).await;
        self.publish().await
    }

    async fn publish(&self) -> Result<()> {
        let active = self.store.active_findings().await?;
        self.metrics.findings(&active);
        let mut snap = self.snapshot.write().await;
        for (name, r) in self.notifier.statuses() {
            snap.set_source(&name, &r);
        }
        snap.findings = active;
        snap.generated_at = Some(Utc::now());
        Ok(())
    }
}

async fn run(cfg: Config) -> Result<()> {
    if !cfg.reo_enabled() && !cfg.liveness_enabled() && !cfg.spend_enabled() {
        anyhow::bail!(
            "nothing to watch: set [sources] index_node_url for liveness, \
             or indexer_address and indexer_postgres_url for the REO meter"
        );
    }
    let store = Store::open(&cfg.outputs.state_db).await?;
    let notifier = Arc::new(Notifier::new(
        &cfg.outputs.notify,
        cfg.display_name(),
        cfg.outputs.public_url.clone(),
    ));
    let shared = Shared {
        store: store.clone(),
        metrics: Arc::new(Metrics::new()),
        notifier: notifier.clone(),
        snapshot: Arc::new(RwLock::new(Snapshot {
            version: env!("CARGO_PKG_VERSION"),
            indexer: cfg.display_name(),
            steps: cfg.reo.rules().steps,
            graph_node: cfg.sources.postgres_url.is_some(),
            ..Snapshot::default()
        })),
    };
    if let Some(addr) = cfg.outputs.listen {
        let (s, m) = (shared.snapshot.clone(), shared.metrics.clone());
        tokio::spawn(async move {
            if let Err(e) = dashboard::serve(addr, s, m).await {
                tracing::error!(error = %e, "dashboard stopped");
            }
        });
    }

    let mut modules = Vec::new();
    if cfg.reo_enabled() {
        let meter = Arc::new(open_meter(&cfg).await?);
        let n = meter.backfill().await?;
        tracing::info!(
            new_rows = n,
            "backfilled the served ledger from the receipt and RAV tables"
        );
        modules.push("REO eligibility");
        tokio::spawn(reo_loop(meter, cfg.clone(), shared.clone()));
    }
    let mut guard_stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(sc) = cfg.spend.clone() {
        let g = spend::Guard::new(sc.clone(), store.clone()).await?;
        guard_stopped = g.stopped.clone();
        modules.push("RPC spend");
        tokio::spawn(spend_loop(g, sc.interval, shared.clone()));
    }
    if cfg.liveness_enabled()
        && let Some(l) = liveness::Liveness::new(&cfg, store.clone())
    {
        modules.push("deployment liveness");
        tokio::spawn(liveness_loop(
            Arc::new(l),
            cfg.liveness.interval,
            shared.clone(),
            guard_stopped,
        ));
    }
    shared.snapshot.write().await.modules = modules.iter().map(|m| m.to_string()).collect();
    if notifier.is_empty() {
        tracing::warn!("no [[outputs.notify]] channel: findings show on the dashboard only");
    } else if cfg.outputs.announce_start {
        notifier.announce(&modules).await;
        let _ = shared.publish().await;
    }

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    Ok(())
}

async fn spend_loop(mut g: spend::Guard, every: Duration, sh: Shared) {
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        match g.tick().await {
            Ok((view, found)) => {
                sh.snapshot.write().await.spend = Some(view);
                if let Err(e) = sh.findings(sentinel_checks::spend::CHECK, found).await {
                    tracing::warn!(error = %e, "recording spend findings failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "spend tick failed"),
        }
    }
}

async fn liveness_loop(
    l: Arc<liveness::Liveness>,
    every: Duration,
    sh: Shared,
    guard_stopped: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut tick = tokio::time::interval(every);
    let mut failures = 0u32;
    loop {
        tick.tick().await;
        let found = match l.tick().await {
            Ok((mut rows, found)) => {
                failures = 0;
                sh.source(
                    "index-node",
                    &Ok(format!("{} deployments read", rows.len())),
                )
                .await;
                rows.sort_by_key(|r| (r.verdict.rank(), std::cmp::Reverse(r.behind.unwrap_or(0))));
                let mut snap = sh.snapshot.write().await;
                snap.deployments = Some(rows);
                snap.liveness_at = Some(Utc::now());
                found
            }
            Err(e) => {
                failures += 1;
                if guard_stopped.load(std::sync::atomic::Ordering::SeqCst) {
                    // Down by the spend guard's hand: say so, alert nothing new.
                    sh.source(
                        "index-node",
                        &Err("stopped by the spend guard until midnight UTC".into()),
                    )
                    .await;
                    let _ = sh.publish().await;
                    continue;
                }
                let msg = format!("{e:#}");
                tracing::warn!(error = %msg, "liveness tick failed");
                sh.source("index-node", &Err(msg.clone())).await;
                // One failed read is a blip; two in a row is graph-node gone.
                if failures < 2 {
                    let _ = sh.publish().await;
                    continue;
                }
                // Hold what was open: an outage says nothing about whether those cleared.
                let mut held: Vec<sentinel_types::Finding> = sh
                    .store
                    .findings(sentinel_checks::liveness::CHECK)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|s| s.active && s.finding.kind != "index_node_down")
                    .map(|s| s.finding)
                    .collect();
                held.push(sentinel_types::Finding::new(
                    sentinel_checks::liveness::CHECK,
                    "index_node_down",
                    "index-node",
                    sentinel_types::Severity::Critical,
                    format!(
                        "graph-node's index-node has not answered for {} checks. Nothing is being indexed or watched: {}",
                        failures,
                        msg.chars().take(160).collect::<String>()
                    ),
                    serde_json::json!({ "error": msg, "consecutive_failures": failures }),
                    Utc::now(),
                ));
                held
            }
        };
        if let Err(e) = sh.findings(sentinel_checks::liveness::CHECK, found).await {
            tracing::warn!(error = %e, "recording liveness findings failed");
        }
    }
}

async fn reo_loop(meter: Arc<Meter>, cfg: Config, sh: Shared) {
    let follower = meter.clone();
    tokio::spawn(async move { follower.follow().await });

    // Candidates are a few seconds of network subgraph paging: refresh them rarely, apart.
    let (m, s) = (meter.clone(), sh.snapshot.clone());
    tokio::spawn(async move {
        let mut every = tokio::time::interval(CANDIDATES_EVERY);
        loop {
            every.tick().await;
            match m.candidates(15).await {
                Ok(c) => {
                    let mut snap = s.write().await;
                    snap.candidates = Some(c);
                    snap.candidates_at = Some(Utc::now());
                }
                Err(e) => tracing::warn!(error = %e, "ranking candidates failed"),
            }
        }
    });

    let mut oracle: Option<(Instant, Option<OnChain>)> = None;
    let mut scan = tokio::time::interval(cfg.reo.scan_interval);
    let mut judge = tokio::time::interval(cfg.reo.interval);
    loop {
        let evaluate = tokio::select! {
            _ = scan.tick() => false,
            _ = judge.tick() => true,
        };
        let scanned = meter.scan(2).await;
        sh.source(
            "indexer db",
            &scanned
                .as_ref()
                .map(|n| format!("receipts and RAVs scanned, {n} new"))
                .map_err(|e| format!("{e:#}")),
        )
        .await;
        if let Err(e) = refresh(&meter, &sh, &mut oracle, evaluate).await {
            tracing::warn!(error = %e, "REO refresh failed");
            sh.snapshot.write().await.reo_error = Some(format!("{e:#}"));
        }
    }
}

/// Rebuild the REO report and the page. Findings and notifications only on `evaluate` ticks.
async fn refresh(
    meter: &Meter,
    sh: &Shared,
    oracle: &mut Option<(Instant, Option<OnChain>)>,
    evaluate: bool,
) -> Result<()> {
    let mapped = meter.refresh_allocations_if_needed().await;
    sh.source(
        "network subgraph",
        &mapped
            .as_ref()
            .map(|_| "allocations mapped".to_string())
            .map_err(|e| format!("{e:#}")),
    )
    .await;

    if meter.has_oracle()
        && oracle
            .as_ref()
            .is_none_or(|(t, _)| t.elapsed() > ORACLE_EVERY)
    {
        let r = meter.on_chain().await;
        sh.source(
            "reo oracle",
            &r.as_ref()
                .map(|c| match c {
                    Some(c) => format!(
                        "last posted {}",
                        c.last_oracle_update.format("%Y-%m-%d %H:%M UTC")
                    ),
                    None => String::new(),
                })
                .map_err(|e| format!("{e:#}")),
        )
        .await;
        // Keep the last good reading through a failed one.
        if let Ok(c) = r {
            *oracle = Some((Instant::now(), c));
        }
    }
    let on_chain = oracle.as_ref().and_then(|(_, c)| c.clone());

    let counts = meter.counts().await?;
    let report = meter.report_with(&counts, on_chain).await?;
    sh.metrics.reo(&report);
    let found = evaluate.then(|| reo::findings(&report, Utc::now()));
    {
        let mut snap = sh.snapshot.write().await;
        snap.days = dashboard::window_days(&counts, report.today, meter.rules().window_days as u64);
        snap.reo = Some(report);
        snap.reo_error = None;
    }
    match found {
        Some(f) => sh.findings(reo::CHECK, f).await,
        None => sh.publish().await,
    }
}
