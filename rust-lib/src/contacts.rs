//! The address book: names a user gave to addresses that are not theirs.
//!
//! Deliberately NOT in the keystore. The keystore names accounts it holds keys for, and every
//! one of those names is about a key; these are about counterparties, carry no key material,
//! and must not be reachable through a surface whose whole point is that it guards one.
//!
//! Sealed under this module's profile key (`profile.rs`): who a person pays is theirs alone.
//! While every wallet is locked the book is neither read nor written.
//!
//! Addresses are stored EIP-55 checksummed and matched case-insensitively. A user who types
//! the same address in two casings has one contact, not two, and the on-disk form is the one
//! every other surface in this wallet renders.
//!
//! Pure: no module calls, no chain, no keystore. `cargo test --no-default-features` covers it.

use std::path::PathBuf;

use alloy::primitives::Address;
use logos_signer_interface::wallets::DataKey;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::profile::{self, SealedFile, Vault};

/// One entry. `name` may be empty — an address worth remembering is worth remembering before
/// its owner has been given a name, and refusing that would make the Save button conditional
/// on typing something.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contact {
    pub address: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ContactsError {
    /// Not an address. Refused rather than stored: an address book whose entries cannot be
    /// sent to is a list of typos a user will trust exactly once.
    BadAddress(String),
    /// A name longer than anything a row can show. Bounded here rather than truncated in a
    /// view, so every surface agrees on what was stored.
    NameTooLong(usize),
    /// The book is full. A cap so a runaway writer cannot grow the file without limit.
    Full(usize),
    Persist(String),
    /// The file is there and is not an address book. Never folded into an empty list: that
    /// would answer "you have no contacts" for a file we simply could not read, and the next
    /// write would then overwrite it.
    Unreadable(String),
    /// No wallet is unlocked, so the book's key is not here. Nothing was read or written.
    Locked,
}

impl ContactsError {
    /// As a reply, with `code: "locked"` while locked.
    pub fn reply(&self) -> String {
        profile::refusal(self, matches!(self, Self::Locked))
    }
}

impl std::fmt::Display for ContactsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContactsError::BadAddress(a) => write!(f, "'{a}' is not an Ethereum address"),
            ContactsError::NameTooLong(n) => {
                write!(f, "a contact name may be at most {n} characters")
            }
            ContactsError::Full(n) => write!(f, "the address book holds at most {n} contacts"),
            ContactsError::Persist(e) => write!(f, "could not save the address book: {e}"),
            ContactsError::Unreadable(e) => write!(f, "could not read the address book: {e}"),
            ContactsError::Locked => {
                write!(f, "the address book is locked: {}", profile::UNLOCK_HINT)
            }
        }
    }
}

pub const MAX_NAME: usize = 64;
pub const MAX_CONTACTS: usize = 512;

/// EIP-55, or a refusal. The one place an address enters the book, so the one place its form
/// is decided.
pub fn normalise(address: &str) -> Result<String, ContactsError> {
    address
        .trim()
        .parse::<Address>()
        .map(|a| a.to_string())
        .map_err(|_| ContactsError::BadAddress(address.trim().to_string()))
}

pub struct ContactsStore {
    /// The sealed book and its key. Its gate serializes read-modify-write, so two concurrent
    /// saves cannot each write from their own snapshot.
    vault: Vault,
}

impl ContactsStore {
    /// The book's `data_key` scope.
    pub const SCOPE: &'static str = "contacts";

    /// Locked until its key comes.
    pub fn with_path(path: PathBuf) -> Self {
        Self { vault: Vault::new(path) }
    }

    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    /// Ordered by name, then address — a stable order a picker can show without sorting, and
    /// one that does not move when an unrelated contact is added. Unnamed entries sort last,
    /// because a row with a name is the one a user is looking for.
    pub fn list(&self) -> Result<Vec<Contact>, ContactsError> {
        self.vault.with(Self::load).unwrap_or(Err(ContactsError::Locked))
    }

    fn load(file: &SealedFile, key: &DataKey) -> Result<Vec<Contact>, ContactsError> {
        let Some(plain) = file.open(key).map_err(ContactsError::Unreadable)? else {
            return Ok(Vec::new());
        };
        let mut all: Vec<Contact> = serde_json::from_slice(&plain)
            .map_err(|e| ContactsError::Unreadable(e.to_string()))?;
        all.sort_by(|a, b| {
            let key = |c: &Contact| (c.name.is_empty(), c.name.to_lowercase(), c.address.clone());
            key(a).cmp(&key(b))
        });
        Ok(all)
    }

    /// Sealed beside the book and renamed over it: a half-written book would read as
    /// unreadable, and the next save would then be refused.
    fn write(file: &SealedFile, key: &DataKey, all: &[Contact]) -> Result<(), ContactsError> {
        let body = Zeroizing::new(
            serde_json::to_vec(all).map_err(|e| ContactsError::Persist(e.to_string()))?,
        );
        file.seal(key, &body).map_err(ContactsError::Persist)
    }

    /// Add, or rename what is already there. UPSERT rather than refuse-if-present: a user who
    /// saves an address they already have meant to name it, and an error there would send
    /// them to find and edit a row they cannot see from the send form.
    pub fn save(&self, address: &str, name: &str) -> Result<Contact, ContactsError> {
        let address = normalise(address)?;
        let name = name.trim();
        if name.chars().count() > MAX_NAME {
            return Err(ContactsError::NameTooLong(MAX_NAME));
        }
        self.vault.with(|file, key| {
            let mut all = Self::load(file, key)?;
            match all.iter_mut().find(|c| c.address.eq_ignore_ascii_case(&address)) {
                Some(existing) => existing.name = name.to_string(),
                None => {
                    if all.len() >= MAX_CONTACTS {
                        return Err(ContactsError::Full(MAX_CONTACTS));
                    }
                    all.push(Contact { address: address.clone(), name: name.to_string() });
                }
            }
            Self::write(file, key, &all)?;
            Ok(Contact { address: address.clone(), name: name.to_string() })
        })
        .unwrap_or(Err(ContactsError::Locked))
    }

    /// Removing something that is not there SUCCEEDS. The caller's goal is "this address is
    /// not in my book", and that is already true — reporting a failure would only make a
    /// double click look like a fault.
    pub fn remove(&self, address: &str) -> Result<(), ContactsError> {
        let address = normalise(address)?;
        self.vault.with(|file, key| {
            let mut all = Self::load(file, key)?;
            all.retain(|c| !c.address.eq_ignore_ascii_case(&address));
            Self::write(file, key, &all)
        })
        .unwrap_or(Err(ContactsError::Locked))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::tests::{key, manager};
    use crate::profile::refresh;

    const A: &str = "0x8626f6940E2eb28930eFb4CeF49B2d1F2C9C1199";
    const B: &str = "0x0adBc7B2D1A2b7C8E9F0A1b2c3d4e5f60718D3A7";

    fn opened(path: PathBuf, k: &DataKey) -> ContactsStore {
        let s = ContactsStore::with_path(path);
        assert_eq!(s.vault().unlock(k, k).error, None);
        s
    }

    fn store() -> (tempfile::TempDir, ContactsStore) {
        let d = tempfile::tempdir().unwrap();
        let s = opened(d.path().join("contacts.json"), &key(1, 0));
        (d, s)
    }

    #[test]
    fn while_locked_the_book_is_neither_read_nor_written_and_says_locked() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("contacts.json");
        let s = ContactsStore::with_path(path.clone());
        assert_eq!(s.list(), Err(ContactsError::Locked));
        assert_eq!(s.save(A, "Rorschach"), Err(ContactsError::Locked));
        assert_eq!(s.remove(A), Err(ContactsError::Locked));
        assert!(!path.exists(), "nothing was written");

        // Bytes no key opens: an answer other than `Locked` would mean they were read.
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(s.list(), Err(ContactsError::Locked));
        assert_eq!(s.save(A, "Rorschach"), Err(ContactsError::Locked));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");

        let v: serde_json::Value = serde_json::from_str(&ContactsError::Locked.reply()).unwrap();
        assert_eq!((v["ok"].as_bool(), v["code"].as_str()), (Some(false), Some("locked")));
        assert!(v["error"].as_str().unwrap().contains("Signer app"), "{v}");
        let v: serde_json::Value =
            serde_json::from_str(&ContactsError::Full(1).reply()).unwrap();
        assert!(v.get("code").is_none(), "only a lock is a lock: {v}");
    }

    #[test]
    fn a_book_locked_again_forgets_its_key() {
        let (_d, s) = store();
        s.save(A, "Rorschach").unwrap();
        assert!(s.vault().close());
        assert_eq!(s.list(), Err(ContactsError::Locked));
    }

    #[test]
    fn a_reopened_book_reads_only_with_its_own_key() {
        let (d, s) = store();
        s.save(A, "Rorschach").unwrap();
        let path = d.path().join("contacts.json");
        let before = std::fs::read(&path).unwrap();

        let other = opened(path.clone(), &key(2, 0));
        assert!(matches!(other.list(), Err(ContactsError::Unreadable(_))));
        assert!(matches!(other.save(B, "x"), Err(ContactsError::Unreadable(_))));
        assert_eq!(std::fs::read(&path).unwrap(), before, "and the box was left as it was");

        let again = opened(path, &key(1, 0));
        assert_eq!(again.list().unwrap(), vec![Contact { address: A.into(), name: "Rorschach".into() }]);
    }

    #[test]
    fn no_name_or_address_reaches_the_file() {
        let (d, s) = store();
        s.save(A, "Rorschach").unwrap();
        let text = std::fs::read_to_string(d.path().join("contacts.json")).unwrap();
        for leak in ["Rorschach", A, &A.to_lowercase(), &A[2..].to_lowercase(), "address"] {
            assert!(!text.contains(leak), "{leak:?} is in the file: {text}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.path().join("contacts.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_new_generation_seals_the_book_again() {
        let (d, s) = store();
        s.save(A, "Rorschach").unwrap();
        let path = d.path().join("contacts.json");

        let reopened = ContactsStore::with_path(path.clone());
        assert_eq!(reopened.vault().wanted(), Some(0));
        let done = refresh(reopened.vault(), manager(1, 1));
        assert_eq!((done.moved, done.error), (true, None));
        assert_eq!(reopened.vault().wanted(), Some(1), "sealed again under generation 1");
        assert_eq!(reopened.list().unwrap().len(), 1);

        assert!(matches!(opened(path.clone(), &key(1, 0)).list(), Err(ContactsError::Unreadable(_))));
        assert_eq!(opened(path, &key(1, 1)).list().unwrap()[0].name, "Rorschach");
    }

    #[test]
    fn a_plain_book_from_before_sealing_is_ignored() {
        let (d, s) = store();
        let path = d.path().join("contacts.json");
        std::fs::write(&path, format!(r#"[{{"address":"{A}","name":"Plain"}}]"#)).unwrap();
        assert_eq!(s.list().unwrap(), vec![]);
        s.save(B, "Sealed").unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("Plain"));
        assert_eq!(s.list().unwrap().len(), 1);
    }

    #[test]
    fn a_book_that_was_never_written_is_empty_rather_than_an_error() {
        let (_d, s) = store();
        assert_eq!(s.list().unwrap(), vec![]);
    }

    #[test]
    fn an_address_is_stored_checksummed_however_it_was_typed() {
        let (_d, s) = store();
        s.save(&A.to_lowercase(), "Rorschach").unwrap();
        assert_eq!(s.list().unwrap()[0].address, A, "stored form must be EIP-55");
    }

    #[test]
    fn the_same_address_in_another_casing_is_the_same_contact() {
        // Two rows for one counterparty is how a user sends to the wrong "Rorschach".
        let (_d, s) = store();
        s.save(A, "Rorschach").unwrap();
        s.save(&A.to_uppercase().replace("0X", "0x"), "Renamed").unwrap();
        let all = s.list().unwrap();
        assert_eq!(all.len(), 1, "one address, one contact");
        assert_eq!(all[0].name, "Renamed", "saving again renames rather than refusing");
    }

    #[test]
    fn anything_that_is_not_an_address_is_refused_rather_than_stored() {
        // An address book whose entries cannot be sent to is a list of typos.
        let (_d, s) = store();
        for bad in ["", "0x", "not an address", "0x8626f6940E2eb28930eFb4CeF49B2d1F2C9C11"] {
            assert!(matches!(s.save(bad, "x"), Err(ContactsError::BadAddress(_))), "{bad:?}");
        }
        assert_eq!(s.list().unwrap(), vec![], "and nothing was written");
    }

    #[test]
    fn a_contact_may_have_no_name_yet() {
        // Saving an address before naming its owner is the common case from a send form.
        let (_d, s) = store();
        s.save(A, "   ").unwrap();
        assert_eq!(s.list().unwrap()[0].name, "", "whitespace is not a name");
    }

    #[test]
    fn removing_something_absent_succeeds() {
        let (_d, s) = store();
        s.save(A, "Rorschach").unwrap();
        s.remove(B).unwrap();
        s.remove(&A.to_lowercase()).unwrap();
        s.remove(A).unwrap();
        assert_eq!(s.list().unwrap(), vec![], "removal matches on casing too");
    }

    #[test]
    fn named_rows_come_first_and_the_order_does_not_move_under_an_unrelated_write() {
        let (_d, s) = store();
        s.save(B, "").unwrap();
        s.save(A, "Rorschach").unwrap();
        let first = s.list().unwrap();
        assert_eq!(first[0].name, "Rorschach", "a named row is the one being looked for");
        assert_eq!(first[1].name, "");
        s.save("0x1234567890123456789012345678901234567890", "Zebra").unwrap();
        let after = s.list().unwrap();
        assert_eq!(after[0].name, "Rorschach", "an unrelated add did not reorder the top");
        assert_eq!(after[2].name, "", "and the unnamed row is still last");
    }

    #[test]
    fn a_name_longer_than_a_row_can_show_is_refused() {
        let (_d, s) = store();
        let long = "x".repeat(MAX_NAME + 1);
        assert!(matches!(s.save(A, &long), Err(ContactsError::NameTooLong(_))));
        assert!(s.save(A, &"x".repeat(MAX_NAME)).is_ok(), "the bound itself is allowed");
    }

    #[test]
    fn a_file_that_is_not_an_address_book_is_reported_rather_than_emptied() {
        // Answering "no contacts" would be a lie the next write turns into data loss.
        let (d, s) = store();
        std::fs::write(d.path().join("contacts.json"), "{not json").unwrap();
        assert!(matches!(s.list(), Err(ContactsError::Unreadable(_))));
        assert!(matches!(s.save(A, "x"), Err(ContactsError::Unreadable(_))));
    }
}
