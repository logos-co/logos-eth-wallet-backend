//! The address book: names a user gave to addresses that are not theirs.
//!
//! Deliberately NOT in the keystore. The keystore names accounts it holds keys for, and every
//! one of those names is about a key; these are about counterparties, carry no key material,
//! and must not be reachable through a surface whose whole point is that it guards one. A
//! contact is also not a secret — losing the file costs a nickname, not an asset.
//!
//! Addresses are stored EIP-55 checksummed and matched case-insensitively. A user who types
//! the same address in two casings has one contact, not two, and the on-disk form is the one
//! every other surface in this wallet renders.
//!
//! Pure: no module calls, no chain, no keystore. `cargo test --no-default-features` covers it.

use std::path::PathBuf;
use std::sync::Mutex;

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};

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
    path: PathBuf,
    /// Serializes read-modify-write, so two concurrent saves cannot each write from their own
    /// snapshot. The same shape `SettingsStore` and `History` use.
    gate: Mutex<()>,
}

impl ContactsStore {
    pub fn with_path(path: PathBuf) -> Self {
        Self { path, gate: Mutex::new(()) }
    }

    /// Ordered by name, then address — a stable order a picker can show without sorting, and
    /// one that does not move when an unrelated contact is added. Unnamed entries sort last,
    /// because a row with a name is the one a user is looking for.
    pub fn list(&self) -> Result<Vec<Contact>, ContactsError> {
        let _g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.load()
    }

    fn load(&self) -> Result<Vec<Contact>, ContactsError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(ContactsError::Unreadable(e.to_string())),
        };
        if raw.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut all: Vec<Contact> = serde_json::from_str(&raw)
            .map_err(|e| ContactsError::Unreadable(e.to_string()))?;
        all.sort_by(|a, b| {
            let key = |c: &Contact| (c.name.is_empty(), c.name.to_lowercase(), c.address.clone());
            key(a).cmp(&key(b))
        });
        Ok(all)
    }

    fn write(&self, all: &[Contact]) -> Result<(), ContactsError> {
        let body = serde_json::to_string_pretty(all)
            .map_err(|e| ContactsError::Persist(e.to_string()))?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| ContactsError::Persist(e.to_string()))?;
        }
        // Write beside and rename: a half-written book is a book that reads as unreadable,
        // and the next save would then be refused against a file we could not parse.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(|e| ContactsError::Persist(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| ContactsError::Persist(e.to_string()))
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
        let _g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load()?;
        match all.iter_mut().find(|c| c.address.eq_ignore_ascii_case(&address)) {
            Some(existing) => existing.name = name.to_string(),
            None => {
                if all.len() >= MAX_CONTACTS {
                    return Err(ContactsError::Full(MAX_CONTACTS));
                }
                all.push(Contact { address: address.clone(), name: name.to_string() });
            }
        }
        self.write(&all)?;
        Ok(Contact { address, name: name.to_string() })
    }

    /// Removing something that is not there SUCCEEDS. The caller's goal is "this address is
    /// not in my book", and that is already true — reporting a failure would only make a
    /// double click look like a fault.
    pub fn remove(&self, address: &str) -> Result<(), ContactsError> {
        let address = normalise(address)?;
        let _g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load()?;
        all.retain(|c| !c.address.eq_ignore_ascii_case(&address));
        self.write(&all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0x8626f6940E2eb28930eFb4CeF49B2d1F2C9C1199";
    const B: &str = "0x0adBc7B2D1A2b7C8E9F0A1b2c3d4e5f60718D3A7";

    fn store() -> (tempfile::TempDir, ContactsStore) {
        let d = tempfile::tempdir().unwrap();
        let s = ContactsStore::with_path(d.path().join("contacts.json"));
        (d, s)
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
