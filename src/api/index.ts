import { invoke } from "@tauri-apps/api/core";
import type {
  ApplySettingsRequest,
  ApplySettingsResponse,
  ApplySetupRequest,
  ApplySetupResponse,
  AppSettingsResponse,
  ChatAskRequest,
  ChatAskResponse,
  ConnectionConfig,
  ConnectionTestReport,
  CountSummariesRequest,
  DataRootValidation,
  DebugPersonalityInventoryRequest,
  EmbeddingPreset,
  EmbeddingSettingsResponse,
  EnqueueAnalysisJobResponse,
  EnqueuePeriodSummaryJobRequest,
  EnqueuePersonalityJobRequest,
  EnqueuePersonalityMergeJobRequest,
  EnqueueReflectionJobRequest,
  EnqueueReflectionJobResponse,
  EnqueueSummaryJobRequest,
  FindCoOccurringLabelsRequest,
  FindDistinctLabelsRequest,
  FindMemoriesRequest,
  FindMemoryPositionRequest,
  FindMemoryThreadPositionRequest,
  GenerateSummariesRequest,
  GetPersonalityRequest,
  LabelWithCount,
  ListPeriodicTasksRequest,
  ListPersonalitySignalsRequest,
  ListReflectionsByThreadRequest,
  ListSummariesRequest,
  ListSummaryPeriodKeysRequest,
  ListThreadsRequest,
  LlmPreset,
  LlmSettingsResponse,
  LogSource,
  LogStream,
  LogTail,
  McpSettingsResponse,
  MemoryEmbeddingStats,
  MemoryPosition,
  MemoryRow,
  MemoryThreadPosition,
  ModelStatusReport,
  PeriodicExecutionHistoryEntry,
  PeriodicExecutionSummary,
  PeriodicTaskEntry,
  PersonalityInventoryReport,
  PersonalityProfileContent,
  PersonalityResponse,
  PersonalitySignal,
  PersonalitySignalContent,
  PurgeReport,
  RecoveryResult,
  RedispatchEmbeddingsResult,
  RedispatchMemoryEmbeddingsRequest,
  RedispatchReflectionEmbeddingsRequest,
  ReflectionEntry,
  ReflectionIntentIndexStats,
  ResolvedSummaryMemoryRef,
  SavePeriodicTaskRequest,
  SearchReflectionsByIntentRequest,
  SearchReflectionsRequest,
  SearchThreadsRequest,
  SetEmbeddingSettingsRequest,
  SetEmbeddingSettingsResponse,
  SetHfHomeRequest,
  SetLlmSettingsRequest,
  SetMcpSettingsRequest,
  SettingsSnapshot,
  SetupStatus,
  SidecarStatusSnapshot,
  StartImportRequest,
  StartImportResponse,
  SummaryContent,
  SummaryEntry,
  ThreadHit,
  ThreadSummary,
} from "@/types/api";

export async function listThreads(req: ListThreadsRequest): Promise<ThreadSummary[]> {
  return invoke<ThreadSummary[]>("list_threads", { req });
}

/** Fetch a single thread row by id; `null` if it no longer exists. Used to
 *  hydrate ThreadDetail's header when opened from a cross-tab jump that only
 *  had the thread id (the synthesized ThreadSummary has empty channel/labels). */
export async function findThread(thread_id: string): Promise<ThreadSummary | null> {
  return invoke<ThreadSummary | null>("find_thread", { req: { thread_id } });
}

export async function findDistinctLabels(
  req: FindDistinctLabelsRequest = {},
): Promise<LabelWithCount[]> {
  return invoke<LabelWithCount[]>("find_distinct_labels", { req });
}

export async function findCoOccurringLabels(
  req: FindCoOccurringLabelsRequest,
): Promise<LabelWithCount[]> {
  return invoke<LabelWithCount[]>("find_co_occurring_labels", { req });
}

export async function findMemoryPosition(
  req: FindMemoryPositionRequest,
): Promise<MemoryPosition | null> {
  return invoke<MemoryPosition | null>("find_memory_position", { req });
}

export async function findMemoryThreadPosition(
  req: FindMemoryThreadPositionRequest,
): Promise<MemoryThreadPosition | null> {
  return invoke<MemoryThreadPosition | null>("find_memory_thread_position", { req });
}

export async function findMemoriesByThreadId(req: FindMemoriesRequest): Promise<MemoryRow[]> {
  return invoke<MemoryRow[]>("find_memories_by_thread_id", { req });
}

export async function countThreads(): Promise<number> {
  return invoke<number>("count_threads");
}

/** Delete a thread and (server cascade) every Memory attached to it. */
export async function deleteThread(thread_id: string): Promise<void> {
  return invoke<void>("delete_thread", { req: { thread_id } });
}

export async function listSummaries(req: ListSummariesRequest): Promise<SummaryEntry[]> {
  return invoke<SummaryEntry[]>("list_summaries", { req });
}

export async function countSummaries(req: CountSummariesRequest = {}): Promise<number> {
  return invoke<number>("count_summaries", { req });
}

/** Distinct `period_key`s with a summary in the window. Backs the calendar
 *  dots — lighter than `listSummaries` since the content is not transferred. */
export async function listSummaryPeriodKeys(req: ListSummaryPeriodKeysRequest): Promise<string[]> {
  return invoke<string[]>("list_summary_period_keys", { req });
}

/** Delete a single summary (per-thread / daily / weekly / monthly) by its
 *  backing Memory row id. */
export async function deleteSummary(memory_id: string): Promise<void> {
  return invoke<void>("delete_summary", { req: { memory_id } });
}

/** Resolve a `source_memory_ids` chip click: returns the cited summary
 *  memory's parsed navigation coordinates (per-thread → thread_id; period
 *  → kind/period_key/scope_key). Returns null when the memory no longer
 *  exists, in which case the UI disables the chip with a tooltip. */
export async function resolveSummaryMemoryRef(
  memory_id: string,
): Promise<ResolvedSummaryMemoryRef | null> {
  return invoke<ResolvedSummaryMemoryRef | null>("resolve_summary_memory_ref", {
    req: { memory_id },
  });
}

export async function startImport(req: StartImportRequest): Promise<StartImportResponse> {
  return invoke<StartImportResponse>("start_import", { req });
}

/** Cancel an in-flight import pipeline. Idempotent: a finished or
 *  unknown dispatch id resolves silently so the toast can fire-and-forget. */
export async function startImportCancel(dispatchId: string): Promise<void> {
  return invoke<void>("start_import_cancel", { dispatchId });
}

/** Cancel an in-flight analysis dispatch (summary / personality / period
 *  summary / staged summaries pipeline) by its dispatch id. Idempotent. */
export async function analysisCancel(dispatchId: string): Promise<void> {
  return invoke<void>("analysis_cancel", { dispatchId });
}

export async function getSettings(): Promise<SettingsSnapshot> {
  return invoke<SettingsSnapshot>("get_settings");
}

/** Persist the generation output language ("ja" | "en") so headless paths
 *  (conductor periodic runs) generate in the UI's current language. The UI
 *  locale itself stays in localStorage; this mirror is read by the backend. */
export async function setOutputLanguage(lang: "ja" | "en"): Promise<void> {
  return invoke<void>("set_output_language", { lang });
}

export async function getSidecarStatus(): Promise<SidecarStatusSnapshot> {
  return invoke<SidecarStatusSnapshot>("get_sidecar_status");
}

export async function purgeAllData(): Promise<PurgeReport> {
  return invoke<PurgeReport>("purge_all_data");
}

export async function listReflectionsByThread(
  req: ListReflectionsByThreadRequest,
): Promise<ReflectionEntry[]> {
  return invoke<ReflectionEntry[]>("list_reflections_by_thread", { req });
}

export async function searchReflections(req: SearchReflectionsRequest): Promise<ReflectionEntry[]> {
  return invoke<ReflectionEntry[]>("search_reflections", { req });
}

/** Delete a single reflection by its id. */
export async function deleteReflection(id: string): Promise<void> {
  return invoke<void>("delete_reflection", { req: { id } });
}

export async function getPersonality(
  req: GetPersonalityRequest = {},
): Promise<PersonalityResponse> {
  return invoke<PersonalityResponse>("get_personality", { req });
}

export async function listPersonalitySignals(
  req: ListPersonalitySignalsRequest = {},
): Promise<PersonalitySignal[]> {
  return invoke<PersonalitySignal[]>("list_personality_signals", { req });
}

/** Delete a single layer-1 personality signal by its backing Memory row id. */
export async function deletePersonalitySignal(memory_id: string): Promise<void> {
  return invoke<void>("delete_personality_signal", { req: { memory_id } });
}

/** Delete the merged personality profile by its backing Memory row id. */
export async function deletePersonalityProfile(memory_id: string): Promise<void> {
  return invoke<void>("delete_personality_profile", { req: { memory_id } });
}

/**
 * Temporary investigation: cross-tabulates threads / memories under
 * `personality_user_id=200000` to expose which failure mode the
 * "signals never grow" symptom is in (LLM all-no_signal vs missing
 * AddLabels vs memory not stored). See `debug_personality_inventory`
 * on the Rust side for the field semantics.
 */
export async function debugPersonalityInventory(
  req: DebugPersonalityInventoryRequest = {},
): Promise<PersonalityInventoryReport> {
  return invoke<PersonalityInventoryReport>("debug_personality_inventory", { req });
}

export async function listPeriodicTasks(
  req: ListPeriodicTasksRequest = {},
): Promise<PeriodicTaskEntry[]> {
  return invoke<PeriodicTaskEntry[]>("list_periodic_tasks", { req });
}

export async function createPeriodicTask(req: SavePeriodicTaskRequest): Promise<string> {
  return invoke<string>("create_periodic_task", { req });
}

export async function updatePeriodicTask(req: SavePeriodicTaskRequest): Promise<void> {
  return invoke<void>("update_periodic_task", { req });
}

export async function deletePeriodicTask(id: string): Promise<void> {
  return invoke<void>("delete_periodic_task", { req: { id } });
}

export async function setEnabledPeriodicTask(id: string, enabled: boolean): Promise<void> {
  return invoke<void>("set_enabled_periodic_task", { req: { id, enabled } });
}

export async function listPeriodicTaskStatuses(
  schedulerIds: string[],
): Promise<PeriodicExecutionSummary[]> {
  return invoke<PeriodicExecutionSummary[]>("list_periodic_task_statuses", {
    req: { scheduler_ids: schedulerIds },
  });
}

export async function listPeriodicExecutionHistory(
  schedulerId: string,
  limit?: number,
): Promise<PeriodicExecutionHistoryEntry[]> {
  return invoke<PeriodicExecutionHistoryEntry[]>("list_periodic_execution_history", {
    req: { scheduler_id: schedulerId, limit },
  });
}

export async function cancelPeriodicExecution(executionRefId: string): Promise<void> {
  return invoke<void>("cancel_periodic_execution", {
    req: { execution_ref_id: executionRefId },
  });
}

export async function searchMemoriesKeyword(req: SearchThreadsRequest): Promise<ThreadHit[]> {
  return invoke<ThreadHit[]>("search_memories_keyword", { req });
}

export async function searchMemoriesSemantic(req: SearchThreadsRequest): Promise<ThreadHit[]> {
  return invoke<ThreadHit[]>("search_memories_semantic", { req });
}

export async function searchMemoriesHybrid(req: SearchThreadsRequest): Promise<ThreadHit[]> {
  return invoke<ThreadHit[]>("search_memories_hybrid", { req });
}

export async function searchReflectionsByIntent(
  req: SearchReflectionsByIntentRequest,
): Promise<ReflectionEntry[]> {
  return invoke<ReflectionEntry[]>("search_reflections_by_intent", { req });
}

/** Intent-vector index health (total / with / without embedding). Lets the
 *  user confirm auto-embedding is running and whether a backfill is needed. */
export async function getReflectionIntentIndexStats(): Promise<ReflectionIntentIndexStats> {
  return invoke<ReflectionIntentIndexStats>("get_reflection_intent_index_stats");
}

/** Backfill missing intent embeddings for existing reflections (one-off
 *  recovery for pre-C-3b data or failed auto-dispatch). Unary/synchronous —
 *  resolves with the dispatched/skipped/failed counts. */
export async function redispatchReflectionEmbeddings(
  req: RedispatchReflectionEmbeddingsRequest = {},
): Promise<RedispatchEmbeddingsResult> {
  return invoke<RedispatchEmbeddingsResult>("redispatch_reflection_embeddings", { req });
}

/** Memory embedding index health (summary / thread coverage). Used by the
 *  Settings "Embedding index" card next to the Embedding model card. */
export async function getMemoryEmbeddingStats(): Promise<MemoryEmbeddingStats> {
  return invoke<MemoryEmbeddingStats>("get_memory_embedding_stats");
}

/** Re-enqueue embedding jobs for every memory. Idempotent at the server
 *  side (RDB scan + Upsert), so this single entry covers both "fill the
 *  gaps after import" and "rebuild after switching embedding model". */
export async function redispatchMemoryEmbeddings(
  req: RedispatchMemoryEmbeddingsRequest = {},
): Promise<RedispatchEmbeddingsResult> {
  return invoke<RedispatchEmbeddingsResult>("redispatch_memory_embeddings", { req });
}

export async function getModelStatus(): Promise<ModelStatusReport> {
  return invoke<ModelStatusReport>("get_model_status");
}

/** Restart sidecars to recover model setup. Resolves
 *  once the restart settles (the Rust command emits sidecar://ready|error). */
export async function retryModelSetup(): Promise<void> {
  return invoke<void>("retry_model_setup");
}

export async function enqueueReflectionJob(
  req: EnqueueReflectionJobRequest = {},
): Promise<EnqueueReflectionJobResponse> {
  return invoke<EnqueueReflectionJobResponse>("enqueue_reflection_job", { req });
}

export async function reflectionCancel(dispatchId: string): Promise<void> {
  return invoke<void>("reflection_cancel", { dispatchId });
}

/** Run the summary batch later, after an import-only run. */
export async function enqueueSummaryJob(
  req: EnqueueSummaryJobRequest = {},
): Promise<EnqueueAnalysisJobResponse> {
  return invoke<EnqueueAnalysisJobResponse>("enqueue_summary_job", { req });
}

/** Run the personality batch later, after an import-only run. */
export async function enqueuePersonalityJob(
  req: EnqueuePersonalityJobRequest = {},
): Promise<EnqueueAnalysisJobResponse> {
  return invoke<EnqueueAnalysisJobResponse>("enqueue_personality_job", { req });
}

/** Run the Layer-2 merge ALONE (skips per-thread fan-out). Used when a
 *  previous personality run left valid layer-1 signals but never produced a
 *  profile — e.g. an external-LLM 429 storm during per-thread extraction.
 *  Progress streams on the shared `personality://step` event. */
export async function enqueuePersonalityMergeJob(
  req: EnqueuePersonalityMergeJobRequest = {},
): Promise<EnqueueAnalysisJobResponse> {
  return invoke<EnqueueAnalysisJobResponse>("enqueue_personality_merge_job", { req });
}

/** Generate a period (daily/weekly/monthly) work summary. Reuses the
 *  `summary://step` progress event. Caller drives the daily→weekly→monthly
 *  order; each layer no-ops if its source layer hasn't been generated. */
export async function enqueuePeriodSummaryJob(
  req: EnqueuePeriodSummaryJobRequest,
): Promise<EnqueueAnalysisJobResponse> {
  return invoke<EnqueueAnalysisJobResponse>("enqueue_period_summary_job", { req });
}

/** Staged generate: runs the per-thread → daily → weekly → monthly chain in a
 *  single pipeline workflow gated by the `run_*` flags. The range is already
 *  expanded per layer by the caller. Progress streams on `summary://step`. */
export async function generateSummaries(
  req: GenerateSummariesRequest,
): Promise<EnqueueAnalysisJobResponse> {
  return invoke<EnqueueAnalysisJobResponse>("generate_summaries", { req });
}

/** Submit a question to the RAG chat. The actual answer streams on
 *  `chat://step` (Token / Source / Searching / Done / Error phases) —
 *  the returned `job_id_hint` is the correlation key for the events. */
export async function chatAsk(req: ChatAskRequest): Promise<ChatAskResponse> {
  return invoke<ChatAskResponse>("chat_ask", { req });
}

/** OPEN-CHAT-2: cancel an in-flight chat by its UI-side `jobId`.
 *  Idempotent — an unknown / finished jobId resolves silently. */
export async function chatCancel(jobId: string): Promise<void> {
  return invoke<void>("chat_cancel", { jobId });
}

/** Read the persisted connection-target override. */
export async function getConnectionConfig(): Promise<ConnectionConfig> {
  return invoke<ConnectionConfig>("get_connection_config");
}

/** Persist the connection override and invalidate cached gRPC
 *  clients so the next command reconnects to the new target. */
export async function setConnectionConfig(cfg: ConnectionConfig): Promise<void> {
  return invoke<void>("set_connection_config", { cfg });
}

/** Dial the selected connection target so remote setup failures surface before
 *  the user navigates to a data page and only sees an empty result. */
export async function testConnectionConfig(cfg: ConnectionConfig): Promise<ConnectionTestReport> {
  return invoke<ConnectionTestReport>("test_connection_config", { cfg });
}

/** Read LLM provider settings (local vs external). */
export async function getLlmSettings(): Promise<LlmSettingsResponse> {
  return invoke<LlmSettingsResponse>("get_llm_settings");
}

/** Persist LLM provider settings. Restarts sidecars when model/key changes. */
export async function setLlmSettings(req: SetLlmSettingsRequest): Promise<void> {
  return invoke<void>("set_llm_settings", { req });
}

/** Read the app-wide settings (data root override + HF_HOME mode) and the
 *  resolved paths the running sidecar / next launch will use. */
export async function getAppSettings(): Promise<AppSettingsResponse> {
  return invoke<AppSettingsResponse>("get_app_settings");
}

/** List the selectable IANA timezone names from the host tz database. Empty
 *  when the host has no zoneinfo dir (the UI then offers Auto + free text). */
export async function listTimezones(): Promise<string[]> {
  return invoke<string[]>("list_timezones");
}

/** Persist the data-root override to `bootstrap.json`. Takes effect on the
 *  NEXT app launch — sqlite / LanceDB / tonic channels are bound to the
 *  current root for the life of this process. Pass `null` to clear. */
export async function setDataRoot(path: string | null): Promise<void> {
  return invoke<void>("set_data_root", { path });
}

/** Persist the HF_HOME mode + restart sidecars so the new value reaches
 *  the jobworkerp child via env. */
export async function setHfHome(req: SetHfHomeRequest): Promise<void> {
  return invoke<void>("set_hf_home", { request: req });
}

/** Pre-flight validation for a candidate data-root path. Returns a
 *  structured outcome with a localised message the UI can render inline. */
export async function validateDataRoot(path: string): Promise<DataRootValidation> {
  return invoke<DataRootValidation>("validate_data_root", { path });
}

/** Create the candidate data-root directory (recursive `mkdir -p`).
 *  Backend validates the parent is writable so a typo can't silently
 *  materialise garbage. Idempotent. */
export async function createDataRoot(path: string): Promise<void> {
  return invoke<void>("create_data_root", { path });
}

/** Curated local LLM presets shown in the Settings dropdown. Sourced from
 *  the Rust `llm_presets::PRESETS` constant; safe to cache (staleTime:
 *  Infinity) because the list only changes when the app binary changes. */
export async function listLlmPresets(): Promise<LlmPreset[]> {
  return invoke<LlmPreset[]>("list_llm_presets");
}

/** Curated embedding model presets (mirror of `listLlmPresets`). */
export async function listEmbeddingPresets(): Promise<EmbeddingPreset[]> {
  return invoke<EmbeddingPreset[]>("list_embedding_presets");
}

export async function getEmbeddingSettings(): Promise<EmbeddingSettingsResponse> {
  return invoke<EmbeddingSettingsResponse>("get_embedding_settings");
}

/** Save embedding settings. Triggers a sidecar restart and (when the
 *  vector dimension changes) the vectordb reset / backup pipeline. */
export async function setEmbeddingSettings(
  req: SetEmbeddingSettingsRequest,
): Promise<SetEmbeddingSettingsResponse> {
  return invoke<SetEmbeddingSettingsResponse>("set_embedding_settings", { req });
}

/** Batch-apply LLM / embedding / HF_HOME / MCP settings with a SINGLE
 *  sidecar restart. Prefer this over the individual set_* commands when
 *  several cards are dirty — each set_* restarts the sidecar on its own, so a
 *  multi-card save would otherwise restart (and re-download models) once
 *  per card. */
export async function applySettings(req: ApplySettingsRequest): Promise<ApplySettingsResponse> {
  return invoke<ApplySettingsResponse>("apply_settings", { req });
}

export async function getMcpSettings(): Promise<McpSettingsResponse> {
  return invoke<McpSettingsResponse>("get_mcp_settings");
}

/** Save MCP server settings. Triggers a sidecar restart (the `MCP_ENABLED`
 *  env is read at jobworkerp spawn time, so a toggle cannot be hot-reloaded).
 *  Funnels through the same `apply_settings` pipeline as the other cards. */
export async function setMcpSettings(req: SetMcpSettingsRequest): Promise<ApplySettingsResponse> {
  return invoke<ApplySettingsResponse>("set_mcp_settings", { req });
}

export async function getSetupStatus(): Promise<SetupStatus> {
  return invoke<SetupStatus>("get_setup_status");
}

export async function applySetup(req: ApplySetupRequest): Promise<ApplySetupResponse> {
  return invoke<ApplySetupResponse>("apply_setup", { req });
}

export async function resumeSetup(): Promise<void> {
  return invoke("resume_setup");
}

export async function restartForSetup(): Promise<void> {
  return invoke("restart_for_setup");
}

/** Read the tail of a sidecar log file (default 64 KiB). */
export async function readSidecarLog(
  source: LogSource,
  stream: LogStream,
  maxBytes?: number,
): Promise<LogTail> {
  return invoke<LogTail>("read_sidecar_log", { source, stream, maxBytes });
}

/**
 * Parse a JSON object payload to a typed shape. Returns `fallback()` when
 * the body is malformed (legacy plain-text rows / corrupted writes). Used
 * for `content_json` columns whose schema lives in upstream workflow YAML.
 */
function parseJsonObject<T extends object>(json: string, fallback: () => T): T {
  try {
    const parsed = JSON.parse(json);
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      return parsed as T;
    }
  } catch {
    // fall through
  }
  return fallback();
}

/**
 * Parse a summary memory's content body into a key-order-preserving object.
 * The keys vary per summary category (English schema keys or the Japanese
 * per-category labels the LLM emits), so the UI renders them generically.
 * `parsed` is null for legacy plain-text / non-object bodies; `raw` always
 * holds the original text as a fallback the UI can surface.
 */
export function parseSummaryContent(entry: { content_json: string }): SummaryContent {
  let parsed: SummaryContent["parsed"] = null;
  try {
    const value = JSON.parse(entry.content_json);
    if (value && typeof value === "object" && !Array.isArray(value)) {
      parsed = value;
    }
  } catch {
    // legacy plain-text body: leave `parsed` null, surface `raw` instead.
  }
  return { parsed, raw: entry.content_json };
}

export function parsePersonalityContent(profile: {
  content_json: string;
}): PersonalityProfileContent {
  return parseJsonObject<PersonalityProfileContent>(profile.content_json, () => ({}));
}

export function parsePersonalitySignalContent(signal: {
  content_json: string;
}): PersonalitySignalContent {
  return parseJsonObject<PersonalitySignalContent>(signal.content_json, () => ({}));
}

// ---- Sidecar recovery commands -----------------------------------------
// Invoked from the BootError screen when a structured `sidecar://error`
// arrives. Each command rewrites local state (or only opens / quits)
// then re-runs the standard startup pipeline; the returned
// `RecoveryResult` tells the UI whether to keep showing BootError
// (restart still failed) or wait for the upcoming `sidecar://ready`.

/** Rename the existing lancedb tree to a timestamped backup directory
 * and restart the sidecars. Use when the user wants the dimension
 * mismatch fixed without losing the existing vectors. */
export function recoverEvacuateLancedb(): Promise<RecoveryResult> {
  return invoke<RecoveryResult>("recover_evacuate_lancedb");
}

/** Delete the existing lancedb tree and restart. Last resort when disk
 * pressure makes the backup-rename infeasible. */
export function recoverPurgeLancedb(): Promise<RecoveryResult> {
  return invoke<RecoveryResult>("recover_purge_lancedb");
}

/** Reset a stored `preset_id` that is no longer in the curated list
 * (typically after a release that retired a preset) to `null`, then
 * restart. Does NOT touch lancedb. */
export function recoverResetEmbeddingSettings(): Promise<RecoveryResult> {
  return invoke<RecoveryResult>("recover_reset_embedding_settings");
}

/** Open the log directory in the OS file browser. Escape hatch on
 * BootError for failures that need manual investigation. */
export function openLogDir(): Promise<void> {
  return invoke<void>("open_log_dir");
}

/** Cleanly stop the sidecars and quit the app. */
export function quitApp(): Promise<void> {
  return invoke<void>("quit_app");
}
