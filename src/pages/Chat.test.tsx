import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { ChatTurn, UseRagChat } from "@/hooks/useRagChat";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { ChatSource } from "@/types/api";
import { buildChatMarkdown, Chat, type ChatMarkdownLabels } from "./Chat";

// `findMemoryPosition` is overridden per-test to flip between the
// success and fallback branches of `resolveThreadHighlight`.
// `findMemoriesByThreadId` is what ThreadDetail calls once mounted;
// keep it minimal so the modal renders without errors.
vi.mock("@/api", () => ({
  getAppCapabilities: vi.fn().mockResolvedValue({ experimental_issues_enabled: false }),
  findMemoryPosition: vi.fn(),
  findMemoriesByThreadId: vi.fn().mockResolvedValue([]),
  saveChatMarkdown: vi.fn(),
  listPendingChatIssueOperations: vi.fn().mockResolvedValue([]),
  retryPendingChatIssueOperation: vi.fn(),
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({ save: vi.fn() }));

import { save } from "@tauri-apps/plugin-dialog";
import {
  findMemoryPosition,
  listPendingChatIssueOperations,
  retryPendingChatIssueOperation,
  saveChatMarkdown,
} from "@/api";

const mockFindMemoryPosition = vi.mocked(findMemoryPosition);
const mockSaveChatMarkdown = vi.mocked(saveChatMarkdown);
const mockSaveDialog = vi.mocked(save);

describe("pending Issue operation recovery", () => {
  it("shows only redacted pending identifiers and retries the selected operation", async () => {
    vi.mocked(listPendingChatIssueOperations).mockResolvedValueOnce([
      {
        operation_key: "issue-chat-update-1",
        issue_id: "issue-42",
        method: "manager_update_issue",
        created_at_ms: 1,
      },
    ]);
    vi.mocked(retryPendingChatIssueOperation).mockResolvedValueOnce({
      issue_id: "issue-42",
      operation_id: "op-1",
      operation_state: "COMMITTED",
      revision: "4",
      resulting_state: "OPEN",
    });
    renderChat([]);
    const retry = await screen.findByRole("button", { name: /再送|retry/i });
    fireEvent.click(retry);
    await waitFor(() =>
      expect(retryPendingChatIssueOperation).toHaveBeenCalledWith("issue-chat-update-1"),
    );
    expect(await screen.findByText(/COMMITTED/)).toBeInTheDocument();
  });

  it("keeps an uncertain operation visible when status recovery fails", async () => {
    vi.mocked(listPendingChatIssueOperations).mockResolvedValueOnce([
      {
        operation_key: "issue-chat-cancel-2",
        issue_id: "issue-43",
        method: "manager_apply_issue_admin_action",
        created_at_ms: 2,
      },
    ]);
    vi.mocked(retryPendingChatIssueOperation).mockRejectedValueOnce(new Error("unconfirmed"));
    renderChat([]);
    fireEvent.click(await screen.findByRole("button", { name: /再送|retry/i }));
    expect(await screen.findByText(/保留中|not confirmed/)).toBeInTheDocument();
    expect(screen.getByText(/issue-43/)).toBeInTheDocument();
  });
});

beforeAll(() => {
  // ThreadDetail relies on these jsdom-absent APIs.
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

function turn(over: Partial<ChatTurn> & { sources: ChatSource[] }): ChatTurn {
  return {
    jobId: "job-1",
    question: "test question",
    answer: "test answer",
    phase: "done",
    message: null,
    error: null,
    ...over,
  };
}

function makeRag(turns: ChatTurn[], overrides: Partial<UseRagChat> = {}): UseRagChat {
  return {
    turns,
    ask: vi.fn().mockResolvedValue(undefined),
    reset: vi.fn(),
    cancel: vi.fn().mockResolvedValue(undefined),
    busy: false,
    requireApproval: true,
    setRequireApproval: vi.fn().mockResolvedValue(undefined),
    respondToToolApproval: vi.fn().mockResolvedValue(undefined),
    setApprovalUiActive: vi.fn(),
    rejectPendingApprovals: vi.fn().mockResolvedValue(undefined),
    ...overrides,
  };
}

function markdownLabels(language: "ja" | "en"): ChatMarkdownLabels {
  const t = i18n.getFixedT(language);
  return {
    selectedSummaries: t("chat.export.selectedSummaries"),
    sources: t("chat.export.sources"),
    kind: t("chat.export.kind"),
    memoryId: t("chat.export.memoryId"),
    memory: (id) => t("chat.export.memory", { id }),
    selectedKind: (kind) => t(`chat.selectSummary.kind.${kind}`),
    sourceKind: (kind) => t(`chat.sourceKind.${kind}`),
  };
}

interface RenderOpts {
  onNavigate?: ReturnType<typeof vi.fn>;
  onNavigateSummariesFocus?: ReturnType<typeof vi.fn>;
  onNavigateThreadGroup?: ReturnType<typeof vi.fn>;
}

function renderChat(sources: ChatSource[], opts: RenderOpts = {}) {
  const onNavigate = opts.onNavigate ?? vi.fn();
  const onNavigateSummariesFocus = opts.onNavigateSummariesFocus ?? vi.fn();
  const onNavigateThreadGroup = opts.onNavigateThreadGroup ?? vi.fn();
  renderWithProviders(
    <Chat
      onNavigate={onNavigate}
      onNavigateSummariesFocus={onNavigateSummariesFocus}
      onNavigateThreadGroup={onNavigateThreadGroup}
      rag={makeRag([turn({ sources })])}
    />,
  );
  return { onNavigate, onNavigateSummariesFocus, onNavigateThreadGroup };
}

beforeEach(() => {
  i18n.changeLanguage("ja");
  mockFindMemoryPosition.mockReset();
  mockSaveChatMarkdown.mockReset();
  mockSaveDialog.mockReset();
  // Default to the fallback branch (memoryId-only highlight); tests
  // that exercise the success path override this.
  mockFindMemoryPosition.mockResolvedValue(null);
  mockSaveChatMarkdown.mockResolvedValue(undefined);
});

describe("Chat source click", () => {
  it("opens ThreadDetail in-place for raw_memory sources", async () => {
    const rawSource: ChatSource = {
      source_kind: "raw_memory",
      memory_id: "100",
      source_thread_id: "200",
      snippet: "snippet",
      score: 0.9,
    };
    const { onNavigate, onNavigateSummariesFocus } = renderChat([rawSource]);
    fireEvent.click(screen.getByTitle(/^原文:/));
    await waitFor(() => {
      expect(screen.getByRole("dialog")).toBeTruthy();
    });
    expect(onNavigate).not.toHaveBeenCalled();
    expect(onNavigateSummariesFocus).not.toHaveBeenCalled();
  });

  it("opens the origin thread for reflection sources", async () => {
    const reflectionSource: ChatSource = {
      source_kind: "reflection",
      reflection_id: "102",
      source_thread_id: "202",
      snippet: "reflection snippet",
      score: 0.8,
    };
    const { onNavigate, onNavigateSummariesFocus } = renderChat([reflectionSource]);
    fireEvent.click(screen.getByTitle(/^自省:/));
    await waitFor(() => {
      expect(screen.getByRole("dialog")).toBeTruthy();
    });
    expect(mockFindMemoryPosition).not.toHaveBeenCalled();
    expect(onNavigate).not.toHaveBeenCalled();
    expect(onNavigateSummariesFocus).not.toHaveBeenCalled();
  });

  it("passes the resolved position into ThreadDetail when findMemoryPosition succeeds", async () => {
    mockFindMemoryPosition.mockResolvedValue({ position: 5, thread_total: 20 });
    const rawSource: ChatSource = {
      source_kind: "raw_memory",
      memory_id: "300",
      source_thread_id: "400",
      snippet: "snippet",
      score: 0.9,
    };
    renderChat([rawSource]);
    fireEvent.click(screen.getByTitle(/^原文:/));
    await waitFor(() => {
      expect(screen.getByRole("dialog")).toBeTruthy();
    });
    expect(mockFindMemoryPosition).toHaveBeenCalledWith({
      thread_id: "400",
      memory_id: "300",
    });
  });

  it("deep-links monthly period_summary sources into the Summaries calendar", () => {
    const periodSource: ChatSource = {
      source_kind: "period_summary",
      memory_id: "102",
      period_key: "2026-05",
      scope_key: "_all",
      snippet: "period snippet",
      score: 0.7,
    };
    const { onNavigate, onNavigateSummariesFocus } = renderChat([periodSource]);
    fireEvent.click(screen.getByTitle(/^期間要約:/));
    expect(onNavigateSummariesFocus).toHaveBeenCalledWith({
      kind: "monthly",
      month: "2026-05",
      periodKey: "2026-05",
    });
    expect(onNavigate).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("deep-links daily period_summary sources into the matching month", () => {
    const periodSource: ChatSource = {
      source_kind: "period_summary",
      memory_id: "103",
      period_key: "2026-05-28",
      scope_key: "project-x",
      snippet: "period snippet",
      score: 0.7,
    };
    const { onNavigateSummariesFocus } = renderChat([periodSource]);
    fireEvent.click(screen.getByTitle(/^期間要約:/));
    expect(onNavigateSummariesFocus).toHaveBeenCalledWith({
      kind: "daily",
      month: "2026-05",
      periodKey: "2026-05-28",
    });
  });

  it("navigates thread-group summary sources to the owning group", () => {
    const groupSource: ChatSource = {
      source_kind: "thread_group_summary",
      memory_id: "105",
      group_id: "9007199254740993",
      snippet: "group summary",
      score: 0.8,
    };
    const { onNavigateThreadGroup } = renderChat([groupSource]);
    fireEvent.click(screen.getByTitle(/^スレッドグループ要約:/));
    expect(onNavigateThreadGroup).toHaveBeenCalledWith("9007199254740993");
  });

  it("falls back to a plain Summaries tab switch when period_key is malformed", () => {
    const periodSource: ChatSource = {
      source_kind: "period_summary",
      memory_id: "104",
      period_key: "garbage",
      scope_key: "_all",
      snippet: "period snippet",
      score: 0.7,
    };
    const { onNavigate, onNavigateSummariesFocus } = renderChat([periodSource]);
    fireEvent.click(screen.getByTitle(/^期間要約:/));
    expect(onNavigate).toHaveBeenCalledWith("summaries");
    expect(onNavigateSummariesFocus).not.toHaveBeenCalled();
  });
});

describe("Chat composer stop button — OPEN-CHAT-2", () => {
  function renderComposer(rag: UseRagChat) {
    renderWithProviders(<Chat onNavigate={vi.fn()} onNavigateSummariesFocus={vi.fn()} rag={rag} />);
  }

  it("shows the stop button instead of submit while busy", () => {
    const inFlight: ChatTurn = {
      jobId: "job-busy",
      question: "thinking out loud",
      answer: "thi",
      sources: [],
      phase: "token",
      message: null,
      error: null,
    };
    renderComposer(makeRag([inFlight], { busy: true }));
    expect(screen.queryByRole("button", { name: "送信" })).toBeNull();
    expect(screen.getByRole("button", { name: "停止" })).toBeTruthy();
  });

  it("invokes rag.cancel when the user clicks stop", () => {
    const inFlight: ChatTurn = {
      jobId: "job-busy",
      question: "thinking out loud",
      answer: "thi",
      sources: [],
      phase: "token",
      message: null,
      error: null,
    };
    const cancel = vi.fn().mockResolvedValue(undefined);
    renderComposer(makeRag([inFlight], { busy: true, cancel }));
    fireEvent.click(screen.getByRole("button", { name: "停止" }));
    expect(cancel).toHaveBeenCalledTimes(1);
  });

  it("restores the submit button once the turn settles", () => {
    const settled: ChatTurn = {
      jobId: "job-done",
      question: "ok",
      answer: "answer",
      sources: [],
      phase: "done",
      message: null,
      error: null,
    };
    renderComposer(makeRag([settled], { busy: false }));
    expect(screen.queryByRole("button", { name: "停止" })).toBeNull();
    expect(screen.getByRole("button", { name: "送信" })).toBeTruthy();
  });
});

describe("Chat tool approval", () => {
  const approval = {
    call_id: "call-9",
    tool_name: "lookback_issue_update",
    arguments_json: '{"issue_id":"issue-3","patch":{"title":"Updated title"}}',
  };

  function renderPendingApproval(overrides: Partial<UseRagChat> = {}) {
    const rag = makeRag(
      [
        turn({
          answer: "",
          phase: "approval-required",
          pendingApproval: approval,
          sources: [],
        }),
      ],
      { busy: true, ...overrides },
    );
    const view = renderWithProviders(
      <Chat onNavigate={vi.fn()} onNavigateSummariesFocus={vi.fn()} rag={rag} />,
    );
    return { ...view, rag };
  }

  it("shows the requested tool and exact arguments before offering approval", () => {
    renderPendingApproval();

    expect(screen.getByText("lookback_issue_update")).toBeInTheDocument();
    expect(screen.getByText(approval.arguments_json)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "承認して実行" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "拒否" })).toBeInTheDocument();
  });

  it("sends an explicit approval or rejection for the displayed call", async () => {
    const { rag } = renderPendingApproval();

    fireEvent.click(screen.getByRole("button", { name: "承認して実行" }));
    await waitFor(() => {
      expect(rag.respondToToolApproval).toHaveBeenCalledWith("job-1", "call-9", true);
    });
  });

  it("returns a rejection without executing the displayed call", async () => {
    const { rag } = renderPendingApproval();

    fireEvent.click(screen.getByRole("button", { name: "拒否" }));
    await waitFor(() => {
      expect(rag.respondToToolApproval).toHaveBeenCalledWith("job-1", "call-9", false);
    });
  });

  it("persists switching approval off from the default-on setting", () => {
    const setRequireApproval = vi.fn().mockResolvedValue(undefined);
    renderPendingApproval({ requireApproval: true, setRequireApproval });

    const toggle = screen.getByRole("checkbox", { name: "ツール実行前に承認を求める" });
    expect(toggle).toBeChecked();
    fireEvent.click(toggle);

    expect(setRequireApproval).toHaveBeenCalledWith(false);
  });

  it("keeps the pending request visible and reports a failed approval response", async () => {
    const respondToToolApproval = vi.fn().mockRejectedValue(new Error("ipc failed"));
    renderPendingApproval({ respondToToolApproval });

    fireEvent.click(screen.getByRole("button", { name: "承認して実行" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "ツールへの応答を送信できませんでした。もう一度お試しください。",
    );
    expect(screen.getByText("lookback_issue_update")).toBeInTheDocument();
  });

  it("does not offer approval when backend call details are missing", () => {
    renderWithProviders(
      <Chat
        onNavigate={vi.fn()}
        onNavigateSummariesFocus={vi.fn()}
        rag={makeRag([
          turn({ answer: "", phase: "approval-required", pendingApproval: null, sources: [] }),
        ])}
      />,
    );

    expect(
      screen.getByText("ツール呼び出しの内容を確認できません。安全のため実行は保留中です。"),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "承認して実行" })).toBeNull();
  });

  it("shows the read/write risk when automatic tool execution is enabled", () => {
    const setRequireApproval = vi.fn().mockResolvedValue(undefined);
    renderPendingApproval({ requireApproval: false, setRequireApproval });

    const toggle = screen.getByRole("checkbox", { name: "ツール実行前に承認を求める" });
    expect(toggle).not.toBeChecked();
    expect(screen.getByText(/読み取り・書き込み操作を含む/)).toBeInTheDocument();

    fireEvent.click(toggle);
    expect(setRequireApproval).toHaveBeenCalledWith(true);
  });

  it("rejects outstanding approvals when leaving the Chat tab", async () => {
    const { unmount, rag } = renderPendingApproval();

    unmount();

    await waitFor(() => expect(rag.rejectPendingApprovals).toHaveBeenCalledTimes(1));
  });
});

describe("Chat terminal status", () => {
  it("renders a committed Issue mutation even when the assistant returns no answer", () => {
    renderWithProviders(
      <Chat
        onNavigate={vi.fn()}
        onNavigateSummariesFocus={vi.fn()}
        rag={makeRag([
          turn({
            answer: "",
            phase: "done",
            sources: [],
            issueMutations: [
              {
                tool_name: "lookback_issue_update",
                issue_id: "issue-42",
                operation_id: "op-42",
                operation_state: "COMMITTED",
                audit_reason: "requested edit",
              },
            ],
          }),
        ])}
      />,
    );
    expect(screen.getByText(/issue-42/)).toBeTruthy();
    expect(screen.getByText(/COMMITTED/)).toBeTruthy();
    expect(screen.getByText(/requested edit/)).toBeTruthy();
  });
  it("localizes the empty terminal state instead of showing a perpetual generating state", () => {
    renderWithProviders(
      <Chat
        onNavigate={vi.fn()}
        onNavigateSummariesFocus={vi.fn()}
        rag={makeRag([
          turn({
            answer: "",
            phase: "done",
            message: null,
            sources: [],
          }),
        ])}
      />,
    );

    expect(screen.getByText("回答を生成できませんでした")).toBeTruthy();
    expect(screen.queryByText("回答を生成中…")).toBeNull();
  });

  it("uses the active locale for an empty terminal state", async () => {
    await i18n.changeLanguage("en");
    try {
      renderWithProviders(
        <Chat
          onNavigate={vi.fn()}
          onNavigateSummariesFocus={vi.fn()}
          rag={makeRag([turn({ answer: "", phase: "done", message: null, sources: [] })])}
        />,
      );

      expect(screen.getByText("No answer was generated.")).toBeTruthy();
      expect(screen.queryByText("回答を生成できませんでした")).toBeNull();
    } finally {
      await i18n.changeLanguage("ja");
    }
  });
});

describe("Chat selected memory display", () => {
  it("keeps the selected snapshot visible on the originating turn", () => {
    const selected = {
      memory_id: "9007199254740993",
      kind: "daily" as const,
      title: "今日の作業",
      content: "実装と検証を行った",
      captured_at_ms: 1,
      source_memory_ids: [],
      source_thread_ids: [],
    };
    renderWithProviders(
      <Chat
        onNavigate={vi.fn()}
        onNavigateSummariesFocus={vi.fn()}
        rag={makeRag([turn({ sources: [], selectedMemories: [selected] })])}
      />,
    );
    expect(screen.getByText("使用した要約 (1)")).toBeTruthy();
    expect(screen.getByText("今日の作業")).toBeTruthy();
    expect(screen.getByText("実装と検証を行った")).toBeTruthy();
  });

  it("localizes selected kind and truncation notice", async () => {
    await i18n.changeLanguage("en");
    const selected = {
      memory_id: "1",
      kind: "daily" as const,
      content: "work",
      captured_at_ms: 1,
      source_memory_ids: [],
      source_thread_ids: [],
    };
    renderWithProviders(
      <Chat
        onNavigate={vi.fn()}
        onNavigateSummariesFocus={vi.fn()}
        rag={makeRag([
          turn({ sources: [], selectedMemories: [selected], selectedContentTruncated: true }),
        ])}
      />,
    );
    expect(screen.getByText("Daily")).toBeTruthy();
    expect(
      screen.getByText("Some selected materials were omitted to fit the context capacity."),
    ).toBeTruthy();
  });

  it("renders reflection selections with the dedicated kind label", () => {
    const selected = {
      memory_id: "8",
      kind: "reflection" as const,
      content: "自省本文",
      captured_at_ms: 1,
    };
    renderWithProviders(
      <Chat
        onNavigate={vi.fn()}
        onNavigateSummariesFocus={vi.fn()}
        rag={makeRag([turn({ sources: [], selectedMemories: [selected] })])}
      />,
    );
    expect(screen.getByText("自省")).toBeInTheDocument();
  });

  it("offers the include sources option when only selected summaries were used", () => {
    const selected = {
      memory_id: "9007199254740993",
      kind: "daily" as const,
      content: "実装と検証を行った",
      captured_at_ms: 1,
      source_memory_ids: [],
      source_thread_ids: [],
    };
    renderWithProviders(
      <Chat
        onNavigate={vi.fn()}
        onNavigateSummariesFocus={vi.fn()}
        rag={makeRag([turn({ sources: [], selectedMemories: [selected] })])}
      />,
    );

    expect(screen.getByRole("checkbox", { name: "出典を含める" })).toBeTruthy();
  });

  it("reports a successful Markdown copy on the same turn", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    renderWithProviders(
      <Chat
        onNavigate={vi.fn()}
        onNavigateSummariesFocus={vi.fn()}
        rag={makeRag([turn({ sources: [] })])}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Markdownをコピー" }));
    expect(await screen.findByRole("status")).toHaveTextContent("コピーしました");
    expect(writeText).toHaveBeenCalledWith("test answer");
  });

  it("does not report success when the save dialog is cancelled", async () => {
    mockSaveDialog.mockResolvedValue(null);
    renderWithProviders(
      <Chat
        onNavigate={vi.fn()}
        onNavigateSummariesFocus={vi.fn()}
        rag={makeRag([turn({ sources: [] })])}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Markdownとして保存" }));

    await waitFor(() => expect(mockSaveDialog).toHaveBeenCalledTimes(1));
    expect(mockSaveChatMarkdown).not.toHaveBeenCalled();
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("reports success after saving Markdown to the selected path", async () => {
    mockSaveDialog.mockResolvedValue("/tmp/lookback.md");
    renderWithProviders(
      <Chat
        onNavigate={vi.fn()}
        onNavigateSummariesFocus={vi.fn()}
        rag={makeRag([turn({ sources: [] })])}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Markdownとして保存" }));

    expect(await screen.findByRole("status")).toHaveTextContent("保存しました");
    expect(mockSaveChatMarkdown).toHaveBeenCalledWith("/tmp/lookback.md", "test answer");
  });

  it("copies the same source-inclusive Markdown selected for export", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    renderWithProviders(
      <Chat
        onNavigate={vi.fn()}
        onNavigateSummariesFocus={vi.fn()}
        rag={makeRag([
          turn({
            sources: [
              {
                source_kind: "raw_memory",
                memory_id: "2",
                source_thread_id: "3",
                snippet: "retrieved detail",
                score: 1,
              },
            ],
          }),
        ])}
      />,
    );
    fireEvent.click(screen.getByRole("checkbox", { name: "出典を含める" }));
    fireEvent.click(screen.getByRole("button", { name: "Markdownをコピー" }));
    await waitFor(() => expect(writeText).toHaveBeenCalledWith(expect.stringContaining("## 出典")));
    expect(writeText).toHaveBeenCalledWith(expect.stringContaining("retrieved detail"));
  });

  it("does not offer copy or save for a cancelled partial answer", () => {
    renderWithProviders(
      <Chat
        onNavigate={vi.fn()}
        onNavigateSummariesFocus={vi.fn()}
        rag={makeRag([turn({ sources: [], answer: "partial", cancelled: true })])}
      />,
    );
    expect(screen.queryByRole("button", { name: "Markdownをコピー" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Markdownとして保存" })).toBeNull();
  });

  it("exports selected summaries separately from retrieved sources", () => {
    const selected = {
      memory_id: "1",
      kind: "daily" as const,
      content: "selected work",
      captured_at_ms: 1,
    };
    const markdown = buildChatMarkdown(
      turn({
        sources: [
          {
            source_kind: "raw_memory",
            memory_id: "2",
            source_thread_id: "3",
            snippet: "retrieved detail",
            score: 1,
          },
        ],
        selectedMemories: [selected],
      }),
      true,
      markdownLabels("en"),
    );
    expect(markdown).toContain("## Selected summaries");
    expect(markdown).toContain("- Kind: Daily");
    expect(markdown).not.toContain("selected work");
    expect(markdown).toContain("## Sources");
    expect(markdown).toContain("- Raw: retrieved detail");
    expect(markdown).toContain("retrieved detail");
  });

  it("exports Japanese labels instead of raw kind values", () => {
    const markdown = buildChatMarkdown(
      turn({
        sources: [
          {
            source_kind: "thread_group_summary",
            memory_id: "2",
            group_id: "3",
            snippet: "取得した詳細",
            score: 1,
          },
        ],
        selectedMemories: [
          {
            memory_id: "1",
            kind: "weekly",
            content: "選択した内容",
            captured_at_ms: 1,
            source_memory_ids: [],
            source_thread_ids: [],
          },
        ],
      }),
      true,
      markdownLabels("ja"),
    );
    expect(markdown).toContain("## 選択した要約");
    expect(markdown).toContain("- 種別: 週次");
    expect(markdown).toContain("- スレッドグループ要約: 取得した詳細");
    expect(markdown).not.toContain("thread_summary");
  });
});

describe("Chat source list collapse", () => {
  // Prefix the memory_id with the kind so concatenating breakdowns
  // doesn't accidentally produce duplicate React keys. ChatSource is a
  // discriminated union, so each branch returns the shape that matches
  // its kind.
  function makeSources(
    count: number,
    kind: ChatSource["source_kind"] = "raw_memory",
  ): ChatSource[] {
    return Array.from({ length: count }, (_, i): ChatSource => {
      const memory_id = `${kind}-${i}`;
      const snippet = `snippet ${i}`;
      const score = 0.5;
      if (kind === "period_summary") {
        return {
          source_kind: "period_summary",
          memory_id,
          period_key: `2026-05-${String(i % 28).padStart(2, "0")}`,
          scope_key: "_all",
          snippet,
          score,
        };
      }
      if (kind === "reflection") {
        return {
          source_kind: "reflection",
          reflection_id: memory_id,
          source_thread_id: `t-${kind}-${i}`,
          snippet,
          score,
        };
      }
      if (kind === "thread_group_summary") {
        return {
          source_kind: "thread_group_summary",
          memory_id,
          group_id: `g-${kind}-${i}`,
          snippet,
          score,
        };
      }
      return {
        source_kind: "raw_memory",
        memory_id,
        source_thread_id: `t-${kind}-${i}`,
        snippet,
        score,
      };
    });
  }

  it("renders the source list collapsed by default so 10 entries don't dominate the viewport", () => {
    renderChat(makeSources(10));
    // <details> defaults to closed in jsdom too; verify via the
    // attribute the browser exposes.
    const details = screen.getByText(/^出典 \(10\)$/).closest("details");
    expect(details).toBeTruthy();
    expect(details?.hasAttribute("open")).toBe(false);
  });

  it("breaks down the count by kind when the list mixes sources", () => {
    renderChat([
      ...makeSources(3, "raw_memory"),
      ...makeSources(2, "thread_group_summary"),
      ...makeSources(1, "period_summary"),
    ]);
    // The single-line summary must let the user know what's inside
    // before they commit to expanding the list.
    expect(screen.getByText("原文 3 / 期間要約 1 / スレッドグループ要約 2")).toBeTruthy();
  });

  it("omits the breakdown when every source is the same kind", () => {
    renderChat(makeSources(5, "raw_memory"));
    // "原文 5" would just repeat the "出典 (5)" count — drop it as noise.
    expect(screen.queryByText(/^原文 \d+/)).toBeNull();
    expect(screen.getByText("出典 (5)")).toBeTruthy();
  });
});
