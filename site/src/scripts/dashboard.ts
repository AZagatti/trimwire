// Typed renderer for the community stats dashboard (used by
// src/components/CommunityDashboard.astro). Fetches the public k-anon aggregate
// JSON and draws a KPI strip + filter controls + a dense sortable sticky-header
// TABLE (one row per cohort) with in-cell bars and semantic colours. Each row
// has an expandable detail section for per-strategy breakdown.
// Shows a real empty-state + opt-in CTA before the collector is live — never
// seeded sample data. Dependency-free; `mount()` is pure DOM (jsdom-testable).

export interface AggregateGroup {
  trimwire_version: string;
  /** Agent harness (grouping key). Optional for forward-compat with older feeds;
   *  defaults to "claude-code" when absent. Shown in the cohort label only when
   *  it isn't claude-code (today: always claude-code → hidden). */
  harness?: string;
  model_family: string;
  profile: string;
  summarizer_backend: string;
  conversation_length_bucket: string;
  contributors: number;
  avg_reduction_pct?: number;
  avg_cache_hit_pct?: number;
  avg_cache_stability?: number;
  strategy_share_avg?: Record<string, number>;
  strategy_fire_rate?: Record<string, number>;
  reprune_on_pct?: number;
  simhash_on_pct?: number;
  accumulator_on_pct?: number;
  avg_native_compaction_rate?: number;
  avg_strategy_any_fired_pct?: number;
  os_distribution?: Record<string, number> | null;
  avg_summarizer_trigger_rate?: number;
  summarizer_size_distribution?: Record<string, number> | null;
  summarizer_accept_rate_distribution?: Record<string, number> | null;
  // Part of the collector's grouping key; emitted on every group.
  summarizer_size_bucket?: string;
  // l-diversity-gated marginal histograms (null when the gate isn't met).
  reduction_distribution?: Record<string, number> | null;
  summarizer_distribution?: Record<string, number> | null;
  max_session_length_distribution?: Record<string, number> | null;
  summarizer_backend_won_distribution?: Record<string, number> | null;
}

export interface AggregatePayload {
  k?: number | null;
  generated_at?: string | null;
  suppressed_groups?: number;
  groups?: AggregateGroup[];
}

/** Exported ONLY as a unit-test fixture for the populated-table rendering — never
 *  rendered in production (the live page shows real data or a real empty-state). */
export const EXAMPLE: AggregatePayload = {
  k: 10,
  suppressed_groups: 3,
  groups: [
    { trimwire_version: "0.1", model_family: "claude-opus-4-8", profile: "default", summarizer_backend: "off", summarizer_size_bucket: "none", conversation_length_bucket: "50-200", contributors: 42, avg_reduction_pct: 55, avg_cache_hit_pct: 80, avg_cache_stability: 9.4, strategy_share_avg: { stale_input_cap: 28, thinking_strip: 24, cross_turn_dedup: 18, bloat_cap: 16, stale_reads: 9, sliding_window: 5 }, strategy_fire_rate: { cross_turn_dedup: 98, thinking_strip: 91, stale_reads: 74, bloat_cap: 69, stale_input_cap: 60, failed_input_purge: 33, sliding_window: 21, image_strip: 7 }, reprune_on_pct: 95, simhash_on_pct: 12, accumulator_on_pct: 0, avg_native_compaction_rate: 8, avg_strategy_any_fired_pct: 82, os_distribution: { linux: 26, macos: 14, windows: 2 } },
    { trimwire_version: "0.1", model_family: "claude-sonnet-4-6", profile: "gentle", summarizer_backend: "off", summarizer_size_bucket: "none", conversation_length_bucket: "10-50", contributors: 17, avg_reduction_pct: 30, avg_cache_hit_pct: 72, avg_cache_stability: 9.8, strategy_share_avg: { cross_turn_dedup: 40, bloat_cap: 35, failed_input_purge: 25 }, strategy_fire_rate: { cross_turn_dedup: 88, bloat_cap: 71, failed_input_purge: 41, thinking_strip: 35 }, reprune_on_pct: 88, simhash_on_pct: 6, accumulator_on_pct: 0, avg_native_compaction_rate: 14, avg_strategy_any_fired_pct: 64, os_distribution: null },
    { trimwire_version: "0.1", model_family: "claude-opus-4-8", profile: "default", summarizer_backend: "local", summarizer_size_bucket: "3-4b", conversation_length_bucket: ">200", contributors: 11, avg_reduction_pct: 60, avg_cache_hit_pct: 84, avg_cache_stability: 9.1, strategy_share_avg: { stale_input_cap: 30, thinking_strip: 26, bloat_cap: 20, cross_turn_dedup: 14 }, strategy_fire_rate: { cross_turn_dedup: 100, thinking_strip: 95, bloat_cap: 80, stale_input_cap: 64, stale_reads: 55, simhash_dedup: 18 }, reprune_on_pct: 100, simhash_on_pct: 18, accumulator_on_pct: 64, avg_native_compaction_rate: 6, avg_strategy_any_fired_pct: 88, os_distribution: null, avg_summarizer_trigger_rate: 40, summarizer_size_distribution: { "3-4b": 7, "5-9b": 4 }, summarizer_accept_rate_distribution: { "70": 5, "80": 4, "60": 2 }, summarizer_backend_won_distribution: { local: 8, api: 3 } },
  ],
};

const STRATEGY_LABELS: Record<string, string> = {
  failed_input_purge: "Failed-input purge",
  stale_input_cap: "Stale-input cap",
  cross_turn_dedup: "Cross-turn dedup",
  stale_reads: "Stale reads",
  simhash_dedup: "Simhash dedup (opt-in)",
  bloat_cap: "Bloat cap",
  sliding_window: "Sliding window",
  image_strip: "Image strip",
  thinking_strip: "Thinking strip",
};

const num = (v: unknown): number => (typeof v === "number" && Number.isFinite(v) ? v : 0);

/** A labelled horizontal bar (used in detail rows). Coerces junk to 0. */
export function bar(label: string, value: unknown, max: number, suffix?: string, className?: string): HTMLDivElement {
  const n = typeof value === "number" && Number.isFinite(value) ? value : 0;
  const denom = typeof max === "number" && max > 0 ? max : 100;
  const pct = Math.max(0, Math.min(100, (n / denom) * 100));
  const row = document.createElement("div");
  row.className = "twbar" + (className ? " " + className : "");
  row.setAttribute("role", "group");
  row.setAttribute("aria-label", label + ": " + n + (suffix ?? ""));
  const l = document.createElement("span");
  l.textContent = label;
  l.title = label;
  l.setAttribute("aria-hidden", "true");
  const track = document.createElement("div");
  track.className = "track";
  const fill = document.createElement("div");
  fill.className = "fill";
  fill.style.width = pct + "%";
  track.appendChild(fill);
  const v = document.createElement("span");
  v.className = "val";
  v.textContent = n + (suffix ?? "");
  v.setAttribute("aria-hidden", "true");
  row.append(l, track, v);
  return row;
}

// ── Table columns ────────────────────────────────────────────────────────────

type Dir = "asc" | "desc";

interface Col {
  key: string;
  label: string;
  get: (g: AggregateGroup) => string | number;
  defaultDir: Dir;
  /** rendering hint */
  cell: "cohort" | "bar" | "num" | "pct";
  hint?: string;
}

const COLS: Col[] = [
  {
    key: "cohort",
    label: "Cohort",
    get: (g) => `${g.harness && g.harness !== "claude-code" ? g.harness + " · " : ""}${g.model_family} · ${g.profile}${g.summarizer_backend && g.summarizer_backend !== "off" ? " · " + g.summarizer_backend : ""}`,
    defaultDir: "asc",
    cell: "cohort",
  },
  {
    key: "conversation_length_bucket",
    label: "Length",
    get: (g) => g.conversation_length_bucket,
    defaultDir: "asc",
    cell: "cohort",
    hint: "Conversation length bucket (turns)",
  },
  {
    key: "avg_reduction_pct",
    label: "Avg reduction",
    get: (g) => num(g.avg_reduction_pct),
    defaultDir: "desc",
    cell: "bar",
    hint: "Average % of context tokens removed per session",
  },
  {
    key: "avg_cache_stability",
    label: "Cache stability",
    get: (g) => num(g.avg_cache_stability),
    defaultDir: "desc",
    cell: "bar",
    hint: "How often pruning kept the prompt-cache prefix byte-stable (0–10)",
  },
  {
    key: "avg_strategy_any_fired_pct",
    label: "Strategy hit",
    get: (g) => num(g.avg_strategy_any_fired_pct),
    defaultDir: "desc",
    cell: "bar",
    hint: "% of sessions where at least one strategy fired",
  },
  {
    key: "avg_native_compaction_rate",
    label: "Native compact.",
    get: (g) => num(g.avg_native_compaction_rate),
    defaultDir: "asc",
    cell: "bar",
    hint: "% of sessions where Claude Code's own context_management fired (lower = better: trimwire prevented the hit)",
  },
  {
    key: "contributors",
    label: "N",
    get: (g) => num(g.contributors),
    defaultDir: "desc",
    cell: "num",
    hint: "Number of contributors in this cohort",
  },
];

// ── Semantic colour helpers ───────────────────────────────────────────────────

function semClass(key: string, v: number): string {
  if (key === "avg_reduction_pct") return v >= 45 ? "tw-good" : v < 20 ? "tw-warn" : "";
  if (key === "avg_cache_stability") return v >= 9 ? "tw-good" : v < 7 ? "tw-warn" : "";
  if (key === "avg_strategy_any_fired_pct") return v >= 70 ? "tw-good" : v < 40 ? "tw-warn" : "";
  if (key === "avg_native_compaction_rate") return v <= 10 ? "tw-good" : v > 25 ? "tw-warn" : "";
  return "";
}

// ── KPI strip ────────────────────────────────────────────────────────────────

function kpi(value: string, lbl: string, sub?: string): HTMLDivElement {
  const c = document.createElement("div");
  c.className = "tw-kpi";
  const l = document.createElement("div");
  l.className = "tw-kpi-l";
  l.textContent = lbl;
  const v = document.createElement("div");
  v.className = "tw-kpi-v";
  v.textContent = value;
  // Always append the subtitle slot (empty when no sub) so every card has the
  // same content height (a subtitle-less card otherwise renders a different
  // height in the grid).
  const s = document.createElement("div");
  s.className = "tw-kpi-s";
  s.textContent = sub ?? "";
  c.append(l, v, s);
  return c;
}

function renderKpi(el: HTMLElement, rows: AggregateGroup[]): void {
  el.innerHTML = "";
  if (rows.length === 0) return;
  const totalContribs = rows.reduce((a, g) => a + num(g.contributors), 0);
  const bestRed = rows.reduce((m, g) => (num(g.avg_reduction_pct) > num(m.avg_reduction_pct) ? g : m), rows[0]);
  const avgStab = rows.filter((g) => g.avg_cache_stability != null).reduce((a, g) => a + num(g.avg_cache_stability), 0) /
    (rows.filter((g) => g.avg_cache_stability != null).length || 1);
  el.append(
    kpi(String(rows.length), "cohorts"),
    kpi(`${num(bestRed.avg_reduction_pct)}%`, "best reduction", `${bestRed.model_family} · ${bestRed.profile}`),
    kpi(avgStab.toFixed(1) + "/10", "avg cache stability"),
    kpi(String(totalContribs), "contributors"),
  );
}

// ── Detail panel (per-strategy breakdown, toggled by the ▶ button) ──────────

function buildDetailPanel(g: AggregateGroup): HTMLTableCellElement {
  const td = document.createElement("td");
  td.colSpan = COLS.length + 1; // +1 for the expand button column
  td.className = "twd-detail";

  const inner = document.createElement("div");
  inner.className = "twd-inner";

  const pctOf = (count: number) => (g.contributors > 0 ? Math.round((count / g.contributors) * 100) : 0);

  // Strategy share
  const shares = g.strategy_share_avg ?? {};
  const shareNames = Object.keys(shares).sort((a, b) => (shares[b] ?? 0) - (shares[a] ?? 0));
  if (shareNames.length) {
    const shareSum = shareNames.reduce((acc, n) => acc + (shares[n] ?? 0), 0);
    const hdr = document.createElement("p");
    hdr.className = "twd-section";
    hdr.textContent = "Bytes saved (share %) by strategy" + (shareSum < 95 ? " — strategies <5% omitted" : "");
    inner.append(hdr);
    for (const n of shareNames) inner.append(bar(STRATEGY_LABELS[n] ?? n, shares[n], 100, "%"));
  }

  // Strategy fire rate
  const fire = g.strategy_fire_rate ?? {};
  const fireNames = Object.keys(fire).sort((a, b) => (fire[b] ?? 0) - (fire[a] ?? 0));
  if (fireNames.length) {
    const hdr = document.createElement("p");
    hdr.className = "twd-section";
    hdr.textContent = "Strategy fire rate (% of sessions)";
    inner.append(hdr);
    for (const n of fireNames) inner.append(bar(STRATEGY_LABELS[n] ?? n, fire[n], 100, "%", "twbar--fire"));
  }

  // Config & environment
  const hdr2 = document.createElement("p");
  hdr2.className = "twd-section";
  hdr2.textContent = "Config & environment";
  inner.append(hdr2);
  if (typeof g.reprune_on_pct === "number") inner.append(bar("Reprune enabled", g.reprune_on_pct, 100, "%"));
  if (typeof g.simhash_on_pct === "number") inner.append(bar("Simhash enabled", g.simhash_on_pct, 100, "%"));
  if (typeof g.accumulator_on_pct === "number") inner.append(bar("Accumulator enabled", g.accumulator_on_pct, 100, "%"));
  if (typeof g.avg_native_compaction_rate === "number") inner.append(bar("Native compaction rate", g.avg_native_compaction_rate, 100, "%"));
  if (typeof g.avg_strategy_any_fired_pct === "number") inner.append(bar("Any strategy fired", g.avg_strategy_any_fired_pct, 100, "%"));

  const os = g.os_distribution;
  if (os) {
    for (const n of Object.keys(os).sort((a, b) => (os[b] ?? 0) - (os[a] ?? 0)))
      inner.append(bar("OS: " + n, pctOf(os[n]), 100, "%"));
  }
  // l-diversity-gated marginal distributions (present only on large-enough cohorts).
  const numKeys = (d: Record<string, number>) =>
    Object.keys(d).sort((a, b) => parseFloat(a) - parseFloat(b));
  const rd = g.reduction_distribution;
  if (rd) for (const n of numKeys(rd)) inner.append(bar("Reduction " + n + "%", pctOf(rd[n]), 100, "%"));
  const ml = g.max_session_length_distribution;
  if (ml)
    for (const n of Object.keys(ml).sort((a, b) => (ml[b] ?? 0) - (ml[a] ?? 0)))
      inner.append(bar("Max length " + n, pctOf(ml[n]), 100, "%"));

  // Summarizer (local + cloud API)
  const sz = g.summarizer_size_distribution;
  const ar = g.summarizer_accept_rate_distribution;
  const won = g.summarizer_backend_won_distribution;
  const sd = g.summarizer_distribution;
  if ((g.avg_summarizer_trigger_rate ?? 0) > 0 || sz || ar || won || sd) {
    const hdr3 = document.createElement("p");
    hdr3.className = "twd-section";
    hdr3.textContent = "Summarizer";
    inner.append(hdr3);
    if (sd) for (const n of Object.keys(sd).sort((a, b) => (sd[b] ?? 0) - (sd[a] ?? 0))) inner.append(bar("Backend: " + n, pctOf(sd[n]), 100, "%"));
    if (typeof g.avg_summarizer_trigger_rate === "number") inner.append(bar("Trigger rate", g.avg_summarizer_trigger_rate, 100, "%"));
    if (won) for (const n of Object.keys(won).sort((a, b) => (won[b] ?? 0) - (won[a] ?? 0))) inner.append(bar("Won: " + n, pctOf(won[n]), 100, "%"));
    if (sz) for (const n of Object.keys(sz).sort((a, b) => (sz[b] ?? 0) - (sz[a] ?? 0))) inner.append(bar("Model " + n, pctOf(sz[n]), 100, "%"));
    if (ar) for (const n of Object.keys(ar).sort((a, b) => (ar[b] ?? 0) - (ar[a] ?? 0))) inner.append(bar("Accept rate " + (n === "none" ? "n/a" : n + "%"), pctOf(ar[n]), 100, "%"));
  }

  td.append(inner);
  return td;
}

// ── Table renderer ───────────────────────────────────────────────────────────

interface State {
  groups: AggregateGroup[];
  filtered: AggregateGroup[];
  sortKey: string;
  sortDir: Dir;
  k: number | null;
  generated_at: string | null;
  suppressed_groups: number;
}

function sortedRows(rows: AggregateGroup[], key: string, dir: Dir): AggregateGroup[] {
  const col = COLS.find((c) => c.key === key) ?? COLS[2];
  const mul = dir === "asc" ? 1 : -1;
  return rows.slice().sort((a, b) => {
    const va = col.get(a);
    const vb = col.get(b);
    if (typeof va === "string" || typeof vb === "string") return mul * String(va).localeCompare(String(vb));
    return mul * (va - vb);
  });
}

function renderTable(host: HTMLElement, statusEl: HTMLElement, state: State, rerender: () => void): void {
  host.innerHTML = "";
  const rows = sortedRows(state.filtered, state.sortKey, state.sortDir);

  if (rows.length === 0) {
    const p = document.createElement("p");
    p.className = "tw-banner";
    p.textContent =
      state.groups.length === 0
        ? `No groups yet — every bucket is still below the k-anonymity threshold (k=${state.k ?? "?"}). The dashboard fills in as more people opt in.`
        : "No cohorts match the selected filters.";
    host.append(p);
  } else {
    const table = document.createElement("table");
    table.className = "twd";

    // thead
    const thead = document.createElement("thead");
    const htr = document.createElement("tr");
    // expand-toggle column header (empty)
    const thToggle = document.createElement("th");
    thToggle.scope = "col";
    thToggle.className = "twd-th twd-th-toggle";
    thToggle.setAttribute("aria-label", "Expand row");
    htr.append(thToggle);
    for (const c of COLS) {
      const th = document.createElement("th");
      th.scope = "col";
      th.tabIndex = 0;
      th.className = "twd-th" + (c.key === "cohort" ? " twd-th-first" : "");
      th.textContent = c.label;
      if (c.hint) th.title = c.hint;
      if (c.key === state.sortKey) th.setAttribute("aria-sort", state.sortDir === "asc" ? "ascending" : "descending");
      else th.setAttribute("aria-sort", "none");
      const doSort = () => {
        if (state.sortKey === c.key) state.sortDir = state.sortDir === "asc" ? "desc" : "asc";
        else { state.sortKey = c.key; state.sortDir = c.defaultDir; }
        rerender();
      };
      th.addEventListener("click", doSort);
      th.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); doSort(); } });
      htr.append(th);
    }
    thead.append(htr);
    table.append(thead);

    const tbody = document.createElement("tbody");
    for (const g of rows) {
      // Main row
      const tr = document.createElement("tr");
      tr.className = "twd-row";

      // Expand toggle cell
      const toggleTd = document.createElement("td");
      toggleTd.className = "twd-toggle-cell";
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "twd-toggle";
      btn.setAttribute("aria-label", `Expand details for ${g.model_family} · ${g.profile}`);
      btn.setAttribute("aria-expanded", "false");
      btn.textContent = "▶";
      toggleTd.append(btn);
      tr.append(toggleTd);

      // Data cells
      for (const c of COLS) {
        const td = document.createElement("td");
        const v = c.get(g);
        if (c.cell === "cohort") {
          td.className = "twd-cohort";
          td.textContent = String(v);
          if (c.key === "cohort") td.title = "Segment: model family · profile" + (g.summarizer_backend && g.summarizer_backend !== "off" ? " · summarizer backend" : "") + ` (${String(v)})`;
        } else if (c.cell === "bar") {
          const n = num(v);
          const sem = semClass(c.key, n);
          td.className = "twd-bar twd-num" + (sem ? " " + sem : "");
          // Use max reduction as 100 for the bar width when it's avg_reduction_pct for visual range
          const barMax = c.key === "avg_cache_stability" ? 10 : 100;
          const pct = Math.max(0, Math.min(100, (n / barMax) * 100));
          td.style.setProperty("--pct", String(pct));
          const span = document.createElement("span");
          span.className = "twd-bar-v";
          span.textContent = c.key === "avg_cache_stability" ? `${n}/10` : `${n}%`;
          td.append(span);
        } else if (c.cell === "num") {
          td.className = "twd-num twd-n";
          td.textContent = String(v);
        } else {
          td.className = "twd-num";
          td.textContent = String(v);
        }
        tr.append(td);
      }
      tbody.append(tr);

      // Detail row (hidden by default)
      const detailTr = document.createElement("tr");
      detailTr.className = "twd-detail-row";
      detailTr.hidden = true;
      detailTr.setAttribute("aria-hidden", "true");
      detailTr.append(buildDetailPanel(g));
      tbody.append(detailTr);

      // Wire up toggle
      btn.addEventListener("click", () => {
        const expanded = btn.getAttribute("aria-expanded") === "true";
        btn.setAttribute("aria-expanded", String(!expanded));
        btn.textContent = expanded ? "▶" : "▼";
        btn.classList.toggle("twd-toggle-open", !expanded);
        detailTr.hidden = expanded;
        detailTr.setAttribute("aria-hidden", String(expanded));
      });
    }
    table.append(tbody);
    host.append(table);
  }

  // Status line
  statusEl.classList.remove("tw-banner");
  statusEl.textContent =
    (state.generated_at ? `Updated ${state.generated_at} · ` : "") +
    `${rows.length} of ${state.groups.length} cohort(s) · k=${state.k ?? "?"} · ` +
    `${state.suppressed_groups} hidden as too small.`;
}

// ── Filter controls ───────────────────────────────────────────────────────────

const FILTER_FIELDS: [keyof AggregateGroup, string][] = [
  ["model_family", "Model"],
  ["profile", "Profile"],
  ["summarizer_backend", "Summarizer"],
  ["conversation_length_bucket", "Length"],
];

function buildControls(
  filtersEl: HTMLElement,
  state: State,
  onFilterChange: (filtered: AggregateGroup[]) => void,
): void {
  filtersEl.innerHTML = "";
  const selects: HTMLSelectElement[] = [];

  const applyFilters = () => {
    const active = selects
      .map((s) => [s.dataset.field, s.value] as const)
      .filter(([, v]) => v !== "*");
    onFilterChange(
      state.groups.filter((g) => active.every(([f, v]) => f && String(g[f as keyof AggregateGroup]) === v)),
    );
  };

  for (const [field, label] of FILTER_FIELDS) {
    const values = [...new Set(state.groups.map((g) => g[field]))].sort();
    if (values.length <= 1) continue;
    const wrap = document.createElement("label");
    wrap.textContent = label;
    const sel = document.createElement("select");
    sel.dataset.field = field;
    sel.setAttribute("aria-label", `Filter by ${label}`);
    const all = document.createElement("option");
    all.value = "*";
    all.textContent = "all";
    sel.append(all);
    for (const v of values) {
      const o = document.createElement("option");
      o.value = String(v);
      o.textContent = String(v);
      sel.append(o);
    }
    sel.addEventListener("change", applyFilters);
    selects.push(sel);
    wrap.append(sel);
    filtersEl.append(wrap);
  }
  filtersEl.hidden = filtersEl.children.length === 0;
}

// ── Public API ────────────────────────────────────────────────────────────────

/** Show/hide the page's "Reading the table" guidance (a `.tw-reading-help` block
 *  authored in the .mdx). It only makes sense when a table is actually rendered,
 *  so the empty states hide it. No-op in tests / when the block isn't present. */
function setReadingHelpVisible(visible: boolean): void {
  if (typeof document === "undefined") return;
  document
    .querySelectorAll<HTMLElement>(".tw-reading-help")
    .forEach((el) => (el.hidden = !visible));
}

/** Render `data` into `root` (a `.tw-dash` element). Pure DOM — unit-testable. */
export function mount(root: HTMLElement, data: AggregatePayload): void {
  const kpiEl = root.querySelector<HTMLElement>(".tw-dash-kpi");
  const filtersEl = root.querySelector<HTMLElement>(".tw-dash-filters");
  const tableHost = root.querySelector<HTMLElement>(".tw-dash-table");
  const statusEl = root.querySelector<HTMLElement>(".tw-dash-status");
  if (!kpiEl || !filtersEl || !tableHost || !statusEl) return;

  const state: State = {
    groups: data.groups ?? [],
    filtered: data.groups ?? [],
    sortKey: "avg_reduction_pct",
    sortDir: "desc",
    k: data.k ?? null,
    generated_at: data.generated_at ?? null,
    suppressed_groups: data.suppressed_groups ?? 0,
  };

  if (state.groups.length === 0) {
    filtersEl.hidden = true;
    kpiEl.innerHTML = "";
    tableHost.innerHTML = "";
    setReadingHelpVisible(false);
    statusEl.classList.add("tw-banner");
    statusEl.textContent =
      `No groups yet — every bucket is still below the k-anonymity threshold (k=${data.k ?? "?"}). The dashboard fills in as more people opt in.`;
    return;
  }
  setReadingHelpVisible(true);

  const rerender = () => {
    renderKpi(kpiEl, state.filtered);
    renderTable(tableHost, statusEl, state, rerender);
  };

  buildControls(filtersEl, state, (filtered) => {
    state.filtered = filtered;
    rerender();
  });

  rerender();
}

/** Render an honest empty-state + opt-in CTA — used before the collector is
 *  published or when the fetch fails. No fake cards, no preview banner. */
export function renderEmptyState(root: HTMLElement): void {
  const kpiEl = root.querySelector<HTMLElement>(".tw-dash-kpi");
  const filtersEl = root.querySelector<HTMLElement>(".tw-dash-filters");
  const tableHost = root.querySelector<HTMLElement>(".tw-dash-table");
  const statusEl = root.querySelector<HTMLElement>(".tw-dash-status");
  if (!kpiEl || !filtersEl || !tableHost || !statusEl) return;
  if (kpiEl) kpiEl.innerHTML = "";
  filtersEl.hidden = true;
  tableHost.innerHTML = "";
  setReadingHelpVisible(false);

  const box = document.createElement("div");
  box.className = "tw-dash-empty";
  const title = document.createElement("p");
  title.className = "tw-empty-title";
  title.textContent = "No community stats published yet.";
  const body = document.createElement("p");
  body.textContent =
    "This dashboard fills in as people opt in. Contribute the first anonymous, content-free snapshot from a local run:";
  const code = document.createElement("code");
  code.className = "tw-empty-cmd";
  code.textContent = "trimwire share stats";
  const link = document.createElement("a");
  link.href = "/guides/telemetry/";
  link.textContent = "How opt-in telemetry works →";
  const demoLink = document.createElement("a");
  demoLink.href = "?demo";
  demoLink.textContent = "Preview with demo data →";
  demoLink.style.cssText = "font-size:0.85rem;color:var(--sl-color-accent);";
  box.append(title, body, code, link, demoLink);
  tableHost.append(box);

  statusEl.classList.remove("tw-banner");
  statusEl.textContent = "";
}

/** Render a visible "Demo data" badge on `root` so demo mode is never mistaken
 *  for real community data. Inserted once; safe to call from init(). */
function mountDemoBadge(root: HTMLElement): void {
  if (root.querySelector(".tw-demo-badge")) return;
  const badge = document.createElement("p");
  badge.className = "tw-demo-badge";
  badge.setAttribute("role", "note");
  badge.textContent = "Demo data — not real community results";
  root.insertBefore(badge, root.firstChild);
}

/** Fetch the feed on `root`'s `data-aggregates` attr. With no URL (collector not
 *  published) or on a failed fetch, show the honest empty-state — NEVER seeded
 *  sample data.
 *
 *  DEV/preview affordance: when the page URL contains `?demo` (or `?preview`),
 *  mount(EXAMPLE) is called instead of fetching, and a visible "Demo data" badge
 *  is shown so the populated design can be evaluated without shipping fake data
 *  to real visitors. The default no-param behaviour is completely unchanged. */
export function init(root: HTMLElement): void {
  const params = new URLSearchParams(typeof window !== "undefined" ? window.location.search : "");
  if (params.has("demo") || params.has("preview")) {
    mountDemoBadge(root);
    mount(root, EXAMPLE);
    return;
  }
  const url = root.dataset.aggregates;
  if (!url) {
    renderEmptyState(root);
    return;
  }
  fetch(url, { headers: { accept: "application/json" } })
    .then((res) => {
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      return res.json() as Promise<AggregatePayload>;
    })
    .then((data) => mount(root, data))
    .catch(() => renderEmptyState(root));
}
