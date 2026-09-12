//! What this wallet adds to a history row `tx_sender_module` hands back.
//!
//! The sender records what it broadcast and decorates the ether: value, fee, gas prices,
//! EIP-55 addresses, raw ERC-20 `Transfer` logs. It holds no token table, so a token amount
//! is the consumer's to render — and this wallet stored, beside each of its own sends, the
//! `meta` that says which token and how much. Decoration happens here, at READ time, from the
//! same offered set the balance list and the send path use, so a transfer in an enabled token
//! decodes on every screen or on none.
//!
//! Pure: `cargo test --no-default-features` covers it.

use alloy::primitives::Address;
use serde_json::{json, Value};

use crate::tokens::{self, Token};
use crate::units;

/// EIP-55 for an address that reaches us in whatever casing the node used. Anything
/// unparseable comes back untouched: we do not know what it is, and reshaping it is a guess.
pub fn checksummed(addr: &str) -> String {
    addr.parse::<Address>().map(|a| a.to_string()).unwrap_or_else(|_| addr.to_string())
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// One row, in place. A row this wallet sent as an ERC-20 transfer carries
/// `meta.kind == "erc20"`: it is re-read as the transfer it was — the recipient the user
/// typed in `to`, the amount in the token's own units — while the transaction's own target
/// stays in `txTo`. A native row, or a call another app made, is left as the sender rendered
/// it. Transfers decoded off the receipt are named from the offered set.
pub fn decorate_row(row: &mut Value, chain_id: u64, enabled: &[Token]) {
    let meta = row.get("meta").cloned().unwrap_or(Value::Null);
    if str_of(&meta, "kind") == Some("erc20") {
        row["kind"] = json!("erc20");
        if let Some(t) = str_of(&meta, "token") {
            row["token"] = json!(checksummed(t));
        }
        if let Some(r) = str_of(&meta, "recipient") {
            row["to"] = json!(checksummed(r));
        }
        let decimals = meta.get("tokenDecimals").and_then(Value::as_u64).map(|d| d as u8);
        let symbol = str_of(&meta, "tokenSymbol").map(str::to_string);
        if let Some(s) = &symbol {
            row["tokenSymbol"] = json!(s);
            row["valueSymbol"] = json!(s);
        }
        // The stored `value` is the ether the call carried (none); the amount that moved is
        // the token's, and it renders at the token's decimals or not at all.
        if let Some(a) = str_of(&meta, "amount") {
            row["value"] = json!(a);
            for k in ["valueDisplay", "valueExact", "valueDecimals"] {
                row.as_object_mut().map(|o| o.remove(k));
            }
            if let Some(d) = decimals {
                row["tokenDecimals"] = json!(d);
                row["valueDecimals"] = json!(d);
                units::decorate(row, "value", a, Some(d));
            } else {
                row.as_object_mut().map(|o| o.remove("valueSymbol"));
            }
        }
        // A token amount and a wei fee do not add up, so an ERC-20 row has no total.
        for k in ["totalWei", "totalWeiDisplay", "totalWeiExact"] {
            row.as_object_mut().map(|o| o.remove(k));
        }
    }

    // Two different facts, and the view labels them apart: `to` is the recipient the user
    // meant, `txTo` the transaction's own target. Recomputed here because `to` may have just
    // moved. Present exactly when `txTo` is, so absent-because-same stays distinct from
    // absent-because-unread.
    if let Some(tx_to) = str_of(row, "txTo").map(str::to_string) {
        let to = str_of(row, "to").unwrap_or_default().to_string();
        row["interactedWithDiffers"] = json!(!tx_to.eq_ignore_ascii_case(&to));
        if let Some(t) = tokens::by_address(chain_id, &tx_to, enabled) {
            row["interactedWithSymbol"] = json!(t.symbol);
        }
    }

    if let Some(transfers) = row.get_mut("transfers").and_then(Value::as_array_mut) {
        for t in transfers.iter_mut() {
            let contract = str_of(t, "contract").unwrap_or_default().to_string();
            let tok = tokens::by_address(chain_id, &contract, enabled);
            t["known"] = json!(tok.is_some());
            if let Some(tok) = tok {
                t["symbol"] = json!(tok.symbol);
                t["decimals"] = json!(tok.decimals);
                if let Some(a) = str_of(t, "amount").map(str::to_string) {
                    units::decorate(t, "amount", &a, Some(tok.decimals));
                }
            }
        }
    }
}

/// The sender's `history` reply, as this wallet's `get_history` answers it: every row
/// decorated, and `strandedNonces` flattened to the numbers on THIS chain.
pub fn decorate_history(v: &mut Value, chain_id: u64, enabled: &[Token]) {
    if let Some(rows) = v.get_mut("transactions").and_then(Value::as_array_mut) {
        for row in rows.iter_mut() {
            decorate_row(row, chain_id, enabled);
        }
    }
    let stranded: Vec<Value> = v
        .get("strandedNonces")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|e| e.get("chainId").and_then(Value::as_u64) == Some(chain_id))
                .filter_map(|e| e.get("nonce").cloned())
                .collect()
        })
        .unwrap_or_default();
    v["strandedNonces"] = json!(stranded);
}

#[cfg(test)]
mod tests {
    use super::*;

    const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
    const ME: &str = "0x8626f6940E2eb28930eFb4CeF49B2d1F2C9C1199";
    const THEM: &str = "0x0adBc7B2D1A2b7C8E9F0A1b2c3d4e5f60718D3A7";

    fn weth() -> Token {
        Token { symbol: "WETH".into(), name: "Wrapped Ether".into(), decimals: 18,
                address: Some(WETH.into()), native: false }
    }

    /// An ERC-20 send as the sender renders it: a call to the token contract carrying no
    /// ether, with this wallet's meta beside it and one Transfer log off the receipt.
    fn erc20_row() -> Value {
        json!({
            "hash": "0x9a3c", "chainId": 1, "from": ME, "to": WETH, "txTo": WETH.to_lowercase(),
            "value": "0", "valueSymbol": "ETH", "valueDecimals": 18, "valueDisplay": "0",
            "valueExact": "0", "nativeSymbol": "ETH", "kind": "call", "status": "confirmed",
            "feeWei": "54663000000000", "totalWei": "54663000000000", "totalWeiExact": "0.000054663",
            "interactedWithDiffers": false,
            "meta": { "kind": "erc20", "token": WETH.to_lowercase(), "recipient": THEM.to_lowercase(),
                      "amount": "1000000000000", "tokenSymbol": "WETH", "tokenDecimals": 18 },
            "transfers": [{ "contract": WETH, "from": ME, "to": THEM, "amount": "1000000000000",
                            "mine": true }]
        })
    }

    #[test]
    fn an_erc20_send_is_read_back_as_the_transfer_it_was() {
        let mut r = erc20_row();
        decorate_row(&mut r, 1, &[weth()]);
        assert_eq!(r["kind"], json!("erc20"));
        assert_eq!(r["to"], json!(THEM), "the recipient the user typed, checksummed");
        assert_eq!(r["txTo"], json!(WETH.to_lowercase()), "the target the sender stored is untouched");
        assert_eq!(r["token"], json!(WETH));
        assert_eq!(r["tokenSymbol"], json!("WETH"));
        assert_eq!(r["tokenDecimals"], json!(18));
        assert_eq!(r["value"], json!("1000000000000"), "the token amount, not the ether");
        assert_eq!(r["valueSymbol"], json!("WETH"));
        assert_eq!(r["valueDecimals"], json!(18));
        assert_eq!(r["valueDisplay"], json!("<0.00001"));
        assert_eq!(r["valueExact"], json!("0.000001"));
        assert!(r.get("totalWei").is_none() && r.get("totalWeiExact").is_none(),
                "a token amount and a wei fee do not add up");
        assert_eq!(r["feeWei"], json!("54663000000000"), "the fee is still ether");
        assert_eq!(r["interactedWithDiffers"], json!(true), "recomputed against the recipient");
        assert_eq!(r["interactedWithSymbol"], json!("WETH"));
        let t = &r["transfers"][0];
        assert_eq!((t["known"].clone(), t["symbol"].clone(), t["decimals"].clone()),
                   (json!(true), json!("WETH"), json!(18)));
        assert_eq!(t["amountExact"], json!("0.000001"));
    }

    /// A meta recorded without decimals cannot scale the amount, so it claims no figure.
    #[test]
    fn an_erc20_row_without_decimals_shows_no_amount_at_all() {
        let mut r = erc20_row();
        r["meta"].as_object_mut().unwrap().remove("tokenDecimals");
        decorate_row(&mut r, 1, &[]);
        assert_eq!(r["value"], json!("1000000000000"));
        for k in ["valueDisplay", "valueExact", "valueDecimals", "valueSymbol"] {
            assert!(r.get(k).is_none(), "{k} cannot be known");
        }
    }

    /// A native row is the sender's own rendering, left alone.
    #[test]
    fn a_native_row_is_left_as_the_sender_rendered_it() {
        let mut r = json!({ "kind": "native", "to": THEM, "txTo": THEM.to_lowercase(),
                            "value": "1500000000000000000", "valueDisplay": "1.5",
                            "valueSymbol": "ETH", "totalWeiExact": "1.500021",
                            "meta": { "kind": "native", "recipient": THEM } });
        let before = r.clone();
        decorate_row(&mut r, 1, &[weth()]);
        assert_eq!(r["valueDisplay"], before["valueDisplay"]);
        assert_eq!(r["totalWeiExact"], before["totalWeiExact"]);
        assert_eq!(r["interactedWithDiffers"], json!(false));
        assert!(r.get("interactedWithSymbol").is_none());
    }

    /// A call another app made carries no meta this wallet wrote; it stays a call, and its
    /// transfers are still named from the offered set.
    #[test]
    fn another_apps_call_stays_a_call_and_still_names_the_tokens_it_moved() {
        let mut r = json!({ "kind": "call", "origin": "uniswap_ui", "label": "Swap",
                            "to": "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45",
                            "txTo": "0x68b3465833fb72a70ecdf485e0e4c7bd8665fc45",
                            "value": "0", "meta": { "kind": "swap" },
                            "transfers": [{ "contract": WETH, "from": ME, "to": THEM, "amount": "5" },
                                          { "contract": "0x1234567890123456789012345678901234567890",
                                            "from": THEM, "to": ME, "amount": "42000000" }] });
        decorate_row(&mut r, 1, &[weth()]);
        assert_eq!(r["kind"], json!("call"));
        assert_eq!(r["label"], json!("Swap"));
        assert_eq!(r["interactedWithDiffers"], json!(false));
        assert_eq!(r["transfers"][0]["known"], json!(true));
        assert_eq!(r["transfers"][0]["symbol"], json!("WETH"));
        assert_eq!(r["transfers"][1]["known"], json!(false));
        assert!(r["transfers"][1].get("symbol").is_none(), "an unknown contract gets no symbol");
        assert!(r["transfers"][1].get("amountExact").is_none(), "and no scaled amount");
    }

    /// A row whose receipt predates these fields claims nothing about where it went.
    #[test]
    fn a_row_with_no_tx_to_claims_nothing_about_either() {
        let mut r = json!({ "kind": "native", "to": THEM, "value": "1" });
        decorate_row(&mut r, 1, &[]);
        assert!(r.get("interactedWithDiffers").is_none(), "not `false`, which is a claim");
    }

    #[test]
    fn the_history_reply_is_decorated_whole_and_stranded_numbers_are_this_chains() {
        let mut v = json!({ "ok": true, "chainId": 1, "transactions": [erc20_row()],
                            "strandedNonces": [{ "chainId": 1, "nonce": 5 },
                                               { "chainId": 11155111, "nonce": 9 }] });
        decorate_history(&mut v, 1, &[weth()]);
        assert_eq!(v["transactions"][0]["kind"], json!("erc20"));
        assert_eq!(v["strandedNonces"], json!([5]), "flattened, and another chain's dropped");

        let mut none = json!({ "ok": true, "transactions": [] });
        decorate_history(&mut none, 1, &[]);
        assert_eq!(none["strandedNonces"], json!([]), "absent reads as none, not as an error");
    }
}
