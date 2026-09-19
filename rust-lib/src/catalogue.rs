//! The token picker's page: `token_list_module`'s catalogue with the chain's native row in
//! slot 0 of page 0. The same merge `uniswap_backend` makes, so the two pickers agree.

use serde_json::{json, Value};

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Whether the native row answers a picker query: symbol or name, case-insensitive substring.
pub fn native_matches(native: &Value, query: &str) -> bool {
    let needle = query.trim().to_ascii_lowercase();
    needle.is_empty()
        || str_of(native, "symbol").to_ascii_lowercase().contains(&needle)
        || str_of(native, "name").to_ascii_lowercase().contains(&needle)
}

/// token_list's offset for the picker's `offset`: the native row takes slot 0 of page 0.
pub fn provider_offset(native_matches: bool, offset: i64) -> i64 {
    let offset = offset.max(0);
    if native_matches { (offset - 1).max(0) } else { offset }
}

/// token_list's page with the native row merged in, counts included.
pub fn merge_native_page(
    native: &Value,
    native_matches: bool,
    mut page: Value,
    offset: i64,
    limit: i64,
) -> Value {
    let offset = usize::try_from(offset).unwrap_or(0);
    let provider_total = page.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;
    let mut rows = page
        .get_mut("tokens")
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .unwrap_or_default();
    for row in &mut rows {
        row["native"] = json!(false);
    }
    if native_matches && offset == 0 && limit != 0 {
        rows.insert(0, native.clone());
    }
    if let Ok(cut) = usize::try_from(limit) {
        if cut > 0 {
            rows.truncate(cut);
        }
    }
    let total = provider_total + usize::from(native_matches);
    page["total"] = json!(total);
    page["offset"] = json!(offset);
    page["shown"] = json!(rows.len());
    page["hasMore"] = json!(offset.saturating_add(rows.len()) < total);
    page["tokens"] = json!(rows);
    page
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native() -> Value {
        json!({ "symbol": "ETH", "name": "Ether", "decimals": 18, "native": true })
    }

    /// What token_list answers over a catalogue of `n` rows, `C0`..: its own paging, no native.
    fn provider(n: usize, offset: i64, limit: i64) -> Value {
        let offset = usize::try_from(offset).unwrap_or(0);
        let mut rows: Vec<Value> = (offset..n).map(|i| json!({ "symbol": format!("C{i}") })).collect();
        rows.truncate(usize::try_from(limit).ok().filter(|l| *l > 0).unwrap_or(rows.len()));
        json!({ "ok": true, "chainId": 1, "total": n, "offset": offset, "shown": rows.len(),
                "hasMore": offset + rows.len() < n, "listed": n, "tokens": rows })
    }

    /// The picker's page for `offset`/`limit`, merged the way the glue calls it.
    fn picker(n: usize, matches: bool, offset: i64, limit: i64) -> Value {
        let page = provider(n, provider_offset(matches, offset), limit);
        merge_native_page(&native(), matches, page, offset, limit)
    }

    fn symbols(page: &Value) -> Vec<&str> {
        page["tokens"].as_array().unwrap().iter().map(|t| t["symbol"].as_str().unwrap()).collect()
    }

    fn counts(page: &Value) -> (u64, u64, bool) {
        (page["total"].as_u64().unwrap(), page["shown"].as_u64().unwrap(),
         page["hasMore"].as_bool().unwrap())
    }

    #[test]
    fn the_native_row_takes_slot_zero_of_the_first_page_only() {
        let first = picker(5, true, 0, 3);
        assert_eq!(symbols(&first), ["ETH", "C0", "C1"]);
        assert_eq!(first["tokens"][0], native(), "the native row is relayed as evm_assets drew it");
        assert_eq!(first["tokens"][1]["native"], false, "a catalogue row is marked non-native");
        let second = picker(5, true, 3, 3);
        assert_eq!(symbols(&second), ["C2", "C3", "C4"], "no row is lost or repeated at the seam");
        assert_eq!(second["offset"], 3, "the picker's own offset, not token_list's");
    }

    #[test]
    fn the_catalogue_shifts_one_slot_behind_a_matching_native_row() {
        assert_eq!(provider_offset(true, 0), 0);
        assert_eq!(provider_offset(true, 100), 99);
        assert_eq!(provider_offset(false, 100), 100);
        assert_eq!(provider_offset(true, -5), 0, "a negative offset is the first page");
        assert_eq!(provider_offset(false, -5), 0);
    }

    #[test]
    fn total_and_has_more_count_the_native_row() {
        assert_eq!(counts(&picker(5, true, 0, 3)), (6, 3, true));
        assert_eq!(counts(&picker(5, true, 3, 3)), (6, 3, false));
        assert_eq!(counts(&picker(0, true, 0, 3)), (1, 1, false), "native alone is an answer");
    }

    #[test]
    fn a_query_the_native_row_does_not_match_leaves_the_catalogue_as_token_list_paged_it() {
        assert!(native_matches(&native(), " eth "));
        assert!(native_matches(&native(), "THER"), "the name answers too");
        assert!(!native_matches(&native(), "usd"));
        let page = picker(4, false, 0, 3);
        assert_eq!(symbols(&page), ["C0", "C1", "C2"]);
        assert_eq!(counts(&page), (4, 3, true));
        assert_eq!(symbols(&picker(4, false, 3, 3)), ["C3"]);
    }
}
