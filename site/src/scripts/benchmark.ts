// Typed renderer for the community model-benchmark LEADERBOARD (used by
// src/components/BenchmarkTable.astro). Fetches the public aggregate JSON and draws
// a KPI strip + search + family chips + a dense, sortable, sticky-header table with
// in-cell bars and semantic good/bad colours. Before the collector is live (or if
// the fetch fails) it shows a real empty-state + opt-in CTA — never seeded sample
// data dressed up as real. Dependency-free; `mount()` is pure DOM (jsdom-testable).

export interface BenchmarkModel {
  /** "local" | "api" — local and api rows are filtered/labeled separately. */
  backend: string;
  /** Coarse provider route for api rows ("none" for local). */
  provider_route: string;
  /** Broad family. */
  model_family: string;
  /** Public coarse model id — the display label. */
  model_bucket: string;
  model_size_bucket: string;
  /** "full_corpus" | "partial_corpus". */
  benchmark_scope: string;
  contributors: number;
  avg_retention: number;
  avg_compression: number;
  false_done_rate: number;
  usable_pct: number;
  /** % of runs with a provider/model call failure (api reliability, not model quality). */
  failed_rate: number;
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
    { backend: "local", provider_route: "none", model_family: "qwen3.5", model_bucket: "qwen3.5", model_size_bucket: "3-4b", benchmark_scope: "full_corpus", contributors: 21, avg_retention: 95, avg_compression: 52, false_done_rate: 0, usable_pct: 100, failed_rate: 0 },
    { backend: "local", provider_route: "none", model_family: "qwen3.5", model_bucket: "qwen3.5", model_size_bucket: "5-9b", benchmark_scope: "full_corpus", contributors: 12, avg_retention: 97, avg_compression: 55, false_done_rate: 0, usable_pct: 100, failed_rate: 0 },
    { backend: "local", provider_route: "none", model_family: "llama3.1", model_bucket: "llama3.1", model_size_bucket: "5-9b", benchmark_scope: "full_corpus", contributors: 8, avg_retention: 88, avg_compression: 48, false_done_rate: 0, usable_pct: 100, failed_rate: 0 },
    { backend: "api", provider_route: "anthropic", model_family: "claude-haiku", model_bucket: "claude-haiku-4-5", model_size_bucket: "api", benchmark_scope: "full_corpus", contributors: 9, avg_retention: 96, avg_compression: 58, false_done_rate: 0, usable_pct: 100, failed_rate: 0 },
    { backend: "api", provider_route: "openrouter", model_family: "gpt", model_bucket: "gpt-4.1-mini", model_size_bucket: "api", benchmark_scope: "partial_corpus", contributors: 6, avg_retention: 90, avg_compression: 60, false_done_rate: 0, usable_pct: 100, failed_rate: 17 },
    { backend: "local", provider_route: "none", model_family: "gemma3", model_bucket: "gemma3", model_size_bucket: "3-4b", benchmark_scope: "full_corpus", contributors: 5, avg_retention: 80, avg_compression: 58, false_done_rate: 20, usable_pct: 80, failed_rate: 0 },
  ],
};

const num = (v: unknown): number => (typeof v === "number" && Number.isFinite(v) ? v : 0);
/** Display label: the public model bucket, + size tier for local models. */
const label = (r: BenchmarkModel): string =>
  r.backend === "api" ? r.model_bucket : `${r.model_bucket} · ${r.model_size_bucket}`;
/** Backend cell text: "local" or "api · <route>". */
const backendLabel = (r: BenchmarkModel): string =>
  r.backend === "api" ? `api · ${r.provider_route}` : "local";

/** FCS = retention × compression (each a 0..100 percent → /100), 0..100. */
export const fcs = (r: BenchmarkModel): number =>
  Math.round((num(r.avg_retention) / 100) * (num(r.avg_compression) / 100) * 100);

type Dir = "asc" | "desc";
type Cell = "model" | "backend" | "bar" | "fcs" | "falsedone" | "num";

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
  { key: "backend", label: "Backend", get: backendLabel, dir: "asc", cell: "backend", hint: "local (ollama) vs api (cloud provider + route). API and local scores are NOT directly comparable." },
  { key: "false_done_rate", label: "False-done", get: (r) => num(r.false_done_rate), dir: "asc", cell: "falsedone", hint: "% of runs with an unsupported completion claim — non-zero is disqualifying" },
  { key: "avg_retention", label: "Retention", get: (r) => num(r.avg_retention), dir: "desc", cell: "bar", hint: "% of load-bearing facts kept" },
  { key: "avg_compression", label: "Compression", get: (r) => num(r.avg_compression), dir: "desc", cell: "bar", hint: "% the summary shrank the excerpt" },
  { key: "usable_pct", label: "Usable", get: (r) => num(r.usable_pct), dir: "desc", cell: "bar", hint: "% of runs that produced a usable summary" },
  { key: "fcs", label: "FCS", get: fcs, dir: "desc", cell: "fcs", hint: "Faithful-compression score = retention × compression (higher = better)" },
  // NOTE: failed_slice_count/error_kind are collected on the wire but failed rows
  // are NOT uploaded yet (reserved for a future error-reporting route), so there is
  // deliberately no "Failed" column — the public dataset has no failure rows.
  { key: "contributors", label: "N", get: (r) => num(r.contributors), dir: "desc", cell: "num", hint: "uploaded rows (identity-free, so not necessarily distinct people)" },
];

interface State {
  rows: BenchmarkModel[];
  sortKey: string;
  sortDir: Dir;
  search: string;
  family: string; // "all" or a model_family
  backend: string; // "all" | "local" | "api"
  k: number | null;
  corpus_version?: string;
  generated_at?: string | null;
  suppressed_groups?: number;
}

function visibleRows(state: State): BenchmarkModel[] {
  const q = state.search.trim().toLowerCase();
  const rows = state.rows.filter(
    (r) =>
      (state.backend === "all" || r.backend === state.backend) &&
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
          if (r.benchmark_scope === "partial_corpus") {
            const badge = document.createElement("span");
            badge.className = "twb-badge";
            badge.textContent = " partial";
            badge.title =
              "partial-corpus run (fewer slices scored) — not comparable to full-corpus rows";
            td.append(document.createTextNode(" "));
            td.append(badge);
          }
        } else if (c.cell === "backend") {
          td.className = "twb-model twb-backend" + (r.backend === "api" ? " twb-api" : "");
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
    "Backend separates local (ollama) from api (cloud) — NOT directly comparable. " +
    "FCS = retention × compression (higher = better); Usable = % producing a usable summary; " +
    "⚑ = disqualifying false-done; 'partial' = fewer corpus slices scored.";
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

  // Backend filter — keep local (ollama) and api (cloud) results visibly distinct;
  // they are NOT directly comparable. Only shown when both kinds are present.
  const backends = [...new Set(state.rows.map((r) => r.backend))];
  const backendChips: HTMLButtonElement[] = [];
  if (backends.length > 1) {
    const row = document.createElement("div");
    row.className = "tw-chips tw-chips-backend";
    row.setAttribute("role", "radiogroup");
    row.setAttribute("aria-label", "Filter by backend (local vs api)");
    for (const be of ["all", "local", "api"]) {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "tw-chip";
      b.textContent = be;
      b.setAttribute("role", "radio");
      b.dataset.backend = be;
      b.addEventListener("click", () => {
        state.backend = be;
        syncBackend();
        rerender();
      });
      backendChips.push(b);
      row.append(b);
    }
    el.append(row);
  }
  const syncBackend = () => {
    for (const b of backendChips) {
      const active = b.dataset.backend === state.backend;
      b.classList.toggle("tw-chip-on", active);
      b.setAttribute("aria-checked", String(active));
    }
  };
  syncBackend();

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
    backend: "all",
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
  code.textContent = "trimwire share benchmark --yes";
  const link = document.createElement("a");
  link.href = "/guides/benchmark/";
  link.textContent = "How the benchmark works →";
  const demoLink = document.createElement("a");
  demoLink.href = "?demo";
  demoLink.textContent = "Show what this looks like when populated →";
  demoLink.style.cssText = "font-size:0.85rem;color:var(--sl-color-accent);";
  box.append(title, body, code, link, demoLink);
  tableHost.append(box);

  status.classList.remove("tw-banner");
  status.textContent = "";
}

/** Render a prominent "Demo data" banner on `root` so demo mode is never mistaken
 *  for real community data (a screenshot must carry the warning). Inserted once;
 *  safe to call from init(). */
function mountDemoBadge(root: HTMLElement): void {
  if (root.querySelector(".tw-demo-badge")) return;
  const badge = document.createElement("p");
  badge.className = "tw-demo-badge";
  badge.setAttribute("role", "alert");
  badge.textContent =
    "⚠ Demo data — synthetic placeholder numbers, NOT real community results.";
  badge.style.cssText =
    "margin:0 0 1rem;padding:0.6rem 0.9rem;border:2px solid #d97706;" +
    "border-radius:0.5rem;background:rgba(217,119,6,0.12);font-weight:600;";
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
