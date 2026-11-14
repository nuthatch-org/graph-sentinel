//! A read-only MCP server over the daemon's snapshot, so an operator's agent can ask what is
//! wrong and what to do about it. Served at `POST /mcp` on the dashboard listener, and over
//! stdio by `graph-sentinel mcp`, which reads a running daemon's `/api/snapshot`.

use std::future::Future;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL: &str = "2025-06-18";
const SUPPORTED: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "graph-sentinel watches a Graph Protocol indexer: deployments frozen while \
graph-node reports them healthy, POI deadlines and disputes, Rewards Eligibility Oracle shortfalls, \
and the TAP payment path. Start with `overview`. Every finding has a `remedy` of typed steps with a \
risk level; graph-sentinel never runs them, so propose them to the operator and let them approve \
anything above low risk. Figures come from the daemon's last tick; `generated_at` says when.";

fn tools() -> Value {
    let none = json!({ "type": "object", "properties": {}, "additionalProperties": false });
    let ro = json!({ "readOnlyHint": true, "openWorldHint": false });
    json!([
        {
            "name": "overview",
            "title": "Overview",
            "description": "Start here. The verdict across modules: finding counts by severity, the worst findings, \
                            sources that are down, which modules run, and when the daemon last ticked.",
            "inputSchema": none, "annotations": ro,
        },
        {
            "name": "findings",
            "title": "Findings",
            "description": "Active findings, worst first, each with its evidence and a remedy: typed steps \
                            (check, command, config, sql, manual), a risk level and how to verify the fix.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "min_severity": { "type": "string", "enum": ["info", "warn", "critical"] },
                    "check": { "type": "string", "description": "liveness, network, reo, tap or spend" },
                    "subject": { "type": "string", "description": "Substring of a deployment, sender or allocation" }
                },
                "additionalProperties": false
            },
            "annotations": ro,
        },
        {
            "name": "deployments",
            "title": "Deployments",
            "description": "graph-node deployments as the liveness module sees them: health, head, chain head, \
                            blocks behind, verdict and last progress. Worst first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "deployment": { "type": "string", "description": "Substring of a Qm hash" },
                    "unhealthy_only": { "type": "boolean" }
                },
                "additionalProperties": false
            },
            "annotations": ro,
        },
        {
            "name": "reo",
            "title": "REO eligibility",
            "description": "Rewards Eligibility Oracle: deployments served per day against the minimum, the \
                            rule steps, the projection, the chain's view, and the cheapest deployments to add.",
            "inputSchema": {
                "type": "object",
                "properties": { "candidates": { "type": "integer", "minimum": 0, "maximum": 50 } },
                "additionalProperties": false
            },
            "annotations": ro,
        },
        {
            "name": "network",
            "title": "Network view",
            "description": "The indexer on the network subgraph: provision, delegation, open allocations with \
                            POI deadlines, and disputes against it or filed by its fisherman address.",
            "inputSchema": none, "annotations": ro,
        },
        {
            "name": "payments",
            "title": "Payments",
            "description": "The TAP payment path: aggregator probes, senders, unaggregated fees against the \
                            loss limit, RAV age, escrow and the denylist.",
            "inputSchema": none, "annotations": ro,
        },
        {
            "name": "rpc_spend",
            "title": "RPC spend",
            "description": "Today's metered RPC spend, the projection and the budget.",
            "inputSchema": none, "annotations": ro,
        },
        {
            "name": "sources",
            "title": "Sources",
            "description": "Every input and notification channel graph-sentinel uses, whether it answered and when.",
            "inputSchema": none, "annotations": ro,
        },
    ])
}

fn rank(sev: &Value) -> u8 {
    match sev.as_str() {
        Some("critical") => 2,
        Some("warn") => 1,
        _ => 0,
    }
}

fn findings(snap: &Value) -> Vec<Value> {
    let remedies = &snap["remedies"];
    let mut out: Vec<Value> = snap["findings"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut f| {
            let r = f["fingerprint"]
                .as_str()
                .map(|fp| remedies[fp].clone())
                .unwrap_or(Value::Null);
            f["remedy"] = r;
            f
        })
        .collect();
    out.sort_by_key(|f| std::cmp::Reverse(rank(&f["severity"])));
    out
}

fn str_arg<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// One tool's answer, from the snapshot as JSON. `Err` is a bad argument, not a failed read.
fn call(name: &str, args: &Value, snap: &Value) -> Result<Value, String> {
    let at = |k: &str| snap.get(k).cloned().unwrap_or(Value::Null);
    let not_running = |module: &str| json!({ "running": false, "why": format!("the {module} module is not configured on this daemon") });
    Ok(match name {
        "overview" => {
            let all = findings(snap);
            let count = |s: &str| all.iter().filter(|f| f["severity"] == s).count();
            let down: Vec<Value> = snap["sources"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| s["up"] == false)
                .map(|s| json!({ "name": s["name"], "detail": s["detail"] }))
                .collect();
            json!({
                "indexer": at("indexer"),
                "version": at("version"),
                "generated_at": at("generated_at"),
                "modules": at("modules"),
                "findings": { "critical": count("critical"), "warn": count("warn"), "info": count("info") },
                "worst": all.iter().take(5).map(|f| json!({
                    "fingerprint": f["fingerprint"], "severity": f["severity"], "summary": f["summary"],
                })).collect::<Vec<_>>(),
                "sources_down": down,
            })
        }
        "findings" => {
            let min = match str_arg(args, "min_severity") {
                None => 0,
                Some(s @ ("info" | "warn" | "critical")) => rank(&json!(s)),
                Some(s) => {
                    return Err(format!(
                        "min_severity must be info, warn or critical, not {s:?}"
                    ));
                }
            };
            let check = str_arg(args, "check");
            let subject = str_arg(args, "subject").map(str::to_ascii_lowercase);
            let out: Vec<Value> = findings(snap)
                .into_iter()
                .filter(|f| rank(&f["severity"]) >= min)
                .filter(|f| check.is_none_or(|c| f["check"] == c))
                .filter(|f| {
                    subject.as_deref().is_none_or(|s| {
                        f["subject"]
                            .as_str()
                            .is_some_and(|x| x.to_ascii_lowercase().contains(s))
                    })
                })
                .collect();
            json!({ "generated_at": at("generated_at"), "findings": out })
        }
        "deployments" => match snap["deployments"].as_array() {
            None => not_running("liveness"),
            Some(rows) => {
                let want = str_arg(args, "deployment");
                let unhealthy = args.get("unhealthy_only").and_then(Value::as_bool) == Some(true);
                let rows: Vec<&Value> = rows
                    .iter()
                    .filter(|r| {
                        want.is_none_or(|w| r["deployment"].as_str().is_some_and(|d| d.contains(w)))
                    })
                    .filter(|r| {
                        !unhealthy
                            || !matches!(
                                r["verdict"].as_str(),
                                Some("moving" | "caught_up" | "idle" | "paused")
                            )
                    })
                    .collect();
                json!({ "checked_at": at("liveness_at"), "deployments": rows })
            }
        },
        "reo" => {
            if snap["reo"].is_null() && snap["reo_error"].is_null() {
                not_running("REO meter")
            } else {
                let n = args
                    .get("candidates")
                    .and_then(Value::as_u64)
                    .unwrap_or(10)
                    .min(50) as usize;
                let cands: Vec<Value> = snap["candidates"]
                    .as_array()
                    .map(|c| c.iter().take(n).cloned().collect())
                    .unwrap_or_default();
                json!({
                    "report": at("reo"), "error": at("reo_error"), "days": at("days"),
                    "steps": at("steps"), "candidates": cands, "candidates_at": at("candidates_at"),
                })
            }
        }
        "network" => {
            if snap["network"].is_null() {
                not_running("network")
            } else {
                json!({ "indexer": at("network"), "fisherman": at("fisherman"), "checked_at": at("network_at") })
            }
        }
        "payments" => match snap.get("payments") {
            Some(p) if !p.is_null() => p.clone(),
            _ => not_running("payments"),
        },
        "rpc_spend" => match snap.get("spend") {
            Some(s) if !s.is_null() => s.clone(),
            _ => not_running("RPC spend"),
        },
        "sources" => json!({ "sources": at("sources") }),
        other => return Err(format!("no tool named {other:?}")),
    })
}

fn ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

fn text_result(v: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&v).unwrap_or_default();
    let mut r = json!({ "content": [{ "type": "text", "text": text }], "isError": is_error });
    if !is_error && v.is_object() {
        r["structuredContent"] = v;
    }
    r
}

/// Answer one JSON-RPC message. Notifications get `None`. `snapshot` is only read for tool calls.
pub async fn handle<F, Fut>(msg: &Value, snapshot: F) -> Option<Value>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let id = msg.get("id")?.clone();
    let method = msg["method"].as_str().unwrap_or_default();
    Some(match method {
        "initialize" => {
            let asked = msg["params"]["protocolVersion"]
                .as_str()
                .unwrap_or(PROTOCOL);
            let version = if SUPPORTED.contains(&asked) {
                asked
            } else {
                PROTOCOL
            };
            ok(
                &id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "graph-sentinel", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "ping" => ok(&id, json!({})),
        "tools/list" => ok(&id, json!({ "tools": tools() })),
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or_default();
            let args = msg["params"]
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !tools()
                .as_array()
                .is_some_and(|t| t.iter().any(|t| t["name"] == name))
            {
                return Some(err(&id, -32602, format!("no tool named {name:?}")));
            }
            match snapshot().await {
                Err(e) => ok(
                    &id,
                    text_result(
                        json!({ "error": format!("graph-sentinel is not answering: {e:#}") }),
                        true,
                    ),
                ),
                Ok(snap) => match call(name, &args, &snap) {
                    Ok(v) => ok(&id, text_result(v, false)),
                    Err(e) => ok(&id, text_result(json!({ "error": e }), true)),
                },
            }
        }
        _ => err(&id, -32601, format!("method {method:?} not found")),
    })
}

/// `graph-sentinel mcp`: newline-delimited JSON-RPC on stdin and stdout.
pub async fn stdio(base: String) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let url = format!("{}/api/snapshot", base.trim_end_matches('/'));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Err(e) => Some(err(&Value::Null, -32700, format!("parse error: {e}"))),
            Ok(msg) => {
                handle(&msg, || async {
                    let r = client.get(&url).send().await.context("connecting")?;
                    r.error_for_status()
                        .context("reading the snapshot")?
                        .json::<Value>()
                        .await
                        .context("decoding the snapshot")
                })
                .await
            }
        };
        if let Some(r) = reply {
            out.write_all(serde_json::to_string(&r)?.as_bytes()).await?;
            out.write_all(b"\n").await?;
            out.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Value {
        json!({
            "indexer": "test", "version": "0.0.1", "generated_at": "2026-11-14T12:00:00Z",
            "modules": ["liveness"],
            "findings": [
                { "check": "tap", "kind": "rav_stalled", "subject": "0xabc", "severity": "critical",
                  "fingerprint": "tap:rav_stalled:0xabc", "summary": "stalled", "evidence": {} },
                { "check": "liveness", "kind": "stalled", "subject": "QmXyz", "severity": "warn",
                  "fingerprint": "liveness:stalled:QmXyz", "summary": "behind", "evidence": {} },
            ],
            "remedies": { "tap:rav_stalled:0xabc": { "risk": "low", "steps": [], "verify": "v" } },
            "sources": [{ "name": "index-node", "up": false, "detail": "refused" }],
            "deployments": null, "reo": null, "reo_error": null, "network": null,
        })
    }

    async fn rpc(msg: Value) -> Option<Value> {
        handle(&msg, || async { Ok(snap()) }).await
    }

    async fn tool(name: &str, args: Value) -> Value {
        rpc(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": { "name": name, "arguments": args } }))
        .await
        .unwrap()["result"]
            .clone()
    }

    #[tokio::test]
    async fn initialize_negotiates_and_notifications_get_no_reply() {
        let r = rpc(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                            "params": { "protocolVersion": "2025-03-26" } }))
        .await
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        let r = rpc(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                            "params": { "protocolVersion": "1999-01-01" } }))
        .await
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL);
        assert!(
            rpc(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn findings_carry_remedies_and_filter() {
        let r = tool("findings", json!({})).await;
        let f = r["structuredContent"]["findings"].as_array().unwrap();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0]["remedy"]["risk"], "low");
        assert!(f[1]["remedy"].is_null());
        let r = tool("findings", json!({ "min_severity": "critical" })).await;
        assert_eq!(
            r["structuredContent"]["findings"].as_array().unwrap().len(),
            1
        );
        let r = tool("findings", json!({ "subject": "qmxyz" })).await;
        assert_eq!(r["structuredContent"]["findings"][0]["kind"], "stalled");
        assert_eq!(
            tool("findings", json!({ "min_severity": "loud" })).await["isError"],
            true
        );
    }

    #[tokio::test]
    async fn overview_counts_and_absent_modules_say_so() {
        let o = tool("overview", json!({})).await["structuredContent"].clone();
        assert_eq!(o["findings"]["critical"], 1);
        assert_eq!(o["sources_down"][0]["name"], "index-node");
        let d = tool("deployments", json!({})).await["structuredContent"].clone();
        assert_eq!(d["running"], false);
    }

    #[tokio::test]
    async fn unknown_tool_is_a_protocol_error_and_a_dead_daemon_a_tool_error() {
        let r = rpc(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "nope" } }))
            .await
            .unwrap();
        assert_eq!(r["error"]["code"], -32602);
        let r = handle(
            &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "overview" } }),
            || async { Err(anyhow::anyhow!("connection refused")) },
        )
        .await
        .unwrap();
        assert_eq!(r["result"]["isError"], true);
    }
}
