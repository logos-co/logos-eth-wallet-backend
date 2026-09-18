//! Portfolio scoping for the sender's all-chain history reply, and what its decoration reports.

use std::collections::BTreeSet;

use serde_json::{json, Value};

fn chain_id(row: &Value) -> Option<u64> {
    row.get("chainId").and_then(Value::as_u64).or_else(|| row.as_u64())
}

/// Keep only rows in `allowed`, preserving the sender's all-chain status separately while
/// recomputing the timer condition for the rows the portfolio actually shows.
pub fn scope(mut history: Value, allowed: &BTreeSet<u64>) -> Value {
    if let Some(rows) = history.get_mut("transactions").and_then(Value::as_array_mut) {
        rows.retain(|row| chain_id(row).is_some_and(|id| allowed.contains(&id)));
    }
    if let Some(rows) = history.get_mut("blockedChains").and_then(Value::as_array_mut) {
        rows.retain(|row| chain_id(row).is_some_and(|id| allowed.contains(&id)));
    }

    let shown: BTreeSet<(String, u64)> = history.get("transactions").and_then(Value::as_array)
        .into_iter().flatten().filter_map(|row| {
            Some((row.get("requestId")?.as_str()?.to_string(), row.get("leg")?.as_u64()?))
        }).collect();
    if let Some(rows) = history.get_mut("unresolved").and_then(Value::as_array_mut) {
        rows.retain(|row| {
            let Some(request) = row.get("requestId").and_then(Value::as_str) else { return false };
            let Some(leg) = row.get("leg").and_then(Value::as_u64) else { return false };
            shown.contains(&(request.to_string(), leg))
        });
    }

    let still_due = history.get("transactions").and_then(Value::as_array)
        .into_iter().flatten().any(|row| {
            row.get("status").and_then(Value::as_str) == Some("pending")
                && row.get("stalled").and_then(Value::as_bool) != Some(true)
        });
    history["stillDue"] = json!(still_due);
    if let Some(object) = history.as_object_mut() { object.remove("chainId"); }
    history
}

/// Every chain the rows name, once each, in the order they first appear.
pub fn chains(history: &Value) -> Vec<u64> {
    let mut out = Vec::new();
    let rows = history.get("transactions").and_then(Value::as_array).into_iter().flatten();
    for id in rows.filter_map(chain_id) {
        if !out.contains(&id) { out.push(id); }
    }
    out
}

/// Report the chains whose offered tokens went unread in the decorated reply's
/// `decorationErrors`, one entry per chain as when evm_assets read them. A refusal is relayed.
pub fn add_decoration_errors(reply: String, unread: Vec<Value>) -> String {
    if unread.is_empty() {
        return reply;
    }
    let Ok(mut v) = serde_json::from_str::<Value>(&reply) else { return reply };
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return reply;
    }
    let mut errors = v.get("decorationErrors").and_then(Value::as_array).cloned().unwrap_or_default();
    for e in unread {
        // A chain evm_assets could not decorate at all keeps its own, earlier reason.
        if !errors.iter().any(|r| r.get("chainId") == e.get("chainId")) {
            errors.push(e);
        }
    }
    v["decorationErrors"] = json!(errors);
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_history_shape_is_scoped_and_the_timer_is_recomputed() {
        let value = json!({
            "ok": true, "chainId": 0, "stillDue": true, "stillDueAnyChain": true,
            "transactions": [
                {"chainId": 1, "requestId": "main", "leg": 0, "status": "confirmed", "stalled": false},
                {"chainId": 11155111, "requestId": "test", "leg": 1, "status": "pending", "stalled": false}
            ],
            "blockedChains": [{"chainId": 11155111}, {"chainId": 1}],
            "unresolved": [
                {"requestId": "main", "leg": 0}, {"requestId": "test", "leg": 1}
            ]
        });
        let out = scope(value, &BTreeSet::from([1]));
        assert!(out.get("chainId").is_none(), "the reply spans the portfolio");
        assert_eq!(out["transactions"].as_array().unwrap().len(), 1);
        assert_eq!(out["blockedChains"].as_array().unwrap(), &[json!({"chainId":1})]);
        assert_eq!(out["unresolved"].as_array().unwrap(), &[json!({"requestId":"main","leg":0})]);
        assert_eq!(out["stillDue"], json!(false), "an out-of-scope pending row cannot keep the timer alive");
        assert_eq!(out["stillDueAnyChain"], json!(true), "the sender's wider fact remains explicit");
    }

    #[test]
    fn a_live_row_inside_scope_keeps_the_timer_running() {
        let out = scope(json!({"transactions":[
            {"chainId":1,"status":"pending","stalled":false}
        ]}), &BTreeSet::from([1]));
        assert_eq!(out["stillDue"], json!(true));
    }

    #[test]
    fn each_chain_the_rows_name_is_listed_once_in_row_order() {
        let value = json!({"transactions":[{"chainId":10},{"chainId":1},{"chainId":10}],
                           "blockedChains":[{"chainId":5}]});
        assert_eq!(chains(&value), [10, 1], "blocked chains carry no rows to decorate");
        assert!(chains(&json!({"ok":true})).is_empty());
    }

    #[test]
    fn a_chain_whose_offered_tokens_went_unread_is_reported_once() {
        let decorated = json!({"ok":true,"transactions":[],
                               "decorationErrors":[{"chainId":10,"error":"no record"}]});
        let unread = vec![json!({"chainId":10,"error":"token_list_module: down"}),
                          json!({"chainId":1,"error":"token_list_module: down"})];
        let out: Value =
            serde_json::from_str(&add_decoration_errors(decorated.to_string(), unread)).unwrap();
        assert_eq!(out["decorationErrors"], json!([{"chainId":10,"error":"no record"},
                                                   {"chainId":1,"error":"token_list_module: down"}]));

        let clean = json!({"ok":true,"transactions":[]}).to_string();
        assert_eq!(add_decoration_errors(clean.clone(), vec![]), clean, "nothing unread, verbatim");
        let refused = json!({"ok":false,"error":"invalid history"}).to_string();
        let unread = vec![json!({"chainId":1,"error":"x"})];
        assert_eq!(add_decoration_errors(refused.clone(), unread), refused, "a refusal stays one");
    }
}
