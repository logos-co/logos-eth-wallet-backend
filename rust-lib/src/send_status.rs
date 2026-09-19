//! The `send_status` relay: the sender's reply, whole, and its `final` — whether polling can
//! still move the send. A poller stops on `final` and on nothing else.

use serde_json::{json, Value};

/// Whether a `send_status` reply is the end of the send: the sender's own `final`. A sender
/// that predates it is read the way it behaves — `awaitingApproval` (held by the verified gate
/// included) and `broadcasting` still move — and none of its refusals is final, because it
/// cannot say which of them is.
pub fn is_final(reply: &Value) -> bool {
    if let Some(f) = reply.get("final").and_then(Value::as_bool) {
        return f;
    }
    reply.get("ok").and_then(Value::as_bool) == Some(true)
        && !matches!(
            reply.get("status").and_then(Value::as_str),
            Some("awaitingApproval" | "broadcasting")
        )
}

/// The sender's reply to `send_status`, carrying `final`. One that never arrived, or arrived
/// unreadable, is not final: the sender may still be broadcasting behind a call that ran out
/// of time.
pub fn relay(raw: Result<String, impl std::fmt::Debug>) -> String {
    let raw = match raw {
        Ok(raw) => raw,
        Err(e) => return refused(format!("tx_sender_module: {e:?}")),
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(mut v) if v.is_object() => {
            v["final"] = json!(is_final(&v));
            v.to_string()
        }
        Ok(_) => refused("tx_sender_module: the reply is not an object"),
        Err(e) => refused(format!("tx_sender_module: {e}")),
    }
}

/// A refusal of this module's own. It never ends a send.
pub fn refused(error: impl std::fmt::Display) -> String {
    json!({ "ok": false, "final": false, "error": error.to_string() }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relayed(reply: Value) -> Value {
        serde_json::from_str(&relay(Ok::<_, String>(reply.to_string()))).unwrap()
    }

    #[test]
    fn the_senders_final_is_relayed_with_the_rest_of_its_reply() {
        for (reply, want) in [
            (json!({ "ok": true, "status": "awaitingApproval", "final": false }), false),
            (json!({ "ok": true, "status": "awaitingApproval", "blocked": true, "final": false }), false),
            (json!({ "ok": true, "status": "broadcasting", "final": false }), false),
            (json!({ "ok": true, "status": "stuck", "final": true }), true),
            (json!({ "ok": true, "status": "broadcast", "hash": "0xb", "final": true }), true),
            (json!({ "ok": false, "error": "no time left to read the approval", "final": false }), false),
            (json!({ "ok": false, "error": "no send with id 'snd_x'", "final": true }), true),
        ] {
            let out = relayed(reply.clone());
            assert_eq!(out["final"], json!(want), "{reply}");
            assert_eq!(out, reply, "the rest passes through untouched");
        }
        // The sender's word, never its sentence.
        let same_words = json!({ "ok": false, "error": "no send with id 'snd_x'", "final": false });
        assert_eq!(relayed(same_words)["final"], json!(false));
    }

    #[test]
    fn a_sender_that_predates_final_is_read_the_way_it_behaves() {
        for live in ["awaitingApproval", "broadcasting"] {
            assert_eq!(relayed(json!({ "ok": true, "status": live }))["final"], json!(false), "{live}");
        }
        let held = relayed(json!({ "ok": true, "status": "awaitingApproval", "blocked": true }));
        assert_eq!(held["final"], json!(false));
        for done in ["broadcast", "rejected", "cancelled", "failed", "stuck"] {
            assert_eq!(relayed(json!({ "ok": true, "status": done }))["final"], json!(true), "{done}");
        }
        for error in ["no time left to read the approval", "no send with id 'snd_x'"] {
            let out = relayed(json!({ "ok": false, "error": error }));
            assert_eq!(out["final"], json!(false), "{error}");
            assert_eq!(out["error"], json!(error), "and the refusal is still the sender's");
        }
    }

    #[test]
    fn a_reply_that_never_arrived_is_not_final() {
        let down: Value = serde_json::from_str(&relay(Err::<String, _>("Timeout"))).unwrap();
        assert_eq!(down["ok"], json!(false));
        assert_eq!(down["final"], json!(false));
        assert_eq!(down["error"], json!("tx_sender_module: \"Timeout\""), "names the sender");
        for garbled in ["not json", "[1]"] {
            let out: Value = serde_json::from_str(&relay(Ok::<_, String>(garbled.into()))).unwrap();
            assert_eq!((out["ok"].clone(), out["final"].clone()), (json!(false), json!(false)), "{garbled}");
        }
        let own: Value = serde_json::from_str(&refused("no time left to read the send")).unwrap();
        assert_eq!(own, json!({ "ok": false, "final": false, "error": "no time left to read the send" }));
    }
}
