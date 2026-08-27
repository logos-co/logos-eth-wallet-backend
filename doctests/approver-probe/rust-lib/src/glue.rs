//! A Tier A approver, so a send can be driven to a real signature without a GUI.
//!
//! Configure it as the keystore's approver (`{"approver":"approver_probe"}` in
//! `keystore.json`) and it can do exactly what a human in `signer_ui` does: claim the
//! request, read back the bundle id the keystore authored, and approve that exact id.

use serde_json::{json, Value};

pub trait ApproverProbeModule: Send + 'static {
    /// The oldest pending request, or `{ ok, pending: false }`.
    fn peek(&mut self) -> String;

    /// Acknowledge and approve the oldest pending request with `password`.
    ///
    /// Approves the bundle id the keystore returned from `acknowledge`, never one supplied
    /// by the caller — a mismatch is what the keystore refuses, and honouring that is the
    /// whole point of the fixture.
    fn approve_oldest(&mut self, password: String) -> String;

    /// Reject the oldest pending request, so the refusal path is testable too.
    fn reject_oldest(&mut self) -> String;

    fn on_context_ready(&mut self, _ctx: &RustModuleContext) {}
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct ApproverProbeModuleImpl;

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

fn parse(reply: Result<String, impl std::fmt::Debug>) -> Result<Value, String> {
    let raw = reply.map_err(|e| format!("{e:?}"))?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(v.get("error").and_then(Value::as_str).unwrap_or("call refused").to_string());
    }
    Ok(v)
}

/// The oldest handle the keystore is offering, if any.
fn oldest_handle() -> Result<Option<String>, String> {
    let v = parse(modules().keystore_module.pending())?;
    Ok(v.get("pending")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|r| r.get("handle"))
        .and_then(Value::as_str)
        .map(str::to_string))
}

impl ApproverProbeModule for ApproverProbeModuleImpl {
    fn peek(&mut self) -> String {
        match oldest_handle() {
            Ok(Some(h)) => json!({ "ok": true, "pending": true, "handle": h }).to_string(),
            Ok(None) => json!({ "ok": true, "pending": false }).to_string(),
            Err(e) => err(e),
        }
    }

    fn approve_oldest(&mut self, password: String) -> String {
        let handle = match oldest_handle() {
            Ok(Some(h)) => h,
            Ok(None) => return err("nothing pending to approve"),
            Err(e) => return err(e),
        };
        let ack = match parse(modules().keystore_module.acknowledge(&handle)) {
            Ok(v) => v,
            Err(e) => return err(format!("acknowledge: {e}")),
        };
        let Some(bundle_id) = ack.get("bundle_id").and_then(Value::as_str) else {
            return err("acknowledge returned no bundle_id");
        };
        match parse(modules().keystore_module.approve(&handle, bundle_id, &password)) {
            Ok(v) => json!({
                "ok": true, "handle": handle, "bundleId": bundle_id,
                "signedCount": v.get("signed_count").cloned().unwrap_or(Value::Null),
                "renderLines": ack.get("render_lines").cloned().unwrap_or(Value::Null),
            })
            .to_string(),
            Err(e) => err(format!("approve: {e}")),
        }
    }

    fn reject_oldest(&mut self) -> String {
        match oldest_handle() {
            Ok(Some(h)) => {
                let ok = modules().keystore_module.reject(&h).unwrap_or(false);
                json!({ "ok": ok, "handle": h }).to_string()
            }
            Ok(None) => err("nothing pending to reject"),
            Err(e) => err(e),
        }
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<ApproverProbeModuleImpl>();
}
