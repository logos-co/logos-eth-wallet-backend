//! Aggregate call budgets.
//!
//! A per-call timeout bounds one round-trip. A method that makes eight of them is bounded
//! only by their SUM, which is what the user actually waits: `list_networks` held ten
//! outbound calls, ~120s of them at the 20s protocol default and still ~29s with every call
//! individually capped. A `Budget` is ONE allowance shared by every call on one entry point.
//!
//! The arithmetic lives here, clock-free, so `cargo test --no-default-features` covers it.

use std::time::{Duration, Instant};

/// Per-call caps. Reading a dependency's config or its metadata is local work; writing one
/// is a file write plus, for token_list, a ~1ms parse — 5s is slack, not a working number.
pub const PROBE_BUDGET: Duration = Duration::from_millis(1500);
pub const INIT_BUDGET: Duration = Duration::from_secs(5);

/// One `token_list` catalogue read. Local work like the two above, but a mainnet list is
/// ~86 KB of JSON to serialize, ship and parse — an order of magnitude more than a probe.
pub const CATALOGUE_BUDGET: Duration = Duration::from_secs(3);

/// One `token_list` offered-rows read: local, and only the pinned and enabled rows, so a small
/// slice in front of the call that consumes them.
pub const OFFERED_BUDGET: Duration = Duration::from_secs(1);

/// One JSON-RPC round trip through `eth_rpc`, or one keystore read. Unlike the two above
/// this crosses a network, so 3s is a working number rather than slack.
pub const RPC_BUDGET: Duration = Duration::from_secs(3);

/// The deadline to hand a callee that this caller will wait `transport` for.
///
/// Shorter than the transport bound on purpose. The two clocks start together, so a callee
/// given the SAME budget finishes exactly as the caller stops listening, and its error
/// sentence -- the one naming which endpoint failed and why -- is lost to a bare timeout.
/// The margin buys the reply its trip home.
///
/// None when there is not enough left to be worth bounding: below `MIN_SLICE` the callee
/// would spend its whole allowance failing, so it is better told nothing and left to its own
/// default than handed a deadline it cannot meet.
pub fn callee_deadline(transport: Duration) -> Option<i64> {
    transport
        .checked_sub(CALLEE_MARGIN)
        .filter(|d| *d >= MIN_SLICE)
        .map(|d| d.as_millis() as i64)
}

const CALLEE_MARGIN: Duration = Duration::from_millis(300);

/// The aggregates. `READ_BUDGET` covers a whole consumer-facing read including the lazy
/// dependency retry in front of it; `STARTUP_BUDGET` covers everything the load hook may
/// spend seeding dependencies before it hands control back to the host.
pub const READ_BUDGET: Duration = Duration::from_secs(4);
pub const STARTUP_BUDGET: Duration = Duration::from_secs(6);

/// A send's outbound work: validate the requested chain against the registry, read the
/// offered tokens, delegate one transfer build, then ask `tx_sender_module` to price or submit.
pub const SEND_BUDGET: Duration = Duration::from_secs(16);

/// The one delegated call of a send: `tx_sender_module.prepare` or `.send`, which itself runs
/// a gate, a fee estimate, a balance and a nonce read, and the approval request. Its own
/// allowance is handed down as `deadlineMs` so its error sentence comes home rather than a
/// bare transport timeout.
pub const SENDER_BUDGET: Duration = Duration::from_secs(9);

/// Resolve and build one transfer in `evm_assets_module`. Together with the registry read,
/// the offered read and the sender allowance this exactly fits the public send budget; the
/// callee gets a slightly shorter deadline so its structured error still has time to return.
pub const ASSETS_BUDGET: Duration = Duration::from_millis(4_500);

/// One relayed `send_status`: the sender reads the approval, collects the signatures, gates,
/// and broadcasts — the broadcast itself deliberately unbounded on its side. Under the
/// protocol's 20s default so a slow node reports a reason rather than a bare timeout.
pub const STATUS_BUDGET: Duration = Duration::from_secs(18);

/// One relayed history read or receipt sweep: the sender's own sweep allowance is 10s.
pub const HISTORY_BUDGET: Duration = Duration::from_secs(12);

/// One chain's `get_balances`: its offered rows, then the lazy eth_rpc retry, the verified
/// gate, and the single Multicall3 read that answers every row. The gate is INSIDE it — an
/// unbounded probe in front of a read is time a user waits that no budget can see — and it is
/// sized so the gate and the read both fit, because a balance list cannot degrade the way a
/// network row can. evm_assets reserves up to fourteen seconds for the chain record plus a
/// verified Multicall3 proof; the offered read goes first and always leaves it that much.
/// Together with registry discovery the public call still fits the UI's 20s transport bound.
pub const BALANCES_BUDGET: Duration = Duration::from_secs(15);

/// One `suggest_fees`: the verified gate and one `fee_module` estimate.
pub const FEES_BUDGET: Duration = Duration::from_secs(5);

/// One `verified_proxy_state`: the verdict probe alone. A report rather than a gate — here
/// the verdict IS the answer — but a chip polling every five seconds must not be able to
/// outlast its own interval.
pub const VERDICT_BUDGET: Duration = Duration::from_secs(2);

/// One relayed `tx_details` (the sender's own allowance is 8s) and one relayed
/// `refresh_tx_status` (5s), each on a button.
pub const DETAILS_BUDGET: Duration = Duration::from_secs(9);
pub const REFRESH_BUDGET: Duration = Duration::from_secs(6);

/// Below this a grant buys nothing, and the protocol ABI refuses a sub-millisecond bound
/// outright. The last sliver of an allowance goes on answering, not on one more call.
pub const MIN_SLICE: Duration = Duration::from_millis(50);

/// A shrinking allowance shared by every outbound call on one entry point.
pub struct Budget {
    started: Instant,
    total: Duration,
}

impl Budget {
    pub fn new(total: Duration) -> Self {
        Self { started: Instant::now(), total }
    }

    /// What the next call may spend, or `None` once too little is left to be worth a
    /// round-trip. Charged against real elapsed time, so a call that returned early costs
    /// what it took rather than what it was granted.
    pub fn take(&self, per_call: Duration) -> Option<Duration> {
        slice(self.total, self.started.elapsed(), per_call)
    }
}

/// The whole policy, clock-free. A grant never exceeds what is left, so the grants of any
/// sequence of calls sum to at most `total`.
pub fn slice(total: Duration, elapsed: Duration, per_call: Duration) -> Option<Duration> {
    let grant = total.checked_sub(elapsed)?.min(per_call);
    (grant >= MIN_SLICE).then_some(grant)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worst case `list_networks` presents: the lazy eth_rpc retry (three chain seeds
    /// plus `init_defaults`), then three networks × (verdict + endpoint).
    fn list_networks_calls() -> Vec<Duration> {
        let mut c = vec![INIT_BUDGET; 4];
        c.extend([PROBE_BUDGET; 6]);
        c
    }

    /// Spend every grant in full and report what the caller waited.
    fn walk(total: Duration, calls: &[Duration]) -> Duration {
        let mut spent = Duration::ZERO;
        for cap in calls {
            match slice(total, spent, *cap) {
                Some(g) => spent += g,
                None => break,
            }
        }
        spent
    }

    #[test]
    fn a_read_is_bounded_by_its_total_and_not_by_its_call_count() {
        let calls = list_networks_calls();
        let per_call_only: Duration = calls.iter().sum();
        // If this ever stops holding the aggregate is decoration, not a bound.
        assert!(per_call_only > READ_BUDGET, "the aggregate must bind: {per_call_only:?}");
        assert!(walk(READ_BUDGET, &calls) <= READ_BUDGET);
    }

    /// A send: the gate, the offered tokens, the transfer build with its token balance read,
    /// then the one delegated call to the sender. The delegated call gets the lion's share —
    /// it is where the fee, the nonce and the approval happen — and it must fit AFTER the
    /// three calls in front of it.
    #[test]
    fn a_send_is_bounded_across_its_reads_and_the_delegated_call() {
        let calls = [PROBE_BUDGET, OFFERED_BUDGET, ASSETS_BUDGET, SENDER_BUDGET];
        assert!(walk(SEND_BUDGET, &calls) <= SEND_BUDGET);
        assert!(calls.iter().sum::<Duration>() <= SEND_BUDGET, "every call must fit");
        assert!(calls.iter().all(|c| slice(SEND_BUDGET, Duration::ZERO, *c).is_some()));
        // What the sender is handed is what is left after the reads, less the margin that
        // brings its reply home — and that is still a working allowance.
        let handed = callee_deadline(SENDER_BUDGET).expect("worth bounding");
        assert!(handed >= 8_000, "the sender needs room for four calls of its own: {handed}ms");
        assert!(SEND_BUDGET < Duration::from_secs(20), "inside the view's 20s transport");
    }

    /// Per chain the offered read goes first and evm_assets gets the rest, which must cover
    /// the fourteen seconds it bounds its own balance read at.
    #[test]
    fn a_chains_offered_read_leaves_the_assets_read_its_own_bound() {
        let assets_own = Duration::from_secs(14);
        let left = slice(BALANCES_BUDGET, OFFERED_BUDGET, BALANCES_BUDGET).expect("time left");
        assert!(left >= assets_own, "{left:?}");
    }

    /// The relays sit under the protocol's 20s default, so a slow sender reports its own
    /// reason rather than being cut off by the transport.
    #[test]
    fn every_relay_answers_inside_the_transport_default() {
        for total in [STATUS_BUDGET, HISTORY_BUDGET, DETAILS_BUDGET, REFRESH_BUDGET] {
            assert!(total < Duration::from_secs(20), "{total:?}");
            assert!(slice(total, Duration::ZERO, total).is_some());
        }
    }

    /// The four gate sites that ran the UNBOUNDED probe. `get_balances` was the worst:
    /// `READ_BUDGET`, then up to 20s of gate, then an untimed Multicall3 on top.
    #[test]
    fn a_balance_read_is_bounded_across_its_gate_and_its_multicall() {
        let mut calls = vec![INIT_BUDGET; 4];
        calls.extend([PROBE_BUDGET, RPC_BUDGET]);
        assert!(calls.iter().sum::<Duration>() > BALANCES_BUDGET, "the aggregate must bind");
        assert!(walk(BALANCES_BUDGET, &calls) <= BALANCES_BUDGET);
        // The two calls that actually answer must BOTH fit, gate included, or a wallet whose
        // dependency is merely slow reports no balances at all.
        assert!(PROBE_BUDGET + RPC_BUDGET <= BALANCES_BUDGET);
    }

    #[test]
    fn a_portfolio_fans_out_without_multiplying_the_interaction_deadline() {
        // Registry discovery is one bounded phase, followed by any number of concurrent
        // per-chain reads. Adding a chain adds work, but not another serial allowance.
        assert!(READ_BUDGET + BALANCES_BUDGET < Duration::from_secs(20));
    }

    #[test]
    fn the_two_one_call_paths_are_bounded_across_their_gates_too() {
        for (total, calls) in
            [(FEES_BUDGET, vec![PROBE_BUDGET, RPC_BUDGET]), (VERDICT_BUDGET, vec![PROBE_BUDGET])]
        {
            assert!(walk(total, &calls) <= total);
            assert!(calls.iter().sum::<Duration>() <= total, "the gate must fit too");
            assert!(calls.iter().all(|c| slice(total, Duration::ZERO, *c).is_some()));
        }
    }

    /// F-4. The gate is INSIDE the aggregate. It used to run before `Budget::new` at the
    /// protocol's 20s default, so the worst case was 20+3+3 against a UI deadline of 20s:
    /// the transport gave up first and the view blamed a backend that had not answered.
    #[test]
    fn a_details_read_is_bounded_across_the_gate_in_front_of_it() {
        let calls = [PROBE_BUDGET, RPC_BUDGET, RPC_BUDGET];
        assert!(walk(DETAILS_BUDGET, &calls) <= DETAILS_BUDGET);
        // Long enough to make every call it needs, gate included, or the budget is the bug:
        // a details read that runs out mid-leg is a button that never works.
        assert!(calls.iter().sum::<Duration>() <= DETAILS_BUDGET, "the gate must fit too");
        assert!(calls.iter().all(|c| slice(DETAILS_BUDGET, Duration::ZERO, *c).is_some()));
    }

    /// The same for the other button: the gate, then one receipt read.
    #[test]
    fn a_refresh_is_bounded_across_the_gate_in_front_of_it() {
        let calls = [PROBE_BUDGET, RPC_BUDGET];
        assert!(walk(REFRESH_BUDGET, &calls) <= REFRESH_BUDGET);
        assert!(calls.iter().sum::<Duration>() <= REFRESH_BUDGET, "the gate must fit too");
        assert!(calls.iter().all(|c| slice(REFRESH_BUDGET, Duration::ZERO, *c).is_some()));
    }

    #[test]
    fn the_load_hook_is_bounded_too() {
        // ensure_eth_rpc (3 seeds + init) then ensure_token_list (config_status + init).
        let calls =
            [INIT_BUDGET, INIT_BUDGET, INIT_BUDGET, INIT_BUDGET, PROBE_BUDGET, INIT_BUDGET];
        assert!(calls.iter().sum::<Duration>() > STARTUP_BUDGET);
        assert!(walk(STARTUP_BUDGET, &calls) <= STARTUP_BUDGET);
    }

    #[test]
    fn no_ordering_or_number_of_calls_can_outlast_the_total() {
        let mut calls = list_networks_calls();
        calls.reverse();
        assert!(walk(READ_BUDGET, &calls) <= READ_BUDGET);
        calls.extend(vec![MIN_SLICE; 500]);
        assert!(walk(READ_BUDGET, &calls) <= READ_BUDGET);
    }

    #[test]
    fn a_spent_budget_grants_nothing_so_the_sequence_terminates() {
        let mut spent = Duration::ZERO;
        let mut granted: u128 = 0;
        for _ in 0..10_000 {
            let Some(g) = slice(READ_BUDGET, spent, MIN_SLICE) else { break };
            spent += g;
            granted += 1;
        }
        assert_eq!(granted, READ_BUDGET.as_millis() / MIN_SLICE.as_millis());
        assert_eq!(slice(READ_BUDGET, READ_BUDGET, PROBE_BUDGET), None);
        assert_eq!(slice(READ_BUDGET, READ_BUDGET + PROBE_BUDGET, PROBE_BUDGET), None);
    }

    #[test]
    fn a_grant_never_exceeds_the_per_call_cap_or_what_is_left() {
        assert_eq!(slice(READ_BUDGET, Duration::ZERO, PROBE_BUDGET), Some(PROBE_BUDGET));
        assert_eq!(
            slice(READ_BUDGET, Duration::from_secs(3), INIT_BUDGET),
            Some(Duration::from_secs(1))
        );
        let sliver = READ_BUDGET - Duration::from_millis(10);
        assert_eq!(slice(READ_BUDGET, sliver, PROBE_BUDGET), None);
        assert!(matches!(slice(READ_BUDGET, READ_BUDGET - MIN_SLICE, PROBE_BUDGET),
                         Some(g) if g >= MIN_SLICE));
    }

    #[test]
    fn the_clock_backs_the_allowance() {
        let b = Budget::new(READ_BUDGET);
        assert_eq!(b.take(PROBE_BUDGET), Some(PROBE_BUDGET));
        assert_eq!(Budget::new(Duration::ZERO).take(PROBE_BUDGET), None);
    }
}
