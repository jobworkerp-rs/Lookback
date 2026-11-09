use serde::Serialize;
use tauri::State;

use crate::{
    error::{AppError, AppResult},
    grpc::{issue_tracker::IssueTrackerClientError, proto::issue_tracker::manager::v1 as manager},
};

use super::super::AppState;
use super::common::map_client_error;
use super::query::collect_pages;
use super::source_evidence::{IssueEvidence, resolve_source_evidence};

fn diagnosis_to_view(diagnosis: manager::OwnershipIntegrityDiagnosis) -> OwnershipDiagnosisView {
    OwnershipDiagnosisView {
        target_kind: diagnosis.target_kind,
        target_id: diagnosis.target_id,
        diagnosis_class: diagnosis.diagnosis_class,
        evidence_digest: diagnosis.evidence_digest,
        diagnosis_generation: diagnosis.diagnosis_generation.to_string(),
        observation_token: diagnosis.observation_token.to_string(),
        repair_epoch: diagnosis.repair_epoch.to_string(),
        observed_revision: diagnosis.observed_revision.map(|value| value.to_string()),
        lease_classification: diagnosis.lease_classification,
        repair_condition: diagnosis.repair_condition,
        state: diagnosis.state,
        resolution_operation_id: diagnosis.resolution_operation_id,
        lease_expires_at_ms: diagnosis
            .lease_expires_at
            .map(|value| value.unix_epoch_millis),
        open_blocker_issue_ids: diagnosis.open_blocker_issue_ids,
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct OwnershipDiagnosisView {
    pub target_kind: String,
    pub target_id: String,
    pub diagnosis_class: String,
    pub evidence_digest: String,
    pub diagnosis_generation: String,
    pub observation_token: String,
    pub repair_epoch: String,
    pub observed_revision: Option<String>,
    pub lease_classification: Option<String>,
    pub repair_condition: String,
    pub state: String,
    pub resolution_operation_id: Option<String>,
    pub lease_expires_at_ms: Option<i64>,
    pub open_blocker_issue_ids: Vec<String>,
}
pub(super) async fn list_issue_ownership_diagnostics_inner(
    state: State<'_, AppState>,
    issue_id: Option<String>,
) -> AppResult<Vec<OwnershipDiagnosisView>> {
    let service = state.issue_tracker_service().await?;
    let page = collect_pages(|page_token| {
        let service = service.clone();
        let issue_id = issue_id.clone();
        async move {
            service
                .list_ownership_diagnostics(
                    "issue-ownership-diagnostics",
                    manager::ListOwnershipDiagnosticsArgs {
                        page: Some(manager::PageRequest {
                            page_size: 100,
                            page_token,
                        }),
                        target_kind: issue_id.as_ref().map(|_| "issue".into()),
                        target_id: issue_id,
                        ..Default::default()
                    },
                )
                .await
                .map(|page| {
                    (
                        page.diagnoses,
                        page.page.and_then(|page| page.next_page_token),
                    )
                })
                .map_err(map_client_error)
        }
    })
    .await?;
    Ok(page.into_iter().map(diagnosis_to_view).collect())
}
pub(super) async fn get_issue_evidence_inner(
    state: State<'_, AppState>,
    item_id: String,
) -> AppResult<Vec<IssueEvidence>> {
    let service = state.issue_tracker_service().await?;
    let candidate = service
        .get_candidate(
            "issue-evidence-candidate",
            manager::GetCandidateArgs {
                candidate_id: item_id.clone(),
                ..Default::default()
            },
        )
        .await;
    let references = match candidate {
        Ok(candidate) => candidate.source_evidence,
        Err(error) if candidate_not_found(&error) => {
            service
                .get_issue_with_sync(
                    "issue-evidence-issue",
                    manager::GetIssueWithSyncArgs {
                        issue_id: item_id,
                        ..Default::default()
                    },
                )
                .await
                .map_err(map_client_error)?
                .issue
                .ok_or_else(|| AppError::IssueTrackerServer("Issue is missing".into()))?
                .source_evidence
        }
        Err(error) => return Err(map_client_error(error)),
    };
    resolve_source_evidence(&state, &references).await
}

fn candidate_not_found(error: &IssueTrackerClientError) -> bool {
    matches!(
        error,
        IssueTrackerClientError::Manager(manager_error)
            if manager_error.code == manager::ManagerErrorCode::CandidateNotFound as i32
    )
}
