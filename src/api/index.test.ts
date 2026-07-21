import { beforeEach, describe, expect, it, vi } from "vitest";
import type { SummaryEntry } from "@/types/api";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

import {
  applySetup,
  cancelPeriodicExecution,
  createPeriodicTask,
  deletePeriodicTask,
  enqueuePersonalityJob,
  enqueueSummaryJob,
  findCoOccurringLabels,
  findDistinctLabels,
  findMemoryPosition,
  findMemoryThreadPosition,
  generateSummaries,
  getConnectionConfig,
  getMcpSettings,
  getModelStatus,
  getReflectionIntentIndexStats,
  getSetupStatus,
  listPeriodicExecutionHistory,
  listPeriodicTaskStatuses,
  listPeriodicTasks,
  listPersonalitySignals,
  parsePersonalitySignalContent,
  parseSummaryContent,
  readSidecarLog,
  redispatchReflectionEmbeddings,
  retryModelSetup,
  searchMemoriesHybrid,
  searchReflectionsByIntent,
  searchReflectionsHybrid,
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
    invokeMock.mockResolvedValue([
      { source_thread_id: "12345", content_json: "{}", updated_at_ms: 1 },
    ]);
    const res = await listPersonalitySignals();
    expect(invokeMock).toHaveBeenCalledWith("list_personality_signals", { req: {} });
    expect(res[0]?.source_thread_id).toBe("12345");
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
      settings: { llm: null, embedding: null, hf_home: null, mcp: null, timezone: null },
    };
    await applySetup(req);
    expect(invokeMock).toHaveBeenCalledWith("apply_setup", { req });
  });

  it("getMcpSettings invokes get_mcp_settings", async () => {
    invokeMock.mockResolvedValue({
      enabled: false,
      exclude_runner_as_tool: null,
      exclude_worker_as_tool: null,
      streaming: null,
      request_timeout_sec: null,
      set_name: "lookback-mcp-rag",
      active_port: null,
    });
    await getMcpSettings();
    expect(invokeMock).toHaveBeenCalledWith("get_mcp_settings");
  });

  it("setMcpSettings passes the request under the req key", async () => {
    invokeMock.mockResolvedValue({
      restarted: true,
      backup_path: null,
      embedding_runtime: null,
      warnings: [],
    });
    const req = {
      enabled: true,
      exclude_runner_as_tool: null,
      exclude_worker_as_tool: null,
      streaming: null,
      request_timeout_sec: null,
    };
    await setMcpSettings(req);
    expect(invokeMock).toHaveBeenCalledWith("set_mcp_settings", { req });
  });

  it("getConnectionConfig invokes get_connection_config", async () => {
    invokeMock.mockResolvedValue({
      mode: "local",
      remote_jobworkerp_url: null,
      remote_memories_url: null,
    });
    const cfg = await getConnectionConfig();
    expect(invokeMock).toHaveBeenCalledWith("get_connection_config");
    expect(cfg.mode).toBe("local");
  });

  it("setConnectionConfig passes the cfg through", async () => {
    invokeMock.mockResolvedValue(undefined);
    const cfg = {
      mode: "remote" as const,
      remote_jobworkerp_url: "http://h:9000",
      remote_memories_url: "http://h:9010",
    };
    await setConnectionConfig(cfg);
    expect(invokeMock).toHaveBeenCalledWith("set_connection_config", { cfg });
  });

  it("testConnectionConfig passes the cfg through", async () => {
    invokeMock.mockResolvedValue({
      jobworkerp_url: "http://h:9000",
      memories_url: "http://h:9010",
    });
    const cfg = {
      mode: "remote" as const,
      remote_jobworkerp_url: "http://h:9000",
      remote_memories_url: "http://h:9010",
    };
    const report = await testConnectionConfig(cfg);
    expect(invokeMock).toHaveBeenCalledWith("test_connection_config", { cfg });
    expect(report.memories_url).toBe("http://h:9010");
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
  });
});
