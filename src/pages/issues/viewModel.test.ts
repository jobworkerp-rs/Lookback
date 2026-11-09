import { describe, expect, it } from "vitest";
import {
  ACTIVE_ISSUE_STATES,
  EMPTY_REVIEW_FILTERS,
  matchesSourceLabels,
  reviewIntegrityLabel,
  reviewMatches,
  reviewResolutionLabel,
  splitSourceLabels,
  uniqueSourceLabels,
  userStateTransitionTarget,
} from "./viewModel";

describe("source label filters", () => {
  it("deduplicates and orders generic labels without interpreting their prefixes", () => {
    expect(
      uniqueSourceLabels(["team:platform", "dir:repo", "team:platform", "path:/repo"]),
    ).toEqual(["dir:repo", "path:/repo", "team:platform"]);
  });

  it("matches any selected source label by default and all selected labels in all mode", () => {
    const item = { source_labels: ["team:platform", "dir:/repo"] };

    expect(matchesSourceLabels(item, [])).toBe(true);
    expect(matchesSourceLabels(item, ["team:platform", "missing"])).toBe(true);
    expect(matchesSourceLabels(item, ["team:platform", "dir:/repo"], "all")).toBe(true);
    expect(matchesSourceLabels(item, ["team:platform", "missing"], "all")).toBe(false);
  });

  it("splits source labels at the visible limit and reports hidden labels", () => {
    expect(splitSourceLabels([], 2)).toEqual({ visible: [], hiddenCount: 0 });
    expect(splitSourceLabels(["a", "b"], 2)).toEqual({
      visible: ["a", "b"],
      hiddenCount: 0,
    });
    expect(splitSourceLabels(["a", "b", "c"], 2)).toEqual({
      visible: ["a", "b"],
      hiddenCount: 1,
    });
  });
});

describe("review row status labels", () => {
  it("distinguishes each integrity anomaly from an actionable row", () => {
    expect(reviewIntegrityLabel("mapping_invalid")).toContain("mapping 不正");
    expect(reviewIntegrityLabel("issue_not_found")).toContain("Issue 不在");
    expect(reviewResolutionLabel("actionable")).toBe("操作可能");
    expect(reviewResolutionLabel("resolution_pending")).toContain("確認中");
  });
});

describe("awaiting-review issue state", () => {
  it("keeps awaiting review in the active read model without offering generic state actions", () => {
    expect(ACTIVE_ISSUE_STATES).toContain("AWAITING_REVIEW");
    expect(userStateTransitionTarget("AWAITING_REVIEW")).toBeNull();
  });
});

describe("reviewMatches", () => {
  const item = {
    review_state: "candidate",
    priority: "P1",
    confidence: 0.6,
    detection: "explicit",
  } as const;

  it("accepts every item with the empty filters", () => {
    expect(reviewMatches(item, EMPTY_REVIEW_FILTERS)).toBe(true);
    expect(reviewMatches(item, {})).toBe(true);
  });

  it("matches the target, detection, and single priority exactly", () => {
    expect(reviewMatches(item, { target: "candidate" })).toBe(true);
    expect(reviewMatches(item, { target: "unreviewed" })).toBe(false);
    expect(reviewMatches(item, { detection: "explicit" })).toBe(true);
    expect(reviewMatches(item, { detection: "inferred" })).toBe(false);
    expect(reviewMatches(item, { priority: "P1" })).toBe(true);
    expect(reviewMatches(item, { priority: "P3" })).toBe(false);
  });

  it("lets a non-empty priorities list override the single priority", () => {
    expect(reviewMatches(item, { priority: "P3", priorities: ["P1", "P0"] })).toBe(true);
    expect(reviewMatches(item, { priority: "P1", priorities: ["P3"] })).toBe(false);
  });

  it("treats min_confidence as an inclusive lower bound", () => {
    expect(reviewMatches(item, { min_confidence: 0.6 })).toBe(true);
    expect(reviewMatches(item, { min_confidence: 0.61 })).toBe(false);
    expect(reviewMatches(item, { min_confidence: null })).toBe(true);
  });
});
