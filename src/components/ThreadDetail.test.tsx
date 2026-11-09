import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { MemoryRow, ThreadSummary } from "@/types/api";
import { memoryDomId } from "./MarkdownMessage";
import { ThreadDetail } from "./ThreadDetail";

const findMemoriesByThreadId = vi.fn();
const findThread = vi.fn();
const getThreadIssueResolutionStatus = vi.fn();
const enqueueThreadIssueResolutionJob = vi.fn();
const getAppCapabilities = vi.fn();
vi.mock("@/api", () => ({
  getAppCapabilities: () => getAppCapabilities(),
  findMemoriesByThreadId: (req: unknown) => findMemoriesByThreadId(req),
  findThread: (id: string) => findThread(id),
  getThreadIssueResolutionStatus: (req: unknown) => getThreadIssueResolutionStatus(req),
  enqueueThreadIssueResolutionJob: (req: unknown) => enqueueThreadIssueResolutionJob(req),
}));

const progressMocks = vi.hoisted(() => ({
  progress: null as { job_id: string; status: string; message: string | null } | null,
  start: vi.fn(),
}));
vi.mock("@/hooks/useStepStreamProgress", () => ({
  useSummaryProgress: () => ({
    progress: progressMocks.progress,
    busy: progressMocks.progress?.status === "active",
    start: progressMocks.start,
    clear: vi.fn(),
    cancel: vi.fn(),
  }),
}));

// jsdom lacks both APIs the component relies on.
beforeAll(() => {
  globalThis.IntersectionObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
    takeRecords() {
      return [];
    }
    root = null;
    rootMargin = "";
    thresholds = [];
  } as unknown as typeof IntersectionObserver;
  Element.prototype.scrollIntoView = vi.fn();
});

function row(id: string, content: string, role = 2, metadata?: Record<string, unknown>): MemoryRow {
  return {
    id,
    role,
    content_type: 0,
    content,
    created_at_ms: 0,
    metadata: metadata ? JSON.stringify(metadata) : null,
  };
}

const thread: ThreadSummary = {
  id: "10",
  user_id: "1",
  description: "Thread T",
  channel: null,
  labels: [],
  created_at_ms: 0,
  updated_at_ms: 0,
};

describe("ThreadDetail", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    findThread.mockReset();
    getAppCapabilities.mockReset().mockResolvedValue({ experimental_issues_enabled: true });
    getThreadIssueResolutionStatus.mockReset().mockResolvedValue({ status: "NOT_STARTED" });
    enqueueThreadIssueResolutionJob
      .mockReset()
      .mockResolvedValue({ job_id_hint: "thread-resolution-job" });
    progressMocks.progress = null;
    progressMocks.start.mockReset();
    // Default: the synthesized-summary hydration finds nothing, so the header
    // falls back to the prop (matches the pre-hydration behaviour these tests
    // were written against).
    findThread.mockResolvedValue(null);
  });

  it("renders the first page of memories", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hello"), row("2", "world")]);
    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);
    expect(await screen.findByText("hello")).toBeInTheDocument();
    expect(screen.getByText("world")).toBeInTheDocument();
  });

  it("requests the centered offset for a search hit", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("7", "hit row")]);
    renderWithProviders(
      <ThreadDetail
        thread={thread}
        highlight={{ memoryId: "7", position: 350 }}
        onClose={() => {}}
      />,
    );
    await screen.findByText("hit row");
    // initialOffset(350) with PAGE_SIZE 100 => 300.
    expect(findMemoriesByThreadId).toHaveBeenCalledWith({
      thread_id: "10",
      limit: 100,
      offset: 300,
    });
  });

  it("marks the hit row with the highlight anchor id", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("7", "hit row")]);
    const { container } = renderWithProviders(
      <ThreadDetail
        thread={thread}
        highlight={{ memoryId: "7", position: 0 }}
        onClose={() => {}}
      />,
    );
    await screen.findByText("hit row");
    const el = container.querySelector(`#${CSS.escape(memoryDomId("7"))}`);
    expect(el).not.toBeNull();
    expect(el?.classList.contains("message-hit")).toBe(true);
  });

  it("shows the empty state when the thread has no memories", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([]);
    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);
    await waitFor(() => expect(screen.getByText("メッセージが見つかりません")).toBeInTheDocument());
  });

  it("shows only User and Assistant roles by default", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([
      row("1", "user message", 1),
      row("2", "assistant message", 2),
      row("3", "system message", 3),
      row("4", "tool message", 4),
      row("5", "meta message", 5),
      row("6", "reflection message", 6),
      row("7", "unspecified message", 0),
    ]);

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    expect(await screen.findByText("user message")).toBeInTheDocument();
    expect(screen.getByText("assistant message")).toBeInTheDocument();
    expect(screen.queryByText("system message")).toBeNull();
    expect(screen.queryByText("tool message")).toBeNull();
    expect(screen.queryByText("meta message")).toBeNull();
    expect(screen.queryByText("reflection message")).toBeNull();
    expect(screen.queryByText("unspecified message")).toBeNull();
    expect(screen.getByRole("button", { name: "User" })).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByRole("button", { name: "Assistant" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    expect(screen.getByRole("button", { name: "Tool" })).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByRole("button", { name: "Meta" })).toHaveAttribute("aria-pressed", "false");
  });

  it("toggles role visibility from the header controls", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([
      row("1", "user message", 1),
      row("2", "tool message", 4),
    ]);

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    expect(await screen.findByText("user message")).toBeInTheDocument();
    expect(screen.queryByText("tool message")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Tool" }));
    expect(screen.getByText("tool message")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Tool" })).toHaveAttribute("aria-pressed", "true");

    fireEvent.click(screen.getByRole("button", { name: "Tool" }));
    expect(screen.queryByText("tool message")).toBeNull();
    expect(screen.getByRole("button", { name: "Tool" })).toHaveAttribute("aria-pressed", "false");

    fireEvent.click(screen.getByRole("button", { name: "User" }));
    expect(screen.queryByText("user message")).toBeNull();
    expect(screen.getByRole("button", { name: "User" })).toHaveAttribute("aria-pressed", "false");
  });

  it("keeps a highlighted memory visible even when its role is hidden by default", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("7", "tool hit", 4)]);
    const { container } = renderWithProviders(
      <ThreadDetail
        thread={thread}
        highlight={{ memoryId: "7", position: 0 }}
        onClose={() => {}}
      />,
    );

    await screen.findByText(/Tool 出力/);
    const el = container.querySelector(`#${CSS.escape(memoryDomId("7"))}`);
    expect(el).not.toBeNull();
    expect(el?.classList.contains("message-hit")).toBe(true);
    expect(screen.getByRole("button", { name: "Tool" })).toHaveAttribute("aria-pressed", "false");
  });

  it("distinguishes empty filtered results from an empty thread", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "tool only", 4)]);

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    await waitFor(() =>
      expect(
        screen.getByText("表示中の Role に一致するメッセージがありません"),
      ).toBeInTheDocument(),
    );
    expect(screen.queryByText("メッセージが見つかりません")).toBeNull();
  });

  it("folds Codex injected user memories in the detail list", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([
      row("1", "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>x</INSTRUCTIONS>", 1, {
        source: "codex",
        kind: "user",
        payload_type: "message",
        block_type: "input_text",
      }),
    ]);

    const { container } = renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    expect(await screen.findByText("システムメッセージ")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });

  it("folds Codex injected AGENTS.md memories without a path suffix in the detail list", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([
      row("1", "# AGENTS.md instructions\n\n<INSTRUCTIONS>x</INSTRUCTIONS>", 1, {
        source: "codex",
        kind: "user",
        payload_type: "message",
        block_type: "input_text",
      }),
    ]);

    const { container } = renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    expect(await screen.findByText("システムメッセージ")).toBeInTheDocument();
    expect(container.querySelector("details.system-fold")).not.toBeNull();
  });

  it("hydrates channel/labels from find_thread when opened with a synthesized summary", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hi")]);
    // Cross-tab entry: prop has empty channel/labels; the fetched row fills them.
    findThread.mockResolvedValueOnce({
      ...thread,
      channel: "codex",
      // Deliberately out of priority order to prove the header sorts them.
      labels: ["dir:/proj", "agent:codex"],
    });

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    await screen.findByText("hi");
    expect(findThread).toHaveBeenCalledWith("10");
    // sortLabelsByPrefixPriority puts agent before dir.
    expect(
      await screen.findByText((_, el) => el?.textContent === "codex · agent:codex, dir:/proj"),
    ).toBeInTheDocument();
  });

  it("does not fetch the thread row when the prop already carries channel/labels", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hi")]);
    const full: ThreadSummary = { ...thread, channel: "codex", labels: ["agent:codex"] };

    renderWithProviders(<ThreadDetail thread={full} onClose={() => {}} />);

    await screen.findByText("hi");
    expect(findThread).not.toHaveBeenCalled();
    expect(
      screen.getByText((_, el) => el?.textContent === "codex · agent:codex"),
    ).toBeInTheDocument();
  });

  it("offers manual issue re-evaluation without a warning when it has never run", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hello")]);

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    const button = await screen.findByRole("button", { name: "Issue 解決を再評価" });
    expect(button).toBeEnabled();
    expect(getThreadIssueResolutionStatus).toHaveBeenCalledWith({ thread_id: "10" });
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByText("結果未確認、手動再評価可")).not.toBeInTheDocument();
  });

  it("hides experimental Issue controls when this build disables Issues", async () => {
    getAppCapabilities.mockResolvedValueOnce({ experimental_issues_enabled: false });
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hello")]);
    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    expect(await screen.findByText("hello")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Issue 解決を再評価" })).toBeNull();
    expect(getThreadIssueResolutionStatus).not.toHaveBeenCalled();
  });

  it.each([
    ["PENDING", "結果未確認、手動再評価可"],
    ["COMPLETE", "再評価結果を確認済み"],
    ["UNKNOWN", "状態不明です。手動で再評価してください。"],
  ])("shows %s without treating it as an enqueue success", async (status, label) => {
    getThreadIssueResolutionStatus.mockResolvedValueOnce({ status });
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hello")]);

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    expect(await screen.findByText(label)).toBeInTheDocument();
    if (status === "UNKNOWN") {
      expect(screen.getByRole("alert")).toHaveTextContent(label);
      expect(screen.getByRole("button", { name: "Issue 解決を再評価" })).toBeEnabled();
    } else {
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    }
  });

  it("shows the accepted job id, but does not claim that re-evaluation completed", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hello")]);

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    fireEvent.click(await screen.findByRole("button", { name: "Issue 解決を再評価" }));

    expect(
      await screen.findByText("受付済み・結果未確認（ジョブ ID: thread-resolution-job）"),
    ).toBeInTheDocument();
    expect(enqueueThreadIssueResolutionJob).toHaveBeenCalledWith({ target_thread_id: "10" });
    expect(progressMocks.start).toHaveBeenCalledWith("thread-resolution-job");
    expect(screen.queryByText("再評価結果を確認済み")).not.toBeInTheDocument();
  });

  it("disables the manual button only while the enqueue request is pending", async () => {
    let finishDispatch: ((value: { job_id_hint: string }) => void) | undefined;
    enqueueThreadIssueResolutionJob.mockImplementationOnce(
      () => new Promise((resolve) => (finishDispatch = resolve)),
    );
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hello")]);

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);
    const button = await screen.findByRole("button", { name: "Issue 解決を再評価" });
    fireEvent.click(button);

    expect(button).toBeDisabled();

    finishDispatch?.({ job_id_hint: "thread-resolution-job" });
    await waitFor(() => expect(button).toBeEnabled());
  });

  it("surfaces backend admission errors and keeps manual re-evaluation available", async () => {
    enqueueThreadIssueResolutionJob.mockRejectedValueOnce(
      new Error("Issue resolution mode is disabled"),
    );
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hello")]);

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);
    fireEvent.click(await screen.findByRole("button", { name: "Issue 解決を再評価" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("Issue resolution mode is disabled");
    expect(screen.getByRole("button", { name: "Issue 解決を再評価" })).toBeEnabled();
  });

  it("surfaces status read errors instead of hiding them", async () => {
    getThreadIssueResolutionStatus.mockRejectedValueOnce(new Error("memories unavailable"));
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hello")]);

    renderWithProviders(<ThreadDetail thread={thread} onClose={() => {}} />);

    expect(await screen.findByRole("alert")).toHaveTextContent("memories unavailable");
    expect(screen.getByRole("button", { name: "Issue 解決を再評価" })).toBeEnabled();
  });

  it("refreshes persisted status when the summary progress stream reaches a terminal event", async () => {
    findMemoriesByThreadId.mockResolvedValueOnce([row("1", "hello")]);
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const renderTree = () => (
      <I18nextProvider i18n={i18n}>
        <QueryClientProvider client={queryClient}>
          <ThreadDetail thread={thread} onClose={() => {}} />
        </QueryClientProvider>
      </I18nextProvider>
    );

    const { rerender } = render(renderTree());
    fireEvent.click(await screen.findByRole("button", { name: "Issue 解決を再評価" }));
    await screen.findByText("受付済み・結果未確認（ジョブ ID: thread-resolution-job）");
    const requestCountAfterEnqueue = getThreadIssueResolutionStatus.mock.calls.length;

    progressMocks.progress = {
      job_id: "thread-resolution-job",
      status: "done",
      message: "complete",
    };
    rerender(renderTree());

    await waitFor(() =>
      expect(getThreadIssueResolutionStatus.mock.calls.length).toBeGreaterThan(
        requestCountAfterEnqueue,
      ),
    );
  });

  it("reads the latest persisted status again when the thread detail is reopened", async () => {
    findMemoriesByThreadId.mockResolvedValue([row("1", "hello")]);
    getThreadIssueResolutionStatus
      .mockResolvedValueOnce({ status: "NOT_STARTED" })
      .mockResolvedValueOnce({ status: "COMPLETE" });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const renderTree = () => (
      <I18nextProvider i18n={i18n}>
        <QueryClientProvider client={queryClient}>
          <ThreadDetail thread={thread} onClose={() => {}} />
        </QueryClientProvider>
      </I18nextProvider>
    );

    const first = render(renderTree());
    await screen.findByRole("button", { name: "Issue 解決を再評価" });
    expect(getThreadIssueResolutionStatus).toHaveBeenCalledTimes(1);
    first.unmount();

    render(renderTree());
    await screen.findByRole("button", { name: "Issue 解決を再評価" });
    expect(await screen.findByText("再評価結果を確認済み")).toBeInTheDocument();
    expect(getThreadIssueResolutionStatus).toHaveBeenCalledTimes(2);
  });
});
