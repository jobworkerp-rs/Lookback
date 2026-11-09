use std::{
    collections::{BTreeMap, BTreeSet},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::{Stream, StreamExt, stream};
use lookback_tauri_lib::coding_agent::model::{ExecutionInvocationKind, ShellProcessObservation};
use lookback_tauri_lib::coding_agent::{
    assessment::{AllowedEvidenceSource, EvidenceSourceKind, ProviderSelection},
    model::{
        ApprovalAssessmentContract, ApprovalCriterionPolicy, ApprovalValidationDraft,
        ApprovedJudgeInputField, AssessmentInputScope, CriterionEvaluationMethod,
    },
};
use lookback_tauri_lib::{
    coding_agent::{
        AgentProfileSnapshot, AgentRunRecord, AgentRunSnapshot, AllowedOperation,
        CancellationObservation, ChildWritersStatus, CodingAgentCoordinator, CoordinatorConfig,
        DurableOperationOutcome, DurableOperationRequest, DurableOperationStatus, ExecutionAdapter,
        ExecutionEvent, ExecutionIdentity, ExecutionMode, ExecutionResult, ForgeKind,
        HostFinalizationObservation, HostResourceLimits, ImmutableExecutionIntent, InputDelivery,
        InteractiveFinishObservation, OperationReconciliation, PreparationOutcome,
        ProcessExitObservation, PublicationAdapter, PublicationOutcome, PublicationPlan,
        PublicationRequest, PublicationSupport, RepositoryIdentity, RepositoryProvenance,
        RepositorySelection, ResolvedRepository, RunId, RunPurpose, RunState, RuntimeObservation,
        SandboxResources, StartOutcome, StartReceipt, TerminalControl, TerminalEvent,
        ValidationEvidence, ValidationState, WorkflowDefinitionSnapshot, WorkspaceFreezeStatus,
        WorkspaceReceipt, WorkspaceRequest,
    },
    data::DataPaths,
};
use sha2::{Digest, Sha256};
use tokio::sync::{Barrier, Notify};

type EventStream = Pin<Box<dyn Stream<Item = Result<ExecutionEvent, ExecutionResult>> + Send>>;
type RuntimeInvalidation = (&'static str, fn(&mut RuntimeObservation));

struct FakeExecution {
    validation_calls: AtomicUsize,
    validation_exit: Mutex<Result<i32, ExecutionResult>>,
    validation_gate: Mutex<Option<Arc<Notify>>>,
    extra_validation_ids: Mutex<Vec<String>>,
    calls: AtomicUsize,
    semantic_calls: AtomicUsize,
    semantic_outcome:
        Mutex<Result<lookback_tauri_lib::coding_agent::assessment::AssessmentUse, ExecutionResult>>,
    semantic_gate: Mutex<Option<Arc<Notify>>>,
    commits: AtomicUsize,
    commit_gate: Mutex<Option<Arc<Notify>>>,
    commit_enabled: AtomicBool,
    source_after_skip: Mutex<Option<coding_agent_executor::SourceInspection>>,
    commit_observation:
        Mutex<Option<lookback_tauri_lib::coding_agent::workspace_commit::LocalCommitObservation>>,
    commit_failure: Mutex<Option<ExecutionResult>>,
    commit_metadata: Mutex<Option<Vec<u8>>>,
    issue_result_saves: AtomicUsize,
    issue_result_admissions: AtomicUsize,
    issue_result_admission_failure: AtomicBool,
    issue_result_failure: AtomicBool,
    issue_result_gate: Mutex<Option<Arc<Notify>>>,
    issue_result_ack: Mutex<Option<Vec<u8>>>,
    saved_result_report: Mutex<Option<Vec<u8>>>,
    cancels: AtomicUsize,
    heartbeats: AtomicUsize,
    reconciliations: AtomicUsize,
    cancel_reconciliations: AtomicUsize,
    fail_start: AtomicUsize,
    finish_calls: AtomicUsize,
    execution_finished_calls: AtomicUsize,
    finish_reconciliations: AtomicUsize,
    pending_events: AtomicUsize,
    events_calls: AtomicUsize,
    events_failure: AtomicBool,
    event_stream_drops: Arc<AtomicUsize>,
    delayed_stream_waiters: Arc<AtomicUsize>,
    delayed_stream_errors: Mutex<BTreeMap<String, Arc<Notify>>>,
    cancel_gate: Mutex<Option<Arc<Notify>>>,
    finish_gate: Mutex<Option<Arc<Notify>>>,
    finish_result: Mutex<InteractiveFinishObservation>,
    finish_reconcile_result: Mutex<OperationReconciliation<InteractiveFinishObservation>>,
    reconcile_start_result: Mutex<OperationReconciliation<StartReceipt>>,
    last_start_receipt: Mutex<Option<StartReceipt>>,
    cancel_result: Mutex<OperationReconciliation<CancellationObservation>>,
    cancel_reconcile_result: Mutex<OperationReconciliation<CancellationObservation>>,
    output: Mutex<Vec<Result<ExecutionEvent, ExecutionResult>>>,
    start_gate: Mutex<Option<Arc<Notify>>>,
    identity_attempt_override: Mutex<Option<String>>,
    identity_job_override: Mutex<Option<&'static str>>,
    preflight: Mutex<RuntimeObservation>,
    finalization: Mutex<HostFinalizationObservation>,
    workflow_result: Mutex<Result<Option<Vec<u8>>, ExecutionResult>>,
    source_inspection:
        Mutex<Result<Option<coding_agent_executor::SourceInspection>, ExecutionResult>>,
    purpose: RunPurpose,
    terminal: Arc<FakeTerminal>,
}

impl FakeExecution {
    fn new(snapshot: &AgentRunSnapshot) -> Self {
        Self {
            validation_calls: AtomicUsize::new(0),
            validation_exit: Mutex::new(Ok(0)),
            validation_gate: Mutex::new(None),
            extra_validation_ids: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            semantic_calls: AtomicUsize::new(0),
            semantic_outcome: Mutex::new(Ok(
                lookback_tauri_lib::coding_agent::assessment::AssessmentUse::Pass,
            )),
            semantic_gate: Mutex::new(None),
            events_failure: AtomicBool::new(false),
            execution_finished_calls: AtomicUsize::new(0),
            commits: AtomicUsize::new(0),
            commit_gate: Mutex::new(None),
            commit_enabled: AtomicBool::new(false),
            source_after_skip: Mutex::new(None),
            commit_observation: Mutex::new(Some(
                lookback_tauri_lib::coding_agent::workspace_commit::LocalCommitObservation {
                    workflow_result:
                        lookback_tauri_lib::coding_agent::one_shot_workflow::OneShotWorkflowResult {
                            exit_code: 0,
                            execution_time_ms: 42,
                            sandbox_id: "commit-vm".into(),
                            workflow_execution_id: Some("actual-commit-execution".into()),
                            parent_job_id: Some(12),
                        },
                    source: coding_agent_executor::SourceInspection {
                        run_id: snapshot.run_id.to_string(),
                        attempt_id: snapshot.primary_attempt_id().into(),
                        base_commit: snapshot.base_commit.clone(),
                        head_commit: snapshot.base_commit.clone(),
                        branch: snapshot.workspace_branch.clone(),
                        tracked_diff: Vec::new(),
                        uncommitted_diff: Some(Vec::new()),
                        new_files: Vec::new(),
                    },
                },
            )),
            commit_failure: Mutex::new(None),
            commit_metadata: Mutex::new(None),
            issue_result_saves: AtomicUsize::new(0),
            issue_result_admissions: AtomicUsize::new(0),
            issue_result_admission_failure: AtomicBool::new(false),
            issue_result_failure: AtomicBool::new(false),
            issue_result_gate: Mutex::new(None),
            issue_result_ack: Mutex::new(None),
            saved_result_report: Mutex::new(None),
            cancels: AtomicUsize::new(0),
            heartbeats: AtomicUsize::new(0),
            reconciliations: AtomicUsize::new(0),
            cancel_reconciliations: AtomicUsize::new(0),
            fail_start: AtomicUsize::new(0),
            finish_calls: AtomicUsize::new(0),
            finish_reconciliations: AtomicUsize::new(0),
            pending_events: AtomicUsize::new(0),
            events_calls: AtomicUsize::new(0),
            event_stream_drops: Arc::new(AtomicUsize::new(0)),
            delayed_stream_waiters: Arc::new(AtomicUsize::new(0)),
            delayed_stream_errors: Mutex::new(BTreeMap::new()),
            cancel_gate: Mutex::new(None),
            finish_gate: Mutex::new(None),
            finish_result: Mutex::new(incomplete_interactive_finish()),
            finish_reconcile_result: Mutex::new(OperationReconciliation::Unknown(None)),
            reconcile_start_result: Mutex::new(OperationReconciliation::Unknown(None)),
            last_start_receipt: Mutex::new(None),
            cancel_result: Mutex::new(OperationReconciliation::Unknown(None)),
            cancel_reconcile_result: Mutex::new(OperationReconciliation::Unknown(None)),
            output: Mutex::new(vec![
                Ok(ExecutionEvent::Output {
                    stream: "stdout".into(),
                    data: b"analysis\0".to_vec(),
                }),
                Ok(ExecutionEvent::Exit { exit_code: 0 }),
            ]),
            start_gate: Mutex::new(None),
            identity_attempt_override: Mutex::new(None),
            identity_job_override: Mutex::new(None),
            preflight: Mutex::new(runtime_observation(snapshot)),
            finalization: Mutex::new(successful_finalization()),
            workflow_result: Mutex::new(Ok(None)),
            source_inspection: Mutex::new(Ok(None)),
            purpose: snapshot.purpose,
            terminal: Arc::new(FakeTerminal::default()),
        }
    }

    fn with_output(
        snapshot: &AgentRunSnapshot,
        output: Vec<Result<ExecutionEvent, ExecutionResult>>,
    ) -> Self {
        let result = Self::new(snapshot);
        *result.output.lock().unwrap() = output;
        result
    }

    fn set_finalization(&self, finalization: HostFinalizationObservation) {
        *self.finalization.lock().unwrap() = finalization;
    }

    fn set_interactive_finish(&self, observation: InteractiveFinishObservation) {
        *self.finish_result.lock().unwrap() = observation;
    }

    fn set_interactive_finish_reconciliation(
        &self,
        result: OperationReconciliation<InteractiveFinishObservation>,
    ) {
        *self.finish_reconcile_result.lock().unwrap() = result;
    }

    fn set_output(&self, output: Vec<Result<ExecutionEvent, ExecutionResult>>) {
        *self.output.lock().unwrap() = output;
    }
}

struct PendingStreamDrop(Arc<AtomicUsize>);

impl Drop for PendingStreamDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl ExecutionAdapter for FakeExecution {
    async fn preflight(
        &self,
        _snapshot: &AgentRunSnapshot,
    ) -> Result<RuntimeObservation, ExecutionResult> {
        Ok(self.preflight.lock().unwrap().clone())
    }

    async fn prepare_workspace(
        &self,
        request: &WorkspaceRequest,
    ) -> Result<WorkspaceReceipt, ExecutionResult> {
        Ok(workspace_receipt(request))
    }

    async fn reconcile_workspace(
        &self,
        request: &WorkspaceRequest,
    ) -> Result<OperationReconciliation<WorkspaceReceipt>, ExecutionResult> {
        Ok(OperationReconciliation::Accepted(workspace_receipt(
            request,
        )))
    }

    async fn start_attempt(
        &self,
        _snapshot: &AgentRunSnapshot,
        intent: &ImmutableExecutionIntent,
        _workspace: &WorkspaceReceipt,
        _prompt: Option<Vec<u8>>,
    ) -> Result<StartReceipt, ExecutionResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut receipt = start_receipt(intent);
        if let Some(attempt_id) = self.identity_attempt_override.lock().unwrap().clone() {
            receipt.identity.attempt_id = attempt_id;
        }
        match *self.identity_job_override.lock().unwrap() {
            Some("workflow") => {
                receipt.identity.workflow_execution_id = "different-workflow".into()
            }
            Some("parent") => receipt.identity.parent_job_id = "different-parent".into(),
            Some("child") => receipt.identity.child_sandbox_job_id = "different-child".into(),
            Some("parent-only") => {
                receipt.identity.workflow_execution_id.clear();
                receipt.identity.child_sandbox_job_id.clear();
                receipt.identity.sandbox_job_id.clear();
            }
            _ => {}
        }
        *self.last_start_receipt.lock().unwrap() = Some(receipt.clone());
        let gate = self.start_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        if self.fail_start.load(Ordering::SeqCst) != 0 {
            return Err(ExecutionResult::ReconciliationUnknown);
        }
        Ok(receipt)
    }

    async fn reconcile_start(
        &self,
        _snapshot: &AgentRunSnapshot,
        _intent: &ImmutableExecutionIntent,
    ) -> Result<OperationReconciliation<StartReceipt>, ExecutionResult> {
        self.reconciliations.fetch_add(1, Ordering::SeqCst);
        Ok(self.reconcile_start_result.lock().unwrap().clone())
    }

    async fn events(&self, identity: &ExecutionIdentity) -> Result<EventStream, ExecutionResult> {
        self.events_calls.fetch_add(1, Ordering::SeqCst);
        if self.events_failure.load(Ordering::SeqCst) {
            return Err(ExecutionResult::StreamFailure);
        }
        let output = std::mem::take(&mut *self.output.lock().unwrap());
        if let Some(gate) = self
            .delayed_stream_errors
            .lock()
            .unwrap()
            .remove(&identity.attempt_id)
        {
            let drop_signal = PendingStreamDrop(self.event_stream_drops.clone());
            let waiters = self.delayed_stream_waiters.clone();
            return Ok(Box::pin(stream::iter(output).chain(stream::once(
                async move {
                    let _drop_signal = drop_signal;
                    waiters.fetch_add(1, Ordering::SeqCst);
                    gate.notified().await;
                    Err(ExecutionResult::StreamFailure)
                },
            ))));
        }
        if self.pending_events.load(Ordering::SeqCst) != 0 {
            let drop_signal = PendingStreamDrop(self.event_stream_drops.clone());
            let pending = stream::unfold(drop_signal, |guard| async move {
                std::future::pending::<()>().await;
                Some((Err(ExecutionResult::StreamFailure), guard))
            });
            return Ok(Box::pin(stream::iter(output).chain(pending)));
        }
        Ok(Box::pin(stream::iter(output)))
    }

    async fn execution_finished(&self, _identity: &ExecutionIdentity) {
        self.execution_finished_calls.fetch_add(1, Ordering::SeqCst);
    }

    async fn terminal_control(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<Arc<dyn TerminalControl>, ExecutionResult> {
        Ok(self.terminal.clone())
    }

    async fn source_inspection(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<Option<coding_agent_executor::SourceInspection>, ExecutionResult> {
        self.source_inspection.lock().unwrap().clone()
    }
    async fn assess_semantics(
        &self,
        identity: &ExecutionIdentity,
        source: Option<&coding_agent_executor::SourceInspection>,
        report: &[u8],
        validation: &ValidationEvidence,
    ) -> Result<lookback_tauri_lib::coding_agent::semantic_judge::SemanticAssessment, ExecutionResult>
    {
        assert_eq!(identity.run_id, "run-test-1");
        if let Some(source) = source {
            assert_eq!(source.run_id, identity.run_id);
            assert_eq!(source.attempt_id, identity.attempt_id);
        }
        assert!(
            serde_json::from_slice::<serde_json::Value>(report)
                .unwrap()
                .is_object()
        );
        assert_ne!(validation.state, ValidationState::Failed);
        self.semantic_calls.fetch_add(1, Ordering::SeqCst);
        let gate = self.semantic_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        (*self.semantic_outcome.lock().unwrap()).map(|outcome| {
            let mut assessment =
                lookback_tauri_lib::coding_agent::semantic_judge::SemanticAssessment::inconclusive(
                    "independent evaluation reason",
                );
            assessment.outcome = outcome;
            assessment
        })
    }

    async fn reassess_semantics(
        &self,
        identity: &ExecutionIdentity,
        source: &coding_agent_executor::SourceInspection,
        report: &[u8],
        validation: &ValidationEvidence,
        _request_key: &str,
    ) -> Result<lookback_tauri_lib::coding_agent::semantic_judge::SemanticAssessment, ExecutionResult>
    {
        self.assess_semantics(identity, Some(source), report, validation)
            .await
    }

    async fn begin_workspace_commit(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<
        Option<lookback_tauri_lib::coding_agent::workspace_commit::LocalCommitStart>,
        ExecutionResult,
    > {
        if !self.commit_enabled.load(Ordering::SeqCst) {
            if let Some(source) = self.source_after_skip.lock().unwrap().take() {
                *self.source_inspection.lock().unwrap() = Ok(Some(source));
            }
            return Ok(None);
        }
        Ok(Some(
            lookback_tauri_lib::coding_agent::workspace_commit::LocalCommitStart {
                parent_job_id: Some(12),
            },
        ))
    }
    async fn commit_workspace(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<
        Option<lookback_tauri_lib::coding_agent::workspace_commit::LocalCommitObservation>,
        ExecutionResult,
    > {
        self.commits.fetch_add(1, Ordering::SeqCst);
        let gate = self.commit_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        if let Some(error) = *self.commit_failure.lock().unwrap() {
            return Err(error);
        }
        let result = self.commit_observation.lock().unwrap().clone();
        if let Some(result) = &result {
            *self.source_inspection.lock().unwrap() = Ok(Some(result.source.clone()));
        }
        Ok(result)
    }

    async fn commit_workflow_result(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        Ok(self.commit_metadata.lock().unwrap().clone())
    }

    async fn workflow_result(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        self.workflow_result.lock().unwrap().clone()
    }

    async fn begin_issue_result(
        &self,
        _identity: &ExecutionIdentity,
        _commit: Option<&str>,
        _report: &[u8],
    ) -> Result<(), ExecutionResult> {
        assert_eq!(
            self.issue_result_admissions.fetch_add(1, Ordering::SeqCst),
            0
        );
        if self.issue_result_admission_failure.load(Ordering::SeqCst) {
            return Err(ExecutionResult::Conflict);
        }
        Ok(())
    }

    async fn save_issue_result(
        &self,
        _identity: &ExecutionIdentity,
        _commit: Option<&str>,
        report: &[u8],
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        assert!(
            serde_json::from_slice::<serde_json::Value>(report)
                .unwrap()
                .is_object()
        );
        *self.saved_result_report.lock().unwrap() = Some(report.to_vec());
        self.issue_result_saves.fetch_add(1, Ordering::SeqCst);
        assert_eq!(self.issue_result_admissions.load(Ordering::SeqCst), 1);
        let gate = self.issue_result_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        if self.issue_result_failure.load(Ordering::SeqCst) {
            return Err(ExecutionResult::ReconciliationUnknown);
        }
        Ok(Some(
            self.issue_result_ack
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| b"{\"saved\":true}".to_vec()),
        ))
    }

    async fn begin_workspace_validation(
        &self,
        _identity: &ExecutionIdentity,
        _definition: &lookback_tauri_lib::coding_agent::model::ApprovedValidationDefinition,
    ) -> Result<lookback_tauri_lib::coding_agent::workspace_commit::LocalCommitStart, ExecutionResult>
    {
        self.validation_calls.fetch_add(1, Ordering::SeqCst);
        Ok(
            lookback_tauri_lib::coding_agent::workspace_commit::LocalCommitStart {
                parent_job_id: Some(71),
            },
        )
    }
    async fn validate_workspace(
        &self,
        _identity: &ExecutionIdentity,
        definition: &lookback_tauri_lib::coding_agent::model::ApprovedValidationDefinition,
    ) -> Result<
        lookback_tauri_lib::coding_agent::workspace_validation::WorkspaceValidationObservation,
        ExecutionResult,
    > {
        let gate = self.validation_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        Ok(lookback_tauri_lib::coding_agent::workspace_validation::WorkspaceValidationObservation {
            validation_id: definition.validation_id.clone(),
            command_digest: lookback_tauri_lib::coding_agent::workspace_validation::command_digest(definition.command.as_ref().unwrap()),
            workflow_result: lookback_tauri_lib::coding_agent::one_shot_workflow::OneShotWorkflowResult {
                exit_code: (*self.validation_exit.lock().unwrap())?, execution_time_ms: 12, sandbox_id: "actual-validation-sandbox".into(),
                workflow_execution_id: Some("actual-validation-workflow".into()), parent_job_id: Some(71),
            },
        })
    }

    async fn finalize_one_shot(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<HostFinalizationObservation, ExecutionResult> {
        let mut observation = self.finalization.lock().unwrap().clone();
        observation.report = observation
            .report
            .map(|report| schema_report_for_fixture(&report, identity, self.purpose));
        let extra_ids = self.extra_validation_ids.lock().unwrap().clone();
        if !extra_ids.is_empty()
            && let Some(report) = observation.report.as_mut()
        {
            let mut value: serde_json::Value = serde_json::from_slice(report).unwrap();
            for id in &extra_ids {
                let mut validation = value["validation"][0].clone();
                validation["validation_id"] = serde_json::json!(id);
                value["validation"].as_array_mut().unwrap().push(validation);
            }
            *report = serde_json::to_vec(&value).unwrap();
        }
        Ok(observation)
    }

    async fn finalize_interactive(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<InteractiveFinishObservation, ExecutionResult> {
        self.finish_calls.fetch_add(1, Ordering::SeqCst);
        let gate = self.finish_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        let mut observation = self.finish_result.lock().unwrap().clone();
        observation.report = observation
            .report
            .map(|report| schema_report_for_fixture(&report, identity, self.purpose));
        Ok(observation)
    }

    async fn reconcile_interactive_finish(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<InteractiveFinishObservation>, ExecutionResult> {
        self.finish_reconciliations.fetch_add(1, Ordering::SeqCst);
        let result = self.finish_reconcile_result.lock().unwrap().clone();
        Ok(match result {
            OperationReconciliation::Accepted(mut observation) => {
                observation.report = observation
                    .report
                    .map(|report| schema_report_for_fixture(&report, identity, self.purpose));
                OperationReconciliation::Accepted(observation)
            }
            OperationReconciliation::NotAccepted => OperationReconciliation::NotAccepted,
            OperationReconciliation::Unknown(_) => OperationReconciliation::Unknown(None),
        })
    }

    async fn cancel(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<CancellationObservation>, ExecutionResult> {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        let gate = self.cancel_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        Ok(self.cancel_result.lock().unwrap().clone())
    }

    async fn reconcile_cancel(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<CancellationObservation>, ExecutionResult> {
        self.cancel_reconciliations.fetch_add(1, Ordering::SeqCst);
        Ok(self.cancel_reconcile_result.lock().unwrap().clone())
    }

    async fn heartbeat(&self, _identity: &ExecutionIdentity) -> Result<(), ExecutionResult> {
        self.heartbeats.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct FakeTerminal {
    writes: AtomicUsize,
    finishes: AtomicUsize,
}

#[async_trait]
impl TerminalControl for FakeTerminal {
    async fn send_input(&self, _bytes: Vec<u8>) -> Result<InputDelivery, ExecutionResult> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(InputDelivery::Accepted)
    }

    async fn finish_input(&self) -> Result<InputDelivery, ExecutionResult> {
        self.finishes.fetch_add(1, Ordering::SeqCst);
        Ok(InputDelivery::Accepted)
    }
}

struct FakePublication {
    begins: AtomicUsize,
    calls: AtomicUsize,
    reconciliations: AtomicUsize,
    outcome: Mutex<PublicationOutcome>,
    support_override: Mutex<Option<PublicationSupport>>,
    publish_gate: Mutex<Option<Arc<Notify>>>,
    admitted: AtomicBool,
    publication_job_id: Mutex<Option<i64>>,
}

#[async_trait]
impl PublicationAdapter for FakePublication {
    async fn begin_publication(
        &self,
        _request: &PublicationRequest,
    ) -> Result<lookback_tauri_lib::coding_agent::PublicationStart, ExecutionResult> {
        self.begins.fetch_add(1, Ordering::SeqCst);
        Ok(if self.admitted.load(Ordering::SeqCst) {
            lookback_tauri_lib::coding_agent::PublicationStart::Admitted {
                parent_job_id: *self.publication_job_id.lock().unwrap(),
            }
        } else {
            lookback_tauri_lib::coding_agent::PublicationStart::Inline
        })
    }

    async fn preflight(
        &self,
        _snapshot: &AgentRunSnapshot,
    ) -> Result<PublicationSupport, ExecutionResult> {
        Ok(self
            .support_override
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| PublicationSupport::Verified {
                executor_version: "test-executor".into(),
                protocol_version: 2,
                features: BTreeSet::from([
                    "prepare".into(),
                    "capture".into(),
                    "seal".into(),
                    "resolve".into(),
                    "artifact_only_publication".into(),
                    "push".into(),
                    "pull_request".into(),
                    "secret_scan_supported_formats_v1".into(),
                ]),
                approved_base_branch: "main".into(),
                approved_web_base_url: "https://forge.example".into(),
            }))
    }

    async fn publish(
        &self,
        _request: &PublicationRequest,
    ) -> Result<PublicationOutcome, ExecutionResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let gate = self.publish_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        Ok(self.outcome.lock().unwrap().clone())
    }

    async fn reconcile(
        &self,
        _request: &PublicationRequest,
    ) -> Result<PublicationOutcome, ExecutionResult> {
        self.reconciliations.fetch_add(1, Ordering::SeqCst);
        Ok(self.outcome.lock().unwrap().clone())
    }
}

#[tokio::test]
async fn approved_investigation_preserves_report_and_waits_for_review() {
    let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    let run = prepare_and_approve(&coordinator, snapshot).await;

    assert!(matches!(
        coordinator
            .start(&run.snapshot.run_id, Some(b"inspect issue".to_vec()))
            .await
            .unwrap(),
        StartOutcome::Started { .. }
    ));
    let completed =
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;

    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    let evidence = completed.execution_evidence.unwrap();
    assert!(evidence.report_save_confirmed);
    assert_eq!(evidence.final_exit_code, Some(0));
    assert!(evidence.end_present);
    assert!(!evidence.stream_error);
    assert_eq!(completed.state, RunState::AwaitingReview);
}

#[tokio::test]
async fn completed_workflow_metadata_is_durable_without_becoming_a_start_requirement() {
    let (coordinator, paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    let result = serde_json::json!({"workflow_execution_id":"actual-completed-workflow", "parent_job_id":41,
        "sandbox_id":"actual-sandbox", "exit_code":0, "execution_time_ms":1});
    *runtime.workflow_result.lock().unwrap() = Ok(Some(serde_json::to_vec(&result).unwrap()));
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator
        .start(&run.snapshot.run_id, Some(b"inspect issue".to_vec()))
        .await
        .unwrap();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
    let reopened = second_coordinator(&paths, runtime.clone());
    let saved = reopened
        .evidence_references(&run.snapshot.run_id)
        .unwrap()
        .into_iter()
        .filter_map(|reference| {
            serde_json::from_slice::<serde_json::Value>(
                &reopened
                    .read_evidence(&run.snapshot.run_id, &reference.evidence_id)
                    .unwrap(),
            )
            .ok()
        })
        .find(|value| {
            value
                .get("workflow_execution_id")
                .and_then(serde_json::Value::as_str)
                == Some("actual-completed-workflow")
        });
    assert_eq!(saved, Some(result));
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unavailable_or_oversized_workflow_metadata_keeps_the_result_unknown_without_redispatch() {
    for result in [
        Err(ExecutionResult::FinalizationUnavailable),
        Ok(Some(vec![b' '; 64 * 1024 + 1])),
    ] {
        let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
        *runtime.workflow_result.lock().unwrap() = result;
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator
            .start(&run.snapshot.run_id, Some(b"inspect issue".to_vec()))
            .await
            .unwrap();
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Unknown).await;
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn normal_finalization_uses_observed_source_without_claiming_a_workspace_freeze() {
    for uncommitted in [None, Some(vec![b'x']), Some(Vec::new())] {
        let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
        let source = coding_agent_executor::SourceInspection {
            run_id: snapshot.run_id.to_string(),
            attempt_id: snapshot.primary_attempt_id().into(),
            base_commit: snapshot.base_commit.clone(),
            head_commit: snapshot.base_commit.clone(),
            branch: snapshot.workspace_branch.clone(),
            tracked_diff: Vec::new(),
            uncommitted_diff: uncommitted.clone(),
            new_files: Vec::new(),
        };
        *runtime.source_inspection.lock().unwrap() = Ok(Some(source));
        let mut observation = successful_finalization();
        observation.workspace_freeze = WorkspaceFreezeStatus::NotFrozen;
        observation.child_writers = ChildWritersStatus::Unknown;
        runtime.set_finalization(observation);
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator
            .start(&run.snapshot.run_id, Some(b"inspect issue".to_vec()))
            .await
            .unwrap();
        let completed =
            wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
        let evidence = completed.execution_evidence.unwrap();
        assert_eq!(
            evidence.commit_consistent,
            uncommitted.as_ref().is_some_and(Vec::is_empty)
        );
        assert_eq!(
            evidence.commit_id,
            if evidence.commit_consistent {
                Some(run.snapshot.base_commit.clone())
            } else {
                None
            }
        );
    }
}

#[tokio::test]
async fn a_source_receipt_for_a_different_base_or_attempt_cannot_finalize_the_current_run() {
    for mismatch in ["base", "attempt", "branch", "unavailable"] {
        let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
        let mut source = coding_agent_executor::SourceInspection {
            run_id: snapshot.run_id.to_string(),
            attempt_id: snapshot.primary_attempt_id().into(),
            base_commit: snapshot.base_commit.clone(),
            head_commit: snapshot.base_commit.clone(),
            branch: snapshot.workspace_branch.clone(),
            tracked_diff: Vec::new(),
            uncommitted_diff: Some(Vec::new()),
            new_files: Vec::new(),
        };
        match mismatch {
            "base" => source.base_commit = "2".repeat(40),
            "attempt" => source.attempt_id = "different-attempt".into(),
            _ => source.branch = "different-branch".into(),
        };
        *runtime.source_inspection.lock().unwrap() = if mismatch == "unavailable" {
            Err(ExecutionResult::FinalizationUnavailable)
        } else {
            Ok(Some(source))
        };
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator
            .start(&run.snapshot.run_id, Some(b"inspect issue".to_vec()))
            .await
            .unwrap();
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Unknown).await;
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn automatic_preparation_does_not_require_child_ids_or_host_freeze_capabilities() {
    let (coordinator, _paths, mut snapshot, runtime) = setup(RunPurpose::Investigation, None);
    snapshot.workflow.workflow_execution_id = None;
    snapshot.workflow.parent_job_id = None;
    snapshot.workflow.child_sandbox_job_id = None;
    *runtime.identity_job_override.lock().unwrap() = Some("parent-only");
    {
        let mut observed = runtime.preflight.lock().unwrap();
        observed.runner_name = "WORKFLOW".into();
        observed.agent_cli_version = "2.0.24".into();
        observed.helper_protocol_version = 0;
        observed.helper_version.clear();
        observed.helper_features.clear();
        observed.workflow_execution_id.clear();
        observed.parent_job_id.clear();
        observed.child_sandbox_job_id.clear();
    }
    let prepared = coordinator
        .prepare_run_with_assessment_contract(
            snapshot.clone(),
            approval_assessment_contract(snapshot.purpose, false),
        )
        .await
        .unwrap();
    assert!(
        matches!(prepared, PreparationOutcome::Ready(_)),
        "{prepared:?}"
    );
    let approved = coordinator.approve_run(&snapshot.run_id).unwrap();
    let started = coordinator
        .start(&snapshot.run_id, Some(b"inspect the Issue".to_vec()))
        .await
        .unwrap();
    assert!(
        matches!(started, StartOutcome::Started { .. }),
        "{started:?}"
    );
    assert_eq!(approved.snapshot.workflow.child_sandbox_job_id, None);
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    let completed = wait_for_state(&coordinator, &snapshot.run_id, RunState::AwaitingReview).await;
    assert_eq!(completed.state, RunState::AwaitingReview);
}

#[tokio::test]
async fn arbitrary_agent_cli_version_does_not_disable_run_before_storage_or_dispatch() {
    let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    runtime.preflight.lock().unwrap().agent_cli_version = "1.18.4".into();

    let result = coordinator.prepare_run(snapshot.clone()).await.unwrap();

    assert!(matches!(result, PreparationOutcome::Ready(_)));
    assert!(coordinator.get_run(&snapshot.run_id).unwrap().is_some());
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn runtime_contract_is_accepted_without_independent_quota_evidence() {
    let (coordinator, _paths, snapshot, _runtime) = setup(RunPurpose::Investigation, None);

    let result = coordinator.prepare_run(snapshot.clone()).await.unwrap();

    assert!(matches!(result, PreparationOutcome::Ready(_)));
    assert!(coordinator.get_run(&snapshot.run_id).unwrap().is_some());
}

#[tokio::test]
async fn runtime_identity_schema_and_resource_mismatches_still_disable_runs() {
    let invalid_observations: [RuntimeInvalidation; 4] = [
        ("worker identity", |observation| observation.worker_id += 1),
        ("schema digest", |observation| {
            observation.arguments_schema_sha256 = "not-a-digest".into()
        }),
        ("admitted resources", |observation| {
            observation.admitted_resources.cpu_millis += 1
        }),
        ("resource limits", |observation| {
            observation.host_limits.cpu_millis = 999
        }),
    ];

    for (description, invalidate) in invalid_observations {
        let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
        invalidate(&mut runtime.preflight.lock().unwrap());

        let result = coordinator.prepare_run(snapshot.clone()).await.unwrap();

        let PreparationOutcome::Disabled { blockers } = result else {
            panic!("invalid {description} must disable execution")
        };
        assert!(blockers.contains(&"verified_runtime_schema_resources_or_identity".into()));
        assert!(coordinator.get_run(&snapshot.run_id).unwrap().is_none());
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn cancellation_requires_host_termination_and_quiesced_writers() {
    let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    runtime.pending_events.store(1, Ordering::SeqCst);
    *runtime.cancel_result.lock().unwrap() =
        OperationReconciliation::Accepted(CancellationObservation {
            process_exit: ProcessExitObservation::Signaled { signal: 15 },
            child_writers: ChildWritersStatus::Stopped,
        });
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();

    let cancelled = coordinator.cancel(&run.snapshot.run_id).await.unwrap();
    let repeated = coordinator.cancel(&run.snapshot.run_id).await.unwrap();

    assert_eq!(cancelled.state, RunState::Cancelled);
    assert_eq!(repeated.state, RunState::Cancelled);
    assert_eq!(runtime.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.cancel_reconciliations.load(Ordering::SeqCst), 0);
    wait_for_counter(&runtime.execution_finished_calls, 1).await;
    assert_eq!(runtime.execution_finished_calls.load(Ordering::SeqCst), 1);
    let reference = coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap()
        .into_iter()
        .find(|reference| reference.name == "cancel-termination")
        .unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(
        &coordinator
            .read_evidence(&run.snapshot.run_id, &reference.evidence_id)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["run_id"], run.snapshot.run_id.as_str());
    assert_eq!(receipt["attempt_id"], "attempt-1");
    assert_eq!(receipt["observation"]["child_writers"], "stopped");
}

#[tokio::test]
async fn confirmed_interactive_cancel_retires_capture_and_releases_the_only_slot() {
    let (coordinator, _paths, mut first_snapshot, runtime) = setup(
        RunPurpose::Investigation,
        Some(CoordinatorConfig {
            max_parallel_runs: 1,
            ..CoordinatorConfig::default()
        }),
    );
    first_snapshot.execution_mode = ExecutionMode::Interactive;
    first_snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&first_snapshot);
    runtime.set_output(Vec::new());
    runtime.pending_events.store(1, Ordering::SeqCst);
    *runtime.cancel_result.lock().unwrap() =
        OperationReconciliation::Accepted(CancellationObservation {
            process_exit: ProcessExitObservation::Killed,
            child_writers: ChildWritersStatus::Stopped,
        });
    let first = prepare_and_approve(&coordinator, first_snapshot).await;
    coordinator
        .start(&first.snapshot.run_id, None)
        .await
        .unwrap();
    let mut feed = coordinator
        .subscribe_terminal(&first.snapshot.run_id)
        .unwrap();
    assert_eq!(coordinator.terminal_owner_count(), 1);

    let cancelled = coordinator.cancel(&first.snapshot.run_id).await.unwrap();
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert_eq!(coordinator.terminal_owner_count(), 0);
    assert_eq!(runtime.event_stream_drops.load(Ordering::SeqCst), 1);
    assert!(
        coordinator
            .send_terminal_input(&first.snapshot.run_id, b"late input".to_vec())
            .await
            .is_err()
    );
    assert_eq!(runtime.terminal.writes.load(Ordering::SeqCst), 0);
    let observed: Vec<_> = std::iter::from_fn(|| feed.try_recv().ok()).collect();
    assert!(
        observed
            .iter()
            .all(|event| !matches!(event, TerminalEvent::Exit { .. }))
    );

    runtime.pending_events.store(0, Ordering::SeqCst);
    runtime.set_output(vec![Ok(ExecutionEvent::Exit { exit_code: 0 })]);
    let mut second_snapshot = snapshot(RunPurpose::Investigation);
    second_snapshot.run_id = RunId::new("run-test-2").unwrap();
    second_snapshot.execution_mode = ExecutionMode::Interactive;
    second_snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&second_snapshot);
    let second = prepare_and_approve(&coordinator, second_snapshot).await;
    let start = tokio::time::timeout(
        Duration::from_secs(1),
        coordinator.start(&second.snapshot.run_id, None),
    )
    .await
    .expect("confirmed cancellation must release its execution permit")
    .unwrap();

    assert!(
        matches!(start, StartOutcome::Started { .. }),
        "{start:?} {:?}",
        coordinator.get_run(&second.snapshot.run_id).unwrap()
    );
    assert_eq!(coordinator.terminal_owner_count(), 1);
}

#[tokio::test]
async fn cross_coordinator_confirmed_cancel_releases_pending_capture_without_stream_events() {
    let (coordinator_a, paths, mut first_snapshot, runtime) = setup(
        RunPurpose::Investigation,
        Some(CoordinatorConfig {
            max_parallel_runs: 1,
            ..CoordinatorConfig::default()
        }),
    );
    first_snapshot.execution_mode = ExecutionMode::Interactive;
    first_snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&first_snapshot);
    runtime.set_output(Vec::new());
    runtime.pending_events.store(1, Ordering::SeqCst);
    *runtime.cancel_result.lock().unwrap() =
        OperationReconciliation::Accepted(CancellationObservation {
            process_exit: ProcessExitObservation::Killed,
            child_writers: ChildWritersStatus::Stopped,
        });
    let first = prepare_and_approve(&coordinator_a, first_snapshot).await;
    coordinator_a
        .start(&first.snapshot.run_id, None)
        .await
        .unwrap();
    wait_for_counter(&runtime.events_calls, 1).await;

    let coordinator_b = second_coordinator(&paths, runtime.clone());
    let cancelled = coordinator_b.cancel(&first.snapshot.run_id).await.unwrap();
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert_eq!(runtime.event_stream_drops.load(Ordering::SeqCst), 0);

    let mut next_snapshot = snapshot(RunPurpose::Investigation);
    next_snapshot.run_id = RunId::new("run-after-cross-coordinator-cancel").unwrap();
    next_snapshot.execution_mode = ExecutionMode::Interactive;
    next_snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&next_snapshot);
    runtime.pending_events.store(0, Ordering::SeqCst);
    runtime.set_output(vec![Ok(ExecutionEvent::Exit { exit_code: 0 })]);
    let next = prepare_and_approve(&coordinator_a, next_snapshot).await;
    let start = tokio::time::timeout(
        Duration::from_secs(2),
        coordinator_a.start(&next.snapshot.run_id, None),
    )
    .await
    .expect("durable cancellation proof must retire coordinator A's pending capture")
    .unwrap();

    assert!(matches!(start, StartOutcome::Started { .. }), "{start:?}");
    assert_eq!(runtime.event_stream_drops.load(Ordering::SeqCst), 1);
    assert_eq!(coordinator_a.terminal_owner_count(), 1);
}

#[tokio::test]
async fn late_cancelled_attempt_stream_error_cannot_mutate_retry_attempt_or_owner() {
    let (coordinator, paths, mut approved, runtime) = setup(RunPurpose::Implementation, None);
    approved.execution_mode = ExecutionMode::Interactive;
    approved.attempt_ids = vec!["attempt-1".into(), "attempt-2".into()];
    approved.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&approved);
    runtime.set_output(vec![
        Ok(ExecutionEvent::Output {
            stream: "stdout".into(),
            data: b"attempt one".to_vec(),
        }),
        Ok(ExecutionEvent::Exit { exit_code: 0 }),
    ]);
    let delayed_error = Arc::new(Notify::new());
    runtime
        .delayed_stream_errors
        .lock()
        .unwrap()
        .insert("attempt-1".into(), delayed_error.clone());
    *runtime.cancel_result.lock().unwrap() =
        OperationReconciliation::Accepted(CancellationObservation {
            process_exit: ProcessExitObservation::Killed,
            child_writers: ChildWritersStatus::Stopped,
        });
    let run = prepare_and_approve(&coordinator, approved).await;
    coordinator
        .start_attempt(&run.snapshot.run_id, "attempt-1", None)
        .await
        .unwrap();
    wait_for_counter(&runtime.delayed_stream_waiters, 1).await;

    let observer = second_coordinator(&paths, runtime.clone());
    let cancelled = observer.cancel(&run.snapshot.run_id).await.unwrap();
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert_eq!(runtime.event_stream_drops.load(Ordering::SeqCst), 0);
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    let preparing = store
        .begin_retry_preparation(&run.snapshot.run_id, cancelled.revision, "attempt-1")
        .unwrap();
    runtime.set_output(vec![
        Ok(ExecutionEvent::Output {
            stream: "stdout".into(),
            data: b"attempt two".to_vec(),
        }),
        Ok(ExecutionEvent::Exit { exit_code: 0 }),
    ]);
    let second = observer
        .start_attempt(&run.snapshot.run_id, "attempt-2", None)
        .await
        .unwrap();
    assert!(matches!(second, StartOutcome::Started { .. }));
    let current = coordinator.get_run(&run.snapshot.run_id).unwrap().unwrap();
    assert_eq!(current.current_attempt_id.as_deref(), Some("attempt-2"));
    assert_eq!(current.state, RunState::Running);

    delayed_error.notify_one();
    wait_for_counter(&runtime.event_stream_drops, 1).await;
    let after_late_signal = coordinator.get_run(&run.snapshot.run_id).unwrap().unwrap();

    assert_eq!(after_late_signal.revision, current.revision);
    assert_eq!(
        after_late_signal.current_attempt_id.as_deref(),
        Some("attempt-2")
    );
    assert_eq!(after_late_signal.state, RunState::Running);
    assert!(after_late_signal.execution_evidence.is_none());
    assert!(
        after_late_signal
            .evidence
            .iter()
            .all(|reference| reference.name != "agent-report")
    );
    assert_eq!(coordinator.terminal_owner_count(), 0);
    assert_eq!(observer.terminal_owner_count(), 1);
    assert_eq!(
        observer
            .send_terminal_input(&run.snapshot.run_id, b"attempt two input".to_vec())
            .await
            .unwrap(),
        InputDelivery::Accepted
    );
    assert_eq!(runtime.terminal.writes.load(Ordering::SeqCst), 1);
    assert!(preparing.current_attempt_id.is_none());
}

#[tokio::test]
async fn trusted_interactive_host_stop_retires_a_stream_that_never_reaches_eof() {
    let (coordinator, _paths, mut first_snapshot, runtime) = setup(
        RunPurpose::Investigation,
        Some(CoordinatorConfig {
            max_parallel_runs: 1,
            finish_timeout: Duration::from_millis(20),
            ..CoordinatorConfig::default()
        }),
    );
    first_snapshot.execution_mode = ExecutionMode::Interactive;
    first_snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&first_snapshot);
    runtime.set_output(Vec::new());
    runtime.pending_events.store(1, Ordering::SeqCst);
    let mut stopped_with_failure = successful_interactive_finish();
    stopped_with_failure.cli_exit = ProcessExitObservation::ExitedNormally { exit_code: 7 };
    runtime.set_interactive_finish(stopped_with_failure);
    let first = prepare_and_approve(&coordinator, first_snapshot).await;
    coordinator
        .start(&first.snapshot.run_id, None)
        .await
        .unwrap();

    let unresolved = tokio::time::timeout(
        Duration::from_secs(1),
        coordinator.finish_interactive(&first.snapshot.run_id),
    )
    .await
    .expect("trusted stop must not wait forever for stream EOF")
    .unwrap();

    assert_eq!(unresolved.state, RunState::Unknown);
    assert!(unresolved.execution_evidence.is_none());
    assert_eq!(coordinator.terminal_owner_count(), 0);
    assert_eq!(runtime.event_stream_drops.load(Ordering::SeqCst), 1);

    runtime.pending_events.store(0, Ordering::SeqCst);
    runtime.set_output(vec![Ok(ExecutionEvent::Exit { exit_code: 0 })]);
    let mut second_snapshot = snapshot(RunPurpose::Investigation);
    second_snapshot.run_id = RunId::new("run-after-trusted-finish").unwrap();
    second_snapshot.execution_mode = ExecutionMode::Interactive;
    second_snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&second_snapshot);
    let second = prepare_and_approve(&coordinator, second_snapshot).await;
    let started = tokio::time::timeout(
        Duration::from_secs(1),
        coordinator.start(&second.snapshot.run_id, None),
    )
    .await
    .expect("trusted host finalization must release the stream slot")
    .unwrap();

    assert!(
        matches!(started, StartOutcome::Started { .. }),
        "{started:?}"
    );
    assert_eq!(coordinator.terminal_owner_count(), 1);
}

#[tokio::test]
async fn direct_interactive_cli_stop_needs_no_shell_and_never_becomes_success() {
    let (coordinator, _paths, mut snapshot, runtime) = setup(
        RunPurpose::Investigation,
        Some(CoordinatorConfig {
            finish_timeout: Duration::from_millis(20),
            ..CoordinatorConfig::default()
        }),
    );
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
    runtime.set_output(Vec::new());
    runtime.pending_events.store(1, Ordering::SeqCst);
    let mut stopped = successful_interactive_finish();
    stopped.invocation_kind = ExecutionInvocationKind::DirectProcess;
    stopped.cli_exit = ProcessExitObservation::Signaled { signal: 9 };
    stopped.shell_observation = ShellProcessObservation::NotSpawned;
    runtime.set_interactive_finish(stopped);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        coordinator.finish_interactive(&run.snapshot.run_id),
    )
    .await
    .expect("direct process stop must not wait for a nonexistent shell")
    .unwrap();

    assert_eq!(result.state, RunState::Unknown);
    assert!(result.execution_evidence.is_none());
    assert_eq!(coordinator.terminal_owner_count(), 0);
    assert_ne!(result.state, RunState::AwaitingReview);
}

#[tokio::test]
async fn direct_interactive_normal_finish_completes_with_no_shell_process() {
    let (coordinator, _paths, mut snapshot, runtime) = setup(RunPurpose::Investigation, None);
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
    let mut finish = successful_interactive_finish();
    finish.invocation_kind = ExecutionInvocationKind::DirectProcess;
    finish.shell_observation = ShellProcessObservation::NotSpawned;
    runtime.set_interactive_finish(finish);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();

    let completed = coordinator
        .finish_interactive(&run.snapshot.run_id)
        .await
        .unwrap();

    assert_eq!(completed.state, RunState::AwaitingReview);
    let evidence = completed.execution_evidence.as_ref().unwrap();
    assert_eq!(evidence.final_exit_code, Some(0));
    assert!(evidence.end_present);
    assert!(evidence.report_save_confirmed);
    let report = coordinator
        .read_evidence(
            &run.snapshot.run_id,
            evidence.report_reference.as_deref().unwrap(),
        )
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&report).unwrap();
    assert_eq!(report["summary"], "interactive report");
    let finalization = completed.interactive_finalization.as_ref().unwrap();
    assert_eq!(
        finalization.invocation_kind(),
        ExecutionInvocationKind::DirectProcess
    );
    assert_eq!(
        finalization.shell_observation(),
        &ShellProcessObservation::NotSpawned
    );
    assert_eq!(finalization.cli_exit(), &successful_exit());
}

#[tokio::test]
async fn interactive_host_validation_failure_is_retained_and_blocks_publication() {
    let (coordinator, _paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    snapshot.publication_plan = PublicationPlan::PushBranch;
    snapshot.allowed_operations = vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
    *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
    let mut finish = successful_interactive_finish();
    finish.validation = ValidationEvidence {
        required: true,
        state: ValidationState::Failed,
    };
    runtime.set_interactive_finish(finish);
    let publication = Arc::new(FakePublication {
        begins: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        reconciliations: AtomicUsize::new(0),
        outcome: Mutex::new(PublicationOutcome::Unknown),
        support_override: Mutex::new(None),
        publish_gate: Mutex::new(None),
        admitted: AtomicBool::new(false),
        publication_job_id: Mutex::new(Some(81)),
    });
    let coordinator = with_publication(coordinator, publication.clone());
    let run = prepare_and_approve_with_required_validation(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();

    let failed = coordinator
        .finish_interactive(&run.snapshot.run_id)
        .await
        .unwrap();

    assert_eq!(failed.state, RunState::Unknown);
    let evidence = failed.execution_evidence.as_ref().unwrap();
    assert_eq!(evidence.validation.state, ValidationState::Failed);
    assert!(evidence.validation.required);
    assert!(!evidence.acceptance_conditions_met);
    let host_observation = coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap()
        .into_iter()
        .find(|reference| {
            reference
                .name
                .starts_with("interactive-host-claim-observation-")
        })
        .unwrap();
    let host_observation: serde_json::Value = serde_json::from_slice(
        &coordinator
            .read_evidence(&run.snapshot.run_id, &host_observation.evidence_id)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        host_observation["validation_observation"]["state"],
        "failed"
    );
    assert_eq!(host_observation["validation_assessment"]["state"], "failed");
    assert_eq!(host_observation["validation_assessment"]["required"], true);
    assert!(coordinator.publish(&run.snapshot.run_id).await.is_err());
    assert_eq!(publication.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn interactive_unreceipted_validation_does_not_block_approved_publication() {
    let (coordinator, _paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    snapshot.publication_plan = PublicationPlan::PushBranch;
    snapshot.allowed_operations = vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
    *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
    let mut finish = successful_interactive_finish();
    finish.validation = ValidationEvidence {
        required: true,
        state: ValidationState::Passed,
    };
    runtime.set_interactive_finish(finish);
    let publication = Arc::new(FakePublication {
        begins: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        reconciliations: AtomicUsize::new(0),
        outcome: Mutex::new(PublicationOutcome::BranchPublished {
            branch: snapshot.workspace_branch.clone(),
            commit: "b".repeat(40),
            remote_head: "b".repeat(40),
        }),
        support_override: Mutex::new(None),
        publish_gate: Mutex::new(None),
        admitted: AtomicBool::new(false),
        publication_job_id: Mutex::new(Some(81)),
    });
    let coordinator = with_publication(coordinator, publication.clone());
    let run = prepare_and_approve_with_required_validation(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();

    let verifying = coordinator
        .finish_interactive(&run.snapshot.run_id)
        .await
        .unwrap();

    assert_eq!(verifying.state, RunState::Verifying);
    let evidence = verifying.execution_evidence.as_ref().unwrap();
    assert!(evidence.validation.required);
    assert_eq!(evidence.validation.state, ValidationState::NotRun);
    assert!(!evidence.acceptance_conditions_met);
    let host_observation = coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap()
        .into_iter()
        .find(|reference| {
            reference
                .name
                .starts_with("interactive-host-claim-observation-")
        })
        .unwrap();
    let host_observation: serde_json::Value = serde_json::from_slice(
        &coordinator
            .read_evidence(&run.snapshot.run_id, &host_observation.evidence_id)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        host_observation["validation_observation"]["state"],
        "passed"
    );
    assert_eq!(
        host_observation["validation_assessment"]["state"],
        "not_run"
    );

    let published = coordinator
        .publish(&run.snapshot.run_id)
        .await
        .expect("unreceipted NotRun validation alone must not block approved publication");
    assert_eq!(published.state, RunState::AwaitingReview);
    assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn uncertain_cancellation_is_reconciled_without_a_second_cancel_request() {
    let (coordinator, paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    runtime.pending_events.store(1, Ordering::SeqCst);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();

    let unknown = coordinator.cancel(&run.snapshot.run_id).await.unwrap();
    assert_eq!(unknown.state, RunState::Unknown);
    *runtime.cancel_reconcile_result.lock().unwrap() =
        OperationReconciliation::Accepted(CancellationObservation {
            process_exit: ProcessExitObservation::Killed,
            child_writers: ChildWritersStatus::Stopped,
        });
    let restarted_coordinator = second_coordinator(&paths, runtime.clone());
    let cancelled = restarted_coordinator
        .cancel(&run.snapshot.run_id)
        .await
        .unwrap();

    assert_eq!(cancelled.state, RunState::Cancelled);
    assert_eq!(runtime.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.cancel_reconciliations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_without_writer_quiescence_remains_unknown() {
    let (coordinator, _paths, first_snapshot, runtime) = setup(
        RunPurpose::Investigation,
        Some(CoordinatorConfig {
            max_parallel_runs: 1,
            ..CoordinatorConfig::default()
        }),
    );
    runtime.pending_events.store(1, Ordering::SeqCst);
    *runtime.cancel_result.lock().unwrap() =
        OperationReconciliation::Accepted(CancellationObservation {
            process_exit: ProcessExitObservation::Killed,
            child_writers: ChildWritersStatus::Active,
        });
    let run = prepare_and_approve(&coordinator, first_snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();

    let unresolved = coordinator.cancel(&run.snapshot.run_id).await.unwrap();
    let references = coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap();

    assert_eq!(unresolved.state, RunState::Unknown);
    assert!(
        !references
            .iter()
            .any(|reference| reference.name == "cancel-termination")
    );
    assert_eq!(runtime.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.event_stream_drops.load(Ordering::SeqCst), 0);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        runtime.event_stream_drops.load(Ordering::SeqCst),
        0,
        "unconfirmed stop must not let the capture watcher retire the live task"
    );

    let mut second_snapshot = snapshot(RunPurpose::Investigation);
    second_snapshot.run_id = RunId::new("run-unconfirmed-cancel-blocked").unwrap();
    *runtime.preflight.lock().unwrap() = runtime_observation(&second_snapshot);
    let second = prepare_and_approve(&coordinator, second_snapshot).await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coordinator.start(&second.snapshot.run_id, None),
        )
        .await
        .is_err(),
        "unconfirmed cancellation must keep the real execution permit active"
    );
}

#[tokio::test]
async fn concurrent_coordinators_claim_only_one_cancel_dispatch() {
    let (first_coordinator, paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    runtime.pending_events.store(1, Ordering::SeqCst);
    let run = prepare_and_approve(&first_coordinator, snapshot).await;
    first_coordinator
        .start(&run.snapshot.run_id, None)
        .await
        .unwrap();
    let second_coordinator = second_coordinator(&paths, runtime.clone());
    let gate = Arc::new(Notify::new());
    *runtime.cancel_gate.lock().unwrap() = Some(gate.clone());

    let first = {
        let coordinator = first_coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        tokio::spawn(async move { coordinator.cancel(&run_id).await })
    };
    wait_for_counter(&runtime.cancels, 1).await;
    let second = {
        let coordinator = second_coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        tokio::spawn(async move { coordinator.cancel(&run_id).await })
    };
    wait_for_counter(&runtime.cancel_reconciliations, 1).await;
    gate.notify_one();

    let first_result = first.await.unwrap();
    let second_result = second.await.unwrap();
    assert!(
        first_result.is_ok(),
        "first cancellation failed: {first_result:?}"
    );
    assert!(
        second_result.is_ok(),
        "second cancellation failed: {second_result:?}"
    );
    assert_eq!(runtime.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.cancel_reconciliations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_cancel_during_existing_interactive_finish_has_one_durable_owner() {
    let (first_coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    runtime.pending_events.store(1, Ordering::SeqCst);
    *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
    let run = prepare_and_approve(&first_coordinator, snapshot).await;
    first_coordinator
        .start(&run.snapshot.run_id, None)
        .await
        .unwrap();

    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    store
        .request_interactive_finish(&run.snapshot.run_id, "attempt-1")
        .unwrap();
    let finish_claim = store
        .claim_effect(
            &run.snapshot.run_id,
            "attempt-1",
            "finish:attempt-1",
            DurableOperationRequest::FinishInteractive { profile_version: 1 },
        )
        .unwrap();
    assert!(finish_claim.newly_created);
    let finalizing = store.get_run(&run.snapshot.run_id).unwrap().unwrap();
    store
        .transition(&run.snapshot.run_id, finalizing.revision, RunState::Unknown)
        .unwrap();
    assert_eq!(
        first_coordinator
            .get_run(&run.snapshot.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Unknown
    );

    let second_coordinator = second_coordinator(&paths, runtime.clone());
    let cancel_gate = Arc::new(Notify::new());
    *runtime.cancel_gate.lock().unwrap() = Some(cancel_gate.clone());
    let start_barrier = Arc::new(Barrier::new(3));
    let first = {
        let coordinator = first_coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        let barrier = Arc::clone(&start_barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            coordinator.cancel(&run_id).await
        })
    };
    let second = {
        let coordinator = second_coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        let barrier = Arc::clone(&start_barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            coordinator.cancel(&run_id).await
        })
    };
    start_barrier.wait().await;
    wait_for_counter(&runtime.cancels, 1).await;
    wait_for_counter(&runtime.cancel_reconciliations, 1).await;

    let cancel_claim = store
        .operation(&run.snapshot.run_id, "cancel:attempt-1")
        .unwrap()
        .expect("the cancellation claim is durable before adapter dispatch");
    assert!(matches!(
        cancel_claim.request,
        DurableOperationRequest::CancelAgent
    ));
    assert_eq!(
        store
            .operation(&run.snapshot.run_id, "finish:attempt-1")
            .unwrap()
            .unwrap(),
        finish_claim.operation
    );
    cancel_gate.notify_one();

    let first_result = first.await.unwrap().unwrap();
    let second_result = second.await.unwrap().unwrap();
    assert_eq!(first_result.state, RunState::Unknown);
    assert_eq!(second_result.state, RunState::Unknown);
    assert_eq!(runtime.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.cancel_reconciliations.load(Ordering::SeqCst), 1);

    *runtime.cancel_reconcile_result.lock().unwrap() =
        OperationReconciliation::Accepted(CancellationObservation {
            process_exit: ProcessExitObservation::Killed,
            child_writers: ChildWritersStatus::Stopped,
        });
    let third_store = Arc::new(
        lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
            paths.path().join("app-data"),
        ))
        .unwrap(),
    );
    let third_coordinator =
        CodingAgentCoordinator::new(Arc::clone(&third_store), Some(runtime.clone()), None);
    let recovered = third_coordinator
        .cancel(&run.snapshot.run_id)
        .await
        .unwrap();
    assert_eq!(recovered.state, RunState::Cancelled);
    assert_eq!(runtime.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.cancel_reconciliations.load(Ordering::SeqCst), 2);
    assert_eq!(
        third_store
            .operation(&run.snapshot.run_id, "cancel:attempt-1")
            .unwrap()
            .unwrap()
            .operation_id,
        cancel_claim.operation_id
    );
    assert_eq!(
        third_store
            .operation(&run.snapshot.run_id, "finish:attempt-1")
            .unwrap()
            .unwrap(),
        finish_claim.operation
    );
}

#[tokio::test]
async fn concurrent_coordinators_claim_only_one_interactive_finish_dispatch() {
    let (first_coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
    let run = prepare_and_approve(&first_coordinator, snapshot).await;
    first_coordinator
        .start(&run.snapshot.run_id, None)
        .await
        .unwrap();
    let second_coordinator = second_coordinator(&paths, runtime.clone());
    let gate = Arc::new(Notify::new());
    *runtime.finish_gate.lock().unwrap() = Some(gate.clone());

    let first = {
        let coordinator = first_coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        tokio::spawn(async move { coordinator.finish_interactive(&run_id).await })
    };
    wait_for_counter(&runtime.finish_calls, 1).await;
    let second = {
        let coordinator = second_coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        tokio::spawn(async move { coordinator.finish_interactive(&run_id).await })
    };
    wait_for_counter(&runtime.finish_reconciliations, 1).await;
    let second_result = second.await.unwrap();
    assert!(second_result.is_ok(), "reconcile failed: {second_result:?}");
    assert_eq!(
        first_coordinator
            .get_run(&run.snapshot.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Unknown,
        "unknown host reconciliation must remain fail-closed while the original request is in flight"
    );
    gate.notify_one();

    let first_result = first.await.unwrap();
    assert!(
        first_result.is_ok(),
        "first finish failed: {first_result:?}"
    );
    assert_eq!(runtime.finish_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.finish_reconciliations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn conflicting_effect_claim_prevents_cancel_adapter_call() {
    let (coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    store
        .request_interactive_finish(&run.snapshot.run_id, "attempt-1")
        .unwrap();
    store
        .claim_effect(
            &run.snapshot.run_id,
            "attempt-1",
            "cancel:attempt-1",
            DurableOperationRequest::FinishInteractive { profile_version: 1 },
        )
        .unwrap();

    assert!(coordinator.cancel(&run.snapshot.run_id).await.is_err());
    assert_eq!(runtime.cancels.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.cancel_reconciliations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn conflicting_effect_claim_prevents_interactive_finish_adapter_call() {
    let (coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    store
        .claim_effect(
            &run.snapshot.run_id,
            "attempt-1",
            "finish:attempt-1",
            DurableOperationRequest::CancelAgent,
        )
        .unwrap();

    assert!(
        coordinator
            .finish_interactive(&run.snapshot.run_id)
            .await
            .is_err()
    );
    assert_eq!(runtime.finish_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.finish_reconciliations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_start_metadata_is_reconciled_without_a_second_dispatch() {
    let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    runtime.fail_start.store(1, Ordering::SeqCst);
    let run = prepare_and_approve(&coordinator, snapshot).await;

    assert!(matches!(
        coordinator.start(&run.snapshot.run_id, None).await.unwrap(),
        StartOutcome::Unknown { .. }
    ));
    assert!(matches!(
        coordinator.start(&run.snapshot.run_id, None).await.unwrap(),
        StartOutcome::Unknown { .. }
    ));

    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.reconciliations.load(Ordering::SeqCst), 1);
    let current = coordinator.get_run(&run.snapshot.run_id).unwrap().unwrap();
    assert_eq!(current.current_attempt_id.as_deref(), Some("attempt-1"));
    assert_eq!(current.state, RunState::Unknown);
}

#[tokio::test]
async fn accepted_reconciled_start_retains_original_intent_and_receipt_without_redispatch() {
    let (coordinator, paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    runtime.fail_start.store(1, Ordering::SeqCst);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    assert!(matches!(
        coordinator.start(&run.snapshot.run_id, None).await.unwrap(),
        StartOutcome::Unknown { .. }
    ));
    let intent = coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap()
        .into_iter()
        .find(|reference| reference.name == "dispatch-intent")
        .unwrap();
    let original_bytes = coordinator
        .read_evidence(&run.snapshot.run_id, &intent.evidence_id)
        .unwrap();
    let receipt = runtime.last_start_receipt.lock().unwrap().clone().unwrap();
    *runtime.reconcile_start_result.lock().unwrap() = OperationReconciliation::Accepted(receipt);
    assert!(matches!(
        coordinator.start(&run.snapshot.run_id, None).await.unwrap(),
        StartOutcome::Unknown { .. }
    ));
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    let start_key = format!(
        "start-{}",
        &format!("{:x}", Sha256::digest(b"attempt-1"))[..32]
    );
    let result = store
        .operation(&run.snapshot.run_id, &start_key)
        .unwrap()
        .unwrap()
        .result
        .unwrap();
    assert_eq!(result.evidence_ids.len(), 2);
    assert_eq!(result.evidence_ids[0], intent.evidence_id);
    assert_eq!(
        store
            .read_evidence(&run.snapshot.run_id, &result.evidence_ids[0])
            .unwrap(),
        original_bytes
    );
    let receipt_ref = store
        .evidence(&run.snapshot.run_id)
        .unwrap()
        .into_iter()
        .find(|reference| reference.evidence_id == result.evidence_ids[1])
        .unwrap();
    assert_eq!(receipt_ref.name, "dispatch-receipt");
    assert_eq!(receipt_ref.attempt_id, "attempt-1");
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.reconciliations.load(Ordering::SeqCst), 1);
    assert_eq!(
        coordinator
            .get_run(&run.snapshot.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Unknown
    );
}

#[tokio::test]
async fn accepted_start_reconciliation_reuses_only_the_matching_local_capture_owner() {
    let (coordinator, paths, mut snapshot, runtime) = setup(
        RunPurpose::Implementation,
        Some(CoordinatorConfig {
            max_parallel_runs: 1,
            ..CoordinatorConfig::default()
        }),
    );
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
    runtime.pending_events.store(1, Ordering::SeqCst);
    let start_gate = Arc::new(Notify::new());
    *runtime.start_gate.lock().unwrap() = Some(start_gate.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    let first = {
        let coordinator = coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        tokio::spawn(async move {
            coordinator
                .start(&run_id, Some(b"start interactive".to_vec()))
                .await
        })
    };
    wait_for_counter(&runtime.calls, 1).await;
    let receipt = runtime.last_start_receipt.lock().unwrap().clone().unwrap();
    *runtime.reconcile_start_result.lock().unwrap() =
        OperationReconciliation::Accepted(receipt.clone());
    let repeated = {
        let coordinator = coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        tokio::spawn(async move { coordinator.start(&run_id, None).await })
    };
    start_gate.notify_one();
    let first = tokio::time::timeout(Duration::from_secs(1), first)
        .await
        .expect("first start should finish after host acceptance")
        .unwrap()
        .unwrap();
    assert!(matches!(first, StartOutcome::Started { .. }));

    let reused = tokio::time::timeout(Duration::from_secs(1), repeated)
        .await
        .expect("existing capture owner must not wait for its own execution permit")
        .unwrap()
        .unwrap();
    let StartOutcome::Reconciled {
        receipt: Some(reused_receipt),
        state: RunState::Running,
        ..
    } = reused
    else {
        panic!("matching accepted reconciliation should reuse the existing receipt")
    };
    assert_eq!(*reused_receipt, receipt);
    assert_eq!(runtime.events_calls.load(Ordering::SeqCst), 1);

    let mut stale_job_receipt = receipt.clone();
    stale_job_receipt.identity.sandbox_job_id = "sandbox-other".into();
    *runtime.reconcile_start_result.lock().unwrap() =
        OperationReconciliation::Accepted(stale_job_receipt);
    assert!(matches!(
        coordinator.start(&run.snapshot.run_id, None).await.unwrap(),
        StartOutcome::Unknown { .. }
    ));
    assert_eq!(
        coordinator
            .get_run(&run.snapshot.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Running,
        "a stale reconciliation must not displace the live owner"
    );
    assert_eq!(runtime.events_calls.load(Ordering::SeqCst), 1);

    *runtime.reconcile_start_result.lock().unwrap() =
        OperationReconciliation::Accepted(receipt.clone());
    let recovered = second_coordinator(&paths, runtime.clone())
        .start(&run.snapshot.run_id, None)
        .await
        .unwrap();
    assert!(matches!(recovered, StartOutcome::Unknown { .. }));
    assert_eq!(runtime.events_calls.load(Ordering::SeqCst), 1);

    tokio::time::timeout(
        Duration::from_secs(1),
        coordinator.cancel(&run.snapshot.run_id),
    )
    .await
    .expect("host cancellation must remain callable while capture owns the only slot")
    .unwrap();
    assert_eq!(runtime.cancels.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn known_workflow_and_job_id_mismatches_do_not_advance_the_approved_run() {
    for field in ["workflow", "parent", "child"] {
        let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
        *runtime.identity_job_override.lock().unwrap() = Some(field);
        let run = prepare_and_approve(&coordinator, snapshot).await;
        assert!(
            matches!(
                coordinator.start(&run.snapshot.run_id, None).await.unwrap(),
                StartOutcome::Unknown { .. }
            ),
            "{field}"
        );
        assert_eq!(
            coordinator
                .get_run(&run.snapshot.run_id)
                .unwrap()
                .unwrap()
                .state,
            RunState::Unknown
        );
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn stale_attempt_receipt_cannot_advance_the_claimed_attempt_to_running() {
    let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    *runtime.identity_attempt_override.lock().unwrap() = Some("attempt-2".into());
    let run = prepare_and_approve(&coordinator, snapshot).await;

    assert!(matches!(
        coordinator
            .start_attempt(&run.snapshot.run_id, "attempt-1", None)
            .await
            .unwrap(),
        StartOutcome::Unknown { .. }
    ));
    let current = coordinator.get_run(&run.snapshot.run_id).unwrap().unwrap();
    assert_eq!(current.current_attempt_id.as_deref(), Some("attempt-1"));
    assert_eq!(current.state, RunState::Unknown);
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn different_concurrent_attempt_requests_cannot_dispatch_twice() {
    let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    let gate = Arc::new(Notify::new());
    *runtime.start_gate.lock().unwrap() = Some(gate.clone());

    let first = {
        let coordinator = coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        tokio::spawn(async move { coordinator.start_attempt(&run_id, "attempt-1", None).await })
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        while runtime.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let second = {
        let coordinator = coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        tokio::spawn(async move {
            coordinator
                .start_attempt(&run_id, "attempt-2", Some(b"different".to_vec()))
                .await
        })
    };
    gate.notify_one();
    let first = first.await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(1), second)
        .await
        .expect("a different attempt must reject without waiting for the owned execution slot")
        .unwrap();

    assert!(first.is_ok());
    assert!(second.is_err());
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    let current = coordinator.get_run(&run.snapshot.run_id).unwrap().unwrap();
    assert_eq!(current.current_attempt_id.as_deref(), Some("attempt-1"));
}

#[tokio::test]
async fn incomplete_stream_and_nonzero_or_missing_report_never_complete() {
    for (events, finalization, expected) in [
        (
            vec![
                Ok(ExecutionEvent::Output {
                    stream: "stdout".into(),
                    data: b"partial".to_vec(),
                }),
                Err(ExecutionResult::StreamFailure),
            ],
            successful_finalization(),
            RunState::Unknown,
        ),
        (
            vec![Ok(ExecutionEvent::Exit { exit_code: 7 })],
            successful_finalization(),
            RunState::Failed,
        ),
        (
            vec![Ok(ExecutionEvent::Exit { exit_code: 0 })],
            HostFinalizationObservation {
                report: None,
                ..successful_finalization()
            },
            RunState::Failed,
        ),
    ] {
        let (coordinator, _paths, snapshot, runtime) =
            setup_with_events(RunPurpose::Investigation, events);
        runtime.set_finalization(finalization);
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();
        let result = wait_for_state(&coordinator, &run.snapshot.run_id, expected).await;
        assert_ne!(result.state, RunState::AwaitingReview);
        tokio::time::timeout(Duration::from_secs(1), async {
            while runtime.execution_finished_calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed finalization must stop its execution lease even while cached");
        assert_eq!(runtime.execution_finished_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn capture_attachment_failure_stops_the_already_accepted_execution_lease() {
    let (coordinator, _paths, snapshot, runtime) = setup_with_events(
        RunPurpose::Investigation,
        vec![Ok(ExecutionEvent::Exit { exit_code: 0 })],
    );
    runtime.events_failure.store(true, Ordering::SeqCst);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    assert!(coordinator.start(&run.snapshot.run_id, None).await.is_err());
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(1), async {
        while runtime.execution_finished_calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accepted execution without a capture owner must stop lease maintenance");
    assert_eq!(runtime.execution_finished_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 0);
    assert_eq!(
        coordinator
            .get_run(&run.snapshot.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Unknown
    );
}

#[tokio::test]
async fn automatic_report_completion_does_not_require_vm_quiescence_or_workspace_freeze() {
    for writers in [ChildWritersStatus::Unknown, ChildWritersStatus::Active] {
        for freeze in [
            WorkspaceFreezeStatus::NotFrozen,
            WorkspaceFreezeStatus::Unknown,
        ] {
            let (coordinator, _paths, snapshot, runtime) = setup_with_events(
                RunPurpose::Investigation,
                vec![Ok(ExecutionEvent::Exit { exit_code: 0 })],
            );
            runtime.set_finalization(HostFinalizationObservation {
                invocation_kind: ExecutionInvocationKind::DirectProcess,
                shell_observation: ShellProcessObservation::NotSpawned,
                child_writers: writers,
                workspace_freeze: freeze,
                ..successful_finalization()
            });
            let run = prepare_and_approve(&coordinator, snapshot).await;
            coordinator.start(&run.snapshot.run_id, None).await.unwrap();
            let completed =
                wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
            let evidence = completed.execution_evidence.unwrap();
            assert!(evidence.report_save_confirmed);
            assert!(evidence.end_present);
            assert_eq!(evidence.final_exit_code, Some(0));
            assert_eq!(evidence.commit_id, None);
            assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn host_finalization_requires_shell_evidence_that_matches_invocation_kind() {
    let mut direct_success = successful_finalization();
    direct_success.invocation_kind = ExecutionInvocationKind::DirectProcess;
    direct_success.shell_observation = ShellProcessObservation::NotSpawned;

    let mut direct_nonzero = direct_success.clone();
    direct_nonzero.cli_exit = ProcessExitObservation::ExitedNormally { exit_code: 7 };
    let mut direct_missing_exit = direct_success.clone();
    direct_missing_exit.cli_exit = ProcessExitObservation::Unknown;
    let mut direct_signaled = direct_success.clone();
    direct_signaled.cli_exit = ProcessExitObservation::Signaled { signal: 9 };
    let mut direct_with_shell = direct_success.clone();
    direct_with_shell.shell_observation = ShellProcessObservation::Spawned {
        exit: successful_exit(),
    };
    let mut wrapper_without_shell = successful_finalization();
    wrapper_without_shell.shell_observation = ShellProcessObservation::NotSpawned;
    let mut wrapper_unknown_shell = successful_finalization();
    wrapper_unknown_shell.shell_observation = ShellProcessObservation::Unknown;
    let mut wrapper_nonzero_shell = successful_finalization();
    wrapper_nonzero_shell.shell_observation = ShellProcessObservation::Spawned {
        exit: ProcessExitObservation::ExitedNormally { exit_code: 3 },
    };
    let mut wrapper_signaled_shell = successful_finalization();
    wrapper_signaled_shell.shell_observation = ShellProcessObservation::Spawned {
        exit: ProcessExitObservation::Signaled { signal: 9 },
    };
    let mut legacy_unknown_kind = successful_finalization();
    legacy_unknown_kind.invocation_kind = ExecutionInvocationKind::Unknown;

    let valid_wrapper = successful_finalization();
    let cases = [
        ("direct process", direct_success, true),
        ("direct nonzero exit", direct_nonzero, false),
        ("direct missing exit", direct_missing_exit, false),
        ("direct signaled exit", direct_signaled, false),
        (
            "direct process with spawned shell",
            direct_with_shell,
            false,
        ),
        (
            "wrapper without shell observation",
            wrapper_without_shell,
            false,
        ),
        (
            "wrapper with unknown shell exit",
            wrapper_unknown_shell,
            false,
        ),
        (
            "wrapper with nonzero shell exit",
            wrapper_nonzero_shell,
            false,
        ),
        ("wrapper with signaled shell", wrapper_signaled_shell, false),
        ("legacy unknown invocation kind", legacy_unknown_kind, false),
        ("wrapper with observed zero exit", valid_wrapper, true),
    ];

    for (description, finalization, completes) in cases {
        let (coordinator, _paths, snapshot, runtime) = setup_with_events(
            RunPurpose::Investigation,
            vec![Ok(ExecutionEvent::Exit { exit_code: 0 })],
        );
        runtime.set_finalization(finalization);
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();
        let expected = if completes {
            RunState::AwaitingReview
        } else {
            RunState::Unknown
        };
        let result = wait_for_state(&coordinator, &run.snapshot.run_id, expected).await;
        assert_eq!(result.state, expected, "host evidence case {description}");
        if !completes {
            assert_ne!(result.state, RunState::AwaitingReview, "{description}");
        }
    }
}

#[tokio::test]
async fn capture_is_bounded_and_disposing_feed_subscriber_does_not_cancel_owner() {
    let (coordinator, _paths, mut snapshot, runtime) = setup(
        RunPurpose::Investigation,
        Some(CoordinatorConfig {
            max_capture_bytes_per_stream: 4,
            ..CoordinatorConfig::default()
        }),
    );
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    let feed = coordinator
        .subscribe_terminal(&run.snapshot.run_id)
        .unwrap();
    drop(feed);
    let unknown = wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Unknown).await;

    assert_eq!(runtime.cancels.load(Ordering::SeqCst), 0);
    assert_eq!(coordinator.terminal_owner_count(), 1);
    assert_eq!(unknown.execution_evidence, None);
    let references = coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap();
    let capture_status = references
        .iter()
        .find(|reference| reference.name == "capture-status")
        .unwrap();
    let capture_status: serde_json::Value = serde_json::from_slice(
        &coordinator
            .read_evidence(&run.snapshot.run_id, &capture_status.evidence_id)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(capture_status["capture_complete"], false);
    assert!(capture_status["stdout_bytes"].as_u64().unwrap() <= 4);
}

#[tokio::test]
async fn heartbeat_remains_independent_of_the_single_execution_slot() {
    let (coordinator, _paths, snapshot, runtime) = setup(
        RunPurpose::Investigation,
        Some(CoordinatorConfig {
            max_parallel_runs: 1,
            ..CoordinatorConfig::default()
        }),
    );
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();

    tokio::time::timeout(
        Duration::from_secs(1),
        coordinator.heartbeat(&run.snapshot.run_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(runtime.heartbeats.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn interactive_finish_without_host_writer_and_freeze_proof_fails_closed() {
    let (coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator
        .start(&run.snapshot.run_id, Some(b"start interactive".to_vec()))
        .await
        .unwrap();
    let finalizing = coordinator
        .finish_interactive(&run.snapshot.run_id)
        .await
        .unwrap();

    assert!(matches!(
        finalizing.state,
        RunState::Unknown | RunState::Finalizing
    ));
    assert!(finalizing.interactive_finalization.is_none());
    assert!(finalizing.execution_evidence.is_none());
    assert_finish_operation_status(
        &paths,
        &run.snapshot.run_id,
        DurableOperationStatus::ResultUnconfirmed,
    );
    let restarted_coordinator = second_coordinator(&paths, runtime.clone());
    let _ = restarted_coordinator
        .reconcile(&run.snapshot.run_id)
        .await
        .unwrap();
    assert_eq!(runtime.finish_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.finish_reconciliations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn incomplete_interactive_finish_stays_unconfirmed_then_reconciles_without_redispatch() {
    let incomplete = [
        incomplete_interactive_finish(),
        interactive_finish_observation(
            successful_exit(),
            ShellProcessObservation::Spawned {
                exit: successful_exit(),
            },
            ChildWritersStatus::Stopped,
            WorkspaceFreezeStatus::Unknown,
        ),
        interactive_finish_observation(
            ProcessExitObservation::ExitedNormally { exit_code: 17 },
            ShellProcessObservation::Spawned {
                exit: successful_exit(),
            },
            ChildWritersStatus::Stopped,
            WorkspaceFreezeStatus::HostVerified {
                commit_id: "b".repeat(40),
            },
        ),
        interactive_finish_observation(
            successful_exit(),
            ShellProcessObservation::Spawned {
                exit: ProcessExitObservation::Unknown,
            },
            ChildWritersStatus::Stopped,
            WorkspaceFreezeStatus::HostVerified {
                commit_id: "b".repeat(40),
            },
        ),
    ];

    for (index, observation) in incomplete.into_iter().enumerate() {
        let (coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
        snapshot.execution_mode = ExecutionMode::Interactive;
        snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
        *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
        runtime.set_interactive_finish(observation);
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();

        let unknown = coordinator
            .finish_interactive(&run.snapshot.run_id)
            .await
            .unwrap();
        assert_eq!(unknown.state, RunState::Unknown, "case {index}");
        assert_eq!(
            unknown.interactive_finalization.is_some(),
            index == 2,
            "persist only the case whose host observation proves quiescence"
        );
        assert!(unknown.execution_evidence.is_none(), "case {index}");
        assert_finish_operation_status(
            &paths,
            &run.snapshot.run_id,
            DurableOperationStatus::ResultUnconfirmed,
        );
        assert_eq!(
            runtime.finish_calls.load(Ordering::SeqCst),
            1,
            "case {index}"
        );

        if index == 0 {
            let repeated_observation = incomplete_interactive_finish();
            runtime.set_interactive_finish_reconciliation(OperationReconciliation::Accepted(
                repeated_observation,
            ));
            let repeated = coordinator
                .finish_interactive(&run.snapshot.run_id)
                .await
                .unwrap();
            assert_eq!(repeated.state, RunState::Unknown);
            let repeated_claims = coordinator
                .evidence_references(&run.snapshot.run_id)
                .unwrap()
                .into_iter()
                .filter(|reference| {
                    reference
                        .name
                        .starts_with("interactive-host-claim-observation-")
                })
                .collect::<Vec<_>>();
            assert_eq!(repeated_claims.len(), 1);
            assert_content_addressed_host_claim_names(&repeated_claims);

            let mut completed_observation = successful_interactive_finish();
            completed_observation.validation = ValidationEvidence {
                required: true,
                state: ValidationState::Passed,
            };
            completed_observation.acceptance_conditions_met = true;
            runtime.set_interactive_finish_reconciliation(OperationReconciliation::Accepted(
                completed_observation,
            ));
            let reconciled = coordinator
                .finish_interactive(&run.snapshot.run_id)
                .await
                .unwrap();
            assert_ne!(reconciled.state, RunState::Unknown);
            assert!(reconciled.interactive_finalization.is_some());
            assert_finish_operation_status(
                &paths,
                &run.snapshot.run_id,
                DurableOperationStatus::Completed,
            );
            assert_eq!(runtime.finish_calls.load(Ordering::SeqCst), 1);
            assert_eq!(runtime.finish_reconciliations.load(Ordering::SeqCst), 2);
            assert!(
                !reconciled
                    .execution_evidence
                    .as_ref()
                    .unwrap()
                    .acceptance_conditions_met,
                "unreceipted host acceptance must stay observational"
            );
            let observation_references = coordinator
                .evidence_references(&run.snapshot.run_id)
                .unwrap()
                .into_iter()
                .filter(|reference| {
                    reference
                        .name
                        .starts_with("interactive-host-claim-observation-")
                })
                .collect::<Vec<_>>();
            assert_content_addressed_host_claim_names(&observation_references);
            let observations = observation_references
                .into_iter()
                .map(|reference| {
                    serde_json::from_slice::<serde_json::Value>(
                        &coordinator
                            .read_evidence(&run.snapshot.run_id, &reference.evidence_id)
                            .unwrap(),
                    )
                    .unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(observations.len(), 2);
            assert!(observations.iter().any(|observation| {
                observation["validation_observation"]["state"] == "not_run"
                    && observation["acceptance_conditions_met_observation"] == false
            }));
            assert!(observations.iter().any(|observation| {
                observation["validation_observation"]["state"] == "passed"
                    && observation["acceptance_conditions_met_observation"] == true
                    && observation["validation_assessment"]["state"] == "not_run"
            }));
        }
    }
}

#[tokio::test]
async fn interactive_failed_validation_stays_latched_across_a_later_host_pass_claim() {
    let (coordinator, _paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    snapshot.publication_plan = PublicationPlan::PushBranch;
    snapshot.allowed_operations = vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
    *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);

    let mut first_observation = incomplete_interactive_finish();
    first_observation.validation = ValidationEvidence {
        required: true,
        state: ValidationState::Failed,
    };
    first_observation.acceptance_conditions_met = true;
    runtime.set_interactive_finish(first_observation);
    let publication = Arc::new(FakePublication {
        begins: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        reconciliations: AtomicUsize::new(0),
        outcome: Mutex::new(PublicationOutcome::Unknown),
        support_override: Mutex::new(None),
        publish_gate: Mutex::new(None),
        admitted: AtomicBool::new(false),
        publication_job_id: Mutex::new(Some(81)),
    });
    let coordinator = with_publication(coordinator, publication.clone());
    let run = prepare_and_approve_with_required_validation(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();

    let first = coordinator
        .finish_interactive(&run.snapshot.run_id)
        .await
        .unwrap();
    assert_eq!(first.state, RunState::Unknown);
    assert_eq!(
        first.execution_evidence.as_ref().unwrap().validation.state,
        ValidationState::Failed
    );

    let mut later_observation = successful_interactive_finish();
    later_observation.validation = ValidationEvidence {
        required: true,
        state: ValidationState::Passed,
    };
    later_observation.acceptance_conditions_met = true;
    runtime.set_interactive_finish_reconciliation(OperationReconciliation::Accepted(
        later_observation,
    ));
    let reconciled = coordinator
        .finish_interactive(&run.snapshot.run_id)
        .await
        .unwrap();

    assert_eq!(reconciled.state, RunState::Unknown);
    assert_eq!(
        reconciled
            .execution_evidence
            .as_ref()
            .unwrap()
            .validation
            .state,
        ValidationState::Failed
    );
    assert!(
        !reconciled
            .execution_evidence
            .as_ref()
            .unwrap()
            .acceptance_conditions_met
    );
    let observation_references = coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap()
        .into_iter()
        .filter(|reference| {
            reference
                .name
                .starts_with("interactive-host-claim-observation-")
        })
        .collect::<Vec<_>>();
    assert_content_addressed_host_claim_names(&observation_references);
    let observations = observation_references
        .into_iter()
        .map(|reference| {
            serde_json::from_slice::<serde_json::Value>(
                &coordinator
                    .read_evidence(&run.snapshot.run_id, &reference.evidence_id)
                    .unwrap(),
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(observations.len(), 2);
    assert!(observations.iter().any(|observation| {
        observation["validation_observation"]["state"] == "failed"
            && observation["validation_assessment"]["state"] == "failed"
    }));
    assert!(observations.iter().any(|observation| {
        observation["validation_observation"]["state"] == "passed"
            && observation["validation_assessment"]["state"] == "failed"
            && observation["acceptance_conditions_met_observation"] == true
    }));
    assert!(coordinator.publish(&run.snapshot.run_id).await.is_err());
    assert_eq!(publication.calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.finish_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.finish_reconciliations.load(Ordering::SeqCst), 1);
}

fn assert_content_addressed_host_claim_names(
    references: &[lookback_tauri_lib::coding_agent::EvidenceReference],
) {
    const PREFIX: &str = "interactive-host-claim-observation-";
    for reference in references {
        assert!(reference.name.len() <= 120);
        let digest = reference.name.strip_prefix(PREFIX).unwrap();
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}

#[tokio::test]
async fn incomplete_finish_report_is_observational_and_final_report_can_change_on_reconcile() {
    let (coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.execution_mode = ExecutionMode::Interactive;
    snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
    *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
    let mut first_observation = incomplete_interactive_finish();
    first_observation.report = Some(b"interim report A".to_vec());
    runtime.set_interactive_finish(first_observation);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();

    let first = coordinator
        .finish_interactive(&run.snapshot.run_id)
        .await
        .unwrap();
    assert_eq!(first.state, RunState::Unknown);
    assert!(first.execution_evidence.is_none());
    assert_eq!(runtime.finish_calls.load(Ordering::SeqCst), 1);
    let interim_ref = coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap()
        .into_iter()
        .find(|reference| reference.name == "finish-report-observation")
        .expect("incomplete report should be retained only as an observation");
    assert!(
        !coordinator
            .evidence_references(&run.snapshot.run_id)
            .unwrap()
            .iter()
            .any(|reference| reference.name == "agent-report")
    );
    let interim: serde_json::Value = serde_json::from_slice(
        &coordinator
            .read_evidence(&run.snapshot.run_id, &interim_ref.evidence_id)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        interim["report_sha256"],
        format!(
            "{:x}",
            Sha256::digest(test_report_bytes(
                run.snapshot.run_id.as_str(),
                "attempt-1",
                RunPurpose::Implementation,
                "interim report A",
            ))
        )
    );
    assert_eq!(
        interim["authority"],
        "unconfirmed_interactive_finish_observation"
    );

    let mut final_observation = successful_interactive_finish();
    final_observation.report = Some(b"final report B".to_vec());
    runtime.set_interactive_finish_reconciliation(OperationReconciliation::Accepted(
        final_observation,
    ));
    let reconciled = coordinator
        .finish_interactive(&run.snapshot.run_id)
        .await
        .unwrap();

    assert_ne!(reconciled.state, RunState::Unknown);
    assert_eq!(runtime.finish_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.finish_reconciliations.load(Ordering::SeqCst), 1);
    let evidence = reconciled.execution_evidence.unwrap();
    let final_report = evidence.report_reference.unwrap();
    let final_report: serde_json::Value = serde_json::from_slice(
        &coordinator
            .read_evidence(&run.snapshot.run_id, &final_report)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(final_report["summary"], "final report B");
    assert_finish_operation_status(
        &paths,
        &run.snapshot.run_id,
        DurableOperationStatus::Completed,
    );
}

#[tokio::test]
async fn missing_or_whitespace_finish_report_reuses_completed_capture_on_same_intent_retry() {
    for (index, initial_report) in [None, Some(b" \n\t".to_vec())].into_iter().enumerate() {
        let (coordinator, paths, mut snapshot, runtime) = setup(
            RunPurpose::Implementation,
            Some(CoordinatorConfig {
                max_parallel_runs: 1,
                ..CoordinatorConfig::default()
            }),
        );
        snapshot.execution_mode = ExecutionMode::Interactive;
        snapshot.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
        *runtime.preflight.lock().unwrap() = runtime_observation(&snapshot);
        let mut first_observation = successful_interactive_finish();
        first_observation.report = initial_report;
        runtime.set_interactive_finish(first_observation);
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();

        let first = coordinator
            .finish_interactive(&run.snapshot.run_id)
            .await
            .unwrap();
        assert_eq!(first.state, RunState::Unknown, "case {index}");
        assert!(first.interactive_finalization.is_some(), "case {index}");
        assert!(first.execution_evidence.is_none(), "case {index}");
        assert_eq!(coordinator.terminal_owner_count(), 0, "case {index}");
        assert!(
            first
                .evidence
                .iter()
                .any(|reference| reference.name == "capture-status"),
            "completed capture must remain durable for case {index}"
        );
        assert!(
            first
                .evidence
                .iter()
                .all(|reference| reference.name != "agent-report"),
            "missing report must not become canonical for case {index}"
        );

        let mut final_observation = successful_interactive_finish();
        final_observation.report = Some(b"final report B".to_vec());
        runtime.set_interactive_finish_reconciliation(OperationReconciliation::Accepted(
            final_observation,
        ));
        let retry_coordinator = if index == 0 {
            coordinator.clone()
        } else {
            second_coordinator(&paths, runtime.clone())
        };
        let completed = retry_coordinator
            .finish_interactive(&run.snapshot.run_id)
            .await
            .unwrap();

        assert_ne!(completed.state, RunState::Unknown, "case {index}");
        assert_eq!(
            runtime.finish_calls.load(Ordering::SeqCst),
            1,
            "case {index}"
        );
        assert_eq!(
            runtime.finish_reconciliations.load(Ordering::SeqCst),
            1,
            "case {index}"
        );
        let evidence = completed.execution_evidence.unwrap();
        assert_eq!(evidence.final_exit_code, Some(0), "case {index}");
        assert!(evidence.end_present, "case {index}");
        assert!(!evidence.stream_error, "case {index}");
        assert!(evidence.stdout_bytes > 0, "case {index}");
        let report_reference = evidence.report_reference.unwrap();
        let report: serde_json::Value = serde_json::from_slice(
            &coordinator
                .read_evidence(&run.snapshot.run_id, &report_reference)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(report["summary"], "final report B", "case {index}");
    }
}

#[tokio::test]
async fn empty_disabled_publication_support_blocks_preparation() {
    let (coordinator, _paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.publication_plan = PublicationPlan::PushBranch;
    snapshot.allowed_operations = vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
    let publication = Arc::new(FakePublication {
        begins: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        reconciliations: AtomicUsize::new(0),
        outcome: Mutex::new(PublicationOutcome::Unknown),
        support_override: Mutex::new(Some(PublicationSupport::Disabled(Vec::new()))),
        publish_gate: Mutex::new(None),
        admitted: AtomicBool::new(false),
        publication_job_id: Mutex::new(Some(81)),
    });
    let coordinator = with_publication(coordinator, publication.clone());

    let outcome = coordinator.prepare_run(snapshot.clone()).await.unwrap();

    let PreparationOutcome::Disabled { blockers } = outcome else {
        panic!("empty Disabled support must not pass publication preflight")
    };
    assert!(blockers.contains(&"publication_support_disabled".to_owned()));
    assert!(coordinator.get_run(&snapshot.run_id).unwrap().is_none());
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 0);
    assert_eq!(publication.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn existing_cancel_claim_blocks_publication_before_intent_or_remote_call() {
    let (coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.publication_plan = PublicationPlan::PushBranch;
    snapshot.allowed_operations = vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
    let publication = Arc::new(FakePublication {
        begins: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        reconciliations: AtomicUsize::new(0),
        outcome: Mutex::new(PublicationOutcome::Unknown),
        support_override: Mutex::new(None),
        publish_gate: Mutex::new(None),
        admitted: AtomicBool::new(false),
        publication_job_id: Mutex::new(Some(81)),
    });
    let coordinator = with_publication(coordinator, publication.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Verifying).await;
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    store
        .claim_effect(
            &run.snapshot.run_id,
            "attempt-1",
            "cancel:attempt-1",
            DurableOperationRequest::CancelAgent,
        )
        .unwrap();

    let unresolved = coordinator.publish(&run.snapshot.run_id).await.unwrap();

    assert_eq!(unresolved.state, RunState::Unknown);
    assert_eq!(publication.calls.load(Ordering::SeqCst), 0);
    assert!(
        store
            .unresolved_operations()
            .unwrap()
            .iter()
            .all(|operation| {
                operation.run_id != run.snapshot.run_id
                    || !matches!(
                        &operation.request,
                        DurableOperationRequest::PublishBranch { .. }
                            | DurableOperationRequest::CreatePullRequest { .. }
                    )
            })
    );
    assert!(
        !coordinator
            .evidence_references(&run.snapshot.run_id)
            .unwrap()
            .iter()
            .any(|reference| reference.name == "publication-intent")
    );
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_during_publication_keeps_receipt_partial_and_operation_unconfirmed() {
    let (coordinator, paths, mut snapshot, _runtime) = setup(RunPurpose::Implementation, None);
    snapshot.publication_plan = PublicationPlan::PushBranch;
    snapshot.allowed_operations = vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
    let publication = Arc::new(FakePublication {
        begins: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        reconciliations: AtomicUsize::new(0),
        outcome: Mutex::new(PublicationOutcome::BranchPublished {
            branch: snapshot.workspace_branch.clone(),
            commit: "b".repeat(40),
            remote_head: "b".repeat(40),
        }),
        support_override: Mutex::new(None),
        publish_gate: Mutex::new(None),
        admitted: AtomicBool::new(false),
        publication_job_id: Mutex::new(Some(81)),
    });
    let coordinator = with_publication(coordinator, publication.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Verifying).await;
    let gate = Arc::new(Notify::new());
    *publication.publish_gate.lock().unwrap() = Some(gate.clone());

    let publish = {
        let coordinator = coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        tokio::spawn(async move { coordinator.publish(&run_id).await })
    };
    wait_for_counter(&publication.calls, 1).await;
    let cancel_store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    let publishing = coordinator.get_run(&run.snapshot.run_id).unwrap().unwrap();
    assert_eq!(publishing.state, RunState::Publishing);
    assert!(matches!(
        cancel_store.claim_publication_effect(
            &run.snapshot.run_id,
            publishing.revision,
            "attempt-1",
            "publish:fresh-key",
            DurableOperationRequest::PublishBranch {
                branch: publishing.snapshot.workspace_branch.clone(),
                commit_id: "b".repeat(40),
            },
        ),
        Err(lookback_tauri_lib::coding_agent::CodingAgentError::Conflict(_))
    ));
    cancel_store
        .claim_effect(
            &run.snapshot.run_id,
            "attempt-1",
            "cancel:attempt-1",
            DurableOperationRequest::CancelAgent,
        )
        .unwrap();
    gate.notify_one();
    let published = publish.await.unwrap().unwrap();

    assert_eq!(published.state, RunState::Unknown);
    assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
    let references = coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap();
    assert!(
        references
            .iter()
            .any(|reference| reference.name == "publication-receipt")
    );
    let receipt_id = references
        .iter()
        .find(|reference| reference.name == "publication-receipt")
        .unwrap()
        .evidence_id
        .clone();
    let request_ref = references
        .iter()
        .find(|reference| reference.name == "publication-intent")
        .unwrap();
    let request: PublicationRequest = serde_json::from_slice(
        &coordinator
            .read_evidence(&run.snapshot.run_id, &request_ref.evidence_id)
            .unwrap(),
    )
    .unwrap();
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    let operation = store
        .operation(&run.snapshot.run_id, &request.operation_key)
        .unwrap()
        .unwrap();
    assert_eq!(operation.status, DurableOperationStatus::ResultUnconfirmed);
    let result = operation.result.unwrap();
    assert_eq!(result.outcome, DurableOperationOutcome::ResultUnconfirmed);
    assert_eq!(result.evidence_ids, vec![receipt_id]);
    let _ = coordinator.publish(&run.snapshot.run_id).await;
    assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn admitted_publication_drain_does_not_block_coordinator_cancellation() {
    let (coordinator, _paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.publication_plan = PublicationPlan::PushBranch;
    snapshot.allowed_operations = vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
    let gate = Arc::new(Notify::new());
    let publication = Arc::new(FakePublication {
        begins: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        reconciliations: AtomicUsize::new(0),
        outcome: Mutex::new(PublicationOutcome::BranchPublished {
            branch: snapshot.workspace_branch.clone(),
            commit: "b".repeat(40),
            remote_head: "b".repeat(40),
        }),
        support_override: Mutex::new(None),
        publish_gate: Mutex::new(Some(gate.clone())),
        admitted: AtomicBool::new(true),
        publication_job_id: Mutex::new(Some(81)),
    });
    let coordinator = with_publication(coordinator, publication.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Verifying).await;
    let publish = {
        let coordinator = coordinator.clone();
        let id = run.snapshot.run_id.clone();
        tokio::spawn(async move { coordinator.publish(&id).await })
    };
    wait_for_counter(&publication.calls, 1).await;
    let cancel = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        coordinator.cancel(&run.snapshot.run_id),
    )
    .await;
    gate.notify_one();
    let published = publish.await.unwrap().unwrap();
    assert!(
        cancel.is_ok(),
        "cancellation must enter the adapter while publication waits"
    );
    assert_eq!(runtime.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(published.state, RunState::Unknown);
    assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
    let start = coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap()
        .into_iter()
        .find(|reference| reference.name == "publication-workflow-start")
        .unwrap();
    let bytes = coordinator
        .read_evidence(&run.snapshot.run_id, &start.evidence_id)
        .unwrap();
    let observed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(observed["parent_job_id"], 81);
}

#[tokio::test]
async fn admitted_publication_uses_optional_actual_ids_and_rejects_invalid_ids() {
    for parent_job_id in [None, Some(81), Some(0), Some(-1)] {
        let (coordinator, _paths, mut snapshot, _runtime) = setup(RunPurpose::Implementation, None);
        snapshot.publication_plan = PublicationPlan::PushBranch;
        snapshot.allowed_operations =
            vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
        let publication = Arc::new(FakePublication {
            begins: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            reconciliations: AtomicUsize::new(0),
            outcome: Mutex::new(PublicationOutcome::BranchPublished {
                branch: snapshot.workspace_branch.clone(),
                commit: "b".repeat(40),
                remote_head: "b".repeat(40),
            }),
            support_override: Mutex::new(None),
            publish_gate: Mutex::new(None),
            admitted: AtomicBool::new(true),
            publication_job_id: Mutex::new(parent_job_id),
        });
        let coordinator = with_publication(coordinator, publication.clone());
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Verifying).await;
        let published = coordinator.publish(&run.snapshot.run_id).await.unwrap();
        if parent_job_id.is_some_and(|id| id <= 0) {
            assert_eq!(published.state, RunState::Unknown);
            assert_eq!(publication.calls.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(published.state, RunState::AwaitingReview);
            assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
            let start = coordinator
                .evidence_references(&run.snapshot.run_id)
                .unwrap()
                .into_iter()
                .find(|reference| reference.name == "publication-workflow-start")
                .unwrap();
            let observed: serde_json::Value = serde_json::from_slice(
                &coordinator
                    .read_evidence(&run.snapshot.run_id, &start.evidence_id)
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(observed["parent_job_id"], serde_json::json!(parent_job_id));
        }
    }
}

#[tokio::test]
async fn interrupted_admitted_publication_is_not_redispatched_after_store_reopen() {
    let (coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.publication_plan = PublicationPlan::PushBranch;
    snapshot.allowed_operations = vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
    let publication = Arc::new(FakePublication {
        begins: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        reconciliations: AtomicUsize::new(0),
        outcome: Mutex::new(PublicationOutcome::Unknown),
        support_override: Mutex::new(None),
        publish_gate: Mutex::new(Some(Arc::new(Notify::new()))),
        admitted: AtomicBool::new(true),
        publication_job_id: Mutex::new(Some(81)),
    });
    let coordinator = with_publication(coordinator, publication.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Verifying).await;
    let task = {
        let coordinator = coordinator.clone();
        let id = run.snapshot.run_id.clone();
        tokio::spawn(async move { coordinator.publish(&id).await })
    };
    wait_for_counter(&publication.calls, 1).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let reopened =
        second_coordinator(&paths, runtime).with_publication_adapter(publication.clone());
    reopened.recover_on_startup().unwrap();
    let reconciled = reopened.reconcile(&run.snapshot.run_id).await.unwrap();
    assert!(matches!(
        reconciled,
        lookback_tauri_lib::coding_agent::RunReconciliation::Unknown(_)
    ));
    let _ = reopened.publish(&run.snapshot.run_id).await;
    assert_eq!(publication.begins.load(Ordering::SeqCst), 1);
    assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        reopened
            .get_run(&run.snapshot.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Publishing
    );
    assert!(
        reopened
            .evidence_references(&run.snapshot.run_id)
            .unwrap()
            .iter()
            .any(|reference| reference.name == "publication-workflow-start")
    );
}

#[tokio::test]
async fn automatic_workflow_publication_precedes_tracker_result_and_preserves_the_selected_plan() {
    for purpose in [RunPurpose::Implementation, RunPurpose::Investigation] {
        for plan in [
            PublicationPlan::NoPublication,
            PublicationPlan::PushBranch,
            PublicationPlan::CreatePullRequest,
        ] {
            for lookup_before_response in [false, true] {
                let (coordinator, paths, mut snapshot, runtime) = setup(purpose, None);
                snapshot.publication_plan = plan.clone();
                snapshot.semantic_judge_enabled = false;
                snapshot.auto_done = false;
                snapshot.allowed_operations = vec![
                    AllowedOperation::Verify,
                    AllowedOperation::PublishBranch,
                    AllowedOperation::CreatePullRequest,
                ];
                let source = coding_agent_executor::SourceInspection {
                    run_id: snapshot.run_id.to_string(),
                    attempt_id: snapshot.primary_attempt_id().into(),
                    base_commit: snapshot.base_commit.clone(),
                    head_commit: "b".repeat(40),
                    branch: snapshot.workspace_branch.clone(),
                    tracked_diff: b"diff --git a/result b/result\n+result\n".to_vec(),
                    uncommitted_diff: Some(Vec::new()),
                    new_files: Vec::new(),
                };
                *runtime.source_inspection.lock().unwrap() = Ok(Some(source.clone()));
                runtime.commit_enabled.store(true, Ordering::SeqCst);
                runtime
                    .commit_observation
                    .lock()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .source = source;
                let gate = Arc::new(Notify::new());
                let outcome = if plan == PublicationPlan::CreatePullRequest {
                    PublicationOutcome::PullRequestCreated {
                        branch: snapshot.workspace_branch.clone(),
                        head_commit: "b".repeat(40),
                        number: 81,
                        url: "https://forge.example/owner/repo/pull/81".into(),
                    }
                } else {
                    PublicationOutcome::BranchPublished {
                        branch: snapshot.workspace_branch.clone(),
                        commit: "b".repeat(40),
                        remote_head: "b".repeat(40),
                    }
                };
                let publication = Arc::new(FakePublication {
                    begins: AtomicUsize::new(0),
                    calls: AtomicUsize::new(0),
                    reconciliations: AtomicUsize::new(0),
                    outcome: Mutex::new(outcome.clone()),
                    support_override: Mutex::new(None),
                    publish_gate: Mutex::new(Some(gate.clone())),
                    admitted: AtomicBool::new(true),
                    publication_job_id: Mutex::new(Some(81)),
                });
                let coordinator =
                    coordinator.with_workflow_publication_adapter(publication.clone());
                let run = prepare_and_approve(&coordinator, snapshot).await;
                coordinator.start(&run.snapshot.run_id, None).await.unwrap();
                if plan != PublicationPlan::NoPublication {
                    wait_for_counter(&publication.calls, 1).await;
                    assert_eq!(runtime.issue_result_admissions.load(Ordering::SeqCst), 0);
                    assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 0);
                    if lookup_before_response {
                        let observer = second_coordinator(&paths, runtime.clone())
                            .with_workflow_publication_adapter(publication.clone());
                        let pending = observer.reconcile(&run.snapshot.run_id).await.unwrap();
                        assert!(matches!(
                            pending,
                            lookback_tauri_lib::coding_agent::RunReconciliation::Unknown(_)
                        ));
                        assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 0);
                        *publication.outcome.lock().unwrap() = PublicationOutcome::Unknown;
                    }
                    gate.notify_one();
                }
                wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
                assert_eq!(
                    publication.calls.load(Ordering::SeqCst),
                    usize::from(plan != PublicationPlan::NoPublication)
                );
                assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 0);
                assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 1);
                let sent: serde_json::Value = serde_json::from_slice(
                    runtime
                        .saved_result_report
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap(),
                )
                .unwrap();
                if plan != PublicationPlan::NoPublication {
                    assert_eq!(sent["publication"], serde_json::to_value(outcome).unwrap());
                }
            }
        }
    }
}

#[tokio::test]
async fn automatic_workflow_publication_stops_tracker_mutation_on_unknown_or_rejection() {
    for outcome in [PublicationOutcome::Unknown, PublicationOutcome::Rejected] {
        let (coordinator, _paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
        snapshot.publication_plan = PublicationPlan::PushBranch;
        snapshot.semantic_judge_enabled = false;
        snapshot.auto_done = false;
        snapshot.allowed_operations =
            vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
        runtime.commit_enabled.store(true, Ordering::SeqCst);
        runtime
            .commit_observation
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .source
            .head_commit = "b".repeat(40);
        let publication = Arc::new(FakePublication {
            begins: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            reconciliations: AtomicUsize::new(0),
            outcome: Mutex::new(outcome.clone()),
            support_override: Mutex::new(None),
            publish_gate: Mutex::new(None),
            admitted: AtomicBool::new(true),
            publication_job_id: Mutex::new(Some(81)),
        });
        let coordinator = coordinator.with_workflow_publication_adapter(publication.clone());
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();
        let expected = if outcome == PublicationOutcome::Rejected {
            RunState::Failed
        } else {
            RunState::Unknown
        };
        wait_for_state(&coordinator, &run.snapshot.run_id, expected).await;
        assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.issue_result_admissions.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn automatic_publication_cancel_and_pending_tracker_reopen_never_promote_completion() {
    for cancel_during_publication in [true, false] {
        let (coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
        snapshot.publication_plan = PublicationPlan::PushBranch;
        snapshot.semantic_judge_enabled = false;
        snapshot.auto_done = false;
        snapshot.allowed_operations =
            vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
        runtime.commit_enabled.store(true, Ordering::SeqCst);
        runtime
            .commit_observation
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .source
            .head_commit = "b".repeat(40);
        let gate = Arc::new(Notify::new());
        if !cancel_during_publication {
            *runtime.issue_result_gate.lock().unwrap() = Some(gate.clone());
        }
        let publication = Arc::new(FakePublication {
            begins: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            reconciliations: AtomicUsize::new(0),
            outcome: Mutex::new(PublicationOutcome::BranchPublished {
                branch: snapshot.workspace_branch.clone(),
                commit: "b".repeat(40),
                remote_head: "b".repeat(40),
            }),
            support_override: Mutex::new(None),
            publish_gate: Mutex::new(cancel_during_publication.then(|| gate.clone())),
            admitted: AtomicBool::new(true),
            publication_job_id: Mutex::new(Some(81)),
        });
        let coordinator = coordinator.with_workflow_publication_adapter(publication.clone());
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();
        if cancel_during_publication {
            wait_for_counter(&publication.calls, 1).await;
            let observer = second_coordinator(&paths, runtime.clone())
                .with_workflow_publication_adapter(publication.clone());
            observer.reconcile(&run.snapshot.run_id).await.unwrap();
            let request = read_publication_request(&coordinator, &run.snapshot.run_id);
            let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
                paths.path().join("app-data"),
            ))
            .unwrap();
            assert_eq!(
                store
                    .operation(&run.snapshot.run_id, &request.operation_key)
                    .unwrap()
                    .unwrap()
                    .status,
                DurableOperationStatus::Completed
            );
            tokio::time::timeout(
                Duration::from_secs(1),
                coordinator.cancel(&run.snapshot.run_id),
            )
            .await
            .unwrap()
            .unwrap();
            gate.notify_one();
            wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Unknown).await;
            assert_eq!(runtime.issue_result_admissions.load(Ordering::SeqCst), 0);
            assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 0);
            assert_eq!(runtime.cancels.load(Ordering::SeqCst), 1);
        } else {
            wait_for_counter(&runtime.issue_result_saves, 1).await;
            assert_eq!(
                coordinator
                    .get_run(&run.snapshot.run_id)
                    .unwrap()
                    .unwrap()
                    .state,
                RunState::Publishing
            );
            let reopened = second_coordinator(&paths, runtime.clone())
                .with_workflow_publication_adapter(publication.clone());
            reopened.recover_on_startup().unwrap();
            reopened.reconcile(&run.snapshot.run_id).await.unwrap();
            assert_ne!(
                reopened
                    .get_run(&run.snapshot.run_id)
                    .unwrap()
                    .unwrap()
                    .state,
                RunState::AwaitingReview
            );
            assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 1);
            assert_eq!(publication.begins.load(Ordering::SeqCst), 1);
            gate.notify_one();
        }
        assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn no_source_changes_can_skip_remote_publication_and_still_save_the_issue_result() {
    for purpose in [RunPurpose::Implementation, RunPurpose::Investigation] {
        for invalid_diff in [false, true] {
            let (coordinator, _paths, mut snapshot, runtime) = setup(purpose, None);
            snapshot.publication_plan = PublicationPlan::CreatePullRequest;
            snapshot.semantic_judge_enabled = false;
            snapshot.auto_done = false;
            snapshot.allowed_operations = vec![
                AllowedOperation::Verify,
                AllowedOperation::PublishBranch,
                AllowedOperation::CreatePullRequest,
            ];
            let source = coding_agent_executor::SourceInspection {
                run_id: snapshot.run_id.to_string(),
                attempt_id: snapshot.primary_attempt_id().into(),
                base_commit: snapshot.base_commit.clone(),
                head_commit: snapshot.base_commit.clone(),
                branch: snapshot.workspace_branch.clone(),
                tracked_diff: Vec::new(),
                uncommitted_diff: Some(Vec::new()),
                new_files: Vec::new(),
            };
            *runtime.source_inspection.lock().unwrap() = Ok(Some(source.clone()));
            let mut proof = source;
            if invalid_diff {
                proof.tracked_diff.push(1);
            }
            let publication = Arc::new(FakePublication {
                begins: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                reconciliations: AtomicUsize::new(0),
                outcome: Mutex::new(PublicationOutcome::NoSourceChanges { source: proof }),
                support_override: Mutex::new(None),
                publish_gate: Mutex::new(None),
                admitted: AtomicBool::new(false),
                publication_job_id: Mutex::new(None),
            });
            let coordinator = coordinator.with_workflow_publication_adapter(publication.clone());
            let run = prepare_and_approve(&coordinator, snapshot).await;
            coordinator.start(&run.snapshot.run_id, None).await.unwrap();
            wait_for_state(
                &coordinator,
                &run.snapshot.run_id,
                if invalid_diff {
                    RunState::Unknown
                } else {
                    RunState::AwaitingReview
                },
            )
            .await;
            assert_eq!(runtime.commits.load(Ordering::SeqCst), 0);
            assert_eq!(
                runtime.issue_result_saves.load(Ordering::SeqCst),
                usize::from(!invalid_diff)
            );
            assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
            if !invalid_diff {
                let sent: serde_json::Value = serde_json::from_slice(
                    runtime
                        .saved_result_report
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(sent["publication"]["outcome"], "no_source_changes");
            }
        }
    }
}

#[tokio::test]
async fn publication_unknown_is_reconciled_without_second_remote_operation() {
    let (coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.publication_plan = PublicationPlan::PushBranch;
    snapshot.allowed_operations = vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
    let publication = Arc::new(FakePublication {
        begins: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        reconciliations: AtomicUsize::new(0),
        outcome: Mutex::new(PublicationOutcome::Unknown),
        support_override: Mutex::new(None),
        publish_gate: Mutex::new(None),
        admitted: AtomicBool::new(false),
        publication_job_id: Mutex::new(Some(81)),
    });
    let coordinator = with_publication(coordinator, publication.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    let _ = wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Verifying).await;
    *publication.outcome.lock().unwrap() = PublicationOutcome::Unknown;

    let _ = coordinator.publish(&run.snapshot.run_id).await;
    *publication.outcome.lock().unwrap() = PublicationOutcome::BranchPublished {
        branch: run.snapshot.workspace_branch.clone(),
        commit: "b".repeat(40),
        remote_head: "b".repeat(40),
    };
    let restarted =
        second_coordinator(&paths, runtime.clone()).with_publication_adapter(publication.clone());
    for (branch, commit) in [
        ("main".to_owned(), "b".repeat(40)),
        (run.snapshot.workspace_branch.clone(), "c".repeat(40)),
    ] {
        *publication.outcome.lock().unwrap() = PublicationOutcome::BranchPublished {
            branch,
            commit: commit.clone(),
            remote_head: commit,
        };
        assert!(matches!(
            restarted.reconcile(&run.snapshot.run_id).await.unwrap(),
            lookback_tauri_lib::coding_agent::RunReconciliation::Unknown(_)
        ));
    }
    *publication.outcome.lock().unwrap() = PublicationOutcome::BranchPublished {
        branch: run.snapshot.workspace_branch.clone(),
        commit: "b".repeat(40),
        remote_head: "b".repeat(40),
    };
    let reconciled = restarted.reconcile(&run.snapshot.run_id).await.unwrap();

    assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
    assert_eq!(publication.reconciliations.load(Ordering::SeqCst), 3);
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        reconciled,
        lookback_tauri_lib::coding_agent::RunReconciliation::Accepted(_)
    ));
    assert_eq!(
        coordinator
            .get_run(&run.snapshot.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::AwaitingReview,
        "a bound remote receipt recovers the existing intent without redispatch"
    );
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    let request = read_publication_request(&coordinator, &run.snapshot.run_id);
    let operation = store
        .operation(&run.snapshot.run_id, &request.operation_key)
        .unwrap()
        .unwrap();
    assert_eq!(operation.status, DurableOperationStatus::Completed);
    let reopened =
        second_coordinator(&paths, runtime.clone()).with_publication_adapter(publication.clone());
    let _ = reopened.reconcile(&run.snapshot.run_id).await.unwrap();
    assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
    assert_eq!(publication.reconciliations.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn lookup_only_reconcile_does_not_invalidate_a_live_publication_permit() {
    let (coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.publication_plan = PublicationPlan::PushBranch;
    snapshot.allowed_operations = vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
    let publication = Arc::new(FakePublication {
        begins: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        reconciliations: AtomicUsize::new(0),
        outcome: Mutex::new(PublicationOutcome::Unknown),
        support_override: Mutex::new(None),
        publish_gate: Mutex::new(None),
        admitted: AtomicBool::new(false),
        publication_job_id: Mutex::new(Some(81)),
    });
    let coordinator = with_publication(coordinator, publication.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Verifying).await;
    let gate = Arc::new(Notify::new());
    *publication.publish_gate.lock().unwrap() = Some(gate.clone());
    let publish = {
        let coordinator = coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        tokio::spawn(async move { coordinator.publish(&run_id).await })
    };
    wait_for_counter(&publication.calls, 1).await;

    let observer =
        second_coordinator(&paths, runtime).with_publication_adapter(publication.clone());
    let observation = observer.reconcile(&run.snapshot.run_id).await.unwrap();
    let observed_run = match observation {
        lookback_tauri_lib::coding_agent::RunReconciliation::Unknown(run) => run,
        other => panic!("pending publication must remain unresolved: {other:?}"),
    };
    assert_eq!(observed_run.state, RunState::Publishing);

    *publication.outcome.lock().unwrap() = PublicationOutcome::BranchPublished {
        branch: run.snapshot.workspace_branch.clone(),
        commit: "b".repeat(40),
        remote_head: "b".repeat(40),
    };
    gate.notify_one();
    let completed = publish.await.unwrap().unwrap();

    assert_eq!(completed.state, RunState::AwaitingReview);
    assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
    assert_eq!(publication.reconciliations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn confirmed_lookup_wins_over_a_late_unknown_publication_response() {
    let (coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
    snapshot.publication_plan = PublicationPlan::PushBranch;
    snapshot.allowed_operations = vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
    let publication = Arc::new(FakePublication {
        begins: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        reconciliations: AtomicUsize::new(0),
        outcome: Mutex::new(PublicationOutcome::Unknown),
        support_override: Mutex::new(None),
        publish_gate: Mutex::new(None),
        admitted: AtomicBool::new(false),
        publication_job_id: Mutex::new(Some(81)),
    });
    let coordinator = with_publication(coordinator, publication.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Verifying).await;
    let gate = Arc::new(Notify::new());
    *publication.publish_gate.lock().unwrap() = Some(gate.clone());
    let publish = {
        let coordinator = coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        tokio::spawn(async move { coordinator.publish(&run_id).await })
    };
    wait_for_counter(&publication.calls, 1).await;

    let observer =
        second_coordinator(&paths, runtime).with_publication_adapter(publication.clone());
    let observation = observer.reconcile(&run.snapshot.run_id).await.unwrap();
    let observed_run = match observation {
        lookback_tauri_lib::coding_agent::RunReconciliation::Unknown(run) => run,
        other => panic!("pending publication must remain unresolved: {other:?}"),
    };
    assert_eq!(observed_run.state, RunState::Publishing);

    *publication.outcome.lock().unwrap() = PublicationOutcome::BranchPublished {
        branch: run.snapshot.workspace_branch.clone(),
        commit: "b".repeat(40),
        remote_head: "b".repeat(40),
    };
    let confirmed = observer.reconcile(&run.snapshot.run_id).await.unwrap();
    assert!(matches!(
        confirmed,
        lookback_tauri_lib::coding_agent::RunReconciliation::Accepted(_)
    ));
    *publication.outcome.lock().unwrap() = PublicationOutcome::Unknown;
    gate.notify_one();
    let completed = publish.await.unwrap().unwrap();

    assert_eq!(completed.state, RunState::AwaitingReview);
    assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
    assert_eq!(publication.reconciliations.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn no_changes_uses_verified_frozen_commit_and_routes_to_review_without_pr() {
    let (coordinator, paths, mut snapshot, _runtime) = setup(RunPurpose::Implementation, None);
    snapshot.publication_plan = PublicationPlan::CreatePullRequest;
    snapshot.allowed_operations = vec![
        AllowedOperation::Verify,
        AllowedOperation::PublishBranch,
        AllowedOperation::CreatePullRequest,
    ];
    let frozen_commit = "b".repeat(40);
    let publication = Arc::new(FakePublication {
        begins: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        reconciliations: AtomicUsize::new(0),
        outcome: Mutex::new(PublicationOutcome::NoChangesWithTreeProof {
            base_commit: "1".repeat(40),
            frozen_commit,
            base_tree_id: "c".repeat(40),
            frozen_tree_id: "c".repeat(40),
        }),
        support_override: Mutex::new(None),
        publish_gate: Mutex::new(None),
        admitted: AtomicBool::new(false),
        publication_job_id: Mutex::new(Some(81)),
    });
    let coordinator = with_publication(coordinator, publication.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Verifying).await;

    let review = coordinator.publish(&run.snapshot.run_id).await.unwrap();

    assert_eq!(review.state, RunState::AwaitingReview);
    assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
    assert_eq!(publication.reconciliations.load(Ordering::SeqCst), 0);
    let references = coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap();
    let receipt = references
        .iter()
        .find(|reference| reference.name == "publication-receipt")
        .unwrap();
    let receipt: PublicationOutcome = serde_json::from_slice(
        &coordinator
            .read_evidence(&run.snapshot.run_id, &receipt.evidence_id)
            .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        receipt,
        PublicationOutcome::NoChangesWithTreeProof { .. }
    ));
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    let request = read_publication_request(&coordinator, &run.snapshot.run_id);
    let operation = store
        .operation(&run.snapshot.run_id, &request.operation_key)
        .unwrap()
        .unwrap();
    assert_eq!(operation.status, DurableOperationStatus::Completed);
    assert_eq!(
        operation.result.unwrap().outcome,
        DurableOperationOutcome::Succeeded
    );
}

#[tokio::test]
async fn no_changes_with_different_tree_ids_stays_unknown() {
    let (coordinator, paths, mut snapshot, _runtime) = setup(RunPurpose::Implementation, None);
    snapshot.publication_plan = PublicationPlan::PushBranch;
    snapshot.allowed_operations = vec![AllowedOperation::Verify, AllowedOperation::PublishBranch];
    let publication = Arc::new(FakePublication {
        begins: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        reconciliations: AtomicUsize::new(0),
        outcome: Mutex::new(PublicationOutcome::NoChangesWithTreeProof {
            base_commit: "1".repeat(40),
            frozen_commit: "b".repeat(40),
            base_tree_id: "c".repeat(40),
            frozen_tree_id: "d".repeat(40),
        }),
        support_override: Mutex::new(None),
        publish_gate: Mutex::new(None),
        admitted: AtomicBool::new(false),
        publication_job_id: Mutex::new(Some(81)),
    });
    let coordinator = with_publication(coordinator, publication.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Verifying).await;

    let unresolved = coordinator.publish(&run.snapshot.run_id).await.unwrap();

    assert_eq!(unresolved.state, RunState::Unknown);
    assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
    assert!(
        !coordinator
            .evidence_references(&run.snapshot.run_id)
            .unwrap()
            .iter()
            .any(|reference| reference.name == "publication-receipt")
    );
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    let request = read_publication_request(&coordinator, &run.snapshot.run_id);
    let operation = store
        .operation(&run.snapshot.run_id, &request.operation_key)
        .unwrap()
        .unwrap();
    assert_eq!(operation.status, DurableOperationStatus::ResultUnconfirmed);
}

#[tokio::test]
async fn branch_and_pull_request_publication_use_their_sealed_effect_claim() {
    for (purpose, plan) in [
        (RunPurpose::Implementation, PublicationPlan::PushBranch),
        (
            RunPurpose::Implementation,
            PublicationPlan::CreatePullRequest,
        ),
        (RunPurpose::Investigation, PublicationPlan::PushBranch),
        (
            RunPurpose::Investigation,
            PublicationPlan::CreatePullRequest,
        ),
    ] {
        let (coordinator, paths, mut snapshot, _runtime) = setup(purpose, None);
        snapshot.publication_plan = plan.clone();
        snapshot.allowed_operations = match &plan {
            PublicationPlan::PushBranch => {
                vec![AllowedOperation::Verify, AllowedOperation::PublishBranch]
            }
            PublicationPlan::CreatePullRequest => vec![
                AllowedOperation::Verify,
                AllowedOperation::PublishBranch,
                AllowedOperation::CreatePullRequest,
            ],
            PublicationPlan::NoPublication => unreachable!(),
        };
        let outcome = match &plan {
            PublicationPlan::PushBranch => PublicationOutcome::BranchPublished {
                branch: snapshot.workspace_branch.clone(),
                commit: "b".repeat(40),
                remote_head: "b".repeat(40),
            },
            PublicationPlan::CreatePullRequest => PublicationOutcome::PullRequestCreated {
                branch: snapshot.workspace_branch.clone(),
                head_commit: "b".repeat(40),
                number: 42,
                url: "https://forge.example/lookback/agent-app/pulls/42".into(),
            },
            PublicationPlan::NoPublication => unreachable!(),
        };
        let publication = Arc::new(FakePublication {
            begins: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            reconciliations: AtomicUsize::new(0),
            outcome: Mutex::new(outcome),
            support_override: Mutex::new(None),
            publish_gate: Mutex::new(None),
            admitted: AtomicBool::new(false),
            publication_job_id: Mutex::new(Some(81)),
        });
        let coordinator = with_publication(coordinator, publication.clone());
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Verifying).await;

        let review = coordinator.publish(&run.snapshot.run_id).await.unwrap();

        assert_eq!(review.state, RunState::AwaitingReview);
        assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
        assert_eq!(publication.reconciliations.load(Ordering::SeqCst), 0);
        let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
            paths.path().join("app-data"),
        ))
        .unwrap();
        let request = read_publication_request(&coordinator, &run.snapshot.run_id);
        let operation = store
            .operation(&run.snapshot.run_id, &request.operation_key)
            .unwrap()
            .unwrap();
        assert_eq!(operation.status, DurableOperationStatus::Completed);
        let result = operation.result.as_ref().unwrap();
        assert_eq!(result.outcome, DurableOperationOutcome::Succeeded);
        assert_eq!(result.evidence_ids.len(), 1);
        assert!(matches!(
            operation.request,
            DurableOperationRequest::PublishBranch { .. }
                | DurableOperationRequest::CreatePullRequest { .. }
        ));
    }
}

fn setup(
    purpose: RunPurpose,
    config: Option<CoordinatorConfig>,
) -> (
    CodingAgentCoordinator,
    tempfile::TempDir,
    AgentRunSnapshot,
    Arc<FakeExecution>,
) {
    setup_with_events(
        purpose,
        vec![
            Ok(ExecutionEvent::Output {
                stream: "stdout".into(),
                data: b"analysis\0".to_vec(),
            }),
            Ok(ExecutionEvent::Exit { exit_code: 0 }),
        ],
    )
    .map_config(config)
}

trait SetupConfig {
    fn map_config(
        self,
        config: Option<CoordinatorConfig>,
    ) -> (
        CodingAgentCoordinator,
        tempfile::TempDir,
        AgentRunSnapshot,
        Arc<FakeExecution>,
    );
}

impl SetupConfig
    for (
        CodingAgentCoordinator,
        tempfile::TempDir,
        AgentRunSnapshot,
        Arc<FakeExecution>,
    )
{
    fn map_config(
        self,
        config: Option<CoordinatorConfig>,
    ) -> (
        CodingAgentCoordinator,
        tempfile::TempDir,
        AgentRunSnapshot,
        Arc<FakeExecution>,
    ) {
        let (coordinator, paths, snapshot, runtime) = self;
        (
            coordinator.with_config(config.unwrap_or_default()),
            paths,
            snapshot,
            runtime,
        )
    }
}

fn setup_with_events(
    purpose: RunPurpose,
    events: Vec<Result<ExecutionEvent, ExecutionResult>>,
) -> (
    CodingAgentCoordinator,
    tempfile::TempDir,
    AgentRunSnapshot,
    Arc<FakeExecution>,
) {
    let paths = tempfile::tempdir().unwrap();
    let store = Arc::new(
        lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
            paths.path().join("app-data"),
        ))
        .unwrap(),
    );
    let snapshot = snapshot(purpose);
    let runtime = Arc::new(FakeExecution::with_output(&snapshot, events));
    let coordinator = CodingAgentCoordinator::new(store, Some(runtime.clone()), None);
    (coordinator, paths, snapshot, runtime)
}

fn second_coordinator(
    paths: &tempfile::TempDir,
    runtime: Arc<FakeExecution>,
) -> CodingAgentCoordinator {
    let store = Arc::new(
        lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
            paths.path().join("app-data"),
        ))
        .unwrap(),
    );
    CodingAgentCoordinator::new(store, Some(runtime), None)
}

fn with_publication(
    coordinator: CodingAgentCoordinator,
    publication: Arc<FakePublication>,
) -> CodingAgentCoordinator {
    coordinator.with_publication_adapter(publication)
}

fn read_publication_request(
    coordinator: &CodingAgentCoordinator,
    run_id: &RunId,
) -> PublicationRequest {
    let reference = coordinator
        .evidence_references(run_id)
        .unwrap()
        .into_iter()
        .find(|reference| reference.name == "publication-intent")
        .unwrap();
    serde_json::from_slice(
        &coordinator
            .read_evidence(run_id, &reference.evidence_id)
            .unwrap(),
    )
    .unwrap()
}

async fn prepare_and_approve(
    coordinator: &CodingAgentCoordinator,
    snapshot: AgentRunSnapshot,
) -> AgentRunRecord {
    let contract = approval_assessment_contract(snapshot.purpose, false);
    prepare_and_approve_with_contract(coordinator, snapshot, contract).await
}

async fn prepare_and_approve_with_required_validation(
    coordinator: &CodingAgentCoordinator,
    snapshot: AgentRunSnapshot,
) -> AgentRunRecord {
    let contract = approval_assessment_contract(snapshot.purpose, true);
    prepare_and_approve_with_contract(coordinator, snapshot, contract).await
}

async fn prepare_and_approve_with_contract(
    coordinator: &CodingAgentCoordinator,
    snapshot: AgentRunSnapshot,
    contract: ApprovalAssessmentContract,
) -> AgentRunRecord {
    let PreparationOutcome::Ready(run) = coordinator
        .prepare_run_with_assessment_contract(snapshot, contract)
        .await
        .unwrap()
    else {
        panic!("verified test runtime should pass preflight")
    };
    coordinator.approve_run(&run.snapshot.run_id).unwrap()
}

fn approval_assessment_contract(
    purpose: RunPurpose,
    validation_required: bool,
) -> ApprovalAssessmentContract {
    ApprovalAssessmentContract::from_issue_acceptance(
        purpose,
        vec!["The expected behavior is present.".into()],
        vec![ApprovalCriterionPolicy {
            required: true,
            method: CriterionEvaluationMethod::SemanticJudge,
        }],
        vec![ApprovalValidationDraft {
            required: validation_required,
            command: None,
        }],
        Some(ProviderSelection {
            provider_id: "test-provider".into(),
            model_id: "test-model".into(),
            model_version: "v1".into(),
            config_version: "config-v1".into(),
        }),
        Some("coding-agent-judge-v1".into()),
        AssessmentInputScope {
            allowed_fields: vec![ApprovedJudgeInputField::Report],
            allowed_sources: vec![AllowedEvidenceSource {
                kind: EvidenceSourceKind::Report,
                identity: "report:attempt-1".into(),
                max_bytes: 32 * 1024,
            }],
            max_input_bytes: 32 * 1024,
        },
    )
    .unwrap()
}

async fn wait_for_state(
    coordinator: &CodingAgentCoordinator,
    run_id: &RunId,
    expected: RunState,
) -> AgentRunRecord {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let run = coordinator.get_run(run_id).unwrap().unwrap();
            if run.state == expected {
                return run;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "expected {expected:?}; actual {:?}",
            coordinator.get_run(run_id).unwrap().unwrap()
        )
    })
}

async fn wait_for_counter(counter: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while counter.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn assert_finish_operation_status(
    paths: &tempfile::TempDir,
    run_id: &RunId,
    expected: DurableOperationStatus,
) {
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    let operation = store
        .operation(run_id, "finish:attempt-1")
        .unwrap()
        .unwrap();
    assert_eq!(operation.status, expected);
    match operation.status {
        DurableOperationStatus::ResultUnconfirmed => assert_eq!(
            operation.result.unwrap().outcome,
            DurableOperationOutcome::ResultUnconfirmed
        ),
        DurableOperationStatus::Completed => assert_eq!(
            operation.result.unwrap().outcome,
            DurableOperationOutcome::Succeeded
        ),
        DurableOperationStatus::Intent => panic!("expected durable finish result"),
    }
}

fn snapshot(purpose: RunPurpose) -> AgentRunSnapshot {
    let repository = RepositoryIdentity::new("forge.example", "lookback", "agent-app").unwrap();
    let image_digest = format!("sha256:{}", "a".repeat(64));
    AgentRunSnapshot {
        run_id: RunId::new("run-test-1").unwrap(),
        issue_id: "issue-17".into(),
        issue_revision: 3,
        execution_mode: ExecutionMode::OneShot,
        purpose,
        repository: ResolvedRepository {
            identity: repository.clone(),
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
            image_ref: format!("registry.example/coder@{image_digest}"),
            image_digest,
            command: vec!["opencode".into(), "run".into()],
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
            allowed_repositories: vec![repository],
            credential_reference: None,
        },
        base_commit: "1".repeat(40),
        workspace_branch: "agent/run-test-1".into(),
        semantic_judge_enabled: true,
        auto_done: false,
        approval: Some(lookback_tauri_lib::coding_agent::RunApproval {
            approved_by: "reviewer".into(),
            approved_at_ms: 1,
            auto_done_approved: false,
            settings_version: Some(1),
            applicable_modes: vec![ExecutionMode::OneShot],
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
        attempt_ids: vec!["attempt-1".into(), "attempt-2".into()],
    }
}

fn identity(run_id: &str, attempt_id: &str) -> ExecutionIdentity {
    ExecutionIdentity {
        run_id: run_id.into(),
        attempt_id: attempt_id.into(),
        workflow_execution_id: "workflow-41".into(),
        parent_job_id: "parent-41".into(),
        child_sandbox_job_id: "child-41".into(),
        worker_id: 41,
        sandbox_job_id: "sandbox-41".into(),
    }
}

fn workspace_receipt(request: &WorkspaceRequest) -> WorkspaceReceipt {
    let repository = &request.snapshot.repository.identity;
    WorkspaceReceipt {
        run_id: request.snapshot.run_id.to_string(),
        attempt_id: request.attempt_id.clone(),
        workspace_token: format!("workspace-{}", request.attempt_id),
        repository_host: repository.host.clone(),
        repository_owner: repository.organization.clone(),
        repository_name: repository.repository.clone(),
        base_commit: request.snapshot.base_commit.clone(),
        branch: request.snapshot.workspace_branch.clone(),
    }
}

fn start_receipt(intent: &ImmutableExecutionIntent) -> StartReceipt {
    let bytes = serde_json::to_vec(intent).unwrap();
    StartReceipt {
        identity: identity(&intent.run_id, &intent.attempt_id),
        execution_intent_sha256: format!("{:x}", Sha256::digest(bytes)),
        method_name: intent.method_name.clone(),
        image_digest: intent.image_digest.clone(),
        prompt_delivery: lookback_tauri_lib::coding_agent::PromptDelivery::Accepted,
    }
}

fn runtime_observation(snapshot: &AgentRunSnapshot) -> RuntimeObservation {
    // Synthetic typed input for coordinator decisions only; this fake is not a
    // live installed-runtime observation.
    RuntimeObservation {
        runtime_instance_id: "runtime-local-1".into(),
        health_observation_id: "health-1".into(),
        worker_name: "agent-sandbox".into(),
        worker_id: 41,
        runner_name: match snapshot.execution_mode {
            ExecutionMode::OneShot => "WORKFLOW".into(),
            ExecutionMode::Interactive => "SANDBOX".into(),
        },
        method_name: match snapshot.execution_mode {
            ExecutionMode::OneShot => "run".into(),
            ExecutionMode::Interactive => "run_with_client".into(),
        },
        arguments_schema_sha256: "a".repeat(64),
        result_schema_sha256: "b".repeat(64),
        response_mode: "DIRECT".into(),
        output_is_streaming: true,
        accepts_client_stream: snapshot.execution_mode == ExecutionMode::Interactive,
        image_digest: snapshot.profile.image_digest.clone(),
        admitted_resources: snapshot.profile.resources,
        host_limits: snapshot.profile.validated_host_limits,
        helper_version: "1.18.3".into(),
        agent_cli_version: match snapshot.execution_mode {
            ExecutionMode::OneShot => "2.0.24".into(),
            ExecutionMode::Interactive => "1.18.3".into(),
        },
        helper_protocol_version: 1,
        helper_features: BTreeSet::from([
            "prepare".into(),
            "frozen_commit_import".into(),
            "host_process_observation".into(),
            "host_writer_quiescence".into(),
            "host_workspace_freeze".into(),
            "binary_output_capture".into(),
            "protected_remote_auth_forwarding_verified".into(),
            "startup_prompt_flag_verified".into(),
            "interactive_prompt_adapter".into(),
            "interactive_finish".into(),
        ]),
        workflow_execution_id: snapshot
            .workflow
            .workflow_execution_id
            .clone()
            .unwrap_or_default(),
        parent_job_id: snapshot.workflow.parent_job_id.clone().unwrap_or_default(),
        child_sandbox_job_id: snapshot
            .workflow
            .child_sandbox_job_id
            .clone()
            .unwrap_or_default(),
    }
}

fn successful_exit() -> lookback_tauri_lib::coding_agent::ProcessExitObservation {
    lookback_tauri_lib::coding_agent::ProcessExitObservation::ExitedNormally { exit_code: 0 }
}

fn interactive_finish_observation(
    cli_exit: ProcessExitObservation,
    shell_observation: ShellProcessObservation,
    child_writers: ChildWritersStatus,
    workspace_freeze: WorkspaceFreezeStatus,
) -> InteractiveFinishObservation {
    InteractiveFinishObservation {
        invocation_kind: ExecutionInvocationKind::ShellWrapper,
        cli_exit,
        shell_observation,
        child_writers,
        workspace_freeze,
        report: Some(b"interactive report".to_vec()),
        validation: ValidationEvidence {
            required: false,
            state: ValidationState::NotRun,
        },
        acceptance_conditions_met: false,
    }
}

fn incomplete_interactive_finish() -> InteractiveFinishObservation {
    interactive_finish_observation(
        successful_exit(),
        ShellProcessObservation::Spawned {
            exit: successful_exit(),
        },
        ChildWritersStatus::Active,
        WorkspaceFreezeStatus::Unknown,
    )
}

fn successful_interactive_finish() -> InteractiveFinishObservation {
    interactive_finish_observation(
        successful_exit(),
        ShellProcessObservation::Spawned {
            exit: successful_exit(),
        },
        ChildWritersStatus::Stopped,
        WorkspaceFreezeStatus::HostVerified {
            commit_id: "b".repeat(40),
        },
    )
}

fn successful_finalization() -> HostFinalizationObservation {
    HostFinalizationObservation {
        invocation_kind: ExecutionInvocationKind::ShellWrapper,
        cli_exit: successful_exit(),
        shell_observation: ShellProcessObservation::Spawned {
            exit: successful_exit(),
        },
        child_writers: ChildWritersStatus::Stopped,
        workspace_freeze: WorkspaceFreezeStatus::HostVerified {
            commit_id: "b".repeat(40),
        },
        report: Some(b"Investigation report".to_vec()),
        validation: ValidationEvidence {
            required: false,
            state: ValidationState::NotRun,
        },
        acceptance_conditions_met: false,
    }
}

fn schema_report_for_fixture(
    report: &[u8],
    identity: &ExecutionIdentity,
    purpose: RunPurpose,
) -> Vec<u8> {
    if String::from_utf8_lossy(report).trim().is_empty() {
        return report.to_vec();
    }
    let summary = String::from_utf8_lossy(report).into_owned();
    test_report_bytes(&identity.run_id, &identity.attempt_id, purpose, &summary)
}

fn test_report_bytes(
    run_id: &str,
    attempt_id: &str,
    purpose: RunPurpose,
    summary: &str,
) -> Vec<u8> {
    let (task_kind, investigation, implementation) = match purpose {
        RunPurpose::Investigation => (
            "investigation",
            serde_json::json!({
                "scope": [],
                "findings": [],
                "limitations": [],
                "response_questions": []
            }),
            serde_json::Value::Null,
        ),
        RunPurpose::Implementation => (
            "implementation",
            serde_json::Value::Null,
            serde_json::json!({
                "change_summary": summary,
                "changed_files": [],
                "implementation_notes": []
            }),
        ),
    };
    let acceptance = serde_json::json!([{
        "criterion_id": "AC-1",
        "assessment": "unknown",
        "evidence": ["report:attempt-1"]
    }]);
    let validation = serde_json::json!([{
        "validation_id": "V-1",
        "claimed_result": "not_run",
        "evidence": [],
        "reason": "No independently verified validation receipt was supplied."
    }]);
    serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "run_id": run_id,
        "attempt_id": attempt_id,
        "task_kind": task_kind,
        "status": "completed",
        "summary": summary,
        "acceptance": acceptance,
        "validation": validation,
        "investigation": investigation,
        "implementation": implementation,
        "unresolved": []
    }))
    .unwrap()
}

#[test]
fn approval_cannot_switch_to_a_reprepared_snapshot_after_the_preview_digest_check() {
    use lookback_tauri_lib::coding_agent::native_execution::{
        prepared_snapshot_digest, verify_prepared_approval,
    };
    let approved = snapshot(RunPurpose::Implementation);
    let digest = prepared_snapshot_digest("plan-key", &approved).unwrap();
    assert!(verify_prepared_approval("plan-key", &approved, &digest).is_ok());
    let changes: [fn(&mut AgentRunSnapshot); 6] = [
        |s| s.workflow.semantic_judge_worker_digest = Some("f".repeat(64)),
        |s| s.base_commit = "f".repeat(40),
        |s| s.workspace_branch = "agent/issue-other-attempt".into(),
        |s| s.profile.image_digest = format!("sha256:{}", "f".repeat(64)),
        |s| s.workflow.registered_worker_id = Some("different-workflow".into()),
        |s| s.run_id = RunId::new("different-run").unwrap(),
    ];
    for change in changes {
        let mut replaced = approved.clone();
        change(&mut replaced);
        assert_eq!(
            verify_prepared_approval("plan-key", &replaced, &digest),
            Err(ExecutionResult::Conflict)
        );
    }
    assert_eq!(
        verify_prepared_approval("different-key", &approved, &digest),
        Err(ExecutionResult::Conflict)
    );
}

#[tokio::test]
async fn local_commit_waits_for_durable_valid_report_and_failed_execution_never_commits() {
    for (exit_code, report_present, expected_commits) in [(0, true, 1), (0, false, 0), (7, true, 0)]
    {
        let (coordinator, _paths, snapshot, runtime) = setup_with_events(
            RunPurpose::Investigation,
            vec![Ok(ExecutionEvent::Exit { exit_code })],
        );
        runtime.commit_enabled.store(true, Ordering::SeqCst);
        let mut observation = successful_finalization();
        if !report_present {
            observation.report = None;
        }
        runtime.set_finalization(observation);
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();
        wait_for_state(
            &coordinator,
            &run.snapshot.run_id,
            if expected_commits == 1 {
                RunState::AwaitingReview
            } else {
                RunState::Failed
            },
        )
        .await;
        assert_eq!(runtime.commits.load(Ordering::SeqCst), expected_commits);
        if expected_commits == 0 {
            assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn result_save_is_confirmed_before_review_and_unconfirmed_save_is_not_repeated() {
    for failure in [false, true] {
        let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
        runtime
            .issue_result_failure
            .store(failure, Ordering::SeqCst);
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();
        wait_for_state(
            &coordinator,
            &run.snapshot.run_id,
            if failure {
                RunState::Unknown
            } else {
                RunState::AwaitingReview
            },
        )
        .await;
        assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 1);
        coordinator
            .resume_finalization(&run.snapshot.run_id)
            .await
            .unwrap();
        assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 1);
        let evidence = coordinator
            .evidence_references(&run.snapshot.run_id)
            .unwrap();
        assert!(
            evidence
                .iter()
                .any(|reference| reference.name == "issue-result-intent")
        );
        assert_eq!(
            evidence
                .iter()
                .any(|reference| reference.name == "issue-result-ack"),
            !failure
        );
    }
}

#[tokio::test]
async fn reopening_during_result_save_cannot_skip_or_repeat_the_tracker_mutation() {
    let (coordinator, paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    let gate = Arc::new(Notify::new());
    *runtime.issue_result_gate.lock().unwrap() = Some(gate.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.issue_result_saves.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let resumed = coordinator
        .resume_finalization(&run.snapshot.run_id)
        .await
        .unwrap();
    assert_eq!(resumed.state, RunState::Verifying);
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    let reopened = CodingAgentCoordinator::new(Arc::new(store), Some(runtime.clone()), None);
    reopened.recover_on_startup().unwrap();
    let resumed = reopened
        .resume_finalization(&run.snapshot.run_id)
        .await
        .unwrap();
    assert_ne!(resumed.state, RunState::AwaitingReview);
    assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 1);
    gate.notify_one();
}

#[tokio::test]
async fn cancellation_during_result_save_does_not_wait_for_the_tracker_response() {
    let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    let gate = Arc::new(Notify::new());
    *runtime.issue_result_gate.lock().unwrap() = Some(gate.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.issue_result_saves.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let cancelled = tokio::time::timeout(
        Duration::from_millis(500),
        coordinator.cancel(&run.snapshot.run_id),
    )
    .await;
    gate.notify_one();
    assert_eq!(cancelled.unwrap().unwrap().state, RunState::Unknown);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !coordinator
            .evidence_references(&run.snapshot.run_id)
            .unwrap()
            .iter()
            .any(|reference| reference.name == "issue-result-ack")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let resumed = coordinator
        .resume_finalization(&run.snapshot.run_id)
        .await
        .unwrap();
    assert_eq!(resumed.state, RunState::Unknown);
    assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invalid_or_oversized_tracker_ack_cannot_confirm_the_result() {
    use lookback_tauri_lib::coding_agent::issue_gateway::MAX_ISSUE_RESULT_ACK_BYTES;
    let mut boundary = b"{}".to_vec();
    boundary.resize(MAX_ISSUE_RESULT_ACK_BYTES, b' ');
    let mut oversized = boundary.clone();
    oversized.push(b' ');
    let wrapper_too_large = serde_json::to_vec(
        &serde_json::json!({"receipt":"x".repeat(MAX_ISSUE_RESULT_ACK_BYTES-20)}),
    )
    .unwrap();
    assert!(wrapper_too_large.len() <= MAX_ISSUE_RESULT_ACK_BYTES);
    for (ack, confirmed) in [
        (b"invalid".to_vec(), false),
        (vec![0xff], false),
        (boundary, true),
        (oversized, false),
        (wrapper_too_large, false),
    ] {
        let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
        *runtime.issue_result_ack.lock().unwrap() = Some(ack);
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();
        wait_for_state(
            &coordinator,
            &run.snapshot.run_id,
            if confirmed {
                RunState::AwaitingReview
            } else {
                RunState::Unknown
            },
        )
        .await;
        assert_eq!(
            coordinator
                .evidence_references(&run.snapshot.run_id)
                .unwrap()
                .iter()
                .any(|reference| reference.name == "issue-result-ack"),
            confirmed
        );
        coordinator
            .resume_finalization(&run.snapshot.run_id)
            .await
            .unwrap();
        assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn rejected_result_admission_never_sends_or_retries_the_tracker_mutation() {
    let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    runtime
        .issue_result_admission_failure
        .store(true, Ordering::SeqCst);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Unknown).await;
    coordinator
        .resume_finalization(&run.snapshot.run_id)
        .await
        .unwrap();
    assert_eq!(runtime.issue_result_admissions.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 0);
    assert!(
        !coordinator
            .evidence_references(&run.snapshot.run_id)
            .unwrap()
            .iter()
            .any(|reference| reference.name == "issue-result-ack")
    );
}

#[tokio::test]
async fn cancellation_is_not_blocked_by_waiting_for_a_commit_workflow_result() {
    let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    runtime.commit_enabled.store(true, Ordering::SeqCst);
    let gate = Arc::new(Notify::new());
    *runtime.commit_gate.lock().unwrap() = Some(gate.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.commits.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let cancelled = tokio::time::timeout(
        Duration::from_millis(500),
        coordinator.cancel(&run.snapshot.run_id),
    )
    .await;
    gate.notify_one();
    assert!(
        cancelled.is_ok(),
        "cancel must not wait for commit result drain"
    );
}

#[tokio::test]
async fn resume_and_reopen_cannot_complete_an_unconfirmed_commit_phase() {
    let (coordinator, paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    runtime.commit_enabled.store(true, Ordering::SeqCst);
    let gate = Arc::new(Notify::new());
    *runtime.commit_gate.lock().unwrap() = Some(gate.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.commits.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let resumed = coordinator
        .resume_finalization(&run.snapshot.run_id)
        .await
        .unwrap();
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    let reopened = CodingAgentCoordinator::new(Arc::new(store), Some(runtime.clone()), None);
    reopened.recover_on_startup().unwrap();
    let after_restart = reopened
        .resume_finalization(&run.snapshot.run_id)
        .await
        .unwrap();
    gate.notify_one();
    assert_ne!(resumed.state, RunState::AwaitingReview);
    assert_ne!(after_restart.state, RunState::AwaitingReview);
    assert_eq!(runtime.commits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_known_commit_result_is_durable_even_when_final_source_inspection_is_unavailable() {
    let (coordinator, paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    runtime.commit_enabled.store(true, Ordering::SeqCst);
    let metadata = serde_json::to_vec(
        &serde_json::json!({"exit_code":0,"execution_time_ms":42,"sandbox_id":"actual-commit-vm",
        "workflow_execution_id":"actual-commit-execution","parent_job_id":12}),
    )
    .unwrap();
    *runtime.commit_metadata.lock().unwrap() = Some(metadata.clone());
    *runtime.commit_failure.lock().unwrap() = Some(ExecutionResult::FinalizationUnavailable);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Unknown).await;
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    let references = store.evidence(&run.snapshot.run_id).unwrap();
    let reference = references
        .iter()
        .find(|reference| reference.name == "local-commit-workflow-result")
        .expect("actual completed commit metadata must survive source failure");
    assert_eq!(
        store
            .read_evidence(&run.snapshot.run_id, &reference.evidence_id)
            .unwrap(),
        metadata
    );
    assert_eq!(
        coordinator
            .resume_finalization(&run.snapshot.run_id)
            .await
            .unwrap()
            .state,
        RunState::Unknown
    );
    assert_eq!(runtime.commits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn accepted_commit_without_a_final_result_is_unknown_and_cannot_be_skipped() {
    let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    runtime.commit_enabled.store(true, Ordering::SeqCst);
    *runtime.commit_observation.lock().unwrap() = None;
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Unknown).await;
    assert_eq!(runtime.commits.load(Ordering::SeqCst), 1);
    assert_eq!(
        coordinator
            .resume_finalization(&run.snapshot.run_id)
            .await
            .unwrap()
            .state,
        RunState::Unknown
    );
}

#[tokio::test]
async fn crash_before_commit_intent_keeps_the_required_phase_unconfirmed_on_reopen() {
    let (coordinator, paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    runtime.commit_enabled.store(true, Ordering::SeqCst);
    let gate = Arc::new(Notify::new());
    *runtime.commit_gate.lock().unwrap() = Some(gate.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.commits.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let data = DataPaths::with_root(paths.path().join("app-data"));
    let store = Arc::new(lookback_tauri_lib::coding_agent::RunStore::open(&data).unwrap());
    assert!(
        store
            .evidence(&run.snapshot.run_id)
            .unwrap()
            .iter()
            .any(|reference| reference.name == "local-commit-required" && reference.save_confirmed)
    );
    // Reconstruct the durable checkpoint before later commit intent/start writes.
    rusqlite::Connection::open(data.coding_agent_runs_db_path()).unwrap().execute(
        "DELETE FROM agent_run_evidence WHERE run_id=?1 AND name IN ('local-commit-intent','local-commit-start')",[run.snapshot.run_id.as_str()]).unwrap();
    let reopened = CodingAgentCoordinator::new(store, Some(runtime.clone()), None);
    reopened.recover_on_startup().unwrap();
    let resumed = reopened
        .resume_finalization(&run.snapshot.run_id)
        .await
        .unwrap();
    gate.notify_one();
    assert_ne!(resumed.state, RunState::AwaitingReview);
    assert_eq!(runtime.commits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn successful_commit_with_remaining_source_changes_is_not_confirmed() {
    for missing_diff in [false, true] {
        let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
        runtime.commit_enabled.store(true, Ordering::SeqCst);
        runtime
            .commit_observation
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .source
            .uncommitted_diff = if missing_diff {
            None
        } else {
            Some(b"remaining modification".to_vec())
        };
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Unknown).await;
        assert_eq!(runtime.commits.load(Ordering::SeqCst), 1);
        assert_eq!(
            coordinator
                .resume_finalization(&run.snapshot.run_id)
                .await
                .unwrap()
                .state,
            RunState::Unknown
        );
    }
}

#[tokio::test]
async fn skipping_commit_cannot_save_a_source_observation_from_before_a_head_change() {
    for initially_dirty in [false, true] {
        let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
        let mut before = runtime
            .commit_observation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .source
            .clone();
        if initially_dirty {
            before.uncommitted_diff = Some(b"pending change".to_vec());
        }
        let mut after = before.clone();
        after.head_commit = "a".repeat(40);
        after.uncommitted_diff = Some(Vec::new());
        *runtime.source_inspection.lock().unwrap() = Ok(Some(before));
        *runtime.source_after_skip.lock().unwrap() = Some(after);
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Unknown).await;
        assert_eq!(runtime.commits.load(Ordering::SeqCst), 0);
        assert_eq!(
            coordinator
                .resume_finalization(&run.snapshot.run_id)
                .await
                .unwrap()
                .state,
            RunState::Unknown
        );
    }
}

#[tokio::test]
async fn local_workspace_location_and_branch_survive_reopening_the_run_store() {
    let (coordinator, paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    let run = prepare_and_approve(&coordinator, snapshot.clone()).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
    let store = lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
        paths.path().join("app-data"),
    ))
    .unwrap();
    let reopened = CodingAgentCoordinator::new(Arc::new(store), Some(runtime.clone()), None);
    let record = reopened.get_run(&run.snapshot.run_id).unwrap().unwrap();
    let receipt = reopened.workspace_receipt(&record).unwrap().unwrap();
    assert_eq!(receipt.run_id, snapshot.run_id.as_str());
    assert_eq!(receipt.branch, snapshot.workspace_branch);
    assert_eq!(receipt.base_commit, snapshot.base_commit);
    assert_eq!(
        receipt.workspace_token,
        workspace_receipt(&WorkspaceRequest {
            snapshot,
            attempt_id: "attempt-1".into()
        })
        .workspace_token
    );
    for mismatch in ["base", "branch", "repository", "run"] {
        let mut changed = record.clone();
        match mismatch {
            "base" => changed.snapshot.base_commit = "a".repeat(40),
            "branch" => changed.snapshot.workspace_branch = "agent/another-run".into(),
            "repository" => changed.snapshot.repository.identity.repository = "other".into(),
            "run" => changed.snapshot.run_id = RunId::new("another-run").unwrap(),
            _ => unreachable!(),
        }
        assert!(
            reopened.workspace_receipt(&changed).is_err(),
            "must reject mismatched {mismatch}"
        );
    }
    let mut next_attempt = record.clone();
    next_attempt.current_attempt_id = Some("attempt-2".into());
    assert!(reopened.workspace_receipt(&next_attempt).unwrap().is_none());
    let mut legacy = record.clone();
    legacy
        .evidence
        .retain(|reference| reference.name != "workspace-receipt");
    assert!(reopened.workspace_receipt(&legacy).unwrap().is_none());
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn human_semantic_reassessment_preserves_the_run_and_replays_only_its_saved_result() {
    use lookback_tauri_lib::coding_agent::assessment::AssessmentUse;
    for purpose in [RunPurpose::Implementation, RunPurpose::Investigation] {
        let (coordinator, paths, snapshot, runtime) = setup(purpose, None);
        *runtime.source_inspection.lock().unwrap() = Ok(Some(
            runtime
                .commit_observation
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .source
                .clone(),
        ));
        *runtime.semantic_outcome.lock().unwrap() = Ok(AssessmentUse::Fail);
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();
        let before =
            wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
        let saved_original = coordinator
            .evidence_references(&run.snapshot.run_id)
            .unwrap()
            .into_iter()
            .find(|item| item.name == "ordinary-semantic-result")
            .unwrap();
        let original_bytes = coordinator
            .read_evidence(&run.snapshot.run_id, &saved_original.evidence_id)
            .unwrap();
        let commits = runtime.commits.load(Ordering::SeqCst);
        let issue_saves = runtime.issue_result_saves.load(Ordering::SeqCst);
        *runtime.semantic_outcome.lock().unwrap() = Ok(AssessmentUse::Pass);
        let result = coordinator
            .reassess_semantics(&run.snapshot.run_id, before.revision, 7, "human-request-1")
            .await
            .unwrap();
        assert_eq!(result.assessment.outcome, AssessmentUse::Pass);
        assert_eq!(result.attempt_id, "attempt-1");
        assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 2);
        let replay = coordinator
            .reassess_semantics(&run.snapshot.run_id, before.revision, 7, "human-request-1")
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(replay).unwrap(),
            serde_json::to_value(&result).unwrap()
        );
        let reopened = CodingAgentCoordinator::new(
            Arc::new(
                lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
                    paths.path().join("app-data"),
                ))
                .unwrap(),
            ),
            None,
            None,
        );
        assert!(
            reopened
                .reassess_semantics(&run.snapshot.run_id, before.revision, 7, "human-request-1")
                .await
                .is_ok()
        );
        assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 2);
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.commits.load(Ordering::SeqCst), commits);
        assert_eq!(
            runtime.issue_result_saves.load(Ordering::SeqCst),
            issue_saves
        );
        let after = coordinator.get_run(&run.snapshot.run_id).unwrap().unwrap();
        assert_eq!(after.state, RunState::AwaitingReview);
        assert!(!after.execution_evidence.unwrap().acceptance_conditions_met);
        assert_eq!(
            coordinator
                .read_evidence(&run.snapshot.run_id, &saved_original.evidence_id)
                .unwrap(),
            original_bytes
        );
        assert!(
            coordinator
                .reassess_semantics(&run.snapshot.run_id, before.revision, 8, "human-request-1")
                .await
                .is_err()
        );
        assert!(
            coordinator
                .reassess_semantics(
                    &run.snapshot.run_id,
                    before.revision + 1,
                    7,
                    "human-request-2"
                )
                .await
                .is_err()
        );
        assert!(
            coordinator
                .reassess_semantics(&run.snapshot.run_id, before.revision, 7, "")
                .await
                .is_err()
        );
        assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn human_semantic_reassessment_recovers_a_saved_result_without_redispatch() {
    let (coordinator, paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    *runtime.source_inspection.lock().unwrap() = Ok(Some(
        runtime
            .commit_observation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .source
            .clone(),
    ));
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    let waiting =
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
    let mut result = coordinator
        .reassess_semantics(&run.snapshot.run_id, waiting.revision, 7, "first-request")
        .await
        .unwrap();
    let store = Arc::new(
        lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
            paths.path().join("app-data"),
        ))
        .unwrap(),
    );
    result.request_key = "crashed-request".into();
    let digest = format!("{:x}", Sha256::digest(result.request_key.as_bytes()));
    let key = format!("human-semantic:{digest}");
    store
        .claim_effect(
            &run.snapshot.run_id,
            "attempt-1",
            &key,
            DurableOperationRequest::ReassessSemantics {
                expected_run_revision: waiting.revision,
                issue_revision: 7,
                source_sha256: result.source_sha256.clone(),
                report_reference: result.report_reference.clone(),
            },
        )
        .unwrap();
    let reference = store
        .write_evidence(
            &run.snapshot.run_id,
            "attempt-1",
            &format!("human-semantic-{digest}"),
            "application/json",
            &serde_json::to_vec(&result).unwrap(),
        )
        .unwrap();
    assert_eq!(
        store
            .operation(&run.snapshot.run_id, &key)
            .unwrap()
            .unwrap()
            .status,
        DurableOperationStatus::Intent
    );
    let reopened = CodingAgentCoordinator::new(store.clone(), None, None);
    let read = reopened
        .read_semantic_reassessment(&run.snapshot.run_id, waiting.revision, 7, "crashed-request")
        .unwrap()
        .unwrap();
    assert_eq!(read.source_sha256, result.source_sha256);
    let operation = store
        .operation(&run.snapshot.run_id, &key)
        .unwrap()
        .unwrap();
    assert_eq!(operation.status, DurableOperationStatus::Completed);
    assert_eq!(
        operation.result.unwrap().evidence_ids,
        vec![reference.evidence_id]
    );
    reopened
        .reassess_semantics(&run.snapshot.run_id, waiting.revision, 7, "crashed-request")
        .await
        .unwrap();
    assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 2);
    for mismatch in ["source", "report"] {
        result.request_key = format!("mismatched-{mismatch}");
        let digest = format!("{:x}", Sha256::digest(result.request_key.as_bytes()));
        let key = format!("human-semantic:{digest}");
        let request = DurableOperationRequest::ReassessSemantics {
            expected_run_revision: waiting.revision,
            issue_revision: 7,
            source_sha256: result.source_sha256.clone(),
            report_reference: result.report_reference.clone(),
        };
        store
            .claim_effect(&run.snapshot.run_id, "attempt-1", &key, request)
            .unwrap();
        let mut wrong = result.clone();
        if mismatch == "source" {
            wrong.source_sha256 = "b".repeat(64);
        } else {
            wrong.report_reference = "another-report".into();
        }
        store
            .write_evidence(
                &run.snapshot.run_id,
                "attempt-1",
                &format!("human-semantic-{digest}"),
                "application/json",
                &serde_json::to_vec(&wrong).unwrap(),
            )
            .unwrap();
        assert!(
            reopened
                .read_semantic_reassessment(
                    &run.snapshot.run_id,
                    waiting.revision,
                    7,
                    &result.request_key
                )
                .is_err()
        );
        assert_eq!(
            store
                .operation(&run.snapshot.run_id, &key)
                .unwrap()
                .unwrap()
                .status,
            DurableOperationStatus::Intent
        );
    }
    assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn human_semantic_reassessment_rejects_off_and_unfinished_runs_without_dispatch() {
    for enabled in [false, true] {
        let (coordinator, _paths, mut snapshot, runtime) = setup(RunPurpose::Investigation, None);
        snapshot.semantic_judge_enabled = enabled;
        let run = prepare_and_approve(&coordinator, snapshot).await;
        assert!(
            coordinator
                .reassess_semantics(&run.snapshot.run_id, run.revision, 7, "human-request")
                .await
                .is_err()
        );
        assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 0);
        if !enabled {
            coordinator.start(&run.snapshot.run_id, None).await.unwrap();
            let waiting =
                wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
            assert!(
                coordinator
                    .reassess_semantics(&run.snapshot.run_id, waiting.revision, 7, "human-request")
                    .await
                    .is_err()
            );
            assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn human_semantic_reassessment_admission_is_atomic_across_independent_stores() {
    let (coordinator, paths, snapshot, _runtime) = setup(RunPurpose::Investigation, None);
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    let waiting =
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
    let report_reference = waiting
        .execution_evidence
        .as_ref()
        .unwrap()
        .report_reference
        .clone()
        .unwrap();
    let request = DurableOperationRequest::ReassessSemantics {
        expected_run_revision: waiting.revision,
        issue_revision: 7,
        source_sha256: "a".repeat(64),
        report_reference,
    };
    let data_paths = DataPaths::with_root(paths.path().join("app-data"));
    let first = Arc::new(lookback_tauri_lib::coding_agent::RunStore::open(&data_paths).unwrap());
    let second = Arc::new(lookback_tauri_lib::coding_agent::RunStore::open(&data_paths).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let claims = std::thread::scope(|scope| {
        let handles = [first.clone(), second.clone()].map(|store| {
            let barrier = barrier.clone();
            let request = request.clone();
            let run_id = run.snapshot.run_id.clone();
            scope.spawn(move || {
                barrier.wait();
                store
                    .claim_effect(&run_id, "attempt-1", "human-semantic:request-1", request)
                    .unwrap()
            })
        });
        handles.map(|handle| handle.join().unwrap())
    });
    assert_eq!(claims.iter().filter(|claim| claim.newly_created).count(), 1);
    assert_eq!(
        claims[0].operation.operation_id,
        claims[1].operation.operation_id
    );
    assert!(
        first
            .claim_effect(
                &run.snapshot.run_id,
                "attempt-1",
                "human-semantic:request-2",
                request.clone()
            )
            .unwrap()
            .newly_created,
        "a distinct explicit human request must remain possible after an unknown prior request"
    );
    let stale = DurableOperationRequest::ReassessSemantics {
        expected_run_revision: waiting.revision + 1,
        issue_revision: 7,
        source_sha256: "a".repeat(64),
        report_reference: waiting
            .execution_evidence
            .as_ref()
            .unwrap()
            .report_reference
            .clone()
            .unwrap(),
    };
    assert!(
        second
            .claim_effect(
                &run.snapshot.run_id,
                "attempt-1",
                "human-semantic:request-3",
                stale
            )
            .is_err()
    );
}

#[tokio::test]
async fn an_interrupted_human_reassessment_does_not_replay_but_allows_a_new_explicit_request() {
    let (coordinator, paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    *runtime.source_inspection.lock().unwrap() = Ok(Some(
        runtime
            .commit_observation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .source
            .clone(),
    ));
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    let waiting =
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
    *runtime.semantic_gate.lock().unwrap() = Some(Arc::new(Notify::new()));
    let task = {
        let coordinator = coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        tokio::spawn(async move {
            coordinator
                .reassess_semantics(&run_id, waiting.revision, 7, "interrupted-request")
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.semantic_calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    *runtime.semantic_gate.lock().unwrap() = None;
    let reopened = second_coordinator(&paths, runtime.clone());
    assert!(
        reopened
            .reassess_semantics(
                &run.snapshot.run_id,
                waiting.revision,
                7,
                "interrupted-request"
            )
            .await
            .is_err()
    );
    assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 2);
    assert!(
        reopened
            .reassess_semantics(
                &run.snapshot.run_id,
                waiting.revision,
                7,
                "new-human-request"
            )
            .await
            .is_ok()
    );
    assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 3);
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn source_changes_during_human_reassessment_are_recorded_as_inconclusive() {
    use lookback_tauri_lib::coding_agent::assessment::AssessmentUse;
    let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Implementation, None);
    *runtime.source_inspection.lock().unwrap() = Ok(Some(
        runtime
            .commit_observation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .source
            .clone(),
    ));
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    let waiting =
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
    let gate = Arc::new(Notify::new());
    *runtime.semantic_gate.lock().unwrap() = Some(gate.clone());
    let task = {
        let coordinator = coordinator.clone();
        let run_id = run.snapshot.run_id.clone();
        tokio::spawn(async move {
            coordinator
                .reassess_semantics(&run_id, waiting.revision, 7, "human-request")
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.semantic_calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    runtime
        .source_inspection
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .as_mut()
        .unwrap()
        .uncommitted_diff = Some(b"changed during judgment".to_vec());
    gate.notify_one();
    let result = task.await.unwrap().unwrap();
    assert_eq!(result.assessment.outcome, AssessmentUse::Inconclusive);
    assert_eq!(
        result.assessment.reason,
        "source_changed_during_reassessment"
    );
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ordinary_semantic_judgement_is_optional_durable_and_never_promotes_unknown_results() {
    use lookback_tauri_lib::coding_agent::assessment::AssessmentUse;
    for purpose in [RunPurpose::Implementation, RunPurpose::Investigation] {
        for (enabled, outcome) in [
            (false, Ok(AssessmentUse::Pass)),
            (true, Ok(AssessmentUse::Pass)),
            (true, Ok(AssessmentUse::Fail)),
            (true, Ok(AssessmentUse::Inconclusive)),
            (true, Err(ExecutionResult::AdapterUnavailable)),
        ] {
            let (coordinator, paths, mut snapshot, runtime) = setup(purpose, None);
            snapshot.semantic_judge_enabled = enabled;
            *runtime.semantic_outcome.lock().unwrap() = outcome;
            let run = prepare_and_approve(&coordinator, snapshot).await;
            coordinator.start(&run.snapshot.run_id, None).await.unwrap();
            wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
            assert_eq!(
                runtime.semantic_calls.load(Ordering::SeqCst),
                usize::from(enabled)
            );
            let stored = coordinator.get_run(&run.snapshot.run_id).unwrap().unwrap();
            assert_eq!(
                stored.execution_evidence.unwrap().acceptance_conditions_met,
                enabled && outcome == Ok(AssessmentUse::Pass)
            );
            let result = coordinator
                .evidence_references(&run.snapshot.run_id)
                .unwrap()
                .into_iter()
                .find(|reference| reference.name == "ordinary-semantic-result")
                .unwrap();
            let bytes = coordinator
                .read_evidence(&run.snapshot.run_id, &result.evidence_id)
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                value["status"],
                if enabled { "completed" } else { "skipped" }
            );
            assert!(
                value["assessment"]["reason"]
                    .as_str()
                    .is_some_and(|reason| !reason.is_empty())
            );
            if enabled {
                assert_eq!(
                    value["outcome"],
                    serde_json::to_value(outcome.unwrap_or(AssessmentUse::Inconclusive)).unwrap()
                );
            }
            let sent: serde_json::Value = serde_json::from_slice(
                runtime
                    .saved_result_report
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(sent["agent_report"]["run_id"], run.snapshot.run_id.as_str());
            assert_eq!(sent["semantic_assessment"], value);
            assert_eq!(
                sent["host_validation"],
                serde_json::to_value(
                    coordinator
                        .get_run(&run.snapshot.run_id)
                        .unwrap()
                        .unwrap()
                        .execution_evidence
                        .unwrap()
                        .validation
                )
                .unwrap()
            );
            let reopened = second_coordinator(&paths, runtime.clone());
            assert_eq!(
                reopened
                    .read_evidence(&run.snapshot.run_id, &result.evidence_id)
                    .unwrap(),
                bytes
            );
            let _ = reopened.resume_finalization(&run.snapshot.run_id).await;
            assert_eq!(
                reopened
                    .get_run(&run.snapshot.run_id)
                    .unwrap()
                    .unwrap()
                    .state,
                RunState::AwaitingReview
            );
            assert_eq!(
                runtime.semantic_calls.load(Ordering::SeqCst),
                usize::from(enabled)
            );
        }
    }
}

#[tokio::test]
async fn cancelling_during_ordinary_judgement_does_not_wait_or_save_an_issue_result() {
    let (coordinator, paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    *runtime.semantic_gate.lock().unwrap() = Some(Arc::new(Notify::new()));
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.semantic_calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        coordinator.cancel(&run.snapshot.run_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 0);
    let reopened = second_coordinator(&paths, runtime.clone());
    let _ = reopened.resume_finalization(&run.snapshot.run_id).await;
    assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn source_changes_during_ordinary_judgement_prevent_issue_result_publication() {
    let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Implementation, None);
    let gate = Arc::new(Notify::new());
    *runtime.semantic_gate.lock().unwrap() = Some(gate.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.semantic_calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut changed = runtime
        .commit_observation
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .source
        .clone();
    changed.head_commit = "f".repeat(40);
    changed.tracked_diff = b"changed while judging".to_vec();
    *runtime.source_inspection.lock().unwrap() = Ok(Some(changed));
    gate.notify_one();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Unknown).await;
    assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reopening_during_ordinary_judgement_neither_dispatches_again_nor_skips_the_pending_phase()
{
    let (coordinator, paths, snapshot, runtime) = setup(RunPurpose::Investigation, None);
    let gate = Arc::new(Notify::new());
    *runtime.semantic_gate.lock().unwrap() = Some(gate.clone());
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.semantic_calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let reopened = second_coordinator(&paths, runtime.clone());
    reopened.recover_on_startup().unwrap();
    let _ = reopened.resume_finalization(&run.snapshot.run_id).await;
    assert_eq!(runtime.semantic_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 0);
    assert_ne!(
        reopened
            .get_run(&run.snapshot.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::AwaitingReview
    );
    gate.notify_one();
}

#[test]
fn a_semantic_worker_configuration_digest_must_be_a_complete_sha256_when_present() {
    let mut snapshot = snapshot(RunPurpose::Implementation);
    snapshot.workflow.semantic_judge_worker_digest = Some("malformed".into());
    assert!(snapshot.clone().normalize_for_storage().is_err());
    snapshot.workflow.semantic_judge_worker_digest = Some("f".repeat(64));
    assert!(snapshot.clone().normalize_for_storage().is_ok());
}

#[tokio::test]
async fn ordinary_validation_uses_actual_results_before_commit_and_keeps_unconfirmed_runs_unknown()
{
    for outcome in [Ok(0), Ok(7), Err(ExecutionResult::ReconciliationUnknown)] {
        let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Implementation, None);
        let contract = executable_validation_contract(&snapshot);
        *runtime.validation_exit.lock().unwrap() = outcome;
        let run = prepare_and_approve_with_contract(&coordinator, snapshot, contract).await;
        coordinator
            .start(&run.snapshot.run_id, Some(b"implement issue".to_vec()))
            .await
            .unwrap();
        let expected_state = match outcome {
            Ok(0) => RunState::AwaitingReview,
            Ok(_) => RunState::Failed,
            Err(_) => RunState::Unknown,
        };
        let completed = wait_for_state(&coordinator, &run.snapshot.run_id, expected_state).await;
        if outcome.is_err() {
            assert_eq!(
                coordinator
                    .resume_finalization(&run.snapshot.run_id)
                    .await
                    .unwrap()
                    .state,
                RunState::Unknown
            );
        }
        assert_eq!(runtime.validation_calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.commits.load(Ordering::SeqCst), 0);
        if let Ok(code) = outcome {
            assert_eq!(
                completed.execution_evidence.unwrap().validation.state,
                if code == 0 {
                    ValidationState::Passed
                } else {
                    ValidationState::Failed
                }
            );
            let saved = coordinator
                .evidence_references(&run.snapshot.run_id)
                .unwrap()
                .into_iter()
                .filter_map(|reference| {
                    serde_json::from_slice::<serde_json::Value>(
                        &coordinator
                            .read_evidence(&run.snapshot.run_id, &reference.evidence_id)
                            .unwrap(),
                    )
                    .ok()
                })
                .find(|value| value.get("workflow_result").is_some());
            assert_eq!(saved.unwrap()["workflow_result"]["exit_code"], code);
        }
    }
}

fn executable_validation_contract(snapshot: &AgentRunSnapshot) -> ApprovalAssessmentContract {
    let mut contract = approval_assessment_contract(snapshot.purpose, true);
    contract.validation_plan[0].command = Some(
        lookback_tauri_lib::coding_agent::model::ApprovalAssessmentCommand {
            argv: vec!["cargo".into(), "test".into()],
            working_directory: "/workspace".into(),
            timeout_ms: 30_000,
            permissions: vec![
                lookback_tauri_lib::coding_agent::model::ValidationPermission::ReadWorkspace,
                lookback_tauri_lib::coding_agent::model::ValidationPermission::WriteBuildOutput,
            ],
            profile_digest: lookback_tauri_lib::coding_agent::model::validation_profile_digest(
                &snapshot.profile.profile_id,
                snapshot.profile.version,
            )
            .unwrap(),
        },
    );
    contract
}

#[tokio::test]
async fn ordinary_validation_can_run_two_commands_with_distinct_durable_results() {
    let (coordinator, _paths, snapshot, runtime) = setup(RunPurpose::Implementation, None);
    let mut contract = executable_validation_contract(&snapshot);
    let mut second = contract.validation_plan[0].clone();
    second.validation_id = "V-2".into();
    second.command.as_mut().unwrap().argv = vec!["cargo".into(), "clippy".into()];
    contract.validation_plan.push(second);
    runtime
        .extra_validation_ids
        .lock()
        .unwrap()
        .push("V-2".into());
    let run = prepare_and_approve_with_contract(&coordinator, snapshot, contract).await;
    coordinator
        .start(&run.snapshot.run_id, Some(b"implement issue".to_vec()))
        .await
        .unwrap();
    let completed =
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
    assert_eq!(runtime.validation_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        completed.execution_evidence.unwrap().validation.state,
        ValidationState::Passed
    );
    for id in ["V-1", "V-2"] {
        assert!(
            coordinator
                .evidence_references(&run.snapshot.run_id)
                .unwrap()
                .iter()
                .any(|reference| reference.name == format!("workspace-validation-result-{id}"))
        );
    }
}

#[tokio::test]
async fn validation_wait_does_not_block_cancel_or_let_reopen_skip_the_pending_phase() {
    let (coordinator, paths, snapshot, runtime) = setup(RunPurpose::Implementation, None);
    let contract = executable_validation_contract(&snapshot);
    let gate = Arc::new(Notify::new());
    *runtime.validation_gate.lock().unwrap() = Some(gate.clone());
    let run = prepare_and_approve_with_contract(&coordinator, snapshot, contract).await;
    coordinator
        .start(&run.snapshot.run_id, Some(b"implement issue".to_vec()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while runtime.validation_calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let reopened = second_coordinator(&paths, runtime.clone());
    let pending = reopened
        .resume_finalization(&run.snapshot.run_id)
        .await
        .unwrap();
    assert_eq!(pending.state, RunState::Verifying);
    assert_eq!(runtime.validation_calls.load(Ordering::SeqCst), 1);
    tokio::time::timeout(
        Duration::from_secs(1),
        coordinator.cancel(&run.snapshot.run_id),
    )
    .await
    .expect("validation drain must not hold the run lock")
    .unwrap();
    gate.notify_waiters();
    wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Unknown).await;
    assert_eq!(runtime.commits.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.issue_result_saves.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn publication_preflight_requires_the_requested_operations_without_mandatory_artifact_helpers()
 {
    for plan in [
        PublicationPlan::PushBranch,
        PublicationPlan::CreatePullRequest,
    ] {
        for missing in [None, Some("push"), Some("pull_request")] {
            let (coordinator, _paths, mut snapshot, runtime) =
                setup(RunPurpose::Implementation, None);
            snapshot.publication_plan = plan.clone();
            snapshot.allowed_operations = vec![
                AllowedOperation::Verify,
                AllowedOperation::PublishBranch,
                AllowedOperation::CreatePullRequest,
            ];
            let mut features = BTreeSet::from(["push".into(), "pull_request".into()]);
            if let Some(feature) = missing {
                features.remove(feature);
            }
            let publication = Arc::new(FakePublication {
                begins: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                reconciliations: AtomicUsize::new(0),
                outcome: Mutex::new(PublicationOutcome::Unknown),
                publish_gate: Mutex::new(None),
                admitted: AtomicBool::new(false),
                publication_job_id: Mutex::new(Some(81)),
                support_override: Mutex::new(Some(PublicationSupport::Verified {
                    executor_version: "test-executor".into(),
                    protocol_version: 2,
                    features,
                    approved_base_branch: "main".into(),
                    approved_web_base_url: "https://forge.example".into(),
                })),
            });
            let coordinator = with_publication(coordinator, publication.clone());
            let contract = approval_assessment_contract(snapshot.purpose, false);
            let outcome = coordinator
                .prepare_run_with_assessment_contract(snapshot, contract)
                .await
                .unwrap();
            let supported = missing.is_none()
                || (missing == Some("pull_request") && plan == PublicationPlan::PushBranch);
            assert_eq!(matches!(outcome, PreparationOutcome::Ready(_)), supported);
            assert_eq!(runtime.calls.load(Ordering::SeqCst), 0);
            assert_eq!(publication.calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[allow(dead_code)]
mod recovery_native_package {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/native_terminal_package.rs"
    ));
}

#[tokio::test]
async fn a_reopened_native_adapter_reads_only_the_saved_issue_workspace_for_reassessment() {
    use lookback_tauri_lib::coding_agent::native_execution::{
        NativeExecutionAdapter, prepared_snapshot_digest,
    };
    use recovery_native_package::*;
    let fixture = NativeFixture::new();
    let features = ["prepare", "inspect_source"];
    let calls_path = fixture.temp.path().join("semantic-helper-operations.txt");
    let script = format!(
        r#"#!/usr/bin/python3
import json, sys
if sys.argv[1] == '--capabilities':
    print(json.dumps({{'protocol_version': {}, 'executor_version': {}, 'features': {}}}))
else:
    name, source = next(iter(json.load(sys.stdin)['operation'].items()))
    with open({}, 'a') as calls:
        calls.write(name + '\n')
    if name != 'inspect_source':
        sys.exit(97)
    branch = 'agent/issue-' + source['issue_id'] + '-' + source['run_id']
    result = {{'source_inspected': {{'run_id': source['run_id'], 'attempt_id': source['attempt_id'],
        'base_commit': source['base_commit'], 'head_commit': source['base_commit'], 'branch': branch,
        'tracked_diff': [100, 105, 102, 102], 'uncommitted_diff': [], 'new_files': []}}}}
    print(json.dumps({{'protocol_version': {}, 'status': 'ok', 'result': result}}))
"#,
        coding_agent_executor::PROTOCOL_VERSION,
        serde_json::to_string(coding_agent_executor::EXECUTOR_VERSION).unwrap(),
        serde_json::to_string(&features).unwrap(),
        serde_json::to_string(&calls_path).unwrap(),
        coding_agent_executor::PROTOCOL_VERSION
    );
    write_private_executable(
        &fixture.resource_root.join("bin/coding-agent-executor"),
        script.as_bytes(),
    );
    let manifest = manifest_json_with_features(&fixture.resource_root, &features);
    set_manifest(&fixture, &manifest);
    write_host_policy(
        &fixture.data_paths,
        &manifest,
        &[("forge.example", "lookback", "agent-app")],
        Some("read-token"),
    );
    let (coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Investigation, None);
    let source = coding_agent_executor::PrepareRequest {
        issue_id: Some(snapshot.issue_id.clone()),
        run_id: snapshot.run_id.to_string(),
        attempt_id: "attempt-1".into(),
        repository: NativeFixture::repository(("forge.example", "lookback", "agent-app")),
        base_branch: "trunk".into(),
        base_commit: snapshot.base_commit.clone(),
    };
    snapshot.workspace_branch = source.expected_branch();
    let run = prepare_and_approve(&coordinator, snapshot).await;
    coordinator.start(&run.snapshot.run_id, None).await.unwrap();
    let waiting =
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::AwaitingReview).await;
    let store = Arc::new(
        lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
            paths.path().join("app-data"),
        ))
        .unwrap(),
    );
    let settings = Arc::new(
        lookback_tauri_lib::coding_agent::settings::CodingAgentSettingsStore::new(
            fixture.temp.path().join("settings.json"),
        ),
    );
    let current = settings.load().unwrap();
    settings
        .compare_and_swap_native_baseline(
            current.revision,
            hash_file(
                &fixture
                    .resource_root
                    .join("coding-agent-artifact-manifest.json"),
            ),
        )
        .unwrap();
    let registry = Arc::new(
        lookback_tauri_lib::native_terminal::registry::NativeRuntimeRegistry::new(
            fixture.data_paths.clone(),
            sidecars(fixture.temp.path()),
            Arc::new(lookback_tauri_lib::commands::IssueTrackerAvailability::default()),
            Arc::clone(&settings),
            Arc::new(tokio::sync::Mutex::new(())),
            run.snapshot.profile.validated_host_limits,
            Some(fixture.app_resource_dir.clone()),
        ),
    );
    let native = NativeExecutionAdapter::new(registry, Arc::new(tokio::sync::Mutex::new(())))
        .with_publication_store(store.clone());
    let receipt_ref = coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap()
        .into_iter()
        .find(|item| item.name == "dispatch-receipt")
        .unwrap();
    let receipt: StartReceipt = serde_json::from_slice(
        &coordinator
            .read_evidence(&run.snapshot.run_id, &receipt_ref.evidence_id)
            .unwrap(),
    )
    .unwrap();
    assert!(native.source_inspection(&receipt.identity).await.is_err());
    assert!(!calls_path.exists());
    let mut frozen = waiting.snapshot.clone();
    frozen.approval = None;
    frozen.auto_done = false;
    let saved = serde_json::json!({
        "snapshot_sha256": prepared_snapshot_digest("native-semantic-inputs", &frozen).unwrap(),
        "source_request": source, "issue_title":"Accepted issue", "issue_description":"Accepted body",
        "settings": lookback_tauri_lib::commands::llm_settings::LlmSettings::default(),
    });
    store
        .write_evidence(
            &run.snapshot.run_id,
            "attempt-1",
            "native-semantic-inputs",
            "application/json",
            &serde_json::to_vec(&saved).unwrap(),
        )
        .unwrap();
    let configured = lookback_tauri_lib::native_terminal::bootstrap::configure_repository(
        &fixture.data_paths,
        &fixture.selection().unwrap(),
        &source.repository,
    )
    .await;
    assert!(
        configured.is_ok(),
        "cold source bootstrap failed: {:?}",
        configured.err()
    );
    let observed = native
        .source_inspection(&receipt.identity)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observed.tracked_diff, b"diff");
    assert_eq!(observed.branch, run.snapshot.workspace_branch);
    let mut issue = lookback_tauri_lib::commands::issues::IssueListItem {
        id: run.snapshot.issue_id.clone(),
        revision: "7".into(),
        title: "Accepted issue".into(),
        description: "Accepted body".into(),
        kind: String::new(),
        priority: String::new(),
        state: "awaiting_review".into(),
        review_state: String::new(),
        created_at_ms: 0,
        updated_at_ms: 0,
        completion_criteria: None,
        acceptance_criteria_items: vec!["The expected behavior is present.".into()],
        registration_path: None,
        policy: None,
        proof: None,
        source_labels: Vec::new(),
        contexts: Vec::new(),
        claim: None,
    };
    native
        .verify_reassessment_issue(&waiting.snapshot, &issue)
        .await
        .unwrap();
    issue.description = "Changed body".into();
    assert!(
        native
            .verify_reassessment_issue(&waiting.snapshot, &issue)
            .await
            .is_err()
    );
    let mut wrong = receipt.identity.clone();
    wrong.parent_job_id = "another-job".into();
    assert!(native.source_inspection(&wrong).await.is_err());
    assert_eq!(
        std::fs::read_to_string(&calls_path).unwrap(),
        "inspect_source\n"
    );
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn native_publication_recovery_uses_durable_requests_without_a_candidate_or_agent_restart() {
    use lookback_tauri_lib::coding_agent::native_execution::NativeExecutionAdapter;
    use recovery_native_package::*;
    for plan in [
        PublicationPlan::PushBranch,
        PublicationPlan::CreatePullRequest,
    ] {
        let fixture = NativeFixture::new();
        let features = ["prepare", "lookup_publication"];
        let calls_path = fixture.temp.path().join("helper-operations.jsonl");
        let script = format!(
            r#"#!/usr/bin/python3
import json, sys
if sys.argv[1] == '--capabilities':
    print(json.dumps({{'protocol_version': {}, 'executor_version': {}, 'features': {}}}))
else:
    request = json.load(sys.stdin)['operation']
    name, value = next(iter(request.items()))
    with open({}, 'a') as calls:
        calls.write(name + '\n')
    if name == 'lookup_push':
        result = {{'pushed': {{'run_id': value['run_id'], 'attempt_id': value['attempt_id'],
            'repository': value['repository'], 'branch': value['branch'], 'commit': value['frozen_commit'],
            'remote_head': value['frozen_commit'], 'artifact_digest': value['artifact_digest'],
            'already_present': True, 'secret_scan': 'clean_supported_formats_v1'}}}}
    elif name == 'lookup_pull_request':
        result = {{'pull_request_created': {{'run_id': value['run_id'], 'attempt_id': value['attempt_id'],
            'repository': value['repository'], 'number': 81, 'url': 'https://forge.example/lookback/agent-app/pull/81',
            'head_commit': value['frozen_commit'], 'artifact_digest': value['artifact_digest'],
            'base_branch': value['base_branch'], 'already_present': True}}}}
    else:
        sys.exit(97)
    print(json.dumps({{'protocol_version': {}, 'status': 'ok', 'result': result}}))
"#,
            coding_agent_executor::PROTOCOL_VERSION,
            serde_json::to_string(coding_agent_executor::EXECUTOR_VERSION).unwrap(),
            serde_json::to_string(&features).unwrap(),
            serde_json::to_string(&calls_path).unwrap(),
            coding_agent_executor::PROTOCOL_VERSION
        );
        write_private_executable(
            &fixture.resource_root.join("bin/coding-agent-executor"),
            script.as_bytes(),
        );
        let manifest = manifest_json_with_features(&fixture.resource_root, &features);
        set_manifest(&fixture, &manifest);
        write_host_policy(
            &fixture.data_paths,
            &manifest,
            &[("forge.example", "lookback", "agent-app")],
            Some("read-token"),
        );
        let mut policy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture.policy_path()).unwrap()).unwrap();
        policy["repositories"][0]["publish_credential_source"] = serde_json::json!("read-token");
        policy["repositories"][0]["forge"] = serde_json::json!({"kind":"gitea", "api_base_url":"https://forge.example/api/v1", "web_base_url":"https://forge.example"});
        write_private_file(
            &fixture.policy_path(),
            &serde_json::to_vec(&policy).unwrap(),
        );
        let (coordinator, paths, mut snapshot, runtime) = setup(RunPurpose::Implementation, None);
        snapshot.publication_plan = plan.clone();
        snapshot.repository.read_credential_ref =
            Some(lookback_tauri_lib::coding_agent::CredentialReference::new("read-token").unwrap());
        snapshot.repository.publication_credential_ref =
            snapshot.repository.read_credential_ref.clone();
        snapshot.allowed_operations = vec![
            AllowedOperation::Verify,
            AllowedOperation::PublishBranch,
            AllowedOperation::CreatePullRequest,
        ];
        let source = coding_agent_executor::PrepareRequest {
            issue_id: Some(snapshot.issue_id.clone()),
            run_id: snapshot.run_id.to_string(),
            attempt_id: "attempt-1".into(),
            repository: NativeFixture::repository(("forge.example", "lookback", "agent-app")),
            base_branch: "main".into(),
            base_commit: snapshot.base_commit.clone(),
        };
        snapshot.workspace_branch = source.expected_branch();
        let publication = Arc::new(FakePublication {
            begins: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            reconciliations: AtomicUsize::new(0),
            outcome: Mutex::new(PublicationOutcome::Unknown),
            support_override: Mutex::new(None),
            publish_gate: Mutex::new(None),
            admitted: AtomicBool::new(false),
            publication_job_id: Mutex::new(Some(81)),
        });
        let coordinator = with_publication(coordinator, publication.clone());
        let run = prepare_and_approve(&coordinator, snapshot).await;
        coordinator.start(&run.snapshot.run_id, None).await.unwrap();
        wait_for_state(&coordinator, &run.snapshot.run_id, RunState::Verifying).await;
        coordinator.publish(&run.snapshot.run_id).await.unwrap();
        let request = read_publication_request(&coordinator, &run.snapshot.run_id);
        let store = Arc::new(
            lookback_tauri_lib::coding_agent::RunStore::open(&DataPaths::with_root(
                paths.path().join("app-data"),
            ))
            .unwrap(),
        );
        let persisted = coding_agent_executor::WorkspacePushRequest {
            source,
            expected_commit: request.frozen_commit.clone(),
            source_digest: "e".repeat(64),
        };
        store
            .write_evidence(
                &run.snapshot.run_id,
                &request.attempt_id,
                "native-publication-push-request",
                "application/json",
                &serde_json::to_vec(&persisted).unwrap(),
            )
            .unwrap();
        let settings = Arc::new(
            lookback_tauri_lib::coding_agent::settings::CodingAgentSettingsStore::new(
                fixture.temp.path().join("settings.json"),
            ),
        );
        let current = settings.load().unwrap();
        settings
            .compare_and_swap_native_baseline(
                current.revision,
                hash_file(
                    &fixture
                        .resource_root
                        .join("coding-agent-artifact-manifest.json"),
                ),
            )
            .unwrap();
        let registry = Arc::new(
            lookback_tauri_lib::native_terminal::registry::NativeRuntimeRegistry::new(
                fixture.data_paths.clone(),
                sidecars(fixture.temp.path()),
                Arc::new(lookback_tauri_lib::commands::IssueTrackerAvailability::default()),
                Arc::clone(&settings),
                Arc::new(tokio::sync::Mutex::new(())),
                run.snapshot.profile.validated_host_limits,
                Some(fixture.app_resource_dir.clone()),
            ),
        );
        let native = Arc::new(
            NativeExecutionAdapter::new(registry, Arc::new(tokio::sync::Mutex::new(())))
                .with_publication_store(store),
        );
        let endpoint_field = if plan == PublicationPlan::CreatePullRequest {
            "api_base_url"
        } else {
            "clone_url"
        };
        let mut changed = policy.clone();
        if endpoint_field == "api_base_url" {
            changed["repositories"][0]["forge"][endpoint_field] =
                serde_json::json!("https://forge.example/other-api");
        } else {
            changed["repositories"][0][endpoint_field] =
                serde_json::json!("https://forge.example:444/lookback/agent-app.git");
        }
        write_private_file(
            &fixture.policy_path(),
            &serde_json::to_vec(&changed).unwrap(),
        );
        assert!(matches!(
            native.reconcile(&request).await,
            Ok(PublicationOutcome::Unknown) | Err(ExecutionResult::ReconciliationUnknown)
        ));
        assert!(
            !calls_path.exists(),
            "changed endpoint must not issue any lookup with credentials"
        );
        write_private_file(
            &fixture.policy_path(),
            &serde_json::to_vec(&policy).unwrap(),
        );
        let restarted =
            second_coordinator(&paths, runtime.clone()).with_publication_adapter(native);
        assert!(matches!(
            restarted.reconcile(&run.snapshot.run_id).await.unwrap(),
            lookback_tauri_lib::coding_agent::RunReconciliation::Accepted(_)
        ));
        let operations = std::fs::read_to_string(calls_path).unwrap();
        assert_eq!(
            operations,
            if plan == PublicationPlan::CreatePullRequest {
                "lookup_push\nlookup_pull_request\n"
            } else {
                "lookup_push\n"
            }
        );
        assert_eq!(publication.calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    }
}
