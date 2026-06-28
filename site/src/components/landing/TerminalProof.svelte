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
  // folds. `ctx` = native context-window usage % the bar should reach after this
  // row (rises through turn 1, drops at the prune, rises again through turn 2).
  const EV = [
    { k: "user", text: "refactor the storage layer to use sqlite", ctx: 11 },
    { k: "asst", text: "Let me see how storage works today.", ctx: 14 },
    { k: "tool", call: "Read(app/storage.ts)", res: "86 lines", g: "reads", old: 1, ctx: 23 },
    { k: "tool", call: 'Grep("interface Store")', res: "12 matches · 4 files", g: "window", old: 1, ctx: 30 },
    { k: "asst", text: "It's a JSON-backed store. I'll add a sqlite implementation.", ctx: 33 },
    { k: "tool", call: "Bash(npm i better-sqlite3)", res: "added 1 package · 612 lines", g: "bloat", old: 1, ctx: 46 },
    { k: "tool", call: "Edit(app/storage.ts)", res: "+48 −12", keep: 1, ctx: 50 },
    { k: "tool", call: "Bash(npm test)", res: "FAIL · 2 failing · “no such table: todos”", g: "dedup", old: 1, ctx: 58 },
    { k: "asst", text: "Migration's missing — I'll add the schema.", ctx: 61 },
    { k: "tool", call: "Write(app/migrations/001_init.sql)", res: "created · 14 lines", keep: 1, ctx: 65 },
    { k: "tool", call: "Edit(app/db.ts)", res: "+9 −0", keep: 1, ctx: 68 },
    { k: "tool", call: "Bash(npm test)", res: "40 passing · 3.0s", keep: 1, ctx: 73 },
    { k: "asst", text: "sqlite storage is in and the suite passes." }, // OPS_AT: prune fires → drop
    { k: "user", text: "also keep the JSON export working", ctx: 36 },
    { k: "asst", text: "Routing the export through sqlite.", ctx: 39 },
    { k: "tool", call: "Edit(app/export.ts)", res: "+21 −3", keep: 1, ctx: 45 },
    { k: "tool", call: "Bash(npm test)", res: "41 passing · 3.1s", keep: 1, ctx: 50 },
    { k: "asst", text: "Done — the JSON export now reads from sqlite. Tests green (41).", ctx: 53 },
  ];
  const OPS_AT = 12;

  // Real pruning strategies + per-strategy KB (illustrative session). Each links
  // to its source row group. sent = inbound − Σ kb.
  const IN_KB = 186;
  const OPS = {
    default: [
      { s: "cross_turn_dedup", kb: 92, why: "repeated test run", g: "dedup" },
      { s: "stale_reads", kb: 41, why: "superseded read", g: "reads" },
      { s: "bloat_cap", kb: 8, why: "install log", g: "bloat" },
      { s: "sliding_window", kb: 4, why: "old search output", g: "window" },
    ],
    gentle: [
      { s: "cross_turn_dedup", kb: 92, why: "repeated test run", g: "dedup" },
      { s: "bloat_cap", kb: 8, why: "install log", g: "bloat" },
    ],
  };
  const sentKb = (m) => IN_KB - (OPS[m] ?? OPS.default).reduce((a, o) => a + o.kb, 0);

  // context-usage settling points (%)
  const CTX_PRUNE = 31;          // immediate drop when turn-1 prune fires
  const CTX_DONE = { default: 53, gentle: 64 }; // settled after turn 2 continues
  const CTX_SUMM = 27;           // after summarizer folds older spans
  const CTX_OFF = 74;            // gateway off → no prune → pressure stays high

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

  function activeFlash(g) { return flashG.has(g) || (hoverG && hoverG.has(g)); }
  async function follow(el, on) { await tick(); if (el && on) el.scrollTop = el.scrollHeight; }
  function onScroll(w) { const el = w === "c" ? convoEl : daemonEl; if (!el) return; const b = el.scrollTop + el.clientHeight >= el.scrollHeight - 4; if (w === "c") stickC = b; else stickD = b; }
  function reveal(sel) { if (!convoEl) return; const el = convoEl.querySelector(sel); if (!el) return; stickC = false; const t = el.getBoundingClientRect().top - convoEl.getBoundingClientRect().top + convoEl.scrollTop; convoEl.scrollTo({ top: Math.max(0, t - convoEl.clientHeight / 2), behavior: reduced ? "auto" : "smooth" }); }
  function live() { stickC = true; if (convoEl) convoEl.scrollTo({ top: convoEl.scrollHeight, behavior: reduced ? "auto" : "smooth" }); }
  async function flash(g) { flashG = new Set([g]); await wait(1200); if (flashG.has(g)) flashG = new Set(); }

  function reset() { shownN = 0; typed = ""; log = []; shaped = false; flashG = new Set(); hoverG = null; summLink = false; summ = "idle"; }
  function idleLog() {
    return gateway
      ? [{ kind: "req" }, { kind: "idle" }]
      : [{ kind: "req" }, { kind: "off" }];
  }

  // build the daemon log for the current gateway/mode (transcript untouched)
  async function applyShaping(my, animate) {
    summ = "idle"; summLink = false; shaped = false;
    if (!gateway) { log = idleLog(); return; }
    log = [{ kind: "req" }, { kind: "head" }];
    await follow(daemonEl, stickD); if (animate) { await wait(240); if (my !== gen) return; }
    for (const op of (OPS[mode] ?? OPS.default)) { // summarizer reuses the default passes
      log = [...log, { kind: "op", op }];
      if (op.g && animate) flash(op.g);
      await follow(daemonEl, stickD);
      if (animate) { await wait(340); await gate(); if (my !== gen) return; }
    }
    log = [...log, { kind: "retain" }, { kind: "fwd", sent: sentKb(mode) }, { kind: "edits" }];
    shaped = true;
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

  // full play — stream the transcript once; prune fires partway; summarizer at end
  async function run() {
    const my = ++gen; paused = false; resumeFn = null;
    reset(); ctx.set(7, { duration: 0 }); phase = "running"; log = idleLog();
    if (reduced) {
      shownN = EV.length;
      await applyShaping(my, false);
      ctx.set(gateway ? CTX_DONE[mode] ?? 53 : CTX_OFF, { duration: 0 });
      phase = "done"; await runSummarizer(my); return;
    }
    await wait(650); await gate(); if (my !== gen) return;
    for (let i = 0; i < EV.length; i++) {
      const e = EV[i];
      if (e.k === "user") { await wait(600); await gate(); if (my !== gen) return; for (let c = 0; c <= e.text.length; c++) { typed = e.text.slice(0, c); await wait(46); if (my !== gen) return; } await wait(360); typed = ""; }
      shownN = i + 1;
      if (e.ctx != null) ctx.set(gateway ? e.ctx : Math.max(e.ctx, CTX_OFF * (i / EV.length)), { duration: 700 });
      await follow(convoEl, stickC);
      if (i === OPS_AT) {
        await applyShaping(my, true); if (my !== gen) return;
        if (gateway) ctx.set(CTX_PRUNE, { duration: reduced ? 0 : 850 }); // the drop
      }
      await wait(e.k === "tool" ? 520 : 320); await gate(); if (my !== gen) return;
    }
    if (gateway) ctx.set(CTX_DONE[mode] ?? 53, { duration: reduced ? 0 : 700 });
    phase = "done";
    await runSummarizer(my);
  }

  // mode/gateway change after `done` → re-shape (no re-stream) + settle the bar
  function reshape() {
    const my = ++gen; paused = false; const r = resumeFn; resumeFn = null; if (r) r();
    applyShaping(my, true).then(() => {
      if (my !== gen) return;
      ctx.set(!gateway ? CTX_OFF : CTX_DONE[mode] ?? 53, { duration: reduced ? 0 : 700 });
      runSummarizer(my);
    });
  }
  function setMode(m) { if (m === mode || !gateway) return; mode = m; if (phase === "running" && !shaped) return; reshape(); }
  function toggleGateway() { gateway = !gateway; if (phase === "running" && !shaped) { run(); } else { reshape(); } }
  function pausePlay() {
    if (phase === "running" && !paused) paused = true;
    else if (paused) { paused = false; const r = resumeFn; resumeFn = null; if (r) r(); }
    else run();
  }

  function inview(node) {
    let was = false;
    const o = new IntersectionObserver((es) => { const v = es[0].isIntersecting; if (v && !was) { was = true; run(); } else if (!v) was = false; }, { threshold: 0.2 });
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
      <button type="button" class="gw-toggle" aria-label="Toggle the Trimwire gateway" aria-pressed={gateway} onclick={toggleGateway}><span class="led" class:on={gateway}></span><span class="gw-lbl">Trimwire {gateway ? "on" : "off"}</span></button>
    </div>

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

    <div class="panes">
      <!-- LEFT — Claude Code (pristine transcript + native statusline) -->
      <section class="pane pane-cc" aria-label="Claude Code session">
        <div class="cc-scroll" aria-hidden="true" bind:this={convoEl} onscroll={() => onScroll("c")}>
          <div class="boot">
            <pre class="mascot">{MASCOT}</pre>
            <div class="boot-meta"><div class="boot-h"><b>✻ agent session</b></div><div class="boot-d">~/todo-cli</div></div>
          </div>
          <ol class="convo">
            {#each EV as e, i (i)}
              {#if i < shownN}
                <li class="msg m-{e.k}" data-g={e.g ?? ""} class:m-old={e.old} class:flash={e.g && activeFlash(e.g)} class:summlit={summLink && summ !== "done" && e.old} class:summdone={summLink && summ === "done" && e.old} transition:fade={{ duration: reduced ? 0 : 170 }}>
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
        <div class="ccline" aria-hidden="true">
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
            <div class="lg lg-{l.kind}" class:linkable={l.kind === "op" && l.op.g} onmouseenter={() => { if (l.kind === "op" && l.op.g) { hoverG = new Set([l.op.g]); reveal(`[data-g="${l.op.g}"]`); } }} onmouseleave={() => { hoverG = null; if (phase === "done") live(); }} transition:fade={{ duration: reduced ? 0 : 150 }}>
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
  <span class="drag-hint dim" aria-hidden="true"><span class="dh-ar">⇆</span> drag across the scene</span>
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

  .tabsrow { display: flex; align-items: stretch; gap: 0.1rem; padding: 0 0.5rem; background: #0a0e0e; border-bottom: 1px solid #1d2625; flex-wrap: nowrap; overflow: hidden; }
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
  .ccline2 { padding: 0 0.9rem 0.4rem; font-size: 0.64rem; color: #7c8b89; }

  .tw-head { display: flex; align-items: center; gap: 0.4rem; padding: 0.4rem 0.6rem; border-bottom: 1px solid #141b1a; }
  .tw-name { font-size: var(--t-fs-sub); color: var(--ink-3); } .tw-head .grow { flex: 1; }
  .modesw { display: inline-flex; gap: 0.1rem; padding: 0.15rem; border: 1px solid var(--border-2); border-radius: 999px; background: var(--inset); }
  .modesw button { font-family: var(--mono); font-size: 0.66rem; color: var(--ink-3); background: transparent; border: 0; border-radius: 999px; padding: 0.22rem 0.5rem; cursor: pointer; transition: color 0.15s, background 0.15s; }
  .modesw button.on { color: #04100f; background: var(--accent); }
  .modesw button.m-summ.on { color: #0a0612; background: var(--c-summ); }
  .modesw.locked { opacity: 0.55; }
  .modesw button:disabled { cursor: default; color: var(--ink-3); }
  .modesw button:disabled.on { background: color-mix(in srgb, var(--accent) 45%, #243130); color: #0c1211; }
  .modesw button:disabled.m-summ.on { background: color-mix(in srgb, var(--c-summ) 45%, #243130); color: #0c1211; }
  @media (hover: hover) { .modesw button:not(.on):not(:disabled):hover { color: var(--ink); } }

  .daemon { height: clamp(15rem, 42vh, 22rem); overflow-y: auto; padding: 0.6rem 0.7rem; font-size: var(--t-fs-sub); line-height: 1.65; color: var(--ink-2); overflow-anchor: none; scrollbar-width: thin; scrollbar-color: #243130 transparent; }
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
    /* let the window itself pan horizontally so the gateway pane is reachable;
       keep vertical clipping + rounded corners intact */
    .termwin { min-width: 0; overflow-x: auto; overflow-y: hidden; scroll-snap-type: x proximity; -webkit-overflow-scrolling: touch; scrollbar-width: none; }
    .termwin::-webkit-scrollbar { display: none; }
    .panes { display: flex; }
    .pane-cc { flex: 0 0 82vw; scroll-snap-align: start; } .pane-tw { flex: 0 0 64vw; scroll-snap-align: end; }
    .cc-scroll, .daemon { height: clamp(13rem, 50vh, 19rem); }
    .drag-hint { display: inline-flex; }
  }
  @media (prefers-reduced-motion: reduce) { .cursor, .msg, .dh-ar { animation: none; transition: none; } }
</style>
