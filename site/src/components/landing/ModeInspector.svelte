<script>
  /**
   * v14.1 below-fold — ONE stable inspector that changes state across
   * default · gentle · summarizer. Plain-language behavior first; the internal
   * strategy name is a small detail on expand. Switching modes deactivates rows
   * and updates the reduction — the layout never swaps components. Summarizer is
   * the same inspector with the "older turns" row folding into a real summary.
   * Copy is for a human visitor, not someone who knows Trimwire internals.
   */
  import { Tween, prefersReducedMotion } from "svelte/motion";
  import { cubicOut } from "svelte/easing";
  import { slide, fade } from "svelte/transition";

  // Each row: plain behavior first; `tech` (the real strategy id) is secondary.
  // `on` lists which modes the pass runs in.
  const ROWS = [
    { plain: "Recent work & edits", verb: "kept raw", detail: "Your current task, every file edit, and the latest turns are always sent untouched.", tech: "authoring + recent window", on: ["default", "gentle", "summ"], always: true },
    { plain: "Repeated tool output", verb: "collapsed", detail: "When the same command runs twice, only the latest result is sent.", tech: "cross_turn_dedup", on: ["default", "gentle", "summ"] },
    { plain: "A file you later changed", verb: "removed", detail: "A file you read earlier and then edited is removed — the newer version is what counts.", tech: "stale_reads", on: ["default", "summ"] },
    { plain: "Oversized tool output", verb: "trimmed", detail: "A huge result — like an install log — is trimmed to its head and tail.", tech: "bloat_cap", on: ["default", "gentle", "summ"] },
    { plain: "Old search output", verb: "collapsed", detail: "Old, re-runnable output like searches or screenshots, once it's no longer recent.", tech: "sliding_window", on: ["default", "summ"] },
    { plain: "Solved-step reasoning", verb: "removed", detail: "Reasoning notes from steps the agent already finished.", tech: "thinking_strip", on: ["default", "gentle", "summ"] },
  ];
  const REDUCE = { default: 65, gentle: 40, summ: 78 };
  const LEDE = {
    default: "Trimwire keeps your recent work and edits intact, then removes repeated or heavy tool output before the request is sent.",
    gentle: "A lighter setting — only the safest, most certain trims run. Less reduction, more caution.",
    summ: "On a long session, the older turns become a short, useful summary. Your recent work stays intact.",
  };

  let mode = $state("default");
  let openIdx = $state(-1);
  const reduced = $derived(prefersReducedMotion.current);
  const pctT = new Tween(65, { duration: 500, easing: cubicOut });
  const pctN = $derived(Math.round(pctT.current));
  function setMode(m) { mode = m; openIdx = -1; pctT.set(REDUCE[m], { duration: reduced ? 0 : 500 }); }
  const active = (r) => r.on.includes(mode);
</script>

<div class="inspector">
  <div class="ins-head">
    <div class="ins-titlewrap">
      <p class="eyebrow">what trimwire does</p>
      <h2 class="ins-title">It keeps what matters, trims the rest.</h2>
    </div>
    <div class="modesw" role="tablist" aria-label="mode">
      <button type="button" role="tab" class:on={mode === "default"} aria-selected={mode === "default"} onclick={() => setMode("default")}>Default</button>
      <button type="button" role="tab" class:on={mode === "gentle"} aria-selected={mode === "gentle"} onclick={() => setMode("gentle")}>Gentle</button>
      <button type="button" role="tab" class:on={mode === "summ"} aria-selected={mode === "summ"} onclick={() => setMode("summ")}>Summarizer</button>
    </div>
  </div>

  <p class="ins-lede">
    {#key mode}<span class="lede-txt" transition:fade={{ duration: reduced ? 0 : 240 }}>{LEDE[mode]}</span>{/key}
  </p>

  <ul class="rows">
    {#each ROWS as r, i (r.tech)}
      <li class="row" class:off={!active(r)}>
        <button type="button" class="row-btn" aria-expanded={openIdx === i} onclick={() => (openIdx = openIdx === i ? -1 : i)}>
          <span class="dot" class:keep={r.always}></span>
          <span class="r-plain">{r.plain}</span>
          <span class="grow"></span>
          <span class="r-state">{active(r) ? (r.always ? "kept raw" : r.verb) : "skipped"}</span>
          <span class="chev" aria-hidden="true">{openIdx === i ? "−" : "+"}</span>
        </button>
        {#if openIdx === i}
          <div class="r-detail" transition:slide={{ duration: reduced ? 0 : 200 }}>
            <p>{r.detail}</p>
            <span class="tech">{r.tech}</span>
          </div>
        {/if}
      </li>
    {/each}

    <!-- the "older turns" row — stable in every mode; folds into a summary under summarizer -->
    <li class="row row-memory" class:summon={mode === "summ"}>
      <div class="row-btn static">
        <span class="dot" class:summ={mode === "summ"}></span>
        <span class="r-plain">Older turns</span>
        <span class="grow"></span>
        <span class="r-state">{mode === "summ" ? "→ summary" : "kept raw"}</span>
      </div>
      <div class="mem-body" aria-hidden={mode !== "summ"}>
        {#if mode === "summ"}
          <p class="mem-sum" transition:slide={{ duration: reduced ? 0 : 220 }}>“Migrated storage to sqlite; Store trait in app/storage.ts + a migration; JSON export reads from sqlite; tests green (41).”</p>
        {/if}
      </div>
    </li>
  </ul>

  <div class="ins-foot">
    <div class="foot-metric">
      <span class="fm-num" class:summ={mode === "summ"}>≈{pctN}%</span>
      <span class="fm-lbl">smaller request <span class="dim">· illustrative session</span></span>
    </div>
    <p class="foot-note">
      {#if mode === "summ"}
        Optional, off by default. Summaries are written by a small local model (<code>qwen3.5:4b</code>) or one you choose, only on long sessions, and never block the request.
      {:else}
        Runs locally on every request — no model call, and your transcript is never changed.
      {/if}
    </p>
    <p class="foot-link"><a href="/performance/">how it's measured →</a></p>
  </div>
</div>

<style>
  .inspector { border: 1px solid var(--border); border-radius: var(--radius); background: var(--card); overflow: hidden; }
  .ins-head { display: flex; flex-wrap: wrap; align-items: flex-end; justify-content: space-between; gap: 0.8rem; padding: 1.1rem 1.2rem 0.9rem; border-bottom: 1px solid var(--border); }
  .eyebrow { font-family: var(--mono); font-size: 0.7rem; letter-spacing: 0.14em; text-transform: uppercase; color: var(--ink-3); margin: 0 0 0.3rem; }
  .ins-title { margin: 0; font-size: clamp(1.15rem, 2.5vw, 1.5rem); font-weight: 640; letter-spacing: -0.02em; }
  .modesw { display: inline-flex; gap: 0.15rem; padding: 0.2rem; border: 1px solid var(--border-2); border-radius: 999px; background: var(--inset); flex-shrink: 0; }
  .modesw button { font-family: var(--mono); font-size: 0.74rem; color: var(--ink-2); background: transparent; border: 0; border-radius: 999px; padding: 0.4rem 0.8rem; min-height: 38px; cursor: pointer; transition: color 0.15s, background 0.15s; }
  .modesw button.on { color: #04100f; background: var(--accent); }
  .modesw button:last-child.on { color: #0a0612; background: var(--c-summ); }
  @media (hover: hover) { .modesw button:not(.on):hover { color: var(--accent-hi); } }

  /* fixed-height lede so mode changes don't jump the layout */
  .ins-lede { position: relative; margin: 0; padding: 0.9rem 1.2rem; color: var(--ink-2); font-size: 0.92rem; line-height: 1.5; min-height: 3.4rem; max-width: 68ch; }
  /* light cross-fade of the explanation text between modes (no layout shift) */
  .lede-txt { position: absolute; left: 1.2rem; right: 1.2rem; top: 0.9rem; }

  .rows { list-style: none; margin: 0; padding: 0 0.7rem 0.5rem; }
  .row { border-bottom: 1px solid color-mix(in srgb, var(--border) 55%, transparent); transition: opacity 0.25s ease; }
  .row.off { opacity: 0.4; }
  .row-btn { width: 100%; display: flex; align-items: center; gap: 0.6rem; padding: 0.6rem 0.5rem; background: transparent; border: 0; cursor: pointer; font-family: inherit; font-size: 0.9rem; color: var(--ink); text-align: left; }
  .row-btn.static { cursor: default; }
  .dot { width: 7px; height: 7px; border-radius: 999px; background: var(--accent); flex-shrink: 0; }
  .dot.keep { background: var(--ok); } .dot.summ { background: var(--c-summ); }
  .row.off .dot { background: var(--ink-3); }
  .r-plain { font-weight: 500; } .grow { flex: 1; }
  .r-state { font-family: var(--mono); font-size: 0.74rem; color: var(--ink-3); white-space: nowrap; }
  .row:not(.off) .r-state { color: var(--accent-hi); }
  .row.off .r-state { color: var(--ink-3); }
  .row-memory .r-state { color: var(--ink-3); } .row-memory.summon .r-state { color: var(--c-summ); }
  .chev { font-family: var(--mono); color: var(--ink-3); width: 1rem; text-align: center; }
  .r-detail { padding: 0 0.5rem 0.7rem 1.6rem; }
  .r-detail p { margin: 0 0 0.4rem; font-size: 0.84rem; color: var(--ink-2); line-height: 1.5; }
  .tech { font-family: var(--mono); font-size: 0.7rem; color: var(--ink-3); border: 1px solid var(--border-2); border-radius: 999px; padding: 0.08rem 0.5rem; }

  .row-memory { border-bottom: 0; }
  .mem-body { } .mem-sum { margin: 0 0.5rem 0.6rem 1.6rem; padding: 0.5rem 0.6rem; border-left: 2px solid color-mix(in srgb, var(--c-summ) 50%, transparent); background: color-mix(in srgb, var(--c-summ) 7%, transparent); color: var(--ink); font-size: 0.84rem; line-height: 1.5; }

  .ins-foot { display: grid; grid-template-columns: auto 1fr; gap: 0.3rem 1.1rem; align-items: center; padding: 0.9rem 1.2rem 1.1rem; border-top: 1px solid var(--border); background: color-mix(in srgb, var(--accent) 2%, var(--card)); }
  .foot-metric { display: flex; align-items: baseline; gap: 0.4rem; }
  .fm-num { font-family: var(--mono); font-size: 1.5rem; font-weight: 700; color: var(--accent-hi); font-variant-numeric: tabular-nums; transition: color 0.3s ease; }
  .fm-num.summ { color: var(--c-summ); }
  .fm-lbl { font-size: 0.8rem; color: var(--ink-2); } .fm-lbl .dim { color: var(--ink-3); }
  .foot-note { grid-column: 2; margin: 0; font-size: 0.8rem; color: var(--ink-3); line-height: 1.45; }
  .foot-note code { font-family: var(--mono); font-size: 0.85em; color: var(--accent-hi); background: var(--inset); padding: 0.02em 0.32em; border-radius: 4px; }
  .foot-link { grid-column: 1 / -1; margin: 0.2rem 0 0; font-family: var(--mono); font-size: 0.74rem; }
  .foot-link a { color: var(--ink-2); } .foot-link a:hover { color: var(--accent-hi); }

  @media (max-width: 40rem) {
    .ins-head { flex-direction: column; align-items: stretch; } .modesw { width: 100%; } .modesw button { flex: 1; padding: 0.4rem 0.4rem; }
    .ins-lede { min-height: 4.6rem; }
    .ins-foot { grid-template-columns: 1fr; } .foot-note { grid-column: 1; }
  }
</style>
