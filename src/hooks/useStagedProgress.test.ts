import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { StagedProgressKind, StagedProgressSnapshot } from "@/types/api";

const { handlers, getActiveStagedProgressMock, analysisCancelMock, startImportCancelMock } =
  vi.hoisted(() => ({
    handlers: new Map<string, ((event: { payload: unknown }) => void)[]>(),
    getActiveStagedProgressMock: vi.fn(),
    analysisCancelMock: vi.fn((..._args: unknown[]) => Promise.resolve()),
    startImportCancelMock: vi.fn((..._args: unknown[]) => Promise.resolve()),
  }));
const { incrementUiMemoryCounterMock } = vi.hoisted(() => ({
  incrementUiMemoryCounterMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((event: string, callback: (event: { payload: unknown }) => void) => {
    handlers.set(event, [...(handlers.get(event) ?? []), callback]);
    return Promise.resolve(() => {
      handlers.set(
        event,
        (handlers.get(event) ?? []).filter((handler) => handler !== callback),
      );
    });
  }),
}));

vi.mock("@/api", () => ({
  getActiveStagedProgress: (kind: StagedProgressKind) => getActiveStagedProgressMock(kind),
  analysisCancel: (jobId: string) => analysisCancelMock(jobId),
  startImportCancel: (jobId: string) => startImportCancelMock(jobId),
}));
vi.mock("@/lib/uiMemoryDiagnostics", () => ({
  incrementUiMemoryCounter: incrementUiMemoryCounterMock,
}));

import { SUMMARY_GENERATION_STEPS, useStagedProgress } from "./useStagedProgress";

function summarySnapshot(
  jobId: string,
  sequence: number,
  overrides: Partial<StagedProgressSnapshot<"summary-generation">["steps"]> = {},
): StagedProgressSnapshot<"summary-generation"> {
  const steps = Object.fromEntries(
    SUMMARY_GENERATION_STEPS.map((step) => [step, { status: "waiting", message: null }]),
  ) as StagedProgressSnapshot<"summary-generation">["steps"];
  Object.assign(steps, overrides);
  return {
    kind: "summary-generation",
    job_id: jobId,
    sequence,
    steps,
  };
}

function emit(event: string, payload: unknown) {
  act(() => {
    for (const handler of handlers.get(event) ?? []) handler({ payload });
  });
}

beforeEach(() => {
  handlers.clear();
  getActiveStagedProgressMock.mockReset().mockResolvedValue(null);
  analysisCancelMock.mockClear();
  startImportCancelMock.mockClear();
  incrementUiMemoryCounterMock.mockClear();
});

describe("useStagedProgress", () => {
  it("initializes the five summary stages as waiting and binds reset to the dispatch id", () => {
    const { result } = renderHook(() => useStagedProgress("summary-generation"));

    act(() => result.current.reset("dispatch-new"));

    expect(result.current.snapshot).toMatchObject({
      kind: "summary-generation",
      job_id: "dispatch-new",
      sequence: 0,
      steps: Object.fromEntries(
        SUMMARY_GENERATION_STEPS.map((step) => [step, { status: "waiting" }]),
      ),
    });
    expect(result.current.busy).toBe(true);
  });

  it("restores a common staged snapshot for the requested kind", async () => {
    const restored = summarySnapshot("dispatch-restored", 7, {
      "thread-summary": { status: "done", message: null },
      "thread-group-summary": { status: "active", message: "(2/5) グループ要約" },
    });
    getActiveStagedProgressMock.mockResolvedValue(restored);

    const { result } = renderHook(() => useStagedProgress("summary-generation"));
    await act(async () => {});

    expect(getActiveStagedProgressMock).toHaveBeenCalledWith("summary-generation");
    expect(result.current.snapshot).toEqual(restored);
    expect(result.current.busy).toBe(true);
  });

  it("does not let a hydration response for the old run overwrite a pre-dispatch reset", async () => {
    let resolveHydration: ((snapshot: StagedProgressSnapshot | null) => void) | undefined;
    getActiveStagedProgressMock.mockReturnValue(
      new Promise((resolve) => {
        resolveHydration = resolve;
      }),
    );

    const { result } = renderHook(() => useStagedProgress("summary-generation"));
    await act(async () => {});
    act(() => result.current.reset("dispatch-current"));

    await act(async () => {
      resolveHydration?.(summarySnapshot("dispatch-old", 12));
    });

    expect(result.current.snapshot?.job_id).toBe("dispatch-current");
  });

  it("ignores other ids and older sequences without replacing the current run", async () => {
    const { result } = renderHook(() => useStagedProgress("summary-generation"));
    await act(async () => {});
    act(() => result.current.reset("dispatch-current"));

    emit("staged://step", summarySnapshot("dispatch-old", 50));
    emit(
      "staged://step",
      summarySnapshot("dispatch-current", 3, {
        "thread-summary": { status: "active", message: "(1/3)" },
      }),
    );
    const newest = result.current.snapshot;
    emit(
      "staged://step",
      summarySnapshot("dispatch-current", 2, {
        "thread-summary": { status: "done", message: "stale" },
      }),
    );

    expect(result.current.snapshot).toBe(newest);
    expect(result.current.snapshot?.job_id).toBe("dispatch-current");
    expect(result.current.snapshot?.steps["thread-summary"]).toEqual({
      status: "active",
      message: "(1/3)",
    });
  });

  it("counts summary snapshot receipts before filtering and only counts accepted publications", async () => {
    const { result } = renderHook(() => useStagedProgress("summary-generation"));
    await act(async () => {});
    act(() => result.current.reset("dispatch-current"));
    incrementUiMemoryCounterMock.mockClear();

    emit(
      "staged://step",
      summarySnapshot("dispatch-current", 3, {
        "thread-summary": { status: "active", message: "(1/3)" },
      }),
    );
    emit("staged://step", summarySnapshot("dispatch-current", 2));
    emit("staged://step", summarySnapshot("dispatch-other", 50));
    act(() => {
      result.current.reset("dispatch-reset");
      result.current.clear();
    });

    expect(incrementUiMemoryCounterMock.mock.calls).toEqual([
      ["staged_received_total"],
      ["staged_published_total"],
      ["staged_received_total"],
      ["staged_received_total"],
    ]);
  });

  it("counts summary hydration but excludes import progress", async () => {
    const restored = summarySnapshot("dispatch-restored", 7);
    getActiveStagedProgressMock.mockResolvedValue(restored);
    const { result } = renderHook(() => useStagedProgress("summary-generation"));
    await act(async () => {});

    expect(result.current.snapshot).toEqual(restored);
    expect(incrementUiMemoryCounterMock.mock.calls).toEqual([
      ["staged_received_total"],
      ["staged_published_total"],
    ]);

    incrementUiMemoryCounterMock.mockClear();
    const importHook = renderHook(() => useStagedProgress("import"));
    await act(async () => {});
    incrementUiMemoryCounterMock.mockClear();
    emit("staged://step", {
      kind: "import",
      job_id: "import-job",
      sequence: 1,
      steps: { "thread-import": { status: "active", message: "Importing" } },
    });
    expect(incrementUiMemoryCounterMock).not.toHaveBeenCalled();
    importHook.unmount();
  });

  it("does not clear a pre-reset run after a matching progress event raced an invoke error", async () => {
    const { result } = renderHook(() => useStagedProgress("summary-generation"));
    await act(async () => {});
    act(() => result.current.reset("dispatch-raced"));
    emit(
      "staged://step",
      summarySnapshot("dispatch-raced", 1, {
        "thread-summary": { status: "active", message: "(1/2)" },
      }),
    );

    act(() => result.current.clear("dispatch-raced"));

    expect(result.current.snapshot?.job_id).toBe("dispatch-raced");
    expect(result.current.snapshot?.steps["thread-summary"].status).toBe("active");
  });

  it("keeps import and summary-generation snapshots independent", async () => {
    const { result } = renderHook(() => ({
      import: useStagedProgress("import"),
      summary: useStagedProgress("summary-generation"),
    }));
    await act(async () => {});
    act(() => {
      result.current.import.reset("import-id");
      result.current.summary.reset("summary-id");
    });

    emit("staged://step", summarySnapshot("summary-id", 1));

    expect(result.current.import.snapshot?.job_id).toBe("import-id");
    expect(result.current.summary.snapshot?.job_id).toBe("summary-id");
    expect(result.current.import.snapshot?.kind).toBe("import");
    expect(result.current.summary.snapshot?.kind).toBe("summary-generation");
  });

  it("cancels summary generation with its dispatch id", async () => {
    const { result } = renderHook(() => useStagedProgress("summary-generation"));
    act(() => result.current.reset("dispatch-cancel"));

    await act(async () => result.current.cancel());

    expect(analysisCancelMock).toHaveBeenCalledWith("dispatch-cancel");
    expect(startImportCancelMock).not.toHaveBeenCalled();
  });
});
