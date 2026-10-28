//! The profile keys: contacts and settings open under keys from the signer manager's `data_key`,
//! asked only by the key worker, on its own thread, with the async call and a channel. A blocking
//! call from a dispatch thread would nest on the main thread.
//!
//! `glue.rs` is behind the `logos_module` feature and `--no-default-features` cannot compile
//! it, so it is read as text. Each check ships with the mutant it must kill.

const GLUE: &str = include_str!("../src/glue.rs");

fn code_only(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let (mut i, mut in_str, mut in_comment) = (0usize, false, false);
    while i < b.len() {
        match (in_str, in_comment, b[i]) {
            (false, false, b'"') => in_str = true,
            (false, false, b'/') if b.get(i + 1) == Some(&b'/') => {
                in_comment = true;
                out[i] = b' ';
            }
            (true, _, b'\\') => {
                out[i] = b' ';
                out[i + 1] = b' ';
                i += 2;
                continue;
            }
            (true, _, b'"') => in_str = false,
            (_, true, b'\n') => in_comment = false,
            (true, _, _) | (_, true, _) => out[i] = b' ',
            _ => {}
        }
        i += 1;
    }
    String::from_utf8(out).expect("blanking replaces bytes one for one")
}

fn closes(code: &str, from: usize, open: char, shut: char) -> usize {
    let at = from + code[from..].find(open).expect("a pair to close");
    let mut depth = 0i32;
    for (k, c) in code[at..].char_indices() {
        if c == open {
            depth += 1;
        } else if c == shut {
            depth -= 1;
            if depth == 0 {
                return at + k + 1;
            }
        }
    }
    code.len()
}

/// Every definition: its name and body span. A declaration reaches `;` before any `{`.
fn functions(code: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = code[from..].find("fn ") {
        let at = from + rel;
        from = at + 3;
        if at > 0 && code.as_bytes()[at - 1].is_ascii_alphanumeric() {
            continue;
        }
        let name: String =
            code[at + 3..].chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        let after = closes(code, at, '(', ')');
        let Some(r) = code[after..].find(['{', ';']) else { continue };
        if name.is_empty() || code.as_bytes()[after + r] == b';' {
            continue;
        }
        let open = after + r;
        out.push((name, open, closes(code, open, '{', '}')));
    }
    out
}

/// The longest definition of `name`: the impl, not a trait's default.
fn body_of(code: &str, name: &str) -> String {
    functions(code)
        .into_iter()
        .filter(|(n, ..)| n == name)
        .map(|(_, a, b)| code[a..b].to_string())
        .max_by_key(|b| b.len())
        .unwrap_or_else(|| panic!("no definition of `{name}`"))
}

/// The innermost function around each call of `callee`, its own definition aside.
fn callers(code: &str, callee: &str) -> Vec<String> {
    let fns = functions(code);
    let pat = format!("{callee}(");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = code[from..].find(&pat) {
        let at = from + rel;
        from = at + pat.len();
        let before = code.as_bytes().get(at.wrapping_sub(1)).copied().unwrap_or(b' ');
        if before.is_ascii_alphanumeric() || before == b'_' || code[..at].ends_with("fn ") {
            continue;
        }
        let inner = fns.iter().filter(|(_, a, b)| *a <= at && at < *b).min_by_key(|(_, a, b)| b - a);
        out.push(inner.map(|(n, ..)| n.clone()).unwrap_or_default());
    }
    out
}

fn mutate(src: &str, from: &str, to: &str) -> String {
    assert_eq!(src.matches(from).count(), 1, "the mutation target moved: {from}");
    src.replacen(from, to, 1)
}

fn check_keys_are_asked_async_by_the_worker(src: &str) -> Result<(), String> {
    let code = code_only(src);
    for sync in [".data_key(", ".data_key_with_timeout("] {
        if code.contains(sync) {
            return Err(format!("`{sync}` is a blocking call: it nests on the main thread"));
        }
    }
    let asks = callers(&code, "data_key_async_with_timeout");
    if asks != ["profile_key"] {
        return Err(format!("data_key is asked from {asks:?}, not from `profile_key` alone"));
    }
    let key_users = callers(&code, "profile_key");
    if key_users.is_empty() || key_users.iter().any(|f| f != "serve_keys") {
        return Err(format!("`profile_key` is called from {key_users:?}; only the key worker may wait on it"));
    }
    if !body_of(&code, "profile_key").contains("recv_timeout(") {
        return Err("`profile_key` no longer bounds its wait on the channel".into());
    }
    Ok(())
}

#[test]
fn the_profile_keys_are_asked_async_and_only_by_the_key_worker() {
    check_keys_are_asked_async_by_the_worker(GLUE).unwrap();
}

#[test]
fn a_blocking_key_call_is_caught() {
    let mutant = mutate(
        GLUE,
        "modules().signer_manager_module.data_key_async_with_timeout(&body, KEY_BUDGET, move |r| {\n        let _ = tx.send(r);\n    });",
        "let _ = tx.send(modules().signer_manager_module.data_key_with_timeout(&body, KEY_BUDGET));",
    );
    let e = check_keys_are_asked_async_by_the_worker(&mutant).unwrap_err();
    assert!(e.contains("blocking"), "{e}");
}

#[test]
fn a_key_asked_from_a_dispatch_thread_is_caught() {
    let mutant = mutate(
        GLUE,
        "    fn list_contacts(&self) -> String {\n",
        "    fn list_contacts(&self) -> String {\n        let _ = profile_key(\"contacts\", None);\n",
    );
    let e = check_keys_are_asked_async_by_the_worker(&mutant).unwrap_err();
    assert!(e.contains("list_contacts"), "{e}");
}

fn check_a_lock_drops_the_keys_before_asking_again(src: &str) -> Result<(), String> {
    let body = body_of(&code_only(src), "serve_keys");
    match (body.find("vault().close()"), body.find("profile::refresh(")) {
        (Some(close), Some(ask)) if close < ask => Ok(()),
        (None, _) => Err("a lock no longer drops the keys".into()),
        _ => Err("the keys are dropped after asking again, or never asked again".into()),
    }
}

#[test]
fn a_lock_drops_the_keys_then_asks_again() {
    check_a_lock_drops_the_keys_before_asking_again(GLUE).unwrap();
}

#[test]
fn a_lock_that_keeps_the_keys_is_caught() {
    let mutant = mutate(
        GLUE,
        "let mut moved = lock && (st.contacts.vault().close() | st.settings.vault().close());",
        "let mut moved = lock;",
    );
    let e = check_a_lock_drops_the_keys_before_asking_again(&mutant).unwrap_err();
    assert!(e.contains("no longer drops"), "{e}");
}

/// Each method that reads or writes a sealed store re-arms the manager's feeds, and answers
/// with the store's own refusal, which keeps `code: "locked"`.
fn check_sealed_methods(src: &str) -> Result<(), String> {
    let code = code_only(src);
    if !body_of(&code, "on_context_ready").contains("self.watch_wallets()") {
        return Err("nothing arms the wallet feeds at startup".into());
    }
    for name in ["list_contacts", "save_contact", "forget_contact", "set_token_sort"] {
        let body = body_of(&code, name);
        if !body.contains("self.watch_wallets()") {
            return Err(format!("`{name}` does not retry the wallet feeds"));
        }
        if !body.contains("e.reply()") {
            return Err(format!("`{name}` flattens the store's refusal and loses `code: locked`"));
        }
    }
    Ok(())
}

#[test]
fn the_sealed_methods_retry_the_feeds_and_keep_the_locked_code() {
    check_sealed_methods(GLUE).unwrap();
}

#[test]
fn a_refusal_that_drops_the_locked_code_is_caught() {
    let mutant = mutate(
        GLUE,
        "Ok(()) => json!({ \"ok\": true, \"address\": address }).to_string(),\n            Err(e) => e.reply(),",
        "Ok(()) => json!({ \"ok\": true, \"address\": address }).to_string(),\n            Err(e) => err(e),",
    );
    let e = check_sealed_methods(&mutant).unwrap_err();
    assert!(e.contains("forget_contact") && e.contains("locked"), "{e}");
}

#[test]
fn a_method_that_never_retries_the_feeds_is_caught() {
    let mutant = mutate(
        GLUE,
        "    fn save_contact(&self, address: String, name: String) -> String {\n        self.watch_wallets();\n",
        "    fn save_contact(&self, address: String, name: String) -> String {\n",
    );
    let e = check_sealed_methods(&mutant).unwrap_err();
    assert!(e.contains("save_contact"), "{e}");
}
