import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { AnalysisStepUpdate } from "@/types/api";

// Capture the handler `useTauriEvent` registers so the test can fire events
// manually, including the race where an event lands before `start` is called.
let captured: ((e: { payload: AnalysisStepUpdate }) => void) | null = null;
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((_event: string, cb: (e: { payload: AnalysisStepUpdate }) => void) => {
    captured = cb;
    return Promise.resolve(() => {
      captured = null;
    });
  }),
}));

// Spy on the cancel API wrapper — same pattern as useImportProgress.test.
const analysisCancelMock = vi.fn<(jobId: string) => Promise<void>>(() => Promise.resolve());
vi.mock("@/api", () => ({
  analysisCancel: (jobId: string) => analysisCancelMock(jobId),
}));

import { useStepStreamProgress } from "./useStepStreamProgress";

function emit(payload: AnalysisStepUpdate) {
  if (!captured) throw new Error("listener not registered yet");
  act(() => captured?.({ payload }));
}

describe("useStepStreamProgress", () => {
  beforeEach(() => {
    captured = null;
  });

  it("opens an active slot on start", () => {
    const { result } = renderHook(() => useStepStreamProgress("summary://step"));
    expect(result.current.progress).toBeNull();
    act(() => result.current.start("summary-1"));
    expect(result.current.progress).toEqual({
      job_id: "summary-1",
      status: "active",
      message: "dispatching",
    });
  });

  it("keeps a failed event that arrives before start (race) instead of dropping it", async () => {
    const { result } = renderHook(() => useStepStreamProgress("summary://step"));
    // Let the mocked listen() promise resolve so the handler is captured.
    await act(async () => {});

    // Rust emits `failed` before the awaited enqueue* promise resolves.
    emit({ job_id: "summary-1", status: "failed", message: "worker not registered" });
    expect(result.current.progress?.status).toBe("failed");

    // The later start() for the same job must NOT clobber the terminal state.
    act(() => result.current.start("summary-1"));
    expect(result.current.progress?.status).toBe("failed");
    expect(result.current.progress?.message).toBe("worker not registered");
  });

  it("start is idempotent for a job whose event already populated the slot", async () => {
    const { result } = renderHook(() => useStepStreamProgress("summary://step"));
    await act(async () => {});

    emit({ job_id: "summary-1", status: "done", message: "ok" });
    const populated = result.current.progress;
    act(() => result.current.start("summary-1"));
    // Same reference — start bailed out rather than resetting to active.
    expect(result.current.progress).toBe(populated);
  });

  it("dedupes repeated same-status/same-message chunks (same reference)", async () => {
    const { result } = renderHook(() => useStepStreamProgress("summary://step"));
    await act(async () => {});

    act(() => result.current.start("summary-1"));
    emit({ job_id: "summary-1", status: "active", message: "(1/3)" });
    const first = result.current.progress;
    emit({ job_id: "summary-1", status: "active", message: "(1/3)" });
    expect(result.current.progress).toBe(first);
  });

  it("ignores a different job's event while the current slot is still active", async () => {
    const { result } = renderHook(() => useStepStreamProgress("summary://step"));
    await act(async () => {});

    act(() => result.current.start("summary-1"));
    emit({ job_id: "summary-2", status: "done", message: "stale" });
    expect(result.current.progress?.job_id).toBe("summary-1");
    expect(result.current.progress?.status).toBe("active");
  });

  it("accepts a new job's fast event when the previous slot is terminal and unclosed", async () => {
    const { result } = renderHook(() => useStepStreamProgress("summary://step"));
    await act(async () => {});

    // First run completes but the user does not dismiss it.
    act(() => result.current.start("summary-1"));
    emit({ job_id: "summary-1", status: "done", message: "first" });
    expect(result.current.progress?.status).toBe("done");

    // Re-run: the new job's fast `failed` lands before its start() call. It
    // must replace the stale terminal slot, not be dropped as a different id.
    emit({ job_id: "summary-2", status: "failed", message: "second" });
    expect(result.current.progress).toEqual({
      job_id: "summary-2",
      status: "failed",
      message: "second",
    });

    // The later start() for the new job keeps the terminal state (idempotent).
    act(() => result.current.start("summary-2"));
    expect(result.current.progress?.status).toBe("failed");
    expect(result.current.progress?.message).toBe("second");
  });

  it("clear dismisses the slot", () => {
    const { result } = renderHook(() => useStepStreamProgress("summary://step"));
    act(() => result.current.start("summary-1"));
    act(() => result.current.clear());
    expect(result.current.progress).toBeNull();
  });

  it("busy is true while active and false once terminal", async () => {
    const { result } = renderHook(() => useStepStreamProgress("summary://step"));
    await act(async () => {});
    act(() => result.current.start("summary-1"));
    expect(result.current.busy).toBe(true);
    emit({ job_id: "summary-1", status: "done", message: "ok" });
    expect(result.current.busy).toBe(false);
  });

  it("cancel forwards the active job id to analysisCancel", async () => {
    analysisCancelMock.mockClear();
    const { result } = renderHook(() => useStepStreamProgress("summary://step"));
    await act(async () => {});
    act(() => result.current.start("summary-cancel"));
    await act(async () => {
      await result.current.cancel();
    });
    expect(analysisCancelMock).toHaveBeenCalledTimes(1);
    expect(analysisCancelMock).toHaveBeenCalledWith("summary-cancel");
  });

  it("cancel is a no-op when no dispatch is in flight", async () => {
    analysisCancelMock.mockClear();
    const { result } = renderHook(() => useStepStreamProgress("summary://step"));
    await act(async () => {});
    await act(async () => {
      await result.current.cancel();
    });
    expect(analysisCancelMock).not.toHaveBeenCalled();
  });
});
