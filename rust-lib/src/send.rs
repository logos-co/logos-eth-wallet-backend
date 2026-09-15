//! What the wallet still decides about a send now that `tx_sender_module` moves the money:
//! which token, how much, whether the account holds it, and what the human is told. The
//! nonce, the fee ceiling, the approval and the broadcast are the sender's.

use alloy::primitives::U256;

use crate::units;

/// Two spellings of one account: an optional `0x` and letter case are no part of it.
pub fn same_account(a: &str, b: &str) -> bool {
    // Lowercased FIRST, so a `0X` prefix is stripped like a `0x` one.
    let k = |s: &str| s.trim().to_lowercase().trim_start_matches("0x").to_string();
    k(a) == k(b)
}

/// Whether the account holds enough of an ERC-20 to send `value`. The sender checks ether
/// against the fee; a token it does not know is this wallet's to check, or an over-large
/// transfer is approved, broadcast, reverts on chain and burns the gas.
pub fn token_affordable(
    balance: U256,
    value: U256,
    symbol: &str,
    decimals: u8,
) -> Result<(), String> {
    if balance >= value {
        return Ok(());
    }
    let amount = |v: U256| {
        units::format_exact(&v.to_string(), decimals).unwrap_or_else(|| v.to_string())
    };
    Err(format!(
        "insufficient {symbol}: this send needs {} {symbol}, and the account holds {} {symbol}",
        amount(value),
        amount(balance)
    ))
}

/// What the requester CLAIMS a signature is for, in one line.
///
/// The signer prints this under a heading saying it cannot check any of it, and prints the
/// keystore's own reading of the same transaction directly below. So it names both ends, in the
/// same checksummed form those lines use: a claim a human can compare against the reading is
/// worth more than one they have to take on trust. Neither address is shortened — an elided
/// claim cannot be compared character for character with a full reading. The sender appends
/// who asked.
pub fn purpose(amount: &str, symbol: &str, from: &str, to: &str) -> String {
    format!("Send {amount} {symbol} from {from} to {to}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROM: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    const TO: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    #[test]
    fn the_claim_names_the_amount_the_token_and_both_ends() {
        assert_eq!(
            purpose("1", "ETH", FROM, TO),
            format!("Send 1 ETH from {FROM} to {TO}")
        );
    }

    #[test]
    fn both_addresses_survive_verbatim() {
        let s = purpose("0.0001", "DAI", FROM, TO);
        assert!(s.contains(FROM), "{s}");
        assert!(s.contains(TO), "{s}");
        assert!(!s.contains('…'), "{s}");
    }

    #[test]
    fn an_exact_amount_is_carried_through_unrounded() {
        let s = purpose("0.000000000000000001", "ETH", FROM, TO);
        assert!(s.starts_with("Send 0.000000000000000001 ETH from "), "{s}");
    }

    #[test]
    fn an_over_large_token_send_is_refused_in_token_units() {
        let two = U256::from(2_000_000_000_000_000_000u64);
        let half = U256::from(500_000_000_000_000_000u64);
        assert!(token_affordable(two, half, "WETH", 18).is_ok());
        let e = token_affordable(half, two, "WETH", 18).unwrap_err();
        assert_eq!(
            e,
            "insufficient WETH: this send needs 2 WETH, and the account holds 0.5 WETH"
        );
    }

    #[test]
    fn a_token_send_of_exactly_the_balance_is_allowed() {
        let all = U256::from(1_000_000u64);
        assert!(token_affordable(all, all, "USDC", 6).is_ok());
        let e = token_affordable(all, all + U256::from(1), "USDC", 6).unwrap_err();
        assert!(e.contains("needs 1.000001 USDC") && e.contains("holds 1 USDC"), "{e}");
    }

    #[test]
    fn an_account_is_the_same_whatever_its_spelling() {
        assert!(same_account("0xAbC", "abc"));
        assert!(same_account(" 0xabc ", "0XABC"));
        assert!(!same_account("0xabc", "0xabd"));
    }
}
