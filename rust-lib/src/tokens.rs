//! The tokens a network offers: a fixed BUILT-IN table, plus the set the user turned on.
//!
//! Two sets, two different provenances, and keeping them apart is the whole point.
//!
//! The BUILT-IN table — native ETH plus WETH — is this wallet asserting a network-wide fact
//! on the user's behalf. A wrong address there spends into a contract that is not the one the
//! user meant, so an address enters it only when it has been verified against an authoritative
//! source AND confirmed on-chain. `None` means "not verified yet" and the network then offers
//! ETH only — never a guess. That rule is unchanged and applies to this table alone.
//!
//! The ENABLED set is the user's own choice, one snapshot per row taken from
//! `token_list_module`. This wallet asserts nothing about those addresses: it relays the list
//! bucket the row came from and marks every published row `builtin: false`, so the screen can
//! say who vouched for it. What it will not do is invent a field — an address `token_list`
//! does not hold cannot be enabled, because `decimals` scales every amount rendered or sent.
//!
//! `for_chain` is the one place the two sets meet, and `resolve` — the send path's validator —
//! reads it. Whatever is admitted here is what this wallet will spend into.

use std::collections::HashMap;

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::networks;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Token {
    pub symbol: String,
    pub name: String,
    pub decimals: u8,
    /// `None` for the chain's native currency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    pub native: bool,
}

/// How the balance list is ordered. Persisted, so a restart shows what the user chose.
///
/// `Balance` orders by each token's OWN amount. This wallet has no fiat price and will not
/// fetch one — a price feed discloses the user's IP — so across two different tokens that is
/// NOT a value order, and nothing rendering it may imply otherwise.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenSort {
    #[default]
    Alpha,
    Balance,
}

impl TokenSort {
    /// `None` for anything else: an order we do not have must not silently become the
    /// default, or a typo in a caller reorders the screen with no complaint.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "alpha" => Some(Self::Alpha),
            "balance" => Some(Self::Balance),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alpha => "alpha",
            Self::Balance => "balance",
        }
    }
}

/// Verified: present in this workspace at `uniswap-module/rust-lib/src/config.rs` and
/// confirmed as the canonical mainnet WETH9 deployment.
const WETH_MAINNET: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";

/// VERIFIED ABSENT — these stay `None`, and the investigation that established it is the
/// reason, not an absence of effort.
///
/// **Sepolia**: at least six WETH contracts are deployed and *all six* return
/// `symbol() == "WETH"` and `decimals() == 18`, so an on-chain check cannot choose between
/// them. Worse, the three largest ecosystems on the chain each hardcode a DIFFERENT one:
/// Uniswap's routers bake in `0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14`, Aave's gateway
/// returns `0xC558DBdd856501FCd9aaF1E62eae57A9F0629a3c`, and Chainlink CCIP lists
/// `0x097D90c9d3E0B50Ca60e1ae45F6A81010f9FB534`. `eth-clients/sepolia` — the authoritative
/// network definition — names no token contracts at all, and there is no genesis predeploy.
/// Picking by popularity does not even converge: the largest deployment by ETH backing is not
/// the best-documented one.
///
/// **Hoodi**: four live WETH contracts, no official registry (`eth-clients/hoodi` names only
/// the deposit contract; Lido's registry, the most complete for this chain, lists none), and
/// no Uniswap deployment to act as a de-facto pointer. The mainnet WETH address is confirmed
/// EMPTY here — `eth_getCode` returns `0x` — because mainnet WETH was a nonce-based CREATE in
/// 2017 and cannot reproduce on another chain.
///
/// Shipping any of them would be this wallet asserting a network-wide fact that is not true.
/// The concrete harm: a user holding Aave-faucet WETH sees a zero balance and concludes their
/// funds vanished. So these networks offer ETH only out of the box — and the user who wants a
/// specific WETH names it themselves, which is what the enabled set is for.
const WETH_SEPOLIA: Option<&str> = None;
const WETH_HOODI: Option<&str> = None;

fn native(symbol: &str) -> Token {
    Token {
        symbol: symbol.to_string(),
        name: "Ether".to_string(),
        decimals: 18,
        address: None,
        native: true,
    }
}

fn weth(address: &str) -> Token {
    Token {
        symbol: "WETH".to_string(),
        name: "Wrapped Ether".to_string(),
        decimals: 18,
        address: Some(address.to_string()),
        native: false,
    }
}

fn weth_for(chain_id: u64) -> Option<&'static str> {
    match chain_id {
        1 => Some(WETH_MAINNET),
        11_155_111 => WETH_SEPOLIA,
        560_048 => WETH_HOODI,
        _ => None,
    }
}

/// The rows this wallet asserts on `chain_id`, native first. Empty for an unsupported network.
pub fn builtin_for_chain(chain_id: u64) -> Vec<Token> {
    let Some(net) = networks::by_chain_id(chain_id) else {
        return Vec::new();
    };
    let mut out = vec![native(net.native_symbol)];
    if let Some(addr) = weth_for(chain_id) {
        out.push(weth(addr));
    }
    out
}

/// Whether `address` names a built-in row on `chain_id`. Built-in rows cannot be turned off:
/// the native currency pays for every fee, and WETH is this wallet's own assertion rather
/// than something the user opted into.
pub fn is_builtin(chain_id: u64, address: &str) -> bool {
    let a = address.trim();
    builtin_for_chain(chain_id)
        .iter()
        .filter_map(|t| t.address.as_deref())
        .any(|b| b.eq_ignore_ascii_case(a))
}

/// The tokens offered on `chain_id`: the built-in rows first, then `enabled` — the user's own
/// snapshots. THE choke point. The balance list, the send validator and the picker all read
/// this, so none of them can disagree about what the wallet offers.
///
/// A built-in wins every field, so an enabled row naming a built-in address is dropped rather
/// than shown twice, and so is a repeat inside `enabled`. A row with no address, or one
/// claiming to be native, is dropped outright: only the chain has a native currency, and a
/// second one would put a second Multicall3 leg on the same balance.
pub fn for_chain(chain_id: u64, enabled: &[Token]) -> Vec<Token> {
    let mut out = builtin_for_chain(chain_id);
    if out.is_empty() {
        return out;
    }
    for t in enabled {
        let Some(a) = t.address.as_deref().map(str::trim).filter(|a| !a.is_empty()) else {
            continue;
        };
        if t.native {
            continue;
        }
        if out.iter().any(|b| b.address.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(a))) {
            continue;
        }
        out.push(t.clone());
    }
    out
}

/// The offered token AT an address on `chain_id` — the only lookup that cannot land on the
/// wrong contract, and what a caller naming a token exactly should use.
pub fn by_address(chain_id: u64, address: &str, enabled: &[Token]) -> Option<Token> {
    let a = address.trim();
    for_chain(chain_id, enabled)
        .into_iter()
        .find(|t| t.address.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(a)))
}

/// Resolve `key` — an address, or a symbol — to the one token it names on `chain_id`.
///
/// A token's identity is its `(chainId, address)`; a symbol is not an identity. The shipped
/// list carries two mainnet contracts both calling themselves `LIT`, both 18 decimals, and a
/// user may hold and enable both. So a symbol resolves only while it is UNAMBIGUOUS among
/// what the chain offers, and an ambiguous one is an error naming every candidate — never a
/// first match, which is how a send reaches the contract the user did not mean.
///
/// A built-in row still wins its symbol outright: the native currency and WETH are this
/// wallet's own assertion, so an enabled contract calling itself `ETH` neither shadows it nor
/// makes it ambiguous.
pub fn resolve(chain_id: u64, key: &str, enabled: &[Token]) -> Result<Token, String> {
    let k = key.trim();
    if k.is_empty() {
        return Err(format!("no token was named for chain {chain_id}"));
    }
    if k.parse::<alloy::primitives::Address>().is_ok() {
        return by_address(chain_id, k, enabled)
            .ok_or_else(|| format!("no token at {k} is offered on chain {chain_id}"));
    }
    let mut hits: Vec<Token> = for_chain(chain_id, enabled)
        .into_iter()
        .filter(|t| t.symbol.eq_ignore_ascii_case(k))
        .collect();
    if let Some(i) = hits
        .iter()
        .position(|t| t.native || t.address.as_deref().is_some_and(|a| is_builtin(chain_id, a)))
    {
        return Ok(hits.swap_remove(i));
    }
    match hits.len() {
        0 => Err(format!("token '{k}' is not offered on chain {chain_id}")),
        1 => Ok(hits.remove(0)),
        _ => {
            let addrs: Vec<&str> = hits.iter().filter_map(|t| t.address.as_deref()).collect();
            Err(format!(
                "'{k}' is the symbol of {} different contracts offered on chain {chain_id} \
                 ({}); name the one you mean by its address",
                hits.len(),
                addrs.join(", ")
            ))
        }
    }
}

/// `resolve` with the reason dropped, for a read-only decoration that has nowhere to put it.
/// An ambiguous symbol is `None` here — anything ACTING on the answer calls `resolve` and
/// relays the refusal instead.
pub fn find(chain_id: u64, key: &str, enabled: &[Token]) -> Option<Token> {
    resolve(chain_id, key, enabled).ok()
}

/// The addresses `list_meta` asks `token_list` about: this chain's non-native rows only.
/// `None` when the chain offers nothing but its native currency — there is nothing to ask.
pub fn meta_query(tokens: &[Token]) -> Option<String> {
    let addrs: Vec<&str> = tokens.iter().filter_map(|t| t.address.as_deref()).collect();
    if addrs.is_empty() {
        return None;
    }
    serde_json::to_string(&addrs).ok()
}

/// Which of `token_list`'s buckets answered for this row. `token_list` labels every reply
/// row; an unlabelled match is `unknown` rather than a guess, because the buckets differ in
/// how much the user asked for them and this module cannot tell them apart on its own.
fn bucket_of(entry: &Value) -> &'static str {
    match entry.get("source").and_then(Value::as_str) {
        Some("custom") => "custom",
        Some("downloaded") => "downloaded",
        Some("embedded") => "embedded",
        _ => "unknown",
    }
}

/// Where a row's fields came from — the provenance every published row carries beside
/// `builtin`, so the screen can say who vouched for the address.
///
/// `allowlist` is this wallet's own assertion and `enabled` is a snapshot the user took from
/// a list that no longer holds the row; the three bucket names are `token_list`'s, relayed.
fn source_of(entry: Option<&Value>, native: bool, builtin: bool) -> &'static str {
    match (native, entry, builtin) {
        (true, _, _) => "native",
        (_, Some(e), _) => bucket_of(e),
        (_, None, true) => "allowlist",
        (_, None, false) => "enabled",
    }
}

/// `token_list` entries for `chain_id`, keyed by lowercased address. An entry for another
/// chain is not a match: the same address is a different contract on a different network.
fn index(chain_id: u64, listed: &[Value]) -> HashMap<String, &Value> {
    listed
        .iter()
        .filter(|e| e.get("chainId").and_then(Value::as_u64) == Some(chain_id))
        .filter_map(|e| Some((e.get("address")?.as_str()?.to_lowercase(), e)))
        .collect()
}

/// Decorate the offered rows with `token_list` metadata, matched on `(chainId, address)`
/// lowercased. The offered set alone decides membership and wins every field it carries —
/// six sepolia contracts answer `symbol() == "WETH"`, so a list can never name a token in.
///
/// `meta` is keyed by lowercased address. Adds `inTokenList`, `metadataSource` and `builtin`
/// (all always present) and `logoURI` (only when the list has one).
pub fn enrich(chain_id: u64, tokens: &[Token], meta: &HashMap<String, Value>) -> Vec<Value> {
    tokens
        .iter()
        .map(|t| {
            let mut v = serde_json::to_value(t).unwrap_or_else(|_| json!({}));
            let entry = t
                .address
                .as_deref()
                .and_then(|a| meta.get(&a.to_lowercase()))
                .filter(|e| e.get("chainId").and_then(Value::as_u64) == Some(chain_id));
            let builtin = t.native || t.address.as_deref().is_some_and(|a| is_builtin(chain_id, a));
            v["inTokenList"] = json!(entry.is_some());
            // Where the row's metadata came from, relayed from token_list rather than
            // inferred: with an embedded list in play a match no longer implies one bucket.
            v["metadataSource"] = json!(source_of(entry, t.native, builtin));
            // Whose assertion the address is. `metadataSource` cannot say it: a built-in row
            // and an enabled one both read `embedded` when the same list decorates them.
            v["builtin"] = json!(builtin);
            if let Some(uri) = entry.and_then(|e| e.get("logoURI")).and_then(Value::as_str) {
                v["logoURI"] = json!(uri);
            }
            v
        })
        .collect()
}

/// The token picker's rows for `chain_id`: everything the wallet offers, plus everything
/// `token_list` holds for the chain, as `(matches BEFORE the cut, rows after it)`.
///
/// `query` matches a symbol or name (case-insensitive substring) or an exact address; an
/// empty query matches everything. `limit` of `None` is no cut at all. Order: the native row,
/// then what is enabled, then the rest — alphabetically within each band.
///
/// A listed row missing `decimals`, `symbol` or a parseable address is dropped rather than
/// filled in: an offer the user could act on must carry the scale its amounts are read at.
pub fn available(
    chain_id: u64,
    listed: &[Value],
    enabled: &[Token],
    query: &str,
    limit: Option<usize>,
) -> (usize, Vec<Value>) {
    if networks::by_chain_id(chain_id).is_none() {
        return (0, Vec::new());
    }
    let meta = index(chain_id, listed);
    let mut rows: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for t in for_chain(chain_id, enabled) {
        let key = t.address.as_deref().map(str::to_lowercase);
        let entry = key.as_deref().and_then(|k| meta.get(k)).copied();
        let builtin = t.native || t.address.as_deref().is_some_and(|a| is_builtin(chain_id, a));
        if let Some(k) = key {
            seen.push(k);
        }
        rows.push(row(&t, entry, true, builtin, source_of(entry, t.native, builtin)));
    }
    for (key, entry) in meta.iter() {
        if seen.iter().any(|s| s == key) {
            continue;
        }
        let Some(t) = token_of(entry) else { continue };
        rows.push(row(&t, Some(entry), false, false, bucket_of(entry)));
    }

    let mut hits: Vec<Value> = rows.into_iter().filter(|r| matches(r, query)).collect();
    hits.sort_by_cached_key(|r| {
        (
            // Native first, then what is on, then the rest — the order the user reads.
            match (r["native"] == json!(true), r["enabled"] == json!(true)) {
                (true, _) => 0u8,
                (_, true) => 1,
                _ => 2,
            },
            r["symbol"].as_str().unwrap_or_default().to_lowercase(),
            r["address"].as_str().unwrap_or_default().to_lowercase(),
        )
    });
    let total = hits.len();
    if let Some(n) = limit {
        hits.truncate(n);
    }
    (total, hits)
}

/// The `token_list` row for `address` on `chain_id`, read as a `Token` — the snapshot the
/// enabled set stores. `None` when the list holds no such row, or holds one this wallet would
/// have to fill in: an address it cannot describe is one this wallet must not offer.
pub fn snapshot_of(chain_id: u64, address: &str, listed: &[Value]) -> Option<Token> {
    index(chain_id, listed).get(&address.trim().to_lowercase()).copied().and_then(token_of)
}

/// One `token_list` entry read as a `Token`, or `None` when a field this wallet would have to
/// invent is missing. `decimals` scales every amount, so a row without one is not an offer.
fn token_of(entry: &Value) -> Option<Token> {
    let address = entry.get("address")?.as_str()?.trim();
    if address.parse::<alloy::primitives::Address>().is_err() {
        return None;
    }
    let symbol = entry.get("symbol")?.as_str()?.trim();
    let decimals = u8::try_from(entry.get("decimals")?.as_u64()?).ok()?;
    (!symbol.is_empty()).then(|| Token {
        symbol: symbol.to_string(),
        name: entry.get("name").and_then(Value::as_str).unwrap_or(symbol).to_string(),
        decimals,
        address: Some(address.to_string()),
        native: false,
    })
}

fn row(t: &Token, entry: Option<&Value>, enabled: bool, builtin: bool, source: &str) -> Value {
    let mut v = serde_json::to_value(t).unwrap_or_else(|_| json!({}));
    v["enabled"] = json!(enabled);
    v["builtin"] = json!(builtin);
    v["source"] = json!(source);
    if let Some(uri) = entry.and_then(|e| e.get("logoURI")).and_then(Value::as_str) {
        v["logoURI"] = json!(uri);
    }
    v
}

fn matches(row: &Value, query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    if row["address"].as_str().is_some_and(|a| a.eq_ignore_ascii_case(q)) {
        return true;
    }
    let needle = q.to_lowercase();
    ["symbol", "name"]
        .iter()
        .filter_map(|k| row[*k].as_str())
        .any(|s| s.to_lowercase().contains(&needle))
}

/// The balance rows for one read: one row per OFFERED token, already in the persisted order.
///
/// `raw` is what each Multicall3 leg decoded to, positionally, and `None` is a leg that
/// failed. Every offered token gets a row, a zero balance included — a token the user turned
/// on and then cannot find in the list reads as the wallet having lost it.
///
/// `display` is bounded and `amountExact` carries every digit as a plain decimal string
/// (`exact` is the older name for the same digits). All three are ABSENT for a failed leg, so
/// a view renders an em-dash: "we could not read it" is not "you have none".
pub fn balance_rows(
    chain_id: u64,
    offered: &[Token],
    raw: &[Option<String>],
    order: TokenSort,
) -> Vec<Value> {
    let mut rows: Vec<Value> = offered
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let amount = raw.get(i).cloned().flatten().unwrap_or_default();
            let builtin = t.native || t.address.as_deref().is_some_and(|a| is_builtin(chain_id, a));
            let mut row = json!({
                "symbol": t.symbol,
                "address": t.address,
                "decimals": t.decimals,
                "native": t.native,
                // Whose assertion the address is, so a row can offer to turn itself off
                // without a second read — and never offer it for a row that cannot.
                "builtin": builtin,
                // Empty means the leg failed; the view shows an em-dash, never a zero.
                "raw": amount,
            });
            // Rendered here so no caller does amount arithmetic: a JS number loses digits
            // above 2^53, and a failed leg emits no key at all rather than a zero.
            if let Some(d) = crate::units::format_display(&amount, t.decimals) {
                row["display"] = json!(d);
            }
            if let Some(e) = crate::units::format_exact(&amount, t.decimals) {
                row["amountExact"] = json!(e);
                row["exact"] = json!(e);
            }
            row
        })
        .collect();
    sort_balance_rows(&mut rows, order);
    rows
}

/// Order the balance rows the persisted way, in place.
///
/// The native currency stays FIRST in both orders. It is the only row that pays for a fee, so
/// it is the one figure a user must always be able to find without hunting; and with no price
/// feed it is not comparable with the rest anyway.
///
/// `Balance` sorts by each token's own amount, descending, with a tie broken alphabetically —
/// non-zero first, then rows whose sub-call failed, then zero, because "we could not read it"
/// is not "you have none" and burying an unread row under the zeros hides that. The
/// comparison is exact `U256`; there is no floating point in this file.
pub fn sort_balance_rows(rows: &mut [Value], order: TokenSort) {
    rows.sort_by_cached_key(|r| {
        let native = r["native"] != json!(true);
        let symbol = r["symbol"].as_str().unwrap_or_default().to_lowercase();
        let address = r["address"].as_str().unwrap_or_default().to_lowercase();
        let (rank, amount) = match order {
            TokenSort::Alpha => (0u8, U256::ZERO),
            TokenSort::Balance => match amount_of(r) {
                Some(v) if !v.is_zero() => (0, v),
                None => (1, U256::ZERO),
                Some(v) => (2, v),
            },
        };
        (native, rank, std::cmp::Reverse(amount), symbol, address)
    });
}

/// A row's balance in base units. `None` when the sub-call failed and the row carries no
/// readable figure — which is not the same as zero and must not sort as one.
fn amount_of(row: &Value) -> Option<U256> {
    let raw = row.get("raw")?.as_str()?.trim();
    (!raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()))
        .then(|| U256::from_str_radix(raw, 10).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const DAI: &str = "0x6B175474E89094C44Da98b954EedeAC495271d0F";
    /// Both are real mainnet rows of the SHIPPED uniswap-default list, both `LIT`, both 18
    /// decimals. Holding and enabling both is ordinary; nothing about this is hypothetical.
    const LIT_LITENTRY: &str = "0xb59490aB09A0f526Cc7305822aC65f2Ab12f9723";
    const LIT_LIGHTER: &str = "0x232CE3bd40fCd6f80f3d55A522d03f25Df784Ee2";
    const UNI_SEPOLIA_WETH: &str = "0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14";

    fn meta_of(entries: &[Value]) -> HashMap<String, Value> {
        entries
            .iter()
            .map(|e| (e["address"].as_str().unwrap().to_lowercase(), e.clone()))
            .collect()
    }

    fn tok(symbol: &str, name: &str, decimals: u8, address: &str) -> Token {
        Token {
            symbol: symbol.into(),
            name: name.into(),
            decimals,
            address: Some(address.into()),
            native: false,
        }
    }

    fn listed(chain_id: u64, symbol: &str, name: &str, decimals: u64, address: &str) -> Value {
        json!({ "chainId": chain_id, "address": address, "symbol": symbol,
                "name": name, "decimals": decimals, "source": "embedded" })
    }

    fn symbols(rows: &[Value]) -> Vec<String> {
        rows.iter().map(|r| r["symbol"].as_str().unwrap_or_default().to_string()).collect()
    }

    fn offered(chain_id: u64, enabled: &[Token]) -> Vec<String> {
        for_chain(chain_id, enabled).iter().map(|t| t.symbol.clone()).collect()
    }

    fn balance(symbol: &str, raw: &str, native: bool) -> Value {
        json!({ "symbol": symbol, "raw": raw, "native": native, "decimals": 18 })
    }

    #[test]
    fn every_address_in_the_table_is_a_valid_eip55_checksum() {
        for net in networks::ALL {
            for t in builtin_for_chain(net.chain_id) {
                let Some(a) = t.address else { continue };
                // parse_checksummed rejects a mistyped or wrongly-cased address, which is
                // the transcription error this test exists to catch.
                let parsed = Address::parse_checksummed(&a, None)
                    .unwrap_or_else(|e| panic!("{} {} on chain {}: {e}", t.symbol, a, net.chain_id));
                assert_ne!(parsed, Address::ZERO, "{} is the zero address", t.symbol);
            }
        }
    }

    #[test]
    fn symbols_and_addresses_are_unique_within_a_network() {
        for net in networks::ALL {
            let list = for_chain(net.chain_id, &[]);
            let mut symbols: Vec<String> = list.iter().map(|t| t.symbol.to_lowercase()).collect();
            symbols.sort();
            symbols.dedup();
            assert_eq!(symbols.len(), list.len(), "duplicate symbol on {}", net.key);

            let mut addrs: Vec<String> =
                list.iter().filter_map(|t| t.address.clone()).map(|a| a.to_lowercase()).collect();
            let n = addrs.len();
            addrs.sort();
            addrs.dedup();
            assert_eq!(addrs.len(), n, "duplicate address on {}", net.key);
        }
    }

    #[test]
    fn every_network_offers_exactly_one_native_token_first() {
        // Including with an enabled set in play: a second native row would put a second
        // Multicall3 leg on the same balance.
        let enabled = [tok("USDC", "USD Coin", 6, USDC), Token { native: true, ..tok("ETH", "Ether", 18, DAI) }];
        for net in networks::ALL {
            let list = for_chain(net.chain_id, &enabled);
            assert!(list[0].native, "{}: native token must be first", net.key);
            assert_eq!(list.iter().filter(|t| t.native).count(), 1);
            assert!(list.iter().filter(|t| !t.native).all(|t| t.address.is_some()));
        }
    }

    #[test]
    fn mainnet_offers_weth_and_the_unverified_testnets_do_not() {
        // Not "unverified" for want of looking: sepolia has six WETH deployments that pass
        // every on-chain check and three ecosystems that disagree about which is real; hoodi
        // has four and no registry at all. See the note on WETH_SEPOLIA.
        assert!(find(1, "WETH", &[]).is_some());
        assert!(find(1, WETH_MAINNET, &[]).is_some(), "lookup by address must work");
        // Deliberate: an unverified address is absent, not guessed. A user who wants one of
        // the six names it themselves — as an ENABLED token, carrying token_list's provenance
        // and not this wallet's assertion.
        assert!(find(11_155_111, "WETH", &[]).is_none());
        assert!(find(560_048, "WETH", &[]).is_none());
    }

    #[test]
    fn naming_the_native_currency_explicitly_reaches_the_native_path() {
        // The Send screen now always names a token, "ETH" included. If that did not resolve
        // to the native entry, every plain send would be built as an ERC-20 transfer.
        for net in networks::ALL {
            let t = find(net.chain_id, net.native_symbol, &[]).unwrap();
            assert!(t.native && t.address.is_none(), "on {}", net.key);
            assert!(find(net.chain_id, "eth", &[]).unwrap().native, "and case-insensitively");
        }
    }

    #[test]
    fn an_unsupported_network_offers_nothing() {
        let enabled = [tok("USDC", "USD Coin", 6, USDC)];
        assert!(for_chain(999, &[]).is_empty());
        assert!(find(999, "ETH", &[]).is_none());
        // And an enabled row cannot conjure a network the wallet does not have.
        assert!(for_chain(999, &enabled).is_empty());
        assert!(find(999, USDC, &enabled).is_none());
        assert_eq!(available(999, &[listed(999, "USDC", "USD Coin", 6, USDC)], &enabled, "", None).0, 0);
    }

    #[test]
    fn a_list_entry_cannot_add_a_token_the_allowlist_omits() {
        // Uniswap's own sepolia list names 0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14.
        assert!(find(11_155_111, UNI_SEPOLIA_WETH, &[]).is_none());
        assert_eq!(for_chain(11_155_111, &[]).len(), 1); // ETH only

        let listed = meta_of(&[json!({
            "chainId": 11_155_111, "address": UNI_SEPOLIA_WETH,
            "symbol": "WETH", "name": "Wrapped Ether", "decimals": 18,
            "logoURI": "https://example.invalid/weth.png"
        })]);
        let out = enrich(11_155_111, &for_chain(11_155_111, &[]), &listed);
        assert_eq!(out.len(), 1, "the list must not add a row");
        assert_eq!(out[0]["symbol"], "ETH");
    }

    #[test]
    fn a_matching_entry_decorates_and_a_wrong_chain_one_does_not() {
        let mainnet = for_chain(1, &[]);
        let good = meta_of(&[json!({
            "chainId": 1, "address": WETH_MAINNET, "symbol": "WETH",
            "logoURI": "https://example.invalid/weth.png"
        })]);
        let out = enrich(1, &mainnet, &good);
        let weth = out.iter().find(|t| t["symbol"] == "WETH").unwrap();
        assert_eq!(weth["inTokenList"], json!(true));
        assert_eq!(weth["logoURI"], "https://example.invalid/weth.png");
        // Native ETH has no address, so it can never match.
        assert_eq!(out[0]["inTokenList"], json!(false));
        assert!(out[0].get("logoURI").is_none());

        // Same address, different chain: the entry is for another network.
        let wrong_chain = meta_of(&[json!({
            "chainId": 10, "address": WETH_MAINNET, "logoURI": "https://example.invalid/op.png"
        })]);
        let out = enrich(1, &mainnet, &wrong_chain);
        let weth = out.iter().find(|t| t["symbol"] == "WETH").unwrap();
        assert_eq!(weth["inTokenList"], json!(false));
        assert!(weth.get("logoURI").is_none());
    }

    #[test]
    fn a_list_disagreeing_on_decimals_or_symbol_never_overwrites_the_allowlist() {
        // Adopting a wrong `decimals` mis-scales every displayed balance by a power of ten.
        let mainnet = for_chain(1, &[]);
        let lying = meta_of(&[json!({
            "chainId": 1, "address": WETH_MAINNET,
            "symbol": "WETH9", "name": "Not Wrapped Ether", "decimals": 6
        })]);
        let weth = enrich(1, &mainnet, &lying)
            .into_iter()
            .find(|t| t["address"] == WETH_MAINNET)
            .unwrap();
        assert_eq!(weth["symbol"], "WETH");
        assert_eq!(weth["name"], "Wrapped Ether");
        assert_eq!(weth["decimals"], 18);
        assert_eq!(weth["inTokenList"], json!(true));
    }

    #[test]
    fn metadata_source_discriminates_where_the_boolean_could_not() {
        // `inTokenList` is false for the native currency on every chain forever — it has no
        // contract to match against — so the boolean alone told the user nothing.
        let mainnet = for_chain(1, &[]);
        let out = enrich(1, &mainnet, &HashMap::new());
        assert_eq!(out[0]["metadataSource"], json!("native"));
        assert_eq!(out[0]["inTokenList"], json!(false));
        let weth = out.iter().find(|t| t["symbol"] == "WETH").unwrap();
        assert_eq!(weth["metadataSource"], json!("allowlist"), "named by us, decorated by nothing");

        // A labelled match reports the bucket token_list named, not "the list matched".
        let listed = meta_of(&[json!({
            "chainId": 1, "address": WETH_MAINNET, "symbol": "WETH",
            "name": "Wrapped Ether", "decimals": 18, "source": "embedded"
        })]);
        let weth = enrich(1, &mainnet, &listed)
            .into_iter()
            .find(|t| t["symbol"] == "WETH")
            .unwrap();
        assert_eq!(weth["metadataSource"], json!("embedded"));
    }

    #[test]
    fn every_bucket_label_is_relayed_and_an_unlabelled_match_is_never_guessed() {
        let mainnet = for_chain(1, &[]);
        for bucket in ["custom", "downloaded", "embedded"] {
            let listed = meta_of(&[json!({
                "chainId": 1, "address": WETH_MAINNET, "source": bucket
            })]);
            let weth = enrich(1, &mainnet, &listed)
                .into_iter()
                .find(|t| t["symbol"] == "WETH")
                .unwrap();
            assert_eq!(weth["metadataSource"], json!(bucket));
        }
        // A token_list that predates bucket labelling, and one that invents a bucket: both
        // matched, so "allowlist" would be a lie and any bucket name would be a guess.
        for entry in [json!({ "chainId": 1, "address": WETH_MAINNET }),
                      json!({ "chainId": 1, "address": WETH_MAINNET, "source": "sideloaded" })] {
            let weth = enrich(1, &mainnet, &meta_of(&[entry]))
                .into_iter()
                .find(|t| t["symbol"] == "WETH")
                .unwrap();
            assert_eq!(weth["metadataSource"], json!("unknown"));
            assert_eq!(weth["inTokenList"], json!(true));
        }
    }

    #[test]
    fn the_metadata_query_names_every_contract_row_and_nothing_else() {
        // Asking by address is what keeps a mainnet list_tokens from dragging back 401 rows
        // to decorate two. The native row has no contract, so it is never asked about.
        let q = meta_query(&for_chain(1, &[])).unwrap();
        assert_eq!(q, format!("[\"{WETH_MAINNET}\"]"));
        // An enabled token is asked about too — it is a contract row like any other.
        let q = meta_query(&for_chain(1, &[tok("USDC", "USD Coin", 6, USDC)])).unwrap();
        assert_eq!(q, format!("[\"{WETH_MAINNET}\",\"{USDC}\"]"));
        // ETH-only chains ask nothing at all rather than sending an empty array.
        assert!(meta_query(&for_chain(11_155_111, &[])).is_none());
        assert!(meta_query(&for_chain(560_048, &[])).is_none());
        assert!(meta_query(&[]).is_none());
    }

    #[test]
    fn metadata_source_is_native_for_the_chains_own_currency() {
        for net in networks::ALL {
            let out = enrich(net.chain_id, &for_chain(net.chain_id, &[]), &HashMap::new());
            assert_eq!(out[0]["metadataSource"], json!("native"), "on {}", net.key);
        }
    }

    #[test]
    fn a_list_entry_still_cannot_overrule_the_allowlist() {
        // Arbitrary list metadata changes nothing but the match; every field still comes
        // from the allowlist, and an entry for another chain is not even a match.
        let listed = meta_of(&[json!({
            "chainId": 1, "address": WETH_MAINNET, "name": "Wrapped Ether",
            "symbol": "WETH", "decimals": 18, "source": "embedded"
        })]);
        let out = enrich(11_155_111, &for_chain(11_155_111, &[]), &listed);
        assert_eq!(out.len(), 1, "mainnet's WETH must not appear on sepolia");
        assert_eq!(out[0]["symbol"], json!("ETH"));
        assert_eq!(out[0]["metadataSource"], json!("native"));
    }

    #[test]
    fn an_empty_map_changes_nothing_but_the_in_list_flag() {
        for net in networks::ALL {
            let list = for_chain(net.chain_id, &[]);
            let out = enrich(net.chain_id, &list, &HashMap::new());
            for (t, v) in list.iter().zip(out.iter()) {
                let mut expected = serde_json::to_value(t).unwrap();
                expected["inTokenList"] = json!(false);
                expected["metadataSource"] = json!(if t.native { "native" } else { "allowlist" });
                expected["builtin"] = json!(true);
                assert_eq!(v, &expected, "{} on {}", t.symbol, net.key);
            }
        }
    }

    // ── the enabled set ──────────────────────────────────────────────────────────────

    #[test]
    fn the_default_offer_is_exactly_native_plus_the_verified_weth() {
        // What the wallet ships with nothing enabled. Mainnet's WETH is verified; the two
        // testnets' are not, so they offer their native currency alone.
        assert_eq!(symbols(&enrich(1, &for_chain(1, &[]), &HashMap::new())), ["ETH", "WETH"]);
        for id in [11_155_111u64, 560_048] {
            assert_eq!(symbols(&enrich(id, &for_chain(id, &[]), &HashMap::new())), ["ETH"]);
        }
        for net in networks::ALL {
            assert!(
                for_chain(net.chain_id, &[]).iter().all(|t| is_builtin(
                    net.chain_id,
                    t.address.as_deref().unwrap_or_default()
                ) || t.native),
                "the default offer is built-in rows only, on {}",
                net.key
            );
        }
    }

    #[test]
    fn an_enabled_token_joins_the_offer_and_the_send_validator_accepts_it() {
        // `find` IS the send path's validator: a token it refuses cannot be sent, so this is
        // the assertion that an enabled token is actually usable.
        let enabled = [tok("USDC", "USD Coin", 6, USDC)];
        assert_eq!(symbols(&enrich(1, &for_chain(1, &enabled), &HashMap::new())), ["ETH", "WETH", "USDC"]);
        let by_symbol = find(1, "usdc", &enabled).expect("the send path must resolve it");
        assert_eq!(by_symbol.decimals, 6, "the snapshot's own scale, not a guess");
        assert_eq!(find(1, USDC, &enabled).unwrap(), by_symbol, "and by address");
        // On a chain it was not enabled for, it is not offered and cannot be sent.
        assert!(find(11_155_111, USDC, &[]).is_none());
    }

    #[test]
    fn an_enabled_token_duplicating_a_builtin_address_yields_one_row() {
        // Cased differently, which is the shape a hand-typed or list-sourced address arrives
        // in. A second row would double-count the balance and offer two identical picker rows.
        let dupe = [tok("WETH9", "Not Wrapped Ether", 6, &WETH_MAINNET.to_lowercase())];
        let list = for_chain(1, &dupe);
        assert_eq!(offered(1, &dupe), ["ETH", "WETH"]);
        // And the built-in wins every field: a 6-decimal WETH mis-scales by 10^12.
        let weth = list.iter().find(|t| !t.native).unwrap();
        assert_eq!((weth.symbol.as_str(), weth.decimals), ("WETH", 18));
        assert_eq!(find(1, WETH_MAINNET, &dupe).unwrap().decimals, 18);
    }

    #[test]
    fn a_repeated_or_structurally_impossible_enabled_row_is_dropped() {
        let enabled = [
            tok("USDC", "USD Coin", 6, USDC),
            tok("USDC", "USD Coin", 6, &USDC.to_lowercase()),
            Token { address: None, ..tok("GHOST", "No Address", 18, USDC) },
            Token { native: true, ..tok("ETH2", "Second Ether", 18, DAI) },
        ];
        assert_eq!(symbols(&enrich(1, &for_chain(1, &enabled), &HashMap::new())), ["ETH", "WETH", "USDC"]);
        assert_eq!(for_chain(1, &enabled).iter().filter(|t| t.native).count(), 1);
    }

    fn both_lits() -> [Token; 2] {
        [tok("LIT", "Litentry", 18, LIT_LITENTRY), tok("LIT", "Lighter", 18, LIT_LIGHTER)]
    }

    #[test]
    fn a_symbol_two_contracts_share_is_refused_rather_than_guessed() {
        let enabled = both_lits();
        // Membership is not the bug: two contracts ARE two tokens and both stay offered.
        assert_eq!(for_chain(1, &enabled).len(), 4, "ETH, WETH and both LITs");

        let e = resolve(1, "LIT", &enabled).unwrap_err();
        for a in [LIT_LITENTRY, LIT_LIGHTER] {
            assert!(e.contains(a), "the refusal must name every candidate: {e}");
        }
        // A first match here is a send of the asset the user did not mean.
        assert!(find(1, "LIT", &enabled).is_none(), "and no silent first match");
        assert!(resolve(1, "lit", &enabled).is_err(), "case is not what disambiguates");
    }

    #[test]
    fn an_address_resolves_to_exactly_one_of_them() {
        let enabled = both_lits();
        for (addr, name) in [(LIT_LITENTRY, "Litentry"), (LIT_LIGHTER, "Lighter")] {
            let t = resolve(1, addr, &enabled).expect("an address is an identity");
            assert_eq!((t.name.as_str(), t.address.as_deref()), (name, Some(addr)));
            assert_eq!(by_address(1, addr, &enabled).unwrap(), t);
            // Case-folded: an address reaches this wallet in the node's lowercase too.
            assert_eq!(resolve(1, &addr.to_lowercase(), &enabled).unwrap(), t);
        }
        // An address the wallet does not offer is refused, not resolved by its symbol.
        assert!(resolve(1, DAI, &enabled).is_err());
        // `enabled` is already one chain's set, so a chain that never enabled it offers nothing.
        assert!(resolve(11_155_111, LIT_LIGHTER, &[]).is_err(), "nor on a chain it is not on");
    }

    #[test]
    fn a_symbol_only_one_contract_carries_still_resolves() {
        let enabled = [tok("USDC", "USD Coin", 6, USDC), both_lits()[0].clone()];
        let t = resolve(1, "usdc", &enabled).expect("one contract, one answer");
        assert_eq!((t.decimals, t.address.as_deref()), (6, Some(USDC)));
        // The other symbol on the same chain being ambiguous does not spread.
        assert_eq!(resolve(1, "LIT", &enabled).unwrap().name, "Litentry");
    }

    #[test]
    fn a_builtin_symbol_is_never_ambiguous() {
        // The wallet's own assertion wins its symbol outright, so an enabled contract
        // calling itself WETH cannot make a plain WETH send undeliverable.
        let enabled = [tok("WETH", "Wrapped Ether", 18, DAI)];
        let t = resolve(1, "WETH", &enabled).unwrap();
        assert_eq!(t.address.as_deref(), Some(WETH_MAINNET));
        assert_eq!(resolve(1, DAI, &enabled).unwrap().address.as_deref(), Some(DAI));
    }

    #[test]
    fn an_enabled_token_cannot_shadow_the_native_symbol() {
        // The phishing shape: a contract calling itself ETH. A plain send names "ETH", and
        // resolving that to a contract would build an ERC-20 transfer to the wrong place.
        let liar = [tok("ETH", "Ether", 18, DAI)];
        let t = find(1, "ETH", &liar).unwrap();
        assert!(t.native && t.address.is_none());
        // It is still offered under its own address, which is the honest half.
        assert_eq!(find(1, DAI, &liar).unwrap().address.as_deref(), Some(DAI));
    }

    #[test]
    fn is_builtin_names_exactly_the_rows_that_cannot_be_turned_off() {
        assert!(is_builtin(1, WETH_MAINNET));
        assert!(is_builtin(1, &WETH_MAINNET.to_lowercase()), "case is not provenance");
        assert!(!is_builtin(1, USDC));
        // Mainnet's WETH is not a built-in anywhere else, and the testnets have none at all.
        assert!(!is_builtin(11_155_111, WETH_MAINNET));
        assert!(!is_builtin(560_048, WETH_MAINNET));
        assert!(!is_builtin(999, WETH_MAINNET));
        // The native row has no address, so no address can name it — which is why the glue
        // refuses an unparseable address before it ever asks.
        assert!(!is_builtin(1, ""));
    }

    // ── the picker ───────────────────────────────────────────────────────────────────

    #[test]
    fn the_picker_puts_native_first_then_enabled_then_the_rest() {
        let enabled = [tok("USDC", "USD Coin", 6, USDC)];
        let list = [
            listed(1, "DAI", "Dai Stablecoin", 18, DAI),
            listed(1, "USDC", "USD Coin", 6, USDC),
            listed(1, "WETH", "Wrapped Ether", 18, WETH_MAINNET),
        ];
        let (total, rows) = available(1, &list, &enabled, "", None);
        // Three listed and three offered, but WETH is both: the union is four rows, not six.
        assert_eq!(total, 4);
        assert_eq!(symbols(&rows), ["ETH", "USDC", "WETH", "DAI"]);
        assert_eq!(
            rows.iter().map(|r| r["enabled"] == json!(true)).collect::<Vec<_>>(),
            [true, true, true, false]
        );
        assert_eq!(
            rows.iter().map(|r| r["builtin"] == json!(true)).collect::<Vec<_>>(),
            [true, false, true, false],
            "only the native row and the verified WETH can never be turned off"
        );
        // Provenance, per row: ours, the user's snapshot decorated by the list, ours, theirs.
        assert_eq!(
            rows.iter().map(|r| r["source"].as_str().unwrap()).collect::<Vec<_>>(),
            ["native", "embedded", "embedded", "embedded"]
        );
    }

    #[test]
    fn the_picker_matches_a_symbol_a_name_or_an_exact_address() {
        let list = [
            listed(1, "DAI", "Dai Stablecoin", 18, DAI),
            listed(1, "USDC", "USD Coin", 6, USDC),
        ];
        let q = |s: &str| symbols(&available(1, &list, &[], s, None).1);
        assert_eq!(q("dai"), ["DAI"], "symbol, case-insensitively");
        assert_eq!(q("stablecoin"), ["DAI"], "a name substring");
        assert_eq!(q(&DAI.to_lowercase()), ["DAI"], "an exact address, case-insensitively");
        assert_eq!(q("usd"), ["USDC"], "matches USD Coin by name");
        assert_eq!(q(""), ["ETH", "WETH", "DAI", "USDC"], "an empty query hides nothing");
        assert!(q("0x0000000000000000000000000000000000000001").is_empty(), "a near-miss address is a miss");
        assert!(q("nosuchtoken").is_empty());
    }

    #[test]
    fn the_picker_total_counts_the_matches_before_the_limit() {
        // `total` vs `shown` is what lets the screen say what it is hiding rather than
        // presenting a truncated list as the whole answer.
        let list: Vec<Value> = (0..12u64)
            .map(|i| listed(1, &format!("T{i:02}"), "Token", 18, &format!("0x{:040x}", i + 1)))
            .collect();
        let (total, rows) = available(1, &list, &[], "", Some(5));
        assert_eq!((total, rows.len()), (14, 5), "12 listed plus ETH and WETH");
        assert_eq!(symbols(&rows), ["ETH", "WETH", "T00", "T01", "T02"]);
        // No limit shows everything, and a limit past the end cuts nothing.
        assert_eq!(available(1, &list, &[], "", None).1.len(), 14);
        assert_eq!(available(1, &list, &[], "", Some(99)).1.len(), 14);
        // The count follows the query, not the catalogue.
        assert_eq!(available(1, &list, &[], "T0", Some(2)), {
            let (_, r) = available(1, &list, &[], "T0", Some(2));
            (10, r)
        });
    }

    #[test]
    fn a_chain_the_list_holds_nothing_for_is_an_empty_result_not_a_failure() {
        // The embedded Uniswap list is overwhelmingly mainnet, so this is the ORDINARY case
        // on sepolia and hoodi. It must read as "the list has none", not as a broken call.
        let mainnet_only = [listed(1, "DAI", "Dai Stablecoin", 18, DAI)];
        for id in [11_155_111u64, 560_048] {
            let (total, rows) = available(id, &mainnet_only, &[], "", None);
            assert_eq!((total, symbols(&rows)), (1, vec!["ETH".to_string()]));
            assert!(rows[0]["builtin"] == json!(true) && rows[0]["enabled"] == json!(true));
        }
        // And a query with no hits is an empty list, which is an answer.
        assert_eq!(available(1, &[], &[], "nosuch", None), (0, Vec::new()));
    }

    #[test]
    fn a_listed_row_missing_a_field_this_wallet_would_have_to_invent_is_dropped() {
        // `decimals` scales every amount rendered or sent, and a bad address is unspendable.
        let broken = [
            json!({ "chainId": 1, "address": DAI, "symbol": "DAI", "name": "Dai" }),
            json!({ "chainId": 1, "address": "0xnothex", "symbol": "BAD", "decimals": 18 }),
            json!({ "chainId": 1, "address": USDC, "decimals": 6 }),
            json!({ "chainId": 1, "address": USDC, "symbol": "", "decimals": 6 }),
            json!({ "chainId": 1, "symbol": "NOADDR", "decimals": 18 }),
        ];
        assert_eq!(symbols(&available(1, &broken, &[], "", None).1), ["ETH", "WETH"]);
        // A row with no `name` is not missing anything — it falls back to its own symbol.
        let nameless = [json!({ "chainId": 1, "address": DAI, "symbol": "DAI", "decimals": 18 })];
        let rows = available(1, &nameless, &[], "", None).1;
        let dai = rows.iter().find(|r| r["symbol"] == "DAI").unwrap();
        assert_eq!(dai["name"], json!("DAI"));
    }

    #[test]
    fn an_enabled_row_the_list_no_longer_holds_says_where_it_came_from() {
        // The snapshot is why this is possible at all: the token stays usable, and the row
        // reports `enabled` rather than claiming a bucket vouches for it.
        let enabled = [tok("USDC", "USD Coin", 6, USDC)];
        let rows = available(1, &[], &enabled, "", None).1;
        let usdc = rows.iter().find(|r| r["symbol"] == "USDC").unwrap();
        assert_eq!(usdc["source"], json!("enabled"));
        assert_eq!((usdc["enabled"].clone(), usdc["builtin"].clone()), (json!(true), json!(false)));
        assert_eq!(usdc["decimals"], json!(6), "the snapshot still carries its scale");
        // And `list_tokens` says the same thing about the same row.
        let out = enrich(1, &for_chain(1, &enabled), &HashMap::new());
        let usdc = out.iter().find(|t| t["symbol"] == "USDC").unwrap();
        assert_eq!(usdc["metadataSource"], json!("enabled"));
        assert_eq!(usdc["builtin"], json!(false));
    }

    #[test]
    fn a_snapshot_is_taken_only_from_a_row_the_list_actually_holds() {
        // THE gate on the enabled set: `None` here is what refuses the enable, so nothing is
        // stored and no `decimals` is invented.
        let list = [listed(1, "USDC", "USD Coin", 6, USDC)];
        let got = snapshot_of(1, &USDC.to_lowercase(), &list).expect("a held row is snapshotted");
        assert_eq!(got, tok("USDC", "USD Coin", 6, USDC));
        assert_eq!(got.decimals, 6, "the list's own scale, carried whole");

        assert!(snapshot_of(1, DAI, &list).is_none(), "an address the list does not hold");
        assert!(snapshot_of(11_155_111, USDC, &list).is_none(), "the same address, another chain");
        assert!(snapshot_of(1, USDC, &[]).is_none(), "an empty catalogue holds nothing");
        assert!(snapshot_of(1, "not an address", &list).is_none());
    }

    #[test]
    fn a_listed_row_this_wallet_would_have_to_complete_is_never_snapshotted() {
        // A row with no decimals is the dangerous one: assume 18 for a 6-decimal token and
        // every balance and every send amount is out by a factor of a million.
        for entry in [
            json!({ "chainId": 1, "address": USDC, "symbol": "USDC", "name": "USD Coin" }),
            json!({ "chainId": 1, "address": USDC, "symbol": "USDC", "decimals": 300 }),
            json!({ "chainId": 1, "address": USDC, "decimals": 6 }),
        ] {
            assert!(snapshot_of(1, USDC, &[entry.clone()]).is_none(), "{entry}");
        }
    }

    // ── the balance order ────────────────────────────────────────────────────────────

    #[test]
    fn an_enabled_token_with_no_balance_still_gets_a_row() {
        // The regression this is about: filtering zeros out of the balance list makes a token
        // the user just turned on vanish, which reads as the wallet having lost it.
        let enabled = [tok("USDC", "USD Coin", 6, USDC)];
        let offered = for_chain(1, &enabled);
        let rows = balance_rows(1, &offered, &[Some("0".into()), Some("0".into()), Some("0".into())], TokenSort::Alpha);
        assert_eq!(symbols(&rows), ["ETH", "USDC", "WETH"]);
        let usdc = rows.iter().find(|r| r["symbol"] == "USDC").unwrap();
        assert_eq!((usdc["raw"].clone(), usdc["display"].clone()), (json!("0"), json!("0")));
        assert_eq!(usdc["amountExact"], json!("0"), "an exact zero is still an exact figure");
        assert_eq!(usdc["builtin"], json!(false), "and it can be turned off again");
    }

    #[test]
    fn a_balance_row_carries_every_digit_and_a_failed_leg_carries_none() {
        // 6-decimal USDC and 18-decimal ETH off the same read: one wrong `decimals` here is a
        // balance wrong by a factor of 10^12, which is why the snapshot stores the scale.
        let enabled = [tok("USDC", "USD Coin", 6, USDC)];
        let offered = for_chain(1, &enabled);
        let raw = [Some("1234567890123456789".into()), None, Some("1234567".into())];
        let rows = balance_rows(1, &offered, &raw, TokenSort::Alpha);
        let by = |s: &str| rows.iter().find(|r| r["symbol"] == s).unwrap().clone();
        assert_eq!(by("ETH")["amountExact"], json!("1.234567890123456789"));
        assert_eq!(by("ETH")["display"], json!("1.23456"), "bounded, and truncated not rounded");
        assert_eq!(by("USDC")["amountExact"], json!("1.234567"));
        // The WETH leg failed: no figure at all, so the view renders an em-dash.
        let weth = by("WETH");
        assert_eq!(weth["raw"], json!(""));
        for k in ["display", "exact", "amountExact"] {
            assert!(weth.get(k).is_none(), "{k} must be absent, never a zero");
        }
        // `exact` is the older name for the same digits; both must agree, always.
        for r in &rows {
            assert_eq!(r.get("exact"), r.get("amountExact"));
        }
    }

    #[test]
    fn the_balance_rows_come_back_already_in_the_persisted_order() {
        let enabled = [tok("USDC", "USD Coin", 6, USDC), tok("DAI", "Dai Stablecoin", 18, DAI)];
        let offered = for_chain(1, &enabled);
        assert_eq!(offered.iter().map(|t| t.symbol.as_str()).collect::<Vec<_>>(), ["ETH", "WETH", "USDC", "DAI"]);
        // Positional: `raw[i]` is the leg for `offered[i]`, so the sort must not shuffle a
        // figure onto another token's row.
        let raw = [Some("0".into()), Some("0".into()), Some("5000000".into()), Some("0".into())];
        let rows = balance_rows(1, &offered, &raw, TokenSort::Balance);
        assert_eq!(symbols(&rows), ["ETH", "USDC", "DAI", "WETH"]);
        assert_eq!(rows[1]["amountExact"], json!("5"), "5 USDC at six decimals, on the USDC row");
        assert_eq!(symbols(&balance_rows(1, &offered, &raw, TokenSort::Alpha)), ["ETH", "DAI", "USDC", "WETH"]);
    }

    #[test]
    fn token_sort_parses_the_two_orders_it_has_and_nothing_else() {
        assert_eq!(TokenSort::parse(" Alpha "), Some(TokenSort::Alpha));
        assert_eq!(TokenSort::parse("BALANCE"), Some(TokenSort::Balance));
        assert_eq!(TokenSort::default(), TokenSort::Alpha);
        for bad in ["", "value", "usd", "desc", "alphabetical"] {
            assert_eq!(TokenSort::parse(bad), None, "{bad:?} must not become a silent default");
        }
        assert_eq!(TokenSort::parse(TokenSort::Balance.as_str()), Some(TokenSort::Balance));
    }

    #[test]
    fn alpha_order_is_case_insensitive_and_keeps_the_native_row_first() {
        let mut rows = vec![
            balance("weth", "0", false),
            balance("aave", "5", false),
            balance("ETH", "0", true),
            balance("USDC", "7", false),
        ];
        sort_balance_rows(&mut rows, TokenSort::Alpha);
        assert_eq!(symbols(&rows), ["ETH", "aave", "USDC", "weth"]);
    }

    #[test]
    fn balance_order_is_non_zero_first_then_descending_with_an_alphabetical_tie() {
        // NOT a value order: with no price feed these are four different tokens' own amounts.
        let mut rows = vec![
            balance("ZRX", "0", false),
            balance("USDC", "100", false),
            balance("AAVE", "0", false),
            // 18-decimal amounts a double could not tell apart — the compare is exact U256.
            balance("DAI", "1000000000000000000000001", false),
            balance("LINK", "1000000000000000000000000", false),
            balance("BAT", "100", false),
            balance("ETH", "0", true),
        ];
        sort_balance_rows(&mut rows, TokenSort::Balance);
        assert_eq!(symbols(&rows), ["ETH", "DAI", "LINK", "BAT", "USDC", "AAVE", "ZRX"]);
    }

    #[test]
    fn an_unread_balance_sorts_between_the_non_zero_and_the_zero() {
        // A failed sub-call carries no figure. "We could not read it" is not "you have none",
        // and burying it under the zeros is exactly how that distinction gets lost.
        let mut rows = vec![
            balance("ZZZ", "0", false),
            balance("AAA", "", false),
            balance("BBB", "5", false),
            balance("ETH", "not a number", true),
        ];
        sort_balance_rows(&mut rows, TokenSort::Balance);
        assert_eq!(symbols(&rows), ["ETH", "BBB", "AAA", "ZZZ"]);
        // Alpha does not care how much is there, only what it is called.
        sort_balance_rows(&mut rows, TokenSort::Alpha);
        assert_eq!(symbols(&rows), ["ETH", "AAA", "BBB", "ZZZ"]);
    }

    #[test]
    fn both_orders_are_total_so_a_re_sort_never_moves_a_row() {
        // Two rows equal on every key the sort reads would make the reply order depend on
        // Multicall3's, and the screen would reshuffle between two identical reads.
        let mut rows = vec![
            balance("USDC", "5", false),
            balance("usdc", "5", false),
            balance("ETH", "1", true),
        ];
        rows[0]["address"] = json!(USDC);
        rows[1]["address"] = json!(DAI);
        for order in [TokenSort::Alpha, TokenSort::Balance] {
            sort_balance_rows(&mut rows, order);
            let once = symbols(&rows);
            rows.reverse();
            sort_balance_rows(&mut rows, order);
            assert_eq!(symbols(&rows), once, "{} is not a total order", order.as_str());
            assert_eq!(rows[1]["address"], json!(DAI), "the tie breaks on the address, ascending");
        }
    }
}
