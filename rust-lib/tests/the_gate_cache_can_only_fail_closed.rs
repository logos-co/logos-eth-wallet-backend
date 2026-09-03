//! The gate cache, read as source. `crate::gate` proves the cache's own rules in isolation;
//! what cannot be proved there is that `glue.rs` USES it the one way those rules assume:
//!
//! * the gate is skipped only for [`Gate::Open`] — a chain eth_rpc told us is `off`, where its
//!   own `blocking = mode_required && !usable` cannot depend on the proxy health this skips;
//! * a read cuts its ticket BEFORE going out, so an event that overtakes it wins;
//! * the cache goes live inside the listener, once the subscription stands, and dies with it.
//!
//! Each check ships with the mutant it must kill, so a check that stopped discriminating
//! fails rather than passes. `glue.rs` is behind the `logos_module` feature and
//! `--no-default-features` cannot compile it, so it is read as text.

const GLUE: &str = include_str!("../src/glue.rs");

/// Comments and string literals blanked, offsets preserved, so brace counting is not fooled
/// by a `{` inside a `json!` string.
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
/// skipped; a DEFAULTED trait method has a body too, so where two remain the longer one is
/// the impl and the empty stub is the default.
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

fn body(name: &str) -> String {
    body_of(&code_only(GLUE), name).to_string()
}

// ── the rules, each a predicate so the mutant can be run through the same one ──────────

/// The skip is allowed for exactly one answer. Anything else — `Ask`, a negated test, a
/// second early exit — is a gate that opens on something other than "verification is off".
fn skips_only_when_open(b: &str) -> bool {
    b.contains("if self.gate.gate(chain_id) == Gate::Open {")
        && b.contains("Self::gate_of(")
        && b.matches("return Ok(())").count() == 1
}

/// The ticket is cut before the read leaves, or the read cannot tell an event overtook it.
fn tickets_before_reading(b: &str) -> bool {
    match (b.find(".ticket()"), b.find("verified_proxy_status"), b.find(".learned(")) {
        (Some(t), Some(read), Some(l)) => t < read && read < l,
        _ => false,
    }
}

/// Trust starts inside the listener, after the subscription stands — never at the call site,
/// where a mode that moved in the gap would be announced to nobody.
fn goes_live_behind_the_subscription(b: &str) -> bool {
    match (b.find("on_verified_proxy_mode_changed"), b.find("feed_live")) {
        (Some(sub), Some(live)) => sub < live && b.find("listen(").is_some_and(|l| l < live),
        _ => false,
    }
}

/// Both ways the feed can end drop what it was holding: a subscription that could not be
/// built, and one that stopped delivering.
fn dies_closed(b: &str) -> bool {
    let after_loop = b.find("for ev in sub").is_some_and(|f| b.rfind("feed_dead").unwrap_or(0) > f);
    after_loop && b.find("else {").is_some_and(|e| b.find("feed_dead").unwrap_or(usize::MAX) > e)
}

#[test]
fn the_gate_skips_the_hop_only_for_a_chain_told_to_be_off() {
    for g in ["verified_gate", "verified_gate_within"] {
        assert!(skips_only_when_open(&body(g)), "`{g}` does not skip on `Gate::Open` alone");
    }
}

#[test]
fn a_gate_that_skips_on_anything_else_is_rejected() {
    let b = body("verified_gate");
    // The inversion: every chain we know nothing about would be waved through.
    assert!(!skips_only_when_open(&b.replace("Gate::Open", "Gate::Ask")));
    assert!(!skips_only_when_open(&b.replace("== Gate::Open", "!= Gate::Open")));
    // A second exit ahead of the live read is the same failure wearing another shape.
    assert!(!skips_only_when_open(&b.replacen(
        "Self::gate_of(",
        "if true { return Ok(()) } Self::gate_of(",
        1
    )));
}

#[test]
fn every_read_that_fills_the_cache_cuts_its_ticket_first() {
    for v in ["verified_verdict", "verified_verdict_within"] {
        assert!(tickets_before_reading(&body(v)), "`{v}` stores an answer it cannot date");
    }
}

#[test]
fn a_ticket_cut_after_the_read_is_rejected() {
    // The lost update: the mode flips to `required` while the read is in flight, and the
    // read's own answer then overwrites the event's.
    let b = body("verified_verdict")
        .replace("let ticket = self.gate.ticket();", "")
        .replace("let v = Self::verdict_of", "let ticket = self.gate.ticket(); let v = Self::verdict_of");
    assert!(!tickets_before_reading(&b));
}

#[test]
fn the_cache_is_trusted_only_behind_a_live_subscription_and_dies_with_it() {
    let b = body("watch_gate");
    assert!(goes_live_behind_the_subscription(&b), "trust must start inside the listener");
    assert!(dies_closed(&b), "a feed that ends must drop what it was holding");
}

#[test]
fn arming_optimistically_or_dying_open_is_rejected() {
    let b = body("watch_gate");
    // Live before the subscription exists: a mode that moved in that window told nobody.
    assert!(!goes_live_behind_the_subscription(
        &b.replace("cache.feed_live();", "").replacen("let mut c =", "cache.feed_live(); let mut c =", 1)
    ));
    // A feed that ends leaving the cache trusted is the stale-open failure itself.
    assert!(!dies_closed(&b.replacen("cache.feed_dead();\n        });", "});", 1)));
}

/// The cache is written from exactly two places, both accounted for above. A third would be
/// a fact nobody is obliged to correct.
#[test]
fn nothing_else_writes_the_cache() {
    let code = code_only(GLUE);
    assert_eq!(code.matches(".told(").count(), 1, "only the mode feed may assert a mode");
    assert_eq!(body("watch_gate").matches(".told(").count(), 1);
    assert_eq!(code.matches(".learned(").count(), 2, "only the two verdict reads may fill it");
    for v in ["verified_verdict", "verified_verdict_within"] {
        assert_eq!(body(v).matches(".learned(").count(), 1, "`{v}` must fill the cache once");
    }
}

#[test]
fn every_feed_is_armed_at_startup_and_retried_from_a_read() {
    let ctx = body("on_context_ready");
    for arm in ["self.watch_gate()", "self.watch_chain_config()", "self.watch_token_list()"] {
        assert!(ctx.contains(arm), "nothing arms `{arm}` at startup");
    }
    // A feed that ended releases its flag, so the read that needs it arms a fresh one.
    assert!(body("verified_verdict").contains("self.watch_gate()"));
    assert!(body("list_networks").contains("self.watch_chain_config()"));
    assert!(body("chain_catalogue").contains("self.watch_token_list()"));
}
