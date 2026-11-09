//! Trusted adapter contract for workspace preparation and sandbox execution.
//!
//! No concrete jobworkerp/workflow binding is selected here. An application
//! must provide an adapter that obtains the observations below from live host
//! RPCs and the pinned executor protocol; missing adapters fail closed.

use std::{
    collections::{BTreeMap, BTreeSet},
    pin::Pin,
    sync::Arc,
};

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::assessment::AllowedEvidenceSource;
use super::executor_client::NativeExecutorClient;
use super::model::{
    ApprovalAssessmentContract, ApprovedValidationDefinition, ExecutionInvocationKind,
    ShellProcessObservation, StoredAgentRunSnapshot,
};
use super::repository::RepositoryIdentity;
use super::terminal::TerminalControl;
use super::validation::{ApprovedValidationPlan, ValidationRecord, ValidationTargetSnapshot};
use super::validation_store::ValidationEvidenceStore;
use super::{
    AgentRunSnapshot, ChildWritersStatus, HostResourceLimits, ProcessExitObservation,
    SandboxResources, ValidationEvidence, ValidationReceipt, WorkspaceFreezeStatus,
};
use coding_agent_executor::{
    RepositoryId, ResolveRequest, ResolvedArtifact, SealedArtifactReceipt,
};

pub type ExecutionEventStream =
    Pin<Box<dyn Stream<Item = Result<ExecutionEvent, ExecutionResult>> + Send + 'static>>;

/// Safe, non-secret result codes crossing from a trusted adapter.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ExecutionResult {
    #[error("execution adapter is unavailable")]
    AdapterUnavailable,
    #[error("execution capability is unsupported")]
    Unsupported,
    #[error("execution request conflicts with the approved snapshot")]
    Conflict,
    #[error("execution could not be reconciled")]
    ReconciliationUnknown,
    #[error("execution stream failed")]
    StreamFailure,
    #[error("host finalization evidence is unavailable")]
    FinalizationUnavailable,
    #[error("host cancellation is not confirmed")]
    CancellationUnknown,
}

/// Live observations returned by an adapter after querying runtime, worker,
/// schema, resource-limit, and helper-version state. Snapshot declarations
/// alone are not a substitute for these values.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeObservation {
    pub runtime_instance_id: String,
    pub health_observation_id: String,
    pub worker_name: String,
    pub worker_id: i64,
    pub runner_name: String,
    pub method_name: String,
    pub arguments_schema_sha256: String,
    pub result_schema_sha256: String,
    pub response_mode: String,
    pub output_is_streaming: bool,
    pub accepts_client_stream: bool,
    pub image_digest: String,
    pub admitted_resources: SandboxResources,
    /// Configured bounds used to validate this run's admitted profile. These do
    /// not attest aggregate VM usage or worker concurrency.
    pub host_limits: HostResourceLimits,
    pub helper_version: String,
    pub agent_cli_version: String,
    pub helper_protocol_version: u32,
    pub helper_features: BTreeSet<String>,
    pub workflow_execution_id: String,
    pub parent_job_id: String,
    pub child_sandbox_job_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceReceipt {
    pub run_id: String,
    pub attempt_id: String,
    pub workspace_token: String,
    pub repository_host: String,
    pub repository_owner: String,
    pub repository_name: String,
    pub base_commit: String,
    pub branch: String,
}

/// Identifies all jobworkerp stages belonging to one immutable attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionIdentity {
    pub run_id: String,
    pub attempt_id: String,
    pub workflow_execution_id: String,
    pub parent_job_id: String,
    pub child_sandbox_job_id: String,
    pub worker_id: i64,
    pub sandbox_job_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StartReceipt {
    pub identity: ExecutionIdentity,
    pub execution_intent_sha256: String,
    pub method_name: String,
    pub image_digest: String,
    pub prompt_delivery: PromptDelivery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptDelivery {
    /// A trusted adapter received semantic acceptance from the agent protocol.
    Accepted,
    /// The adapter attempted delivery but has no agent-level acknowledgement.
    Attempted,
    /// The adapter cannot determine whether delivery reached the child.
    Unknown,
    /// The child explicitly rejected the prompt.
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionEvent {
    Output { stream: String, data: Vec<u8> },
    Exit { exit_code: i32 },
}

/// Reconciliation never authorizes redispatch. `Unknown` may carry an
/// observed identity, but the coordinator treats it as nonterminal evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationReconciliation<T> {
    Accepted(T),
    NotAccepted,
    Unknown(Option<T>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostFinalizationObservation {
    pub invocation_kind: ExecutionInvocationKind,
    pub cli_exit: ProcessExitObservation,
    pub shell_observation: ShellProcessObservation,
    pub child_writers: ChildWritersStatus,
    pub workspace_freeze: WorkspaceFreezeStatus,
    /// Report bytes are host-captured output from the completed attempt.
    pub report: Option<Vec<u8>>,
    /// Validation and acceptance facts must be host-observed, never guest
    /// booleans copied directly from a terminal message.
    pub validation: ValidationEvidence,
    pub acceptance_conditions_met: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractiveFinishObservation {
    pub invocation_kind: ExecutionInvocationKind,
    pub cli_exit: ProcessExitObservation,
    pub shell_observation: ShellProcessObservation,
    pub child_writers: ChildWritersStatus,
    pub workspace_freeze: WorkspaceFreezeStatus,
    pub report: Option<Vec<u8>>,
    pub validation: ValidationEvidence,
    pub acceptance_conditions_met: bool,
}

/// Host-issued finalization evidence for an adapter that reopens its sealed
/// artifact and validation evidence stores. The adapter must bind receipts to
/// the approved command/profile and exact target snapshot. A report or legacy
/// boolean cannot be converted into this record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypedFinalizationEvidence {
    pub identity: ExecutionIdentity,
    pub invocation_kind: ExecutionInvocationKind,
    pub cli_exit: ProcessExitObservation,
    pub shell_observation: ShellProcessObservation,
    pub child_writers: ChildWritersStatus,
    pub workspace_freeze: WorkspaceFreezeStatus,
    pub sealed_artifact: Option<SealedArtifactBinding>,
    /// These must be reopened from the host's durable ValidationEvidenceStore.
    pub validation_receipts: Vec<ValidationReceipt>,
}

/// Full target identity produced by the host executor's sealed-workspace read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SealedArtifactBinding {
    pub full_commit_id: String,
    pub full_tree_id: String,
    pub untracked_manifest_digest: String,
    pub target_snapshot_digest: String,
}

/// The executor's native sealed receipt and target snapshot retained by the
/// host. A guest report or serialized `SealedArtifactBinding` is not a source
/// for this record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeSealedArtifactRecord {
    pub receipt: SealedArtifactReceipt,
    pub target: ValidationTargetSnapshot,
}

#[async_trait]
#[allow(clippy::double_must_use)] // async_trait's boxed future is already must-use.
pub trait NativeSealedArtifactLookup: Send + Sync {
    async fn lookup(
        &self,
        run_id: &str,
        attempt_id: &str,
    ) -> Result<Option<NativeSealedArtifactRecord>, ExecutionResult>;

    /// Read the exact sealed manifest bytes retained by the host for this
    /// attempt. The digest is checked against the reopened native receipt.
    async fn manifest_bytes(
        &self,
        _run_id: &str,
        _attempt_id: &str,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        Ok(None)
    }
}

/// Non-secret execution details selected by the host for one approved
/// validation. Command arguments, directory, permissions, and timeout are
/// always copied from the immutable approval definition.
pub struct ValidationPlanRuntime {
    pub worker_name: String,
    pub method_name: String,
    pub expected_sandbox_id: Option<String>,
    pub environment: BTreeMap<String, String>,
}

pub trait ValidationPlanRuntimeSource: Send + Sync {
    fn runtime_for(
        &self,
        snapshot: &AgentRunSnapshot,
        attempt_id: &str,
        definition: &ApprovedValidationDefinition,
    ) -> Result<Option<ValidationPlanRuntime>, ExecutionResult>;
}

pub trait ApprovedAssessmentContextSource: Send + Sync {
    /// Resolve only the exact source identity already recorded in approval.
    /// Implementations must not widen this into repository or home scanning.
    fn resolve(
        &self,
        run_id: &str,
        attempt_id: &str,
        source: &AllowedEvidenceSource,
    ) -> Result<Option<Vec<u8>>, ExecutionResult>;
}

#[derive(Clone, Debug)]
pub struct ReopenedValidationRun {
    pub plan: ApprovedValidationPlan,
    pub records: Vec<ValidationRecord>,
}

#[derive(Clone, Debug)]
pub struct VerifiedFinalization {
    pub artifact: ResolvedArtifact,
    pub target: ValidationTargetSnapshot,
    pub validation_runs: Vec<ReopenedValidationRun>,
    pub validation_receipts: Vec<ValidationReceipt>,
}

#[async_trait]
#[allow(clippy::double_must_use)] // async_trait's boxed future is already must-use.
pub trait VerifiedFinalizationSources: Send + Sync {
    /// Reopen native executor and validation-store evidence for this exact
    /// attempt. The adapter's typed fields are compared to, never used instead
    /// of, those host-owned records.
    async fn reopen(
        &self,
        snapshot: &AgentRunSnapshot,
        contract: &ApprovalAssessmentContract,
        evidence: &TypedFinalizationEvidence,
    ) -> Result<VerifiedFinalization, ExecutionResult>;

    /// Return the exact sealed snapshot manifest for the same attempt. It is
    /// persisted as bounded evidence and bound by its native digest; it is not
    /// included in judge input unless approval names it as an allowed source.
    async fn artifact_manifest_bytes(
        &self,
        _run_id: &str,
        _attempt_id: &str,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        Ok(None)
    }

    fn resolve_assessment_source(
        &self,
        run_id: &str,
        attempt_id: &str,
        source: &AllowedEvidenceSource,
    ) -> Result<Option<Vec<u8>>, ExecutionResult>;
}

/// Host implementation that resolves a retained native seal through the
/// pinned executor and reopens validation records and output bytes from the
/// private validation evidence store.
pub struct NativeExecutorFinalizationSources {
    executor: Arc<NativeExecutorClient>,
    sealed_artifacts: Arc<dyn NativeSealedArtifactLookup>,
    validation_store: Arc<ValidationEvidenceStore>,
    validation_runtime: Arc<dyn ValidationPlanRuntimeSource>,
    assessment_context: Arc<dyn ApprovedAssessmentContextSource>,
}

impl NativeExecutorFinalizationSources {
    pub fn new(
        executor: Arc<NativeExecutorClient>,
        sealed_artifacts: Arc<dyn NativeSealedArtifactLookup>,
        validation_store: Arc<ValidationEvidenceStore>,
        validation_runtime: Arc<dyn ValidationPlanRuntimeSource>,
        assessment_context: Arc<dyn ApprovedAssessmentContextSource>,
    ) -> Self {
        Self {
            executor,
            sealed_artifacts,
            validation_store,
            validation_runtime,
            assessment_context,
        }
    }
}

#[async_trait]
impl VerifiedFinalizationSources for NativeExecutorFinalizationSources {
    async fn reopen(
        &self,
        snapshot: &AgentRunSnapshot,
        contract: &ApprovalAssessmentContract,
        evidence: &TypedFinalizationEvidence,
    ) -> Result<VerifiedFinalization, ExecutionResult> {
        let identity = &evidence.identity;
        if identity.run_id != snapshot.run_id.as_str()
            || !snapshot
                .attempt_ids
                .iter()
                .any(|id| id == &identity.attempt_id)
            || contract.validate_for_snapshot(snapshot).is_err()
        {
            return Err(ExecutionResult::Conflict);
        }
        let sealed = self
            .sealed_artifacts
            .lookup(&identity.run_id, &identity.attempt_id)
            .await?
            .ok_or(ExecutionResult::FinalizationUnavailable)?;
        if sealed.receipt.run_id != identity.run_id
            || sealed.receipt.attempt_id != identity.attempt_id
            || sealed.receipt.repository != executor_repository(&snapshot.repository.identity)
            || sealed.receipt.base_commit != snapshot.base_commit
            || sealed.receipt.commit != sealed.target.full_commit_id()
            || sealed.receipt.tree != sealed.target.full_tree_id()
        {
            return Err(ExecutionResult::Conflict);
        }
        let artifact = self
            .executor
            .resolve(ResolveRequest {
                run_id: identity.run_id.clone(),
                attempt_id: identity.attempt_id.clone(),
                artifact_digest: sealed.receipt.artifact_digest.clone(),
            })
            .await
            .map_err(|_| ExecutionResult::FinalizationUnavailable)?;
        let snapshot_digest = approved_snapshot_digest(snapshot, contract)?;
        let binding = evidence
            .sealed_artifact
            .as_ref()
            .ok_or(ExecutionResult::FinalizationUnavailable)?;
        validate_resolved_sealed_artifact(
            identity,
            &snapshot.repository.identity,
            &snapshot.workspace_branch,
            &snapshot.base_commit,
            &snapshot_digest,
            binding,
            &sealed.target,
            &artifact,
        )?;
        if artifact.base_branch != sealed.receipt.base_branch
            || artifact.approved_snapshot_digest != sealed.receipt.approved_snapshot_digest
            || artifact.scope_digest != sealed.receipt.scope_digest
            || artifact.intent_approval_digest != sealed.receipt.intent_approval_digest
            || artifact.manifest_digest != sealed.receipt.manifest_digest
            || artifact.finalized_bytes_digest != sealed.receipt.finalized_bytes_digest
            || artifact.commit != sealed.receipt.commit
            || artifact.tree != sealed.receipt.tree
            || artifact.artifact_digest != sealed.receipt.artifact_digest
        {
            return Err(ExecutionResult::Conflict);
        }

        let mut validation_runs = Vec::new();
        let mut reopened_receipts = Vec::new();
        for definition in &contract.validation_plan {
            let Some(command) = definition.command.as_ref() else {
                if evidence
                    .validation_receipts
                    .iter()
                    .any(|receipt| receipt.validation_id() == definition.validation_id)
                {
                    return Err(ExecutionResult::Conflict);
                }
                continue;
            };
            let Some(runtime) =
                self.validation_runtime
                    .runtime_for(snapshot, &identity.attempt_id, definition)?
            else {
                if evidence
                    .validation_receipts
                    .iter()
                    .any(|receipt| receipt.validation_id() == definition.validation_id)
                {
                    return Err(ExecutionResult::Conflict);
                }
                continue;
            };
            if runtime.method_name != "run" || runtime.worker_name.is_empty() {
                return Err(ExecutionResult::Conflict);
            }
            let plan = match runtime.expected_sandbox_id {
                Some(sandbox_id) => ApprovedValidationPlan::new(
                    identity.run_id.clone(),
                    identity.attempt_id.clone(),
                    definition.validation_id.clone(),
                    definition.required,
                    runtime.worker_name,
                    sandbox_id,
                    command.argv.clone(),
                    command.working_directory.clone(),
                    runtime.environment,
                    command.profile_digest.clone(),
                    Some(command.timeout_ms),
                    sealed.target.clone(),
                ),
                None => ApprovedValidationPlan::new_for_worker(
                    identity.run_id.clone(),
                    identity.attempt_id.clone(),
                    definition.validation_id.clone(),
                    definition.required,
                    runtime.worker_name,
                    command.argv.clone(),
                    command.working_directory.clone(),
                    runtime.environment,
                    command.profile_digest.clone(),
                    Some(command.timeout_ms),
                    sealed.target.clone(),
                ),
            }
            .map_err(|_| ExecutionResult::Conflict)?;
            let records = self
                .validation_store
                .load_validation_records(&plan)
                .map_err(|_| ExecutionResult::FinalizationUnavailable)?;
            if records.len() > 1
                || records.iter().any(|record| {
                    record.run_id() != identity.run_id
                        || record.attempt_id() != identity.attempt_id
                        || record.validation_id() != definition.validation_id
                        || record.required() != definition.required
                        || record.binding_digest() != plan.binding_digest()
                })
            {
                return Err(ExecutionResult::Conflict);
            }
            for record in &records {
                if let Some(receipt) = record.receipt() {
                    if receipt.run_id() != identity.run_id
                        || receipt.attempt_id() != identity.attempt_id
                        || receipt.validation_id() != definition.validation_id
                        || receipt.required() != definition.required
                        || receipt.worker_name() != plan.worker_name()
                        || receipt.command_definition_digest() != plan.command_definition_digest()
                        || receipt.permission_profile_digest() != command.profile_digest
                        || receipt.target_full_commit_id() != sealed.target.full_commit_id()
                        || receipt.target_full_tree_id() != sealed.target.full_tree_id()
                        || receipt.target_untracked_manifest_digest()
                            != sealed.target.untracked_manifest_digest()
                        || receipt.target_snapshot_digest() != sealed.target.digest()
                    {
                        return Err(ExecutionResult::Conflict);
                    }
                    reopened_receipts.push(receipt.clone());
                }
            }
            validation_runs.push(ReopenedValidationRun { plan, records });
        }
        reopened_receipts.sort_by(|left, right| {
            (&left.validation_id(), &left.receipt_id())
                .cmp(&(&right.validation_id(), &right.receipt_id()))
        });
        let mut adapter_receipts = evidence.validation_receipts.clone();
        adapter_receipts.sort_by(|left, right| {
            (&left.validation_id(), &left.receipt_id())
                .cmp(&(&right.validation_id(), &right.receipt_id()))
        });
        if adapter_receipts != reopened_receipts {
            return Err(ExecutionResult::Conflict);
        }
        Ok(VerifiedFinalization {
            artifact,
            target: sealed.target,
            validation_runs,
            validation_receipts: reopened_receipts,
        })
    }

    async fn artifact_manifest_bytes(
        &self,
        run_id: &str,
        attempt_id: &str,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        self.sealed_artifacts
            .manifest_bytes(run_id, attempt_id)
            .await
    }

    fn resolve_assessment_source(
        &self,
        run_id: &str,
        attempt_id: &str,
        source: &AllowedEvidenceSource,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        self.assessment_context.resolve(run_id, attempt_id, source)
    }
}

fn executor_repository(repository: &RepositoryIdentity) -> RepositoryId {
    RepositoryId {
        host: repository.host.clone(),
        owner: repository.organization.clone(),
        name: repository.repository.clone(),
    }
}

fn approved_snapshot_digest(
    snapshot: &AgentRunSnapshot,
    contract: &ApprovalAssessmentContract,
) -> Result<String, ExecutionResult> {
    let stored = StoredAgentRunSnapshot::new(snapshot.clone(), Some(contract.clone()))
        .map_err(|_| ExecutionResult::Conflict)?;
    let bytes = serde_json::to_vec(&stored).map_err(|_| ExecutionResult::Conflict)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Cross-check the executor's protocol-2 resolve result against the retained
/// seal, approved base and full target. The caller still obtains `resolved`
/// from `NativeExecutorClient::resolve`; this helper alone grants no physical
/// process-stop or writer-quiescence proof.
#[allow(clippy::too_many_arguments)]
pub fn validate_resolved_sealed_artifact(
    identity: &ExecutionIdentity,
    repository: &RepositoryIdentity,
    branch: &str,
    base_commit: &str,
    approved_snapshot_digest: &str,
    binding: &SealedArtifactBinding,
    target: &ValidationTargetSnapshot,
    resolved: &ResolvedArtifact,
) -> Result<(), ExecutionResult> {
    let target_matches = ValidationTargetSnapshot::new(
        binding.full_commit_id.clone(),
        binding.full_tree_id.clone(),
        binding.untracked_manifest_digest.clone(),
    )
    .is_ok_and(|actual| actual == *target && actual.digest() == binding.target_snapshot_digest);
    if !target_matches
        || resolved.run_id != identity.run_id
        || resolved.attempt_id != identity.attempt_id
        || resolved.repository != executor_repository(repository)
        || resolved.branch != branch
        || resolved.base_commit != base_commit
        || resolved.approved_snapshot_digest != approved_snapshot_digest
        || resolved.commit != target.full_commit_id()
        || resolved.tree != target.full_tree_id()
        || resolved.protocol_version != coding_agent_executor::PROTOCOL_VERSION
        || resolved.executor_version != coding_agent_executor::EXECUTOR_VERSION
        || resolved.artifact_digest.len() != 64
        || !resolved
            .artifact_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ExecutionResult::Conflict);
    }
    Ok(())
}

/// Host-observed cancellation outcome. The coordinator accepts cancellation
/// only after the execution process has stopped and all child writers have
/// quiesced; a request acknowledgement alone is insufficient.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CancellationObservation {
    pub process_exit: ProcessExitObservation,
    pub child_writers: ChildWritersStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRequest {
    pub snapshot: AgentRunSnapshot,
    pub attempt_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImmutableExecutionIntent {
    pub run_id: String,
    pub attempt_id: String,
    pub issue_id: String,
    pub issue_revision: u64,
    pub execution_mode: super::ExecutionMode,
    pub purpose: super::RunPurpose,
    pub repository_host: String,
    pub repository_owner: String,
    pub repository_name: String,
    pub base_commit: String,
    pub branch: String,
    pub profile_id: String,
    pub profile_version: u64,
    pub image_digest: String,
    pub command: Vec<String>,
    pub workflow_definition_hash: String,
    pub workflow_execution_id: String,
    pub parent_job_id: String,
    pub child_sandbox_job_id: String,
    pub registered_worker_id: String,
    pub worker_id: i64,
    pub method_name: String,
    pub prompt_sha256: Option<String>,
}

impl ImmutableExecutionIntent {
    pub fn build(
        snapshot: &AgentRunSnapshot,
        attempt_id: &str,
        observation: &RuntimeObservation,
        prompt_sha256: Option<String>,
    ) -> Result<Self, ExecutionResult> {
        let workflow = &snapshot.workflow;
        let required = |value: &Option<String>| {
            value
                .as_deref()
                .filter(|value| !value.is_empty() && value.len() <= 128)
                .map(str::to_owned)
                .ok_or(ExecutionResult::Unsupported)
        };
        let registered_worker_id = required(&workflow.registered_worker_id)?;
        if registered_worker_id != observation.worker_id.to_string() {
            return Err(ExecutionResult::Conflict);
        }
        let identity = |value: &Option<String>| {
            if snapshot.execution_mode == super::ExecutionMode::OneShot && value.is_none() {
                Ok(String::new())
            } else {
                required(value)
            }
        };
        let workflow_execution_id = identity(&workflow.workflow_execution_id)?;
        let parent_job_id = identity(&workflow.parent_job_id)?;
        let child_sandbox_job_id = identity(&workflow.child_sandbox_job_id)?;
        if workflow_execution_id != observation.workflow_execution_id
            || parent_job_id != observation.parent_job_id
            || child_sandbox_job_id != observation.child_sandbox_job_id
        {
            return Err(ExecutionResult::Conflict);
        }
        let repository = &snapshot.repository.identity;
        Ok(Self {
            run_id: snapshot.run_id.to_string(),
            attempt_id: attempt_id.into(),
            issue_id: snapshot.issue_id.clone(),
            issue_revision: snapshot.issue_revision,
            execution_mode: snapshot.execution_mode,
            purpose: snapshot.purpose,
            repository_host: repository.host.clone(),
            repository_owner: repository.organization.clone(),
            repository_name: repository.repository.clone(),
            base_commit: snapshot.base_commit.clone(),
            branch: snapshot.workspace_branch.clone(),
            profile_id: snapshot.profile.profile_id.clone(),
            profile_version: snapshot.profile.version,
            image_digest: snapshot.profile.image_digest.clone(),
            command: snapshot.profile.command.clone(),
            workflow_definition_hash: workflow.definition_hash.clone(),
            workflow_execution_id,
            parent_job_id,
            child_sandbox_job_id,
            registered_worker_id,
            worker_id: observation.worker_id,
            method_name: observation.method_name.clone(),
            prompt_sha256,
        })
    }
}

#[async_trait]
#[allow(clippy::double_must_use)] // async_trait's boxed future is already must-use.
pub trait ExecutionAdapter: Send + Sync {
    /// Query live runtime facts; values copied only from AgentProfileSnapshot
    /// are not verification evidence.
    async fn preflight(
        &self,
        snapshot: &AgentRunSnapshot,
    ) -> Result<RuntimeObservation, ExecutionResult>;

    async fn prepare_workspace(
        &self,
        request: &WorkspaceRequest,
    ) -> Result<WorkspaceReceipt, ExecutionResult>;

    async fn reconcile_workspace(
        &self,
        request: &WorkspaceRequest,
    ) -> Result<OperationReconciliation<WorkspaceReceipt>, ExecutionResult>;

    async fn start_attempt(
        &self,
        snapshot: &AgentRunSnapshot,
        intent: &ImmutableExecutionIntent,
        workspace: &WorkspaceReceipt,
        prompt: Option<Vec<u8>>,
    ) -> Result<StartReceipt, ExecutionResult>;

    async fn reconcile_start(
        &self,
        snapshot: &AgentRunSnapshot,
        intent: &ImmutableExecutionIntent,
    ) -> Result<OperationReconciliation<StartReceipt>, ExecutionResult>;

    /// Each result stream is exclusively drained by one app-owned task.
    async fn events(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<ExecutionEventStream, ExecutionResult>;

    /// Release execution-owned background maintenance after automatic processing stops.
    async fn execution_finished(&self, _identity: &ExecutionIdentity) {}

    async fn terminal_control(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<Arc<dyn TerminalControl>, ExecutionResult>;

    /// Observe the actual CLI result and read the completed attempt's report.
    /// Publication checks commit evidence separately from workflow completion.
    async fn finalize_one_shot(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<HostFinalizationObservation, ExecutionResult>;

    /// Read the mounted workspace through its approved host executor.
    async fn source_inspection(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<Option<coding_agent_executor::SourceInspection>, ExecutionResult> {
        Ok(None)
    }

    /// Evaluate the completed task using actual mounted source, report, and host validation.
    async fn assess_semantics(
        &self,
        _identity: &ExecutionIdentity,
        _source: Option<&coding_agent_executor::SourceInspection>,
        _report: &[u8],
        _validation: &ValidationEvidence,
    ) -> Result<super::semantic_judge::SemanticAssessment, ExecutionResult> {
        Err(ExecutionResult::Unsupported)
    }

    /// A distinct human request may judge the retained workspace again; it never restarts the agent.
    async fn reassess_semantics(
        &self,
        _identity: &ExecutionIdentity,
        _source: &coding_agent_executor::SourceInspection,
        _report: &[u8],
        _validation: &ValidationEvidence,
        _request_key: &str,
    ) -> Result<super::semantic_judge::SemanticAssessment, ExecutionResult> {
        Err(ExecutionResult::Unsupported)
    }

    /// Admit approved validation while the coordinator serializes cancellation.
    async fn begin_workspace_validation(
        &self,
        _identity: &ExecutionIdentity,
        _definition: &super::model::ApprovedValidationDefinition,
    ) -> Result<super::workspace_commit::LocalCommitStart, ExecutionResult> {
        Err(ExecutionResult::Unsupported)
    }

    async fn validate_workspace(
        &self,
        _identity: &ExecutionIdentity,
        _definition: &super::model::ApprovedValidationDefinition,
    ) -> Result<super::workspace_validation::WorkspaceValidationObservation, ExecutionResult> {
        Err(ExecutionResult::Unsupported)
    }

    /// Admit the local commit while the coordinator serializes cancellation.
    async fn begin_workspace_commit(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<Option<super::workspace_commit::LocalCommitStart>, ExecutionResult> {
        Ok(None)
    }

    /// Run the approved local commit phase after durable result and report gates.
    async fn commit_workspace(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<Option<super::workspace_commit::LocalCommitObservation>, ExecutionResult> {
        Ok(None)
    }

    /// Preserve an actual commit result even when source inspection is unavailable.
    async fn commit_workflow_result(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        Ok(None)
    }

    /// Actual workflow completion metadata, when supplied by the existing runtime API.
    async fn workflow_result(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        Ok(None)
    }

    /// Reserve result publication while the coordinator serializes cancellation.
    async fn begin_issue_result(
        &self,
        _identity: &ExecutionIdentity,
        _commit_id: Option<&str>,
        _report: &[u8],
    ) -> Result<(), ExecutionResult> {
        Ok(())
    }

    /// Drain the already admitted result publication through its owned Issue claim.
    async fn save_issue_result(
        &self,
        _identity: &ExecutionIdentity,
        _commit_id: Option<&str>,
        _report: &[u8],
    ) -> Result<Option<Vec<u8>>, ExecutionResult> {
        Ok(None)
    }

    async fn finalize_interactive(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<InteractiveFinishObservation, ExecutionResult>;

    /// Reopen host-issued finalization records for gates that require semantic
    /// and validation receipts. Old adapters remain source-compatible and fail
    /// closed until they can verify these durable records.
    async fn finalization_evidence(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<TypedFinalizationEvidence, ExecutionResult> {
        Err(ExecutionResult::FinalizationUnavailable)
    }

    /// Lookup-only recovery for an existing persisted interactive-finish
    /// request. Implementations must not repeat finish side effects.
    async fn reconcile_interactive_finish(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<InteractiveFinishObservation>, ExecutionResult>;

    /// Cancellation is separate from closing a terminal input stream.
    async fn cancel(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<CancellationObservation>, ExecutionResult>;

    /// Lookup-only reconciliation for an existing durable cancellation intent.
    /// It must not issue another cancel/delete request.
    async fn reconcile_cancel(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<OperationReconciliation<CancellationObservation>, ExecutionResult>;

    /// Direct control RPC path. It must not acquire an execution/workflow slot.
    async fn heartbeat(&self, identity: &ExecutionIdentity) -> Result<(), ExecutionResult>;
}

pub(crate) fn validate_runtime_observation(
    snapshot: &AgentRunSnapshot,
    observation: &RuntimeObservation,
    prompt_requested: bool,
) -> Vec<String> {
    let mut blockers = Vec::new();
    let method = match snapshot.execution_mode {
        super::ExecutionMode::OneShot => "run",
        super::ExecutionMode::Interactive => "run_with_client",
    };
    let expected_client_stream = snapshot.execution_mode == super::ExecutionMode::Interactive;
    let one_shot = snapshot.execution_mode == super::ExecutionMode::OneShot;
    let expected_runner = if one_shot { "WORKFLOW" } else { "SANDBOX" };
    let identity_matches = |expected: &Option<String>, observed: &str| match expected.as_deref() {
        Some(expected) => !expected.is_empty() && expected == observed,
        None => one_shot && observed.is_empty(),
    };
    let expected_registered_worker = snapshot
        .workflow
        .registered_worker_id
        .as_deref()
        .and_then(|id| id.parse::<i64>().ok());
    let resources_match = observation.admitted_resources == snapshot.profile.resources
        && observation.host_limits == snapshot.profile.validated_host_limits
        && observation
            .admitted_resources
            .validate_against(&observation.host_limits)
            .is_ok();
    let required_features = [
        "prepare",
        "frozen_commit_import",
        "host_process_observation",
        "host_writer_quiescence",
        "host_workspace_freeze",
        "binary_output_capture",
        "protected_remote_auth_forwarding_verified",
    ];
    if observation.runtime_instance_id.is_empty()
        || observation.health_observation_id.is_empty()
        || observation.worker_name.is_empty()
        || observation.worker_id <= 0
        || observation.runner_name != expected_runner
        || observation.method_name != method
        || observation.response_mode != "DIRECT"
        || !observation.output_is_streaming
        || observation.accepts_client_stream != expected_client_stream
        || observation.arguments_schema_sha256.len() != 64
        || observation.result_schema_sha256.len() != 64
        || observation
            .arguments_schema_sha256
            .chars()
            .any(|c| !c.is_ascii_hexdigit())
        || observation
            .result_schema_sha256
            .chars()
            .any(|c| !c.is_ascii_hexdigit())
        || expected_registered_worker != Some(observation.worker_id)
        || observation.image_digest != snapshot.profile.image_digest
        || !resources_match
        || (!one_shot
            && (observation.helper_version.is_empty() || observation.helper_protocol_version != 1))
        || !identity_matches(
            &snapshot.workflow.workflow_execution_id,
            &observation.workflow_execution_id,
        )
        || !identity_matches(&snapshot.workflow.parent_job_id, &observation.parent_job_id)
        || !identity_matches(
            &snapshot.workflow.child_sandbox_job_id,
            &observation.child_sandbox_job_id,
        )
    {
        blockers.push("verified_runtime_schema_resources_or_identity".to_owned());
    }
    if !one_shot {
        blockers.extend(
            required_features
                .into_iter()
                .filter(|feature| !observation.helper_features.contains(*feature))
                .map(str::to_owned),
        );
    }
    if prompt_requested && !one_shot {
        let prompt_feature = match snapshot.execution_mode {
            super::ExecutionMode::OneShot => "startup_prompt_flag_verified",
            super::ExecutionMode::Interactive => "interactive_prompt_adapter",
        };
        if !observation.helper_features.contains(prompt_feature) {
            blockers.push(prompt_feature.to_owned());
        }
    }
    if snapshot.execution_mode == super::ExecutionMode::Interactive
        && !observation.helper_features.contains("interactive_finish")
    {
        blockers.push("interactive_finalization_unsupported".to_owned());
    }
    blockers.sort();
    blockers.dedup();
    blockers
}
