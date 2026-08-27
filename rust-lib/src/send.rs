//! The Send state machine and nonce reservation — the parts that must be correct without a
//! network. Everything here is pure and unit-tested; `glue.rs` does the I/O around it.

use std::collections::{BTreeSet, HashMap};

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};

/// Where a send has got to. A job is created `AwaitingApproval` and reaches exactly one
/// terminal state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum SendStatus {
    AwaitingApproval,
    /// Approved and handed to the chain. `hash` is the broadcast result.
    Broadcast { hash: String },
    Rejected,
    Cancelled,
    Failed { reason: String },
}

impl SendStatus {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, SendStatus::AwaitingApproval)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendJob {
    pub request_id: String,
    pub handle: String,
    pub receipt: String,
    pub chain_id: u64,
    pub from: String,
    pub to: String,
    /// Decimal wei (native) or token base units (erc20).
    pub value: String,
    /// "native" | "erc20".
    pub kind: String,
    pub token: Option<String>,
    pub nonce: u64,
    pub status: SendStatus,
    /// Set before the broadcast RPC leaves, so a concurrent poll cannot send it twice.
    /// A crash between this and the reply leaves the job `Failed`, never re-broadcast:
    /// a duplicate spend is worse than a lost status.
    pub broadcast_started: bool,
}

impl SendJob {
    /// Claim the right to broadcast. Returns false if anyone already claimed it.
    pub fn claim_broadcast(&mut self) -> bool {
        if self.broadcast_started || self.status.is_terminal() {
            return false;
        }
        self.broadcast_started = true;
        true
    }
}

/// Nonces handed out but not yet mined.
///
/// Load-bearing rather than a nicety: the verified proxy refuses the `pending` block tag
/// (a light client proves against a header's stateRoot, and pending has none), so the nonce
/// comes from `latest`, which does not count a broadcast-but-unmined transaction. Without
/// this, two sends in quick succession collide and the second is silently lost.
#[derive(Default)]
pub struct NonceReserver {
    reserved: HashMap<(u64, String), BTreeSet<u64>>,
}

fn key(chain_id: u64, address: &str) -> (u64, String) {
    (chain_id, address.trim().trim_start_matches("0x").to_lowercase())
}

impl NonceReserver {
    /// The next nonce to use, given what the chain reports at `latest`.
    ///
    /// Reservations below `chain_nonce` are dropped first: once the chain has caught up they
    /// are stale, and keeping them would push the account's nonce ever further ahead.
    pub fn reserve(&mut self, chain_id: u64, address: &str, chain_nonce: u64) -> u64 {
        let set = self.reserved.entry(key(chain_id, address)).or_default();
        set.retain(|n| *n >= chain_nonce);
        let mut candidate = chain_nonce;
        while set.contains(&candidate) {
            candidate += 1;
        }
        set.insert(candidate);
        candidate
    }

    /// Give a nonce back — the send was rejected, cancelled or failed before broadcast.
    pub fn release(&mut self, chain_id: u64, address: &str, nonce: u64) {
        if let Some(set) = self.reserved.get_mut(&key(chain_id, address)) {
            set.remove(&nonce);
            if set.is_empty() {
                self.reserved.remove(&key(chain_id, address));
            }
        }
    }

    pub fn outstanding(&self, chain_id: u64, address: &str) -> usize {
        self.reserved.get(&key(chain_id, address)).map(|s| s.len()).unwrap_or(0)
    }
}

/// The worst-case cost of a transaction: the value moved plus the fee ceiling.
///
/// `max_fee_per_gas` is a ceiling, not a price — the user is never charged more, so this is
/// what their balance must cover for the transaction to be includable.
pub fn max_cost_wei(value_wei: U256, gas_limit: u64, max_fee_per_gas: U256) -> Option<U256> {
    let fee = max_fee_per_gas.checked_mul(U256::from(gas_limit))?;
    value_wei.checked_add(fee)
}

/// Whether `balance` covers a native send. An ERC-20 send moves no ether, so only the fee
/// is charged against the native balance — the token balance is checked separately.
pub fn affordable(
    balance_wei: U256,
    value_wei: U256,
    gas_limit: u64,
    max_fee_per_gas: U256,
    native: bool,
) -> Result<(), String> {
    let charged = if native { value_wei } else { U256::ZERO };
    let Some(total) = max_cost_wei(charged, gas_limit, max_fee_per_gas) else {
        return Err("fee calculation overflowed".into());
    };
    if balance_wei < total {
        return Err(format!(
            "insufficient funds: need {total} wei (value plus the fee ceiling), balance is {balance_wei} wei"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> SendJob {
        SendJob {
            request_id: "req_1".into(),
            handle: "ksh_1".into(),
            receipt: "ksc_1".into(),
            chain_id: 1,
            from: "0xaaaa".into(),
            to: "0xbbbb".into(),
            value: "1".into(),
            kind: "native".into(),
            token: None,
            nonce: 7,
            status: SendStatus::AwaitingApproval,
            broadcast_started: false,
        }
    }

    #[test]
    fn broadcast_can_be_claimed_exactly_once() {
        let mut j = job();
        assert!(j.claim_broadcast(), "the first claim wins");
        assert!(!j.claim_broadcast(), "a concurrent poll must not broadcast again");
    }

    #[test]
    fn a_settled_job_can_never_be_broadcast() {
        let mut j = job();
        j.status = SendStatus::Rejected;
        assert!(!j.claim_broadcast());

        let mut j = job();
        j.status = SendStatus::Broadcast { hash: "0xdead".into() };
        assert!(!j.claim_broadcast());
    }

    #[test]
    fn consecutive_sends_get_consecutive_nonces_from_the_same_chain_view() {
        let mut r = NonceReserver::default();
        // The chain still reports 5 for the second send: `latest` does not count the first,
        // which is exactly the case this exists for.
        assert_eq!(r.reserve(1, "0xAbC", 5), 5);
        assert_eq!(r.reserve(1, "0xabc", 5), 6);
        assert_eq!(r.reserve(1, "0xABC", 5), 7);
        assert_eq!(r.outstanding(1, "abc"), 3, "the address key is case- and prefix-insensitive");
    }

    #[test]
    fn a_released_nonce_is_handed_out_again() {
        let mut r = NonceReserver::default();
        assert_eq!(r.reserve(1, "0xa", 5), 5);
        assert_eq!(r.reserve(1, "0xa", 5), 6);
        r.release(1, "0xa", 5);
        assert_eq!(r.reserve(1, "0xa", 5), 5, "the gap is reused, not skipped");
    }

    #[test]
    fn reservations_the_chain_has_caught_up_with_are_dropped() {
        let mut r = NonceReserver::default();
        r.reserve(1, "0xa", 5);
        r.reserve(1, "0xa", 5);
        // Both mined; the chain now reports 7.
        assert_eq!(r.reserve(1, "0xa", 7), 7);
        assert_eq!(r.outstanding(1, "0xa"), 1, "stale reservations must not accumulate");
    }

    #[test]
    fn reservations_do_not_leak_across_chains_or_accounts() {
        let mut r = NonceReserver::default();
        assert_eq!(r.reserve(1, "0xa", 5), 5);
        assert_eq!(r.reserve(11_155_111, "0xa", 5), 5, "a different chain is a different account");
        assert_eq!(r.reserve(1, "0xb", 5), 5, "a different account is independent");
    }

    #[test]
    fn max_cost_is_the_value_plus_the_fee_ceiling() {
        let c = max_cost_wei(U256::from(1000), 21_000, U256::from(2)).unwrap();
        assert_eq!(c, U256::from(1000 + 42_000));
        assert!(max_cost_wei(U256::MAX, 21_000, U256::MAX).is_none(), "overflow must not wrap");
    }

    #[test]
    fn affordability_charges_value_only_for_a_native_send() {
        let bal = U256::from(50_000);
        // native: 30_000 value + 21_000 fee > 50_000
        assert!(affordable(bal, U256::from(30_000), 21_000, U256::from(1), true).is_err());
        // erc20: the same "value" is tokens, so only the fee is charged against ether
        assert!(affordable(bal, U256::from(30_000), 21_000, U256::from(1), false).is_ok());
    }

    #[test]
    fn the_insufficient_funds_message_names_both_numbers() {
        let e = affordable(U256::from(1), U256::from(10), 21_000, U256::from(1), true).unwrap_err();
        assert!(e.contains("insufficient funds"), "{e}");
        assert!(e.contains("21010") && e.contains("balance is 1"), "{e}");
    }
}
