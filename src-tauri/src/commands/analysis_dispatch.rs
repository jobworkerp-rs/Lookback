//! Tauri commands for the standalone "generate summary / personality"
//! buttons (run the analysis later, after an import-only run). Mirrors
//! `reflection_dispatch` but builds the workflow input via `import`'s
//! `BatchDispatch` so the wire-shape stays a single source of truth.
//!
//! Each command returns immediately with a `job_id_hint`; the stream is
//! consumed in a detached task that emits `summary://step` /
//! `personality://step` events `{ job_id, status, message }`.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::error::AppResult;
use crate::jobworkerp::{JobworkerpHandle, StreamEvent, run_cancellable_named_stream};

use super::import::{
    BatchDispatch, GenerateSummariesRequest, PeriodKind, PeriodRange, summarize_workflow_chunk,
    summarize_workflow_error,
};
use super::{
    AppState, DispatchCancelEntry, GeneratedRefreshScope, StepOutcome, StepStatus,
    cancel_dispatch_inner, emit_event, emit_generated_refresh,
    generated_refresh_scopes_from_position, json_as_i64,
};

// Worker names must match what the import pipeline dispatches
// (import.rs success path). Kept as constants here and guarded by a
// drift test against those literals. `pub(crate)` so the Settings queue
// card (`background_jobs.rs`) can classify counts by the same names
// instead of carrying its own copy that could drift.
pub(crate) const SUMMARY_WORKER_NAME: &str = "memories-summarize-batch";
pub(crate) const PERSONALITY_WORKER_NAME: &str = "memories-personality-batch";
/// Base name of the Layer-2 merge lang-worker. The full worker name is
/// `<base>-<lang>` (`memories-user-personality-merge-ja` / `-en`), registered
/// by `memories-import upsert-generation-workers`. The Personality tab uses it
/// to skip per-thread fan-out and re-run the merge alone (e.g. after a 429
/// storm left signals but no profile). Same lang-worker the personality batch
/// resolves for its tail merge step.
pub(crate) const PERSONALITY_MERGE_WORKER_BASE: &str = "memories-user-personality-merge";
/// Staged pipeline parent (per-thread → daily → weekly → monthly). Must
/// match the `name:` in workers/llm-workers.yaml.
pub(crate) const SUMMARIES_PIPELINE_WORKER_NAME: &str = "memories-summaries-pipeline";
/// A bounded operational guardrail: jobs that cannot keep up with normal
/// real-time summary volume must be retried as smaller backfill ranges.
const SUMMARIES_PIPELINE_JOB_TIMEOUT_SEC: u64 = 72 * 24 * 60 * 60;

fn summaries_pipeline_request_timeout_sec() -> u64 {
    SUMMARIES_PIPELINE_JOB_TIMEOUT_SEC
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
    SummariesPipeline,
}

#[tauri::command]
pub async fn enqueue_summary_job(
    app: AppHandle,
    state: State<'_, AppState>,
    req: EnqueueSummaryJobRequest,
) -> AppResult<EnqueueAnalysisJobResponse> {
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

    let target = state.resolve_targets()?;
    let handle = JobworkerpHandle::connect_with_timeout(
        &target.jobworkerp_url,
        SUMMARIES_PIPELINE_JOB_TIMEOUT_SEC,
    )
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
    let entry = state.dispatch_register(&job_id).await;
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
pub async fn enqueue_personality_job(
    app: AppHandle,
    state: State<'_, AppState>,
    req: EnqueuePersonalityJobRequest,
) -> AppResult<EnqueueAnalysisJobResponse> {
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

    let handle = state.jobworkerp().await?;
    let job_id = resolve_dispatch_id(req.dispatch_id.as_deref(), "personality");
    let maintenance_registered = super::begin_search_index_write(
        &state,
        &[
            crate::search_index_maintenance::MaintenanceTable::Memory,
            crate::search_index_maintenance::MaintenanceTable::Thread,
        ],
    )
    .await?;
    let entry = state.dispatch_register(&job_id).await;
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

    let handle = state.jobworkerp().await?;
    let job_id = resolve_dispatch_id(req.dispatch_id.as_deref(), "personality-merge");
    let maintenance_registered = super::begin_search_index_write(
        &state,
        &[
            crate::search_index_maintenance::MaintenanceTable::Memory,
            crate::search_index_maintenance::MaintenanceTable::Thread,
        ],
    )
    .await?;
    let entry = state.dispatch_register(&job_id).await;
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
    // Generation writes summary / personality embeddings into the local
    // LanceDB; refuse when it is degraded (local mode only).
    state.ensure_local_embedding_available()?;
    let callback = state.resolve_targets()?.memories_callback()?;
    // Period summaries always operate on the synthetic owner; the importing
    // user_id is irrelevant here, so resolve with a placeholder.
    let dispatch = BatchDispatch::resolve(
        &callback,
        1,
        None,
        state.active_llm_worker_name().to_string(),
        state.active_output_language(),
    )?;
    let range = req.last_n.map_or(PeriodRange::Auto, PeriodRange::LastN);
    let args = super::wrap_workflow_run_args(&dispatch.period_input(req.kind, range));

    let handle = state.jobworkerp().await?;
    let job_id = resolve_dispatch_id(req.dispatch_id.as_deref(), req.kind.job_prefix());
    let maintenance_registered = super::begin_search_index_write(
        &state,
        &[
            crate::search_index_maintenance::MaintenanceTable::Memory,
            crate::search_index_maintenance::MaintenanceTable::Thread,
        ],
    )
    .await?;
    let entry = state.dispatch_register(&job_id).await;
    spawn_step_stream(
        app,
        handle,
        req.kind.worker_name().to_string(),
        args,
        &job_id,
        SUMMARY_EVENT,
        entry,
        RefreshMode::Terminal(vec![period_refresh_scope(req.kind)]),
        None,
        maintenance_registered,
    );

    Ok(EnqueueAnalysisJobResponse {
        job_id_hint: job_id,
    })
}

/// Dispatch the staged summaries pipeline: the per-thread → daily → weekly →
/// monthly chain in one workflow gated by the dialog's `run_*` flags. Progress
/// and failures stream on the shared `summary://step` event.
#[tauri::command]
pub async fn generate_summaries(
    app: AppHandle,
    state: State<'_, AppState>,
    req: GenerateSummariesRequest,
) -> AppResult<EnqueueAnalysisJobResponse> {
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
    // The per-thread epoch bounds ride on the request (forwarded by
    // pipeline_input), so the dispatch window stays None.
    let dispatch = BatchDispatch::resolve(
        &callback,
        user_id,
        None,
        state.active_llm_worker_name().to_string(),
        state.active_output_language(),
    )?;
    let args = super::wrap_workflow_run_args(&dispatch.pipeline_input(&req));

    // This pipeline can run for days. Do not reuse AppState's shared
    // three-hour client, or its stream deadline will expire while the
    // server-side job is still running.
    let target = state.resolve_targets()?;
    let handle = JobworkerpHandle::connect_with_timeout(
        &target.jobworkerp_url,
        summaries_pipeline_request_timeout_sec(),
    )
    .await?;
    let job_id = resolve_dispatch_id(req.dispatch_id.as_deref(), "summaries");
    let maintenance_registered = super::begin_search_index_write(
        &state,
        &[
            crate::search_index_maintenance::MaintenanceTable::Memory,
            crate::search_index_maintenance::MaintenanceTable::Thread,
        ],
    )
    .await?;
    let entry = state.dispatch_register(&job_id).await;
    spawn_step_stream(
        app,
        handle,
        SUMMARIES_PIPELINE_WORKER_NAME.to_string(),
        args,
        &job_id,
        SUMMARY_EVENT,
        entry,
        RefreshMode::SummariesPipeline,
        Some(SUMMARIES_PIPELINE_JOB_TIMEOUT_SEC),
        maintenance_registered,
    );

    Ok(EnqueueAnalysisJobResponse {
        job_id_hint: job_id,
    })
}

/// Cancel an in-flight analysis dispatch (summary / personality / period
/// summary / staged summaries pipeline). Mirrors `chat_cancel` semantics:
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
    job_timeout_sec: Option<u64>,
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
        let job_id_for_emit = job_id.clone();
        let app_for_emit = app.clone();
        let event_name_for_emit: &str = event_name;
        // `Some("run")`: WORKFLOW runner has create + run methods; we
        // execute the pre-registered workflow. See import.rs for the
        // same rationale.
        let park_job_id = current_job_id.clone();
        let cancel_for_emit = cancel.clone();
        let mut emitted_refresh_scopes: HashSet<GeneratedRefreshScope> = HashSet::new();
        run_cancellable_named_stream(
            &handle,
            &worker_name,
            args,
            Some("run"),
            job_timeout_sec,
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
                            if matches!(refresh_mode, RefreshMode::SummariesPipeline) {
                                emit_new_refresh_scopes(
                                    &app_for_emit,
                                    &job_id_for_emit,
                                    &mut emitted_refresh_scopes,
                                    pipeline_refresh_scopes(raw),
                                );
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
                    StreamEvent::Done(msg) => {
                        if let RefreshMode::Terminal(scopes) = &refresh_mode {
                            // A terminal refresh is intentionally not deduplicated
                            // against an Active chunk: it closes the race where the
                            // last persisted result arrives after the final chunk.
                            emit_generated_refresh(&app_for_emit, &job_id_for_emit, scopes.clone());
                        }
                        match (&refresh_mode, msg) {
                            (RefreshMode::SummariesPipeline, Some(raw)) => {
                                // Re-emit the terminal stage scopes even when Active
                                // chunks already refreshed them.
                                let (scopes, outcome) = pipeline_terminal_update(raw);
                                emit_generated_refresh(&app_for_emit, &job_id_for_emit, scopes);
                                (outcome.status, outcome.message)
                            }
                            (RefreshMode::SummariesPipeline, None) => (
                                StepStatus::Failed,
                                Some("パイプラインの終端出力がありません".to_string()),
                            ),
                            (RefreshMode::Terminal(_), raw) => (
                                StepStatus::Done,
                                raw.map(|chunk| summarize_workflow_chunk(chunk, last_progress).0),
                            ),
                        }
                    }
                    StreamEvent::Failed(msg) => {
                        emit_generated_refresh(
                            &app_for_emit,
                            &job_id_for_emit,
                            failed_stream_refresh_scopes(&refresh_mode, &emitted_refresh_scopes),
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

fn failed_stream_refresh_scopes(
    refresh_mode: &RefreshMode,
    emitted_refresh_scopes: &HashSet<GeneratedRefreshScope>,
) -> Vec<GeneratedRefreshScope> {
    match refresh_mode {
        RefreshMode::Terminal(scopes) => scopes.clone(),
        // The staged pipeline has no fixed output set. Re-notify precisely
        // the scopes whose persisted output was observed before failure.
        RefreshMode::SummariesPipeline => emitted_refresh_scopes.iter().copied().collect(),
    }
}

fn emit_new_refresh_scopes(
    app: &AppHandle,
    job_id: &str,
    emitted: &mut HashSet<GeneratedRefreshScope>,
    scopes: Vec<GeneratedRefreshScope>,
) {
    let fresh: Vec<GeneratedRefreshScope> = scopes
        .into_iter()
        .filter(|scope| emitted.insert(*scope))
        .collect();
    emit_generated_refresh(app, job_id, fresh);
}

const PIPELINE_STAGE_SCOPE_KEYS: [(&str, GeneratedRefreshScope); 12] = [
    ("per_thread_result", GeneratedRefreshScope::ThreadSummary),
    ("per_thread", GeneratedRefreshScope::ThreadSummary),
    ("daily_result", GeneratedRefreshScope::DailySummary),
    ("daily", GeneratedRefreshScope::DailySummary),
    // A per-day result is emitted only after its thread and daily
    // dependencies have both run, so refresh both derived views.
    ("per_day_result", GeneratedRefreshScope::ThreadSummary),
    ("per_day_result", GeneratedRefreshScope::DailySummary),
    ("per_day", GeneratedRefreshScope::ThreadSummary),
    ("per_day", GeneratedRefreshScope::DailySummary),
    ("weekly_result", GeneratedRefreshScope::WeeklySummary),
    ("weekly", GeneratedRefreshScope::WeeklySummary),
    ("monthly_result", GeneratedRefreshScope::MonthlySummary),
    ("monthly", GeneratedRefreshScope::MonthlySummary),
];

fn pipeline_refresh_scopes(raw: &str) -> Vec<GeneratedRefreshScope> {
    pipeline_output_value(raw)
        .as_ref()
        .map_or_else(Vec::new, pipeline_refresh_scopes_from_output)
}

fn pipeline_refresh_scopes_from_output(output: &serde_json::Value) -> Vec<GeneratedRefreshScope> {
    PIPELINE_STAGE_SCOPE_KEYS
        .iter()
        .copied()
        .filter_map(|(key, scope)| pipeline_stage_result_present(output, key).then_some(scope))
        .fold(Vec::new(), |mut acc, scope| {
            if !acc.contains(&scope) {
                acc.push(scope);
            }
            acc
        })
}

fn pipeline_output_value(raw: &str) -> Option<serde_json::Value> {
    let v = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    if pipeline_has_stage_keys(&v) || pipeline_has_terminal_contract(&v) {
        return Some(v);
    }
    match v.get("output") {
        Some(serde_json::Value::String(s)) => serde_json::from_str::<serde_json::Value>(s).ok(),
        Some(output @ serde_json::Value::Object(_)) => Some(output.clone()),
        _ => v
            .as_str()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
    }
}

fn pipeline_has_terminal_contract(v: &serde_json::Value) -> bool {
    v.get("completed").is_some() || v.get("outcome").is_some()
}

/// Classify the pipeline's explicit terminal contract. Unlike ordinary batch
/// workers, a completed parent workflow can intentionally carry partial
/// failures, so its `completed`/`outcome` pair is authoritative.
fn pipeline_terminal_update(raw: &str) -> (Vec<GeneratedRefreshScope>, StepOutcome) {
    let Some(output) = pipeline_output_value(raw) else {
        return (Vec::new(), invalid_pipeline_terminal_output());
    };
    (
        pipeline_refresh_scopes_from_output(&output),
        summarize_pipeline_terminal_outcome_from_output(&output),
    )
}

fn invalid_pipeline_terminal_output() -> StepOutcome {
    StepOutcome {
        status: StepStatus::Failed,
        message: Some("パイプラインの終端出力を解釈できません".to_string()),
    }
}

fn summarize_pipeline_terminal_outcome_from_output(output: &serde_json::Value) -> StepOutcome {
    match (
        output.get("completed").and_then(serde_json::Value::as_bool),
        output.get("outcome").and_then(serde_json::Value::as_str),
    ) {
        (Some(true), Some("succeeded")) => StepOutcome {
            status: StepStatus::Done,
            message: None,
        },
        (Some(false), Some("partial")) => {
            let per_day_failures = output
                .get("per_day_failed_dates")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            let stage_failures: i64 = ["per_thread", "daily", "weekly", "monthly"]
                .into_iter()
                .filter_map(|key| output.get(key))
                .filter_map(|stage| stage.get("failed_count"))
                .filter_map(json_as_i64)
                .sum();
            let execution_failures = output
                .get("execution_failures")
                .and_then(serde_json::Value::as_array)
                .map(|failures| {
                    failures
                        .iter()
                        .filter_map(|failure| {
                            let stage = failure.get("stage")?.as_str()?;
                            let detail = failure
                                .get("detail")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("実行エラー");
                            Some(format!("{stage}: {detail}"))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let execution_line = if execution_failures.is_empty() {
                String::new()
            } else {
                format!("\n実行失敗: {}", execution_failures.join("、"))
            };
            StepOutcome {
                status: StepStatus::Warning,
                message: Some(format!(
                    "一部失敗（stage {stage_failures} 件、日別 {per_day_failures} 件）{execution_line}"
                )),
            }
        }
        _ => StepOutcome {
            status: StepStatus::Failed,
            message: Some("パイプラインの終端出力が不正です".to_string()),
        },
    }
}

fn pipeline_has_stage_keys(v: &serde_json::Value) -> bool {
    PIPELINE_STAGE_SCOPE_KEYS
        .iter()
        .any(|(key, _)| v.get(key).is_some())
}

fn pipeline_stage_result_present(output: &serde_json::Value, key: &str) -> bool {
    match output.get(key) {
        Some(serde_json::Value::Null) | None => false,
        Some(serde_json::Value::Object(map)) => !map.is_empty(),
        Some(_) => true,
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
    use super::*;
    use crate::commands::GeneratedRefreshUpdate;

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
        let pm: EnqueuePersonalityMergeJobRequest =
            serde_json::from_str(r#"{"dispatch_id": "abc-merge"}"#).unwrap();
        assert_eq!(pm.dispatch_id.as_deref(), Some("abc-merge"));
        // Omitting dispatch_id keeps the field None so the resolver can
        // synthesize the legacy timestamp-shaped id.
        let s_absent: EnqueueSummaryJobRequest = serde_json::from_str("{}").unwrap();
        assert!(s_absent.dispatch_id.is_none());
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
    fn pipeline_worker_name_matches_registered_worker() {
        // Drift guard: must match the `name:` in workers/llm-workers.yaml
        // and the $file workflow under workers/workflows/summaries-pipeline/.
        assert_eq!(
            SUMMARIES_PIPELINE_WORKER_NAME,
            "memories-summaries-pipeline"
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
    fn pipeline_refresh_scopes_reads_stage_results_from_running_context() {
        let chunk = serde_json::json!({
            "status": "Running",
            "output": serde_json::json!({
                "per_thread_result": { "processed_threads": 3, "succeeded_count": 3 },
                "daily_result": null,
                "weekly_result": {},
                "monthly_result": null
            }).to_string()
        })
        .to_string();

        assert_eq!(
            pipeline_refresh_scopes(&chunk),
            vec![GeneratedRefreshScope::ThreadSummary]
        );
    }

    #[test]
    fn pipeline_refresh_scopes_reads_terminal_pipeline_output() {
        let chunk = serde_json::json!({
            "status": "Completed",
            "output": serde_json::json!({
                "completed": true,
                "per_thread": { "processed_threads": 3, "succeeded_count": 3 },
                "daily": { "processed_dates": 1, "succeeded_count": 1 },
                "weekly": null,
                "monthly": { "processed_months": 1, "succeeded_count": 1 }
            }).to_string()
        })
        .to_string();

        assert_eq!(
            pipeline_refresh_scopes(&chunk),
            vec![
                GeneratedRefreshScope::ThreadSummary,
                GeneratedRefreshScope::DailySummary,
                GeneratedRefreshScope::MonthlySummary,
            ]
        );
    }

    #[test]
    fn pipeline_refresh_scopes_reads_per_day_results_as_both_dependencies() {
        let chunk = serde_json::json!({
            "status": "Completed",
            "output": serde_json::json!({
                "completed": true,
                "per_thread": null,
                "daily": null,
                "per_day": { "processed_dates": 1, "succeeded_count": 1 },
                "weekly": null,
                "monthly": null
            }).to_string()
        })
        .to_string();

        assert_eq!(
            pipeline_refresh_scopes(&chunk),
            vec![
                GeneratedRefreshScope::ThreadSummary,
                GeneratedRefreshScope::DailySummary,
            ]
        );
    }

    #[test]
    fn pipeline_refresh_scopes_reads_direct_terminal_output() {
        let raw = serde_json::json!({
            "completed": true,
            "per_thread": { "processed_threads": 2, "succeeded_count": 2 },
            "daily": null,
            "weekly": null,
            "monthly": null
        })
        .to_string();

        assert_eq!(
            pipeline_refresh_scopes(&raw),
            vec![GeneratedRefreshScope::ThreadSummary]
        );
    }

    #[test]
    fn pipeline_refresh_scopes_reads_json_string_terminal_output() {
        let output = serde_json::json!({
            "completed": true,
            "per_thread": { "processed_threads": 2, "succeeded_count": 2 },
            "daily": { "processed_dates": 1, "succeeded_count": 1 }
        })
        .to_string();
        let raw = serde_json::Value::String(output).to_string();

        assert_eq!(
            pipeline_refresh_scopes(&raw),
            vec![
                GeneratedRefreshScope::ThreadSummary,
                GeneratedRefreshScope::DailySummary,
            ]
        );
    }

    #[test]
    fn pipeline_terminal_update_derives_scopes_and_outcome_from_one_output() {
        let raw = r#"{"status":"Completed","output":"{\"completed\":false,\"outcome\":\"partial\",\"per_thread\":{\"failed_count\":1}}"}"#;

        let (scopes, outcome) = pipeline_terminal_update(raw);

        assert_eq!(scopes, vec![GeneratedRefreshScope::ThreadSummary]);
        assert_eq!(outcome.status, StepStatus::Warning);
        assert!(outcome.message.unwrap().contains("stage 1 件"));
    }

    #[test]
    fn pipeline_terminal_outcome_distinguishes_success_partial_and_invalid_output() {
        let succeeded =
            r#"{"status":"Completed","output":"{\"completed\":true,\"outcome\":\"succeeded\"}"}"#;
        assert_eq!(
            pipeline_terminal_update(succeeded).1.status,
            StepStatus::Done
        );

        let partial = r#"{"status":"Completed","output":"{\"completed\":false,\"outcome\":\"partial\",\"per_day_failed_dates\":[\"2026-08-01\"],\"execution_failures\":[{\"stage\":\"weekly\",\"detail\":\"worker timeout\"}]}"}"#;
        let partial_outcome = pipeline_terminal_update(partial).1;
        assert_eq!(partial_outcome.status, StepStatus::Warning);
        let message = partial_outcome.message.unwrap();
        assert!(message.contains("一部失敗"));
        assert!(message.contains("weekly: worker timeout"));

        let malformed = r#"{"status":"Completed","output":"{\"outcome\":\"unexpected\"}"}"#;
        assert_eq!(
            pipeline_terminal_update(malformed).1.status,
            StepStatus::Failed
        );

        let direct = r#"{"completed":true,"outcome":"succeeded"}"#;
        assert_eq!(pipeline_terminal_update(direct).1.status, StepStatus::Done);
    }

    #[test]
    fn pipeline_terminal_outcome_accepts_numeric_and_string_failed_counts() {
        for failed_count in [serde_json::json!(2), serde_json::json!("2")] {
            let raw = serde_json::json!({
                "completed": false,
                "outcome": "partial",
                "per_thread": { "failed_count": failed_count },
            })
            .to_string();

            let outcome = pipeline_terminal_update(&raw).1;
            assert_eq!(outcome.status, StepStatus::Warning);
            assert!(outcome.message.unwrap().contains("stage 2 件"));
        }
    }

    #[test]
    fn failed_stream_refresh_scopes_reissues_known_persisted_scopes() {
        let terminal = RefreshMode::Terminal(vec![GeneratedRefreshScope::Personality]);
        assert_eq!(
            failed_stream_refresh_scopes(&terminal, &HashSet::new()),
            vec![GeneratedRefreshScope::Personality]
        );

        let observed = HashSet::from([
            GeneratedRefreshScope::ThreadSummary,
            GeneratedRefreshScope::WeeklySummary,
        ]);
        let scopes = failed_stream_refresh_scopes(&RefreshMode::SummariesPipeline, &observed);
        assert_eq!(scopes.len(), 2);
        assert!(scopes.contains(&GeneratedRefreshScope::ThreadSummary));
        assert!(scopes.contains(&GeneratedRefreshScope::WeeklySummary));
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
    fn summaries_pipeline_job_timeout_covers_all_stage_budgets() {
        assert_eq!(SUMMARIES_PIPELINE_JOB_TIMEOUT_SEC, 72 * 24 * 60 * 60);
    }

    #[test]
    fn summaries_pipeline_client_deadline_matches_its_job_deadline() {
        assert_eq!(
            summaries_pipeline_request_timeout_sec(),
            SUMMARIES_PIPELINE_JOB_TIMEOUT_SEC
        );
    }
}
