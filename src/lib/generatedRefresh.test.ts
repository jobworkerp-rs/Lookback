import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import { refreshGeneratedCaches, refreshImportedThreadCaches } from "./generatedRefresh";

function clientWithSpies() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const invalidate = vi.spyOn(client, "invalidateQueries");
  return { client, invalidate };
}

function invalidatedKeys(spy: { mock: { calls: readonly (readonly unknown[])[] } }) {
  return spy.mock.calls.map(([arg]) => (arg as { queryKey: readonly unknown[] }).queryKey);
}

describe("refreshGeneratedCaches", () => {
  it("refreshes thread and summary caches for per-thread summary output", () => {
    const { client, invalidate } = clientWithSpies();

    refreshGeneratedCaches(client, ["thread_summary"]);

    expect(invalidatedKeys(invalidate)).toEqual(
      expect.arrayContaining([
        ["threads"],
        ["thread-search"],
        ["distinct-labels"],
        ["co-occurring-labels"],
        ["summaries"],
        ["count-summaries"],
        ["summary-search"],
        ["summary-hit"],
        ["memories"],
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
        ["count-summaries"],
        ["summary-hit"],
      ]),
    );
    expect(invalidatedKeys(invalidate)).not.toContainEqual(["threads"]);
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
      expect.arrayContaining([["reflections"], ["memories"]]),
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
        ["thread-search"],
        ["distinct-labels"],
        ["co-occurring-labels"],
        ["personality", 1],
        ["memories"],
      ]),
    );
  });
});
