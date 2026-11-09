import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { StrictMode } from "react";
import { I18nextProvider } from "react-i18next";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { AgentTerminal, type AgentTerminalProps, validateTerminalLink } from "./AgentTerminal";
import type {
  AgentTerminalBridge,
  AgentTerminalSession,
  TerminalRenderer,
  TerminalRendererFactory,
  TerminalRendererOptions,
  TerminalRendererSubscription,
} from "./types";

class FakeRenderer implements TerminalRenderer {
  readonly writes: Uint8Array[] = [];
  readonly subscriptions: TerminalRendererSubscription[] = [];
  readonly disposedSubscriptions: TerminalRendererSubscription[] = [];
  inputEnabled = false;
  disposed = false;
  disposeCalls = 0;
  private dataListener: ((data: string) => void) | undefined;
  private binaryListener: ((data: string) => void) | undefined;

  constructor(private readonly failure?: "open" | "onData" | "onBinary") {}

  open(_container: HTMLElement) {
    if (this.failure === "open") throw new Error("open failed");
  }

  write(data: Uint8Array) {
    this.writes.push(data);
  }

  onData(listener: (data: string) => void) {
    if (this.failure === "onData") throw new Error("onData subscription failed");
    this.dataListener = listener;
    return this.trackSubscription();
  }

  onBinary(listener: (data: string) => void) {
    if (this.failure === "onBinary") throw new Error("onBinary subscription failed");
    this.binaryListener = listener;
    return this.trackSubscription();
  }

  setInputEnabled(enabled: boolean) {
    this.inputEnabled = enabled;
  }

  dispose() {
    this.disposeCalls += 1;
    this.disposed = true;
  }

  emitData(data: string) {
    this.dataListener?.(data);
  }

  emitBinary(data: string) {
    this.binaryListener?.(data);
  }

  private trackSubscription() {
    const subscription: TerminalRendererSubscription = {
      dispose: () => this.disposedSubscriptions.push(subscription),
    };
    this.subscriptions.push(subscription);
    return subscription;
  }
}

function makeSession(overrides: Partial<AgentTerminalSession> = {}): AgentTerminalSession {
  return {
    runId: "run-17",
    jobId: "job-29",
    purpose: "interactive-run",
    status: "running",
    ownsSession: true,
    feedStatus: "active",
    ...overrides,
  };
}

function makeHarness(rendererFailure?: "open" | "onData" | "onBinary") {
  const rendererInstances: FakeRenderer[] = [];
  const rendererOptions: TerminalRendererOptions[] = [];
  const rendererFactory: TerminalRendererFactory = (options) => {
    rendererOptions.push(options);
    const renderer = new FakeRenderer(rendererFailure);
    rendererInstances.push(renderer);
    return renderer;
  };
  const outputListeners = new Set<(bytes: Uint8Array) => void>();
  const unsubscribeOutput = vi.fn();
  const bridge: AgentTerminalBridge = {
    subscribeOutput: vi.fn((_runId, onOutput) => {
      outputListeners.add(onOutput);
      return () => {
        outputListeners.delete(onOutput);
        unsubscribeOutput();
      };
    }),
    sendInput: vi.fn(),
  };
  return {
    bridge,
    outputListeners,
    rendererFactory,
    rendererInstances,
    rendererOptions,
    unsubscribeOutput,
  };
}

function terminalTree(props: Partial<AgentTerminalProps>, harness: ReturnType<typeof makeHarness>) {
  return (
    <I18nextProvider i18n={i18n}>
      <AgentTerminal
        bridge={harness.bridge}
        session={props.session ?? makeSession()}
        rendererFactory={harness.rendererFactory}
        onCancelRequested={props.onCancelRequested}
        onFinishRequested={props.onFinishRequested}
        canFinish={props.canFinish}
        {...props}
      />
    </I18nextProvider>
  );
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function renderTerminal(props: Partial<AgentTerminalProps> = {}, harness = makeHarness()) {
  const session = props.session ?? makeSession();
  const onCancelRequested = props.onCancelRequested ?? vi.fn();
  const onFinishRequested = props.onFinishRequested ?? vi.fn();
  const result = renderWithProviders(
    <AgentTerminal
      bridge={harness.bridge}
      session={session}
      rendererFactory={harness.rendererFactory}
      onCancelRequested={onCancelRequested}
      onFinishRequested={onFinishRequested}
      {...props}
    />,
  );
  return { ...result, harness, onCancelRequested, onFinishRequested };
}

describe("AgentTerminal", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("passes independent byte chunks to the renderer without decoding their UTF-8 boundaries", () => {
    const { harness } = renderTerminal();
    const firstChunk = new Uint8Array([0xe3, 0x81]);
    const secondChunk = new Uint8Array([0x82]);

    for (const output of harness.outputListeners) {
      output(firstChunk);
      output(secondChunk);
    }

    expect(harness.rendererInstances[0]?.writes).toEqual([firstChunk, secondChunk]);
  });

  it("only forwards typed terminal input from an active session owner", () => {
    const nonOwnerHarness = makeHarness();
    renderTerminal({ session: makeSession({ ownsSession: false }) }, nonOwnerHarness);
    nonOwnerHarness.rendererInstances[0]?.emitData("secret\r");
    expect(nonOwnerHarness.bridge.sendInput).not.toHaveBeenCalled();
    expect(nonOwnerHarness.rendererInstances[0]?.inputEnabled).toBe(false);

    const unknownHarness = makeHarness();
    renderTerminal({ session: makeSession({ status: "unknown" }) }, unknownHarness);
    unknownHarness.rendererInstances[0]?.emitData("still blocked\r");
    expect(unknownHarness.bridge.sendInput).not.toHaveBeenCalled();
    expect(unknownHarness.rendererInstances[0]?.inputEnabled).toBe(false);

    const activeHarness = makeHarness();
    renderTerminal({ session: makeSession({ status: "ready" }) }, activeHarness);
    activeHarness.rendererInstances[0]?.emitData("echo 日本語\r");
    expect(activeHarness.bridge.sendInput).toHaveBeenCalledWith(
      "run-17",
      new TextEncoder().encode("echo 日本語\r"),
    );
    expect(
      new TextDecoder().decode(vi.mocked(activeHarness.bridge.sendInput).mock.calls[0]?.[1]),
    ).toBe("echo 日本語\r");
    expect(activeHarness.rendererInstances[0]?.inputEnabled).toBe(true);
  });

  it("routes xterm binary input as bytes rather than UTF-8 text", () => {
    const harness = makeHarness();
    renderTerminal({}, harness);
    harness.rendererInstances[0]?.emitBinary("\u0001\u00ff");

    expect(harness.bridge.sendInput).toHaveBeenCalledWith("run-17", new Uint8Array([1, 255]));
  });

  it("disables input and session controls when the app-owned feed is detached", () => {
    const harness = makeHarness();
    const onCancelRequested = vi.fn();
    const onFinishRequested = vi.fn();
    renderTerminal(
      {
        session: makeSession({ feedStatus: "disconnected" }),
        onCancelRequested,
        onFinishRequested,
      },
      harness,
    );
    harness.rendererInstances[0]?.emitData("stale input");

    expect(harness.bridge.sendInput).not.toHaveBeenCalled();
    expect(harness.rendererInstances[0]?.inputEnabled).toBe(false);
    expect(screen.getByRole("button", { name: "取消" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "作業を終了して検証・公開へ" })).toBeDisabled();
    expect(
      screen.getByText(/端末フィードが切断されています。成功・終了を示さず/),
    ).toBeInTheDocument();
  });

  it("keeps active feed and fixed PTY dimensions separate from renderer attachment", () => {
    const harness = makeHarness();
    renderTerminal({}, harness);

    expect(screen.getByText(/80列×24行固定/)).toBeInTheDocument();
    expect(screen.getByText(/ウィンドウを閉じても描画器だけが切り離され/)).toBeInTheDocument();
    expect(screen.getByText(/アプリ管理の端末フィードは接続中/)).toBeInTheDocument();
    expect(harness.rendererOptions[0]?.screenReaderMode).toBe(true);
  });

  it("passes the screen-reader preference through to the renderer", () => {
    const harness = makeHarness();
    renderTerminal({ screenReaderMode: false }, harness);

    expect(harness.rendererOptions[0]?.screenReaderMode).toBe(false);
  });

  it("uses the user-approved link callback only for safe HTTP(S) links", () => {
    const harness = makeHarness();
    const onRequestOpenLink = vi.fn();
    renderTerminal({ onRequestOpenLink }, harness);
    const activateLink = harness.rendererOptions[0]?.onLinkActivation;

    activateLink?.("https://example.test/path");
    activateLink?.("javascript:alert(1)");
    activateLink?.("file:///etc/passwd");
    activateLink?.("https://user:secret@example.test/");
    activateLink?.("https://example.test/\nmalicious");

    expect(onRequestOpenLink).toHaveBeenCalledTimes(1);
    expect(onRequestOpenLink).toHaveBeenCalledWith("https://example.test/path");
    expect(validateTerminalLink("HTTP://example.test/a b")).toBe("http://example.test/a%20b");
    expect(validateTerminalLink("data:text/html,hello")).toBeNull();
  });

  it("only calls cancellation and finish handlers after their explicit buttons are activated", async () => {
    const harness = makeHarness();
    const { onCancelRequested, onFinishRequested } = renderTerminal({ canFinish: true }, harness);
    for (const output of harness.outputListeners) {
      output(new TextEncoder().encode("exit 0\r\n"));
    }
    await waitFor(() => expect(harness.rendererInstances[0]?.writes).toHaveLength(1));

    expect(onCancelRequested).not.toHaveBeenCalled();
    expect(onFinishRequested).not.toHaveBeenCalled();
    harness.rendererInstances[0]?.emitData("\u0004");
    expect(harness.bridge.sendInput).toHaveBeenCalledTimes(1);
    expect(new TextDecoder().decode(vi.mocked(harness.bridge.sendInput).mock.calls[0]?.[1])).toBe(
      "\u0004",
    );
    expect(onCancelRequested).not.toHaveBeenCalled();
    expect(onFinishRequested).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "取消" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "取消" })).toBeEnabled());
    fireEvent.click(screen.getByRole("button", { name: "作業を終了して検証・公開へ" }));

    expect(onCancelRequested).toHaveBeenCalledWith("run-17");
    expect(onFinishRequested).toHaveBeenCalledWith("run-17");
  });

  it("allows an explicitly eligible normal exit to request finish without an active input feed", () => {
    const harness = makeHarness();
    const onFinishRequested = vi.fn();
    renderTerminal(
      {
        session: makeSession({ status: "finished", feedStatus: "disconnected" }),
        canFinish: true,
        onFinishRequested,
      },
      harness,
    );

    expect(screen.getByRole("button", { name: "作業を終了して検証・公開へ" })).toBeEnabled();
    fireEvent.click(screen.getByRole("button", { name: "作業を終了して検証・公開へ" }));

    expect(onFinishRequested).toHaveBeenCalledWith("run-17");
    expect(harness.bridge.sendInput).not.toHaveBeenCalled();
  });

  it("does not allow finish for a cancelled session without backend eligibility", () => {
    const harness = makeHarness();
    const onFinishRequested = vi.fn();
    renderTerminal(
      { session: makeSession({ status: "cancelled" }), canFinish: false, onFinishRequested },
      harness,
    );

    expect(screen.getByRole("button", { name: "作業を終了して検証・公開へ" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "作業を終了して検証・公開へ" }));
    expect(onFinishRequested).not.toHaveBeenCalled();
  });

  it.each([
    ["cancel", "取消"],
    ["finish", "作業を終了して検証・公開へ"],
  ] as const)(
    "clears pending %s after sync and async failures and only disables while awaiting",
    async (action, label) => {
      const harness = makeHarness();
      const asyncFailure = deferred<void>();
      const handler = vi
        .fn<() => void | Promise<void>>()
        .mockImplementationOnce(() => {
          throw new Error(`${action} failed synchronously`);
        })
        .mockImplementationOnce(() => asyncFailure.promise);
      renderTerminal(
        action === "cancel"
          ? { onCancelRequested: handler }
          : { canFinish: true, onFinishRequested: handler },
        harness,
      );
      const button = screen.getByRole("button", { name: label });

      fireEvent.click(button);
      await waitFor(() => expect(handler).toHaveBeenCalledTimes(1));
      expect(screen.getByRole("alert")).toBeInTheDocument();
      expect(button).toBeEnabled();

      fireEvent.click(button);
      await waitFor(() => expect(handler).toHaveBeenCalledTimes(2));
      expect(button).toBeDisabled();
      await act(async () => {
        asyncFailure.reject(new Error(`${action} failed asynchronously`));
        await asyncFailure.promise.catch(() => undefined);
      });
      await waitFor(() => expect(button).toBeEnabled());
      expect(screen.getByRole("alert")).toBeInTheDocument();
    },
  );

  it("keeps run B finish pending when run A completes late", async () => {
    const harness = makeHarness();
    const a = deferred<void>();
    const b = deferred<void>();
    const onFinishRequested = vi.fn((runId: string) => (runId === "run-A" ? a.promise : b.promise));
    const result = renderWithProviders(
      terminalTree(
        {
          session: makeSession({ runId: "run-A", status: "finished" }),
          canFinish: true,
          onFinishRequested,
        },
        harness,
      ),
    );

    fireEvent.click(screen.getByRole("button", { name: "作業を終了して検証・公開へ" }));
    result.rerender(
      terminalTree(
        {
          session: makeSession({ runId: "run-B", status: "finished" }),
          canFinish: true,
          onFinishRequested,
        },
        harness,
      ),
    );
    const finishButton = screen.getByRole("button", { name: "作業を終了して検証・公開へ" });
    expect(finishButton).toBeEnabled();
    fireEvent.click(finishButton);
    expect(finishButton).toBeDisabled();

    await act(async () => {
      a.resolve();
      await a.promise;
    });
    expect(finishButton).toBeDisabled();
    b.resolve();
    await waitFor(() => expect(finishButton).toBeEnabled());
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("does not show run A failure on run B or clear B pending", async () => {
    const harness = makeHarness();
    const a = deferred<void>();
    const b = deferred<void>();
    const onFinishRequested = vi.fn((runId: string) => (runId === "run-A" ? a.promise : b.promise));
    const result = renderWithProviders(
      terminalTree(
        {
          session: makeSession({ runId: "run-A", status: "finished" }),
          canFinish: true,
          onFinishRequested,
        },
        harness,
      ),
    );

    fireEvent.click(screen.getByRole("button", { name: "作業を終了して検証・公開へ" }));
    result.rerender(
      terminalTree(
        {
          session: makeSession({ runId: "run-B", status: "finished" }),
          canFinish: true,
          onFinishRequested,
        },
        harness,
      ),
    );
    const finishButton = screen.getByRole("button", { name: "作業を終了して検証・公開へ" });
    fireEvent.click(finishButton);

    await act(async () => {
      a.reject(new Error("run A failed"));
      await a.promise.catch(() => undefined);
    });
    expect(finishButton).toBeDisabled();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    b.resolve();
    await waitFor(() => expect(finishButton).toBeEnabled());
  });

  it("cleans partial renderer startup on create, open, and subscription failures", () => {
    const creationFailure = makeHarness();
    const failedFactory = vi.fn(() => {
      throw new Error("renderer construction failed");
    });
    const onCancelRequested = vi.fn();
    const createResult = renderTerminal(
      { rendererFactory: failedFactory, onCancelRequested },
      creationFailure,
    );
    expect(screen.getByRole("alert")).toBeInTheDocument();
    createResult.unmount();
    expect(onCancelRequested).not.toHaveBeenCalled();

    const openFailure = makeHarness("open");
    const openResult = renderTerminal({ onCancelRequested }, openFailure);
    expect(screen.getByRole("alert")).toBeInTheDocument();
    expect(openFailure.rendererInstances[0]?.disposeCalls).toBe(1);
    openResult.unmount();
    expect(openFailure.rendererInstances[0]?.disposeCalls).toBe(1);
    expect(openFailure.unsubscribeOutput).not.toHaveBeenCalled();

    const subscribeFailure = makeHarness();
    vi.mocked(subscribeFailure.bridge.subscribeOutput).mockImplementation(() => {
      throw new Error("feed subscribe failed");
    });
    const subscribeResult = renderTerminal({ onCancelRequested }, subscribeFailure);
    expect(screen.getByRole("alert")).toBeInTheDocument();
    expect(subscribeFailure.rendererInstances[0]?.disposeCalls).toBe(1);
    subscribeResult.unmount();
    expect(subscribeFailure.rendererInstances[0]?.disposeCalls).toBe(1);
    expect(onCancelRequested).not.toHaveBeenCalled();
  });

  it("unsubscribes and disposes once when a later renderer subscription fails", () => {
    const harness = makeHarness("onBinary");
    const onCancelRequested = vi.fn();
    const result = renderTerminal({ onCancelRequested }, harness);

    expect(screen.getByRole("alert")).toBeInTheDocument();
    expect(harness.outputListeners.size).toBe(0);
    expect(harness.unsubscribeOutput).toHaveBeenCalledTimes(1);
    expect(harness.rendererInstances[0]?.disposedSubscriptions).toHaveLength(1);
    expect(harness.rendererInstances[0]?.disposeCalls).toBe(1);
    result.unmount();
    expect(harness.rendererInstances[0]?.disposeCalls).toBe(1);
    expect(harness.unsubscribeOutput).toHaveBeenCalledTimes(1);
    expect(onCancelRequested).not.toHaveBeenCalled();
  });

  it("releases a subscription whose synchronous output fails before subscribe returns", () => {
    const harness = makeHarness();
    const subscribe = vi.mocked(harness.bridge.subscribeOutput).getMockImplementation();
    if (!subscribe) throw new Error("missing fixture subscription");
    vi.mocked(harness.bridge.subscribeOutput).mockImplementation((runId, listener) => {
      const unsubscribe = subscribe(runId, listener);
      listener(new Uint8Array([65]));
      return unsubscribe;
    });
    const factory: TerminalRendererFactory = (options) => {
      const renderer = harness.rendererFactory(options);
      if (renderer instanceof Promise) throw new Error("unexpected async fixture");
      renderer.write = () => {
        throw new Error("write failed");
      };
      return renderer;
    };
    const result = renderTerminal({ rendererFactory: factory }, harness);
    expect(screen.getByRole("alert")).toBeInTheDocument();
    expect(harness.outputListeners.size).toBe(0);
    expect(harness.unsubscribeOutput).toHaveBeenCalledTimes(1);
    expect(harness.rendererInstances[0]?.disposeCalls).toBe(1);
    result.unmount();
    expect(harness.unsubscribeOutput).toHaveBeenCalledTimes(1);
  });

  it("does not carry a renderer error from run A into run B while B initializes", async () => {
    const harness = makeHarness();
    const runBRenderer = deferred<TerminalRenderer>();
    let factoryCalls = 0;
    const rendererFactory: TerminalRendererFactory = () => {
      factoryCalls += 1;
      if (factoryCalls === 1) throw new Error("run A renderer failed");
      return runBRenderer.promise;
    };
    const result = renderWithProviders(
      terminalTree(
        {
          session: makeSession({ runId: "run-A" }),
          rendererFactory,
        },
        harness,
      ),
    );
    expect(screen.getByRole("alert")).toBeInTheDocument();

    result.rerender(
      terminalTree(
        {
          session: makeSession({ runId: "run-B" }),
          rendererFactory,
        },
        harness,
      ),
    );
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    await act(async () => {
      runBRenderer.resolve(new FakeRenderer());
      await runBRenderer.promise;
    });
  });

  it("keeps cancel and finish unavailable to non-owners and terminal sessions", () => {
    const harness = makeHarness();
    const onCancelRequested = vi.fn();
    const onFinishRequested = vi.fn();
    renderTerminal(
      {
        session: makeSession({ ownsSession: false, status: "finished" }),
        onCancelRequested,
        onFinishRequested,
      },
      harness,
    );

    expect(screen.getByRole("button", { name: "取消" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "作業を終了して検証・公開へ" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "取消" }));
    fireEvent.click(screen.getByRole("button", { name: "作業を終了して検証・公開へ" }));
    expect(onCancelRequested).not.toHaveBeenCalled();
    expect(onFinishRequested).not.toHaveBeenCalled();
  });

  it("supports an authentication terminal without exposing Issue finish", () => {
    const harness = makeHarness();
    const onFinishRequested = vi.fn();
    renderTerminal(
      { session: makeSession({ purpose: "authentication" }), onFinishRequested },
      harness,
    );
    harness.rendererInstances[0]?.emitData("provider-secret\r");

    expect(harness.bridge.sendInput).toHaveBeenCalledTimes(1);
    expect(
      screen.queryByRole("button", { name: "作業を終了して検証・公開へ" }),
    ).not.toBeInTheDocument();
    expect(onFinishRequested).not.toHaveBeenCalled();
  });

  it("disposes renderer and subscriptions once without implicitly cancelling on unmount", () => {
    const harness = makeHarness();
    const onCancelRequested = vi.fn();
    const onRequestOpenLink = vi.fn();
    const result = renderWithProviders(
      <StrictMode>
        <AgentTerminal
          bridge={harness.bridge}
          session={makeSession()}
          rendererFactory={harness.rendererFactory}
          onCancelRequested={onCancelRequested}
          onRequestOpenLink={onRequestOpenLink}
        />
      </StrictMode>,
    );

    expect(harness.outputListeners.size).toBe(1);
    result.unmount();

    expect(harness.outputListeners.size).toBe(0);
    expect(harness.unsubscribeOutput).toHaveBeenCalledTimes(harness.rendererInstances.length);
    expect(harness.rendererInstances.every((renderer) => renderer.disposed)).toBe(true);
    expect(
      harness.rendererInstances.every((renderer) =>
        renderer.subscriptions.every((subscription) =>
          renderer.disposedSubscriptions.includes(subscription),
        ),
      ),
    ).toBe(true);
    expect(
      harness.rendererInstances.every(
        (renderer) => renderer.disposedSubscriptions.length === renderer.subscriptions.length,
      ),
    ).toBe(true);
    harness.rendererInstances[0]?.emitData("input after detach");
    harness.rendererOptions[0]?.onLinkActivation("https://example.test/");
    expect(harness.bridge.sendInput).not.toHaveBeenCalled();
    expect(onRequestOpenLink).not.toHaveBeenCalled();
    expect(onCancelRequested).not.toHaveBeenCalled();
  });
});
