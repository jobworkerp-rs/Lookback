//! Commands backing the Issue Tracker management page.
//!
//! The renderer receives screen-oriented DTOs rather than protobuf messages so
//! the manager's internal revisions and oneofs stay behind the Tauri boundary.

mod admin;
mod common;
mod diagnostics;
mod history;
#[path = "issue_operation_journal.rs"]
pub(crate) mod issue_operation_journal;
mod query;
mod relations;
mod review;
pub(crate) mod source_evidence;
mod source_labels;

use super::AppState;
use crate::error::AppResult;
use tauri::{AppHandle, State};

pub use admin::{
    ApplyIssueAdminActionRequest, CompletionEvidenceRequest, CompletionReportRequest,
    DependencyOverrideRequest, GetIssueOperationStatusRequest, InconsistentClaimResolutionRequest,
    IssueOperationStatus, OwnershipDiagnosisReferenceRequest, PendingIssueOperationView,
    ResolveIssueOperationRequest, RetryPendingIssueOperationRequest,
};
pub use diagnostics::OwnershipDiagnosisView;
pub use history::{
    IssueHistoryEntry, IssueHistoryMode, IssueHistoryPage, IssueHistoryPageOutcome,
    IssueHistoryResponse, ListIssueDirectHistoryRequest,
};
pub use relations::{CreateIssueFromCanceledRequest, IssueDuplicate};
pub use review::{
    CreateExplicitIssueRequest, IssueClaimView, IssueListItem, IssueReviewItem,
    ListIssueReviewRequest, ListIssuesRequest, ReextractRejectedCandidateRequest,
    ResolveIssueReviewRequest, TransitionIssueStateRequest, UpdateIssueCandidateRequest,
};
pub use source_evidence::IssueEvidence;

#[tauri::command]
pub async fn list_issue_review_items(
    state: State<'_, AppState>,
    req: ListIssueReviewRequest,
) -> AppResult<Vec<IssueReviewItem>> {
    review::list_issue_review_items_inner(state, req).await
}

#[tauri::command]
pub async fn list_issues(
    state: State<'_, AppState>,
    req: ListIssuesRequest,
) -> AppResult<Vec<IssueListItem>> {
    review::list_issues_inner(state, req).await
}

#[tauri::command]
pub async fn get_issue_detail(
    state: State<'_, AppState>,
    issue_id: String,
) -> AppResult<IssueListItem> {
    review::get_issue_detail_inner(state, issue_id).await
}

#[tauri::command]
pub async fn list_issue_ownership_diagnostics(
    state: State<'_, AppState>,
    issue_id: Option<String>,
) -> AppResult<Vec<OwnershipDiagnosisView>> {
    diagnostics::list_issue_ownership_diagnostics_inner(state, issue_id).await
}

#[tauri::command]
pub async fn get_issue_operation_status(
    state: State<'_, AppState>,
    req: GetIssueOperationStatusRequest,
) -> AppResult<IssueOperationStatus> {
    admin::get_issue_operation_status_inner(state, req).await
}

#[tauri::command]
pub async fn apply_issue_admin_action(
    state: State<'_, AppState>,
    req: ApplyIssueAdminActionRequest,
) -> AppResult<IssueOperationStatus> {
    admin::apply_issue_admin_action_inner(state, req).await
}

#[tauri::command]
pub async fn resolve_issue_operation(
    state: State<'_, AppState>,
    req: ResolveIssueOperationRequest,
) -> AppResult<IssueOperationStatus> {
    admin::resolve_issue_operation_inner(state, req).await
}

#[tauri::command]
pub async fn list_pending_issue_operations(
    state: State<'_, AppState>,
) -> AppResult<Vec<PendingIssueOperationView>> {
    admin::list_pending_issue_operations_inner(state).await
}

#[tauri::command]
pub async fn retry_pending_issue_operation(
    state: State<'_, AppState>,
    req: RetryPendingIssueOperationRequest,
) -> AppResult<IssueOperationStatus> {
    admin::retry_pending_issue_operation_inner(state, req).await
}

#[tauri::command]
pub async fn list_issue_direct_history(
    state: State<'_, AppState>,
    req: ListIssueDirectHistoryRequest,
) -> AppResult<IssueHistoryPageOutcome> {
    history::list_issue_direct_history_inner(state, req).await
}

#[tauri::command]
pub async fn create_issue_from_canceled(
    state: State<'_, AppState>,
    req: CreateIssueFromCanceledRequest,
) -> AppResult<IssueListItem> {
    relations::create_issue_from_canceled_inner(state, req).await
}

#[tauri::command]
pub async fn get_issue_evidence(
    state: State<'_, AppState>,
    item_id: String,
) -> AppResult<Vec<IssueEvidence>> {
    diagnostics::get_issue_evidence_inner(state, item_id).await
}

#[tauri::command]
pub async fn get_issue_duplicates(
    state: State<'_, AppState>,
    item_id: String,
) -> AppResult<Vec<IssueDuplicate>> {
    relations::get_issue_duplicates_inner(state, item_id).await
}

#[tauri::command]
pub async fn list_issue_history(state: State<'_, AppState>) -> AppResult<IssueHistoryResponse> {
    history::list_issue_history_inner(state).await
}

#[tauri::command]
pub async fn resolve_issue_review(
    state: State<'_, AppState>,
    req: ResolveIssueReviewRequest,
) -> AppResult<()> {
    review::resolve_issue_review_inner(state, req).await
}

#[tauri::command]
pub async fn update_issue_candidate(
    state: State<'_, AppState>,
    req: UpdateIssueCandidateRequest,
) -> AppResult<()> {
    review::update_issue_candidate_inner(state, req).await
}

#[tauri::command]
pub async fn create_explicit_issue_from_candidate(
    state: State<'_, AppState>,
    req: CreateExplicitIssueRequest,
) -> AppResult<()> {
    review::create_explicit_issue_from_candidate_inner(state, req).await
}

#[tauri::command]
pub async fn reextract_rejected_candidate(
    app: AppHandle,
    state: State<'_, AppState>,
    req: ReextractRejectedCandidateRequest,
) -> AppResult<crate::commands::analysis_dispatch::EnqueueAnalysisJobResponse> {
    review::reextract_rejected_candidate_inner(app, state, req).await
}

#[tauri::command]
pub async fn transition_issue_state(
    state: State<'_, AppState>,
    req: TransitionIssueStateRequest,
) -> AppResult<()> {
    review::transition_issue_state_inner(state, req).await
}
