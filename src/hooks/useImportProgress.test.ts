import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ImportStepUpdate } from "@/types/api";

// Stub the Tauri event API so `listen()` resolves without touching IPC; the
// hook's useEffect would otherwise hit an undefined window.__TAURI_INTERNALS__.
let captured: ((event: { payload: ImportStepUpdate }) => void) | null = null;
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((_event: string, callback: (event: { payload: ImportStepUpdate }) => void) => {
    captured = callback;
    return Promise.resolve(() => {
      captured = null;
    });
  }),
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

beforeEach(() => {
  captured = null;
});

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

describe("ImportStepUpdate notices", () => {
  it("shows an initial event even when it matches the default active step", async () => {
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});

    act(() => {
      captured?.({
        payload: {
          job_id: "job-initial-active",
          step: "thread-import",
          status: "active",
          message: null,
        },
      });
    });

    expect(result.current.snapshot?.job_id).toBe("job-initial-active");
    expect(result.current.busy).toBe(true);
  });

  it("keeps a completed step settled when a delayed active update arrives", async () => {
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});

    act(() => {
      captured?.({
        payload: {
          job_id: "job-delayed-active",
          step: "thread-import",
          status: "done",
          message: "インポート完了",
        },
      });
      captured?.({
        payload: {
          job_id: "job-delayed-active",
          step: "thread-import",
          status: "active",
          message: "読み込み中",
        },
      });
    });

    expect(result.current.snapshot?.steps["thread-import"]).toMatchObject({
      status: "done",
      message: "インポート完了",
    });
    expect(result.current.busy).toBe(false);
  });

  it.each(["warning", "failed"] as const)(
    "keeps a %s step settled when a delayed active update arrives",
    async (terminalStatus) => {
      const { result } = renderHook(() => useImportProgress());
      await act(async () => {});

      act(() => {
        captured?.({
          payload: {
            job_id: `job-delayed-${terminalStatus}`,
            step: "thread-import",
            status: terminalStatus,
            message: "処理終了",
          },
        });
        captured?.({
          payload: {
            job_id: `job-delayed-${terminalStatus}`,
            step: "thread-import",
            status: "active",
            message: "読み込み中",
          },
        });
      });

      expect(result.current.snapshot?.steps["thread-import"]).toMatchObject({
        status: terminalStatus,
        message: "処理終了",
      });
      expect(result.current.busy).toBe(false);
    },
  );

  it("keeps an immediately completed next job when reset follows its event", async () => {
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});
    act(() => result.current.reset("job-A"));

    act(() => {
      captured?.({
        payload: {
          job_id: "job-B",
          step: "thread-import",
          status: "active",
          message: null,
        },
      });
      captured?.({
        payload: {
          job_id: "job-B",
          step: "thread-import",
          status: "done",
          message: null,
          notice_code: "no-importable-log-sources",
        },
      });
    });

    // An event for a later dispatch must not replace the toast still showing
    // the earlier job before start_import returns its job id.
    expect(result.current.snapshot?.job_id).toBe("job-A");

    // start_import can resolve after the first event of the next job. Its
    // completion must remain visible when the caller associates the toast
    // with that job after the response arrives.
    act(() => result.current.reset("job-B"));

    expect(result.current.snapshot?.job_id).toBe("job-B");
    expect(result.current.snapshot?.steps["thread-import"]).toMatchObject({
      status: "done",
      notice_code: "no-importable-log-sources",
    });
    expect(result.current.busy).toBe(false);
  });

  it("keeps a no-importable-log-sources notice on the thread-import step", async () => {
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});
    expect(captured).not.toBeNull();

    act(() => {
      captured?.({
        payload: {
          job_id: "job-no-logs",
          step: "thread-import",
          status: "done",
          message: null,
          notice_code: "no-importable-log-sources",
        },
      });
    });

    expect(result.current.snapshot?.steps["thread-import"]).toMatchObject({
      status: "done",
      notice_code: "no-importable-log-sources",
    });
  });

  it("clears a previous notice when a later update has none", async () => {
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});

    act(() => {
      captured?.({
        payload: {
          job_id: "job-clear-notice",
          step: "thread-import",
          status: "done",
          message: null,
          notice_code: "no-importable-log-sources",
        },
      });
      captured?.({
        payload: {
          job_id: "job-clear-notice",
          step: "thread-import",
          status: "done",
          message: null,
        },
      });
    });

    expect(result.current.snapshot?.steps["thread-import"].notice_code).toBeUndefined();
  });

  it("keeps the snapshot reference for an identical update", async () => {
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});

    const update: ImportStepUpdate = {
      job_id: "job-no-op",
      step: "thread-import",
      status: "active",
      message: "読み込み中",
    };
    act(() => captured?.({ payload: update }));
    const firstSnapshot = result.current.snapshot;

    act(() => captured?.({ payload: update }));

    expect(result.current.snapshot).toBe(firstSnapshot);
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
