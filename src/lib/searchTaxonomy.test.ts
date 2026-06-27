import type { TFunction } from "i18next";
import { describe, expect, it } from "vitest";
import { outcomeLabel, reflectionAspectLabel, taskCategoryLabel } from "./searchTaxonomy";

// Stand-in for i18next's `t`: echoes the key (with the `value` interpolation for
// the unknown fallback) so the tests assert the mapping logic, not the wording.
const t = ((key: string, opts?: { value?: number }) =>
  key === "taxonomy.unknown" ? `?(${opts?.value})` : key) as unknown as TFunction;

describe("taskCategoryLabel", () => {
  it("maps known enum integers to their dictionary keys", () => {
    expect(taskCategoryLabel(t, 1)).toBe("taxonomy.taskCategory.1");
    expect(taskCategoryLabel(t, 5)).toBe("taxonomy.taskCategory.5");
  });

  it("falls back with the raw integer for unknown values", () => {
    // Future proto additions show up as `?(N)` until the dictionary is
    // updated, which is visible enough to prompt a fix without crashing the UI.
    expect(taskCategoryLabel(t, 99)).toBe("?(99)");
  });
});

describe("outcomeLabel", () => {
  it("maps known enum integers to their dictionary keys", () => {
    expect(outcomeLabel(t, 1)).toBe("taxonomy.outcome.1");
    expect(outcomeLabel(t, 3)).toBe("taxonomy.outcome.3");
    expect(outcomeLabel(t, 5)).toBe("taxonomy.outcome.5");
  });
});

describe("reflectionAspectLabel", () => {
  it("maps the three documented aspects", () => {
    expect(reflectionAspectLabel(t, 1)).toBe("taxonomy.reflectionAspect.1");
    expect(reflectionAspectLabel(t, 2)).toBe("taxonomy.reflectionAspect.2");
    expect(reflectionAspectLabel(t, 3)).toBe("taxonomy.reflectionAspect.3");
  });
});
