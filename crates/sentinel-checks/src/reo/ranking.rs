use std::cmp::Reverse;

use sentinel_types::DeploymentId;
use serde::Serialize;

/// A deployment the indexer is not currently serving, with what the ranking needs to know.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub deployment: DeploymentId,
    pub network: Option<String>,
    pub signal_grt: f64,
    pub query_fees_grt: f64,
    pub active_allocations: u32,
    /// graph-node already holds this deployment, so serving it costs no sync.
    pub synced_locally: bool,
    /// graph-node is configured for this chain, so it needs no new RPC.
    pub chain_supported: bool,
    pub rewards_denied: bool,
}

/// Order the deployments to add, cheapest and likeliest to receive queries first.
/// A deployment that has never earned a query fee is unlikely to be routed any queries,
/// however cheap it is to sync, so demand outranks competition.
pub fn rank(mut candidates: Vec<Candidate>, min_signal_grt: f64) -> Vec<Candidate> {
    candidates.retain(|c| !c.rewards_denied && c.signal_grt >= min_signal_grt);
    candidates.sort_by(|a, b| {
        let key = |c: &Candidate| {
            (
                Reverse(c.synced_locally),
                Reverse(c.chain_supported),
                Reverse(c.query_fees_grt > 0.0),
                c.active_allocations,
            )
        };
        key(a)
            .cmp(&key(b))
            .then(b.query_fees_grt.total_cmp(&a.query_fees_grt))
            .then(b.signal_grt.total_cmp(&a.signal_grt))
    });
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: char, synced: bool, chain: bool, fees: f64, allocs: u32) -> Candidate {
        Candidate {
            deployment: format!("Qm{}", id.to_string().repeat(44)).parse().unwrap(),
            network: Some("arbitrum-one".into()),
            signal_grt: 1_000.0,
            query_fees_grt: fees,
            active_allocations: allocs,
            synced_locally: synced,
            chain_supported: chain,
            rewards_denied: false,
        }
    }

    #[test]
    fn local_then_chain_then_demand_then_competition() {
        let ranked = rank(
            vec![
                c('a', false, false, 50.0, 1),
                c('b', false, true, 0.0, 1),
                c('c', false, true, 10.0, 9),
                c('d', false, true, 10.0, 2),
                c('e', true, false, 0.0, 40),
            ],
            0.0,
        );
        let order: String = ranked
            .iter()
            .map(|c| c.deployment.as_str().as_bytes()[2] as char)
            .collect();
        assert_eq!(order, "edcba");
    }

    #[test]
    fn denied_and_low_signal_are_dropped() {
        let mut denied = c('a', true, true, 1.0, 0);
        denied.rewards_denied = true;
        let mut thin = c('b', true, true, 1.0, 0);
        thin.signal_grt = 10.0;
        assert!(rank(vec![denied, thin], 500.0).is_empty());
    }
}
