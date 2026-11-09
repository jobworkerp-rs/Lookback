//! Commands for persisted human review requirements.

use serde::Deserialize;
use tauri::State;

use crate::{
    error::AppResult,
    issue_review_policy::{IssueReviewPolicy, IssueReviewPolicyRegistry},
};

use super::AppState;

#[derive(Debug, Clone, Deserialize)]
pub struct SetIssueReviewPolicyRequest {
    pub policy: IssueReviewPolicy,
}

#[tauri::command]
pub fn get_issue_review_policy(state: State<'_, AppState>) -> AppResult<IssueReviewPolicy> {
    Ok(IssueReviewPolicyRegistry::open(&state.data.root).load())
}

#[tauri::command]
pub fn set_issue_review_policy(
    state: State<'_, AppState>,
    req: SetIssueReviewPolicyRequest,
) -> AppResult<IssueReviewPolicy> {
    IssueReviewPolicyRegistry::open(&state.data.root).update(req.policy.clone())?;
    Ok(req.policy)
}
