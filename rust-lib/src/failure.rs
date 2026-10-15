//! A dependency the runtime got no answer from, in words, with the runtime's own code beside
//! them. Pure, so `cargo test --no-default-features` covers the wording.

use serde_json::{json, Value};

/// The reply for an SDK failure calling `module`, whose message is the runtime's
/// `{ code, message }` object: a sentence, never a Debug dump.
pub fn unreached(module: &str, message: &str) -> String {
    let v: Value = serde_json::from_str(message).unwrap_or(Value::Null);
    let code = v.get("code").and_then(Value::as_str).unwrap_or("transport");
    let detail = v.get("message").and_then(Value::as_str).unwrap_or(message);
    let error = if code == "timeout" {
        format!("{module} did not answer in time ({detail}).")
    } else {
        format!("{module} could not be asked ({detail}).")
    };
    json!({ "ok": false, "code": code, "error": error }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_reads_as_a_sentence_and_keeps_its_code() {
        let m = r#"{"code":"timeout","message":"call to 'tx_sender_module.prepare' timed out after 45100ms","origin":"tx_sender_module"}"#;
        let v: Value = serde_json::from_str(&unreached("tx_sender_module", m)).unwrap();
        assert_eq!(v["code"], "timeout");
        assert_eq!(v["error"], "tx_sender_module did not answer in time (call to 'tx_sender_module.prepare' timed out after 45100ms).");
        let v: Value = serde_json::from_str(&unreached("fee_module", "lp_invoke failed with code 3")).unwrap();
        assert_eq!((v["code"].as_str(), v["error"].as_str()),
                   (Some("transport"), Some("fee_module could not be asked (lp_invoke failed with code 3).")));
    }
}
