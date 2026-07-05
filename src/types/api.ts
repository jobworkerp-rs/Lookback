// Types shared between the Tauri Rust core (commands::*) and the React UI.
// Keep field names in sync with `src-tauri/src/commands/*.rs`.

export type ImportSource = "claude-code" | "codex" | "plain";

/** How `memories-import plain` groups discovered files into threads. */
export type ThreadStrategy = "per-file" | "per-dir" | "single";

/** Plain-source parameters. Required when `sources` contains `"plain"`. */
export interface PlainImportConfig {
  /** Root directory walked recursively by the plain importer. */
  root: string;
  /** Channel / external_id namespace prefix. Omit to let the CLI default to
   *  `plain`; when set it must match `^[a-z0-9_-]{1,32}$`. */
  source_name?: string;
  thread_strategy: ThreadStrategy;
}

// memories' thread/memory ids are i64 snowflakes that exceed
// Number.MAX_SAFE_INTEGER; they are serialized as strings across the IPC
// boundary (see src-tauri/src/serde_id.rs).
export interface ThreadSummary {
  id: string;
  user_id: string;
  description: string | null;
  channel: string | null;
  labels: string[];
  created_at_ms: number;
  updated_at_ms: number;
}

export interface MemoryRow {
  id: string;
  role: number;
  content_type: number;
  content: string;
  created_at_ms: number;
  metadata?: string | null;
  external_id?: string | null;
}

export type LabelMatch = "any" | "all";

export interface ListThreadsRequest {
  user_id?: number;
  limit?: number;
  offset?: number;
  created_after_ms?: number;
  created_before_ms?: number;
  labels_any?: string[];
  label_match?: LabelMatch;
}

export interface FindDistinctLabelsRequest {
  user_id?: number;
  limit?: number;
  offset?: number;
  created_after_ms?: number;
  created_before_ms?: number;
}

export interface FindCoOccurringLabelsRequest {
  user_id?: number;
  labels: string[];
  limit?: number;
  offset?: number;
  created_after_ms?: number;
  created_before_ms?: number;
}

export interface LabelWithCount {
  label: string;
  thread_count: number;
}

export interface FindMemoriesRequest {
  thread_id: string;
  limit?: number;
  offset?: number;
}

export interface FindMemoryPositionRequest {
  thread_id: string;
  memory_id: string;
}

export interface FindMemoryThreadPositionRequest {
  memory_id: string;
}

/** Thread-internal position of a memory + the thread's total, for seeding
 *  ThreadDetail's scroll-to-hit when following a `memory_ids` cross-link. */
export interface MemoryPosition {
  position: number;
  thread_total: number;
}

export interface MemoryThreadPosition extends MemoryPosition {
  thread_id: string;
}

/** Summary granularity. `per-thread` is one summary per imported thread;
 *  daily/weekly/monthly are the periodic work-summary rollups. Kept in
 *  manual correspondence with the Rust `SummaryKind` (kebab-case serde). */
export type SummaryKind = "per-thread" | "daily" | "weekly" | "monthly";

export type GeneratedRefreshScope =
  | "thread_summary"
  | "daily_summary"
  | "weekly_summary"
  | "monthly_summary"
  | "personality"
  | "reflection";

export interface GeneratedRefreshEvent {
  job_id: string;
  scopes: GeneratedRefreshScope[];
}

export type PeriodicTaskKind = "regular" | "weekly" | "monthly";

export interface PeriodicTaskArgs {
  name: string;
  source: string;
  sources?: string[];
  task_kind: PeriodicTaskKind;
  hour: number;
  minute: number;
  interval_hours: number | null;
  interval_days: number | null;
  weekly_day: number | null;
  monthly_day: number | null;
  lookback_days: number;
  force_thread_summary: boolean;
  run_summary_daily?: boolean;
  run_personality?: boolean;
  run_reflection?: boolean;
}

export interface PeriodicTaskEntry {
  id: string;
  name: string;
  enabled: boolean;
  crontab: string;
  description: string | null;
  task: PeriodicTaskArgs | null;
  status: "supported" | "unsupported";
}

export interface ListPeriodicTasksRequest {
  limit?: number;
  offset?: number;
}

export interface SavePeriodicTaskRequest {
  id?: string | null;
  task: PeriodicTaskArgs;
  enabled: boolean;
  description?: string | null;
}

/** Resolved runtime status of a periodic execution. Mirrors the Rust
 *  `PeriodicExecutionStatus` (snake_case serde). `not_started` is synthesized
 *  when the scheduler has never run; `unavailable` when status can't be
 *  resolved. */
export type PeriodicExecutionStatus =
  | "pending"
  | "running"
  | "wait_result"
  | "cancelling"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "unknown"
  | "unavailable"
  | "enqueue_failed"
  | "not_started";

/** Which signal conductor used to resolve the status. */
export type PeriodicExecutionStatusSource =
  | "job_processing_status"
  | "job_result"
  | "execution_ref"
  | "unavailable"
  | "unspecified";

/** A periodic execution that has a resolved `ExecutionRuntimeStatus`. Times are
 *  epoch milliseconds (converted from conductor's epoch seconds). */
export interface PeriodicExecutionRuntime {
  execution_ref_id: string;
  scheduler_id: string;
  scheduler_name: string;
  job_id: string | null;
  status: PeriodicExecutionStatus;
  status_source: PeriodicExecutionStatusSource;
  triggered_at_ms: number;
  observed_at_ms: number | null;
  detail: string | null;
  enqueue_error: string | null;
  active: boolean;
  cancelable: boolean;
}

/** Per-scheduler status summary for the list cards. `runtime` is null for
 *  `not_started` / `unavailable`; `active` / `cancelable` are always present. */
export interface PeriodicExecutionSummary {
  scheduler_id: string;
  status: PeriodicExecutionStatus;
  runtime: PeriodicExecutionRuntime | null;
  active: boolean;
  cancelable: boolean;
  error: string | null;
}

/** One row in a scheduler's execution history. */
export interface PeriodicExecutionHistoryEntry {
  execution_ref_id: string;
  scheduler_id: string;
  scheduler_name: string;
  job_id: string | null;
  status: PeriodicExecutionStatus;
  status_source: PeriodicExecutionStatusSource;
  triggered_at_ms: number;
  observed_at_ms: number | null;
  detail: string | null;
  enqueue_error: string | null;
  active: boolean;
  cancelable: boolean;
  trigger_context_json: string | null;
  created_at_ms: number;
}

export interface SummaryEntry {
  memory_id: string;
  thread_id: string | null;
  external_id: string | null;
  kind: SummaryKind;
  /** `2026-05-24` / `2026-W21` / `2026-05`; null for per-thread. */
  period_key: string | null;
  /** Project/team scope (`_all` by default); null for per-thread. */
  scope_key: string | null;
  content_json: string;
  updated_at_ms: number;
}

export interface ListSummariesRequest {
  /** Defaults to `per-thread` server-side when omitted. */
  kind?: SummaryKind;
  limit?: number;
  offset?: number;
  created_after_ms?: number;
  created_before_ms?: number;
  updated_after_ms?: number;
  updated_before_ms?: number;
}

export interface CountSummariesRequest {
  kind?: SummaryKind;
}

/** Returned by `resolve_summary_memory_ref`. Carries the navigation
 *  coordinates derived from the cited summary memory's `external_id`. All
 *  fields are nullable because the memory may have been deleted (null
 *  response) or carry an external_id that doesn't match a summary prefix. */
export interface ResolvedSummaryMemoryRef {
  memory_id: string;
  thread_id: string | null;
  external_id: string | null;
  kind: SummaryKind | null;
  period_key: string | null;
  scope_key: string | null;
}

export interface ListSummaryPeriodKeysRequest {
  kind: SummaryKind;
  updated_after_ms?: number;
  updated_before_ms?: number;
}

export interface StartImportRequest {
  sources: ImportSource[];
  since?: string;
  user_id?: number;
  dry_run: boolean;
  labels: string[];
  memories_import_bin: string;
  /** Post-import generation toggles. All-true reproduces the legacy
   *  "run everything" behaviour; an unselected step is skipped server-side. */
  run_summary: boolean;
  run_personality: boolean;
  run_reflection: boolean;
  /** Cancel-key. The frontend generates a UUID and the toast's Stop
   *  button forwards it to `start_import_cancel`. Optional only so the
   *  Rust side still accepts older requests in tests; production
   *  callers always pass it. */
  dispatch_id?: string;
  /** Plain-source parameters. Sent only when `"plain"` is among `sources`. */
  plain?: PlainImportConfig;
}

export interface StartImportResponse {
  job_id: string;
}

export type ImportStep = "thread-import" | "thread-summary" | "thread-personality" | "reflection";

export type StepStatus = "waiting" | "active" | "done" | "warning" | "failed";

export interface ImportStepUpdate {
  job_id: string;
  step: ImportStep;
  status: StepStatus;
  message: string | null;
}

export interface SidecarEndpoints {
  jobworkerp_port: number;
  memories_port: number;
  conductor_port: number;
  /**
   * MCP HTTP server bind port when the user enabled it (and the sidecar is
   * up), else `null`. jobworkerp boots the MCP server in the same process as
   * the gRPC front, so this is not a separate child — it is the port the MCP
   * server bound to, surfaced so the UI can print the external-client URL.
   */
  mcp_server_port: number | null;
}

/**
 * Non-fatal startup conditions surfaced by the Rust core via
 * `sidecar://ready` so the UI can disable LLM-tagged flows when the
 * underlying runner registration failed.
 */
export type SidecarWarningKind =
  | "worker_apply_failed"
  | "plugins_stage_failed"
  /**
   * The local LanceDB could not be opened at the configured embedding
   * dimension, so the memories sidecar was restarted with the vector store
   * disabled (degraded mode). Startup is allowed; embedding-dependent
   * features (semantic / hybrid / intent search, import, generation) are
   * unavailable until the user switches the embedding model back to the
   * matching dimension. `SidecarWarning.detail` carries a JSON blob with
   * `reason` / `expected_dim` / `actual_dim` (see `isVectorDegraded`).
   */
  | "vector_store_degraded";

export interface SidecarWarning {
  kind: SidecarWarningKind;
  message: string;
  detail: string | null;
}

/** Payload of `sidecar://ready`. */
export interface SidecarStartReport extends SidecarEndpoints {
  warnings: SidecarWarning[];
}

/**
 * Returned by the `get_sidecar_status` command. Carries whichever of
 * the two one-shot lifecycle events (`sidecar://ready` /
 * `sidecar://error`) most recently fired, so a React tree that mounts
 * after the event already fired can still reach the matching UI state
 * via the snapshot. Both fields are `null` while a fresh start is still
 * in flight (the boot spinner stays).
 */
export interface SidecarStatusSnapshot {
  ready: SidecarStartReport | null;
  failure: SidecarErrorPayload | null;
}

/**
 * Structured startup-failure codes the memories sidecar emits via
 * `tracing::error!(target = "app::startup_error", code = ...)`. Mirror
 * of the Rust-side `StartupFailureCode` discriminator. Pinned: the set
 * of strings is part of the agent-app ↔ memories contract — adding /
 * renaming a value requires a coordinated change in both crates and a
 * matching entry in `BootError`'s recovery table.
 */
export type StartupFailureCode =
  | "lancedb_schema_mismatch"
  | "lancedb_init_failed"
  | "embedding_dimension_mismatch"
  | "media_config_conflict"
  | "rdb_pool_init_failed"
  | "env_var_invalid"
  | "config_load_failed"
  | "other";

/**
 * Discriminated union mirroring `StartupFailure` on the Rust side. The
 * structured fields are what the BootError UI reads to render
 * recovery-actionable messages (`expected_dim` / `actual_dim` etc.);
 * memories' human-readable `message` field is intentionally NOT
 * exposed — wording is owned by the frontend so a memories rephrase
 * never silently breaks the UI.
 */
export type StartupFailure =
  | {
      code: "lancedb_schema_mismatch";
      table: string;
      uri: string;
      expected_dim: number;
      actual_dim: number;
      expected_fingerprint: string;
      actual_fingerprint: string;
    }
  | { code: "lancedb_init_failed"; uri: string; message: string }
  | {
      code: "embedding_dimension_mismatch";
      expected_dim: number;
      actual_dim: number;
      runner_name: string;
    }
  | { code: "media_config_conflict"; backend: string; image_search_mode: string }
  | { code: "rdb_pool_init_failed"; url_sanitized: string; message: string }
  | { code: "env_var_invalid"; name: string; message: string }
  | { code: "config_load_failed"; component: string; message: string }
  | { code: "other"; component: string; message: string };

/**
 * Payload of `sidecar://error`. The Rust side lifts an `AppError` into
 * this tagged union via `SidecarErrorPayload::from_app_error`: only the
 * `SidecarStartupFailed` AppError variant carries a structured failure,
 * everything else collapses to a raw message. The BootError UI branches
 * on `kind` (then on `failure.code`) — it must NOT parse the human
 * `message` text.
 */
export type SidecarErrorPayload =
  | { kind: "structured"; failure: StartupFailure }
  | { kind: "raw"; message: string };

/** Response of every `recover_*` command. */
export interface RecoveryResult {
  restarted: boolean;
  backupPath: string | null;
  restartError: string | null;
}

export interface SettingsSnapshot {
  data_root: string;
  sqlite_path: string;
  lancedb_path: string;
  plugins_path: string;
  models_path: string;
  log_path: string;
  jobworkerp_url: string | null;
  memories_url: string | null;
}

/** Outcome of `purge_all_data`. The data root deletion always succeeds (or
 *  returns Err); secondary cleanups that live outside the data root —
 *  currently the macOS Keychain entry for the external LLM API key — are
 *  best-effort and surface as warnings so the UI can tell the user to clean
 *  them up manually. */
export interface PurgeReport {
  warnings: string[];
}

/** FR-CONFIG-5: model preparation state (3 states only). */
export type ModelState = "preparing" | "ready" | "failed";

export interface ModelStatus {
  state: ModelState;
  error: string | null;
  /** Configured model identity, resolved by Rust from the worker YAML (+ env
   *  override for the LLM). The UI shows this instead of a hardcoded name so it
   *  follows model swaps. Null only when the YAML couldn't be read. */
  name: string | null;
  repo: string | null;
}

/** Readiness of both local models: the LLM (generation) and the embedding
 *  model (search). Each is fetched lazily on first use and reported separately. */
export interface ModelStatusReport {
  llm: ModelStatus;
  embedding: ModelStatus;
}

// ====================================================================
// LLM provider settings (local vs external)
// ====================================================================

export type LlmMode = "local" | "external";

/** Non-secret LLM provider config. `api_key_set` indicates whether a key
 *  is stored in the OS keychain; the actual key is never sent to the frontend.
 *  `local_*` drive the local LLM model selection (preset / custom). */
export interface LlmSettingsResponse {
  mode: LlmMode;
  provider_model: string | null;
  api_key_set: boolean;
  base_url: string | null;
  max_tokens: number | null;
  temperature: number | null;
  local_preset_id: string | null;
  local_model_file: string | null;
  local_hf_repo: string | null;
  local_ctx_size: number | null;
  local_kv_cache_type: KvCacheType | null;
}

/** Request to update LLM settings. `api_key = null` means "don't change";
 *  an empty string deletes the stored key. */
export interface SetLlmSettingsRequest {
  mode: LlmMode;
  provider_model: string | null;
  api_key: string | null;
  base_url: string | null;
  max_tokens: number | null;
  temperature: number | null;
  local_preset_id: string | null;
  local_model_file: string | null;
  local_hf_repo: string | null;
  local_ctx_size: number | null;
  local_kv_cache_type: KvCacheType | null;
}

export type KvCacheType = "Q4_0" | "Q4_1" | "IQ4_NL" | "Q5_0" | "Q5_1" | "Q8_0";

/** Per-preset policy for `chat_template_kwargs.enable_thinking`. Mirrors
 *  the Rust `llm_presets::ThinkingKwarg` enum (serde rename_all snake_case). */
export type ThinkingKwarg = "none" | "disable" | "enable";

/** Curated local LLM preset surfaced in the Settings dropdown. Sourced
 *  from the Rust `llm_presets::PRESETS` constant via `list_llm_presets`. */
export interface LlmPreset {
  id: string;
  display_name: string;
  hf_repo: string;
  gguf_file: string;
  recommended_ctx_size: number;
  min_ctx_size: number;
  estimated_model_ram_gb: number;
  estimated_ram_gb: number;
  kv_layers: number;
  kv_embd_k_gqa: number;
  kv_embd_v_gqa: number;
  /** An i18n key (`settings.llmPreset.desc.<id>`), not a localized string —
   *  resolve via `t()` so language switching is instant. */
  description: string;
  thinking_kwarg: ThinkingKwarg;
}

/** Sentinel preset id for "free-text custom entry". Matches
 *  `llm_presets::CUSTOM_PRESET_ID` on the Rust side. */
export const CUSTOM_LLM_PRESET_ID = "custom";

// ====================================================================
// Embedding model settings (preset / custom + sidecar restart)
// ====================================================================

/** Curated embedding model preset. Mirrors the Rust
 *  `embedding_presets::EmbeddingPreset` struct. */
export interface EmbeddingPreset {
  id: string;
  display_name: string;
  hf_repo: string;
  tokenizer_hf_repo: string | null;
  vector_size: number;
  dtype: string;
  max_sequence_length: number;
  is_multimodal: boolean;
  estimated_ram_gb: number;
  /** An i18n key (`settings.embeddingPreset.desc.<id>`), not a localized
   *  string — resolve via `t()` so language switching is instant. */
  description: string;
}

/** Sentinel preset id for the Settings UI's "Custom" row. */
export const CUSTOM_EMBEDDING_PRESET_ID = "custom";

/** Resolved embedding-runner values — what the sidecar will use.
 *  Note: `device` is intentionally absent; the agent-app distribution
 *  pins it to the platform-supported embedding runner backend, so there
 *  is nothing to display or vary per preset. */
export interface EmbeddingRuntime {
  model_id: string;
  tokenizer_id: string | null;
  vector_size: number;
  dtype: string;
  max_sequence_length: number;
  is_multimodal: boolean;
}

/** Frontend view of `embedding-settings.json` plus derived flags. */
export interface EmbeddingSettingsResponse {
  preset_id: string | null;
  custom_model_id: string | null;
  custom_tokenizer_id: string | null;
  custom_vector_size: number | null;
  custom_dtype: string | null;
  custom_max_sequence_length: number | null;
  custom_is_multimodal: boolean | null;
  effective: EmbeddingRuntime;
  /** When true the embedding card must disable inputs — the user is
   *  pointed at a remote memories and the swap would be a no-op. */
  connection_remote: boolean;
}

export interface SetEmbeddingSettingsRequest {
  preset_id: string | null;
  custom_model_id: string | null;
  custom_tokenizer_id: string | null;
  custom_vector_size: number | null;
  custom_dtype: string | null;
  custom_max_sequence_length: number | null;
  custom_is_multimodal: boolean | null;
  /** `true` = rename existing lancedb into `<root>/lancedb-backup/`;
   *  `false` = delete it outright. */
  evacuate_vectordb: boolean;
}

export interface SetEmbeddingSettingsResponse {
  runtime: EmbeddingRuntime;
  /** Filesystem path of the renamed backup, when one was created. */
  backup_path: string | null;
  restarted: boolean;
}

// ====================================================================
// MCP server settings (expose RAG retrieval as MCP tools)
// ====================================================================

/** Frontend view of `mcp-settings.json` plus derived flags. Mirrors the
 *  Rust `commands::mcp_settings::McpSettingsResponse`. */
export interface McpSettingsResponse {
  enabled: boolean;
  /** Advanced overrides. `null` ⇒ jobworkerp default is used. */
  exclude_runner_as_tool: boolean | null;
  exclude_worker_as_tool: boolean | null;
  streaming: boolean | null;
  request_timeout_sec: number | null;
  /** FunctionSet name exposed over MCP (constant `lookback-mcp-rag`). */
  set_name: string;
  /** The MCP server's bound port while it is running, else `null`. */
  active_port: number | null;
}

export interface SetMcpSettingsRequest {
  enabled: boolean;
  exclude_runner_as_tool: boolean | null;
  exclude_worker_as_tool: boolean | null;
  streaming: boolean | null;
  request_timeout_sec: number | null;
}

// ====================================================================
// App settings (App data dir override + HF_HOME mode)
// ====================================================================

/** HF cache root policy. Mirrors `data::paths::HfHomeMode` (snake_case
 *  serde). `data_root` = `<app data dir>/models`, `global` = `$HF_HOME` or
 *  `~/.cache/huggingface`, `custom` = the user-supplied `hf_home_path`. */
export type HfHomeMode = "data_root" | "global" | "custom";

export interface ResolvedAppPaths {
  current_data_root: string;
  default_data_root: string;
  effective_hf_home: string;
  /** Where the NEXT launch will actually resolve the data root to —
   *  applies the same validity gate as the Rust `DataPaths::resolve`
   *  (absolute + currently-existing directory). A dangling override
   *  (e.g. external disk unplugged, target purged) is silently
   *  replaced with `default_data_root` here so the UI reports what the
   *  next boot will actually use, not what the user typed. */
  pending_data_root: string;
}

export interface AppSettingsResponse {
  hf_home_mode: HfHomeMode;
  hf_home_path: string | null;
  /** `null` = use the OS default (current behaviour). */
  data_root_override: string | null;
  resolved: ResolvedAppPaths;
}

export interface SetHfHomeRequest {
  mode: HfHomeMode;
  path: string | null;
}

// ====================================================================
// Unified settings apply (batch save + single sidecar restart)
// ====================================================================

/** Batch-apply request. Each present field is persisted; absent fields are
 *  left untouched. The sidecar restarts at most ONCE for the whole batch
 *  (Rust `apply_settings` re-reads all three files on start). */
export interface ApplySettingsRequest {
  llm: SetLlmSettingsRequest | null;
  embedding: SetEmbeddingSettingsRequest | null;
  hf_home: SetHfHomeRequest | null;
  mcp: SetMcpSettingsRequest | null;
}

export interface ApplySettingsResponse {
  /** `false` ⇒ nothing meaningful changed; the sidecar was left running. */
  restarted: boolean;
  /** Set when an embedding dimension change evacuated the lancedb. */
  backup_path: string | null;
  /** The embedding runtime now in effect (only when embedding was part of
   *  the batch). */
  embedding_runtime: EmbeddingRuntime | null;
  /** Plugin-staging warnings surfaced by the restart, if any. */
  warnings: SidecarWarning[];
}

export interface SetupStatus {
  required: boolean;
  resume_apply: boolean;
  current_data_root: string;
  default_data_root: string;
}

export interface ApplySetupRequest {
  data_root: string | null;
  settings: ApplySettingsRequest;
}

export interface ApplySetupResponse {
  restart_required: boolean;
}

export interface DataRootValidation {
  ok: boolean;
  writable: boolean;
  is_existing_lookback_root: boolean;
  /** True when the path is absolute, doesn't exist yet, and its parent
   *  directory is writable — the UI surfaces a "create" button only in
   *  this case so a wildly wrong path can't silently materialise. */
  creatable: boolean;
  /** An i18n key (e.g. `settings.dataRoot.validation.notExist`), not a
   *  localized string — resolve via `t()` so language switching is instant. */
  message: string | null;
}

/** FR-CONFIG-1: connection-target override (local sidecar vs remote server). */
export type ConnectionMode = "local" | "remote";

export interface ConnectionConfig {
  mode: ConnectionMode;
  remote_jobworkerp_url: string | null;
  remote_memories_url: string | null;
}

export interface ConnectionTestReport {
  jobworkerp_url: string;
  memories_url: string;
}

/** NFR-6: which log file to read. `app` is Lookback's own Rust-side log
 * (carries the memories-import child output); the rest are sidecar logs. */
export type LogSource = "jobworkerp" | "memories" | "app";
export type LogStream = "stdout" | "stderr";

export interface LogTail {
  file_name: string;
  content: string;
  /** True when the file was larger than the requested tail size. */
  truncated: boolean;
  file_size: number;
}

/**
 * Parsed shape of a summary memory's `content` JSON. The summary schema is
 * authored in `workers/workflows/thread-summary/thread-summary-single.yaml`,
 * but the LLM is free to vary the keys per category (e.g. coding summaries use
 * 目的/実施内容/…, research summaries use 調査テーマ/発見事項/…). The UI must
 * therefore render arbitrary keys generically rather than a fixed field set.
 *
 * `parsed` is the JSON object as-is (key order preserved); `raw` is the
 * original text, kept so genuinely unparseable bodies can still be surfaced.
 */
export interface SummaryContent {
  parsed: Record<string, SummaryValue> | null;
  raw: string;
}

/** A value of a summary field: a scalar, a list of scalars, or a nested
 *  object. Rendered structurally by the Summaries page. */
export type SummaryValue =
  | string
  | number
  | boolean
  | null
  | SummaryValue[]
  | {
      [key: string]: SummaryValue;
    };

// ====================================================================
// Reflections (FR-REF)
// ====================================================================

/** Trimmed projection of `llm_memory.data.Reflection`. */
export interface ReflectionEntry {
  id: string;
  origin_thread_id: string;
  summary: string;
  task_intent: string;
  task_category: number;
  reflection_aspect: number;
  outcome: number;
  score: number;
  score_self: number;
  score_heuristic: number;
  lessons: string[];
  key_decisions: string[];
  success_factors: string[];
  failure_modes: number[];
  mitigation_hint: string | null;
  pinned: boolean;
  prompt_version: string;
  intent_embedding_status: number;
  created_at_ms: number;
  updated_at_ms: number;
}

export interface ListReflectionsByThreadRequest {
  thread_id: string;
  include_history?: boolean;
}

export interface SearchReflectionsRequest {
  query_text?: string;
  user_id?: number;
  outcomes?: number[];
  created_after_ms?: number;
  created_before_ms?: number;
  limit?: number;
}

/** Natural-language intent search over reflections (FindSimilarByIntentText).
 *  The server embeds `intent_text` itself — no client vector required. */
export interface SearchReflectionsByIntentRequest {
  intent_text: string;
  top_k?: number;
  user_id?: number;
  outcomes?: number[];
  created_after_ms?: number;
  created_before_ms?: number;
}

/** Intent-vector index health for the Reflections natural-language search.
 *  `records_without_embedding > 0` means some reflections won't surface in
 *  intent search yet. `vector_dimension === 0` signals the intent vector table
 *  hasn't been created (sidecar not up / REFLECTION_INTENT_VECTOR_ENABLED unset). */
export interface ReflectionIntentIndexStats {
  total_records: number;
  records_with_embedding: number;
  records_without_embedding: number;
  vector_dimension: number;
}

/** Backfill intent (and/or summary) embeddings for existing reflections.
 *  All fields optional: `kind` defaults to INTENT(2), filters default to the
 *  single local user (origin_user_id=1) / no period. */
export interface RedispatchReflectionEmbeddingsRequest {
  kind?: number;
  user_id?: number;
  outcomes?: number[];
  created_after_ms?: number;
  created_before_ms?: number;
  batch_size?: number;
}

export interface RedispatchEmbeddingsResult {
  dispatched_count: number;
  skipped_count: number;
  failed_count: number;
  duration_ms: number;
}

/** Memory (summary / thread) embedding index health. `vector_dimension === 0`
 *  signals the memory_vector LanceDB table is missing (sidecar not up or
 *  MEMORY_VECTOR_ENABLED unset). Distinct from the Reflection intent index
 *  — semantic/hybrid/natural-language search reads this one. */
export interface MemoryEmbeddingStats {
  total_records: number;
  records_with_embedding: number;
  records_without_embedding: number;
  vector_dimension: number;
}

/** Redispatch every memory's embedding job. The backend
 *  (MemoryVectorService.RedispatchEmbeddings) scans the RDB and is idempotent
 *  — there is no "missing only" vs "force" distinction at the proto level,
 *  so this maps 1:1 to the UI's single "再生成" button. Filter fields are
 *  reserved for a future scoped backfill (currently unused by the card). */
export interface RedispatchMemoryEmbeddingsRequest {
  user_id?: number;
  thread_id?: number;
  batch_size?: number;
}

export interface EnqueueReflectionJobRequest {
  user_id?: number;
  updated_after_ms?: number;
  prompt_version?: string;
  /** Cancel-key. The frontend generates a UUID and the Stop button
   *  forwards it to `reflection_cancel`. */
  dispatch_id?: string;
}

export interface EnqueueReflectionJobResponse {
  job_id_hint: string;
}

/** Emitted by the Rust side as the reflection workflow streams. */
export type ReflectionStepStatus = Exclude<StepStatus, "waiting">;

export interface ReflectionStepUpdate {
  job_id: string;
  status: ReflectionStepStatus;
  message: string | null;
}

// ====================================================================
// Standalone summary / personality dispatch (run analysis later)
// ====================================================================

/** Mirrors the reflection manual-dispatch request shape. */
export interface EnqueueSummaryJobRequest {
  user_id?: number;
  updated_after_ms?: number;
  /** Inclusive epoch-ms upper bound; omit for no upper bound. */
  updated_before_ms?: number;
  /** Cancel-key. The backend uses this verbatim as the in-flight map
   *  key, so a Stop click that calls `analysis_cancel(dispatch_id)`
   *  hits the same entry. */
  dispatch_id?: string;
}

export interface EnqueuePersonalityJobRequest {
  user_id?: number;
  updated_after_ms?: number;
  /** When true, re-run extraction on every eligible thread (ignores the
   *  per-thread `existing_signal` skip and the batch's `target_signal_count`
   *  short-circuit). Used by the Personality tab's Force checkbox so a
   *  prompt change can be applied to the historical thread set. */
  force_reextract?: boolean;
  dispatch_id?: string;
}

/** Request shape for the standalone Layer-2 merge dispatch. The merge YAML
 *  short-circuits when `max(signal.updated_at) <= profile.updated_at`; the
 *  Force checkbox on the Personality tab sets `force_remerge: true` so the
 *  same source signals can produce a fresh profile (e.g. after a prompt
 *  change, or when the previous merge step failed and the source threads'
 *  timestamps haven't moved). */
export interface EnqueuePersonalityMergeJobRequest {
  user_id?: number;
  force_remerge?: boolean;
  dispatch_id?: string;
}

/** Period-summary granularity for generation. Matches the Rust `PeriodKind`
 *  (kebab-case serde) and is the non-per-thread subset of `SummaryKind`. */
export type PeriodKind = Exclude<SummaryKind, "per-thread">;

export interface EnqueuePeriodSummaryJobRequest {
  kind: PeriodKind;
  /** Back-fill the last N periods; omit to generate the last completed one. */
  last_n?: number;
  dispatch_id?: string;
}

/** Staged generate request: the dialog's checkbox selection + an already
 *  range-expanded set of per-layer inputs. Per-thread carries epoch-ms bounds;
 *  the period layers carry granularity tokens (computed on the frontend from
 *  date strings, so they're TZ/DST-independent). Mirrors the Rust
 *  `GenerateSummariesRequest`; empty token strings mean "no range". */
export interface GenerateSummariesRequest {
  user_id?: number;
  run_per_thread: boolean;
  run_daily: boolean;
  run_weekly: boolean;
  run_monthly: boolean;
  /** Per-thread window (epoch ms); omit both for unbounded. */
  updated_after_ms?: number;
  updated_before_ms?: number;
  daily_start: string;
  daily_end: string;
  weekly_start: string;
  weekly_end: string;
  monthly_start: string;
  monthly_end: string;
  /** Day-boundary tz for the period single workflows (range expansion ignores it). */
  timezone_offset_hours: number;
  dispatch_id?: string;
}

export interface EnqueueAnalysisJobResponse {
  job_id_hint: string;
}

/** Payload of `summary://step` / `personality://step`. Identical shape to the
 *  reflection stream (the Rust side never emits `waiting` for these). */
export type AnalysisStepUpdate = ReflectionStepUpdate;

// ====================================================================
// Personality (FR-PER)
// ====================================================================

export interface PersonalityProfile {
  /** Backing Memory row id (i64 snowflake as string); used to delete the profile. */
  memory_id: string;
  content_json: string;
  updated_at_ms: number;
  external_id: string;
}

export interface PersonalityResponse {
  profile: PersonalityProfile | null;
  thread_count: number;
  thread_count_truncated: boolean;
  /** Number of `personality_signal`-tagged threads = signal row count. */
  signal_count: number;
}

export interface GetPersonalityRequest {
  user_id?: number;
}

export interface ListPersonalitySignalsRequest {
  user_id?: number;
}

export interface DebugPersonalityInventoryRequest {
  user_id?: number;
}

/** Temporary investigation payload — see `debug_personality_inventory`. */
export interface PersonalityInventoryReport {
  personality_user_threads_total: number;
  personality_label_threads: number;
  signal_label_threads: number;
  signal_memories_total: number;
  signal_memories_with_metadata: number;
  signal_memories_no_signal_true: number;
  signal_memories_no_signal_false: number;
  signal_memories_no_signal_missing: number;
  sample_signal_payload: string | null;
  sample_signal_metadata: string | null;
  /** A separate sample slot for a no_signal=true row, so the panel can show
   *  both 'good' and 'no_signal' representative payloads side by side. */
  sample_no_signal_payload: string | null;
  sample_no_signal_metadata: string | null;
}

/**
 * One layer-1 personality signal, as returned by `list_personality_signals`.
 * `source_thread_id` is serialized as a string (i64 snowflake) like other ids.
 */
export interface PersonalitySignal {
  /** Backing Memory row id (i64 snowflake as string); used to delete the signal. */
  memory_id: string;
  source_thread_id: string;
  content_json: string;
  updated_at_ms: number;
}

/**
 * Parsed shape of a layer-1 signal's `content_json`. See
 * `agent-app/workers/workflows/personality/thread-personality-single.yaml`
 * (`json_schema`) for the authoritative schema. All fields optional because
 * the LLM emits only the categories it found.
 */
export interface PersonalitySignalContent {
  no_signal?: boolean;
  /** Why no usable signal was found; the LLM may also emit it alongside a
   *  thin no_signal:false payload, so the drawer surfaces it. */
  reason?: string;
  interests?: SignalInterest[];
  preferences?: SignalPreference[];
  decision_style?: SignalDecisionStyle;
  communication_style?: SignalCommunicationStyle;
  values_and_beliefs?: SignalBelief[];
  anti_preferences?: SignalAntiPreference[];
}

// Layer-1 signal entries come in TWO shapes. The json_schema in
// thread-personality-single.yaml prescribes per-category field names
// (topic / axis+preference / belief / avoid + summary/traits, tone/verbosity).
// But memories-llm does not strictly enforce the schema, and in practice the
// model emits a uniform `category` + `description` shape across every
// category. Both must be accepted, with the schema names taking precedence
// when present and `category`/`description` as the observed fallback.

// `memory_ids` are the source-thread memory ids the LLM cited as evidence
// (stringified i64, max 5 per entry). The drawer turns them into links that
// scroll to the memory inside its thread.

// Profile-only fields (see PersonalityProfileContent below): the layer-2
// merge reuses these per-category interfaces, adding `weight` (importance the
// merge LLM assigned), `supporting_source_thread_ids` (validated source
// threads), and the post-process-derived `first_seen_at`/`last_seen_at`.
// Optional so the layer-1 signal path, which never emits them, is unaffected.
interface ProfileEntryFields {
  weight?: string;
  supporting_threads?: number;
  supporting_source_thread_ids?: string[];
  first_seen_at?: string;
  last_seen_at?: string;
}

export interface SignalInterest extends ProfileEntryFields {
  topic?: string;
  category?: string;
  description?: string;
  confidence?: string;
  evidence?: string;
  memory_ids?: string[];
}

export interface SignalPreference extends ProfileEntryFields {
  axis?: string;
  preference?: string;
  category?: string;
  description?: string;
  confidence?: string;
  evidence?: string;
  memory_ids?: string[];
}

// Profile-only fields for the single-object categories (decision_style /
// communication_style). Unlike the list entries these carry no `weight`.
interface ProfileObjectFields {
  supporting_source_thread_ids?: string[];
  first_seen_at?: string;
  last_seen_at?: string;
}

export interface SignalDecisionStyle extends ProfileObjectFields {
  summary?: string;
  traits?: string[];
  description?: string;
  confidence?: string;
  memory_ids?: string[];
}

export interface SignalCommunicationStyle extends ProfileObjectFields {
  tone?: string;
  verbosity?: string;
  language_preference?: string;
  notes?: string;
  description?: string;
  confidence?: string;
  memory_ids?: string[];
}

export interface SignalBelief extends ProfileEntryFields {
  belief?: string;
  category?: string;
  description?: string;
  confidence?: string;
  evidence?: string;
  memory_ids?: string[];
}

export interface SignalAntiPreference extends ProfileEntryFields {
  avoid?: string;
  category?: string;
  description?: string;
  confidence?: string;
  evidence?: string;
  memory_ids?: string[];
}

/**
 * Parsed shape of `personality_profile:<user_id>` content. See
 * `agent-app/workers/workflows/personality/user-personality-merge.yaml`
 * for the authoritative schema (LLM json_schema + the `confirmEntryDates`
 * post-process jq). The list categories are arrays of objects and the two
 * style categories are objects — NOT the flat strings the UI once assumed.
 * The per-category interfaces are reused from the layer-1 signal shape
 * (which is a structural subset), so the formatter is shared.
 */
export interface PersonalityProfileContent {
  profile_version?: string;
  summary?: string;
  interests?: SignalInterest[];
  preferences?: SignalPreference[];
  decision_style?: SignalDecisionStyle;
  communication_style?: SignalCommunicationStyle;
  values_and_beliefs?: SignalBelief[];
  anti_preferences?: SignalAntiPreference[];
  metrics?: ProfileMetrics;
}

export interface ProfileMetrics {
  source_thread_count?: number;
  no_signal_thread_count?: number;
  earliest_signal_at?: string;
  latest_signal_at?: string;
}

// ====================================================================
// Search (FR-SEARCH)
// ====================================================================

export type SearchMode = "keyword" | "semantic" | "hybrid";

export interface SearchThreadsRequest {
  query_text: string;
  mode: SearchMode;
  user_id?: number;
  created_after_ms?: number;
  created_before_ms?: number;
  labels_any?: string[];
  label_match?: LabelMatch;
  limit?: number;
}

export interface ThreadHit {
  thread_id: string;
  thread_description: string | null;
  top_score: number;
  top_memory_id: string;
  top_snippet: string;
  top_position: number | null;
  top_thread_total: number | null;
  top_created_at_ms: number;
  hit_count: number;
}

// ====================================================================
// RAG chat (specs/rag-chat-spec.md FR-CHAT-1..9)
//
// Mirrors `src-tauri/src/commands/chat.rs`. Wire shape is hand-synced
// with the Rust serde structs — same convention as the rest of this
// file. Snowflake IDs travel as strings (serde_id) so JS numbers don't
// truncate.
// ====================================================================

export interface ChatMessage {
  role: "user" | "assistant" | "system";
  content: string;
}

export interface ChatAskRequest {
  messages: ChatMessage[];
  /** Correlation key chosen by the caller. Must be unique per chat
   *  request; the Rust side echoes it on every `chat://step` event so
   *  the UI can register a turn under this id BEFORE invoking the
   *  command (closing the early-event-drop race where a Start emitted
   *  synchronously during dispatch could hit an unregistered turn). */
  job_id: string;
}

export interface ChatAskResponse {
  /** Echo of `ChatAskRequest.job_id`. */
  job_id: string;
}

/** FR-CHAT-9 minimum-contract phases. Kebab-case mirrors the Rust
 *  `ChatPhase` serde rename. */
export type ChatPhase = "start" | "searching" | "source" | "token" | "done" | "error";

/** One citation entry. Discriminated by `source_kind`; the union
 *  enforces that only `period_summary` carries `period_key/scope_key`
 *  and only the other two carry `source_thread_id` (FR-CHAT-4b). */
export type ChatSource =
  | {
      source_kind: "raw_memory";
      memory_id: string;
      source_thread_id: string;
      snippet: string;
      score: number;
    }
  | {
      source_kind: "thread_summary";
      memory_id: string;
      source_thread_id: string;
      snippet: string;
      score: number;
    }
  | {
      source_kind: "period_summary";
      memory_id: string;
      period_key: string;
      scope_key: string;
      snippet: string;
      score: number;
    };

/** One `chat://step` event payload. Phase-irrelevant fields are
 *  optional and omitted on the wire (Rust side uses
 *  `skip_serializing_if = Option::is_none`). */
export interface ChatStepUpdate {
  job_id: string;
  phase: ChatPhase;
  token_delta?: string | null;
  sources?: ChatSource[] | null;
  message?: string | null;
}
