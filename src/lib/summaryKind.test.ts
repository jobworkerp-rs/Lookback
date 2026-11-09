import { describe, expect, it } from "vitest";
import { applyDependency, type KindSelection, topKind } from "@/lib/summaryKind";

function selection(partial: Partial<KindSelection> = {}): KindSelection {
  return {
    "per-thread": false,
    "thread-group": false,
    daily: false,
    weekly: false,
    monthly: false,
    ...partial,
  };
}

describe("summary kind dependencies", () => {
  it.each(["daily", "weekly", "monthly"] as const)(
    "%s selection includes ThreadGroup and per-thread dependencies",
    (kind) => {
      expect(applyDependency(selection(), kind, true)).toMatchObject({
        "per-thread": true,
        "thread-group": true,
        [kind]: true,
      });
    },
  );

  it("turning off ThreadGroup turns off every public period while retaining per-thread", () => {
    expect(
      applyDependency(
        selection({
          "per-thread": true,
          "thread-group": true,
          daily: true,
          weekly: true,
          monthly: true,
        }),
        "thread-group",
        false,
      ),
    ).toEqual(selection({ "per-thread": true }));
  });

  it("ranges a ThreadGroup-only run by day like its per-thread dependency", () => {
    expect(topKind(selection({ "per-thread": true, "thread-group": true }))).toBe("per-thread");
    expect(topKind(selection({ "thread-group": true, daily: true }))).toBe("daily");
  });
});
