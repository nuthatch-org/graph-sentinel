mod config;
mod findings;
mod meter;
mod outputs;
mod store;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};
use sentinel_checks::reo::{self, Report};
use sentinel_sources::graph_node::GraphNodeDb;
use sentinel_sources::index_node::IndexNode;
use sentinel_sources::network::NetworkSubgraph;
use sentinel_sources::rpc::Rpc;
use sentinel_sources::tap::TapDb;

use crate::config::Config;
use crate::meter::Meter;
use crate::outputs::{Metrics, Notifier};
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
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon.
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
    match cli.cmd {
        Cmd::Run => run(cfg).await,
        Cmd::Doctor => doctor(&cfg).await,
        Cmd::Status => status(&cfg, cli.json).await,
        Cmd::Reo { sub } => reo_cmd(&cfg, sub, cli.json).await,
    }
}

async fn open_meter(cfg: &Config) -> Result<Meter> {
    let store = Store::open(&cfg.outputs.state_db).await?;
    Meter::new(cfg, store).await
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
        None => {
            println!("FAIL  indexer db         set indexer_postgres_url (or postgres_url)");
            ok = false;
        }
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
    }

    ok &= line(
        "network subgraph",
        async {
            let n = NetworkSubgraph::new(
                cfg.sources.network_subgraph_url.clone(),
                cfg.sources.network_subgraph_token(),
            );
            let block = n.block_number().await?;
            let allocs = n.allocations(&cfg.sources.indexer_address).await?;
            Ok(format!(
                "block {block}, {} allocations for {}",
                allocs.len(),
                cfg.sources.indexer_address
            ))
        }
        .await,
    );

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

    if cfg.sources.rpc.contains_key(&cfg.reo.oracle_chain) {
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

async fn run(cfg: Config) -> Result<()> {
    let metrics = Arc::new(Metrics::new());
    if let Some(addr) = cfg.outputs.metrics_listen {
        let m = metrics.clone();
        tokio::spawn(async move {
            if let Err(e) = outputs::serve(addr, m).await {
                tracing::error!(error = %e, "metrics endpoint stopped");
            }
        });
    }
    let notifier = Notifier::new(&cfg.outputs.webhook);
    let meter = Arc::new(open_meter(&cfg).await?);
    let n = meter.backfill().await?;
    tracing::info!(
        new_rows = n,
        "backfilled the served ledger from the receipt and RAV tables"
    );

    let follower = meter.clone();
    tokio::spawn(async move { follower.follow().await });

    let mut scan = tokio::time::interval(cfg.reo.scan_interval);
    let mut judge = tokio::time::interval(cfg.reo.interval);
    loop {
        tokio::select! {
            _ = scan.tick() => {
                let r = meter.scan(2).await;
                metrics.source("indexer_db", r.is_ok());
                if let Err(e) = r { tracing::warn!(error = %e, "receipt scan failed"); }
            }
            _ = judge.tick() => {
                if let Err(e) = judge_reo(&meter, &metrics, &notifier).await {
                    tracing::warn!(error = %e, "REO evaluation failed");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                return Ok(());
            }
        }
    }
}

async fn judge_reo(meter: &Meter, metrics: &Metrics, notifier: &Notifier) -> Result<()> {
    let refreshed = meter.refresh_allocations_if_needed().await;
    metrics.source("network_subgraph", refreshed.is_ok());
    refreshed?;
    let report = meter.report().await?;
    metrics.reo(&report);
    let now = Utc::now();
    let prev = meter.store.findings(reo::CHECK).await?;
    let (stored, events) = findings::apply(prev, reo::findings(&report, now), now);
    meter.store.save_findings(&stored).await?;
    metrics.findings(&meter.store.active_findings().await?);
    notifier.send(&events).await;
    Ok(())
}
