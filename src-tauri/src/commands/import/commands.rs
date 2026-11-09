use std::sync::Arc;

use tauri::{AppHandle, State};
use tracing::{info, warn};

use super::super::connection::ConnectionMode;
use super::super::staged_progress::ProgressKind;
use super::super::{AppState, StepStatus, cancel_dispatch_inner};
use super::aggregator::{
    AggregatedState, ImportAggregator, mark_downstream_skipped_by_cancel, step_label,
    validate_plain,
};
use super::events::ImportSessionCompletedEvent;
use super::forward::{
    ImportLogContext, emit_thread_import_terminal, import_state_allows_downstream, run_one_source,
};
use super::guards::{
    CANCELLED_ACTIVE_MESSAGE, CANCELLED_SKIP_MESSAGE, ImportExecutionGuard, ImportMaintenanceGuard,
    ImportSource, NativeImportLogGuard,
};
use super::pending::{
    PendingImportScope, find_import_window_candidates, generation_operation_key,
    import_activity_window, reconcile_import_targets, verify_import_threads,
};
use super::pending_store::{
    completed_pending_state, load_pending_imports, record_pending_operation, save_pending_imports,
    select_targeted_sessions, should_dispatch_import_targets,
};
use super::plan::ImportPlan;
use super::request::{
    DownstreamPlan, ImportNoticeCode, ImportStep, StartImportRequest, StartImportResponse,
    filter_manual_import_sources, filter_manual_import_sources_with_opencode_db,
    no_target_downstream_decisions, resolve_opencode_db_path, should_run_thread_group_summary,
};
use super::step_runner::{
    emit_step, emit_step_with_notice, run_cancellable_step, staged_step_job_timeout_sec,
};
use super::workflow_inputs::BatchDispatch;
use crate::error::{AppError, AppResult};
use crate::jobworkerp::JobworkerpHandle;

pub(super) async fn start_import_inner(
    app: AppHandle,
    state: State<'_, AppState>,
    req: StartImportRequest,
) -> AppResult<StartImportResponse> {
    let import_execution = ImportExecutionGuard::acquire()?;
    // Validate up front so a half-spawned set of children can't be left
    // behind when an invalid source slips into the middle of the list.
    validate_plain(&req)?;
    let job_id = req
        .dispatch_id
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("import-{}", chrono::Utc::now().timestamp_millis()));
    // Resolve the application cwd once; every importer child runs in it so
    // OpenCode's `.env` lookup and the CLI's own `~` expansion see the same
    // working directory the probe used.
    let import_cwd = std::env::current_dir()
        .map_err(|error| AppError::Config(format!("could not resolve import cwd: {error}")))?;
    let opencode_db_path = if req
        .sources
        .iter()
        .any(|source| matches!(source, ImportSource::OpenCode))
    {
        Some(resolve_opencode_db_path(&import_cwd))
    } else {
        None
    };
    let source_selection = match opencode_db_path.as_deref() {
        Some(path) => filter_manual_import_sources_with_opencode_db(
            &req.sources,
            dirs::home_dir().as_deref(),
            Some(path),
        ),
        None => filter_manual_import_sources(&req.sources, dirs::home_dir().as_deref()),
    };
    let sources = source_selection.sources;
    let downstream = DownstreamPlan::from_request(&req);
    // The import itself remains available when summaries are disabled. When
    // the user asked for the dependent summary stage, however, fail before
    // mutating threads if its durable recovery state cannot be trusted.
    if downstream.summary {
        super::super::summary_recovery::ensure_healthy(&state.data.summary_recovery_path())?;
    }
    if source_selection.no_importable_log_sources {
        state.import_progress.start(job_id.clone());
        emit_no_importable_log_sources(&app, &job_id, downstream);
        return Ok(StartImportResponse { job_id });
    }

    // Import targets the same memories / jobworkerp the browse clients use,
    // each the local sidecar or its configured remote server. The pending
    // scope and candidate window follow memories, which holds the imported
    // data.
    let resolved_connection = state.resolve_connection()?;
    let connection_mode = resolved_connection.memories_mode;
    let targets = resolved_connection.targets;
    if !req.dry_run {
        super::super::connection::require_thread_group_owner_capability_at_url(
            &targets.memories_url,
        )
        .await?;
    }

    // Import writes memory embeddings into the local LanceDB; refuse when it
    // is degraded (local memories only — a remote memories writes to its own
    // vector store, which is unaffected).
    state.ensure_local_embedding_available()?;

    state.import_progress.start(job_id.clone());

    let memories_url_for_validation = targets.memories_url.clone();
    let use_local_candidate_window = connection_mode == ConnectionMode::Local;
    let import_started_after = chrono::Utc::now().timestamp_millis().saturating_sub(1);
    let callback = targets.memories_callback()?;
    let user_id = req.user_id.unwrap_or(1);
    let pending_scope = PendingImportScope::new(connection_mode, &targets.memories_url, user_id)?;

    emit_step(
        &app,
        &job_id,
        ImportStep::ThreadImport,
        StepStatus::Active,
        None,
    );

    // dry-run skips dispatch entirely (workflows would fan out new jobs
    // against threads that were never imported). Failing eagerly when
    // the workflows bundle is missing avoids spawning the importer
    // pointlessly.
    let dispatch_admission = if req.dry_run {
        None
    } else {
        Some(state.lock_llm_settings_dispatch_gate().await)
    };
    let dispatch_ctx = if req.dry_run {
        None
    } else {
        let d = BatchDispatch::resolve(
            &callback,
            user_id,
            None,
            state.active_llm_worker_name().to_string(),
            state.active_output_language(),
        )?;
        // Downstream batches run for hours; the shared three-hour client
        // would close their result streams early.
        let h = JobworkerpHandle::connect_with_timeout(
            &targets.jobworkerp_url,
            crate::jobworkerp::job_timeouts::STAGED_GENERATION_CLIENT,
        )
        .await?;
        Some((d, h))
    };

    let selected_sources = sources.clone();
    let pending_path = state.data.import_pending_path();
    let pending_imports = Arc::new(parking_lot::Mutex::new(load_pending_imports(
        &pending_path,
    )?));
    let completed_sessions = Arc::new(parking_lot::Mutex::new(
        Vec::<ImportSessionCompletedEvent>::new(),
    ));
    let app_for_task = app.clone();
    let job_id_for_task = job_id.clone();
    let aggregator = Arc::new(ImportAggregator::new(sources.len()));
    let dry_run = req.dry_run;
    // Persist the conservative dirty generation before the import process is
    // dispatched. Dry-runs do not create a scope because they cannot write.
    let maintenance_registered = if dry_run {
        false
    } else {
        super::super::begin_search_index_write(
            &state,
            &[
                crate::search_index_maintenance::MaintenanceTable::Memory,
                crate::search_index_maintenance::MaintenanceTable::Thread,
            ],
        )
        .await?
    };
    let log_dir = state.data.log_dir();
    let native_log_dir = crate::log_rotation::native_log_dir(&log_dir, &format!("import-{job_id}"))
        .map_err(|error| {
            AppError::Config(format!(
                "could not create native import log directory: {error}"
            ))
        })?;
    let plan = match opencode_db_path {
        Some(path) => ImportPlan::new_with_opencode_db(
            req.memories_import_bin.clone(),
            import_cwd,
            user_id,
            &targets,
            req.since.clone(),
            req.dry_run,
            req.labels.clone(),
            native_log_dir,
            req.plain.clone(),
            Some(path),
        ),
        None => ImportPlan::new(
            req.memories_import_bin.clone(),
            import_cwd,
            user_id,
            &targets,
            req.since.clone(),
            req.dry_run,
            req.labels.clone(),
            native_log_dir,
            req.plain.clone(),
        ),
    };
    let entry = if dispatch_admission.is_some() {
        state.dispatch_register_admitted(&job_id).await
    } else {
        state.dispatch_register(&job_id).await
    };
    drop(dispatch_admission);
    let native_log_dir_for_task = plan.native_log_dir.clone();

    tokio::spawn(async move {
        let _import_execution = import_execution;
        let _native_import_log = NativeImportLogGuard(native_log_dir_for_task);
        let _maintenance = ImportMaintenanceGuard {
            app: app_for_task.clone(),
            registered: maintenance_registered,
        };
        let cancel = entry.token.clone();
        let current_job_id = entry.current_job_id.clone();
        let current_step = entry.current_step.clone();
        // RAII so a panic anywhere in the import driver still purges the
        // AppState entry — mirrors the chat path's `ChatInFlightGuard`.
        let _cleanup = ImportCleanupGuard::new(app_for_task.clone(), job_id_for_task.clone());

        // Step 1: thread-import (memories-import CLI, sequential per
        // source). The aggregator's `record` drains itself on the last
        // call; `take_final` is the fallback for the case where the
        // last `record` somehow didn't surface (shouldn't happen with
        // serial execution but failing-open beats freezing the toast).
        *current_step.lock().await = Some("thread-import".into());
        let mut import_state: Option<AggregatedState> = None;
        let mut cancelled_mid_import = false;
        for source in sources {
            if cancel.is_cancelled() {
                cancelled_mid_import = true;
                break;
            }
            let cmd = plan.build_command(source);
            let outcome = run_one_source(
                cmd,
                ImportLogContext {
                    aggregator: aggregator.clone(),
                    pending_path: pending_path.clone(),
                    pending_imports: pending_imports.clone(),
                    completed_sessions: completed_sessions.clone(),
                    pending_scope: pending_scope.clone(),
                    expected_source: source,
                    dry_run,
                    cancel: cancel.clone(),
                    native_log_dir: Some(plan.native_log_dir.clone()),
                },
            )
            .await;
            if cancel.is_cancelled() {
                cancelled_mid_import = true;
                break;
            }
            if let Some(state) = outcome {
                import_state = Some(state);
            }
        }
        if cancelled_mid_import {
            emit_step(
                &app_for_task,
                &job_id_for_task,
                ImportStep::ThreadImport,
                StepStatus::Cancelled,
                Some(CANCELLED_ACTIVE_MESSAGE.into()),
            );
            mark_downstream_skipped_by_cancel(&app_for_task, &job_id_for_task);
            return;
        }
        let import_state = import_state.unwrap_or_else(|| aggregator.take_final());
        let succeeded = import_state_allows_downstream(&import_state);
        emit_thread_import_terminal(&app_for_task, &job_id_for_task, import_state, dry_run);

        let Some((mut dispatch, handle)) = dispatch_ctx else {
            mark_downstream(
                &app_for_task,
                &job_id_for_task,
                StepStatus::Waiting,
                "dry-run",
            );
            return;
        };
        if !succeeded {
            mark_downstream(
                &app_for_task,
                &job_id_for_task,
                StepStatus::Waiting,
                "skipped: import failed",
            );
            return;
        }
        let current_completions = completed_sessions.lock().clone();
        let targeted_sessions = select_targeted_sessions(
            &pending_imports.lock(),
            &pending_scope,
            &current_completions,
            &selected_sources,
        );
        let mut target_thread_ids = targeted_sessions
            .iter()
            .map(|session| session.thread_id.clone())
            .collect::<Vec<_>>();
        target_thread_ids.sort();
        target_thread_ids.dedup();
        let candidate_ids = if use_local_candidate_window {
            match find_import_window_candidates(
                &memories_url_for_validation,
                user_id,
                import_started_after,
                chrono::Utc::now().timestamp_millis(),
            )
            .await
            {
                Ok(ids) => ids,
                Err(error) => {
                    mark_downstream(
                        &app_for_task,
                        &job_id_for_task,
                        StepStatus::Failed,
                        &error.to_string(),
                    );
                    return;
                }
            }
        } else {
            Vec::new()
        };
        let verified =
            match verify_import_threads(&memories_url_for_validation, &target_thread_ids).await {
                Ok(verified) => verified,
                Err(error) => {
                    mark_downstream(
                        &app_for_task,
                        &job_id_for_task,
                        StepStatus::Failed,
                        &error.to_string(),
                    );
                    return;
                }
            };
        let targets = match reconcile_import_targets(
            &target_thread_ids,
            &candidate_ids,
            &verified,
            user_id,
        ) {
            Ok(targets) => targets,
            Err(error) => {
                mark_downstream(
                    &app_for_task,
                    &job_id_for_task,
                    StepStatus::Failed,
                    &error.to_string(),
                );
                return;
            }
        };
        info!(
            target_count = targets.target_thread_ids.len(),
            candidate_intersection_count = targets.candidate_intersection.len(),
            supplemented_count = targets.supplemented_ids.len(),
            unrelated_candidate_count = candidate_ids
                .len()
                .saturating_sub(targets.candidate_intersection.len()),
            "resolved import generation target IDs"
        );
        if !should_dispatch_import_targets(&targets.target_thread_ids) {
            emit_no_target_downstream(&app_for_task, &job_id_for_task, downstream);
            return;
        }
        let group_activity_window = import_activity_window(&verified, &targets.target_thread_ids);
        dispatch.target_thread_ids = Some(targets.target_thread_ids);

        // Step 2-5 run sequentially; each step's stream
        // blocks until the worker's stream closes. They're independent
        // workflows so a failed summary or group summary does not skip
        // personality / reflection. Each step is individually gated by the dialog's
        // checkboxes; an unselected step is surfaced as `Waiting`
        // ("スキップ") so the toast distinguishes it from a stuck step.
        // A user-triggered cancel between/within steps converts the remaining
        // ones to `Cancelled` + "中断によりスキップ".
        let mut downstream_completed = true;
        for (step, run, worker, input) in [
            (
                ImportStep::ThreadSummary,
                downstream.summary,
                "memories-summarize-batch",
                dispatch.summarize_input(),
            ),
            (
                ImportStep::ThreadGroupSummary,
                should_run_thread_group_summary(downstream.summary, &target_thread_ids),
                "memories-thread-group-summary-batch",
                dispatch.thread_group_summary_input(group_activity_window),
            ),
            (
                ImportStep::ThreadPersonality,
                downstream.personality,
                "memories-personality-batch",
                dispatch.personality_input(),
            ),
            (
                ImportStep::Reflection,
                downstream.reflection,
                "memories-reflection-batch",
                dispatch.reflection_input(),
            ),
        ] {
            if cancel.is_cancelled() {
                downstream_completed = false;
                emit_step(
                    &app_for_task,
                    &job_id_for_task,
                    step,
                    StepStatus::Cancelled,
                    Some(CANCELLED_SKIP_MESSAGE.into()),
                );
                continue;
            }
            if !run {
                emit_skipped(&app_for_task, &job_id_for_task, step);
                continue;
            }
            *current_step.lock().await = Some(step_label(step).into());
            let operation_key = generation_operation_key(
                &pending_scope,
                &selected_sources,
                &target_thread_ids,
                step,
                &input,
            );
            if let Err(error) = record_pending_operation(
                &pending_path,
                &mut pending_imports.lock(),
                &targeted_sessions,
                step,
                &operation_key,
                &input,
                None,
                false,
            ) {
                emit_step(
                    &app_for_task,
                    &job_id_for_task,
                    step,
                    StepStatus::Failed,
                    Some(format!("pending state could not be saved: {error}")),
                );
                downstream_completed = false;
                continue;
            }
            let result = run_cancellable_step(
                &app_for_task,
                ProgressKind::Import,
                &job_id_for_task,
                step,
                &handle,
                worker,
                input.clone(),
                Some(&operation_key),
                staged_step_job_timeout_sec(step),
                cancel.clone(),
                current_job_id.clone(),
            )
            .await;
            let completed = result.outcome.status == StepStatus::Done;
            if let Err(error) = record_pending_operation(
                &pending_path,
                &mut pending_imports.lock(),
                &targeted_sessions,
                step,
                &operation_key,
                &input,
                result.dispatched_job_id.as_deref(),
                completed,
            ) {
                warn!(%error, ?step, "could not persist periodic operation outcome");
            }
            downstream_completed &= completed;
        }
        if downstream_completed {
            let mut guard = pending_imports.lock();
            let next = completed_pending_state(&guard, &targeted_sessions, true);
            if let Err(error) = save_pending_imports(&pending_path, &next) {
                warn!(error = %error, "could not clear completed import pending state");
            } else {
                *guard = next;
            }
        }
    });

    Ok(StartImportResponse { job_id })
}

/// Cancel an in-flight import pipeline. Idempotent: an unknown id is a
/// no-op so the toast can fire-and-forget on every Stop click. Mirrors
/// `chat_cancel` semantics — flips the token (so the next-step gate
/// short-circuits) and `JobService/Delete`s the live workflow job (so
/// the LLM slot is released immediately instead of running to
/// completion).
pub(super) async fn start_import_cancel_inner(
    state: State<'_, AppState>,
    dispatch_id: String,
) -> AppResult<()> {
    cancel_dispatch_inner(&state, &dispatch_id).await
}

/// RAII purger for `AppState::dispatch_in_flight`. Matches `chat.rs`'s
/// `ChatInFlightGuard` — release builds compile out via `panic = "abort"`
/// but a dev/test panic must still leave the map clean for the next run.
struct ImportCleanupGuard {
    app: AppHandle,
    job_id: String,
}

impl ImportCleanupGuard {
    fn new(app: AppHandle, job_id: String) -> Self {
        Self { app, job_id }
    }
}

impl Drop for ImportCleanupGuard {
    fn drop(&mut self) {
        // Bare `tokio::spawn` would panic if the runtime is already shut
        // down (e.g. RunEvent::ExitRequested); the map vanishes with the
        // process so a missed cleanup is harmless.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        use tauri::Manager;
        let app = self.app.clone();
        let job_id = std::mem::take(&mut self.job_id);
        handle.spawn(async move {
            if let Some(state) = app.try_state::<AppState>() {
                let _ = state.dispatch_take(&job_id).await;
            }
        });
    }
}

/// Emit a single downstream step as intentionally skipped. Used when the
/// import succeeded but the user unchecked this generation step in the
/// dialog — distinct from the dry-run / import-failure cases that
/// `mark_downstream` covers for *all three* steps at once.
fn emit_skipped(app: &AppHandle, job_id: &str, step: ImportStep) {
    emit_step(
        app,
        job_id,
        step,
        StepStatus::Waiting,
        Some("スキップ".into()),
    );
}

fn mark_downstream(app: &AppHandle, job_id: &str, status: StepStatus, msg: &str) {
    for step in [
        ImportStep::ThreadSummary,
        ImportStep::ThreadGroupSummary,
        ImportStep::ThreadPersonality,
        ImportStep::Reflection,
    ] {
        emit_step(app, job_id, step, status, Some(msg.into()));
    }
}

/// Finish a manual import without touching sidecars when every selected
/// auto-discovered log root is absent. This is a successful no-op, while the
/// notice code lets the toast explain why no child process was started.
fn emit_no_importable_log_sources(app: &AppHandle, job_id: &str, downstream: DownstreamPlan) {
    emit_step_with_notice(
        app,
        job_id,
        ImportStep::ThreadImport,
        StepStatus::Done,
        None,
        Some(ImportNoticeCode::NoImportableLogSources),
    );
    emit_no_target_downstream(app, job_id, downstream);
}

/// Complete selected downstream steps when the import produced no target
/// threads; unselected steps retain the normal explicit skip state.
fn emit_no_target_downstream(app: &AppHandle, job_id: &str, downstream: DownstreamPlan) {
    for (step, status, message) in no_target_downstream_decisions(downstream) {
        emit_step(app, job_id, step, status, Some(message.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::StepStatus;
    use super::super::guards::{CANCELLED_ACTIVE_MESSAGE, CANCELLED_SKIP_MESSAGE};
    use super::super::pending_store::should_dispatch_import_targets;
    use super::super::request::{
        DownstreamPlan, ImportStep, downstream_steps, no_target_downstream_decisions,
        should_run_thread_group_summary,
    };
    use super::super::tests_common::make_request;

    #[test]
    fn downstream_plan_from_request_copies_flags() {
        let all = DownstreamPlan::from_request(&make_request(true, true, true));
        assert_eq!(
            all,
            DownstreamPlan {
                summary: true,
                personality: true,
                reflection: true
            }
        );

        let none = DownstreamPlan::from_request(&make_request(false, false, false));
        assert_eq!(
            none,
            DownstreamPlan {
                summary: false,
                personality: false,
                reflection: false
            }
        );

        // Mixed: only personality off, proving each flag maps independently
        // (not a single all-or-nothing toggle).
        let mixed = DownstreamPlan::from_request(&make_request(true, false, true));
        assert_eq!(
            mixed,
            DownstreamPlan {
                summary: true,
                personality: false,
                reflection: true
            }
        );
    }

    #[test]
    fn downstream_plan_all_true_equals_legacy() {
        // Invariant: all three flags on reproduces the pre-selection
        // behaviour where every downstream step always ran.
        let plan = DownstreamPlan::from_request(&make_request(true, true, true));
        assert!(plan.summary && plan.personality && plan.reflection);
    }

    #[test]
    fn downstream_steps_preserve_pipeline_order_and_selection() {
        let plan = DownstreamPlan::from_request(&make_request(true, false, true));
        assert_eq!(
            downstream_steps(plan),
            [
                (ImportStep::ThreadSummary, true),
                (ImportStep::ThreadGroupSummary, true),
                (ImportStep::ThreadPersonality, false),
                (ImportStep::Reflection, true),
            ]
        );
    }

    #[test]
    fn no_target_downstream_decisions_cover_all_selection_combinations() {
        let cases = [
            (false, false, false),
            (false, false, true),
            (false, true, false),
            (false, true, true),
            (true, false, false),
            (true, false, true),
            (true, true, false),
            (true, true, true),
        ];

        for (summary, personality, reflection) in cases {
            let plan = DownstreamPlan {
                summary,
                personality,
                reflection,
            };
            let expected_state = |step, selected| {
                if selected {
                    (step, StepStatus::Done, "対象スレッドなし")
                } else {
                    (step, StepStatus::Waiting, "スキップ")
                }
            };
            let expected = [
                expected_state(ImportStep::ThreadSummary, summary),
                expected_state(ImportStep::ThreadGroupSummary, summary),
                expected_state(ImportStep::ThreadPersonality, personality),
                expected_state(ImportStep::Reflection, reflection),
            ];
            assert_eq!(no_target_downstream_decisions(plan), expected);
        }
    }

    #[test]
    fn empty_authoritative_target_set_dispatches_no_generation_jobs() {
        assert!(!should_dispatch_import_targets(&[]));
        assert!(should_dispatch_import_targets(&["42".into()]));
    }

    #[test]
    fn thread_group_summary_requires_selected_summary_and_nonempty_targets() {
        assert!(should_run_thread_group_summary(true, &["42".into()]));
        assert!(!should_run_thread_group_summary(false, &["42".into()]));
        assert!(!should_run_thread_group_summary(true, &[]));
    }

    #[test]
    fn cancelled_messages_distinguish_active_step_from_skipped_ones() {
        // The toast surfaces both — the active step (the one that was
        // actually torn down by JobService/Delete) shows "中断", the
        // remaining ones show "中断によりスキップ". Pin the strings
        // because the UI looks them up to colour the row.
        assert_eq!(CANCELLED_ACTIVE_MESSAGE, "中断");
        assert_eq!(CANCELLED_SKIP_MESSAGE, "中断によりスキップ");
    }
}
