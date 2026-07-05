import { QueryClient } from "@tanstack/react-query";
import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { GeneratedRefreshEvent } from "@/types/api";

let captured: ((e: { payload: GeneratedRefreshEvent }) => void) | null = null;

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((_event: string, cb: (e: { payload: GeneratedRefreshEvent }) => void) => {
    captured = cb;
    return Promise.resolve(() => {
      captured = null;
    });
  }),
}));

import { useGeneratedRefresh } from "./useGeneratedRefresh";

function emit(payload: GeneratedRefreshEvent) {
  if (!captured) throw new Error("listener not registered yet");
  act(() => captured?.({ payload }));
}

describe("useGeneratedRefresh", () => {
  beforeEach(() => {
    captured = null;
  });

  it("invalidates caches from generated refresh events", async () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const invalidate = vi.spyOn(client, "invalidateQueries");

    renderHook(() => useGeneratedRefresh(client));
    await act(async () => {});

    emit({ job_id: "summary-1", scopes: ["thread_summary"] });

    expect(invalidate).toHaveBeenCalledWith({ queryKey: ["summaries"] });
    expect(invalidate).toHaveBeenCalledWith({ queryKey: ["threads"] });
  });
});
