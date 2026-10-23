//! Receipt evidence from the indexer database that tap-agent and indexer-service share.
//!
//! tap-agent deletes receipts once a RAV covers them, so no single read of the table can say
//! what was served on a past day. Three views are combined by the caller: live notifications,
//! a scan of receipts not yet aggregated, and the timestamp of each RAV.

use anyhow::{Context, Result};
use chrono::{Days, NaiveDate};
use sentinel_types::{Address, utc_day_of_ns};
use serde::Deserialize;
use sqlx::postgres::{PgListener, PgPool, PgPoolOptions};
use tokio::sync::mpsc;

/// One allocation was served at least once on this UTC day.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Served {
    pub day: NaiveDate,
    pub allocation: Address,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Notify,
    Scan,
    Rav,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Notify => "notify",
            Via::Scan => "scan",
            Via::Rav => "rav",
        }
    }
}

pub const HORIZON_CHANNEL: &str = "tap_horizon_receipt_notification";
pub const LEGACY_CHANNEL: &str = "scalar_tap_receipt_notification";

#[derive(Clone)]
pub struct TapDb {
    pool: PgPool,
}

#[derive(Debug, Clone, Default)]
pub struct Tables {
    pub horizon_receipts: bool,
    pub horizon_ravs: bool,
    pub legacy_receipts: bool,
    pub legacy_ravs: bool,
}

const DAY_NS: i64 = 86_400_000_000_000;

fn day_from_index(i: i64) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(1970, 1, 1)?.checked_add_days(Days::new(u64::try_from(i).ok()?))
}

impl TapDb {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect(url)
            .await
            .context("connecting to the indexer database")?;
        Ok(TapDb { pool })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn tables(&self) -> Result<Tables> {
        let exists = |t: &'static str| async move {
            sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
                .bind(t)
                .fetch_one(&self.pool)
                .await
        };
        Ok(Tables {
            horizon_receipts: exists("tap_horizon_receipts").await?,
            horizon_ravs: exists("tap_horizon_ravs").await?,
            legacy_receipts: exists("scalar_tap_receipts").await?,
            legacy_ravs: exists("scalar_tap_ravs").await?,
        })
    }

    /// Receipts still in the table with a timestamp at or after `since_ns`.
    pub async fn scan_receipts(&self, t: &Tables, since_ns: i64) -> Result<Vec<Served>> {
        let mut out = Vec::new();
        if t.horizon_receipts {
            out.extend(
                self.day_rows(
                    "SELECT DISTINCT RIGHT(collection_id, 40), div(timestamp_ns, $2)::bigint \
                     FROM tap_horizon_receipts WHERE timestamp_ns >= $1",
                    since_ns,
                )
                .await
                .context("scanning tap_horizon_receipts")?,
            );
        }
        if t.legacy_receipts {
            out.extend(
                self.day_rows(
                    "SELECT DISTINCT allocation_id, div(timestamp_ns, $2)::bigint \
                     FROM scalar_tap_receipts WHERE timestamp_ns >= $1",
                    since_ns,
                )
                .await
                .context("scanning scalar_tap_receipts")?,
            );
        }
        Ok(out)
    }

    /// A RAV's `timestamp_ns` is that of the newest receipt it aggregates, so each RAV
    /// proves its allocation was served on that day.
    pub async fn rav_days(&self, t: &Tables, since_ns: i64) -> Result<Vec<Served>> {
        let mut out = Vec::new();
        if t.horizon_ravs {
            out.extend(
                self.day_rows(
                    "SELECT RIGHT(collection_id, 40), div(timestamp_ns, $2)::bigint \
                     FROM tap_horizon_ravs WHERE timestamp_ns >= $1",
                    since_ns,
                )
                .await
                .context("reading tap_horizon_ravs")?,
            );
        }
        if t.legacy_ravs {
            out.extend(
                self.day_rows(
                    "SELECT allocation_id, div(timestamp_ns, $2)::bigint \
                     FROM scalar_tap_ravs WHERE timestamp_ns >= $1",
                    since_ns,
                )
                .await
                .context("reading scalar_tap_ravs")?,
            );
        }
        Ok(out)
    }

    async fn day_rows(&self, sql: &'static str, since_ns: i64) -> Result<Vec<Served>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(sql)
            .bind(since_ns)
            .bind(DAY_NS)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(a, d)| {
                Some(Served {
                    allocation: a.parse().ok()?,
                    day: day_from_index(d)?,
                })
            })
            .collect())
    }

    /// Payers with a receipt or a RAV at or after `since_ns`. Receipts alone undercount:
    /// aggregated ones are already deleted.
    pub async fn recent_payers(&self, t: &Tables, since_ns: i64) -> Result<Vec<String>> {
        let mut out = Vec::new();
        if t.horizon_receipts {
            out.extend(
                sqlx::query_scalar::<_, String>(
                    "SELECT DISTINCT payer FROM tap_horizon_receipts WHERE timestamp_ns >= $1",
                )
                .bind(since_ns)
                .fetch_all(&self.pool)
                .await?,
            );
        }
        if t.horizon_ravs {
            out.extend(
                sqlx::query_scalar::<_, String>(
                    "SELECT DISTINCT payer FROM tap_horizon_ravs WHERE timestamp_ns >= $1",
                )
                .bind(since_ns)
                .fetch_all(&self.pool)
                .await?,
            );
        }
        Ok(out)
    }

    /// Per payer: receipts still in the table, their value in GRT, oldest and newest timestamp.
    pub async fn unaggregated(&self, t: &Tables) -> Result<Vec<(String, i64, f64, i64, i64)>> {
        if !t.horizon_receipts {
            return Ok(vec![]);
        }
        let rows: Vec<(String, i64, String, i64, i64)> = sqlx::query_as(
            "SELECT payer, COUNT(*), SUM(value)::text, MIN(timestamp_ns)::bigint, MAX(timestamp_ns)::bigint
             FROM tap_horizon_receipts GROUP BY payer",
        )
        .fetch_all(&self.pool)
        .await
        .context("reading unaggregated receipts")?;
        Ok(rows
            .into_iter()
            .map(|(p, n, v, lo, hi)| (p, n, v.parse::<f64>().unwrap_or(0.0) / 1e18, lo, hi))
            .collect())
    }

    /// Current cumulative RAV value per (payer, collection), in GRT.
    pub async fn rav_totals(&self, t: &Tables) -> Result<Vec<(String, String, f64)>> {
        if !t.horizon_ravs {
            return Ok(vec![]);
        }
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT payer, collection_id, value_aggregate::text FROM tap_horizon_ravs",
        )
        .fetch_all(&self.pool)
        .await
        .context("reading RAV totals")?;
        Ok(rows
            .into_iter()
            .map(|(p, c, v)| (p, c, v.parse::<f64>().unwrap_or(0.0) / 1e18))
            .collect())
    }

    pub async fn denylist(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for sql in [
            "SELECT sender_address FROM tap_horizon_denylist",
            "SELECT sender_address FROM scalar_tap_denylist",
        ] {
            let table = if sql.contains("tap_horizon") {
                "tap_horizon_denylist"
            } else {
                "scalar_tap_denylist"
            };
            let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
                .bind(table)
                .fetch_one(&self.pool)
                .await?;
            if exists {
                out.extend(
                    sqlx::query_scalar::<_, String>(sql)
                        .fetch_all(&self.pool)
                        .await?,
                );
            }
        }
        Ok(out)
    }

    /// Forward every receipt notification tap-agent's triggers emit. LISTEN needs no grant.
    /// Returns when the connection drops; the caller reconnects and rescans.
    pub async fn listen(&self, tx: mpsc::Sender<Served>) -> Result<()> {
        let mut l = PgListener::connect_with(&self.pool).await?;
        l.listen_all([HORIZON_CHANNEL, LEGACY_CHANNEL]).await?;
        loop {
            let n = l.recv().await?;
            match parse_notification(n.channel(), n.payload()) {
                Some(s) => {
                    if tx.send(s).await.is_err() {
                        return Ok(());
                    }
                }
                None => tracing::debug!(channel = n.channel(), "unparseable receipt notification"),
            }
        }
    }
}

#[derive(Deserialize)]
struct Notification {
    collection_id: Option<String>,
    allocation_id: Option<String>,
    timestamp_ns: serde_json::Number,
}

pub fn parse_notification(channel: &str, payload: &str) -> Option<Served> {
    let n: Notification = serde_json::from_str(payload).ok()?;
    let allocation = match channel {
        HORIZON_CHANNEL => Address::from_collection_id(n.collection_id.as_deref()?).ok()?,
        LEGACY_CHANNEL => n.allocation_id?.parse().ok()?,
        _ => return None,
    };
    let ts = n.timestamp_ns.as_u64()? as i128;
    Some(Served {
        day: utc_day_of_ns(ts)?,
        allocation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tap_agent_trigger_payloads() {
        // Shapes from indexer-rs migrations: format('{"id": %s, "collection_id": "%s", ...}')
        let h = r#"{"id": 7, "collection_id": "000000000000000000000000f7b4cf0767de7ed5e11f3d47c5a5c73c5062e8ba", "signer_address": "abc", "timestamp_ns": 1793059200000000001, "value": 100}"#;
        let s = parse_notification(HORIZON_CHANNEL, h).unwrap();
        assert_eq!(s.day.to_string(), "2026-10-27");
        assert_eq!(
            s.allocation.to_string(),
            "0xf7b4cf0767de7ed5e11f3d47c5a5c73c5062e8ba"
        );

        let l = r#"{"id": 1, "allocation_id": "f7b4cf0767de7ed5e11f3d47c5a5c73c5062e8ba", "signer_address": "abc", "timestamp_ns": 1793059199999999999, "value": 1}"#;
        assert_eq!(
            parse_notification(LEGACY_CHANNEL, l)
                .unwrap()
                .day
                .to_string(),
            "2026-10-26"
        );

        assert!(parse_notification("other", h).is_none());
    }

    #[test]
    fn day_index_round_trips() {
        assert_eq!(
            day_from_index(1_793_059_200 / 86_400).unwrap().to_string(),
            "2026-10-27"
        );
    }
}
