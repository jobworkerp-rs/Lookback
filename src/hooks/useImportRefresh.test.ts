import { QueryClient } from "@tanstack/react-query";
import { renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { StepStatus } from "@/types/api";
import { defaultSnapshot, type ImportSnapshot } from "./useImportProgress";
import { useImportRefresh } from "./useImportRefresh";

function stepSnapshot(
  step: keyof ImportSnapshot["steps"],
  status: StepStatus,
  jobId = "import-1",
): ImportSnapshot {
  const snapshot = defaultSnapshot(jobId);
  snapshot.steps["thread-import"] = { status: "waiting", message: null };
  snapshot.steps[step] = { status, message: null };
  return snapshot;
}

function invalidatedKeys(spy: { mock: { calls: readonly (readonly unknown[])[] } }) {
  return spy.mock.calls.map(([arg]) => (arg as { queryKey: readonly unknown[] }).queryKey);
}

describe("useImportRefresh", () => {
  it("refreshes thread caches when thread import completes", () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const invalidate = vi.spyOn(client, "invalidateQueries");

    renderHook(() => useImportRefresh(client, stepSnapshot("thread-import", "done")));

    expect(invalidatedKeys(invalidate)).toEqual(
      expect.arrayContaining([["threads"], ["thread-search"], ["distinct-labels"]]),
    );
  });

  it("refreshes generated caches when downstream import steps complete", () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const invalidate = vi.spyOn(client, "invalidateQueries");

    renderHook(() => useImportRefresh(client, stepSnapshot("thread-summary", "done")));
    renderHook(() => useImportRefresh(client, stepSnapshot("thread-personality", "done")));
    renderHook(() => useImportRefresh(client, stepSnapshot("reflection", "done")));

    expect(invalidatedKeys(invalidate)).toEqual(
      expect.arrayContaining([
        ["summaries"],
        ["summary-search"],
        ["personality", 1],
        ["personality-signals"],
        ["reflections"],
      ]),
    );
  });

  it("refreshes generated caches when downstream import steps partially succeed", () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const invalidate = vi.spyOn(client, "invalidateQueries");

    renderHook(() => useImportRefresh(client, stepSnapshot("thread-summary", "warning")));
    renderHook(() => useImportRefresh(client, stepSnapshot("thread-personality", "warning")));
    renderHook(() => useImportRefresh(client, stepSnapshot("reflection", "warning")));

    expect(invalidatedKeys(invalidate)).toEqual(
      expect.arrayContaining([
        ["summaries"],
        ["personality", 1],
        ["personality-signals"],
        ["reflections"],
      ]),
    );
  });

  it("refreshes again when a new import job reaches the same terminal status", () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const invalidate = vi.spyOn(client, "invalidateQueries");

    const { rerender } = renderHook(({ snapshot }) => useImportRefresh(client, snapshot), {
      initialProps: {
        snapshot: stepSnapshot("thread-summary", "done", "import-1"),
      },
    });
    const firstCount = invalidate.mock.calls.length;

    rerender({ snapshot: stepSnapshot("thread-summary", "done", "import-2") });

    expect(invalidate.mock.calls.length).toBeGreaterThan(firstCount);
    expect(invalidatedKeys(invalidate)).toEqual(expect.arrayContaining([["summaries"]]));
  });
});
