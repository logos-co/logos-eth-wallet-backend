//! The rule this file holds: a method that changes persisted or observable state announces
//! it, on the change alone and after the write; a pure reader announces nothing; and what
//! the sender announces reaches this module's subscribers unchanged.
//!
//! `glue.rs` is behind the `logos_module` feature and `--no-default-features` cannot compile
//! it, so it is read as text. Every check pins the SITE rather than the vocabulary, and every
//! check ships with the mutant it is meant to kill.

const GLUE: &str = include_str!("../src/glue.rs");

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

fn sites(hay: &str, needle: &str) -> Vec<usize> {
    let (mut out, mut from) = (Vec::new(), 0);
    while let Some(rel) = hay[from..].find(needle) {
        out.push(from + rel);
        from += rel + needle.len();
    }
    out
}

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

struct Func {
    name: String,
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
        out.push(Func { name, body: (open, block_end(code, open)) });
    }
    out
}

/// Every body under `name`. All of them, not the first.
fn bodies_of<'a>(fns: &[Func], code: &'a str, name: &str) -> Vec<&'a str> {
    let out: Vec<&str> =
        fns.iter().filter(|f| f.name == name).map(|f| &code[f.body.0..f.body.1]).collect();
    assert!(!out.is_empty(), "no fn {name}");
    out
}

fn mutate(src: &str, from: &str, to: &str) -> String {
    assert_eq!(src.matches(from).count(), 1, "the mutation target moved: {from}");
    src.replacen(from, to, 1)
}

/// The head of the innermost block enclosing `at` — the ~120 bytes before its opening brace,
/// which is where an `if` puts its condition. Empty when `at` sits at the body's own depth.
fn enclosing_head(body: &str, at: usize) -> &str {
    let mut open: Vec<usize> = Vec::new();
    for (k, c) in body[..at].char_indices() {
        match c {
            '{' => open.push(k),
            '}' => {
                open.pop();
            }
            _ => {}
        }
    }
    match open.last() {
        Some(&brace) => &body[brace.saturating_sub(120)..brace],
        None => "",
    }
}

/// The events this module declares, in `EthWalletBackendModuleEvents`.
fn declared_events(code: &str) -> Vec<String> {
    let at = code.find("trait EthWalletBackendModuleEvents").expect("the events trait");
    let block = &code[at..block_end(code, at)];
    sites(block, "fn ")
        .into_iter()
        .map(|a| {
            block[a + 3..].trim_start().chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect()
        })
        .collect()
}

/// The one method that writes `settings.json`. Chain scope and token membership live in
/// their reusable providers.
const SETTINGS_MUTATORS: &[&str] = &["set_token_sort"];

/// Methods that only read, or relay a read. The sender announces what its sweep moves; a
/// relay that announced on top would be a view driving its own subscription round forever.
const READERS: &[&str] = &[
    "list_networks",
    "verified_proxy_state",
    "list_tokens",
    "list_available_tokens",
    "get_balances",
    "get_history",
    "refresh_pending",
    "refresh_tx_status",
    "get_tx_details",
    "send_status",
    "prepare_send",
    "suggest_fees",
    "get_account_wallets",
    "list_contacts",
];

// ---------------------------------------------------------------------------------------
// 1. A declared event has an emitter.
// ---------------------------------------------------------------------------------------

fn check_every_declared_event_is_emitted(src: &str) -> Result<(), String> {
    let code = code_only(src);
    let declared = declared_events(&code);
    if declared.len() < 5 {
        return Err(format!("only {declared:?} parsed out of the events trait — the scan broke"));
    }
    let silent: Vec<&String> =
        declared.iter().filter(|e| !code.contains(&format!("emit_{e}("))).collect();
    if !silent.is_empty() {
        return Err(format!(
            "{silent:?} are declared as events and never emitted. A consumer cannot tell a \
             subscription that will never fire from one whose fact has not happened yet."
        ));
    }
    Ok(())
}

#[test]
fn every_event_this_module_declares_is_emitted_somewhere() {
    check_every_declared_event_is_emitted(GLUE).unwrap();
}

#[test]
fn declaring_an_event_nothing_fires_is_caught() {
    let mutant = mutate(
        GLUE,
        "    fn accounts_changed(&self, count: i64);",
        "    fn accounts_changed(&self, count: i64);\n    fn settings_changed(&self);",
    );
    let e = check_every_declared_event_is_emitted(&mutant).unwrap_err();
    assert!(e.contains("settings_changed"), "{e}");
}

// ---------------------------------------------------------------------------------------
// 2. A settings mutator announces, after the write, and only on a change.
// ---------------------------------------------------------------------------------------

fn check_mutators_announce_a_change(src: &str) -> Result<(), String> {
    let code = code_only(src);
    let fns = functions(&code);
    for name in SETTINGS_MUTATORS {
        for body in bodies_of(&fns, &code, name) {
            let emits = sites(body, "emit_");
            if emits.is_empty() {
                return Err(format!(
                    "{name} writes settings.json and announces nothing. A view has no way to \
                     learn of the change except by asking again."
                ));
            }
            let write = body
                .find("st.settings.")
                .ok_or_else(|| format!("{name} no longer writes through st.settings"))?;
            for at in emits {
                if at < write {
                    return Err(format!("{name} announces before it writes"));
                }
                let head = enclosing_head(body, at);
                if !head.contains("changed") {
                    return Err(format!(
                        "{name} announces from a block headed `{}` — nothing there tests \
                         whether the write moved anything, so re-setting what is already \
                         stored fires an event and a view that re-reads on it loops.",
                        head.trim()
                    ));
                }
            }
        }
    }
    Ok(())
}

#[test]
fn every_settings_mutator_announces_its_change_and_only_its_change() {
    check_mutators_announce_a_change(GLUE).unwrap();
}

#[test]
fn a_silent_mutator_is_caught() {
    let mutant = mutate(
        GLUE,
        "                    emit_token_sort_changed(order);\n",
        "",
    );
    let e = check_mutators_announce_a_change(&mutant).unwrap_err();
    assert!(e.contains("set_token_sort"), "{e}");
}

#[test]
fn announcing_a_write_that_moved_nothing_is_caught() {
    let mutant = mutate(
        GLUE,
        "                if a.changed {\n                    emit_token_sort_changed(order);\n                }",
        "                if true {\n                    emit_token_sort_changed(order);\n                }",
    );
    let e = check_mutators_announce_a_change(&mutant).unwrap_err();
    assert!(e.contains("set_token_sort") && e.contains("loops"), "{e}");
}

#[test]
fn announcing_before_the_write_lands_is_caught() {
    let mutant = mutate(
        GLUE,
        "        match self.state().and_then(|st| st.settings.set_token_sort(o)",
        "        emit_token_sort_changed(&order);\n        match self.state().and_then(|st| st.settings.set_token_sort(o)",
    );
    let e = check_mutators_announce_a_change(&mutant).unwrap_err();
    assert!(e.contains("announces before it writes"), "{e}");
}

// ---------------------------------------------------------------------------------------
// 3. A reader announces nothing.
// ---------------------------------------------------------------------------------------

fn check_readers_stay_silent(src: &str) -> Result<(), String> {
    let code = code_only(src);
    let fns = functions(&code);
    for name in READERS {
        for body in bodies_of(&fns, &code, name) {
            if body.contains("emit_") {
                return Err(format!(
                    "{name} only reads, and announces. A view subscribed to that event \
                     re-reads, which announces again."
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn no_read_announces_itself() {
    check_readers_stay_silent(GLUE).unwrap();
}

#[test]
fn a_read_that_announces_itself_is_caught() {
    let mutant = mutate(
        GLUE,
        "        match modules().evm_assets_module.decorate_history_with_timeout(&history.to_string(), t) {",
        "        emit_balances_updated(&address);\n        match modules().evm_assets_module.decorate_history_with_timeout(&history.to_string(), t) {",
    );
    let e = check_readers_stay_silent(&mutant).unwrap_err();
    assert!(e.contains("get_history"), "{e}");
}

// ---------------------------------------------------------------------------------------
// 4. What the sender announces reaches this module's subscribers.
// ---------------------------------------------------------------------------------------

/// A view depends on this module alone, so the sender's three events must come out of here
/// under this module's own names — and a settle, which can move a balance the view is
/// showing, must also say so, with an empty address because the sender's event names none.
fn check_the_sender_relay_re_emits_everything(src: &str) -> Result<(), String> {
    let code = code_only(src);
    let fns = functions(&code);
    let body = bodies_of(&fns, &code, "watch_sender")[0];
    for (decode, emit) in [
        ("decode_send_status_changed", "emit_send_status_changed("),
        ("decode_tx_status_changed", "emit_tx_status_changed("),
        ("decode_history_changed", "emit_history_changed("),
    ] {
        let at = body.find(decode).ok_or_else(|| format!("watch_sender no longer decodes {decode}"))?;
        let arm = &body[at..block_end(body, at)];
        if !arm.contains(emit) {
            return Err(format!("the sender's {decode} is decoded and not re-emitted as {emit}"));
        }
    }
    let at = body.find("decode_tx_status_changed").expect("checked above");
    let arm = &body[at..block_end(body, at)];
    if !arm.contains("emit_balances_updated(") {
        return Err("a settled transaction can move a balance the view is showing, and the \
                    relay of tx_status_changed does not say so"
            .into());
    }
    Ok(())
}

#[test]
fn the_sender_relay_re_emits_every_event_it_subscribes_to() {
    check_the_sender_relay_re_emits_everything(GLUE).unwrap();
}

#[test]
fn a_relay_that_drops_the_balance_announcement_is_caught() {
    let mutant = mutate(GLUE, "                    emit_balances_updated(\"\");\n", "");
    let e = check_the_sender_relay_re_emits_everything(&mutant).unwrap_err();
    assert!(e.contains("does not say so"), "{e}");
}

#[test]
fn a_relay_that_swallows_an_event_is_caught() {
    let mutant = mutate(
        GLUE,
        "                    emit_history_changed(&e.address);\n",
        "                    let _ = e.address;\n",
    );
    let e = check_the_sender_relay_re_emits_everything(&mutant).unwrap_err();
    assert!(e.contains("decode_history_changed"), "{e}");
}
