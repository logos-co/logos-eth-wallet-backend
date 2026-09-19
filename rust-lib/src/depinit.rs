//! The consumer half of a dependency's defaults: whether its `init_defaults` answer settles it.
//! Each dependency owns its defaults and fills only what is absent, so the glue asks with no
//! `config_status` gate until one call answers `ok`.
//!
//! Kept out of the glue so it is exercised by `cargo test --no-default-features`.

use serde_json::Value;

/// A module answered `{ ok: true, ... }`. `init_defaults` answering `applied: false` is
/// such an answer — there was nothing left for it to fill, which is not a failure.
pub fn reply_ok(raw: &str) -> bool {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.get("ok").and_then(Value::as_bool))
        == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_already_applied_default_is_a_success_not_a_failure() {
        let second = json!({ "ok": true, "applied": false, "state": "configured",
                             "source": "external", "reason": "already configured" });
        assert!(reply_ok(&second.to_string()));
        assert!(reply_ok(&json!({ "ok": true, "applied": true }).to_string()));
        assert!(!reply_ok(&json!({ "ok": false, "error": "unready" }).to_string()));
        assert!(!reply_ok("nonsense"));
    }
}
