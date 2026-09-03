//! The rule this file holds: a method that changes persisted or observable state announces
//! it, on the change alone and after the write; a pure reader announces nothing.
//!
//! `set_token_enabled` and `set_token_sort` were silent — the offered set could move under a
//! view with nothing saying so — and `set_active_chain` announced every call, including one
//! that re-affirmed the chain already stored. Both directions are checked here.
//!
//! `glue.rs` is behind the `logos_module` feature and `--no-default-features` cannot compile
//! it, so it is read as text, exactly as `glue_never_calls_under_a_lock.rs` does. The same two
//! rules keep that honest: every check pins the SITE rather than the vocabulary, and every
//! check ships with the mutant it is meant to kill.

const GLUE: &str = include_str!("../src/glue.rs");

/// The file with comments and string literals blanked out, byte offsets preserved. Brace
/// counting must not be fooled by a `{` inside a `json!` string.
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

/// Every body under `name`. All of them, not the first: an impl and a trait default can
/// share a name, and picking one is how a check reads the wrong one.
fn bodies_of<'a>(fns: &[Func], code: &'a str, name: &str) -> Vec<&'a str> {
    let out: Vec<&str> =
        fns.iter().filter(|f| f.name == name).map(|f| &code[f.body.0..f.body.1]).collect();
    assert!(!out.is_empty(), "no fn {name}");
    out
}

/// A copy of the source with one regression applied, for the mutant tests.
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

/// The three methods that write `settings.json`. Each must announce, on the change alone.
const SETTINGS_MUTATORS: &[&str] = &["set_active_chain", "set_token_enabled", "set_token_sort"];

/// Methods that only read. `get_history` and `refresh_pending` are deliberately absent: both
/// drive the sweep, which WRITES, and section 4 is where its announcement is pinned.
const READERS: &[&str] = &[
    "list_networks",
    "get_active_network",
    "verified_proxy_state",
    "list_tokens",
    "list_available_tokens",
    "get_balances",
    "get_tx_details",
    "suggest_fees",
];

// ---------------------------------------------------------------------------------------
// 1. A declared event has an emitter.
// ---------------------------------------------------------------------------------------

/// The defect that motivated all of this, generalised: an event in the trait that nothing
/// ever fires is a consumer subscribing to silence, and it costs a subscription to find out.
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
            // The write is what `changed` is computed from, so an announcement in front of it
            // would be announcing what the caller asked for, not what reached disk.
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
        "                if a.changed {\n                    emit_tokens_changed(chain_id);\n                }\n",
        "",
    );
    let e = check_mutators_announce_a_change(&mutant).unwrap_err();
    assert!(e.contains("set_token_enabled"), "{e}");
}

#[test]
fn announcing_a_write_that_moved_nothing_is_caught() {
    let mutant = mutate(
        GLUE,
        "                if a.changed {\n                    emit_tokens_changed(chain_id);\n                }",
        "                if true {\n                    emit_tokens_changed(chain_id);\n                }",
    );
    let e = check_mutators_announce_a_change(&mutant).unwrap_err();
    assert!(e.contains("set_token_enabled") && e.contains("loops"), "{e}");
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

/// The other direction, and the one that bites quietly: a read that announces itself is a
/// view driving its own subscription round forever.
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
        "        let balances = tokens::balance_rows(chain_id, &list, &decoded, settings.token_sort);",
        "        let balances = tokens::balance_rows(chain_id, &list, &decoded, settings.token_sort);\n        emit_balances_updated(&address);",
    );
    let e = check_readers_stay_silent(&mutant).unwrap_err();
    assert!(e.contains("get_balances"), "{e}");
}

// ---------------------------------------------------------------------------------------
// 4. The sweep announces the balance it moved, wherever it was driven from.
// ---------------------------------------------------------------------------------------

/// `refresh_pending` announced a confirmation and `get_history` did not, though both sweep.
/// A row confirming under an open history view moved the balance with nothing saying so, so
/// the announcement belongs in the writer — once, and in one place.
fn check_the_sweep_announces_its_own_confirmations(src: &str) -> Result<(), String> {
    let code = code_only(src);
    let fns = functions(&code);
    for body in bodies_of(&fns, &code, "sweep") {
        if !body.contains("emit_balances_updated(") {
            return Err("the sweep confirms rows and does not announce the balances they \
                        moved, so whichever caller forgets to is silent"
                .into());
        }
    }
    for name in ["refresh_pending", "get_history"] {
        for body in bodies_of(&fns, &code, name) {
            if body.contains("emit_balances_updated(") {
                return Err(format!(
                    "{name} announces a balance move the sweep already announced. Two sites \
                     is how one of them ends up being the only one."
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn a_confirmation_is_announced_once_by_the_sweep_that_found_it() {
    check_the_sweep_announces_its_own_confirmations(GLUE).unwrap();
}

#[test]
fn announcing_from_one_caller_of_the_sweep_instead_is_caught() {
    let mutant = mutate(
        GLUE,
        "        if out.confirmed {\n            emit_balances_updated(address);\n        }\n",
        "",
    );
    let mutant = mutate(
        &mutant,
        "        let s = self.sweep(&st, &address, &Budget::new(SWEEP_BUDGET));\n        json!({ \"ok\": true, \"address\": address,",
        "        let s = self.sweep(&st, &address, &Budget::new(SWEEP_BUDGET));\n        if s.confirmed {\n            emit_balances_updated(&address);\n        }\n        json!({ \"ok\": true, \"address\": address,",
    );
    let e = check_the_sweep_announces_its_own_confirmations(&mutant).unwrap_err();
    assert!(e.contains("whichever caller forgets"), "{e}");
}

// ---------------------------------------------------------------------------------------
// 5. A history row no hash can name still announces itself.
// ---------------------------------------------------------------------------------------

/// The intent row goes to disk BEFORE the broadcast and has no hash until the node answers,
/// so `tx_status_changed` cannot name it. A history view would otherwise learn of a send only
/// if it succeeded — and on the `leave_unknown` arm, never.
fn check_the_intent_row_is_announced(src: &str) -> Result<(), String> {
    let code = code_only(src);
    let fns = functions(&code);
    for body in bodies_of(&fns, &code, "advance_send") {
        let record = body.find("record_intent(").ok_or("advance_send no longer records")?;
        let announced: Vec<usize> =
            sites(body, "emit_history_changed(").into_iter().filter(|a| *a > record).collect();
        // One for the intent, one per broadcast arm that leaves the row unknown.
        if announced.len() < 3 {
            return Err(format!(
                "advance_send announces {} of the 3 history writes that no hash can name: \
                 the intent, and the two arms that leave the row unknown.",
                announced.len()
            ));
        }
    }
    Ok(())
}

#[test]
fn every_hashless_history_write_is_announced() {
    check_the_intent_row_is_announced(GLUE).unwrap();
}

#[test]
fn a_row_written_before_the_broadcast_and_never_announced_is_caught() {
    let mutant = mutate(GLUE, "\n        emit_history_changed(&job.from);\n", "\n");
    let e = check_the_intent_row_is_announced(&mutant).unwrap_err();
    assert!(e.contains("announces 2 of the 3"), "{e}");
}

// ---------------------------------------------------------------------------------------
// 6. A settle announces only the status it actually moved.
// ---------------------------------------------------------------------------------------

/// `settle_locked` answers with the live job on three paths, and only one of them wrote
/// anything: an outsider settling a claimed broadcast, and a settle arriving after a
/// terminal status, both get the job back unchanged. Announcing on `Some` alone made
/// `send_status_changed` mean "someone tried", which is the one thing an event may not mean.
fn check_a_settle_announces_only_a_move(src: &str) -> Result<(), String> {
    let code = code_only(src);
    let fns = functions(&code);
    for name in ["settle", "settle_owned"] {
        for body in bodies_of(&fns, &code, name) {
            let emits = sites(body, "emit_send_status_changed(");
            if emits.is_empty() {
                return Err(format!("{name} settles a send and announces nothing"));
            }
            for at in emits {
                let head = enclosing_head(body, at);
                if !head.contains("changed") {
                    return Err(format!(
                        "{name} announces from a block headed `{}` — the ledger answers with \
                         the live job whether or not this call moved it, so a settle the \
                         broadcast owner refused, or one that arrived after a terminal \
                         status, is announced as a state change that never happened.",
                        head.trim()
                    ));
                }
            }
        }
    }
    Ok(())
}

#[test]
fn only_a_settle_that_moved_the_status_is_announced() {
    check_a_settle_announces_only_a_move(GLUE).unwrap();
}

#[test]
fn announcing_a_settle_that_lost_the_race_is_caught() {
    let mutant = mutate(
        GLUE,
        "            .ok_or_else(|| format!(\"no send with id '{request_id}'\"))?;\n        if s.changed {\n            emit_send_status_changed(&s.job.request_id);\n        }\n",
        "            .ok_or_else(|| format!(\"no send with id '{request_id}'\"))?;\n        emit_send_status_changed(&s.job.request_id);\n",
    );
    let e = check_a_settle_announces_only_a_move(&mutant).unwrap_err();
    assert!(e.contains("never happened"), "{e}");
}

// ---------------------------------------------------------------------------------------
// 7. A receipt is announced once it is STORED, not once it is decided.
// ---------------------------------------------------------------------------------------

/// `apply_receipt` decides the new status in memory and then writes it. Announcing on the
/// in-memory verdict announces a settle that a refused write left on disk as `pending` — a
/// subscriber re-reads the row it was told about and finds it unmoved, the sweep re-polls it
/// forever, and the reply's `changed` count names a transaction nothing settled.
fn check_a_receipt_is_announced_only_once_stored(src: &str) -> Result<(), String> {
    let code = code_only(src);
    let fns = functions(&code);
    for name in ["sweep", "refresh_one"] {
        for body in bodies_of(&fns, &code, name) {
            if !body.contains("apply_receipt(") {
                return Err(format!("{name} no longer applies receipts — the check is blind"));
            }
            for at in sites(body, "emit_tx_status_changed(") {
                let head = enclosing_head(body, at);
                if !head.contains("apply_receipt") {
                    return Err(format!(
                        "{name} announces a receipt from a block headed `{}`, which is not \
                         the apply that stored it.",
                        head.trim()
                    ));
                }
                // `Ok(true)` and `?` are the two shapes that can only be reached by a write
                // that landed; a bare bool cannot distinguish one from a full disk.
                if !(head.contains("Ok(true)") || head.contains('?')) {
                    return Err(format!(
                        "{name} announces a receipt from a block headed `{}` — that answers \
                         whether the row moved in memory, not whether the new status reached \
                         disk, so a write the disk refused is announced as a settle.",
                        head.trim()
                    ));
                }
            }
        }
    }
    Ok(())
}

#[test]
fn a_receipt_is_announced_only_once_it_is_on_disk() {
    check_a_receipt_is_announced_only_once_stored(GLUE).unwrap();
}

#[test]
fn announcing_a_receipt_whose_write_was_refused_is_caught() {
    let mutant = mutate(
        GLUE,
        "if st.history.apply_receipt(&rec, &receipt, history::now_secs())? {",
        "if st.history.apply_receipt(&rec, &receipt, history::now_secs()).unwrap_or_default() {",
    );
    let e = check_a_receipt_is_announced_only_once_stored(&mutant).unwrap_err();
    assert!(e.contains("refused"), "{e}");
}
