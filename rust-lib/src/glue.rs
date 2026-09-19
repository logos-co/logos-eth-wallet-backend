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

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::{json, Value};

use crate::budget::{
    callee_deadline, Budget, BALANCES_BUDGET, CATALOGUE_BUDGET, DETAILS_BUDGET, FEES_BUDGET,
    HISTORY_BUDGET, INIT_BUDGET, OFFERED_BUDGET, PROBE_BUDGET, READ_BUDGET, REFRESH_BUDGET,
    RPC_BUDGET, ASSETS_BUDGET, SENDER_BUDGET, SEND_BUDGET, STARTUP_BUDGET, STATUS_BUDGET,
};
use crate::catalogue;
use crate::contacts::ContactsStore;
use crate::depinit::{self, Next};
use crate::history;
use crate::settings::{Settings, SettingsStore};
use crate::settings::TokenSort;
use crate::verified;

pub trait EthWalletBackendModule: Send + Sync + 'static {
    /// Enabled chains selected by the device scope, each carrying its current verdict.
    /// `{ ok, scope, networks: [{ chainId, name, nativeSymbol, testnet, rpcUrl,
    /// verifiedProxyMode, verifiedProxy }] }`.
    /// `rpcUrl`, `verifiedProxyMode` and `verifiedProxy` all come from `eth_rpc_module`, which
    /// owns them. All three are read-only here — a device-wide store shared with every wallet
    /// on the machine is configured in the `eth_rpc_ui` app, not from inside one wallet.
    ///
    /// Answers within a fixed budget however slow `eth_rpc` is. A network whose reads did
    /// not fit reports `verifiedProxyMode: "unknown"` and an empty `rpcUrl`.
    fn list_networks(&self) -> String;

    /// Relay the chain registry's enable switch.
    fn set_chain_enabled(&self, chain_id: i64, enabled: bool) -> String;
    /// Relay the device-wide `mainnets` | `testnets` | `both` scope selector.
    fn set_network_scope(&self, scope: String) -> String;

    /// `eth_rpc`'s verified-proxy verdict for every enabled in-scope chain:
    /// `{ ok, chains: [{ chainId, verdict }] }`.
    fn verified_proxy_state(&self) -> String;

    /// Tokens OFFERED on `chain_id`, native first: the built-in rows plus whatever
    /// the user turned on. `{ ok, chainId, tokenSort, tokens: [{ symbol, name, decimals,
    /// address?, native, builtin, inTokenList, metadataSource, logoURI? }] }`.
    ///
    /// `builtin` says the asset layer pins the address rather than accepting a user snapshot.
    /// `metadataSource` says who decorated the row:
    /// `native` | `allowlist` (ours, undecorated) | `custom` | `downloaded` | `embedded`
    /// (`token_list`'s own bucket labels, relayed rather than inferred) | `unknown` | `enabled`
    /// (a persisted snapshot the list no longer holds).
    fn list_tokens(&self, chain_id: i64) -> String;

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

    /// Native and token balances for `address` on every enabled chain in the device scope.
    /// The per-chain reads fan out concurrently, so another configured chain cannot multiply
    /// the interaction deadline. `{ ok, address, tokenSort, chains: [{ ok, chainId,
    /// balances: [{ symbol, address?, raw, decimals, native, builtin, display, exact,
    /// amountExact }], route } | { ok: false, chainId, error }] }`. `display` is bounded;
    /// `amountExact` carries every digit as a plain decimal string (`exact` is the older name
    /// for the same digits). All three are absent when a sub-call failed, so a view renders an
    /// em-dash and never a zero. A caller must not scale `raw` itself — a JS number loses
    /// digits above 2^53.
    ///
    /// EVERY offered token gets a row, including one the account holds none of. The array
    /// arrives ALREADY SORTED by the persisted `tokenSort` — comparing 18-decimal amounts is
    /// exact `U256` work and belongs where it is testable, not in QML.
    ///
    /// `route` is `eth_rpc`'s own label for the read — `verified` (proof-backed), `proxied`
    /// (forwarded on trust), `direct` (never touched the proxy) or `unknown`. Badge the
    /// balances on `route`, never on the network's mode.
    fn get_balances(&self, address: String) -> String;

    /// Transactions `tx_sender_module` broadcast for `address` on every chain in the current
    /// device scope, newest first — this wallet's own sends and any other app's calls from the
    /// same account. Only transactions the sender broadcast: there is no indexer.
    ///
    /// `{ ok, address, stillDue, stillDueAnyChain, unstored, unresolved, blockedChains,
    /// strandedNonces, transactions, decorationErrors? }`. A row this wallet sent as an ERC-20
    /// transfer reads back as one: `kind: "erc20"`, `to` the recipient, `value` the token
    /// amount at the token's decimals, `txTo` the contract. Another app's call keeps
    /// `kind: "call"` with its `label`, `origin` and `purpose`. Each row carries `stalled`,
    /// `unresolved` and `verificationBlocked`; `stillDue` covers the rows in THIS reply.
    fn get_history(&self, address: String) -> String;

    /// Fee tiers for `chain_id`, from `fee_module`. `{ ok, chainId, baseFeePerGas,
    /// source, tiers: { slow, normal, fast } }`; `source` distinguishes a real EIP-1559
    /// suggestion from the legacy `gasPrice` fallback.
    fn suggest_fees(&self, chain_id: i64) -> String;

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
}

/// The subscriptions this module keeps open on its dependencies. Each flag is held for as
/// long as its thread runs, so a feed that ends re-arms on the next read rather than going
/// quiet for the life of the process — unlike `watching_keystore`, which is armed once.
#[derive(Default)]
struct Feeds {
    chains: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    scope: Arc<AtomicBool>,
    tokens: Arc<AtomicBool>,
}

/// Run a subscription's listener thread, releasing `flag` when the feed ends.
fn listen<S: Send + 'static>(flag: Arc<AtomicBool>, sub: S, body: impl FnOnce(S) + Send + 'static) {
    std::thread::spawn(move || {
        body(sub);
        flag.store(false, Ordering::SeqCst);
    });
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
    let message = e.to_string();
    if let Ok(v) = serde_json::from_str::<Value>(&message) {
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            return v.to_string();
        }
    }
    json!({ "ok": false, "error": message }).to_string()
}

const NO_CONTEXT: &str = "module context not ready";

/// A reply from `tx_sender_module`, parsed, with its refusal surfaced as this module's own.
/// A transport error names the sender, so an operator can tell a sender that is down from a
/// sender that said no.
fn sender_reply(raw: Result<String, impl std::fmt::Debug>) -> Result<Value, String> {
    let raw = raw.map_err(|e| format!("tx_sender_module: {e:?}"))?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(v.to_string());
    }
    Ok(v)
}

/// A structured reply from the reusable asset provider. A provider refusal remains an
/// object (not a flattened sentence), including `verified_blocked` and its verdict.
fn assets_reply(raw: Result<String, impl std::fmt::Debug>) -> Result<Value, String> {
    let raw = raw.map_err(|e| format!("evm_assets_module: {e:?}"))?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(v.to_string());
    }
    Ok(v)
}

/// The same, for a `token_list_module` read: its refusal stays the object it was.
fn token_list_reply(raw: Result<String, impl std::fmt::Debug>) -> Result<Value, String> {
    let raw = raw.map_err(|e| format!("token_list_module: {e:?}"))?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(v.to_string());
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
    chain_id: i64,
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

fn parse_u64_any(s: &str) -> Option<u64> {
    let raw = s.trim();
    if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        raw.parse().ok()
    }
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
        listen(self.feeds.chains.clone(), sub, move |sub| {
            for ev in sub {
                if let Some(e) = eth_rpc_module::EthRpcModuleClient::decode_chain_config_changed(&ev)
                {
                    emit_networks_changed(e.chain_id);
                    // The record names the native asset, and its row heads every token list.
                    emit_tokens_changed(e.chain_id);
                }
            }
        });
    }

    fn watch_scope(&self) {
        if self.feeds.scope.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut c = modules().eth_rpc_module;
        let Ok(sub) = c.on_network_scope_changed() else {
            self.feeds.scope.store(false, Ordering::SeqCst);
            return;
        };
        listen(self.feeds.scope.clone(), sub, |sub| {
            for ev in sub {
                if eth_rpc_module::EthRpcModuleClient::decode_network_scope_changed(&ev).is_some() {
                    emit_networks_changed(-1);
                }
            }
        });
    }

    fn watch_chain_enabled(&self) {
        if self.feeds.enabled.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut c = modules().eth_rpc_module;
        let Ok(sub) = c.on_chain_enabled_changed() else {
            self.feeds.enabled.store(false, Ordering::SeqCst);
            return;
        };
        listen(self.feeds.enabled.clone(), sub, |sub| {
            for ev in sub {
                if let Some(e) = eth_rpc_module::EthRpcModuleClient::decode_chain_enabled_changed(&ev) {
                    emit_networks_changed(e.chain_id);
                }
            }
        });
    }

    /// Relay token_list's own feed: the rows it serves on a chain moved. The native row that
    /// heads them moves with the chain record, which `watch_chain_config` announces.
    fn watch_tokens(&self) {
        if self.feeds.tokens.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut c = modules().token_list_module;
        let Ok(sub) = c.on_tokens_updated() else {
            self.feeds.tokens.store(false, Ordering::SeqCst);
            return;
        };
        listen(self.feeds.tokens.clone(), sub, |sub| {
            for ev in sub {
                if let Some(e) = token_list_module::TokenListModuleClient::decode_tokens_updated(&ev) {
                    emit_tokens_changed(e.chain_id);
                }
            }
        });
    }

    /// Have eth_rpc seed its default chains. No `config_status` gate: it fills only what is
    /// absent and seeds a default chain at most once per device, so a store another app has
    /// already written to still gets the defaults it lacks.
    fn ensure_eth_rpc(&self, b: &Budget) {
        if self.deps.eth_rpc.load(Ordering::Relaxed) {
            return;
        }
        let Some(t) = b.take(INIT_BUDGET) else { return };
        let applied = modules().eth_rpc_module.init_defaults_with_timeout(t);
        if applied.map(|raw| depinit::reply_ok(&raw)).unwrap_or(false) {
            self.deps.eth_rpc.store(true, Ordering::Relaxed);
        }
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
        let raw = modules().eth_rpc_module.verified_proxy_status_with_timeout(chain_id as i64, t);
        Self::verdict_of(chain_id, raw)
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

    /// The rows token_list offers on `chain_id`, pinned then enabled: evm_assets' token
    /// descriptors, passed on unchanged. A local read, so it takes a small slice.
    fn offered_tokens(&self, chain_id: u64, b: &Budget) -> Result<Vec<Value>, String> {
        let t = b.take(OFFERED_BUDGET).ok_or("no time left to read offered tokens")?;
        let raw = modules().token_list_module.list_offered_with_timeout(chain_id as i64, t);
        let v = token_list_reply(raw)?;
        Ok(v.get("tokens").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// Ask the reusable asset module to resolve units, check one ERC-20 balance when needed,
    /// and build the single unsigned call. It receives only its slice of the send budget.
    fn build_transfer(&self, chain_id: u64, request_json: &str, b: &Budget) -> Result<Value, String> {
        // The candidates `token` and `tokenAddress` resolve against; native is implicit.
        let tokens = self.offered_tokens(chain_id, b)?;
        let t = b.take(ASSETS_BUDGET).ok_or("no time left to build the transfer")?;
        let mut request: Value = serde_json::from_str(request_json)
            .map_err(|e| format!("invalid send request: {e}"))?;
        let object = request.as_object_mut().ok_or("send request must be a JSON object")?;
        object.insert("tokens".into(), json!(tokens));
        if let Some(deadline) = callee_deadline(t) {
            object.insert("deadlineMs".into(), json!(deadline));
        }
        assets_reply(modules().evm_assets_module.build_transfer_with_timeout(
            chain_id as i64, &request.to_string(), t,
        ))
    }

    /// Convert a built asset transfer into the sender's request without reinterpreting any
    /// asset fact. Fee controls remain the wallet caller's choice.
    fn built_sender_request(
        built: &Value,
        req: &SendRequest,
        t: std::time::Duration,
    ) -> Result<Value, String> {
        let calls = built.get("calls").and_then(Value::as_array)
            .filter(|calls| calls.len() == 1).cloned()
            .ok_or("evm_assets_module did not build exactly one call")?;
        let mut v = json!({
            "chainId": built.get("chainId").cloned().unwrap_or(Value::Null),
            "from": built.get("from").cloned().unwrap_or(Value::Null),
            "calls": calls,
        });
        if let Some(x) = &req.tier { v["tier"] = json!(x); }
        if let Some(x) = &req.max_fee_per_gas { v["maxFeePerGas"] = json!(x); }
        if let Some(x) = &req.max_priority_fee_per_gas { v["maxPriorityFeePerGas"] = json!(x); }
        if let Some(x) = &req.gas_limit { v["calls"][0]["gasLimit"] = json!(x); }
        if let Some(n) = req.nonce { v["nonce"] = json!(n); }
        if let Some(d) = callee_deadline(t) { v["deadlineMs"] = json!(d); }
        Ok(v)
    }

    /// The assets module owns transfer facts; the sender owns nonce and pricing. This joins
    /// their replies and folds the route without reconstructing either answer.
    fn built_quote_reply(built: &Value, priced: &Value) -> Value {
        let mut v = built.clone();
        v.as_object_mut().map(|object| { object.remove("calls"); object.remove("purpose"); });
        v["amountSymbol"] = built.get("symbol").cloned().unwrap_or(Value::Null);
        v["amountDecimals"] = built.get("decimals").cloned().unwrap_or(Value::Null);
        v["token"] = if built.get("native").and_then(Value::as_bool) == Some(true) {
            Value::Null
        } else {
            built.get("symbol").cloned().unwrap_or(Value::Null)
        };
        for key in ["nonce", "gasLimit", "maxFeePerGas", "maxPriorityFeePerGas", "feeSource"] {
            v[key] = priced.get(key).cloned().unwrap_or(Value::Null);
        }
        for key in ["maxCostWei", "feeCeilingWei"] {
            for suffix in ["", "Display", "Exact"] {
                let field = format!("{key}{suffix}");
                if let Some(value) = priced.get(&field) { v[field] = value.clone(); }
            }
        }
        v["route"] = json!(verified::weakest_route(&[
            built.get("route").and_then(Value::as_str),
            priced.get("route").and_then(Value::as_str),
        ]));
        v["feeRoute"] = json!(verified::UNKNOWN_ROUTE);
        v
    }

    /// The chain registry, behind the lazy eth_rpc seeding retry: every read that needs a
    /// chain comes through here, so a startup that could not seed is retried by the next.
    fn chain_configs(&self, b: &Budget) -> Result<(String, Vec<Value>), String> {
        self.ensure_eth_rpc(b);
        let t = b.take(PROBE_BUDGET).ok_or("no time left to read the chain registry")?;
        let raw = modules()
            .eth_rpc_module
            .list_chain_configs_with_timeout(t)
            .map_err(|e| format!("{e:?}"))?;
        let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(v.to_string());
        }
        let scope = v.get("scope").and_then(Value::as_str).unwrap_or("mainnets").to_string();
        let chains = v.get("chains").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok((scope, chains))
    }

    fn in_scope_chain(&self, chain_id: u64, b: &Budget) -> Result<Value, String> {
        let (_, records) = self.chain_configs(b)?;
        records
            .into_iter()
            .find(|v| {
                v.get("chainId").and_then(Value::as_u64) == Some(chain_id)
                    && v.get("inScope").and_then(Value::as_bool) == Some(true)
            })
            .ok_or_else(|| format!("chain {chain_id} is not enabled in the current network scope"))
    }

    /// One network entry, preserving the wallet's existing public shape while sourcing every
    /// chain fact from eth_rpc's registry.
    fn network_json(&self, record: &Value, b: &Budget) -> Value {
        let chain_id = record.get("chainId").and_then(Value::as_u64).unwrap_or_default();
        let vp = self.verified_verdict_within(chain_id, b);
        json!({
            "chainId": chain_id,
            "key": "",
            "name": record.get("name").cloned().unwrap_or(Value::Null),
            "nativeSymbol": record.get("nativeSymbol").cloned().unwrap_or(Value::Null),
            "nativeDecimals": record.get("nativeDecimals").cloned().unwrap_or(Value::Null),
            "testnet": record.get("testnet").cloned().unwrap_or(Value::Null),
            "rpcUrl": record.get("endpoint").cloned().unwrap_or(json!("")),
            "enabled": record.get("enabled").cloned().unwrap_or(json!(true)),
            "inScope": record.get("inScope").cloned().unwrap_or(json!(false)),
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
        // Dependency defaults are best effort, and both share one startup budget.
        let b = Budget::new(STARTUP_BUDGET);
        self.ensure_eth_rpc(&b);
        self.ensure_token_list(&b);
        // After the state above exists: the relay's first act is a `list_accounts`, and a
        // consumer must never be told to re-read before this module can answer.
        self.watch_keystore();
        self.watch_sender();
        self.watch_chain_config();
        self.watch_chain_enabled();
        self.watch_scope();
        self.watch_tokens();
    }

    fn list_networks(&self) -> String {
        let b = Budget::new(READ_BUDGET);
        self.watch_chain_config();
        self.watch_chain_enabled();
        self.watch_scope();
        let (scope, all) = match self.chain_configs(&b) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        let configured: Vec<Value> = all.iter().map(|record| self.network_json(record, &b)).collect();
        let networks: Vec<Value> = configured.iter()
            .filter(|record| record.get("inScope").and_then(Value::as_bool) == Some(true))
            .cloned().collect();
        json!({ "ok": true, "scope": scope, "networks": networks,
                "configuredNetworks": configured }).to_string()
    }

    fn set_chain_enabled(&self, chain_id: i64, enabled: bool) -> String {
        if chain_id < 0 {
            return err(format!("chain {chain_id} is not a valid chain id"));
        }
        let b = Budget::new(READ_BUDGET);
        let Some(t) = b.take(RPC_BUDGET) else { return err("no time left to update the chain") };
        match modules().eth_rpc_module.set_chain_enabled_with_timeout(chain_id, enabled, t) {
            Ok(reply) => reply,
            Err(e) => err(format!("eth_rpc_module: {e:?}")),
        }
    }

    fn set_network_scope(&self, scope: String) -> String {
        let b = Budget::new(READ_BUDGET);
        let Some(t) = b.take(RPC_BUDGET) else { return err("no time left to update the network scope") };
        match modules().eth_rpc_module.set_network_scope_with_timeout(&scope, t) {
            Ok(reply) => reply,
            Err(e) => err(format!("eth_rpc_module: {e:?}")),
        }
    }

    fn verified_proxy_state(&self) -> String {
        let b = Budget::new(READ_BUDGET);
        let (_, records) = match self.chain_configs(&b) { Ok(v) => v, Err(e) => return err(e) };
        let chains: Vec<Value> = records.into_iter()
            .filter(|record| record.get("inScope").and_then(Value::as_bool) == Some(true))
            .filter_map(|record| record.get("chainId").and_then(Value::as_u64))
            .map(|chain_id| json!({
                "chainId": chain_id,
                "verdict": self.verified_verdict_within(chain_id, &b),
            }))
            .collect();
        json!({ "ok": true, "chains": chains }).to_string()
    }

    fn list_tokens(&self, chain_id: i64) -> String {
        if chain_id < 0 {
            return err(format!("chain {chain_id} is not a valid chain id"));
        }
        let b = Budget::new(READ_BUDGET);
        let s = match self.settings() {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        let id = chain_id as u64;
        if let Err(e) = self.in_scope_chain(id, &b) { return err(e); }
        self.ensure_token_list(&b);
        let tokens = match self.offered_tokens(id, &b) {
            Ok(rows) => json!(rows).to_string(),
            Err(e) => return err(e),
        };
        let Some(t) = b.take(CATALOGUE_BUDGET) else { return err("no time left to read offered assets") };
        match assets_reply(modules().evm_assets_module.list_assets_with_timeout(chain_id, &tokens, t)) {
            Ok(mut reply) => {
                reply["tokenSort"] = json!(s.token_sort.as_str());
                reply.to_string()
            }
            Err(e) => err(e),
        }
    }

    fn list_available_tokens(&self, chain_id: i64, query: String, offset: i64, limit: i64) -> String {
        if chain_id < 0 {
            return err(format!("chain {chain_id} is not a valid chain id"));
        }
        let b = Budget::new(READ_BUDGET);
        let chain_id = chain_id as u64;
        if let Err(e) = self.in_scope_chain(chain_id, &b) { return err(e); }
        let s = match self.settings() {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        self.ensure_token_list(&b);
        // The native row as evm_assets draws it off the chain record: its first, given no tokens.
        let Some(t) = b.take(PROBE_BUDGET) else { return err("no time left to read the token picker") };
        let listed = modules().evm_assets_module.list_assets_with_timeout(chain_id as i64, "[]", t);
        let native = match assets_reply(listed).map(|v| v["tokens"][0].clone()) {
            Ok(row) if row.is_object() => row,
            Ok(_) => return err("evm_assets_module listed no native asset"),
            Err(e) => return err(e),
        };
        let matches = catalogue::native_matches(&native, &query);
        let Some(t) = b.take(CATALOGUE_BUDGET) else { return err("no time left to read the token picker") };
        match token_list_reply(modules().token_list_module.list_available_with_timeout(
            chain_id as i64, &query, catalogue::provider_offset(matches, offset), limit, t,
        )) {
            Ok(page) => {
                let mut reply = catalogue::merge_native_page(&native, matches, page, offset, limit);
                reply["tokenSort"] = json!(s.token_sort.as_str());
                reply.to_string()
            }
            Err(e) => err(e),
        }
    }

    fn set_token_enabled(&self, chain_id: i64, address: String, enabled: bool) -> String {
        if chain_id < 0 {
            return err(format!("chain {chain_id} is not a valid chain id"));
        }
        let b = Budget::new(READ_BUDGET);
        if let Err(e) = self.in_scope_chain(chain_id as u64, &b) {
            return err(e);
        }
        self.watch_tokens();
        self.ensure_token_list(&b);
        let Some(t) = b.take(CATALOGUE_BUDGET) else { return err("no time left to update the token set") };
        match modules().token_list_module.set_token_enabled_with_timeout(chain_id, &address, enabled, t) {
            Ok(reply) => reply,
            Err(e) => err(format!("token_list_module: {e:?}")),
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
        self.watch_keystore();
        match modules().keystore_module.get_account_wallets() {
            Ok(reply) => reply,
            Err(e) => err(format!("{e:?}")),
        }
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
        let settings = match self.settings() {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        let registry_budget = Budget::new(READ_BUDGET);
        let (_, records) = match self.chain_configs(&registry_budget) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        let ids: Vec<u64> = records.into_iter()
            .filter(|record| record.get("inScope").and_then(Value::as_bool) == Some(true))
            .filter_map(|record| record.get("chainId").and_then(Value::as_u64))
            .collect();
        // `logos_module` is multi-concurrency and dependency clients are thread-safe. Keep
        // one handle per chain and join in registry order: wall-clock cost is one chain's
        // offered read and bounded Multicall round trip, while the stable order keeps
        // rendering deterministic.
        let chains: Vec<Value> = std::thread::scope(|scope| {
            let handles: Vec<_> = ids.into_iter().map(|chain_id| {
                let address = &address;
                let token_sort = settings.token_sort.as_str();
                (chain_id, scope.spawn(move || {
                    let b = Budget::new(BALANCES_BUDGET);
                    let tokens = match self.offered_tokens(chain_id, &b) {
                        Ok(rows) => json!(rows).to_string(),
                        // A token_list failure refuses this chain alone, in the per-chain shape.
                        Err(e) => return Ok(err(e)),
                    };
                    let Some(t) = b.take(BALANCES_BUDGET) else {
                        return Ok(err("no time left to read the balances"));
                    };
                    modules().evm_assets_module.get_balances_with_timeout(
                        chain_id as i64, address, &tokens, token_sort, t,
                    )
                }))
            }).collect();
            handles.into_iter().map(|(chain_id, handle)| match handle.join() {
                Ok(Ok(reply)) => match serde_json::from_str::<Value>(&reply) {
                    Ok(mut value) => {
                        if value.get("chainId").is_none() { value["chainId"] = json!(chain_id); }
                        value
                    }
                    Err(e) => json!({ "chainId": chain_id, "ok": false,
                                      "error": format!("unreadable assets reply: {e}") }),
                },
                Ok(Err(e)) => json!({ "chainId": chain_id, "ok": false,
                                      "error": format!("evm_assets_module: {e:?}") }),
                Err(_) => json!({ "chainId": chain_id, "ok": false,
                                  "error": "evm_assets_module balance worker panicked" }),
            }).collect()
        });
        json!({ "ok": true, "address": address, "tokenSort": settings.token_sort.as_str(),
                "chains": chains }).to_string()
    }

    fn get_history(&self, address: String) -> String {
        let registry_budget = Budget::new(READ_BUDGET);
        let (_, records) = match self.chain_configs(&registry_budget) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        let ids: std::collections::BTreeSet<u64> = records.into_iter()
            .filter(|record| record.get("inScope").and_then(Value::as_bool) == Some(true))
            .filter_map(|record| record.get("chainId").and_then(Value::as_u64))
            .collect();
        let b = Budget::new(HISTORY_BUDGET);
        let Some(t) = b.take(SENDER_BUDGET) else { return err("no time left to read the history") };
        // Chain zero asks the sender for this address across every chain it has recorded.
        let history = match sender_reply(
            modules().tx_sender_module.history_with_timeout(&address, 0, t),
        ) {
            Ok(value) => value,
            Err(e) => return err(e),
        };
        let history = history::scope(history, &ids);
        // Each chain decorates with its own offered tokens. One whose read failed decorates
        // with its native asset alone, and the reply says so.
        let mut tokens = serde_json::Map::new();
        let mut unread = Vec::new();
        for chain in history::chains(&history) {
            match self.offered_tokens(chain, &b) {
                Ok(rows) => { tokens.insert(chain.to_string(), json!(rows)); }
                Err(e) => unread.push(json!({ "chainId": chain, "error": e })),
            }
        }
        let Some(t) = b.take(CATALOGUE_BUDGET) else { return err("no time left to decorate the history") };
        match modules().evm_assets_module.decorate_history_with_timeout(
            &history.to_string(), &Value::Object(tokens).to_string(), t,
        ) {
            Ok(reply) => history::add_decoration_errors(reply, unread),
            Err(e) => err(format!("evm_assets_module: {e:?}")),
        }
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
        if req.chain_id < 0 { return err(format!("chain {} is not a valid chain id", req.chain_id)); }
        let chain_id = req.chain_id as u64;
        if let Err(e) = self.in_scope_chain(chain_id, &b) { return err(e); }
        let built = match self.build_transfer(chain_id, &request_json, &b) {
            Ok(built) => built,
            Err(e) => return err(e),
        };
        let Some(t) = b.take(SENDER_BUDGET) else { return err("no time left to price the send") };
        let request = match Self::built_sender_request(&built, &req, t) { Ok(v) => v, Err(e) => return err(e) };
        match sender_reply(modules().tx_sender_module.prepare_with_timeout(&request.to_string(), t)) {
            Ok(priced) => Self::built_quote_reply(&built, &priced).to_string(),
            Err(e) => err(e),
        }
    }

    fn send(&self, request_json: String) -> String {
        let b = Budget::new(SEND_BUDGET);
        let req: SendRequest = match serde_json::from_str(&request_json) {
            Ok(r) => r,
            Err(e) => return err(format!("invalid send request: {e}")),
        };
        if req.chain_id < 0 { return err(format!("chain {} is not a valid chain id", req.chain_id)); }
        // Read the request's chain once and validate it against the shared registry before
        // building. The assets reply stamps the same id into the sender request.
        let chain_id = req.chain_id as u64;
        if let Err(e) = self.in_scope_chain(chain_id, &b) { return err(e); }
        let built = match self.build_transfer(chain_id, &request_json, &b) {
            Ok(built) => built,
            Err(e) => return err(e),
        };
        let Some(t) = b.take(SENDER_BUDGET) else { return err("no time left to request approval") };
        let mut request = match Self::built_sender_request(&built, &req, t) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        request["purpose"] = built.get("purpose").cloned().unwrap_or(Value::Null);
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

    fn suggest_fees(&self, chain_id: i64) -> String {
        if chain_id < 0 {
            return err(format!("chain {chain_id} is not a valid chain id"));
        }
        let b = Budget::new(FEES_BUDGET);
        if let Err(e) = self.in_scope_chain(chain_id as u64, &b) { return err(e); }
        let Some(t) = b.take(RPC_BUDGET) else {
            return err("no time left to price the fee");
        };
        match modules().fee_module.suggest_fees_with_timeout(chain_id, t) {
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
