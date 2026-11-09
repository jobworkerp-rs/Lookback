import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ImportStep, ImportStepState, StagedProgressSnapshot } from "@/types/api";

// Stub the Tauri event API so `listen()` resolves without touching IPC; the
// hook's useEffect would otherwise hit an undefined window.__TAURI_INTERNALS__.
const { listenedEvents, getActiveStagedProgressMock } = vi.hoisted(() => ({
  listenedEvents: [] as string[],
  getActiveStagedProgressMock: vi.fn<(kind: string) => Promise<StagedProgressSnapshot | null>>(),
}));
let capturedStaged: ((event: { payload: StagedProgressSnapshot }) => void) | null = null;
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((event: string, callback: (event: { payload: StagedProgressSnapshot }) => void) => {
    listenedEvents.push(event);
    if (event === "staged://step") capturedStaged = callback;
    return Promise.resolve(() => {
      if (event === "staged://step") capturedStaged = null;
    });
  }),
}));

// Spy on the cancel API wrapper so the test asserts the hook forwards
// the snapshot's job id without booting the Tauri runtime.
const startImportCancelMock = vi.fn<(jobId: string) => Promise<void>>(() => Promise.resolve());
vi.mock("@/api", () => ({
  startImportCancel: (jobId: string) => startImportCancelMock(jobId),
  getActiveStagedProgress: (kind: string) => getActiveStagedProgressMock(kind),
}));

import {
  defaultSnapshot,
  IMPORT_STEPS,
  isImportBusy,
  useImportProgress,
} from "./useImportProgress";

beforeEach(() => {
  capturedStaged = null;
  listenedEvents.length = 0;
  getActiveStagedProgressMock.mockReset().mockResolvedValue(null);
});

/** An import snapshot as the backend registry publishes it: every step
 *  present, unspecified steps still waiting. */
function importSnapshot(
  jobId: string,
  sequence: number,
  overrides: Partial<Record<ImportStep, ImportStepState>> = {},
): StagedProgressSnapshot<"import"> {
  const steps = Object.fromEntries(
    IMPORT_STEPS.map((step) => [step, { status: "waiting", message: null }]),
  ) as StagedProgressSnapshot<"import">["steps"];
  Object.assign(steps, overrides);
  return { kind: "import", job_id: jobId, sequence, steps };
}

function skippedAfterImport(
  status: ImportStepState["status"],
  message: string,
): Partial<Record<ImportStep, ImportStepState>> {
  return Object.fromEntries(IMPORT_STEPS.slice(1).map((step) => [step, { status, message }]));
}

function emitStaged(payload: StagedProgressSnapshot) {
  capturedStaged?.({ payload });
}

describe("defaultSnapshot", () => {
  it("includes the group summary between summary and personality", () => {
    expect(IMPORT_STEPS).toEqual([
      "thread-import",
      "thread-summary",
      "thread-group-summary",
      "thread-personality",
      "reflection",
    ]);
    expect(defaultSnapshot("job-group").steps["thread-group-summary"].status).toBe("waiting");
  });
  it("creates the five canonical steps", () => {
    const snap = defaultSnapshot("job-1");
    expect(Object.keys(snap.steps).sort()).toEqual([...IMPORT_STEPS].sort());
  });

  it("starts thread-import active and the rest waiting", () => {
    const snap = defaultSnapshot("job-1");
    expect(snap.steps["thread-import"].status).toBe("active");
    expect(snap.steps["thread-summary"].status).toBe("waiting");
    expect(snap.steps["thread-group-summary"].status).toBe("waiting");
    expect(snap.steps["thread-personality"].status).toBe("waiting");
    expect(snap.steps.reflection.status).toBe("waiting");
  });

  it("carries the job id through", () => {
    const snap = defaultSnapshot("xyz");
    expect(snap.job_id).toBe("xyz");
  });
});

describe("import progress events", () => {
  it("listens only to the common staged progress event", async () => {
    renderHook(() => useImportProgress());
    await act(async () => {});

    expect(listenedEvents).toEqual(["staged://step"]);
  });

  it("restores an in-flight import through the common restore API", async () => {
    const restored = importSnapshot("import-restored", 8, {
      "thread-import": { status: "done", message: null },
      "thread-summary": { status: "done", message: null },
      "thread-group-summary": { status: "active", message: "(1/2) グループ要約" },
    });
    getActiveStagedProgressMock.mockResolvedValue(restored);

    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});

    expect(getActiveStagedProgressMock).toHaveBeenCalledTimes(1);
    expect(getActiveStagedProgressMock).toHaveBeenCalledWith("import");
    expect(result.current.snapshot).toEqual(restored);
  });

  it("stays empty when no import is in flight", async () => {
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});

    expect(result.current.snapshot).toBeNull();
  });

  it("consumes common staged snapshots only for the import kind", async () => {
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});

    act(() => {
      emitStaged({
        kind: "summary-generation",
        job_id: "summary-1",
        sequence: 1,
        steps: {
          "thread-summary": { status: "waiting", message: null },
          "thread-group-summary": { status: "waiting", message: null },
          daily: { status: "waiting", message: null },
          weekly: { status: "waiting", message: null },
          monthly: { status: "waiting", message: null },
        },
      });
    });
    expect(result.current.snapshot).toBeNull();

    act(() => {
      emitStaged(
        importSnapshot("import-common", 4, {
          "thread-import": { status: "done", message: null },
          "thread-summary": { status: "active", message: "(1/3) 要約中" },
        }),
      );
    });

    expect(result.current.snapshot).toMatchObject({
      kind: "import",
      job_id: "import-common",
      sequence: 4,
      steps: { "thread-summary": { status: "active", message: "(1/3) 要約中" } },
    });
  });

  it("keeps the import busy during group summarization and advances to personality", async () => {
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});
    const done = { status: "done", message: null } as const;
    act(() => {
      emitStaged(
        importSnapshot("job-group", 3, {
          "thread-import": done,
          "thread-summary": done,
          "thread-group-summary": { status: "active", message: "グループを要約中" },
        }),
      );
    });
    expect(result.current.snapshot?.steps["thread-group-summary"].status).toBe("active");
    expect(result.current.busy).toBe(true);
    act(() => {
      emitStaged(
        importSnapshot("job-group", 5, {
          "thread-import": done,
          "thread-summary": done,
          "thread-group-summary": done,
          "thread-personality": { status: "active", message: null },
        }),
      );
    });
    expect(result.current.snapshot?.steps["thread-group-summary"].status).toBe("done");
    expect(result.current.snapshot?.steps["thread-personality"].status).toBe("active");
  });

  it("restores completed stages when reflection is the first received snapshot", async () => {
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});
    const done = { status: "done", message: null } as const;
    act(() => {
      emitStaged(
        importSnapshot("job-reflection", 4, {
          "thread-import": done,
          "thread-summary": done,
          "thread-group-summary": done,
          "thread-personality": done,
          reflection: { status: "active", message: "(1/2) 自省を生成中" },
        }),
      );
    });
    expect(result.current.snapshot?.steps["thread-import"].status).toBe("done");
    expect(result.current.snapshot?.steps["thread-summary"].status).toBe("done");
    expect(result.current.snapshot?.steps["thread-personality"].status).toBe("done");
    expect(result.current.snapshot?.steps.reflection).toMatchObject({
      status: "active",
      message: "(1/2) 自省を生成中",
    });
  });

  it("shows an initial event even when it matches the default active step", async () => {
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});

    act(() => {
      emitStaged(
        importSnapshot("job-initial-active", 1, {
          "thread-import": { status: "active", message: null },
        }),
      );
    });

    expect(result.current.snapshot?.job_id).toBe("job-initial-active");
    expect(result.current.busy).toBe(true);
  });

  it.each(["done", "warning", "failed", "cancelled"] as const)(
    "keeps a %s step settled when an older snapshot arrives late",
    async (terminalStatus) => {
      const { result } = renderHook(() => useImportProgress());
      await act(async () => {});
      const jobId = `job-delayed-${terminalStatus}`;

      act(() => {
        emitStaged(
          importSnapshot(jobId, 6, {
            "thread-import": { status: terminalStatus, message: "処理終了" },
            ...skippedAfterImport("cancelled", "中断によりスキップ"),
          }),
        );
        emitStaged(
          importSnapshot(jobId, 1, {
            "thread-import": { status: "active", message: "読み込み中" },
          }),
        );
      });

      expect(result.current.snapshot?.sequence).toBe(6);
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
      emitStaged(
        importSnapshot("job-B", 1, { "thread-import": { status: "active", message: null } }),
      );
      emitStaged(
        importSnapshot("job-B", 6, {
          "thread-import": {
            status: "done",
            message: null,
            notice_code: "no-importable-log-sources",
          },
          ...skippedAfterImport("waiting", "スキップ"),
        }),
      );
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
    expect(capturedStaged).not.toBeNull();

    act(() => {
      emitStaged(
        importSnapshot("job-no-logs", 1, {
          "thread-import": {
            status: "done",
            message: null,
            notice_code: "no-importable-log-sources",
          },
        }),
      );
    });

    expect(result.current.snapshot?.steps["thread-import"]).toMatchObject({
      status: "done",
      notice_code: "no-importable-log-sources",
    });
  });

  it("clears a previous notice when a later snapshot has none", async () => {
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});

    act(() => {
      emitStaged(
        importSnapshot("job-clear-notice", 1, {
          "thread-import": {
            status: "done",
            message: null,
            notice_code: "no-importable-log-sources",
          },
        }),
      );
      emitStaged(
        importSnapshot("job-clear-notice", 2, {
          "thread-import": { status: "done", message: null },
        }),
      );
    });

    expect(result.current.snapshot?.steps["thread-import"].notice_code).toBeUndefined();
  });

  it("keeps the snapshot reference when an already accepted sequence is redelivered", async () => {
    const { result } = renderHook(() => useImportProgress());
    await act(async () => {});

    const snapshot = importSnapshot("job-no-op", 1, {
      "thread-import": { status: "active", message: "読み込み中" },
    });
    act(() => emitStaged(snapshot));
    const firstSnapshot = result.current.snapshot;

    act(() => emitStaged({ ...snapshot }));

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
  it("remains busy between completed and not-yet-started stages", () => {
    const snap = defaultSnapshot("job-between");
    snap.steps["thread-import"] = { status: "done", message: null };
    snap.steps["thread-summary"] = { status: "done", message: null };
    snap.steps["thread-group-summary"] = { status: "done", message: null };
    expect(isImportBusy(snap)).toBe(true);
  });

  it("does not treat deliberately skipped waiting stages as busy", () => {
    const snap = defaultSnapshot("job-skipped");
    snap.steps["thread-import"] = { status: "done", message: null };
    for (const step of IMPORT_STEPS.slice(1)) {
      snap.steps[step] = { status: "waiting", message: "スキップ" };
    }
    expect(isImportBusy(snap)).toBe(false);
  });
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

  it("treats cancelled steps as terminal", () => {
    const snap = defaultSnapshot("job-cancelled");
    for (const step of IMPORT_STEPS) {
      snap.steps[step] = { status: "cancelled", message: "中断" };
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
