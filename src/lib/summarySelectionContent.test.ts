import { describe, expect, it } from "vitest";
import {
  parseSummarySelection,
  summarySelectionExcerpt,
  summarySelectionExcerptFromParsed,
} from "./summarySelectionContent";

describe("summarySelectionContent", () => {
  it("extracts an excerpt from a structured summary", () => {
    expect(
      summarySelectionExcerpt(
        JSON.stringify({
          タイトル: "日本語の題名",
          overall_purpose: "目的",
          category: "coding",
          status: "resolved",
          key_decisions: ["一つ目", "二つ目", "三つ目", 1],
        }),
      ),
    ).toEqual({
      title: "日本語の題名",
      summary: "目的",
      category: "coding",
      status: "resolved",
      decisions: ["一つ目", "二つ目"],
    });
  });

  it("keeps malformed content readable as a plain-text excerpt", () => {
    expect(summarySelectionExcerpt("  legacy content  ")).toEqual({
      title: null,
      summary: "legacy content",
      category: null,
      status: null,
      decisions: [],
    });
  });

  it("creates the same excerpt from an already parsed summary without reparsing", () => {
    const parsed = parseSummarySelection(
      JSON.stringify({ title: "題名", summary: "本文", key_decisions: ["決定"] }),
    );

    expect(summarySelectionExcerptFromParsed(parsed, "unused fallback")).toEqual({
      title: "題名",
      summary: "本文",
      category: null,
      status: null,
      decisions: ["決定"],
    });
  });

  it("rejects JSON values that are not objects", () => {
    expect(parseSummarySelection('["not", "a summary"]')).toBeNull();
  });
});
