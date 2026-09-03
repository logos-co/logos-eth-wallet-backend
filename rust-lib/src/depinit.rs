//! The consumer half of the ask-then-initialize convention: what a dependency's
//! `config_status` answer tells this wallet to do next.
//!
//! Kept out of the glue so it is exercised by `cargo test --no-default-features`. The rule
//! that matters is the one a bool could not express: "I could not ask" and "I asked and it
//! has no config" are different answers, and only the second one licenses a write.

use serde_json::Value;

/// What to do after reading a dependency's `config_status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Nothing has ever been configured. Apply the dependency's own defaults, once.
    Initialize,
    /// A config is already set, or we just wrote one. Stop asking, permanently.
    Settled,
    /// The module has not finished starting, or did not answer intelligibly. Ask later —
    /// a call that did not arrive is not evidence of an empty config.
    AskAgain,
}

/// Read one `config_status` reply. Anything but a `state` this convention defines is
/// `AskAgain`: an unrecognised answer must never be read as permission to write.
pub fn next_step(status_json: &str) -> Next {
    let Ok(v) = serde_json::from_str::<Value>(status_json) else { return Next::AskAgain };
    match v.get("state").and_then(Value::as_str) {
        Some("unconfigured") => Next::Initialize,
        Some("configured") => Next::Settled,
        _ => Next::AskAgain,
    }
}

/// A module answered `{ ok: true, ... }`. `init_defaults` answering `applied: false` is
/// such an answer — another consumer got there first, which is a race won, not a failure.
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
    fn only_an_explicit_unconfigured_licenses_a_write() {
        assert_eq!(next_step(&json!({ "ok": true, "state": "unconfigured", "source": "none" }).to_string()),
                   Next::Initialize);
        assert_eq!(next_step(&json!({ "ok": true, "state": "configured", "source": "external" }).to_string()),
                   Next::Settled);
    }

    #[test]
    fn unready_is_ask_again_and_never_unconfigured() {
        // The conflation this convention exists to prevent: a module that has not run
        // `on_context_ready` has not told us it has no config, and initializing it here
        // would race its own load.
        let unready = json!({ "ok": false, "state": "unready", "error": "context not ready" });
        assert_eq!(next_step(&unready.to_string()), Next::AskAgain);
    }

    #[test]
    fn nothing_unreadable_is_ever_read_as_permission_to_write() {
        for raw in ["", "not json", "[]", "{}", r#"{"ok":true}"#, r#"{"state":"weird"}"#,
                    r#"{"state":null}"#, r#"{"ok":false,"error":"boom"}"#] {
            assert_eq!(next_step(raw), Next::AskAgain, "{raw}");
        }
    }

    #[test]
    fn state_decides_and_the_ok_flag_does_not_override_it() {
        // `ok` is false only for `unready`; a module that sets it otherwise is still
        // answering about its config, and `state` is the discriminator.
        assert_eq!(next_step(r#"{"ok":false,"state":"configured"}"#), Next::Settled);
        assert_eq!(next_step(r#"{"ok":false,"state":"unconfigured"}"#), Next::Initialize);
    }

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
