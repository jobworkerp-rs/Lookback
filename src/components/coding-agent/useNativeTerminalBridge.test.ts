import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  type NativeTerminalAttachment,
  type NativeTerminalOutputRecord,
  type NativeTerminalSession,
  nativeTerminalAttach,
  nativeTerminalDetach,
  nativeTerminalOutput,
  nativeTerminalSendInput,
} from "@/api/nativeTerminal";
import {
  createDetachSubscriptionManager,
  useNativeTerminalBridge,
} from "./useNativeTerminalBridge";

const listen = vi.fn();
vi.mock("@tauri-apps/api/event", () => ({
  listen: (...args: unknown[]) => listen(...args),
}));
vi.mock("@/api/nativeTerminal", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/nativeTerminal")>();
  return {
    ...actual,
    nativeTerminalAttach: vi.fn(),
    nativeTerminalDetach: vi.fn(),
    nativeTerminalOutput: vi.fn(),
    nativeTerminalSendInput: vi.fn(),
  };
});

function session(overrides: Partial<NativeTerminalSession> = {}): NativeTerminalSession {
  return {
    sessionId: "session-a",
    previewId: "preview-a",
    profileId: "opencode",
    issueId: "issue-7",
    state: "running",
    jobId: "9223372036854775807",
    exitCode: null,
    endReceived: false,
    streamEof: false,
    streamError: null,
    cancelRequested: false,
    ...overrides,
  };
}

function record(
  sequence: string,
  event: NativeTerminalOutputRecord["event"] = {
    kind: "output",
    stream: "stdout",
    data: [0xe3, 0x81],
  },
): NativeTerminalOutputRecord {
  return { sessionId: "session-a", sequence, event };
}

function attachment(overrides: Partial<NativeTerminalAttachment> = {}): NativeTerminalAttachment {
  return {
    session: session(),
    events: [],
    nextSequence: "1",
    subscriptionId: "subscription-a",
    ...overrides,
  };
}

function emit(recordValue: NativeTerminalOutputRecord, subscriptionId = "subscription-a") {
  const handler = vi.mocked(listen).mock.calls[0]?.[1] as
    | ((event: { payload: unknown }) => void)
    | undefined;
  handler?.({ payload: { subscriptionId, record: recordValue } });
}

function emitDirect(recordValue: NativeTerminalOutputRecord) {
  const handler = vi.mocked(listen).mock.calls[0]?.[1] as
    | ((event: { payload: unknown }) => void)
    | undefined;
  handler?.({ payload: recordValue });
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

describe("useNativeTerminalBridge", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    listen.mockResolvedValue(vi.fn());
    vi.mocked(nativeTerminalAttach).mockResolvedValue(attachment());
    vi.mocked(nativeTerminalDetach).mockResolvedValue(undefined);
    vi.mocked(nativeTerminalOutput).mockResolvedValue({ events: [], nextSequence: "1" });
    vi.mocked(nativeTerminalSendInput).mockResolvedValue("accepted");
  });

  it("installs the native event listener before attach and flushes replay bytes to late terminal subscribers", async () => {
    vi.mocked(nativeTerminalAttach).mockResolvedValue(
      attachment({
        events: [record("1", { kind: "output", stream: "stdout", data: [0xe3, 0x81] })],
        nextSequence: "2",
      }),
    );
    const { result, unmount } = renderHook(() => useNativeTerminalBridge("session-a"));

    await waitFor(() => expect(result.current.session?.sessionId).toBe("session-a"));
    expect(listen).toHaveBeenCalledWith("native-terminal-output", expect.any(Function));
    expect(nativeTerminalAttach).toHaveBeenCalledWith({
      sessionId: "session-a",
      afterSequence: "0",
    });
    const listenOrder = vi.mocked(listen).mock.invocationCallOrder[0];
    const attachOrder = vi.mocked(nativeTerminalAttach).mock.invocationCallOrder[0];
    if (listenOrder === undefined || attachOrder === undefined) {
      throw new Error("native terminal subscription calls were not recorded");
    }
    expect(listenOrder).toBeLessThan(attachOrder);

    const output = vi.fn();
    act(() => {
      result.current.bridge.subscribeOutput("session-a", output);
    });
    expect(output).toHaveBeenCalledWith(new Uint8Array([0xe3, 0x81]));

    unmount();
    await waitFor(() =>
      expect(vi.mocked(listen).mock.results[0]?.value).resolves.toBeTypeOf("function"),
    );
    await waitFor(() =>
      expect(nativeTerminalDetach).toHaveBeenCalledWith({
        sessionId: "session-a",
        subscriptionId: "subscription-a",
      }),
    );
  });

  it("buffers output during the attach handshake and filters it against the returned subscription", async () => {
    const attaching = deferred<NativeTerminalAttachment>();
    vi.mocked(nativeTerminalAttach).mockReturnValueOnce(attaching.promise);
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(listen).toHaveBeenCalledTimes(1));

    act(() => {
      emit(record("1", { kind: "output", stream: "stdout", data: [65] }), "stale-subscription");
      emit(record("1", { kind: "output", stream: "stdout", data: [66] }));
    });
    await act(async () => {
      attaching.resolve(attachment({ nextSequence: "1" }));
      await attaching.promise;
    });
    await waitFor(() => expect(result.current.session?.sessionId).toBe("session-a"));

    const output = vi.fn();
    act(() => {
      result.current.bridge.subscribeOutput("session-a", output);
    });
    expect(output).toHaveBeenCalledTimes(1);
    expect(output).toHaveBeenCalledWith(new Uint8Array([66]));
  });

  it("filters by session and subscription, preserving binary chunks and decimal sequences", async () => {
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    const output = vi.fn();
    act(() => {
      result.current.bridge.subscribeOutput("session-a", output);
      emit(record("1", { kind: "output", stream: "stdout", data: [0, 128, 255] }));
      emit(record("2", { kind: "output", stream: "stdout", data: [65] }), "other-subscription");
      emitDirect({
        ...record("2", { kind: "output", stream: "stdout", data: [66] }),
        sessionId: "other",
      });
      emitDirect(record("2", { kind: "output", stream: "stdout", data: [67] }));
    });

    expect(output).toHaveBeenCalledTimes(2);
    expect(output).toHaveBeenNthCalledWith(1, new Uint8Array([0, 128, 255]));
    expect(output).toHaveBeenNthCalledWith(2, new Uint8Array([67]));
  });

  it("recovers an out-of-order live event from the native output page", async () => {
    vi.mocked(nativeTerminalAttach).mockResolvedValue(
      attachment({
        events: [record("1", { kind: "output", stream: "stdout", data: [65] })],
        nextSequence: "2",
      }),
    );
    vi.mocked(nativeTerminalOutput).mockResolvedValueOnce({
      events: [
        record("2", { kind: "output", stream: "stdout", data: [66] }),
        record("3", { kind: "output", stream: "stdout", data: [67] }),
      ],
      nextSequence: "4",
    });
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    const output = vi.fn();
    act(() => {
      result.current.bridge.subscribeOutput("session-a", output);
      emit(record("3", { kind: "output", stream: "stdout", data: [67] }));
    });

    await waitFor(() =>
      expect(nativeTerminalOutput).toHaveBeenCalledWith({
        sessionId: "session-a",
        afterSequence: "1",
      }),
    );
    await waitFor(() => expect(output).toHaveBeenCalledTimes(3));
    expect(output.mock.calls.map((call) => Array.from(call[0] as Uint8Array))).toEqual([
      [65],
      [66],
      [67],
    ]);
  });

  it("disconnects instead of skipping a sequence gap when native replay cannot fill it", async () => {
    vi.mocked(nativeTerminalOutput).mockResolvedValueOnce({ events: [], nextSequence: "3" });
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    const output = vi.fn();
    act(() => {
      result.current.bridge.subscribeOutput("session-a", output);
      emit(record("2", { kind: "output", stream: "stdout", data: [66] }));
    });

    await waitFor(() => expect(result.current.feedStatus).toBe("disconnected"));
    expect(output).not.toHaveBeenCalled();
    expect(result.current.error).toMatch(/sequence has a gap/i);
  });

  it("replays a gap before the first attached event instead of assuming that sequence is the origin", async () => {
    vi.mocked(nativeTerminalAttach).mockResolvedValue(
      attachment({
        events: [record("2", { kind: "output", stream: "stdout", data: [66] })],
        nextSequence: "3",
      }),
    );
    vi.mocked(nativeTerminalOutput).mockResolvedValueOnce({ events: [], nextSequence: "3" });
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));

    await waitFor(() => expect(result.current.feedStatus).toBe("disconnected"));
    expect(nativeTerminalOutput).toHaveBeenCalledWith({
      sessionId: "session-a",
      afterSequence: "0",
    });
    expect(result.current.error).toMatch(/sequence has a gap/i);
  });

  it("allows input only for an attached running session and stops accepting input after an exit record", async () => {
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    const bytes = new Uint8Array([3, 10, 255]);
    await expect(result.current.bridge.sendInput("session-a", bytes)).resolves.toBeUndefined();
    expect(nativeTerminalSendInput).toHaveBeenCalledWith("session-a", bytes);

    act(() => {
      emit(
        record("1", { kind: "exit", exitCode: 0, executionTimeMs: "9", sandboxId: "sandbox-a" }),
      );
    });
    await expect(result.current.bridge.sendInput("session-a", bytes)).rejects.toThrow();
    expect(nativeTerminalSendInput).toHaveBeenCalledTimes(1);
  });

  it("does not send input for an unknown session or detach by cancelling the native process", async () => {
    vi.mocked(nativeTerminalAttach).mockResolvedValue(
      attachment({ session: session({ state: "unknown" }) }),
    );
    const { result, unmount } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.session?.state).toBe("unknown"));
    await expect(
      result.current.bridge.sendInput("session-a", new Uint8Array([65])),
    ).rejects.toThrow();
    expect(nativeTerminalSendInput).not.toHaveBeenCalled();

    unmount();
    expect(nativeTerminalSendInput).not.toHaveBeenCalled();
  });

  it("keeps an unknown persisted session read-only even if a later attach reply says running", async () => {
    const storedSession = session({ state: "unknown" });
    const { result } = renderHook(() => useNativeTerminalBridge("session-a", storedSession));
    await waitFor(() => expect(result.current.session?.state).toBe("running"));

    await expect(
      result.current.bridge.sendInput("session-a", new Uint8Array([65])),
    ).rejects.toThrow();
    expect(nativeTerminalSendInput).not.toHaveBeenCalled();
  });

  it("keeps a closed late attachment read-only when the latest listing is unknown", async () => {
    vi.mocked(nativeTerminalAttach).mockResolvedValue(
      attachment({ session: session({ state: "closed", exitCode: 143 }) }),
    );
    const { result } = renderHook(() =>
      useNativeTerminalBridge("session-a", session({ state: "unknown" })),
    );
    await waitFor(() => expect(result.current.session?.state).toBe("closed"));

    expect(result.current.feedStatus).toBe("unknown");
    await expect(
      result.current.bridge.sendInput("session-a", new Uint8Array([65])),
    ).rejects.toThrow();
    expect(nativeTerminalSendInput).not.toHaveBeenCalled();
  });

  it.each([
    ["unknown", "unknown"],
    ["unavailable", "unavailable"],
  ] as const)(
    "latches input after an %s acknowledgement before the next key",
    async (resultValue, feedStatus) => {
      vi.mocked(nativeTerminalSendInput).mockResolvedValueOnce(resultValue);
      const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
      await waitFor(() => expect(result.current.feedStatus).toBe("active"));

      await act(async () => {
        await expect(
          result.current.bridge.sendInput("session-a", new Uint8Array([65])),
        ).rejects.toThrow();
      });
      expect(result.current.feedStatus).toBe(feedStatus);
      expect(result.current.session?.state).toBe("running");
      await expect(
        result.current.bridge.sendInput("session-a", new Uint8Array([66])),
      ).rejects.toThrow();
      expect(nativeTerminalSendInput).toHaveBeenCalledTimes(1);
    },
  );

  it("latches input and feed permission after a transport exception without retrying bytes", async () => {
    vi.mocked(nativeTerminalSendInput).mockRejectedValueOnce(new Error("delivery outcome unknown"));
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));

    await act(async () => {
      await expect(
        result.current.bridge.sendInput("session-a", new Uint8Array([3, 255])),
      ).rejects.toThrow(/delivery outcome unknown/);
    });
    expect(result.current.feedStatus).toBe("unknown");
    expect(result.current.session?.state).toBe("running");
    await expect(
      result.current.bridge.sendInput("session-a", new Uint8Array([3, 255])),
    ).rejects.toThrow();
    expect(nativeTerminalSendInput).toHaveBeenCalledTimes(1);
  });

  it("does not activate the input feed for a native attach that reports unknown or cancellation requested", async () => {
    vi.mocked(nativeTerminalAttach)
      .mockResolvedValueOnce(attachment({ session: session({ state: "unknown" }) }))
      .mockResolvedValueOnce(
        attachment({ session: session({ sessionId: "session-b", cancelRequested: true }) }),
      );
    const { result, rerender } = renderHook(({ id }) => useNativeTerminalBridge(id), {
      initialProps: { id: "session-a" },
    });
    await waitFor(() => expect(result.current.session?.state).toBe("unknown"));
    expect(result.current.feedStatus).toBe("unknown");
    rerender({ id: "session-b" });
    await waitFor(() => expect(result.current.session?.cancelRequested).toBe(true));
    expect(result.current.feedStatus).toBe("unavailable");
    await expect(
      result.current.bridge.sendInput("session-b", new Uint8Array([65])),
    ).rejects.toThrow();
    expect(nativeTerminalSendInput).not.toHaveBeenCalled();
  });

  it("keeps the input feed inactive when the listed session already has a stop request", async () => {
    const listed = session({ cancelRequested: true });
    const { result } = renderHook(() => useNativeTerminalBridge("session-a", listed));
    await waitFor(() => expect(result.current.session?.state).toBe("running"));

    expect(result.current.feedStatus).toBe("unavailable");
    await expect(
      result.current.bridge.sendInput("session-a", new Uint8Array([65])),
    ).rejects.toThrow();
    expect(nativeTerminalSendInput).not.toHaveBeenCalled();
  });

  it("detaches a subscription returned after its attach generation was cleaned up", async () => {
    const attaching = deferred<NativeTerminalAttachment>();
    vi.mocked(nativeTerminalAttach).mockReturnValueOnce(attaching.promise);
    const { unmount } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(nativeTerminalAttach).toHaveBeenCalledTimes(1));
    unmount();

    await act(async () => {
      attaching.resolve(attachment({ subscriptionId: "late-subscription" }));
      await attaching.promise;
    });
    await waitFor(() =>
      expect(nativeTerminalDetach).toHaveBeenCalledWith({
        sessionId: "session-a",
        subscriptionId: "late-subscription",
      }),
    );
  });

  it("detaches the old event subscription on session switch without cancelling either session", async () => {
    vi.mocked(nativeTerminalAttach)
      .mockResolvedValueOnce(attachment({ subscriptionId: "sub-a" }))
      .mockResolvedValueOnce(
        attachment({ session: session({ sessionId: "session-b" }), subscriptionId: "sub-b" }),
      );
    const { result, rerender, unmount } = renderHook(({ id }) => useNativeTerminalBridge(id), {
      initialProps: { id: "session-a" },
    });
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    rerender({ id: "session-b" });
    await waitFor(() => expect(result.current.session?.sessionId).toBe("session-b"));
    expect(nativeTerminalDetach).toHaveBeenCalledTimes(1);
    expect(nativeTerminalDetach).toHaveBeenCalledWith({
      sessionId: "session-a",
      subscriptionId: "sub-a",
    });
    unmount();
    await waitFor(() => expect(nativeTerminalDetach).toHaveBeenCalledTimes(2));
    expect(nativeTerminalDetach).toHaveBeenLastCalledWith({
      sessionId: "session-b",
      subscriptionId: "sub-b",
    });
    expect(nativeTerminalSendInput).not.toHaveBeenCalled();
  });

  it("replays a session from its beginning after selecting it again with a fresh renderer", async () => {
    const firstOutput = record("1", { kind: "output", stream: "stdout", data: [65] });
    vi.mocked(nativeTerminalAttach)
      .mockResolvedValueOnce(
        attachment({ events: [firstOutput], nextSequence: "2", subscriptionId: "a-first" }),
      )
      .mockResolvedValueOnce(
        attachment({
          session: session({ sessionId: "session-b" }),
          subscriptionId: "b-only",
        }),
      )
      .mockResolvedValueOnce(
        attachment({ events: [firstOutput], nextSequence: "2", subscriptionId: "a-again" }),
      );
    const { result, rerender } = renderHook(({ id }) => useNativeTerminalBridge(id), {
      initialProps: { id: "session-a" },
    });
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    const firstRenderer = vi.fn();
    act(() => result.current.bridge.subscribeOutput("session-a", firstRenderer));
    await waitFor(() => expect(firstRenderer).toHaveBeenCalledWith(new Uint8Array([65])));

    rerender({ id: "session-b" });
    await waitFor(() => expect(result.current.session?.sessionId).toBe("session-b"));
    rerender({ id: "session-a" });
    await waitFor(() => expect(result.current.session?.sessionId).toBe("session-a"));

    expect(nativeTerminalAttach).toHaveBeenNthCalledWith(3, {
      sessionId: "session-a",
      afterSequence: "0",
    });
    const nextRenderer = vi.fn();
    act(() => result.current.bridge.subscribeOutput("session-a", nextRenderer));
    await waitFor(() => expect(nextRenderer).toHaveBeenCalledWith(new Uint8Array([65])));
  });

  it("keeps queued output across reattachment until a renderer accepts it", async () => {
    const firstOutput = record("1", { kind: "output", stream: "stdout", data: [65] });
    vi.mocked(nativeTerminalAttach).mockImplementation(async ({ afterSequence }) =>
      attachment({
        events: afterSequence === "0" ? [firstOutput] : [],
        nextSequence: "2",
      }),
    );
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));

    act(() => result.current.reattach());
    await waitFor(() => expect(nativeTerminalAttach).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    expect(nativeTerminalAttach).toHaveBeenLastCalledWith({
      sessionId: "session-a",
      afterSequence: "1",
    });

    const renderer = vi.fn();
    act(() => result.current.bridge.subscribeOutput("session-a", renderer));
    expect(renderer).toHaveBeenCalledTimes(1);
    expect(renderer).toHaveBeenCalledWith(new Uint8Array([65]));
  });

  it("replays delivered output when its terminal renderer unmounts before its write queue flushes", async () => {
    const firstOutput = record("1", { kind: "output", stream: "stdout", data: [65] });
    vi.mocked(nativeTerminalAttach).mockImplementation(async ({ afterSequence }) =>
      attachment({
        events: afterSequence === "0" ? [firstOutput] : [],
        nextSequence: "2",
      }),
    );
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    const firstRenderer = vi.fn();
    let unmountFirstRenderer = () => {};
    act(() => {
      unmountFirstRenderer = result.current.bridge.subscribeOutput("session-a", firstRenderer);
    });
    expect(firstRenderer).toHaveBeenCalledWith(new Uint8Array([65]));

    act(unmountFirstRenderer);
    const nextRenderer = vi.fn();
    act(() => result.current.bridge.subscribeOutput("session-a", nextRenderer));
    await waitFor(() => expect(nativeTerminalAttach).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(nextRenderer).toHaveBeenCalledTimes(1));
    expect(nativeTerminalAttach).toHaveBeenLastCalledWith({
      sessionId: "session-a",
      afterSequence: "0",
    });
    expect(nextRenderer).toHaveBeenCalledWith(new Uint8Array([65]));
  });

  it("does not commit an output sequence when a mounted renderer rejects the write", async () => {
    const rejectedOutput = record("1", { kind: "output", stream: "stdout", data: [65] });
    let attachCount = 0;
    vi.mocked(nativeTerminalAttach).mockImplementation(async () => {
      attachCount += 1;
      return attachment({
        events: attachCount === 2 ? [rejectedOutput] : [],
        nextSequence: attachCount === 2 ? "2" : "1",
      });
    });
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    const rejectWrite = vi.fn(() => {
      throw new Error("renderer disposed");
    });
    let unsubscribe = () => {};
    act(() => {
      unsubscribe = result.current.bridge.subscribeOutput("session-a", rejectWrite);
      emit(rejectedOutput);
    });
    expect(result.current.feedStatus).toBe("disconnected");
    act(unsubscribe);

    act(() => result.current.reattach());
    await waitFor(() => expect(nativeTerminalAttach).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    expect(nativeTerminalAttach).toHaveBeenLastCalledWith({
      sessionId: "session-a",
      afterSequence: "0",
    });
    const recoveredRenderer = vi.fn();
    act(() => result.current.bridge.subscribeOutput("session-a", recoveredRenderer));
    expect(recoveredRenderer).toHaveBeenCalledTimes(1);
    expect(recoveredRenderer).toHaveBeenCalledWith(new Uint8Array([65]));
  });

  it("leaves the cursor before output rejected by the bounded renderer queue and replays it", async () => {
    const acceptedOutput = Array.from({ length: 16 }, (_, index) =>
      record(String(index + 1), {
        kind: "output",
        stream: "stdout",
        data: new Array(64 * 1024).fill(65),
      }),
    );
    const undeliveredOutput = record("17", {
      kind: "output",
      stream: "stdout",
      data: [66],
    });
    vi.mocked(nativeTerminalAttach).mockImplementation(async ({ afterSequence }) =>
      attachment({
        events: afterSequence === "0" ? acceptedOutput : [undeliveredOutput],
        nextSequence: afterSequence === "0" ? "17" : "18",
      }),
    );
    vi.mocked(nativeTerminalOutput).mockImplementation(async ({ afterSequence }) => ({
      events:
        BigInt(afterSequence) < 16n
          ? [
              ...acceptedOutput.filter((output) => BigInt(output.sequence) > BigInt(afterSequence)),
              undeliveredOutput,
            ]
          : [undeliveredOutput],
      nextSequence: "18",
    }));
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("disconnected"));

    act(() => result.current.reattach());
    await waitFor(() => expect(nativeTerminalAttach).toHaveBeenCalledTimes(2));
    expect(nativeTerminalAttach).toHaveBeenNthCalledWith(1, {
      sessionId: "session-a",
      afterSequence: "0",
    });
    expect(nativeTerminalAttach).toHaveBeenNthCalledWith(2, {
      sessionId: "session-a",
      afterSequence: "16",
    });
    await waitFor(() => expect(result.current.feedStatus).toBe("disconnected"));

    const renderer = vi.fn();
    act(() => result.current.bridge.subscribeOutput("session-a", renderer));
    expect(renderer).toHaveBeenCalledTimes(16);
    // Element-wise toEqual over 1 MiB takes seconds on a loaded CI runner.
    for (const [chunk] of renderer.mock.calls as [Uint8Array][]) {
      expect(chunk.byteLength).toBe(64 * 1024);
      expect(chunk.every((byte) => byte === 65)).toBe(true);
    }

    act(() => result.current.reattach());
    await waitFor(() => expect(nativeTerminalAttach).toHaveBeenCalledTimes(3));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    expect(nativeTerminalAttach).toHaveBeenNthCalledWith(3, {
      sessionId: "session-a",
      afterSequence: "16",
    });
    await waitFor(() => expect(renderer).toHaveBeenCalledTimes(17));
    expect(renderer).toHaveBeenNthCalledWith(17, new Uint8Array([66]));
  });

  it.each([
    ["exitCode", { exitCode: 7 }],
    ["endReceived", { endReceived: true }],
    ["streamEof", { streamEof: true }],
    ["streamError", { streamError: "native output failed" }],
    ["cancelRequested", { cancelRequested: true }],
  ] as const)(
    "does not send input when the listed session has terminal evidence: %s",
    async (_, facts) => {
      const listed = session(facts);
      const { result } = renderHook(() => useNativeTerminalBridge("session-a", listed));
      await waitFor(() => expect(result.current.session?.sessionId).toBe("session-a"));

      await expect(
        result.current.bridge.sendInput("session-a", new Uint8Array([65])),
      ).rejects.toThrow();
      expect(nativeTerminalSendInput).not.toHaveBeenCalled();
    },
  );

  it.each([
    ["exitCode", { exitCode: 7 }],
    ["endReceived", { endReceived: true }],
    ["streamEof", { streamEof: true }],
    ["streamError", { streamError: "native output failed" }],
    ["cancelRequested", { cancelRequested: true }],
  ] as const)("does not send input when attach reports terminal evidence: %s", async (_, facts) => {
    vi.mocked(nativeTerminalAttach).mockResolvedValue(attachment({ session: session(facts) }));
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.session?.sessionId).toBe("session-a"));

    await expect(
      result.current.bridge.sendInput("session-a", new Uint8Array([65])),
    ).rejects.toThrow();
    expect(nativeTerminalSendInput).not.toHaveBeenCalled();
  });

  it("sends detach once for duplicate cleanup of the same subscription ID", async () => {
    vi.mocked(nativeTerminalAttach).mockResolvedValue(
      attachment({ subscriptionId: "duplicate-cleanup-sub" }),
    );
    const first = renderHook(() => useNativeTerminalBridge("session-a"));
    const second = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(first.result.current.feedStatus).toBe("active"));
    await waitFor(() => expect(second.result.current.feedStatus).toBe("active"));

    first.unmount();
    second.unmount();
    expect(nativeTerminalDetach).toHaveBeenCalledTimes(1);
    expect(nativeTerminalDetach).toHaveBeenCalledWith({
      sessionId: "session-a",
      subscriptionId: "duplicate-cleanup-sub",
    });
  });

  it("allows a later explicit cleanup to retry a failed detach for the same receiver", async () => {
    vi.mocked(nativeTerminalAttach).mockResolvedValue(
      attachment({
        session: session({ sessionId: "retryable-detach-session" }),
        subscriptionId: "retryable-detach-sub",
      }),
    );
    vi.mocked(nativeTerminalDetach).mockRejectedValueOnce(new Error("temporary detach failure"));
    const { result } = renderHook(() => useNativeTerminalBridge("retryable-detach-session"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));

    act(() => result.current.reattach());
    await waitFor(() => expect(nativeTerminalDetach).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));

    act(() => result.current.reattach());
    await waitFor(() => expect(nativeTerminalDetach).toHaveBeenCalledTimes(2));
    expect(nativeTerminalDetach).toHaveBeenNthCalledWith(2, {
      sessionId: "retryable-detach-session",
      subscriptionId: "retryable-detach-sub",
    });
  });

  it("forgets old successful detachments after a bounded history", async () => {
    const detach = vi.fn(async (_request: { sessionId: string; subscriptionId: string }) => {});
    const detachSession = createDetachSubscriptionManager(detach);
    for (let index = 0; index < 1000; index += 1) {
      await detachSession(`detach-history-${index}`, `subscription-${index}`);
    }

    expect(detach).toHaveBeenCalledTimes(1000);
    await detachSession("detach-history-0", "subscription-0");
    expect(detach).toHaveBeenCalledTimes(1001);
  });

  it("keeps an unknown attach blocked and only resets an input latch after a trusted reattach", async () => {
    vi.mocked(nativeTerminalAttach)
      .mockResolvedValueOnce(attachment())
      .mockResolvedValueOnce(attachment({ session: session({ state: "unknown" }) }))
      .mockResolvedValueOnce(attachment());
    vi.mocked(nativeTerminalSendInput).mockResolvedValueOnce("unknown");
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));

    await act(async () => {
      await expect(
        result.current.bridge.sendInput("session-a", new Uint8Array([65])),
      ).rejects.toThrow();
    });
    expect(result.current.feedStatus).toBe("unknown");

    act(() => result.current.reattach());
    await waitFor(() => expect(nativeTerminalAttach).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(result.current.session?.state).toBe("unknown"));
    expect(result.current.feedStatus).toBe("unknown");
    await expect(
      result.current.bridge.sendInput("session-a", new Uint8Array([66])),
    ).rejects.toThrow();
    expect(nativeTerminalSendInput).toHaveBeenCalledTimes(1);

    act(() => result.current.reattach());
    await waitFor(() => expect(nativeTerminalAttach).toHaveBeenCalledTimes(3));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    await expect(
      result.current.bridge.sendInput("session-a", new Uint8Array([67])),
    ).resolves.toBeUndefined();
    expect(nativeTerminalSendInput).toHaveBeenCalledTimes(2);
  });

  it("ignores a rejected input ACK from a previous attachment generation", async () => {
    const delivery = deferred<"accepted" | "unknown" | "unavailable">();
    vi.mocked(nativeTerminalAttach)
      .mockResolvedValueOnce(attachment())
      .mockResolvedValueOnce(
        attachment({ session: session({ sessionId: "session-b" }), subscriptionId: "sub-b" }),
      );
    vi.mocked(nativeTerminalSendInput).mockReturnValueOnce(delivery.promise);
    const { result, rerender } = renderHook(({ id }) => useNativeTerminalBridge(id), {
      initialProps: { id: "session-a" },
    });
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    const previousSend = result.current.bridge.sendInput("session-a", new Uint8Array([65]));
    rerender({ id: "session-b" });
    await waitFor(() => expect(result.current.session?.sessionId).toBe("session-b"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));

    await act(async () => {
      delivery.resolve("unknown");
      await expect(previousSend).rejects.toThrow();
    });
    expect(result.current.session?.sessionId).toBe("session-b");
    expect(result.current.feedStatus).toBe("active");
  });

  it("surfaces detach failure as a bridge error without changing the native session state", async () => {
    vi.mocked(nativeTerminalAttach)
      .mockResolvedValueOnce(attachment({ subscriptionId: "cleanup-error-sub" }))
      .mockResolvedValueOnce(attachment({ subscriptionId: "cleanup-error-sub-next" }));
    vi.mocked(nativeTerminalDetach).mockRejectedValueOnce(new Error("receiver removal failed"));
    const { result } = renderHook(() => useNativeTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));

    act(() => result.current.reattach());
    await waitFor(() => expect(nativeTerminalAttach).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(result.current.error).toMatch(/receiver removal failed/));
    expect(result.current.session?.state).toBe("running");
    expect(result.current.feedStatus).toBe("active");
  });
});
