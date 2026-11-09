import { beforeEach, describe, expect, it, vi } from "vitest";
import { LOCAL_CONNECTION_CONFIG } from "@/lib/connectionConfig";
import { remoteConnectionConfig } from "@/test-utils";
import type {
  ApplySettingsRequest,
  CodingAgentRepositoryHost,
  CodingAgentRepositoryIdentity,
  CodingAgentReviewActionRequest,
  CodingAgentRunPreviewRequest,
  CodingAgentSettings,
  SetMcpSettingsRequest,
  SummaryEntry,
} from "@/types/api";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

import {
  applySettings,
  applySetup,
  cancelPeriodicExecution,
  chatRespondToolApproval,
  codingAgentApplyReviewAction,
  codingAgentApproveAndStart,
  codingAgentApproveNativeBackend,
  codingAgentAttachRun,
  codingAgentCancelRun,
  codingAgentCheckRepositoryHost,
  codingAgentGetBackendStatus,
  codingAgentGetNativeSetup,
  codingAgentGetReview,
  codingAgentGetRun,
  codingAgentGetSettings,
  codingAgentListDelegationPlans,
  codingAgentListEvidence,
  codingAgentListRuns,
  codingAgentPrepareProfileHome,
  codingAgentPreviewRun,
  codingAgentReadEvidence,
  codingAgentReassess,
  codingAgentUpdateSettings,
  countActiveThreadGroups,
  countThreadGroupSummaries,
  createPeriodicTask,
  deletePeriodicTask,
  enqueuePersonalityJob,
  enqueueSummaryJob,
  enqueueThreadGroupIssueResolutionJob,
  enqueueThreadGroupSummaryJob,
  enqueueThreadIssueResolutionJob,
  findCoOccurringLabels,
  findDistinctLabels,
  findMemoryPosition,
  findMemoryThreadPosition,
  findThreadGroup,
  findThreadGroupSummaries,
  findThreadGroupSummary,
  generateMcpAuthToken,
  generateSummaries,
  getChatToolSettings,
  getConnectionConfig,
  getIssueTrackerSettings,
  getMcpSettings,
  getModelStatus,
  getReflectionIntentIndexStats,
  getReflectionSelectionContent,
  getSetupStatus,
  getSummaryContent,
  getThreadGroupRecoveryStatus,
  getThreadIssueResolutionStatus,
  inspectRemoteJobworkerp,
  listIssueDirectHistoryPage,
  listPeriodicExecutionHistory,
  listPeriodicTaskStatuses,
  listPeriodicTasks,
  listPersonalitySignals,
  listReflectionSelection,
  listSummariesForSelection,
  listThreadGroups,
  parsePersonalitySignalContent,
  parseSummaryContent,
  readSidecarLog,
  redispatchReflectionEmbeddings,
  registerRemoteJobworkerpWorkers,
  resolveMemoryReference,
  retryModelSetup,
  rotateMcpAuthToken,
  searchMemoriesHybrid,
  searchReflectionsByIntent,
  searchReflectionsHybrid,
  searchThreadGroups,
  setChatToolSettings,
  setConnectionConfig,
  setEnabledPeriodicTask,
  setMcpSettings,
  startImport,
  testConnectionConfig,
  updatePeriodicTask,
} from "./index";

function entry(content_json: string): SummaryEntry {
  return {
    memory_id: "1",
    thread_id: "1",
    external_id: "summary:1",
    kind: "per-thread",
    period_key: null,
    scope_key: null,
    content_json,
    updated_at_ms: 0,
  };
}

describe("parseSummaryContent", () => {
  it("preserves arbitrary object keys (English schema)", () => {
    const { parsed, raw } = parseSummaryContent(
      entry(JSON.stringify({ title: "T", summary: "S", status: "resolved" })),
    );
    expect(parsed).toEqual({ title: "T", summary: "S", status: "resolved" });
    expect(raw).toContain('"title":"T"');
  });

  it("preserves Japanese keys and array values the LLM emits", () => {
    const content = {
      目的: "Phase C の実装",
      実施内容: ["A の実装", "B の修正"],
    };
    const { parsed } = parseSummaryContent(entry(JSON.stringify(content)));
    expect(parsed).toEqual(content);
    expect(parsed?.実施内容).toEqual(["A の実装", "B の修正"]);
  });

  it("preserves key order", () => {
    const { parsed } = parseSummaryContent(
      entry(JSON.stringify({ 目的: "x", 成果物: "y", 課題: "z" })),
    );
    expect(parsed && Object.keys(parsed)).toEqual(["目的", "成果物", "課題"]);
  });

  it("leaves parsed null for plain text and keeps the raw body", () => {
    const { parsed, raw } = parseSummaryContent(entry("just a plain string"));
    expect(parsed).toBeNull();
    expect(raw).toBe("just a plain string");
  });

  it("leaves parsed null for non-object JSON values", () => {
    expect(parseSummaryContent(entry("42")).parsed).toBeNull();
    expect(parseSummaryContent(entry("[1,2,3]")).parsed).toBeNull();
  });
});

describe("parsePersonalitySignalContent", () => {
  it("parses the layer-1 signal schema", () => {
    const content = {
      interests: [{ topic: "rust", confidence: "high", evidence: "..." }],
      decision_style: { summary: "decisive", traits: ["fast"] },
    };
    const parsed = parsePersonalitySignalContent({ content_json: JSON.stringify(content) });
    expect(parsed.interests?.[0]?.topic).toBe("rust");
    expect(parsed.decision_style?.traits).toEqual(["fast"]);
  });

  it("returns an empty object for malformed JSON", () => {
    expect(parsePersonalitySignalContent({ content_json: "not json" })).toEqual({});
  });
});

describe("command wrappers", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue([]);
  });

  it("searchMemoriesHybrid invokes search_memories_hybrid with the request", async () => {
    const req = { query_text: "rust", mode: "hybrid" as const, user_id: 1 };
    await searchMemoriesHybrid(req);
    expect(invokeMock).toHaveBeenCalledWith("search_memories_hybrid", { req });
  });

  it("getSummaryContent invokes get_summary_content with the memory id", async () => {
    await getSummaryContent("99");
    expect(invokeMock).toHaveBeenCalledWith("get_summary_content", { req: { memory_id: "99" } });
  });

  it("findThreadGroupSummaries sends string ids and the owner in one request", async () => {
    await findThreadGroupSummaries(["9007199254740993", "7"], 7);
    expect(invokeMock).toHaveBeenCalledWith("find_thread_group_summaries", {
      req: { group_ids: ["9007199254740993", "7"], group_owner_user_id: 7 },
    });
  });

  it("leaves omitted ThreadGroup owners to the Rust boundary and preserves explicit ids", async () => {
    await listThreadGroups({ limit: 20, offset: 0 });
    await findThreadGroup("9007199254740993", 7);
    await findThreadGroupSummary("9007199254740993", 7);
    await countActiveThreadGroups(7);
    await countThreadGroupSummaries(7);

    expect(invokeMock).toHaveBeenNthCalledWith(1, "list_thread_groups", {
      req: { limit: 20, offset: 0 },
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "find_thread_group", {
      req: { group_id: "9007199254740993", group_owner_user_id: 7 },
    });
    expect(invokeMock).toHaveBeenNthCalledWith(3, "find_thread_group_summary", {
      req: { group_id: "9007199254740993", group_owner_user_id: 7 },
    });
    expect(invokeMock).toHaveBeenNthCalledWith(4, "count_active_thread_groups", {
      groupOwnerUserId: 7,
    });
    expect(invokeMock).toHaveBeenNthCalledWith(5, "count_thread_group_summaries", {
      groupOwnerUserId: 7,
    });
  });

  it("reads durable ThreadGroup summary recovery state", async () => {
    await getThreadGroupRecoveryStatus();
    expect(invokeMock).toHaveBeenCalledWith("get_thread_group_recovery_status");
  });

  it("reads and approves native setup using the backend CAS request envelope", async () => {
    await codingAgentGetNativeSetup();
    expect(invokeMock).toHaveBeenNthCalledWith(1, "coding_agent_get_native_setup");

    await codingAgentApproveNativeBackend({
      expectedRevision: 8,
      expectedPackageFingerprint: "package-fingerprint",
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "coding_agent_approve_native_backend", {
      request: {
        expectedRevision: 8,
        expectedPackageFingerprint: "package-fingerprint",
      },
    });
  });

  it("forwards group search filters and its opaque cursor", async () => {
    const req = {
      query_text: "rust",
      mode: "hybrid" as const,
      group_owner_user_id: 1,
      member_user_id: 2,
      created_after_ms: 10,
      created_before_ms: 20,
      labels_any: ["project:lookback"],
      label_match: "all" as const,
      allow_cross_member: true,
      page_size: 10,
      page_token: "opaque",
    };
    invokeMock.mockResolvedValue({ results: [], next_page_token: null });

    await searchThreadGroups(req);

    expect(invokeMock).toHaveBeenCalledWith("search_thread_groups", { req });
  });

  it("dispatches group summaries through the dedicated worker command", async () => {
    await enqueueThreadGroupSummaryJob({
      group_owner_user_id: 7,
      force_resummarize: true,
      dispatch_id: "group-job",
      target_group_id: "9007199254740993",
      expected_membership_snapshot_digest: "digest-a",
    });

    expect(invokeMock).toHaveBeenCalledWith("enqueue_thread_group_summary_job", {
      req: {
        group_owner_user_id: 7,
        force_resummarize: true,
        dispatch_id: "group-job",
        target_group_id: "9007199254740993",
        expected_membership_snapshot_digest: "digest-a",
      },
    });
  });

  it("dispatches targeted ThreadGroup issue resolution without changing the summary request", async () => {
    const req = {
      target_group_id: "9007199254740993",
      group_owner_user_id: 7,
      expected_membership_snapshot_digest: "digest-a",
      dispatch_id: "resolution-job",
    };
    await enqueueThreadGroupIssueResolutionJob(req);

    expect(invokeMock).toHaveBeenCalledWith("enqueue_thread_group_issue_resolution_job", {
      req,
    });
  });

  it("dispatches manual issue resolution for one thread", async () => {
    invokeMock.mockResolvedValue({ job_id_hint: "thread-resolution-job" });
    const req = { target_thread_id: "9007199254740993", dispatch_id: "manual-resolution" };

    await expect(enqueueThreadIssueResolutionJob(req)).resolves.toEqual({
      job_id_hint: "thread-resolution-job",
    });

    expect(invokeMock).toHaveBeenCalledWith("enqueue_thread_issue_resolution_job", { req });
  });

  it("reads the persisted manual issue-resolution status for one thread", async () => {
    invokeMock.mockResolvedValue({ status: "PENDING" });

    await expect(
      getThreadIssueResolutionStatus({ thread_id: "9007199254740993" }),
    ).resolves.toEqual({ status: "PENDING" });

    expect(invokeMock).toHaveBeenCalledWith("get_thread_issue_resolution_status", {
      req: { thread_id: "9007199254740993" },
    });
  });

  it("listIssueDirectHistoryPage forwards the mode and opaque token", async () => {
    const outcome = { kind: "page" as const, page: { entries: [], next_page_token: "opaque" } };
    invokeMock.mockResolvedValue(outcome);
    const req = { issue_id: "issue-1", mode: "audit" as const, page_token: "opaque" };
    await expect(listIssueDirectHistoryPage(req)).resolves.toEqual(outcome);
    expect(invokeMock).toHaveBeenCalledWith("list_issue_direct_history", { req });
  });

  it("listSummariesForSelection preserves the continuation offset", async () => {
    const req = { kind: "daily" as const, limit: 50, offset: 50 };
    invokeMock.mockResolvedValue({ entries: [], next_offset: 100 });
    const page = await listSummariesForSelection(req);
    expect(invokeMock).toHaveBeenCalledWith("list_summaries_for_selection", { req });
    expect(page.next_offset).toBe(100);
  });

  it("listReflectionSelection invokes the dedicated selection list command", async () => {
    invokeMock.mockResolvedValue({ entries: [], next_cursor_after_memory_id: "100" });
    const page = await listReflectionSelection({ limit: 50, cursor_after_memory_id: "50" });
    expect(invokeMock).toHaveBeenCalledWith("list_reflections_for_selection", {
      req: { limit: 50, cursor_after_memory_id: "50" },
    });
    expect(page.next_cursor_after_memory_id).toBe("100");
  });

  it("getReflectionContent invokes the canonical reflection content command", async () => {
    await getReflectionSelectionContent("99");
    expect(invokeMock).toHaveBeenCalledWith("get_reflection_selection_content", {
      req: { memory_id: "99" },
    });
  });

  it("searchReflectionsByIntent invokes search_reflections_by_intent", async () => {
    const req = { intent_text: "fix flaky tests", top_k: 50 };
    await searchReflectionsByIntent(req);
    expect(invokeMock).toHaveBeenCalledWith("search_reflections_by_intent", { req });
  });

  it("searchReflectionsHybrid invokes search_reflections_hybrid", async () => {
    const req = { query_text: "fix flaky tests", limit: 50 };
    await searchReflectionsHybrid(req);
    expect(invokeMock).toHaveBeenCalledWith("search_reflections_hybrid", { req });
  });

  it("getReflectionIntentIndexStats invokes get_reflection_intent_index_stats", async () => {
    invokeMock.mockResolvedValue({
      total_records: 10,
      records_with_embedding: 8,
      records_without_embedding: 2,
      vector_dimension: 2048,
    });
    const stats = await getReflectionIntentIndexStats();
    expect(invokeMock).toHaveBeenCalledWith("get_reflection_intent_index_stats");
    expect(stats.records_without_embedding).toBe(2);
  });

  it("startImport forwards the post-import generation flags", async () => {
    invokeMock.mockResolvedValue({ job_id: "import-1" });
    const req = {
      sources: ["claude-code" as const],
      dry_run: false,
      labels: [],
      memories_import_bin: "/bin/memories-import",
      run_summary: true,
      run_personality: false,
      run_reflection: true,
    };
    const res = await startImport(req);
    expect(invokeMock).toHaveBeenCalledWith("start_import", { req });
    expect(res.job_id).toBe("import-1");
  });

  it("startImport forwards the plain config verbatim", async () => {
    invokeMock.mockResolvedValue({ job_id: "import-2" });
    const req = {
      sources: ["plain" as const],
      dry_run: false,
      labels: [],
      memories_import_bin: "/bin/memories-import",
      run_summary: false,
      run_personality: false,
      run_reflection: false,
      plain: {
        root: "/Users/me/notes",
        source_name: "notes",
        thread_strategy: "per-dir" as const,
      },
    };
    await startImport(req);
    expect(invokeMock).toHaveBeenCalledWith("start_import", { req });
  });

  it("enqueueSummaryJob defaults to an empty request", async () => {
    invokeMock.mockResolvedValue({ job_id_hint: "summary-1" });
    const res = await enqueueSummaryJob();
    expect(invokeMock).toHaveBeenCalledWith("enqueue_summary_job", { req: {} });
    expect(res.job_id_hint).toBe("summary-1");
  });

  it("enqueuePersonalityJob defaults to an empty request", async () => {
    invokeMock.mockResolvedValue({ job_id_hint: "personality-1" });
    const res = await enqueuePersonalityJob();
    expect(invokeMock).toHaveBeenCalledWith("enqueue_personality_job", { req: {} });
    expect(res.job_id_hint).toBe("personality-1");
  });

  it("generateSummaries forwards the staged request verbatim", async () => {
    invokeMock.mockResolvedValue({ job_id_hint: "summaries-1" });
    const req = {
      run_per_thread: true,
      run_daily: true,
      run_weekly: false,
      run_monthly: false,
      stop_on_error: false,
      daily_start: "2026-05-01",
      daily_end: "2026-05-31",
      weekly_start: "",
      weekly_end: "",
      monthly_start: "",
      monthly_end: "",
      timezone_offset_hours: 9,
    };
    const res = await generateSummaries(req);
    expect(invokeMock).toHaveBeenCalledWith("generate_summaries", { req });
    expect(res.job_id_hint).toBe("summaries-1");
  });

  it("listPersonalitySignals defaults to an empty request", async () => {
    invokeMock.mockResolvedValue({
      signals: [{ source_thread_id: "12345", content_json: "{}", updated_at_ms: 1 }],
      next_offset: null,
    });
    const res = await listPersonalitySignals();
    expect(invokeMock).toHaveBeenCalledWith("list_personality_signals", { req: {} });
    expect(res.signals[0]?.source_thread_id).toBe("12345");
    expect(res.next_offset).toBeNull();
  });

  it("findMemoryPosition passes thread_id and memory_id", async () => {
    invokeMock.mockResolvedValue({ position: 3, thread_total: 40 });
    const res = await findMemoryPosition({ thread_id: "7", memory_id: "99" });
    expect(invokeMock).toHaveBeenCalledWith("find_memory_position", {
      req: { thread_id: "7", memory_id: "99" },
    });
    expect(res?.position).toBe(3);
  });

  it("findMemoryThreadPosition passes memory_id", async () => {
    invokeMock.mockResolvedValue({ thread_id: "7", position: 3, thread_total: 40 });
    const res = await findMemoryThreadPosition({ memory_id: "99" });
    expect(invokeMock).toHaveBeenCalledWith("find_memory_thread_position", {
      req: { memory_id: "99" },
    });
    expect(res?.thread_id).toBe("7");
  });

  it("resolveMemoryReference passes a string id without number coercion", async () => {
    invokeMock.mockResolvedValue(null);
    await resolveMemoryReference("9007199254740993");
    expect(invokeMock).toHaveBeenCalledWith("resolve_memory_reference", {
      req: { memory_id: "9007199254740993" },
    });
  });

  it("findDistinctLabels keeps the UI request free of memory kinds", async () => {
    await findDistinctLabels({ user_id: 1, limit: 100 });
    expect(invokeMock).toHaveBeenCalledWith("find_distinct_labels", {
      req: { user_id: 1, limit: 100 },
    });
  });

  it("findCoOccurringLabels keeps the UI request free of memory kinds", async () => {
    await findCoOccurringLabels({ user_id: 1, labels: ["rust"], limit: 100 });
    expect(invokeMock).toHaveBeenCalledWith("find_co_occurring_labels", {
      req: { user_id: 1, labels: ["rust"], limit: 100 },
    });
  });

  it("redispatchReflectionEmbeddings defaults to an empty request", async () => {
    invokeMock.mockResolvedValue({
      dispatched_count: 2,
      skipped_count: 0,
      failed_count: 0,
      duration_ms: 5,
    });
    const res = await redispatchReflectionEmbeddings();
    expect(invokeMock).toHaveBeenCalledWith("redispatch_reflection_embeddings", { req: {} });
    expect(res.dispatched_count).toBe(2);
  });

  it("getModelStatus invokes get_model_status and returns llm + embedding", async () => {
    invokeMock.mockResolvedValue({
      llm: { state: "ready", error: null, name: "the-llm", repo: "org/the-llm" },
      embedding: { state: "preparing", error: null, name: "the-embed", repo: "org/the-embed" },
    });
    const status = await getModelStatus();
    expect(invokeMock).toHaveBeenCalledWith("get_model_status");
    expect(status.llm.state).toBe("ready");
    expect(status.llm.name).toBe("the-llm");
    expect(status.embedding.state).toBe("preparing");
    expect(status.embedding.repo).toBe("org/the-embed");
  });

  it("retryModelSetup invokes retry_model_setup", async () => {
    invokeMock.mockResolvedValue(undefined);
    await retryModelSetup();
    expect(invokeMock).toHaveBeenCalledWith("retry_model_setup");
  });

  it("getSetupStatus invokes get_setup_status", async () => {
    invokeMock.mockResolvedValue({
      required: true,
      resume_apply: false,
      current_data_root: "/current",
      default_data_root: "/default",
    });
    await getSetupStatus();
    expect(invokeMock).toHaveBeenCalledWith("get_setup_status");
  });

  it("applySetup passes the request through", async () => {
    invokeMock.mockResolvedValue({ restart_required: false });
    const req = {
      data_root: null,
      settings: {
        llm: null,
        embedding: null,
        hf_home: null,
        mcp: null,
        timezone: null,
        issue_tracker: null,
        migration_backup: null,
      },
    };
    await applySetup(req);
    expect(invokeMock).toHaveBeenCalledWith("apply_setup", { req });
  });

  it("getMcpSettings invokes get_mcp_settings", async () => {
    invokeMock.mockResolvedValue({
      enabled: false,
      allow_unauthenticated_requests: false,
      exclude_runner_as_tool: null,
      exclude_worker_as_tool: null,
      streaming: null,
      request_timeout_sec: null,
      rag_default_period_scope: "overall_only",
      set_name: "lookback-mcp",
      active_port: null,
      auth_token_configured: false,
    });
    await getMcpSettings();
    expect(invokeMock).toHaveBeenCalledWith("get_mcp_settings");
  });

  it("getIssueTrackerSettings invokes get_issue_tracker_settings", async () => {
    invokeMock.mockResolvedValue({ enabled: null, effective_enabled: false });

    await getIssueTrackerSettings();

    expect(invokeMock).toHaveBeenCalledWith("get_issue_tracker_settings");
  });

  it("reads and persists chat tool approval settings", async () => {
    invokeMock.mockResolvedValueOnce({ require_approval: true });
    await expect(getChatToolSettings()).resolves.toEqual({ require_approval: true });
    expect(invokeMock).toHaveBeenLastCalledWith("get_chat_tool_settings");

    invokeMock.mockResolvedValueOnce({ require_approval: false });
    await expect(setChatToolSettings({ require_approval: false })).resolves.toEqual({
      require_approval: false,
    });
    expect(invokeMock).toHaveBeenLastCalledWith("set_chat_tool_settings", {
      req: { require_approval: false },
    });
  });

  it("responds to one tool approval using the job and call correlation ids", async () => {
    invokeMock.mockResolvedValue(undefined);

    await chatRespondToolApproval("chat-7", "call-2", false);

    expect(invokeMock).toHaveBeenCalledWith("chat_respond_tool_approval", {
      jobId: "chat-7",
      callId: "call-2",
      approved: false,
    });
  });

  it("applySettings forwards the Issue Tracker payload", async () => {
    const req: ApplySettingsRequest = {
      llm: null,
      embedding: null,
      hf_home: null,
      mcp: null,
      timezone: null,
      issue_tracker: { enabled: true },
      migration_backup: null,
    };

    await applySettings(req);

    expect(invokeMock).toHaveBeenCalledWith("apply_settings", { req });
  });

  it("generateMcpAuthToken returns the one-time token from the command", async () => {
    invokeMock.mockResolvedValue("generated-token");

    await expect(generateMcpAuthToken()).resolves.toBe("generated-token");
    expect(invokeMock).toHaveBeenCalledWith("generate_mcp_auth_token");
  });

  it("rotateMcpAuthToken returns the replacement one-time token from the command", async () => {
    invokeMock.mockResolvedValue("rotated-token");

    await expect(rotateMcpAuthToken()).resolves.toBe("rotated-token");
    expect(invokeMock).toHaveBeenCalledWith("rotate_mcp_auth_token");
  });

  it("setMcpSettings passes the request under the req key", async () => {
    invokeMock.mockResolvedValue({
      restarted: true,
      embedding_runtime: null,
      warnings: [],
    });
    const req: SetMcpSettingsRequest = {
      enabled: true,
      allow_unauthenticated_requests: false,
      exclude_runner_as_tool: null,
      exclude_worker_as_tool: null,
      streaming: null,
      request_timeout_sec: null,
      rag_default_period_scope: "overall_only",
    };
    await setMcpSettings(req);
    expect(invokeMock).toHaveBeenCalledWith("set_mcp_settings", { req });
  });

  it("getConnectionConfig invokes get_connection_config", async () => {
    invokeMock.mockResolvedValue(LOCAL_CONNECTION_CONFIG);
    const cfg = await getConnectionConfig();
    expect(invokeMock).toHaveBeenCalledWith("get_connection_config");
    expect(cfg.memories.mode).toBe("local");
  });

  it("setConnectionConfig passes the cfg through", async () => {
    invokeMock.mockResolvedValue(undefined);
    const cfg = remoteConnectionConfig("http://h:9000", "http://h:9010");
    await setConnectionConfig(cfg);
    expect(invokeMock).toHaveBeenCalledWith("set_connection_config", { cfg });
  });

  it("testConnectionConfig passes the cfg through", async () => {
    invokeMock.mockResolvedValue({
      jobworkerp_url: "http://h:9000",
      memories_url: "http://h:9010",
    });
    const cfg = remoteConnectionConfig("http://h:9000", "http://h:9010");
    const report = await testConnectionConfig(cfg);
    expect(invokeMock).toHaveBeenCalledWith("test_connection_config", { cfg });
    expect(report.memories_url).toBe("http://h:9010");
  });

  it("remote jobworkerp wrappers invoke their commands without arguments", async () => {
    invokeMock.mockResolvedValue({ warnings: [], readiness: {} });
    await inspectRemoteJobworkerp();
    expect(invokeMock).toHaveBeenCalledWith("inspect_remote_jobworkerp");
    await registerRemoteJobworkerpWorkers();
    expect(invokeMock).toHaveBeenCalledWith("register_remote_jobworkerp_workers");
  });

  it("periodic task wrappers invoke conductor-backed commands", async () => {
    const task = {
      name: "朝",
      source: "codex",
      sources: ["codex"],
      task_kind: "regular" as const,
      hour: 9,
      minute: 0,
      interval_hours: 24,
      interval_days: null,
      weekly_day: null,
      monthly_day: null,
      lookback_days: 7,
      force_thread_summary: true,
    };
    const saveReq = { id: "42", task, enabled: true, description: null };

    invokeMock.mockResolvedValueOnce([{ id: "42" }]);
    await listPeriodicTasks({ limit: 100, offset: 0 });
    expect(invokeMock).toHaveBeenLastCalledWith("list_periodic_tasks", {
      req: { limit: 100, offset: 0 },
    });

    invokeMock.mockResolvedValueOnce("42");
    await createPeriodicTask(saveReq);
    expect(invokeMock).toHaveBeenLastCalledWith("create_periodic_task", { req: saveReq });

    invokeMock.mockResolvedValueOnce(undefined);
    await updatePeriodicTask(saveReq);
    expect(invokeMock).toHaveBeenLastCalledWith("update_periodic_task", { req: saveReq });

    invokeMock.mockResolvedValueOnce(undefined);
    await deletePeriodicTask("42");
    expect(invokeMock).toHaveBeenLastCalledWith("delete_periodic_task", { req: { id: "42" } });

    invokeMock.mockResolvedValueOnce(undefined);
    await setEnabledPeriodicTask("42", false);
    expect(invokeMock).toHaveBeenLastCalledWith("set_enabled_periodic_task", {
      req: { id: "42", enabled: false },
    });
  });

  it("periodic execution status/history/cancel wrappers pass command names and payloads", async () => {
    invokeMock.mockResolvedValueOnce([]);
    await listPeriodicTaskStatuses(["3", "1"]);
    expect(invokeMock).toHaveBeenLastCalledWith("list_periodic_task_statuses", {
      req: { scheduler_ids: ["3", "1"] },
    });

    invokeMock.mockResolvedValueOnce([]);
    await listPeriodicExecutionHistory("42", 20);
    expect(invokeMock).toHaveBeenLastCalledWith("list_periodic_execution_history", {
      req: { scheduler_id: "42", limit: 20 },
    });

    // Omitting limit leaves it undefined so the Rust side applies its default.
    invokeMock.mockResolvedValueOnce([]);
    await listPeriodicExecutionHistory("42");
    expect(invokeMock).toHaveBeenLastCalledWith("list_periodic_execution_history", {
      req: { scheduler_id: "42", limit: undefined },
    });

    invokeMock.mockResolvedValueOnce(undefined);
    await cancelPeriodicExecution("99");
    expect(invokeMock).toHaveBeenLastCalledWith("cancel_periodic_execution", {
      req: { execution_ref_id: "99" },
    });
  });

  it("readSidecarLog passes source/stream/maxBytes", async () => {
    invokeMock.mockResolvedValue({
      file_name: "memories.stdout.log",
      content: "",
      truncated: false,
      file_size: 0,
    });
    await readSidecarLog("memories", "stdout");
    expect(invokeMock).toHaveBeenCalledWith("read_sidecar_log", {
      source: "memories",
      stream: "stdout",
      maxBytes: undefined,
    });
    await readSidecarLog("jobworkerp", "stderr", 4096);
    expect(invokeMock).toHaveBeenCalledWith("read_sidecar_log", {
      source: "jobworkerp",
      stream: "stderr",
      maxBytes: 4096,
    });
    await readSidecarLog("issue_tracker", "stderr");
    expect(invokeMock).toHaveBeenCalledWith("read_sidecar_log", {
      source: "issue_tracker",
      stream: "stderr",
      maxBytes: undefined,
    });
  });

  it("maps coding-agent settings and exact-revision updates to the registered Tauri commands", async () => {
    const settings: CodingAgentSettings = {
      formatVersion: 1,
      revision: 7,
      activeProfileId: "profile-a",
      semanticJudgeEnabled: true,
      autoDoneEnabled: false,
      profiles: [],
      repositoryHosts: [],
    };
    const loaded = {
      settings,
      profileMountAvailability: [],
      availableAgentPresets: [
        {
          id: "opencode",
          displayName: "OpenCode",
          imageRef: "ghcr.io/sutr-app/opencode:20261025_0",
          command: ["opencode", "run"],
          containerHome: "/home/codespace/.agent-home",
          runtimeEnvironment: {
            CARGO_HOME: "/home/codespace/.cargo",
            RUSTUP_HOME: "/home/codespace/.rustup",
          },
        },
      ],
    };
    invokeMock.mockResolvedValueOnce(loaded);

    await expect(codingAgentGetSettings()).resolves.toEqual(loaded);
    expect(invokeMock).toHaveBeenLastCalledWith("coding_agent_get_settings");

    invokeMock.mockResolvedValueOnce(loaded);
    await codingAgentUpdateSettings({ expectedRevision: 7, settings });
    expect(invokeMock).toHaveBeenLastCalledWith("coding_agent_update_settings", {
      request: { expectedRevision: 7, settings },
    });
    expect(settings.activeProfileId).toBe("profile-a");
    expect(settings.autoDoneEnabled).toBe(false);
    invokeMock.mockResolvedValueOnce(loaded);
    await expect(
      codingAgentPrepareProfileHome({ expectedRevision: 7, profileId: "profile-a" }),
    ).resolves.toEqual(loaded);
    expect(invokeMock).toHaveBeenLastCalledWith("coding_agent_prepare_profile_home", {
      request: { expectedRevision: 7, profileId: "profile-a" },
    });
  });

  it("forwards a repository host check with the draft host, optional target, and CAS revision", async () => {
    const host: CodingAgentRepositoryHost = {
      labelHost: "forge.example",
      forge: "gitea",
      apiBaseUrl: "https://forge.example/api/v1",
      cloneTransport: { kind: "ssh", host: "git.forge.example", user: "git", port: 2222 },
      readCredentialRef: "keyring:read-ref",
      publicationCredentialRef: "keyring:publish-ref",
    };
    const repository: CodingAgentRepositoryIdentity = {
      host: "forge.example",
      organization: "team",
      repository: "project",
    };
    const response = {
      settingsRevision: 12,
      repository,
      stages: [
        { stage: "configuration", status: "passed", code: "settings_valid" },
        { stage: "trusted_policy", status: "passed", code: "policy_matched" },
        { stage: "git_read", status: "skipped", code: "repository_required" },
        { stage: "forge_api", status: "skipped", code: "repository_required" },
      ],
    };
    invokeMock.mockResolvedValueOnce(response);

    await expect(
      codingAgentCheckRepositoryHost({ expectedRevision: 12, host, repository }),
    ).resolves.toEqual(response);
    expect(invokeMock).toHaveBeenLastCalledWith("coding_agent_check_repository_host", {
      request: { expectedRevision: 12, host, repository },
    });
  });

  it("forwards coding-agent preview claims without inventing manager or host authority", async () => {
    const request: CodingAgentRunPreviewRequest = {
      issueId: "issue-1",
      profileId: "profile-a",
      executionMode: "one_shot",
      purpose: "investigation",
      publicationPlan: "no_publication",
      autoDoneRequested: false,
    };
    const preview = {
      planId: "plan-id",
      digest: "actual-digest",
      issueId: "issue-1",
      issueRevision: 3,
      issueTitle: "Investigate",
      issueState: "open",
      sourceLabels: [],
      settingsRevision: 7,
      profileId: "profile-a",
      profileVersion: 2,
      executionMode: "one_shot",
      purpose: "investigation",
      publicationPlan: "no_publication",
      selectedRepository: null,
      repositorySelectionReason: null,
      repositoryCandidates: [],
      autoDoneRequested: false,
      blockers: ["approved_base_commit_unavailable"],
      canApprove: false,
    };
    invokeMock.mockResolvedValueOnce(preview);

    await expect(codingAgentPreviewRun(request)).resolves.toEqual(preview);
    expect(invokeMock).toHaveBeenLastCalledWith("coding_agent_preview_run", { request });
  });

  it("reads delegated proposals for the selected Issue without starting execution", async () => {
    const plans = [
      {
        delegationPlanId: "proposal",
        request: {
          issueId: "issue-1",
          profileId: "profile",
          purpose: "investigation",
          prompt: null,
        },
        issueRevision: 1,
        settingsRevision: 1,
        createdAtMs: 1,
        runId: null,
      },
    ];
    invokeMock.mockResolvedValueOnce(plans);
    await expect(codingAgentListDelegationPlans("issue-1")).resolves.toEqual(plans);
    expect(invokeMock).toHaveBeenLastCalledWith("coding_agent_list_delegation_plans", {
      issueId: "issue-1",
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("uses the approved digest for start and keeps terminal operations owner-scoped", async () => {
    const preview: CodingAgentRunPreviewRequest = {
      issueId: "issue-1",
      profileId: "profile-a",
      executionMode: "one_shot",
      purpose: "implementation",
      publicationPlan: "no_publication",
      autoDoneRequested: false,
    };
    await codingAgentApproveAndStart({
      preview,
      delegationPlanId: "delegation-one",
      expectedPlanId: "plan-id",
      expectedDigest: "server-digest",
      userConfirmed: true,
      manualRepositoryConfirmed: false,
      autoDoneConfirmed: false,
    });
    expect(invokeMock).toHaveBeenLastCalledWith("coding_agent_approve_and_start", {
      request: {
        preview,
        delegationPlanId: "delegation-one",
        expectedPlanId: "plan-id",
        expectedDigest: "server-digest",
        userConfirmed: true,
        manualRepositoryConfirmed: false,
        autoDoneConfirmed: false,
      },
    });

    await codingAgentAttachRun("run-1");
    await codingAgentCancelRun("run-1");
    expect(invokeMock.mock.calls.slice(-2)).toEqual([
      ["coding_agent_attach_run", { runId: "run-1" }],
      ["coding_agent_cancel_run", { runId: "run-1" }],
    ]);
  });

  it("maps status, run, evidence, and human-review reads and writes to their native commands", async () => {
    const report: CodingAgentReviewActionRequest = {
      runId: "run-1",
      expectedRunRevision: 4,
      expectedIssueRevision: 8,
      expectedMarker: {
        runId: "run-1",
        fullCommitId: "a".repeat(40),
        evidenceReferences: ["evidence-1"],
      },
      action: "request_changes",
      rationale: "Please address the missing test.",
      report: null,
    };
    await codingAgentGetBackendStatus();
    await codingAgentListRuns();
    await codingAgentGetRun("run-1");
    await codingAgentListEvidence("run-1");
    await codingAgentReadEvidence("run-1", "evidence-1");
    await codingAgentGetReview("run-1");
    await codingAgentApplyReviewAction(report);
    const reassessment = {
      runId: report.runId,
      expectedRunRevision: report.expectedRunRevision,
      expectedIssueRevision: report.expectedIssueRevision,
      expectedMarker: report.expectedMarker,
      requestKey: "human-request-1",
    };
    await codingAgentReassess(reassessment);

    expect(invokeMock.mock.calls).toEqual([
      ["coding_agent_get_backend_status"],
      ["coding_agent_list_runs"],
      ["coding_agent_get_run", { runId: "run-1" }],
      ["coding_agent_list_evidence", { runId: "run-1" }],
      ["coding_agent_read_evidence", { runId: "run-1", evidenceId: "evidence-1" }],
      ["coding_agent_get_review", { runId: "run-1" }],
      ["coding_agent_apply_review_action", { request: report }],
      ["coding_agent_reassess", { request: reassessment }],
    ]);
  });
});
