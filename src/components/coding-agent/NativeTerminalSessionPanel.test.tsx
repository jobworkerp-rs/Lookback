import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { type ReactNode, useState } from "react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  type NativeTerminalPreview,
  type NativeTerminalSession,
  nativeTerminalCancel,
  nativeTerminalList,
  nativeTerminalPrepare,
  nativeTerminalStart,
  nativeTerminalStatus,
} from "@/api/nativeTerminal";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { NativeTerminalSessionPanel } from "./NativeTerminalSessionPanel";
import { useNativeTerminalBridge } from "./useNativeTerminalBridge";

vi.mock("@/api/nativeTerminal", () => ({
  nativeTerminalList: vi.fn(),
  nativeTerminalCancel: vi.fn(),
  nativeTerminalPrepare: vi.fn(),
  nativeTerminalStart: vi.fn(),
  nativeTerminalStatus: vi.fn(),
}));
vi.mock("./useNativeTerminalBridge", () => ({
  useNativeTerminalBridge: vi.fn(() => ({
    session: null,
    ownsSession: false,
    feedStatus: "unavailable",
    error: null,
    bridge: { subscribeOutput: () => () => {}, sendInput: vi.fn() },
    reattach: vi.fn(),
  })),
}));
vi.mock("./AgentTerminal", () => ({
  AgentTerminal: ({
    session,
    onCancelRequested,
  }: {
    session: {
      runId: string;
      status: string;
      jobId?: string;
      purpose: string;
      feedStatus: string;
    };
    onCancelRequested?: (runId: string) => void | Promise<void>;
  }) => (
    <section
      aria-label="Native terminal renderer fixture"
      data-job-id={session.jobId}
      data-status={session.status}
      data-feed-status={session.feedStatus}
      data-has-finish-work-control={session.purpose === "interactive-run"}
    >
      {session.runId}
      {onCancelRequested && (
        <button type="button" onClick={() => void onCancelRequested(session.runId)}>
          fixture native cancel
        </button>
      )}
    </section>
  ),
}));

const I64_MAX = "9223372036854775807";

function preview(overrides: Partial<NativeTerminalPreview> = {}): NativeTerminalPreview {
  return {
    previewId: "preview-1",
    previewDigest: "digest-1",
    sessionId: "session-1",
    profileId: "opencode",
    expectedSettingsRevision: "12",
    expectedIssueRevision: "27",
    repository: { host: "forge.example", organization: "team", repository: "repo" },
    baseBranch: "release/base",
    baseCommit: "a".repeat(40),
    command: "opencode",
    args: ["--prompt=Inspect this Issue"],
    workingDirectory: "/workspace/repo",
    initialPrompt: "Inspect this Issue",
    expectedCliVersion: "1.18.3",
    safeImageDigest: `sha256:${"b".repeat(64)}`,
    runtimeObserved: true,
    ...overrides,
  };
}

function session(overrides: Partial<NativeTerminalSession> = {}): NativeTerminalSession {
  return {
    sessionId: "session-1",
    previewId: "preview-1",
    profileId: "opencode",
    issueId: "issue-27",
    state: "starting",
    jobId: I64_MAX,
    exitCode: null,
    endReceived: false,
    streamEof: false,
    streamError: null,
    cancelRequested: false,
    ...overrides,
  };
}

function availableStatus() {
  return {
    available: true,
    blockers: [],
    profiles: [
      {
        profileId: "opencode",
        label: "OpenCode interactive",
        imageUri: "registry.example/opencode@sha256:abc",
        expectedCliVersion: "1.18.3",
        allowedBaseBranches: ["release/base"],
      },
    ],
  };
}

function IssuePanelHarness() {
  const [issueId, setIssueId] = useState("issue-27");
  return (
    <>
      <button type="button" onClick={() => setIssueId("issue-28")}>
        Open another Issue
      </button>
      <NativeTerminalSessionPanel issueId={issueId} />
    </>
  );
}

function renderWithClient(node: ReactNode) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return {
    queryClient,
    ...render(
      <I18nextProvider i18n={i18n}>
        <QueryClientProvider client={queryClient}>{node}</QueryClientProvider>
      </I18nextProvider>,
    ),
  };
}

describe("NativeTerminalSessionPanel", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    vi.resetAllMocks();
    vi.mocked(nativeTerminalList).mockResolvedValue([]);
    vi.mocked(nativeTerminalCancel).mockImplementation(async (sessionId) =>
      session({ sessionId, state: "running", cancelRequested: true }),
    );
    vi.mocked(nativeTerminalStatus).mockResolvedValue({
      available: false,
      blockers: ["native_cli_not_observed"],
      profiles: [],
    });
    vi.mocked(useNativeTerminalBridge).mockReturnValue({
      session: null,
      ownsSession: false,
      feedStatus: "unavailable",
      error: null,
      bridge: { subscribeOutput: () => () => {}, sendInput: vi.fn() },
      reattach: vi.fn(),
    });
  });

  it("shows native blockers and does not offer preparation or start while unavailable", async () => {
    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    expect(await screen.findByText("native_cli_not_observed")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "実行環境を準備" })).toBeDisabled();
    expect(screen.queryByRole("button", { name: "端末を起動" })).not.toBeInTheDocument();
    expect(nativeTerminalPrepare).not.toHaveBeenCalled();
    expect(nativeTerminalStart).not.toHaveBeenCalled();
  });

  it("prepares the repository standard branch without a manual branch selection and confirms the resolved preview", async () => {
    vi.mocked(nativeTerminalStatus).mockResolvedValue(availableStatus());
    vi.mocked(nativeTerminalPrepare).mockResolvedValue(preview());
    vi.mocked(nativeTerminalStart).mockResolvedValue(session());
    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);
    await screen.findByRole("option", { name: /OpenCode interactive/ });
    expect(screen.getByRole("option", { name: "標準branch（remote HEAD）" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "実行環境を準備" }));
    const start = await screen.findByRole("button", { name: "端末を起動" });
    expect(nativeTerminalPrepare).toHaveBeenCalledWith(expect.objectContaining({ baseBranch: "" }));
    expect(screen.getByRole("region", { name: "実行プレビュー" })).toHaveTextContent(
      "release/base",
    );
    fireEvent.click(start);
    await waitFor(() =>
      expect(nativeTerminalStart).toHaveBeenCalledWith(
        expect.objectContaining({ previewId: "preview-1", previewDigest: "digest-1" }),
      ),
    );
  });

  it("allows an explicit branch override, previews exact native facts, and starts only after an explicit click", async () => {
    vi.mocked(nativeTerminalStatus).mockResolvedValue(availableStatus());
    vi.mocked(nativeTerminalPrepare)
      .mockResolvedValueOnce(preview({ runtimeObserved: false }))
      .mockResolvedValueOnce(preview({ initialPrompt: "<script>plain text only</script>" }));
    vi.mocked(nativeTerminalStart).mockResolvedValue(session());
    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    await screen.findByRole("option", { name: /OpenCode interactive/ });
    await screen.findByLabelText("ベースブランチ");
    expect(screen.getByLabelText("ベースブランチ")).toHaveValue("");
    const prepare = screen.getByRole("button", { name: "実行環境を準備" });
    expect(prepare).toBeEnabled();
    fireEvent.change(screen.getByLabelText("ベースブランチ"), {
      target: { value: "release/base" },
    });
    fireEvent.change(screen.getByLabelText("初回プロンプト"), {
      target: { value: "Inspect this Issue" },
    });
    expect(prepare).toBeEnabled();

    fireEvent.click(prepare);
    expect(await screen.findByText(/Native runtime の観測を確認できません/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "端末を起動" })).not.toBeInTheDocument();
    expect(nativeTerminalStart).not.toHaveBeenCalled();

    fireEvent.click(prepare);
    const previewRegion = await screen.findByRole("region", { name: "実行プレビュー" });
    expect(previewRegion).toHaveTextContent("forge.example/team/repo");
    expect(previewRegion).toHaveTextContent("release/base");
    expect(previewRegion).toHaveTextContent("/workspace/repo");
    expect(previewRegion).toHaveTextContent("--prompt=Inspect this Issue");
    await waitFor(() => expect(screen.getByRole("button", { name: "端末を起動" })).toBeEnabled());
    expect(previewRegion.querySelector("script")).toBeNull();
    expect(previewRegion).toHaveTextContent("<script>plain text only</script>");
    expect(nativeTerminalPrepare).toHaveBeenCalledWith({
      issueId: "issue-27",
      profileId: "opencode",
      baseBranch: "release/base",
      prompt: "Inspect this Issue",
    });
    expect(nativeTerminalStart).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "端末を起動" }));
    await waitFor(() =>
      expect(nativeTerminalStart).toHaveBeenCalledWith({
        previewId: "preview-1",
        previewDigest: "digest-1",
      }),
    );
    expect(
      await screen.findByRole("region", { name: "Native terminal renderer fixture" }),
    ).toHaveAttribute("data-job-id", I64_MAX);
    expect(
      screen.getByRole("region", { name: "Native terminal renderer fixture" }),
    ).toHaveAttribute("data-has-finish-work-control", "false");
    expect(screen.queryByText(/Issue を完了|finish work|Done/)).not.toBeInTheDocument();
  });

  it("re-attaches a listed unknown session by its existing ID and displays its large job ID as text", async () => {
    vi.mocked(nativeTerminalStatus).mockResolvedValue(availableStatus());
    vi.mocked(nativeTerminalList).mockResolvedValue([
      session({ sessionId: "stored-session", state: "unknown", jobId: I64_MAX }),
    ]);
    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    const terminal = await screen.findByRole("region", {
      name: "Native terminal renderer fixture",
    });
    expect(terminal).toHaveTextContent("stored-session");
    expect(terminal).toHaveAttribute("data-job-id", I64_MAX);
    expect(terminal).toHaveAttribute("data-status", "unknown");
    expect(terminal).toHaveAttribute("data-has-finish-work-control", "false");
    expect(nativeTerminalStart).not.toHaveBeenCalled();
    expect(nativeTerminalPrepare).not.toHaveBeenCalled();
    expect(screen.queryByText(/Issue を完了|finish work|Done/)).not.toBeInTheDocument();
  });

  it("does not promote an unknown listed session from a stale running attachment", async () => {
    vi.mocked(nativeTerminalList).mockResolvedValue([session({ state: "unknown", jobId: null })]);
    vi.mocked(useNativeTerminalBridge).mockReturnValue({
      session: session({ state: "running" }),
      ownsSession: true,
      feedStatus: "active",
      error: null,
      bridge: { subscribeOutput: () => () => {}, sendInput: vi.fn() },
      reattach: vi.fn(),
    });

    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    const terminal = await screen.findByRole("region", {
      name: "Native terminal renderer fixture",
    });
    expect(terminal).toHaveAttribute("data-status", "unknown");
    expect(terminal).toHaveAttribute("data-feed-status", "unavailable");
  });

  it("does not carry a prompt, prepared preview, or selected session across Issue changes", async () => {
    vi.mocked(nativeTerminalStatus).mockResolvedValue(availableStatus());
    vi.mocked(nativeTerminalList).mockResolvedValue([
      session({ sessionId: "session-27", state: "unknown" }),
      session({ issueId: "issue-28", sessionId: "session-28", state: "unknown" }),
    ]);
    renderWithProviders(<IssuePanelHarness />);

    expect(await screen.findByText("session-27")).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText("初回プロンプト"), {
      target: { value: "Issue 27 only" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Open another Issue" }));

    expect(await screen.findByText("session-28")).toBeInTheDocument();
    expect(screen.getByLabelText("初回プロンプト")).toHaveValue("");
    expect(screen.getByLabelText("ベースブランチ")).toHaveValue("");
    expect(nativeTerminalPrepare).not.toHaveBeenCalled();
    expect(nativeTerminalStart).not.toHaveBeenCalled();
  });

  it("invalidates a preview irreversibly when edits return to their prepared values", async () => {
    vi.mocked(nativeTerminalStatus).mockResolvedValue(availableStatus());
    vi.mocked(nativeTerminalPrepare)
      .mockResolvedValueOnce(preview())
      .mockResolvedValueOnce(
        preview({ previewId: "fresh-preview", previewDigest: "fresh-digest" }),
      );
    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    await screen.findByRole("option", { name: /OpenCode interactive/ });
    fireEvent.change(screen.getByLabelText("ベースブランチ"), {
      target: { value: "release/base" },
    });
    fireEvent.change(screen.getByLabelText("初回プロンプト"), {
      target: { value: "Inspect this Issue" },
    });
    fireEvent.click(screen.getByRole("button", { name: "実行環境を準備" }));
    await screen.findByRole("button", { name: "端末を起動" });

    fireEvent.change(screen.getByLabelText("初回プロンプト"), {
      target: { value: "temporarily changed" },
    });
    fireEvent.change(screen.getByLabelText("初回プロンプト"), {
      target: { value: "Inspect this Issue" },
    });
    expect(screen.queryByRole("button", { name: "端末を起動" })).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "実行環境を準備" }));
    await screen.findByText("fresh-preview");
    await waitFor(() => expect(screen.getByRole("button", { name: "端末を起動" })).toBeEnabled());
    expect(nativeTerminalPrepare).toHaveBeenCalledTimes(2);
  });

  it("ignores an in-flight prepare response after edit and rollback, requiring a fresh prepare", async () => {
    let resolvePreview!: (value: NativeTerminalPreview) => void;
    const delayedPreview = new Promise<NativeTerminalPreview>((resolve) => {
      resolvePreview = resolve;
    });
    vi.mocked(nativeTerminalStatus).mockResolvedValue(availableStatus());
    vi.mocked(nativeTerminalPrepare)
      .mockReturnValueOnce(delayedPreview)
      .mockResolvedValueOnce(preview({ previewId: "new-generation" }));
    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    await screen.findByRole("option", { name: /OpenCode interactive/ });
    fireEvent.change(screen.getByLabelText("ベースブランチ"), {
      target: { value: "release/base" },
    });
    fireEvent.change(screen.getByLabelText("初回プロンプト"), {
      target: { value: "Inspect this Issue" },
    });
    fireEvent.click(screen.getByRole("button", { name: "実行環境を準備" }));
    await waitFor(() => expect(nativeTerminalPrepare).toHaveBeenCalledTimes(1));

    fireEvent.change(screen.getByLabelText("初回プロンプト"), {
      target: { value: "changed" },
    });
    fireEvent.change(screen.getByLabelText("初回プロンプト"), {
      target: { value: "Inspect this Issue" },
    });
    resolvePreview(preview());
    await act(async () => await delayedPreview);
    expect(screen.queryByRole("button", { name: "端末を起動" })).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "実行環境を準備" }));
    await screen.findByText("new-generation");
    expect(nativeTerminalPrepare).toHaveBeenCalledTimes(2);
  });

  it("disables start when the current native status turns unavailable after a ready preview", async () => {
    vi.mocked(nativeTerminalStatus)
      .mockResolvedValueOnce(availableStatus())
      .mockResolvedValueOnce({
        ...availableStatus(),
        available: false,
        blockers: ["runtime_lost"],
      });
    vi.mocked(nativeTerminalPrepare).mockResolvedValue(preview());
    const { queryClient } = renderWithClient(<NativeTerminalSessionPanel issueId="issue-27" />);

    await screen.findByRole("option", { name: /OpenCode interactive/ });
    fireEvent.change(screen.getByLabelText("ベースブランチ"), {
      target: { value: "release/base" },
    });
    fireEvent.click(screen.getByRole("button", { name: "実行環境を準備" }));
    const start = await screen.findByRole("button", { name: "端末を起動" });
    await waitFor(() => expect(start).toBeEnabled());

    await act(async () => {
      await queryClient.invalidateQueries({ queryKey: ["native-terminal-status", "issue-27"] });
    });
    expect(await screen.findByText("runtime_lost")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "端末を起動" })).not.toBeInTheDocument();
    expect(screen.getByRole("region", { name: "以前のプレビュー" })).toBeInTheDocument();
    expect(nativeTerminalStart).not.toHaveBeenCalled();
  });

  it("requires a new preview after the native status changes away and back", async () => {
    vi.mocked(nativeTerminalStatus).mockResolvedValue(availableStatus());
    vi.mocked(nativeTerminalPrepare).mockResolvedValue(preview());
    const { queryClient } = renderWithClient(<NativeTerminalSessionPanel issueId="issue-27" />);

    await screen.findByRole("option", { name: /OpenCode interactive/ });
    fireEvent.change(screen.getByLabelText("ベースブランチ"), {
      target: { value: "release/base" },
    });
    fireEvent.click(screen.getByRole("button", { name: "実行環境を準備" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "端末を起動" })).toBeEnabled());

    act(() => {
      queryClient.setQueryData(["native-terminal-status", "issue-27"], {
        ...availableStatus(),
        available: false,
        blockers: ["runtime_lost"],
      });
    });
    await screen.findByText("runtime_lost");
    act(() => {
      queryClient.setQueryData(["native-terminal-status", "issue-27"], availableStatus());
    });

    await waitFor(() => expect(screen.queryByRole("button", { name: "端末を起動" })).toBeNull());
    expect(screen.getByRole("region", { name: "以前のプレビュー" })).toBeInTheDocument();
    expect(nativeTerminalStart).not.toHaveBeenCalled();
  });

  it("applies the initial prompt UTF-8 byte limit without truncating or invoking prepare", async () => {
    vi.mocked(nativeTerminalStatus).mockResolvedValue(availableStatus());
    vi.mocked(nativeTerminalPrepare).mockResolvedValue(preview());
    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);
    await screen.findByRole("option", { name: /OpenCode interactive/ });
    fireEvent.change(screen.getByLabelText("ベースブランチ"), {
      target: { value: "release/base" },
    });
    const prompt = screen.getByLabelText("初回プロンプト");

    fireEvent.change(prompt, { target: { value: "a".repeat(64 * 1024) } });
    const prepare = screen.getByRole("button", { name: "実行環境を準備" });
    expect(prepare).toBeEnabled();
    fireEvent.click(prepare);
    await waitFor(() => expect(nativeTerminalPrepare).toHaveBeenCalledTimes(1));

    fireEvent.change(prompt, { target: { value: "a".repeat(64 * 1024 + 1) } });
    expect(prepare).toBeDisabled();
    expect(screen.getAllByText(/UTF-8.*64 KiB/).length).toBeGreaterThan(0);
    expect(nativeTerminalPrepare).toHaveBeenCalledTimes(1);

    fireEvent.change(prompt, { target: { value: "contains\u0000nul" } });
    expect(prepare).toBeDisabled();
    expect(screen.getByText(/NUL.*使用できません/)).toBeInTheDocument();
    expect(nativeTerminalPrepare).toHaveBeenCalledTimes(1);
  });

  it("sends an explicit stop request for an unknown native session without declaring it ended", async () => {
    vi.mocked(nativeTerminalList).mockResolvedValue([
      session({ sessionId: "uncertain-session", state: "unknown", jobId: null }),
    ]);
    vi.mocked(nativeTerminalCancel).mockResolvedValue(
      session({
        sessionId: "uncertain-session",
        state: "unknown",
        jobId: null,
        cancelRequested: true,
      }),
    );
    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    const stop = await screen.findByRole("button", { name: "停止を要求" });
    fireEvent.click(stop);
    await waitFor(() => expect(nativeTerminalCancel).toHaveBeenCalledWith("uncertain-session"));
    expect(screen.getByText(/停止要求中/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "停止を要求" })).not.toBeInTheDocument();
    expect(nativeTerminalStart).not.toHaveBeenCalled();
  });

  it("renders a native end returned by cancel instead of stale running list data", async () => {
    vi.mocked(nativeTerminalList).mockResolvedValue([
      session({ sessionId: "ending-session", state: "running" }),
    ]);
    vi.mocked(nativeTerminalCancel).mockResolvedValue(
      session({
        sessionId: "ending-session",
        state: "closed",
        exitCode: 143,
        cancelRequested: false,
      }),
    );
    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    fireEvent.click(await screen.findByRole("button", { name: "停止を要求" }));
    await waitFor(() => expect(nativeTerminalCancel).toHaveBeenCalledWith("ending-session"));
    expect(screen.getByText(/終了コード: 143/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "停止を要求" })).not.toBeInTheDocument();
  });

  it("preserves listed terminal facts over a stale running attachment and disables its feed", async () => {
    const staleAttachment = session({ state: "running" });
    vi.mocked(nativeTerminalList).mockResolvedValue([
      session({
        state: "closed",
        exitCode: 143,
        endReceived: true,
        streamEof: true,
        streamError: "native output ended",
      }),
    ]);
    vi.mocked(useNativeTerminalBridge).mockReturnValue({
      session: staleAttachment,
      ownsSession: true,
      feedStatus: "active",
      error: null,
      bridge: { subscribeOutput: () => () => {}, sendInput: vi.fn() },
      reattach: vi.fn(),
    });

    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    expect(await screen.findByText(/終了コード: 143/)).toBeInTheDocument();
    expect(screen.getByText("native output ended")).toBeInTheDocument();
    expect(screen.getByText(/Native 端末出力が終了/)).toBeInTheDocument();
    const terminal = screen.getByRole("region", { name: "Native terminal renderer fixture" });
    expect(terminal).toHaveAttribute("data-status", "finished");
    expect(terminal).toHaveAttribute("data-feed-status", "unavailable");
  });

  it("fails closed on conflicting native exit codes instead of choosing a snapshot", async () => {
    vi.mocked(nativeTerminalList).mockResolvedValue([session({ state: "closed", exitCode: 143 })]);
    vi.mocked(useNativeTerminalBridge).mockReturnValue({
      session: session({ state: "running", exitCode: 7 }),
      ownsSession: true,
      feedStatus: "active",
      error: null,
      bridge: { subscribeOutput: () => () => {}, sendInput: vi.fn() },
      reattach: vi.fn(),
    });

    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    expect(await screen.findByRole("alert")).toHaveTextContent(/conflicting native session facts/i);
    expect(
      screen.getByRole("region", { name: "Native terminal renderer fixture" }),
    ).toHaveAttribute("data-status", "unknown");
    expect(screen.queryByText(/終了コード: 143|終了コード: 7/)).not.toBeInTheDocument();
  });

  it("fails closed when native snapshots disagree on a non-null job ID", async () => {
    vi.mocked(nativeTerminalList).mockResolvedValue([session({ state: "running", jobId: "101" })]);
    vi.mocked(useNativeTerminalBridge).mockReturnValue({
      session: session({ state: "running", jobId: "102" }),
      ownsSession: true,
      feedStatus: "active",
      error: null,
      bridge: { subscribeOutput: () => () => {}, sendInput: vi.fn() },
      reattach: vi.fn(),
    });

    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    expect(await screen.findByRole("alert")).toHaveTextContent(/conflicting native session facts/i);
    expect(
      screen.getByRole("region", { name: "Native terminal renderer fixture" }),
    ).toHaveAttribute("data-status", "unknown");
  });

  it("does not present split end and EOF facts as a closed unknown session", async () => {
    vi.mocked(nativeTerminalList).mockResolvedValue([
      session({ state: "unknown", endReceived: true, streamEof: false }),
    ]);
    vi.mocked(useNativeTerminalBridge).mockReturnValue({
      session: session({ state: "unknown", endReceived: false, streamEof: true }),
      ownsSession: false,
      feedStatus: "unknown",
      error: null,
      bridge: { subscribeOutput: () => () => {}, sendInput: vi.fn() },
      reattach: vi.fn(),
    });

    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    const terminal = await screen.findByRole("region", {
      name: "Native terminal renderer fixture",
    });
    expect(terminal).toHaveAttribute("data-status", "unknown");
    expect(screen.getByText(/状態不明（読み取り専用）.*job/)).toBeInTheDocument();
  });

  it("keeps native state uncertain and does not retry when a stop request fails", async () => {
    vi.mocked(nativeTerminalList).mockResolvedValue([
      session({ sessionId: "uncertain-session", state: "unknown", jobId: null }),
    ]);
    vi.mocked(nativeTerminalCancel).mockRejectedValue(new Error("owner check failed"));
    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    fireEvent.click(await screen.findByRole("button", { name: "停止を要求" }));
    expect(await screen.findByText(/owner check failed/)).toBeInTheDocument();
    expect(screen.getByText("状態不明（読み取り専用）")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "停止を要求" })).toBeDisabled();
    expect(nativeTerminalCancel).toHaveBeenCalledTimes(1);
    expect(nativeTerminalStart).not.toHaveBeenCalled();
  });

  it("passes native cancellation through the shared terminal's explicit stop action", async () => {
    vi.mocked(nativeTerminalStart).mockResolvedValue(session({ state: "running" }));
    vi.mocked(nativeTerminalPrepare).mockResolvedValue(preview());
    vi.mocked(nativeTerminalStatus).mockResolvedValue(availableStatus());
    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    await screen.findByRole("option", { name: /OpenCode interactive/ });
    fireEvent.change(screen.getByLabelText("ベースブランチ"), {
      target: { value: "release/base" },
    });
    fireEvent.click(screen.getByRole("button", { name: "実行環境を準備" }));
    const start = await screen.findByRole("button", { name: "端末を起動" });
    await waitFor(() => expect(start).toBeEnabled());
    fireEvent.click(start);

    fireEvent.click(await screen.findByRole("button", { name: "fixture native cancel" }));
    await waitFor(() => expect(nativeTerminalCancel).toHaveBeenCalledWith("session-1"));
  });

  it("does not expose prepared start for a new session while an unknown session is listed", async () => {
    vi.mocked(nativeTerminalStatus).mockResolvedValue(availableStatus());
    vi.mocked(nativeTerminalList).mockResolvedValue([
      session({ sessionId: "uncertain-session", state: "unknown" }),
    ]);
    vi.mocked(nativeTerminalPrepare).mockResolvedValue(preview());
    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    await screen.findByText("uncertain-session");
    fireEvent.change(screen.getByLabelText("ベースブランチ"), {
      target: { value: "release/base" },
    });
    fireEvent.click(screen.getByRole("button", { name: "実行環境を準備" }));

    const start = await screen.findByRole("button", { name: "端末を起動" });
    expect(start).toBeDisabled();
    expect(screen.getByText(/状態不明の native session/)).toBeInTheDocument();
    expect(nativeTerminalStart).not.toHaveBeenCalled();
  });

  it("redacts credential-like argv values in the preview", async () => {
    vi.mocked(nativeTerminalStatus).mockResolvedValue(availableStatus());
    vi.mocked(nativeTerminalPrepare).mockResolvedValue(
      preview({
        args: [
          "--api-key=secret-one",
          "--password",
          "secret-two",
          "--header=Authorization: Bearer secret-three",
          "--registry=https://build-user:secret-four@registry.example/team",
          "--prompt=plain text",
        ],
      }),
    );
    renderWithProviders(<NativeTerminalSessionPanel issueId="issue-27" />);

    await screen.findByRole("option", { name: /OpenCode interactive/ });
    fireEvent.change(screen.getByLabelText("ベースブランチ"), {
      target: { value: "release/base" },
    });
    fireEvent.click(screen.getByRole("button", { name: "実行環境を準備" }));
    const previewRegion = await screen.findByRole("region", { name: "実行プレビュー" });

    expect(previewRegion).toHaveTextContent("--api-key=[REDACTED]");
    expect(previewRegion).toHaveTextContent("--password");
    expect(previewRegion).toHaveTextContent("--prompt=plain text");
    expect(previewRegion).not.toHaveTextContent("secret-one");
    expect(previewRegion).not.toHaveTextContent("secret-two");
    expect(previewRegion).not.toHaveTextContent("secret-three");
    expect(previewRegion).toHaveTextContent("--registry=https://[REDACTED]@registry.example/team");
    expect(previewRegion).not.toHaveTextContent("secret-four");
  });
});
