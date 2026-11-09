import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  applyIssueAdminAction,
  codingAgentGetBackendStatus,
  codingAgentGetSettings,
  codingAgentListDelegationPlans,
  codingAgentListRuns,
  createExplicitIssueFromCandidate,
  getIssueDetail,
  getIssueEvidence,
  getIssueReviewPolicy,
  getWorkEligibility,
  listIssueDirectHistoryPage,
  listIssueHistory,
  listIssueReviewItems,
  listIssues,
  listPendingIssueOperations,
  reextractRejectedCandidate,
  resolveIssueReview,
  updateIssueCandidate,
} from "@/api";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type { IssueHistoryEntry, IssueHistoryPageOutcome, IssueListItem } from "@/types/api";
import { Issues } from "./Issues";
import { IssueDetailDialog } from "./issues/IssueDetailDialog";

vi.mock("@/api", () => ({
  applyIssueAdminAction: vi.fn(),
  codingAgentGetBackendStatus: vi.fn(),
  codingAgentGetSettings: vi.fn(),
  codingAgentListRuns: vi.fn(),
  codingAgentListDelegationPlans: vi.fn(),
  getIssueEvidence: vi.fn(),
  getIssueReviewPolicy: vi.fn(),
  getWorkEligibility: vi.fn(),
  listIssueHistory: vi.fn(),
  listIssueDirectHistoryPage: vi.fn(),
  listPendingIssueOperations: vi.fn(),
  listIssueReviewItems: vi.fn(),
  listIssues: vi.fn(),
  getIssueDuplicates: vi.fn(),
  getIssueDetail: vi.fn(),
  getLlmSettings: vi
    .fn()
    .mockRejectedValue(new Error("LLM settings unavailable in Issue fixtures")),
  resolveIssueReview: vi.fn(),
  updateIssueCandidate: vi.fn(),
  createExplicitIssueFromCandidate: vi.fn(),
  createIssueFromCanceled: vi.fn(),
  reextractRejectedCandidate: vi.fn(),
  transitionIssueState: vi.fn(),
}));
let summaryListener:
  | ((event: {
      payload: { job_id: string; status: "done" | "failed"; message?: string | null };
    }) => void)
  | null = null;
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((_event: string, handler: typeof summaryListener) => {
    summaryListener = handler;
    return Promise.resolve(() => {});
  }),
}));

const item = {
  id: "candidate-1",
  revision: "1",
  associated_issue_id: null,
  issue_id: null,
  title: "認証エラー",
  description: "再現する",
  kind: "BUG",
  priority: "P1" as const,
  suggested_state: "OPEN" as const,
  acceptance_criteria: [],
  confidence: 0.9,
  review_state: "candidate" as const,
  detection: "explicit" as const,
  strong_proof: false,
  evidence_count: 1,
  duplicate_count: 0,
  issue_state: null,
  integrity: "normal" as const,
  resolution_state: "actionable" as const,
  source_labels: [],
};

const historyEntry: IssueHistoryEntry = {
  id: "history-1",
  created_at_ms: 0,
  actor: "tester",
  decision: "CREATE",
  target_title: "Issue",
  issue_ids: ["issue-1"],
  rationale: null,
  state: "COMMITTED",
};

const historyIssueDetail: IssueListItem = {
  id: "issue-1",
  revision: "1",
  title: "Issue detail",
  description: "description",
  kind: "BUG",
  priority: "P1",
  state: "OPEN",
  review_state: "unreviewed",
  created_at_ms: 0,
  updated_at_ms: 0,
  completion_criteria: null,
  registration_path: null,
  policy: null,
  proof: null,
  source_labels: [],
  claim: null,
};

function mockHistoryDetail() {
  vi.mocked(listIssueHistory).mockResolvedValue({ entries: [historyEntry], truncated: false });
  vi.mocked(getIssueDetail).mockResolvedValue(historyIssueDetail);
}

function renderIssuesWithQueryClient() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}>
        <Issues />
      </QueryClientProvider>
    </I18nextProvider>,
  );
  return queryClient;
}

describe("Issues", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    summaryListener = null;
    vi.mocked(listIssueReviewItems).mockResolvedValue([item]);
    vi.mocked(codingAgentGetSettings).mockResolvedValue({
      settings: {
        formatVersion: 1,
        revision: 1,
        activeProfileId: "profile-a",
        semanticJudgeEnabled: true,
        autoDoneEnabled: false,
        profiles: [
          {
            profileId: "profile-a",
            version: 1,
            displayName: "Agent profile",
            imageRef: "registry.example/agent@sha256:abc",
            allowedImageRefs: ["registry.example/agent@sha256:abc"],
            command: ["agent"],
            containerHome: "/workspace",
            resources: {
              cpuMillis: 1000,
              memoryMib: 1024,
              rootDiskMib: 2048,
              maxParallelRuns: 1,
              useStatic: false,
            },
            mountAllowedRoots: [],
            persistentMounts: [],
            allowedRepositories: [],
            credentialReference: null,
          },
        ],
        repositoryHosts: [],
      },
      profileMountAvailability: [],
      availableAgentPresets: [],
    });
    vi.mocked(codingAgentGetBackendStatus).mockResolvedValue({
      enabled: false,
      executorVersion: null,
      executorProtocolVersion: null,
      executorFeatures: [],
      appManagedJobworkerpReady: false,
      blockers: ["coding_agent_executor_not_bundled", "host_workspace_freeze_unavailable"],
    });
    vi.mocked(codingAgentListRuns).mockResolvedValue([]);
    vi.mocked(codingAgentListDelegationPlans).mockResolvedValue([]);
    vi.mocked(listIssues).mockResolvedValue([]);
    vi.mocked(listIssueHistory).mockResolvedValue({ entries: [], truncated: false });
    vi.mocked(listIssueDirectHistoryPage).mockResolvedValue({
      kind: "page",
      page: { entries: [], next_page_token: null },
    });
    vi.mocked(listIssueDirectHistoryPage).mockClear();
    vi.mocked(listPendingIssueOperations).mockResolvedValue([]);
    vi.mocked(getIssueReviewPolicy).mockResolvedValue({ require_high_priority_reason: false });
    vi.mocked(getWorkEligibility).mockResolvedValue({ work_eligibility: "REVIEWED_ONLY" });
    vi.mocked(getIssueEvidence).mockResolvedValue([
      {
        thread_id: "1",
        memory_id: "2",
        excerpt: "原文",
        availability: "available",
        unavailable_reason: null,
      },
    ]);
    vi.mocked(resolveIssueReview).mockResolvedValue();
    vi.mocked(reextractRejectedCandidate).mockResolvedValue({ job_id_hint: "unused" });
  });

  it("collapses the review label filter and applies OR and AND matching", async () => {
    const reviewA = {
      ...item,
      id: "candidate-a",
      title: "レビュー対象A",
      source_labels: ["team:a"],
    };
    const reviewB = {
      ...item,
      id: "candidate-b",
      title: "レビュー対象B",
      source_labels: ["team:b"],
    };
    const reviewBoth = {
      ...item,
      id: "candidate-both",
      title: "レビュー対象両方",
      source_labels: ["team:a", "team:b"],
    };
    vi.mocked(listIssueReviewItems).mockResolvedValue([reviewA, reviewB, reviewBoth]);

    renderWithProviders(<Issues />);

    const summary = await screen.findByText(/(ラベルで絞り込む|Filter by label) \(0\)/);
    const details = summary.closest("details");
    expect(details).not.toHaveAttribute("open");
    fireEvent.click(summary);
    expect(details).toHaveAttribute("open");

    fireEvent.click(screen.getByRole("button", { name: "team:a" }));
    expect(details).toHaveAttribute("open");
    await waitFor(() => {
      expect(screen.getByText("レビュー対象A")).toBeInTheDocument();
      expect(screen.queryByText("レビュー対象B")).not.toBeInTheDocument();
    });

    fireEvent.click(screen.getByRole("button", { name: "team:b" }));
    await waitFor(() => {
      expect(screen.getByText("レビュー対象A")).toBeInTheDocument();
      expect(screen.getByText("レビュー対象B")).toBeInTheDocument();
      expect(screen.getByText("レビュー対象両方")).toBeInTheDocument();
    });

    fireEvent.click(screen.getByRole("button", { name: "AND" }));
    await waitFor(() => {
      expect(screen.queryByText("レビュー対象A")).not.toBeInTheDocument();
      expect(screen.queryByText("レビュー対象B")).not.toBeInTheDocument();
      expect(screen.getByText("レビュー対象両方")).toBeInTheDocument();
    });
  });

  it("renders review and inventory label filters in rows below their headers", async () => {
    vi.mocked(listIssueReviewItems).mockResolvedValue([{ ...item, source_labels: ["review:one"] }]);
    vi.mocked(listIssues).mockResolvedValue([
      { ...historyIssueDetail, source_labels: ["issue:one"] },
    ]);
    renderWithProviders(<Issues />);

    const reviewSummary = await screen.findByText(/(ラベルで絞り込む|Filter by label) \(0\)/);
    expect(reviewSummary.closest(".issues-label-filter-row")).toBeInTheDocument();
    expect(reviewSummary.closest(".issues-panel-head")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /Issues|Issue 一覧/ }));
    const inventorySummary = await screen.findByText(/(ラベルで絞り込む|Filter by label) \(0\)/);
    expect(inventorySummary.closest(".issues-label-filter-row")).toBeInTheDocument();
    expect(inventorySummary.closest(".issues-panel-head")).not.toBeInTheDocument();
  });

  it("filters review items by toggling a row label and clears on the second click", async () => {
    vi.mocked(listIssueReviewItems).mockResolvedValue([
      { ...item, id: "candidate-a", title: "レビュー対象A", source_labels: ["team:a"] },
      { ...item, id: "candidate-b", title: "レビュー対象B", source_labels: ["team:b"] },
    ]);

    renderWithProviders(<Issues />);
    const rowLabel = /Filter by label.*team:a|Remove label filter.*team:a|ラベル.*team:a/;
    const labelButton = await screen.findByRole("button", { name: rowLabel });
    expect(labelButton).toHaveAttribute("aria-pressed", "false");

    fireEvent.click(labelButton);
    await waitFor(() => {
      expect(screen.getByText("レビュー対象A")).toBeInTheDocument();
      expect(screen.queryByText("レビュー対象B")).not.toBeInTheDocument();
    });
    expect(screen.getByRole("button", { name: rowLabel })).toHaveAttribute("aria-pressed", "true");

    fireEvent.click(screen.getByRole("button", { name: rowLabel }));
    await waitFor(() => {
      expect(screen.getByText("レビュー対象A")).toBeInTheDocument();
      expect(screen.getByText("レビュー対象B")).toBeInTheDocument();
    });
  });

  it("truncates inventory row labels and expands them locally", async () => {
    const issue = {
      ...historyIssueDetail,
      title: "ラベル付き Issue",
      source_labels: ["one", "two", "three"],
    };
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    vi.mocked(listIssues).mockResolvedValue([issue]);

    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: /Issues|Issue 一覧/ }));

    await screen.findByRole("button", { name: "ラベル付き Issue" });
    expect(screen.queryByText("issue-1")).not.toBeInTheDocument();
    const copyIdButton = screen.getByRole("button", { name: /(?:ID をコピー|Copy ID): issue-1/ });
    expect(copyIdButton).toHaveAccessibleName(/(?:ID をコピー|Copy ID): issue-1/);
    fireEvent.click(copyIdButton);
    expect(writeText).toHaveBeenCalledWith("issue-1");
    expect(screen.getByRole("columnheader", { name: "タイトル" })).toBeInTheDocument();
    expect(screen.queryByRole("columnheader", { name: "ID / タイトル" })).not.toBeInTheDocument();
    const labels = screen.getByRole("group", { name: "ソースラベル" });
    expect(labels).toHaveAttribute("title", "one, two, three");
    const oneLabel = screen.getByRole("button", { name: /Filter by label.*one|ラベル.*one/ });
    expect(oneLabel).toHaveAttribute("aria-pressed", "false");
    expect(oneLabel).not.toHaveClass("active");
    expect(
      screen.getByRole("button", { name: /Filter by label.*two|ラベル.*two/ }),
    ).toHaveAttribute("aria-pressed", "false");
    expect(
      screen.queryByRole("button", { name: /Filter by label.*three|ラベル.*three/ }),
    ).not.toBeInTheDocument();

    const showMoreLabels = /残り 1 件のラベルを表示|Show 1 more labels/;
    writeText.mockClear();
    fireEvent.click(screen.getByRole("button", { name: showMoreLabels }));
    expect(vi.mocked(writeText)).not.toHaveBeenCalled();
    const threeLabel = screen.getByRole("button", { name: /Filter by label.*three|ラベル.*three/ });
    expect(threeLabel).toHaveAttribute("aria-pressed", "false");
    expect(threeLabel).not.toHaveClass("active");
    fireEvent.click(threeLabel);
    expect(threeLabel).toHaveAttribute("aria-pressed", "true");
    expect(threeLabel).not.toHaveClass("active");
    expect(screen.queryByRole("button", { name: showMoreLabels })).not.toBeInTheDocument();
  });

  it("shows and copies the full Issue ID in the detail dialog", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    vi.mocked(listIssues).mockResolvedValue([
      { ...historyIssueDetail, source_labels: ["detail:one"] },
    ]);

    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: /Issues|Issue 一覧/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Issue detail" }));

    const detailDialog = await screen.findByRole("dialog", { name: "Issue 詳細" });
    expect(screen.getByText("ID")).toBeInTheDocument();
    expect(screen.getAllByText("issue-1")).toHaveLength(1);
    const copyButtons = screen.getAllByRole("button", {
      name: /(?:ID をコピー|Copy ID): issue-1/,
    });
    expect(copyButtons).toHaveLength(2);
    fireEvent.click(within(detailDialog).getByRole("button", { name: /ID をコピー|Copy ID/ }));
    expect(writeText).toHaveBeenLastCalledWith("issue-1");

    fireEvent.click(within(detailDialog).getByRole("button", { name: "detail:one をコピー" }));
    expect(writeText).toHaveBeenLastCalledWith("detail:one");
  });

  it("adds the coding-agent entry to Issue details without implying execution readiness", async () => {
    vi.mocked(listIssues).mockResolvedValue([historyIssueDetail]);
    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: /Issues|Issue 一覧/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Issue detail" }));

    expect(await screen.findByRole("region", { name: "Coding agent" })).toBeInTheDocument();
    expect(await screen.findByText(/実行環境は利用できません/)).toBeInTheDocument();
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "実行計画をプレビュー" })).toBeEnabled(),
    );
    expect(screen.queryByText(/Ready|実行を開始しました/)).not.toBeInTheDocument();
  });

  it("does not offer the generic Issue completion action while the Issue awaits coding-agent review", async () => {
    vi.mocked(listIssues).mockResolvedValue([{ ...historyIssueDetail, state: "AWAITING_REVIEW" }]);
    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: /Issues|Issue 一覧/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Issue detail" }));

    const adminActions = await screen.findByRole("heading", { name: "人間による管理操作" });
    const adminSection = adminActions.closest(".issues-admin-actions");
    if (!(adminSection instanceof HTMLElement)) {
      throw new Error("Issue administration actions are missing");
    }
    expect(within(adminSection).queryByRole("button", { name: "完了" })).toBeNull();
  });

  it("revokes an open generic completion form when the canonical Issue refetch enters coding-agent review", async () => {
    vi.mocked(listIssueHistory).mockResolvedValue({ entries: [historyEntry], truncated: false });
    vi.mocked(getIssueDetail)
      .mockResolvedValueOnce(historyIssueDetail)
      .mockResolvedValueOnce({
        ...historyIssueDetail,
        revision: "2",
        state: "AWAITING_REVIEW",
      });
    const queryClient = renderIssuesWithQueryClient();

    fireEvent.click(await screen.findByRole("button", { name: "判断履歴" }));
    fireEvent.click(await screen.findByRole("button", { name: "Issue issue-1 の詳細を開く" }));
    await waitFor(() => expect(getIssueDetail).toHaveBeenCalledWith("issue-1"));
    await waitFor(() =>
      expect(queryClient.getQueryData(["issue-detail", "issue-1"])).toEqual(historyIssueDetail),
    );
    const dialog = await screen.findByRole("dialog", { name: "Issue 詳細" });
    await within(dialog).findByText("Issue detail");
    fireEvent.click(within(dialog).getByRole("button", { name: "完了" }));
    fireEvent.change(within(dialog).getByLabelText("操作理由"), {
      target: { value: "Completed after verification." },
    });
    fireEvent.change(within(dialog).getByLabelText("検証不能理由"), {
      target: { value: "Cannot run the external check." },
    });
    fireEvent.change(within(dialog).getByLabelText("残存 risk"), {
      target: { value: "External system unavailable." },
    });

    await act(async () => {
      await queryClient.invalidateQueries({ queryKey: ["issue-detail", "issue-1"] });
    });

    expect(getIssueDetail).toHaveBeenCalledTimes(2);
    expect(queryClient.getQueryData(["issue-detail", "issue-1"])).toMatchObject({
      revision: "2",
      state: "AWAITING_REVIEW",
    });
    expect(await within(dialog).findByText("AWAITING_REVIEW")).toBeInTheDocument();
    expect(within(dialog).queryByRole("button", { name: "実行" })).not.toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "取消" })).toBeInTheDocument();
    expect(applyIssueAdminAction).not.toHaveBeenCalled();
  });

  it("does not retarget an open admin form when the detail Issue changes and still permits a fresh CANCEL action", async () => {
    const queryClient = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    const onAdminAction = vi.fn();
    const props = {
      busy: false,
      error: null,
      onClose: vi.fn(),
      onTransition: vi.fn(),
      onCreateFromCanceled: vi.fn(),
      onAdminAction,
      diagnostics: [],
      operationStatus: null,
      onResolveOperation: vi.fn(),
      onInspectOperation: vi.fn(),
      onRefreshAfterError: vi.fn().mockResolvedValue(undefined),
      historyRefreshIssueId: null,
      historyRefreshToken: 0,
    };
    const renderDialog = (item: IssueListItem) => (
      <I18nextProvider i18n={i18n}>
        <QueryClientProvider client={queryClient}>
          <IssueDetailDialog {...props} item={item} />
        </QueryClientProvider>
      </I18nextProvider>
    );
    const { rerender } = render(renderDialog(historyIssueDetail));
    const dialog = await screen.findByRole("dialog", { name: "Issue 詳細" });
    fireEvent.click(within(dialog).getByRole("button", { name: "完了" }));
    fireEvent.change(within(dialog).getByLabelText("操作理由"), {
      target: { value: "Stale completion reason." },
    });
    fireEvent.change(within(dialog).getByLabelText("検証不能理由"), {
      target: { value: "Stale completion evidence." },
    });
    fireEvent.change(within(dialog).getByLabelText("残存 risk"), {
      target: { value: "Stale residual risk." },
    });

    const nextIssue = { ...historyIssueDetail, id: "issue-2", revision: "9" };
    rerender(renderDialog(nextIssue));

    expect(await within(dialog).findByText("issue-2")).toBeInTheDocument();
    expect(within(dialog).queryByRole("button", { name: "実行" })).not.toBeInTheDocument();
    expect(onAdminAction).not.toHaveBeenCalled();

    fireEvent.click(within(dialog).getByRole("button", { name: "取消" }));
    fireEvent.change(within(dialog).getByLabelText("操作理由"), {
      target: { value: "Stop the current issue." },
    });
    fireEvent.click(within(dialog).getByRole("button", { name: "実行" }));
    expect(onAdminAction).toHaveBeenCalledWith(
      expect.objectContaining({
        issue_id: "issue-2",
        expected_revision: "9",
        action: "CANCEL",
      }),
    );
  });

  it("keeps full history and pending Issue IDs in title attributes", async () => {
    const pendingIssueId = "pending-issue-with-a-long-identifier";
    vi.mocked(listPendingIssueOperations).mockResolvedValue([
      {
        operation_key: "operation-1",
        method: "manager_transition_issue",
        issue_id: pendingIssueId,
        created_at_ms: 0,
      },
    ]);
    mockHistoryDetail();

    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: /Issues|Issue 一覧/ }));
    const pendingLink = await screen.findByRole("button", { name: pendingIssueId });
    expect(pendingLink).toHaveAttribute("title", pendingIssueId);

    fireEvent.click(screen.getByRole("button", { name: "判断履歴" }));
    const historyLink = await screen.findByRole("button", {
      name: "Issue issue-1 の詳細を開く",
    });
    expect(historyLink).toHaveAttribute("title", "issue-1");
  });

  it("shows a notice when the history response is truncated", async () => {
    vi.mocked(listIssueHistory).mockResolvedValue({
      entries: [historyEntry],
      truncated: true,
    });

    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: "判断履歴" }));

    expect(
      await screen.findByText("履歴が多いため最新1,000件を表示しています"),
    ).toBeInTheDocument();
  });

  it("keeps review and inventory label selections independent", async () => {
    vi.mocked(listIssueReviewItems).mockResolvedValue([{ ...item, source_labels: ["review:one"] }]);
    vi.mocked(listIssues).mockResolvedValue([
      { ...historyIssueDetail, title: "Inventory issue", source_labels: ["issue:one"] },
    ]);

    renderWithProviders(<Issues />);
    const reviewSummary = await screen.findByText(/(ラベルで絞り込む|Filter by label) \(0\)/);
    fireEvent.click(reviewSummary);
    fireEvent.click(screen.getByRole("button", { name: "review:one" }));

    fireEvent.click(screen.getByRole("button", { name: /Issues|Issue 一覧/ }));
    const issueSummary = await screen.findByText(/(ラベルで絞り込む|Filter by label) \(0\)/);
    expect(issueSummary.closest("details")).not.toHaveAttribute("open");
    fireEvent.click(issueSummary);
    fireEvent.click(screen.getByRole("button", { name: "issue:one" }));

    fireEvent.click(screen.getByRole("button", { name: /Review|要確認/ }));
    const selectedReviewSummary = await screen.findByText(
      /(ラベルで絞り込む|Filter by label) \(1\)/,
    );
    expect(selectedReviewSummary.closest("details")).toHaveAttribute("open");
  });

  it("keeps row label toggles independent between review and inventory", async () => {
    vi.mocked(listIssueReviewItems).mockResolvedValue([
      { ...item, title: "レビュー共有", source_labels: ["shared"] },
      { ...item, id: "review-other", title: "レビュー別", source_labels: ["review-only"] },
    ]);
    vi.mocked(listIssues).mockResolvedValue([
      { ...historyIssueDetail, title: "一覧共有", source_labels: ["shared"] },
      {
        ...historyIssueDetail,
        id: "inventory-other",
        title: "一覧別",
        source_labels: ["issue-only"],
      },
    ]);

    renderWithProviders(<Issues />);
    const sharedLabel = /Filter by label.*shared|ラベル.*shared/;
    fireEvent.click(await screen.findByRole("button", { name: sharedLabel }));
    await waitFor(() => {
      expect(screen.getByText("レビュー共有")).toBeInTheDocument();
      expect(screen.queryByText("レビュー別")).not.toBeInTheDocument();
    });

    fireEvent.click(screen.getByRole("button", { name: /Issues|Issue 一覧/ }));
    expect(await screen.findByText("一覧共有")).toBeInTheDocument();
    expect(screen.getByText("一覧別")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: sharedLabel }));
    await waitFor(() => expect(screen.queryByText("一覧別")).not.toBeInTheDocument());

    fireEvent.click(screen.getByRole("button", { name: /Review|要確認/ }));
    expect(await screen.findByText("レビュー共有")).toBeInTheDocument();
    expect(screen.queryByText("レビュー別")).not.toBeInTheDocument();
  });

  it("opens normal and human acceptance options after source preflight", async () => {
    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: "レビュー" }));
    fireEvent.click(screen.getByRole("button", { name: "採用する" }));
    await waitFor(() => expect(screen.getByLabelText("確認して採用")).toBeInTheDocument());
    expect(screen.getByLabelText("人為確認で採用")).toBeInTheDocument();
  });

  it("requires human review when a source cannot be opened", async () => {
    vi.mocked(getIssueEvidence).mockResolvedValue([
      {
        thread_id: "1",
        memory_id: "2",
        excerpt: null,
        availability: "unavailable",
        unavailable_reason: "削除済み",
      },
    ]);
    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: "レビュー" }));
    fireEvent.click(screen.getByRole("button", { name: "採用する" }));
    expect(await screen.findByText(/原文参照を開けないため/)).toBeInTheDocument();
  });

  it("connects candidate edit and explicit Issue buttons to their own dialogs", async () => {
    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: "レビュー" }));
    fireEvent.click(screen.getByRole("button", { name: "候補を編集" }));
    expect(await screen.findByRole("dialog", { name: "候補を編集" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "キャンセル" }));
    fireEvent.click(screen.getByRole("button", { name: "明示 Issue を作成" }));
    expect(await screen.findByRole("dialog", { name: "新しい Issue に置換" })).toBeInTheDocument();
    expect(vi.mocked(updateIssueCandidate)).not.toHaveBeenCalled();
    expect(vi.mocked(createExplicitIssueFromCandidate)).not.toHaveBeenCalled();
  });

  it("applies a terminal re-extract event received before the command response", async () => {
    let resolveDispatch: ((value: { job_id_hint: string }) => void) | undefined;
    vi.mocked(reextractRejectedCandidate).mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveDispatch = resolve;
        }),
    );
    renderWithProviders(<Issues />);
    await waitFor(() => expect(summaryListener).not.toBeNull());
    fireEvent.click(await screen.findByRole("button", { name: "レビュー" }));
    fireEvent.click(screen.getByRole("button", { name: "却下する" }));
    fireEvent.click(screen.getByRole("button", { name: "確定" }));
    expect(await screen.findByRole("button", { name: "この候補を再抽出" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "この候補を再抽出" }));
    await waitFor(() => expect(reextractRejectedCandidate).toHaveBeenCalled());
    await act(async () => {
      summaryListener?.({
        payload: { job_id: "reextract-1", status: "failed", message: "snapshot mismatch" },
      });
      resolveDispatch?.({ job_id_hint: "reextract-1" });
    });
    expect(await screen.findByText("snapshot mismatch")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "この候補を再抽出" })).toBeEnabled();
  });

  it("keeps a later rejected candidate visible when another re-extract completes", async () => {
    const candidateB = { ...item, id: "candidate-2", title: "別の候補" };
    let resolveDispatch: ((value: { job_id_hint: string }) => void) | undefined;
    vi.mocked(reextractRejectedCandidate).mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveDispatch = resolve;
        }),
    );
    renderWithProviders(<Issues />);
    await waitFor(() => expect(summaryListener).not.toBeNull());
    fireEvent.click(await screen.findByRole("button", { name: "レビュー" }));
    vi.mocked(listIssueReviewItems).mockResolvedValue([candidateB]);
    fireEvent.click(screen.getByRole("button", { name: "却下する" }));
    fireEvent.click(screen.getByRole("button", { name: "確定" }));
    expect(await screen.findByRole("button", { name: "この候補を再抽出" })).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "この候補を再抽出" }));
    await waitFor(() =>
      expect(reextractRejectedCandidate).toHaveBeenCalledWith("candidate-1", expect.anything()),
    );
    await act(async () => resolveDispatch?.({ job_id_hint: "reextract-a" }));
    expect(await screen.findByRole("button", { name: "再抽出中…" })).toBeDisabled();

    fireEvent.click(await screen.findByRole("button", { name: "レビュー" }));
    fireEvent.click(screen.getByRole("button", { name: "却下する" }));
    fireEvent.click(screen.getByRole("button", { name: "確定" }));
    await act(async () => {
      summaryListener?.({ payload: { job_id: "reextract-a", status: "done" } });
    });

    expect(await screen.findByRole("button", { name: "この候補を再抽出" })).toBeEnabled();
  });

  it("passes the selected associated Issue ID and limits auto-registered actions", async () => {
    vi.mocked(resolveIssueReview).mockClear();
    const autoItem = {
      ...item,
      review_state: "unreviewed" as const,
      associated_issue_id: "issue-9",
      issue_id: "issue-9",
    };
    vi.mocked(listIssueReviewItems).mockResolvedValue([autoItem]);
    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: "レビュー" }));
    expect(screen.queryByRole("button", { name: "候補を編集" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "明示 Issue を作成" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "採用する" }));
    expect(await screen.findByLabelText("確認して採用")).toBeInTheDocument();
    expect(screen.queryByLabelText("既存項目へ統合")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "確定" }));
    await waitFor(() => expect(resolveIssueReview).toHaveBeenCalled());
    const request = (resolveIssueReview as unknown as { mock: { calls: Array<Array<unknown>> } })
      .mock.calls[0]?.[0];
    expect(request).toMatchObject({
      associated_issue_id: "issue-9",
    });
  });

  it("omits the associated Issue ID for proposed candidates", async () => {
    vi.mocked(resolveIssueReview).mockClear();
    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: "レビュー" }));
    fireEvent.click(screen.getByRole("button", { name: "却下する" }));
    fireEvent.click(screen.getByRole("button", { name: "確定" }));
    await waitFor(() => expect(resolveIssueReview).toHaveBeenCalled());
    const request = (resolveIssueReview as unknown as { mock: { calls: Array<Array<unknown>> } })
      .mock.calls[0]?.[0] as Record<string, unknown>;
    expect(request).not.toHaveProperty("associated_issue_id");
  });

  it("disables every candidate mutation for an integrity anomaly", async () => {
    vi.mocked(listIssueReviewItems).mockResolvedValue([
      { ...item, integrity: "mapping_invalid", resolution_state: "actionable" },
    ]);
    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: "レビュー" }));
    expect(screen.getByText(/整合性異常/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "候補を編集" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "明示 Issue を作成" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "採用する" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "却下する" })).toBeDisabled();
  });

  it("starts one review polling timer for pending rows and cleans it up", async () => {
    vi.mocked(listIssueReviewItems).mockResolvedValue([
      { ...item, resolution_state: "resolution_pending" },
    ]);
    const interval = vi.spyOn(window, "setInterval");
    const clearInterval = vi.spyOn(window, "clearInterval");
    const { unmount } = renderWithProviders(<Issues />);
    await waitFor(() => expect(interval).toHaveBeenCalledTimes(1));
    unmount();
    expect(clearInterval).toHaveBeenCalledWith(interval.mock.results[0]?.value);
    interval.mockRestore();
    clearInterval.mockRestore();
  });

  it("shares the unfiltered review projection between the list and summary", async () => {
    vi.mocked(listIssueReviewItems).mockClear();
    renderWithProviders(<Issues />);
    await waitFor(() => expect(listIssueReviewItems).toHaveBeenCalledTimes(1));
  });

  it("filters the review list locally from the single unfiltered fetch", async () => {
    vi.mocked(listIssueReviewItems).mockResolvedValue([
      item,
      { ...item, id: "candidate-2", title: "推定の候補", detection: "inferred" },
    ]);
    vi.mocked(listIssueReviewItems).mockClear();
    renderWithProviders(<Issues />);
    expect(await screen.findByText("推定の候補")).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText("検出由来"), { target: { value: "explicit" } });
    await waitFor(() => expect(screen.queryByText("推定の候補")).not.toBeInTheDocument());
    expect(screen.getByText("認証エラー")).toBeInTheDocument();
    expect(listIssueReviewItems).toHaveBeenCalledTimes(1);
  });

  it("does not load the operation history until the history view opens", async () => {
    vi.mocked(listIssueHistory).mockClear();
    renderWithProviders(<Issues />);
    await waitFor(() => expect(listIssueReviewItems).toHaveBeenCalled());
    expect(listIssueHistory).not.toHaveBeenCalled();
  });

  it("does not load the inventory projection while the review list is active", async () => {
    vi.mocked(listIssues).mockClear();
    renderWithProviders(<Issues />);
    await waitFor(() => expect(listIssueReviewItems).toHaveBeenCalled());
    expect(listIssues).not.toHaveBeenCalled();
  });

  it("loads recovery first and audit only after the history disclosure opens", async () => {
    mockHistoryDetail();
    let recoveryFirstPage = true;
    vi.mocked(listIssueDirectHistoryPage).mockImplementation(async (req) => {
      if (req.mode === "recovery" && req.page_token === null && recoveryFirstPage) {
        recoveryFirstPage = false;
        return {
          kind: "page",
          page: { entries: [], next_page_token: "recovery-next" },
        };
      }
      return {
        kind: "page",
        page: {
          entries: [
            {
              id: req.mode === "recovery" ? "recovery-op" : "audit-op",
              created_at_ms: 0,
              actor: "tester",
              decision: "PENDING",
              target_title: "operation",
              issue_ids: ["issue-1"],
              rationale: null,
              state: "PENDING",
            },
          ],
          next_page_token: null,
        },
      };
    });
    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: "判断履歴" }));
    fireEvent.click(await screen.findByRole("button", { name: "Issue issue-1 の詳細を開く" }));
    await waitFor(() =>
      expect(listIssueDirectHistoryPage).toHaveBeenCalledWith({
        issue_id: "issue-1",
        mode: "recovery",
        page_token: null,
      }),
    );
    expect(listIssueDirectHistoryPage).toHaveBeenCalledTimes(1);
    expect(await screen.findByText("要確認の操作はありません。")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "さらに読み込む" }));
    await waitFor(() =>
      expect(listIssueDirectHistoryPage).toHaveBeenCalledWith({
        issue_id: "issue-1",
        mode: "recovery",
        page_token: "recovery-next",
      }),
    );
    fireEvent.click(screen.getByText("全操作履歴"));
    await waitFor(() =>
      expect(listIssueDirectHistoryPage).toHaveBeenCalledWith({
        issue_id: "issue-1",
        mode: "audit",
        page_token: null,
      }),
    );
    expect(screen.getAllByRole("button", { name: /PENDING/ })).toHaveLength(2);
  });

  it("shows the reset notice even when the refreshed first page is empty", async () => {
    mockHistoryDetail();
    const recoveryEntry: IssueHistoryEntry = {
      id: "recovery-op",
      created_at_ms: 0,
      actor: "tester",
      decision: "PENDING",
      target_title: "operation",
      issue_ids: ["issue-1"],
      rationale: null,
      state: "PENDING",
    };
    let initialRecoveryPage = true;
    vi.mocked(listIssueDirectHistoryPage).mockImplementation(
      async (request): Promise<IssueHistoryPageOutcome> => {
        if (request.mode === "audit") {
          return { kind: "page", page: { entries: [], next_page_token: null } };
        }
        if (request.page_token === "opaque-token") return { kind: "reset_required" };
        if (request.page_token === null) {
          const firstPage = initialRecoveryPage;
          initialRecoveryPage = false;
          return {
            kind: "page",
            page: {
              entries: firstPage ? [recoveryEntry] : [],
              next_page_token: firstPage ? "opaque-token" : null,
            },
          };
        }
        return { kind: "page", page: { entries: [], next_page_token: null } };
      },
    );

    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: "判断履歴" }));
    fireEvent.click(await screen.findByRole("button", { name: "Issue issue-1 の詳細を開く" }));
    await waitFor(() =>
      expect(screen.getByRole("button", { name: /PENDING/ })).toBeInTheDocument(),
    );
    fireEvent.click(screen.getByRole("button", { name: "さらに読み込む" }));

    expect(
      await screen.findByText("履歴が更新されたため、先頭から読み込み直しました。"),
    ).toBeInTheDocument();
    expect(screen.getByText("要確認の操作はありません。")).toBeInTheDocument();
  });

  it("keeps loaded history entries visible when loading another page fails", async () => {
    mockHistoryDetail();
    const recoveryEntry: IssueHistoryEntry = {
      id: "recovery-op",
      created_at_ms: 0,
      actor: "tester",
      decision: "PENDING",
      target_title: "operation",
      issue_ids: ["issue-1"],
      rationale: null,
      state: "PENDING",
    };
    vi.mocked(listIssueDirectHistoryPage).mockImplementation(async (request) => {
      if (request.page_token === null) {
        return {
          kind: "page",
          page: { entries: [recoveryEntry], next_page_token: "next-page" },
        };
      }
      throw new Error("page failed");
    });

    renderWithProviders(<Issues />);
    fireEvent.click(await screen.findByRole("button", { name: "判断履歴" }));
    fireEvent.click(await screen.findByRole("button", { name: "Issue issue-1 の詳細を開く" }));
    expect(await screen.findByRole("button", { name: /PENDING/ })).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "さらに読み込む" }));

    expect(await screen.findByText("操作履歴を取得できません: page failed")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /PENDING/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "再読み込み" })).toBeInTheDocument();
  });
});
