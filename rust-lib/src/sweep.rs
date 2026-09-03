//! What a receipt sweep did, and the `get_history` reply derived from it.
//!
//! The glue does the polling; the decisions — which rows the caller is being shown, which of
//! them another poll could still move, and how a chain frozen by its proxy is disclosed —
//! live here, so `cargo test --no-default-features` covers them.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::history::{self, TxRecord};
use crate::tokens::Token;
use crate::{networks, receipt, tokens, units, verified};

/// At most this many receipts per sweep, so a long history cannot turn one read into a
/// hundred round-trips.
pub const SWEEP_MAX: usize = 8;

/// Gas prices are rendered in GWEI, never in the native currency. `format_display` keeps five
/// fraction digits, and any plausible gas price in ETH is below that resolution — so an ETH
/// figure would read `<0.00001` for every transaction ever made: true, and useless.
pub const GAS_PRICE_DECIMALS: u8 = 9;
pub const GAS_PRICE_UNIT: &str = "gwei";

/// What one sweep did. `still_due` spans every chain, including ones the caller is not
/// being shown.
#[derive(Default)]
pub struct SweepOutcome {
    pub polled: usize,
    pub changed: usize,
    /// Receipts the store refused to write. The rows are unmoved and due again next sweep;
    /// disclosed so a wallet stuck on a full or read-only disk says why rather than showing
    /// `pending` for ever.
    pub unstored: usize,
    /// Rows skipped because their own chain's proxy is blocking: chain -> (hashes, verdict).
    /// A bare count leaves a frozen row on a non-active chain with nothing to explain it.
    pub blocked: BTreeMap<u64, (Vec<String>, Value)>,
    pub confirmed: bool,
    pub still_due: bool,
}

impl SweepOutcome {
    pub fn blocked_count(&self) -> usize {
        self.blocked.values().map(|(h, _)| h.len()).sum()
    }

    /// One entry per frozen chain, each naming its rows and carrying the verdict.
    pub fn blocked_json(&self) -> Vec<Value> {
        self.blocked
            .iter()
            .map(|(id, (hashes, v))| {
                let name = networks::by_chain_id(*id).map(|n| n.name).unwrap_or_default();
                verified::blocked_chain_json(*id, name, hashes, v)
            })
            .collect()
    }
}

/// One history row as the view reads it: the stored record, `stalled` (derived from the poll
/// horizon) and every amount rendered for display.
///
/// Rendered here rather than in `TxRecord` because that is serialized to disk: a display
/// string persisted there would need a migration the day the resolution changes. `nativeSymbol`
/// is the row's OWN chain's, so a fee is never priced in the active network's currency.
///
/// `enabled` is the user's token set for this row's chain, threaded in so a transfer is
/// decoded against exactly what the wallet offers — the same `tokens::for_chain` the balance
/// list and the send validator read, and never a second table that could disagree with them.
pub fn row_json(r: &TxRecord, now: u64, enabled: &[Token]) -> Value {
    let mut v = serde_json::to_value(r).unwrap_or_else(|_| json!({}));
    v["stalled"] = json!(history::is_stalled(r, now));
    // An intent whose broadcast never answered. It has no hash, so nothing can poll it and
    // no receipt will ever move it; what it has is a nonce that is still spoken for.
    v["unresolved"] = json!(history::is_unresolved(r));

    let native_symbol = networks::by_chain_id(r.chain_id).map(|n| n.native_symbol);
    if let Some(s) = native_symbol {
        v["nativeSymbol"] = json!(s);
    }
    let erc20 = r.kind == "erc20";
    let value_symbol =
        if erc20 { r.token_symbol.clone() } else { native_symbol.map(str::to_string) };
    // An ERC-20 row recorded before decimals were stored keeps the honest degradation: no
    // symbol, no amount, an em-dash. Native decimals follow the network for the same reason —
    // on a chain this wallet does not offer we cannot name the currency, so we claim no
    // figure in it either.
    let value_decimals = if erc20 { r.token_decimals } else { native_symbol.map(|_| 18) };
    if let Some(s) = value_symbol {
        v["valueSymbol"] = json!(s);
    }
    if let Some(d) = value_decimals {
        v["valueDecimals"] = json!(d);
    }
    units::decorate(&mut v, "value", &r.value, value_decimals);
    for (key, raw) in [
        ("feeWei", r.fee_wei.as_deref()),
        ("feeCeilingWei", r.fee_ceiling_wei.as_deref()),
        ("totalWei", r.total_wei.as_deref()),
    ] {
        if let Some(raw) = raw {
            units::decorate(&mut v, key, raw, Some(18));
        }
    }

    // Two different facts, and the view labels them apart rather than deciding which is the
    // real one: `to` is the recipient the user typed, `txTo` is the transaction's own target —
    // the token contract, for an ERC-20 send. Whether they differ is answered HERE, so a plain
    // native send costs the screen no extra row and QML does no address arithmetic. Present
    // exactly when `txTo` is, so absent-because-same stays distinct from absent-because-unread.
    // F-6. ONE casing for every address this reply carries. `from` and `to` are parsed
    // Addresses and already EIP-55; `txTo` and the log topics reach us in the node's own
    // lowercase, and rows storing that are on disk already — so it is normalised at READ time
    // too, not at decode alone. Case-folded comparisons below are unaffected either way.
    for (key, raw) in [("from", Some(&r.from)), ("to", Some(&r.to)), ("txTo", r.tx_to.as_ref())] {
        if let Some(raw) = raw {
            v[key] = json!(receipt::checksummed(raw));
        }
    }
    if let Some(tx_to) = r.tx_to.as_deref() {
        v["interactedWithDiffers"] = json!(!tx_to.eq_ignore_ascii_case(&r.to));
        if let Some(t) = tokens::by_address(r.chain_id, tx_to, enabled) {
            v["interactedWithSymbol"] = json!(t.symbol);
        }
    }

    v["gasPriceUnit"] = json!(GAS_PRICE_UNIT);
    for (key, raw) in [
        ("effectiveGasPrice", r.effective_gas_price.as_deref()),
        ("maxPriorityFeePerGas", r.max_priority_fee_per_gas.as_deref()),
    ] {
        if let Some(raw) = raw {
            units::decorate(&mut v, key, raw, Some(GAS_PRICE_DECIMALS));
        }
    }
    if let Some(p) = gas_used_percent(r) {
        v["gasUsedPercent"] = json!(p);
    }
    if !r.transfers.is_empty() {
        v["transfers"] = json!(transfers_json(r, enabled));
    }
    v
}

/// How much of the approved limit the transaction actually burnt, as an integer percent.
/// Both sides are stored, so this costs no call; absent unless both are known.
fn gas_used_percent(r: &TxRecord) -> Option<u64> {
    let limit = r.gas_limit.filter(|l| *l > 0)?;
    let used = crate::txbuild::parse_u256_any(r.gas_used.as_deref()?)?;
    u64::try_from(used).ok()?.checked_mul(100).map(|x| x / limit)
}

/// Every Transfer on the row, decorated from the token table at READ time.
///
/// A token the table does not hold gets `known: false` and no rendering at all: `decorate`
/// writes no key without decimals, so the honest degradation is structural rather than
/// remembered, and the view has nothing it could mistake for a token amount.
fn transfers_json(r: &TxRecord, enabled: &[Token]) -> Vec<Value> {
    r.transfers
        .iter()
        .map(|t| {
            let mut v = serde_json::to_value(t).unwrap_or_else(|_| json!({}));
            // As row_json: one casing, and a log topic is lowercase.
            for (key, raw) in [("contract", &t.contract), ("from", &t.from), ("to", &t.to)] {
                v[key] = json!(receipt::checksummed(raw));
            }
            v["mine"] = json!(t.from.eq_ignore_ascii_case(&r.from));
            let tok = tokens::by_address(r.chain_id, &t.contract, enabled);
            v["known"] = json!(tok.is_some());
            if let Some(tok) = tok {
                v["symbol"] = json!(tok.symbol);
                v["decimals"] = json!(tok.decimals);
                units::decorate(&mut v, "amount", &t.amount, Some(tok.decimals));
            }
            v
        })
        .collect()
}

/// The `get_history` body: the rows on `chain_id` only, each told whether a blocking proxy
/// froze it, plus both due flags. `stillDue` covers the rows in THIS reply — a due row on a
/// chain the caller is not being shown must not keep a timer polling over an idle screen —
/// while `stillDueAnyChain` is the sweep's all-chain answer.
pub fn history_reply(
    address: &str,
    chain_id: u64,
    rows: &[TxRecord],
    now: u64,
    out: &SweepOutcome,
    enabled: &[Token],
) -> Value {
    let mine: Vec<&TxRecord> = rows.iter().filter(|r| r.chain_id == chain_id).collect();
    let still_due = mine.iter().any(|r| history::is_live(r, now));
    let frozen = out.blocked.contains_key(&chain_id);
    let transactions: Vec<Value> = mine
        .iter()
        .map(|r| {
            let mut v = row_json(r, now, enabled);
            v["verificationBlocked"] = json!(frozen && history::is_live(r, now));
            v
        })
        .collect();
    json!({ "ok": true, "chainId": chain_id, "address": address,
            "transactions": transactions, "stillDue": still_due,
            "stillDueAnyChain": out.still_due, "unstored": out.unstored,
            "unresolved": unresolved_json(&mine),
            "blockedChains": out.blocked_json() })
}

/// The rows whose outcome we never learned, each naming the number it is holding.
///
/// This is the escape hatch made visible. A nonce that never mines blocks every later send
/// from the same account for ever — no timer clears it and no restart frees it — so the one
/// way out is a replacement send with that number pinned, and a user cannot pin a number
/// nobody has told them. `detail` is the node's own words when there were any.
fn unresolved_json(rows: &[&TxRecord]) -> Vec<Value> {
    rows.iter()
        .filter(|r| history::is_unresolved(r))
        .map(|r| {
            let what = match r.nonce {
                Some(n) => format!(
                    "This transaction was sent but never acknowledged, so nonce {n} is still \
                     held and later sends from this account cannot be mined until it is. To \
                     clear it, send a replacement with nonce {n} pinned."
                ),
                None => "This transaction was sent but never acknowledged, and no nonce was \
                         recorded for it."
                    .to_string(),
            };
            json!({ "nonce": r.nonce, "requestId": r.request_id, "timestamp": r.timestamp,
                    "detail": r.unknown_reason, "message": what })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(chain_id: u64, hash: &str, timestamp: u64) -> TxRecord {
        TxRecord {
            hash: hash.into(),
            chain_id,
            from: "0xaaaa".into(),
            to: "0xbbbb".into(),
            value: "0x1".into(),
            kind: "native".into(),
            status: "pending".into(),
            timestamp,
            ..Default::default()
        }
    }

    fn unhealthy() -> Value {
        json!({ "state": "unhealthy", "blocking": true, "action": "restart_or_reload",
                "message": "The verified proxy is not tracking the chain." })
    }

    #[test]
    fn a_live_row_on_the_active_chain_keeps_the_view_timer_running() {
        let now = 1_000_000;
        let live = pending(1, "0xaaa", now - 10);
        let done = TxRecord { status: "confirmed".into(), ..pending(1, "0xdone", now - 10) };

        let out = SweepOutcome { still_due: true, ..Default::default() };
        let v = history_reply("0xaaaa", 1, &[live, done.clone()], now, &out, &[]);
        assert_eq!((v["ok"].clone(), v["chainId"].clone()), (json!(true), json!(1)));
        assert_eq!(v["address"], json!("0xaaaa"));
        assert_eq!(v["transactions"].as_array().unwrap().len(), 2);
        assert_eq!(v["transactions"][0]["hash"], json!("0xaaa"));
        assert_eq!(v["transactions"][0]["stalled"], json!(false));
        assert_eq!(v["transactions"][0]["verificationBlocked"], json!(false));
        assert_eq!(v["stillDue"], json!(true));
        assert_eq!(v["blockedChains"], json!([]));

        // A settled row alone leaves nothing another poll could move.
        let v = history_reply("0xaaaa", 1, &[done], now, &SweepOutcome::default(), &[]);
        assert_eq!(v["stillDue"], json!(false));
    }

    #[test]
    fn a_row_due_on_another_chain_does_not_keep_this_screens_timer_running() {
        let now = 1_000_000;
        let elsewhere = pending(11_155_111, "0xbbb", now - 10);
        let out = SweepOutcome { still_due: true, ..Default::default() };
        let v = history_reply("0xaaaa", 1, &[elsewhere], now, &out, &[]);

        assert!(v["transactions"].as_array().unwrap().is_empty(), "the row is another chain's");
        assert_eq!(v["stillDue"], json!(false), "nothing on this screen can move");
        assert_eq!(v["stillDueAnyChain"], json!(true), "but the sweep still has work");
    }

    /// A receipt the store refused leaves its row pending and due again, so the count is
    /// disclosed: without it a wallet on a full or read-only disk shows `pending` for ever
    /// and says why nowhere at all.
    #[test]
    fn receipts_the_store_refused_are_counted_in_the_reply() {
        let now = 1_000_000;
        let out = SweepOutcome { unstored: 1, still_due: true, ..Default::default() };
        let v = history_reply("0xaaaa", 1, &[pending(1, "0xaaa", now - 10)], now, &out, &[]);
        assert_eq!(v["unstored"], json!(1));

        let clean = history_reply("0xaaaa", 1, &[], now, &SweepOutcome::default(), &[]);
        assert_eq!(clean["unstored"], json!(0), "and a clean sweep says zero, not nothing");
    }

    #[test]
    fn a_blocking_proxy_freezes_the_rows_it_skipped_and_names_every_frozen_chain() {
        let now = 1_000_000;
        let rows = [
            pending(1, "0xaaa", now - 10),
            pending(1, "0xold", now - 4_000),
            pending(11_155_111, "0xbbb", now - 10),
        ];
        let mut out = SweepOutcome { still_due: true, ..Default::default() };
        out.blocked.insert(1, (vec!["0xaaa".into()], unhealthy()));
        out.blocked.insert(11_155_111, (vec!["0xbbb".into()], unhealthy()));
        let v = history_reply("0xaaaa", 1, &rows, now, &out, &[]);

        // Both chains are disclosed, including the one this reply shows no rows for.
        let blocked = v["blockedChains"].as_array().unwrap();
        assert_eq!(blocked.len(), 2);
        assert_eq!((blocked[0]["chainId"].clone(), blocked[0]["network"].clone()),
                   (json!(1), json!("Ethereum")));
        assert_eq!(blocked[1]["chainId"], json!(11_155_111));
        assert_eq!(blocked[1]["hashes"], json!(["0xbbb"]));
        assert_eq!(out.blocked_count(), 2);

        // Only a row a poll would have moved is frozen: one past the horizon was not going
        // to be polled, so calling it blocked would blame the proxy for the give-up.
        let txs = v["transactions"].as_array().unwrap();
        assert_eq!(txs.len(), 2, "scoped to the active chain");
        assert_eq!(txs[0]["verificationBlocked"], json!(true));
        assert_eq!((txs[1]["verificationBlocked"].clone(), txs[1]["stalled"].clone()),
                   (json!(false), json!(true)));

        // Viewed from a third chain, both frozen chains are still disclosed even though
        // this reply shows not one of their rows.
        let v = history_reply("0xaaaa", 560_048, &rows, now, &out, &[]);
        assert_eq!(v["transactions"], json!([]));
        assert_eq!(v["blockedChains"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_native_row_is_rendered_in_ether_by_its_own_chains_symbol() {
        let now = 1_000_000;
        let mut r = pending(11_155_111, "0xaaa", now - 10);
        r.value = "1500000000000000000".into();
        r.fee_wei = Some("21000000000000".into());
        r.fee_ceiling_wei = Some("42000000000000".into());
        r.total_wei = Some("1500021000000000000".into());
        let v = row_json(&r, now, &[]);

        assert_eq!(v["valueSymbol"], json!("ETH"));
        assert_eq!(v["valueDecimals"], json!(18));
        assert_eq!(v["valueDisplay"], json!("1.5"));
        assert_eq!(v["valueExact"], json!("1.5"));
        assert_eq!(v["nativeSymbol"], json!("ETH"));
        assert_eq!(v["feeWeiDisplay"], json!("0.00002"));
        assert_eq!(v["feeWeiExact"], json!("0.000021"));
        assert_eq!(v["feeCeilingWeiDisplay"], json!("0.00004"));
        assert_eq!(v["totalWeiExact"], json!("1.500021"));
    }

    #[test]
    fn a_stored_hex_value_renders_the_same_as_a_decimal_one() {
        // Rows written by an earlier build store `value` as hex; both reach this field.
        let now = 1_000_000;
        let v = row_json(&pending(1, "0xaaa", now), now, &[]);
        assert_eq!(v["value"], json!("0x1"), "the stored form is untouched");
        assert_eq!(v["valueDisplay"], json!("<0.00001"), "1 wei is not an empty account");
        assert_eq!(v["valueExact"], json!("0.000000000000000001"));
    }

    #[test]
    fn an_erc20_row_with_no_recorded_decimals_shows_no_amount_at_all() {
        let now = 1_000_000;
        let mut r = pending(1, "0xaaa", now);
        r.kind = "erc20".into();
        r.value = "5000000".into();
        let v = row_json(&r, now, &[]);
        // Absent, not zero and not "5000000": we cannot scale it, so we do not claim a figure.
        assert!(v.get("valueDisplay").is_none() && v.get("valueExact").is_none());
        assert!(v.get("valueDecimals").is_none() && v.get("valueSymbol").is_none());

        r.token_symbol = Some("USDC".into());
        r.token_decimals = Some(6);
        let v = row_json(&r, now, &[]);
        assert_eq!(v["valueSymbol"], json!("USDC"));
        assert_eq!(v["valueDisplay"], json!("5"));
        // The fee is still ether, and it says so on the row rather than on the screen.
        assert_eq!(v["nativeSymbol"], json!("ETH"));
    }

    #[test]
    fn a_row_on_a_chain_this_wallet_no_longer_offers_names_no_currency() {
        // The bug this fixes: the view priced every row's fee in the ACTIVE network's symbol.
        let now = 1_000_000;
        let v = row_json(&pending(42_161, "0xaaa", now), now, &[]);
        assert!(v.get("nativeSymbol").is_none());
        assert!(v.get("valueSymbol").is_none());
        assert!(v.get("valueDisplay").is_none());
    }

    /// The escape, disclosed. A row whose broadcast never answered holds a nonce that no
    /// timer and no restart will ever free, and the only way out is a replacement send with
    /// that number pinned — which a user cannot do without being told the number.
    #[test]
    fn a_row_whose_outcome_never_came_back_names_the_number_it_is_holding() {
        let now = 1_000_000;
        let unknown = TxRecord {
            hash: String::new(),
            status: "unknown".into(),
            nonce: Some(7),
            request_id: Some("snd_1".into()),
            unknown_reason: Some("connection reset".into()),
            ..pending(1, "", now - 10)
        };
        let v = history_reply("0xaaaa", 1, &[unknown.clone()], now, &SweepOutcome::default(), &[]);

        let one = &v["unresolved"][0];
        assert_eq!(one["nonce"], json!(7));
        assert_eq!(one["requestId"], json!("snd_1"));
        assert_eq!(one["detail"], json!("connection reset"));
        assert!(one["message"].as_str().unwrap().contains("nonce 7 pinned"), "{one}");

        // It is shown as a transaction too, and it is neither pending nor stalled: nothing
        // is polling it, so a view that renders it as in-flight would be lying.
        assert_eq!(v["transactions"][0]["unresolved"], json!(true));
        assert_eq!(v["transactions"][0]["stalled"], json!(false));
        assert_eq!(v["stillDue"], json!(false), "there is no hash to poll");

        // And a settled row claims nothing: the disclosure is for unknowns alone.
        let done = TxRecord { status: "confirmed".into(), ..pending(1, "0xaaa", now) };
        let v = history_reply("0xaaaa", 1, &[done], now, &SweepOutcome::default(), &[]);
        assert_eq!(v["unresolved"], json!([]));
        assert_eq!(v["transactions"][0]["unresolved"], json!(false));
    }

    #[test]
    fn an_empty_history_stops_the_timer_and_claims_nothing() {
        let v = history_reply("0xaaaa", 1, &[], 1_000_000, &SweepOutcome::default(), &[]);
        assert_eq!(v["transactions"], json!([]));
        assert_eq!((v["stillDue"].clone(), v["stillDueAnyChain"].clone()),
                   (json!(false), json!(false)));
        assert_eq!(v["blockedChains"], json!([]));
    }

    const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
    const ME: &str = "0x8626f6940E2eb28930eFb4CeF49B2d1F2C9C1199";
    const THEM: &str = "0x0adBc7B2D1A2b7C8E9F0A1b2c3d4e5f60718D3A7";

    fn transfer(contract: &str, from: &str, to: &str, amount: &str) -> crate::TokenTransfer {
        crate::TokenTransfer {
            contract: contract.into(),
            from: from.into(),
            to: to.into(),
            amount: amount.into(),
        }
    }

    /// A settled ERC-20 send on mainnet: the recipient the user typed in `to`, the token
    /// contract in `tx_to`, and one Transfer log decoded off the receipt.
    fn erc20_row() -> TxRecord {
        TxRecord {
            hash: "0x9a3c".into(),
            chain_id: 1,
            from: ME.into(),
            to: THEM.into(),
            value: "1000000000000".into(),
            kind: "erc20".into(),
            token: Some(WETH.into()),
            token_symbol: Some("WETH".into()),
            token_decimals: Some(18),
            status: "confirmed".into(),
            timestamp: 1_000_000,
            nonce: Some(26),
            gas_limit: Some(51_000),
            gas_used: Some("36442".into()),
            effective_gas_price: Some("1500000000".into()),
            max_priority_fee_per_gas: Some("1000000000".into()),
            block_number: Some(25_882_523),
            tx_to: Some(WETH.into()),
            transfers: vec![transfer(WETH, ME, THEM, "1000000000000")],
            ..Default::default()
        }
    }

    /// THE LABELLING DEFECT. Our "To" is the token RECIPIENT; the transaction's own `to` is
    /// the WETH contract. Both are facts and they disagree, so the row says they differ and
    /// the view labels them apart rather than picking one.
    #[test]
    fn an_erc20_row_reports_the_recipient_and_the_contract_it_interacted_with() {
        let v = row_json(&erc20_row(), 1_000_100, &[]);
        assert_eq!(v["to"], json!(THEM), "the recipient the user meant, unchanged");
        assert_eq!(v["txTo"], json!(WETH));
        assert_eq!(v["interactedWithDiffers"], json!(true));
        assert_eq!(v["interactedWithSymbol"], json!("WETH"), "named from the token table");
    }

    /// The common case must not get noisier. A plain ETH send targets the recipient itself,
    /// so the row says the two do not differ and the screen shows no second address.
    #[test]
    fn a_native_send_has_nothing_extra_to_say_about_where_it_went() {
        let r = TxRecord { tx_to: Some(THEM.to_lowercase()), ..
                           TxRecord { transfers: vec![], ..erc20_row() } };
        let r = TxRecord { kind: "native".into(), to: THEM.into(), ..r };
        let v = row_json(&r, 1_000_100, &[]);
        // Case-folded: a receipt answers lowercase hex and `to` is EIP-55 checksummed.
        assert_eq!(v["interactedWithDiffers"], json!(false));
        assert!(v.get("transfers").is_none(), "nothing moved but ether");
    }

    /// F-6. A row written by a build that stored the node's lowercase is already on disk, so
    /// the checksumming cannot live at decode alone — the card that exists to RECONCILE
    /// addresses printed `0x0ADB…d3A7` against `0x0adb…d3a7` for one address.
    #[test]
    fn a_row_stored_in_the_nodes_own_casing_still_renders_one_casing() {
        let r = TxRecord {
            tx_to: Some(WETH.to_lowercase()),
            transfers: vec![transfer(&WETH.to_lowercase(), &ME.to_lowercase(),
                                     &THEM.to_lowercase(), "1000000000000")],
            ..erc20_row()
        };
        let v = row_json(&r, 1_000_100, &[]);
        assert_eq!(v["txTo"], json!(WETH), "EIP-55, whatever the node spelled");
        assert_eq!(v["interactedWithDiffers"], json!(true), "and the comparison is unmoved");
        let t = &v["transfers"][0];
        assert_eq!((t["contract"].clone(), t["from"].clone(), t["to"].clone()),
                   (json!(WETH), json!(ME), json!(THEM)));
        assert_eq!(t["mine"], json!(true), "the account is still recognised as the sender");
        assert_eq!(t["symbol"], json!("WETH"), "and the token table still matches it");
    }

    /// Absent-because-same and absent-because-never-read are different answers, and the flag
    /// is present exactly when `txTo` is so a view can tell them apart.
    #[test]
    fn a_row_whose_receipt_predates_these_fields_claims_nothing_about_either() {
        let old = TxRecord { tx_to: None, transfers: vec![], ..erc20_row() };
        let v = row_json(&old, 1_000_100, &[]);
        assert!(v.get("txTo").is_none());
        assert!(v.get("interactedWithDiffers").is_none(), "not `false`, which is a claim");
        assert!(v.get("transfers").is_none(), "we never saw a receipt, so we assert no logs");
    }

    /// Gas prices go out in GWEI. In ether every plausible one is below the display
    /// resolution, so the bounded rendering would read `<0.00001` for every transaction.
    #[test]
    fn a_gas_price_is_priced_in_gwei_or_it_is_useless() {
        let v = row_json(&erc20_row(), 1_000_100, &[]);
        assert_eq!(v["gasPriceUnit"], json!("gwei"));
        assert_eq!(v["effectiveGasPriceDisplay"], json!("1.5"));
        assert_eq!(v["effectiveGasPriceExact"], json!("1.5"));
        assert_eq!(v["maxPriorityFeePerGasDisplay"], json!("1"));

        // The control: the same figure in ether is the dust marker, for every gas price there
        // has ever been. That is what this unit exists to avoid.
        assert_eq!(units::format_display("1500000000", 18).unwrap(), "<0.00001");
    }

    /// Free and offline: the limit is stored from the approved quote and the used from the
    /// receipt, so the whole row survives a restart with no call.
    #[test]
    fn gas_used_against_the_limit_costs_nothing_to_report() {
        let v = row_json(&erc20_row(), 1_000_100, &[]);
        assert_eq!(v["gasLimit"], json!(51_000));
        assert_eq!(v["gasUsed"], json!("36442"));
        assert_eq!(v["gasUsedPercent"], json!(71), "36442 * 100 / 51000, integer");

        // Either side missing is an em-dash, never a percentage of nothing.
        let no_used = TxRecord { gas_used: None, ..erc20_row() };
        assert!(row_json(&no_used, 1_000_100, &[]).get("gasUsedPercent").is_none());
        let no_limit = TxRecord { gas_limit: None, ..erc20_row() };
        assert!(row_json(&no_limit, 1_000_100, &[]).get("gasUsedPercent").is_none());
    }

    #[test]
    fn a_transfer_of_a_token_we_know_is_rendered_in_its_own_units() {
        let v = row_json(&erc20_row(), 1_000_100, &[]);
        let t = &v["transfers"][0];
        assert_eq!(t["known"], json!(true));
        assert_eq!(t["mine"], json!(true), "this account sent it");
        assert_eq!(t["symbol"], json!("WETH"));
        assert_eq!(t["decimals"], json!(18));
        assert_eq!(t["amount"], json!("1000000000000"), "base units, unscaled");
        assert_eq!(t["amountDisplay"], json!("<0.00001"), "bounded, and never a plain 0");
        assert_eq!(t["amountExact"], json!("0.000001"));
        assert!(v.get("transfersMore").is_none(), "the cap dropped none");
    }

    /// An unlisted token has unknown decimals, and `decorate` writes NO key without them —
    /// so the honest degradation is structural. Nothing here may be scaled by an assumed 18.
    #[test]
    fn a_transfer_of_a_token_we_do_not_know_carries_no_rendered_amount_at_all() {
        let stranger = "0x1234567890123456789012345678901234567890";
        let r = TxRecord {
            transfers: vec![transfer(stranger, THEM, ME, "42000000")],
            ..erc20_row()
        };
        let t = &row_json(&r, 1_000_100, &[])["transfers"][0];
        assert_eq!(t["known"], json!(false));
        assert_eq!(t["mine"], json!(false));
        assert_eq!(t["amount"], json!("42000000"), "the raw on-chain integer, and only that");
        for k in ["symbol", "decimals", "amountDisplay", "amountExact"] {
            assert!(t.get(k).is_none(), "{k} cannot be known without the token's decimals");
        }
    }

    /// The same offered set the balance list and the send path read. An enabled token that
    /// decoded on one screen and read as an unknown contract on another would be two tables
    /// disagreeing, which is exactly what threading one `for_chain` through here prevents.
    #[test]
    fn a_transfer_in_an_enabled_token_is_decoded_from_the_same_offered_set() {
        let usdc = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        let r = TxRecord {
            transfers: vec![transfer(usdc, THEM, ME, "42000000")],
            tx_to: Some(usdc.into()),
            ..erc20_row()
        };
        // Nothing enabled: an unknown contract, and — correctly — no amount at all.
        let t = &row_json(&r, 1_000_100, &[])["transfers"][0];
        assert_eq!(t["known"], json!(false));
        assert!(t.get("amountExact").is_none());

        let enabled = [crate::tokens::Token {
            symbol: "USDC".into(),
            name: "USD Coin".into(),
            decimals: 6,
            address: Some(usdc.into()),
            native: false,
        }];
        let v = row_json(&r, 1_000_100, &enabled);
        assert_eq!(v["interactedWithSymbol"], json!("USDC"));
        let t = &v["transfers"][0];
        assert_eq!((t["known"].clone(), t["symbol"].clone()), (json!(true), json!("USDC")));
        assert_eq!(t["amountExact"], json!("42"), "at the snapshot's six decimals, not an assumed 18");
    }

    /// The calldata a row publishes is the transaction's own. Absent stays absent: a row
    /// written before it was recorded must not be shown a `"0x"` nobody ever signed.
    #[test]
    fn a_row_publishes_the_calldata_it_recorded_and_invents_none() {
        let r = TxRecord { tx_input: Some("0xa9059cbb0000".into()), ..erc20_row() };
        assert_eq!(row_json(&r, 1_000_100, &[])["txInput"], json!("0xa9059cbb0000"));
        assert!(row_json(&erc20_row(), 1_000_100, &[]).get("txInput").is_none());
    }

    #[test]
    fn the_transfers_the_cap_dropped_are_counted_rather_than_forgotten() {
        let r = TxRecord { transfers_more: Some(3), ..erc20_row() };
        assert_eq!(row_json(&r, 1_000_100, &[])["transfersMore"], json!(3));
    }
}
