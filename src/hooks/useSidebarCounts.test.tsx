import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { countActiveThreadGroups, countThreadGroupSummaries, getPersonality } from "@/api";
import { refreshGeneratedCaches } from "@/lib/generatedRefresh";
import { SIDEBAR_COUNT_SAFETY_REFETCH_MS, useSidebarCounts } from "./useSidebarCounts";

vi.mock("@/api", () => ({
  getPersonality: vi.fn(),
  countThreadGroupSummaries: vi.fn(),
  countActiveThreadGroups: vi.fn(),
}));

const personality = vi.mocked(getPersonality);
const summaries = vi.mocked(countThreadGroupSummaries);
const groups = vi.mocked(countActiveThreadGroups);

function setup() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  const hook = renderHook(({ enabled }) => useSidebarCounts(enabled), {
    wrapper,
    initialProps: { enabled: true },
  });
  return { client, hook };
}

describe("useSidebarCounts", () => {
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    personality.mockResolvedValue({ thread_count: 3, thread_count_truncated: false } as never);
    summaries.mockResolvedValue(2);
    groups.mockResolvedValue(1);
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.clearAllMocks();
  });

  it("loads all counts once and exposes them", async () => {
    const { hook } = setup();
    await waitFor(() => expect(hook.result.current.threadGroupCount).toBe(1));
    expect(hook.result.current.threadCount).toBe(3);
    expect(hook.result.current.threadCountTruncated).toBe(false);
    expect(hook.result.current.summaryCount).toBe(2);
    expect(personality).toHaveBeenCalledTimes(1);
    expect(summaries).toHaveBeenCalledTimes(1);
    expect(groups).toHaveBeenCalledTimes(1);
  });

  it("does not poll on a short interval; events drive refreshes instead", async () => {
    const { hook } = setup();
    await waitFor(() => expect(hook.result.current.summaryCount).toBe(2));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
    expect(personality).toHaveBeenCalledTimes(1);
    expect(summaries).toHaveBeenCalledTimes(1);
    expect(groups).toHaveBeenCalledTimes(1);
  });

  it("refetches when a generated-refresh event invalidates the counts", async () => {
    const { client, hook } = setup();
    await waitFor(() => expect(hook.result.current.threadGroupCount).toBe(1));
    groups.mockResolvedValue(5);
    act(() => refreshGeneratedCaches(client, ["thread_group_summary"]));
    await waitFor(() => expect(hook.result.current.threadGroupCount).toBe(5));
  });

  it("still refetches on the safety interval", async () => {
    const { hook } = setup();
    await waitFor(() => expect(hook.result.current.summaryCount).toBe(2));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(SIDEBAR_COUNT_SAFETY_REFETCH_MS + 1_000);
    });
    expect(summaries.mock.calls.length).toBeGreaterThanOrEqual(2);
  });

  it("does not fetch until the sidecar is ready", () => {
    const client = new QueryClient();
    const wrapper = ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={client}>{children}</QueryClientProvider>
    );
    renderHook(() => useSidebarCounts(false), { wrapper });
    expect(personality).not.toHaveBeenCalled();
    expect(summaries).not.toHaveBeenCalled();
    expect(groups).not.toHaveBeenCalled();
  });
});
