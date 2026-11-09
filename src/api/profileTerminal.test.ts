import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import {
  profileTerminalCancel,
  profileTerminalFinishInput,
  profileTerminalList,
  profileTerminalOutput,
  profileTerminalSendInput,
  profileTerminalSession,
  profileTerminalStart,
} from "./profileTerminal";

const session = {
  sessionId: "session",
  profileId: "profile",
  profileVersion: 3,
  settingsRevision: 4,
  imageRef: "registry.example/coder@sha256:aaa",
  state: "running",
  jobId: "9223372036854775807",
  exitCode: null,
  inputClosed: false,
};
describe("profile terminal boundary", () => {
  beforeEach(() => vi.clearAllMocks());
  it("passes profile identity and saved revision without an Issue or shell command", async () => {
    invokeMock.mockResolvedValue(session);
    expect(await profileTerminalStart({ expectedRevision: 4, profileId: "profile" })).toEqual(
      session,
    );
    expect(invokeMock).toHaveBeenLastCalledWith("coding_agent_profile_terminal_start", {
      request: { expectedRevision: 4, profileId: "profile" },
    });
    await profileTerminalSession("session");
    expect(invokeMock).toHaveBeenLastCalledWith("coding_agent_profile_terminal_session", {
      request: { sessionId: "session" },
    });
    invokeMock.mockResolvedValue([session]);
    expect(await profileTerminalList()).toEqual([session]);
    expect(invokeMock).toHaveBeenLastCalledWith("coding_agent_profile_terminal_list");
  });
  it("forwards exact bytes and explicit finish or cancel without manufacturing completion", async () => {
    invokeMock.mockResolvedValue(undefined);
    await profileTerminalSendInput("session", new Uint8Array([0, 255, 10]));
    expect(invokeMock).toHaveBeenLastCalledWith("coding_agent_profile_terminal_send_input", {
      request: { sessionId: "session", bytes: [0, 255, 10] },
    });
    await profileTerminalFinishInput("session");
    expect(invokeMock).toHaveBeenLastCalledWith("coding_agent_profile_terminal_finish_input", {
      request: { sessionId: "session" },
    });
    await profileTerminalCancel("session");
    expect(invokeMock).toHaveBeenLastCalledWith("coding_agent_profile_terminal_cancel", {
      request: { sessionId: "session" },
    });
  });
  it("rejects malformed session facts and output from another session", async () => {
    for (const changed of [
      { state: "done" },
      { jobId: 42 },
      { state: "closed", exitCode: null },
      { inputClosed: "false" },
    ]) {
      invokeMock.mockResolvedValue({ ...session, ...changed });
      await expect(profileTerminalSession("session")).rejects.toThrow();
    }
    invokeMock.mockResolvedValue({
      events: [
        {
          sessionId: "other",
          sequence: "1",
          event: { kind: "output", stream: "stdout", data: [1] },
        },
      ],
      nextSequence: "1",
    });
    await expect(profileTerminalOutput("session", "0")).rejects.toThrow();
    invokeMock.mockResolvedValue({
      events: [
        {
          sessionId: "session",
          sequence: "1",
          event: { kind: "output", stream: "stdout", data: [255] },
        },
      ],
      nextSequence: "1",
    });
    expect((await profileTerminalOutput("session", "0")).events.at(0)?.event).toEqual({
      kind: "output",
      stream: "stdout",
      data: [255],
    });
  });
});
