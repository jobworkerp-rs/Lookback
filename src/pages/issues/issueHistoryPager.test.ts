import { act, renderHook, waitFor } from "@testing-library/react";
import { createElement, type ReactNode, StrictMode } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { listIssueDirectHistoryPage } from "@/api";
import type { IssueHistoryPageOutcome } from "@/types/api";
import { useIssueHistoryPager } from "./issueHistoryPager";

vi.mock("@/api", () => ({
  listIssueDirectHistoryPage: vi.fn(),
}));

function page(ids: string[], next_page_token: string | null = null): IssueHistoryPageOutcome {
  return {
    kind: "page",
    page: {
      entries: ids.map((id) => ({
        id,
        created_at_ms: 0,
        actor: "tester",
        decision: "PENDING",
        target_title: "operation",
        issue_ids: ["issue-1"],
        rationale: null,
        state: "PENDING",
      })),
      next_page_token,
    },
  };
}

describe("useIssueHistoryPager", () => {
  beforeEach(() => {
    vi.mocked(listIssueDirectHistoryPage).mockReset();
  });

  it("loads a page and forwards only the opaque continuation token", async () => {
    vi.mocked(listIssueDirectHistoryPage)
      .mockResolvedValueOnce(page(["op-1"], "opaque-token"))
      .mockResolvedValueOnce(page(["op-2"]));
    const { result } = renderHook(() =>
      useIssueHistoryPager({ issueId: "issue-1", mode: "recovery", enabled: true }),
    );

    await waitFor(() => expect(result.current.entries.map((entry) => entry.id)).toEqual(["op-1"]));
    act(() => result.current.loadMore());
    await waitFor(() =>
      expect(result.current.entries.map((entry) => entry.id)).toEqual(["op-1", "op-2"]),
    );
    expect(vi.mocked(listIssueDirectHistoryPage)).toHaveBeenNthCalledWith(1, {
      issue_id: "issue-1",
      mode: "recovery",
      page_token: null,
    });
    expect(vi.mocked(listIssueDirectHistoryPage)).toHaveBeenNthCalledWith(2, {
      issue_id: "issue-1",
      mode: "recovery",
      page_token: "opaque-token",
    });
  });

  it("resets once on token expiry and stops after a second expiry", async () => {
    vi.mocked(listIssueDirectHistoryPage)
      .mockResolvedValueOnce(page(["op-1"], "opaque-token"))
      .mockResolvedValueOnce({ kind: "reset_required" })
      .mockResolvedValueOnce(page(["op-3"], "next-token"));
    const { result } = renderHook(() =>
      useIssueHistoryPager({ issueId: "issue-1", mode: "audit", enabled: true }),
    );
    await waitFor(() => expect(result.current.entries.map((entry) => entry.id)).toEqual(["op-1"]));
    act(() => result.current.loadMore());
    await waitFor(() => expect(result.current.entries.map((entry) => entry.id)).toEqual(["op-3"]));
    expect(result.current.resetNotice).toBe(true);

    vi.mocked(listIssueDirectHistoryPage).mockResolvedValueOnce({ kind: "reset_required" });
    act(() => result.current.loadMore());
    await waitFor(() => expect(result.current.error?.message).toContain("再読み込み"));
    expect(vi.mocked(listIssueDirectHistoryPage)).toHaveBeenCalledTimes(4);
  });

  it("does not start a disabled audit pager until it is enabled", async () => {
    vi.mocked(listIssueDirectHistoryPage).mockResolvedValue(page(["op-1"]));
    const { result, rerender } = renderHook(
      ({ enabled }) => useIssueHistoryPager({ issueId: "issue-1", mode: "audit", enabled }),
      { initialProps: { enabled: false } },
    );
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(listIssueDirectHistoryPage).not.toHaveBeenCalled();
    rerender({ enabled: true });
    await waitFor(() => expect(result.current.entries).toHaveLength(1));
  });

  it("deduplicates the first request during Strict Mode effect probing", async () => {
    let resolvePage: ((outcome: IssueHistoryPageOutcome) => void) | undefined;
    vi.mocked(listIssueDirectHistoryPage).mockImplementation(
      () =>
        new Promise((resolve) => {
          resolvePage = resolve;
        }),
    );
    const StrictModeWrapper = ({ children }: { children: ReactNode }) =>
      createElement(StrictMode, null, children);
    const { result } = renderHook(
      () => useIssueHistoryPager({ issueId: "issue-1", mode: "recovery", enabled: true }),
      { wrapper: StrictModeWrapper },
    );
    await waitFor(() => expect(listIssueDirectHistoryPage).toHaveBeenCalledTimes(1));
    await act(async () => resolvePage?.(page(["op-1"])));
    await waitFor(() => expect(result.current.entries).toHaveLength(1));
  });

  it("starts a fresh first-page request when reloading an in-flight page", async () => {
    let resolveFirst: ((outcome: IssueHistoryPageOutcome) => void) | undefined;
    vi.mocked(listIssueDirectHistoryPage)
      .mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            resolveFirst = resolve;
          }),
      )
      .mockResolvedValueOnce(page(["new-operation"]));
    const { result } = renderHook(() =>
      useIssueHistoryPager({ issueId: "issue-1", mode: "recovery", enabled: true }),
    );
    await waitFor(() => expect(listIssueDirectHistoryPage).toHaveBeenCalledTimes(1));

    act(() => result.current.reload());
    await waitFor(() => expect(listIssueDirectHistoryPage).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(result.current.entries[0]?.id).toBe("new-operation"));

    await act(async () => resolveFirst?.(page(["stale-operation"])));
    expect(result.current.entries.map((entry) => entry.id)).toEqual(["new-operation"]);
  });
});
