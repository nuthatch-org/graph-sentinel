//! Is a TAP aggregator up? A gRPC server answers any call, even one to a method it lacks,
//! with HTTP 200, `content-type: application/grpc` and a `grpc-status` header. A Cloudflare
//! error page (the 2026-09-30 outage) answers `text/html` with neither, so that is the test.

use std::time::Duration;

pub struct Answer {
    pub ok: bool,
    pub detail: String,
}

pub async fn probe(http: &reqwest::Client, endpoint: &str) -> Answer {
    let url = format!(
        "{}/grpc.health.v1.Health/Check",
        endpoint.trim_end_matches('/')
    );
    let r = http
        .post(url)
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(Vec::new())
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    match r {
        Ok(resp) => {
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none")
                .to_string();
            let grpc = resp.headers().contains_key("grpc-status");
            let ok = grpc && ct.starts_with("application/grpc");
            Answer {
                ok,
                detail: if ok {
                    "answers as gRPC".into()
                } else {
                    format!("HTTP {} with {ct}, no grpc-status", resp.status().as_u16())
                },
            }
        }
        Err(e) => Answer {
            ok: false,
            detail: e.without_url().to_string(),
        },
    }
}

/// A client that speaks HTTP/2, as gRPC needs.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .build()
        .expect("static client config")
}
