//! App-owned authorization for the separate AWAITING_REVIEW-to-DONE mutation.

use std::{
    collections::{BTreeSet, HashSet},
    fmt,
    sync::Arc,
};

use async_trait::async_trait;
use prost::Message;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::grpc::{
    issue_tracker::{IssueTrackerGrpcService, ManagerWriteRequest},
    proto::issue_tracker::{manager::v1 as manager, v1 as issue},
};

use super::model::{PrimaryOperationReceipt, PrimaryTrackerIdentity};
use super::{
    AgentRunRecord, CodingAgentError, CompletionActionKind, CompletionAuthorizationStatus,
    RunCompletionAuthorization, RunId, RunStore, ValidationState,
    store::{
        CompletionCandidate, NewCompletionAuthorization, completion_authorization_intent_matches,
        completion_manager_idempotency_key,
    },
};

const AUTO_COMPLETION_ACTOR_ID: &str = "lookback-coding-agent";
const HUMAN_REVIEW_ACTOR_ID: &str = "lookback-user";
const AUTO_POLICY_VERSION: u64 = 1;
const HUMAN_POLICY_IDENTITY: &str = "agent-app/human-review";
const MAX_AUDIT_REASON_BYTES: usize = 2_048;
const MAX_REPORT_TEXT_BYTES: usize = 4_096;
const MAX_REPORT_EVIDENCE: usize = 1_000;
const MANAGER_TRANSITION_METHOD: &str = "manager_transition_issue";
pub(super) const MANAGER_DURABLE_BACKEND_REQUEST_SCHEMA: &str =
    "issue_tracker.manager.v1.DurableBackendRequest";
const TRACKER_TRANSITION_METHOD: &str = "transition_issue";
const COMPLETION_OPERATION_HISTORY_PAGE_SIZE: u32 = 100;
const MAX_COMPLETION_OPERATION_HISTORY_PAGES: usize = 100;

pub type DurableCompletionReport = issue::CompletionReport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueState {
    Open,
    InProgress,
    Blocked,
    Deferred,
    Done,
    Canceled,
    AwaitingReview,
    Unknown,
}

impl IssueState {
    fn from_wire(value: i32) -> Self {
        match issue::IssueState::try_from(value) {
            Ok(issue::IssueState::Open) => Self::Open,
            Ok(issue::IssueState::InProgress) => Self::InProgress,
            Ok(issue::IssueState::Blocked) => Self::Blocked,
            Ok(issue::IssueState::Deferred) => Self::Deferred,
            Ok(issue::IssueState::Done) => Self::Done,
            Ok(issue::IssueState::Canceled) => Self::Canceled,
            Ok(issue::IssueState::AwaitingReview) => Self::AwaitingReview,
            _ => Self::Unknown,
        }
    }

    fn to_wire(self) -> i32 {
        match self {
            Self::Open => issue::IssueState::Open as i32,
            Self::InProgress => issue::IssueState::InProgress as i32,
            Self::Blocked => issue::IssueState::Blocked as i32,
            Self::Deferred => issue::IssueState::Deferred as i32,
            Self::Done => issue::IssueState::Done as i32,
            Self::Canceled => issue::IssueState::Canceled as i32,
            Self::AwaitingReview => issue::IssueState::AwaitingReview as i32,
            Self::Unknown => issue::IssueState::Unspecified as i32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitingReviewMarker {
    pub run_id: String,
    pub full_commit_id: String,
    pub evidence_references: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IssueHistoryEntry {
    pub issue_id: String,
    pub resulting_revision: u64,
    pub old_state: Option<IssueState>,
    pub new_state: IssueState,
    pub awaiting_review: Option<AwaitingReviewMarker>,
    pub completion: Option<DurableCompletionReport>,
    pub actor_id: String,
    pub actor_kind: String,
    pub correlation_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackerIssueSnapshot {
    pub issue_id: String,
    pub state: IssueState,
    pub revision: u64,
    /// Complete state history from a snapshot-pinned primary tracker read.
    pub history: Vec<IssueHistoryEntry>,
}

/// A stable, typed link from a canonical history revision to its backend
/// operation. Kept separate from the long-lived display model so legacy UI
/// wrappers cannot manufacture operation proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerHistoryOperation {
    pub issue_id: String,
    pub resulting_revision: u64,
    pub downstream_operation_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackerIssueOperationSnapshot {
    pub issue: TrackerIssueSnapshot,
    pub operations: Vec<TrackerHistoryOperation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackerMutationRequest {
    pub run_id: RunId,
    pub authorization_id: String,
    pub intent_sha256: String,
    pub primary_tracker_identity: PrimaryTrackerIdentity,
    pub issue_id: String,
    pub expected_revision: u64,
    pub target_state: IssueState,
    pub expected_manager_request_digest: String,
    /// Caller key is persisted locally; the manager receives its deterministic
    /// run-scoped derivation in `idempotency_key`.
    pub intent_idempotency_key: String,
    pub idempotency_key: String,
    pub actor_id: String,
    pub actor_kind: String,
    pub correlation_id: String,
    pub audit_reason: String,
    pub completion: Option<DurableCompletionReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TrackerError {
    #[error("primary Issue Tracker rejected the operation")]
    Rejected,
    #[error("primary Issue Tracker issue was not found")]
    NotFound,
    #[error("primary Issue Tracker operation is unconfirmed")]
    Unconfirmed,
    #[error("primary Issue Tracker is unavailable")]
    Unavailable,
}

pub type TrackerResult<T> = Result<T, TrackerError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimaryOperationLookup {
    Pending,
    Rejected(Option<PrimaryOperationReceipt>),
    Committed(PrimaryOperationReceipt),
    Unknown,
}

/// The only permitted reason to re-dispatch a recovered completion request.
/// This capability is local to a client implementation that pins a compatible
/// Manager contract; it is not learned from an untrusted response flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDispatchPermission {
    SamePayloadIdempotentManager,
}

#[async_trait]
#[allow(clippy::double_must_use)]
pub trait CompletionTracker: Send + Sync {
    async fn read_issue_with_history(&self, issue_id: &str) -> TrackerResult<TrackerIssueSnapshot>;
    async fn read_issue_with_operation_history(
        &self,
        issue_id: &str,
    ) -> TrackerResult<TrackerIssueOperationSnapshot> {
        Ok(TrackerIssueOperationSnapshot {
            issue: self.read_issue_with_history(issue_id).await?,
            operations: Vec::new(),
        })
    }
    fn primary_identity(&self) -> TrackerResult<PrimaryTrackerIdentity> {
        Err(TrackerError::Unavailable)
    }
    async fn transition_issue(&self, request: TrackerMutationRequest) -> TrackerResult<()>;
    /// No legacy transition fallback is allowed because it cannot return an
    /// operation identity suitable for durable recovery.
    async fn transition_issue_with_receipt(
        &self,
        _request: TrackerMutationRequest,
    ) -> TrackerResult<PrimaryOperationReceipt> {
        Err(TrackerError::Unavailable)
    }
    async fn lookup_transition_operation(
        &self,
        _authorization: &RunCompletionAuthorization,
    ) -> PrimaryOperationLookup {
        PrimaryOperationLookup::Unknown
    }
    fn recovery_dispatch_permission(
        &self,
        _authorization: &RunCompletionAuthorization,
    ) -> Option<RecoveryDispatchPermission> {
        None
    }
}

/// Primary Issue Tracker adapter; it uses manager reads/writes and the
/// snapshot-pinned ManagerTrustedReadService history, never Gateway claim capabilities.
pub struct PrimaryIssueTrackerAdapter {
    service: IssueTrackerGrpcService,
}

impl PrimaryIssueTrackerAdapter {
    pub fn new(service: IssueTrackerGrpcService) -> Self {
        Self { service }
    }
}

#[async_trait]
impl CompletionTracker for PrimaryIssueTrackerAdapter {
    async fn read_issue_with_history(&self, issue_id: &str) -> TrackerResult<TrackerIssueSnapshot> {
        self.read_primary_issue_with_operation_history(issue_id)
            .await
            .map(|snapshot| snapshot.issue)
    }

    async fn read_issue_with_operation_history(
        &self,
        issue_id: &str,
    ) -> TrackerResult<TrackerIssueOperationSnapshot> {
        self.read_primary_issue_with_operation_history(issue_id)
            .await
    }

    async fn transition_issue(&self, request: TrackerMutationRequest) -> TrackerResult<()> {
        self.transition_issue_with_receipt(request)
            .await
            .map(|_| ())
    }

    async fn transition_issue_with_receipt(
        &self,
        request: TrackerMutationRequest,
    ) -> TrackerResult<PrimaryOperationReceipt> {
        let server_identity = self
            .service
            .primary_tracker_identity()
            .map_err(|_| TrackerError::Unconfirmed)?;
        let backend_contract_fingerprint = self
            .service
            .primary_backend_contract_fingerprint()
            .map_err(|_| TrackerError::Unconfirmed)?;
        if server_identity != request.primary_tracker_identity {
            return Err(TrackerError::Unconfirmed);
        }
        if !authorization_intent_matches_request(&request) {
            return Err(TrackerError::Unconfirmed);
        }
        let write = ManagerWriteRequest {
            correlation_id: request.correlation_id.clone(),
            idempotency_key: request.idempotency_key.clone(),
            actor_audit: Some(manager::ActorAudit {
                actor_id: request.actor_id.clone(),
                actor_kind: wire_actor_kind(&request.actor_kind),
                display_name: Some("Lookback coding agent".into()),
                reason: Some(request.audit_reason.clone()),
                ..Default::default()
            }),
            payload: manager_transition_args(&request),
        };
        let exact_payload = self
            .service
            .encode_transition_issue(write.clone())
            .map_err(|_| TrackerError::Unconfirmed)?;
        if sha256(&exact_payload) != request.expected_manager_request_digest {
            return Err(TrackerError::Unconfirmed);
        }
        let mutation = self
            .service
            .transition_issue(write)
            .await
            .map_err(|_| TrackerError::Unconfirmed)?;
        let operation = mutation.operation.ok_or(TrackerError::Unconfirmed)?;
        manager_operation_receipt(
            &operation,
            &request,
            server_identity,
            backend_contract_fingerprint,
        )
    }

    fn primary_identity(&self) -> TrackerResult<PrimaryTrackerIdentity> {
        self.service
            .primary_tracker_identity()
            .map_err(|_| TrackerError::Unconfirmed)
    }

    async fn lookup_transition_operation(
        &self,
        authorization: &RunCompletionAuthorization,
    ) -> PrimaryOperationLookup {
        self.lookup_transition_operation_inner(authorization)
            .await
            .unwrap_or(PrimaryOperationLookup::Unknown)
    }

    fn recovery_dispatch_permission(
        &self,
        authorization: &RunCompletionAuthorization,
    ) -> Option<RecoveryDispatchPermission> {
        let identity = authorization.primary_tracker_identity.as_ref()?;
        if authorization.primary_operation.is_some()
            || !completion_authorization_intent_matches(authorization)
            || !self.service.supports_same_payload_manager_replay(identity)
        {
            return None;
        }
        Some(RecoveryDispatchPermission::SamePayloadIdempotentManager)
    }
}

impl PrimaryIssueTrackerAdapter {
    async fn read_primary_issue_with_operation_history(
        &self,
        issue_id: &str,
    ) -> TrackerResult<TrackerIssueOperationSnapshot> {
        let before = self.read_issue(issue_id).await?;
        let history = self
            .service
            .list_trusted_issue_history("coding-agent-completion-history", issue_id)
            .await
            .map_err(|_| TrackerError::Unconfirmed)?;
        let after = self.read_issue(issue_id).await?;
        if before.issue_id != after.issue_id
            || before.revision != after.revision
            || before.state != after.state
        {
            return Err(TrackerError::Unconfirmed);
        }
        let mapped = history
            .into_iter()
            .filter_map(map_history_entry)
            .collect::<TrackerResult<Vec<_>>>()?;
        let operations = mapped
            .iter()
            .map(|(entry, downstream_operation_id)| TrackerHistoryOperation {
                issue_id: entry.issue_id.clone(),
                resulting_revision: entry.resulting_revision,
                downstream_operation_id: downstream_operation_id.clone(),
            })
            .collect();
        let history = mapped.into_iter().map(|(entry, _)| entry).collect();
        Ok(TrackerIssueOperationSnapshot {
            issue: TrackerIssueSnapshot {
                issue_id: after.issue_id,
                state: after.state,
                revision: after.revision,
                history,
            },
            operations,
        })
    }

    async fn read_issue(&self, issue_id: &str) -> TrackerResult<TrackerIssueSnapshot> {
        let result = self
            .service
            .get_issue_with_sync(
                "coding-agent-completion-current-issue",
                manager::GetIssueWithSyncArgs {
                    issue_id: issue_id.into(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|_| TrackerError::Unconfirmed)?;
        let issue = result.issue.ok_or(TrackerError::NotFound)?;
        if issue.issue_id != issue_id || issue.revision == 0 {
            return Err(TrackerError::Unconfirmed);
        }
        Ok(TrackerIssueSnapshot {
            issue_id: issue.issue_id,
            state: IssueState::from_wire(issue.state),
            revision: issue.revision,
            history: Vec::new(),
        })
    }

    async fn lookup_transition_operation_inner(
        &self,
        authorization: &RunCompletionAuthorization,
    ) -> TrackerResult<PrimaryOperationLookup> {
        let expected_identity = authorization
            .primary_tracker_identity
            .as_ref()
            .ok_or(TrackerError::Unconfirmed)?;
        let current_identity = self.primary_identity()?;
        if &current_identity != expected_identity {
            return Ok(PrimaryOperationLookup::Unknown);
        }
        let backend_contract_fingerprint = self
            .service
            .primary_backend_contract_fingerprint()
            .map_err(|_| TrackerError::Unconfirmed)?;

        if let Some(expected_receipt) = &authorization.primary_operation {
            if &expected_receipt.server_identity != expected_identity {
                return Ok(PrimaryOperationLookup::Unknown);
            }
            let operation = self
                .service
                .get_operation(
                    &completion_correlation(authorization),
                    expected_receipt.manager_operation_id.clone(),
                )
                .await
                .map_err(|_| TrackerError::Unconfirmed)?;
            let Some(receipt) = manager_operation_receipt_for_authorization(
                &operation,
                authorization,
                expected_identity,
                &backend_contract_fingerprint,
            ) else {
                return Ok(PrimaryOperationLookup::Unknown);
            };
            if &receipt != expected_receipt {
                return Ok(PrimaryOperationLookup::Unknown);
            }
            return self
                .classify_manager_operation(operation, receipt, authorization)
                .await;
        }

        let mut page_token = None;
        let mut seen_tokens = HashSet::new();
        let mut history_snapshot = None;
        let mut page_count = 0usize;
        loop {
            if page_count >= MAX_COMPLETION_OPERATION_HISTORY_PAGES {
                return Ok(PrimaryOperationLookup::Unknown);
            }
            page_count += 1;
            let page = self
                .service
                .list_operation_history(
                    &completion_correlation(authorization),
                    completion_operation_history_args(&authorization.issue_id, page_token.clone()),
                )
                .await
                .map_err(|_| TrackerError::Unconfirmed)?;
            let page_info = page.page.ok_or(TrackerError::Unconfirmed)?;
            let current_snapshot = page_info.snapshot.ok_or(TrackerError::Unconfirmed)?;
            if current_snapshot.snapshot_id.trim().is_empty()
                || history_snapshot
                    .as_ref()
                    .is_some_and(|previous| previous != &current_snapshot)
            {
                return Ok(PrimaryOperationLookup::Unknown);
            }
            history_snapshot = Some(current_snapshot);
            for entry in page.entries {
                if entry.correlation_id != completion_correlation(authorization) {
                    continue;
                }
                let operation = self
                    .service
                    .get_operation(&completion_correlation(authorization), entry.operation_id)
                    .await
                    .map_err(|_| TrackerError::Unconfirmed)?;
                if !manager_operation_has_expected_idempotency_key(&operation, authorization) {
                    return Ok(PrimaryOperationLookup::Unknown);
                }
                let Some(receipt) = manager_operation_receipt_for_authorization(
                    &operation,
                    authorization,
                    expected_identity,
                    &backend_contract_fingerprint,
                ) else {
                    return Ok(PrimaryOperationLookup::Unknown);
                };
                return self
                    .classify_manager_operation(operation, receipt, authorization)
                    .await;
            }
            page_token = page_info.next_page_token;
            let Some(token) = page_token.as_ref() else {
                break;
            };
            if token.is_empty() || !seen_tokens.insert(token.clone()) {
                return Ok(PrimaryOperationLookup::Unknown);
            }
        }
        // Operation history is not an idempotency-reservation index. A
        // reservation can exist before an operation row is inserted, so an
        // empty complete history cannot authorize another write attempt.
        Ok(PrimaryOperationLookup::Unknown)
    }

    async fn classify_manager_operation(
        &self,
        operation: manager::DurableOperation,
        receipt: PrimaryOperationReceipt,
        authorization: &RunCompletionAuthorization,
    ) -> TrackerResult<PrimaryOperationLookup> {
        use manager::DurableOperationState as State;

        match State::try_from(operation.state).ok() {
            Some(State::Received | State::Pending) => Ok(PrimaryOperationLookup::Pending),
            Some(State::Failed) if manager_operation_has_terminal_rejection(&operation) => {
                Ok(PrimaryOperationLookup::Rejected(Some(receipt)))
            }
            Some(State::Failed) => Ok(PrimaryOperationLookup::Unknown),
            Some(State::Committed) => {
                if !manager_operation_commit_is_consistent(&operation) {
                    return Ok(PrimaryOperationLookup::Unknown);
                }
                let trusted = self
                    .service
                    .get_trusted_primary_operation(
                        &completion_correlation(authorization),
                        &authorization.issue_id,
                        &operation.operation_id,
                        &receipt.downstream_operation_id,
                    )
                    .await
                    .map_err(|_| TrackerError::Unconfirmed)?;
                let Some(record) = trusted_downstream_record_bound_to_manager(
                    &trusted,
                    &operation,
                    &authorization.issue_id,
                ) else {
                    return Ok(PrimaryOperationLookup::Unknown);
                };
                if !primary_operation_record_matches_authorization(record, &receipt, authorization)
                {
                    return Ok(PrimaryOperationLookup::Unknown);
                }
                Ok(PrimaryOperationLookup::Committed(receipt))
            }
            Some(
                State::Unconfirmed
                | State::ManualResolution
                | State::Arbitrated
                | State::Unspecified,
            )
            | None => Ok(PrimaryOperationLookup::Unknown),
        }
    }
}

fn trusted_downstream_record_bound_to_manager<'a>(
    trusted: &'a manager::TrustedDownstreamOperation,
    manager_operation: &manager::DurableOperation,
    expected_issue_id: &str,
) -> Option<&'a issue::OperationRecord> {
    let binding = trusted.binding.as_ref()?;
    if manager_operation.operation_id.trim().is_empty()
        || manager_operation.downstream_operation_id.trim().is_empty()
        || binding.manager_operation_id != manager_operation.operation_id
        || binding.issue_id != expected_issue_id
        || binding.downstream_operation_id != manager_operation.downstream_operation_id
        || binding.manager_request_digest != manager_operation.request_digest
        || binding.manager_request_schema != manager_operation.request_schema
    {
        return None;
    }
    trusted.operation.as_ref()
}

fn completion_operation_history_args(
    issue_id: &str,
    page_token: Option<String>,
) -> manager::ListOperationHistoryArgs {
    manager::ListOperationHistoryArgs {
        page: Some(manager::PageRequest {
            page_size: COMPLETION_OPERATION_HISTORY_PAGE_SIZE,
            page_token,
        }),
        filter: Some(manager::OperationHistoryFilter {
            direct_target: Some(manager::operation_history_filter::DirectTarget::IssueId(
                issue_id.into(),
            )),
            method: Some(MANAGER_TRANSITION_METHOD.into()),
            direct_scope_only: true,
            ..Default::default()
        }),
        include_total_size: Some(false),
        ..Default::default()
    }
}

fn map_history_entry(
    entry: issue::IssueHistoryEntry,
) -> Option<TrackerResult<(IssueHistoryEntry, String)>> {
    let state = entry.state_change?;
    let new_state = IssueState::from_wire(state.new_state);
    if new_state == IssueState::Unknown {
        return Some(Err(TrackerError::Unconfirmed));
    }
    let actor = entry.actor.and_then(|actor| actor.actor);
    let actor_kind = actor
        .as_ref()
        .map(|actor| match issue::ActorKind::try_from(actor.kind) {
            Ok(issue::ActorKind::System) => "system",
            Ok(issue::ActorKind::Human) => "human",
            Ok(issue::ActorKind::InternalAgent) => "internal_agent",
            Ok(issue::ActorKind::ExternalAgent) => "external_agent",
            _ => "unknown",
        })
        .unwrap_or("unknown")
        .to_owned();
    let awaiting_review = state.awaiting_review.map(|marker| AwaitingReviewMarker {
        run_id: marker.run_id,
        full_commit_id: marker.full_commit_id,
        evidence_references: marker.evidence_references,
    });
    let downstream_operation_id = entry.downstream_operation_id;
    Some(Ok((
        IssueHistoryEntry {
            issue_id: entry.issue_id,
            resulting_revision: entry.resulting_revision,
            old_state: state.old_state.map(IssueState::from_wire),
            new_state,
            awaiting_review,
            completion: state.completion,
            actor_id: actor.map(|actor| actor.actor_id).unwrap_or_default(),
            actor_kind,
            correlation_id: entry.correlation_id,
            reason: entry.reason,
        },
        downstream_operation_id,
    )))
}

fn wire_actor_kind(kind: &str) -> i32 {
    match kind {
        "system" => issue::ActorKind::System as i32,
        "human" => issue::ActorKind::Human as i32,
        _ => issue::ActorKind::Unspecified as i32,
    }
}

fn manager_actor_audit(request: &TrackerMutationRequest) -> manager::ActorAudit {
    manager::ActorAudit {
        actor_id: request.actor_id.clone(),
        actor_kind: wire_actor_kind(&request.actor_kind),
        display_name: Some("Lookback coding agent".into()),
        reason: Some(request.audit_reason.clone()),
        ..Default::default()
    }
}

fn manager_transition_args(request: &TrackerMutationRequest) -> manager::TransitionIssueArgs {
    manager::TransitionIssueArgs {
        context: Some(manager::ManagerWriteContext {
            method: MANAGER_TRANSITION_METHOD.into(),
            target: format!("issue:{}", request.issue_id),
            schema_fingerprint: request.primary_tracker_identity.schema_fingerprint.clone(),
            idempotency_key: request.idempotency_key.clone(),
            actor_audit: Some(manager_actor_audit(request)),
            correlation_id: request.correlation_id.clone(),
            data_root_id: request.primary_tracker_identity.data_root_id.clone(),
            contract_fingerprint: request
                .primary_tracker_identity
                .contract_fingerprint
                .clone(),
        }),
        issue_id: request.issue_id.clone(),
        expected_revision: Some(request.expected_revision),
        target_state: request.target_state.to_wire(),
        terminal_report: request
            .completion
            .clone()
            .map(manager::transition_issue_args::TerminalReport::Completion),
        ..Default::default()
    }
}

/// Reconstruct the exact typed manager payload whose bytes the server hashes
/// during admission. `request_schema` on the returned durable operation names
/// its outbox envelope, not this manager RPC argument type.
pub(super) fn manager_request_digest_for_authorization(
    authorization: &RunCompletionAuthorization,
) -> Result<String, CodingAgentError> {
    let target_state = match authorization.action {
        CompletionActionKind::HumanCorrection => IssueState::Open,
        CompletionActionKind::AutoDone | CompletionActionKind::HumanDone => IssueState::Done,
    };
    let actor_kind = if authorization.action == CompletionActionKind::AutoDone {
        "system"
    } else {
        "human"
    };
    let completion = if authorization.completion_report_protobuf.is_empty() {
        None
    } else {
        Some(
            decode_report(&authorization.completion_report_protobuf).map_err(|_| {
                CodingAgentError::Invalid("invalid stored completion report".into())
            })?,
        )
    };
    let request = TrackerMutationRequest {
        run_id: authorization.run_id.clone(),
        authorization_id: authorization.authorization_id.clone(),
        intent_sha256: authorization.intent_sha256.clone(),
        primary_tracker_identity: authorization
            .primary_tracker_identity
            .clone()
            .ok_or_else(|| CodingAgentError::Invalid("missing primary tracker identity".into()))?,
        issue_id: authorization.issue_id.clone(),
        expected_revision: authorization.target_issue_revision,
        target_state,
        expected_manager_request_digest: String::new(),
        intent_idempotency_key: authorization.intent_idempotency_key.clone(),
        idempotency_key: completion_manager_idempotency_key(
            &authorization.run_id,
            &authorization.intent_idempotency_key,
        ),
        actor_id: authorization.actor_id.clone(),
        actor_kind: actor_kind.into(),
        correlation_id: completion_correlation(authorization),
        audit_reason: authorization.audit_reason.clone(),
        completion,
    };
    Ok(sha256(&manager_transition_args(&request).encode_to_vec()))
}

fn authorization_intent_matches_request(request: &TrackerMutationRequest) -> bool {
    valid_sha256(&request.intent_sha256)
        && valid_sha256(&request.expected_manager_request_digest)
        && sha256(&manager_transition_args(request).encode_to_vec())
            == request.expected_manager_request_digest
        && request.audit_reason.ends_with(&format!(
            " [coding-agent-intent-sha256={}]",
            request.intent_sha256
        ))
        && request.idempotency_key
            == completion_manager_idempotency_key(&request.run_id, &request.intent_idempotency_key)
        && request.correlation_id == format!("coding-agent-completion-{}", request.authorization_id)
}

fn manager_operation_receipt(
    operation: &manager::DurableOperation,
    request: &TrackerMutationRequest,
    server_identity: PrimaryTrackerIdentity,
    backend_contract_fingerprint: String,
) -> TrackerResult<PrimaryOperationReceipt> {
    if !manager_operation_matches_request(operation, request, &server_identity) {
        return Err(TrackerError::Unconfirmed);
    }
    let audit = operation.audit.as_ref().ok_or(TrackerError::Unconfirmed)?;
    Ok(PrimaryOperationReceipt {
        manager_operation_id: operation.operation_id.clone(),
        downstream_operation_id: operation.downstream_operation_id.clone(),
        manager_idempotency_key: audit.idempotency_key.clone(),
        idempotency_scope: operation.idempotency_scope.clone(),
        request_digest: operation.request_digest.clone(),
        request_schema: operation.request_schema.clone(),
        backend_contract_fingerprint,
        manager_sequence: operation.manager_sequence,
        generation: operation.generation,
        server_identity: PrimaryTrackerIdentity {
            schema_fingerprint: audit.schema_fingerprint.clone(),
            data_root_id: audit.data_root_id.clone(),
            contract_fingerprint: audit.contract_fingerprint.clone(),
        },
    })
}

fn manager_operation_receipt_for_authorization(
    operation: &manager::DurableOperation,
    authorization: &RunCompletionAuthorization,
    server_identity: &PrimaryTrackerIdentity,
    backend_contract_fingerprint: &str,
) -> Option<PrimaryOperationReceipt> {
    let request = mutation_request(
        authorization,
        match authorization.action {
            CompletionActionKind::HumanCorrection => IssueState::Open,
            CompletionActionKind::AutoDone | CompletionActionKind::HumanDone => IssueState::Done,
        },
        &authorization.actor_id,
        if authorization.action == CompletionActionKind::AutoDone {
            "system"
        } else {
            "human"
        },
        if authorization.completion_report_protobuf.is_empty() {
            None
        } else {
            decode_report(&authorization.completion_report_protobuf).ok()
        },
    )
    .ok()?;
    manager_operation_receipt(
        operation,
        &request,
        server_identity.clone(),
        backend_contract_fingerprint.into(),
    )
    .ok()
}

fn manager_operation_matches_request(
    operation: &manager::DurableOperation,
    request: &TrackerMutationRequest,
    expected_identity: &PrimaryTrackerIdentity,
) -> bool {
    use manager::durable_operation::Scope;

    let Some(audit) = operation.audit.as_ref() else {
        return false;
    };
    let Some(actor) = audit.actor_audit.as_ref() else {
        return false;
    };
    let scope_matches = matches!(
        operation.scope.as_ref(),
        Some(Scope::IssueId(issue_id)) if issue_id == &request.issue_id
    );
    !operation.operation_id.trim().is_empty()
        && !operation.downstream_operation_id.trim().is_empty()
        && operation.idempotency_scope
            == completion_idempotency_scope(
                expected_identity,
                &request.issue_id,
                &request.idempotency_key,
            )
        && operation.request_digest == request.expected_manager_request_digest
        && operation.request_schema == MANAGER_DURABLE_BACKEND_REQUEST_SCHEMA
        && operation.method == MANAGER_TRANSITION_METHOD
        && operation.generation > 0
        && scope_matches
        && audit.method == MANAGER_TRANSITION_METHOD
        && audit.target == format!("issue:{}", request.issue_id)
        && audit.idempotency_key == request.idempotency_key
        && audit.correlation_id == request.correlation_id
        && audit.schema_fingerprint == expected_identity.schema_fingerprint
        && audit.data_root_id == expected_identity.data_root_id
        && audit.contract_fingerprint == expected_identity.contract_fingerprint
        && actor.actor_id == request.actor_id
        && actor.actor_kind == wire_actor_kind(&request.actor_kind)
        && actor.agent_session_id.is_none()
        && actor.reason.as_deref() == Some(request.audit_reason.as_str())
        && actor.display_name.as_deref() == Some("Lookback coding agent")
        && request.primary_tracker_identity == *expected_identity
        && authorization_intent_matches_request(request)
}

pub(super) fn completion_idempotency_scope(
    identity: &PrimaryTrackerIdentity,
    issue_id: &str,
    key: &str,
) -> String {
    fn append(value: &mut String, part: &str) {
        use std::fmt::Write;
        let _ = write!(value, "{}:", part.len());
        value.push_str(part);
    }

    let mut scope = String::new();
    for component in [
        identity.contract_fingerprint.as_str(),
        identity.data_root_id.as_str(),
        MANAGER_TRANSITION_METHOD,
    ] {
        append(&mut scope, component);
    }
    append(&mut scope, &format!("issue:{issue_id}"));
    append(&mut scope, key);
    scope
}

fn manager_operation_has_expected_idempotency_key(
    operation: &manager::DurableOperation,
    authorization: &RunCompletionAuthorization,
) -> bool {
    operation.audit.as_ref().is_some_and(|audit| {
        audit.idempotency_key
            == completion_manager_idempotency_key(
                &authorization.run_id,
                &authorization.intent_idempotency_key,
            )
    })
}

fn manager_operation_commit_is_consistent(operation: &manager::DurableOperation) -> bool {
    use manager::{DurableOperationState, durable_operation::TerminalSnapshot};

    if manager::DurableOperationState::try_from(operation.state).ok()
        != Some(DurableOperationState::Committed)
    {
        return false;
    }
    let Some(TerminalSnapshot::ResultSnapshot(snapshot)) = operation.terminal_snapshot.as_ref()
    else {
        return false;
    };
    if operation.commit_durability != issue::OperationCommitDurability::Durable as i32
        || operation.result_replayability != issue::OperationResultReplayability::Replayable as i32
        || operation.resolution_required
        || operation.commit_snapshot.is_some()
        || operation.failure_class != manager::FailureClass::Unspecified as i32
    {
        return false;
    }
    if snapshot.schema != "issue_tracker.manager.v1.ManagerMutation" {
        return false;
    }
    let Ok(confirmed_transition_result) =
        manager::ManagerMutation::decode(snapshot.payload.as_slice())
    else {
        return false;
    };
    // The server's confirmed_backend_mutation removes the operation from its
    // admission snapshot and transitions have no method-specific manager
    // result. The resulting canonical success payload is ManagerMutation::default()
    // (currently zero bytes), not a synthesized ManagerCommitSnapshot.
    confirmed_transition_result == manager::ManagerMutation::default()
        && confirmed_transition_result.encode_to_vec() == snapshot.payload
}

fn manager_operation_has_terminal_rejection(operation: &manager::DurableOperation) -> bool {
    use manager::{DurableOperationState, FailureClass, durable_operation::TerminalSnapshot};

    if operation.state != DurableOperationState::Failed as i32
        || operation.failure_class != FailureClass::Terminal as i32
        || operation.affects_ready
        || operation.resolution_required
        || operation.commit_snapshot.is_some()
        || operation.commit_durability != issue::OperationCommitDurability::Durable as i32
        || operation.result_replayability != issue::OperationResultReplayability::Replayable as i32
        || operation.generation == 0
    {
        return false;
    }
    let Some(TerminalSnapshot::ErrorSnapshot(snapshot)) = operation.terminal_snapshot.as_ref()
    else {
        return false;
    };
    if snapshot.schema != "issue_tracker.manager.v1.ManagerError" {
        return false;
    }
    let Ok(error) = manager::ManagerError::decode(snapshot.payload.as_slice()) else {
        return false;
    };
    match manager::ManagerErrorCode::try_from(error.code) {
        // This allowlist is the manager-side mapping of the actual definitive
        // TaskChampion typed-error set. Unknown future enum values and every
        // retryable/uncertain code remain fenced.
        Ok(
            manager::ManagerErrorCode::ManagerInvalidArgument
            | manager::ManagerErrorCode::IssueNotFound
            | manager::ManagerErrorCode::RevisionConflict
            | manager::ManagerErrorCode::IdempotencyConflict
            | manager::ManagerErrorCode::InvalidStateTransition
            | manager::ManagerErrorCode::PreconditionFailed
            | manager::ManagerErrorCode::ManagerSecretDetected,
        ) => !error.retryable,
        // The only retry-exhausted terminal shape emitted by the server proves
        // every delivery attempt was not sent. Validate that structured proof,
        // rather than treating the enum value itself as a rejection.
        Ok(manager::ManagerErrorCode::RetryExhausted) => {
            error.retryable
                && matches!(
                    error.detail,
                    Some(manager::manager_error::Detail::OperationDiagnosis(diagnosis))
                        if diagnosis.operation_id == operation.operation_id
                            && diagnosis.classification == "NOT_SENT_RETRY_EXHAUSTED"
                            && diagnosis.evidence_ref == operation.operation_id
                            && diagnosis.replayability == "NEW_READMISSION_ONLY"
                            && diagnosis.resolution_state == "ALL_ATTEMPTS_NOT_SENT"
                            && diagnosis.resolution_operation_id.is_none()
                )
        }
        _ => false,
    }
}

fn primary_operation_record_matches_authorization(
    operation: &issue::OperationRecord,
    receipt: &PrimaryOperationReceipt,
    authorization: &RunCompletionAuthorization,
) -> bool {
    use issue::{OperationScope, operation_result_snapshot::Value};

    let expected_revision = authorization.target_issue_revision.checked_add(1);
    let Some(fingerprint) = operation.fingerprint_input.as_ref() else {
        return false;
    };
    let expected_payload_fingerprint = sha256(
        format!(
            "{TRACKER_TRANSITION_METHOD}\u{1f}{}",
            receipt.downstream_operation_id
        )
        .as_bytes(),
    );
    if operation.downstream_operation_id != receipt.downstream_operation_id
        || operation.scope != OperationScope::Issue as i32
        || operation.contract_fingerprint != receipt.backend_contract_fingerprint
        || operation.resulting_revision != expected_revision
        || operation.affected_issue_ids.len() != 1
        || operation.affected_issue_ids.first() != Some(&authorization.issue_id)
        || operation.method_key != TRACKER_TRANSITION_METHOD
        || operation.payload_fingerprint != expected_payload_fingerprint
        || operation.commit_durability != issue::OperationCommitDurability::Durable as i32
        || operation.result_replayability != issue::OperationResultReplayability::Replayable as i32
        || fingerprint.downstream_operation_id != receipt.downstream_operation_id
        || fingerprint.scope != operation.scope
        || fingerprint.method_key != TRACKER_TRANSITION_METHOD
        || fingerprint.payload_fingerprint != operation.payload_fingerprint
    {
        return false;
    }
    let Some(snapshot) = operation.result_snapshot.as_ref() else {
        return false;
    };
    let Some(Value::IssueMutation(mutation)) = snapshot.value.as_ref() else {
        return false;
    };
    let Some(details) = mutation.current.as_ref() else {
        return false;
    };
    let Some(current) = details.issue.as_ref() else {
        return false;
    };
    current.issue_id == authorization.issue_id
        && current.revision == expected_revision.unwrap_or_default()
        && IssueState::from_wire(current.state)
            == if authorization.action == CompletionActionKind::HumanCorrection {
                IssueState::Open
            } else {
                IssueState::Done
            }
        && mutation.previous_state == Some(issue::IssueState::AwaitingReview as i32)
}

/// Immutable app-owned manager policy. It is constructed when application
/// configuration is loaded, never from an Issue, UI payload, or model call.
pub struct CompletionPolicy {
    identity: String,
    version: u64,
}

impl CompletionPolicy {
    pub fn new(identity: impl Into<String>, version: u64) -> Result<Self, CompletionError> {
        let identity = identity.into();
        if identity.trim().is_empty()
            || identity.len() > 256
            || identity.chars().any(char::is_control)
            || version == 0
        {
            return Err(CompletionError::InvalidPolicy);
        }
        Ok(Self { identity, version })
    }
}

struct CompletionAuthorityInner {
    store: Arc<RunStore>,
    tracker: Arc<dyn CompletionTracker>,
    policy: CompletionPolicy,
    instance_id: String,
}

#[derive(Clone)]
pub struct CompletionAuthority {
    inner: Arc<CompletionAuthorityInner>,
}

pub(crate) fn auto_completion_intent_key(run_id: &str, attempt_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"coding-agent-auto-done-v1");
    for identifier in [run_id, attempt_id] {
        digest.update((identifier.len() as u64).to_be_bytes());
        digest.update(identifier.as_bytes());
    }
    format!("coding-agent-auto-done-{:x}", digest.finalize())
}

impl CompletionAuthority {
    pub fn new(
        store: Arc<RunStore>,
        tracker: Arc<dyn CompletionTracker>,
        policy: CompletionPolicy,
    ) -> Result<Self, CompletionError> {
        Ok(Self {
            inner: Arc::new(CompletionAuthorityInner {
                store,
                tracker,
                policy,
                instance_id: RunId::generate()
                    .map_err(|error| CompletionError::Store(CodingAgentError::Model(error)))?
                    .to_string(),
            }),
        })
    }

    /// Independent candidates must not be starved by a different Issue's
    /// conflict, missing evidence, or unconfirmed mutation.
    pub async fn reconcile_approved_auto_completions(&self, run_ids: &[RunId]) -> usize {
        let mut completed = 0;
        for run_id in run_ids {
            match self.reconcile_approved_auto_completion(run_id).await {
                Ok(true) => completed += 1,
                Ok(false) => {}
                Err(error) => {
                    tracing::debug!(%error, "coding-agent auto-DONE candidate remains unconfirmed")
                }
            }
        }
        completed
    }

    /// Re-read the current run before a periodic completion attempt so stale
    /// candidate lists cannot revive a cancelled or already completed run.
    pub async fn reconcile_approved_auto_completion(
        &self,
        run_id: &RunId,
    ) -> Result<bool, CompletionError> {
        let run = self
            .inner
            .store
            .get_run(run_id)?
            .ok_or(CompletionError::Conflict)?;
        if run.state != super::RunState::AwaitingReview
            || !run.snapshot.auto_done
            || !run.snapshot.semantic_judge_enabled
            || !matches!(
                self.inner.store.approval_assessment_contract(run_id)?,
                super::model::ApprovalAssessmentContractStatus::Versioned { .. }
            )
        {
            return Ok(false);
        }
        let attempt = run
            .current_attempt_id
            .as_deref()
            .ok_or(CompletionError::Conflict)?;
        // Keep the same intent across local revision changes and response loss.
        let key = auto_completion_intent_key(run_id.as_str(), attempt);
        let grant = self.authorize_auto_completion(run_id, &key).await?;
        Ok(matches!(
            self.complete_auto(grant).await?,
            CompletionOutcome::Completed { .. }
        ))
    }

    /// Issue a non-serializable grant only after validating the immutable run,
    /// current canonical review marker, and trusted policy snapshot.
    pub async fn authorize_auto_completion(
        &self,
        run_id: &RunId,
        intent_idempotency_key: &str,
    ) -> Result<CompletionGrant, CompletionError> {
        let primary_identity = self
            .inner
            .tracker
            .primary_identity()
            .map_err(CompletionError::Tracker)?;
        if let Some(existing) = self
            .inner
            .store
            .completion_authorization_for_intent(run_id, intent_idempotency_key)?
        {
            ensure_policy_matches(&self.inner.policy, &existing)?;
            if existing.primary_tracker_identity.as_ref() != Some(&primary_identity) {
                return Err(CompletionError::PolicyMismatch);
            }
            if existing.action != CompletionActionKind::AutoDone
                || existing.actor_id != AUTO_COMPLETION_ACTOR_ID
            {
                return Err(CompletionError::Conflict);
            }
            if existing.status == CompletionAuthorizationStatus::Completed {
                self.verify_completed_authorization(&existing).await?;
                return Ok(self.grant_for(&existing));
            }
            if existing.status == CompletionAuthorizationStatus::Revoked {
                return Err(CompletionError::Conflict);
            }
            let candidate = self
                .inner
                .store
                .completion_candidate(run_id, CompletionActionKind::AutoDone)?;
            let report = auto_completion_report(&candidate)?;
            let report_bytes = report.encode_to_vec();
            let report_sha256 = sha256(&report_bytes);
            if !authorization_matches_candidate(&existing, &candidate)
                || existing.report_sha256 != report_sha256
                || existing.completion_report_protobuf != report_bytes
            {
                return Err(CompletionError::Conflict);
            }
            if existing.status == CompletionAuthorizationStatus::Prepared {
                let issue = self
                    .inner
                    .tracker
                    .read_issue_with_history(&existing.issue_id)
                    .await
                    .map_err(CompletionError::Tracker)?;
                validate_review_issue(&candidate, &issue)?;
                if issue.revision != existing.target_issue_revision {
                    return Err(CompletionError::Conflict);
                }
            }
            return Ok(self.grant_for(&existing));
        }

        let candidate = self
            .inner
            .store
            .completion_candidate(run_id, CompletionActionKind::AutoDone)?;
        let report = auto_completion_report(&candidate)?;
        let report_bytes = report.encode_to_vec();
        let report_sha256 = sha256(&report_bytes);

        let issue = self
            .inner
            .tracker
            .read_issue_with_history(&candidate.run.snapshot.issue_id)
            .await
            .map_err(CompletionError::Tracker)?;
        let marker_revision = validate_review_issue(&candidate, &issue)?;
        if issue.revision == u64::MAX {
            return Err(CompletionError::Conflict);
        }
        let audit_reason = auto_audit_reason(&candidate, &self.inner.policy)?;
        let authorization = self.inner.store.prepare_completion_authorization(
            &candidate,
            NewCompletionAuthorization {
                action: CompletionActionKind::AutoDone,
                actor_id: AUTO_COMPLETION_ACTOR_ID.into(),
                policy_identity: self.inner.policy.identity.clone(),
                policy_version: self.inner.policy.version,
                report_sha256,
                completion_report_protobuf: report_bytes,
                audit_reason,
                marker_revision,
                target_issue_revision: issue.revision,
                intent_idempotency_key: intent_idempotency_key.into(),
                primary_tracker_identity: primary_identity,
            },
        )?;
        Ok(self.grant_for(&authorization))
    }

    /// Apply or reconcile one auto-completion authorization. A later explicit
    /// recovery call may replay the exact Manager request once when the pinned
    /// primary client contract guarantees scoped idempotent replay.
    pub async fn complete_auto(
        &self,
        grant: CompletionGrant,
    ) -> Result<CompletionOutcome, CompletionError> {
        if grant.authority_instance_id != self.inner.instance_id {
            return Err(CompletionError::PolicyMismatch);
        }
        let authorization = self
            .inner
            .store
            .completion_authorization(&grant.authorization_id)?
            .ok_or(CompletionError::Conflict)?;
        ensure_policy_matches(&self.inner.policy, &authorization)?;
        if authorization.action != CompletionActionKind::AutoDone
            || authorization.run_id != grant.run_id
            || authorization.status == CompletionAuthorizationStatus::Revoked
        {
            return Err(CompletionError::Conflict);
        }
        if authorization.status == CompletionAuthorizationStatus::Completed {
            let issue_revision = self.verify_completed_authorization(&authorization).await?;
            let run = self
                .inner
                .store
                .get_run(&authorization.run_id)?
                .ok_or(CompletionError::Conflict)?;
            return Ok(CompletionOutcome::Completed {
                run,
                issue_revision,
            });
        }
        let candidate = self
            .inner
            .store
            .completion_candidate(&authorization.run_id, CompletionActionKind::AutoDone)?;
        if !authorization_matches_candidate(&authorization, &candidate) {
            return Err(CompletionError::Conflict);
        }
        let report = decode_report(&authorization.completion_report_protobuf)?;
        if sha256(&authorization.completion_report_protobuf) != authorization.report_sha256
            || auto_completion_report(&candidate)? != report
        {
            return Err(CompletionError::Conflict);
        }

        let dispatch_initial = authorization.status == CompletionAuthorizationStatus::Prepared;
        let mut dispatched = authorization.clone();
        if dispatch_initial {
            let issue = self
                .read_for_authorization(&candidate, &authorization)
                .await?;
            if issue.issue.state != IssueState::AwaitingReview
                || issue.issue.revision != authorization.target_issue_revision
            {
                return Err(CompletionError::Conflict);
            }
            if !self
                .inner
                .store
                .mark_completion_dispatched(&authorization.authorization_id)?
            {
                return Err(CompletionError::Unconfirmed);
            }
            dispatched = self.load_completion_authorization(&authorization.authorization_id)?;
        }
        let dispatched = self
            .dispatch_or_reconcile_completion(
                &dispatched,
                IssueState::Done,
                Some(report.clone()),
                dispatch_initial,
            )
            .await?;
        let latest = self.read_for_authorization(&candidate, &dispatched).await?;
        let resulting_revision = matching_done_revision(
            &dispatched,
            &latest,
            &report,
            AUTO_COMPLETION_ACTOR_ID,
            "system",
        )
        .ok_or(CompletionError::Unconfirmed)?;
        self.commit_done(&dispatched, resulting_revision)
            .map(|run| CompletionOutcome::Completed {
                run,
                issue_revision: resulting_revision,
            })
    }

    /// Human completion requires a trusted dialog-created review action and an
    /// explicit verified report. It is separate from auto-DONE policy grants.
    pub async fn complete_human(
        &self,
        approval: HumanReviewApproval,
    ) -> Result<CompletionOutcome, CompletionError> {
        let candidate = self
            .inner
            .store
            .completion_candidate(&approval.run_id, CompletionActionKind::HumanDone)?;
        validate_verified_report(&approval.report)?;
        let report_bytes = approval.report.encode_to_vec();
        let primary_identity = self
            .inner
            .tracker
            .primary_identity()
            .map_err(CompletionError::Tracker)?;
        let issue = self
            .inner
            .tracker
            .read_issue_with_history(&candidate.run.snapshot.issue_id)
            .await
            .map_err(CompletionError::Tracker)?;
        let marker_revision = validate_review_issue(&candidate, &issue)?;
        let authorization = self.inner.store.prepare_completion_authorization(
            &candidate,
            NewCompletionAuthorization {
                action: CompletionActionKind::HumanDone,
                actor_id: HUMAN_REVIEW_ACTOR_ID.into(),
                policy_identity: HUMAN_POLICY_IDENTITY.into(),
                policy_version: AUTO_POLICY_VERSION,
                report_sha256: sha256(&report_bytes),
                completion_report_protobuf: report_bytes,
                audit_reason: approval.rationale,
                marker_revision,
                target_issue_revision: issue.revision,
                intent_idempotency_key: approval.intent_idempotency_key,
                primary_tracker_identity: primary_identity,
            },
        )?;
        self.complete_human_authorization(authorization).await
    }

    /// A correction reopens only the Issue. The durable AgentRun remains in
    /// awaiting-review with its evidence intact and no execution is dispatched.
    pub async fn request_correction(
        &self,
        correction: HumanCorrectionRequest,
    ) -> Result<CompletionOutcome, CompletionError> {
        let candidate = self
            .inner
            .store
            .completion_candidate(&correction.run_id, CompletionActionKind::HumanCorrection)?;
        let primary_identity = self
            .inner
            .tracker
            .primary_identity()
            .map_err(CompletionError::Tracker)?;
        let issue = self
            .inner
            .tracker
            .read_issue_with_history(&candidate.run.snapshot.issue_id)
            .await
            .map_err(CompletionError::Tracker)?;
        let marker_revision = validate_review_issue(&candidate, &issue)?;
        let authorization = self.inner.store.prepare_completion_authorization(
            &candidate,
            NewCompletionAuthorization {
                action: CompletionActionKind::HumanCorrection,
                actor_id: HUMAN_REVIEW_ACTOR_ID.into(),
                policy_identity: HUMAN_POLICY_IDENTITY.into(),
                policy_version: AUTO_POLICY_VERSION,
                report_sha256: sha256(correction.rationale.as_bytes()),
                completion_report_protobuf: Vec::new(),
                audit_reason: correction.rationale,
                marker_revision,
                target_issue_revision: issue.revision,
                intent_idempotency_key: correction.intent_idempotency_key,
                primary_tracker_identity: primary_identity,
            },
        )?;
        self.complete_human_authorization(authorization).await
    }

    /// Resume a previously confirmed human dialog action after an app restart.
    /// The stored action is the authorization; no UI boolean or model input is
    /// accepted by this reconciliation API.
    pub async fn reconcile_human_action(
        &self,
        run_id: &RunId,
        intent_idempotency_key: &str,
    ) -> Result<CompletionOutcome, CompletionError> {
        let authorization = self
            .inner
            .store
            .completion_authorization_for_intent(run_id, intent_idempotency_key)?
            .ok_or(CompletionError::Conflict)?;
        if !matches!(
            authorization.action,
            CompletionActionKind::HumanDone | CompletionActionKind::HumanCorrection
        ) {
            return Err(CompletionError::Conflict);
        }
        if authorization.status == CompletionAuthorizationStatus::Completed {
            return match authorization.action {
                CompletionActionKind::HumanDone => {
                    let issue_revision =
                        self.verify_completed_authorization(&authorization).await?;
                    let run = self
                        .inner
                        .store
                        .get_run(run_id)?
                        .ok_or(CompletionError::Conflict)?;
                    Ok(CompletionOutcome::Completed {
                        run,
                        issue_revision,
                    })
                }
                CompletionActionKind::HumanCorrection => {
                    let issue_revision =
                        self.verify_completed_authorization(&authorization).await?;
                    Ok(CompletionOutcome::CorrectionRequested {
                        run_id: run_id.clone(),
                        issue_revision,
                    })
                }
                CompletionActionKind::AutoDone => Err(CompletionError::Conflict),
            };
        }
        self.complete_human_authorization(authorization).await
    }

    fn grant_for(&self, authorization: &RunCompletionAuthorization) -> CompletionGrant {
        CompletionGrant {
            authorization_id: authorization.authorization_id.clone(),
            run_id: authorization.run_id.clone(),
            authority_instance_id: self.inner.instance_id.clone(),
        }
    }

    fn commit_done(
        &self,
        authorization: &RunCompletionAuthorization,
        issue_revision: u64,
    ) -> Result<AgentRunRecord, CompletionError> {
        self.inner
            .store
            .complete_run_authorization(
                &authorization.authorization_id,
                &authorization.policy_identity,
                authorization.policy_version,
                issue_revision,
            )
            .map_err(CompletionError::Store)
    }

    fn load_completion_authorization(
        &self,
        authorization_id: &str,
    ) -> Result<RunCompletionAuthorization, CompletionError> {
        self.inner
            .store
            .completion_authorization(authorization_id)?
            .ok_or(CompletionError::Conflict)
    }

    async fn dispatch_or_reconcile_completion(
        &self,
        authorization: &RunCompletionAuthorization,
        target: IssueState,
        report: Option<DurableCompletionReport>,
        dispatch_initial: bool,
    ) -> Result<RunCompletionAuthorization, CompletionError> {
        let mut current = authorization.clone();
        if dispatch_initial {
            current = self
                .dispatch_completion_once(&current, target, report.clone())
                .await?;
        }
        let lookup = self
            .inner
            .tracker
            .lookup_transition_operation(&current)
            .await;
        match lookup {
            PrimaryOperationLookup::Committed(receipt) => {
                self.persist_committed_operation(&current, receipt)
            }
            PrimaryOperationLookup::Rejected(Some(receipt)) => {
                self.inner
                    .store
                    .record_primary_operation_receipt(&current.authorization_id, receipt.clone())?;
                self.inner
                    .store
                    .revoke_dispatched_completion(&current.authorization_id, receipt)?;
                Err(CompletionError::Conflict)
            }
            PrimaryOperationLookup::Rejected(None)
            | PrimaryOperationLookup::Pending
            | PrimaryOperationLookup::Unknown
                if !dispatch_initial
                    && current.dispatch_attempts == 1
                    && self.inner.tracker.recovery_dispatch_permission(&current)
                        == Some(RecoveryDispatchPermission::SamePayloadIdempotentManager) =>
            {
                // Do not re-read or rebuild the Issue mutation here. The
                // original expected_revision is part of the immutable request;
                // Manager replay must win before its CAS check on a committed
                // operation whose Issue revision has since advanced.
                if !self
                    .inner
                    .store
                    .claim_completion_retry(&current.authorization_id)?
                {
                    return Err(CompletionError::Unconfirmed);
                }
                current = self.load_completion_authorization(&current.authorization_id)?;
                current = self
                    .dispatch_completion_once(&current, target, report)
                    .await?;
                match self
                    .inner
                    .tracker
                    .lookup_transition_operation(&current)
                    .await
                {
                    PrimaryOperationLookup::Committed(receipt) => {
                        self.persist_committed_operation(&current, receipt)
                    }
                    PrimaryOperationLookup::Rejected(Some(receipt)) => {
                        self.inner.store.record_primary_operation_receipt(
                            &current.authorization_id,
                            receipt.clone(),
                        )?;
                        self.inner
                            .store
                            .revoke_dispatched_completion(&current.authorization_id, receipt)?;
                        Err(CompletionError::Conflict)
                    }
                    PrimaryOperationLookup::Pending
                    | PrimaryOperationLookup::Rejected(None)
                    | PrimaryOperationLookup::Unknown => Err(CompletionError::Unconfirmed),
                }
            }
            PrimaryOperationLookup::Rejected(None)
            | PrimaryOperationLookup::Pending
            | PrimaryOperationLookup::Unknown => Err(CompletionError::Unconfirmed),
        }
    }

    async fn dispatch_completion_once(
        &self,
        authorization: &RunCompletionAuthorization,
        target: IssueState,
        report: Option<DurableCompletionReport>,
    ) -> Result<RunCompletionAuthorization, CompletionError> {
        let actor_kind = if authorization.action == CompletionActionKind::AutoDone {
            "system"
        } else {
            "human"
        };
        let request = mutation_request(
            authorization,
            target,
            &authorization.actor_id,
            actor_kind,
            report,
        )?;
        match self
            .inner
            .tracker
            .transition_issue_with_receipt(request)
            .await
        {
            Ok(receipt) => Ok(self
                .inner
                .store
                .record_primary_operation_receipt(&authorization.authorization_id, receipt)?),
            Err(
                TrackerError::Rejected
                | TrackerError::NotFound
                | TrackerError::Unconfirmed
                | TrackerError::Unavailable,
            ) => self.load_completion_authorization(&authorization.authorization_id),
        }
    }

    fn persist_committed_operation(
        &self,
        authorization: &RunCompletionAuthorization,
        receipt: PrimaryOperationReceipt,
    ) -> Result<RunCompletionAuthorization, CompletionError> {
        match authorization.primary_operation.as_ref() {
            Some(existing) if existing != &receipt => Err(CompletionError::Unconfirmed),
            Some(_) => Ok(authorization.clone()),
            None => Ok(self
                .inner
                .store
                .record_primary_operation_receipt(&authorization.authorization_id, receipt)?),
        }
    }

    async fn read_for_authorization(
        &self,
        candidate: &CompletionCandidate,
        authorization: &RunCompletionAuthorization,
    ) -> Result<TrackerIssueOperationSnapshot, CompletionError> {
        let issue = self
            .inner
            .tracker
            .read_issue_with_operation_history(&authorization.issue_id)
            .await
            .map_err(|_| CompletionError::Unconfirmed)?;
        validate_marker_for_authorization(candidate, &issue.issue, authorization)?;
        Ok(issue)
    }

    async fn verify_completed_authorization(
        &self,
        authorization: &RunCompletionAuthorization,
    ) -> Result<u64, CompletionError> {
        if authorization.status != CompletionAuthorizationStatus::Completed
            || authorization.primary_operation.is_none()
            || authorization.resulting_issue_revision.is_none()
            || !completion_authorization_intent_matches(authorization)
        {
            return Err(CompletionError::Conflict);
        }
        let candidate = self
            .inner
            .store
            .completed_completion_candidate(&authorization.authorization_id)?;
        if !authorization_matches_candidate(authorization, &candidate) {
            return Err(CompletionError::Conflict);
        }
        let report = match authorization.action {
            CompletionActionKind::AutoDone => {
                let report = decode_report(&authorization.completion_report_protobuf)?;
                if sha256(&authorization.completion_report_protobuf) != authorization.report_sha256
                    || auto_completion_report(&candidate)? != report
                {
                    return Err(CompletionError::Conflict);
                }
                Some(report)
            }
            CompletionActionKind::HumanDone => {
                let report = decode_report(&authorization.completion_report_protobuf)?;
                if sha256(&authorization.completion_report_protobuf) != authorization.report_sha256
                {
                    return Err(CompletionError::Conflict);
                }
                validate_verified_report(&report)?;
                Some(report)
            }
            CompletionActionKind::HumanCorrection => {
                if !authorization.completion_report_protobuf.is_empty()
                    || sha256(raw_audit_reason(authorization).as_bytes())
                        != authorization.report_sha256
                {
                    return Err(CompletionError::Conflict);
                }
                None
            }
        };
        let receipt = authorization
            .primary_operation
            .as_ref()
            .ok_or(CompletionError::Conflict)?;
        match self
            .inner
            .tracker
            .lookup_transition_operation(authorization)
            .await
        {
            PrimaryOperationLookup::Committed(found) if &found == receipt => {}
            _ => return Err(CompletionError::Unconfirmed),
        }
        let issue = self
            .read_for_authorization(&candidate, authorization)
            .await?;
        let (target, actor_id, actor_kind) = match authorization.action {
            CompletionActionKind::AutoDone => {
                (IssueState::Done, AUTO_COMPLETION_ACTOR_ID, "system")
            }
            CompletionActionKind::HumanDone => (IssueState::Done, HUMAN_REVIEW_ACTOR_ID, "human"),
            CompletionActionKind::HumanCorrection => {
                (IssueState::Open, HUMAN_REVIEW_ACTOR_ID, "human")
            }
        };
        let resulting_revision = matching_transition_revision(
            authorization,
            &issue,
            target,
            actor_id,
            actor_kind,
            report.as_ref(),
        )
        .ok_or(CompletionError::Unconfirmed)?;
        if Some(resulting_revision) != authorization.resulting_issue_revision {
            return Err(CompletionError::Unconfirmed);
        }
        Ok(resulting_revision)
    }

    async fn complete_human_authorization(
        &self,
        authorization: RunCompletionAuthorization,
    ) -> Result<CompletionOutcome, CompletionError> {
        if authorization.policy_identity != HUMAN_POLICY_IDENTITY
            || authorization.actor_id != HUMAN_REVIEW_ACTOR_ID
            || authorization.status == CompletionAuthorizationStatus::Revoked
        {
            return Err(CompletionError::Conflict);
        }
        if authorization.status == CompletionAuthorizationStatus::Completed {
            return match authorization.action {
                CompletionActionKind::HumanDone => {
                    let issue_revision =
                        self.verify_completed_authorization(&authorization).await?;
                    let run = self
                        .inner
                        .store
                        .get_run(&authorization.run_id)?
                        .ok_or(CompletionError::Conflict)?;
                    Ok(CompletionOutcome::Completed {
                        run,
                        issue_revision,
                    })
                }
                CompletionActionKind::HumanCorrection => {
                    let issue_revision =
                        self.verify_completed_authorization(&authorization).await?;
                    Ok(CompletionOutcome::CorrectionRequested {
                        run_id: authorization.run_id,
                        issue_revision,
                    })
                }
                CompletionActionKind::AutoDone => Err(CompletionError::Conflict),
            };
        }
        if !matches!(
            authorization.action,
            CompletionActionKind::HumanDone | CompletionActionKind::HumanCorrection
        ) {
            return Err(CompletionError::Conflict);
        }
        let candidate = self
            .inner
            .store
            .completion_candidate(&authorization.run_id, authorization.action)?;
        if !authorization_matches_candidate(&authorization, &candidate) {
            return Err(CompletionError::Conflict);
        }
        let report = if authorization.action == CompletionActionKind::HumanDone {
            if sha256(&authorization.completion_report_protobuf) != authorization.report_sha256 {
                return Err(CompletionError::Conflict);
            }
            let report = decode_report(&authorization.completion_report_protobuf)?;
            validate_verified_report(&report)?;
            Some(report)
        } else {
            if !authorization.completion_report_protobuf.is_empty()
                || sha256(raw_audit_reason(&authorization).as_bytes())
                    != authorization.report_sha256
            {
                return Err(CompletionError::Conflict);
            }
            None
        };
        let target = match authorization.action {
            CompletionActionKind::HumanDone => IssueState::Done,
            CompletionActionKind::HumanCorrection => IssueState::Open,
            CompletionActionKind::AutoDone => return Err(CompletionError::Conflict),
        };
        let dispatch_initial = authorization.status == CompletionAuthorizationStatus::Prepared;
        let mut dispatched = authorization.clone();
        if dispatch_initial {
            let issue = self
                .read_for_authorization(&candidate, &authorization)
                .await?;
            if issue.issue.state != IssueState::AwaitingReview
                || issue.issue.revision != authorization.target_issue_revision
            {
                return Err(CompletionError::Conflict);
            }
            if !self
                .inner
                .store
                .mark_completion_dispatched(&authorization.authorization_id)?
            {
                return Err(CompletionError::Unconfirmed);
            }
            dispatched = self.load_completion_authorization(&authorization.authorization_id)?;
        }
        let dispatched = self
            .dispatch_or_reconcile_completion(&dispatched, target, report.clone(), dispatch_initial)
            .await?;
        let latest = self.read_for_authorization(&candidate, &dispatched).await?;
        let resulting_revision = matching_transition_revision(
            &dispatched,
            &latest,
            target,
            HUMAN_REVIEW_ACTOR_ID,
            "human",
            report.as_ref(),
        )
        .ok_or(CompletionError::Unconfirmed)?;
        match target {
            IssueState::Done => {
                let run = self.commit_done(&dispatched, resulting_revision)?;
                Ok(CompletionOutcome::Completed {
                    run,
                    issue_revision: resulting_revision,
                })
            }
            IssueState::Open => {
                let _ = self.inner.store.complete_human_correction_authorization(
                    &dispatched.authorization_id,
                    resulting_revision,
                )?;
                Ok(CompletionOutcome::CorrectionRequested {
                    run_id: dispatched.run_id,
                    issue_revision: resulting_revision,
                })
            }
            _ => Err(CompletionError::Conflict),
        }
    }
}

pub struct CompletionGrant {
    authorization_id: String,
    run_id: RunId,
    authority_instance_id: String,
}

impl fmt::Debug for CompletionGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompletionGrant")
            .field("run_id", &self.run_id)
            .field("authorization", &"<opaque>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // Completed returns the durable run snapshot to its caller.
pub enum CompletionOutcome {
    Completed {
        run: AgentRunRecord,
        issue_revision: u64,
    },
    CorrectionRequested {
        run_id: RunId,
        issue_revision: u64,
    },
}

#[derive(Debug, Error)]
pub enum CompletionError {
    #[error(transparent)]
    Store(#[from] CodingAgentError),
    #[error("trusted completion policy configuration is invalid")]
    InvalidPolicy,
    #[error("completion action conflicts with canonical Issue or run state")]
    Conflict,
    #[error("completion authorization belongs to another app authority or policy")]
    PolicyMismatch,
    #[error("primary tracker result could not be proven; run remains awaiting review")]
    Unconfirmed,
    #[error(transparent)]
    Tracker(TrackerError),
    #[error("completion report or audit input is invalid")]
    InvalidInput,
}

pub struct HumanReviewApproval {
    run_id: RunId,
    rationale: String,
    report: DurableCompletionReport,
    intent_idempotency_key: String,
}

impl HumanReviewApproval {
    /// Called only from the authenticated human review dialog command, not from
    /// a generic run-completion payload or model-facing tool.
    #[allow(dead_code)] // The S6 human-review command is wired by the app integration owner.
    pub(crate) fn from_confirmed_review_dialog(
        run_id: RunId,
        rationale: String,
        report: DurableCompletionReport,
    ) -> Result<Self, CompletionError> {
        validate_human_rationale(&rationale)?;
        validate_verified_report(&report)?;
        Ok(Self {
            run_id,
            rationale,
            report,
            intent_idempotency_key: new_intent_key("human-done")?,
        })
    }
}

pub struct HumanCorrectionRequest {
    run_id: RunId,
    rationale: String,
    intent_idempotency_key: String,
}

impl HumanCorrectionRequest {
    /// Called only after a human selected the explicit request-changes action.
    #[allow(dead_code)] // The S6 human-review command is wired by the app integration owner.
    pub(crate) fn from_review_dialog(
        run_id: RunId,
        rationale: String,
    ) -> Result<Self, CompletionError> {
        validate_human_rationale(&rationale)?;
        Ok(Self {
            run_id,
            rationale,
            intent_idempotency_key: new_intent_key("human-correction")?,
        })
    }
}

fn mutation_request(
    authorization: &RunCompletionAuthorization,
    target_state: IssueState,
    actor_id: &str,
    actor_kind: &str,
    completion: Option<DurableCompletionReport>,
) -> Result<TrackerMutationRequest, CompletionError> {
    let primary_tracker_identity = authorization
        .primary_tracker_identity
        .clone()
        .ok_or(CompletionError::Conflict)?;
    Ok(TrackerMutationRequest {
        run_id: authorization.run_id.clone(),
        authorization_id: authorization.authorization_id.clone(),
        intent_sha256: authorization.intent_sha256.clone(),
        primary_tracker_identity,
        issue_id: authorization.issue_id.clone(),
        expected_revision: authorization.target_issue_revision,
        target_state,
        expected_manager_request_digest: authorization.expected_manager_request_digest.clone(),
        intent_idempotency_key: authorization.intent_idempotency_key.clone(),
        idempotency_key: completion_manager_idempotency_key(
            &authorization.run_id,
            &authorization.intent_idempotency_key,
        ),
        actor_id: actor_id.into(),
        actor_kind: actor_kind.into(),
        correlation_id: completion_correlation(authorization),
        audit_reason: authorization.audit_reason.clone(),
        completion,
    })
}

fn raw_audit_reason(authorization: &RunCompletionAuthorization) -> &str {
    let marker = format!(
        " [coding-agent-intent-sha256={}]",
        authorization.intent_sha256
    );
    authorization
        .audit_reason
        .strip_suffix(&marker)
        .unwrap_or(&authorization.audit_reason)
}

fn completion_correlation(authorization: &RunCompletionAuthorization) -> String {
    format!("coding-agent-completion-{}", authorization.authorization_id)
}

fn matching_done_revision(
    authorization: &RunCompletionAuthorization,
    issue: &TrackerIssueOperationSnapshot,
    report: &DurableCompletionReport,
    actor_id: &str,
    actor_kind: &str,
) -> Option<u64> {
    if issue.issue.issue_id != authorization.issue_id || issue.issue.state != IssueState::Done {
        return None;
    }
    matching_transition_revision(
        authorization,
        issue,
        IssueState::Done,
        actor_id,
        actor_kind,
        Some(report),
    )
}

fn matching_transition_revision(
    authorization: &RunCompletionAuthorization,
    issue_snapshot: &TrackerIssueOperationSnapshot,
    target_state: IssueState,
    actor_id: &str,
    actor_kind: &str,
    report: Option<&DurableCompletionReport>,
) -> Option<u64> {
    let issue = &issue_snapshot.issue;
    let expected_revision = authorization.target_issue_revision.checked_add(1)?;
    if issue.issue_id != authorization.issue_id
        || issue.state != target_state
        || issue.revision != expected_revision
        || !matching_marker_exists(authorization, issue)
    {
        return None;
    }
    let expected_downstream_operation_id = authorization
        .primary_operation
        .as_ref()?
        .downstream_operation_id
        .as_str();
    let operation_is_exact = issue_snapshot.operations.iter().any(|operation| {
        operation.issue_id == authorization.issue_id
            && operation.resulting_revision == expected_revision
            && operation.downstream_operation_id == expected_downstream_operation_id
    });
    if !operation_is_exact {
        return None;
    }
    issue.history.iter().find_map(|entry| {
        (entry.issue_id == authorization.issue_id
            && entry.resulting_revision == expected_revision
            && entry.old_state == Some(IssueState::AwaitingReview)
            && entry.new_state == target_state
            && entry.actor_id == actor_id
            && entry.actor_kind == actor_kind
            && entry.correlation_id == completion_correlation(authorization)
            && entry.reason == authorization.audit_reason
            && entry.completion.as_ref() == report)
            .then_some(entry.resulting_revision)
    })
}

fn validate_marker_for_authorization(
    candidate: &CompletionCandidate,
    issue: &TrackerIssueSnapshot,
    authorization: &RunCompletionAuthorization,
) -> Result<(), CompletionError> {
    let marker_revision = matching_marker_revision(candidate, issue)?;
    if marker_revision != authorization.marker_revision
        || issue.issue_id != authorization.issue_id
        || issue.revision < authorization.target_issue_revision
    {
        return Err(CompletionError::Conflict);
    }
    Ok(())
}

fn validate_review_issue(
    candidate: &CompletionCandidate,
    issue: &TrackerIssueSnapshot,
) -> Result<u64, CompletionError> {
    if issue.state != IssueState::AwaitingReview {
        return Err(CompletionError::Conflict);
    }
    matching_marker_revision(candidate, issue)
}

fn matching_marker_exists(
    authorization: &RunCompletionAuthorization,
    issue: &TrackerIssueSnapshot,
) -> bool {
    issue.history.iter().any(|entry| {
        entry.issue_id == authorization.issue_id
            && entry.resulting_revision == authorization.marker_revision
            && entry.new_state == IssueState::AwaitingReview
            && entry.awaiting_review.as_ref().is_some_and(|marker| {
                marker.run_id == authorization.run_id.as_str()
                    && marker.full_commit_id == authorization.commit_id
            })
    })
}

fn matching_marker_revision(
    candidate: &CompletionCandidate,
    issue: &TrackerIssueSnapshot,
) -> Result<u64, CompletionError> {
    if issue.issue_id != candidate.run.snapshot.issue_id
        || issue.revision < candidate.run.snapshot.issue_revision
        || issue.revision == u64::MAX
    {
        return Err(CompletionError::Conflict);
    }
    let mut seen_revisions = BTreeSet::new();
    if issue.history.iter().any(|entry| {
        entry.issue_id != issue.issue_id
            || entry.resulting_revision == 0
            || entry.resulting_revision > issue.revision
            || !seen_revisions.insert(entry.resulting_revision)
    }) {
        return Err(CompletionError::Unconfirmed);
    }
    let mut expected_references = candidate.marker_evidence_references.to_vec();
    expected_references.sort();
    let latest_marker = issue
        .history
        .iter()
        .filter(|entry| entry.new_state == IssueState::AwaitingReview)
        .max_by_key(|entry| entry.resulting_revision)
        .ok_or(CompletionError::Conflict)?;
    let marker = latest_marker
        .awaiting_review
        .as_ref()
        .ok_or(CompletionError::Conflict)?;
    let mut marker_references = marker.evidence_references.clone();
    marker_references.sort();
    if marker.run_id != candidate.run.snapshot.run_id.as_str()
        || candidate
            .marker_revision
            .is_some_and(|revision| revision != latest_marker.resulting_revision)
        || marker.full_commit_id != candidate.commit_id
        || !valid_full_commit_id(&marker.full_commit_id)
        || marker_references != expected_references
        || !seen_revisions.contains(&latest_marker.resulting_revision)
        || latest_marker.resulting_revision <= candidate.run.snapshot.issue_revision
    {
        return Err(CompletionError::Conflict);
    }
    if issue.state == IssueState::AwaitingReview {
        let latest_state = issue
            .history
            .iter()
            .max_by_key(|entry| entry.resulting_revision)
            .ok_or(CompletionError::Conflict)?;
        if latest_state.new_state != IssueState::AwaitingReview {
            return Err(CompletionError::Conflict);
        }
    }
    Ok(latest_marker.resulting_revision)
}

fn authorization_matches_candidate(
    authorization: &RunCompletionAuthorization,
    candidate: &CompletionCandidate,
) -> bool {
    let run_revision_matches = if authorization.status == CompletionAuthorizationStatus::Completed
        && authorization.action != CompletionActionKind::HumanCorrection
    {
        authorization.run_revision.checked_add(1) == Some(candidate.run.revision)
            && candidate.run.state == super::RunState::Done
    } else {
        authorization.run_revision == candidate.run.revision
    };
    authorization.run_id == candidate.run.snapshot.run_id
        && authorization.issue_id == candidate.run.snapshot.issue_id
        && !authorization.attempt_id.is_empty()
        && candidate.run.current_attempt_id.as_deref() == Some(authorization.attempt_id.as_str())
        && authorization.snapshot_sha256 == candidate.snapshot_sha256
        && authorization.evidence_sha256 == candidate.evidence_sha256
        && authorization.commit_id == candidate.commit_id
        && completion_authorization_intent_matches(authorization)
        && authorization.execution_mode == candidate.run.snapshot.execution_mode
        && run_revision_matches
        && authorization.approved_settings_version
            == candidate
                .run
                .snapshot
                .approval
                .as_ref()
                .and_then(|approval| approval.settings_version)
}

fn ensure_policy_matches(
    policy: &CompletionPolicy,
    authorization: &RunCompletionAuthorization,
) -> Result<(), CompletionError> {
    if authorization.policy_identity != policy.identity
        || authorization.policy_version != policy.version
        || authorization.actor_id != AUTO_COMPLETION_ACTOR_ID
    {
        return Err(CompletionError::PolicyMismatch);
    }
    Ok(())
}

fn auto_completion_report(
    candidate: &CompletionCandidate,
) -> Result<DurableCompletionReport, CompletionError> {
    use issue::{
        CompletionEvidence, CompletionEvidenceKind, VerifiedCompletion,
        completion_report::Result as ReportResult,
    };

    if candidate.marker_evidence_references.len() > MAX_REPORT_EVIDENCE.saturating_sub(2) {
        return Err(CompletionError::InvalidInput);
    }
    let mut evidence = vec![CompletionEvidence {
        kind: CompletionEvidenceKind::OtherEvidence as i32,
        summary: if candidate.assessment.commit_id.is_some() {
            format!(
                "Verified source commit {} for {:?} run {}.",
                candidate.commit_id, candidate.run.snapshot.purpose, candidate.run.snapshot.run_id
            )
        } else {
            format!(
                "Investigation report for run {}; base commit {} is a source context reference, with no new source commit.",
                candidate.run.snapshot.run_id, candidate.commit_id
            )
        },
        uri: None,
    }];
    for reference in &candidate.marker_evidence_references {
        evidence.push(CompletionEvidence {
            kind: CompletionEvidenceKind::Artifact as i32,
            summary: format!("Durable run evidence {}.", reference),
            uri: Some(reference.clone()),
        });
    }
    if candidate.assessment.validation.required
        && candidate.assessment.validation.state == ValidationState::Passed
    {
        evidence.push(CompletionEvidence {
            kind: CompletionEvidenceKind::TestResult as i32,
            summary: "All required validation recorded in the immutable run assessment passed."
                .into(),
            uri: None,
        });
    }
    if evidence.iter().any(|item| {
        item.summary.trim().is_empty()
            || item.summary.len() > MAX_REPORT_TEXT_BYTES
            || item.summary.chars().any(char::is_control)
            || item.uri.as_ref().is_some_and(|uri| {
                uri.trim().is_empty() || uri.len() > 4_096 || uri.chars().any(char::is_control)
            })
    }) {
        return Err(CompletionError::InvalidInput);
    }
    Ok(DurableCompletionReport {
        result: Some(ReportResult::Verified(VerifiedCompletion { evidence })),
    })
}

fn validate_verified_report(report: &DurableCompletionReport) -> Result<(), CompletionError> {
    let Some(issue::completion_report::Result::Verified(verified)) = report.result.as_ref() else {
        return Err(CompletionError::InvalidInput);
    };
    if verified.evidence.is_empty() || verified.evidence.len() > MAX_REPORT_EVIDENCE {
        return Err(CompletionError::InvalidInput);
    }
    for evidence in &verified.evidence {
        if issue::CompletionEvidenceKind::try_from(evidence.kind).map_or(true, |kind| {
            kind == issue::CompletionEvidenceKind::Unspecified
        }) || evidence.summary.trim().is_empty()
            || evidence.summary.len() > MAX_REPORT_TEXT_BYTES
            || evidence.summary.chars().any(char::is_control)
            || evidence.uri.as_ref().is_some_and(|uri| {
                uri.trim().is_empty() || uri.len() > 4_096 || uri.chars().any(char::is_control)
            })
        {
            return Err(CompletionError::InvalidInput);
        }
    }
    if report.encoded_len() > 4 * 1024 * 1024 {
        return Err(CompletionError::InvalidInput);
    }
    Ok(())
}

fn auto_audit_reason(
    candidate: &CompletionCandidate,
    policy: &CompletionPolicy,
) -> Result<String, CompletionError> {
    let settings_version = candidate
        .run
        .snapshot
        .approval
        .as_ref()
        .and_then(|approval| approval.settings_version)
        .ok_or(CompletionError::Conflict)?;
    let reason = format!(
        "coding-agent-auto-done/v1 run={} policy={} policy_version={} settings_version={} snapshot_sha256={} evidence_sha256={} commit={}",
        candidate.run.snapshot.run_id,
        policy.identity,
        policy.version,
        settings_version,
        candidate.snapshot_sha256,
        candidate.evidence_sha256,
        candidate.commit_id,
    );
    if reason.len() > MAX_AUDIT_REASON_BYTES || reason.chars().any(char::is_control) {
        return Err(CompletionError::InvalidInput);
    }
    Ok(reason)
}

#[allow(dead_code)] // Used only by trusted human-dialog constructors.
fn validate_human_rationale(rationale: &str) -> Result<(), CompletionError> {
    if rationale.trim().is_empty()
        || rationale.len() > MAX_AUDIT_REASON_BYTES
        || rationale.chars().any(char::is_control)
    {
        return Err(CompletionError::InvalidInput);
    }
    Ok(())
}

#[allow(dead_code)] // Used only by trusted human-dialog constructors.
fn new_intent_key(prefix: &str) -> Result<String, CompletionError> {
    let random = RunId::generate()
        .map_err(|error| CompletionError::Store(CodingAgentError::Model(error)))?;
    Ok(format!("{prefix}-{random}"))
}

fn decode_report(bytes: &[u8]) -> Result<DurableCompletionReport, CompletionError> {
    DurableCompletionReport::decode(bytes).map_err(|_| CompletionError::Conflict)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_full_commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {

    #[test]
    fn periodic_intent_keys_are_bounded_and_distinguish_identifier_boundaries() {
        let key = super::auto_completion_intent_key(&"r".repeat(128), &"a".repeat(128));
        assert!(key.len() <= 200);
        assert_eq!(
            key,
            super::auto_completion_intent_key(&"r".repeat(128), &"a".repeat(128))
        );
        assert_ne!(
            super::auto_completion_intent_key("a-b", "c"),
            super::auto_completion_intent_key("a", "b-c")
        );
    }
    use super::*;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use crate::{
        coding_agent::{
            AgentProfileSnapshot, AgentRunSnapshot, AllowedOperation, ForgeKind,
            HostResourceLimits, PublicationPlan, RepositoryIdentity, RepositoryProvenance,
            RepositorySelection, ResolvedRepository, RunEvidenceAssessment, RunState,
            SandboxResources, ValidationEvidence, WorkflowDefinitionSnapshot,
        },
        data::DataPaths,
    };
    use async_trait::async_trait;

    struct HumanTestTracker {
        issue: Mutex<TrackerIssueSnapshot>,
        identity: PrimaryTrackerIdentity,
        lookup: Mutex<PrimaryOperationLookup>,
        history_operations: Mutex<HashMap<(String, u64), String>>,
        after_transition: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    }

    #[async_trait]
    impl CompletionTracker for HumanTestTracker {
        async fn read_issue_with_history(
            &self,
            issue_id: &str,
        ) -> TrackerResult<TrackerIssueSnapshot> {
            let snapshot = self.issue.lock().unwrap().clone();
            if snapshot.issue_id == issue_id {
                Ok(snapshot)
            } else {
                Err(TrackerError::NotFound)
            }
        }

        fn primary_identity(&self) -> TrackerResult<PrimaryTrackerIdentity> {
            Ok(self.identity.clone())
        }

        async fn transition_issue(&self, request: TrackerMutationRequest) -> TrackerResult<()> {
            self.transition_issue_with_receipt(request)
                .await
                .map(|_| ())
        }

        async fn transition_issue_with_receipt(
            &self,
            request: TrackerMutationRequest,
        ) -> TrackerResult<PrimaryOperationReceipt> {
            let receipt = test_operation_receipt(&request);
            let mut issue = self.issue.lock().unwrap();
            let old_state = issue.state;
            issue.state = request.target_state;
            issue.revision += 1;
            let resulting_revision = issue.revision;
            let issue_id = issue.issue_id.clone();
            issue.history.push(IssueHistoryEntry {
                issue_id: issue_id.clone(),
                resulting_revision,
                old_state: Some(old_state),
                new_state: request.target_state,
                awaiting_review: None,
                completion: request.completion,
                actor_id: request.actor_id,
                actor_kind: request.actor_kind,
                correlation_id: request.correlation_id,
                reason: request.audit_reason,
            });
            self.history_operations.lock().unwrap().insert(
                (issue_id, resulting_revision),
                receipt.downstream_operation_id.clone(),
            );
            *self.lookup.lock().unwrap() = PrimaryOperationLookup::Committed(receipt.clone());
            drop(issue);
            if let Some(hook) = self.after_transition.lock().unwrap().take() {
                hook();
            }
            Ok(receipt)
        }

        async fn lookup_transition_operation(
            &self,
            _authorization: &RunCompletionAuthorization,
        ) -> PrimaryOperationLookup {
            self.lookup.lock().unwrap().clone()
        }

        async fn read_issue_with_operation_history(
            &self,
            issue_id: &str,
        ) -> TrackerResult<TrackerIssueOperationSnapshot> {
            let issue = self.read_issue_with_history(issue_id).await?;
            let operations = self
                .history_operations
                .lock()
                .unwrap()
                .iter()
                .map(
                    |((issue_id, resulting_revision), downstream_operation_id)| {
                        TrackerHistoryOperation {
                            issue_id: issue_id.clone(),
                            resulting_revision: *resulting_revision,
                            downstream_operation_id: downstream_operation_id.clone(),
                        }
                    },
                )
                .collect();
            Ok(TrackerIssueOperationSnapshot { issue, operations })
        }
    }

    #[test]
    fn grants_are_not_serializable_and_debug_does_not_publish_authorization_identity() {
        let grant = CompletionGrant {
            authorization_id: "private-authorization-id".into(),
            run_id: RunId::new("run-safe").unwrap(),
            authority_instance_id: "private-authority-id".into(),
        };
        let debug = format!("{grant:?}");
        assert!(!debug.contains("private-authorization-id"));
        assert!(!debug.contains("private-authority-id"));
        assert!(debug.contains("<opaque>"));
    }

    #[test]
    fn trusted_downstream_proof_requires_the_exact_manager_binding() {
        let manager_operation = manager::DurableOperation {
            operation_id: "manager-op-1".into(),
            downstream_operation_id: "tracker-op-1".into(),
            request_digest: "manager-request-digest".into(),
            request_schema: MANAGER_DURABLE_BACKEND_REQUEST_SCHEMA.into(),
            ..Default::default()
        };
        let trusted = manager::TrustedDownstreamOperation {
            operation: Some(issue::OperationRecord {
                downstream_operation_id: "tracker-op-1".into(),
                ..Default::default()
            }),
            binding: Some(manager::ManagerDownstreamOperationBinding {
                manager_operation_id: "manager-op-1".into(),
                issue_id: "issue-1".into(),
                downstream_operation_id: "tracker-op-1".into(),
                manager_request_digest: "manager-request-digest".into(),
                manager_request_schema: MANAGER_DURABLE_BACKEND_REQUEST_SCHEMA.into(),
            }),
        };

        assert!(
            trusted_downstream_record_bound_to_manager(&trusted, &manager_operation, "issue-1")
                .is_some()
        );

        for field in 0..5 {
            let mut mismatched = trusted.clone();
            let binding = mismatched.binding.as_mut().unwrap();
            match field {
                0 => binding.manager_operation_id.push_str("-other"),
                1 => binding.issue_id.push_str("-other"),
                2 => binding.downstream_operation_id.push_str("-other"),
                3 => binding.manager_request_digest.push_str("-other"),
                4 => binding.manager_request_schema.push_str("-other"),
                _ => unreachable!(),
            }
            assert!(
                trusted_downstream_record_bound_to_manager(
                    &mismatched,
                    &manager_operation,
                    "issue-1"
                )
                .is_none()
            );
        }

        let mut missing_binding = trusted.clone();
        missing_binding.binding = None;
        assert!(
            trusted_downstream_record_bound_to_manager(
                &missing_binding,
                &manager_operation,
                "issue-1"
            )
            .is_none()
        );
        let mut missing_operation = trusted;
        missing_operation.operation = None;
        assert!(
            trusted_downstream_record_bound_to_manager(
                &missing_operation,
                &manager_operation,
                "issue-1"
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn manager_receipt_binds_the_full_admitted_transition_and_real_schema() {
        let fixture = human_fixture(true);
        let authority = CompletionAuthority::new(
            fixture.store.clone(),
            fixture.tracker.clone(),
            CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
        )
        .unwrap();
        let grant = authority
            .authorize_auto_completion(&fixture.run_id, "wire-contract-intent")
            .await
            .unwrap();
        let authorization = fixture
            .store
            .completion_authorization(&grant.authorization_id)
            .unwrap()
            .unwrap();
        let request = mutation_request(
            &authorization,
            IssueState::Done,
            AUTO_COMPLETION_ACTOR_ID,
            "system",
            Some(decode_report(&authorization.completion_report_protobuf).unwrap()),
        )
        .unwrap();
        let wire_request = manager_transition_args(&request);
        let bytes = wire_request.encode_to_vec();
        let decoded = manager::TransitionIssueArgs::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded, wire_request);
        assert_eq!(
            sha256(&bytes),
            authorization.expected_manager_request_digest
        );
        assert_eq!(decoded.issue_id, authorization.issue_id);
        assert_eq!(
            decoded.expected_revision,
            Some(authorization.target_issue_revision)
        );
        assert_eq!(decoded.target_state, issue::IssueState::Done as i32);
        assert!(decoded.lease_handle.is_none());
        assert!(decoded.deferred_review_at.is_none());
        assert!(decoded.claim_revision.is_none());
        assert!(decoded.awaiting_review.is_none());
        assert!(matches!(
            decoded.terminal_report,
            Some(manager::transition_issue_args::TerminalReport::Completion(
                _
            ))
        ));

        let operation = server_issued_manager_operation(&request);
        assert_eq!(
            operation.request_schema,
            MANAGER_DURABLE_BACKEND_REQUEST_SCHEMA
        );
        assert!(manager_operation_matches_request(
            &operation,
            &request,
            &request.primary_tracker_identity,
        ));
        let receipt = manager_operation_receipt(
            &operation,
            &request,
            request.primary_tracker_identity.clone(),
            "backend-contract-fingerprint-v1".into(),
        )
        .unwrap();
        assert_eq!(
            receipt.request_digest,
            authorization.expected_manager_request_digest
        );
        assert_eq!(
            receipt.request_schema,
            "issue_tracker.manager.v1.DurableBackendRequest"
        );

        let primary_operation = server_issued_primary_operation(&authorization, &receipt);
        assert!(primary_operation_record_matches_authorization(
            &primary_operation,
            &receipt,
            &authorization,
        ));
        let mut wrong_downstream_fingerprint = primary_operation.clone();
        wrong_downstream_fingerprint.payload_fingerprint = "b".repeat(64);
        assert!(!primary_operation_record_matches_authorization(
            &wrong_downstream_fingerprint,
            &receipt,
            &authorization,
        ));

        let mut wrong_digest = operation.clone();
        wrong_digest.request_digest = "a".repeat(64);
        assert!(!manager_operation_matches_request(
            &wrong_digest,
            &request,
            &request.primary_tracker_identity,
        ));
        let mut wrong_namespace = operation;
        wrong_namespace.idempotency_scope.push_str("tampered");
        assert!(!manager_operation_matches_request(
            &wrong_namespace,
            &request,
            &request.primary_tracker_identity,
        ));
        let mut different_payload_with_same_audit = request;
        different_payload_with_same_audit.expected_revision += 1;
        assert!(!authorization_intent_matches_request(
            &different_payload_with_same_audit
        ));
    }

    #[tokio::test]
    async fn manager_committed_operation_matches_the_servers_unstamped_admission_record() {
        let fixture = human_fixture(true);
        let authority = CompletionAuthority::new(
            fixture.store.clone(),
            fixture.tracker.clone(),
            CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
        )
        .unwrap();
        let grant = authority
            .authorize_auto_completion(&fixture.run_id, "source-terminal-operation")
            .await
            .unwrap();
        let authorization = fixture
            .store
            .completion_authorization(&grant.authorization_id)
            .unwrap()
            .unwrap();
        let request = mutation_request(
            &authorization,
            IssueState::Done,
            AUTO_COMPLETION_ACTOR_ID,
            "system",
            Some(decode_report(&authorization.completion_report_protobuf).unwrap()),
        )
        .unwrap();
        let operation = server_committed_manager_operation(&request);

        assert_eq!(operation.manager_sequence, 0);
        assert!(operation.commit_snapshot.is_none());
        assert_eq!(
            operation.terminal_snapshot,
            Some(
                manager::durable_operation::TerminalSnapshot::ResultSnapshot(manager::Snapshot {
                    schema: "issue_tracker.manager.v1.ManagerMutation".into(),
                    payload: manager::ManagerMutation::default().encode_to_vec(),
                })
            )
        );
        assert!(manager_operation_matches_request(
            &operation,
            &request,
            &request.primary_tracker_identity,
        ));
        assert!(manager_operation_commit_is_consistent(&operation));

        let receipt = manager_operation_receipt(
            &operation,
            &request,
            request.primary_tracker_identity.clone(),
            "backend-contract-fingerprint-v1".into(),
        )
        .unwrap();
        assert_eq!(receipt.manager_sequence, 0);
        assert_eq!(
            receipt.downstream_operation_id,
            "backend-server-issued-manager-operation-test"
        );

        let mut wrong_state = operation.clone();
        wrong_state.state = manager::DurableOperationState::Pending as i32;
        assert!(!manager_operation_commit_is_consistent(&wrong_state));

        let mut wrong_schema = operation.clone();
        let Some(manager::durable_operation::TerminalSnapshot::ResultSnapshot(result)) =
            wrong_schema.terminal_snapshot.as_mut()
        else {
            unreachable!();
        };
        result.schema = "issue_tracker.manager.v1.OperationRecord".into();
        assert!(!manager_operation_commit_is_consistent(&wrong_schema));

        let mut nonempty_result = operation.clone();
        let Some(manager::durable_operation::TerminalSnapshot::ResultSnapshot(result)) =
            nonempty_result.terminal_snapshot.as_mut()
        else {
            unreachable!();
        };
        result.payload = vec![0x0a, 0x00];
        assert!(!manager_operation_commit_is_consistent(&nonempty_result));

        let mut wrong_digest = operation;
        wrong_digest.request_digest = "f".repeat(64);
        assert!(!manager_operation_matches_request(
            &wrong_digest,
            &request,
            &request.primary_tracker_identity,
        ));
    }

    #[test]
    fn only_a_terminal_manager_operation_snapshot_proves_rejection() {
        let mut operation = manager::DurableOperation {
            operation_id: "server-issued-terminal-rejection-test".into(),
            state: manager::DurableOperationState::Failed as i32,
            failure_class: manager::FailureClass::Terminal as i32,
            commit_durability: issue::OperationCommitDurability::Durable as i32,
            result_replayability: issue::OperationResultReplayability::Replayable as i32,
            generation: 1,
            terminal_snapshot: Some(manager::durable_operation::TerminalSnapshot::ErrorSnapshot(
                manager::Snapshot {
                    schema: "issue_tracker.manager.v1.ManagerError".into(),
                    payload: manager::ManagerError {
                        code: manager::ManagerErrorCode::ManagerInvalidArgument as i32,
                        message: "fixture only".into(),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                },
            )),
            ..Default::default()
        };
        assert!(manager_operation_has_terminal_rejection(&operation));

        operation.state = manager::DurableOperationState::Pending as i32;
        assert!(!manager_operation_has_terminal_rejection(&operation));
        operation.state = manager::DurableOperationState::Failed as i32;
        operation.resolution_required = true;
        assert!(!manager_operation_has_terminal_rejection(&operation));
        operation.resolution_required = false;
        operation.terminal_snapshot = Some(
            manager::durable_operation::TerminalSnapshot::ErrorSnapshot(manager::Snapshot {
                schema: "issue_tracker.manager.v1.ManagerError".into(),
                payload: manager::ManagerError {
                    code: manager::ManagerErrorCode::ResolutionRequired as i32,
                    ..Default::default()
                }
                .encode_to_vec(),
            }),
        );
        assert!(!manager_operation_has_terminal_rejection(&operation));
    }

    #[test]
    fn terminal_rejection_requires_a_known_nonretryable_code_or_all_attempts_not_sent() {
        let mut operation = terminal_manager_failure(
            manager::ManagerErrorCode::ManagerInvalidArgument,
            false,
            None,
        );
        assert!(manager_operation_has_terminal_rejection(&operation));

        operation.terminal_snapshot = Some(manager_error_snapshot(10_000, false, None));
        assert!(!manager_operation_has_terminal_rejection(&operation));
        operation.terminal_snapshot = Some(manager_error_snapshot(
            manager::ManagerErrorCode::ManagerInvalidArgument as i32,
            true,
            None,
        ));
        assert!(!manager_operation_has_terminal_rejection(&operation));

        operation.terminal_snapshot = Some(manager_error_snapshot(
            manager::ManagerErrorCode::RetryExhausted as i32,
            true,
            None,
        ));
        assert!(!manager_operation_has_terminal_rejection(&operation));
        operation.terminal_snapshot = Some(manager_error_snapshot(
            manager::ManagerErrorCode::RetryExhausted as i32,
            true,
            Some(manager::manager_error::Detail::OperationDiagnosis(
                manager::OperationDiagnosis {
                    operation_id: operation.operation_id.clone(),
                    classification: "NOT_SENT_RETRY_EXHAUSTED".into(),
                    evidence_ref: operation.operation_id.clone(),
                    replayability: "NEW_READMISSION_ONLY".into(),
                    resolution_state: "ALL_ATTEMPTS_NOT_SENT".into(),
                    ..Default::default()
                },
            )),
        ));
        assert!(manager_operation_has_terminal_rejection(&operation));
    }

    #[test]
    fn manager_history_lookup_uses_the_server_required_direct_issue_filter() {
        let args =
            completion_operation_history_args("issue:with:delimiters", Some("cursor".into()));
        let filter = args.filter.unwrap();
        assert_eq!(
            filter.direct_target,
            Some(manager::operation_history_filter::DirectTarget::IssueId(
                "issue:with:delimiters".into()
            ))
        );
        assert_eq!(filter.method.as_deref(), Some(MANAGER_TRANSITION_METHOD));
        assert!(filter.direct_scope_only);
        assert_eq!(
            args.page.as_ref().unwrap().page_token.as_deref(),
            Some("cursor")
        );
        assert_eq!(args.include_total_size, Some(false));
    }

    #[tokio::test]
    async fn reassessment_pass_binds_the_current_issue_for_human_and_auto_completion() {
        use super::super::{
            DurableOperationOutcome, DurableOperationRequest, DurableOperationResult,
            assessment::AssessmentUse, coordinator::SemanticReassessmentRecord,
            semantic_judge::SemanticAssessment,
        };
        for (auto_done, fresh_revision) in
            [(false, true), (false, false), (true, true), (true, false)]
        {
            let fixture = human_fixture_with_assessment(auto_done, true, false);
            let waiting = fixture.store.get_run(&fixture.run_id).unwrap().unwrap();
            let report_reference = waiting
                .execution_evidence
                .as_ref()
                .unwrap()
                .report_reference
                .clone()
                .unwrap();
            let marker = fixture.tracker.issue.lock().unwrap().history[0]
                .awaiting_review
                .clone()
                .unwrap();
            fixture.store.write_evidence(&fixture.run_id, "attempt-1", "issue-result-ack", "application/json", &serde_json::to_vec(&serde_json::json!({
                "schema_version": 1, "run_id": fixture.run_id.as_str(), "attempt_id": "attempt-1", "status": "completed",
                "response": {
                    "issue_id": waiting.snapshot.issue_id, "revision": 9, "run_id": fixture.run_id.as_str(),
                    "full_commit_id": marker.full_commit_id, "evidence_references": marker.evidence_references, "lease_closed": true,
                },
            })).unwrap()).unwrap();
            let mut assessment = SemanticAssessment::inconclusive("Independent review");
            assessment.outcome = AssessmentUse::Pass;
            let record = SemanticReassessmentRecord {
                run_id: fixture.run_id.to_string(),
                attempt_id: "attempt-1".into(),
                request_key: "manual-pass".into(),
                expected_run_revision: waiting.revision,
                issue_revision: if fresh_revision { 10 } else { 9 },
                source_sha256: "a".repeat(64),
                source_head_commit: "e".repeat(40),
                source_is_clean: true,
                report_reference: report_reference.clone(),
                assessment,
            };
            let operation_key = format!(
                "human-semantic:{:x}",
                Sha256::digest(record.request_key.as_bytes())
            );
            let claim = fixture
                .store
                .claim_effect(
                    &fixture.run_id,
                    "attempt-1",
                    &operation_key,
                    DurableOperationRequest::ReassessSemantics {
                        expected_run_revision: waiting.revision,
                        issue_revision: record.issue_revision,
                        source_sha256: record.source_sha256.clone(),
                        report_reference,
                    },
                )
                .unwrap();
            let result = fixture
                .store
                .write_evidence(
                    &fixture.run_id,
                    "attempt-1",
                    "human-semantic-result",
                    "application/json",
                    &serde_json::to_vec(&record).unwrap(),
                )
                .unwrap();
            fixture
                .store
                .complete_operation(
                    &fixture.run_id,
                    &claim.operation.idempotency_key,
                    DurableOperationResult {
                        outcome: DurableOperationOutcome::Succeeded,
                        result_digest: Some(result.sha256),
                        evidence_ids: vec![result.evidence_id],
                    },
                )
                .unwrap();
            let authority = CompletionAuthority::new(
                fixture.store.clone(),
                fixture.tracker.clone(),
                CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
            )
            .unwrap();
            let approval = HumanReviewApproval::from_confirmed_review_dialog(
                fixture.run_id.clone(),
                "Reviewed the new judgment and unchanged source".into(),
                verified_report("Human verified acceptance"),
            )
            .unwrap();
            let key = if auto_done {
                "reassessment-auto".to_owned()
            } else {
                approval.intent_idempotency_key.clone()
            };
            let outcome = if auto_done {
                match authority
                    .authorize_auto_completion(&fixture.run_id, &key)
                    .await
                {
                    Ok(grant) => authority.complete_auto(grant).await,
                    Err(error) => Err(error),
                }
            } else {
                authority.complete_human(approval).await
            };
            if fresh_revision {
                assert!(matches!(
                    outcome.unwrap(),
                    CompletionOutcome::Completed {
                        issue_revision: 11,
                        ..
                    }
                ));
                let paths = DataPaths::with_root(fixture._directory.path());
                let reopened = Arc::new(RunStore::open(&paths).unwrap());
                let recovered = CompletionAuthority::new(
                    reopened.clone(),
                    fixture.tracker.clone(),
                    CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
                )
                .unwrap();
                let replay = if auto_done {
                    let grant = recovered
                        .authorize_auto_completion(&fixture.run_id, &key)
                        .await
                        .unwrap();
                    recovered.complete_auto(grant).await.unwrap()
                } else {
                    recovered
                        .reconcile_human_action(&fixture.run_id, &key)
                        .await
                        .unwrap()
                };
                assert!(matches!(replay, CompletionOutcome::Completed { .. }));
                assert!(
                    !reopened
                        .get_run(&fixture.run_id)
                        .unwrap()
                        .unwrap()
                        .execution_evidence
                        .unwrap()
                        .acceptance_conditions_met
                );
                let issue = fixture.tracker.issue.lock().unwrap();
                assert_eq!(issue.revision, 11);
                assert_eq!(issue.history.len(), 2);
            } else {
                assert!(outcome.is_err());
                assert_eq!(fixture.tracker.issue.lock().unwrap().revision, 10);
                assert_eq!(
                    fixture
                        .store
                        .get_run(&fixture.run_id)
                        .unwrap()
                        .unwrap()
                        .state,
                    RunState::AwaitingReview
                );
                assert_eq!(
                    fixture
                        .store
                        .get_run(&fixture.run_id)
                        .unwrap()
                        .unwrap()
                        .revision,
                    waiting.revision
                );
            }
        }
    }

    #[tokio::test]
    async fn explicit_human_review_can_complete_a_false_auto_done_run_with_verified_report() {
        let fixture = human_fixture(false);
        let authority = CompletionAuthority::new(
            fixture.store.clone(),
            fixture.tracker.clone(),
            CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
        )
        .unwrap();
        let approval = HumanReviewApproval::from_confirmed_review_dialog(
            fixture.run_id.clone(),
            "Human reviewed the durable report and verified acceptance.".into(),
            verified_report("Acceptance criteria verified by reviewer."),
        )
        .unwrap();

        let outcome = authority.complete_human(approval).await.unwrap();

        let CompletionOutcome::Completed {
            run,
            issue_revision,
        } = outcome
        else {
            panic!("human approval must complete the run");
        };
        assert_eq!(issue_revision, 11);
        assert_eq!(run.state, RunState::Done);
        let issue = fixture.tracker.issue.lock().unwrap();
        let event = issue.history.last().unwrap();
        assert_eq!(event.old_state, Some(IssueState::AwaitingReview));
        assert_eq!(event.new_state, IssueState::Done);
        assert_eq!(event.actor_kind, "human");
    }

    #[tokio::test]
    async fn explicit_human_review_can_complete_when_semantic_assessment_was_disabled() {
        let fixture = human_fixture_with_assessment(false, false, false);
        let authority = CompletionAuthority::new(
            fixture.store.clone(),
            fixture.tracker.clone(),
            CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
        )
        .unwrap();
        let approval = HumanReviewApproval::from_confirmed_review_dialog(
            fixture.run_id.clone(),
            "Reviewed the result without an automated assessment.".into(),
            verified_report("Human verified the acceptance criteria."),
        )
        .unwrap();
        assert!(matches!(
            authority.complete_human(approval).await.unwrap(),
            CompletionOutcome::Completed { .. }
        ));
        let run = fixture.store.get_run(&fixture.run_id).unwrap().unwrap();
        assert!(
            !run.execution_evidence.unwrap().acceptance_conditions_met,
            "human completion must not invent an automated judge PASS"
        );
    }

    #[test]
    fn generic_store_transition_cannot_mark_an_approved_run_done() {
        let fixture = human_fixture(true);
        let run = fixture.store.get_run(&fixture.run_id).unwrap().unwrap();

        assert!(matches!(
            fixture
                .store
                .transition(&fixture.run_id, run.revision, RunState::Done),
            Err(CodingAgentError::Conflict(_))
        ));
        assert_eq!(
            fixture
                .store
                .get_run(&fixture.run_id)
                .unwrap()
                .unwrap()
                .state,
            RunState::AwaitingReview
        );
    }

    #[tokio::test]
    async fn human_correction_reopens_only_issue_and_revokes_unspent_auto_grant() {
        let fixture = human_fixture(true);
        let authority = CompletionAuthority::new(
            fixture.store.clone(),
            fixture.tracker.clone(),
            CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
        )
        .unwrap();
        let auto_grant = authority
            .authorize_auto_completion(&fixture.run_id, "auto-grant-to-revoke")
            .await
            .unwrap();
        let evidence_before = fixture.store.evidence(&fixture.run_id).unwrap();
        let correction = HumanCorrectionRequest::from_review_dialog(
            fixture.run_id.clone(),
            "Please add regression coverage for the reported edge case.".into(),
        )
        .unwrap();

        let outcome = authority.request_correction(correction).await.unwrap();

        assert!(matches!(
            outcome,
            CompletionOutcome::CorrectionRequested {
                issue_revision: 11,
                ..
            }
        ));
        let run = fixture.store.get_run(&fixture.run_id).unwrap().unwrap();
        assert_eq!(run.state, RunState::AwaitingReview);
        assert_eq!(run.current_attempt_id.as_deref(), Some("attempt-1"));
        assert_eq!(
            fixture.store.evidence(&fixture.run_id).unwrap(),
            evidence_before
        );
        assert_eq!(
            fixture.tracker.issue.lock().unwrap().state,
            IssueState::Open
        );
        assert!(matches!(
            fixture
                .store
                .completion_authorization(&auto_grant.authorization_id)
                .unwrap()
                .unwrap()
                .status,
            CompletionAuthorizationStatus::Revoked
        ));
        assert!(authority.complete_auto(auto_grant).await.is_err());
        assert_eq!(fixture.tracker.issue.lock().unwrap().history.len(), 2);
    }

    #[tokio::test]
    async fn completed_correction_is_not_reported_after_issue_leaves_open_state() {
        let fixture = human_fixture(false);
        let authority = CompletionAuthority::new(
            fixture.store.clone(),
            fixture.tracker.clone(),
            CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
        )
        .unwrap();
        let correction = HumanCorrectionRequest::from_review_dialog(
            fixture.run_id.clone(),
            "Please revise the implementation.".into(),
        )
        .unwrap();
        let intent_key = correction.intent_idempotency_key.clone();
        *fixture.tracker.after_transition.lock().unwrap() = Some({
            let tracker = fixture.tracker.clone();
            Arc::new(move || {
                let mut issue = tracker.issue.lock().unwrap();
                issue.state = IssueState::InProgress;
                issue.revision += 1;
            })
        });

        assert!(authority.request_correction(correction).await.is_err());
        assert!(
            authority
                .reconcile_human_action(&fixture.run_id, &intent_key)
                .await
                .is_err()
        );
        assert_eq!(
            fixture.tracker.issue.lock().unwrap().state,
            IssueState::InProgress
        );
        assert_eq!(
            fixture
                .store
                .get_run(&fixture.run_id)
                .unwrap()
                .unwrap()
                .state,
            RunState::AwaitingReview
        );
    }

    struct HumanFixture {
        _directory: tempfile::TempDir,
        store: Arc<RunStore>,
        run_id: RunId,
        tracker: Arc<HumanTestTracker>,
    }

    fn human_fixture(auto_done: bool) -> HumanFixture {
        human_fixture_with_assessment(auto_done, true, true)
    }

    fn human_fixture_with_assessment(
        auto_done: bool,
        semantic_judge_enabled: bool,
        acceptance_conditions_met: bool,
    ) -> HumanFixture {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(RunStore::open(&DataPaths::with_root(directory.path())).unwrap());
        let repository_id =
            RepositoryIdentity::new("forge.example", "lookback", "agent-app").unwrap();
        let image_digest = "a".repeat(64);
        let run_id = RunId::new(if auto_done {
            "human-review-auto"
        } else {
            "human-review-manual"
        })
        .unwrap();
        let run_snapshot = AgentRunSnapshot {
            run_id: run_id.clone(),
            issue_id: "issue-human-review".into(),
            issue_revision: 5,
            execution_mode: super::super::ExecutionMode::OneShot,
            purpose: super::super::RunPurpose::Implementation,
            repository: ResolvedRepository {
                identity: repository_id.clone(),
                forge: ForgeKind::Gitea,
                api_base_url: "https://forge.example/api/v1".into(),
                clone_url: "https://forge.example/lookback/agent-app".into(),
                read_credential_ref: None,
                publication_credential_ref: None,
                provenance: RepositoryProvenance {
                    source_labels: vec!["repo:forge.example/lookback/agent-app".into()],
                    selection: RepositorySelection::SourceLabel,
                    approved_by: None,
                    approved_at_ms: None,
                },
            },
            profile: AgentProfileSnapshot {
                profile_id: "local-coder".into(),
                version: 1,
                display_name: "Local coder".into(),
                image_ref: format!("registry.example/coder@sha256:{image_digest}"),
                image_digest: format!("sha256:{image_digest}"),
                command: vec!["agent".into(), "run".into()],
                container_home: "/home/agent".into(),
                managed_home_overlay: None,
                runtime_environment: Default::default(),
                network_access: Default::default(),
                resources: SandboxResources {
                    cpu_millis: 1000,
                    memory_mib: 1024,
                    root_disk_mib: 4096,
                    max_parallel_runs: 1,
                    use_static: false,
                },
                validated_host_limits: HostResourceLimits {
                    cpu_millis: 8000,
                    memory_mib: 16384,
                    root_disk_mib: 65536,
                    max_parallel_runs: 4,
                },
                canonical_mount_allowed_roots: vec!["/tmp/approved".into()],
                persistent_mounts: Vec::new(),
                allowed_repositories: vec![repository_id],
                credential_reference: None,
            },
            base_commit: "b".repeat(40),
            workspace_branch: "agent/human-review".into(),
            semantic_judge_enabled,
            auto_done,
            approval: Some(super::super::RunApproval {
                approved_by: "reviewer".into(),
                approved_at_ms: 1,
                auto_done_approved: auto_done,
                settings_version: auto_done.then_some(7),
                applicable_modes: vec![super::super::ExecutionMode::OneShot],
            }),
            allowed_operations: vec![AllowedOperation::Verify],
            publication_plan: PublicationPlan::NoPublication,
            workflow: WorkflowDefinitionSnapshot {
                definition_hash: "c".repeat(64),
                semantic_judge_worker_digest: None,
                template_id: Some("coding-agent".into()),
                materialized_definition_hash: Some("d".repeat(64)),
                registered_worker_id: Some("41".into()),
                workflow_execution_id: Some("workflow-41".into()),
                parent_job_id: Some("parent-41".into()),
                child_sandbox_job_id: Some("child-41".into()),
            },
            attempt_ids: vec!["attempt-1".into()],
        };
        store.create_run(run_snapshot).unwrap();
        let queued = store.transition(&run_id, 0, RunState::Queued).unwrap();
        let preparing = store
            .transition(&run_id, queued.revision, RunState::Preparing)
            .unwrap();
        let start = store
            .start_attempt(&run_id, preparing.revision, "attempt-1", "human-start")
            .unwrap();
        let running = store
            .mark_attempt_running(&run_id, start.run_revision, "attempt-1")
            .unwrap();
        let report = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "agent-report",
                "text/markdown",
                b"human review report",
            )
            .unwrap();
        store
            .transition(&run_id, running.revision, RunState::Finalizing)
            .unwrap();
        store
            .record_execution_evidence(
                &run_id,
                "attempt-1",
                RunEvidenceAssessment {
                    final_exit_code: Some(0),
                    end_present: true,
                    stream_error: false,
                    cancelled: false,
                    known_failure: false,
                    stdout_reference: None,
                    stdout_bytes: 0,
                    stderr_reference: None,
                    stderr_bytes: 0,
                    report_reference: Some(report.evidence_id),
                    report_save_confirmed: true,
                    evidence_integrity_verified: true,
                    commit_id: Some("e".repeat(40)),
                    commit_consistent: true,
                    acceptance_conditions_met,
                    validation: ValidationEvidence {
                        required: false,
                        state: ValidationState::Passed,
                    },
                },
            )
            .unwrap();
        let verifying = store.get_run(&run_id).unwrap().unwrap();
        let verifying = store
            .transition(&run_id, verifying.revision, RunState::Verifying)
            .unwrap();
        store
            .transition(&run_id, verifying.revision, RunState::AwaitingReview)
            .unwrap();
        let run = store.get_run(&run_id).unwrap().unwrap();
        let evidence_references = run
            .evidence
            .iter()
            .map(|item| {
                format!(
                    "agent-app://coding-agent/{}/{}/{}",
                    run_id, item.evidence_id, item.sha256
                )
            })
            .collect();
        let issue = TrackerIssueSnapshot {
            issue_id: run.snapshot.issue_id,
            state: IssueState::AwaitingReview,
            revision: 10,
            history: vec![IssueHistoryEntry {
                issue_id: "issue-human-review".into(),
                resulting_revision: 9,
                old_state: Some(IssueState::InProgress),
                new_state: IssueState::AwaitingReview,
                awaiting_review: Some(AwaitingReviewMarker {
                    run_id: run_id.to_string(),
                    full_commit_id: "e".repeat(40),
                    evidence_references,
                }),
                completion: None,
                actor_id: AUTO_COMPLETION_ACTOR_ID.into(),
                actor_kind: "system".into(),
                correlation_id: "review-marker".into(),
                reason: "completed execution".into(),
            }],
        };
        let tracker = Arc::new(HumanTestTracker {
            issue: Mutex::new(issue),
            identity: test_primary_identity(),
            lookup: Mutex::new(PrimaryOperationLookup::Unknown),
            history_operations: Mutex::new(HashMap::new()),
            after_transition: Mutex::new(None),
        });
        HumanFixture {
            _directory: directory,
            store,
            run_id,
            tracker,
        }
    }

    fn verified_report(summary: &str) -> DurableCompletionReport {
        use issue::{
            CompletionEvidence, CompletionEvidenceKind, VerifiedCompletion,
            completion_report::Result as ReportResult,
        };
        DurableCompletionReport {
            result: Some(ReportResult::Verified(VerifiedCompletion {
                evidence: vec![CompletionEvidence {
                    kind: CompletionEvidenceKind::OtherEvidence as i32,
                    summary: summary.into(),
                    uri: None,
                }],
            })),
        }
    }

    fn test_primary_identity() -> PrimaryTrackerIdentity {
        PrimaryTrackerIdentity {
            schema_fingerprint: "schema-fingerprint-v1".into(),
            data_root_id: "test-data-root".into(),
            contract_fingerprint: "contract-fingerprint-v1".into(),
        }
    }

    fn server_issued_manager_operation(
        request: &TrackerMutationRequest,
    ) -> manager::DurableOperation {
        manager::DurableOperation {
            operation_id: "server-issued-manager-operation-test".into(),
            scope: Some(manager::durable_operation::Scope::IssueId(
                request.issue_id.clone(),
            )),
            method: MANAGER_TRANSITION_METHOD.into(),
            idempotency_scope: completion_idempotency_scope(
                &request.primary_tracker_identity,
                &request.issue_id,
                &request.idempotency_key,
            ),
            request_digest: request.expected_manager_request_digest.clone(),
            request_schema: MANAGER_DURABLE_BACKEND_REQUEST_SCHEMA.into(),
            downstream_operation_id: "server-issued-downstream-operation-test".into(),
            state: manager::DurableOperationState::Pending as i32,
            commit_durability: issue::OperationCommitDurability::Durable as i32,
            result_replayability: issue::OperationResultReplayability::Replayable as i32,
            manager_sequence: request.expected_revision + 1,
            generation: 1,
            audit: Some(manager::ManagerOperationAudit {
                method: MANAGER_TRANSITION_METHOD.into(),
                target: format!("issue:{}", request.issue_id),
                data_root_id: request.primary_tracker_identity.data_root_id.clone(),
                contract_fingerprint: request
                    .primary_tracker_identity
                    .contract_fingerprint
                    .clone(),
                schema_fingerprint: request.primary_tracker_identity.schema_fingerprint.clone(),
                idempotency_key: request.idempotency_key.clone(),
                actor_audit: Some(manager_actor_audit(request)),
                correlation_id: request.correlation_id.clone(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn terminal_manager_failure(
        code: manager::ManagerErrorCode,
        retryable: bool,
        detail: Option<manager::manager_error::Detail>,
    ) -> manager::DurableOperation {
        manager::DurableOperation {
            operation_id: "server-issued-manager-failure-test".into(),
            state: manager::DurableOperationState::Failed as i32,
            failure_class: manager::FailureClass::Terminal as i32,
            commit_durability: issue::OperationCommitDurability::Durable as i32,
            result_replayability: issue::OperationResultReplayability::Replayable as i32,
            generation: 1,
            affects_ready: false,
            terminal_snapshot: Some(manager_error_snapshot(code as i32, retryable, detail)),
            ..Default::default()
        }
    }

    fn manager_error_snapshot(
        code: i32,
        retryable: bool,
        detail: Option<manager::manager_error::Detail>,
    ) -> manager::durable_operation::TerminalSnapshot {
        manager::durable_operation::TerminalSnapshot::ErrorSnapshot(manager::Snapshot {
            schema: "issue_tracker.manager.v1.ManagerError".into(),
            payload: manager::ManagerError {
                code,
                retryable,
                detail,
                ..Default::default()
            }
            .encode_to_vec(),
        })
    }

    fn server_committed_manager_operation(
        request: &TrackerMutationRequest,
    ) -> manager::DurableOperation {
        // Contract fixture from the Issue Tracker server's current write path:
        // NewOperation::build initializes manager_sequence from the pre-commit
        // storage value and commit_snapshot=None (manager/operations.rs:381-421).
        // Backend admission stores that operation payload before the storage
        // commit stamp (manager/repository/backend_outbox.rs:66-89); get_operation
        // decodes that payload unchanged (manager/repository/durable.rs:585-615).
        // On a confirmed transition, confirmed_backend_mutation removes the
        // operation and leaves the default ManagerMutation result (backend_outbox.rs:1354-1385),
        // which finalize_backend_dispatch persists as the result snapshot
        // (backend_outbox.rs:603-640).
        let mut operation = server_issued_manager_operation(request);
        operation.downstream_operation_id = format!("backend-{}", operation.operation_id);
        operation.state = manager::DurableOperationState::Committed as i32;
        operation.manager_sequence = 0;
        operation.terminal_snapshot = Some(
            manager::durable_operation::TerminalSnapshot::ResultSnapshot(manager::Snapshot {
                schema: "issue_tracker.manager.v1.ManagerMutation".into(),
                payload: manager::ManagerMutation::default().encode_to_vec(),
            }),
        );
        operation.commit_snapshot = None;
        operation
    }

    fn server_issued_primary_operation(
        authorization: &RunCompletionAuthorization,
        receipt: &PrimaryOperationReceipt,
    ) -> issue::OperationRecord {
        let payload_fingerprint = sha256(
            format!(
                "{TRACKER_TRANSITION_METHOD}\u{1f}{}",
                receipt.downstream_operation_id
            )
            .as_bytes(),
        );
        issue::OperationRecord {
            downstream_operation_id: receipt.downstream_operation_id.clone(),
            affected_issue_ids: vec![authorization.issue_id.clone()],
            resulting_revision: authorization.target_issue_revision.checked_add(1),
            payload_fingerprint: payload_fingerprint.clone(),
            contract_fingerprint: receipt.backend_contract_fingerprint.clone(),
            scope: issue::OperationScope::Issue as i32,
            method_key: TRACKER_TRANSITION_METHOD.into(),
            result_snapshot: Some(issue::OperationResultSnapshot {
                value: Some(issue::operation_result_snapshot::Value::IssueMutation(
                    issue::IssueMutationSnapshot {
                        current: Some(issue::IssueDetails {
                            issue: Some(issue::Issue {
                                issue_id: authorization.issue_id.clone(),
                                state: if authorization.action
                                    == CompletionActionKind::HumanCorrection
                                {
                                    issue::IssueState::Open as i32
                                } else {
                                    issue::IssueState::Done as i32
                                },
                                revision: authorization.target_issue_revision + 1,
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        previous_state: Some(issue::IssueState::AwaitingReview as i32),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }),
            commit_durability: issue::OperationCommitDurability::Durable as i32,
            result_replayability: issue::OperationResultReplayability::Replayable as i32,
            fingerprint_input: Some(issue::OperationFingerprintInput {
                downstream_operation_id: receipt.downstream_operation_id.clone(),
                method_key: TRACKER_TRANSITION_METHOD.into(),
                scope: issue::OperationScope::Issue as i32,
                payload_fingerprint,
            }),
            ..Default::default()
        }
    }

    fn test_operation_receipt(request: &TrackerMutationRequest) -> PrimaryOperationReceipt {
        PrimaryOperationReceipt {
            manager_operation_id: "server-issued-manager-operation-test".into(),
            downstream_operation_id: "server-issued-downstream-operation-test".into(),
            manager_idempotency_key: request.idempotency_key.clone(),
            idempotency_scope: completion_idempotency_scope(
                &request.primary_tracker_identity,
                &request.issue_id,
                &request.idempotency_key,
            ),
            request_digest: request.expected_manager_request_digest.clone(),
            request_schema: MANAGER_DURABLE_BACKEND_REQUEST_SCHEMA.into(),
            backend_contract_fingerprint: "backend-contract-fingerprint-v1".into(),
            manager_sequence: request.expected_revision + 1,
            generation: 1,
            server_identity: request.primary_tracker_identity.clone(),
        }
    }
}
