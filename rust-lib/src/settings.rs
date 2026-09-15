//! The wallet's one persisted view preference. Chain facts and scope live in
//! `eth_rpc_module`; token membership and snapshots live in `token_list_module`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::store;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenSort {
    #[default]
    Alpha,
    Balance,
}

impl TokenSort {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "alpha" => Some(Self::Alpha),
            "balance" => Some(Self::Balance),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self { Self::Alpha => "alpha", Self::Balance => "balance" }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default)]
    pub token_sort: TokenSort,
}

impl Default for Settings {
    fn default() -> Self {
        Self { token_sort: TokenSort::Alpha }
    }
}

#[derive(Clone, Debug)]
pub struct Applied {
    pub settings: Settings,
    pub changed: bool,
}

pub struct SettingsStore {
    path: PathBuf,
    gate: Mutex<()>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SettingsError {
    Persist(String),
    Unreadable(String),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Persist(error) => write!(f, "could not save settings: {error}"),
            Self::Unreadable(error) => write!(f, "could not read settings: {error}"),
        }
    }
}

impl SettingsStore {
    pub fn with_path(path: PathBuf) -> Self { Self { path, gate: Mutex::new(()) } }

    pub fn try_load(&self) -> Result<Settings, SettingsError> {
        let _guard = self.gate.lock();
        self.read()
    }

    fn read(&self) -> Result<Settings, SettingsError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Settings::default());
            }
            Err(error) => return Err(SettingsError::Unreadable(error.to_string())),
        };
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| SettingsError::Unreadable(error.to_string()))?;
        if !value.is_object() {
            return Err(SettingsError::Unreadable("settings must be a JSON object".into()));
        }
        serde_json::from_value(value).map_err(|error| SettingsError::Unreadable(error.to_string()))
    }

    fn quarantine(&self) -> bool {
        let aside = self.path.with_extension(format!("json.unreadable-{}", store::now_secs()));
        std::fs::rename(&self.path, aside).is_ok()
    }

    fn save(&self, settings: &Settings) -> Result<(), SettingsError> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SettingsError::Persist(e.to_string()))?;
        }
        let text = serde_json::to_string_pretty(settings)
            .map_err(|e| SettingsError::Persist(e.to_string()))?;
        let temporary = self.path.with_extension(format!(
            "{}.{}.tmp", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        if store::write_then_rename(&temporary, &self.path, &text) {
            Ok(())
        } else {
            Err(SettingsError::Persist(format!("could not replace {}", self.path.display())))
        }
    }

    fn update(&self, edit: impl FnOnce(&mut Settings)) -> Result<Applied, SettingsError> {
        let _guard = self.gate.lock();
        let (mut settings, readable) = match self.read() {
            Ok(settings) => (settings, true),
            Err(SettingsError::Unreadable(_)) if self.quarantine() => (Settings::default(), false),
            Err(error) => return Err(error),
        };
        let before = settings.clone();
        edit(&mut settings);
        self.save(&settings)?;
        Ok(Applied { changed: !readable || settings != before, settings })
    }

    pub fn set_token_sort(&self, order: TokenSort) -> Result<Applied, SettingsError> {
        self.update(|settings| settings.token_sort = order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &tempfile::TempDir) -> SettingsStore {
        SettingsStore::with_path(dir.path().join("settings.json"))
    }

    #[test]
    fn defaults_are_alpha() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(store(&dir).try_load().unwrap(), Settings::default());
    }

    #[test]
    fn old_network_and_enabled_token_copies_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"activeChainId":11155111,"tokenSort":"balance","networks":[{"chainId":1,"enabledTokens":[{"symbol":"OLD"}]}]}"#).unwrap();
        let settings = SettingsStore::with_path(path).try_load().unwrap();
        assert_eq!(settings.token_sort, TokenSort::Balance);
    }

    #[test]
    fn preference_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let writer = SettingsStore::with_path(path.clone());
        writer.set_token_sort(TokenSort::Balance).unwrap();
        let value = SettingsStore::with_path(path).try_load().unwrap();
        assert_eq!(value.token_sort, TokenSort::Balance);
    }

    #[test]
    fn unreadable_bytes_are_never_mainnet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = SettingsStore::with_path(path.clone());
        for bytes in ["", "{", "[]", "not json"] {
            std::fs::write(&path, bytes).unwrap();
            assert!(matches!(settings.try_load(), Err(SettingsError::Unreadable(_))), "{bytes:?}");
        }
    }

    #[test]
    fn a_write_repairs_an_unreadable_file_by_moving_it_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{").unwrap();
        let applied = SettingsStore::with_path(path.clone()).set_token_sort(TokenSort::Balance).unwrap();
        assert!(applied.changed);
        assert_eq!(SettingsStore::with_path(path).try_load().unwrap().token_sort, TokenSort::Balance);
        assert!(std::fs::read_dir(dir.path()).unwrap().flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains("unreadable")));
    }

    #[test]
    fn a_no_op_is_not_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let settings = store(&dir);
        assert!(!settings.set_token_sort(TokenSort::Alpha).unwrap().changed);
        assert!(!settings.set_token_sort(TokenSort::Alpha).unwrap().changed);
    }

    #[test]
    fn writes_leave_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let settings = store(&dir);
        settings.set_token_sort(TokenSort::Balance).unwrap();
        assert!(!std::fs::read_dir(dir.path()).unwrap().flatten()
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn sort_parser_refuses_silent_defaults() {
        assert_eq!(TokenSort::parse(" Alpha "), Some(TokenSort::Alpha));
        assert_eq!(TokenSort::parse("BALANCE"), Some(TokenSort::Balance));
        assert_eq!(TokenSort::parse("value"), None);
    }
}
