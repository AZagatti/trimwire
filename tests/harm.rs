//! Harm guard (Phase 0 of the pivot): the pruning profiles must never drop a
//! fact the session genuinely *depends on*.
//!
//! We plant a **unique-dependency** needle — an old, small, unique, non-denylisted
//! tool result that a later turn relies on (the kind of fact that has no
//! mitigation if dropped: it's not bloat to head/tail-trim, not a duplicate, not
//! an image, not denylisted) — and a **recent** needle, then assert both survive
//! pruning under *every* profile.
//!
//! This is the offline, deterministic half of the harm benchmark (substring
//! survival ≠ comprehension — a model-in-the-loop A/B is the other half — but a
//! dropped *unique* fact is a hard lower bound on what the model definitely can't
//! see). It is forward-looking: it stays green when the default profile flips to
//! aggressive, and it FAILS the build if a future change starts dropping
//! load-bearing context. Known/accepted drops (the *middle* of an oversized old
//! result, a whole denylisted/throwaway result) are deliberately NOT asserted
//! here — those have mitigations (head+tail salvage, offload, verb-class).

use serde_json::{Value, json};
use trimwire::config::{PROFILES, profile_baseline};
use trimwire::strategies;

fn assistant(id: &str, name: &str, cmd: &str) -> Value {
    json!({"role": "assistant", "content": [
        {"type": "text", "text": "working"},
        {"type": "tool_use", "id": id, "name": name, "input": {"command": cmd}},
    ]})
}
fn result(id: &str, content: &str) -> Value {
    json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": id, "content": content},
    ]})
}

/// A session where one OLD result holds a load-bearing fact (small, unique, from
/// a tool on no denylist/exempt list → no strategy legitimately targets it),
/// followed by enough recent turns to age it past every profile's keep-recent
/// window (the aggressive `high` keeps only 2).
fn session_with_unique_dependency() -> Vec<Value> {
    let mut m = vec![json!({"role": "user", "content": "start the task"})];

    // The load-bearing fact. `Bash` is on no denylist (high denylists mcp__*+Grep)
    // and not exempt; the result is small (no bloat_cap) and unique (no dedup).
    m.push(assistant("t_dep", "Bash", "cat config.toml"));
    m.push(result("t_dep", "port = 8080  # NEEDLE_UNIQUE_DEP"));

    // Age it well past keep_recent for all profiles.
    for i in 0..6 {
        let id = format!("t{i}");
        m.push(assistant(&id, "Bash", &format!("echo {i}")));
        m.push(result(&id, &format!("out {i}")));
    }

    // Most-recent result (must always survive).
    m.push(assistant("t_now", "Bash", "echo done"));
    m.push(result("t_now", "current step: NEEDLE_RECENT"));
    m
}

/// A `tool_use` with an arbitrary input object (Read/Write carry `file_path`).
fn tool_call(id: &str, name: &str, input: Value) -> Value {
    json!({"role": "assistant", "content": [
        {"type": "text", "text": "working"},
        {"type": "tool_use", "id": id, "name": name, "input": input},
    ]})
}

/// A session exercising the two cache-safe levers now ON in `default`
/// (`stale_input_cap`, `stale_reads`). It plants:
/// - `NEEDLE_READ_LIVE` — a Read never superseded → must ALWAYS survive.
/// - `NEEDLE_PATH` / `NEEDLE_WRITE_BODY` — the `file_path` AND the bulky
///   `new_string` of an old successful Write → must ALWAYS survive. Authored file
///   content is EXEMPT from stale_input_cap (eliding it corrupted real sessions —
///   the model reproduced the elision marker as file content). This is the
///   corruption-regression gate.
/// - `NEEDLE_BULK` — the bulky `stdin` of an old successful Bash call (a
///   NON-authoring input) → elided by stale_input_cap in `default`, intact in `gentle`.
/// - `NEEDLE_READ_STALE` — a Read later superseded by a Write of the same path
///   (elided by stale_reads in `default`; intact in `gentle`).
fn session_with_input_and_read_needles() -> Vec<Value> {
    let mut m = vec![json!({"role": "user", "content": "start the task"})];

    // A live Read never touched again → the only op on its path → must survive.
    // Read uses `path` (the convention the stale_reads unit tests + extract_path use).
    m.push(tool_call("r_live", "Read", json!({"path": "/src/live.rs"})));
    m.push(result(
        "r_live",
        "fn live() { let port = 8080; } // NEEDLE_READ_LIVE",
    ));

    // A Read later superseded by a Write of the SAME path → stale_reads elides it
    // in `default`. Long enough to clear the shrink guard.
    m.push(tool_call(
        "r_stale",
        "Read",
        json!({"path": "/src/edit.rs"}),
    ));
    m.push(result(
        "r_stale",
        "// NEEDLE_READ_STALE old file contents the model later re-reads after editing \
         — padded so the elision marker is strictly smaller and the shrink guard fires.",
    ));

    // An old SUCCESSFUL Write: file_path (NEEDLE_PATH) AND the bulky new_string
    // (NEEDLE_WRITE_BODY) must BOTH survive — Write is exempt from stale_input_cap
    // (authored content must never be elided, or the model reproduces the marker).
    m.push(tool_call(
        "w_bulk",
        "Write",
        json!({
            "file_path": "/src/gen_NEEDLE_PATH.rs",
            "new_string": format!("// NEEDLE_WRITE_BODY\n{}", "x".repeat(2000)),
        }),
    ));
    m.push(result("w_bulk", "wrote successfully"));

    // An old SUCCESSFUL non-authoring call with bulky input: stale_input_cap DOES
    // elide its stdin (NEEDLE_BULK) in `default`, keeping the small `command`.
    m.push(tool_call(
        "b_bulk",
        "Bash",
        json!({
            "command": "psql < dump.sql",
            "stdin": format!("-- NEEDLE_BULK\n{}", "y".repeat(2000)),
        }),
    ));
    m.push(result("b_bulk", "loaded 2000 rows"));

    // The Write that supersedes /src/edit.rs (makes r_stale stale).
    m.push(tool_call(
        "w_edit",
        "Write",
        json!({"file_path": "/src/edit.rs", "new_string": "fn edited() {}"}),
    ));
    m.push(result("w_edit", "wrote /src/edit.rs"));

    // Age everything past keep_recent (the aggressive `default` keeps only 2).
    for i in 0..6 {
        let id = format!("t{i}");
        m.push(assistant(&id, "Bash", &format!("echo {i}")));
        m.push(result(&id, &format!("out {i}")));
    }
    m.push(assistant("t_now", "Bash", "echo done"));
    m.push(result("t_now", "current step: NEEDLE_RECENT"));
    m
}

fn survives(needle: &str, msgs: &[Value]) -> bool {
    serde_json::to_vec(msgs)
        .unwrap()
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// THE GATE: no profile may drop a unique-dependency fact or a recent result.
/// Iterates every shipped profile, so a renamed/added profile is covered too.
#[test]
fn no_profile_drops_a_unique_dependency_or_recent_fact() {
    for profile in PROFILES {
        let cfg = profile_baseline(profile);
        let mut msgs = session_with_unique_dependency();
        strategies::run(&mut msgs, &cfg).expect("strategies must not orphan");

        assert!(
            survives("NEEDLE_UNIQUE_DEP", &msgs),
            "profile `{profile}` dropped a UNIQUE-DEPENDENCY fact (old, small, unique, \
             non-denylisted result a later turn needs) — that's an unmitigated, \
             load-bearing context loss the agent can't recover."
        );
        assert!(
            survives("NEEDLE_RECENT", &msgs),
            "profile `{profile}` dropped a RECENT result — keep-recent-window regression."
        );
    }
}

/// THE GATE for the input/read levers: no profile may drop a live (never
/// superseded) Read or the structural `file_path` of an old successful call.
#[test]
fn no_profile_drops_a_live_read_or_a_structural_input_field() {
    for profile in PROFILES {
        let cfg = profile_baseline(profile);
        let mut msgs = session_with_input_and_read_needles();
        strategies::run(&mut msgs, &cfg).expect("strategies must not orphan");

        assert!(
            survives("NEEDLE_READ_LIVE", &msgs),
            "profile `{profile}` dropped a LIVE Read (never superseded) — stale_reads \
             must only elide reads a later op makes stale."
        );
        assert!(
            survives("NEEDLE_PATH", &msgs),
            "profile `{profile}` dropped the `file_path` of an old successful Write."
        );
        assert!(
            survives("NEEDLE_WRITE_BODY", &msgs),
            "profile `{profile}` elided an old Write's authored content (new_string) — \
             file-authoring tools MUST be exempt from stale_input_cap, or the model \
             reproduces the elision marker as file content (real corruption regression)."
        );
        assert!(
            survives("NEEDLE_RECENT", &msgs),
            "profile `{profile}` dropped a RECENT result — keep-recent-window regression."
        );
    }
}

/// Proves the two levers actually FIRE in `default` (so the gate above is not
/// vacuously green) and stay OFF in `gentle`. If `default` stopped pruning these,
/// the equality assertions below would fail, flagging silent strategy breakage.
#[test]
fn input_and_read_levers_fire_in_default_and_are_off_in_gentle() {
    let mut def_msgs = session_with_input_and_read_needles();
    strategies::run(&mut def_msgs, &profile_baseline("default")).unwrap();
    let mut gentle_msgs = session_with_input_and_read_needles();
    strategies::run(&mut gentle_msgs, &profile_baseline("gentle")).unwrap();

    // stale_input_cap: bulky old NON-authoring input (Bash stdin) elided in
    // default, intact in gentle. (Write/Edit content is exempt — tested above.)
    assert!(
        !survives("NEEDLE_BULK", &def_msgs),
        "stale_input_cap did not fire in `default` — old Bash stdin bulk should be elided"
    );
    assert!(
        survives("NEEDLE_BULK", &gentle_msgs),
        "`gentle` must NOT enable stale_input_cap — bulky input should stay intact"
    );
    // stale_reads: superseded read elided in default, intact in gentle.
    assert!(
        !survives("NEEDLE_READ_STALE", &def_msgs),
        "stale_reads did not fire in `default` — a superseded read should be elided"
    );
    assert!(
        survives("NEEDLE_READ_STALE", &gentle_msgs),
        "`gentle` must NOT enable stale_reads — a superseded read should stay intact"
    );
}

/// Demand-paging in `default` must be RECOVERABLE, never a silent drop: a huge old
/// read is replaced with a marker that NAMES the path and instructs a re-read, so
/// the model can self-heal. (The needle content itself is gone by design — paging
/// trades an in-context copy for re-fetchability; the gate is that the marker is
/// present + actionable, not that the content survives.)
#[test]
fn paging_leaves_a_recoverable_marker_not_a_silent_drop() {
    let mut m = vec![json!({"role": "user", "content": "start"})];
    // An old, HUGE (>32KB) Read, never superseded → paged in `default`.
    m.push(tool_call("r_huge", "Read", json!({"path": "/src/huge.rs"})));
    m.push(result(
        "r_huge",
        &format!("// NEEDLE_HUGE\n{}", "x".repeat(40_000)),
    ));
    // Age it well past keep_recent.
    for i in 0..8 {
        let id = format!("t{i}");
        m.push(assistant(&id, "Bash", &format!("echo {i}")));
        m.push(result(&id, &format!("out {i}")));
    }
    m.push(assistant("t_now", "Bash", "echo done"));
    m.push(result("t_now", "current: NEEDLE_RECENT"));

    strategies::run(&mut m, &profile_baseline("default")).expect("must not orphan");
    let blob = serde_json::to_vec(&m).unwrap();
    let s = String::from_utf8(blob).unwrap();
    // The huge content is paged (gone) BUT a recoverable marker took its place.
    assert!(
        !s.contains("NEEDLE_HUGE"),
        "huge old read should be paged out in `default`"
    );
    assert!(
        s.contains("paged out") && s.contains("/src/huge.rs") && s.contains("re-read"),
        "paging must leave a recoverable marker naming the path + instructing re-read"
    );
    assert!(survives("NEEDLE_RECENT", &m), "recent result must survive");
}

/// A session whose OLD assistant turn carries a `thinking` block AND load-bearing
/// facts in its sibling `text` block + the following `tool_result`, aged past BOTH
/// profiles' thinking_strip windows (default keep=4, gentle keep=8). Proves that
/// stripping old thinking (now ON in gentle too) removes ONLY the reasoning, never
/// the facts beside it — closing the review's "thinking_strip-in-gentle is only
/// vacuously covered" gap.
fn session_with_old_thinking_and_facts() -> Vec<Value> {
    let mut m = vec![json!({"role": "user", "content": "start the task"})];
    // Oldest assistant turn: thinking (should be stripped) + a fact in text + a tool_use.
    m.push(json!({"role": "assistant", "content": [
        {"type": "thinking", "thinking": "NEEDLE_OLD_THINKING reasoning that may be stripped", "signature": "sig0"},
        {"type": "text", "text": "decided: NEEDLE_FACT_IN_TEXT"},
        {"type": "tool_use", "id": "k0", "name": "Bash", "input": {"command": "echo plan"}},
    ]}));
    m.push(result("k0", "port = 8080  # NEEDLE_FACT_IN_RESULT"));
    // Age it past gentle's keep_recent=8 (and default's 4): need >8 later assistant turns.
    for i in 0..10 {
        let id = format!("a{i}");
        m.push(assistant(&id, "Bash", &format!("echo {i}")));
        m.push(result(&id, &format!("out {i}")));
    }
    m.push(assistant("t_now", "Bash", "echo done"));
    m.push(result("t_now", "current: NEEDLE_RECENT"));
    m
}

/// THE GATE for thinking_strip-in-gentle: stripping old thinking must NEVER drop a
/// fact in a sibling text block or the following tool_result. Runs over EVERY profile.
#[test]
fn no_profile_drops_a_fact_beside_stripped_thinking() {
    for profile in PROFILES {
        let cfg = profile_baseline(profile);
        let mut msgs = session_with_old_thinking_and_facts();
        strategies::run(&mut msgs, &cfg).expect("strategies must not orphan");
        assert!(
            survives("NEEDLE_FACT_IN_TEXT", &msgs),
            "profile `{profile}` dropped a fact in the text block of a thinking-bearing turn"
        );
        assert!(
            survives("NEEDLE_FACT_IN_RESULT", &msgs),
            "profile `{profile}` dropped a tool_result fact beside stripped thinking"
        );
        assert!(
            survives("NEEDLE_RECENT", &msgs),
            "profile `{profile}` dropped a recent result"
        );
    }
}

// ---- §13 regression: deterministic elision must not corrupt authored content
//      nor force a read-spiral ----

/// §13A reproduction: an authored Write body that a LATER op on the same path
/// supersedes must NOT be collapsed by `stale_reads` behavior #2. Models the
/// ubiquitous create→read→edit flow on one path:
///   Write P (full body) → Read P (verify) → Edit P (modify)
/// After this, behavior #1 elides the Read result AND behavior #2 collapses the
/// Write body → the only authored copy of P's content left in context is the tiny
/// Edit diff. The full body (`NEEDLE_AUTHORED`) must survive: authored file
/// content is load-bearing for re-authoring. Eliding it is exactly what corrupted
/// a real session (a 9.5KB Write landed on disk as `[trimwire: 9558B input
/// elided]` — the model reproduced the marker as the file body).
fn session_with_superseded_authored_content() -> Vec<Value> {
    let mut m = vec![json!({"role": "user", "content": "build the file"})];
    // Create a new file (Write needs no prior Read).
    m.push(tool_call(
        "w_auth",
        "Write",
        json!({
            "file_path": "/src/perf.rs",
            "content": format!("// NEEDLE_AUTHORED\n{}", "x".repeat(4000)),
        }),
    ));
    m.push(result("w_auth", "wrote /src/perf.rs"));
    // Read it back to verify — supersedes the Write per stale_reads.
    m.push(tool_call("r_back", "Read", json!({"path": "/src/perf.rs"})));
    m.push(result(
        "r_back",
        &format!("// NEEDLE_AUTHORED\n{}", "x".repeat(4000)),
    ));
    // Edit it — supersedes the Read; the last op on the path.
    m.push(tool_call(
        "e_auth",
        "Edit",
        json!({"file_path": "/src/perf.rs", "old_string": "x", "new_string": "y"}),
    ));
    m.push(result("e_auth", "edited /src/perf.rs"));
    // Age everything past keep_recent (aggressive `default` keeps only 2).
    for i in 0..6 {
        let id = format!("t{i}");
        m.push(assistant(&id, "Bash", &format!("echo {i}")));
        m.push(result(&id, &format!("out {i}")));
    }
    m.push(assistant("t_now", "Bash", "echo done"));
    m.push(result("t_now", "current step: NEEDLE_RECENT"));
    m
}

/// THE GATE (§13A): no profile may elide authored Write/Edit/MultiEdit content,
/// even when a later op on the same path supersedes it. Authored content is the
/// model's only faithful copy of what it wrote; eliding it makes the model
/// reproduce the elision marker as the file body (real corruption).
#[test]
fn no_profile_elides_superseded_authored_content() {
    for profile in PROFILES {
        let cfg = profile_baseline(profile);
        let mut msgs = session_with_superseded_authored_content();
        strategies::run(&mut msgs, &cfg).expect("strategies must not orphan");
        assert!(
            survives("NEEDLE_AUTHORED", &msgs),
            "profile `{profile}` elided an authored Write body that a later op \
             superseded — the model loses its only copy of the file content it is \
             actively editing and reproduces the elision marker as the file body \
             (§13A corruption regression)."
        );
        assert!(
            survives("NEEDLE_RECENT", &msgs),
            "profile `{profile}` dropped a RECENT result — keep-recent-window regression."
        );
    }
}

/// §13B reproduction (read-spiral): a path the model has Read more than once (a
/// "hot" file it keeps needing) must NOT be demand-paged. Paging the current view
/// of a hot file forces yet another re-read, which trimwire pages out again →
/// the read-spiral observed live ("stuck re-reading the same file repeatedly").
fn session_with_hot_reread_path() -> Vec<Value> {
    let mut m = vec![json!({"role": "user", "content": "work on the big file"})];
    // First read of a huge file.
    m.push(tool_call("r0", "Read", json!({"path": "/src/huge.rs"})));
    m.push(result(
        "r0",
        &format!("// hot file v0\n{}", "x".repeat(40_000)),
    ));
    for i in 0..3 {
        let id = format!("a{i}");
        m.push(assistant(&id, "Bash", &format!("echo {i}")));
        m.push(result(&id, &format!("out {i}")));
    }
    // The model RE-READS the same huge file — it clearly still needs it.
    m.push(tool_call("r1", "Read", json!({"path": "/src/huge.rs"})));
    m.push(result(
        "r1",
        &format!("// hot file v0 NEEDLE_HOT\n{}", "x".repeat(40_000)),
    ));
    // Age the re-read past keep_recent so demand-paging would target it.
    for i in 0..6 {
        let id = format!("b{i}");
        m.push(assistant(&id, "Bash", &format!("echo {i}")));
        m.push(result(&id, &format!("out {i}")));
    }
    m.push(assistant("t_now", "Bash", "echo done"));
    m.push(result("t_now", "current: NEEDLE_RECENT"));
    m
}

/// THE GATE (§13B): a hot re-read path's current view must survive in `default`
/// (demand-paging must skip paths read more than once), or the model re-reads in
/// a loop.
#[test]
fn default_does_not_page_a_hot_reread_path() {
    let mut msgs = session_with_hot_reread_path();
    strategies::run(&mut msgs, &profile_baseline("default")).expect("must not orphan");
    assert!(
        survives("NEEDLE_HOT", &msgs),
        "`default` demand-paged the current view of a file the model has re-read \
         (a hot path) — paging it forces another re-read → the §13B read-spiral. \
         Demand-paging must skip paths read more than once."
    );
}

/// Proves the gate above is NOT vacuous: both profiles (thinking_strip is now ON in
/// gentle too) actually STRIP the old reasoning, so the fact-survival assertions are
/// meaningful, not "nothing happened".
#[test]
fn thinking_strip_actually_fires_in_both_profiles() {
    for profile in PROFILES {
        let mut msgs = session_with_old_thinking_and_facts();
        strategies::run(&mut msgs, &profile_baseline(profile)).unwrap();
        assert!(
            !survives("NEEDLE_OLD_THINKING", &msgs),
            "profile `{profile}` should strip the OLD thinking block (aged past keep_recent)"
        );
    }
}
