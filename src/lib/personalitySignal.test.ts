import { describe, expect, it } from "vitest";
import type { PersonalityProfileContent, PersonalitySignalContent } from "@/types/api";
import {
  formatProfileContent,
  formatSignalContent,
  PERSONA_CATEGORY_LABELS,
  rawSignalFallback,
} from "./personalitySignal";

describe("formatSignalContent", () => {
  it("formats schema-shaped list categories with confidence inline and evidence as detail", () => {
    const content: PersonalitySignalContent = {
      interests: [{ topic: "rust", confidence: "high", evidence: "uses it daily" }],
      preferences: [{ axis: "editor", preference: "vim", confidence: "medium" }],
      values_and_beliefs: [{ belief: "DRY", confidence: "high" }],
      anti_preferences: [{ avoid: "long PRs", confidence: "low", evidence: "splits them" }],
    };
    const views = formatSignalContent(content);
    const byLabel = Object.fromEntries(views.map((v) => [v.label, v]));

    expect(byLabel.Interests?.items[0]).toEqual({
      primary: "rust (high)",
      detail: "uses it daily",
    });
    expect(byLabel.Preferences?.items[0]?.primary).toBe("editor: vim (medium)");
    expect(byLabel["Values & beliefs"]?.items[0]?.primary).toBe("DRY (high)");
    expect(byLabel["Anti-preferences"]?.items[0]?.primary).toBe("long PRs (low)");
  });

  it("formats the observed category/description shape the LLM actually emits", () => {
    // Real export: every category uses a uniform `category` + `description`
    // instead of the per-category schema field names. The heading is the
    // category, the body is the description.
    const content: PersonalitySignalContent = {
      preferences: [
        {
          category: "language",
          description: "LLM用ドキュメントは英語で記載することを好む",
          confidence: "high",
        },
      ],
      anti_preferences: [
        { category: "language", description: "日本語を使用することを好まない", confidence: "high" },
      ],
      decision_style: { description: "明確な指示を直接的に与える", confidence: "medium" },
      communication_style: { description: "簡潔で直接的な指示を出す", confidence: "medium" },
    };
    const views = formatSignalContent(content);
    const byLabel = Object.fromEntries(views.map((v) => [v.label, v]));

    expect(byLabel.Preferences?.items[0]).toEqual({
      primary: "language (high)",
      detail: "LLM用ドキュメントは英語で記載することを好む",
    });
    expect(byLabel["Anti-preferences"]?.items[0]).toEqual({
      primary: "language (high)",
      detail: "日本語を使用することを好まない",
    });
    // Object categories with only `description` promote it to the primary.
    expect(byLabel["Decision style"]?.items[0]?.primary).toBe("明確な指示を直接的に与える");
    expect(byLabel["Communication style"]?.items[0]?.primary).toBe("簡潔で直接的な指示を出す");
  });

  it("formats decision_style schema summary + traits as one line", () => {
    const views = formatSignalContent({
      decision_style: { summary: "first principles", traits: ["fast", "data-driven"] },
    });
    const ds = views.find((v) => v.label === "Decision style");
    expect(ds?.items[0]?.primary).toBe("first principles · fast · data-driven");
  });

  it("formats communication_style schema head line with notes as detail", () => {
    const views = formatSignalContent({
      communication_style: {
        tone: "direct",
        verbosity: "concise",
        language_preference: "ja",
        notes: "prefers bullet points",
      },
    });
    const cs = views.find((v) => v.label === "Communication style");
    expect(cs?.items[0]?.primary).toBe("tone: direct · verbosity: concise · language: ja");
    expect(cs?.items[0]?.detail).toBe("prefers bullet points");
  });

  it("drops entries with neither a heading nor a body", () => {
    const views = formatSignalContent({
      interests: [{ topic: "  ", category: "  ", description: "  ", evidence: "  " }],
      preferences: [],
      decision_style: { summary: "   ", traits: [], description: "  " },
    });
    expect(views).toEqual([]);
  });

  it("omits confidence suffix when absent", () => {
    const views = formatSignalContent({ interests: [{ topic: "tauri" }] });
    expect(views[0]?.items[0]?.primary).toBe("tauri");
    expect(views[0]?.items[0]?.detail).toBeUndefined();
  });

  it("returns an empty array for empty content", () => {
    expect(formatSignalContent({})).toEqual([]);
  });

  it("surfaces reason as its own category", () => {
    const views = formatSignalContent({ reason: "user spoke only in short acks" });
    expect(views).toEqual([
      { label: "Reason", items: [{ primary: "user spoke only in short acks" }] },
    ]);
  });

  it("carries memory_ids on list and object items", () => {
    const views = formatSignalContent({
      interests: [{ topic: "rust", memory_ids: ["100", "200"] }],
      decision_style: { description: "decisive", memory_ids: ["300"] },
    });
    const byLabel = Object.fromEntries(views.map((v) => [v.label, v]));
    expect(byLabel.Interests?.items[0]?.memoryIds).toEqual(["100", "200"]);
    expect(byLabel["Decision style"]?.items[0]?.memoryIds).toEqual(["300"]);
  });

  it("normalizes memory_ids: trims, drops blanks, undefined when empty", () => {
    const views = formatSignalContent({
      interests: [
        { topic: "a", memory_ids: [" 1 ", "", "2"] },
        { topic: "b", memory_ids: ["  ", ""] },
      ],
    });
    expect(views[0]?.items[0]?.memoryIds).toEqual(["1", "2"]);
    expect(views[0]?.items[1]?.memoryIds).toBeUndefined();
  });
});

describe("formatProfileContent", () => {
  // The authoritative merged shape: object-array list categories + object
  // style categories. The original crash was ListView rendering these objects
  // as React children; the formatter must flatten them into strings.
  const realProfile: PersonalityProfileContent = {
    profile_version: "1.0",
    summary: "a builder",
    interests: [
      {
        topic: "rust",
        weight: "high",
        supporting_source_thread_ids: ["10"],
        memory_ids: ["100", "200"],
        first_seen_at: "2026-01-01T00:00:00Z",
        last_seen_at: "2026-05-01T00:00:00Z",
      },
    ],
    preferences: [
      { axis: "editor", preference: "vim", weight: "medium", supporting_source_thread_ids: ["12"] },
    ],
    values_and_beliefs: [{ belief: "DRY", weight: "high", supporting_source_thread_ids: ["13"] }],
    anti_preferences: [{ avoid: "long PRs", weight: "low", supporting_source_thread_ids: ["14"] }],
    decision_style: {
      summary: "first principles",
      traits: ["fast", "data-driven"],
      supporting_source_thread_ids: ["15"],
      memory_ids: ["300"],
    },
    communication_style: {
      tone: "direct",
      verbosity: "concise",
      language_preference: "ja",
      notes: "prefers bullets",
      supporting_source_thread_ids: ["16"],
    },
    metrics: { source_thread_count: 6 },
  };

  it("flattens every category into renderable strings (regression: no raw objects)", () => {
    const views = formatProfileContent(realProfile);
    for (const view of views) {
      for (const item of view.items) {
        expect(typeof item.primary).toBe("string");
        expect(item.detail === undefined || typeof item.detail === "string").toBe(true);
      }
    }
  });

  it("maps each category's primary, weight and memoryIds", () => {
    const views = formatProfileContent(realProfile);
    const byLabel = Object.fromEntries(views.map((v) => [v.label, v]));

    expect(byLabel.Interests?.items[0]?.primary).toBe("rust");
    expect(byLabel.Interests?.items[0]?.weight).toBe("high");
    expect(byLabel.Interests?.items[0]?.memoryIds).toEqual(["100", "200"]);

    expect(byLabel.Preferences?.items[0]?.primary).toBe("editor: vim");
    expect(byLabel.Preferences?.items[0]?.weight).toBe("medium");
    expect(byLabel["Values & beliefs"]?.items[0]?.primary).toBe("DRY");
    expect(byLabel["Anti-preferences"]?.items[0]?.primary).toBe("long PRs");

    expect(byLabel["Decision style"]?.items[0]?.primary).toBe(
      "first principles · fast · data-driven",
    );
    expect(byLabel["Communication style"]?.items[0]?.primary).toBe(
      "tone: direct · verbosity: concise · language: ja",
    );
    expect(byLabel["Communication style"]?.items[0]?.detail).toBe("prefers bullets");
  });

  it("never emits a Reason category", () => {
    // `reason` is signal-only; even if present it must not surface in a profile.
    const withReason = {
      interests: [{ topic: "x" }],
      reason: "should be ignored",
    } as unknown as PersonalityProfileContent;
    const views = formatProfileContent(withReason);
    expect(views.some((v) => v.label === "Reason")).toBe(false);
  });

  it("does not throw on malformed input (list field is a string, style is null)", () => {
    const malformed = {
      // biome-ignore lint/suspicious/noExplicitAny: simulating bad stored JSON
      interests: "not-an-array" as any,
      decision_style: null as unknown as undefined,
    } satisfies Partial<PersonalityProfileContent>;
    expect(() => formatProfileContent(malformed)).not.toThrow();
    expect(formatProfileContent(malformed)).toEqual([]);
  });

  it("returns an empty array for empty content", () => {
    expect(formatProfileContent({})).toEqual([]);
  });

  it("only emits labels that PERSONA_CATEGORY_LABELS declares (drift guard)", () => {
    // The grid renders fixed PERSONA_CATEGORY_LABELS slots and matches views by
    // exact label string. If a formatter label drifts from the constant, the
    // category silently renders empty — this guards that correspondence.
    const labels = formatProfileContent(realProfile).map((v) => v.label);
    for (const label of labels) {
      expect(PERSONA_CATEGORY_LABELS).toContain(label);
    }
    // realProfile populates every category, so all 6 must surface.
    expect(labels).toHaveLength(PERSONA_CATEGORY_LABELS.length);
  });
});

describe("rawSignalFallback", () => {
  it("pretty-prints content with no_signal stripped", () => {
    const out = rawSignalFallback(JSON.stringify({ no_signal: false, mystery_field: "value" }));
    expect(out).toBe(`{\n  "mystery_field": "value"\n}`);
  });

  it("returns null when only no_signal remains after stripping", () => {
    expect(rawSignalFallback(JSON.stringify({ no_signal: false }))).toBeNull();
    expect(rawSignalFallback(JSON.stringify({ no_signal: true }))).toBeNull();
  });

  it("falls back to the raw string for invalid JSON", () => {
    expect(rawSignalFallback("not json")).toBe("not json");
  });

  it("returns null for empty / whitespace content", () => {
    expect(rawSignalFallback("")).toBeNull();
    expect(rawSignalFallback("   ")).toBeNull();
  });
});
