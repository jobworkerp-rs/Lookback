//! Tauri commands for the standalone "generate summary / personality"
//! buttons (run the analysis later, after an import-only run). Mirrors
//! `reflection_dispatch` but builds the workflow input via `import`'s
//! `BatchDispatch` so the wire-shape stays a single source of truth.
//!
//! Each command returns immediately with a `job_id_hint`; the stream is
//! consumed in a detached task that emits `summary://step` /
//! `personality://step` events `{ job_id, status, message }`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tokio_stream::StreamExt;

use crate::error::{AppError, AppResult};
use crate::jobworkerp::{
    JobworkerpHandle, StreamEvent, job_timeouts, run_cancellable_named_stream,
};

use super::import::{
    BatchDispatch, GenerateSummariesRequest, PeriodKind, PeriodRange, log_thread_group_failure,
    run_cancellable_step, staged_step_job_timeout_sec, summarize_workflow_chunk,
    summarize_workflow_error, terminal_batch_outcome, thread_group_summary_terminal_outcome,
    workflow_output_value, workflow_terminal_failure,
};
use super::staged_progress::{ProgressKind, StagedStep};
use super::thread_group_owner::{resolve_group_owner_user_id, validate_positive_user_id};
use super::{
    AppState, DispatchCancelEntry, GeneratedRefreshScope, StepOutcome, StepStatus,
    cancel_dispatch_inner, emit_event, emit_generated_refresh,
    generated_refresh_scopes_from_position,
};

// Worker names must match what the import pipeline dispatches
// (import.rs success path). Kept as constants here and guarded by a
// drift test against those literals. `pub(crate)` so the Settings queue
// card (`background_jobs.rs`) can classify counts by the same names
// instead of carrying its own copy that could drift.
pub(crate) const SUMMARY_WORKER_NAME: &str = "memories-summarize-batch";
pub(crate) const THREAD_GROUP_SUMMARY_WORKER_NAME: &str = "memories-thread-group-summary-batch";
pub(crate) const THREAD_GROUP_ISSUE_RESOLUTION_WORKER_NAME: &str =
    "lookback-thread-group-issue-resolution";
/// Same-thread resolution, awaited by the thread summary single and the
/// manual worker; named here only so the queue card can classify it.
pub(crate) const THREAD_ISSUE_RESOLUTION_WORKER_NAME: &str = "lookback-thread-issue-resolution";
pub(crate) const THREAD_ISSUE_RESOLUTION_MANUAL_WORKER_NAME: &str =
    "lookback-thread-issue-resolution-manual";
pub(crate) const PERSONALITY_WORKER_NAME: &str = "memories-personality-batch";
/// Base name of the Layer-2 merge lang-worker. The full worker name is
/// `<base>-<lang>` (`memories-user-personality-merge-ja` / `-en`), registered
/// by `memories-import upsert-generation-workers`. The Personality tab uses it
/// to skip per-thread fan-out and re-run the merge alone (e.g. after a 429
/// storm left signals but no profile). Same lang-worker the personality batch
/// resolves for its tail merge step.
pub(crate) const PERSONALITY_MERGE_WORKER_BASE: &str = "memories-user-personality-merge";
static SUMMARY_GENERATION_ADMISSION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone)]
struct PeriodRecoveryContext {
    period_kind: PeriodKind,
    memories_endpoint_id: String,
    source_scope_digest: String,
    generation_id: String,
    generation_started_at: String,
    /// The batch's implicit target cannot be reconstructed from a failed
    /// workflow stream. Keep the resolved fallback periods so strict
    /// `raise` failures remain recoverable without parsing error prose.
    fallback_period_keys: Vec<String>,
}

#[derive(Debug, Clone)]
struct ThreadGroupRecoveryContext {
    memories_endpoint_id: String,
    target_group_id: Option<i64>,
    expected_membership_snapshot_digest: Option<String>,
    generation_id: String,
}

#[derive(Debug, Clone)]
enum SummaryRecoveryContext {
    Period(PeriodRecoveryContext),
    ThreadGroup(ThreadGroupRecoveryContext),
}
const SUMMARY_EVENT: &str = "summary://step";
const PERSONALITY_EVENT: &str = "personality://step";

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EnqueueSummaryJobRequest {
    pub user_id: Option<i64>,
    /// Optional epoch-ms lower bound on the source thread's last message.
    /// Unset = all threads for the user (subject to single-workflow
    /// eligibility checks).
    pub last_message_after_ms: Option<i64>,
    /// Optional inclusive epoch-ms upper bound. Unset = no upper bound.
    pub last_message_before_ms: Option<i64>,
    /// Optional client-supplied dispatch id used as the cancel key. Older
    /// callers (the legacy frontend before cancel landed) omit it; we
    /// then synthesize a timestamp-shaped one as before.
    pub dispatch_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EnqueueThreadGroupSummaryJobRequest {
    pub user_id: Option<i64>,
    pub group_owner_user_id: Option<i64>,
    pub force_resummarize: Option<bool>,
    pub dispatch_id: Option<String>,
    #[serde(default, with = "crate::serde_id::option")]
    pub target_group_id: Option<i64>,
    pub expected_membership_snapshot_digest: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnqueueThreadGroupIssueResolutionJobRequest {
    #[serde(with = "crate::serde_id")]
    pub target_group_id: i64,
    pub group_owner_user_id: i64,
    pub expected_membership_snapshot_digest: String,
    pub dispatch_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnqueueThreadIssueResolutionJobRequest {
    pub target_thread_id: String,
    pub dispatch_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetThreadIssueResolutionStatusRequest {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ThreadIssueResolutionStatus {
    pub status: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EnqueuePersonalityJobRequest {
    pub user_id: Option<i64>,
    pub last_message_after_ms: Option<i64>,
    /// When true, ignore both `existing_signal` skips in
    /// thread-personality-single AND the target_signal_count short-circuit
    /// in thread-personality-batch, re-extracting every eligible source
    /// thread. Surfaced through the Personality tab's "Force 再抽出"
    /// checkbox so a prompt change can be applied to the historical set.
    pub force_reextract: Option<bool>,
    /// See [`EnqueueSummaryJobRequest::dispatch_id`].
    pub dispatch_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EnqueuePersonalityMergeJobRequest {
    pub user_id: Option<i64>,
    /// Bypass the merge YAML's eligibility short-circuit
    /// (`max(signal.updated_at) <= profile.updated_at` ⇒ skip). The
    /// re-extract path stamps signals with the source thread's
    /// `updated_at`, so re-running per-thread on unchanged threads leaves
    /// the max untouched and the merge would no-op without this; the
    /// Personality tab's Force checkbox sets this flag.
    pub force_remerge: Option<bool>,
    /// See [`EnqueueSummaryJobRequest::dispatch_id`].
    pub dispatch_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnqueuePeriodSummaryJobRequest {
    pub kind: PeriodKind,
    /// Back-fill the last N periods (days/weeks/months). `None` = let the
    /// batch fall back to "last completed period only".
    pub last_n: Option<i32>,
    /// Keep the standalone endpoint consistent with the staged generation
    /// dialog. Missing legacy clients retain best-effort execution.
    #[serde(default)]
    pub stop_on_error: bool,
    /// See [`EnqueueSummaryJobRequest::dispatch_id`].
    pub dispatch_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnqueueAnalysisJobResponse {
    /// Frontend correlates the `*://step` events with the progress slot it
    /// just opened. Synthesized on dispatch so we can return before the
    /// gRPC enqueue completes.
    pub job_id_hint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnalysisStepUpdate {
    pub job_id: String,
    pub status: StepStatus,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
enum RefreshMode {
    Terminal(Vec<GeneratedRefreshScope>),
    ThreadGroupSummary,
    TargetedReextract { thread_id: String },
}

#[tauri::command]
pub async fn enqueue_summary_job(
    app: AppHandle,
    state: State<'_, AppState>,
    req: EnqueueSummaryJobRequest,
) -> AppResult<EnqueueAnalysisJobResponse> {
    let dispatch_admission = state.lock_llm_settings_dispatch_gate().await;
    // Generation writes summary / personality embeddings into the local
    // LanceDB; refuse when it is degraded (local mode only).
    state.ensure_local_embedding_available()?;
    let callback = state.resolve_targets()?.memories_callback()?;
    let user_id = req.user_id.unwrap_or(1);
    let dispatch = BatchDispatch::resolve_with_window(
        &callback,
        user_id,
        req.last_message_after_ms,
        req.last_message_before_ms,
        state.active_llm_worker_name().to_string(),
        state.active_output_language(),
    )?;
    let args = super::wrap_workflow_run_args(&dispatch.summarize_input());

    let handle = state
        .jobworkerp_with_timeout(job_timeouts::THREAD_SUMMARY_BATCH)
        .await?;
    let job_id = resolve_dispatch_id(req.dispatch_id.as_deref(), "summary");
    let maintenance_registered = super::begin_search_index_write(
        &state,
        &[
            crate::search_index_maintenance::MaintenanceTable::Memory,
            crate::search_index_maintenance::MaintenanceTable::Thread,
        ],
    )
    .await?;
    let entry = state.dispatch_register_admitted(&job_id).await;
    drop(dispatch_admission);
    spawn_step_stream(
        app,
        handle,
        SUMMARY_WORKER_NAME.to_string(),
        args,
        &job_id,
        SUMMARY_EVENT,
        entry,
        RefreshMode::Terminal(vec![GeneratedRefreshScope::ThreadSummary]),
        None,
        maintenance_registered,
    );

    Ok(EnqueueAnalysisJobResponse {
        job_id_hint: job_id,
    })
}

#[tauri::command]
pub async fn enqueue_thread_group_summary_job(
    app: AppHandle,
    state: State<'_, AppState>,
    req: EnqueueThreadGroupSummaryJobRequest,
) -> AppResult<EnqueueAnalysisJobResponse> {
    let dispatch_admission = state.lock_llm_settings_dispatch_gate().await;
    let group_owner_user_id = resolve_group_owner_user_id(req.group_owner_user_id, req.user_id)?;
    super::thread_group_recovery::ensure_healthy_for_dispatch(
        &state.data.thread_group_recovery_path(),
    )?;
    match (
        req.target_group_id,
        req.expected_membership_snapshot_digest.as_deref(),
    ) {
        (Some(_), Some(digest)) if !digest.trim().is_empty() => {}
        (None, None) => {}
        (Some(_), _) => {
            return Err(AppError::Config(
                "target_group_id requires expected_membership_snapshot_digest".into(),
            ));
        }
        (None, Some(_)) => {
            return Err(AppError::Config(
                "expected_membership_snapshot_digest requires target_group_id".into(),
            ));
        }
    }
    let targets = state.resolve_targets()?;
    let callback = targets.memories_callback()?;
    super::connection::require_thread_group_owner_capability_at_url(&targets.memories_url).await?;
    state.ensure_local_embedding_available()?;
    let dispatch = BatchDispatch::resolve_with_window(
        &callback,
        group_owner_user_id,
        None,
        None,
        state.active_llm_worker_name().to_string(),
        state.active_output_language(),
    )?;
    let args = thread_group_summary_dispatch_input(
        dispatch.summarize_input_with(req.force_resummarize.unwrap_or(false)),
        &req,
        group_owner_user_id,
    );
    let args = super::wrap_workflow_run_args(&args);
    let job_id = resolve_dispatch_id(req.dispatch_id.as_deref(), "thread-group-summary");
    let handle = state
        .jobworkerp_with_timeout(job_timeouts::THREAD_GROUP_SUMMARY_BATCH)
        .await?;
    let maintenance_registered = super::begin_search_index_write(
        &state,
        &[
            crate::search_index_maintenance::MaintenanceTable::Memory,
            crate::search_index_maintenance::MaintenanceTable::Thread,
        ],
    )
    .await?;
    let entry = state.dispatch_register_admitted(&job_id).await;
    drop(dispatch_admission);
    spawn_step_stream(
        app,
        handle,
        THREAD_GROUP_SUMMARY_WORKER_NAME.to_string(),
        args,
        &job_id,
        SUMMARY_EVENT,
        entry,
        RefreshMode::ThreadGroupSummary,
        Some(SummaryRecoveryContext::ThreadGroup(
            ThreadGroupRecoveryContext {
                memories_endpoint_id: format!("{}:{}", callback.host, callback.port),
                target_group_id: req.target_group_id,
                expected_membership_snapshot_digest: req.expected_membership_snapshot_digest,
                generation_id: job_id.clone(),
            },
        )),
        maintenance_registered,
    );
    Ok(EnqueueAnalysisJobResponse {
        job_id_hint: job_id,
    })
}

#[tauri::command]
pub async fn enqueue_thread_group_issue_resolution_job(
    app: AppHandle,
    state: State<'_, AppState>,
    req: EnqueueThreadGroupIssueResolutionJobRequest,
) -> AppResult<EnqueueAnalysisJobResponse> {
    let dispatch_admission = state.lock_llm_settings_dispatch_gate().await;
    validate_thread_group_issue_resolution_request(&req)?;
    let targets = state.resolve_targets()?;
    super::connection::require_thread_group_owner_capability_at_url(&targets.memories_url).await?;
    let callback = targets.memories_callback()?;
    let dispatch = BatchDispatch::resolve_with_window(
        &callback,
        1,
        None,
        None,
        state.active_llm_worker_name().to_string(),
        state.active_output_language(),
    )?;
    let args = thread_group_issue_resolution_dispatch_input(dispatch.summarize_input(), &req);
    let args = super::wrap_workflow_run_args(&args);
    let job_id = resolve_dispatch_id(req.dispatch_id.as_deref(), "thread-group-issue-resolution");
    let handle = state
        .jobworkerp_with_timeout(job_timeouts::THREAD_GROUP_ISSUE_RESOLUTION)
        .await?;
    let entry = state.dispatch_register_admitted(&job_id).await;
    drop(dispatch_admission);
    spawn_step_stream(
        app,
        handle,
        THREAD_GROUP_ISSUE_RESOLUTION_WORKER_NAME.to_string(),
        args,
        &job_id,
        SUMMARY_EVENT,
        entry,
        RefreshMode::Terminal(Vec::new()),
        None,
        false,
    );
    Ok(EnqueueAnalysisJobResponse {
        job_id_hint: job_id,
    })
}

/// Re-evaluate an existing thread summary without generating or writing a new
/// summary. This explicit manual path is not replayed after an app restart.
#[tauri::command]
pub async fn enqueue_thread_issue_resolution_job(
    app: AppHandle,
    state: State<'_, AppState>,
    req: EnqueueThreadIssueResolutionJobRequest,
) -> AppResult<EnqueueAnalysisJobResponse> {
    let dispatch_admission = state.lock_llm_settings_dispatch_gate().await;
    let thread_id = validate_thread_issue_resolution_request(&req)?;
    let issue_tracking_enabled =
        crate::commands::issue_tracker_settings::effective_issue_tracking_enabled(&state.data);
    let policy_json =
        thread_issue_resolution_policy_json(&state.data.root, issue_tracking_enabled)?;
    state
        .issue_tracker_availability
        .require_endpoint_connected()?;

    let targets = state.resolve_targets()?;
    let memories_callback = targets.memories_callback()?;
    // The workflow dials the tracker from jobworkerp, so use the address as
    // jobworkerp reaches it rather than the app's.
    let tracker_callback = crate::sidecar::lifecycle::published_issue_tracker_for_jobworkerp()
        .ok_or_else(|| {
            AppError::SidecarNotReady("Issue Tracker server endpoint is unavailable".into())
        })?;
    let args = thread_issue_resolution_dispatch_input(
        &thread_id,
        &memories_callback,
        &tracker_callback,
        &policy_json,
        state.active_llm_worker_name(),
        &state.active_output_language(),
    );
    let args = super::wrap_workflow_run_args(&args);
    let job_id = resolve_dispatch_id(req.dispatch_id.as_deref(), "thread-issue-resolution");
    let handle = state
        .jobworkerp_with_timeout(job_timeouts::THREAD_ISSUE_RESOLUTION_MANUAL)
        .await?;
    let entry = state.dispatch_register_admitted(&job_id).await;
    drop(dispatch_admission);
    spawn_step_stream(
        app,
        handle,
        THREAD_ISSUE_RESOLUTION_MANUAL_WORKER_NAME.to_string(),
        args,
        &job_id,
        SUMMARY_EVENT,
        entry,
        RefreshMode::Terminal(Vec::new()),
        None,
        false,
    );
    Ok(EnqueueAnalysisJobResponse {
        job_id_hint: job_id,
    })
}

/// Read the last resolution status stored on the thread's existing summary.
/// A pending value is intentionally surfaced rather than replayed because
/// queued jobworkerp work is not durable across app restarts.
#[tauri::command]
pub async fn get_thread_issue_resolution_status(
    state: State<'_, AppState>,
    req: GetThreadIssueResolutionStatusRequest,
) -> AppResult<ThreadIssueResolutionStatus> {
    let thread_id = validate_positive_thread_id(&req.thread_id)?;
    let mut client =
        crate::grpc::proto::llm_memory::service::memory_service_client::MemoryServiceClient::new(
            state.memories_channel().await?,
        );
    let request = crate::grpc::proto::llm_memory::service::FindMemoryListRequest {
        limit: Some(2),
        offset: None,
        roles: vec![],
        user_id: Some(crate::grpc::proto::llm_memory::data::UserId { value: 1 }),
        thread_id: None,
        updated_after: None,
        updated_before: None,
        external_id: Some(format!("summary:{thread_id}")),
        content_types: vec![],
        thread_filter: None,
        created_after: None,
        created_before: None,
        sort: None,
        external_id_prefix: None,
        memory_kinds: vec![crate::grpc::proto::llm_memory::data::MemoryKind::ThreadSummary as i32],
    };
    let mut stream = client.find_list_by_condition(request).await?.into_inner();
    let mut summary_metadata = None;
    let mut summary_count = 0;
    while let Some(item) = stream.next().await {
        let Some(memory) = item?.memory else {
            continue;
        };
        summary_count += 1;
        if summary_count > 1 {
            break;
        }
        summary_metadata = memory.data.and_then(|data| data.metadata);
    }
    let status = thread_issue_resolution_status(summary_count, summary_metadata.as_deref());
    Ok(ThreadIssueResolutionStatus { status })
}

/// Re-runs extraction for one source thread while explicitly lifting only the
/// selected rejected Candidate's suppression.  It reuses the normal summary
/// dispatch lifecycle so cancellation, status events, and index maintenance
/// remain identical to a user-triggered summary run.
pub(crate) async fn enqueue_targeted_summary_reextract(
    app: AppHandle,
    state: &AppState,
    thread_id: String,
    candidate_id: String,
) -> AppResult<EnqueueAnalysisJobResponse> {
    let dispatch_admission = state.lock_llm_settings_dispatch_gate().await;
    state.ensure_local_embedding_available()?;
    let callback = state.resolve_targets()?.memories_callback()?;
    let target_thread_id = thread_id.clone();
    let dispatch = BatchDispatch::resolve(
        &callback,
        1,
        None,
        state.active_llm_worker_name().to_string(),
        state.active_output_language(),
    )?
    .target_only(thread_id, candidate_id);
    let args = super::wrap_workflow_run_args(&dispatch.summarize_input_with(true));
    let handle = state
        .jobworkerp_with_timeout(job_timeouts::THREAD_SUMMARY_BATCH)
        .await?;
    let job_id = resolve_dispatch_id(None, "summary-reextract");
    let maintenance_registered = super::begin_search_index_write(
        state,
        &[
            crate::search_index_maintenance::MaintenanceTable::Memory,
            crate::search_index_maintenance::MaintenanceTable::Thread,
        ],
    )
    .await?;
    let entry = state.dispatch_register_admitted(&job_id).await;
    drop(dispatch_admission);
    spawn_step_stream(
        app,
        handle,
        SUMMARY_WORKER_NAME.to_string(),
        args,
        &job_id,
        SUMMARY_EVENT,
        entry,
        RefreshMode::TargetedReextract {
            thread_id: target_thread_id,
        },
        None,
        maintenance_registered,
    );
    Ok(EnqueueAnalysisJobResponse {
        job_id_hint: job_id,
    })
}

#[tauri::command]
pub async fn enqueue_personality_job(
    app: AppHandle,
    state: State<'_, AppState>,
    req: EnqueuePersonalityJobRequest,
) -> AppResult<EnqueueAnalysisJobResponse> {
    let dispatch_admission = state.lock_llm_settings_dispatch_gate().await;
    // Generation writes summary / personality embeddings into the local
    // LanceDB; refuse when it is degraded (local mode only).
    state.ensure_local_embedding_available()?;
    let callback = state.resolve_targets()?.memories_callback()?;
    let user_id = req.user_id.unwrap_or(1);
    let dispatch = BatchDispatch::resolve(
        &callback,
        user_id,
        req.last_message_after_ms,
        state.active_llm_worker_name().to_string(),
        state.active_output_language(),
    )?;
    let args = super::wrap_workflow_run_args(
        &dispatch.personality_input_with(req.force_reextract.unwrap_or(false)),
    );

    let handle = state
        .jobworkerp_with_timeout(job_timeouts::PERSONALITY_BATCH)
        .await?;
    let job_id = resolve_dispatch_id(req.dispatch_id.as_deref(), "personality");
    let maintenance_registered = super::begin_search_index_write(
        &state,
        &[
            crate::search_index_maintenance::MaintenanceTable::Memory,
            crate::search_index_maintenance::MaintenanceTable::Thread,
        ],
    )
    .await?;
    let entry = state.dispatch_register_admitted(&job_id).await;
    drop(dispatch_admission);
    spawn_step_stream(
        app,
        handle,
        PERSONALITY_WORKER_NAME.to_string(),
        args,
        &job_id,
        PERSONALITY_EVENT,
        entry,
        RefreshMode::Terminal(vec![GeneratedRefreshScope::Personality]),
        None,
        maintenance_registered,
    );

    Ok(EnqueueAnalysisJobResponse {
        job_id_hint: job_id,
    })
}

/// Dispatch the standalone Layer-2 merge (`memories-user-personality-merge-<lang>`).
/// Skips per-thread fan-out entirely and re-runs the merge against the
/// existing layer-1 signals. Surfaced as a separate Personality-tab button
/// because the normal "generate" path runs per-thread first, which is the
/// step a 429 storm corrupts — leaving the merge orphaned with no path to
/// trigger it alone.
///
/// Progress streams on the shared `personality://step` event so the existing
/// Personality progress hook surfaces it without an extra wire.
#[tauri::command]
pub async fn enqueue_personality_merge_job(
    app: AppHandle,
    state: State<'_, AppState>,
    req: EnqueuePersonalityMergeJobRequest,
) -> AppResult<EnqueueAnalysisJobResponse> {
    let dispatch_admission = state.lock_llm_settings_dispatch_gate().await;
    // Generation writes summary / personality embeddings into the local
    // LanceDB; refuse when it is degraded (local mode only).
    state.ensure_local_embedding_available()?;
    let callback = state.resolve_targets()?.memories_callback()?;
    let user_id = req.user_id.unwrap_or(1);
    let output_language = state.active_output_language();
    // The standalone merge dispatches the language-specific merge lang-worker
    // by name (the same one the batch resolves for its tail step), so a
    // merge-only run honors the UI language like every other generation path.
    let merge_worker_name = format!("{PERSONALITY_MERGE_WORKER_BASE}-{output_language}");
    let dispatch = BatchDispatch::resolve(
        &callback,
        user_id,
        None,
        state.active_llm_worker_name().to_string(),
        output_language,
    )?;
    let args = super::wrap_workflow_run_args(
        &dispatch.merge_only_input(req.force_remerge.unwrap_or(false)),
    );

    let handle = state
        .jobworkerp_with_timeout(job_timeouts::PERSONALITY_MERGE)
        .await?;
    let job_id = resolve_dispatch_id(req.dispatch_id.as_deref(), "personality-merge");
    let maintenance_registered = super::begin_search_index_write(
        &state,
        &[
            crate::search_index_maintenance::MaintenanceTable::Memory,
            crate::search_index_maintenance::MaintenanceTable::Thread,
        ],
    )
    .await?;
    let entry = state.dispatch_register_admitted(&job_id).await;
    drop(dispatch_admission);
    spawn_step_stream(
        app,
        handle,
        merge_worker_name,
        args,
        &job_id,
        PERSONALITY_EVENT,
        entry,
        RefreshMode::Terminal(vec![GeneratedRefreshScope::Personality]),
        None,
        maintenance_registered,
    );

    Ok(EnqueueAnalysisJobResponse {
        job_id_hint: job_id,
    })
}

/// Dispatch a period (daily/weekly/monthly) work-summary batch. Unlike the
/// per-thread summary, these read/write the synthetic summary owner and are
/// scoped by `last_n`, so they go through `BatchDispatch::period_input`. The
/// caller is responsible for the layer order (daily before weekly before
/// monthly); the workflow no-ops when the source layer is missing.
///
/// Progress reuses the `summary://step` event so the existing summary progress
/// slot surfaces it without an extra hook.
#[tauri::command]
pub async fn enqueue_period_summary_job(
    app: AppHandle,
    state: State<'_, AppState>,
    req: EnqueuePeriodSummaryJobRequest,
) -> AppResult<EnqueueAnalysisJobResponse> {
    let dispatch_admission = state.lock_llm_settings_dispatch_gate().await;
    super::summary_recovery::ensure_healthy(&state.data.summary_recovery_path())?;
    // Generation writes summary / personality embeddings into the local
    // LanceDB; refuse when it is degraded (local mode only).
    state.ensure_local_embedding_available()?;
    let callback = state.resolve_targets()?.memories_callback()?;
    // Period summaries always operate on the synthetic owner; the importing
    // user_id is irrelevant here, so resolve with a placeholder.
    let dispatch = BatchDispatch::resolve_for_period_summaries(
        &callback,
        1,
        None,
        state.active_llm_worker_name().to_string(),
        state.active_output_language(),
    )?;
    let range = req.last_n.map_or(PeriodRange::Auto, PeriodRange::LastN);
    let job_id = resolve_dispatch_id(req.dispatch_id.as_deref(), req.kind.job_prefix());
    let period_input = dispatch.period_input(req.kind, range, req.stop_on_error, Some(&job_id));
    let args = super::wrap_workflow_run_args(&period_input);

    let handle = state
        .jobworkerp_with_timeout(req.kind.job_timeout_sec())
        .await?;
    let recovery_context = PeriodRecoveryContext {
        period_kind: req.kind,
        memories_endpoint_id: format!("{}:{}", callback.host, callback.port),
        source_scope_digest: crate::commands::summaries::source_scope_digest(
            &[],
            Some("LABEL_ALL"),
        ),
        generation_id: job_id.clone(),
        generation_started_at: period_input
            .get("generation_started_at")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        fallback_period_keys: fallback_period_keys(
            req.kind,
            req.last_n,
            workflow_timezone(&state)?,
        ),
    };
    let maintenance_registered = super::begin_search_index_write(
        &state,
        &[
            crate::search_index_maintenance::MaintenanceTable::Memory,
            crate::search_index_maintenance::MaintenanceTable::Thread,
        ],
    )
    .await?;
    let entry = state.dispatch_register_admitted(&job_id).await;
    drop(dispatch_admission);
    spawn_step_stream(
        app,
        handle,
        req.kind.worker_name().to_string(),
        args,
        &job_id,
        SUMMARY_EVENT,
        entry,
        RefreshMode::Terminal(vec![period_refresh_scope(req.kind)]),
        Some(SummaryRecoveryContext::Period(recovery_context)),
        maintenance_registered,
    );

    Ok(EnqueueAnalysisJobResponse {
        job_id_hint: job_id,
    })
}

/// Dispatch each selected summary-generation batch as an individually visible
/// and cancellable staged step.
#[tauri::command]
pub async fn generate_summaries(
    app: AppHandle,
    state: State<'_, AppState>,
    req: GenerateSummariesRequest,
) -> AppResult<EnqueueAnalysisJobResponse> {
    let dispatch_admission = state.lock_llm_settings_dispatch_gate().await;
    ensure_summary_dispatch_recoveries_are_healthy(
        &state.data.summary_recovery_path(),
        &state.data.thread_group_recovery_path(),
        req.run_thread_group,
    )?;
    // Generation writes summary / personality embeddings into the local
    // LanceDB; refuse when it is degraded (local mode only).
    state.ensure_local_embedding_available()?;
    tracing::info!(
        user_id = req.user_id.unwrap_or(1),
        run_per_thread = req.run_per_thread,
        run_daily = req.run_daily,
        run_weekly = req.run_weekly,
        run_monthly = req.run_monthly,
        daily_start = %req.daily_start,
        daily_end = %req.daily_end,
        weekly_start = %req.weekly_start,
        weekly_end = %req.weekly_end,
        monthly_start = %req.monthly_start,
        monthly_end = %req.monthly_end,
        "received summaries generation request"
    );
    let callback = state.resolve_targets()?.memories_callback()?;
    let user_id = req.user_id.unwrap_or(1);
    let has_period_summary = req.run_daily || req.run_weekly || req.run_monthly;
    let dispatch = BatchDispatch::resolve_for_summary_generation(
        &callback,
        user_id,
        req.last_message_after_ms,
        req.last_message_before_ms,
        state.active_llm_worker_name().to_string(),
        state.active_output_language(),
        has_period_summary,
    )?;
    let job_id = resolve_dispatch_id(req.dispatch_id.as_deref(), "summaries");
    let metadata = dispatch.summary_generation_metadata(req.dispatch_id.as_deref());
    let timezone = has_period_summary
        .then(|| workflow_timezone(&state))
        .transpose()?;
    let memories_endpoint_id = format!("{}:{}", callback.host, callback.port);
    let mut stages = Vec::with_capacity(5);
    for SummaryGenerationStageSpec {
        step,
        selected,
        worker_name,
        period_kind,
    } in summary_generation_stage_specs(&req)
    {
        let input = selected.then(|| {
            dispatch
                .summary_generation_input(step, &req, &metadata)
                .expect("summary generation stage has an input builder")
        });
        let recovery_context = if !selected {
            None
        } else if step == StagedStep::ThreadGroupSummary {
            Some(SummaryRecoveryContext::ThreadGroup(
                ThreadGroupRecoveryContext {
                    memories_endpoint_id: memories_endpoint_id.clone(),
                    target_group_id: None,
                    expected_membership_snapshot_digest: None,
                    generation_id: metadata.generation_id.clone(),
                },
            ))
        } else if let Some(period_kind) = period_kind {
            let timezone = timezone.expect("period timezone resolved when a period runs");
            Some(SummaryRecoveryContext::Period(PeriodRecoveryContext {
                period_kind,
                memories_endpoint_id: memories_endpoint_id.clone(),
                source_scope_digest: metadata.source_scope_digest.clone(),
                generation_id: metadata.generation_id.clone(),
                generation_started_at: metadata.generation_started_at.clone(),
                fallback_period_keys: summary_generation_fallback_period_keys(
                    period_kind,
                    &req,
                    timezone,
                ),
            }))
        } else {
            None
        };
        stages.push(SummaryGenerationStage {
            step,
            selected,
            worker_name: worker_name.to_string(),
            input,
            timeout_sec: staged_step_job_timeout_sec(step),
            recovery_context,
        });
    }

    let handle = state
        .jobworkerp_with_timeout(job_timeouts::STAGED_GENERATION_CLIENT)
        .await?;
    let maintenance_registered = if has_period_summary || req.run_per_thread || req.run_thread_group
    {
        super::begin_search_index_write(
            &state,
            &[
                crate::search_index_maintenance::MaintenanceTable::Memory,
                crate::search_index_maintenance::MaintenanceTable::Thread,
            ],
        )
        .await?
    } else {
        false
    };
    let entry = state.dispatch_register_admitted(&job_id).await;
    state.import_progress.start_execution(
        ProgressKind::SummaryGeneration,
        job_id.clone(),
        stages.iter().map(|stage| stage.step),
    );
    drop(dispatch_admission);
    spawn_summary_generation(
        app,
        handle,
        stages,
        req.stop_on_error,
        &job_id,
        entry,
        maintenance_registered,
    );

    Ok(EnqueueAnalysisJobResponse {
        job_id_hint: job_id,
    })
}

#[derive(Debug)]
struct SummaryGenerationStage {
    step: StagedStep,
    selected: bool,
    worker_name: String,
    input: Option<serde_json::Value>,
    timeout_sec: Option<u64>,
    recovery_context: Option<SummaryRecoveryContext>,
}

#[derive(Debug, Clone, Copy)]
struct SummaryGenerationStageSpec {
    step: StagedStep,
    selected: bool,
    worker_name: &'static str,
    period_kind: Option<PeriodKind>,
}

fn summary_generation_stage_specs(
    req: &GenerateSummariesRequest,
) -> [SummaryGenerationStageSpec; 5] {
    [
        SummaryGenerationStageSpec {
            step: StagedStep::ThreadSummary,
            selected: req.run_per_thread,
            worker_name: SUMMARY_WORKER_NAME,
            period_kind: None,
        },
        SummaryGenerationStageSpec {
            step: StagedStep::ThreadGroupSummary,
            selected: req.run_thread_group,
            worker_name: THREAD_GROUP_SUMMARY_WORKER_NAME,
            period_kind: None,
        },
        SummaryGenerationStageSpec {
            step: StagedStep::Daily,
            selected: req.run_daily,
            worker_name: PeriodKind::Daily.worker_name(),
            period_kind: Some(PeriodKind::Daily),
        },
        SummaryGenerationStageSpec {
            step: StagedStep::Weekly,
            selected: req.run_weekly,
            worker_name: PeriodKind::Weekly.worker_name(),
            period_kind: Some(PeriodKind::Weekly),
        },
        SummaryGenerationStageSpec {
            step: StagedStep::Monthly,
            selected: req.run_monthly,
            worker_name: PeriodKind::Monthly.worker_name(),
            period_kind: Some(PeriodKind::Monthly),
        },
    ]
}

fn summary_generation_fallback_period_keys(
    kind: PeriodKind,
    req: &GenerateSummariesRequest,
    timezone: chrono_tz::Tz,
) -> Vec<String> {
    let (start, end) = match kind {
        PeriodKind::Daily => (&req.daily_start, &req.daily_end),
        PeriodKind::Weekly => (&req.weekly_start, &req.weekly_end),
        PeriodKind::Monthly => (&req.monthly_start, &req.monthly_end),
    };
    super::summary_calendar::explicit_periods(calendar_period_kind(kind), start, end)
        .unwrap_or_else(|| fallback_period_keys(kind, None, timezone))
}

fn summary_step_label(step: StagedStep) -> &'static str {
    match step {
        StagedStep::ThreadSummary => "スレッド要約",
        StagedStep::ThreadGroupSummary => "ThreadGroup 要約",
        StagedStep::Daily => "日次要約",
        StagedStep::Weekly => "週次要約",
        StagedStep::Monthly => "月次要約",
        StagedStep::ThreadImport => "import",
        StagedStep::ThreadPersonality => "パーソナリティ",
        StagedStep::Reflection => "自省",
    }
}

fn summary_stage_should_stop(outcome: &StepOutcome, has_reported_failures: bool) -> bool {
    outcome.status == StepStatus::Failed || has_reported_failures
}

fn emit_summary_analysis_update(
    app: &AppHandle,
    job_id: &str,
    status: StepStatus,
    message: Option<String>,
) {
    emit_event(
        app,
        SUMMARY_EVENT,
        AnalysisStepUpdate {
            job_id: job_id.to_string(),
            status,
            message,
        },
    );
}

fn emit_summary_cancelled_steps(app: &AppHandle, job_id: &str, stages: &[SummaryGenerationStage]) {
    for stage in stages {
        if stage.selected {
            super::import::emit_staged_step(
                app,
                ProgressKind::SummaryGeneration,
                job_id,
                stage.step,
                StepStatus::Cancelled,
                Some("中断によりスキップ".into()),
                None,
            );
        } else {
            super::import::emit_staged_step(
                app,
                ProgressKind::SummaryGeneration,
                job_id,
                stage.step,
                StepStatus::Waiting,
                Some("スキップ".into()),
                None,
            );
        }
    }
}

fn summary_overall_outcome(
    stages: &[SummaryGenerationStage],
    results: &[(StagedStep, StepOutcome)],
    cancelled: bool,
) -> StepOutcome {
    if cancelled {
        return StepOutcome {
            status: StepStatus::Cancelled,
            message: Some("中断".into()),
        };
    }
    if !stages.iter().any(|stage| stage.selected) {
        return StepOutcome {
            status: StepStatus::Done,
            message: Some("実行対象の段はありません".into()),
        };
    }
    if results
        .iter()
        .all(|(_, outcome)| outcome.status == StepStatus::Done)
    {
        return StepOutcome {
            status: StepStatus::Done,
            message: None,
        };
    }
    let details = results
        .iter()
        .filter(|(_, outcome)| outcome.status != StepStatus::Done)
        .map(|(step, outcome)| {
            let detail = outcome
                .message
                .as_deref()
                .map(str::trim)
                .filter(|message| !message.is_empty())
                .unwrap_or("失敗");
            format!("{}: {}", summary_step_label(*step), detail)
        })
        .collect::<Vec<_>>();
    let any_success = results
        .iter()
        .any(|(_, outcome)| outcome.status == StepStatus::Done);
    let any_warning = results
        .iter()
        .any(|(_, outcome)| outcome.status == StepStatus::Warning);
    StepOutcome {
        status: if any_success || any_warning {
            StepStatus::Warning
        } else {
            StepStatus::Failed
        },
        message: Some(format!("一部失敗: {}", details.join(" / "))),
    }
}

async fn record_or_reconcile_summary_stage_recovery(
    app: &AppHandle,
    job_id: &str,
    context: &SummaryRecoveryContext,
    result: &super::import::CancellableStepResult,
    cancel: &tokio_util::sync::CancellationToken,
) {
    if cancel.is_cancelled() {
        return;
    }
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let terminal_raw = result.terminal_output.as_deref();
    let recovered = match (context, terminal_raw) {
        (SummaryRecoveryContext::Period(period), Some(raw)) => {
            let _guard = state.summary_recovery.lock().await;
            reconcile_period_recovery(&state.data.summary_recovery_path(), period, raw)
        }
        (SummaryRecoveryContext::Period(period), None) if result.stream_failed => {
            let _guard = state.summary_recovery.lock().await;
            record_failed_period_stream_recovery(&state.data.summary_recovery_path(), period)
        }
        (SummaryRecoveryContext::ThreadGroup(group), Some(raw)) => {
            let _guard = state.thread_group_recovery.lock().await;
            reconcile_thread_group_recovery(&state.data.thread_group_recovery_path(), group, raw)
        }
        (SummaryRecoveryContext::ThreadGroup(group), None) if result.stream_failed => {
            let _guard = state.thread_group_recovery.lock().await;
            record_failed_thread_group_stream_recovery(
                &state.data.thread_group_recovery_path(),
                group,
            )
        }
        _ => Ok(()),
    };
    if let Err(error) = recovered {
        tracing::warn!(%error, "could not persist staged summary recovery diagnostics");
    }
    if let Some(raw) = terminal_raw
        && let Err(error) = materialize_failed_overall_attempts(&state, context, raw).await
    {
        tracing::warn!(%error, "could not persist failed overall attempt metadata");
    }
    emit_generated_refresh(app, job_id, recovery_refresh_scopes(context));
}

fn spawn_summary_generation(
    app: AppHandle,
    handle: JobworkerpHandle,
    stages: Vec<SummaryGenerationStage>,
    stop_on_error: bool,
    job_id: &str,
    entry: DispatchCancelEntry,
    maintenance_registered: bool,
) {
    let job_id = job_id.to_string();
    tokio::spawn(async move {
        let cancel = entry.token.clone();
        let current_job_id = entry.current_job_id.clone();
        let admission = tokio::select! {
            _ = cancel.cancelled() => None,
            guard = SUMMARY_GENERATION_ADMISSION.lock() => Some(guard),
        };
        let mut results = Vec::new();
        let mut cancelled = admission.is_none() || cancel.is_cancelled();
        if cancelled {
            emit_summary_cancelled_steps(&app, &job_id, &stages);
        } else {
            let _admission = admission.expect("summary admission acquired");
            let mut stop_after_failure = false;
            for (index, stage) in stages.iter().enumerate() {
                if !stage.selected {
                    super::import::emit_staged_step(
                        &app,
                        ProgressKind::SummaryGeneration,
                        &job_id,
                        stage.step,
                        StepStatus::Waiting,
                        Some("スキップ".into()),
                        None,
                    );
                    continue;
                }
                if cancel.is_cancelled() {
                    cancelled = true;
                    emit_summary_cancelled_steps(&app, &job_id, &stages[index..]);
                    break;
                }
                if stop_after_failure {
                    super::import::emit_staged_step(
                        &app,
                        ProgressKind::SummaryGeneration,
                        &job_id,
                        stage.step,
                        StepStatus::Waiting,
                        Some("中断によりスキップ".into()),
                        None,
                    );
                    continue;
                }
                emit_summary_analysis_update(
                    &app,
                    &job_id,
                    StepStatus::Active,
                    Some(format!("{}を実行中", summary_step_label(stage.step))),
                );
                let result = run_cancellable_step(
                    &app,
                    ProgressKind::SummaryGeneration,
                    &job_id,
                    stage.step,
                    &handle,
                    &stage.worker_name,
                    stage.input.clone().expect("selected stage input"),
                    None,
                    stage.timeout_sec,
                    cancel.clone(),
                    current_job_id.clone(),
                )
                .await;
                if let Some(context) = &stage.recovery_context {
                    record_or_reconcile_summary_stage_recovery(
                        &app, &job_id, context, &result, &cancel,
                    )
                    .await;
                }
                let outcome = result.outcome;
                results.push((stage.step, outcome.clone()));
                if cancel.is_cancelled() || outcome.status == StepStatus::Cancelled {
                    cancelled = true;
                    emit_summary_cancelled_steps(&app, &job_id, &stages[index + 1..]);
                    break;
                }
                if stop_on_error
                    && summary_stage_should_stop(&outcome, result.has_reported_failures)
                {
                    stop_after_failure = true;
                }
            }
        }
        let outcome = summary_overall_outcome(&stages, &results, cancelled);
        emit_summary_analysis_update(&app, &job_id, outcome.status, outcome.message);
        *current_job_id.lock().await = None;
        if let Some(state) = app.try_state::<AppState>() {
            state.dispatch_take(&job_id).await;
            state
                .import_progress
                .finish(ProgressKind::SummaryGeneration, &job_id);
            if let Err(error) =
                super::finish_search_index_write(&state, maintenance_registered).await
            {
                tracing::warn!(%error, "summary generation terminal maintenance reconcile failed");
            }
        }
    });
}

fn ensure_summary_dispatch_recoveries_are_healthy(
    summary_recovery_path: &std::path::Path,
    thread_group_recovery_path: &std::path::Path,
    run_thread_group: bool,
) -> AppResult<()> {
    super::summary_recovery::ensure_healthy(summary_recovery_path)?;
    if run_thread_group {
        super::thread_group_recovery::ensure_healthy_for_dispatch(thread_group_recovery_path)?;
    }
    Ok(())
}

/// Cancel an in-flight analysis dispatch (summary / personality / period
/// summary / staged summary generation). Mirrors `chat_cancel` semantics:
/// flips the cancel token so the spawned stream-drain task bails, and —
/// if a jobworkerp job is currently live — issues `JobService/Delete`
/// against it so the server-side WORKFLOW releases its LLM slot
/// immediately. Idempotent against unknown dispatch ids so the UI can
/// fire-and-forget on every Stop click.
#[tauri::command]
pub async fn analysis_cancel(state: State<'_, AppState>, dispatch_id: String) -> AppResult<()> {
    cancel_dispatch_inner(&state, &dispatch_id).await
}

/// Use the caller-supplied dispatch id when present (so a Cancel click
/// against the same UUID hits the in-flight entry), otherwise synthesise
/// the legacy timestamp-shaped id so old frontends without `dispatch_id`
/// keep working.
fn resolve_dispatch_id(supplied: Option<&str>, prefix: &str) -> String {
    match supplied {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => format!("{prefix}-{}", chrono::Utc::now().timestamp_millis()),
    }
}

fn thread_group_summary_dispatch_input(
    mut args: serde_json::Value,
    req: &EnqueueThreadGroupSummaryJobRequest,
    group_owner_user_id: i64,
) -> serde_json::Value {
    if let Some(object) = args.as_object_mut() {
        object.insert(
            "group_owner_user_id".into(),
            serde_json::Value::Number(group_owner_user_id.into()),
        );
        if let Some(group_id) = req.target_group_id {
            object.insert(
                "target_group_id".into(),
                serde_json::Value::String(group_id.to_string()),
            );
        }
        if let Some(digest) = &req.expected_membership_snapshot_digest {
            object.insert(
                "expected_membership_snapshot_digest".into(),
                serde_json::Value::String(digest.clone()),
            );
        }
    }
    args
}

fn validate_thread_group_issue_resolution_request(
    req: &EnqueueThreadGroupIssueResolutionJobRequest,
) -> AppResult<()> {
    validate_positive_user_id("target_group_id", req.target_group_id)?;
    validate_positive_user_id("group_owner_user_id", req.group_owner_user_id)?;
    if req.expected_membership_snapshot_digest.trim().is_empty() {
        return Err(AppError::Config(
            "expected_membership_snapshot_digest is required".into(),
        ));
    }
    Ok(())
}

fn validate_thread_issue_resolution_request(
    req: &EnqueueThreadIssueResolutionJobRequest,
) -> AppResult<String> {
    validate_positive_thread_id(&req.target_thread_id)
}

fn validate_positive_thread_id(thread_id: &str) -> AppResult<String> {
    let parsed = thread_id
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| AppError::Config("thread_id must be a positive integer".into()))?;
    Ok(parsed.to_string())
}

fn thread_issue_resolution_policy_json(
    data_root: &std::path::Path,
    issue_tracking_enabled: bool,
) -> AppResult<String> {
    let policy_json = crate::issue_extract::snapshot_json(data_root, issue_tracking_enabled)
        .ok_or_else(|| AppError::Config("could not serialize effective issue policy".into()))?;
    let policy: serde_json::Value = serde_json::from_str(&policy_json)
        .map_err(|error| AppError::Config(format!("invalid effective issue policy: {error}")))?;
    if policy
        .get("resolution_mode")
        .and_then(serde_json::Value::as_str)
        != Some("APPLY")
    {
        return Err(AppError::Config(
            "thread issue resolution requires resolution_mode=APPLY and Issue Tracker enabled"
                .into(),
        ));
    }
    Ok(policy_json)
}

fn thread_issue_resolution_dispatch_input(
    thread_id: &str,
    memories_callback: &crate::commands::connection::GrpcAddress,
    tracker_callback: &crate::commands::connection::GrpcAddress,
    policy_json: &str,
    llm_worker_name: &str,
    output_language: &str,
) -> serde_json::Value {
    serde_json::json!({
        "thread_id": thread_id,
        "memories_grpc_host": memories_callback.host,
        "memories_grpc_port": memories_callback.port,
        "memories_grpc_tls": memories_callback.tls,
        "issue_tracker_grpc_host": tracker_callback.host,
        "issue_tracker_grpc_port": tracker_callback.port,
        "issue_tracker_grpc_tls": tracker_callback.tls,
        "policy_json": policy_json,
        "llm_worker_name": llm_worker_name,
        "output_language": output_language,
    })
}

fn thread_issue_resolution_status_from_metadata(metadata: Option<&str>) -> &'static str {
    let Some(metadata) = metadata else {
        return "NOT_STARTED";
    };
    let Ok(metadata) = serde_json::from_str::<serde_json::Value>(metadata) else {
        return "UNKNOWN";
    };
    match metadata
        .get("issue_resolution_status")
        .and_then(serde_json::Value::as_str)
    {
        Some("PENDING") => "PENDING",
        Some("COMPLETE") => "COMPLETE",
        Some(_) => "UNKNOWN",
        None => "NOT_STARTED",
    }
}

fn thread_issue_resolution_status(summary_count: usize, metadata: Option<&str>) -> String {
    match summary_count {
        0 => "NOT_STARTED",
        1 => thread_issue_resolution_status_from_metadata(metadata),
        _ => "UNKNOWN",
    }
    .to_string()
}

fn thread_group_issue_resolution_dispatch_input(
    args: serde_json::Value,
    req: &EnqueueThreadGroupIssueResolutionJobRequest,
) -> serde_json::Value {
    let value = |key: &str, fallback: serde_json::Value| args.get(key).cloned().unwrap_or(fallback);
    serde_json::json!({
        "group_id": req.target_group_id.to_string(),
        "group_owner_user_id": req.group_owner_user_id,
        "expected_membership_snapshot_digest": req.expected_membership_snapshot_digest,
        "memories_grpc_host": value("memories_grpc_host", serde_json::json!("localhost")),
        "memories_grpc_port": value("memories_grpc_port", serde_json::json!(9100)),
        "memories_grpc_tls": value("memories_grpc_tls", serde_json::json!(false)),
        // An empty endpoint is safe because the workflow exits before any
        // tracker call when the persisted resolution policy is DISABLED.
        "issue_tracker_grpc_host": value("issue_tracker_grpc_host", serde_json::json!("")),
        "issue_tracker_grpc_port": value("issue_tracker_grpc_port", serde_json::json!(0)),
        "issue_tracker_grpc_tls": value("issue_tracker_grpc_tls", serde_json::json!(false)),
        "policy_json": value("policy_json", serde_json::json!("")),
        "llm_worker_name": value("llm_worker_name", serde_json::json!("memories-llm")),
        "output_language": value("output_language", serde_json::json!("ja")),
    })
}

/// Detach a task that drains the named worker stream and re-emits each
/// event under `event_name`. Shared by both commands so the spawn /
/// stream / emit scaffolding lives in one place.
///
/// `entry` carries the cancel token + JobId park slot owned by the
/// matching `AppState::dispatch_in_flight` entry. The spawned task:
///   1. parks the streaming-dispatch JobId via `entry.current_job_id`
///      so `analysis_cancel` can issue `JobService/Delete` against it,
///   2. surfaces `Failed("cancelled")` → `StepStatus::Failed` +
///      `"中断"` message when the cancel token fires, and
///   3. clears its own AppState entry via `dispatch_take` on exit
///      regardless of how it terminated.
// Each argument is a distinct dispatch concern (worker / args / job id /
// event name / cancel entry / refresh mode); bundling them into a struct
// would only move the arity to the call site without improving clarity.
#[allow(clippy::too_many_arguments)]
fn spawn_step_stream(
    app: AppHandle,
    handle: JobworkerpHandle,
    // Owned: the merge path computes a per-language worker name at runtime
    // (`memories-user-personality-merge-<lang>`), so this can't be `&'static`.
    worker_name: String,
    args: serde_json::Value,
    job_id: &str,
    event_name: &'static str,
    entry: DispatchCancelEntry,
    refresh_mode: RefreshMode,
    recovery_context: Option<SummaryRecoveryContext>,
    maintenance_registered: bool,
) {
    let job_id = job_id.to_string();
    tokio::spawn(async move {
        let cancel = entry.token.clone();
        let current_job_id = entry.current_job_id.clone();
        // The raw chunk is a WorkflowResult JSON whose `output` is the whole
        // intermediate context; emitting it verbatim floods the UI with
        // `{"id":..,"output":..,"status":"Running",..}`. Reuse the import
        // pipeline's digest so the progress reads `(N/M) パーソナリティを抽出中`.
        // `last_progress` carries the `(N/M)` counter across the long
        // invokeSingle chunks that don't re-publish it.
        let mut last_progress: Option<(i64, i64)> = None;
        let mut logged_group_failures = HashSet::new();
        let job_id_for_emit = job_id.clone();
        let app_for_emit = app.clone();
        let event_name_for_emit: &str = event_name;
        // `Some("run")`: WORKFLOW runner has create + run methods; we
        // execute the pre-registered workflow. See import.rs for the
        // same rationale.
        let park_job_id = current_job_id.clone();
        let cancel_for_emit = cancel.clone();
        let terminal_output = Arc::new(StdMutex::new(None::<String>));
        let terminal_output_for_events = Arc::clone(&terminal_output);
        let stream_failed = Arc::new(AtomicBool::new(false));
        let stream_failed_for_events = Arc::clone(&stream_failed);
        run_cancellable_named_stream(
            &handle,
            &worker_name,
            args,
            Some("run"),
            None,
            cancel.clone(),
            // Async park: `blocking_lock` from a runtime worker panics
            // ("Cannot block the current thread from within a runtime"),
            // so the parking write must go through the async lock. The
            // callback is `FnOnce` because the trailer JobId arrives
            // exactly once.
            move |jid| async move {
                *park_job_id.lock().await = Some(jid);
            },
            move |ev| {
                let (status, message) = match ev {
                    StreamEvent::Active(msg) => {
                        let digest = msg.map(|raw| {
                            if matches!(&refresh_mode, RefreshMode::ThreadGroupSummary) {
                                log_thread_group_failure(raw, &mut logged_group_failures);
                            }
                            emit_generated_refresh(
                                &app_for_emit,
                                &job_id_for_emit,
                                generated_refresh_scopes_from_position(raw),
                            );
                            let (d, p) = summarize_workflow_chunk(raw, last_progress);
                            last_progress = p;
                            d
                        });
                        (StepStatus::Active, digest)
                    }
                    StreamEvent::Done(msg) => 'done: {
                        if let Some(failure) = msg.and_then(workflow_terminal_failure) {
                            // A faulted run has no usable output: record and
                            // refresh it exactly like a failed stream.
                            if !cancel_for_emit.is_cancelled() {
                                stream_failed_for_events.store(true, Ordering::Release);
                            }
                            emit_generated_refresh(
                                &app_for_emit,
                                &job_id_for_emit,
                                failed_stream_refresh_scopes(&refresh_mode),
                            );
                            break 'done (StepStatus::Failed, Some(failure));
                        }
                        if let Some(raw) = &msg {
                            *terminal_output_for_events
                                .lock()
                                .expect("terminal output lock is not poisoned") =
                                Some(raw.to_string());
                        }
                        if let Some(scopes) = terminal_refresh_scopes(&refresh_mode) {
                            // A terminal refresh is intentionally not deduplicated
                            // against an Active chunk: it closes the race where the
                            // last persisted result arrives after the final chunk.
                            emit_generated_refresh(&app_for_emit, &job_id_for_emit, scopes);
                        }
                        match (&refresh_mode, msg) {
                            (RefreshMode::ThreadGroupSummary, Some(raw)) => {
                                let outcome = thread_group_summary_terminal_outcome(raw);
                                (outcome.status, outcome.message)
                            }
                            (RefreshMode::ThreadGroupSummary, None) => (
                                StepStatus::Failed,
                                Some("グループ要約の終端出力がありません".into()),
                            ),
                            (RefreshMode::Terminal(_), raw) => {
                                terminal_batch_outcome(raw, last_progress)
                            }
                            (RefreshMode::TargetedReextract { thread_id }, Some(raw)) => {
                                targeted_reextract_outcome(raw, thread_id)
                            }
                            (RefreshMode::TargetedReextract { .. }, None) => (
                                StepStatus::Failed,
                                Some("再抽出の終端出力がありません".into()),
                            ),
                        }
                    }
                    StreamEvent::Failed(msg) => {
                        if !cancel_for_emit.is_cancelled() {
                            stream_failed_for_events.store(true, Ordering::Release);
                        }
                        emit_generated_refresh(
                            &app_for_emit,
                            &job_id_for_emit,
                            failed_stream_refresh_scopes(&refresh_mode),
                        );
                        // Distinguish user-triggered cancel from a genuine
                        // worker failure: `analysis_cancel` flips the token
                        // first and then `JobService/Delete`s the live job,
                        // so a Failed event arriving once the token is
                        // cancelled is the expected shape of "server closed
                        // the stream on cancel" — surface it as a clearer
                        // "中断" message instead of the underlying gRPC
                        // text.
                        let text = if cancel_for_emit.is_cancelled() {
                            "中断".to_string()
                        } else {
                            summarize_workflow_error(msg)
                        };
                        (StepStatus::Failed, Some(text))
                    }
                };
                emit_event(
                    &app_for_emit,
                    event_name_for_emit,
                    AnalysisStepUpdate {
                        job_id: job_id_for_emit.clone(),
                        status,
                        message,
                    },
                );
            },
        )
        .await;
        let terminal_raw = {
            terminal_output
                .lock()
                .expect("terminal output lock is not poisoned")
                .take()
        };
        if let Some(raw) = terminal_raw.as_deref() {
            super::issue_resolution_diagnostics::warn_issue_resolution_errors(raw);
        }
        if let Some(context) = recovery_context
            && let Some(state) = app.try_state::<AppState>()
        {
            if terminal_raw.is_none()
                && stream_failed.load(Ordering::Acquire)
                && !cancel.is_cancelled()
            {
                let result = {
                    match &context {
                        SummaryRecoveryContext::ThreadGroup(group) => {
                            let _recovery_guard = state.thread_group_recovery.lock().await;
                            record_failed_thread_group_stream_recovery(
                                &state.data.thread_group_recovery_path(),
                                group,
                            )
                        }
                        SummaryRecoveryContext::Period(period) => {
                            let _recovery_guard = state.summary_recovery.lock().await;
                            record_failed_period_stream_recovery(
                                &state.data.summary_recovery_path(),
                                period,
                            )
                        }
                    }
                };
                if let Err(error) = result {
                    tracing::warn!(%error, "could not persist failed summary stream recovery diagnostics");
                }
            }
            if let Some(raw) = terminal_raw {
                let result = {
                    match &context {
                        SummaryRecoveryContext::ThreadGroup(context) => {
                            let _recovery_guard = state.thread_group_recovery.lock().await;
                            reconcile_thread_group_recovery(
                                &state.data.thread_group_recovery_path(),
                                context,
                                &raw,
                            )
                        }
                        SummaryRecoveryContext::Period(context) => {
                            let _recovery_guard = state.summary_recovery.lock().await;
                            reconcile_period_recovery(
                                &state.data.summary_recovery_path(),
                                context,
                                &raw,
                            )
                        }
                    }
                };
                if let Err(error) = result {
                    tracing::warn!(%error, "could not persist summary recovery diagnostics");
                }
                if let Err(error) =
                    materialize_failed_overall_attempts(&state, &context, &raw).await
                {
                    tracing::warn!(%error, "could not persist failed overall attempt metadata");
                }
            }
            // The terminal event's refresh can be served before the recovery
            // file above is written; refresh again so the warning appears.
            emit_generated_refresh(&app, &job_id, recovery_refresh_scopes(&context));
        }
        // Clear the parking slot so a (late) cancel doesn't try to Delete
        // an already-finished job, then drop the AppState entry.
        *current_job_id.lock().await = None;
        if let Some(state) = app.try_state::<AppState>() {
            state.dispatch_take(&job_id).await;
            if let Err(error) =
                super::finish_search_index_write(&state, maintenance_registered).await
            {
                tracing::warn!(%error, "analysis terminal maintenance reconcile failed");
            }
        }
    });
}

fn failed_stream_refresh_scopes(refresh_mode: &RefreshMode) -> Vec<GeneratedRefreshScope> {
    match refresh_mode {
        RefreshMode::Terminal(scopes) => scopes.clone(),
        RefreshMode::ThreadGroupSummary => vec![GeneratedRefreshScope::ThreadGroupSummary],
        RefreshMode::TargetedReextract { .. } => vec![GeneratedRefreshScope::ThreadSummary],
    }
}

fn terminal_refresh_scopes(refresh_mode: &RefreshMode) -> Option<Vec<GeneratedRefreshScope>> {
    match refresh_mode {
        RefreshMode::Terminal(scopes) => Some(scopes.clone()),
        RefreshMode::ThreadGroupSummary => Some(vec![GeneratedRefreshScope::ThreadGroupSummary]),
        // The final database write may happen after the last Active event.
        // Re-notify so summary caches cannot retain that earlier snapshot.
        RefreshMode::TargetedReextract { .. } => Some(vec![GeneratedRefreshScope::ThreadSummary]),
    }
}

fn targeted_reextract_outcome(raw: &str, thread_id: &str) -> (StepStatus, Option<String>) {
    let output = targeted_reextract_terminal_output(raw);
    let errors = output
        .as_ref()
        .and_then(|value| value.get("issue_extract_errors"))
        .and_then(serde_json::Value::as_array);
    if let Some(error) = errors.and_then(|items| {
        items.iter().find(|item| {
            item.get("thread_id").and_then(serde_json::Value::as_str) == Some(thread_id)
        })
    }) {
        return (
            StepStatus::Failed,
            error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or_else(|| Some("Issue 抽出に失敗しました".into())),
        );
    }
    let failure_message = || {
        output
            .as_ref()
            .and_then(|value| value.get("last_error"))
            .and_then(json_message)
            .or_else(|| Some("再抽出に失敗しました".into()))
    };
    if terminal_output_lists_thread(output.as_ref(), "failed_threads", thread_id) {
        return (StepStatus::Failed, failure_message());
    }
    if output
        .as_ref()
        .and_then(|value| value.get("completed"))
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return (StepStatus::Failed, failure_message());
    }
    if terminal_output_lists_thread(output.as_ref(), "skipped_threads", thread_id) {
        return (
            StepStatus::Failed,
            Some("対象 Thread は再抽出されずスキップされました".into()),
        );
    }
    if !terminal_output_lists_thread(output.as_ref(), "generated_threads", thread_id) {
        return (
            StepStatus::Failed,
            Some("対象 Thread の再抽出完了を確認できません".into()),
        );
    }
    let Some(issue_extract_outcome) =
        terminal_output_thread_entry(output.as_ref(), "issue_extract_outcomes", thread_id)
    else {
        return (
            StepStatus::Failed,
            Some("Issue 抽出の完了を確認できません".into()),
        );
    };
    if issue_extract_outcome
        .get("skipped")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return (
            StepStatus::Failed,
            Some("Issue 抽出はスキップされました".into()),
        );
    }
    let Some(run_state) = issue_extract_outcome
        .get("run_state")
        .and_then(serde_json::Value::as_str)
    else {
        return (
            StepStatus::Failed,
            Some("Issue 抽出の完了を確認できません".into()),
        );
    };
    if issue_extract_run_failed(run_state) {
        return (StepStatus::Failed, Some("Issue 抽出に失敗しました".into()));
    }
    if !issue_extract_run_completed(run_state) {
        return (
            StepStatus::Failed,
            Some("Issue 抽出の完了を確認できません".into()),
        );
    }
    (
        StepStatus::Done,
        Some(summarize_workflow_chunk(raw, None).0),
    )
}

fn issue_extract_run_failed(run_state: &str) -> bool {
    // ApplyExtraction currently declares `run_state` as a string and the
    // server returns the numeric enum value. Accept the generated enum name
    // too, so a JSON adapter change cannot turn a logical failure into Done.
    matches!(run_state, "5" | "EXTRACTION_RUN_STATE_FAILED")
}

fn issue_extract_run_completed(run_state: &str) -> bool {
    matches!(run_state, "4" | "EXTRACTION_RUN_STATE_COMPLETED")
}

fn terminal_output_lists_thread(
    output: Option<&serde_json::Value>,
    field: &str,
    thread_id: &str,
) -> bool {
    output
        .and_then(|value| value.get(field))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|threads| {
            threads
                .iter()
                .any(|thread| thread.as_str() == Some(thread_id))
        })
}

fn terminal_output_thread_entry<'a>(
    output: Option<&'a serde_json::Value>,
    field: &str,
    thread_id: &str,
) -> Option<&'a serde_json::Value> {
    output
        .and_then(|value| value.get(field))
        .and_then(serde_json::Value::as_array)
        .and_then(|entries| {
            entries.iter().find(|entry| {
                entry.get("thread_id").and_then(serde_json::Value::as_str) == Some(thread_id)
            })
        })
}

fn targeted_reextract_terminal_output(raw: &str) -> Option<serde_json::Value> {
    let value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    match value.get("output") {
        Some(serde_json::Value::String(output)) => serde_json::from_str(output).ok(),
        Some(output) => Some(output.clone()),
        None => Some(value),
    }
}

fn json_message(value: &serde_json::Value) -> Option<String> {
    value.as_str().map(str::to_owned).or_else(|| {
        (!value.is_null())
            .then(|| value.to_string())
            .filter(|message| !message.is_empty())
    })
}

#[cfg(test)]
mod targeted_reextract_tests {
    use super::*;

    #[test]
    fn targeted_reextract_reports_only_the_requested_thread_error() {
        let raw = serde_json::json!({
            "output": serde_json::json!({
                "completed": true,
                "generated_threads": ["99"],
                "issue_extract_errors": [{
                    "thread_id": "42",
                    "message": "snapshot mismatch",
                }],
                "issue_extract_outcomes": [{
                    "thread_id": "99",
                    "skipped": false,
                    "run_state": "4",
                }],
            })
            .to_string(),
        })
        .to_string();

        let (status, message) = targeted_reextract_outcome(&raw, "42");
        assert_eq!(status, StepStatus::Failed);
        assert_eq!(message.as_deref(), Some("snapshot mismatch"));
        assert_eq!(targeted_reextract_outcome(&raw, "99").0, StepStatus::Done);
    }

    #[test]
    fn targeted_reextract_reports_the_target_thread_summary_failure() {
        let raw = serde_json::json!({
            "output": serde_json::json!({
                "completed": false,
                "failed_threads": ["42"],
                "last_error": "LLM unavailable",
                "issue_extract_errors": [],
            })
            .to_string(),
        })
        .to_string();

        assert_eq!(
            targeted_reextract_outcome(&raw, "42"),
            (StepStatus::Failed, Some("LLM unavailable".into()))
        );
    }

    #[test]
    fn targeted_reextract_reports_a_failed_issue_extract_run() {
        let raw = serde_json::json!({
            "output": serde_json::json!({
                "completed": true,
                "generated_threads": ["42"],
                "issue_extract_errors": [],
                "issue_extract_outcomes": [{
                    "thread_id": "42",
                    "skipped": false,
                    "run_state": "5",
                }],
            })
            .to_string(),
        })
        .to_string();

        assert_eq!(
            targeted_reextract_outcome(&raw, "42"),
            (StepStatus::Failed, Some("Issue 抽出に失敗しました".into()))
        );
    }

    #[test]
    fn targeted_reextract_accepts_the_failed_run_state_enum_name() {
        let raw = serde_json::json!({
            "output": serde_json::json!({
                "completed": true,
                "generated_threads": ["42"],
                "issue_extract_errors": [],
                "issue_extract_outcomes": [{
                    "thread_id": "42",
                    "skipped": false,
                    "run_state": "EXTRACTION_RUN_STATE_FAILED",
                }],
            })
            .to_string(),
        })
        .to_string();

        assert_eq!(
            targeted_reextract_outcome(&raw, "42"),
            (StepStatus::Failed, Some("Issue 抽出に失敗しました".into()))
        );
    }

    #[test]
    fn targeted_reextract_reports_a_skipped_issue_extract_as_failed() {
        let raw = serde_json::json!({
            "output": serde_json::json!({
                "completed": true,
                "generated_threads": ["42"],
                "issue_extract_errors": [],
                "issue_extract_outcomes": [{
                    "thread_id": "42",
                    "skipped": true,
                    "run_state": null,
                }],
            })
            .to_string(),
        })
        .to_string();

        assert_eq!(
            targeted_reextract_outcome(&raw, "42"),
            (
                StepStatus::Failed,
                Some("Issue 抽出はスキップされました".into())
            )
        );
    }

    #[test]
    fn targeted_reextract_requires_a_completed_issue_extract_run() {
        let raw = serde_json::json!({
            "output": serde_json::json!({
                "completed": true,
                "generated_threads": ["42"],
                "issue_extract_errors": [],
                "issue_extract_outcomes": [{
                    "thread_id": "42",
                    "skipped": false,
                    "run_state": null,
                }],
            })
            .to_string(),
        })
        .to_string();

        assert_eq!(
            targeted_reextract_outcome(&raw, "42"),
            (
                StepStatus::Failed,
                Some("Issue 抽出の完了を確認できません".into())
            )
        );
    }

    #[test]
    fn targeted_reextract_requires_an_issue_extract_outcome() {
        let raw = serde_json::json!({
            "output": serde_json::json!({
                "completed": true,
                "generated_threads": ["42"],
                "issue_extract_errors": [],
            })
            .to_string(),
        })
        .to_string();

        assert_eq!(
            targeted_reextract_outcome(&raw, "42"),
            (
                StepStatus::Failed,
                Some("Issue 抽出の完了を確認できません".into())
            )
        );
    }

    #[test]
    fn targeted_reextract_requires_a_successful_batch_outcome() {
        let raw = serde_json::json!({
            "output": serde_json::json!({
                "completed": false,
                "failed_threads": [],
                "last_error": null,
                "issue_extract_errors": [],
            })
            .to_string(),
        })
        .to_string();

        assert_eq!(
            targeted_reextract_outcome(&raw, "42"),
            (StepStatus::Failed, Some("再抽出に失敗しました".into()))
        );
    }

    #[test]
    fn targeted_reextract_reports_a_skipped_target_as_failed() {
        let raw = serde_json::json!({
            "output": serde_json::json!({
                "completed": true,
                "generated_threads": [],
                "skipped_threads": ["42"],
                "issue_extract_errors": [],
            })
            .to_string(),
        })
        .to_string();

        assert_eq!(
            targeted_reextract_outcome(&raw, "42"),
            (
                StepStatus::Failed,
                Some("対象 Thread は再抽出されずスキップされました".into())
            )
        );
    }

    #[test]
    fn targeted_reextract_requires_the_target_thread_to_be_generated() {
        let raw = serde_json::json!({
            "output": serde_json::json!({
                "completed": true,
                "generated_threads": ["99"],
                "skipped_threads": [],
                "issue_extract_errors": [],
                "issue_extract_outcomes": [{
                    "thread_id": "99",
                    "skipped": false,
                    "run_state": "4",
                }],
            })
            .to_string(),
        })
        .to_string();

        assert_eq!(
            targeted_reextract_outcome(&raw, "42"),
            (
                StepStatus::Failed,
                Some("対象 Thread の再抽出完了を確認できません".into())
            )
        );
        assert_eq!(targeted_reextract_outcome(&raw, "99").0, StepStatus::Done);
    }

    #[test]
    fn targeted_reextract_workflows_propagate_issue_extract_outcome() {
        let single = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml"
        ));
        assert!(
            single.contains("$extract_child_result.extract.applied.runState // null"),
            "the single-thread workflow must expose the checked ApplyExtraction run state"
        );
        let reject = single
            .find("rejectFaultedExtractResult:")
            .expect("the single-thread workflow must reject faulted child output");
        let capture = single
            .find("captureExtractOutcome:")
            .expect("the single-thread workflow must capture successful child output");
        assert!(
            reject < capture,
            "a faulted child must not be reported as a successful extraction"
        );
        assert!(
            single.contains("extract_run_state: ${ $extract_run_state // null }"),
            "the single-thread workflow must include the run state in its output"
        );
        assert!(
            single.contains("extract_skipped: ${ $extract_skipped // false }"),
            "the single-thread workflow must include whether issue extraction was skipped"
        );
        assert!(
            single.contains("extract_skipped: \"${ $extract_child_result.skipped // false }\""),
            "the single-thread workflow must read the issue-extract skip result"
        );

        let batch = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../workers/workflows/thread-summary/thread-summary-batch.yaml"
        ));
        assert!(
            batch.contains("recordIssueExtractOutcome"),
            "the batch workflow must collect every single-thread extract outcome"
        );
        assert!(
            batch.contains("issue_extract_outcomes: ${ $issue_extract_outcomes }"),
            "the batch workflow must include extract outcomes in its output"
        );
    }

    #[test]
    fn targeted_reextract_terminal_refreshes_thread_summaries() {
        assert_eq!(
            terminal_refresh_scopes(&RefreshMode::TargetedReextract {
                thread_id: "42".into(),
            }),
            Some(vec![GeneratedRefreshScope::ThreadSummary])
        );
    }
}

/// Reconcile the durable retry set from a terminal period-batch result.
///
/// A period can be removed only when the batch explicitly reports it as
/// succeeded and did not report any failed path bucket for it. This preserves
/// retry work for partial outputs while letting a later complete run converge.
fn reconcile_period_recovery(
    path: &std::path::Path,
    context: &PeriodRecoveryContext,
    raw: &str,
) -> AppResult<()> {
    let Some(output) = workflow_output_value(raw) else {
        return Ok(());
    };
    reconcile_period_recovery_output(path, context, &output)
}

fn record_failed_thread_group_stream_recovery(
    path: &std::path::Path,
    context: &ThreadGroupRecoveryContext,
) -> AppResult<()> {
    let Some(group_id) = context.target_group_id else {
        return Ok(());
    };
    super::thread_group_recovery::record(
        path,
        super::thread_group_recovery::build_record(
            super::thread_group_recovery::ThreadGroupRecoveryIdentity {
                memories_endpoint_id: &context.memories_endpoint_id,
                group_id,
                expected_membership_snapshot_digest: context
                    .expected_membership_snapshot_digest
                    .as_deref(),
                generation_id: &context.generation_id,
                trigger_kind: "manual",
            },
            "workflow_execution_failed",
            Some("workflow terminated before emitting a group summary result".into()),
        ),
    )
}

fn reconcile_thread_group_recovery(
    path: &std::path::Path,
    context: &ThreadGroupRecoveryContext,
    raw: &str,
) -> AppResult<()> {
    let Some(output) = workflow_output_value(raw) else {
        return record_failed_thread_group_stream_recovery(path, context);
    };
    let failed_groups = output
        .get("failed_groups")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut accounted_group_ids = HashSet::new();
    for failure in failed_groups {
        let Some(group_id) = failure
            .get("group_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| value.parse::<i64>().ok())
        else {
            continue;
        };
        accounted_group_ids.insert(group_id);
        let reason = failure
            .get("reason_code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("workflow_failed");
        let detail = failure.get("detail").map(serde_json::Value::to_string);
        let digest = failure
            .get("expected_membership_snapshot_digest")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                (context.target_group_id == Some(group_id))
                    .then_some(context.expected_membership_snapshot_digest.as_deref())
                    .flatten()
            });
        super::thread_group_recovery::record(
            path,
            super::thread_group_recovery::build_record(
                super::thread_group_recovery::ThreadGroupRecoveryIdentity {
                    memories_endpoint_id: &context.memories_endpoint_id,
                    group_id,
                    expected_membership_snapshot_digest: digest,
                    generation_id: &context.generation_id,
                    trigger_kind: "manual",
                },
                reason,
                detail,
            ),
        )?;
    }
    if let Some(groups) = output
        .get("generated_groups")
        .and_then(serde_json::Value::as_array)
    {
        for group_id in groups.iter().filter_map(|value| value.as_str()) {
            if let Ok(group_id) = group_id.parse::<i64>() {
                accounted_group_ids.insert(group_id);
                super::thread_group_recovery::complete_group(
                    path,
                    &context.memories_endpoint_id,
                    group_id,
                )?;
            }
        }
    }
    if let Some(groups) = output
        .get("skipped_groups")
        .and_then(serde_json::Value::as_array)
    {
        for group_id in groups.iter().filter_map(serde_json::Value::as_str) {
            if let Ok(group_id) = group_id.parse::<i64>() {
                accounted_group_ids.insert(group_id);
                super::thread_group_recovery::complete_group(
                    path,
                    &context.memories_endpoint_id,
                    group_id,
                )?;
            }
        }
    }
    if let Some(group_id) = context.target_group_id
        && output.get("completed").and_then(serde_json::Value::as_bool) != Some(true)
        && !accounted_group_ids.contains(&group_id)
    {
        // A malformed or truncated batch result must not make a targeted retry
        // disappear merely because its failed_groups entry was omitted.
        super::thread_group_recovery::record(
            path,
            super::thread_group_recovery::build_record(
                super::thread_group_recovery::ThreadGroupRecoveryIdentity {
                    memories_endpoint_id: &context.memories_endpoint_id,
                    group_id,
                    expected_membership_snapshot_digest: context
                        .expected_membership_snapshot_digest
                        .as_deref(),
                    generation_id: &context.generation_id,
                    trigger_kind: "manual",
                },
                "workflow_incomplete",
                Some("targeted workflow reported incomplete without a group outcome".into()),
            ),
        )?;
    }
    Ok(())
}

/// Persist the known period identity when the workflow terminates through a
/// stream error before it can emit its normal batch contract.  This is most
/// notably the `stop_on_error=true` path: the YAML deliberately raises to
/// prevent later work, so there is no `failed_dates` output to reconcile.
///
/// The individual path bucket is not guessed from an error message.  The
/// overall record is enough to make the period visible and eligible for an
/// explicit replay; a later successful batch reconciliation removes it.
fn record_failed_period_stream_recovery(
    path: &std::path::Path,
    context: &PeriodRecoveryContext,
) -> AppResult<()> {
    let period_kind = match context.period_kind {
        PeriodKind::Daily => "daily",
        PeriodKind::Weekly => "weekly",
        PeriodKind::Monthly => "monthly",
    };
    for period_key in &context.fallback_period_keys {
        super::summary_recovery::record(
            path,
            super::summary_recovery::build_record(
                recovery_identity(
                    &context.memories_endpoint_id,
                    &context.source_scope_digest,
                    &context.generation_id,
                    "manual",
                ),
                period_kind,
                period_key,
                super::summary_recovery::SummaryRecoveryScope::Overall,
                "period_batch",
                "workflow_execution_failed",
                Some("workflow terminated before emitting a batch result".to_string()),
            ),
        )?;
    }
    Ok(())
}

fn recovery_identity<'a>(
    memories_endpoint_id: &'a str,
    source_scope_digest: &'a str,
    generation_id: &'a str,
    trigger_kind: &'a str,
) -> super::summary_recovery::SummaryRecoveryIdentity<'a> {
    super::summary_recovery::SummaryRecoveryIdentity {
        memories_endpoint_id,
        source_scope_digest,
        generation_id,
        trigger_kind,
    }
}

/// Resolve the same IANA timezone that the sidecar injects into each summary
/// worker as `TZ`. A strict stream failure has no terminal contract to tell us
/// its target periods, so its fallback identity must use that same calendar.
fn workflow_timezone(state: &AppState) -> AppResult<chrono_tz::Tz> {
    let app_settings = crate::data::paths::load_app_settings(&state.data.app_settings_path());
    let timezone_name = crate::sidecar::lifecycle::resolve_timezone(Some(&app_settings));
    timezone_name.parse::<chrono_tz::Tz>().map_err(|error| {
        AppError::Config(format!(
            "要約 workflow のタイムゾーン `{timezone_name}` を解決できません: {error}"
        ))
    })
}

/// Mirror the standalone batch calendar defaults so a failed stream has a
/// concrete recovery identity. The normal result contract remains
/// authoritative whenever it is available.
fn fallback_period_keys(
    kind: PeriodKind,
    last_n: Option<i32>,
    timezone: chrono_tz::Tz,
) -> Vec<String> {
    fallback_period_keys_at(kind, last_n, chrono::Utc::now(), timezone)
}

fn fallback_period_keys_at(
    kind: PeriodKind,
    last_n: Option<i32>,
    now: chrono::DateTime<chrono::Utc>,
    timezone: chrono_tz::Tz,
) -> Vec<String> {
    super::summary_calendar::completed_periods(
        calendar_period_kind(kind),
        last_n.unwrap_or(1),
        now,
        timezone,
    )
}

fn calendar_period_kind(kind: PeriodKind) -> super::summary_calendar::PeriodKind {
    match kind {
        PeriodKind::Daily => super::summary_calendar::PeriodKind::Daily,
        PeriodKind::Weekly => super::summary_calendar::PeriodKind::Weekly,
        PeriodKind::Monthly => super::summary_calendar::PeriodKind::Monthly,
    }
}

fn reconcile_period_recovery_output(
    path: &std::path::Path,
    context: &PeriodRecoveryContext,
    output: &serde_json::Value,
) -> AppResult<()> {
    let (kind, failed_key, succeeded_key) = match context.period_kind {
        PeriodKind::Daily => ("daily", "failed_dates", "succeeded_dates"),
        PeriodKind::Weekly => ("weekly", "failed_weeks", "succeeded_weeks"),
        PeriodKind::Monthly => ("monthly", "failed_months", "succeeded_months"),
    };
    let has_identified_failures = output
        .get(failed_key)
        .and_then(serde_json::Value::as_array)
        .is_some_and(|periods| !periods.is_empty())
        || output
            .get("failed_path_buckets")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|failures| !failures.is_empty());
    if output.get("completed").and_then(serde_json::Value::as_bool) == Some(false)
        && !has_identified_failures
    {
        // A terminal output can still be incomplete if it does not identify
        // the failed selected period. Preserve the whole selection rather
        // than letting an empty failure list erase retry visibility.
        record_failed_period_stream_recovery(path, context)?;
    }
    for period_key in output
        .get(failed_key)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
    {
        super::summary_recovery::record(
            path,
            super::summary_recovery::build_record(
                recovery_identity(
                    &context.memories_endpoint_id,
                    &context.source_scope_digest,
                    &context.generation_id,
                    "manual",
                ),
                kind,
                period_key,
                super::summary_recovery::SummaryRecoveryScope::Overall,
                "overall_summary",
                "llm_execution_failed",
                output
                    .get("last_error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            ),
        )?;
    }
    for failure in output
        .get("failed_path_buckets")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(period_key), Some(bucket_kind)) = (
            failure
                .get("period_key")
                .and_then(serde_json::Value::as_str),
            failure
                .get("path_bucket_kind")
                .and_then(serde_json::Value::as_str),
        ) else {
            continue;
        };
        let path_label = failure
            .get("path_label")
            .and_then(serde_json::Value::as_str)
            .filter(|label| !label.is_empty());
        let Some(path_bucket_digest) =
            crate::commands::summaries::path_bucket_digest(bucket_kind, path_label)
        else {
            tracing::warn!(%period_key, %bucket_kind, "ignoring invalid failed path bucket");
            continue;
        };
        super::summary_recovery::record(
            path,
            super::summary_recovery::build_record(
                recovery_identity(
                    &context.memories_endpoint_id,
                    &context.source_scope_digest,
                    &context.generation_id,
                    "manual",
                ),
                kind,
                period_key,
                super::summary_recovery::SummaryRecoveryScope::Path {
                    bucket_digest: &path_bucket_digest,
                    bucket_kind,
                    path_label,
                },
                "path_summary",
                "llm_execution_failed",
                failure.get("error").map(serde_json::Value::to_string),
            ),
        )?;
    }

    let failed_path_periods: HashSet<&str> = output
        .get("failed_path_buckets")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|failure| failure.get("period_key")?.as_str())
        .collect();
    for period_key in output
        .get(succeeded_key)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|period_key| !failed_path_periods.contains(period_key))
    {
        let overall = super::summary_recovery::build_record(
            recovery_identity(
                &context.memories_endpoint_id,
                &context.source_scope_digest,
                &context.generation_id,
                "manual",
            ),
            kind,
            period_key,
            super::summary_recovery::SummaryRecoveryScope::Overall,
            "overall_summary",
            "unknown",
            None,
        );
        super::summary_recovery::complete_period(path, &overall)?;
    }
    Ok(())
}

/// A failed overall attempt must not replace a readable previous body, but the
/// body itself should expose the failed generation rather than relying only on
/// the separate recovery file.  The update is intentionally best-effort: the
/// durable recovery record remains authoritative when Memories is unavailable.
async fn materialize_failed_overall_attempts(
    state: &AppState,
    context: &SummaryRecoveryContext,
    raw: &str,
) -> AppResult<()> {
    let Some(output) = workflow_output_value(raw) else {
        return Ok(());
    };
    match context {
        SummaryRecoveryContext::ThreadGroup(_) => {}
        SummaryRecoveryContext::Period(context) => {
            materialize_failed_overall_attempt_stage(
                state,
                context.period_kind,
                &context.source_scope_digest,
                &context.generation_id,
                &context.generation_started_at,
                &output,
            )
            .await?;
        }
    }
    Ok(())
}

async fn materialize_failed_overall_attempt_stage(
    state: &AppState,
    period_kind: PeriodKind,
    source_scope_digest: &str,
    generation_id: &str,
    generation_started_at: &str,
    stage_output: &serde_json::Value,
) -> AppResult<usize> {
    let failed_key = match period_kind {
        PeriodKind::Daily => "failed_dates",
        PeriodKind::Weekly => "failed_weeks",
        PeriodKind::Monthly => "failed_months",
    };
    let periods = stage_output
        .get(failed_key)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let kind = match period_kind {
        PeriodKind::Daily => crate::commands::summaries::SummaryKind::Daily,
        PeriodKind::Weekly => crate::commands::summaries::SummaryKind::Weekly,
        PeriodKind::Monthly => crate::commands::summaries::SummaryKind::Monthly,
    };
    crate::commands::summaries::mark_failed_overall_attempts(
        state,
        kind,
        source_scope_digest,
        generation_id,
        generation_started_at,
        &periods,
        "llm_execution_failed",
    )
    .await
}

/// Scopes whose views read the recovery state written for `context`.
fn recovery_refresh_scopes(context: &SummaryRecoveryContext) -> Vec<GeneratedRefreshScope> {
    match context {
        SummaryRecoveryContext::Period(period) => vec![period_refresh_scope(period.period_kind)],
        SummaryRecoveryContext::ThreadGroup(_) => vec![GeneratedRefreshScope::ThreadGroupSummary],
    }
}

fn period_refresh_scope(kind: PeriodKind) -> GeneratedRefreshScope {
    match kind {
        PeriodKind::Daily => GeneratedRefreshScope::DailySummary,
        PeriodKind::Weekly => GeneratedRefreshScope::WeeklySummary,
        PeriodKind::Monthly => GeneratedRefreshScope::MonthlySummary,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use super::*;
    use crate::commands::GeneratedRefreshUpdate;
    use crate::data::paths::tests::{evaluate_workflow_jq, find_workflow_task};

    #[test]
    fn thread_group_member_owner_protojson_int64_is_a_child_integer() {
        let group_workflow: serde_yaml::Value = serde_yaml::from_str(include_str!(
            "../../../workers/workflows/thread-group-summary/thread-group-summary-batch.yaml"
        ))
        .expect("thread-group workflow YAML");
        let child_workflow: serde_yaml::Value = serde_yaml::from_str(include_str!(
            "../../../workers/lang-workers/workers/thread-summary/thread-summary-single.yaml"
        ))
        .expect("thread-summary single workflow YAML");
        let child_schema = serde_json::to_value(&child_workflow["input"]["schema"]["document"])
            .expect("child input schema JSON");
        let child_validator = jsonschema::draft202012::new(&child_schema)
            .expect("compile thread-summary single input schema");

        let members = serde_json::json!([
            {
                "state": "THREAD_GROUP_MEMBER_STATE_ACTIVE",
                "threadId": "1001",
                "userId": "9007199254740993"
            },
            {
                "state": "THREAD_GROUP_MEMBER_STATE_ACTIVE",
                "threadId": "1002",
                "userId": "9007199254740993"
            },
            {
                "state": "THREAD_GROUP_MEMBER_STATE_ACTIVE",
                "threadId": "1003",
                "userId": "1"
            },
            {
                "state": "THREAD_GROUP_MEMBER_STATE_ACTIVE",
                "threadId": "1004",
                "userId": "9223372036854775807"
            },
            {
                "state": "THREAD_GROUP_MEMBER_STATE_ACTIVE",
                "threadId": "1005",
                "userId": 15
            },
            {
                "state": "THREAD_GROUP_MEMBER_STATE_ACTIVE",
                "threadId": "1006",
                "userId": "15"
            },
            {
                "state": "THREAD_GROUP_MEMBER_STATE_ACTIVE",
                "threadId": "1007",
                "userId": null
            },
            {
                "state": "THREAD_GROUP_MEMBER_STATE_ACTIVE",
                "threadId": "1008",
                "userId": "-9223372036854775808"
            }
        ]);
        let lineage_context = BTreeMap::from([
            (
                "lineage".into(),
                Arc::new(serde_json::json!({ "members": members })),
            ),
            (
                "owner_member_entries".into(),
                Arc::new(serde_json::json!([])),
            ),
        ]);
        let record_members = find_workflow_task(&group_workflow, "recordGroupMembers")
            .expect("recordGroupMembers task");
        let owner_member_entries = evaluate_workflow_jq(
            &record_members["set"]["owner_member_entries"],
            &lineage_context,
        )
        .expect("evaluate production owner-entry expression");
        let member_refresh_targets = evaluate_workflow_jq(
            &find_workflow_task(&group_workflow, "buildMemberRefreshTargets")
                .expect("buildMemberRefreshTargets task")["set"]["member_refresh_targets"],
            &BTreeMap::from([(
                "owner_member_entries".into(),
                Arc::new(owner_member_entries),
            )]),
        )
        .expect("evaluate production member-target expression");
        assert_eq!(
            member_refresh_targets,
            serde_json::json!([
                {"user_id": i64::MIN, "thread_id": "1008"},
                {"user_id": 1, "thread_id": "1003"},
                {"user_id": 15, "thread_id": "1005"},
                {"user_id": 15, "thread_id": "1006"},
                {"user_id": 9007199254740993_i64, "thread_id": "1001"},
                {"user_id": 9007199254740993_i64, "thread_id": "1002"},
                {"user_id": i64::MAX, "thread_id": "1004"}
            ])
        );

        let report_progress = find_workflow_task(&group_workflow, "reportProgress")
            .expect("member refresh reportProgress task");
        let progress_context = BTreeMap::from([
            (
                "member_refresh_targets".into(),
                Arc::new(member_refresh_targets.clone()),
            ),
            ("member_target_index".into(), Arc::new(serde_json::json!(2))),
        ]);
        for (key, expected) in [("progress_processed", 3), ("progress_total", 7)] {
            assert_eq!(
                evaluate_workflow_jq(&report_progress["set"][key], &progress_context)
                    .expect("evaluate production progress expression"),
                serde_json::json!(expected),
                "{key}"
            );
        }

        let child_input_expression = &find_workflow_task(&group_workflow, "invokeMemberSingle")
            .expect("invokeMemberSingle task")["run"]["function"]["arguments"]["input"];
        for member_target in member_refresh_targets.as_array().expect("member targets") {
            let child_context = BTreeMap::from([
                ("member_target".into(), Arc::new(member_target.clone())),
                (
                    "workflow".into(),
                    Arc::new(serde_json::json!({
                        "input": {
                            "memories_grpc_host": "localhost",
                            "memories_grpc_port": 9100,
                            "memories_grpc_tls": false,
                            "output_language": "ja",
                            "llm_worker_name": "memories-llm",
                            "max_context_chars": 200000,
                            "force_resummarize": false
                        }
                    })),
                ),
            ]);
            let encoded_input = evaluate_workflow_jq(child_input_expression, &child_context)
                .expect("evaluate production child input expression");
            let child_input: serde_json::Value =
                serde_json::from_str(encoded_input.as_str().expect("child input JSON string"))
                    .expect("parse child input JSON");

            assert_eq!(child_input["user_id"], member_target["user_id"]);
            assert_eq!(child_input["thread_id"], member_target["thread_id"]);
            assert!(
                child_validator.iter_errors(&child_input).next().is_none(),
                "forwarded child input must satisfy its schema: {child_input}"
            );
        }

        // The batch cannot refresh an owner-less member, so the single that
        // aggregates the group must reject it.
        let single_workflow: serde_yaml::Value = serde_yaml::from_str(include_str!(
            "../../../workers/lang-workers/workers/thread-group-summary/thread-group-summary-single.yaml"
        ))
        .expect("thread-group single workflow YAML");
        let collect_members = find_workflow_task(&single_workflow, "collectMemberThreadIds")
            .expect("collectMemberThreadIds task");
        let missing_owner_thread_ids = evaluate_workflow_jq(
            &collect_members["set"]["missing_member_owner_thread_ids"],
            &lineage_context,
        )
        .expect("evaluate production missing-owner collection");
        assert_eq!(missing_owner_thread_ids, serde_json::json!(["1007"]));
        let reject_context = BTreeMap::from([(
            "missing_member_owner_thread_ids".into(),
            Arc::new(missing_owner_thread_ids),
        )]);
        assert_eq!(
            evaluate_workflow_jq(
                &find_workflow_task(&single_workflow, "rejectMissingMemberOwner")
                    .expect("rejectMissingMemberOwner task")["if"],
                &reject_context,
            )
            .expect("evaluate production missing-owner rejection predicate"),
            serde_json::json!(true)
        );
    }

    #[test]
    fn thread_group_member_owner_conversion_rejects_non_int64_values() {
        let group_workflow: serde_yaml::Value = serde_yaml::from_str(include_str!(
            "../../../workers/workflows/thread-group-summary/thread-group-summary-batch.yaml"
        ))
        .expect("thread-group workflow YAML");
        let record_members = find_workflow_task(&group_workflow, "recordGroupMembers")
            .expect("recordGroupMembers task");
        let expression = &record_members["set"]["owner_member_entries"];

        for invalid_owner in [
            serde_json::json!("not-an-int64"),
            serde_json::json!("1.5"),
            serde_json::json!("1.0"),
            serde_json::json!("1e0"),
            serde_json::json!("9223372036854775808"),
            serde_json::json!("-9223372036854775809"),
            serde_json::json!(1.5),
            serde_json::from_str::<serde_json::Value>("15.0")
                .expect("integral float owner fixture"),
            serde_json::from_str::<serde_json::Value>("1e0").expect("exponent float owner fixture"),
            serde_json::from_str::<serde_json::Value>("9223372036854775808.0")
                .expect("out-of-range float owner fixture"),
            serde_json::json!(u64::MAX),
        ] {
            let context = BTreeMap::from([
                (
                    "lineage".into(),
                    Arc::new(serde_json::json!({
                        "members": [{
                            "state": "THREAD_GROUP_MEMBER_STATE_ACTIVE",
                            "threadId": "1001",
                            "userId": invalid_owner
                        }]
                    })),
                ),
                (
                    "owner_member_entries".into(),
                    Arc::new(serde_json::json!([])),
                ),
            ]);
            assert!(
                evaluate_workflow_jq(expression, &context).is_err(),
                "invalid owner must fail instead of becoming a rounded child ID: {invalid_owner}"
            );
        }
    }

    #[test]
    fn personality_request_deserializes_force_reextract() {
        // The Personality tab's Force checkbox flips this single field.
        // Drift guard for the JSON contract — if the field renames or
        // changes type the UI would silently lose the toggle.
        let on: EnqueuePersonalityJobRequest =
            serde_json::from_str(r#"{"force_reextract": true}"#).unwrap();
        assert_eq!(on.force_reextract, Some(true));
        let off: EnqueuePersonalityJobRequest =
            serde_json::from_str(r#"{"force_reextract": false}"#).unwrap();
        assert_eq!(off.force_reextract, Some(false));
        // Absence keeps it None so the dispatch defaults to the normal
        // (skip-up-to-date) batch, which is what the import path relies on.
        let absent: EnqueuePersonalityJobRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.force_reextract, None);
    }

    #[test]
    fn personality_merge_request_deserializes_force_remerge() {
        // The Personality tab's "マージのみ" button bridges the same Force
        // checkbox; the request shape must accept it without renaming or
        // collapsing the flag into `force_reextract` (which has different
        // semantics — re-run per-thread vs override merge eligibility).
        let on: EnqueuePersonalityMergeJobRequest =
            serde_json::from_str(r#"{"force_remerge": true}"#).unwrap();
        assert_eq!(on.force_remerge, Some(true));
        let off: EnqueuePersonalityMergeJobRequest =
            serde_json::from_str(r#"{"force_remerge": false}"#).unwrap();
        assert_eq!(off.force_remerge, Some(false));
        let absent: EnqueuePersonalityMergeJobRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.force_remerge, None);
    }

    #[test]
    fn merge_worker_name_matches_registered_lang_worker() {
        // Drift guard: the merge lang-worker registered by
        // `memories-import upsert-generation-workers` is named
        // `<base>-<lang>`. If the base or the suffix shape drifts the merge
        // dispatch hits WorkerNotFound; a unit test failure catches it before
        // the toast surfaces the gRPC error.
        assert_eq!(
            PERSONALITY_MERGE_WORKER_BASE,
            "memories-user-personality-merge"
        );
        assert_eq!(
            format!("{PERSONALITY_MERGE_WORKER_BASE}-ja"),
            "memories-user-personality-merge-ja"
        );
        assert_eq!(
            format!("{PERSONALITY_MERGE_WORKER_BASE}-en"),
            "memories-user-personality-merge-en"
        );
    }

    #[test]
    fn personality_merge_reuses_personality_event() {
        // The Personality tab's progress hook subscribes to
        // `personality://step`; the merge dispatch shares the event so the
        // existing hook surfaces it without an extra subscription. A
        // separate event would silently drop the progress on the floor.
        assert_eq!(PERSONALITY_EVENT, "personality://step");
    }

    #[test]
    fn requests_accept_dispatch_id_for_cancel() {
        // The cancel UI generates a UUID per dispatch; the request shape
        // must accept and surface it without requiring older callers to
        // start sending the field.
        let s: EnqueueSummaryJobRequest =
            serde_json::from_str(r#"{"dispatch_id": "abc-123"}"#).unwrap();
        assert_eq!(s.dispatch_id.as_deref(), Some("abc-123"));
        let p: EnqueuePersonalityJobRequest =
            serde_json::from_str(r#"{"dispatch_id": "abc-456"}"#).unwrap();
        assert_eq!(p.dispatch_id.as_deref(), Some("abc-456"));
        let pp: EnqueuePeriodSummaryJobRequest =
            serde_json::from_str(r#"{"kind": "daily", "dispatch_id": "abc-789"}"#).unwrap();
        assert_eq!(pp.dispatch_id.as_deref(), Some("abc-789"));
        assert!(!pp.stop_on_error);
        let pm: EnqueuePersonalityMergeJobRequest =
            serde_json::from_str(r#"{"dispatch_id": "abc-merge"}"#).unwrap();
        assert_eq!(pm.dispatch_id.as_deref(), Some("abc-merge"));
        // Omitting dispatch_id keeps the field None so the resolver can
        // synthesize the legacy timestamp-shaped id.
        let s_absent: EnqueueSummaryJobRequest = serde_json::from_str("{}").unwrap();
        assert!(s_absent.dispatch_id.is_none());
    }

    #[test]
    fn thread_group_dispatch_carries_target_and_expected_snapshot() {
        let request: EnqueueThreadGroupSummaryJobRequest = serde_json::from_str(
            r#"{"user_id":1,"group_owner_user_id":7,"target_group_id": "42", "expected_membership_snapshot_digest": "digest-a"}"#,
        )
        .unwrap();
        assert_eq!(request.target_group_id, Some(42));
        assert_eq!(
            request.expected_membership_snapshot_digest.as_deref(),
            Some("digest-a")
        );
        assert_eq!(request.group_owner_user_id, Some(7));
    }

    #[test]
    fn thread_group_summary_dispatch_requires_matching_owner_aliases() {
        use super::super::thread_group_owner::resolve_group_owner_user_id;

        let matching: EnqueueThreadGroupSummaryJobRequest =
            serde_json::from_str(r#"{"group_owner_user_id":7,"user_id":7}"#).unwrap();
        assert_eq!(
            resolve_group_owner_user_id(matching.group_owner_user_id, matching.user_id).unwrap(),
            7
        );

        let mismatched: EnqueueThreadGroupSummaryJobRequest =
            serde_json::from_str(r#"{"group_owner_user_id":7,"user_id":1}"#).unwrap();
        assert!(
            resolve_group_owner_user_id(mismatched.group_owner_user_id, mismatched.user_id)
                .is_err()
        );

        for raw in [
            r#"{"group_owner_user_id":0}"#,
            r#"{"group_owner_user_id":-1}"#,
            r#"{"user_id":0}"#,
            r#"{"user_id":-1}"#,
        ] {
            let request: EnqueueThreadGroupSummaryJobRequest = serde_json::from_str(raw).unwrap();
            assert!(
                resolve_group_owner_user_id(request.group_owner_user_id, request.user_id).is_err()
            );
        }
    }

    #[test]
    fn thread_group_summary_workflow_probes_owner_capability_before_group_reads() {
        let workflow = include_str!(
            "../../../workers/workflows/thread-group-summary/thread-group-summary-batch.yaml"
        );
        let capability_probe = workflow
            .find("/llm_memory.service.ThreadGroupService/GetThreadGroupCapabilities")
            .unwrap();
        let first_owner_filtered_read = workflow
            .find("/llm_memory.service.ThreadGroupService/FindThreadGroupList")
            .unwrap();

        assert!(capability_probe < first_owner_filtered_read);
        assert!(workflow.contains("thread-group-summary/owner-scope-unsupported"));
        assert!(workflow.contains("group_owner_user_id: $group_owner_user_id"));
    }

    #[test]
    fn thread_group_dispatch_allows_an_untargeted_request() {
        let request: EnqueueThreadGroupSummaryJobRequest =
            serde_json::from_str(r#"{"dispatch_id": "summary-tab"}"#).unwrap();

        assert_eq!(request.dispatch_id.as_deref(), Some("summary-tab"));
        assert_eq!(request.target_group_id, None);
        assert_eq!(request.expected_membership_snapshot_digest, None);
    }

    #[test]
    fn thread_group_dispatch_preserves_issue_resolution_inputs() {
        let request: EnqueueThreadGroupSummaryJobRequest = serde_json::from_str(
            r#"{"target_group_id":"42","expected_membership_snapshot_digest":"digest-a","group_owner_user_id":7}"#,
        )
        .unwrap();
        let input = thread_group_summary_dispatch_input(
            serde_json::json!({
                "policy_json": "{\"resolution_mode\":\"APPLY\"}",
                "issue_tracker_grpc_host": "tracker.example",
                "issue_tracker_grpc_port": 9555,
            }),
            &request,
            7,
        );

        assert_eq!(input["policy_json"], "{\"resolution_mode\":\"APPLY\"}");
        assert_eq!(input["issue_tracker_grpc_host"], "tracker.example");
        assert_eq!(input["issue_tracker_grpc_port"], 9555);
        assert_eq!(input["target_group_id"], "42");
        assert_eq!(input["expected_membership_snapshot_digest"], "digest-a");
        assert_eq!(input["group_owner_user_id"], 7);
    }

    #[test]
    fn issue_resolution_request_requires_a_target_and_digest() {
        let large_group = serde_json::from_str::<EnqueueThreadGroupIssueResolutionJobRequest>(
            r#"{"target_group_id":"9007199254740993","group_owner_user_id":7,"expected_membership_snapshot_digest":"digest-a"}"#,
        )
        .unwrap();
        assert_eq!(large_group.target_group_id, 9_007_199_254_740_993);
        assert_eq!(large_group.group_owner_user_id, 7);

        assert!(
            serde_json::from_str::<EnqueueThreadGroupIssueResolutionJobRequest>(
                r#"{"expected_membership_snapshot_digest":"digest-a"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<EnqueueThreadGroupIssueResolutionJobRequest>(
                r#"{"target_group_id":"42"}"#
            )
            .is_err()
        );

        let empty_digest = EnqueueThreadGroupIssueResolutionJobRequest {
            target_group_id: 42,
            group_owner_user_id: 7,
            expected_membership_snapshot_digest: "  ".into(),
            dispatch_id: None,
        };
        assert!(validate_thread_group_issue_resolution_request(&empty_digest).is_err());
        let zero_group = EnqueueThreadGroupIssueResolutionJobRequest {
            target_group_id: 0,
            group_owner_user_id: 7,
            expected_membership_snapshot_digest: "digest-a".into(),
            dispatch_id: None,
        };
        assert!(validate_thread_group_issue_resolution_request(&zero_group).is_err());
        let zero_owner = EnqueueThreadGroupIssueResolutionJobRequest {
            target_group_id: 42,
            group_owner_user_id: 0,
            expected_membership_snapshot_digest: "digest-a".into(),
            dispatch_id: None,
        };
        assert!(validate_thread_group_issue_resolution_request(&zero_owner).is_err());

        let negative_owner = EnqueueThreadGroupIssueResolutionJobRequest {
            group_owner_user_id: -1,
            ..zero_owner
        };
        assert!(validate_thread_group_issue_resolution_request(&negative_owner).is_err());

        let negative_group = EnqueueThreadGroupIssueResolutionJobRequest {
            target_group_id: -1,
            group_owner_user_id: 7,
            expected_membership_snapshot_digest: "digest-a".into(),
            dispatch_id: None,
        };
        assert!(validate_thread_group_issue_resolution_request(&negative_group).is_err());
    }

    #[test]
    fn issue_resolution_input_targets_the_new_worker_without_summary_fields() {
        let request = EnqueueThreadGroupIssueResolutionJobRequest {
            target_group_id: 42,
            group_owner_user_id: 7,
            expected_membership_snapshot_digest: "digest-a".into(),
            dispatch_id: Some("resolution-job".into()),
        };
        let input = thread_group_issue_resolution_dispatch_input(
            serde_json::json!({
                "memories_grpc_host": "memories.example",
                "memories_grpc_port": 9010,
                "policy_json": "{\"resolution_mode\":\"DISABLED\"}",
                "llm_worker_name": "memories-llm-external",
            }),
            &request,
        );

        assert_eq!(
            THREAD_GROUP_ISSUE_RESOLUTION_WORKER_NAME,
            "lookback-thread-group-issue-resolution"
        );
        assert_eq!(input["group_id"], "42");
        assert_eq!(input["group_owner_user_id"], 7);
        assert_eq!(input["expected_membership_snapshot_digest"], "digest-a");
        assert_eq!(input["policy_json"], "{\"resolution_mode\":\"DISABLED\"}");
        assert_eq!(input["llm_worker_name"], "memories-llm-external");
        assert!(input.get("force_resummarize").is_none());
    }

    #[test]
    fn thread_group_issue_resolution_workflow_gates_owner_scope_and_filters_lineage() {
        let workflow = include_str!(
            "../../../workers/workflows/thread-group-issue-resolution/thread-group-issue-resolution-batch.yaml"
        );
        let capability_probe = workflow
            .find("/llm_memory.service.ThreadGroupService/GetThreadGroupCapabilities")
            .unwrap();
        let first_lineage_read = workflow
            .find("/llm_memory.service.ThreadGroupService/FindThreadGroupLineage")
            .unwrap();
        let first_llm_call = workflow
            .find("workerName: \"${ $workflow.input.llm_worker_name }")
            .unwrap();

        assert!(capability_probe < first_lineage_read);
        assert!(capability_probe < first_llm_call);
        assert!(workflow.contains("thread-group-issue-resolution/owner-scope-unsupported"));
        assert!(workflow.contains("user_id: $workflow.input.group_owner_user_id"));
    }

    #[test]
    fn thread_issue_resolution_request_requires_a_positive_numeric_thread_id() {
        let request: EnqueueThreadIssueResolutionJobRequest = serde_json::from_str(
            r#"{"target_thread_id":"9007199254740993","dispatch_id":"manual-run"}"#,
        )
        .unwrap();
        assert_eq!(request.target_thread_id, "9007199254740993");
        assert_eq!(request.dispatch_id.as_deref(), Some("manual-run"));
        let legacy_request: EnqueueThreadIssueResolutionJobRequest =
            serde_json::from_str(r#"{"target_thread_id":"42"}"#).unwrap();
        assert_eq!(legacy_request.dispatch_id, None);

        for thread_id in ["", "thread-1", "0", "-1", "9223372036854775808"] {
            let request = EnqueueThreadIssueResolutionJobRequest {
                target_thread_id: thread_id.into(),
                dispatch_id: None,
            };
            assert!(
                validate_thread_issue_resolution_request(&request).is_err(),
                "thread id {thread_id:?} must be rejected"
            );
        }

        let request = EnqueueThreadIssueResolutionJobRequest {
            target_thread_id: "42".into(),
            dispatch_id: None,
        };
        assert_eq!(
            validate_thread_issue_resolution_request(&request).unwrap(),
            "42"
        );
    }

    #[test]
    fn thread_issue_resolution_dispatch_uses_saved_summary_and_resolved_inputs() {
        let memories = crate::commands::connection::GrpcAddress {
            host: "memories.example".into(),
            port: 9010,
            tls: true,
        };
        let tracker = crate::commands::connection::GrpcAddress {
            host: "tracker.example".into(),
            port: 9555,
            tls: false,
        };
        let input = thread_issue_resolution_dispatch_input(
            "42",
            &memories,
            &tracker,
            r#"{"resolution_mode":"APPLY"}"#,
            "memories-llm-custom",
            "en",
        );

        assert_eq!(
            THREAD_ISSUE_RESOLUTION_MANUAL_WORKER_NAME,
            "lookback-thread-issue-resolution-manual"
        );
        assert_eq!(input["thread_id"], "42");
        assert_eq!(input["memories_grpc_host"], "memories.example");
        assert_eq!(input["memories_grpc_port"], 9010);
        assert_eq!(input["memories_grpc_tls"], true);
        assert_eq!(input["issue_tracker_grpc_host"], "tracker.example");
        assert_eq!(input["issue_tracker_grpc_port"], 9555);
        assert_eq!(input["issue_tracker_grpc_tls"], false);
        assert_eq!(input["policy_json"], r#"{"resolution_mode":"APPLY"}"#);
        assert_eq!(input["llm_worker_name"], "memories-llm-custom");
        assert_eq!(input["output_language"], "en");
        assert!(input.get("summary_content").is_none());
        assert!(input.get("force_resummarize").is_none());
    }

    #[test]
    fn manual_thread_resolution_policy_fails_closed_when_not_effectively_applicable() {
        let root = tempfile::tempdir().unwrap();
        let registry = crate::issue_extract::PolicyRegistry::open(root.path());
        let mut apply_policy =
            serde_json::to_value(crate::issue_extract::AutoIssuePolicy::default()).unwrap();
        apply_policy["resolution_mode"] = serde_json::json!("APPLY");
        registry
            .update(serde_json::from_value(apply_policy).unwrap())
            .unwrap();

        let enabled = thread_issue_resolution_policy_json(root.path(), true).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&enabled).unwrap()["resolution_mode"],
            "APPLY"
        );
        assert!(thread_issue_resolution_policy_json(root.path(), false).is_err());

        registry
            .update(crate::issue_extract::AutoIssuePolicy::default())
            .unwrap();
        assert!(thread_issue_resolution_policy_json(root.path(), true).is_err());
    }

    #[test]
    fn thread_issue_resolution_status_reads_only_known_saved_summary_states() {
        assert_eq!(
            thread_issue_resolution_status_from_metadata(Some(
                r#"{"issue_resolution_status":"PENDING"}"#
            )),
            "PENDING"
        );
        assert_eq!(
            thread_issue_resolution_status_from_metadata(Some(
                r#"{"issue_resolution_status":"COMPLETE"}"#
            )),
            "COMPLETE"
        );
        assert_eq!(
            thread_issue_resolution_status_from_metadata(Some(
                r#"{"issue_resolution_status":"UNKNOWN"}"#
            )),
            "UNKNOWN"
        );
        assert_eq!(
            thread_issue_resolution_status_from_metadata(None),
            "NOT_STARTED"
        );
        assert_eq!(
            thread_issue_resolution_status_from_metadata(Some("not-json")),
            "UNKNOWN"
        );
        assert_eq!(
            thread_issue_resolution_status_from_metadata(Some(
                r#"{"issue_resolution_status":"INJECTED"}"#
            )),
            "UNKNOWN"
        );
        assert_eq!(thread_issue_resolution_status(0, None), "NOT_STARTED");
        assert_eq!(
            thread_issue_resolution_status(2, Some(r#"{"issue_resolution_status":"COMPLETE"}"#)),
            "UNKNOWN"
        );
        assert_eq!(
            serde_json::to_value(ThreadIssueResolutionStatus {
                status: "PENDING".into(),
            })
            .unwrap(),
            serde_json::json!({"status": "PENDING"})
        );
    }

    #[test]
    fn thread_group_terminal_outcome_counts_partial_and_snapshot_failures() {
        let raw = r#"{"output":"{\"completed\":false,\"generated_count\":1,\"skipped_count\":1,\"failed_groups\":[{\"group_id\":\"42\",\"reason_code\":\"membership_snapshot_changed\"}]}"}"#;
        let outcome = thread_group_summary_terminal_outcome(raw);
        assert_eq!(outcome.status, StepStatus::Warning);
        assert!(outcome.message.unwrap().contains("1"));

        let failed = r#"{"completed":false,"failed_count":1,"failed_groups":[{"group_id":"42"}]}"#;
        assert_eq!(
            thread_group_summary_terminal_outcome(failed).status,
            StepStatus::Failed
        );
    }

    #[test]
    fn incomplete_targeted_group_result_is_retained_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thread-group-recovery-state.json");
        let context = ThreadGroupRecoveryContext {
            memories_endpoint_id: "local".into(),
            target_group_id: Some(42),
            expected_membership_snapshot_digest: Some("digest-a".into()),
            generation_id: "generation".into(),
        };

        reconcile_thread_group_recovery(
            &path,
            &context,
            r#"{"completed":false,"processed_groups":0,"expected_group_count":1}"#,
        )
        .unwrap();

        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(state["records"].as_array().unwrap().len(), 1);
        assert_eq!(state["records"][0]["group_id"], "42");
        assert_eq!(
            state["records"][0]["expected_membership_snapshot_digest"],
            "digest-a"
        );
    }

    #[test]
    fn targeted_group_result_without_completed_is_retained_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thread-group-recovery-state.json");
        let context = ThreadGroupRecoveryContext {
            memories_endpoint_id: "local".into(),
            target_group_id: Some(42),
            expected_membership_snapshot_digest: Some("digest-a".into()),
            generation_id: "generation".into(),
        };

        reconcile_thread_group_recovery(&path, &context, r#"{"output":{"failed_groups":[]}}"#)
            .unwrap();

        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(state["records"].as_array().unwrap().len(), 1);
        assert_eq!(state["records"][0]["group_id"], "42");
        assert_eq!(state["records"][0]["reason_code"], "workflow_incomplete");
    }

    #[test]
    fn complete_or_accounted_targeted_group_result_never_gets_fallback_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let context = ThreadGroupRecoveryContext {
            memories_endpoint_id: "local".into(),
            target_group_id: Some(42),
            expected_membership_snapshot_digest: Some("digest-a".into()),
            generation_id: "generation".into(),
        };
        let completed_path = dir.path().join("completed.json");
        reconcile_thread_group_recovery(&completed_path, &context, r#"{"completed":true}"#)
            .unwrap();
        assert!(!completed_path.exists());

        let failed_path = dir.path().join("failed.json");
        reconcile_thread_group_recovery(
            &failed_path,
            &context,
            r#"{"completed":false,"failed_groups":[{"group_id":"42","reason_code":"membership_snapshot_changed"}]}"#,
        )
        .unwrap();
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(failed_path).unwrap()).unwrap();
        assert_eq!(state["records"].as_array().unwrap().len(), 1);
        assert_eq!(
            state["records"][0]["reason_code"],
            "membership_snapshot_changed"
        );
    }

    #[test]
    fn recovery_refresh_targets_the_views_reading_each_recovery_state() {
        let period = SummaryRecoveryContext::Period(PeriodRecoveryContext {
            period_kind: PeriodKind::Weekly,
            memories_endpoint_id: "local".into(),
            source_scope_digest: "scope".into(),
            generation_id: "generation".into(),
            generation_started_at: "2026-10-13T00:00:00Z".into(),
            fallback_period_keys: vec!["2026-W41".into()],
        });
        assert_eq!(
            recovery_refresh_scopes(&period),
            vec![GeneratedRefreshScope::WeeklySummary]
        );
    }

    #[test]
    fn period_terminal_failure_creates_an_overall_recovery_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary-recovery-state.json");
        let context = PeriodRecoveryContext {
            period_kind: PeriodKind::Daily,
            memories_endpoint_id: "local".into(),
            source_scope_digest: "scope".into(),
            generation_id: "generation".into(),
            generation_started_at: "2026-09-03T00:00:00Z".into(),
            fallback_period_keys: vec!["2026-09-03".into()],
        };
        let raw = r#"{"output":"{\"failed_dates\":[\"2026-09-03\"]}"}"#;

        reconcile_period_recovery(&path, &context, raw).unwrap();

        let raw_state = std::fs::read_to_string(path).unwrap();
        let state: serde_json::Value = serde_json::from_str(&raw_state).unwrap();
        assert_eq!(state["records"].as_array().unwrap().len(), 1);
        assert_eq!(state["records"][0]["period_key"], "2026-09-03");
        assert_eq!(state["records"][0]["rollup_scope"], "overall");
    }

    #[test]
    fn failed_period_stream_without_a_terminal_output_is_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary-recovery-state.json");
        let context = PeriodRecoveryContext {
            period_kind: PeriodKind::Daily,
            memories_endpoint_id: "local".into(),
            source_scope_digest: "scope".into(),
            generation_id: "generation".into(),
            generation_started_at: "2026-09-03T00:00:00Z".into(),
            fallback_period_keys: vec!["2026-09-03".into()],
        };

        record_failed_period_stream_recovery(&path, &context).unwrap();

        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(state["records"].as_array().unwrap().len(), 1);
        assert_eq!(state["records"][0]["period_key"], "2026-09-03");
        assert_eq!(
            state["records"][0]["reason_code"],
            "workflow_execution_failed"
        );
    }

    #[test]
    fn incomplete_period_output_without_identified_failures_uses_selected_periods_for_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary-recovery-state.json");
        let context = PeriodRecoveryContext {
            period_kind: PeriodKind::Weekly,
            memories_endpoint_id: "local".into(),
            source_scope_digest: "scope".into(),
            generation_id: "generation".into(),
            generation_started_at: "2026-09-03T00:00:00Z".into(),
            fallback_period_keys: vec!["2026-W36".into(), "2026-W37".into()],
        };

        reconcile_period_recovery(
            &path,
            &context,
            r#"{"output":"{\"completed\":false,\"failed_count\":0,\"failed_weeks\":[],\"succeeded_weeks\":[],\"failed_path_buckets\":[]}"}"#,
        )
        .unwrap();

        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let records = state["records"].as_array().unwrap();
        assert_eq!(records.len(), 2);
        assert!(
            records
                .iter()
                .all(|record| record["period_kind"] == "weekly")
        );
        assert!(
            records
                .iter()
                .all(|record| record["rollup_scope"] == "overall")
        );
    }

    #[test]
    fn period_terminal_path_failure_is_retained_until_a_complete_period_retries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary-recovery-state.json");
        let context = PeriodRecoveryContext {
            period_kind: PeriodKind::Daily,
            memories_endpoint_id: "local".into(),
            source_scope_digest: "scope".into(),
            generation_id: "generation".into(),
            generation_started_at: "2026-09-03T00:00:00Z".into(),
            fallback_period_keys: vec!["2026-09-03".into()],
        };
        let failed = r#"{"output":"{\"failed_path_buckets\":[{\"period_key\":\"2026-09-03\",\"path_bucket_kind\":\"label\",\"path_label\":\"path:/workspace/example\",\"error\":\"timeout\"}]}"}"#;
        reconcile_period_recovery(&path, &context, failed).unwrap();

        let partial_success = r#"{"output":"{\"succeeded_dates\":[\"2026-09-03\"],\"failed_path_buckets\":[{\"period_key\":\"2026-09-03\",\"path_bucket_kind\":\"label\",\"path_label\":\"path:/workspace/example\"}]}"}"#;
        reconcile_period_recovery(&path, &context, partial_success).unwrap();
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(state["records"].as_array().unwrap().len(), 1);
        assert_eq!(state["records"][0]["rollup_scope"], "path");

        let complete =
            r#"{"output":"{\"succeeded_dates\":[\"2026-09-03\"],\"failed_path_buckets\":[]}"}"#;
        reconcile_period_recovery(&path, &context, complete).unwrap();
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(state["records"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn successful_period_generation_clears_legacy_pipeline_recovery_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary-recovery-state.json");
        let legacy = super::super::summary_recovery::build_record(
            recovery_identity("local", "scope", "old-generation", "manual"),
            "daily",
            "2026-09-03",
            super::super::summary_recovery::SummaryRecoveryScope::Overall,
            "summaries_pipeline",
            "workflow_execution_failed",
            Some("legacy pipeline stopped before terminal output".into()),
        );
        super::super::summary_recovery::record(&path, legacy).unwrap();
        let context = PeriodRecoveryContext {
            period_kind: PeriodKind::Daily,
            memories_endpoint_id: "local".into(),
            source_scope_digest: "scope".into(),
            generation_id: "new-generation".into(),
            generation_started_at: "2026-10-15T10:11:12Z".into(),
            fallback_period_keys: vec!["2026-09-03".into()],
        };

        reconcile_period_recovery(
            &path,
            &context,
            r#"{"output":"{\"completed\":true,\"succeeded_dates\":[\"2026-09-03\"],\"failed_dates\":[],\"failed_path_buckets\":[]}"}"#,
        )
        .unwrap();

        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(state["records"].as_array().unwrap().is_empty());
    }

    #[test]
    fn summary_generation_stages_keep_order_and_independent_selection() {
        let req = GenerateSummariesRequest {
            run_thread_group: true,
            run_weekly: true,
            ..GenerateSummariesRequest::default()
        };
        let stages = summary_generation_stage_specs(&req);

        assert_eq!(
            stages.map(|stage| stage.step),
            [
                StagedStep::ThreadSummary,
                StagedStep::ThreadGroupSummary,
                StagedStep::Daily,
                StagedStep::Weekly,
                StagedStep::Monthly,
            ]
        );
        assert_eq!(
            stages.map(|stage| stage.selected),
            [false, true, false, true, false]
        );
    }

    #[test]
    fn stop_on_error_includes_warning_stages_with_reported_item_failures() {
        let warning = StepOutcome {
            status: StepStatus::Warning,
            message: Some("path failure".into()),
        };
        assert!(summary_stage_should_stop(&warning, true));
        assert!(!summary_stage_should_stop(&warning, false));
        assert!(summary_stage_should_stop(
            &StepOutcome {
                status: StepStatus::Failed,
                message: None,
            },
            false,
        ));
    }

    #[test]
    fn staged_summary_fallback_expands_each_explicit_period_range() {
        let req = GenerateSummariesRequest {
            daily_start: "2026-09-01".into(),
            daily_end: "2026-09-02".into(),
            weekly_start: "2026-W36".into(),
            weekly_end: "2026-W37".into(),
            monthly_start: "2026-08".into(),
            monthly_end: "2026-09".into(),
            ..GenerateSummariesRequest::default()
        };
        assert_eq!(
            summary_generation_fallback_period_keys(
                PeriodKind::Daily,
                &req,
                chrono_tz::Asia::Tokyo,
            ),
            vec!["2026-09-01", "2026-09-02"]
        );
        assert_eq!(
            summary_generation_fallback_period_keys(
                PeriodKind::Weekly,
                &req,
                chrono_tz::Asia::Tokyo,
            ),
            vec!["2026-W36", "2026-W37"]
        );
        assert_eq!(
            summary_generation_fallback_period_keys(
                PeriodKind::Monthly,
                &req,
                chrono_tz::Asia::Tokyo,
            ),
            vec!["2026-08", "2026-09"]
        );
    }

    #[test]
    fn implicit_fallback_period_uses_the_worker_timezone() {
        let instant = chrono::DateTime::parse_from_rfc3339("2026-09-04T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            fallback_period_keys_at(PeriodKind::Daily, Some(1), instant, chrono_tz::Asia::Tokyo,),
            vec!["2026-09-03"]
        );
        assert_eq!(
            fallback_period_keys_at(
                PeriodKind::Daily,
                Some(1),
                instant,
                chrono_tz::America::New_York,
            ),
            vec!["2026-09-02"]
        );
    }

    #[test]
    fn resolve_dispatch_id_uses_supplied_value_when_present() {
        // The frontend-generated UUID becomes the cancel key, so it must
        // pass through verbatim — otherwise a Cancel click targets a
        // different id and the JobService/Delete misses.
        let id = resolve_dispatch_id(Some("uuid-1234"), "summary");
        assert_eq!(id, "uuid-1234");
    }

    #[test]
    fn resolve_dispatch_id_falls_back_to_legacy_shape() {
        // Older frontends omit dispatch_id; the synthesized fallback
        // keeps the legacy `<prefix>-<ts>` shape so anything inspecting
        // job ids (logs, tests) still matches the old format.
        let id = resolve_dispatch_id(None, "summary");
        assert!(id.starts_with("summary-"), "legacy shape lost; got {id}");
        let id_personality = resolve_dispatch_id(Some(""), "personality");
        assert!(
            id_personality.starts_with("personality-"),
            "empty string treated as 'absent' so the fallback fires"
        );
    }

    #[test]
    fn worker_names_match_import_pipeline() {
        // Drift guard: these must equal the worker names the import
        // success path dispatches, or the standalone buttons would target
        // a different (non-existent) worker.
        assert_eq!(SUMMARY_WORKER_NAME, "memories-summarize-batch");
        assert_eq!(PERSONALITY_WORKER_NAME, "memories-personality-batch");
        // The merge is now a language-specific lang-worker
        // (`memories-user-personality-merge-<lang>`) registered by
        // `upsert-generation-workers`; the personality batch resolves it by
        // name for its tail step, and the standalone button reuses the same
        // base. Drift-guarded so a rename surfaces here, not as a runtime
        // WorkerNotFound.
        assert_eq!(
            PERSONALITY_MERGE_WORKER_BASE,
            "memories-user-personality-merge"
        );
    }

    #[test]
    fn period_worker_names_match_registered_workers() {
        // Drift guard: these literals must match the `name:` entries in
        // workers/llm-workers.yaml or the dispatch hits a missing worker.
        assert_eq!(
            PeriodKind::Daily.worker_name(),
            "memories-daily-summary-batch"
        );
        assert_eq!(
            PeriodKind::Weekly.worker_name(),
            "memories-weekly-summary-batch"
        );
        assert_eq!(
            PeriodKind::Monthly.worker_name(),
            "memories-monthly-summary-batch"
        );
    }

    #[test]
    fn period_job_ids_distinguish_by_kind() {
        let daily = format!(
            "{}-{}",
            PeriodKind::Daily.job_prefix(),
            1_700_000_000_000_i64
        );
        let weekly = format!(
            "{}-{}",
            PeriodKind::Weekly.job_prefix(),
            1_700_000_000_000_i64
        );
        assert!(daily.starts_with("daily-"));
        assert!(weekly.starts_with("weekly-"));
        assert_ne!(daily, weekly);
    }

    #[test]
    fn period_summary_reuses_summary_event() {
        // The period dispatch intentionally emits on the shared summary
        // event so the existing progress hook/invalidation covers it.
        assert_eq!(SUMMARY_EVENT, "summary://step");
    }

    #[test]
    fn period_refresh_scope_matches_kind() {
        assert_eq!(
            period_refresh_scope(PeriodKind::Daily),
            GeneratedRefreshScope::DailySummary
        );
        assert_eq!(
            period_refresh_scope(PeriodKind::Weekly),
            GeneratedRefreshScope::WeeklySummary
        );
        assert_eq!(
            period_refresh_scope(PeriodKind::Monthly),
            GeneratedRefreshScope::MonthlySummary
        );
    }

    #[test]
    fn generated_refresh_scope_serializes_for_frontend() {
        let s = serde_json::to_string(&GeneratedRefreshUpdate {
            job_id: "job-1".into(),
            scopes: vec![
                GeneratedRefreshScope::ThreadSummary,
                GeneratedRefreshScope::DailySummary,
                GeneratedRefreshScope::Personality,
            ],
        })
        .unwrap();
        assert_eq!(
            s,
            r#"{"job_id":"job-1","scopes":["thread_summary","daily_summary","personality"]}"#
        );
    }

    #[test]
    fn failed_stream_refresh_scopes_reissues_known_persisted_scopes() {
        let terminal = RefreshMode::Terminal(vec![GeneratedRefreshScope::Personality]);
        assert_eq!(
            failed_stream_refresh_scopes(&terminal),
            vec![GeneratedRefreshScope::Personality]
        );
    }

    #[test]
    fn job_id_prefixes_distinguish_the_two_dispatches() {
        let summary = format!("summary-{}", 1_700_000_000_000_i64);
        let personality = format!("personality-{}", 1_700_000_000_000_i64);
        assert!(summary.starts_with("summary-"));
        assert!(personality.starts_with("personality-"));
        assert_ne!(summary, personality);
    }

    #[test]
    fn progress_chunk_is_digested_not_emitted_raw() {
        // Regression: the personality progress used to surface the raw
        // WorkflowResult JSON. The dispatch must run it through the import
        // digest so the user sees `(1/59) 進捗を更新中`, not the JSON blob.
        let raw = r#"{"id":"019e57b5","output":"{\"progress_processed\":1,\"progress_total\":59}","position":"/ROOT/do/7/personalityEach/for/do/0/reportProgress","status":"Running","errorMessage":"{\"progress_processed\":1,\"progress_total\":59}"}"#;
        let (digest, progress) = summarize_workflow_chunk(raw, None);
        assert_eq!(digest, "(1/59) 進捗を更新中");
        assert_eq!(progress, Some((1, 59)));
        assert!(
            !digest.contains('{'),
            "raw JSON must not leak into the digest"
        );
    }

    #[test]
    fn thread_group_summary_dispatch_requires_healthy_thread_group_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let summary_recovery_path = directory.path().join("summary-recovery.json");
        let thread_group_recovery_path = directory.path().join("thread-group-recovery.json");
        std::fs::write(&thread_group_recovery_path, "not valid json").unwrap();

        assert!(
            ensure_summary_dispatch_recoveries_are_healthy(
                &summary_recovery_path,
                &thread_group_recovery_path,
                false,
            )
            .is_ok()
        );
        assert!(
            ensure_summary_dispatch_recoveries_are_healthy(
                &summary_recovery_path,
                &thread_group_recovery_path,
                true,
            )
            .is_err()
        );
    }
}
