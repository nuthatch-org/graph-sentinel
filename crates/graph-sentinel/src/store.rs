//! Local state: the served-day ledger, the allocation map and the findings history.
//! Survives tap-agent deleting receipts and graph-sentinel restarting.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Days, NaiveDate, Utc};
use sentinel_checks::liveness::manifest::Activity;
use sentinel_checks::reo::projection::DailyCounts;
use sentinel_sources::tap::{Served, Via};
use sentinel_types::{Address, DeploymentId, Finding};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

use crate::findings::Stored;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS served (
        day TEXT NOT NULL, allocation TEXT NOT NULL, via TEXT NOT NULL, first_seen TEXT NOT NULL,
        PRIMARY KEY (day, allocation))",
    "CREATE TABLE IF NOT EXISTS allocations (allocation TEXT PRIMARY KEY, deployment TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS spend (
        day TEXT NOT NULL, provider TEXT NOT NULL, method TEXT NOT NULL,
        requests INTEGER NOT NULL, cu INTEGER NOT NULL, PRIMARY KEY (day, provider, method))",
    "CREATE TABLE IF NOT EXISTS rav_snap (ts INTEGER NOT NULL, payer TEXT NOT NULL, collection TEXT NOT NULL, grt REAL NOT NULL)",
    "CREATE INDEX IF NOT EXISTS rav_snap_ts ON rav_snap (ts)",
    "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS manifests (deployment TEXT PRIMARY KEY, activity TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS findings (
        fingerprint TEXT PRIMARY KEY, body TEXT NOT NULL, active INTEGER NOT NULL, missed INTEGER NOT NULL)",
];

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        for s in SCHEMA {
            sqlx::query(*s).execute(&pool).await?;
        }
        Ok(Store { pool })
    }

    pub async fn record(&self, served: &[Served], via: Via, now: DateTime<Utc>) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let mut added = 0;
        for s in served {
            added += sqlx::query(
                "INSERT OR IGNORE INTO served (day, allocation, via, first_seen) VALUES (?, ?, ?, ?)",
            )
            .bind(s.day.to_string())
            .bind(s.allocation.to_string())
            .bind(via.as_str())
            .bind(now.to_rfc3339())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        Ok(added)
    }

    pub async fn set_allocations(&self, map: &HashMap<Address, DeploymentId>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for (a, d) in map {
            sqlx::query(
                "INSERT OR REPLACE INTO allocations (allocation, deployment) VALUES (?, ?)",
            )
            .bind(a.to_string())
            .bind(d.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Allocations seen in receipts that the map cannot place yet.
    pub async fn unmapped(&self) -> Result<u64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT allocation) FROM served
             WHERE allocation NOT IN (SELECT allocation FROM allocations)",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(n as u64)
    }

    pub async fn daily_counts(&self, since: NaiveDate) -> Result<DailyCounts> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT s.day, COUNT(DISTINCT a.deployment) FROM served s
             JOIN allocations a ON a.allocation = s.allocation
             WHERE s.day >= ? GROUP BY s.day",
        )
        .bind(since.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(d, n)| Some((d.parse().ok()?, n as u32)))
            .collect())
    }

    pub async fn first_day(&self) -> Result<Option<NaiveDate>> {
        let d: Option<String> = sqlx::query_scalar("SELECT MIN(day) FROM served")
            .fetch_one(&self.pool)
            .await?;
        Ok(d.and_then(|d| d.parse().ok()))
    }

    pub async fn deployments_served_since(
        &self,
        since: NaiveDate,
    ) -> Result<HashSet<DeploymentId>> {
        let v: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT a.deployment FROM served s
             JOIN allocations a ON a.allocation = s.allocation WHERE s.day >= ?",
        )
        .bind(since.to_string())
        .fetch_all(&self.pool)
        .await?;
        Ok(v.into_iter().filter_map(|d| d.parse().ok()).collect())
    }

    /// Per day, the deployments served, for `graph-sentinel reo history`.
    pub async fn history(
        &self,
        days: u64,
        today: NaiveDate,
    ) -> Result<Vec<(NaiveDate, Vec<String>)>> {
        let since = today - Days::new(days);
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT s.day, a.deployment FROM served s
             JOIN allocations a ON a.allocation = s.allocation WHERE s.day >= ? ORDER BY s.day, a.deployment",
        )
        .bind(since.to_string())
        .fetch_all(&self.pool)
        .await?;
        let mut out: Vec<(NaiveDate, Vec<String>)> = Vec::new();
        for (d, dep) in rows {
            let Ok(d) = d.parse() else { continue };
            match out.last_mut() {
                Some((last, v)) if *last == d => v.push(dep),
                _ => out.push((d, vec![dep])),
            }
        }
        Ok(out)
    }

    pub async fn add_spend(
        &self,
        day: NaiveDate,
        provider: &str,
        method: &str,
        requests: u64,
        cu: u64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO spend (day, provider, method, requests, cu) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (day, provider, method)
             DO UPDATE SET requests = requests + excluded.requests, cu = cu + excluded.cu",
        )
        .bind(day.to_string())
        .bind(provider)
        .bind(method)
        .bind(requests as i64)
        .bind(cu as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn spend_cu(&self, day: NaiveDate) -> Result<u64> {
        let v: Option<i64> = sqlx::query_scalar("SELECT SUM(cu) FROM spend WHERE day = ?")
            .bind(day.to_string())
            .fetch_one(&self.pool)
            .await?;
        Ok(v.unwrap_or(0) as u64)
    }

    /// The day's billed lines, most expensive first.
    pub async fn spend_lines(
        &self,
        day: NaiveDate,
        limit: i64,
    ) -> Result<Vec<(String, String, u64, u64)>> {
        let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
            "SELECT provider, method, requests, cu FROM spend WHERE day = ? AND cu > 0
             ORDER BY cu DESC LIMIT ?",
        )
        .bind(day.to_string())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(p, m, r, c)| (p, m, r as u64, c as u64))
            .collect())
    }

    /// Billed CU per day for the last `days` days ending today, zero-filled, oldest first.
    pub async fn spend_days(&self, today: NaiveDate, days: u64) -> Result<Vec<(NaiveDate, u64)>> {
        let since = today - Days::new(days - 1);
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT day, SUM(cu) FROM spend WHERE day >= ? GROUP BY day")
                .bind(since.to_string())
                .fetch_all(&self.pool)
                .await?;
        let by: HashMap<String, i64> = rows.into_iter().collect();
        Ok((0..days)
            .rev()
            .map(|i| {
                let d = today - Days::new(i);
                (d, by.get(&d.to_string()).copied().unwrap_or(0) as u64)
            })
            .collect())
    }

    /// Record cumulative RAV totals and return fees per payer over the last 24 hours (or since
    /// the first snapshot, if younger), from the growth of each collection's total.
    pub async fn rav_fees_24h(
        &self,
        now: i64,
        totals: &[(String, String, f64)],
    ) -> Result<HashMap<String, f64>> {
        let mut tx = self.pool.begin().await?;
        for (p, c, g) in totals {
            sqlx::query("INSERT INTO rav_snap (ts, payer, collection, grt) VALUES (?, ?, ?, ?)")
                .bind(now)
                .bind(p)
                .bind(c)
                .bind(g)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM rav_snap WHERE ts < ?")
            .bind(now - 2 * 86_400)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let rows: Vec<(String, f64)> = sqlx::query_as(
            "SELECT payer, TOTAL(MAX(0.0, last - first)) FROM (
                SELECT payer, collection,
                       (SELECT grt FROM rav_snap s2 WHERE s2.payer = s.payer AND s2.collection = s.collection
                          AND s2.ts >= ? ORDER BY ts ASC LIMIT 1) AS first,
                       (SELECT grt FROM rav_snap s3 WHERE s3.payer = s.payer AND s3.collection = s.collection
                          ORDER BY ts DESC LIMIT 1) AS last
                FROM rav_snap s WHERE s.ts >= ? GROUP BY payer, collection
             ) GROUP BY payer",
        )
        .bind(now - 86_400)
        .bind(now - 86_400)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    pub async fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM meta WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn clear_meta(&self, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM meta WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn manifest(&self, deployment: &str) -> Result<Option<Activity>> {
        let v: Option<String> =
            sqlx::query_scalar("SELECT activity FROM manifests WHERE deployment = ?")
                .bind(deployment)
                .fetch_optional(&self.pool)
                .await?;
        Ok(v.and_then(|v| serde_json::from_str(&v).ok()))
    }

    pub async fn save_manifest(&self, deployment: &str, a: &Activity) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO manifests (deployment, activity) VALUES (?, ?)")
            .bind(deployment)
            .bind(serde_json::to_string(a)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn findings(&self, check: &str) -> Result<Vec<Stored>> {
        let rows: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT body, active, missed FROM findings WHERE fingerprint LIKE ? || ':%'",
        )
        .bind(check)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(b, a, m)| {
                Ok(Stored {
                    finding: serde_json::from_str(&b)?,
                    active: a != 0,
                    missed: m as u32,
                })
            })
            .collect()
    }

    pub async fn active_findings(&self) -> Result<Vec<Finding>> {
        let rows: Vec<String> = sqlx::query_scalar("SELECT body FROM findings WHERE active = 1")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(|b| Ok(serde_json::from_str(b)?)).collect()
    }

    pub async fn save_findings(&self, stored: &[Stored]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for s in stored {
            sqlx::query(
                "INSERT OR REPLACE INTO findings (fingerprint, body, active, missed) VALUES (?, ?, ?, ?)",
            )
            .bind(&s.finding.fingerprint)
            .bind(serde_json::to_string(&s.finding)?)
            .bind(s.active as i64)
            .bind(s.missed as i64)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn served(day: &str, alloc: &str) -> Served {
        Served {
            day: d(day),
            allocation: alloc.parse().unwrap(),
        }
    }

    const A1: &str = "0x1111111111111111111111111111111111111111";
    const A2: &str = "0x2222222222222222222222222222222222222222";
    const A3: &str = "0x3333333333333333333333333333333333333333";
    const DEP_X: &str = "QmZA6BHHHiFGZAawSw4UVPjEBFT87Z3wu3nc7K7rgEZpeV";
    const DEP_Y: &str = "QmfXRVfP1AeQZNH8tWv5zqUNeVQ3YgDutQXYbeaQUC4uS6";

    #[tokio::test]
    async fn counts_distinct_deployments_not_allocations() {
        let dir = std::env::temp_dir().join(format!("graph-sentinel-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("counts.sqlite");
        let _ = std::fs::remove_file(&path);
        let s = Store::open(&path).await.unwrap();
        let now = Utc::now();
        // Two allocations on the same deployment count once; a repeat sighting is ignored.
        s.record(
            &[served("2026-10-22", A1), served("2026-10-22", A2)],
            Via::Scan,
            now,
        )
        .await
        .unwrap();
        let again = s
            .record(&[served("2026-10-22", A1)], Via::Notify, now)
            .await
            .unwrap();
        assert_eq!(again, 0);
        s.record(&[served("2026-10-23", A3)], Via::Rav, now)
            .await
            .unwrap();

        assert_eq!(s.unmapped().await.unwrap(), 3);
        let map = [(A1, DEP_X), (A2, DEP_X), (A3, DEP_Y)]
            .into_iter()
            .map(|(a, dep)| (a.parse().unwrap(), dep.parse().unwrap()))
            .collect();
        s.set_allocations(&map).await.unwrap();
        assert_eq!(s.unmapped().await.unwrap(), 0);

        let c = s.daily_counts(d("2026-10-01")).await.unwrap();
        assert_eq!(c.get(&d("2026-10-22")), Some(&1));
        assert_eq!(c.get(&d("2026-10-23")), Some(&1));
        assert_eq!(s.first_day().await.unwrap(), Some(d("2026-10-22")));
        assert_eq!(
            s.deployments_served_since(d("2026-10-23"))
                .await
                .unwrap()
                .len(),
            1
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
