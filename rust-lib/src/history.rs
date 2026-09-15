//! Portfolio scoping for the sender's all-chain history reply.

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
}
