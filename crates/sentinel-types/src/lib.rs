use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Critical => "critical",
        })
    }
}

impl FromStr for Severity {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "info" => Ok(Severity::Info),
            "warn" => Ok(Severity::Warn),
            "critical" => Ok(Severity::Critical),
            other => Err(Error::Parse(format!("unknown severity {other:?}"))),
        }
    }
}

/// What a check reports. Dedupe is by `fingerprint`, so it must be stable across ticks
/// for the same underlying condition and must not include the severity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub check: String,
    pub kind: String,
    pub subject: String,
    pub severity: Severity,
    pub fingerprint: String,
    pub summary: String,
    pub evidence: serde_json::Value,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

impl Finding {
    pub fn new(
        check: &str,
        kind: &str,
        subject: impl Into<String>,
        severity: Severity,
        summary: impl Into<String>,
        evidence: serde_json::Value,
        now: DateTime<Utc>,
    ) -> Self {
        let subject = subject.into();
        Finding {
            fingerprint: format!("{check}:{kind}:{subject}"),
            check: check.to_owned(),
            kind: kind.to_owned(),
            subject,
            severity,
            summary: summary.into(),
            evidence,
            first_seen: now,
            last_seen: now,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Parse(String),
}

/// A subgraph deployment's IPFS hash, `Qm…`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeploymentId(String);

impl DeploymentId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for DeploymentId {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        let ok = s.len() == 46
            && s.starts_with("Qm")
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() && !b"0OIl".contains(&b));
        if ok {
            Ok(DeploymentId(s.to_owned()))
        } else {
            Err(Error::Parse(format!("not a deployment id: {s:?}")))
        }
    }
}

impl fmt::Display for DeploymentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A 20-byte address (allocation, indexer, sender), held as lower-case hex without `0x`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Address(String);

impl Address {
    /// Horizon receipts carry a 32-byte collection id: the allocation id, zero-padded on the left.
    pub fn from_collection_id(s: &str) -> Result<Self, Error> {
        let s = s.trim().trim_start_matches("0x");
        if s.len() != 64 || !s[..24].bytes().all(|b| b == b'0') {
            return Err(Error::Parse(format!(
                "not an allocation collection id: {s:?}"
            )));
        }
        s[24..].parse()
    }

    pub fn hex(&self) -> &str {
        &self.0
    }
}

impl FromStr for Address {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        let h = s.trim().trim_start_matches("0x").to_ascii_lowercase();
        if h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Address(h))
        } else {
            Err(Error::Parse(format!("not an address: {s:?}")))
        }
    }
}

impl TryFrom<String> for Address {
    type Error = Error;
    fn try_from(s: String) -> Result<Self, Error> {
        s.parse()
    }
}

impl From<Address> for String {
    fn from(a: Address) -> String {
        format!("0x{}", a.0)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{}", self.0)
    }
}

/// The UTC calendar day a nanosecond timestamp falls on, which is how the oracle partitions.
pub fn utc_day_of_ns(timestamp_ns: i128) -> Option<NaiveDate> {
    let secs = i64::try_from(timestamp_ns.div_euclid(1_000_000_000)).ok()?;
    DateTime::from_timestamp(secs, 0).map(|t| t.date_naive())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_id_maps_to_allocation() {
        let a = Address::from_collection_id(
            "0000000000000000000000003e1f9c2ab4c7f1b3d7e839ebe6ae451c8a0b1d24",
        )
        .unwrap();
        assert_eq!(a.to_string(), "0x3e1f9c2ab4c7f1b3d7e839ebe6ae451c8a0b1d24");
        assert!(
            Address::from_collection_id(
                "1000000000000000000000003e1f9c2ab4c7f1b3d7e839ebe6ae451c8a0b1d24"
            )
            .is_err()
        );
    }

    #[test]
    fn address_normalises_case_and_prefix() {
        let a: Address = "0xB43B2CCCceadA5292732a8C58ae134AdEFcE09Bb"
            .parse()
            .unwrap();
        let b: Address = "b43b2cccceada5292732a8c58ae134adefce09bb".parse().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn deployment_id_validates() {
        assert!(
            "QmZA6BHHHiFGZAawSw4UVPjEBFT87Z3wu3nc7K7rgEZpeV"
                .parse::<DeploymentId>()
                .is_ok()
        );
        assert!(
            "QmZA6BHHHiFGZAawSw4UVPjEBFT87Z3wu3nc7K7rgEZpe"
                .parse::<DeploymentId>()
                .is_err()
        );
        assert!("0xdeadbeef".parse::<DeploymentId>().is_err());
    }

    #[test]
    fn utc_day_boundaries() {
        // 2026-10-27T00:00:00Z = 1793059200
        let d = |s: i128| utc_day_of_ns(s * 1_000_000_000).unwrap().to_string();
        assert_eq!(d(1_793_059_200), "2026-10-27");
        assert_eq!(d(1_793_059_199), "2026-10-26");
    }
}
