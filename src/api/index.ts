import { invoke } from "@tauri-apps/api/core";
import type {
  AppCapabilities,
  ApplyIssueAdminActionRequest,
  ApplySettingsRequest,
  ApplySettingsResponse,
  ApplySetupRequest,
  ApplySetupResponse,
  AppSettingsResponse,
  AutoIssuePolicySnapshot,
  BackgroundJobQueueStatus,
  ChatAskRequest,
  ChatAskResponse,
  ChatToolApprovalSettings,
  CodingAgentApproveAndStartRequest,
  CodingAgentApproveNativeBackendRequest,
  CodingAgentBackendStatus,
  CodingAgentDelegationPlan,
  CodingAgentEvidenceReference,
  CodingAgentEvidenceResponse,
  CodingAgentNativeSetupResponse,
  CodingAgentReassessmentRequest,
  CodingAgentRepositoryHostCheckRequest,
  CodingAgentRepositoryHostCheckResponse,
  CodingAgentReviewActionRequest,
  CodingAgentReviewActionResponse,
  CodingAgentReviewSnapshot,
  CodingAgentRunActionResponse,
  CodingAgentRunOwnerResponse,
  CodingAgentRunPreview,
  CodingAgentRunPreviewRequest,
  CodingAgentRunStatus,
  CodingAgentSemanticReassessment,
  CodingAgentSettingsResponse,
  CodingAgentUpdateSettingsRequest,
  ConnectionConfig,
  ConnectionTestReport,
  CountSummariesRequest,
  CreateIssueFromCanceledRequest,
  DatabaseRestoreResult,
  DataRootValidation,
  DebugPersonalityInventoryRequest,
  EmbeddingBackup,
  EmbeddingMigrationOperationResponse,
  EmbeddingMigrationResult,
  EmbeddingPreset,
  EmbeddingRebuildProgressView,
  EmbeddingReconcileProgressView,
  EmbeddingSettingsResponse,
  EmbeddingSpaceView,
  EnqueueAnalysisJobResponse,
  EnqueuePeriodSummaryJobRequest,
  EnqueuePersonalityJobRequest,
  EnqueuePersonalityMergeJobRequest,
  EnqueueReflectionJobRequest,
  EnqueueReflectionJobResponse,
  EnqueueSummaryJobRequest,
  EnqueueThreadGroupIssueResolutionJobRequest,
  EnqueueThreadGroupSummaryJobRequest,
  EnqueueThreadIssueResolutionJobRequest,
  FindCoOccurringLabelsRequest,
  FindDistinctLabelsRequest,
  FindMemoriesRequest,
  FindMemoryPositionRequest,
  FindMemoryThreadPositionRequest,
  GenerateSummariesRequest,
  GetPersonalityRequest,
  GetThreadIssueResolutionStatusRequest,
  GetThreadIssueResolutionStatusResponse,
  IssueDuplicate,
  IssueEvidence,
  IssueHistoryPageOutcome,
  IssueHistoryResponse,
  IssueListItem,
  IssueOperationStatus,
  IssueReviewItem,
  IssueTrackerSettingsResponse,
  LabelWithCount,
  ListIssueDirectHistoryRequest,
  ListIssueReviewRequest,
  ListPeriodicTasksRequest,
  ListPersonalitySignalsRequest,
  ListReflectionSelectionRequest,
  ListReflectionSelectionResponse,
  ListReflectionsByThreadRequest,
  ListSummariesForSelectionRequest,
  ListSummariesForSelectionResponse,
  ListSummariesRequest,
  ListSummaryPathCandidatesRequest,
  ListSummaryPathChildrenRequest,
  ListSummaryPeriodKeysRequest,
  ListThreadGroupsRequest,
  ListThreadsRequest,
  LlmPreset,
  LlmSettingsResponse,
  LogSource,
  LogStream,
  LogTail,
  McpSettingsResponse,
  MemoryEmbeddingStats,
  MemoryPosition,
  MemoryReference,
  MemoryRow,
  MemoryThreadPosition,
  ModelStatusReport,
  OwnershipDiagnosisView,
  PendingIssueOperationView,
  PeriodicExecutionHistoryEntry,
  PeriodicExecutionSummary,
  PeriodicTaskEntry,
  PersonalityInventoryReport,
  PersonalityProfileContent,
  PersonalityResponse,
  PersonalitySignalContent,
  PersonalitySignalPage,
  PurgeReport,
  RecoveryResult,
  RedispatchEmbeddingsResult,
  RedispatchMemoryEmbeddingsRequest,
  RedispatchReflectionEmbeddingsRequest,
  ReflectionEntry,
  ReflectionIntentIndexStats,
  ReflectionSelectionContent,
  RemoteJobworkerpReadiness,
  RemoteJobworkerpRegistration,
  ResolvedSummaryMemoryRef,
  ResolveIssueOperationRequest,
  ResolveIssueReviewRequest,
  RetryPendingIssueOperationRequest,
  SavePeriodicTaskRequest,
  SearchIndexMaintenanceStatus,
  SearchReflectionsByIntentRequest,
  SearchReflectionsHybridRequest,
  SearchReflectionsRequest,
  SearchThreadGroupsRequest,
  SearchThreadsRequest,
  SelectedMemoryLimits,
  SetAutoIssuePolicyRequest,
  SetEmbeddingSettingsRequest,
  SetEmbeddingSettingsResponse,
  SetHfHomeRequest,
  SetLlmSettingsRequest,
  SetMcpSettingsRequest,
  SettingsSnapshot,
  SetupStatus,
  SidecarStatusSnapshot,
  SpaceValue,
  StagedProgressKind,
  StagedProgressSnapshot,
  StartEmbeddingRebuildRequest,
  StartEmbeddingRebuildResult,
  StartEmbeddingReconcileRequest,
  StartEmbeddingReconcileResult,
  StartImportRequest,
  StartImportResponse,
  SummaryContent,
  SummaryEntry,
  SummaryPathCandidate,
  SummaryPathChildEntry,
  SummaryRecoveryStatus,
  ThreadGroupLineageView,
  ThreadGroupListItem,
  ThreadGroupRecoveryStatus,
  ThreadGroupSearchPage,
  ThreadGroupSummaryEntryView,
  ThreadGroupSummaryView,
  ThreadHit,
  ThreadSummary,
  UpdateIssueCandidateRequest,
} from "@/types/api";

/** Read the versioned coding-agent profile document without exposing secret values. */
export async function codingAgentGetSettings(): Promise<CodingAgentSettingsResponse> {
  return invoke<CodingAgentSettingsResponse>("coding_agent_get_settings");
}

/** Persist a complete settings snapshot using the backend's compare-and-swap revision. */
export async function codingAgentUpdateSettings(
  request: CodingAgentUpdateSettingsRequest,
): Promise<CodingAgentSettingsResponse> {
  return invoke<CodingAgentSettingsResponse>("coding_agent_update_settings", { request });
}

export async function codingAgentPrepareProfileHome(request: {
  expectedRevision: number;
  profileId: string;
}): Promise<CodingAgentSettingsResponse> {
  return invoke<CodingAgentSettingsResponse>("coding_agent_prepare_profile_home", { request });
}

export async function codingAgentCheckRepositoryHost(
  request: CodingAgentRepositoryHostCheckRequest,
): Promise<CodingAgentRepositoryHostCheckResponse> {
  return invoke<CodingAgentRepositoryHostCheckResponse>("coding_agent_check_repository_host", {
    request,
  });
}

export async function codingAgentGetBackendStatus(): Promise<CodingAgentBackendStatus> {
  return invoke<CodingAgentBackendStatus>("coding_agent_get_backend_status");
}

export async function codingAgentGetNativeSetup(): Promise<CodingAgentNativeSetupResponse> {
  return invoke<CodingAgentNativeSetupResponse>("coding_agent_get_native_setup");
}

export async function codingAgentApproveNativeBackend(
  request: CodingAgentApproveNativeBackendRequest,
): Promise<CodingAgentNativeSetupResponse> {
  return invoke<CodingAgentNativeSetupResponse>("coding_agent_approve_native_backend", { request });
}

/** The preview is server-derived and its digest must be echoed for approval. */
export async function codingAgentPreviewRun(
  request: CodingAgentRunPreviewRequest,
): Promise<CodingAgentRunPreview> {
  return invoke<CodingAgentRunPreview>("coding_agent_preview_run", { request });
}

export async function codingAgentApproveAndStart(
  request: CodingAgentApproveAndStartRequest,
): Promise<import("@/types/api").CodingAgentStartResponse> {
  return invoke<import("@/types/api").CodingAgentStartResponse>("coding_agent_approve_and_start", {
    request,
  });
}

export async function codingAgentListDelegationPlans(
  issueId: string,
): Promise<CodingAgentDelegationPlan[]> {
  return invoke("coding_agent_list_delegation_plans", { issueId });
}

export async function codingAgentListRuns(): Promise<CodingAgentRunStatus[]> {
  return invoke<CodingAgentRunStatus[]>("coding_agent_list_runs");
}

export async function codingAgentGetRun(runId: string): Promise<CodingAgentRunStatus> {
  return invoke<CodingAgentRunStatus>("coding_agent_get_run", { runId });
}

export async function codingAgentAttachRun(runId: string): Promise<CodingAgentRunOwnerResponse> {
  return invoke<CodingAgentRunOwnerResponse>("coding_agent_attach_run", { runId });
}

export async function codingAgentCancelRun(runId: string): Promise<CodingAgentRunActionResponse> {
  return invoke<CodingAgentRunActionResponse>("coding_agent_cancel_run", { runId });
}

export async function codingAgentListEvidence(
  runId: string,
): Promise<CodingAgentEvidenceReference[]> {
  return invoke<CodingAgentEvidenceReference[]>("coding_agent_list_evidence", { runId });
}

export async function codingAgentReadEvidence(
  runId: string,
  evidenceId: string,
): Promise<CodingAgentEvidenceResponse> {
  return invoke<CodingAgentEvidenceResponse>("coding_agent_read_evidence", { runId, evidenceId });
}

export async function codingAgentGetReview(runId: string): Promise<CodingAgentReviewSnapshot> {
  return invoke<CodingAgentReviewSnapshot>("coding_agent_get_review", { runId });
}

export async function codingAgentApplyReviewAction(
  request: CodingAgentReviewActionRequest,
): Promise<CodingAgentReviewActionResponse> {
  return invoke<CodingAgentReviewActionResponse>("coding_agent_apply_review_action", { request });
}

export async function codingAgentReassess(
  request: CodingAgentReassessmentRequest,
): Promise<CodingAgentSemanticReassessment> {
  return invoke("coding_agent_reassess", { request });
}

export async function listIssueReviewItems(
  req: ListIssueReviewRequest = {},
): Promise<IssueReviewItem[]> {
  return issueInvoke<IssueReviewItem[]>("list_issue_review_items", { req });
}

export async function getAppCapabilities(): Promise<AppCapabilities> {
  return invoke<AppCapabilities>("get_app_capabilities");
}

export async function getIssueTrackerSettings(): Promise<IssueTrackerSettingsResponse> {
  return invoke<IssueTrackerSettingsResponse>("get_issue_tracker_settings");
}

export async function listIssues(req: { states?: string[] } = {}): Promise<IssueListItem[]> {
  return invoke<IssueListItem[]>("list_issues", { req });
}

export async function getIssueDetail(issueId: string): Promise<IssueListItem> {
  return invoke<IssueListItem>("get_issue_detail", { issueId });
}

export async function listIssueOwnershipDiagnostics(
  issueId?: string,
): Promise<OwnershipDiagnosisView[]> {
  return issueInvoke<OwnershipDiagnosisView[]>("list_issue_ownership_diagnostics", { issueId });
}

export async function applyIssueAdminAction(
  req: ApplyIssueAdminActionRequest,
): Promise<IssueOperationStatus> {
  return issueInvoke<IssueOperationStatus>("apply_issue_admin_action", { req });
}

export async function getIssueOperationStatus(
  req: import("@/types/api").GetIssueOperationStatusRequest,
): Promise<IssueOperationStatus> {
  return issueInvoke<IssueOperationStatus>("get_issue_operation_status", { req });
}

export async function resolveIssueOperation(
  req: ResolveIssueOperationRequest,
): Promise<IssueOperationStatus> {
  return issueInvoke<IssueOperationStatus>("resolve_issue_operation", { req });
}

export async function listIssueDirectHistoryPage(
  req: ListIssueDirectHistoryRequest,
): Promise<IssueHistoryPageOutcome> {
  return issueInvoke<IssueHistoryPageOutcome>("list_issue_direct_history", { req });
}

export async function listPendingIssueOperations(): Promise<PendingIssueOperationView[]> {
  return issueInvoke<PendingIssueOperationView[]>("list_pending_issue_operations");
}

export async function retryPendingIssueOperation(
  req: RetryPendingIssueOperationRequest,
): Promise<IssueOperationStatus> {
  return issueInvoke<IssueOperationStatus>("retry_pending_issue_operation", {
    req,
  });
}

export async function getIssueEvidence(itemId: string): Promise<IssueEvidence[]> {
  return issueInvoke<IssueEvidence[]>("get_issue_evidence", { itemId });
}

export async function getIssueDuplicates(itemId: string): Promise<IssueDuplicate[]> {
  return invoke<IssueDuplicate[]>("get_issue_duplicates", { itemId });
}

export async function listIssueHistory(): Promise<IssueHistoryResponse> {
  return invoke<IssueHistoryResponse>("list_issue_history");
}

export async function resolveIssueReview(req: ResolveIssueReviewRequest): Promise<void> {
  return issueInvoke<void>("resolve_issue_review", { req });
}

export async function updateIssueCandidate(req: UpdateIssueCandidateRequest): Promise<void> {
  return invoke<void>("update_issue_candidate", { req });
}

export async function createExplicitIssueFromCandidate(
  req: import("@/types/api").CreateExplicitIssueRequest,
): Promise<void> {
  return invoke<void>("create_explicit_issue_from_candidate", { req });
}

export async function createIssueFromCanceled(
  req: CreateIssueFromCanceledRequest,
): Promise<IssueListItem> {
  return invoke<IssueListItem>("create_issue_from_canceled", { req });
}

export async function reextractRejectedCandidate(
  candidateId: string,
): Promise<EnqueueAnalysisJobResponse> {
  return issueInvoke<EnqueueAnalysisJobResponse>("reextract_rejected_candidate", {
    req: { candidate_id: candidateId },
  });
}

async function issueInvoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(command, args);
  } catch (error) {
    if (error instanceof Error) throw error;
    throw new Error(typeof error === "string" ? error : String(error));
  }
}

export async function transitionIssueState(
  req: import("@/types/api").TransitionIssueStateRequest,
): Promise<void> {
  return invoke("transition_issue_state", { req });
}

export async function getWorkEligibility(): Promise<{
  work_eligibility: "REVIEWED_ONLY" | "INCLUDE_AUTO_REGISTERED";
}> {
  return invoke("get_work_eligibility");
}

export async function setWorkEligibility(req: {
  work_eligibility: "REVIEWED_ONLY" | "INCLUDE_AUTO_REGISTERED";
}): Promise<{ work_eligibility: "REVIEWED_ONLY" | "INCLUDE_AUTO_REGISTERED" }> {
  return invoke("set_work_eligibility", { req });
}

export async function getIssueReviewPolicy(): Promise<import("@/types/api").IssueReviewPolicy> {
  return invoke("get_issue_review_policy");
}

export async function setIssueReviewPolicy(
  req: import("@/types/api").SetIssueReviewPolicyRequest,
): Promise<import("@/types/api").IssueReviewPolicy> {
  return invoke("set_issue_review_policy", { req });
}

export async function getAutoIssuePolicy(): Promise<AutoIssuePolicySnapshot> {
  return invoke<AutoIssuePolicySnapshot>("get_auto_issue_policy");
}

export async function setAutoIssuePolicy(
  req: SetAutoIssuePolicyRequest,
): Promise<AutoIssuePolicySnapshot> {
  return invoke<AutoIssuePolicySnapshot>("set_auto_issue_policy", { req });
}

export async function listThreads(req: ListThreadsRequest): Promise<ThreadSummary[]> {
  return invoke<ThreadSummary[]>("list_threads", { req });
}

export async function listThreadGroups(
  req: ListThreadGroupsRequest = {},
): Promise<ThreadGroupListItem[]> {
  return invoke<ThreadGroupListItem[]>("list_thread_groups", { req });
}

export async function searchThreadGroups(
  req: SearchThreadGroupsRequest,
): Promise<ThreadGroupSearchPage> {
  return invoke<ThreadGroupSearchPage>("search_thread_groups", { req });
}

export async function findThreadGroup(
  group_id: string,
  group_owner_user_id?: number,
): Promise<ThreadGroupLineageView> {
  return invoke<ThreadGroupLineageView>("find_thread_group", {
    req: { group_id, group_owner_user_id },
  });
}

export async function findThreadGroupSummary(
  group_id: string,
  group_owner_user_id?: number,
): Promise<ThreadGroupSummaryView | null> {
  return invoke<ThreadGroupSummaryView | null>("find_thread_group_summary", {
    req: { group_id, group_owner_user_id },
  });
}

/** Summaries for many groups in one IPC/RPC round trip, in request order. */
export async function findThreadGroupSummaries(
  group_ids: string[],
  group_owner_user_id?: number,
): Promise<ThreadGroupSummaryEntryView[]> {
  return invoke<ThreadGroupSummaryEntryView[]>("find_thread_group_summaries", {
    req: { group_ids, group_owner_user_id },
  });
}

export async function countActiveThreadGroups(group_owner_user_id?: number): Promise<number> {
  return invoke<number>("count_active_thread_groups", { groupOwnerUserId: group_owner_user_id });
}

export async function countThreadGroupSummaries(group_owner_user_id?: number): Promise<number> {
  return invoke<number>("count_thread_group_summaries", { groupOwnerUserId: group_owner_user_id });
}

export async function getThreadGroupRecoveryStatus(): Promise<ThreadGroupRecoveryStatus> {
  return invoke<ThreadGroupRecoveryStatus>("get_thread_group_recovery_status");
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

/** Fetch a Memory plus every available Thread location for direct ID navigation. */
export async function resolveMemoryReference(memory_id: string): Promise<MemoryReference | null> {
  return invoke<MemoryReference | null>("resolve_memory_reference", { req: { memory_id } });
}

export async function findMemoriesByThreadId(req: FindMemoriesRequest): Promise<MemoryRow[]> {
  return invoke<MemoryRow[]>("find_memories_by_thread_id", { req });
}

/** Delete a thread and (server cascade) every Memory attached to it. */
export async function deleteThread(thread_id: string): Promise<void> {
  return invoke<void>("delete_thread", { req: { thread_id } });
}

export async function listSummaries(req: ListSummariesRequest): Promise<SummaryEntry[]> {
  return invoke<SummaryEntry[]>("list_summaries", { req });
}

/** Offset page for the selection modal; continuation is based on raw rows
 * rather than the number of valid rows returned after namespace checks. */
export async function listSummariesForSelection(
  req: ListSummariesForSelectionRequest,
): Promise<ListSummariesForSelectionResponse> {
  return invoke<ListSummariesForSelectionResponse>("list_summaries_for_selection", { req });
}

/** Path children of one v2 Overall period summary. Bodies remain lazy-loaded. */
export async function listSummaryPathChildren(
  req: ListSummaryPathChildrenRequest,
): Promise<SummaryPathChildEntry[]> {
  return invoke<SummaryPathChildEntry[]>("list_summary_path_children", { req });
}

/** Fetch complete summary content for selection and preview. `null` means the
 * listed memory no longer exists or is no longer a summary. */
export async function getSummaryContent(memory_id: string): Promise<string | null> {
  return invoke<string | null>("get_summary_content", { req: { memory_id } });
}

export async function listSummaryPathCandidates(
  req: ListSummaryPathCandidatesRequest,
): Promise<SummaryPathCandidate[]> {
  return invoke<SummaryPathCandidate[]>("list_summary_path_candidates", { req });
}

/** Labels attached to per-thread summary threads, excluding internal period axes. */
export async function findSummaryDistinctLabels(): Promise<LabelWithCount[]> {
  return invoke<LabelWithCount[]>("find_summary_distinct_labels");
}

/** Labels co-occurring with every selected label on per-thread summary threads. */
export async function findSummaryCoOccurringLabels(labels: string[]): Promise<LabelWithCount[]> {
  return invoke<LabelWithCount[]>("find_summary_co_occurring_labels", { req: { labels } });
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

/** Restore the latest active staged process of one kind without sharing its
 *  identity or progress sequence with other process kinds. */
export async function getActiveStagedProgress(
  kind: StagedProgressKind,
): Promise<StagedProgressSnapshot | null> {
  return invoke<StagedProgressSnapshot | null>("get_active_staged_progress", { kind });
}

/** Cancel an in-flight import pipeline. Idempotent: a finished or
 *  unknown dispatch id resolves silently so callers can safely retry. */
export async function startImportCancel(dispatchId: string): Promise<void> {
  return invoke<void>("start_import_cancel", { dispatchId });
}

/** Cancel an in-flight analysis dispatch (summary / personality / period
 *  summary / staged summary generation) by its dispatch id. Idempotent. */
export async function analysisCancel(dispatchId: string): Promise<void> {
  return invoke<void>("analysis_cancel", { dispatchId });
}

export async function getSettings(): Promise<SettingsSnapshot> {
  return invoke<SettingsSnapshot>("get_settings");
}

export async function getSearchIndexMaintenanceStatus(): Promise<SearchIndexMaintenanceStatus> {
  return invoke<SearchIndexMaintenanceStatus>("get_search_index_maintenance_status");
}

export async function getSummaryRecoveryStatus(): Promise<SummaryRecoveryStatus> {
  return invoke<SummaryRecoveryStatus>("get_summary_recovery_status");
}

export async function resetSummaryRecoveryState(): Promise<SummaryRecoveryStatus> {
  return invoke<SummaryRecoveryStatus>("reset_summary_recovery_state");
}

export async function startSearchIndexMaintenance(): Promise<void> {
  return invoke<void>("start_search_index_maintenance");
}

export async function setSearchIndexMaintenanceSchedule(req: {
  enabled: boolean;
  start: string | null;
  end: string | null;
}): Promise<void> {
  return invoke<void>("set_search_index_maintenance_schedule", { req });
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

/** Chronological, filter-only reflection listing for the selection modal. */
export async function listReflectionSelection(
  req: ListReflectionSelectionRequest,
): Promise<ListReflectionSelectionResponse> {
  return invoke<ListReflectionSelectionResponse>("list_reflections_for_selection", { req });
}

/** Fetch the canonical structured reflection body for selection/preview. */
export async function getReflectionSelectionContent(
  memory_id: string,
): Promise<ReflectionSelectionContent | null> {
  return invoke<ReflectionSelectionContent | null>("get_reflection_selection_content", {
    req: { memory_id },
  });
}

export async function searchReflectionsHybrid(
  req: SearchReflectionsHybridRequest,
): Promise<ReflectionEntry[]> {
  return invoke<ReflectionEntry[]>("search_reflections_hybrid", { req });
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
): Promise<PersonalitySignalPage> {
  return invoke<PersonalitySignalPage>("list_personality_signals", { req });
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
 * the Lookback user ID to expose which failure mode the
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

/** Active local jobworkerp queue counts for the Settings monitor. */
export async function getBackgroundJobQueueStatus(): Promise<BackgroundJobQueueStatus> {
  return invoke<BackgroundJobQueueStatus>("get_background_job_queue_status");
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

export async function enqueueThreadGroupSummaryJob(
  req: EnqueueThreadGroupSummaryJobRequest = {},
): Promise<EnqueueAnalysisJobResponse> {
  return invoke<EnqueueAnalysisJobResponse>("enqueue_thread_group_summary_job", { req });
}

/** Reconcile Issue candidates for one ThreadGroup without regenerating its summary. */
export async function enqueueThreadGroupIssueResolutionJob(
  req: EnqueueThreadGroupIssueResolutionJobRequest,
): Promise<EnqueueAnalysisJobResponse> {
  return invoke<EnqueueAnalysisJobResponse>("enqueue_thread_group_issue_resolution_job", { req });
}

/** Re-evaluate Issue resolution for a saved thread summary without regenerating the summary. */
export async function enqueueThreadIssueResolutionJob(
  req: EnqueueThreadIssueResolutionJobRequest,
): Promise<EnqueueAnalysisJobResponse> {
  return invoke<EnqueueAnalysisJobResponse>("enqueue_thread_issue_resolution_job", { req });
}

export async function getThreadIssueResolutionStatus(
  req: GetThreadIssueResolutionStatusRequest,
): Promise<GetThreadIssueResolutionStatusResponse> {
  return invoke<GetThreadIssueResolutionStatusResponse>("get_thread_issue_resolution_status", {
    req,
  });
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

/** Dispatch manual staged summary generation. The range is already expanded
 *  per layer by the caller; stage snapshots stream on `staged://step` and use
 *  `dispatch_id` as their identity. */
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

/** Read the persisted, app-wide policy for chat tool approvals. */
export async function getChatToolSettings(): Promise<ChatToolApprovalSettings> {
  return invoke<ChatToolApprovalSettings>("get_chat_tool_settings");
}

/** Persist the app-wide policy for chat tool approvals. */
export async function setChatToolSettings(
  req: ChatToolApprovalSettings,
): Promise<ChatToolApprovalSettings> {
  return invoke<ChatToolApprovalSettings>("set_chat_tool_settings", { req });
}

/** Resolve one backend approval pause using both correlation ids. */
export async function chatRespondToolApproval(
  jobId: string,
  callId: string,
  approved: boolean,
): Promise<void> {
  return invoke<void>("chat_respond_tool_approval", { jobId, callId, approved });
}

export interface PendingChatIssueOperation {
  operation_key: string;
  issue_id: string;
  method: string;
  created_at_ms: number;
}

export interface ChatIssueOperationResult {
  issue_id: string;
  operation_id: string;
  operation_state: string;
  revision: string;
  resulting_state: string;
}

export async function listPendingChatIssueOperations(): Promise<PendingChatIssueOperation[]> {
  return invoke<PendingChatIssueOperation[]>("list_pending_chat_issue_operations");
}

export async function retryPendingChatIssueOperation(
  operationKey: string,
): Promise<ChatIssueOperationResult> {
  return invoke<ChatIssueOperationResult>("retry_pending_chat_issue_operation", { operationKey });
}

export async function getSelectedMemoryLimits(): Promise<SelectedMemoryLimits> {
  return invoke<SelectedMemoryLimits>("get_selected_memory_limits");
}

export async function saveChatMarkdown(path: string, content: string): Promise<void> {
  return invoke<void>("save_chat_markdown", { req: { path, content } });
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

/** Report what the active remote jobworkerp lacks for Lookback. */
export async function inspectRemoteJobworkerp(): Promise<RemoteJobworkerpReadiness> {
  return invoke<RemoteJobworkerpReadiness>("inspect_remote_jobworkerp");
}

/** Register Lookback's workers on the active remote jobworkerp. */
export async function registerRemoteJobworkerpWorkers(): Promise<RemoteJobworkerpRegistration> {
  return invoke<RemoteJobworkerpRegistration>("register_remote_jobworkerp_workers");
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

/** Switch to a separately selected empty root and restart into first-time setup.
 * The old root remains untouched for manual recovery and audit. */
export async function startFreshSetup(path: string): Promise<void> {
  return invoke<void>("start_fresh_setup", { path });
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

/** Read the embedding space and table markers from the selected Memories endpoint. */
export async function getEmbeddingSpace(): Promise<EmbeddingSpaceView> {
  return invoke<EmbeddingSpaceView>("get_embedding_space");
}

export async function startEmbeddingRebuild(
  req: StartEmbeddingRebuildRequest,
): Promise<StartEmbeddingRebuildResult> {
  return invoke<StartEmbeddingRebuildResult>("start_embedding_rebuild", { req });
}

export async function getEmbeddingRebuildProgress(
  attemptId: string,
): Promise<EmbeddingRebuildProgressView> {
  return invoke<EmbeddingRebuildProgressView>("get_embedding_rebuild_progress", { attemptId });
}

export async function startEmbeddingReconcile(
  req: StartEmbeddingReconcileRequest = {},
): Promise<StartEmbeddingReconcileResult> {
  return invoke<StartEmbeddingReconcileResult>("start_embedding_reconcile", { req });
}

export async function getEmbeddingReconcileProgress(
  taskId: string,
): Promise<EmbeddingReconcileProgressView> {
  return invoke<EmbeddingReconcileProgressView>("get_embedding_reconcile_progress", { taskId });
}

/** Read actual Memories-owned migration state before exposing any recovery operation. */
export async function inspectEmbeddingMigration(): Promise<EmbeddingMigrationResult> {
  return invoke<EmbeddingMigrationResult>("inspect_embedding_migration");
}

export async function planEmbeddingChange(
  req: SetEmbeddingSettingsRequest,
): Promise<EmbeddingMigrationResult> {
  return invoke<EmbeddingMigrationResult>("plan_embedding_change", { req });
}

export async function switchEmbeddingSpace(req: {
  settings: SetEmbeddingSettingsRequest;
  expected_space: SpaceValue;
  backup: EmbeddingBackup;
}): Promise<EmbeddingMigrationOperationResponse> {
  return invoke<EmbeddingMigrationOperationResponse>("switch_embedding_space", { req });
}

export type EmbeddingMigrationRecoveryAction = "resume" | "finalize" | "restore" | "abandon";

export async function recoverEmbeddingMigration(req: {
  action: EmbeddingMigrationRecoveryAction;
  attempt_id: string;
  accept_failed?: number;
  settings?: SetEmbeddingSettingsRequest;
}): Promise<EmbeddingMigrationOperationResponse> {
  return invoke<EmbeddingMigrationOperationResponse>("recover_embedding_migration", { req });
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

/** Generate and persist an MCP bearer token, returning it for this display only. */
export async function generateMcpAuthToken(): Promise<string> {
  return invoke<string>("generate_mcp_auth_token");
}

/** Replace the MCP bearer token, returning the replacement for this display only. */
export async function rotateMcpAuthToken(): Promise<string> {
  return invoke<string>("rotate_mcp_auth_token");
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
// Recovery commands that restart return `RecoveryResult`. Database restore is
// deliberately separate: it leaves sidecars stopped until the user selects
// migration retry.

/** Reset a stored `preset_id` that is no longer in the curated list
 * (typically after a release that retired a preset) to `null`, then
 * restart. Does NOT touch lancedb. */
export function recoverResetEmbeddingSettings(): Promise<RecoveryResult> {
  return invoke<RecoveryResult>("recover_reset_embedding_settings");
}

export function retryDatabaseMigration(): Promise<RecoveryResult> {
  return invoke<RecoveryResult>("retry_database_migration");
}

export function restoreDatabaseMigrationBackup(backupPath: string): Promise<DatabaseRestoreResult> {
  return invoke<DatabaseRestoreResult>("restore_database_migration_backup", { backupPath });
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

export type CloseChoice = "quit" | "background" | "cancel";
export type BackgroundUnavailableReason = "unverified" | "unavailable" | "hide_failed";
export type CloseActionResult =
  | "accepted"
  | "backgroundUnavailable"
  | "hideFailed"
  | "staleRequest"
  | "invalidState";

export interface CloseChoiceRequest {
  requestId: number;
  windowLabel: string;
  backgroundAvailable: boolean;
  backgroundUnavailableReason: BackgroundUnavailableReason | null;
}

export async function registerCloseChoiceUi(locale: "ja" | "en"): Promise<{
  pendingRequest: CloseChoiceRequest | null;
}> {
  return invoke("register_close_choice_ui", { locale });
}

export async function syncCloseChoiceLocale(locale: "ja" | "en"): Promise<void> {
  return invoke("sync_close_choice_locale", { locale });
}

export async function acknowledgeCloseChoiceRequest(requestId: number): Promise<boolean> {
  return invoke("acknowledge_close_choice_request", { requestId });
}

export async function chooseCloseAction(
  requestId: number,
  choice: CloseChoice,
): Promise<CloseActionResult> {
  return invoke("choose_close_action", { requestId, choice });
}
