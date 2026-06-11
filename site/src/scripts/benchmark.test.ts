import { beforeEach, describe, expect, it } from "vitest";

import { EXAMPLE, fcs, init, mount, renderEmptyState, type BenchmarkPayload } from "./benchmark";

/** A `.tw-bench` host with the slots the leaderboard renderer queries. */
function host(): HTMLElement {
  const root = document.createElement("div");
  root.className = "tw-bench";
  root.innerHTML =
    `<div class="tw-bench-kpi"></div>` +
    `<div class="tw-bench-controls"></div>` +
    `<div class="tw-bench-table"></div>` +
    `<p class="tw-bench-status"></p>`;
  document.body.append(root);
  return root;
}

beforeEach(() => {
  document.body.innerHTML = "";
});

describe("benchmark leaderboard", () => {
  it("fcs = retention × compression / 100, rounded", () => {
    expect(fcs({ avg_retention: 95, avg_compression: 52 } as never)).toBe(49);
    expect(fcs({ avg_retention: 100, avg_compression: 50 } as never)).toBe(50);
    expect(fcs({} as never)).toBe(0); // non-finite → 0, no NaN
  });

  it("renders one row per model with the 7 columns, default-sorted by FCS desc", () => {
    const root = host();
    mount(root, EXAMPLE);
    expect(root.querySelectorAll("tbody tr")).toHaveLength(EXAMPLE.models!.length);
    expect(root.querySelectorAll("thead th")).toHaveLength(7);
    const fcsCells = [...root.querySelectorAll("td.twb-fcs")].map((c) => Number(c.textContent));
    expect(fcsCells[0]).toBe(Math.max(...fcsCells));
  });

  it("renders a KPI strip (models / best FCS / best compression / contributors)", () => {
    const root = host();
    mount(root, EXAMPLE);
    const cards = root.querySelectorAll(".tw-bench-kpi .tw-kpi");
    expect(cards).toHaveLength(4);
    expect(cards[0].querySelector(".tw-kpi-v")?.textContent).toBe(String(EXAMPLE.models!.length));
  });

  it("flags non-zero false-done rows with an aria-labelled warning, clean rows none", () => {
    const root = host();
    mount(root, EXAMPLE);
    const warned = root.querySelectorAll('.twb-flag[aria-label*="false-done"]');
    const expected = EXAMPLE.models!.filter((m) => m.false_done_rate > 0).length;
    expect(warned).toHaveLength(expected);
    expect(expected).toBeGreaterThan(0);
  });

  it("marks every header sortable (aria-sort) and the active one directional", () => {
    const root = host();
    mount(root, EXAMPLE);
    const ths = [...root.querySelectorAll("thead th")];
    expect(ths.every((th) => th.hasAttribute("aria-sort"))).toBe(true);
    expect(ths.filter((th) => th.getAttribute("aria-sort") === "descending")).toHaveLength(1);
  });

  it("re-sorts when a header is clicked (interactive, no framework)", () => {
    const root = host();
    mount(root, EXAMPLE);
    root
      .querySelector<HTMLElement>("thead th")!
      .dispatchEvent(new MouseEvent("click", { bubbles: true })); // Model col
    expect(root.querySelector("thead th")!.getAttribute("aria-sort")).toBe("ascending");
    const firstModel = root.querySelector("tbody tr td")!.textContent ?? "";
    expect(firstModel.startsWith("gemma3")).toBe(true); // ascending by family·size
  });

  it("search box filters the visible rows", () => {
    const root = host();
    mount(root, EXAMPLE);
    const search = root.querySelector<HTMLInputElement>(".tw-search")!;
    search.value = "llama";
    search.dispatchEvent(new Event("input", { bubbles: true }));
    const rows = root.querySelectorAll("tbody tr");
    expect(rows).toHaveLength(1);
    expect(rows[0].textContent).toContain("llama3.1");
  });

  it("family chip filters rows + sets aria-checked", () => {
    const root = host();
    mount(root, EXAMPLE);
    const chip = root.querySelector<HTMLButtonElement>('.tw-chip[data-family="qwen3.5"]')!;
    chip.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    expect(chip.getAttribute("aria-checked")).toBe("true");
    expect(root.querySelectorAll("tbody tr")).toHaveLength(
      EXAMPLE.models!.filter((m) => m.model_family === "qwen3.5").length,
    );
  });

  it("shows the k-anon empty state when there are no models", () => {
    const root = host();
    mount(root, { k: 5, models: [] } as BenchmarkPayload);
    expect(root.querySelector("table")).toBeNull();
    expect(root.querySelector(".tw-bench-table .tw-banner")?.textContent).toContain("k-anonymity");
  });

  it("renders an honest empty-state + opt-in CTA (no fake rows) when there's no data", () => {
    const root = host();
    renderEmptyState(root);
    expect(root.querySelector("table")).toBeNull(); // no seeded sample table
    expect(root.querySelectorAll(".tw-bench-kpi .tw-kpi")).toHaveLength(0); // no fake KPIs
    const empty = root.querySelector(".tw-bench-empty");
    expect(empty).not.toBeNull();
    expect(empty?.querySelector(".tw-empty-cmd")?.textContent).toBe("trimwire share benchmark");
    expect(empty?.querySelector("a")?.getAttribute("href")).toBe("/guides/benchmark/");
  });

  it("?demo query param mounts EXAMPLE fixture + shows demo badge (no real fetch)", () => {
    // Simulate ?demo in the URL.
    Object.defineProperty(window, "location", {
      value: { search: "?demo" },
      configurable: true,
    });
    const root = host();
    init(root);
    // Should have rendered the fixture table (not the empty-state).
    expect(root.querySelectorAll("tbody tr")).toHaveLength(EXAMPLE.models!.length);
    // Badge must be present and labelled.
    const badge = root.querySelector(".tw-demo-badge");
    expect(badge).not.toBeNull();
    expect(badge!.textContent).toContain("Demo data");
    // Restore.
    Object.defineProperty(window, "location", {
      value: { search: "" },
      configurable: true,
    });
  });

  it("FCS column header has a title tooltip mentioning 'higher = better'", () => {
    const root = host();
    mount(root, EXAMPLE);
    const fcsHeader = [...root.querySelectorAll<HTMLElement>("thead th")].find(
      (th) => th.textContent?.trim() === "FCS",
    )!;
    expect(fcsHeader.title).toContain("higher = better");
  });

  it("status line includes Usable definition", () => {
    const root = host();
    mount(root, EXAMPLE);
    const status = root.querySelector(".tw-bench-status")?.textContent ?? "";
    expect(status).toContain("Usable");
  });

  it("escapes data via textContent (no HTML injection from a hostile feed)", () => {
    const root = host();
    const evil = { model_family: "<img src=x onerror=alert(1)>", model_size_bucket: "3-4b",
      contributors: 1, avg_retention: 50, avg_compression: 50, false_done_rate: 0, usable_pct: 100 };
    mount(root, { k: 5, models: [evil] } as BenchmarkPayload);
    expect(root.querySelector("tbody tr td img")).toBeNull();
    expect(root.querySelector("td.twb-model")?.textContent).toContain("<img");
  });
});
