import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

// Stub the Tauri event API so `listen()` resolves without touching IPC; the
// hook's useEffect would otherwise hit an undefined window.__TAURI_INTERNALS__.
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
}));

// Spy on the cancel API wrapper so the test asserts the hook forwards
// the snapshot's job id without booting the Tauri runtime.
const startImportCancelMock = vi.fn<(jobId: string) => Promise<void>>(() => Promise.resolve());
vi.mock("@/api", () => ({
  startImportCancel: (jobId: string) => startImportCancelMock(jobId),
}));

import {
  defaultSnapshot,
  IMPORT_STEPS,
  isImportBusy,
  useImportProgress,
} from "./useImportProgress";

describe("defaultSnapshot", () => {
  it("creates the four canonical steps", () => {
    const snap = defaultSnapshot("job-1");
    expect(Object.keys(snap.steps).sort()).toEqual([...IMPORT_STEPS].sort());
  });

  it("starts thread-import active and the rest waiting", () => {
    const snap = defaultSnapshot("job-1");
    expect(snap.steps["thread-import"].status).toBe("active");
    expect(snap.steps["thread-summary"].status).toBe("waiting");
    expect(snap.steps["thread-personality"].status).toBe("waiting");
    expect(snap.steps.reflection.status).toBe("waiting");
  });

  it("carries the job id through", () => {
    const snap = defaultSnapshot("xyz");
    expect(snap.job_id).toBe("xyz");
  });
});

describe("useImportProgress.reset", () => {
  it("initializes a snapshot when none exists", () => {
    const { result } = renderHook(() => useImportProgress());
    expect(result.current.snapshot).toBeNull();
    act(() => result.current.reset("job-A"));
    expect(result.current.snapshot?.job_id).toBe("job-A");
    expect(result.current.snapshot?.steps["thread-import"].status).toBe("active");
  });

  it("is a no-op when the snapshot already belongs to the same job", () => {
    const { result } = renderHook(() => useImportProgress());

    act(() => result.current.reset("job-A"));
    const initial = result.current.snapshot;
    expect(initial).not.toBeNull();

    act(() => result.current.reset("job-A"));
    // Same reference — React would otherwise replace the snapshot wholesale.
    expect(result.current.snapshot).toBe(initial);
  });

  it("replaces the snapshot when called with a different job id", () => {
    const { result } = renderHook(() => useImportProgress());
    act(() => result.current.reset("job-A"));
    act(() => result.current.reset("job-B"));
    expect(result.current.snapshot?.job_id).toBe("job-B");
  });
});

describe("isImportBusy", () => {
  it("is true while any step is still active", () => {
    const snap = defaultSnapshot("job-A");
    expect(isImportBusy(snap)).toBe(true);
  });

  it("flips to false once every step is terminal", () => {
    const snap = defaultSnapshot("job-A");
    for (const step of IMPORT_STEPS) {
      snap.steps[step] = { status: "done", message: null };
    }
    expect(isImportBusy(snap)).toBe(false);
  });

  it("is false for an empty (post-clear) snapshot", () => {
    expect(isImportBusy(null)).toBe(false);
  });
});

describe("useImportProgress.cancel", () => {
  it("forwards the snapshot's job id to startImportCancel", async () => {
    startImportCancelMock.mockClear();
    const { result } = renderHook(() => useImportProgress());
    act(() => result.current.reset("job-cancel-1"));
    await act(async () => {
      await result.current.cancel();
    });
    expect(startImportCancelMock).toHaveBeenCalledTimes(1);
    expect(startImportCancelMock).toHaveBeenCalledWith("job-cancel-1");
  });

  it("is a no-op when no run is in flight", async () => {
    startImportCancelMock.mockClear();
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {
      await result.current.cancel();
    });
    expect(startImportCancelMock).not.toHaveBeenCalled();
  });
});
