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
use std::sync::RwLock;

use alloy::primitives::U256;
use serde_json::{json, Value};

use crate::history::{History, TxRecord};
use crate::send::{self, NonceReserver, SendJob, SendStatus};
use crate::settings::{SettingsStore, VerifiedProxyMode};
use crate::{networks, tokens, txbuild};

pub trait EthWalletBackendModule: Send + Sync + 'static {
    /// The three selectable networks. `{ ok, activeChainId, networks: [{ chainId, key,
    /// name, nativeSymbol, testnet, explorer, rpcUrl, verifiedProxyMode }] }`.
    fn list_networks(&self) -> String;

    /// The active network alone, in the same shape as one `list_networks` entry.
    fn get_active_network(&self) -> String;

    /// Switch the active network. Refuses any chain outside {1, 11155111, 560048} —
    /// this wallet is Ethereum only. `{ ok, activeChainId }`.
    fn set_active_chain(&self, chain_id: i64) -> String;

    /// Set the JSON-RPC endpoint for a network and push it down to `eth_rpc_module`.
    fn set_rpc_url(&self, chain_id: i64, url: String) -> String;

    /// `"off"` talks to the endpoint directly; `"required"` routes through the
    /// light-client proxy and refuses rather than falling back to clear-net.
    fn set_verified_proxy_mode(&self, chain_id: i64, mode: String) -> String;

    /// Tokens offered on the active network, native first. Fixed table, no custom tokens.
    /// `{ ok, chainId, tokens: [{ symbol, name, decimals, address?, native }] }`.
    fn list_tokens(&self) -> String;

    /// Accounts the keystore holds. Read-only: this module can never create, import or
    /// export one — those are the custodian's, and reach the keystore only via `keystore_ui`.
    fn list_accounts(&self) -> String;

    /// Native and token balances for `address` on the active network, in one Multicall3
    /// round-trip. `{ ok, chainId, address, balances: [{ symbol, address?, raw, decimals }] }`.
    fn get_balances(&self, address: String) -> String;

    /// Locally recorded transactions for `address`, newest first, scoped to the active
    /// network. Only transactions this wallet broadcast — there is no indexer.
    fn get_history(&self, address: String) -> String;

    /// Fee tiers for the active network, from `fee_module`. `{ ok, chainId, baseFeePerGas,
    /// source, tiers: { slow, normal, fast } }`; `source` distinguishes a real EIP-1559
    /// suggestion from the legacy `gasPrice` fallback.
    fn suggest_fees(&self) -> String;

    /// Quote a send without doing anything: resolves the fee through `fee_module`, reads the
    /// nonce, and refuses up front if the balance cannot cover value plus the fee ceiling.
    ///
    /// `request_json`: `{ from, to, amount, token?, tier?, maxFeePerGas?,
    /// maxPriorityFeePerGas?, gasLimit?, nonce? }`. `amount` is in the token's base units;
    /// `token` is a symbol or contract address, absent for a native send. Any explicit fee
    /// field is used verbatim — the user overrules the suggestion, never the other way round.
    ///
    /// Returns `{ ok, chainId, from, to, amount, token?, nonce, gasLimit, maxFeePerGas,
    /// maxPriorityFeePerGas, maxCostWei, feeSource }`. No approval is requested and no nonce
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
    /// and records it in history. `{ ok, requestId, status, hash?, reason? }` where `status`
    /// is `awaitingApproval` | `broadcast` | `rejected` | `cancelled` | `failed`.
    fn send_status(&self, request_id: String) -> String;

    /// Withdraw a send that has not been approved yet, releasing its reserved nonce.
    fn cancel_send(&self, request_id: String) -> String;

    /// Re-read the receipt for a recorded transaction and update its stored status.
    /// `{ ok, hash, status }` where `status` is `pending` | `confirmed` | `failed`.
    fn refresh_tx_status(&self, hash_hex: String) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

pub trait EthWalletBackendModuleEvents {
    fn balances_updated(&self, address: String);
    fn active_chain_changed(&self, chain_id: i64);
    fn tx_status_changed(&self, hash_hex: String);
    /// A pending send changed state — approved, rejected, broadcast or failed.
    fn send_status_changed(&self, request_id: String);
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct EthWalletBackendImpl {
    state: RwLock<Option<State>>,
}

struct State {
    settings: SettingsStore,
    history: History,
    jobs: RwLock<HashMap<String, SendJob>>,
    nonces: RwLock<NonceReserver>,
}

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

const NO_CONTEXT: &str = "module context not ready";

/// A send as the caller asked for it, before any chain lookup.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendRequest {
    from: String,
    to: String,
    /// Base units of the token being moved (wei for a native send).
    amount: String,
    #[serde(default)]
    token: Option<String>,
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
    nonce: u64,
    gas_limit: u64,
    max_fee: U256,
    max_priority: U256,
    fee_source: String,
}

fn parse_u256_any(s: &str) -> Option<U256> {
    let t = s.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => U256::from_str_radix(h, 16).ok(),
        None => U256::from_str_radix(t, 10).ok(),
    }
}

fn parse_u64_any(s: &str) -> Option<u64> {
    parse_u256_any(s).and_then(|v| u64::try_from(v).ok())
}



/// `eth_rpc_module` wraps most replies as `{ ok, result }` — but not all: broadcasting
/// answers `{ ok, hash }`. Reading only `result` there silently yields a null hash, a
/// history row that cannot be followed up, and a send that looks like it went nowhere while
/// the money has actually moved. Accept both keys.
fn unwrap_rpc(reply: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(reply).map_err(|e| e.to_string())?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("eth_rpc call failed")
            .to_string());
    }
    Ok(v.get("result").or_else(|| v.get("hash")).cloned().unwrap_or(Value::Null))
}

impl EthWalletBackendImpl {
    fn with_state<T>(&self, f: impl FnOnce(&State) -> Result<T, String>) -> Result<T, String> {
        let guard = self.state.read().map_err(|_| "settings lock poisoned".to_string())?;
        let st = guard.as_ref().ok_or_else(|| NO_CONTEXT.to_string())?;
        f(st)
    }

    /// Mirror a network's transport settings into `eth_rpc_module`, which is where the
    /// endpoint actually lives. Only fields this wallet owns are written.
    fn push_chain_config(&self, chain_id: u64, rpc_url: &str) -> Result<(), String> {
        if rpc_url.is_empty() {
            return Ok(());
        }
        let cfg = json!({ "endpoint": rpc_url, "timeoutSecs": 8 });
        modules()
            .eth_rpc_module
            .set_chain_config(chain_id as i64, &cfg.to_string())
            .map_err(|e| format!("{e:?}"))?;
        Ok(())
    }


    /// Price a send: fee from `fee_module`, nonce from the chain, affordability from the
    /// balance. Pure of side effects — reserves nothing and requests no approval.
    fn quote(&self, req: &SendRequest, chain_id: u64) -> Result<Quote, String> {
        let from = req.from.trim().parse::<alloy::primitives::Address>()
            .map_err(|e| format!("invalid `from` address: {e}"))?;
        let to = req.to.trim().parse::<alloy::primitives::Address>()
            .map_err(|e| format!("invalid `to` address: {e}"))?;
        let amount = parse_u256_any(&req.amount)
            .ok_or_else(|| format!("amount '{}' is not a number", req.amount))?;

        let token = match &req.token {
            None => None,
            Some(k) => Some(
                tokens::find(chain_id, k)
                    .ok_or_else(|| format!("token '{k}' is not offered on chain {chain_id}"))?,
            ),
        };
        let native = token.as_ref().map(|t| t.native).unwrap_or(true);

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

        let raw = modules().fee_module.estimate(chain_id as i64, &fee_req.to_string())
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

        let balance = self.native_balance(chain_id, &from.to_string())?;
        send::affordable(balance, amount, gas_limit, max_fee, native)?;

        let nonce = match req.nonce {
            Some(n) => n,
            None => self.chain_nonce(chain_id, &from.to_string())?,
        };

        Ok(Quote { chain_id, from, to, amount, token, nonce, gas_limit, max_fee, max_priority, fee_source })
    }

    fn native_balance(&self, chain_id: u64, address: &str) -> Result<U256, String> {
        let raw = modules().eth_rpc_module.get_balance(chain_id as i64, address)
            .map_err(|e| format!("{e:?}"))?;
        let v = unwrap_rpc(&raw)?;
        v.as_str().and_then(parse_u256_any).ok_or_else(|| "could not read the native balance".into())
    }

    fn chain_nonce(&self, chain_id: u64, address: &str) -> Result<u64, String> {
        let raw = modules().eth_rpc_module.get_transaction_count(chain_id as i64, address)
            .map_err(|e| format!("{e:?}"))?;
        let v = unwrap_rpc(&raw)?;
        v.as_str().and_then(parse_u64_any).ok_or_else(|| "could not read the account nonce".into())
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

    fn job_reply(j: &SendJob) -> Value {
        let mut v = json!({ "ok": true, "requestId": j.request_id });
        match &j.status {
            SendStatus::AwaitingApproval => { v["status"] = json!("awaitingApproval"); }
            SendStatus::Broadcast { hash } => {
                v["status"] = json!("broadcast");
                v["hash"] = json!(hash);
            }
            SendStatus::Rejected => { v["status"] = json!("rejected"); }
            SendStatus::Cancelled => { v["status"] = json!("cancelled"); }
            SendStatus::Failed { reason } => {
                v["status"] = json!("failed");
                v["reason"] = json!(reason);
            }
        }
        v
    }

    /// Settle a job that is no longer awaiting approval: release its nonce and store it.
    fn settle(&self, st: &State, mut job: SendJob, status: SendStatus) -> SendJob {
        if !matches!(status, SendStatus::Broadcast { .. }) {
            if let Ok(mut n) = st.nonces.write() {
                n.release(job.chain_id, &job.from, job.nonce);
            }
        }
        job.status = status;
        if let Ok(mut jobs) = st.jobs.write() {
            jobs.insert(job.request_id.clone(), job.clone());
        }
        emit_send_status_changed(&job.request_id);
        job
    }

    fn network_json(&self, chain_id: u64, st: &State) -> Value {
        let net = networks::by_chain_id(chain_id);
        let s = st.settings.load();
        let ns = s.network(chain_id);
        json!({
            "chainId": chain_id,
            "key": net.map(|n| n.key).unwrap_or_default(),
            "name": net.map(|n| n.name).unwrap_or_default(),
            "nativeSymbol": net.map(|n| n.native_symbol).unwrap_or_default(),
            "testnet": net.map(|n| n.testnet).unwrap_or(false),
            "explorer": net.map(|n| n.explorer).unwrap_or_default(),
            "rpcUrl": ns.map(|n| n.rpc_url.clone()).unwrap_or_default(),
            "verifiedProxyMode": ns.map(|n| n.verified_proxy_mode).unwrap_or_default(),
        })
    }
}

impl EthWalletBackendModule for EthWalletBackendImpl {
    fn on_context_ready(&self, ctx: &RustModuleContext) {
        let dir = PathBuf::from(&ctx.instance_persistence_path);
        let settings = SettingsStore::with_path(dir.join("settings.json"));
        let loaded = settings.load();
        let history = History::new(dir);

        if let Ok(mut g) = self.state.write() {
            *g = Some(State {
                settings,
                history,
                jobs: RwLock::new(HashMap::new()),
                nonces: RwLock::new(NonceReserver::default()),
            });
        }
        // Re-assert every configured endpoint: eth_rpc's config store is shared with other
        // wallets on this device and may not carry ours.
        for n in &loaded.networks {
            let _ = self.push_chain_config(n.chain_id, &n.rpc_url);
        }
    }

    fn list_networks(&self) -> String {
        match self.with_state(|st| {
            let s = st.settings.load();
            let list: Vec<Value> =
                networks::ALL.iter().map(|n| self.network_json(n.chain_id, st)).collect();
            Ok(json!({ "ok": true, "activeChainId": s.active_chain_id, "networks": list }))
        }) {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn get_active_network(&self) -> String {
        match self.with_state(|st| {
            let id = st.settings.load().active_chain_id;
            Ok(json!({ "ok": true, "network": self.network_json(id, st) }))
        }) {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn set_active_chain(&self, chain_id: i64) -> String {
        if chain_id < 0 {
            return err(format!("chain {chain_id} is not a valid chain id"));
        }
        match self.with_state(|st| st.settings.set_active_chain(chain_id as u64).map_err(|e| e.to_string()))
        {
            Ok(s) => {
                emit_active_chain_changed(chain_id);
                json!({ "ok": true, "activeChainId": s.active_chain_id }).to_string()
            }
            Err(e) => err(e),
        }
    }

    fn set_rpc_url(&self, chain_id: i64, url: String) -> String {
        if chain_id < 0 {
            return err(format!("chain {chain_id} is not a valid chain id"));
        }
        let id = chain_id as u64;
        match self
            .with_state(|st| st.settings.set_rpc_url(id, &url).map_err(|e| e.to_string()))
            .and_then(|s| {
                let stored = s.network(id).map(|n| n.rpc_url.clone()).unwrap_or_default();
                self.push_chain_config(id, &stored).map(|_| stored)
            }) {
            Ok(stored) => json!({ "ok": true, "chainId": id, "rpcUrl": stored }).to_string(),
            Err(e) => err(e),
        }
    }

    fn set_verified_proxy_mode(&self, chain_id: i64, mode: String) -> String {
        if chain_id < 0 {
            return err(format!("chain {chain_id} is not a valid chain id"));
        }
        let parsed = match mode.trim().to_ascii_lowercase().as_str() {
            "off" => VerifiedProxyMode::Off,
            "required" => VerifiedProxyMode::Required,
            other => return err(format!("unknown verified proxy mode '{other}' (expected off or required)")),
        };
        match self.with_state(|st| {
            st.settings.set_verified_proxy_mode(chain_id as u64, parsed).map_err(|e| e.to_string())
        }) {
            Ok(_) => json!({ "ok": true, "chainId": chain_id, "verifiedProxyMode": parsed }).to_string(),
            Err(e) => err(e),
        }
    }

    fn list_tokens(&self) -> String {
        match self.with_state(|st| Ok(st.settings.load().active_chain_id)) {
            Ok(id) => json!({ "ok": true, "chainId": id, "tokens": tokens::for_chain(id) }).to_string(),
            Err(e) => err(e),
        }
    }

    fn list_accounts(&self) -> String {
        match modules().keystore_module.list_accounts() {
            Ok(reply) => reply,
            Err(e) => err(format!("{e:?}")),
        }
    }

    fn get_balances(&self, address: String) -> String {
        let chain_id = match self.with_state(|st| Ok(st.settings.load().active_chain_id)) {
            Ok(id) => id,
            Err(e) => return err(e),
        };
        let owner = match address.trim().parse::<alloy::primitives::Address>() {
            Ok(a) => a,
            Err(e) => return err(format!("invalid address: {e}")),
        };

        let list = tokens::for_chain(chain_id);
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
        let result = match unwrap_rpc(&raw) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        let bytes = match result.as_str().and_then(|s| hex::decode(s.trim_start_matches("0x")).ok()) {
            Some(b) => b,
            None => return err("eth_call returned no decodable data"),
        };
        let Some(returns) = txbuild::decode_aggregate3_returns(&bytes) else {
            return err("could not decode the Multicall3 response");
        };

        let balances: Vec<Value> = list
            .iter()
            .zip(returns.into_iter())
            .map(|(t, ret)| {
                let raw = ret
                    .as_deref()
                    .and_then(txbuild::decode_uint256)
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                json!({
                    "symbol": t.symbol,
                    "address": t.address,
                    "decimals": t.decimals,
                    "native": t.native,
                    // Empty means the sub-call failed; the UI shows an em-dash, never a zero,
                    // because "you have none" and "we could not read it" are different.
                    "raw": raw,
                })
            })
            .collect();

        emit_balances_updated(&address);
        json!({ "ok": true, "chainId": chain_id, "address": address, "balances": balances }).to_string()
    }

    fn get_history(&self, address: String) -> String {
        match self.with_state(|st| {
            let chain_id = st.settings.load().active_chain_id;
            let records: Vec<_> =
                st.history.list(&address).into_iter().filter(|r| r.chain_id == chain_id).collect();
            Ok(json!({ "ok": true, "chainId": chain_id, "address": address, "transactions": records }))
        }) {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }


    fn prepare_send(&self, request_json: String) -> String {
        let req: SendRequest = match serde_json::from_str(&request_json) {
            Ok(r) => r,
            Err(e) => return err(format!("invalid send request: {e}")),
        };
        let chain_id = match self.with_state(|st| Ok(st.settings.load().active_chain_id)) {
            Ok(id) => id,
            Err(e) => return err(e),
        };
        match self.quote(&req, chain_id) {
            Ok(q) => json!({
                "ok": true, "chainId": q.chain_id,
                "from": q.from.to_string(), "to": q.to.to_string(),
                "amount": q.amount.to_string(),
                "token": q.token.as_ref().map(|t| t.symbol.clone()),
                "nonce": q.nonce, "gasLimit": q.gas_limit,
                "maxFeePerGas": q.max_fee.to_string(),
                "maxPriorityFeePerGas": q.max_priority.to_string(),
                "maxCostWei": send::max_cost_wei(
                    if q.token.as_ref().map(|t| t.native).unwrap_or(true) { q.amount } else { U256::ZERO },
                    q.gas_limit, q.max_fee).map(|v| v.to_string()),
                "feeSource": q.fee_source,
            })
            .to_string(),
            Err(e) => err(e),
        }
    }

    fn send(&self, request_json: String) -> String {
        let req: SendRequest = match serde_json::from_str(&request_json) {
            Ok(r) => r,
            Err(e) => return err(format!("invalid send request: {e}")),
        };
        let outcome = self.with_state(|st| {
            let chain_id = st.settings.load().active_chain_id;
            let mut q = self.quote(&req, chain_id)?;

            // Reserve against the chain's own view so a second send cannot reuse the nonce.
            // `latest` does not count a broadcast-but-unmined transaction, and the verified
            // path refuses `pending`, so the reservation is the only thing preventing a clash.
            if req.nonce.is_none() {
                let chain_nonce = q.nonce;
                q.nonce = st
                    .nonces
                    .write()
                    .map_err(|_| "nonce lock poisoned".to_string())?
                    .reserve(chain_id, &q.from.to_string(), chain_nonce);
            }

            let tx = self.unsigned_tx(&q)?;
            let purpose = match &q.token {
                Some(t) if !t.native => format!("Send {} {}", q.amount, t.symbol),
                _ => format!("Send {} wei", q.amount),
            };
            let intent = json!({
                "address": q.from.to_string(),
                "purpose": purpose,
                "legs": [{ "kind": "tx", "chain_id": chain_id, "tx": tx }],
            });

            let raw = modules()
                .keystore_module
                .request_approval(&intent.to_string())
                .map_err(|e| format!("{e:?}"))?;
            let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            if v.get("ok").and_then(Value::as_bool) != Some(true) {
                if let Ok(mut n) = st.nonces.write() {
                    n.release(chain_id, &q.from.to_string(), q.nonce);
                }
                return Err(v.get("error").and_then(Value::as_str)
                    .unwrap_or("the keystore refused the approval request").to_string());
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
                kind: if q.token.as_ref().map(|t| t.native).unwrap_or(true) { "native".into() } else { "erc20".into() },
                token: q.token.as_ref().and_then(|t| t.address.clone()),
                nonce: q.nonce,
                status: SendStatus::AwaitingApproval,
                broadcast_started: false,
            };
            st.jobs
                .write()
                .map_err(|_| "job lock poisoned".to_string())?
                .insert(job.request_id.clone(), job.clone());
            Ok(job.request_id)
        });

        match outcome {
            // Deliberately no hash: nothing is signed or broadcast until a human approves.
            Ok(id) => json!({ "ok": true, "pending": true, "requestId": id }).to_string(),
            Err(e) => err(e),
        }
    }

    fn send_status(&self, request_id: String) -> String {
        let result = self.with_state(|st| {
            let job = st
                .jobs
                .read()
                .map_err(|_| "job lock poisoned".to_string())?
                .get(&request_id)
                .cloned()
                .ok_or_else(|| format!("no send with id '{request_id}'"))?;
            if job.status.is_terminal() {
                return Ok(Self::job_reply(&job));
            }

            let raw = modules()
                .keystore_module
                .approval_status(&job.handle, &job.receipt)
                .map_err(|e| format!("{e:?}"))?;
            let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            if v.get("ok").and_then(Value::as_bool) != Some(true) {
                return Ok(Self::job_reply(&self.settle(
                    st,
                    job,
                    SendStatus::Failed {
                        reason: v.get("error").and_then(Value::as_str)
                            .unwrap_or("the keystore lost this request").to_string(),
                    },
                )));
            }
            match v.get("state").and_then(Value::as_str).unwrap_or("") {
                "offered" | "rendered" => return Ok(Self::job_reply(&job)),
                "settled" => {}
                other => return Err(format!("unknown approval state '{other}'")),
            }
            match v.get("reason").and_then(Value::as_str).unwrap_or("approved") {
                "approved" => {}
                "rejected" => return Ok(Self::job_reply(&self.settle(st, job, SendStatus::Rejected))),
                r => {
                    return Ok(Self::job_reply(&self.settle(
                        st, job, SendStatus::Failed { reason: r.to_string() },
                    )))
                }
            }

            // Approved. Claim the broadcast under the write lock so a concurrent poll cannot
            // send the same signed transaction twice.
            {
                let mut jobs = st.jobs.write().map_err(|_| "job lock poisoned".to_string())?;
                let live = jobs.get_mut(&request_id).ok_or("the send vanished mid-flight")?;
                if !live.claim_broadcast() {
                    return Ok(Self::job_reply(live));
                }
            }

            let fetched = modules()
                .keystore_module
                .fetch_result(&job.handle, &job.receipt)
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
                return Ok(Self::job_reply(&self.settle(
                    st, job, SendStatus::Failed { reason: "the approval carried no signature".into() },
                )));
            };

            let sent = modules()
                .eth_rpc_module
                .send_raw_transaction(job.chain_id as i64, &raw_tx)
                .map_err(|e| format!("{e:?}"))
                .and_then(|r| unwrap_rpc(&r));
            let hash = match sent {
                Ok(v) => match v.as_str().map(str::to_string).filter(|h| !h.is_empty()) {
                    Some(h) => h,
                    None => {
                        return Ok(Self::job_reply(&self.settle(
                            st,
                            job,
                            // The transaction may well be on-chain; we simply cannot follow it.
                            SendStatus::Failed {
                                reason: "the node accepted the transaction but returned no hash".into(),
                            },
                        )))
                    }
                },
                Err(e) => {
                    return Ok(Self::job_reply(&self.settle(
                        st, job, SendStatus::Failed { reason: e },
                    )))
                }
            };

            // Record only now: a record written at request time would show the user a
            // transaction they never approved, with a hash that does not exist.
            st.history.add(
                &job.from,
                TxRecord {
                    hash: hash.clone(),
                    chain_id: job.chain_id,
                    from: job.from.clone(),
                    to: job.to.clone(),
                    value: job.value.clone(),
                    kind: job.kind.clone(),
                    token: job.token.clone(),
                    status: "pending".into(),
                    timestamp: crate::history::now_secs(),
                },
            );
            let _ = modules().keystore_module.ack_result(&job.handle, &job.receipt);
            emit_tx_status_changed(&hash);
            Ok(Self::job_reply(&self.settle(st, job, SendStatus::Broadcast { hash })))
        });
        match result {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn cancel_send(&self, request_id: String) -> String {
        match self.with_state(|st| {
            let job = st
                .jobs
                .read()
                .map_err(|_| "job lock poisoned".to_string())?
                .get(&request_id)
                .cloned()
                .ok_or_else(|| format!("no send with id '{request_id}'"))?;
            if job.status.is_terminal() {
                return Err(format!("this send is already {:?} and cannot be cancelled", job.status));
            }
            let _ = modules().keystore_module.cancel_approval(&job.handle, &job.receipt);
            Ok(Self::job_reply(&self.settle(st, job, SendStatus::Cancelled)))
        }) {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn refresh_tx_status(&self, hash_hex: String) -> String {
        match self.with_state(|st| {
            let chain_id = st.settings.load().active_chain_id;
            let raw = modules()
                .eth_rpc_module
                .get_transaction_receipt(chain_id as i64, &hash_hex)
                .map_err(|e| format!("{e:?}"))?;
            let receipt = unwrap_rpc(&raw)?;
            // A null receipt means "not mined yet", which is not an error.
            let status = match receipt.get("status").and_then(Value::as_str) {
                None => "pending",
                Some(s) if s == "0x1" => "confirmed",
                Some(_) => "failed",
            };
            if status != "pending" {
                if let Some(from) = receipt.get("from").and_then(Value::as_str) {
                    st.history.update_status(from, &hash_hex, status);
                }
                emit_tx_status_changed(&hash_hex);
            }
            Ok(json!({ "ok": true, "hash": hash_hex, "status": status }))
        }) {
            Ok(v) => v.to_string(),
            Err(e) => err(e),
        }
    }

    fn suggest_fees(&self) -> String {
        let chain_id = match self.with_state(|st| Ok(st.settings.load().active_chain_id)) {
            Ok(id) => id,
            Err(e) => return err(e),
        };
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

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn unwrap_rpc_reads_the_broadcast_hash_key_too() {
        // send_raw_transaction answers `{ok, hash}` while every other method answers
        // `{ok, result}`. Reading only `result` loses the hash of a transaction that has
        // already moved money.
        assert_eq!(unwrap_rpc(r#"{"ok":true,"hash":"0xabc"}"#).unwrap(), json!("0xabc"));
        assert_eq!(unwrap_rpc(r#"{"ok":true,"result":"0x2a"}"#).unwrap(), json!("0x2a"));
    }

    #[test]
    fn unwrap_rpc_surfaces_the_inner_error_not_a_generic_one() {
        assert_eq!(unwrap_rpc(r#"{"ok":true,"result":"0x2a"}"#).unwrap(), json!("0x2a"));
        let e = unwrap_rpc(r#"{"ok":false,"error":"no configuration for chain 7"}"#).unwrap_err();
        assert_eq!(e, "no configuration for chain 7");
        assert!(unwrap_rpc("not json").is_err());
    }
}
