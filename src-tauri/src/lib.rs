//! Lookback Tauri core: sidecar lifecycle, gRPC clients to memories,
//! and the Tauri commands the React frontend invokes.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

pub mod coding_agent;
pub mod commands;
pub mod crashtrace;
pub mod data;
pub mod error;
pub(crate) mod error_messages;
pub mod experimental_features;
pub mod grpc;
pub mod issue_extract;
pub mod issue_review_policy;
pub mod issue_work_policy;
pub mod jobworkerp;
pub mod lindera;
pub mod log_rotation;
pub mod maintenance;
pub mod native_terminal;
pub mod plugins;
pub mod search_index_maintenance;
pub mod serde_id;
pub mod sidecar;
pub mod window_lifecycle;
#[cfg(test)]
mod workflow_expression_tests;

pub use window_lifecycle::{
    TrayAvailability, platform_tray_host_availability, restore_window, tray_availability_for_icon,
};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tauri::{
    AppHandle, Manager, RunEvent, WebviewWindow,
    menu::{Menu, MenuItem},
    tray::{TrayIcon, TrayIconBuilder},
};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

use crate::commands::AppState;
use crate::data::DataPaths;
use crate::error::{AppError, AppResult};
use crate::sidecar::{SidecarConfig, SidecarWarning, SidecarWarningKind, Sidecars};

pub fn run() {
    init_tracing();
    apply_linux_webkit_workarounds();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            restore_main_window(app);
        }))
        .invoke_handler(tauri::generate_handler![
            commands::threads::list_threads,
            commands::threads::find_thread,
            commands::threads::find_distinct_labels,
            commands::threads::find_co_occurring_labels,
            commands::threads::find_memories_by_thread_id,
            commands::threads::find_memory_position,
            commands::threads::find_memory_thread_position,
            commands::threads::resolve_memory_reference,
            commands::thread_groups::list_thread_groups,
            commands::thread_groups::search_thread_groups,
            commands::thread_groups::find_thread_group,
            commands::thread_groups::find_thread_group_summary,
            commands::thread_groups::find_thread_group_summaries,
            commands::thread_groups::count_thread_group_summaries,
            commands::thread_groups::count_active_thread_groups,
            commands::thread_group_recovery::get_thread_group_recovery_status,
            commands::threads::delete_thread,
            commands::summaries::list_summaries,
            commands::summaries::list_summaries_for_selection,
            commands::summaries::list_summary_path_candidates,
            commands::summaries::list_summary_path_children,
            commands::summaries::get_summary_content,
            commands::summaries::find_summary_distinct_labels,
            commands::summaries::find_summary_co_occurring_labels,
            commands::summaries::count_summaries,
            commands::summaries::list_summary_period_keys,
            commands::summaries::delete_summary,
            commands::summaries::resolve_summary_memory_ref,
            commands::summary_recovery::get_summary_recovery_status,
            commands::summary_recovery::reset_summary_recovery_state,
            commands::import::start_import,
            commands::import::start_import_cancel,
            commands::import::get_active_staged_progress,
            commands::settings::get_settings,
            commands::settings::get_app_capabilities,
            commands::settings::get_sidecar_status,
            commands::search_index_maintenance::get_search_index_maintenance_status,
            commands::search_index_maintenance::start_search_index_maintenance,
            commands::search_index_maintenance::set_search_index_maintenance_schedule,
            commands::settings::purge_all_data,
            commands::coding_agent::coding_agent_get_settings,
            commands::coding_agent::coding_agent_get_native_setup,
            commands::coding_agent::coding_agent_approve_native_backend,
            commands::coding_agent::coding_agent_check_repository_host,
            commands::native_terminal::native_terminal_status,
            commands::native_terminal::native_terminal_prepare,
            commands::native_terminal::native_terminal_start,
            commands::native_terminal::native_terminal_list,
            commands::native_terminal::native_terminal_attach,
            commands::native_terminal::native_terminal_output,
            commands::native_terminal::native_terminal_send_input,
            commands::native_terminal::native_terminal_cancel,
            commands::native_terminal::native_terminal_detach,
            commands::coding_agent::coding_agent_update_settings,
            commands::coding_agent::coding_agent_prepare_profile_home,
            commands::profile_terminal::coding_agent_profile_terminal_start,
            commands::profile_terminal::coding_agent_profile_terminal_list,
            commands::profile_terminal::coding_agent_profile_terminal_session,
            commands::profile_terminal::coding_agent_profile_terminal_output,
            commands::profile_terminal::coding_agent_profile_terminal_send_input,
            commands::profile_terminal::coding_agent_profile_terminal_finish_input,
            commands::profile_terminal::coding_agent_profile_terminal_cancel,
            commands::coding_agent::coding_agent_get_backend_status,
            commands::coding_agent::coding_agent_preview_run,
            commands::coding_agent::coding_agent_approve_and_start,
            commands::coding_agent::coding_agent_list_runs,
            commands::coding_agent::coding_agent_list_delegation_plans,
            commands::coding_agent::coding_agent_get_run,
            commands::coding_agent::coding_agent_attach_run,
            commands::coding_agent::coding_agent_cancel_run,
            commands::coding_agent::coding_agent_list_evidence,
            commands::coding_agent::coding_agent_read_evidence,
            commands::coding_agent::coding_agent_get_review,
            commands::coding_agent::coding_agent_reassess,
            commands::coding_agent::coding_agent_apply_review_action,
            commands::issue_auto_policy::get_auto_issue_policy,
            commands::issue_auto_policy::set_auto_issue_policy,
            commands::issue_review_policy::get_issue_review_policy,
            commands::issue_review_policy::set_issue_review_policy,
            commands::issue_work_policy::get_work_eligibility,
            commands::issue_work_policy::set_work_eligibility,
            commands::issues::list_issue_review_items,
            commands::issues::list_issues,
            commands::issues::get_issue_detail,
            commands::issues::list_issue_ownership_diagnostics,
            commands::issues::get_issue_operation_status,
            commands::issues::apply_issue_admin_action,
            commands::issues::resolve_issue_operation,
            commands::issues::list_pending_issue_operations,
            commands::issues::retry_pending_issue_operation,
            commands::issues::list_issue_direct_history,
            commands::issues::create_issue_from_canceled,
            commands::issues::get_issue_evidence,
            commands::issues::get_issue_duplicates,
            commands::issues::list_issue_history,
            commands::issues::resolve_issue_review,
            commands::issues::update_issue_candidate,
            commands::issues::create_explicit_issue_from_candidate,
            commands::issues::reextract_rejected_candidate,
            commands::issues::transition_issue_state,
            commands::connection::get_connection_config,
            commands::connection::set_connection_config,
            commands::connection::test_connection_config,
            commands::connection::inspect_remote_jobworkerp,
            commands::connection::register_remote_jobworkerp_workers,
            commands::logs::read_sidecar_log,
            commands::reflections::list_reflections_by_thread,
            commands::reflections::list_reflections_for_selection,
            commands::reflections::get_reflection_selection_content,
            commands::reflections::search_reflections,
            commands::reflections::search_reflections_hybrid,
            commands::reflections::search_reflections_by_intent,
            commands::reflections::get_reflection_intent_index_stats,
            commands::reflections::redispatch_reflection_embeddings,
            commands::reflections::delete_reflection,
            commands::personality::get_personality,
            commands::personality::list_personality_signals,
            commands::personality::delete_personality_signal,
            commands::personality::delete_personality_profile,
            commands::personality::debug_personality_inventory,
            commands::periodic_tasks::list_periodic_tasks,
            commands::periodic_tasks::create_periodic_task,
            commands::periodic_tasks::update_periodic_task,
            commands::periodic_tasks::delete_periodic_task,
            commands::periodic_tasks::set_enabled_periodic_task,
            commands::periodic_execution::list_periodic_task_statuses,
            commands::periodic_execution::list_periodic_execution_history,
            commands::periodic_execution::cancel_periodic_execution,
            commands::search::search_memories_keyword,
            commands::search::search_memories_semantic,
            commands::search::search_memories_hybrid,
            commands::model::get_model_status,
            commands::model::retry_model_setup,
            commands::reflection_dispatch::enqueue_reflection_job,
            commands::reflection_dispatch::reflection_cancel,
            commands::analysis_dispatch::enqueue_summary_job,
            commands::analysis_dispatch::enqueue_thread_group_summary_job,
            commands::analysis_dispatch::enqueue_thread_group_issue_resolution_job,
            commands::analysis_dispatch::enqueue_thread_issue_resolution_job,
            commands::analysis_dispatch::get_thread_issue_resolution_status,
            commands::analysis_dispatch::enqueue_personality_job,
            commands::analysis_dispatch::enqueue_personality_merge_job,
            commands::analysis_dispatch::enqueue_period_summary_job,
            commands::analysis_dispatch::generate_summaries,
            commands::analysis_dispatch::analysis_cancel,
            commands::chat::chat_ask,
            commands::chat::chat_cancel,
            commands::chat::chat_respond_tool_approval,
            commands::chat::get_chat_tool_settings,
            commands::chat::set_chat_tool_settings,
            commands::issue_chat_recovery::list_pending_chat_issue_operations,
            commands::issue_chat_recovery::retry_pending_chat_issue_operation,
            commands::chat::get_selected_memory_limits,
            commands::chat::save_chat_markdown,
            commands::llm_settings::get_llm_settings,
            commands::llm_settings::set_llm_settings,
            commands::app_settings::get_app_settings,
            commands::app_settings::set_data_root,
            commands::app_settings::set_hf_home,
            commands::app_settings::set_output_language,
            commands::app_settings::set_timezone,
            commands::app_settings::list_timezones,
            commands::app_settings::validate_data_root,
            commands::app_settings::create_data_root,
            commands::llm_presets::list_llm_presets,
            commands::embedding_presets::list_embedding_presets,
            commands::embedding_settings::get_embedding_settings,
            commands::embedding_settings::set_embedding_settings,
            commands::mcp_settings::get_mcp_settings,
            commands::mcp_settings::set_mcp_settings,
            commands::issue_tracker_settings::get_issue_tracker_settings,
            commands::mcp_settings::generate_mcp_auth_token,
            commands::mcp_settings::rotate_mcp_auth_token,
            commands::apply_settings::apply_settings,
            commands::setup::get_setup_status,
            commands::setup::apply_setup,
            commands::setup::resume_setup,
            commands::setup::restart_for_setup,
            commands::setup::start_fresh_setup,
            commands::ui_diagnostics::report_ui_diagnostic,
            commands::ui_diagnostics::report_ui_activity_sample,
            commands::embeddings::get_memory_embedding_stats,
            commands::embeddings::redispatch_memory_embeddings,
            commands::embedding_space::get_embedding_space,
            commands::embedding_space::start_embedding_rebuild,
            commands::embedding_space::get_embedding_rebuild_progress,
            commands::embedding_space::start_embedding_reconcile,
            commands::embedding_space::get_embedding_reconcile_progress,
            commands::embedding_management::inspect_embedding_migration,
            commands::embedding_management::plan_embedding_change,
            commands::embedding_management::switch_embedding_space,
            commands::embedding_management::recover_embedding_migration,
            commands::background_jobs::get_background_job_queue_status,
            commands::recovery::recover_reset_embedding_settings,
            commands::recovery::retry_database_migration,
            commands::recovery::restore_database_migration_backup,
            commands::recovery::open_log_dir,
            commands::recovery::quit_app,
            commands::window_lifecycle::register_close_choice_ui,
            commands::window_lifecycle::sync_close_choice_locale,
            commands::window_lifecycle::acknowledge_close_choice_request,
            commands::window_lifecycle::choose_close_action,
            resolve_memories_import_bin,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            // Point LOOKBACK_WORKERS_DIR at the bundled resource (prod) so
            // build_sidecar_config's llm_workers_yaml() resolves; no-op in
            // dev where the CARGO_MANIFEST_DIR fallback applies.
            crate::data::paths::stage_workers_env(&handle);
            let config = build_sidecar_config(&handle)?;
            let data = config.data.clone();
            let sidecars = Arc::new(Sidecars::new(config));

            let state = AppState::try_new_with_resources(
                sidecars.clone(),
                data.clone(),
                app.path().resource_dir().ok(),
            )?;
            let tray_icon = install_tray_icon(&handle).ok();
            state.set_coding_agent_tray_icon(tray_icon);
            match state.coding_agent_tray_availability() {
                TrayAvailability::VerifiedRestorable => {}
                TrayAvailability::Unverified => tracing::warn!(
                    "system tray host visibility is unverified; closing the window will leave it visible"
                ),
                TrayAvailability::Unavailable => tracing::warn!(
                    "system tray is unavailable; closing the window will leave it visible"
                ),
            }
            let executor_path = handle
                .path()
                .resource_dir()
                .ok()
                .map(|path| path.join("coding-agent/bin/coding-agent-executor"));
            state
                .coding_agent_backend
                .set_bundled_executor_path(executor_path);
            app.manage(state);

            #[cfg(target_os = "linux")]
            {
                install_linux_web_process_diagnostics(&handle);
                commands::ui_diagnostics::start_memory_sampler();
            }

            spawn_log_maintenance(data.clone());
            spawn_search_index_maintenance_scheduler(handle.clone());
            spawn_coding_agent_auto_completion(handle.clone());

            // Spawn sidecars on a tokio task — keep the Tauri setup path
            // non-blocking so the UI shell renders immediately.
            let handle_for_task = handle.clone();
            let sidecars_for_task = sidecars.clone();
            let data_for_task = data.clone();
            tauri::async_runtime::spawn(async move {
                stage_and_start_sidecars(&handle_for_task, &sidecars_for_task, &data_for_task)
                    .await;
            });
            crate::maintenance::spawn_jobworkerp_maintenance_loop(sidecars.clone(), data.clone());

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            match event {
                RunEvent::WindowEvent {
                    label,
                    event: tauri::WindowEvent::CloseRequested { api, .. },
                    ..
                } => {
                    api.prevent_close();
                    if app_handle.get_webview_window(&label).is_some()
                        && let Some(state) = app_handle.try_state::<AppState>()
                    {
                        let tray_availability = state.coding_agent_tray_availability();
                        if let Some(request) = state
                            .window_lifecycle
                            .begin_close_request(&label, tray_availability)
                        {
                            commands::window_lifecycle::notify_close_request(
                                app_handle,
                                request.clone(),
                            );
                            commands::window_lifecycle::schedule_native_fallback(
                                app_handle.clone(),
                                request.request_id,
                            );
                        }
                    }
                }
                RunEvent::ExitRequested { api, .. } => {
                    if let Some(state) = app_handle.try_state::<AppState>() {
                        if !state.window_lifecycle.begin_exit_requested() {
                            api.prevent_exit();
                            return;
                        }
                        tracing::info!("exit requested; reconciling coding-agent runs before sidecars");
                        let result = tauri::async_runtime::block_on(async {
                            let labels = app_handle.webview_windows().into_keys().collect::<Vec<_>>();
                            state.shutdown_for_quit(&labels).await
                        });
                        match result {
                            Ok(outcome)
                                if !outcome.unconfirmed_run_ids.is_empty()
                                    || !outcome.unconfirmed_native_session_ids.is_empty() =>
                            {
                                tracing::warn!(
                                    cancellation_attempts = outcome.cancellation_attempts,
                                    unconfirmed_run_ids = ?outcome.unconfirmed_run_ids,
                                    unconfirmed_native_session_ids = ?outcome.unconfirmed_native_session_ids,
                                    "exiting with durable Unknown runs; remote execution cleanup remains unconfirmed; guest logs were not collected"
                                );
                            }
                            Ok(_) => {
                                state.window_lifecycle.finish_exit(true);
                            }
                            Err(error) => {
                                tracing::warn!(%error, "application exit held because shutdown state could not be persisted or sidecars could not stop");
                                state.window_lifecycle.finish_exit(false);
                                let native_fallback_locale = state
                                    .window_lifecycle
                                    .take_native_fallback_retry_locale();
                                commands::window_lifecycle::exit_request_failed(
                                    app_handle,
                                    native_fallback_locale,
                                );
                                api.prevent_exit();
                            }
                        }
                    }
                }
                RunEvent::Exit => commands::ui_diagnostics::stop_memory_sampler(),
                #[cfg(target_os = "macos")]
                RunEvent::Reopen { .. } => restore_main_window(app_handle),
                _ => {}
            }
        });
}

struct TauriWindowRestoreAdapter {
    window: WebviewWindow,
}

impl crate::window_lifecycle::WindowRestoreAdapter for TauriWindowRestoreAdapter {
    fn show(&self) -> bool {
        self.window.show().is_ok()
    }

    fn unminimize(&self) -> bool {
        self.window.unminimize().is_ok()
    }

    fn focus(&self) -> bool {
        self.window.set_focus().is_ok()
    }
}

fn restore_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let adapter = TauriWindowRestoreAdapter { window };
        if !restore_window(&adapter) {
            tracing::warn!("the existing main window could not be restored");
        }
    }
}

#[derive(Debug)]
enum TrayIconInstallError {
    IconMissing,
    MenuSetupFailed,
    NativeBuildFailed,
}

fn install_tray_icon(app: &AppHandle) -> Result<TrayIcon, TrayIconInstallError> {
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or(TrayIconInstallError::IconMissing)?;
    let open = MenuItem::with_id(app, "open", "Open", true, None::<&str>)
        .map_err(|_| TrayIconInstallError::MenuSetupFailed)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)
        .map_err(|_| TrayIconInstallError::MenuSetupFailed)?;
    let menu = Menu::with_items(app, &[&open, &quit])
        .map_err(|_| TrayIconInstallError::MenuSetupFailed)?;
    TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .tooltip("Lookback")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => restore_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)
        .map_err(|_| TrayIconInstallError::NativeBuildFailed)
}

fn spawn_coding_agent_auto_completion(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tick.tick().await;
            let Some(state) = app.try_state::<AppState>() else {
                return;
            };
            match commands::coding_agent::reconcile_approved_auto_completion(&state).await {
                Ok(0) => {}
                Ok(completed) => tracing::info!(
                    completed,
                    "approved coding-agent auto-DONE actions reconciled"
                ),
                Err(error) => {
                    tracing::debug!(%error, "approved coding-agent auto-DONE reconciliation is not currently available")
                }
            }
        }
    });
}

/// Run best-effort log retention at startup and every six hours. The cleanup
/// never blocks sidecar startup and failures are diagnostic only: losing a
/// maintenance pass must not make the application unavailable.
fn spawn_log_maintenance(data: DataPaths) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 60 * 60));
        loop {
            tick.tick().await;
            let log_dir = data.log_dir();
            match tokio::task::spawn_blocking(move || {
                crate::log_rotation::cleanup_log_dir(&log_dir)
            })
            .await
            {
                Ok(Ok(report)) if report.expired > 0 || report.capacity > 0 => {
                    tracing::debug!(
                        expired = report.expired,
                        capacity = report.capacity,
                        "log retention maintenance removed archived logs"
                    );
                }
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::debug!(%error, "log retention maintenance failed"),
                Err(error) => tracing::debug!(%error, "log retention task failed"),
            }
        }
    });
}

/// Local, sidecar-ready time is checkpointed once a minute.  The scheduler is
/// deliberately owned by the app, not by `front`, so desktop downtime never
/// counts towards the 24-hour compaction qualification.
fn spawn_search_index_maintenance_scheduler(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        let mut ready_minutes: u64 = 0;
        let mut retry_warnings = RetryWarnings::default();
        loop {
            tick.tick().await;
            let Some(state) = app.try_state::<AppState>() else {
                return;
            };
            if !state.is_service_local(commands::connection::Service::Memories)
                || state.sidecars.current_endpoints().is_none()
            {
                continue;
            }
            if let Err(error) = state.search_index_maintenance.add_ready_runtime(60).await {
                tracing::warn!(%error, "search-index maintenance runtime checkpoint failed");
                continue;
            }
            let app_settings =
                crate::data::paths::load_app_settings(&state.data.app_settings_path());
            let timezone = crate::sidecar::lifecycle::resolve_timezone(Some(&app_settings));
            if let Ok(channel) = state.memories_channel().await {
                let coordinator = state.search_index_maintenance.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = coordinator
                        .start_scheduled_optimize(channel, chrono::Utc::now(), &timezone)
                        .await
                    {
                        tracing::warn!(%error, "scheduled search-index optimization failed");
                    }
                });
            }
            monitor_periodic_maintenance(&app, &mut retry_warnings).await;
            ready_minutes = ready_minutes.saturating_add(1);
            if ready_minutes.is_multiple_of(60) {
                match state.memories_channel().await {
                    Ok(channel) => {
                        if let Err(error) = state.search_index_maintenance.reconcile(channel).await
                        {
                            tracing::warn!(%error, "hourly search-index reconcile failed");
                        }
                    }
                    Err(error) => tracing::warn!(%error, "hourly maintenance channel unavailable"),
                }
            }
        }
    });
}

/// Conductor owns periodic workflow dispatch, so terminal execution records
/// are observed in the backend rather than relying on a mounted UI view.
async fn monitor_periodic_maintenance(app: &AppHandle, retry_warnings: &mut RetryWarnings) {
    let tasks = match commands::periodic_tasks::list_periodic_tasks(
        app.state::<AppState>(),
        commands::periodic_tasks::ListPeriodicTasksRequest {
            limit: Some(100),
            offset: Some(0),
        },
    )
    .await
    {
        Ok(tasks) => tasks,
        Err(error) => {
            tracing::debug!(%error, "periodic maintenance monitor could not list tasks");
            return;
        }
    };
    let task_by_id: HashMap<_, _> = tasks
        .iter()
        .filter(|task| task.enabled)
        .filter_map(|task| task.task.clone().map(|args| (task.id.clone(), args)))
        .collect();
    let ids = tasks
        .iter()
        .filter(|task| task.enabled)
        .map(|task| task.id.clone())
        .collect();
    let statuses = match commands::periodic_execution::list_periodic_task_statuses(
        app.state::<AppState>(),
        commands::periodic_execution::ListPeriodicTaskStatusesRequest { scheduler_ids: ids },
    )
    .await
    {
        Ok(statuses) => statuses,
        Err(error) => {
            tracing::debug!(%error, "periodic maintenance monitor could not resolve status");
            return;
        }
    };
    for summary in statuses {
        let terminal = matches!(
            summary.status,
            commands::periodic_execution::PeriodicExecutionStatus::Succeeded
                | commands::periodic_execution::PeriodicExecutionStatus::Failed
                | commands::periodic_execution::PeriodicExecutionStatus::Cancelled
                | commands::periodic_execution::PeriodicExecutionStatus::EnqueueFailed
        );
        let Some(execution_id) = summary
            .runtime
            .as_ref()
            .map(|runtime| runtime.execution_ref_id.as_str())
        else {
            continue;
        };
        if !terminal {
            if summary.active {
                let state = app.state::<AppState>();
                let app_settings =
                    crate::data::paths::load_app_settings(&state.data.app_settings_path());
                let timezone_name =
                    crate::sidecar::lifecycle::resolve_timezone(Some(&app_settings));
                if let Err(error) = state
                    .search_index_maintenance
                    .begin_periodic_execution(execution_id, &timezone_name)
                    .await
                {
                    tracing::warn!(%error, "periodic execution maintenance admission failed");
                }
            }
            continue;
        }
        let state = app.state::<AppState>();
        if !state
            .search_index_maintenance
            .needs_periodic_terminal_processing(execution_id)
            .await
        {
            continue;
        }
        // A successful run needs no follow-up: its summaries are visible in
        // the Summaries view. Only failures are recorded so the view can
        // show which periods are missing.
        let recovery_result = match periodic_terminal_failure(summary.status) {
            None => Ok(()),
            Some(failure) => {
                record_failed_periodic_execution(
                    &state,
                    task_by_id.get(&summary.scheduler_id),
                    &summary,
                    execution_id,
                    failure,
                )
                .await
            }
        };
        if let Err(error) = recovery_result {
            // Do not record the execution as terminal yet so the next
            // maintenance poll retries it. A persistent cause would repeat
            // every minute, so only its first occurrence is a warning.
            report_periodic_retry(retry_warnings, execution_id, &summary.scheduler_id, &error);
            continue;
        }
        retry_warnings.finished(execution_id);

        match state
            .search_index_maintenance
            .finish_periodic_execution(execution_id)
            .await
        {
            Ok(true) => match state.memories_channel().await {
                Ok(channel) => {
                    if let Err(error) = state.search_index_maintenance.reconcile(channel).await {
                        tracing::warn!(%error, "periodic terminal maintenance reconcile failed");
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "periodic terminal maintenance channel unavailable")
                }
            },
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "periodic terminal maintenance state failed"),
        }
    }
}

/// Number of error characters kept in a retry warning. jobworkerp errors can
/// embed whole worker definitions, which made each warning hundreds of KB.
const RETRY_WARNING_ERROR_CHARS: usize = 300;

/// Failure kinds recorded for a terminal periodic execution, or `None` for a
/// success, which needs no record.
fn periodic_terminal_failure(
    status: commands::periodic_execution::PeriodicExecutionStatus,
) -> Option<(&'static str, &'static str)> {
    use commands::periodic_execution::PeriodicExecutionStatus;
    match status {
        PeriodicExecutionStatus::Failed => Some(("workflow_execution_failed", "failed")),
        PeriodicExecutionStatus::Cancelled => Some(("workflow_execution_cancelled", "cancelled")),
        PeriodicExecutionStatus::EnqueueFailed => {
            Some(("workflow_enqueue_failed", "enqueue_failed"))
        }
        _ => None,
    }
}

/// Executions whose retry has already been reported at warning level.
#[derive(Debug, Default)]
struct RetryWarnings {
    warned: std::collections::HashSet<String>,
}

impl RetryWarnings {
    /// Whether this is the first failure seen for the execution.
    fn first_failure(&mut self, execution_id: &str) -> bool {
        self.warned.insert(execution_id.to_string())
    }

    fn finished(&mut self, execution_id: &str) {
        self.warned.remove(execution_id);
    }
}

/// Keeps at most `max_chars` characters, marking a cut with an ellipsis.
fn concise_error(error: &str, max_chars: usize) -> String {
    match error.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &error[..cut]),
        None => error.to_string(),
    }
}

fn report_periodic_retry(
    retry_warnings: &mut RetryWarnings,
    execution_id: &str,
    scheduler_id: &str,
    error: &AppError,
) {
    let error = error.to_string();
    if retry_warnings.first_failure(execution_id) {
        tracing::warn!(
            error = %concise_error(&error, RETRY_WARNING_ERROR_CHARS),
            execution_id,
            scheduler_id,
            "periodic execution processing failed; will retry (repeats and full error at debug level)"
        );
    }
    tracing::debug!(
        %error,
        execution_id,
        scheduler_id,
        "periodic execution processing failed; will retry"
    );
}

/// Records the periods a failed periodic execution was meant to summarize.
/// The period plan is rebuilt from the persisted scheduler task and trigger
/// time under the timezone the worker ran with, never a fallback zone.
async fn record_failed_periodic_execution(
    state: &AppState,
    task: Option<&commands::periodic_tasks::PeriodicTaskArgs>,
    summary: &commands::periodic_execution::PeriodicExecutionSummary,
    execution_id: &str,
    failure: (&'static str, &'static str),
) -> Result<(), AppError> {
    // These messages are log-only, so they stay plain English.
    let (Some(task), Some(runtime)) = (task, summary.runtime.as_ref()) else {
        return Err(AppError::Config(
            "failed periodic execution is missing task settings or a triggered time".into(),
        ));
    };
    let timezone_name = match state
        .search_index_maintenance
        .periodic_execution_timezone(execution_id)
        .await
    {
        Some(snapshot) => snapshot,
        None => {
            let app_settings =
                crate::data::paths::load_app_settings(&state.data.app_settings_path());
            crate::sidecar::lifecycle::resolve_timezone(Some(&app_settings))
        }
    };
    let timezone = timezone_name.parse::<chrono_tz::Tz>().map_err(|error| {
        AppError::Config(format!(
            "periodic execution timezone {timezone_name} is invalid: {error}"
        ))
    })?;
    record_terminal_periodic_recovery(
        state,
        task,
        runtime.triggered_at_ms,
        execution_id,
        failure,
        timezone,
    )
    .await
}

/// A failed periodic workflow cannot identify the individual path buckets it
/// completed, so every unit of its deterministic period plan is recorded.
/// This applies to cancellation and enqueue failures too: the stop-on-error
/// option changes workflow control flow, not what is missing.
async fn record_terminal_periodic_recovery(
    state: &AppState,
    task: &commands::periodic_tasks::PeriodicTaskArgs,
    triggered_at_ms: i64,
    execution_id: &str,
    (reason_code, terminal_status): (&'static str, &'static str),
    timezone: chrono_tz::Tz,
) -> Result<(), AppError> {
    let callback = state.resolve_targets()?.memories_callback()?;
    let source_scope_digest = periodic_source_scope_digest(task);
    let started_at = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(triggered_at_ms)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let generation_id = format!("periodic-{execution_id}");
    let memories_endpoint_id = format!("{}:{}", callback.host, callback.port);
    let _recovery_guard = state.summary_recovery.lock().await;
    for (period_kind, period_key) in periodic_failure_periods(task, triggered_at_ms, timezone) {
        commands::summary_recovery::record(
            &state.data.summary_recovery_path(),
            commands::summary_recovery::build_record(
                commands::summary_recovery::SummaryRecoveryIdentity {
                    memories_endpoint_id: &memories_endpoint_id,
                    source_scope_digest: &source_scope_digest,
                    generation_id: &generation_id,
                    trigger_kind: "periodic",
                },
                period_kind.key(),
                &period_key,
                commands::summary_recovery::SummaryRecoveryScope::Overall,
                "periodic_workflow",
                reason_code,
                Some(format!(
                    "periodic workflow {terminal_status} before emitting a readable batch result (started {started_at})"
                )),
            ),
        )?;
    }
    Ok(())
}

fn periodic_source_scope_digest(task: &commands::periodic_tasks::PeriodicTaskArgs) -> String {
    let sources = if task.sources.is_empty() {
        if task.source == "codex+claude-code" {
            vec!["codex", "claude-code"]
        } else {
            vec![task.source.as_str()]
        }
    } else {
        task.sources.iter().map(String::as_str).collect()
    };
    let mut labels = sources
        .into_iter()
        .map(|source| match source {
            "codex" => "agent:codex".to_string(),
            "claude-code" => "agent:claude_code".to_string(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>();
    labels.sort();
    labels.dedup();
    let match_mode = if task.source == "codex+claude-code" || labels.len() > 1 {
        "LABEL_ANY"
    } else {
        "LABEL_ALL"
    };
    commands::summaries::source_scope_digest(&labels, Some(match_mode))
}

fn periodic_failure_periods(
    task: &commands::periodic_tasks::PeriodicTaskArgs,
    triggered_at_ms: i64,
    timezone: chrono_tz::Tz,
) -> Vec<(commands::summary_calendar::PeriodKind, String)> {
    let task_kind = match task.task_kind {
        commands::periodic_tasks::PeriodicTaskKind::Regular => "regular",
        commands::periodic_tasks::PeriodicTaskKind::Weekly => "weekly",
        commands::periodic_tasks::PeriodicTaskKind::Monthly => "monthly",
    };
    commands::summary_calendar::periodic_failure_periods(
        task_kind,
        task.run_summary_daily,
        task.lookback_days.into(),
        triggered_at_ms,
        timezone,
    )
}

#[cfg(target_os = "linux")]
fn install_linux_web_process_diagnostics(app: &AppHandle) {
    use webkit2gtk::WebViewExt;

    let mut registration_failed = false;
    for window in app.webview_windows().into_values() {
        if window
            .with_webview(|webview| {
                webview.inner().connect_web_process_terminated(|_, reason| {
                    crate::commands::ui_diagnostics::report_web_process_termination(reason);
                });
            })
            .is_err()
        {
            registration_failed = true;
        }
    }
    if registration_failed {
        tracing::warn!(
            target: "ui_diagnostics",
            "failed to register WebKit renderer termination diagnostics"
        );
    }
}

/// Initialise tracing to stderr AND, when the data root is reachable, to
/// `<root>/log/lookback.log`. A bundled `.app` has no attached terminal, so
/// stderr-only logging meant the Rust-side logs (including the `memories-import`
/// child's stdout/stderr forwarded under `target: "memories-import"`) vanished
/// — which is exactly why a remote-import failure could not be diagnosed. The
/// file mirrors what sidecar logs already do (`<root>/log/<name>.std*.log`).
///
/// Falls back to stderr-only if the log file can't be opened; logging must
/// never block startup.
/// On Linux, WebKitGTK's DMABUF-based GPU renderer deadlocks the WebView on
/// several NVIDIA proprietary-driver setups (notably under Wayland or a
/// driver/CUDA mismatch): the window paints the first screen, then freezes the
/// whole UI the moment a later screen triggers a recomposite. The dev launcher
/// (`scripts/run-tauri.sh`) already exports these, but packaged builds
/// (AppImage/deb/rpm) have no such wrapper, so set them here BEFORE the WebView
/// initializes. Both honor an explicit user value so a Wayland user can opt
/// back in with `WEBKIT_DISABLE_DMABUF_RENDERER=0 GDK_BACKEND=wayland`.
#[cfg(target_os = "linux")]
fn apply_linux_webkit_workarounds() {
    fn set_default(key: &str, value: &str) {
        if std::env::var_os(key).is_none() {
            // Safe: single-threaded process start, before the WebView/GTK and
            // any other thread that might read the environment exists.
            unsafe { std::env::set_var(key, value) };
        }
    }
    set_default("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    set_default("GDK_BACKEND", "wayland,x11");
}

#[cfg(not(target_os = "linux"))]
fn apply_linux_webkit_workarounds() {}

fn init_tracing() {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr_layer = tracing_subscriber::fmt::layer().with_target(true);

    // Append (not truncate) so a crash-and-relaunch keeps prior context. The
    // shared writer rotates the active file on the common size/day boundary.
    let file = DataPaths::resolve().ok().and_then(|data| {
        let dir = data.log_dir();
        std::fs::create_dir_all(&dir).ok()?;
        // Point the fsync-per-line crash breadcrumb at the same log dir, so the
        // External→Local hard-crash position survives the OS panic that the
        // buffered tracing appender below loses (see `crashtrace`).
        crate::crashtrace::init(dir.clone());
        crate::log_rotation::SharedRotatingWriter::new(
            dir.join(crate::commands::logs::APP_LOG_FILE),
        )
        .ok()
    });

    let registry = tracing_subscriber::registry().with(stderr_layer.with_filter(filter()));
    let init = match file {
        Some(file) => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false) // a file is not a TTY; colour codes are noise
                    .with_target(true)
                    .with_writer(file)
                    .with_filter(filter()),
            )
            .try_init(),
        None => registry.try_init(),
    };
    let _ = init;
}

/// Stage plugin dylibs then bring the sidecars up, emitting the
/// `sidecar://ready` / `sidecar://error` events the frontend listens for.
///
/// Plugin dylibs are staged *before* spawn so jobworkerp sees them when it
/// scans `PLUGINS_RUNNER_DIR` (it only scans at startup, so a dylib added
/// after boot needs a full restart — which is exactly what the retry command
/// does via `Sidecars::stop` + this function). Staging failures degrade to a
/// `PluginsStageFailed` warning rather than blocking the browse-only paths.
///
/// Shared by `setup` (initial boot) and `commands::model::retry_model_setup`
/// (user-triggered retry after a failure).
pub(crate) async fn stage_and_start_sidecars(
    app: &AppHandle,
    sidecars: &Sidecars,
    data: &DataPaths,
) {
    if app
        .try_state::<std::sync::Arc<crate::grpc::issue_chat_gateway::IssueChatGateway>>()
        .is_none()
    {
        match crate::grpc::issue_chat_gateway::start(app.clone(), 0).await {
            Ok(server) => {
                let port = server.local_addr().port();
                // The worker manifest is expanded during sidecar registration;
                // only the local facade address is made available to it.
                unsafe {
                    std::env::set_var("LOOKBACK_ISSUE_CHAT_WORKER_HOST", "127.0.0.1");
                    std::env::set_var("LOOKBACK_ISSUE_CHAT_WORKER_PORT", port.to_string());
                }
                app.manage(server.gateway_state());
                // Dropping the handle detaches the server task. The runtime owns
                // it until process exit; permits are held in managed state.
                drop(server);
            }
            Err(error) => tracing::warn!(%error, "chat Issue facade unavailable"),
        }
    }
    let plugin_warnings = match crate::plugins::stage_plugins(app, &data.plugins_dir()) {
        Ok(report) => {
            tracing::info!(
                source = %report.source.display(),
                copied = report.copied.len(),
                skipped = report.skipped_same.len(),
                deleted = report.deleted.len(),
                "plugins staged"
            );
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(error = %e, "plugin staging failed");
            vec![SidecarWarning {
                kind: SidecarWarningKind::PluginsStageFailed,
                message: e.to_string(),
                detail: None,
            }]
        }
    };

    match sidecars.start_with_warnings(plugin_warnings).await {
        Ok(report) => {
            // A listening TCP port alone does not prove that the new explicit
            // maintenance service is compatible.  Verify its read-only RPC
            // before advertising this Local sidecar as ready, then request a
            // single reconcile for work left by a previous app session.
            if let Some(state) = app.try_state::<crate::commands::AppState>()
                && state.is_service_local(crate::commands::connection::Service::Memories)
            {
                let maintenance = state.search_index_maintenance.clone();
                match state.memories_channel().await {
                    Ok(channel) => {
                        if let Err(error) = maintenance.verify_service(channel.clone()).await {
                            tracing::error!(%error, "maintenance service compatibility check failed");
                            let _ = sidecars.stop().await;
                            crate::commands::emit_event(
                                app,
                                "sidecar://error",
                                crate::sidecar::startup_error::SidecarErrorPayload::from_app_error(
                                    &error,
                                ),
                            );
                            return;
                        }
                        if let Err(error) = maintenance.reconcile(channel).await {
                            tracing::warn!(%error, "initial search-index reconcile is pending retry");
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "maintenance service connection failed");
                        let _ = sidecars.stop().await;
                        crate::commands::emit_event(
                            app,
                            "sidecar://error",
                            crate::sidecar::startup_error::SidecarErrorPayload::from_app_error(
                                &error,
                            ),
                        );
                        return;
                    }
                }
            }
            crate::commands::emit_event(app, "sidecar://ready", report);
        }
        Err(e) => {
            tracing::error!(error = ?e, "sidecar start failed");
            // Lift the `AppError` into the tagged payload the frontend
            // expects so a structured failure carries its `code` (and
            // recovery-actionable fields like `expected_dim`) through to
            // the BootError UI instead of collapsing to a string.
            crate::commands::emit_event(
                app,
                "sidecar://error",
                crate::sidecar::startup_error::SidecarErrorPayload::from_app_error(&e),
            );
        }
    }
}

fn build_sidecar_config(handle: &AppHandle) -> Result<SidecarConfig, Box<dyn std::error::Error>> {
    let data = DataPaths::resolve()?;
    // The local sidecar performs its read-only memory_kind compatibility gate
    // immediately before `ensure()`. Only create runtime prerequisites here so
    // config construction never changes a legacy memories database first.
    data.ensure_runtime()?;

    // Resolve sidecar binary paths. Priority:
    //   1. LOOKBACK_JOBWORKERP_BIN / LOOKBACK_MEMORIES_BIN env (dev override).
    //   2. Tauri externalBin next to the packaged executable.
    //   3. `which` lookup on PATH.
    //   4. Hard-coded relative paths inside the parent jobworkerp workspace
    //      (developer convenience while sidecar bundling is not yet wired).
    let jobworkerp_bin = resolve_bin_for_app(
        handle,
        "LOOKBACK_JOBWORKERP_BIN",
        "all-in-one",
        "all-in-one",
        "../../target/release/all-in-one",
    )?;
    // memories ships its gRPC frontend as the `front` binary inside the
    // `grpc-admin` crate (`memories/grpc-admin/Cargo.toml` [[bin]] name = "front").
    // `which` looks up the more specific name `memories-front` to avoid PATH
    // collisions; the fallback resolves to the actual artifact name.
    let memories_bin = resolve_bin_for_app(
        handle,
        "LOOKBACK_MEMORIES_BIN",
        "front",
        "memories-front",
        "../../memories/target/release/front",
    )?;
    let memories_db_migrate_bin = resolve_memories_db_migrate_bin(handle);
    let conductor_bin = resolve_bin_for_app(
        handle,
        "LOOKBACK_CONDUCTOR_BIN",
        "conductor-main",
        "conductor-main",
        "../../conductor/target/release/conductor-main",
    )?;
    let protoc_bin = resolve_bin_for_app(
        handle,
        "PROTOC",
        "protoc",
        "protoc",
        "../../protobuf/bin/protoc",
    )?;
    if protoc_bin.exists() {
        // SAFETY: Tauri setup performs this before the sidecar startup task is
        // spawned. In-process registration and children then share the path.
        unsafe { std::env::set_var("PROTOC", &protoc_bin) };
    }

    // Pre-register the `memories-llm` + batch named workers before
    // memories starts dispatching LLM-containing workflows. Resolution
    // errors are tolerated (the apply step handles missing files
    // non-fatally and surfaces a SidecarWarning).
    let worker_yaml_paths = data::paths::llm_workers_yaml().ok().into_iter().collect();
    let issue_tracker_server_bin = issue_tracker_bin_for_build(
        crate::experimental_features::ISSUE_TRACKING_BUILD_ALLOWS,
        || {
            resolve_bin_for_app(
                handle,
                "LOOKBACK_ISSUE_TRACKER_SERVER_BIN",
                "issue-tracker-server",
                "issue-tracker-server",
                "../../issue-tracker-server/target/release/issue-tracker-server",
            )
            .map_err(|error| AppError::Config(error.to_string()))
        },
    )?;

    // Apply the `lookback-rag` function set (used by the RAG chat to
    // narrow the LLM's callable tool surface to `lookback_recall`).
    // Kept in a separate file because the worker-YAML deserializer
    // rejects unknown keys (rag-chat-design.md DECIDE-CHAT-9).
    let function_set_yaml_paths = data::paths::function_sets_yaml().ok().into_iter().collect();

    // Resolve the bundled workflows dir so the memories embedding dispatchers
    // use agent-app's staged YAMLs instead of memories' compile-time defaults.
    // Tolerate failure: memories falls back to its own defaults, and any
    // backend mismatch surfaces in memories logs.
    let workflows_dir = data::paths::workflows_bundle_dir().ok();

    // Stage the bundled Lindera IPADIC dictionary so the lindera-feature
    // `front` build can do Japanese/Korean morphological FTS. Missing
    // source degrades to ngram (lindera_dict_staged = false).
    let lance_language_model_home = data.lance_language_model_home();
    let lindera_dict_staged = match crate::lindera::stage_lindera_dict(
        handle,
        &data.lindera_ipadic_dir(),
    ) {
        Ok(Some(report)) => {
            tracing::info!(
                source = %report.source.display(),
                copied = report.copied.len(),
                skipped = report.skipped_same.len(),
                config = %report.config_path.display(),
                "lindera dictionary staged"
            );
            true
        }
        Ok(None) => {
            tracing::warn!("lindera dictionary source not found; FTS falls back to ngram");
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, "lindera dictionary staging failed; FTS falls back to ngram");
            false
        }
    };

    // Local LLM resolution: persisted Settings (Local mode preset / custom)
    // is the authoritative source. Process env (`LOOKBACK_LLM_MODEL` etc.)
    // is a dev override that still wins when the user has not yet touched
    // Settings (i.e. `local_preset_id == None`) — once they DO save, the
    // settings file is authoritative so a stray shell env can't silently
    // re-route the next launch back to the old model. The same triple is
    // re-resolved in `Sidecars::start_inner` on a restart so a Settings
    // change takes effect without a full app relaunch (the cached
    // `SidecarConfig` is frozen at boot).
    let llm_settings = commands::llm_settings::load_llm_settings(&data.llm_settings_path());
    let (llm_model, llm_hf_repo, llm_ctx_size) =
        commands::llm_settings::resolve_local_llm_env_triple(&llm_settings, |name| {
            std::env::var(name).ok()
        });
    let llm_kv_cache_type =
        commands::llm_settings::resolve_kv_cache_type_with_env(&llm_settings, |name| {
            std::env::var(name).ok()
        })
        .runner_value()
        .to_string();

    Ok(SidecarConfig {
        jobworkerp_bin,
        memories_bin,
        memories_db_migrate_bin,
        startup_database_migration_enabled: true,
        conductor_bin,
        data,
        worker_yaml_paths,
        issue_tracker_server_bin,
        function_set_yaml_paths,
        reflection_dispatch_enabled: true,
        auto_embedding_enabled: true,
        workflows_dir,
        lance_language_model_home,
        lindera_dict_staged,
        llm_model,
        llm_hf_repo,
        llm_ctx_size,
        llm_kv_cache_type: Some(llm_kv_cache_type),
        env_file: resolve_env_file(),
    })
}

fn issue_tracker_bin_for_build(
    build_allows: bool,
    resolve: impl FnOnce() -> AppResult<PathBuf>,
) -> AppResult<Option<PathBuf>> {
    if build_allows {
        resolve().map(Some)
    } else {
        Ok(None)
    }
}

fn resolve_memories_db_migrate_bin(app: &AppHandle) -> PathBuf {
    if let Ok(path) = std::env::var("LOOKBACK_MEMORIES_DB_MIGRATE_BIN") {
        return PathBuf::from(path);
    }
    if let Ok(resources) = app.path().resource_dir() {
        let candidate = resources
            .join("memories-db-migrate")
            .join("memories-db-migrate");
        if candidate.is_file() {
            return candidate;
        }
    }
    let candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("migration-bundle")
        .join("memories-db-migrate");
    if candidate.is_file() {
        return candidate;
    }
    candidate
}

/// Locate a `.env` template to forward to the sidecars. Resolution:
///   1. `LOOKBACK_ENV_FILE` env override,
///   2. `<CARGO_MANIFEST_DIR>/../../.env` (the parent workspace template
///      that jobworkerp / memories were authored against),
///   3. None — the sidecars then run against pure defaults.
fn resolve_env_file() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LOOKBACK_ENV_FILE") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let candidate = PathBuf::from(manifest_dir).join("../../.env");
    candidate.exists().then_some(candidate)
}

/// Resolve the bundled `memories-import` binary, erroring when it can't be
/// found anywhere. Single owner of the env-override name + bundled/fallback
/// paths so the import command and the sidecar's lang-worker registration
/// (`sidecar::generation_workers`) can't drift apart.
pub(crate) fn resolve_memories_import_bin_path() -> Result<PathBuf, AppError> {
    let p = resolve_bin(
        "LOOKBACK_MEMORIES_IMPORT_BIN",
        "memories-import",
        "memories-import",
        "../../memories/target/release/memories-import",
    )
    .map_err(|e| AppError::Config(format!("memories-import resolve failed: {e}")))?;
    if !p.exists() {
        return Err(AppError::Config(format!(
            "memories-import binary not found at {} — set LOOKBACK_MEMORIES_IMPORT_BIN",
            p.display()
        )));
    }
    Ok(p)
}

/// Exposed to the frontend so the Import dialog can default its
/// `memories-import` path without baking it into the JS bundle. Returns
/// `Err` when the binary can't be found anywhere — the UI surfaces this so
/// the user knows to set `LOOKBACK_MEMORIES_IMPORT_BIN`.
#[tauri::command]
fn resolve_memories_import_bin() -> Result<PathBuf, AppError> {
    resolve_memories_import_bin_path()
}

/// Resolve a sidecar / CLI binary in this order:
///   1. the app resource directory's package-specific sidecar locations,
///   2. `bundled_name` next to the running executable (Tauri `externalBin`
///      drops sidecars into `.app/Contents/MacOS/` alongside the app binary,
///      with the platform-triple suffix stripped at bundle time),
///   3. `env_var` override (set in dev to point at a local cargo build),
///   4. the target-triple-suffixed `externalBin` staged for `tauri dev`,
///   5. `on_path` via `which::which`,
///   6. a workspace fallback, only for non-packaged development runs.
///
/// `CARGO_MANIFEST_DIR` resolves to `agent-app/src-tauri/`, so workspace
/// siblings live at `../../<name>/...` (NOT `../<name>/...`, which would point
/// at a non-existent `agent-app/<name>/`).
///
/// Packaged applications never use PATH or workspace fallbacks. This keeps a
/// release independent of a developer checkout and avoids exposing local
/// filesystem paths in user-visible startup errors.
pub(crate) fn resolve_bin(
    env_var: &str,
    bundled_name: &str,
    on_path: &str,
    fallback_rel: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    resolve_bin_with_resources(env_var, bundled_name, on_path, fallback_rel, None)
}

/// Like [`resolve_bin`], but also consults Tauri's runtime resource directory.
/// This handles macOS app translocation and Linux package layouts without
/// assuming that `current_exe` retains the original bundle path.
pub(crate) fn resolve_bin_for_app(
    app: &AppHandle,
    env_var: &str,
    bundled_name: &str,
    on_path: &str,
    fallback_rel: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    resolve_bin_with_resources(
        env_var,
        bundled_name,
        on_path,
        fallback_rel,
        app.path().resource_dir().ok().as_deref(),
    )
}

fn resolve_bin_with_resources(
    env_var: &str,
    bundled_name: &str,
    on_path: &str,
    fallback_rel: &str,
    resource_dir: Option<&std::path::Path>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from));
    let staged_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin");
    resolve_bin_from_dir(
        env_var,
        bundled_name,
        on_path,
        fallback_rel,
        BinResolutionPaths {
            exe_dir: exe_dir.as_deref(),
            resource_dir,
            staged_dir: Some(&staged_dir),
            target_triple: env!("LOOKBACK_TARGET_TRIPLE"),
        },
    )
}

struct BinResolutionPaths<'a> {
    exe_dir: Option<&'a std::path::Path>,
    resource_dir: Option<&'a std::path::Path>,
    staged_dir: Option<&'a std::path::Path>,
    target_triple: &'a str,
}

fn resolve_bin_from_dir(
    env_var: &str,
    bundled_name: &str,
    on_path: &str,
    fallback_rel: &str,
    paths: BinResolutionPaths<'_>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    // A packaged app must prefer its own sidecars over inherited developer
    // overrides. For example, Finder can inherit an old terminal launch
    // environment whose override points at a deleted build directory.
    if let Some(resources) = paths.resource_dir {
        for bundled in resource_sidecar_candidates(resources, bundled_name) {
            if bundled.is_file() {
                return Ok(bundled);
            }
        }
    }
    if let Ok(p) = std::env::var(env_var) {
        return Ok(PathBuf::from(p));
    }
    if paths.resource_dir.is_some() {
        return Err(format!("bundled sidecar {bundled_name} is missing").into());
    }
    // Bundled sidecar: Tauri places `externalBin` entries next to the app
    // executable using the externalBin basename. This can differ from the
    // intentionally collision-resistant name used for PATH lookup.
    if let Some(dir) = paths.exe_dir {
        let bundled = dir.join(bundled_name);
        if bundled.exists() {
            return Ok(bundled);
        }
    }
    if let Some(dir) = paths.staged_dir {
        let staged = dir.join(format!("{bundled_name}-{}", paths.target_triple));
        if staged.is_file() {
            return Ok(staged);
        }
    }
    if let Ok(p) = which::which(on_path) {
        return Ok(p);
    }
    if cfg!(debug_assertions) {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        return Ok(PathBuf::from(manifest_dir).join(fallback_rel));
    }
    Err(format!("sidecar {bundled_name} is unavailable").into())
}

fn resource_sidecar_candidates(resources: &std::path::Path, name: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(parent) = resources.parent() {
        // macOS: Contents/Resources -> Contents/MacOS/<sidecar>.
        candidates.push(parent.join("MacOS").join(name));
        // Linux DEB/AppImage layouts commonly place sidecars next to the
        // resource directory's parent or directly in the resource directory.
        candidates.push(parent.join(name));
    }
    if let Some(usr) = resources.ancestors().find(|path| {
        path.file_name().is_some_and(|name| name == "usr")
            && resources
                .strip_prefix(path)
                .ok()
                .and_then(|relative| relative.components().next())
                .is_some_and(|component| component.as_os_str() == "lib")
    }) {
        // AppImage puts externalBin entries in usr/bin while Tauri reports
        // resources from usr/lib/<product>[/resources].
        candidates.push(usr.join("bin").join(name));
    }
    candidates.push(resources.join(name));
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEnvVarGuard {
        key: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl TestEnvVarGuard {
        fn new(key: &'static str) -> Self {
            Self {
                key,
                original: std::env::var_os(key),
            }
        }
    }

    impl Drop for TestEnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: resolve-bin tests run with one test thread because env
            // variables are process-global and mutation is not thread-safe.
            unsafe {
                match &self.original {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    fn periodic_task(
        kind: commands::periodic_tasks::PeriodicTaskKind,
    ) -> commands::periodic_tasks::PeriodicTaskArgs {
        commands::periodic_tasks::PeriodicTaskArgs {
            name: "test".into(),
            source: "codex".into(),
            sources: vec!["codex".into()],
            task_kind: kind,
            hour: 0,
            minute: 0,
            interval_hours: None,
            interval_days: Some(1),
            weekly_day: None,
            monthly_day: None,
            lookback_days: 1,
            force_thread_summary: true,
            run_summary_thread_group: true,
            run_summary_daily: true,
            run_personality: false,
            run_reflection: false,
            stop_on_error: true,
        }
    }

    #[test]
    fn issue_tracker_bin_is_not_resolved_when_build_disallows_tracking() {
        let resolved = std::cell::Cell::new(false);

        let result = issue_tracker_bin_for_build(false, || {
            resolved.set(true);
            Err(AppError::Config("resolver must not run".into()))
        })
        .unwrap();

        assert_eq!(result, None);
        assert!(!resolved.get());
    }

    #[test]
    fn issue_tracker_bin_is_resolved_when_build_allows_tracking() {
        let expected = PathBuf::from("issue-tracker-server");

        let result = issue_tracker_bin_for_build(true, || Ok(expected.clone())).unwrap();

        assert_eq!(result, Some(expected));
    }

    #[test]
    fn issue_tracker_bin_resolution_errors_are_propagated() {
        let result =
            issue_tracker_bin_for_build(true, || Err(AppError::Config("resolver failed".into())));

        assert!(matches!(
            result,
            Err(AppError::Config(message)) if message == "resolver failed"
        ));
    }

    #[test]
    fn successful_periodic_execution_records_no_failure() {
        use commands::periodic_execution::PeriodicExecutionStatus;

        assert_eq!(
            periodic_terminal_failure(PeriodicExecutionStatus::Succeeded),
            None
        );
        assert_eq!(
            periodic_terminal_failure(PeriodicExecutionStatus::Failed),
            Some(("workflow_execution_failed", "failed"))
        );
        assert_eq!(
            periodic_terminal_failure(PeriodicExecutionStatus::Cancelled),
            Some(("workflow_execution_cancelled", "cancelled"))
        );
        assert_eq!(
            periodic_terminal_failure(PeriodicExecutionStatus::EnqueueFailed),
            Some(("workflow_enqueue_failed", "enqueue_failed"))
        );
    }

    #[test]
    fn retry_warnings_warn_once_per_execution_until_it_finishes() {
        let mut warnings = RetryWarnings::default();

        assert!(warnings.first_failure("exec-1"));
        assert!(!warnings.first_failure("exec-1"));
        assert!(warnings.first_failure("exec-2"));

        warnings.finished("exec-1");
        assert!(warnings.first_failure("exec-1"));
    }

    #[test]
    fn concise_error_keeps_short_text_and_truncates_long_text_on_char_boundaries() {
        assert_eq!(concise_error("short", 10), "short");
        assert_eq!(concise_error("0123456789abc", 10), "0123456789…");
        // Multi-byte characters must not be split mid-codepoint.
        assert_eq!(concise_error("日本語のエラーです", 3), "日本語…");
    }

    #[test]
    fn strict_periodic_failure_records_the_same_daily_window_as_regular_batch() {
        let task = periodic_task(commands::periodic_tasks::PeriodicTaskKind::Regular);
        // 2026-09-04T00:00Z is 09:00 JST, so a one-day lookback targets
        // the previous completed local day.
        let periods = periodic_failure_periods(&task, 1_788_480_000_000, chrono_tz::Asia::Tokyo);
        assert_eq!(
            periods
                .iter()
                .map(|(kind, key)| (kind.key(), key.as_str()))
                .collect::<Vec<_>>(),
            vec![("daily", "2026-09-03")]
        );
    }

    #[test]
    fn periodic_failure_uses_the_worker_timezone_at_the_date_boundary() {
        let task = periodic_task(commands::periodic_tasks::PeriodicTaskKind::Regular);
        // The same instant is Sep 4 in Tokyo but Sep 3 in New York. The
        // one-day recovery window must match the worker's TZ calendar.
        let periods =
            periodic_failure_periods(&task, 1_788_480_000_000, chrono_tz::America::New_York);
        assert_eq!(
            periods
                .iter()
                .map(|(kind, key)| (kind.key(), key.as_str()))
                .collect::<Vec<_>>(),
            vec![("daily", "2026-09-02")]
        );
    }

    #[test]
    fn strict_weekly_periodic_failure_keeps_the_weekly_rollup_target() {
        let task = periodic_task(commands::periodic_tasks::PeriodicTaskKind::Weekly);
        let periods = periodic_failure_periods(&task, 1_788_480_000_000, chrono_tz::Asia::Tokyo);
        assert!(
            periods
                .iter()
                .any(|(kind, key)| kind.key() == "weekly" && key == "2026-W35")
        );
        assert_eq!(
            periods
                .iter()
                .filter(|(kind, _)| kind.key() == "daily")
                .count(),
            7
        );
    }

    #[test]
    fn strict_monthly_periodic_failure_keeps_daily_and_weekly_dependencies() {
        let task = periodic_task(commands::periodic_tasks::PeriodicTaskKind::Monthly);
        let periods = periodic_failure_periods(&task, 1_788_480_000_000, chrono_tz::Asia::Tokyo);
        // The previous month is August 2026. Its first day is Saturday, so
        // monthlyStage rebuilds daily dependencies from the prior Monday.
        assert!(
            periods
                .iter()
                .any(|(kind, key)| kind.key() == "daily" && key == "2026-07-27")
        );
        assert!(
            periods
                .iter()
                .any(|(kind, key)| kind.key() == "daily" && key == "2026-08-31")
        );
        assert!(
            periods
                .iter()
                .any(|(kind, key)| kind.key() == "weekly" && key == "2026-W31")
        );
        assert!(
            periods
                .iter()
                .any(|(kind, key)| kind.key() == "weekly" && key == "2026-W36")
        );
        assert!(
            periods
                .iter()
                .any(|(kind, key)| kind.key() == "monthly" && key == "2026-08")
        );
    }

    #[test]
    fn resolve_bin_prefers_env_override() {
        // SAFETY: single-threaded test; unique env var name.
        unsafe { std::env::set_var("LOOKBACK_TEST_BIN_X", "/custom/path/to/bin") };
        let p = resolve_bin(
            "LOOKBACK_TEST_BIN_X",
            "definitely-not-bundled",
            "definitely-not-on-path",
            "fallback",
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/custom/path/to/bin"));
        unsafe { std::env::remove_var("LOOKBACK_TEST_BIN_X") };
    }

    // The manifest-relative workspace fallback exists only in debug builds.
    #[cfg(debug_assertions)]
    #[test]
    fn resolve_bin_falls_back_to_manifest_relative_when_nothing_else() {
        let _env_guard = TestEnvVarGuard::new("LOOKBACK_TEST_BIN_Y");
        // A name that isn't on PATH and isn't next to the test binary must
        // resolve to the CARGO_MANIFEST_DIR-relative fallback.
        unsafe { std::env::remove_var("LOOKBACK_TEST_BIN_Y") };
        let p = resolve_bin(
            "LOOKBACK_TEST_BIN_Y",
            "lookback-nonexistent-bundled-binary-zzz",
            "lookback-nonexistent-binary-zzz",
            "../../target/release/some-bin",
        )
        .unwrap();
        assert!(
            p.ends_with("../../target/release/some-bin"),
            "got {}",
            p.display()
        );
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn resolve_bin_returns_unavailable_when_nothing_else_exists() {
        let _env_guard = TestEnvVarGuard::new("LOOKBACK_TEST_BIN_Y");
        unsafe { std::env::remove_var("LOOKBACK_TEST_BIN_Y") };
        let error = resolve_bin(
            "LOOKBACK_TEST_BIN_Y",
            "lookback-nonexistent-bundled-binary-zzz",
            "lookback-nonexistent-binary-zzz",
            "../../target/release/some-bin",
        )
        .unwrap_err();

        assert!(error.to_string().contains("is unavailable"));
    }

    #[test]
    fn resolve_bin_uses_a_distinct_bundled_name() {
        let dir = tempfile::tempdir().unwrap();
        let bundled = dir.path().join("front");
        std::fs::write(&bundled, b"bundled sidecar").unwrap();

        unsafe { std::env::remove_var("LOOKBACK_TEST_BIN_BUNDLED_NAME") };
        let p = resolve_bin_from_dir(
            "LOOKBACK_TEST_BIN_BUNDLED_NAME",
            "front",
            "lookback-nonexistent-memories-front-zzz",
            "fallback",
            BinResolutionPaths {
                exe_dir: Some(dir.path()),
                resource_dir: None,
                staged_dir: None,
                target_triple: "test-target",
            },
        )
        .unwrap();

        assert_eq!(p, bundled);
    }

    #[test]
    fn resolve_bin_uses_a_resource_directory_sidecar_when_executable_path_differs() {
        let dir = tempfile::tempdir().unwrap();
        let resources = dir.path().join("Contents/Resources");
        let macos = dir.path().join("Contents/MacOS");
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::create_dir_all(&macos).unwrap();
        let bundled = macos.join("front");
        std::fs::write(&bundled, b"bundled sidecar").unwrap();

        let p = resolve_bin_from_dir(
            "LOOKBACK_TEST_BIN_RESOURCE",
            "front",
            "lookback-nonexistent-resource-bin-zzz",
            "fallback",
            BinResolutionPaths {
                exe_dir: None,
                resource_dir: Some(&resources),
                staged_dir: None,
                target_triple: "test-target",
            },
        )
        .unwrap();

        assert_eq!(p, bundled);
    }

    #[test]
    fn packaged_resolution_ignores_an_inherited_development_override() {
        let dir = tempfile::tempdir().unwrap();
        let resources = dir.path().join("Contents/Resources");
        let macos = dir.path().join("Contents/MacOS");
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::create_dir_all(&macos).unwrap();
        let bundled = macos.join("front");
        std::fs::write(&bundled, b"bundled sidecar").unwrap();

        unsafe {
            std::env::set_var(
                "LOOKBACK_TEST_BIN_PACKAGED_OVERRIDE",
                "/Users/example/target/release/front",
            )
        };
        let resolved = resolve_bin_from_dir(
            "LOOKBACK_TEST_BIN_PACKAGED_OVERRIDE",
            "front",
            "lookback-nonexistent-packaged-override-bin-zzz",
            "fallback",
            BinResolutionPaths {
                exe_dir: None,
                resource_dir: Some(&resources),
                staged_dir: None,
                target_triple: "test-target",
            },
        )
        .unwrap();
        unsafe { std::env::remove_var("LOOKBACK_TEST_BIN_PACKAGED_OVERRIDE") };

        assert_eq!(resolved, bundled);
    }

    #[test]
    fn packaged_resolution_never_falls_back_to_a_development_path() {
        let dir = tempfile::tempdir().unwrap();
        let resources = dir.path().join("Contents/Resources");
        std::fs::create_dir_all(&resources).unwrap();

        let error = resolve_bin_from_dir(
            "LOOKBACK_TEST_BIN_PACKAGED",
            "front",
            "lookback-nonexistent-packaged-bin-zzz",
            "/Users/example/workspace/front",
            BinResolutionPaths {
                exe_dir: None,
                resource_dir: Some(&resources),
                staged_dir: None,
                target_triple: "test-target",
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("bundled sidecar front is missing")
        );
        assert!(!error.to_string().contains("/Users/example"));
    }

    #[test]
    fn resolve_bin_uses_a_linux_package_resource_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let resources = dir.path().join("usr/lib/lookback/resources");
        let package_dir = resources.parent().unwrap();
        std::fs::create_dir_all(&resources).unwrap();
        let bundled = package_dir.join("front");
        std::fs::write(&bundled, b"bundled sidecar").unwrap();

        let p = resolve_bin_from_dir(
            "LOOKBACK_TEST_BIN_LINUX_RESOURCE",
            "front",
            "lookback-nonexistent-linux-resource-bin-zzz",
            "fallback",
            BinResolutionPaths {
                exe_dir: None,
                resource_dir: Some(&resources),
                staged_dir: None,
                target_triple: "test-target",
            },
        )
        .unwrap();

        assert_eq!(p, bundled);
    }

    #[test]
    fn resolve_bin_uses_an_appimage_usr_bin_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let resources = dir.path().join("usr/lib/Lookback");
        let bundled = dir.path().join("usr/bin/all-in-one");
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::create_dir_all(bundled.parent().unwrap()).unwrap();
        std::fs::write(&bundled, b"bundled sidecar").unwrap();

        let p = resolve_bin_from_dir(
            "LOOKBACK_TEST_BIN_APPIMAGE_RESOURCE",
            "all-in-one",
            "lookback-nonexistent-appimage-bin-zzz",
            "fallback",
            BinResolutionPaths {
                exe_dir: None,
                resource_dir: Some(&resources),
                staged_dir: None,
                target_triple: "test-target",
            },
        )
        .unwrap();

        assert_eq!(p, bundled);
    }

    #[test]
    fn resolve_bin_does_not_treat_a_macos_usr_local_bundle_as_an_appimage() {
        let dir = tempfile::tempdir().unwrap();
        let resources = dir.path().join("usr/local/Lookback.app/Contents/Resources");
        let unrelated = dir.path().join("usr/bin/front");
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::create_dir_all(unrelated.parent().unwrap()).unwrap();
        std::fs::write(&unrelated, b"not a bundled macOS sidecar").unwrap();

        let error = resolve_bin_from_dir(
            "LOOKBACK_TEST_BIN_MACOS_USR_LOCAL",
            "front",
            "lookback-nonexistent-macos-bin-zzz",
            "fallback",
            BinResolutionPaths {
                exe_dir: None,
                resource_dir: Some(&resources),
                staged_dir: None,
                target_triple: "test-target",
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("bundled sidecar front is missing")
        );
    }

    #[test]
    fn resolve_bin_uses_the_staged_development_external_binary() {
        let dir = tempfile::tempdir().unwrap();
        let staged_dir = dir.path().join("bin");
        std::fs::create_dir_all(&staged_dir).unwrap();
        let staged = staged_dir.join("front-test-target");
        std::fs::write(&staged, b"staged sidecar").unwrap();

        let p = resolve_bin_from_dir(
            "LOOKBACK_TEST_BIN_STAGED",
            "front",
            "lookback-nonexistent-staged-bin-zzz",
            "fallback",
            BinResolutionPaths {
                exe_dir: None,
                resource_dir: None,
                staged_dir: Some(&staged_dir),
                target_triple: "test-target",
            },
        )
        .unwrap();

        assert_eq!(p, staged);
    }

    #[test]
    fn bundle_resources_use_stable_runtime_paths() {
        // Platform-agnostic resources + externalBin live in tauri.conf.json.
        // The plugin shared-library globs are platform-specific (a *.so glob
        // errors on macOS and a *.dylib glob errors on Linux, since tauri-build
        // rejects a glob that matches nothing), so they live in the
        // tauri.<platform>.conf.json files that tauri-build auto-merges.
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let resources = config["bundle"]["resources"]
            .as_object()
            .expect("bundle.resources must map source directories to stable runtime paths");

        assert_eq!(resources.get("../workers/"), Some(&"workers/".into()));
        assert_eq!(resources.get("../dict/"), Some(&"dict/".into()));
        assert_eq!(
            resources.get("migration-bundle/"),
            Some(&"memories-db-migrate/".into())
        );
        // The plugin glob must NOT be in the shared config (would break the
        // other platform's build).
        assert!(resources.get("plugins/*.dylib").is_none());
        assert!(resources.get("plugins/*.so*").is_none());

        let external_bins = config["bundle"]["externalBin"].as_array().unwrap();
        assert!(external_bins.iter().any(|entry| entry == "bin/protoc"));

        // Each platform overlay carries its own plugin glob.
        let macos: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.macos.conf.json")).unwrap();
        assert_eq!(
            macos["bundle"]["resources"].get("plugins/*.dylib"),
            Some(&"plugins/".into())
        );
        let linux: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.linux.conf.json")).unwrap();
        assert_eq!(
            linux["bundle"]["resources"].get("plugins/*.so*"),
            Some(&"plugins/".into())
        );
    }

    #[test]
    fn shell_open_is_limited_to_the_memory_kind_migration_release() {
        let capability: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
        let permissions = capability["permissions"]
            .as_array()
            .expect("default capability has permissions");
        assert!(
            permissions
                .iter()
                .any(|permission| permission == "shell:allow-open"),
            "the migration-release button needs the shell open command"
        );
        assert!(
            !permissions
                .iter()
                .any(|permission| permission == "shell:default"),
            "shell:default would grant the plugin's broad http(s), tel and mailto scope"
        );

        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(
            config["plugins"]["shell"]["open"],
            "https://github\\.com/jobworkerp-rs/Lookback/releases/tag/v0\\.0\\.7",
            "the shell plugin anchors this validation regex, so only the exact release URL is openable"
        );
    }
}
