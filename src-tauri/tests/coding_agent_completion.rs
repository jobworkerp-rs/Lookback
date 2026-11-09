use std::{
    collections::HashMap,
    pin::Pin,
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use lookback_tauri_lib::{
    coding_agent::{
        AgentProfileSnapshot, AgentRunSnapshot, AllowedOperation, CompletionAuthority,
        CompletionOutcome, CompletionPolicy, CompletionTracker, DurableCompletionReport,
        DurableOperationClaim, DurableOperationOutcome, DurableOperationRequest,
        DurableOperationResult, ExecutionMode, ForgeKind, IssueHistoryEntry, IssueState,
        PublicationOutcome, PublicationPlan, RepositoryIdentity, RepositoryProvenance,
        RepositorySelection, ResolvedRepository, RunEvidenceAssessment, RunId, RunPurpose,
        RunState, RunStore, SandboxResources, TrackerIssueSnapshot, TrackerMutationRequest,
        TrackerResult, ValidationEvidence, ValidationState, WorkflowDefinitionSnapshot,
        completion::{
            PrimaryIssueTrackerAdapter, PrimaryOperationLookup, RecoveryDispatchPermission,
            TrackerHistoryOperation, TrackerIssueOperationSnapshot,
        },
        model::{PrimaryOperationReceipt, PrimaryTrackerIdentity},
    },
    data::DataPaths,
    grpc::{
        connect,
        issue_tracker::{IssueTrackerGrpcService, ManagerWriteRequest, render_server_config},
        proto::issue_tracker::{manager::v1 as manager, rpc::v1 as rpc, v1 as issue},
    },
};
use prost::Message;
use sha2::{Digest, Sha256};
use std::{process::Stdio, time::Duration};

mod common;

#[derive(Clone, Copy)]
enum MutationBehavior {
    Accept,
    CommitThenLoseResponse,
    CommitThenRejectedResponse,
    LoseBeforeCommit,
    LoseBeforeCommitThenAccept,
    ManagerErrorResponse,
}

struct FakeTracker {
    issue: Mutex<TrackerIssueSnapshot>,
    behavior: Mutex<MutationBehavior>,
    mutations: Mutex<Vec<TrackerMutationRequest>>,
    before_read: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    before_transition: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    after_transition: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    history_operations: Mutex<HashMap<(String, u64), String>>,
    lookup: Mutex<PrimaryOperationLookup>,
    primary_operation: Mutex<Option<PrimaryOperationReceipt>>,
    admitted_request: Mutex<Option<TrackerMutationRequest>>,
    manager_idempotency: Mutex<bool>,
}

#[async_trait]
impl CompletionTracker for FakeTracker {
    async fn read_issue_with_history(&self, issue_id: &str) -> TrackerResult<TrackerIssueSnapshot> {
        if let Some(hook) = self.before_read.lock().unwrap().take() {
            hook();
        }
        let issue = self.issue.lock().unwrap().clone();
        if issue.issue_id == issue_id {
            Ok(issue)
        } else {
            Err(lookback_tauri_lib::coding_agent::TrackerError::NotFound)
        }
    }

    async fn transition_issue(&self, request: TrackerMutationRequest) -> TrackerResult<()> {
        self.transition_issue_with_receipt(request)
            .await
            .map(|_| ())
    }

    fn primary_identity(&self) -> TrackerResult<PrimaryTrackerIdentity> {
        Ok(fake_primary_identity())
    }

    async fn transition_issue_with_receipt(
        &self,
        request: TrackerMutationRequest,
    ) -> TrackerResult<PrimaryOperationReceipt> {
        if let Some(hook) = self.before_transition.lock().unwrap().take() {
            hook();
        }
        if let Some(admitted) = self.admitted_request.lock().unwrap().as_ref() {
            if admitted == &request {
                self.mutations.lock().unwrap().push(request);
                return self
                    .primary_operation
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or(lookback_tauri_lib::coding_agent::TrackerError::Unconfirmed);
            }
            if admitted.idempotency_key == request.idempotency_key {
                self.mutations.lock().unwrap().push(request);
                return Err(lookback_tauri_lib::coding_agent::TrackerError::Rejected);
            }
        }
        self.mutations.lock().unwrap().push(request.clone());
        let attempt = self.mutations.lock().unwrap().len();
        let receipt = fake_operation_receipt(&request);
        match *self.behavior.lock().unwrap() {
            MutationBehavior::Accept => {
                self.apply(&request, &receipt);
                Ok(receipt)
            }
            MutationBehavior::CommitThenLoseResponse => {
                self.apply(&request, &receipt);
                Err(lookback_tauri_lib::coding_agent::TrackerError::Unconfirmed)
            }
            MutationBehavior::CommitThenRejectedResponse => {
                self.apply(&request, &receipt);
                Err(lookback_tauri_lib::coding_agent::TrackerError::Rejected)
            }
            MutationBehavior::LoseBeforeCommit => {
                Err(lookback_tauri_lib::coding_agent::TrackerError::Unavailable)
            }
            MutationBehavior::LoseBeforeCommitThenAccept if attempt == 1 => {
                Err(lookback_tauri_lib::coding_agent::TrackerError::Unavailable)
            }
            MutationBehavior::LoseBeforeCommitThenAccept => {
                self.apply(&request, &receipt);
                Ok(receipt)
            }
            MutationBehavior::ManagerErrorResponse => {
                Err(lookback_tauri_lib::coding_agent::TrackerError::Rejected)
            }
        }
    }

    async fn lookup_transition_operation(
        &self,
        _authorization: &lookback_tauri_lib::coding_agent::RunCompletionAuthorization,
    ) -> PrimaryOperationLookup {
        self.lookup.lock().unwrap().clone()
    }

    fn recovery_dispatch_permission(
        &self,
        authorization: &lookback_tauri_lib::coding_agent::RunCompletionAuthorization,
    ) -> Option<RecoveryDispatchPermission> {
        (*self.manager_idempotency.lock().unwrap()
            && authorization.primary_tracker_identity.as_ref() == Some(&fake_primary_identity())
            && authorization.primary_operation.is_none()
            && !authorization.expected_manager_request_digest.is_empty())
        .then_some(RecoveryDispatchPermission::SamePayloadIdempotentManager)
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

impl FakeTracker {
    fn apply(&self, request: &TrackerMutationRequest, receipt: &PrimaryOperationReceipt) {
        *self.admitted_request.lock().unwrap() = Some(request.clone());
        let mut issue = self.issue.lock().unwrap();
        let previous = issue.state;
        issue.state = request.target_state;
        issue.revision += 1;
        let revision = issue.revision;
        let issue_id = issue.issue_id.clone();
        issue.history.push(IssueHistoryEntry {
            issue_id: issue_id.clone(),
            resulting_revision: revision,
            old_state: Some(previous),
            new_state: request.target_state,
            awaiting_review: None,
            completion: request.completion.clone(),
            actor_id: request.actor_id.clone(),
            actor_kind: request.actor_kind.clone(),
            correlation_id: request.correlation_id.clone(),
            reason: request.audit_reason.clone(),
        });
        self.history_operations.lock().unwrap().insert(
            (issue_id, revision),
            receipt.downstream_operation_id.clone(),
        );
        *self.primary_operation.lock().unwrap() = Some(receipt.clone());
        *self.lookup.lock().unwrap() = PrimaryOperationLookup::Committed(receipt.clone());
        drop(issue);
        if let Some(hook) = self.after_transition.lock().unwrap().take() {
            hook();
        }
    }
}

#[tokio::test]
async fn approved_run_with_canonical_review_marker_completes_through_dedicated_authority() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "completion-intent-1")
        .await
        .unwrap();

    let outcome = authority.complete_auto(grant).await.unwrap();

    assert!(matches!(
        outcome,
        CompletionOutcome::Completed {
            issue_revision: 11,
            ..
        }
    ));
    let run = fixture.store.get_run(&fixture.run_id).unwrap().unwrap();
    assert_eq!(run.state, RunState::Done);
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
    assert_eq!(
        tracker.mutations.lock().unwrap()[0].target_state,
        IssueState::Done
    );
    assert_eq!(tracker.mutations.lock().unwrap()[0].expected_revision, 10);
}

#[tokio::test]
async fn review_uses_the_actual_gateway_references_after_additional_local_evidence() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    let references = vec!["urn:lookback:coding-agent:report:actual-digest".to_owned()];
    tracker.issue.lock().unwrap().history[0]
        .awaiting_review
        .as_mut()
        .unwrap()
        .evidence_references = references.clone();
    let ack = serde_json::json!({"schema_version":1,"run_id":fixture.run_id.as_str(),"attempt_id":"attempt-1","status":"completed","response":{
        "issue_id":"issue-17","revision":9,"run_id":fixture.run_id.as_str(),"full_commit_id":"e".repeat(40),"evidence_references":references,"lease_closed":true}});
    fixture
        .store
        .write_evidence(
            &fixture.run_id,
            "attempt-1",
            "issue-result-ack",
            "application/json",
            &serde_json::to_vec(&ack).unwrap(),
        )
        .unwrap();
    fixture
        .store
        .write_evidence(
            &fixture.run_id,
            "attempt-1",
            "later-local-observation",
            "text/plain",
            b"local information after the saved tracker marker",
        )
        .unwrap();
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "completion-intent-1")
        .await
        .unwrap();
    assert!(matches!(
        authority.complete_auto(grant).await.unwrap(),
        CompletionOutcome::Completed { .. }
    ));
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_malformed_or_different_execution_ack_cannot_authorize_review_completion() {
    for mismatch in [
        "issue_id",
        "run_id",
        "full_commit_id",
        "lease_closed",
        "revision",
        "marker_revision",
        "evidence_references",
        "invalid_json",
    ] {
        let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
        let tracker = tracker_for(&fixture);
        let run = fixture.store.get_run(&fixture.run_id).unwrap().unwrap();
        let references = tracker.issue.lock().unwrap().history[0]
            .awaiting_review
            .as_ref()
            .unwrap()
            .evidence_references
            .clone();
        let mut ack = serde_json::json!({"schema_version":1,"run_id":fixture.run_id.as_str(),"attempt_id":"attempt-1","status":"completed","response":{
            "issue_id":run.snapshot.issue_id,"revision":9,"run_id":fixture.run_id.as_str(),"full_commit_id":"e".repeat(40),"evidence_references":references,"lease_closed":true}});
        let field = if mismatch == "marker_revision" {
            "revision"
        } else {
            mismatch
        };
        ack["response"][field] = match mismatch {
            "lease_closed" => serde_json::json!(false),
            "revision" => serde_json::json!(run.snapshot.issue_revision),
            "marker_revision" => serde_json::json!(8),
            "evidence_references" => serde_json::json!([]),
            _ => serde_json::json!("different"),
        };
        let bytes = if mismatch == "invalid_json" {
            b"invalid".to_vec()
        } else {
            serde_json::to_vec(&ack).unwrap()
        };
        fixture
            .store
            .write_evidence(
                &fixture.run_id,
                "attempt-1",
                "issue-result-ack",
                "application/json",
                &bytes,
            )
            .unwrap();
        let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
        assert!(
            authority
                .authorize_auto_completion(&fixture.run_id, "completion-intent-1")
                .await
                .is_err()
        );
        assert!(tracker.mutations.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn exact_history_reconciles_a_lost_response_once_without_reissuing_mutation() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    *tracker.behavior.lock().unwrap() = MutationBehavior::CommitThenLoseResponse;
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "completion-intent-loss")
        .await
        .unwrap();

    let outcome = authority.complete_auto(grant).await.unwrap();

    assert!(matches!(outcome, CompletionOutcome::Completed { .. }));
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
    assert_eq!(
        fixture
            .store
            .get_run(&fixture.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Done
    );
}

#[tokio::test]
async fn committed_response_loss_retries_the_exact_cas_request_but_keeps_unreadable_proof_fenced() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    *tracker.behavior.lock().unwrap() = MutationBehavior::CommitThenLoseResponse;
    *tracker.after_transition.lock().unwrap() = Some({
        let tracker = tracker.clone();
        Arc::new(move || {
            *tracker.lookup.lock().unwrap() = PrimaryOperationLookup::Unknown;
        })
    });
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "committed-response-loss")
        .await
        .unwrap();

    assert!(authority.complete_auto(grant).await.is_err());
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
    assert_eq!(tracker.issue.lock().unwrap().revision, 11);
    assert_eq!(tracker.issue.lock().unwrap().state, IssueState::Done);

    let root = fixture._directory.path().to_path_buf();
    drop(authority);
    let reloaded_store = Arc::new(RunStore::open(&DataPaths::with_root(root)).unwrap());
    let recovered_authority = CompletionAuthority::new(
        reloaded_store,
        tracker.clone(),
        CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
    )
    .unwrap();
    let recovery_grant = recovered_authority
        .authorize_auto_completion(&fixture.run_id, "committed-response-loss")
        .await
        .unwrap();
    assert!(
        recovered_authority
            .complete_auto(recovery_grant)
            .await
            .is_err()
    );

    let requests = tracker.mutations.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[1].expected_revision, 10);
    assert_eq!(
        requests[0].expected_manager_request_digest,
        requests[1].expected_manager_request_digest
    );
    drop(requests);
    assert_eq!(tracker.issue.lock().unwrap().revision, 11);
    assert_eq!(
        tracker
            .issue
            .lock()
            .unwrap()
            .history
            .iter()
            .filter(|entry| entry.new_state == IssueState::Done)
            .count(),
        1,
        "the server-side idempotency replay has one observable Issue effect"
    );
    assert_eq!(
        fixture
            .store
            .get_run(&fixture.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::AwaitingReview,
        "a replay receipt without exact downstream and Issue-history proof is not completion"
    );
}

#[tokio::test]
async fn manager_error_after_admission_replays_once_with_same_request_and_keeps_fence() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    *tracker.behavior.lock().unwrap() = MutationBehavior::ManagerErrorResponse;
    *tracker.lookup.lock().unwrap() = PrimaryOperationLookup::Pending;
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "manager-error-is-ambiguous")
        .await
        .unwrap();

    assert!(authority.complete_auto(grant).await.is_err());
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
    let retry = authority
        .authorize_auto_completion(&fixture.run_id, "manager-error-is-ambiguous")
        .await
        .unwrap();
    assert!(authority.complete_auto(retry).await.is_err());
    assert_eq!(tracker.mutations.lock().unwrap().len(), 2);
    {
        let requests = tracker.mutations.lock().unwrap();
        assert_eq!(requests[0], requests[1]);
    }
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
async fn a_commit_followed_by_a_manager_error_is_reconciled_by_exact_operation() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    *tracker.behavior.lock().unwrap() = MutationBehavior::CommitThenRejectedResponse;
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "post-commit-manager-error")
        .await
        .unwrap();

    assert!(matches!(
        authority.complete_auto(grant).await.unwrap(),
        CompletionOutcome::Completed { .. }
    ));
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
    assert_eq!(
        fixture
            .store
            .get_run(&fixture.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Done
    );
}

#[tokio::test]
async fn only_an_exact_terminal_operation_receipt_can_revoke_the_dispatched_fence() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    *tracker.behavior.lock().unwrap() = MutationBehavior::ManagerErrorResponse;
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "terminal-rejection-receipt")
        .await
        .unwrap();
    assert!(authority.complete_auto(grant).await.is_err());
    let request = tracker.mutations.lock().unwrap()[0].clone();
    *tracker.lookup.lock().unwrap() =
        PrimaryOperationLookup::Rejected(Some(fake_operation_receipt(&request)));

    let recovery = authority
        .authorize_auto_completion(&fixture.run_id, "terminal-rejection-receipt")
        .await
        .unwrap();
    assert!(authority.complete_auto(recovery).await.is_err());
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
    assert!(
        authority
            .authorize_auto_completion(&fixture.run_id, "terminal-rejection-receipt")
            .await
            .is_err()
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

#[tokio::test]
async fn a_rejected_lookup_with_a_mutated_digest_or_scope_keeps_the_fence() {
    for mutate_scope in [false, true] {
        let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
        let tracker = tracker_for(&fixture);
        *tracker.behavior.lock().unwrap() = MutationBehavior::ManagerErrorResponse;
        let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
        let grant = authority
            .authorize_auto_completion(&fixture.run_id, "malformed-rejection-receipt")
            .await
            .unwrap();
        assert!(authority.complete_auto(grant).await.is_err());
        let request = tracker.mutations.lock().unwrap()[0].clone();
        let mut receipt = fake_operation_receipt(&request);
        if mutate_scope {
            receipt.idempotency_scope.push_str("wrong-root");
        } else {
            receipt.request_digest = "f".repeat(64);
        }
        *tracker.lookup.lock().unwrap() = PrimaryOperationLookup::Rejected(Some(receipt));

        let recovery = authority
            .authorize_auto_completion(&fixture.run_id, "malformed-rejection-receipt")
            .await
            .unwrap();
        assert!(authority.complete_auto(recovery).await.is_err());
        assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
        assert!(
            authority
                .authorize_auto_completion(&fixture.run_id, "malformed-rejection-receipt")
                .await
                .is_ok()
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
}

#[tokio::test]
async fn unrelated_done_history_never_completes_this_run() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    {
        let mut issue = tracker.issue.lock().unwrap();
        issue.state = IssueState::Done;
        issue.revision = 11;
        let issue_id = issue.issue_id.clone();
        issue.history.push(IssueHistoryEntry {
            issue_id,
            resulting_revision: 11,
            old_state: Some(IssueState::AwaitingReview),
            new_state: IssueState::Done,
            awaiting_review: None,
            completion: Some(verified_report("other run report")),
            actor_id: "different-actor".into(),
            actor_kind: "human".into(),
            correlation_id: "different-operation".into(),
            reason: "completed a different run".into(),
        });
    }
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);

    assert!(
        authority
            .authorize_auto_completion(&fixture.run_id, "completion-intent-unrelated-done")
            .await
            .is_err()
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
    assert!(tracker.mutations.lock().unwrap().is_empty());
}

#[tokio::test]
async fn investigation_report_can_complete_without_a_new_source_commit() {
    let fixture = prepared_run_with_publication_for_issue(
        RunFixtureOptions {
            issue_id: "issue-17",
            issue_revision: 5,
            publication_plan: PublicationPlan::NoPublication,
            publication_receipt: None,
            report_only: true,
            publication_base_branch: "main",
        },
        true,
        RunPurpose::Investigation,
        ExecutionMode::OneShot,
        false,
        false,
    );
    let tracker = tracker_for(&fixture);
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "report-only-completion")
        .await
        .unwrap();
    assert!(tracker.mutations.lock().unwrap().is_empty());
    assert!(matches!(
        authority.complete_auto(grant).await.unwrap(),
        CompletionOutcome::Completed { .. }
    ));
    let replay = authority
        .authorize_auto_completion(&fixture.run_id, "report-only-completion")
        .await
        .unwrap();
    assert!(matches!(
        authority.complete_auto(replay).await.unwrap(),
        CompletionOutcome::Completed { .. }
    ));
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
    assert_eq!(
        fixture
            .store
            .get_run(&fixture.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Done
    );
}

#[tokio::test]
async fn approved_investigation_can_request_completion_like_implementation() {
    let fixture = prepared_run(true, RunPurpose::Investigation, false, false);
    let tracker = tracker_for(&fixture);
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let authorization = authority
        .authorize_auto_completion(&fixture.run_id, "investigation-completion")
        .await;
    assert!(
        authorization.is_ok(),
        "investigation must use the common approval and result gates: {authorization:?}"
    );
    assert!(
        tracker.mutations.lock().unwrap().is_empty(),
        "authorization alone must not mutate the issue"
    );
}

#[tokio::test]
async fn false_auto_done_approval_is_denied() {
    for fixture in [prepared_run(
        false,
        RunPurpose::Implementation,
        false,
        false,
    )] {
        let tracker = tracker_for(&fixture);
        let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
        assert!(
            authority
                .authorize_auto_completion(&fixture.run_id, "denied-intent")
                .await
                .is_err()
        );
        assert!(tracker.mutations.lock().unwrap().is_empty());
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
}

#[tokio::test]
async fn required_validation_and_missing_interactive_host_finalization_are_denied() {
    let required_validation = prepared_run(true, RunPurpose::Implementation, true, true);
    let tracker = tracker_for(&required_validation);
    let required_authority = authority(
        &required_validation,
        tracker.clone(),
        "agent-app/coding-agent",
        1,
    );
    assert!(
        required_authority
            .authorize_auto_completion(&required_validation.run_id, "required-validation")
            .await
            .is_err()
    );

    let interactive = prepared_run_with_mode(true, ExecutionMode::Interactive);
    let tracker = tracker_for(&interactive);
    let interactive_authority =
        authority(&interactive, tracker.clone(), "agent-app/coding-agent", 1);
    assert!(
        interactive_authority
            .authorize_auto_completion(&interactive.run_id, "interactive-not-frozen")
            .await
            .is_err()
    );
    assert!(tracker.mutations.lock().unwrap().is_empty());
}

#[tokio::test]
async fn mismatched_run_commit_or_evidence_in_canonical_marker_is_denied() {
    for mutation in ["run", "commit", "evidence"] {
        let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
        let tracker = tracker_for(&fixture);
        {
            let mut issue = tracker.issue.lock().unwrap();
            let marker = issue
                .history
                .last_mut()
                .unwrap()
                .awaiting_review
                .as_mut()
                .unwrap();
            match mutation {
                "run" => marker.run_id = "another-run".into(),
                "commit" => marker.full_commit_id = "c".repeat(40),
                "evidence" => marker.evidence_references = vec!["evidence:other".into()],
                _ => unreachable!(),
            }
        }
        let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
        assert!(
            authority
                .authorize_auto_completion(&fixture.run_id, "marker-mismatch")
                .await
                .is_err()
        );
        assert!(tracker.mutations.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn stale_latest_issue_revision_is_refused_and_different_intent_payload_conflicts() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    let first_authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = first_authority
        .authorize_auto_completion(&fixture.run_id, "stale-intent")
        .await
        .unwrap();
    tracker.issue.lock().unwrap().revision += 1;
    assert!(first_authority.complete_auto(grant).await.is_err());
    assert!(tracker.mutations.lock().unwrap().is_empty());

    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    let first_authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let _grant = first_authority
        .authorize_auto_completion(&fixture.run_id, "stable-intent")
        .await
        .unwrap();
    tracker.issue.lock().unwrap().revision += 1;
    assert!(
        first_authority
            .authorize_auto_completion(&fixture.run_id, "stable-intent")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn trusted_policy_identity_mismatch_cannot_consume_a_durable_grant() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    let original = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = original
        .authorize_auto_completion(&fixture.run_id, "policy-bound-intent")
        .await
        .unwrap();
    let replacement = authority(&fixture, tracker.clone(), "other-policy", 1);

    assert!(replacement.complete_auto(grant).await.is_err());
    assert!(tracker.mutations.lock().unwrap().is_empty());
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
async fn unresolved_response_loss_uses_one_recovery_retry_then_stays_fenced() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    *tracker.behavior.lock().unwrap() = MutationBehavior::LoseBeforeCommit;
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "no-result-intent")
        .await
        .unwrap();

    assert!(authority.complete_auto(grant).await.is_err());
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
    let recovery_grant = authority
        .authorize_auto_completion(&fixture.run_id, "no-result-intent")
        .await
        .unwrap();
    assert!(authority.complete_auto(recovery_grant).await.is_err());
    assert_eq!(tracker.mutations.lock().unwrap().len(), 2);
    {
        let requests = tracker.mutations.lock().unwrap();
        assert_eq!(requests[0], requests[1]);
    }
    let exhausted_grant = authority
        .authorize_auto_completion(&fixture.run_id, "no-result-intent")
        .await
        .unwrap();
    assert!(authority.complete_auto(exhausted_grant).await.is_err());
    assert_eq!(tracker.mutations.lock().unwrap().len(), 2);
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
async fn history_with_matching_actor_report_revision_but_wrong_downstream_operation_is_not_proof() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    *tracker.behavior.lock().unwrap() = MutationBehavior::CommitThenLoseResponse;
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "wrong-downstream-operation")
        .await
        .unwrap();
    *tracker.after_transition.lock().unwrap() = Some({
        let tracker = tracker.clone();
        Arc::new(move || {
            tracker
                .history_operations
                .lock()
                .unwrap()
                .insert(("issue-17".into(), 11), "different-operation-id".into());
        })
    });

    assert!(authority.complete_auto(grant).await.is_err());
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
async fn historical_done_does_not_complete_when_canonical_issue_changes_before_proof_read() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "current-state-fence")
        .await
        .unwrap();
    *tracker.after_transition.lock().unwrap() = Some({
        let tracker = tracker.clone();
        Arc::new(move || {
            let mut issue = tracker.issue.lock().unwrap();
            issue.state = IssueState::Open;
            issue.revision += 1;
        })
    });

    assert!(authority.complete_auto(grant).await.is_err());
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
async fn ambiguous_precommit_dispatch_retries_once_with_the_same_request() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    *tracker.behavior.lock().unwrap() = MutationBehavior::LoseBeforeCommitThenAccept;
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "recover-same-intent")
        .await
        .unwrap();

    assert!(authority.complete_auto(grant).await.is_err());
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
    let root = fixture._directory.path().to_path_buf();
    drop(authority);
    let reloaded_store = Arc::new(RunStore::open(&DataPaths::with_root(root)).unwrap());
    let recovered_authority = CompletionAuthority::new(
        reloaded_store,
        tracker.clone(),
        CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
    )
    .unwrap();
    let recovery_grant = recovered_authority
        .authorize_auto_completion(&fixture.run_id, "recover-same-intent")
        .await
        .unwrap();

    assert!(matches!(
        recovered_authority
            .complete_auto(recovery_grant)
            .await
            .unwrap(),
        CompletionOutcome::Completed { .. }
    ));
    let mutations = tracker.mutations.lock().unwrap();
    assert_eq!(mutations.len(), 2);
    assert_eq!(mutations[0].idempotency_key, mutations[1].idempotency_key);
    assert_eq!(mutations[0], mutations[1]);
    assert_eq!(
        fixture
            .store
            .get_run(&fixture.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Done
    );
}

#[tokio::test]
async fn unresolved_lookup_states_retry_only_the_same_request_and_never_complete_without_proof() {
    for lookup in [
        PrimaryOperationLookup::Unknown,
        PrimaryOperationLookup::Pending,
        PrimaryOperationLookup::Rejected(None),
    ] {
        let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
        let tracker = tracker_for(&fixture);
        *tracker.behavior.lock().unwrap() = MutationBehavior::LoseBeforeCommit;
        let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
        let grant = authority
            .authorize_auto_completion(&fixture.run_id, "ambiguous-dispatch")
            .await
            .unwrap();
        assert!(authority.complete_auto(grant).await.is_err());
        *tracker.lookup.lock().unwrap() = lookup;
        let recovery_grant = authority
            .authorize_auto_completion(&fixture.run_id, "ambiguous-dispatch")
            .await
            .unwrap();

        assert!(authority.complete_auto(recovery_grant).await.is_err());
        {
            let requests = tracker.mutations.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0], requests[1]);
        }
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

    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    *tracker.behavior.lock().unwrap() = MutationBehavior::LoseBeforeCommit;
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "rejection-without-receipt")
        .await
        .unwrap();
    assert!(authority.complete_auto(grant).await.is_err());
    *tracker.lookup.lock().unwrap() = PrimaryOperationLookup::Rejected(None);
    let recovery_grant = authority
        .authorize_auto_completion(&fixture.run_id, "rejection-without-receipt")
        .await
        .unwrap();

    assert!(authority.complete_auto(recovery_grant).await.is_err());
    {
        let requests = tracker.mutations.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
    }
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
async fn one_durable_recovery_claim_allows_at_most_one_same_request_dispatch() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    *tracker.behavior.lock().unwrap() = MutationBehavior::LoseBeforeCommit;
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "retry-only-once")
        .await
        .unwrap();
    assert!(authority.complete_auto(grant).await.is_err());
    let recovery_grant = authority
        .authorize_auto_completion(&fixture.run_id, "retry-only-once")
        .await
        .unwrap();
    assert!(authority.complete_auto(recovery_grant).await.is_err());
    assert_eq!(tracker.mutations.lock().unwrap().len(), 2);

    let final_recovery_grant = authority
        .authorize_auto_completion(&fixture.run_id, "retry-only-once")
        .await
        .unwrap();
    assert!(authority.complete_auto(final_recovery_grant).await.is_err());
    assert_eq!(tracker.mutations.lock().unwrap().len(), 2);
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
async fn concurrent_recovery_calls_can_consume_only_one_durable_retry_claim() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    *tracker.behavior.lock().unwrap() = MutationBehavior::LoseBeforeCommit;
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let initial = authority
        .authorize_auto_completion(&fixture.run_id, "concurrent-same-intent-recovery")
        .await
        .unwrap();
    assert!(authority.complete_auto(initial).await.is_err());

    let first_recovery = authority
        .authorize_auto_completion(&fixture.run_id, "concurrent-same-intent-recovery")
        .await
        .unwrap();
    let second_recovery = authority
        .authorize_auto_completion(&fixture.run_id, "concurrent-same-intent-recovery")
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        authority.complete_auto(first_recovery),
        authority.complete_auto(second_recovery),
    );

    assert!(first.is_err());
    assert!(second.is_err());
    let requests = tracker.mutations.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "one initial and one claimed recovery dispatch"
    );
    assert_eq!(requests[0], requests[1]);
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
async fn tracker_without_pinned_manager_replay_capability_never_retries() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    *tracker.behavior.lock().unwrap() = MutationBehavior::LoseBeforeCommitThenAccept;
    *tracker.manager_idempotency.lock().unwrap() = false;
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "untrusted-replay-policy")
        .await
        .unwrap();
    assert!(authority.complete_auto(grant).await.is_err());

    let recovery = authority
        .authorize_auto_completion(&fixture.run_id, "untrusted-replay-policy")
        .await
        .unwrap();
    assert!(authority.complete_auto(recovery).await.is_err());
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
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
async fn completed_intent_replays_after_store_reload_only_while_current_issue_proof_matches() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    let first_authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = first_authority
        .authorize_auto_completion(&fixture.run_id, "reload-completed-intent")
        .await
        .unwrap();
    first_authority.complete_auto(grant).await.unwrap();

    let reloaded_store =
        Arc::new(RunStore::open(&DataPaths::with_root(fixture._directory.path())).unwrap());
    let restarted_authority = CompletionAuthority::new(
        reloaded_store,
        tracker.clone(),
        CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
    )
    .unwrap();
    let replay = restarted_authority
        .authorize_auto_completion(&fixture.run_id, "reload-completed-intent")
        .await
        .unwrap();
    assert!(matches!(
        restarted_authority.complete_auto(replay).await.unwrap(),
        CompletionOutcome::Completed { .. }
    ));
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);

    {
        let mut issue = tracker.issue.lock().unwrap();
        issue.state = IssueState::Open;
        issue.revision += 1;
    }
    assert!(
        restarted_authority
            .authorize_auto_completion(&fixture.run_id, "reload-completed-intent")
            .await
            .is_err()
    );
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "requires LOOKBACK_ISSUE_TRACKER_SERVER_BIN; starts an isolated local Issue Tracker server"]
async fn actual_primary_trusted_reads_complete_and_replay_an_authorized_run_after_reload() {
    use issue::completion_report::Result as CompletionResult;

    let root = tempfile::tempdir().unwrap();
    let data = DataPaths::with_root(root.path());
    let port = common::reserve_loopback_port();
    let config = render_server_config(&data, port).expect("render isolated server config");
    std::fs::write(config.config_path(), config.toml()).expect("write isolated config");

    // Use only a child-local working directory and a scrubbed environment. The
    // server writes both stores under this TempDir; no developer .env or
    // external Issue Tracker endpoint is consulted.
    let binary = std::env::var_os("LOOKBACK_ISSUE_TRACKER_SERVER_BIN")
        .expect("ignored contract test requires LOOKBACK_ISSUE_TRACKER_SERVER_BIN");
    let mut child = tokio::process::Command::new(binary)
        .arg("--config")
        .arg(config.config_path())
        .current_dir(root.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn isolated Issue Tracker server");
    let service = IssueTrackerGrpcService::new(config.listen_url());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if connect(&config.listen_url()).await.is_ok() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "isolated Issue Tracker server did not start"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    service
        .run_outbox_cycle("coding-agent-completion-bootstrap")
        .await
        .expect("bootstrap local server");
    service
        .health("coding-agent-completion-health")
        .await
        .expect("local server is ready");

    let issue_id = "01234567-89ab-8def-8123-456789abcdef";
    let create = service
        .create_issue(ManagerWriteRequest {
            correlation_id: "coding-agent-contract-create".into(),
            idempotency_key: "coding-agent-contract-create".into(),
            actor_audit: Some(manager::ActorAudit {
                actor_id: "contract-test-reviewer".into(),
                actor_kind: issue::ActorKind::Human as i32,
                reason: Some("seed local completion contract Issue".into()),
                ..Default::default()
            }),
            payload: manager::CreateIssueArgs {
                issue_id: issue_id.into(),
                issue: Some(issue::IssueDraft {
                    title: "Local completion contract Issue".into(),
                    kind: issue::IssueKind::Task as i32,
                    priority: issue::IssuePriority::P2 as i32,
                    creation_basis: issue::CreationBasis::Explicit as i32,
                    confirmation_status: issue::ConfirmationStatus::Confirmed as i32,
                    initial_state: issue::IssueState::Open as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
        })
        .await
        .expect("create canonical Issue on isolated server");
    service
        .run_outbox_cycle("coding-agent-contract-create-outbox")
        .await
        .expect("materialize canonical Issue");
    let revision_update = service
        .update_issue(ManagerWriteRequest {
            correlation_id: "coding-agent-contract-revision".into(),
            idempotency_key: "coding-agent-contract-revision".into(),
            actor_audit: Some(manager::ActorAudit {
                actor_id: "contract-test-reviewer".into(),
                actor_kind: issue::ActorKind::Human as i32,
                reason: Some("give the canonical fixture a positive revision".into()),
                ..Default::default()
            }),
            payload: manager::UpdateIssueArgs {
                issue_id: issue_id.into(),
                expected_revision: Some(0),
                update_mask: vec![manager::ManagerIssueField::Title as i32],
                patch: Some(manager::ManagerIssuePatch {
                    title: Some("Local completion contract Issue revision 1".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        })
        .await
        .expect("advance canonical issue revision");
    service
        .run_outbox_cycle("coding-agent-contract-revision-outbox")
        .await
        .expect("materialize canonical revision");
    let initial_issue = service
        .get_issue_with_sync(
            "coding-agent-contract-created-issue",
            manager::GetIssueWithSyncArgs {
                issue_id: issue_id.into(),
                ..Default::default()
            },
        )
        .await
        .expect("read canonical Issue")
        .issue
        .expect("created Issue");
    assert!(initial_issue.revision > 0);
    assert_eq!(
        create.operation.unwrap().state,
        manager::DurableOperationState::Committed as i32
    );
    assert_eq!(
        revision_update.operation.unwrap().state,
        manager::DurableOperationState::Committed as i32
    );

    let fixture = prepared_run_with_publication_for_issue(
        RunFixtureOptions {
            issue_id,
            issue_revision: initial_issue.revision,
            publication_plan: PublicationPlan::NoPublication,
            publication_receipt: None,
            report_only: false,
            publication_base_branch: "main",
        },
        true,
        RunPurpose::Implementation,
        ExecutionMode::OneShot,
        false,
        false,
    );
    let run = fixture
        .store
        .get_run(&fixture.run_id)
        .unwrap()
        .expect("prepared run");
    let evidence_references = run
        .evidence
        .iter()
        .filter(|entry| Some(entry.attempt_id.as_str()) == run.current_attempt_id.as_deref())
        .map(|entry| {
            format!(
                "agent-app://coding-agent/{}/{}/{}",
                fixture.run_id, entry.evidence_id, entry.sha256
            )
        })
        .collect::<Vec<_>>();
    assert!(!evidence_references.is_empty());

    service
        .register_and_verify_gateway_policy(
            &config.gateway_control_url(),
            "coding-agent-contract-gateway-policy",
            "coding-agent-contract-gateway-policy",
            manager::WorkEligibilityMode::ReviewedOnly,
        )
        .await
        .expect("register isolated Gateway policy");
    let channel = connect(&config.gateway_worker_url())
        .await
        .expect("connect isolated Gateway");
    let mut gateway = rpc::issue_gateway_client::IssueGatewayClient::new(channel);
    let claim = gateway
        .claim(rpc::IssueGatewayClaimRequest {
            issue_id: issue_id.into(),
            expected_revision: Some(initial_issue.revision),
            idempotency_key: "coding-agent-contract-review-claim".into(),
        })
        .await
        .expect("claim local Issue")
        .into_inner();
    let claim = match claim.outcome {
        Some(rpc::issue_gateway_claim_response::Outcome::Success(value)) => value,
        other => panic!("expected local Gateway claim, got {other:?}"),
    };
    let review_marker = issue::AwaitingReviewCompletion {
        run_id: fixture.run_id.to_string(),
        full_commit_id: "e".repeat(40),
        evidence_references,
        report: Some(issue::CompletionReport {
            result: Some(CompletionResult::Verified(issue::VerifiedCompletion {
                evidence: vec![issue::CompletionEvidence {
                    kind: issue::CompletionEvidenceKind::OtherEvidence as i32,
                    summary: "local contract fixture records verified run evidence".into(),
                    uri: None,
                }],
            })),
        }),
    };
    let marker = service
        .complete_execution_to_awaiting_review(
            &config.gateway_worker_url(),
            issue_id,
            Some(&claim.capability_id),
            claim.revision,
            "coding-agent-contract-awaiting-review",
            review_marker,
        )
        .await
        .expect("write exact local AWAITING_REVIEW marker");
    assert!(marker.revision > initial_issue.revision);
    let adapter = Arc::new(PrimaryIssueTrackerAdapter::new(service.clone()));
    let authority = CompletionAuthority::new(
        fixture.store.clone(),
        adapter,
        CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
    )
    .unwrap();
    let Fixture {
        _directory,
        store,
        run_id,
    } = fixture;
    let completion_intent = "actual-server-completion";
    let grant = authority
        .authorize_auto_completion(&run_id, completion_intent)
        .await
        .expect("trusted primary Issue history authorizes the approved run");
    let completed = match authority.complete_auto(grant).await {
        Ok(completed) => completed,
        Err(_) => {
            // The isolated Manager may admit the operation before its outbox
            // worker applies it; an explicit recovery call must observe proof.
            service
                .run_outbox_cycle("coding-agent-contract-completion-outbox")
                .await
                .expect("drive the isolated Manager outbox");
            let recovery = authority
                .authorize_auto_completion(&run_id, completion_intent)
                .await
                .expect("the same durable completion intent remains recoverable");
            authority
                .complete_auto(recovery)
                .await
                .expect("trusted downstream proof confirms the same completion")
        }
    };
    assert!(matches!(
        completed,
        CompletionOutcome::Completed {
            issue_revision,
            ..
        } if issue_revision == marker.revision + 1
    ));
    assert_eq!(
        store.get_run(&run_id).unwrap().unwrap().state,
        RunState::Done
    );

    let history = service
        .list_trusted_issue_history("coding-agent-contract-final-history", issue_id)
        .await
        .expect("read canonical history through ManagerTrustedReadService");
    assert!(history.iter().any(|entry| {
        entry.resulting_revision == marker.revision
            && entry.state_change.as_ref().is_some_and(|state| {
                state.new_state == issue::IssueState::AwaitingReview as i32
                    && state.awaiting_review.as_ref().is_some_and(|saved| {
                        saved.run_id == run_id.to_string() && saved.full_commit_id == "e".repeat(40)
                    })
            })
    }));
    assert!(history.iter().any(|entry| {
        entry.resulting_revision == marker.revision + 1
            && entry
                .state_change
                .as_ref()
                .is_some_and(|state| state.new_state == issue::IssueState::Done as i32)
    }));

    let current_issue = service
        .get_issue_with_sync(
            "coding-agent-contract-final-issue",
            manager::GetIssueWithSyncArgs {
                issue_id: issue_id.into(),
                ..Default::default()
            },
        )
        .await
        .expect("read completed canonical Issue")
        .issue
        .expect("canonical Issue remains present");
    assert_eq!(current_issue.state, issue::IssueState::Done as i32);
    assert_eq!(current_issue.revision, marker.revision + 1);

    let manager_history_args = manager::ListOperationHistoryArgs {
        page: Some(manager::PageRequest {
            page_size: 100,
            ..Default::default()
        }),
        filter: Some(manager::OperationHistoryFilter {
            direct_target: Some(manager::operation_history_filter::DirectTarget::IssueId(
                issue_id.into(),
            )),
            method: Some("manager_transition_issue".into()),
            direct_scope_only: true,
            ..Default::default()
        }),
        include_total_size: Some(true),
        ..Default::default()
    };
    let manager_history = service
        .list_operation_history(
            "coding-agent-contract-manager-history",
            manager_history_args.clone(),
        )
        .await
        .expect("read the actual Manager transition row");
    let completion_rows = manager_history
        .entries
        .iter()
        .filter(|entry| entry.correlation_id.starts_with("coding-agent-completion-"))
        .collect::<Vec<_>>();
    assert_eq!(
        completion_rows.len(),
        1,
        "one Manager completion was admitted"
    );
    let completion_row = completion_rows[0];
    let manager_operation = service
        .get_operation(
            &completion_row.correlation_id,
            completion_row.operation_id.clone(),
        )
        .await
        .expect("read the committed Manager completion");
    assert_eq!(
        manager_operation.state,
        manager::DurableOperationState::Committed as i32
    );
    assert!(manager_operation.commit_snapshot.is_none());
    let Some(manager::durable_operation::TerminalSnapshot::ResultSnapshot(result)) =
        manager_operation.terminal_snapshot.as_ref()
    else {
        panic!("server did not return its terminal result snapshot");
    };
    assert_eq!(result.schema, "issue_tracker.manager.v1.ManagerMutation");
    assert_eq!(
        manager::ManagerMutation::decode(result.payload.as_slice()).unwrap(),
        manager::ManagerMutation::default()
    );

    let completed_history = history
        .iter()
        .find(|entry| entry.resulting_revision == marker.revision + 1)
        .expect("the exact DONE history event exists");
    let completion_report = completed_history
        .state_change
        .as_ref()
        .and_then(|state| state.completion.clone())
        .expect("the DONE history event contains the completion report");
    let audit = manager_operation
        .audit
        .as_ref()
        .expect("Manager audit exists");
    let replay_request = ManagerWriteRequest {
        correlation_id: audit.correlation_id.clone(),
        idempotency_key: audit.idempotency_key.clone(),
        actor_audit: audit.actor_audit.clone(),
        payload: manager::TransitionIssueArgs {
            issue_id: issue_id.into(),
            expected_revision: Some(marker.revision),
            target_state: issue::IssueState::Done as i32,
            terminal_report: Some(manager::transition_issue_args::TerminalReport::Completion(
                completion_report,
            )),
            ..Default::default()
        },
    };
    let replayed_transition = service
        .transition_issue(replay_request)
        .await
        .expect("exact same-key Manager request replays before the stale Issue CAS");
    let replayed_operation = replayed_transition
        .operation
        .expect("replay returns the original Manager operation");
    assert_eq!(
        replayed_operation.operation_id,
        manager_operation.operation_id
    );
    assert_eq!(
        replayed_operation.state,
        manager::DurableOperationState::Committed as i32
    );
    let after_replay = service
        .get_issue_with_sync(
            "coding-agent-contract-after-manager-replay",
            manager::GetIssueWithSyncArgs {
                issue_id: issue_id.into(),
                ..Default::default()
            },
        )
        .await
        .expect("read Issue after exact Manager replay")
        .issue
        .expect("canonical Issue remains present");
    assert_eq!(after_replay.state, issue::IssueState::Done as i32);
    assert_eq!(after_replay.revision, marker.revision + 1);
    let manager_history_after_replay = service
        .list_operation_history(
            "coding-agent-contract-manager-history-after-replay",
            manager_history_args,
        )
        .await
        .expect("confirm replay admitted no second Manager operation");
    assert_eq!(
        manager_history_after_replay.entries.len(),
        manager_history.entries.len()
    );

    drop(authority);
    drop(store);
    let reloaded_store =
        Arc::new(RunStore::open(&DataPaths::with_root(_directory.path())).unwrap());
    let reloaded_authority = CompletionAuthority::new(
        reloaded_store,
        Arc::new(PrimaryIssueTrackerAdapter::new(service.clone())),
        CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
    )
    .unwrap();
    let replayed = reloaded_authority
        .authorize_auto_completion(&run_id, completion_intent)
        .await
        .expect("completed authorization survives the local store reload");
    assert!(matches!(
        reloaded_authority.complete_auto(replayed).await.unwrap(),
        CompletionOutcome::Completed {
            issue_revision,
            ..
        } if issue_revision == marker.revision + 1
    ));
    let final_history = service
        .list_trusted_issue_history("coding-agent-contract-replay-history", issue_id)
        .await
        .expect("re-read trusted history after local store reload");
    assert_eq!(final_history.len(), history.len());

    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[test]
fn cancel_claim_before_publication_admission_prevents_a_publication_permit() {
    let fixture = publication_inflight_run();
    fixture
        .store
        .claim_effect(
            &fixture.run_id,
            "attempt-1",
            "cancel-before-publish",
            DurableOperationRequest::CancelAgent,
        )
        .unwrap();
    let run = fixture.store.get_run(&fixture.run_id).unwrap().unwrap();

    assert!(
        fixture
            .store
            .claim_publication_effect(
                &fixture.run_id,
                run.revision,
                "attempt-1",
                "publish-after-cancel",
                DurableOperationRequest::PublishBranch {
                    branch: "agent/completion-run-1".into(),
                    commit_id: "e".repeat(40),
                },
            )
            .is_err()
    );
}

#[test]
fn cross_process_cancel_fences_accepted_publication_receipt_and_prevents_replay() {
    let fixture = publication_inflight_run();
    let run = fixture.store.get_run(&fixture.run_id).unwrap().unwrap();
    let publication = fixture
        .store
        .claim_publication_effect(
            &fixture.run_id,
            run.revision,
            "attempt-1",
            "publish-once",
            DurableOperationRequest::PublishBranch {
                branch: "agent/completion-run-1".into(),
                commit_id: "e".repeat(40),
            },
        )
        .unwrap();
    assert!(publication.newly_created);
    let permit = publication
        .permit
        .expect("new publication owns a sealed permit");

    // A second Store connection models another app/coordinator process.
    let cancel_store = RunStore::open(&DataPaths::with_root(fixture._directory.path())).unwrap();
    cancel_store
        .claim_effect(
            &fixture.run_id,
            "attempt-1",
            "cancel-after-publish-admission",
            DurableOperationRequest::CancelAgent,
        )
        .unwrap();
    let receipt_bytes = b"accepted remote publication receipt";
    let receipt = fixture
        .store
        .write_evidence(
            &fixture.run_id,
            "attempt-1",
            "publication-receipt",
            "application/octet-stream",
            receipt_bytes,
        )
        .unwrap();
    let result = fixture
        .store
        .complete_publication_effect(
            permit,
            DurableOperationResult {
                outcome: DurableOperationOutcome::Succeeded,
                result_digest: Some(format!("{:x}", Sha256::digest(receipt_bytes))),
                evidence_ids: vec![receipt.evidence_id.clone()],
            },
        )
        .unwrap();

    assert_eq!(
        result.status,
        lookback_tauri_lib::coding_agent::DurableOperationStatus::ResultUnconfirmed
    );
    let stored_result = result.result.unwrap();
    assert_eq!(
        stored_result.outcome,
        DurableOperationOutcome::ResultUnconfirmed
    );
    assert_eq!(
        stored_result.evidence_ids,
        vec![receipt.evidence_id.clone()]
    );
    assert_eq!(
        fixture
            .store
            .read_evidence(&fixture.run_id, &receipt.evidence_id)
            .unwrap(),
        receipt_bytes
    );

    let replay = fixture
        .store
        .claim_publication_effect(
            &fixture.run_id,
            run.revision,
            "attempt-1",
            "publish-once",
            DurableOperationRequest::PublishBranch {
                branch: "agent/completion-run-1".into(),
                commit_id: "e".repeat(40),
            },
        )
        .unwrap();
    assert!(!replay.newly_created);
    assert!(replay.permit.is_none());
    assert!(
        fixture
            .store
            .claim_publication_effect(
                &fixture.run_id,
                run.revision,
                "attempt-1",
                "publish-fresh-key-cannot-bypass-fence",
                DurableOperationRequest::PublishBranch {
                    branch: "agent/completion-run-1".into(),
                    commit_id: "e".repeat(40),
                },
            )
            .is_err()
    );
}

#[tokio::test]
async fn no_source_changes_completion_uses_the_approved_non_main_base_and_rejects_wrong_scope() {
    for wrong_scope in [false, true] {
        let source = coding_agent_executor::SourceInspection {
            run_id: "completion-run-1".into(),
            attempt_id: if wrong_scope { "other" } else { "attempt-1" }.into(),
            base_commit: "b".repeat(40),
            head_commit: "e".repeat(40),
            branch: "agent/completion-run-1".into(),
            tracked_diff: Vec::new(),
            uncommitted_diff: Some(Vec::new()),
            new_files: Vec::new(),
        };
        let fixture = prepared_run_with_contract(
            RunFixtureOptions {
                issue_id: "issue-17",
                issue_revision: 5,
                publication_plan: PublicationPlan::CreatePullRequest,
                publication_receipt: Some(PublicationOutcome::NoSourceChanges { source }),
                report_only: false,
                publication_base_branch: "trunk",
            },
            true,
            RunPurpose::Implementation,
            ExecutionMode::OneShot,
            false,
            false,
            true,
        );
        let tracker = tracker_for(&fixture);
        let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
        let grant = authority
            .authorize_auto_completion(&fixture.run_id, "non-main-no-source-changes")
            .await;
        if wrong_scope {
            assert!(grant.is_err());
            assert!(tracker.mutations.lock().unwrap().is_empty());
        } else {
            assert!(matches!(
                authority.complete_auto(grant.unwrap()).await.unwrap(),
                CompletionOutcome::Completed { .. }
            ));
            assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
            assert_eq!(
                fixture
                    .store
                    .get_run(&fixture.run_id)
                    .unwrap()
                    .unwrap()
                    .state,
                RunState::Done
            );
        }
    }
}

#[tokio::test]
async fn matching_no_changes_tree_proof_allows_auto_completion_for_publication_plan() {
    let fixture = prepared_run_with_publication(
        true,
        RunPurpose::Implementation,
        ExecutionMode::OneShot,
        false,
        false,
        PublicationPlan::PushBranch,
        Some(PublicationOutcome::NoChangesWithTreeProof {
            base_commit: "b".repeat(40),
            frozen_commit: "e".repeat(40),
            base_tree_id: "f".repeat(40),
            frozen_tree_id: "f".repeat(40),
        }),
    );
    let tracker = tracker_for(&fixture);
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "no-change-proof-intent")
        .await
        .unwrap();

    assert!(matches!(
        authority.complete_auto(grant).await.unwrap(),
        CompletionOutcome::Completed { .. }
    ));
    assert_eq!(
        fixture
            .store
            .get_run(&fixture.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Done
    );
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn no_changes_receipt_without_matching_frozen_tree_proof_is_denied() {
    let fixture = prepared_run_with_publication(
        true,
        RunPurpose::Implementation,
        ExecutionMode::OneShot,
        false,
        false,
        PublicationPlan::PushBranch,
        Some(PublicationOutcome::NoChangesWithTreeProof {
            base_commit: "b".repeat(40),
            frozen_commit: "e".repeat(40),
            base_tree_id: "f".repeat(40),
            frozen_tree_id: "a".repeat(40),
        }),
    );
    let tracker = tracker_for(&fixture);
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);

    assert!(
        authority
            .authorize_auto_completion(&fixture.run_id, "unproven-no-change-intent")
            .await
            .is_err()
    );
    assert!(tracker.mutations.lock().unwrap().is_empty());
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
async fn evidence_added_during_tracker_read_invalidates_completion_before_dispatch() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "admission-evidence-race")
        .await
        .unwrap();
    let store = fixture.store.clone();
    let run_id = fixture.run_id.clone();
    let evidence_write_succeeded = Arc::new(Mutex::new(None));
    let write_result = evidence_write_succeeded.clone();
    *tracker.before_read.lock().unwrap() = Some(Arc::new(move || {
        let result = store.write_evidence(
            &run_id,
            "attempt-1",
            "late-evidence",
            "text/plain",
            b"evidence added after tracker read began",
        );
        *write_result.lock().unwrap() = Some(result.is_ok());
    }));

    assert!(authority.complete_auto(grant).await.is_err());
    assert_eq!(*evidence_write_succeeded.lock().unwrap(), Some(true));
    assert!(tracker.mutations.lock().unwrap().is_empty());
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
async fn admitted_completion_fences_evidence_writes_until_tracker_reconciliation() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, "admission-evidence-fence")
        .await
        .unwrap();
    let store = fixture.store.clone();
    let run_id = fixture.run_id.clone();
    let mutation_results = Arc::new(Mutex::new(None));
    let results = mutation_results.clone();
    *tracker.before_transition.lock().unwrap() = Some(Arc::new(move || {
        let run = store.get_run(&run_id).unwrap().unwrap();
        let evidence_write_blocked = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "late-evidence",
                "text/plain",
                b"evidence must be fenced after dispatch admission",
            )
            .is_err();
        let assessment_write_blocked = store
            .record_execution_evidence(&run_id, "attempt-1", run.execution_evidence.unwrap())
            .is_err();
        let receipt_write_blocked = store
            .complete_operation(
                &run_id,
                "start-attempt-1",
                DurableOperationResult {
                    outcome: DurableOperationOutcome::Succeeded,
                    result_digest: None,
                    evidence_ids: Vec::new(),
                },
            )
            .is_err();
        let attempt_write_blocked = store
            .start_attempt(&run_id, run.revision, "attempt-1", "fenced-start-attempt")
            .is_err();
        let transition_write_blocked = store
            .transition(&run_id, run.revision, RunState::Unknown)
            .is_err();
        *results.lock().unwrap() = Some([
            evidence_write_blocked,
            assessment_write_blocked,
            receipt_write_blocked,
            attempt_write_blocked,
            transition_write_blocked,
        ]);
    }));

    assert!(matches!(
        authority.complete_auto(grant).await.unwrap(),
        CompletionOutcome::Completed { .. }
    ));
    assert_eq!(
        *mutation_results.lock().unwrap(),
        Some([true, true, true, true, true])
    );
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
    assert_eq!(
        fixture
            .store
            .get_run(&fixture.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Done
    );
}

#[test]
fn same_phase_effect_claims_are_unique_per_kind_across_store_connections() {
    let cancel = DurableOperationRequest::CancelAgent;
    let finish = DurableOperationRequest::FinishInteractive { profile_version: 1 };

    for (requests, expected_winner_kind) in [
        (
            [
                ("cancel-key-a", cancel.clone()),
                ("cancel-key-b", cancel.clone()),
            ],
            "cancel",
        ),
        (
            [
                ("finish-key-a", finish.clone()),
                ("finish-key-b", finish.clone()),
            ],
            "finish",
        ),
    ] {
        let fixture = prepared_run_with_mode(false, ExecutionMode::Interactive);
        fixture
            .store
            .request_interactive_finish(&fixture.run_id, "attempt-1")
            .unwrap();
        let outcomes = race_effect_claims(&fixture, requests);
        let winners = outcomes
            .into_iter()
            .filter_map(Result::ok)
            .filter(|claim| claim.newly_created)
            .collect::<Vec<_>>();
        assert_eq!(
            winners.len(),
            1,
            "one operation may receive dispatch authority"
        );

        let winner = &winners[0];
        let winner_kind = match winner.operation.request {
            DurableOperationRequest::CancelAgent => "cancel",
            DurableOperationRequest::FinishInteractive { .. } => "finish",
            _ => panic!("only cancellation and finish are expected"),
        };
        assert_eq!(winner_kind, expected_winner_kind);
        let replay = fixture
            .store
            .claim_effect(
                &fixture.run_id,
                "attempt-1",
                &winner.operation.idempotency_key,
                winner.operation.request.clone(),
            )
            .unwrap();
        assert!(!replay.newly_created, "exact replay returns a receipt only");

        let other_key = match winner_kind {
            "cancel" => "different-cancel-key",
            _ => "different-finish-key",
        };
        assert!(
            fixture
                .store
                .claim_effect(
                    &fixture.run_id,
                    "attempt-1",
                    other_key,
                    winner.operation.request.clone(),
                )
                .is_err()
        );
    }
}

#[test]
fn phase_scoped_cancel_and_finish_claims_follow_the_cancellation_fence_in_order() {
    let cancel = DurableOperationRequest::CancelAgent;
    let finish = DurableOperationRequest::FinishInteractive { profile_version: 1 };

    let cancel_first = prepared_run_with_mode(false, ExecutionMode::Interactive);
    cancel_first
        .store
        .request_interactive_finish(&cancel_first.run_id, "attempt-1")
        .unwrap();
    let cancel_claim = cancel_first
        .store
        .claim_effect(
            &cancel_first.run_id,
            "attempt-1",
            "cancel-first",
            cancel.clone(),
        )
        .unwrap();
    assert!(cancel_claim.newly_created);
    assert!(
        cancel_first
            .store
            .claim_effect(
                &cancel_first.run_id,
                "attempt-1",
                "finish-after-cancel",
                finish.clone(),
            )
            .is_err(),
        "a cancellation claim fences a later finish"
    );
    assert!(
        !cancel_first
            .store
            .claim_effect(
                &cancel_first.run_id,
                "attempt-1",
                "cancel-first",
                cancel.clone(),
            )
            .unwrap()
            .newly_created
    );
    assert!(
        cancel_first
            .store
            .claim_effect(
                &cancel_first.run_id,
                "attempt-1",
                "second-cancel-key",
                cancel.clone(),
            )
            .is_err(),
        "a second idempotency key cannot create a second cancellation"
    );

    let finish_first = prepared_run_with_mode(false, ExecutionMode::Interactive);
    finish_first
        .store
        .request_interactive_finish(&finish_first.run_id, "attempt-1")
        .unwrap();
    let finish_claim = finish_first
        .store
        .claim_effect(
            &finish_first.run_id,
            "attempt-1",
            "finish-first",
            finish.clone(),
        )
        .unwrap();
    assert!(finish_claim.newly_created);
    let cancel_after_finish = finish_first
        .store
        .claim_effect(
            &finish_first.run_id,
            "attempt-1",
            "cancel-after-finish",
            cancel.clone(),
        )
        .unwrap();
    assert!(cancel_after_finish.newly_created);
    assert!(
        finish_first
            .store
            .claim_effect(
                &finish_first.run_id,
                "attempt-1",
                "second-finish-key",
                finish.clone(),
            )
            .is_err(),
        "finish remains unique after cancellation is admitted"
    );
    assert!(
        !finish_first
            .store
            .claim_effect(&finish_first.run_id, "attempt-1", "finish-first", finish,)
            .unwrap()
            .newly_created
    );
    assert!(
        !finish_first
            .store
            .claim_effect(
                &finish_first.run_id,
                "attempt-1",
                "cancel-after-finish",
                cancel,
            )
            .unwrap()
            .newly_created
    );
}

#[test]
fn racing_finish_and_cancel_always_claims_cancel_and_allows_only_one_of_each_kind() {
    let cancel = DurableOperationRequest::CancelAgent;
    let finish = DurableOperationRequest::FinishInteractive { profile_version: 1 };
    let fixture = prepared_run_with_mode(false, ExecutionMode::Interactive);
    fixture
        .store
        .request_interactive_finish(&fixture.run_id, "attempt-1")
        .unwrap();

    let [cancel_result, finish_result] = race_effect_claims(
        &fixture,
        [
            ("racing-cancel", cancel.clone()),
            ("racing-finish", finish.clone()),
        ],
    );
    let cancel_claim = cancel_result.expect("cancellation is admitted in either claim order");
    assert!(cancel_claim.newly_created);
    assert_eq!(cancel_claim.operation.request, cancel);
    let finish_claim = match finish_result {
        Ok(claim) => {
            assert!(claim.newly_created);
            assert_eq!(claim.operation.request, finish);
            Some(claim)
        }
        Err(_) => None,
    };
    let winner_count = 1 + usize::from(finish_claim.is_some());
    assert!(matches!(winner_count, 1 | 2));

    let cancel_replay = fixture
        .store
        .claim_effect(
            &fixture.run_id,
            "attempt-1",
            "racing-cancel",
            cancel.clone(),
        )
        .unwrap();
    assert!(!cancel_replay.newly_created);
    assert!(
        fixture
            .store
            .claim_effect(&fixture.run_id, "attempt-1", "second-racing-cancel", cancel,)
            .is_err()
    );

    if let Some(finish_claim) = finish_claim {
        let finish_replay = fixture
            .store
            .claim_effect(
                &fixture.run_id,
                "attempt-1",
                &finish_claim.operation.idempotency_key,
                finish.clone(),
            )
            .unwrap();
        assert!(!finish_replay.newly_created);
    }
    assert!(
        fixture
            .store
            .claim_effect(&fixture.run_id, "attempt-1", "second-racing-finish", finish,)
            .is_err(),
        "finish cannot be newly claimed after the cancellation fence"
    );
}

fn race_effect_claims(
    fixture: &Fixture,
    requests: [(&'static str, DurableOperationRequest); 2],
) -> [Result<DurableOperationClaim, lookback_tauri_lib::coding_agent::CodingAgentError>; 2] {
    let paths = DataPaths::with_root(fixture._directory.path());
    let barrier = Arc::new(Barrier::new(3));
    let first_paths = paths.clone();
    let first_run = fixture.run_id.clone();
    let first_barrier = barrier.clone();
    let (first_key, first_request) = requests[0].clone();
    let first = std::thread::spawn(move || {
        let store = RunStore::open(&first_paths).unwrap();
        first_barrier.wait();
        store.claim_effect(&first_run, "attempt-1", first_key, first_request)
    });
    let second_paths = paths;
    let second_run = fixture.run_id.clone();
    let second_barrier = barrier.clone();
    let (second_key, second_request) = requests[1].clone();
    let second = std::thread::spawn(move || {
        let store = RunStore::open(&second_paths).unwrap();
        second_barrier.wait();
        store.claim_effect(&second_run, "attempt-1", second_key, second_request)
    });
    barrier.wait();
    [first.join().unwrap(), second.join().unwrap()]
}

struct Fixture {
    _directory: tempfile::TempDir,
    store: Arc<RunStore>,
    run_id: RunId,
}

struct RunFixtureOptions<'a> {
    issue_id: &'a str,
    issue_revision: u64,
    publication_plan: PublicationPlan,
    publication_receipt: Option<PublicationOutcome>,
    report_only: bool,
    publication_base_branch: &'a str,
}

fn authority(
    fixture: &Fixture,
    tracker: Arc<FakeTracker>,
    policy_identity: &str,
    policy_version: u64,
) -> CompletionAuthority {
    CompletionAuthority::new(
        fixture.store.clone(),
        tracker,
        CompletionPolicy::new(policy_identity, policy_version).unwrap(),
    )
    .unwrap()
}

fn verified_report(summary: &str) -> DurableCompletionReport {
    use lookback_tauri_lib::grpc::proto::issue_tracker::v1::{
        CompletionEvidence, CompletionEvidenceKind, CompletionReport, VerifiedCompletion,
        completion_report::Result as ReportResult,
    };
    CompletionReport {
        result: Some(ReportResult::Verified(VerifiedCompletion {
            evidence: vec![CompletionEvidence {
                kind: CompletionEvidenceKind::OtherEvidence as i32,
                summary: summary.into(),
                uri: None,
            }],
        })),
    }
}

fn prepared_run(
    auto_done: bool,
    purpose: RunPurpose,
    required_validation: bool,
    fail_validation: bool,
) -> Fixture {
    prepared_run_with_mode_options(
        auto_done,
        purpose,
        ExecutionMode::OneShot,
        required_validation,
        fail_validation,
    )
}

fn prepared_run_with_mode(auto_done: bool, mode: ExecutionMode) -> Fixture {
    prepared_run_with_mode_options(auto_done, RunPurpose::Implementation, mode, false, false)
}

fn prepared_run_with_mode_options(
    auto_done: bool,
    purpose: RunPurpose,
    mode: ExecutionMode,
    required_validation: bool,
    fail_validation: bool,
) -> Fixture {
    prepared_run_with_publication(
        auto_done,
        purpose,
        mode,
        required_validation,
        fail_validation,
        PublicationPlan::NoPublication,
        None,
    )
}

fn prepared_run_with_publication(
    auto_done: bool,
    purpose: RunPurpose,
    mode: ExecutionMode,
    required_validation: bool,
    fail_validation: bool,
    publication_plan: PublicationPlan,
    publication_receipt: Option<PublicationOutcome>,
) -> Fixture {
    prepared_run_with_publication_for_issue(
        RunFixtureOptions {
            issue_id: "issue-17",
            issue_revision: 5,
            publication_plan,
            publication_receipt,
            report_only: false,
            publication_base_branch: "main",
        },
        auto_done,
        purpose,
        mode,
        required_validation,
        fail_validation,
    )
}

fn prepared_run_with_publication_for_issue(
    options: RunFixtureOptions<'_>,
    auto_done: bool,
    purpose: RunPurpose,
    mode: ExecutionMode,
    required_validation: bool,
    fail_validation: bool,
) -> Fixture {
    prepared_run_with_contract(
        options,
        auto_done,
        purpose,
        mode,
        required_validation,
        fail_validation,
        false,
    )
}

fn prepared_run_with_contract(
    options: RunFixtureOptions<'_>,
    auto_done: bool,
    purpose: RunPurpose,
    mode: ExecutionMode,
    required_validation: bool,
    fail_validation: bool,
    versioned: bool,
) -> Fixture {
    let RunFixtureOptions {
        issue_id,
        issue_revision,
        publication_plan,
        publication_receipt,
        report_only,
        publication_base_branch,
    } = options;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(RunStore::open(&DataPaths::with_root(directory.path())).unwrap());
    let repository_id = RepositoryIdentity::new("forge.example", "lookback", "agent-app").unwrap();
    let digest = "a".repeat(64);
    let run_id = RunId::new("completion-run-1").unwrap();
    let snapshot = AgentRunSnapshot {
        run_id: run_id.clone(),
        issue_id: issue_id.into(),
        issue_revision,
        execution_mode: mode,
        purpose,
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
            image_ref: format!("registry.example/coder@sha256:{digest}"),
            image_digest: format!("sha256:{digest}"),
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
            validated_host_limits: lookback_tauri_lib::coding_agent::HostResourceLimits {
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
        workspace_branch: "agent/completion-run-1".into(),
        semantic_judge_enabled: true,
        auto_done,
        approval: Some(lookback_tauri_lib::coding_agent::RunApproval {
            approved_by: "reviewer".into(),
            approved_at_ms: 123,
            auto_done_approved: auto_done,
            settings_version: auto_done.then_some(7),
            applicable_modes: vec![mode],
        }),
        allowed_operations: if publication_plan == PublicationPlan::NoPublication {
            vec![AllowedOperation::Verify]
        } else {
            let mut allowed = vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
            if publication_plan == PublicationPlan::CreatePullRequest {
                allowed.push(AllowedOperation::CreatePullRequest);
            }
            allowed
        },
        publication_plan: publication_plan.clone(),
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
    if versioned {
        let contract =
            lookback_tauri_lib::commands::coding_agent::build_approval_assessment_contract(
                purpose,
                vec![],
                issue_id,
                None,
                None,
                vec![],
                Some(
                    lookback_tauri_lib::coding_agent::semantic_judge::provider_selection(
                        &lookback_tauri_lib::commands::llm_settings::LlmSettings::default(),
                    )
                    .unwrap(),
                ),
            )
            .unwrap();
        store
            .create_run_with_approval_contract(snapshot, contract)
            .unwrap();
    } else {
        store.create_run(snapshot).unwrap();
    }
    let queued = store.transition(&run_id, 0, RunState::Queued).unwrap();
    let preparing = store
        .transition(&run_id, queued.revision, RunState::Preparing)
        .unwrap();
    let start = store
        .start_attempt(&run_id, preparing.revision, "attempt-1", "start-attempt-1")
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
            b"verified report",
        )
        .unwrap();
    let mut assessment = RunEvidenceAssessment {
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
        commit_id: (!report_only).then(|| "e".repeat(40)),
        commit_consistent: true,
        acceptance_conditions_met: true,
        validation: ValidationEvidence {
            required: required_validation,
            state: if fail_validation {
                ValidationState::NotRun
            } else {
                ValidationState::Passed
            },
        },
    };
    if !auto_done || purpose == RunPurpose::Investigation {
        assessment.acceptance_conditions_met = true;
    }
    if mode == ExecutionMode::Interactive {
        store
            .record_execution_evidence(&run_id, "attempt-1", assessment)
            .unwrap();
    } else {
        let finalizing = store
            .transition(&run_id, running.revision, RunState::Finalizing)
            .unwrap();
        store
            .record_execution_evidence(&run_id, "attempt-1", assessment)
            .unwrap();
        let verifying = store
            .transition(&run_id, finalizing.revision, RunState::Verifying)
            .unwrap();
        let before_review = if publication_plan == PublicationPlan::NoPublication {
            verifying
        } else {
            let publishing = store
                .transition(&run_id, verifying.revision, RunState::Publishing)
                .unwrap();
            let Some(receipt) = publication_receipt else {
                return Fixture {
                    _directory: directory,
                    store,
                    run_id,
                };
            };
            let receipt_bytes = serde_json::to_vec(&receipt).unwrap();
            let receipt_reference = store
                .write_evidence(
                    &run_id,
                    "attempt-1",
                    "publication-receipt",
                    "application/json",
                    &receipt_bytes,
                )
                .unwrap();
            let operation_key = format!(
                "publish-{}",
                &format!("{:x}", Sha256::digest(b"attempt-1"))[..32]
            );
            let publication_request = if publication_plan == PublicationPlan::CreatePullRequest {
                DurableOperationRequest::CreatePullRequest {
                    branch: "agent/completion-run-1".into(),
                    base_branch: publication_base_branch.into(),
                    commit_id: "e".repeat(40),
                }
            } else {
                DurableOperationRequest::PublishBranch {
                    branch: "agent/completion-run-1".into(),
                    commit_id: "e".repeat(40),
                }
            };
            let publication_claim = store
                .claim_publication_effect(
                    &run_id,
                    publishing.revision,
                    "attempt-1",
                    &operation_key,
                    publication_request,
                )
                .unwrap();
            store
                .complete_publication_effect(
                    publication_claim
                        .permit
                        .expect("fresh fixture publication permit"),
                    DurableOperationResult {
                        outcome: DurableOperationOutcome::Succeeded,
                        result_digest: Some(format!("{:x}", Sha256::digest(&receipt_bytes))),
                        evidence_ids: vec![receipt_reference.evidence_id],
                    },
                )
                .unwrap();
            store
                .transition(&run_id, publishing.revision, RunState::AwaitingReview)
                .unwrap()
        };
        let awaiting_review = if before_review.state == RunState::AwaitingReview {
            before_review
        } else {
            store
                .transition(&run_id, before_review.revision, RunState::AwaitingReview)
                .unwrap()
        };
        assert_eq!(awaiting_review.state, RunState::AwaitingReview);
    }

    Fixture {
        _directory: directory,
        store,
        run_id,
    }
}

fn publication_inflight_run() -> Fixture {
    prepared_run_with_publication(
        true,
        RunPurpose::Implementation,
        ExecutionMode::OneShot,
        false,
        false,
        PublicationPlan::PushBranch,
        None,
    )
}

fn tracker_for(fixture: &Fixture) -> Arc<FakeTracker> {
    let run = fixture.store.get_run(&fixture.run_id).unwrap().unwrap();
    let attempt_id = run.current_attempt_id.as_deref().unwrap();
    let evidence_references = run
        .evidence
        .iter()
        .filter(|evidence| evidence.attempt_id == attempt_id)
        .map(|evidence| {
            format!(
                "agent-app://coding-agent/{}/{}/{}",
                fixture.run_id, evidence.evidence_id, evidence.sha256
            )
        })
        .collect();
    Arc::new(FakeTracker {
        issue: Mutex::new(TrackerIssueSnapshot {
            issue_id: run.snapshot.issue_id,
            state: IssueState::AwaitingReview,
            revision: 10,
            history: vec![IssueHistoryEntry {
                issue_id: "issue-17".into(),
                resulting_revision: 9,
                old_state: Some(IssueState::InProgress),
                new_state: IssueState::AwaitingReview,
                awaiting_review: Some(lookback_tauri_lib::coding_agent::AwaitingReviewMarker {
                    run_id: fixture.run_id.to_string(),
                    full_commit_id: run
                        .execution_evidence
                        .as_ref()
                        .unwrap()
                        .commit_id
                        .clone()
                        .unwrap_or_else(|| run.snapshot.base_commit.clone()),
                    evidence_references,
                }),
                completion: None,
                actor_id: "lookback-coding-agent".into(),
                actor_kind: "system".into(),
                correlation_id: "review-marker-correlation".into(),
                reason: "execution completed; awaiting review".into(),
            }],
        }),
        behavior: Mutex::new(MutationBehavior::Accept),
        mutations: Mutex::new(Vec::new()),
        before_read: Mutex::new(None),
        before_transition: Mutex::new(None),
        after_transition: Mutex::new(None),
        history_operations: Mutex::new(HashMap::new()),
        lookup: Mutex::new(PrimaryOperationLookup::Unknown),
        primary_operation: Mutex::new(None),
        admitted_request: Mutex::new(None),
        manager_idempotency: Mutex::new(true),
    })
}

fn fake_primary_identity() -> PrimaryTrackerIdentity {
    PrimaryTrackerIdentity {
        schema_fingerprint: "schema-fingerprint-v1".into(),
        data_root_id: "test-data-root".into(),
        contract_fingerprint: "contract-fingerprint-v1".into(),
    }
}

fn fake_operation_receipt(request: &TrackerMutationRequest) -> PrimaryOperationReceipt {
    let identity = fake_primary_identity();
    let mut idempotency_scope = String::new();
    for part in [
        identity.contract_fingerprint.as_str(),
        identity.data_root_id.as_str(),
        "manager_transition_issue",
        &format!("issue:{}", request.issue_id),
        request.idempotency_key.as_str(),
    ] {
        idempotency_scope.push_str(&format!("{}:{part}", part.len()));
    }
    PrimaryOperationReceipt {
        manager_operation_id: "server-issued-manager-operation-1".into(),
        downstream_operation_id: "server-issued-downstream-operation-1".into(),
        manager_idempotency_key: request.idempotency_key.clone(),
        idempotency_scope,
        request_digest: request.expected_manager_request_digest.clone(),
        request_schema: "issue_tracker.manager.v1.DurableBackendRequest".into(),
        backend_contract_fingerprint: "test-backend-contract-fingerprint-v1".into(),
        manager_sequence: request.expected_revision + 1,
        generation: 1,
        server_identity: identity,
    }
}

#[derive(Default)]
struct RpcResponseLossPlan {
    armed: AtomicBool,
    transition_requests: AtomicUsize,
    transition_responses_dropped: AtomicUsize,
    operation_history_requests: AtomicUsize,
    operation_history_responses_dropped: AtomicUsize,
}

impl RpcResponseLossPlan {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn response_to_drop(&self, path: &str) -> Option<RpcResponseKind> {
        if !self.armed.load(Ordering::SeqCst) {
            return None;
        }
        match path {
            "/issue_tracker.rpc.v1.ManagerConsumerService/TransitionIssue" => {
                (self.transition_requests.fetch_add(1, Ordering::SeqCst) == 0)
                    .then_some(RpcResponseKind::Transition)
            }
            "/issue_tracker.rpc.v1.ManagerConsumerService/ListOperationHistory" => (self
                .operation_history_requests
                .fetch_add(1, Ordering::SeqCst)
                < 2)
            .then_some(RpcResponseKind::OperationHistory),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum RpcResponseKind {
    Transition,
    OperationHistory,
}

#[derive(Clone)]
struct GrpcForwarder {
    upstream: tonic::transport::Channel,
    loss_plan: Arc<RpcResponseLossPlan>,
}

impl GrpcForwarder {
    fn call(
        &self,
        request: tonic::codegen::http::Request<tonic::body::Body>,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        tonic::codegen::http::Response<tonic::body::Body>,
                        std::convert::Infallible,
                    >,
                > + Send,
        >,
    > {
        use tonic::codegen::Service as _;

        let response_to_drop = self.loss_plan.response_to_drop(request.uri().path());
        let loss_plan = self.loss_plan.clone();
        let mut upstream = self.upstream.clone();
        Box::pin(async move {
            if std::future::poll_fn(|context| upstream.poll_ready(context))
                .await
                .is_err()
            {
                return Ok(proxy_upstream_error());
            }
            let response = match upstream.call(request).await {
                Ok(response) => response,
                Err(_) => return Ok(proxy_upstream_error()),
            };
            if let Some(response_kind) = response_to_drop {
                let (parts, _body) = response.into_parts();
                // The upstream RPC handler has returned; withholding its
                // gRPC body and trailers gives the real caller an
                // unconfirmed transport result after the Manager call.
                match response_kind {
                    RpcResponseKind::Transition => {
                        loss_plan
                            .transition_responses_dropped
                            .fetch_add(1, Ordering::SeqCst);
                    }
                    RpcResponseKind::OperationHistory => {
                        loss_plan
                            .operation_history_responses_dropped
                            .fetch_add(1, Ordering::SeqCst);
                    }
                }
                return Ok(tonic::codegen::http::Response::from_parts(
                    parts,
                    tonic::body::Body::empty(),
                ));
            }
            Ok(response)
        })
    }
}

fn proxy_upstream_error() -> tonic::codegen::http::Response<tonic::body::Body> {
    tonic::codegen::http::Response::builder()
        .status(tonic::codegen::http::StatusCode::BAD_GATEWAY)
        .body(tonic::body::Body::empty())
        .expect("construct test proxy upstream error")
}

type GrpcForwardFuture = Pin<
    Box<
        dyn std::future::Future<
                Output = Result<
                    tonic::codegen::http::Response<tonic::body::Body>,
                    std::convert::Infallible,
                >,
            > + Send,
    >,
>;

macro_rules! make_grpc_forward_service {
    ($service:ident, $name:literal) => {
        #[derive(Clone)]
        struct $service(GrpcForwarder);

        impl tonic::server::NamedService for $service {
            const NAME: &'static str = $name;
        }

        impl tonic::codegen::Service<tonic::codegen::http::Request<tonic::body::Body>>
            for $service
        {
            type Response = tonic::codegen::http::Response<tonic::body::Body>;
            type Error = std::convert::Infallible;
            type Future = GrpcForwardFuture;

            fn poll_ready(
                &mut self,
                _context: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Result<(), Self::Error>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn call(
                &mut self,
                request: tonic::codegen::http::Request<tonic::body::Body>,
            ) -> Self::Future {
                self.0.call(request)
            }
        }
    };
}

make_grpc_forward_service!(
    ManagerInfoForwarder,
    "issue_tracker.rpc.v1.ManagerInfoService"
);
make_grpc_forward_service!(
    ManagerConsumerForwarder,
    "issue_tracker.rpc.v1.ManagerConsumerService"
);
make_grpc_forward_service!(
    ManagerTrustedReadForwarder,
    "issue_tracker.rpc.v1.ManagerTrustedReadService"
);
make_grpc_forward_service!(OperationForwarder, "issue_tracker.rpc.v1.OperationService");

struct RpcResponseLossProxy {
    endpoint: String,
    loss_plan: Arc<RpcResponseLossPlan>,
    task: tokio::task::JoinHandle<()>,
}

impl RpcResponseLossProxy {
    async fn start(upstream: tonic::transport::Channel) -> Self {
        let port = common::reserve_loopback_port();
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let endpoint = format!("http://127.0.0.1:{port}");
        let loss_plan = Arc::new(RpcResponseLossPlan::default());
        let forwarder = GrpcForwarder {
            upstream,
            loss_plan: loss_plan.clone(),
        };
        let router = tonic::transport::Server::builder()
            .add_service(ManagerInfoForwarder(forwarder.clone()))
            .add_service(ManagerConsumerForwarder(forwarder.clone()))
            .add_service(ManagerTrustedReadForwarder(forwarder.clone()))
            .add_service(OperationForwarder(forwarder));
        let task = tokio::spawn(async move {
            router
                .serve(address)
                .await
                .expect("serve isolated gRPC response-loss proxy");
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if connect(&endpoint).await.is_ok() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "isolated gRPC response-loss proxy did not start"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        Self {
            endpoint,
            loss_plan,
            task,
        }
    }

    fn arm(&self) {
        self.loss_plan.arm();
    }
}

impl Drop for RpcResponseLossProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct RecordingPrimaryTracker {
    inner: Arc<PrimaryIssueTrackerAdapter>,
    transition_requests: Arc<Mutex<Vec<TrackerMutationRequest>>>,
}

#[async_trait]
impl CompletionTracker for RecordingPrimaryTracker {
    async fn read_issue_with_history(&self, issue_id: &str) -> TrackerResult<TrackerIssueSnapshot> {
        self.inner.read_issue_with_history(issue_id).await
    }

    async fn read_issue_with_operation_history(
        &self,
        issue_id: &str,
    ) -> TrackerResult<TrackerIssueOperationSnapshot> {
        self.inner.read_issue_with_operation_history(issue_id).await
    }

    async fn transition_issue(&self, request: TrackerMutationRequest) -> TrackerResult<()> {
        self.transition_issue_with_receipt(request)
            .await
            .map(|_| ())
    }

    fn primary_identity(&self) -> TrackerResult<PrimaryTrackerIdentity> {
        self.inner.primary_identity()
    }

    async fn transition_issue_with_receipt(
        &self,
        request: TrackerMutationRequest,
    ) -> TrackerResult<PrimaryOperationReceipt> {
        self.transition_requests
            .lock()
            .unwrap()
            .push(request.clone());
        self.inner.transition_issue_with_receipt(request).await
    }

    async fn lookup_transition_operation(
        &self,
        authorization: &lookback_tauri_lib::coding_agent::RunCompletionAuthorization,
    ) -> PrimaryOperationLookup {
        self.inner.lookup_transition_operation(authorization).await
    }

    fn recovery_dispatch_permission(
        &self,
        authorization: &lookback_tauri_lib::coding_agent::RunCompletionAuthorization,
    ) -> Option<RecoveryDispatchPermission> {
        self.inner.recovery_dispatch_permission(authorization)
    }
}

#[tokio::test]
#[ignore = "requires LOOKBACK_ISSUE_TRACKER_SERVER_BIN; starts an isolated local Issue Tracker server"]
async fn actual_primary_replays_the_same_cas_after_the_real_manager_response_is_lost() {
    use issue::completion_report::Result as CompletionResult;

    let root = tempfile::tempdir().unwrap();
    let data = DataPaths::with_root(root.path());
    let port = common::reserve_loopback_port();
    let config = render_server_config(&data, port).expect("render isolated server config");
    std::fs::write(config.config_path(), config.toml()).expect("write isolated config");
    let binary = std::env::var_os("LOOKBACK_ISSUE_TRACKER_SERVER_BIN")
        .expect("ignored contract test requires LOOKBACK_ISSUE_TRACKER_SERVER_BIN");
    let mut child = tokio::process::Command::new(binary)
        .arg("--config")
        .arg(config.config_path())
        .current_dir(root.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn isolated Issue Tracker server");

    let service = IssueTrackerGrpcService::new(config.listen_url());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if connect(&config.listen_url()).await.is_ok() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "isolated Issue Tracker server did not start"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    service
        .run_outbox_cycle("coding-agent-wire-loss-bootstrap")
        .await
        .expect("bootstrap local server");
    service
        .health("coding-agent-wire-loss-health")
        .await
        .expect("local server is ready");

    let issue_id = "01234567-89ab-8def-8123-456789abcdef";
    let create = service
        .create_issue(ManagerWriteRequest {
            correlation_id: "coding-agent-wire-loss-create".into(),
            idempotency_key: "coding-agent-wire-loss-create".into(),
            actor_audit: Some(manager::ActorAudit {
                actor_id: "contract-test-reviewer".into(),
                actor_kind: issue::ActorKind::Human as i32,
                reason: Some("seed local wire-loss completion Issue".into()),
                ..Default::default()
            }),
            payload: manager::CreateIssueArgs {
                issue_id: issue_id.into(),
                issue: Some(issue::IssueDraft {
                    title: "Local wire-loss completion Issue".into(),
                    kind: issue::IssueKind::Task as i32,
                    priority: issue::IssuePriority::P2 as i32,
                    creation_basis: issue::CreationBasis::Explicit as i32,
                    confirmation_status: issue::ConfirmationStatus::Confirmed as i32,
                    initial_state: issue::IssueState::Open as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
        })
        .await
        .expect("create canonical Issue on isolated server");
    service
        .run_outbox_cycle("coding-agent-wire-loss-create-outbox")
        .await
        .expect("materialize canonical Issue");
    let revision_update = service
        .update_issue(ManagerWriteRequest {
            correlation_id: "coding-agent-wire-loss-revision".into(),
            idempotency_key: "coding-agent-wire-loss-revision".into(),
            actor_audit: Some(manager::ActorAudit {
                actor_id: "contract-test-reviewer".into(),
                actor_kind: issue::ActorKind::Human as i32,
                reason: Some("give the canonical fixture a positive revision".into()),
                ..Default::default()
            }),
            payload: manager::UpdateIssueArgs {
                issue_id: issue_id.into(),
                expected_revision: Some(0),
                update_mask: vec![manager::ManagerIssueField::Title as i32],
                patch: Some(manager::ManagerIssuePatch {
                    title: Some("Local wire-loss completion Issue revision 1".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        })
        .await
        .expect("advance canonical Issue revision");
    service
        .run_outbox_cycle("coding-agent-wire-loss-revision-outbox")
        .await
        .expect("materialize canonical revision");
    let initial_issue = service
        .get_issue_with_sync(
            "coding-agent-wire-loss-created-issue",
            manager::GetIssueWithSyncArgs {
                issue_id: issue_id.into(),
                ..Default::default()
            },
        )
        .await
        .expect("read canonical Issue")
        .issue
        .expect("created Issue");
    assert!(initial_issue.revision > 0);
    assert_eq!(
        create.operation.unwrap().state,
        manager::DurableOperationState::Committed as i32
    );
    assert_eq!(
        revision_update.operation.unwrap().state,
        manager::DurableOperationState::Committed as i32
    );

    let fixture = prepared_run_with_publication_for_issue(
        RunFixtureOptions {
            issue_id,
            issue_revision: initial_issue.revision,
            publication_plan: PublicationPlan::NoPublication,
            publication_receipt: None,
            report_only: false,
            publication_base_branch: "main",
        },
        true,
        RunPurpose::Implementation,
        ExecutionMode::OneShot,
        false,
        false,
    );
    let run = fixture
        .store
        .get_run(&fixture.run_id)
        .unwrap()
        .expect("prepared run");
    let evidence_references = run
        .evidence
        .iter()
        .filter(|entry| Some(entry.attempt_id.as_str()) == run.current_attempt_id.as_deref())
        .map(|entry| {
            format!(
                "agent-app://coding-agent/{}/{}/{}",
                fixture.run_id, entry.evidence_id, entry.sha256
            )
        })
        .collect::<Vec<_>>();
    assert!(!evidence_references.is_empty());

    service
        .register_and_verify_gateway_policy(
            &config.gateway_control_url(),
            "coding-agent-wire-loss-gateway-policy",
            "coding-agent-wire-loss-gateway-policy",
            manager::WorkEligibilityMode::ReviewedOnly,
        )
        .await
        .expect("register isolated Gateway policy");
    let channel = connect(&config.gateway_worker_url())
        .await
        .expect("connect isolated Gateway");
    let mut gateway = rpc::issue_gateway_client::IssueGatewayClient::new(channel);
    let claim = gateway
        .claim(rpc::IssueGatewayClaimRequest {
            issue_id: issue_id.into(),
            expected_revision: Some(initial_issue.revision),
            idempotency_key: "coding-agent-wire-loss-review-claim".into(),
        })
        .await
        .expect("claim local Issue")
        .into_inner();
    let claim = match claim.outcome {
        Some(rpc::issue_gateway_claim_response::Outcome::Success(value)) => value,
        other => panic!("expected local Gateway claim, got {other:?}"),
    };
    let review_marker = issue::AwaitingReviewCompletion {
        run_id: fixture.run_id.to_string(),
        full_commit_id: "e".repeat(40),
        evidence_references,
        report: Some(issue::CompletionReport {
            result: Some(CompletionResult::Verified(issue::VerifiedCompletion {
                evidence: vec![issue::CompletionEvidence {
                    kind: issue::CompletionEvidenceKind::OtherEvidence as i32,
                    summary: "local wire-loss fixture records verified run evidence".into(),
                    uri: None,
                }],
            })),
        }),
    };
    let marker = service
        .complete_execution_to_awaiting_review(
            &config.gateway_worker_url(),
            issue_id,
            Some(&claim.capability_id),
            claim.revision,
            "coding-agent-wire-loss-awaiting-review",
            review_marker,
        )
        .await
        .expect("write exact local AWAITING_REVIEW marker");
    assert!(marker.revision > initial_issue.revision);

    let proxy = RpcResponseLossProxy::start(connect(&config.listen_url()).await.unwrap()).await;
    let proxy_service = IssueTrackerGrpcService::new(proxy.endpoint.clone());
    proxy_service
        .health("coding-agent-wire-loss-proxy-health")
        .await
        .expect("the test proxy forwards trusted Manager health");
    let recording_tracker = RecordingPrimaryTracker {
        inner: Arc::new(PrimaryIssueTrackerAdapter::new(proxy_service)),
        transition_requests: Arc::new(Mutex::new(Vec::new())),
    };
    let authority = CompletionAuthority::new(
        fixture.store.clone(),
        Arc::new(recording_tracker.clone()),
        CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
    )
    .unwrap();
    let completion_intent = "actual-manager-wire-loss-retry";
    let grant = authority
        .authorize_auto_completion(&fixture.run_id, completion_intent)
        .await
        .expect("trusted primary Issue history authorizes the approved run");

    proxy.arm();
    assert!(
        authority.complete_auto(grant).await.is_err(),
        "the deliberately truncated real Manager response must remain unconfirmed"
    );
    assert_eq!(
        fixture
            .store
            .get_run(&fixture.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::AwaitingReview,
        "missing proof must not mark the local run DONE"
    );

    service
        .run_outbox_cycle("coding-agent-wire-loss-first-outbox")
        .await
        .expect("drive the real Manager operation after its response was lost");
    let first_issue_after_loss = service
        .get_issue_with_sync(
            "coding-agent-wire-loss-first-read",
            manager::GetIssueWithSyncArgs {
                issue_id: issue_id.into(),
                ..Default::default()
            },
        )
        .await
        .expect("read actual Issue after lost response and outbox cycle")
        .issue
        .expect("canonical Issue remains present");
    assert_eq!(first_issue_after_loss.state, issue::IssueState::Done as i32);
    assert_eq!(first_issue_after_loss.revision, marker.revision + 1);
    assert_eq!(
        fixture
            .store
            .get_run(&fixture.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::AwaitingReview,
        "the server-side effect alone is not enough to complete the local run"
    );

    let manager_history_args = manager::ListOperationHistoryArgs {
        page: Some(manager::PageRequest {
            page_size: 100,
            ..Default::default()
        }),
        filter: Some(manager::OperationHistoryFilter {
            direct_target: Some(manager::operation_history_filter::DirectTarget::IssueId(
                issue_id.into(),
            )),
            method: Some("manager_transition_issue".into()),
            direct_scope_only: true,
            ..Default::default()
        }),
        include_total_size: Some(true),
        ..Default::default()
    };
    let manager_history_before_retry = service
        .list_operation_history(
            "coding-agent-wire-loss-manager-history-before-retry",
            manager_history_args.clone(),
        )
        .await
        .expect("read actual Manager transition operation before retry");
    let completion_rows_before_retry = manager_history_before_retry
        .entries
        .iter()
        .filter(|entry| entry.correlation_id.starts_with("coding-agent-completion-"))
        .collect::<Vec<_>>();
    assert_eq!(completion_rows_before_retry.len(), 1);
    let original_manager_operation_id = completion_rows_before_retry[0].operation_id.clone();
    let original_manager_operation = service
        .get_operation(
            &completion_rows_before_retry[0].correlation_id,
            original_manager_operation_id.clone(),
        )
        .await
        .expect("read the actual durable completion operation");
    assert_eq!(
        original_manager_operation.state,
        manager::DurableOperationState::Committed as i32
    );
    assert!(!original_manager_operation.request_digest.is_empty());
    let original_audit = original_manager_operation
        .audit
        .as_ref()
        .expect("the actual Manager operation retains its safe audit");
    assert_eq!(original_audit.method, "manager_transition_issue");
    assert_eq!(original_audit.target, format!("issue:{issue_id}"));
    assert!(!original_audit.idempotency_key.is_empty());

    let retry = authority
        .authorize_auto_completion(&fixture.run_id, completion_intent)
        .await
        .expect("the dispatched authorization remains recoverable");
    let recovered = authority
        .complete_auto(retry)
        .await
        .expect("same-key real Manager replay confirms the original completion");
    assert!(matches!(
        recovered,
        CompletionOutcome::Completed {
            issue_revision,
            ..
        } if issue_revision == marker.revision + 1
    ));
    assert_eq!(
        fixture
            .store
            .get_run(&fixture.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Done
    );

    {
        let requests = recording_tracker.transition_requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "the real manager received an initial call and one retry"
        );
        assert_eq!(
            requests[0], requests[1],
            "the retry must preserve the entire immutable request, not just its key"
        );
        assert_eq!(requests[0].issue_id, issue_id);
        assert_eq!(requests[0].expected_revision, marker.revision);
        assert_eq!(requests[0].target_state, IssueState::Done);
        assert_eq!(requests[0].idempotency_key, original_audit.idempotency_key);
        assert!(!requests[0].expected_manager_request_digest.is_empty());
    }

    assert_eq!(
        proxy.loss_plan.transition_requests.load(Ordering::SeqCst),
        2
    );
    assert_eq!(
        proxy
            .loss_plan
            .transition_responses_dropped
            .load(Ordering::SeqCst),
        1,
        "the first actual Manager response was dropped after forwarding the RPC"
    );
    assert_eq!(
        proxy
            .loss_plan
            .operation_history_requests
            .load(Ordering::SeqCst),
        2
    );
    assert_eq!(
        proxy
            .loss_plan
            .operation_history_responses_dropped
            .load(Ordering::SeqCst),
        2,
        "both ambiguous lookups were hidden so recovery must issue the same-key RPC"
    );

    let issue_after_retry = service
        .get_issue_with_sync(
            "coding-agent-wire-loss-final-issue",
            manager::GetIssueWithSyncArgs {
                issue_id: issue_id.into(),
                ..Default::default()
            },
        )
        .await
        .expect("read canonical Issue after exact replay")
        .issue
        .expect("canonical Issue remains present");
    assert_eq!(issue_after_retry.state, issue::IssueState::Done as i32);
    assert_eq!(issue_after_retry.revision, marker.revision + 1);

    let history_after_retry = service
        .list_trusted_issue_history("coding-agent-wire-loss-final-history", issue_id)
        .await
        .expect("read exact trusted Issue history after replay");
    let completed_history = history_after_retry
        .iter()
        .find(|entry| entry.resulting_revision == marker.revision + 1)
        .expect("one terminal history event exists");
    let completed_state = completed_history
        .state_change
        .as_ref()
        .expect("terminal history records its state change");
    assert_eq!(completed_state.new_state, issue::IssueState::Done as i32);
    let actor = completed_history
        .actor
        .as_ref()
        .and_then(|context| context.actor.as_ref())
        .expect("terminal history retains the completion actor");
    assert_eq!(actor.actor_id, "lookback-coding-agent");
    assert_eq!(actor.kind, issue::ActorKind::System as i32);
    assert!(completed_state.completion.is_some());
    assert_eq!(
        history_after_retry
            .iter()
            .filter(|entry| {
                entry
                    .state_change
                    .as_ref()
                    .is_some_and(|state| state.new_state == issue::IssueState::Done as i32)
            })
            .count(),
        1,
        "same-key replay produces one observable Issue effect"
    );

    let manager_history_after_retry = service
        .list_operation_history(
            "coding-agent-wire-loss-manager-history-after-retry",
            manager_history_args,
        )
        .await
        .expect("read actual Manager history after same-key replay");
    let completion_rows_after_retry = manager_history_after_retry
        .entries
        .iter()
        .filter(|entry| entry.correlation_id.starts_with("coding-agent-completion-"))
        .collect::<Vec<_>>();
    assert_eq!(completion_rows_after_retry.len(), 1);
    assert_eq!(
        completion_rows_after_retry[0].operation_id, original_manager_operation_id,
        "the replay returns the original durable operation, with no extra epoch"
    );
    assert_eq!(
        manager_history_after_retry.entries.len(),
        manager_history_before_retry.entries.len()
    );
    let final_manager_operation = service
        .get_operation(
            &completion_rows_after_retry[0].correlation_id,
            completion_rows_after_retry[0].operation_id.clone(),
        )
        .await
        .expect("read the same committed Manager operation after replay");
    assert_eq!(
        final_manager_operation.request_digest,
        original_manager_operation.request_digest
    );
    assert_eq!(
        final_manager_operation
            .audit
            .as_ref()
            .unwrap()
            .idempotency_key,
        original_audit.idempotency_key
    );

    drop(authority);
    drop(fixture);
    drop(proxy);
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[tokio::test]
async fn periodic_auto_completion_uses_current_approved_versioned_evidence_for_both_purposes() {
    for purpose in [RunPurpose::Implementation, RunPurpose::Investigation] {
        for (auto_done, validation_failed, changed_issue) in [
            (true, false, false),
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            let fixture = prepared_run_with_contract(
                RunFixtureOptions {
                    issue_id: "issue-17",
                    issue_revision: 5,
                    publication_plan: PublicationPlan::NoPublication,
                    publication_receipt: None,
                    report_only: false,
                    publication_base_branch: "main",
                },
                auto_done,
                purpose,
                ExecutionMode::OneShot,
                validation_failed,
                validation_failed,
                true,
            );
            let tracker = tracker_for(&fixture);
            if changed_issue {
                tracker.issue.lock().unwrap().state = IssueState::Open;
            }
            let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
            let outcome = authority
                .reconcile_approved_auto_completion(&fixture.run_id)
                .await;
            if auto_done && !validation_failed && !changed_issue {
                assert!(outcome.unwrap());
                assert_eq!(
                    fixture
                        .store
                        .get_run(&fixture.run_id)
                        .unwrap()
                        .unwrap()
                        .state,
                    RunState::Done
                );
                assert!(
                    !authority
                        .reconcile_approved_auto_completion(&fixture.run_id)
                        .await
                        .unwrap()
                );
                assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
            } else {
                assert!(!matches!(outcome, Ok(true)));
                assert_eq!(tracker.mutations.lock().unwrap().len(), 0);
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
        }
    }
}

#[tokio::test]
async fn periodic_auto_completion_does_not_upgrade_legacy_execution_evidence() {
    let fixture = prepared_run(true, RunPurpose::Implementation, false, false);
    let tracker = tracker_for(&fixture);
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    assert!(
        !authority
            .reconcile_approved_auto_completion(&fixture.run_id)
            .await
            .unwrap()
    );
    assert_eq!(tracker.mutations.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn periodic_auto_completion_keeps_processing_after_a_rejected_candidate() {
    let fixture = prepared_run_with_contract(
        RunFixtureOptions {
            issue_id: "issue-17",
            issue_revision: 5,
            publication_plan: PublicationPlan::NoPublication,
            publication_receipt: None,
            report_only: false,
            publication_base_branch: "main",
        },
        true,
        RunPurpose::Implementation,
        ExecutionMode::OneShot,
        false,
        false,
        true,
    );
    let tracker = tracker_for(&fixture);
    let authority = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    assert_eq!(
        authority
            .reconcile_approved_auto_completions(&[
                RunId::new("missing-candidate").unwrap(),
                fixture.run_id.clone(),
                fixture.run_id.clone(),
            ])
            .await,
        1
    );
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn periodic_auto_completion_reconciles_response_loss_after_store_reopen() {
    let fixture = prepared_run_with_contract(
        RunFixtureOptions {
            issue_id: "issue-17",
            issue_revision: 5,
            publication_plan: PublicationPlan::NoPublication,
            publication_receipt: None,
            report_only: false,
            publication_base_branch: "main",
        },
        true,
        RunPurpose::Investigation,
        ExecutionMode::OneShot,
        false,
        false,
        true,
    );
    let tracker = tracker_for(&fixture);
    *tracker.behavior.lock().unwrap() = MutationBehavior::CommitThenLoseResponse;
    *tracker.after_transition.lock().unwrap() = Some({
        let tracker = tracker.clone();
        Arc::new(move || *tracker.lookup.lock().unwrap() = PrimaryOperationLookup::Unknown)
    });
    let first = authority(&fixture, tracker.clone(), "agent-app/coding-agent", 1);
    assert!(
        first
            .reconcile_approved_auto_completion(&fixture.run_id)
            .await
            .is_err()
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
    let receipt = tracker.primary_operation.lock().unwrap().clone().unwrap();
    *tracker.lookup.lock().unwrap() = PrimaryOperationLookup::Committed(receipt);
    let reopened =
        Arc::new(RunStore::open(&DataPaths::with_root(fixture._directory.path())).unwrap());
    let recovered = CompletionAuthority::new(
        reopened.clone(),
        tracker.clone(),
        CompletionPolicy::new("agent-app/coding-agent", 1).unwrap(),
    )
    .unwrap();
    assert!(
        recovered
            .reconcile_approved_auto_completion(&fixture.run_id)
            .await
            .unwrap()
    );
    assert_eq!(
        reopened.get_run(&fixture.run_id).unwrap().unwrap().state,
        RunState::Done
    );
    assert_eq!(tracker.mutations.lock().unwrap().len(), 1);
}
