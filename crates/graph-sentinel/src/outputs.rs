use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use sentinel_checks::reo::Report;
use sentinel_types::Finding;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct DayLabel {
    day: String,
    method: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct CriteriaLabel {
    criteria: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct SourceLabel {
    source: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct FindingLabel {
    check: String,
    severity: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct BuildLabel {
    version: String,
}

pub struct Metrics {
    registry: Registry,
    qualifying_deployments: Family<DayLabel, Gauge>,
    qualifying_days: Family<CriteriaLabel, Gauge>,
    eligible: Family<SourceLabel, Gauge>,
    shortfall: Family<CriteriaLabel, Gauge>,
    days_remaining: Gauge,
    source_up: Family<SourceLabel, Gauge>,
    findings: Family<FindingLabel, Gauge>,
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("graph_sentinel");
        let m = Metrics {
            qualifying_deployments: Family::default(),
            qualifying_days: Family::default(),
            eligible: Family::default(),
            shortfall: Family::default(),
            days_remaining: Gauge::default(),
            source_up: Family::default(),
            findings: Family::default(),
            registry: Registry::default(),
        };
        registry.register(
            "reo_qualifying_deployments",
            "Distinct deployments served per UTC day (receipts method: an upper bound on the oracle's count)",
            m.qualifying_deployments.clone(),
        );
        registry.register(
            "reo_qualifying_days_28d",
            "Days in today's oracle window that meet the minimum",
            m.qualifying_days.clone(),
        );
        registry.register("reo_eligible", "1 if eligible", m.eligible.clone());
        registry.register(
            "reo_shortfall_deployments",
            "Extra deployments per day needed at the typical count",
            m.shortfall.clone(),
        );
        registry.register(
            "reo_days_remaining",
            "Days of eligibility left if traffic holds at the typical count",
            m.days_remaining.clone(),
        );
        registry.register(
            "source_up",
            "1 if the source answered on the last tick",
            m.source_up.clone(),
        );
        registry.register("findings", "Active findings", m.findings.clone());
        let build = Family::<BuildLabel, Gauge>::default();
        build
            .get_or_create(&BuildLabel {
                version: env!("CARGO_PKG_VERSION").into(),
            })
            .set(1);
        registry.register("build_info", "Build information", build);
        Metrics { registry, ..m }
    }

    pub fn reo(&self, r: &Report) {
        let day = |d: &str, v: u32| {
            self.qualifying_deployments
                .get_or_create(&DayLabel {
                    day: d.into(),
                    method: "receipts".into(),
                })
                .set(v as i64)
        };
        day("today", r.today_count);
        day("typical", r.typical_count);
        let crit = |c: &str| CriteriaLabel { criteria: c.into() };
        self.qualifying_days
            .get_or_create(&crit("current"))
            .set(r.qualifying_days_now as i64);
        if let Some(n) = r.qualifying_days_next {
            self.qualifying_days
                .get_or_create(&crit("next"))
                .set(n as i64);
        }
        self.shortfall
            .get_or_create(&crit("next"))
            .set(r.shortfall_next as i64);
        if let Some(c) = &r.on_chain {
            self.eligible
                .get_or_create(&SourceLabel {
                    source: "chain".into(),
                })
                .set(c.eligible as i64);
        }
        let projected = r.if_traffic_holds.runs.first().is_some_and(|v| v.passes);
        self.eligible
            .get_or_create(&SourceLabel {
                source: "projection".into(),
            })
            .set(projected as i64);
        self.days_remaining
            .set(crate::meter::days_left(r).unwrap_or(0));
    }

    pub fn source(&self, name: &str, up: bool) {
        self.source_up
            .get_or_create(&SourceLabel {
                source: name.into(),
            })
            .set(up as i64);
    }

    pub fn findings(&self, active: &[Finding]) {
        self.findings.clear();
        for f in active {
            self.findings
                .get_or_create(&FindingLabel {
                    check: f.check.clone(),
                    severity: f.severity.to_string(),
                })
                .inc();
        }
    }

    pub fn encode(&self) -> String {
        let mut s = String::new();
        encode(&mut s, &self.registry).expect("writing to a String cannot fail");
        s
    }
}
