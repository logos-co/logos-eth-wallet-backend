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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use alloy::primitives::U256;
use serde_json::{json, Value};

use crate::budget::{
    Budget, CATALOGUE_BUDGET, DETAILS_BUDGET, INIT_BUDGET, PROBE_BUDGET, READ_BUDGET,
    REFRESH_BUDGET, RPC_BUDGET, SEND_BUDGET, STARTUP_BUDGET, SWEEP_BUDGET,
};
use crate::depinit::{self, Next};
use crate::gate::{self, Gate};
use crate::details;
use crate::history::{self, History, TxRecord};
use crate::send::{self, BroadcastClaim, SendJob, SendLedger, SendStatus};
use crate::settings::{Settings, SettingsStore};
use crate::sweep::{history_reply, SweepOutcome, GAS_PRICE_DECIMALS, SWEEP_MAX};
use crate::txbuild::parse_u256_any;
use crate::verified::{self, unwrap_answer, unwrap_rpc, Answer};
use crate::tokens::{Token, TokenSort};
use crate::{networks, tokens, txbuild, units};

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
    /// empty query matches everything. A `limit` of zero or less is no limit. `total` counts
    /// the matches BEFORE the cut and `shown` after, so a view can say what it is hiding
    /// rather than presenting a truncated list as the whole answer.
    ///
    /// The embedded Uniswap list is overwhelmingly mainnet, so on sepolia and hoodi `listed`
    /// is legitimately 0 and the reply carries the built-in rows alone. That is an ANSWER:
    /// `ok` stays true, and `listError` — present only when the `token_list` call itself
    /// failed — is what tells an empty catalogue from an unread one.
    fn list_available_tokens(&self, chain_id: i64, query: String, limit: i64) -> String;

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
    /// export one — those are the custodian's, and reach the keystore only via `keystore_ui`.
    fn list_accounts(&self) -> String;

    /// Account names, `{ ok, labels: { "<lowercase hex, no 0x>": "<name>" } }`, relayed from
    /// the keystore verbatim. `keystore_module.get_labels` is ungated — a label is not a
    /// secret — and this passthrough keeps a view's dependency list at exactly one module.
    ///
    /// The keys are `vault_name` form and `list_accounts` answers EIP-55 checksummed
    /// addresses, so the two never match textually and a lookup must normalise.
    fn get_account_labels(&self) -> String;

    /// Native and token balances for `address` on the active network, in one Multicall3
    /// round-trip. `{ ok, chainId, address, tokenSort, balances: [{ symbol, address?, raw,
    /// decimals, native, builtin, display, exact, amountExact }], route }`. `display` is
    /// bounded; `amountExact` carries every digit, as a plain decimal string (`exact` is the
    /// older name for the same digits). All three are absent when the sub-call failed, so a
    /// view renders an em-dash and never a zero. A caller must not scale `raw` itself — a JS
    /// number loses digits above 2^53.
    ///
    /// EVERY offered token gets a row, including one the account holds none of: a token the
    /// user turned on and then cannot find reads as the wallet having lost it. The array
    /// arrives ALREADY SORTED by the persisted `tokenSort` — comparing 18-decimal amounts is
    /// exact `U256` work and belongs where it is testable, not in QML.
    ///
    /// `route` is `eth_rpc`'s own label for the read — `verified` (proof-backed), `proxied`
    /// (forwarded on trust), `direct` (never touched the proxy) or `unknown`. Badge the
    /// balances on `route`, never on the network's mode.
    fn get_balances(&self, address: String) -> String;

    /// Locally recorded transactions for `address`, newest first, scoped to the active
    /// network. Only transactions this wallet broadcast — there is no indexer.
    ///
    /// `{ ok, chainId, address, stillDue, stillDueAnyChain, blockedChains, transactions }`.
    /// Each row carries its stored fields, `stalled` (pending, past the give-up horizon and
    /// no longer polled) and `verificationBlocked` (frozen because its chain's proxy is
    /// blocking). `stillDue` covers the rows in THIS reply — stop your poll timer on it;
    /// `stillDueAnyChain` covers every chain. `blockedChains` explains a frozen row.
    fn get_history(&self, address: String) -> String;

    /// Fee tiers for the active network, from `fee_module`. `{ ok, chainId, baseFeePerGas,
    /// source, tiers: { slow, normal, fast } }`; `source` distinguishes a real EIP-1559
    /// suggestion from the legacy `gasPrice` fallback.
    fn suggest_fees(&self) -> String;

    /// Quote a send without doing anything: resolves the fee through `fee_module`, reads the
    /// nonce, and refuses up front if the balance cannot cover value plus the fee ceiling.
    ///
    /// `request_json`: `{ from, to, amount | amountUnits, token?, tier?, maxFeePerGas?,
    /// maxPriorityFeePerGas?, gasLimit?, nonce? }`. `amount` is base units, `amountUnits` is
    /// what the user typed in TOKEN units ("0.1" ETH, not 10^17 wei); exactly one of the two,
    /// because they mean different things and only one can be what the caller meant. `token`
    /// is a symbol or contract address, absent for a native send. Any explicit fee field is
    /// used verbatim — the user overrules the suggestion, never the other way round.
    ///
    /// Returns `{ ok, chainId, from, to, amount, amountDisplay, amountExact, amountSymbol,
    /// amountDecimals, nativeSymbol, token?, nonce, gasLimit, maxFeePerGas,
    /// maxPriorityFeePerGas, maxCostWei(+Display/Exact), feeCeilingWei(+Display/Exact),
    /// feeSource, route, feeRoute }`. `feeCeilingWei` is `maxFeePerGas × gasLimit` — a
    /// ceiling, never a price, so a view must say "at most". `route` labels the
    /// balance and nonce reads; `feeRoute` is always `unknown`, because `fee_module` emits no
    /// label and its figures are never proof-backed. No approval is requested and no nonce
    /// is reserved, so it is safe to call on every keystroke.
    fn prepare_send(&self, request_json: String) -> String;

    /// Ask a human to approve a send. Takes the same `request_json` as `prepare_send`.
    ///
    /// Returns `{ ok, pending: true, requestId }` and **never a transaction hash** — nothing
    /// has been signed or broadcast at this point. The human approves in `signer_ui`; drive
    /// the rest with `send_status`.
    fn send(&self, request_json: String) -> String;

    /// Advance a pending send and report where it got to. Poll this.
    ///
    /// When the human has approved, this collects the signature, broadcasts it exactly once
    /// and records it in history. `{ ok, requestId, status, hash?, route?, reason? }` where
    /// `status` is `awaitingApproval` | `broadcasting` | `stuck` | `broadcast` | `rejected` |
    /// `cancelled` | `failed`. The first three are not settled states: `broadcasting` means
    /// the signed transaction is with a node, and `stuck` that it has not answered — neither
    /// may be retried or cancelled, and only `stuck` carries a `reason` without failing.
    /// `route` accompanies a broadcast and is never `verified`: nothing proves a send was
    /// accepted.
    fn send_status(&self, request_id: String) -> String;

    /// Withdraw a send that has not been approved yet, releasing its reserved nonce.
    fn cancel_send(&self, request_id: String) -> String;

    /// Re-read one recorded transaction's receipt on ITS OWN chain and update the stored
    /// status. `{ ok, hash, chainId, status, route }` — `pending` | `confirmed` | `failed`.
    /// `route` is never `verified`: a receipt is forwarded on trust, not proved.
    fn refresh_tx_status(&self, address: String, hash_hex: String) -> String;

    /// The transaction- and block-level fields a RECEIPT does not carry, for one recorded
    /// transaction on ITS OWN chain. At most two RPCs, and the second is skipped outright when
    /// the row already stores what it would answer — so a send recorded by this build costs
    /// one call, and the method gets cheaper as the data model improves.
    ///
    /// `{ ok, hash, chainId, route, fetchedAt, gasPriceUnit, block?: { number, timestamp },
    /// transaction?: { gasLimit?, maxPriorityFeePerGas(+Display/Exact)? }, blockError?,
    /// transactionError? }`. The two legs are independent: `ok` is true when EITHER landed, and
    /// the failed one's own words come back beside the fields it could not fill.
    ///
    /// `route` is never `verified` — neither method is proof-backed, so nothing fetched here
    /// may wear a verified badge. Every reply names the `hash` it is about, refusals included,
    /// so a view can never render one transaction's detail under another's.
    fn get_tx_details(&self, address: String, hash_hex: String) -> String;

    /// Poll receipts for this address's still-pending transactions, on each row's OWN chain,
    /// and update their stored status. `{ ok, address, polled, changed, blocked,
    /// blockedChains, stillDue }`. `blocked` counts the rows skipped by their chain's proxy
    /// verdict and `blockedChains` says which rows, on which chain, and why — a row on a
    /// non-active chain is explained nowhere else. `stillDue` is false once no row can move
    /// again, which is when a caller's poll timer should stop.
    fn refresh_pending(&self, address: String) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

pub trait EthWalletBackendModuleEvents {
    fn balances_updated(&self, address: String);
    fn active_chain_changed(&self, chain_id: i64);
    fn tx_status_changed(&self, hash_hex: String);
    /// A pending send changed state — approved, rejected, broadcast or failed.
    fn send_status_changed(&self, request_id: String);
    /// The keystore's accounts moved — the set itself, or the names they are shown under.
    /// Relayed from `keystore_module::accounts_changed`; `count` is carried verbatim and is
    /// ADVISORY, not a change detector: a rename does not move it. Re-read both
    /// `list_accounts` and `get_account_labels` on it.
    fn accounts_changed(&self, count: i64);
    /// The set of tokens OFFERED on `chain_id` moved. Not `balances_updated`: no amount
    /// moved and there is no address to name, while every address on that chain now has a row
    /// more or a row fewer. Chain-scoped, so a wallet on another network can ignore it.
    fn tokens_changed(&self, chain_id: i64);
    /// The balance-row order moved. Device-wide and chainless, unlike `tokens_changed`:
    /// nothing was offered or withdrawn, and the same rows arrive re-ordered on every network.
    fn token_sort_changed(&self, order: String);
    /// A recorded row for `address` appeared or changed with no hash for `tx_status_changed`
    /// to name — a send is written to history BEFORE it is broadcast, and on the arm where the
    /// node returns no hash it never gets one.
    fn history_changed(&self, address: String);
    /// What `eth_rpc_module` reports for one chain moved — its endpoint, its transport, or its
    /// verified-proxy mode. Relayed from `eth_rpc_module`, which owns that record: this wallet
    /// serves it through `list_networks`, and a view one hop out cannot subscribe to eth_rpc
    /// without taking a token for its whole surface. Re-read `list_networks`.
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
    /// Built once with the module and never replaced. `on_context_ready` can be called
    /// again — a re-init installs a fresh `State` — and a second ledger would drop every
    /// reservation at once, including those protecting transactions already on chain.
    sends: Arc<SendLedger>,
    /// Whether the keystore relay is armed. `on_context_ready` can run again, and a second
    /// listener thread would sit on a channel nothing closes for the life of the process.
    watching_keystore: AtomicBool,
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
    history: History,
    /// Shared with the module, not owned here: see `EthWalletBackendImpl::sends`.
    sends: Arc<SendLedger>,
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

/// A priced send: what `prepare_send` reports and what `send` acts on.
struct Quote {
    chain_id: u64,
    from: alloy::primitives::Address,
    to: alloy::primitives::Address,
    amount: U256,
    token: Option<crate::tokens::Token>,
    /// What `amount` is denominated in. Carried so every rendering — the reply, the signer
    /// intent, the history row — reads the same units without re-deriving them.
    decimals: u8,
    symbol: String,
    native_symbol: String,
    nonce: u64,
    gas_limit: u64,
    max_fee: U256,
    max_priority: U256,
    fee_source: String,
    /// The weakest route behind the balance and nonce reads. Says nothing about the fee.
    route: String,
}

fn parse_u64_any(s: &str) -> Option<u64> {
    parse_u256_any(s).and_then(|v| u64::try_from(v).ok())
}



impl EthWalletBackendImpl {
    /// The state, with the guard already dropped — the only lock this file takes. It used to
    /// run a closure under the guard, and six entry points made an IPC call in there. An
    /// owned handle is not a convention to remember: the guard is gone before this returns.
    fn state(&self) -> Result<Arc<State>, String> {
        let guard = self.state.read().map_err(|_| "state lock poisoned".to_string())?;
        guard.clone().ok_or_else(|| NO_CONTEXT.to_string())
    }

    /// The whole settings file. A file read; no lock and no call. An unreadable settings file
    /// is an error, never chain 1: every caller here gates, prices or labels on this answer.
    ///
    /// Read whole because the callers that want the active network usually want that
    /// network's enabled tokens too, and two reads are two snapshots that can straddle a
    /// change — which is how the balance list ends up naming a token the send path refuses.
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

    /// Arm the gate feed. Everything the mode cache is allowed to remember rests on this
    /// subscription: it is what turns "verification is off for this chain" from a reading
    /// taken once into a fact someone is obliged to correct. The cache goes live only inside
    /// the thread, once the subscription exists, and dies with it — so the two windows where
    /// nobody would tell us the user switched verification ON are both windows in which
    /// nothing is trusted.
    fn watch_gate(&self) {
        if self.feeds.gate.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut c = modules().eth_rpc_module;
        let Ok(sub) = c.on_verified_proxy_mode_changed() else {
            self.feeds.gate.store(false, Ordering::SeqCst);
            return;
        };
        let cache = self.gate.clone();
        listen(self.feeds.gate.clone(), sub, move |sub| {
            cache.feed_live();
            for ev in sub {
                let Some(e) =
                    eth_rpc_module::EthRpcModuleClient::decode_verified_proxy_mode_changed(&ev)
                else {
                    // An event we cannot read is a contract we no longer share, and it names
                    // no chain to invalidate. Drop the lot rather than guess.
                    break;
                };
                cache.told(e.chain_id as u64, &e.mode);
                emit_networks_changed(e.chain_id);
            }
            cache.feed_dead();
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
    /// cannot be read. Never falls back to `off`. Unbounded, for the paths where a refusal IS
    /// the whole answer — balances, the two send entry points, `suggest_fees` and the verdict
    /// poll — so a slow probe there is not turned into one.
    fn verified_verdict(&self, chain_id: u64) -> Value {
        self.watch_gate();
        let ticket = self.gate.ticket();
        let raw = modules().eth_rpc_module.verified_proxy_status(chain_id as i64);
        let v = Self::verdict_of(chain_id, raw);
        self.gate.learned(chain_id, v.get("mode").and_then(Value::as_str), ticket);
        v
    }

    /// The bounded twin, charged to a shared budget. For the paths that REPORT the verdict
    /// rather than enforce it — there an `unknown` is a label on a screen, not a locked wallet
    /// — and, through `verified_gate_within`, for the two a button drives.
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
    fn verified_gate(&self, chain_id: u64) -> Result<(), Value> {
        if self.gate.gate(chain_id) == Gate::Open {
            return Ok(());
        }
        Self::gate_of(self.verified_verdict(chain_id))
    }

    /// The bounded twin, charged to the SAME budget as the calls behind it. For the two paths
    /// a button drives: there the gate is time a user is watching, and an unbounded probe puts
    /// the method past the view's own deadline, which then reports a backend that never
    /// answered. A probe that outruns the budget still refuses — it says so in `detail`.
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

    /// Poll due receipts for `address`, each on ITS OWN chain, and update the stored rows.
    /// An `Err` is never a status — the row stays pending and the stamped poll time is the
    /// backoff. The verdict is read once per distinct chain, not once per record.
    /// Rows are collected through History's lock, receipts fetched holding nothing, results
    /// applied back through that lock — which re-reads the file, so the apply is a
    /// compare-and-set rather than a blind write from this snapshot. Bounded as a whole:
    /// eleven round trips at worst, and `get_history` is a read a view polls.
    fn sweep(&self, st: &State, address: &str, b: &Budget) -> SweepOutcome {
        let now = history::now_secs();
        let mut out = SweepOutcome::default();
        let mut failures: HashMap<u64, u32> = HashMap::new();
        let mut blocking: HashMap<u64, Option<Value>> = HashMap::new();

        for rec in st.history.pending_due(address, now, SWEEP_MAX) {
            // Two consecutive errors on a chain drop it for the rest of this sweep.
            if failures.get(&rec.chain_id).copied().unwrap_or(0) >= 2 {
                continue;
            }
            // Bounded, unlike the paths that refuse outright: a row this skips is DISCLOSED in
            // `blockedChains` and retried on the next poll, so an expired probe costs one
            // degraded cycle rather than a wallet that shows nothing.
            let gated = blocking.entry(rec.chain_id).or_insert_with(|| {
                let v = self.verified_verdict_within(rec.chain_id, b);
                verified::is_blocking(&v).then_some(v)
            });
            if let Some(verdict) = gated {
                out.blocked
                    .entry(rec.chain_id)
                    .or_insert_with(|| (Vec::new(), verdict.clone()))
                    .0
                    .push(rec.hash.clone());
                continue;
            }
            let Some(t) = b.take(RPC_BUDGET) else { break };
            let receipt = modules()
                .eth_rpc_module
                .get_transaction_receipt_with_timeout(rec.chain_id as i64, &rec.hash, t)
                .map_err(|e| format!("{e:?}"))
                .and_then(|raw| unwrap_rpc(&raw));
            out.polled += 1;
            match receipt {
                Ok(r) => {
                    failures.insert(rec.chain_id, 0);
                    if st.history.apply_receipt(&rec, &r, now) {
                        out.changed += 1;
                        out.confirmed |= history::classify_receipt(&r) == "confirmed";
                        emit_tx_status_changed(&rec.hash);
                    }
                }
                Err(_) => {
                    *failures.entry(rec.chain_id).or_insert(0) += 1;
                    st.history.apply_receipt(&rec, &Value::Null, now);
                }
            }
        }
        out.still_due = st.history.has_live(address, now);
        // Announced HERE, not in `refresh_pending`: `get_history` sweeps too, and a row that
        // confirmed under it moved the balance with nothing saying so. `confirmed` is set
        // only where `apply_receipt` actually moved a row, so this fires on a transition.
        if out.confirmed {
            emit_balances_updated(address);
        }
        out
    }


    /// Price, commit, then ask for approval — the commit under the ledger's lock rather than
    /// around the call. Two concurrent sends both reach `open` and take consecutive nonces;
    /// what the old shape got wrong was the other direction, where five `?`s could return
    /// without releasing the nonce and the job did not exist until after the approval.
    fn request_send(
        &self,
        st: &State,
        req: &SendRequest,
        chain_id: u64,
    ) -> Result<String, String> {
        let b = Budget::new(SEND_BUDGET);
        let mut q = self.quote(req, chain_id, &b)?;

        // `latest` does not count a broadcast-but-unmined transaction and the verified path
        // refuses `pending`, so this reservation is all that stops a clash. The guard owns it
        // from here: every path out short of `commit` hands it back.
        let guard = st.sends.open(chain_id, &q.from.to_string(), q.nonce, req.nonce, || {
            st.settings.try_load().map(|s| s.active_chain_id).map_err(|e| e.to_string())
        })?;
        q.nonce = guard.claim().nonce;

        let tx = self.unsigned_tx(&q)?;
        let tx_input = tx.get("data").and_then(Value::as_str).map(str::to_string);
        // What a human reads at the moment of approval: exact to the last digit and in the
        // token's own units. Nobody can check a figure denominated in wei.
        let amount = units::format_exact(&q.amount.to_string(), q.decimals)
            .unwrap_or_else(|| q.amount.to_string());
        let intent = json!({
            "address": q.from.to_string(),
            "purpose": format!("Send {amount} {}", q.symbol),
            "legs": [{ "kind": "tx", "chain_id": chain_id, "tx": tx }],
        });

        // Bounded: this registers the request, it does not wait for the human. A late deadline
        // costs a stray prompt whose signature is never fetched — no money moves.
        let t = b.take(INIT_BUDGET).ok_or("no time left to request approval")?;
        let raw = modules()
            .keystore_module
            .request_approval_with_timeout(&intent.to_string(), t)
            .map_err(|e| format!("{e:?}"))?;
        let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the keystore refused the approval request")
                .to_string());
        }
        let handle = v.get("handle").and_then(Value::as_str).unwrap_or_default().to_string();
        let receipt = v.get("receipt").and_then(Value::as_str).unwrap_or_default().to_string();

        let job = SendJob {
            request_id: format!("snd_{handle}"),
            handle,
            receipt,
            chain_id,
            from: q.from.to_string(),
            to: q.to.to_string(),
            value: q.amount.to_string(),
            kind: if q.token.as_ref().map(|t| t.native).unwrap_or(true) {
                "native".into()
            } else {
                "erc20".into()
            },
            token: q.token.as_ref().and_then(|t| t.address.clone()),
            nonce: q.nonce,
            gas_limit: q.gas_limit,
            max_fee: q.max_fee.to_string(),
            max_priority: q.max_priority.to_string(),
            token_symbol: q.token.as_ref().map(|t| t.symbol.clone()),
            token_decimals: q.token.as_ref().map(|t| t.decimals),
            tx_input,
            status: SendStatus::AwaitingApproval,
            broadcast: None,
            // Set from the claim by `commit`; a caller does not get to name it.
            replaces: None,
        };
        let request_id = job.request_id.clone();
        guard.commit(job);
        Ok(request_id)
    }

    /// Advance one pending send. Four outbound calls, none under a lock, and they need no
    /// consistent view of each other: the claim belongs immediately before the one call that
    /// moves money. Taken before `fetch_result` instead, as it used to be, one transient
    /// failure left the job claimed and reporting `awaitingApproval` for ever.
    ///
    /// The claim hands back a ticket, and from then on nothing else may settle this job. A
    /// concurrent dispatch that read the job before the claim bounces off it rather than
    /// failing a transaction already on its way to a node.
    fn advance_send(&self, request_id: &str) -> Result<Value, String> {
        let st = self.state()?;
        let b = Budget::new(SEND_BUDGET);
        let now = history::now_secs();
        let job =
            st.sends.get(request_id).ok_or_else(|| format!("no send with id '{request_id}'"))?;
        if job.status.is_terminal() {
            return Ok(Self::job_reply(&job, now));
        }
        // Another dispatch owns the broadcast. Going on would ask the keystore about a request
        // it has already answered and read the absence of a signature as a failure — settling
        // a transaction that is on its way, and handing its nonce to the next send.
        if job.broadcast_started() {
            return Ok(Self::job_reply(&job, now));
        }

        let t = b.take(RPC_BUDGET).ok_or("no time left to read the approval")?;
        let raw = modules()
            .keystore_module
            .approval_status_with_timeout(&job.handle, &job.receipt, t)
            .map_err(|e| format!("{e:?}"))?;
        let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            let reason = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the keystore lost this request")
                .to_string();
            return self.settle(&st, request_id, SendStatus::Failed { reason });
        }
        match v.get("state").and_then(Value::as_str).unwrap_or("") {
            // Re-read: a cancel may have landed while the keystore was answering.
            "offered" | "rendered" => {
                return Ok(Self::job_reply(&st.sends.get(request_id).unwrap_or(job), now))
            }
            "settled" => {}
            other => return Err(format!("unknown approval state '{other}'")),
        }
        match v.get("reason").and_then(Value::as_str).unwrap_or("approved") {
            "approved" => {}
            "rejected" => return self.settle(&st, request_id, SendStatus::Rejected),
            r => {
                return self.settle(&st, request_id, SendStatus::Failed { reason: r.to_string() })
            }
        }

        // Approved. Fetching the signature is a read and is safe to repeat, so it happens
        // BEFORE the claim: a failure here leaves the send exactly as it was, and the next
        // poll tries again.
        let t = b.take(RPC_BUDGET).ok_or("no time left to collect the signature")?;
        let fetched = modules()
            .keystore_module
            .fetch_result_with_timeout(&job.handle, &job.receipt, t)
            .map_err(|e| format!("{e:?}"))?;
        let fv: Value = serde_json::from_str(&fetched).map_err(|e| e.to_string())?;
        // `signed`, not `results` — the documented key, and the one the keystore emits.
        let raw_tx = fv
            .get("signed")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(raw_tx) = raw_tx else {
            let reason = "the approval carried no signature".to_string();
            return self.settle(&st, request_id, SendStatus::Failed { reason });
        };

        // Claim, record, then broadcast. The ticket is the only key to this job from here on,
        // and it is the whole answer to a broadcast that never returns: the job cannot be
        // settled behind our back, and after STUCK_AFTER_SECS it reports `stuck` rather than
        // wedging. The claim burns the nonce IN MEMORY, which no restart can read.
        let ticket = match st.sends.claim_broadcast(request_id, now) {
            BroadcastClaim::Claimed(t) => t,
            // Another poll is inside the broadcast right now; it will settle the job.
            BroadcastClaim::InFlight(j) | BroadcastClaim::Settled(j) => {
                return Ok(Self::job_reply(&j, now))
            }
            BroadcastClaim::Unknown => {
                return Err(format!("no send with id '{request_id}'"))
            }
        };

        // WRITE AHEAD. The record used to go down after the broadcast returned, and on a
        // failed broadcast not at all, so a crash inside the RPC lost a number that had
        // already left. The intent goes first; the outcome only completes it, and a row that
        // cannot be written means no send — `broadcast` takes the proof.
        let recorded = match st.history.record_intent(request_id, Self::intent_row(&job)) {
            Ok(r) => r,
            Err(reason) => return self.settle_owned(&st, &ticket, SendStatus::Failed { reason }),
        };
        // A row exists from here on and has no hash yet, so `tx_status_changed` cannot name
        // it. A history view learns of it now rather than only if the broadcast answers.
        emit_history_changed(&job.from);

        let (hash, route) = match self.broadcast(&recorded, job.chain_id, &raw_tx) {
            Ok(a) => match a.value.as_str().map(str::to_string).filter(|h| !h.is_empty()) {
                Some(h) => (h, verified::weakest_route(&[a.route.as_deref()])),
                None => {
                    // The transaction may well be on-chain; we simply cannot follow it. The
                    // nonce stays held for exactly that reason — see `settle_locked` — and
                    // the row stays `unknown` on disk, so the next process holds it too.
                    let reason =
                        "the node accepted the transaction but returned no hash".to_string();
                    if st.history.leave_unknown(&recorded, &reason) {
                        emit_history_changed(&job.from);
                    }
                    return self.settle_owned(&st, &ticket, SendStatus::Failed { reason });
                }
            },
            Err(reason) => {
                if st.history.leave_unknown(&recorded, &reason) {
                    emit_history_changed(&job.from);
                }
                return self.settle_owned(&st, &ticket, SendStatus::Failed { reason });
            }
        };

        // The intent becomes an ordinary pollable row. Nothing new is recorded here: the
        // evidence has been on disk since before the transaction left.
        let took_hash = st.history.resolve_broadcast(&recorded, &hash, history::now_secs());
        let _ = modules().keystore_module.ack_result_with_timeout(
            &job.handle,
            &job.receipt,
            RPC_BUDGET,
        );
        // Only if the row actually took it: otherwise this names a hash history does not hold.
        if took_hash {
            emit_tx_status_changed(&hash);
        }
        self.settle_owned(&st, &ticket, SendStatus::Broadcast { hash, route })
    }

    /// The one call that moves money, and the only site allowed to make it. It takes
    /// `Recorded`, which only `History::record_intent` produces, so broadcasting before the
    /// record is written is not something this file can express — the ordering is a type,
    /// not a rule each new path has to remember.
    ///
    /// Deliberately UNBOUNDED, alone among the four: a deadline here does not stop the
    /// transaction, it only stops us learning its hash.
    fn broadcast(
        &self,
        _recorded: &history::Recorded,
        chain_id: u64,
        raw_tx: &str,
    ) -> Result<Answer, String> {
        modules()
            .eth_rpc_module
            .send_raw_transaction(chain_id as i64, raw_tx)
            .map_err(|e| format!("{e:?}"))
            .and_then(|r| unwrap_answer(&r))
    }

    /// The durable record of one send, as it goes down BEFORE the broadcast. Every field but
    /// the hash is known from the quote the user approved; the hash is what the broadcast is
    /// for, and `record_intent` supplies the status.
    fn intent_row(j: &SendJob) -> TxRecord {
        TxRecord {
            chain_id: j.chain_id,
            from: j.from.clone(),
            to: j.to.clone(),
            value: j.value.clone(),
            kind: j.kind.clone(),
            token: j.token.clone(),
            timestamp: history::now_secs(),
            nonce: Some(j.nonce),
            gas_limit: Some(j.gas_limit),
            max_fee_per_gas: Some(j.max_fee.clone()),
            max_priority_fee_per_gas: Some(j.max_priority.clone()),
            fee_ceiling_wei: history::fee_ceiling_wei(&j.max_fee, j.gas_limit),
            token_symbol: j.token_symbol.clone(),
            token_decimals: j.token_decimals,
            tx_input: j.tx_input.clone(),
            // The receipt has not landed yet; the poll fills the rest.
            ..Default::default()
        }
    }

    /// Re-read one row's receipt on its own chain and settle it. Bounded AS A WHOLE, gate
    /// included: this is a button, and an unbounded probe in front of the receipt read is up
    /// to twenty seconds of frozen wallet. A probe the budget cuts short still refuses — the
    /// verdict it returns carries its own reason, so a timeout is not reported as a freeze.
    fn refresh_one(&self, address: &str, hash_hex: &str) -> Result<Value, String> {
        let st = self.state()?;
        // The record's own chain, not the active one: switching networks must not send every
        // refresh to the wrong node and re-affirm `pending` forever.
        let rec = st
            .history
            .find(address, hash_hex)
            .ok_or_else(|| format!("no recorded transaction with hash {hash_hex}"))?;
        let b = Budget::new(REFRESH_BUDGET);
        if let Err(v) = self.verified_gate_within(rec.chain_id, &b) {
            return Ok(blocked(&v));
        }
        let t = b.take(RPC_BUDGET).ok_or("no time left to read the receipt")?;
        let raw = modules()
            .eth_rpc_module
            .get_transaction_receipt_with_timeout(rec.chain_id as i64, &rec.hash, t)
            .map_err(|e| format!("{e:?}"))?;
        let Answer { value: receipt, route } = unwrap_answer(&raw)?;
        let status = history::classify_receipt(&receipt);
        // Exactly one of two concurrent refreshes of the same row sees `true` here — the
        // apply compares against what is on disk — so the event is announced once.
        if st.history.apply_receipt(&rec, &receipt, history::now_secs()) {
            emit_tx_status_changed(&rec.hash);
            if status == "confirmed" {
                emit_balances_updated(&rec.from);
            }
        }
        Ok(json!({ "ok": true, "hash": rec.hash, "chainId": rec.chain_id, "status": status,
                   "route": verified::weakest_route(&[route.as_deref()]) }))
    }

    /// The mined-at time. `eth_getBlockByNumber` has no typed helper on `eth_rpc`, so it goes
    /// through `raw_rpc`; `false` asks for the header rather than every transaction in it.
    fn block_header(
        &self,
        chain_id: u64,
        number: u64,
        b: &Budget,
    ) -> Result<(Value, Option<String>), String> {
        let t = b.take(RPC_BUDGET).ok_or("no time left to read the block")?;
        let params = format!("[\"0x{number:x}\", false]");
        let raw = modules()
            .eth_rpc_module
            .raw_rpc_with_timeout(chain_id as i64, "eth_getBlockByNumber", &params, t)
            .map_err(|e| format!("{e:?}"))?;
        let Answer { value, route } = unwrap_answer(&raw)?;
        let ts = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_u64_any)
            .ok_or("the node returned no timestamp for this block")?;
        Ok((json!({ "number": number, "timestamp": ts }), route))
    }

    /// `gas` (the LIMIT the transaction carried), `maxPriorityFeePerGas` and `input`, none of
    /// which a receipt reports. An absent field stays absent: a legacy transaction has no
    /// priority fee, and a zero here would be a figure the chain never carried.
    fn tx_fields(
        &self,
        chain_id: u64,
        hash: &str,
        b: &Budget,
    ) -> Result<(Value, Option<String>), String> {
        let t = b.take(RPC_BUDGET).ok_or("no time left to read the transaction")?;
        let raw = modules()
            .eth_rpc_module
            .get_transaction_by_hash_with_timeout(chain_id as i64, hash, t)
            .map_err(|e| format!("{e:?}"))?;
        let Answer { value, route } = unwrap_answer(&raw)?;
        if value.is_null() {
            return Err("the node does not have this transaction".to_string());
        }
        let mut out = json!({});
        if let Some(g) = value.get("gas").and_then(Value::as_str).and_then(parse_u64_any) {
            out["gasLimit"] = json!(g);
        }
        if let Some(p) =
            value.get("maxPriorityFeePerGas").and_then(Value::as_str).and_then(parse_u256_any)
        {
            let tip = p.to_string();
            out["maxPriorityFeePerGas"] = json!(tip);
            units::decorate(&mut out, "maxPriorityFeePerGas", &tip, Some(GAS_PRICE_DECIMALS));
        }
        if let Some(d) = value.get("input").and_then(Value::as_str) {
            out["input"] = json!(d);
        }
        Ok((out, route))
    }

    /// Fetch what a receipt never carried, for ONE row, on its own chain. Bounded AS A WHOLE,
    /// gate included, for the reason `refresh_one` gives: the budget taken after the gate
    /// bounded the two legs and not the call a user actually waits on.
    ///
    /// The two legs are independent, so one failing is reported BESIDE the fields it could not
    /// fill rather than failing the other — a timed-out block read must not withhold a priority
    /// fee that landed.
    fn tx_details(&self, address: &str, hash_hex: &str) -> Result<Value, String> {
        let st = self.state()?;
        let rec = st
            .history
            .find(address, hash_hex)
            .ok_or_else(|| format!("no recorded transaction with hash {hash_hex}"))?;
        let b = Budget::new(DETAILS_BUDGET);
        // The hash goes onto the refusal too: the view renders this beside one transaction's
        // own rows, so every reply has to say which transaction it is about.
        if let Err(v) = self.verified_gate_within(rec.chain_id, &b) {
            let mut r = blocked(&v);
            r["hash"] = json!(rec.hash);
            r["chainId"] = json!(rec.chain_id);
            return Ok(r);
        }
        let Some(number) = rec.block_number else {
            return Err("this transaction has no block yet, so there is nothing to read".into());
        };

        // The second leg is SKIPPED, not failed, when the row already stores every answer it
        // would bring: that is what makes this one call for a send recorded by this build.
        let needed = details::transaction_leg_needed(&rec);
        // The block first: it is the gap a user comparing with an explorer notices, so it gets
        // the allowance ahead of a leg that may not even be made.
        let block = self.block_header(rec.chain_id, number, &b);
        let tx = needed.then(|| self.tx_fields(rec.chain_id, &rec.hash, &b));
        Ok(details::details_reply(&rec.hash, rec.chain_id, history::now_secs(), block, tx))
    }

    /// Price a send: fee from `fee_module`, nonce from the chain, affordability from the
    /// balance. Pure of side effects — reserves nothing and requests no approval.
    fn quote(&self, req: &SendRequest, chain_id: u64, b: &Budget) -> Result<Quote, String> {
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
        let native = token.as_ref().map(|t| t.native).unwrap_or(true);
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

        // The transaction the fee estimate must price is the real one, so an ERC-20 send is
        // estimated against its calldata rather than a bare transfer's 21 000.
        let tx_shape = match &token {
            Some(t) if !t.native => {
                let addr = t.address.as_deref().unwrap_or_default()
                    .parse::<alloy::primitives::Address>()
                    .map_err(|e| format!("token has an unparseable address: {e}"))?;
                json!({ "from": from.to_string(), "to": addr.to_string(),
                        "data": format!("0x{}", hex::encode(txbuild::erc20_transfer_calldata(to, amount))) })
            }
            _ => json!({ "from": from.to_string(), "to": to.to_string(),
                         "value": format!("0x{amount:x}") }),
        };

        let mut fee_req = json!({ "tx": tx_shape });
        if let Some(t) = &req.tier { fee_req["tier"] = json!(t); }
        if let Some(v) = &req.max_fee_per_gas { fee_req["maxFeePerGas"] = json!(v); }
        if let Some(v) = &req.max_priority_fee_per_gas { fee_req["maxPriorityFeePerGas"] = json!(v); }
        if let Some(v) = &req.gas_limit { fee_req["gasLimit"] = json!(v); }

        let t = b.take(RPC_BUDGET).ok_or("no time left to price the fee")?;
        let raw = modules()
            .fee_module
            .estimate_with_timeout(chain_id as i64, &fee_req.to_string(), t)
            .map_err(|e| format!("{e:?}"))?;
        let fee: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        if fee.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(fee.get("error").and_then(Value::as_str)
                .unwrap_or("fee estimation failed").to_string());
        }
        // fee_module emits amounts as decimal strings but `gasLimit` as a JSON number, so
        // every numeric field is read through both forms rather than assuming one.
        let pick = |k: &str| -> Result<U256, String> {
            fee.get(k)
                .and_then(|v| match v {
                    Value::String(t) => parse_u256_any(t),
                    Value::Number(n) => n.as_u64().map(U256::from),
                    _ => None,
                })
                .ok_or_else(|| format!("fee_module returned no usable `{k}`"))
        };
        let max_fee = pick("maxFeePerGas")?;
        let max_priority = pick("maxPriorityFeePerGas")?;
        if max_priority > max_fee {
            return Err("maxPriorityFeePerGas cannot exceed maxFeePerGas".into());
        }
        let gas_limit = u64::try_from(pick("gasLimit").map_err(|_| {
            "fee_module returned no usable `gasLimit` — refusing rather than guessing one"
        })?)
        .map_err(|_| "fee_module returned an implausible `gasLimit`")?;
        let fee_source =
            fee.get("source").and_then(Value::as_str).unwrap_or("unknown").to_string();

        let (balance, balance_route) = self.native_balance(chain_id, &from.to_string(), b)?;
        send::affordable(balance, amount, gas_limit, max_fee, native, &native_symbol)?;

        // `affordable` only ever charges the fee against ether, so an ERC-20 send is checked
        // against the token itself here. Unreachable while the Send screen was hardcoded to
        // native; the moment a token can be chosen, its absence means an over-large transfer
        // is approved, broadcast, and reverts on chain having burned the gas.
        let mut token_route = None;
        if let Some(t) = token.as_ref().filter(|t| !t.native) {
            let addr = t.address.as_deref().unwrap_or_default()
                .parse::<alloy::primitives::Address>()
                .map_err(|e| format!("token has an unparseable address: {e}"))?;
            let (held, r) = self.token_balance(chain_id, addr, from, b)?;
            token_route = r;
            send::token_affordable(held, amount, &t.symbol, t.decimals)?;
        }

        // A caller-supplied nonce came from nothing we can vouch for, so it unlabels the quote.
        let (nonce, nonce_route) = match req.nonce {
            Some(n) => (n, None),
            None => self.chain_nonce(chain_id, &from.to_string(), b)?,
        };
        let route = verified::weakest_route(&[
            balance_route.as_deref(),
            token_route.as_deref(),
            nonce_route.as_deref(),
        ]);

        Ok(Quote { chain_id, from, to, amount, token, decimals, symbol, native_symbol, nonce,
                   gas_limit, max_fee, max_priority, fee_source, route })
    }

    fn native_balance(
        &self,
        chain_id: u64,
        address: &str,
        b: &Budget,
    ) -> Result<(U256, Option<String>), String> {
        let t = b.take(RPC_BUDGET).ok_or("no time left to read the balance")?;
        let raw = modules()
            .eth_rpc_module
            .get_balance_with_timeout(chain_id as i64, address, t)
            .map_err(|e| format!("{e:?}"))?;
        let a = unwrap_answer(&raw)?;
        let v = a.value.as_str().and_then(parse_u256_any)
            .ok_or_else(|| "could not read the native balance".to_string())?;
        Ok((v, a.route))
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
            .call_with_timeout(chain_id as i64, &call.to_string(), t)
            .map_err(|e| format!("{e:?}"))?;
        let a = unwrap_answer(&raw)?;
        let v = a.value.as_str()
            .and_then(|s| hex::decode(s.trim_start_matches("0x")).ok())
            .as_deref()
            .and_then(txbuild::decode_uint256)
            .ok_or_else(|| "could not read the token balance".to_string())?;
        Ok((v, a.route))
    }

    fn chain_nonce(
        &self,
        chain_id: u64,
        address: &str,
        b: &Budget,
    ) -> Result<(u64, Option<String>), String> {
        let t = b.take(RPC_BUDGET).ok_or("no time left to read the nonce")?;
        let raw = modules()
            .eth_rpc_module
            .get_transaction_count_with_timeout(chain_id as i64, address, t)
            .map_err(|e| format!("{e:?}"))?;
        let a = unwrap_answer(&raw)?;
        let v = a.value.as_str().and_then(parse_u64_any)
            .ok_or_else(|| "could not read the account nonce".to_string())?;
        Ok((v, a.route))
    }

    /// The unsigned transaction for a quote, in the shape `keystore_module` deserializes.
    fn unsigned_tx(&self, q: &Quote) -> Result<Value, String> {
        let fee = txbuild::Fee::Eip1559 {
            max_fee_per_gas: q.max_fee,
            max_priority_fee_per_gas: q.max_priority,
        };
        Ok(match &q.token {
            Some(t) if !t.native => {
                let addr = t.address.as_deref().unwrap_or_default()
                    .parse::<alloy::primitives::Address>()
                    .map_err(|e| format!("token has an unparseable address: {e}"))?;
                txbuild::unsigned_erc20_tx(addr, q.to, q.amount, q.nonce, q.gas_limit, &fee)
            }
            _ => txbuild::unsigned_native_tx(q.to, q.amount, q.nonce, q.gas_limit, &fee),
        })
    }

    /// `status` is what the send is DOING, not only what it has settled into: a claimed
    /// broadcast reads `broadcasting`, and one that has not answered reads `stuck`.
    fn job_reply(j: &SendJob, now: u64) -> Value {
        let status = j.reported_status(now);
        let mut v = json!({ "ok": true, "requestId": j.request_id, "status": status });
        match &j.status {
            SendStatus::Broadcast { hash, route } => {
                v["hash"] = json!(hash);
                v["route"] = json!(route);
            }
            SendStatus::Failed { reason } => v["reason"] = json!(reason),
            _ => {}
        }
        if status == "stuck" {
            v["reason"] = json!(
                "the broadcast has not answered; this send may already be on chain and must \
                 not be sent again"
            );
        }
        v
    }

    /// Settle a job and announce it. The ledger applies the status to the LIVE job and gives
    /// back whatever it now holds, so a status another dispatch settled first is reported
    /// rather than overwritten from this caller's stale copy.
    fn settle(&self, st: &State, request_id: &str, status: SendStatus) -> Result<Value, String> {
        let job = st
            .sends
            .settle(request_id, status)
            .ok_or_else(|| format!("no send with id '{request_id}'"))?;
        emit_send_status_changed(&job.request_id);
        Ok(Self::job_reply(&job, history::now_secs()))
    }

    /// The broadcast owner's door — the only settle that lands once a broadcast is claimed.
    fn settle_owned(
        &self,
        st: &State,
        t: &send::BroadcastTicket,
        status: SendStatus,
    ) -> Result<Value, String> {
        let job = st
            .sends
            .settle_owned(t, status)
            .ok_or_else(|| "the send vanished while it was being broadcast".to_string())?;
        emit_send_status_changed(&job.request_id);
        Ok(Self::job_reply(&job, history::now_secs()))
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
        let history = History::new(dir);

        // R-1. The ledger is in-memory and `latest` does not count a broadcast that has not
        // mined, so a restart would hand the next send a number an unsettled transaction is
        // already using. Burn them before any send can reach `state()`.
        let seeded = self.sends.seed_spent(history.unsettled_nonces());
        if seeded > 0 {
            eprintln!("eth_wallet_backend: {seeded} unsettled nonces carried over from disk");
        }

        if let Ok(mut g) = self.state.write() {
            *g = Some(Arc::new(State { settings, history, sends: self.sends.clone() }));
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
        // The refusal and the write are one critical section inside the ledger. A send
        // opening its claim between the two would straddle the switch: approved for a
        // network the wallet had already left, and invisible to the check that forbids it.
        match self.state().and_then(|st| {
            st.sends.switch(history::now_secs(), || {
                st.settings.set_active_chain(chain_id as u64).map_err(|e| e.to_string())
            })
        }) {
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
        match self.active_chain() {
            Ok(id) => self.verified_verdict(id).to_string(),
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

    fn list_available_tokens(&self, chain_id: i64, query: String, limit: i64) -> String {
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
        let (total, rows) =
            tokens::available(chain_id, &listed, s.enabled_tokens(chain_id), &query, cut);
        let mut v = json!({ "ok": true, "chainId": chain_id, "tokenSort": s.token_sort.as_str(),
                            "total": total, "shown": rows.len(), "listed": listed.len(),
                            "tokens": rows });
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

    fn get_balances(&self, address: String) -> String {
        self.ensure_eth_rpc(&Budget::new(READ_BUDGET));
        let settings = match self.settings() {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        let chain_id = settings.active_chain_id;
        if let Err(v) = self.verified_gate(chain_id) {
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
        let raw = match modules().eth_rpc_module.call(chain_id as i64, &call.to_string()) {
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
        let st = match self.state() {
            Ok(st) => st,
            Err(e) => return err(e),
        };
        // Rows are derived on read, so one that confirmed while this view was closed reads
        // `confirmed` the moment anything asks. The sweep announces that itself, per row: a
        // read must not, or the view's subscription drives it round again.
        let settings = match self.settings() {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        let chain_id = settings.active_chain_id;
        let swept = self.sweep(&st, &address, &Budget::new(SWEEP_BUDGET));
        let rows = st.history.list(&address);
        // The same offered set the balance list and the send path read, so a transfer in an
        // enabled token is decoded rather than shown as an unknown contract.
        let mut v = history_reply(
            &address,
            chain_id,
            &rows,
            history::now_secs(),
            &swept,
            settings.enabled_tokens(chain_id),
        );
        // F-5. Numbers a duplicate request id stranded: nothing holds them, nothing will hand
        // them back, and every later send queues behind them. Disclosed rather than released,
        // because "nobody holds it" is not evidence a transaction did not leave.
        let stranded: Vec<Value> = st
            .sends
            .stranded()
            .iter()
            .filter(|(c, a, _)| *c == chain_id && send::same_account(a, &address))
            .map(|(_, _, n)| json!(n))
            .collect();
        v["strandedNonces"] = json!(stranded);
        v.to_string()
    }

    fn refresh_pending(&self, address: String) -> String {
        // Gated per record inside the sweep, like `get_history` and `refresh_tx_status`:
        // a row carries its own chain, so a blocking proxy on the ACTIVE one must not
        // suppress a due row elsewhere. `blockedChains` says which rows that cost, and why.
        let st = match self.state() {
            Ok(st) => st,
            Err(e) => return err(e),
        };
        let s = self.sweep(&st, &address, &Budget::new(SWEEP_BUDGET));
        json!({ "ok": true, "address": address, "polled": s.polled,
                "changed": s.changed, "blocked": s.blocked_count(),
                "blockedChains": s.blocked_json(), "stillDue": s.still_due })
        .to_string()
    }


    fn prepare_send(&self, request_json: String) -> String {
        let req: SendRequest = match serde_json::from_str(&request_json) {
            Ok(r) => r,
            Err(e) => return err(format!("invalid send request: {e}")),
        };
        let chain_id = match self.active_chain() {
            Ok(id) => id,
            Err(e) => return err(e),
        };
        if let Err(v) = self.verified_gate(chain_id) {
            return blocked(&v).to_string();
        }
        match self.quote(&req, chain_id, &Budget::new(SEND_BUDGET)) {
            Ok(q) => {
                let native = q.token.as_ref().map(|t| t.native).unwrap_or(true);
                let charged = if native { q.amount } else { U256::ZERO };
                let max_cost =
                    send::max_cost_wei(charged, q.gas_limit, q.max_fee).map(|v| v.to_string());
                let ceiling = history::fee_ceiling_wei(&q.max_fee.to_string(), q.gas_limit);
                let mut v = json!({
                    "ok": true, "chainId": q.chain_id,
                    "from": q.from.to_string(), "to": q.to.to_string(),
                    "amount": q.amount.to_string(),
                    "amountSymbol": q.symbol,
                    "amountDecimals": q.decimals,
                    "nativeSymbol": q.native_symbol,
                    "token": q.token.as_ref().map(|t| t.symbol.clone()),
                    // WHICH contract the send will call, resolved. A symbol cannot say it:
                    // two tokens can share one, so a confirmation step showing only the
                    // symbol cannot reveal that the wrong asset is about to move.
                    // Null for a native send, which calls no contract.
                    "tokenAddress": q.token.as_ref().and_then(|t| t.address.clone()),
                    "nonce": q.nonce, "gasLimit": q.gas_limit,
                    "maxFeePerGas": q.max_fee.to_string(),
                    "maxPriorityFeePerGas": q.max_priority.to_string(),
                    "maxCostWei": max_cost,
                    // `maxFeePerGas × gasLimit`: a ceiling, never a price. A view that
                    // presents it as "the fee" is how an overpayment goes unnoticed.
                    "feeCeilingWei": ceiling,
                    "feeSource": q.fee_source,
                    // `route` covers the balance and nonce reads only. The fee is fee_module's,
                    // which emits no label, so it is never proof-backed whatever `route` says.
                    "route": q.route,
                    "feeRoute": verified::UNKNOWN_ROUTE,
                });
                units::decorate(&mut v, "amount", &q.amount.to_string(), Some(q.decimals));
                // Fees are ether whatever is being sent, so they render at 18 places.
                if let Some(c) = &ceiling {
                    units::decorate(&mut v, "feeCeilingWei", c, Some(18));
                }
                if let Some(m) = &max_cost {
                    units::decorate(&mut v, "maxCostWei", m, Some(18));
                }
                v.to_string()
            }
            Err(e) => err(e),
        }
    }

    fn send(&self, request_json: String) -> String {
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
        if let Err(v) = self.verified_gate(chain_id) {
            return blocked(&v).to_string();
        }
        match self.request_send(&st, &req, chain_id) {
            // Deliberately no hash: nothing is signed or broadcast until a human approves.
            Ok(id) => json!({ "ok": true, "pending": true, "requestId": id }).to_string(),
            Err(e) => err(e),
        }
    }

    fn send_status(&self, request_id: String) -> String {
        match self.advance_send(&request_id) {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn cancel_send(&self, request_id: String) -> String {
        // Cancel locally FIRST, under the ledger's lock; telling the keystore is a courtesy
        // whose reply was already discarded. The other order let two cancels release the same
        // nonce twice, or one cancel a send another poll had begun broadcasting.
        match self.state().and_then(|st| {
            let job = st.sends.claim_cancel(&request_id)?;
            let _ = modules().keystore_module.cancel_approval_with_timeout(
                &job.handle,
                &job.receipt,
                RPC_BUDGET,
            );
            emit_send_status_changed(&job.request_id);
            Ok(Self::job_reply(&job, history::now_secs()))
        }) {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn refresh_tx_status(&self, address: String, hash_hex: String) -> String {
        // Advisory: `hash`, `chain_id` and `from` are a row's identity and never change, so
        // nothing needs re-checking after the call — and `apply_receipt` settles the row as
        // it stands now, not as this copy remembers it.
        match self.refresh_one(&address, &hash_hex) {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn get_tx_details(&self, address: String, hash_hex: String) -> String {
        match self.tx_details(&address, &hash_hex) {
            Ok(v) => v.to_string(),
            // Not `err()`: this reply is rendered beside ONE transaction's rows, so even a
            // refusal has to name the hash it is about or it could land under another.
            Err(e) => details::details_refusal(&hash_hex, &e).to_string(),
        }
    }

    fn suggest_fees(&self) -> String {
        let chain_id = match self.active_chain() {
            Ok(id) => id,
            Err(e) => return err(e),
        };
        if let Err(v) = self.verified_gate(chain_id) {
            return blocked(&v).to_string();
        }
        match modules().fee_module.suggest_fees(chain_id as i64) {
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
