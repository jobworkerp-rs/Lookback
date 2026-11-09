import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const {
  createRootMock,
  disposeMemoryDiagnosticsMock,
  installMemoryDiagnosticsMock,
  installReactTimingCleanupMock,
  invokeMock,
  renderMock,
} = vi.hoisted(() => ({
  createRootMock: vi.fn(),
  disposeMemoryDiagnosticsMock: vi.fn(),
  installMemoryDiagnosticsMock: vi.fn(() => ({ dispose: disposeMemoryDiagnosticsMock })),
  installReactTimingCleanupMock: vi.fn(() => ({ dispose: vi.fn() })),
  invokeMock: vi.fn(() => Promise.resolve()),
  renderMock: vi.fn(),
}));

vi.mock("react-dom/client", () => ({
  default: { createRoot: createRootMock },
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("./App", () => ({ App: () => null }));
vi.mock("./i18n", () => ({ default: {} }));
vi.mock("./lib/uiMemoryDiagnostics", () => ({
  installUiMemoryDiagnostics: installMemoryDiagnosticsMock,
}));
vi.mock("./lib/reactPerformanceTimingCleanup", () => ({
  installReactPerformanceTimingCleanup: installReactTimingCleanupMock,
}));

import { installUiMemoryDiagnostics } from "./lib/uiMemoryDiagnostics";

describe("main UI diagnostics wiring", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.stubEnv("DEV", true);
    document.body.innerHTML = '<div id="root"></div>';
    vi.stubGlobal("reportError", vi.fn());
    vi.spyOn(console, "error").mockImplementation(() => {});
    createRootMock.mockImplementation(() => ({ render: renderMock }));
    createRootMock.mockClear();
    invokeMock.mockClear();
    renderMock.mockClear();
    installMemoryDiagnosticsMock.mockClear();
    disposeMemoryDiagnosticsMock.mockClear();
    installReactTimingCleanupMock.mockClear();
  });

  afterEach(() => {
    vi.unstubAllEnvs();
  });

  it("starts development timing cleanup before creating the React root", async () => {
    await import("./main");

    expect(installReactTimingCleanupMock).toHaveBeenCalledTimes(1);
    expect(installReactTimingCleanupMock.mock.invocationCallOrder[0]).toBeLessThan(
      createRootMock.mock.invocationCallOrder[0] ?? 0,
    );
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("does not install timing cleanup outside development", async () => {
    vi.stubEnv("DEV", false);
    await import("./main");

    expect(installReactTimingCleanupMock).not.toHaveBeenCalled();
    expect(createRootMock).toHaveBeenCalledTimes(1);
    expect(installMemoryDiagnosticsMock).toHaveBeenCalledTimes(1);
  });

  it("registers React callbacks without reporting during normal startup", async () => {
    await import("./main");

    expect(createRootMock).toHaveBeenCalledTimes(1);
    expect(installMemoryDiagnosticsMock).toHaveBeenCalledTimes(1);
    const sendActivitySample = vi.mocked(installUiMemoryDiagnostics).mock.calls[0]?.[0];
    expect(sendActivitySample).toEqual(expect.any(Function));
    await sendActivitySample?.("report_ui_activity_sample", {
      counters: {
        staged_received_total: 0,
        staged_published_total: 0,
        generated_refresh_total: 0,
        summaries_invalidated_total: 0,
        summary_list_render_attempts_total: 0,
        summary_card_render_attempts_total: 0,
      },
    });
    expect(invokeMock).toHaveBeenCalledWith("report_ui_activity_sample", {
      counters: {
        staged_received_total: 0,
        staged_published_total: 0,
        generated_refresh_total: 0,
        summaries_invalidated_total: 0,
        summary_list_render_attempts_total: 0,
        summary_card_render_attempts_total: 0,
      },
    });
    invokeMock.mockClear();
    const options = createRootMock.mock.calls[0]?.[1];
    expect(options.onCaughtError).toEqual(expect.any(Function));
    expect(options.onUncaughtError).toEqual(expect.any(Function));
    expect(renderMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).not.toHaveBeenCalled();

    options.onCaughtError(new TypeError("Maximum update depth exceeded"), {
      componentStack: "",
    });
    await Promise.resolve();

    expect(invokeMock).toHaveBeenCalledWith(
      "report_ui_diagnostic",
      expect.objectContaining({
        diagnostic: expect.objectContaining({
          source: "react-caught",
          error_kind: "TypeError",
          code: "maximum-update-depth",
        }),
      }),
    );
  });
});
