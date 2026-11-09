import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  profileTerminalCancel,
  profileTerminalOutput,
  profileTerminalSendInput,
  profileTerminalSession,
} from "@/api/profileTerminal";
import { useProfileSettingsTerminalBridge } from "./useProfileSettingsTerminalBridge";

vi.mock("@/api/profileTerminal", () => ({
  profileTerminalOutput: vi.fn(),
  profileTerminalSession: vi.fn(),
  profileTerminalSendInput: vi.fn(),
  profileTerminalCancel: vi.fn(),
}));
const session = {
  sessionId: "session-a",
  profileId: "profile",
  profileVersion: 3,
  settingsRevision: 4,
  imageRef: "image",
  state: "running" as const,
  jobId: "42",
  exitCode: null,
  inputClosed: false,
};
describe("profile settings terminal bridge", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(profileTerminalSession).mockResolvedValue(session);
    vi.mocked(profileTerminalOutput).mockImplementation(async (_id, after) => ({
      events: [],
      nextSequence: after,
    }));
    vi.mocked(profileTerminalSendInput).mockResolvedValue(undefined);
  });
  it("delivers binary output and input and only detaches the renderer on unmount", async () => {
    vi.mocked(profileTerminalOutput).mockResolvedValueOnce({
      events: [
        {
          sessionId: "session-a",
          sequence: "1",
          event: { kind: "output", stream: "stdout", data: [0, 255, 10] },
        },
      ],
      nextSequence: "1",
    });
    const { result, unmount } = renderHook(() => useProfileSettingsTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    const receive = vi.fn();
    let detach!: () => void;
    act(() => {
      detach = result.current.bridge.subscribeOutput("session-a", receive);
    });
    expect(receive).toHaveBeenCalledWith(new Uint8Array([0, 255, 10]));
    await act(async () => result.current.bridge.sendInput("session-a", new Uint8Array([4])));
    expect(profileTerminalSendInput).toHaveBeenCalledWith("session-a", new Uint8Array([4]));
    detach();
    unmount();
    expect(profileTerminalCancel).not.toHaveBeenCalled();
  });
  it("blocks further input after uncertain delivery and rejects another session identity", async () => {
    const { result } = renderHook(() => useProfileSettingsTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    await expect(result.current.bridge.sendInput("other", new Uint8Array([4]))).rejects.toThrow();
    vi.mocked(profileTerminalSendInput).mockRejectedValue(new Error("unknown input delivery"));
    await act(async () => {
      await expect(
        result.current.bridge.sendInput("session-a", new Uint8Array([4])),
      ).rejects.toThrow();
    });
    expect(result.current.feedStatus).toBe("disconnected");
    await expect(
      result.current.bridge.sendInput("session-a", new Uint8Array([4])),
    ).rejects.toThrow();
    expect(profileTerminalSendInput).toHaveBeenCalledTimes(1);
  });
  it("does not mistake a disconnected output feed for shell completion", async () => {
    vi.mocked(profileTerminalOutput).mockRejectedValue(new Error("output unavailable"));
    const { result } = renderHook(() => useProfileSettingsTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("disconnected"));
    expect(result.current.session?.state).not.toBe("closed");
    expect(profileTerminalCancel).not.toHaveBeenCalled();
  });

  it("does not replay displayed bytes when the same shell feed is reattached", async () => {
    vi.mocked(profileTerminalOutput).mockImplementation(async (id, after) =>
      after === "0"
        ? {
            events: [
              {
                sessionId: id,
                sequence: "1",
                event: { kind: "output", stream: "stdout", data: [7] },
              },
            ],
            nextSequence: "1",
          }
        : { events: [], nextSequence: after },
    );
    const { result } = renderHook(() => useProfileSettingsTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    const receive = vi.fn();
    act(() => {
      result.current.bridge.subscribeOutput("session-a", receive);
      result.current.reattach();
    });
    await waitFor(() => expect(profileTerminalOutput).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    expect(receive).toHaveBeenCalledTimes(1);
  });

  it("does not deliver another selected shell's output to an old renderer", async () => {
    vi.mocked(profileTerminalOutput).mockImplementation(async (id, after) =>
      after === "0"
        ? {
            events: [
              {
                sessionId: id,
                sequence: "1",
                event: { kind: "output", stream: "stdout", data: [7] },
              },
            ],
            nextSequence: "1",
          }
        : { events: [], nextSequence: after },
    );
    vi.mocked(profileTerminalSession).mockImplementation(async (id) => ({
      ...session,
      sessionId: id,
    }));
    const { result, rerender } = renderHook(({ id }) => useProfileSettingsTerminalBridge(id), {
      initialProps: { id: "session-a" },
    });
    await waitFor(() => expect(result.current.feedStatus).toBe("active"));
    const receive = vi.fn();
    act(() => {
      result.current.bridge.subscribeOutput("session-a", receive);
    });
    rerender({ id: "session-b" });
    await waitFor(() => expect(result.current.session?.sessionId).toBe("session-b"));
    expect(receive).toHaveBeenCalledTimes(1);
  });

  it("continues observing an unknown job until it actually closes", async () => {
    vi.mocked(profileTerminalSession)
      .mockResolvedValueOnce({ ...session, state: "unknown" })
      .mockResolvedValue({ ...session, state: "closed", exitCode: 0, inputClosed: true });
    const { result } = renderHook(() => useProfileSettingsTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.session?.state).toBe("unknown"));
    await waitFor(() => expect(result.current.session?.state).toBe("closed"), { timeout: 2500 });
  });

  it("drains the final output and End after a closed status races with a short page", async () => {
    vi.mocked(profileTerminalSession).mockResolvedValue({
      ...session,
      state: "closed",
      exitCode: 0,
      inputClosed: true,
    });
    vi.mocked(profileTerminalOutput)
      .mockResolvedValueOnce({
        events: [
          {
            sessionId: "session-a",
            sequence: "1",
            event: { kind: "output", stream: "stdout", data: [1] },
          },
        ],
        nextSequence: "1",
      })
      .mockResolvedValueOnce({
        events: [
          {
            sessionId: "session-a",
            sequence: "2",
            event: { kind: "output", stream: "stdout", data: [2] },
          },
          {
            sessionId: "session-a",
            sequence: "3",
            event: { kind: "exit", exitCode: 0, executionTimeMs: "1", sandboxId: "vm" },
          },
          { sessionId: "session-a", sequence: "4", event: { kind: "end" } },
        ],
        nextSequence: "4",
      });
    const { result } = renderHook(() => useProfileSettingsTerminalBridge("session-a"));
    await waitFor(() => expect(result.current.session?.state).toBe("closed"));
    const receive = vi.fn();
    act(() => {
      result.current.bridge.subscribeOutput("session-a", receive);
    });
    await waitFor(() => expect(receive).toHaveBeenCalledTimes(2));
    expect(receive).toHaveBeenLastCalledWith(new Uint8Array([2]));
    expect(profileTerminalOutput).toHaveBeenLastCalledWith("session-a", "1");
  });
});
