use chrono::Utc;
use prost::Message;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::{
    error::{AppError, AppResult},
    grpc::{
        issue_tracker::{IssueTrackerClientError, ManagerWriteRequest},
        proto::issue_tracker::{manager::v1 as manager, v1},
    },
};

use super::super::AppState;
use super::common::{
    durable_operation_state_name, human_actor, map_client_error, parse_revision, require_rationale,
};
use super::issue_operation_journal::{PendingOperationJournal, PendingOperationRecord};
#[derive(Debug, Clone, Deserialize)]
pub struct CompletionEvidenceRequest {
    pub kind: String,
    pub summary: String,
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompletionReportRequest {
    pub verified_evidence: Option<Vec<CompletionEvidenceRequest>>,
    pub unverified_reason: Option<String>,
    pub residual_risk: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DependencyOverrideRequest {
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InconsistentClaimResolutionRequest {
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OwnershipDiagnosisReferenceRequest {
    pub target_kind: String,
    pub target_id: String,
    pub diagnosis_class: String,
    pub evidence_digest: String,
    pub diagnosis_generation: String,
    pub observation_token: String,
    pub repair_epoch: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplyIssueAdminActionRequest {
    pub issue_id: String,
    pub expected_revision: String,
    pub action: String,
    pub rationale: String,
    pub completion: Option<CompletionReportRequest>,
    pub cancellation_classification: Option<String>,
    pub dependency_override: Option<DependencyOverrideRequest>,
    pub inconsistent_claim_resolution: Option<InconsistentClaimResolutionRequest>,
    pub ownership_diagnosis: Option<OwnershipDiagnosisReferenceRequest>,
    pub operation_key: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueOperationStatus {
    pub operation_id: Option<String>,
    pub operation_key: Option<String>,
    pub issue_id: String,
    pub method: String,
    pub state: String,
    pub generation: String,
    pub resolution_required: bool,
    pub affects_ready: bool,
    pub diagnosis: Option<String>,
    /// References exposed by the manager as safe inputs for evidence
    /// confirmation.  The client never invents or accepts arbitrary
    /// evidence identifiers for this mode.
    pub evidence_refs: Vec<String>,
    pub resulting_revision: Option<String>,
    pub resulting_state: Option<String>,
    pub resolution_operation_id: Option<String>,
    pub error_code: Option<String>,
    pub ownership_cleared: Option<bool>,
    pub dependency_override_applied: Option<bool>,
    pub inconsistent_claim_resolution_applied: Option<bool>,
    pub observed_lease_classification: Option<String>,
    pub arbitration_outcome: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetIssueOperationStatusRequest {
    pub issue_id: String,
    pub operation_id: String,
    pub operation_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveIssueOperationRequest {
    pub issue_id: String,
    pub operation_id: String,
    pub expected_operation_generation: String,
    pub expected_revision: String,
    /// Evidence confirmation uses this field. Human arbitration derives the
    /// conclusion from `arbitration_outcome` and omits it at the boundary.
    pub decision: Option<String>,
    pub mode: String,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    pub arbitration_outcome: Option<String>,
    #[serde(default)]
    pub uncertainty_acknowledged: bool,
    pub arbitration_reason: Option<String>,
    pub operation_key: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingIssueOperationView {
    pub operation_key: String,
    pub method: String,
    pub issue_id: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RetryPendingIssueOperationRequest {
    pub operation_key: String,
}
pub(super) async fn get_issue_operation_status_inner(
    state: State<'_, AppState>,
    req: GetIssueOperationStatusRequest,
) -> AppResult<IssueOperationStatus> {
    let issue_id = req.issue_id.trim();
    if issue_id.is_empty() || req.operation_id.trim().is_empty() {
        return Err(AppError::Config(
            "Issue ID and operation ID are required".into(),
        ));
    }
    let service = state.issue_tracker_service().await?;
    let operation = service
        .get_operation("issue-operation-status", req.operation_id.clone())
        .await
        .map_err(map_client_error)?;
    if !operation_targets_issue(&operation, issue_id) {
        return Err(AppError::IssueTrackerServer(
            "Operation is not directly scoped to the requested Issue".into(),
        ));
    }
    let requested_operation_key = req
        .operation_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let status = operation_to_status(&operation, issue_id, req.operation_key);
    if status_is_journal_cleanup_safe(&status)
        && let Some(operation_key) = requested_operation_key
    {
        let journal =
            PendingOperationJournal::new(state.data.issue_tracker_pending_operations_path());
        if let Some(record) = journal
            .list()
            .map_err(|error| {
                AppError::IssueTrackerServer(format!("pending operation lookup failed: {error}"))
            })?
            .into_iter()
            .find(|record| record.operation_key == operation_key)
            && pending_record_matches_operation(&record, &operation, issue_id)
        {
            journal.remove_record(&record).map_err(|error| {
                AppError::IssueTrackerServer(format!("pending operation cleanup failed: {error}"))
            })?;
        }
    }
    Ok(status)
}

pub(super) async fn apply_issue_admin_action_inner(
    state: State<'_, AppState>,
    req: ApplyIssueAdminActionRequest,
) -> AppResult<IssueOperationStatus> {
    let operation_key = req.operation_key.trim().to_owned();
    let service = state.issue_tracker_service().await?;
    let request = build_admin_request(&req)?;
    let payload_bytes = service
        .encode_apply_issue_admin_action(request.clone())
        .map_err(map_client_error)?;
    let identity = service
        .identity_snapshot()
        .ok_or_else(|| AppError::IssueTrackerServer("manager identity is unavailable".into()))?;
    let journal = PendingOperationJournal::new(state.data.issue_tracker_pending_operations_path());
    journal
        .put(PendingOperationRecord::new(
            operation_key.clone(),
            "manager_apply_issue_admin_action",
            req.issue_id.clone(),
            &payload_bytes,
            Utc::now().timestamp_millis(),
            identity,
        ))
        .map_err(|error| {
            AppError::IssueTrackerServer(format!("pending operation journal failed: {error}"))
        })?;
    let mutation = service.apply_issue_admin_action(request).await;
    let mutation = match mutation {
        Ok(mutation) => mutation,
        Err(error) => {
            if let Some(status) = manager_error_to_status(
                &error,
                &req.issue_id,
                &operation_key,
                "manager_apply_issue_admin_action",
            ) {
                if status_is_journal_cleanup_safe(&status) {
                    let _ = journal.remove(&operation_key);
                }
                return Ok(status);
            }
            if definitive_client_error(&error) {
                let _ = journal.remove(&operation_key);
            }
            // A transport failure leaves the prepared request in the journal;
            // callers can resend it after the endpoint recovers.
            return Err(map_client_error(error));
        }
    };
    let Some(operation) = mutation.operation.clone() else {
        return Ok(result_unconfirmed_status(
            operation_key,
            req.issue_id,
            "manager_apply_issue_admin_action",
        ));
    };
    let operation_id = operation.operation_id.clone();
    let status = operation_to_status_with_mutation(
        &operation,
        &req.issue_id,
        Some(operation_key.clone()),
        Some(&mutation),
    );
    if status_is_journal_cleanup_safe(&status) {
        journal.remove(&operation_key).map_err(|error| {
            AppError::IssueTrackerServer(format!("pending operation cleanup failed: {error}"))
        })?;
    }
    Ok(IssueOperationStatus {
        operation_id: Some(operation_id),
        ..status
    })
}

pub(super) async fn resolve_issue_operation_inner(
    state: State<'_, AppState>,
    req: ResolveIssueOperationRequest,
) -> AppResult<IssueOperationStatus> {
    let operation_key = req.operation_key.trim().to_owned();
    let service = state.issue_tracker_service().await?;
    let request = build_resolution_request(&req)?;
    let source_operation = service
        .get_operation(
            "issue-operation-resolution-source",
            req.operation_id.clone(),
        )
        .await
        .map_err(map_client_error)?;
    if !resolution_allowed_for_client(&source_operation, &request.payload) {
        return Err(AppError::Config(
            "This operation cannot be resolved with the selected mode and decision".into(),
        ));
    }
    let safe_refs = safe_evidence_refs(&source_operation);
    if request
        .payload
        .evidence_refs
        .iter()
        .any(|reference| !safe_refs.iter().any(|safe| safe == reference))
    {
        return Err(AppError::Config(
            "Evidence references must come from the Issue Tracker operation".into(),
        ));
    }
    if request.payload.mode == manager::IssueOperationResolutionMode::EvidenceConfirmation as i32 {
        let decision = manager::IssueOperationResolutionDecision::try_from(
            request.payload.decision,
        )
        .map_err(|_| AppError::Config("Evidence confirmation decision is invalid".into()))?;
        let required_reference = required_evidence_reference(&source_operation, decision)
            .ok_or_else(|| {
                AppError::Config(
                    "The selected evidence confirmation has no server-owned proof reference".into(),
                )
            })?;
        if !request
            .payload
            .evidence_refs
            .iter()
            .any(|reference| reference == &required_reference)
        {
            return Err(AppError::Config(format!(
                "Evidence confirmation requires reference: {required_reference}"
            )));
        }
    }
    let payload_bytes = service
        .encode_resolve_issue_operation(request.clone())
        .map_err(map_client_error)?;
    let identity = service
        .identity_snapshot()
        .ok_or_else(|| AppError::IssueTrackerServer("manager identity is unavailable".into()))?;
    let journal = PendingOperationJournal::new(state.data.issue_tracker_pending_operations_path());
    journal
        .put(PendingOperationRecord::new(
            operation_key.clone(),
            "manager_resolve_issue_operation",
            req.issue_id.clone(),
            &payload_bytes,
            Utc::now().timestamp_millis(),
            identity,
        ))
        .map_err(|error| {
            AppError::IssueTrackerServer(format!("pending operation journal failed: {error}"))
        })?;
    let mutation = match service.resolve_issue_operation(request).await {
        Ok(mutation) => mutation,
        Err(error) => {
            if let Some(status) = manager_error_to_status(
                &error,
                &req.issue_id,
                &operation_key,
                "manager_resolve_issue_operation",
            ) {
                if status_is_journal_cleanup_safe(&status) {
                    let _ = journal.remove(&operation_key);
                }
                return Ok(status);
            }
            if definitive_client_error(&error) {
                let _ = journal.remove(&operation_key);
            }
            return Err(map_client_error(error));
        }
    };
    let Some(operation) = mutation.operation.clone() else {
        return Ok(result_unconfirmed_status(
            operation_key,
            req.issue_id,
            "manager_resolve_issue_operation",
        ));
    };
    let status = operation_to_status_with_mutation(
        &operation,
        &req.issue_id,
        Some(operation_key.clone()),
        Some(&mutation),
    );
    if status_is_journal_cleanup_safe(&status) {
        journal.remove(&operation_key).map_err(|error| {
            AppError::IssueTrackerServer(format!("pending operation cleanup failed: {error}"))
        })?;
    }
    Ok(status)
}

pub(super) async fn list_pending_issue_operations_inner(
    state: State<'_, AppState>,
) -> AppResult<Vec<PendingIssueOperationView>> {
    let journal = PendingOperationJournal::new(state.data.issue_tracker_pending_operations_path());
    journal
        .list()
        .map(|records| {
            records
                .into_iter()
                .map(|record| PendingIssueOperationView {
                    operation_key: record.operation_key,
                    method: record.method,
                    issue_id: record.issue_id,
                    created_at_ms: record.created_at_ms,
                })
                .collect()
        })
        .map_err(|error| {
            AppError::IssueTrackerServer(format!("pending operation journal failed: {error}"))
        })
}

/// Replays the exact encoded request retained by the journal. This command is
/// intentionally explicit: a journal entry is never matched to an operation
/// by issue id or timestamp.
pub(super) async fn retry_pending_issue_operation_inner(
    state: State<'_, AppState>,
    req: RetryPendingIssueOperationRequest,
) -> AppResult<IssueOperationStatus> {
    let operation_key = req.operation_key.trim();
    if operation_key.is_empty() {
        return Err(AppError::Config("Operation key is required".into()));
    }
    let journal = PendingOperationJournal::new(state.data.issue_tracker_pending_operations_path());
    let record = journal
        .list()
        .map_err(|error| {
            AppError::IssueTrackerServer(format!("pending operation journal failed: {error}"))
        })?
        .into_iter()
        .find(|record| record.operation_key == operation_key)
        .ok_or_else(|| AppError::Config("Pending operation key was not found".into()))?;
    let service = state.issue_tracker_service().await?;
    let current_identity = service
        .identity_snapshot()
        .ok_or_else(|| AppError::IssueTrackerServer("manager identity is unavailable".into()))?;
    if current_identity.0 != record.schema_fingerprint
        || current_identity.1 != record.data_root_id
        || current_identity.2 != record.contract_fingerprint
    {
        return Err(AppError::IssueTrackerServer(
            "pending operation belongs to a different manager identity; refusing replay".into(),
        ));
    }
    let bytes = record.payload().map_err(|error| {
        AppError::IssueTrackerServer(format!("pending operation payload is invalid: {error}"))
    })?;
    let status = match record.method.as_str() {
        "manager_apply_issue_admin_action" => {
            let payload =
                manager::ApplyIssueAdminActionArgs::decode(bytes.as_slice()).map_err(|error| {
                    AppError::IssueTrackerServer(format!(
                        "pending admin request is invalid: {error}"
                    ))
                })?;
            let context = payload.context.clone().ok_or_else(|| {
                AppError::IssueTrackerServer("pending admin request has no context".into())
            })?;
            let request = ManagerWriteRequest {
                correlation_id: context.correlation_id.clone(),
                idempotency_key: context.idempotency_key.clone(),
                actor_audit: context.actor_audit.clone(),
                payload,
            };
            let mutation = match service.apply_issue_admin_action(request).await {
                Ok(mutation) => mutation,
                Err(error) => {
                    if let Some(status) = manager_error_to_status(
                        &error,
                        &record.issue_id,
                        &record.operation_key,
                        &record.method,
                    ) {
                        if status_is_journal_cleanup_safe(&status) {
                            let _ = journal.remove(&record.operation_key);
                        }
                        return Ok(status);
                    }
                    if definitive_client_error(&error) {
                        let _ = journal.remove(&record.operation_key);
                    }
                    return Err(map_client_error(error));
                }
            };
            let Some(operation) = mutation.operation.clone() else {
                return Ok(result_unconfirmed_status(
                    record.operation_key.clone(),
                    record.issue_id.clone(),
                    record.method.clone(),
                ));
            };
            let status = operation_to_status_with_mutation(
                &operation,
                &record.issue_id,
                Some(record.operation_key.clone()),
                Some(&mutation),
            );
            if status_is_journal_cleanup_safe(&status) {
                journal.remove(&record.operation_key).map_err(|error| {
                    AppError::IssueTrackerServer(format!(
                        "pending operation cleanup failed: {error}"
                    ))
                })?;
            }
            status
        }
        "manager_resolve_issue_operation" => {
            let payload =
                manager::ResolveIssueOperationArgs::decode(bytes.as_slice()).map_err(|error| {
                    AppError::IssueTrackerServer(format!(
                        "pending resolution request is invalid: {error}"
                    ))
                })?;
            let context = payload.context.clone().ok_or_else(|| {
                AppError::IssueTrackerServer("pending resolution request has no context".into())
            })?;
            let request = ManagerWriteRequest {
                correlation_id: context.correlation_id.clone(),
                idempotency_key: context.idempotency_key.clone(),
                actor_audit: context.actor_audit.clone(),
                payload,
            };
            let mutation = match service.resolve_issue_operation(request).await {
                Ok(mutation) => mutation,
                Err(error) => {
                    if let Some(status) = manager_error_to_status(
                        &error,
                        &record.issue_id,
                        &record.operation_key,
                        &record.method,
                    ) {
                        if status_is_journal_cleanup_safe(&status) {
                            let _ = journal.remove(&record.operation_key);
                        }
                        return Ok(status);
                    }
                    if definitive_client_error(&error) {
                        let _ = journal.remove(&record.operation_key);
                    }
                    return Err(map_client_error(error));
                }
            };
            let Some(operation) = mutation.operation.clone() else {
                return Ok(result_unconfirmed_status(
                    record.operation_key.clone(),
                    record.issue_id.clone(),
                    record.method.clone(),
                ));
            };
            let status = operation_to_status_with_mutation(
                &operation,
                &record.issue_id,
                Some(record.operation_key.clone()),
                Some(&mutation),
            );
            if status_is_journal_cleanup_safe(&status) {
                journal.remove(&record.operation_key).map_err(|error| {
                    AppError::IssueTrackerServer(format!(
                        "pending operation cleanup failed: {error}"
                    ))
                })?;
            }
            status
        }
        _ => {
            return Err(AppError::Config(
                "Pending operation method is not supported".into(),
            ));
        }
    };
    Ok(status)
}
fn build_admin_request(
    req: &ApplyIssueAdminActionRequest,
) -> AppResult<ManagerWriteRequest<manager::ApplyIssueAdminActionArgs>> {
    let issue_id = req.issue_id.trim();
    if issue_id.is_empty() {
        return Err(AppError::Config("Issue ID is required".into()));
    }
    let operation_key = req.operation_key.trim();
    if operation_key.is_empty() {
        return Err(AppError::Config("Operation key is required".into()));
    }
    let rationale = require_rationale(Some(req.rationale.trim()))?.to_owned();
    let expected_revision = parse_revision(&req.expected_revision)?;
    let action = match req.action.trim().to_ascii_uppercase().as_str() {
        "COMPLETE" => manager::IssueAdminAction::Complete,
        "CANCEL" => manager::IssueAdminAction::Cancel,
        "RECOVER" => manager::IssueAdminAction::Recover,
        "REVOKE" => manager::IssueAdminAction::Revoke,
        _ => return Err(AppError::Config("Unknown Issue admin action".into())),
    };
    let terminal_report = match action {
        manager::IssueAdminAction::Complete => {
            if req.cancellation_classification.is_some() {
                return Err(AppError::Config(
                    "Cancellation classification is only valid for CANCEL".into(),
                ));
            }
            Some(
                manager::apply_issue_admin_action_args::TerminalReport::Completion(
                    build_completion_report(req.completion.as_ref())?,
                ),
            )
        }
        manager::IssueAdminAction::Cancel => {
            if req.completion.is_some() {
                return Err(AppError::Config(
                    "Completion report is only valid for COMPLETE".into(),
                ));
            }
            Some(
                manager::apply_issue_admin_action_args::TerminalReport::Cancellation(
                    v1::CancellationReport {
                        classification: parse_cancellation_class(
                            req.cancellation_classification.as_deref(),
                        )? as i32,
                    },
                ),
            )
        }
        _ => {
            if req.completion.is_some() || req.cancellation_classification.is_some() {
                return Err(AppError::Config(
                    "Terminal report is only valid for COMPLETE or CANCEL".into(),
                ));
            }
            None
        }
    };
    let dependency_override = req
        .dependency_override
        .as_ref()
        .map(|override_| {
            Ok::<manager::DependencyOverride, AppError>(manager::DependencyOverride {
                enabled: true,
                reason: require_rationale(Some(override_.reason.trim()))?.to_owned(),
            })
        })
        .transpose()?;
    if action != manager::IssueAdminAction::Complete && dependency_override.is_some() {
        return Err(AppError::Config(
            "Dependency override is only valid for COMPLETE".into(),
        ));
    }
    let inconsistent_claim_resolution = req
        .inconsistent_claim_resolution
        .as_ref()
        .map(|resolution| {
            Ok::<manager::InconsistentClaimResolution, AppError>(
                manager::InconsistentClaimResolution {
                    clear: true,
                    reason: require_rationale(Some(resolution.reason.trim()))?.to_owned(),
                },
            )
        })
        .transpose()?;
    if inconsistent_claim_resolution.is_some() != req.ownership_diagnosis.is_some() {
        return Err(AppError::Config(
            "Inconsistent claim resolution requires its ownership diagnosis reference".into(),
        ));
    }
    if !matches!(
        action,
        manager::IssueAdminAction::Complete | manager::IssueAdminAction::Cancel
    ) && inconsistent_claim_resolution.is_some()
    {
        return Err(AppError::Config(
            "Inconsistent claim resolution is only valid for COMPLETE or CANCEL".into(),
        ));
    }
    if let Some(diagnosis) = req.ownership_diagnosis.as_ref()
        && (diagnosis.target_kind.trim() != "issue" || diagnosis.target_id.trim() != issue_id)
    {
        return Err(AppError::Config(
            "Ownership diagnosis reference must target this Issue".into(),
        ));
    }
    let ownership_diagnosis = req
        .ownership_diagnosis
        .as_ref()
        .map(parse_diagnosis_reference)
        .transpose()?;
    Ok(ManagerWriteRequest {
        correlation_id: format!("issue-admin-{operation_key}"),
        idempotency_key: operation_key.to_owned(),
        actor_audit: Some(human_actor(Some(rationale))),
        payload: manager::ApplyIssueAdminActionArgs {
            issue_id: issue_id.to_owned(),
            expected_revision: Some(expected_revision),
            action: action as i32,
            terminal_report,
            dependency_override,
            inconsistent_claim_resolution,
            ownership_diagnosis,
            ..Default::default()
        },
    })
}

fn build_resolution_request(
    req: &ResolveIssueOperationRequest,
) -> AppResult<ManagerWriteRequest<manager::ResolveIssueOperationArgs>> {
    if req.issue_id.trim().is_empty() || req.operation_id.trim().is_empty() {
        return Err(AppError::Config(
            "Issue ID and operation ID are required".into(),
        ));
    }
    if req.operation_key.trim().is_empty() {
        return Err(AppError::Config("Operation key is required".into()));
    }
    let mode = match req.mode.trim().to_ascii_uppercase().as_str() {
        "EVIDENCE_CONFIRMATION" | "EVIDENCE" => {
            manager::IssueOperationResolutionMode::EvidenceConfirmation
        }
        "HUMAN_ARBITRATION" | "ARBITRATION" => {
            manager::IssueOperationResolutionMode::HumanArbitration
        }
        _ => return Err(AppError::Config("Unknown operation resolution mode".into())),
    };
    let decision = if mode == manager::IssueOperationResolutionMode::EvidenceConfirmation {
        match req
            .decision
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_uppercase)
            .as_deref()
        {
            Some("CONFIRM_APPLIED") | Some("APPLIED") => {
                manager::IssueOperationResolutionDecision::ConfirmApplied
            }
            Some("CONFIRM_NOT_APPLIED") | Some("NOT_APPLIED") => {
                manager::IssueOperationResolutionDecision::ConfirmNotApplied
            }
            _ => {
                return Err(AppError::Config(
                    "Operation resolution decision is required for evidence confirmation".into(),
                ));
            }
        }
    } else {
        manager::IssueOperationResolutionDecision::Unspecified
    };
    let arbitration_outcome = match req
        .arbitration_outcome
        .as_deref()
        .map(|value| value.trim().to_ascii_uppercase())
    {
        None => manager::IssueOperationArbitrationOutcome::Unspecified,
        Some(value) => match value.as_str() {
            "APPLIED" => manager::IssueOperationArbitrationOutcome::Applied,
            "NOT_APPLIED" => manager::IssueOperationArbitrationOutcome::NotApplied,
            "UNKNOWN_SETTLED" => manager::IssueOperationArbitrationOutcome::UnknownSettled,
            _ => return Err(AppError::Config("Unknown arbitration outcome".into())),
        },
    };
    if mode == manager::IssueOperationResolutionMode::EvidenceConfirmation
        && (req.evidence_refs.is_empty()
            || req
                .evidence_refs
                .iter()
                .all(|reference| reference.trim().is_empty()))
    {
        return Err(AppError::Config(
            "Evidence confirmation requires evidence references".into(),
        ));
    }
    if mode == manager::IssueOperationResolutionMode::HumanArbitration {
        if !req.uncertainty_acknowledged {
            return Err(AppError::Config(
                "Human arbitration requires uncertainty acknowledgement".into(),
            ));
        }
        require_rationale(req.arbitration_reason.as_deref())?;
        if arbitration_outcome == manager::IssueOperationArbitrationOutcome::Unspecified {
            return Err(AppError::Config(
                "Human arbitration requires a conclusion".into(),
            ));
        }
    } else if req
        .arbitration_reason
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        || req.uncertainty_acknowledged
        || arbitration_outcome != manager::IssueOperationArbitrationOutcome::Unspecified
    {
        return Err(AppError::Config(
            "Arbitration fields are only valid in human arbitration mode".into(),
        ));
    }
    let expected_operation_generation = parse_revision(&req.expected_operation_generation)?;
    let expected_revision = parse_revision(&req.expected_revision)?;
    let audit_reason = if mode == manager::IssueOperationResolutionMode::HumanArbitration {
        Some(
            req.arbitration_reason
                .as_deref()
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .unwrap_or("人間による結果不明操作の仲裁")
                .to_owned(),
        )
    } else {
        Some("サーバ所有 evidence による結果確認".to_owned())
    };
    Ok(ManagerWriteRequest {
        correlation_id: format!("issue-operation-resolution-{}", req.operation_key.trim()),
        idempotency_key: req.operation_key.trim().to_owned(),
        actor_audit: Some(human_actor(audit_reason)),
        payload: manager::ResolveIssueOperationArgs {
            issue_id: req.issue_id.trim().to_owned(),
            operation_id: req.operation_id.trim().to_owned(),
            expected_operation_generation: Some(expected_operation_generation),
            expected_revision: Some(expected_revision),
            // Human arbitration derives its conclusion from
            // `arbitration_outcome`; the server rejects a populated decision
            // field in that mode because it would mix evidence and judgment.
            decision: decision as i32,
            evidence_refs: req
                .evidence_refs
                .iter()
                .map(|reference| reference.trim().to_owned())
                .filter(|reference| !reference.is_empty())
                .collect(),
            mode: mode as i32,
            arbitration_outcome: arbitration_outcome as i32,
            uncertainty_acknowledged: req.uncertainty_acknowledged,
            arbitration_reason: req.arbitration_reason.clone().unwrap_or_default(),
            ..Default::default()
        },
    })
}

fn resolution_allowed_for_client(
    operation: &manager::DurableOperation,
    request: &manager::ResolveIssueOperationArgs,
) -> bool {
    if !matches!(
        operation.scope.as_ref(),
        Some(manager::durable_operation::Scope::IssueId(issue_id))
            if issue_id == &request.issue_id
    ) {
        return false;
    }
    let state = manager::DurableOperationState::try_from(operation.state).ok();
    let mode = manager::IssueOperationResolutionMode::try_from(request.mode).ok();
    match (mode, state) {
        (Some(manager::IssueOperationResolutionMode::HumanArbitration), Some(state)) => {
            matches!(
                state,
                manager::DurableOperationState::Received
                    | manager::DurableOperationState::Pending
                    | manager::DurableOperationState::Unconfirmed
                    | manager::DurableOperationState::ManualResolution
            ) || (state == manager::DurableOperationState::Failed && operation.resolution_required)
        }
        (Some(manager::IssueOperationResolutionMode::EvidenceConfirmation), Some(state)) => {
            let decision =
                manager::IssueOperationResolutionDecision::try_from(request.decision).ok();
            if !matches!(
                operation.method.as_str(),
                "manager_apply_issue_admin_action"
                    | "manager_transition_issue"
                    | "manager_claim_issue"
                    | "manager_heartbeat_issue_claim"
                    | "manager_release_issue_claim"
                    | "manager_reconcile_expired_claim"
            ) {
                return false;
            }
            match decision {
                Some(manager::IssueOperationResolutionDecision::ConfirmApplied) => matches!(
                    state,
                    manager::DurableOperationState::Unconfirmed
                        | manager::DurableOperationState::ManualResolution
                ),
                Some(manager::IssueOperationResolutionDecision::ConfirmNotApplied) => {
                    (state == manager::DurableOperationState::Failed
                        && !operation.affects_ready
                        && matches!(
                            operation.terminal_snapshot,
                            Some(manager::durable_operation::TerminalSnapshot::ErrorSnapshot(
                                _
                            ))
                        ))
                        || (matches!(
                            state,
                            manager::DurableOperationState::Unconfirmed
                                | manager::DurableOperationState::ManualResolution
                        ) && operation.resolution_required)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn build_completion_report(
    request: Option<&CompletionReportRequest>,
) -> AppResult<v1::CompletionReport> {
    let request =
        request.ok_or_else(|| AppError::Config("Completion report is required".into()))?;
    match (
        &request.verified_evidence,
        request.unverified_reason.as_deref(),
        request.residual_risk.as_deref(),
    ) {
        (Some(evidence), None, None) if !evidence.is_empty() => {
            let evidence = evidence
                .iter()
                .map(|item| {
                    let kind = match item.kind.trim().to_ascii_uppercase().as_str() {
                        "TEST_RESULT" | "TEST" => v1::CompletionEvidenceKind::TestResult,
                        "ARTIFACT" => v1::CompletionEvidenceKind::Artifact,
                        "OTHER_EVIDENCE" | "OTHER" => v1::CompletionEvidenceKind::OtherEvidence,
                        _ => v1::CompletionEvidenceKind::Unspecified,
                    };
                    if kind == v1::CompletionEvidenceKind::Unspecified
                        || item.summary.trim().is_empty()
                    {
                        return Err(AppError::Config(
                            "Each completion evidence needs a kind and summary".into(),
                        ));
                    }
                    Ok(v1::CompletionEvidence {
                        kind: kind as i32,
                        summary: item.summary.trim().to_owned(),
                        uri: item.uri.clone().filter(|uri| !uri.trim().is_empty()),
                    })
                })
                .collect::<AppResult<Vec<_>>>()?;
            Ok(v1::CompletionReport {
                result: Some(v1::completion_report::Result::Verified(
                    v1::VerifiedCompletion { evidence },
                )),
            })
        }
        (None, Some(reason), Some(risk))
            if !reason.trim().is_empty() && !risk.trim().is_empty() =>
        {
            Ok(v1::CompletionReport {
                result: Some(v1::completion_report::Result::Unverified(
                    v1::UnverifiedCompletion {
                        reason: reason.trim().to_owned(),
                        residual_risk: risk.trim().to_owned(),
                    },
                )),
            })
        }
        _ => Err(AppError::Config(
            "Provide exactly one verified evidence set or an unverified reason and residual risk"
                .into(),
        )),
    }
}

fn parse_cancellation_class(value: Option<&str>) -> AppResult<v1::CancellationClass> {
    match value
        .map(|value| value.trim().to_ascii_uppercase())
        .as_deref()
    {
        Some("FALSE_POSITIVE") => Ok(v1::CancellationClass::FalsePositive),
        Some("OTHER") => Ok(v1::CancellationClass::Other),
        _ => Err(AppError::Config(
            "Cancellation classification is required".into(),
        )),
    }
}

fn parse_diagnosis_reference(
    value: &OwnershipDiagnosisReferenceRequest,
) -> AppResult<manager::OwnershipIntegrityDiagnosisReference> {
    Ok(manager::OwnershipIntegrityDiagnosisReference {
        target_kind: require_rationale(Some(value.target_kind.trim()))?.to_owned(),
        target_id: require_rationale(Some(value.target_id.trim()))?.to_owned(),
        diagnosis_class: require_rationale(Some(value.diagnosis_class.trim()))?.to_owned(),
        evidence_digest: require_rationale(Some(value.evidence_digest.trim()))?.to_owned(),
        diagnosis_generation: parse_revision(&value.diagnosis_generation)?,
        observation_token: parse_revision(&value.observation_token)?,
        repair_epoch: parse_revision(&value.repair_epoch)?,
    })
}

fn operation_to_status(
    operation: &manager::DurableOperation,
    issue_id: &str,
    operation_key: Option<String>,
) -> IssueOperationStatus {
    operation_to_status_with_mutation(operation, issue_id, operation_key, None)
}

fn operation_to_status_with_mutation(
    operation: &manager::DurableOperation,
    issue_id: &str,
    operation_key: Option<String>,
    direct_mutation: Option<&manager::ManagerMutation>,
) -> IssueOperationStatus {
    let operation_state = manager::DurableOperationState::try_from(operation.state).ok();
    let typed_receipt_required = matches!(
        operation.method.as_str(),
        "manager_apply_issue_admin_action" | "manager_resolve_issue_operation"
    );
    let mut receipt = OperationReceipt::default();
    let mut typed_receipt_valid = false;
    let mut terminal_error_code = None;
    if let Some(mutation) = direct_mutation {
        typed_receipt_valid = match operation.method.as_str() {
            "manager_apply_issue_admin_action" => {
                apply_mutation_status(mutation, &mut receipt)
                    && matches!(
                        mutation.result.as_ref(),
                        Some(manager::manager_mutation::Result::IssueAdminAction(_))
                    )
            }
            "manager_resolve_issue_operation" => {
                if let Some(manager::manager_mutation::Result::IssueOperationResolution(result)) =
                    mutation.result.as_ref()
                {
                    apply_resolution_receipt(result, &mut receipt)
                } else {
                    false
                }
            }
            _ => false,
        };
    }
    if let Some(manager::durable_operation::TerminalSnapshot::ResultSnapshot(snapshot)) =
        operation.terminal_snapshot.as_ref()
    {
        if !typed_receipt_valid
            && operation.method == "manager_apply_issue_admin_action"
            && snapshot.schema == "issue_tracker.manager.v1.ApplyIssueAdminActionResult"
        {
            if let Ok(result) =
                manager::ApplyIssueAdminActionResult::decode(snapshot.payload.as_slice())
                && let Some(manager::apply_issue_admin_action_result::Outcome::Success(mutation)) =
                    result.outcome
            {
                typed_receipt_valid = apply_mutation_status(&mutation, &mut receipt)
                    && matches!(
                        mutation.result,
                        Some(manager::manager_mutation::Result::IssueAdminAction(_))
                    );
            }
        } else if !typed_receipt_valid
            && operation.method == "manager_resolve_issue_operation"
            && snapshot.schema == "issue_tracker.manager.v1.IssueOperationResolutionResult"
        {
            if let Ok(result) =
                manager::IssueOperationResolutionResult::decode(snapshot.payload.as_slice())
            {
                typed_receipt_valid = apply_resolution_receipt(&result, &mut receipt);
            }
        } else if !typed_receipt_valid
            && !typed_receipt_required
            && snapshot.schema == "issue_tracker.manager.v1.ManagerMutation"
            && let Ok(mutation) = manager::ManagerMutation::decode(snapshot.payload.as_slice())
        {
            typed_receipt_valid = apply_mutation_status(&mutation, &mut receipt);
        }
    } else if let Some(manager::durable_operation::TerminalSnapshot::ErrorSnapshot(snapshot)) =
        operation.terminal_snapshot.as_ref()
        && snapshot.schema == "issue_tracker.manager.v1.ManagerError"
        && let Ok(error) = manager::ManagerError::decode(snapshot.payload.as_slice())
    {
        terminal_error_code = manager::ManagerErrorCode::try_from(error.code)
            .ok()
            .map(|code| code.as_str_name().to_owned());
    }
    let committed_without_typed_receipt = typed_receipt_required
        && operation_state == Some(manager::DurableOperationState::Committed)
        && !typed_receipt_valid;
    let evidence_refs = safe_evidence_refs(operation);
    IssueOperationStatus {
        operation_id: Some(operation.operation_id.clone()).filter(|value| !value.is_empty()),
        operation_key,
        issue_id: issue_id.to_owned(),
        method: operation.method.clone(),
        state: if committed_without_typed_receipt {
            "RESULT_UNCONFIRMED".into()
        } else {
            operation_state.map_or_else(|| "UNKNOWN".into(), durable_operation_state_name)
        },
        generation: operation.generation.to_string(),
        resolution_required: operation.resolution_required || committed_without_typed_receipt,
        affects_ready: operation.affects_ready,
        diagnosis: operation
            .diagnosis
            .as_ref()
            .map(|diagnosis| diagnosis.classification.clone()),
        evidence_refs,
        resulting_revision: receipt.resulting_revision,
        resulting_state: receipt.resulting_state,
        resolution_operation_id: operation
            .diagnosis
            .as_ref()
            .and_then(|diagnosis| diagnosis.resolution_operation_id.clone()),
        error_code: if committed_without_typed_receipt {
            Some("RESULT_UNCONFIRMED".into())
        } else {
            terminal_error_code
        },
        ownership_cleared: receipt.ownership_cleared,
        dependency_override_applied: receipt.dependency_override_applied,
        inconsistent_claim_resolution_applied: receipt.inconsistent_claim_resolution_applied,
        observed_lease_classification: receipt.observed_lease_classification,
        arbitration_outcome: receipt.arbitration_outcome,
    }
}

fn operation_targets_issue(operation: &manager::DurableOperation, issue_id: &str) -> bool {
    matches!(
        operation.scope.as_ref(),
        Some(manager::durable_operation::Scope::IssueId(target)) if target == issue_id
    )
}

fn canonical_idempotency_scope(
    contract_fingerprint: &str,
    data_root_id: &str,
    method: &str,
    target: &str,
    operation_key: &str,
) -> String {
    fn append(buffer: &mut String, value: &str) {
        use std::fmt::Write;
        let _ = write!(buffer, "{}:", value.len());
        buffer.push_str(value);
    }

    let mut scope = String::new();
    append(&mut scope, contract_fingerprint);
    append(&mut scope, data_root_id);
    append(&mut scope, method);
    append(&mut scope, target);
    append(&mut scope, operation_key);
    scope
}

/// Confirm that a terminal operation is the same request represented by the
/// local pending journal entry before removing that entry. The operation ID
/// alone is insufficient here because status polling accepts a caller
/// supplied operation key; the immutable idempotency scope and request digest
/// bind the key, method, target, and encoded payload together.
fn pending_record_matches_operation(
    record: &PendingOperationRecord,
    operation: &manager::DurableOperation,
    issue_id: &str,
) -> bool {
    if !matches!(
        record.method.as_str(),
        "manager_apply_issue_admin_action" | "manager_resolve_issue_operation"
    ) || record.issue_id != issue_id
        || record.method != operation.method
        || record.operation_key.trim().is_empty()
        || operation.request_digest.trim().is_empty()
        || operation.idempotency_scope.trim().is_empty()
    {
        return false;
    }

    let expected_scope = canonical_idempotency_scope(
        &record.contract_fingerprint,
        &record.data_root_id,
        &record.method,
        &format!("issue:{issue_id}"),
        &record.operation_key,
    );
    operation.idempotency_scope == expected_scope
        && record.payload_digest == operation.request_digest
        && record.payload().is_ok()
}

/// Return only references the Issue Tracker can bind to this operation when
/// evidence confirmation is requested.  These are derived from server-owned
/// operation metadata; request input is never treated as an evidence
/// namespace by the client.
fn safe_evidence_refs(operation: &manager::DurableOperation) -> Vec<String> {
    let mut references = Vec::new();
    let mut add = |reference: &str| {
        if !reference.trim().is_empty() && !references.iter().any(|known| known == reference) {
            references.push(reference.to_owned());
        }
    };

    if let Some(diagnosis) = operation.diagnosis.as_ref() {
        add(&diagnosis.evidence_ref);
    }

    if operation.operation_id.trim().is_empty() {
        return references;
    }

    // Administrative actions use their durable manager operation as the
    // Tracker marker. Other worker methods expose the downstream marker.
    if operation.method == "manager_apply_issue_admin_action" {
        add(&operation.operation_id);
    } else {
        add(&operation.downstream_operation_id);
    }

    // A proven NOT_APPLIED decision is bound to this validated error
    // snapshot reference. Do not offer it when the snapshot is absent or
    // malformed, because the server would reject it as unbound evidence.
    if matches!(
        operation.terminal_snapshot.as_ref(),
        Some(manager::durable_operation::TerminalSnapshot::ErrorSnapshot(snapshot))
            if snapshot.schema == "issue_tracker.manager.v1.ManagerError"
                && manager::ManagerError::decode(snapshot.payload.as_slice()).is_ok()
    ) {
        add(&format!(
            "manager-operation:{}:error-snapshot",
            operation.operation_id
        ));
    }

    references
}

fn required_evidence_reference(
    operation: &manager::DurableOperation,
    decision: manager::IssueOperationResolutionDecision,
) -> Option<String> {
    if operation.operation_id.trim().is_empty() {
        return None;
    }
    match decision {
        manager::IssueOperationResolutionDecision::ConfirmApplied => {
            let reference = if operation.method == "manager_apply_issue_admin_action" {
                &operation.operation_id
            } else {
                &operation.downstream_operation_id
            };
            (!reference.trim().is_empty()).then(|| reference.clone())
        }
        manager::IssueOperationResolutionDecision::ConfirmNotApplied => {
            let error_snapshot_valid = matches!(
                operation.terminal_snapshot.as_ref(),
                Some(manager::durable_operation::TerminalSnapshot::ErrorSnapshot(snapshot))
                    if snapshot.schema == "issue_tracker.manager.v1.ManagerError"
                        && manager::ManagerError::decode(snapshot.payload.as_slice()).is_ok()
            );
            error_snapshot_valid.then(|| {
                format!(
                    "manager-operation:{}:error-snapshot",
                    operation.operation_id
                )
            })
        }
        manager::IssueOperationResolutionDecision::Unspecified => None,
    }
}

fn manager_error_to_status(
    error: &IssueTrackerClientError,
    issue_id: &str,
    operation_key: &str,
    method: &str,
) -> Option<IssueOperationStatus> {
    let IssueTrackerClientError::Manager(error) = error else {
        return None;
    };
    if let Some(manager::manager_error::Detail::Operation(operation)) = error.detail.as_ref() {
        return Some(operation_to_status(
            operation,
            issue_id,
            Some(operation_key.to_owned()),
        ));
    }
    if let Some(manager::manager_error::Detail::OwnershipIntegrityDiagnosis(diagnosis)) =
        error.detail.as_ref()
    {
        let diagnosis_text = if diagnosis.diagnosis_class.trim().is_empty() {
            error.message.clone()
        } else {
            diagnosis.diagnosis_class.clone()
        };
        return Some(IssueOperationStatus {
            operation_id: None,
            operation_key: Some(operation_key.to_owned()),
            issue_id: issue_id.to_owned(),
            method: method.to_owned(),
            state: "PRECONDITION_FAILED".into(),
            generation: String::new(),
            resolution_required: false,
            affects_ready: false,
            diagnosis: Some(diagnosis_text),
            evidence_refs: Vec::new(),
            resulting_revision: None,
            resulting_state: None,
            resolution_operation_id: diagnosis.resolution_operation_id.clone(),
            error_code: Some(
                manager::ManagerErrorCode::try_from(error.code)
                    .ok()
                    .map_or_else(
                        || "PRECONDITION_FAILED".to_owned(),
                        |code| code.as_str_name().to_owned(),
                    ),
            ),
            ownership_cleared: None,
            dependency_override_applied: None,
            inconsistent_claim_resolution_applied: None,
            observed_lease_classification: diagnosis.lease_classification.clone(),
            arbitration_outcome: None,
        });
    }
    let code = manager::ManagerErrorCode::try_from(error.code).ok()?;
    let state = match code {
        manager::ManagerErrorCode::BackendUnavailable => "BACKEND_UNAVAILABLE",
        manager::ManagerErrorCode::ResultUnconfirmed => "RESULT_UNCONFIRMED",
        manager::ManagerErrorCode::RevisionConflict => "REVISION_CONFLICT",
        manager::ManagerErrorCode::GenerationConflict => "GENERATION_CONFLICT",
        _ => return None,
    };
    Some(IssueOperationStatus {
        operation_id: None,
        operation_key: Some(operation_key.to_owned()),
        issue_id: issue_id.to_owned(),
        method: method.to_owned(),
        state: state.into(),
        generation: String::new(),
        resolution_required: state == "RESULT_UNCONFIRMED",
        affects_ready: false,
        diagnosis: Some(error.message.clone()),
        evidence_refs: Vec::new(),
        resulting_revision: None,
        resulting_state: None,
        resolution_operation_id: None,
        error_code: Some(code.as_str_name().to_owned()),
        ownership_cleared: None,
        dependency_override_applied: None,
        inconsistent_claim_resolution_applied: None,
        observed_lease_classification: None,
        arbitration_outcome: None,
    })
}

fn definitive_client_error(error: &IssueTrackerClientError) -> bool {
    match error {
        IssueTrackerClientError::InvalidRequest(_) => true,
        IssueTrackerClientError::Manager(error) => {
            !error.retryable
                && !matches!(
                    manager::ManagerErrorCode::try_from(error.code).ok(),
                    Some(manager::ManagerErrorCode::ResultUnconfirmed)
                )
        }
        _ => false,
    }
}

#[derive(Default)]
struct OperationReceipt {
    resulting_revision: Option<String>,
    resulting_state: Option<String>,
    ownership_cleared: Option<bool>,
    dependency_override_applied: Option<bool>,
    inconsistent_claim_resolution_applied: Option<bool>,
    observed_lease_classification: Option<String>,
    arbitration_outcome: Option<String>,
}

fn apply_mutation_status(
    mutation: &manager::ManagerMutation,
    receipt: &mut OperationReceipt,
) -> bool {
    if let Some(result) = mutation.result.as_ref() {
        match result {
            manager::manager_mutation::Result::IssueAdminAction(result) => {
                receipt.resulting_revision = Some(result.resulting_revision.to_string());
                receipt.resulting_state = v1::IssueState::try_from(result.resulting_state)
                    .ok()
                    .map(issue_state_name);
                receipt.ownership_cleared = Some(result.ownership_cleared);
                receipt.dependency_override_applied = Some(result.dependency_override_applied);
                receipt.inconsistent_claim_resolution_applied =
                    Some(result.inconsistent_claim_resolution_applied);
                receipt.observed_lease_classification =
                    (!result.observed_lease_classification.is_empty())
                        .then(|| result.observed_lease_classification.clone());
            }
            manager::manager_mutation::Result::IssueOperationResolution(result) => {
                receipt.resulting_revision = Some(result.verified_issue_revision.to_string());
                receipt.resulting_state =
                    manager::DurableOperationState::try_from(result.resulting_state)
                        .ok()
                        .map(durable_operation_state_name);
                receipt.arbitration_outcome =
                    manager::IssueOperationArbitrationOutcome::try_from(result.arbitration_outcome)
                        .ok()
                        .filter(|outcome| {
                            *outcome != manager::IssueOperationArbitrationOutcome::Unspecified
                        })
                        .map(arbitration_outcome_name);
            }
            _ => return false,
        }
        true
    } else {
        false
    }
}

fn apply_resolution_receipt(
    result: &manager::IssueOperationResolutionResult,
    receipt: &mut OperationReceipt,
) -> bool {
    let mode = manager::IssueOperationResolutionMode::try_from(result.mode).ok();
    let decision = manager::IssueOperationResolutionDecision::try_from(result.decision).ok();
    let arbitration_outcome =
        manager::IssueOperationArbitrationOutcome::try_from(result.arbitration_outcome).ok();
    let conclusion_valid = match mode {
        // Human arbitration intentionally leaves the legacy evidence decision
        // unspecified. Its conclusion is carried by the three-valued
        // arbitration outcome, including UNKNOWN_SETTLED.
        Some(manager::IssueOperationResolutionMode::HumanArbitration) => {
            decision == Some(manager::IssueOperationResolutionDecision::Unspecified)
                && arbitration_outcome.is_some_and(|outcome| {
                    outcome != manager::IssueOperationArbitrationOutcome::Unspecified
                })
        }
        Some(manager::IssueOperationResolutionMode::EvidenceConfirmation) => {
            decision.is_some_and(|decision| {
                matches!(
                    decision,
                    manager::IssueOperationResolutionDecision::ConfirmApplied
                        | manager::IssueOperationResolutionDecision::ConfirmNotApplied
                )
            }) && arbitration_outcome
                == Some(manager::IssueOperationArbitrationOutcome::Unspecified)
                && !result.evidence_refs.is_empty()
        }
        _ => false,
    };
    if result.operation_id.trim().is_empty()
        || result.source_operation_id.trim().is_empty()
        || manager::DurableOperationState::try_from(result.resulting_state).ok()
            == Some(manager::DurableOperationState::Unspecified)
        || !conclusion_valid
    {
        return false;
    }
    receipt.resulting_revision = Some(result.verified_issue_revision.to_string());
    receipt.resulting_state = manager::DurableOperationState::try_from(result.resulting_state)
        .ok()
        .map(durable_operation_state_name);
    receipt.arbitration_outcome =
        manager::IssueOperationArbitrationOutcome::try_from(result.arbitration_outcome)
            .ok()
            .filter(|outcome| *outcome != manager::IssueOperationArbitrationOutcome::Unspecified)
            .map(arbitration_outcome_name);
    true
}

fn issue_state_name(state: v1::IssueState) -> String {
    match state {
        v1::IssueState::Open => "OPEN",
        v1::IssueState::InProgress => "IN_PROGRESS",
        v1::IssueState::Blocked => "BLOCKED",
        v1::IssueState::Deferred => "DEFERRED",
        v1::IssueState::Done => "DONE",
        v1::IssueState::Canceled => "CANCELED",
        v1::IssueState::AwaitingReview => "AWAITING_REVIEW",
        v1::IssueState::Unspecified => "UNKNOWN",
    }
    .into()
}

fn arbitration_outcome_name(outcome: manager::IssueOperationArbitrationOutcome) -> String {
    match outcome {
        manager::IssueOperationArbitrationOutcome::Applied => "APPLIED",
        manager::IssueOperationArbitrationOutcome::NotApplied => "NOT_APPLIED",
        manager::IssueOperationArbitrationOutcome::UnknownSettled => "UNKNOWN_SETTLED",
        manager::IssueOperationArbitrationOutcome::Unspecified => "UNKNOWN",
    }
    .into()
}

fn result_unconfirmed_status(
    operation_key: impl Into<String>,
    issue_id: impl Into<String>,
    method: impl Into<String>,
) -> IssueOperationStatus {
    IssueOperationStatus {
        operation_id: None,
        operation_key: Some(operation_key.into()),
        issue_id: issue_id.into(),
        method: method.into(),
        state: "RESULT_UNCONFIRMED".into(),
        generation: String::new(),
        resolution_required: true,
        affects_ready: true,
        diagnosis: None,
        evidence_refs: Vec::new(),
        resulting_revision: None,
        resulting_state: None,
        resolution_operation_id: None,
        error_code: Some("RESULT_UNCONFIRMED".into()),
        ownership_cleared: None,
        dependency_override_applied: None,
        inconsistent_claim_resolution_applied: None,
        observed_lease_classification: None,
        arbitration_outcome: None,
    }
}

fn status_is_terminal_and_resolved(status: &IssueOperationStatus) -> bool {
    match status.state.as_str() {
        "COMMITTED" => !status.resolution_required,
        "FAILED" | "ARBITRATED" => !status.resolution_required && !status.affects_ready,
        _ => false,
    }
}

/// A request can be removed from the local replay journal once the manager
/// has either settled it or definitively rejected its compare-and-swap or
/// ownership/dependency precondition fence. Transport failures and
/// inconsistent failed operations remain journaled so the caller can recover
/// them after restart.
fn status_is_journal_cleanup_safe(status: &IssueOperationStatus) -> bool {
    if matches!(status.state.as_str(), "FAILED" | "ARBITRATED") && status.affects_ready {
        return false;
    }
    status_is_terminal_and_resolved(status)
        || (status.operation_id.is_none()
            && !status.resolution_required
            && matches!(
                status.state.as_str(),
                "REVISION_CONFLICT" | "GENERATION_CONFLICT" | "PRECONDITION_FAILED"
            ))
}

#[cfg(test)]
mod admin_action_tests {
    use super::*;

    fn base(action: &str) -> ApplyIssueAdminActionRequest {
        ApplyIssueAdminActionRequest {
            issue_id: "issue-1".into(),
            expected_revision: "0".into(),
            action: action.into(),
            rationale: "human decision".into(),
            completion: None,
            cancellation_classification: None,
            dependency_override: None,
            inconsistent_claim_resolution: None,
            ownership_diagnosis: None,
            operation_key: "operation-key".into(),
        }
    }

    #[test]
    fn complete_requires_exactly_one_completion_report() {
        assert!(build_admin_request(&base("COMPLETE")).is_err());
        let mut request = base("COMPLETE");
        request.completion = Some(CompletionReportRequest {
            verified_evidence: Some(vec![CompletionEvidenceRequest {
                kind: "TEST_RESULT".into(),
                summary: "tests passed".into(),
                uri: None,
            }]),
            unverified_reason: None,
            residual_risk: None,
        });
        let request = build_admin_request(&request).unwrap();
        assert!(matches!(
            request.payload.terminal_report,
            Some(manager::apply_issue_admin_action_args::TerminalReport::Completion(_))
        ));
    }

    #[test]
    fn cancel_rejects_a_completion_report_and_requires_classification() {
        let mut request = base("CANCEL");
        request.completion = Some(CompletionReportRequest {
            verified_evidence: None,
            unverified_reason: Some("x".into()),
            residual_risk: Some("y".into()),
        });
        assert!(build_admin_request(&request).is_err());
        request.completion = None;
        request.cancellation_classification = Some("OTHER".into());
        assert!(build_admin_request(&request).is_ok());
    }

    #[test]
    fn resolution_modes_are_mutually_exclusive() {
        let request = ResolveIssueOperationRequest {
            issue_id: "issue-1".into(),
            operation_id: "operation-1".into(),
            expected_operation_generation: "1".into(),
            expected_revision: "2".into(),
            decision: Some("CONFIRM_APPLIED".into()),
            mode: "EVIDENCE_CONFIRMATION".into(),
            evidence_refs: Vec::new(),
            arbitration_outcome: None,
            uncertainty_acknowledged: false,
            arbitration_reason: None,
            operation_key: "resolution-key".into(),
        };
        assert!(build_resolution_request(&request).is_err());
    }

    #[test]
    fn evidence_resolution_has_a_non_empty_audit_reason() {
        let request = ResolveIssueOperationRequest {
            issue_id: "issue-1".into(),
            operation_id: "operation-1".into(),
            expected_operation_generation: "1".into(),
            expected_revision: "2".into(),
            decision: Some("CONFIRM_APPLIED".into()),
            mode: "EVIDENCE_CONFIRMATION".into(),
            evidence_refs: vec!["manager-operation:operation-1".into()],
            arbitration_outcome: None,
            uncertainty_acknowledged: false,
            arbitration_reason: None,
            operation_key: "resolution-key".into(),
        };
        let request = build_resolution_request(&request).unwrap();
        assert_eq!(
            request
                .actor_audit
                .as_ref()
                .and_then(|actor| actor.reason.as_deref()),
            Some("サーバ所有 evidence による結果確認")
        );
    }

    #[test]
    fn human_resolution_uses_arbitration_outcome_instead_of_decision_field() {
        let request = ResolveIssueOperationRequest {
            issue_id: "issue-1".into(),
            operation_id: "operation-1".into(),
            expected_operation_generation: "1".into(),
            expected_revision: "2".into(),
            decision: None,
            mode: "HUMAN_ARBITRATION".into(),
            evidence_refs: Vec::new(),
            arbitration_outcome: Some("APPLIED".into()),
            uncertainty_acknowledged: true,
            arbitration_reason: Some("reviewed the incident log".into()),
            operation_key: "resolution-key".into(),
        };
        let request = build_resolution_request(&request).unwrap();
        assert_eq!(
            request.payload.decision,
            manager::IssueOperationResolutionDecision::Unspecified as i32
        );
        assert_eq!(
            request.payload.arbitration_outcome,
            manager::IssueOperationArbitrationOutcome::Applied as i32
        );
    }

    #[test]
    fn resolution_allowlist_rejects_mutation_methods_for_evidence_confirmation() {
        let operation = manager::DurableOperation {
            scope: Some(manager::durable_operation::Scope::IssueId("issue-1".into())),
            method: "manager_update_issue".into(),
            state: manager::DurableOperationState::Unconfirmed as i32,
            resolution_required: true,
            ..Default::default()
        };
        let request = manager::ResolveIssueOperationArgs {
            issue_id: "issue-1".into(),
            mode: manager::IssueOperationResolutionMode::EvidenceConfirmation as i32,
            decision: manager::IssueOperationResolutionDecision::ConfirmApplied as i32,
            ..Default::default()
        };
        assert!(!resolution_allowed_for_client(&operation, &request));
    }

    #[test]
    fn evidence_confirmation_requires_the_decision_specific_server_proof() {
        let operation = manager::DurableOperation {
            operation_id: "operation-1".into(),
            method: "manager_apply_issue_admin_action".into(),
            terminal_snapshot: Some(manager::durable_operation::TerminalSnapshot::ErrorSnapshot(
                manager::Snapshot {
                    schema: "issue_tracker.manager.v1.ManagerError".into(),
                    payload: manager::ManagerError::default().encode_to_vec(),
                },
            )),
            ..Default::default()
        };
        assert_eq!(
            required_evidence_reference(
                &operation,
                manager::IssueOperationResolutionDecision::ConfirmApplied,
            )
            .as_deref(),
            Some("operation-1")
        );
        assert_eq!(
            required_evidence_reference(
                &operation,
                manager::IssueOperationResolutionDecision::ConfirmNotApplied,
            )
            .as_deref(),
            Some("manager-operation:operation-1:error-snapshot")
        );

        let operation_without_downstream_marker = manager::DurableOperation {
            method: "manager_transition_issue".into(),
            ..operation
        };
        assert!(
            required_evidence_reference(
                &operation_without_downstream_marker,
                manager::IssueOperationResolutionDecision::ConfirmApplied,
            )
            .is_none()
        );
    }

    #[test]
    fn committed_admin_operation_exposes_the_typed_receipt() {
        let mutation = manager::ManagerMutation {
            result: Some(manager::manager_mutation::Result::IssueAdminAction(
                manager::IssueAdminActionResult {
                    resulting_state: v1::IssueState::Done as i32,
                    resulting_revision: 4,
                    ownership_cleared: true,
                    dependency_override_applied: true,
                    observed_lease_classification: "NO_LEASE".into(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        let wrapper = manager::ApplyIssueAdminActionResult {
            outcome: Some(manager::apply_issue_admin_action_result::Outcome::Success(
                mutation,
            )),
        };
        let operation = manager::DurableOperation {
            operation_id: "operation-1".into(),
            scope: Some(manager::durable_operation::Scope::IssueId("issue-1".into())),
            method: "manager_apply_issue_admin_action".into(),
            state: manager::DurableOperationState::Committed as i32,
            terminal_snapshot: Some(
                manager::durable_operation::TerminalSnapshot::ResultSnapshot(manager::Snapshot {
                    payload: wrapper.encode_to_vec(),
                    schema: "issue_tracker.manager.v1.ApplyIssueAdminActionResult".into(),
                }),
            ),
            ..Default::default()
        };
        let status = operation_to_status(&operation, "issue-1", None);
        assert_eq!(status.state, "COMMITTED");
        assert_eq!(status.resulting_revision.as_deref(), Some("4"));
        assert_eq!(status.resulting_state.as_deref(), Some("DONE"));
        assert_eq!(status.ownership_cleared, Some(true));
        assert_eq!(status.dependency_override_applied, Some(true));
        assert_eq!(
            status.observed_lease_classification.as_deref(),
            Some("NO_LEASE")
        );
    }

    #[test]
    fn awaiting_review_is_reported_without_collapsing_to_open_or_terminal_state() {
        assert_eq!(
            issue_state_name(v1::IssueState::AwaitingReview),
            "AWAITING_REVIEW"
        );
    }

    #[test]
    fn committed_admin_operation_without_typed_snapshot_is_result_unconfirmed() {
        let operation = manager::DurableOperation {
            operation_id: "operation-1".into(),
            scope: Some(manager::durable_operation::Scope::IssueId("issue-1".into())),
            method: "manager_apply_issue_admin_action".into(),
            state: manager::DurableOperationState::Committed as i32,
            ..Default::default()
        };
        let status = operation_to_status(&operation, "issue-1", None);
        assert_eq!(status.state, "RESULT_UNCONFIRMED");
        assert!(status.resolution_required);
        assert_eq!(status.error_code.as_deref(), Some("RESULT_UNCONFIRMED"));
    }

    #[test]
    fn failed_operation_affecting_ready_is_not_removed_from_journal() {
        let operation = manager::DurableOperation {
            operation_id: "operation-1".into(),
            scope: Some(manager::durable_operation::Scope::IssueId("issue-1".into())),
            method: "manager_transition_issue".into(),
            state: manager::DurableOperationState::Failed as i32,
            affects_ready: true,
            ..Default::default()
        };
        let status = operation_to_status(&operation, "issue-1", Some("key-1".into()));
        assert!(!status_is_terminal_and_resolved(&status));
        assert!(!status_is_journal_cleanup_safe(&status));

        let mut settled_operation = operation;
        settled_operation.affects_ready = false;
        let settled_status =
            operation_to_status(&settled_operation, "issue-1", Some("key-1".into()));
        assert!(status_is_terminal_and_resolved(&settled_status));
        assert!(status_is_journal_cleanup_safe(&settled_status));
    }

    #[test]
    fn compare_and_swap_conflicts_are_exposed_as_definitive_statuses() {
        for code in [
            manager::ManagerErrorCode::RevisionConflict,
            manager::ManagerErrorCode::GenerationConflict,
        ] {
            let error = IssueTrackerClientError::Manager(Box::new(manager::ManagerError {
                code: code as i32,
                message: "compare-and-swap rejected".into(),
                ..Default::default()
            }));
            let status = manager_error_to_status(&error, "issue-1", "key-1", "manager_test")
                .expect("conflicts should be returned as structured status");
            assert_eq!(status.state, code.as_str_name());
            assert_eq!(status.error_code.as_deref(), Some(code.as_str_name()));
            assert!(!status.resolution_required);
            assert!(status_is_journal_cleanup_safe(&status));
        }
    }

    #[test]
    fn missing_operation_receipt_is_exposed_as_result_unconfirmed() {
        let status = result_unconfirmed_status(
            "operation-key",
            "issue-1",
            "manager_apply_issue_admin_action",
        );
        assert_eq!(status.state, "RESULT_UNCONFIRMED");
        assert_eq!(status.operation_key.as_deref(), Some("operation-key"));
        assert_eq!(status.issue_id, "issue-1");
        assert!(status.resolution_required);
        assert!(status.affects_ready);
        assert_eq!(status.error_code.as_deref(), Some("RESULT_UNCONFIRMED"));
    }

    #[test]
    fn ownership_preconditions_are_exposed_as_structured_statuses() {
        let error = IssueTrackerClientError::Manager(Box::new(manager::ManagerError {
            code: manager::ManagerErrorCode::PreconditionFailed as i32,
            message: "the issue has an active lease".into(),
            detail: Some(manager::manager_error::Detail::OwnershipIntegrityDiagnosis(
                manager::OwnershipIntegrityDiagnosis {
                    diagnosis_class: "ACTIVE_LEASE".into(),
                    lease_classification: Some("ACTIVE".into()),
                    resolution_operation_id: Some("diagnosis-operation".into()),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }));
        let status = manager_error_to_status(
            &error,
            "issue-1",
            "operation-key",
            "manager_apply_issue_admin_action",
        )
        .expect("ownership preconditions should be structured");
        assert_eq!(status.state, "PRECONDITION_FAILED");
        assert_eq!(status.error_code.as_deref(), Some("PRECONDITION_FAILED"));
        assert_eq!(status.diagnosis.as_deref(), Some("ACTIVE_LEASE"));
        assert_eq!(
            status.observed_lease_classification.as_deref(),
            Some("ACTIVE")
        );
        assert_eq!(
            status.resolution_operation_id.as_deref(),
            Some("diagnosis-operation")
        );
        assert!(status_is_journal_cleanup_safe(&status));
    }

    #[test]
    fn journal_cleanup_requires_the_operation_scope_and_request_digest() {
        let payload = b"encoded-request";
        let record = PendingOperationRecord::new(
            "operation-key",
            "manager_apply_issue_admin_action",
            "issue-1",
            payload,
            1,
            ("schema".into(), "root".into(), "contract".into()),
        );
        let operation = manager::DurableOperation {
            method: record.method.clone(),
            idempotency_scope: canonical_idempotency_scope(
                &record.contract_fingerprint,
                &record.data_root_id,
                &record.method,
                "issue:issue-1",
                &record.operation_key,
            ),
            request_digest: record.payload_digest.clone(),
            ..Default::default()
        };
        assert!(pending_record_matches_operation(
            &record, &operation, "issue-1"
        ));

        let mut mismatched_operation = operation.clone();
        mismatched_operation.idempotency_scope = canonical_idempotency_scope(
            &record.contract_fingerprint,
            &record.data_root_id,
            &record.method,
            "issue:issue-1",
            "other-key",
        );
        assert!(!pending_record_matches_operation(
            &record,
            &mismatched_operation,
            "issue-1"
        ));

        mismatched_operation.idempotency_scope = operation.idempotency_scope;
        mismatched_operation.request_digest = "different-request".into();
        assert!(!pending_record_matches_operation(
            &record,
            &mismatched_operation,
            "issue-1"
        ));
    }

    #[test]
    fn direct_admin_mutation_receipt_is_used_before_polling() {
        let operation = manager::DurableOperation {
            operation_id: "operation-1".into(),
            scope: Some(manager::durable_operation::Scope::IssueId("issue-1".into())),
            method: "manager_apply_issue_admin_action".into(),
            state: manager::DurableOperationState::Committed as i32,
            ..Default::default()
        };
        let mutation = manager::ManagerMutation {
            result: Some(manager::manager_mutation::Result::IssueAdminAction(
                manager::IssueAdminActionResult {
                    resulting_state: v1::IssueState::Done as i32,
                    resulting_revision: 5,
                    ownership_cleared: true,
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        let status =
            operation_to_status_with_mutation(&operation, "issue-1", None, Some(&mutation));
        assert_eq!(status.state, "COMMITTED");
        assert_eq!(status.resulting_revision.as_deref(), Some("5"));
        assert_eq!(status.resulting_state.as_deref(), Some("DONE"));
        assert_eq!(status.ownership_cleared, Some(true));
    }

    #[test]
    fn committed_resolution_operation_exposes_the_server_resolution_receipt() {
        let receipt = manager::IssueOperationResolutionResult {
            operation_id: "resolution-operation".into(),
            source_operation_id: "source-operation".into(),
            mode: manager::IssueOperationResolutionMode::HumanArbitration as i32,
            decision: manager::IssueOperationResolutionDecision::Unspecified as i32,
            resulting_state: manager::DurableOperationState::Arbitrated as i32,
            resulting_generation: 3,
            verified_issue_revision: 9,
            arbitration_outcome: manager::IssueOperationArbitrationOutcome::Applied as i32,
            uncertainty_acknowledged: true,
            arbitration_reason: "operator review".into(),
            ..Default::default()
        };
        let operation = manager::DurableOperation {
            operation_id: "resolution-operation".into(),
            scope: Some(manager::durable_operation::Scope::IssueId("issue-1".into())),
            method: "manager_resolve_issue_operation".into(),
            state: manager::DurableOperationState::Committed as i32,
            terminal_snapshot: Some(
                manager::durable_operation::TerminalSnapshot::ResultSnapshot(manager::Snapshot {
                    payload: receipt.encode_to_vec(),
                    schema: "issue_tracker.manager.v1.IssueOperationResolutionResult".into(),
                }),
            ),
            ..Default::default()
        };
        let status = operation_to_status(&operation, "issue-1", None);
        assert_eq!(status.state, "COMMITTED");
        assert_eq!(status.resulting_revision.as_deref(), Some("9"));
        assert_eq!(status.resulting_state.as_deref(), Some("ARBITRATED"));
        assert_eq!(status.arbitration_outcome.as_deref(), Some("APPLIED"));
    }

    #[test]
    fn human_resolution_receipt_accepts_unknown_settled_without_decision() {
        let receipt = manager::IssueOperationResolutionResult {
            operation_id: "resolution-operation".into(),
            source_operation_id: "source-operation".into(),
            mode: manager::IssueOperationResolutionMode::HumanArbitration as i32,
            decision: manager::IssueOperationResolutionDecision::Unspecified as i32,
            resulting_state: manager::DurableOperationState::Arbitrated as i32,
            arbitration_outcome: manager::IssueOperationArbitrationOutcome::UnknownSettled as i32,
            ..Default::default()
        };
        let mut parsed = OperationReceipt::default();
        assert!(apply_resolution_receipt(&receipt, &mut parsed));
        assert_eq!(
            parsed.arbitration_outcome.as_deref(),
            Some("UNKNOWN_SETTLED")
        );
    }

    #[test]
    fn committed_resolution_operation_with_generic_snapshot_is_result_unconfirmed() {
        let mutation = manager::ManagerMutation {
            result: Some(manager::manager_mutation::Result::IssueOperationResolution(
                manager::IssueOperationResolutionResult {
                    operation_id: "resolution-operation".into(),
                    source_operation_id: "source-operation".into(),
                    decision: manager::IssueOperationResolutionDecision::ConfirmApplied as i32,
                    resulting_state: manager::DurableOperationState::Arbitrated as i32,
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        let operation = manager::DurableOperation {
            operation_id: "resolution-operation".into(),
            scope: Some(manager::durable_operation::Scope::IssueId("issue-1".into())),
            method: "manager_resolve_issue_operation".into(),
            state: manager::DurableOperationState::Committed as i32,
            terminal_snapshot: Some(
                manager::durable_operation::TerminalSnapshot::ResultSnapshot(manager::Snapshot {
                    payload: mutation.encode_to_vec(),
                    schema: "issue_tracker.manager.v1.ManagerMutation".into(),
                }),
            ),
            ..Default::default()
        };
        let status = operation_to_status(&operation, "issue-1", None);
        assert_eq!(status.state, "RESULT_UNCONFIRMED");
        assert!(status.resolution_required);
    }

    #[test]
    fn operation_status_exposes_only_server_owned_evidence_references() {
        let operation = manager::DurableOperation {
            operation_id: "operation-1".into(),
            method: "manager_transition_issue".into(),
            downstream_operation_id: "tracker-marker-1".into(),
            diagnosis: Some(manager::OperationDiagnosis {
                evidence_ref: "manager-operation-diagnosis:operation-1".into(),
                ..Default::default()
            }),
            terminal_snapshot: Some(manager::durable_operation::TerminalSnapshot::ErrorSnapshot(
                manager::Snapshot {
                    schema: "issue_tracker.manager.v1.ManagerError".into(),
                    payload: manager::ManagerError::default().encode_to_vec(),
                },
            )),
            ..Default::default()
        };
        let status = operation_to_status(&operation, "issue-1", None);
        assert_eq!(
            status.evidence_refs,
            vec![
                "manager-operation-diagnosis:operation-1",
                "tracker-marker-1",
                "manager-operation:operation-1:error-snapshot",
            ]
        );
    }
}
