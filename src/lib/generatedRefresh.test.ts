import { QueryClient } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";

const { incrementUiMemoryCounterMock } = vi.hoisted(() => ({
  incrementUiMemoryCounterMock: vi.fn(),
}));

vi.mock("@/lib/uiMemoryDiagnostics", () => ({
  incrementUiMemoryCounter: incrementUiMemoryCounterMock,
}));

import { refreshGeneratedCaches, refreshImportedThreadCaches } from "./generatedRefresh";

beforeEach(() => {
  incrementUiMemoryCounterMock.mockClear();
});

function clientWithSpies() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const invalidate = vi.spyOn(client, "invalidateQueries");
  return { client, invalidate };
}

function invalidatedKeys(spy: { mock: { calls: readonly (readonly unknown[])[] } }) {
  return spy.mock.calls.map(([arg]) => (arg as { queryKey: readonly unknown[] }).queryKey);
}

describe("refreshGeneratedCaches", () => {
  it("counts each actual summary-list invalidation request and no unrelated key", () => {
    const { client } = clientWithSpies();

    refreshGeneratedCaches(client, ["thread_summary", "thread_group_summary"]);

    expect(incrementUiMemoryCounterMock.mock.calls).toEqual([
      ["summaries_invalidated_total"],
      ["summaries_invalidated_total"],
    ]);
  });

  it("refreshes public summary caches when per-thread inputs are regenerated", () => {
    const { client, invalidate } = clientWithSpies();

    refreshGeneratedCaches(client, ["thread_summary"]);

    expect(invalidatedKeys(invalidate)).toEqual(
      expect.arrayContaining([
        ["threads"],
        ["thread-search"],
        ["distinct-labels"],
        ["co-occurring-labels"],
        ["summaries"],
        ["count-thread-group-summaries"],
        ["summary-search"],
        ["summary-hit"],
        ["memories"],
        ["thread-row"],
        ["memory-reference"],
      ]),
    );
  });

  it("refreshes period summary caches without touching thread browse caches", () => {
    const { client, invalidate } = clientWithSpies();

    refreshGeneratedCaches(client, ["daily_summary", "weekly_summary"]);

    expect(invalidatedKeys(invalidate)).toEqual(
      expect.arrayContaining([
        ["summaries"],
        ["summary-period-keys"],
        ["summary-search"],
        ["count-thread-group-summaries"],
        ["summary-hit"],
        ["summary-recovery-status"],
      ]),
    );
    expect(invalidatedKeys(invalidate)).not.toContainEqual(["threads"]);
  });

  it("refreshes group and summary caches for group-summary output", () => {
    const { client, invalidate } = clientWithSpies();

    refreshGeneratedCaches(client, ["thread_group_summary"]);

    expect(invalidatedKeys(invalidate)).toEqual(
      expect.arrayContaining([
        ["thread-groups"],
        ["thread-group"],
        ["thread-group-summary"],
        ["count-thread-groups"],
        ["summaries"],
        ["count-thread-group-summaries"],
      ]),
    );
  });

  it("refreshes personality profile and signal caches", () => {
    const { client, invalidate } = clientWithSpies();

    refreshGeneratedCaches(client, ["personality"]);

    expect(invalidatedKeys(invalidate)).toEqual(
      expect.arrayContaining([["personality", 1], ["personality-signals"], ["memories"]]),
    );
  });

  it("refreshes reflection caches", () => {
    const { client, invalidate } = clientWithSpies();

    refreshGeneratedCaches(client, ["reflection"]);

    expect(invalidatedKeys(invalidate)).toEqual(
      expect.arrayContaining([["reflections"], ["reflection-hybrid-search"], ["memories"]]),
    );
  });
});

describe("refreshImportedThreadCaches", () => {
  it("refreshes imported thread, search, label, count, and detail caches", () => {
    const { client, invalidate } = clientWithSpies();

    refreshImportedThreadCaches(client);

    expect(invalidatedKeys(invalidate)).toEqual(
      expect.arrayContaining([
        ["threads"],
        ["thread-groups"],
        ["count-thread-groups"],
        ["thread-search"],
        ["distinct-labels"],
        ["co-occurring-labels"],
        ["personality", 1],
        ["memories"],
        ["thread-row"],
        ["memory-reference"],
      ]),
    );
  });
});
