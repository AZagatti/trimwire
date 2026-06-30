<script>
  /**
   * v14.3 hero — a real Claude Code session (LEFT) beside the live Trimwire
   * gateway (RIGHT). Product-accurate to the repo:
   *  - LEFT statusline is NATIVE Claude Code chrome inside the terminal: model ·
   *    green ASCII context bar · % used · used/200K · `accept edits on`. It tracks
   *    CONTEXT-WINDOW USAGE (not savings): it rises as the conversation grows and
   *    DROPS after Trimwire prunes/summarizes the next request, then grows again.
   *  - RIGHT is the local gateway log: real route POST /v1/messages, pruning
   *    messages[], inbound→sent KB, real strategy names + per-strategy KB, retained
   *    raw tail, TTFT / tokens / cache / applied_edits. No savings-% hero claims.
   *  - Honest split: the transcript is never mutated; daemon ops hover-link to the
   *    exact source row(s). Summarizer folds older spans (grouped highlight).
   * State machine is generation-guarded (rapid clicks can't corrupt it).
   * Motion = opacity/transform only; reduced-motion → final state.
   * Byte/token figures are an illustrative demo session.
   */
  import { tick } from "svelte";
  import { Tween, prefersReducedMotion } from "svelte/motion";
  import { cubicOut } from "svelte/easing";
  import { fade } from "svelte/transition";

  // Transform-only entrance for transcript rows: a brief upward slide with NO
  // opacity ramp, so text is always at full contrast (avoids axe/Lighthouse
  // flagging a mid-fade frame as low-contrast). Reduced-motion → instant.
  function riseIn(_node, { duration = 170 } = {}) {
    if (reduced) return { duration: 0 };
    return { duration, easing: cubicOut, css: (t) => `transform: translateY(${(1 - t) * 4}px)` };
  }

  const TABS = [
    { label: "Claude", icon: "claude" },
    { label: "Codex", icon: "openai" },
    { label: "opencode", icon: "opencode" },
    { label: "Gemini", icon: "gemini" },
  ];
  const MASCOT = "  * ▐▛███▜▌ *\n * ▝▜█████▛▘ *\n  *  ▘▘ ▝▝  *";
  const MODEL = "sonnet 4.6";
  const CTX_TOTAL = 200; // 200K context window

  // Real session output only. `g` links a row to the daemon strategy that shapes
  // it on the wire (text never changes). `old` = turn-1 backlog the summarizer
  // folds. `ctx` = native context-window usage % the bar reaches after this row.
  // Longer multi-turn session: usage climbs, the gateway prunes each request, so
  // the bar oscillates in a healthy band instead of running away. `prune` on a
  // row = the gateway shapes that turn's request there (turn 1 = full showcase,
  // later turns = compact one-line prunes → the daemon keeps pace with the agent).
  const EV = [
    // ── turn 1 — refactor storage to sqlite (the teaching moment) ──
    { k: "user", text: "refactor the storage layer to use sqlite", ctx: 9 },
    { k: "asst", text: "Let me see how storage works today.", ctx: 12 },
    { k: "tool", call: "Read(app/storage.ts)", res: "86 lines", g: "reads", old: 1, ctx: 19 },
    { k: "tool", call: 'Grep("interface Store")', res: "12 matches · 4 files", g: "window", old: 1, ctx: 26 },
    { k: "asst", text: "It's a JSON-backed store. I'll add a sqlite implementation.", ctx: 30 },
    { k: "tool", call: "Bash(npm i better-sqlite3)", res: "added 1 package · 612 lines", g: "bloat", old: 1, ctx: 41 },
    { k: "tool", call: "Edit(app/storage.ts)", res: "+48 −12", keep: 1, ctx: 46 },
    { k: "tool", call: "Bash(npm test)", res: "FAIL · 2 failing · “no such table: todos”", g: "dedup", old: 1, ctx: 54 },
    { k: "asst", text: "Migration's missing — I'll add the schema.", ctx: 58, prune: "begin" },
    { k: "tool", call: "Write(app/migrations/001_init.sql)", res: "created · 14 lines", keep: 1, ctx: 63 },
    { k: "tool", call: "Edit(app/db.ts)", res: "+9 −0", keep: 1, ctx: 68 },
    { k: "tool", call: "Bash(npm test)", res: "40 passing · 3.0s", keep: 1, ctx: 73 }, // turn-1 peak
    { k: "asst", text: "sqlite storage is in and the suite passes.", prune: "show" },
    // ── turn 2 — keep the JSON export working ──
    { k: "user", text: "also keep the JSON export working", ctx: 44 },
    { k: "asst", text: "Routing the export through sqlite.", ctx: 48 },
    { k: "tool", call: "Edit(app/export.ts)", res: "+21 −3", keep: 1, ctx: 52 },
    { k: "tool", call: "Bash(npm test)", res: "41 passing · 3.1s", g: "dedup2", ctx: 56 },
    { k: "asst", text: "JSON export now reads from sqlite.", prune: "t2" },
    // ── turn 3 — concurrency safety ──
    { k: "user", text: "make sure concurrent writes don't corrupt the db", ctx: 53 },
    { k: "asst", text: "I'll wrap writes in a transaction and turn on WAL.", ctx: 58 },
    { k: "tool", call: "Read(app/db.ts)", res: "63 lines", g: "reads2", ctx: 64 },
    { k: "tool", call: "Edit(app/db.ts)", res: "+12 −2 · BEGIN IMMEDIATE + WAL", keep: 1, ctx: 69 },
    { k: "tool", call: "Bash(npm test -- --concurrency)", res: "44 passing · 4.2s", g: "dedup2", ctx: 72 }, // turn-3 peak
    { k: "asst", text: "Writes are transactional now and WAL is on.", prune: "t3" },
    // ── turn 4 — query performance ──
    { k: "user", text: "add an index so the list query is fast", ctx: 56 },
    { k: "asst", text: "Adding an index on (done, created_at).", ctx: 60 },
    { k: "tool", call: "Edit(app/migrations/002_index.sql)", res: "created · 3 lines", keep: 1, ctx: 63 },
    { k: "tool", call: "Bash(npm test)", res: "45 passing · 3.4s", g: "dedup2", ctx: 66 },
    { k: "asst", text: "Indexed — list query 40ms → 3ms. Tests green (45).", prune: "t4" },
  ];

  // Real pruning strategies + per-strategy KB. The turn-1 totals match the live
  // `resumed_session` long-session result (186.4 KB → 65.2 KB sent, 65% on
  // default), with bloat_cap dominant as it is on real read-heavy sessions.
  // Later turns prune smaller, per-request deltas (the gateway runs every turn).
  const IN_KB = 186;
  const OPS = {
    default: [
      { s: "bloat_cap", kb: 92, why: "install log + old results", g: "bloat" },
      { s: "stale_reads", kb: 14, why: "superseded read", g: "reads" },
      { s: "cross_turn_dedup", kb: 9, why: "repeated test run", g: "dedup" },
      { s: "sliding_window", kb: 6, why: "old search output", g: "window" },
    ],
    gentle: [
      { s: "bloat_cap", kb: 71, why: "conservative cap on old results", g: "bloat" },
      { s: "cross_turn_dedup", kb: 9, why: "repeated test run", g: "dedup" },
    ],
  };
  // compact per-turn prunes for the later requests (each is one daemon line)
  const TURN_PRUNES = {
    t2: { kb: 12, ops: "cross_turn_dedup", g: "dedup2" },
    t3: { kb: 21, ops: "stale_reads · cross_turn_dedup", g: "reads2" },
    t4: { kb: 9, ops: "cross_turn_dedup", g: "dedup2" },
  };
  const sentKb = (m) => IN_KB - (OPS[m] ?? OPS.default).reduce((a, o) => a + o.kb, 0);

  // context-usage settling points (%). The bar oscillates: each turn climbs, each
  // prune nudges it back. CTX_DONE = where it rests after the whole session.
  const CTX_DONE = { default: 50, gentle: 60 }; // live read-heavy default band
  const CTX_T1 = { default: 46, gentle: 56 };   // bar rests here right after turn-1 prune
  const CTX_SUMM = 41;           // after summarizer folds older spans
  const CTX_OFF = 78;            // gateway off → no prune → pressure stays high

  let gateway = $state(true);
  let mode = $state("default");
  let phase = $state("boot");
  let paused = $state(false);
  let summ = $state("idle"); // idle | running | done
  let shownN = $state(0);
  let typed = $state("");
  let log = $state([]);
  let shaped = $state(false);
  let flashG = $state(new Set());
  let hoverG = $state(null);
  let summLink = $state(false);
  let convoEl = $state(null), daemonEl = $state(null);
  let stickC = true, stickD = true;
  let gen = 0;
  let resumeFn = null;

  const reduced = $derived(prefersReducedMotion.current);
  const wait = (ms) => new Promise((r) => setTimeout(r, reduced ? 0 : ms));
  const gate = () => (paused ? new Promise((r) => (resumeFn = r)) : Promise.resolve());

  // native context-window usage bar
  const ctx = new Tween(7, { duration: 850, easing: cubicOut });
  const ctxPct = $derived(Math.round(ctx.current));
  const ctxK = $derived(Math.round((ctx.current / 100) * CTX_TOTAL));
  const BAR = 12;
  const ctxFill = $derived(Math.max(0, Math.min(BAR, Math.round((ctx.current / 100) * BAR))));
  // usage level drives the bar colour, like a real Claude Code statusline:
  // green under pressure, amber as it fills, red when the window is nearly full.
  const ctxLevel = $derived(ctx.current >= 80 ? "hi" : ctx.current >= 60 ? "mid" : "ok");

  function activeFlash(g) { return flashG.has(g) || (hoverG && hoverG.has(g)); }
  async function follow(el, on) { await tick(); if (el && on) el.scrollTop = el.scrollHeight; }
  function onScroll(w) { const el = w === "c" ? convoEl : daemonEl; if (!el) return; const b = el.scrollTop + el.clientHeight >= el.scrollHeight - 4; if (w === "c") stickC = b; else stickD = b; }
  function reveal(sel) { if (!convoEl) return; const el = convoEl.querySelector(sel); if (!el) return; stickC = false; const t = el.getBoundingClientRect().top - convoEl.getBoundingClientRect().top + convoEl.scrollTop; convoEl.scrollTo({ top: Math.max(0, t - convoEl.clientHeight / 2), behavior: reduced ? "auto" : "smooth" }); }
  function live() { stickC = true; if (convoEl) convoEl.scrollTo({ top: convoEl.scrollHeight, behavior: reduced ? "auto" : "smooth" }); }
  async function flash(g) { flashG = new Set([g]); await wait(1200); if (flashG.has(g)) flashG = new Set(); }

  // Click a daemon line → reveal the linked row(s), highlight briefly, then let
  // it settle back. Works on touch (no hover needed) and is self-clearing so the
  // highlight never stays stuck. `groups` may be one or several row-group keys.
  let clickTok = 0;
  let poking = $state(false); // true only while a click-poke's timed highlight is live
  async function pokeGroups(groups) {
    const set = new Set(groups.filter(Boolean));
    if (!set.size) return;
    const my = ++clickTok;
    poking = true;
    hoverG = set;
    reveal(`[data-g="${[...set][0]}"]`);
    await wait(1500);
    if (my !== clickTok) return;          // another click superseded us
    poking = false;
    hoverG = null;
    if (phase === "done") live();
  }

  function reset() { shownN = 0; typed = ""; log = []; shaped = false; flashG = new Set(); hoverG = null; summLink = false; summ = "idle"; clickTok++; poking = false; }
  function idleLog() {
    return gateway
      ? [{ kind: "req" }, { kind: "idle" }]
      : [{ kind: "req" }, { kind: "off" }];
  }

  // Turn-1 full prune showcase — the teaching moment. Built incrementally so it
  // interleaves with the agent's turn-1 stream (feels concurrent, not "wait then
  // prune"). Returns when the forward line is on screen.
  async function shapeTurn1(my, animate) {
    if (!gateway) { log = idleLog(); shaped = false; return; }
    log = [{ kind: "req" }, { kind: "head" }];
    await follow(daemonEl, stickD); if (animate) { await wait(200); if (my !== gen) return; }
    for (const op of (OPS[mode] ?? OPS.default)) {
      log = [...log, { kind: "op", op }];
      if (op.g && animate) flash(op.g);
      await follow(daemonEl, stickD);
      if (animate) { await wait(300); await gate(); if (my !== gen) return; }
    }
    log = [...log, { kind: "retain" }, { kind: "fwd", sent: sentKb(mode) }, { kind: "edits" }];
    shaped = true;
    await follow(daemonEl, stickD);
  }

  // Compact per-turn prune (one request → one line) for turns 2+.
  async function shapeTurn(my, key) {
    if (!gateway || !shaped) return;
    const tp = TURN_PRUNES[key]; if (!tp) return;
    log = [...log, { kind: "turnreq" }, { kind: "turnop", tp }];
    if (tp.g) flash(tp.g);
    await follow(daemonEl, stickD);
  }

  async function runSummarizer(my) {
    if (summ !== "idle" || !gateway || mode !== "summarizer") return;
    summ = "running"; summLink = true; reveal(".m-old");
    log = [...log, { kind: "summhead" }];
    await follow(daemonEl, stickD); await wait(reduced ? 0 : 1100); if (my !== gen || mode !== "summarizer") { summ = "idle"; summLink = false; live(); return; }
    summ = "done";
    log = [...log, { kind: "summ", sent: sentKb("default") }];
    ctx.set(CTX_SUMM, { duration: reduced ? 0 : 850 }); // context drops further
    await follow(daemonEl, stickD); await wait(reduced ? 0 : 700); if (my !== gen) return;
    live(); // keep older spans subtly grouped (summLink stays true → .summdone)
  }

  // re-shape the daemon for the current mode/gateway without re-streaming the
  // transcript (used when toggling after the run settled)
  async function reshapeDaemon(my, animate) {
    summ = "idle"; summLink = false;
    await shapeTurn1(my, animate); if (my !== gen) return;
    for (const e of EV) { if (e.prune && e.prune !== "show") { await shapeTurn(my, e.prune); if (my !== gen) return; } }
  }

  // full play — stream the transcript once; daemon prunes each turn as it lands;
  // summarizer at the end (summarizer mode only)
  async function run() {
    const my = ++gen; paused = false; resumeFn = null;
    reset(); ctx.set(7, { duration: 0 }); phase = "running"; log = idleLog();
    if (reduced) {
      shownN = EV.length;
      await reshapeDaemon(my, false);
      ctx.set(gateway ? CTX_DONE[mode] ?? 50 : CTX_OFF, { duration: 0 });
      phase = "done"; await runSummarizer(my); return;
    }
    await wait(650); await gate(); if (my !== gen) return;
    let turn1P = null;
    for (let i = 0; i < EV.length; i++) {
      const e = EV[i];
      if (e.k === "user") { await wait(600); await gate(); if (my !== gen) return; for (let c = 0; c <= e.text.length; c++) { typed = e.text.slice(0, c); await wait(40); if (my !== gen) return; } await wait(320); typed = ""; }
      shownN = i + 1;
      if (e.ctx != null) ctx.set(gateway ? e.ctx : Math.max(e.ctx, CTX_OFF * (i / EV.length)), { duration: 700 });
      await follow(convoEl, stickC);
      // daemon shapes the turn's request IN PARALLEL with the transcript: the
      // turn-1 showcase STARTS a few rows early ("begin") and runs concurrently
      // while the agent keeps streaming, then the context-bar drop is synced to the
      // moment the prune completes ("show"). Later turns are a compact one-liner.
      if (e.prune === "begin") { turn1P = shapeTurn1(my, true); }
      else if (e.prune === "show") {
        await (turn1P ?? shapeTurn1(my, true)); turn1P = null; if (my !== gen) return;
        if (gateway) ctx.set(CTX_T1[mode] ?? 46, { duration: reduced ? 0 : 700 }); // bar drops WITH the prune
      }
      else if (e.prune) { await shapeTurn(my, e.prune); if (my !== gen) return; }
      await wait(e.k === "tool" ? 480 : 300); await gate(); if (my !== gen) return;
    }
    if (gateway) ctx.set(CTX_DONE[mode] ?? 50, { duration: reduced ? 0 : 700 });
    phase = "done";
    await runSummarizer(my);
  }

  // mode/gateway change after `done` → re-shape (no re-stream) + settle the bar
  function reshape() {
    const my = ++gen; paused = false; const r = resumeFn; resumeFn = null; if (r) r();
    if (!gateway) { log = idleLog(); shaped = false; ctx.set(CTX_OFF, { duration: reduced ? 0 : 700 }); return; }
    reshapeDaemon(my, true).then(() => {
      if (my !== gen) return;
      ctx.set(CTX_DONE[mode] ?? 50, { duration: reduced ? 0 : 700 });
      runSummarizer(my);
    });
  }
  function setMode(m) { if (m === mode || !gateway) return; mode = m; if (phase === "running" && !shaped) return; reshape(); }
  function toggleGateway() {
    gateway = !gateway;
    if (phase === "running" && !shaped) { run(); return; }
    // sync the context bar to the gateway state IMMEDIATELY (off → pressure climbs
    // back up, on → pruned band) instead of waiting for the daemon re-shape, then
    // re-shape the log to match.
    ctx.set(gateway ? (CTX_DONE[mode] ?? 50) : CTX_OFF, { duration: reduced ? 0 : 600 });
    reshape();
  }
  function pausePlay() {
    if (phase === "running" && !paused) paused = true;
    else if (paused) { paused = false; const r = resumeFn; resumeFn = null; if (r) r(); }
    else run();
  }

  // Auto-run once when scrolled into view. Re-running only happens on an explicit
  // replay click — casual scrolling back (especially on mobile) must NOT restart
  // it. A high threshold means it triggers only when the terminal is genuinely
  // the focus, not on a glancing scroll-by.
  let hasRun = false;
  function inview(node) {
    const o = new IntersectionObserver((es) => {
      if (es[0].isIntersecting && !hasRun) { hasRun = true; run(); o.disconnect(); }
    }, { threshold: 0.55 });
    o.observe(node); return { destroy() { o.disconnect(); } };
  }
</script>

<figure class="termwrap" use:inview>
  <figcaption class="sr-only">
    A Claude Code coding session on the left runs beside the local Trimwire gateway on the right. As the conversation
    grows, Claude Code's context bar fills; Trimwire intercepts the POST /v1/messages request, prunes redundant
    messages[] (repeated tool output, superseded reads, oversized logs) while keeping the current task and edits, and
    forwards a smaller request — so the context bar drops, then grows again as work continues. The transcript is never
    changed. Byte and token figures are an illustrative demo session.
  </figcaption>

  <div class="termwin" data-phase={phase} data-gw={gateway ? "on" : "off"}>
    <div class="winbar">
      <span class="lights" aria-hidden="true"><i class="l-r"></i><i class="l-y"></i><i class="l-g"></i></span>
      <span class="wintitle">~/todo-cli</span>
      <span class="win-spacer"></span>
      <span class="democtl"><button type="button" class="dc" aria-label={phase === "running" && !paused ? "Pause demo" : paused ? "Resume demo" : "Replay demo"} onclick={pausePlay}>{phase === "running" && !paused ? "⏸" : paused ? "▶" : "↻"}</button></span>
      <button type="button" class="gw-toggle" aria-label={`Trimwire ${gateway ? "on" : "off"} — toggle the gateway`} aria-pressed={gateway} onclick={toggleGateway}><span class="led" class:on={gateway}></span><span class="gw-lbl">Trimwire {gateway ? "on" : "off"}</span></button>
    </div>

    <div class="panes">
      <!-- LEFT — Claude Code (pristine transcript + native statusline) -->
      <section class="pane pane-cc" aria-label="Claude Code session">
        <!-- harness tabs belong to the Claude pane only — scoped to its width,
             horizontally scrollable with a right-edge fade when they overflow -->
        <div class="tabsrow" aria-hidden="true">
          {#each TABS as t, i}
            <span class="tab" class:active={i === 0} title={i === 0 ? "active session" : "switch harness"}>
              <svg class="ticon" width="12" height="12" viewBox="0 0 16 16">
                {#if t.icon === "claude"}<path d="M8 1v14M1 8h14M3 3l10 10M13 3L3 13" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" />{/if}
                {#if t.icon === "openai"}<path d="M8 1.4 13.7 4.7v6.6L8 14.6 2.3 11.3V4.7Z M8 1.4V14.6 M2.3 4.7 13.7 11.3 M13.7 4.7 2.3 11.3" fill="none" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" />{/if}
                {#if t.icon === "opencode"}<path d="M6 3C4 3 4.3 7 2.3 8 4.3 9 4 13 6 13M10 3c2 0 1.7 4 3.7 5-2 1-1.7 5-3.7 5" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" />{/if}
                {#if t.icon === "gemini"}<path d="M8 1c.4 4 2.9 6.6 7 7-4.1.4-6.6 3-7 7-.4-4-2.9-6.6-7-7 4.1-.4 6.6-3 7-7Z" fill="currentColor" />{/if}
              </svg>
              <span class="tlabel">{t.label}</span>
            </span>
          {/each}
        </div>
        <div class="cc-scroll" aria-hidden="true" bind:this={convoEl} onscroll={() => onScroll("c")}>
          <div class="boot">
            <pre class="mascot">{MASCOT}</pre>
            <div class="boot-meta"><div class="boot-h"><b>✻ agent session</b></div><div class="boot-d">~/todo-cli</div></div>
          </div>
          <ol class="convo">
            {#each EV as e, i (i)}
              {#if i < shownN}
                <li class="msg m-{e.k}" data-g={e.g ?? ""} class:m-old={e.old} class:flash={e.g && activeFlash(e.g)} class:summlit={summLink && summ !== "done" && e.old} class:summdone={summLink && summ === "done" && e.old} transition:riseIn={{ duration: 170 }}>
                  {#if e.k === "user"}<span class="bul user">&gt;</span> <span class="utext">{e.text}</span>
                  {:else if e.k === "asst"}<span class="bul ok">⏺</span> {e.text}
                  {:else}<div class="call-line"><span class="bul ok">⏺</span> <span class="call">{e.call}</span></div><div class="res-line"><span class="elbow">⎿</span> {e.res}</div>{/if}
                </li>
              {/if}
            {/each}
          </ol>
        </div>
        <div class="cc-input"><span class="cc-ps">&gt;</span> <span class="typed">{typed}</span><span class="cursor"></span></div>
        <!-- native Claude Code statusline (context bar = window usage) -->
        <div class="ccline" data-level={ctxLevel} aria-hidden="true">
          <span class="cl-model">{MODEL}</span>
          <span class="cl-sep">·</span>
          <span class="cl-bar">{"█".repeat(ctxFill)}<span class="cl-empty">{"░".repeat(BAR - ctxFill)}</span></span>
          <span class="cl-pct">{ctxPct}%</span>
          <span class="cl-sep">·</span>
          <span class="cl-tok">{ctxK}K/{CTX_TOTAL}K</span>
        </div>
        <div class="ccline2" aria-hidden="true">⏵⏵ accept edits on</div>
      </section>

      <!-- RIGHT — local gateway log (technical, product-native) -->
      <section class="pane pane-tw" aria-label="Trimwire gateway log">
        <div class="tw-head">
          <span class="tw-name">trimwire · gateway</span>
          <span class="grow"></span>
          <div class="modesw" class:locked={!gateway} role="tablist" aria-label="Trimwire mode">
            <button type="button" role="tab" class:on={mode === "default"} aria-selected={mode === "default"} disabled={!gateway} onclick={() => setMode("default")}>Default</button>
            <button type="button" role="tab" class:on={mode === "gentle"} aria-selected={mode === "gentle"} disabled={!gateway} onclick={() => setMode("gentle")}>Gentle</button>
            <button type="button" role="tab" class="m-summ" class:on={mode === "summarizer"} aria-selected={mode === "summarizer"} disabled={!gateway} onclick={() => setMode("summarizer")}>Summarizer</button>
          </div>
        </div>
        <div class="daemon" aria-hidden="true" bind:this={daemonEl} onscroll={() => onScroll("d")}>
          {#each log as l, i (i)}
            <div
              class="lg lg-{l.kind}"
              class:linkable={(l.kind === "op" && l.op.g) || (l.kind === "turnop" && l.tp.g) || l.kind === "summ"}
              onmouseenter={() => { if (l.kind === "op" && l.op.g) { hoverG = new Set([l.op.g]); reveal(`[data-g="${l.op.g}"]`); } else if (l.kind === "turnop" && l.tp.g) { hoverG = new Set([l.tp.g]); reveal(`[data-g="${l.tp.g}"]`); } else if (l.kind === "summ") { hoverG = new Set(["reads", "window", "bloat", "dedup"]); reveal('[data-g="reads"]'); } }}
              onmouseleave={() => { if (!poking) { hoverG = null; if (phase === "done") live(); } }}
              onclick={() => { if (l.kind === "op" && l.op.g) pokeGroups([l.op.g]); else if (l.kind === "turnop" && l.tp.g) pokeGroups([l.tp.g]); else if (l.kind === "summ") pokeGroups(["reads", "window", "bloat", "dedup"]); }}
              transition:fade={{ duration: reduced ? 0 : 150 }}
            >
              {#if l.kind === "req"}
                <span class="route">POST /v1/messages</span> <span class="dim">· session 3f8a2c · {MODEL}</span>
              {:else if l.kind === "idle"}
                <span class="dim">watching messages[] · nothing to trim yet</span>
              {:else if l.kind === "off"}
                <span class="dim">no pruning · full messages[] forwarded unchanged</span>
              {:else if l.kind === "head"}
                <span class="dim">pruning messages[] · {IN_KB} KB inbound</span>
              {:else if l.kind === "op"}
                <span class="op-s">{l.op.s}</span> <span class="op-kb">−{l.op.kb} KB</span> <span class="op-why dim">{l.op.why}</span>
              {:else if l.kind === "retain"}
                <span class="op-keep">retained raw tail</span> <span class="dim">· current task + edits</span>
              {:else if l.kind === "fwd"}
                <span class="fwd">sent {l.sent} KB</span> <span class="dim">· TTFT 0.48s · in 11.2K tok · cache read 8.1K</span>
              {:else if l.kind === "edits"}
                <span class="dim">applied_edits 3</span>
              {:else if l.kind === "turnreq"}
                <span class="route">POST /v1/messages</span> <span class="dim">· next turn</span>
              {:else if l.kind === "turnop"}
                <span class="op-s">{l.tp.ops}</span> <span class="op-kb">−{l.tp.kb} KB</span> <span class="dim">· kept raw tail</span>
              {:else if l.kind === "summhead"}
                <span class="summc">summarizer</span> <span class="dim">· folding 3 older spans → 1 summary</span>
              {:else if l.kind === "summ"}
                <div class="summ-row">
                  <span class="summc">summary</span> <span class="dim">3 older spans → 1 · sent {sentKb("default")} → {l.sent - 14} KB</span>
                  <p class="summ-sum">“Migrated storage to sqlite (better-sqlite3); Store trait in app/storage.ts plus a migration; JSON export reads from sqlite; tests green (41).”</p>
                </div>
              {/if}
            </div>
          {/each}
        </div>
      </section>
    </div>
  </div>
  <span class="drag-hint dim" aria-hidden="true"><span class="dh-ar">⇆</span> drag or scroll across the scene</span>
</figure>

<style>
  .sr-only { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; overflow: hidden; clip: rect(0,0,0,0); white-space: nowrap; border: 0; }
  .termwrap { margin: 0; --t-fs: clamp(0.72rem, 0.68rem + 0.3vw, 0.82rem); --t-fs-sub: clamp(0.66rem, 0.63rem + 0.22vw, 0.74rem); }
  .termwin { border: 1px solid #232c2b; border-radius: 12px; overflow: hidden; background: #0b0e0e; font-family: var(--mono); box-shadow: 0 0 0 1px rgba(255,255,255,0.02), 0 24px 60px -34px rgba(0,0,0,0.9); }

  .winbar { display: flex; align-items: center; gap: 0.55rem; padding: 0.5rem 0.75rem; background: #0f1514; border-bottom: 1px solid #1d2625; line-height: 1; }
  .lights { display: inline-flex; align-items: center; gap: 0.42rem; } .lights i { width: 11px; height: 11px; border-radius: 999px; display: block; }
  .l-r { background: #ff5f56; } .l-y { background: #ffbd2e; } .l-g { background: #27c93f; }
  .wintitle { font-size: 0.72rem; color: var(--ink-3); display: inline-flex; align-items: center; } .win-spacer { flex: 1; }
  .democtl { display: inline-flex; align-items: center; }
  .dc { display: inline-flex; align-items: center; justify-content: center; width: 1.5rem; height: 1.5rem; font-size: 0.78rem; line-height: 1; color: var(--ink-3); background: transparent; border: 1px solid transparent; border-radius: 6px; cursor: pointer; transition: color 0.15s, border-color 0.15s; }
  .dc:hover { color: var(--ink); border-color: var(--border-2); }
  .gw-toggle { display: inline-flex; align-items: center; gap: 0.42rem; font: inherit; font-size: 0.72rem; line-height: 1; color: var(--ink-2); background: transparent; border: 1px solid var(--border-2); border-radius: 6px; padding: 0.28rem 0.55rem; cursor: pointer; transition: border-color 0.15s, color 0.15s; }
  .gw-toggle:hover { border-color: var(--accent); color: var(--ink); }
  .gw-toggle[aria-pressed="false"] { color: var(--ink-3); }
  .gw-lbl { display: inline-flex; align-items: center; }
  .led { width: 8px; height: 8px; border-radius: 999px; background: var(--ink-3); flex-shrink: 0; } .led.on { background: var(--ok); box-shadow: 0 0 7px color-mix(in srgb, var(--ok) 70%, transparent); }

  /* scoped to the Claude pane; scrolls horizontally with a right-edge fade that
     only "shows" when a tab actually reaches the edge (masks empty bg otherwise) */
  .tabsrow { display: flex; align-items: stretch; gap: 0.1rem; padding: 0 0.5rem; background: #0a0e0e; border-bottom: 1px solid #1d2625; flex-wrap: nowrap; overflow-x: auto; overflow-y: hidden; scrollbar-width: none; -webkit-mask-image: linear-gradient(to right, #000 calc(100% - 1.4rem), transparent); mask-image: linear-gradient(to right, #000 calc(100% - 1.4rem), transparent); }
  .tabsrow::-webkit-scrollbar { display: none; }
  .tab { display: inline-flex; align-items: center; gap: 0.4rem; height: 2rem; font-size: 0.73rem; line-height: 1; color: var(--ink-3); padding: 0 0.6rem; border-bottom: 2px solid transparent; white-space: nowrap; flex-shrink: 0; transition: color 0.15s, background 0.15s, border-color 0.15s; cursor: default; }
  .tab .ticon { opacity: 0.55; flex-shrink: 0; display: block; transition: opacity 0.15s; }
  .tab .tlabel { display: inline-flex; align-items: center; }
  .tab.active { color: var(--ink); border-bottom-color: var(--accent); } .tab.active .ticon { opacity: 1; color: #f0883e; }
  @media (hover: hover) { .tab:not(.active):hover { color: var(--ink-2); background: color-mix(in srgb, var(--accent) 6%, transparent); border-bottom-color: var(--border-2); } .tab:not(.active):hover .ticon { opacity: 0.9; } }

  .panes { display: grid; grid-template-columns: 1.7fr 1fr; }
  .pane { min-width: 0; display: flex; flex-direction: column; background: #0b0e0e; }
  .pane-cc { border-right: 1px solid #18211f; } .pane-tw { background: #090c0c; }

  .cc-scroll { height: clamp(15rem, 42vh, 22rem); overflow-y: auto; padding: 0.8rem 0.9rem 0.3rem; scrollbar-width: thin; scrollbar-color: #243130 transparent; overflow-anchor: none; }
  .cc-scroll::-webkit-scrollbar { width: 5px; } .cc-scroll::-webkit-scrollbar-thumb { background: #243130; border-radius: 999px; }
  .boot { display: flex; gap: 0.8rem; align-items: center; padding-bottom: 0.5rem; margin-bottom: 0.45rem; border-bottom: 1px solid #141b1a; }
  .mascot { margin: 0; font-size: 0.56rem; line-height: 1.05; color: #f0883e; text-shadow: 0 0 10px rgba(240,136,62,0.35); white-space: pre; }
  .boot-h { font-size: 0.76rem; color: var(--ink); } .boot-meta .boot-h b::first-letter { color: #f0883e; } .boot-d { font-size: var(--t-fs-sub); color: var(--ink-3); }
  .convo { list-style: none; margin: 0; padding: 0; font-size: var(--t-fs); line-height: 1.5; }
  .msg { padding: 0.12rem 0.35rem; margin: 0 -0.35rem; border-radius: 6px; color: var(--ink-2); transition: background 0.3s ease, box-shadow 0.3s ease; }
  .bul { color: var(--ink-3); } .bul.ok { color: var(--accent); } .bul.user { color: var(--accent-hi); }
  .m-user { color: var(--ink); margin-top: 0.35rem; } .utext { color: var(--ink); } .m-asst { color: var(--ink-2); }
  .call-line { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; } .call { color: var(--ink); }
  .res-line { color: var(--ink-3); padding-left: 1.05rem; font-size: var(--t-fs-sub); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; } .elbow { color: #3c4947; }
  .msg.flash { background: color-mix(in srgb, var(--accent) 15%, transparent); box-shadow: inset 2px 0 0 var(--accent); }
  .msg.summlit { background: color-mix(in srgb, var(--c-summ) 15%, transparent); box-shadow: inset 2px 0 0 var(--c-summ); }
  .msg.summdone { box-shadow: inset 2px 0 0 color-mix(in srgb, var(--c-summ) 65%, transparent); background: color-mix(in srgb, var(--c-summ) 6%, transparent); }

  .cc-input { display: flex; align-items: center; gap: 0.4rem; padding: 0.5rem 0.9rem; border-top: 1px solid #18211f; font-size: var(--t-fs); color: var(--ink); }
  .cc-ps { color: var(--accent); } .typed { white-space: pre; }
  .cursor { display: inline-block; width: 0.45rem; height: 0.95rem; background: var(--accent); animation: blink 1.1s step-end infinite; }
  @keyframes blink { 50% { opacity: 0; } }
  /* native Claude Code statusline — compact terminal chrome */
  .ccline { display: flex; align-items: baseline; gap: 0.4rem; padding: 0.3rem 0.9rem 0.05rem; font-size: 0.66rem; color: var(--ink-3); white-space: nowrap; overflow: hidden; }
  .cl-model { color: var(--ink-2); } .cl-sep { color: #3c4947; }
  .cl-bar { color: var(--ok); letter-spacing: -0.08em; } .cl-empty { color: #2a3534; }
  .cl-pct { color: var(--ink-2); font-variant-numeric: tabular-nums; } .cl-tok { color: var(--ink-3); font-variant-numeric: tabular-nums; }
  /* context-usage level colours the bar + percent (real CC statusline behaviour) */
  .ccline[data-level="mid"] .cl-bar { color: #e0b341; } .ccline[data-level="mid"] .cl-pct { color: #e0b341; }
  .ccline[data-level="hi"] .cl-bar { color: #e0664f; } .ccline[data-level="hi"] .cl-pct { color: #e0664f; }
  .ccline2 { padding: 0 0.9rem 0.4rem; font-size: 0.64rem; color: #7c8b89; }

  /* wrap so the mode switcher drops to its own line on a narrow daemon pane
     instead of overflowing the terminal window (which clips it). */
  .tw-head { display: flex; align-items: center; gap: 0.4rem 0.5rem; padding: 0.4rem 0.6rem; border-bottom: 1px solid #141b1a; flex-wrap: wrap; row-gap: 0.35rem; min-height: 2.1rem; }
  .tw-name { font-size: var(--t-fs-sub); color: var(--ink-3); white-space: nowrap; flex-shrink: 0; } .tw-head .grow { flex: 1; }
  .modesw { display: inline-flex; gap: 0.1rem; padding: 0.15rem; border: 1px solid var(--border-2); border-radius: 999px; background: var(--inset); flex-shrink: 0; flex-wrap: nowrap; }
  .modesw button { font-family: var(--mono); font-size: 0.66rem; color: var(--ink-3); background: transparent; border: 0; border-radius: 999px; padding: 0.22rem 0.5rem; cursor: pointer; transition: color 0.15s, background 0.15s; white-space: nowrap; }
  .modesw button.on { color: #04100f; background: var(--accent); }
  .modesw button.m-summ.on { color: #0a0612; background: var(--c-summ); }
  .modesw.locked { opacity: 0.55; }
  .modesw button:disabled { cursor: default; color: var(--ink-3); }
  .modesw button:disabled.on { background: color-mix(in srgb, var(--accent) 45%, #243130); color: #0c1211; }
  .modesw button:disabled.m-summ.on { background: color-mix(in srgb, var(--c-summ) 45%, #243130); color: #0c1211; }
  @media (hover: hover) { .modesw button:not(.on):not(:disabled):hover { color: var(--ink); } }

  /* flex:1 1 0 + min-height:0 so the daemon fills its pane down to match the
     Claude pane (no dead black space) BUT scrolls internally instead of growing
     the terminal — basis 0 + min-height 0 stop the tall log from inflating the
     row height. */
  .daemon { flex: 1 1 0; min-height: 0; overflow-y: auto; padding: 0.6rem 0.7rem; font-size: var(--t-fs-sub); line-height: 1.65; color: var(--ink-2); overflow-anchor: none; scrollbar-width: thin; scrollbar-color: #243130 transparent; }
  .daemon::-webkit-scrollbar { width: 5px; } .daemon::-webkit-scrollbar-thumb { background: #243130; border-radius: 999px; }
  .lg { margin-bottom: 0.14rem; border-radius: 5px; padding: 0.06rem 0.3rem; margin-inline: -0.3rem; transition: background 0.2s ease; }
  .lg.linkable { cursor: pointer; } .lg.linkable:hover { background: color-mix(in srgb, var(--accent) 12%, transparent); box-shadow: inset 2px 0 0 var(--accent); }
  .dim { color: var(--ink-3); }
  .route { color: var(--accent-hi); } .op-s { color: var(--ink); } .op-kb { color: var(--accent-hi); font-variant-numeric: tabular-nums; } .op-keep { color: var(--ok); }
  .fwd { color: var(--ok); }
  .summc { color: var(--c-summ); border: 1px solid color-mix(in srgb, var(--c-summ) 45%, transparent); border-radius: 999px; padding: 0.02rem 0.4rem; font-size: 0.64rem; }
  .summ-sum { margin: 0.3rem 0 0.1rem; padding: 0.45rem 0.55rem; border-left: 2px solid color-mix(in srgb, var(--c-summ) 50%, transparent); background: color-mix(in srgb, var(--c-summ) 7%, transparent); color: var(--ink); font-size: var(--t-fs-sub); line-height: 1.5; }

  .drag-hint { display: none; font-size: 0.72rem; align-items: center; gap: 0.35rem; margin-top: 0.5rem; }
  .dh-ar { color: var(--accent-hi); display: inline-block; animation: dh 1.6s ease-in-out infinite; }
  @keyframes dh { 0%,100% { transform: translateX(0); } 50% { transform: translateX(3px); } }

  @media (max-width: 52rem) {
    /* The WHOLE window pans as one wide canvas (Superset-style): the winbar (which
       stretches to the full scene width, so its Trimwire on/off toggle sits at the
       scene's right edge), the tabs, and both panes all scroll together. */
    /* thin (not hidden) horizontal scrollbar so MOUSE users on a narrow desktop
       window can see the canvas pans — on touch the overlay scrollbar auto-hides,
       so phones stay clean. Without this the off-screen daemon looked unreachable. */
    .termwin { min-width: 0; overflow-x: auto; overflow-y: hidden; scroll-snap-type: x mandatory; -webkit-overflow-scrolling: touch; scrollbar-width: thin; scrollbar-color: #2f3f3e transparent; }
    .termwin::-webkit-scrollbar { height: 8px; }
    .termwin::-webkit-scrollbar-thumb { background: #2f3f3e; border-radius: 999px; }
    .panes { display: flex; }
    /* bigger panes so the terminals are comfortably readable */
    .pane-cc { flex: 0 0 90vw; scroll-snap-align: start; } .pane-tw { flex: 0 0 86vw; scroll-snap-align: end; }
    .cc-scroll { height: clamp(15rem, 56vh, 22rem); }
    /* winbar + tabs span the FULL canvas (= pane-cc 90vw + pane-tw 86vw) so the
       title bar reads like a real window: lights at the left corner, the Trimwire
       on/off toggle at the canvas's RIGHT corner (not stranded mid-scene). */
    .winbar { width: 176vw; }
    .drag-hint { display: inline-flex; }
  }
  @media (prefers-reduced-motion: reduce) { .cursor, .msg, .dh-ar { animation: none; transition: none; } }
</style>
