import { beforeEach, describe, expect, it } from "vitest";

import { EXAMPLE, bar, init, mount, renderEmptyState, type AggregatePayload } from "./dashboard";

/** A `.tw-dash` host with the slots the renderer queries. */
function host(): HTMLElement {
  const root = document.createElement("div");
  root.className = "tw-dash";
  root.innerHTML =
    `<div class="tw-dash-kpi"></div>` +
    `<div class="tw-dash-filters" hidden></div>` +
    `<div class="tw-dash-table"></div>` +
    `<p class="tw-dash-status"></p>`;
  document.body.append(root);
  return root;
}

beforeEach(() => {
  document.body.innerHTML = "";
});

describe("dashboard renderer", () => {
  const fillPct = (el: HTMLDivElement) =>
    parseFloat(el.querySelector<HTMLElement>(".fill")!.style.width);

  it("bar() coerces junk to a 0..100-clamped bar with a combined aria-label", () => {
    expect(bar("avg reduction", 55, 100, "%").getAttribute("aria-label")).toBe("avg reduction: 55%");
    expect(fillPct(bar("avg reduction", 55, 100, "%"))).toBeCloseTo(55);
    expect(fillPct(bar("x", Number.NaN, 100))).toBe(0); // junk → 0
    expect(fillPct(bar("x", 999, 100))).toBe(100); // clamped
  });

  it("renders one data row per group in the table (plus one detail row each)", () => {
    const root = host();
    mount(root, EXAMPLE);
    const dataRows = root.querySelectorAll("tbody tr.twd-row");
    expect(dataRows).toHaveLength(EXAMPLE.groups!.length);
  });

  it("renders a KPI strip (cohorts / best reduction / cache stability / contributors)", () => {
    const root = host();
    mount(root, EXAMPLE);
    const cards = root.querySelectorAll(".tw-dash-kpi .tw-kpi");
    expect(cards).toHaveLength(4);
    // First card = cohort count
    expect(cards[0].querySelector(".tw-kpi-v")?.textContent).toBe(String(EXAMPLE.groups!.length));
  });

  it("marks every sortable header with aria-sort and the active one directional", () => {
    const root = host();
    mount(root, EXAMPLE);
    // Only sortable data-column headers have tabIndex=0 (the toggle th does not)
    const ths = [...root.querySelectorAll<HTMLElement>("thead th[tabindex]")];
    expect(ths.length).toBeGreaterThan(0);
    expect(ths.every((th) => th.hasAttribute("aria-sort"))).toBe(true);
    expect(ths.filter((th) => th.getAttribute("aria-sort") === "descending")).toHaveLength(1);
  });

  it("re-sorts when a sortable header is clicked", () => {
    const root = host();
    mount(root, EXAMPLE);
    // Click the "Cohort" column header (sort asc — rerender rebuilds the DOM)
    const cohortTh = [...root.querySelectorAll<HTMLElement>("thead th[tabindex]")]
      .find((th) => th.textContent?.trim() === "Cohort")!;
    cohortTh.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    // After rerender, re-query the fresh th
    const cohortThAfter = [...root.querySelectorAll<HTMLElement>("thead th[tabindex]")]
      .find((th) => th.textContent?.trim().startsWith("Cohort"))!;
    expect(cohortThAfter.getAttribute("aria-sort")).toBe("ascending");
  });

  it("builds filter selects for fields with >1 distinct value", () => {
    const root = host();
    mount(root, EXAMPLE);
    const selects = root.querySelectorAll<HTMLSelectElement>(".tw-dash-filters select");
    expect(selects.length).toBeGreaterThan(0);
    expect([...selects].every((s) => s.dataset.field)).toBe(true);
  });

  it("uses the collector's summarizer_backend field (filter select + cohort label)", () => {
    const root = host();
    mount(root, EXAMPLE);
    // The filter select keys off the wire field name, not the old local_model.
    const sel = root.querySelector<HTMLSelectElement>(
      '.tw-dash-filters select[data-field="summarizer_backend"]',
    )!;
    expect(sel).not.toBeNull();
    expect([...sel.options].map((o) => o.value)).toContain("local");
    // A non-off backend is appended to the cohort label.
    const labels = [...root.querySelectorAll("tbody tr.twd-row td.twd-cohort")].map(
      (td) => td.textContent ?? "",
    );
    expect(labels.some((l) => l.includes("· local"))).toBe(true);
  });

  it("re-renders on a filter change — filters rows down", () => {
    const root = host();
    mount(root, EXAMPLE);
    const before = root.querySelectorAll("tbody tr.twd-row").length;
    const modelSel = root.querySelector<HTMLSelectElement>('.tw-dash-filters select[data-field="model_family"]')!;
    modelSel.value = "claude-sonnet-4-6";
    modelSel.dispatchEvent(new Event("change", { bubbles: true }));
    const after = root.querySelectorAll("tbody tr.twd-row").length;
    expect(after).toBeLessThan(before);
    expect(after).toBe(EXAMPLE.groups!.filter((g) => g.model_family === "claude-sonnet-4-6").length);
  });

  it("shows the 'no match' banner when filters exclude everything", () => {
    const root = host();
    mount(root, EXAMPLE);
    const sel = root.querySelector<HTMLSelectElement>('.tw-dash-filters select[data-field="model_family"]')!;
    sel.value = "claude-haiku"; // not in the EXAMPLE
    sel.dispatchEvent(new Event("change", { bubbles: true }));
    expect(root.querySelector(".tw-dash-table .tw-banner")?.textContent).toContain("No cohorts match");
  });

  it("shows the k-anon empty state for a live feed with no groups", () => {
    const root = host();
    mount(root, { k: 10, groups: [] } as AggregatePayload);
    expect(root.querySelector("table")).toBeNull();
    expect(root.querySelector(".tw-dash-status")?.textContent).toContain("k-anonymity");
  });

  it("expand toggle button: aria-expanded toggles between false/true + detail row visibility", () => {
    const root = host();
    mount(root, EXAMPLE);
    const btn = root.querySelector<HTMLButtonElement>("button.twd-toggle")!;
    expect(btn.getAttribute("aria-expanded")).toBe("false");
    const detailRow = btn.closest("tr")!.nextElementSibling as HTMLElement;
    expect(detailRow.hidden).toBe(true);
    // Expand
    btn.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    expect(btn.getAttribute("aria-expanded")).toBe("true");
    expect(detailRow.hidden).toBe(false);
    // Collapse
    btn.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    expect(btn.getAttribute("aria-expanded")).toBe("false");
    expect(detailRow.hidden).toBe(true);
  });

  it("?demo query param mounts EXAMPLE fixture + shows demo badge (no real fetch)", () => {
    Object.defineProperty(window, "location", {
      value: { search: "?demo" },
      configurable: true,
    });
    const root = host();
    init(root);
    // Should have rendered the fixture rows (not the empty-state).
    expect(root.querySelectorAll("tbody tr.twd-row")).toHaveLength(EXAMPLE.groups!.length);
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

  it("cohort cell has a title tooltip describing the segment format", () => {
    const root = host();
    mount(root, EXAMPLE);
    const cohortCells = [...root.querySelectorAll<HTMLElement>("tbody tr.twd-row td.twd-cohort")];
    // The main cohort cell (key="cohort") carries the format tooltip; the
    // "Length" cell (same cell type) must not. Key off the rendered text, not
    // cell order, so adding another cohort-type column can't silently skip it.
    const mainCells = cohortCells.filter((td) => td.textContent?.includes("·"));
    expect(mainCells.length).toBe(EXAMPLE.groups!.length);
    expect(mainCells.every((td) => td.title.startsWith("Segment: model family · profile"))).toBe(true);
    const lengthCells = cohortCells.filter((td) => !td.textContent?.includes("·"));
    expect(lengthCells.every((td) => td.title === "")).toBe(true);
  });

  it("renderEmptyState() renders the CTA — no fake rows, correct command + guide link", () => {
    const root = host();
    renderEmptyState(root);
    // No fake rows.
    expect(root.querySelector("tbody tr.twd-row")).toBeNull();
    // The empty-state container is present.
    const box = root.querySelector(".tw-dash-table .tw-dash-empty");
    expect(box).not.toBeNull();
    // Title text.
    expect(box!.querySelector(".tw-empty-title")?.textContent).toContain("No community stats published yet");
    // Opt-in command.
    expect(box!.querySelector(".tw-empty-cmd")?.textContent).toBe("trimwire share stats");
    // Guide link.
    const link = box!.querySelector("a");
    expect(link?.getAttribute("href")).toBe("/guides/telemetry/");
    // Status line is blank.
    expect(root.querySelector(".tw-dash-status")?.textContent).toBe("");
    // Filters hidden.
    expect((root.querySelector(".tw-dash-filters") as HTMLElement).hidden).toBe(true);
  });
});
