import { invoke } from "@tauri-apps/api/core";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  type NativeTerminalPreview,
  type NativeTerminalSession,
  nativeTerminalAttach,
  nativeTerminalCancel,
  nativeTerminalDetach,
  nativeTerminalList,
  nativeTerminalOutput,
  nativeTerminalPrepare,
  nativeTerminalSendInput,
  nativeTerminalStart,
  nativeTerminalStatus,
  parseNativeTerminalOutputEvent,
} from "./nativeTerminal";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const I64_MAX = "9223372036854775807";
const U64_MAX = "18446744073709551615";

function preview(overrides: Partial<NativeTerminalPreview> = {}): NativeTerminalPreview {
  return {
    previewId: "preview-a",
    previewDigest: "digest-a",
    sessionId: "session-a",
    profileId: "opencode",
    expectedSettingsRevision: "12",
    expectedIssueRevision: "9007199254740993",
    repository: { host: "forge.example", organization: "team", repository: "repo" },
    baseBranch: "feature/base",
    baseCommit: "a".repeat(40),
    command: "opencode",
    args: ["--prompt=Inspect this Issue literally"],
    workingDirectory: "/workspace/repo",
    initialPrompt: "Inspect this Issue literally",
    expectedCliVersion: "1.18.3",
    safeImageDigest: `sha256:${"b".repeat(64)}`,
    runtimeObserved: true,
    ...overrides,
  };
}

function session(overrides: Partial<NativeTerminalSession> = {}): NativeTerminalSession {
  return {
    sessionId: "session-a",
    previewId: "preview-a",
    profileId: "opencode",
    issueId: "issue-7",
    state: "running",
    jobId: I64_MAX,
    exitCode: null,
    endReceived: false,
    streamEof: false,
    streamError: null,
    cancelRequested: false,
    ...overrides,
  };
}

describe("native terminal API", () => {
  beforeEach(() => vi.resetAllMocks());

  it("uses the native status and prepare commands with the exact request envelope", async () => {
    vi.mocked(invoke)
      .mockResolvedValueOnce({ available: false, blockers: ["native_cli_missing"], profiles: [] })
      .mockResolvedValueOnce(preview());

    await expect(nativeTerminalStatus("issue-7")).resolves.toEqual({
      available: false,
      blockers: ["native_cli_missing"],
      profiles: [],
    });
    await expect(
      nativeTerminalPrepare({
        issueId: "issue-7",
        profileId: "opencode",
        baseBranch: "feature/base",
        prompt: "Inspect this Issue literally",
      }),
    ).resolves.toMatchObject({ runtimeObserved: true });

    expect(invoke).toHaveBeenNthCalledWith(1, "native_terminal_status", { issueId: "issue-7" });
    expect(invoke).toHaveBeenNthCalledWith(2, "native_terminal_prepare", {
      request: {
        issueId: "issue-7",
        profileId: "opencode",
        baseBranch: "feature/base",
        prompt: "Inspect this Issue literally",
      },
    });
  });

  it("rejects a preview whose native-runtime observation is absent or malformed", async () => {
    vi.mocked(invoke).mockResolvedValueOnce({ ...preview(), runtimeObserved: undefined });
    await expect(
      nativeTerminalPrepare({
        issueId: "issue-7",
        profileId: "opencode",
        baseBranch: "feature/base",
        prompt: null,
      }),
    ).rejects.toThrow(/runtimeObserved/i);

    vi.mocked(invoke).mockResolvedValueOnce({ ...preview(), runtimeObserved: "true" });
    await expect(
      nativeTerminalPrepare({
        issueId: "issue-7",
        profileId: "opencode",
        baseBranch: "feature/base",
        prompt: null,
      }),
    ).rejects.toThrow(/runtimeObserved/i);
  });

  it("accepts the UTF-8 prompt boundary and rejects NUL or oversized prompts before IPC", async () => {
    vi.mocked(invoke).mockResolvedValueOnce(preview());
    const boundaryPrompt = `${"あ".repeat(21_845)}a`;
    expect(new TextEncoder().encode(boundaryPrompt).byteLength).toBe(64 * 1024);
    await expect(
      nativeTerminalPrepare({
        issueId: "issue-7",
        profileId: "opencode",
        baseBranch: "main",
        prompt: boundaryPrompt,
      }),
    ).resolves.toBeDefined();
    expect(invoke).toHaveBeenCalledTimes(1);

    await expect(
      nativeTerminalPrepare({
        issueId: "issue-7",
        profileId: "opencode",
        baseBranch: "main",
        prompt: "あ".repeat(21_846),
      }),
    ).rejects.toThrow(/64 KiB UTF-8/i);
    await expect(
      nativeTerminalPrepare({
        issueId: "issue-7",
        profileId: "opencode",
        baseBranch: "main",
        prompt: "invalid\u0000prompt",
      }),
    ).rejects.toThrow(/NUL/i);
    expect(invoke).toHaveBeenCalledTimes(1);
  });

  it("preserves decimal identifiers as strings and rejects values outside their native integer bounds", async () => {
    vi.mocked(invoke).mockResolvedValueOnce(preview({ expectedIssueRevision: I64_MAX }));
    await expect(
      nativeTerminalPrepare({
        issueId: "issue-7",
        profileId: "opencode",
        baseBranch: "main",
        prompt: null,
      }),
    ).resolves.toMatchObject({ expectedIssueRevision: I64_MAX });

    vi.mocked(invoke).mockResolvedValueOnce(
      preview({ expectedIssueRevision: "9223372036854775808" }),
    );
    await expect(
      nativeTerminalPrepare({
        issueId: "issue-7",
        profileId: "opencode",
        baseBranch: "main",
        prompt: null,
      }),
    ).rejects.toThrow(/expectedIssueRevision/i);
  });

  it("validates and preserves start/list sessions without coercing a large job ID to Number", async () => {
    vi.mocked(invoke)
      .mockResolvedValueOnce(session())
      .mockResolvedValueOnce([session({ jobId: I64_MAX })]);

    await expect(
      nativeTerminalStart({ previewId: "preview-a", previewDigest: "digest-a" }),
    ).resolves.toMatchObject({ jobId: I64_MAX });
    await expect(nativeTerminalList("issue-7")).resolves.toMatchObject([{ jobId: I64_MAX }]);
    expect(invoke).toHaveBeenNthCalledWith(1, "native_terminal_start", {
      request: { previewId: "preview-a", previewDigest: "digest-a" },
    });
    expect(invoke).toHaveBeenNthCalledWith(2, "native_terminal_list", { issueId: "issue-7" });

    vi.mocked(invoke).mockResolvedValueOnce(session({ jobId: "9223372036854775808" }));
    await expect(
      nativeTerminalStart({ previewId: "preview-a", previewDigest: "digest-a" }),
    ).rejects.toThrow(/jobId/i);

    vi.mocked(invoke).mockResolvedValueOnce(
      session({ state: ["running"] as unknown as "running" }),
    );
    await expect(
      nativeTerminalStart({ previewId: "preview-a", previewDigest: "digest-a" }),
    ).rejects.toThrow(/state/i);

    vi.mocked(invoke).mockResolvedValueOnce({ ...session(), jobId: 123 });
    await expect(
      nativeTerminalStart({ previewId: "preview-a", previewDigest: "digest-a" }),
    ).rejects.toThrow(/jobId/i);

    for (const malformedState of [null, 17]) {
      vi.mocked(invoke).mockResolvedValueOnce(
        session({ state: malformedState as unknown as NativeTerminalSession["state"] }),
      );
      await expect(
        nativeTerminalStart({ previewId: "preview-a", previewDigest: "digest-a" }),
      ).rejects.toThrow(/state/i);
    }
  });

  it("attaches before replay and requests output using canonical u64 cursors", async () => {
    const attachment = {
      session: session(),
      events: [
        {
          sessionId: "session-a",
          sequence: U64_MAX,
          event: { kind: "output", stream: "stdout", data: [0, 255] },
        },
      ],
      nextSequence: U64_MAX,
      subscriptionId: "subscription-a",
    };
    vi.mocked(invoke).mockResolvedValueOnce(attachment).mockResolvedValueOnce({
      events: [],
      nextSequence: U64_MAX,
    });

    await expect(
      nativeTerminalAttach({ sessionId: "session-a", afterSequence: "0" }),
    ).resolves.toEqual(attachment);
    await expect(
      nativeTerminalOutput({ sessionId: "session-a", afterSequence: U64_MAX }),
    ).resolves.toEqual({
      events: [],
      nextSequence: U64_MAX,
    });
    expect(invoke).toHaveBeenNthCalledWith(1, "native_terminal_attach", {
      request: { sessionId: "session-a", afterSequence: "0" },
    });
    expect(invoke).toHaveBeenNthCalledWith(2, "native_terminal_output", {
      request: { sessionId: "session-a", afterSequence: U64_MAX },
    });
  });

  it("rejects noncanonical or out-of-range output cursors and invalid binary bytes", async () => {
    await expect(
      nativeTerminalAttach({ sessionId: "session-a", afterSequence: "01" }),
    ).rejects.toThrow(/afterSequence/i);
    vi.mocked(invoke).mockResolvedValueOnce({
      events: [
        {
          sessionId: "session-a",
          sequence: "1",
          event: { kind: "output", stream: "stdout", data: [256] },
        },
      ],
      nextSequence: "2",
    });
    await expect(
      nativeTerminalOutput({ sessionId: "session-a", afterSequence: "0" }),
    ).rejects.toThrow(/data/i);
    await expect(
      nativeTerminalOutput({ sessionId: "session-a", afterSequence: `${U64_MAX}0` }),
    ).rejects.toThrow(/afterSequence/i);
  });

  it("keeps output sequences unsigned while job IDs remain positive signed int64 values", async () => {
    const aboveI64 = "9223372036854775808";
    vi.mocked(invoke).mockResolvedValueOnce({
      session: session(),
      events: [
        {
          sessionId: "session-a",
          sequence: aboveI64,
          event: { kind: "output", stream: "stdout", data: [65] },
        },
      ],
      nextSequence: "9223372036854775809",
      subscriptionId: "subscription-a",
    });
    await expect(
      nativeTerminalAttach({ sessionId: "session-a", afterSequence: "0" }),
    ).resolves.toMatchObject({ events: [{ sequence: aboveI64 }] });

    vi.mocked(invoke).mockResolvedValueOnce(session({ jobId: aboveI64 }));
    await expect(
      nativeTerminalStart({ previewId: "preview-a", previewDigest: "digest-a" }),
    ).rejects.toThrow(/jobId/i);
  });

  it("sends one byte-preserving input request and never retries an ambiguous result", async () => {
    vi.mocked(invoke).mockResolvedValueOnce("unknown");
    await expect(nativeTerminalSendInput("session-a", new Uint8Array([0, 128, 255]))).resolves.toBe(
      "unknown",
    );

    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenCalledWith("native_terminal_send_input", {
      request: { sessionId: "session-a", bytes: [0, 128, 255] },
    });
  });

  it("cancels only the exact native session and detaches only its event receiver", async () => {
    vi.mocked(invoke)
      .mockResolvedValueOnce(session({ cancelRequested: true }))
      .mockResolvedValueOnce(null);

    await expect(nativeTerminalCancel("session-a")).resolves.toMatchObject({
      sessionId: "session-a",
      cancelRequested: true,
    });
    await expect(
      nativeTerminalDetach({ sessionId: "session-a", subscriptionId: "subscription-a" }),
    ).resolves.toBeUndefined();
    expect(invoke).toHaveBeenNthCalledWith(1, "native_terminal_cancel", {
      request: { sessionId: "session-a" },
    });
    expect(invoke).toHaveBeenNthCalledWith(2, "native_terminal_detach", {
      request: { sessionId: "session-a", subscriptionId: "subscription-a" },
    });

    vi.mocked(invoke).mockResolvedValueOnce(session({ sessionId: "different-session" }));
    await expect(nativeTerminalCancel("session-a")).rejects.toThrow(/sessionId/i);
  });

  it("accepts wrapped and direct native output event payloads without weakening record validation", () => {
    const outputRecord = {
      sessionId: "session-a",
      sequence: "1",
      event: { kind: "output", stream: "stdout", data: [0, 255] },
    };
    expect(
      parseNativeTerminalOutputEvent({ subscriptionId: "subscription-a", record: outputRecord }),
    ).toEqual({ subscriptionId: "subscription-a", record: outputRecord });
    expect(parseNativeTerminalOutputEvent(outputRecord)).toEqual({
      subscriptionId: null,
      record: outputRecord,
    });
    expect(
      parseNativeTerminalOutputEvent({
        subscriptionId: "subscription-a",
        record: { ...outputRecord, sequence: 1 },
      }),
    ).toBeNull();
  });
});
