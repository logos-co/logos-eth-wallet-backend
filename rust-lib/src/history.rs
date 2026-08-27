//! Local transaction history — the only tx history this wallet has (we never
//! query a proprietary indexer; we record the txs we ourselves broadcast).
//!
//! One JSON file per account address under `<dir>/history/<address>.json`. Pure
//! Rust, unit-tested with `cargo test`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A wallet-originated transaction record.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TxRecord {
    pub hash: String,
    pub chain_id: u64,
    pub from: String,
    pub to: String,
    /// wei (native) or token base units (erc20), as a decimal/hex string.
    pub value: String,
    /// "native" | "erc20".
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// "pending" | "confirmed" | "failed".
    pub status: String,
    pub timestamp: u64,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Per-address history store rooted at a persistence directory.
pub struct History {
    dir: PathBuf,
}

impl History {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, address: &str) -> PathBuf {
        // Normalize the key: strip an optional `0x` and lowercase, so the same
        // account resolves regardless of prefix/case in the caller's address.
        let a = address.trim();
        let a = a.strip_prefix("0x").or_else(|| a.strip_prefix("0X")).unwrap_or(a).to_lowercase();
        self.dir.join("history").join(format!("{a}.json"))
    }

    pub fn list(&self, address: &str) -> Vec<TxRecord> {
        match std::fs::read_to_string(self.path(address)) {
            Ok(txt) => serde_json::from_str(&txt).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn write(&self, address: &str, records: &[TxRecord]) {
        let p = self.path(address);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(txt) = serde_json::to_string_pretty(records) {
            let _ = std::fs::write(p, txt);
        }
    }

    /// Prepend a record (newest first).
    pub fn add(&self, address: &str, record: TxRecord) {
        let mut records = self.list(address);
        records.insert(0, record);
        self.write(address, &records);
    }

    /// Update the status of a recorded tx by hash. Returns true if found.
    pub fn update_status(&self, address: &str, hash: &str, status: &str) -> bool {
        let mut records = self.list(address);
        let mut found = false;
        for r in records.iter_mut() {
            if r.hash.eq_ignore_ascii_case(hash) {
                r.status = status.to_string();
                found = true;
            }
        }
        if found {
            self.write(address, &records);
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(hash: &str) -> TxRecord {
        TxRecord {
            hash: hash.into(),
            chain_id: 1,
            from: "0xaaaa".into(),
            to: "0xbbbb".into(),
            value: "0x1".into(),
            kind: "native".into(),
            token: None,
            status: "pending".into(),
            timestamp: 123,
        }
    }

    #[test]
    fn add_list_update_persist() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::new(dir.path().to_path_buf());
        let addr = "0xF39fd6E51Aad88f6f4CE6Ab8827279cFFfB92266";
        assert!(h.list(addr).is_empty());
        h.add(addr, rec("0xdead"));
        h.add(addr, rec("0xbeef"));
        let list = h.list(addr);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].hash, "0xbeef"); // newest first

        assert!(h.update_status(addr, "0xDEAD", "confirmed")); // case-insensitive
        assert!(!h.update_status(addr, "0xmissing", "confirmed"));

        // reopen — persisted + status updated
        let h2 = History::new(dir.path().to_path_buf());
        let reread = h2.list(addr);
        let dead = reread.iter().find(|r| r.hash == "0xdead").unwrap();
        assert_eq!(dead.status, "confirmed");

        // address key is normalized: stored under 0x-checksummed, found via bare hex
        let bare = "f39fd6e51aad88f6f4ce6ab8827279cfffb92266";
        assert_eq!(h2.list(bare).len(), 2);
    }
}
