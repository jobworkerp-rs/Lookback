//! Typed app-lifetime coordinator for durable coding-agent runs.
//!
//! UI code receives snapshots, statuses, evidence references, and bounded
//! terminal feeds. Durable mutations and remote side effects stay behind this
//! module and its trusted adapters.

mod reassessment;
pub use reassessment::SemanticReassessmentRecord;

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    sync::{Mutex as AsyncMutex, Semaphore},
    task::JoinHandle,
};

use super::assessment::{
    ApprovedAssessmentPolicy, ApprovedCriterion, ApprovedValidationDefinition,
    AssessmentApprovalSnapshot, AssessmentAuthorization, AssessmentEngine, AssessmentOutcome,
    AssessmentPreparation, AssessmentUse, CriterionMethod, EvidencePayload, EvidenceSourceKind,
    ProviderSelection, SystemAssessmentClock, ValidationReceiptBinding, prepare_assessment,
};
use super::judge_provider::{ApprovedJudgeProviderProfile, assess_claimed_with_jobworkerp};
use super::model::{
    AssessmentIntentClaim, AssessmentIntentRequest, ExecutionInvocationKind,
    ShellProcessObservation,
};
use super::report::{
    AgentReportStatus, CompletionAssessment, ReportExpectation, ReportRejection, TaskKind,
    ValidatedReport, validate_report,
};
use super::store::{FinalizationCaptureInputs, HostVerifiedFinalizationInputs};
use super::validation::{ValidationOutcome as HostValidationOutcome, ValidationTargetSnapshot};
use super::{
    AgentRunRecord, AgentRunSnapshot, CancellationObservation, ChildWritersStatus,
    CodingAgentError, CompletionGateDecision, DurableOperationOutcome, DurableOperationRequest,
    DurableOperationResult, DurableOperationStatus, EvidenceReference, ExecutionMode, GateDecision,
    InteractiveFinalizationEvidence, ProcessExitObservation, PublicationPlan,
    RunEvidenceAssessment, RunId, RunPurpose, RunState, RunStore, StartAttemptResult,
    ValidationEvidence, ValidationState, can_transition, evaluate_completion_gates,
    execution::{
        ExecutionAdapter, ExecutionEvent, ExecutionIdentity, HostFinalizationObservation,
        ImmutableExecutionIntent, InteractiveFinishObservation, OperationReconciliation,
        PromptDelivery, StartReceipt, TypedFinalizationEvidence, VerifiedFinalization,
        VerifiedFinalizationSources, WorkspaceReceipt, WorkspaceRequest,
        validate_runtime_observation,
    },
    model::{ApprovalAssessmentContract, ApprovalAssessmentContractStatus},
    publication::{
        PublicationAdapter, PublicationOutcome, PublicationRequest, PublicationSupport,
        validate_publication_support,
    },
    terminal::{InputDelivery, PromptStatus, TerminalError, TerminalEvent, TerminalOwnerRegistry},
};

const PROMPT_MEDIA_TYPE: &str = "text/plain";
const JSON_MEDIA_TYPE: &str = "application/json";
const STDOUT_MEDIA_TYPE: &str = "application/octet-stream";
const DEFAULT_INPUT_LIMIT: usize = 64 * 1024;
const CAPTURE_RECONCILE_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub struct CoordinatorConfig {
    pub max_parallel_runs: usize,
    pub max_capture_bytes_per_stream: usize,
    pub terminal_feed_capacity: usize,
    pub max_input_bytes: usize,
    pub finish_timeout: Duration,
}

/// Durable engine and non-secret jobworkerp/provider configuration for
/// coordinator-owned, approval-claimed semantic assessment.
#[derive(Clone)]
pub struct ClaimedAssessmentConfig {
    pub engine: Arc<AssessmentEngine>,
    pub jobworkerp_endpoint: String,
    pub judge_settings: crate::commands::llm_settings::LlmSettings,
    pub timeout: Duration,
}

impl ClaimedAssessmentConfig {
    pub fn new(
        engine: Arc<AssessmentEngine>,
        jobworkerp_endpoint: impl Into<String>,
        judge_settings: crate::commands::llm_settings::LlmSettings,
        timeout: Duration,
    ) -> Self {
        Self {
            engine,
            jobworkerp_endpoint: jobworkerp_endpoint.into(),
            judge_settings,
            timeout,
        }
    }
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            max_parallel_runs: 1,
            max_capture_bytes_per_stream: 8 * 1024 * 1024,
            terminal_feed_capacity: 128,
            max_input_bytes: DEFAULT_INPUT_LIMIT,
            finish_timeout: Duration::from_secs(20),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreparationOutcome {
    Ready(Box<AgentRunRecord>),
    Disabled { blockers: Vec<String> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartOutcome {
    Started {
        attempt_id: String,
        identity: ExecutionIdentity,
        prompt_delivery: Option<PromptDelivery>,
    },
    Reconciled {
        attempt_id: String,
        state: RunState,
        /// Present only when this coordinator already owns the active stream
        /// and the accepted reconciliation exactly matches its receipt.
        receipt: Option<Box<StartReceipt>>,
    },
    Unknown {
        attempt_id: String,
    },
    Disabled {
        blockers: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunReconciliation {
    Unchanged(AgentRunRecord),
    Accepted(AgentRunRecord),
    Unknown(AgentRunRecord),
}

#[derive(Debug, Error)]
pub enum CoordinatorError {
    #[error(transparent)]
    Store(#[from] CodingAgentError),
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    #[error("coding-agent run conflicts with its immutable snapshot or current state")]
    Conflict,
    #[error("coding-agent adapter is unavailable")]
    AdapterUnavailable,
    #[error("coding-agent operation is not supported by verified host capabilities")]
    Unsupported,
    #[error("coding-agent background operation failed")]
    BackgroundFailure,
}

struct CoordinatorInner {
    store: Arc<RunStore>,
    execution: Option<Arc<dyn ExecutionAdapter>>,
    publication: Option<Arc<dyn PublicationAdapter>>,
    automatic_publication: bool,
    verified_finalization: Option<Arc<dyn VerifiedFinalizationSources>>,
    claimed_assessment: Option<ClaimedAssessmentConfig>,
    config: CoordinatorConfig,
    slots: Arc<Semaphore>,
    terminal_owners: TerminalOwnerRegistry,
    run_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    tasks: Mutex<HashMap<String, JoinHandle<()>>>,
    identities: Mutex<HashMap<String, ExecutionIdentity>>,
    captures: Mutex<HashMap<String, CapturedOutput>>,
}

#[derive(Clone)]
pub struct CodingAgentCoordinator {
    inner: Arc<CoordinatorInner>,
}

#[derive(Clone, Debug)]
struct CapturedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
    end_present: bool,
    stream_error: bool,
    capture_complete: bool,
    stdout_reference: Option<String>,
    stderr_reference: Option<String>,
}

struct ExecutionMaintenanceGuard {
    owner: Option<(Arc<dyn ExecutionAdapter>, ExecutionIdentity)>,
}

impl Drop for ExecutionMaintenanceGuard {
    fn drop(&mut self) {
        if let Some((execution, identity)) = self.owner.take() {
            tokio::spawn(async move {
                execution.execution_finished(&identity).await;
            });
        }
    }
}

struct RunLockLease {
    lock: Arc<AsyncMutex<()>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl RunLockLease {
    async fn acquire(lock: Arc<AsyncMutex<()>>) -> Self {
        let guard = Arc::clone(&lock).lock_owned().await;
        Self {
            lock,
            guard: Some(guard),
        }
    }

    fn release(&mut self) {
        drop(self.guard.take());
    }

    async fn reacquire(&mut self) {
        if self.guard.is_none() {
            self.guard = Some(Arc::clone(&self.lock).lock_owned().await);
        }
    }
}

struct SemanticEvaluation {
    assessment_id: Option<String>,
    judge_input_digest: Option<String>,
    judge_input_scope_digest: Option<String>,
    outcome: serde_json::Value,
    acceptance_conditions_met: bool,
    claim_revision: Option<u64>,
}

impl SemanticEvaluation {
    fn inconclusive(reason: &str) -> Self {
        Self {
            assessment_id: None,
            judge_input_digest: None,
            judge_input_scope_digest: None,
            outcome: serde_json::json!({"status":"inconclusive", "reason":reason}),
            acceptance_conditions_met: false,
            claim_revision: None,
        }
    }

    fn not_required() -> Self {
        Self {
            assessment_id: None,
            judge_input_digest: None,
            judge_input_scope_digest: None,
            outcome: serde_json::json!({"status":"not_required"}),
            acceptance_conditions_met: false,
            claim_revision: None,
        }
    }

    fn with_claim_revision(mut self, revision: u64) -> Self {
        self.claim_revision = Some(revision);
        self
    }

    fn is_pending(&self) -> bool {
        self.outcome.get("pending").is_some()
            || self
                .outcome
                .get("status")
                .and_then(serde_json::Value::as_str)
                == Some("pending")
    }
}

#[derive(Deserialize)]
struct CaptureStatusEvidence {
    version: u32,
    attempt_id: String,
    end_present: bool,
    stream_error: bool,
    capture_complete: bool,
    exit_code: Option<i32>,
    stdout_bytes: usize,
    stderr_bytes: usize,
    stdout_reference: Option<String>,
    stderr_reference: Option<String>,
}

#[derive(Clone, Copy)]
enum CaptureRetirement {
    Continue,
    Superseded,
    Cancelled,
    HostQuiesced,
}

struct AssessmentInput<'a> {
    capture: &'a CapturedOutput,
    report: Option<&'a [u8]>,
    persist_report: bool,
    freeze: &'a super::WorkspaceFreezeStatus,
    host_validation: &'a ValidationEvidence,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ReportFinalizationEvidenceV1 {
    schema_version: u32,
    run_id: String,
    attempt_id: String,
    purpose: RunPurpose,
    task_kind: TaskKind,
    disposition: String,
    rejection: Option<String>,
    raw_evidence_id: Option<String>,
    raw_bytes_observed: usize,
    raw_bytes_saved: usize,
    raw_bytes_complete: bool,
    canonical_evidence_id: Option<String>,
    canonical_sha256: Option<String>,
    agent_status: Option<AgentReportStatus>,
    required_acceptance_not_met_or_unknown: Vec<String>,
    required_validation_claims_not_passed: Vec<String>,
    semantic_assessment: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VerifiedArtifactEvidenceV1 {
    schema_version: u32,
    run_id: String,
    attempt_id: String,
    resolved_artifact: coding_agent_executor::ResolvedArtifact,
    full_commit_id: String,
    full_tree_id: String,
    untracked_manifest_digest: String,
    target_snapshot_digest: String,
    validation_receipt_digests: Vec<String>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SemanticAssessmentEvidenceV1 {
    schema_version: u32,
    run_id: String,
    attempt_id: String,
    assessment_id: Option<String>,
    judge_input_digest: Option<String>,
    judge_input_scope_digest: Option<String>,
    outcome: serde_json::Value,
    acceptance_conditions_met: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VerifiedFinalizationAssessmentBaseV1 {
    schema_version: u32,
    run_id: String,
    attempt_id: String,
    finalization_inputs_evidence_id: String,
    assessment: RunEvidenceAssessment,
    capture_complete: bool,
    invocation_kind: ExecutionInvocationKind,
    cli_exit: ProcessExitObservation,
    shell_observation: ShellProcessObservation,
    child_writers: ChildWritersStatus,
    workspace_freeze: super::WorkspaceFreezeStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PreparedClaimMetadataV1 {
    schema_version: u32,
    run_id: String,
    attempt_id: String,
    assessment_id: String,
    finalization_inputs_evidence_id: String,
    deadline_unix_ms: u64,
    judge_input_digest: String,
    system_policy_digest: String,
}

struct AssessmentEvidenceReferences<'a> {
    artifact: &'a EvidenceReference,
    finalization_inputs: &'a EvidenceReference,
    snapshot_manifest: &'a EvidenceReference,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct HostFinalizationEvidenceV2 {
    schema_version: u32,
    run_id: String,
    attempt_id: String,
    invocation_kind: ExecutionInvocationKind,
    cli_exit: ProcessExitObservation,
    shell_observation: ShellProcessObservation,
    child_writers: ChildWritersStatus,
    workspace_freeze: super::WorkspaceFreezeStatus,
    validation_observation: ValidationEvidence,
    acceptance_conditions_met_observation: bool,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct InteractiveHostClaimObservationV1 {
    schema_version: u32,
    run_id: String,
    attempt_id: String,
    validation_observation: ValidationEvidence,
    validation_assessment: ValidationEvidence,
    acceptance_conditions_met_observation: bool,
}

fn missing_capture() -> CapturedOutput {
    CapturedOutput {
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit_code: None,
        end_present: false,
        stream_error: true,
        capture_complete: false,
        stdout_reference: None,
        stderr_reference: None,
    }
}

fn typed_finalization_matches_observation(
    typed: &TypedFinalizationEvidence,
    observation: &HostFinalizationObservation,
) -> bool {
    typed.invocation_kind == observation.invocation_kind
        && typed.cli_exit == observation.cli_exit
        && typed.shell_observation == observation.shell_observation
        && typed.child_writers == observation.child_writers
        && typed.workspace_freeze == observation.workspace_freeze
}

fn interactive_as_host_observation(
    observation: &InteractiveFinishObservation,
) -> HostFinalizationObservation {
    HostFinalizationObservation {
        invocation_kind: observation.invocation_kind,
        cli_exit: observation.cli_exit.clone(),
        shell_observation: observation.shell_observation.clone(),
        child_writers: observation.child_writers,
        workspace_freeze: observation.workspace_freeze.clone(),
        report: observation.report.clone(),
        validation: observation.validation.clone(),
        acceptance_conditions_met: observation.acceptance_conditions_met,
    }
}

fn typed_physical_finalization_succeeded(
    observation: &HostFinalizationObservation,
    capture: &CapturedOutput,
    verified: &VerifiedFinalization,
) -> bool {
    observation.cli_exit == ProcessExitObservation::ExitedNormally { exit_code: 0 }
        && observation
            .shell_observation
            .is_successful_for(observation.invocation_kind)
        && observation.child_writers == ChildWritersStatus::Stopped
        && capture.end_present
        && capture.capture_complete
        && !capture.stream_error
        && capture.exit_code == Some(0)
        && matches!(
            &observation.workspace_freeze,
            super::WorkspaceFreezeStatus::HostVerified { commit_id }
                if commit_id == &verified.artifact.commit
        )
}

fn verified_validation_state(
    verified: &VerifiedFinalization,
    contract: &ApprovalAssessmentContract,
) -> ValidationState {
    if verified
        .validation_runs
        .iter()
        .flat_map(|run| &run.records)
        .any(|record| {
            record.outcome() == HostValidationOutcome::Failed
                || record
                    .receipt()
                    .is_some_and(|receipt| receipt.outcome() == HostValidationOutcome::Failed)
        })
    {
        return ValidationState::Failed;
    }
    let required = contract
        .validation_plan
        .iter()
        .filter(|definition| definition.required)
        .collect::<Vec<_>>();
    if required.is_empty()
        || required.iter().any(|definition| {
            verified
                .validation_runs
                .iter()
                .find(|run| run.plan.validation_id() == definition.validation_id)
                .is_none_or(|run| {
                    run.records.len() != 1
                        || run.records[0].outcome() != HostValidationOutcome::Passed
                        || run.records[0].receipt().is_none_or(|receipt| {
                            receipt.outcome() != HostValidationOutcome::Passed
                        })
                })
        })
    {
        ValidationState::NotRun
    } else {
        ValidationState::Passed
    }
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn semantic_completion_conditions_met(
    criteria: &[super::model::ApprovedAcceptanceCriterion],
    report: Option<&ValidatedReport>,
    overall: AssessmentUse,
) -> bool {
    let required_are_judgeable = !criteria.is_empty()
        && criteria
            .iter()
            .filter(|criterion| criterion.required)
            .all(|criterion| {
                criterion.method == super::model::CriterionEvaluationMethod::SemanticJudge
            });
    let report_claims_consistent = report.is_some_and(|report| {
        report.report.status == AgentReportStatus::Completed
            && report.assessment.completion != CompletionAssessment::Inconsistent
    });
    required_are_judgeable && report_claims_consistent && overall == AssessmentUse::Pass
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CancellationIntent {
    run_id: String,
    attempt_id: String,
    requested: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CancellationReceipt {
    run_id: String,
    attempt_id: String,
    observation: CancellationObservation,
}

impl CodingAgentCoordinator {
    pub fn new(
        store: Arc<RunStore>,
        execution: Option<Arc<dyn ExecutionAdapter>>,
        publication: Option<Arc<dyn PublicationAdapter>>,
    ) -> Self {
        Self::with_config_and_adapters(
            store,
            execution,
            publication,
            None,
            None,
            false,
            CoordinatorConfig::default(),
        )
    }

    pub fn with_config(self, config: CoordinatorConfig) -> Self {
        Self::with_config_and_adapters(
            self.inner.store.clone(),
            self.inner.execution.clone(),
            self.inner.publication.clone(),
            self.inner.verified_finalization.clone(),
            self.inner.claimed_assessment.clone(),
            self.inner.automatic_publication,
            config,
        )
    }

    pub fn with_publication_adapter(self, publication: Arc<dyn PublicationAdapter>) -> Self {
        Self::with_config_and_adapters(
            self.inner.store.clone(),
            self.inner.execution.clone(),
            Some(publication),
            self.inner.verified_finalization.clone(),
            self.inner.claimed_assessment.clone(),
            self.inner.automatic_publication,
            self.inner.config.clone(),
        )
    }

    /// Publish through the registered workflow before releasing Issue ownership.
    pub fn with_workflow_publication_adapter(
        self,
        publication: Arc<dyn PublicationAdapter>,
    ) -> Self {
        Self::with_config_and_adapters(
            self.inner.store.clone(),
            self.inner.execution.clone(),
            Some(publication),
            self.inner.verified_finalization.clone(),
            self.inner.claimed_assessment.clone(),
            true,
            self.inner.config.clone(),
        )
    }

    /// Enable the native finalization verifier. This dependency is absent by
    /// default, so older hosts retain their existing fail-closed C1 behavior.
    pub fn with_verified_finalization(self, sources: Arc<dyn VerifiedFinalizationSources>) -> Self {
        Self::with_config_and_adapters(
            self.inner.store.clone(),
            self.inner.execution.clone(),
            self.inner.publication.clone(),
            Some(sources),
            self.inner.claimed_assessment.clone(),
            self.inner.automatic_publication,
            self.inner.config.clone(),
        )
    }

    /// Configure an explicit native LLM provider for coordinator-claimed
    /// semantic assessments. Provider dispatch still requires a new RunStore
    /// claim and an exact frozen prepared payload.
    pub fn with_claimed_assessment(self, config: ClaimedAssessmentConfig) -> Self {
        Self::with_config_and_adapters(
            self.inner.store.clone(),
            self.inner.execution.clone(),
            self.inner.publication.clone(),
            self.inner.verified_finalization.clone(),
            Some(config),
            self.inner.automatic_publication,
            self.inner.config.clone(),
        )
    }

    fn with_config_and_adapters(
        store: Arc<RunStore>,
        execution: Option<Arc<dyn ExecutionAdapter>>,
        publication: Option<Arc<dyn PublicationAdapter>>,
        verified_finalization: Option<Arc<dyn VerifiedFinalizationSources>>,
        claimed_assessment: Option<ClaimedAssessmentConfig>,
        automatic_publication: bool,
        config: CoordinatorConfig,
    ) -> Self {
        let permits = config.max_parallel_runs.max(1);
        Self {
            inner: Arc::new(CoordinatorInner {
                store,
                execution,
                publication,
                automatic_publication,
                verified_finalization,
                claimed_assessment,
                config,
                slots: Arc::new(Semaphore::new(permits)),
                terminal_owners: TerminalOwnerRegistry::default(),
                run_locks: Mutex::new(HashMap::new()),
                tasks: Mutex::new(HashMap::new()),
                identities: Mutex::new(HashMap::new()),
                captures: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Validate live host capability evidence before creating a durable run.
    /// Unsupported configurations are returned as a typed Disabled outcome;
    /// they never reach workspace or job dispatch.
    pub async fn prepare_run(
        &self,
        snapshot: AgentRunSnapshot,
    ) -> Result<PreparationOutcome, CoordinatorError> {
        self.prepare_run_inner(snapshot, None).await
    }

    /// Persist the exact assessment policy that was approved with this run.
    /// The legacy entry point above intentionally leaves that policy absent.
    pub async fn prepare_run_with_assessment_contract(
        &self,
        snapshot: AgentRunSnapshot,
        contract: ApprovalAssessmentContract,
    ) -> Result<PreparationOutcome, CoordinatorError> {
        self.prepare_run_inner(snapshot, Some(contract)).await
    }

    async fn prepare_run_inner(
        &self,
        snapshot: AgentRunSnapshot,
        assessment_contract: Option<ApprovalAssessmentContract>,
    ) -> Result<PreparationOutcome, CoordinatorError> {
        let Some(execution) = &self.inner.execution else {
            return Ok(PreparationOutcome::Disabled {
                blockers: vec!["execution_adapter_unavailable".into()],
            });
        };
        let observation = match execution.preflight(&snapshot).await {
            Ok(observation) => observation,
            Err(_) => {
                return Ok(PreparationOutcome::Disabled {
                    blockers: vec!["live_runtime_preflight_unavailable".into()],
                });
            }
        };
        let mut blockers = validate_runtime_observation(&snapshot, &observation, false);
        if snapshot.publication_plan != PublicationPlan::NoPublication {
            match &self.inner.publication {
                Some(adapter) => match adapter.preflight(&snapshot).await {
                    Ok(support) => {
                        blockers.extend(validate_publication_support(&snapshot, &support))
                    }
                    Err(_) => blockers.push("publication_preflight_unavailable".into()),
                },
                None => blockers.push("publication_adapter_unavailable".into()),
            }
        }
        blockers.sort();
        blockers.dedup();
        if !blockers.is_empty() {
            return Ok(PreparationOutcome::Disabled { blockers });
        }
        let created = match assessment_contract {
            Some(contract) => self
                .inner
                .store
                .create_run_with_approval_contract(snapshot, contract)?,
            None => self.inner.store.create_run(snapshot)?,
        };
        Ok(PreparationOutcome::Ready(Box::new(created)))
    }

    /// Explicit approval binds to the approval record already frozen in the
    /// run snapshot. This coordinator never reads mutable settings to grant it.
    pub fn approve_run(&self, run_id: &RunId) -> Result<AgentRunRecord, CoordinatorError> {
        let run = self.require_run(run_id)?;
        if run.state != RunState::ApprovalPending || run.snapshot.approval.is_none() {
            return Err(CoordinatorError::Conflict);
        }
        Ok(self
            .inner
            .store
            .transition(run_id, run.revision, RunState::Queued)?)
    }

    pub async fn start(
        &self,
        run_id: &RunId,
        prompt: Option<Vec<u8>>,
    ) -> Result<StartOutcome, CoordinatorError> {
        let run = self.require_run(run_id)?;
        self.start_attempt(run_id, run.snapshot.primary_attempt_id(), prompt)
            .await
    }

    /// Persist workspace and StartAgent intents before external side effects.
    /// Existing intents are reconciled and are never dispatched a second time.
    pub async fn start_attempt(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        prompt: Option<Vec<u8>>,
    ) -> Result<StartOutcome, CoordinatorError> {
        let lock = self.run_lock(run_id);
        let _guard = lock.lock().await;
        let Some(execution) = &self.inner.execution else {
            return Ok(StartOutcome::Disabled {
                blockers: vec!["execution_adapter_unavailable".into()],
            });
        };
        let run = self.require_run(run_id)?;
        if run.snapshot.approval.is_none()
            || !run
                .snapshot
                .attempt_ids
                .iter()
                .any(|candidate| candidate == attempt_id)
            || !matches!(
                run.state,
                RunState::Queued | RunState::Preparing | RunState::Running | RunState::Unknown
            )
        {
            return Err(CoordinatorError::Conflict);
        }
        let start_key = operation_key("start", attempt_id);
        let existing_start = self.inner.store.operation(run_id, &start_key)?;
        if existing_start.is_none() && matches!(run.state, RunState::Running | RunState::Unknown) {
            return Err(CoordinatorError::Conflict);
        }
        let prompt_requested = prompt.is_some() && existing_start.is_none();
        let observation = match execution.preflight(&run.snapshot).await {
            Ok(observation) => observation,
            Err(_) => {
                return Ok(StartOutcome::Disabled {
                    blockers: vec!["live_runtime_preflight_unavailable".into()],
                });
            }
        };
        let blockers = validate_runtime_observation(&run.snapshot, &observation, prompt_requested);
        if !blockers.is_empty() {
            return Ok(StartOutcome::Disabled { blockers });
        }

        let permit = if existing_start.is_none() {
            Some(
                self.inner
                    .slots
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| CoordinatorError::AdapterUnavailable)?,
            )
        } else {
            None
        };

        let mut current = self.require_run(run_id)?;
        if let Some(existing) = existing_start {
            let Some(intent) = self.read_intent(run_id, attempt_id)? else {
                self.transition_if_allowed(&current, RunState::Unknown)?;
                return Ok(StartOutcome::Unknown {
                    attempt_id: attempt_id.into(),
                });
            };
            let reconciled = execution
                .reconcile_start(&current.snapshot, &intent)
                .await
                .unwrap_or(OperationReconciliation::Unknown(None));
            if current.current_attempt_id.as_deref() != Some(attempt_id) {
                // Legacy intents with no current attempt are reconciled for
                // diagnostics only; they can never gain dispatch permission.
                self.transition_if_allowed(&current, RunState::Unknown)?;
                return Ok(StartOutcome::Unknown {
                    attempt_id: attempt_id.into(),
                });
            }
            return self.finish_start_reconciliation(
                &current.snapshot,
                attempt_id,
                &intent,
                existing,
                reconciled,
            );
        }
        if current.state == RunState::Queued {
            current = self
                .inner
                .store
                .transition(run_id, current.revision, RunState::Preparing)?;
        }
        let workspace_request = WorkspaceRequest {
            snapshot: current.snapshot.clone(),
            attempt_id: attempt_id.into(),
        };
        let workspace = self
            .prepare_or_reconcile_workspace(execution.as_ref(), &workspace_request)
            .await?;
        if workspace.is_none() {
            let latest = self.require_run(run_id)?;
            self.transition_if_allowed(&latest, RunState::Unknown)?;
            return Ok(StartOutcome::Unknown {
                attempt_id: attempt_id.into(),
            });
        }
        let workspace = workspace.expect("checked above");

        let prompt_sha256 = prompt.as_deref().map(sha256_hex);
        let intent = match ImmutableExecutionIntent::build(
            &current.snapshot,
            attempt_id,
            &observation,
            prompt_sha256,
        ) {
            Ok(intent) => intent,
            Err(_) => {
                self.transition_if_allowed(&self.require_run(run_id)?, RunState::Unknown)?;
                return Ok(StartOutcome::Unknown {
                    attempt_id: attempt_id.into(),
                });
            }
        };
        let intent_bytes = serde_json::to_vec(&intent).map_err(|_| CoordinatorError::Conflict)?;
        let intent_digest = sha256_hex(&intent_bytes);

        // start_attempt performs a store-owned, revision-checked single-use
        // grant. The returned latest record is always read before Running.
        let StartAttemptResult {
            operation,
            newly_created,
            run_revision,
            ..
        } = self
            .inner
            .store
            .start_attempt(run_id, current.revision, attempt_id, &start_key)?;
        let latest = self.require_run(run_id)?;
        if latest.current_attempt_id.as_deref() != Some(attempt_id)
            || latest.state != RunState::Preparing
        {
            self.transition_if_allowed(&latest, RunState::Unknown)?;
            return Ok(StartOutcome::Unknown {
                attempt_id: attempt_id.into(),
            });
        }
        if !newly_created {
            let Some(existing_intent) = self.read_intent(run_id, attempt_id)? else {
                self.transition_if_allowed(&latest, RunState::Unknown)?;
                return Ok(StartOutcome::Unknown {
                    attempt_id: attempt_id.into(),
                });
            };
            let reconciled = execution
                .reconcile_start(&latest.snapshot, &existing_intent)
                .await
                .unwrap_or(OperationReconciliation::Unknown(None));
            return self.finish_start_reconciliation(
                &latest.snapshot,
                attempt_id,
                &existing_intent,
                operation,
                reconciled,
            );
        }

        let workspace_bytes =
            serde_json::to_vec(&workspace).map_err(|_| CoordinatorError::Conflict)?;
        if let Err(error) = self.write_unique_evidence(
            run_id,
            attempt_id,
            "workspace-receipt",
            JSON_MEDIA_TYPE,
            &workspace_bytes,
        ) {
            self.complete_unconfirmed(run_id, &start_key, "workspace receipt persistence failed")?;
            self.transition_if_allowed(&self.require_run(run_id)?, RunState::Unknown)?;
            return Err(error);
        }
        let prompt_reference = if let Some(prompt) = prompt.as_deref() {
            Some(self.write_unique_evidence(
                run_id,
                attempt_id,
                "prompt-intent",
                PROMPT_MEDIA_TYPE,
                prompt,
            )?)
        } else {
            None
        };
        let intent_reference = match self.write_unique_evidence(
            run_id,
            attempt_id,
            "dispatch-intent",
            JSON_MEDIA_TYPE,
            &intent_bytes,
        ) {
            Ok(reference) => reference,
            Err(error) => {
                self.complete_unconfirmed(
                    run_id,
                    &start_key,
                    "dispatch intent persistence failed",
                )?;
                self.transition_if_allowed(&self.require_run(run_id)?, RunState::Unknown)?;
                return Err(error);
            }
        };
        let _ = prompt_reference;

        // Store may have advanced revision when consuming the attempt. Never
        // reuse the pre-start revision or trust a response for another attempt.
        let preparing = self.require_run(run_id)?;
        if preparing.current_attempt_id.as_deref() != Some(attempt_id)
            || preparing.state != RunState::Preparing
        {
            self.transition_if_allowed(&preparing, RunState::Unknown)?;
            return Ok(StartOutcome::Unknown {
                attempt_id: attempt_id.into(),
            });
        }
        let prompt_bytes = prompt.clone();
        let receipt = match execution
            .start_attempt(&current.snapshot, &intent, &workspace, prompt_bytes)
            .await
        {
            Ok(receipt) => receipt,
            Err(_) => {
                self.complete_unconfirmed(run_id, &start_key, "runtime acceptance is unknown")?;
                self.transition_if_allowed(&self.require_run(run_id)?, RunState::Unknown)?;
                return Ok(StartOutcome::Unknown {
                    attempt_id: attempt_id.into(),
                });
            }
        };
        let mut maintenance = ExecutionMaintenanceGuard {
            owner: (current.snapshot.execution_mode == ExecutionMode::OneShot)
                .then(|| (execution.clone(), receipt.identity.clone())),
        };
        if !valid_start_receipt(&receipt, &intent, &intent_digest) {
            self.complete_unconfirmed(run_id, &start_key, "runtime identity did not match intent")?;
            self.transition_if_allowed(&self.require_run(run_id)?, RunState::Unknown)?;
            return Ok(StartOutcome::Unknown {
                attempt_id: attempt_id.into(),
            });
        }
        let receipt_bytes = serde_json::to_vec(&receipt).map_err(|_| CoordinatorError::Conflict)?;
        let receipt_reference = self.write_unique_evidence(
            run_id,
            attempt_id,
            "dispatch-receipt",
            JSON_MEDIA_TYPE,
            &receipt_bytes,
        )?;
        self.inner.store.complete_operation(
            run_id,
            &start_key,
            DurableOperationResult {
                outcome: DurableOperationOutcome::Succeeded,
                result_digest: Some(sha256_hex(&receipt_bytes)),
                evidence_ids: vec![intent_reference.evidence_id, receipt_reference.evidence_id],
            },
        )?;
        if self
            .inner
            .store
            .mark_attempt_running(run_id, run_revision, attempt_id)
            .is_err()
        {
            self.transition_if_allowed(&self.require_run(run_id)?, RunState::Unknown)?;
            return Ok(StartOutcome::Unknown {
                attempt_id: attempt_id.into(),
            });
        }
        if let Err(error) = self
            .launch_capture_task(&current.snapshot, &receipt, permit)
            .await
        {
            self.transition_if_allowed(&self.require_run(run_id)?, RunState::Unknown)?;
            return Err(error);
        }
        maintenance.owner = None;
        Ok(StartOutcome::Started {
            attempt_id: attempt_id.into(),
            identity: receipt.identity,
            prompt_delivery: prompt.map(|_| receipt.prompt_delivery),
        })
    }

    pub fn get_run(&self, run_id: &RunId) -> Result<Option<AgentRunRecord>, CoordinatorError> {
        Ok(self.inner.store.get_run(run_id)?)
    }

    pub fn recoverable_runs(&self) -> Result<Vec<AgentRunRecord>, CoordinatorError> {
        Ok(self.inner.store.recoverable_runs()?)
    }

    /// Startup recovery intentionally reports unresolved runs without
    /// approving, attaching, dispatching, or replaying any operation.
    pub fn recover_on_startup(&self) -> Result<Vec<AgentRunRecord>, CoordinatorError> {
        self.recoverable_runs()
    }

    pub fn read_evidence(
        &self,
        run_id: &RunId,
        evidence_id: &str,
    ) -> Result<Vec<u8>, CoordinatorError> {
        Ok(self.inner.store.read_evidence(run_id, evidence_id)?)
    }

    pub fn evidence_references(
        &self,
        run_id: &RunId,
    ) -> Result<Vec<super::EvidenceReference>, CoordinatorError> {
        Ok(self.inner.store.evidence(run_id)?)
    }

    pub fn subscribe_terminal(
        &self,
        run_id: &RunId,
    ) -> Result<tokio::sync::broadcast::Receiver<TerminalEvent>, CoordinatorError> {
        let run = self.require_run(run_id)?;
        let attempt_id = run
            .current_attempt_id
            .as_deref()
            .ok_or(CoordinatorError::Conflict)?;
        Ok(self
            .inner
            .terminal_owners
            .subscribe(run_id.as_str(), attempt_id)?)
    }

    pub async fn send_terminal_input(
        &self,
        run_id: &RunId,
        bytes: Vec<u8>,
    ) -> Result<InputDelivery, CoordinatorError> {
        let run = self.require_run(run_id)?;
        if run.snapshot.execution_mode != ExecutionMode::Interactive
            || run.state != RunState::Running
            || run.interactive_finish_request.is_some()
            || run.current_attempt_id.as_deref().is_some_and(|attempt_id| {
                self.has_cancellation_effect(run_id, attempt_id)
                    .unwrap_or(true)
            })
        {
            return Err(CoordinatorError::Conflict);
        }
        let attempt_id = run
            .current_attempt_id
            .as_deref()
            .ok_or(CoordinatorError::Conflict)?;
        Ok(self
            .inner
            .terminal_owners
            .send_input(
                run_id.as_str(),
                attempt_id,
                bytes,
                self.inner.config.max_input_bytes,
            )
            .await?)
    }

    pub fn terminal_owner_count(&self) -> usize {
        self.inner.terminal_owners.owner_count()
    }

    /// Direct host control RPC. It deliberately does not acquire the run
    /// semaphore or await a jobworkerp workflow slot.
    pub async fn heartbeat(&self, run_id: &RunId) -> Result<(), CoordinatorError> {
        let execution = self
            .inner
            .execution
            .as_ref()
            .ok_or(CoordinatorError::AdapterUnavailable)?;
        let identity = self.identity_for(run_id)?;
        execution
            .heartbeat(&identity)
            .await
            .map_err(|_| CoordinatorError::AdapterUnavailable)
    }

    /// Persist normal interactive completion before closing stdin or asking
    /// the host to stop writers and freeze the workspace.
    pub async fn finish_interactive(
        &self,
        run_id: &RunId,
    ) -> Result<AgentRunRecord, CoordinatorError> {
        let mut run_guard = RunLockLease::acquire(self.run_lock(run_id)).await;
        let execution = self
            .inner
            .execution
            .as_ref()
            .ok_or(CoordinatorError::AdapterUnavailable)?;
        let run = self.require_run(run_id)?;
        let existing_finish = run.interactive_finish_request.is_some();
        if run.snapshot.execution_mode != ExecutionMode::Interactive
            || !(run.state == RunState::Running
                || (existing_finish
                    && matches!(run.state, RunState::Finalizing | RunState::Unknown)))
            || run.current_attempt_id.as_deref().is_some_and(|attempt_id| {
                self.has_cancellation_effect(run_id, attempt_id)
                    .unwrap_or(true)
            })
        {
            return Err(CoordinatorError::Conflict);
        }
        let attempt_id = run
            .current_attempt_id
            .as_deref()
            .ok_or(CoordinatorError::Conflict)?;
        if existing_finish
            && matches!(run.state, RunState::Finalizing | RunState::Unknown)
            && self
                .inner
                .store
                .assessment_intent(run_id, attempt_id)
                .is_ok()
        {
            run_guard.release();
            return self.resume_finalization(run_id).await;
        }
        self.inner
            .store
            .request_interactive_finish(run_id, attempt_id)?;
        let identity = self.identity_for(run_id)?;
        let effect_key = effect_operation_key("finish", attempt_id);
        let claim = self.inner.store.claim_effect(
            run_id,
            attempt_id,
            &effect_key,
            DurableOperationRequest::FinishInteractive {
                profile_version: run.snapshot.profile.version,
            },
        )?;
        let may_dispatch = claim.newly_created && !existing_finish;
        if may_dispatch {
            // EOF is only an input operation, never a finish-success signal.
            let _ = self
                .inner
                .terminal_owners
                .finish_input(run_id.as_str(), attempt_id)
                .await;
        }
        let observation = if !may_dispatch {
            match execution.reconcile_interactive_finish(&identity).await {
                Ok(OperationReconciliation::Accepted(observation)) => observation,
                Ok(OperationReconciliation::NotAccepted | OperationReconciliation::Unknown(_))
                | Err(_) => {
                    self.complete_operation_unconfirmed(run_id, &effect_key)?;
                    return self.transition_attempt_or_current(
                        run_id,
                        attempt_id,
                        &identity,
                        RunState::Unknown,
                    );
                }
            }
        } else {
            match execution.finalize_interactive(&identity).await {
                Ok(observation) => observation,
                Err(_) => {
                    self.complete_operation_unconfirmed(run_id, &effect_key)?;
                    return self.transition_attempt_or_current(
                        run_id,
                        attempt_id,
                        &identity,
                        RunState::Unknown,
                    );
                }
            }
        };
        if self
            .has_cancellation_effect(run_id, attempt_id)
            .unwrap_or(true)
        {
            self.complete_operation_unconfirmed(run_id, &effect_key)?;
            return self.transition_attempt_or_current(
                run_id,
                attempt_id,
                &identity,
                RunState::Unknown,
            );
        }
        let latest = self.require_run(run_id)?;
        if latest.current_attempt_id.as_deref() != Some(attempt_id) || latest.state.is_terminal() {
            return Ok(latest);
        }
        // Another coordinator may have conservatively moved a still-running
        // finish request to Unknown after lookup returned no result. A trusted
        // observation from this request can resume the durable finalization
        // stage before the store accepts attempt-bound evidence.
        if latest.state == RunState::Unknown {
            self.transition_if_allowed(&latest, RunState::Finalizing)?;
        }
        let capture = self.await_capture(run_id, attempt_id).await;
        let prior_validation_failed = latest.current_attempt_id.as_deref() == Some(attempt_id)
            && latest
                .execution_evidence
                .as_ref()
                .is_some_and(|evidence| evidence.validation.state == ValidationState::Failed);
        let host_validation_failed = observation.validation.state == ValidationState::Failed;
        let mut assessment =
            self.assessment_from_interactive(run_id, attempt_id, capture.clone(), &observation)?;
        let contract = match self.inner.store.approval_assessment_contract(run_id)? {
            ApprovalAssessmentContractStatus::Versioned { contract, .. } => Some(*contract),
            ApprovalAssessmentContractStatus::LegacyIncomplete => None,
        };
        let typed_finalization = if let Some(execution) = self.inner.execution.as_ref() {
            match execution.finalization_evidence(&identity).await {
                Ok(typed)
                    if typed.identity == identity
                        && typed_finalization_matches_observation(
                            &typed,
                            &interactive_as_host_observation(&observation),
                        ) =>
                {
                    Some(typed)
                }
                Ok(_) | Err(_) => None,
            }
        } else {
            None
        };
        let verified = if let (Some(sources), Some(contract), Some(typed)) = (
            self.inner.verified_finalization.as_ref(),
            contract.as_ref(),
            typed_finalization.as_ref(),
        ) {
            sources.reopen(&run.snapshot, contract, typed).await.ok()
        } else {
            None
        };
        let verified_artifact_reference = if let Some(verified) = verified.as_ref() {
            self.persist_verified_artifact_evidence(run_id, attempt_id, verified)
                .ok()
        } else {
            None
        };
        let verified_finalization_enabled = self.inner.verified_finalization.is_some();
        if verified_finalization_enabled {
            if let Some(contract) = contract.as_ref() {
                assessment.validation = ValidationEvidence {
                    required: contract
                        .validation_plan
                        .iter()
                        .any(|validation| validation.required),
                    state: verified
                        .as_ref()
                        .map_or(ValidationState::NotRun, |verified| {
                            verified_validation_state(verified, contract)
                        }),
                };
            } else {
                assessment.validation = ValidationEvidence {
                    required: false,
                    state: ValidationState::NotRun,
                };
            }
            if let Some(verified) = verified
                .as_ref()
                .filter(|_| verified_artifact_reference.is_some())
            {
                let frozen_commit = match &observation.workspace_freeze {
                    super::WorkspaceFreezeStatus::HostVerified { commit_id }
                        if commit_id == &verified.artifact.commit =>
                    {
                        Some(commit_id.clone())
                    }
                    _ => None,
                };
                assessment.commit_id = frozen_commit;
                assessment.commit_consistent = assessment.commit_id.is_some();
            } else {
                assessment.commit_id = None;
                assessment.commit_consistent = false;
                assessment.validation.state = ValidationState::NotRun;
            }
        }
        if prior_validation_failed || host_validation_failed {
            // Host validation failure is monotonic within an attempt. A later
            // legacy `Passed` observation has no receipt authority to clear it.
            assessment.validation.state = ValidationState::Failed;
        }
        self.persist_interactive_host_claim_observation(
            run_id,
            attempt_id,
            &observation,
            &assessment,
        )?;
        let proof = InteractiveFinalizationEvidence::from_host_observations_with_invocation(
            run.snapshot.profile.version,
            observation.invocation_kind,
            observation.cli_exit.clone(),
            observation.shell_observation.clone(),
            observation.child_writers,
            observation.workspace_freeze.clone(),
        );
        let host_quiesced = trusted_interactive_host_quiesced(&observation);
        let mut defer_interactive_finalization = run.snapshot.semantic_judge_enabled
            && verified_finalization_enabled
            && self.inner.claimed_assessment.is_some()
            && contract
                .as_ref()
                .is_some_and(ApprovalAssessmentContract::can_dispatch_semantic_assessment)
            && verified.is_some()
            && verified_artifact_reference.is_some()
            && typed_finalization.is_some()
            && assessment.validation.state != ValidationState::Failed;
        let host_stop_verified = proof.is_successful_for(run.snapshot.profile.version, &assessment)
            && (!verified_finalization_enabled
                || (verified.is_some()
                    && verified_artifact_reference.is_some()
                    && verified.as_ref().is_some_and(|verified| {
                        typed_physical_finalization_succeeded(
                            &interactive_as_host_observation(&observation),
                            &capture,
                            verified,
                        )
                    })));
        let report = observation.report.as_deref();
        let report_valid = report.is_some_and(|bytes| {
            self.validate_report_for_run(run_id, attempt_id, bytes)
                .is_ok()
        });
        let complete = host_stop_verified
            && report_valid
            && assessment_execution_is_successful_without_report(&assessment)
            && assessment.validation.state != ValidationState::Failed;
        defer_interactive_finalization &= complete;
        let latest = self.require_run(run_id)?;
        let cancellation_requested = self.has_cancellation_effect(run_id, attempt_id)?;
        if latest.current_attempt_id.as_deref() != Some(attempt_id) {
            return Ok(latest);
        }
        if host_quiesced {
            // Preserve a quiescence proof even when the report is missing or
            // rejected; that evidence lets a same-intent retry reuse capture.
            if !defer_interactive_finalization {
                self.inner.store.record_interactive_finalization(
                    run_id,
                    attempt_id,
                    proof.clone(),
                )?;
            }
            if !cancellation_requested {
                let finalizing = self.require_run(run_id)?;
                if finalizing.current_attempt_id.as_deref() == Some(attempt_id)
                    && finalizing.state == RunState::Unknown
                {
                    let _ = self.inner.store.transition(
                        run_id,
                        finalizing.revision,
                        RunState::Finalizing,
                    );
                }
            }
            // Keep the completed capture/result until canonical assessment is
            // committed. Task retirement and result retention are independent.
            self.retire_capture_owner(&identity, false).await;
        }
        if !complete || cancellation_requested {
            if !cancellation_requested
                && assessment.validation.state == ValidationState::Failed
                && latest.execution_evidence.is_none()
            {
                // Preserve a conclusive host-reported failure even though an
                // interactive failed finish cannot enter the successful
                // Verifying transition. Once recorded, the typed assessment is
                // immutable; later observations remain append-only evidence.
                self.inner.store.record_execution_evidence(
                    run_id,
                    attempt_id,
                    assessment.clone(),
                )?;
            }
            self.persist_finish_report_observation(run_id, attempt_id, report)?;
            self.complete_operation_unconfirmed(run_id, &effect_key)?;
            return self.transition_attempt_or_current(
                run_id,
                attempt_id,
                &identity,
                RunState::Unknown,
            );
        }
        let report = report.expect("valid interactive completion requires a report");
        let Some(report_reference) =
            self.finalize_report_evidence(run_id, attempt_id, Some(report), true)?
        else {
            self.persist_finish_report_observation(run_id, attempt_id, Some(report))?;
            self.complete_operation_unconfirmed(run_id, &effect_key)?;
            return self.transition_attempt_or_current(
                run_id,
                attempt_id,
                &identity,
                RunState::Unknown,
            );
        };
        assessment.report_reference = Some(report_reference);
        assessment.report_save_confirmed = true;
        assessment.evidence_integrity_verified = true;
        let latest = self.require_run(run_id)?;
        if latest.current_attempt_id.as_deref() != Some(attempt_id)
            || self.has_cancellation_effect(run_id, attempt_id)?
        {
            self.complete_operation_unconfirmed(run_id, &effect_key)?;
            return self.transition_attempt_or_current(
                run_id,
                attempt_id,
                &identity,
                RunState::Unknown,
            );
        }
        let proof_bytes = serde_json::to_vec(&proof).map_err(|_| CoordinatorError::Conflict)?;
        let mut evidence_ids = Vec::new();
        evidence_ids.extend(assessment.stdout_reference.iter().cloned());
        evidence_ids.extend(assessment.stderr_reference.iter().cloned());
        evidence_ids.extend(assessment.report_reference.iter().cloned());
        self.inner.store.complete_operation(
            run_id,
            &effect_key,
            DurableOperationResult {
                outcome: DurableOperationOutcome::Succeeded,
                result_digest: Some(sha256_hex(&proof_bytes)),
                evidence_ids,
            },
        )?;
        self.inner
            .terminal_owners
            .publish(run_id.as_str(), attempt_id, TerminalEvent::FeedClosed);
        self.retire_capture_owner(&identity, true).await;
        let latest = self.require_run(run_id)?;
        if latest.current_attempt_id.as_deref() != Some(attempt_id) {
            return Ok(latest);
        }
        let semantic = if verified_finalization_enabled && run.snapshot.semantic_judge_enabled {
            match (
                contract.as_ref(),
                verified.as_ref(),
                verified_artifact_reference.as_ref(),
                typed_finalization.as_ref(),
            ) {
                (Some(contract), Some(verified), Some(artifact_reference), Some(typed)) => {
                    if contract.can_dispatch_semantic_assessment()
                        && assessment.validation.state != ValidationState::Failed
                    {
                        let (finalization_inputs_reference, _, manifest_reference) = match self
                            .persist_verified_finalization_base(
                                run_id,
                                attempt_id,
                                typed,
                                verified,
                                &capture,
                                &assessment,
                            )
                            .await
                        {
                            Ok(references) => references,
                            Err(_) => {
                                return self.transition_attempt_or_current(
                                    run_id,
                                    attempt_id,
                                    &identity,
                                    RunState::Unknown,
                                );
                            }
                        };
                        let latest = self.require_run(run_id)?;
                        self.evaluate_claimed_semantics(
                            &latest,
                            attempt_id,
                            contract,
                            verified,
                            Some(&mut run_guard),
                            AssessmentEvidenceReferences {
                                artifact: artifact_reference,
                                finalization_inputs: &finalization_inputs_reference,
                                snapshot_manifest: &manifest_reference,
                            },
                        )
                        .await
                    } else {
                        if contract.criteria.iter().any(|criterion| {
                            criterion.method
                                == super::model::CriterionEvaluationMethod::SemanticJudge
                        }) {
                            SemanticEvaluation::inconclusive(
                                if assessment.validation.state == ValidationState::Failed {
                                    "host_validation_failed"
                                } else {
                                    "approved_semantic_policy_unsupported"
                                },
                            )
                        } else {
                            SemanticEvaluation::not_required()
                        }
                    }
                }
                _ => SemanticEvaluation::inconclusive(
                    "approved_contract_or_verified_artifact_missing",
                ),
            }
        } else {
            SemanticEvaluation::not_required()
        };
        if !self.claimed_finalization_is_current(run_id, attempt_id, &semantic) {
            return self.require_run(run_id);
        }
        let requires_claim = run.snapshot.semantic_judge_enabled
            && contract
                .as_ref()
                .is_some_and(ApprovalAssessmentContract::can_dispatch_semantic_assessment)
            && self.inner.claimed_assessment.is_some()
            && assessment.validation.state != ValidationState::Failed;
        if requires_claim && semantic.assessment_id.is_none() {
            let _ = self.persist_semantic_assessment_evidence(run_id, attempt_id, &semantic);
            return self.transition_attempt_or_current(
                run_id,
                attempt_id,
                &identity,
                RunState::Unknown,
            );
        }
        if semantic.is_pending() {
            if self
                .persist_semantic_assessment_evidence(run_id, attempt_id, &semantic)
                .is_err()
            {
                return self.require_run(run_id);
            }
            return self.require_run(run_id);
        }
        assessment.acceptance_conditions_met = semantic.acceptance_conditions_met;
        if self
            .persist_semantic_assessment_evidence(run_id, attempt_id, &semantic)
            .is_err()
        {
            return self.require_run(run_id);
        }
        self.inner
            .store
            .record_interactive_finalization(run_id, attempt_id, proof)?;
        if self
            .inner
            .store
            .record_execution_evidence(run_id, attempt_id, assessment)
            .is_err()
        {
            return self.require_run(run_id);
        }
        let latest = self.require_run(run_id)?;
        let verifying = if latest.state == RunState::Finalizing {
            self.inner
                .store
                .transition(run_id, latest.revision, RunState::Verifying)?
        } else {
            latest
        };
        self.complete_verified_run(verifying)
    }

    /// Reconcile a durable verified-finalization claim without restarting the
    /// coding agent or creating a second judge job.
    pub async fn resume_finalization(
        &self,
        run_id: &RunId,
    ) -> Result<AgentRunRecord, CoordinatorError> {
        let mut run_guard = RunLockLease::acquire(self.run_lock(run_id)).await;
        let mut run = self.require_run(run_id)?;
        if run.state.is_terminal() {
            return Ok(run);
        }
        let Some(attempt_id) = run.current_attempt_id.clone() else {
            return Ok(run);
        };
        if self.has_cancellation_effect(run_id, &attempt_id)? {
            return self.transition_if_allowed(&run, RunState::Unknown);
        }
        if !matches!(
            run.state,
            RunState::Finalizing | RunState::Verifying | RunState::Unknown
        ) {
            return Ok(run);
        }
        if self.local_commit_confirmation_pending(run_id, &attempt_id)? {
            return Ok(run);
        }
        if run.execution_evidence.is_some() {
            if run.state == RunState::Finalizing {
                run = self
                    .inner
                    .store
                    .transition(run_id, run.revision, RunState::Verifying)?;
            }
            if run.state == RunState::Verifying {
                return self.complete_verified_run(run);
            }
            return Ok(run);
        }
        if run.state == RunState::Verifying {
            return self.transition_if_allowed(&run, RunState::Unknown);
        }

        if !run.snapshot.semantic_judge_enabled {
            return Ok(run);
        }
        let Some(config) = self.inner.claimed_assessment.as_ref() else {
            return Ok(run);
        };
        let saved = match self
            .inner
            .store
            .read_saved_verified_finalization_inputs(run_id, &attempt_id)
        {
            Ok(saved) => saved,
            Err(_) => return Ok(run),
        };
        let binding = match self.inner.store.assessment_intent(run_id, &attempt_id) {
            Ok(binding) => binding,
            Err(_) => return Ok(run),
        };
        if binding.run_id != *run_id || binding.attempt_id != attempt_id {
            return Ok(run);
        }
        let (base, validated_report) =
            match self.load_verified_finalization_base(run_id, &attempt_id, &saved) {
                Ok(base) => base,
                Err(_) => return Ok(run),
            };
        let contract = match self.inner.store.approval_assessment_contract(run_id)? {
            ApprovalAssessmentContractStatus::Versioned { contract, .. }
                if contract.can_dispatch_semantic_assessment() =>
            {
                *contract
            }
            ApprovalAssessmentContractStatus::Versioned { .. }
            | ApprovalAssessmentContractStatus::LegacyIncomplete => return Ok(run),
        };
        let prepared = match config
            .engine
            .load_saved_prepared_for_claim(&self.inner.store, &binding)
        {
            Ok(prepared) => prepared,
            Err(_) => return Ok(run),
        };
        let metadata_name = format!("semantic-prepared-{}", binding.assessment_id);
        let metadata_reference = self
            .inner
            .store
            .evidence(run_id)?
            .into_iter()
            .find(|reference| {
                reference.attempt_id == attempt_id
                    && reference.name == metadata_name
                    && reference.media_type == JSON_MEDIA_TYPE
                    && reference.save_confirmed
            });
        let Some(metadata_reference) = metadata_reference else {
            return Ok(run);
        };
        let metadata_bytes = match self
            .inner
            .store
            .read_evidence(run_id, &metadata_reference.evidence_id)
        {
            Ok(bytes) => bytes,
            Err(_) => return Ok(run),
        };
        let metadata: PreparedClaimMetadataV1 = match serde_json::from_slice(&metadata_bytes) {
            Ok(metadata) => metadata,
            Err(_) => return Ok(run),
        };
        let evidence = self.inner.store.evidence(run_id)?;
        let manifest_reference = evidence.iter().find(|reference| {
            reference.attempt_id == attempt_id
                && reference.evidence_id == binding.snapshot_manifest_evidence_id
                && reference.name == "snapshot-manifest"
                && reference.media_type == JSON_MEDIA_TYPE
                && reference.save_confirmed
                && reference.sha256 == binding.snapshot_manifest_digest
        });
        let input_reference = evidence.iter().find(|reference| {
            reference.attempt_id == attempt_id
                && reference.evidence_id == binding.judge_input_evidence_id
                && reference.name == "judge-input"
                && reference.media_type == JSON_MEDIA_TYPE
                && reference.save_confirmed
        });
        let system_reference = evidence.iter().find(|reference| {
            reference.attempt_id == attempt_id
                && reference.name == "judge-system-policy"
                && reference.media_type == JSON_MEDIA_TYPE
                && reference.save_confirmed
        });
        let (Some(manifest_reference), Some(input_reference), Some(system_reference)) =
            (manifest_reference, input_reference, system_reference)
        else {
            return Ok(run);
        };
        let (Ok(manifest_bytes), Ok(input_bytes), Ok(system_bytes)) = (
            self.inner
                .store
                .read_evidence(run_id, &manifest_reference.evidence_id),
            self.inner
                .store
                .read_evidence(run_id, &input_reference.evidence_id),
            self.inner
                .store
                .read_evidence(run_id, &system_reference.evidence_id),
        ) else {
            return Ok(run);
        };
        if metadata.schema_version != 1
            || metadata.run_id != run_id.as_str()
            || metadata.attempt_id != attempt_id
            || metadata.assessment_id != binding.assessment_id
            || metadata.finalization_inputs_evidence_id != saved.index_reference().evidence_id
            || metadata.deadline_unix_ms == 0
            || metadata.judge_input_digest != prepared.judge_input_digest()
            || metadata.system_policy_digest != sha256_hex(prepared.system_policy_bytes())
            || manifest_reference.size_bytes != manifest_bytes.len() as u64
            || manifest_reference.sha256 != sha256_hex(&manifest_bytes)
            || input_reference.sha256 != prepared.judge_input_digest()
            || input_reference.size_bytes != input_bytes.len() as u64
            || input_bytes != prepared.input_bytes()
            || system_reference.sha256 != metadata.system_policy_digest
            || system_reference.size_bytes != system_bytes.len() as u64
            || system_bytes != prepared.system_policy_bytes()
            || binding.judge_input_digest != prepared.judge_input_digest()
            || binding.judge_input_scope_digest != prepared.judge_input_scope_digest()
            || binding.system_prompt_digest != metadata.system_policy_digest
        {
            return Ok(run);
        }
        if run.state == RunState::Unknown {
            run = self
                .inner
                .store
                .transition(run_id, run.revision, RunState::Finalizing)?;
        }
        let provider_selection =
            serde_json::from_slice::<serde_json::Value>(prepared.input_bytes())
                .ok()
                .and_then(|input| input.get("provider").cloned())
                .and_then(|provider| serde_json::from_value::<ProviderSelection>(provider).ok());
        let Some(provider_selection) = provider_selection else {
            return Ok(run);
        };
        let profile = match ApprovedJudgeProviderProfile::from_settings(
            &config.judge_settings,
            provider_selection,
            metadata.deadline_unix_ms,
        ) {
            Ok(profile) => profile,
            Err(_) => return Ok(run),
        };
        let claim_revision = run.revision;
        run_guard.release();
        let outcome = assess_claimed_with_jobworkerp(
            &config.jobworkerp_endpoint,
            profile,
            Arc::clone(&config.engine),
            prepared.clone(),
            Arc::clone(&self.inner.store),
            AssessmentIntentClaim {
                binding: binding.clone(),
                newly_created: false,
            },
            SystemAssessmentClock,
        )
        .await;
        run_guard.reacquire().await;
        let after_assessment = self.require_run(run_id)?;
        if after_assessment.current_attempt_id.as_deref() != Some(attempt_id.as_str())
            || after_assessment.state != RunState::Finalizing
            || after_assessment.revision != claim_revision
            || self
                .has_cancellation_effect(run_id, &attempt_id)
                .unwrap_or(true)
        {
            return Ok(after_assessment);
        }
        run = after_assessment;
        let Ok(outcome) = outcome else {
            return self.require_run(run_id);
        };
        let Ok(outcome_json) = serde_json::to_value(&outcome) else {
            return self.require_run(run_id);
        };
        let acceptance_conditions_met = match &outcome {
            AssessmentOutcome::Assessed { assessment, .. } => semantic_completion_conditions_met(
                &contract.criteria,
                Some(&validated_report),
                assessment.overall,
            ),
            AssessmentOutcome::Pending { .. } | AssessmentOutcome::Inconclusive(_) => false,
        };
        let semantic = SemanticEvaluation {
            assessment_id: Some(binding.assessment_id.clone()),
            judge_input_digest: Some(prepared.judge_input_digest().to_owned()),
            judge_input_scope_digest: Some(prepared.judge_input_scope_digest().to_owned()),
            outcome: outcome_json,
            acceptance_conditions_met,
            claim_revision: Some(claim_revision),
        };
        if !self.claimed_finalization_is_current(run_id, &attempt_id, &semantic) {
            return self.require_run(run_id);
        }
        if semantic.is_pending() {
            if self
                .persist_semantic_assessment_evidence(run_id, &attempt_id, &semantic)
                .is_err()
            {
                return self.require_run(run_id);
            }
            return self.require_run(run_id);
        }
        if self
            .persist_semantic_assessment_evidence(run_id, &attempt_id, &semantic)
            .is_err()
        {
            return self.require_run(run_id);
        }
        let mut final_assessment = base.assessment.clone();
        final_assessment.acceptance_conditions_met = semantic.acceptance_conditions_met;
        if run.snapshot.execution_mode == ExecutionMode::Interactive {
            let proof = InteractiveFinalizationEvidence::from_host_observations_with_invocation(
                run.snapshot.profile.version,
                base.invocation_kind,
                base.cli_exit.clone(),
                base.shell_observation.clone(),
                base.child_writers,
                base.workspace_freeze.clone(),
            );
            if self
                .inner
                .store
                .record_interactive_finalization(run_id, &attempt_id, proof)
                .is_err()
            {
                return self.require_run(run_id);
            }
        }
        if self
            .inner
            .store
            .record_execution_evidence(run_id, &attempt_id, final_assessment)
            .is_err()
        {
            return self.require_run(run_id);
        }
        run = self.require_run(run_id)?;
        if run.state == RunState::Finalizing {
            run = self
                .inner
                .store
                .transition(run_id, run.revision, RunState::Verifying)?;
        }
        self.complete_verified_run(run)
    }

    pub async fn cancel(&self, run_id: &RunId) -> Result<AgentRunRecord, CoordinatorError> {
        let mut run_guard = RunLockLease::acquire(self.run_lock(run_id)).await;
        let run = self.require_run(run_id)?;
        if run.state.is_terminal() {
            self.retire_local_capture_if_proven(&run).await;
            return Ok(run);
        }
        let execution = self
            .inner
            .execution
            .as_ref()
            .cloned()
            .ok_or(CoordinatorError::AdapterUnavailable)?;
        let attempt_id = run
            .current_attempt_id
            .as_deref()
            .ok_or(CoordinatorError::Conflict)?;
        let identity = self.identity_for(run_id)?;
        let prior_intent = self.read_cancel_intent(run_id, attempt_id)?;
        let effect_key = effect_operation_key("cancel", attempt_id);
        let claim = self.inner.store.claim_effect(
            run_id,
            attempt_id,
            &effect_key,
            DurableOperationRequest::CancelAgent,
        )?;
        if !matches!(
            claim.operation.request,
            DurableOperationRequest::CancelAgent
        ) {
            return Err(CoordinatorError::Conflict);
        }
        // Only the atomic journal winner may dispatch. A legacy intent-only
        // marker may suppress a replay, but can never grant ownership.
        let may_dispatch_claim = claim.newly_created && prior_intent.is_none();
        if prior_intent.is_none() {
            self.persist_cancel_intent(run_id, attempt_id)?;
        }
        if let Some(receipt) = self.read_cancel_receipt(run_id, attempt_id)? {
            return self
                .finish_cancellation(run_id, attempt_id, &receipt, &identity)
                .await;
        }
        self.transition_if_allowed(&run, RunState::Unknown)?;
        let latest = self.require_run(run_id)?;
        if latest.state.is_terminal() {
            return Ok(latest);
        }
        let may_dispatch =
            may_dispatch_claim && latest.current_attempt_id.as_deref() == Some(attempt_id);
        run_guard.release();
        let reconciliation = if may_dispatch {
            execution.cancel(&identity).await
        } else {
            execution.reconcile_cancel(&identity).await
        }
        .unwrap_or(OperationReconciliation::Unknown(None));
        run_guard.reacquire().await;
        let after_cancellation_rpc = self.require_run(run_id)?;
        if after_cancellation_rpc.current_attempt_id.as_deref() != Some(attempt_id)
            || after_cancellation_rpc.state.is_terminal()
            || !self.has_cancellation_effect(run_id, attempt_id)?
        {
            return Ok(after_cancellation_rpc);
        }
        match reconciliation {
            OperationReconciliation::Accepted(observation)
                if valid_cancellation_observation(&observation) =>
            {
                let receipt = CancellationReceipt {
                    run_id: run_id.to_string(),
                    attempt_id: attempt_id.to_owned(),
                    observation,
                };
                self.finish_cancellation(run_id, attempt_id, &receipt, &identity)
                    .await
            }
            OperationReconciliation::Accepted(_)
            | OperationReconciliation::NotAccepted
            | OperationReconciliation::Unknown(_) => self.require_run(run_id),
        }
    }

    fn read_cancel_intent(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<Option<CancellationIntent>, CoordinatorError> {
        let reference = self
            .inner
            .store
            .evidence(run_id)?
            .into_iter()
            .find(|reference| {
                reference.attempt_id == attempt_id && reference.name == "cancel-intent"
            });
        let Some(reference) = reference else {
            return Ok(None);
        };
        let bytes = self
            .inner
            .store
            .read_evidence(run_id, &reference.evidence_id)?;
        let intent: CancellationIntent =
            serde_json::from_slice(&bytes).map_err(|_| CoordinatorError::Conflict)?;
        if intent.run_id != run_id.as_str() || intent.attempt_id != attempt_id || !intent.requested
        {
            return Err(CoordinatorError::Conflict);
        }
        Ok(Some(intent))
    }

    fn persist_cancel_intent(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<(), CoordinatorError> {
        if self.read_cancel_intent(run_id, attempt_id)?.is_some() {
            return Ok(());
        }
        let intent = CancellationIntent {
            run_id: run_id.to_string(),
            attempt_id: attempt_id.to_owned(),
            requested: true,
        };
        let bytes = serde_json::to_vec(&intent).map_err(|_| CoordinatorError::Conflict)?;
        self.write_unique_evidence(run_id, attempt_id, "cancel-intent", JSON_MEDIA_TYPE, &bytes)?;
        Ok(())
    }

    fn read_cancel_receipt(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<Option<CancellationReceipt>, CoordinatorError> {
        let reference = self
            .inner
            .store
            .evidence(run_id)?
            .into_iter()
            .find(|reference| {
                reference.attempt_id == attempt_id && reference.name == "cancel-termination"
            });
        let Some(reference) = reference else {
            return Ok(None);
        };
        let bytes = self
            .inner
            .store
            .read_evidence(run_id, &reference.evidence_id)?;
        let receipt: CancellationReceipt =
            serde_json::from_slice(&bytes).map_err(|_| CoordinatorError::Conflict)?;
        if receipt.run_id != run_id.as_str()
            || receipt.attempt_id != attempt_id
            || !valid_cancellation_observation(&receipt.observation)
        {
            return Err(CoordinatorError::Conflict);
        }
        Ok(Some(receipt))
    }

    fn has_cancel_intent(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<bool, CoordinatorError> {
        Ok(self.read_cancel_intent(run_id, attempt_id)?.is_some())
    }

    fn has_cancellation_effect(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<bool, CoordinatorError> {
        if self.has_cancel_intent(run_id, attempt_id)? {
            return Ok(true);
        }
        Ok(self
            .inner
            .store
            .operation(run_id, &effect_operation_key("cancel", attempt_id))?
            .is_some())
    }

    async fn finish_cancellation(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        receipt: &CancellationReceipt,
        identity: &ExecutionIdentity,
    ) -> Result<AgentRunRecord, CoordinatorError> {
        let run = self.require_run(run_id)?;
        if run.current_attempt_id.as_deref() != Some(attempt_id)
            || identity.run_id != run_id.as_str()
            || identity.attempt_id != attempt_id
        {
            return Ok(run);
        }
        if receipt.run_id != run_id.as_str()
            || receipt.attempt_id != attempt_id
            || !valid_cancellation_observation(&receipt.observation)
        {
            return self.require_run(run_id);
        }
        let bytes = serde_json::to_vec(receipt).map_err(|_| CoordinatorError::Conflict)?;
        let evidence = self.write_unique_evidence(
            run_id,
            attempt_id,
            "cancel-termination",
            JSON_MEDIA_TYPE,
            &bytes,
        )?;
        let effect_key = effect_operation_key("cancel", attempt_id);
        if self.inner.store.operation(run_id, &effect_key)?.is_some() {
            self.inner.store.complete_operation(
                run_id,
                &effect_key,
                DurableOperationResult {
                    outcome: DurableOperationOutcome::Succeeded,
                    result_digest: Some(sha256_hex(&bytes)),
                    evidence_ids: vec![evidence.evidence_id],
                },
            )?;
        }
        self.retire_capture_owner(identity, true).await;
        self.inner
            .terminal_owners
            .publish(run_id.as_str(), attempt_id, TerminalEvent::FeedClosed);
        let latest = self.require_run(run_id)?;
        if latest.current_attempt_id.as_deref() != Some(attempt_id) {
            return Ok(latest);
        }
        // The final transition is revision-CAS protected. If a newer attempt
        // appeared after the current-attempt check, its revision wins.
        self.inner
            .store
            .transition(run_id, latest.revision, RunState::Cancelled)
            .map_err(CoordinatorError::from)
    }

    /// Reconcile in-flight job/publication metadata. This path can finish a
    /// previously accepted intent but never enqueues a replacement operation.
    pub async fn reconcile(&self, run_id: &RunId) -> Result<RunReconciliation, CoordinatorError> {
        let run = self.require_run(run_id)?;
        if run.state.is_terminal() {
            return Ok(RunReconciliation::Unchanged(run));
        }
        let Some(attempt_id) = run.current_attempt_id.as_deref() else {
            return Ok(RunReconciliation::Unchanged(run));
        };
        let cancel_key = effect_operation_key("cancel", attempt_id);
        let cancel_operation = self.inner.store.operation(run_id, &cancel_key)?;
        let has_cancel_marker = self.read_cancel_intent(run_id, attempt_id)?.is_some();
        let cancellation_requested = has_cancel_marker || cancel_operation.is_some();
        if cancellation_requested
            && cancel_operation.as_ref().is_some_and(|operation| {
                operation.attempt_id != attempt_id
                    || !matches!(&operation.request, DurableOperationRequest::CancelAgent)
            })
        {
            let unknown = self.transition_if_allowed(&run, RunState::Unknown)?;
            return Ok(RunReconciliation::Unknown(unknown));
        }
        if cancellation_requested {
            self.persist_cancel_intent(run_id, attempt_id)?;
            let identity = self.identity_for(run_id)?;
            if let Some(receipt) = self.read_cancel_receipt(run_id, attempt_id)? {
                let cancelled = self
                    .finish_cancellation(run_id, attempt_id, &receipt, &identity)
                    .await?;
                return Ok(RunReconciliation::Accepted(cancelled));
            }
            let execution = self
                .inner
                .execution
                .as_ref()
                .ok_or(CoordinatorError::AdapterUnavailable)?;
            self.transition_if_allowed(&run, RunState::Unknown)?;
            let result = execution
                .reconcile_cancel(&identity)
                .await
                .unwrap_or(OperationReconciliation::Unknown(None));
            return match result {
                OperationReconciliation::Accepted(observation)
                    if valid_cancellation_observation(&observation) =>
                {
                    let receipt = CancellationReceipt {
                        run_id: run_id.to_string(),
                        attempt_id: attempt_id.to_owned(),
                        observation,
                    };
                    Ok(RunReconciliation::Accepted(
                        self.finish_cancellation(run_id, attempt_id, &receipt, &identity)
                            .await?,
                    ))
                }
                OperationReconciliation::Accepted(_)
                | OperationReconciliation::NotAccepted
                | OperationReconciliation::Unknown(_) => {
                    Ok(RunReconciliation::Unknown(self.require_run(run_id)?))
                }
            };
        }
        if run.interactive_finish_request.is_some()
            && matches!(
                run.state,
                RunState::Running | RunState::Finalizing | RunState::Unknown
            )
        {
            let reconciled = self.finish_interactive(run_id).await?;
            return if reconciled.state == RunState::Unknown {
                Ok(RunReconciliation::Unknown(reconciled))
            } else {
                Ok(RunReconciliation::Accepted(reconciled))
            };
        }
        let start_key = operation_key("start", attempt_id);
        if let Some(operation) = self.inner.store.operation(run_id, &start_key)?
            && operation.status != DurableOperationStatus::Completed
        {
            let execution = self
                .inner
                .execution
                .as_ref()
                .ok_or(CoordinatorError::AdapterUnavailable)?;
            let Some(intent) = self.read_intent(run_id, attempt_id)? else {
                let unknown = self.transition_if_allowed(&run, RunState::Unknown)?;
                return Ok(RunReconciliation::Unknown(unknown));
            };
            let result = execution
                .reconcile_start(&run.snapshot, &intent)
                .await
                .unwrap_or(OperationReconciliation::Unknown(None));
            return match result {
                OperationReconciliation::Accepted(receipt)
                    if valid_start_receipt(&receipt, &intent, &sha256_serialized(&intent)?) =>
                {
                    self.persist_reconciled_start(run_id, &run.snapshot, &start_key, receipt)?;
                    Ok(RunReconciliation::Accepted(self.require_run(run_id)?))
                }
                OperationReconciliation::NotAccepted => {
                    self.complete_operation_failure(run_id, &start_key)?;
                    let failed =
                        self.transition_if_allowed(&self.require_run(run_id)?, RunState::Failed)?;
                    Ok(RunReconciliation::Unchanged(failed))
                }
                OperationReconciliation::Accepted(_) | OperationReconciliation::Unknown(_) => {
                    let unknown =
                        self.transition_if_allowed(&self.require_run(run_id)?, RunState::Unknown)?;
                    Ok(RunReconciliation::Unknown(unknown))
                }
            };
        }
        let publication_operation = self
            .inner
            .store
            .operation(run_id, &operation_key("publish", attempt_id))?;
        if publication_operation.is_some()
            || self.read_publication_intent(run_id, attempt_id)?.is_some()
        {
            return self.reconcile_publication(run_id, &run).await;
        }
        Ok(RunReconciliation::Unchanged(run))
    }

    /// Side effects use a durable intent and the current frozen commit. Any
    /// pre-existing intent is sent only to the adapter's reconciliation API.
    pub async fn publish(&self, run_id: &RunId) -> Result<AgentRunRecord, CoordinatorError> {
        let lock = self.run_lock(run_id);
        let mut run_guard = RunLockLease::acquire(lock).await;
        let run = self.require_run(run_id)?;
        let attempt_id_owned = run
            .current_attempt_id
            .as_deref()
            .ok_or(CoordinatorError::Conflict)?
            .to_owned();
        let attempt_id = attempt_id_owned.as_str();
        if self.has_cancellation_effect(run_id, attempt_id)? {
            return self.transition_if_allowed(&run, RunState::Unknown);
        }
        let publication_key = operation_key("publish", attempt_id);
        let prior_intent = self.read_publication_intent(run_id, attempt_id)?;
        let existing_operation = self.inner.store.operation(run_id, &publication_key)?;
        if existing_operation.is_some() || run.state == RunState::Unknown {
            if prior_intent.is_some() {
                let reconciled = self.reconcile_publication(run_id, &run).await?;
                return Ok(match reconciled {
                    RunReconciliation::Unchanged(record)
                    | RunReconciliation::Accepted(record)
                    | RunReconciliation::Unknown(record) => record,
                });
            }
            if existing_operation.is_some() {
                return self.transition_if_allowed(&run, RunState::Unknown);
            }
        }
        let adapter = self
            .inner
            .publication
            .as_ref()
            .ok_or(CoordinatorError::AdapterUnavailable)?;
        if !matches!(run.state, RunState::Verifying | RunState::Publishing)
            || run.snapshot.publication_plan == PublicationPlan::NoPublication
        {
            return Err(CoordinatorError::Conflict);
        }
        let assessment = run
            .execution_evidence
            .as_ref()
            .ok_or(CoordinatorError::Conflict)?;
        if matches!(
            self.inner.store.approval_assessment_contract(run_id)?,
            ApprovalAssessmentContractStatus::LegacyIncomplete
        ) || !self.verify_canonical_report_evidence(run_id, attempt_id, assessment)?
        {
            return Err(CoordinatorError::Unsupported);
        }
        if self.inner.verified_finalization.is_some()
            && !self.verify_verified_artifact_evidence(run_id, attempt_id, assessment)?
        {
            return Err(CoordinatorError::Unsupported);
        }
        let decision = evaluate_completion_gates(
            run.snapshot.purpose,
            run.snapshot.auto_done,
            run.snapshot.publication_plan.clone(),
            &run.snapshot.allowed_operations,
            assessment,
        );
        if decision.publication != GateDecision::Allowed {
            return Err(CoordinatorError::Unsupported);
        }
        let request = if let Some(request) = prior_intent.as_ref() {
            if !publication_request_matches_run(&run, attempt_id, assessment, request) {
                return Err(CoordinatorError::Conflict);
            }
            request.clone()
        } else {
            if run.state != RunState::Verifying {
                return Err(CoordinatorError::Conflict);
            }
            let support = adapter
                .preflight(&run.snapshot)
                .await
                .map_err(|_| CoordinatorError::AdapterUnavailable)?;
            if !validate_publication_support(&run.snapshot, &support).is_empty() {
                return Err(CoordinatorError::Unsupported);
            }
            let PublicationSupport::Verified {
                approved_base_branch,
                approved_web_base_url,
                ..
            } = support
            else {
                return Err(CoordinatorError::Unsupported);
            };
            let commit = assessment
                .commit_id
                .as_deref()
                .ok_or(CoordinatorError::Conflict)?;
            let request = publication_request(
                &run,
                attempt_id,
                commit,
                &approved_base_branch,
                &approved_web_base_url,
            );
            if !publication_request_matches_run(&run, attempt_id, assessment, &request) {
                return Err(CoordinatorError::Unsupported);
            }
            let request_bytes =
                serde_json::to_vec(&request).map_err(|_| CoordinatorError::Conflict)?;
            self.write_unique_evidence(
                run_id,
                attempt_id,
                "publication-intent",
                JSON_MEDIA_TYPE,
                &request_bytes,
            )?;
            request
        };
        if request.operation_key != publication_key
            || request.plan != run.snapshot.publication_plan
            || request.operation_key != operation_key("publish", attempt_id)
        {
            return Err(CoordinatorError::Conflict);
        }
        let publishing = if run.state == RunState::Verifying {
            self.inner
                .store
                .transition(run_id, run.revision, RunState::Publishing)?
        } else {
            run
        };
        if self.has_cancellation_effect(run_id, attempt_id)? {
            return self.transition_if_allowed(&self.require_run(run_id)?, RunState::Unknown);
        }
        let claim = match self.inner.store.claim_publication_effect(
            run_id,
            publishing.revision,
            attempt_id,
            &publication_key,
            publication_operation(&request),
        ) {
            Ok(claim) => claim,
            Err(_) if self.has_cancellation_effect(run_id, attempt_id)? => {
                return self.transition_if_allowed(&self.require_run(run_id)?, RunState::Unknown);
            }
            Err(error) => return Err(error.into()),
        };
        if !claim.newly_created {
            let current = self.require_run(run_id)?;
            let reconciled = self.reconcile_publication(run_id, &current).await?;
            return Ok(match reconciled {
                RunReconciliation::Unchanged(record)
                | RunReconciliation::Accepted(record)
                | RunReconciliation::Unknown(record) => record,
            });
        }
        let Some(permit) = claim.permit else {
            return self.transition_if_allowed(&self.require_run(run_id)?, RunState::Unknown);
        };
        if self.has_cancellation_effect(run_id, attempt_id)? {
            self.complete_new_publication(
                run_id,
                attempt_id,
                &request,
                permit,
                PublicationOutcome::Unknown,
            )?;
            return self.transition_if_allowed(&self.require_run(run_id)?, RunState::Unknown);
        }
        let start = adapter.begin_publication(&request).await;
        match start {
            Ok(super::PublicationStart::Inline) => {}
            Ok(start @ super::PublicationStart::Admitted { parent_job_id })
                if parent_job_id.is_none_or(|id| id > 0) =>
            {
                let bytes = serde_json::to_vec(&start).map_err(|_| CoordinatorError::Conflict)?;
                if self
                    .write_unique_evidence(
                        run_id,
                        attempt_id,
                        "publication-workflow-start",
                        JSON_MEDIA_TYPE,
                        &bytes,
                    )
                    .is_err()
                {
                    return self.complete_new_publication(
                        run_id,
                        attempt_id,
                        &request,
                        permit,
                        PublicationOutcome::Unknown,
                    );
                }
                run_guard.release();
            }
            _ => {
                return self.complete_new_publication(
                    run_id,
                    attempt_id,
                    &request,
                    permit,
                    PublicationOutcome::Unknown,
                );
            }
        }
        let outcome = adapter
            .publish(&request)
            .await
            .unwrap_or(PublicationOutcome::Unknown);
        run_guard.reacquire().await;
        self.complete_new_publication(run_id, attempt_id, &request, permit, outcome)
    }

    pub fn ensure_profile_mutable(&self, profile_id: &str) -> Result<(), CoordinatorError> {
        if self
            .inner
            .store
            .recoverable_runs()?
            .iter()
            .any(|run| run.snapshot.profile.profile_id == profile_id)
        {
            return Err(CoordinatorError::Conflict);
        }
        Ok(())
    }

    async fn prepare_or_reconcile_workspace(
        &self,
        execution: &dyn ExecutionAdapter,
        request: &WorkspaceRequest,
    ) -> Result<Option<WorkspaceReceipt>, CoordinatorError> {
        let run_id = &request.snapshot.run_id;
        let key = operation_key("prepare", &request.attempt_id);
        if let Some(operation) = self.inner.store.operation(run_id, &key)? {
            return match execution
                .reconcile_workspace(request)
                .await
                .unwrap_or(OperationReconciliation::Unknown(None))
            {
                OperationReconciliation::Accepted(receipt)
                    if valid_workspace_receipt(&receipt, request) =>
                {
                    if operation.status != DurableOperationStatus::Completed {
                        self.persist_workspace_result(run_id, &key, &receipt)?;
                    }
                    Ok(Some(receipt))
                }
                OperationReconciliation::Accepted(_) | OperationReconciliation::Unknown(_) => {
                    Ok(None)
                }
                OperationReconciliation::NotAccepted => {
                    self.complete_operation_failure(run_id, &key)?;
                    Ok(None)
                }
            };
        }
        self.inner.store.begin_operation(
            run_id,
            &request.attempt_id,
            &key,
            DurableOperationRequest::PrepareWorkspace {
                repository: request.snapshot.repository.identity.clone(),
                base_commit: request.snapshot.base_commit.clone(),
                branch: request.snapshot.workspace_branch.clone(),
            },
        )?;
        let receipt = match execution.prepare_workspace(request).await {
            Ok(receipt) if valid_workspace_receipt(&receipt, request) => receipt,
            _ => {
                self.complete_unconfirmed(run_id, &key, "workspace side effect is unconfirmed")?;
                return Ok(None);
            }
        };
        self.persist_workspace_result(run_id, &key, &receipt)?;
        Ok(Some(receipt))
    }

    fn persist_workspace_result(
        &self,
        run_id: &RunId,
        key: &str,
        receipt: &WorkspaceReceipt,
    ) -> Result<(), CoordinatorError> {
        let bytes = serde_json::to_vec(receipt).map_err(|_| CoordinatorError::Conflict)?;
        self.inner.store.complete_operation(
            run_id,
            key,
            DurableOperationResult {
                outcome: DurableOperationOutcome::Succeeded,
                result_digest: Some(sha256_hex(&bytes)),
                evidence_ids: Vec::new(),
            },
        )?;
        Ok(())
    }

    pub fn workspace_receipt(
        &self,
        run: &AgentRunRecord,
    ) -> Result<Option<WorkspaceReceipt>, CoordinatorError> {
        let Some(attempt_id) = &run.current_attempt_id else {
            return Ok(None);
        };
        let Some(reference) = run.evidence.iter().find(|reference| {
            &reference.attempt_id == attempt_id
                && reference.name == "workspace-receipt"
                && reference.save_confirmed
        }) else {
            return Ok(None);
        };
        let bytes = self.read_evidence(&run.snapshot.run_id, &reference.evidence_id)?;
        let receipt: WorkspaceReceipt =
            serde_json::from_slice(&bytes).map_err(|_| CoordinatorError::Conflict)?;
        let request = WorkspaceRequest {
            snapshot: run.snapshot.clone(),
            attempt_id: attempt_id.clone(),
        };
        if !valid_workspace_receipt(&receipt, &request) {
            return Err(CoordinatorError::Conflict);
        }
        Ok(Some(receipt))
    }

    fn write_unique_evidence(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        name: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<super::EvidenceReference, CoordinatorError> {
        if let Some(existing) = self
            .inner
            .store
            .evidence(run_id)?
            .into_iter()
            .find(|item| item.attempt_id == attempt_id && item.name == name)
        {
            let stored = self
                .inner
                .store
                .read_evidence(run_id, &existing.evidence_id)?;
            if stored == bytes && existing.media_type == media_type {
                return Ok(existing);
            }
            return Err(CoordinatorError::Conflict);
        }
        Ok(self
            .inner
            .store
            .write_evidence(run_id, attempt_id, name, media_type, bytes)?)
    }

    fn read_intent(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<Option<ImmutableExecutionIntent>, CoordinatorError> {
        let Some(reference) = self
            .inner
            .store
            .evidence(run_id)?
            .into_iter()
            .find(|reference| {
                reference.attempt_id == attempt_id && reference.name == "dispatch-intent"
            })
        else {
            return Ok(None);
        };
        let bytes = self
            .inner
            .store
            .read_evidence(run_id, &reference.evidence_id)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| CoordinatorError::Conflict)
    }

    fn read_start_receipt(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<Option<StartReceipt>, CoordinatorError> {
        let Some(reference) = self
            .inner
            .store
            .evidence(run_id)?
            .into_iter()
            .find(|reference| {
                reference.attempt_id == attempt_id && reference.name == "dispatch-receipt"
            })
        else {
            return Ok(None);
        };
        let bytes = self
            .inner
            .store
            .read_evidence(run_id, &reference.evidence_id)?;
        let receipt: StartReceipt =
            serde_json::from_slice(&bytes).map_err(|_| CoordinatorError::Conflict)?;
        if receipt.identity.run_id != run_id.as_str() || receipt.identity.attempt_id != attempt_id {
            return Err(CoordinatorError::Conflict);
        }
        Ok(Some(receipt))
    }

    fn local_capture_identity(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Option<ExecutionIdentity> {
        let key = attempt_key(run_id.as_str(), attempt_id);
        let active_task = self
            .inner
            .tasks
            .lock()
            .expect("run task map poisoned")
            .get(&key)
            .is_some_and(|task| !task.is_finished());
        let captured = self
            .inner
            .captures
            .lock()
            .expect("capture map poisoned")
            .contains_key(&key);
        if !active_task && !captured {
            return None;
        }
        let registered = self
            .inner
            .identities
            .lock()
            .expect("identity map poisoned")
            .get(&key)
            .cloned();
        let terminal = self
            .inner
            .terminal_owners
            .identity(run_id.as_str(), attempt_id);
        match (registered, terminal) {
            (Some(registered), Some(terminal)) if registered != terminal => None,
            (Some(registered), _) => Some(registered),
            (None, Some(terminal)) => Some(terminal),
            (None, None) => None,
        }
    }

    fn finish_start_reconciliation(
        &self,
        snapshot: &AgentRunSnapshot,
        attempt_id: &str,
        intent: &ImmutableExecutionIntent,
        operation: super::DurableOperationRecord,
        result: OperationReconciliation<StartReceipt>,
    ) -> Result<StartOutcome, CoordinatorError> {
        let latest = self.require_run(&snapshot.run_id)?;
        if let Some(owner_identity) = self.local_capture_identity(&snapshot.run_id, attempt_id) {
            let Some(existing_receipt) = self.read_start_receipt(&snapshot.run_id, attempt_id)?
            else {
                return Ok(StartOutcome::Unknown {
                    attempt_id: attempt_id.into(),
                });
            };
            let intent_digest = sha256_serialized(intent)?;
            if latest.current_attempt_id.as_deref() != Some(attempt_id)
                || existing_receipt.identity != owner_identity
                || !valid_start_receipt(&existing_receipt, intent, &intent_digest)
            {
                return Ok(StartOutcome::Unknown {
                    attempt_id: attempt_id.into(),
                });
            }
            return match result {
                OperationReconciliation::Accepted(receipt) if receipt == existing_receipt => {
                    Ok(StartOutcome::Reconciled {
                        attempt_id: attempt_id.into(),
                        state: latest.state,
                        receipt: Some(Box::new(existing_receipt)),
                    })
                }
                OperationReconciliation::Unknown(_) => Ok(StartOutcome::Reconciled {
                    attempt_id: attempt_id.into(),
                    state: latest.state,
                    receipt: Some(Box::new(existing_receipt)),
                }),
                OperationReconciliation::Accepted(_) | OperationReconciliation::NotAccepted => {
                    Ok(StartOutcome::Unknown {
                        attempt_id: attempt_id.into(),
                    })
                }
            };
        }
        match result {
            OperationReconciliation::Accepted(receipt)
                if valid_start_receipt(&receipt, intent, &sha256_serialized(intent)?) =>
            {
                let latest = self.require_run(&snapshot.run_id)?;
                if latest.current_attempt_id.as_deref() != Some(attempt_id) {
                    self.transition_if_allowed(&latest, RunState::Unknown)?;
                    return Ok(StartOutcome::Unknown {
                        attempt_id: attempt_id.into(),
                    });
                }
                self.persist_reconciled_start(
                    &snapshot.run_id,
                    snapshot,
                    &operation.idempotency_key,
                    receipt,
                )?;
                // The prior process may have accepted the job, but this
                // coordinator has no safe way to reattach its output stream.
                // Never reacquire a semaphore and reopen events on a receipt.
                self.transition_if_allowed(
                    &self.require_run(&snapshot.run_id)?,
                    RunState::Unknown,
                )?;
                Ok(StartOutcome::Unknown {
                    attempt_id: attempt_id.into(),
                })
            }
            OperationReconciliation::NotAccepted => {
                self.complete_operation_failure(&snapshot.run_id, &operation.idempotency_key)?;
                let failed = self.transition_if_allowed(
                    &self.require_run(&snapshot.run_id)?,
                    RunState::Failed,
                )?;
                Ok(StartOutcome::Reconciled {
                    attempt_id: attempt_id.into(),
                    state: failed.state,
                    receipt: None,
                })
            }
            OperationReconciliation::Accepted(_) | OperationReconciliation::Unknown(_) => {
                self.transition_if_allowed(
                    &self.require_run(&snapshot.run_id)?,
                    RunState::Unknown,
                )?;
                Ok(StartOutcome::Unknown {
                    attempt_id: attempt_id.into(),
                })
            }
        }
    }

    fn persist_reconciled_start(
        &self,
        run_id: &RunId,
        snapshot: &AgentRunSnapshot,
        start_key: &str,
        receipt: StartReceipt,
    ) -> Result<(), CoordinatorError> {
        let latest = self.require_run(run_id)?;
        if latest.current_attempt_id.as_deref() != Some(receipt.identity.attempt_id.as_str()) {
            self.transition_if_allowed(&latest, RunState::Unknown)?;
            return Err(CoordinatorError::Conflict);
        }
        let mut references = self
            .inner
            .store
            .evidence(run_id)?
            .into_iter()
            .filter(|reference| {
                reference.attempt_id == receipt.identity.attempt_id
                    && reference.name == "dispatch-intent"
            });
        let intent_reference = references.next().ok_or(CoordinatorError::Conflict)?;
        if references.next().is_some() || &snapshot.run_id != run_id {
            return Err(CoordinatorError::Conflict);
        }
        let intent_bytes = self
            .inner
            .store
            .read_evidence(run_id, &intent_reference.evidence_id)?;
        let intent: ImmutableExecutionIntent =
            serde_json::from_slice(&intent_bytes).map_err(|_| CoordinatorError::Conflict)?;
        if !valid_start_receipt(&receipt, &intent, &sha256_serialized(&intent)?) {
            return Err(CoordinatorError::Conflict);
        }
        let bytes = serde_json::to_vec(&receipt).map_err(|_| CoordinatorError::Conflict)?;
        let evidence = self.write_unique_evidence(
            run_id,
            &receipt.identity.attempt_id,
            "dispatch-receipt",
            JSON_MEDIA_TYPE,
            &bytes,
        )?;
        let operation = self
            .inner
            .store
            .operation(run_id, start_key)?
            .ok_or(CoordinatorError::Conflict)?;
        if operation.status != DurableOperationStatus::Completed {
            self.inner.store.complete_operation(
                run_id,
                start_key,
                DurableOperationResult {
                    outcome: DurableOperationOutcome::Succeeded,
                    result_digest: Some(sha256_hex(&bytes)),
                    evidence_ids: vec![intent_reference.evidence_id, evidence.evidence_id],
                },
            )?;
        }
        Ok(())
    }

    async fn launch_capture_task(
        &self,
        snapshot: &AgentRunSnapshot,
        receipt: &StartReceipt,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<(), CoordinatorError> {
        if receipt.identity.attempt_id != snapshot.primary_attempt_id()
            && !snapshot
                .attempt_ids
                .iter()
                .any(|attempt| attempt == &receipt.identity.attempt_id)
        {
            return Err(CoordinatorError::Conflict);
        }
        let latest = self.require_run(&snapshot.run_id)?;
        if latest.current_attempt_id.as_deref() != Some(receipt.identity.attempt_id.as_str())
            || latest.state != RunState::Running
        {
            return Err(CoordinatorError::Conflict);
        }
        let execution = self
            .inner
            .execution
            .as_ref()
            .cloned()
            .ok_or(CoordinatorError::AdapterUnavailable)?;
        let control = if snapshot.execution_mode == ExecutionMode::Interactive {
            Some(
                execution
                    .terminal_control(&receipt.identity)
                    .await
                    .map_err(|_| CoordinatorError::AdapterUnavailable)?,
            )
        } else {
            None
        };
        if let Some(control) = control {
            self.inner.terminal_owners.install(
                receipt.identity.clone(),
                control,
                self.inner.config.terminal_feed_capacity,
            )?;
            if receipt.prompt_delivery == PromptDelivery::Accepted {
                self.inner.terminal_owners.set_prompt_status(
                    &receipt.identity.run_id,
                    &receipt.identity.attempt_id,
                    PromptStatus::Accepted,
                );
            } else if snapshot.execution_mode == ExecutionMode::Interactive {
                self.inner.terminal_owners.set_prompt_status(
                    &receipt.identity.run_id,
                    &receipt.identity.attempt_id,
                    prompt_status(receipt.prompt_delivery),
                );
            }
        }
        self.inner
            .identities
            .lock()
            .expect("identity map poisoned")
            .insert(
                attempt_key(&receipt.identity.run_id, &receipt.identity.attempt_id),
                receipt.identity.clone(),
            );
        let events = execution
            .events(&receipt.identity)
            .await
            .map_err(|_| CoordinatorError::AdapterUnavailable)?;
        let task_key = attempt_key(&receipt.identity.run_id, &receipt.identity.attempt_id);
        let mut tasks = self.inner.tasks.lock().expect("run task map poisoned");
        let latest = self.require_run(&snapshot.run_id)?;
        if latest.current_attempt_id.as_deref() != Some(receipt.identity.attempt_id.as_str())
            || latest.state != RunState::Running
            || !self.identity_is_registered(&receipt.identity)
        {
            drop(tasks);
            self.inner
                .terminal_owners
                .remove_owner_for_identity(&receipt.identity);
            let mut identities = self.inner.identities.lock().expect("identity map poisoned");
            if identities
                .get(&task_key)
                .is_some_and(|registered| registered == &receipt.identity)
            {
                identities.remove(&task_key);
            }
            return Err(CoordinatorError::Conflict);
        }
        if tasks
            .get(&task_key)
            .is_some_and(|existing| !existing.is_finished())
        {
            return Err(CoordinatorError::Conflict);
        }
        let coordinator = self.clone();
        let snapshot = snapshot.clone();
        let receipt = receipt.clone();
        let maintenance = ExecutionMaintenanceGuard {
            owner: (snapshot.execution_mode == ExecutionMode::OneShot)
                .then(|| (execution, receipt.identity.clone())),
        };
        let task = tokio::spawn(async move {
            let _permit = permit;
            let _maintenance = maintenance;
            coordinator
                .capture_and_finalize(&snapshot, &receipt, events)
                .await;
        });
        tasks.insert(task_key, task);
        Ok(())
    }

    async fn capture_and_finalize(
        &self,
        snapshot: &AgentRunSnapshot,
        receipt: &StartReceipt,
        mut events: super::execution::ExecutionEventStream,
    ) {
        let run_id = &snapshot.run_id;
        let attempt_id = &receipt.identity.attempt_id;
        let mut capture = CapturedOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code: None,
            end_present: false,
            stream_error: false,
            capture_complete: true,
            stdout_reference: None,
            stderr_reference: None,
        };
        let mut next_retirement_check = tokio::time::Instant::now() + CAPTURE_RECONCILE_INTERVAL;
        loop {
            if tokio::time::Instant::now() >= next_retirement_check {
                next_retirement_check = tokio::time::Instant::now() + CAPTURE_RECONCILE_INTERVAL;
                if self
                    .capture_retirement_state(&receipt.identity)
                    .is_ok_and(|state| self.apply_capture_retirement(&receipt.identity, state))
                {
                    return;
                }
            }
            let event = tokio::select! {
                biased;
                event = events.next() => event,
                _ = tokio::time::sleep_until(next_retirement_check) => {
                    next_retirement_check = tokio::time::Instant::now() + CAPTURE_RECONCILE_INTERVAL;
                    if self
                        .capture_retirement_state(&receipt.identity)
                        .is_ok_and(|state| self.apply_capture_retirement(&receipt.identity, state))
                    {
                        return;
                    }
                    continue;
                }
            };
            let Some(event) = event else {
                break;
            };
            match self.capture_identity_is_current(&receipt.identity) {
                Ok(true) => {}
                Ok(false) => {
                    self.finish_capture_task(&receipt.identity, true);
                    return;
                }
                Err(_) => {
                    capture.capture_complete = false;
                    capture.stream_error = true;
                    continue;
                }
            }
            match event {
                Ok(ExecutionEvent::Output { stream, data })
                    if capture.exit_code.is_none()
                        && matches!(stream.as_str(), "stdout" | "stderr") =>
                {
                    let buffer = if stream == "stdout" {
                        &mut capture.stdout
                    } else {
                        &mut capture.stderr
                    };
                    let remaining = self
                        .inner
                        .config
                        .max_capture_bytes_per_stream
                        .saturating_sub(buffer.len());
                    let kept = data.len().min(remaining);
                    if kept < data.len() {
                        capture.capture_complete = false;
                    }
                    if kept > 0 {
                        let bytes = &data[..kept];
                        buffer.extend_from_slice(bytes);
                        let name = if stream == "stdout" {
                            "stdout-chunk"
                        } else {
                            "stderr-chunk"
                        };
                        if self
                            .inner
                            .store
                            .write_evidence(run_id, attempt_id, name, STDOUT_MEDIA_TYPE, bytes)
                            .is_err()
                        {
                            capture.capture_complete = false;
                        }
                    }
                    match self.capture_identity_is_current(&receipt.identity) {
                        Ok(true) => {}
                        Ok(false) => {
                            self.finish_capture_task(&receipt.identity, true);
                            return;
                        }
                        Err(_) => {
                            capture.capture_complete = false;
                            capture.stream_error = true;
                            continue;
                        }
                    }
                    self.inner.terminal_owners.publish(
                        run_id.as_str(),
                        attempt_id,
                        TerminalEvent::Output {
                            stream,
                            data: data[..kept].to_vec(),
                        },
                    );
                }
                Ok(ExecutionEvent::Output { .. }) => capture.stream_error = true,
                Ok(ExecutionEvent::Exit { exit_code }) if capture.exit_code.is_none() => {
                    capture.exit_code = Some(exit_code);
                    self.inner.terminal_owners.publish(
                        run_id.as_str(),
                        attempt_id,
                        TerminalEvent::Exit { exit_code },
                    );
                }
                Ok(ExecutionEvent::Exit { .. }) => capture.stream_error = true,
                Err(_) => {
                    capture.stream_error = true;
                    break;
                }
            }
        }
        match self.capture_identity_is_current(&receipt.identity) {
            Ok(true) => {}
            Ok(false) => {
                self.finish_capture_task(&receipt.identity, true);
                return;
            }
            Err(_) => {
                capture.capture_complete = false;
                capture.stream_error = true;
            }
        }
        capture.end_present = !capture.stream_error;
        capture.stdout_reference =
            match self.persist_aggregate(run_id, attempt_id, "stdout", &capture.stdout) {
                Ok(reference) => reference,
                Err(_) => {
                    capture.capture_complete = false;
                    None
                }
            };
        capture.stderr_reference =
            match self.persist_aggregate(run_id, attempt_id, "stderr", &capture.stderr) {
                Ok(reference) => reference,
                Err(_) => {
                    capture.capture_complete = false;
                    None
                }
            };
        capture.stream_error |= !capture.capture_complete || capture.exit_code.is_none();
        match self.capture_identity_is_current(&receipt.identity) {
            Ok(true) => {}
            Ok(false) => {
                self.finish_capture_task(&receipt.identity, true);
                return;
            }
            Err(_) => {
                capture.capture_complete = false;
                capture.stream_error = true;
            }
        }
        let capture_status = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "attempt_id": attempt_id,
            "end_present": capture.end_present,
            "stream_error": capture.stream_error,
            "capture_complete": capture.capture_complete,
            "exit_code": capture.exit_code,
            "stdout_bytes": capture.stdout.len(),
            "stderr_bytes": capture.stderr.len(),
            "stdout_reference": capture.stdout_reference.as_deref(),
            "stderr_reference": capture.stderr_reference.as_deref(),
        }))
        .unwrap_or_default();
        if capture_status.is_empty()
            || self
                .write_unique_evidence(
                    run_id,
                    attempt_id,
                    "capture-status",
                    JSON_MEDIA_TYPE,
                    &capture_status,
                )
                .is_err()
        {
            capture.capture_complete = false;
            capture.stream_error = true;
        }
        match self.capture_identity_is_current(&receipt.identity) {
            Ok(true) => {}
            Ok(false) => {
                self.finish_capture_task(&receipt.identity, true);
                return;
            }
            Err(_) => {
                capture.capture_complete = false;
                capture.stream_error = true;
            }
        }
        self.inner
            .terminal_owners
            .publish(run_id.as_str(), attempt_id, TerminalEvent::FeedClosed);
        let capture_key = attempt_key(run_id.as_str(), attempt_id);
        self.inner
            .captures
            .lock()
            .expect("capture map poisoned")
            .insert(capture_key, capture.clone());

        match self.capture_retirement_state(&receipt.identity) {
            Ok(CaptureRetirement::Superseded | CaptureRetirement::Cancelled) => {
                self.finish_capture_task(&receipt.identity, true);
                return;
            }
            Ok(CaptureRetirement::HostQuiesced) => {
                self.finish_capture_task(&receipt.identity, false);
                return;
            }
            Ok(CaptureRetirement::Continue) | Err(_) => {}
        }

        if snapshot.execution_mode == ExecutionMode::Interactive {
            if capture.stream_error {
                let _ = self.transition_attempt_if_current(
                    run_id,
                    attempt_id,
                    &receipt.identity,
                    RunState::Unknown,
                );
            }
            return;
        }
        if self
            .has_cancellation_effect(run_id, attempt_id)
            .unwrap_or(true)
        {
            return;
        }
        self.finalize_one_shot(snapshot, receipt, capture).await;
    }

    fn persist_aggregate(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<Option<String>, CoordinatorError> {
        if bytes.is_empty() {
            return Ok(None);
        }
        let reference =
            self.write_unique_evidence(run_id, attempt_id, name, STDOUT_MEDIA_TYPE, bytes)?;
        Ok(Some(reference.evidence_id))
    }

    async fn finalize_one_shot(
        &self,
        snapshot: &AgentRunSnapshot,
        receipt: &StartReceipt,
        capture: CapturedOutput,
    ) {
        let run_id = &snapshot.run_id;
        let attempt_id = &receipt.identity.attempt_id;
        let Ok(run) = self.require_run(run_id) else {
            return;
        };
        if run.current_attempt_id.as_deref() != Some(attempt_id)
            || run.state != RunState::Running
            || self
                .has_cancellation_effect(run_id, attempt_id)
                .unwrap_or(true)
        {
            return;
        }
        let Some(execution) = &self.inner.execution else {
            return;
        };
        let observation = if capture.end_present && !capture.stream_error {
            match execution.workflow_result(&receipt.identity).await {
                Ok(Some(bytes)) if bytes.len() <= 64 * 1024 => {
                    if self
                        .write_unique_evidence(
                            run_id,
                            attempt_id,
                            "workflow-result",
                            JSON_MEDIA_TYPE,
                            &bytes,
                        )
                        .is_err()
                    {
                        let _ = self.transition_attempt_if_current(
                            run_id,
                            attempt_id,
                            &receipt.identity,
                            RunState::Unknown,
                        );
                        return;
                    }
                }
                Ok(None) => {}
                _ => {
                    let _ = self.transition_attempt_if_current(
                        run_id,
                        attempt_id,
                        &receipt.identity,
                        RunState::Unknown,
                    );
                    return;
                }
            }
            execution.finalize_one_shot(&receipt.identity).await.ok()
        } else {
            None
        };
        let _current = match self.require_run(run_id) {
            Ok(current)
                if current.current_attempt_id.as_deref() == Some(attempt_id)
                    && current.state == RunState::Running
                    && self.identity_is_registered(&receipt.identity) =>
            {
                current
            }
            _ => return,
        };
        let Some(observation) = observation else {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        };
        if self.inner.verified_finalization.is_some() {
            self.finalize_one_shot_with_verified(snapshot, receipt, capture, observation)
                .await;
            return;
        }
        if self
            .has_cancellation_effect(run_id, attempt_id)
            .unwrap_or(true)
        {
            return;
        }
        let mut assessment =
            match self.assessment_from_host(run_id, attempt_id, &capture, &observation) {
                Ok(assessment) => assessment,
                Err(_) => {
                    let _ = self.transition_attempt_if_current(
                        run_id,
                        attempt_id,
                        &receipt.identity,
                        RunState::Unknown,
                    );
                    return;
                }
            };
        let mut source_inspection = match execution.source_inspection(&receipt.identity).await {
            Ok(source) => source,
            Err(_) => {
                let _ = self.transition_attempt_if_current(
                    run_id,
                    attempt_id,
                    &receipt.identity,
                    RunState::Unknown,
                );
                return;
            }
        };
        if let Some(source) = &source_inspection {
            if source.run_id != run_id.as_str()
                || source.attempt_id != attempt_id.as_str()
                || source.base_commit != snapshot.base_commit
                || source.branch != snapshot.workspace_branch
                || !super::model::is_full_commit_id(&source.head_commit)
            {
                let _ = self.transition_attempt_if_current(
                    run_id,
                    attempt_id,
                    &receipt.identity,
                    RunState::Unknown,
                );
                return;
            }
            let bytes = match serde_json::to_vec(&source) {
                Ok(bytes) => bytes,
                Err(_) => {
                    let _ = self.transition_attempt_if_current(
                        run_id,
                        attempt_id,
                        &receipt.identity,
                        RunState::Unknown,
                    );
                    return;
                }
            };
            if self
                .write_unique_evidence(
                    run_id,
                    attempt_id,
                    "source-inspection",
                    JSON_MEDIA_TYPE,
                    &bytes,
                )
                .is_err()
            {
                let _ = self.transition_attempt_if_current(
                    run_id,
                    attempt_id,
                    &receipt.identity,
                    RunState::Unknown,
                );
                return;
            }
            assessment.commit_consistent =
                source.uncommitted_diff.as_ref().is_some_and(Vec::is_empty)
                    && source.new_files.is_empty();
            assessment.commit_id = assessment
                .commit_consistent
                .then(|| source.head_commit.clone());
        }
        let _commit_guard = RunLockLease::acquire(self.run_lock(run_id)).await;
        let latest = match self.require_run(run_id) {
            Ok(latest)
                if latest.current_attempt_id.as_deref() == Some(attempt_id)
                    && latest.state == RunState::Running
                    && self.identity_is_registered(&receipt.identity) =>
            {
                latest
            }
            _ => return,
        };
        // Persist the required phase before entering any resumable finalization state.
        let required = serde_json::json!({"run_id":run_id.as_str(),"attempt_id":attempt_id});
        if self
            .write_unique_evidence(
                run_id,
                attempt_id,
                "local-commit-required",
                JSON_MEDIA_TYPE,
                &serde_json::to_vec(&required).expect("JSON object serialization"),
            )
            .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        if self
            .write_unique_evidence(
                run_id,
                attempt_id,
                "issue-result-required",
                JSON_MEDIA_TYPE,
                &serde_json::to_vec(&required).expect("JSON object serialization"),
            )
            .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        if snapshot.semantic_judge_enabled
            && self
                .write_unique_evidence(
                    run_id,
                    attempt_id,
                    "ordinary-semantic-required",
                    JSON_MEDIA_TYPE,
                    &serde_json::to_vec(&required).expect("JSON object serialization"),
                )
                .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        if self
            .inner
            .store
            .transition(run_id, latest.revision, RunState::Finalizing)
            .is_err()
        {
            return;
        }
        let host_complete = host_finalization_complete(&observation, &assessment);
        if self
            .persist_host_finalization_evidence(run_id, attempt_id, &observation)
            .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        assessment.report_reference = match self.finalize_report_evidence(
            run_id,
            attempt_id,
            observation.report.as_deref(),
            host_complete,
        ) {
            Ok(reference) => reference,
            Err(_) => {
                let _ = self.transition_attempt_if_current(
                    run_id,
                    attempt_id,
                    &receipt.identity,
                    RunState::Unknown,
                );
                return;
            }
        };
        assessment.report_save_confirmed = assessment.report_reference.is_some();
        assessment.evidence_integrity_verified = assessment.report_save_confirmed
            || assessment.stdout_reference.is_some()
            || assessment.stderr_reference.is_some();
        let canonical_report = self
            .verify_canonical_report_evidence(run_id, attempt_id, &assessment)
            .unwrap_or(false);
        if (!host_complete || !assessment_is_successful(&assessment) || !canonical_report)
            && self
                .inner
                .store
                .record_execution_evidence(run_id, attempt_id, assessment.clone())
                .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        if !host_complete {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        let latest = match self.require_run(run_id) {
            Ok(latest)
                if latest.current_attempt_id.as_deref() == Some(attempt_id)
                    && latest.state == RunState::Finalizing
                    && self.identity_is_registered(&receipt.identity) =>
            {
                latest
            }
            Err(_) => return,
            _ => return,
        };
        let _verifying =
            match self
                .inner
                .store
                .transition(run_id, latest.revision, RunState::Verifying)
            {
                Ok(verifying) => verifying,
                Err(_) => return,
            };
        if !assessment_is_successful(&assessment) {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Failed,
            );
            return;
        }
        let current = match self.require_run(run_id) {
            Ok(current)
                if current.state == RunState::Verifying
                    && current.current_attempt_id.as_deref() == Some(attempt_id) =>
            {
                current
            }
            _ => return,
        };
        if self
            .has_cancellation_effect(run_id, attempt_id)
            .unwrap_or(true)
        {
            return;
        }
        if !canonical_report {
            let skipped = serde_json::json!({"schema_version":1,"run_id":run_id.as_str(),"attempt_id":attempt_id,"status":"skipped"});
            if self
                .write_unique_evidence(
                    run_id,
                    attempt_id,
                    "local-commit-result",
                    JSON_MEDIA_TYPE,
                    &serde_json::to_vec(&skipped).expect("JSON object serialization"),
                )
                .is_err()
            {
                return;
            }
            if self
                .write_unique_evidence(
                    run_id,
                    attempt_id,
                    "issue-result-ack",
                    JSON_MEDIA_TYPE,
                    &serde_json::to_vec(&skipped).expect("JSON object serialization"),
                )
                .is_err()
            {
                return;
            }
            let _ = self.complete_verified_run(current);
            return;
        }
        drop(_commit_guard);
        let contract = match self.inner.store.approval_assessment_contract(run_id) {
            Ok(ApprovalAssessmentContractStatus::Versioned { contract, .. }) => Some(contract),
            Ok(ApprovalAssessmentContractStatus::LegacyIncomplete) => None,
            Err(_) => {
                let _ = self.transition_attempt_if_current(
                    run_id,
                    attempt_id,
                    &receipt.identity,
                    RunState::Unknown,
                );
                return;
            }
        };
        if let Some(contract) = contract
            && contract
                .validation_plan
                .iter()
                .any(|definition| definition.command.is_some())
        {
            assessment.validation = match self.run_workspace_validations(receipt, &contract).await {
                Ok(validation) => validation,
                Err(_) => {
                    let _ = self.transition_attempt_if_current(
                        run_id,
                        attempt_id,
                        &receipt.identity,
                        RunState::Unknown,
                    );
                    return;
                }
            };
            if assessment.validation.state == ValidationState::Failed {
                if self
                    .inner
                    .store
                    .record_execution_evidence(run_id, attempt_id, assessment.clone())
                    .is_err()
                {
                    return;
                }
                let _ = self.transition_attempt_if_current(
                    run_id,
                    attempt_id,
                    &receipt.identity,
                    RunState::Failed,
                );
                return;
            }
            source_inspection = match execution.source_inspection(&receipt.identity).await {
                Ok(source) => source,
                Err(_) => {
                    let _ = self.transition_attempt_if_current(
                        run_id,
                        attempt_id,
                        &receipt.identity,
                        RunState::Unknown,
                    );
                    return;
                }
            };
        }
        let _commit_guard = RunLockLease::acquire(self.run_lock(run_id)).await;
        if self
            .has_cancellation_effect(run_id, attempt_id)
            .unwrap_or(true)
        {
            return;
        }
        let intent = serde_json::json!({"run_id":run_id.as_str(),"attempt_id":attempt_id,"base_commit":snapshot.base_commit,
            "branch":snapshot.workspace_branch,"report_reference":assessment.report_reference});
        let intent_bytes = match serde_json::to_vec(&intent) {
            Ok(bytes) => bytes,
            Err(_) => return,
        };
        if self
            .write_unique_evidence(
                run_id,
                attempt_id,
                "local-commit-intent",
                JSON_MEDIA_TYPE,
                &intent_bytes,
            )
            .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        let started = match execution.begin_workspace_commit(&receipt.identity).await {
            Ok(started) => started,
            Err(_) => {
                let _ = self.transition_attempt_if_current(
                    run_id,
                    attempt_id,
                    &receipt.identity,
                    RunState::Unknown,
                );
                return;
            }
        };
        if let Some(started) = &started {
            let bytes = match serde_json::to_vec(started) {
                Ok(bytes) => bytes,
                Err(_) => return,
            };
            if self
                .write_unique_evidence(
                    run_id,
                    attempt_id,
                    "local-commit-start",
                    JSON_MEDIA_TYPE,
                    &bytes,
                )
                .is_err()
            {
                let _ = self.transition_attempt_if_current(
                    run_id,
                    attempt_id,
                    &receipt.identity,
                    RunState::Unknown,
                );
                return;
            }
        }
        drop(_commit_guard);
        let completed = if started.is_some() {
            execution.commit_workspace(&receipt.identity).await
        } else {
            Ok(None)
        };
        if started.is_some() {
            match execution.commit_workflow_result(&receipt.identity).await {
                Ok(Some(bytes)) if bytes.len() <= 64 * 1024 => {
                    if self
                        .write_unique_evidence(
                            run_id,
                            attempt_id,
                            "local-commit-workflow-result",
                            JSON_MEDIA_TYPE,
                            &bytes,
                        )
                        .is_err()
                    {
                        let _ = self.transition_attempt_if_current(
                            run_id,
                            attempt_id,
                            &receipt.identity,
                            RunState::Unknown,
                        );
                        return;
                    }
                }
                Ok(None) => {}
                _ => {
                    let _ = self.transition_attempt_if_current(
                        run_id,
                        attempt_id,
                        &receipt.identity,
                        RunState::Unknown,
                    );
                    return;
                }
            }
        }
        let phase_result = serde_json::json!({"schema_version":1,"run_id":run_id.as_str(),"attempt_id":attempt_id,
            "status":if matches!(&completed,Ok(Some(_))){"completed"}else{"skipped"}});
        match completed {
            Ok(Some(committed)) => {
                let source = &committed.source;
                if source.run_id != run_id.as_str()
                    || source.attempt_id != attempt_id.as_str()
                    || source.base_commit != snapshot.base_commit
                    || source.branch != snapshot.workspace_branch
                    || !super::model::is_full_commit_id(&source.head_commit)
                {
                    let _ = self.transition_attempt_if_current(
                        run_id,
                        attempt_id,
                        &receipt.identity,
                        RunState::Unknown,
                    );
                    return;
                }
                let bytes = match serde_json::to_vec(&committed) {
                    Ok(bytes) => bytes,
                    Err(_) => return,
                };
                if self
                    .write_unique_evidence(
                        run_id,
                        attempt_id,
                        "local-commit",
                        JSON_MEDIA_TYPE,
                        &bytes,
                    )
                    .is_err()
                {
                    let _ = self.transition_attempt_if_current(
                        run_id,
                        attempt_id,
                        &receipt.identity,
                        RunState::Unknown,
                    );
                    return;
                }
                if committed.workflow_result.exit_code != 0 {
                    let _ = self.transition_attempt_if_current(
                        run_id,
                        attempt_id,
                        &receipt.identity,
                        RunState::Failed,
                    );
                    return;
                }
                assessment.commit_consistent =
                    source.uncommitted_diff.as_ref().is_some_and(Vec::is_empty)
                        && source.new_files.is_empty();
                if !assessment.commit_consistent {
                    let _ = self.transition_attempt_if_current(
                        run_id,
                        attempt_id,
                        &receipt.identity,
                        RunState::Unknown,
                    );
                    return;
                }
                assessment.commit_id = assessment
                    .commit_consistent
                    .then(|| source.head_commit.clone());
                source_inspection = Some(committed.source);
            }
            Ok(None) if started.is_none() => {
                // A skipped commit must still describe the same source observed before dispatch.
                if execution
                    .source_inspection(&receipt.identity)
                    .await
                    .ok()
                    .as_ref()
                    != Some(&source_inspection)
                {
                    let _ = self.transition_attempt_if_current(
                        run_id,
                        attempt_id,
                        &receipt.identity,
                        RunState::Unknown,
                    );
                    return;
                }
            }
            Ok(None) => {
                let _ = self.transition_attempt_if_current(
                    run_id,
                    attempt_id,
                    &receipt.identity,
                    RunState::Unknown,
                );
                return;
            }
            Err(_) => {
                let _ = self.transition_attempt_if_current(
                    run_id,
                    attempt_id,
                    &receipt.identity,
                    RunState::Unknown,
                );
                return;
            }
        }
        if self
            .has_cancellation_effect(run_id, attempt_id)
            .unwrap_or(true)
        {
            return;
        }
        let semantic_intent = serde_json::json!({
            "schema_version": 1, "run_id": run_id.as_str(), "attempt_id": attempt_id,
            "source": source_inspection, "report_reference": assessment.report_reference,
            "validation": assessment.validation,
        });
        if snapshot.semantic_judge_enabled
            && self
                .write_unique_evidence(
                    run_id,
                    attempt_id,
                    "ordinary-semantic-intent",
                    JSON_MEDIA_TYPE,
                    &serde_json::to_vec(&semantic_intent).expect("JSON object serialization"),
                )
                .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        let semantic = if snapshot.semantic_judge_enabled {
            execution
                .assess_semantics(
                    &receipt.identity,
                    source_inspection.as_ref(),
                    observation.report.as_deref().unwrap_or_default(),
                    &assessment.validation,
                )
                .await
                .unwrap_or_else(|_| {
                    super::semantic_judge::SemanticAssessment::inconclusive("judge_unavailable")
                })
        } else {
            super::semantic_judge::SemanticAssessment::inconclusive("semantic_disabled")
        };
        if self
            .has_cancellation_effect(run_id, attempt_id)
            .unwrap_or(true)
        {
            return;
        }
        if execution
            .source_inspection(&receipt.identity)
            .await
            .ok()
            .as_ref()
            != Some(&source_inspection)
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        let semantic_result = serde_json::json!({
            "schema_version": 1, "run_id": run_id.as_str(), "attempt_id": attempt_id,
            "status": if snapshot.semantic_judge_enabled { "completed" } else { "skipped" },
            "outcome": semantic.outcome, "assessment": semantic,
        });
        if self
            .write_unique_evidence(
                run_id,
                attempt_id,
                "ordinary-semantic-result",
                JSON_MEDIA_TYPE,
                &serde_json::to_vec(&semantic_result).expect("JSON object serialization"),
            )
            .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        assessment.acceptance_conditions_met = snapshot.semantic_judge_enabled
            && semantic.outcome == super::assessment::AssessmentUse::Pass;
        let committed_id = assessment.commit_id.clone();
        let host_validation = assessment.validation.clone();
        if self
            .inner
            .store
            .record_execution_evidence(run_id, attempt_id, assessment)
            .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        let _result_guard = RunLockLease::acquire(self.run_lock(run_id)).await;
        if self
            .write_unique_evidence(
                run_id,
                attempt_id,
                "local-commit-result",
                JSON_MEDIA_TYPE,
                &serde_json::to_vec(&phase_result).expect("JSON object serialization"),
            )
            .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        if self
            .has_cancellation_effect(run_id, attempt_id)
            .unwrap_or(true)
        {
            return;
        }
        if !self.require_run(run_id).is_ok_and(|run| {
            run.state == RunState::Verifying
                && run.current_attempt_id.as_deref() == Some(attempt_id)
                && self.identity_is_registered(&receipt.identity)
        }) {
            return;
        }
        drop(_result_guard);
        let mut publication_result = None;
        if self.inner.automatic_publication
            && snapshot.publication_plan != PublicationPlan::NoPublication
        {
            match self.publish(run_id).await {
                Ok(run) if run.state == RunState::Publishing => {
                    publication_result = self.confirmed_publication(&run).ok().flatten();
                    if publication_result.is_none() {
                        let _ = self.transition_if_allowed(&run, RunState::Unknown);
                        return;
                    }
                }
                Ok(_) => return,
                Err(_) => {
                    let _ = self.transition_attempt_if_current(
                        run_id,
                        attempt_id,
                        &receipt.identity,
                        RunState::Unknown,
                    );
                    return;
                }
            }
        }
        let _result_guard = RunLockLease::acquire(self.run_lock(run_id)).await;
        if !self.require_run(run_id).is_ok_and(|run| {
            matches!(run.state, RunState::Verifying | RunState::Publishing)
                && run.current_attempt_id.as_deref() == Some(attempt_id)
                && self.identity_is_registered(&receipt.identity)
        }) || self
            .has_cancellation_effect(run_id, attempt_id)
            .unwrap_or(true)
        {
            return;
        }
        let commit_id = committed_id.as_deref();
        let Some(report) = observation.report.as_deref() else {
            return;
        };
        let agent_report = match serde_json::from_slice::<serde_json::Value>(report) {
            Ok(report) => report,
            Err(_) => return,
        };
        let result_report=serde_json::to_vec(&serde_json::json!({
            "agent_report":agent_report,"semantic_assessment":semantic_result,"host_validation":host_validation,
            "publication":publication_result,
        })).expect("JSON object serialization");
        if self
            .write_unique_evidence(
                run_id,
                attempt_id,
                "issue-result-report",
                JSON_MEDIA_TYPE,
                &result_report,
            )
            .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        let report = result_report.as_slice();
        let result_intent = serde_json::json!({"run_id":run_id.as_str(),"attempt_id":attempt_id,"commit_id":commit_id,"report_reference":self.require_run(run_id).ok().and_then(|run|run.execution_evidence).and_then(|assessment|assessment.report_reference)});
        if self
            .write_unique_evidence(
                run_id,
                attempt_id,
                "issue-result-intent",
                JSON_MEDIA_TYPE,
                &serde_json::to_vec(&result_intent).expect("JSON object serialization"),
            )
            .is_err()
        {
            return;
        }
        if execution
            .begin_issue_result(&receipt.identity, commit_id, report)
            .await
            .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        drop(_result_guard);
        let saved = execution
            .save_issue_result(&receipt.identity, commit_id, report)
            .await;
        let _result_guard = RunLockLease::acquire(self.run_lock(run_id)).await;
        let ack = match saved {
            Ok(Some(bytes)) if bytes.len() <= super::issue_gateway::MAX_ISSUE_RESULT_ACK_BYTES => {
                match serde_json::from_slice::<serde_json::Value>(&bytes) {
                    Ok(value) => {
                        serde_json::json!({"schema_version":1,"run_id":run_id.as_str(),"attempt_id":attempt_id,"status":"completed","response":value})
                    }
                    Err(_) => {
                        let _ = self.transition_attempt_if_current(
                            run_id,
                            attempt_id,
                            &receipt.identity,
                            RunState::Unknown,
                        );
                        return;
                    }
                }
            }
            Ok(None) => {
                serde_json::json!({"schema_version":1,"run_id":run_id.as_str(),"attempt_id":attempt_id,"status":"skipped"})
            }
            _ => {
                let _ = self.transition_attempt_if_current(
                    run_id,
                    attempt_id,
                    &receipt.identity,
                    RunState::Unknown,
                );
                return;
            }
        };
        let ack_bytes = serde_json::to_vec(&ack).expect("JSON object serialization");
        if ack_bytes.len() > super::issue_gateway::MAX_ISSUE_RESULT_ACK_BYTES
            || self
                .write_unique_evidence(
                    run_id,
                    attempt_id,
                    "issue-result-ack",
                    JSON_MEDIA_TYPE,
                    &ack_bytes,
                )
                .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        if let Ok(current) = self.require_run(run_id)
            && matches!(current.state, RunState::Verifying | RunState::Publishing)
            && current.current_attempt_id.as_deref() == Some(attempt_id)
            && !self
                .has_cancellation_effect(run_id, attempt_id)
                .unwrap_or(true)
        {
            let _ = self.complete_verified_run(current);
        }
    }

    async fn finalize_one_shot_with_verified(
        &self,
        snapshot: &AgentRunSnapshot,
        receipt: &StartReceipt,
        capture: CapturedOutput,
        observation: HostFinalizationObservation,
    ) {
        let run_id = &snapshot.run_id;
        let attempt_id = receipt.identity.attempt_id.as_str();
        let mut run_guard = RunLockLease::acquire(self.run_lock(run_id)).await;
        let Ok(run) = self.require_run(run_id) else {
            return;
        };
        if run.current_attempt_id.as_deref() != Some(attempt_id)
            || run.state != RunState::Running
            || self
                .has_cancellation_effect(run_id, attempt_id)
                .unwrap_or(true)
        {
            return;
        }

        let contract = match self.inner.store.approval_assessment_contract(run_id) {
            Ok(ApprovalAssessmentContractStatus::Versioned { contract, .. }) => Some(*contract),
            _ => None,
        };
        let typed_finalization = if let Some(execution) = self.inner.execution.as_ref() {
            match execution.finalization_evidence(&receipt.identity).await {
                Ok(typed)
                    if typed.identity == receipt.identity
                        && typed_finalization_matches_observation(&typed, &observation) =>
                {
                    Some(typed)
                }
                Ok(_) | Err(_) => None,
            }
        } else {
            None
        };
        let verified = if let (Some(sources), Some(contract), Some(typed)) = (
            self.inner.verified_finalization.as_ref(),
            contract.as_ref(),
            typed_finalization.as_ref(),
        ) {
            sources.reopen(snapshot, contract, typed).await.ok()
        } else {
            None
        };

        let prior_validation_failed = run.current_attempt_id.as_deref() == Some(attempt_id)
            && run
                .execution_evidence
                .as_ref()
                .is_some_and(|evidence| evidence.validation.state == ValidationState::Failed);
        let host_validation_failed = observation.validation.state == ValidationState::Failed;
        let mut assessment =
            match self.assessment_from_host(run_id, attempt_id, &capture, &observation) {
                Ok(assessment) => assessment,
                Err(_) => {
                    let _ = self.transition_attempt_if_current(
                        run_id,
                        attempt_id,
                        &receipt.identity,
                        RunState::Unknown,
                    );
                    return;
                }
            };
        if let Some(contract) = contract.as_ref() {
            let required = contract
                .validation_plan
                .iter()
                .any(|validation| validation.required);
            assessment.validation = ValidationEvidence {
                required,
                state: verified
                    .as_ref()
                    .map_or(ValidationState::NotRun, |verified| {
                        verified_validation_state(verified, contract)
                    }),
            };
        } else {
            assessment.validation.state = ValidationState::NotRun;
            assessment.acceptance_conditions_met = false;
        }
        if let Some(verified) = verified.as_ref() {
            let frozen_commit = match &observation.workspace_freeze {
                super::WorkspaceFreezeStatus::HostVerified { commit_id }
                    if commit_id == &verified.artifact.commit =>
                {
                    Some(commit_id.clone())
                }
                _ => None,
            };
            assessment.commit_id = frozen_commit;
            assessment.commit_consistent = assessment.commit_id.is_some();
        } else {
            assessment.commit_id = None;
            assessment.commit_consistent = false;
            assessment.validation.state = ValidationState::NotRun;
            assessment.acceptance_conditions_met = false;
        }
        if prior_validation_failed || host_validation_failed {
            assessment.validation.state = ValidationState::Failed;
        }

        let latest = match self.require_run(run_id) {
            Ok(latest)
                if latest.current_attempt_id.as_deref() == Some(attempt_id)
                    && latest.state == RunState::Running
                    && self.identity_is_registered(&receipt.identity) =>
            {
                latest
            }
            _ => return,
        };
        let Ok(finalizing) =
            self.inner
                .store
                .transition(run_id, latest.revision, RunState::Finalizing)
        else {
            return;
        };
        if self
            .persist_host_finalization_evidence(run_id, attempt_id, &observation)
            .is_err()
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        let artifact_reference = match verified.as_ref() {
            Some(verified) => {
                match self.persist_verified_artifact_evidence(run_id, attempt_id, verified) {
                    Ok(reference) => Some(reference),
                    Err(_) => {
                        let _ = self.transition_attempt_if_current(
                            run_id,
                            attempt_id,
                            &receipt.identity,
                            RunState::Unknown,
                        );
                        return;
                    }
                }
            }
            None => None,
        };
        let host_complete = verified.is_some()
            && artifact_reference.is_some()
            && capture.end_present
            && !capture.stream_error
            && capture.capture_complete
            && capture.exit_code == Some(0)
            && verified.as_ref().is_some_and(|verified| {
                typed_physical_finalization_succeeded(&observation, &capture, verified)
            });
        assessment.report_reference = match self.finalize_report_evidence(
            run_id,
            attempt_id,
            observation.report.as_deref(),
            host_complete,
        ) {
            Ok(reference) => reference,
            Err(_) => {
                let _ = self.transition_if_allowed(&finalizing, RunState::Unknown);
                return;
            }
        };
        assessment.report_save_confirmed = assessment.report_reference.is_some();
        assessment.evidence_integrity_verified = assessment.report_save_confirmed
            || assessment.stdout_reference.is_some()
            || assessment.stderr_reference.is_some();
        if !host_complete {
            let _ = self
                .inner
                .store
                .record_execution_evidence(run_id, attempt_id, assessment);
            let _ = self.transition_if_allowed(
                &self.require_run(run_id).unwrap_or(finalizing),
                RunState::Unknown,
            );
            return;
        }

        let semantic = if !run.snapshot.semantic_judge_enabled {
            SemanticEvaluation::not_required()
        } else {
            match (
                contract.as_ref(),
                verified.as_ref(),
                artifact_reference.as_ref(),
                typed_finalization.as_ref(),
            ) {
                (Some(contract), Some(verified), Some(artifact_reference), Some(typed))
                    if contract.can_dispatch_semantic_assessment()
                        && assessment.validation.state != ValidationState::Failed =>
                {
                    let (finalization_inputs_reference, _, manifest_reference) = match self
                        .persist_verified_finalization_base(
                            run_id,
                            attempt_id,
                            typed,
                            verified,
                            &capture,
                            &assessment,
                        )
                        .await
                    {
                        Ok(references) => references,
                        Err(_) => {
                            let _ = self.transition_attempt_if_current(
                                run_id,
                                attempt_id,
                                &receipt.identity,
                                RunState::Unknown,
                            );
                            return;
                        }
                    };
                    let latest = match self.require_run(run_id) {
                        Ok(latest)
                            if latest.current_attempt_id.as_deref() == Some(attempt_id)
                                && latest.state == RunState::Finalizing =>
                        {
                            latest
                        }
                        _ => return,
                    };
                    self.evaluate_claimed_semantics(
                        &latest,
                        attempt_id,
                        contract,
                        verified,
                        Some(&mut run_guard),
                        AssessmentEvidenceReferences {
                            artifact: artifact_reference,
                            finalization_inputs: &finalization_inputs_reference,
                            snapshot_manifest: &manifest_reference,
                        },
                    )
                    .await
                }
                (Some(contract), _, _, _)
                    if contract.criteria.iter().any(|criterion| {
                        criterion.method == super::model::CriterionEvaluationMethod::SemanticJudge
                    }) =>
                {
                    SemanticEvaluation::inconclusive(
                        "approved_semantic_policy_or_host_validation_unsupported",
                    )
                }
                (Some(_), _, _, _) => SemanticEvaluation::not_required(),
                _ => SemanticEvaluation::inconclusive(
                    "approved_contract_or_verified_artifact_missing",
                ),
            }
        };
        if !self.claimed_finalization_is_current(run_id, attempt_id, &semantic) {
            return;
        }
        let requires_claim = run.snapshot.semantic_judge_enabled
            && contract
                .as_ref()
                .is_some_and(ApprovalAssessmentContract::can_dispatch_semantic_assessment)
            && self.inner.claimed_assessment.is_some()
            && assessment.validation.state != ValidationState::Failed;
        if requires_claim && semantic.assessment_id.is_none() {
            let _ = self.persist_semantic_assessment_evidence(run_id, attempt_id, &semantic);
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        if semantic.is_pending() {
            let _ = self.persist_semantic_assessment_evidence(run_id, attempt_id, &semantic);
            return;
        }
        assessment.acceptance_conditions_met = semantic.acceptance_conditions_met;
        if self
            .persist_semantic_assessment_evidence(run_id, attempt_id, &semantic)
            .is_err()
        {
            return;
        }
        if self
            .has_cancellation_effect(run_id, attempt_id)
            .unwrap_or(true)
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        if self
            .inner
            .store
            .record_execution_evidence(run_id, attempt_id, assessment)
            .is_err()
        {
            return;
        }
        if self
            .has_cancellation_effect(run_id, attempt_id)
            .unwrap_or(true)
        {
            let _ = self.transition_attempt_if_current(
                run_id,
                attempt_id,
                &receipt.identity,
                RunState::Unknown,
            );
            return;
        }
        let latest = match self.require_run(run_id) {
            Ok(latest)
                if latest.current_attempt_id.as_deref() == Some(attempt_id)
                    && latest.state == RunState::Finalizing =>
            {
                latest
            }
            _ => return,
        };
        let verifying =
            match self
                .inner
                .store
                .transition(run_id, latest.revision, RunState::Verifying)
            {
                Ok(verifying) => verifying,
                Err(_) => return,
            };
        let _ = self.complete_verified_run(verifying);
    }

    fn persist_verified_artifact_evidence(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        verified: &VerifiedFinalization,
    ) -> Result<super::EvidenceReference, CoordinatorError> {
        let receipt_digests = verified
            .validation_receipts
            .iter()
            .map(|receipt| {
                serde_json::to_vec(receipt)
                    .map(|bytes| sha256_hex(&bytes))
                    .map_err(|_| CoordinatorError::Conflict)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let evidence = VerifiedArtifactEvidenceV1 {
            schema_version: 1,
            run_id: run_id.to_string(),
            attempt_id: attempt_id.to_owned(),
            resolved_artifact: verified.artifact.clone(),
            full_commit_id: verified.target.full_commit_id().to_owned(),
            full_tree_id: verified.target.full_tree_id().to_owned(),
            untracked_manifest_digest: verified.target.untracked_manifest_digest().to_owned(),
            target_snapshot_digest: verified.target.digest().to_owned(),
            validation_receipt_digests: receipt_digests,
        };
        let bytes = serde_json::to_vec(&evidence).map_err(|_| CoordinatorError::Conflict)?;
        let reference = self.write_unique_evidence(
            run_id,
            attempt_id,
            "verified-sealed-artifact",
            JSON_MEDIA_TYPE,
            &bytes,
        )?;
        if reference.attempt_id != attempt_id
            || reference.name != "verified-sealed-artifact"
            || reference.media_type != JSON_MEDIA_TYPE
            || reference.sha256 != sha256_hex(&bytes)
            || self
                .inner
                .store
                .read_evidence(run_id, &reference.evidence_id)?
                != bytes
        {
            return Err(CoordinatorError::Conflict);
        }
        Ok(reference)
    }

    async fn persist_verified_finalization_base(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        typed: &TypedFinalizationEvidence,
        verified: &VerifiedFinalization,
        capture: &CapturedOutput,
        assessment: &RunEvidenceAssessment,
    ) -> Result<(EvidenceReference, EvidenceReference, EvidenceReference), CoordinatorError> {
        let evidence = self.inner.store.evidence(run_id)?;
        let find_reference = |evidence_id: &str, expected_name: &str| {
            evidence
                .iter()
                .find(|reference| {
                    reference.evidence_id == evidence_id
                        && reference.attempt_id == attempt_id
                        && reference.name == expected_name
                        && reference.save_confirmed
                })
                .cloned()
                .ok_or(CoordinatorError::Conflict)
        };
        let report_id = assessment
            .report_reference
            .as_deref()
            .ok_or(CoordinatorError::Conflict)?;
        let report_reference = find_reference(report_id, "agent-report")?;
        let stdout_reference = capture
            .stdout_reference
            .as_deref()
            .map(|id| find_reference(id, "stdout"))
            .transpose()?;
        let stderr_reference = capture
            .stderr_reference
            .as_deref()
            .map(|id| find_reference(id, "stderr"))
            .transpose()?;
        let capture_inputs = FinalizationCaptureInputs::new(
            report_reference,
            stdout_reference,
            stderr_reference,
            capture.end_present,
            capture.stream_error,
            assessment.cancelled,
        );
        let inputs = HostVerifiedFinalizationInputs::from_verified_finalization(
            typed,
            verified,
            capture_inputs,
        )?;
        let sources = self
            .inner
            .verified_finalization
            .as_ref()
            .ok_or(CoordinatorError::AdapterUnavailable)?;
        let manifest_bytes = sources
            .artifact_manifest_bytes(run_id.as_str(), attempt_id)
            .await
            .map_err(|_| CoordinatorError::AdapterUnavailable)?
            .ok_or(CoordinatorError::Conflict)?;
        if manifest_bytes.is_empty()
            || manifest_bytes.len() > super::report::MAX_REPORT_BYTES
            || sha256_hex(&manifest_bytes) != verified.artifact.manifest_digest
        {
            return Err(CoordinatorError::Conflict);
        }
        let manifest_reference = self.write_unique_evidence(
            run_id,
            attempt_id,
            "snapshot-manifest",
            JSON_MEDIA_TYPE,
            &manifest_bytes,
        )?;
        if manifest_reference.sha256 != verified.artifact.manifest_digest {
            return Err(CoordinatorError::Conflict);
        }
        let current = self.require_run(run_id)?;
        if current.current_attempt_id.as_deref() != Some(attempt_id)
            || current.state != RunState::Finalizing
        {
            return Err(CoordinatorError::Conflict);
        }
        let index_reference = self.inner.store.write_verified_finalization_inputs(
            run_id,
            current.revision,
            attempt_id,
            inputs,
        )?;
        let base = VerifiedFinalizationAssessmentBaseV1 {
            schema_version: 1,
            run_id: run_id.to_string(),
            attempt_id: attempt_id.to_owned(),
            finalization_inputs_evidence_id: index_reference.evidence_id.clone(),
            assessment: assessment.clone(),
            capture_complete: capture.capture_complete,
            invocation_kind: typed.invocation_kind,
            cli_exit: typed.cli_exit.clone(),
            shell_observation: typed.shell_observation.clone(),
            child_writers: typed.child_writers,
            workspace_freeze: typed.workspace_freeze.clone(),
        };
        let bytes = serde_json::to_vec(&base).map_err(|_| CoordinatorError::Conflict)?;
        let base_reference = self.write_unique_evidence(
            run_id,
            attempt_id,
            "verified-finalization-assessment-base-v1",
            JSON_MEDIA_TYPE,
            &bytes,
        )?;
        if base_reference.attempt_id != attempt_id
            || self
                .inner
                .store
                .read_evidence(run_id, &base_reference.evidence_id)?
                != bytes
        {
            return Err(CoordinatorError::Conflict);
        }
        Ok((index_reference, base_reference, manifest_reference))
    }

    fn load_verified_finalization_base(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        saved: &super::store::SavedVerifiedFinalizationInputs,
    ) -> Result<(VerifiedFinalizationAssessmentBaseV1, ValidatedReport), CoordinatorError> {
        let reference = self
            .inner
            .store
            .evidence(run_id)?
            .into_iter()
            .find(|reference| {
                reference.attempt_id == attempt_id
                    && reference.name == "verified-finalization-assessment-base-v1"
                    && reference.media_type == JSON_MEDIA_TYPE
                    && reference.save_confirmed
            })
            .ok_or(CoordinatorError::Conflict)?;
        let bytes = self
            .inner
            .store
            .read_evidence(run_id, &reference.evidence_id)?;
        let base: VerifiedFinalizationAssessmentBaseV1 =
            serde_json::from_slice(&bytes).map_err(|_| CoordinatorError::Conflict)?;
        let report_reference = saved.report_reference();
        let saved_stdout = saved.stdout_reference();
        let saved_stderr = saved.stderr_reference();
        if base.schema_version != 1
            || base.run_id != run_id.as_str()
            || base.attempt_id != attempt_id
            || base.finalization_inputs_evidence_id != saved.index_reference().evidence_id
            || !base.capture_complete
            || base.assessment.report_reference.as_deref()
                != Some(report_reference.evidence_id.as_str())
            || !base.assessment.report_save_confirmed
            || base.assessment.stdout_reference.as_deref()
                != saved_stdout.map(|reference| reference.evidence_id.as_str())
            || base.assessment.stdout_bytes
                != saved_stdout.map_or(0, |reference| reference.size_bytes)
            || base.assessment.stderr_reference.as_deref()
                != saved_stderr.map(|reference| reference.evidence_id.as_str())
            || base.assessment.stderr_bytes
                != saved_stderr.map_or(0, |reference| reference.size_bytes)
            || reference.size_bytes != bytes.len() as u64
            || reference.sha256 != sha256_hex(&bytes)
        {
            return Err(CoordinatorError::Conflict);
        }
        let report_bytes = self
            .inner
            .store
            .read_evidence(run_id, &report_reference.evidence_id)?;
        let validated = self
            .validate_report_for_run(run_id, attempt_id, &report_bytes)
            .map_err(|_| CoordinatorError::Conflict)?;
        let canonical =
            serde_json::to_vec(&validated.report).map_err(|_| CoordinatorError::Conflict)?;
        if canonical != report_bytes
            || report_reference.media_type != JSON_MEDIA_TYPE
            || report_reference.attempt_id != attempt_id
            || !report_reference.save_confirmed
        {
            return Err(CoordinatorError::Conflict);
        }
        Ok((base, validated))
    }

    fn persist_semantic_assessment_evidence(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        evaluation: &SemanticEvaluation,
    ) -> Result<(), CoordinatorError> {
        let evidence = SemanticAssessmentEvidenceV1 {
            schema_version: 1,
            run_id: run_id.to_string(),
            attempt_id: attempt_id.to_owned(),
            assessment_id: evaluation.assessment_id.clone(),
            judge_input_digest: evaluation.judge_input_digest.clone(),
            judge_input_scope_digest: evaluation.judge_input_scope_digest.clone(),
            outcome: evaluation.outcome.clone(),
            acceptance_conditions_met: evaluation.acceptance_conditions_met,
        };
        let bytes = serde_json::to_vec(&evidence).map_err(|_| CoordinatorError::Conflict)?;
        let outcome_name = evaluation
            .outcome
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let name = if evaluation.is_pending() {
            if evaluation.assessment_id.is_none() {
                return Err(CoordinatorError::Conflict);
            }
            format!("semantic-pending-{}", sha256_hex(&bytes))
        } else {
            evaluation.assessment_id.as_ref().map_or_else(
                || format!("semantic-assessment-{outcome_name}"),
                |id| format!("semantic-assessment-{id}-terminal"),
            )
        };
        self.write_unique_evidence(run_id, attempt_id, &name, JSON_MEDIA_TYPE, &bytes)?;
        Ok(())
    }

    fn claimed_finalization_is_current(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        evaluation: &SemanticEvaluation,
    ) -> bool {
        self.require_run(run_id).is_ok_and(|current| {
            current.current_attempt_id.as_deref() == Some(attempt_id)
                && current.state == RunState::Finalizing
                && evaluation
                    .claim_revision
                    .is_none_or(|revision| current.revision == revision)
                && !self
                    .has_cancellation_effect(run_id, attempt_id)
                    .unwrap_or(true)
        })
    }

    async fn evaluate_claimed_semantics(
        &self,
        run: &AgentRunRecord,
        attempt_id: &str,
        contract: &ApprovalAssessmentContract,
        verified: &VerifiedFinalization,
        mut run_guard: Option<&mut RunLockLease>,
        references: AssessmentEvidenceReferences<'_>,
    ) -> SemanticEvaluation {
        if !run.snapshot.semantic_judge_enabled {
            return SemanticEvaluation::not_required();
        }
        if !contract.criteria.iter().any(|criterion| {
            criterion.method == super::model::CriterionEvaluationMethod::SemanticJudge
        }) {
            return SemanticEvaluation::not_required();
        }
        if !contract.can_dispatch_semantic_assessment()
            || contract.criteria.iter().any(|criterion| {
                criterion.method == super::model::CriterionEvaluationMethod::Unknown
            })
        {
            return SemanticEvaluation::inconclusive("approved_semantic_policy_unsupported");
        }
        let Some(config) = self.inner.claimed_assessment.as_ref() else {
            return SemanticEvaluation::inconclusive("approved_judge_provider_unavailable");
        };
        if config.timeout.is_zero() || config.timeout > Duration::from_secs(24 * 60 * 60) {
            return SemanticEvaluation::inconclusive("approved_judge_deadline_unsupported");
        }
        let Some(sources) = self.inner.verified_finalization.as_ref() else {
            return SemanticEvaluation::inconclusive("approved_context_source_unavailable");
        };

        let mut expected_fields =
            super::model::judge_input_fields_for_sources(&contract.input_scope.allowed_sources);
        expected_fields.sort();
        let mut approved_fields = contract.input_scope.allowed_fields.clone();
        approved_fields.sort();
        if expected_fields != approved_fields {
            return SemanticEvaluation::inconclusive("approved_source_mapping_unsupported");
        }

        let attempt_evidence = match self.inner.store.evidence(&run.snapshot.run_id) {
            Ok(evidence) => evidence,
            Err(_) => return SemanticEvaluation::inconclusive("approved_evidence_unavailable"),
        };
        let Some(report_reference) = attempt_evidence.iter().find(|reference| {
            reference.attempt_id == attempt_id
                && reference.name == "agent-report"
                && reference.media_type == JSON_MEDIA_TYPE
                && reference.save_confirmed
        }) else {
            return SemanticEvaluation::inconclusive("canonical_report_missing");
        };
        let report_bytes = match self
            .inner
            .store
            .read_evidence(&run.snapshot.run_id, &report_reference.evidence_id)
        {
            Ok(bytes) => bytes,
            Err(_) => return SemanticEvaluation::inconclusive("canonical_report_unreadable"),
        };
        let validated =
            match self.validate_report_for_run(&run.snapshot.run_id, attempt_id, &report_bytes) {
                Ok(report) => report,
                Err(_) => return SemanticEvaluation::inconclusive("canonical_report_rejected"),
            };
        let report_read = super::report::ReportReadResult {
            validated_report: Some(validated),
            rejection: None,
            observed_byte_count: report_bytes.len(),
            reported_file_size: Some(report_bytes.len() as u64),
            raw_bytes_sha256: sha256_hex(&report_bytes),
            bytes_complete: true,
            raw_bytes: report_bytes.clone(),
        };

        let mut evidence = Vec::new();
        for source in &contract.input_scope.allowed_sources {
            if source.kind == EvidenceSourceKind::Report {
                continue;
            }
            let resolved = match sources.resolve_assessment_source(
                run.snapshot.run_id.as_str(),
                attempt_id,
                source,
            ) {
                Ok(Some(bytes)) if bytes.len() <= source.max_bytes => bytes,
                Ok(Some(_)) => {
                    return SemanticEvaluation::inconclusive("approved_source_too_large");
                }
                Ok(None) | Err(_) => {
                    return SemanticEvaluation::inconclusive("approved_source_unavailable");
                }
            };
            let Ok(content) = String::from_utf8(resolved) else {
                return SemanticEvaluation::inconclusive("approved_source_not_utf8");
            };
            evidence.push(EvidencePayload {
                kind: source.kind.clone(),
                identity: source.identity.clone(),
                content,
            });
        }

        let task_kind = match run.snapshot.purpose {
            RunPurpose::Investigation => TaskKind::Investigation,
            RunPurpose::Implementation => TaskKind::Implementation,
        };
        let criteria = contract
            .criteria
            .iter()
            .map(|criterion| ApprovedCriterion {
                criterion_id: criterion.criterion_id.clone(),
                statement: criterion.statement.clone(),
                required: criterion.required,
                method: match criterion.method {
                    super::model::CriterionEvaluationMethod::SemanticJudge => {
                        CriterionMethod::SemanticJudge
                    }
                    super::model::CriterionEvaluationMethod::Deterministic => {
                        CriterionMethod::Deterministic
                    }
                    super::model::CriterionEvaluationMethod::Unknown => {
                        unreachable!("rejected above")
                    }
                },
            })
            .collect::<Vec<_>>();
        let mut validation_plan = Vec::new();
        for definition in &contract.validation_plan {
            let digest = if let Some(actual) = verified
                .validation_runs
                .iter()
                .find(|run| run.plan.validation_id() == definition.validation_id)
            {
                actual.plan.command_definition_digest().to_owned()
            } else if definition.command.is_none() {
                match serde_json::to_vec(definition) {
                    Ok(bytes) => sha256_hex(&bytes),
                    Err(_) => {
                        return SemanticEvaluation::inconclusive("approved_validation_invalid");
                    }
                }
            } else {
                return SemanticEvaluation::inconclusive("approved_validation_plan_unavailable");
            };
            validation_plan.push(ApprovedValidationDefinition {
                validation_id: definition.validation_id.clone(),
                required: definition.required,
                definition_digest: digest,
            });
        }
        let mut validation_receipts = Vec::new();
        for receipt in &verified.validation_receipts {
            let Ok(bytes) = serde_json::to_vec(receipt) else {
                return SemanticEvaluation::inconclusive("approved_validation_receipt_invalid");
            };
            validation_receipts.push(ValidationReceiptBinding {
                receipt_id: receipt.receipt_id().to_owned(),
                validation_id: receipt.validation_id().to_owned(),
                digest: sha256_hex(&bytes),
            });
        }

        let approval_snapshot_digest = match super::model::StoredAgentRunSnapshot::new(
            run.snapshot.clone(),
            Some(contract.clone()),
        )
        .and_then(|stored| {
            serde_json::to_vec(&stored)
                .map_err(|error| super::model::RunModelError::Invalid(error.to_string()))
        }) {
            Ok(bytes) => sha256_hex(&bytes),
            Err(_) => return SemanticEvaluation::inconclusive("approved_snapshot_invalid"),
        };
        let provider = contract.provider.clone();
        let Some(provider) = provider else {
            return SemanticEvaluation::inconclusive("approved_provider_missing");
        };
        let deadline_unix_ms = current_unix_ms()
            .saturating_add(config.timeout.as_millis().min(u64::MAX as u128) as u64);
        let approval = AssessmentApprovalSnapshot {
            authorization: AssessmentAuthorization::RunApproval,
            run_id: run.snapshot.run_id.to_string(),
            attempt_id: attempt_id.to_owned(),
            task_kind,
            approval_snapshot_digest,
            frozen_artifact_digest: verified.artifact.artifact_digest.clone(),
            report_digest: sha256_hex(&report_bytes),
            validation_receipts,
            validation_plan,
            criteria,
            allowed_sources: contract.input_scope.allowed_sources.clone(),
            provider: provider.clone(),
            prompt_policy_version: contract.prompt_policy_version.clone().unwrap_or_default(),
            max_input_bytes: contract.input_scope.max_input_bytes,
            deadline_unix_ms,
        };
        let policy = match ApprovedAssessmentPolicy::new(approval) {
            Ok(policy) => policy,
            Err(_) => {
                return SemanticEvaluation::inconclusive("approved_assessment_policy_invalid");
            }
        };
        let scope_evidence = match serde_json::to_vec(&(
            run.snapshot.run_id.as_str(),
            attempt_id,
            references.artifact.sha256.as_str(),
            report_reference.sha256.as_str(),
            &evidence,
        )) {
            Ok(bytes) => sha256_hex(&bytes),
            Err(_) => return SemanticEvaluation::inconclusive("approved_assessment_input_invalid"),
        };
        let assessment_id = format!("assessment-{}", &scope_evidence[..32]);
        let prepared = match prepare_assessment(&policy, &assessment_id, evidence, &report_read) {
            Ok(AssessmentPreparation::Ready(prepared)) => *prepared,
            Ok(AssessmentPreparation::NotRequired) => return SemanticEvaluation::not_required(),
            Err(_) => return SemanticEvaluation::inconclusive("approved_assessment_input_invalid"),
        };
        if prepared.judge_input_scope_digest() != contract.judge_input_scope_digest()
            || serde_json::from_slice::<serde_json::Value>(prepared.input_bytes())
                .ok()
                .and_then(|input| input.get("provider").cloned())
                .and_then(|value| {
                    serde_json::from_value::<super::assessment::ProviderSelection>(value).ok()
                })
                .as_ref()
                != Some(&provider)
        {
            return SemanticEvaluation::inconclusive("approved_assessment_binding_mismatch");
        }

        let metadata = PreparedClaimMetadataV1 {
            schema_version: 1,
            run_id: run.snapshot.run_id.to_string(),
            attempt_id: attempt_id.to_owned(),
            assessment_id: assessment_id.clone(),
            finalization_inputs_evidence_id: references.finalization_inputs.evidence_id.clone(),
            deadline_unix_ms,
            judge_input_digest: prepared.judge_input_digest().to_owned(),
            system_policy_digest: sha256_hex(prepared.system_policy_bytes()),
        };
        let metadata_bytes = match serde_json::to_vec(&metadata) {
            Ok(bytes) => bytes,
            Err(_) => {
                return SemanticEvaluation::inconclusive("assessment_metadata_invalid");
            }
        };
        if self
            .write_unique_evidence(
                &run.snapshot.run_id,
                attempt_id,
                &format!("semantic-prepared-{assessment_id}"),
                JSON_MEDIA_TYPE,
                &metadata_bytes,
            )
            .is_err()
        {
            return SemanticEvaluation::inconclusive("assessment_metadata_persistence_failed");
        }
        let prepared = match config.engine.persist_prepared_for_claim(&prepared) {
            Ok(prepared) => prepared,
            Err(_) => {
                return SemanticEvaluation::inconclusive(
                    "assessment_preparation_persistence_failed",
                );
            }
        };

        let input_reference = match self.write_unique_evidence(
            &run.snapshot.run_id,
            attempt_id,
            "judge-input",
            JSON_MEDIA_TYPE,
            prepared.input_bytes(),
        ) {
            Ok(reference) => reference,
            Err(_) => return SemanticEvaluation::inconclusive("judge_input_persistence_failed"),
        };
        if self
            .write_unique_evidence(
                &run.snapshot.run_id,
                attempt_id,
                "judge-system-policy",
                JSON_MEDIA_TYPE,
                prepared.system_policy_bytes(),
            )
            .is_err()
        {
            return SemanticEvaluation::inconclusive("judge_policy_persistence_failed");
        }
        let target = &verified.target;
        let latest = match self.require_run(&run.snapshot.run_id) {
            Ok(latest)
                if latest.current_attempt_id.as_deref() == Some(attempt_id)
                    && latest.state == RunState::Finalizing =>
            {
                latest
            }
            _ => return SemanticEvaluation::inconclusive("assessment_claim_state_changed"),
        };
        let claim = self
            .inner
            .store
            .bind_assessment_intent_from_finalization_inputs(
                &run.snapshot.run_id,
                latest.revision,
                attempt_id,
                AssessmentIntentRequest {
                    assessment_id: assessment_id.clone(),
                    judge_input_evidence_id: input_reference.evidence_id,
                    report_evidence_id: report_reference.evidence_id.clone(),
                    snapshot_manifest_evidence_id: references.snapshot_manifest.evidence_id.clone(),
                    target_full_commit_id: target.full_commit_id().to_owned(),
                    target_full_tree_id: target.full_tree_id().to_owned(),
                    target_untracked_manifest_digest: target.untracked_manifest_digest().to_owned(),
                    target_snapshot_digest: target.digest().to_owned(),
                    system_prompt_digest: sha256_hex(prepared.system_policy_bytes()),
                    validation_receipts: verified.validation_receipts.clone(),
                },
            );
        let Ok(claim) = claim else {
            return SemanticEvaluation::inconclusive("assessment_claim_failed");
        };
        let dispatch_state = match self.require_run(&run.snapshot.run_id) {
            Ok(current)
                if current.current_attempt_id.as_deref() == Some(attempt_id)
                    && current.state == RunState::Finalizing
                    && !self
                        .has_cancellation_effect(&run.snapshot.run_id, attempt_id)
                        .unwrap_or(true) =>
            {
                current
            }
            _ => return SemanticEvaluation::inconclusive("assessment_claim_state_changed"),
        };
        let profile = match ApprovedJudgeProviderProfile::from_settings(
            &config.judge_settings,
            provider,
            deadline_unix_ms,
        ) {
            Ok(profile) => profile,
            Err(_) => return SemanticEvaluation::inconclusive("approved_provider_unavailable"),
        };
        if let Some(guard) = run_guard.as_deref_mut() {
            guard.release();
        }
        let outcome = assess_claimed_with_jobworkerp(
            &config.jobworkerp_endpoint,
            profile,
            Arc::clone(&config.engine),
            prepared.clone(),
            Arc::clone(&self.inner.store),
            claim,
            SystemAssessmentClock,
        )
        .await;
        if let Some(guard) = run_guard {
            guard.reacquire().await;
        }
        let claim_revision = dispatch_state.revision;
        let current = match self.require_run(&run.snapshot.run_id) {
            Ok(current)
                if current.current_attempt_id.as_deref() == Some(attempt_id)
                    && current.state == RunState::Finalizing
                    && current.revision == claim_revision
                    && !self
                        .has_cancellation_effect(&run.snapshot.run_id, attempt_id)
                        .unwrap_or(true) =>
            {
                current
            }
            _ => {
                return SemanticEvaluation::inconclusive("assessment_claim_state_changed")
                    .with_claim_revision(claim_revision);
            }
        };
        drop(current);
        let Ok(outcome) = outcome else {
            return SemanticEvaluation {
                assessment_id: Some(assessment_id),
                judge_input_digest: Some(prepared.judge_input_digest().to_owned()),
                judge_input_scope_digest: Some(prepared.judge_input_scope_digest().to_owned()),
                outcome: serde_json::json!({"status":"inconclusive", "reason":"provider_result_unconfirmed"}),
                acceptance_conditions_met: false,
                claim_revision: Some(claim_revision),
            };
        };
        let Ok(outcome_json) = serde_json::to_value(&outcome) else {
            return SemanticEvaluation::inconclusive("assessment_result_invalid")
                .with_claim_revision(claim_revision);
        };
        let acceptance_conditions_met = match &outcome {
            AssessmentOutcome::Assessed { assessment, .. } => semantic_completion_conditions_met(
                &contract.criteria,
                report_read.validated_report.as_ref(),
                assessment.overall,
            ),
            AssessmentOutcome::Pending { .. } | AssessmentOutcome::Inconclusive(_) => false,
        };
        SemanticEvaluation {
            assessment_id: Some(assessment_id),
            judge_input_digest: Some(prepared.judge_input_digest().to_owned()),
            judge_input_scope_digest: Some(prepared.judge_input_scope_digest().to_owned()),
            outcome: outcome_json,
            acceptance_conditions_met,
            claim_revision: Some(claim_revision),
        }
    }

    fn assessment_from_host(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        capture: &CapturedOutput,
        observation: &HostFinalizationObservation,
    ) -> Result<RunEvidenceAssessment, CoordinatorError> {
        self.assessment_fields(
            run_id,
            attempt_id,
            AssessmentInput {
                capture,
                report: observation.report.as_deref(),
                persist_report: true,
                freeze: &observation.workspace_freeze,
                host_validation: &observation.validation,
            },
        )
    }

    fn assessment_from_interactive(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        capture: CapturedOutput,
        observation: &InteractiveFinishObservation,
    ) -> Result<RunEvidenceAssessment, CoordinatorError> {
        self.assessment_fields(
            run_id,
            attempt_id,
            AssessmentInput {
                capture: &capture,
                report: observation.report.as_deref(),
                persist_report: false,
                freeze: &observation.workspace_freeze,
                host_validation: &observation.validation,
            },
        )
    }

    fn assessment_fields(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        input: AssessmentInput<'_>,
    ) -> Result<RunEvidenceAssessment, CoordinatorError> {
        if input.persist_report
            && let Some(report) = input.report
        {
            self.persist_bounded_raw_report(run_id, attempt_id, "agent-report-raw", report)?;
        }
        let commit_id = match input.freeze {
            super::WorkspaceFreezeStatus::HostVerified { commit_id } => Some(commit_id.clone()),
            super::WorkspaceFreezeStatus::NotFrozen | super::WorkspaceFreezeStatus::Unknown => None,
        };
        let stream_error = input.capture.stream_error || !input.capture.capture_complete;
        let known_failure = input.capture.exit_code.is_some_and(|code| code != 0);
        let references_present =
            input.capture.stdout_reference.is_some() || input.capture.stderr_reference.is_some();
        let validation_required = match self.inner.store.approval_assessment_contract(run_id)? {
            ApprovalAssessmentContractStatus::LegacyIncomplete => false,
            ApprovalAssessmentContractStatus::Versioned { contract, .. } => contract
                .validation_plan
                .iter()
                .any(|validation| validation.required),
        };
        let validation_state = match input.host_validation.state {
            ValidationState::Failed => ValidationState::Failed,
            // A legacy host `Passed` bit is observational only. A typed,
            // durable receipt is required before the run can record `Passed`.
            ValidationState::NotRun | ValidationState::Passed => ValidationState::NotRun,
        };
        Ok(RunEvidenceAssessment {
            final_exit_code: input.capture.exit_code,
            end_present: input.capture.end_present,
            stream_error,
            cancelled: false,
            known_failure,
            stdout_reference: input.capture.stdout_reference.clone(),
            stdout_bytes: input.capture.stdout.len() as u64,
            stderr_reference: input.capture.stderr_reference.clone(),
            stderr_bytes: input.capture.stderr.len() as u64,
            report_reference: None,
            report_save_confirmed: false,
            evidence_integrity_verified: references_present,
            commit_consistent: commit_id.is_some(),
            commit_id,
            // Legacy host booleans and report claims are not accepted as a
            // semantic receipt or a durable validation receipt.
            acceptance_conditions_met: false,
            validation: ValidationEvidence {
                required: validation_required,
                state: validation_state,
            },
        })
    }

    fn report_expectation(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<ReportExpectation, CoordinatorError> {
        let run = self.require_run(run_id)?;
        let (acceptance_criteria, validation_plan) =
            match self.inner.store.approval_assessment_contract(run_id)? {
                ApprovalAssessmentContractStatus::LegacyIncomplete => {
                    (BTreeMap::new(), BTreeMap::new())
                }
                ApprovalAssessmentContractStatus::Versioned { contract, .. } => (
                    contract
                        .criteria
                        .iter()
                        .map(|criterion| (criterion.criterion_id.clone(), criterion.required))
                        .collect(),
                    contract
                        .validation_plan
                        .iter()
                        .map(|validation| (validation.validation_id.clone(), validation.required))
                        .collect(),
                ),
            };
        let task_kind = match run.snapshot.purpose {
            RunPurpose::Investigation => TaskKind::Investigation,
            RunPurpose::Implementation => TaskKind::Implementation,
        };
        ReportExpectation::new(
            run_id.to_string(),
            attempt_id,
            task_kind,
            acceptance_criteria,
            validation_plan,
        )
        .map_err(|_| CoordinatorError::Conflict)
    }

    fn validate_report_for_run(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        bytes: &[u8],
    ) -> Result<ValidatedReport, ReportRejection> {
        let expectation = self
            .report_expectation(run_id, attempt_id)
            .map_err(|_| ReportRejection::InvalidExpectation)?;
        validate_report(bytes, &expectation)
    }

    fn persist_bounded_raw_report(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        evidence_name: &str,
        report: &[u8],
    ) -> Result<super::EvidenceReference, CoordinatorError> {
        let prefix = &report[..report.len().min(super::report::MAX_REPORT_BYTES)];
        self.write_unique_evidence(run_id, attempt_id, evidence_name, STDOUT_MEDIA_TYPE, prefix)
    }

    /// Promote a report into canonical evidence only after host finalization
    /// has proved process exit, writer quiescence, and a frozen workspace.
    fn finalize_report_evidence(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        report: Option<&[u8]>,
        host_finalization_verified: bool,
    ) -> Result<Option<String>, CoordinatorError> {
        let run = self.require_run(run_id)?;
        let task_kind = match run.snapshot.purpose {
            RunPurpose::Investigation => TaskKind::Investigation,
            RunPurpose::Implementation => TaskKind::Implementation,
        };
        let raw_reference = report
            .map(|bytes| {
                self.persist_bounded_raw_report(run_id, attempt_id, "agent-report-raw", bytes)
            })
            .transpose()?;
        let validated =
            report.and_then(|bytes| self.validate_report_for_run(run_id, attempt_id, bytes).ok());
        let rejection = match (report, validated.as_ref()) {
            (None, _) => Some("missing_report".to_owned()),
            (Some(bytes), None) if !report_has_content(bytes) => Some("empty_report".to_owned()),
            (Some(_), None) => Some(
                self.validate_report_for_run(run_id, attempt_id, report.expect("report exists"))
                    .err()
                    .map_or_else(|| "invalid_report".to_owned(), |error| error.to_string()),
            ),
            (Some(_), Some(_)) => None,
        };
        let canonical_reference = if host_finalization_verified {
            validated
                .as_ref()
                .map(|validated| {
                    let canonical = serde_json::to_vec(&validated.report)
                        .map_err(|_| CoordinatorError::Conflict)?;
                    self.write_unique_evidence(
                        run_id,
                        attempt_id,
                        "agent-report",
                        JSON_MEDIA_TYPE,
                        &canonical,
                    )
                    .map(|reference| reference.evidence_id)
                })
                .transpose()?
        } else {
            None
        };
        let approval_contract = self.inner.store.approval_assessment_contract(run_id)?;
        let semantic_assessment = match approval_contract {
            ApprovalAssessmentContractStatus::LegacyIncomplete => "unavailable_legacy_contract",
            ApprovalAssessmentContractStatus::Versioned { contract, .. }
                if contract.criteria.iter().any(|criterion| {
                    criterion.required
                        && criterion.method
                            == super::model::CriterionEvaluationMethod::SemanticJudge
                }) =>
            {
                "inconclusive_not_dispatched"
            }
            ApprovalAssessmentContractStatus::Versioned { .. } => "not_evaluated",
        };
        let report_assessment = validated.as_ref().map(|validated| &validated.assessment);
        let canonical_sha256 = match canonical_reference.as_deref() {
            Some(evidence_id) => Some(
                self.inner
                    .store
                    .evidence(run_id)?
                    .into_iter()
                    .find(|reference| reference.evidence_id == evidence_id)
                    .ok_or(CoordinatorError::Conflict)?
                    .sha256,
            ),
            None => None,
        };
        let evidence = ReportFinalizationEvidenceV1 {
            schema_version: 1,
            run_id: run_id.to_string(),
            attempt_id: attempt_id.to_owned(),
            purpose: run.snapshot.purpose,
            task_kind,
            disposition: match (report, validated.as_ref(), host_finalization_verified) {
                (None, _, _) => "missing".into(),
                (Some(_), None, _) => "invalid".into(),
                (Some(_), Some(_), false) => "validated_host_unconfirmed".into(),
                (Some(_), Some(_), true) => "canonicalized".into(),
            },
            rejection,
            raw_evidence_id: raw_reference
                .as_ref()
                .map(|reference| reference.evidence_id.clone()),
            raw_bytes_observed: report.map_or(0, <[u8]>::len),
            raw_bytes_saved: raw_reference
                .as_ref()
                .map_or(0, |reference| reference.size_bytes as usize),
            raw_bytes_complete: report
                .is_some_and(|bytes| bytes.len() <= super::report::MAX_REPORT_BYTES),
            canonical_evidence_id: canonical_reference.clone(),
            canonical_sha256,
            agent_status: validated.as_ref().map(|validated| validated.report.status),
            required_acceptance_not_met_or_unknown: report_assessment
                .map_or_else(Vec::new, |assessment| {
                    assessment.required_acceptance_not_met_or_unknown.clone()
                }),
            required_validation_claims_not_passed: report_assessment
                .map_or_else(Vec::new, |assessment| {
                    assessment.required_validation_claims_not_passed.clone()
                }),
            semantic_assessment: semantic_assessment.into(),
        };
        let evidence_bytes =
            serde_json::to_vec(&evidence).map_err(|_| CoordinatorError::Conflict)?;
        self.write_unique_evidence(
            run_id,
            attempt_id,
            "finalization-report-evidence",
            JSON_MEDIA_TYPE,
            &evidence_bytes,
        )?;
        Ok(canonical_reference)
    }

    fn persist_host_finalization_evidence(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        observation: &HostFinalizationObservation,
    ) -> Result<(), CoordinatorError> {
        let evidence = HostFinalizationEvidenceV2 {
            schema_version: 2,
            run_id: run_id.to_string(),
            attempt_id: attempt_id.to_owned(),
            invocation_kind: observation.invocation_kind,
            cli_exit: observation.cli_exit.clone(),
            shell_observation: observation.shell_observation.clone(),
            child_writers: observation.child_writers,
            workspace_freeze: observation.workspace_freeze.clone(),
            validation_observation: observation.validation.clone(),
            acceptance_conditions_met_observation: observation.acceptance_conditions_met,
        };
        let bytes = serde_json::to_vec(&evidence).map_err(|_| CoordinatorError::Conflict)?;
        self.write_unique_evidence(
            run_id,
            attempt_id,
            "host-finalization-evidence",
            JSON_MEDIA_TYPE,
            &bytes,
        )?;
        Ok(())
    }

    fn persist_interactive_host_claim_observation(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        observation: &InteractiveFinishObservation,
        assessment: &RunEvidenceAssessment,
    ) -> Result<(), CoordinatorError> {
        let evidence = InteractiveHostClaimObservationV1 {
            schema_version: 1,
            run_id: run_id.to_string(),
            attempt_id: attempt_id.to_owned(),
            validation_observation: observation.validation.clone(),
            validation_assessment: assessment.validation.clone(),
            acceptance_conditions_met_observation: observation.acceptance_conditions_met,
        };
        let bytes = serde_json::to_vec(&evidence).map_err(|_| CoordinatorError::Conflict)?;
        let name = format!("interactive-host-claim-observation-{}", sha256_hex(&bytes));
        self.write_unique_evidence(run_id, attempt_id, &name, JSON_MEDIA_TYPE, &bytes)?;
        Ok(())
    }

    fn verify_canonical_report_evidence(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        assessment: &RunEvidenceAssessment,
    ) -> Result<bool, CoordinatorError> {
        if !assessment.report_save_confirmed {
            return Ok(false);
        }
        let Some(report_id) = assessment.report_reference.as_deref() else {
            return Ok(false);
        };
        let references = self.inner.store.evidence(run_id)?;
        let Some(reference) = references.iter().find(|reference| {
            reference.evidence_id == report_id
                && reference.attempt_id == attempt_id
                && reference.name == "agent-report"
                && reference.media_type == JSON_MEDIA_TYPE
                && reference.save_confirmed
        }) else {
            return Ok(false);
        };
        if !references.iter().any(|reference| {
            reference.attempt_id == attempt_id && reference.name == "agent-report-raw"
        }) {
            return Ok(false);
        }
        let bytes = self.inner.store.read_evidence(run_id, report_id)?;
        let Ok(validated) = self.validate_report_for_run(run_id, attempt_id, &bytes) else {
            return Ok(false);
        };
        let canonical =
            serde_json::to_vec(&validated.report).map_err(|_| CoordinatorError::Conflict)?;
        Ok(canonical == bytes
            && reference.size_bytes == bytes.len() as u64
            && reference.sha256 == sha256_hex(&bytes))
    }

    fn verify_verified_artifact_evidence(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        assessment: &RunEvidenceAssessment,
    ) -> Result<bool, CoordinatorError> {
        let references = self.inner.store.evidence(run_id)?;
        let Some(reference) = references.iter().find(|reference| {
            reference.attempt_id == attempt_id
                && reference.name == "verified-sealed-artifact"
                && reference.media_type == JSON_MEDIA_TYPE
                && reference.save_confirmed
        }) else {
            return Ok(false);
        };
        let bytes = self
            .inner
            .store
            .read_evidence(run_id, &reference.evidence_id)?;
        let Ok(evidence) = serde_json::from_slice::<VerifiedArtifactEvidenceV1>(&bytes) else {
            return Ok(false);
        };
        let target = ValidationTargetSnapshot::new(
            evidence.full_commit_id.clone(),
            evidence.full_tree_id.clone(),
            evidence.untracked_manifest_digest.clone(),
        );
        Ok(evidence.schema_version == 1
            && evidence.run_id == run_id.as_str()
            && evidence.attempt_id == attempt_id
            && target.is_ok_and(|target| target.digest() == evidence.target_snapshot_digest)
            && evidence.resolved_artifact.run_id == run_id.as_str()
            && evidence.resolved_artifact.attempt_id == attempt_id
            && evidence.resolved_artifact.commit == evidence.full_commit_id
            && evidence.resolved_artifact.tree == evidence.full_tree_id
            && assessment.commit_id.as_deref() == Some(evidence.full_commit_id.as_str())
            && assessment.commit_consistent
            && reference.size_bytes == bytes.len() as u64
            && reference.sha256 == sha256_hex(&bytes))
    }

    /// Keep one bounded raw observation for an unconfirmed interactive finish.
    /// It remains human-readable evidence but never becomes assessment evidence;
    /// canonical report promotion waits for trusted host finalization.
    fn persist_finish_report_observation(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        report: Option<&[u8]>,
    ) -> Result<(), CoordinatorError> {
        let raw_reference = report
            .map(|report| {
                if let Some(existing) =
                    self.inner
                        .store
                        .evidence(run_id)?
                        .into_iter()
                        .find(|reference| {
                            reference.attempt_id == attempt_id
                                && reference.name == "finish-report-raw"
                        })
                {
                    Ok(existing)
                } else {
                    self.persist_bounded_raw_report(run_id, attempt_id, "finish-report-raw", report)
                }
            })
            .transpose()?;
        let observation = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "operation_key": effect_operation_key("finish", attempt_id),
            "authority": "unconfirmed_interactive_finish_observation",
            "report_present": report.is_some(),
            "report_bytes": report.map_or(0, <[u8]>::len),
            "report_sha256": report.map(sha256_hex),
            "raw_evidence_id": raw_reference.as_ref().map(|reference| &reference.evidence_id),
            "raw_bytes_saved": raw_reference.as_ref().map(|reference| reference.size_bytes),
            "raw_bytes_complete": report.is_some_and(|bytes| {
                bytes.len() <= super::report::MAX_REPORT_BYTES
            }),
        }))
        .map_err(|_| CoordinatorError::Conflict)?;
        match self.write_unique_evidence(
            run_id,
            attempt_id,
            "finish-report-observation",
            JSON_MEDIA_TYPE,
            &observation,
        ) {
            Ok(_) => Ok(()),
            Err(CoordinatorError::Conflict)
                if self.inner.store.evidence(run_id)?.iter().any(|reference| {
                    reference.attempt_id == attempt_id
                        && reference.name == "finish-report-observation"
                }) =>
            {
                // First observation wins. This is intentionally bounded to a
                // single metadata record per attempt and is not canonical.
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn run_workspace_validations(
        &self,
        receipt: &StartReceipt,
        contract: &ApprovalAssessmentContract,
    ) -> Result<ValidationEvidence, CoordinatorError> {
        let run_id = &receipt.identity.run_id;
        let run_id = RunId::new(run_id).map_err(|_| CoordinatorError::Conflict)?;
        let attempt_id = &receipt.identity.attempt_id;
        let execution = self
            .inner
            .execution
            .as_ref()
            .ok_or(CoordinatorError::Conflict)?;
        let mut observations = Vec::new();
        for definition in contract
            .validation_plan
            .iter()
            .filter(|definition| definition.command.is_some())
        {
            let lock = RunLockLease::acquire(self.run_lock(&run_id)).await;
            let current = self.require_run(&run_id)?;
            if current.state != RunState::Verifying
                || current.current_attempt_id.as_deref() != Some(attempt_id)
                || self.has_cancellation_effect(&run_id, attempt_id)?
            {
                return Err(CoordinatorError::Conflict);
            }
            let intent = serde_json::json!({"schema_version":1,"run_id":run_id.as_str(),"attempt_id":attempt_id,"definition":definition});
            if observations.is_empty() {
                self.write_unique_evidence(
                    &run_id,
                    attempt_id,
                    "workspace-validation-required",
                    JSON_MEDIA_TYPE,
                    &serde_json::to_vec(&intent).map_err(|_| CoordinatorError::Conflict)?,
                )?;
            }
            let intent_name = format!("workspace-validation-intent-{}", definition.validation_id);
            self.write_unique_evidence(
                &run_id,
                attempt_id,
                &intent_name,
                JSON_MEDIA_TYPE,
                &serde_json::to_vec(&intent).map_err(|_| CoordinatorError::Conflict)?,
            )?;
            let started = execution
                .begin_workspace_validation(&receipt.identity, definition)
                .await
                .map_err(|_| CoordinatorError::Conflict)?;
            let start_name = format!("workspace-validation-start-{}", definition.validation_id);
            self.write_unique_evidence(
                &run_id,
                attempt_id,
                &start_name,
                JSON_MEDIA_TYPE,
                &serde_json::to_vec(&started).map_err(|_| CoordinatorError::Conflict)?,
            )?;
            drop(lock);
            let observation = execution
                .validate_workspace(&receipt.identity, definition)
                .await
                .map_err(|_| CoordinatorError::Conflict)?;
            super::workspace_validation::summarize(
                std::slice::from_ref(definition),
                std::slice::from_ref(&observation),
            )
            .map_err(|_| CoordinatorError::Conflict)?;
            if observation.workflow_result.parent_job_id != started.parent_job_id {
                return Err(CoordinatorError::Conflict);
            }
            let result_name = format!("workspace-validation-result-{}", definition.validation_id);
            self.write_unique_evidence(
                &run_id,
                attempt_id,
                &result_name,
                JSON_MEDIA_TYPE,
                &serde_json::to_vec(&observation).map_err(|_| CoordinatorError::Conflict)?,
            )?;
            let failed = observation.workflow_result.exit_code != 0;
            observations.push(observation);
            if failed {
                break;
            }
        }
        let validation =
            super::workspace_validation::summarize(&contract.validation_plan, &observations)
                .map_err(|_| CoordinatorError::Conflict)?;
        let result = serde_json::json!({"schema_version":1,"run_id":run_id.as_str(),"attempt_id":attempt_id,"status":"completed","validation":validation,"records":observations});
        self.write_unique_evidence(
            &run_id,
            attempt_id,
            "workspace-validation-result",
            JSON_MEDIA_TYPE,
            &serde_json::to_vec(&result).map_err(|_| CoordinatorError::Conflict)?,
        )?;
        Ok(validation)
    }

    fn local_commit_confirmation_pending(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<bool, CoordinatorError> {
        let evidence = self.inner.store.evidence(run_id)?;
        for (required, intent, result_name) in [
            (
                "workspace-validation-required",
                "workspace-validation-required",
                "workspace-validation-result",
            ),
            (
                "ordinary-semantic-required",
                "ordinary-semantic-intent",
                "ordinary-semantic-result",
            ),
            (
                "local-commit-required",
                "local-commit-intent",
                "local-commit-result",
            ),
            (
                "issue-result-required",
                "issue-result-intent",
                "issue-result-ack",
            ),
        ] {
            if !evidence.iter().any(|reference| {
                reference.attempt_id == attempt_id
                    && (reference.name == required || reference.name == intent)
            }) {
                continue;
            }
            let Some(reference) = evidence.iter().find(|reference| {
                reference.attempt_id == attempt_id
                    && reference.name == result_name
                    && reference.save_confirmed
            }) else {
                return Ok(true);
            };
            let bytes = self
                .inner
                .store
                .read_evidence(run_id, &reference.evidence_id)?;
            let Ok(result) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                return Ok(true);
            };
            if result["schema_version"] != 1
                || result["run_id"] != run_id.as_str()
                || result["attempt_id"] != attempt_id
                || !matches!(result["status"].as_str(), Some("completed" | "skipped"))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn complete_verified_run(
        &self,
        run: AgentRunRecord,
    ) -> Result<AgentRunRecord, CoordinatorError> {
        if run.current_attempt_id.as_deref().is_some_and(|attempt_id| {
            self.has_cancellation_effect(&run.snapshot.run_id, attempt_id)
                .unwrap_or(true)
        }) {
            return self.transition_if_allowed(&run, RunState::Unknown);
        }
        if let Some(attempt_id) = run.current_attempt_id.as_deref()
            && self.local_commit_confirmation_pending(&run.snapshot.run_id, attempt_id)?
        {
            return Ok(run);
        }
        let assessment = run
            .execution_evidence
            .as_ref()
            .ok_or(CoordinatorError::Conflict)?;
        if !assessment_is_successful(assessment) {
            return Ok(self.inner.store.transition(
                &run.snapshot.run_id,
                run.revision,
                RunState::Failed,
            )?);
        }
        if self.confirmed_publication(&run)?.is_some() {
            return self.transition_if_allowed(&run, RunState::AwaitingReview);
        }
        let report_only_investigation = run.snapshot.purpose == RunPurpose::Investigation
            && assessment
                .commit_id
                .as_deref()
                .is_none_or(|commit| commit == run.snapshot.base_commit);
        if report_only_investigation
            || run.snapshot.publication_plan == PublicationPlan::NoPublication
        {
            // S6 human completion and issue-state mutation remain separate.
            return Ok(self.inner.store.transition(
                &run.snapshot.run_id,
                run.revision,
                RunState::AwaitingReview,
            )?);
        }
        if matches!(
            self.inner
                .store
                .approval_assessment_contract(&run.snapshot.run_id)?,
            ApprovalAssessmentContractStatus::LegacyIncomplete
        ) || !self.verify_canonical_report_evidence(
            &run.snapshot.run_id,
            run.current_attempt_id
                .as_deref()
                .ok_or(CoordinatorError::Conflict)?,
            assessment,
        )? {
            return Ok(self.inner.store.transition(
                &run.snapshot.run_id,
                run.revision,
                RunState::AwaitingReview,
            )?);
        }
        let decision: CompletionGateDecision = evaluate_completion_gates(
            run.snapshot.purpose,
            run.snapshot.auto_done,
            run.snapshot.publication_plan.clone(),
            &run.snapshot.allowed_operations,
            assessment,
        );
        if decision.publication != GateDecision::Allowed {
            return Ok(self.inner.store.transition(
                &run.snapshot.run_id,
                run.revision,
                RunState::AwaitingReview,
            )?);
        }
        Ok(run)
    }

    async fn await_capture(&self, run_id: &RunId, attempt_id: &str) -> CapturedOutput {
        let key = attempt_key(run_id.as_str(), attempt_id);
        if let Some(capture) = self.capture_from_memory(&key) {
            return capture;
        }
        if let Some(capture) = self.read_durable_capture(run_id, attempt_id).ok().flatten() {
            return capture;
        }
        if !self.capture_identity_registered(run_id, attempt_id) {
            return missing_capture();
        }

        let deadline = tokio::time::Instant::now() + self.inner.config.finish_timeout;
        loop {
            if let Some(capture) = self.capture_from_memory(&key) {
                return capture;
            }
            if tokio::time::Instant::now() >= deadline {
                return self
                    .read_durable_capture(run_id, attempt_id)
                    .ok()
                    .flatten()
                    .unwrap_or_else(missing_capture);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn capture_from_memory(&self, key: &str) -> Option<CapturedOutput> {
        self.inner
            .captures
            .lock()
            .expect("capture map poisoned")
            .get(key)
            .cloned()
    }

    fn capture_identity_registered(&self, run_id: &RunId, attempt_id: &str) -> bool {
        self.inner
            .identities
            .lock()
            .expect("identity map poisoned")
            .contains_key(&attempt_key(run_id.as_str(), attempt_id))
    }

    fn read_durable_capture(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<Option<CapturedOutput>, CoordinatorError> {
        let evidence = self.inner.store.evidence(run_id)?;
        let Some(status_reference) = evidence.iter().find(|reference| {
            reference.attempt_id == attempt_id && reference.name == "capture-status"
        }) else {
            return Ok(None);
        };
        if status_reference.media_type != JSON_MEDIA_TYPE || !status_reference.save_confirmed {
            return Ok(None);
        }
        let status_bytes = self
            .inner
            .store
            .read_evidence(run_id, &status_reference.evidence_id)?;
        let status: CaptureStatusEvidence =
            serde_json::from_slice(&status_bytes).map_err(|_| CoordinatorError::Conflict)?;
        if status.version != 1
            || status.attempt_id != attempt_id
            || status.stdout_bytes > self.inner.config.max_capture_bytes_per_stream
            || status.stderr_bytes > self.inner.config.max_capture_bytes_per_stream
        {
            return Ok(None);
        }
        let stdout = self.read_capture_channel(
            run_id,
            attempt_id,
            &evidence,
            "stdout",
            status.stdout_reference.as_deref(),
            status.stdout_bytes,
        )?;
        let stderr = self.read_capture_channel(
            run_id,
            attempt_id,
            &evidence,
            "stderr",
            status.stderr_reference.as_deref(),
            status.stderr_bytes,
        )?;
        let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
            return Ok(None);
        };
        Ok(Some(CapturedOutput {
            stdout,
            stderr,
            exit_code: status.exit_code,
            end_present: status.end_present,
            stream_error: status.stream_error,
            capture_complete: status.capture_complete,
            stdout_reference: status.stdout_reference,
            stderr_reference: status.stderr_reference,
        }))
    }

    fn read_capture_channel(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        evidence: &[super::EvidenceReference],
        name: &str,
        evidence_id: Option<&str>,
        expected_size: usize,
    ) -> Result<Option<Vec<u8>>, CoordinatorError> {
        let Some(evidence_id) = evidence_id else {
            return Ok((expected_size == 0).then(Vec::new));
        };
        let Some(reference) = evidence.iter().find(|reference| {
            reference.evidence_id == evidence_id
                && reference.attempt_id == attempt_id
                && reference.name == name
                && reference.media_type == STDOUT_MEDIA_TYPE
                && reference.size_bytes == expected_size as u64
        }) else {
            return Ok(None);
        };
        let bytes = self
            .inner
            .store
            .read_evidence(run_id, &reference.evidence_id)?;
        if bytes.len() != expected_size {
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    fn identity_for(&self, run_id: &RunId) -> Result<ExecutionIdentity, CoordinatorError> {
        let run = self.require_run(run_id)?;
        let attempt_id = run
            .current_attempt_id
            .as_deref()
            .ok_or(CoordinatorError::Conflict)?;
        let key = attempt_key(run_id.as_str(), attempt_id);
        if let Some(identity) = self
            .inner
            .identities
            .lock()
            .expect("identity map poisoned")
            .get(&key)
            .cloned()
        {
            return Ok(identity);
        }
        for evidence in self.inner.store.evidence(run_id)? {
            if evidence.attempt_id == attempt_id && evidence.name == "dispatch-receipt" {
                let bytes = self
                    .inner
                    .store
                    .read_evidence(run_id, &evidence.evidence_id)?;
                let receipt: StartReceipt =
                    serde_json::from_slice(&bytes).map_err(|_| CoordinatorError::Conflict)?;
                if receipt.identity.run_id != run_id.as_str()
                    || receipt.identity.attempt_id != attempt_id
                {
                    return Err(CoordinatorError::Conflict);
                }
                return Ok(receipt.identity);
            }
        }
        Err(CoordinatorError::Conflict)
    }

    async fn reconcile_publication(
        &self,
        run_id: &RunId,
        run: &AgentRunRecord,
    ) -> Result<RunReconciliation, CoordinatorError> {
        let attempt_id = run
            .current_attempt_id
            .as_deref()
            .ok_or(CoordinatorError::Conflict)?;
        let Some(request) = self.read_publication_intent(run_id, attempt_id)? else {
            let unknown = self.unconfirmed_publication_run(run)?;
            return Ok(RunReconciliation::Unknown(unknown));
        };
        let Some(assessment) = run.execution_evidence.as_ref() else {
            let unknown = self.unconfirmed_publication_run(run)?;
            return Ok(RunReconciliation::Unknown(unknown));
        };
        let operation_key = operation_key("publish", attempt_id);
        if request.operation_key != operation_key
            || !publication_request_matches_run(run, attempt_id, assessment, &request)
        {
            let unknown = self.unconfirmed_publication_run(run)?;
            return Ok(RunReconciliation::Unknown(unknown));
        }
        let Some(operation) = self.inner.store.operation(run_id, &operation_key)? else {
            // A request evidence record without a durable publication claim
            // was never authorized to dispatch. Do not turn it into a fresh
            // effect during recovery.
            let unknown = self.unconfirmed_publication_run(run)?;
            return Ok(RunReconciliation::Unknown(unknown));
        };
        if operation.attempt_id != attempt_id
            || operation.idempotency_key != operation_key
            || operation.request != publication_operation(&request)
        {
            let unknown = self.unconfirmed_publication_run(run)?;
            return Ok(RunReconciliation::Unknown(unknown));
        }
        if self.has_cancellation_effect(run_id, attempt_id)? {
            let unknown =
                self.transition_if_allowed(&self.require_run(run_id)?, RunState::Unknown)?;
            return Ok(RunReconciliation::Unknown(unknown));
        }
        if operation.status == DurableOperationStatus::Completed {
            let recovered = self.recover_completed_publication(run, &request, &operation)?;
            if recovered.state == RunState::AwaitingReview {
                return Ok(RunReconciliation::Accepted(recovered));
            }
            return Ok(RunReconciliation::Unknown(recovered));
        }
        let Some(adapter) = self.inner.publication.as_ref() else {
            return Err(CoordinatorError::AdapterUnavailable);
        };
        let outcome = adapter
            .reconcile(&request)
            .await
            .unwrap_or(PublicationOutcome::Unknown);
        if valid_publication_receipt(&request, &outcome) {
            let bytes = serde_json::to_vec(&outcome).map_err(|_| CoordinatorError::Conflict)?;
            let evidence = self.write_unique_evidence(
                run_id,
                attempt_id,
                "publication-receipt",
                JSON_MEDIA_TYPE,
                &bytes,
            )?;
            let current = self.require_run(run_id)?;
            let completed = self.inner.store.confirm_publication_lookup(
                &current,
                &operation,
                DurableOperationResult {
                    outcome: DurableOperationOutcome::Succeeded,
                    result_digest: Some(sha256_hex(&bytes)),
                    evidence_ids: vec![evidence.evidence_id],
                },
            )?;
            let recovered = self.recover_completed_publication(&current, &request, &completed)?;
            return Ok(if recovered.state == RunState::AwaitingReview {
                RunReconciliation::Accepted(recovered)
            } else {
                RunReconciliation::Unknown(recovered)
            });
        }
        let current = self.require_run(run_id)?;
        let unknown = self.unconfirmed_publication_run(&current)?;
        Ok(RunReconciliation::Unknown(unknown))
    }

    fn unconfirmed_publication_run(
        &self,
        run: &AgentRunRecord,
    ) -> Result<AgentRunRecord, CoordinatorError> {
        if run.state == RunState::Publishing
            && let Some(attempt_id) = run.current_attempt_id.as_deref()
            && self
                .inner
                .store
                .operation(&run.snapshot.run_id, &operation_key("publish", attempt_id))?
                .is_some_and(|operation| operation.status == DurableOperationStatus::Intent)
        {
            // Intent means an adapter call may still be live in this or
            // another process. Lookup-only Unknown must not advance the
            // revision bound into the original sealed permit.
            return Ok(run.clone());
        }
        self.transition_if_allowed(run, RunState::Unknown)
    }

    fn read_publication_intent(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<Option<PublicationRequest>, CoordinatorError> {
        let Some(reference) = self
            .inner
            .store
            .evidence(run_id)?
            .into_iter()
            .find(|reference| {
                reference.attempt_id == attempt_id && reference.name == "publication-intent"
            })
        else {
            return Ok(None);
        };
        let bytes = self
            .inner
            .store
            .read_evidence(run_id, &reference.evidence_id)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| CoordinatorError::Conflict)
    }

    fn complete_new_publication(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        request: &PublicationRequest,
        permit: super::model::PublicationEffectPermit,
        outcome: PublicationOutcome,
    ) -> Result<AgentRunRecord, CoordinatorError> {
        let result = match &outcome {
            PublicationOutcome::Rejected => DurableOperationResult {
                outcome: DurableOperationOutcome::Failed,
                result_digest: None,
                evidence_ids: Vec::new(),
            },
            accepted if valid_publication_receipt(request, accepted) => {
                let bytes = serde_json::to_vec(&outcome).map_err(|_| CoordinatorError::Conflict)?;
                let evidence = self.write_unique_evidence(
                    run_id,
                    attempt_id,
                    "publication-receipt",
                    JSON_MEDIA_TYPE,
                    &bytes,
                )?;
                DurableOperationResult {
                    outcome: DurableOperationOutcome::Succeeded,
                    result_digest: Some(sha256_hex(&bytes)),
                    evidence_ids: vec![evidence.evidence_id],
                }
            }
            _ => {
                let bytes = serde_json::to_vec(&outcome).map_err(|_| CoordinatorError::Conflict)?;
                let evidence = self.write_unique_evidence(
                    run_id,
                    attempt_id,
                    "publication-observation",
                    JSON_MEDIA_TYPE,
                    &bytes,
                )?;
                DurableOperationResult {
                    outcome: DurableOperationOutcome::ResultUnconfirmed,
                    result_digest: Some(sha256_hex(&bytes)),
                    evidence_ids: vec![evidence.evidence_id],
                }
            }
        };
        let completed = self
            .inner
            .store
            .complete_publication_effect(permit, result)?;
        let current = self.require_run(run_id)?;
        match completed.result.map(|result| result.outcome) {
            Some(DurableOperationOutcome::Succeeded)
                if completed.status == DurableOperationStatus::Completed
                    && !self.has_cancellation_effect(run_id, attempt_id)? =>
            {
                if self.local_commit_confirmation_pending(run_id, attempt_id)? {
                    Ok(current)
                } else {
                    self.transition_if_allowed(&current, RunState::AwaitingReview)
                }
            }
            Some(DurableOperationOutcome::Failed)
                if !self.has_cancellation_effect(run_id, attempt_id)? =>
            {
                self.transition_if_allowed(&current, RunState::Failed)
            }
            _ => self.transition_if_allowed(&current, RunState::Unknown),
        }
    }

    fn confirmed_publication(
        &self,
        run: &AgentRunRecord,
    ) -> Result<Option<PublicationOutcome>, CoordinatorError> {
        let Some(attempt_id) = run.current_attempt_id.as_deref() else {
            return Ok(None);
        };
        let Some(request) = self.read_publication_intent(&run.snapshot.run_id, attempt_id)? else {
            return Ok(None);
        };
        let Some(assessment) = run.execution_evidence.as_ref() else {
            return Ok(None);
        };
        let expected_key = operation_key("publish", attempt_id);
        if request.plan == PublicationPlan::NoPublication
            || request.operation_key != expected_key
            || !publication_request_matches_run(run, attempt_id, assessment, &request)
        {
            return Ok(None);
        }
        let Some(operation) = self
            .inner
            .store
            .operation(&run.snapshot.run_id, &request.operation_key)?
        else {
            return Ok(None);
        };
        if operation.attempt_id != attempt_id
            || operation.idempotency_key != expected_key
            || operation.request != publication_operation(&request)
            || operation.status != DurableOperationStatus::Completed
            || !operation
                .result
                .as_ref()
                .is_some_and(|result| result.outcome == DurableOperationOutcome::Succeeded)
        {
            return Ok(None);
        }
        self.completed_publication_receipt(run, &request, &operation)
    }

    fn recover_completed_publication(
        &self,
        run: &AgentRunRecord,
        request: &PublicationRequest,
        operation: &super::DurableOperationRecord,
    ) -> Result<AgentRunRecord, CoordinatorError> {
        let result = operation
            .result
            .as_ref()
            .ok_or(CoordinatorError::Conflict)?;
        match &result.outcome {
            DurableOperationOutcome::Succeeded => {
                let receipt = self.completed_publication_receipt(run, request, operation)?;
                if receipt.is_none()
                    || self.has_cancellation_effect(&run.snapshot.run_id, &request.attempt_id)?
                {
                    return self.transition_if_allowed(
                        &self.require_run(&run.snapshot.run_id)?,
                        RunState::Unknown,
                    );
                }
                let current = self.require_run(&run.snapshot.run_id)?;
                if self
                    .local_commit_confirmation_pending(&run.snapshot.run_id, &request.attempt_id)?
                {
                    Ok(current)
                } else {
                    self.transition_if_allowed(&current, RunState::AwaitingReview)
                }
            }
            DurableOperationOutcome::Failed => {
                let current = self.require_run(&run.snapshot.run_id)?;
                if self.has_cancellation_effect(&run.snapshot.run_id, &request.attempt_id)? {
                    self.transition_if_allowed(&current, RunState::Unknown)
                } else {
                    self.transition_if_allowed(&current, RunState::Failed)
                }
            }
            DurableOperationOutcome::ResultUnconfirmed => self
                .transition_if_allowed(&self.require_run(&run.snapshot.run_id)?, RunState::Unknown),
        }
    }

    fn completed_publication_receipt(
        &self,
        run: &AgentRunRecord,
        request: &PublicationRequest,
        operation: &super::DurableOperationRecord,
    ) -> Result<Option<PublicationOutcome>, CoordinatorError> {
        let Some(result) = operation.result.as_ref() else {
            return Ok(None);
        };
        if result.evidence_ids.len() != 1 {
            return Ok(None);
        }
        let evidence_id = &result.evidence_ids[0];
        let Some(reference) = self
            .inner
            .store
            .evidence(&run.snapshot.run_id)?
            .into_iter()
            .find(|reference| reference.evidence_id == *evidence_id)
        else {
            return Ok(None);
        };
        if reference.attempt_id != request.attempt_id
            || reference.name != "publication-receipt"
            || !self
                .inner
                .store
                .verify_evidence(&run.snapshot.run_id, evidence_id)?
        {
            return Ok(None);
        }
        let bytes = self
            .inner
            .store
            .read_evidence(&run.snapshot.run_id, evidence_id)?;
        let digest = sha256_hex(&bytes);
        if result.result_digest.as_deref() != Some(digest.as_str()) {
            return Ok(None);
        }
        let receipt: PublicationOutcome =
            serde_json::from_slice(&bytes).map_err(|_| CoordinatorError::Conflict)?;
        Ok(valid_publication_receipt(request, &receipt).then_some(receipt))
    }

    fn complete_unconfirmed(
        &self,
        run_id: &RunId,
        key: &str,
        _reason: &str,
    ) -> Result<(), CoordinatorError> {
        if self
            .inner
            .store
            .operation(run_id, key)?
            .is_some_and(|operation| operation.status != DurableOperationStatus::Completed)
        {
            self.inner.store.complete_operation(
                run_id,
                key,
                DurableOperationResult {
                    outcome: DurableOperationOutcome::ResultUnconfirmed,
                    result_digest: None,
                    evidence_ids: Vec::new(),
                },
            )?;
        }
        Ok(())
    }

    fn complete_operation_unconfirmed(
        &self,
        run_id: &RunId,
        key: &str,
    ) -> Result<(), CoordinatorError> {
        self.complete_unconfirmed(run_id, key, "operation result is unknown")
    }

    fn complete_operation_failure(
        &self,
        run_id: &RunId,
        key: &str,
    ) -> Result<(), CoordinatorError> {
        let operation = self
            .inner
            .store
            .operation(run_id, key)?
            .ok_or(CoordinatorError::Conflict)?;
        if operation.status != DurableOperationStatus::Completed {
            self.inner.store.complete_operation(
                run_id,
                key,
                DurableOperationResult {
                    outcome: DurableOperationOutcome::Failed,
                    result_digest: None,
                    evidence_ids: Vec::new(),
                },
            )?;
        }
        Ok(())
    }

    fn transition_if_allowed(
        &self,
        run: &AgentRunRecord,
        target: RunState,
    ) -> Result<AgentRunRecord, CoordinatorError> {
        if run.state == target {
            return Ok(run.clone());
        }
        if !can_transition(run.state, target) {
            return Ok(run.clone());
        }
        Ok(self
            .inner
            .store
            .transition(&run.snapshot.run_id, run.revision, target)?)
    }

    fn transition_attempt_if_current(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        identity: &ExecutionIdentity,
        target: RunState,
    ) -> Result<Option<AgentRunRecord>, CoordinatorError> {
        let current = self.require_run(run_id)?;
        if identity.run_id != run_id.as_str()
            || identity.attempt_id != attempt_id
            || current.current_attempt_id.as_deref() != Some(attempt_id)
        {
            return Ok(None);
        }
        if current.state == target || !can_transition(current.state, target) {
            return Ok(Some(current));
        }
        match self
            .inner
            .store
            .transition(run_id, current.revision, target)
        {
            Ok(updated) => Ok(Some(updated)),
            // A retry increments the revision. Do not re-read and retry this
            // transition against the newer attempt after losing the CAS.
            Err(CodingAgentError::Conflict(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn transition_attempt_or_current(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        identity: &ExecutionIdentity,
        target: RunState,
    ) -> Result<AgentRunRecord, CoordinatorError> {
        match self.transition_attempt_if_current(run_id, attempt_id, identity, target)? {
            Some(run) => Ok(run),
            None => self.require_run(run_id),
        }
    }

    fn require_run(&self, run_id: &RunId) -> Result<AgentRunRecord, CoordinatorError> {
        self.inner
            .store
            .get_run(run_id)?
            .ok_or(CoordinatorError::Conflict)
    }

    fn run_lock(&self, run_id: &RunId) -> Arc<AsyncMutex<()>> {
        let mut locks = self.inner.run_locks.lock().expect("run lock map poisoned");
        locks
            .entry(run_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    fn identity_is_registered(&self, identity: &ExecutionIdentity) -> bool {
        self.inner
            .identities
            .lock()
            .expect("identity map poisoned")
            .get(&attempt_key(&identity.run_id, &identity.attempt_id))
            == Some(identity)
    }

    fn capture_identity_is_current(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<bool, CoordinatorError> {
        if !self.identity_is_registered(identity) {
            return Ok(false);
        }
        let run_id = RunId::new(identity.run_id.clone()).map_err(|_| CoordinatorError::Conflict)?;
        let run = self.require_run(&run_id)?;
        Ok(run.current_attempt_id.as_deref() == Some(identity.attempt_id.as_str()))
    }

    fn capture_retirement_state(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<CaptureRetirement, CoordinatorError> {
        let run_id = RunId::new(identity.run_id.clone()).map_err(|_| CoordinatorError::Conflict)?;
        let run = self.require_run(&run_id)?;
        if run.current_attempt_id.as_deref() != Some(identity.attempt_id.as_str()) {
            return Ok(CaptureRetirement::Superseded);
        }
        if self
            .read_cancel_receipt(&run_id, &identity.attempt_id)?
            .is_some()
        {
            return Ok(CaptureRetirement::Cancelled);
        }
        if run
            .interactive_finalization
            .as_ref()
            .is_some_and(trusted_finalization_evidence_quiesced)
        {
            return Ok(CaptureRetirement::HostQuiesced);
        }
        Ok(CaptureRetirement::Continue)
    }

    async fn retire_local_capture_if_proven(&self, run: &AgentRunRecord) {
        let Some(attempt_id) = run.current_attempt_id.as_deref() else {
            return;
        };
        let key = attempt_key(run.snapshot.run_id.as_str(), attempt_id);
        let identity = self
            .inner
            .identities
            .lock()
            .expect("identity map poisoned")
            .get(&key)
            .cloned();
        let Some(identity) = identity else {
            return;
        };
        let Ok(retirement) = self.capture_retirement_state(&identity) else {
            return;
        };
        match retirement {
            CaptureRetirement::Continue => {}
            CaptureRetirement::Superseded | CaptureRetirement::Cancelled => {
                self.retire_capture_owner(&identity, true).await;
            }
            CaptureRetirement::HostQuiesced => {
                self.retire_capture_owner(&identity, false).await;
            }
        }
    }

    fn apply_capture_retirement(
        &self,
        identity: &ExecutionIdentity,
        retirement: CaptureRetirement,
    ) -> bool {
        match retirement {
            CaptureRetirement::Continue => false,
            CaptureRetirement::Superseded | CaptureRetirement::Cancelled => {
                self.finish_capture_task(identity, true);
                true
            }
            CaptureRetirement::HostQuiesced => {
                self.finish_capture_task(identity, false);
                true
            }
        }
    }

    fn finish_capture_task(&self, identity: &ExecutionIdentity, remove_capture: bool) {
        let key = attempt_key(&identity.run_id, &identity.attempt_id);
        if let Some(current_task_id) = tokio::task::try_id() {
            let mut tasks = self.inner.tasks.lock().expect("run task map poisoned");
            if tasks
                .get(&key)
                .is_some_and(|task| task.id() == current_task_id)
            {
                tasks.remove(&key);
            }
        }
        self.inner
            .terminal_owners
            .remove_owner_for_identity(identity);
        let mut identities = self.inner.identities.lock().expect("identity map poisoned");
        if identities
            .get(&key)
            .is_some_and(|registered| registered == identity)
        {
            identities.remove(&key);
        }
        drop(identities);
        if remove_capture {
            self.inner
                .captures
                .lock()
                .expect("capture map poisoned")
                .remove(&key);
        }
    }

    /// Retire an attempt's capture stream only after trusted host evidence says
    /// its process is quiescent. The task handle is removed under the registry
    /// lock, then aborted and awaited with no synchronous locks held so its
    /// semaphore permit is released before another run can start.
    async fn retire_capture_owner(&self, identity: &ExecutionIdentity, remove_capture: bool) {
        let key = attempt_key(&identity.run_id, &identity.attempt_id);
        let current_task_id = tokio::task::try_id();
        let task = {
            let mut tasks = self.inner.tasks.lock().expect("run task map poisoned");
            match tasks.remove(&key) {
                Some(task) if current_task_id == Some(task.id()) => {
                    // The capture task must never await or abort itself. Keep
                    // its owner registered; normal return releases the permit.
                    tasks.insert(key.clone(), task);
                    return;
                }
                task => task,
            }
        };
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        self.finish_capture_task(identity, remove_capture);
    }
}

fn valid_workspace_receipt(receipt: &WorkspaceReceipt, request: &WorkspaceRequest) -> bool {
    receipt.run_id == request.snapshot.run_id.as_str()
        && receipt.attempt_id == request.attempt_id
        && !receipt.workspace_token.is_empty()
        && receipt.repository_host == request.snapshot.repository.identity.host
        && receipt.repository_owner == request.snapshot.repository.identity.organization
        && receipt.repository_name == request.snapshot.repository.identity.repository
        && receipt.base_commit == request.snapshot.base_commit
        && receipt.branch == request.snapshot.workspace_branch
}

fn valid_start_receipt(
    receipt: &StartReceipt,
    intent: &ImmutableExecutionIntent,
    expected_digest: &str,
) -> bool {
    let one_shot = intent.execution_mode == ExecutionMode::OneShot;
    let identity_matches =
        |expected: &str, observed: &str| (one_shot && expected.is_empty()) || expected == observed;
    receipt.identity.attempt_id == intent.attempt_id
        && receipt.identity.run_id == intent.run_id
        && identity_matches(
            &intent.workflow_execution_id,
            &receipt.identity.workflow_execution_id,
        )
        && identity_matches(&intent.parent_job_id, &receipt.identity.parent_job_id)
        && identity_matches(
            &intent.child_sandbox_job_id,
            &receipt.identity.child_sandbox_job_id,
        )
        && receipt.identity.worker_id == intent.worker_id
        && (one_shot || !receipt.identity.sandbox_job_id.is_empty())
        && receipt.execution_intent_sha256 == expected_digest
        && receipt.method_name == intent.method_name
        && receipt.image_digest == intent.image_digest
}

fn valid_cancellation_observation(observation: &CancellationObservation) -> bool {
    observation.child_writers == ChildWritersStatus::Stopped
        && match &observation.process_exit {
            ProcessExitObservation::ExitedNormally { .. } | ProcessExitObservation::Killed => true,
            ProcessExitObservation::Signaled { signal } => *signal > 0,
            ProcessExitObservation::Disconnected | ProcessExitObservation::Unknown => false,
        }
}

fn trusted_interactive_host_quiesced(observation: &InteractiveFinishObservation) -> bool {
    trusted_host_observations_quiesced(
        observation.invocation_kind,
        &observation.cli_exit,
        &observation.shell_observation,
        observation.child_writers,
        &observation.workspace_freeze,
    )
}

fn trusted_finalization_evidence_quiesced(evidence: &InteractiveFinalizationEvidence) -> bool {
    trusted_host_observations_quiesced(
        evidence.invocation_kind(),
        evidence.cli_exit(),
        evidence.shell_observation(),
        evidence.child_writers(),
        evidence.workspace_freeze(),
    )
}

fn trusted_host_observations_quiesced(
    invocation_kind: ExecutionInvocationKind,
    cli_exit: &ProcessExitObservation,
    shell_observation: &ShellProcessObservation,
    child_writers: ChildWritersStatus,
    workspace_freeze: &super::WorkspaceFreezeStatus,
) -> bool {
    let process_exited = |exit: &ProcessExitObservation| match exit {
        ProcessExitObservation::ExitedNormally { .. } | ProcessExitObservation::Killed => true,
        ProcessExitObservation::Signaled { signal } => *signal > 0,
        ProcessExitObservation::Disconnected | ProcessExitObservation::Unknown => false,
    };
    child_writers == ChildWritersStatus::Stopped
        && process_exited(cli_exit)
        && shell_observation.is_quiesced_for(invocation_kind)
        && matches!(
            workspace_freeze,
            super::WorkspaceFreezeStatus::HostVerified { commit_id }
                if matches!(commit_id.len(), 40 | 64)
                    && commit_id
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        )
}

fn host_finalization_complete(
    observation: &HostFinalizationObservation,
    assessment: &RunEvidenceAssessment,
) -> bool {
    observation.cli_exit == super::ProcessExitObservation::ExitedNormally { exit_code: 0 }
        && observation
            .shell_observation
            .is_successful_for(observation.invocation_kind)
        && assessment.end_present
        && !assessment.stream_error
}

fn assessment_is_successful(assessment: &RunEvidenceAssessment) -> bool {
    assessment_is_successful_without_report(assessment)
        && assessment.report_reference.is_some()
        && assessment.report_save_confirmed
        && assessment.evidence_integrity_verified
}

fn report_has_content(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .map(|report| !report.trim().is_empty())
        .unwrap_or_else(|_| bytes.iter().any(|byte| !byte.is_ascii_whitespace()))
}

fn assessment_is_successful_without_report(assessment: &RunEvidenceAssessment) -> bool {
    assessment_execution_is_successful_without_report(assessment)
        && assessment.validation.state != ValidationState::Failed
}

fn assessment_execution_is_successful_without_report(assessment: &RunEvidenceAssessment) -> bool {
    assessment.final_exit_code == Some(0)
        && assessment.end_present
        && !assessment.stream_error
        && !assessment.cancelled
        && !assessment.known_failure
}

fn prompt_status(delivery: PromptDelivery) -> PromptStatus {
    match delivery {
        PromptDelivery::Accepted => PromptStatus::Accepted,
        PromptDelivery::Attempted => PromptStatus::Attempted,
        PromptDelivery::Unknown => PromptStatus::Unknown,
        PromptDelivery::Rejected => PromptStatus::Rejected,
    }
}

fn operation_key(prefix: &str, attempt_id: &str) -> String {
    format!("{prefix}-{}", &sha256_hex(attempt_id.as_bytes())[..32])
}

fn effect_operation_key(action: &str, attempt_id: &str) -> String {
    format!("{action}:{attempt_id}")
}

fn attempt_key(run_id: &str, attempt_id: &str) -> String {
    format!("{run_id}:{attempt_id}")
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha256_serialized<T: Serialize>(value: &T) -> Result<String, CoordinatorError> {
    serde_json::to_vec(value)
        .map(|bytes| sha256_hex(&bytes))
        .map_err(|_| CoordinatorError::Conflict)
}

fn publication_request(
    run: &AgentRunRecord,
    attempt_id: &str,
    frozen_commit: &str,
    base_branch: &str,
    approved_web_base_url: &str,
) -> PublicationRequest {
    let repository = &run.snapshot.repository.identity;
    PublicationRequest {
        run_id: run.snapshot.run_id.to_string(),
        attempt_id: attempt_id.into(),
        repository_host: repository.host.clone(),
        repository_owner: repository.organization.clone(),
        repository_name: repository.repository.clone(),
        approved_web_base_url: approved_web_base_url.to_owned(),
        branch: run.snapshot.workspace_branch.clone(),
        base_branch: base_branch.to_owned(),
        base_commit: run.snapshot.base_commit.clone(),
        frozen_commit: frozen_commit.into(),
        plan: run.snapshot.publication_plan.clone(),
        operation_key: operation_key("publish", attempt_id),
        pull_request_title: Some(format!(
            "{}: {}",
            run.snapshot.issue_id, run.snapshot.profile.display_name
        )),
        pull_request_body: Some(format!("Automated work for {}", run.snapshot.issue_id)),
    }
}

fn publication_request_matches_run(
    run: &AgentRunRecord,
    attempt_id: &str,
    assessment: &RunEvidenceAssessment,
    request: &PublicationRequest,
) -> bool {
    let Some(commit_id) = assessment.commit_id.as_deref() else {
        return false;
    };
    if !super::is_valid_branch_name(&request.base_branch)
        || !valid_web_base_url_for_host(
            &request.approved_web_base_url,
            &run.snapshot.repository.identity.host,
        )
    {
        return false;
    }
    request
        == &publication_request(
            run,
            attempt_id,
            commit_id,
            &request.base_branch,
            &request.approved_web_base_url,
        )
}

fn valid_web_base_url_for_host(value: &str, expected_host: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(expected_host))
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

fn publication_operation(request: &PublicationRequest) -> DurableOperationRequest {
    match request.plan {
        PublicationPlan::PushBranch => DurableOperationRequest::PublishBranch {
            branch: request.branch.clone(),
            commit_id: request.frozen_commit.clone(),
        },
        PublicationPlan::CreatePullRequest => DurableOperationRequest::CreatePullRequest {
            branch: request.branch.clone(),
            base_branch: request.base_branch.clone(),
            commit_id: request.frozen_commit.clone(),
        },
        PublicationPlan::NoPublication => unreachable!("publication request requires a plan"),
    }
}

fn valid_publication_receipt(request: &PublicationRequest, outcome: &PublicationOutcome) -> bool {
    match outcome {
        PublicationOutcome::NoSourceChanges { source } => {
            request.plan != PublicationPlan::NoPublication
                && super::publication::no_changes_receipt_matches(
                    source,
                    &request.run_id,
                    &request.attempt_id,
                    &request.branch,
                    &request.base_commit,
                    &request.frozen_commit,
                )
        }
        PublicationOutcome::BranchPublished {
            branch,
            commit,
            remote_head,
        } => {
            request.plan == PublicationPlan::PushBranch
                && branch == &request.branch
                && commit == &request.frozen_commit
                && remote_head == &request.frozen_commit
        }
        PublicationOutcome::PullRequestCreated {
            branch,
            head_commit,
            number,
            url,
        } => {
            request.plan == PublicationPlan::CreatePullRequest
                && branch == &request.branch
                && head_commit == &request.frozen_commit
                && *number > 0
                && valid_pull_request_url(&request.approved_web_base_url, url)
        }
        PublicationOutcome::NoChangesWithTreeProof {
            base_commit,
            frozen_commit,
            base_tree_id,
            frozen_tree_id,
        } => {
            is_full_commit_id(base_commit)
                && is_full_commit_id(frozen_commit)
                && base_commit == &request.base_commit
                && frozen_commit == &request.frozen_commit
                && is_full_commit_id(base_tree_id)
                && is_full_commit_id(frozen_tree_id)
                && base_tree_id == frozen_tree_id
        }
        PublicationOutcome::NoChanges { .. } => false,
        PublicationOutcome::Unknown | PublicationOutcome::Rejected => false,
    }
}

fn is_full_commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_pull_request_url(expected_base: &str, candidate: &str) -> bool {
    let Ok(expected) = url::Url::parse(expected_base) else {
        return false;
    };
    let Ok(actual) = url::Url::parse(candidate) else {
        return false;
    };
    actual.scheme() == "https"
        && actual.host_str() == expected.host_str()
        && actual.port_or_known_default() == expected.port_or_known_default()
        && actual.username().is_empty()
        && actual.password().is_none()
        && actual.query().is_none()
        && actual.fragment().is_none()
        && actual.path() != "/"
}

#[cfg(test)]
mod completion_consistency_tests {
    use std::collections::BTreeMap;

    use super::semantic_completion_conditions_met;
    use crate::coding_agent::{
        assessment::AssessmentUse,
        model::{ApprovedAcceptanceCriterion, CriterionEvaluationMethod},
        report::{
            CompletionAssessment, ReportExpectation, TaskKind, ValidatedReport, validate_report,
        },
    };

    fn criteria(required_method: CriterionEvaluationMethod) -> Vec<ApprovedAcceptanceCriterion> {
        vec![
            ApprovedAcceptanceCriterion {
                criterion_id: "AC-1".into(),
                statement: "Required acceptance criterion".into(),
                required: true,
                method: required_method,
            },
            ApprovedAcceptanceCriterion {
                criterion_id: "AC-2".into(),
                statement: "Optional acceptance criterion".into(),
                required: false,
                method: CriterionEvaluationMethod::Deterministic,
            },
        ]
    }

    fn report(
        status: &str,
        required_claim: &str,
        optional_claim: &str,
        blocks_completion: bool,
        validation_claim: &str,
    ) -> ValidatedReport {
        let expectation = ReportExpectation::new(
            "run-1",
            "attempt-1",
            TaskKind::Implementation,
            BTreeMap::from([("AC-1".into(), true), ("AC-2".into(), false)]),
            BTreeMap::from([("V-1".into(), true)]),
        )
        .unwrap();
        let unresolved = if blocks_completion {
            vec![serde_json::json!({
                "description": "A blocking issue remains.",
                "blocks_completion": true,
                "criterion_ids": ["AC-1"]
            })]
        } else {
            Vec::new()
        };
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "run_id": "run-1",
            "attempt_id": "attempt-1",
            "task_kind": "implementation",
            "status": status,
            "summary": "Completion consistency fixture.",
            "acceptance": [
                {"criterion_id": "AC-1", "assessment": required_claim, "evidence": ["report:attempt-1"]},
                {"criterion_id": "AC-2", "assessment": optional_claim, "evidence": ["report:attempt-1"]}
            ],
            "validation": [{
                "validation_id": "V-1",
                "claimed_result": validation_claim,
                "evidence": [],
                "reason": "Validation claims are not host receipts."
            }],
            "investigation": null,
            "implementation": {
                "change_summary": "Test report.",
                "changed_files": [],
                "implementation_notes": []
            },
            "unresolved": unresolved
        }))
        .unwrap();
        validate_report(&bytes, &expectation).unwrap()
    }

    #[test]
    fn semantic_pass_cannot_complete_inconsistent_or_non_completed_reports() {
        let criteria = criteria(CriterionEvaluationMethod::SemanticJudge);
        assert!(!semantic_completion_conditions_met(
            &criteria,
            None,
            AssessmentUse::Pass,
        ));
        for report in [
            report("completed", "unknown", "unmet", false, "passed"),
            report("completed", "met", "unmet", true, "passed"),
            report("partial", "met", "unmet", false, "passed"),
            report("blocked", "met", "unmet", false, "passed"),
        ] {
            assert!(!semantic_completion_conditions_met(
                &criteria,
                Some(&report),
                AssessmentUse::Pass,
            ));
        }
    }

    #[test]
    fn consistent_completed_report_allows_required_semantic_pass_without_promoting_optional_claims()
    {
        let criteria = criteria(CriterionEvaluationMethod::SemanticJudge);
        let report = report("completed", "met", "unmet", false, "not_run");
        assert_eq!(
            report.assessment.completion,
            CompletionAssessment::IncompleteRequiredValidation
        );
        assert!(semantic_completion_conditions_met(
            &criteria,
            Some(&report),
            AssessmentUse::Pass,
        ));
        assert!(!semantic_completion_conditions_met(
            &criteria,
            Some(&report),
            AssessmentUse::Fail,
        ));
    }

    #[test]
    fn semantic_pass_does_not_infer_required_deterministic_criterion_success() {
        let criteria = criteria(CriterionEvaluationMethod::Deterministic);
        let report = report("completed", "met", "unmet", false, "passed");
        assert!(!semantic_completion_conditions_met(
            &criteria,
            Some(&report),
            AssessmentUse::Pass,
        ));
    }
}
