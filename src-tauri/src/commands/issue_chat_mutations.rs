use std::time::Duration;

use chrono::Utc;
use prost::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    commands::AppState,
    error::{AppError, AppResult},
    grpc::{
        issue_tracker::{IssueTrackerGrpcService, ManagerWriteRequest},
        proto::issue_tracker::{manager::v1 as manager, v1},
    },
};

// Keep source-evidence verification shared without widening its module visibility.
#[allow(clippy::duplicate_mod)]
#[path = "issues/source_evidence.rs"]
mod source_evidence;

use super::issues::issue_operation_journal::{PendingOperationJournal, PendingOperationRecord};

const OPERATION_WAIT: Duration = Duration::from_secs(10);
const HUMAN_AUDIT_ACTOR_ID: &str = "lookback-user";
const INTERNAL_AGENT_AUDIT_ACTOR_ID: &str = "lookback-issue-chat-agent";
const INTERNAL_AGENT_AUDIT_SESSION_ID: &str = "lookback-issue-chat";

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum IssueChatActorKind {
    Human,
    InternalAgent,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum IssueChatKind {
    Task,
    Bug,
    Research,
    Decision,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum IssueChatPriority {
    P0,
    P1,
    P2,
    P3,
    P4,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum IssueChatConfirmationStatus {
    Confirmed,
    Unreviewed,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum IssueChatCancellationClassification {
    FalsePositive,
    Other,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IssueChatSourceEvidenceInput {
    pub(crate) thread_id: String,
    pub(crate) memory_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateIssueInput {
    pub(crate) speech_id: String,
    pub(crate) tool_call_id: String,
    pub(crate) actor_kind: IssueChatActorKind,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) kind: IssueChatKind,
    pub(crate) priority: IssueChatPriority,
    pub(crate) confirmation_status: IssueChatConfirmationStatus,
    #[serde(default)]
    pub(crate) acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub(crate) source_evidence: Vec<IssueChatSourceEvidenceInput>,
    pub(crate) rationale: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IssueChatPatchInput {
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) title: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) description: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) kind: Option<IssueChatKind>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) priority: Option<IssueChatPriority>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) labels: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) acceptance_criteria: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) confirmation_status: Option<IssueChatConfirmationStatus>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateIssueInput {
    pub(crate) speech_id: String,
    pub(crate) tool_call_id: String,
    pub(crate) issue_id: String,
    pub(crate) expected_revision: String,
    pub(crate) patch: IssueChatPatchInput,
    pub(crate) rationale: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CancelIssueInput {
    pub(crate) speech_id: String,
    pub(crate) tool_call_id: String,
    pub(crate) issue_id: String,
    pub(crate) expected_revision: String,
    pub(crate) classification: IssueChatCancellationClassification,
    pub(crate) rationale: String,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub(crate) duplicate_issue_id: Option<String>,
}

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedCreateIssueInput {
    operation_key: String,
    correlation_id: String,
    issue_id: String,
    actor_kind: IssueChatActorKind,
    title: String,
    description: String,
    kind: IssueChatKind,
    priority: IssueChatPriority,
    confirmation_status: IssueChatConfirmationStatus,
    acceptance_criteria: Vec<String>,
    source_evidence: Vec<v1::SourceEvidenceRef>,
    rationale: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedUpdateIssueInput {
    issue_id: String,
    expected_revision: u64,
    operation_key: String,
    correlation_id: String,
    patch: ValidatedIssueChatPatch,
    rationale: String,
}

#[derive(Debug, Clone, Default)]
struct ValidatedIssueChatPatch {
    title: Option<String>,
    description: Option<String>,
    kind: Option<IssueChatKind>,
    priority: Option<IssueChatPriority>,
    labels: Option<Vec<String>>,
    acceptance_criteria: Option<Vec<String>>,
    confirmation_status: Option<IssueChatConfirmationStatus>,
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedCancelIssueInput {
    issue_id: String,
    expected_revision: u64,
    operation_key: String,
    correlation_id: String,
    classification: IssueChatCancellationClassification,
    rationale: String,
    duplicate_issue_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ManagerIdentitySnapshot {
    schema_fingerprint: String,
    data_root_id: String,
    contract_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct IssueChatMutationResult {
    pub(crate) issue_id: String,
    pub(crate) operation_id: String,
    pub(crate) operation_state: String,
    pub(crate) revision: u64,
    pub(crate) resulting_state: Option<String>,
}

pub(crate) fn validate_create_issue_input(
    input: CreateIssueInput,
) -> AppResult<ValidatedCreateIssueInput> {
    let speech_id = validate_identifier(&input.speech_id, "Speech ID")?;
    let tool_call_id = validate_identifier(&input.tool_call_id, "Tool call ID")?;
    let title = validate_required(&input.title, "Issue title")?;
    let description = validate_text(&input.description, "Issue description", true)?;
    let rationale = validate_required(&input.rationale, "Reason")?;
    let acceptance_criteria =
        normalize_string_list(input.acceptance_criteria, "acceptance criteria")?;
    let source_evidence = input
        .source_evidence
        .into_iter()
        .map(|reference| {
            let thread_id = validate_required(&reference.thread_id, "Source thread ID")?;
            let memory_id = validate_required(&reference.memory_id, "Source memory ID")?;
            thread_id.parse::<i64>().map_err(|_| {
                AppError::Config("Source thread ID must be a numeric memory identifier".into())
            })?;
            memory_id.parse::<i64>().map_err(|_| {
                AppError::Config("Source memory ID must be a numeric memory identifier".into())
            })?;
            Ok(v1::SourceEvidenceRef {
                thread_id,
                memory_id,
            })
        })
        .collect::<AppResult<Vec<_>>>()?;
    if input.actor_kind == IssueChatActorKind::InternalAgent && source_evidence.is_empty() {
        return Err(AppError::Config(
            "INTERNAL_AGENT issue creation requires source evidence".into(),
        ));
    }
    let identity = operation_identity("create", &speech_id, &tool_call_id);
    Ok(ValidatedCreateIssueInput {
        operation_key: format!("issue-chat-create-{}", identity.digest),
        correlation_id: format!("issue-chat-create-{}", identity.digest),
        issue_id: deterministic_issue_id(&identity.digest),
        actor_kind: input.actor_kind,
        title,
        description,
        kind: input.kind,
        priority: input.priority,
        confirmation_status: input.confirmation_status,
        acceptance_criteria,
        source_evidence,
        rationale,
    })
}

fn deterministic_issue_id(digest: &str) -> String {
    // A stable version-8 UUID preserves idempotency while satisfying the
    // canonical tracker's UUID-only Issue ID contract.
    format!(
        "{}-{}-8{}-8{}-{}",
        &digest[..8],
        &digest[8..12],
        &digest[13..16],
        &digest[17..20],
        &digest[20..32],
    )
}

pub(crate) fn validate_update_issue_input(
    input: UpdateIssueInput,
) -> AppResult<ValidatedUpdateIssueInput> {
    let speech_id = validate_identifier(&input.speech_id, "Speech ID")?;
    let tool_call_id = validate_identifier(&input.tool_call_id, "Tool call ID")?;
    let issue_id = validate_identifier(&input.issue_id, "Issue ID")?;
    let expected_revision = parse_revision(&input.expected_revision)?;
    let rationale = validate_required(&input.rationale, "Reason")?;
    let patch = ValidatedIssueChatPatch {
        title: input
            .patch
            .title
            .map(|value| validate_required(&value, "Issue title"))
            .transpose()?,
        description: input
            .patch
            .description
            .map(|value| validate_text(&value, "Issue description", true))
            .transpose()?,
        kind: input.patch.kind,
        priority: input.patch.priority,
        labels: input
            .patch
            .labels
            .map(|values| normalize_string_list(values, "labels"))
            .transpose()?,
        acceptance_criteria: input
            .patch
            .acceptance_criteria
            .map(|values| normalize_string_list(values, "acceptance criteria"))
            .transpose()?,
        confirmation_status: input.patch.confirmation_status,
    };
    if patch_is_empty(&patch) {
        return Err(AppError::Config(
            "Issue update patch must not be empty".into(),
        ));
    }
    let identity = operation_identity("update", &speech_id, &tool_call_id);
    Ok(ValidatedUpdateIssueInput {
        issue_id,
        expected_revision,
        operation_key: format!("issue-chat-update-{}", identity.digest),
        correlation_id: format!("issue-chat-update-{}", identity.digest),
        patch,
        rationale,
    })
}

pub(crate) fn validate_cancel_issue_input(
    input: CancelIssueInput,
) -> AppResult<ValidatedCancelIssueInput> {
    let speech_id = validate_identifier(&input.speech_id, "Speech ID")?;
    let tool_call_id = validate_identifier(&input.tool_call_id, "Tool call ID")?;
    let issue_id = validate_identifier(&input.issue_id, "Issue ID")?;
    let expected_revision = parse_revision(&input.expected_revision)?;
    let rationale = validate_required(&input.rationale, "Reason")?;
    let duplicate_issue_id = input
        .duplicate_issue_id
        .map(|value| validate_identifier(&value, "Duplicate target Issue ID"))
        .transpose()?;
    if let Some(duplicate_id) = duplicate_issue_id.as_deref() {
        if duplicate_id == issue_id {
            return Err(AppError::Config(
                "Duplicate target must be a different Issue".into(),
            ));
        }
        if input.classification != IssueChatCancellationClassification::Other {
            return Err(AppError::Config(
                "Duplicate Issue cancellation must use OTHER classification".into(),
            ));
        }
    }
    let identity = operation_identity("cancel", &speech_id, &tool_call_id);
    Ok(ValidatedCancelIssueInput {
        issue_id,
        expected_revision,
        operation_key: format!("issue-chat-cancel-{}", identity.digest),
        correlation_id: format!("issue-chat-cancel-{}", identity.digest),
        classification: input.classification,
        rationale,
        duplicate_issue_id,
    })
}

pub(crate) async fn create_issue(
    state: &AppState,
    input: ValidatedCreateIssueInput,
) -> AppResult<IssueChatMutationResult> {
    if input.actor_kind == IssueChatActorKind::InternalAgent
        && !source_evidence::all_source_evidence_available(state, &input.source_evidence).await
    {
        return Err(AppError::Config(
            "INTERNAL_AGENT source evidence could not be verified".into(),
        ));
    }
    let service = state.issue_tracker_service().await?;
    let identity = manager_identity(&service)?;
    let request = build_create_manager_request(&input, &identity)?;
    let (journal, record) = journal_request(
        state,
        &identity,
        &input.operation_key,
        "manager_create_issue",
        &input.issue_id,
        &request.payload,
    )?;
    let mutation = service
        .create_issue(request)
        .await
        .map_err(map_client_error)?;
    finish_mutation(
        &service,
        &journal,
        &record,
        mutation,
        SubmittedMutation {
            method: "manager_create_issue",
            kind: MutationKind::Create,
            issue_id: &input.issue_id,
            correlation_id: &input.correlation_id,
            expected_revision: 0,
        },
    )
    .await
}

pub(crate) async fn update_issue(
    state: &AppState,
    input: ValidatedUpdateIssueInput,
) -> AppResult<IssueChatMutationResult> {
    let service = state.issue_tracker_service().await?;
    let identity = manager_identity(&service)?;
    let request = build_update_manager_request(&input, &identity)?;
    let (journal, record) = journal_request(
        state,
        &identity,
        &input.operation_key,
        "manager_update_issue",
        &input.issue_id,
        &request.payload,
    )?;
    let mutation = service
        .update_issue(request)
        .await
        .map_err(map_client_error)?;
    finish_mutation(
        &service,
        &journal,
        &record,
        mutation,
        SubmittedMutation {
            method: "manager_update_issue",
            kind: MutationKind::Update,
            issue_id: &input.issue_id,
            correlation_id: &input.correlation_id,
            expected_revision: input.expected_revision,
        },
    )
    .await
}

pub(crate) async fn cancel_issue(
    state: &AppState,
    input: ValidatedCancelIssueInput,
) -> AppResult<IssueChatMutationResult> {
    let service = state.issue_tracker_service().await?;
    let target = get_issue(
        &service,
        &input.issue_id,
        &format!("{}-target", input.correlation_id),
    )
    .await?;
    let duplicate = match input.duplicate_issue_id.as_deref() {
        Some(issue_id) => Some(
            get_issue(
                &service,
                issue_id,
                &format!("{}-duplicate", input.correlation_id),
            )
            .await?,
        ),
        None => None,
    };
    let target_issue = target
        .issue
        .as_ref()
        .ok_or_else(|| issue_error("Issue was not found"))?;
    let duplicate_id = duplicate
        .as_ref()
        .and_then(|issue| issue.issue.as_ref())
        .map(|issue| issue.issue_id.as_str());
    validate_cancel_target(&input, target_issue, target.sync.as_ref(), duplicate_id)?;

    let identity = manager_identity(&service)?;
    let request = build_cancel_manager_request(&input, &identity)?;
    let (journal, record) = journal_request(
        state,
        &identity,
        &input.operation_key,
        "manager_apply_issue_admin_action",
        &input.issue_id,
        &request.payload,
    )?;
    let mutation = service
        .apply_issue_admin_action(request)
        .await
        .map_err(map_client_error)?;
    finish_mutation(
        &service,
        &journal,
        &record,
        mutation,
        SubmittedMutation {
            method: "manager_apply_issue_admin_action",
            kind: MutationKind::Cancel,
            issue_id: &input.issue_id,
            correlation_id: &input.correlation_id,
            expected_revision: input.expected_revision,
        },
    )
    .await
}

pub(crate) async fn retry_pending_issue_chat_operation(
    state: &AppState,
    operation_key: &str,
) -> AppResult<IssueChatMutationResult> {
    let operation_key = validate_required(operation_key, "Operation key")?;
    let journal = PendingOperationJournal::new(state.data.issue_tracker_pending_operations_path());
    let record = journal
        .list()
        .map_err(|error| issue_error(format!("pending operation journal failed: {error}")))?
        .into_iter()
        .find(|record| record.operation_key == operation_key)
        .ok_or_else(|| AppError::Config("Pending Issue chat operation was not found".into()))?;
    let service = state.issue_tracker_service().await?;
    verify_record_identity(&service, &record)?;
    let bytes = record
        .payload()
        .map_err(|error| issue_error(format!("pending operation payload is invalid: {error}")))?;

    match record.method.as_str() {
        "manager_create_issue" => {
            let payload = manager::CreateIssueArgs::decode(bytes.as_slice()).map_err(|error| {
                issue_error(format!("pending create request is invalid: {error}"))
            })?;
            let context =
                verify_replay_context(payload.context.as_ref(), &record, "manager_create_issue")?
                    .clone();
            ensure_canonical_payload(&payload, &bytes)?;
            if payload.issue_id != record.issue_id || payload.issue.is_none() {
                return Err(issue_error(
                    "Pending create request does not match its journaled Issue",
                ));
            }
            let issue_id = payload.issue_id.clone();
            let mutation = service
                .create_issue(replay_request(payload, &context))
                .await
                .map_err(map_client_error)?;
            finish_mutation(
                &service,
                &journal,
                &record,
                mutation,
                SubmittedMutation {
                    method: "manager_create_issue",
                    kind: MutationKind::Create,
                    issue_id: &issue_id,
                    correlation_id: &context.correlation_id,
                    expected_revision: 0,
                },
            )
            .await
        }
        "manager_update_issue" => {
            let payload = manager::UpdateIssueArgs::decode(bytes.as_slice()).map_err(|error| {
                issue_error(format!("pending update request is invalid: {error}"))
            })?;
            let context =
                verify_replay_context(payload.context.as_ref(), &record, "manager_update_issue")?
                    .clone();
            ensure_canonical_payload(&payload, &bytes)?;
            if payload.issue_id != record.issue_id
                || payload.expected_revision.is_none()
                || payload.update_mask.is_empty()
                || payload.target_state.is_some()
                || payload.terminal_report.is_some()
            {
                return Err(issue_error(
                    "Pending update request does not match its journaled Issue",
                ));
            }
            let issue_id = payload.issue_id.clone();
            let expected_revision = payload
                .expected_revision
                .ok_or_else(|| issue_error("Pending update request has no expected revision"))?;
            let mutation = service
                .update_issue(replay_request(payload, &context))
                .await
                .map_err(map_client_error)?;
            finish_mutation(
                &service,
                &journal,
                &record,
                mutation,
                SubmittedMutation {
                    method: "manager_update_issue",
                    kind: MutationKind::Update,
                    issue_id: &issue_id,
                    correlation_id: &context.correlation_id,
                    expected_revision,
                },
            )
            .await
        }
        "manager_apply_issue_admin_action" => {
            let payload =
                manager::ApplyIssueAdminActionArgs::decode(bytes.as_slice()).map_err(|error| {
                    issue_error(format!("pending cancel request is invalid: {error}"))
                })?;
            let context = verify_replay_context(
                payload.context.as_ref(),
                &record,
                "manager_apply_issue_admin_action",
            )?
            .clone();
            ensure_canonical_payload(&payload, &bytes)?;
            if payload.issue_id != record.issue_id
                || payload.expected_revision.is_none()
                || payload.action != manager::IssueAdminAction::Cancel as i32
                || payload.dependency_override.is_some()
                || payload.inconsistent_claim_resolution.is_some()
                || payload.ownership_diagnosis.is_some()
                || !matches!(
                    payload.terminal_report.as_ref(),
                    Some(manager::apply_issue_admin_action_args::TerminalReport::Cancellation(_))
                )
            {
                return Err(issue_error(
                    "Pending cancellation request does not match its journaled Issue",
                ));
            }
            let issue_id = payload.issue_id.clone();
            let expected_revision = payload
                .expected_revision
                .ok_or_else(|| issue_error("Pending cancellation has no expected revision"))?;
            let mutation = service
                .apply_issue_admin_action(replay_request(payload, &context))
                .await
                .map_err(map_client_error)?;
            finish_mutation(
                &service,
                &journal,
                &record,
                mutation,
                SubmittedMutation {
                    method: "manager_apply_issue_admin_action",
                    kind: MutationKind::Cancel,
                    issue_id: &issue_id,
                    correlation_id: &context.correlation_id,
                    expected_revision,
                },
            )
            .await
        }
        _ => Err(AppError::Config(
            "Pending operation method is not an Issue chat mutation".into(),
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OperationIdentity {
    digest: String,
}

fn operation_identity(action: &str, speech_id: &str, tool_call_id: &str) -> OperationIdentity {
    let mut digest = Sha256::new();
    for component in [action, speech_id, tool_call_id] {
        digest.update((component.len() as u64).to_be_bytes());
        digest.update(component.as_bytes());
    }
    OperationIdentity {
        digest: format!("{:x}", digest.finalize()),
    }
}

fn validate_required(value: &str, name: &str) -> AppResult<String> {
    validate_text(value, name, false)
}

fn validate_identifier(value: &str, name: &str) -> AppResult<String> {
    let normalized = validate_required(value, name)?;
    if normalized.len() > 256 {
        return Err(AppError::Config(format!(
            "{name} exceeds the supported length"
        )));
    }
    Ok(normalized)
}

fn validate_text(value: &str, name: &str, allow_empty: bool) -> AppResult<String> {
    let normalized = value.trim();
    if !allow_empty && normalized.is_empty() {
        return Err(AppError::Config(format!("{name} is required")));
    }
    if normalized.len() > 4096 {
        return Err(AppError::Config(format!(
            "{name} exceeds the supported length"
        )));
    }
    Ok(normalized.to_owned())
}

fn normalize_string_list(values: Vec<String>, name: &str) -> AppResult<Vec<String>> {
    values
        .into_iter()
        .map(|value| validate_required(&value, name))
        .collect()
}

fn parse_revision(value: &str) -> AppResult<u64> {
    value
        .trim()
        .parse()
        .map_err(|_| AppError::Config("Invalid expected revision".into()))
}

fn patch_is_empty(patch: &ValidatedIssueChatPatch) -> bool {
    patch.title.is_none()
        && patch.description.is_none()
        && patch.kind.is_none()
        && patch.priority.is_none()
        && patch.labels.is_none()
        && patch.acceptance_criteria.is_none()
        && patch.confirmation_status.is_none()
}

fn manager_identity(service: &IssueTrackerGrpcService) -> AppResult<ManagerIdentitySnapshot> {
    let (schema_fingerprint, data_root_id, contract_fingerprint) = service
        .identity_snapshot()
        .ok_or_else(|| issue_error("manager identity is unavailable"))?;
    Ok(ManagerIdentitySnapshot {
        schema_fingerprint,
        data_root_id,
        contract_fingerprint,
    })
}

fn write_context(
    identity: &ManagerIdentitySnapshot,
    method: &str,
    issue_id: &str,
    operation_key: &str,
    correlation_id: &str,
    actor_audit: &manager::ActorAudit,
) -> manager::ManagerWriteContext {
    manager::ManagerWriteContext {
        method: method.into(),
        target: format!("issue:{issue_id}"),
        schema_fingerprint: identity.schema_fingerprint.clone(),
        idempotency_key: operation_key.into(),
        actor_audit: Some(actor_audit.clone()),
        correlation_id: correlation_id.into(),
        data_root_id: identity.data_root_id.clone(),
        contract_fingerprint: identity.contract_fingerprint.clone(),
    }
}

fn actor_audit(actor_kind: IssueChatActorKind, rationale: &str) -> manager::ActorAudit {
    let (actor_id, kind, display_name, agent_session_id, reason) = match actor_kind {
        IssueChatActorKind::Human => (
            HUMAN_AUDIT_ACTOR_ID,
            v1::ActorKind::Human,
            "Lookback user",
            None,
            format!("利用者のチャット依頼による: {rationale}"),
        ),
        IssueChatActorKind::InternalAgent => (
            INTERNAL_AGENT_AUDIT_ACTOR_ID,
            v1::ActorKind::InternalAgent,
            "Lookback Issue chat agent",
            Some(INTERNAL_AGENT_AUDIT_SESSION_ID),
            format!("内部 agent による明示的起票: {rationale}"),
        ),
    };
    manager::ActorAudit {
        actor_id: actor_id.into(),
        actor_kind: kind as i32,
        display_name: Some(display_name.into()),
        agent_session_id: agent_session_id.map(str::to_owned),
        reason: Some(reason),
    }
}

fn human_audit(rationale: String, operation: &str) -> manager::ActorAudit {
    manager::ActorAudit {
        actor_id: HUMAN_AUDIT_ACTOR_ID.into(),
        actor_kind: v1::ActorKind::Human as i32,
        display_name: Some("Lookback user".into()),
        reason: Some(format!("チャットからの Issue {operation}: {rationale}")),
        ..Default::default()
    }
}

fn build_create_manager_request(
    input: &ValidatedCreateIssueInput,
    identity: &ManagerIdentitySnapshot,
) -> AppResult<ManagerWriteRequest<manager::CreateIssueArgs>> {
    let actor_audit = actor_audit(input.actor_kind, &input.rationale);
    let issue = v1::IssueDraft {
        title: input.title.clone(),
        description: input.description.clone(),
        kind: issue_kind(input.kind) as i32,
        priority: issue_priority(input.priority) as i32,
        acceptance_criteria: input.acceptance_criteria.clone(),
        creation_basis: v1::CreationBasis::Explicit as i32,
        confirmation_status: confirmation_status(input.confirmation_status) as i32,
        initial_state: v1::IssueState::Open as i32,
        source_evidence: input.source_evidence.clone(),
        ..Default::default()
    };
    Ok(ManagerWriteRequest {
        correlation_id: input.correlation_id.clone(),
        idempotency_key: input.operation_key.clone(),
        actor_audit: Some(actor_audit.clone()),
        payload: manager::CreateIssueArgs {
            context: Some(write_context(
                identity,
                "manager_create_issue",
                &input.issue_id,
                &input.operation_key,
                &input.correlation_id,
                &actor_audit,
            )),
            issue_id: input.issue_id.clone(),
            issue: Some(issue),
            strong_detection_proof: None,
            provenance_sources: Vec::new(),
        },
    })
}

fn build_update_manager_request(
    input: &ValidatedUpdateIssueInput,
    identity: &ManagerIdentitySnapshot,
) -> AppResult<ManagerWriteRequest<manager::UpdateIssueArgs>> {
    let actor_audit = human_audit(input.rationale.clone(), "更新");
    let mut update_mask = Vec::new();
    let mut patch = manager::ManagerIssuePatch::default();
    if let Some(title) = input.patch.title.as_ref() {
        update_mask.push(manager::ManagerIssueField::Title as i32);
        patch.title = Some(title.clone());
    }
    if let Some(description) = input.patch.description.as_ref() {
        update_mask.push(manager::ManagerIssueField::Description as i32);
        patch.description = Some(description.clone());
    }
    if let Some(kind) = input.patch.kind {
        update_mask.push(manager::ManagerIssueField::Kind as i32);
        patch.kind = Some(issue_kind(kind) as i32);
    }
    if let Some(priority) = input.patch.priority {
        update_mask.push(manager::ManagerIssueField::Priority as i32);
        patch.priority = Some(issue_priority(priority) as i32);
    }
    if let Some(labels) = input.patch.labels.as_ref() {
        update_mask.push(manager::ManagerIssueField::Labels as i32);
        patch.labels = labels.clone();
    }
    if let Some(acceptance_criteria) = input.patch.acceptance_criteria.as_ref() {
        update_mask.push(manager::ManagerIssueField::AcceptanceCriteria as i32);
        patch.acceptance_criteria = acceptance_criteria.clone();
    }
    if let Some(status) = input.patch.confirmation_status {
        update_mask.push(manager::ManagerIssueField::ConfirmationStatus as i32);
        patch.confirmation_status = Some(confirmation_status(status) as i32);
    }
    if update_mask.is_empty() {
        return Err(AppError::Config(
            "Issue update patch must not be empty".into(),
        ));
    }
    Ok(ManagerWriteRequest {
        correlation_id: input.correlation_id.clone(),
        idempotency_key: input.operation_key.clone(),
        actor_audit: Some(actor_audit.clone()),
        payload: manager::UpdateIssueArgs {
            context: Some(write_context(
                identity,
                "manager_update_issue",
                &input.issue_id,
                &input.operation_key,
                &input.correlation_id,
                &actor_audit,
            )),
            issue_id: input.issue_id.clone(),
            expected_revision: Some(input.expected_revision),
            update_mask,
            patch: Some(patch),
            ..Default::default()
        },
    })
}

fn build_cancel_manager_request(
    input: &ValidatedCancelIssueInput,
    identity: &ManagerIdentitySnapshot,
) -> AppResult<ManagerWriteRequest<manager::ApplyIssueAdminActionArgs>> {
    let actor_audit = human_audit(input.rationale.clone(), "取消");
    let classification = match input.classification {
        IssueChatCancellationClassification::FalsePositive => v1::CancellationClass::FalsePositive,
        IssueChatCancellationClassification::Other => v1::CancellationClass::Other,
    };
    Ok(ManagerWriteRequest {
        correlation_id: input.correlation_id.clone(),
        idempotency_key: input.operation_key.clone(),
        actor_audit: Some(actor_audit.clone()),
        payload: manager::ApplyIssueAdminActionArgs {
            context: Some(write_context(
                identity,
                "manager_apply_issue_admin_action",
                &input.issue_id,
                &input.operation_key,
                &input.correlation_id,
                &actor_audit,
            )),
            issue_id: input.issue_id.clone(),
            expected_revision: Some(input.expected_revision),
            action: manager::IssueAdminAction::Cancel as i32,
            terminal_report: Some(
                manager::apply_issue_admin_action_args::TerminalReport::Cancellation(
                    v1::CancellationReport {
                        classification: classification as i32,
                    },
                ),
            ),
            ..Default::default()
        },
    })
}

fn issue_kind(kind: IssueChatKind) -> v1::IssueKind {
    match kind {
        IssueChatKind::Task => v1::IssueKind::Task,
        IssueChatKind::Bug => v1::IssueKind::Bug,
        IssueChatKind::Research => v1::IssueKind::Research,
        IssueChatKind::Decision => v1::IssueKind::Decision,
    }
}

fn issue_priority(priority: IssueChatPriority) -> v1::IssuePriority {
    match priority {
        IssueChatPriority::P0 => v1::IssuePriority::P0,
        IssueChatPriority::P1 => v1::IssuePriority::P1,
        IssueChatPriority::P2 => v1::IssuePriority::P2,
        IssueChatPriority::P3 => v1::IssuePriority::P3,
        IssueChatPriority::P4 => v1::IssuePriority::P4,
    }
}

fn confirmation_status(status: IssueChatConfirmationStatus) -> v1::ConfirmationStatus {
    match status {
        IssueChatConfirmationStatus::Confirmed => v1::ConfirmationStatus::Confirmed,
        IssueChatConfirmationStatus::Unreviewed => v1::ConfirmationStatus::Unreviewed,
    }
}

async fn get_issue(
    service: &IssueTrackerGrpcService,
    issue_id: &str,
    correlation_id: &str,
) -> AppResult<manager::IssueWithSync> {
    service
        .get_issue_with_sync(
            correlation_id,
            manager::GetIssueWithSyncArgs {
                issue_id: issue_id.into(),
                ..Default::default()
            },
        )
        .await
        .map_err(map_client_error)
}

fn validate_cancel_target(
    input: &ValidatedCancelIssueInput,
    issue: &v1::Issue,
    sync: Option<&manager::IssueSyncView>,
    confirmed_duplicate_id: Option<&str>,
) -> AppResult<()> {
    if issue.issue_id != input.issue_id {
        return Err(issue_error("Issue tracker returned a different Issue"));
    }
    if issue.revision != input.expected_revision {
        return Err(issue_error(
            "Issue has changed; refresh before cancelling it",
        ));
    }
    let state = v1::IssueState::try_from(issue.state)
        .map_err(|_| issue_error("Issue has an unknown state; refusing cancellation"))?;
    if issue.claim.is_some() {
        return Err(AppError::Config(
            "An Issue with an active claim cannot be cancelled from chat".into(),
        ));
    }
    let sync = sync.ok_or_else(|| issue_error("Issue synchronization status is unavailable"))?;
    if sync.resolution_required
        || manager::SyncState::try_from(sync.sync_state).ok() != Some(manager::SyncState::Confirmed)
    {
        return Err(AppError::Config(
            "Issue has a pending or uncertain operation; refusing cancellation".into(),
        ));
    }
    if matches!(state, v1::IssueState::Done | v1::IssueState::Canceled)
        || state == v1::IssueState::Unspecified
    {
        return Err(AppError::Config(
            "A terminal or unspecified Issue cannot be cancelled from chat".into(),
        ));
    }
    if input.classification == IssueChatCancellationClassification::FalsePositive
        && v1::CreationBasis::try_from(issue.creation_basis).ok()
            != Some(v1::CreationBasis::AutoDetected)
    {
        return Err(AppError::Config(
            "FALSE_POSITIVE is only valid for auto-detected Issues".into(),
        ));
    }
    match (input.duplicate_issue_id.as_deref(), confirmed_duplicate_id) {
        (Some(expected), Some(confirmed)) if expected == confirmed => {}
        (Some(_), _) => {
            return Err(AppError::Config(
                "Duplicate cancellation requires a successfully read, distinct target Issue".into(),
            ));
        }
        (None, Some(_)) => {
            return Err(AppError::Config(
                "Unexpected duplicate target confirmation".into(),
            ));
        }
        (None, None) => {}
    }
    Ok(())
}

fn journal_request<T: Message>(
    state: &AppState,
    identity: &ManagerIdentitySnapshot,
    operation_key: &str,
    method: &str,
    issue_id: &str,
    payload: &T,
) -> AppResult<(PendingOperationJournal, PendingOperationRecord)> {
    let bytes = payload.encode_to_vec();
    let record = PendingOperationRecord::new(
        operation_key,
        method,
        issue_id,
        &bytes,
        Utc::now().timestamp_millis(),
        (
            identity.schema_fingerprint.clone(),
            identity.data_root_id.clone(),
            identity.contract_fingerprint.clone(),
        ),
    );
    let journal = PendingOperationJournal::new(state.data.issue_tracker_pending_operations_path());
    journal
        .put(record.clone())
        .map_err(|error| issue_error(format!("pending operation journal failed: {error}")))?;
    Ok((journal, record))
}

#[derive(Debug, Clone, Copy)]
enum MutationKind {
    Create,
    Update,
    Cancel,
}

struct SubmittedMutation<'a> {
    method: &'static str,
    kind: MutationKind,
    issue_id: &'a str,
    correlation_id: &'a str,
    expected_revision: u64,
}

async fn finish_mutation(
    service: &IssueTrackerGrpcService,
    journal: &PendingOperationJournal,
    record: &PendingOperationRecord,
    mutation: manager::ManagerMutation,
    submitted: SubmittedMutation<'_>,
) -> AppResult<IssueChatMutationResult> {
    let operation_id = mutation
        .operation
        .as_ref()
        .map(|operation| operation.operation_id.clone())
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| issue_error("Mutation response has no durable operation ID"))?;
    let operation = await_chat_operation(service, submitted.correlation_id, &operation_id).await?;
    validate_committed_operation(
        &operation,
        &operation_id,
        submitted.method,
        submitted.issue_id,
    )?;
    let (revision, resulting_state) = if is_marker_confirmed_empty_result(&operation) {
        let issue = get_issue(service, submitted.issue_id, submitted.correlation_id).await?;
        read_back_result(
            &issue,
            submitted.kind,
            submitted.issue_id,
            submitted.expected_revision,
        )?
    } else {
        let committed_mutation = decode_committed_mutation(&operation, submitted.method)?;
        mutation_result(
            &committed_mutation,
            submitted.kind,
            submitted.issue_id,
            submitted.expected_revision,
        )?
    };
    journal
        .remove_record(record)
        .map_err(|error| issue_error(format!("pending operation cleanup failed: {error}")))?;
    Ok(IssueChatMutationResult {
        issue_id: submitted.issue_id.into(),
        operation_id,
        operation_state: "COMMITTED".into(),
        revision,
        resulting_state,
    })
}

async fn await_chat_operation(
    service: &IssueTrackerGrpcService,
    correlation_id: &str,
    operation_id: &str,
) -> AppResult<manager::DurableOperation> {
    let deadline = tokio::time::Instant::now() + OPERATION_WAIT;
    loop {
        let operation = service
            .get_operation(correlation_id, operation_id.to_owned())
            .await
            .map_err(map_client_error)?;
        match manager::DurableOperationState::try_from(operation.state).ok() {
            Some(manager::DurableOperationState::Committed) => return Ok(operation),
            Some(
                manager::DurableOperationState::Received | manager::DurableOperationState::Pending,
            ) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            _ => {
                return Err(issue_error(format!(
                    "Issue operation {operation_id} is not confirmed COMMITTED"
                )));
            }
        }
    }
}

fn is_marker_confirmed_empty_result(operation: &manager::DurableOperation) -> bool {
    matches!(
        operation.terminal_snapshot.as_ref(),
        Some(manager::durable_operation::TerminalSnapshot::ResultSnapshot(snapshot))
            if snapshot.schema == "issue_tracker.manager.v1.ManagerMutation" && snapshot.payload.is_empty()
    )
}

fn read_back_result(
    response: &manager::IssueWithSync,
    kind: MutationKind,
    issue_id: &str,
    expected_revision: u64,
) -> AppResult<(u64, Option<String>)> {
    let issue = response
        .issue
        .as_ref()
        .ok_or_else(|| issue_error("Committed Issue could not be read back"))?;
    if issue.issue_id != issue_id || issue.revision <= expected_revision {
        return Err(issue_error(
            "Committed Issue read-back does not match the submitted revision",
        ));
    }
    let state = v1::IssueState::try_from(issue.state)
        .map_err(|_| issue_error("Committed Issue has an unknown state"))?;
    match kind {
        MutationKind::Create if state == v1::IssueState::Open => {
            Ok((issue.revision, Some("OPEN".into())))
        }
        MutationKind::Update => Ok((
            issue.revision,
            Some(
                state
                    .as_str_name()
                    .trim_start_matches("ISSUE_STATE_")
                    .into(),
            ),
        )),
        MutationKind::Cancel if state == v1::IssueState::Canceled => {
            Ok((issue.revision, Some("CANCELED".into())))
        }
        _ => Err(issue_error(
            "Committed Issue has not reached the requested state",
        )),
    }
}

fn validate_committed_operation(
    operation: &manager::DurableOperation,
    expected_operation_id: &str,
    expected_method: &str,
    expected_issue_id: &str,
) -> AppResult<()> {
    if operation.operation_id != expected_operation_id || operation.method != expected_method {
        return Err(issue_error(
            "Durable operation does not match the submitted Issue mutation",
        ));
    }
    if !matches!(
        operation.scope.as_ref(),
        Some(manager::durable_operation::Scope::IssueId(target)) if target == expected_issue_id
    ) {
        return Err(issue_error(
            "Durable operation is not scoped to the submitted Issue",
        ));
    }
    if manager::DurableOperationState::try_from(operation.state).ok()
        != Some(manager::DurableOperationState::Committed)
    {
        return Err(issue_error(
            "Issue mutation has no durable COMMITTED result",
        ));
    }
    Ok(())
}

fn decode_committed_mutation(
    operation: &manager::DurableOperation,
    method: &str,
) -> AppResult<manager::ManagerMutation> {
    let Some(manager::durable_operation::TerminalSnapshot::ResultSnapshot(snapshot)) =
        operation.terminal_snapshot.as_ref()
    else {
        return Err(issue_error("COMMITTED operation has no result snapshot"));
    };
    if snapshot.schema == "issue_tracker.manager.v1.ManagerMutation" {
        return manager::ManagerMutation::decode(snapshot.payload.as_slice()).map_err(|error| {
            issue_error(format!("committed mutation snapshot is invalid: {error}"))
        });
    }
    match (method, snapshot.schema.as_str()) {
        ("manager_create_issue", "issue_tracker.manager.v1.CreateIssueResult") => {
            let response = manager::CreateIssueResult::decode(snapshot.payload.as_slice())
                .map_err(|error| {
                    issue_error(format!("committed create result is invalid: {error}"))
                })?;
            match response.outcome {
                Some(manager::create_issue_result::Outcome::Success(mutation)) => Ok(mutation),
                _ => Err(issue_error(
                    "COMMITTED create result has no successful outcome",
                )),
            }
        }
        ("manager_update_issue", "issue_tracker.manager.v1.UpdateIssueResult") => {
            let response = manager::UpdateIssueResult::decode(snapshot.payload.as_slice())
                .map_err(|error| {
                    issue_error(format!("committed update result is invalid: {error}"))
                })?;
            match response.outcome {
                Some(manager::update_issue_result::Outcome::Success(mutation)) => Ok(mutation),
                _ => Err(issue_error(
                    "COMMITTED update result has no successful outcome",
                )),
            }
        }
        (
            "manager_apply_issue_admin_action",
            "issue_tracker.manager.v1.ApplyIssueAdminActionResult",
        ) => {
            let response = manager::ApplyIssueAdminActionResult::decode(
                snapshot.payload.as_slice(),
            )
            .map_err(|error| {
                issue_error(format!("committed cancellation result is invalid: {error}"))
            })?;
            match response.outcome {
                Some(manager::apply_issue_admin_action_result::Outcome::Success(mutation)) => {
                    Ok(mutation)
                }
                _ => Err(issue_error(
                    "COMMITTED cancellation result has no successful outcome",
                )),
            }
        }
        _ => Err(issue_error(
            "COMMITTED operation result has an unexpected schema",
        )),
    }
}

fn mutation_result(
    mutation: &manager::ManagerMutation,
    kind: MutationKind,
    issue_id: &str,
    expected_revision: u64,
) -> AppResult<(u64, Option<String>)> {
    match kind {
        MutationKind::Create => {
            let Some(manager::manager_mutation::Result::IssueCreation(result)) =
                mutation.result.as_ref()
            else {
                return Err(issue_error("Committed create result has no Issue receipt"));
            };
            let Some(mapping) = result.mapping.as_ref() else {
                return Err(issue_error("Committed create result has no Issue mapping"));
            };
            if mapping.issue_id != issue_id
                || result
                    .issue
                    .as_ref()
                    .is_some_and(|issue| issue.issue_id != issue_id)
            {
                return Err(issue_error(
                    "Committed create result targets a different Issue",
                ));
            }
            if result.issue.as_ref().is_some_and(|issue| {
                v1::IssueState::try_from(issue.state).ok() != Some(v1::IssueState::Open)
            }) {
                return Err(issue_error(
                    "Committed create result does not have the requested OPEN state",
                ));
            }
            Ok((mapping.revision, Some("OPEN".into())))
        }
        MutationKind::Update => {
            let revision = mutation
                .entity_revisions
                .iter()
                .find(|entity| {
                    entity.entity_id == issue_id && entity.entity_kind.eq_ignore_ascii_case("issue")
                })
                .map(|entity| entity.revision)
                .or_else(|| match mutation.entity.as_ref() {
                    Some(manager::manager_mutation::Entity::IssueMapping(mapping))
                        if mapping.issue_id == issue_id =>
                    {
                        Some(mapping.revision)
                    }
                    _ => None,
                })
                .ok_or_else(|| issue_error("Committed update result has no Issue revision"))?;
            if revision < expected_revision {
                return Err(issue_error(
                    "Committed update result has an older Issue revision",
                ));
            }
            Ok((revision, None))
        }
        MutationKind::Cancel => {
            let Some(manager::manager_mutation::Result::IssueAdminAction(receipt)) =
                mutation.result.as_ref()
            else {
                return Err(issue_error("Committed cancel result has no admin receipt"));
            };
            if receipt.issue_id != issue_id
                || receipt.action != "CANCEL"
                || v1::IssueState::try_from(receipt.resulting_state).ok()
                    != Some(v1::IssueState::Canceled)
                || receipt.resulting_revision < expected_revision
            {
                return Err(issue_error(
                    "Committed cancel receipt does not match the requested cancellation",
                ));
            }
            Ok((receipt.resulting_revision, Some("CANCELED".into())))
        }
    }
}

fn verify_record_identity(
    service: &IssueTrackerGrpcService,
    record: &PendingOperationRecord,
) -> AppResult<()> {
    let current = manager_identity(service)?;
    if current.schema_fingerprint != record.schema_fingerprint
        || current.data_root_id != record.data_root_id
        || current.contract_fingerprint != record.contract_fingerprint
    {
        return Err(issue_error(
            "Pending operation belongs to a different manager identity; refusing replay",
        ));
    }
    Ok(())
}

fn verify_replay_context<'a>(
    context: Option<&'a manager::ManagerWriteContext>,
    record: &PendingOperationRecord,
    method: &str,
) -> AppResult<&'a manager::ManagerWriteContext> {
    let context = context.ok_or_else(|| issue_error("Pending request has no manager context"))?;
    let actor = context.actor_audit.as_ref();
    let actor_is_valid = match method {
        "manager_create_issue" => actor.is_some_and(|actor| {
            (actor.actor_id == HUMAN_AUDIT_ACTOR_ID
                && actor.actor_kind == v1::ActorKind::Human as i32
                && actor.agent_session_id.is_none())
                || (actor.actor_id == INTERNAL_AGENT_AUDIT_ACTOR_ID
                    && actor.actor_kind == v1::ActorKind::InternalAgent as i32
                    && actor.agent_session_id.as_deref() == Some(INTERNAL_AGENT_AUDIT_SESSION_ID))
        }),
        "manager_update_issue" | "manager_apply_issue_admin_action" => actor.is_some_and(|actor| {
            actor.actor_id == HUMAN_AUDIT_ACTOR_ID
                && actor.actor_kind == v1::ActorKind::Human as i32
                && actor.agent_session_id.is_none()
        }),
        _ => false,
    };
    if record.method != method
        || context.method != method
        || context.target != format!("issue:{}", record.issue_id)
        || context.idempotency_key != record.operation_key
        || context.correlation_id.trim().is_empty()
        || context.schema_fingerprint != record.schema_fingerprint
        || context.data_root_id != record.data_root_id
        || context.contract_fingerprint != record.contract_fingerprint
        || !actor_is_valid
    {
        return Err(issue_error(
            "Pending request context does not match its journal record",
        ));
    }
    Ok(context)
}

fn ensure_canonical_payload<T: Message>(payload: &T, original: &[u8]) -> AppResult<()> {
    if payload.encode_to_vec() != original {
        return Err(issue_error(
            "Pending request is not canonical; refusing a modified replay",
        ));
    }
    Ok(())
}

fn replay_request<T>(payload: T, context: &manager::ManagerWriteContext) -> ManagerWriteRequest<T> {
    ManagerWriteRequest {
        correlation_id: context.correlation_id.clone(),
        idempotency_key: context.idempotency_key.clone(),
        actor_audit: context.actor_audit.clone(),
        payload,
    }
}

fn issue_error(message: impl Into<String>) -> AppError {
    AppError::IssueTrackerServer(message.into())
}

fn map_client_error(error: crate::grpc::issue_tracker::IssueTrackerClientError) -> AppError {
    issue_error(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn create_input(actor_kind: IssueChatActorKind) -> CreateIssueInput {
        CreateIssueInput {
            speech_id: "speech-1".into(),
            tool_call_id: "call-1".into(),
            actor_kind,
            title: " A useful issue ".into(),
            description: " Detail ".into(),
            kind: IssueChatKind::Task,
            priority: IssueChatPriority::P2,
            confirmation_status: IssueChatConfirmationStatus::Confirmed,
            acceptance_criteria: vec!["Done means tested".into()],
            source_evidence: Vec::new(),
            rationale: "requested in chat".into(),
        }
    }

    fn update_input(patch: IssueChatPatchInput) -> UpdateIssueInput {
        UpdateIssueInput {
            speech_id: "speech-1".into(),
            tool_call_id: "call-1".into(),
            issue_id: "issue-1".into(),
            expected_revision: "0".into(),
            patch,
            rationale: "clarified in chat".into(),
        }
    }

    fn cancel_input(classification: IssueChatCancellationClassification) -> CancelIssueInput {
        CancelIssueInput {
            speech_id: "speech-1".into(),
            tool_call_id: "call-1".into(),
            issue_id: "issue-1".into(),
            expected_revision: "0".into(),
            classification,
            rationale: "not needed after comparison".into(),
            duplicate_issue_id: None,
        }
    }

    fn identity() -> ManagerIdentitySnapshot {
        ManagerIdentitySnapshot {
            schema_fingerprint: "schema".into(),
            data_root_id: "root".into(),
            contract_fingerprint: "contract".into(),
        }
    }

    #[test]
    fn human_create_allows_no_evidence_and_builds_an_explicit_confirmed_draft() {
        let input = validate_create_issue_input(create_input(IssueChatActorKind::Human)).unwrap();
        assert_eq!(input.issue_id.len(), 36);
        assert_eq!(&input.issue_id[14..15], "8");
        assert_eq!(&input.issue_id[19..20], "8");
        assert_eq!(
            input.issue_id.chars().filter(|char| *char == '-').count(),
            4
        );
        assert!(
            input
                .issue_id
                .chars()
                .all(|char| char.is_ascii_hexdigit() || char == '-')
        );
        assert_eq!(
            input.issue_id,
            validate_create_issue_input(create_input(IssueChatActorKind::Human))
                .unwrap()
                .issue_id
        );
        let request = build_create_manager_request(&input, &identity()).unwrap();
        let issue = request.payload.issue.as_ref().unwrap();

        assert_eq!(issue.title, "A useful issue");
        assert_eq!(issue.description, "Detail");
        assert_eq!(issue.creation_basis, v1::CreationBasis::Explicit as i32);
        assert_eq!(
            issue.confirmation_status,
            v1::ConfirmationStatus::Confirmed as i32
        );
        assert_eq!(issue.initial_state, v1::IssueState::Open as i32);
        assert!(issue.source_evidence.is_empty());
        assert!(issue.contexts.is_empty());
        assert_eq!(request.payload.issue_id, input.issue_id);
        assert_eq!(
            request.actor_audit.as_ref().unwrap().actor_id,
            HUMAN_AUDIT_ACTOR_ID
        );
        assert_eq!(
            request.actor_audit.as_ref().unwrap().reason.as_deref(),
            Some("利用者のチャット依頼による: requested in chat")
        );
        let context = request.payload.context.as_ref().unwrap();
        assert_eq!(context.method, "manager_create_issue");
        assert_eq!(context.target, format!("issue:{}", input.issue_id));
        assert_eq!(context.data_root_id, "root");

        let mut unreviewed = create_input(IssueChatActorKind::Human);
        unreviewed.confirmation_status = IssueChatConfirmationStatus::Unreviewed;
        let unreviewed = validate_create_issue_input(unreviewed).unwrap();
        let request = build_create_manager_request(&unreviewed, &identity()).unwrap();
        assert_eq!(
            request.payload.issue.unwrap().confirmation_status,
            v1::ConfirmationStatus::Unreviewed as i32
        );
    }

    #[test]
    fn internal_agent_create_requires_evidence_and_uses_fixed_agent_audit_identity() {
        let input = create_input(IssueChatActorKind::InternalAgent);
        assert!(validate_create_issue_input(input.clone()).is_err());

        let mut input = input;
        input.source_evidence.push(IssueChatSourceEvidenceInput {
            thread_id: "12".into(),
            memory_id: "34".into(),
        });
        let validated = validate_create_issue_input(input).unwrap();
        let request = build_create_manager_request(&validated, &identity()).unwrap();
        let actor = request.actor_audit.as_ref().unwrap();

        assert_eq!(actor.actor_kind, v1::ActorKind::InternalAgent as i32);
        assert_eq!(actor.actor_id, INTERNAL_AGENT_AUDIT_ACTOR_ID);
        assert_eq!(
            actor.agent_session_id.as_deref(),
            Some(INTERNAL_AGENT_AUDIT_SESSION_ID)
        );
        assert_eq!(
            request
                .payload
                .issue
                .as_ref()
                .unwrap()
                .source_evidence
                .len(),
            1
        );
    }

    #[test]
    fn create_validation_rejects_missing_title_and_malformed_evidence_ids() {
        let mut input = create_input(IssueChatActorKind::Human);
        input.title = " \n ".into();
        assert!(validate_create_issue_input(input).is_err());

        let mut input = create_input(IssueChatActorKind::InternalAgent);
        input.source_evidence.push(IssueChatSourceEvidenceInput {
            thread_id: "not-a-number".into(),
            memory_id: "34".into(),
        });
        assert!(validate_create_issue_input(input).is_err());
    }

    #[test]
    fn update_mask_contains_only_present_fields_and_empty_lists_are_explicit_clears() {
        let patch = IssueChatPatchInput {
            labels: Some(Vec::new()),
            confirmation_status: Some(IssueChatConfirmationStatus::Unreviewed),
            ..Default::default()
        };
        let validated = validate_update_issue_input(update_input(patch)).unwrap();
        let request = build_update_manager_request(&validated, &identity()).unwrap();

        assert_eq!(
            request.payload.update_mask,
            vec![
                manager::ManagerIssueField::Labels as i32,
                manager::ManagerIssueField::ConfirmationStatus as i32,
            ]
        );
        let patch = request.payload.patch.unwrap();
        assert!(patch.labels.is_empty());
        assert_eq!(
            patch.confirmation_status,
            Some(v1::ConfirmationStatus::Unreviewed as i32)
        );
        assert_eq!(request.payload.expected_revision, Some(0));
    }

    #[test]
    fn update_validation_rejects_empty_patch_blank_title_and_invalid_revision() {
        assert!(validate_update_issue_input(update_input(IssueChatPatchInput::default())).is_err());

        let patch = IssueChatPatchInput {
            title: Some("  ".into()),
            ..Default::default()
        };
        assert!(validate_update_issue_input(update_input(patch)).is_err());

        let mut request = update_input(IssueChatPatchInput {
            description: Some(String::new()),
            ..Default::default()
        });
        request.expected_revision = "-1".into();
        assert!(validate_update_issue_input(request).is_err());

        let mut request = update_input(IssueChatPatchInput {
            description: Some(String::new()),
            ..Default::default()
        });
        request.expected_revision = (u64::MAX as u128 + 1).to_string();
        assert!(validate_update_issue_input(request).is_err());

        let mut request = update_input(IssueChatPatchInput {
            description: Some(String::new()),
            ..Default::default()
        });
        request.expected_revision = u64::MAX.to_string();
        assert_eq!(
            validate_update_issue_input(request)
                .unwrap()
                .expected_revision,
            u64::MAX
        );
    }

    #[test]
    fn update_deserialization_rejects_unknown_fields_and_null_patch_values() {
        let unknown = json!({
            "speech_id": "s",
            "tool_call_id": "c",
            "issue_id": "i",
            "expected_revision": "0",
            "rationale": "reason",
            "patch": { "state": "DONE" }
        });
        assert!(serde_json::from_value::<UpdateIssueInput>(unknown).is_err());

        let null_patch = json!({
            "speech_id": "s",
            "tool_call_id": "c",
            "issue_id": "i",
            "expected_revision": "0",
            "rationale": "reason",
            "patch": { "labels": null }
        });
        assert!(serde_json::from_value::<UpdateIssueInput>(null_patch).is_err());

        let caller_actor = json!({
            "speech_id": "s",
            "tool_call_id": "c",
            "actor_kind": "HUMAN",
            "actor_id": "caller-controlled",
            "title": "title",
            "description": "description",
            "kind": "TASK",
            "priority": "P2",
            "confirmation_status": "CONFIRMED",
            "rationale": "reason"
        });
        assert!(serde_json::from_value::<CreateIssueInput>(caller_actor).is_err());
    }

    #[test]
    fn cancellation_requires_a_current_unclaimed_nonterminal_issue() {
        let input =
            validate_cancel_issue_input(cancel_input(IssueChatCancellationClassification::Other))
                .unwrap();
        assert!(
            validate_cancel_target(
                &input,
                &test_issue(v1::IssueState::Open, 0, v1::CreationBasis::Explicit, false),
                Some(&confirmed_sync()),
                None,
            )
            .is_ok()
        );
        assert!(
            validate_cancel_target(
                &input,
                &test_issue(v1::IssueState::Open, 1, v1::CreationBasis::Explicit, false),
                Some(&confirmed_sync()),
                None,
            )
            .is_err()
        );
        assert!(
            validate_cancel_target(
                &input,
                &test_issue(
                    v1::IssueState::InProgress,
                    0,
                    v1::CreationBasis::Explicit,
                    true
                ),
                Some(&confirmed_sync()),
                None,
            )
            .is_err()
        );
        assert!(
            validate_cancel_target(
                &input,
                &test_issue(v1::IssueState::Done, 0, v1::CreationBasis::Explicit, false),
                Some(&confirmed_sync()),
                None,
            )
            .is_err()
        );
        assert!(
            validate_cancel_target(
                &input,
                &test_issue(v1::IssueState::Open, 0, v1::CreationBasis::Explicit, false),
                Some(&manager::IssueSyncView {
                    sync_state: manager::SyncState::Pending as i32,
                    ..Default::default()
                }),
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn false_positive_is_only_for_auto_detected_issues_and_duplicates_use_other() {
        let false_positive = validate_cancel_issue_input(cancel_input(
            IssueChatCancellationClassification::FalsePositive,
        ))
        .unwrap();
        assert!(
            validate_cancel_target(
                &false_positive,
                &test_issue(
                    v1::IssueState::Open,
                    0,
                    v1::CreationBasis::AutoDetected,
                    false
                ),
                Some(&confirmed_sync()),
                None,
            )
            .is_ok()
        );
        assert!(
            validate_cancel_target(
                &false_positive,
                &test_issue(v1::IssueState::Open, 0, v1::CreationBasis::Explicit, false),
                Some(&confirmed_sync()),
                None,
            )
            .is_err()
        );

        let mut duplicate_input = cancel_input(IssueChatCancellationClassification::Other);
        duplicate_input.duplicate_issue_id = Some("issue-2".into());
        let duplicate = validate_cancel_issue_input(duplicate_input).unwrap();
        assert!(
            validate_cancel_target(
                &duplicate,
                &test_issue(v1::IssueState::Open, 0, v1::CreationBasis::Explicit, false),
                Some(&confirmed_sync()),
                Some("issue-2"),
            )
            .is_ok()
        );

        let mut invalid_duplicate =
            cancel_input(IssueChatCancellationClassification::FalsePositive);
        invalid_duplicate.duplicate_issue_id = Some("issue-2".into());
        assert!(validate_cancel_issue_input(invalid_duplicate).is_err());
    }

    #[test]
    fn duplicate_cancel_target_must_be_distinct_and_confirmed() {
        let mut input = cancel_input(IssueChatCancellationClassification::Other);
        input.duplicate_issue_id = Some("issue-1".into());
        assert!(validate_cancel_issue_input(input).is_err());

        let mut input = cancel_input(IssueChatCancellationClassification::Other);
        input.duplicate_issue_id = Some("issue-2".into());
        let validated = validate_cancel_issue_input(input).unwrap();
        let issue = test_issue(v1::IssueState::Open, 0, v1::CreationBasis::Explicit, false);
        assert!(validate_cancel_target(&validated, &issue, Some(&confirmed_sync()), None).is_err());
        let request = build_cancel_manager_request(&validated, &identity()).unwrap();
        let Some(manager::apply_issue_admin_action_args::TerminalReport::Cancellation(report)) =
            request.payload.terminal_report.as_ref()
        else {
            panic!("cancel request must include a cancellation report");
        };
        assert_eq!(report.classification, v1::CancellationClass::Other as i32);
    }

    #[test]
    fn operation_identity_is_deterministic_and_domain_separated() {
        let first = operation_identity("create", "ab", "c");
        assert_eq!(first, operation_identity("create", "ab", "c"));
        assert_ne!(first, operation_identity("create", "a", "bc"));
        assert_ne!(first, operation_identity("update", "ab", "c"));
        assert!(
            validate_create_issue_input(CreateIssueInput {
                speech_id: " ".into(),
                ..create_input(IssueChatActorKind::Human)
            })
            .is_err()
        );
    }

    #[test]
    fn uncommitted_durable_operations_never_validate_as_success() {
        let mut operation = manager::DurableOperation {
            operation_id: "operation-1".into(),
            scope: Some(manager::durable_operation::Scope::IssueId("issue-1".into())),
            method: "manager_create_issue".into(),
            state: manager::DurableOperationState::Pending as i32,
            ..Default::default()
        };
        assert!(
            validate_committed_operation(
                &operation,
                "operation-1",
                "manager_create_issue",
                "issue-1",
            )
            .is_err()
        );

        operation.state = manager::DurableOperationState::Committed as i32;
        assert!(
            validate_committed_operation(
                &operation,
                "operation-1",
                "manager_create_issue",
                "issue-1",
            )
            .is_ok()
        );
        assert!(
            validate_committed_operation(
                &operation,
                "other-operation",
                "manager_create_issue",
                "issue-1",
            )
            .is_err()
        );
        assert!(
            validate_committed_operation(
                &operation,
                "operation-1",
                "manager_update_issue",
                "issue-1",
            )
            .is_err()
        );
        assert!(
            validate_committed_operation(
                &operation,
                "operation-1",
                "manager_create_issue",
                "issue-2",
            )
            .is_err()
        );
    }

    #[test]
    fn marker_confirmed_empty_result_requires_matching_issue_read() {
        let empty = manager::DurableOperation {
            operation_id: "op-1".into(),
            method: "manager_update_issue".into(),
            scope: Some(manager::durable_operation::Scope::IssueId("issue-1".into())),
            state: manager::DurableOperationState::Committed as i32,
            terminal_snapshot: Some(
                manager::durable_operation::TerminalSnapshot::ResultSnapshot(manager::Snapshot {
                    schema: "issue_tracker.manager.v1.ManagerMutation".into(),
                    payload: vec![],
                }),
            ),
            ..Default::default()
        };
        assert!(is_marker_confirmed_empty_result(&empty));
        let committed = manager::IssueWithSync {
            issue: Some(v1::Issue {
                issue_id: "issue-1".into(),
                revision: 5,
                state: v1::IssueState::Open as i32,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            read_back_result(&committed, MutationKind::Update, "issue-1", 4)
                .unwrap()
                .0,
            5
        );
        assert_eq!(
            read_back_result(&committed, MutationKind::Create, "issue-1", 0)
                .unwrap()
                .1
                .as_deref(),
            Some("OPEN")
        );
        assert!(read_back_result(&committed, MutationKind::Update, "issue-1", 5).is_err());
        assert!(read_back_result(&committed, MutationKind::Cancel, "issue-1", 4).is_err());
        let mut canceled = committed.clone();
        canceled.issue.as_mut().unwrap().state = v1::IssueState::Canceled as i32;
        assert_eq!(
            read_back_result(&canceled, MutationKind::Cancel, "issue-1", 4)
                .unwrap()
                .1
                .as_deref(),
            Some("CANCELED")
        );
        let mut wrong_id = committed;
        wrong_id.issue.as_mut().unwrap().issue_id = "other".into();
        assert!(read_back_result(&wrong_id, MutationKind::Update, "issue-1", 4).is_err());
    }

    #[test]
    fn committed_create_result_requires_a_matching_typed_issue_receipt() {
        let create_mutation = manager::ManagerMutation {
            result: Some(manager::manager_mutation::Result::IssueCreation(
                manager::IssueCreationResult {
                    issue: Some(v1::Issue {
                        issue_id: "issue-1".into(),
                        state: v1::IssueState::Open as i32,
                        revision: 0,
                        ..Default::default()
                    }),
                    mapping: Some(manager::IssueMapping {
                        issue_id: "issue-1".into(),
                        revision: 0,
                        ..Default::default()
                    }),
                },
            )),
            ..Default::default()
        };
        let operation = manager::DurableOperation {
            terminal_snapshot: Some(
                manager::durable_operation::TerminalSnapshot::ResultSnapshot(manager::Snapshot {
                    schema: "issue_tracker.manager.v1.ManagerMutation".into(),
                    payload: create_mutation.encode_to_vec(),
                }),
            ),
            ..Default::default()
        };
        let decoded = decode_committed_mutation(&operation, "manager_create_issue").unwrap();
        assert_eq!(
            mutation_result(&decoded, MutationKind::Create, "issue-1", 0).unwrap(),
            (0, Some("OPEN".into()))
        );
        assert!(mutation_result(&decoded, MutationKind::Create, "issue-2", 0).is_err());
    }

    fn test_issue(
        state: v1::IssueState,
        revision: u64,
        creation_basis: v1::CreationBasis,
        claimed: bool,
    ) -> v1::Issue {
        v1::Issue {
            issue_id: "issue-1".into(),
            state: state as i32,
            revision,
            creation_basis: creation_basis as i32,
            claim: claimed.then_some(v1::ClaimView::default()),
            ..Default::default()
        }
    }

    fn confirmed_sync() -> manager::IssueSyncView {
        manager::IssueSyncView {
            sync_state: manager::SyncState::Confirmed as i32,
            ..Default::default()
        }
    }
}
