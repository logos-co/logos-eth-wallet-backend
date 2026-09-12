//! The relays: this module subscribes to `keystore_module::accounts_changed` and to the three
//! `tx_sender_module` events and re-emits them, because a `ui_qml` view one hop further out
//! can subscribe to this module alone.
//!
//! `glue.rs` is behind the `logos_module` feature and `--no-default-features` cannot compile
//! it, so it is read as text. Each check names the SITE it is about and ships with the mutant
//! it must kill, so a check that stopped discriminating fails rather than passes.

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

/// The implementation of `fn <name>`. A trait declaration reaches `;` before any `{` and is
/// skipped; where two definitions remain the longer one is the impl.
fn body_of<'a>(code: &'a str, name: &str) -> &'a str {
    let pat = format!("fn {name}(");
    let (mut out, mut from) = (Vec::new(), 0usize);
    while let Some(rel) = code[from..].find(&pat) {
        let at = from + rel;
        from = at + pat.len();
        let after = closes(code, at, '(', ')');
        let Some(r) = code[after..].find(['{', ';']) else { continue };
        if code.as_bytes()[after + r] == b';' {
            continue;
        }
        let open = after + r;
        out.push(&code[open..closes(code, open, '{', '}')]);
    }
    assert!(!out.is_empty(), "no definition of `{name}`");
    out.into_iter().max_by_key(|b| b.len()).unwrap()
}

#[test]
fn the_relays_are_armed_at_startup_and_the_keystore_one_retried_from_the_account_reads() {
    let code = code_only(GLUE);
    let ctx = body_of(&code, "on_context_ready");
    assert!(ctx.contains("self.watch_keystore()"), "nothing arms the keystore relay at startup");
    assert!(ctx.contains("self.watch_sender()"), "nothing arms the sender relay at startup");
    // Startup is the only chance a module gets; a client that could not be built there would
    // otherwise leave the view deaf for the life of the process.
    for read in ["list_accounts", "get_account_labels", "get_account_wallets"] {
        assert!(body_of(&code, read).contains("self.watch_keystore()"),
                "`{read}` does not retry the relay");
    }
}

/// Every rule below is a predicate over the function's text, so the same one can be run
/// against the real body and against the mutant it exists to reject.
fn arms_once(body: &str, sub: &str) -> bool {
    match (body.find("swap(true"), body.find(sub)) {
        (Some(gate), Some(s)) => gate < s,
        _ => false,
    }
}

/// The subscription is a blocking iterator: draining it on the calling thread would wedge
/// whichever entry point armed it.
fn listens_off_thread(body: &str, threads: usize) -> bool {
    body.matches("std::thread::spawn").count() == threads
}

/// Arming is not retroactive and nothing buffers, so the keystore relay announces once
/// itself — after the subscription stands, never before it.
fn closes_its_own_window(body: &str) -> bool {
    match (body.find("on_accounts_changed"), body.find("emit_accounts_changed")) {
        (Some(sub), Some(emit)) => sub < emit,
        _ => false,
    }
}

fn keystore_relay() -> String {
    body_of(&code_only(GLUE), "watch_keystore").to_string()
}

fn sender_relay() -> String {
    body_of(&code_only(GLUE), "watch_sender").to_string()
}

#[test]
fn the_keystore_relay_arms_once_listens_off_thread_and_closes_its_own_window() {
    let body = keystore_relay();
    assert!(arms_once(&body, "on_accounts_changed"), "the once-flag must be taken BEFORE subscribing");
    assert!(listens_off_thread(&body, 1), "the listener must have a thread of its own");
    assert!(closes_its_own_window(&body), "the announcement must come after arming");
}

/// Three subscriptions, three threads, one flag taken before any of them.
#[test]
fn the_sender_relay_arms_once_and_listens_to_each_event_off_thread() {
    let body = sender_relay();
    for sub in ["on_send_status_changed", "on_tx_status_changed", "on_history_changed"] {
        assert!(arms_once(&body, sub), "the once-flag must be taken BEFORE `{sub}`");
    }
    assert!(listens_off_thread(&body, 3), "one thread per blocking subscription");
    // A subscription that could not be built un-arms the flag, so a later call retries.
    assert!(body.matches("watching_sender.store(false").count() == 3);
}

// ── the mutants the rules above exist to reject ──────────────────────────────────

#[test]
fn an_unguarded_resubscribe_is_rejected() {
    assert!(!arms_once(&keystore_relay().replace("swap(true", "load("), "on_accounts_changed"));
    let late = keystore_relay().replacen("if self.watching_keystore.swap(true, Ordering::SeqCst) {", "if false {", 1)
        + "self.watching_keystore.swap(true, Ordering::SeqCst);";
    assert!(!arms_once(&late, "on_accounts_changed"));
    assert!(!arms_once(&sender_relay().replace("swap(true", "load("), "on_history_changed"));
}

#[test]
fn draining_a_subscription_on_the_calling_thread_is_rejected() {
    assert!(!listens_off_thread(&keystore_relay().replace("std::thread::spawn", "let _drain ="), 1));
    let one_short = sender_relay().replacen("std::thread::spawn", "let _drain =", 1);
    assert!(!listens_off_thread(&one_short, 3));
}

#[test]
fn announcing_before_the_subscription_stands_is_rejected() {
    let body = keystore_relay();
    let moved = body.replace("emit_accounts_changed(keystore_account_count());", "")
        .replacen("let mut ks", "emit_accounts_changed(-1); let mut ks", 1);
    assert!(!closes_its_own_window(&moved), "an announcement ahead of arming closes no window");
}
