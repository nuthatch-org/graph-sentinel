//! What a deployment's manifest says it should react to, reduced to what an `eth_getLogs`
//! activity probe needs.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use tiny_keccak::{Hasher, Keccak};

/// The probe's view of a manifest. `probe_complete` is false when logs alone cannot say
/// whether the deployment had work: block or call handlers, templates, or non-EVM sources.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub network: Option<String>,
    /// Lower-case hex, `0x`-prefixed.
    pub addresses: BTreeSet<String>,
    /// `0x`-prefixed keccak hashes.
    pub topics: BTreeSet<String>,
    pub block_handlers: bool,
    pub call_handlers: bool,
    pub templates: bool,
    pub non_evm: bool,
}

impl Activity {
    pub fn probe_complete(&self) -> bool {
        !self.block_handlers && !self.call_handlers && !self.templates && !self.non_evm
    }

    /// Every block is work, so a non-moving head is never idle.
    pub fn always_active(&self) -> bool {
        self.block_handlers
    }

    /// Logs can be probed at all: EVM sources with at least one static address and topic.
    pub fn probeable(&self) -> bool {
        !self.non_evm && !self.addresses.is_empty() && !self.topics.is_empty()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    #[serde(default)]
    data_sources: Vec<DataSource>,
    #[serde(default)]
    templates: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DataSource {
    kind: String,
    network: Option<String>,
    source: Option<Source>,
    mapping: Option<Mapping>,
}

#[derive(Deserialize)]
struct Source {
    address: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Mapping {
    #[serde(default)]
    event_handlers: Vec<EventHandler>,
    #[serde(default)]
    block_handlers: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    call_handlers: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct EventHandler {
    event: String,
    topic0: Option<String>,
}

/// graph-node's rule (chain/ethereum/src/data_source.rs `topic0()`): an explicit `topic0`,
/// else keccak of the event signature with every `"indexed "` removed.
pub fn topic0(event: &str, explicit: Option<&str>) -> String {
    if let Some(t) = explicit {
        return t.to_ascii_lowercase();
    }
    let mut k = Keccak::v256();
    k.update(event.replace("indexed ", "").as_bytes());
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    format!("0x{}", hex::encode(out))
}

fn is_evm(kind: &str) -> bool {
    matches!(kind, "ethereum" | "ethereum/contract")
}

pub fn parse(yaml: &str) -> Result<Activity, String> {
    let m: Manifest = serde_saphyr::from_str(yaml).map_err(|e| e.to_string())?;
    let mut a = Activity {
        templates: !m.templates.is_empty(),
        ..Activity::default()
    };
    for ds in m.data_sources {
        if a.network.is_none() {
            a.network = ds.network.clone();
        }
        if !is_evm(&ds.kind) {
            a.non_evm = true;
            continue;
        }
        if let Some(addr) = ds.source.and_then(|s| s.address) {
            a.addresses.insert(addr.to_ascii_lowercase());
        }
        if let Some(map) = ds.mapping {
            a.block_handlers |= !map.block_handlers.is_empty();
            a.call_handlers |= !map.call_handlers.is_empty();
            for h in map.event_handlers {
                a.topics.insert(topic0(&h.event, h.topic0.as_deref()));
            }
        }
    }
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic0_matches_graph_node() {
        // keccak("Transfer(address,address,uint256)"), the ERC-20 Transfer topic.
        assert_eq!(
            topic0("Transfer(indexed address,indexed address,uint256)", None),
            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
        );
        assert_eq!(topic0("X()", Some("0xABCD")), "0xabcd");
    }

    const MANIFEST: &str = r#"
specVersion: 0.0.5
schema:
  file: { /: /ipfs/QmSchema }
dataSources:
  - kind: ethereum/contract
    name: Token
    network: arbitrum-one
    source:
      address: "0x9623063377AD1B27544C965cCd7342f7EA7e88C7"
      abi: Token
      startBlock: 42
    mapping:
      kind: ethereum/events
      apiVersion: 0.0.7
      language: wasm/assemblyscript
      entities: [Transfer]
      abis: [{ name: Token, file: { /: /ipfs/QmAbi } }]
      eventHandlers:
        - event: Transfer(indexed address,indexed address,uint256)
          handler: handleTransfer
        - event: Approval(indexed address,indexed address,uint256)
          handler: handleApproval
      file: { /: /ipfs/QmMapping }
"#;

    #[test]
    fn plain_event_manifest_is_fully_probeable() {
        let a = parse(MANIFEST).unwrap();
        assert_eq!(a.network.as_deref(), Some("arbitrum-one"));
        assert_eq!(a.addresses.len(), 1);
        assert!(
            a.addresses
                .contains("0x9623063377ad1b27544c965ccd7342f7ea7e88c7")
        );
        assert_eq!(a.topics.len(), 2);
        assert!(a.probe_complete() && a.probeable() && !a.always_active());
    }

    #[test]
    fn templates_and_block_handlers_make_the_probe_incomplete() {
        let y = MANIFEST.replace(
            "      file: { /: /ipfs/QmMapping }",
            "      blockHandlers:\n        - handler: handleBlock\n      file: { /: /ipfs/QmMapping }",
        ) + "templates:\n  - kind: ethereum/contract\n    name: Pair\n";
        let a = parse(&y).unwrap();
        assert!(a.block_handlers && a.templates && a.always_active());
        assert!(!a.probe_complete());
    }

    #[test]
    fn non_evm_and_garbage() {
        let a = parse("dataSources:\n  - kind: substreams\n    network: mainnet\n").unwrap();
        assert!(a.non_evm && !a.probeable());
        assert!(parse(": : :\n- [").is_err());
    }
}
