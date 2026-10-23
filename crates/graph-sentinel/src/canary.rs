//! The payment-path canary as a service.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use figment::Figment;
use figment::providers::{Format, Toml};
use sentinel_checks::tap::{self, Lint, Probe, Reading, Rules, Unaggregated, norm};
use sentinel_sources::aggregator;
use sentinel_sources::rpc::{Rpc, escrow_balance};
use sentinel_sources::tap::{Tables, TapDb};
use sentinel_types::{Address, Finding};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::store::Store;

/// The parts of indexer-rs's config.toml the canary reads. Everything else is ignored.
#[derive(Debug, Default, Deserialize)]
struct IndexerRs {
    #[serde(default)]
    tap: IrTap,
    #[serde(default)]
    blockchain: IrChain,
}

#[derive(Debug, Default, Deserialize)]
struct IrTap {
    max_amount_willing_to_lose_grt: Option<serde_json::Value>,
    #[serde(default)]
    sender_aggregator_endpoints: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct IrChain {
    chain_id: Option<u64>,
    receipts_verifier_address_v2: Option<String>,
}

fn read_indexer_rs(path: &Path) -> Result<(Lint, BTreeMap<String, String>)> {
    let c: IndexerRs = Figment::from(Toml::file(path)).extract()?;
    let max = c.tap.max_amount_willing_to_lose_grt.and_then(|v| match v {
        serde_json::Value::String(s) => s.parse().ok(),
        v => v.as_f64(),
    });
    let endpoints: BTreeMap<String, String> = c
        .tap
        .sender_aggregator_endpoints
        .into_iter()
        .map(|(k, v)| (norm(&k), v))
        .collect();
    Ok((
        Lint {
            chain_id: c.blockchain.chain_id,
            collector: c.blockchain.receipts_verifier_address_v2,
            max_willing_to_lose_grt: max,
            senders: endpoints.keys().cloned().collect(),
        },
        endpoints,
    ))
}

/// One sender as the page shows it.
#[derive(Debug, Clone, Serialize)]
pub struct SenderRow {
    pub sender: String,
    pub endpoint: Option<String>,
    pub aggregator_ok: Option<bool>,
    pub aggregator_detail: Option<String>,
    pub configured: bool,
    pub recent: bool,
    pub denylisted: bool,
    pub unaggregated_grt: f64,
    pub oldest_receipt_secs: Option<i64>,
    pub fees_24h_grt: Option<f64>,
    pub escrow_grt: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct View {
    pub senders: Vec<SenderRow>,
    pub limit_grt: Option<f64>,
    pub config_read: bool,
    pub config_error: Option<String>,
}

pub struct Canary {
    tap: TapDb,
    tables: Tables,
    http: reqwest::Client,
    store: Store,
    cfg: crate::config::Tap,
    rpc: Option<Rpc>,
    indexer: Option<Address>,
    failures: HashMap<String, u32>,
    rules: Rules,
}

impl Canary {
    pub async fn new(cfg: &Config, store: Store) -> Result<Option<Self>> {
        let Some(url) = cfg.sources.tap_db_url() else {
            return Ok(None);
        };
        let tap = TapDb::connect(url).await?;
        let tables = tap.tables().await?;
        Ok(Some(Canary {
            tap,
            tables,
            http: aggregator::client(),
            store,
            cfg: cfg.tap.clone(),
            rpc: cfg
                .sources
                .rpc
                .get("arbitrum-one")
                .map(|u| Rpc::new(u.clone())),
            indexer: cfg.sources.indexer_address.clone(),
            failures: HashMap::new(),
            rules: Rules {
                rav_stall_secs: cfg.tap.rav_stall.as_secs() as i64,
                // "Still flowing" scales with the stall window, so short demo settings stay coherent.
                flowing_secs: (cfg.tap.rav_stall.as_secs() as i64 / 2).max(30),
                fees_warn_ratio: cfg.tap.fees_warn_ratio,
                fees_critical_ratio: cfg.tap.fees_critical_ratio,
                ..Rules::default()
            },
        }))
    }

    pub async fn tick(&mut self) -> Result<(View, Vec<Finding>)> {
        let now = Utc::now();
        let now_ns = now.timestamp_nanos_opt().unwrap_or(0);

        let (lint, endpoints, config_error) = match &self.cfg.indexer_rs_config {
            Some(p) => match read_indexer_rs(p) {
                Ok((l, e)) => (l, e, None),
                Err(e) => (Lint::default(), BTreeMap::new(), Some(format!("{e:#}"))),
            },
            None => (Lint::default(), BTreeMap::new(), None),
        };
        let config_read = self.cfg.indexer_rs_config.is_some() && config_error.is_none();

        let mut probes = Vec::new();
        for (sender, endpoint) in &endpoints {
            let a = aggregator::probe(&self.http, endpoint).await;
            let f = self.failures.entry(sender.clone()).or_default();
            *f = if a.ok { 0 } else { *f + 1 };
            probes.push(Probe {
                sender: sender.clone(),
                endpoint: endpoint.clone(),
                ok: a.ok,
                detail: a.detail,
                failures: *f,
            });
        }

        let day_ago = now_ns - 86_400 * 1_000_000_000;
        let recent_payers: BTreeSet<String> = self
            .tap
            .recent_payers(&self.tables, day_ago)
            .await?
            .iter()
            .map(|p| norm(p))
            .collect();
        let denylist: BTreeSet<String> =
            self.tap.denylist().await?.iter().map(|p| norm(p)).collect();
        let unaggregated: Vec<Unaggregated> = self
            .tap
            .unaggregated(&self.tables)
            .await?
            .into_iter()
            .map(|(p, n, grt, lo, hi)| Unaggregated {
                payer: norm(&p),
                receipts: n as u64,
                grt,
                oldest_ns: lo,
                newest_ns: hi,
            })
            .collect();
        let totals: Vec<(String, String, f64)> = self
            .tap
            .rav_totals(&self.tables)
            .await?
            .into_iter()
            .map(|(p, c, g)| (norm(&p), c, g))
            .collect();
        let fees_24h: BTreeMap<String, f64> = self
            .store
            .rav_fees_24h(now.timestamp(), &totals)
            .await?
            .into_iter()
            .collect();

        let mut escrow = BTreeMap::new();
        if let (Some(rpc), Some(me)) = (&self.rpc, &self.indexer) {
            let collector: Address = self
                .cfg
                .collector_address
                .clone()
                .or_else(|| lint.collector.as_deref().and_then(|c| c.parse().ok()))
                .unwrap_or_else(|| tap::ARBITRUM_COLLECTOR.parse().expect("static"));
            for p in &recent_payers {
                let Ok(payer) = p.parse::<Address>() else {
                    continue;
                };
                match escrow_balance(rpc, &self.cfg.escrow_address, &payer, &collector, me).await {
                    Ok(b) => {
                        escrow.insert(p.clone(), b);
                    }
                    Err(e) => tracing::debug!(error = %e, payer = %p, "escrow read failed"),
                }
            }
        }

        let reading = Reading {
            probes: probes.clone(),
            recent_payers: recent_payers.clone(),
            denylist: denylist.clone(),
            unaggregated: unaggregated.clone(),
            escrow: escrow.clone(),
            fees_24h: fees_24h.clone(),
            lint: lint.clone(),
        };
        let found = tap::findings(&reading, &self.rules, now);

        let mut all: BTreeSet<String> = recent_payers.clone();
        all.extend(lint.senders.iter().cloned());
        all.extend(denylist.iter().cloned());
        all.extend(unaggregated.iter().map(|u| u.payer.clone()));
        let senders = all
            .into_iter()
            .map(|s| {
                let probe = probes.iter().find(|p| p.sender == s);
                let u = unaggregated.iter().find(|u| u.payer == s);
                SenderRow {
                    endpoint: endpoints.get(&s).cloned(),
                    aggregator_ok: probe.map(|p| p.ok),
                    aggregator_detail: probe.map(|p| p.detail.clone()),
                    configured: lint.senders.contains(&s),
                    recent: recent_payers.contains(&s),
                    denylisted: denylist.contains(&s),
                    unaggregated_grt: u.map_or(0.0, |u| u.grt),
                    oldest_receipt_secs: u.map(|u| (now_ns - u.oldest_ns) / 1_000_000_000),
                    fees_24h_grt: fees_24h.get(&s).copied(),
                    escrow_grt: escrow.get(&s).copied(),
                    sender: s,
                }
            })
            .collect();
        Ok((
            View {
                senders,
                limit_grt: lint.max_willing_to_lose_grt,
                config_read,
                config_error,
            },
            found,
        ))
    }
}
