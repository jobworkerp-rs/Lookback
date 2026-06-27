import { describe, expect, it } from "vitest";
import type { LabelWithCount } from "@/types/api";
import { CATEGORY_GROUP_KEY, groupByPrefix, splitLabelPrefix } from "./labelPrefix";

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
  it("pins the category group first, then orders prefixes by label count desc", () => {
    const groups = groupByPrefix([
      l("summary", 10),
      l("dir:/a", 5),
      l("dir:/b", 3),
      l("branch:main", 7),
    ]);
    expect(groups.map((g) => g.prefix)).toEqual([CATEGORY_GROUP_KEY, "dir", "branch"]);
  });

  it("sorts intra-group labels by thread_count desc", () => {
    const groups = groupByPrefix([l("dir:/b", 3), l("dir:/a", 5), l("dir:/c", 5)]);
    expect(groups[0]?.labels.map((x) => x.label)).toEqual(["dir:/a", "dir:/c", "dir:/b"]);
  });

  it("breaks ties between prefixes by ascending prefix name", () => {
    const groups = groupByPrefix([l("z:x", 1), l("a:x", 1)]);
    expect(groups.map((g) => g.prefix)).toEqual(["a", "z"]);
  });

  it("does not emit a category group when no no-prefix labels exist", () => {
    const groups = groupByPrefix([l("dir:/a", 1)]);
    expect(groups.map((g) => g.prefix)).toEqual(["dir"]);
  });
});
