import { describe, expect, it } from "vitest";
import type { LabelWithCount } from "@/types/api";
import {
  CATEGORY_GROUP_KEY,
  groupByPrefix,
  sortLabelsByPrefixPriority,
  splitLabelPrefix,
} from "./labelPrefix";

describe("splitLabelPrefix", () => {
  it("splits a prefixed label on the first colon", () => {
    expect(splitLabelPrefix("dir:/foo")).toEqual({ prefix: "dir", value: "/foo" });
  });

  it("returns null prefix when no colon is present", () => {
    expect(splitLabelPrefix("summary")).toEqual({ prefix: null, value: "summary" });
  });

  it("keeps later colons in the value", () => {
    // `git:branch:main` belongs to the `git` group, with value `branch:main`.
    expect(splitLabelPrefix("git:branch:main")).toEqual({ prefix: "git", value: "branch:main" });
  });
});

function l(label: string, n: number): LabelWithCount {
  return { label, thread_count: n };
}

describe("groupByPrefix", () => {
  it("orders groups by the fixed prefix priority (category first), not by label count", () => {
    // agent has fewer labels than dir, but priority pins it ahead regardless.
    const groups = groupByPrefix([
      l("summary", 10),
      l("dir:/a", 5),
      l("dir:/b", 3),
      l("branch:main", 7),
      l("agent:codex", 1),
    ]);
    expect(groups.map((g) => g.prefix)).toEqual([CATEGORY_GROUP_KEY, "agent", "branch", "dir"]);
  });

  it("sorts intra-group labels by label name ascending", () => {
    const groups = groupByPrefix([l("dir:/b", 3), l("dir:/a", 5), l("dir:/c", 5)]);
    expect(groups[0]?.labels.map((x) => x.label)).toEqual(["dir:/a", "dir:/b", "dir:/c"]);
  });

  it("places unknown prefixes after the known ones, by ascending prefix name", () => {
    const groups = groupByPrefix([l("zeta:x", 1), l("alpha:x", 1), l("agent:codex", 1)]);
    expect(groups.map((g) => g.prefix)).toEqual(["agent", "alpha", "zeta"]);
  });

  it("does not emit a category group when no no-prefix labels exist", () => {
    const groups = groupByPrefix([l("dir:/a", 1)]);
    expect(groups.map((g) => g.prefix)).toEqual(["dir"]);
  });
});

describe("sortLabelsByPrefixPriority", () => {
  it("orders labels by the fixed prefix priority (category first)", () => {
    // Deliberately shuffled input covering every priority prefix.
    const sorted = sortLabelsByPrefixPriority([
      "path:/p",
      "dir:/d",
      "branch:main",
      "vault:v",
      "provider:openai",
      "agent:codex",
      "summary",
    ]);
    expect(sorted).toEqual([
      "summary",
      "agent:codex",
      "provider:openai",
      "vault:v",
      "branch:main",
      "dir:/d",
      "path:/p",
    ]);
  });

  it("places unknown prefixes after the known ones, sorted by prefix ascending", () => {
    const sorted = sortLabelsByPrefixPriority(["zeta:z", "agent:codex", "alpha:a", "dir:/d"]);
    expect(sorted).toEqual(["agent:codex", "dir:/d", "alpha:a", "zeta:z"]);
  });

  it("keeps prefix-less (category) labels first, in input order", () => {
    const sorted = sortLabelsByPrefixPriority(["coding_agent", "dir:/d", "summary"]);
    expect(sorted).toEqual(["coding_agent", "summary", "dir:/d"]);
  });

  it("preserves input order within the same prefix group (stable sort)", () => {
    const sorted = sortLabelsByPrefixPriority(["dir:/b", "dir:/a", "dir:/c"]);
    expect(sorted).toEqual(["dir:/b", "dir:/a", "dir:/c"]);
  });

  it("returns an empty array unchanged", () => {
    expect(sortLabelsByPrefixPriority([])).toEqual([]);
  });

  it("does not mutate the input array", () => {
    const input = ["dir:/d", "agent:codex"];
    sortLabelsByPrefixPriority(input);
    expect(input).toEqual(["dir:/d", "agent:codex"]);
  });
});
