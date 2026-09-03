//! Kill the process at every point in the broadcast sequence, then ask the next one for a
//! nonce.
//!
//! "The process died" is simulated the only way a test can: the `SendLedger` is dropped and a
//! new one is built over the SAME directory, exactly as `on_context_ready` does. Nothing
//! in-memory survives — the directory is the whole of the evidence, which is the point.
//!
//! Two directions, and both matter. Once the signed transaction may have left, no restart may
//! ever hand its number out again, or two transactions carry the same nonce and one of them
//! is silently lost. Before it could have left, the number must come BACK, or every crash
//! costs a gap and a gap stops every later send from mining.

use std::path::Path;

use eth_wallet_backend::history::{History, TxRecord};
use eth_wallet_backend::send::{BroadcastClaim, SendJob, SendLedger, SendStatus};
use eth_wallet_backend::sweep::{history_reply, SweepOutcome};

const CHAIN: u64 = 1;
const ACC: &str = "0xF39fd6E51Aad88f6f4CE6Ab8827279cFFfB92266";
const ID: &str = "snd_1";
const HASH: &str = "0xdead";
/// What the chain reports at `latest`, on every call in this file. It does not move: a
/// broadcast that has not mined does not count, which is why the reserver exists at all.
const LATEST: u64 = 5;

/// How far the send got before the process died.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Died {
    BeforeTheClaim,
    AfterTheClaim,
    /// The signed transaction is on the wire and nothing has come back. The window the
    /// record used to be written after.
    InsideTheBroadcast,
    AfterTheHashLanded,
    AfterTheHashWasSettled,
    AfterTheErrorLanded,
    AfterTheErrorWasSettled,
}

use Died::*;

/// Everything past `InsideTheBroadcast`: the bytes have left the process.
const MAY_HAVE_LEFT: [Died; 5] = [
    InsideTheBroadcast,
    AfterTheHashLanded,
    AfterTheHashWasSettled,
    AfterTheErrorLanded,
    AfterTheErrorWasSettled,
];

fn job(nonce: u64) -> SendJob {
    SendJob {
        request_id: ID.into(),
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

fn row(nonce: u64) -> TxRecord {
    TxRecord {
        chain_id: CHAIN,
        from: ACC.into(),
        to: "0xbbbb".into(),
        value: "1".into(),
        kind: "native".into(),
        timestamp: 1_000,
        nonce: Some(nonce),
        ..Default::default()
    }
}

/// One process's send, stopped dead at `died`. Answers the nonce it took.
///
/// The order is `advance_send`'s: claim, record, broadcast, outcome, settle. Nothing is
/// carried out of here — the ledger and the history handle both drop with the function.
fn a_send_that_died(dir: &Path, died: Died) -> u64 {
    let h = History::new(dir.to_path_buf());
    let l = SendLedger::default();
    l.seed_spent(h.unsettled_nonces());

    let guard = l.open(CHAIN, ACC, LATEST, None, || Ok(CHAIN)).expect("the send is priced");
    let nonce = guard.claim().nonce;
    if died == BeforeTheClaim {
        return nonce;
    }
    guard.commit(job(nonce));
    let BroadcastClaim::Claimed(t) = l.claim_broadcast(ID, 0) else {
        panic!("nothing else has claimed this send")
    };
    if died == AfterTheClaim {
        return nonce;
    }

    let proof = h.record_intent(ID, row(nonce)).expect("the intent must land, or nothing sends");
    if died == InsideTheBroadcast {
        return nonce;
    }

    if died == AfterTheHashLanded || died == AfterTheHashWasSettled {
        h.resolve_broadcast(&proof, HASH, 1_000);
        if died == AfterTheHashLanded {
            return nonce;
        }
        let done = SendStatus::Broadcast { hash: HASH.into(), route: "proxied".into() };
        l.settle_owned(&t, done);
    } else {
        h.leave_unknown(&proof, "connection reset");
        if died == AfterTheErrorLanded {
            return nonce;
        }
        l.settle_owned(&t, SendStatus::Failed { reason: "connection reset".into() });
    }
    nonce
}

/// What the next process hands the next send, reading nothing but the directory.
fn the_next_process_hands_out(dir: &Path) -> u64 {
    let h = History::new(dir.to_path_buf());
    let l = SendLedger::default();
    l.seed_spent(h.unsettled_nonces());
    let g = l.open(CHAIN, ACC, LATEST, None, || Ok(CHAIN)).expect("the send is priced");
    g.claim().nonce
}

#[test]
fn no_restart_hands_out_a_number_whose_transaction_may_have_left() {
    for died in MAY_HAVE_LEFT {
        let dir = tempfile::tempdir().unwrap();
        let took = a_send_that_died(dir.path(), died);
        assert_eq!(took, LATEST, "{died:?}: the send took the number the chain offered");

        let next = the_next_process_hands_out(dir.path());
        assert_ne!(next, took, "{died:?}: that transaction may be on chain");
        assert_eq!(next, LATEST + 1, "{died:?}");
        // Not once: `latest` never moves for a transaction that never mines, so the number
        // has to stay burnt for every process that follows, not just the first.
        assert_eq!(the_next_process_hands_out(dir.path()), LATEST + 1, "{died:?}: still burnt");
    }
}

/// The other direction. Before the record there is nothing on the wire, so the number must
/// come back — a crash that costs a nonce costs a gap, and a gap stops the account dead.
#[test]
fn a_death_before_anything_could_leave_gives_the_number_back() {
    for died in [BeforeTheClaim, AfterTheClaim] {
        let dir = tempfile::tempdir().unwrap();
        let took = a_send_that_died(dir.path(), died);
        assert_eq!(
            the_next_process_hands_out(dir.path()),
            took,
            "{died:?}: nothing was broadcast, so nothing may be lost"
        );
    }
}

/// The row is what carries the number across, so it has to say which nonce, on which chain,
/// for which account — an intent with no nonce burns nothing at all.
#[test]
fn the_carried_over_row_names_the_number_the_next_process_burns() {
    let dir = tempfile::tempdir().unwrap();
    a_send_that_died(dir.path(), InsideTheBroadcast);

    let h = History::new(dir.path().to_path_buf());
    assert_eq!(h.unsettled_nonces(), vec![(CHAIN, ACC.to_string(), LATEST)]);
    let row = h.list(ACC).remove(0);
    assert_eq!((row.status.as_str(), row.hash.as_str()), ("unknown", ""));
    assert_eq!((row.chain_id, row.nonce), (CHAIN, Some(LATEST)));
}

/// THE WEDGE, end to end. A number left behind by a send that never came back is burnt in
/// every process that follows, and because a gap stops everything behind it from mining, the
/// account goes nowhere until that number is used. The escape is a replacement send with it
/// pinned — against a ledger SEEDED from the record, which is the case that used to refuse —
/// and the user can only pin what the reply told them.
#[test]
fn the_number_a_dead_send_left_behind_can_be_pinned_by_the_next_process() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(a_send_that_died(dir.path(), InsideTheBroadcast), LATEST);

    let h = History::new(dir.path().to_path_buf());
    let l = SendLedger::default();
    assert_eq!(l.seed_spent(h.unsettled_nonces()), 1, "the dead send's number is carried over");

    let reply = history_reply(ACC, CHAIN, &h.list(ACC), 2_000, &SweepOutcome::default(), &[]);
    let told = reply["unresolved"][0]["nonce"].as_u64().expect("the reply names the number");
    assert_eq!(told, LATEST);
    assert!(reply["unresolved"][0]["message"].as_str().unwrap().contains("pinned"));

    let g = l.open(CHAIN, ACC, LATEST, Some(told), || Ok(CHAIN)).expect("the escape must work");
    assert_eq!(g.claim().nonce, told, "the replacement takes the very number that is stuck");
}

/// F-4 crossed with F-1: the account file cannot be read at the moment the intent is written.
/// `add` used to return without writing anything, so the send was recorded nowhere and the
/// next process handed its number straight out.
#[cfg(unix)]
#[test]
fn a_record_stranded_by_an_unreadable_file_still_burns_the_number() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let h = History::new(dir.path().to_path_buf());
    assert!(h.add(ACC, TxRecord { status: "confirmed".into(), ..row(4) }));

    let p = dir.path().join("history").join("f39fd6e51aad88f6f4ce6ab8827279cfffb92266.json");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();

    assert_eq!(a_send_that_died(dir.path(), InsideTheBroadcast), LATEST);
    assert_eq!(
        the_next_process_hands_out(dir.path()),
        LATEST + 1,
        "the intent was stranded beside a file nobody could read, and it still counts"
    );
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
}
