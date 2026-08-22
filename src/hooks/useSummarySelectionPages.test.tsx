import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { listReflectionSelection, listSummariesForSelection } from "@/api";
import type { SummaryEntry } from "@/types/api";
import { useSummarySelectionPages } from "./useSummarySelectionPages";

vi.mock("@/api", () => ({
  listReflectionSelection: vi.fn(),
  listSummariesForSelection: vi.fn(),
}));

const mockListSummariesForSelection = vi.mocked(listSummariesForSelection);

function entry(memory_id: string): SummaryEntry {
  return {
    memory_id,
    thread_id: "20",
    external_id: null,
    kind: "per-thread",
    period_key: null,
    scope_key: null,
    content_json: "summary",
    updated_at_ms: 1,
  };
}

beforeEach(() => {
  mockListSummariesForSelection.mockReset();
  vi.mocked(listReflectionSelection).mockReset();
});

describe("useSummarySelectionPages", () => {
  it("keeps an initial request active when the selected tab is clicked again", async () => {
    let resolveInitial:
      | ((page: { entries: SummaryEntry[]; next_offset: number | null }) => void)
      | undefined;
    mockListSummariesForSelection.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveInitial = resolve;
        }),
    );
    const { result } = renderHook(() => useSummarySelectionPages());

    act(() => result.current.selectKind("per-thread"));
    act(() => resolveInitial?.({ entries: [entry("loaded")], next_offset: null }));

    await waitFor(() => expect(result.current.entries).toEqual([entry("loaded")]));
    expect(result.current.loading).toBe(false);
  });

  it("keeps a load-more request active when the selected tab is clicked again", async () => {
    let resolveMore:
      | ((page: { entries: SummaryEntry[]; next_offset: number | null }) => void)
      | undefined;
    mockListSummariesForSelection
      .mockResolvedValueOnce({ entries: [entry("first")], next_offset: 1 })
      .mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            resolveMore = resolve;
          }),
      );
    const { result } = renderHook(() => useSummarySelectionPages());

    await waitFor(() => expect(result.current.hasMore).toBe(true));
    act(() => result.current.loadMore());
    act(() => result.current.selectKind("per-thread"));
    act(() => resolveMore?.({ entries: [entry("second")], next_offset: null }));

    await waitFor(() => expect(result.current.entries).toEqual([entry("first"), entry("second")]));
    expect(result.current.loadingMore).toBe(false);
  });

  it("ignores a pre-retry load-more success", async () => {
    let resolveMore:
      | ((page: { entries: SummaryEntry[]; next_offset: number | null }) => void)
      | undefined;
    mockListSummariesForSelection
      .mockResolvedValueOnce({ entries: [], next_offset: 1 })
      .mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            resolveMore = resolve;
          }),
      )
      .mockResolvedValueOnce({ entries: [entry("fresh")], next_offset: null });
    const { result } = renderHook(() => useSummarySelectionPages());

    await waitFor(() => expect(result.current.hasMore).toBe(true));
    act(() => result.current.loadMore());
    act(() => result.current.retry());
    await waitFor(() => expect(result.current.entries).toEqual([entry("fresh")]));
    act(() => resolveMore?.({ entries: [entry("stale")], next_offset: null }));

    await waitFor(() => expect(result.current.entries).toEqual([entry("fresh")]));
  });

  it("ignores a pre-retry load-more failure", async () => {
    let rejectMore: ((reason?: unknown) => void) | undefined;
    mockListSummariesForSelection
      .mockResolvedValueOnce({ entries: [], next_offset: 1 })
      .mockImplementationOnce(
        () =>
          new Promise((_, reject) => {
            rejectMore = reject;
          }),
      )
      .mockResolvedValueOnce({ entries: [entry("fresh")], next_offset: null });
    const { result } = renderHook(() => useSummarySelectionPages());

    await waitFor(() => expect(result.current.hasMore).toBe(true));
    act(() => result.current.loadMore());
    act(() => result.current.retry());
    await waitFor(() => expect(result.current.entries).toEqual([entry("fresh")]));
    act(() => rejectMore?.(new Error("stale failure")));

    await waitFor(() => expect(result.current.error).toBeNull());
  });
});
