//! The wallet's one persisted view preference. Chain facts and scope live in
//! `eth_rpc_module`; token membership and snapshots live in `token_list_module`.
//!
//! Sealed under this module's profile key (`profile.rs`). While every wallet is locked nothing
//! is read or written, and views are shown the defaults.

use std::path::PathBuf;

use logos_signer_interface::wallets::DataKey;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::profile::{self, SealedFile, Vault};
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
    vault: Vault,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SettingsError {
    Persist(String),
    Unreadable(String),
    /// No wallet is unlocked, so the settings' key is not here. Nothing was read or written.
    Locked,
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Persist(error) => write!(f, "could not save settings: {error}"),
            Self::Unreadable(error) => write!(f, "could not read settings: {error}"),
            Self::Locked => write!(f, "settings are locked: {}", profile::UNLOCK_HINT),
        }
    }
}

impl SettingsError {
    /// As a reply, with `code: "locked"` while locked.
    pub fn reply(&self) -> String {
        profile::refusal(self, matches!(self, Self::Locked))
    }
}

impl SettingsStore {
    /// The settings' `data_key` scope.
    pub const SCOPE: &'static str = "settings";

    /// Locked until its key comes.
    pub fn with_path(path: PathBuf) -> Self { Self { vault: Vault::new(path) } }

    pub fn vault(&self) -> &Vault { &self.vault }

    pub fn try_load(&self) -> Result<Settings, SettingsError> {
        self.vault.with(Self::read).unwrap_or(Err(SettingsError::Locked))
    }

    /// What a view is shown: the stored settings, or the defaults while locked, read from nowhere.
    pub fn shown(&self) -> Result<Settings, SettingsError> {
        match self.try_load() {
            Err(SettingsError::Locked) => Ok(Settings::default()),
            other => other,
        }
    }

    fn read(file: &SealedFile, key: &DataKey) -> Result<Settings, SettingsError> {
        let Some(plain) = file.open(key).map_err(SettingsError::Unreadable)? else {
            return Ok(Settings::default());
        };
        let value: serde_json::Value = serde_json::from_slice(&plain)
            .map_err(|error| SettingsError::Unreadable(error.to_string()))?;
        if !value.is_object() {
            return Err(SettingsError::Unreadable("settings must be a JSON object".into()));
        }
        serde_json::from_value(value).map_err(|error| SettingsError::Unreadable(error.to_string()))
    }

    fn quarantine(file: &SealedFile) -> bool {
        let aside = file.path().with_extension(format!("json.unreadable-{}", store::now_secs()));
        std::fs::rename(file.path(), aside).is_ok()
    }

    fn save(file: &SealedFile, key: &DataKey, settings: &Settings) -> Result<(), SettingsError> {
        let text = Zeroizing::new(
            serde_json::to_vec(settings).map_err(|e| SettingsError::Persist(e.to_string()))?,
        );
        file.seal(key, &text).map_err(SettingsError::Persist)
    }

    fn update(&self, edit: impl FnOnce(&mut Settings)) -> Result<Applied, SettingsError> {
        self.vault.with(|file, key| {
            let (mut settings, readable) = match Self::read(file, key) {
                Ok(settings) => (settings, true),
                Err(SettingsError::Unreadable(_)) if Self::quarantine(file) => (Settings::default(), false),
                Err(error) => return Err(error),
            };
            let before = settings.clone();
            edit(&mut settings);
            Self::save(file, key, &settings)?;
            Ok(Applied { changed: !readable || settings != before, settings })
        })
        .unwrap_or(Err(SettingsError::Locked))
    }

    pub fn set_token_sort(&self, order: TokenSort) -> Result<Applied, SettingsError> {
        self.update(|settings| settings.token_sort = order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::tests::{key, manager};
    use crate::profile::refresh;

    fn opened(path: PathBuf, k: &DataKey) -> SettingsStore {
        let s = SettingsStore::with_path(path);
        assert_eq!(s.vault().unlock(k, k).error, None);
        s
    }

    fn store(dir: &tempfile::TempDir) -> SettingsStore {
        opened(dir.path().join("settings.json"), &key(1, 0))
    }

    fn sealed(path: &std::path::Path, k: &DataKey, text: &str) {
        SealedFile::new(path.to_path_buf()).seal(k, text.as_bytes()).unwrap();
    }

    #[test]
    fn defaults_are_alpha() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(store(&dir).try_load().unwrap(), Settings::default());
    }

    #[test]
    fn while_locked_nothing_is_read_or_written_and_views_get_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let locked = SettingsStore::with_path(path.clone());
        assert_eq!(locked.try_load(), Err(SettingsError::Locked));
        assert_eq!(locked.set_token_sort(TokenSort::Balance).unwrap_err(), SettingsError::Locked);
        assert!(!path.exists(), "nothing was written");

        sealed(&path, &key(1, 0), r#"{"tokenSort":"balance"}"#);
        let before = std::fs::read(&path).unwrap();
        assert_eq!(locked.shown().unwrap().token_sort, TokenSort::Alpha, "the stored order was not read");
        assert_eq!(locked.set_token_sort(TokenSort::Alpha).unwrap_err(), SettingsError::Locked);
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let v: serde_json::Value = serde_json::from_str(&SettingsError::Locked.reply()).unwrap();
        assert_eq!(v["code"], "locked");
        assert!(v["error"].as_str().unwrap().contains("Signer app"), "{v}");
    }

    #[test]
    fn old_network_and_enabled_token_copies_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        sealed(&path, &key(1, 0), r#"{"activeChainId":11155111,"tokenSort":"balance","networks":[{"chainId":1,"enabledTokens":[{"symbol":"OLD"}]}]}"#);
        let settings = opened(path, &key(1, 0)).try_load().unwrap();
        assert_eq!(settings.token_sort, TokenSort::Balance);
    }

    #[test]
    fn preference_round_trips_and_opens_only_with_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        store(&dir).set_token_sort(TokenSort::Balance).unwrap();
        assert_eq!(opened(path.clone(), &key(1, 0)).try_load().unwrap().token_sort, TokenSort::Balance);
        let other = opened(path.clone(), &key(2, 0));
        assert!(matches!(other.try_load(), Err(SettingsError::Unreadable(_))));
        assert!(matches!(other.shown(), Err(SettingsError::Unreadable(_))), "unreadable is not locked");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("balance") && !text.contains("tokenSort"), "{text}");
    }

    #[test]
    fn a_new_generation_seals_the_settings_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        store(&dir).set_token_sort(TokenSort::Balance).unwrap();
        let reopened = SettingsStore::with_path(path.clone());
        assert_eq!(refresh(reopened.vault(), manager(1, 4)).error, None);
        assert_eq!(reopened.vault().wanted(), Some(4));
        assert_eq!(opened(path, &key(1, 4)).try_load().unwrap().token_sort, TokenSort::Balance);
    }

    #[test]
    fn unreadable_bytes_are_an_error_and_plain_files_from_before_sealing_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let settings = store(&dir);
        for bytes in ["{", "not json"] {
            std::fs::write(&path, bytes).unwrap();
            assert!(matches!(settings.try_load(), Err(SettingsError::Unreadable(_))), "{bytes:?}");
        }
        sealed(&path, &key(1, 0), "[]");
        assert!(matches!(settings.try_load(), Err(SettingsError::Unreadable(_))), "a box of an array");
        for plain in ["", "[]", r#"{"tokenSort":"balance"}"#] {
            std::fs::write(&path, plain).unwrap();
            assert_eq!(settings.try_load().unwrap(), Settings::default(), "{plain:?}");
        }
    }

    #[test]
    fn a_write_repairs_an_unreadable_file_by_moving_it_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{").unwrap();
        let applied = store(&dir).set_token_sort(TokenSort::Balance).unwrap();
        assert!(applied.changed);
        assert_eq!(store(&dir).try_load().unwrap().token_sort, TokenSort::Balance);
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
