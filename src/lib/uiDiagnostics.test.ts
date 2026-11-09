import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { installUiDiagnostics, type UiDiagnostic } from "./uiDiagnostics";

function diagnosticInvoker() {
  return vi.fn((_command: string, _args: { diagnostic: UiDiagnostic }) => Promise.resolve());
}

describe("UI diagnostics", () => {
  let dispose: (() => void) | undefined;

  beforeEach(() => {
    vi.stubGlobal("reportError", vi.fn());
    vi.spyOn(console, "error").mockImplementation(() => {});
  });

  afterEach(() => {
    dispose?.();
    dispose = undefined;
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("does not report when hooks are merely registered or on ordinary events", async () => {
    const invoke = diagnosticInvoker();
    const hooks = installUiDiagnostics(window, invoke);
    dispose = hooks.dispose;

    window.dispatchEvent(new Event("progress"));
    await Promise.resolve();
    expect(hooks.rootOptions.onCaughtError).toEqual(expect.any(Function));
    expect(hooks.rootOptions.onUncaughtError).toEqual(expect.any(Function));
    expect(invoke).not.toHaveBeenCalled();
  });

  it("reports only fixed classifications and mapped positions for each exception source", async () => {
    const invoke = diagnosticInvoker();
    const hooks = installUiDiagnostics(window, invoke);
    dispose = hooks.dispose;
    const caught = new TypeError("Maximum update depth exceeded secret-message");
    Object.defineProperty(caught, "stack", {
      value:
        "TypeError: Maximum update depth exceeded secret-stack\n" +
        "    at render (http://localhost:5180/src/pages/Summaries.tsx?token=secret-query:17:9)",
    });

    hooks.rootOptions.onCaughtError?.(caught, {
      componentStack: "\n    at Summaries (http://localhost:5180/src/pages/Summaries.tsx:18:4)",
    });
    hooks.rootOptions.onUncaughtError?.(new RangeError("secret-uncaught"), {
      componentStack: "",
    });

    const windowError = new Error("window secret-message");
    Object.defineProperty(windowError, "stack", {
      value: "Error\n    at App (http://localhost/src/App.tsx:21:3)",
    });
    const errorEvent = new ErrorEvent("error", { error: windowError });
    window.dispatchEvent(errorEvent);

    const rejection = new Event("unhandledrejection") as PromiseRejectionEvent;
    Object.defineProperty(rejection, "reason", {
      value: new Error("rejection secret-message"),
    });
    window.dispatchEvent(rejection);

    await Promise.resolve();

    expect(invoke).toHaveBeenCalledTimes(4);
    const reports = invoke.mock.calls.map(([, args]) => args.diagnostic);
    expect(reports.map(({ source }) => source)).toEqual([
      "react-caught",
      "react-uncaught",
      "window-error",
      "unhandled-rejection",
    ]);
    expect(reports[0]).toMatchObject({
      error_kind: "TypeError",
      code: "maximum-update-depth",
      locations: [
        { area: "summaries", line: 17, column: 9 },
        { area: "summaries", line: 18, column: 4 },
      ],
    });
    expect(reports[1]?.error_kind).toBe("RangeError");
    expect(reports[2]?.locations[0]).toMatchObject({
      area: "app",
      line: 21,
      column: 3,
    });
    expect(JSON.stringify(reports)).not.toContain("secret-");
    expect(errorEvent.defaultPrevented).toBe(false);
  });

  it("treats arbitrary rejection values as unknown without stringifying them", async () => {
    const invoke = diagnosticInvoker();
    const hooks = installUiDiagnostics(window, invoke);
    dispose = hooks.dispose;
    const rejection = new Event("unhandledrejection") as PromiseRejectionEvent;
    Object.defineProperty(rejection, "reason", {
      value: { message: "secret-rejection-object", token: "secret-token" },
    });

    window.dispatchEvent(rejection);
    await Promise.resolve();

    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke.mock.calls[0]?.[1].diagnostic).toMatchObject({
      source: "unhandled-rejection",
      error_kind: "Unknown",
      code: "unknown",
    });
    expect(JSON.stringify(invoke.mock.calls[0]?.[1])).not.toContain("secret-");
  });

  it("recognizes the fixed React rerender code without retaining its message", async () => {
    const invoke = diagnosticInvoker();
    const hooks = installUiDiagnostics(window, invoke);
    dispose = hooks.dispose;

    hooks.rootOptions.onCaughtError?.(
      new Error("Too many re-renders. React limits the number of renders. secret-message"),
      { componentStack: "" },
    );
    await Promise.resolve();

    expect(invoke.mock.calls[0]?.[1].diagnostic).toMatchObject({
      error_kind: "Error",
      code: "too-many-rerenders",
    });
    expect(JSON.stringify(invoke.mock.calls[0]?.[1])).not.toContain("secret-message");
  });

  it("does not duplicate React uncaught errors re-emitted as global errors", async () => {
    const invoke = diagnosticInvoker();
    const error = new Error("same error object");
    vi.stubGlobal("reportError", (reported: unknown) => {
      window.dispatchEvent(new ErrorEvent("error", { error: reported }));
    });
    const hooks = installUiDiagnostics(window, invoke);
    dispose = hooks.dispose;

    hooks.rootOptions.onUncaughtError?.(error, { componentStack: "" });
    await Promise.resolve();

    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke.mock.calls[0]?.[1].diagnostic.source).toBe("react-uncaught");
  });

  it("limits positions, deduplicates repeated signatures, and caps reports per session", () => {
    const invoke = diagnosticInvoker();
    const hooks = installUiDiagnostics(window, invoke);
    dispose = hooks.dispose;
    const caught = hooks.rootOptions.onCaughtError;
    expect(caught).toBeDefined();

    const stack = Array.from(
      { length: 6 },
      (_, index) => `    at render (http://localhost/src/pages/Summaries.tsx:${index + 1}:2)`,
    ).join("\n");
    const first = new Error("ordinary render failure");
    Object.defineProperty(first, "stack", { value: stack });
    caught?.(first, { componentStack: "" });
    caught?.(first, { componentStack: "" });

    for (let index = 0; index < 14; index += 1) {
      const error = new Error("ordinary render failure");
      Object.defineProperty(error, "stack", {
        value: `Error\n    at render (http://localhost/src/pages/Summaries.tsx:${100 + index}:2)`,
      });
      caught?.(error, { componentStack: "" });
    }

    expect(invoke).toHaveBeenCalledTimes(10);
    expect(invoke.mock.calls[0]?.[1].diagnostic.locations).toHaveLength(4);
  });

  it("does not retry or leak invoke failures into the error event loop", async () => {
    const invoke = vi.fn(() => Promise.reject(new Error("secret-invoke-failure")));
    const hooks = installUiDiagnostics(window, invoke);
    dispose = hooks.dispose;
    const rejectionObserver = vi.fn();
    window.addEventListener("unhandledrejection", rejectionObserver);

    const failure = new Error("failure");
    hooks.rootOptions.onCaughtError?.(failure, { componentStack: "" });
    await Promise.resolve();
    await Promise.resolve();
    hooks.rootOptions.onCaughtError?.(failure, { componentStack: "" });
    await Promise.resolve();

    window.removeEventListener("unhandledrejection", rejectionObserver);
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(rejectionObserver).not.toHaveBeenCalled();
  });

  it("keeps React's default uncaught and caught console visibility", () => {
    const invoke = diagnosticInvoker();
    const reportError = window.reportError;
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => {});
    const hooks = installUiDiagnostics(window, invoke);
    dispose = hooks.dispose;
    const caught = new Error("caught");
    const uncaught = new Error("uncaught");

    hooks.rootOptions.onCaughtError?.(caught, { componentStack: "" });
    hooks.rootOptions.onUncaughtError?.(uncaught, { componentStack: "" });

    expect(consoleError).toHaveBeenCalledWith(caught);
    expect(reportError).toHaveBeenCalledWith(uncaught);
  });
});
