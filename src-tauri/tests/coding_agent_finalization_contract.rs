use std::{
    collections::{BTreeMap, BTreeSet},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::{Stream, stream};
use lookback_tauri_lib::coding_agent::{
    assessment::{
        AllowedEvidenceSource, ApprovedAssessmentPolicy, ApprovedCriterion,
        ApprovedValidationDefinition, AssessmentApprovalSnapshot, AssessmentAuthorization,
        AssessmentClock, AssessmentEngine, AssessmentError, AssessmentOutcome,
        AssessmentPreparation, AssessmentStore, CriterionMethod, EvidencePayload,
        EvidenceSourceKind, JudgeProvider, JudgeProviderError, JudgeRequest, ProviderSelection,
        ReceiptLookup, prepare_assessment,
    },
    model::{
        ApprovalAssessmentCommand, ApprovalAssessmentContract, ApprovalCriterionPolicy,
        ApprovalValidationDraft, ApprovedJudgeInputField, AssessmentInputScope,
        CriterionEvaluationMethod, StoredAgentRunSnapshot,
    },
    report::{ReportExpectation, ReportReadResult, TaskKind, validate_report},
};
use lookback_tauri_lib::{
    coding_agent::model::AssessmentIntentRequest,
    coding_agent::{
        AgentProfileSnapshot, AgentRunRecord, AgentRunSnapshot, AllowedOperation,
        CancellationObservation, ChildWritersStatus, CodingAgentCoordinator, ExecutionAdapter,
        ExecutionEvent, ExecutionIdentity, ExecutionInvocationKind, ExecutionMode, ExecutionResult,
        ForgeKind, HostFinalizationObservation, HostResourceLimits, ImmutableExecutionIntent,
        InteractiveFinishObservation, OperationReconciliation, ProcessExitObservation,
        PublicationAdapter, PublicationOutcome, PublicationPlan, PublicationRequest,
        PublicationSupport, RepositoryIdentity, RepositoryProvenance, RepositorySelection,
        ResolvedRepository, RunId, RunPurpose, RunState, RunStore, RuntimeObservation,
        SandboxResources, ShellProcessObservation, StartOutcome, StartReceipt, TerminalControl,
        ValidationEvidence, ValidationState, ValidationTargetSnapshot, WorkflowDefinitionSnapshot,
        WorkspaceFreezeStatus, WorkspaceReceipt, WorkspaceRequest,
    },
    data::DataPaths,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::Mutex;

type EventStream = Pin<Box<dyn Stream<Item = Result<ExecutionEvent, ExecutionResult>> + Send>>;

struct FinalizationExecution {
    observation: Mutex<HostFinalizationObservation>,
}

#[async_trait]
impl ExecutionAdapter for FinalizationExecution {
    async fn preflight(
        &self,
        snapshot: &AgentRunSnapshot,
    ) -> Result<RuntimeObservation, ExecutionResult> {
        Ok(runtime_observation(snapshot))
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
        let encoded = serde_json::to_vec(intent).map_err(|_| ExecutionResult::Conflict)?;
        Ok(StartReceipt {
            identity: identity(&intent.run_id, &intent.attempt_id),
            execution_intent_sha256: format!("{:x}", Sha256::digest(encoded)),
            method_name: intent.method_name.clone(),
            image_digest: intent.image_digest.clone(),
            prompt_delivery: lookback_tauri_lib::coding_agent::PromptDelivery::Accepted,
        })
    }

    async fn reconcile_start(
        &self,
        _snapshot: &AgentRunSnapshot,
        _intent: &ImmutableExecutionIntent,
    ) -> Result<OperationReconciliation<StartReceipt>, ExecutionResult> {
        Ok(OperationReconciliation::Unknown(None))
    }

    async fn events(&self, _identity: &ExecutionIdentity) -> Result<EventStream, ExecutionResult> {
        Ok(Box::pin(stream::iter([Ok(ExecutionEvent::Exit {
            exit_code: 0,
        })])))
    }

    async fn terminal_control(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<Arc<dyn TerminalControl>, ExecutionResult> {
        Err(ExecutionResult::Unsupported)
    }

    async fn finalize_one_shot(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<HostFinalizationObservation, ExecutionResult> {
        Ok(self.observation.lock().await.clone())
    }

    async fn finalize_interactive(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<InteractiveFinishObservation, ExecutionResult> {
        Err(ExecutionResult::Unsupported)
    }

    async fn reconcile_interactive_finish(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<InteractiveFinishObservation>, ExecutionResult> {
        Ok(OperationReconciliation::Unknown(None))
    }

    async fn cancel(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<CancellationObservation>, ExecutionResult> {
        Ok(OperationReconciliation::Unknown(None))
    }

    async fn reconcile_cancel(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<CancellationObservation>, ExecutionResult> {
        Ok(OperationReconciliation::Unknown(None))
    }

    async fn heartbeat(&self, _identity: &ExecutionIdentity) -> Result<(), ExecutionResult> {
        Ok(())
    }
}

struct CountingPublication {
    calls: AtomicUsize,
}

#[async_trait]
impl PublicationAdapter for CountingPublication {
    async fn preflight(
        &self,
        _snapshot: &AgentRunSnapshot,
    ) -> Result<PublicationSupport, ExecutionResult> {
        Ok(PublicationSupport::Verified {
            executor_version: coding_agent_executor::EXECUTOR_VERSION.into(),
            protocol_version: coding_agent_executor::PROTOCOL_VERSION,
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
        })
    }

    async fn publish(
        &self,
        request: &PublicationRequest,
    ) -> Result<PublicationOutcome, ExecutionResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(PublicationOutcome::BranchPublished {
            branch: request.branch.clone(),
            commit: request.frozen_commit.clone(),
            remote_head: request.frozen_commit.clone(),
        })
    }

    async fn reconcile(
        &self,
        _request: &PublicationRequest,
    ) -> Result<PublicationOutcome, ExecutionResult> {
        Ok(PublicationOutcome::Unknown)
    }
}

#[tokio::test]
async fn malformed_report_is_durably_available_but_cannot_publish_or_complete() {
    let fixture = Fixture::new(RunPurpose::Implementation, true, Some(b"{broken".to_vec()));
    let run = fixture.prepare_and_approve().await;
    assert!(matches!(
        fixture
            .coordinator
            .start(&run.snapshot.run_id, None)
            .await
            .unwrap(),
        StartOutcome::Started { .. }
    ));

    let failed = fixture.wait_for_state(RunState::Failed).await;

    let raw = fixture
        .coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap()
        .into_iter()
        .find(|reference| reference.name == "agent-report-raw")
        .expect("malformed raw report should remain available for human review");
    assert_eq!(
        fixture
            .coordinator
            .read_evidence(&run.snapshot.run_id, &raw.evidence_id)
            .unwrap(),
        b"{broken"
    );
    assert!(
        failed
            .execution_evidence
            .as_ref()
            .is_some_and(|evidence| evidence.report_reference.is_none())
    );
    assert_eq!(fixture.publication.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn missing_or_approval_id_mismatched_report_never_becomes_canonical_evidence() {
    let mut mismatched: serde_json::Value =
        serde_json::from_slice(&report_bytes("completed", true)).unwrap();
    mismatched["acceptance"][0]["criterion_id"] = "AC-unknown".into();
    let cases = [
        ("missing", None),
        ("unknown-id", Some(serde_json::to_vec(&mismatched).unwrap())),
    ];

    for (_name, report) in cases {
        let fixture = Fixture::new(RunPurpose::Implementation, true, report);
        let run = fixture.prepare_and_approve().await;
        fixture
            .coordinator
            .start(&run.snapshot.run_id, None)
            .await
            .unwrap();
        let failed = fixture.wait_for_state(RunState::Failed).await;

        assert!(
            failed
                .execution_evidence
                .as_ref()
                .is_some_and(|assessment| assessment.report_reference.is_none())
        );
        assert!(
            !fixture
                .coordinator
                .evidence_references(&run.snapshot.run_id)
                .unwrap()
                .iter()
                .any(|reference| reference.name == "agent-report")
        );
        assert_eq!(fixture.publication.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn oversized_raw_report_is_truncated_at_the_reader_bound() {
    let raw = vec![b'x'; lookback_tauri_lib::coding_agent::MAX_REPORT_BYTES + 1];
    let fixture = Fixture::new(RunPurpose::Implementation, true, Some(raw));
    let run = fixture.prepare_and_approve().await;
    fixture
        .coordinator
        .start(&run.snapshot.run_id, None)
        .await
        .unwrap();
    let failed = fixture.wait_for_state(RunState::Failed).await;

    let raw_reference = fixture
        .coordinator
        .evidence_references(&run.snapshot.run_id)
        .unwrap()
        .into_iter()
        .find(|reference| reference.name == "agent-report-raw")
        .unwrap();
    assert_eq!(
        raw_reference.size_bytes,
        lookback_tauri_lib::coding_agent::MAX_REPORT_BYTES as u64
    );
    assert_eq!(
        fixture
            .coordinator
            .read_evidence(&run.snapshot.run_id, &raw_reference.evidence_id)
            .unwrap()
            .len(),
        lookback_tauri_lib::coding_agent::MAX_REPORT_BYTES
    );
    assert!(
        failed
            .execution_evidence
            .as_ref()
            .is_some_and(|assessment| assessment.report_reference.is_none())
    );
}

#[tokio::test]
async fn partial_report_and_inconclusive_semantics_do_not_block_approved_publication() {
    for status in ["partial", "blocked"] {
        let fixture = Fixture::new(
            RunPurpose::Implementation,
            true,
            Some(report_bytes(status, true)),
        );
        let run = fixture.prepare_and_approve().await;
        assert!(matches!(
            fixture
                .coordinator
                .start(&run.snapshot.run_id, None)
                .await
                .unwrap(),
            StartOutcome::Started { .. }
        ));

        let verifying = fixture.wait_for_state(RunState::Verifying).await;
        let assessment = verifying.execution_evidence.as_ref().unwrap();
        assert!(assessment.report_save_confirmed);
        assert!(!assessment.acceptance_conditions_met);
        assert!(assessment.validation.required);
        assert_eq!(assessment.validation.state, ValidationState::NotRun);
        let host_observation = fixture
            .coordinator
            .evidence_references(&run.snapshot.run_id)
            .unwrap()
            .into_iter()
            .find(|reference| reference.name == "host-finalization-evidence")
            .unwrap();
        let host_observation: serde_json::Value = serde_json::from_slice(
            &fixture
                .coordinator
                .read_evidence(&run.snapshot.run_id, &host_observation.evidence_id)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            host_observation["validation_observation"]["state"],
            "passed"
        );
        assert_eq!(
            host_observation["acceptance_conditions_met_observation"],
            true
        );
        assert!(!assessment.acceptance_conditions_met);
        let report_reference = assessment.report_reference.as_ref().unwrap();
        let canonical = fixture
            .coordinator
            .read_evidence(&run.snapshot.run_id, report_reference)
            .unwrap();
        let canonical_value: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
        assert_eq!(canonical_value["schema_version"], 1);
        assert_eq!(canonical_value["status"], status);
        let report_evidence = fixture
            .coordinator
            .evidence_references(&run.snapshot.run_id)
            .unwrap()
            .into_iter()
            .find(|reference| reference.name == "finalization-report-evidence")
            .unwrap();
        let report_evidence: serde_json::Value = serde_json::from_slice(
            &fixture
                .coordinator
                .read_evidence(&run.snapshot.run_id, &report_evidence.evidence_id)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            report_evidence["semantic_assessment"],
            "inconclusive_not_dispatched"
        );

        let published = fixture
            .coordinator
            .publish(&run.snapshot.run_id)
            .await
            .expect("required validation NotRun alone must not block approved publication");

        assert_eq!(published.state, RunState::AwaitingReview);
        assert_ne!(published.state, RunState::Done);
        assert_eq!(fixture.publication.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn host_validation_failure_is_preserved_and_blocks_approved_publication() {
    let fixture = Fixture::new_with_validation(
        RunPurpose::Implementation,
        true,
        Some(report_bytes("completed", true)),
        ValidationState::Failed,
    );
    let run = fixture.prepare_and_approve().await;
    fixture
        .coordinator
        .start(&run.snapshot.run_id, None)
        .await
        .unwrap();

    let failed = fixture.wait_for_state(RunState::Failed).await;

    let evidence = failed.execution_evidence.as_ref().unwrap();
    assert_eq!(evidence.validation.state, ValidationState::Failed);
    assert!(evidence.validation.required);
    assert!(!evidence.acceptance_conditions_met);
    assert!(
        fixture
            .coordinator
            .publish(&run.snapshot.run_id)
            .await
            .is_err()
    );
    assert_eq!(fixture.publication.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn run_store_claim_binds_the_approved_scope_and_recovers_without_redispatch() {
    let fixture = Fixture::new(
        RunPurpose::Implementation,
        true,
        Some(report_bytes_without_validation("completed")),
    );
    let contract = semantic_contract_without_validation();
    let run = fixture
        .prepare_and_approve_with_contract(contract.clone())
        .await;
    fixture
        .coordinator
        .start(&run.snapshot.run_id, None)
        .await
        .unwrap();
    let verifying = fixture.wait_for_state(RunState::Verifying).await;
    let attempt_id = verifying.current_attempt_id.as_deref().unwrap();
    let report_evidence_id = verifying
        .execution_evidence
        .as_ref()
        .unwrap()
        .report_reference
        .as_deref()
        .unwrap();
    let store = RunStore::open(&DataPaths::with_root(
        fixture._paths.path().join("app-data"),
    ))
    .unwrap();
    let report_bytes = store
        .read_evidence(&run.snapshot.run_id, report_evidence_id)
        .unwrap();
    let report_expectation = ReportExpectation::new(
        run.snapshot.run_id.as_str(),
        attempt_id,
        TaskKind::Implementation,
        contract
            .criteria
            .iter()
            .map(|criterion| (criterion.criterion_id.clone(), criterion.required))
            .collect::<BTreeMap<_, _>>(),
        BTreeMap::new(),
    )
    .unwrap();
    let validated_report = validate_report(&report_bytes, &report_expectation).unwrap();
    let report_digest = sha256_hex(&report_bytes);
    let report_read = ReportReadResult {
        validated_report: Some(validated_report),
        rejection: None,
        raw_bytes: report_bytes.clone(),
        observed_byte_count: report_bytes.len(),
        reported_file_size: Some(report_bytes.len() as u64),
        raw_bytes_sha256: report_digest.clone(),
        bytes_complete: true,
    };

    let stored_snapshot =
        StoredAgentRunSnapshot::new(run.snapshot.clone(), Some(contract.clone())).unwrap();
    let approval_snapshot_digest = sha256_hex(&serde_json::to_vec(&stored_snapshot).unwrap());
    let untracked_manifest = b"[]";
    let target = ValidationTargetSnapshot::new(
        "b".repeat(40),
        "c".repeat(40),
        sha256_hex(untracked_manifest),
    )
    .unwrap();
    let artifact_manifest = serde_json::to_vec(&json!({
        "schema_version": 1,
        "run_id": run.snapshot.run_id.as_str(),
        "attempt_id": attempt_id,
        "full_commit_id": target.full_commit_id(),
        "full_tree_id": target.full_tree_id(),
        "untracked_manifest_digest": target.untracked_manifest_digest(),
        "report_digest": report_digest,
    }))
    .unwrap();
    let frozen_artifact_digest = sha256_hex(&artifact_manifest);
    let manifest_reference = store
        .write_evidence(
            &run.snapshot.run_id,
            attempt_id,
            "test-sealed-artifact-manifest",
            "application/json",
            &artifact_manifest,
        )
        .unwrap();

    let approval = AssessmentApprovalSnapshot {
        authorization: AssessmentAuthorization::RunApproval,
        run_id: run.snapshot.run_id.to_string(),
        attempt_id: attempt_id.to_owned(),
        task_kind: TaskKind::Implementation,
        approval_snapshot_digest,
        frozen_artifact_digest,
        report_digest: report_digest.clone(),
        validation_receipts: Vec::new(),
        validation_plan: Vec::<ApprovedValidationDefinition>::new(),
        criteria: contract
            .criteria
            .iter()
            .map(|criterion| ApprovedCriterion {
                criterion_id: criterion.criterion_id.clone(),
                statement: criterion.statement.clone(),
                required: criterion.required,
                method: CriterionMethod::SemanticJudge,
            })
            .collect(),
        allowed_sources: contract.input_scope.allowed_sources.clone(),
        provider: contract.provider.clone().unwrap(),
        prompt_policy_version: contract.prompt_policy_version.clone().unwrap(),
        max_input_bytes: contract.input_scope.max_input_bytes,
        deadline_unix_ms: 100_000,
    };
    let policy = ApprovedAssessmentPolicy::new(approval.clone()).unwrap();
    let prepared = match prepare_assessment(
        &policy,
        "assessment-finalization-positive",
        Vec::<EvidencePayload>::new(),
        &report_read,
    )
    .unwrap()
    {
        AssessmentPreparation::Ready(prepared) => *prepared,
        AssessmentPreparation::NotRequired => panic!("approved semantic criterion needs judge"),
    };
    let prepared_input: serde_json::Value = serde_json::from_slice(prepared.input_bytes()).unwrap();
    assert_eq!(prepared_input["assessment_id"], prepared.assessment_id());
    assert_eq!(prepared_input["run_id"], run.snapshot.run_id.as_str());
    assert_eq!(prepared_input["attempt_id"], attempt_id);
    assert_eq!(
        prepared_input["approval_snapshot_digest"],
        approval.approval_snapshot_digest
    );
    assert_eq!(
        prepared_input["frozen_artifact_digest"],
        approval.frozen_artifact_digest
    );
    assert_eq!(prepared_input["report_digest"], report_digest);
    assert_eq!(
        prepared_input["criteria"][0]["criterion_id"],
        contract.criteria[0].criterion_id
    );
    assert_eq!(
        prepared_input["criteria"][0]["statement"],
        contract.criteria[0].statement
    );
    assert_eq!(prepared_input["validation_receipts"], json!([]));
    assert_eq!(prepared_input["validation_plan"], json!([]));
    assert_eq!(
        prepared_input["provider"],
        serde_json::to_value(contract.provider.as_ref().unwrap()).unwrap()
    );
    assert_eq!(
        prepared_input["prompt_policy_version"],
        contract.prompt_policy_version.as_deref().unwrap()
    );
    assert_eq!(
        prepared_input["judge_input_scope_digest"],
        contract.judge_input_scope_digest()
    );
    assert_eq!(
        prepared_input["evidence"][0]["content"],
        String::from_utf8(report_bytes.clone()).unwrap()
    );
    let system_prompt: serde_json::Value =
        serde_json::from_slice(prepared.system_policy_bytes()).unwrap();
    assert_eq!(
        system_prompt["policy_version"],
        contract.prompt_policy_version.as_deref().unwrap()
    );
    assert_eq!(
        system_prompt["response_contract"]["assessment_id"],
        prepared.assessment_id()
    );
    assert_eq!(
        system_prompt["response_contract"]["run_id"],
        run.snapshot.run_id.as_str()
    );
    assert_eq!(system_prompt["response_contract"]["attempt_id"], attempt_id);
    assert_eq!(
        system_prompt["response_contract"]["input_digest"],
        prepared.judge_input_digest()
    );
    let input_reference = store
        .write_evidence(
            &run.snapshot.run_id,
            attempt_id,
            "test-judge-input",
            "application/json",
            prepared.input_bytes(),
        )
        .unwrap();
    let request = AssessmentIntentRequest {
        assessment_id: prepared.assessment_id().to_owned(),
        judge_input_evidence_id: input_reference.evidence_id,
        report_evidence_id: report_evidence_id.to_owned(),
        snapshot_manifest_evidence_id: manifest_reference.evidence_id,
        target_full_commit_id: target.full_commit_id().to_owned(),
        target_full_tree_id: target.full_tree_id().to_owned(),
        target_untracked_manifest_digest: target.untracked_manifest_digest().to_owned(),
        target_snapshot_digest: target.digest().to_owned(),
        system_prompt_digest: sha256_hex(prepared.system_policy_bytes()),
        validation_receipts: Vec::new(),
    };
    let claim = store
        .bind_assessment_intent(
            &run.snapshot.run_id,
            verifying.revision,
            attempt_id,
            request.clone(),
        )
        .unwrap();
    assert!(claim.newly_created);
    assert_eq!(
        claim.binding.judge_input_scope_digest,
        contract.judge_input_scope_digest()
    );

    let assessment_root = fixture._paths.path().join("assessments");
    let engine = AssessmentEngine::new(AssessmentStore::new(&assessment_root).unwrap());
    let sends = Arc::new(AtomicUsize::new(0));
    let mut intent_only_provider = PassingJudge(Arc::clone(&sends));
    let reopened_store = RunStore::open(&DataPaths::with_root(
        fixture._paths.path().join("app-data"),
    ))
    .unwrap();
    let reopened_engine = AssessmentEngine::new(AssessmentStore::new(&assessment_root).unwrap());
    assert!(matches!(
        reopened_engine.resume_with_run_store_binding(
            prepared.assessment_id(),
            &mut intent_only_provider,
            &FixedClock,
            &reopened_store,
            &claim.binding,
        ),
        Err(AssessmentError::Io(_))
    ));
    assert_eq!(sends.load(Ordering::SeqCst), 0);

    for mutate in [
        |snapshot: &mut AssessmentApprovalSnapshot| {
            snapshot.allowed_sources[0].identity = "report:other-attempt".into();
        },
        |snapshot: &mut AssessmentApprovalSnapshot| {
            snapshot.allowed_sources[0].max_bytes -= 1;
        },
    ] {
        let mut out_of_scope = approval.clone();
        mutate(&mut out_of_scope);
        let out_of_scope_policy = ApprovedAssessmentPolicy::new(out_of_scope).unwrap();
        let out_of_scope_prepared = match prepare_assessment(
            &out_of_scope_policy,
            prepared.assessment_id(),
            Vec::new(),
            &report_read,
        )
        .unwrap()
        {
            AssessmentPreparation::Ready(prepared) => *prepared,
            AssessmentPreparation::NotRequired => unreachable!(),
        };
        assert!(matches!(
            engine.assess_with_run_store_claim(
                &out_of_scope_prepared,
                &mut intent_only_provider,
                &FixedClock,
                &store,
                &claim,
            ),
            Err(AssessmentError::DispatchUnauthorized)
        ));
    }
    assert_eq!(sends.load(Ordering::SeqCst), 0);

    let changed_assessment = match prepare_assessment(
        &policy,
        "assessment-finalization-different-payload",
        Vec::new(),
        &report_read,
    )
    .unwrap()
    {
        AssessmentPreparation::Ready(prepared) => *prepared,
        AssessmentPreparation::NotRequired => unreachable!(),
    };
    assert_eq!(
        prepared.judge_input_scope_digest(),
        changed_assessment.judge_input_scope_digest()
    );
    assert_ne!(
        prepared.judge_input_digest(),
        changed_assessment.judge_input_digest(),
        "assessment payload identity is committed separately from approved scope"
    );

    let first_result = engine
        .assess_with_run_store_claim(
            &prepared,
            &mut intent_only_provider,
            &FixedClock,
            &store,
            &claim,
        )
        .unwrap();
    assert!(matches!(first_result, AssessmentOutcome::Assessed { .. }));
    assert_eq!(sends.load(Ordering::SeqCst), 1);

    let reopened_claim = reopened_store
        .bind_assessment_intent(
            &run.snapshot.run_id,
            verifying.revision,
            attempt_id,
            request,
        )
        .unwrap();
    assert!(!reopened_claim.newly_created);
    let mut resume_provider = PassingJudge(Arc::clone(&sends));
    let resumed = reopened_engine
        .resume_with_run_store_binding(
            prepared.assessment_id(),
            &mut resume_provider,
            &FixedClock,
            &reopened_store,
            &reopened_claim.binding,
        )
        .unwrap();
    assert!(matches!(resumed, AssessmentOutcome::Assessed { .. }));
    assert_eq!(sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn run_store_intent_without_assessment_dispatch_marker_stays_unknown_and_lookup_only() {
    let fixture = Fixture::new(
        RunPurpose::Implementation,
        true,
        Some(report_bytes_without_validation("completed")),
    );
    let contract = semantic_contract_without_validation();
    let run = fixture
        .prepare_and_approve_with_contract(contract.clone())
        .await;
    fixture
        .coordinator
        .start(&run.snapshot.run_id, None)
        .await
        .unwrap();
    let verifying = fixture.wait_for_state(RunState::Verifying).await;
    let attempt_id = verifying.current_attempt_id.as_deref().unwrap();
    let report_evidence_id = verifying
        .execution_evidence
        .as_ref()
        .unwrap()
        .report_reference
        .as_deref()
        .unwrap();
    let store = RunStore::open(&DataPaths::with_root(
        fixture._paths.path().join("app-data"),
    ))
    .unwrap();
    let input_reference = store
        .write_evidence(
            &run.snapshot.run_id,
            attempt_id,
            "test-unprepared-judge-input",
            "application/json",
            br#"{"not_yet_prepared":true}"#,
        )
        .unwrap();
    let manifest_reference = store
        .write_evidence(
            &run.snapshot.run_id,
            attempt_id,
            "test-unverified-artifact-manifest",
            "application/json",
            b"{}",
        )
        .unwrap();
    let target =
        ValidationTargetSnapshot::new("b".repeat(40), "c".repeat(40), sha256_hex(b"[]")).unwrap();
    let request = AssessmentIntentRequest {
        assessment_id: "assessment-intent-only".into(),
        judge_input_evidence_id: input_reference.evidence_id,
        report_evidence_id: report_evidence_id.to_owned(),
        snapshot_manifest_evidence_id: manifest_reference.evidence_id,
        target_full_commit_id: target.full_commit_id().to_owned(),
        target_full_tree_id: target.full_tree_id().to_owned(),
        target_untracked_manifest_digest: target.untracked_manifest_digest().to_owned(),
        target_snapshot_digest: target.digest().to_owned(),
        system_prompt_digest: sha256_hex(b"not yet prepared system policy"),
        validation_receipts: Vec::new(),
    };
    let claim = store
        .bind_assessment_intent(
            &run.snapshot.run_id,
            verifying.revision,
            attempt_id,
            request.clone(),
        )
        .unwrap();
    assert!(claim.newly_created);

    let unknown = store
        .transition(&run.snapshot.run_id, verifying.revision, RunState::Unknown)
        .unwrap();
    assert_eq!(unknown.state, RunState::Unknown);
    let reopened_store = RunStore::open(&DataPaths::with_root(
        fixture._paths.path().join("app-data"),
    ))
    .unwrap();
    let recovered_claim = reopened_store
        .bind_assessment_intent(
            &run.snapshot.run_id,
            verifying.revision,
            attempt_id,
            request,
        )
        .unwrap();
    assert!(!recovered_claim.newly_created);

    let assessment_store = AssessmentStore::new(fixture._paths.path().join("assessments")).unwrap();
    let engine = AssessmentEngine::new(assessment_store);
    let sends = Arc::new(AtomicUsize::new(0));
    let mut provider = PassingJudge(Arc::clone(&sends));
    assert!(matches!(
        engine.resume_with_run_store_binding(
            &claim.binding.assessment_id,
            &mut provider,
            &FixedClock,
            &reopened_store,
            &recovered_claim.binding,
        ),
        Err(AssessmentError::Io(_))
    ));
    assert_eq!(sends.load(Ordering::SeqCst), 0);
    assert_eq!(
        reopened_store
            .get_run(&run.snapshot.run_id)
            .unwrap()
            .unwrap()
            .state,
        RunState::Unknown
    );
}

#[tokio::test]
async fn legacy_run_without_approval_contract_cannot_start_automatic_publication() {
    let fixture = Fixture::new(
        RunPurpose::Implementation,
        false,
        Some(report_bytes("completed", false)),
    );
    let run = fixture.prepare_and_approve().await;
    assert!(matches!(
        fixture
            .coordinator
            .start(&run.snapshot.run_id, None)
            .await
            .unwrap(),
        StartOutcome::Started { .. }
    ));
    let review = fixture.wait_for_state(RunState::AwaitingReview).await;
    assert_ne!(review.state, RunState::Done);

    assert!(
        fixture
            .coordinator
            .publish(&run.snapshot.run_id)
            .await
            .is_err()
    );
    assert_eq!(fixture.publication.calls.load(Ordering::SeqCst), 0);
}

struct Fixture {
    _paths: TempDir,
    coordinator: CodingAgentCoordinator,
    snapshot: AgentRunSnapshot,
    publication: Arc<CountingPublication>,
    with_contract: bool,
}

impl Fixture {
    fn new(purpose: RunPurpose, with_contract: bool, report: Option<Vec<u8>>) -> Self {
        Self::new_with_validation(purpose, with_contract, report, ValidationState::Passed)
    }

    fn new_with_validation(
        purpose: RunPurpose,
        with_contract: bool,
        report: Option<Vec<u8>>,
        validation_state: ValidationState,
    ) -> Self {
        let snapshot = snapshot(purpose);
        let report = report.map(|bytes| {
            let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                return bytes;
            };
            if !value.is_object() {
                return bytes;
            }
            value["run_id"] = snapshot.run_id.to_string().into();
            serde_json::to_vec(&value).unwrap()
        });
        let execution = Arc::new(FinalizationExecution {
            observation: Mutex::new(finalization(report, validation_state)),
        });
        let publication = Arc::new(CountingPublication {
            calls: AtomicUsize::new(0),
        });
        let paths = tempfile::tempdir().unwrap();
        let store =
            Arc::new(RunStore::open(&DataPaths::with_root(paths.path().join("app-data"))).unwrap());
        let coordinator =
            CodingAgentCoordinator::new(store, Some(execution), Some(publication.clone()));
        Self {
            _paths: paths,
            coordinator,
            snapshot,
            publication,
            with_contract,
        }
    }

    async fn prepare_and_approve(&self) -> AgentRunRecord {
        let prepared = if self.with_contract {
            let contract = assessment_contract();
            self.coordinator
                .prepare_run_with_assessment_contract(self.snapshot.clone(), contract)
                .await
                .unwrap()
        } else {
            self.coordinator
                .prepare_run(self.snapshot.clone())
                .await
                .unwrap()
        };
        let lookback_tauri_lib::coding_agent::PreparationOutcome::Ready(run) = prepared else {
            panic!("test runtime should be accepted")
        };
        self.coordinator.approve_run(&run.snapshot.run_id).unwrap()
    }

    async fn prepare_and_approve_with_contract(
        &self,
        contract: ApprovalAssessmentContract,
    ) -> AgentRunRecord {
        let lookback_tauri_lib::coding_agent::PreparationOutcome::Ready(run) = self
            .coordinator
            .prepare_run_with_assessment_contract(self.snapshot.clone(), contract)
            .await
            .unwrap()
        else {
            panic!("test runtime should be accepted")
        };
        self.coordinator.approve_run(&run.snapshot.run_id).unwrap()
    }

    async fn wait_for_state(&self, expected: RunState) -> AgentRunRecord {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let run = self
                    .coordinator
                    .get_run(&self.snapshot.run_id)
                    .unwrap()
                    .unwrap();
                if run.state == expected {
                    return run;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }
}

fn snapshot(purpose: RunPurpose) -> AgentRunSnapshot {
    let repository = RepositoryIdentity::new("forge.example", "org", "repo").unwrap();
    let image_digest = format!("sha256:{}", "a".repeat(64));
    AgentRunSnapshot {
        run_id: RunId::new(format!("run-finalization-{}", rand_suffix())).unwrap(),
        issue_id: "issue-17".into(),
        issue_revision: 3,
        execution_mode: ExecutionMode::OneShot,
        purpose,
        repository: ResolvedRepository {
            identity: repository.clone(),
            forge: ForgeKind::Gitea,
            api_base_url: "https://forge.example/api/v1".into(),
            clone_url: "https://forge.example/org/repo".into(),
            read_credential_ref: None,
            publication_credential_ref: None,
            provenance: RepositoryProvenance {
                source_labels: vec!["repo:forge.example/org/repo".into()],
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
        workspace_branch: "agent/finalization".into(),
        semantic_judge_enabled: true,
        auto_done: true,
        approval: Some(lookback_tauri_lib::coding_agent::RunApproval {
            approved_by: "reviewer".into(),
            approved_at_ms: 1,
            auto_done_approved: true,
            settings_version: Some(1),
            applicable_modes: vec![ExecutionMode::OneShot],
        }),
        allowed_operations: vec![AllowedOperation::Verify, AllowedOperation::PublishBranch],
        publication_plan: PublicationPlan::PushBranch,
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
    }
}

fn assessment_contract() -> ApprovalAssessmentContract {
    ApprovalAssessmentContract::from_issue_acceptance(
        RunPurpose::Implementation,
        vec!["The accepted behavior is implemented.".into()],
        vec![ApprovalCriterionPolicy {
            required: true,
            method: CriterionEvaluationMethod::SemanticJudge,
        }],
        vec![ApprovalValidationDraft {
            required: true,
            command: None::<ApprovalAssessmentCommand>,
        }],
        Some(ProviderSelection {
            provider_id: "provider".into(),
            model_id: "model".into(),
            model_version: "v1".into(),
            config_version: "config-v1".into(),
        }),
        Some("coding-agent-judge-v1".into()),
        AssessmentInputScope {
            allowed_fields: vec![ApprovedJudgeInputField::Report],
            allowed_sources: vec![AllowedEvidenceSource {
                kind: EvidenceSourceKind::Report,
                identity: "report:attempt-1".into(),
                max_bytes: 64 * 1024,
            }],
            max_input_bytes: 64 * 1024,
        },
    )
    .unwrap()
}

fn semantic_contract_without_validation() -> ApprovalAssessmentContract {
    ApprovalAssessmentContract::from_issue_acceptance(
        RunPurpose::Implementation,
        vec!["The accepted behavior is implemented.".into()],
        vec![ApprovalCriterionPolicy {
            required: true,
            method: CriterionEvaluationMethod::SemanticJudge,
        }],
        Vec::new(),
        Some(ProviderSelection {
            provider_id: "provider".into(),
            model_id: "model".into(),
            model_version: "v1".into(),
            config_version: "config-v1".into(),
        }),
        Some("coding-agent-judge-v1".into()),
        AssessmentInputScope {
            allowed_fields: vec![ApprovedJudgeInputField::Report],
            allowed_sources: vec![AllowedEvidenceSource {
                kind: EvidenceSourceKind::Report,
                identity: "report:attempt-1".into(),
                max_bytes: 64 * 1024,
            }],
            max_input_bytes: 64 * 1024,
        },
    )
    .unwrap()
}

fn report_bytes(status: &str, with_contract: bool) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema_version": 1,
        "run_id": "placeholder",
        "attempt_id": "attempt-1",
        "task_kind": "implementation",
        "status": status,
        "summary": "Work was attempted.",
        "acceptance": if with_contract { json!([{
            "criterion_id": "AC-1",
            "assessment": "unknown",
            "evidence": ["report:attempt-1"]
        }]) } else { json!([]) },
        "validation": if with_contract { json!([{
            "validation_id": "V-1",
            "claimed_result": "not_run",
            "evidence": [],
            "reason": "No durable host validation receipt was supplied."
        }]) } else { json!([]) },
        "investigation": null,
        "implementation": {
            "change_summary": "Work was attempted.",
            "changed_files": [],
            "implementation_notes": []
        },
        "unresolved": []
    }))
    .unwrap()
}

fn report_bytes_without_validation(status: &str) -> Vec<u8> {
    let mut report: serde_json::Value =
        serde_json::from_slice(&report_bytes(status, true)).unwrap();
    report["validation"] = json!([]);
    serde_json::to_vec(&report).unwrap()
}

struct FixedClock;

impl AssessmentClock for FixedClock {
    fn now_unix_ms(&self) -> u64 {
        1_000
    }
}

struct PassingJudge(Arc<AtomicUsize>);

impl JudgeProvider for PassingJudge {
    fn complete_tool_free(
        &mut self,
        request: &JudgeRequest<'_>,
    ) -> Result<Vec<u8>, JudgeProviderError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let input: serde_json::Value = serde_json::from_slice(request.input_bytes()).unwrap();
        Ok(serde_json::to_vec(&json!({
            "schema_version": 1,
            "assessment_id": request.assessment_id(),
            "run_id": input["run_id"],
            "attempt_id": input["attempt_id"],
            "input_digest": request.input_digest(),
            "criteria": [{
                "criterion_id": "AC-1",
                "result": "PASS",
                "reason": "The approved evidence supports the criterion.",
                "evidence_refs": ["report:attempt-1"]
            }],
            "overall": "PASS"
        }))
        .unwrap())
    }

    fn lookup_receipt(
        &mut self,
        _assessment_id: &str,
        _input_digest: &str,
        _idempotency_key: &str,
    ) -> Result<ReceiptLookup, JudgeProviderError> {
        Ok(ReceiptLookup::Unsupported)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn finalization(
    report: Option<Vec<u8>>,
    validation_state: ValidationState,
) -> HostFinalizationObservation {
    HostFinalizationObservation {
        invocation_kind: ExecutionInvocationKind::DirectProcess,
        cli_exit: ProcessExitObservation::ExitedNormally { exit_code: 0 },
        shell_observation: ShellProcessObservation::NotSpawned,
        child_writers: ChildWritersStatus::Stopped,
        workspace_freeze: WorkspaceFreezeStatus::HostVerified {
            commit_id: "b".repeat(40),
        },
        report,
        validation: ValidationEvidence {
            required: true,
            state: validation_state,
        },
        acceptance_conditions_met: true,
    }
}

fn runtime_observation(snapshot: &AgentRunSnapshot) -> RuntimeObservation {
    RuntimeObservation {
        runtime_instance_id: "runtime-1".into(),
        health_observation_id: "health-1".into(),
        worker_name: "agent-sandbox".into(),
        worker_id: 41,
        runner_name: "WORKFLOW".into(),
        method_name: "run".into(),
        arguments_schema_sha256: "a".repeat(64),
        result_schema_sha256: "b".repeat(64),
        response_mode: "DIRECT".into(),
        output_is_streaming: true,
        accepts_client_stream: false,
        image_digest: snapshot.profile.image_digest.clone(),
        admitted_resources: snapshot.profile.resources,
        host_limits: snapshot.profile.validated_host_limits,
        helper_version: "1.18.3".into(),
        agent_cli_version: "2.0.24".into(),
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
        ]),
        workflow_execution_id: "workflow-41".into(),
        parent_job_id: "parent-41".into(),
        child_sandbox_job_id: "child-41".into(),
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
    WorkspaceReceipt {
        run_id: request.snapshot.run_id.to_string(),
        attempt_id: request.attempt_id.clone(),
        workspace_token: "workspace-1".into(),
        repository_host: request.snapshot.repository.identity.host.clone(),
        repository_owner: request.snapshot.repository.identity.organization.clone(),
        repository_name: request.snapshot.repository.identity.repository.clone(),
        base_commit: request.snapshot.base_commit.clone(),
        branch: request.snapshot.workspace_branch.clone(),
    }
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}
