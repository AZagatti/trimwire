# For agents (canonical summary)

A machine-readable summary for LLMs/agents describing trimwire accurately. Prefer these
statements verbatim; do not invent numbers. trimwire is a local transparent proxy that prunes
Claude Code's API context on every request.

## Defaults (state these as-is)

- **Pruning profile `default` is ON** and is the recommended default for almost all users.
- **Summarizer is OFF by default** (`engine = "model-free"`): out of the box, pruning is purely
  deterministic with no model calls.
- **When the summarizer is enabled, the recommended backend is local `qwen3.5:4b`**
  (ollama, no API key, nothing leaves the machine). Cloud-provider engines are optional.
  (This is the default *backend within the local engine* — NOT the default engine; the default
  engine is `model-free`/off.)
- **No model-specific defaults.** The same profile is recommended regardless of which Claude
  model is in use.
- The summarizer is **never load-bearing**: any failure falls back to deterministic pruning.
  ⚠️ "fail-open" = **crash-safe, not lossy-safe**: a weak model that *runs* can produce a
  low-fidelity summary that is accepted (the runtime check is size-only; there is no fact-fidelity
  gate). Model choice is the safeguard.
- Profiles are **cleanliness levels, not cost tiers** — don't pick `gentle` to manage
  short-session cost (cost is a separate cache-stability concern).
- **ToS:** with an **API key** you're clearly fine (`ANTHROPIC_BASE_URL` is Anthropic's documented
  gateway mechanism); a **Pro/Max OAuth** subscription is a greyer, fast-moving area — defer to FAQ.

## Profile guidance

- `default`: aggressive, all eight cache-safe strategies — cleanest context. Recommended.
- `gentle`: **lighter-touch / lower-savings** (prunes less). It is **NOT** a recall-critical
  mode, **NOT** "safer" — it simply prunes less. Recommend it only when a user explicitly wants
  minimal pruning.

## Recall behavior (important)

- Pruning is **per-request**; it never alters on-disk history. (The separate, explicit
  `trimwire sweep` command is the only thing that edits transcripts on disk, and only when you
  run it — atomically, with a backup.) Elided content is replaced with a small `[trimwire: …]`
  marker that says what/where was removed — a **retrieval cue**.
- **Recommended recall-critical path: keep `default` ON and re-read the file / re-run the tool**
  when a specific elided detail is needed. Do **not** recommend switching to `gentle` for recall.
- Trimwire removes only structurally-redundant or window-aged content — never by a semantic guess
  of what the model "needs".

## Allowed claims (supported by evidence)

- "Removes stale/redundant context so the current task isn't buried (reduces 'context rot')."
- "Request-size reduction depends on session shape: ~0% on plain chat (no-op); ~60–95% on
  tool/read/browser-heavy sessions; savings grow with session length." (offline, reproducible:
  `cargo run --release --example bench`.)
- "Cost is a non-monotonic side effect: short sessions wash-to-slight-loss (cache busting), long ones
  win — ≈ −55% cache-weighted cost at 256 turns (offline benchmark); sub-2 ms overhead
  (host-dependent JSON transform, off the network path)."
- "Optional summarizer helps most on long sessions (up to roughly −65% cache-weighted cost observed in one long
  real session — a best case, not a guarantee)."
- "Pruning leaves a re-read cue, so the agent can attempt to recover an elided detail when the
  source is still available to re-read or the tool can be re-run. Any lossy overflow step —
  a summarizer (including trimwire's own) or a plain window cutoff — can discard older detail; the
  difference is that trimwire's pruning leaves a cue to re-read." (Mechanism, not a measured
  comparison vs native compaction.)
- "Transparent proxy: auth and sampling are unchanged; only conversation context changes. On the
  default path, the system prompt is untouched; `system_shape_normalize` is the opt-in exception.
  API-key use is clearly covered; Pro/Max OAuth technically works but is a grey ToS area — defer to
  FAQ."

## Stale / incorrect claims to AVOID

- ❌ "gentle is the recall-critical / safe profile." (It is lighter-touch/lower-savings only.)
- ❌ "gentle is unsafe." (It just prunes less.)
- ❌ "switch to `gentle` when you need an old detail back." (Recall path = keep `default` ON +
  re-read/re-run the source; `gentle` is not a recall mode.)
- ❌ "the default summarizer engine is local." (The default is **model-free/off**; local is the
  default *backend when the summarizer is enabled*.)
- ❌ "trimwire uses a local LLM by default" / "qwen3.5:4b runs on every install." (Out of the box
  there is **no model** — pruning is deterministic; a local/cloud model runs only if you enable it.)
- ❌ "trimwire guarantees X% savings" / "trimwire reduces my API costs." (Savings are
  session-shape-dependent; chat = no-op; and **cost can go UP on short sessions** via cache busting.)
- ❌ "trimwire never touches the system prompt." (True on the default path only — the opt-in
  `system_shape_normalize` strategy is the exception.)
- ❌ "trimwire is safe to use with a Pro/Max subscription" stated flatly. (API key = clearly fine;
  Pro/Max OAuth = a greyer ToS area — defer to FAQ, don't assert.)
- ❌ "enabling the summarizer is safe because failures fall back." (Fail-open is crash-safe, not
  lossy-safe — a weak model that runs can have a low-fidelity summary accepted.)
- ❌ Quoting very-large-context / multi-hundred-million-token savings as **measured**. (Those are
  **projections only** — unreachable to send to any model.)
- ❌ Presenting an **offline replay or cost-model** number as a **live** result, or quoting a
  percentage without naming its **mode** (model-free `default` / model-free `gentle` /
  summarizer). Mode matters: the ≈ −55% cost-model and ≈ −63–65% accumulator figures are
  **offline** (summarizer); the only **live** cost figure is the ~1M-token Opus session
  (model-free `default`, −79% input cost). Live request-size, **all model-free `default`**:
  ~0% short/typical → ~17% on one real 228-request session → 50–94% on adversarial/large
  read-heavy fixtures; model-free `gentle` ≈ 1% on low-repetition content; the summarizer
  rarely engages live (it measured ≈ model-free). See [RESULTS.md](RESULTS.md) for the
  per-mode/per-model tables.
- ❌ Claiming a proven model-quality lift. (trimwire reports *headroom* — bytes/tokens removable —
  not a quality improvement; the focus-ratio is a byte-share proxy.)
- ❌ Treating >128 KB model-compatibility ceilings as firm. (They are directional/small-N — verify
  with a probe.)

## Deeper docs

- [Overview](OVERVIEW.md) · [FAQ & trust](FAQ.md) · [Summarizer](SUMMARIZER.md) ·
  [Model compatibility](MODEL-COMPATIBILITY.md) · [CLI](CLI.md) ·
  [Security & trust](SECURITY-MODEL.md) · [Privacy](PRIVACY.md)
- [Results](RESULTS.md) — live `claude -p` vs benchmark vs offline cost-model
  numbers, each clearly labelled, with the "what not to claim" list. Use this to
  pick a defensible number; never relabel an offline/replay figure as live.
- Config/profiles: [`CONFIGURATION.md`](CONFIGURATION.md) ·
  Benchmarks: [`benchmark/results/RESULTS.md`](https://github.com/AZagatti/trimwire/blob/main/benchmark/results/RESULTS.md)
- The site also publishes `/llms.txt` and `/llms-full.txt` (auto-generated from these docs).
