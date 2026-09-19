//! A source-shape guard on `glue.rs`: no outbound call is made under a lock, every call is
//! bounded or argued for, a send names one contract, money leaves ONLY through
//! `tx_sender_module`, and eth_rpc's and token_list's defaults are asked for with no gate.
//!
//! `glue.rs` is behind the `logos_module` feature and `--no-default-features` cannot compile
//! it, so it is read as text. Two rules keep these honest:
//!
//! 1. A `contains` over a region asserts that SOME line in the region matches; the property
//!    is about the line on ONE path. Every check below pins the site, not the vocabulary.
//! 2. Every check ships with the mutant it is meant to kill, and the mutant test asserts the
//!    check REJECTS it — so the check's discriminating power is itself under test.

use std::collections::BTreeSet;

const GLUE: &str = include_str!("../src/glue.rs");

/// The file with comments and string literals blanked out, byte offsets preserved. Brace
/// counting and call-site scanning must not be fooled by a `{` inside a `json!` string.
fn code_only(src: &str) -> String {
    let mut out: Vec<u8> = src.as_bytes().to_vec();
    let b = src.as_bytes();
    let (mut i, mut in_str, mut in_line_comment) = (0usize, false, false);
    while i < b.len() {
        match (in_str, in_line_comment, b[i]) {
            (false, false, b'"') => in_str = true,
            (false, false, b'/') if b.get(i + 1) == Some(&b'/') => {
                in_line_comment = true;
                out[i] = b' ';
            }
            (true, _, b'\\') => {
                out[i] = b' ';
                out[i + 1] = b' ';
                i += 2;
                continue;
            }
            (true, _, b'"') => in_str = false,
            (_, true, b'\n') => in_line_comment = false,
            (true, _, _) => out[i] = b' ',
            (_, true, _) => out[i] = b' ',
            _ => {}
        }
        i += 1;
    }
    String::from_utf8(out).expect("blanking replaces bytes one for one")
}

fn line_of(src: &str, at: usize) -> usize {
    src[..at].matches('\n').count() + 1
}

/// The file above its own test module.
fn non_test(code: &str) -> &str {
    match code.find("mod tests") {
        Some(at) => &code[..at],
        None => code,
    }
}

fn no_ws(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn sites(hay: &str, needle: &str) -> Vec<usize> {
    let (mut out, mut from) = (Vec::new(), 0);
    while let Some(rel) = hay[from..].find(needle) {
        out.push(from + rel);
        from += rel + needle.len();
    }
    out
}

/// The end of the block opened after `from` — its matching close brace.
fn block_end(code: &str, from: usize) -> usize {
    let open = from + code[from..].find('{').expect("a block to close");
    let mut depth = 0i32;
    for (k, c) in code[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return open + k;
                }
            }
            _ => {}
        }
    }
    code.len()
}

/// The end of the call expression starting at `from` — its matching close paren.
fn call_end(code: &str, from: usize) -> usize {
    let open = from + code[from..].find('(').expect("a call to close");
    let mut depth = 0i32;
    for (k, c) in code[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return open + k + 1;
                }
            }
            _ => {}
        }
    }
    code.len()
}

/// One `fn` in the file: its name, its signature span and its body span. A trait method with
/// no body is skipped — its `;` arrives before any `{`, so it opens no scope.
struct Func {
    name: String,
    sig: (usize, usize),
    body: (usize, usize),
}

fn functions(code: &str) -> Vec<Func> {
    let mut out = Vec::new();
    for at in sites(code, "fn ") {
        if at > 0 && code.as_bytes()[at - 1].is_ascii_alphanumeric() {
            continue;
        }
        let rest = &code[at + 3..];
        let name: String =
            rest.trim_start().chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        if name.is_empty() {
            continue;
        }
        let after_params = call_end(code, at);
        let Some(rel) = code[after_params..].find(['{', ';']) else { continue };
        if code.as_bytes()[after_params + rel] == b';' {
            continue; // a declaration, not a definition
        }
        let open = after_params + rel;
        out.push(Func { name, sig: (at, open), body: (open, block_end(code, open)) });
    }
    out
}

/// Every body under `name`. All of them, not the first: a trait's empty default definition
/// sits above the impl that does the work, and picking one is how a check reads the wrong one.
fn bodies_of<'a>(fns: &[Func], code: &'a str, name: &str) -> Vec<&'a str> {
    let out: Vec<&str> =
        fns.iter().filter(|f| f.name == name).map(|f| &code[f.body.0..f.body.1]).collect();
    assert!(!out.is_empty(), "no fn {name}");
    out
}

/// The innermost function containing `at`.
fn enclosing_fn(fns: &[Func], at: usize) -> String {
    fns.iter()
        .filter(|f| f.body.0 <= at && at < f.body.1)
        .min_by_key(|f| f.body.1 - f.body.0)
        .map(|f| f.name.clone())
        .unwrap_or_else(|| "<top level>".into())
}

/// Where each lock is taken, and how far its guard can still be held.
///
/// Two shapes, and the difference matters: `let g = X.lock();` binds the guard for the rest
/// of the enclosing block, while `if let Ok(g) = X.write() { .. }` binds it for that block
/// alone. Charging the first shape's span to the second reports a call made after the guard
/// was dropped, which is how a check gets weakened instead of fixed.
fn guard_scopes(code: &str) -> Vec<(usize, usize)> {
    let mut scopes = Vec::new();
    for pat in [".read()", ".write()", ".lock()"] {
        for at in sites(code, pat) {
            let Some(rel) = code[at..].find(['{', ';']) else { continue };
            let end = if code.as_bytes()[at + rel] == b'{' {
                block_end(code, at)
            } else {
                let mut depth = 0i32;
                let mut end = code.len();
                for (k, c) in code[at..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' if depth == 0 => {
                            end = at + k;
                            break;
                        }
                        '}' => depth -= 1,
                        _ => {}
                    }
                }
                end
            };
            scopes.push((at, end));
        }
    }
    scopes
}

/// Every method of the glue that reaches `modules()`, directly or through another of its own
/// methods. A scan for the literal token cannot see `self.chain_configs(..)` or
/// `self.resolve(..)` — each an IPC round trip one level down, and each invisible to the
/// check that was supposed to keep calls out of a lock scope.
fn reaches_modules(code: &str, fns: &[Func]) -> BTreeSet<String> {
    let mut reaching: BTreeSet<String> = fns
        .iter()
        .filter(|f| code[f.body.0..f.body.1].contains("modules()"))
        .map(|f| f.name.clone())
        .collect();
    loop {
        let mut grew = false;
        for f in fns {
            if reaching.contains(&f.name) {
                continue;
            }
            let body = &code[f.body.0..f.body.1];
            if reaching.iter().any(|r| calls(body, r)) {
                reaching.insert(f.name.clone());
                grew = true;
            }
        }
        if !grew {
            return reaching;
        }
    }
}

fn calls(body: &str, name: &str) -> bool {
    body.contains(&format!("self.{name}(")) || body.contains(&format!("Self::{name}("))
}

/// A copy of the source with one regression applied, for the mutant tests.
fn mutate(src: &str, from: &str, to: &str) -> String {
    assert_eq!(src.matches(from).count(), 1, "the mutation target moved: {from}");
    src.replacen(from, to, 1)
}

// ---------------------------------------------------------------------------------------
// 1. No outbound call shares a scope with a lock guard.
// ---------------------------------------------------------------------------------------

fn check_no_call_under_a_lock(src: &str) -> Result<(), String> {
    let code = code_only(src);
    let fns = functions(&code);
    let reaching = reaches_modules(&code, &fns);
    let scopes = guard_scopes(&code);
    if scopes.is_empty() {
        return Err("the scan found no locks at all — it has stopped working".into());
    }
    for (at, end) in scopes {
        let scope = &code[at..end];
        let culprit = scope
            .contains("modules()")
            .then(|| "modules()".to_string())
            .or_else(|| reaching.iter().find(|r| calls(scope, r)).map(|r| format!("self.{r}()")));
        if let Some(culprit) = culprit {
            return Err(format!(
                "glue.rs:{} takes a lock and calls {culprit} while it is still held. Copy \
                 what you need out of the state, drop the guard, THEN call.",
                line_of(src, at)
            ));
        }
    }
    Ok(())
}

#[test]
fn no_outbound_call_shares_a_scope_with_a_lock_guard() {
    check_no_call_under_a_lock(GLUE).unwrap();
}

/// An IPC call reached through one of the glue's own methods is not the token `modules()`.
#[test]
fn an_outbound_call_reached_through_a_helper_is_caught() {
    let mutant = mutate(
        GLUE,
        "guard.clone().ok_or_else(|| NO_CONTEXT.to_string())",
        "let _ = self.chain_configs(b);\n        guard.clone().ok_or_else(|| NO_CONTEXT.to_string())",
    );
    let e = check_no_call_under_a_lock(&mutant).unwrap_err();
    assert!(e.contains("self.chain_configs()"), "{e}");

    let code = code_only(&mutant);
    assert!(
        guard_scopes(&code).into_iter().all(|(a, e)| !code[a..e].contains("modules()")),
        "the literal-token scan this replaces would have rejected the mutant after all"
    );
}

// ---------------------------------------------------------------------------------------
// 2. The glue takes exactly the two locks that are argued for, in the two named places.
// ---------------------------------------------------------------------------------------

fn check_lock_sites(src: &str) -> Result<(), String> {
    let code = code_only(src);
    let fns = functions(&code);
    let mut taken: Vec<String> =
        guard_scopes(&code).into_iter().map(|(at, _)| enclosing_fn(&fns, at)).collect();
    taken.sort();
    if taken != ["on_context_ready", "state"] {
        return Err(format!(
            "glue.rs should take exactly two locks — `state()` reading the handle and \
             `on_context_ready` installing it. Found them in {taken:?}. Every other lock \
             belongs inside a store, taken around local work and released before anything \
             is called."
        ));
    }
    if code.contains("with_state") {
        return Err("`with_state(|st| ...)` is back: it reads as `borrow the state` and means \
                    `hold a read lock across whatever the closure does`"
            .into());
    }
    let sig = no_ws(&code[fns.iter().find(|f| f.name == "state").expect("fn state").sig.0..]);
    if !sig.starts_with("fnstate(&self)->Result<Arc<State>,String>") {
        return Err("`state()` must hand back an owned handle".into());
    }
    Ok(())
}

#[test]
fn the_glue_takes_no_lock_except_the_one_that_hands_back_a_handle() {
    check_lock_sites(GLUE).unwrap();
}

#[test]
fn a_third_lock_anywhere_else_is_caught() {
    let mutant = mutate(
        GLUE,
        "        // Read the request's chain once and validate it against the shared registry before\n",
        "        let _a = self.state.read();\n        // Read the request's chain once and validate it against the shared registry before\n",
    );
    let e = check_lock_sites(&mutant).unwrap_err();
    assert!(e.contains("send"), "{e}");
}

#[test]
fn handing_back_a_borrow_instead_of_a_handle_is_caught() {
    let mutant =
        mutate(GLUE, "fn state(&self) -> Result<Arc<State>, String> {", "fn state(&self) -> Result<&State, String> {");
    let e = check_lock_sites(&mutant).unwrap_err();
    assert!(e.contains("owned handle"), "{e}");
}

// ---------------------------------------------------------------------------------------
// 3. Every outbound call is bounded, or argued for.
// ---------------------------------------------------------------------------------------

/// Every outbound call carries a deadline, except these — each one a decision, not an
/// oversight. A new unbounded call fails this test and has to be argued for here.
const DELIBERATELY_UNBOUNDED: &[(&str, &str, &str)] = &[
    // One call each, and neither sits behind a gate.
    ("keystore_module", "list_accounts", "a passthrough, one call"),
    ("keystore_module", "get_labels", "a passthrough, one call"),
    ("keystore_module", "get_account_wallets", "a passthrough, one call"),
    // Not calls at all: each arms an `EventSubscription` and the generated client emits no
    // bounded twin for one. A deadline on arming would be a deadline on the SUBSCRIPTION,
    // which is meant to outlive every call this module makes.
    ("keystore_module", "on_accounts_changed", "a subscription has no bounded twin"),
    ("eth_rpc_module", "on_chain_config_changed", "a subscription has no bounded twin"),
    ("eth_rpc_module", "on_chain_enabled_changed", "a subscription has no bounded twin"),
    ("eth_rpc_module", "on_network_scope_changed", "a subscription has no bounded twin"),
    ("token_list_module", "on_tokens_updated", "a subscription has no bounded twin"),
    ("tx_sender_module", "on_send_status_changed", "a subscription has no bounded twin"),
    ("tx_sender_module", "on_tx_status_changed", "a subscription has no bounded twin"),
    ("tx_sender_module", "on_history_changed", "a subscription has no bounded twin"),
];

fn check_calls_are_bounded(src: &str) -> Result<(), String> {
    let code = code_only(src);
    let mut unbounded: Vec<(String, String, usize)> = Vec::new();
    for at in sites(&code, "modules()") {
        let rest = &code[at + "modules()".len()..];
        let mut parts = rest.split('.').skip(1).map(|p| {
            p.trim_start().chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect()
        });
        let (dep, method): (String, String) =
            (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        if !method.ends_with("_with_timeout") {
            unbounded.push((dep, method, line_of(src, at)));
        }
    }
    for (dep, method, line) in &unbounded {
        if !DELIBERATELY_UNBOUNDED.iter().any(|(d, m, _)| d == dep && m == method) {
            return Err(format!(
                "glue.rs:{line} calls {dep}.{method} with no deadline. Give it one from \
                 `budget.rs`, or add it to DELIBERATELY_UNBOUNDED with the reason."
            ));
        }
    }
    // And the list must not outlive its entries, or it becomes a place to hide things.
    for (dep, method, why) in DELIBERATELY_UNBOUNDED {
        if !unbounded.iter().any(|(d, m, _)| d == dep && m == method) {
            return Err(format!(
                "{dep}.{method} is listed as unbounded ({why}) but the glue no longer calls it"
            ));
        }
    }
    Ok(())
}

#[test]
fn every_outbound_call_is_bounded_except_the_ones_argued_for_here() {
    check_calls_are_bounded(GLUE).unwrap();
}

#[test]
fn a_new_unbounded_call_is_caught() {
    let mutant = mutate(
        GLUE,
        "modules().token_list_module.list_offered_with_timeout(chain_id as i64, t)",
        "modules().token_list_module.list_offered(chain_id as i64)",
    );
    let e = check_calls_are_bounded(&mutant).unwrap_err();
    assert!(e.contains("token_list_module.list_offered with no deadline"), "{e}");
}

/// The delegated send is a call across a process boundary too, and the one that registers
/// an approval — it is bounded, and it hands the sender its own deadline.
#[test]
fn an_unbounded_delegated_send_is_caught() {
    let mutant = mutate(
        GLUE,
        "relay(modules().tx_sender_module.send_with_timeout(&request.to_string(), t))",
        "relay(modules().tx_sender_module.send(&request.to_string()))",
    );
    let e = check_calls_are_bounded(&mutant).unwrap_err();
    assert!(e.contains("tx_sender_module.send with no deadline"), "{e}");
}

#[test]
fn an_entry_the_glue_no_longer_calls_is_caught() {
    let mutant = mutate(
        GLUE,
        "let Ok(sub) = ks.on_accounts_changed() else {",
        "let Ok(sub) = ks.on_accounts_changed_with_timeout(RPC_BUDGET) else {",
    );
    let e = check_calls_are_bounded(&mutant).unwrap_err();
    assert!(e.contains("no longer calls it"), "{e}");
}

// ---------------------------------------------------------------------------------------
// 5. A send names ONE contract.
// ---------------------------------------------------------------------------------------

/// Asset resolution and call construction happen exactly once in the assets module. The
/// composer forwards that one call and clones the built facts into its quote.
fn check_a_send_names_one_contract(src: &str) -> Result<(), String> {
    let full = code_only(src);
    let code = non_test(&full);
    let fns = functions(code);
    let found = sites(code, ".evm_assets_module.build_transfer_with_timeout(");
    if found.len() != 1 {
        return Err(format!(
            "`evm_assets_module.build_transfer_with_timeout` must have exactly one site; found {}",
            found.len()
        ));
    }
    if enclosing_fn(&fns, found[0]) != "build_transfer" {
        return Err("asset building must be isolated in `build_transfer`".into());
    }
    let sender = bodies_of(&fns, code, "built_sender_request")[0];
    if !no_ws(sender).contains(".filter(|calls|calls.len()==1)") {
        return Err("the composer must accept exactly one built call".into());
    }
    let quote = bodies_of(&fns, code, "built_quote_reply")[0];
    if !quote.contains("built.clone()") {
        return Err("the quote must preserve the assets module's resolved contract facts".into());
    }
    Ok(())
}

#[test]
fn the_send_path_resolves_a_token_to_one_contract() {
    check_a_send_names_one_contract(GLUE).unwrap();
}

#[test]
fn resolving_a_send_by_first_match_is_caught() {
    let mutant = mutate(
        GLUE,
        ".filter(|calls| calls.len() == 1).cloned()",
        ".filter(|calls| !calls.is_empty()).cloned()",
    );
    let e = check_a_send_names_one_contract(&mutant).unwrap_err();
    assert!(e.contains("exactly one built call"), "{e}");
}

#[test]
fn a_reply_that_names_only_the_symbol_is_caught() {
    let mutant = mutate(GLUE, "let mut v = built.clone();", "let mut v = json!({ \"symbol\": built.get(\"symbol\") });");
    let e = check_a_send_names_one_contract(&mutant).unwrap_err();
    assert!(e.contains("resolved contract facts"), "{e}");
}

// ---------------------------------------------------------------------------------------
// 7. Money leaves only through the sender.
// ---------------------------------------------------------------------------------------

/// This wallet decides WHAT to send and hands it to `tx_sender_module`, which alone reserves
/// the nonce, asks the keystore and broadcasts. A second path to any of those is a second
/// nonce authority on the device — exactly the collision the sender exists to prevent.
fn check_money_leaves_through_the_sender(src: &str) -> Result<(), String> {
    let code = code_only(src);
    let fns = functions(&code);
    for forbidden in ["send_raw_transaction", "request_approval", "fetch_result", "get_transaction_count"] {
        if let Some(at) = code.find(forbidden) {
            return Err(format!(
                "glue.rs:{} calls `{forbidden}`. Broadcasting, asking the keystore for a \
                 signature and reading the nonce are tx_sender_module's; a wallet that does \
                 any of them itself is a second nonce authority.",
                line_of(src, at)
            ));
        }
    }
    let sends = sites(&code, ".tx_sender_module.send_with_timeout(");
    if sends.len() != 1 || enclosing_fn(&fns, sends[0]) != "send" {
        return Err(format!(
            "the delegated send must have exactly one site, in `send`; found {} in {:?}",
            sends.len(),
            sends.iter().map(|a| enclosing_fn(&fns, *a)).collect::<Vec<_>>()
        ));
    }
    // The request carries the claim the assets module authored; this composer never rebuilds
    // human-facing asset text itself.
    let body = bodies_of(&fns, &code, "send")[0];
    if !(body.contains("request[") && body.contains("] = built.get(")) {
        return Err("`send` does not forward the built purpose".into());
    }
    if code.contains("send::purpose(") {
        return Err("the composer reconstructs `purpose` instead of forwarding it".into());
    }
    Ok(())
}

#[test]
fn money_leaves_only_through_tx_sender_module() {
    check_money_leaves_through_the_sender(GLUE).unwrap();
}

#[test]
fn a_wallet_that_broadcasts_itself_is_caught() {
    let mutant = mutate(
        GLUE,
        "        send_status::relay(modules().tx_sender_module.send_status_with_timeout(&request_id, t))",
        "        let _ = modules().eth_rpc_module.send_raw_transaction_with_timeout(1, &request_id, t);\n        send_status::relay(modules().tx_sender_module.send_status_with_timeout(&request_id, t))",
    );
    let e = check_money_leaves_through_the_sender(&mutant).unwrap_err();
    assert!(e.contains("send_raw_transaction"), "{e}");
}

#[test]
fn a_send_with_no_claim_line_is_caught() {
    let mutant = mutate(
        GLUE,
        "        request[\"purpose\"] = built.get(\"purpose\").cloned().unwrap_or(Value::Null);\n",
        "",
    );
    let e = check_money_leaves_through_the_sender(&mutant).unwrap_err();
    assert!(e.contains("does not forward"), "{e}");
}

// ---------------------------------------------------------------------------------------
// 8. eth_rpc's and token_list's defaults are asked for, never gated.
// ---------------------------------------------------------------------------------------

/// Each dependency owns its defaults and fills only what is absent, and this wallet asks for
/// them: at startup and in front of the reads that need them, until one answer lands. A
/// `config_status` gate would strand eth_rpc's: a store another app already wrote to reads
/// `configured` and still lacks what it needs.
fn check_defaults_are_asked_for(
    src: &str,
    dep: &str,
    ensure: &str,
    callers: &[&str],
) -> Result<(), String> {
    let full = code_only(src);
    let code = non_test(&full);
    let fns = functions(code);
    let asks = sites(code, &format!(".{dep}.init_defaults_with_timeout("));
    if asks.len() != 1 || enclosing_fn(&fns, asks[0]) != ensure {
        return Err(format!(
            "{dep}'s defaults must be asked for in `{ensure}` alone; found {} sites",
            asks.len()
        ));
    }
    if no_ws(code).contains(&format!(".{dep}.config_status")) {
        return Err(format!("{dep}'s `config_status` is read: its defaults must not be gated on it"));
    }
    let call = format!("self.{ensure}(");
    for caller in callers {
        if !bodies_of(&fns, code, caller).iter().any(|b| b.contains(&call)) {
            return Err(format!("`{caller}` does not ask {dep} for its defaults"));
        }
    }
    Ok(())
}

fn check_eth_rpc_defaults_are_asked_for(src: &str) -> Result<(), String> {
    let callers = ["on_context_ready", "chain_configs"];
    check_defaults_are_asked_for(src, "eth_rpc_module", "ensure_eth_rpc", &callers)
}

fn check_token_list_defaults_are_asked_for(src: &str) -> Result<(), String> {
    let callers = ["on_context_ready", "list_tokens", "list_available_tokens", "set_token_enabled"];
    check_defaults_are_asked_for(src, "token_list_module", "ensure_token_list", &callers)
}

#[test]
fn eth_rpc_defaults_are_asked_for_at_startup_and_before_every_registry_read() {
    check_eth_rpc_defaults_are_asked_for(GLUE).unwrap();
}

#[test]
fn token_list_defaults_are_asked_for_at_startup_and_before_its_token_reads() {
    check_token_list_defaults_are_asked_for(GLUE).unwrap();
}

#[test]
fn a_config_status_gate_on_eth_rpc_defaults_is_caught() {
    let mutant = mutate(
        GLUE,
        "let applied = modules().eth_rpc_module.init_defaults_with_timeout(t);",
        "let _ = modules().eth_rpc_module.config_status_with_timeout(t);\n        let applied = modules().eth_rpc_module.init_defaults_with_timeout(t);",
    );
    let e = check_eth_rpc_defaults_are_asked_for(&mutant).unwrap_err();
    assert!(e.contains("config_status"), "{e}");
}

#[test]
fn a_config_status_gate_on_token_list_defaults_is_caught() {
    let mutant = mutate(
        GLUE,
        "let applied = modules().token_list_module.init_defaults_with_timeout(t);",
        "let _ = modules().token_list_module.config_status_with_timeout(t);\n        let applied = modules().token_list_module.init_defaults_with_timeout(t);",
    );
    let e = check_token_list_defaults_are_asked_for(&mutant).unwrap_err();
    assert!(e.contains("config_status"), "{e}");
}

#[test]
fn a_registry_read_that_skips_the_defaults_is_caught() {
    let mutant = mutate(GLUE, "        self.ensure_eth_rpc(b);\n", "");
    let e = check_eth_rpc_defaults_are_asked_for(&mutant).unwrap_err();
    assert!(e.contains("chain_configs"), "{e}");
}

#[test]
fn a_token_read_that_skips_the_defaults_is_caught() {
    let mutant = mutate(
        GLUE,
        "        self.watch_tokens();\n        self.ensure_token_list(&b);\n",
        "        self.watch_tokens();\n",
    );
    let e = check_token_list_defaults_are_asked_for(&mutant).unwrap_err();
    assert!(e.contains("set_token_enabled"), "{e}");
}
