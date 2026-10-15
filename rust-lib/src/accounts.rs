//! Adapt the signer manager's inventory to the wallet's existing address/label contracts.
//! Discovery, deduplication and signer precedence remain the manager's responsibility.

use alloy::primitives::Address;
use serde_json::{json, Value};

fn reply(raw: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    if v["ok"] != true {
        return Err(v["error"].as_str().unwrap_or("account inventory unavailable").into());
    }
    Ok(v)
}

fn evm_rows(v: &Value) -> Result<Vec<(Address, Option<&str>)>, String> {
    let rows = v["accounts"].as_array().ok_or("account inventory is missing accounts")?;
    rows.iter()
        .filter(|a| a["family"] == "evm")
        .map(|a| {
            let address = a["address"].as_str().ok_or("EVM account is missing its address")?
                .parse::<Address>().map_err(|_| "invalid EVM account address")?;
            Ok((address, a["label"].as_str().filter(|s| !s.trim().is_empty())))
        })
        .collect()
}

pub fn evm_accounts(signers: &str) -> Result<String, String> {
    let v = reply(signers)?;
    // Availability controls signing, not whether a person can view their balance/history.
    let accounts: Vec<_> = evm_rows(&v)?.into_iter().map(|(a, _)| a.to_checksum(None)).collect();
    Ok(json!({ "ok": true, "accounts": accounts }).to_string())
}

pub fn merge_labels(keystore: &str, signers: &str) -> Result<String, String> {
    let mut labels = reply(keystore)?;
    let out = labels["labels"].as_object_mut().ok_or("keystore reply is missing labels")?;
    let v = reply(signers)?;
    for (address, label) in evm_rows(&v)? {
        if let Some(label) = label {
            // The view expects the keystore's lowercase, prefix-free label keys.
            let key = hex::encode(address.as_slice());
            let named = out.iter().any(|(k, v)| {
                k.trim_start_matches("0x").eq_ignore_ascii_case(&key)
                    && v.as_str().is_some_and(|s| !s.trim().is_empty())
            });
            if !named {
                out.insert(key, json!(label));
            }
        }
    }
    Ok(labels.to_string())
}

pub fn merge_wallets(keystore: &str, signers: &str) -> Result<String, String> {
    // Wallet names are optional; an unavailable/corrupt names reply must not discard
    // the authoritative provider origins or retain another provider's old metadata.
    let mut wallets = reply(keystore).unwrap_or_else(|_| json!({ "ok": true, "wallets": {} }));
    if !wallets["wallets"].is_object() { wallets["wallets"] = json!({}); }
    let out = wallets["wallets"].as_object_mut().unwrap();
    let v = reply(signers)?;
    evm_rows(&v)?; // Validate before publishing any rows.
    for row in v["accounts"].as_array().unwrap().iter().filter(|r| r["family"] == "evm") {
        let address = row["address"].as_str().unwrap().to_lowercase();
        let key = out.keys().find(|k| k.eq_ignore_ascii_case(&address)).cloned().unwrap_or(address);
        let entry = out.entry(key).or_insert_with(|| json!({}));
        if !entry.is_object() { *entry = json!({}); }
        entry["signer"] = row["signer"].clone();
        entry["origin"] = row["origin"].clone();
        entry["available"] = row["available"].clone();
    }
    Ok(wallets.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOFTWARE: &str = "0x0000000000000000000000000000000000000001";
    const CARD: &str = "0x52908400098527886e0f7030069857d2e4169ee7";

    fn inventory(available: bool) -> String {
        json!({"ok": true, "accounts": [
            {"id": SOFTWARE, "family": "evm", "address": SOFTWARE,
             "signer": "keystore_module", "label": "Software", "available": true},
            {"id": CARD, "family": "evm", "address": CARD,
             "signer": "keycard_module", "label": "Keycard EVM", "available": available},
            {"id": "btc:test:keycard", "family": "bitcoin", "address": null,
             "signer": "keycard_module", "label": "Bitcoin", "available": true}
        ]}).to_string()
    }

    #[test]
    fn origins_survive_without_a_wallet_label_or_connected_device() {
        let mut inv: Value = serde_json::from_str(&inventory(false)).unwrap();
        let origin = json!({"devices":[{"kind":"keycard","id":"card1","name":"Travel"}], "phrase":{"id":"seed1"}});
        inv["accounts"][1]["origin"] = origin.clone();
        let v: Value = serde_json::from_str(&merge_wallets(r#"{"ok":true,"wallets":{}}"#, &inv.to_string()).unwrap()).unwrap();
        assert_eq!(v["wallets"][CARD]["origin"], origin);
        assert_eq!(v["wallets"][CARD]["signer"], "keycard_module");
        assert_eq!(v["wallets"][CARD]["available"], false);
        let unavailable: Value = serde_json::from_str(&merge_wallets(r#"{"ok":false,"error":"offline"}"#, &inv.to_string()).unwrap()).unwrap();
        assert_eq!(unavailable["wallets"][CARD]["origin"], origin);
    }

    #[test]
    fn software_and_keycard_evm_accounts_are_visible_but_bitcoin_is_not() {
        let v: Value = serde_json::from_str(&evm_accounts(&inventory(true)).unwrap()).unwrap();
        assert_eq!(v["accounts"], json!([SOFTWARE, CARD.parse::<Address>().unwrap().to_checksum(None)]));
    }

    #[test]
    fn unplugging_does_not_hide_the_account_and_withdrawing_does() {
        assert_eq!(evm_accounts(&inventory(true)), evm_accounts(&inventory(false)));
        let mut v: Value = serde_json::from_str(&inventory(true)).unwrap();
        v["accounts"].as_array_mut().unwrap().remove(1);
        let listed: Value = serde_json::from_str(&evm_accounts(&v.to_string()).unwrap()).unwrap();
        assert_eq!(listed["accounts"], json!([SOFTWARE]));
    }

    #[test]
    fn labels_include_the_card_and_preserve_the_keystore_name() {
        let ks = json!({"ok": true, "labels": {"0000000000000000000000000000000000000001": "Savings"}});
        let v: Value = serde_json::from_str(&merge_labels(&ks.to_string(), &inventory(true)).unwrap()).unwrap();
        assert_eq!(v["labels"][&SOFTWARE[2..]], "Savings");
        assert_eq!(v["labels"][&CARD[2..]], "Keycard EVM");
        assert_eq!(v["labels"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn failed_or_malformed_inventories_do_not_become_empty_successes() {
        for raw in [r#"{"ok":false,"error":"offline"}"#, "garbage", r#"{"ok":true}"#,
                    r#"{"ok":true,"accounts":[{"family":"evm","address":"bad"}]}"#] {
            assert!(evm_accounts(raw).is_err());
            assert!(merge_labels(r#"{"ok":true,"labels":{}}"#, raw).is_err());
        }
        assert!(merge_labels(r#"{"ok":false,"error":"offline"}"#, &inventory(true)).is_err());
        assert_eq!(evm_accounts(r#"{"ok":true,"accounts":[]}"#).unwrap(), r#"{"accounts":[],"ok":true}"#);
    }
}
