//! Runs against a Postgres carrying indexer-rs's TAP migrations, when one is provided:
//! `GRAPH_SENTINEL_TEST_TAP_DB=postgres://… cargo test -p sentinel-sources --test tap_db`.
//! Each run uses its own timestamps far in the future, so reruns on one database do not collide.

use std::time::Duration;

use sentinel_sources::tap::{Served, TapDb};
use tokio::sync::mpsc;

const DAY_NS: i64 = 86_400_000_000_000;

fn db_url() -> Option<String> {
    std::env::var("GRAPH_SENTINEL_TEST_TAP_DB").ok()
}

fn alloc(n: u8) -> String {
    format!(
        "{:040x}",
        0xabc0_0000u64 + n as u64 + (std::process::id() as u64) * 1000
    )
}

#[tokio::test]
async fn scan_ravs_and_notifications() {
    let Some(url) = db_url() else {
        eprintln!("GRAPH_SENTINEL_TEST_TAP_DB not set; skipped");
        return;
    };
    let db = TapDb::connect(&url).await.unwrap();
    let t = db.tables().await.unwrap();
    assert!(t.horizon_receipts && t.horizon_ravs && t.legacy_receipts && t.legacy_ravs);

    // A day well clear of real data: 2099-01-01 + pid days.
    let base_day: i64 = 47_482 + std::process::id() as i64 % 1000;
    let day_ns = |d: i64, offset: i64| (base_day + d) * DAY_NS + offset;
    let (a1, a2, a3, a4) = (alloc(1), alloc(2), alloc(3), alloc(4));
    let pool = db.pool();

    let horizon = "INSERT INTO tap_horizon_receipts (signer_address, signature, collection_id, payer,
        data_service, service_provider, timestamp_ns, nonce, value)
        VALUES (repeat('1',40), '\\x00', $1, repeat('2',40), repeat('3',40), repeat('4',40), $2::numeric, 0, 1)";
    // Last nanosecond of day 0 and first of day 1 must land on different days.
    for (a, ts) in [
        (&a1, day_ns(1, -1)),
        (&a1, day_ns(1, 0)),
        (&a2, day_ns(1, 5)),
    ] {
        sqlx::query(horizon)
            .bind(format!("{:0>64}", a))
            .bind(ts.to_string())
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO scalar_tap_receipts (signer_address, signature, allocation_id, timestamp_ns, nonce, value)
         VALUES (repeat('1',40), '\\x00', $1, $2::numeric, 0, 1)",
    )
    .bind(&a3)
    .bind(day_ns(0, 10).to_string())
    .execute(pool)
    .await
    .unwrap();

    let got = db.scan_receipts(&t, day_ns(0, 0)).await.unwrap();
    let mine: Vec<&Served> = got
        .iter()
        .filter(|s| {
            [&a1, &a2, &a3]
                .iter()
                .any(|a| s.allocation.hex() == a.as_str())
        })
        .collect();
    let day0 = mine
        .iter()
        .filter(|s| s.allocation.hex() == a1)
        .map(|s| s.day)
        .min()
        .unwrap();
    assert_eq!(mine.len(), 4, "a1 on two days, a2 and a3 once: {mine:?}");
    assert!(
        mine.iter()
            .any(|s| s.allocation.hex() == a1 && s.day == day0.succ_opt().unwrap())
    );

    // The scan's lower bound is respected.
    let later = db.scan_receipts(&t, day_ns(1, 0)).await.unwrap();
    assert!(!later.iter().any(|s| s.allocation.hex() == a3));

    sqlx::query(
        "INSERT INTO tap_horizon_ravs (signature, collection_id, payer, data_service, service_provider,
         timestamp_ns, value_aggregate, metadata) VALUES ('\\x00', $1, repeat('2',40), repeat('3',40),
         repeat('4',40), $2::numeric, 5, '\\x00')",
    )
    .bind(format!("{:0>64}", a4))
    .bind(day_ns(2, 7).to_string())
    .execute(pool)
    .await
    .unwrap();
    let ravs = db.rav_days(&t, day_ns(0, 0)).await.unwrap();
    let r = ravs
        .iter()
        .find(|s| s.allocation.hex() == a4)
        .expect("rav day");
    assert_eq!(r.day, day0 + chrono::Days::new(2));

    // Live path: tap-agent's own trigger fires on insert and reaches the listener.
    let (tx, mut rx) = mpsc::channel(16);
    let l = db.clone();
    let task = tokio::spawn(async move { l.listen(tx).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    sqlx::query(horizon)
        .bind(format!("{:0>64}", a2))
        .bind(day_ns(3, 1).to_string())
        .execute(pool)
        .await
        .unwrap();
    let s = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let s = rx.recv().await.expect("listener alive");
            if s.allocation.hex() == a2 {
                return s;
            }
        }
    })
    .await
    .expect("notification within 5 s");
    assert_eq!(s.day, day0 + chrono::Days::new(3));
    task.abort();
}
