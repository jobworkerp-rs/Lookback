// Types shared between the Tauri Rust core (commands::*) and the React UI.
// Keep field names in sync with `src-tauri/src/commands/*.rs`.

export interface AppCapabilities {
  experimental_issues_enabled: boolean;
  issue_tracking_configurable: boolean;
}

export type ImportSource = "claude-code" | "codex" | "opencode" | "plain";

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

export interface SearchIndexMaintenanceTableStatus {
  write_generation: number;
  dirty: boolean;
  ready_runtime_secs: number;
  last_result: string | null;
}

export interface SearchIndexMaintenanceSchedule {
  enabled: boolean;
  start: string | null;
  end: string | null;
}

export interface ActiveSearchIndexOptimization {
  table: "memory" | "thread";
  task_id: string | null;
  state: "attempting" | "accepted";
}

export interface SearchIndexMaintenanceStatus {
  memory: SearchIndexMaintenanceTableStatus;
  thread: SearchIndexMaintenanceTableStatus;
  reconcile_pending: boolean;
  writing: boolean;
  optimizing: boolean;
  active_optimizations: ActiveSearchIndexOptimization[];
  memory_eligible: boolean;
  thread_eligible: boolean;
  recovery_required: string | null;
  schedule: SearchIndexMaintenanceSchedule;
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
  first_message_at_ms?: number;
  last_message_at_ms?: number;
}

export interface ListThreadGroupsRequest {
  limit?: number;
  offset?: number;
  include_inactive?: boolean;
  group_owner_user_id?: number;
  /** Legacy alias for group_owner_user_id. */
  user_id?: number;
}

export interface SearchThreadGroupsRequest {
  query_text: string;
  mode: SearchMode;
  group_owner_user_id?: number;
  member_user_id?: number;
  /** Legacy alias for group_owner_user_id. */
  user_id?: number;
  created_after_ms?: number;
  created_before_ms?: number;
  labels_any?: string[];
  label_match?: LabelMatch;
  allow_cross_member?: boolean;
  page_size?: number;
  page_token?: string;
}

export interface ThreadGroupThreadDisplay {
  thread_id: string | null;
  thread_canonical_key: string;
  description: string | null;
  source: string | null;
  created_at_ms: number | null;
  last_message_at_ms: number | null;
  deleted_at_ms: number | null;
}

export interface ThreadGroupListItem {
  id: string;
  user_id: number;
  group_canonical_key: string;
  title: string | null;
  display_name: string;
  status: string;
  grouping_authority: string;
  redirect_to_group_id: string | null;
  latest_activity_at_ms: number | null;
  root_thread_id: string | null;
  root_thread_canonical_key: string | null;
  root_display: ThreadGroupThreadDisplay | null;
  active_member_count: number;
  deleted_member_count: number;
  unresolved_count: number;
  membership_snapshot_digest: string;
}

export interface ThreadGroupMemberView {
  group_id: string;
  user_id: number;
  thread_id: string | null;
  thread_canonical_key: string;
  source: string | null;
  role: string;
  state: string;
  provenance: string;
  identity_scope: string | null;
  native_id: string | null;
  deleted_at_ms: number | null;
  display: ThreadGroupThreadDisplay | null;
}

export interface ThreadRelationView {
  id: string;
  parent_thread_id: string | null;
  child_thread_id: string | null;
  parent_thread_canonical_key: string;
  child_thread_canonical_key: string;
  parent_user_id: number;
  child_user_id: number;
  relation_type: string;
  state: string;
  selection_basis: string;
  source_confidence: string | null;
  selected_observation_id: string | null;
  selected_operator_decision_id: string | null;
  parent_group_id?: string | null;
  child_group_id?: string | null;
  parent_display?: ThreadGroupThreadDisplay | null;
  child_display?: ThreadGroupThreadDisplay | null;
}

export interface ThreadGroupUnresolvedView {
  id: string;
  subject_thread_id: string | null;
  subject_source: string;
  subject_user_id: number;
  candidate_group_id: string | null;
  candidate_parent_thread_id: string | null;
  state: string;
  subject_identity_scope_known: boolean;
  subject_identity_scope_value: string;
  subject_native_id: string;
  selected_observation_id: string | null;
}

export interface ThreadObservationView {
  id: string;
  subject_source: string;
  subject_user_id: number;
  subject_identity_scope_known: boolean;
  subject_identity_scope_value: string;
  subject_native_id: string;
  candidate_parent_present: boolean;
  candidate_parent_user_id: number | null;
  candidate_parent_source: string | null;
  candidate_parent_identity_scope_known: boolean;
  candidate_parent_identity_scope_value: string | null;
  candidate_parent_native_id: string | null;
  relation_kind: string | null;
  evidence_kind: string;
  polarity: string;
  source_confidence: string | null;
  state: string;
  adapter_version: string;
  source_record_ref: string;
  observed_at_ms: number;
}

export interface ThreadGroupLineageView {
  group: ThreadGroupListItem;
  members: ThreadGroupMemberView[];
  relations: ThreadRelationView[];
  unresolved: ThreadGroupUnresolvedView[];
  observations: ThreadObservationView[];
}

export interface ThreadGroupSummaryView {
  memory_id: string;
  external_id: string | null;
  content_json: string;
  metadata: string | null;
  updated_at_ms: number;
  /** Digest of the membership snapshot used to generate this summary. */
  membership_snapshot_digest?: string | null;
  group_id?: string | null;
  active_member_count?: number | null;
  deleted_member_count?: number | null;
  unresolved_count?: number | null;
  member_summary_count?: number | null;
  summary_version?: string | null;
  aggregation_mode?: string | null;
  passthrough_source_memory_id?: string | null;
}

/** One entry of `find_thread_group_summaries`, aligned with the requested ids. */
export interface ThreadGroupSummaryEntryView {
  group_id: string;
  summary: ThreadGroupSummaryView | null;
}

export interface ThreadGroupSearchWitnessView {
  predicate_index: number;
  target: string;
  thread_id: string;
  thread_canonical_key: string;
  score: number;
  member: ThreadGroupThreadDisplay | null;
}

export interface ThreadGroupSearchResultView {
  group: ThreadGroupListItem;
  relevance_score: number;
  witnesses: ThreadGroupSearchWitnessView[];
  root_member: ThreadGroupThreadDisplay | null;
  current_member_count: number;
  active_member_count: number;
  deleted_member_count: number;
}

export interface ThreadGroupSearchPage {
  results: ThreadGroupSearchResultView[];
  next_page_token: string | null;
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
  thread_created_after_ms?: number;
  thread_created_before_ms?: number;
  thread_updated_after_ms?: number;
  thread_updated_before_ms?: number;
  first_message_after_ms?: number;
  first_message_before_ms?: number;
  last_message_after_ms?: number;
  last_message_before_ms?: number;
  labels_any?: string[];
  label_match?: LabelMatch;
}

export interface FindDistinctLabelsRequest {
  user_id?: number;
  limit?: number;
  offset?: number;
  thread_created_after_ms?: number;
  thread_created_before_ms?: number;
  thread_updated_after_ms?: number;
  thread_updated_before_ms?: number;
  first_message_after_ms?: number;
  first_message_before_ms?: number;
  last_message_after_ms?: number;
  last_message_before_ms?: number;
}

export interface FindCoOccurringLabelsRequest {
  user_id?: number;
  labels: string[];
  limit?: number;
  offset?: number;
  thread_created_after_ms?: number;
  thread_created_before_ms?: number;
  thread_updated_after_ms?: number;
  thread_updated_before_ms?: number;
  first_message_after_ms?: number;
  first_message_before_ms?: number;
  last_message_after_ms?: number;
  last_message_before_ms?: number;
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

/** A Memory resolved for direct navigation from the Threads browse filter. */
export interface MemoryReference {
  memory: MemoryRow;
  locations: MemoryReferenceLocation[];
}

export interface MemoryReferenceLocation extends MemoryPosition {
  thread: ThreadSummary;
}

/** Summary granularity. `per-thread` is one summary per imported thread;
 *  daily/weekly/monthly are the periodic work-summary rollups. Kept in
 *  manual correspondence with the Rust `SummaryKind` (kebab-case serde). */
export type SummaryKind = "per-thread" | "thread-group" | "daily" | "weekly" | "monthly";

/** Kinds that can be attached to a chat selection. Per-thread summaries are
 *  internal generation inputs and are deliberately not a public selection
 *  kind. Keep this separate from SummaryKind because reflections are not
 *  summaries and are not valid input to summary-generation/calendar APIs. */
export type SelectableMemoryKind = Exclude<SummaryKind, "per-thread"> | "reflection";

export type GeneratedRefreshScope =
  | "thread_summary"
  | "thread_group_summary"
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
  /** Explicit ThreadGroup periodic opt-in; omitted by legacy scheduler rows. */
  run_summary_thread_group?: boolean;
  run_summary_daily?: boolean;
  run_personality?: boolean;
  run_reflection?: boolean;
  stop_on_error?: boolean;
}

export type SummaryRecoveryHealth = "healthy" | "corrupt" | "unsupported_version" | "unreadable";

export interface SummaryRecoveryRecord {
  memories_endpoint_id: string;
  owner_id: number;
  source_scope_digest: string;
  period_kind: "daily" | "weekly" | "monthly";
  period_key: string;
  rollup_scope: "overall" | "path";
  path_bucket_digest: string | null;
  path_bucket_kind: string | null;
  path_label: string | null;
  stage: string;
  reason_code: string;
  latest_generation_id: string;
  first_failed_at: string;
  last_attempt_at: string;
  last_trigger_kind: string | null;
  detail: string | null;
}

export interface SummaryRecoveryStatus {
  health: SummaryRecoveryHealth;
  record_count: number;
  detail: string | null;
  records: SummaryRecoveryRecord[];
}

export type ThreadGroupRecoveryHealth =
  | "healthy"
  | "corrupt"
  | "unsupported_version"
  | "unreadable";

export interface ThreadGroupRecoveryRecord {
  memories_endpoint_id: string;
  owner_id: number;
  group_id: string;
  expected_membership_snapshot_digest: string | null;
  stage: string;
  reason_code: string;
  latest_generation_id: string;
  first_failed_at: string;
  last_attempt_at: string;
  last_trigger_kind: string;
  detail: string | null;
}

export interface ThreadGroupRecoveryStatus {
  health: ThreadGroupRecoveryHealth;
  record_count: number;
  detail: string | null;
  records: ThreadGroupRecoveryRecord[];
}

export interface SummaryPathBucket {
  path_bucket_digest: string;
  path_bucket_kind: string;
  path_label: string | null;
}

export interface SummaryPathCandidate {
  path_bucket: SummaryPathBucket;
  count: number;
}

export interface ListSummaryPathCandidatesRequest {
  kind: Exclude<SummaryKind, "per-thread" | "thread-group">;
  period_key?: string | null;
  source_scope_digest?: string | null;
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
  group_id?: string | null;
  external_id: string | null;
  kind: SummaryKind;
  /** `2026-05-24` / `2026-W21` / `2026-05`; null for per-thread. */
  period_key: string | null;
  /** Project/team scope (`_all` by default); null for per-thread. */
  scope_key: string | null;
  content_json: string;
  updated_at_ms: number;
  /** Present only for v2 period summaries. */
  rollup_scope?: "overall" | "path" | null;
  source_scope_digest?: string | null;
  path_bucket?: {
    path_bucket_digest: string;
    path_bucket_kind: string;
    path_label: string | null;
  } | null;
  aggregation_mode?: string | null;
  completion_status?: "complete" | "incomplete" | null;
  generation_id?: string | null;
  generation_started_at?: string | null;
  generation_consistency?: string | null;
  input_generation_ids?: string[] | null;
  input_without_generation_id_count?: number | null;
  failed_path_bucket_digests?: string[];
  fallback_path_bucket_digests?: string[];
  missing_path_bucket_digests?: string[];
  path_bucket_diagnostics?: Array<{
    path_bucket_digest: string;
    statuses: string[];
    path_bucket_kind: string;
    path_label: string | null;
  }>;
  last_material_attempt_generation_id?: string | null;
  last_material_attempt_started_at?: string | null;
  last_material_attempt_status?: string | null;
  /** Number of path children that were planned but unavailable to an overall v2 rollup. */
  missing_path_bucket_count?: number | null;
  context_truncated?: boolean | null;
  included_source_count?: number | null;
  omitted_source_count?: number | null;
  /** Number of Path-scope summaries available for this Overall period row. */
  path_summary_count?: number | null;
  /** True only for pre-v2 period summaries. */
  legacy?: boolean;
}

/** Metadata-only child row for an expanded Overall Path disclosure. Its body
 * is intentionally fetched on demand through `getSummaryContent`. */
export interface SummaryPathChildEntry {
  memory_id: string;
  path_bucket: {
    path_bucket_digest: string;
    path_bucket_kind: string;
    path_label: string | null;
  };
  aggregation_mode?: string | null;
}

export interface ListSummariesRequest {
  /** Defaults to the public `thread-group` view server-side when omitted. */
  kind?: SummaryKind;
  limit?: number;
  offset?: number;
  created_after_ms?: number;
  created_before_ms?: number;
  updated_after_ms?: number;
  updated_before_ms?: number;
  /** Exact period token within a daily/weekly/monthly summary external ID. */
  period_key?: string;
  /** Thread labels that must match the backing summary thread (AND-only; see FR-SUM-2a). */
  labels_any?: string[];
  rollup_scopes?: Array<"overall" | "path">;
  path_bucket_digests?: string[];
}

/** Cursor-like offset page used by the selection modal. The dedicated
 * endpoint returns a continuation even when malformed rows are filtered. */
export interface ListSummariesForSelectionRequest {
  kind: SummaryKind;
  limit?: number;
  offset?: number;
  rollup_scopes?: Array<"overall" | "path">;
  path_bucket_digests?: string[];
  include_shadowed_legacy?: boolean;
}

export interface ListSummariesForSelectionResponse {
  entries: SummaryEntry[];
  next_offset: number | null;
}

export interface ListSummaryPathChildrenRequest {
  kind: Exclude<SummaryKind, "per-thread" | "thread-group">;
  period_key: string;
  source_scope_digest: string;
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
  group_id?: string | null;
  external_id: string | null;
  kind: SummaryKind | null;
  period_key: string | null;
  scope_key: string | null;
}

export interface ListSummaryPeriodKeysRequest {
  kind: SummaryKind;
  /** Prefixes of period tokens visible in the calendar month. */
  period_key_prefixes?: string[];
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
  /** Cancel-key. The frontend generates a UUID and the Threads toolbar's Stop
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

export type ImportStep =
  | "thread-import"
  | "thread-summary"
  | "thread-group-summary"
  | "thread-personality"
  | "reflection";

export type SummaryGenerationStep =
  | "thread-summary"
  | "thread-group-summary"
  | "daily"
  | "weekly"
  | "monthly";

export type StagedProgressKind = "import" | "summary-generation";
export type StagedProgressStep = ImportStep | SummaryGenerationStep;

export type StepStatus = "waiting" | "active" | "done" | "warning" | "failed" | "cancelled";

/** Stable non-error notices emitted alongside an import step update. */
export type ImportNoticeCode = "no-importable-log-sources";

export interface ImportStepState {
  status: StepStatus;
  message: string | null;
  notice_code?: ImportNoticeCode;
}

/** Shared snapshot for processes that report progress one stage at a time. */
export interface StagedProgressSnapshot<K extends StagedProgressKind = StagedProgressKind> {
  kind: K;
  job_id: string;
  sequence: number;
  steps: K extends "import"
    ? Record<ImportStep, ImportStepState>
    : K extends "summary-generation"
      ? Record<SummaryGenerationStep, ImportStepState>
      : Record<StagedProgressStep, ImportStepState>;
}

/** A port is `null` while that service is remote (or not running). */
export interface SidecarEndpoints {
  jobworkerp_port: number | null;
  memories_port: number | null;
  conductor_port: number | null;
  /**
   * MCP HTTP server bind port when the user enabled it (and the sidecar is
   * up), else `null`. jobworkerp boots the MCP server in the same process as
   * the gRPC front, so this is not a separate child — it is the port the MCP
   * server bound to, surfaced so the UI can print the external-client URL.
   */
  mcp_server_port: number | null;
  /** Issue Tracker gRPC bind port while it passed startup readiness, else `null`. */
  issue_tracker_port: number | null;
}

/**
 * Non-fatal startup conditions surfaced by the Rust core via
 * `sidecar://ready` so the UI can disable LLM-tagged flows when the
 * underlying runner registration failed.
 */
export type SidecarWarningKind =
  | "issue_tracker_unavailable"
  /** conductor stored the periodic schedulers without their job timeout. */
  | "conductor_outdated"
  /** The remote jobworkerp lacks Lookback's workers, channels or plugins. */
  | "remote_jobworkerp_unprepared"
  /** The remote jobworkerp was unreachable at startup; the app opened anyway. */
  | "remote_jobworkerp_unavailable"
  | "mcp_unavailable"
  | "mcp_toolset_unavailable"
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
 * via the snapshot. Both lifecycle fields are `null` while a fresh start is
 * still in flight; `database_migration_in_progress` identifies its local
 * migration-gate portion (the boot spinner stays).
 */
export interface SidecarStatusSnapshot {
  ready: SidecarStartReport | null;
  failure: SidecarErrorPayload | null;
  /** True only while the memories migration tool is changing the local DB at startup. */
  database_migration_in_progress: boolean;
  issue_tracker_availability?: IssueTrackerAvailabilitySnapshot | null;
}

export interface IssueTrackerAvailabilitySnapshot {
  endpoint_connected: boolean;
  gateway_ready: boolean;
  mutation_ready: boolean;
  reason: string | null;
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
 * this tagged union via `SidecarErrorPayload::from_app_error`: known startup
 * and database migration failures preserve their structured fields, while other
 * errors collapse to a raw message. The BootError UI branches on `kind`
 * (then on `failure.code`) — it must NOT parse the human `message` text.
 */
export type SidecarErrorPayload =
  | { kind: "structured"; failure: StartupFailure }
  | { kind: "recovery_failed"; failure: StartupFailure; recovery_error: string }
  | {
      kind: "database_migration_failed";
      /** Stage reported by `memories-db-migrate`. */
      stage: string;
      /** Fixed vocabulary of the migration tool; the UI acts on this only. */
      error_code: string;
      /** Tool's own classification; shown for reference, never branched on. */
      resolution: string;
      /** Backup to restore, or `""` when the tool reported none. */
      backup_path: string;
      /** English diagnostic for logs and bug reports. */
      detail: string;
    }
  | { kind: "raw"; message: string };

/** Response of every `recover_*` command. */
export interface RecoveryResult {
  restarted: boolean;
  backupPath: string | null;
  restartError: string | null;
}

export interface DatabaseRestoreResult {
  state: "safeStopped";
  backupPath: string;
}

export interface SettingsSnapshot {
  data_root: string;
  sqlite_path: string;
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

/** Model preparation state (3 states only). */
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

export interface SummaryLengthGuide {
  item_chars: number;
  total_chars: number;
}

export interface SummaryGuideProfile {
  daily: SummaryLengthGuide;
  weekly: SummaryLengthGuide;
  monthly: SummaryLengthGuide;
}

export interface SummaryOutputGuidance {
  small: SummaryGuideProfile;
  standard: SummaryGuideProfile;
  large: SummaryGuideProfile;
  limits: {
    daily: number;
    weekly: number;
    monthly: number;
    max_source_memory_ids: number;
  };
}

/** Non-secret LLM provider config. `api_key_set` indicates whether a key
 *  is stored in the OS keychain; the actual key is never sent to the frontend.
 *  `local_*` drive the local LLM model selection (preset / custom). */
export interface LlmSettingsResponse {
  mode: LlmMode;
  provider_model: string | null;
  api_key_set: boolean;
  base_url: string | null;
  max_tokens: number | null;
  model_max_output_tokens?: number | null;
  temperature: number | null;
  local_preset_id: string | null;
  local_model_file: string | null;
  local_hf_repo: string | null;
  local_ctx_size: number | null;
  local_kv_cache_type: KvCacheType | null;
  summary_output_guidance: SummaryOutputGuidance;
}

/** Request to update LLM settings. `api_key = null` means "don't change";
 *  an empty string deletes the stored key. */
export interface SetLlmSettingsRequest {
  mode: LlmMode;
  provider_model: string | null;
  api_key: string | null;
  base_url: string | null;
  max_tokens: number | null;
  model_max_output_tokens?: number | null;
  temperature: number | null;
  local_preset_id: string | null;
  local_model_file: string | null;
  local_hf_repo: string | null;
  local_ctx_size: number | null;
  local_kv_cache_type: KvCacheType | null;
  summary_output_guidance: SummaryOutputGuidance | null;
}

export type KvCacheType = "Q4_0" | "Q4_1" | "IQ4_NL" | "Q5_0" | "Q5_1" | "Q8_0";

/** Retained legacy per-preset `enable_thinking` preference, exposed as preset
 *  metadata but not sent in current chat payloads. */
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
  mtp_enabled: boolean;
  mtp_draft_model: string | null;
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
  onnx_model_file?: string | null;
  onnx_pooling?: string | null;
  document_prefix?: string | null;
  query_prefix?: string | null;
  recommended_languages?: string[];
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
  /** Optional for compatibility with older application-settings snapshots. */
  model_revision?: string;
  /** Embedding distance function reported by the active runtime. */
  distance_type?: string;
  onnx_model_file?: string | null;
  onnx_pooling?: string | null;
  document_prefix?: string | null;
  query_prefix?: string | null;
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
  /** Empty string uses the model's default revision. */
  model_revision?: string | null;
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
  /** Empty string uses the model's default revision. */
  model_revision?: string;
}

export interface SetEmbeddingSettingsResponse {
  runtime: EmbeddingRuntime;
  /** Retained for response compatibility; always null. */
  backup_path: string | null;
  restarted: boolean;
}

// ====================================================================
// Memories-owned embedding space and migration contract
// ====================================================================

export type SpaceValue = { kind: "unknown" } | { kind: "none" } | { kind: "id"; value: string };

export interface EmbeddingSpaceValue {
  space_id: string;
  model_id: string;
  tokenizer_model_id: string;
  revision: string;
  dimension: number;
  distance: string;
}

export interface EmbeddingMigrationMarkerView {
  state: string;
  attempt_id: string;
}

export interface EmbeddingSpaceRecordView {
  space: EmbeddingSpaceValue | null;
  legacy_accept: boolean;
  migration_marker: EmbeddingMigrationMarkerView | null;
}

export interface EmbeddingTableRecordView {
  table: string;
  record: EmbeddingSpaceRecordView | null;
}

export interface EmbeddingSpaceView {
  current_space: EmbeddingSpaceValue | null;
  mm_embedding_worker_name: string;
  registration_state: "registered" | "pending";
  tables: EmbeddingTableRecordView[];
  rpc_migration_supported: boolean;
  write_rejections: Record<string, number>;
}

export type EmbeddingMigrationState =
  | "consistent"
  | "incomplete"
  | "unverified"
  | "failed"
  | "unknown"
  | "migrating"
  | "rebuild_inconsistent"
  | "unavailable";

export type EmbeddingMigrationNextAction =
  | "none"
  | "wait"
  | "reconcile"
  | "resolve_failed"
  | "switch"
  | "check_environment"
  | "manual_recovery"
  | "restore"
  | "abandon"
  | "resume_or_abandon"
  | "resume_or_restore"
  | "start_rebuild"
  | "finalize"
  | "apply";

export type EmbeddingMigrationResolution =
  | "retry"
  | "plan_required"
  | "check_environment"
  | "manual_recovery"
  | "tool_update_required"
  | "resume_or_abandon"
  | "abandon_required"
  | "resume_or_restore"
  | "continue_or_restore"
  | "continue_or_abandon"
  | "resume_rebuild"
  | "resolve_failures"
  | "finalize_required"
  | "restore_required"
  | "apply_required"
  | "apply_restore_required"
  | "legacy_upgrade_required";

export interface EmbeddingAttemptInfo {
  attempt: string;
  stage: "backup" | "replace" | "rebuild_pending" | "commit" | "restoring" | "discarding";
  backup_mode: "backup" | "none";
}

export interface EmbeddingVerificationCounts {
  missing_memory_text: number;
  stale_memory_text: number;
  unverified_memory_text: number;
  failed_memory_text: number;
  missing_memory_media: number;
  stale_memory_media: number;
  unverified_memory_media: number;
  failed_memory_media: number;
  missing_thread: number;
  stale_thread: number;
  unverified_thread: number;
  failed_thread: number;
  missing_reflection_intent: number;
  stale_reflection_intent: number;
  unverified_reflection_intent: number;
  failed_reflection_intent: number;
  orphan: number;
}

export interface EmbeddingMigrationFailure {
  command: string;
  stage: string;
  error_code: string;
  resolution: EmbeddingMigrationResolution;
  attempt: EmbeddingAttemptInfo | null;
  backup: string | null;
  cancel_operation: "restore" | "abandon" | null;
  verification_counts: EmbeddingVerificationCounts | null;
}

export type EmbeddingMigrationDecision =
  | "no_change"
  | "reconcile_required"
  | "adopt_on_start"
  | "reembed_required";

export interface EmbeddingMigrationInspect {
  state: EmbeddingMigrationState;
  space: SpaceValue;
  required: number | null;
  complete: number | null;
  missing: number | null;
  stale: number | null;
  unverified: number | null;
  failed_transient: number | null;
  failed_permanent: number | null;
  orphan: number | null;
  backup_supported: boolean | null;
  attempt: EmbeddingAttemptInfo | null;
  backup: string | null;
  unavailable_reason: "resource_unavailable" | "resource_corrupt" | null;
  next_action: EmbeddingMigrationNextAction;
}

export interface EmbeddingMigrationPlan {
  decision: EmbeddingMigrationDecision;
  executable: boolean;
  reason: string | null;
  current_space: SpaceValue;
  target_space: string;
  memory_text: number;
  memory_media: number;
  thread: number;
  reflection_intent: number;
  failed_transient: number | null;
  failed_permanent: number | null;
}

export type EmbeddingBackup =
  | { mode: "directory"; path: string; keep?: number | null }
  | { mode: "no_backup_unsafe" };

export type EmbeddingMigrationResult =
  | { kind: "inspect"; data: EmbeddingMigrationInspect }
  | { kind: "plan"; data: EmbeddingMigrationPlan }
  | {
      kind: "switched";
      data: {
        outcome: "switched" | "already_switched";
        attempt: string;
        source_space: SpaceValue;
        target_space: string;
        backup: string | null;
        next_action: "start_rebuild";
      };
    }
  | {
      kind: "abandoned";
      data: {
        outcome: "abandoned" | "discarded" | "already_abandoned";
        attempt: string;
        next_action: "none" | "reconcile";
      };
    }
  | {
      kind: "finalized";
      data: {
        outcome: "finalized" | "already_completed";
        attempt: string;
        space: string;
        accepted_failed: number;
        next_action: "apply";
      };
    }
  | {
      kind: "restored";
      data: {
        outcome: "restored" | "already_restored";
        attempt: string;
        space: SpaceValue;
        next_action: "reconcile";
      };
    }
  | { kind: "failed"; data: EmbeddingMigrationFailure };

export interface EmbeddingMigrationOperationResponse {
  result: EmbeddingMigrationResult;
  restarted: boolean;
}

export interface StartEmbeddingRebuildRequest {
  attempt_id: string;
  retry_failed?: boolean;
}

export interface StartEmbeddingReconcileRequest {
  retry_failed?: boolean;
  force?: boolean;
  force_table?: string;
  force_entity_ids?: number[];
}

export interface StartEmbeddingRebuildResult {
  attempt_id: string;
  task_id: string;
}

export interface StartEmbeddingReconcileResult {
  task_id: string;
}

export interface EmbeddingRebuildKindProgressView {
  kind: string;
  targets: number;
  complete: number;
  missing: number;
  stale: number;
  unverified: number;
  failed_transient: number;
  failed_permanent: number;
  dispatched: number;
  failed_to_dispatch: number;
}

export interface EmbeddingRebuildProgressView {
  attempt_id: string;
  status: string;
  kinds: EmbeddingRebuildKindProgressView[];
  orphan: number;
  caption_missing: number;
  captions_requested: number;
  completion_expected: boolean;
  failed: number;
  write_rejections: Record<string, number>;
  error: string | null;
}

export interface EmbeddingReconcileKindProgressView {
  kind: string;
  missing: number;
  stale: number;
  unverified: number;
  failed: number;
  orphan: number;
  dispatched: number;
  complete: number;
  failed_to_dispatch: number;
}

export interface EmbeddingReconcileProgressView {
  task_id: string;
  status: string;
  kinds: EmbeddingReconcileKindProgressView[];
  captions_requested: number;
  error: string | null;
}

export interface EmbeddingMigrationFailedEvent {
  kind: "embedding_migration_failed";
  failure: EmbeddingMigrationFailure | null;
  detail: string;
  inspect_required: boolean;
}

// ====================================================================
// MCP server settings (expose RAG retrieval as MCP tools)
// ====================================================================

/** Frontend view of `mcp-settings.json` plus derived flags. Mirrors the
 *  Rust `commands::mcp_settings::McpSettingsResponse`. */
export interface McpSettingsResponse {
  enabled: boolean;
  /** Whether the MCP listener accepts requests without Bearer authentication. */
  allow_unauthenticated_requests: boolean;
  /** Advanced overrides. `null` ⇒ jobworkerp default is used. */
  exclude_runner_as_tool: boolean | null;
  exclude_worker_as_tool: boolean | null;
  streaming: boolean | null;
  request_timeout_sec: number | null;
  /** Period-summary scope used by MCP recall when the tool call omits one. */
  rag_default_period_scope: "overall_only" | "overall_and_path";
  /** Composite FunctionSet name exposed over MCP (constant `lookback-mcp`). */
  set_name: string;
  /** The MCP server's bound port while it is running, else `null`. */
  active_port: number | null;
  /** Whether an MCP bearer token is stored in the OS keyring. */
  auth_token_configured: boolean;
  /** Runtime availability after the most recent sidecar startup. */
  runtime_status: "disabled" | "ready" | "unavailable";
  /** Safe diagnostic detail when `runtime_status` is `unavailable`. */
  unavailable_reason: string | null;
}

export interface SetMcpSettingsRequest {
  enabled: boolean;
  /** Explicit opt-in to accepting MCP requests without a Bearer token. */
  allow_unauthenticated_requests: boolean;
  exclude_runner_as_tool: boolean | null;
  exclude_worker_as_tool: boolean | null;
  streaming: boolean | null;
  request_timeout_sec: number | null;
  rag_default_period_scope: "overall_only" | "overall_and_path";
  /** Non-persistent instruction to replace the MCP bearer token during the
   *  coordinated jobworkerp restart. */
  rotate_auth_token?: boolean;
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
  /** Explicit IANA timezone the user picked, or `null` for "Auto" (follow the
   *  env `TZ` / OS zone). Drives the Timezone card's select. */
  timezone: string | null;
  /** The zone the sidecar will inject as `TZ` right now (the resolved value of
   *  `timezone`, or the env/OS fallback when it is `null`). Shown as the card's
   *  "current effective" preview so an Auto selection still names a zone. */
  effective_timezone: string;
  /** Backup choice handed to the memories migration tool at startup. */
  migration_backup: MigrationBackupSettings;
  resolved: ResolvedAppPaths;
}

/** Whether the memories migration tool backs up the database before a
 *  migration that changes it, and how many completed backups it keeps. */
export interface MigrationBackupSettings {
  enabled: boolean;
  keep: number;
}

export interface SetHfHomeRequest {
  mode: HfHomeMode;
  path: string | null;
}

/** `timezone: null` (or empty) means "Auto" — clear the explicit selection and
 *  follow the env `TZ` / OS zone. */
export interface SetTimezoneRequest {
  timezone: string | null;
}

// ====================================================================
// Issue Tracker feature settings
// ====================================================================

/** Frontend view of the explicit Issue Tracker setting and its effective value. */
export interface IssueTrackerSettingsResponse {
  /** `null` means the build-time default is being used. */
  enabled: boolean | null;
  effective_enabled: boolean;
}

/** Request used by the unified settings apply command. */
export interface SetIssueTrackerSettingsRequest {
  enabled: boolean | null;
}

// ====================================================================
// Unified settings apply (batch save + single sidecar restart)
// ====================================================================

/** Batch-apply request. Each present field is persisted; absent fields are
 *  left untouched. The sidecar restarts at most ONCE for the whole batch
 *  (Rust `apply_settings` re-reads all settings files on start). */
export interface ApplySettingsRequest {
  llm: SetLlmSettingsRequest | null;
  embedding: SetEmbeddingSettingsRequest | null;
  hf_home: SetHfHomeRequest | null;
  mcp: SetMcpSettingsRequest | null;
  timezone: SetTimezoneRequest | null;
  issue_tracker: SetIssueTrackerSettingsRequest | null;
  migration_backup: MigrationBackupSettings | null;
}

export interface ApplySettingsResponse {
  /** `false` ⇒ nothing meaningful changed; the sidecar was left running. */
  restarted: boolean;
  /** The embedding runtime now in effect (only when embedding was part of
   *  the batch). */
  embedding_runtime: EmbeddingRuntime | null;
  /** Plugin-staging warnings surfaced by the restart, if any. */
  warnings: SidecarWarning[];
  /** Newly generated MCP bearer token, returned only after the coordinated
   *  restart succeeded. */
  mcp_auth_token: string | null;
}

export interface SetupStatus {
  required: boolean;
  resume_apply: boolean;
  current_data_root: string;
  default_data_root: string;
}

export interface ApplySetupRequest {
  data_root: string | null;
  preferred_language?: "ja" | "en";
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

/** Connection-target override (local sidecar vs remote server). */
export type ConnectionMode = "local" | "remote";

/** One service's target. The remote URL is kept while the service is local
 *  so the form can re-display it when the user switches back. */
export interface EndpointConfig {
  mode: ConnectionMode;
  remote_url: string | null;
}

/** A remote tracker also exposes its gateway on separate listeners. */
export interface IssueTrackerEndpointConfig extends EndpointConfig {
  remote_gateway_control_url: string | null;
  remote_gateway_worker_url: string | null;
}

/** Addresses for the remote side where they differ from the ones the app
 *  dials (e.g. behind a proxy). Unset values fall back to the app's. */
export interface WorkerViewOverrides {
  jobworkerp_url: string | null;
  memories_url: string | null;
  issue_tracker_url: string | null;
  issue_tracker_gateway_worker_url: string | null;
}

/** Paths on the jobworkerp host for the periodic import, which runs there. */
export interface JobworkerpHostPaths {
  memories_import_bin: string | null;
  opencode_db_path: string | null;
  import_working_dir: string | null;
}

export interface ConnectionConfig {
  jobworkerp: EndpointConfig;
  memories: EndpointConfig;
  issue_tracker: IssueTrackerEndpointConfig;
  conductor: EndpointConfig;
  worker_view: WorkerViewOverrides;
  jobworkerp_host_paths: JobworkerpHostPaths;
}

/** What a remote jobworkerp still lacks for Lookback. */
export interface RemoteJobworkerpReadiness {
  jobworkerp_url: string;
  missing_workers: string[];
  missing_function_sets: string[];
  /** The workers were registered by another app version, or never by this app. */
  registration_outdated: boolean;
  missing_channels: string[];
  missing_runners: string[];
  /** Environment the remote jobworkerp process needs, as `KEY=value`. */
  environment: string[];
  /** No gap above remains. */
  ready: boolean;
}

export interface RemoteJobworkerpRegistration {
  warnings: SidecarWarning[];
  readiness: RemoteJobworkerpReadiness;
}

export interface ConnectionTestReport {
  jobworkerp_url: string;
  memories_url: string;
}

/** Which log file to read. `app` is Lookback's own Rust-side log
 * (carries the memories-import child output); the rest are sidecar logs. */
export type LogSource = "jobworkerp" | "memories" | "issue_tracker" | "app";
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
// Reflections
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

/** Canonical structured reflection content used by the selection modal. The
 *  server serializes this as JSON so selected-memory projection can safely
 *  extract only validated source IDs instead of exposing raw protobuf data. */
export interface ReflectionSelectionContent {
  memory_id: string;
  origin_thread_id: string | null;
  content_json: string;
  source_thread_ids: string[];
  source_memory_ids: string[];
}

/** Compact row returned by the dedicated reflection-selection listing. */
export interface ReflectionSelectionEntry {
  memory_id: string;
  origin_thread_id: string | null;
  summary: string;
  content_json: string;
  source_thread_ids: string[];
  source_memory_ids: string[];
  created_at_ms: number;
  updated_at_ms: number;
}

/** Cursor-aware response for reflection selection listing. */
export interface ListReflectionSelectionResponse {
  entries: ReflectionSelectionEntry[];
  next_cursor_after_memory_id: string | null;
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

/** Request for the selection modal's chronological reflection list. */
export interface ListReflectionSelectionRequest {
  limit?: number;
  cursor_after_memory_id?: string;
}

export interface SearchReflectionsHybridRequest {
  query_text: string;
  /** Backing reflection memory owner. Defaults to the Lookback user ID. */
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

/** Live local-jobworkerp queue counts, grouped for the Settings monitor. */
export type BackgroundTaskKind =
  | "embedding"
  | "summary"
  | "personality"
  | "reflection"
  | "issue_resolution"
  | "llm_other";

export interface BackgroundJobQueueRow {
  kind: BackgroundTaskKind;
  pending: number;
  running: number;
  wait_result: number;
  cancelling: number;
}

export interface BackgroundJobQueueStatus {
  rows: BackgroundJobQueueRow[];
  active: boolean;
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
  last_message_after_ms?: number;
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
  last_message_after_ms?: number;
  /** Inclusive epoch-ms upper bound; omit for no upper bound. */
  last_message_before_ms?: number;
  /** Cancel-key. The backend uses this verbatim as the in-flight map
   *  key, so a Stop click that calls `analysis_cancel(dispatch_id)`
   *  hits the same entry. */
  dispatch_id?: string;
}

export interface EnqueueThreadGroupSummaryJobRequest {
  user_id?: number;
  group_owner_user_id?: number;
  force_resummarize?: boolean;
  dispatch_id?: string;
  target_group_id?: string;
  expected_membership_snapshot_digest?: string;
}

/** Reconcile Issue candidates for one ThreadGroup without regenerating its summary. */
export interface EnqueueThreadGroupIssueResolutionJobRequest {
  target_group_id: string;
  group_owner_user_id: number;
  expected_membership_snapshot_digest: string;
  dispatch_id?: string;
}

/** Re-evaluate Issue resolution for one thread's saved summary without regenerating it. */
export interface EnqueueThreadIssueResolutionJobRequest {
  target_thread_id: string;
  dispatch_id?: string;
}

export interface GetThreadIssueResolutionStatusRequest {
  thread_id: string;
}

/** `PENDING` does not prove that queued work is still alive after an app restart. */
export interface GetThreadIssueResolutionStatusResponse {
  status: string;
}

export interface EnqueuePersonalityJobRequest {
  user_id?: number;
  last_message_after_ms?: number;
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
export type PeriodKind = Exclude<SummaryKind, "per-thread" | "thread-group">;

export interface EnqueuePeriodSummaryJobRequest {
  kind: PeriodKind;
  /** Back-fill the last N periods; omit to generate the last completed one. */
  last_n?: number;
  /** Abort this standalone period run on the first observed path failure. */
  stop_on_error?: boolean;
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
  /** ThreadGroup stage. Optional for older typed callers; the manual dialog
   * always emits this field. */
  run_thread_group?: boolean;
  run_daily: boolean;
  run_weekly: boolean;
  run_monthly: boolean;
  stop_on_error: boolean;
  /** Per-thread window (epoch ms); omit both for unbounded. */
  last_message_after_ms?: number;
  last_message_before_ms?: number;
  /** Exclusive epoch-ms window selecting the ThreadGroups to summarize. */
  thread_group_activity_after_ms?: number;
  thread_group_activity_before_ms?: number;
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
// Personality
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
  /** Raw rows to scan for this page (server default 100). */
  limit?: number;
  /** `next_offset` of the previous page. */
  offset?: number;
}

/** One page of the signals drawer; pages advance over raw rows. */
export interface PersonalitySignalPage {
  signals: PersonalitySignal[];
  next_offset: number | null;
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
// Search
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
  /** Lifecycle kinds to search. Omit for the Threads tab's RAW-only scope. */
  memory_kinds?: number[];
  limit?: number;
}

export interface ThreadHit {
  thread_id: string;
  /** Present when a generated group-summary memory is the representative hit. */
  group_id?: string | null;
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
// RAG chat
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

export type RetrievalMode = "selected_only" | "source_details" | "related_memories";

/** Immutable content captured when the user confirms a summary selection. */
export interface SelectedMemorySnapshot {
  memory_id: string;
  kind: SelectableMemoryKind;
  content: string;
  title?: string;
  period_key?: string;
  scope_key?: string;
  group_id?: string;
  source_thread_ids?: string[];
  source_memory_ids?: string[];
  source_ids_truncated?: boolean;
  captured_at_ms: number;
}

export interface SelectedMemoryLimits {
  max_selected: number;
  max_direct_ids_per_memory: number;
  max_direct_ids_total: number;
  max_tool_ids_per_call: number;
  default_max_items_per_thread: number;
  max_items_per_thread: number;
  default_retrieval_hops: number;
  max_retrieval_hops: number;
  min_selected_tokens: number;
}

export interface ChatAskRequest {
  messages: ChatMessage[];
  /** Correlation key chosen by the caller. Must be unique per chat
   *  request; the Rust side echoes it on every `chat://step` event so
   *  the UI can register a turn under this id BEFORE invoking the
   *  command (closing the early-event-drop race where a Start emitted
   *  synchronously during dispatch could hit an unregistered turn). */
  job_id: string;
  selected_memories?: SelectedMemorySnapshot[];
  retrieval_mode?: RetrievalMode;
}

export interface ChatAskResponse {
  /** Echo of `ChatAskRequest.job_id`. */
  job_id: string;
  /** True when the model received a capacity-limited projection. */
  selected_content_truncated: boolean;
}

/** Minimum-contract chat phases. Kebab-case mirrors the Rust
 *  `ChatPhase` serde rename. */
export type ChatPhase =
  | "start"
  | "searching"
  | "approval-required"
  | "issue-mutation"
  | "source"
  | "token"
  | "done"
  | "error";

/** One pending tool call awaiting the user's approval decision. */
export interface ChatToolApproval {
  call_id: string;
  tool_name: string;
  arguments_json: string;
}

export interface ChatIssueMutationReceipt {
  tool_name: string;
  issue_id: string;
  operation_id: string;
  operation_state: string;
  audit_reason: string;
}

/** Persisted app-wide tool approval policy. */
export interface ChatToolApprovalSettings {
  require_approval: boolean;
}

/** One citation entry. Discriminated by `source_kind`; period and group
 *  summaries carry their owning scope instead of a source thread ID. */
export type ChatSource =
  | {
      source_kind: "raw_memory";
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
    }
  | {
      source_kind: "thread_group_summary";
      memory_id: string;
      group_id: string;
      snippet: string;
      score: number;
    }
  | {
      source_kind: "reflection";
      reflection_id: string;
      source_thread_id: string;
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
  cancelled?: boolean | null;
  tool_approval?: ChatToolApproval | null;
  issue_mutation?: ChatIssueMutationReceipt | null;
}

// ---- Issue tracker UI ---------------------------------------------------
// These DTOs deliberately describe the screen contract rather than generated
// protobuf messages. The Rust command layer owns adapting tracker revisions
// and nullable wire fields into this stable, renderer-friendly shape.
export type IssuePriority = "P0" | "P1" | "P2" | "P3" | "P4";
export type IssueState =
  | "OPEN"
  | "IN_PROGRESS"
  | "BLOCKED"
  | "DEFERRED"
  | "DONE"
  | "CANCELED"
  | "AWAITING_REVIEW";
export type IssueSuggestedState = Extract<
  IssueState,
  "OPEN" | "IN_PROGRESS" | "BLOCKED" | "DEFERRED"
>;
export type IssueContextKind = "PROJECT" | "REPOSITORY" | "OTHER";

export interface IssueContext {
  kind: IssueContextKind;
  value: string;
  uri: string | null;
}
export type IssueReviewState = "candidate" | "unreviewed" | "confirmed";
export type EvidenceFreshness = "normal" | "stale";
export type WorkEligibilityMode = "reviewed_only" | "include_auto_registered";

export type AutoRegisterPriority = "P0" | "P1" | "P2" | "P3" | "P4";
export type AutoIssueResolutionMode = "DISABLED" | "APPLY";

export interface AutoIssuePolicy {
  extraction_enabled: boolean;
  /** `null` keeps Candidates but disables automatic Issue registration. */
  auto_register_min_confidence: number | null;
  auto_register_priority_range: {
    lower: AutoRegisterPriority;
    upper: AutoRegisterPriority;
  };
  explicit_only: boolean;
  allowed_kinds: Array<"TASK" | "BUG" | "RESEARCH" | "DECISION">;
  extract_cap: number;
  resolution_mode: AutoIssueResolutionMode;
  resolution_min_confidence: number;
  /** Not edited in the UI; kept as-is when the policy is saved. */
  cross_thread_resolution_mode?: AutoIssueResolutionMode;
}

export interface AutoIssuePolicySnapshot {
  version: number;
  changed_at_ms: number;
  policy: AutoIssuePolicy;
}

export interface SetAutoIssuePolicyRequest {
  policy: AutoIssuePolicy;
}

export interface IssueReviewPolicy {
  require_high_priority_reason: boolean;
}

export interface SetIssueReviewPolicyRequest {
  policy: IssueReviewPolicy;
}

export interface IssueReviewItem {
  id: string;
  revision: string;
  associated_issue_id?: string | null;
  issue_id?: string | null;
  title: string;
  description: string;
  kind: string;
  priority: IssuePriority;
  suggested_state: IssueSuggestedState;
  acceptance_criteria: string[];
  confidence: number;
  review_state: Extract<IssueReviewState, "candidate" | "unreviewed">;
  detection: "explicit" | "inferred";
  strong_proof: boolean;
  evidence_count: number;
  duplicate_count: number;
  issue_state?: IssueState | null;
  integrity:
    | "normal"
    | "mapping_missing"
    | "mapping_invalid"
    | "issue_not_found"
    | "unknown_issue_state";
  resolution_state: "actionable" | "resolution_pending";
  source_labels: string[];
}

export interface IssueListItem {
  id: string;
  revision: string;
  title: string;
  description: string;
  kind: string;
  priority: IssuePriority;
  state: IssueState;
  review_state: Extract<IssueReviewState, "unreviewed" | "confirmed">;
  created_at_ms: number;
  updated_at_ms: number;
  completion_criteria?: string | null;
  /** Canonical Issue acceptance items. `completion_criteria` remains joined display text. */
  acceptanceCriteriaItems?: string[];
  registration_path?: string | null;
  policy?: string | null;
  proof?: string | null;
  source_labels: string[];
  /** Older app responses may omit contexts; current Rust projections return an empty array. */
  contexts?: IssueContext[] | null;
  claim?: IssueClaimView | null;
}

export interface IssueClaimView {
  lease_expires_at_ms?: number | null;
  claim_revision: string;
  lease_status: "lease_present" | "lease_expired_possible" | "lease_without_expiry";
}

export interface OwnershipDiagnosisView {
  target_kind: string;
  target_id: string;
  diagnosis_class: string;
  evidence_digest: string;
  diagnosis_generation: string;
  observation_token: string;
  repair_epoch: string;
  observed_revision?: string | null;
  lease_classification?: string | null;
  repair_condition: string;
  state: string;
  resolution_operation_id?: string | null;
  lease_expires_at_ms?: number | null;
  open_blocker_issue_ids: string[];
}

export interface CompletionEvidenceRequest {
  kind: "TEST_RESULT" | "ARTIFACT" | "OTHER_EVIDENCE";
  summary: string;
  uri?: string | null;
}

export interface CompletionReportRequest {
  verified_evidence?: CompletionEvidenceRequest[] | null;
  unverified_reason?: string | null;
  residual_risk?: string | null;
}

export interface ApplyIssueAdminActionRequest {
  issue_id: string;
  expected_revision: string;
  action: "COMPLETE" | "CANCEL" | "RECOVER" | "REVOKE";
  rationale: string;
  completion?: CompletionReportRequest | null;
  cancellation_classification?: "FALSE_POSITIVE" | "OTHER" | null;
  dependency_override?: { reason: string } | null;
  inconsistent_claim_resolution?: { reason: string } | null;
  ownership_diagnosis?: OwnershipDiagnosisReferenceRequest | null;
  operation_key: string;
}

export interface OwnershipDiagnosisReferenceRequest {
  target_kind: string;
  target_id: string;
  diagnosis_class: string;
  evidence_digest: string;
  diagnosis_generation: string;
  observation_token: string;
  repair_epoch: string;
}

export interface IssueOperationStatus {
  operation_id?: string | null;
  operation_key?: string | null;
  issue_id: string;
  method: string;
  state: string;
  generation: string;
  resolution_required: boolean;
  affects_ready: boolean;
  diagnosis?: string | null;
  /** Server-owned references eligible for evidence confirmation. */
  evidence_refs: string[];
  resulting_revision?: string | null;
  resulting_state?: string | null;
  resolution_operation_id?: string | null;
  error_code?: string | null;
  ownership_cleared?: boolean | null;
  dependency_override_applied?: boolean | null;
  inconsistent_claim_resolution_applied?: boolean | null;
  observed_lease_classification?: string | null;
  arbitration_outcome?: string | null;
}

export interface PendingIssueOperationView {
  operation_key: string;
  method: string;
  issue_id: string;
  created_at_ms: number;
}

export interface RetryPendingIssueOperationRequest {
  operation_key: string;
}

export interface GetIssueOperationStatusRequest {
  issue_id: string;
  operation_id: string;
  operation_key?: string | null;
}

interface ResolveIssueOperationCommon {
  issue_id: string;
  operation_id: string;
  expected_operation_generation: string;
  expected_revision: string;
  operation_key: string;
}

export type ResolveIssueOperationRequest = ResolveIssueOperationCommon &
  (
    | {
        mode: "EVIDENCE_CONFIRMATION";
        decision: "CONFIRM_APPLIED" | "CONFIRM_NOT_APPLIED";
        evidence_refs: string[];
        arbitration_outcome?: never;
        uncertainty_acknowledged?: never;
        arbitration_reason?: never;
      }
    | {
        mode: "HUMAN_ARBITRATION";
        arbitration_outcome: "APPLIED" | "NOT_APPLIED" | "UNKNOWN_SETTLED";
        uncertainty_acknowledged: true;
        arbitration_reason: string;
        evidence_refs?: string[];
        decision?: never;
      }
  );

export interface IssueEvidence {
  thread_id: string;
  memory_id: string;
  excerpt?: string | null;
  availability: "available" | "unavailable";
  unavailable_reason?: string | null;
}

export interface IssueDuplicate {
  id: string;
  title: string;
  description: string;
  state: IssueState;
  completion_criteria: string[];
  evidence_count: number;
}

export interface IssueHistoryEntry {
  id: string;
  created_at_ms: number;
  actor: string;
  decision: string;
  target_title: string;
  issue_ids: string[];
  rationale?: string | null;
  state: string;
}

export interface IssueHistoryResponse {
  entries: IssueHistoryEntry[];
  truncated: boolean;
}

export type IssueHistoryMode = "recovery" | "audit";

export interface ListIssueDirectHistoryRequest {
  issue_id: string;
  mode: IssueHistoryMode;
  page_token?: string | null;
}

export interface IssueHistoryPage {
  entries: IssueHistoryEntry[];
  next_page_token?: string | null;
}

export type IssueHistoryPageOutcome =
  | { kind: "page"; page: IssueHistoryPage }
  | { kind: "reset_required" };

export interface ListIssueReviewRequest {
  target?: "all" | "candidate" | "unreviewed";
  priority?: IssuePriority | "all";
  priorities?: IssuePriority[];
  min_confidence?: number | null;
  detection?: "all" | "explicit" | "inferred";
}

export interface ResolveIssueReviewRequest {
  candidate_id: string;
  expected_revision: string;
  associated_issue_id?: string | null;
  decision:
    | "confirm"
    | "reject"
    | "false_positive"
    | "cancel"
    | "human_review"
    | "merge"
    | "human_merge";
  rationale?: string | null;
  rejection_reason?: "false_positive" | "completed" | "other" | null;
  merge_target_id?: string | null;
  merge_target_type?: "candidate" | "issue" | null;
}

export interface UpdateIssueCandidateRequest {
  candidate_id: string;
  expected_revision: string;
  title: string;
  description: string;
  kind: string;
  priority: IssuePriority;
  suggested_state: IssueSuggestedState;
  acceptance_criteria: string[];
}

export interface CreateExplicitIssueRequest {
  candidate_id: string;
  expected_revision: string;
  rationale: string;
  title: string;
  description: string;
  kind: string;
  priority: IssuePriority;
  suggested_state: IssueSuggestedState;
  acceptance_criteria: string[];
}

export interface CreateIssueFromCanceledRequest {
  source_issue_id: string;
  expected_source_revision: string;
  operation_id: string;
  title: string;
  description: string;
  kind: string;
  priority: IssuePriority;
  suggested_state: IssueSuggestedState;
  acceptance_criteria: string[];
  rationale: string;
  link_to_source: boolean;
}

export interface TransitionIssueStateRequest {
  issue_id: string;
  expected_revision: string;
  target_state: "BLOCKED" | "OPEN";
  rationale?: string | null;
}

// ====================================================================
// Coding-agent Tauri commands. Keep this DTO layer aligned with the
// serde casing on src-tauri/src/commands/coding_agent.rs.
// ====================================================================

export type CodingAgentExecutionMode = "one_shot" | "interactive";
export type CodingAgentRunPurpose = "investigation" | "implementation";
export type CodingAgentPublicationPlan = "no_publication" | "push_branch" | "create_pull_request";

export interface CodingAgentSandboxResources {
  cpuMillis: number;
  memoryMib: number;
  rootDiskMib: number;
  maxParallelRuns: number;
  useStatic: boolean;
}

export interface CodingAgentPersistentMount {
  hostPath: string;
  guestPath: string;
  readOnly: boolean;
  executable: boolean;
}

export interface CodingAgentRepositoryIdentity {
  host: string;
  organization: string;
  repository: string;
}

export type CodingAgentCloneTransport =
  | { kind: "https"; baseUrl: string }
  | {
      kind: "ssh";
      host: string;
      user: string;
      port: number | null;
      /** Explicit host directory containing the trusted SSH configuration. */
      sshDirectory?: string | null;
    };

export interface CodingAgentNativeRepositoryApproval {
  repository: CodingAgentRepositoryIdentity;
  allowedBaseBranches: string[];
}

export interface CodingAgentRepositoryHost {
  labelHost: string;
  forge: "github" | "gitea";
  apiBaseUrl: string;
  cloneTransport: CodingAgentCloneTransport;
  readCredentialRef: string | null;
  publicationCredentialRef: string | null;
}

export type CodingAgentNativeSetupState =
  | "not_approved"
  | "approved"
  | "reapproval_required"
  | "package_mismatch"
  | "unavailable";

export type CodingAgentNativeSetupUnavailableReason =
  | "unsupported_platform"
  | "package_missing"
  | "trusted_policy_unsafe"
  | "trusted_policy_invalid"
  | "storage_unavailable"
  | "executor_unavailable"
  | "settings_unavailable";

export type CodingAgentNativeSetupPackageProvenance = "release" | "development";

export type CodingAgentNativeSetupApprovalSource = "gui" | "legacy_policy";

export interface CodingAgentNativeSetupResponse {
  settingsRevision: number;
  state: CodingAgentNativeSetupState;
  packageFingerprint: string | null;
  packageVersion: string | null;
  repositoryApprovals: CodingAgentNativeRepositoryApproval[];
  unavailableReason: CodingAgentNativeSetupUnavailableReason | null;
  packageProvenance: CodingAgentNativeSetupPackageProvenance | null;
  approvalSource: CodingAgentNativeSetupApprovalSource;
}

export interface CodingAgentApproveNativeBackendRequest {
  expectedRevision: number;
  expectedPackageFingerprint: string | null;
}

export type CodingAgentRepositoryHostCheckStageName =
  | "configuration"
  | "trusted_policy"
  | "git_read"
  | "forge_api";

export type CodingAgentRepositoryHostCheckStageStatus = "passed" | "failed" | "skipped";

/** Stable backend diagnostic codes with forward compatibility for newer executors. */
export type CodingAgentRepositoryHostCheckCode =
  | "settings_valid"
  | "invalid_settings"
  | "revision_conflict"
  | "policy_matched"
  | "repository_required"
  | "policy_missing"
  | "policy_invalid"
  | "repository_not_approved"
  | "native_backend_not_approved"
  | "policy_mismatch"
  | "git_readable"
  | "git_failed"
  | "forge_readable"
  | "forge_not_configured"
  | "credential_not_configured"
  | "authentication_failed"
  | "access_denied"
  | "repository_not_found"
  | "redirect_rejected"
  | "invalid_response"
  | "credential_unavailable"
  | "timeout"
  | "response_too_large"
  | "unavailable"
  | "unsupported_capability"
  | "prerequisite_failed"
  | (string & {});

export interface CodingAgentRepositoryHostCheckStage {
  stage: CodingAgentRepositoryHostCheckStageName;
  status: CodingAgentRepositoryHostCheckStageStatus;
  code: CodingAgentRepositoryHostCheckCode;
}

export interface CodingAgentRepositoryHostCheckRequest {
  expectedRevision: number;
  host: CodingAgentRepositoryHost;
  repository: CodingAgentRepositoryIdentity | null;
}

export interface CodingAgentRepositoryHostCheckResponse {
  settingsRevision: number;
  repository: CodingAgentRepositoryIdentity | null;
  stages: CodingAgentRepositoryHostCheckStage[];
}

export interface CodingAgentProfile {
  profileId: string;
  version: number;
  displayName: string;
  imageRef: string;
  allowedImageRefs: string[];
  command: string[];
  containerHome: string;
  /** Optional for compatibility with profiles saved before runtime env support. */
  runtimeEnvironment?: Record<string, string>;
  /** Optional host directories shared with the execution environment. */
  hostConfiguration?: {
    codexDirectory: string | null;
    claudeDirectory: string | null;
    claudeSettingsFile: string | null;
  } | null;
  networkAccess?: "disabled" | "public" | "public_and_private" | "public_private_and_host";
  resources: CodingAgentSandboxResources;
  mountAllowedRoots: string[];
  persistentMounts: CodingAgentPersistentMount[];
  allowedRepositories: CodingAgentRepositoryIdentity[];
  /** Opaque reference only; never a secret value. */
  credentialReference: string | null;
}

export interface CodingAgentSettings {
  formatVersion: number;
  revision: number;
  /** Read-only in the Settings editor; profile selection never mutates it. */
  activeProfileId: string | null;
  semanticJudgeEnabled: boolean;
  semanticJudgePolicy?: { maxRetries: number; deadlineSeconds: number };
  autoDoneEnabled: boolean;
  profiles: CodingAgentProfile[];
  repositoryHosts: CodingAgentRepositoryHost[];
  /** Global native execution approvals; this is the sole GUI-owned policy list. */
  nativeRepositoryApprovals?: CodingAgentNativeRepositoryApproval[];
  /** Read-only package baseline marker; approval uses its dedicated backend CAS action. */
  nativeBackendPackageFingerprint?: string | null;
}

export interface CodingAgentProfileMountAvailability {
  profileId: string;
  available: boolean;
  blockers: string[];
}

export interface CodingAgentPreset {
  id: string;
  displayName: string;
  imageRef: string;
  command: string[];
  containerHome: string;
  runtimeEnvironment: Record<string, string>;
}

export interface CodingAgentSettingsResponse {
  settings: CodingAgentSettings;
  profileMountAvailability: CodingAgentProfileMountAvailability[];
  availableAgentPresets: CodingAgentPreset[];
}

export interface CodingAgentUpdateSettingsRequest {
  expectedRevision: number;
  settings: CodingAgentSettings;
}

export interface CodingAgentBackendStatus {
  enabled: boolean;
  executorVersion: string | null;
  executorProtocolVersion: number | null;
  executorFeatures: string[];
  appManagedJobworkerpReady: boolean;
  blockers: string[];
}

export type CodingAgentCriterionEvaluationMethod = "deterministic" | "semantic_judge" | "unknown";

export interface CodingAgentCriterionPolicyRequest {
  criterionId: string;
  required: boolean;
  method: CodingAgentCriterionEvaluationMethod;
}

export interface CodingAgentValidationCommandRequest {
  argv: string[];
  workingDirectory: string;
  timeoutMs: number;
}

export interface CodingAgentValidationDraftRequest {
  required: boolean;
  command: CodingAgentValidationCommandRequest | null;
}

export type CodingAgentApprovedJudgeInputField =
  | "issue_title"
  | "issue_body"
  | "repository_identity"
  | "repository_summary"
  | "thread_group_context"
  | "bounded_file"
  | "bounded_diff"
  | "report"
  | "validation_result";

export type CodingAgentEvidenceSourceKind =
  | "issue_context"
  | "repository_context"
  | "thread_group_context"
  | "file"
  | "diff"
  | "report"
  | "validation_result";

export interface CodingAgentAllowedEvidenceSource {
  kind: CodingAgentEvidenceSourceKind;
  identity: string;
  max_bytes: number;
}

export interface CodingAgentAssessmentInputScope {
  allowed_fields: CodingAgentApprovedJudgeInputField[];
  allowed_sources: CodingAgentAllowedEvidenceSource[];
  max_input_bytes: number;
}

export type CodingAgentValidationPermission = "read_workspace" | "write_build_output";

export interface CodingAgentApprovalAssessmentCommand {
  argv: string[];
  working_directory: string;
  permissions: CodingAgentValidationPermission[];
  profile_digest: string;
  timeout_ms: number;
}

export interface CodingAgentApprovedValidationDefinition {
  validation_id: string;
  required: boolean;
  command: CodingAgentApprovalAssessmentCommand | null;
}

export interface CodingAgentApprovedAcceptanceCriterion {
  criterion_id: string;
  statement: string;
  required: boolean;
  method: CodingAgentCriterionEvaluationMethod;
}

export interface CodingAgentProviderSelection {
  provider_id: string;
  model_id: string;
  model_version: string;
  config_version: string;
}

/** Native ApprovalAssessmentContract: its nested Rust fields intentionally remain snake_case. */
export interface CodingAgentApprovalAssessmentContract {
  schema_version: number;
  purpose: CodingAgentRunPurpose;
  criteria: CodingAgentApprovedAcceptanceCriterion[];
  validation_plan: CodingAgentApprovedValidationDefinition[];
  provider: CodingAgentProviderSelection | null;
  prompt_policy_version: string | null;
  input_scope: CodingAgentAssessmentInputScope;
  judge_input_scope_digest: string;
}

export interface CodingAgentRunPreviewRequest {
  issueId: string;
  profileId: string;
  executionMode: "one_shot";
  purpose: CodingAgentRunPurpose;
  publicationPlan: CodingAgentPublicationPlan;
  manualRepository?: CodingAgentRepositoryIdentity | null;
  autoDoneRequested: boolean;
  prompt?: string | null;
  criterionPolicies?: CodingAgentCriterionPolicyRequest[];
  validationDrafts?: CodingAgentValidationDraftRequest[];
}

export interface CodingAgentRunPreview {
  planId: string;
  digest: string;
  issueId: string;
  issueRevision: number;
  issueTitle: string;
  issueState: string;
  sourceLabels: string[];
  settingsRevision: number;
  profileId: string | null;
  profileVersion: number | null;
  executionMode: CodingAgentExecutionMode;
  purpose: CodingAgentRunPurpose;
  publicationPlan: CodingAgentPublicationPlan;
  selectedRepository: CodingAgentRepositoryIdentity | null;
  repositorySelectionReason: string | null;
  repositoryCandidates: CodingAgentRepositoryIdentity[];
  autoDoneRequested: boolean;
  assessmentContract: CodingAgentApprovalAssessmentContract;
  assessmentContractDigest: string;
  assessmentProviderSupported: boolean;
  baseCommit?: string | null;
  workspaceBranch?: string | null;
  imageDigest?: string | null;
  blockers: string[];
  canApprove: boolean;
}

export interface CodingAgentDelegationPlan {
  delegationPlanId: string;
  request: {
    issueId: string;
    profileId: string;
    purpose: CodingAgentRunPurpose;
    prompt: string | null;
  };
  issueRevision: number;
  settingsRevision: number;
  createdAtMs: number;
  runId: string | null;
}

export interface CodingAgentApproveAndStartRequest {
  delegationPlanId?: string | null;
  preview: CodingAgentRunPreviewRequest;
  expectedPlanId: string;
  expectedDigest: string;
  userConfirmed: boolean;
  manualRepositoryConfirmed: boolean;
  autoDoneConfirmed: boolean;
}

export interface CodingAgentStartResponse {
  started: boolean;
  runId: string | null;
  blockers: string[];
}

export type CodingAgentRunState =
  | "approval_pending"
  | "queued"
  | "preparing"
  | "running"
  | "finalizing"
  | "verifying"
  | "publishing"
  | "awaiting_review"
  | "investigation_completed"
  | "done"
  | "failed"
  | "cancelled"
  | "unknown";

export type CodingAgentRunStage =
  | "approval"
  | "queue"
  | "preparation"
  | "execution"
  | "finalization"
  | "verification"
  | "publication"
  | "review"
  | "complete"
  | "failure"
  | "cancellation"
  | "reconciliation";

export interface CodingAgentEvidenceReference {
  evidenceId: string;
  attemptId: string;
  name: string;
  mediaType: string;
  sha256: string;
  sizeBytes: number;
  saveConfirmed: boolean;
  createdAtMs: number;
}

export interface CodingAgentRunStatus {
  runId: string;
  issueId: string;
  issueRevision: number;
  revision: number;
  executionMode: CodingAgentExecutionMode;
  purpose: CodingAgentRunPurpose;
  state: CodingAgentRunState;
  stage: CodingAgentRunStage;
  profileId: string;
  profileVersion: number;
  currentAttemptId: string | null;
  autoDone: boolean;
  workspacePath?: string | null;
  workspaceBranch?: string;
  commitId?: string | null;
  stdoutBytes: number | null;
  stderrBytes: number | null;
  evidence: CodingAgentEvidenceReference[];
}

export interface CodingAgentRunActionResponse {
  run: CodingAgentRunStatus;
}

export interface CodingAgentRunOwnerResponse {
  runId: string;
  windowLabel: string;
}

export interface CodingAgentEvidenceResponse {
  evidenceId: string;
  mediaType: string;
  bytes: number[];
}

export interface CodingAgentReviewMarker {
  runId: string;
  fullCommitId: string;
  evidenceReferences: string[];
}

export interface CodingAgentReviewSnapshot {
  semanticReassessmentAvailable?: boolean;
  run: CodingAgentRunStatus;
  issueState: string;
  issueRevision: number;
  marker: CodingAgentReviewMarker;
}

export interface CodingAgentReviewEvidenceInput {
  kind: "test_result" | "artifact" | "other_evidence";
  summary: string;
  uri?: string | null;
}

export interface CodingAgentReviewActionRequest {
  runId: string;
  expectedRunRevision: number;
  expectedIssueRevision: number;
  expectedMarker: CodingAgentReviewMarker;
  action: "complete" | "request_changes";
  rationale: string;
  report?: { evidence: CodingAgentReviewEvidenceInput[] } | null;
}

export interface CodingAgentReviewActionResponse {
  outcome: string;
  run: CodingAgentRunStatus | null;
  issueRevision: number;
}

export interface CodingAgentReassessmentRequest {
  runId: string;
  expectedRunRevision: number;
  expectedIssueRevision: number;
  expectedMarker: CodingAgentReviewMarker;
  requestKey: string;
}

export interface CodingAgentSemanticReassessment {
  runId: string;
  attemptId: string;
  requestKey: string;
  expectedRunRevision: number;
  issueRevision: number;
  sourceSha256: string;
  reportReference: string;
  assessment: { outcome: "PASS" | "FAIL" | "INCONCLUSIVE"; reason: string };
}
