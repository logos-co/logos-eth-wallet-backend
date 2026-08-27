//! The fixed token table: native ETH plus WETH, per network. No user-added tokens.
//!
//! A wrong address here spends into a contract that is not the one the user meant, so an
//! address enters this table only when it has been verified against an authoritative source
//! AND confirmed on-chain. `None` means "not verified yet" and the network then offers ETH
//! only — never a guess.

use serde::{Deserialize, Serialize};

use crate::networks;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Token {
    pub symbol: String,
    pub name: String,
    pub decimals: u8,
    /// `None` for the chain's native currency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    pub native: bool,
}

/// Verified: present in this workspace at `uniswap-module/rust-lib/src/config.rs` and
/// confirmed as the canonical mainnet WETH9 deployment.
const WETH_MAINNET: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";

/// MUST-VERIFY before any release that offers WETH on these networks. Several unrelated
/// WETH deployments exist on Sepolia and none is canonical; Hoodi is not surveyed at all.
/// Verify against an authoritative source, confirm `symbol()`/`decimals()` on-chain through
/// the configured endpoint, and have the transcription checked by a second person.
const WETH_SEPOLIA: Option<&str> = None;
const WETH_HOODI: Option<&str> = None;

fn native(symbol: &str) -> Token {
    Token {
        symbol: symbol.to_string(),
        name: "Ether".to_string(),
        decimals: 18,
        address: None,
        native: true,
    }
}

fn weth(address: &str) -> Token {
    Token {
        symbol: "WETH".to_string(),
        name: "Wrapped Ether".to_string(),
        decimals: 18,
        address: Some(address.to_string()),
        native: false,
    }
}

fn weth_for(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        1 => Some(WETH_MAINNET),
        11_155_111 => WETH_SEPOLIA,
        560_048 => WETH_HOODI,
        _ => None,
    }
}

/// The tokens offered on `chain_id`, native first. Empty for an unsupported network.
pub fn for_chain(chain_id: u64) -> Vec<Token> {
    let Some(net) = networks::by_chain_id(chain_id) else {
        return Vec::new();
    };
    let mut out = vec![native(net.native_symbol)];
    if let Some(addr) = weth_for(chain_id) {
        out.push(weth(addr));
    }
    out
}

/// Look up a token on `chain_id` by symbol (case-insensitive) or by contract address.
pub fn find(chain_id: u64, key: &str) -> Option<Token> {
    let k = key.trim();
    for t in for_chain(chain_id) {
        if t.symbol.eq_ignore_ascii_case(k) {
            return Some(t);
        }
        if let Some(a) = &t.address {
            if a.eq_ignore_ascii_case(k) {
                return Some(t);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    #[test]
    fn every_address_in_the_table_is_a_valid_eip55_checksum() {
        for net in networks::ALL {
            for t in for_chain(net.chain_id) {
                let Some(a) = t.address else { continue };
                // parse_checksummed rejects a mistyped or wrongly-cased address, which is
                // the transcription error this test exists to catch.
                let parsed = Address::parse_checksummed(&a, None)
                    .unwrap_or_else(|e| panic!("{} {} on chain {}: {e}", t.symbol, a, net.chain_id));
                assert_ne!(parsed, Address::ZERO, "{} is the zero address", t.symbol);
            }
        }
    }

    #[test]
    fn symbols_and_addresses_are_unique_within_a_network() {
        for net in networks::ALL {
            let list = for_chain(net.chain_id);
            let mut symbols: Vec<String> = list.iter().map(|t| t.symbol.to_lowercase()).collect();
            symbols.sort();
            symbols.dedup();
            assert_eq!(symbols.len(), list.len(), "duplicate symbol on {}", net.key);

            let mut addrs: Vec<String> =
                list.iter().filter_map(|t| t.address.clone()).map(|a| a.to_lowercase()).collect();
            let n = addrs.len();
            addrs.sort();
            addrs.dedup();
            assert_eq!(addrs.len(), n, "duplicate address on {}", net.key);
        }
    }

    #[test]
    fn every_network_offers_exactly_one_native_token_first() {
        for net in networks::ALL {
            let list = for_chain(net.chain_id);
            assert!(list[0].native, "{}: native token must be first", net.key);
            assert_eq!(list.iter().filter(|t| t.native).count(), 1);
            assert!(list.iter().filter(|t| !t.native).all(|t| t.address.is_some()));
        }
    }

    #[test]
    fn mainnet_offers_weth_and_the_unverified_testnets_do_not() {
        assert!(find(1, "WETH").is_some());
        assert!(find(1, WETH_MAINNET).is_some(), "lookup by address must work");
        // Deliberate: an unverified address is absent, not guessed. Flip these when
        // the addresses are verified and the table is filled in.
        assert!(find(11_155_111, "WETH").is_none());
        assert!(find(560_048, "WETH").is_none());
    }

    #[test]
    fn an_unsupported_network_offers_nothing() {
        assert!(for_chain(999).is_empty());
        assert!(find(999, "ETH").is_none());
    }
}
