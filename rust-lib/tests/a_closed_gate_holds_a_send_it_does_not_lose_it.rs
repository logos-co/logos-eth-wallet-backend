//! Where a send LANDS when the verified gate closes between `send` and the poll that would
//! have broadcast it.
//!
//! The window is minutes wide: `send` passes the gate, a human takes their time in the
//! signer, and by the time the poll comes back with a signature the proxy may be unusable or
//! the mode may have flipped to `required`. `advance_send` refuses there — but the refusal is
//! the delicate part, because by then the keystore has handed back a signature over ONE
//! nonce, and this wallet's whole send design is a write-ahead record plus a held number.
//!
//! So the refusal touches the ledger not at all: no claim, no record, no settle. The job
//! stays `awaitingApproval` with its nonce reserved, which is what the four tests below name
//! one property at a time — including the one that shows what settling it `Failed` instead
//! would have cost.
//!
//! `glue.rs` is behind the `logos_module` feature and is read as source elsewhere. What is
//! driven here is `SendLedger` itself, which `cargo test` does compile: the refusal is
//! modelled as what it is — the ledger being left alone.

use eth_wallet_backend::send::{BroadcastClaim, SendJob, SendLedger, SendStatus};

const CHAIN: u64 = 1;
const ACC: &str = "0xF39fd6E51Aad88f6f4CE6Ab8827279cFFfB92266";
const ID: &str = "snd_1";
/// What the chain reports at `latest`. It does not move: a broadcast that has not mined does
/// not count, which is why a nonce is reserved at all.
const LATEST: u64 = 5;
const NOW: u64 = 1_700_000_000;

fn job(request_id: &str, nonce: u64) -> SendJob {
    SendJob {
        request_id: request_id.into(),
        handle: "ksh_1".into(),
        receipt: "ksc_1".into(),
        chain_id: CHAIN,
        from: ACC.into(),
        to: "0xbbbb".into(),
        value: "1".into(),
        kind: "native".into(),
        token: None,
        nonce,
        gas_limit: 21_000,
        max_fee: "1".into(),
        max_priority: "1".into(),
        token_symbol: None,
        token_decimals: None,
        tx_input: Some("0x".into()),
        status: SendStatus::AwaitingApproval,
        broadcast: None,
        replaces: None,
    }
}

/// One send priced, its nonce reserved and its job committed — `request_send` in full.
fn requested(l: &SendLedger, request_id: &str) -> u64 {
    let g = l.open(CHAIN, ACC, LATEST, None, || Ok(CHAIN)).expect("a claim");
    let nonce = g.claim().nonce;
    g.commit(job(request_id, nonce));
    nonce
}

/// The state itself, named. All three halves are load-bearing: non-terminal so a poll comes
/// back, unclaimed so `cancel_send` is still open, reserved so nothing else takes the number
/// the signature in hand is over.
#[test]
fn a_send_the_gate_refused_is_still_a_live_send() {
    let l = SendLedger::default();
    assert_eq!(requested(&l, ID), LATEST);

    // The refusal: `advance_send` returns having touched nothing.
    let j = l.get(ID).expect("the job is still there");
    assert_eq!(j.reported_status(NOW), "awaitingApproval");
    assert!(!j.broadcast_started(), "nothing was claimed, so nothing may have left");
    assert!(!j.status.is_terminal(), "a closed gate is not an outcome");
    assert_eq!(l.outstanding(CHAIN, ACC), 1, "the number the signature is over stays held");
}

/// Why it must not be reported as a failure, in the only terms that matter. The signature the
/// keystore handed back is over THIS nonce and the send can still go out on a later poll, so
/// a `Failed` here gives 5 away while a transaction signed at 5 is still waiting to leave.
#[test]
fn failing_the_send_would_hand_its_nonce_to_the_next_one() {
    let l = SendLedger::default();
    assert_eq!(requested(&l, ID), LATEST);

    let reason = "the verified proxy is not usable".to_string();
    l.settle(ID, SendStatus::Failed { reason }).expect("the job");
    assert_eq!(l.outstanding(CHAIN, ACC), 0, "a failed send lets go of its number");
    assert_eq!(requested(&l, "snd_2"), LATEST, "and the next send takes the very same one");
}

/// The resumption. The proxy comes back, the next poll re-reads the approval, re-fetches the
/// signature and claims the broadcast — on the same number, because nothing let go of it. A
/// send started in the meantime queued behind it rather than colliding with it.
#[test]
fn the_next_poll_sends_it_once_the_gate_reopens() {
    let l = SendLedger::default();
    assert_eq!(requested(&l, ID), LATEST);
    assert_eq!(requested(&l, "snd_2"), LATEST + 1);

    let BroadcastClaim::Claimed(t) = l.claim_broadcast(ID, NOW) else {
        panic!("a held send must still be claimable once the gate reopens")
    };
    let s = l
        .settle_owned(&t, SendStatus::Broadcast { hash: "0xdead".into(), route: "proxied".into() })
        .expect("the ticket owns this job");
    assert!(s.changed);
    assert_eq!(s.job.nonce, LATEST, "the number it was signed at, not a re-quoted one");
    assert_eq!(l.outstanding(CHAIN, ACC), 2, "burnt rather than released, for ever");
}

/// And the way out while it is held. Nothing claimed the broadcast, so cancelling is still
/// open — which is what stops a proxy that never comes back from wedging the account, and is
/// the door a `Failed` would have closed by settling the job behind the user's back.
#[test]
fn a_held_send_can_still_be_cancelled() {
    let l = SendLedger::default();
    assert_eq!(requested(&l, ID), LATEST);

    let j = l.claim_cancel(ID).expect("a send that has not broadcast is cancellable");
    assert_eq!(j.status, SendStatus::Cancelled);
    assert_eq!(l.outstanding(CHAIN, ACC), 0, "and the number goes back, because nothing left");
}
