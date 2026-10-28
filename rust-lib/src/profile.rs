//! This module's own data, sealed under its profile keys: `data_key({ seed: "profile", scope })`
//! from the signer manager, which answers while any wallet is unlocked. One box per file, 0600,
//! named by the file in its associated data. Pure: the glue asks for the keys.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use logos_signer_interface::wallets::{self, DataKey};
use logos_signer_interface::{ErrorCode, SignerError};
use serde_json::{json, Value};
use zeroize::Zeroizing;

use crate::store;

/// What a locked store tells the person to do.
pub const UNLOCK_HINT: &str = "unlock a wallet in the Signer app";

/// A store's refusal as a reply; a locked one carries `code: "locked"` for the view to key on.
pub fn refusal(e: &impl std::fmt::Display, locked: bool) -> String {
    let mut v = json!({ "ok": false, "error": e.to_string() });
    if locked {
        v["code"] = json!("locked");
    }
    v.to_string()
}

/// One box in one file.
pub struct SealedFile {
    path: PathBuf,
    /// The file's name, so a box moved into another file doesn't open there.
    aad: String,
}

impl SealedFile {
    pub fn new(path: PathBuf) -> Self {
        let aad = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Self { path, aad }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The box on disk. None without a file, or for a plain one from before sealing: ignored.
    fn boxed(&self) -> Result<Option<String>, String> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        if raw.trim().is_empty() {
            return Ok(None);
        }
        let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        Ok(v.get("box").is_some().then_some(raw))
    }

    /// The generation the box was sealed at: the key to ask for before opening it.
    pub fn wanted(&self) -> Option<u32> {
        self.boxed().ok().flatten().and_then(|b| wallets::generation(&b).ok())
    }

    /// What the box holds, opened with `key`; None when there is no box.
    pub fn open(&self, key: &DataKey) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
        match self.boxed()? {
            Some(b) => wallets::open(key, &b, &self.aad).map(Some).map_err(|e| e.message),
            None => Ok(None),
        }
    }

    /// Seal `plain` under `key`, staged beside the file and renamed over it.
    pub fn seal(&self, key: &DataKey, plain: &[u8]) -> Result<(), String> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let sealed = wallets::seal(key, plain, &self.aad).map_err(|e| e.message)?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = self.path.with_extension(format!(
            "{}.{}.tmp", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        if store::write_private_then_rename(&tmp, &self.path, sealed.as_bytes()) {
            Ok(())
        } else {
            Err(format!("could not replace {}", self.path.display()))
        }
    }
}

/// A sealed file and, while unlocked, its key. The key's gate also serializes a store's
/// read-modify-write, so two saves cannot each write from their own snapshot.
pub struct Vault {
    file: SealedFile,
    key: Mutex<Option<DataKey>>,
}

/// What a `refresh` did: whether the vault's key moved, and what kept it from opening.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Refreshed {
    pub moved: bool,
    pub error: Option<String>,
}

impl Vault {
    /// Locked until its key comes.
    pub fn new(path: PathBuf) -> Self {
        Self { file: SealedFile::new(path), key: Mutex::new(None) }
    }

    /// `f` with the file and its key, under the gate. None while locked: nothing is read or written.
    pub fn with<T>(&self, f: impl FnOnce(&SealedFile, &DataKey) -> T) -> Option<T> {
        let held = self.key.lock().unwrap_or_else(|e| e.into_inner());
        held.as_ref().map(|key| f(&self.file, key))
    }

    pub fn is_locked(&self) -> bool {
        self.key.lock().unwrap_or_else(|e| e.into_inner()).is_none()
    }

    pub fn wanted(&self) -> Option<u32> {
        self.file.wanted()
    }

    /// Take `current`, first sealing the box again under it when `box_key` is another
    /// generation's. Taken even when the box doesn't open, so reads say unreadable, not locked.
    pub fn unlock(&self, box_key: &DataKey, current: &DataKey) -> Refreshed {
        let mut held = self.key.lock().unwrap_or_else(|e| e.into_inner());
        let error = if box_key.generation == current.generation {
            None
        } else {
            match self.file.open(box_key) {
                Ok(Some(plain)) => self.file.seal(current, &plain).err(),
                Ok(None) => None,
                Err(e) => Some(e),
            }
        };
        let moved = held.as_ref() != Some(current);
        *held = Some(current.clone());
        Refreshed { moved, error }
    }

    /// Drop the key. True when there was one.
    pub fn close(&self) -> bool {
        self.key.lock().unwrap_or_else(|e| e.into_inner()).take().is_some()
    }
}

/// Ask for the current key, and for the box's own generation when that is another, then unlock.
/// A `locked` answer closes the vault; any other refusal leaves it as it was.
pub fn refresh(
    vault: &Vault,
    mut ask: impl FnMut(Option<u32>) -> Result<DataKey, SignerError>,
) -> Refreshed {
    let current = match ask(None) {
        Ok(key) => key,
        Err(e) if e.code == ErrorCode::Locked => return Refreshed { moved: vault.close(), error: None },
        Err(e) => return Refreshed { moved: false, error: Some(e.message) },
    };
    let box_key = match vault.wanted() {
        Some(g) if g != current.generation => match ask(Some(g)) {
            Ok(key) => key,
            Err(e) => {
                return Refreshed { moved: false, error: Some(format!("generation {g}: {}", e.message)) }
            }
        },
        _ => current.clone(),
    };
    vault.unlock(&box_key, &current)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use logos_signer_interface::Secret;

    pub(crate) fn key(byte: u8, generation: u32) -> DataKey {
        DataKey { key: Secret::new(hex::encode([byte; 32])), generation }
    }

    /// The manager for one key byte: `current` by default, any earlier generation on request.
    pub(crate) fn manager(byte: u8, current: u32) -> impl FnMut(Option<u32>) -> Result<DataKey, SignerError> {
        move |g| Ok(key(byte, g.unwrap_or(current)))
    }

    fn locked(_: Option<u32>) -> Result<DataKey, SignerError> {
        Err(SignerError::new(ErrorCode::Locked, "every wallet is locked"))
    }

    #[test]
    fn a_locked_answer_closes_the_vault_and_another_refusal_leaves_it_open() {
        let dir = tempfile::tempdir().unwrap();
        let v = Vault::new(dir.path().join("x.json"));
        assert_eq!(refresh(&v, locked), Refreshed::default(), "closed already: nothing moved");
        assert!(refresh(&v, manager(1, 0)).moved);
        assert!(!refresh(&v, manager(1, 0)).moved, "the same key again moves nothing");
        let down = refresh(&v, |_| Err(SignerError::new(ErrorCode::Unavailable, "no answer")));
        assert_eq!((down.moved, v.is_locked()), (false, false), "a dead manager is not a lock");
        assert!(refresh(&v, locked).moved);
        assert!(v.is_locked());
    }

    #[test]
    fn a_box_names_its_file_and_lands_0600() {
        let dir = tempfile::tempdir().unwrap();
        let a = SealedFile::new(dir.path().join("settings.json"));
        a.seal(&key(1, 0), b"{}").unwrap();
        let b = SealedFile::new(dir.path().join("contacts.json"));
        std::fs::copy(a.path(), b.path()).unwrap();
        assert!(b.open(&key(1, 0)).is_err(), "a settings box doesn't open as the address book");
        assert_eq!(a.open(&key(1, 0)).unwrap().unwrap().as_slice(), b"{}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(a.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(!std::fs::read_dir(dir.path()).unwrap().flatten()
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn an_older_generation_is_asked_for_only_when_the_box_was_sealed_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let v = Vault::new(dir.path().join("x.json"));
        let mut asked = Vec::new();
        refresh(&v, |g| { asked.push(g); Ok(key(1, 2)) });
        assert_eq!(asked, vec![None], "no box: the current key alone");
        v.with(|file, k| file.seal(k, b"x")).unwrap().unwrap();
        v.close();
        let mut asked = Vec::new();
        let done = refresh(&v, |g| { asked.push(g); Ok(key(1, g.unwrap_or(3))) });
        assert_eq!((asked, done.error), (vec![None, Some(2)], None));
        assert_eq!(v.wanted(), Some(3), "sealed again under the current generation");
    }
}
