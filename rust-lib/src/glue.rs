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

use std::path::PathBuf;
use std::sync::RwLock;

use serde_json::{json, Value};

use crate::history::History;
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

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

pub trait EthWalletBackendModuleEvents {
    fn balances_updated(&self, address: String);
    fn active_chain_changed(&self, chain_id: i64);
    fn tx_status_changed(&self, hash_hex: String);
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct EthWalletBackendImpl {
    state: RwLock<Option<State>>,
}

struct State {
    settings: SettingsStore,
    history: History,
}

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

const NO_CONTEXT: &str = "module context not ready";

/// `eth_rpc_module` wraps every reply as `{ ok, result }`. Unwrap to the inner result,
/// surfacing its own error rather than a generic one.
fn unwrap_rpc(reply: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(reply).map_err(|e| e.to_string())?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("eth_rpc call failed")
            .to_string());
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
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
            *g = Some(State { settings, history });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwrap_rpc_surfaces_the_inner_error_not_a_generic_one() {
        assert_eq!(unwrap_rpc(r#"{"ok":true,"result":"0x2a"}"#).unwrap(), json!("0x2a"));
        let e = unwrap_rpc(r#"{"ok":false,"error":"no configuration for chain 7"}"#).unwrap_err();
        assert_eq!(e, "no configuration for chain 7");
        assert!(unwrap_rpc("not json").is_err());
    }
}
