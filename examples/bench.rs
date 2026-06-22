//! Offline-replay benchmark for trimwire's pruning strategies.
//!
//! Builds deterministic synthetic `/v1/messages` bodies spanning the honest
//! spectrum of session shapes (from no-op floors to image-heavy wins), feeds
//! them through the real strategy code, and reports savings, per-strategy
//! attribution, prompt-cache stability, a **dollar cost model**, a
//! savings-over-time curve, and statistical per-request overhead. No API key,
//! no network; every number except wall-clock timing is reproducible bit-for-bit.
//!
//! The corpus generators + metrics live in `benchmark/corpora.rs`, shared with
//! `tests/benchmark.rs` so the report and the regression guards never drift.
//!
//! ```sh
//! cargo run --release --example bench                       # the report
//! cargo run --release --example bench > benchmark/results/RESULTS.md
//! cargo run --release --example bench -- --dump benchmark/fixtures
//! ```
//!
//! ## Caveats baked into the report
//!
//! * **Token/cost figures are estimates** at ~4 bytes/token; least reliable for
//!   base64 image bytes (Anthropic bills images by resolution, not base64
//!   length). Base64 also tokenizes *denser* than 4 B/tok, so image-corpus token
//!   counts here are conservative under-counts. Byte columns are ground truth.
//! * **Cache stability** is a byte/block-prefix proxy for Anthropic's
//!   breakpoint-based cache invalidation — a conservative lower bound.
//! * **Synthetic, not captured traffic.** The corpora are reproducible and
//!   carry no PII; for your real numbers run `trimwire stats` on live traffic.

use std::time::{Duration, Instant};

#[path = "../benchmark/corpora.rs"]
#[allow(dead_code)]
mod corpora;

use corpora::{
    Pricing, cache_stability, context_quality, corpora as all_corpora, default_install_config,
    focus_over_time, marginal_attribution, measure, savings_curve, session_cost, tuned_config,
};

fn fmt_pct(p: f64) -> String {
    format!("{p:.1}%")
}

fn fmt_bytes(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1} KB", n as f64 / 1_000.0)
    } else {
        format!("{n} B")
    }
}

// ---- timing stats ---------------------------------------------------------

struct Stats {
    min: f64,
    median: f64,
    mean: f64,
    p99: f64,
    stddev: f64,
    round_spread: (f64, f64),
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((q * (sorted.len() - 1) as f64).round() as usize).min(sorted.len() - 1);
    sorted[idx]
}

/// Time `apply_to_body` over R rounds of N iterations (warm-up first). Returns
/// per-call millisecond stats pooled across rounds, plus the round-median spread.
fn time_apply(body: &[u8], cfg: &trimwire::config::Config, rounds: u32, iters: u32) -> Stats {
    // Warm-up loop (steady CPU + allocator arena).
    for _ in 0..100 {
        std::hint::black_box(trimwire::strategies::apply_to_body(
            std::hint::black_box(body),
            cfg,
        ));
    }
    let mut samples = Vec::with_capacity((rounds * iters) as usize);
    let mut round_medians = Vec::with_capacity(rounds as usize);
    for _ in 0..rounds {
        let mut round = Vec::with_capacity(iters as usize);
        for _ in 0..iters {
            let t = Instant::now();
            std::hint::black_box(trimwire::strategies::apply_to_body(
                std::hint::black_box(body),
                cfg,
            ));
            round.push(t.elapsed());
        }
        round.sort_unstable();
        round_medians.push(round[round.len() / 2].as_secs_f64() * 1_000.0);
        samples.extend(
            round
                .into_iter()
                .map(|d: Duration| d.as_secs_f64() * 1_000.0),
        );
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let var = samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / samples.len() as f64;
    round_medians.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Stats {
        min: samples[0],
        median: percentile(&samples, 0.50),
        mean,
        p99: percentile(&samples, 0.99),
        stddev: var.sqrt(),
        round_spread: (round_medians[0], round_medians[round_medians.len() - 1]),
    }
}

// ---- report ---------------------------------------------------------------

/// Full human report. `with_timing == false` (the `--ci-drift` mode) emits the
/// same deterministic sections 0–6b but SKIPS the expensive `## 7. Gateway
/// overhead` micro-timing loops (~150k `apply_to_body` calls, ~36s, host-
/// dependent and intentionally dropped by `scripts/bench-drift-check.sh`). The
/// §7 header is still printed so the drift script's normalization boundary holds.
fn report(with_timing: bool) {
    let def = default_install_config();
    let gentle = tuned_config();
    let corpora = all_corpora();

    println!("# trimwire benchmark — offline replay\n");
    println!(
        "> **TL;DR** — request size **0–99% lighter** by session shape (nothing when\n\
         > there's no redundancy); the point is **context-window headroom**, not money;\n\
         > cost is non-monotonic (wash-to-loss short, ≈ −55% at 256 turns — §6b computes −54.6%); **sub-2 ms**\n\
         > overhead; orphan-free + `system` untouched on every corpus + a 3,000-body fuzz.\n"
    );
    println!(
        "Deterministic synthetic `/v1/messages` bodies fed through the real strategy\n\
         code under the **shipped default config**, unless noted. Corpora are ordered\n\
         low-savings → high. Byte columns are exact and reproducible; cost/token figures\n\
         are estimates; timing is host-dependent. See `examples/bench.rs` for caveats.\n"
    );

    // Plain-English legend.
    println!("## Legend (plain English)\n");
    println!("| Term | What it means |");
    println!("|---|---|");
    println!("| `cross_turn_dedup` | drops earlier copies of a tool call you repeated |");
    println!("| `failed_input_purge` | clears the bulky input of an old *failed* command |");
    println!("| `bloat_cap` | shrinks a huge *old* tool result to its head + tail |");
    println!("| `sliding_window` | stubs old browser-automation tool calls |");
    println!("| `image_strip` | replaces old screenshots with a marker (keeps recent ones) |");
    println!(
        "| cache stability | how much of the previous request the prompt cache can still reuse |"
    );
    println!("| orphan-free | never deletes half of a command↔result pair |");
    println!("| no-op | trimwire forwarded the request byte-for-byte (nothing to prune) |\n");

    // ---- Section 0: context quality (the point of pruning) ----
    println!("## 0. Context quality — keeping the session focused and rot-free\n");
    println!(
        "> The real job: keep the session **clean** so the model isn't wading through\n\
         > stale backlog (\"context rot\"). **Focus** = the share of the request that is\n\
         > the recent window you're actually working on (higher = the current task\n\
         > isn't drowned in history). **Redundancy** = the share that is repeated tool\n\
         > output (lower = less dead weight). Both are defined by recency/repetition,\n\
         > not by what trimwire happens to delete.\n"
    );
    println!("| Corpus | Focus (unpruned → pruned) | Redundancy (unpruned → pruned) |");
    println!("|---|--:|--:|");
    for c in &corpora {
        let raw = context_quality(c.messages());
        let pruned = context_quality(&corpora::prune(c.messages(), &def));
        println!(
            "| `{}` | {} → **{}** | {} → **{}** |",
            c.name,
            fmt_pct(raw.focus * 100.0),
            fmt_pct(pruned.focus * 100.0),
            fmt_pct(raw.redundancy * 100.0),
            fmt_pct(pruned.redundancy * 100.0),
        );
    }
    println!(
        "\n> Pruning raises focus and drops redundancy on every shape with rot to remove,\n\
         > and leaves the clean floors untouched. Note the absolute level varies: on\n\
         > image-/log-dominated shapes (`coding` 2.6→12.6, `mixed_realistic` 0.3→6.9)\n\
         > focus stays single-digit even after pruning — those sessions are\n\
         > backlog-dominated regardless, and the real win there is the byte/redundancy\n\
         > cut, not focus. **Caveat:** focus and redundancy are *structural* proxies\n\
         > (byte share, repeated content); whether they translate into better model\n\
         > behaviour is plausible but unproven here — a model-in-the-loop eval (lost-in-\n\
         > the-middle / task-completion on long sessions) would be needed to show that,\n\
         > and we haven't run one. §6 shows focus over a growing session: unpruned\n\
         > decays as the backlog piles up; pruned holds higher.\n"
    );

    // ---- Section 1: savings (default) ----
    println!("## 1. Savings — shipped default config\n");
    println!("| Corpus | What it models | In | Out | Saved | Reduction | Result |");
    println!("|---|---|--:|--:|--:|--:|:-:|");
    let mut reductions = Vec::new();
    for c in &corpora {
        let r = measure(&c.body, &def);
        reductions.push(r.reduction_pct());
        assert!(
            r.orphan_free && r.never_grew && r.system_preserved,
            "{} invariant",
            c.name
        );
        println!(
            "| `{}` | {} | {} | {} | {} | {} | {} |",
            c.name,
            c.note,
            fmt_bytes(r.in_bytes),
            fmt_bytes(r.out_bytes),
            fmt_bytes(r.saved().max(0) as usize),
            fmt_pct(r.reduction_pct()),
            if r.unchanged { "no-op" } else { "pruned" },
        );
    }
    let lo = reductions.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = reductions.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    println!(
        "\n> Range across these shapes: **{} – {}**. There is no single \"trimwire\n\
         > saves X%\" — it depends entirely on what your session looks like, and on\n\
         > a session with nothing redundant it correctly does nothing. Every pruned\n\
         > body is orphan-free, never larger than the input, and leaves `system`\n\
         > untouched (asserted; the harness panics otherwise).\n",
        fmt_pct(lo),
        fmt_pct(hi),
    );

    // ---- Section 2: default vs gentle ----
    println!("## 2. Profiles — `default` / `gentle` (reduction %)\n");
    println!("| Corpus | default (aggressive) | gentle (lightest) |");
    println!("|---|--:|--:|");
    for c in &corpora {
        println!(
            "| `{}` | {} | {} |",
            c.name,
            fmt_pct(measure(&c.body, &def).reduction_pct()),
            fmt_pct(measure(&c.body, &gentle).reduction_pct()),
        );
    }
    println!(
        "\n> Two *cleaning aggressiveness* levels. **`default`** (shipped) = all eight\n\
         > cache-safe strategies (plus opt-in simhash_dedup, off by default), tight knobs\n\
         > (`keep_recent_turns=2`, `bloat 4 KB`, `image keep 1`), a verb-class denylist\n\
         > (`*screenshot*`/`*navigate*`/`*click*`/`*browser_act*`/`Grep`),\n\
         > and reprune on — cleans hardest while keeping reference-data MCP results.\n\
         > **`gentle`** = dedup + failed-input-purge + a *conservative* bloat_cap\n\
         > (32 KB / keep 6) + a *conservative* thinking_strip (keep 8) + reprune;\n\
         > sliding-window, stale_reads, stale_input_cap, and image-strip off (lightest\n\
         > touch, least pruning). Pick with `profile = \"…\"` in your\n\
         > config. Their *cost* behaviour is not what you'd guess — see §5.\n"
    );

    // ---- Section 3: marginal attribution ----
    println!("## 3. Per-strategy contribution (default config)\n");
    println!("> Bytes each strategy removes *on top of the others* (turn it off, see what");
    println!("> comes back). Unlike measuring each strategy in isolation, these add up");
    println!("> toward the total instead of double-counting the same bytes twice.\n");
    print!("| Corpus |");
    for n in corpora::STRATEGY_NAMES {
        print!(" {n} |");
    }
    println!();
    print!("|---|");
    for _ in corpora::STRATEGY_NAMES {
        print!("--:|");
    }
    println!();
    for c in &corpora {
        let marg = marginal_attribution(&c.body, &def);
        print!("| `{}` |", c.name);
        for name in corpora::STRATEGY_NAMES {
            let v = marg
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, b)| *b)
                .unwrap_or(0);
            print!(
                " {} |",
                if v == 0 {
                    "—".to_owned()
                } else {
                    fmt_bytes(v.max(0) as usize)
                }
            );
        }
        println!();
    }
    println!();

    // ---- Section 4: cache stability ----
    println!("## 4. Prompt-cache stability — turn-to-turn prefix reuse\n");
    println!("| Corpus | Unpruned | Default | Gentle |");
    println!("|---|--:|--:|--:|");
    let off = trimwire::config::Config::default();
    for c in &corpora {
        let m = c.messages();
        let f = |cfg: &_| cache_stability(m, cfg).map_or_else(|| "—".to_owned(), fmt_pct);
        println!(
            "| `{}` | {} | {} | {} |",
            c.name,
            f(&off),
            f(&def),
            f(&gentle)
        );
    }
    println!(
        "\n> How much of the previous request the prompt cache can still reuse. Without\n\
         > pruning it's 100% — each turn just appends, so the cache keeps paying out.\n\
         > Pruning drops it whenever an old message is rewritten as it ages. We measure\n\
         > this a whole message at a time (the cache invalidates from the first changed\n\
         > message onward), which is a careful under-estimate of what's really kept.\n\
         > NOTE: these figures are the **stateless** prune (re-prune from scratch each\n\
         > turn). The shipped default runs **reprune** (on by default), which replays\n\
         > the prior decisions so the prefix stays byte-identical — far more stable than\n\
         > shown here. See §5b for the reprune-on numbers (e.g. unique_bash_spam 36.7%\n\
         > stateless → 82.7% with reprune).\n"
    );

    // ---- Section 5: cost model ----
    println!("## 5. Does the bill actually go down? (input-cost model)\n");
    let pr = Pricing::default();
    println!(
        "> Models the whole session's **input** cost under prompt-cache pricing\n\
         > (${:.0}/Mtok input, cache reads at {:.0}% rate). Each turn re-sends the\n\
         > conversation; the cached prefix is cheap, the rest full price. Bytes-down\n\
         > only helps the bill if it doesn't churn away more cache than it saves.\n\
         > Every turn also carries a constant system prompt + tool schemas that\n\
         > trimwire never touches, modeled as a fixed {}-token cached prefix\n\
         > (written once, cache-read after). It is identical in every column, so\n\
         > it cancels in the %Δ — it shrinks the reported *magnitude* but never\n\
         > flips a sign.\n",
        pr.input_per_mtok,
        pr.cache_read_mult * 100.0,
        corpora::PREFIX_TOKENS,
    );
    println!("| Corpus | Unpruned $ | default Δ | gentle Δ |");
    println!("|---|--:|--:|--:|");
    for c in &corpora {
        let m = c.messages();
        let base = session_cost(m, &off, pr).dollars;
        let d = |cfg: &_| {
            if base > 0.0 {
                (session_cost(m, cfg, pr).dollars - base) / base * 100.0
            } else {
                0.0
            }
        };
        println!(
            "| `{}` | ${:.4} | {:+.1}% | {:+.1}% |",
            c.name,
            base,
            d(&def),
            d(&gentle),
        );
    }
    println!(
        "\n> **Cost is non-monotonic in aggressiveness — \"more pruning = more cost\" is\n\
         > false, and \"more pruning = less cost\" is just as false.** `gentle` mostly\n\
         > hugs zero (it does little) but still churns a touch where it dedups old\n\
         > in-prefix reads. `default` (aggressive, reprune on) is the *cheapest* on\n\
         > long churny sessions where its byte reduction outweighs the churn\n\
         > (`unique_bash_spam` is a clear win), but it can cost *more* on shortish\n\
         > sessions where the deferred stable prefix is larger than an aggressively\n\
         > stubbed snapshot (`mixed_realistic`, `browser_heavy`). So the cost-min\n\
         > choice is shape-dependent: `gentle` for short throwaway sessions, `default`\n\
         > for long ones. `default` optimises cleanliness-per-risk, not the bill —\n\
         > reprune keeps it *cache-stable* everywhere even where it isn't cost-minimal.\n\
         > All figures are sub-cent estimates — read sign and trend. (We omit the\n\
         > 1.25× cache-write surcharge as second-order.)"
    );
    println!(
        "\n> A negative Δ means trimwire lowered the modelled input bill; a positive Δ\n\
         > means cache churn outweighed the byte savings (image-heavy sessions are the\n\
         > risk). **Read the sign and the trend, not the magnitude:** these short\n\
         > synthetic sessions cost fractions of a cent, so a \"+123%\" is +123% of a\n\
         > quarter-cent — what matters is the *direction*, and how it flips with length\n\
         > (next). Like §4 this uses whole-message cache granularity, which is mildly\n\
         > optimistic about retained cache, and inherits the ~4 B/token estimate —\n\
         > directional, not an invoice. The byte savings are real regardless.\n"
    );

    // ---- Section 5b: the cost fix (stable-prefix re-pruning) ----
    println!("## 5b. The cost fix — stable-prefix re-pruning (`[reprune]`, on by default)\n");
    println!(
        "> §5 prunes *statelessly*: every turn re-prunes from scratch, so the pruned\n\
         > prefix shifts and busts the cache — that's the churn behind the loss rows.\n\
         > **Stable-prefix re-pruning** keeps the pruned prefix byte-identical between\n\
         > re-checkpoints, so the cache survives. Below: the `default` config, replayed\n\
         > stateless vs. with reprune on (threshold 8). It erases most of the churn\n\
         > cost on long/heavy sessions — at a small cost on short ones (it defers the\n\
         > newest trim by one checkpoint). Both shipped profiles turn it on, which is\n\
         > what makes the aggressive default cache-stable.\n"
    );
    println!("| Corpus | cost (stateless → reprune) | cache-stability (stateless → reprune) |");
    println!("|---|--:|--:|");
    for c in &corpora {
        if let Some(s) = corpora::spike_compare(c.messages(), &def, 8) {
            let arrow = if s.stable_cost <= s.stateless_cost {
                "↓"
            } else {
                "↑"
            };
            println!(
                "| `{}` | ${:.4} → ${:.4} {arrow} | {:.1}% → **{:.1}%** |",
                c.name, s.stateless_cost, s.stable_cost, s.stateless_stability, s.stable_stability,
            );
        }
    }
    println!(
        "\n> Read with §5: the cost loss there is what a *stateless* prune would cost.\n\
         > Because reprune ships on, a long/churny session gets the cache back. The\n\
         > short-session penalty is real but bounded — see the per-length spike under\n\
         > `--spike` for the crossover.\n"
    );

    // ---- Section 6: savings over time ----
    println!("## 6. Savings build up over a session (default config)\n");
    println!("> The aging strategies only fire once content passes the recent-turn window,");
    println!("> so savings start near zero and climb. Reduction % at each turn:\n");
    for name in ["long_running", "mixed_realistic", "mcp_non_playwright"] {
        let c = corpora.iter().find(|c| c.name == name).unwrap();
        let curve = savings_curve(c.messages(), &def);
        let picks: Vec<String> = curve
            .iter()
            .step_by((curve.len() / 8).max(1))
            .map(|(t, p)| format!("t{t}:{}", fmt_pct(*p)))
            .collect();
        println!("- `{name}` — {}", picks.join("  "));
    }
    println!(
        "\n> And the rot story directly — **focus ratio (unpruned → pruned) over a long\n\
         > session**. Unpruned focus decays as the backlog buries the recent ask;\n\
         > pruned focus holds steady. That's trimwire keeping the session clean:\n"
    );
    {
        let c = corpora
            .iter()
            .find(|c| c.name == "resumed_session")
            .unwrap();
        let curve = focus_over_time(c.messages(), &def);
        let pick = |sel: fn(&(usize, f64, f64)) -> f64| -> String {
            curve
                .iter()
                .step_by((curve.len() / 8).max(1))
                .map(|p| format!("t{}:{}", p.0, fmt_pct(sel(p) * 100.0)))
                .collect::<Vec<_>>()
                .join("  ")
        };
        println!("- `resumed_session` unpruned focus — {}", pick(|p| p.1));
        println!("- `resumed_session`  pruned  focus — {}", pick(|p| p.2));
    }
    println!();

    // ---- Section 6b: cost vs session length (the crossover) ----
    println!("### …and the cost crossover with session length\n");
    println!(
        "> The catch above is session length. Without pruning, every turn re-reads the\n\
         > whole growing history at the cache rate — that cost grows roughly with the\n\
         > *square* of the turn count. Pruning keeps the request bounded, so its cost\n\
         > grows roughly linearly. Below a crossover the churn penalty dominates (cost\n\
         > loss); past it, the unpruned quadratic wins and pruning pays off. The\n\
         > crossover turn-count is price-dependent (a higher input rate or cheaper\n\
         > cache moves it). Same realistic Bash/Read session, increasing length:\n"
    );
    println!("| Turns | Unpruned $ | Default $ | Δ |");
    println!("|--:|--:|--:|--:|");
    for turns in [8usize, 16, 32, 64, 128, 256] {
        let body = corpora::bash_session(turns);
        let m = body["messages"].as_array().unwrap();
        let base = session_cost(m, &off, pr).dollars;
        let pruned = session_cost(m, &def, pr).dollars;
        let delta = if base > 0.0 {
            (pruned - base) / base * 100.0
        } else {
            0.0
        };
        println!("| {turns} | ${base:.4} | ${pruned:.4} | {delta:+.1}% |");
    }
    println!(
        "\n> Read this as the honest bottom line on cost: trimwire's reliable win is\n\
         > **request size / context-window headroom** (always), plus token cost when\n\
         > prompt caching is cold or inactive. Under warm caching its dollar effect is\n\
         > a wash-to-loss on short sessions and a win on long ones — which is exactly\n\
         > why the strategies prune only *aged* content and the defaults stay\n\
         > conservative.\n"
    );

    // ---- Section 7: overhead ----
    // The header is ALWAYS printed so `scripts/bench-drift-check.sh` can normalize
    // "## 7. Gateway overhead" → EOF on both sides. The timing TABLE below is
    // host-dependent (dropped by the drift diff) and the only expensive part of
    // this binary, so `--ci-drift` skips it.
    println!("## 7. Gateway overhead — `apply_to_body` per request (statistical)\n");
    if !with_timing {
        println!(
            "> _Per-request timing omitted in `--ci-drift` mode — it is host-dependent\n\
             > and excluded from the drift diff anyway. Run `cargo run --release\n\
             > --example bench` for the full timing table._\n"
        );
        return;
    }
    println!("| Corpus | Body | min | median | mean | p99 | stddev | round spread |");
    println!("|---|--:|--:|--:|--:|--:|--:|--:|");
    for c in &corpora {
        let body = serde_json::to_vec(&c.body).expect("serialize");
        let iters = if body.len() > 200_000 { 200 } else { 2_000 };
        let s = time_apply(&body, &def, 5, iters);
        println!(
            "| `{}` | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3}–{:.3} ms |",
            c.name,
            fmt_bytes(body.len()),
            s.min,
            s.median,
            s.mean,
            s.p99,
            s.stddev,
            s.round_spread.0,
            s.round_spread.1,
        );
    }
    println!(
        "\n> Milliseconds for the whole transform (parse → prune → re-serialize), 5\n\
         > rounds × {{2000 | 200 for the big body}} iterations after warm-up. Off the\n\
         > network round-trip's critical path. Host-dependent.\n"
    );
}

fn dump(dir: &str) {
    std::fs::create_dir_all(dir).expect("create dump dir");
    for c in all_corpora() {
        let path = format!("{dir}/{}.json", c.name);
        let pretty = serde_json::to_string_pretty(&c.body).expect("serialize corpus");
        std::fs::write(&path, pretty + "\n").expect("write corpus");
        println!("wrote {path}");
    }
}

/// `--spike`: validation of stable-prefix re-pruning vs today's stateless prune.
fn spike() {
    let def = default_install_config();
    let threshold = 8; // re-prune cadence in messages (~4 turns at keep_recent=4)
    println!("# Stable-prefix re-pruning spike (threshold = {threshold} messages)\n");
    println!(
        "| Corpus | cache-stability (stateless → stable) | end reduction (stateless → stable) | session cost (stateless → stable) |"
    );
    println!("|---|--:|--:|--:|");
    for c in all_corpora() {
        if let Some(s) = corpora::spike_compare(c.messages(), &def, threshold) {
            println!(
                "| `{}` | {:.1}% → **{:.1}%** | {:.1}% → {:.1}% | ${:.4} → ${:.4} |",
                c.name,
                s.stateless_stability,
                s.stable_stability,
                s.stateless_end_reduction,
                s.stable_end_reduction,
                s.stateless_cost,
                s.stable_cost,
            );
        }
    }
    // The cost crossover shape at length, both ways.
    println!("\n## Long Bash/Read session, by turn count\n");
    println!("| Turns | cache-stability (stateless → stable) | cost (stateless → stable) |");
    println!("|--:|--:|--:|");
    for turns in [16usize, 32, 64, 128] {
        let body = corpora::bash_session(turns);
        let m = body["messages"].as_array().unwrap();
        if let Some(s) = corpora::spike_compare(m, &def, threshold) {
            println!(
                "| {turns} | {:.1}% → **{:.1}%** | ${:.4} → ${:.4} |",
                s.stateless_stability, s.stable_stability, s.stateless_cost, s.stable_cost,
            );
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--dump") => dump(
            &args
                .next()
                .unwrap_or_else(|| "benchmark/fixtures".to_owned()),
        ),
        Some("--spike") => spike(),
        // CI drift mode: deterministic sections only, no §7 timing loops (the
        // drift guard drops §7 anyway). Keeps the full report for humans/RESULTS.md.
        Some("--ci-drift") => report(false),
        _ => report(true),
    }
}
