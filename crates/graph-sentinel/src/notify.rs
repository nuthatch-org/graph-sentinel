//! Telegram, Discord, Slack and generic webhooks. Written to be read on a phone:
//! severity and a plain title first, a sentence or two, the indexer, a link to the dashboard.

use std::sync::Mutex;

use chrono::{DateTime, Utc};
use sentinel_types::Severity;
use serde_json::{Value, json};

use crate::config::{Channel, ChannelKind};
use crate::findings::{Event, Transition};

/// Above this many events in one tick, each channel gets a single digest instead.
const DIGEST_OVER: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// `CRITICAL`, `WARN`, `RESOLVED`, `WATCHING`.
    pub label: String,
    pub title: String,
    pub body: String,
    pub severity: Severity,
    pub resolved: bool,
}

pub fn title(kind: &str) -> String {
    match kind {
        "reo_ineligible" => "REO ineligible".into(),
        "reo_at_risk" => "REO eligibility at risk".into(),
        "reo_oracle_disagrees" => "Oracle disagrees with receipts".into(),
        "frozen" => "Deployment frozen".into(),
        "stalled" => "Deployment stalled".into(),
        "stopped" => "Deployment not moving".into(),
        "failed" => "Deployment failed".into(),
        "chain_stalled" => "Chain head stalled".into(),
        "restart_issued" => "Restart issued".into(),
        "index_node_down" => "graph-node unreachable".into(),
        "budget_warning" => "RPC spend warning".into(),
        "budget_exceeded" => "RPC budget reached".into(),
        other => {
            let s = other.replace('_', " ");
            let mut c = s.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default()
        }
    }
}

pub fn message(e: &Event) -> Message {
    let f = &e.finding;
    let (label, resolved) = match e.transition {
        Transition::New => (f.severity.to_string().to_uppercase(), false),
        Transition::Escalated => (
            format!("{} (escalated)", f.severity.to_string().to_uppercase()),
            false,
        ),
        Transition::Resolved => ("RESOLVED".to_string(), true),
    };
    let body = if resolved {
        format!("Cleared. It was: {}", f.summary)
    } else {
        f.summary.clone()
    };
    Message {
        label,
        title: title(&f.kind),
        body,
        severity: f.severity,
        resolved,
    }
}

pub fn digest(events: &[Event]) -> Message {
    let lines: Vec<String> = events
        .iter()
        .map(|e| {
            let m = message(e);
            format!("{}: {}", m.label, m.title)
        })
        .collect();
    Message {
        label: "DIGEST".into(),
        title: format!("{} changes", events.len()),
        body: lines.join("\n"),
        severity: events
            .iter()
            .map(|e| e.finding.severity)
            .max()
            .unwrap_or(Severity::Info),
        resolved: false,
    }
}

fn html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn short(addr: &str) -> String {
    if addr.len() > 14 {
        format!("{}…{}", &addr[..6], &addr[addr.len() - 4..])
    } else {
        addr.into()
    }
}

pub fn telegram_text(m: &Message, indexer: &str, public_url: Option<&str>) -> String {
    let mut t = format!(
        "<b>{}</b> · {}\n{}\n\n<code>{}</code>",
        html(&m.label),
        html(&m.title),
        html(&m.body),
        html(&short(indexer))
    );
    if let Some(u) = public_url {
        t.push_str(&format!(" · <a href=\"{}\">dashboard</a>", html(u)));
    }
    t
}

/// The dashboard's palette, so a Discord embed's edge matches the page.
fn colour(m: &Message) -> u32 {
    if m.resolved {
        return 0x86b592;
    }
    if m.label == "WATCHING" {
        return 0x8bb8dc;
    }
    match m.severity {
        Severity::Critical => 0xcdb06a,
        Severity::Warn => 0xcd8560,
        Severity::Info => 0x9c978c,
    }
}

pub fn discord_body(
    m: &Message,
    indexer: &str,
    public_url: Option<&str>,
    now: DateTime<Utc>,
) -> Value {
    let mut embed = json!({
        "title": format!("{} · {}", m.label, m.title),
        "description": m.body,
        "color": colour(m),
        "footer": { "text": format!("graph-sentinel · {}", short(indexer)) },
        "timestamp": now.to_rfc3339(),
    });
    if let Some(u) = public_url {
        embed["url"] = json!(u);
    }
    json!({ "username": "graph-sentinel", "embeds": [embed], "allowed_mentions": { "parse": [] } })
}

pub fn slack_text(m: &Message, indexer: &str, public_url: Option<&str>) -> String {
    let mut t = format!(
        "*{}* · {}\n{}\n`{}`",
        m.label,
        m.title,
        m.body,
        short(indexer)
    );
    if let Some(u) = public_url {
        t.push_str(&format!(" · <{u}|dashboard>"));
    }
    t
}

struct Target {
    name: String,
    channel: Channel,
    /// Webhook URL, or the Telegram sendMessage endpoint with its token.
    endpoint: String,
}

#[derive(Debug, Clone, Default)]
struct Delivery {
    sent: u64,
    last_ok: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

pub struct Notifier {
    http: reqwest::Client,
    targets: Vec<Target>,
    indexer: String,
    public_url: Option<String>,
    status: Mutex<Vec<Delivery>>,
    /// Channels configured but unusable, with the reason, for the dashboard.
    broken: Vec<(String, String)>,
}

impl Notifier {
    pub fn new(channels: &[Channel], indexer: String, public_url: Option<String>) -> Self {
        let mut targets = Vec::new();
        let mut broken = Vec::new();
        for (i, c) in channels.iter().enumerate() {
            let same = |x: &&Channel| x.kind == c.kind;
            let name = if channels.iter().filter(same).count() > 1 {
                format!(
                    "{} {}",
                    c.kind.name(),
                    channels[..i].iter().filter(same).count() + 1
                )
            } else {
                c.kind.name().to_string()
            };
            let endpoint = match c.kind {
                ChannelKind::Telegram => match (c.resolved_bot_token(), &c.chat_id) {
                    (Some(tok), Some(_)) => Ok(format!(
                        "{}/bot{tok}/sendMessage",
                        c.telegram_api.trim_end_matches('/')
                    )),
                    (None, _) => Err("no bot token (bot_token or bot_token_env)".to_string()),
                    (_, None) => Err("no chat_id".to_string()),
                },
                _ => c
                    .resolved_url()
                    .ok_or_else(|| "no url (url or url_env)".to_string()),
            };
            match endpoint {
                Ok(endpoint) => targets.push(Target {
                    name,
                    channel: c.clone(),
                    endpoint,
                }),
                Err(why) => {
                    tracing::warn!(channel = %name, "notification channel unusable: {why}");
                    broken.push((name, why));
                }
            }
        }
        let status = Mutex::new(vec![Delivery::default(); targets.len()]);
        Notifier {
            http: reqwest::Client::new(),
            targets,
            indexer,
            public_url,
            status,
            broken,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty() && self.broken.is_empty()
    }

    pub async fn announce(&self, modules: &[&str]) {
        let m = Message {
            label: "WATCHING".into(),
            title: "graph-sentinel started".into(),
            body: format!(
                "Watching {}. You will hear from it here when something needs attention.",
                modules.join(" and ")
            ),
            severity: Severity::Critical, // reaches every channel, whatever its threshold
            resolved: false,
        };
        for i in 0..self.targets.len() {
            self.deliver(i, &m, None).await;
        }
    }

    pub async fn send(&self, events: &[Event]) {
        for e in events {
            tracing::info!(
                "{} {}: {}",
                message(e).label,
                e.finding.kind,
                e.finding.summary
            );
        }
        for (i, t) in self.targets.iter().enumerate() {
            // Same threshold for a resolution, so it lands wherever its finding did.
            let mine: Vec<Event> = events
                .iter()
                .filter(|e| e.finding.severity >= t.channel.min_severity)
                .cloned()
                .collect();
            if mine.len() > DIGEST_OVER {
                self.deliver(i, &digest(&mine), Some(&mine)).await;
            } else {
                for e in &mine {
                    self.deliver(i, &message(e), Some(std::slice::from_ref(e)))
                        .await;
                }
            }
        }
    }

    async fn deliver(&self, i: usize, m: &Message, events: Option<&[Event]>) {
        let t = &self.targets[i];
        let url = self.public_url.as_deref();
        let body = match t.channel.kind {
            ChannelKind::Telegram => json!({
                "chat_id": t.channel.chat_id,
                "text": telegram_text(m, &self.indexer, url),
                "parse_mode": "HTML",
                "disable_web_page_preview": true,
            }),
            ChannelKind::Discord => discord_body(m, &self.indexer, url, Utc::now()),
            ChannelKind::Slack => json!({ "text": slack_text(m, &self.indexer, url) }),
            ChannelKind::Generic => json!({
                "label": m.label, "title": m.title, "summary": m.body,
                "indexer": self.indexer, "dashboard": url, "events": events.unwrap_or_default(),
            }),
        };
        let r = self
            .http
            .post(&t.endpoint)
            .timeout(std::time::Duration::from_secs(10))
            .json(&body)
            .send()
            .await
            .and_then(|r| r.error_for_status());
        let mut st = self.status.lock().expect("status lock");
        match r {
            Ok(_) => {
                st[i].sent += 1;
                st[i].last_ok = Some(Utc::now());
                st[i].last_error = None;
            }
            Err(e) => {
                // Never log or show the endpoint: it carries the token or the webhook secret.
                let e = e.without_url().to_string();
                tracing::warn!(channel = %t.name, error = %e, "notification failed");
                st[i].last_error = Some(e);
            }
        }
    }

    /// For the dashboard's sources list.
    pub fn statuses(&self) -> Vec<(String, Result<String, String>)> {
        let st = self.status.lock().expect("status lock");
        let mut out: Vec<_> = self
            .targets
            .iter()
            .zip(st.iter())
            .map(|(t, d)| {
                let floor = format!("{} and above", t.channel.min_severity);
                let r = match (&d.last_error, d.last_ok) {
                    (Some(e), _) => Err(format!("last delivery failed: {e}")),
                    (None, Some(at)) => Ok(format!(
                        "{} sent, last {}; {floor}",
                        d.sent,
                        at.format("%-d %b %H:%M UTC")
                    )),
                    (None, None) => Ok(format!("nothing sent yet; {floor}")),
                };
                (t.name.clone(), r)
            })
            .collect();
        out.extend(
            self.broken
                .iter()
                .map(|(n, why)| (n.clone(), Err(format!("not configured: {why}")))),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_types::Finding;

    fn ev(kind: &str, sev: Severity, t: Transition) -> Event {
        Event {
            transition: t,
            finding: Finding::new(
                "reo",
                kind,
                "indexer",
                sev,
                "At 7 deployments a day the oracle run on 27 Oct fails. Serve 3 more a day to clear it.",
                json!({}),
                Utc::now(),
            ),
        }
    }

    const IDX: &str = "0xb43b2cccceada5292732a8c58ae134adefce09bb";

    #[test]
    fn telegram_reads_on_a_phone_and_escapes() {
        let m = message(&ev("reo_at_risk", Severity::Warn, Transition::New));
        let t = telegram_text(&m, IDX, Some("https://sentinel.example/?a=1&b=2"));
        assert!(t.starts_with("<b>WARN</b> · REO eligibility at risk\nAt 7 deployments"));
        assert!(t.contains("<code>0xb43b…09bb</code>"));
        assert!(t.contains("href=\"https://sentinel.example/?a=1&amp;b=2\""));
        let evil = Message {
            body: "<script>".into(),
            ..m
        };
        assert!(telegram_text(&evil, IDX, None).contains("&lt;script&gt;"));
    }

    #[test]
    fn resolution_is_green_and_says_what_cleared() {
        let m = message(&ev(
            "reo_ineligible",
            Severity::Critical,
            Transition::Resolved,
        ));
        assert_eq!(m.label, "RESOLVED");
        assert!(m.body.starts_with("Cleared. It was: "));
        let d = discord_body(&m, IDX, None, Utc::now());
        assert_eq!(d["embeds"][0]["color"], 0x86b592);
        assert_eq!(d["allowed_mentions"]["parse"], json!([]));
    }

    #[test]
    fn many_events_become_one_digest() {
        let evs: Vec<Event> = (0..7)
            .map(|_| ev("reo_at_risk", Severity::Warn, Transition::New))
            .collect();
        let d = digest(&evs);
        assert_eq!(d.title, "7 changes");
        assert_eq!(d.body.lines().count(), 7);
    }

    #[test]
    fn unknown_kinds_get_a_readable_title() {
        assert_eq!(title("tap_rav_stalled"), "Tap rav stalled");
    }

    #[test]
    fn misconfigured_channels_are_reported_not_dropped() {
        let c: Channel =
            serde_json::from_value(json!({ "kind": "telegram", "chat_id": "1" })).unwrap();
        let n = Notifier::new(&[c], IDX.into(), None);
        let s = n.statuses();
        assert_eq!(s[0].0, "telegram");
        assert!(s[0].1.as_ref().unwrap_err().contains("no bot token"));
    }
}
