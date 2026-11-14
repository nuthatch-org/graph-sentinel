//! What to do about a finding, as typed steps an operator or their agent can act on.
//! graph-sentinel never runs these (the opt-in graphman restart aside); they are proposals.

use sentinel_types::Finding;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    /// Safe to retry; undoes itself or changes nothing durable.
    Low,
    /// Changes config or state an operator should look at first.
    Medium,
    /// On-chain, or hard to undo.
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Step {
    /// Look at something before acting.
    Check { what: String },
    /// A shell command, and the host it belongs on.
    Command { run: String, on: &'static str },
    /// An edit to a config file.
    Config {
        file: &'static str,
        section: &'static str,
        change: String,
    },
    /// A statement for a database graph-sentinel only reads; run it with a role that can write.
    Sql {
        database: &'static str,
        statement: String,
    },
    /// Something only a person can do.
    Manual { what: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Remedy {
    pub risk: Risk,
    pub steps: Vec<Step>,
    /// How to tell it worked.
    pub verify: String,
}

fn s<'a>(ev: &'a Value, k: &str) -> Option<&'a str> {
    ev.get(k).and_then(Value::as_str)
}

fn check(w: impl Into<String>) -> Step {
    Step::Check { what: w.into() }
}

fn cmd(run: impl Into<String>, on: &'static str) -> Step {
    Step::Command {
        run: run.into(),
        on,
    }
}

const TAP_RESTART: &str = "docker compose restart tap-agent";

/// `restart_command` is the configured graphman template, with `{deployment}` in it.
pub fn remedy(f: &Finding, restart_command: &str) -> Option<Remedy> {
    let ev = &f.evidence;
    let restart = |d: &str| {
        cmd(
            restart_command.replace("{deployment}", d),
            "graph-node host",
        )
    };
    let r = match f.kind.as_str() {
        "frozen" | "stalled" | "stopped" => {
            let d = s(ev, "deployment").unwrap_or(&f.subject);
            let mut steps = Vec::new();
            if f.kind == "stopped" {
                steps.push(check(format!(
                    "The RPC for {} answers eth_getLogs; without it nothing can say whether work is waiting.",
                    s(ev, "network").unwrap_or("this chain")
                )));
            }
            // graph-node v0.45.0 reports a runner that died at start as healthy; a restart repeats it.
            steps.push(check(format!(
                "graph-node's log for {d}: \"Subgraph instance failed to run\" means the runner never started, \
                 often for want of a provider feature such as traces or archive, and a restart will not help until \
                 that provider is configured."
            )));
            steps.push(restart(d));
            Remedy {
                risk: Risk::Low,
                steps,
                verify: format!("{d}'s latest block moves within a few minutes and the finding clears."),
            }
        }
        "failed" => {
            let d = s(ev, "deployment").unwrap_or(&f.subject);
            Remedy {
                risk: Risk::Medium,
                steps: vec![
                    check(format!(
                        "The fatal error: {}. RPC and database errors are non-deterministic and usually clear on restart; \
                         a deterministic error recurs at the same block and will not.",
                        s(ev, "fatal_error")
                            .map(|e| e.chars().take(240).collect::<String>())
                            .unwrap_or_else(|| "none recorded".into())
                    )),
                    restart(d),
                    check("If it fails again at the same block, close its allocation rather than keep serving a failed deployment."),
                ],
                verify: format!("{d} reports healthy and its latest block moves."),
            }
        }
        "chain_stalled" => Remedy {
            risk: Risk::Low,
            steps: vec![check(format!(
                "graph-node's RPC or Firehose provider for {}: is it synced, and is graph-node pointed at the right one?",
                f.subject
            ))],
            verify: "The chain head moves again.".into(),
        },
        "index_node_down" => Remedy {
            risk: Risk::Low,
            steps: vec![
                check("graph-node is running and its index-node port (8030) answers; its logs say why if not."),
                cmd("docker compose restart graph-node", "graph-node host"),
            ],
            verify: "doctor reports the index-node up.".into(),
        },
        "restart_issued" => Remedy {
            risk: Risk::Low,
            steps: vec![check(
                "The restart command's output in the finding's evidence, and that graphman can reach graph-node's config and database.",
            )],
            verify: "The deployment's latest block moves.".into(),
        },
        "poi_stale" | "poi_due" => {
            let list = |k: &str| -> Vec<&str> {
                ev.get(k)
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default()
            };
            let mut steps = vec![check(
                "Why indexer-agent has not presented a POI: its logs, and whether the deployment is synced.",
            )];
            // Reallocating presents a POI and keeps the stake working; unallocate if the deployment is unwanted.
            steps.extend(list("allocations").iter().zip(list("deployments")).map(|(a, d)| {
                cmd(
                    format!("graph indexer actions queue reallocate {d} {a} <allocation-amount>"),
                    "indexer-agent host",
                )
            }));
            Remedy {
                risk: Risk::High,
                steps,
                verify: "The allocations leave the network view or show a fresh POI.".into(),
            }
        }
        "dispute_against" => Remedy {
            risk: Risk::High,
            steps: vec![
                check("The disputed POI or query against your graph-node at the same block, so you know whether it stands."),
                Step::Manual {
                    what: "Answer in the dispute's forum thread with that evidence before the arbitrator rules.".into(),
                },
            ],
            verify: "The dispute is ruled.".into(),
        },
        "dispute_cancellable" => Remedy {
            risk: Risk::High,
            steps: vec![Step::Manual {
                what: format!(
                    "Cancel dispute {} from the fisherman address on the DisputeManager to recover the bond.",
                    f.subject
                ),
            }],
            verify: "The dispute shows as cancelled.".into(),
        },
        "dispute_ruled" => Remedy {
            risk: Risk::Low,
            steps: vec![check("The ruling and its reasoning in the forum thread.")],
            verify: "Nothing further; it clears when acknowledged.".into(),
        },
        "reo_ineligible" | "reo_at_risk" => Remedy {
            risk: Risk::Medium,
            steps: vec![
                cmd("graph-sentinel reo candidates --limit 10", "sentinel host"),
                cmd(
                    "graph indexer rules set <deployment> decisionBasis always",
                    "indexer-agent host",
                ),
                check("Each new deployment syncs and starts taking queries before the next oracle run."),
            ],
            verify: "graph-sentinel reo shows the daily count at or above the minimum.".into(),
        },
        "reo_oracle_disagrees" => Remedy {
            risk: Risk::Low,
            steps: vec![check(
                "Query latency (the oracle wants under 5,000 ms) and freshness (within 50,000 blocks of head) \
                 on the deployments you count on.",
            )],
            verify: "The next oracle run renews eligibility.".into(),
        },
        "aggregator_down" => Remedy {
            risk: Risk::Low,
            steps: vec![
                check(format!(
                    "The aggregator endpoint answers gRPC: {}. If it is the sender's, nothing local fixes it.",
                    s(ev, "endpoint").unwrap_or("see evidence")
                )),
                Step::Manual {
                    what: "Report it to the sender (graph-support for the Foundation's gateway) and watch fees_near_limit.".into(),
                },
            ],
            verify: "The aggregator probe passes and RAVs resume.".into(),
        },
        "unknown_sender" => Remedy {
            risk: Risk::Medium,
            steps: vec![
                Step::Config {
                    file: "indexer-rs config.toml",
                    section: "[tap.sender_aggregator_endpoints]",
                    change: format!("\"{}\" = \"<the sender's aggregator URL>\"", f.subject),
                },
                cmd(TAP_RESTART, "tap-agent host"),
            ],
            verify: "RAVs appear for the sender.".into(),
        },
        "sender_denylisted" => {
            let hex = f.subject.trim_start_matches("0x").to_ascii_lowercase();
            Remedy {
                risk: Risk::Medium,
                steps: vec![
                    check("The sender's aggregator is healthy and RAVs flow; otherwise the sender is denylisted again."),
                    Step::Sql {
                        database: "indexer (tap-agent)",
                        statement: format!(
                            "DELETE FROM tap_horizon_denylist WHERE sender_address = '{hex}'; \
                             DELETE FROM scalar_tap_denylist WHERE sender_address = '{hex}';"
                        ),
                    },
                ],
                verify: "The finding clears and the gateway routes queries again.".into(),
            }
        }
        "fees_near_limit" => Remedy {
            risk: Risk::Medium,
            steps: vec![
                check("Why RAVs are not keeping up: aggregator_down or rav_stalled for the same sender."),
                Step::Config {
                    file: "indexer-rs config.toml",
                    section: "[tap]",
                    change: "Raise max_amount_willing_to_lose_grt only if the sender is trusted.".into(),
                },
            ],
            verify: "Unaggregated fees fall after the next RAV.".into(),
        },
        "rav_stalled" => Remedy {
            risk: Risk::Low,
            steps: vec![cmd(TAP_RESTART, "tap-agent host")],
            verify: "The oldest unaggregated receipt gets younger.".into(),
        },
        "escrow_low" => Remedy {
            risk: Risk::Low,
            steps: vec![Step::Manual {
                what: format!("Ask {} to top up its escrow for you.", f.subject),
            }],
            verify: "Escrow covers more days of fees.".into(),
        },
        "config_suspect" => Remedy {
            risk: Risk::Medium,
            steps: vec![Step::Config {
                file: "indexer-rs config.toml",
                section: "[tap]",
                change: f.subject.clone(),
            }],
            verify: "The finding clears on the next config read.".into(),
        },
        "budget_warning" | "budget_exceeded" => Remedy {
            risk: Risk::Low,
            steps: vec![check(
                "Which methods and deployments are spending, in the RPC spend section; a deployment stuck retrying is the usual cause.",
            )],
            verify: "Projected spend falls under the budget.".into(),
        },
        _ => return None,
    };
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sentinel_types::Severity;
    use serde_json::json;

    fn f(kind: &str, subject: &str, ev: Value) -> Finding {
        Finding::new("x", kind, subject, Severity::Warn, "s", ev, Utc::now())
    }

    #[test]
    fn frozen_fills_the_restart_template() {
        let r = remedy(
            &f("frozen", "Qmabc", json!({ "deployment": "Qmabc" })),
            "graphman restart {deployment}",
        )
        .unwrap();
        assert_eq!(r.risk, Risk::Low);
        assert_eq!(
            r.steps.last(),
            Some(&cmd("graphman restart Qmabc", "graph-node host"))
        );
    }

    #[test]
    fn denylist_sql_uses_tap_agents_bare_lowercase_hex() {
        let r = remedy(&f("sender_denylisted", "0xABCdef", json!({})), "").unwrap();
        let Step::Sql { statement, .. } = &r.steps[1] else {
            panic!()
        };
        assert!(statement.contains("sender_address = 'abcdef'"));
    }

    #[test]
    fn poi_findings_list_one_step_per_allocation() {
        let r = remedy(
            &f(
                "poi_stale",
                "allocations",
                json!({ "allocations": ["0x1", "0x2"], "deployments": ["Qma", "Qmb"] }),
            ),
            "",
        )
        .unwrap();
        assert_eq!(r.risk, Risk::High);
        assert_eq!(r.steps.len(), 3);
    }

    #[test]
    fn unknown_kinds_have_no_remedy() {
        assert!(remedy(&f("something_new", "x", json!({})), "").is_none());
    }
}
