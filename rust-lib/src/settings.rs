//! Persisted wallet settings: which network is active, which tokens the user turned on for
//! it, and the order the balance list is shown in. One network is active at a time — there is
//! no chain list to fan out over.
//!
//! `enabledTokens` holds FULL snapshotted records, not bare addresses. `decimals` scales every
//! amount this wallet renders or signs, so a bare address would make `tokens::for_chain` an
//! async lookup against `token_list` and leave a token's meaning hostage to a list that may be
//! stale, re-fetched or gone. A snapshot keeps the choke point synchronous and keeps a token
//! meaning today what it meant when the user chose it.
//!
//! Neither the verified-proxy mode nor the endpoint is written here any more. `eth_rpc_module`
//! owns both, and its store is shared with every wallet on the device; a second writer is how
//! the two come to disagree, and a wallet showing "required" while calls go clear-net is worse
//! than one that cannot answer at all.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::tokens::{Token, TokenSort};
use crate::{history, networks, tokens};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSettings {
    pub chain_id: u64,
    /// DEPRECATED: read at startup to seed `eth_rpc_module` and never written again. Kept so
    /// an endpoint set while eth_rpc was down is not lost; the `eth_rpc_ui` app owns it now.
    #[serde(default)]
    pub rpc_url: String,
    /// The tokens the user turned on for this network, each a whole record snapshotted from
    /// `token_list` at the moment it was enabled. Never the built-in rows: those are offered
    /// unconditionally, and storing one here would be a second copy that can drift.
    #[serde(default)]
    pub enabled_tokens: Vec<Token>,
}

impl NetworkSettings {
    fn new(chain_id: u64) -> Self {
        Self { chain_id, rpc_url: String::new(), enabled_tokens: Vec::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub active_chain_id: u64,
    pub networks: Vec<NetworkSettings>,
    /// How the balance list is ordered. Device-wide rather than per-network: it is a reading
    /// preference, and one that changed under the user on every network switch would read as
    /// a bug.
    #[serde(default)]
    pub token_sort: TokenSort,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            active_chain_id: networks::DEFAULT_CHAIN_ID,
            networks: networks::ALL.iter().map(|n| NetworkSettings::new(n.chain_id)).collect(),
            token_sort: TokenSort::default(),
        }
    }
}

impl Settings {
    pub fn network(&self, chain_id: u64) -> Option<&NetworkSettings> {
        self.networks.iter().find(|n| n.chain_id == chain_id)
    }

    pub fn active(&self) -> Option<&NetworkSettings> {
        self.network(self.active_chain_id)
    }

    /// The tokens the user turned on for `chain_id`. Empty for a network with none and for
    /// one this wallet does not offer — the same answer, because neither has an enabled set.
    pub fn enabled_tokens(&self, chain_id: u64) -> &[Token] {
        self.network(chain_id).map(|n| n.enabled_tokens.as_slice()).unwrap_or_default()
    }

    /// Drop settings for networks no longer offered and add any that are missing, so an
    /// older on-disk file cannot leave the active network unrepresented.
    ///
    /// Enabled rows are sanitised here too. A row with no address, or one claiming to be the
    /// native currency, is not a token this wallet could ever spend — it is a hand-edited or
    /// corrupted file — and dropping it at the door keeps the offer honest without every
    /// reader re-deriving the rule.
    fn reconcile(&mut self) {
        self.networks.retain(|n| networks::is_supported(n.chain_id));
        for n in self.networks.iter_mut() {
            n.enabled_tokens.retain(|t| {
                !t.native && t.address.as_deref().is_some_and(|a| !a.trim().is_empty())
            });
        }
        for n in networks::ALL {
            if self.network(n.chain_id).is_none() {
                self.networks.push(NetworkSettings::new(n.chain_id));
            }
        }
        if !networks::is_supported(self.active_chain_id) {
            self.active_chain_id = networks::DEFAULT_CHAIN_ID;
        }
    }
}

/// A completed write and whether it actually moved anything.
///
/// An event announces a CHANGE, so re-setting what is already stored must not emit one — a
/// view that re-reads on every no-op write drives itself round in a loop. `changed` is a
/// whole-value diff taken inside the one read-modify-write section, which is what makes it
/// true of what reached disk rather than of what the caller asked for.
#[derive(Clone, Debug)]
pub struct Applied {
    pub settings: Settings,
    pub changed: bool,
}

pub struct SettingsStore {
    path: PathBuf,
    /// Serializes read-modify-write, so two concurrent switches cannot each write from
    /// their own snapshot. The same shape `History` uses.
    gate: Mutex<()>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SettingsError {
    UnsupportedChain(u64),
    /// The change asked for is not one this wallet can make — turning off a built-in row, or
    /// naming something that is not a token address. Distinct from a write failure: nothing
    /// went wrong, the answer is no.
    RefusedToken(String),
    /// A write failed. Callers surface this rather than dropping it: a silently
    /// unpersisted network switch is how a user sends on the wrong chain.
    Persist(String),
    /// The file is there and is not settings. Never folded into the default: answering
    /// "chain 1" for a config we could not read is the one wrong direction here — the
    /// wallet would gate, price and label against a network the user is not on.
    Unreadable(String),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsError::RefusedToken(e) => write!(f, "{e}"),
            SettingsError::UnsupportedChain(id) => write!(
                f,
                "chain {id} is not one of this wallet's networks (1 mainnet, 11155111 sepolia, 560048 hoodi)"
            ),
            SettingsError::Persist(e) => write!(f, "could not save settings: {e}"),
            SettingsError::Unreadable(e) => write!(f, "could not read settings: {e}"),
        }
    }
}

impl SettingsStore {
    pub fn with_path(path: PathBuf) -> Self {
        Self { path, gate: Mutex::new(()) }
    }

    /// The stored settings, or why they could not be read. A missing or empty file IS an
    /// empty config and defaults honestly; bytes that will not parse are not, and a caller
    /// that cannot be told the truth must be told nothing.
    pub fn try_load(&self) -> Result<Settings, SettingsError> {
        let _g = self.gate.lock();
        self.read()
    }

    fn read(&self) -> Result<Settings, SettingsError> {
        let txt = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            // Only an ABSENT file is an empty config. Zero bytes is what a truncating write
            // leaves behind, and reading that as chain 1 is the defect itself.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(e) => return Err(SettingsError::Unreadable(e.to_string())),
        };
        let mut s: Settings =
            serde_json::from_str(&txt).map_err(|e| SettingsError::Unreadable(e.to_string()))?;
        s.reconcile();
        Ok(s)
    }

    /// Move bytes that are not settings aside, never over. Reads keep failing until then —
    /// a config we cannot read must not answer chain 1 — but naming a network is a way out.
    fn quarantine(&self) -> bool {
        let aside =
            self.path.with_extension(format!("json.unreadable-{}", history::now_secs()));
        std::fs::rename(&self.path, aside).is_ok()
    }

    /// Write by rename. `std::fs::write` truncates first, and a reader landing in that window
    /// parsed nothing and silently read as mainnet.
    fn save(&self, s: &Settings) -> Result<(), SettingsError> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SettingsError::Persist(e.to_string()))?;
        }
        let txt =
            serde_json::to_string_pretty(s).map_err(|e| SettingsError::Persist(e.to_string()))?;
        let tmp = self.path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        match history::write_then_rename(&tmp, &self.path, &txt) {
            true => Ok(()),
            false => Err(SettingsError::Persist(format!(
                "could not replace {}",
                self.path.display()
            ))),
        }
    }

    /// Read, mutate and write as ONE critical section, so two concurrent changes cannot each
    /// write from their own snapshot. Every mutator goes through here — a second read-modify-
    /// write shape is how one of them ends up dropping the other's edit.
    ///
    /// A file that will not parse is moved aside rather than over, exactly as before: naming a
    /// setting is the way out of an unreadable config, and nothing readable is lost.
    fn update(
        &self,
        edit: impl FnOnce(&mut Settings) -> Result<(), SettingsError>,
    ) -> Result<Applied, SettingsError> {
        let _g = self.gate.lock();
        let (mut s, was_readable) = match self.read() {
            Ok(s) => (s, true),
            Err(SettingsError::Unreadable(_)) if self.quarantine() => (Settings::default(), false),
            Err(e) => return Err(e),
        };
        // Compared against the RECONCILED load, not the file: repairing an old on-disk shape
        // is this store's own housekeeping and is not the change the caller asked for. A
        // config that could NOT be read answered nothing at all, so replacing it is a change
        // even when the values land on the defaults.
        let before = s.clone();
        edit(&mut s)?;
        self.save(&s)?;
        Ok(Applied { changed: !was_readable || s != before, settings: s })
    }

    pub fn set_active_chain(&self, chain_id: u64) -> Result<Applied, SettingsError> {
        if !networks::is_supported(chain_id) {
            return Err(SettingsError::UnsupportedChain(chain_id));
        }
        self.update(|s| {
            s.active_chain_id = chain_id;
            Ok(())
        })
    }

    /// Turn on `token` for `chain_id`, storing the whole record. The caller has already taken
    /// the snapshot from `token_list`; this refuses to invent one.
    ///
    /// A built-in address is accepted and stored NOTHING: the row is already offered
    /// unconditionally, and a stored copy would be a second truth that can disagree with the
    /// table. Re-enabling replaces the stored snapshot, so a list correction can be adopted.
    pub fn enable_token(&self, chain_id: u64, token: Token) -> Result<Applied, SettingsError> {
        let addr = validated_address(chain_id, token.address.as_deref())?;
        self.update(move |s| {
            if tokens::is_builtin(chain_id, &addr) {
                return Ok(());
            }
            let Some(net) = s.networks.iter_mut().find(|n| n.chain_id == chain_id) else {
                return Err(SettingsError::UnsupportedChain(chain_id));
            };
            net.enabled_tokens
                .retain(|t| !t.address.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(&addr)));
            net.enabled_tokens.push(Token { address: Some(addr), native: false, ..token });
            Ok(())
        })
    }

    /// Turn off the token at `address` on `chain_id`. Refuses a built-in row: the native
    /// currency pays every fee and WETH is this wallet's own assertion, so neither is the
    /// user's to remove. Removing one that is not there is a no-op, not an error.
    pub fn disable_token(&self, chain_id: u64, address: &str) -> Result<Applied, SettingsError> {
        let addr = validated_address(chain_id, Some(address))?;
        if tokens::is_builtin(chain_id, &addr) {
            return Err(SettingsError::RefusedToken(format!(
                "{addr} is built in on chain {chain_id} and cannot be turned off"
            )));
        }
        self.update(move |s| {
            let Some(net) = s.networks.iter_mut().find(|n| n.chain_id == chain_id) else {
                return Err(SettingsError::UnsupportedChain(chain_id));
            };
            net.enabled_tokens
                .retain(|t| !t.address.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(&addr)));
            Ok(())
        })
    }

    pub fn set_token_sort(&self, order: TokenSort) -> Result<Applied, SettingsError> {
        self.update(|s| {
            s.token_sort = order;
            Ok(())
        })
    }
}

/// `address` as EIP-55, or why it is not a token address at all. Normalised on the way in so
/// the file has one casing and the screen shows a checksummed address the user can proofread;
/// the 20 bytes are the list's, and re-casing changes nothing about them.
///
/// The native currency has no address, so this is also what refuses an attempt to turn the
/// chain's own currency off.
fn validated_address(chain_id: u64, address: Option<&str>) -> Result<String, SettingsError> {
    if !networks::is_supported(chain_id) {
        return Err(SettingsError::UnsupportedChain(chain_id));
    }
    let raw = address.unwrap_or_default().trim();
    raw.parse::<alloy::primitives::Address>()
        .map(|a| a.to_string())
        .map_err(|e| SettingsError::RefusedToken(format!("'{raw}' is not a token address: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &tempfile::TempDir) -> SettingsStore {
        SettingsStore::with_path(dir.path().join("settings.json"))
    }

    #[test]
    fn defaults_to_mainnet_with_every_network_represented() {
        let d = tempfile::tempdir().unwrap();
        let s = store(&d).try_load().unwrap();
        assert_eq!(s.active_chain_id, 1);
        assert_eq!(s.networks.len(), networks::ALL.len());
        assert!(s.active().unwrap().rpc_url.is_empty());
    }

    #[test]
    fn set_active_chain_refuses_an_unsupported_chain_and_leaves_the_active_one_alone() {
        let d = tempfile::tempdir().unwrap();
        let st = store(&d);
        assert_eq!(st.set_active_chain(999).unwrap_err(), SettingsError::UnsupportedChain(999));
        // An L2 is refused for the same reason: this wallet is Ethereum only.
        assert_eq!(st.set_active_chain(10).unwrap_err(), SettingsError::UnsupportedChain(10));
        assert_eq!(st.try_load().unwrap().active_chain_id, 1);
    }

    #[test]
    fn settings_round_trip_through_disk() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        // A file an older build wrote, carrying the deprecated per-network endpoint.
        std::fs::write(
            &p,
            r#"{"activeChainId":1,"networks":[{"chainId":11155111,"rpcUrl":"https://a.invalid"}]}"#,
        )
        .unwrap();
        let st = SettingsStore::with_path(p.clone());
        st.set_active_chain(11_155_111).unwrap();

        let reread = SettingsStore::with_path(p).try_load().unwrap();
        assert_eq!(reread.active_chain_id, 11_155_111);
        // We stopped writing the endpoint; we must not erase one either, or a user who set it
        // while eth_rpc was down loses it on the next network switch.
        assert_eq!(reread.active().unwrap().rpc_url, "https://a.invalid");
        let other = &reread.network(1).unwrap().rpc_url;
        assert!(other.is_empty(), "one network's transport is its own");
    }

    #[test]
    fn an_on_disk_file_naming_a_dropped_network_reconciles_to_the_default() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        std::fs::write(
            &p,
            r#"{"activeChainId":42161,"networks":[{"chainId":42161,"rpcUrl":"https://arb.invalid"}]}"#,
        )
        .unwrap();
        let s = SettingsStore::with_path(p).try_load().unwrap();
        assert_eq!(s.active_chain_id, 1, "an unsupported active chain falls back to mainnet");
        assert_eq!(s.networks.len(), networks::ALL.len());
        assert!(s.network(42161).is_none(), "the dropped network must not survive");
    }

    #[test]
    fn a_stale_verified_proxy_mode_key_on_disk_is_ignored_not_a_load_failure() {
        // eth_rpc owns the mode now. An existing file still carrying our old copy must
        // load unchanged; serde drops the unknown key on the next write.
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        std::fs::write(
            &p,
            r#"{"activeChainId":11155111,"networks":[
                 {"chainId":11155111,"rpcUrl":"https://a.invalid","verifiedProxyMode":"required"}]}"#,
        )
        .unwrap();
        let st = SettingsStore::with_path(p.clone());
        assert_eq!(st.try_load().unwrap().active().unwrap().rpc_url, "https://a.invalid");
        st.set_active_chain(1).unwrap();
        assert!(!std::fs::read_to_string(&p).unwrap().contains("verifiedProxyMode"));
    }

    /// FINDING 2. `std::fs::write` truncates and then writes; a reader landing in that window
    /// parsed nothing, defaulted, and reported MAINNET. The two stores are the point — one
    /// per `State`, one per process — so they share the file and not the gate.
    #[test]
    fn a_concurrent_reader_never_sees_a_truncated_file_and_never_answers_mainnet() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        let writer = SettingsStore::with_path(p.clone());
        let reader = SettingsStore::with_path(p);
        writer.set_active_chain(11_155_111).unwrap();

        std::thread::scope(|s| {
            s.spawn(|| {
                for _ in 0..400 {
                    writer.set_active_chain(11_155_111).unwrap();
                }
            });
            s.spawn(|| {
                for _ in 0..4000 {
                    match reader.try_load() {
                        Ok(s) => assert_eq!(
                            s.active_chain_id, 11_155_111,
                            "a read landed inside a write and answered mainnet"
                        ),
                        Err(e) => panic!("a read landed inside a write: {e}"),
                    }
                }
            });
        });
    }

    /// A config we cannot read must SAY so. Silently defaulting means the wallet gates, prices
    /// and labels against a network the user is not on.
    #[test]
    fn bytes_that_are_not_settings_are_an_error_not_a_silent_mainnet() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        let st = SettingsStore::with_path(p.clone());
        assert_eq!(st.try_load().unwrap().active_chain_id, 1, "an ABSENT file is an empty config");

        for bytes in ["", r#"{"activeChainId":111"#, "not json at all", "[]"] {
            std::fs::write(&p, bytes).unwrap();
            let got = st.try_load();
            assert!(
                matches!(got, Err(SettingsError::Unreadable(_))),
                "{bytes:?} read as settings ({:?}); zero bytes is what a truncated write leaves",
                got.map(|s| s.active_chain_id)
            );
        }
    }

    #[test]
    fn a_switch_moves_a_config_it_could_not_read_aside_rather_than_over() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        std::fs::write(&p, "{ truncated").unwrap();
        let st = SettingsStore::with_path(p.clone());
        assert!(matches!(st.try_load(), Err(SettingsError::Unreadable(_))), "reads stay honest");

        // Naming a network is the way out: it is the whole file, so nothing readable is lost.
        assert_eq!(st.set_active_chain(11_155_111).unwrap().settings.active_chain_id, 11_155_111);
        assert_eq!(st.try_load().unwrap().active_chain_id, 11_155_111);
        let aside: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("unreadable"))
            .collect();
        assert_eq!(aside.len(), 1, "the bytes are kept under a name that says what happened");
        assert_eq!(std::fs::read_to_string(aside[0].path()).unwrap(), "{ truncated");
    }

    // ── the enabled token set and the balance order ──────────────────────────────────

    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";

    fn usdc() -> Token {
        Token {
            symbol: "USDC".into(),
            name: "USD Coin".into(),
            decimals: 6,
            address: Some(USDC.into()),
            native: false,
        }
    }

    #[test]
    fn a_fresh_wallet_has_nothing_enabled_and_reads_alphabetically() {
        let d = tempfile::tempdir().unwrap();
        let s = store(&d).try_load().unwrap();
        assert_eq!(s.token_sort, TokenSort::Alpha);
        for net in networks::ALL {
            assert!(s.enabled_tokens(net.chain_id).is_empty(), "on {}", net.key);
            // Which is exactly native + the verified WETH, and nothing else.
            let offered: Vec<String> = tokens::for_chain(net.chain_id, s.enabled_tokens(net.chain_id))
                .iter()
                .map(|t| t.symbol.clone())
                .collect();
            let expected: &[&str] = if net.chain_id == 1 { &["ETH", "WETH"] } else { &["ETH"] };
            assert_eq!(offered, expected, "on {}", net.key);
        }
        // A chain this wallet does not offer has no enabled set, rather than a panic.
        assert!(s.enabled_tokens(42_161).is_empty());
    }

    #[test]
    fn enabling_and_disabling_a_token_round_trips_through_a_restart() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        SettingsStore::with_path(p.clone()).enable_token(1, usdc()).unwrap();

        // A fresh store over the same file — the restart. The WHOLE record survives, decimals
        // included, which is what keeps the offer synchronous and the amounts correctly scaled.
        let reread = SettingsStore::with_path(p.clone()).try_load().unwrap();
        assert_eq!(reread.enabled_tokens(1), &[usdc()]);
        assert!(reread.enabled_tokens(11_155_111).is_empty(), "one network's set is its own");
        assert!(tokens::find(1, "USDC", reread.enabled_tokens(1)).is_some());

        // Off again, and the removal survives a restart the same way.
        SettingsStore::with_path(p.clone()).disable_token(1, &USDC.to_lowercase()).unwrap();
        let reread = SettingsStore::with_path(p).try_load().unwrap();
        assert!(reread.enabled_tokens(1).is_empty(), "case is not identity");
        assert!(tokens::find(1, "USDC", reread.enabled_tokens(1)).is_none());
    }

    #[test]
    fn a_builtin_row_cannot_be_turned_off_and_enabling_one_stores_nothing() {
        let d = tempfile::tempdir().unwrap();
        let st = store(&d);
        let e = st.disable_token(1, WETH).unwrap_err();
        assert!(matches!(e, SettingsError::RefusedToken(_)), "{e:?}");
        assert!(e.to_string().contains("cannot be turned off"), "{e}");

        // Enabling one is a no-op success: it is already offered, and a stored copy would be
        // a second truth the built-in table could drift from.
        let fake = Token { symbol: "WETH9".into(), decimals: 6, address: Some(WETH.into()), ..usdc() };
        let s = st.enable_token(1, fake).unwrap().settings;
        assert!(s.enabled_tokens(1).is_empty());
        assert_eq!(tokens::find(1, WETH, s.enabled_tokens(1)).unwrap().decimals, 18);
    }

    #[test]
    fn the_chains_own_currency_has_no_address_so_nothing_can_name_it() {
        let d = tempfile::tempdir().unwrap();
        let st = store(&d);
        for name in ["", "ETH", "native", "0x", "0xnothex"] {
            assert!(matches!(st.disable_token(1, name), Err(SettingsError::RefusedToken(_))), "{name:?}");
        }
        assert_eq!(st.try_load().unwrap().enabled_tokens(1).len(), 0);
    }

    #[test]
    fn an_address_is_stored_checksummed_and_re_enabling_replaces_the_snapshot() {
        let d = tempfile::tempdir().unwrap();
        let st = store(&d);
        let lower = Token { address: Some(USDC.to_lowercase()), ..usdc() };
        let s = st.enable_token(1, lower).unwrap().settings;
        assert_eq!(s.enabled_tokens(1)[0].address.as_deref(), Some(USDC), "one casing on disk");

        // A list correction is adopted rather than duplicated.
        let fixed = Token { name: "USD Coin (corrected)".into(), ..usdc() };
        let s = st.enable_token(1, fixed.clone()).unwrap().settings;
        assert_eq!(s.enabled_tokens(1), &[fixed]);
    }

    #[test]
    fn an_unsupported_chain_is_refused_by_every_token_mutator() {
        let d = tempfile::tempdir().unwrap();
        let st = store(&d);
        for chain in [999u64, 10] {
            assert_eq!(st.enable_token(chain, usdc()).unwrap_err(), SettingsError::UnsupportedChain(chain));
            assert_eq!(st.disable_token(chain, USDC).unwrap_err(), SettingsError::UnsupportedChain(chain));
        }
    }

    #[test]
    fn the_balance_order_persists_and_an_old_file_without_one_reads_alphabetically() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        SettingsStore::with_path(p.clone()).set_token_sort(TokenSort::Balance).unwrap();
        assert_eq!(SettingsStore::with_path(p.clone()).try_load().unwrap().token_sort, TokenSort::Balance);
        assert!(std::fs::read_to_string(&p).unwrap().contains("\"balance\""));

        // A file an older build wrote names no order at all; that is the default, not an error.
        std::fs::write(&p, r#"{"activeChainId":1,"networks":[]}"#).unwrap();
        assert_eq!(SettingsStore::with_path(p).try_load().unwrap().token_sort, TokenSort::Alpha);
    }

    #[test]
    fn a_hand_edited_enabled_row_that_could_never_be_spent_is_dropped_at_the_door() {
        // No address is unspendable and a second `native` would put a second Multicall3 leg on
        // the same balance. Neither is a token this wallet could have written.
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        std::fs::write(
            &p,
            r#"{"activeChainId":1,"tokenSort":"balance","networks":[{"chainId":1,"enabledTokens":[
                 {"symbol":"GHOST","name":"No Address","decimals":18,"native":false},
                 {"symbol":"ETH2","name":"Second Ether","decimals":18,"address":"0x6B175474E89094C44Da98b954EedeAC495271d0F","native":true},
                 {"symbol":"USDC","name":"USD Coin","decimals":6,"address":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","native":false}]}]}"#,
        )
        .unwrap();
        let s = SettingsStore::with_path(p).try_load().unwrap();
        assert_eq!(s.enabled_tokens(1), &[usdc()], "only the row that could be spent survives");
        assert_eq!(s.token_sort, TokenSort::Balance, "the rest of the file is untouched");
    }

    #[test]
    fn a_write_leaves_no_temporary_behind() {
        let d = tempfile::tempdir().unwrap();
        let st = store(&d);
        st.set_active_chain(11_155_111).unwrap();
        st.set_active_chain(1).unwrap();
        let tmps: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(tmps.is_empty(), "write-by-rename must not accumulate temporaries: {tmps:?}");
    }

    /// Rule: an event announces a CHANGE. Every mutator reports whether it moved anything,
    /// and re-setting what is already stored must report `false` — a view that re-reads on a
    /// no-op write drives itself round in a loop.
    #[test]
    fn re_setting_what_is_already_stored_is_not_a_change() {
        let d = tempfile::tempdir().unwrap();
        let st = store(&d);

        assert!(st.set_active_chain(11_155_111).unwrap().changed);
        assert!(!st.set_active_chain(11_155_111).unwrap().changed);
        assert!(st.set_active_chain(1).unwrap().changed);

        assert!(st.set_token_sort(TokenSort::Balance).unwrap().changed);
        assert!(!st.set_token_sort(TokenSort::Balance).unwrap().changed);

        assert!(st.enable_token(1, usdc()).unwrap().changed);
        assert!(!st.enable_token(1, usdc()).unwrap().changed, "the same snapshot, again");
        // A corrected record replaces the stored one, so it IS a change.
        let fixed = Token { name: "USD Coin (corrected)".into(), ..usdc() };
        assert!(st.enable_token(1, fixed).unwrap().changed);

        assert!(st.disable_token(1, USDC).unwrap().changed);
        assert!(!st.disable_token(1, USDC).unwrap().changed, "removing what is not there");

        // Enabling a built-in stores nothing, so there is nothing to announce.
        assert!(!st.enable_token(1, Token { address: Some(WETH.into()), ..usdc() }).unwrap().changed);
    }

    /// A config that could not be read answered every reader with an error, so replacing it
    /// is an observable change even when the write lands on the values already asked for.
    #[test]
    fn repairing_an_unreadable_config_is_a_change_even_at_the_default_values() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.json");
        std::fs::write(&p, "not json at all").unwrap();
        let st = SettingsStore::with_path(p);
        assert!(st.try_load().is_err(), "the file has to be unreadable for this to mean anything");
        assert!(st.set_active_chain(networks::DEFAULT_CHAIN_ID).unwrap().changed);
        assert!(!st.set_active_chain(networks::DEFAULT_CHAIN_ID).unwrap().changed);
    }
}
