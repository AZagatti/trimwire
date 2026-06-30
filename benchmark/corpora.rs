//! Shared benchmark harness: deterministic synthetic corpora + the metrics
//! computed over them. Included verbatim (via `#[path]`) by both
//! `examples/bench.rs` (the report) and `tests/benchmark.rs` (the guards), so
//! the numbers and the assertions can never drift apart. Lives outside `src/`
//! so it never ships in the published crate.
//!
//! Everything here is a pure function of `(corpus, config)` — the savings,
//! ablation, attribution, cache-stability, and cost figures are exact and
//! reproducible bit-for-bit. Only the wall-clock timing in the example is
//! host-dependent.

use std::collections::HashMap;

use serde_json::{Value, json};
use trimwire::config::Config;
use trimwire::pairing::PairingIndex;
use trimwire::strategies::{self, BodyOutcome};

// ===========================================================================
// Generators
// ===========================================================================

/// `n` bytes of deterministic log-like text, salted with `tag` so different
/// calls produce *different* content (real tool output is rarely identical).
pub fn lines(n: usize, tag: &str) -> String {
    let mut s = String::with_capacity(n + 80);
    let mut i = 0usize;
    while s.len() < n {
        s.push_str(&format!(
            "  [{tag}] line {i:05}: processed record, advancing cursor, flushing buffer\n"
        ));
        i += 1;
    }
    s.truncate(n);
    s
}

/// `n` bytes over the base64 alphabet — what Claude Code's screenshot tools
/// emit, and what `image_strip` recognises as an image payload.
pub fn b64_blob(n: usize, tag: &str) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let salt = tag.bytes().map(|b| b as usize).sum::<usize>();
    (0..n)
        .map(|i| ALPHA[(i.wrapping_mul(31).wrapping_add(salt)) % ALPHA.len()] as char)
        .collect()
}

fn assistant_call(narration: &str, id: &str, name: &str, input: Value) -> Value {
    json!({
        "role": "assistant",
        "content": [
            {"type": "text", "text": narration},
            {"type": "tool_use", "id": id, "name": name, "input": input},
        ],
    })
}

/// An assistant turn that issues several tool calls at once (the parallel
/// pattern real Claude Code uses).
fn assistant_parallel(narration: &str, calls: Vec<Value>) -> Value {
    let mut content = vec![json!({"type": "text", "text": narration})];
    content.extend(calls);
    json!({"role": "assistant", "content": content})
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

fn user_result(id: &str, content: Value, is_error: bool) -> Value {
    let mut block = json!({"type": "tool_result", "tool_use_id": id, "content": content});
    if is_error {
        block["is_error"] = Value::Bool(true);
    }
    json!({ "role": "user", "content": [block] })
}

fn user_results(results: Vec<Value>) -> Value {
    json!({"role": "user", "content": results})
}

fn result_block(id: &str, content: Value, is_error: bool) -> Value {
    let mut block = json!({"type": "tool_result", "tool_use_id": id, "content": content});
    if is_error {
        block["is_error"] = Value::Bool(true);
    }
    block
}

fn assistant_text(text: &str) -> Value {
    json!({"role": "assistant", "content": [{"type": "text", "text": text}]})
}

fn user_text(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

/// Wrap a `messages[]` array in a realistic request envelope — a multi-block
/// `system` with cache breakpoints (like real Claude Code), `tools`, etc. Only
/// `messages[]` is ever mutated; everything here must survive untouched.
fn envelope(messages: Vec<Value>) -> Value {
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 8192,
        "system": [
            {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
            {"type": "text", "text": lines(1200, "sysprompt"),
             "cache_control": {"type": "ephemeral"}},
        ],
        "tools": [{"name": "Bash"}, {"name": "Read"}, {"name": "Edit"}, {"name": "Grep"}],
        "messages": messages,
    })
}

/// A generated request body and the session shape it models.
pub struct Corpus {
    pub name: &'static str,
    pub profile: &'static str,
    /// One-line plain-English note for the report.
    pub note: &'static str,
    pub body: Value,
}

impl Corpus {
    pub fn messages(&self) -> &[Value] {
        self.body["messages"].as_array().expect("messages[]")
    }
}

// ---- the corpora ----------------------------------------------------------

/// No tools at all — plain Q&A. The true floor: trimwire forwards verbatim.
fn pure_chat_floor() -> Corpus {
    let mut m = Vec::new();
    for i in 0..6 {
        m.push(user_text(&format!(
            "Question {i}: can you explain how {} works and when I'd use it?",
            [
                "lifetimes",
                "trait objects",
                "async cancellation",
                "Pin",
                "Send+Sync",
                "Drop order"
            ][i]
        )));
        m.push(assistant_text(&lines(900 + i * 200, &format!("answer{i}"))));
    }
    Corpus {
        name: "pure_chat_floor",
        profile: "plain Q&A, no tools at all",
        note: "the floor: nothing to prune, forwarded byte-for-byte",
        body: envelope(m),
    }
}

/// Many unique large file-AUTHORING (`Write`/`Edit`) results. These are
/// load-bearing and exempt from bloat_cap at EVERY age (eliding them corrupts
/// sessions), inputs all distinct → nothing prunes. The honest low case the
/// headline numbers conveniently omit. NB: `Read` is deliberately NOT here —
/// since the "Read coverage gap" fix, OLD large Read results DO get trimmed
/// (recent reads stay protected), so a Read-heavy session is no longer a floor.
fn exempt_heavy() -> Corpus {
    let mut m = Vec::new();
    for i in 0..9 {
        let id = format!("t_wr_{i}");
        let (name, input) = if i % 2 == 0 {
            (
                "Write",
                json!({"file_path": format!("src/module_{i}.rs"), "content": "fn x() {}"}),
            )
        } else {
            (
                "Edit",
                json!({"file_path": format!("src/module_{i}.rs"), "old": "a", "new": "b"}),
            )
        };
        m.push(assistant_call("Authoring the next file.", &id, name, input));
        m.push(user_result(
            &id,
            json!(lines(6_000, &format!("file{i}"))),
            false,
        ));
    }
    Corpus {
        name: "exempt_heavy",
        profile: "9 unique Write/Edit results (authoring — exempt at every age, all distinct)",
        note: "honest low case: load-bearing authoring content → ~nothing to prune",
        body: envelope(m),
    }
}

/// Many *distinct* large Bash outputs. No dedup (unique), and only the few
/// oldest past the keep-recent window get bloat_capped → modest, realistic.
fn unique_bash_spam() -> Corpus {
    let mut m = Vec::new();
    for i in 0..12 {
        let id = format!("t_bash_{i}");
        m.push(assistant_call(
            "Running the next diagnostic.",
            &id,
            "Bash",
            json!({"command": format!("./diagnose --case {i}")}),
        ));
        // Each output is large and UNIQUE (a distinct stack trace / log).
        m.push(user_result(
            &id,
            json!(lines(18_000, &format!("trace{i}"))),
            false,
        ));
    }
    Corpus {
        name: "unique_bash_spam",
        profile: "12 distinct ~18 KB Bash outputs (nothing repeats)",
        note: "only the oldest results past the recent window get capped",
        body: envelope(m),
    }
}

/// Many *distinct* large `Read` results (distinct paths → no supersession or dedup;
/// each ~8 KB → over bloat_cap's 4 KB threshold but under stale_reads' 32 KB page
/// floor, so ONLY bloat_cap can touch them). The OLD ones (past keep_recent) are now
/// trimmed because `Read` is age-gated (the "Read coverage gap" fix); the recent ones
/// stay intact. The integration-level proof that fix #1 is wired end-to-end.
fn read_heavy() -> Corpus {
    let mut m = Vec::new();
    for i in 0..10 {
        let id = format!("t_read_{i}");
        m.push(assistant_call(
            "Reading the next file.",
            &id,
            "Read",
            json!({"file_path": format!("src/distinct_module_{i}.rs")}),
        ));
        m.push(user_result(
            &id,
            json!(lines(8_000, &format!("read{i}"))),
            false,
        ));
    }
    Corpus {
        name: "read_heavy",
        profile: "10 distinct ~8 KB Read results (distinct paths, no dedup/supersession)",
        note: "old reads bloat_capped now Read is age-gated (coverage-gap fix); recent reads intact",
        body: envelope(m),
    }
}

/// Rigor corpus: an oversized log one turn either side of the keep_recent
/// boundary. Old ones trimmed, recent ones protected — proves the window.
fn at_the_boundary() -> Corpus {
    let mut m = Vec::new();
    for i in 0..7 {
        let id = format!("t_b_{i}");
        m.push(assistant_call(
            "Dumping the build log.",
            &id,
            "Bash",
            json!({"command": format!("build --variant {i}")}),
        ));
        m.push(user_result(
            &id,
            json!(lines(20_000, &format!("buildlog{i}"))),
            false,
        ));
    }
    Corpus {
        name: "at_the_boundary",
        profile: "7 oversized logs straddling the keep-recent-4 boundary",
        note: "recent results stay intact; only aged ones are capped",
        body: envelope(m),
    }
}

/// Agentic search loop: the same Grep run many times (identical input) plus a
/// few distinct ones. dedup is the only meaningful strategy here.
fn repeated_grep() -> Corpus {
    let mut m = Vec::new();
    // 8 identical Greps (same pattern+path) → dedup stubs 7.
    for i in 0..8 {
        let id = format!("t_grep_{i}");
        m.push(assistant_call(
            "Searching for the symbol again.",
            &id,
            "Grep",
            json!({"pattern": "fn apply", "path": "src", "output_mode": "content"}),
        ));
        m.push(user_result(&id, json!(lines(2_500, "grephits")), false));
    }
    // 4 distinct Greps (control: must NOT dedup).
    for i in 0..4 {
        let id = format!("t_grepd_{i}");
        m.push(assistant_call(
            "A different search.",
            &id,
            "Grep",
            json!({"pattern": format!("TODO-{i}"), "path": "src"}),
        ));
        m.push(user_result(
            &id,
            json!(lines(1_500, &format!("todo{i}"))),
            false,
        ));
    }
    Corpus {
        name: "repeated_grep",
        profile: "8 identical Greps + 4 distinct (dedup territory)",
        note: "drops 7 superseded repeats; the distinct searches are kept",
        body: envelope(m),
    }
}

/// A coding session with re-reads whose content *changes* between reads (the
/// realistic shape) plus one old oversized build log.
fn coding() -> Corpus {
    let mut m = Vec::new();
    // Turn 1 (old): a failed Bash with a fat heredoc input → failed_input_purge.
    m.push(assistant_call(
        "Running the migration.",
        "t_fail",
        "Bash",
        json!({"command": format!("set -e\n{}", lines(3_000, "script"))}),
    ));
    m.push(user_result(
        "t_fail",
        json!("migrate: command not found (127)"),
        true,
    ));
    // Turn 2 (old): a 30 KB build log → bloat_cap.
    m.push(assistant_call(
        "Building.",
        "t_log",
        "Bash",
        json!({"command": "cargo build"}),
    ));
    m.push(user_result("t_log", json!(lines(30_000, "build")), false));
    // Turns 3..=8: re-reads of one file whose content CHANGES each time
    // (edits land between reads). dedup keys on input, so the earlier results
    // are still superseded — but they are realistically distinct, not clones.
    for i in 0..6 {
        let id = format!("t_read_{i}");
        m.push(assistant_call(
            "Re-reading src/lib.rs after the edit.",
            &id,
            "Read",
            json!({"file_path": "src/lib.rs"}),
        ));
        m.push(user_result(
            &id,
            json!(lines(1_800, &format!("librs_v{i}"))),
            false,
        ));
    }
    // Recent distinct edits → untouched.
    for i in 0..5 {
        let id = format!("t_edit_{i}");
        m.push(assistant_call(
            "Applying an edit.",
            &id,
            "Edit",
            json!({"file_path": "src/lib.rs", "old": format!("x{i}"), "new": format!("y{i}")}),
        ));
        m.push(user_result(&id, json!(format!("edit {i} applied")), false));
    }
    Corpus {
        name: "coding",
        profile: "re-reads (changing content) + one old build log",
        note: "dedup on the superseded reads, bloat_cap on the old log",
        body: envelope(m),
    }
}

/// The flagship realistic session: every strategy takes a meaningful slice.
fn mixed_realistic() -> Corpus {
    let mut m = Vec::new();
    // Old failed command (big input).
    m.push(assistant_call(
        "Trying the codegen step.",
        "t_fail",
        "Bash",
        json!({"command": format!("codegen <<EOF\n{}\nEOF", lines(2_500, "gen"))}),
    ));
    m.push(user_result(
        "t_fail",
        json!("error: template not found"),
        true,
    ));
    // Old big build log.
    m.push(assistant_call(
        "Building.",
        "t_build",
        "Bash",
        json!({"command": "cargo build"}),
    ));
    m.push(user_result("t_build", json!(lines(28_000, "build")), false));
    // Two repeated config reads (identical input) → dedup.
    for i in 0..2 {
        let id = format!("t_cfg_{i}");
        m.push(assistant_call(
            "Reading the config.",
            &id,
            "Read",
            json!({"file_path": "Cargo.toml"}),
        ));
        m.push(user_result(&id, json!(lines(1_600, "cargotoml")), false));
    }
    // One unique read (control).
    m.push(assistant_call(
        "Reading main.",
        "t_main",
        "Read",
        json!({"file_path": "src/main.rs"}),
    ));
    m.push(user_result("t_main", json!(lines(2_000, "mainrs")), false));
    // Five UI screenshots (→ image_strip keeps 3; sliding_window stubs old).
    for i in 0..5 {
        let id = format!("t_shot_{i}");
        m.push(assistant_call(
            "Snapshotting the UI.",
            &id,
            "mcp__playwright__browser_take_screenshot",
            json!({"name": format!("ui-{i}.png")}),
        ));
        m.push(user_result(
            &id,
            json!(b64_blob(60_000, &format!("png{i}"))),
            false,
        ));
    }
    // Old test log.
    m.push(assistant_call(
        "Testing.",
        "t_test",
        "Bash",
        json!({"command": "cargo test"}),
    ));
    m.push(user_result("t_test", json!(lines(22_000, "test")), false));
    // Recent distinct edits → untouched.
    for i in 0..6 {
        let id = format!("t_edit_{i}");
        m.push(assistant_call(
            "Editing.",
            &id,
            "Edit",
            json!({"file_path": format!("src/f{i}.rs"), "old": "a", "new": "b"}),
        ));
        m.push(user_result(&id, json!(format!("ok {i}")), false));
    }
    Corpus {
        name: "mixed_realistic",
        profile: "a believable feature session: reads, fails, logs, screenshots, edits",
        note: "several strategies each take a slice — the realistic composite",
        body: envelope(m),
    }
}

/// A non-browser MCP server emitting big text results. The default install
/// denylists only playwright, so only bloat_cap fires here — until you tune
/// the denylist (see the tuned config).
fn mcp_non_playwright() -> Corpus {
    let mut m = Vec::new();
    for i in 0..10 {
        let id = format!("t_q_{i}");
        m.push(assistant_call(
            "Querying the database.",
            &id,
            "mcp__postgres__query",
            json!({"sql": format!("select * from events where day = {i}")}),
        ));
        let result = if i % 2 == 0 {
            json!(lines(25_000, &format!("rows{i}")))
        } else {
            json!(format!("{} rows", i * 7))
        };
        m.push(user_result(&id, result, false));
    }
    Corpus {
        name: "mcp_non_playwright",
        profile: "10 Postgres-MCP queries, half with ~25 KB result tables",
        note: "default: bloat_cap only (denylist is playwright-only); more if tuned",
        body: envelope(m),
    }
}

/// Long session with DIVERSE large outputs (not clones): only the old oversized
/// ones get capped; the unique recent ones survive.
fn long_running() -> Corpus {
    let mut m = Vec::new();
    for i in 0..22 {
        let id = format!("t_run_{i}");
        m.push(assistant_call(
            "Running the next step.",
            &id,
            "Bash",
            json!({"command": format!("./step --n {i}")}),
        ));
        // Every 4th step dumps a big UNIQUE log; the rest are small + unique.
        let result = if i % 4 == 0 {
            json!(lines(20_000, &format!("biglog{i}")))
        } else {
            json!(lines(400, &format!("step{i}")))
        };
        m.push(user_result(&id, result, false));
    }
    Corpus {
        name: "long_running",
        profile: "22 steps, every 4th a distinct ~20 KB log",
        note: "diverse outputs; only aged oversized logs are capped",
        body: envelope(m),
    }
}

/// Browser session: navigation + assertions interleaved with screenshots
/// (~60 KB each). image_strip + sliding_window dominate; cache churns hard.
fn browser_heavy() -> Corpus {
    let mut m = Vec::new();
    for i in 0..7 {
        let nav = format!("t_nav_{i}");
        let shot = format!("t_shot_{i}");
        // A navigation/assert Bash, then a screenshot — realistic interleave.
        m.push(assistant_parallel(
            "Navigate, then snapshot.",
            vec![
                tool_use(
                    &nav,
                    "Bash",
                    json!({"command": format!("playwright goto /page/{i}")}),
                ),
                tool_use(
                    &shot,
                    "mcp__playwright__browser_take_screenshot",
                    json!({"name": format!("p{i}.png")}),
                ),
            ],
        ));
        m.push(user_results(vec![
            result_block(&nav, json!(format!("navigated to /page/{i}")), false),
            result_block(&shot, json!(b64_blob(60_000, &format!("shot{i}"))), false),
        ]));
    }
    Corpus {
        name: "browser_heavy",
        profile: "7 navigate+screenshot turns (~60 KB base64 each)",
        note: "biggest byte win, but the heaviest cache churn — often a wash on cost",
        body: envelope(m),
    }
}

/// One enormous old result (a full-repo dump). Extreme bloat_cap + a big-body
/// overhead datapoint.
fn giant_paste() -> Corpus {
    let mut m = Vec::new();
    m.push(assistant_call(
        "Dumping the whole tree.",
        "t_dump",
        "Bash",
        json!({"command": "find . -type f | xargs cat"}),
    ));
    m.push(user_result(
        "t_dump",
        json!(lines(500_000, "treedump")),
        false,
    ));
    for i in 0..6 {
        let id = format!("t_after_{i}");
        m.push(assistant_call(
            "Following up.",
            &id,
            "Bash",
            json!({"command": format!("ls {i}")}),
        ));
        m.push(user_result(&id, json!(format!("entry {i}")), false));
    }
    Corpus {
        name: "giant_paste",
        profile: "one ~500 KB old result, then small turns",
        note: "extreme single-result bloat_cap; the big-body overhead probe",
        body: envelope(m),
    }
}

/// A long *resumed* coding session (close your laptop, `claude --resume`
/// tomorrow). ~50 Bash/Read round-trips — the length where pruning pays off on
/// cost, not just size. The relatable face of the §6 cost crossover.
fn resumed_session() -> Corpus {
    Corpus {
        name: "resumed_session",
        profile: "a long resumed session, ~50 Bash/Read turns",
        note: "the length sweet spot: big size savings AND a real cost win",
        body: bash_session(50),
    }
}

/// Several OLD *successful* Bash calls carrying bulky `stdin` input, aged past
/// keep_recent — exercises `stale_input_cap` (ON in `default`: reduces old
/// successful inputs; off in `gentle`). Fills the coverage gap where
/// stale_input_cap showed 0 marginal attribution on every other corpus. The
/// bulk is in the tool_use INPUT (only stale_input_cap reduces it), not the
/// small results (so bloat_cap stays out of it).
fn stale_input_heavy() -> Corpus {
    let mut m = vec![user_text("load several datasets into the dev db")];
    for i in 0..4 {
        let id = format!("load{i}");
        m.push(assistant_call(
            &format!("loading dataset {i}"),
            &id,
            "Bash",
            json!({
                "command": format!("psql < dump{i}.sql"),
                "stdin": lines(3000, &format!("rows{i}")),
            }),
        ));
        m.push(user_result(
            &id,
            json!(format!("loaded dataset {i}: 3000 rows")),
            false,
        ));
    }
    // Age the bulky-input calls past keep_recent (default stale_input_cap = 2).
    for i in 0..4 {
        let id = format!("s{i}");
        m.push(assistant_call(
            &format!("step {i}"),
            &id,
            "Bash",
            json!({"command": format!("echo {i}")}),
        ));
        m.push(user_result(&id, json!(format!("out {i}")), false));
    }
    Corpus {
        name: "stale_input_heavy",
        profile: "default",
        note: "old successful calls with bulky inputs — stale_input_cap territory",
        body: envelope(m),
    }
}

/// OLD assistant turns each carrying a bulky `thinking` block (+ text + tool_use so
/// the turn is never thinking-only), aged past keep_recent — exercises
/// `thinking_strip` (ON in BOTH profiles since 2026-06-05 — default keep_recent=4,
/// gentle keep_recent=8). Fills the coverage gap where thinking_strip showed 0
/// marginal attribution on every other corpus.
fn thinking_heavy() -> Corpus {
    let mut m = vec![user_text("plan and implement the refactor")];
    for i in 0..5 {
        let id = format!("th{i}");
        m.push(json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": lines(2000, &format!("reason{i}")), "signature": format!("sig{i}")},
            {"type": "text", "text": format!("step {i}")},
            {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": format!("echo think{i}")}},
        ]}));
        m.push(user_result(&id, json!(format!("out {i}")), false));
    }
    // Age the reasoning turns past thinking_strip keep_recent (default = 4). Unique
    // commands so cross_turn_dedup stays out of it — this corpus isolates thinking_strip.
    for i in 0..4 {
        let id = format!("t{i}");
        m.push(assistant_call(
            &format!("more {i}"),
            &id,
            "Bash",
            json!({"command": format!("echo age{i}")}),
        ));
        m.push(user_result(&id, json!(format!("done {i}")), false));
    }
    Corpus {
        name: "thinking_heavy",
        profile: "default",
        note: "old reasoning-heavy turns — thinking_strip territory",
        body: envelope(m),
    }
}

/// All corpora, ordered low-savings → high-savings so the report can't be read
/// as "everything wins".
/// A multi-agent orchestration session: many `Task`/`Agent` subagent calls, each
/// returning a DENSE ~10 KB findings/blocker list. Before #124 these results were
/// exempt from bloat_cap at EVERY age, so a long multi-agent session carried the full
/// untrimmed mass forever. Now they are age-gated on `subagent_keep_recent_turns` (8):
/// the recent findings stay verbatim (the parent agent is still consuming them), while
/// findings older than the window are head+tail-salvaged (top findings + conclusion
/// kept). 12 subagent turns → the oldest ~3-4 age past the window and get trimmed.
fn subagent_heavy() -> Corpus {
    let mut m = vec![user_text("orchestrate a multi-agent audit of the codebase")];
    for i in 0..12 {
        let id = format!("t_sub_{i}");
        // Alternate the two subagent-launch names (Task is the original, Agent the
        // drifted name) so the corpus exercises both.
        let name = if i % 2 == 0 { "Task" } else { "Agent" };
        m.push(assistant_call(
            "Delegating the next slice to a subagent.",
            &id,
            name,
            json!({"description": format!("audit slice {i}"),
                   "prompt": format!("Review module {i} and report findings.")}),
        ));
        // A dense findings list: well over bloat_cap's 4 KB default threshold, with a
        // realistic head (summary), bulky middle (per-file findings), and tail
        // (conclusion) — the shape head+tail salvage is designed to preserve.
        m.push(user_result(
            &id,
            json!(lines(10_000, &format!("finding{i}"))),
            false,
        ));
    }
    Corpus {
        name: "subagent_heavy",
        profile: "12 subagent (Task/Agent) results, each ~10 KB of dense findings",
        note: "#124: old subagent findings (past the 8-turn window) get head+tail-salvaged",
        body: envelope(m),
    }
}

pub fn corpora() -> Vec<Corpus> {
    vec![
        pure_chat_floor(),
        exempt_heavy(),
        subagent_heavy(),
        read_heavy(),
        unique_bash_spam(),
        at_the_boundary(),
        repeated_grep(),
        coding(),
        mixed_realistic(),
        mcp_non_playwright(),
        long_running(),
        resumed_session(),
        browser_heavy(),
        giant_paste(),
        stale_input_heavy(),
        thinking_heavy(),
    ]
}

// ===========================================================================
// Configs
// ===========================================================================

/// Every strategy `strategies::run` can apply, in run order. Must stay in sync
/// with `run()` — `marginal_attribution` iterates this, so a missing entry
/// silently drops that strategy's byte attribution (the bug that hid
/// stale_input_cap / stale_reads / thinking_strip — all ON in `default`).
pub const STRATEGY_NAMES: &[&str] = &[
    "failed_input_purge",
    "stale_input_cap",
    "cross_turn_dedup",
    "stale_reads",
    "simhash_dedup",
    "bloat_cap",
    "sliding_window",
    "image_strip",
    "thinking_strip",
];

/// Enable or disable one strategy by name.
pub fn set_enabled(c: &mut Config, name: &str, on: bool) {
    match name {
        "failed_input_purge" => c.strategies.failed_input_purge.enabled = on,
        "stale_input_cap" => c.strategies.stale_input_cap.enabled = on,
        "cross_turn_dedup" => c.strategies.cross_turn_dedup.enabled = on,
        "stale_reads" => c.strategies.stale_reads.enabled = on,
        "simhash_dedup" => c.strategies.simhash_dedup.enabled = on,
        "bloat_cap" => c.strategies.bloat_cap.enabled = on,
        "sliding_window" => c.strategies.sliding_window.enabled = on,
        "image_strip" => c.strategies.image_strip.enabled = on,
        "thinking_strip" => c.strategies.thinking_strip.enabled = on,
        other => panic!("unknown strategy {other}"),
    }
}

// The benchmark measures exactly the shipping profiles — sourced from the one
// place they're defined (`trimwire::config::profile_baseline`), so the report
// can never drift from what users actually run.

/// The shipped default config = the aggressive `default` profile.
pub fn default_install_config() -> Config {
    trimwire::config::profile_baseline("default")
}

/// The conservative `gentle` profile — the floor that brackets the default.
pub fn tuned_config() -> Config {
    trimwire::config::profile_baseline("gentle")
}

// ===========================================================================
// Metrics
// ===========================================================================

/// Compact serialized byte length of a `messages[]` slice.
pub fn serialized_len(m: &[Value]) -> usize {
    serde_json::to_vec(m).map(|v| v.len()).unwrap_or(0)
}

/// Estimated tokens for a byte count (~4 bytes/token; an estimate — see the
/// caveats in the report. Least reliable on base64 image bytes).
pub fn est_tokens(bytes: usize) -> u64 {
    (bytes / 4) as u64
}

/// Signed reduction as a percentage of `total`.
pub fn pct(saved: i64, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        (saved as f64 / total as f64) * 100.0
    }
}

/// Length of the common leading byte run of two slices.
pub fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Count of leading messages that serialize byte-identically — a block-granular
/// proxy for how much of the prompt-cache prefix survives (closer to Anthropic's
/// breakpoint-based invalidation than a raw byte prefix).
pub fn leading_common_msgs(a: &[Value], b: &[Value]) -> usize {
    a.iter()
        .zip(b)
        .take_while(|(x, y)| serde_json::to_vec(x).ok() == serde_json::to_vec(y).ok())
        .count()
}

/// Outcome of running one config over one corpus.
pub struct Run {
    pub in_bytes: usize,
    pub out_bytes: usize,
    pub orphan_free: bool,
    pub never_grew: bool,
    pub system_preserved: bool,
    pub unchanged: bool,
    pub per_strategy_stubbed: Vec<(&'static str, usize)>,
}

impl Run {
    pub fn saved(&self) -> i64 {
        self.in_bytes as i64 - self.out_bytes as i64
    }
    pub fn reduction_pct(&self) -> f64 {
        pct(self.saved(), self.in_bytes)
    }
}

/// Prune a clone of `messages` with `cfg`; returns the pruned array.
pub fn prune(messages: &[Value], cfg: &Config) -> Vec<Value> {
    let mut pruned = messages.to_vec();
    strategies::run(&mut pruned, cfg).expect("strategies must not orphan");
    pruned
}

/// Prune just `messages[..=end]` (a growing-session snapshot).
pub fn prune_snapshot(messages: &[Value], end: usize, cfg: &Config) -> Vec<Value> {
    prune(&messages[..=end], cfg)
}

/// Run `cfg` over `body` and measure every invariant + the savings.
pub fn measure(body: &Value, cfg: &Config) -> Run {
    let messages = body["messages"].as_array().expect("messages[]");
    let baseline = serialized_len(messages);
    let pruned = prune(messages, cfg);
    let out = serialized_len(&pruned);

    let per_strategy_stubbed = {
        let mut snap = messages.to_vec();
        strategies::run(&mut snap, cfg)
            .expect("strategies must not orphan")
            .into_iter()
            .map(|(n, s)| (n, s.stubbed))
            .collect()
    };

    let body_bytes = serde_json::to_vec(body).expect("serialize body");
    let (unchanged, system_preserved) = match strategies::apply_to_body(&body_bytes, cfg) {
        BodyOutcome::Unchanged => (true, true),
        BodyOutcome::Mutated { bytes, .. } => {
            let after: Value = serde_json::from_slice(&bytes).expect("parse mutated");
            (false, after.get("system") == body.get("system"))
        }
    };

    Run {
        in_bytes: baseline,
        out_bytes: out,
        orphan_free: PairingIndex::build(&pruned).validate().is_ok(),
        never_grew: out <= baseline,
        system_preserved,
        unchanged,
        per_strategy_stubbed,
    }
}

/// **Leave-one-out marginal attribution**: bytes each strategy removes *on top
/// of the others*. Unlike the "each alone" ablation, these compose toward the
/// total (with a small interaction residual) instead of double-counting overlap.
pub fn marginal_attribution(body: &Value, full: &Config) -> Vec<(&'static str, i64)> {
    let messages = body["messages"].as_array().expect("messages[]");
    let all_saved = measure(body, full).saved();
    STRATEGY_NAMES
        .iter()
        .filter(|n| strategy_enabled(full, n))
        .map(|name| {
            let mut without = full.clone();
            set_enabled(&mut without, name, false);
            let saved_without =
                serialized_len(messages) as i64 - serialized_len(&prune(messages, &without)) as i64;
            (*name, all_saved - saved_without)
        })
        .collect()
}

fn strategy_enabled(c: &Config, name: &str) -> bool {
    match name {
        "failed_input_purge" => c.strategies.failed_input_purge.enabled,
        "stale_input_cap" => c.strategies.stale_input_cap.enabled,
        "cross_turn_dedup" => c.strategies.cross_turn_dedup.enabled,
        "stale_reads" => c.strategies.stale_reads.enabled,
        "simhash_dedup" => c.strategies.simhash_dedup.enabled,
        "bloat_cap" => c.strategies.bloat_cap.enabled,
        "sliding_window" => c.strategies.sliding_window.enabled,
        "image_strip" => c.strategies.image_strip.enabled,
        "thinking_strip" => c.strategies.thinking_strip.enabled,
        other => panic!("unknown strategy {other} (strategy_enabled out of sync with run)"),
    }
}

/// Turn-to-turn prompt-cache stability: replay the corpus as the growing request
/// sequence, prune each snapshot, and average the share of the earlier pruned
/// request that survives (block-granular) as a prefix of the next. `None` if
/// there are fewer than two turns. The unpruned baseline is ~100%.
pub fn cache_stability(messages: &[Value], cfg: &Config) -> Option<f64> {
    let bounds = turn_bounds(messages);
    if bounds.len() < 2 {
        return None;
    }
    let mut ratios = Vec::new();
    for w in bounds.windows(2) {
        let earlier = prune_snapshot(messages, w[0], cfg);
        let later = prune_snapshot(messages, w[1], cfg);
        // Block-granular (whole leading messages), matching `session_cost` and
        // Anthropic's breakpoint cache far better than a raw byte prefix —
        // pruning rewrites a message *in place*, so one changed message
        // invalidates everything after it. Append-only growth keeps every
        // earlier message intact → a true 100% baseline (no trailing-bracket
        // artifact, since we serialize the same leading slice each side).
        let common = leading_common_msgs(&earlier, &later);
        let total = serialized_len(&earlier);
        if total > 0 {
            ratios.push(serialized_len(&earlier[..common]) as f64 / total as f64);
        }
    }
    (!ratios.is_empty()).then(|| 100.0 * ratios.iter().sum::<f64>() / ratios.len() as f64)
}

/// Turn boundaries = `user` messages (each closes a round-trip).
pub fn turn_bounds(messages: &[Value]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|(i, _)| i)
        .collect()
}

/// Anthropic-style input pricing. Output tokens are identical with/without
/// pruning (only `messages[]` input changes), so the model bills input only.
#[derive(Clone, Copy)]
pub struct Pricing {
    /// USD per million input tokens (Sonnet-class default).
    pub input_per_mtok: f64,
    /// Multiplier for a prompt-cache hit (read).
    pub cache_read_mult: f64,
}

impl Default for Pricing {
    fn default() -> Self {
        Self {
            input_per_mtok: 3.0,
            cache_read_mult: 0.10,
        }
    }
}

/// Cost of a whole session's *input* under prompt caching. Each turn re-sends
/// the conversation so far; the prefix shared with the previous request is
/// billed at the cache-read rate, the rest at full rate. (The 1.25× cache-write
/// surcharge is omitted as second-order.)
pub struct SessionCost {
    pub dollars: f64,
    pub cached_tokens: u64,
    pub fresh_tokens: u64,
}

/// Constant system+tools prefix every `/v1/messages` turn carries but trimwire
/// never touches (the Claude Code system prompt + tool schemas). It sits *before*
/// `messages[]`, so it is identical in the pruned and unpruned requests and
/// survives every prune — pure denominator, never something pruning "saves". A
/// real CC system prompt + tool schemas serialize to ~12 KB ≈ 3000 tokens
/// (4 B/tok). Billing it (fresh on turn 1, cache-read every later turn) keeps the
/// reported cost %Δ honest: without it the deltas sit on a messages-only
/// denominator and overstate the *magnitude* — the constant cancels in the ratio,
/// so the *sign* of every delta is unchanged.
pub const PREFIX_TOKENS: u64 = 3000;

pub fn session_cost(messages: &[Value], cfg: &Config, pr: Pricing) -> SessionCost {
    let bounds = turn_bounds(messages);
    let mut cost_units = 0.0f64; // base-input-token equivalents
    let mut cached_tokens = 0u64;
    let mut fresh_tokens = 0u64;
    let mut prev: Option<Vec<Value>> = None;

    for &end in &bounds {
        let snap = prune_snapshot(messages, end, cfg);
        let total_tok = est_tokens(serialized_len(&snap));
        let cached = match &prev {
            None => 0,
            Some(p) => {
                let common = leading_common_msgs(p, &snap);
                est_tokens(serialized_len(&snap[..common]))
            }
        };
        let fresh = total_tok.saturating_sub(cached);
        // The constant prefix: written (full rate) on the first turn, cache-read
        // on every later turn. Same value in every config → pure denominator.
        let prefix = if prev.is_none() {
            PREFIX_TOKENS as f64
        } else {
            PREFIX_TOKENS as f64 * pr.cache_read_mult
        };
        cost_units += cached as f64 * pr.cache_read_mult + fresh as f64 + prefix;
        cached_tokens += cached;
        fresh_tokens += fresh;
        prev = Some(snap);
    }

    SessionCost {
        dollars: cost_units / 1_000_000.0 * pr.input_per_mtok,
        cached_tokens,
        fresh_tokens,
    }
}

/// A realistic Bash/Read session of `turns` round-trips, for the cost-vs-length
/// study: mostly small unique outputs, every 5th a big log (→ bloat_cap once
/// aged), a repeated config read every 7th (→ dedup). Deterministic.
pub fn bash_session(turns: usize) -> Value {
    let mut m = Vec::with_capacity(turns * 2);
    for i in 0..turns {
        let id = format!("t_{i}");
        if i % 7 == 3 {
            // A repeated identical config read → dedup territory.
            m.push(assistant_call(
                "Re-reading the config.",
                &id,
                "Read",
                json!({"file_path": "Cargo.toml"}),
            ));
            m.push(user_result(&id, json!(lines(1_500, "cargotoml")), false));
        } else if i % 5 == 0 {
            // A big unique log → bloat_cap once it ages out.
            m.push(assistant_call(
                "Running a verbose step.",
                &id,
                "Bash",
                json!({"command": format!("./run --verbose --n {i}")}),
            ));
            m.push(user_result(
                &id,
                json!(lines(18_000, &format!("verbose{i}"))),
                false,
            ));
        } else {
            // A small unique step.
            m.push(assistant_call(
                "Running a step.",
                &id,
                "Bash",
                json!({"command": format!("./run --n {i}")}),
            ));
            m.push(user_result(
                &id,
                json!(lines(500, &format!("step{i}"))),
                false,
            ));
        }
    }
    envelope(m)
}

/// Savings-% at each turn snapshot — shows the aging warm-up (near-zero early,
/// rising as history grows past the recent window).
pub fn savings_curve(messages: &[Value], cfg: &Config) -> Vec<(usize, f64)> {
    let bounds = turn_bounds(messages);
    bounds
        .iter()
        .enumerate()
        .map(|(turn, &end)| {
            let base = serialized_len(&messages[..=end]);
            let pruned = serialized_len(&prune_snapshot(messages, end, cfg));
            (turn + 1, pct(base as i64 - pruned as i64, base))
        })
        .collect()
}

// ===========================================================================
// Context quality (rot resistance) — the actual point of pruning
// ===========================================================================

/// First index of the "recent" window: everything before it is old backlog.
/// Mirrors the strategies' `keep_recent_turns` boundary (which is `pub(crate)`
/// in the crate, so we recompute it here).
fn recent_start(messages: &[Value], keep_recent_turns: usize) -> usize {
    let mut seen = 0usize;
    for (i, m) in messages.iter().enumerate().rev() {
        if m.get("role").and_then(Value::as_str) == Some("assistant") {
            seen += 1;
            if seen > keep_recent_turns {
                return i + 1;
            }
        }
    }
    0
}

/// **Focus ratio**: the fraction of the request that is the *recent* window the
/// model is actually working on, versus the whole backlog. A low ratio is
/// context rot — the current task drowned in stale history. Pruning keeps it
/// high. Defined purely by recency (not by what trimwire deletes), so it's an
/// independent measure, not a restatement of the savings.
pub fn focus_ratio(messages: &[Value], keep_recent_turns: usize) -> f64 {
    let total = serialized_len(messages);
    if total == 0 {
        return 1.0;
    }
    let start = recent_start(messages, keep_recent_turns);
    serialized_len(&messages[start..]) as f64 / total as f64
}

/// **Redundancy ratio**: the fraction of bytes that are *repeated* `tool_result`
/// content (every occurrence after the first). Objectively dead weight — the
/// model re-reads identical output N times. Computed by exact-content match,
/// independent of trimwire's `(name,input)` dedup key.
pub fn redundancy_ratio(messages: &[Value]) -> f64 {
    let total = serialized_len(messages);
    if total == 0 {
        return 0.0;
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut redundant = 0usize;
    for m in messages {
        let Some(content) = m.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(c) = block.get("content") else {
                continue;
            };
            // trimwire's own elision markers are model-facing breadcrumbs, not
            // dead weight — don't count them as redundancy (they'd otherwise
            // inflate the metric just because several share a prefix).
            if c.as_str().is_some_and(|s| s.starts_with("[trimwire:")) {
                continue;
            }
            let key = serde_json::to_string(c).unwrap_or_default();
            let count = seen.entry(key.clone()).or_insert(0);
            if *count >= 1 {
                redundant += key.len();
            }
            *count += 1;
        }
    }
    redundant as f64 / total as f64
}

/// Context-quality snapshot for one config: how focused (recent-dominated) and
/// how redundancy-free the request is. Higher focus + lower redundancy = less
/// rot. The keep-recent window is fixed at 4 (the shipped default) so unpruned
/// and pruned are compared on the same yardstick.
pub struct Quality {
    pub focus: f64,
    pub redundancy: f64,
}

pub fn context_quality(messages: &[Value]) -> Quality {
    Quality {
        focus: focus_ratio(messages, 4),
        redundancy: redundancy_ratio(messages),
    }
}

/// Rot accumulation over a growing session: focus ratio at each turn, unpruned
/// vs pruned. Unpruned focus decays as the backlog buries the recent ask (rot);
/// pruned focus stays high (clean). Returns `(turn, unpruned_focus, pruned_focus)`.
pub fn focus_over_time(messages: &[Value], cfg: &Config) -> Vec<(usize, f64, f64)> {
    turn_bounds(messages)
        .iter()
        .enumerate()
        .map(|(turn, &end)| {
            let raw = &messages[..=end];
            let pruned = prune_snapshot(messages, end, cfg);
            (turn + 1, focus_ratio(raw, 4), focus_ratio(&pruned, 4))
        })
        .collect()
}

// ===========================================================================
// Stable-prefix re-pruning measurement — exercises the PRODUCTION `reprune`
// (trimwire::reprune::stable_apply_to_body), so the benchmark and the shipped
// gateway share one implementation (no drift).
// ===========================================================================

/// Serialize a minimal request body for the first `end+1` messages.
fn snapshot_body(messages: &[Value], end: usize) -> Vec<u8> {
    serde_json::to_vec(&json!({ "messages": &messages[..=end] })).unwrap_or_default()
}

/// Pruned messages of one snapshot via the production stateful path.
fn stable_snapshot(
    messages: &[Value],
    end: usize,
    cfg: &Config,
    state: &mut trimwire::reprune::PruneState,
    threshold: usize,
) -> Vec<Value> {
    let body = snapshot_body(messages, end);
    let bytes = match trimwire::reprune::stable_apply_to_body(&body, cfg, state, threshold) {
        BodyOutcome::Unchanged => body,
        BodyOutcome::Mutated { bytes, .. } => bytes,
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v.get("messages").and_then(|m| m.as_array().cloned()))
        .unwrap_or_default()
}

/// Spike metrics: stateless vs stable-prefix, over the growing session.
pub struct Spike {
    pub stateless_stability: f64,
    pub stable_stability: f64,
    pub stateless_end_reduction: f64,
    pub stable_end_reduction: f64,
    pub stateless_cost: f64,
    pub stable_cost: f64,
}

/// Replay the growing session both ways (stateless `prune` vs the production
/// `reprune::stable_apply_to_body`) and measure cache stability, end-state
/// savings, and total input cost. `threshold` is the re-prune cadence (messages).
pub fn spike_compare(messages: &[Value], cfg: &Config, threshold: usize) -> Option<Spike> {
    let bounds = turn_bounds(messages);
    if bounds.len() < 2 {
        return None;
    }
    let pr = Pricing::default();

    let stateless: Vec<Vec<Value>> = bounds
        .iter()
        .map(|&e| prune_snapshot(messages, e, cfg))
        .collect();

    let mut state = trimwire::reprune::PruneState::default();
    let stable: Vec<Vec<Value>> = bounds
        .iter()
        .map(|&e| stable_snapshot(messages, e, cfg, &mut state, threshold))
        .collect();

    let stability = |snaps: &[Vec<Value>]| -> f64 {
        let mut ratios = Vec::new();
        for w in snaps.windows(2) {
            let common = leading_common_msgs(&w[0], &w[1]);
            let total = serialized_len(&w[0]);
            if total > 0 {
                ratios.push(serialized_len(&w[0][..common]) as f64 / total as f64);
            }
        }
        if ratios.is_empty() {
            100.0
        } else {
            100.0 * ratios.iter().sum::<f64>() / ratios.len() as f64
        }
    };
    let end_reduction = |snaps: &[Vec<Value>]| -> f64 {
        let last = snaps.last().unwrap();
        let base = serialized_len(&messages[..=*bounds.last().unwrap()]);
        pct(base as i64 - serialized_len(last) as i64, base)
    };
    let cost = |snaps: &[Vec<Value>]| -> f64 {
        let mut units = 0.0f64;
        let mut prev: Option<&Vec<Value>> = None;
        for snap in snaps {
            let total = est_tokens(serialized_len(snap));
            let cached = match prev {
                None => 0,
                Some(p) => est_tokens(serialized_len(&snap[..leading_common_msgs(p, snap)])),
            };
            // Same constant system+tools prefix as `session_cost` (see PREFIX_TOKENS).
            let prefix = if prev.is_none() {
                PREFIX_TOKENS as f64
            } else {
                PREFIX_TOKENS as f64 * pr.cache_read_mult
            };
            units +=
                cached as f64 * pr.cache_read_mult + total.saturating_sub(cached) as f64 + prefix;
            prev = Some(snap);
        }
        units / 1_000_000.0 * pr.input_per_mtok
    };

    Some(Spike {
        stateless_stability: stability(&stateless),
        stable_stability: stability(&stable),
        stateless_end_reduction: end_reduction(&stateless),
        stable_end_reduction: end_reduction(&stable),
        stateless_cost: cost(&stateless),
        stable_cost: cost(&stable),
    })
}
