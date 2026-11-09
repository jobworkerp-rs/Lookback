//! Tauri commands backing Import. Drives the 4-step
//! pipeline (import → summary → personality → reflection) directly via
//! `jobworkerp::run_cancellable_named_stream` so `staged://step` events surface
//! per-chunk progress to the toast. The three post-import generation
//! steps are individually selectable from the dialog (`run_summary` /
//! `run_personality` / `run_reflection`); an unselected step is emitted
//! as a skipped `Waiting` rather than dispatched.

mod aggregator;
mod commands;
mod events;
mod forward;
mod guards;
mod pending;
mod pending_store;
mod plan;
mod request;
mod step_runner;
mod workflow_inputs;

pub use guards::ImportSource;
pub(super) use guards::REFLECTION_PROMPT_VERSION;
pub(super) use request::resolve_opencode_db_path;
pub use request::{
    ImportNoticeCode, ImportProgressRegistry, ImportStep, ImportStepState, PlainImportConfig,
    StartImportRequest, StartImportResponse, ThreadStrategy,
};
pub(super) use workflow_inputs::{
    BatchDispatch, SummaryBudgetRequirement, SummaryLlmContract, summary_llm_contract_with,
    summary_output_guidance_for_context,
};
pub use workflow_inputs::{GenerateSummariesRequest, PeriodKind, PeriodRange};

use super::AppState;
use super::staged_progress::ProgressKind;
use crate::error::AppResult;
pub(super) use step_runner::{
    CancellableStepResult, emit_staged_step, log_thread_group_failure, run_cancellable_step,
    staged_step_job_timeout_sec, summarize_workflow_chunk, summarize_workflow_error,
    terminal_batch_outcome, thread_group_summary_terminal_outcome, workflow_output_value,
    workflow_terminal_failure,
};
use tauri::{AppHandle, State};

#[tauri::command]
pub async fn start_import(
    app: AppHandle,
    state: State<'_, AppState>,
    req: StartImportRequest,
) -> AppResult<StartImportResponse> {
    commands::start_import_inner(app, state, req).await
}

#[tauri::command]
pub async fn start_import_cancel(state: State<'_, AppState>, dispatch_id: String) -> AppResult<()> {
    commands::start_import_cancel_inner(state, dispatch_id).await
}

#[tauri::command]
pub fn get_active_staged_progress(
    state: State<'_, AppState>,
    kind: String,
) -> Option<super::staged_progress::StagedProgressSnapshot> {
    let kind = ProgressKind::parse(&kind)?;
    state.import_progress.active_snapshot_for(Some(kind))
}

#[cfg(test)]
mod tests_common;
