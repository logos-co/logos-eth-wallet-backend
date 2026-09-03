//! The three Ethereum networks this wallet can be pointed at, exactly one at a time.
//!
//! The set matches `verified_proxy_module`'s `networkProfiles()` whitelist, so every
//! selectable network is light-client-verifiable. Adding a fourth here without adding it
//! there gives the user a verified-proxy toggle that cannot work.
//!
//! There is deliberately no `explorer` field. Nothing ever fetched or opened one, so its
//! removal closes no live leak — it removes the loaded gun a live explorer URL leaves for
//! whoever next adds a "view on explorer" button that would disclose the user's IP.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Network {
    pub chain_id: u64,
    /// Matches the verified proxy's `network` enum, which it validates strictly.
    pub key: &'static str,
    pub name: &'static str,
    pub native_symbol: &'static str,
    pub testnet: bool,
}

pub const MAINNET: Network = Network {
    chain_id: 1,
    key: "mainnet",
    name: "Ethereum",
    native_symbol: "ETH",
    testnet: false,
};

pub const SEPOLIA: Network = Network {
    chain_id: 11_155_111,
    key: "sepolia",
    name: "Sepolia",
    native_symbol: "ETH",
    testnet: true,
};

// Multicall3 IS confirmed present here — its bytecode is byte-identical to mainnet's
// (sha256 0fb6a9db…) and aggregate3 answers live — so the balance path works on all three.
pub const HOODI: Network = Network {
    chain_id: 560_048,
    key: "hoodi",
    name: "Hoodi",
    native_symbol: "ETH",
    testnet: true,
};

pub const ALL: [Network; 3] = [MAINNET, SEPOLIA, HOODI];

pub const DEFAULT_CHAIN_ID: u64 = MAINNET.chain_id;

pub fn by_chain_id(chain_id: u64) -> Option<Network> {
    ALL.into_iter().find(|n| n.chain_id == chain_id)
}

pub fn is_supported(chain_id: u64) -> bool {
    by_chain_id(chain_id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_networks_are_distinct_and_resolvable() {
        for n in ALL {
            assert_eq!(by_chain_id(n.chain_id).unwrap().key, n.key);
            assert!(!n.key.is_empty() && !n.name.is_empty());
        }
        let mut ids: Vec<u64> = ALL.iter().map(|n| n.chain_id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ALL.len(), "duplicate chain id in the network table");
    }

    #[test]
    fn only_mainnet_is_not_a_testnet() {
        assert!(!MAINNET.testnet);
        assert!(SEPOLIA.testnet && HOODI.testnet);
    }

    #[test]
    fn an_unknown_chain_is_refused() {
        assert!(!is_supported(999));
        assert!(!is_supported(10), "an L2 must not resolve — this wallet is Ethereum only");
        assert!(is_supported(DEFAULT_CHAIN_ID));
    }
}
