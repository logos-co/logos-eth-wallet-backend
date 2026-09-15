//! Logos module glue for `eth_wallet_backend`.
//!
//! The builder derives the `.lidl` from the `EthWalletBackendModule` trait below
//! (`codegen.rust = { trait, source: "src/glue.rs" }`). Compiled only with the default
//! `logos_module` feature; `cargo test --no-default-features` exercises the pure cores.
//!
//! `concurrency: "multi"`: every read here is a blocking network round-trip through
//! `eth_rpc_module`, so the module opts into concurrent dispatch and one slow call cannot
//! stall the rest. The multi contract makes the generated trait take `&self` + `Send + Sync`,
//! so all state lives behind an `RwLock` — chosen at the first commit because a `&mut self`
//! module cannot be retrofitted onto multi later.
//!
//! Every transaction this wallet makes LEAVES through `tx_sender_module`, the one sender on
//! the device. This module decides WHAT to send — which token, how much, to whom, and what
//! the human is told — and the sender decides the nonce, the fee ceiling, the approval and
//! the broadcast, and keeps the record. What this module reads back it decorates with the
//! one thing the sender does not have: the token table.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use alloy::primitives::U256;
use serde_json::{json, Value};

use crate::budget::{
    callee_deadline, Budget, BALANCES_BUDGET, CATALOGUE_BUDGET, DETAILS_BUDGET, FEES_BUDGET,
    HISTORY_BUDGET, INIT_BUDGET, PROBE_BUDGET, READ_BUDGET, REFRESH_BUDGET, RPC_BUDGET,
    SENDER_BUDGET, SEND_BUDGET, STARTUP_BUDGET, STATUS_BUDGET, VERDICT_BUDGET,
};
use crate::contacts::ContactsStore;
use crate::depinit::{self, Next};
use crate::gate::{self, Gate};
use crate::rows;
use crate::send;
use crate::settings::{Settings, SettingsStore};
use crate::tokens::{Token, TokenSort};
use crate::txbuild::parse_u256_any;
use crate::verified::{self, unwrap_answer, Answer};
use crate::{networks, tokens, txbuild, units};

/// This module's own name, as the runtime attests it to the sender and the sender records
/// it on every row. What `get_history` and the network-switch refusal filter on.
const OWN_NAME: &str = "eth_wallet_backend";

pub trait EthWalletBackendModule: Send + Sync + 'static {
    /// The three selectable networks. `{ ok, activeChainId, networks: [{ chainId, key,
    /// name, nativeSymbol, testnet, rpcUrl, verifiedProxyMode, verifiedProxy }] }`.
    /// `rpcUrl`, `verifiedProxyMode` and `verifiedProxy` all come from `eth_rpc_module`, which
    /// owns them. All three are read-only here — a device-wide store shared with every wallet
    /// on the machine is configured in the `eth_rpc_ui` app, not from inside one wallet.
    ///
    /// Answers within a fixed budget however slow `eth_rpc` is. A network whose reads did
    /// not fit reports `verifiedProxyMode: "unknown"` and an empty `rpcUrl`; the active one
    /// is read first, so it is the last to degrade.
    fn list_networks(&self) -> String;

    /// The active network alone, in the same shape as one `list_networks` entry.
    fn get_active_network(&self) -> String;

    /// Switch the active network. Refuses any chain outside {1, 11155111, 560048} —
    /// this wallet is Ethereum only. `{ ok, activeChainId }`.
    ///
    /// Refused while a send THIS wallet made is awaiting a human: the request names the
    /// network it was built for, and moving the wallet under it would have the user approve
    /// for a chain the wallet no longer shows. Another app's pending send does not hold the
    /// wallet — the sender sends it on its own chain either way.
    ///
    /// Emits `active_chain_changed` when the chain MOVES. Re-selecting the network already
    /// active is a successful no-op and announces nothing.
    fn set_active_chain(&self, chain_id: i64) -> String;

    /// `eth_rpc`'s verified-proxy verdict for the active network: `{ ok, chainId, mode,
    /// state, usable, blocking, message, action, detail }`. Cheap: `off` costs no probe and
    /// a repeat inside eth_rpc's 5s TTL costs none either.
    fn verified_proxy_state(&self) -> String;

    /// Tokens OFFERED on the active network, native first: the built-in rows plus whatever
    /// the user turned on. `{ ok, chainId, tokenSort, tokens: [{ symbol, name, decimals,
    /// address?, native, builtin, inTokenList, metadataSource, logoURI? }] }`.
    ///
    /// `builtin` says whose assertion the address is — this wallet's fixed table, or a
    /// snapshot the user took from `token_list`. `metadataSource` says who decorated the row:
    /// `native` | `allowlist` (ours, undecorated) | `custom` | `downloaded` | `embedded`
    /// (`token_list`'s own bucket labels, relayed rather than inferred) | `unknown` (a match
    /// from a token_list too old to label its buckets) | `enabled` (a snapshot the list no
    /// longer holds). Neither flag can be derived from the other: a built-in row and an
    /// enabled one both read `embedded` when the same list decorates them.
    fn list_tokens(&self) -> String;

    /// Every token that COULD be offered on `chain_id`: what the wallet offers now, plus
    /// everything `token_list_module` holds for that chain. The token picker's one read.
    ///
    /// `{ ok, chainId, tokenSort, total, shown, listed, tokens: [{ symbol, name, decimals,
    /// address?, native, enabled, builtin, logoURI?, source }], listError? }`. `source` uses
    /// the same vocabulary as `list_tokens`'s `metadataSource`, and `builtin` is true for the
    /// native row and the verified WETH row — the two that cannot be turned off.
    ///
    /// `query` matches a symbol or name (case-insensitive substring) or an exact address; an
    /// empty query matches everything. The answer comes in pages: `offset` skips that many
    /// matches and `limit` caps the rest (zero or less is no limit). `total` counts every
    /// match, `shown` the rows in this page, and `hasMore` says whether another page follows,
    /// so a view loads the list as it scrolls instead of presenting a slice as the whole.
    ///
    /// The embedded Uniswap list is overwhelmingly mainnet, so on sepolia and hoodi `listed`
    /// is legitimately 0 and the reply carries the built-in rows alone. That is an ANSWER:
    /// `ok` stays true, and `listError` — present only when the `token_list` call itself
    /// failed — is what tells an empty catalogue from an unread one.
    fn list_available_tokens(&self, chain_id: i64, query: String, offset: i64, limit: i64) -> String;

    /// Turn a token on or off for `chain_id`. `{ ok }` or `{ ok: false, error }`.
    ///
    /// Enabling SNAPSHOTS the whole record from `token_list_module` and refuses an address it
    /// does not hold on that chain: `decimals` scales every amount this wallet renders or
    /// signs, and there is no honest way to invent one. Enabling a built-in row succeeds and
    /// stores nothing — it is already offered. Disabling one is refused outright: the native
    /// currency pays every fee and WETH is this wallet's own assertion, not the user's.
    ///
    /// The change persists, so an enabled token is still offered after a restart, and emits
    /// `tokens_changed(chain_id)` once it is on disk — but only when the offered set actually
    /// moved. Turning on a token already enabled with the same snapshot, or a built-in row,
    /// changes nothing and says nothing.
    fn set_token_enabled(&self, chain_id: i64, address: String, enabled: bool) -> String;

    /// The order `get_balances` returns its rows in — `alpha` or `balance`. `{ ok, tokenSort }`.
    ///
    /// `balance` orders by each token's OWN amount. This wallet has no fiat price and will
    /// not fetch one, because a price feed discloses the user's IP — so across two different
    /// tokens this is NOT a value order, and nothing rendering it may imply that it is.
    ///
    /// Device-wide, and emits `token_sort_changed` on a move: the same rows come back in a
    /// new order on every network at once, so there is no chain to scope it to.
    fn set_token_sort(&self, order: String) -> String;

    /// Accounts the keystore holds. Read-only: this module can never create, import or
    /// export one — those are the custodian's, and reach the keystore only via `evm_keystore_ui`.
    fn list_accounts(&self) -> String;

    /// Account names, `{ ok, labels: { "<lowercase hex, no 0x>": "<name>" } }`, relayed from
    /// the keystore verbatim. `keystore_module.get_labels` is ungated — a label is not a
    /// secret — and this passthrough keeps a view's dependency list at exactly one module.
    ///
    /// The keys are `vault_name` form and `list_accounts` answers EIP-55 checksummed
    /// addresses, so the two never match textually and a lookup must normalise.
    fn get_account_labels(&self) -> String;

    /// The WALLET each account was derived under, and where in it:
    /// `{ ok, wallets: { "<address>": { "wallet": "<name>", "index": <n> } } }`.
    ///
    /// Separate from `get_account_labels` because they are different things and a view must
    /// tell them apart: an account's own name identifies THAT account, a wallet's name is
    /// shared by every account under it. `index` is the DERIVATION index, straight off
    /// `m/44'/60'/0'/0/<index>`, and it is stable for the life of the account. Absent for an
    /// account whose wallet has no name, and `index` is absent for one that was imported
    /// rather than derived — both are ordinary, and an empty map is the normal state.
    fn get_account_wallets(&self) -> String;

    /// The address book: `{ ok, contacts: [{ address, name }] }`, named rows first and then
    /// unnamed, each ordered by name and address so a picker can show them without sorting
    /// and the order does not move when an unrelated contact is added.
    ///
    /// These are COUNTERPARTIES and live here rather than in the keystore, which names
    /// accounts it holds keys for. A contact carries no key material and is not a secret.
    fn list_contacts(&self) -> String;

    /// Add a contact, or rename one already there — an UPSERT, because a user who saves an
    /// address they already have meant to name it. The address is stored EIP-55 and matched
    /// case-insensitively. `{ ok, contact: { address, name } }`. An empty name is allowed.
    fn save_contact(&self, address: String, name: String) -> String;

    /// Forget a contact. Removing one that is not there SUCCEEDS — the caller's goal is that
    /// the address is not in the book, and that is already true.
    fn forget_contact(&self, address: String) -> String;

    /// Native and token balances for `address` on the active network, in one Multicall3
    /// round-trip. `{ ok, chainId, address, tokenSort, balances: [{ symbol, address?, raw,
    /// decimals, native, builtin, display, exact, amountExact }], route }`. `display` is
    /// bounded; `amountExact` carries every digit, as a plain decimal string (`exact` is the
    /// older name for the same digits). All three are absent when the sub-call failed, so a
    /// view renders an em-dash and never a zero. A caller must not scale `raw` itself — a JS
    /// number loses digits above 2^53.
    ///
    /// EVERY offered token gets a row, including one the account holds none of. The array
    /// arrives ALREADY SORTED by the persisted `tokenSort` — comparing 18-decimal amounts is
    /// exact `U256` work and belongs where it is testable, not in QML.
    ///
    /// `route` is `eth_rpc`'s own label for the read — `verified` (proof-backed), `proxied`
    /// (forwarded on trust), `direct` (never touched the proxy) or `unknown`. Badge the
    /// balances on `route`, never on the network's mode.
    fn get_balances(&self, address: String) -> String;

    /// Transactions `tx_sender_module` broadcast for `address` on the active network, newest
    /// first — this wallet's own sends and any other app's calls from the same account. Only
    /// transactions the sender broadcast: there is no indexer.
    ///
    /// `{ ok, chainId, address, stillDue, stillDueAnyChain, unstored, unresolved,
    /// blockedChains, strandedNonces, transactions }`. A row this wallet sent as an ERC-20
    /// transfer reads back as one: `kind: "erc20"`, `to` the recipient, `value` the token
    /// amount at the token's decimals, `txTo` the contract. Another app's call keeps
    /// `kind: "call"` with its `label`, `origin` and `purpose`. Each row carries `stalled`,
    /// `unresolved` and `verificationBlocked`; `stillDue` covers the rows in THIS reply.
    fn get_history(&self, address: String) -> String;

    /// Fee tiers for the active network, from `fee_module`. `{ ok, chainId, baseFeePerGas,
    /// source, tiers: { slow, normal, fast } }`; `source` distinguishes a real EIP-1559
    /// suggestion from the legacy `gasPrice` fallback.
    fn suggest_fees(&self) -> String;

    /// Quote a send without doing anything: resolves the token, checks an ERC-20 balance
    /// here, and has `tx_sender_module` price the fee, check the ether and read the nonce.
    ///
    /// `request_json`: `{ from, to, amount | amountUnits, token?, tokenAddress?, tier?,
    /// maxFeePerGas?, maxPriorityFeePerGas?, gasLimit?, nonce? }`. `amount` is base units,
    /// `amountUnits` is what the user typed in TOKEN units ("0.1" ETH, not 10^17 wei);
    /// exactly one of the two. `tokenAddress` names the contract exactly and wins over
    /// `token`, a symbol that is refused when two offered contracts share it. Any explicit
    /// fee field is used verbatim — the user overrules the suggestion, never the other way.
    ///
    /// Returns `{ ok, chainId, from, to, amount, amountDisplay, amountExact, amountSymbol,
    /// amountDecimals, nativeSymbol, token?, tokenAddress, nonce, gasLimit, maxFeePerGas,
    /// maxPriorityFeePerGas, maxCostWei(+Display/Exact), feeCeilingWei(+Display/Exact),
    /// feeSource, route, feeRoute }`. `feeCeilingWei` is `maxFeePerGas × gasLimit` — a
    /// ceiling, never a price, so a view must say "at most". No approval is requested and no
    /// nonce is reserved, so it is safe to call on every keystroke.
    fn prepare_send(&self, request_json: String) -> String;

    /// Ask a human to approve a send. Takes the same `request_json` as `prepare_send`.
    ///
    /// Returns `{ ok, pending: true, requestId, handle }` and **never a transaction hash** —
    /// nothing has been signed or broadcast at this point. `tx_sender_module` reserved the
    /// nonce and registered the approval; the human approves in `evm_signer_ui`; drive the
    /// rest with `send_status`. `handle` is the KEYSTORE's name for the approval record, for
    /// pointing a signer at this specific request.
    fn send(&self, request_json: String) -> String;

    /// Advance a pending send and report where it got to. Poll this — the sender broadcasts
    /// on this call, exactly once, and records the row before the transaction leaves.
    /// `{ ok, requestId, handle, status, hash?, hashes, route?, reason?, origin, purpose,
    /// legs }` where `status` is `awaitingApproval` | `broadcasting` | `stuck` | `broadcast`
    /// | `rejected` | `cancelled` | `failed`. A reply carrying `blocked: true` is a send being
    /// HELD by the verified-proxy gate, not a failed one: keep polling, or `cancel_send`.
    fn send_status(&self, request_id: String) -> String;

    /// Withdraw a send that has not been approved yet, releasing its reserved nonce.
    fn cancel_send(&self, request_id: String) -> String;

    /// Re-read one recorded transaction's receipt on ITS OWN chain and update the stored
    /// status. `{ ok, hash, chainId, status, route }` — `pending` | `confirmed` | `failed`.
    fn refresh_tx_status(&self, address: String, hash_hex: String) -> String;

    /// The transaction- and block-level fields a RECEIPT does not carry, for one recorded
    /// transaction on ITS OWN chain. `{ ok, hash, chainId, route, fetchedAt, gasPriceUnit,
    /// block?, transaction?, blockError?, transactionError? }`; `ok` is true when EITHER leg
    /// landed. Every reply names the `hash` it is about, refusals included.
    fn get_tx_details(&self, address: String, hash_hex: String) -> String;

    /// Poll receipts for this address's still-pending transactions, on each row's OWN chain,
    /// and update their stored status. `{ ok, address, polled, changed, blocked,
    /// blockedChains, stillDue }`. `stillDue` is false once no row can move again, which is
    /// when a caller's poll timer should stop.
    fn refresh_pending(&self, address: String) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

pub trait EthWalletBackendModuleEvents {
    /// A balance may have moved: a transaction this wallet is tracking settled. The address
    /// is EMPTY when the sender's event named only a hash — re-read what is on screen.
    fn balances_updated(&self, address: String);
    fn active_chain_changed(&self, chain_id: i64);
    /// Relayed from `tx_sender_module`: a recorded transaction took a hash or settled.
    fn tx_status_changed(&self, hash_hex: String);
    /// Relayed from `tx_sender_module`: a pending send changed state.
    fn send_status_changed(&self, request_id: String);
    /// The keystore's accounts moved — the set itself, or the names they are shown under.
    /// Relayed from `keystore_module::accounts_changed`; `count` is carried verbatim and is
    /// ADVISORY, not a change detector: a rename does not move it. Re-read both
    /// `list_accounts` and `get_account_labels` on it.
    fn accounts_changed(&self, count: i64);
    /// The set of tokens OFFERED on `chain_id` moved. Chain-scoped, so a wallet on another
    /// network can ignore it.
    fn tokens_changed(&self, chain_id: i64);
    /// The balance-row order moved. Device-wide and chainless.
    fn token_sort_changed(&self, order: String);
    /// Relayed from `tx_sender_module`: a recorded row for `address` appeared or changed with
    /// no hash for `tx_status_changed` to name.
    fn history_changed(&self, address: String);
    /// What `eth_rpc_module` reports for one chain moved — its endpoint, its transport, or its
    /// verified-proxy mode. Relayed from `eth_rpc_module`, which owns that record.
    fn networks_changed(&self, chain_id: i64);
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct EthWalletBackendImpl {
    /// Behind an `Arc` so a caller takes a HANDLE out of the guard and drops it, rather than
    /// borrowing through it. That is what makes holding it across an outbound call
    /// unexpressible: the state a method works on outlives the lock by construction.
    state: RwLock<Option<Arc<State>>>,
    deps: DepInit,
    /// Whether the keystore relay is armed. `on_context_ready` can run again, and a second
    /// listener thread would sit on a channel nothing closes for the life of the process.
    watching_keystore: AtomicBool,
    /// The same, for the three sender relays.
    watching_sender: AtomicBool,
    feeds: Feeds,
    /// Which chains may be gated without asking eth_rpc at all. Shared with the listener
    /// thread that keeps it honest; see [`crate::gate`] for why only `off` is ever held.
    gate: Arc<gate::ModeCache>,
}

/// The subscriptions this module keeps open on its dependencies. Each flag is held for as
/// long as its thread runs, so a feed that ends re-arms on the next read rather than going
/// quiet for the life of the process — unlike `watching_keystore`, which is armed once.
#[derive(Default)]
struct Feeds {
    gate: Arc<AtomicBool>,
    chains: Arc<AtomicBool>,
    tokens: Arc<AtomicBool>,
}

/// Run a subscription's listener thread, releasing `flag` when the feed ends.
fn listen<S: Send + 'static>(flag: Arc<AtomicBool>, sub: S, body: impl FnOnce(S) + Send + 'static) {
    std::thread::spawn(move || {
        body(sub);
        flag.store(false, Ordering::SeqCst);
    });
}

/// How many new subscriptions a lost gate feed takes before giving up and leaving it to the
/// next gated read, which arms one itself. Bounded so a provider that is gone for good is
/// not spun on.
const GATE_REARMS: u32 = 5;
const GATE_REARM_PAUSE: std::time::Duration = std::time::Duration::from_secs(2);

/// One arming of the mode feed: the status watcher first, then the subscription.
///
/// The watcher goes on first because the C side replays the current state synchronously from
/// inside the install, so no arm can fall into the gap ahead of it. `subscribed` is what
/// stops that replay opening the cache before there is a mode subscription for an arm to be
/// about — the status is per TARGET, so a client already armed for the chain-config feed
/// reports `Armed` the instant it is asked. Re-installing after the subscription exists is a
/// second replay and not a second gate: same closure, same flag, one edge.
fn arm_gate(cache: &Arc<gate::ModeCache>) -> Option<logos_rust_sdk::EventSubscription> {
    let subscribed = Arc::new(AtomicBool::new(false));
    let watcher = |cache: Arc<gate::ModeCache>, subscribed: Arc<AtomicBool>| {
        move |s: logos_rust_sdk::SubStatus, _generation: u64| match s {
            // The one edge that opens this cache, and never before there is a feed to open it
            // for. `Lost`, `Held`, `Abandoned` and anything a later protocol adds close it.
            logos_rust_sdk::SubStatus::Armed if subscribed.load(Ordering::Acquire) => {
                cache.feed_live()
            }
            _ => cache.feed_dead(),
        }
    };
    let mut w = modules().eth_rpc_module;
    w.on_subscription_status(watcher(cache.clone(), subscribed.clone())).ok()?;
    // Bound at function scope, so `w` still holds the client when this second proxy asks the
    // cache for it: the cache is weak, and letting the last handle go destroys the client and
    // silently discards the watcher just installed.
    let mut c = modules().eth_rpc_module;
    let Ok(sub) = c.on_verified_proxy_mode_changed() else {
        return None;
    };
    subscribed.store(true, Ordering::Release);
    let _ = w.on_subscription_status(watcher(cache.clone(), subscribed));
    Some(sub)
}

/// Keep the mode feed running for as long as one can be had.
///
/// A stream now ENDS: `Abandoned` is terminal, and it wakes its reader rather than parking it
/// for ever, so everything below the loop is reachable. Terminal is terminal — the way back
/// is not a re-arm of this subscription but a NEW one, which is unarmed at creation. Nothing
/// here opens the cache: taking a subscription is not an arm, and every re-subscribe waits
/// on the same single edge in [`arm_gate`] the first one did.
fn gate_feed(cache: &Arc<gate::ModeCache>) {
    for attempt in 0..=GATE_REARMS {
        if attempt > 0 {
            std::thread::sleep(GATE_REARM_PAUSE);
        }
        let Some(sub) = arm_gate(cache) else {
            cache.feed_dead();
            continue;
        };
        for ev in sub {
            let Some(e) =
                eth_rpc_module::EthRpcModuleClient::decode_verified_proxy_mode_changed(&ev)
            else {
                // An event we cannot read is a contract we no longer share, and it names no
                // chain to invalidate. Drop the lot, and do not go back for more of it.
                cache.feed_dead();
                return;
            };
            cache.told(e.chain_id as u64, &e.mode);
            emit_networks_changed(e.chain_id);
        }
        cache.feed_dead();
    }
}

/// One flag per dependency, set once its config is settled and never disturbed again.
/// A dependency can appear after us: `dependencies` orders the initial load, but a module
/// that crashed and restarted comes back late, so startup is not the only chance to ask.
#[derive(Default)]
struct DepInit {
    eth_rpc: AtomicBool,
    token_list: AtomicBool,
}

/// Everything a request works on. Each field carries its own lock, taken and released inside
/// itself around local work only — so nothing in this file ever holds one across a call.
struct State {
    settings: SettingsStore,
    contacts: ContactsStore,
}

/// Accounts the keystore holds, or -1 when it could not be asked — the keystore's own
/// "unknown, and not zero". Only ever a payload: every consumer of the event re-reads.
fn keystore_account_count() -> i64 {
    let Ok(raw) = modules().keystore_module.list_accounts() else { return -1 };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else { return -1 };
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return -1;
    }
    v.get("accounts").and_then(Value::as_array).map(|a| a.len() as i64).unwrap_or(-1)
}

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

const NO_CONTEXT: &str = "module context not ready";

/// The reply every gated method returns when the proxy is blocking. `error` is the verdict's
/// own sentence, so the view renders something actionable with no new wiring.
fn blocked(verdict: &Value) -> Value {
    json!({
        "ok": false,
        "error": verdict.get("message").and_then(Value::as_str)
            .unwrap_or("the verified proxy is not usable"),
        "verifiedProxy": verdict,
    })
}

/// eth_rpc's config mutators answer `{ ok, ... }`. A `false` there is a real refusal —
/// an empty endpoint, an unknown chain — and must not be reported as success.
fn expect_ok(raw: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(v)
    } else {
        Err(v.get("error").and_then(Value::as_str).unwrap_or("eth_rpc refused the change").to_string())
    }
}

/// A reply from `tx_sender_module`, parsed, with its refusal surfaced as this module's own.
/// A transport error names the sender, so an operator can tell a sender that is down from a
/// sender that said no.
fn sender_reply(raw: Result<String, impl std::fmt::Debug>) -> Result<Value, String> {
    let raw = raw.map_err(|e| format!("tx_sender_module: {e:?}"))?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("tx_sender_module refused the call")
            .to_string());
    }
    Ok(v)
}

/// A reply relayed to the view VERBATIM — the sender's refusals included, because they are
/// already in the `{ ok: false, error, verifiedProxy? }` shape the view renders.
fn relay(raw: Result<String, impl std::fmt::Debug>) -> String {
    match raw {
        Ok(reply) => reply,
        Err(e) => err(format!("tx_sender_module: {e:?}")),
    }
}

/// A send as the caller asked for it, before any chain lookup.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendRequest {
    from: String,
    to: String,
    /// Base units of the token being moved (wei for a native send). Kept exactly as it was
    /// so an existing caller is never silently rescaled.
    #[serde(default)]
    amount: Option<String>,
    /// The amount as the user typed it, in TOKEN units — "0.1" ETH, not 10^17 wei. Parsed
    /// against the resolved token's `decimals` with exact integer arithmetic.
    #[serde(default)]
    amount_units: Option<String>,
    /// The token's SYMBOL, or an address. A symbol is not an identity — two contracts can
    /// share one — so an ambiguous symbol is refused rather than guessed; prefer
    /// `tokenAddress`, which names one contract exactly.
    #[serde(default)]
    token: Option<String>,
    /// The token CONTRACT the caller means. Wins over `token` when both are given; omit it,
    /// or leave it empty, for the native currency.
    #[serde(default)]
    token_address: Option<String>,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    max_fee_per_gas: Option<String>,
    #[serde(default)]
    max_priority_fee_per_gas: Option<String>,
    #[serde(default)]
    gas_limit: Option<String>,
    #[serde(default)]
    nonce: Option<u64>,
}

/// A resolved send: what the transaction IS, before the sender prices it.
struct Resolved {
    chain_id: u64,
    from: alloy::primitives::Address,
    to: alloy::primitives::Address,
    amount: U256,
    token: Option<Token>,
    /// What `amount` is denominated in. Carried so every rendering — the reply, the claim,
    /// the history meta — reads the same units without re-deriving them.
    decimals: u8,
    symbol: String,
    native_symbol: String,
    /// The route label of this module's own token-balance read, when it made one.
    token_route: Option<String>,
}

impl Resolved {
    fn native(&self) -> bool {
        self.token.as_ref().map(|t| t.native).unwrap_or(true)
    }

    fn token_address(&self) -> Option<String> {
        self.token.as_ref().and_then(|t| t.address.clone())
    }

    /// The one call this send is: the transfer, as `tx_sender_module` takes it. `meta` is
    /// what this wallet wants back on the history row — the transfer's own facts, so the row
    /// can be read as a transfer rather than as a call to a token contract.
    fn call(&self) -> Result<Value, String> {
        let meta = match &self.token {
            Some(t) if !t.native => json!({
                "kind": "erc20", "token": t.address, "tokenSymbol": t.symbol,
                "tokenDecimals": t.decimals, "recipient": self.to.to_string(),
                "amount": self.amount.to_string(),
            }),
            _ => json!({ "kind": "native", "recipient": self.to.to_string(),
                         "amount": self.amount.to_string() }),
        };
        Ok(match &self.token {
            Some(t) if !t.native => {
                let addr = t.address.as_deref().unwrap_or_default()
                    .parse::<alloy::primitives::Address>()
                    .map_err(|e| format!("token has an unparseable address: {e}"))?;
                json!({ "to": addr.to_string(), "value": "0x0",
                        "data": format!("0x{}", hex::encode(txbuild::erc20_transfer_calldata(self.to, self.amount))),
                        "label": format!("Send {}", t.symbol), "meta": meta })
            }
            _ => json!({ "to": self.to.to_string(), "value": format!("0x{:x}", self.amount),
                         "data": "0x", "label": format!("Send {}", self.native_symbol),
                         "meta": meta }),
        })
    }
}

fn parse_u64_any(s: &str) -> Option<u64> {
    parse_u256_any(s).and_then(|v| u64::try_from(v).ok())
}

impl EthWalletBackendImpl {
    /// The state, with the guard already dropped — the only lock this file takes. An owned
    /// handle is not a convention to remember: the guard is gone before this returns.
    fn state(&self) -> Result<Arc<State>, String> {
        let guard = self.state.read().map_err(|_| "state lock poisoned".to_string())?;
        guard.clone().ok_or_else(|| NO_CONTEXT.to_string())
    }

    /// The whole settings file. A file read; no lock and no call. An unreadable settings file
    /// is an error, never chain 1: every caller here gates, prices or labels on this answer.
    fn settings(&self) -> Result<Settings, String> {
        self.state()?.settings.try_load().map_err(|e| e.to_string())
    }

    fn active_chain(&self) -> Result<u64, String> {
        Ok(self.settings()?.active_chain_id)
    }

    /// Seed a network's transport where eth_rpc has none. `chains.json` is shared with other
    /// wallets on this device: seeding is ours to do, overwriting is not.
    fn seed_chain_config(&self, chain_id: u64, rpc_url: &str, b: &Budget) -> Result<(), String> {
        if rpc_url.trim().is_empty() {
            return Ok(());
        }
        let t = b.take(INIT_BUDGET).ok_or_else(|| "no time left to seed a chain".to_string())?;
        let cfg = json!({ "endpoint": rpc_url, "timeoutSecs": 8 });
        let raw = modules()
            .eth_rpc_module
            .ensure_chain_config_with_timeout(chain_id as i64, &cfg.to_string(), t)
            .map_err(|e| format!("{e:?}"))?;
        expect_ok(&raw).map(|_| ())
    }

    /// Give eth_rpc a transport for every network without ever overwriting one. Runs at
    /// startup and, if it did not land, at most once per consumer-facing read after that.
    /// Four calls, all charged to `b`: the retry must not outlast the read that triggered it.
    fn ensure_eth_rpc(&self, b: &Budget) {
        if self.deps.eth_rpc.load(Ordering::Relaxed) {
            return;
        }
        // A url the user set while eth_rpc was down goes in first, so it claims an absent
        // slot ahead of the built-in default. Both writes only ever fill an absent field.
        // The settings are copied out first — none of these calls runs under the guard.
        if let Ok(st) = self.state() {
            for n in st.settings.try_load().map(|s| s.networks).unwrap_or_default() {
                let _ = self.seed_chain_config(n.chain_id, &n.rpc_url, b);
            }
        }
        // Keyed and idempotent per chain, so no gate: a store with one chain configured and
        // another missing still needs seeding.
        let Some(t) = b.take(INIT_BUDGET) else { return };
        let Ok(raw) = modules().eth_rpc_module.init_defaults_with_timeout(t) else {
            return;
        };
        if depinit::reply_ok(&raw) {
            self.deps.eth_rpc.store(true, Ordering::Relaxed);
        }
    }

    /// Relay the keystore's account changes to this module's own subscribers, so a view one
    /// hop further out learns about a rename it can see but cannot subscribe to. The
    /// subscription is a blocking iterator, so it needs a thread of its own; `concurrency:
    /// "multi"` is what makes that safe.
    fn watch_keystore(&self) {
        if self.watching_keystore.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut ks = modules().keystore_module;
        let Ok(sub) = ks.on_accounts_changed() else {
            // Only a client that could not be built lands here; lp defers a module that is
            // merely not up yet. Un-arm so the next account read tries again.
            self.watching_keystore.store(false, Ordering::SeqCst);
            return;
        };
        std::thread::spawn(move || {
            // Arming is not retroactive and nothing buffers, so a change made before this
            // point is lost. One read closes that window — off the host thread, because
            // `on_context_ready` is time-budgeted and this is not part of startup.
            emit_accounts_changed(keystore_account_count());
            for ev in sub {
                let count = keystore_module::KeystoreModuleClient::decode_accounts_changed(&ev)
                    .map(|e| e.count)
                    .unwrap_or(-1);
                emit_accounts_changed(count);
            }
        });
    }

    /// Relay the sender's three events as this module's own. A view depends on this module
    /// alone, and the facts these carry — a send settled, a row took a hash, a row was
    /// written ahead of its broadcast — now happen one hop further out. Armed once, like the
    /// keystore relay: three threads on channels nothing closes.
    ///
    /// `tx_status_changed` names a hash and no account, and this wallet cannot say from a
    /// hash whether a balance moved — so it also announces `balances_updated` with an EMPTY
    /// address, which a view reads as "re-read what you are showing". Every path the sender
    /// announces on is a settle or a broadcast, and both can move a balance.
    fn watch_sender(&self) {
        if self.watching_sender.swap(true, Ordering::SeqCst) {
            return;
        }
        // Three clients, each subscribing as it is built: a subscription outlives the call
        // that armed it, and a client dropped before its subscription is armed takes the
        // subscription with it.
        let mut a = modules().tx_sender_module;
        let Ok(sends) = a.on_send_status_changed() else {
            self.watching_sender.store(false, Ordering::SeqCst);
            return;
        };
        let mut b = modules().tx_sender_module;
        let Ok(txs) = b.on_tx_status_changed() else {
            self.watching_sender.store(false, Ordering::SeqCst);
            return;
        };
        let mut c = modules().tx_sender_module;
        let Ok(rows) = c.on_history_changed() else {
            self.watching_sender.store(false, Ordering::SeqCst);
            return;
        };
        std::thread::spawn(move || {
            for ev in sends {
                if let Some(e) = tx_sender_module::TxSenderModuleClient::decode_send_status_changed(&ev) {
                    emit_send_status_changed(&e.request_id);
                }
            }
        });
        std::thread::spawn(move || {
            for ev in txs {
                if let Some(e) = tx_sender_module::TxSenderModuleClient::decode_tx_status_changed(&ev) {
                    emit_tx_status_changed(&e.hash_hex);
                    emit_balances_updated("");
                }
            }
        });
        std::thread::spawn(move || {
            for ev in rows {
                if let Some(e) = tx_sender_module::TxSenderModuleClient::decode_history_changed(&ev) {
                    emit_history_changed(&e.address);
                }
            }
        });
    }

    /// Arm the gate feed. Everything the mode cache is allowed to remember rests on this
    /// subscription: it is what turns "verification is off for this chain" from a reading
    /// taken once into a fact someone is obliged to correct.
    ///
    /// Holding the subscription handle is NOT that fact. A subscription taken from
    /// `on_context_ready` is deferred until eth_rpc listens, so the handle exists across a
    /// window in which nobody would tell us the user switched verification ON. Only the
    /// runtime's per-module status channel separates the two, so a runtime without one
    /// latches the cache cold instead and every gated read pays its own probe.
    fn watch_gate(&self) {
        if self.feeds.gate.swap(true, Ordering::SeqCst) {
            return;
        }
        let cache = self.gate.clone();
        if !gate::status_channel(&logos_rust_sdk::protocol_version()) {
            // No one here can say when a subscription arms or dies, and a handle that merely
            // exists is not an arm. Latched for the process, which makes `feed_live` inert.
            cache.no_status_channel();
            eprintln!(
                "eth_wallet_backend: logos-protocol {} carries no per-module subscription \
                 status channel; the verified-proxy gate reads live on every check",
                logos_rust_sdk::protocol_version()
            );
        }
        listen(self.feeds.gate.clone(), cache, |cache| gate_feed(&cache));
    }

    /// Arm the chain-config feed. Freshness only — `list_networks` serves eth_rpc's record,
    /// and this is how a view learns the other app moved an endpoint. The gate invalidation
    /// is belt and braces: a config change that did not move the mode cannot alter a verdict,
    /// since `off` is never blocking whatever the endpoint is.
    fn watch_chain_config(&self) {
        if self.feeds.chains.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut c = modules().eth_rpc_module;
        let Ok(sub) = c.on_chain_config_changed() else {
            self.feeds.chains.store(false, Ordering::SeqCst);
            return;
        };
        let cache = self.gate.clone();
        listen(self.feeds.chains.clone(), sub, move |sub| {
            for ev in sub {
                if let Some(e) = eth_rpc_module::EthRpcModuleClient::decode_chain_config_changed(&ev)
                {
                    cache.invalidate(e.chain_id as u64);
                    emit_networks_changed(e.chain_id);
                }
            }
        });
    }

    /// Relay token_list's catalogue changes as this module's own `tokens_changed`. Same
    /// argument as the keystore relay: the rows this wallet OFFERS on a chain are that
    /// catalogue filtered by local settings, so a token another app imported moves them, and
    /// the view can subscribe to us but not to token_list. `config_changed` is deliberately
    /// not relayed — a proxy or interval edit moves no row, and the one field that does
    /// (`useEmbeddedList`) already comes back as `tokens_updated` per chain that moved.
    fn watch_token_list(&self) {
        if self.feeds.tokens.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut t = modules().token_list_module;
        let Ok(sub) = t.on_tokens_updated() else {
            self.feeds.tokens.store(false, Ordering::SeqCst);
            return;
        };
        listen(self.feeds.tokens.clone(), sub, |sub| {
            for ev in sub {
                if let Some(e) =
                    token_list_module::TokenListModuleClient::decode_tokens_updated(&ev)
                {
                    emit_tokens_changed(e.chain_id);
                }
            }
        });
    }

    /// Ask token_list whether it holds a config and, only if it says it holds none, tell it
    /// to apply its own defaults. Unkeyed, so the gate is mandatory; an `Err` or an `unready`
    /// initializes nothing — a call that did not arrive is not an empty config.
    fn ensure_token_list(&self, b: &Budget) {
        if self.deps.token_list.load(Ordering::Relaxed) {
            return;
        }
        let Some(t) = b.take(PROBE_BUDGET) else { return };
        let Ok(status) = modules().token_list_module.config_status_with_timeout(t) else {
            return;
        };
        match depinit::next_step(&status) {
            Next::Settled => self.deps.token_list.store(true, Ordering::Relaxed),
            Next::Initialize => {
                let Some(t) = b.take(INIT_BUDGET) else { return };
                let applied = modules().token_list_module.init_defaults_with_timeout(t);
                // `applied: false` is another consumer having got there first, not a failure.
                if applied.map(|raw| depinit::reply_ok(&raw)).unwrap_or(false) {
                    self.deps.token_list.store(true, Ordering::Relaxed);
                }
            }
            Next::AskAgain => {}
        }
    }

    /// eth_rpc's verified-proxy verdict for `chain_id`, or a synthetic blocking one when it
    /// cannot be read. Never falls back to `off`, and never unbounded: the probe is a
    /// cross-process hop the protocol answers with a 20s default, which on any path a user is
    /// watching is twenty seconds of frozen wallet. A probe the budget cuts short still
    /// refuses — the verdict it returns carries its own reason, so a timeout is not read as
    /// permission and not reported as a freeze.
    fn verified_verdict_within(&self, chain_id: u64, b: &Budget) -> Value {
        let Some(t) = b.take(PROBE_BUDGET) else {
            return verified::unknown_verdict(chain_id, "this read's budget ran out");
        };
        self.watch_gate();
        let ticket = self.gate.ticket();
        let raw = modules().eth_rpc_module.verified_proxy_status_with_timeout(chain_id as i64, t);
        let v = Self::verdict_of(chain_id, raw);
        self.gate.learned(chain_id, v.get("mode").and_then(Value::as_str), ticket);
        v
    }

    fn verdict_of(chain_id: u64, raw: Result<String, impl std::fmt::Debug>) -> Value {
        let raw = match raw {
            Ok(r) => r,
            Err(e) => return verified::unknown_verdict(chain_id, &format!("{e:?}")),
        };
        match serde_json::from_str::<Value>(&raw) {
            Ok(v) => verified::normalize(chain_id, &v),
            Err(e) => verified::unknown_verdict(chain_id, &format!("unreadable verdict: {e}")),
        }
    }

    /// Whether `chain_id` may be read. `Err` carries the verdict to return to the caller:
    /// with verification required and the proxy not usable, this wallet shows no chain data
    /// at all — not stale numbers, not zeros, not a clear-net read.
    ///
    /// [`Gate::Open`] skips the hop entirely, and only ever for a chain whose mode eth_rpc
    /// has told us is `off` — where its own `blocking` is `mode_required && !usable`, so the
    /// answer cannot depend on the proxy health this would have probed. Every other chain,
    /// and every chain we are not certain about, is read live and refuses on its own.
    ///
    /// Charged to the SAME budget as the calls behind it, so the gate is time the method's
    /// own allowance can see rather than twenty seconds in front of it.
    fn verified_gate_within(&self, chain_id: u64, b: &Budget) -> Result<(), Value> {
        if self.gate.gate(chain_id) == Gate::Open {
            return Ok(());
        }
        Self::gate_of(self.verified_verdict_within(chain_id, b))
    }

    fn gate_of(v: Value) -> Result<(), Value> {
        if verified::is_blocking(&v) {
            Err(v)
        } else {
            Ok(())
        }
    }

    /// `token_list` entries for the rows `chain_id` OFFERS, keyed by lowercased address.
    /// Asked by address — a mainnet `get_tokens` is ~86 KB to decorate two rows — and bounded:
    /// enrichment is decoration and must never fail, or stall, the token list.
    fn list_meta(
        &self,
        chain_id: i64,
        list: &[tokens::Token],
        b: &Budget,
    ) -> HashMap<String, Value> {
        let mut out = HashMap::new();
        let Some(query) = tokens::meta_query(list) else { return out };
        let Some(t) = b.take(PROBE_BUDGET) else { return out };
        let Ok(raw) =
            modules().token_list_module.get_tokens_by_address_with_timeout(chain_id, &query, t)
        else {
            return out;
        };
        let Ok(v) = serde_json::from_str::<Value>(&raw) else { return out };
        for t in v.get("tokens").and_then(Value::as_array).into_iter().flatten() {
            if let Some(a) = t.get("address").and_then(Value::as_str) {
                out.insert(a.to_lowercase(), t.clone());
            }
        }
        out
    }

    /// Everything `token_list` holds for `chain_id`, and — separately — why it could not be
    /// asked. Two values rather than one empty vector: on sepolia and hoodi an empty catalogue
    /// is the ORDINARY answer, and a caller that cannot tell it from a failed call shows
    /// "this network has no tokens" for what is really an outage.
    fn chain_catalogue(&self, chain_id: i64, b: &Budget) -> (Vec<Value>, Option<String>) {
        self.watch_token_list();
        let Some(t) = b.take(CATALOGUE_BUDGET) else {
            return (Vec::new(), Some("this read ran out of time before token_list was asked".into()));
        };
        let raw = match modules().token_list_module.get_tokens_with_timeout(chain_id, t) {
            Ok(r) => r,
            Err(e) => return (Vec::new(), Some(format!("{e:?}"))),
        };
        let v: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => return (Vec::new(), Some(format!("unreadable token_list reply: {e}"))),
        };
        match v.get("tokens").and_then(Value::as_array) {
            Some(a) => (a.clone(), None),
            None => (
                Vec::new(),
                Some(
                    v.get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("token_list returned no token list")
                        .to_string(),
                ),
            ),
        }
    }

    /// The record `token_list` holds for one address on one chain — the snapshot the enabled
    /// set stores — or why there is none.
    ///
    /// This is THE gate on the enabled set. An address the list cannot describe is not
    /// enabled, because `decimals` would have to be invented and a wrong one mis-scales every
    /// amount by a power of ten — on the screen and in what the user signs.
    fn snapshot(&self, chain_id: i64, address: &str, b: &Budget) -> Result<Token, String> {
        let query = serde_json::to_string(&[address]).map_err(|e| e.to_string())?;
        let t = b.take(PROBE_BUDGET).ok_or("this read ran out of time before token_list was asked")?;
        let raw = modules()
            .token_list_module
            .get_tokens_by_address_with_timeout(chain_id, &query, t)
            .map_err(|e| format!("{e:?}"))?;
        let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        let rows = v.get("tokens").and_then(Value::as_array).cloned().unwrap_or_default();
        tokens::snapshot_of(chain_id as u64, address, &rows).ok_or_else(|| {
            format!(
                "token_list does not hold {address} on chain {chain_id}, so this wallet cannot \
                 say what its amounts mean — a token it cannot describe is one it must not offer"
            )
        })
    }

    /// Resolve a send: the token, the amount in its units, and — for an ERC-20 — whether the
    /// account holds it. Pure of side effects. What the sender cannot know is decided here;
    /// what it can (the fee, the ether, the nonce) is left to it.
    fn resolve(&self, req: &SendRequest, chain_id: u64, b: &Budget) -> Result<Resolved, String> {
        let from = req.from.trim().parse::<alloy::primitives::Address>()
            .map_err(|e| format!("invalid `from` address: {e}"))?;
        let to = req.to.trim().parse::<alloy::primitives::Address>()
            .map_err(|e| format!("invalid `to` address: {e}"))?;
        // The token first: `amountUnits` cannot be scaled until its decimals are known. The
        // offered set is the SAME one the balance list reads, enabled tokens included, so a
        // token the wallet shows a balance for can always be sent.
        let settings = self.settings()?;
        // Address first, and a symbol only while it names ONE contract: `tokens::resolve`
        // refuses an ambiguous one instead of taking the first row, which is how a send
        // reached the wrong asset for a user holding both tokens that call themselves LIT.
        let addr = req.token_address.as_deref().map(str::trim).filter(|a| !a.is_empty());
        let token = match (addr, &req.token) {
            (Some(a), _) => Some(
                tokens::by_address(chain_id, a, settings.enabled_tokens(chain_id))
                    .ok_or_else(|| format!("no token at {a} is offered on chain {chain_id}"))?,
            ),
            (None, Some(k)) => {
                Some(tokens::resolve(chain_id, k, settings.enabled_tokens(chain_id))?)
            }
            (None, None) => None,
        };
        let native_symbol =
            networks::by_chain_id(chain_id).map(|n| n.native_symbol).unwrap_or("ETH").to_string();
        let decimals = token.as_ref().map(|t| t.decimals).unwrap_or(18);
        let symbol =
            token.as_ref().map(|t| t.symbol.clone()).unwrap_or_else(|| native_symbol.clone());
        let amount = units::resolve_amount(
            req.amount.as_deref(),
            req.amount_units.as_deref(),
            decimals,
            &symbol,
        )?;

        // The sender charges the fee against ether. An ERC-20 send is checked against the
        // token itself here: without it an over-large transfer is approved, broadcast,
        // reverts on chain and burns the gas.
        let mut token_route = None;
        if let Some(t) = token.as_ref().filter(|t| !t.native) {
            let addr = t.address.as_deref().unwrap_or_default()
                .parse::<alloy::primitives::Address>()
                .map_err(|e| format!("token has an unparseable address: {e}"))?;
            let (held, r) = self.token_balance(chain_id, addr, from, b)?;
            token_route = r;
            send::token_affordable(held, amount, &t.symbol, t.decimals)?;
        }

        Ok(Resolved { chain_id, from, to, amount, token, decimals, symbol, native_symbol, token_route })
    }

    fn token_balance(
        &self,
        chain_id: u64,
        token: alloy::primitives::Address,
        owner: alloy::primitives::Address,
        b: &Budget,
    ) -> Result<(U256, Option<String>), String> {
        let call = json!({
            "to": token.to_string(),
            "data": format!("0x{}", hex::encode(txbuild::erc20_balance_of_calldata(owner))),
        });
        let t = b.take(RPC_BUDGET).ok_or("no time left to read the token balance")?;
        let raw = modules()
            .eth_rpc_module
            .call_with_timeout(chain_id as i64, &call.to_string(), callee_deadline(t), t)
            .map_err(|e| format!("{e:?}"))?;
        let a = unwrap_answer(&raw)?;
        let v = a.value.as_str()
            .and_then(|s| hex::decode(s.trim_start_matches("0x")).ok())
            .as_deref()
            .and_then(txbuild::decode_uint256)
            .ok_or_else(|| "could not read the token balance".to_string())?;
        Ok((v, a.route))
    }

    /// The request `tx_sender_module` takes for a resolved send: one call, the fee controls
    /// as the caller gave them, and the sender's own allowance cut to what is left here.
    fn sender_request(r: &Resolved, req: &SendRequest, t: std::time::Duration) -> Result<Value, String> {
        let mut v = json!({
            "chainId": r.chain_id,
            "from": r.from.to_string(),
            "calls": [r.call()?],
        });
        if let Some(x) = &req.tier { v["tier"] = json!(x); }
        if let Some(x) = &req.max_fee_per_gas { v["maxFeePerGas"] = json!(x); }
        if let Some(x) = &req.max_priority_fee_per_gas { v["maxPriorityFeePerGas"] = json!(x); }
        if let Some(x) = &req.gas_limit { v["calls"][0]["gasLimit"] = json!(x); }
        if let Some(n) = req.nonce { v["nonce"] = json!(n); }
        if let Some(d) = callee_deadline(t) { v["deadlineMs"] = json!(d); }
        Ok(v)
    }

    /// Have the sender price the resolved send. Its reply carries the nonce, the gas limit,
    /// the fee and the ether check; this module's reply wraps them around the token.
    fn sender_prepare(&self, r: &Resolved, req: &SendRequest, b: &Budget) -> Result<Value, String> {
        let t = b.take(SENDER_BUDGET).ok_or("no time left to price the send")?;
        let request = Self::sender_request(r, req, t)?;
        sender_reply(modules().tx_sender_module.prepare_with_timeout(&request.to_string(), t))
    }

    /// The `prepare_send` reply: the resolved transfer, plus what the sender priced it at.
    fn quote_reply(r: &Resolved, priced: &Value) -> Value {
        let mut v = json!({
            "ok": true, "chainId": r.chain_id,
            "from": r.from.to_string(), "to": r.to.to_string(),
            "amount": r.amount.to_string(),
            "amountSymbol": r.symbol,
            "amountDecimals": r.decimals,
            "nativeSymbol": r.native_symbol,
            "token": r.token.as_ref().map(|t| t.symbol.clone()),
            // WHICH contract the send will call, resolved. A symbol cannot say it: two tokens
            // can share one, so a confirmation step showing only the symbol cannot reveal
            // that the wrong asset is about to move. Null for a native send.
            "tokenAddress": r.token_address(),
            "nonce": priced.get("nonce").cloned().unwrap_or(Value::Null),
            "gasLimit": priced.get("gasLimit").cloned().unwrap_or(Value::Null),
            "maxFeePerGas": priced.get("maxFeePerGas").cloned().unwrap_or(Value::Null),
            "maxPriorityFeePerGas": priced.get("maxPriorityFeePerGas").cloned().unwrap_or(Value::Null),
            "feeSource": priced.get("feeSource").cloned().unwrap_or(json!("unknown")),
            // `route` covers the sender's balance and nonce reads and this module's token
            // read; the weakest of them. The fee is fee_module's, which emits no label, so it
            // is never proof-backed whatever `route` says.
            "route": verified::weakest_route(&[
                priced.get("route").and_then(Value::as_str),
                r.token_route.as_deref(),
            ]),
            "feeRoute": verified::UNKNOWN_ROUTE,
        });
        // The ceiling and the worst case, as the sender computed them, in the sender's own
        // decoration: `maxFeePerGas × gasLimit` is a ceiling, never a price.
        for key in ["maxCostWei", "feeCeilingWei"] {
            for suffix in ["", "Display", "Exact"] {
                let k = format!("{key}{suffix}");
                if let Some(x) = priced.get(&k) {
                    v[k] = x.clone();
                }
            }
        }
        units::decorate(&mut v, "amount", &r.amount.to_string(), Some(r.decimals));
        v
    }

    /// The sends THIS wallet made that a human has not answered yet, or an empty list when
    /// the sender cannot be asked — a sender that is down holds nothing.
    fn own_sends_in_flight(&self, b: &Budget) -> Vec<Value> {
        let Some(t) = b.take(RPC_BUDGET) else { return Vec::new() };
        let Ok(v) = sender_reply(modules().tx_sender_module.live_sends_with_timeout(t)) else {
            return Vec::new();
        };
        v.get("sends")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|s| s.get("origin").and_then(Value::as_str) == Some(OWN_NAME))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The endpoint eth_rpc holds for `chain_id`, empty when it has none or when `b` had no
    /// time left to ask. Read, never written: that store is shared with every wallet on the
    /// device and `eth_rpc_ui` owns it.
    fn chain_endpoint(&self, chain_id: u64, b: &Budget) -> String {
        let Some(t) = b.take(PROBE_BUDGET) else { return String::new() };
        modules()
            .eth_rpc_module
            .get_chain_config_with_timeout(chain_id as i64, t)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .filter(|v| v.get("ok").and_then(Value::as_bool) == Some(true))
            .and_then(|v| Some(v.get("config")?.get("endpoint")?.as_str()?.to_string()))
            .unwrap_or_default()
    }

    /// One network entry. Endpoint and mode both come out of eth_rpc rather than a local
    /// copy — one source of truth, and no way for the two to disagree. Two IPC calls, both
    /// charged to `b`, and neither may run under the state guard.
    fn network_json(&self, chain_id: u64, b: &Budget) -> Value {
        let net = networks::by_chain_id(chain_id);
        let vp = self.verified_verdict_within(chain_id, b);
        json!({
            "chainId": chain_id,
            "key": net.map(|n| n.key).unwrap_or_default(),
            "name": net.map(|n| n.name).unwrap_or_default(),
            "nativeSymbol": net.map(|n| n.native_symbol).unwrap_or_default(),
            "testnet": net.map(|n| n.testnet).unwrap_or(false),
            "rpcUrl": self.chain_endpoint(chain_id, b),
            "verifiedProxyMode": vp.get("mode").and_then(Value::as_str).unwrap_or("unknown"),
            "verifiedProxy": vp,
        })
    }
}

impl EthWalletBackendModule for EthWalletBackendImpl {
    fn on_context_ready(&self, ctx: &RustModuleContext) {
        let dir = PathBuf::from(&ctx.instance_persistence_path);
        let settings = SettingsStore::with_path(dir.join("settings.json"));
        let contacts = ContactsStore::with_path(dir.join("contacts.json"));

        if let Ok(mut g) = self.state.write() {
            *g = Some(Arc::new(State { settings, contacts }));
        }
        // eth_rpc first: every balance, fee and send goes through it while token_list only
        // decorates. Neither may fail startup, and neither writes over an existing config.
        // ONE budget across both: the host is blocked in here, and six individually bounded
        // calls still add up to ~26s. What does not fit is retried on the first read.
        let b = Budget::new(STARTUP_BUDGET);
        self.ensure_eth_rpc(&b);
        self.ensure_token_list(&b);
        // After the state above exists: the relay's first act is a `list_accounts`, and a
        // consumer must never be told to re-read before this module can answer.
        self.watch_keystore();
        self.watch_sender();
        // Arm before the first gated read rather than on it: the gate cache may only trust an
        // answer read after its feed existed, so arming late costs a live read per chain.
        self.watch_gate();
        self.watch_chain_config();
        self.watch_token_list();
    }

    fn list_networks(&self) -> String {
        let b = Budget::new(READ_BUDGET);
        self.ensure_eth_rpc(&b);
        self.watch_chain_config();
        // Copy out the one field this needs and drop the guard: ten outbound calls held
        // under a read lock is how one slow dependency stalls every other reader.
        let active = match self.active_chain() {
            Ok(id) => id,
            Err(e) => return err(e),
        };
        // Active network first so a short budget degrades the other two to `unknown` rather
        // than the one the view is showing. The reply keeps the table's own order.
        let mut order: Vec<usize> = (0..networks::ALL.len()).collect();
        order.sort_by_key(|&i| networks::ALL[i].chain_id != active);
        let mut built: Vec<Option<Value>> = vec![None; networks::ALL.len()];
        for i in order {
            built[i] = Some(self.network_json(networks::ALL[i].chain_id, &b));
        }
        let list: Vec<Value> = built.into_iter().flatten().collect();
        json!({ "ok": true, "activeChainId": active, "networks": list }).to_string()
    }

    fn get_active_network(&self) -> String {
        let b = Budget::new(READ_BUDGET);
        // `network_json` is two IPC calls, so it runs against a released guard.
        match self.active_chain() {
            Ok(id) => json!({ "ok": true, "network": self.network_json(id, &b) }).to_string(),
            Err(e) => err(e),
        }
    }

    fn set_active_chain(&self, chain_id: i64) -> String {
        if chain_id < 0 {
            return err(format!("chain {chain_id} is not a valid chain id"));
        }
        let st = match self.state() {
            Ok(st) => st,
            Err(e) => return err(e),
        };
        // A send this wallet made and a human has not answered names the network it was
        // built for; moving the wallet under it would have them approve for a chain the
        // wallet no longer shows. Only this wallet's own sends hold it — another app's is
        // sent on its own chain either way.
        let b = Budget::new(READ_BUDGET);
        if let Some(live) = self.own_sends_in_flight(&b).first() {
            let on = live.get("chainId").and_then(Value::as_u64).unwrap_or(0);
            let name = networks::by_chain_id(on).map(|n| n.name.to_string()).unwrap_or_else(|| on.to_string());
            let id = live.get("requestId").and_then(Value::as_str).unwrap_or_default();
            return err(format!("cannot switch network while a send is awaiting approval on {name} ({id})"));
        }
        match st.settings.set_active_chain(chain_id as u64) {
            Ok(a) => {
                if a.changed {
                    emit_active_chain_changed(chain_id);
                }
                json!({ "ok": true, "activeChainId": a.settings.active_chain_id }).to_string()
            }
            Err(e) => err(e),
        }
    }

    fn verified_proxy_state(&self) -> String {
        let b = Budget::new(VERDICT_BUDGET);
        match self.active_chain() {
            Ok(id) => self.verified_verdict_within(id, &b).to_string(),
            Err(e) => err(e),
        }
    }

    fn list_tokens(&self) -> String {
        let b = Budget::new(READ_BUDGET);
        self.ensure_token_list(&b);
        let s = match self.settings() {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        let id = s.active_chain_id;
        let list = tokens::for_chain(id, s.enabled_tokens(id));
        let meta = self.list_meta(id as i64, &list, &b);
        json!({ "ok": true, "chainId": id, "tokenSort": s.token_sort.as_str(),
                "tokens": tokens::enrich(id, &list, &meta) })
        .to_string()
    }

    fn list_available_tokens(&self, chain_id: i64, query: String, offset: i64, limit: i64) -> String {
        if chain_id < 0 || !networks::is_supported(chain_id as u64) {
            return err(format!("chain {chain_id} is not one of this wallet's networks"));
        }
        let b = Budget::new(READ_BUDGET);
        self.ensure_token_list(&b);
        let chain_id = chain_id as u64;
        let s = match self.settings() {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        let (listed, list_error) = self.chain_catalogue(chain_id as i64, &b);
        // A non-positive limit is no limit: "show me everything" needs a spelling, and zero
        // meaning "nothing" would make an off-by-one in a caller look like an empty chain.
        let cut = usize::try_from(limit).ok().filter(|n| *n > 0);
        let offset = usize::try_from(offset).unwrap_or(0);
        let (total, rows) =
            tokens::available(chain_id, &listed, s.enabled_tokens(chain_id), &query, offset, cut);
        let has_more = offset.saturating_add(rows.len()) < total;
        let mut v = json!({ "ok": true, "chainId": chain_id, "tokenSort": s.token_sort.as_str(),
                            "total": total, "offset": offset, "shown": rows.len(),
                            "hasMore": has_more, "listed": listed.len(), "tokens": rows });
        // Only when the call itself failed. Its ABSENCE is what makes `listed: 0` readable as
        // "this chain has none" — the ordinary answer on sepolia and hoodi.
        if let Some(e) = list_error {
            v["listError"] = json!(e);
        }
        v.to_string()
    }

    fn set_token_enabled(&self, chain_id: i64, address: String, enabled: bool) -> String {
        if chain_id < 0 || !networks::is_supported(chain_id as u64) {
            return err(format!("chain {chain_id} is not one of this wallet's networks"));
        }
        let b = Budget::new(READ_BUDGET);
        // Normalised once, before anything is asked or written: token_list matches an address
        // case-insensitively and the store keeps one casing, so both halves must agree on
        // which 20 bytes are meant.
        let addr = match address.trim().parse::<alloy::primitives::Address>() {
            Ok(a) => a.to_string(),
            Err(e) => return err(format!("'{}' is not a token address: {e}", address.trim())),
        };
        let st = match self.state() {
            Ok(st) => st,
            Err(e) => return err(e),
        };
        // A built-in row is offered unconditionally, so turning it on costs no round trip and
        // stores nothing. Turning it off is the store's refusal to make.
        if enabled && tokens::is_builtin(chain_id as u64, &addr) {
            return json!({ "ok": true }).to_string();
        }
        let outcome = if enabled {
            self.ensure_token_list(&b);
            match self.snapshot(chain_id, &addr, &b) {
                Ok(t) => st.settings.enable_token(chain_id as u64, t),
                Err(e) => return err(e),
            }
        } else {
            st.settings.disable_token(chain_id as u64, &addr)
        };
        match outcome {
            Ok(a) => {
                // After the write, and only when it moved: re-enabling a row whose snapshot
                // is already stored offers nothing new.
                if a.changed {
                    emit_tokens_changed(chain_id);
                }
                json!({ "ok": true }).to_string()
            }
            Err(e) => err(e),
        }
    }

    fn set_token_sort(&self, order: String) -> String {
        let Some(o) = TokenSort::parse(&order) else {
            return err(format!("'{order}' is not an order; use 'alpha' or 'balance'"));
        };
        match self.state().and_then(|st| st.settings.set_token_sort(o).map_err(|e| e.to_string())) {
            Ok(a) => {
                let order = a.settings.token_sort.as_str();
                if a.changed {
                    emit_token_sort_changed(order);
                }
                json!({ "ok": true, "tokenSort": order }).to_string()
            }
            Err(e) => err(e),
        }
    }

    fn list_accounts(&self) -> String {
        // The relay's only retry: `on_context_ready` is the one chance startup gives it, and
        // a client that could not be built there would otherwise leave the view deaf forever.
        self.watch_keystore();
        match modules().keystore_module.list_accounts() {
            Ok(reply) => reply,
            Err(e) => err(format!("{e:?}")),
        }
    }

    fn get_account_labels(&self) -> String {
        self.watch_keystore();
        match modules().keystore_module.get_labels() {
            Ok(reply) => reply,
            Err(e) => err(format!("{e:?}")),
        }
    }

    fn get_account_wallets(&self) -> String {
        // Bounded, unlike its two siblings: they are one passthrough call each, this is two,
        // so an unbounded pair could hold the host for twice the protocol's own default.
        let b = Budget::new(READ_BUDGET);
        self.watch_keystore();
        let Some(t) = b.take(RPC_BUDGET) else { return err("no time left to read the wallets") };
        let provenance = match modules().keystore_module.get_provenance_with_timeout(t) {
            Ok(r) => r,
            Err(e) => return err(format!("{e:?}")),
        };
        let Some(t) = b.take(RPC_BUDGET) else { return err("no time left to name the wallets") };
        let labels = match modules().keystore_module.get_group_labels_with_timeout(t) {
            Ok(r) => r,
            Err(e) => return err(format!("{e:?}")),
        };
        let provenance: Value = match serde_json::from_str(&provenance) {
            Ok(v) => v,
            Err(e) => return err(e.to_string()),
        };
        let labels: Value = match serde_json::from_str(&labels) {
            Ok(v) => v,
            Err(e) => return err(e.to_string()),
        };
        let by_group = labels.get("labels").and_then(Value::as_object);
        let accounts = provenance.get("accounts").and_then(Value::as_object);
        let mut wallets = serde_json::Map::new();
        if let (Some(accounts), Some(by_group)) = (accounts, by_group) {
            for (address, row) in accounts {
                let Some(group) = row.get("group").and_then(Value::as_str) else { continue };
                // An unnamed wallet contributes nothing: the view's fallback is the address,
                // and an empty string here would read as a name that happens to be blank.
                match by_group.get(group).and_then(Value::as_str) {
                    Some(name) if !name.trim().is_empty() => {
                        let mut entry = json!({ "wallet": name });
                        if let Some(i) = row.get("index").and_then(Value::as_i64) {
                            entry["index"] = json!(i);
                        }
                        wallets.insert(address.clone(), entry);
                    }
                    _ => {}
                }
            }
        }
        json!({ "ok": true, "wallets": wallets }).to_string()
    }

    fn list_contacts(&self) -> String {
        let st = match self.state() {
            Ok(st) => st,
            Err(e) => return err(e),
        };
        match st.contacts.list() {
            Ok(all) => json!({ "ok": true, "contacts": all }).to_string(),
            Err(e) => err(e),
        }
    }

    fn save_contact(&self, address: String, name: String) -> String {
        let st = match self.state() {
            Ok(st) => st,
            Err(e) => return err(e),
        };
        match st.contacts.save(&address, &name) {
            Ok(c) => json!({ "ok": true, "contact": c }).to_string(),
            Err(e) => err(e),
        }
    }

    fn forget_contact(&self, address: String) -> String {
        let st = match self.state() {
            Ok(st) => st,
            Err(e) => return err(e),
        };
        match st.contacts.remove(&address) {
            Ok(()) => json!({ "ok": true, "address": address }).to_string(),
            Err(e) => err(e),
        }
    }

    fn get_balances(&self, address: String) -> String {
        // ONE allowance across the retry, the gate and the Multicall3 read. An unbounded probe
        // in front of a read is time a user waits that the method's own budget cannot see.
        let b = Budget::new(BALANCES_BUDGET);
        self.ensure_eth_rpc(&b);
        let settings = match self.settings() {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        let chain_id = settings.active_chain_id;
        if let Err(v) = self.verified_gate_within(chain_id, &b) {
            return blocked(&v).to_string();
        }
        let owner = match address.trim().parse::<alloy::primitives::Address>() {
            Ok(a) => a,
            Err(e) => return err(format!("invalid address: {e}")),
        };

        // Every offered token, enabled ones included — so a token the user turned on has a row
        // even at zero. A token that silently vanishes from the list reads as a lost balance.
        let list = tokens::for_chain(chain_id, settings.enabled_tokens(chain_id));
        let mut calls: Vec<(alloy::primitives::Address, Vec<u8>)> = Vec::new();
        for t in &list {
            match &t.address {
                None => calls
                    .push((txbuild::multicall3_address(), txbuild::multicall3_get_eth_balance_calldata(owner))),
                Some(a) => match a.parse::<alloy::primitives::Address>() {
                    Ok(token) => calls.push((token, txbuild::erc20_balance_of_calldata(owner))),
                    Err(e) => return err(format!("token {} has an unparseable address: {e}", t.symbol)),
                },
            }
        }

        let data = txbuild::multicall3_aggregate3_calldata(&calls);
        let call = json!({
            "to": txbuild::multicall3_address().to_string(),
            "data": format!("0x{}", hex::encode(&data)),
        });
        let Some(t) = b.take(RPC_BUDGET) else {
            return err("no time left to read the balances");
        };
        let payload = call.to_string();
        let raw = match modules().eth_rpc_module.call_with_timeout(chain_id as i64, &payload, callee_deadline(t), t) {
            Ok(r) => r,
            Err(e) => return err(format!("{e:?}")),
        };
        let Answer { value: result, route } = match unwrap_answer(&raw) {
            Ok(a) => a,
            Err(e) => return err(e),
        };
        let bytes = match result.as_str().and_then(|s| hex::decode(s.trim_start_matches("0x")).ok()) {
            Some(b) => b,
            None => return err("eth_call returned no decodable data"),
        };
        let Some(returns) = txbuild::decode_aggregate3_returns(&bytes) else {
            return err("could not decode the Multicall3 response");
        };

        // Assembled and sorted in `tokens`, against U256, rather than here: comparing
        // 18-decimal amounts in QML means parseFloat, and a double loses the digits that
        // decide the order. Keeping it out of the glue is also what makes it testable —
        // this file is behind the module feature and `cargo test` never compiles it.
        let decoded: Vec<Option<String>> = returns
            .iter()
            .map(|ret| ret.as_deref().and_then(txbuild::decode_uint256).map(|v| v.to_string()))
            .collect();
        let balances = tokens::balance_rows(chain_id, &list, &decoded, settings.token_sort);

        // Deliberately no `balances_updated` here: a read must not announce itself, or the
        // view's own subscription drives it round again forever.
        json!({ "ok": true, "chainId": chain_id, "address": address, "balances": balances,
                "tokenSort": settings.token_sort.as_str(),
                "route": verified::weakest_route(&[route.as_deref()]) })
        .to_string()
    }

    fn get_history(&self, address: String) -> String {
        let b = Budget::new(HISTORY_BUDGET);
        let settings = match self.settings() {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        let chain_id = settings.active_chain_id;
        let Some(t) = b.take(HISTORY_BUDGET) else { return err("no time left to read the history") };
        // The sender sweeps due receipts and announces what moved; this read announces
        // nothing of its own.
        let mut v = match sender_reply(
            modules().tx_sender_module.history_with_timeout(&address, chain_id as i64, t),
        ) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        // The same offered set the balance list and the send path read, so a transfer in an
        // enabled token is decoded rather than shown as an unknown contract.
        rows::decorate_history(&mut v, chain_id, settings.enabled_tokens(chain_id));
        v.to_string()
    }

    fn refresh_pending(&self, address: String) -> String {
        let b = Budget::new(HISTORY_BUDGET);
        let Some(t) = b.take(HISTORY_BUDGET) else { return err("no time left to sweep the receipts") };
        relay(modules().tx_sender_module.refresh_pending_with_timeout(&address, t))
    }

    fn prepare_send(&self, request_json: String) -> String {
        let b = Budget::new(SEND_BUDGET);
        let req: SendRequest = match serde_json::from_str(&request_json) {
            Ok(r) => r,
            Err(e) => return err(format!("invalid send request: {e}")),
        };
        let chain_id = match self.active_chain() {
            Ok(id) => id,
            Err(e) => return err(e),
        };
        if let Err(v) = self.verified_gate_within(chain_id, &b) {
            return blocked(&v).to_string();
        }
        let r = match self.resolve(&req, chain_id, &b) {
            Ok(r) => r,
            Err(e) => return err(e),
        };
        match self.sender_prepare(&r, &req, &b) {
            Ok(priced) => Self::quote_reply(&r, &priced).to_string(),
            Err(e) => err(e),
        }
    }

    fn send(&self, request_json: String) -> String {
        let b = Budget::new(SEND_BUDGET);
        let req: SendRequest = match serde_json::from_str(&request_json) {
            Ok(r) => r,
            Err(e) => return err(format!("invalid send request: {e}")),
        };
        let st = match self.state() {
            Ok(st) => st,
            Err(e) => return err(e),
        };
        // Read ONCE. Gating on one chain and pricing on another is how a send is checked
        // against a network it is not on.
        let chain_id = match st.settings.try_load() {
            Ok(s) => s.active_chain_id,
            Err(e) => return err(e.to_string()),
        };
        if let Err(v) = self.verified_gate_within(chain_id, &b) {
            return blocked(&v).to_string();
        }
        let r = match self.resolve(&req, chain_id, &b) {
            Ok(r) => r,
            Err(e) => return err(e),
        };
        let Some(t) = b.take(SENDER_BUDGET) else { return err("no time left to request approval") };
        let mut request = match Self::sender_request(&r, &req, t) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        // What a human reads at the moment of approval: exact to the last digit and in the
        // token's own units. Nobody can check a figure denominated in wei.
        let amount = units::format_exact(&r.amount.to_string(), r.decimals)
            .unwrap_or_else(|| r.amount.to_string());
        request["purpose"] = json!(send::purpose(&amount, &r.symbol, &r.from.to_string(), &r.to.to_string()));
        // Deliberately no hash: nothing is signed or broadcast until a human approves. The
        // sender's `{ ok, pending, requestId, handle }` is this module's own reply.
        relay(modules().tx_sender_module.send_with_timeout(&request.to_string(), t))
    }

    fn send_status(&self, request_id: String) -> String {
        let b = Budget::new(STATUS_BUDGET);
        let Some(t) = b.take(STATUS_BUDGET) else { return err("no time left to read the send") };
        relay(modules().tx_sender_module.send_status_with_timeout(&request_id, t))
    }

    fn cancel_send(&self, request_id: String) -> String {
        let b = Budget::new(READ_BUDGET);
        let Some(t) = b.take(RPC_BUDGET) else { return err("no time left to cancel the send") };
        relay(modules().tx_sender_module.cancel_send_with_timeout(&request_id, t))
    }

    fn refresh_tx_status(&self, address: String, hash_hex: String) -> String {
        let b = Budget::new(REFRESH_BUDGET);
        let Some(t) = b.take(REFRESH_BUDGET) else { return err("no time left to read the receipt") };
        relay(modules().tx_sender_module.refresh_tx_status_with_timeout(&address, &hash_hex, t))
    }

    fn get_tx_details(&self, address: String, hash_hex: String) -> String {
        let b = Budget::new(DETAILS_BUDGET);
        let Some(t) = b.take(DETAILS_BUDGET) else {
            // Not `err()`: this reply is rendered beside ONE transaction's rows, so even a
            // refusal has to name the hash it is about or it could land under another.
            return json!({ "ok": false, "hash": hash_hex, "error": "no time left to read the transaction" }).to_string();
        };
        relay(modules().tx_sender_module.tx_details_with_timeout(&address, &hash_hex, t))
    }

    fn suggest_fees(&self) -> String {
        let b = Budget::new(FEES_BUDGET);
        let chain_id = match self.active_chain() {
            Ok(id) => id,
            Err(e) => return err(e),
        };
        if let Err(v) = self.verified_gate_within(chain_id, &b) {
            return blocked(&v).to_string();
        }
        let Some(t) = b.take(RPC_BUDGET) else {
            return err("no time left to price the fee");
        };
        match modules().fee_module.suggest_fees_with_timeout(chain_id as i64, t) {
            Ok(reply) => reply,
            Err(e) => err(format!("{e:?}")),
        }
    }
}

// The registration hook. The generated provider glue DECLARES this symbol and the loader
// resolves it at dlopen; the author owes the definition. Omitting it links cleanly and
// segfaults inside `ensure_ready` at set_context time on macOS (lazy resolution, no hint);
// Linux at least says `undefined symbol: logos_module_install`.
#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<EthWalletBackendImpl>();
}
