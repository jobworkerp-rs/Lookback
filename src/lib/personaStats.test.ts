import { describe, expect, it } from "vitest";
import type { PersonalityProfileContent } from "@/types/api";
import { buildPersonaStats, countPopulatedCategories, PERSONA_CATEGORIES } from "./personaStats";

describe("countPopulatedCategories", () => {
  it("returns 0 for null content", () => {
    expect(countPopulatedCategories(null)).toBe(0);
  });

  it("returns 0 when every category is empty (empty arrays / empty objects / missing)", () => {
    expect(
      countPopulatedCategories({
        interests: [],
        preferences: [],
        values_and_beliefs: [],
        anti_preferences: [],
        decision_style: {},
        communication_style: {},
      }),
    ).toBe(0);
  });

  it("counts non-empty object-array lists and content-bearing object styles", () => {
    expect(
      countPopulatedCategories({
        interests: [{ topic: "rust" }],
        preferences: [],
        decision_style: { summary: "decisive" },
        communication_style: {},
      }),
    ).toBe(2);
  });

  it("does not count style objects whose fields are present but blank", () => {
    // Regression: keys-present-but-blank styles ({ summary: "" }) render nothing
    // in the grid, so the badge must not count them either. Derived from
    // formatProfileContent so the two can never disagree.
    expect(
      countPopulatedCategories({
        interests: [{ topic: "rust" }],
        decision_style: { summary: "   " },
        communication_style: { tone: "" },
      }),
    ).toBe(1);
  });

  it("does not count list entries that flatten to nothing", () => {
    // An entry with only structural fields (weight/threads, no topic/text)
    // produces no displayable item, so the category is empty.
    expect(
      countPopulatedCategories({
        interests: [{ weight: "high", supporting_source_thread_ids: ["1"] }],
      }),
    ).toBe(0);
  });

  it("counts all 6 when every category has content", () => {
    const content: PersonalityProfileContent = {
      interests: [{ topic: "a" }],
      preferences: [{ axis: "x", preference: "y" }],
      values_and_beliefs: [{ belief: "c" }],
      anti_preferences: [{ avoid: "d" }],
      decision_style: { summary: "e" },
      communication_style: { tone: "f" },
    };
    expect(countPopulatedCategories(content)).toBe(PERSONA_CATEGORIES);
  });
});

describe("buildPersonaStats", () => {
  it("returns 0 categories and placeholder version when profile missing", () => {
    const stats = buildPersonaStats({ threads: 42, signals: 7, content: null });
    expect(stats).toEqual({
      threads: 42,
      signals: 7,
      categories: 0,
      profile_version: "-",
    });
  });

  it("uses profile_version and the populated-category count", () => {
    const stats = buildPersonaStats({
      threads: 12,
      signals: 5,
      content: { interests: [{ topic: "a" }], profile_version: "v3" },
    });
    expect(stats.profile_version).toBe("v3");
    expect(stats.signals).toBe(5);
    expect(stats.threads).toBe(12);
    expect(stats.categories).toBe(1);
  });

  it("falls back to '-' when profile_version is empty string", () => {
    const stats = buildPersonaStats({
      threads: 0,
      signals: 0,
      content: { interests: [], profile_version: "   " },
    });
    expect(stats.profile_version).toBe("-");
  });
});
