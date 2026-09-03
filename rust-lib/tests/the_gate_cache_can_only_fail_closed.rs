//! The gate cache, driven through the surface a shipped build actually has, and then read as
//! source. `crate::gate` proves the cache's own rules in isolation; what cannot be proved
//! there is that `glue.rs` USES it the one way those rules assume:
//!
//! * the gate is skipped only for `Gate::Open` — a chain eth_rpc told us is `off`, where its
//!   own `blocking = mode_required && !usable` cannot depend on the proxy health this skips;
//! * a read cuts its ticket BEFORE going out, so an event that overtakes it wins;
//! * the cache is opened by `SubStatus::Armed` and by nothing else — ONE edge, in `arm_gate`,
//!   behind a flag that is only raised once a mode subscription exists for that arm to be
//!   about. A re-subscribe after a dead feed goes back through the same edge: a new
//!   subscription is unarmed at creation, so "the re-subscribe worked" opens nothing;
//! * every way the feed can stop delivering drops what it was holding.
//!
//! `glue.rs` is behind the `logos_module` feature and `--no-default-features` cannot compile
//! it, so it is read as text, and each source check ships with the mutant it must kill — a
//! check that stopped discriminating then fails rather than passes. The first two tests are
//! not source reads at all: they drive `ModeCache` through the call sequences `arm_gate` and
//! `gate_feed` produce, which is the property every source check below only approximates.
//!
//! Nothing here waits on anything. The bug this file exists for survived because a stream
//! that never ended made the assertion after it unreachable, and an unreachable assertion
//! does not fail — it hangs, or it passes vacuously. Every check below is a decision over
//! values already in hand.

use eth_wallet_backend::gate::{Gate, ModeCache, MODE_OFF};

/// What opens this cache, driven in the order a live wallet produces it. `feed_live` is
/// reachable from a shipped build now — that IS the restore — so what has to be shown is that
/// reaching it takes an arm, and that nothing short of one gets there.
#[test]
fn only_an_arm_opens_the_cache() {
    let c = ModeCache::default();
    assert_eq!(c.gate(1), Gate::Ask, "a fresh cache trusts nothing");

    // A subscription exists and has not armed: what `arm_gate` holds between `on()` returning
    // and the status channel saying so. The deferred arm is exactly this state, indefinitely.
    c.told(1, MODE_OFF);
    c.learned(1, Some(MODE_OFF), c.ticket());
    assert_eq!(c.gate(1), Gate::Ask, "a subscription handle is not an arm");

    // `SubStatus::Armed`, and then the mode feed's own word for the chain.
    c.feed_live();
    assert_eq!(c.gate(1), Gate::Ask, "an arm restores the FEED, not a fact it never carried");
    c.told(1, MODE_OFF);
    assert_eq!(c.gate(1), Gate::Open);
    assert_eq!(c.gate(10), Gate::Ask, "and per chain, still");
}

/// The recovery, and the fail-open it must not become. `Abandoned` is terminal, so the way
/// back is a NEW subscription — and a new subscription is unarmed at creation. Every call the
/// re-subscribe path can make short of an arm is driven here.
#[test]
fn a_re_subscribe_alone_does_not_open_the_cache() {
    let c = ModeCache::default();
    c.feed_live();
    c.told(1, MODE_OFF);
    assert_eq!(c.gate(1), Gate::Open);

    // The feed dies: `Abandoned` woke the reader, or the stream simply ended.
    c.feed_dead();
    assert_eq!(c.gate(1), Gate::Ask);

    // `arm_gate` runs again and succeeds. The mode has not changed and eth_rpc still answers
    // `off` — none of which is anybody being obliged to tell us when it stops being true.
    c.told(1, MODE_OFF);
    c.invalidate(1);
    c.told(1, MODE_OFF);
    c.learned(1, Some(MODE_OFF), c.ticket());
    assert_eq!(c.gate(1), Gate::Ask, "taking a subscription is not being armed on one");

    // Only the new subscription's own arm reopens it, through the same single edge.
    c.feed_live();
    c.told(1, MODE_OFF);
    assert_eq!(c.gate(1), Gate::Open);

    // And a runtime that can never arm cannot be talked round by the retry loop either.
    let c = ModeCache::default();
    c.no_status_channel();
    for _ in 0..3 {
        c.feed_dead();
        c.feed_live();
        c.told(1, MODE_OFF);
        assert_eq!(c.gate(1), Gate::Ask, "no status channel means no cached answer, ever");
    }
}

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

fn sites(hay: &str, needle: &str) -> Vec<usize> {
    let (mut out, mut from) = (Vec::new(), 0);
    while let Some(rel) = hay[from..].find(needle) {
        out.push(from + rel);
        from += rel + needle.len();
    }
    out
}

/// Brace depth of `at` inside a function body that starts at its own `{`. Depth 1 is a
/// statement of the function itself; anything deeper sits inside a branch, block or closure
/// that some runtime could skip.
fn depth_at(body: &str, at: usize) -> i32 {
    body[..at].chars().fold(0i32, |d, c| match c {
        '{' => d + 1,
        '}' => d - 1,
        _ => d,
    })
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

/// The `else` block of a let-else whose scrutinee starts at `head`.
fn else_block<'a>(b: &'a str, head: &str) -> &'a str {
    let Some(at) = b.find(head) else { return "" };
    let Some(rel) = b[at..].find("else {") else { return "" };
    let e = at + rel;
    &b[e..closes(b, e, '{', '}')]
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

/// ONE edge opens this cache, and it is `SubStatus::Armed` reaching a watcher that already
/// has a mode subscription behind it. Both halves are load-bearing: a `feed_live` anywhere
/// else is a second way in, and an unguarded `Armed` arm opens the cache on the replay the
/// install fires — the status is per TARGET, so a client already armed for the chain-config
/// feed reports `Armed` at a moment nothing is listening for modes.
fn opens_only_on_an_arm(code: &str) -> bool {
    if sites(code, "feed_live").len() != 1 {
        return false;
    }
    let arm = body_of(code, "arm_gate");
    match (arm.find("SubStatus::Armed if subscribed.load"), arm.find("feed_live")) {
        (Some(guard), Some(live)) => guard < live && arm.contains("_ => cache.feed_dead()"),
        _ => false,
    }
}

/// The watcher is installed BEFORE the subscription — the install replays the current state
/// synchronously, so there is no gap for an arm to fall into — and the flag that lets an
/// `Armed` through is raised only once that subscription exists.
fn watches_before_it_subscribes(arm: &str) -> bool {
    match (
        arm.find("on_subscription_status"),
        arm.find("on_verified_proxy_mode_changed"),
        arm.find("subscribed.store(true"),
    ) {
        (Some(w), Some(s), Some(f)) => w < s && s < f,
        _ => false,
    }
}

/// The cold latch is taken exactly where it belongs now: on a runtime with NO status channel,
/// where nothing can ever report an arm. It is one statement inside that one branch — taken
/// unconditionally it would make the restored arm inert on every runtime.
fn latches_cold_only_without_a_status_channel(b: &str) -> bool {
    let latch = sites(b, "no_status_channel(");
    let Some(guard) = b.find("if !gate::status_channel(") else { return false };
    latch.len() == 1 && depth_at(b, latch[0]) == 2 && guard < latch[0]
}

/// Every way the feed can stop delivering drops what it was holding: an arming that produced
/// no subscription, an event we cannot decode, and a stream that ended.
fn dies_closed(b: &str) -> bool {
    // The statement AFTER the stream loop closes, not merely one below where it opens: the
    // undecodable-event arm sits inside the loop and answered a `rfind` just as well.
    let Some(loop_at) = b.find("for ev in sub") else { return false };
    let after = closes(b, loop_at, '{', '}');
    let tail = &b[after..];
    let stream_ended = tail[..tail.find('}').unwrap_or(tail.len())].contains("cache.feed_dead();");
    else_block(b, "arm_gate(cache)").contains("feed_dead")
        && else_block(b, "decode_verified_proxy_mode_changed").contains("feed_dead")
        && stream_ended
}

/// Recovery is a NEW subscription taken through the SAME arming, and it is bounded: a
/// provider that is gone for good must not be spun on, and the flag `listen` releases when
/// this returns is what lets the next gated read try again.
fn recovers_by_resubscribing(b: &str) -> bool {
    b.contains("for attempt in 0..=GATE_REARMS")
        && b.matches("arm_gate(cache)").count() == 1
        && !b.contains("feed_live")
}

// ── the gate itself ────────────────────────────────────────────────────────────────────

/// One gate, and it is the bounded one: the unbounded twins were deleted when the last four
/// gate sites were budgeted, so there is no second shape for this rule to miss.
#[test]
fn the_gate_skips_the_hop_only_for_a_chain_told_to_be_off() {
    assert!(
        skips_only_when_open(&body("verified_gate_within")),
        "`verified_gate_within` does not skip on `Gate::Open` alone"
    );
}

#[test]
fn a_gate_that_skips_on_anything_else_is_rejected() {
    let b = body("verified_gate_within");
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
    assert!(
        tickets_before_reading(&body("verified_verdict_within")),
        "`verified_verdict_within` stores an answer it cannot date"
    );
}

#[test]
fn a_ticket_cut_after_the_read_is_rejected() {
    // The lost update: the mode flips to `required` while the read is in flight, and the
    // read's own answer then overwrites the event's.
    let b = body("verified_verdict_within")
        .replace("let ticket = self.gate.ticket();", "")
        .replace("let v = Self::verdict_of", "let ticket = self.gate.ticket(); let v = Self::verdict_of");
    assert!(!tickets_before_reading(&b));
}

// ── the arm ────────────────────────────────────────────────────────────────────────────

#[test]
fn the_cache_is_opened_by_an_arm_and_by_nothing_else() {
    assert!(
        opens_only_on_an_arm(&code_only(GLUE)),
        "`feed_live` must be reached from `SubStatus::Armed` behind the subscribed flag, once"
    );
}

#[test]
fn opening_the_cache_any_other_way_is_rejected() {
    let code = code_only(GLUE);

    // The shipped bug: trust taken from the SUBSCRIPTION rather than from an arm. The handle
    // is non-null the moment lp_subscribe defers it, so this opens the cache over exactly
    // the window `on_context_ready` subscribes in.
    assert!(!opens_only_on_an_arm(&code.replacen(
        "    subscribed.store(true, Ordering::Release);",
        "    cache.feed_live();\n    subscribed.store(true, Ordering::Release);",
        1
    )));
    // The arm ungated: the install's own replay then opens the cache before there is a mode
    // subscription, on a client armed for the chain-config feed alone.
    assert!(!opens_only_on_an_arm(
        &code.replacen("SubStatus::Armed if subscribed.load(Ordering::Acquire)", "SubStatus::Armed", 1)
    ));
    // Recovery given its own edge — "the re-subscribe succeeded" as permission.
    assert!(!opens_only_on_an_arm(&code.replacen(
        "        let Some(sub) = arm_gate(cache) else {",
        "        cache.feed_live();\n        let Some(sub) = arm_gate(cache) else {",
        1
    )));
    // Every status treated as an arm, so a `Lost` reads as a live feed.
    assert!(!opens_only_on_an_arm(&code.replacen("_ => cache.feed_dead()", "_ => {}", 1)));
}

#[test]
fn the_watcher_is_installed_before_the_subscription_it_is_about() {
    assert!(
        watches_before_it_subscribes(&body("arm_gate")),
        "an arm delivered between `on()` and the install would reach nobody"
    );
}

#[test]
fn a_watcher_installed_after_the_flag_is_raised_is_rejected() {
    let b = body("arm_gate");
    // The flag raised before the subscription exists: the install's replay is then read as
    // this subscription's own arm.
    assert!(!watches_before_it_subscribes(
        &b.replacen("    let mut w = modules().eth_rpc_module;", "    subscribed.store(true, Ordering::Release);\n    let mut w = modules().eth_rpc_module;", 1)
    ));
    // And the watcher installed only after the subscription, which is the gap itself.
    assert!(!watches_before_it_subscribes(
        &b.replace("w.on_subscription_status", "w.installed_later")
    ));
}

#[test]
fn a_runtime_without_the_status_channel_latches_the_cache_cold() {
    let b = body("watch_gate");
    assert!(
        latches_cold_only_without_a_status_channel(&b),
        "`watch_gate` must latch cold exactly when the runtime cannot report an arm"
    );
    assert!(b.contains("eprintln!"), "and say so where an operator would see it");
}

#[test]
fn a_latch_that_moved_off_its_condition_is_rejected() {
    let b = body("watch_gate");
    // Latched on every runtime: the restored arm is then inert everywhere, and the wallet
    // pays a live probe per gated check for ever with nothing reporting why.
    assert!(!latches_cold_only_without_a_status_channel(
        &b.replacen("if !gate::status_channel(&logos_rust_sdk::protocol_version()) {", "if true {", 1)
    ));
    // Latched nowhere: a runtime that can never report an arm would then be trusted the
    // moment a stray `feed_live` reached the cache.
    assert!(!latches_cold_only_without_a_status_channel(
        &b.replacen("cache.no_status_channel();", "", 1)
    ));
    // The condition inverted, which is the same hole wearing the right vocabulary.
    assert!(!latches_cold_only_without_a_status_channel(
        &b.replacen("if !gate::status_channel(", "if gate::status_channel(", 1)
    ));
}

// ── the feed, and the recovery ─────────────────────────────────────────────────────────

#[test]
fn every_way_the_feed_stops_drops_what_it_was_holding() {
    assert!(
        dies_closed(&body("gate_feed")),
        "an arming that failed, an event we cannot read, and a stream that ended must all close it"
    );
}

#[test]
fn a_feed_that_stopped_leaving_the_cache_trusted_is_rejected() {
    let b = body("gate_feed");
    // A stream that ended, leaving the cache open over a window nobody is watching.
    assert!(!dies_closed(&b.replacen("        cache.feed_dead();\n    }\n}", "    }\n}", 1)));
    // An arming that produced nothing at all.
    assert!(!dies_closed(&b.replacen(
        "        let Some(sub) = arm_gate(cache) else {\n            cache.feed_dead();",
        "        let Some(sub) = arm_gate(cache) else {",
        1
    )));
    // An event whose shape we no longer share: the contract is gone, and it names no chain.
    assert!(!dies_closed(&b.replacen(
        "                cache.feed_dead();\n                return;",
        "                return;",
        1
    )));
}

#[test]
fn a_lost_feed_comes_back_by_taking_a_new_subscription() {
    assert!(
        recovers_by_resubscribing(&body("gate_feed")),
        "`gate_feed` must re-subscribe through `arm_gate`, boundedly, and open nothing itself"
    );
    // The flag is released when the thread exits, which is what lets a gated read try again
    // after the retries run out. `listen` owns that, and `watch_gate` hands the feed to it.
    assert!(body("watch_gate").contains("listen(self.feeds.gate.clone()"));
}

#[test]
fn a_recovery_that_opens_the_cache_or_never_gives_up_is_rejected() {
    let b = body("gate_feed");
    // Unbounded: a provider that is never coming back is then spun on for the life of the
    // process, and the flag is never released for a read to re-arm from.
    assert!(!recovers_by_resubscribing(&b.replacen("for attempt in 0..=GATE_REARMS", "loop", 1)));
    // A second arming that skips the watcher — the shape where re-subscribing IS the arm.
    assert!(!recovers_by_resubscribing(
        &b.replacen("        cache.feed_dead();\n    }\n}", "        cache.feed_live();\n    }\n}", 1)
    ));
}

// ── the writers ────────────────────────────────────────────────────────────────────────

/// The cache is written from exactly three places, each accounted for above. A fourth would
/// be a fact nobody is obliged to correct.
#[test]
fn nothing_else_writes_the_cache() {
    let code = code_only(GLUE);
    assert_eq!(code.matches(".told(").count(), 1, "only the mode feed may assert a mode");
    assert_eq!(body("gate_feed").matches(".told(").count(), 1);
    assert_eq!(code.matches(".learned(").count(), 1, "only the verdict read may fill it");
    assert_eq!(body("verified_verdict_within").matches(".learned(").count(), 1);
    assert_eq!(code.matches("feed_live").count(), 1, "only an arm may open it");
    assert_eq!(body("arm_gate").matches("feed_live").count(), 1);
}

#[test]
fn every_feed_is_armed_at_startup_and_retried_from_a_read() {
    let ctx = body("on_context_ready");
    for arm in ["self.watch_gate()", "self.watch_chain_config()", "self.watch_token_list()"] {
        assert!(ctx.contains(arm), "nothing arms `{arm}` at startup");
    }
    // A feed that ended releases its flag, so the read that needs it arms a fresh one.
    assert!(body("verified_verdict_within").contains("self.watch_gate()"));
    assert!(body("list_networks").contains("self.watch_chain_config()"));
    assert!(body("chain_catalogue").contains("self.watch_token_list()"));
}
