//! The daemon's own page: `/` (the dashboard), `/api/snapshot`, `/metrics`, `/mcp`.
//! One embedded HTML file and two fonts; no frontend build, no third-party requests.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Days, NaiveDate, Utc};
use sentinel_checks::reo::Report;
use sentinel_checks::reo::projection::DailyCounts;
use sentinel_checks::reo::ranking::Candidate;
use sentinel_checks::reo::rules::Step;
use sentinel_types::Finding;
use serde::Serialize;
use tokio::sync::RwLock;

use crate::outputs::Metrics;

#[derive(Debug, Clone, Serialize)]
pub struct Source {
    pub name: String,
    pub up: bool,
    pub detail: String,
    pub checked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Day {
    pub day: NaiveDate,
    pub count: u32,
}

/// Everything the page shows, replaced wholesale on each tick.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub version: &'static str,
    pub indexer: String,
    pub generated_at: Option<DateTime<Utc>>,
    pub reo: Option<Report>,
    pub reo_error: Option<String>,
    /// The oracle's window ending today, zero-filled, oldest first.
    pub days: Vec<Day>,
    pub steps: Vec<Step>,
    pub findings: Vec<Finding>,
    /// What to do about each active finding, by fingerprint.
    pub remedies: std::collections::BTreeMap<String, sentinel_checks::remedy::Remedy>,
    /// The graphman template remedies fill in.
    #[serde(skip)]
    pub restart_command: String,
    pub sources: Vec<Source>,
    /// graph-node's database is configured, so candidates carry the local columns.
    pub graph_node: bool,
    pub candidates: Option<Vec<Candidate>>,
    /// Modules running, in the order they started.
    pub modules: Vec<String>,
    pub deployments: Option<Vec<crate::liveness::Row>>,
    pub liveness_at: Option<DateTime<Utc>>,
    pub spend: Option<crate::spend::View>,
    pub price: Option<sentinel_sources::lodestar::Price>,
    pub chain_lag: Option<std::collections::HashMap<String, sentinel_sources::lodestar::ChainLag>>,
    pub network: Option<crate::network::IndexerView>,
    pub fisherman: Option<crate::network::FishermanView>,
    pub network_at: Option<DateTime<Utc>>,
    pub payments: Option<crate::canary::View>,
    pub candidates_at: Option<DateTime<Utc>>,
}

impl Snapshot {
    pub fn set_source(&mut self, name: &str, r: &Result<String, String>) {
        let s = Source {
            name: name.into(),
            up: r.is_ok(),
            detail: match r {
                Ok(d) | Err(d) => d.clone(),
            },
            checked_at: Utc::now(),
        };
        match self.sources.iter_mut().find(|x| x.name == name) {
            Some(x) => *x = s,
            None => self.sources.push(s),
        }
    }
}

/// Zero-filled days `[today - window, today]`, the span one oracle run reads.
pub fn window_days(counts: &DailyCounts, today: NaiveDate, window: u64) -> Vec<Day> {
    (0..=window)
        .rev()
        .map(|i| {
            let day = today - Days::new(i);
            Day {
                day,
                count: counts.get(&day).copied().unwrap_or(0),
            }
        })
        .collect()
}

#[derive(Clone)]
struct AppState {
    snapshot: Arc<RwLock<Snapshot>>,
    metrics: Arc<Metrics>,
}

const INDEX: &str = include_str!("../assets/index.html");
const SPACE_GROTESK: &[u8] = include_bytes!("../assets/space-grotesk.woff2");
const JETBRAINS_MONO: &[u8] = include_bytes!("../assets/jetbrains-mono.woff2");

fn font(bytes: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("font/woff2")),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// Streamable HTTP without sessions or streaming: every reply is a single JSON body.
async fn mcp(State(s): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    // A browser page on another origin must not be able to read the snapshot through this.
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
        let origin_host = origin.split("://").nth(1);
        if origin_host.is_none() || origin_host != host {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    let msg: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(m) => m,
        Err(e) => {
            let e = serde_json::json!({ "jsonrpc": "2.0", "id": null,
                "error": { "code": -32700, "message": format!("parse error: {e}") } });
            return (StatusCode::BAD_REQUEST, axum::Json(e)).into_response();
        }
    };
    let snap = s.snapshot.clone();
    match crate::mcp::handle(&msg, || async move {
        Ok(serde_json::to_value(&*snap.read().await)?)
    })
    .await
    {
        Some(r) => axum::Json(r).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

pub async fn serve(
    addr: SocketAddr,
    snapshot: Arc<RwLock<Snapshot>>,
    metrics: Arc<Metrics>,
) -> Result<()> {
    let app = Router::new()
        .route(
            "/",
            get(|| async {
                (
                    [
                        (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                        (header::CACHE_CONTROL, "no-cache"),
                    ],
                    INDEX,
                )
            }),
        )
        .route(
            "/api/snapshot",
            get(|State(s): State<AppState>| async move {
                let snap = s.snapshot.read().await.clone();
                ([(header::CACHE_CONTROL, "no-store")], axum::Json(snap))
            }),
        )
        .route(
            "/metrics",
            get(|State(s): State<AppState>| async move {
                (
                    [(
                        header::CONTENT_TYPE,
                        "application/openmetrics-text; version=1.0.0; charset=utf-8",
                    )],
                    s.metrics.encode(),
                )
            }),
        )
        .route(
            "/mcp",
            post(mcp).get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .route(
            "/fonts/space-grotesk.woff2",
            get(|| async { font(SPACE_GROTESK) }),
        )
        .route(
            "/fonts/jetbrains-mono.woff2",
            get(|| async { font(JETBRAINS_MONO) }),
        )
        .with_state(AppState { snapshot, metrics });
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("dashboard on http://{addr}/");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_is_zero_filled_and_oldest_first() {
        let today: NaiveDate = "2026-10-23".parse().unwrap();
        let mut c = DailyCounts::new();
        c.insert(today, 3);
        c.insert(today - Days::new(28), 7);
        c.insert(today - Days::new(29), 9); // outside the window
        let w = window_days(&c, today, 28);
        assert_eq!(w.len(), 29);
        assert_eq!(w[0].count, 7);
        assert_eq!(w[28].count, 3);
        assert_eq!(w[1..28].iter().map(|d| d.count).sum::<u32>(), 0);
    }
}
