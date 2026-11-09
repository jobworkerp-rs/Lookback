import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import type {
  ThreadGroupLineageView,
  ThreadGroupListItem,
  ThreadGroupSearchPage,
  ThreadGroupSummaryView,
} from "@/types/api";
import { Threads } from "./Threads";

const listThreadGroups = vi.fn();
const findThreadGroup = vi.fn();
const findThreadGroupSummary = vi.fn();
const findThreadGroupSummaries = vi.fn();
const searchThreadGroups = vi.fn();
const listThreads = vi.fn();
const findThread = vi.fn();
const findDistinctLabels = vi.fn();
const enqueueThreadGroupSummaryJob = vi.fn();
const enqueueThreadGroupIssueResolutionJob = vi.fn();
const getThreadGroupRecoveryStatus = vi.fn();

vi.mock("@/api", () => ({
  getAppCapabilities: vi.fn().mockResolvedValue({ experimental_issues_enabled: false }),
  listThreadGroups: (req: unknown) => listThreadGroups(req),
  findThreadGroup: (...args: unknown[]) => findThreadGroup(...args),
  findThreadGroupSummary: (...args: unknown[]) => findThreadGroupSummary(...args),
  findThreadGroupSummaries: (...args: unknown[]) => findThreadGroupSummaries(...args),
  searchThreadGroups: (req: unknown) => searchThreadGroups(req),
  listThreads: (req: unknown) => listThreads(req),
  getThreadGroupRecoveryStatus: () => getThreadGroupRecoveryStatus(),
  parseSummaryContent: (entry: { content_json: string }) => ({
    parsed: JSON.parse(entry.content_json),
    raw: entry.content_json,
  }),
  enqueueThreadGroupSummaryJob: (req: unknown) => enqueueThreadGroupSummaryJob(req),
  enqueueThreadGroupIssueResolutionJob: (req: unknown) => enqueueThreadGroupIssueResolutionJob(req),
  findDistinctLabels: (req: unknown) => findDistinctLabels(req),
  findCoOccurringLabels: vi.fn().mockResolvedValue([]),
  deleteThread: vi.fn(),
  findThread: (id: unknown) => findThread(id),
  resolveMemoryReference: vi.fn(),
  searchMemoriesKeyword: vi.fn().mockResolvedValue([]),
  searchMemoriesSemantic: vi.fn().mockResolvedValue([]),
  searchMemoriesHybrid: vi.fn().mockResolvedValue([]),
}));

const sidecar = { phase: "ready" as const, warnings: [] };

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

const group: ThreadGroupListItem = {
  id: "7",
  user_id: 7,
  group_canonical_key: "group-7",
  title: "Project work",
  display_name: "Project work",
  status: "active",
  grouping_authority: "reconciler",
  redirect_to_group_id: null,
  latest_activity_at_ms: 1700000000000,
  root_thread_id: "1",
  root_thread_canonical_key: "thread-1",
  root_display: null,
  active_member_count: 1,
  deleted_member_count: 1,
  unresolved_count: 0,
  membership_snapshot_digest: "digest",
};

const lineage: ThreadGroupLineageView = {
  group,
  members: [
    {
      group_id: "7",
      user_id: 1,
      thread_id: "1",
      thread_canonical_key: "thread-1",
      source: "codex",
      role: "root",
      state: "active",
      provenance: "reconciler",
      identity_scope: null,
      native_id: null,
      deleted_at_ms: null,
      display: {
        thread_id: "1",
        thread_canonical_key: "thread-1",
        description: "【Thread title】 **Markdown body** [Reference](https://example.com)",
        source: "codex",
        created_at_ms: null,
        last_message_at_ms: null,
        deleted_at_ms: null,
      },
    },
    {
      group_id: "7",
      user_id: 2,
      thread_id: null,
      thread_canonical_key: "thread-deleted",
      source: "codex",
      role: "member",
      state: "deleted",
      provenance: "reconciler",
      identity_scope: null,
      native_id: null,
      deleted_at_ms: 1700000000000,
      display: {
        thread_id: null,
        thread_canonical_key: "thread-deleted",
        description: "【Deleted title】",
        source: "codex",
        created_at_ms: null,
        last_message_at_ms: null,
        deleted_at_ms: 1700000000000,
      },
    },
  ],
  relations: [
    {
      id: "14",
      parent_thread_id: "1",
      child_thread_id: "2",
      parent_thread_canonical_key: "thread-1",
      child_thread_canonical_key: "thread-1",
      parent_user_id: 1,
      child_user_id: 2,
      relation_type: "continuation",
      state: "active",
      selection_basis: "source_exact",
      source_confidence: "exact",
      selected_observation_id: null,
      selected_operator_decision_id: null,
      parent_group_id: "7",
      child_group_id: "8",
      parent_display: {
        thread_id: "1",
        thread_canonical_key: "thread-1",
        description: "Parent",
        source: "codex",
        created_at_ms: null,
        last_message_at_ms: null,
        deleted_at_ms: null,
      },
      child_display: {
        thread_id: "2",
        thread_canonical_key: "thread-2",
        description: "Child",
        source: "codex",
        created_at_ms: null,
        last_message_at_ms: null,
        deleted_at_ms: null,
      },
    },
  ],
  unresolved: [
    {
      id: "12",
      subject_thread_id: "2",
      subject_source: "codex",
      subject_user_id: 2,
      candidate_group_id: "7",
      candidate_parent_thread_id: null,
      state: "ambiguous",
      subject_identity_scope_known: true,
      subject_identity_scope_value: "workspace",
      subject_native_id: "native-1",
      selected_observation_id: null,
    },
  ],
  observations: [
    {
      id: "13",
      subject_source: "codex",
      subject_user_id: 2,
      subject_identity_scope_known: true,
      subject_identity_scope_value: "workspace",
      subject_native_id: "native-1",
      candidate_parent_present: true,
      candidate_parent_user_id: 1,
      candidate_parent_source: "codex",
      candidate_parent_identity_scope_known: true,
      candidate_parent_identity_scope_value: "workspace",
      candidate_parent_native_id: "native-0",
      relation_kind: "continuation",
      evidence_kind: "source_event",
      polarity: "supports",
      source_confidence: "strong",
      state: "candidate",
      adapter_version: "v1",
      source_record_ref: "record-1",
      observed_at_ms: 1700000000000,
    },
  ],
};

const summary: ThreadGroupSummaryView = {
  memory_id: "100",
  external_id: "thread-group-summary:7",
  content_json: JSON.stringify({ title: "Group summary", summary: "Done" }),
  metadata: null,
  updated_at_ms: 1700000000000,
};

describe("Threads group view", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    listThreadGroups.mockReset().mockResolvedValue([group]);
    findThreadGroup.mockReset().mockResolvedValue(lineage);
    findThreadGroupSummary.mockReset().mockResolvedValue(summary);
    findThreadGroupSummaries
      .mockReset()
      .mockImplementation(async (groupIds: string[]) =>
        groupIds.map((group_id) => ({ group_id, summary })),
      );
    enqueueThreadGroupSummaryJob.mockReset().mockResolvedValue({ job_id_hint: "group-job" });
    enqueueThreadGroupIssueResolutionJob
      .mockReset()
      .mockResolvedValue({ job_id_hint: "resolution-job" });
    getThreadGroupRecoveryStatus.mockReset().mockResolvedValue({
      health: "healthy",
      record_count: 0,
      detail: null,
      records: [],
    });
    searchThreadGroups.mockReset().mockResolvedValue({
      results: [
        {
          group,
          relevance_score: 0,
          witnesses: [],
          root_member: group.root_display,
          current_member_count: 1,
          active_member_count: 1,
          deleted_member_count: 1,
        },
      ],
      next_page_token: null,
    });
    findDistinctLabels.mockReset().mockResolvedValue([]);
    listThreads.mockReset().mockResolvedValue([]);
    findThread.mockReset().mockResolvedValue(null);
  });

  it("shows active group rows and keeps deleted lineage placeholders non-clickable", async () => {
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    expect(await screen.findByText("Project work")).toBeInTheDocument();
    expect(screen.getByText("1 件の削除済み")).toBeInTheDocument();
    expect(listThreads).not.toHaveBeenCalled();
    expect(listThreadGroups).not.toHaveBeenCalled();
    expect(searchThreadGroups).toHaveBeenCalledWith(
      expect.objectContaining({ query_text: "", mode: "keyword", page_token: undefined }),
    );
    expect(searchThreadGroups.mock.calls[0]?.[0]).not.toHaveProperty("group_owner_user_id");
    fireEvent.click(screen.getByRole("button", { name: /Project work/ }));

    await waitFor(() => expect(findThreadGroup).toHaveBeenCalledWith("7", 7));
    expect(await screen.findByText("削除済みプレースホルダー")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /スレッド 1 を開く/ })).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /削除済みプレースホルダー/ }),
    ).not.toBeInTheDocument();
    expect(screen.getByText("関連グループ")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "関連グループ 8 を開く" })).toBeInTheDocument();
    expect(screen.queryByText("推定")).not.toBeInTheDocument();
    expect(screen.queryByText("推定された関連")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "対象スレッドを開く" })).not.toBeInTheDocument();
    expect(screen.queryByText(/Parent → Child/)).not.toBeInTheDocument();
    expect(screen.queryByText(/候補 #/)).not.toBeInTheDocument();
    expect(screen.queryByText(/観測 #/)).not.toBeInTheDocument();
    expect(screen.getByText("スレッドツリー")).toBeInTheDocument();
    expect(screen.getByText("技術情報")).toBeInTheDocument();
    expect(screen.queryByText("グループ要約")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "要約を再生成" })).not.toBeInTheDocument();
    expect(screen.getByText("Thread title")).toHaveClass("thread-tree-title");
    expect(screen.getByText("Markdown body").tagName).toBe("STRONG");
    expect(screen.getByRole("link", { name: "Reference" })).toHaveAttribute(
      "href",
      "https://example.com",
    );
    expect(screen.getByText("Deleted title")).toHaveClass("thread-tree-title");
    expect(screen.getByText("Deleted title").closest(".thread-hit-card")).not.toHaveTextContent(
      "Markdown body",
    );

    fireEvent.click(screen.getByRole("button", { name: "関連グループ 8 を開く" }));
    await waitFor(() => expect(findThreadGroup).toHaveBeenCalledWith("8", 7));
    fireEvent.click(await screen.findByRole("button", { name: /スレッド 1 を開く/ }));
    expect(await screen.findByRole("dialog", { name: "Thread title" })).toBeInTheDocument();
  });

  it("labels the root action and each member summary without calling it a group summary", async () => {
    findThreadGroup.mockResolvedValue({
      ...lineage,
      group: { ...group, active_member_count: 2 },
      members: [
        lineage.members[0],
        {
          ...lineage.members[0],
          user_id: 2,
          thread_id: "2",
          thread_canonical_key: "thread-2",
          role: "member",
          display: {
            ...lineage.members[0]?.display,
            thread_id: "2",
            thread_canonical_key: "thread-2",
            description: "【Child title】 Child summary",
          },
        },
      ],
      relations: [
        {
          ...lineage.relations[0],
          child_thread_canonical_key: "thread-2",
          child_group_id: "7",
          parent_user_id: 1,
          child_user_id: 2,
          relation_type: "delegated",
        },
      ],
      unresolved: [],
    });
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);
    fireEvent.click(await screen.findByRole("button", { name: /Project work/ }));

    expect(
      await screen.findByRole("button", { name: /起点のスレッド 1 を開く/ }),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /委任されたスレッド 2 を開く/ })).toBeInTheDocument();
    expect(screen.getAllByText("スレッド要約")).toHaveLength(2);
    expect(screen.queryByText("起点のスレッド")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /委任されたスレッド 2 を開く/ }));
    expect(await screen.findByRole("dialog", { name: "Child title" })).toBeInTheDocument();
  });

  it("shows thread counts without internal group status, unresolved, or relevance labels", async () => {
    const groupWithoutOptionalCounts = {
      ...group,
      deleted_member_count: 0,
      unresolved_count: 0,
    };
    searchThreadGroups.mockResolvedValue({
      results: [
        {
          group: groupWithoutOptionalCounts,
          relevance_score: 0,
          witnesses: [
            {
              predicate_index: 0,
              target: "memory",
              thread_id: "1",
              thread_canonical_key: "thread-1",
              score: 0,
              member: groupWithoutOptionalCounts.root_display,
            },
          ],
          root_member: groupWithoutOptionalCounts.root_display,
          current_member_count: 1,
          active_member_count: 1,
          deleted_member_count: 0,
        },
      ],
      next_page_token: null,
    });

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    expect(await screen.findByText("スレッド 1件")).toBeInTheDocument();
    expect(screen.queryByText(/削除済み/)).not.toBeInTheDocument();
    expect(screen.queryByText("active")).not.toBeInTheDocument();
    expect(screen.queryByText("未解決 0 件")).not.toBeInTheDocument();
    expect(screen.queryByText(/関連度/)).not.toBeInTheDocument();
    expect(screen.queryByText(/一致したスレッド/)).not.toBeInTheDocument();
  });

  it("hides unresolved counts even when the backend provides them", async () => {
    const groupWithUnresolved = { ...group, unresolved_count: 2 };
    searchThreadGroups.mockResolvedValue({
      results: [
        {
          group: groupWithUnresolved,
          relevance_score: 0,
          witnesses: [],
          root_member: groupWithUnresolved.root_display,
          current_member_count: 2,
          active_member_count: 1,
          deleted_member_count: 1,
        },
      ],
      next_page_token: null,
    });

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    await screen.findByText("スレッド 1件");
    expect(screen.queryByText("未解決 2 件")).not.toBeInTheDocument();
  });

  it("hides the redundant summary for a single member while keeping review details actionable", async () => {
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    fireEvent.click(await screen.findByRole("button", { name: /Project work/ }));
    await screen.findByText("スレッドツリー");
    expect(
      screen.getAllByRole("heading", { level: 3 }).map((heading) => heading.textContent),
    ).toEqual(["スレッドツリー", "関連グループ"]);
    const technicalInfo = screen.getByText("技術情報").closest("details");
    expect(technicalInfo).not.toHaveAttribute("open");

    fireEvent.click(screen.getByText("技術情報"));
    expect(technicalInfo).toHaveAttribute("open");
  });

  it("shows roots and confirmed group-internal relations in the thread tree", async () => {
    findThreadGroup.mockResolvedValue({
      ...lineage,
      group: { ...lineage.group, active_member_count: 2 },
      members: [
        lineage.members[0],
        {
          ...lineage.members[0],
          user_id: 2,
          thread_id: "2",
          thread_canonical_key: "thread-2",
          role: "member",
          display: {
            thread_id: "2",
            thread_canonical_key: "thread-2",
            description: "【Child thread】 Summary",
            source: "codex",
            created_at_ms: null,
            last_message_at_ms: null,
            deleted_at_ms: null,
          },
        },
      ],
      relations: [
        {
          ...lineage.relations[0],
          parent_thread_id: "1",
          child_thread_id: "2",
          parent_user_id: 1,
          child_user_id: 2,
          parent_group_id: "7",
          child_group_id: "7",
        },
      ],
      unresolved: [
        {
          ...lineage.unresolved[0],
          subject_thread_id: "2",
          candidate_parent_thread_id: "1",
          state: "candidate",
        },
      ],
    });
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    fireEvent.click(await screen.findByRole("button", { name: /Project work/ }));

    expect(
      await screen.findByRole("button", { name: /起点のスレッド 1 を開く/ }),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /継続したスレッド 2 を開く/ })).toBeInTheDocument();
    expect(screen.queryByText("推定")).not.toBeInTheDocument();
  });

  it("shows and orders an inferred relation only for distinct live group members", async () => {
    findThreadGroup.mockResolvedValue({
      ...lineage,
      group: { ...lineage.group, active_member_count: 2 },
      members: [
        {
          ...lineage.members[0],
          thread_id: "2",
          thread_canonical_key: "thread-2",
          role: "member",
          display: {
            thread_id: "2",
            thread_canonical_key: "thread-2",
            description: "【Inferred child】 Summary",
            source: "codex",
            created_at_ms: null,
            last_message_at_ms: null,
            deleted_at_ms: null,
          },
        },
        lineage.members[0],
      ],
      relations: [],
      unresolved: [
        {
          ...lineage.unresolved[0],
          subject_thread_id: "2",
          candidate_parent_thread_id: "1",
          state: "candidate",
        },
      ],
    });
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    fireEvent.click(await screen.findByRole("button", { name: /Project work/ }));

    expect(await screen.findByText("推定")).toHaveClass("thread-tree-inferred");
    expect(
      screen.getByRole("button", { name: /推定された関連のスレッド 2 を開く/ }),
    ).toBeInTheDocument();
    const titles = screen
      .getAllByText(/Thread title|Inferred child/)
      .map((element) => element.textContent);
    expect(titles).toEqual(["Thread title", "Inferred child"]);
  });

  it("renders an explicit unsupported state from the backend", async () => {
    searchThreadGroups.mockRejectedValue(
      new Error("この memories バージョンはスレッドグループ表示に未対応です"),
    );
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "現在の memories バージョンはスレッドグループに対応していません。",
    );
  });

  it("retries a group with its current membership digest", async () => {
    findThreadGroup.mockResolvedValue({
      ...lineage,
      group: { ...lineage.group, active_member_count: 2 },
    });
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    fireEvent.click(await screen.findByRole("button", { name: /Project work/ }));
    fireEvent.click(await screen.findByRole("button", { name: "要約を再生成" }));

    await waitFor(() =>
      expect(enqueueThreadGroupSummaryJob).toHaveBeenCalledWith(
        expect.objectContaining({
          target_group_id: "7",
          group_owner_user_id: 7,
          expected_membership_snapshot_digest: "digest",
          force_resummarize: true,
        }),
      ),
    );
  });

  it("force-reconciles Issue candidates for the group without regenerating its summary", async () => {
    findThreadGroup.mockResolvedValue({
      ...lineage,
      group: { ...lineage.group, active_member_count: 2 },
    });
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    fireEvent.click(await screen.findByRole("button", { name: /Project work/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Issue 解決照合を再実行" }));

    await waitFor(() =>
      expect(enqueueThreadGroupIssueResolutionJob).toHaveBeenCalledWith({
        target_group_id: "7",
        group_owner_user_id: 7,
        expected_membership_snapshot_digest: "digest",
        dispatch_id: expect.any(String),
      }),
    );
    expect(enqueueThreadGroupSummaryJob).not.toHaveBeenCalled();
  });

  it("shows a direct Issue resolution dispatch failure without hiding the group", async () => {
    findThreadGroup.mockResolvedValue({
      ...lineage,
      group: { ...lineage.group, active_member_count: 2 },
    });
    enqueueThreadGroupIssueResolutionJob.mockRejectedValueOnce(new Error("tracker unavailable"));
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    fireEvent.click(await screen.findByRole("button", { name: /Project work/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Issue 解決照合を再実行" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("tracker unavailable");
    expect(screen.getByText("スレッドツリー")).toBeInTheDocument();
  });

  it("shows a durable failure before the stale digest state", async () => {
    findThreadGroup.mockResolvedValue({
      ...lineage,
      group: { ...lineage.group, active_member_count: 2 },
    });
    getThreadGroupRecoveryStatus.mockResolvedValue({
      health: "healthy",
      record_count: 1,
      detail: null,
      records: [{ group_id: "7" }],
    });

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    fireEvent.click(await screen.findByRole("button", { name: /Project work/ }));
    expect(await screen.findByText("失敗（再試行可能）")).toBeInTheDocument();
    expect(
      screen.getByText("現在のスナップショットを指定して再試行できます。"),
    ).toBeInTheDocument();
  });

  it("uses group search controls, shows matching thread counts, pages with the cursor, and resets it", async () => {
    const page: ThreadGroupSearchPage = {
      results: [
        {
          group,
          relevance_score: 0.91,
          witnesses: [
            {
              predicate_index: 0,
              target: "memory",
              thread_id: "1",
              thread_canonical_key: "thread-1",
              score: 0.91,
              member: group.root_display,
            },
            {
              predicate_index: 1,
              target: "memory",
              thread_id: "1",
              thread_canonical_key: "thread-1",
              score: 0.001,
              member: group.root_display,
            },
          ],
          root_member: group.root_display,
          current_member_count: 2,
          active_member_count: 1,
          deleted_member_count: 1,
        },
      ],
      next_page_token: "opaque-next",
    };
    searchThreadGroups.mockResolvedValue(page);
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    fireEvent.click(screen.getByRole("button", { name: "Keyword" }));
    fireEvent.change(screen.getByPlaceholderText("検索クエリ"), {
      target: { value: "rust" },
    });

    await waitFor(() =>
      expect(searchThreadGroups).toHaveBeenLastCalledWith(
        expect.objectContaining({ query_text: "rust", page_token: undefined }),
      ),
    );
    expect(searchThreadGroups.mock.calls.at(-1)?.[0]).toMatchObject({
      query_text: "rust",
      mode: "keyword",
      allow_cross_member: false,
      page_token: undefined,
    });
    expect(await screen.findByText("一致したスレッド 1件")).toBeInTheDocument();
    expect(screen.queryByText(/関連度/)).not.toBeInTheDocument();
    expect(screen.queryByText(/根拠 \(memory\)/)).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "次のグループ" }));
    await waitFor(() =>
      expect(searchThreadGroups).toHaveBeenLastCalledWith(
        expect.objectContaining({ page_token: "opaque-next" }),
      ),
    );

    fireEvent.click(screen.getByLabelText("別メンバーの根拠も組み合わせる"));
    await waitFor(() =>
      expect(searchThreadGroups).toHaveBeenLastCalledWith(
        expect.objectContaining({ allow_cross_member: true, page_token: undefined }),
      ),
    );
  });

  it("sends date and label-only filters while browsing groups", async () => {
    findDistinctLabels
      .mockReset()
      .mockResolvedValue([{ label: "project:lookback", thread_count: 1 }]);
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    fireEvent.change(screen.getByTitle("作成日 下限 (strict >)"), {
      target: { value: "2026-01-02" },
    });
    fireEvent.click(await screen.findByText("project:lookback"));

    await waitFor(() =>
      expect(searchThreadGroups).toHaveBeenLastCalledWith(
        expect.objectContaining({
          query_text: "",
          page_token: undefined,
          labels_any: ["project:lookback"],
          created_after_ms: expect.any(Number),
        }),
      ),
    );
  });

  it("resets and reloads the first page when the backend reports snapshot_changed", async () => {
    const page: ThreadGroupSearchPage = {
      results: [
        {
          group,
          relevance_score: 0,
          witnesses: [],
          root_member: group.root_display,
          current_member_count: 2,
          active_member_count: 1,
          deleted_member_count: 1,
        },
      ],
      next_page_token: "opaque-next",
    };
    searchThreadGroups.mockImplementation((request: { page_token?: string }) =>
      request.page_token ? Promise.reject(new Error("snapshot_changed: restart")) : page,
    );
    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    fireEvent.click(screen.getByRole("button", { name: "Keyword" }));
    fireEvent.change(screen.getByPlaceholderText("検索クエリ"), {
      target: { value: "rust" },
    });
    await waitFor(() =>
      expect(searchThreadGroups).toHaveBeenLastCalledWith(
        expect.objectContaining({ query_text: "rust", page_token: undefined }),
      ),
    );
    fireEvent.click(await screen.findByRole("button", { name: "次のグループ" }));

    await waitFor(() =>
      expect(searchThreadGroups).toHaveBeenLastCalledWith(
        expect.objectContaining({ page_token: undefined }),
      ),
    );
  });

  it("loads every group card's summary state in one batch and seeds the detail cache", async () => {
    const other: ThreadGroupListItem = { ...group, id: "9", title: "Other work" };
    searchThreadGroups.mockResolvedValue({
      results: [group, other].map((item) => ({
        group: item,
        relevance_score: 0,
        witnesses: [],
        root_member: item.root_display,
        current_member_count: 1,
        active_member_count: 1,
        deleted_member_count: 1,
      })),
      next_page_token: null,
    });
    findThreadGroupSummaries.mockResolvedValue([
      { group_id: "7", summary },
      { group_id: "9", summary: null },
    ]);
    // A never-settling single read proves the detail pane uses the seeded entry.
    findThreadGroupSummary.mockReturnValue(new Promise(() => {}));
    findThreadGroup.mockResolvedValue({
      ...lineage,
      group: { ...lineage.group, active_member_count: 2 },
    });

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    expect(await screen.findByText("Other work")).toBeInTheDocument();
    await waitFor(() => expect(findThreadGroupSummaries).toHaveBeenCalledTimes(1));
    expect(findThreadGroupSummaries).toHaveBeenCalledWith(["7", "9"], 7);
    const projectCard = screen.getByRole("button", { name: /Project work/ });
    const otherCard = screen.getByRole("button", { name: /Other work/ });
    await waitFor(() => expect(projectCard).toHaveTextContent("要約が古い"));
    expect(otherCard).not.toHaveTextContent("要約");
    expect(findThreadGroupSummary).not.toHaveBeenCalled();

    fireEvent.click(projectCard);
    expect(await screen.findByText("グループ要約")).toBeInTheDocument();
    expect(await screen.findByText("Done")).toBeInTheDocument();
  });

  it("marks every card's summary state as unavailable when the batch read fails", async () => {
    findThreadGroupSummaries.mockRejectedValue(new Error("memories down"));

    renderWithProviders(<Threads onOpenImport={() => {}} sidecar={sidecar} />);

    const card = await screen.findByRole("button", { name: /Project work/ });
    await waitFor(() => expect(card).toHaveTextContent("要約状態を取得できません"));
  });
});
