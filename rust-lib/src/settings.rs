//! Persisted wallet settings: which network is active, and the per-network transport the
//! user has chosen. One network is active at a time — there is no chain list to fan out over.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::networks;

/// How JSON-RPC for a network should be routed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VerifiedProxyMode {
    /// Talk to the configured endpoint directly.
    #[default]
    Off,
    /// Route through the light-client proxy; refuse rather than fall back.
    Required,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkSettings {
    pub chain_id: u64,
    /// Empty means "no endpoint chosen yet" — the wallet cannot read until one is set.
    #[serde(default)]
    pub rpc_url: String,
    #[serde(default)]
    pub verified_proxy_mode: VerifiedProxyMode,
}

impl NetworkSettings {
    fn new(chain_id: u64) -> Self {
        Self { chain_id, rpc_url: String::new(), verified_proxy_mode: VerifiedProxyMode::Off }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub active_chain_id: u64,
    pub networks: Vec<NetworkSettings>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            active_chain_id: networks::DEFAULT_CHAIN_ID,
            networks: networks::ALL.iter().map(|n| NetworkSettings::new(n.chain_id)).collect(),
        }
    }
}

impl Settings {
    pub fn network(&self, chain_id: u64) -> Option<&NetworkSettings> {
        self.networks.iter().find(|n| n.chain_id == chain_id)
    }

    fn network_mut(&mut self, chain_id: u64) -> Option<&mut NetworkSettings> {
        self.networks.iter_mut().find(|n| n.chain_id == chain_id)
    }

    pub fn active(&self) -> Option<&NetworkSettings> {
        self.network(self.active_chain_id)
    }

    /// Drop settings for networks no longer offered and add any that are missing, so an
    /// older on-disk file cannot leave the active network unrepresented.
    fn reconcile(&mut self) {
        self.networks.retain(|n| networks::is_supported(n.chain_id));
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

pub struct SettingsStore {
    path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SettingsError {
    UnsupportedChain(u64),
    /// A write failed. Callers surface this rather than dropping it: a silently
    /// unpersisted network switch is how a user sends on the wrong chain.
    Persist(String),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsError::UnsupportedChain(id) => write!(
                f,
                "chain {id} is not one of this wallet's networks (1 mainnet, 11155111 sepolia, 560048 hoodi)"
            ),
            SettingsError::Persist(e) => write!(f, "could not save settings: {e}"),
        }
    }
}

impl SettingsStore {
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Settings {
        let mut s: Settings = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        s.reconcile();
        s
    }

    fn save(&self, s: &Settings) -> Result<(), SettingsError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SettingsError::Persist(e.to_string()))?;
        }
        let txt =
            serde_json::to_string_pretty(s).map_err(|e| SettingsError::Persist(e.to_string()))?;
        std::fs::write(&self.path, txt).map_err(|e| SettingsError::Persist(e.to_string()))
    }

    pub fn set_active_chain(&self, chain_id: u64) -> Result<Settings, SettingsError> {
        if !networks::is_supported(chain_id) {
            return Err(SettingsError::UnsupportedChain(chain_id));
        }
        let mut s = self.load();
        s.active_chain_id = chain_id;
        self.save(&s)?;
        Ok(s)
    }

    pub fn set_rpc_url(&self, chain_id: u64, url: &str) -> Result<Settings, SettingsError> {
        let mut s = self.load();
        let n = s.network_mut(chain_id).ok_or(SettingsError::UnsupportedChain(chain_id))?;
        n.rpc_url = url.trim().to_string();
        self.save(&s)?;
        Ok(s)
    }

    pub fn set_verified_proxy_mode(
        &self,
        chain_id: u64,
        mode: VerifiedProxyMode,
    ) -> Result<Settings, SettingsError> {
        let mut s = self.load();
        let n = s.network_mut(chain_id).ok_or(SettingsError::UnsupportedChain(chain_id))?;
        n.verified_proxy_mode = mode;
        self.save(&s)?;
        Ok(s)
    }
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
        let s = store(&d).load();
        assert_eq!(s.active_chain_id, 1);
        assert_eq!(s.networks.len(), networks::ALL.len());
        assert_eq!(s.active().unwrap().verified_proxy_mode, VerifiedProxyMode::Off);
        assert!(s.active().unwrap().rpc_url.is_empty());
    }

    #[test]
    fn set_active_chain_refuses_an_unsupported_chain_and_leaves_the_active_one_alone() {
        let d = tempfile::tempdir().unwrap();
        let st = store(&d);
        assert_eq!(st.set_active_chain(999).unwrap_err(), SettingsError::UnsupportedChain(999));
        // An L2 is refused for the same reason: this wallet is Ethereum only.
        assert_eq!(st.set_active_chain(10).unwrap_err(), SettingsError::UnsupportedChain(10));
        assert_eq!(st.load().active_chain_id, 1);
    }

    #[test]
    fn settings_round_trip_through_disk() {
        let d = tempfile::tempdir().unwrap();
        let st = store(&d);
        st.set_active_chain(11_155_111).unwrap();
        st.set_rpc_url(11_155_111, "  https://example.invalid/rpc  ").unwrap();
        st.set_verified_proxy_mode(11_155_111, VerifiedProxyMode::Required).unwrap();

        let reread = SettingsStore::with_path(d.path().join("settings.json")).load();
        assert_eq!(reread.active_chain_id, 11_155_111);
        let n = reread.active().unwrap();
        assert_eq!(n.rpc_url, "https://example.invalid/rpc", "the url must be trimmed");
        assert_eq!(n.verified_proxy_mode, VerifiedProxyMode::Required);
        // Switching networks must not carry the previous network's transport across.
        assert_eq!(reread.network(1).unwrap().verified_proxy_mode, VerifiedProxyMode::Off);
        assert!(reread.network(1).unwrap().rpc_url.is_empty());
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
        let s = SettingsStore::with_path(p).load();
        assert_eq!(s.active_chain_id, 1, "an unsupported active chain falls back to mainnet");
        assert_eq!(s.networks.len(), networks::ALL.len());
        assert!(s.network(42161).is_none(), "the dropped network must not survive");
    }

    #[test]
    fn verified_proxy_mode_serialises_as_camel_case() {
        let json = serde_json::to_string(&VerifiedProxyMode::Required).unwrap();
        assert_eq!(json, "\"required\"");
        assert_eq!(
            serde_json::from_str::<VerifiedProxyMode>("\"off\"").unwrap(),
            VerifiedProxyMode::Off
        );
    }
}
