import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

const MAX_COUNTER = Number.MAX_SAFE_INTEGER;
const SAMPLE_COMMAND = "report_ui_activity_sample";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

async function freshDiagnostics() {
  vi.resetModules();
  return import("./uiMemoryDiagnostics");
}

async function flushPromises() {
  await vi.advanceTimersByTimeAsync(0);
}

afterEach(() => {
  vi.clearAllTimers();
  vi.useRealTimers();
});

describe("UI memory diagnostics", () => {
  it("sends only the fixed zeroed counters initially and emits no normal log", async () => {
    vi.useFakeTimers();
    const { installUiMemoryDiagnostics } = await freshDiagnostics();
    const invoke = vi.fn().mockResolvedValue(false);
    const log = vi.spyOn(console, "log").mockImplementation(() => {});
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const error = vi.spyOn(console, "error").mockImplementation(() => {});

    const diagnostics = installUiMemoryDiagnostics(invoke);
    await flushPromises();

    expect(invoke).toHaveBeenCalledExactlyOnceWith(SAMPLE_COMMAND, {
      counters: {
        staged_received_total: 0,
        staged_published_total: 0,
        generated_refresh_total: 0,
        summaries_invalidated_total: 0,
        summary_list_render_attempts_total: 0,
        summary_card_render_attempts_total: 0,
      },
    });
    expect(vi.getTimerCount()).toBe(0);
    expect(log).not.toHaveBeenCalled();
    expect(warn).not.toHaveBeenCalled();
    expect(error).not.toHaveBeenCalled();
    diagnostics.dispose();
  });

  it("keeps six numeric counters at fixed size, saturates, and does not trigger React renders", async () => {
    vi.useFakeTimers();
    const { incrementUiMemoryCounter, installUiMemoryDiagnostics } = await freshDiagnostics();
    const renders = vi.fn();

    act(() => {
      renderHook(() => {
        incrementUiMemoryCounter("summary_list_render_attempts_total");
        renders();
      });
    });
    for (let index = 0; index < 10_000; index += 1) {
      incrementUiMemoryCounter("generated_refresh_total");
    }
    incrementUiMemoryCounter("staged_received_total", MAX_COUNTER);
    incrementUiMemoryCounter("staged_received_total");

    const invoke = vi.fn().mockResolvedValue(false);
    const diagnostics = installUiMemoryDiagnostics(invoke);
    await flushPromises();

    const counters = invoke.mock.calls[0]?.[1].counters;
    expect(Object.keys(counters ?? {}).sort()).toEqual(
      [
        "staged_received_total",
        "staged_published_total",
        "generated_refresh_total",
        "summaries_invalidated_total",
        "summary_list_render_attempts_total",
        "summary_card_render_attempts_total",
      ].sort(),
    );
    expect(counters).toEqual({
      staged_received_total: MAX_COUNTER,
      staged_published_total: 0,
      generated_refresh_total: 10_000,
      summaries_invalidated_total: 0,
      summary_list_render_attempts_total: 1,
      summary_card_render_attempts_total: 0,
    });
    expect(Object.values(counters ?? {}).every(Number.isSafeInteger)).toBe(true);
    expect(renders).toHaveBeenCalledTimes(1);
    diagnostics.dispose();
  });

  it("allows one in-flight sample and sends the latest counters at the next free tick", async () => {
    vi.useFakeTimers();
    const { incrementUiMemoryCounter, installUiMemoryDiagnostics } = await freshDiagnostics();
    const initial = deferred<boolean>();
    const second = deferred<boolean>();
    const invoke = vi.fn().mockReturnValueOnce(initial.promise).mockReturnValueOnce(second.promise);

    const diagnostics = installUiMemoryDiagnostics(invoke);
    for (let index = 0; index < 20; index += 1) {
      incrementUiMemoryCounter("staged_received_total");
    }
    initial.resolve(true);
    await flushPromises();
    await vi.advanceTimersByTimeAsync(60_000);

    expect(invoke).toHaveBeenCalledTimes(2);
    expect(invoke.mock.calls[1]?.[1].counters.staged_received_total).toBe(20);
    for (let index = 0; index < 5; index += 1) {
      incrementUiMemoryCounter("staged_received_total");
    }
    await vi.advanceTimersByTimeAsync(60_000 * 300);
    expect(invoke).toHaveBeenCalledTimes(2);

    second.resolve(true);
    await flushPromises();
    await vi.advanceTimersByTimeAsync(60_000);

    expect(invoke).toHaveBeenCalledTimes(3);
    expect(invoke.mock.calls[2]?.[1].counters.staged_received_total).toBe(25);
    diagnostics.dispose();
  });

  it("does not install a timer after disposal while the initial request is pending", async () => {
    vi.useFakeTimers();
    const { installUiMemoryDiagnostics } = await freshDiagnostics();
    const initial = deferred<boolean>();
    const invoke = vi.fn().mockReturnValue(initial.promise);

    const diagnostics = installUiMemoryDiagnostics(invoke);
    diagnostics.dispose();
    initial.resolve(true);
    await flushPromises();
    await vi.advanceTimersByTimeAsync(60_000 * 10);

    expect(invoke).toHaveBeenCalledTimes(1);
    expect(vi.getTimerCount()).toBe(0);
  });

  it.each([
    ["false response", () => Promise.resolve(false)],
    ["rejected response", () => Promise.reject(new Error("private failure"))],
    [
      "synchronous throw",
      () => {
        throw new Error("private failure");
      },
    ],
  ])("does not start periodic sends after an initial %s", async (_name, response) => {
    vi.useFakeTimers();
    const { installUiMemoryDiagnostics } = await freshDiagnostics();
    const invoke = vi.fn(response);
    const diagnostics = installUiMemoryDiagnostics(invoke);

    await flushPromises();
    await vi.advanceTimersByTimeAsync(60_000 * 10);

    expect(invoke).toHaveBeenCalledTimes(1);
    expect(vi.getTimerCount()).toBe(0);
    diagnostics.dispose();
  });

  it("silently retries the newest sample on the next interval after a later failure", async () => {
    vi.useFakeTimers();
    const { incrementUiMemoryCounter, installUiMemoryDiagnostics } = await freshDiagnostics();
    const invoke = vi
      .fn()
      .mockResolvedValueOnce(true)
      .mockRejectedValueOnce(new Error("private failure"))
      .mockResolvedValue(true);
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const diagnostics = installUiMemoryDiagnostics(invoke);

    await flushPromises();
    await vi.advanceTimersByTimeAsync(60_000);
    incrementUiMemoryCounter("generated_refresh_total");
    await flushPromises();
    await vi.advanceTimersByTimeAsync(60_000);

    expect(invoke).toHaveBeenCalledTimes(3);
    expect(invoke.mock.calls[2]?.[1].counters.generated_refresh_total).toBe(1);
    expect(error).not.toHaveBeenCalled();
    expect(warn).not.toHaveBeenCalled();
    diagnostics.dispose();
  });
});
