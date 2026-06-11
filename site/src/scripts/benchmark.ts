// Typed renderer for the community model-benchmark LEADERBOARD (used by
// src/components/BenchmarkTable.astro). Fetches the public aggregate JSON and draws
// a KPI strip + search + family chips + a dense, sortable, sticky-header table with
// in-cell bars and semantic good/bad colours. Before the collector is live (or if
// the fetch fails) it shows a real empty-state + opt-in CTA — never seeded sample
// data dressed up as real. Dependency-free; `mount()` is pure DOM (jsdom-testable).

export interface BenchmarkModel {
  model_family: string;
  model_size_bucket: string;
  contributors: number;
  avg_retention: number;
  avg_compression: number;
  false_done_rate: number;
  usable_pct: number;
}

export interface BenchmarkPayload {
  k?: number | null;
  corpus_version?: string;
  generated_at?: string | null;
  suppressed_groups?: number;
  models?: BenchmarkModel[];
}

/** Exported ONLY as a unit-test fixture for the populated-table rendering — never
 *  rendered in production (the live page shows real data or a real empty-state). */
export const EXAMPLE: BenchmarkPayload = {
  k: 5,
  corpus_version: "1",
  generated_at: null,
  suppressed_groups: 2,
  models: [
    { model_family: "qwen3.5", model_size_bucket: "3-4b", contributors: 21, avg_retention: 95, avg_compression: 52, false_done_rate: 0, usable_pct: 100 },
    { model_family: "qwen3.5", model_size_bucket: "5-9b", contributors: 12, avg_retention: 97, avg_compression: 55, false_done_rate: 0, usable_pct: 100 },
    { model_family: "llama3.1", model_size_bucket: "5-9b", contributors: 8, avg_retention: 88, avg_compression: 48, false_done_rate: 0, usable_pct: 100 },
    { model_family: "granite4.1", model_size_bucket: "5-9b", contributors: 6, avg_retention: 90, avg_compression: 60, false_done_rate: 33, usable_pct: 100 },
    { model_family: "gemma3", model_size_bucket: "3-4b", contributors: 5, avg_retention: 80, avg_compression: 58, false_done_rate: 20, usable_pct: 80 },
  ],
};

const num = (v: unknown): number => (typeof v === "number" && Number.isFinite(v) ? v : 0);
const label = (r: BenchmarkModel): string => `${r.model_family} · ${r.model_size_bucket}`;

/** FCS = retention × compression (each a 0..100 percent → /100), 0..100. */
export const fcs = (r: BenchmarkModel): number =>
  Math.round((num(r.avg_retention) / 100) * (num(r.avg_compression) / 100) * 100);

type Dir = "asc" | "desc";
type Cell = "model" | "bar" | "fcs" | "falsedone" | "num";

interface Col {
  key: string;
  label: string;
  get: (r: BenchmarkModel) => string | number;
  dir: Dir;
  cell: Cell;
  hint?: string;
}

// False-done is the disqualifying signal, so it sits right after the model name.
const COLS: Col[] = [
  { key: "model", label: "Model", get: label, dir: "asc", cell: "model" },
  { key: "false_done_rate", label: "False-done", get: (r) => num(r.false_done_rate), dir: "asc", cell: "falsedone", hint: "% of runs with an unsupported completion claim — non-zero is disqualifying" },
  { key: "avg_retention", label: "Retention", get: (r) => num(r.avg_retention), dir: "desc", cell: "bar", hint: "% of load-bearing facts kept" },
  { key: "avg_compression", label: "Compression", get: (r) => num(r.avg_compression), dir: "desc", cell: "bar", hint: "% the summary shrank the excerpt" },
  { key: "usable_pct", label: "Usable", get: (r) => num(r.usable_pct), dir: "desc", cell: "bar", hint: "% of runs that produced a usable summary" },
  { key: "fcs", label: "FCS", get: fcs, dir: "desc", cell: "fcs", hint: "Faithful-compression score = retention × compression (higher = better)" },
  { key: "contributors", label: "N", get: (r) => num(r.contributors), dir: "desc", cell: "num", hint: "contributors" },
];

interface State {
  rows: BenchmarkModel[];
  sortKey: string;
  sortDir: Dir;
  search: string;
  family: string; // "all" or a model_family
  k: number | null;
  corpus_version?: string;
  generated_at?: string | null;
  suppressed_groups?: number;
}

function visibleRows(state: State): BenchmarkModel[] {
  const q = state.search.trim().toLowerCase();
  const rows = state.rows.filter(
    (r) =>
      (state.family === "all" || r.model_family === state.family) &&
      (q === "" || label(r).toLowerCase().includes(q)),
  );
  const col = COLS.find((c) => c.key === state.sortKey) ?? COLS[5];
  const mul = state.sortDir === "asc" ? 1 : -1;
  return rows.sort((a, b) => {
    const va = col.get(a);
    const vb = col.get(b);
    if (typeof va === "string" || typeof vb === "string") return mul * String(va).localeCompare(String(vb));
    return mul * (va - vb);
  });
}

/** A KPI card. */
function kpi(value: string, lbl: string, sub?: string): HTMLDivElement {
  const c = document.createElement("div");
  c.className = "tw-kpi";
  c.innerHTML = "";
  const v = document.createElement("div");
  v.className = "tw-kpi-v";
  v.textContent = value;
  const l = document.createElement("div");
  l.className = "tw-kpi-l";
  l.textContent = lbl;
  // Always append the subtitle slot (empty when no sub) so every card has the
  // same content height — otherwise the grid sizes a subtitle-less card (MODELS,
  // CONTRIBUTORS) differently and it renders a different height from the rest.
  const s = document.createElement("div");
  s.className = "tw-kpi-s";
  s.textContent = sub ?? "";
  c.append(l, v, s);
  return c;
}

function renderKpi(el: HTMLElement, state: State): void {
  const rows = visibleRows(state);
  el.innerHTML = "";
  if (rows.length === 0) return;
  const best = (sel: (r: BenchmarkModel) => number) =>
    rows.reduce((m, r) => (sel(r) > sel(m) ? r : m), rows[0]);
  const bestF = best(fcs);
  const bestC = best((r) => num(r.avg_compression));
  const contribs = rows.reduce((a, r) => a + num(r.contributors), 0);
  el.append(
    kpi(String(rows.length), "models"),
    kpi(String(fcs(bestF)), "best FCS", label(bestF)),
    kpi(`${num(bestC.avg_compression)}%`, "best compression", label(bestC)),
    kpi(String(contribs), "contributors"),
  );
}

/** Semantic class for a metric value (good/warn/empty). */
function sem(key: string, v: number): string {
  if (key === "avg_retention") return v >= 90 ? "tw-good" : v < 70 ? "tw-warn" : "";
  if (key === "usable_pct") return v >= 100 ? "tw-good" : v < 80 ? "tw-warn" : "";
  return "";
}

function renderTable(host: HTMLElement, status: HTMLElement, state: State, rerender: () => void): void {
  host.innerHTML = "";
  const rows = visibleRows(state);

  if (rows.length === 0) {
    const p = document.createElement("p");
    p.className = "tw-banner";
    p.textContent =
      state.rows.length === 0
        ? `No models yet — every group is still below the k-anonymity threshold (k=${state.k ?? "?"}). The table fills in as more people run \`trimwire share benchmark\`.`
        : "No models match your search / filter.";
    host.append(p);
  } else {
    const heroFcs = Math.max(...rows.map(fcs));
    const table = document.createElement("table");
    table.className = "twb";

    const thead = document.createElement("thead");
    const htr = document.createElement("tr");
    for (const c of COLS) {
      const th = document.createElement("th");
      th.scope = "col";
      th.tabIndex = 0;
      th.className = "twb-th" + (c.key === "fcs" ? " twb-th-fcs" : "");
      th.textContent = c.label;
      if (c.hint) th.title = c.hint;
      if (c.key === state.sortKey) th.setAttribute("aria-sort", state.sortDir === "asc" ? "ascending" : "descending");
      else th.setAttribute("aria-sort", "none");
      const doSort = () => {
        if (state.sortKey === c.key) state.sortDir = state.sortDir === "asc" ? "desc" : "asc";
        else {
          state.sortKey = c.key;
          state.sortDir = c.dir;
        }
        rerender();
      };
      th.addEventListener("click", doSort);
      th.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          doSort();
        }
      });
      htr.append(th);
    }
    thead.append(htr);
    table.append(thead);

    const tbody = document.createElement("tbody");
    for (const r of rows) {
      const tr = document.createElement("tr");
      if (fcs(r) === heroFcs && num(r.false_done_rate) === 0) tr.dataset.hero = "true";
      for (const c of COLS) {
        const td = document.createElement("td");
        const v = c.get(r);
        if (c.cell === "model") {
          td.className = "twb-model";
          td.textContent = String(v);
        } else if (c.cell === "falsedone") {
          const n = num(v);
          td.className = "twb-num" + (n > 0 ? " tw-bad" : " tw-dim");
          if (n > 0) {
            const flag = document.createElement("span");
            flag.className = "twb-flag";
            flag.setAttribute("role", "img");
            flag.setAttribute("aria-label", "warning: non-zero false-done rate — disqualifying");
            flag.textContent = "⚑ ";
            td.append(flag);
          }
          td.append(document.createTextNode(`${n}%`));
        } else if (c.cell === "bar") {
          const n = num(v);
          td.className = "twb-bar twb-num " + sem(c.key, n);
          td.style.setProperty("--pct", String(Math.max(0, Math.min(100, n))));
          const span = document.createElement("span");
          span.className = "twb-bar-v";
          span.textContent = `${n}%`;
          td.append(span);
        } else if (c.cell === "fcs") {
          td.className = "twb-fcs twb-num";
          td.textContent = String(v);
        } else {
          td.className = "twb-num twb-n";
          td.textContent = String(v);
        }
        tr.append(td);
      }
      tbody.append(tr);
    }
    table.append(tbody);
    host.append(table);
  }

  // Footer / status line (real data only — there is no "preview" mode).
  status.classList.remove("tw-banner");
  status.textContent =
    (state.generated_at ? `Updated ${state.generated_at} · ` : "") +
    `${rows.length} of ${state.rows.length} model(s) · corpus v${state.corpus_version ?? "?"} · ` +
    `k=${state.k ?? "?"} · ${state.suppressed_groups ?? 0} hidden as too small. ` +
    "FCS = retention × compression (higher = better); Usable = % of runs that produced a usable summary; ⚑ = disqualifying false-done.";
}

/** Build the search box + family filter chips ONCE (so the search input keeps
 *  focus while typing). Returns a `syncChips` that re-styles chips from state. */
function buildControls(el: HTMLElement, state: State, rerender: () => void): () => void {
  el.innerHTML = "";
  const search = document.createElement("input");
  search.type = "search";
  search.className = "tw-search";
  search.placeholder = "Search models…";
  search.setAttribute("aria-label", "Search models");
  search.addEventListener("input", () => {
    state.search = search.value;
    rerender();
  });
  el.append(search);

  const families = ["all", ...[...new Set(state.rows.map((r) => r.model_family))].sort()];
  const chips: HTMLButtonElement[] = [];
  const chipRow = document.createElement("div");
  chipRow.className = "tw-chips";
  chipRow.setAttribute("role", "radiogroup");
  chipRow.setAttribute("aria-label", "Filter by model family");
  for (const fam of families) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "tw-chip";
    b.textContent = fam;
    b.setAttribute("role", "radio");
    b.dataset.family = fam;
    b.addEventListener("click", () => {
      state.family = fam;
      sync();
      rerender();
    });
    chips.push(b);
    chipRow.append(b);
  }
  el.append(chipRow);

  const sync = () => {
    for (const b of chips) {
      const active = b.dataset.family === state.family;
      b.classList.toggle("tw-chip-on", active);
      b.setAttribute("aria-checked", String(active));
    }
  };
  sync();
  return sync;
}

/** Render `data` into `root` (a `.tw-bench` element). Pure DOM — unit-testable. */
export function mount(root: HTMLElement, data: BenchmarkPayload): void {
  const kpiEl = root.querySelector<HTMLElement>(".tw-bench-kpi");
  const controlsEl = root.querySelector<HTMLElement>(".tw-bench-controls");
  const tableHost = root.querySelector<HTMLElement>(".tw-bench-table");
  const status = root.querySelector<HTMLElement>(".tw-bench-status");
  if (!kpiEl || !controlsEl || !tableHost || !status) return;

  const state: State = {
    rows: (data.models ?? []).slice(),
    sortKey: "fcs",
    sortDir: "desc",
    search: "",
    family: "all",
    k: data.k ?? null,
    corpus_version: data.corpus_version,
    generated_at: data.generated_at ?? null,
    suppressed_groups: data.suppressed_groups,
  };

  const rerender = () => {
    renderKpi(kpiEl, state);
    renderTable(tableHost, status, state, rerender);
  };
  buildControls(controlsEl, state, rerender);
  rerender();
}

/** Render an honest empty-state + opt-in CTA — used before the collector is
 *  published or when the fetch fails. No fake rows, no KPI strip. */
export function renderEmptyState(root: HTMLElement): void {
  const kpiEl = root.querySelector<HTMLElement>(".tw-bench-kpi");
  const controlsEl = root.querySelector<HTMLElement>(".tw-bench-controls");
  const tableHost = root.querySelector<HTMLElement>(".tw-bench-table");
  const status = root.querySelector<HTMLElement>(".tw-bench-status");
  if (!kpiEl || !controlsEl || !tableHost || !status) return;
  kpiEl.innerHTML = "";
  controlsEl.innerHTML = "";
  tableHost.innerHTML = "";

  const box = document.createElement("div");
  box.className = "tw-bench-empty";
  const title = document.createElement("p");
  title.className = "tw-empty-title";
  title.textContent = "No community results published yet.";
  const body = document.createElement("p");
  body.textContent =
    "This leaderboard fills in as people opt in. Contribute the first anonymous, content-free row from a local run:";
  const code = document.createElement("code");
  code.className = "tw-empty-cmd";
  code.textContent = "trimwire share benchmark";
  const link = document.createElement("a");
  link.href = "/guides/benchmark/";
  link.textContent = "How the benchmark works →";
  box.append(title, body, code, link);
  tableHost.append(box);

  status.classList.remove("tw-banner");
  status.textContent = "";
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

/** Fetch the feed on `root`'s `data-benchmark` attr. With no URL (collector not
 *  published) or on a failed/empty fetch, show the honest empty-state — NEVER
 *  seeded sample data.
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
  const url = root.dataset.benchmark;
  if (!url) {
    renderEmptyState(root);
    return;
  }
  fetch(url, { headers: { accept: "application/json" } })
    .then((res) => {
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      return res.json() as Promise<BenchmarkPayload>;
    })
    .then((d) => mount(root, d))
    .catch(() => renderEmptyState(root));
}
