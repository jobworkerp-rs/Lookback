use std::{collections::HashSet, time::Duration};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::{
    error::{AppError, AppResult},
    grpc::{
        issue_tracker::ManagerWriteRequest,
        proto::{
            issue_tracker::{manager::v1 as manager, v1},
            llm_memory::{data as mem_data, service::thread_service_client::ThreadServiceClient},
        },
    },
};

use super::super::AppState;
use super::common::{human_actor, map_client_error, parse_revision, require_rationale};
use super::query::*;
use super::source_evidence::all_source_evidence_available;
use super::source_labels::{source_labels_from_candidate, source_labels_from_issue};

const RESOLUTION_WAIT: Duration = Duration::from_secs(10);
#[derive(Debug, Clone, Serialize)]
pub struct IssueReviewItem {
    pub id: String,
    pub revision: String,
    pub associated_issue_id: Option<String>,
    pub issue_id: Option<String>,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub priority: String,
    pub suggested_state: String,
    pub acceptance_criteria: Vec<String>,
    pub confidence: f64,
    pub review_state: String,
    pub detection: String,
    pub strong_proof: bool,
    pub evidence_count: usize,
    pub duplicate_count: usize,
    pub issue_state: Option<String>,
    pub integrity: String,
    pub resolution_state: String,
    pub source_labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueListItem {
    pub id: String,
    pub revision: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub priority: String,
    pub state: String,
    pub review_state: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub completion_criteria: Option<String>,
    /// Preserve item boundaries because `completion_criteria` is display-only joined text.
    #[serde(rename = "acceptanceCriteriaItems")]
    pub acceptance_criteria_items: Vec<String>,
    pub registration_path: Option<String>,
    pub policy: Option<String>,
    pub proof: Option<String>,
    pub source_labels: Vec<String>,
    pub contexts: Vec<IssueContext>,
    pub claim: Option<IssueClaimView>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IssueContext {
    pub kind: IssueContextKind,
    pub value: String,
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IssueContextKind {
    Project,
    Repository,
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueClaimView {
    pub lease_expires_at_ms: Option<i64>,
    pub claim_revision: String,
    /// A client-clock observation only; it is never used to authorize a
    /// recovery or revoke operation.
    pub lease_status: String,
}
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListIssueReviewRequest {
    pub target: Option<String>,
    pub priority: Option<String>,
    #[serde(default)]
    pub priorities: Vec<String>,
    pub min_confidence: Option<f64>,
    pub detection: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListIssuesRequest {
    #[serde(default)]
    pub states: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveIssueReviewRequest {
    pub candidate_id: String,
    pub expected_revision: String,
    pub associated_issue_id: Option<String>,
    pub decision: String,
    pub rationale: Option<String>,
    pub rejection_reason: Option<String>,
    pub merge_target_id: Option<String>,
    pub merge_target_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateIssueCandidateRequest {
    pub candidate_id: String,
    pub expected_revision: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub priority: String,
    pub suggested_state: String,
    pub acceptance_criteria: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateExplicitIssueRequest {
    pub candidate_id: String,
    pub expected_revision: String,
    pub rationale: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub priority: String,
    pub suggested_state: String,
    pub acceptance_criteria: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReextractRejectedCandidateRequest {
    pub candidate_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TransitionIssueStateRequest {
    pub issue_id: String,
    pub expected_revision: String,
    pub target_state: String,
    pub rationale: Option<String>,
}

pub(super) async fn create_explicit_issue_from_candidate_inner(
    state: State<'_, AppState>,
    req: CreateExplicitIssueRequest,
) -> AppResult<()> {
    let rationale = require_rationale(Some(req.rationale.trim()))?.to_owned();
    let service = state.issue_tracker_service().await?;
    let candidate = service
        .get_candidate(
            "explicit-issue-candidate",
            manager::GetCandidateArgs {
                candidate_id: req.candidate_id.clone(),
                ..Default::default()
            },
        )
        .await
        .map_err(map_client_error)?;
    if !candidate_is_editable(&candidate)
        || candidate.revision != parse_revision(&req.expected_revision)?
    {
        return Err(AppError::IssueTrackerServer(
            "Candidate has changed; refresh before creating an explicit Issue".into(),
        ));
    }
    let issue_id = format!("explicit-from-candidate-{}", candidate.candidate_id);
    let mutation = service
        .create_issue(ManagerWriteRequest {
            correlation_id: format!("explicit-issue-{}", candidate.candidate_id),
            idempotency_key: format!("explicit-issue-{issue_id}"),
            actor_audit: Some(human_actor(Some(rationale))),
            payload: manager::CreateIssueArgs {
                issue_id,
                issue: Some(explicit_issue_draft(&req, &candidate.source_evidence)?),
                provenance_sources: vec![candidate_provenance_source(&candidate)],
                ..Default::default()
            },
        })
        .await
        .map_err(map_client_error)?;
    await_mutation(&service, "explicit-issue-await", mutation).await
}

pub(super) async fn transition_issue_state_inner(
    state: State<'_, AppState>,
    req: TransitionIssueStateRequest,
) -> AppResult<()> {
    let target_state = parse_user_managed_state_target(&req.target_state)?;
    let service = state.issue_tracker_service().await?;
    let issue = service
        .get_issue_with_sync(
            "issue-user-state-read",
            manager::GetIssueWithSyncArgs {
                issue_id: req.issue_id.clone(),
                ..Default::default()
            },
        )
        .await
        .map_err(map_client_error)?
        .issue
        .ok_or_else(|| AppError::IssueTrackerServer("Issue is missing".into()))?;
    let expected_revision = parse_revision(&req.expected_revision)?;
    if issue.revision != expected_revision {
        return Err(AppError::IssueTrackerServer(
            "Issue has changed; refresh before changing its execution state".into(),
        ));
    }
    if !user_state_transition_allowed(issue.state, target_state) {
        return Err(AppError::Config(
            "Only active Issues can be blocked, and only blocked or canceled Issues can be reopened".into(),
        ));
    }
    let rationale = normalized_rationale(req.rationale);
    let is_canceled_reopen =
        issue.state == v1::IssueState::Canceled as i32 && target_state == v1::IssueState::Open;
    if !is_canceled_reopen {
        require_rationale(rationale.as_deref())?;
    }
    let audit_reason = if is_canceled_reopen {
        rationale.map(|value| format!("取消を戻す: {value}"))
    } else {
        let rationale = rationale.expect("active state transitions require a rationale");
        Some(match target_state {
            v1::IssueState::Blocked => format!("自動実行停止: {rationale}"),
            _ => format!("自動実行再開: {rationale}"),
        })
    };
    let mutation = service
        .transition_issue(ManagerWriteRequest {
            correlation_id: format!("issue-user-state-{}", req.issue_id),
            idempotency_key: format!(
                "issue-user-state-{}-{}",
                req.issue_id,
                Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ),
            actor_audit: Some(human_actor(audit_reason)),
            payload: manager::TransitionIssueArgs {
                issue_id: req.issue_id,
                expected_revision: Some(issue.revision),
                target_state: target_state as i32,
                ..Default::default()
            },
        })
        .await
        .map_err(map_client_error)?;
    await_mutation(&service, "issue-user-state-await", mutation).await?;
    Ok(())
}

pub(super) async fn reextract_rejected_candidate_inner(
    app: AppHandle,
    state: State<'_, AppState>,
    req: ReextractRejectedCandidateRequest,
) -> AppResult<crate::commands::analysis_dispatch::EnqueueAnalysisJobResponse> {
    let service = state.issue_tracker_service().await?;
    let candidate = service
        .get_candidate(
            "reextract-rejected-candidate",
            manager::GetCandidateArgs {
                candidate_id: req.candidate_id,
                ..Default::default()
            },
        )
        .await
        .map_err(map_client_error)?;
    if candidate.status != manager::CandidateStatus::Rejected as i32 {
        return Err(AppError::Config(
            "Only rejected candidates can be explicitly re-extracted".into(),
        ));
    }
    let thread_id = candidate_metadata_thread_id(&candidate)?;
    ensure_thread_exists(&state, &thread_id).await?;
    crate::commands::analysis_dispatch::enqueue_targeted_summary_reextract(
        app,
        &state,
        thread_id,
        candidate.candidate_id,
    )
    .await
}

pub(super) async fn update_issue_candidate_inner(
    state: State<'_, AppState>,
    req: UpdateIssueCandidateRequest,
) -> AppResult<()> {
    let service = state.issue_tracker_service().await?;
    let candidate = service
        .get_candidate(
            "issue-candidate-edit-read",
            manager::GetCandidateArgs {
                candidate_id: req.candidate_id.clone(),
                ..Default::default()
            },
        )
        .await
        .map_err(map_client_error)?;
    if !candidate_is_editable(&candidate) {
        return Err(AppError::Config(
            "Only proposed candidates can be edited".into(),
        ));
    }
    let expected_revision = parse_revision(&req.expected_revision)?;
    if candidate.revision != expected_revision {
        return Err(AppError::IssueTrackerServer(
            "Candidate has changed; refresh before editing it".into(),
        ));
    }
    let mutation = service
        .update_candidate(ManagerWriteRequest {
            correlation_id: format!("issue-candidate-edit-{}", req.candidate_id),
            idempotency_key: format!(
                "issue-candidate-edit-{}-{}",
                req.candidate_id,
                Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ),
            actor_audit: Some(human_actor(None)),
            payload: manager::UpdateCandidateArgs {
                candidate_id: req.candidate_id,
                expected_revision: Some(expected_revision),
                update_mask: vec![
                    manager::CandidateField::Title as i32,
                    manager::CandidateField::Description as i32,
                    manager::CandidateField::Kind as i32,
                    manager::CandidateField::SuggestedPriority as i32,
                    manager::CandidateField::SuggestedState as i32,
                    manager::CandidateField::AcceptanceCriteria as i32,
                ],
                patch: Some(manager::CandidatePatch {
                    title: Some(req.title),
                    description: Some(req.description),
                    kind: Some(parse_candidate_kind(&req.kind)? as i32),
                    suggested_priority: Some(parse_candidate_priority(&req.priority)? as i32),
                    suggested_state: Some(
                        parse_candidate_suggested_state(&req.suggested_state)? as i32
                    ),
                    acceptance_criteria: req.acceptance_criteria,
                    ..Default::default()
                }),
                ..Default::default()
            },
        })
        .await
        .map_err(map_client_error)?;
    await_mutation(&service, "issue-candidate-edit-await", mutation).await
}

pub(super) async fn list_issue_review_items_inner(
    state: State<'_, AppState>,
    req: ListIssueReviewRequest,
) -> AppResult<Vec<IssueReviewItem>> {
    let service = state.issue_tracker_service().await?;
    let priorities = if req.priorities.is_empty() {
        match req.priority.as_deref().filter(|value| *value != "all") {
            Some(value) => vec![parse_candidate_priority(value)?],
            None => Vec::new(),
        }
    } else {
        req.priorities
            .iter()
            .map(|value| parse_candidate_priority(value))
            .collect::<AppResult<Vec<_>>>()?
    };
    let rows = list_review_candidates_all(
        &service,
        "issue-review-list",
        manager::ReviewCandidateFilter {
            priorities: priorities
                .into_iter()
                .map(|priority| priority as i32)
                .collect(),
            ..Default::default()
        },
    )
    .await?;
    let mut items = Vec::new();
    for row in rows {
        let item = candidate_row_to_review_item(row)?;
        if review_matches(&item, &req) {
            items.push(item);
        }
    }
    Ok(items)
}

pub(super) async fn list_issues_inner(
    state: State<'_, AppState>,
    req: ListIssuesRequest,
) -> AppResult<Vec<IssueListItem>> {
    let service = state.issue_tracker_service().await?;
    let work_eligibility =
        crate::issue_work_policy::WorkEligibilityRegistry::open(&state.data.root).load();
    let auto_registered_candidates = if matches!(
        work_eligibility,
        crate::issue_work_policy::WorkEligibility::IncludeAutoRegistered
    ) {
        list_candidates_all(
            &service,
            "issue-list-auto-registered-candidates",
            manager::CandidateFilter {
                statuses: vec![manager::CandidateStatus::AutoRegistered as i32],
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .map(|candidate| candidate.candidate_id)
        .collect()
    } else {
        HashSet::new()
    };
    let include_terminal = req
        .states
        .iter()
        .any(|state| matches!(state.as_str(), "DONE" | "CANCELED"));
    let Some(state_filter) = issue_state_filter(&req.states) else {
        return Ok(Vec::new());
    };
    let mut items = Vec::new();
    for row in list_issues_all(&service, "issue-list", state_filter).await? {
        if !issue_is_listed(&row, &auto_registered_candidates, include_terminal)? {
            continue;
        }
        let Some(item) = issue_to_list_item_checked(row)? else {
            continue;
        };
        if req.states.is_empty() || req.states.iter().any(|state| state == &item.state) {
            items.push(item);
        }
    }
    Ok(items)
}

pub(super) async fn get_issue_detail_inner(
    state: State<'_, AppState>,
    issue_id: String,
) -> AppResult<IssueListItem> {
    let service = state.issue_tracker_service().await?;
    let row = service
        .get_issue_with_sync(
            "issue-detail",
            manager::GetIssueWithSyncArgs {
                issue_id,
                ..Default::default()
            },
        )
        .await
        .map_err(map_client_error)?;
    issue_to_list_item_checked(row)?
        .ok_or_else(|| AppError::IssueTrackerServer("Issue is missing".into()))
}
async fn ensure_thread_exists(state: &AppState, thread_id: &str) -> AppResult<()> {
    let thread = thread_id
        .parse::<i64>()
        .map_err(|_| AppError::Config("Invalid source thread_id".into()))?;
    let mut client = ThreadServiceClient::new(state.memories_channel().await?);
    let response = client
        .find(mem_data::ThreadId { value: thread })
        .await
        .map_err(|_| AppError::Config("Rejected Candidate source Thread is unavailable".into()))?;
    if response.into_inner().data.is_none() {
        return Err(AppError::Config(
            "Rejected Candidate source Thread is unavailable".into(),
        ));
    }
    Ok(())
}
pub(super) async fn resolve_issue_review_inner(
    state: State<'_, AppState>,
    req: ResolveIssueReviewRequest,
) -> AppResult<()> {
    let expected_revision = parse_revision(&req.expected_revision)?;
    let service = state.issue_tracker_service().await?;
    let candidate = service
        .get_candidate(
            "issue-review-resolve",
            manager::GetCandidateArgs {
                candidate_id: req.candidate_id.clone(),
                ..Default::default()
            },
        )
        .await
        .map_err(map_client_error)?;
    if candidate.revision != expected_revision {
        return Err(AppError::IssueTrackerServer(
            "Candidate has changed; refresh before reviewing it".into(),
        ));
    }
    let status = manager::CandidateStatus::try_from(candidate.status)
        .map_err(|_| AppError::IssueTrackerServer("Candidate has an unknown status".into()))?;
    let associated_issue = match status {
        manager::CandidateStatus::AutoRegistered => Some(
            load_auto_registered_issue(&service, &candidate, req.associated_issue_id.as_deref())
                .await?,
        ),
        manager::CandidateStatus::Proposed => {
            if req.associated_issue_id.is_some() {
                return Err(AppError::Config(
                    "PROPOSED candidates must not include an associated Issue ID".into(),
                ));
            }
            None
        }
        _ => {
            return Err(AppError::Config(
                "Only proposed or auto-registered candidates can be reviewed".into(),
            ));
        }
    };
    let rationale = normalized_rationale(req.rationale);
    let review_policy =
        crate::issue_review_policy::IssueReviewPolicyRegistry::open(&state.data.root).load();
    if high_priority_reason_required(&candidate, &req.decision, &review_policy) {
        let _ = require_rationale(rationale.as_deref())?;
    }
    let decision = build_resolution(
        &service,
        &candidate,
        &state,
        associated_issue.as_ref(),
        ReviewDecision {
            kind: &req.decision,
            rationale: rationale.as_deref(),
            rejection_reason: req.rejection_reason.as_deref(),
            merge_target_id: req.merge_target_id.as_deref(),
            merge_target_type: req.merge_target_type.as_deref(),
        },
    )
    .await?;
    let receipt = service
        .resolve_candidate(ManagerWriteRequest {
            correlation_id: format!("issue-review-{}", candidate.candidate_id),
            idempotency_key: format!(
                "issue-review-{}-{}",
                candidate.candidate_id,
                Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ),
            actor_audit: Some(manager::ActorAudit {
                actor_id: "lookback-user".into(),
                actor_kind: v1::ActorKind::Human as i32,
                display_name: Some("Lookback user".into()),
                reason: rationale,
                ..Default::default()
            }),
            payload: manager::ResolveCandidateArgs {
                candidate_id: candidate.candidate_id,
                expected_revision: Some(expected_revision),
                decision: Some(decision),
                ..Default::default()
            },
        })
        .await
        .map_err(map_client_error)?;
    service
        .await_operation("issue-review-await", receipt.operation_id, RESOLUTION_WAIT)
        .await
        .map_err(map_client_error)?;
    Ok(())
}

struct ReviewDecision<'a> {
    kind: &'a str,
    rationale: Option<&'a str>,
    rejection_reason: Option<&'a str>,
    merge_target_id: Option<&'a str>,
    merge_target_type: Option<&'a str>,
}

async fn load_auto_registered_issue(
    service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
    candidate: &manager::Candidate,
    associated_issue_id: Option<&str>,
) -> AppResult<v1::Issue> {
    let requested_issue_id = associated_issue_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::Config("AUTO_REGISTERED resolution requires an associated Issue ID".into())
        })?;
    let row = service
        .get_issue_with_sync(
            "issue-review-associated-issue",
            manager::GetIssueWithSyncArgs {
                issue_id: requested_issue_id.to_owned(),
                ..Default::default()
            },
        )
        .await
        .map_err(map_client_error)?;
    validate_auto_registered_issue(row, candidate, requested_issue_id)
}

fn validate_auto_registered_issue(
    row: manager::IssueWithSync,
    candidate: &manager::Candidate,
    requested_issue_id: &str,
) -> AppResult<v1::Issue> {
    let mapping = row.mapping.as_ref().ok_or_else(|| {
        AppError::IssueTrackerServer(
            "AUTO_REGISTERED candidate has no associated Issue mapping".into(),
        )
    })?;
    if mapping.candidate_id.as_deref() != Some(candidate.candidate_id.as_str()) {
        return Err(AppError::IssueTrackerServer(
            "Issue mapping Candidate ID does not match the reviewed Candidate".into(),
        ));
    }
    if mapping.issue_id != requested_issue_id {
        return Err(AppError::IssueTrackerServer(
            "Issue mapping ID does not match the requested associated Issue".into(),
        ));
    }
    let issue = row
        .issue
        .ok_or_else(|| AppError::IssueTrackerServer("Associated Issue is missing".into()))?;
    if issue.issue_id != requested_issue_id || issue.issue_id != mapping.issue_id {
        return Err(AppError::IssueTrackerServer(
            "Canonical Issue ID does not match the mapping or request".into(),
        ));
    }
    let state = v1::IssueState::try_from(issue.state).map_err(|_| {
        AppError::IssueTrackerServer("Associated Issue has an unknown lifecycle state".into())
    })?;
    if !matches!(
        state,
        v1::IssueState::Open
            | v1::IssueState::InProgress
            | v1::IssueState::Blocked
            | v1::IssueState::Deferred
    ) {
        return Err(AppError::IssueTrackerServer(
            "AUTO_REGISTERED resolution requires a non-terminal associated Issue".into(),
        ));
    }
    Ok(issue)
}

async fn build_resolution(
    service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
    candidate: &manager::Candidate,
    state: &AppState,
    associated_issue: Option<&v1::Issue>,
    review: ReviewDecision<'_>,
) -> AppResult<manager::resolve_candidate_args::Decision> {
    match review.kind {
        "reject" => {
            let reason = review_rejection_reason(review.rejection_reason)?;
            let rationale = rationale_for_rejection(reason, review.rationale)?;
            Ok(manager::resolve_candidate_args::Decision::Reject(
                manager::RejectResolution {
                    reason_category: reason.category as i32,
                    rationale: rationale.map(str::to_owned),
                },
            ))
        }
        "false_positive" | "cancel" => {
            let reason = review_rejection_reason(review.rejection_reason)?;
            let is_false_positive = matches!(reason.kind, ReviewRejectionKind::FalsePositive);
            if (review.kind == "false_positive") != is_false_positive {
                return Err(AppError::Config(
                    "Issue cancellation decision does not match its rejection reason".into(),
                ));
            }
            let Some(issue) = associated_issue else {
                return Err(AppError::Config(
                    "Canceling a candidate requires an associated Issue".into(),
                ));
            };
            let issue_id = issue.issue_id.clone();
            let issue_revision = issue.revision;
            let rationale = rationale_for_rejection(reason, review.rationale)?;
            Ok(
                manager::resolve_candidate_args::Decision::CancelAssociatedIssue(
                    manager::CancelAssociatedIssueResolution {
                        issue_id: issue_id.clone(),
                        expected_issue_revision: Some(issue_revision),
                        expected_mapping_candidate_id: candidate.candidate_id.clone(),
                        expected_mapping_issue_id: issue_id,
                        cancellation_report: Some(v1::CancellationReport {
                            classification: reason.cancellation_class as i32,
                        }),
                        reason_category: reason.category as i32,
                        rationale: rationale.map(str::to_owned),
                    },
                ),
            )
        }
        "confirm" => {
            if !all_source_evidence_available(state, &candidate.source_evidence).await {
                return Err(AppError::IssueTrackerServer(
                    "Source material is unavailable; use human review with a reason instead".into(),
                ));
            }
            let basis = manager::ResolutionBasis::SourceEvidence as i32;
            if let Some(issue) = associated_issue {
                Ok(confirm_existing_issue_resolution(issue, candidate, basis))
            } else {
                Ok(manager::resolve_candidate_args::Decision::CreateIssue(
                    manager::CreateIssueResolution {
                        issue: Some(issue_draft(
                            candidate,
                            v1::ConfirmationStatus::Confirmed,
                            true,
                        )),
                        basis,
                    },
                ))
            }
        }
        "human_review" => {
            let _ = require_rationale(review.rationale)?;
            Ok(match associated_issue {
                Some(issue) => confirm_existing_issue_resolution(
                    issue,
                    candidate,
                    manager::ResolutionBasis::HumanReview as i32,
                ),
                None => manager::resolve_candidate_args::Decision::CreateIssue(
                    manager::CreateIssueResolution {
                        issue: Some(issue_draft(
                            candidate,
                            v1::ConfirmationStatus::Confirmed,
                            false,
                        )),
                        basis: manager::ResolutionBasis::HumanReview as i32,
                    },
                ),
            })
        }
        "merge" | "human_merge" => {
            let human = review.kind == "human_merge";
            if human {
                let _ = require_rationale(review.rationale)?;
            } else if !all_source_evidence_available(state, &candidate.source_evidence).await {
                return Err(AppError::IssueTrackerServer(
                    "Source material is unavailable; use human merge with a reason instead".into(),
                ));
            }
            let target_id = review
                .merge_target_id
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| AppError::Config("Merge requires a target".into()))?;
            let target = match review.merge_target_type {
                Some("candidate") => {
                    let target = service
                        .get_candidate(
                            "issue-review-merge-target",
                            manager::GetCandidateArgs {
                                candidate_id: target_id.into(),
                                ..Default::default()
                            },
                        )
                        .await
                        .map_err(map_client_error)?;
                    manager::merge_resolution::Target::Candidate(manager::CandidateMergeTarget {
                        candidate_id: target.candidate_id,
                        expected_revision: Some(target.revision),
                    })
                }
                Some("issue") => {
                    let target = service
                        .get_issue_with_sync(
                            "issue-review-merge-target",
                            manager::GetIssueWithSyncArgs {
                                issue_id: target_id.into(),
                                ..Default::default()
                            },
                        )
                        .await
                        .map_err(map_client_error)?
                        .issue
                        .ok_or_else(|| {
                            AppError::IssueTrackerServer("Merge target Issue is missing".into())
                        })?;
                    manager::merge_resolution::Target::Issue(manager::IssueMergeTarget {
                        issue_id: target.issue_id,
                        expected_revision: Some(target.revision),
                        ..Default::default()
                    })
                }
                _ => {
                    return Err(AppError::Config(
                        "Merge target type must be candidate or issue".into(),
                    ));
                }
            };
            Ok(manager::resolve_candidate_args::Decision::Merge(
                manager::MergeResolution {
                    target: Some(target),
                    basis: if human {
                        manager::ResolutionBasis::HumanReview as i32
                    } else {
                        manager::ResolutionBasis::SourceEvidence as i32
                    },
                    ..Default::default()
                },
            ))
        }
        _ => Err(AppError::Config("Unknown issue review decision".into())),
    }
}

pub(super) async fn await_mutation(
    service: &crate::grpc::issue_tracker::IssueTrackerGrpcService,
    correlation_id: &str,
    mutation: manager::ManagerMutation,
) -> AppResult<()> {
    let operation_id = mutation
        .operation
        .map(|operation| operation.operation_id)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| AppError::IssueTrackerServer("Mutation has no operation ID".into()))?;
    service
        .await_operation(correlation_id, operation_id, RESOLUTION_WAIT)
        .await
        .map_err(map_client_error)?;
    Ok(())
}

fn issue_draft(
    candidate: &manager::Candidate,
    confirmation: v1::ConfirmationStatus,
    source_evidence: bool,
) -> v1::IssueDraft {
    v1::IssueDraft {
        title: candidate.title.clone(),
        description: candidate.description.clone(),
        kind: candidate_kind_to_issue_kind(candidate.kind) as i32,
        priority: candidate_priority_to_issue_priority(candidate.suggested_priority) as i32,
        acceptance_criteria: candidate.acceptance_criteria.clone(),
        creation_basis: v1::CreationBasis::Explicit as i32,
        confirmation_status: confirmation as i32,
        initial_state: candidate_suggested_state_to_issue_state(candidate.suggested_state) as i32,
        source_evidence: if source_evidence {
            candidate.source_evidence.clone()
        } else {
            Vec::new()
        },
        ..Default::default()
    }
}
fn candidate_provenance_source(candidate: &manager::Candidate) -> manager::ProvenanceSourceRef {
    provenance_source(
        manager::provenance_source_ref::Source::CandidateId(candidate.candidate_id.clone()),
        candidate.revision,
    )
}
pub(super) fn issue_provenance_source(issue: &v1::Issue) -> manager::ProvenanceSourceRef {
    provenance_source(
        manager::provenance_source_ref::Source::IssueId(issue.issue_id.clone()),
        issue.revision,
    )
}

fn provenance_source(
    source: manager::provenance_source_ref::Source,
    expected_revision: u64,
) -> manager::ProvenanceSourceRef {
    manager::ProvenanceSourceRef {
        source: Some(source),
        expected_revision: Some(expected_revision),
    }
}
pub(super) fn explicit_issue_draft_from_fields(
    title: &str,
    description: &str,
    kind: &str,
    priority: &str,
    suggested_state: &str,
    acceptance_criteria: &[String],
) -> AppResult<v1::IssueDraft> {
    Ok(v1::IssueDraft {
        title: title.trim().into(),
        description: description.trim().into(),
        kind: candidate_kind_to_issue_kind(parse_candidate_kind(kind)? as i32) as i32,
        priority: candidate_priority_to_issue_priority(parse_candidate_priority(priority)? as i32)
            as i32,
        acceptance_criteria: acceptance_criteria.into(),
        creation_basis: v1::CreationBasis::Explicit as i32,
        confirmation_status: v1::ConfirmationStatus::Confirmed as i32,
        initial_state: candidate_suggested_state_to_issue_state(parse_candidate_suggested_state(
            suggested_state,
        )? as i32) as i32,
        ..Default::default()
    })
}

fn confirm_existing_issue_resolution(
    issue: &v1::Issue,
    candidate: &manager::Candidate,
    basis: i32,
) -> manager::resolve_candidate_args::Decision {
    manager::resolve_candidate_args::Decision::ConfirmAssociatedIssue(
        manager::ConfirmAssociatedIssueResolution {
            issue_id: issue.issue_id.clone(),
            expected_revision: Some(issue.revision),
            update_mask: vec![manager::ManagerIssueField::ConfirmationStatus as i32],
            patch: Some(manager::ManagerIssuePatch {
                confirmation_status: Some(v1::ConfirmationStatus::Confirmed as i32),
                ..Default::default()
            }),
            basis,
            target_state: Some(
                candidate_suggested_state_to_issue_state(candidate.suggested_state) as i32,
            ),
            ..Default::default()
        },
    )
}
fn user_state_transition_allowed(source: i32, target: v1::IssueState) -> bool {
    matches!(
        (v1::IssueState::try_from(source).ok(), target),
        (Some(v1::IssueState::Blocked), v1::IssueState::Open)
            | (Some(v1::IssueState::Canceled), v1::IssueState::Open)
            | (
                Some(v1::IssueState::Open | v1::IssueState::InProgress | v1::IssueState::Deferred),
                v1::IssueState::Blocked
            )
    )
}

fn parse_user_managed_state_target(value: &str) -> AppResult<v1::IssueState> {
    match value {
        "BLOCKED" => Ok(v1::IssueState::Blocked),
        "OPEN" => Ok(v1::IssueState::Open),
        _ => Err(AppError::Config(
            "Only BLOCKED and OPEN are user-managed here".into(),
        )),
    }
}

fn issue_is_listed(
    row: &manager::IssueWithSync,
    auto_registered_candidates: &HashSet<String>,
    include_terminal: bool,
) -> AppResult<bool> {
    let Some(issue) = row.issue.as_ref() else {
        return Ok(false);
    };
    let state = v1::IssueState::try_from(issue.state).map_err(|_| {
        AppError::IssueTrackerServer("Issue list row has an unknown lifecycle state".into())
    })?;
    if state == v1::IssueState::Unspecified {
        return Err(AppError::IssueTrackerServer(
            "Issue list row has an unspecified lifecycle state".into(),
        ));
    }
    if matches!(state, v1::IssueState::Done | v1::IssueState::Canceled) {
        // Terminal state filters are an Issue history view. A Candidate can
        // become REJECTED after cancelling its associated auto-registered
        // Issue, so work eligibility must not hide that canonical Issue.
        return Ok(include_terminal);
    }
    if !matches!(
        state,
        v1::IssueState::Open
            | v1::IssueState::InProgress
            | v1::IssueState::Blocked
            | v1::IssueState::Deferred
            | v1::IssueState::AwaitingReview
    ) {
        return Ok(false);
    }
    if issue.confirmation_status == v1::ConfirmationStatus::Confirmed as i32 {
        return Ok(true);
    }
    Ok(row
        .mapping
        .as_ref()
        .and_then(|mapping| mapping.candidate_id.as_ref())
        .is_some_and(|candidate_id| auto_registered_candidates.contains(candidate_id))
        && issue.confirmation_status == v1::ConfirmationStatus::Unreviewed as i32
        && !issue.source_evidence.is_empty())
}

fn candidate_row_to_review_item(row: manager::ReviewCandidateRow) -> AppResult<IssueReviewItem> {
    let candidate = row.candidate.ok_or_else(|| {
        AppError::IssueTrackerServer("Issue Tracker review row has no Candidate".into())
    })?;
    if candidate.candidate_id.trim().is_empty() {
        return Err(AppError::IssueTrackerServer(
            "Issue Tracker review row has an empty Candidate ID".into(),
        ));
    }
    let status = manager::CandidateStatus::try_from(candidate.status).map_err(|_| {
        AppError::IssueTrackerServer(
            "Issue Tracker review row has an unknown Candidate status".into(),
        )
    })?;
    if !matches!(
        status,
        manager::CandidateStatus::Proposed | manager::CandidateStatus::AutoRegistered
    ) {
        return Err(AppError::IssueTrackerServer(
            "Issue Tracker review row contains a non-reviewable Candidate status".into(),
        ));
    }
    let kind = manager::CandidateKind::try_from(candidate.kind).map_err(|_| {
        AppError::IssueTrackerServer(
            "Issue Tracker review row has an unknown Candidate kind".into(),
        )
    })?;
    if kind == manager::CandidateKind::Unspecified {
        return Err(AppError::IssueTrackerServer(
            "Issue Tracker review row has an unspecified Candidate kind".into(),
        ));
    }
    let priority =
        manager::CandidatePriority::try_from(candidate.suggested_priority).map_err(|_| {
            AppError::IssueTrackerServer(
                "Issue Tracker review row has an unknown Candidate priority".into(),
            )
        })?;
    if priority == manager::CandidatePriority::Unspecified {
        return Err(AppError::IssueTrackerServer(
            "Issue Tracker review row has an unspecified Candidate priority".into(),
        ));
    }
    let suggested_state = manager::CandidateSuggestedState::try_from(candidate.suggested_state)
        .map_err(|_| {
            AppError::IssueTrackerServer(
                "Issue Tracker review row has an unknown Candidate suggested state".into(),
            )
        })?;
    if suggested_state == manager::CandidateSuggestedState::Unspecified {
        return Err(AppError::IssueTrackerServer(
            "Issue Tracker review row has an unspecified Candidate suggested state".into(),
        ));
    }
    let detection = manager::DetectionBasis::try_from(candidate.detection_basis).map_err(|_| {
        AppError::IssueTrackerServer(
            "Issue Tracker review row has an unknown detection basis".into(),
        )
    })?;
    if detection == manager::DetectionBasis::Unspecified {
        return Err(AppError::IssueTrackerServer(
            "Issue Tracker review row has an unspecified detection basis".into(),
        ));
    }
    let integrity = manager::ReviewCandidateIntegrity::try_from(row.integrity).map_err(|_| {
        AppError::IssueTrackerServer("Issue Tracker review row has unknown integrity".into())
    })?;
    let resolution_state = manager::ReviewCandidateResolutionState::try_from(row.resolution_state)
        .map_err(|_| {
            AppError::IssueTrackerServer(
                "Issue Tracker review row has unknown resolution state".into(),
            )
        })?;
    if integrity == manager::ReviewCandidateIntegrity::Unspecified
        || resolution_state == manager::ReviewCandidateResolutionState::Unspecified
    {
        return Err(AppError::IssueTrackerServer(
            "Issue Tracker review row has unspecified integrity or resolution state".into(),
        ));
    }
    let associated_issue_id = row
        .associated_issue
        .as_ref()
        .map(|issue| issue.issue_id.clone());
    if associated_issue_id
        .as_deref()
        .is_some_and(|issue_id| issue_id.trim().is_empty())
    {
        return Err(AppError::IssueTrackerServer(
            "Issue Tracker review row has an empty associated Issue ID".into(),
        ));
    }
    let issue_state = row
        .associated_issue
        .as_ref()
        .map(|issue| {
            v1::IssueState::try_from(issue.lifecycle_state)
                .map_err(|_| {
                    AppError::IssueTrackerServer(
                        "Issue Tracker review row has an unknown associated Issue state".into(),
                    )
                })
                .and_then(|state| {
                    if state == v1::IssueState::Unspecified {
                        Err(AppError::IssueTrackerServer(
                            "Issue Tracker review row has an unspecified associated Issue state"
                                .into(),
                        ))
                    } else {
                        Ok(issue_state(state as i32)?.into())
                    }
                })
        })
        .transpose()?;
    if status == manager::CandidateStatus::AutoRegistered
        && integrity == manager::ReviewCandidateIntegrity::Normal
        && (row.mapping.is_none() || associated_issue_id.is_none())
    {
        return Err(AppError::IssueTrackerServer(
            "Issue Tracker review row is missing its AUTO_REGISTERED mapping or Issue".into(),
        ));
    }
    let confidence = candidate_confidence(&candidate);
    let source_labels = source_labels_from_candidate(&candidate);
    Ok(IssueReviewItem {
        id: candidate.candidate_id,
        revision: candidate.revision.to_string(),
        associated_issue_id: associated_issue_id.clone(),
        issue_id: associated_issue_id,
        title: candidate.title,
        description: candidate.description,
        kind: kind
            .as_str_name()
            .strip_prefix("CANDIDATE_KIND_")
            .unwrap_or("TASK")
            .into(),
        priority: priority
            .as_str_name()
            .strip_prefix("CANDIDATE_PRIORITY_")
            .unwrap_or("P2")
            .into(),
        suggested_state: match suggested_state {
            manager::CandidateSuggestedState::Open => "OPEN",
            manager::CandidateSuggestedState::InProgress => "IN_PROGRESS",
            manager::CandidateSuggestedState::Blocked => "BLOCKED",
            manager::CandidateSuggestedState::Deferred => "DEFERRED",
            manager::CandidateSuggestedState::Unspecified => unreachable!(),
        }
        .into(),
        acceptance_criteria: candidate.acceptance_criteria,
        confidence,
        review_state: if status == manager::CandidateStatus::AutoRegistered {
            "unreviewed"
        } else {
            "candidate"
        }
        .into(),
        detection: if detection == manager::DetectionBasis::Explicit {
            "explicit"
        } else {
            "inferred"
        }
        .into(),
        strong_proof: candidate.strong_detection_proof.is_some(),
        evidence_count: candidate.source_evidence.len(),
        duplicate_count: candidate.duplicate_candidate_ids.len(),
        issue_state,
        integrity: match integrity {
            manager::ReviewCandidateIntegrity::Normal => "normal",
            manager::ReviewCandidateIntegrity::MappingMissing => "mapping_missing",
            manager::ReviewCandidateIntegrity::MappingInvalid => "mapping_invalid",
            manager::ReviewCandidateIntegrity::IssueNotFound => "issue_not_found",
            manager::ReviewCandidateIntegrity::UnknownIssueState => "unknown_issue_state",
            manager::ReviewCandidateIntegrity::Unspecified => unreachable!(),
        }
        .into(),
        resolution_state: match resolution_state {
            manager::ReviewCandidateResolutionState::Actionable => "actionable",
            manager::ReviewCandidateResolutionState::ResolutionPending => "resolution_pending",
            manager::ReviewCandidateResolutionState::Unspecified => unreachable!(),
        }
        .into(),
        source_labels,
    })
}

pub(super) fn issue_to_list_item(row: manager::IssueWithSync) -> Option<IssueListItem> {
    issue_to_list_item_checked(row).ok().flatten()
}

fn issue_to_list_item_checked(row: manager::IssueWithSync) -> AppResult<Option<IssueListItem>> {
    let Some(issue) = row.issue else {
        return Ok(None);
    };
    let state = issue_state(issue.state)?;
    let contexts = issue_contexts(issue.contexts.clone())?;
    let acceptance_criteria_items = issue.acceptance_criteria.clone();
    let source_labels = source_labels_from_issue(&issue);
    let proof = row
        .mapping
        .as_ref()
        .and_then(|mapping| mapping.candidate_id.as_ref())
        .map(|_| "Candidate evidence".into());
    Ok(Some(IssueListItem {
        id: issue.issue_id,
        revision: issue.revision.to_string(),
        title: issue.title,
        description: issue.description,
        kind: issue_kind(issue.kind).into(),
        priority: issue_priority(issue.priority).into(),
        state: state.into(),
        review_state: if issue.confirmation_status == v1::ConfirmationStatus::Confirmed as i32 {
            "confirmed"
        } else {
            "unreviewed"
        }
        .into(),
        created_at_ms: timestamp_ms_issue(issue.created_at.as_ref()),
        updated_at_ms: timestamp_ms_issue(issue.updated_at.as_ref()),
        completion_criteria: (!issue.acceptance_criteria.is_empty())
            .then(|| issue.acceptance_criteria.join("\n")),
        acceptance_criteria_items,
        registration_path: Some(
            if row.mapping.is_some() {
                "auto_registered"
            } else {
                "manual"
            }
            .into(),
        ),
        policy: None,
        proof,
        source_labels,
        contexts,
        claim: issue.claim.as_ref().map(claim_to_view),
    }))
}

fn issue_contexts(contexts: Vec<v1::ContextRef>) -> AppResult<Vec<IssueContext>> {
    contexts
        .into_iter()
        .map(|context| {
            let kind = v1::ContextKind::try_from(context.kind).map_err(|_| {
                AppError::IssueTrackerServer("Issue has an unknown context kind".into())
            })?;
            let kind = match kind {
                v1::ContextKind::Project => IssueContextKind::Project,
                v1::ContextKind::Repository => IssueContextKind::Repository,
                v1::ContextKind::Other => IssueContextKind::Other,
                v1::ContextKind::Unspecified => {
                    return Err(AppError::IssueTrackerServer(
                        "Issue has an unspecified context kind".into(),
                    ));
                }
            };
            Ok(IssueContext {
                kind,
                value: context.value,
                uri: context.uri,
            })
        })
        .collect()
}

fn claim_to_view(claim: &v1::ClaimView) -> IssueClaimView {
    let lease_expires_at_ms = claim
        .lease_expires_at
        .as_ref()
        .map(|value| value.unix_epoch_millis);
    let now = Utc::now().timestamp_millis();
    let lease_status = match lease_expires_at_ms {
        Some(expires) if expires > now => "lease_present".to_owned(),
        Some(_) => "lease_expired_possible".to_owned(),
        None => "lease_without_expiry".to_owned(),
    };
    IssueClaimView {
        lease_expires_at_ms,
        claim_revision: claim.claim_revision.to_string(),
        lease_status,
    }
}
// Mirrored by `reviewMatches` in src/pages/issues/viewModel.ts, which filters
// the unfiltered review query locally; keep the two predicates identical.
fn review_matches(item: &IssueReviewItem, req: &ListIssueReviewRequest) -> bool {
    req.target
        .as_deref()
        .is_none_or(|value| value == "all" || value == item.review_state)
        && (if req.priorities.is_empty() {
            req.priority
                .as_deref()
                .is_none_or(|value| value == "all" || value == item.priority)
        } else {
            req.priorities.iter().any(|value| value == &item.priority)
        })
        && req
            .min_confidence
            .is_none_or(|minimum| item.confidence >= minimum)
        && req
            .detection
            .as_deref()
            .is_none_or(|value| value == "all" || value == item.detection)
}
fn normalized_rationale(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_owned();
        (!value.is_empty()).then_some(value)
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewRejectionKind {
    FalsePositive,
    Completed,
    Other,
}

#[derive(Clone, Copy, Debug)]
struct ReviewRejectionReason {
    kind: ReviewRejectionKind,
    category: manager::ResolutionReasonCategory,
    cancellation_class: v1::CancellationClass,
}

fn review_rejection_reason(value: Option<&str>) -> AppResult<ReviewRejectionReason> {
    match value {
        Some("false_positive") => Ok(ReviewRejectionReason {
            kind: ReviewRejectionKind::FalsePositive,
            category: manager::ResolutionReasonCategory::NotActionable,
            cancellation_class: v1::CancellationClass::FalsePositive,
        }),
        Some("completed") => Ok(ReviewRejectionReason {
            kind: ReviewRejectionKind::Completed,
            category: manager::ResolutionReasonCategory::NotActionable,
            cancellation_class: v1::CancellationClass::Other,
        }),
        Some("other") => Ok(ReviewRejectionReason {
            kind: ReviewRejectionKind::Other,
            category: manager::ResolutionReasonCategory::Other,
            cancellation_class: v1::CancellationClass::Other,
        }),
        _ => Err(AppError::Config(
            "Rejection reason must be false_positive, completed, or other".into(),
        )),
    }
}

fn rationale_for_rejection(
    reason: ReviewRejectionReason,
    rationale: Option<&str>,
) -> AppResult<Option<&str>> {
    if reason.kind == ReviewRejectionKind::Other {
        return require_rationale(rationale).map(Some);
    }
    Ok(rationale.filter(|value| !value.trim().is_empty()))
}

fn high_priority_reason_required(
    candidate: &manager::Candidate,
    decision: &str,
    policy: &crate::issue_review_policy::IssueReviewPolicy,
) -> bool {
    policy.require_high_priority_reason
        && decision == "confirm"
        && matches!(
            manager::CandidatePriority::try_from(candidate.suggested_priority).ok(),
            Some(manager::CandidatePriority::P0 | manager::CandidatePriority::P1)
        )
}
fn candidate_is_editable(candidate: &manager::Candidate) -> bool {
    candidate.status == manager::CandidateStatus::Proposed as i32
}
fn timestamp_ms_issue(value: Option<&v1::UtcInstant>) -> i64 {
    value.map_or(0, |instant| instant.unix_epoch_millis)
}

fn candidate_confidence(candidate: &manager::Candidate) -> f64 {
    candidate
        .strong_detection_proof
        .as_ref()
        .map(|proof| proof.confidence)
        .or_else(|| {
            candidate
                .confidence_factors
                .first()
                .map(|factor| factor.score)
        })
        .unwrap_or(0.0)
}

fn candidate_metadata_thread_id(candidate: &manager::Candidate) -> AppResult<String> {
    let values = candidate
        .metadata
        .iter()
        .filter(|entry| entry.key == "thread_id")
        .map(|entry| entry.value.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if values.len() != 1 || values[0].parse::<i64>().is_err() {
        return Err(AppError::Config(
            "Rejected Candidate has no single valid source thread_id".into(),
        ));
    }
    Ok(values[0].to_owned())
}

fn candidate_suggested_state_to_issue_state(value: i32) -> v1::IssueState {
    match manager::CandidateSuggestedState::try_from(value).ok() {
        Some(manager::CandidateSuggestedState::InProgress) => v1::IssueState::InProgress,
        Some(manager::CandidateSuggestedState::Blocked) => v1::IssueState::Blocked,
        Some(manager::CandidateSuggestedState::Deferred) => v1::IssueState::Deferred,
        _ => v1::IssueState::Open,
    }
}

fn parse_candidate_kind(value: &str) -> AppResult<manager::CandidateKind> {
    match value {
        "TASK" => Ok(manager::CandidateKind::Task),
        "BUG" => Ok(manager::CandidateKind::Bug),
        "RESEARCH" => Ok(manager::CandidateKind::Research),
        "DECISION" => Ok(manager::CandidateKind::Decision),
        _ => Err(AppError::Config("Unknown Candidate kind".into())),
    }
}

fn parse_candidate_priority(value: &str) -> AppResult<manager::CandidatePriority> {
    match value {
        "P0" => Ok(manager::CandidatePriority::P0),
        "P1" => Ok(manager::CandidatePriority::P1),
        "P2" => Ok(manager::CandidatePriority::P2),
        "P3" => Ok(manager::CandidatePriority::P3),
        "P4" => Ok(manager::CandidatePriority::P4),
        _ => Err(AppError::Config("Unknown Candidate priority".into())),
    }
}

fn parse_candidate_suggested_state(value: &str) -> AppResult<manager::CandidateSuggestedState> {
    match value {
        "OPEN" => Ok(manager::CandidateSuggestedState::Open),
        "IN_PROGRESS" => Ok(manager::CandidateSuggestedState::InProgress),
        "BLOCKED" => Ok(manager::CandidateSuggestedState::Blocked),
        "DEFERRED" => Ok(manager::CandidateSuggestedState::Deferred),
        _ => Err(AppError::Config("Unknown Candidate suggested state".into())),
    }
}
fn issue_priority(value: i32) -> &'static str {
    match v1::IssuePriority::try_from(value).ok() {
        Some(v1::IssuePriority::P0) => "P0",
        Some(v1::IssuePriority::P1) => "P1",
        Some(v1::IssuePriority::P2) => "P2",
        Some(v1::IssuePriority::P3) => "P3",
        Some(v1::IssuePriority::P4) => "P4",
        _ => "P2",
    }
}
pub(super) fn issue_state(value: i32) -> AppResult<&'static str> {
    match v1::IssueState::try_from(value)
        .map_err(|_| AppError::IssueTrackerServer("Issue has an unknown lifecycle state".into()))?
    {
        v1::IssueState::Open => Ok("OPEN"),
        v1::IssueState::InProgress => Ok("IN_PROGRESS"),
        v1::IssueState::Blocked => Ok("BLOCKED"),
        v1::IssueState::Deferred => Ok("DEFERRED"),
        v1::IssueState::Done => Ok("DONE"),
        v1::IssueState::Canceled => Ok("CANCELED"),
        v1::IssueState::AwaitingReview => Ok("AWAITING_REVIEW"),
        v1::IssueState::Unspecified => Err(AppError::IssueTrackerServer(
            "Issue has an unspecified lifecycle state".into(),
        )),
    }
}
fn issue_kind(value: i32) -> &'static str {
    match v1::IssueKind::try_from(value).ok() {
        Some(v1::IssueKind::Task) => "TASK",
        Some(v1::IssueKind::Bug) => "BUG",
        Some(v1::IssueKind::Research) => "RESEARCH",
        Some(v1::IssueKind::Decision) => "DECISION",
        _ => "TASK",
    }
}

fn candidate_kind_to_issue_kind(value: i32) -> v1::IssueKind {
    match manager::CandidateKind::try_from(value).ok() {
        Some(manager::CandidateKind::Bug) => v1::IssueKind::Bug,
        Some(manager::CandidateKind::Research) => v1::IssueKind::Research,
        Some(manager::CandidateKind::Decision) => v1::IssueKind::Decision,
        _ => v1::IssueKind::Task,
    }
}

fn candidate_priority_to_issue_priority(value: i32) -> v1::IssuePriority {
    match manager::CandidatePriority::try_from(value).ok() {
        Some(manager::CandidatePriority::P0) => v1::IssuePriority::P0,
        Some(manager::CandidatePriority::P1) => v1::IssuePriority::P1,
        Some(manager::CandidatePriority::P3) => v1::IssuePriority::P3,
        Some(manager::CandidatePriority::P4) => v1::IssuePriority::P4,
        _ => v1::IssuePriority::P2,
    }
}

fn explicit_issue_draft(
    request: &CreateExplicitIssueRequest,
    source_evidence: &[v1::SourceEvidenceRef],
) -> AppResult<v1::IssueDraft> {
    explicit_issue_draft_from_fields(
        &request.title,
        &request.description,
        &request.kind,
        &request.priority,
        &request.suggested_state,
        &request.acceptance_criteria,
    )
    .map(|mut draft| {
        draft.source_evidence = source_evidence.to_vec();
        draft
    })
}

#[cfg(test)]
mod source_reference_tests {
    use super::*;

    #[test]
    fn rejected_reextract_requires_one_numeric_origin_thread() {
        let valid = manager::Candidate {
            metadata: vec![manager::MetadataEntry {
                key: "thread_id".into(),
                value: "42".into(),
            }],
            ..Default::default()
        };
        assert_eq!(candidate_metadata_thread_id(&valid).unwrap(), "42");
        let missing = manager::Candidate::default();
        assert!(candidate_metadata_thread_id(&missing).is_err());
        let duplicate = manager::Candidate {
            metadata: vec![
                manager::MetadataEntry {
                    key: "thread_id".into(),
                    value: "1".into(),
                },
                manager::MetadataEntry {
                    key: "thread_id".into(),
                    value: "2".into(),
                },
            ],
            ..Default::default()
        };
        assert!(candidate_metadata_thread_id(&duplicate).is_err());
    }
}

#[cfg(test)]
mod canonical_acceptance_criteria_tests {
    use super::*;

    #[test]
    fn issue_dto_preserves_canonical_acceptance_vector_beside_legacy_text() {
        let canonical = vec!["first line\nsecond line".into(), "follow-up".into()];
        let row = manager::IssueWithSync {
            issue: Some(v1::Issue {
                issue_id: "issue-42".into(),
                revision: 7,
                state: v1::IssueState::Open as i32,
                acceptance_criteria: canonical.clone(),
                ..Default::default()
            }),
            ..Default::default()
        };

        let item = issue_to_list_item_checked(row).unwrap().unwrap();

        assert_eq!(item.acceptance_criteria_items, canonical);
        assert_eq!(
            item.completion_criteria.as_deref(),
            Some("first line\nsecond line\nfollow-up"),
            "the legacy flattened field remains unchanged for existing screens"
        );
        let encoded = serde_json::to_value(item).unwrap();
        assert_eq!(
            encoded["acceptanceCriteriaItems"],
            serde_json::json!(["first line\nsecond line", "follow-up"])
        );
    }
}

#[cfg(test)]
mod review_candidate_tests {
    use super::*;

    fn candidate() -> manager::Candidate {
        manager::Candidate {
            candidate_id: "candidate-1".into(),
            revision: 3,
            status: manager::CandidateStatus::Proposed as i32,
            kind: manager::CandidateKind::Bug as i32,
            suggested_priority: manager::CandidatePriority::P1 as i32,
            suggested_state: manager::CandidateSuggestedState::Open as i32,
            detection_basis: manager::DetectionBasis::Explicit as i32,
            title: "Title".into(),
            description: "Description".into(),
            ..Default::default()
        }
    }

    fn row(candidate: manager::Candidate) -> manager::ReviewCandidateRow {
        manager::ReviewCandidateRow {
            candidate: Some(candidate),
            integrity: manager::ReviewCandidateIntegrity::Normal as i32,
            resolution_state: manager::ReviewCandidateResolutionState::Actionable as i32,
            ..Default::default()
        }
    }

    #[test]
    fn dto_mapper_preserves_normal_and_pending_states() {
        let item = candidate_row_to_review_item(row(candidate())).unwrap();
        assert_eq!(item.integrity, "normal");
        assert_eq!(item.resolution_state, "actionable");
        assert_eq!(item.review_state, "candidate");

        let mut pending = row(candidate());
        pending.resolution_state =
            manager::ReviewCandidateResolutionState::ResolutionPending as i32;
        let item = candidate_row_to_review_item(pending).unwrap();
        assert_eq!(item.resolution_state, "resolution_pending");

        for (integrity, expected) in [
            (
                manager::ReviewCandidateIntegrity::MappingMissing,
                "mapping_missing",
            ),
            (
                manager::ReviewCandidateIntegrity::MappingInvalid,
                "mapping_invalid",
            ),
            (
                manager::ReviewCandidateIntegrity::IssueNotFound,
                "issue_not_found",
            ),
            (
                manager::ReviewCandidateIntegrity::UnknownIssueState,
                "unknown_issue_state",
            ),
        ] {
            let mut anomaly = row(candidate());
            anomaly.candidate.as_mut().unwrap().status =
                manager::CandidateStatus::AutoRegistered as i32;
            anomaly.integrity = integrity as i32;
            assert_eq!(
                candidate_row_to_review_item(anomaly).unwrap().integrity,
                expected
            );
        }
    }

    #[test]
    fn dto_mapper_fails_closed_for_unknown_enum_and_missing_candidate() {
        assert!(candidate_row_to_review_item(manager::ReviewCandidateRow::default()).is_err());
        let mut unknown = row(candidate());
        unknown.integrity = 99;
        assert!(candidate_row_to_review_item(unknown).is_err());
        let mut unknown_candidate = row(candidate());
        unknown_candidate.candidate.as_mut().unwrap().kind = 99;
        assert!(candidate_row_to_review_item(unknown_candidate).is_err());
    }

    #[test]
    fn auto_registered_resolution_requires_matching_mapping_and_open_issue() {
        let mut auto = candidate();
        auto.status = manager::CandidateStatus::AutoRegistered as i32;
        let issue = v1::Issue {
            issue_id: "issue-1".into(),
            state: v1::IssueState::Open as i32,
            ..Default::default()
        };
        let row = manager::IssueWithSync {
            issue: Some(issue.clone()),
            mapping: Some(manager::IssueMapping {
                issue_id: "issue-1".into(),
                candidate_id: Some("candidate-1".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(validate_auto_registered_issue(row, &auto, "issue-1").is_ok());

        let mut mismatch = manager::IssueWithSync {
            issue: Some(issue.clone()),
            mapping: Some(manager::IssueMapping {
                issue_id: "issue-1".into(),
                candidate_id: Some("other-candidate".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(validate_auto_registered_issue(mismatch.clone(), &auto, "issue-1").is_err());

        mismatch.mapping = Some(manager::IssueMapping {
            issue_id: "issue-1".into(),
            candidate_id: Some("candidate-1".into()),
            ..Default::default()
        });
        mismatch.issue = Some(v1::Issue {
            state: v1::IssueState::Done as i32,
            ..issue
        });
        assert!(validate_auto_registered_issue(mismatch, &auto, "issue-1").is_err());
    }
}

#[cfg(test)]
mod issue_list_projection_tests {
    use super::*;

    fn issue_row(issue: v1::Issue) -> manager::IssueWithSync {
        manager::IssueWithSync {
            issue: Some(issue),
            ..Default::default()
        }
    }

    #[test]
    fn issue_dto_preserves_awaiting_state_typed_context_and_provenance_separately() {
        let row = issue_row(v1::Issue {
            issue_id: "issue-1".into(),
            state: v1::IssueState::AwaitingReview as i32,
            contexts: vec![v1::ContextRef {
                kind: v1::ContextKind::Repository as i32,
                value: "owner/repository".into(),
                uri: Some("https://example.test/owner/repository".into()),
            }],
            provenance_metadata: vec![v1::MetadataEntry {
                key: super::super::source_labels::SOURCE_LABEL_SNAPSHOTS_KEY.into(),
                value: r#"[{"thread_id":"thread-1","extraction_run_id":"run-1","labels":["team:platform"]}]"#.into(),
            }],
            ..Default::default()
        });

        let item = issue_to_list_item_checked(row)
            .expect("known Issue values map")
            .expect("Issue row is present");

        assert_eq!(item.state, "AWAITING_REVIEW");
        assert_eq!(
            item.contexts,
            vec![IssueContext {
                kind: IssueContextKind::Repository,
                value: "owner/repository".into(),
                uri: Some("https://example.test/owner/repository".into()),
            }]
        );
        assert_eq!(
            serde_json::to_value(&item).unwrap()["contexts"],
            serde_json::json!([{
                "kind": "REPOSITORY",
                "value": "owner/repository",
                "uri": "https://example.test/owner/repository"
            }])
        );
        assert_eq!(item.source_labels, vec!["team:platform"]);
    }

    #[test]
    fn issue_dto_defaults_absent_contexts_and_rejects_unknown_state_or_context_enums() {
        let item = issue_to_list_item_checked(issue_row(v1::Issue {
            state: v1::IssueState::Open as i32,
            ..Default::default()
        }))
        .expect("default Issue values map")
        .expect("Issue row is present");
        assert!(item.contexts.is_empty());
        assert_eq!(
            serde_json::to_value(&item).unwrap()["contexts"],
            serde_json::json!([])
        );

        let unknown_state = issue_row(v1::Issue {
            state: 99,
            ..Default::default()
        });
        assert!(matches!(
            issue_to_list_item_checked(unknown_state),
            Err(AppError::IssueTrackerServer(_))
        ));

        let unknown_context = issue_row(v1::Issue {
            state: v1::IssueState::Open as i32,
            contexts: vec![v1::ContextRef {
                kind: 99,
                value: "unknown".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        assert!(matches!(
            issue_to_list_item_checked(unknown_context),
            Err(AppError::IssueTrackerServer(_))
        ));
    }

    #[test]
    fn awaiting_review_is_listed_as_nonterminal_and_is_not_a_user_transition_target() {
        let row = issue_row(v1::Issue {
            state: v1::IssueState::AwaitingReview as i32,
            confirmation_status: v1::ConfirmationStatus::Confirmed as i32,
            ..Default::default()
        });
        assert!(issue_is_listed(&row, &HashSet::new(), false).unwrap());

        let unknown = issue_row(v1::Issue {
            state: 99,
            ..Default::default()
        });
        assert!(matches!(
            issue_is_listed(&unknown, &HashSet::new(), false),
            Err(AppError::IssueTrackerServer(_))
        ));

        assert!(parse_user_managed_state_target("OPEN").is_ok());
        assert!(parse_user_managed_state_target("BLOCKED").is_ok());
        assert!(parse_user_managed_state_target("AWAITING_REVIEW").is_err());
        assert!(!user_state_transition_allowed(
            v1::IssueState::AwaitingReview as i32,
            v1::IssueState::Open
        ));
        assert!(!user_state_transition_allowed(
            v1::IssueState::AwaitingReview as i32,
            v1::IssueState::Blocked
        ));
    }

    #[test]
    fn canonical_issue_state_projection_covers_all_seven_public_states() {
        for (wire_state, expected) in [
            (v1::IssueState::Open, "OPEN"),
            (v1::IssueState::InProgress, "IN_PROGRESS"),
            (v1::IssueState::Blocked, "BLOCKED"),
            (v1::IssueState::Deferred, "DEFERRED"),
            (v1::IssueState::Done, "DONE"),
            (v1::IssueState::Canceled, "CANCELED"),
            (v1::IssueState::AwaitingReview, "AWAITING_REVIEW"),
        ] {
            let item = issue_to_list_item_checked(issue_row(v1::Issue {
                state: wire_state as i32,
                ..Default::default()
            }))
            .expect("known lifecycle state maps")
            .expect("Issue row is present");
            assert_eq!(item.state, expected);
        }
    }
}
