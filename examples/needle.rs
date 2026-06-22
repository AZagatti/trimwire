//! Needle-survival probe (harm signal): plant known facts in OLD tool results,
//! prune under each profile, and report which facts survived in the pruned
//! `messages[]`. A surviving needle = the model can still see that fact; a
//! dropped needle = pruning removed it (potential "dumber agent").
//!
//! Columns: the two shipped profiles — `default` (aggressive: all seven
//! cache-safe strategies incl. stale_input_cap + stale_reads) and `gentle`
//! (conservative subset; stale_input_cap/stale_reads off). The contrast shows
//! which needles the aggressive levers drop that the gentle profile keeps.
//!
//! Deterministic, offline. Run: cargo run --release --example needle

use serde_json::{Value, json};
use trimwire::config::{Config, profile_baseline};
use trimwire::strategies;

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}
fn result(id: &str, content: String) -> Value {
    json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": content}]})
}
fn assistant(id: &str, name: &str, input: Value) -> Value {
    json!({"role": "assistant", "content": [
        {"type": "text", "text": "working"},
        tool_use(id, name, input),
    ]})
}
fn filler(n: usize, tag: &str) -> String {
    let mut s = String::new();
    let mut i = 0;
    while s.len() < n {
        s.push_str(&format!(
            "[{tag}] log line {i}: doing routine work and flushing buffers\n"
        ));
        i += 1;
    }
    s
}

/// Build a session with planted needles in OLD results, then fresh turns so the
/// planted ones age out of every keep-recent window.
fn session() -> Vec<Value> {
    let mut m = Vec::new();
    m.push(json!({"role": "user", "content": "start the task"}));

    // OLD reference-data result from an MCP tool (high denylists mcp__* → drops it).
    m.push(assistant(
        "t_db",
        "mcp__postgres__query",
        json!({"sql": "select port from cfg"}),
    ));
    m.push(result(
        "t_db",
        "query result: the service NEEDLE_DBREF runs on port 8080\n".to_string(),
    ));

    // OLD oversized Bash result: needle at head / middle / tail.
    let big = format!(
        "NEEDLE_HEAD build started\n{}\nNEEDLE_MIDDLE the critical constant is 42\n{}\nNEEDLE_TAIL build finished with 1 error\n",
        filler(10_000, "a"),
        filler(10_000, "b"),
    );
    m.push(assistant(
        "t_big",
        "Bash",
        json!({"command": "cargo build --verbose"}),
    ));
    m.push(result("t_big", big));

    // OLD FAILED call: the input (which command) is what failed_input_purge blanks.
    m.push(assistant(
        "t_fail",
        "Bash",
        json!({"command": "NEEDLE_FAILCMD deploy --prod"}),
    ));
    m.push(json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "t_fail", "content": "error: permission denied", "is_error": true}
    ]}));

    // OLD live Read (path never touched again) — must survive (stale_reads keeps
    // the last/only op on a path). Read uses `path` (extract_path's convention).
    m.push(assistant("r_live", "Read", json!({"path": "/src/live.rs"})));
    m.push(result(
        "r_live",
        "fn live() { let port = 8080; } // NEEDLE_READ_LIVE\n".to_string(),
    ));

    // OLD Read later superseded by a Write of the SAME path — stale_reads elides
    // it in `default`; `gentle` (stale_reads off) keeps it.
    m.push(assistant(
        "r_stale",
        "Read",
        json!({"path": "/src/edit.rs"}),
    ));
    m.push(result(
        "r_stale",
        "// NEEDLE_READ_STALE old contents re-read after editing; padded past the marker\n"
            .to_string(),
    ));

    // OLD successful Write: file_path (NEEDLE_PATH) AND bulky new_string
    // (NEEDLE_WRITE_BODY) must BOTH survive — authored file content is EXEMPT from
    // stale_input_cap (eliding it corrupted real sessions; the model reproduced the
    // marker as file content). This demonstrates the corruption fix.
    m.push(assistant(
        "w_bulk",
        "Write",
        json!({
            "file_path": "/src/gen_NEEDLE_PATH.rs",
            "new_string": format!("// NEEDLE_WRITE_BODY\n{}", "x".repeat(2000)),
        }),
    ));
    m.push(result("w_bulk", "wrote successfully\n".to_string()));

    // The Write that supersedes /src/edit.rs (makes r_stale stale).
    m.push(assistant(
        "w_edit",
        "Write",
        json!({"file_path": "/src/edit.rs", "new_string": "fn edited() {}"}),
    ));
    m.push(result("w_edit", "wrote /src/edit.rs\n".to_string()));

    // Several RECENT turns so the above are old (past keep_recent for all profiles).
    for i in 0..6 {
        let id = format!("t_r{i}");
        m.push(assistant(
            &id,
            "Bash",
            json!({"command": format!("ls {i}")}),
        ));
        m.push(result(&id, format!("entry {i}\n")));
    }

    // Most-recent result (must always survive).
    m.push(assistant("t_now", "Bash", json!({"command": "echo done"})));
    m.push(result(
        "t_now",
        "NEEDLE_RECENT current step output\n".to_string(),
    ));
    m
}

fn survives(needle: &str, bytes: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn main() {
    let needles = [
        ("NEEDLE_RECENT", "recent result (must always survive)"),
        ("NEEDLE_HEAD", "head of an old oversized Bash result"),
        ("NEEDLE_MIDDLE", "middle of an old oversized Bash result"),
        ("NEEDLE_TAIL", "tail of an old oversized Bash result"),
        ("NEEDLE_DBREF", "old MCP (reference-data) result"),
        ("NEEDLE_FAILCMD", "the command of an old FAILED call"),
        (
            "NEEDLE_READ_LIVE",
            "an old Read never superseded (must survive)",
        ),
        (
            "NEEDLE_READ_STALE",
            "an old Read superseded by a later Write (stale_reads)",
        ),
        (
            "NEEDLE_PATH",
            "file_path of an old successful Write (must survive)",
        ),
        (
            "NEEDLE_WRITE_BODY",
            "bulky new_string of an old Write (authored — must SURVIVE; exempt)",
        ),
    ];

    let cols: Vec<(&str, Config)> = vec![
        ("default", profile_baseline("default")),
        ("gentle", profile_baseline("gentle")),
    ];

    let pruned: Vec<(&str, Vec<u8>)> = cols
        .iter()
        .map(|(name, cfg)| {
            let mut msgs = session();
            strategies::run(&mut msgs, cfg).expect("no orphan");
            (*name, serde_json::to_vec(&msgs).unwrap())
        })
        .collect();

    println!("# Needle survival by profile (✓ survived / ✗ dropped)\n");
    print!("| needle | where |");
    for (name, _) in &pruned {
        print!(" {name} |");
    }
    println!();
    print!("|---|---|");
    for _ in 0..pruned.len() {
        print!("--|");
    }
    println!();
    for (needle, where_) in needles {
        print!("| `{needle}` | {where_} |");
        for (_, bytes) in &pruned {
            print!(
                " {} |",
                if survives(needle, bytes) {
                    "✓"
                } else {
                    "✗"
                }
            );
        }
        println!();
    }
    println!(
        "\n> `default` = aggressive (all eight cache-safe strategies, verb-class denylist, thinking_strip on).\n\
         > `gentle` = conservative (dedup + failed_input_purge + bloat_cap@32KB + thinking_strip@keep8; \
         stale_input_cap/stale_reads/sliding_window/image_strip off).\n\
         > NEEDLE_MIDDLE drop is bloat_cap (head+tail only) — only offload-to-artifact recovers it.\n\
         > NEEDLE_FAILCMD drop is failed_input_purge blanking input — shape-preserving purge keeps it.\n\
         > NEEDLE_READ_STALE drops in `default` (stale_reads), survives in `gentle`. NEEDLE_READ_LIVE / \
         NEEDLE_PATH / NEEDLE_WRITE_BODY (authored content — exempt) must survive everywhere."
    );
}
