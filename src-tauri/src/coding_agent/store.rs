use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::data::DataPaths;

use super::delegation::{DelegationPlanRecord, DelegationPlanRequest};
use super::execution::{
    ExecutionIdentity, ImmutableExecutionIntent, StartReceipt, TypedFinalizationEvidence,
    VerifiedFinalization,
};
use super::model::{
    AgentRunRecord, AgentRunSnapshot, AllowedOperation, ApprovalAssessmentContract,
    ApprovalAssessmentContractStatus, AssessmentIntentBinding, AssessmentIntentClaim,
    AssessmentIntentRequest, AssessmentValidationReceiptReference, ChildWritersStatus,
    CompletionActionKind, CompletionAuthorizationStatus, DurableOperationClaim,
    DurableOperationOutcome, DurableOperationRecord, DurableOperationRequest,
    DurableOperationResult, DurableOperationStatus, EvidenceReference, ExecutionInvocationKind,
    InteractiveFinalizationEvidence, InteractiveFinishRequest, PrimaryOperationReceipt,
    PrimaryTrackerIdentity, ProcessExitObservation, PublicationEffectClaim,
    PublicationEffectPermit, PublicationPlan, RunAttempt, RunCompletionAuthorization,
    RunEvidenceAssessment, RunId, RunModelError, RunPurpose, RunStage, RunState,
    ShellProcessObservation, StartAttemptResult, StoredAgentRunSnapshot, ValidationState,
    WorkspaceFreezeStatus, can_transition, evaluate_completion_gates, is_valid_branch_name,
};
use super::profile::ProfileError;
use super::report::{ReportExpectation, TaskKind, validate_report};
use super::repository::RepositoryError;
use super::validation::{ValidationOutcome, ValidationRecord};

const STORE_SCHEMA_VERSION: i64 = 11;
const VERIFIED_FINALIZATION_INPUTS_SCHEMA_VERSION: u32 = 1;
const VERIFIED_FINALIZATION_INPUTS_EVIDENCE_NAME: &str = "verified-finalization-inputs-v1";
const MAX_VERIFIED_FINALIZATION_INPUTS_BYTES: usize = 8 * 1024 * 1024;
const MAX_FINALIZATION_STREAM_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FINALIZATION_VALIDATIONS: usize = 128;
const MAX_START_INTENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_START_RECEIPT_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedIssueResultAck {
    schema_version: u32,
    run_id: String,
    attempt_id: String,
    status: String,
    response: Option<super::issue_gateway::AutomaticIssueReviewReceipt>,
}

/// Host-captured references and stream termination facts. It intentionally has
/// no field for semantic acceptance or any other judge result.
#[derive(Clone, Debug, Serialize)]
pub struct FinalizationCaptureInputs {
    report_reference: EvidenceReference,
    stdout_reference: Option<EvidenceReference>,
    stderr_reference: Option<EvidenceReference>,
    end_present: bool,
    stream_error: bool,
    cancelled: bool,
}

impl FinalizationCaptureInputs {
    pub fn new(
        report_reference: EvidenceReference,
        stdout_reference: Option<EvidenceReference>,
        stderr_reference: Option<EvidenceReference>,
        end_present: bool,
        stream_error: bool,
        cancelled: bool,
    ) -> Self {
        Self {
            report_reference,
            stdout_reference,
            stderr_reference,
            end_present,
            stream_error,
            cancelled,
        }
    }
}

/// Immutable host observations captured before the judge payload is claimed.
/// Private fields and the lack of `Deserialize` keep serialized guest data from
/// becoming a host verification capability.
#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostVerifiedFinalizationInputs {
    schema_version: u32,
    identity: ExecutionIdentity,
    invocation_kind: super::model::ExecutionInvocationKind,
    cli_exit: super::model::ProcessExitObservation,
    shell_observation: super::model::ShellProcessObservation,
    child_writers: super::model::ChildWritersStatus,
    workspace_freeze: WorkspaceFreezeStatus,
    sealed_artifact: StoredFinalizationTarget,
    artifact: coding_agent_executor::ResolvedArtifact,
    target: StoredFinalizationTarget,
    report_reference: EvidenceReference,
    stdout_reference: Option<EvidenceReference>,
    stderr_reference: Option<EvidenceReference>,
    end_present: bool,
    stream_error: bool,
    cancelled: bool,
    validation_receipts: Vec<super::validation::ValidationReceipt>,
    validation_receipt_json: Vec<String>,
    validation_receipt_references: Vec<AssessmentValidationReceiptReference>,
    validation_records: Vec<ValidationRecord>,
    validation_record_json: Vec<String>,
    validation_record_facts: Vec<StoredFinalizationValidationRecord>,
    validation_plans: Vec<StoredFinalizationValidationPlan>,
}

impl HostVerifiedFinalizationInputs {
    pub fn from_verified_finalization(
        typed: &TypedFinalizationEvidence,
        verified: &VerifiedFinalization,
        capture: FinalizationCaptureInputs,
    ) -> Result<Self, CodingAgentError> {
        let sealed = typed.sealed_artifact.as_ref().ok_or_else(|| {
            CodingAgentError::Conflict(
                "verified finalization requires a retained native sealed-artifact binding".into(),
            )
        })?;
        let target = StoredFinalizationTarget::from_verified(verified);
        let sealed_artifact = StoredFinalizationTarget {
            full_commit_id: sealed.full_commit_id.clone(),
            full_tree_id: sealed.full_tree_id.clone(),
            untracked_manifest_digest: sealed.untracked_manifest_digest.clone(),
            target_snapshot_digest: sealed.target_snapshot_digest.clone(),
            artifact_manifest_digest: verified.artifact.manifest_digest.clone(),
            artifact_digest: verified.artifact.artifact_digest.clone(),
        };
        if typed.validation_receipts != verified.validation_receipts
            || typed.identity.run_id != verified.artifact.run_id
            || typed.identity.attempt_id != verified.artifact.attempt_id
            || target.full_commit_id != verified.artifact.commit
            || target.full_tree_id != verified.artifact.tree
            || target.full_commit_id != sealed.full_commit_id
            || target.full_tree_id != sealed.full_tree_id
            || target.untracked_manifest_digest != sealed.untracked_manifest_digest
            || target.target_snapshot_digest != sealed.target_snapshot_digest
            || matches!(
                &typed.workspace_freeze,
                WorkspaceFreezeStatus::HostVerified { commit_id }
                    if commit_id != &target.full_commit_id
            )
        {
            return Err(CodingAgentError::Conflict(
                "native finalization, sealed target, and captured workspace facts disagree".into(),
            ));
        }
        validate_finalization_capture_metadata(&capture)?;

        let mut validation_plans = Vec::with_capacity(verified.validation_runs.len());
        let mut plan_ids = BTreeSet::new();
        let mut validation_records = Vec::new();
        let mut record_facts = Vec::new();
        let mut record_ids = BTreeSet::new();
        for reopened in &verified.validation_runs {
            let plan = &reopened.plan;
            if plan.run_id() != typed.identity.run_id
                || plan.attempt_id() != typed.identity.attempt_id
                || plan.target().full_commit_id() != target.full_commit_id
                || plan.target().full_tree_id() != target.full_tree_id
                || plan.target().untracked_manifest_digest() != target.untracked_manifest_digest
                || plan.target().digest() != target.target_snapshot_digest
                || !plan_ids.insert(plan.validation_id().to_owned())
            {
                return Err(CodingAgentError::Conflict(
                    "reopened validation plan is not bound to the native finalization target"
                        .into(),
                ));
            }
            validation_plans.push(StoredFinalizationValidationPlan {
                validation_id: plan.validation_id().into(),
                required: plan.required(),
                argv: plan.argv().to_vec(),
                working_directory: plan.working_directory().into(),
                command_definition_digest: plan.command_definition_digest().into(),
                binding_digest: plan.binding_digest().into(),
                permission_profile_digest: plan.permission_profile_digest().into(),
            });
            for record in &reopened.records {
                if record.run_id() != typed.identity.run_id
                    || record.attempt_id() != typed.identity.attempt_id
                    || record.validation_id() != plan.validation_id()
                    || record.required() != plan.required()
                    || record.binding_digest() != plan.binding_digest()
                    || !record_ids.insert(record.validation_id().to_owned())
                {
                    return Err(CodingAgentError::Conflict(
                        "reopened validation record differs from its immutable plan".into(),
                    ));
                }
                let record_json = serde_json::to_string(record)?;
                let record_size = record_json.len();
                if record_size > super::validation::MAX_VALIDATION_RECORD_BYTES
                    || record_facts.len() >= MAX_FINALIZATION_VALIDATIONS
                {
                    return Err(CodingAgentError::Invalid(
                        "reopened validation records exceed the bounded finalization limit".into(),
                    ));
                }
                let (receipt_id, receipt_digest) = match record.receipt() {
                    Some(receipt) => {
                        let serialized = serde_json::to_string(receipt)?;
                        if !verified
                            .validation_receipts
                            .iter()
                            .any(|verified| verified == receipt)
                        {
                            return Err(CodingAgentError::Conflict(
                                "validation record receipt was not reopened from the host store"
                                    .into(),
                            ));
                        }
                        (
                            Some(receipt.receipt_id().to_owned()),
                            Some(format!("{:x}", Sha256::digest(serialized.as_bytes()))),
                        )
                    }
                    None => (None, None),
                };
                record_facts.push(StoredFinalizationValidationRecord {
                    validation_id: record.validation_id().into(),
                    required: record.required(),
                    outcome: record.outcome(),
                    digest: format!("{:x}", Sha256::digest(record_json.as_bytes())),
                    receipt_id,
                    receipt_digest,
                    end_observed: record.end_observed(),
                    transport_error_observed: record.transport_error_observed(),
                    serialized: record_json.clone(),
                });
                validation_records.push(record.clone());
            }
        }
        if validation_plans.len() > MAX_FINALIZATION_VALIDATIONS
            || verified.validation_receipts.len() > MAX_FINALIZATION_VALIDATIONS
            || validation_records.len() > MAX_FINALIZATION_VALIDATIONS
        {
            return Err(CodingAgentError::Invalid(
                "verified finalization contains too many validation records".into(),
            ));
        }

        let mut validation_receipt_references = verified
            .validation_receipts
            .iter()
            .map(validation_receipt_reference)
            .collect::<Result<Vec<_>, _>>()?;
        validation_receipt_references
            .sort_by(|left, right| left.validation_id.cmp(&right.validation_id));
        if validation_receipt_references
            .windows(2)
            .any(|pair| pair[0].validation_id == pair[1].validation_id)
        {
            return Err(CodingAgentError::Conflict(
                "verified finalization contains duplicate validation receipts".into(),
            ));
        }
        let validation_receipt_json = verified
            .validation_receipts
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            schema_version: VERIFIED_FINALIZATION_INPUTS_SCHEMA_VERSION,
            identity: typed.identity.clone(),
            invocation_kind: typed.invocation_kind,
            cli_exit: typed.cli_exit.clone(),
            shell_observation: typed.shell_observation.clone(),
            child_writers: typed.child_writers,
            workspace_freeze: typed.workspace_freeze.clone(),
            sealed_artifact,
            artifact: verified.artifact.clone(),
            target,
            report_reference: capture.report_reference,
            stdout_reference: capture.stdout_reference,
            stderr_reference: capture.stderr_reference,
            end_present: capture.end_present,
            stream_error: capture.stream_error,
            cancelled: capture.cancelled,
            validation_receipts: verified.validation_receipts.clone(),
            validation_receipt_json,
            validation_receipt_references,
            validation_records,
            validation_record_json: record_facts
                .iter()
                .map(|record| record.serialized.clone())
                .collect(),
            validation_record_facts: record_facts,
            validation_plans,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredFinalizationTarget {
    full_commit_id: String,
    full_tree_id: String,
    untracked_manifest_digest: String,
    target_snapshot_digest: String,
    artifact_manifest_digest: String,
    artifact_digest: String,
}

impl StoredFinalizationTarget {
    fn from_verified(verified: &VerifiedFinalization) -> Self {
        Self {
            full_commit_id: verified.target.full_commit_id().into(),
            full_tree_id: verified.target.full_tree_id().into(),
            untracked_manifest_digest: verified.target.untracked_manifest_digest().into(),
            target_snapshot_digest: verified.target.digest().into(),
            artifact_manifest_digest: verified.artifact.manifest_digest.clone(),
            artifact_digest: verified.artifact.artifact_digest.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredFinalizationValidationRecord {
    validation_id: String,
    required: bool,
    outcome: ValidationOutcome,
    digest: String,
    receipt_id: Option<String>,
    receipt_digest: Option<String>,
    end_observed: bool,
    transport_error_observed: bool,
    serialized: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredFinalizationValidationPlan {
    validation_id: String,
    required: bool,
    argv: Vec<String>,
    working_directory: String,
    command_definition_digest: String,
    binding_digest: String,
    permission_profile_digest: String,
}

/// Reopened durable host inputs returned by the store. Receipt/record payloads
/// are exposed only as validated digest references, never rehydrated as proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SavedVerifiedFinalizationInputs {
    index_reference: EvidenceReference,
    identity: ExecutionIdentity,
    report_reference: EvidenceReference,
    stdout_reference: Option<EvidenceReference>,
    stderr_reference: Option<EvidenceReference>,
    invocation_kind: super::model::ExecutionInvocationKind,
    cli_exit: super::model::ProcessExitObservation,
    shell_observation: super::model::ShellProcessObservation,
    child_writers: super::model::ChildWritersStatus,
    workspace_freeze: WorkspaceFreezeStatus,
    sealed_artifact: StoredFinalizationTarget,
    target: StoredFinalizationTarget,
    end_present: bool,
    stream_error: bool,
    cancelled: bool,
    validation_receipt_references: Vec<AssessmentValidationReceiptReference>,
    validation_record_facts: Vec<StoredFinalizationValidationRecord>,
    validation_plans: Vec<StoredFinalizationValidationPlan>,
}

impl SavedVerifiedFinalizationInputs {
    pub fn index_reference(&self) -> &EvidenceReference {
        &self.index_reference
    }

    pub fn report_reference(&self) -> &EvidenceReference {
        &self.report_reference
    }

    pub fn stdout_reference(&self) -> Option<&EvidenceReference> {
        self.stdout_reference.as_ref()
    }

    pub fn stderr_reference(&self) -> Option<&EvidenceReference> {
        self.stderr_reference.as_ref()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredHostVerifiedFinalizationInputs {
    schema_version: u32,
    identity: StoredExecutionIdentity,
    invocation_kind: super::model::ExecutionInvocationKind,
    cli_exit: super::model::ProcessExitObservation,
    shell_observation: super::model::ShellProcessObservation,
    child_writers: super::model::ChildWritersStatus,
    workspace_freeze: WorkspaceFreezeStatus,
    sealed_artifact: StoredFinalizationTarget,
    artifact: coding_agent_executor::ResolvedArtifact,
    target: StoredFinalizationTarget,
    report_reference: StoredEvidenceReference,
    stdout_reference: Option<StoredEvidenceReference>,
    stderr_reference: Option<StoredEvidenceReference>,
    end_present: bool,
    stream_error: bool,
    cancelled: bool,
    validation_receipts: Vec<serde_json::Value>,
    validation_receipt_json: Vec<String>,
    validation_receipt_references: Vec<AssessmentValidationReceiptReference>,
    validation_records: Vec<serde_json::Value>,
    validation_record_json: Vec<String>,
    validation_record_facts: Vec<StoredFinalizationValidationRecord>,
    validation_plans: Vec<StoredFinalizationValidationPlan>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredExecutionIdentity {
    run_id: String,
    attempt_id: String,
    workflow_execution_id: String,
    parent_job_id: String,
    child_sandbox_job_id: String,
    worker_id: i64,
    sandbox_job_id: String,
}

struct AcceptedStartOperationRecord {
    claim_generation: i64,
    operation_generation: i64,
    operation_attempt_id: String,
    operation_payload_hash: String,
    kind_json: String,
    request_json: String,
    status_json: String,
    result_json: Option<String>,
}

impl From<StoredExecutionIdentity> for ExecutionIdentity {
    fn from(value: StoredExecutionIdentity) -> Self {
        Self {
            run_id: value.run_id,
            attempt_id: value.attempt_id,
            workflow_execution_id: value.workflow_execution_id,
            parent_job_id: value.parent_job_id,
            child_sandbox_job_id: value.child_sandbox_job_id,
            worker_id: value.worker_id,
            sandbox_job_id: value.sandbox_job_id,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredEvidenceReference {
    evidence_id: String,
    attempt_id: String,
    name: String,
    media_type: String,
    sha256: String,
    size_bytes: u64,
    save_confirmed: bool,
    created_at_ms: u64,
}

impl From<StoredEvidenceReference> for EvidenceReference {
    fn from(value: StoredEvidenceReference) -> Self {
        Self {
            evidence_id: value.evidence_id,
            attempt_id: value.attempt_id,
            name: value.name,
            media_type: value.media_type,
            sha256: value.sha256,
            size_bytes: value.size_bytes,
            save_confirmed: value.save_confirmed,
            created_at_ms: value.created_at_ms,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredValidationReceiptDto {
    schema_version: u32,
    receipt_id: String,
    binding_digest: String,
    run_id: String,
    attempt_id: String,
    validation_id: String,
    required: bool,
    worker_name: String,
    expected_sandbox_id: Option<String>,
    sandbox_id: String,
    command_definition_digest: String,
    permission_profile_digest: String,
    child_job_id: String,
    exit_code: i32,
    execution_time_ms: u64,
    end_observed: bool,
    stream_error_observed: bool,
    target_full_commit_id: String,
    target_full_tree_id: String,
    target_untracked_manifest_digest: String,
    target_snapshot_digest: String,
    output: StoredValidationOutputDto,
    outcome: ValidationOutcome,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredValidationOutputDto {
    reference: String,
    stdout_sha256: String,
    stderr_sha256: String,
    stdout_bytes: usize,
    stderr_bytes: usize,
    capture_complete: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredValidationRecordDto {
    schema_version: u32,
    run_id: String,
    attempt_id: String,
    validation_id: String,
    required: bool,
    binding_digest: String,
    outcome: ValidationOutcome,
    child_job_id: Option<String>,
    sandbox_id: Option<String>,
    exit_code: Option<i32>,
    execution_time_ms: Option<u64>,
    end_observed: bool,
    transport_error_observed: bool,
    receipt: Option<StoredValidationReceiptDto>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredJudgeInput {
    schema_version: u8,
    assessment_id: String,
    run_id: String,
    attempt_id: String,
    task_kind: TaskKind,
    approval_snapshot_digest: String,
    frozen_artifact_digest: String,
    report_digest: String,
    validation_receipts: Vec<super::assessment::ValidationReceiptBinding>,
    validation_plan: Vec<super::assessment::ApprovedValidationDefinition>,
    criteria: Vec<super::assessment::ApprovedCriterion>,
    evidence: Vec<super::assessment::EvidencePayload>,
    provider: super::assessment::ProviderSelection,
    prompt_policy_version: String,
    judge_input_scope_digest: String,
}

struct SavedJudgeInputValidationContext<'a> {
    run_id: &'a RunId,
    attempt_id: &'a str,
    assessment_id: &'a str,
    snapshot_json: &'a str,
    contract: &'a ApprovalAssessmentContract,
    contract_digest: &'a str,
    report_reference: &'a EvidenceReference,
    report_bytes: &'a [u8],
    saved: &'a SavedVerifiedFinalizationInputs,
}

#[derive(Clone)]
pub(crate) struct CompletionCandidate {
    pub(crate) run: AgentRunRecord,
    pub(crate) assessment: RunEvidenceAssessment,
    pub(crate) commit_id: String,
    pub(crate) marker_evidence_references: Vec<String>,
    pub(crate) marker_revision: Option<u64>,
    pub(crate) human_semantic_issue_revision: Option<u64>,
    pub(crate) snapshot_sha256: String,
    pub(crate) evidence_sha256: String,
}

pub(crate) struct NewCompletionAuthorization {
    pub(crate) action: CompletionActionKind,
    pub(crate) actor_id: String,
    pub(crate) policy_identity: String,
    pub(crate) policy_version: u64,
    pub(crate) report_sha256: String,
    pub(crate) completion_report_protobuf: Vec<u8>,
    pub(crate) audit_reason: String,
    pub(crate) marker_revision: u64,
    pub(crate) target_issue_revision: u64,
    pub(crate) intent_idempotency_key: String,
    pub(crate) primary_tracker_identity: PrimaryTrackerIdentity,
}

#[derive(Debug, Error)]
pub enum CodingAgentError {
    #[error("invalid coding-agent data: {0}")]
    Invalid(String),
    #[error("coding-agent operation conflicts with durable state: {0}")]
    Conflict(String),
    #[error("coding-agent record not found: {0}")]
    NotFound(String),
    #[error("coding-agent store is corrupt: {0}")]
    Corrupt(String),
    #[error("coding-agent storage initialization failed: {0}")]
    Initialization(String),
    #[error("coding-agent I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("coding-agent SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("coding-agent record serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("invalid run snapshot: {0}")]
    Model(#[from] RunModelError),
    #[error("invalid repository snapshot: {0}")]
    Repository(#[from] RepositoryError),
    #[error("invalid agent profile snapshot: {0}")]
    Profile(#[from] ProfileError),
}

/// Dedicated durable store for immutable AgentRun snapshots, state revisions,
/// operation intents, and references to evidence stored outside the VM.
pub struct RunStore {
    connection: Mutex<Connection>,
    evidence_root: PathBuf,
    store_identity: Arc<()>,
}

impl RunStore {
    pub fn open(paths: &DataPaths) -> Result<Self, CodingAgentError> {
        paths
            .ensure_coding_agent_runs()
            .map_err(|error| CodingAgentError::Initialization(error.to_string()))?;
        let evidence_root = paths.coding_agent_runs_dir();
        ensure_private_directory(&evidence_root)?;
        #[cfg(unix)]
        if let Some(parent) = evidence_root.parent() {
            sync_directory(parent)?;
        }

        let database_path = paths.coding_agent_runs_db_path();
        reject_symlink(&paths.db_dir())?;
        reject_symlink(&database_path)?;
        let connection = Connection::open(&database_path)?;
        connection.busy_timeout(Duration::from_secs(10))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;",
        )?;
        set_private_file(&database_path)?;
        initialize_schema(&connection)?;

        Ok(Self {
            connection: Mutex::new(connection),
            evidence_root,
            store_identity: Arc::new(()),
        })
    }

    /// Create a run exactly once. The snapshot is validated and normalized
    /// before storage; a missing/invalid auto-DONE approval is persisted false.
    pub fn create_run(
        &self,
        snapshot: AgentRunSnapshot,
    ) -> Result<AgentRunRecord, CodingAgentError> {
        self.create_run_with_assessment_contract(snapshot, None)
    }

    /// Persist the run and its approval-time assessment contract in one snapshot.
    /// The legacy constructor intentionally stores no contract rather than defaults.
    pub fn create_run_with_approval_contract(
        &self,
        snapshot: AgentRunSnapshot,
        contract: ApprovalAssessmentContract,
    ) -> Result<AgentRunRecord, CodingAgentError> {
        self.create_run_with_assessment_contract(snapshot, Some(contract))
    }

    fn create_run_with_assessment_contract(
        &self,
        snapshot: AgentRunSnapshot,
        contract: Option<ApprovalAssessmentContract>,
    ) -> Result<AgentRunRecord, CodingAgentError> {
        let snapshot = snapshot.normalize_for_storage()?;
        let stored_snapshot = StoredAgentRunSnapshot::new(snapshot, contract)?;
        let snapshot_json = serde_json::to_string(&stored_snapshot)?;
        let created_at = now_ms();
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = RunState::ApprovalPending;
        transaction
            .execute(
                "INSERT INTO agent_runs
                    (run_id, snapshot_json, revision, state_json, stage_json, created_at_ms)
                 VALUES (?1, ?2, 0, ?3, ?4, ?5)",
                params![
                    stored_snapshot.snapshot.run_id.as_str(),
                    snapshot_json,
                    serde_json::to_string(&state)?,
                    serde_json::to_string(&state.stage())?,
                    sql_i64(created_at, "creation time")?,
                ],
            )
            .map_err(|error| match &error {
                rusqlite::Error::SqliteFailure(code, _)
                    if code.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    CodingAgentError::Conflict(format!(
                        "run {} already exists",
                        stored_snapshot.snapshot.run_id
                    ))
                }
                _ => CodingAgentError::Sqlite(error),
            })?;
        for (ordinal, attempt_id) in stored_snapshot.snapshot.attempt_ids.iter().enumerate() {
            transaction.execute(
                "INSERT INTO agent_run_attempts (run_id, attempt_id, ordinal, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    stored_snapshot.snapshot.run_id.as_str(),
                    attempt_id,
                    sql_i64(ordinal as u64, "attempt ordinal")?,
                    sql_i64(created_at, "creation time")?,
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO agent_run_transitions (run_id, revision, from_state_json, to_state_json, at_ms)
             VALUES (?1, 0, NULL, ?2, ?3)",
            params![
                stored_snapshot.snapshot.run_id.as_str(),
                serde_json::to_string(&state)?,
                sql_i64(created_at, "creation time")?,
            ],
        )?;
        transaction.commit()?;
        drop(connection);
        self.get_run(&stored_snapshot.snapshot.run_id)?
            .ok_or_else(|| CodingAgentError::Corrupt("created run disappeared".into()))
    }

    pub fn create_delegation_plan(
        &self,
        request_key: &str,
        request: DelegationPlanRequest,
        issue_revision: u64,
        settings_revision: u64,
    ) -> Result<DelegationPlanRecord, CodingAgentError> {
        request.validate()?;
        if !super::delegation::valid_identifier(request_key, 512) || issue_revision == 0 {
            return Err(CodingAgentError::Invalid(
                "invalid delegation identity or revision".into(),
            ));
        }
        let request_json = serde_json::to_string(&request)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let query = format!(
            "SELECT {DELEGATION_COLUMNS} FROM agent_delegation_plans WHERE request_key = ?1"
        );
        if let Some(existing) = transaction
            .query_row(&query, [request_key], delegation_from_row)
            .optional()?
        {
            if existing.request != request {
                return Err(CodingAgentError::Conflict(
                    "delegation request key has a different payload".into(),
                ));
            }
            transaction.commit()?;
            return Ok(existing);
        }
        let plan = DelegationPlanRecord {
            delegation_plan_id: RunId::generate()?.to_string(),
            request,
            issue_revision,
            settings_revision,
            created_at_ms: now_ms(),
            run_id: None,
        };
        transaction.execute(
            "INSERT INTO agent_delegation_plans (plan_id, request_key, issue_id, request_json,
                issue_revision, settings_revision, created_at_ms, run_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![plan.delegation_plan_id, request_key, plan.request.issue_id, request_json,
                sql_i64(issue_revision, "Issue revision")?, sql_i64(settings_revision, "settings revision")?,
                sql_i64(plan.created_at_ms, "creation timestamp")?],
        )?;
        transaction.commit()?;
        Ok(plan)
    }

    pub fn delegation_plans(
        &self,
        issue_id: &str,
    ) -> Result<Vec<DelegationPlanRecord>, CodingAgentError> {
        if !super::delegation::valid_identifier(issue_id, 512) {
            return Err(CodingAgentError::Invalid("invalid Issue identity".into()));
        }
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(&format!(
            "SELECT {DELEGATION_COLUMNS} FROM agent_delegation_plans WHERE issue_id = ?1 AND run_id IS NULL ORDER BY created_at_ms ASC, plan_id ASC LIMIT 100"))?;
        let records = statement
            .query_map([issue_id], delegation_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(records)
    }

    pub fn delegation_plan(
        &self,
        plan_id: &str,
        issue_id: &str,
    ) -> Result<Option<DelegationPlanRecord>, CodingAgentError> {
        let connection = self.lock_connection()?;
        connection.query_row(&format!(
            "SELECT {DELEGATION_COLUMNS} FROM agent_delegation_plans WHERE plan_id = ?1 AND issue_id = ?2"),
            params![plan_id, issue_id], delegation_from_row).optional().map_err(Into::into)
    }

    pub fn link_delegation_plan(
        &self,
        plan_id: &str,
        run_id: &RunId,
    ) -> Result<DelegationPlanRecord, CodingAgentError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut plan = transaction
            .query_row(
                &format!(
                    "SELECT {DELEGATION_COLUMNS} FROM agent_delegation_plans WHERE plan_id = ?1"
                ),
                [plan_id],
                delegation_from_row,
            )
            .optional()?
            .ok_or_else(|| CodingAgentError::NotFound("delegation plan".into()))?;
        let run = load_run(&transaction, run_id)?
            .ok_or_else(|| CodingAgentError::NotFound("delegation run".into()))?;
        if run.snapshot.issue_id != plan.request.issue_id
            || run.snapshot.profile.profile_id != plan.request.profile_id
            || run.snapshot.purpose != plan.request.purpose
            || run.snapshot.approval.is_none()
            || (plan.run_id.is_none() && run.state != RunState::Queued)
            || plan.run_id.as_ref().is_some_and(|linked| linked != run_id)
        {
            return Err(CodingAgentError::Conflict(
                "delegation requires a matching, explicitly approved run".into(),
            ));
        }
        if plan.run_id.is_none() {
            transaction.execute("UPDATE agent_delegation_plans SET run_id = ?1 WHERE plan_id = ?2 AND run_id IS NULL",
                params![run_id.as_str(), plan_id])?;
            plan.run_id = Some(run_id.clone());
        }
        transaction.commit()?;
        Ok(plan)
    }

    pub fn get_run(&self, run_id: &RunId) -> Result<Option<AgentRunRecord>, CodingAgentError> {
        let connection = self.lock_connection()?;
        load_run(&connection, run_id)
    }

    /// Read the immutable approval contract. Older snapshots remain readable but
    /// cannot be interpreted as if a user had approved missing policy fields.
    pub fn approval_assessment_contract(
        &self,
        run_id: &RunId,
    ) -> Result<ApprovalAssessmentContractStatus, CodingAgentError> {
        let connection = self.lock_connection()?;
        let snapshot_json: String = connection
            .query_row(
                "SELECT snapshot_json FROM agent_runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
        let stored = decode_stored_snapshot(&snapshot_json)?;
        if stored.snapshot.run_id != *run_id {
            return Err(CodingAgentError::Corrupt(
                "stored run snapshot identity does not match its database key".into(),
            ));
        }
        stored
            .contract_status()
            .map_err(|error| CodingAgentError::Corrupt(error.to_string()))
    }

    /// Return the immutable judge intent for the named attempt, if one exists.
    pub fn assessment_intent(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<AssessmentIntentBinding, CodingAgentError> {
        validate_safe_id(attempt_id, "assessment attempt id")?;
        let connection = self.lock_connection()?;
        let binding = load_assessment_intent(&connection, self, run_id, attempt_id)?
            .ok_or_else(|| CodingAgentError::NotFound("assessment intent".into()))?;
        let snapshot_json: String = connection.query_row(
            "SELECT snapshot_json FROM agent_runs WHERE run_id = ?1",
            [run_id.as_str()],
            |row| row.get(0),
        )?;
        let stored_snapshot = decode_stored_snapshot(&snapshot_json)?;
        let stored_contract_digest = stored_snapshot
            .contract_status()
            .map_err(|error| CodingAgentError::Corrupt(error.to_string()))?;
        let snapshot_matches = match stored_contract_digest {
            ApprovalAssessmentContractStatus::Versioned { contract, digest } => {
                validate_assessment_intent_scope(&binding, &contract)?;
                digest == binding.approval_contract_digest
                    && format!("{:x}", Sha256::digest(snapshot_json.as_bytes()))
                        == binding.approval_snapshot_digest
                    && binding.validation_receipts.iter().all(|receipt| {
                        contract.validation_plan.iter().any(|validation| {
                            validation.validation_id == receipt.validation_id
                                && validation.required == receipt.required
                                && validation.command.as_ref().is_some_and(|command| {
                                    command.profile_digest == receipt.permission_profile_digest
                                })
                        })
                    })
            }
            ApprovalAssessmentContractStatus::LegacyIncomplete => false,
        };
        if !snapshot_matches {
            return Err(CodingAgentError::Corrupt(
                "assessment intent no longer matches its immutable approval snapshot".into(),
            ));
        }
        Ok(binding)
    }

    /// Validate durable report/input references, then CAS an immutable assessment
    /// intent to the current attempt before the caller contacts a judge provider.
    pub fn bind_assessment_intent(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        attempt_id: &str,
        request: AssessmentIntentRequest,
    ) -> Result<AssessmentIntentClaim, CodingAgentError> {
        validate_safe_id(attempt_id, "assessment attempt id")?;
        validate_safe_id(&request.assessment_id, "assessment id")?;
        validate_safe_id(&request.judge_input_evidence_id, "judge input evidence id")?;
        validate_safe_id(&request.report_evidence_id, "report evidence id")?;
        validate_safe_id(
            &request.snapshot_manifest_evidence_id,
            "snapshot manifest evidence id",
        )?;
        if !valid_commit_id(&request.target_full_commit_id)
            || !valid_commit_id(&request.target_full_tree_id)
            || request.target_full_commit_id.len() != request.target_full_tree_id.len()
            || !valid_sha256(&request.target_untracked_manifest_digest)
            || !valid_sha256(&request.target_snapshot_digest)
            || !valid_sha256(&request.system_prompt_digest)
        {
            return Err(CodingAgentError::Invalid(
                "assessment target must bind full commit, tree, untracked manifest, and snapshot digests"
                    .into(),
            ));
        }
        let target_snapshot = super::validation::ValidationTargetSnapshot::new(
            request.target_full_commit_id.clone(),
            request.target_full_tree_id.clone(),
            request.target_untracked_manifest_digest.clone(),
        )
        .map_err(|_| {
            CodingAgentError::Invalid("assessment target snapshot identity is invalid".into())
        })?;
        if target_snapshot.digest() != request.target_snapshot_digest {
            return Err(CodingAgentError::Invalid(
                "assessment target snapshot digest does not match its commit/tree manifest".into(),
            ));
        }

        let snapshot_json = self.snapshot_json(run_id)?;
        let stored_snapshot = decode_stored_snapshot(&snapshot_json)?;
        if stored_snapshot.snapshot.run_id != *run_id {
            return Err(CodingAgentError::Corrupt(
                "stored assessment snapshot has a different run identity".into(),
            ));
        }
        let contract_status = stored_snapshot
            .contract_status()
            .map_err(|error| CodingAgentError::Corrupt(error.to_string()))?;
        let (contract, contract_digest) = match contract_status {
            ApprovalAssessmentContractStatus::LegacyIncomplete => {
                return Err(CodingAgentError::Conflict(
                    "legacy run has no complete assessment approval contract".into(),
                ));
            }
            ApprovalAssessmentContractStatus::Versioned { contract, digest } => (contract, digest),
        };
        if !contract.can_dispatch_semantic_assessment() {
            return Err(CodingAgentError::Conflict(
                "approved assessment policy is unknown or has no dispatchable semantic criteria"
                    .into(),
            ));
        }
        let validation_receipts =
            assessment_validation_receipt_references(&contract, run_id, attempt_id, &request)?;

        let (input_reference, input_bytes) = self.read_bounded_attempt_evidence(
            run_id,
            attempt_id,
            &request.judge_input_evidence_id,
            contract.input_scope.max_input_bytes,
        )?;
        if input_bytes.is_empty() {
            return Err(CodingAgentError::Invalid(
                "exact judge input payload must not be empty".into(),
            ));
        }
        if input_reference.media_type != "application/json" {
            return Err(CodingAgentError::Invalid(
                "judge input evidence must be stored as exact JSON bytes".into(),
            ));
        }
        let (report_reference, report_bytes) = self.read_bounded_attempt_evidence(
            run_id,
            attempt_id,
            &request.report_evidence_id,
            super::report::MAX_REPORT_BYTES,
        )?;
        let approved_report_limit = contract
            .input_scope
            .allowed_sources
            .iter()
            .find(|source| source.kind == super::assessment::EvidenceSourceKind::Report)
            .map(|source| source.max_bytes)
            .ok_or_else(|| {
                CodingAgentError::Conflict("approved judge scope has no report source".into())
            })?;
        if report_reference.media_type != "application/json"
            || report_bytes.len() > approved_report_limit
        {
            return Err(CodingAgentError::Invalid(
                "report is not within the approved judge source bounds".into(),
            ));
        }
        let (manifest_reference, _manifest_bytes) = self.read_bounded_attempt_evidence(
            run_id,
            attempt_id,
            &request.snapshot_manifest_evidence_id,
            super::report::MAX_REPORT_BYTES,
        )?;
        validate_report_bytes(&contract, run_id, attempt_id, &report_bytes)?;

        let candidate = AssessmentIntentBinding {
            run_id: run_id.clone(),
            attempt_id: attempt_id.into(),
            assessment_id: request.assessment_id.clone(),
            expected_run_revision: expected_revision,
            approval_snapshot_digest: format!("{:x}", Sha256::digest(snapshot_json.as_bytes())),
            approval_contract_digest: contract_digest,
            judge_input_scope_digest: contract.judge_input_scope_digest().into(),
            system_prompt_digest: request.system_prompt_digest.clone(),
            judge_input_evidence_id: input_reference.evidence_id.clone(),
            judge_input_digest: input_reference.sha256.clone(),
            report_evidence_id: report_reference.evidence_id.clone(),
            report_digest: report_reference.sha256.clone(),
            snapshot_manifest_evidence_id: manifest_reference.evidence_id.clone(),
            snapshot_manifest_digest: manifest_reference.sha256.clone(),
            target_full_commit_id: request.target_full_commit_id,
            target_full_tree_id: request.target_full_tree_id,
            target_untracked_manifest_digest: request.target_untracked_manifest_digest,
            target_snapshot_digest: request.target_snapshot_digest,
            validation_receipts,
            created_at_ms: 0,
        };

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_assessment_intent(&transaction, self, run_id, attempt_id)? {
            validate_assessment_intent_scope(&existing, &contract)?;
            let mut exact_retry = candidate.clone();
            exact_retry.created_at_ms = existing.created_at_ms;
            if existing == exact_retry {
                transaction.commit()?;
                return Ok(AssessmentIntentClaim {
                    binding: existing,
                    newly_created: false,
                });
            }
            return Err(CodingAgentError::Conflict(
                "attempt already has a different immutable assessment intent".into(),
            ));
        }

        let (current_snapshot_json, revision, state_json, current_attempt_id): (
            String,
            i64,
            String,
            Option<String>,
        ) = transaction
            .query_row(
                "SELECT snapshot_json, revision, state_json, current_attempt_id
                 FROM agent_runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
        let revision = nonnegative_u64(revision, "run revision")?;
        let state: RunState = decode(&state_json, "run state")?;
        let current_snapshot = decode_stored_snapshot(&current_snapshot_json)?;
        if revision != expected_revision
            || state != RunState::Verifying
            || current_attempt_id.as_deref() != Some(attempt_id)
            || current_snapshot_json != snapshot_json
        {
            return Err(CodingAgentError::Conflict(
                "assessment intent CAS does not match the current run snapshot, revision, state, and attempt"
                    .into(),
            ));
        }
        if current_snapshot
            .approval_assessment_contract
            .as_ref()
            .and_then(|stored| stored.digest().ok())
            .as_deref()
            != Some(candidate.approval_contract_digest.as_str())
        {
            return Err(CodingAgentError::Conflict(
                "assessment approval contract changed before intent binding".into(),
            ));
        }
        ensure_no_attempt_cancellation_effect(&transaction, run_id, attempt_id)?;
        let execution: Option<String> = transaction
            .query_row(
                "SELECT result_json FROM agent_run_execution_evidence
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), attempt_id],
                |row| row.get(0),
            )
            .optional()?;
        let execution = execution.ok_or_else(|| {
            CodingAgentError::Conflict("attempt has no final execution evidence".into())
        })?;
        let execution: RunEvidenceAssessment = decode(&execution, "execution evidence")?;
        if execution.report_reference.as_deref() != Some(request.report_evidence_id.as_str())
            || !successful_report_is_durable(&execution)
        {
            return Err(CodingAgentError::Conflict(
                "assessment report is not the current attempt's successful durable report".into(),
            ));
        }
        recheck_assessment_evidence_reference(&transaction, run_id, attempt_id, &input_reference)?;
        recheck_assessment_evidence_reference(&transaction, run_id, attempt_id, &report_reference)?;
        recheck_assessment_evidence_reference(
            &transaction,
            run_id,
            attempt_id,
            &manifest_reference,
        )?;

        let mut binding = candidate;
        binding.created_at_ms = now_ms();
        transaction.execute(
            "INSERT INTO agent_run_assessment_intents
                (run_id, attempt_id, assessment_id, record_json, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                run_id.as_str(),
                attempt_id,
                binding.assessment_id,
                serde_json::to_string(&binding)?,
                sql_i64(binding.created_at_ms, "assessment intent timestamp")?,
            ],
        )?;
        transaction.commit()?;
        Ok(AssessmentIntentClaim {
            binding,
            newly_created: true,
        })
    }

    /// Read current run evidence only after revalidating its immutable files and
    /// the action-specific completion gates inside a SQLite transaction.
    pub(crate) fn completion_candidate(
        &self,
        run_id: &RunId,
        action: CompletionActionKind,
    ) -> Result<CompletionCandidate, CodingAgentError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidate =
            self.completion_candidate_in_transaction(&transaction, run_id, action, false)?;
        transaction.commit()?;
        Ok(candidate)
    }

    /// Revalidate a completed authorization against local evidence without
    /// relaxing normal preparation gates for any other state.
    pub(crate) fn completed_completion_candidate(
        &self,
        authorization_id: &str,
    ) -> Result<CompletionCandidate, CodingAgentError> {
        validate_safe_id(authorization_id, "completion authorization id")?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authorization = load_completion_authorization(&transaction, authorization_id)?
            .ok_or_else(|| CodingAgentError::NotFound("completion authorization".into()))?;
        if authorization.status != CompletionAuthorizationStatus::Completed {
            return Err(CodingAgentError::Conflict(
                "completion authorization has no completed receipt".into(),
            ));
        }
        let candidate = self.completion_candidate_in_transaction(
            &transaction,
            &authorization.run_id,
            authorization.action,
            true,
        )?;
        let valid_local_receipt = if authorization.action == CompletionActionKind::HumanCorrection {
            candidate.run.state == RunState::AwaitingReview
                && candidate.run.revision == authorization.run_revision
        } else {
            candidate.run.state == RunState::Done
                && authorization.run_revision.checked_add(1) == Some(candidate.run.revision)
        };
        if !valid_local_receipt {
            return Err(CodingAgentError::Conflict(
                "local run state no longer matches its completion receipt".into(),
            ));
        }
        transaction.commit()?;
        Ok(candidate)
    }

    pub(crate) fn completion_authorization(
        &self,
        authorization_id: &str,
    ) -> Result<Option<RunCompletionAuthorization>, CodingAgentError> {
        validate_safe_id(authorization_id, "completion authorization id")?;
        let connection = self.lock_connection()?;
        connection
            .query_row(
                "SELECT record_json FROM agent_run_completion_authorizations
                 WHERE authorization_id = ?1",
                [authorization_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|json| decode(&json, "completion authorization"))
            .transpose()
    }

    pub(crate) fn completion_authorization_for_intent(
        &self,
        run_id: &RunId,
        idempotency_key: &str,
    ) -> Result<Option<RunCompletionAuthorization>, CodingAgentError> {
        validate_idempotency_key(idempotency_key)?;
        let connection = self.lock_connection()?;
        connection
            .query_row(
                "SELECT record_json FROM agent_run_completion_authorizations
                 WHERE run_id = ?1 AND intent_idempotency_key = ?2",
                params![run_id.as_str(), idempotency_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|json| decode(&json, "completion authorization"))
            .transpose()
    }

    /// Persist one immutable run-specific authorization before the tracker RPC.
    /// A duplicate key can only recover the byte-for-byte same decision.
    pub(crate) fn prepare_completion_authorization(
        &self,
        candidate: &CompletionCandidate,
        request: NewCompletionAuthorization,
    ) -> Result<RunCompletionAuthorization, CodingAgentError> {
        validate_safe_id(&request.actor_id, "completion actor id")?;
        validate_bounded_text(&request.policy_identity, 256, "completion policy identity")?;
        validate_bounded_text(&request.audit_reason, 2_048, "completion audit reason")?;
        if !valid_sha256(&request.report_sha256) {
            return Err(CodingAgentError::Invalid(
                "completion report digest must be a SHA-256 value".into(),
            ));
        }
        validate_idempotency_key(&request.intent_idempotency_key)?;
        if request.policy_version == 0
            || request.target_issue_revision == u64::MAX
            || request.marker_revision <= candidate.run.snapshot.issue_revision
            || request.marker_revision > request.target_issue_revision
            || candidate.run.snapshot.issue_id.is_empty()
        {
            return Err(CodingAgentError::Invalid(
                "completion authorization revision or policy identity is invalid".into(),
            ));
        }
        validate_bounded_text(
            &request.primary_tracker_identity.schema_fingerprint,
            256,
            "primary tracker schema fingerprint",
        )?;
        validate_bounded_text(
            &request.primary_tracker_identity.data_root_id,
            256,
            "primary tracker data-root identity",
        )?;
        validate_bounded_text(
            &request.primary_tracker_identity.contract_fingerprint,
            256,
            "primary tracker contract fingerprint",
        )?;

        let run_id = &candidate.run.snapshot.run_id;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let latest =
            self.completion_candidate_in_transaction(&transaction, run_id, request.action, false)?;
        if !completion_candidates_match(candidate, &latest) {
            return Err(CodingAgentError::Conflict(
                "run evidence changed while preparing completion authorization".into(),
            ));
        }
        if matches!(
            request.action,
            CompletionActionKind::HumanDone | CompletionActionKind::AutoDone
        ) && latest
            .human_semantic_issue_revision
            .is_some_and(|revision| revision != request.target_issue_revision)
        {
            return Err(CodingAgentError::Conflict(
                "Issue changed after the human semantic reassessment".into(),
            ));
        }

        if let Some(existing) = load_completion_authorization_for_intent(
            &transaction,
            run_id,
            &request.intent_idempotency_key,
        )? {
            if !completion_authorization_matches_request(&existing, candidate, &request) {
                return Err(CodingAgentError::Conflict(
                    "completion idempotency key was already used with a different decision".into(),
                ));
            }
            if existing.status == CompletionAuthorizationStatus::Revoked {
                return Err(CodingAgentError::Conflict(
                    "completion authorization has been revoked".into(),
                ));
            }
            return Ok(existing);
        }

        if request.action == CompletionActionKind::AutoDone {
            let existing_auto: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_run_completion_authorizations
                 WHERE run_id = ?1 AND action_json = ?2)",
                params![
                    run_id.as_str(),
                    serde_json::to_string(&CompletionActionKind::AutoDone)?
                ],
                |row| row.get(0),
            )?;
            if existing_auto {
                return Err(CodingAgentError::Conflict(
                    "this run already has an auto-completion authorization".into(),
                ));
            }
        } else {
            revoke_prepared_auto_authorizations(&transaction, run_id)?;
        }

        let now = now_ms();
        let mut authorization = RunCompletionAuthorization {
            authorization_id: RunId::generate()?.to_string(),
            run_id: run_id.clone(),
            issue_id: latest.run.snapshot.issue_id.clone(),
            attempt_id: latest.run.current_attempt_id.clone().ok_or_else(|| {
                CodingAgentError::Conflict("completion authorization has no current attempt".into())
            })?,
            action: request.action,
            actor_id: request.actor_id,
            policy_identity: request.policy_identity,
            policy_version: request.policy_version,
            snapshot_sha256: latest.snapshot_sha256,
            approved_settings_version: latest
                .run
                .snapshot
                .approval
                .as_ref()
                .and_then(|approval| approval.settings_version),
            execution_mode: latest.run.snapshot.execution_mode,
            commit_id: latest.commit_id,
            evidence_sha256: latest.evidence_sha256,
            report_sha256: request.report_sha256,
            intent_sha256: String::new(),
            expected_manager_request_digest: String::new(),
            completion_report_protobuf: request.completion_report_protobuf,
            audit_reason: request.audit_reason,
            marker_revision: request.marker_revision,
            target_issue_revision: request.target_issue_revision,
            run_revision: latest.run.revision,
            intent_idempotency_key: request.intent_idempotency_key,
            primary_tracker_identity: Some(request.primary_tracker_identity),
            primary_operation: None,
            dispatch_attempts: 0,
            status: CompletionAuthorizationStatus::Prepared,
            resulting_issue_revision: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        authorization.intent_sha256 = completion_authorization_intent_sha256(&authorization)?;
        authorization.audit_reason = format!(
            "{} [coding-agent-intent-sha256={}]",
            authorization.audit_reason, authorization.intent_sha256
        );
        authorization.expected_manager_request_digest =
            super::completion::manager_request_digest_for_authorization(&authorization)?;
        validate_bounded_text(
            &authorization.audit_reason,
            2_048,
            "completion audit reason",
        )?;
        let result = transaction.execute(
            "INSERT INTO agent_run_completion_authorizations
                (authorization_id, run_id, intent_idempotency_key, action_json,
                 status_json, record_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                authorization.authorization_id,
                run_id.as_str(),
                authorization.intent_idempotency_key,
                serde_json::to_string(&authorization.action)?,
                serde_json::to_string(&authorization.status)?,
                serde_json::to_string(&authorization)?,
            ],
        );
        result.map_err(|error| match &error {
            rusqlite::Error::SqliteFailure(code, _)
                if code.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                CodingAgentError::Conflict(
                    "run already has a conflicting completion authorization".into(),
                )
            }
            _ => CodingAgentError::Sqlite(error),
        })?;
        transaction.commit()?;
        Ok(authorization)
    }

    /// Atomically mark the intent as dispatched before crossing the tracker
    /// boundary. A later recovery call may claim one exact-request replay.
    pub(crate) fn mark_completion_dispatched(
        &self,
        authorization_id: &str,
    ) -> Result<bool, CodingAgentError> {
        validate_safe_id(authorization_id, "completion authorization id")?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut authorization = load_completion_authorization(&transaction, authorization_id)?
            .ok_or_else(|| CodingAgentError::NotFound("completion authorization".into()))?;
        if authorization.status != CompletionAuthorizationStatus::Prepared {
            return Ok(false);
        }
        if authorization.primary_tracker_identity.is_none()
            || !completion_authorization_intent_matches(&authorization)
        {
            return Err(CodingAgentError::Conflict(
                "completion authorization has no bound primary tracker identity".into(),
            ));
        }
        let candidate = self.completion_candidate_in_transaction(
            &transaction,
            &authorization.run_id,
            authorization.action,
            false,
        )?;
        if !completion_authorization_matches_candidate(&authorization, &candidate) {
            return Err(CodingAgentError::Conflict(
                "run no longer matches its immutable completion authorization".into(),
            ));
        }
        ensure_no_other_dispatched_completion(
            &transaction,
            &authorization.run_id,
            authorization_id,
        )?;
        authorization.status = CompletionAuthorizationStatus::Dispatched;
        authorization.dispatch_attempts = 1;
        authorization.updated_at_ms = now_ms();
        transaction.execute(
            "UPDATE agent_run_completion_authorizations
             SET action_json = ?1, status_json = ?2, record_json = ?3
             WHERE authorization_id = ?4 AND status_json = ?5",
            params![
                serde_json::to_string(&authorization.action)?,
                serde_json::to_string(&authorization.status)?,
                serde_json::to_string(&authorization)?,
                authorization_id,
                serde_json::to_string(&CompletionAuthorizationStatus::Prepared)?,
            ],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    /// Store the actual manager and downstream IDs returned by the primary
    /// tracker. The same exact receipt is idempotent; it cannot be replaced.
    pub(crate) fn record_primary_operation_receipt(
        &self,
        authorization_id: &str,
        receipt: PrimaryOperationReceipt,
    ) -> Result<RunCompletionAuthorization, CodingAgentError> {
        validate_safe_id(authorization_id, "completion authorization id")?;
        validate_primary_operation_receipt(&receipt)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut authorization = load_completion_authorization(&transaction, authorization_id)?
            .ok_or_else(|| CodingAgentError::NotFound("completion authorization".into()))?;
        if authorization.status != CompletionAuthorizationStatus::Dispatched
            || authorization.primary_tracker_identity.as_ref() != Some(&receipt.server_identity)
            || !completion_authorization_intent_matches(&authorization)
            || receipt.manager_idempotency_key
                != completion_manager_idempotency_key(
                    &authorization.run_id,
                    &authorization.intent_idempotency_key,
                )
            || receipt.idempotency_scope
                != super::completion::completion_idempotency_scope(
                    &receipt.server_identity,
                    &authorization.issue_id,
                    &receipt.manager_idempotency_key,
                )
            || receipt.request_digest != authorization.expected_manager_request_digest
            || receipt.request_schema != super::completion::MANAGER_DURABLE_BACKEND_REQUEST_SCHEMA
        {
            return Err(CodingAgentError::Conflict(
                "primary operation receipt does not match the dispatched authorization".into(),
            ));
        }
        if let Some(existing) = &authorization.primary_operation {
            if existing == &receipt {
                return Ok(authorization);
            }
            return Err(CodingAgentError::Conflict(
                "primary operation identity cannot be replaced".into(),
            ));
        }
        authorization.primary_operation = Some(receipt);
        authorization.updated_at_ms = now_ms();
        transaction.execute(
            "UPDATE agent_run_completion_authorizations
             SET record_json = ?1 WHERE authorization_id = ?2 AND status_json = ?3",
            params![
                serde_json::to_string(&authorization)?,
                authorization_id,
                serde_json::to_string(&CompletionAuthorizationStatus::Dispatched)?,
            ],
        )?;
        transaction.commit()?;
        Ok(authorization)
    }

    /// Claim the one permitted same-payload recovery RPC. The key, CAS revision,
    /// audit context, and request digest remain frozen in this authorization.
    pub(crate) fn claim_completion_retry(
        &self,
        authorization_id: &str,
    ) -> Result<bool, CodingAgentError> {
        validate_safe_id(authorization_id, "completion authorization id")?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut authorization = load_completion_authorization(&transaction, authorization_id)?
            .ok_or_else(|| CodingAgentError::NotFound("completion authorization".into()))?;
        if authorization.status != CompletionAuthorizationStatus::Dispatched
            || authorization.dispatch_attempts != 1
            || authorization.primary_operation.is_some()
            || !completion_authorization_intent_matches(&authorization)
        {
            return Ok(false);
        }
        let candidate = self.completion_candidate_in_transaction(
            &transaction,
            &authorization.run_id,
            authorization.action,
            false,
        )?;
        if !completion_authorization_matches_candidate(&authorization, &candidate) {
            return Err(CodingAgentError::Conflict(
                "run no longer matches its dispatched completion authorization".into(),
            ));
        }
        authorization.dispatch_attempts = 2;
        authorization.updated_at_ms = now_ms();
        transaction.execute(
            "UPDATE agent_run_completion_authorizations
             SET record_json = ?1 WHERE authorization_id = ?2 AND status_json = ?3",
            params![
                serde_json::to_string(&authorization)?,
                authorization_id,
                serde_json::to_string(&CompletionAuthorizationStatus::Dispatched)?,
            ],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    /// Release the in-flight proof fence only after the primary manager gives a
    /// definitive rejection for this exact immutable manager request.
    pub(crate) fn revoke_dispatched_completion(
        &self,
        authorization_id: &str,
        receipt: PrimaryOperationReceipt,
    ) -> Result<RunCompletionAuthorization, CodingAgentError> {
        validate_safe_id(authorization_id, "completion authorization id")?;
        validate_primary_operation_receipt(&receipt)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut authorization = load_completion_authorization(&transaction, authorization_id)?
            .ok_or_else(|| CodingAgentError::NotFound("completion authorization".into()))?;
        if authorization.status == CompletionAuthorizationStatus::Revoked {
            if authorization.primary_operation.as_ref() == Some(&receipt) {
                return Ok(authorization);
            }
            return Err(CodingAgentError::Conflict(
                "revoked completion operation identity cannot be replaced".into(),
            ));
        }
        if authorization.status != CompletionAuthorizationStatus::Dispatched
            || authorization.primary_tracker_identity.as_ref() != Some(&receipt.server_identity)
            || !completion_authorization_intent_matches(&authorization)
            || receipt.manager_idempotency_key
                != completion_manager_idempotency_key(
                    &authorization.run_id,
                    &authorization.intent_idempotency_key,
                )
            || receipt.idempotency_scope
                != super::completion::completion_idempotency_scope(
                    &receipt.server_identity,
                    &authorization.issue_id,
                    &receipt.manager_idempotency_key,
                )
            || receipt.request_digest != authorization.expected_manager_request_digest
            || receipt.request_schema != super::completion::MANAGER_DURABLE_BACKEND_REQUEST_SCHEMA
            || authorization
                .primary_operation
                .as_ref()
                .is_some_and(|existing| existing != &receipt)
        {
            return Err(CodingAgentError::Conflict(
                "only a definitively rejected dispatched completion can be revoked".into(),
            ));
        }
        authorization.primary_operation = Some(receipt);
        authorization.status = CompletionAuthorizationStatus::Revoked;
        authorization.updated_at_ms = now_ms();
        transaction.execute(
            "UPDATE agent_run_completion_authorizations
             SET status_json = ?1, record_json = ?2 WHERE authorization_id = ?3",
            params![
                serde_json::to_string(&authorization.status)?,
                serde_json::to_string(&authorization)?,
                authorization_id,
            ],
        )?;
        transaction.commit()?;
        Ok(authorization)
    }

    /// Record proof from the primary tracker and transition the local run to
    /// Done in the same transaction as the durable authorization receipt.
    pub(crate) fn complete_run_authorization(
        &self,
        authorization_id: &str,
        policy_identity: &str,
        policy_version: u64,
        resulting_issue_revision: u64,
    ) -> Result<AgentRunRecord, CodingAgentError> {
        validate_safe_id(authorization_id, "completion authorization id")?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut authorization = load_completion_authorization(&transaction, authorization_id)?
            .ok_or_else(|| CodingAgentError::NotFound("completion authorization".into()))?;
        if authorization.policy_identity != policy_identity
            || authorization.policy_version != policy_version
        {
            return Err(CodingAgentError::Conflict(
                "completion authorization belongs to a different trusted policy".into(),
            ));
        }
        if authorization.status == CompletionAuthorizationStatus::Completed {
            if authorization.resulting_issue_revision == Some(resulting_issue_revision) {
                let run = load_run(&transaction, &authorization.run_id)?
                    .ok_or_else(|| CodingAgentError::Corrupt("completed run disappeared".into()))?;
                return Ok(run);
            }
            return Err(CodingAgentError::Conflict(
                "completion authorization result cannot be replaced".into(),
            ));
        }
        let Some(receipt) = authorization.primary_operation.as_ref() else {
            return Err(CodingAgentError::Conflict(
                "tracker completion proof has no manager operation receipt".into(),
            ));
        };
        if authorization.status != CompletionAuthorizationStatus::Dispatched
            || !completion_authorization_intent_matches(&authorization)
            || authorization.primary_tracker_identity.as_ref() != Some(&receipt.server_identity)
            || receipt.manager_idempotency_key
                != completion_manager_idempotency_key(
                    &authorization.run_id,
                    &authorization.intent_idempotency_key,
                )
            || receipt.idempotency_scope
                != super::completion::completion_idempotency_scope(
                    &receipt.server_identity,
                    &authorization.issue_id,
                    &receipt.manager_idempotency_key,
                )
            || receipt.request_digest != authorization.expected_manager_request_digest
            || receipt.request_schema != super::completion::MANAGER_DURABLE_BACKEND_REQUEST_SCHEMA
            || resulting_issue_revision
                != authorization
                    .target_issue_revision
                    .checked_add(1)
                    .ok_or_else(|| CodingAgentError::Conflict("issue revision overflow".into()))?
        {
            return Err(CodingAgentError::Conflict(
                "tracker completion proof does not match the durable intent".into(),
            ));
        }
        let candidate = self.completion_candidate_in_transaction(
            &transaction,
            &authorization.run_id,
            authorization.action,
            false,
        )?;
        if candidate.run.revision != authorization.run_revision
            || candidate.run.snapshot.issue_id != authorization.issue_id
            || !completion_attempt_matches(&authorization, &candidate)
            || candidate.snapshot_sha256 != authorization.snapshot_sha256
            || candidate.evidence_sha256 != authorization.evidence_sha256
            || candidate.commit_id != authorization.commit_id
            || authorization.action == CompletionActionKind::HumanCorrection
        {
            return Err(CodingAgentError::Conflict(
                "run no longer matches its immutable completion authorization".into(),
            ));
        }
        if authorization.action == CompletionActionKind::AutoDone
            && candidate
                .run
                .snapshot
                .approval
                .as_ref()
                .and_then(|approval| approval.settings_version)
                != authorization.approved_settings_version
        {
            return Err(CodingAgentError::Conflict(
                "auto-completion approval version changed".into(),
            ));
        }
        let next_revision = authorization
            .run_revision
            .checked_add(1)
            .ok_or_else(|| CodingAgentError::Conflict("run revision overflow".into()))?;
        let updated = transaction.execute(
            "UPDATE agent_runs SET revision = ?1, state_json = ?2, stage_json = ?3
             WHERE run_id = ?4 AND revision = ?5 AND state_json = ?6",
            params![
                sql_i64(next_revision, "run revision")?,
                serde_json::to_string(&RunState::Done)?,
                serde_json::to_string(&RunState::Done.stage())?,
                authorization.run_id.as_str(),
                sql_i64(authorization.run_revision, "expected run revision")?,
                serde_json::to_string(&RunState::AwaitingReview)?,
            ],
        )?;
        if updated != 1 {
            return Err(CodingAgentError::Conflict(
                "run changed while recording tracker completion proof".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO agent_run_transitions
                (run_id, revision, from_state_json, to_state_json, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                authorization.run_id.as_str(),
                sql_i64(next_revision, "run revision")?,
                serde_json::to_string(&RunState::AwaitingReview)?,
                serde_json::to_string(&RunState::Done)?,
                sql_i64(now_ms(), "transition time")?,
            ],
        )?;
        authorization.status = CompletionAuthorizationStatus::Completed;
        authorization.resulting_issue_revision = Some(resulting_issue_revision);
        authorization.updated_at_ms = now_ms();
        transaction.execute(
            "UPDATE agent_run_completion_authorizations
             SET status_json = ?1, record_json = ?2 WHERE authorization_id = ?3",
            params![
                serde_json::to_string(&authorization.status)?,
                serde_json::to_string(&authorization)?,
                authorization_id,
            ],
        )?;
        transaction.commit()?;
        drop(connection);
        self.get_run(&authorization.run_id)?
            .ok_or_else(|| CodingAgentError::Corrupt("completed run disappeared".into()))
    }

    /// Persist a confirmed human correction without changing the local run or
    /// dispatching a replacement attempt. Its history and evidence stay intact.
    pub(crate) fn complete_human_correction_authorization(
        &self,
        authorization_id: &str,
        resulting_issue_revision: u64,
    ) -> Result<RunCompletionAuthorization, CodingAgentError> {
        validate_safe_id(authorization_id, "completion authorization id")?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut authorization = load_completion_authorization(&transaction, authorization_id)?
            .ok_or_else(|| CodingAgentError::NotFound("completion authorization".into()))?;
        if authorization.action != CompletionActionKind::HumanCorrection
            || authorization.status != CompletionAuthorizationStatus::Dispatched
            || authorization.primary_operation.is_none()
            || resulting_issue_revision
                != authorization
                    .target_issue_revision
                    .checked_add(1)
                    .ok_or_else(|| CodingAgentError::Conflict("issue revision overflow".into()))?
        {
            return Err(CodingAgentError::Conflict(
                "human correction proof does not match its durable intent".into(),
            ));
        }
        let run = load_run(&transaction, &authorization.run_id)?
            .ok_or_else(|| CodingAgentError::Corrupt("corrected run disappeared".into()))?;
        if run.revision != authorization.run_revision
            || run.snapshot.issue_id != authorization.issue_id
            || (!authorization.attempt_id.is_empty()
                && run.current_attempt_id.as_deref() != Some(authorization.attempt_id.as_str()))
            || run.state != RunState::AwaitingReview
        {
            return Err(CodingAgentError::Conflict(
                "run changed while recording the human correction".into(),
            ));
        }
        let candidate = self.completion_candidate_in_transaction(
            &transaction,
            &authorization.run_id,
            CompletionActionKind::HumanCorrection,
            false,
        )?;
        if candidate.snapshot_sha256 != authorization.snapshot_sha256
            || candidate.evidence_sha256 != authorization.evidence_sha256
            || candidate.commit_id != authorization.commit_id
        {
            return Err(CodingAgentError::Conflict(
                "run evidence changed while recording the human correction".into(),
            ));
        }
        authorization.status = CompletionAuthorizationStatus::Completed;
        authorization.resulting_issue_revision = Some(resulting_issue_revision);
        authorization.updated_at_ms = now_ms();
        transaction.execute(
            "UPDATE agent_run_completion_authorizations
             SET status_json = ?1, record_json = ?2 WHERE authorization_id = ?3",
            params![
                serde_json::to_string(&authorization.status)?,
                serde_json::to_string(&authorization)?,
                authorization_id,
            ],
        )?;
        transaction.commit()?;
        Ok(authorization)
    }

    /// Persist a stable StartAgent intent before dispatch and make its attempt
    /// the only attempt whose evidence may satisfy subsequent stage gates.
    /// Dispatch is authorized only when `newly_created` is true; an existing
    /// intent is a reconciliation case, not a replay instruction.
    pub fn start_attempt(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        attempt_id: &str,
        idempotency_key: &str,
    ) -> Result<StartAttemptResult, CodingAgentError> {
        validate_safe_id(attempt_id, "attempt id")?;
        validate_idempotency_key(idempotency_key)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_no_dispatched_completion(&transaction, run_id)?;
        let (snapshot_json, revision, state_json, phase_generation): (String, i64, String, i64) =
            transaction
                .query_row(
                    "SELECT snapshot_json, revision, state_json, start_phase_generation
                 FROM agent_runs WHERE run_id = ?1",
                    [run_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?
                .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
        let snapshot = decode_agent_run_snapshot(&snapshot_json)?;
        let revision = nonnegative_u64(revision, "run revision")?;
        let phase_generation = nonnegative_u64(phase_generation, "start phase generation")?;
        let request = DurableOperationRequest::StartAgent {
            workflow_definition_hash: snapshot.workflow.definition_hash.clone(),
            attempt_id: attempt_id.into(),
            execution_mode: snapshot.execution_mode,
        };
        validate_operation_request(&request)?;
        let payload_hash = request.payload_hash()?;

        self.ensure_attempt_in_transaction(&transaction, run_id, attempt_id)?;
        let existing = transaction
            .query_row(
                "SELECT operation_id, attempt_id, kind_json, payload_hash, request_json,
                        status_json, result_json, created_at_ms, updated_at_ms
                 FROM agent_run_operations WHERE run_id = ?1 AND idempotency_key = ?2",
                params![run_id.as_str(), idempotency_key],
                operation_from_row(run_id, idempotency_key),
            )
            .optional()?;
        if let Some(operation) = existing {
            if operation.payload_hash != payload_hash
                || operation.kind != super::model::DurableOperationKind::StartAgent
            {
                return Err(CodingAgentError::Conflict(
                    "idempotency key was already used with a different request".into(),
                ));
            }
            let claim: Option<(i64, i64)> = transaction
                .query_row(
                    "SELECT phase_generation, run_revision FROM agent_run_start_claims
                     WHERE run_id = ?1 AND idempotency_key = ?2",
                    params![run_id.as_str(), idempotency_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (phase_generation, run_revision) = match claim {
                Some((generation, claimed_revision)) => (
                    nonnegative_u64(generation, "start phase generation")?,
                    nonnegative_u64(claimed_revision, "start claim revision")?,
                ),
                // Legacy intents have no atomic claim. Returning their receipt is
                // safe only as a reconciliation result; `newly_created` is false.
                None => (phase_generation, revision),
            };
            return Ok(StartAttemptResult {
                operation,
                newly_created: false,
                run_revision,
                phase_generation,
            });
        }

        let state: RunState = decode(&state_json, "run state")?;
        if state != RunState::Preparing {
            return Err(CodingAgentError::Conflict(
                "StartAgent intent requires an available preparation phase".into(),
            ));
        }
        if revision != expected_revision {
            return Err(CodingAgentError::Conflict(format!(
                "run revision changed from expected {expected_revision} to {revision}"
            )));
        }
        let phase_claimed: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_run_start_claims
             WHERE run_id = ?1 AND phase_generation = ?2)",
            params![
                run_id.as_str(),
                sql_i64(phase_generation, "start phase generation")?
            ],
            |row| row.get(0),
        )?;
        if phase_claimed {
            return Err(CodingAgentError::Conflict(
                "this preparation phase already has a consumed StartAgent claim".into(),
            ));
        }
        let legacy_start_intent: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM agent_run_operations operation
                 WHERE operation.run_id = ?1
                   AND operation.kind_json = ?2
                   AND NOT EXISTS (
                       SELECT 1 FROM agent_run_start_claims claim
                       WHERE claim.run_id = operation.run_id
                         AND claim.idempotency_key = operation.idempotency_key
                   )
             )",
            params![
                run_id.as_str(),
                serde_json::to_string(&super::model::DurableOperationKind::StartAgent)?
            ],
            |row| row.get(0),
        )?;
        if legacy_start_intent {
            return Err(CodingAgentError::Conflict(
                "legacy StartAgent intent requires reconciliation before another attempt".into(),
            ));
        }
        self.validate_operation_for_run(&transaction, run_id, attempt_id, &request)?;

        let payload_json = serde_json::to_string(&request)?;
        let now = now_ms();
        let operation_id = RunId::generate()?.to_string();
        let operation = DurableOperationRecord {
            operation_id: operation_id.clone(),
            run_id: run_id.clone(),
            attempt_id: attempt_id.into(),
            idempotency_key: idempotency_key.into(),
            kind: request.kind(),
            payload_hash,
            request,
            status: DurableOperationStatus::Intent,
            result: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let run_revision = revision
            .checked_add(1)
            .ok_or_else(|| CodingAgentError::Conflict("run revision overflow".into()))?;
        transaction.execute(
            "INSERT INTO agent_run_operations
                (operation_id, run_id, attempt_id, phase_generation, idempotency_key, kind_json,
                 payload_hash, request_json, status_json, result_json, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?10)",
            params![
                operation_id,
                run_id.as_str(),
                attempt_id,
                sql_i64(phase_generation, "start phase generation")?,
                idempotency_key,
                serde_json::to_string(&operation.kind)?,
                operation.payload_hash,
                payload_json,
                serde_json::to_string(&DurableOperationStatus::Intent)?,
                sql_i64(now, "operation timestamp")?,
            ],
        )?;
        transaction.execute(
            "INSERT INTO agent_run_start_claims
                (run_id, phase_generation, attempt_id, idempotency_key, expected_revision, run_revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run_id.as_str(),
                sql_i64(phase_generation, "start phase generation")?,
                attempt_id,
                idempotency_key,
                sql_i64(expected_revision, "expected run revision")?,
                sql_i64(run_revision, "start claim revision")?,
            ],
        )?;
        let updated = transaction.execute(
            "UPDATE agent_runs
             SET revision = ?1, current_attempt_id = ?2
             WHERE run_id = ?3 AND revision = ?4 AND state_json = ?5
               AND start_phase_generation = ?6",
            params![
                sql_i64(run_revision, "run revision")?,
                attempt_id,
                run_id.as_str(),
                sql_i64(expected_revision, "expected run revision")?,
                serde_json::to_string(&RunState::Preparing)?,
                sql_i64(phase_generation, "start phase generation")?,
            ],
        )?;
        if updated != 1 {
            return Err(CodingAgentError::Conflict(
                "preparation phase changed while claiming StartAgent".into(),
            ));
        }
        transaction.commit()?;
        Ok(StartAttemptResult {
            operation,
            newly_created: true,
            run_revision,
            phase_generation,
        })
    }

    /// Durably request normal interactive completion and atomically advance the
    /// run into Finalizing. This is not a cancellation or exit-success signal.
    pub fn request_interactive_finish(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<InteractiveFinishRequest, CodingAgentError> {
        validate_safe_id(attempt_id, "attempt id")?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_no_dispatched_completion(&transaction, run_id)?;
        let (snapshot_json, revision, state_json, current_attempt_id, phase_generation): (
            String,
            i64,
            String,
            Option<String>,
            i64,
        ) = transaction
            .query_row(
                "SELECT snapshot_json, revision, state_json, current_attempt_id,
                        start_phase_generation
                 FROM agent_runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
        let snapshot = decode_agent_run_snapshot(&snapshot_json)?;
        let state: RunState = decode(&state_json, "run state")?;
        if snapshot.execution_mode != super::model::ExecutionMode::Interactive
            || current_attempt_id.as_deref() != Some(attempt_id)
        {
            return Err(CodingAgentError::Conflict(
                "finish request must target the current interactive attempt".into(),
            ));
        }
        self.ensure_attempt_in_transaction(&transaction, run_id, attempt_id)?;
        let phase_generation = nonnegative_u64(phase_generation, "start phase generation")?;
        if has_phase_cancellation_effect(&transaction, run_id, attempt_id, phase_generation)? {
            return Err(CodingAgentError::Conflict(
                "interactive finish is fenced by a cancellation claim in this phase".into(),
            ));
        }
        let existing: Option<(i64, i64)> = transaction
            .query_row(
                "SELECT profile_version, requested_at_ms
                 FROM agent_run_interactive_finish_requests
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), attempt_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((profile_version, requested_at_ms)) = existing {
            let request = InteractiveFinishRequest {
                run_id: run_id.clone(),
                attempt_id: attempt_id.into(),
                profile_version: nonnegative_u64(profile_version, "finish profile version")?,
                requested_at_ms: nonnegative_u64(requested_at_ms, "finish request timestamp")?,
            };
            if request.profile_version != snapshot.profile.version {
                return Err(CodingAgentError::Corrupt(
                    "finish request profile differs from the immutable snapshot".into(),
                ));
            }
            return Ok(request);
        }
        if state != RunState::Running {
            return Err(CodingAgentError::Conflict(
                "interactive finish can be requested only while running".into(),
            ));
        }
        let requested_at_ms = now_ms();
        transaction.execute(
            "INSERT INTO agent_run_interactive_finish_requests
                (run_id, attempt_id, profile_version, requested_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                run_id.as_str(),
                attempt_id,
                sql_i64(snapshot.profile.version, "profile version")?,
                sql_i64(requested_at_ms, "finish request timestamp")?,
            ],
        )?;
        let next_revision = nonnegative_u64(revision, "run revision")?
            .checked_add(1)
            .ok_or_else(|| CodingAgentError::Conflict("run revision overflow".into()))?;
        let updated = transaction.execute(
            "UPDATE agent_runs SET revision = ?1, state_json = ?2, stage_json = ?3
             WHERE run_id = ?4 AND revision = ?5",
            params![
                sql_i64(next_revision, "run revision")?,
                serde_json::to_string(&RunState::Finalizing)?,
                serde_json::to_string(&RunState::Finalizing.stage())?,
                run_id.as_str(),
                revision,
            ],
        )?;
        if updated != 1 {
            return Err(CodingAgentError::Conflict(
                "run revision changed while persisting interactive finish request".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO agent_run_transitions
                (run_id, revision, from_state_json, to_state_json, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                run_id.as_str(),
                sql_i64(next_revision, "run revision")?,
                serde_json::to_string(&state)?,
                serde_json::to_string(&RunState::Finalizing)?,
                sql_i64(now_ms(), "transition time")?,
            ],
        )?;
        transaction.commit()?;
        Ok(InteractiveFinishRequest {
            run_id: run_id.clone(),
            attempt_id: attempt_id.into(),
            profile_version: snapshot.profile.version,
            requested_at_ms,
        })
    }

    /// Store host observations bound to the persisted finish request and exact
    /// current attempt. The host adapter, never the guest, supplies this proof.
    pub fn record_interactive_finalization(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        evidence: InteractiveFinalizationEvidence,
    ) -> Result<(), CodingAgentError> {
        validate_safe_id(attempt_id, "attempt id")?;
        if let WorkspaceFreezeStatus::HostVerified { commit_id } = evidence.workspace_freeze()
            && !valid_commit_id(commit_id)
        {
            return Err(CodingAgentError::Invalid(
                "host-verified frozen commit must be a full object id".into(),
            ));
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_no_dispatched_completion(&transaction, run_id)?;
        let (snapshot_json, state_json, current_attempt_id): (String, String, Option<String>) =
            transaction
                .query_row(
                    "SELECT snapshot_json, state_json, current_attempt_id
                     FROM agent_runs WHERE run_id = ?1",
                    [run_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
        let snapshot = decode_agent_run_snapshot(&snapshot_json)?;
        let state: RunState = decode(&state_json, "run state")?;
        if snapshot.execution_mode != super::model::ExecutionMode::Interactive
            || state != RunState::Finalizing
            || current_attempt_id.as_deref() != Some(attempt_id)
            || evidence.profile_version() != snapshot.profile.version
        {
            return Err(CodingAgentError::Conflict(
                "interactive finalization does not match the current run snapshot".into(),
            ));
        }
        self.ensure_attempt_in_transaction(&transaction, run_id, attempt_id)?;
        let finish_profile_version: Option<i64> = transaction
            .query_row(
                "SELECT profile_version FROM agent_run_interactive_finish_requests
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), attempt_id],
                |row| row.get(0),
            )
            .optional()?;
        if finish_profile_version != Some(sql_i64(snapshot.profile.version, "profile version")?) {
            return Err(CodingAgentError::Conflict(
                "interactive finalization requires a persisted matching finish request".into(),
            ));
        }
        let serialized =
            serde_json::to_string(&StoredInteractiveFinalizationEvidence::from(&evidence))?;
        let validated: StoredInteractiveFinalizationEvidence =
            decode(&serialized, "interactive finalization")?;
        let _ = validated.into_public()?;
        let existing: Option<String> = transaction
            .query_row(
                "SELECT evidence_json FROM agent_run_interactive_finalizations
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), attempt_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing == serialized {
                return Ok(());
            }
            return Err(CodingAgentError::Conflict(
                "interactive finalization evidence is immutable for this attempt".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO agent_run_interactive_finalizations
                (run_id, attempt_id, evidence_json, recorded_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                run_id.as_str(),
                attempt_id,
                serialized,
                sql_i64(now_ms(), "interactive finalization timestamp")?,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Save verified finalization inputs as one immutable evidence blob and
    /// advance the run revision so all later claimants observe this CAS point.
    pub fn write_verified_finalization_inputs(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        attempt_id: &str,
        inputs: HostVerifiedFinalizationInputs,
    ) -> Result<EvidenceReference, CodingAgentError> {
        self.write_verified_finalization_inputs_inner(
            run_id,
            expected_revision,
            attempt_id,
            inputs,
            || Ok(()),
        )
    }

    fn write_verified_finalization_inputs_inner<F>(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        attempt_id: &str,
        inputs: HostVerifiedFinalizationInputs,
        before_index: F,
    ) -> Result<EvidenceReference, CodingAgentError>
    where
        F: FnOnce() -> Result<(), CodingAgentError>,
    {
        validate_safe_id(attempt_id, "attempt id")?;
        if inputs.schema_version != VERIFIED_FINALIZATION_INPUTS_SCHEMA_VERSION
            || inputs.identity.run_id != run_id.as_str()
            || inputs.identity.attempt_id != attempt_id
        {
            return Err(CodingAgentError::Invalid(
                "verified finalization input identity or schema is invalid".into(),
            ));
        }
        let serialized = serde_json::to_vec(&inputs)?;
        if serialized.len() > MAX_VERIFIED_FINALIZATION_INPUTS_BYTES {
            return Err(CodingAgentError::Invalid(
                "verified finalization inputs exceed their bounded stored size".into(),
            ));
        }
        let current_bytes = {
            let connection = self.lock_connection()?;
            load_finalization_input_bytes(&connection, self, run_id, attempt_id)?
        };
        if let Some((reference, bytes)) = current_bytes {
            if bytes == serialized {
                return Ok(reference);
            }
            return Err(CodingAgentError::Conflict(
                "verified finalization inputs are immutable for this attempt".into(),
            ));
        }

        let (snapshot, contract) = {
            let connection = self.lock_connection()?;
            let snapshot_json: String = connection
                .query_row(
                    "SELECT snapshot_json FROM agent_runs WHERE run_id = ?1",
                    [run_id.as_str()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
            let stored_snapshot = decode_stored_snapshot(&snapshot_json)?;
            let contract = match stored_snapshot
                .contract_status()
                .map_err(|error| CodingAgentError::Corrupt(error.to_string()))?
            {
                ApprovalAssessmentContractStatus::LegacyIncomplete => {
                    return Err(CodingAgentError::Conflict(
                        "legacy run has no complete finalization approval contract".into(),
                    ));
                }
                ApprovalAssessmentContractStatus::Versioned { contract, .. } => *contract,
            };
            validate_accepted_one_shot_start_receipt(
                &connection,
                self,
                &stored_snapshot.snapshot,
                attempt_id,
                &inputs.identity,
            )?;
            (stored_snapshot.snapshot, contract)
        };
        validate_host_finalization_inputs_for_run(&inputs, &snapshot, &contract)?;
        validate_host_capture_evidence(self, run_id, attempt_id, &inputs)?;

        {
            let mut connection = self.lock_connection()?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            ensure_no_dispatched_completion(&transaction, run_id)?;
            ensure_no_attempt_cancellation_effect(&transaction, run_id, attempt_id)?;
            self.ensure_attempt_in_transaction(&transaction, run_id, attempt_id)?;
            self.ensure_current_attempt_in_transaction(&transaction, run_id, attempt_id)?;
            let (revision, state_json, snapshot_json): (i64, String, String) = transaction
                .query_row(
                    "SELECT revision, state_json, snapshot_json FROM agent_runs WHERE run_id = ?1",
                    [run_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
            let current_snapshot = decode_stored_snapshot(&snapshot_json)?;
            if nonnegative_u64(revision, "run revision")? != expected_revision
                || decode::<RunState>(&state_json, "run state")? != RunState::Finalizing
                || current_snapshot.snapshot != snapshot
            {
                return Err(CodingAgentError::Conflict(
                    "verified finalization input CAS does not match the current finalizing attempt"
                        .into(),
                ));
            }
            match snapshot.execution_mode {
                super::model::ExecutionMode::Interactive => {
                    self.require_finish_request(&transaction, run_id, attempt_id, &snapshot)?;
                }
                super::model::ExecutionMode::OneShot => {
                    validate_accepted_one_shot_start_receipt(
                        &transaction,
                        self,
                        &snapshot,
                        attempt_id,
                        &inputs.identity,
                    )?;
                }
            }
            let final_row_exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_run_execution_evidence
                 WHERE run_id = ?1 AND attempt_id = ?2)",
                params![run_id.as_str(), attempt_id],
                |row| row.get(0),
            )?;
            if final_row_exists {
                return Err(CodingAgentError::Conflict(
                    "verified finalization inputs must be saved before final execution assessment"
                        .into(),
                ));
            }
            transaction.commit()?;
        }

        // Blob durability is established before the immutable per-attempt index
        // and the revision CAS become visible in SQLite.
        let reference = self.write_evidence(
            run_id,
            attempt_id,
            VERIFIED_FINALIZATION_INPUTS_EVIDENCE_NAME,
            "application/json",
            &serialized,
        )?;
        before_index()?;

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_no_dispatched_completion(&transaction, run_id)?;
        ensure_no_attempt_cancellation_effect(&transaction, run_id, attempt_id)?;
        if let Some((existing_reference, existing_bytes)) =
            load_finalization_input_bytes(&transaction, self, run_id, attempt_id)?
        {
            if existing_bytes == serialized {
                transaction.commit()?;
                return Ok(existing_reference);
            }
            return Err(CodingAgentError::Conflict(
                "verified finalization inputs are immutable for this attempt".into(),
            ));
        }
        self.ensure_attempt_in_transaction(&transaction, run_id, attempt_id)?;
        self.ensure_current_attempt_in_transaction(&transaction, run_id, attempt_id)?;
        let (snapshot_json, revision, state_json, current_attempt_id): (
            String,
            i64,
            String,
            Option<String>,
        ) = transaction.query_row(
            "SELECT snapshot_json, revision, state_json, current_attempt_id
             FROM agent_runs WHERE run_id = ?1",
            [run_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        if nonnegative_u64(revision, "run revision")? != expected_revision
            || decode::<RunState>(&state_json, "run state")? != RunState::Finalizing
            || current_attempt_id.as_deref() != Some(attempt_id)
        {
            return Err(CodingAgentError::Conflict(
                "verified finalization input CAS does not match the current finalizing attempt"
                    .into(),
            ));
        }
        let current_snapshot = decode_stored_snapshot(&snapshot_json)?;
        if current_snapshot.snapshot.run_id != *run_id || current_snapshot.snapshot != snapshot {
            return Err(CodingAgentError::Conflict(
                "run snapshot changed while saving verified finalization inputs".into(),
            ));
        }
        match current_snapshot.snapshot.execution_mode {
            super::model::ExecutionMode::Interactive => {
                self.require_finish_request(
                    &transaction,
                    run_id,
                    attempt_id,
                    &current_snapshot.snapshot,
                )?;
            }
            super::model::ExecutionMode::OneShot => {
                validate_accepted_one_shot_start_receipt(
                    &transaction,
                    self,
                    &current_snapshot.snapshot,
                    attempt_id,
                    &inputs.identity,
                )?;
            }
        }
        let final_row_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_run_execution_evidence
             WHERE run_id = ?1 AND attempt_id = ?2)",
            params![run_id.as_str(), attempt_id],
            |row| row.get(0),
        )?;
        if final_row_exists {
            return Err(CodingAgentError::Conflict(
                "verified finalization inputs must be saved before final execution assessment"
                    .into(),
            ));
        }
        let actual_reference =
            find_evidence_in_transaction(&transaction, run_id, &reference.evidence_id)?
                .ok_or_else(|| {
                    CodingAgentError::Corrupt("verified finalization evidence disappeared".into())
                })?;
        if actual_reference != reference
            || !self.verify_reference_for_run(run_id, &actual_reference)
        {
            return Err(CodingAgentError::Corrupt(
                "verified finalization evidence failed its durable reference check".into(),
            ));
        }
        let next_revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| CodingAgentError::Conflict("run revision overflow".into()))?;
        transaction.execute(
            "INSERT INTO agent_run_finalization_inputs (run_id, attempt_id, evidence_id)
             VALUES (?1, ?2, ?3)",
            params![run_id.as_str(), attempt_id, reference.evidence_id],
        )?;
        let updated = transaction.execute(
            "UPDATE agent_runs SET revision = ?1
             WHERE run_id = ?2 AND revision = ?3 AND state_json = ?4
               AND current_attempt_id = ?5",
            params![
                sql_i64(next_revision, "run revision")?,
                run_id.as_str(),
                sql_i64(expected_revision, "expected run revision")?,
                serde_json::to_string(&RunState::Finalizing)?,
                attempt_id,
            ],
        )?;
        if updated != 1 {
            return Err(CodingAgentError::Conflict(
                "run changed while indexing verified finalization inputs".into(),
            ));
        }
        transaction.commit()?;
        Ok(reference)
    }

    #[cfg(test)]
    fn write_verified_finalization_inputs_with_before_index<F>(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        attempt_id: &str,
        inputs: HostVerifiedFinalizationInputs,
        before_index: F,
    ) -> Result<EvidenceReference, CodingAgentError>
    where
        F: FnOnce() -> Result<(), CodingAgentError>,
    {
        self.write_verified_finalization_inputs_inner(
            run_id,
            expected_revision,
            attempt_id,
            inputs,
            before_index,
        )
    }

    /// Reopen the one indexed finalization-input blob and revalidate its schema,
    /// attempt ownership, evidence hashes, report, native target, and receipts.
    pub fn read_saved_verified_finalization_inputs(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<SavedVerifiedFinalizationInputs, CodingAgentError> {
        validate_safe_id(attempt_id, "attempt id")?;
        let connection = self.lock_connection()?;
        load_saved_verified_finalization_inputs(&connection, self, run_id, attempt_id)?
            .ok_or_else(|| CodingAgentError::NotFound("verified finalization inputs".into()))
    }

    /// Bind the exact approved report, target, receipts, and actual saved judge
    /// payload before provider dispatch. A new claim is valid only in Finalizing;
    /// Verifying is recovery-only after its immutable final execution row exists.
    pub fn bind_assessment_intent_from_finalization_inputs(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        attempt_id: &str,
        request: AssessmentIntentRequest,
    ) -> Result<AssessmentIntentClaim, CodingAgentError> {
        validate_safe_id(attempt_id, "assessment attempt id")?;
        validate_safe_id(&request.assessment_id, "assessment id")?;
        validate_safe_id(&request.judge_input_evidence_id, "judge input evidence id")?;
        validate_safe_id(&request.report_evidence_id, "report evidence id")?;
        validate_safe_id(
            &request.snapshot_manifest_evidence_id,
            "snapshot manifest evidence id",
        )?;
        let target = super::validation::ValidationTargetSnapshot::new(
            request.target_full_commit_id.clone(),
            request.target_full_tree_id.clone(),
            request.target_untracked_manifest_digest.clone(),
        )
        .map_err(|_| {
            CodingAgentError::Invalid("assessment target snapshot identity is invalid".into())
        })?;
        if target.digest() != request.target_snapshot_digest
            || !valid_sha256(&request.system_prompt_digest)
        {
            return Err(CodingAgentError::Invalid(
                "assessment target or system prompt digest is invalid".into(),
            ));
        }

        let snapshot_json = self.snapshot_json(run_id)?;
        let stored_snapshot = decode_stored_snapshot(&snapshot_json)?;
        if stored_snapshot.snapshot.run_id != *run_id {
            return Err(CodingAgentError::Corrupt(
                "stored assessment snapshot has a different run identity".into(),
            ));
        }
        let (contract, contract_digest) = match stored_snapshot
            .contract_status()
            .map_err(|error| CodingAgentError::Corrupt(error.to_string()))?
        {
            ApprovalAssessmentContractStatus::LegacyIncomplete => {
                return Err(CodingAgentError::Conflict(
                    "legacy run has no complete assessment approval contract".into(),
                ));
            }
            ApprovalAssessmentContractStatus::Versioned { contract, digest } => (*contract, digest),
        };
        if !contract.can_dispatch_semantic_assessment() {
            return Err(CodingAgentError::Conflict(
                "approved assessment policy is unknown or has no dispatchable semantic criteria"
                    .into(),
            ));
        }

        let saved = self.read_saved_verified_finalization_inputs(run_id, attempt_id)?;
        if saved.identity.run_id != run_id.as_str()
            || saved.identity.attempt_id != attempt_id
            || saved.target.full_commit_id != request.target_full_commit_id
            || saved.target.full_tree_id != request.target_full_tree_id
            || saved.target.untracked_manifest_digest != request.target_untracked_manifest_digest
            || saved.target.target_snapshot_digest != request.target_snapshot_digest
            || saved.report_reference.evidence_id != request.report_evidence_id
        {
            return Err(CodingAgentError::Conflict(
                "assessment request differs from the saved verified finalization inputs".into(),
            ));
        }
        validate_finalization_claimable(&saved, &contract)?;
        let validation_receipts =
            assessment_validation_receipt_references(&contract, run_id, attempt_id, &request)?;
        if validation_receipts != saved.validation_receipt_references {
            return Err(CodingAgentError::Conflict(
                "assessment receipts differ from the exact reopened host validation receipts"
                    .into(),
            ));
        }

        let (input_reference, input_bytes) = self.read_bounded_attempt_evidence(
            run_id,
            attempt_id,
            &request.judge_input_evidence_id,
            contract.input_scope.max_input_bytes,
        )?;
        if input_bytes.is_empty() || input_reference.media_type != "application/json" {
            return Err(CodingAgentError::Invalid(
                "exact judge input payload must be nonempty JSON evidence".into(),
            ));
        }
        let (report_reference, report_bytes) = self.read_bounded_attempt_evidence(
            run_id,
            attempt_id,
            &request.report_evidence_id,
            super::report::MAX_REPORT_BYTES,
        )?;
        let approved_report_limit = contract
            .input_scope
            .allowed_sources
            .iter()
            .find(|source| source.kind == super::assessment::EvidenceSourceKind::Report)
            .map(|source| source.max_bytes)
            .ok_or_else(|| {
                CodingAgentError::Conflict("approved judge scope has no report source".into())
            })?;
        if report_reference.media_type != "application/json"
            || report_bytes.len() > approved_report_limit
        {
            return Err(CodingAgentError::Invalid(
                "report is not within the approved judge source bounds".into(),
            ));
        }
        let (manifest_reference, _manifest_bytes) = self.read_bounded_attempt_evidence(
            run_id,
            attempt_id,
            &request.snapshot_manifest_evidence_id,
            super::report::MAX_REPORT_BYTES,
        )?;
        if manifest_reference.media_type != "application/json"
            || manifest_reference.sha256 != saved.target.artifact_manifest_digest
        {
            return Err(CodingAgentError::Conflict(
                "snapshot manifest reference does not match the reopened native artifact manifest"
                    .into(),
            ));
        }
        validate_report_bytes(&contract, run_id, attempt_id, &report_bytes)?;
        validate_saved_judge_input(
            &input_bytes,
            SavedJudgeInputValidationContext {
                run_id,
                attempt_id,
                assessment_id: &request.assessment_id,
                snapshot_json: &snapshot_json,
                contract: &contract,
                contract_digest: &contract_digest,
                report_reference: &report_reference,
                report_bytes: &report_bytes,
                saved: &saved,
            },
        )?;

        let candidate = AssessmentIntentBinding {
            run_id: run_id.clone(),
            attempt_id: attempt_id.into(),
            assessment_id: request.assessment_id.clone(),
            expected_run_revision: expected_revision,
            approval_snapshot_digest: format!("{:x}", Sha256::digest(snapshot_json.as_bytes())),
            approval_contract_digest: contract_digest,
            judge_input_scope_digest: contract.judge_input_scope_digest().into(),
            system_prompt_digest: request.system_prompt_digest,
            judge_input_evidence_id: input_reference.evidence_id.clone(),
            judge_input_digest: input_reference.sha256.clone(),
            report_evidence_id: report_reference.evidence_id.clone(),
            report_digest: report_reference.sha256.clone(),
            snapshot_manifest_evidence_id: manifest_reference.evidence_id.clone(),
            snapshot_manifest_digest: manifest_reference.sha256.clone(),
            target_full_commit_id: request.target_full_commit_id,
            target_full_tree_id: request.target_full_tree_id,
            target_untracked_manifest_digest: request.target_untracked_manifest_digest,
            target_snapshot_digest: request.target_snapshot_digest,
            validation_receipts,
            created_at_ms: 0,
        };

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_no_dispatched_completion(&transaction, run_id)?;
        ensure_no_attempt_cancellation_effect(&transaction, run_id, attempt_id)?;
        let (current_snapshot_json, revision, state_json, current_attempt_id): (
            String,
            i64,
            String,
            Option<String>,
        ) = transaction
            .query_row(
                "SELECT snapshot_json, revision, state_json, current_attempt_id
                 FROM agent_runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
        let revision = nonnegative_u64(revision, "run revision")?;
        let state: RunState = decode(&state_json, "run state")?;
        if current_snapshot_json != snapshot_json
            || current_attempt_id.as_deref() != Some(attempt_id)
            || revision != expected_revision
        {
            return Err(CodingAgentError::Conflict(
                "assessment intent CAS does not match the current run snapshot, revision, and attempt"
                    .into(),
            ));
        }
        let current_snapshot = decode_stored_snapshot(&current_snapshot_json)?;
        let current_contract_digest = current_snapshot
            .approval_assessment_contract
            .as_ref()
            .and_then(|stored| stored.digest().ok());
        if current_contract_digest.as_deref() != Some(candidate.approval_contract_digest.as_str()) {
            return Err(CodingAgentError::Conflict(
                "assessment approval contract changed before intent binding".into(),
            ));
        }

        let existing = load_assessment_intent(&transaction, self, run_id, attempt_id)?;
        if let Some(existing) = existing {
            validate_assessment_intent_scope(&existing, &contract)?;
            let linked_finalization_id: Option<String> = transaction.query_row(
                "SELECT finalization_evidence_id FROM agent_run_assessment_intents
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), attempt_id],
                |row| row.get(0),
            )?;
            if linked_finalization_id.as_deref() != Some(saved.index_reference.evidence_id.as_str())
            {
                return Err(CodingAgentError::Conflict(
                    "existing assessment intent is not linked to these verified finalization inputs"
                        .into(),
                ));
            }
            let final_row: Option<String> = transaction
                .query_row(
                    "SELECT result_json FROM agent_run_execution_evidence
                     WHERE run_id = ?1 AND attempt_id = ?2",
                    params![run_id.as_str(), attempt_id],
                    |row| row.get(0),
                )
                .optional()?;
            if state == RunState::Verifying {
                let final_json = final_row.ok_or_else(|| {
                    CodingAgentError::Conflict(
                        "verifying assessment recovery requires the immutable final execution row"
                            .into(),
                    )
                })?;
                let final_evidence: RunEvidenceAssessment =
                    decode(&final_json, "execution evidence")?;
                validate_final_assessment_against_inputs(&final_evidence, &saved)?;
            } else if state != RunState::Finalizing || final_row.is_some() {
                return Err(CodingAgentError::Conflict(
                    "assessment intent recovery is permitted only before final assessment or in Verifying"
                        .into(),
                ));
            }
            let mut retry = candidate;
            retry.expected_run_revision = existing.expected_run_revision;
            retry.created_at_ms = existing.created_at_ms;
            if existing == retry {
                transaction.commit()?;
                return Ok(AssessmentIntentClaim {
                    binding: existing,
                    newly_created: false,
                });
            }
            return Err(CodingAgentError::Conflict(
                "attempt already has a different immutable assessment intent".into(),
            ));
        }

        if state != RunState::Finalizing {
            return Err(CodingAgentError::Conflict(
                "a new assessment intent requires the current attempt to remain Finalizing".into(),
            ));
        }
        let final_row_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_run_execution_evidence
             WHERE run_id = ?1 AND attempt_id = ?2)",
            params![run_id.as_str(), attempt_id],
            |row| row.get(0),
        )?;
        if final_row_exists {
            return Err(CodingAgentError::Conflict(
                "judge intent must be claimed before immutable final execution assessment".into(),
            ));
        }
        let indexed_reference = find_finalization_input_reference(
            &transaction,
            run_id,
            attempt_id,
        )?
        .ok_or_else(|| {
            CodingAgentError::Conflict("verified finalization inputs are not indexed".into())
        })?;
        if indexed_reference != saved.index_reference
            || !self.verify_reference_for_run(run_id, &indexed_reference)
        {
            return Err(CodingAgentError::Conflict(
                "verified finalization input reference changed before intent binding".into(),
            ));
        }
        recheck_assessment_evidence_reference(&transaction, run_id, attempt_id, &input_reference)?;
        recheck_assessment_evidence_reference(&transaction, run_id, attempt_id, &report_reference)?;
        recheck_assessment_evidence_reference(
            &transaction,
            run_id,
            attempt_id,
            &manifest_reference,
        )?;
        recheck_assessment_evidence_reference(
            &transaction,
            run_id,
            attempt_id,
            &indexed_reference,
        )?;
        let mut binding = candidate;
        binding.created_at_ms = now_ms();
        transaction.execute(
            "INSERT INTO agent_run_assessment_intents
                (run_id, attempt_id, assessment_id, record_json, created_at_ms,
                 finalization_evidence_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run_id.as_str(),
                attempt_id,
                binding.assessment_id,
                serde_json::to_string(&binding)?,
                sql_i64(binding.created_at_ms, "assessment intent timestamp")?,
                saved.index_reference.evidence_id,
            ],
        )?;
        transaction.commit()?;
        Ok(AssessmentIntentClaim {
            binding,
            newly_created: true,
        })
    }

    /// Compare-and-swap a state transition. Publishing and auto-DONE are
    /// evidence-gated here, not merely by a caller's requested target state.
    pub fn transition(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        target: RunState,
    ) -> Result<AgentRunRecord, CodingAgentError> {
        if target == RunState::Done {
            return Err(CodingAgentError::Conflict(
                "Done requires a committed Issue completion authorization".into(),
            ));
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_no_dispatched_completion(&transaction, run_id)?;
        let (snapshot_json, current_revision, current_state_json, current_attempt_id): (
            String,
            i64,
            String,
            Option<String>,
        ) = transaction
            .query_row(
                "SELECT snapshot_json, revision, state_json, current_attempt_id
                     FROM agent_runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
        let current_revision = nonnegative_u64(current_revision, "run revision")?;
        if current_revision != expected_revision {
            return Err(CodingAgentError::Conflict(format!(
                "run revision changed from expected {expected_revision} to {current_revision}"
            )));
        }
        let snapshot = decode_agent_run_snapshot(&snapshot_json)?;
        let current: RunState = decode(&current_state_json, "run state")?;
        if target == RunState::Running {
            return Err(CodingAgentError::Conflict(
                "Running requires mark_attempt_running with the claimed attempt and revision"
                    .into(),
            ));
        }
        if target == RunState::Preparing && current != RunState::Queued {
            return Err(CodingAgentError::Conflict(
                "retry preparation requires begin_retry_preparation after a confirmed terminal result"
                    .into(),
            ));
        }
        if !can_transition(current, target) {
            return Err(CodingAgentError::Conflict(format!(
                "state transition {current:?} -> {target:?} is not allowed"
            )));
        }
        match target {
            RunState::Finalizing
                if snapshot.execution_mode == super::model::ExecutionMode::Interactive =>
            {
                let attempt_id = current_attempt_id.as_deref().ok_or_else(|| {
                    CodingAgentError::Conflict("current attempt is not established".into())
                })?;
                self.require_finish_request(&transaction, run_id, attempt_id, &snapshot)?;
            }
            RunState::Verifying => {
                if snapshot.execution_mode == super::model::ExecutionMode::Interactive {
                    let attempt_id = current_attempt_id.as_deref().ok_or_else(|| {
                        CodingAgentError::Conflict("current attempt is not established".into())
                    })?;
                    let evidence = required_execution_evidence(&transaction, run_id, attempt_id)?;
                    let evidence = self.verify_assessment(&transaction, run_id, evidence)?;
                    if !successful_report_is_durable(&evidence) {
                        return Err(CodingAgentError::Conflict(
                            "interactive execution evidence is not a successful durable finish"
                                .into(),
                        ));
                    }
                    self.require_interactive_finalization(
                        &transaction,
                        run_id,
                        attempt_id,
                        &snapshot,
                        &evidence,
                    )?;
                }
            }
            RunState::Publishing => {
                let attempt_id = current_attempt_id.as_deref().ok_or_else(|| {
                    CodingAgentError::Conflict("current attempt is not established".into())
                })?;
                ensure_no_attempt_cancellation_effect(&transaction, run_id, attempt_id)?;
                let evidence = required_execution_evidence(&transaction, run_id, attempt_id)?;
                let evidence = self.verify_assessment(&transaction, run_id, evidence)?;
                let decision = evaluate_completion_gates(
                    snapshot.purpose,
                    snapshot.auto_done,
                    snapshot.publication_plan.clone(),
                    &snapshot.allowed_operations,
                    &evidence,
                );
                if decision.publication != super::model::GateDecision::Allowed {
                    return Err(CodingAgentError::Conflict(format!(
                        "publication gate is not satisfied: {:?}",
                        decision.publication
                    )));
                }
                if snapshot.execution_mode == super::model::ExecutionMode::Interactive {
                    self.require_interactive_finalization(
                        &transaction,
                        run_id,
                        attempt_id,
                        &snapshot,
                        &evidence,
                    )?;
                }
            }
            RunState::AwaitingReview => {
                if let Some(attempt_id) = current_attempt_id.as_deref() {
                    ensure_no_attempt_cancellation_effect(&transaction, run_id, attempt_id)?;
                }
            }
            RunState::Done => {
                let attempt_id = current_attempt_id.as_deref().ok_or_else(|| {
                    CodingAgentError::Conflict("current attempt is not established".into())
                })?;
                ensure_no_attempt_cancellation_effect(&transaction, run_id, attempt_id)?;
                let evidence = required_execution_evidence(&transaction, run_id, attempt_id)?;
                let evidence = self.verify_assessment(&transaction, run_id, evidence)?;
                let decision = evaluate_completion_gates(
                    snapshot.purpose,
                    snapshot.auto_done,
                    snapshot.publication_plan.clone(),
                    &snapshot.allowed_operations,
                    &evidence,
                );
                if decision.auto_done != super::model::GateDecision::Allowed {
                    return Err(CodingAgentError::Conflict(format!(
                        "auto-DONE gate is not satisfied: {:?}",
                        decision.auto_done
                    )));
                }
                if snapshot.execution_mode == super::model::ExecutionMode::Interactive {
                    self.require_interactive_finalization(
                        &transaction,
                        run_id,
                        attempt_id,
                        &snapshot,
                        &evidence,
                    )?;
                }
            }
            RunState::InvestigationCompleted => {
                if snapshot.purpose != RunPurpose::Investigation {
                    return Err(CodingAgentError::Conflict(
                        "only investigation runs can enter investigation-completed".into(),
                    ));
                }
                let attempt_id = current_attempt_id.as_deref().ok_or_else(|| {
                    CodingAgentError::Conflict("current attempt is not established".into())
                })?;
                let evidence = required_execution_evidence(&transaction, run_id, attempt_id)?;
                let evidence = self.verify_assessment(&transaction, run_id, evidence)?;
                if !successful_report_is_durable(&evidence) {
                    return Err(CodingAgentError::Conflict(
                        "investigation report and successful stream evidence are incomplete".into(),
                    ));
                }
                if snapshot.execution_mode == super::model::ExecutionMode::Interactive {
                    self.require_interactive_finalization(
                        &transaction,
                        run_id,
                        attempt_id,
                        &snapshot,
                        &evidence,
                    )?;
                }
            }
            _ => {}
        }

        let next_revision = current_revision
            .checked_add(1)
            .ok_or_else(|| CodingAgentError::Conflict("run revision overflow".into()))?;
        let updated = transaction.execute(
            "UPDATE agent_runs SET revision = ?1, state_json = ?2, stage_json = ?3
             WHERE run_id = ?4 AND revision = ?5",
            params![
                sql_i64(next_revision, "run revision")?,
                serde_json::to_string(&target)?,
                serde_json::to_string(&target.stage())?,
                run_id.as_str(),
                sql_i64(current_revision, "run revision")?,
            ],
        )?;
        if updated != 1 {
            return Err(CodingAgentError::Conflict(
                "run revision changed during state transition".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO agent_run_transitions (run_id, revision, from_state_json, to_state_json, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                run_id.as_str(),
                sql_i64(next_revision, "run revision")?,
                serde_json::to_string(&current)?,
                serde_json::to_string(&target)?,
                sql_i64(now_ms(), "transition time")?,
            ],
        )?;
        transaction.commit()?;
        drop(connection);
        self.get_run(run_id)?
            .ok_or_else(|| CodingAgentError::Corrupt("transitioned run disappeared".into()))
    }

    /// Confirm the claimed attempt is the one actually launched. Callers must
    /// pass the revision returned by `start_attempt`; stale or losing attempts
    /// cannot advance the run to Running.
    pub fn mark_attempt_running(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        attempt_id: &str,
    ) -> Result<AgentRunRecord, CodingAgentError> {
        validate_safe_id(attempt_id, "attempt id")?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_no_dispatched_completion(&transaction, run_id)?;
        let (revision, state_json, current_attempt_id, phase_generation): (
            i64,
            String,
            Option<String>,
            i64,
        ) = transaction
            .query_row(
                "SELECT revision, state_json, current_attempt_id, start_phase_generation
                 FROM agent_runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
        let revision = nonnegative_u64(revision, "run revision")?;
        let phase_generation = nonnegative_u64(phase_generation, "start phase generation")?;
        let state: RunState = decode(&state_json, "run state")?;
        if revision != expected_revision
            || state != RunState::Preparing
            || current_attempt_id.as_deref() != Some(attempt_id)
        {
            return Err(CodingAgentError::Conflict(
                "running transition does not match the current claimed attempt and revision".into(),
            ));
        }
        let claim_revision: Option<i64> = transaction
            .query_row(
                "SELECT run_revision FROM agent_run_start_claims
                 WHERE run_id = ?1 AND phase_generation = ?2 AND attempt_id = ?3",
                params![
                    run_id.as_str(),
                    sql_i64(phase_generation, "start phase generation")?,
                    attempt_id,
                ],
                |row| row.get(0),
            )
            .optional()?;
        if claim_revision != Some(sql_i64(expected_revision, "start claim revision")?) {
            return Err(CodingAgentError::Conflict(
                "attempt has no current durable start claim for this revision".into(),
            ));
        }
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| CodingAgentError::Conflict("run revision overflow".into()))?;
        let updated = transaction.execute(
            "UPDATE agent_runs SET revision = ?1, state_json = ?2, stage_json = ?3
             WHERE run_id = ?4 AND revision = ?5 AND current_attempt_id = ?6
               AND start_phase_generation = ?7",
            params![
                sql_i64(next_revision, "run revision")?,
                serde_json::to_string(&RunState::Running)?,
                serde_json::to_string(&RunState::Running.stage())?,
                run_id.as_str(),
                sql_i64(expected_revision, "expected run revision")?,
                attempt_id,
                sql_i64(phase_generation, "start phase generation")?,
            ],
        )?;
        if updated != 1 {
            return Err(CodingAgentError::Conflict(
                "run changed while confirming the launched attempt".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO agent_run_transitions
                (run_id, revision, from_state_json, to_state_json, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                run_id.as_str(),
                sql_i64(next_revision, "run revision")?,
                serde_json::to_string(&RunState::Preparing)?,
                serde_json::to_string(&RunState::Running)?,
                sql_i64(now_ms(), "transition time")?,
            ],
        )?;
        transaction.commit()?;
        drop(connection);
        self.get_run(run_id)?
            .ok_or_else(|| CodingAgentError::Corrupt("started run disappeared".into()))
    }

    /// Explicitly reopen preparation only after the previous attempt has a
    /// confirmed Failed/Cancelled outcome and every durable operation resolved.
    /// Unknown is intentionally not retryable through this API.
    pub fn begin_retry_preparation(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        resolved_attempt_id: &str,
    ) -> Result<AgentRunRecord, CodingAgentError> {
        validate_safe_id(resolved_attempt_id, "attempt id")?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_no_dispatched_completion(&transaction, run_id)?;
        let (revision, state_json, current_attempt_id, phase_generation): (
            i64,
            String,
            Option<String>,
            i64,
        ) = transaction
            .query_row(
                "SELECT revision, state_json, current_attempt_id, start_phase_generation
                 FROM agent_runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
        let revision = nonnegative_u64(revision, "run revision")?;
        let phase_generation = nonnegative_u64(phase_generation, "start phase generation")?;
        let state: RunState = decode(&state_json, "run state")?;
        if revision != expected_revision
            || !matches!(state, RunState::Failed | RunState::Cancelled)
            || current_attempt_id.as_deref() != Some(resolved_attempt_id)
        {
            return Err(CodingAgentError::Conflict(
                "retry requires the current attempt to have a confirmed terminal outcome".into(),
            ));
        }
        let has_start_result: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM agent_run_operations
                 WHERE run_id = ?1 AND attempt_id = ?2 AND kind_json = ?3
                   AND status_json = ?4
             )",
            params![
                run_id.as_str(),
                resolved_attempt_id,
                serde_json::to_string(&super::model::DurableOperationKind::StartAgent)?,
                serde_json::to_string(&DurableOperationStatus::Completed)?,
            ],
            |row| row.get(0),
        )?;
        let unresolved_operations: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM agent_run_operations
                 WHERE run_id = ?1 AND attempt_id = ?2 AND status_json != ?3
             )",
            params![
                run_id.as_str(),
                resolved_attempt_id,
                serde_json::to_string(&DurableOperationStatus::Completed)?,
            ],
            |row| row.get(0),
        )?;
        if !has_start_result || unresolved_operations {
            return Err(CodingAgentError::Conflict(
                "retry requires a reconciled StartAgent and no unresolved operations".into(),
            ));
        }
        let next_phase_generation = phase_generation
            .checked_add(1)
            .ok_or_else(|| CodingAgentError::Conflict("start phase generation overflow".into()))?;
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| CodingAgentError::Conflict("run revision overflow".into()))?;
        let updated = transaction.execute(
            "UPDATE agent_runs
             SET revision = ?1, state_json = ?2, stage_json = ?3,
                 current_attempt_id = NULL, start_phase_generation = ?4
             WHERE run_id = ?5 AND revision = ?6 AND current_attempt_id = ?7",
            params![
                sql_i64(next_revision, "run revision")?,
                serde_json::to_string(&RunState::Preparing)?,
                serde_json::to_string(&RunState::Preparing.stage())?,
                sql_i64(next_phase_generation, "start phase generation")?,
                run_id.as_str(),
                sql_i64(expected_revision, "expected run revision")?,
                resolved_attempt_id,
            ],
        )?;
        if updated != 1 {
            return Err(CodingAgentError::Conflict(
                "run changed while reopening retry preparation".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO agent_run_transitions
                (run_id, revision, from_state_json, to_state_json, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                run_id.as_str(),
                sql_i64(next_revision, "run revision")?,
                serde_json::to_string(&state)?,
                serde_json::to_string(&RunState::Preparing)?,
                sql_i64(now_ms(), "transition time")?,
            ],
        )?;
        transaction.commit()?;
        drop(connection);
        self.get_run(run_id)?
            .ok_or_else(|| CodingAgentError::Corrupt("retry preparation disappeared".into()))
    }

    /// Reopen all runs whose final state is not known. In particular, Unknown
    /// and AwaitingReview remain recoverable; no successful state is inferred.
    pub fn recoverable_runs(&self) -> Result<Vec<AgentRunRecord>, CodingAgentError> {
        let connection = self.lock_connection()?;
        let mut statement =
            connection.prepare("SELECT run_id FROM agent_runs ORDER BY created_at_ms")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let ids = rows.collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|value| RunId::new(value).map_err(CodingAgentError::Model))
            .map(|id| {
                let id = id?;
                let run = load_run(&connection, &id)?;
                match run {
                    Some(run) if !run.state.is_terminal() => Ok(Some(run)),
                    _ => Ok(None),
                }
            })
            .filter_map(|result| result.transpose())
            .collect()
    }

    /// Explicitly remove evidence blobs only after a known terminal state and
    /// all durable operations are resolved. There is intentionally no age-based
    /// cleanup; snapshots and evidence digests remain as audit metadata.
    pub fn purge_resolved_evidence(&self, run_id: &RunId) -> Result<usize, CodingAgentError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state_json: String = transaction
            .query_row(
                "SELECT state_json FROM agent_runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
        let state: RunState = decode(&state_json, "run state")?;
        if !state.is_terminal() {
            return Err(CodingAgentError::Conflict(
                "evidence retention requires a resolved terminal run".into(),
            ));
        }
        let completed_status = serde_json::to_string(&DurableOperationStatus::Completed)?;
        let unresolved: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM agent_run_operations
                 WHERE run_id = ?1 AND status_json != ?2
             )",
            params![run_id.as_str(), completed_status],
            |row| row.get(0),
        )?;
        if unresolved {
            return Err(CodingAgentError::Conflict(
                "evidence retention cannot remove artifacts with unresolved operations".into(),
            ));
        }
        let mut statement =
            transaction.prepare("SELECT evidence_id FROM agent_run_evidence WHERE run_id = ?1")?;
        let evidence_ids = statement
            .query_map([run_id.as_str()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let run_directory = self.evidence_root.join(run_id.as_str());
        let evidence_directory = run_directory.join("evidence");
        if !is_real_directory(&self.evidence_root)?
            || !is_real_directory(&run_directory)?
            || !is_real_directory(&evidence_directory)?
        {
            transaction.commit()?;
            return Ok(0);
        }
        let mut removed = 0;
        for evidence_id in evidence_ids {
            let path = evidence_directory.join(format!("{evidence_id}.blob"));
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                    fs::remove_file(path)?;
                    removed += 1;
                }
                Ok(_) => {
                    return Err(CodingAgentError::Invalid(
                        "evidence artifact is not a removable file".into(),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if removed > 0 {
            sync_directory(&evidence_directory)?;
        }
        transaction.commit()?;
        Ok(removed)
    }

    /// Write evidence through a same-directory temp file, fsync it, rename it,
    /// fsync the directory, then publish its digest reference in SQLite.
    pub fn write_evidence(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        name: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<EvidenceReference, CodingAgentError> {
        validate_safe_id(attempt_id, "attempt id")?;
        validate_evidence_metadata(name, media_type)?;
        self.ensure_current_attempt(run_id, attempt_id)?;

        let evidence_id = RunId::generate()?.to_string();
        let run_directory = self.evidence_root.join(run_id.as_str());
        ensure_private_directory(&run_directory)?;
        let directory = run_directory.join("evidence");
        ensure_private_directory(&directory)?;
        let temporary = directory.join(format!(".tmp-{evidence_id}"));
        let committed = directory.join(format!("{evidence_id}.blob"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        set_private_file_mode(&mut options);
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &committed)?;
        sync_directory(&directory)?;

        let reference = EvidenceReference {
            evidence_id,
            attempt_id: attempt_id.into(),
            name: name.into(),
            media_type: media_type.into(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
            size_bytes: bytes.len() as u64,
            save_confirmed: true,
            created_at_ms: now_ms(),
        };
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_no_dispatched_completion(&transaction, run_id)?;
        self.ensure_attempt_in_transaction(&transaction, run_id, attempt_id)?;
        self.ensure_current_attempt_in_transaction(&transaction, run_id, attempt_id)?;
        transaction.execute(
            "INSERT INTO agent_run_evidence
                (run_id, evidence_id, attempt_id, name, media_type, sha256, size_bytes,
                 save_confirmed, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8)",
            params![
                run_id.as_str(),
                reference.evidence_id,
                attempt_id,
                name,
                media_type,
                reference.sha256,
                sql_i64(reference.size_bytes, "evidence size")?,
                sql_i64(reference.created_at_ms, "evidence timestamp")?,
            ],
        )?;
        transaction.commit()?;
        Ok(reference)
    }

    pub fn evidence(&self, run_id: &RunId) -> Result<Vec<EvidenceReference>, CodingAgentError> {
        let connection = self.lock_connection()?;
        if !run_exists(&connection, run_id)? {
            return Err(CodingAgentError::NotFound(format!("run {run_id}")));
        }
        load_evidence(&connection, run_id)
    }

    pub fn read_evidence(
        &self,
        run_id: &RunId,
        evidence_id: &str,
    ) -> Result<Vec<u8>, CodingAgentError> {
        validate_safe_id(evidence_id, "evidence id")?;
        let reference = self
            .find_evidence(run_id, evidence_id)?
            .ok_or_else(|| CodingAgentError::NotFound(format!("evidence {evidence_id}")))?;
        let path = self.evidence_path(run_id, evidence_id);
        let bytes = read_regular_nofollow(&path)?;
        if bytes.len() as u64 != reference.size_bytes
            || format!("{:x}", Sha256::digest(&bytes)) != reference.sha256
        {
            return Err(CodingAgentError::Corrupt(format!(
                "evidence {evidence_id} failed digest/size verification"
            )));
        }
        Ok(bytes)
    }

    pub fn verify_evidence(
        &self,
        run_id: &RunId,
        evidence_id: &str,
    ) -> Result<bool, CodingAgentError> {
        validate_safe_id(evidence_id, "evidence id")?;
        let Some(reference) = self.find_evidence(run_id, evidence_id)? else {
            return Err(CodingAgentError::NotFound(format!(
                "evidence {evidence_id}"
            )));
        };
        let Ok(bytes) = read_regular_nofollow(&self.evidence_path(run_id, evidence_id)) else {
            return Ok(false);
        };
        Ok(bytes.len() as u64 == reference.size_bytes
            && format!("{:x}", Sha256::digest(bytes)) == reference.sha256)
    }

    /// Persist typed final output evidence after matching every byte reference
    /// to a durable reference owned by this run and attempt.
    pub fn record_execution_evidence(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        mut evidence: RunEvidenceAssessment,
    ) -> Result<(), CodingAgentError> {
        validate_safe_id(attempt_id, "attempt id")?;
        if evidence
            .commit_id
            .as_deref()
            .is_some_and(|commit| !valid_commit_id(commit))
        {
            return Err(CodingAgentError::Invalid(
                "commit evidence must be a full hexadecimal object id".into(),
            ));
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_no_dispatched_completion(&transaction, run_id)?;
        self.ensure_attempt_in_transaction(&transaction, run_id, attempt_id)?;
        self.ensure_current_attempt_in_transaction(&transaction, run_id, attempt_id)?;
        if let Some(saved) =
            load_saved_verified_finalization_inputs(&transaction, self, run_id, attempt_id)?
        {
            let snapshot_json: String = transaction.query_row(
                "SELECT snapshot_json FROM agent_runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| row.get(0),
            )?;
            let stored_snapshot = decode_stored_snapshot(&snapshot_json)?;
            let contract = match stored_snapshot
                .contract_status()
                .map_err(|error| CodingAgentError::Corrupt(error.to_string()))?
            {
                ApprovalAssessmentContractStatus::LegacyIncomplete => {
                    return Err(CodingAgentError::Conflict(
                        "final assessment cannot use a legacy incomplete approval contract".into(),
                    ));
                }
                ApprovalAssessmentContractStatus::Versioned { contract, .. } => *contract,
            };
            let intent = load_assessment_intent(&transaction, self, run_id, attempt_id)?;
            let claimable = validate_finalization_claimable(&saved, &contract).is_ok();
            if contract.can_dispatch_semantic_assessment() && claimable && intent.is_none() {
                return Err(CodingAgentError::Conflict(
                    "final execution assessment must be recorded only after its pre-judge intent"
                        .into(),
                ));
            }
            if let Some(intent) = intent {
                if intent.report_evidence_id != saved.report_reference.evidence_id
                    || intent.report_digest != saved.report_reference.sha256
                    || intent.target_full_commit_id != saved.target.full_commit_id
                    || intent.target_full_tree_id != saved.target.full_tree_id
                    || intent.target_untracked_manifest_digest
                        != saved.target.untracked_manifest_digest
                    || intent.target_snapshot_digest != saved.target.target_snapshot_digest
                    || intent.validation_receipts != saved.validation_receipt_references
                {
                    return Err(CodingAgentError::Conflict(
                        "assessment intent does not match immutable finalization inputs".into(),
                    ));
                }
                validate_final_assessment_against_inputs(&evidence, &saved)?;
            } else if !claimable {
                let explicit_failure = evidence.known_failure
                    || evidence.cancelled
                    || evidence.stream_error
                    || evidence.final_exit_code != Some(0)
                    || !evidence.end_present;
                if evidence.acceptance_conditions_met || !explicit_failure {
                    return Err(CodingAgentError::Conflict(
                        "an unsuccessful host finalization must remain a final failure, not a successful preliminary assessment"
                            .into(),
                    ));
                }
                validate_final_assessment_against_inputs(&evidence, &saved)?;
            }
        }
        let references = [
            evidence.stdout_reference.as_deref(),
            evidence.stderr_reference.as_deref(),
            evidence.report_reference.as_deref(),
        ];
        let mut integrity_verified = true;
        for evidence_id in references.into_iter().flatten() {
            validate_safe_id(evidence_id, "evidence id")?;
            let reference = find_evidence_in_transaction(&transaction, run_id, evidence_id)?
                .ok_or_else(|| {
                    CodingAgentError::Invalid(format!(
                        "execution result refers to evidence not owned by run {run_id}"
                    ))
                })?;
            if reference.attempt_id != attempt_id {
                return Err(CodingAgentError::Invalid(
                    "execution evidence must belong to its attempt".into(),
                ));
            }
            if evidence.stdout_reference.as_deref() == Some(evidence_id)
                && evidence.stdout_bytes != reference.size_bytes
            {
                return Err(CodingAgentError::Invalid(
                    "stdout byte count does not match durable evidence".into(),
                ));
            }
            if evidence.stderr_reference.as_deref() == Some(evidence_id)
                && evidence.stderr_bytes != reference.size_bytes
            {
                return Err(CodingAgentError::Invalid(
                    "stderr byte count does not match durable evidence".into(),
                ));
            }
            if evidence.report_reference.as_deref() == Some(evidence_id)
                && evidence.report_save_confirmed
                && !reference.save_confirmed
            {
                return Err(CodingAgentError::Invalid(
                    "report save confirmation does not match the durable reference".into(),
                ));
            }
            integrity_verified &= self.verify_reference_for_run(run_id, &reference);
        }
        evidence.evidence_integrity_verified &=
            integrity_verified && references.iter().any(Option::is_some);
        let result_json = serde_json::to_string(&evidence)?;
        let existing: Option<String> = transaction
            .query_row(
                "SELECT result_json FROM agent_run_execution_evidence
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), attempt_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing == result_json {
                return Ok(());
            }
            return Err(CodingAgentError::Conflict(
                "final execution evidence for this attempt is immutable".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO agent_run_execution_evidence
                (run_id, attempt_id, result_json, recorded_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                run_id.as_str(),
                attempt_id,
                result_json,
                sql_i64(now_ms(), "execution evidence timestamp")?,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Persist an operation intent. StartAgent is excluded so coordinators use
    /// `start_attempt` and receive an explicit new-versus-replay decision.
    pub fn begin_operation(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        idempotency_key: &str,
        request: DurableOperationRequest,
    ) -> Result<DurableOperationRecord, CodingAgentError> {
        self.begin_operation_with_creation(run_id, attempt_id, idempotency_key, request)
            .map(|(record, _)| record)
    }

    /// Atomically create or observe a durable effect intent. Only a caller
    /// receiving `newly_created == true` may perform the external side effect;
    /// retries must reconcile the existing operation instead of replaying it.
    pub fn claim_effect(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        idempotency_key: &str,
        request: DurableOperationRequest,
    ) -> Result<DurableOperationClaim, CodingAgentError> {
        if matches!(
            &request,
            DurableOperationRequest::PublishBranch { .. }
                | DurableOperationRequest::CreatePullRequest { .. }
        ) {
            return Err(CodingAgentError::Invalid(
                "publication dispatch requires claim_publication_effect and its sealed permit"
                    .into(),
            ));
        }
        let (operation, newly_created) =
            self.begin_operation_with_creation(run_id, attempt_id, idempotency_key, request)?;
        Ok(DurableOperationClaim {
            operation,
            newly_created,
        })
    }

    /// Atomically admits the sole remote publication effect for one attempt.
    /// `expected_revision` must be the current revision while the run is in
    /// `Publishing`; `request` must be `PublishBranch` or `CreatePullRequest`.
    /// Only `(newly_created, Some(permit))` authorizes one adapter dispatch.
    /// Replays return only the existing operation record and must reconcile it,
    /// never dispatch it again or select a fresh idempotency key.
    pub fn claim_publication_effect(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        attempt_id: &str,
        idempotency_key: &str,
        request: DurableOperationRequest,
    ) -> Result<PublicationEffectClaim, CodingAgentError> {
        if !matches!(
            &request,
            DurableOperationRequest::PublishBranch { .. }
                | DurableOperationRequest::CreatePullRequest { .. }
        ) {
            return Err(CodingAgentError::Invalid(
                "publication admission requires a branch or pull-request request".into(),
            ));
        }
        validate_safe_id(attempt_id, "attempt id")?;
        validate_idempotency_key(idempotency_key)?;
        validate_operation_request(&request)?;
        let payload_hash = request.payload_hash()?;
        let request_json = serde_json::to_string(&request)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.ensure_attempt_in_transaction(&transaction, run_id, attempt_id)?;
        let existing = transaction
            .query_row(
                "SELECT operation_id, attempt_id, kind_json, payload_hash, request_json,
                        status_json, result_json, created_at_ms, updated_at_ms
                 FROM agent_run_operations WHERE run_id = ?1 AND idempotency_key = ?2",
                params![run_id.as_str(), idempotency_key],
                operation_from_row(run_id, idempotency_key),
            )
            .optional()?;
        if let Some(operation) = existing {
            if operation.attempt_id != attempt_id
                || operation.payload_hash != payload_hash
                || operation.request != request
                || !matches!(
                    operation.kind,
                    super::model::DurableOperationKind::PublishBranch
                        | super::model::DurableOperationKind::CreatePullRequest
                )
            {
                return Err(CodingAgentError::Conflict(
                    "publication idempotency key was used for another attempt or request".into(),
                ));
            }
            transaction.commit()?;
            return Ok(PublicationEffectClaim {
                operation,
                newly_created: false,
                permit: None,
            });
        }

        let (revision, state_json, current_attempt_id, phase_generation): (
            i64,
            String,
            Option<String>,
            i64,
        ) = transaction
            .query_row(
                "SELECT revision, state_json, current_attempt_id, start_phase_generation
                 FROM agent_runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
        let revision = nonnegative_u64(revision, "run revision")?;
        let phase_generation = nonnegative_u64(phase_generation, "start phase generation")?;
        if revision != expected_revision
            || decode::<RunState>(&state_json, "run state")? != RunState::Publishing
            || current_attempt_id.as_deref() != Some(attempt_id)
        {
            return Err(CodingAgentError::Conflict(
                "publication admission does not match the current run revision and attempt".into(),
            ));
        }
        ensure_no_dispatched_completion(&transaction, run_id)?;
        ensure_no_attempt_cancellation_effect(&transaction, run_id, attempt_id)?;
        self.validate_operation_for_run(&transaction, run_id, attempt_id, &request)?;
        let other_publication_exists: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM agent_run_operations
                 WHERE run_id = ?1 AND attempt_id = ?2
                   AND kind_json IN (?3, ?4)
             )",
            params![
                run_id.as_str(),
                attempt_id,
                serde_json::to_string(&super::model::DurableOperationKind::PublishBranch)?,
                serde_json::to_string(&super::model::DurableOperationKind::CreatePullRequest)?,
            ],
            |row| row.get(0),
        )?;
        if other_publication_exists {
            return Err(CodingAgentError::Conflict(
                "this attempt already has an immutable publication intent".into(),
            ));
        }

        let now = now_ms();
        let operation_id = RunId::generate()?.to_string();
        let operation = DurableOperationRecord {
            operation_id: operation_id.clone(),
            run_id: run_id.clone(),
            attempt_id: attempt_id.into(),
            idempotency_key: idempotency_key.into(),
            kind: request.kind(),
            payload_hash: payload_hash.clone(),
            request,
            status: DurableOperationStatus::Intent,
            result: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        transaction.execute(
            "INSERT INTO agent_run_operations
                (operation_id, run_id, attempt_id, phase_generation, idempotency_key, kind_json,
                 payload_hash, request_json, status_json, result_json, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?10)",
            params![
                operation_id,
                run_id.as_str(),
                attempt_id,
                sql_i64(phase_generation, "publication phase generation")?,
                idempotency_key,
                serde_json::to_string(&operation.kind)?,
                payload_hash,
                request_json,
                serde_json::to_string(&DurableOperationStatus::Intent)?,
                sql_i64(now, "operation timestamp")?,
            ],
        )?;
        transaction.commit()?;
        Ok(PublicationEffectClaim {
            operation,
            newly_created: true,
            permit: Some(PublicationEffectPermit {
                run_id: run_id.clone(),
                attempt_id: attempt_id.into(),
                idempotency_key: idempotency_key.into(),
                operation_id,
                payload_hash,
                phase_generation,
                expected_revision,
                store_identity: self.store_identity.clone(),
            }),
        })
    }

    /// Atomically records the one publication response authorized by `permit`.
    /// The adapter should first persist any receipt bytes as evidence for the
    /// same attempt and include those evidence IDs in `result`. If a cancellation
    /// intent or any run/attempt/revision change won the SQLite transaction race,
    /// an accepted response is retained as `ResultUnconfirmed` and cannot unlock
    /// review or auto-completion.
    pub(crate) fn confirm_publication_lookup(
        &self,
        run: &AgentRunRecord,
        expected: &DurableOperationRecord,
        result: DurableOperationResult,
    ) -> Result<DurableOperationRecord, CodingAgentError> {
        validate_operation_result(&result)?;
        if result.outcome != DurableOperationOutcome::Succeeded
            || result.evidence_ids.len() != 1
            || !matches!(
                expected.request,
                DurableOperationRequest::PublishBranch { .. }
                    | DurableOperationRequest::CreatePullRequest { .. }
            )
        {
            return Err(CodingAgentError::Invalid(
                "invalid publication lookup result".into(),
            ));
        }
        let run_id = &run.snapshot.run_id;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = transaction.query_row(
            "SELECT current_attempt_id, revision, state_json FROM agent_runs WHERE run_id = ?1",
            [run_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        if current.0.as_deref() != Some(expected.attempt_id.as_str())
            || nonnegative_u64(current.1, "run revision")? != run.revision
            || ![RunState::Publishing, RunState::Unknown]
                .iter()
                .any(|state| serde_json::to_string(state).is_ok_and(|value| value == current.2))
            || has_attempt_cancellation_effect(&transaction, run_id, &expected.attempt_id)?
        {
            return Err(CodingAgentError::Conflict(
                "publication lookup lost its run scope".into(),
            ));
        }
        let operation = transaction.query_row(
            "SELECT operation_id, attempt_id, kind_json, payload_hash, request_json,
                    status_json, result_json, created_at_ms, updated_at_ms
             FROM agent_run_operations WHERE run_id = ?1 AND idempotency_key = ?2",
            params![run_id.as_str(), expected.idempotency_key],
            operation_from_row(run_id, &expected.idempotency_key),
        )?;
        if operation.operation_id != expected.operation_id
            || operation.attempt_id != expected.attempt_id
            || operation.payload_hash != expected.payload_hash
            || operation.request != expected.request
        {
            return Err(CodingAgentError::Conflict(
                "publication lookup intent changed".into(),
            ));
        }
        if operation.status == DurableOperationStatus::Completed {
            if operation.result.as_ref() == Some(&result) {
                transaction.commit()?;
                return Ok(operation);
            }
            return Err(CodingAgentError::Conflict(
                "publication result already recorded".into(),
            ));
        }
        let reference =
            find_evidence_in_transaction(&transaction, run_id, &result.evidence_ids[0])?
                .ok_or_else(|| {
                    CodingAgentError::Invalid("publication lookup evidence missing".into())
                })?;
        if reference.attempt_id != expected.attempt_id
            || reference.name != "publication-receipt"
            || result.result_digest.as_deref() != Some(reference.sha256.as_str())
            || !self.verify_reference_for_run(run_id, &reference)
        {
            return Err(CodingAgentError::Invalid(
                "publication lookup receipt is not durable".into(),
            ));
        }
        transaction.execute(
            "UPDATE agent_run_operations SET status_json = ?1, result_json = ?2, updated_at_ms = ?3
             WHERE operation_id = ?4 AND status_json = ?5",
            params![
                serde_json::to_string(&DurableOperationStatus::Completed)?,
                serde_json::to_string(&result)?,
                sql_i64(now_ms(), "operation timestamp")?,
                operation.operation_id,
                serde_json::to_string(&operation.status)?
            ],
        )?;
        transaction.commit()?;
        drop(connection);
        self.operation(run_id, &expected.idempotency_key)?
            .ok_or_else(|| CodingAgentError::Corrupt("publication operation disappeared".into()))
    }

    pub fn complete_publication_effect(
        &self,
        permit: PublicationEffectPermit,
        mut result: DurableOperationResult,
    ) -> Result<DurableOperationRecord, CodingAgentError> {
        if !Arc::ptr_eq(&permit.store_identity, &self.store_identity) {
            return Err(CodingAgentError::Conflict(
                "publication permit belongs to another RunStore instance".into(),
            ));
        }
        validate_operation_result(&result)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let operation = transaction
            .query_row(
                "SELECT operation_id, attempt_id, kind_json, payload_hash, request_json,
                        status_json, result_json, created_at_ms, updated_at_ms
                 FROM agent_run_operations WHERE run_id = ?1 AND idempotency_key = ?2",
                params![permit.run_id.as_str(), permit.idempotency_key],
                operation_from_row(&permit.run_id, &permit.idempotency_key),
            )
            .optional()?
            .ok_or_else(|| {
                CodingAgentError::NotFound(format!("publication {}", permit.idempotency_key))
            })?;
        if operation.operation_id != permit.operation_id
            || operation.attempt_id != permit.attempt_id
            || operation.payload_hash != permit.payload_hash
            || operation.kind
                != match &operation.request {
                    DurableOperationRequest::PublishBranch { .. } => {
                        super::model::DurableOperationKind::PublishBranch
                    }
                    DurableOperationRequest::CreatePullRequest { .. } => {
                        super::model::DurableOperationKind::CreatePullRequest
                    }
                    _ => {
                        return Err(CodingAgentError::Conflict(
                            "publication permit refers to a non-publication operation".into(),
                        ));
                    }
                }
        {
            return Err(CodingAgentError::Conflict(
                "publication permit does not match its immutable operation".into(),
            ));
        }
        if operation.status == DurableOperationStatus::Completed {
            // A remote lookup may have confirmed this intent while its original
            // dispatch was still awaiting a response. Preserve the durable fact.
            transaction.commit()?;
            return Ok(operation);
        }
        let current_attempt = transaction
            .query_row(
                "SELECT current_attempt_id, revision, state_json, start_phase_generation
                 FROM agent_runs WHERE run_id = ?1",
                [permit.run_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| CodingAgentError::NotFound(format!("run {}", permit.run_id)))?;
        let current_revision = nonnegative_u64(current_attempt.1, "run revision")?;
        let current_phase_generation =
            nonnegative_u64(current_attempt.3, "start phase generation")?;
        let cancellation_fence =
            has_attempt_cancellation_effect(&transaction, &permit.run_id, &permit.attempt_id)?;
        let still_admitted_phase = current_attempt.0.as_deref() == Some(&permit.attempt_id)
            && current_revision == permit.expected_revision
            && current_attempt.2 == serde_json::to_string(&RunState::Publishing)?
            && current_phase_generation == permit.phase_generation;
        if result.outcome == DurableOperationOutcome::Succeeded
            && (cancellation_fence || !still_admitted_phase)
        {
            result.outcome = DurableOperationOutcome::ResultUnconfirmed;
        }
        if operation.status == DurableOperationStatus::Completed
            || operation.status == DurableOperationStatus::ResultUnconfirmed
        {
            if operation.result.as_ref() == Some(&result) {
                transaction.commit()?;
                return Ok(operation);
            }
            return Err(CodingAgentError::Conflict(
                "publication result is already durably recorded".into(),
            ));
        }
        if operation.status != DurableOperationStatus::Intent {
            return Err(CodingAgentError::Conflict(
                "publication permit no longer owns an undispatched intent".into(),
            ));
        }
        for evidence_id in &result.evidence_ids {
            let reference =
                find_evidence_in_transaction(&transaction, &permit.run_id, evidence_id)?
                    .ok_or_else(|| {
                        CodingAgentError::Invalid(format!(
                            "publication result references evidence not owned by run {}",
                            permit.run_id
                        ))
                    })?;
            if reference.attempt_id != permit.attempt_id
                || !self.verify_reference_for_run(&permit.run_id, &reference)
            {
                return Err(CodingAgentError::Invalid(
                    "publication receipt evidence must be durable and belong to its attempt".into(),
                ));
            }
        }
        let status = if result.outcome == DurableOperationOutcome::ResultUnconfirmed {
            DurableOperationStatus::ResultUnconfirmed
        } else {
            DurableOperationStatus::Completed
        };
        transaction.execute(
            "UPDATE agent_run_operations SET status_json = ?1, result_json = ?2, updated_at_ms = ?3
             WHERE operation_id = ?4 AND status_json = ?5",
            params![
                serde_json::to_string(&status)?,
                serde_json::to_string(&result)?,
                sql_i64(now_ms(), "operation timestamp")?,
                permit.operation_id,
                serde_json::to_string(&operation.status)?,
            ],
        )?;
        transaction.commit()?;
        drop(connection);
        self.operation(&permit.run_id, &permit.idempotency_key)?
            .ok_or_else(|| CodingAgentError::Corrupt("publication operation disappeared".into()))
    }

    fn begin_operation_with_creation(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        idempotency_key: &str,
        request: DurableOperationRequest,
    ) -> Result<(DurableOperationRecord, bool), CodingAgentError> {
        if matches!(&request, DurableOperationRequest::StartAgent { .. }) {
            return Err(CodingAgentError::Invalid(
                "StartAgent intents must use the transactional start_attempt API".into(),
            ));
        }
        if matches!(
            &request,
            DurableOperationRequest::PublishBranch { .. }
                | DurableOperationRequest::CreatePullRequest { .. }
        ) {
            return Err(CodingAgentError::Invalid(
                "publication intents must use claim_publication_effect".into(),
            ));
        }
        validate_safe_id(attempt_id, "attempt id")?;
        validate_idempotency_key(idempotency_key)?;
        validate_operation_request(&request)?;
        let payload_hash = request.payload_hash()?;
        let request_json = serde_json::to_string(&request)?;
        let now = now_ms();
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.ensure_attempt_in_transaction(&transaction, run_id, attempt_id)?;
        let existing = transaction
            .query_row(
                "SELECT operation_id, attempt_id, kind_json, payload_hash, request_json,
                        status_json, result_json, created_at_ms, updated_at_ms
                 FROM agent_run_operations WHERE run_id = ?1 AND idempotency_key = ?2",
                params![run_id.as_str(), idempotency_key],
                operation_from_row(run_id, idempotency_key),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing.payload_hash != payload_hash || existing.attempt_id != attempt_id {
                return Err(CodingAgentError::Conflict(
                    "idempotency key was already used by a different attempt or request".into(),
                ));
            }
            return Ok((existing, false));
        }
        ensure_no_dispatched_completion(&transaction, run_id)?;
        self.validate_operation_for_run(&transaction, run_id, attempt_id, &request)?;

        let phase_generation: i64 = transaction.query_row(
            "SELECT start_phase_generation FROM agent_runs WHERE run_id = ?1",
            [run_id.as_str()],
            |row| row.get(0),
        )?;
        let phase_generation = nonnegative_u64(phase_generation, "start phase generation")?;
        if matches!(&request, DurableOperationRequest::FinishInteractive { .. })
            && has_phase_cancellation_effect(&transaction, run_id, attempt_id, phase_generation)?
        {
            return Err(CodingAgentError::Conflict(
                "interactive finish is fenced by a cancellation claim in this phase".into(),
            ));
        }

        let operation_id = RunId::generate()?.to_string();
        transaction
            .execute(
                "INSERT INTO agent_run_operations
                (operation_id, run_id, attempt_id, phase_generation, idempotency_key, kind_json,
                 payload_hash, request_json, status_json, result_json, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?10)",
                params![
                    operation_id,
                    run_id.as_str(),
                    attempt_id,
                    sql_i64(phase_generation, "start phase generation")?,
                    idempotency_key,
                    serde_json::to_string(&request.kind())?,
                    payload_hash,
                    request_json,
                    serde_json::to_string(&DurableOperationStatus::Intent)?,
                    sql_i64(now, "operation timestamp")?,
                ],
            )
            .map_err(|error| match &error {
                rusqlite::Error::SqliteFailure(code, _)
                    if code.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    CodingAgentError::Conflict(
                        "this lifecycle effect is already claimed in the current phase".into(),
                    )
                }
                _ => CodingAgentError::Sqlite(error),
            })?;
        let record = DurableOperationRecord {
            operation_id,
            run_id: run_id.clone(),
            attempt_id: attempt_id.into(),
            idempotency_key: idempotency_key.into(),
            kind: request.kind(),
            payload_hash,
            request,
            status: DurableOperationStatus::Intent,
            result: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        transaction.commit()?;
        Ok((record, true))
    }

    pub fn complete_operation(
        &self,
        run_id: &RunId,
        idempotency_key: &str,
        result: DurableOperationResult,
    ) -> Result<DurableOperationRecord, CodingAgentError> {
        validate_idempotency_key(idempotency_key)?;
        validate_operation_result(&result)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = transaction
            .query_row(
                "SELECT operation_id, attempt_id, kind_json, payload_hash, request_json,
                        status_json, result_json, created_at_ms, updated_at_ms
                 FROM agent_run_operations WHERE run_id = ?1 AND idempotency_key = ?2",
                params![run_id.as_str(), idempotency_key],
                operation_from_row(run_id, idempotency_key),
            )
            .optional()?
            .ok_or_else(|| CodingAgentError::NotFound(format!("operation {idempotency_key}")))?;
        if matches!(
            existing.kind,
            super::model::DurableOperationKind::PublishBranch
                | super::model::DurableOperationKind::CreatePullRequest
        ) {
            return Err(CodingAgentError::Conflict(
                "publication results require their sealed PublicationEffectPermit".into(),
            ));
        }
        if existing.status == DurableOperationStatus::Completed {
            if existing.result.as_ref() == Some(&result) {
                return Ok(existing);
            }
            return Err(CodingAgentError::Conflict(
                "completed operation cannot be replaced with a different result".into(),
            ));
        }
        let result_json = serde_json::to_string(&result)?;
        ensure_no_dispatched_completion(&transaction, run_id)?;
        for evidence_id in &result.evidence_ids {
            let reference = find_evidence_in_transaction(&transaction, run_id, evidence_id)?
                .ok_or_else(|| {
                    CodingAgentError::Invalid(format!(
                        "operation result references evidence not owned by run {run_id}"
                    ))
                })?;
            if reference.attempt_id != existing.attempt_id {
                return Err(CodingAgentError::Invalid(
                    "operation result evidence must belong to its attempt".into(),
                ));
            }
        }
        let status = if result.outcome == DurableOperationOutcome::ResultUnconfirmed {
            DurableOperationStatus::ResultUnconfirmed
        } else {
            DurableOperationStatus::Completed
        };
        let updated_at = now_ms();
        transaction.execute(
            "UPDATE agent_run_operations SET status_json = ?1, result_json = ?2, updated_at_ms = ?3
             WHERE operation_id = ?4",
            params![
                serde_json::to_string(&status)?,
                result_json,
                sql_i64(updated_at, "operation timestamp")?,
                existing.operation_id,
            ],
        )?;
        transaction.commit()?;
        drop(connection);
        self.operation(run_id, idempotency_key)?
            .ok_or_else(|| CodingAgentError::Corrupt("completed operation disappeared".into()))
    }

    pub fn operation(
        &self,
        run_id: &RunId,
        idempotency_key: &str,
    ) -> Result<Option<DurableOperationRecord>, CodingAgentError> {
        let connection = self.lock_connection()?;
        connection
            .query_row(
                "SELECT operation_id, attempt_id, kind_json, payload_hash, request_json,
                        status_json, result_json, created_at_ms, updated_at_ms
                 FROM agent_run_operations WHERE run_id = ?1 AND idempotency_key = ?2",
                params![run_id.as_str(), idempotency_key],
                operation_from_row(run_id, idempotency_key),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn unresolved_operations(&self) -> Result<Vec<DurableOperationRecord>, CodingAgentError> {
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(
            "SELECT operation_id, run_id, attempt_id, idempotency_key, kind_json, payload_hash,
                    request_json, status_json, result_json, created_at_ms, updated_at_ms
             FROM agent_run_operations ORDER BY created_at_ms",
        )?;
        let rows = statement.query_map([], |row| {
            let run_id: String = row.get(1)?;
            Ok((
                run_id,
                row.get::<_, String>(0)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|row| {
                let status = decode::<DurableOperationStatus>(&row.7, "operation status");
                match status {
                    Ok(DurableOperationStatus::Completed) => None,
                    Ok(_) => Some(operation_tuple_to_record(row)),
                    Err(error) => Some(Err(error)),
                }
            })
            .collect()
    }

    fn find_evidence(
        &self,
        run_id: &RunId,
        evidence_id: &str,
    ) -> Result<Option<EvidenceReference>, CodingAgentError> {
        let connection = self.lock_connection()?;
        find_evidence_in_connection(&connection, run_id, evidence_id)
    }

    fn evidence_path(&self, run_id: &RunId, evidence_id: &str) -> PathBuf {
        self.evidence_root
            .join(run_id.as_str())
            .join("evidence")
            .join(format!("{evidence_id}.blob"))
    }

    fn verify_assessment(
        &self,
        transaction: &Transaction<'_>,
        run_id: &RunId,
        mut evidence: RunEvidenceAssessment,
    ) -> Result<RunEvidenceAssessment, CodingAgentError> {
        let ids = [
            evidence.stdout_reference.as_deref(),
            evidence.stderr_reference.as_deref(),
            evidence.report_reference.as_deref(),
        ];
        let mut integrity = ids.iter().any(|id| id.is_some());
        let mut report_confirmed = false;
        for evidence_id in ids.into_iter().flatten() {
            let Some(reference) = find_evidence_in_transaction(transaction, run_id, evidence_id)?
            else {
                integrity = false;
                continue;
            };
            integrity &= self.verify_reference_for_run(run_id, &reference);
            if evidence.report_reference.as_deref() == Some(evidence_id) {
                report_confirmed = reference.save_confirmed;
            }
        }
        evidence.evidence_integrity_verified &= integrity;
        evidence.report_save_confirmed &= report_confirmed;
        Ok(evidence)
    }

    fn require_finish_request(
        &self,
        transaction: &Transaction<'_>,
        run_id: &RunId,
        attempt_id: &str,
        snapshot: &AgentRunSnapshot,
    ) -> Result<InteractiveFinishRequest, CodingAgentError> {
        let current_attempt_id: Option<String> = transaction.query_row(
            "SELECT current_attempt_id FROM agent_runs WHERE run_id = ?1",
            [run_id.as_str()],
            |row| row.get(0),
        )?;
        if snapshot.execution_mode != super::model::ExecutionMode::Interactive
            || current_attempt_id.as_deref() != Some(attempt_id)
        {
            return Err(CodingAgentError::Conflict(
                "finish request is not bound to the current interactive attempt".into(),
            ));
        }
        let row: Option<(i64, i64)> = transaction
            .query_row(
                "SELECT profile_version, requested_at_ms
                 FROM agent_run_interactive_finish_requests
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), attempt_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((profile_version, requested_at_ms)) = row else {
            return Err(CodingAgentError::Conflict(
                "interactive finish request is not persisted".into(),
            ));
        };
        let request = InteractiveFinishRequest {
            run_id: run_id.clone(),
            attempt_id: attempt_id.into(),
            profile_version: nonnegative_u64(profile_version, "finish profile version")?,
            requested_at_ms: nonnegative_u64(requested_at_ms, "finish request timestamp")?,
        };
        if request.profile_version != snapshot.profile.version {
            return Err(CodingAgentError::Conflict(
                "finish request profile differs from the immutable snapshot".into(),
            ));
        }
        Ok(request)
    }

    fn require_interactive_finalization(
        &self,
        transaction: &Transaction<'_>,
        run_id: &RunId,
        attempt_id: &str,
        snapshot: &AgentRunSnapshot,
        assessment: &RunEvidenceAssessment,
    ) -> Result<(), CodingAgentError> {
        self.require_finish_request(transaction, run_id, attempt_id, snapshot)?;
        let evidence_json: Option<String> = transaction
            .query_row(
                "SELECT evidence_json FROM agent_run_interactive_finalizations
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), attempt_id],
                |row| row.get(0),
            )
            .optional()?;
        let evidence_json = evidence_json.ok_or_else(|| {
            CodingAgentError::Conflict(
                "host-observed interactive finalization is not recorded".into(),
            )
        })?;
        let evidence: StoredInteractiveFinalizationEvidence =
            decode(&evidence_json, "interactive finalization")?;
        let evidence = evidence.into_public()?;
        if !evidence.is_successful_for(snapshot.profile.version, assessment) {
            return Err(CodingAgentError::Conflict(
                "interactive finalization does not prove normal exit and a matching frozen commit"
                    .into(),
            ));
        }
        Ok(())
    }

    fn verify_reference_for_run(&self, run_id: &RunId, reference: &EvidenceReference) -> bool {
        let Ok(bytes) = read_regular_nofollow(&self.evidence_path(run_id, &reference.evidence_id))
        else {
            return false;
        };
        bytes.len() as u64 == reference.size_bytes
            && format!("{:x}", Sha256::digest(bytes)) == reference.sha256
    }

    fn validate_operation_for_run(
        &self,
        transaction: &Transaction<'_>,
        run_id: &RunId,
        attempt_id: &str,
        request: &DurableOperationRequest,
    ) -> Result<(), CodingAgentError> {
        let (snapshot_json, state_json, current_attempt_id): (String, String, Option<String>) =
            transaction
                .query_row(
                    "SELECT snapshot_json, state_json, current_attempt_id
                 FROM agent_runs WHERE run_id = ?1",
                    [run_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
        let snapshot = decode_agent_run_snapshot(&snapshot_json)?;
        let state: RunState = decode(&state_json, "run state")?;
        let allowed = |operation| snapshot.allowed_operations.contains(&operation);
        match request {
            DurableOperationRequest::PrepareWorkspace {
                repository,
                base_commit,
                branch,
            } => {
                if state != RunState::Preparing
                    || repository != &snapshot.repository.identity
                    || base_commit != &snapshot.base_commit
                    || branch != &snapshot.workspace_branch
                {
                    return Err(CodingAgentError::Conflict(
                        "workspace intent differs from the approved run snapshot".into(),
                    ));
                }
                let requested_ordinal = attempt_ordinal(transaction, run_id, attempt_id)?;
                if requested_ordinal
                    != expected_next_attempt_ordinal(
                        transaction,
                        run_id,
                        current_attempt_id.as_deref(),
                    )?
                {
                    return Err(CodingAgentError::Conflict(
                        "workspace preparation must target the next attempt".into(),
                    ));
                }
            }
            DurableOperationRequest::StartAgent {
                workflow_definition_hash,
                attempt_id: requested_attempt,
                execution_mode,
            } => {
                if state != RunState::Preparing
                    || requested_attempt != attempt_id
                    || workflow_definition_hash != &snapshot.workflow.definition_hash
                    || execution_mode != &snapshot.execution_mode
                {
                    return Err(CodingAgentError::Conflict(
                        "agent intent differs from the approved run snapshot or stage".into(),
                    ));
                }
                let requested_ordinal = attempt_ordinal(transaction, run_id, requested_attempt)?;
                if requested_ordinal
                    != expected_next_attempt_ordinal(
                        transaction,
                        run_id,
                        current_attempt_id.as_deref(),
                    )?
                {
                    return Err(CodingAgentError::Conflict(
                        "agent attempt must advance monotonically from the current attempt".into(),
                    ));
                }
            }
            DurableOperationRequest::ReassessSemantics {
                expected_run_revision,
                report_reference,
                ..
            } => {
                let revision: i64 = transaction.query_row(
                    "SELECT revision FROM agent_runs WHERE run_id = ?1",
                    [run_id.as_str()],
                    |row| row.get(0),
                )?;
                if state != RunState::AwaitingReview
                    || current_attempt_id.as_deref() != Some(attempt_id)
                    || nonnegative_u64(revision, "run revision")? != *expected_run_revision
                    || !snapshot.semantic_judge_enabled
                    || snapshot.approval.is_none()
                    || snapshot.execution_mode != super::model::ExecutionMode::OneShot
                {
                    return Err(CodingAgentError::Conflict("human reassessment requires the current approved semantic run awaiting review".into()));
                }
                ensure_no_attempt_cancellation_effect(transaction, run_id, attempt_id)?;
                let recorded = required_execution_evidence(transaction, run_id, attempt_id)?;
                if recorded.report_reference.as_deref() != Some(report_reference) {
                    return Err(CodingAgentError::Conflict(
                        "human reassessment report does not match the current attempt".into(),
                    ));
                }
            }
            DurableOperationRequest::VerifyEvidence {
                evidence_ids,
                commit_id,
            } => {
                if state != RunState::Verifying || !allowed(AllowedOperation::Verify) {
                    return Err(CodingAgentError::Conflict(
                        "verification is not approved at the current run stage".into(),
                    ));
                }
                if current_attempt_id.as_deref() != Some(attempt_id) {
                    return Err(CodingAgentError::Conflict(
                        "verification intent must target the current attempt".into(),
                    ));
                }
                for evidence_id in evidence_ids {
                    let reference = find_evidence_in_transaction(transaction, run_id, evidence_id)?
                        .ok_or_else(|| {
                            CodingAgentError::Invalid(
                                "verification intent references unknown evidence".into(),
                            )
                        })?;
                    if reference.attempt_id != attempt_id {
                        return Err(CodingAgentError::Invalid(
                            "verification evidence must belong to its attempt".into(),
                        ));
                    }
                }
                if let Some(commit_id) = commit_id {
                    let recorded = required_execution_evidence(transaction, run_id, attempt_id)?;
                    if recorded.commit_id.as_deref() != Some(commit_id.as_str()) {
                        return Err(CodingAgentError::Conflict(
                            "verification commit differs from recorded execution evidence".into(),
                        ));
                    }
                }
            }
            DurableOperationRequest::PublishBranch { branch, commit_id } => {
                if state != RunState::Publishing
                    || !matches!(
                        snapshot.publication_plan,
                        PublicationPlan::PushBranch | PublicationPlan::CreatePullRequest
                    )
                    || !allowed(AllowedOperation::PublishBranch)
                    || branch != &snapshot.workspace_branch
                    || current_attempt_id.as_deref() != Some(attempt_id)
                {
                    return Err(CodingAgentError::Conflict(
                        "branch publication intent is outside the approved plan".into(),
                    ));
                }
                self.require_publication_gate(
                    transaction,
                    run_id,
                    attempt_id,
                    &snapshot,
                    commit_id,
                )?;
            }
            DurableOperationRequest::CreatePullRequest {
                branch, commit_id, ..
            } => {
                if state != RunState::Publishing
                    || snapshot.publication_plan != PublicationPlan::CreatePullRequest
                    || !allowed(AllowedOperation::PublishBranch)
                    || !allowed(AllowedOperation::CreatePullRequest)
                    || branch != &snapshot.workspace_branch
                    || current_attempt_id.as_deref() != Some(attempt_id)
                {
                    return Err(CodingAgentError::Conflict(
                        "pull request intent is outside the approved plan".into(),
                    ));
                }
                self.require_publication_gate(
                    transaction,
                    run_id,
                    attempt_id,
                    &snapshot,
                    commit_id,
                )?;
            }
            DurableOperationRequest::UpdateIssueState {
                expected_revision,
                target_state,
            } => {
                if !allowed(AllowedOperation::IssueStateUpdate)
                    || *expected_revision != snapshot.issue_revision
                    || current_attempt_id.as_deref() != Some(attempt_id)
                {
                    return Err(CodingAgentError::Conflict(
                        "Issue state intent is not approved for this revision".into(),
                    ));
                }
                if target_state == "DONE" {
                    if state != RunState::AwaitingReview
                        || !snapshot.semantic_judge_enabled
                        || !snapshot.auto_done
                    {
                        return Err(CodingAgentError::Conflict(
                            "auto-DONE intent is not approved for this run".into(),
                        ));
                    }
                    let evidence = required_execution_evidence(transaction, run_id, attempt_id)?;
                    let evidence = self.verify_assessment(transaction, run_id, evidence)?;
                    let decision = evaluate_completion_gates(
                        snapshot.purpose,
                        snapshot.auto_done,
                        snapshot.publication_plan.clone(),
                        &snapshot.allowed_operations,
                        &evidence,
                    );
                    if decision.auto_done != super::model::GateDecision::Allowed {
                        return Err(CodingAgentError::Conflict(format!(
                            "auto-DONE gate is not satisfied: {:?}",
                            decision.auto_done
                        )));
                    }
                    if snapshot.execution_mode == super::model::ExecutionMode::Interactive {
                        self.require_interactive_finalization(
                            transaction,
                            run_id,
                            attempt_id,
                            &snapshot,
                            &evidence,
                        )?;
                    }
                } else if state != RunState::AwaitingReview {
                    return Err(CodingAgentError::Conflict(
                        "Issue state intent requires a run awaiting review".into(),
                    ));
                }
            }
            DurableOperationRequest::CancelAgent => {
                if state.is_terminal() || current_attempt_id.as_deref() != Some(attempt_id) {
                    return Err(CodingAgentError::Conflict(
                        "cancellation intent must target the active run attempt".into(),
                    ));
                }
            }
            DurableOperationRequest::FinishInteractive { profile_version } => {
                if state != RunState::Finalizing
                    || snapshot.execution_mode != super::model::ExecutionMode::Interactive
                    || *profile_version != snapshot.profile.version
                    || current_attempt_id.as_deref() != Some(attempt_id)
                {
                    return Err(CodingAgentError::Conflict(
                        "interactive finish intent differs from the active approved attempt".into(),
                    ));
                }
                let request =
                    self.require_finish_request(transaction, run_id, attempt_id, &snapshot)?;
                if request.profile_version != *profile_version {
                    return Err(CodingAgentError::Conflict(
                        "interactive finish intent differs from the persisted finish request"
                            .into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn require_publication_gate(
        &self,
        transaction: &Transaction<'_>,
        run_id: &RunId,
        attempt_id: &str,
        snapshot: &AgentRunSnapshot,
        requested_commit: &str,
    ) -> Result<(), CodingAgentError> {
        let evidence = required_execution_evidence(transaction, run_id, attempt_id)?;
        let evidence = self.verify_assessment(transaction, run_id, evidence)?;
        if evidence.commit_id.as_deref() != Some(requested_commit) {
            return Err(CodingAgentError::Conflict(
                "publication commit differs from current attempt evidence".into(),
            ));
        }
        let decision = evaluate_completion_gates(
            snapshot.purpose,
            snapshot.auto_done,
            snapshot.publication_plan.clone(),
            &snapshot.allowed_operations,
            &evidence,
        );
        if decision.publication != super::model::GateDecision::Allowed {
            return Err(CodingAgentError::Conflict(format!(
                "publication gate is not satisfied: {:?}",
                decision.publication
            )));
        }
        if snapshot.execution_mode == super::model::ExecutionMode::Interactive {
            self.require_interactive_finalization(
                transaction,
                run_id,
                attempt_id,
                snapshot,
                &evidence,
            )?;
        }
        Ok(())
    }

    fn ensure_current_attempt(
        &self,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<(), CodingAgentError> {
        let connection = self.lock_connection()?;
        ensure_current_attempt_in_connection(&connection, run_id, attempt_id)
    }

    fn ensure_attempt_in_transaction(
        &self,
        transaction: &Transaction<'_>,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<(), CodingAgentError> {
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_run_attempts WHERE run_id = ?1 AND attempt_id = ?2)",
            params![run_id.as_str(), attempt_id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(CodingAgentError::NotFound(format!(
                "attempt {attempt_id} for run {run_id}"
            )));
        }
        Ok(())
    }

    fn ensure_current_attempt_in_transaction(
        &self,
        transaction: &Transaction<'_>,
        run_id: &RunId,
        attempt_id: &str,
    ) -> Result<(), CodingAgentError> {
        let current_attempt_id: Option<String> = transaction.query_row(
            "SELECT current_attempt_id FROM agent_runs WHERE run_id = ?1",
            [run_id.as_str()],
            |row| row.get(0),
        )?;
        if current_attempt_id.as_deref() != Some(attempt_id) {
            return Err(CodingAgentError::Conflict(
                "evidence must belong to the current attempt".into(),
            ));
        }
        Ok(())
    }

    fn completion_candidate_in_transaction(
        &self,
        transaction: &Transaction<'_>,
        run_id: &RunId,
        action: CompletionActionKind,
        allow_done: bool,
    ) -> Result<CompletionCandidate, CodingAgentError> {
        let run = load_run(transaction, run_id)?
            .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
        if run.state != RunState::AwaitingReview && !(allow_done && run.state == RunState::Done) {
            return Err(CodingAgentError::Conflict(
                "completion requires the current run in awaiting-review".into(),
            ));
        }
        let attempt_id = run.current_attempt_id.as_deref().ok_or_else(|| {
            CodingAgentError::Conflict("completion has no selected execution attempt".into())
        })?;
        ensure_no_attempt_cancellation_effect(transaction, run_id, attempt_id)?;
        let evidence = required_execution_evidence(transaction, run_id, attempt_id)?;
        let mut evidence = self.verify_assessment(transaction, run_id, evidence)?;
        if !successful_report_is_durable(&evidence)
            || !evidence.commit_consistent
            || match evidence.commit_id.as_deref() {
                Some(commit) => !valid_commit_id(commit),
                None => run.snapshot.purpose != RunPurpose::Investigation,
            }
        {
            return Err(CodingAgentError::Conflict(
                "completion requires successful durable report and full-commit evidence".into(),
            ));
        }
        let human_semantic_issue_revision = if matches!(
            action,
            CompletionActionKind::HumanDone | CompletionActionKind::AutoDone
        ) {
            self.human_semantic_completion_revision(transaction, &run, &evidence)?
        } else {
            None
        };
        if action == CompletionActionKind::AutoDone && human_semantic_issue_revision.is_some() {
            // Use the separately verified judgment without rewriting the original assessment.
            evidence.acceptance_conditions_met = true;
        }
        if action == CompletionActionKind::AutoDone {
            let approval = run.snapshot.approval.as_ref().ok_or_else(|| {
                CodingAgentError::Conflict("auto-completion has no immutable approval".into())
            })?;
            if !run.snapshot.semantic_judge_enabled
                || !run.snapshot.auto_done
                || !approval.auto_done_approved
                || approval.settings_version.is_none()
                || approval.approved_at_ms == 0
                || approval.approved_by.trim().is_empty()
                || !approval
                    .applicable_modes
                    .contains(&run.snapshot.execution_mode)
            {
                return Err(CodingAgentError::Conflict(
                    "auto-completion is not explicitly approved by the run snapshot".into(),
                ));
            }
            let decision = evaluate_completion_gates(
                run.snapshot.purpose,
                run.snapshot.auto_done,
                run.snapshot.publication_plan.clone(),
                &run.snapshot.allowed_operations,
                &evidence,
            );
            if decision.auto_done != super::model::GateDecision::Allowed {
                return Err(CodingAgentError::Conflict(format!(
                    "auto-completion evidence gate is not satisfied: {:?}",
                    decision.auto_done
                )));
            }
            self.require_completion_publication_receipt(
                transaction,
                run_id,
                attempt_id,
                &run.snapshot,
                &evidence,
            )?;
        }
        if action == CompletionActionKind::HumanDone
            && ((run.snapshot.semantic_judge_enabled
                && !evidence.acceptance_conditions_met
                && human_semantic_issue_revision.is_none())
                || (evidence.validation.required
                    && evidence.validation.state != ValidationState::Passed))
        {
            return Err(CodingAgentError::Conflict(
                "human completion requires verified acceptance and required validation".into(),
            ));
        }
        if run.snapshot.execution_mode == super::model::ExecutionMode::Interactive {
            self.require_interactive_finalization(
                transaction,
                run_id,
                attempt_id,
                &run.snapshot,
                &evidence,
            )?;
        }

        let references = load_evidence(transaction, run_id)?
            .into_iter()
            .filter(|reference| reference.attempt_id == attempt_id)
            .collect::<Vec<_>>();
        if references.is_empty()
            || references
                .iter()
                .any(|reference| !self.verify_reference_for_run(run_id, reference))
        {
            return Err(CodingAgentError::Conflict(
                "completion evidence manifest is missing or no longer durable".into(),
            ));
        }
        let snapshot_bytes = serde_json::to_vec(&run.snapshot)?;
        let evidence_bytes = serde_json::to_vec(&(&evidence, &references))?;
        let commit_id = evidence
            .commit_id
            .clone()
            .unwrap_or_else(|| run.snapshot.base_commit.clone());
        let (marker_evidence_references, marker_revision) =
            self.completion_marker_references(&run, attempt_id, &references, &commit_id)?;
        Ok(CompletionCandidate {
            run,
            assessment: evidence,
            commit_id,
            marker_evidence_references,
            marker_revision,
            human_semantic_issue_revision,
            snapshot_sha256: format!("{:x}", Sha256::digest(snapshot_bytes)),
            evidence_sha256: format!("{:x}", Sha256::digest(evidence_bytes)),
        })
    }

    fn human_semantic_completion_revision(
        &self,
        transaction: &Transaction<'_>,
        run: &AgentRunRecord,
        evidence: &RunEvidenceAssessment,
    ) -> Result<Option<u64>, CodingAgentError> {
        let attempt_id = run.current_attempt_id.as_deref().ok_or_else(|| {
            CodingAgentError::Conflict("human reassessment has no current attempt".into())
        })?;
        let run_id = &run.snapshot.run_id;
        let key: Option<String> = transaction.query_row(
            "SELECT idempotency_key FROM agent_run_operations WHERE run_id = ?1 AND attempt_id = ?2 AND kind_json = ?3 ORDER BY rowid DESC LIMIT 1",
            params![run_id.as_str(), attempt_id, serde_json::to_string(&super::model::DurableOperationKind::ReassessSemantics)?],
            |row| row.get(0),
        ).optional()?;
        let Some(key) = key else { return Ok(None) };
        let operation = transaction.query_row(
            "SELECT operation_id, attempt_id, kind_json, payload_hash, request_json, status_json, result_json, created_at_ms, updated_at_ms FROM agent_run_operations WHERE run_id = ?1 AND idempotency_key = ?2",
            params![run_id.as_str(), key], operation_from_row(run_id, &key),
        )?;
        let DurableOperationRequest::ReassessSemantics {
            expected_run_revision,
            issue_revision,
            source_sha256,
            report_reference,
        } = &operation.request
        else {
            return Err(CodingAgentError::Conflict(
                "human reassessment operation is invalid".into(),
            ));
        };
        let result = operation
            .result
            .as_ref()
            .filter(|result| {
                operation.status == DurableOperationStatus::Completed
                    && result.outcome == DurableOperationOutcome::Succeeded
                    && result.evidence_ids.len() == 1
            })
            .ok_or_else(|| {
                CodingAgentError::Conflict(
                    "latest human reassessment result is not confirmed".into(),
                )
            })?;
        let reference = find_evidence_in_transaction(transaction, run_id, &result.evidence_ids[0])?
            .ok_or_else(|| {
                CodingAgentError::Conflict("human reassessment result is missing".into())
            })?;
        let bytes = self.read_bounded_evidence_reference(
            run_id,
            &reference,
            super::assessment::MAX_JUDGE_RESPONSE_BYTES + 16 * 1024,
        )?;
        let record: super::coordinator::SemanticReassessmentRecord = serde_json::from_slice(&bytes)
            .map_err(|_| {
                CodingAgentError::Conflict("human reassessment result is invalid".into())
            })?;
        let expected_revision = if run.state == RunState::Done {
            run.revision.checked_sub(1)
        } else {
            Some(run.revision)
        };
        if expected_revision != Some(*expected_run_revision)
            || record.expected_run_revision != *expected_run_revision
            || record.run_id != run_id.as_str()
            || record.attempt_id != attempt_id
            || reference.attempt_id != attempt_id
            || record.issue_revision != *issue_revision
            || record.source_sha256 != *source_sha256
            || !record.source_is_clean
            || evidence
                .commit_id
                .as_deref()
                .unwrap_or(&run.snapshot.base_commit)
                != record.source_head_commit
            || record.report_reference != *report_reference
            || evidence.report_reference.as_deref() != Some(report_reference)
            || record.assessment.outcome != super::assessment::AssessmentUse::Pass
            || result.result_digest.as_deref() != Some(reference.sha256.as_str())
            || key
                != format!(
                    "human-semantic:{:x}",
                    Sha256::digest(record.request_key.as_bytes())
                )
            || reference.media_type != "application/json"
            || run
                .snapshot
                .workflow
                .semantic_judge_worker_digest
                .as_ref()
                .is_some_and(|digest| {
                    record.assessment.worker_configuration_digest.as_ref() != Some(digest)
                })
        {
            return Err(CodingAgentError::Conflict(
                "latest human reassessment does not verify this completion target".into(),
            ));
        }
        Ok(Some(*issue_revision))
    }

    fn completion_marker_references(
        &self,
        run: &AgentRunRecord,
        attempt_id: &str,
        references: &[EvidenceReference],
        commit_id: &str,
    ) -> Result<(Vec<String>, Option<u64>), CodingAgentError> {
        let acknowledgements = references
            .iter()
            .filter(|reference| reference.name == "issue-result-ack")
            .collect::<Vec<_>>();
        if let [reference] = acknowledgements.as_slice() {
            let bytes = self.read_bounded_evidence_reference(
                &run.snapshot.run_id,
                reference,
                super::issue_gateway::MAX_ISSUE_RESULT_ACK_BYTES,
            )?;
            let ack: SavedIssueResultAck = serde_json::from_slice(&bytes).map_err(|_| {
                CodingAgentError::Conflict("Issue result acknowledgement is invalid".into())
            })?;
            if ack.schema_version != 1
                || ack.run_id != run.snapshot.run_id.as_str()
                || ack.attempt_id != attempt_id
            {
                return Err(CodingAgentError::Conflict(
                    "Issue result acknowledgement belongs to another execution".into(),
                ));
            }
            if ack.status == "completed" {
                let receipt = ack.response.ok_or_else(|| {
                    CodingAgentError::Conflict("Issue result acknowledgement has no receipt".into())
                })?;
                if receipt.issue_id != run.snapshot.issue_id
                    || receipt.run_id != run.snapshot.run_id.as_str()
                    || receipt.full_commit_id != commit_id
                    || !receipt.lease_closed
                    || receipt.revision <= run.snapshot.issue_revision
                    || receipt.revision == u64::MAX
                    || !crate::grpc::issue_tracker::awaiting_review_evidence_references_valid(
                        &receipt.evidence_references,
                    )
                    || receipt
                        .evidence_references
                        .iter()
                        .collect::<BTreeSet<_>>()
                        .len()
                        != receipt.evidence_references.len()
                {
                    return Err(CodingAgentError::Conflict(
                        "Issue result acknowledgement does not match the saved execution".into(),
                    ));
                }
                return Ok((receipt.evidence_references, Some(receipt.revision)));
            }
            if ack.status != "skipped" || ack.response.is_some() {
                return Err(CodingAgentError::Conflict(
                    "Issue result acknowledgement is not confirmed".into(),
                ));
            }
        } else if !acknowledgements.is_empty() {
            return Err(CodingAgentError::Conflict(
                "Issue result acknowledgements are ambiguous".into(),
            ));
        }
        // Older runs used local evidence references in their canonical tracker marker.
        Ok((
            references
                .iter()
                .map(|reference| {
                    format!(
                        "agent-app://coding-agent/{}/{}/{}",
                        run.snapshot.run_id, reference.evidence_id, reference.sha256
                    )
                })
                .collect(),
            None,
        ))
    }

    fn require_completion_publication_receipt(
        &self,
        transaction: &Transaction<'_>,
        run_id: &RunId,
        attempt_id: &str,
        snapshot: &AgentRunSnapshot,
        evidence: &RunEvidenceAssessment,
    ) -> Result<(), CodingAgentError> {
        if snapshot.publication_plan == PublicationPlan::NoPublication {
            return Ok(());
        }
        let key = format!(
            "publish-{}",
            &format!("{:x}", Sha256::digest(attempt_id.as_bytes()))[..32]
        );
        let operation = transaction
            .query_row(
                "SELECT operation_id, attempt_id, kind_json, payload_hash, request_json,
                        status_json, result_json, created_at_ms, updated_at_ms
                 FROM agent_run_operations WHERE run_id = ?1 AND idempotency_key = ?2",
                params![run_id.as_str(), key],
                operation_from_row(run_id, &key),
            )
            .optional()?
            .ok_or_else(|| {
                CodingAgentError::Conflict(
                    "approved publication plan has no durable publication receipt".into(),
                )
            })?;
        if operation.status != DurableOperationStatus::Completed
            || operation.result.as_ref().is_none_or(|result| {
                result.outcome != DurableOperationOutcome::Succeeded
                    || result.evidence_ids.len() != 1
            })
        {
            return Err(CodingAgentError::Conflict(
                "approved publication result is not durably confirmed".into(),
            ));
        }
        let expected_commit = evidence.commit_id.as_deref().ok_or_else(|| {
            CodingAgentError::Conflict("publication has no verified full commit".into())
        })?;
        let expected_request = match snapshot.publication_plan {
            PublicationPlan::PushBranch => DurableOperationRequest::PublishBranch {
                branch: snapshot.workspace_branch.clone(),
                commit_id: expected_commit.into(),
            },
            PublicationPlan::CreatePullRequest => {
                let DurableOperationRequest::CreatePullRequest { base_branch, .. } =
                    &operation.request
                else {
                    return Err(CodingAgentError::Conflict(
                        "approved PR intent is missing".into(),
                    ));
                };
                if !valid_branch(base_branch) || base_branch == &snapshot.workspace_branch {
                    return Err(CodingAgentError::Conflict(
                        "approved PR base branch is invalid".into(),
                    ));
                }
                DurableOperationRequest::CreatePullRequest {
                    branch: snapshot.workspace_branch.clone(),
                    base_branch: base_branch.clone(),
                    commit_id: expected_commit.into(),
                }
            }
            PublicationPlan::NoPublication => return Ok(()),
        };
        if operation.request != expected_request {
            return Err(CodingAgentError::Conflict(
                "publication intent differs from the approved commit and plan".into(),
            ));
        }
        let evidence_id = &operation.result.as_ref().unwrap().evidence_ids[0];
        let reference = find_evidence_in_transaction(transaction, run_id, evidence_id)?
            .ok_or_else(|| {
                CodingAgentError::Conflict(
                    "publication receipt is not owned by the current run".into(),
                )
            })?;
        if reference.attempt_id != attempt_id || !self.verify_reference_for_run(run_id, &reference)
        {
            return Err(CodingAgentError::Conflict(
                "publication receipt is not durable for the current attempt".into(),
            ));
        }
        let bytes = read_regular_nofollow(&self.evidence_path(run_id, evidence_id))?;
        let receipt: super::publication::PublicationOutcome = serde_json::from_slice(&bytes)?;
        let valid = match (&snapshot.publication_plan, receipt) {
            (
                PublicationPlan::PushBranch | PublicationPlan::CreatePullRequest,
                super::publication::PublicationOutcome::NoSourceChanges { source },
            ) => super::publication::no_changes_receipt_matches(
                &source,
                run_id.as_str(),
                attempt_id,
                &snapshot.workspace_branch,
                &snapshot.base_commit,
                expected_commit,
            ),
            (
                PublicationPlan::PushBranch,
                super::publication::PublicationOutcome::BranchPublished {
                    branch,
                    commit,
                    remote_head,
                },
            ) => {
                branch == snapshot.workspace_branch
                    && commit == expected_commit
                    && remote_head == expected_commit
            }
            (
                PublicationPlan::CreatePullRequest,
                super::publication::PublicationOutcome::PullRequestCreated {
                    branch,
                    head_commit,
                    number,
                    url,
                },
            ) => {
                branch == snapshot.workspace_branch
                    && head_commit == expected_commit
                    && number > 0
                    && valid_https_receipt_url(&url)
            }
            (
                PublicationPlan::PushBranch | PublicationPlan::CreatePullRequest,
                super::publication::PublicationOutcome::NoChangesWithTreeProof {
                    base_commit,
                    frozen_commit,
                    base_tree_id,
                    frozen_tree_id,
                },
            ) => {
                valid_commit_id(&base_commit)
                    && valid_commit_id(&frozen_commit)
                    && base_commit == snapshot.base_commit
                    && frozen_commit == expected_commit
                    && valid_commit_id(&base_tree_id)
                    && valid_commit_id(&frozen_tree_id)
                    && base_tree_id == frozen_tree_id
            }
            _ => false,
        };
        if !valid {
            return Err(CodingAgentError::Conflict(
                "publication receipt does not prove the approved full commit remotely".into(),
            ));
        }
        Ok(())
    }

    fn lock_connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, CodingAgentError> {
        self.connection
            .lock()
            .map_err(|_| CodingAgentError::Corrupt("SQLite connection mutex was poisoned".into()))
    }

    fn snapshot_json(&self, run_id: &RunId) -> Result<String, CodingAgentError> {
        let connection = self.lock_connection()?;
        connection
            .query_row(
                "SELECT snapshot_json FROM agent_runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))
    }

    fn read_bounded_attempt_evidence(
        &self,
        run_id: &RunId,
        attempt_id: &str,
        evidence_id: &str,
        max_bytes: usize,
    ) -> Result<(EvidenceReference, Vec<u8>), CodingAgentError> {
        let reference = self
            .find_evidence(run_id, evidence_id)?
            .ok_or_else(|| CodingAgentError::NotFound(format!("evidence {evidence_id}")))?;
        if reference.attempt_id != attempt_id {
            return Err(CodingAgentError::Conflict(
                "assessment evidence must be saved and owned by the selected attempt".into(),
            ));
        }
        let bytes = self.read_bounded_evidence_reference(run_id, &reference, max_bytes)?;
        Ok((reference, bytes))
    }

    fn read_bounded_evidence_reference(
        &self,
        run_id: &RunId,
        reference: &EvidenceReference,
        max_bytes: usize,
    ) -> Result<Vec<u8>, CodingAgentError> {
        if !reference.save_confirmed {
            return Err(CodingAgentError::Conflict(
                "evidence save is unconfirmed".into(),
            ));
        }
        let size = usize::try_from(reference.size_bytes)
            .map_err(|_| CodingAgentError::Invalid("assessment evidence is too large".into()))?;
        if size > max_bytes {
            return Err(CodingAgentError::Invalid(
                "assessment evidence exceeds its bounded input limit".into(),
            ));
        }
        let bytes = read_regular_nofollow_bounded(
            &self.evidence_path(run_id, &reference.evidence_id),
            max_bytes,
        )?;
        if bytes.len() != size || format!("{:x}", Sha256::digest(&bytes)) != reference.sha256 {
            return Err(CodingAgentError::Corrupt(format!(
                "evidence {} failed digest/size verification",
                reference.evidence_id
            )));
        }
        Ok(bytes)
    }
}

fn validate_finalization_capture_metadata(
    capture: &FinalizationCaptureInputs,
) -> Result<(), CodingAgentError> {
    validate_safe_id(&capture.report_reference.evidence_id, "report evidence id")?;
    if capture.report_reference.media_type != "application/json"
        || !capture.report_reference.save_confirmed
        || capture.report_reference.size_bytes > super::report::MAX_REPORT_BYTES as u64
    {
        return Err(CodingAgentError::Invalid(
            "finalization report reference must be saved bounded JSON evidence".into(),
        ));
    }
    let mut evidence_ids = BTreeSet::new();
    evidence_ids.insert(capture.report_reference.evidence_id.as_str());
    let mut total_stream_bytes = 0_u64;
    for (reference, label) in [
        (capture.stdout_reference.as_ref(), "stdout"),
        (capture.stderr_reference.as_ref(), "stderr"),
    ] {
        let Some(reference) = reference else {
            continue;
        };
        validate_safe_id(&reference.evidence_id, &format!("{label} evidence id"))?;
        if !reference.save_confirmed || !evidence_ids.insert(reference.evidence_id.as_str()) {
            return Err(CodingAgentError::Invalid(
                "captured finalization evidence references must be saved and unique".into(),
            ));
        }
        total_stream_bytes = total_stream_bytes
            .checked_add(reference.size_bytes)
            .ok_or_else(|| CodingAgentError::Invalid("captured output size overflow".into()))?;
    }
    if total_stream_bytes > MAX_FINALIZATION_STREAM_BYTES {
        return Err(CodingAgentError::Invalid(
            "finalization stream evidence exceeds its bounded capture limit".into(),
        ));
    }
    Ok(())
}

fn validation_receipt_reference(
    receipt: &super::validation::ValidationReceipt,
) -> Result<AssessmentValidationReceiptReference, CodingAgentError> {
    validate_safe_id(receipt.validation_id(), "assessment validation id")?;
    validate_safe_id(receipt.receipt_id(), "assessment receipt id")?;
    let bytes = serde_json::to_vec(receipt)?;
    if bytes.len() > super::validation::MAX_VALIDATION_RECORD_BYTES {
        return Err(CodingAgentError::Invalid(
            "validation receipt exceeds its bounded stored size".into(),
        ));
    }
    Ok(AssessmentValidationReceiptReference {
        validation_id: receipt.validation_id().into(),
        required: receipt.required(),
        receipt_id: receipt.receipt_id().into(),
        digest: format!("{:x}", Sha256::digest(&bytes)),
        command_definition_digest: receipt.command_definition_digest().into(),
        permission_profile_digest: receipt.permission_profile_digest().into(),
        target_full_commit_id: receipt.target_full_commit_id().into(),
        target_full_tree_id: receipt.target_full_tree_id().into(),
        target_untracked_manifest_digest: receipt.target_untracked_manifest_digest().into(),
        target_snapshot_digest: receipt.target_snapshot_digest().into(),
    })
}

fn finalization_identity_matches_snapshot(
    snapshot: &AgentRunSnapshot,
    identity: &ExecutionIdentity,
) -> bool {
    if identity.run_id != snapshot.run_id.as_str() {
        return false;
    }
    match snapshot.execution_mode {
        super::model::ExecutionMode::OneShot => {
            let approved = snapshot.approval.as_ref().is_some_and(|approval| {
                approval.approved_at_ms > 0
                    && !approval.approved_by.trim().is_empty()
                    && approval
                        .applicable_modes
                        .contains(&super::model::ExecutionMode::OneShot)
            });
            approved
                && identity.worker_id > 0
                && snapshot
                    .workflow
                    .registered_worker_id
                    .as_deref()
                    .is_some_and(|worker_id| worker_id == identity.worker_id.to_string())
                && snapshot.workflow.workflow_execution_id.as_deref()
                    == Some(identity.workflow_execution_id.as_str())
                && snapshot.workflow.parent_job_id.as_deref()
                    == Some(identity.parent_job_id.as_str())
                && snapshot.workflow.child_sandbox_job_id.as_deref()
                    == Some(identity.child_sandbox_job_id.as_str())
        }
        super::model::ExecutionMode::Interactive => {
            snapshot
                .workflow
                .workflow_execution_id
                .as_ref()
                .is_none_or(|id| id == &identity.workflow_execution_id)
                && snapshot
                    .workflow
                    .parent_job_id
                    .as_ref()
                    .is_none_or(|id| id == &identity.parent_job_id)
                && snapshot
                    .workflow
                    .child_sandbox_job_id
                    .as_ref()
                    .is_none_or(|id| id == &identity.child_sandbox_job_id)
        }
    }
}

fn validate_accepted_one_shot_start_receipt(
    connection: &Connection,
    store: &RunStore,
    snapshot: &AgentRunSnapshot,
    attempt_id: &str,
    identity: &ExecutionIdentity,
) -> Result<(), CodingAgentError> {
    if snapshot.execution_mode != super::model::ExecutionMode::OneShot {
        return Ok(());
    }
    if identity.attempt_id != attempt_id
        || !finalization_identity_matches_snapshot(snapshot, identity)
    {
        return Err(CodingAgentError::Conflict(
            "one-shot finalization identity differs from the approved workflow identity".into(),
        ));
    }

    let start: Option<AcceptedStartOperationRecord> = connection
        .query_row(
            "SELECT claim.phase_generation, operation.phase_generation, operation.attempt_id,
                    operation.payload_hash, operation.kind_json, operation.request_json,
                    operation.status_json, operation.result_json
             FROM agent_run_start_claims claim
             JOIN agent_run_operations operation
               ON operation.run_id = claim.run_id
              AND operation.idempotency_key = claim.idempotency_key
             WHERE claim.run_id = ?1 AND claim.attempt_id = ?2",
            params![snapshot.run_id.as_str(), attempt_id],
            |row| {
                Ok(AcceptedStartOperationRecord {
                    claim_generation: row.get(0)?,
                    operation_generation: row.get(1)?,
                    operation_attempt_id: row.get(2)?,
                    operation_payload_hash: row.get(3)?,
                    kind_json: row.get(4)?,
                    request_json: row.get(5)?,
                    status_json: row.get(6)?,
                    result_json: row.get(7)?,
                })
            },
        )
        .optional()?;
    let Some(AcceptedStartOperationRecord {
        claim_generation,
        operation_generation,
        operation_attempt_id,
        operation_payload_hash,
        kind_json,
        request_json,
        status_json,
        result_json,
    }) = start
    else {
        return Err(CodingAgentError::Conflict(
            "one-shot finalization requires its persisted current StartAgent claim".into(),
        ));
    };
    if nonnegative_u64(claim_generation, "start claim generation")?
        != nonnegative_u64(operation_generation, "start operation generation")?
        || operation_attempt_id != attempt_id
        || decode::<super::model::DurableOperationKind>(&kind_json, "start operation kind")?
            != super::model::DurableOperationKind::StartAgent
    {
        return Err(CodingAgentError::Corrupt(
            "one-shot StartAgent claim does not identify its durable start operation".into(),
        ));
    }
    let expected_request = super::model::DurableOperationRequest::StartAgent {
        workflow_definition_hash: snapshot.workflow.definition_hash.clone(),
        attempt_id: attempt_id.into(),
        execution_mode: super::model::ExecutionMode::OneShot,
    };
    if decode::<super::model::DurableOperationRequest>(&request_json, "start operation request")?
        != expected_request
        || operation_payload_hash != expected_request.payload_hash()?
    {
        return Err(CodingAgentError::Conflict(
            "persisted StartAgent request or payload hash differs from the approved one-shot attempt"
                .into(),
        ));
    }
    if decode::<DurableOperationStatus>(&status_json, "start operation status")?
        != DurableOperationStatus::Completed
    {
        return Err(CodingAgentError::Conflict(
            "one-shot finalization requires an accepted completed StartAgent operation".into(),
        ));
    }
    let result: DurableOperationResult = result_json
        .as_deref()
        .map(|json| decode(json, "start operation result"))
        .transpose()?
        .ok_or_else(|| {
            CodingAgentError::Conflict(
                "accepted StartAgent operation has no persisted receipt result".into(),
            )
        })?;
    let Some(result_digest) = result.result_digest.as_deref() else {
        return Err(CodingAgentError::Conflict(
            "accepted StartAgent operation has no receipt digest".into(),
        ));
    };
    if result.outcome != DurableOperationOutcome::Succeeded
        || !valid_sha256(result_digest)
        || result.evidence_ids.len() != 2
        || result.evidence_ids[0] == result.evidence_ids[1]
    {
        return Err(CodingAgentError::Conflict(
            "StartAgent did not persist one successful immutable intent and accepted receipt"
                .into(),
        ));
    }
    let intent_reference =
        find_evidence_in_connection(connection, &snapshot.run_id, &result.evidence_ids[0])?
            .ok_or_else(|| {
                CodingAgentError::Corrupt("persisted StartAgent intent evidence is missing".into())
            })?;
    let receipt_reference =
        find_evidence_in_connection(connection, &snapshot.run_id, &result.evidence_ids[1])?
            .ok_or_else(|| {
                CodingAgentError::Corrupt("accepted StartAgent receipt evidence is missing".into())
            })?;
    if intent_reference.attempt_id != attempt_id
        || intent_reference.name != "dispatch-intent"
        || intent_reference.media_type != "application/json"
        || !intent_reference.save_confirmed
        || !valid_sha256(&intent_reference.sha256)
        || intent_reference.size_bytes > MAX_START_INTENT_BYTES as u64
        || receipt_reference.attempt_id != attempt_id
        || receipt_reference.name != "dispatch-receipt"
        || receipt_reference.media_type != "application/json"
        || !receipt_reference.save_confirmed
        || receipt_reference.sha256 != result_digest
        || receipt_reference.size_bytes > MAX_START_RECEIPT_BYTES as u64
    {
        return Err(CodingAgentError::Corrupt(
            "accepted StartAgent receipt reference is not exact attempt JSON evidence".into(),
        ));
    }
    let intent_bytes = read_regular_nofollow_bounded(
        &store.evidence_path(&snapshot.run_id, &intent_reference.evidence_id),
        MAX_START_INTENT_BYTES,
    )?;
    if intent_bytes.len() as u64 != intent_reference.size_bytes
        || format!("{:x}", Sha256::digest(&intent_bytes)) != intent_reference.sha256
    {
        return Err(CodingAgentError::Corrupt(
            "persisted StartAgent intent failed its durable digest/size check".into(),
        ));
    }
    let intent_text = std::str::from_utf8(&intent_bytes).map_err(|_| {
        CodingAgentError::Corrupt("persisted StartAgent intent is not UTF-8 JSON".into())
    })?;
    let intent: ImmutableExecutionIntent = decode(intent_text, "persisted StartAgent intent")?;
    if serde_json::to_vec(&intent)? != intent_bytes
        || !immutable_execution_intent_matches_snapshot(&intent, snapshot, attempt_id, identity)
    {
        return Err(CodingAgentError::Conflict(
            "persisted StartAgent intent differs from the approved one-shot execution context"
                .into(),
        ));
    }

    let receipt_bytes = read_regular_nofollow_bounded(
        &store.evidence_path(&snapshot.run_id, &receipt_reference.evidence_id),
        MAX_START_RECEIPT_BYTES,
    )?;
    if receipt_bytes.len() as u64 != receipt_reference.size_bytes
        || format!("{:x}", Sha256::digest(&receipt_bytes)) != receipt_reference.sha256
    {
        return Err(CodingAgentError::Corrupt(
            "accepted StartAgent receipt failed its durable digest/size check".into(),
        ));
    }
    let receipt_text = std::str::from_utf8(&receipt_bytes).map_err(|_| {
        CodingAgentError::Corrupt("accepted StartAgent receipt is not UTF-8 JSON".into())
    })?;
    let receipt: StartReceipt = decode(receipt_text, "accepted StartAgent receipt")?;
    if serde_json::to_vec(&receipt)? != receipt_bytes
        || receipt.identity != *identity
        || receipt.method_name != intent.method_name
        || receipt.image_digest != snapshot.profile.image_digest
        || receipt.execution_intent_sha256 != intent_reference.sha256
    {
        return Err(CodingAgentError::Conflict(
            "host finalization identity differs from the accepted StartAgent intent or receipt"
                .into(),
        ));
    }

    let prompt_references = {
        let mut statement = connection.prepare(
            "SELECT evidence_id, attempt_id, name, media_type, sha256, size_bytes,
                    save_confirmed, created_at_ms
             FROM agent_run_evidence
             WHERE run_id = ?1 AND attempt_id = ?2 AND name = 'prompt-intent'
             ORDER BY created_at_ms, evidence_id",
        )?;
        statement
            .query_map(
                params![snapshot.run_id.as_str(), attempt_id],
                evidence_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?
    };
    match intent.prompt_sha256.as_deref() {
        Some(prompt_digest) if valid_sha256(prompt_digest) && prompt_references.len() == 1 => {
            let prompt_reference = &prompt_references[0];
            if prompt_reference.media_type != "text/plain"
                || !prompt_reference.save_confirmed
                || prompt_reference.sha256 != prompt_digest
                || prompt_reference.size_bytes > MAX_START_INTENT_BYTES as u64
            {
                return Err(CodingAgentError::Corrupt(
                    "persisted StartAgent prompt does not match its immutable intent digest".into(),
                ));
            }
            let prompt_bytes = read_regular_nofollow_bounded(
                &store.evidence_path(&snapshot.run_id, &prompt_reference.evidence_id),
                MAX_START_INTENT_BYTES,
            )?;
            if prompt_bytes.len() as u64 != prompt_reference.size_bytes
                || format!("{:x}", Sha256::digest(&prompt_bytes)) != prompt_digest
            {
                return Err(CodingAgentError::Corrupt(
                    "persisted StartAgent prompt failed its durable digest/size check".into(),
                ));
            }
        }
        None if prompt_references.is_empty() => {}
        _ => {
            return Err(CodingAgentError::Corrupt(
                "persisted StartAgent prompt evidence differs from its immutable intent".into(),
            ));
        }
    }
    Ok(())
}

fn immutable_execution_intent_matches_snapshot(
    intent: &ImmutableExecutionIntent,
    snapshot: &AgentRunSnapshot,
    attempt_id: &str,
    identity: &ExecutionIdentity,
) -> bool {
    let repository = &snapshot.repository.identity;
    intent.run_id == snapshot.run_id.as_str()
        && intent.attempt_id == attempt_id
        && intent.issue_id == snapshot.issue_id
        && intent.issue_revision == snapshot.issue_revision
        && intent.execution_mode == super::model::ExecutionMode::OneShot
        && intent.purpose == snapshot.purpose
        && intent.repository_host == repository.host
        && intent.repository_owner == repository.organization
        && intent.repository_name == repository.repository
        && intent.base_commit == snapshot.base_commit
        && intent.branch == snapshot.workspace_branch
        && intent.profile_id == snapshot.profile.profile_id
        && intent.profile_version == snapshot.profile.version
        && intent.image_digest == snapshot.profile.image_digest
        && intent.command == snapshot.profile.command
        && intent.workflow_definition_hash == snapshot.workflow.definition_hash
        && intent.workflow_execution_id == identity.workflow_execution_id
        && intent.parent_job_id == identity.parent_job_id
        && intent.child_sandbox_job_id == identity.child_sandbox_job_id
        && snapshot
            .workflow
            .registered_worker_id
            .as_deref()
            .is_some_and(|worker_id| intent.registered_worker_id == worker_id)
        && intent.worker_id == identity.worker_id
        && intent.method_name == "run"
        && intent.prompt_sha256.as_deref().is_none_or(valid_sha256)
}

fn validate_host_finalization_inputs_for_run(
    inputs: &HostVerifiedFinalizationInputs,
    snapshot: &AgentRunSnapshot,
    contract: &ApprovalAssessmentContract,
) -> Result<(), CodingAgentError> {
    if inputs.identity.run_id != snapshot.run_id.as_str()
        || inputs.identity.attempt_id.is_empty()
        || inputs.identity.workflow_execution_id.is_empty()
        || inputs.identity.parent_job_id.is_empty()
        || inputs.identity.child_sandbox_job_id.is_empty()
        || inputs.identity.sandbox_job_id.is_empty()
        || inputs.identity.worker_id <= 0
        || !finalization_identity_matches_snapshot(snapshot, &inputs.identity)
    {
        return Err(CodingAgentError::Conflict(
            "host finalization execution identity differs from the immutable run snapshot".into(),
        ));
    }
    for (value, label) in [
        (
            &inputs.identity.workflow_execution_id,
            "workflow execution id",
        ),
        (&inputs.identity.parent_job_id, "parent job id"),
        (
            &inputs.identity.child_sandbox_job_id,
            "child sandbox job id",
        ),
        (&inputs.identity.sandbox_job_id, "sandbox job id"),
    ] {
        validate_bounded_text(value, 256, label)?;
    }
    if !valid_sha256(&inputs.artifact.artifact_digest)
        || !valid_sha256(&inputs.artifact.manifest_digest)
        || inputs.artifact.run_id != snapshot.run_id.as_str()
        || inputs.artifact.attempt_id != inputs.identity.attempt_id
        || inputs.artifact.branch != snapshot.workspace_branch
        || inputs.artifact.base_commit != snapshot.base_commit
        || inputs.artifact.repository.host != snapshot.repository.identity.host
        || inputs.artifact.repository.owner != snapshot.repository.identity.organization
        || inputs.artifact.repository.name != snapshot.repository.identity.repository
        || inputs.artifact.commit != inputs.target.full_commit_id
        || inputs.artifact.tree != inputs.target.full_tree_id
        || inputs.target.artifact_manifest_digest != inputs.artifact.manifest_digest
        || inputs.target.artifact_digest != inputs.artifact.artifact_digest
        || inputs.sealed_artifact.full_commit_id != inputs.target.full_commit_id
        || inputs.sealed_artifact.full_tree_id != inputs.target.full_tree_id
        || inputs.sealed_artifact.untracked_manifest_digest
            != inputs.target.untracked_manifest_digest
        || inputs.sealed_artifact.target_snapshot_digest != inputs.target.target_snapshot_digest
        || !valid_commit_id(&inputs.target.full_commit_id)
        || !valid_commit_id(&inputs.target.full_tree_id)
        || inputs.target.full_commit_id.len() != inputs.target.full_tree_id.len()
        || !valid_sha256(&inputs.target.untracked_manifest_digest)
        || !valid_sha256(&inputs.target.target_snapshot_digest)
        || !valid_sha256(&inputs.target.artifact_manifest_digest)
        || !valid_sha256(&inputs.target.artifact_digest)
        || !valid_commit_id(&inputs.sealed_artifact.full_commit_id)
        || !valid_commit_id(&inputs.sealed_artifact.full_tree_id)
        || !valid_sha256(&inputs.sealed_artifact.untracked_manifest_digest)
        || !valid_sha256(&inputs.sealed_artifact.target_snapshot_digest)
    {
        return Err(CodingAgentError::Conflict(
            "native artifact, frozen target, or immutable repository identity does not match"
                .into(),
        ));
    }
    let target = super::validation::ValidationTargetSnapshot::new(
        inputs.target.full_commit_id.clone(),
        inputs.target.full_tree_id.clone(),
        inputs.target.untracked_manifest_digest.clone(),
    )
    .map_err(|_| CodingAgentError::Invalid("finalization target identity is invalid".into()))?;
    if target.digest() != inputs.target.target_snapshot_digest {
        return Err(CodingAgentError::Conflict(
            "finalization target digest does not match the complete frozen target".into(),
        ));
    }
    if let WorkspaceFreezeStatus::HostVerified { commit_id } = &inputs.workspace_freeze
        && commit_id != &inputs.target.full_commit_id
    {
        return Err(CodingAgentError::Conflict(
            "host-verified workspace commit differs from the sealed target".into(),
        ));
    }
    if inputs.schema_version != VERIFIED_FINALIZATION_INPUTS_SCHEMA_VERSION
        || inputs.report_reference.attempt_id != inputs.identity.attempt_id
        || inputs
            .stdout_reference
            .as_ref()
            .is_some_and(|reference| reference.attempt_id != inputs.identity.attempt_id)
        || inputs
            .stderr_reference
            .as_ref()
            .is_some_and(|reference| reference.attempt_id != inputs.identity.attempt_id)
    {
        return Err(CodingAgentError::Conflict(
            "captured finalization output references do not belong to the exact attempt".into(),
        ));
    }
    validate_finalization_capture_metadata(&FinalizationCaptureInputs {
        report_reference: inputs.report_reference.clone(),
        stdout_reference: inputs.stdout_reference.clone(),
        stderr_reference: inputs.stderr_reference.clone(),
        end_present: inputs.end_present,
        stream_error: inputs.stream_error,
        cancelled: inputs.cancelled,
    })?;

    let mut plan_map = std::collections::BTreeMap::new();
    for plan in &inputs.validation_plans {
        validate_safe_id(&plan.validation_id, "finalization validation id")?;
        if !valid_sha256(&plan.command_definition_digest)
            || !valid_sha256(&plan.binding_digest)
            || !valid_sha256(&plan.permission_profile_digest)
            || plan_map.insert(plan.validation_id.as_str(), plan).is_some()
        {
            return Err(CodingAgentError::Invalid(
                "reopened validation plan bindings are invalid or duplicated".into(),
            ));
        }
        let approved = contract
            .validation_plan
            .iter()
            .find(|definition| definition.validation_id == plan.validation_id)
            .ok_or_else(|| {
                CodingAgentError::Conflict(
                    "reopened validation plan is not present in the immutable approval".into(),
                )
            })?;
        let command = approved.command.as_ref().ok_or_else(|| {
            CodingAgentError::Conflict(
                "an unavailable approved command cannot produce a reopened plan".into(),
            )
        })?;
        if approved.required != plan.required
            || command.profile_digest != plan.permission_profile_digest
            || command.argv != plan.argv
            || command.working_directory != plan.working_directory
        {
            return Err(CodingAgentError::Conflict(
                "reopened validation plan differs from its approved profile".into(),
            ));
        }
    }
    for approved in &contract.validation_plan {
        if approved.command.is_some() && !plan_map.contains_key(approved.validation_id.as_str()) {
            return Err(CodingAgentError::Conflict(
                "an approved validation command has no reopened host plan".into(),
            ));
        }
    }

    let mut record_map = std::collections::BTreeMap::new();
    for record in &inputs.validation_record_facts {
        validate_safe_id(&record.validation_id, "finalization validation id")?;
        if !valid_sha256(&record.digest)
            || record
                .receipt_digest
                .as_deref()
                .is_some_and(|digest| !valid_sha256(digest))
            || record_map
                .insert(record.validation_id.as_str(), record)
                .is_some()
        {
            return Err(CodingAgentError::Invalid(
                "reopened validation records are invalid or duplicated".into(),
            ));
        }
        let plan = plan_map.get(record.validation_id.as_str()).ok_or_else(|| {
            CodingAgentError::Conflict("validation record has no reopened approved plan".into())
        })?;
        if record.required != plan.required {
            return Err(CodingAgentError::Conflict(
                "validation record requiredness differs from its approved plan".into(),
            ));
        }
    }
    if plan_map
        .keys()
        .any(|validation_id| !record_map.contains_key(validation_id))
    {
        return Err(CodingAgentError::Conflict(
            "a reopened validation plan has no immutable host result record".into(),
        ));
    }

    let mut expected_receipts = inputs
        .validation_receipts
        .iter()
        .map(validation_receipt_reference)
        .collect::<Result<Vec<_>, _>>()?;
    expected_receipts.sort_by(|left, right| left.validation_id.cmp(&right.validation_id));
    if expected_receipts != inputs.validation_receipt_references
        || inputs.validation_receipt_json.len() != inputs.validation_receipts.len()
        || inputs.validation_records.len() != inputs.validation_record_json.len()
        || inputs.validation_records.len() != inputs.validation_record_facts.len()
        || inputs.validation_records.len() > MAX_FINALIZATION_VALIDATIONS
        || inputs.validation_receipts.len() > MAX_FINALIZATION_VALIDATIONS
    {
        return Err(CodingAgentError::Corrupt(
            "typed reopened validation facts do not match their bounded stored bindings".into(),
        ));
    }
    for (receipt, serialized) in inputs
        .validation_receipts
        .iter()
        .zip(&inputs.validation_receipt_json)
    {
        if serde_json::to_string(receipt)? != *serialized {
            return Err(CodingAgentError::Corrupt(
                "serialized validation receipt differs from its native receipt".into(),
            ));
        }
    }
    for ((record, serialized), facts) in inputs
        .validation_records
        .iter()
        .zip(&inputs.validation_record_json)
        .zip(&inputs.validation_record_facts)
    {
        if serde_json::to_string(record)? != *serialized
            || format!("{:x}", Sha256::digest(serialized.as_bytes())) != facts.digest
        {
            return Err(CodingAgentError::Corrupt(
                "serialized validation record differs from its native host record".into(),
            ));
        }
    }
    for receipt in &inputs.validation_receipt_references {
        let approved = contract
            .validation_plan
            .iter()
            .find(|definition| definition.validation_id == receipt.validation_id)
            .ok_or_else(|| {
                CodingAgentError::Conflict(
                    "reopened validation receipt is not in the immutable approval".into(),
                )
            })?;
        let command = approved.command.as_ref().ok_or_else(|| {
            CodingAgentError::Conflict(
                "an unavailable approved validation cannot have a reopened receipt".into(),
            )
        })?;
        let plan = plan_map
            .get(receipt.validation_id.as_str())
            .ok_or_else(|| {
                CodingAgentError::Conflict(
                    "validation receipt has no reopened approved plan".into(),
                )
            })?;
        let record = record_map
            .get(receipt.validation_id.as_str())
            .ok_or_else(|| {
                CodingAgentError::Conflict(
                    "validation receipt has no reopened result record".into(),
                )
            })?;
        if approved.required != receipt.required
            || command.profile_digest != receipt.permission_profile_digest
            || plan.command_definition_digest != receipt.command_definition_digest
            || plan.permission_profile_digest != receipt.permission_profile_digest
            || record.receipt_id.as_deref() != Some(receipt.receipt_id.as_str())
            || record.receipt_digest.as_deref() != Some(receipt.digest.as_str())
            || receipt.target_full_commit_id != inputs.target.full_commit_id
            || receipt.target_full_tree_id != inputs.target.full_tree_id
            || receipt.target_untracked_manifest_digest != inputs.target.untracked_manifest_digest
            || receipt.target_snapshot_digest != inputs.target.target_snapshot_digest
        {
            return Err(CodingAgentError::Conflict(
                "native validation receipt differs from its approved command, record, or target"
                    .into(),
            ));
        }
    }
    Ok(())
}

fn validate_stored_finalization_inputs(
    stored: &StoredHostVerifiedFinalizationInputs,
    index_reference: &EvidenceReference,
    run_id: &RunId,
    attempt_id: &str,
    snapshot: &AgentRunSnapshot,
    contract: &ApprovalAssessmentContract,
) -> Result<SavedVerifiedFinalizationInputs, CodingAgentError> {
    if stored.schema_version != VERIFIED_FINALIZATION_INPUTS_SCHEMA_VERSION
        || stored.identity.run_id != run_id.as_str()
        || stored.identity.attempt_id != attempt_id
        || !finalization_identity_matches_snapshot(
            snapshot,
            &ExecutionIdentity::from(stored.identity.clone()),
        )
        || stored.artifact.run_id != run_id.as_str()
        || stored.artifact.attempt_id != attempt_id
        || stored.artifact.branch != snapshot.workspace_branch
        || stored.artifact.base_commit != snapshot.base_commit
        || stored.artifact.repository.host != snapshot.repository.identity.host
        || stored.artifact.repository.owner != snapshot.repository.identity.organization
        || stored.artifact.repository.name != snapshot.repository.identity.repository
        || stored.artifact.commit != stored.target.full_commit_id
        || stored.artifact.tree != stored.target.full_tree_id
        || stored.artifact.artifact_digest != stored.target.artifact_digest
        || stored.artifact.manifest_digest != stored.target.artifact_manifest_digest
        || stored.sealed_artifact.full_commit_id != stored.target.full_commit_id
        || stored.sealed_artifact.full_tree_id != stored.target.full_tree_id
        || stored.sealed_artifact.untracked_manifest_digest
            != stored.target.untracked_manifest_digest
        || stored.sealed_artifact.target_snapshot_digest != stored.target.target_snapshot_digest
        || !valid_sha256(&stored.target.artifact_digest)
        || !valid_commit_id(&stored.target.full_commit_id)
        || !valid_commit_id(&stored.target.full_tree_id)
        || stored.target.full_commit_id.len() != stored.target.full_tree_id.len()
        || !valid_sha256(&stored.target.untracked_manifest_digest)
        || !valid_sha256(&stored.target.target_snapshot_digest)
        || !valid_sha256(&stored.target.artifact_manifest_digest)
        || stored.target.artifact_digest != stored.artifact.artifact_digest
        || stored.report_reference.attempt_id != attempt_id
        || stored
            .stdout_reference
            .as_ref()
            .is_some_and(|reference| reference.attempt_id != attempt_id)
        || stored
            .stderr_reference
            .as_ref()
            .is_some_and(|reference| reference.attempt_id != attempt_id)
        || index_reference.name != VERIFIED_FINALIZATION_INPUTS_EVIDENCE_NAME
        || index_reference.media_type != "application/json"
        || index_reference.attempt_id != attempt_id
    {
        return Err(CodingAgentError::Corrupt(
            "saved verified finalization identity or target is inconsistent".into(),
        ));
    }
    let target = super::validation::ValidationTargetSnapshot::new(
        stored.target.full_commit_id.clone(),
        stored.target.full_tree_id.clone(),
        stored.target.untracked_manifest_digest.clone(),
    )
    .map_err(|_| CodingAgentError::Corrupt("saved finalization target is invalid".into()))?;
    if target.digest() != stored.target.target_snapshot_digest
        || matches!(
            &stored.workspace_freeze,
            WorkspaceFreezeStatus::HostVerified { commit_id }
                if commit_id != &stored.target.full_commit_id
        )
    {
        return Err(CodingAgentError::Corrupt(
            "saved target digest or host-verified workspace commit is inconsistent".into(),
        ));
    }
    if stored.identity.worker_id <= 0 {
        return Err(CodingAgentError::Corrupt(
            "saved execution identity has no positive worker id".into(),
        ));
    }
    for (value, label) in [
        (
            &stored.identity.workflow_execution_id,
            "workflow execution id",
        ),
        (&stored.identity.parent_job_id, "parent job id"),
        (
            &stored.identity.child_sandbox_job_id,
            "child sandbox job id",
        ),
        (&stored.identity.sandbox_job_id, "sandbox job id"),
    ] {
        validate_bounded_text(value, 256, label).map_err(|_| {
            CodingAgentError::Corrupt(format!("saved execution identity has invalid {label}"))
        })?;
    }
    validate_stored_evidence_reference(&stored.report_reference, attempt_id, "report")?;
    if let Some(reference) = &stored.stdout_reference {
        validate_stored_evidence_reference(reference, attempt_id, "stdout")?;
    }
    if let Some(reference) = &stored.stderr_reference {
        validate_stored_evidence_reference(reference, attempt_id, "stderr")?;
    }
    let stream_bytes = stored
        .stdout_reference
        .as_ref()
        .map_or(0, |reference| reference.size_bytes)
        .checked_add(
            stored
                .stderr_reference
                .as_ref()
                .map_or(0, |reference| reference.size_bytes),
        )
        .ok_or_else(|| CodingAgentError::Corrupt("saved stream size overflow".into()))?;
    if stream_bytes > MAX_FINALIZATION_STREAM_BYTES
        || stored.validation_receipts.len() > MAX_FINALIZATION_VALIDATIONS
        || stored.validation_receipt_json.len() != stored.validation_receipts.len()
        || stored.validation_receipt_references.len() != stored.validation_receipts.len()
        || stored.validation_records.len() > MAX_FINALIZATION_VALIDATIONS
        || stored.validation_record_json.len() != stored.validation_records.len()
        || stored.validation_record_facts.len() != stored.validation_records.len()
        || stored.validation_plans.len() > MAX_FINALIZATION_VALIDATIONS
    {
        return Err(CodingAgentError::Corrupt(
            "saved finalization input records exceed their bounded shape".into(),
        ));
    }
    let mut receipt_reference_map = std::collections::BTreeMap::new();
    for (index, receipt_json) in stored.validation_receipt_json.iter().enumerate() {
        if receipt_json.len() > super::validation::MAX_VALIDATION_RECORD_BYTES {
            return Err(CodingAgentError::Corrupt(
                "saved validation receipt exceeds its bounded size".into(),
            ));
        }
        let receipt: StoredValidationReceiptDto = decode(receipt_json, "validation receipt")?;
        let value: serde_json::Value = decode(receipt_json, "validation receipt")?;
        if stored.validation_receipts[index] != value {
            return Err(CodingAgentError::Corrupt(
                "saved native receipt bytes differ from their serialized receipt record".into(),
            ));
        }
        validate_stored_validation_receipt(&receipt, run_id, attempt_id, &stored.target)?;
        let reference = validation_receipt_reference_from_dto(&receipt, receipt_json)?;
        if receipt_reference_map
            .insert(reference.receipt_id.clone(), reference.clone())
            .is_some()
        {
            return Err(CodingAgentError::Corrupt(
                "saved native receipts contain duplicate receipt identities".into(),
            ));
        }
    }
    let mut declared_receipts = stored.validation_receipt_references.clone();
    declared_receipts.sort_by(|left, right| left.validation_id.cmp(&right.validation_id));
    let mut actual_receipts = receipt_reference_map.values().cloned().collect::<Vec<_>>();
    actual_receipts.sort_by(|left, right| left.validation_id.cmp(&right.validation_id));
    if declared_receipts != actual_receipts {
        return Err(CodingAgentError::Corrupt(
            "saved typed receipt references do not match reopened receipt bytes".into(),
        ));
    }

    let mut plans = std::collections::BTreeMap::new();
    for plan in &stored.validation_plans {
        validate_safe_id(&plan.validation_id, "saved validation id")?;
        if !valid_sha256(&plan.command_definition_digest)
            || !valid_sha256(&plan.binding_digest)
            || !valid_sha256(&plan.permission_profile_digest)
            || plans.insert(plan.validation_id.as_str(), plan).is_some()
        {
            return Err(CodingAgentError::Corrupt(
                "saved validation plan is invalid or duplicated".into(),
            ));
        }
        let approved = contract
            .validation_plan
            .iter()
            .find(|definition| definition.validation_id == plan.validation_id)
            .ok_or_else(|| {
                CodingAgentError::Corrupt("saved validation plan is outside approval".into())
            })?;
        if approved.command.as_ref().is_none_or(|command| {
            approved.required != plan.required
                || command.profile_digest != plan.permission_profile_digest
                || command.argv != plan.argv
                || command.working_directory != plan.working_directory
        }) {
            return Err(CodingAgentError::Corrupt(
                "saved validation plan differs from approved command metadata".into(),
            ));
        }
    }
    for approved in &contract.validation_plan {
        if approved.command.is_some() && !plans.contains_key(approved.validation_id.as_str()) {
            return Err(CodingAgentError::Corrupt(
                "saved validation inputs omitted an approved available command".into(),
            ));
        }
    }

    let mut record_facts = std::collections::BTreeMap::new();
    for (index, record_json) in stored.validation_record_json.iter().enumerate() {
        if record_json.len() > super::validation::MAX_VALIDATION_RECORD_BYTES {
            return Err(CodingAgentError::Corrupt(
                "saved validation record exceeds its bounded size".into(),
            ));
        }
        let record: StoredValidationRecordDto = decode(record_json, "validation record")?;
        let value: serde_json::Value = decode(record_json, "validation record")?;
        if stored.validation_records[index] != value {
            return Err(CodingAgentError::Corrupt(
                "saved host record differs from its exact serialized record".into(),
            ));
        }
        let facts = &stored.validation_record_facts[index];
        if record.schema_version != 1
            || record.run_id != run_id.as_str()
            || record.attempt_id != attempt_id
            || record.validation_id != facts.validation_id
            || record.required != facts.required
            || record.outcome != facts.outcome
            || record.end_observed != facts.end_observed
            || record.transport_error_observed != facts.transport_error_observed
            || format!("{:x}", Sha256::digest(record_json.as_bytes())) != facts.digest
        {
            return Err(CodingAgentError::Corrupt(
                "saved validation record facts do not match their immutable bytes".into(),
            ));
        }
        let plan = plans.get(record.validation_id.as_str()).ok_or_else(|| {
            CodingAgentError::Corrupt("saved validation record has no approved plan".into())
        })?;
        if record.binding_digest != plan.binding_digest
            || record.required != plan.required
            || record_facts
                .insert(record.validation_id.clone(), facts)
                .is_some()
        {
            return Err(CodingAgentError::Corrupt(
                "saved validation record differs from its native plan".into(),
            ));
        }
        let (receipt_id, receipt_digest) = match &record.receipt {
            Some(receipt) => {
                if record.child_job_id.as_deref() != Some(receipt.child_job_id.as_str())
                    || record.sandbox_id.as_deref() != Some(receipt.sandbox_id.as_str())
                    || record.exit_code != Some(receipt.exit_code)
                    || record.execution_time_ms != Some(receipt.execution_time_ms)
                    || receipt.validation_id != record.validation_id
                    || receipt.required != record.required
                    || receipt.binding_digest != record.binding_digest
                {
                    return Err(CodingAgentError::Corrupt(
                        "saved validation record summary differs from its native receipt".into(),
                    ));
                }
                let receipt_json = serde_json::to_string(receipt)?;
                let reference = validation_receipt_reference_from_dto(receipt, &receipt_json)?;
                if !receipt_reference_map.contains_key(&reference.receipt_id) {
                    return Err(CodingAgentError::Corrupt(
                        "saved validation record receipt was not reopened independently".into(),
                    ));
                }
                (Some(reference.receipt_id), Some(reference.digest))
            }
            None => (None, None),
        };
        if facts.receipt_id != receipt_id || facts.receipt_digest != receipt_digest {
            return Err(CodingAgentError::Corrupt(
                "saved validation record receipt binding does not match its host record".into(),
            ));
        }
    }
    if plans
        .keys()
        .any(|validation_id| !record_facts.contains_key(*validation_id))
    {
        return Err(CodingAgentError::Corrupt(
            "saved validation plan has no immutable result record".into(),
        ));
    }
    for receipt in &stored.validation_receipt_references {
        let approved = contract
            .validation_plan
            .iter()
            .find(|definition| definition.validation_id == receipt.validation_id)
            .ok_or_else(|| {
                CodingAgentError::Corrupt("saved receipt is outside the approved plan".into())
            })?;
        let plan = plans.get(receipt.validation_id.as_str()).ok_or_else(|| {
            CodingAgentError::Corrupt("saved receipt has no approved validation plan".into())
        })?;
        let facts = record_facts
            .get(receipt.validation_id.as_str())
            .ok_or_else(|| {
                CodingAgentError::Corrupt("saved receipt has no immutable validation record".into())
            })?;
        if approved.command.as_ref().is_none_or(|command| {
            approved.required != receipt.required
                || command.profile_digest != receipt.permission_profile_digest
        }) || plan.command_definition_digest != receipt.command_definition_digest
            || plan.permission_profile_digest != receipt.permission_profile_digest
            || facts.receipt_id.as_deref() != Some(receipt.receipt_id.as_str())
            || facts.receipt_digest.as_deref() != Some(receipt.digest.as_str())
            || receipt.target_full_commit_id != stored.target.full_commit_id
            || receipt.target_full_tree_id != stored.target.full_tree_id
            || receipt.target_untracked_manifest_digest != stored.target.untracked_manifest_digest
            || receipt.target_snapshot_digest != stored.target.target_snapshot_digest
        {
            return Err(CodingAgentError::Corrupt(
                "saved receipt differs from its approved command and frozen target".into(),
            ));
        }
    }
    let identity: ExecutionIdentity = stored.identity.clone().into();
    Ok(SavedVerifiedFinalizationInputs {
        index_reference: index_reference.clone(),
        identity,
        report_reference: stored.report_reference.clone().into(),
        stdout_reference: stored.stdout_reference.clone().map(Into::into),
        stderr_reference: stored.stderr_reference.clone().map(Into::into),
        invocation_kind: stored.invocation_kind,
        cli_exit: stored.cli_exit.clone(),
        shell_observation: stored.shell_observation.clone(),
        child_writers: stored.child_writers,
        workspace_freeze: stored.workspace_freeze.clone(),
        sealed_artifact: stored.sealed_artifact.clone(),
        target: stored.target.clone(),
        end_present: stored.end_present,
        stream_error: stored.stream_error,
        cancelled: stored.cancelled,
        validation_receipt_references: stored.validation_receipt_references.clone(),
        validation_record_facts: stored.validation_record_facts.clone(),
        validation_plans: stored.validation_plans.clone(),
    })
}

fn validate_stored_evidence_reference(
    reference: &StoredEvidenceReference,
    attempt_id: &str,
    label: &str,
) -> Result<(), CodingAgentError> {
    validate_safe_id(
        &reference.evidence_id,
        &format!("saved {label} evidence id"),
    )?;
    if reference.attempt_id != attempt_id
        || !reference.save_confirmed
        || !valid_sha256(&reference.sha256)
        || reference.name.is_empty()
        || reference.name.len() > 256
        || reference.media_type.is_empty()
        || reference.media_type.len() > 128
    {
        return Err(CodingAgentError::Corrupt(format!(
            "saved {label} evidence reference has invalid ownership or metadata"
        )));
    }
    Ok(())
}

fn validate_stored_validation_receipt(
    receipt: &StoredValidationReceiptDto,
    run_id: &RunId,
    attempt_id: &str,
    target: &StoredFinalizationTarget,
) -> Result<(), CodingAgentError> {
    if receipt.schema_version != 1
        || receipt.run_id != run_id.as_str()
        || receipt.attempt_id != attempt_id
        || !receipt.end_observed
        || receipt.stream_error_observed
        || !receipt.output.capture_complete
        || receipt.output.reference.is_empty()
        || receipt.output.reference.len() > 4096
        || receipt.output.reference.chars().any(char::is_control)
        || receipt
            .output
            .stdout_bytes
            .saturating_add(receipt.output.stderr_bytes)
            > super::validation::MAX_VALIDATION_OUTPUT_BYTES
        || !valid_sha256(&receipt.binding_digest)
        || !valid_sha256(&receipt.command_definition_digest)
        || !valid_sha256(&receipt.permission_profile_digest)
        || receipt.target_full_commit_id != target.full_commit_id
        || receipt.target_full_tree_id != target.full_tree_id
        || receipt.target_untracked_manifest_digest != target.untracked_manifest_digest
        || receipt.target_snapshot_digest != target.target_snapshot_digest
        || !valid_sha256(&receipt.output.stdout_sha256)
        || !valid_sha256(&receipt.output.stderr_sha256)
        || (receipt.exit_code == 0 && receipt.outcome != ValidationOutcome::Passed)
        || (receipt.exit_code != 0 && receipt.outcome != ValidationOutcome::Failed)
    {
        return Err(CodingAgentError::Corrupt(format!(
            "saved validation receipt {} is malformed or unbounded",
            receipt.receipt_id
        )));
    }
    for (value, label) in [
        (&receipt.worker_name, "saved validation worker name"),
        (&receipt.sandbox_id, "saved validation sandbox id"),
        (&receipt.child_job_id, "saved validation job id"),
    ] {
        validate_bounded_text(value, 256, label).map_err(|_| {
            CodingAgentError::Corrupt(format!(
                "saved validation receipt {} has an invalid {label}",
                receipt.receipt_id
            ))
        })?;
    }
    validate_safe_id(&receipt.receipt_id, "saved validation receipt id")?;
    validate_safe_id(&receipt.validation_id, "saved validation id")?;
    Ok(())
}

fn validation_receipt_reference_from_dto(
    receipt: &StoredValidationReceiptDto,
    serialized: &str,
) -> Result<AssessmentValidationReceiptReference, CodingAgentError> {
    Ok(AssessmentValidationReceiptReference {
        validation_id: receipt.validation_id.clone(),
        required: receipt.required,
        receipt_id: receipt.receipt_id.clone(),
        digest: format!("{:x}", Sha256::digest(serialized.as_bytes())),
        command_definition_digest: receipt.command_definition_digest.clone(),
        permission_profile_digest: receipt.permission_profile_digest.clone(),
        target_full_commit_id: receipt.target_full_commit_id.clone(),
        target_full_tree_id: receipt.target_full_tree_id.clone(),
        target_untracked_manifest_digest: receipt.target_untracked_manifest_digest.clone(),
        target_snapshot_digest: receipt.target_snapshot_digest.clone(),
    })
}

fn validate_finalization_claimable(
    saved: &SavedVerifiedFinalizationInputs,
    contract: &ApprovalAssessmentContract,
) -> Result<(), CodingAgentError> {
    let valid_exit =
        saved.cli_exit == super::model::ProcessExitObservation::ExitedNormally { exit_code: 0 };
    if !valid_exit
        || !saved
            .shell_observation
            .is_successful_for(saved.invocation_kind)
        || saved.child_writers != super::model::ChildWritersStatus::Stopped
        || !saved.end_present
        || saved.stream_error
        || saved.cancelled
        || !matches!(
            &saved.workspace_freeze,
            WorkspaceFreezeStatus::HostVerified { commit_id }
                if commit_id == &saved.target.full_commit_id
        )
        || saved.sealed_artifact != saved.target
    {
        return Err(CodingAgentError::Conflict(
            "host process, writer, output, or frozen-target observations do not authorize assessment"
                .into(),
        ));
    }
    for approved in &contract.validation_plan {
        let record = saved
            .validation_record_facts
            .iter()
            .find(|record| record.validation_id == approved.validation_id);
        if approved.required && approved.command.is_none() {
            return Err(CodingAgentError::Conflict(
                "required approved validation command is unavailable".into(),
            ));
        }
        if let Some(record) = record {
            if record.outcome == ValidationOutcome::Failed
                || (record.required && record.outcome != ValidationOutcome::Passed)
                || record.transport_error_observed
            {
                return Err(CodingAgentError::Conflict(
                    "reopened host validation failure is immutable and blocks assessment".into(),
                ));
            }
        } else if approved.required {
            return Err(CodingAgentError::Conflict(
                "required approved validation has no reopened host result".into(),
            ));
        }
    }
    Ok(())
}

fn validate_final_assessment_against_inputs(
    evidence: &RunEvidenceAssessment,
    saved: &SavedVerifiedFinalizationInputs,
) -> Result<(), CodingAgentError> {
    let expected_exit = match saved.cli_exit {
        super::model::ProcessExitObservation::ExitedNormally { exit_code } => Some(exit_code),
        super::model::ProcessExitObservation::Signaled { .. }
        | super::model::ProcessExitObservation::Killed
        | super::model::ProcessExitObservation::Disconnected
        | super::model::ProcessExitObservation::Unknown => None,
    };
    let validation_failed = saved
        .validation_record_facts
        .iter()
        .any(|record| record.outcome == ValidationOutcome::Failed);
    let required_validation_missing = saved
        .validation_record_facts
        .iter()
        .any(|record| record.required && record.outcome != ValidationOutcome::Passed);
    let report_id = saved.report_reference.evidence_id.as_str();
    let stream_refs_match = evidence.stdout_reference.as_deref()
        == saved
            .stdout_reference
            .as_ref()
            .map(|reference| reference.evidence_id.as_str())
        && evidence.stdout_bytes
            == saved
                .stdout_reference
                .as_ref()
                .map_or(0, |reference| reference.size_bytes)
        && evidence.stderr_reference.as_deref()
            == saved
                .stderr_reference
                .as_ref()
                .map(|reference| reference.evidence_id.as_str())
        && evidence.stderr_bytes
            == saved
                .stderr_reference
                .as_ref()
                .map_or(0, |reference| reference.size_bytes);
    let frozen_commit_matches = match &saved.workspace_freeze {
        WorkspaceFreezeStatus::HostVerified { commit_id } => {
            commit_id == &saved.target.full_commit_id
                && evidence.commit_id.as_deref() == Some(saved.target.full_commit_id.as_str())
                && evidence.commit_consistent
        }
        WorkspaceFreezeStatus::NotFrozen | WorkspaceFreezeStatus::Unknown => {
            evidence.commit_id.is_none() && !evidence.commit_consistent
        }
    };
    if evidence.report_reference.as_deref() != Some(report_id)
        || evidence.report_save_confirmed != saved.report_reference.save_confirmed
        || !stream_refs_match
        || evidence.final_exit_code != expected_exit
        || evidence.end_present != saved.end_present
        || evidence.stream_error != saved.stream_error
        || evidence.cancelled != saved.cancelled
        || !frozen_commit_matches
        || (validation_failed && evidence.validation.state != ValidationState::Failed)
        || (required_validation_missing && evidence.validation.state == ValidationState::Passed)
    {
        return Err(CodingAgentError::Conflict(
            "final execution assessment does not preserve the immutable host finalization facts"
                .into(),
        ));
    }
    Ok(())
}

fn validate_saved_judge_input(
    input_bytes: &[u8],
    context: SavedJudgeInputValidationContext<'_>,
) -> Result<(), CodingAgentError> {
    let SavedJudgeInputValidationContext {
        run_id,
        attempt_id,
        assessment_id,
        snapshot_json,
        contract,
        contract_digest,
        report_reference,
        report_bytes,
        saved,
    } = context;
    let input_text = std::str::from_utf8(input_bytes)
        .map_err(|_| CodingAgentError::Invalid("judge input payload is not UTF-8 JSON".into()))?;
    let input: StoredJudgeInput = decode(input_text, "exact judge input payload")?;
    if input.schema_version != 1
        || input.assessment_id != assessment_id
        || input.run_id != run_id.as_str()
        || input.attempt_id != attempt_id
        || input.task_kind
            != match contract.purpose {
                RunPurpose::Investigation => TaskKind::Investigation,
                RunPurpose::Implementation => TaskKind::Implementation,
            }
        || input.approval_snapshot_digest
            != format!("{:x}", Sha256::digest(snapshot_json.as_bytes()))
        || input.frozen_artifact_digest != saved.target.artifact_digest
        || input.report_digest != report_reference.sha256
        || input.provider
            != contract.provider.clone().ok_or_else(|| {
                CodingAgentError::Conflict("approved semantic judge provider is missing".into())
            })?
        || contract.prompt_policy_version.as_deref() != Some(input.prompt_policy_version.as_str())
        || input.judge_input_scope_digest != contract.judge_input_scope_digest()
        || contract.digest()? != contract_digest
    {
        return Err(CodingAgentError::Conflict(
            "judge payload differs from immutable approval, report, or frozen artifact".into(),
        ));
    }

    let expected_criteria = contract
        .criteria
        .iter()
        .filter_map(|criterion| {
            let method = match criterion.method {
                super::model::CriterionEvaluationMethod::SemanticJudge => {
                    super::assessment::CriterionMethod::SemanticJudge
                }
                super::model::CriterionEvaluationMethod::Deterministic => return None,
                super::model::CriterionEvaluationMethod::Unknown => {
                    return Some(Err(CodingAgentError::Conflict(
                        "unknown acceptance method cannot enter a semantic judge payload".into(),
                    )));
                }
            };
            Some(Ok(super::assessment::ApprovedCriterion {
                criterion_id: criterion.criterion_id.clone(),
                statement: criterion.statement.clone(),
                required: criterion.required,
                method,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if input.criteria != expected_criteria {
        return Err(CodingAgentError::Conflict(
            "judge criteria do not exactly match approved semantic acceptance IDs".into(),
        ));
    }

    let mut expected_receipts = saved
        .validation_receipt_references
        .iter()
        .map(|receipt| super::assessment::ValidationReceiptBinding {
            receipt_id: receipt.receipt_id.clone(),
            validation_id: receipt.validation_id.clone(),
            digest: receipt.digest.clone(),
        })
        .collect::<Vec<_>>();
    expected_receipts.sort_by(|left, right| left.validation_id.cmp(&right.validation_id));
    let mut actual_receipts = input.validation_receipts;
    actual_receipts.sort_by(|left, right| left.validation_id.cmp(&right.validation_id));
    if actual_receipts != expected_receipts {
        return Err(CodingAgentError::Conflict(
            "judge payload receipt list differs from the reopened typed host receipts".into(),
        ));
    }

    let mut expected_plan = contract
        .validation_plan
        .iter()
        .map(
            |definition| super::assessment::ApprovedValidationDefinition {
                validation_id: definition.validation_id.clone(),
                required: definition.required,
                definition_digest: saved
                    .validation_plans
                    .iter()
                    .find(|plan| plan.validation_id == definition.validation_id)
                    .map(|plan| plan.command_definition_digest.clone())
                    .unwrap_or_else(|| {
                        format!("{:x}", Sha256::digest(b"unavailable-approved-command"))
                    }),
            },
        )
        .collect::<Vec<_>>();
    expected_plan.sort_by(|left, right| left.validation_id.cmp(&right.validation_id));
    let mut actual_plan = input.validation_plan;
    actual_plan.sort_by(|left, right| left.validation_id.cmp(&right.validation_id));
    if actual_plan.len() != expected_plan.len()
        || actual_plan
            .iter()
            .zip(&expected_plan)
            .any(|(actual, expected)| {
                actual.validation_id != expected.validation_id
                    || actual.required != expected.required
                    || !valid_sha256(&actual.definition_digest)
                    || (contract
                        .validation_plan
                        .iter()
                        .find(|definition| definition.validation_id == actual.validation_id)
                        .is_some_and(|definition| definition.command.is_some())
                        && actual.definition_digest != expected.definition_digest)
            })
    {
        return Err(CodingAgentError::Conflict(
            "judge validation IDs or command digests differ from approved host validation plans"
                .into(),
        ));
    }

    let mut allowed_sources = contract.input_scope.allowed_sources.clone();
    allowed_sources.sort();
    let mut payload_sources = input.evidence;
    payload_sources
        .sort_by(|left, right| (&left.kind, &left.identity).cmp(&(&right.kind, &right.identity)));
    if payload_sources.len() != allowed_sources.len()
        || payload_sources
            .iter()
            .zip(&allowed_sources)
            .any(|(payload, source)| {
                payload.kind != source.kind
                    || payload.identity != source.identity
                    || payload.content.len() > source.max_bytes
                    || (source.kind == super::assessment::EvidenceSourceKind::Report
                        && (source.identity
                            != contract
                                .input_scope
                                .allowed_sources
                                .iter()
                                .find(|candidate| {
                                    candidate.kind == super::assessment::EvidenceSourceKind::Report
                                })
                                .map_or("", |candidate| candidate.identity.as_str())
                            || payload.content.as_bytes() != report_bytes))
            })
    {
        return Err(CodingAgentError::Conflict(
            "judge evidence identities or report bytes exceed the approved input scope".into(),
        ));
    }
    if report_reference.evidence_id != saved.report_reference.evidence_id
        || input_bytes.len() > contract.input_scope.max_input_bytes
    {
        return Err(CodingAgentError::Conflict(
            "judge payload is not bound to the canonical saved report or approved input bound"
                .into(),
        ));
    }
    Ok(())
}

fn find_finalization_input_reference(
    connection: &Connection,
    run_id: &RunId,
    attempt_id: &str,
) -> Result<Option<EvidenceReference>, CodingAgentError> {
    let evidence_id: Option<String> = connection
        .query_row(
            "SELECT evidence_id FROM agent_run_finalization_inputs
             WHERE run_id = ?1 AND attempt_id = ?2",
            params![run_id.as_str(), attempt_id],
            |row| row.get(0),
        )
        .optional()?;
    evidence_id
        .map(|evidence_id| {
            let reference = find_evidence_in_connection(connection, run_id, &evidence_id)?
                .ok_or_else(|| {
                    CodingAgentError::Corrupt(
                        "verified finalization input index references missing evidence".into(),
                    )
                })?;
            if reference.attempt_id != attempt_id {
                return Err(CodingAgentError::Corrupt(
                    "verified finalization input index references evidence from another attempt"
                        .into(),
                ));
            }
            Ok(reference)
        })
        .transpose()
}

fn load_finalization_input_bytes(
    connection: &Connection,
    store: &RunStore,
    run_id: &RunId,
    attempt_id: &str,
) -> Result<Option<(EvidenceReference, Vec<u8>)>, CodingAgentError> {
    let Some(reference) = find_finalization_input_reference(connection, run_id, attempt_id)? else {
        return Ok(None);
    };
    if reference.name != VERIFIED_FINALIZATION_INPUTS_EVIDENCE_NAME
        || reference.media_type != "application/json"
        || reference.attempt_id != attempt_id
        || !reference.save_confirmed
    {
        return Err(CodingAgentError::Corrupt(
            "verified finalization input index has invalid evidence metadata".into(),
        ));
    }
    let bytes = read_regular_nofollow_bounded(
        &store.evidence_path(run_id, &reference.evidence_id),
        MAX_VERIFIED_FINALIZATION_INPUTS_BYTES,
    )?;
    if bytes.len() as u64 != reference.size_bytes
        || format!("{:x}", Sha256::digest(&bytes)) != reference.sha256
    {
        return Err(CodingAgentError::Corrupt(
            "verified finalization input blob failed its digest/size check".into(),
        ));
    }
    Ok(Some((reference, bytes)))
}

fn load_saved_verified_finalization_inputs(
    connection: &Connection,
    store: &RunStore,
    run_id: &RunId,
    attempt_id: &str,
) -> Result<Option<SavedVerifiedFinalizationInputs>, CodingAgentError> {
    let Some((index_reference, bytes)) =
        load_finalization_input_bytes(connection, store, run_id, attempt_id)?
    else {
        return Ok(None);
    };
    let stored_text = std::str::from_utf8(&bytes).map_err(|_| {
        CodingAgentError::Corrupt("verified finalization inputs are not UTF-8 JSON".into())
    })?;
    let stored: StoredHostVerifiedFinalizationInputs =
        decode(stored_text, "verified finalization inputs")?;
    let snapshot_json: String = connection
        .query_row(
            "SELECT snapshot_json FROM agent_runs WHERE run_id = ?1",
            [run_id.as_str()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| CodingAgentError::NotFound(format!("run {run_id}")))?;
    let stored_snapshot = decode_stored_snapshot(&snapshot_json)?;
    let contract = match stored_snapshot
        .contract_status()
        .map_err(|error| CodingAgentError::Corrupt(error.to_string()))?
    {
        ApprovalAssessmentContractStatus::LegacyIncomplete => {
            return Err(CodingAgentError::Corrupt(
                "verified finalization inputs have no complete approval contract".into(),
            ));
        }
        ApprovalAssessmentContractStatus::Versioned { contract, .. } => *contract,
    };
    let saved = validate_stored_finalization_inputs(
        &stored,
        &index_reference,
        run_id,
        attempt_id,
        &stored_snapshot.snapshot,
        &contract,
    )?;
    validate_accepted_one_shot_start_receipt(
        connection,
        store,
        &stored_snapshot.snapshot,
        attempt_id,
        &saved.identity,
    )?;
    let report_reference = validate_saved_output_reference(
        connection,
        store,
        run_id,
        attempt_id,
        &saved.report_reference,
        "report",
    )?;
    let stdout_reference = saved
        .stdout_reference
        .as_ref()
        .map(|reference| {
            validate_saved_output_reference(
                connection, store, run_id, attempt_id, reference, "stdout",
            )
        })
        .transpose()?;
    let stderr_reference = saved
        .stderr_reference
        .as_ref()
        .map(|reference| {
            validate_saved_output_reference(
                connection, store, run_id, attempt_id, reference, "stderr",
            )
        })
        .transpose()?;
    let report_bytes = read_regular_nofollow_bounded(
        &store.evidence_path(run_id, &report_reference.evidence_id),
        super::report::MAX_REPORT_BYTES,
    )?;
    validate_report_bytes(&contract, run_id, attempt_id, &report_bytes).map_err(|error| {
        CodingAgentError::Corrupt(format!(
            "saved canonical report failed revalidation: {error}"
        ))
    })?;
    Ok(Some(SavedVerifiedFinalizationInputs {
        index_reference,
        identity: saved.identity,
        report_reference,
        stdout_reference,
        stderr_reference,
        invocation_kind: saved.invocation_kind,
        cli_exit: saved.cli_exit,
        shell_observation: saved.shell_observation,
        child_writers: saved.child_writers,
        workspace_freeze: saved.workspace_freeze,
        sealed_artifact: saved.sealed_artifact,
        target: saved.target,
        end_present: saved.end_present,
        stream_error: saved.stream_error,
        cancelled: saved.cancelled,
        validation_receipt_references: saved.validation_receipt_references,
        validation_record_facts: saved.validation_record_facts,
        validation_plans: saved.validation_plans,
    }))
}

fn validate_saved_output_reference(
    connection: &Connection,
    store: &RunStore,
    run_id: &RunId,
    attempt_id: &str,
    expected: &EvidenceReference,
    kind: &str,
) -> Result<EvidenceReference, CodingAgentError> {
    let actual = find_evidence_in_connection(connection, run_id, &expected.evidence_id)?
        .ok_or_else(|| CodingAgentError::Corrupt(format!("saved {kind} evidence is missing")))?;
    if actual != *expected || actual.attempt_id != attempt_id || !actual.save_confirmed {
        return Err(CodingAgentError::Corrupt(format!(
            "saved {kind} evidence reference changed ownership or metadata"
        )));
    }
    let max = if kind == "report" {
        super::report::MAX_REPORT_BYTES
    } else {
        usize::try_from(MAX_FINALIZATION_STREAM_BYTES).unwrap_or(usize::MAX)
    };
    let bytes =
        read_regular_nofollow_bounded(&store.evidence_path(run_id, &actual.evidence_id), max)?;
    if bytes.len() as u64 != actual.size_bytes
        || format!("{:x}", Sha256::digest(&bytes)) != actual.sha256
    {
        return Err(CodingAgentError::Corrupt(format!(
            "saved {kind} evidence failed its digest/size check"
        )));
    }
    if kind == "report" && actual.media_type != "application/json" {
        return Err(CodingAgentError::Corrupt(
            "saved report evidence is not JSON".into(),
        ));
    }
    Ok(actual)
}

fn validate_host_capture_evidence(
    store: &RunStore,
    run_id: &RunId,
    attempt_id: &str,
    inputs: &HostVerifiedFinalizationInputs,
) -> Result<(), CodingAgentError> {
    for (reference, kind) in [
        (Some(&inputs.report_reference), "report"),
        (inputs.stdout_reference.as_ref(), "stdout"),
        (inputs.stderr_reference.as_ref(), "stderr"),
    ] {
        let Some(reference) = reference else {
            continue;
        };
        if reference.attempt_id != attempt_id {
            return Err(CodingAgentError::Conflict(format!(
                "captured {kind} reference belongs to another attempt"
            )));
        }
        let (actual, bytes) = store.read_bounded_attempt_evidence(
            run_id,
            attempt_id,
            &reference.evidence_id,
            if kind == "report" {
                super::report::MAX_REPORT_BYTES
            } else {
                usize::try_from(MAX_FINALIZATION_STREAM_BYTES).unwrap_or(usize::MAX)
            },
        )?;
        if actual != *reference || (kind == "report" && actual.media_type != "application/json") {
            return Err(CodingAgentError::Conflict(format!(
                "captured {kind} reference is not the exact saved attempt evidence"
            )));
        }
        if kind == "report" {
            let snapshot_json = store.snapshot_json(run_id)?;
            let snapshot = decode_stored_snapshot(&snapshot_json)?;
            let contract = match snapshot
                .contract_status()
                .map_err(|error| CodingAgentError::Corrupt(error.to_string()))?
            {
                ApprovalAssessmentContractStatus::LegacyIncomplete => {
                    return Err(CodingAgentError::Conflict(
                        "report capture requires a versioned approval contract".into(),
                    ));
                }
                ApprovalAssessmentContractStatus::Versioned { contract, .. } => *contract,
            };
            validate_report_bytes(&contract, run_id, attempt_id, &bytes)?;
        }
    }
    Ok(())
}

fn initialize_schema(connection: &Connection) -> Result<(), CodingAgentError> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > STORE_SCHEMA_VERSION {
        return Err(CodingAgentError::Initialization(format!(
            "database schema {version} is newer than supported {STORE_SCHEMA_VERSION}"
        )));
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_delegation_plans (
             plan_id TEXT PRIMARY KEY,
             request_key TEXT NOT NULL UNIQUE,
             issue_id TEXT NOT NULL,
             request_json TEXT NOT NULL,
             issue_revision INTEGER NOT NULL CHECK(issue_revision > 0),
             settings_revision INTEGER NOT NULL CHECK(settings_revision >= 0),
             created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
             run_id TEXT REFERENCES agent_runs(run_id) ON DELETE RESTRICT
         );
         CREATE INDEX IF NOT EXISTS agent_delegation_plans_by_issue ON agent_delegation_plans(issue_id, created_at_ms);
         CREATE TABLE IF NOT EXISTS agent_runs (
             run_id TEXT PRIMARY KEY,
             snapshot_json TEXT NOT NULL,
             revision INTEGER NOT NULL CHECK(revision >= 0),
             state_json TEXT NOT NULL,
             stage_json TEXT NOT NULL,
               created_at_ms INTEGER NOT NULL,
               current_attempt_id TEXT,
               start_phase_generation INTEGER NOT NULL DEFAULT 0 CHECK(start_phase_generation >= 0)
         );
         CREATE TABLE IF NOT EXISTS agent_run_attempts (
             run_id TEXT NOT NULL REFERENCES agent_runs(run_id) ON DELETE RESTRICT,
             attempt_id TEXT NOT NULL,
             ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
             created_at_ms INTEGER NOT NULL,
             PRIMARY KEY(run_id, attempt_id),
             UNIQUE(run_id, ordinal)
         );
         CREATE TABLE IF NOT EXISTS agent_run_transitions (
             run_id TEXT NOT NULL REFERENCES agent_runs(run_id) ON DELETE RESTRICT,
             revision INTEGER NOT NULL CHECK(revision >= 0),
             from_state_json TEXT,
             to_state_json TEXT NOT NULL,
             at_ms INTEGER NOT NULL,
             PRIMARY KEY(run_id, revision)
         );
          CREATE TABLE IF NOT EXISTS agent_run_operations (
              operation_id TEXT PRIMARY KEY,
              run_id TEXT NOT NULL REFERENCES agent_runs(run_id) ON DELETE RESTRICT,
              attempt_id TEXT NOT NULL,
              phase_generation INTEGER NOT NULL DEFAULT 0 CHECK(phase_generation >= 0),
              idempotency_key TEXT NOT NULL,
             kind_json TEXT NOT NULL,
             payload_hash TEXT NOT NULL,
             request_json TEXT NOT NULL,
             status_json TEXT NOT NULL,
             result_json TEXT,
             created_at_ms INTEGER NOT NULL,
             updated_at_ms INTEGER NOT NULL,
             UNIQUE(run_id, idempotency_key),
             FOREIGN KEY(run_id, attempt_id) REFERENCES agent_run_attempts(run_id, attempt_id) ON DELETE RESTRICT
         );
          CREATE TABLE IF NOT EXISTS agent_run_evidence (
              run_id TEXT NOT NULL REFERENCES agent_runs(run_id) ON DELETE RESTRICT,
              evidence_id TEXT NOT NULL,
             attempt_id TEXT NOT NULL,
             name TEXT NOT NULL,
             media_type TEXT NOT NULL,
             sha256 TEXT NOT NULL,
             size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
             save_confirmed INTEGER NOT NULL CHECK(save_confirmed IN (0, 1)),
             created_at_ms INTEGER NOT NULL,
              PRIMARY KEY(run_id, evidence_id),
              FOREIGN KEY(run_id, attempt_id) REFERENCES agent_run_attempts(run_id, attempt_id) ON DELETE RESTRICT
          );
          CREATE UNIQUE INDEX IF NOT EXISTS agent_run_evidence_attempt_reference
              ON agent_run_evidence(run_id, attempt_id, evidence_id);
            CREATE TABLE IF NOT EXISTS agent_run_execution_evidence (
               run_id TEXT NOT NULL REFERENCES agent_runs(run_id) ON DELETE RESTRICT,
               attempt_id TEXT NOT NULL,
               result_json TEXT NOT NULL,
               recorded_at_ms INTEGER NOT NULL,
               PRIMARY KEY(run_id, attempt_id),
               FOREIGN KEY(run_id, attempt_id) REFERENCES agent_run_attempts(run_id, attempt_id) ON DELETE RESTRICT
           );
            CREATE TABLE IF NOT EXISTS agent_run_assessment_intents (
                run_id TEXT NOT NULL,
                attempt_id TEXT NOT NULL,
                assessment_id TEXT NOT NULL,
                record_json TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
                finalization_evidence_id TEXT,
                PRIMARY KEY(run_id, attempt_id),
                 UNIQUE(run_id, assessment_id),
                 FOREIGN KEY(run_id, attempt_id)
                     REFERENCES agent_run_attempts(run_id, attempt_id) ON DELETE RESTRICT,
                 FOREIGN KEY(run_id, attempt_id, finalization_evidence_id)
                     REFERENCES agent_run_finalization_inputs(run_id, attempt_id, evidence_id)
                     ON DELETE RESTRICT
            );
           CREATE TABLE IF NOT EXISTS agent_run_start_claims (
              run_id TEXT NOT NULL REFERENCES agent_runs(run_id) ON DELETE RESTRICT,
              phase_generation INTEGER NOT NULL CHECK(phase_generation >= 0),
              attempt_id TEXT NOT NULL,
              idempotency_key TEXT NOT NULL,
              expected_revision INTEGER NOT NULL CHECK(expected_revision >= 0),
              run_revision INTEGER NOT NULL CHECK(run_revision >= 0),
              PRIMARY KEY(run_id, phase_generation),
              UNIQUE(run_id, attempt_id),
              FOREIGN KEY(run_id, attempt_id) REFERENCES agent_run_attempts(run_id, attempt_id) ON DELETE RESTRICT,
              FOREIGN KEY(run_id, idempotency_key) REFERENCES agent_run_operations(run_id, idempotency_key) ON DELETE RESTRICT
          );
          CREATE TABLE IF NOT EXISTS agent_run_interactive_finish_requests (
              run_id TEXT NOT NULL REFERENCES agent_runs(run_id) ON DELETE RESTRICT,
              attempt_id TEXT NOT NULL,
              profile_version INTEGER NOT NULL CHECK(profile_version > 0),
              requested_at_ms INTEGER NOT NULL CHECK(requested_at_ms >= 0),
              PRIMARY KEY(run_id, attempt_id),
              FOREIGN KEY(run_id, attempt_id) REFERENCES agent_run_attempts(run_id, attempt_id) ON DELETE RESTRICT
          );
            CREATE TABLE IF NOT EXISTS agent_run_interactive_finalizations (
                run_id TEXT NOT NULL,
                attempt_id TEXT NOT NULL,
                evidence_json TEXT NOT NULL,
                recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms >= 0),
               PRIMARY KEY(run_id, attempt_id),
                FOREIGN KEY(run_id, attempt_id)
                    REFERENCES agent_run_interactive_finish_requests(run_id, attempt_id) ON DELETE RESTRICT
            );
            CREATE TABLE IF NOT EXISTS agent_run_finalization_inputs (
                run_id TEXT NOT NULL,
                attempt_id TEXT NOT NULL,
                 evidence_id TEXT NOT NULL,
                 PRIMARY KEY(run_id, attempt_id),
                 UNIQUE(run_id, attempt_id, evidence_id),
                 FOREIGN KEY(run_id, attempt_id)
                     REFERENCES agent_run_attempts(run_id, attempt_id) ON DELETE RESTRICT,
                 FOREIGN KEY(run_id, attempt_id, evidence_id)
                     REFERENCES agent_run_evidence(run_id, attempt_id, evidence_id)
                     ON DELETE RESTRICT
            );
            CREATE TABLE IF NOT EXISTS agent_run_completion_authorizations (
               authorization_id TEXT PRIMARY KEY,
               run_id TEXT NOT NULL REFERENCES agent_runs(run_id) ON DELETE RESTRICT,
               intent_idempotency_key TEXT NOT NULL,
               action_json TEXT NOT NULL,
               status_json TEXT NOT NULL,
               record_json TEXT NOT NULL,
               UNIQUE(run_id, intent_idempotency_key)
           );
            CREATE UNIQUE INDEX IF NOT EXISTS one_auto_completion_authorization_per_run
                ON agent_run_completion_authorizations(run_id)
                WHERE action_json = '\"auto_done\"';
            CREATE UNIQUE INDEX IF NOT EXISTS one_inflight_completion_dispatch_per_run
                ON agent_run_completion_authorizations(run_id)
                WHERE status_json = '\"dispatched\"';
            CREATE INDEX IF NOT EXISTS agent_run_recovery ON agent_runs(created_at_ms);
          ",
    )?;
    if version < STORE_SCHEMA_VERSION {
        connection.execute_batch("BEGIN IMMEDIATE;")?;
        let columns = {
            let mut statement = connection.prepare("PRAGMA table_info(agent_runs)")?;
            let names = statement.query_map([], |row| row.get::<_, String>(1))?;
            names.collect::<Result<Vec<_>, _>>()?
        };
        if !columns.iter().any(|name| name == "current_attempt_id") {
            connection
                .execute_batch("ALTER TABLE agent_runs ADD COLUMN current_attempt_id TEXT;")?;
        }
        if !columns.iter().any(|name| name == "start_phase_generation") {
            connection.execute_batch(
                "ALTER TABLE agent_runs ADD COLUMN start_phase_generation INTEGER NOT NULL DEFAULT 0 CHECK(start_phase_generation >= 0);",
            )?;
        }
        let operation_columns = {
            let mut statement = connection.prepare("PRAGMA table_info(agent_run_operations)")?;
            let names = statement.query_map([], |row| row.get::<_, String>(1))?;
            names.collect::<Result<Vec<_>, _>>()?
        };
        if !operation_columns
            .iter()
            .any(|name| name == "phase_generation")
        {
            connection.execute_batch(
                "ALTER TABLE agent_run_operations ADD COLUMN phase_generation INTEGER NOT NULL DEFAULT 0 CHECK(phase_generation >= 0);",
            )?;
        }
        let assessment_intent_columns = {
            let mut statement =
                connection.prepare("PRAGMA table_info(agent_run_assessment_intents)")?;
            let names = statement.query_map([], |row| row.get::<_, String>(1))?;
            names.collect::<Result<Vec<_>, _>>()?
        };
        let legacy_finalization_link = if assessment_intent_columns
            .iter()
            .any(|name| name == "finalization_evidence_id")
        {
            "finalization_evidence_id"
        } else {
            "NULL"
        };
        connection.execute_batch(
            "CREATE TABLE agent_run_finalization_inputs_v10_migration (
                 run_id TEXT NOT NULL,
                 attempt_id TEXT NOT NULL,
                 evidence_id TEXT NOT NULL,
                 PRIMARY KEY(run_id, attempt_id),
                 UNIQUE(run_id, attempt_id, evidence_id),
                 FOREIGN KEY(run_id, attempt_id)
                     REFERENCES agent_run_attempts(run_id, attempt_id) ON DELETE RESTRICT,
                 FOREIGN KEY(run_id, attempt_id, evidence_id)
                     REFERENCES agent_run_evidence(run_id, attempt_id, evidence_id)
                     ON DELETE RESTRICT
             );
             INSERT INTO agent_run_finalization_inputs_v10_migration
                 (run_id, attempt_id, evidence_id)
             SELECT run_id, attempt_id, evidence_id FROM agent_run_finalization_inputs;
             DROP TABLE agent_run_finalization_inputs;
             ALTER TABLE agent_run_finalization_inputs_v10_migration
                 RENAME TO agent_run_finalization_inputs;",
        )?;
        connection.execute_batch(&format!(
            "CREATE TABLE agent_run_assessment_intents_v10_migration (
                 run_id TEXT NOT NULL,
                 attempt_id TEXT NOT NULL,
                 assessment_id TEXT NOT NULL,
                 record_json TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
                 finalization_evidence_id TEXT,
                 PRIMARY KEY(run_id, attempt_id),
                 UNIQUE(run_id, assessment_id),
                 FOREIGN KEY(run_id, attempt_id)
                     REFERENCES agent_run_attempts(run_id, attempt_id) ON DELETE RESTRICT,
                 FOREIGN KEY(run_id, attempt_id, finalization_evidence_id)
                     REFERENCES agent_run_finalization_inputs(run_id, attempt_id, evidence_id)
                     ON DELETE RESTRICT
             );
             INSERT INTO agent_run_assessment_intents_v10_migration
                 (run_id, attempt_id, assessment_id, record_json, created_at_ms,
                  finalization_evidence_id)
             SELECT run_id, attempt_id, assessment_id, record_json, created_at_ms,
                    {legacy_finalization_link}
             FROM agent_run_assessment_intents;
             DROP TABLE agent_run_assessment_intents;
             ALTER TABLE agent_run_assessment_intents_v10_migration
                 RENAME TO agent_run_assessment_intents;"
        ))?;
        connection.execute_batch(&format!(
            "PRAGMA user_version = {STORE_SCHEMA_VERSION}; COMMIT;"
        ))?;
    }
    replace_lifecycle_effect_indexes(connection)?;
    Ok(())
}

fn replace_lifecycle_effect_indexes(connection: &Connection) -> Result<(), CodingAgentError> {
    // Keep the old, stricter constraint until both replacement indexes exist.
    // A failed CREATE or interrupted upgrade must never expose an unguarded phase.
    connection.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS one_cancel_effect_per_attempt_phase
         ON agent_run_operations(run_id, attempt_id, phase_generation)
             WHERE kind_json = '\"cancel_agent\"';
         CREATE UNIQUE INDEX IF NOT EXISTS one_finish_effect_per_attempt_phase
         ON agent_run_operations(run_id, attempt_id, phase_generation)
              WHERE kind_json = '\"finish_interactive\"';
          DROP INDEX IF EXISTS one_lifecycle_effect_per_attempt_phase;",
    )?;
    Ok(())
}

fn load_assessment_intent(
    connection: &Connection,
    store: &RunStore,
    run_id: &RunId,
    attempt_id: &str,
) -> Result<Option<AssessmentIntentBinding>, CodingAgentError> {
    let row: Option<(String, String, i64, Option<String>)> = connection
        .query_row(
            "SELECT assessment_id, record_json, created_at_ms, finalization_evidence_id
             FROM agent_run_assessment_intents
             WHERE run_id = ?1 AND attempt_id = ?2",
            params![run_id.as_str(), attempt_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    row.map(
        |(assessment_id, json, created_at_ms, finalization_evidence_id)| {
            let binding: AssessmentIntentBinding = decode(&json, "assessment intent")?;
            validate_assessment_intent_record(
                &binding,
                run_id,
                attempt_id,
                &assessment_id,
                nonnegative_u64(created_at_ms, "assessment intent timestamp")?,
            )?;
            validate_assessment_finalization_link(
                connection,
                store,
                &binding,
                run_id,
                attempt_id,
                finalization_evidence_id.as_deref(),
            )?;
            Ok(binding)
        },
    )
    .transpose()
}

fn validate_assessment_finalization_link(
    connection: &Connection,
    store: &RunStore,
    binding: &AssessmentIntentBinding,
    run_id: &RunId,
    attempt_id: &str,
    finalization_evidence_id: Option<&str>,
) -> Result<(), CodingAgentError> {
    let indexed_reference = find_finalization_input_reference(connection, run_id, attempt_id)?;
    match (finalization_evidence_id, indexed_reference) {
        (None, None) => Ok(()),
        (Some(linked_id), Some(indexed_reference))
            if linked_id == indexed_reference.evidence_id
                && indexed_reference.attempt_id == attempt_id =>
        {
            let saved =
                load_saved_verified_finalization_inputs(connection, store, run_id, attempt_id)?
                    .ok_or_else(|| {
                        CodingAgentError::Corrupt(
                            "assessment intent links missing verified finalization inputs".into(),
                        )
                    })?;
            let manifest_reference = find_evidence_in_connection(
                connection,
                run_id,
                &binding.snapshot_manifest_evidence_id,
            )?
            .ok_or_else(|| {
                CodingAgentError::Corrupt(
                    "assessment intent snapshot manifest evidence is missing".into(),
                )
            })?;
            let target = super::validation::ValidationTargetSnapshot::new(
                binding.target_full_commit_id.clone(),
                binding.target_full_tree_id.clone(),
                binding.target_untracked_manifest_digest.clone(),
            )
            .map_err(|_| {
                CodingAgentError::Corrupt(
                    "assessment intent target differs from verified finalization inputs".into(),
                )
            })?;
            if saved.index_reference != indexed_reference
                || saved.report_reference.evidence_id != binding.report_evidence_id
                || saved.report_reference.sha256 != binding.report_digest
                || saved.target.full_commit_id != binding.target_full_commit_id
                || saved.target.full_tree_id != binding.target_full_tree_id
                || saved.target.untracked_manifest_digest
                    != binding.target_untracked_manifest_digest
                || saved.target.target_snapshot_digest != binding.target_snapshot_digest
                || target.digest() != binding.target_snapshot_digest
                || saved.target.artifact_manifest_digest != binding.snapshot_manifest_digest
                || binding.validation_receipts != saved.validation_receipt_references
                || manifest_reference.attempt_id != attempt_id
                || manifest_reference.sha256 != binding.snapshot_manifest_digest
                || manifest_reference.media_type != "application/json"
                || !manifest_reference.save_confirmed
            {
                return Err(CodingAgentError::Corrupt(
                    "assessment intent differs from its immutable finalization inputs".into(),
                ));
            }
            Ok(())
        }
        _ => Err(CodingAgentError::Corrupt(
            "assessment intent finalization link differs from its immutable index".into(),
        )),
    }
}

fn validate_assessment_intent_record(
    binding: &AssessmentIntentBinding,
    run_id: &RunId,
    attempt_id: &str,
    assessment_id: &str,
    created_at_ms: u64,
) -> Result<(), CodingAgentError> {
    if binding.run_id != *run_id
        || binding.attempt_id != attempt_id
        || binding.assessment_id != assessment_id
        || binding.created_at_ms != created_at_ms
        || !valid_sha256(&binding.approval_snapshot_digest)
        || !valid_sha256(&binding.approval_contract_digest)
        || !valid_sha256(&binding.judge_input_scope_digest)
        || !valid_sha256(&binding.system_prompt_digest)
        || !valid_sha256(&binding.judge_input_digest)
        || !valid_sha256(&binding.report_digest)
        || !valid_sha256(&binding.snapshot_manifest_digest)
        || !valid_sha256(&binding.target_untracked_manifest_digest)
        || !valid_sha256(&binding.target_snapshot_digest)
        || !valid_commit_id(&binding.target_full_commit_id)
        || !valid_commit_id(&binding.target_full_tree_id)
        || binding.target_full_commit_id.len() != binding.target_full_tree_id.len()
    {
        return Err(CodingAgentError::Corrupt(
            "assessment intent key or digest does not match its durable record".into(),
        ));
    }
    let mut validation_ids = BTreeSet::new();
    let mut receipt_ids = BTreeSet::new();
    if binding.validation_receipts.iter().any(|receipt| {
        validate_safe_id(&receipt.validation_id, "assessment validation id").is_err()
            || validate_safe_id(&receipt.receipt_id, "assessment receipt id").is_err()
            || !valid_sha256(&receipt.digest)
            || !valid_sha256(&receipt.command_definition_digest)
            || !valid_sha256(&receipt.permission_profile_digest)
            || !valid_sha256(&receipt.target_untracked_manifest_digest)
            || !valid_sha256(&receipt.target_snapshot_digest)
            || !valid_commit_id(&receipt.target_full_commit_id)
            || !valid_commit_id(&receipt.target_full_tree_id)
            || receipt.target_full_commit_id.len() != receipt.target_full_tree_id.len()
            || receipt.target_full_commit_id != binding.target_full_commit_id
            || receipt.target_full_tree_id != binding.target_full_tree_id
            || receipt.target_untracked_manifest_digest != binding.target_untracked_manifest_digest
            || receipt.target_snapshot_digest != binding.target_snapshot_digest
            || !validation_ids.insert(receipt.validation_id.as_str())
            || !receipt_ids.insert(receipt.receipt_id.as_str())
    }) {
        return Err(CodingAgentError::Corrupt(
            "assessment intent contains an invalid or mismatched validation receipt".into(),
        ));
    }
    Ok(())
}

fn validate_assessment_intent_scope(
    binding: &AssessmentIntentBinding,
    contract: &ApprovalAssessmentContract,
) -> Result<(), CodingAgentError> {
    if binding.judge_input_scope_digest != contract.judge_input_scope_digest() {
        return Err(CodingAgentError::Corrupt(
            "assessment intent scope digest does not match the immutable approval contract".into(),
        ));
    }
    Ok(())
}

fn assessment_validation_receipt_references(
    contract: &ApprovalAssessmentContract,
    run_id: &RunId,
    attempt_id: &str,
    request: &AssessmentIntentRequest,
) -> Result<Vec<AssessmentValidationReceiptReference>, CodingAgentError> {
    let receipts = &request.validation_receipts;
    if receipts.len() > 128 {
        return Err(CodingAgentError::Invalid(
            "assessment receipt references exceed the bounded item limit".into(),
        ));
    }
    let mut validation_ids = BTreeSet::new();
    let mut receipt_ids = BTreeSet::new();
    let mut bindings = Vec::with_capacity(receipts.len());
    for receipt in receipts {
        let approved = contract
            .validation_plan
            .iter()
            .find(|validation| validation.validation_id == receipt.validation_id());
        let Some(approved) = approved else {
            return Err(CodingAgentError::Invalid(
                "assessment receipt does not identify an approved validation".into(),
            ));
        };
        let command = approved.command.as_ref().ok_or_else(|| {
            CodingAgentError::Invalid(
                "an unavailable approved validation command cannot have a receipt".into(),
            )
        })?;
        validate_safe_id(receipt.receipt_id(), "assessment receipt id")?;
        if receipt.run_id() != run_id.as_str()
            || receipt.attempt_id() != attempt_id
            || receipt.required() != approved.required
            || receipt.permission_profile_digest() != command.profile_digest
            || !valid_sha256(receipt.binding_digest())
            || !valid_sha256(receipt.command_definition_digest())
            || receipt.target_full_commit_id() != request.target_full_commit_id
            || receipt.target_full_tree_id() != request.target_full_tree_id
            || receipt.target_untracked_manifest_digest()
                != request.target_untracked_manifest_digest
            || receipt.target_snapshot_digest() != request.target_snapshot_digest
            || !valid_commit_id(receipt.target_full_commit_id())
            || !valid_commit_id(receipt.target_full_tree_id())
            || !valid_sha256(receipt.target_untracked_manifest_digest())
            || !validation_ids.insert(receipt.validation_id())
            || !receipt_ids.insert(receipt.receipt_id())
        {
            return Err(CodingAgentError::Invalid(
                "assessment receipt does not match its approved attempt, validation, or target"
                    .into(),
            ));
        }
        let receipt_bytes = serde_json::to_vec(receipt)?;
        if receipt_bytes.len() > super::validation::MAX_VALIDATION_RECORD_BYTES {
            return Err(CodingAgentError::Invalid(
                "assessment receipt exceeds its bounded stored size".into(),
            ));
        }
        bindings.push(AssessmentValidationReceiptReference {
            validation_id: receipt.validation_id().into(),
            required: receipt.required(),
            receipt_id: receipt.receipt_id().into(),
            digest: format!("{:x}", Sha256::digest(&receipt_bytes)),
            command_definition_digest: receipt.command_definition_digest().into(),
            permission_profile_digest: receipt.permission_profile_digest().into(),
            target_full_commit_id: receipt.target_full_commit_id().into(),
            target_full_tree_id: receipt.target_full_tree_id().into(),
            target_untracked_manifest_digest: receipt.target_untracked_manifest_digest().into(),
            target_snapshot_digest: receipt.target_snapshot_digest().into(),
        });
    }
    bindings.sort_by(|left, right| left.validation_id.cmp(&right.validation_id));
    Ok(bindings)
}

fn validate_report_bytes(
    contract: &ApprovalAssessmentContract,
    run_id: &RunId,
    attempt_id: &str,
    bytes: &[u8],
) -> Result<(), CodingAgentError> {
    let task_kind = match contract.purpose {
        RunPurpose::Investigation => TaskKind::Investigation,
        RunPurpose::Implementation => TaskKind::Implementation,
    };
    let acceptance = contract
        .criteria
        .iter()
        .map(|criterion| (criterion.criterion_id.clone(), criterion.required))
        .collect::<std::collections::BTreeMap<_, _>>();
    let validation_plan = contract
        .validation_plan
        .iter()
        .map(|validation| (validation.validation_id.clone(), validation.required))
        .collect::<std::collections::BTreeMap<_, _>>();
    let expectation = ReportExpectation::new(
        run_id.as_str(),
        attempt_id,
        task_kind,
        acceptance,
        validation_plan,
    )
    .map_err(|error| CodingAgentError::Invalid(format!("invalid report expectation: {error}")))?;
    validate_report(bytes, &expectation)
        .map(|_| ())
        .map_err(|error| {
            CodingAgentError::Conflict(format!("assessment report is invalid: {error}"))
        })
}

fn recheck_assessment_evidence_reference(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    attempt_id: &str,
    expected: &EvidenceReference,
) -> Result<(), CodingAgentError> {
    let actual = find_evidence_in_transaction(transaction, run_id, &expected.evidence_id)?
        .ok_or_else(|| {
            CodingAgentError::Conflict("assessment evidence disappeared before CAS".into())
        })?;
    if &actual != expected || actual.attempt_id != attempt_id || !actual.save_confirmed {
        return Err(CodingAgentError::Conflict(
            "assessment evidence changed ownership or metadata before CAS".into(),
        ));
    }
    Ok(())
}

fn load_completion_authorization(
    transaction: &Transaction<'_>,
    authorization_id: &str,
) -> Result<Option<RunCompletionAuthorization>, CodingAgentError> {
    transaction
        .query_row(
            "SELECT record_json FROM agent_run_completion_authorizations
             WHERE authorization_id = ?1",
            [authorization_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|json| decode(&json, "completion authorization"))
        .transpose()
}

fn load_completion_authorization_for_intent(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    idempotency_key: &str,
) -> Result<Option<RunCompletionAuthorization>, CodingAgentError> {
    transaction
        .query_row(
            "SELECT record_json FROM agent_run_completion_authorizations
             WHERE run_id = ?1 AND intent_idempotency_key = ?2",
            params![run_id.as_str(), idempotency_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|json| decode(&json, "completion authorization"))
        .transpose()
}

fn completion_authorization_matches_candidate(
    authorization: &RunCompletionAuthorization,
    candidate: &CompletionCandidate,
) -> bool {
    let run_revision_matches = if authorization.status == CompletionAuthorizationStatus::Completed
        && authorization.action != CompletionActionKind::HumanCorrection
    {
        authorization.run_revision.checked_add(1) == Some(candidate.run.revision)
            && candidate.run.state == RunState::Done
    } else {
        authorization.run_revision == candidate.run.revision
    };
    authorization.run_id == candidate.run.snapshot.run_id
        && authorization.issue_id == candidate.run.snapshot.issue_id
        && completion_attempt_matches(authorization, candidate)
        && run_revision_matches
        && authorization.snapshot_sha256 == candidate.snapshot_sha256
        && authorization.evidence_sha256 == candidate.evidence_sha256
        && authorization.commit_id == candidate.commit_id
        && completion_authorization_intent_matches(authorization)
        && authorization.approved_settings_version
            == candidate
                .run
                .snapshot
                .approval
                .as_ref()
                .and_then(|approval| approval.settings_version)
}

fn completion_authorization_intent_sha256(
    authorization: &RunCompletionAuthorization,
) -> Result<String, CodingAgentError> {
    let marker = format!(
        " [coding-agent-intent-sha256={}]",
        authorization.intent_sha256
    );
    let audit_reason = authorization
        .audit_reason
        .strip_suffix(&marker)
        .unwrap_or(&authorization.audit_reason);
    let canonical = serde_json::to_vec(&(
        (
            &authorization.run_id,
            &authorization.issue_id,
            &authorization.attempt_id,
            authorization.action,
            &authorization.actor_id,
            &authorization.policy_identity,
            authorization.policy_version,
            &authorization.snapshot_sha256,
            authorization.approved_settings_version,
        ),
        (
            authorization.execution_mode,
            &authorization.commit_id,
            &authorization.evidence_sha256,
            &authorization.report_sha256,
            authorization.marker_revision,
            authorization.target_issue_revision,
            authorization.run_revision,
            &authorization.intent_idempotency_key,
            &authorization.primary_tracker_identity,
            audit_reason,
        ),
    ))?;
    Ok(format!("{:x}", Sha256::digest(canonical)))
}

pub(super) fn completion_authorization_intent_matches(
    authorization: &RunCompletionAuthorization,
) -> bool {
    valid_sha256(&authorization.intent_sha256)
        && completion_authorization_intent_sha256(authorization)
            .is_ok_and(|digest| digest == authorization.intent_sha256)
        && valid_sha256(&authorization.expected_manager_request_digest)
        && super::completion::manager_request_digest_for_authorization(authorization)
            .is_ok_and(|digest| digest == authorization.expected_manager_request_digest)
}

fn completion_attempt_matches(
    authorization: &RunCompletionAuthorization,
    candidate: &CompletionCandidate,
) -> bool {
    !authorization.attempt_id.is_empty()
        && candidate.run.current_attempt_id.as_deref() == Some(authorization.attempt_id.as_str())
}

fn validate_primary_operation_receipt(
    receipt: &PrimaryOperationReceipt,
) -> Result<(), CodingAgentError> {
    validate_bounded_text(&receipt.manager_operation_id, 512, "manager operation ID")?;
    validate_bounded_text(
        &receipt.downstream_operation_id,
        512,
        "downstream operation ID",
    )?;
    validate_bounded_text(
        &receipt.manager_idempotency_key,
        256,
        "manager idempotency key",
    )?;
    validate_bounded_text(
        &receipt.idempotency_scope,
        1_024,
        "operation idempotency scope",
    )?;
    validate_bounded_text(&receipt.request_schema, 512, "operation request schema")?;
    validate_bounded_text(
        &receipt.backend_contract_fingerprint,
        256,
        "primary backend contract fingerprint",
    )?;
    for (value, field) in [
        (
            receipt.server_identity.schema_fingerprint.as_str(),
            "primary tracker schema fingerprint",
        ),
        (
            receipt.server_identity.data_root_id.as_str(),
            "primary tracker data-root identity",
        ),
        (
            receipt.server_identity.contract_fingerprint.as_str(),
            "primary tracker contract fingerprint",
        ),
    ] {
        validate_bounded_text(value, 256, field)?;
    }
    if !valid_sha256(&receipt.request_digest)
        || receipt.request_schema != super::completion::MANAGER_DURABLE_BACKEND_REQUEST_SCHEMA
    {
        return Err(CodingAgentError::Invalid(
            "primary operation receipt request identity is invalid".into(),
        ));
    }
    Ok(())
}

pub(super) fn completion_manager_idempotency_key(run_id: &RunId, intent_key: &str) -> String {
    let identity = format!("{}\0{intent_key}", run_id.as_str());
    format!(
        "coding-agent-completion-v1-{:x}",
        Sha256::digest(identity.as_bytes())
    )
}

fn ensure_no_dispatched_completion(
    transaction: &Transaction<'_>,
    run_id: &RunId,
) -> Result<(), CodingAgentError> {
    let dispatched: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM agent_run_completion_authorizations
             WHERE run_id = ?1 AND status_json = ?2
         )",
        params![
            run_id.as_str(),
            serde_json::to_string(&CompletionAuthorizationStatus::Dispatched)?
        ],
        |row| row.get(0),
    )?;
    if dispatched {
        return Err(CodingAgentError::Conflict(
            "run evidence is frozen while completion is awaiting tracker reconciliation".into(),
        ));
    }
    Ok(())
}

fn has_attempt_cancellation_effect(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    attempt_id: &str,
) -> Result<bool, CodingAgentError> {
    transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM agent_run_operations
                 WHERE run_id = ?1 AND attempt_id = ?2 AND kind_json = ?3
             )",
            params![
                run_id.as_str(),
                attempt_id,
                serde_json::to_string(&super::model::DurableOperationKind::CancelAgent)?,
            ],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn has_phase_cancellation_effect(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    attempt_id: &str,
    phase_generation: u64,
) -> Result<bool, CodingAgentError> {
    transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM agent_run_operations
                 WHERE run_id = ?1 AND attempt_id = ?2 AND phase_generation = ?3
                   AND kind_json = ?4
             )",
            params![
                run_id.as_str(),
                attempt_id,
                sql_i64(phase_generation, "operation phase generation")?,
                serde_json::to_string(&super::model::DurableOperationKind::CancelAgent)?,
            ],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn ensure_no_attempt_cancellation_effect(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    attempt_id: &str,
) -> Result<(), CodingAgentError> {
    if has_attempt_cancellation_effect(transaction, run_id, attempt_id)? {
        return Err(CodingAgentError::Conflict(
            "publication is fenced by a cancellation intent for this attempt".into(),
        ));
    }
    Ok(())
}

fn ensure_no_other_dispatched_completion(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    authorization_id: &str,
) -> Result<(), CodingAgentError> {
    let dispatched: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM agent_run_completion_authorizations
             WHERE run_id = ?1 AND authorization_id != ?2 AND status_json = ?3
         )",
        params![
            run_id.as_str(),
            authorization_id,
            serde_json::to_string(&CompletionAuthorizationStatus::Dispatched)?
        ],
        |row| row.get(0),
    )?;
    if dispatched {
        return Err(CodingAgentError::Conflict(
            "another completion authorization is already dispatched".into(),
        ));
    }
    Ok(())
}

fn completion_candidates_match(left: &CompletionCandidate, right: &CompletionCandidate) -> bool {
    if left.human_semantic_issue_revision != right.human_semantic_issue_revision {
        return false;
    }
    left.run.snapshot == right.run.snapshot
        && left.run.revision == right.run.revision
        && left.run.state == right.run.state
        && left.run.current_attempt_id == right.run.current_attempt_id
        && left.assessment == right.assessment
        && left.commit_id == right.commit_id
        && left.marker_evidence_references == right.marker_evidence_references
        && left.marker_revision == right.marker_revision
        && left.snapshot_sha256 == right.snapshot_sha256
        && left.evidence_sha256 == right.evidence_sha256
}

fn completion_authorization_matches_request(
    existing: &RunCompletionAuthorization,
    candidate: &CompletionCandidate,
    request: &NewCompletionAuthorization,
) -> bool {
    completion_authorization_intent_matches(existing)
        && existing.run_id == candidate.run.snapshot.run_id
        && existing.issue_id == candidate.run.snapshot.issue_id
        && existing.action == request.action
        && existing.actor_id == request.actor_id
        && existing.policy_identity == request.policy_identity
        && existing.policy_version == request.policy_version
        && existing.snapshot_sha256 == candidate.snapshot_sha256
        && existing.approved_settings_version
            == candidate
                .run
                .snapshot
                .approval
                .as_ref()
                .and_then(|approval| approval.settings_version)
        && existing.execution_mode == candidate.run.snapshot.execution_mode
        && existing.commit_id == candidate.commit_id
        && existing.evidence_sha256 == candidate.evidence_sha256
        && existing.report_sha256 == request.report_sha256
        && existing.completion_report_protobuf == request.completion_report_protobuf
        && existing.audit_reason == request.audit_reason
        && existing.marker_revision == request.marker_revision
        && existing.target_issue_revision == request.target_issue_revision
        && existing.intent_idempotency_key == request.intent_idempotency_key
}

fn revoke_prepared_auto_authorizations(
    transaction: &Transaction<'_>,
    run_id: &RunId,
) -> Result<(), CodingAgentError> {
    let dispatched: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_run_completion_authorizations
         WHERE run_id = ?1 AND action_json = ?2 AND status_json = ?3)",
        params![
            run_id.as_str(),
            serde_json::to_string(&CompletionActionKind::AutoDone)?,
            serde_json::to_string(&CompletionAuthorizationStatus::Dispatched)?,
        ],
        |row| row.get(0),
    )?;
    if dispatched {
        return Err(CodingAgentError::Conflict(
            "an auto-completion request is unresolved; reconcile it before human review".into(),
        ));
    }
    let prepared_status = serde_json::to_string(&CompletionAuthorizationStatus::Prepared)?;
    let auto_action = serde_json::to_string(&CompletionActionKind::AutoDone)?;
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT authorization_id, record_json FROM agent_run_completion_authorizations
             WHERE run_id = ?1 AND action_json = ?2 AND status_json = ?3",
        )?;
        let rows = statement.query_map(
            params![run_id.as_str(), auto_action, prepared_status],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (authorization_id, record_json) in rows {
        let mut record: RunCompletionAuthorization =
            decode(&record_json, "completion authorization")?;
        record.status = CompletionAuthorizationStatus::Revoked;
        record.updated_at_ms = now_ms();
        transaction.execute(
            "UPDATE agent_run_completion_authorizations
             SET status_json = ?1, record_json = ?2 WHERE authorization_id = ?3",
            params![
                serde_json::to_string(&record.status)?,
                serde_json::to_string(&record)?,
                authorization_id,
            ],
        )?;
    }
    Ok(())
}

fn load_run(
    connection: &Connection,
    run_id: &RunId,
) -> Result<Option<AgentRunRecord>, CodingAgentError> {
    let row: Option<(String, i64, String, String, Option<String>, i64)> = connection
        .query_row(
            "SELECT snapshot_json, revision, state_json, stage_json, current_attempt_id,
                    start_phase_generation
             FROM agent_runs WHERE run_id = ?1",
            [run_id.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((
        snapshot_json,
        revision,
        state_json,
        stage_json,
        current_attempt_id,
        start_phase_generation,
    )) = row
    else {
        return Ok(None);
    };
    let snapshot = decode_agent_run_snapshot(&snapshot_json)?;
    let state: RunState = decode(&state_json, "run state")?;
    let stage: RunStage = decode(&stage_json, "run stage")?;
    if stage != state.stage() || snapshot.run_id != *run_id {
        return Err(CodingAgentError::Corrupt(
            "stored run identity or stage does not match its state".into(),
        ));
    }
    let mut attempts_statement = connection.prepare(
        "SELECT attempt_id, ordinal, created_at_ms FROM agent_run_attempts
         WHERE run_id = ?1 ORDER BY ordinal",
    )?;
    let attempts = attempts_statement
        .query_map([run_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(attempt_id, ordinal, created_at_ms)| {
            Ok(RunAttempt {
                attempt_id,
                ordinal: u32::try_from(ordinal)
                    .map_err(|_| CodingAgentError::Corrupt("attempt ordinal overflow".into()))?,
                created_at_ms: nonnegative_u64(created_at_ms, "attempt timestamp")?,
            })
        })
        .collect::<Result<Vec<_>, CodingAgentError>>()?;
    if attempts.len() != snapshot.attempt_ids.len()
        || attempts
            .iter()
            .map(|attempt| attempt.attempt_id.as_str())
            .ne(snapshot.attempt_ids.iter().map(String::as_str))
    {
        return Err(CodingAgentError::Corrupt(
            "stored attempts do not match immutable run snapshot".into(),
        ));
    }
    if current_attempt_id
        .as_ref()
        .is_some_and(|current| !snapshot.attempt_ids.contains(current))
    {
        return Err(CodingAgentError::Corrupt(
            "current attempt is not present in the immutable snapshot".into(),
        ));
    }
    let evidence = load_evidence(connection, run_id)?;
    let execution_evidence_json: Option<String> = current_attempt_id
        .as_deref()
        .map(|attempt_id| {
            connection
                .query_row(
                    "SELECT result_json FROM agent_run_execution_evidence
                     WHERE run_id = ?1 AND attempt_id = ?2",
                    params![run_id.as_str(), attempt_id],
                    |row| row.get(0),
                )
                .optional()
        })
        .transpose()?
        .flatten();
    let execution_evidence = execution_evidence_json
        .as_deref()
        .map(|json| decode(json, "execution evidence"))
        .transpose()?;
    let (interactive_finish_request, interactive_finalization) = if let Some(attempt_id) =
        current_attempt_id.as_deref()
    {
        let finish_request: Option<(i64, i64)> = connection
            .query_row(
                "SELECT profile_version, requested_at_ms
                     FROM agent_run_interactive_finish_requests
                     WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), attempt_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let finish_request = finish_request
            .map(|(profile_version, requested_at_ms)| {
                let request = InteractiveFinishRequest {
                    run_id: run_id.clone(),
                    attempt_id: attempt_id.into(),
                    profile_version: nonnegative_u64(profile_version, "finish profile version")?,
                    requested_at_ms: nonnegative_u64(requested_at_ms, "finish request timestamp")?,
                };
                if request.profile_version != snapshot.profile.version {
                    return Err(CodingAgentError::Corrupt(
                        "finish request profile differs from the immutable snapshot".into(),
                    ));
                }
                Ok(request)
            })
            .transpose()?;
        let finalization_json: Option<String> = connection
            .query_row(
                "SELECT evidence_json FROM agent_run_interactive_finalizations
                     WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), attempt_id],
                |row| row.get(0),
            )
            .optional()?;
        let finalization = finalization_json
            .as_deref()
            .map(|json| {
                decode::<StoredInteractiveFinalizationEvidence>(json, "interactive finalization")
            })
            .transpose()?
            .map(StoredInteractiveFinalizationEvidence::into_public)
            .transpose()?;
        if finalization
            .as_ref()
            .is_some_and(|evidence| evidence.profile_version() != snapshot.profile.version)
        {
            return Err(CodingAgentError::Corrupt(
                "finalization profile differs from the immutable snapshot".into(),
            ));
        }
        (finish_request, finalization)
    } else {
        (None, None)
    };
    Ok(Some(AgentRunRecord {
        snapshot,
        revision: nonnegative_u64(revision, "run revision")?,
        state,
        stage,
        start_phase_generation: nonnegative_u64(start_phase_generation, "start phase generation")?,
        current_attempt_id,
        attempts,
        evidence,
        execution_evidence,
        interactive_finish_request,
        interactive_finalization,
    }))
}

fn decode_stored_snapshot(value: &str) -> Result<StoredAgentRunSnapshot, CodingAgentError> {
    let stored: StoredAgentRunSnapshot = decode(value, "AgentRun snapshot")?;
    stored
        .validate()
        .map_err(|error| CodingAgentError::Corrupt(error.to_string()))?;
    Ok(stored)
}

fn decode_agent_run_snapshot(value: &str) -> Result<AgentRunSnapshot, CodingAgentError> {
    Ok(decode_stored_snapshot(value)?.snapshot)
}

fn load_evidence(
    connection: &Connection,
    run_id: &RunId,
) -> Result<Vec<EvidenceReference>, CodingAgentError> {
    let mut statement = connection.prepare(
        "SELECT evidence_id, attempt_id, name, media_type, sha256, size_bytes,
                save_confirmed, created_at_ms
         FROM agent_run_evidence WHERE run_id = ?1 ORDER BY created_at_ms, evidence_id",
    )?;
    let rows = statement.query_map([run_id.as_str()], evidence_from_row)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn find_evidence_in_connection(
    connection: &Connection,
    run_id: &RunId,
    evidence_id: &str,
) -> Result<Option<EvidenceReference>, CodingAgentError> {
    connection
        .query_row(
            "SELECT evidence_id, attempt_id, name, media_type, sha256, size_bytes,
                    save_confirmed, created_at_ms
             FROM agent_run_evidence WHERE run_id = ?1 AND evidence_id = ?2",
            params![run_id.as_str(), evidence_id],
            evidence_from_row,
        )
        .optional()
        .map_err(Into::into)
}

fn find_evidence_in_transaction(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    evidence_id: &str,
) -> Result<Option<EvidenceReference>, CodingAgentError> {
    transaction
        .query_row(
            "SELECT evidence_id, attempt_id, name, media_type, sha256, size_bytes,
                    save_confirmed, created_at_ms
             FROM agent_run_evidence WHERE run_id = ?1 AND evidence_id = ?2",
            params![run_id.as_str(), evidence_id],
            evidence_from_row,
        )
        .optional()
        .map_err(Into::into)
}

const DELEGATION_COLUMNS: &str =
    "plan_id, request_json, issue_revision, settings_revision, created_at_ms, run_id";

fn delegation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DelegationPlanRecord> {
    let request = from_json_sql::<DelegationPlanRequest>(&row.get::<_, String>(1)?)?;
    request.validate().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let run_id = row
        .get::<_, Option<String>>(5)?
        .map(|id| RunId::new(&id))
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    Ok(DelegationPlanRecord {
        delegation_plan_id: row.get(0)?,
        request,
        issue_revision: row.get(2)?,
        settings_revision: row.get(3)?,
        created_at_ms: row.get(4)?,
        run_id,
    })
}

fn evidence_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EvidenceReference> {
    let evidence_id: String = row.get(0)?;
    let attempt_id: String = row.get(1)?;
    let size_bytes: i64 = row.get(5)?;
    let save_confirmed: i64 = row.get(6)?;
    let created_at_ms: i64 = row.get(7)?;
    if !is_safe_id(&evidence_id)
        || !is_safe_id(&attempt_id)
        || size_bytes < 0
        || created_at_ms < 0
        || !matches!(save_confirmed, 0 | 1)
    {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(
                "invalid evidence metadata in database",
            )),
        ));
    }
    let sha256: String = row.get(4)?;
    if !valid_sha256(&sha256) {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other("invalid evidence digest in database")),
        ));
    }
    Ok(EvidenceReference {
        evidence_id,
        attempt_id,
        name: row.get(2)?,
        media_type: row.get(3)?,
        sha256,
        size_bytes: size_bytes as u64,
        save_confirmed: save_confirmed == 1,
        created_at_ms: created_at_ms as u64,
    })
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredInteractiveFinalizationEvidence {
    profile_version: u64,
    cli_exit: ProcessExitObservation,
    #[serde(default, deserialize_with = "deserialize_present_non_null")]
    invocation_kind: Option<ExecutionInvocationKind>,
    #[serde(default, deserialize_with = "deserialize_present_non_null")]
    shell_observation: Option<ShellProcessObservation>,
    #[serde(default)]
    shell_exit: Option<ProcessExitObservation>,
    child_writers: ChildWritersStatus,
    workspace_freeze: WorkspaceFreezeStatus,
}

fn deserialize_present_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

impl From<&InteractiveFinalizationEvidence> for StoredInteractiveFinalizationEvidence {
    fn from(evidence: &InteractiveFinalizationEvidence) -> Self {
        Self {
            profile_version: evidence.profile_version(),
            cli_exit: evidence.cli_exit().clone(),
            invocation_kind: Some(evidence.invocation_kind()),
            shell_observation: Some(evidence.shell_observation().clone()),
            shell_exit: Some(evidence.shell_exit().clone()),
            child_writers: evidence.child_writers(),
            workspace_freeze: evidence.workspace_freeze().clone(),
        }
    }
}

impl StoredInteractiveFinalizationEvidence {
    fn into_public(self) -> Result<InteractiveFinalizationEvidence, CodingAgentError> {
        let (invocation_kind, shell_observation) =
            match (self.invocation_kind, self.shell_observation) {
                (Some(invocation_kind), Some(shell_observation)) => {
                    let shell_exit = self.shell_exit.as_ref().ok_or_else(|| {
                        CodingAgentError::Corrupt(
                        "typed interactive finalization is missing its compatibility shell exit"
                            .into(),
                    )
                    })?;
                    let compatibility_shell_exit_matches = match &shell_observation {
                        ShellProcessObservation::Spawned { exit } => shell_exit == exit,
                        ShellProcessObservation::NotSpawned | ShellProcessObservation::Unknown => {
                            shell_exit == &ProcessExitObservation::Unknown
                        }
                    };
                    let invocation_matches_observation = !matches!(
                        (invocation_kind, &shell_observation),
                        (
                            ExecutionInvocationKind::DirectProcess,
                            ShellProcessObservation::Spawned { .. },
                        ) | (
                            ExecutionInvocationKind::ShellWrapper,
                            ShellProcessObservation::NotSpawned,
                        )
                    );
                    if !compatibility_shell_exit_matches || !invocation_matches_observation {
                        return Err(CodingAgentError::Corrupt(
                            "interactive finalization invocation and shell observations conflict"
                                .into(),
                        ));
                    }
                    (invocation_kind, shell_observation)
                }
                (None, None) => {
                    let legacy_shell_exit = self.shell_exit;
                    // Decode the old shell-exit shape, but do not trust its inferred wrapper kind:
                    // legacy rows did not persist which invocation was approved.
                    let legacy_projection = InteractiveFinalizationEvidence::from_host_observations(
                        self.profile_version,
                        self.cli_exit.clone(),
                        legacy_shell_exit
                            .clone()
                            .unwrap_or(ProcessExitObservation::Unknown),
                        self.child_writers,
                        self.workspace_freeze.clone(),
                    );
                    let shell_observation = match legacy_projection.shell_observation() {
                        ShellProcessObservation::Spawned { exit } => {
                            ShellProcessObservation::Spawned { exit: exit.clone() }
                        }
                        ShellProcessObservation::NotSpawned | ShellProcessObservation::Unknown => {
                            match legacy_shell_exit {
                                Some(ProcessExitObservation::Unknown) | None => {
                                    ShellProcessObservation::Unknown
                                }
                                Some(exit) => ShellProcessObservation::Spawned { exit },
                            }
                        }
                    };
                    (ExecutionInvocationKind::Unknown, shell_observation)
                }
                _ => {
                    return Err(CodingAgentError::Corrupt(
                        "interactive finalization has partial invocation metadata".into(),
                    ));
                }
            };
        Ok(
            InteractiveFinalizationEvidence::from_host_observations_with_invocation(
                self.profile_version,
                invocation_kind,
                self.cli_exit,
                shell_observation,
                self.child_writers,
                self.workspace_freeze,
            ),
        )
    }
}

fn required_execution_evidence(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    attempt_id: &str,
) -> Result<RunEvidenceAssessment, CodingAgentError> {
    transaction
        .query_row(
            "SELECT result_json FROM agent_run_execution_evidence
             WHERE run_id = ?1 AND attempt_id = ?2",
            params![run_id.as_str(), attempt_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .as_deref()
        .map(|json| decode(json, "execution evidence"))
        .transpose()?
        .ok_or_else(|| {
            CodingAgentError::Conflict(format!(
                "execution evidence is not recorded for attempt {attempt_id}"
            ))
        })
}

fn operation_from_row<'a>(
    run_id: &'a RunId,
    idempotency_key: &'a str,
) -> impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<DurableOperationRecord> + 'a {
    move |row| {
        let kind_json: String = row.get(2)?;
        let request_json: String = row.get(4)?;
        let status_json: String = row.get(5)?;
        let result_json: Option<String> = row.get(6)?;
        Ok(DurableOperationRecord {
            operation_id: row.get(0)?,
            run_id: run_id.clone(),
            attempt_id: row.get(1)?,
            idempotency_key: idempotency_key.into(),
            kind: from_json_sql(&kind_json)?,
            payload_hash: row.get(3)?,
            request: from_json_sql(&request_json)?,
            status: from_json_sql(&status_json)?,
            result: result_json.as_deref().map(from_json_sql).transpose()?,
            created_at_ms: u64::try_from(row.get::<_, i64>(7)?).unwrap_or_default(),
            updated_at_ms: u64::try_from(row.get::<_, i64>(8)?).unwrap_or_default(),
        })
    }
}

type OperationRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    i64,
    i64,
);

fn operation_tuple_to_record(
    row: OperationRow,
) -> Result<DurableOperationRecord, CodingAgentError> {
    let run_id = RunId::new(row.1.clone())?;
    Ok(DurableOperationRecord {
        operation_id: row.0,
        run_id,
        attempt_id: row.2,
        idempotency_key: row.3,
        kind: decode(&row.4, "operation kind")?,
        payload_hash: row.5,
        request: decode(&row.6, "operation request")?,
        status: decode(&row.7, "operation status")?,
        result: row
            .8
            .as_deref()
            .map(|value| decode(value, "operation result"))
            .transpose()?,
        created_at_ms: nonnegative_u64(row.9, "operation creation time")?,
        updated_at_ms: nonnegative_u64(row.10, "operation update time")?,
    })
}

fn decode<T: DeserializeOwned>(value: &str, kind: &str) -> Result<T, CodingAgentError> {
    serde_json::from_str(value)
        .map_err(|error| CodingAgentError::Corrupt(format!("{kind} JSON: {error}")))
}

fn from_json_sql<T: DeserializeOwned>(value: &str) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn run_exists(connection: &Connection, run_id: &RunId) -> Result<bool, CodingAgentError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_runs WHERE run_id = ?1)",
            [run_id.as_str()],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn ensure_current_attempt_in_connection(
    connection: &Connection,
    run_id: &RunId,
    attempt_id: &str,
) -> Result<(), CodingAgentError> {
    let current: Option<Option<String>> = connection
        .query_row(
            "SELECT current_attempt_id FROM agent_runs WHERE run_id = ?1",
            [run_id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    match current {
        None => Err(CodingAgentError::NotFound(format!("run {run_id}"))),
        Some(Some(current)) if current == attempt_id => Ok(()),
        Some(_) => Err(CodingAgentError::Conflict(
            "evidence must belong to the current attempt".into(),
        )),
    }
}

fn attempt_ordinal(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    attempt_id: &str,
) -> Result<i64, CodingAgentError> {
    transaction
        .query_row(
            "SELECT ordinal FROM agent_run_attempts
             WHERE run_id = ?1 AND attempt_id = ?2",
            params![run_id.as_str(), attempt_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn expected_next_attempt_ordinal(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    _current_attempt_id: Option<&str>,
) -> Result<i64, CodingAgentError> {
    let start_kind = serde_json::to_string(&super::model::DurableOperationKind::StartAgent)?;
    let latest_started_ordinal: Option<i64> = transaction.query_row(
        "SELECT MAX(attempt.ordinal)
         FROM agent_run_operations operation
         JOIN agent_run_attempts attempt
           ON attempt.run_id = operation.run_id AND attempt.attempt_id = operation.attempt_id
         WHERE operation.run_id = ?1 AND operation.kind_json = ?2",
        params![run_id.as_str(), start_kind],
        |row| row.get(0),
    )?;
    latest_started_ordinal.map_or(Ok(0), |ordinal| {
        ordinal
            .checked_add(1)
            .ok_or_else(|| CodingAgentError::Corrupt("attempt ordinal overflow".into()))
    })
}

fn validate_operation_request(request: &DurableOperationRequest) -> Result<(), CodingAgentError> {
    match request {
        DurableOperationRequest::PrepareWorkspace {
            repository,
            base_commit,
            branch,
        } => {
            repository.validate()?;
            if !valid_commit_id(base_commit) || !valid_branch(branch) {
                return Err(CodingAgentError::Invalid(
                    "invalid workspace preparation intent".into(),
                ));
            }
        }
        DurableOperationRequest::StartAgent {
            workflow_definition_hash,
            attempt_id,
            ..
        } => {
            validate_safe_id(attempt_id, "attempt id")?;
            if !valid_sha256(workflow_definition_hash) {
                return Err(CodingAgentError::Invalid(
                    "agent start intent requires a workflow digest".into(),
                ));
            }
        }
        DurableOperationRequest::ReassessSemantics {
            issue_revision,
            source_sha256,
            report_reference,
            ..
        } => {
            if *issue_revision == 0 || !valid_sha256(source_sha256) || !is_safe_id(report_reference)
            {
                return Err(CodingAgentError::Invalid(
                    "invalid human reassessment intent".into(),
                ));
            }
        }
        DurableOperationRequest::VerifyEvidence {
            evidence_ids,
            commit_id,
        } => {
            if evidence_ids.iter().any(|id| !is_safe_id(id))
                || commit_id.as_deref().is_some_and(|id| !valid_commit_id(id))
            {
                return Err(CodingAgentError::Invalid(
                    "invalid evidence verification intent".into(),
                ));
            }
        }
        DurableOperationRequest::PublishBranch { branch, commit_id }
        | DurableOperationRequest::CreatePullRequest {
            branch, commit_id, ..
        } => {
            if !valid_branch(branch) || !valid_commit_id(commit_id) {
                return Err(CodingAgentError::Invalid(
                    "invalid publication intent".into(),
                ));
            }
            if let DurableOperationRequest::CreatePullRequest { base_branch, .. } = request
                && !valid_branch(base_branch)
            {
                return Err(CodingAgentError::Invalid(
                    "invalid pull request base branch".into(),
                ));
            }
        }
        DurableOperationRequest::UpdateIssueState {
            expected_revision,
            target_state,
        } => {
            if *expected_revision == 0
                || !matches!(target_state.as_str(), "OPEN" | "AWAITING_REVIEW" | "DONE")
            {
                return Err(CodingAgentError::Invalid(
                    "invalid Issue state intent".into(),
                ));
            }
        }
        DurableOperationRequest::CancelAgent => {}
        DurableOperationRequest::FinishInteractive { profile_version } => {
            if *profile_version == 0 {
                return Err(CodingAgentError::Invalid(
                    "interactive finish requires a valid profile version".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_operation_result(result: &DurableOperationResult) -> Result<(), CodingAgentError> {
    if result
        .result_digest
        .as_deref()
        .is_some_and(|digest| !valid_sha256(digest))
        || result.evidence_ids.iter().any(|id| !is_safe_id(id))
    {
        return Err(CodingAgentError::Invalid(
            "operation result may contain only digests and evidence references".into(),
        ));
    }
    Ok(())
}

fn validate_evidence_metadata(name: &str, media_type: &str) -> Result<(), CodingAgentError> {
    if name.is_empty()
        || name.len() > 120
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        || media_type.is_empty()
        || media_type.len() > 127
        || !media_type
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-+./".contains(&byte))
        || !media_type.contains('/')
    {
        return Err(CodingAgentError::Invalid(
            "evidence name or media type is not a bounded identifier".into(),
        ));
    }
    Ok(())
}

fn validate_idempotency_key(value: &str) -> Result<(), CodingAgentError> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
    {
        return Err(CodingAgentError::Invalid(
            "idempotency key must be a bounded opaque identifier".into(),
        ));
    }
    Ok(())
}

fn validate_bounded_text(
    value: &str,
    max_bytes: usize,
    description: &str,
) -> Result<(), CodingAgentError> {
    if value.trim().is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(CodingAgentError::Invalid(format!(
            "{description} must be bounded and contain no control characters"
        )));
    }
    Ok(())
}

fn validate_safe_id(value: &str, description: &str) -> Result<(), CodingAgentError> {
    if is_safe_id(value) {
        Ok(())
    } else {
        Err(CodingAgentError::Invalid(format!(
            "{description} must be a path-safe identifier"
        )))
    }
}

fn is_safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_https_receipt_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path() != "/"
}

fn valid_branch(branch: &str) -> bool {
    is_valid_branch_name(branch)
}

fn successful_report_is_durable(evidence: &RunEvidenceAssessment) -> bool {
    evidence.final_exit_code == Some(0)
        && evidence.end_present
        && !evidence.stream_error
        && !evidence.cancelled
        && !evidence.known_failure
        && evidence
            .report_reference
            .as_deref()
            .is_some_and(|id| !id.is_empty())
        && evidence.report_save_confirmed
        && evidence.evidence_integrity_verified
        && evidence.validation.state != ValidationState::Failed
}

fn sql_i64(value: u64, description: &str) -> Result<i64, CodingAgentError> {
    i64::try_from(value)
        .map_err(|_| CodingAgentError::Invalid(format!("{description} exceeds SQLite range")))
}

fn nonnegative_u64(value: i64, description: &str) -> Result<u64, CodingAgentError> {
    u64::try_from(value)
        .map_err(|_| CodingAgentError::Corrupt(format!("negative {description} in database")))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn sync_directory(directory: &Path) -> Result<(), CodingAgentError> {
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    #[cfg(not(unix))]
    return Err(CodingAgentError::Initialization(format!(
        "durable evidence requires directory fsync support: {}",
        directory.display()
    )));
    Ok(())
}

fn is_real_directory(path: &Path) -> Result<bool, CodingAgentError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(CodingAgentError::Invalid(
            "evidence retention path is not a real directory".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), CodingAgentError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            set_private_directory(path)?;
            Ok(())
        }
        Ok(_) => Err(CodingAgentError::Invalid(
            "coding-agent evidence directory is not a real directory".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => {
                set_private_directory(path)?;
                if let Some(parent) = path.parent() {
                    sync_directory(parent)?;
                }
                sync_directory(path)?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure_private_directory(path)
            }
            Err(error) => Err(error.into()),
        },
        Err(error) => Err(error.into()),
    }
}

fn reject_symlink(path: &Path) -> Result<(), CodingAgentError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(CodingAgentError::Invalid(
            "coding-agent database paths must not be symbolic links".into(),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn read_regular_nofollow(path: &Path) -> std::io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(std::io::Error::other("evidence path is not a regular file"));
    }
    fs::read(path)
}

fn read_regular_nofollow_bounded(path: &Path, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    let path_metadata = fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(std::io::Error::other("evidence path is not a regular file"));
    }
    if path_metadata.len() > max_bytes as u64 {
        return Err(std::io::Error::other(
            "evidence exceeds its bounded read limit",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > max_bytes as u64 {
        return Err(std::io::Error::other(
            "evidence exceeds its bounded read limit",
        ));
    }
    let read_limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(max_bytes));
    file.take(read_limit).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(std::io::Error::other(
            "evidence exceeds its bounded read limit",
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<(), CodingAgentError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> Result<(), CodingAgentError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> Result<(), CodingAgentError> {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> Result<(), CodingAgentError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_file_mode(_options: &mut OpenOptions) {}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use crate::coding_agent::{
        assessment::{AllowedEvidenceSource, EvidenceSourceKind, ProviderSelection},
        model::*,
        profile::*,
        repository::*,
    };
    use crate::data::DataPaths;

    use super::*;

    fn legacy_lifecycle_index_fixture() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE agent_run_operations (
                    run_id TEXT, attempt_id TEXT, phase_generation INTEGER, kind_json TEXT
                 );
                 CREATE UNIQUE INDEX one_lifecycle_effect_per_attempt_phase
                 ON agent_run_operations(run_id, attempt_id, phase_generation)
                 WHERE kind_json IN ('\"cancel_agent\"', '\"finish_interactive\"');
                 INSERT INTO agent_run_operations VALUES ('run', 'attempt', 0, '\"cancel_agent\"');",
            )
            .unwrap();
        connection
    }

    #[test]
    fn lifecycle_index_upgrade_preserves_kind_uniqueness_and_allows_finish_cancel_coexistence() {
        let connection = legacy_lifecycle_index_fixture();
        replace_lifecycle_effect_indexes(&connection).unwrap();
        replace_lifecycle_effect_indexes(&connection).unwrap();
        assert!(connection.execute(
            "INSERT INTO agent_run_operations VALUES ('run', 'attempt', 0, '\"cancel_agent\"')",
            [],
        ).is_err());
        connection.execute(
            "INSERT INTO agent_run_operations VALUES ('run', 'attempt', 0, '\"finish_interactive\"')",
            [],
        ).unwrap();
        assert!(connection.execute(
            "INSERT INTO agent_run_operations VALUES ('run', 'attempt', 0, '\"finish_interactive\"')",
            [],
        ).is_err());
    }

    #[test]
    fn failed_lifecycle_index_upgrade_keeps_the_legacy_constraint() {
        let connection = legacy_lifecycle_index_fixture();
        connection
            .execute_batch("CREATE TABLE one_finish_effect_per_attempt_phase (value TEXT);")
            .unwrap();
        assert!(replace_lifecycle_effect_indexes(&connection).is_err());
        let legacy_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'one_lifecycle_effect_per_attempt_phase')",
            [],
            |row| row.get(0),
        ).unwrap();
        assert!(legacy_exists);
        assert!(connection.execute(
            "INSERT INTO agent_run_operations VALUES ('run', 'attempt', 0, '\"cancel_agent\"')",
            [],
        ).is_err());
    }

    fn repository() -> ResolvedRepository {
        ResolvedRepository {
            identity: RepositoryIdentity::new("forge.example:3001", "Team", "Lookback").unwrap(),
            forge: ForgeKind::Gitea,
            api_base_url: "https://api.example:9443/v1".into(),
            clone_url: "https://clone.example:8443/git/Team/Lookback".into(),
            read_credential_ref: Some(CredentialReference::new("keychain:read").unwrap()),
            publication_credential_ref: Some(CredentialReference::new("keychain:publish").unwrap()),
            provenance: RepositoryProvenance {
                source_labels: vec!["repo:FORGE.example:3001/Team/Lookback".into()],
                selection: RepositorySelection::SourceLabel,
                approved_by: None,
                approved_at_ms: None,
            },
        }
    }

    fn profile() -> AgentProfileSnapshot {
        AgentProfileSnapshot {
            profile_id: "opencode-default".into(),
            version: 2,
            display_name: "OpenCode".into(),
            image_ref: format!("registry.example/agents/opencode@sha256:{}", "a".repeat(64)),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            command: vec!["opencode".into(), "run".into()],
            container_home: "/home/codespace".into(),
            managed_home_overlay: None,
            runtime_environment: Default::default(),
            network_access: Default::default(),
            resources: SandboxResources {
                cpu_millis: 1000,
                memory_mib: 2048,
                root_disk_mib: 10240,
                max_parallel_runs: 1,
                use_static: false,
            },
            validated_host_limits: HostResourceLimits {
                cpu_millis: 8000,
                memory_mib: 16384,
                root_disk_mib: 102400,
                max_parallel_runs: 4,
            },
            canonical_mount_allowed_roots: Vec::new(),
            persistent_mounts: Vec::new(),
            allowed_repositories: vec![repository().identity],
            credential_reference: Some(CredentialReference::new("keychain:agent-home").unwrap()),
        }
    }

    fn snapshot(run_id: &str, purpose: RunPurpose, auto_done: bool) -> AgentRunSnapshot {
        AgentRunSnapshot {
            run_id: RunId::new(run_id).unwrap(),
            issue_id: "local-issue-72".into(),
            issue_revision: 4,
            execution_mode: ExecutionMode::OneShot,
            purpose,
            repository: repository(),
            profile: profile(),
            base_commit: "b".repeat(40),
            workspace_branch: "coding-agent/issue-72".into(),
            semantic_judge_enabled: true,
            auto_done,
            approval: None,
            allowed_operations: vec![AllowedOperation::Verify],
            publication_plan: PublicationPlan::NoPublication,
            workflow: WorkflowDefinitionSnapshot {
                definition_hash: "c".repeat(64),
                semantic_judge_worker_digest: None,
                template_id: Some("coding-agent/one-shot".into()),
                materialized_definition_hash: Some("d".repeat(64)),
                registered_worker_id: None,
                workflow_execution_id: None,
                parent_job_id: None,
                child_sandbox_job_id: None,
            },
            attempt_ids: vec!["attempt-1".into()],
        }
    }

    fn create_run(root: &std::path::Path, run_id: &str) -> (RunStore, RunId) {
        let paths = DataPaths::with_root(root);
        let store = RunStore::open(&paths).unwrap();
        let snapshot = snapshot(run_id, RunPurpose::Implementation, true);
        let created = store.create_run(snapshot).unwrap();
        (store, created.snapshot.run_id)
    }

    fn transition_to(store: &RunStore, run_id: &RunId, target: RunState) -> AgentRunRecord {
        let revision = store.get_run(run_id).unwrap().unwrap().revision;
        store.transition(run_id, revision, target).unwrap()
    }

    fn prepare_first_attempt(store: &RunStore, run_id: &RunId) {
        transition_to(store, run_id, RunState::Queued);
        let preparing = transition_to(store, run_id, RunState::Preparing);
        let started = store
            .start_attempt(
                run_id,
                preparing.revision,
                "attempt-1",
                "dispatch:attempt-1",
            )
            .unwrap();
        assert!(started.newly_created);
        let completed = store
            .complete_operation(
                run_id,
                "dispatch:attempt-1",
                DurableOperationResult {
                    outcome: DurableOperationOutcome::Succeeded,
                    result_digest: Some("a".repeat(64)),
                    evidence_ids: Vec::new(),
                },
            )
            .unwrap();
        let replay = store
            .start_attempt(
                run_id,
                preparing.revision,
                "attempt-1",
                "dispatch:attempt-1",
            )
            .unwrap();
        assert!(!replay.newly_created);
        assert_eq!(replay.operation, completed);
    }

    fn mark_current_attempt_running(store: &RunStore, run_id: &RunId) -> AgentRunRecord {
        let run = store.get_run(run_id).unwrap().unwrap();
        let attempt_id = run.current_attempt_id.unwrap();
        store
            .mark_attempt_running(run_id, run.revision, &attempt_id)
            .unwrap()
    }

    fn semantic_interactive_contract() -> ApprovalAssessmentContract {
        ApprovalAssessmentContract::from_issue_acceptance(
            RunPurpose::Implementation,
            vec!["The implementation satisfies the approved change".into()],
            vec![ApprovalCriterionPolicy {
                required: true,
                method: CriterionEvaluationMethod::SemanticJudge,
            }],
            Vec::new(),
            Some(ProviderSelection {
                provider_id: "test-provider".into(),
                model_id: "test-model".into(),
                model_version: "v1".into(),
                config_version: "config-1".into(),
            }),
            Some("coding-agent-judge-v1".into()),
            AssessmentInputScope {
                allowed_fields: vec![ApprovedJudgeInputField::Report],
                allowed_sources: vec![AllowedEvidenceSource {
                    kind: EvidenceSourceKind::Report,
                    identity: "canonical-report".into(),
                    max_bytes: super::super::report::MAX_REPORT_BYTES,
                }],
                max_input_bytes: 128 * 1024,
            },
        )
        .unwrap()
    }

    fn create_interactive_semantic_run(
        root: &std::path::Path,
        run_id: &str,
    ) -> (RunStore, RunId, ApprovalAssessmentContract) {
        let store = RunStore::open(&DataPaths::with_root(root)).unwrap();
        let contract = semantic_interactive_contract();
        let mut approved = snapshot(run_id, RunPurpose::Implementation, false);
        approved.execution_mode = ExecutionMode::Interactive;
        approved.approval = Some(RunApproval {
            approved_by: "test-user".into(),
            approved_at_ms: 1,
            auto_done_approved: false,
            settings_version: None,
            applicable_modes: vec![ExecutionMode::Interactive],
        });
        let created = store
            .create_run_with_approval_contract(approved, contract.clone())
            .unwrap();
        prepare_first_attempt(&store, &created.snapshot.run_id);
        mark_current_attempt_running(&store, &created.snapshot.run_id);
        store
            .request_interactive_finish(&created.snapshot.run_id, "attempt-1")
            .unwrap();
        (store, created.snapshot.run_id, contract)
    }

    fn approved_one_shot_snapshot(run_id: &str) -> AgentRunSnapshot {
        let mut approved = snapshot(run_id, RunPurpose::Implementation, false);
        approved.approval = Some(RunApproval {
            approved_by: "test-user".into(),
            approved_at_ms: 42,
            auto_done_approved: false,
            settings_version: Some(3),
            applicable_modes: vec![ExecutionMode::OneShot],
        });
        approved.workflow.registered_worker_id = Some("7".into());
        approved.workflow.workflow_execution_id = Some("workflow-1".into());
        approved.workflow.parent_job_id = Some("parent-job-1".into());
        approved.workflow.child_sandbox_job_id = Some("child-job-1".into());
        approved
    }

    fn create_one_shot_finalizing_run(
        root: &std::path::Path,
        run_id: &str,
    ) -> (
        RunStore,
        RunId,
        ApprovalAssessmentContract,
        ExecutionIdentity,
    ) {
        create_one_shot_finalizing_run_with_method(root, run_id, "run")
    }

    fn create_one_shot_finalizing_run_with_method(
        root: &std::path::Path,
        run_id: &str,
        method_name: &str,
    ) -> (
        RunStore,
        RunId,
        ApprovalAssessmentContract,
        ExecutionIdentity,
    ) {
        create_one_shot_finalizing_run_with_intent_image(root, run_id, method_name, None)
    }

    fn create_one_shot_finalizing_run_with_intent_image(
        root: &std::path::Path,
        run_id: &str,
        method_name: &str,
        intent_image_digest: Option<&str>,
    ) -> (
        RunStore,
        RunId,
        ApprovalAssessmentContract,
        ExecutionIdentity,
    ) {
        create_one_shot_finalizing_run_with_intent_metadata(
            root,
            run_id,
            method_name,
            intent_image_digest,
            None,
        )
    }

    fn create_one_shot_finalizing_run_with_intent_metadata(
        root: &std::path::Path,
        run_id: &str,
        method_name: &str,
        intent_image_digest: Option<&str>,
        intent_worker_id: Option<i64>,
    ) -> (
        RunStore,
        RunId,
        ApprovalAssessmentContract,
        ExecutionIdentity,
    ) {
        let store = RunStore::open(&DataPaths::with_root(root)).unwrap();
        let contract = semantic_interactive_contract();
        let created = store
            .create_run_with_approval_contract(approved_one_shot_snapshot(run_id), contract.clone())
            .unwrap();
        let run_id = created.snapshot.run_id;
        transition_to(&store, &run_id, RunState::Queued);
        let preparing = transition_to(&store, &run_id, RunState::Preparing);
        let started = store
            .start_attempt(
                &run_id,
                preparing.revision,
                "attempt-1",
                "dispatch:attempt-1",
            )
            .unwrap();
        assert!(started.newly_created);
        let identity = ExecutionIdentity {
            run_id: run_id.as_str().into(),
            attempt_id: "attempt-1".into(),
            workflow_execution_id: "workflow-1".into(),
            parent_job_id: "parent-job-1".into(),
            child_sandbox_job_id: "child-job-1".into(),
            worker_id: 7,
            sandbox_job_id: "sandbox-job-1".into(),
        };
        let intent = super::super::execution::ImmutableExecutionIntent {
            run_id: run_id.as_str().into(),
            attempt_id: "attempt-1".into(),
            issue_id: created.snapshot.issue_id.clone(),
            issue_revision: created.snapshot.issue_revision,
            execution_mode: created.snapshot.execution_mode,
            purpose: created.snapshot.purpose,
            repository_host: created.snapshot.repository.identity.host.clone(),
            repository_owner: created.snapshot.repository.identity.organization.clone(),
            repository_name: created.snapshot.repository.identity.repository.clone(),
            base_commit: created.snapshot.base_commit.clone(),
            branch: created.snapshot.workspace_branch.clone(),
            profile_id: created.snapshot.profile.profile_id.clone(),
            profile_version: created.snapshot.profile.version,
            image_digest: intent_image_digest
                .unwrap_or(&created.snapshot.profile.image_digest)
                .to_owned(),
            command: created.snapshot.profile.command.clone(),
            workflow_definition_hash: created.snapshot.workflow.definition_hash.clone(),
            workflow_execution_id: identity.workflow_execution_id.clone(),
            parent_job_id: identity.parent_job_id.clone(),
            child_sandbox_job_id: identity.child_sandbox_job_id.clone(),
            registered_worker_id: created
                .snapshot
                .workflow
                .registered_worker_id
                .clone()
                .unwrap(),
            worker_id: intent_worker_id.unwrap_or(identity.worker_id),
            method_name: method_name.into(),
            prompt_sha256: None,
        };
        let intent_bytes = serde_json::to_vec(&intent).unwrap();
        let intent_reference = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "dispatch-intent",
                "application/json",
                &intent_bytes,
            )
            .unwrap();
        let receipt = StartReceipt {
            identity: identity.clone(),
            execution_intent_sha256: format!("{:x}", Sha256::digest(&intent_bytes)),
            method_name: method_name.into(),
            image_digest: created.snapshot.profile.image_digest.clone(),
            prompt_delivery: super::super::execution::PromptDelivery::Accepted,
        };
        let receipt_bytes = serde_json::to_vec(&receipt).unwrap();
        let receipt_reference = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "dispatch-receipt",
                "application/json",
                &receipt_bytes,
            )
            .unwrap();
        store
            .complete_operation(
                &run_id,
                "dispatch:attempt-1",
                DurableOperationResult {
                    outcome: DurableOperationOutcome::Succeeded,
                    result_digest: Some(format!("{:x}", Sha256::digest(&receipt_bytes))),
                    evidence_ids: vec![intent_reference.evidence_id, receipt_reference.evidence_id],
                },
            )
            .unwrap();
        let running = store
            .mark_attempt_running(&run_id, started.run_revision, "attempt-1")
            .unwrap();
        store
            .transition(&run_id, running.revision, RunState::Finalizing)
            .unwrap();
        (store, run_id, contract, identity)
    }

    fn valid_host_finalization_inputs(
        store: &RunStore,
        run_id: &RunId,
        report_reference: EvidenceReference,
        stdout_reference: Option<EvidenceReference>,
        stderr_reference: Option<EvidenceReference>,
    ) -> HostVerifiedFinalizationInputs {
        use super::super::execution::{
            ExecutionIdentity, SealedArtifactBinding, TypedFinalizationEvidence,
            VerifiedFinalization,
        };
        use super::super::validation::ValidationTargetSnapshot;
        use coding_agent_executor::{CaptureGuarantee, RepositoryId, ResolvedArtifact};

        let run = store.get_run(run_id).unwrap().unwrap();
        let commit = "e".repeat(40);
        let tree = "f".repeat(40);
        let manifest = "1".repeat(64);
        let target =
            ValidationTargetSnapshot::new(commit.clone(), tree.clone(), manifest.clone()).unwrap();
        let target_digest = target.digest().to_owned();
        let artifact_digest = "2".repeat(64);
        let artifact_manifest_digest = format!("{:x}", Sha256::digest(b"{}"));
        let repository = RepositoryId {
            host: "forge.example:3001".into(),
            owner: "Team".into(),
            name: "Lookback".into(),
        };
        let identity = ExecutionIdentity {
            run_id: run_id.as_str().into(),
            attempt_id: "attempt-1".into(),
            workflow_execution_id: "workflow-1".into(),
            parent_job_id: "parent-job-1".into(),
            child_sandbox_job_id: "child-job-1".into(),
            worker_id: 7,
            sandbox_job_id: "sandbox-job-1".into(),
        };
        let typed = TypedFinalizationEvidence {
            identity: identity.clone(),
            invocation_kind: ExecutionInvocationKind::DirectProcess,
            cli_exit: ProcessExitObservation::ExitedNormally { exit_code: 0 },
            shell_observation: ShellProcessObservation::NotSpawned,
            child_writers: ChildWritersStatus::Stopped,
            workspace_freeze: WorkspaceFreezeStatus::HostVerified {
                commit_id: commit.clone(),
            },
            sealed_artifact: Some(SealedArtifactBinding {
                full_commit_id: commit.clone(),
                full_tree_id: tree.clone(),
                untracked_manifest_digest: manifest.clone(),
                target_snapshot_digest: target_digest.clone(),
            }),
            validation_receipts: Vec::new(),
        };
        let verified = VerifiedFinalization {
            artifact: ResolvedArtifact {
                run_id: run_id.as_str().into(),
                attempt_id: "attempt-1".into(),
                repository,
                branch: run.snapshot.workspace_branch,
                base_branch: "main".into(),
                base_commit: run.snapshot.base_commit,
                approved_snapshot_digest: "3".repeat(64),
                scope_digest: "4".repeat(64),
                intent_approval_digest: "5".repeat(64),
                manifest_digest: artifact_manifest_digest,
                finalized_bytes_digest: "6".repeat(64),
                tree: tree.clone(),
                commit: commit.clone(),
                artifact_digest: artifact_digest.clone(),
                protocol_version: 2,
                executor_version: "test".into(),
                capture_guarantee: CaptureGuarantee::HostIsolatedAfterSeal,
            },
            target,
            validation_runs: Vec::new(),
            validation_receipts: Vec::new(),
        };
        HostVerifiedFinalizationInputs::from_verified_finalization(
            &typed,
            &verified,
            FinalizationCaptureInputs::new(
                report_reference,
                stdout_reference,
                stderr_reference,
                true,
                false,
                false,
            ),
        )
        .unwrap()
    }

    fn assessment_report(run_id: &RunId) -> Vec<u8> {
        serde_json::to_vec(&super::super::report::AgentReport {
            schema_version: 1,
            run_id: run_id.as_str().into(),
            attempt_id: "attempt-1".into(),
            task_kind: TaskKind::Implementation,
            status: super::super::report::AgentReportStatus::Completed,
            summary: "Implemented the approved change".into(),
            acceptance: vec![super::super::report::CriterionAssessment {
                criterion_id: "AC-1".into(),
                assessment: super::super::report::AcceptanceAssessment::Met,
                evidence: vec!["canonical-report".into()],
            }],
            validation: Vec::new(),
            investigation: None,
            implementation: Some(super::super::report::ImplementationReport {
                change_summary: "Implemented the approved change".into(),
                changed_files: vec!["src/lib.rs".into()],
                implementation_notes: Vec::new(),
            }),
            unresolved: Vec::new(),
        })
        .unwrap()
    }

    fn assessment_payload_and_request(
        store: &RunStore,
        run_id: &RunId,
        contract: &ApprovalAssessmentContract,
        report_reference: &EvidenceReference,
        manifest_reference: &EvidenceReference,
    ) -> (Vec<u8>, AssessmentIntentRequest) {
        let snapshot_json = store.snapshot_json(run_id).unwrap();
        let report_bytes = store
            .read_evidence(run_id, &report_reference.evidence_id)
            .unwrap();
        let target = super::super::validation::ValidationTargetSnapshot::new(
            "e".repeat(40),
            "f".repeat(40),
            "1".repeat(64),
        )
        .unwrap();
        let input_bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "assessment_id": "assessment-1",
            "run_id": run_id.as_str(),
            "attempt_id": "attempt-1",
            "task_kind": "implementation",
            "approval_snapshot_digest": format!("{:x}", Sha256::digest(snapshot_json.as_bytes())),
            "frozen_artifact_digest": "2".repeat(64),
            "report_digest": report_reference.sha256,
            "validation_receipts": [],
            "validation_plan": [],
            "criteria": [{
                "criterion_id": "AC-1",
                "statement": contract.criteria[0].statement,
                "required": true,
                "method": "semantic_judge",
            }],
            "evidence": [{
                "kind": "report",
                "identity": "canonical-report",
                "content": String::from_utf8(report_bytes).unwrap(),
            }],
            "provider": contract.provider,
            "prompt_policy_version": contract.prompt_policy_version,
            "judge_input_scope_digest": contract.judge_input_scope_digest(),
        }))
        .unwrap();
        let request = AssessmentIntentRequest {
            assessment_id: "assessment-1".into(),
            judge_input_evidence_id: String::new(),
            report_evidence_id: report_reference.evidence_id.clone(),
            snapshot_manifest_evidence_id: manifest_reference.evidence_id.clone(),
            target_full_commit_id: target.full_commit_id().into(),
            target_full_tree_id: target.full_tree_id().into(),
            target_untracked_manifest_digest: target.untracked_manifest_digest().into(),
            target_snapshot_digest: target.digest().into(),
            system_prompt_digest: "7".repeat(64),
            validation_receipts: Vec::new(),
        };
        (input_bytes, request)
    }

    fn setup_linked_finalization_intent(
        store: &RunStore,
        run_id: &RunId,
        contract: &ApprovalAssessmentContract,
    ) -> (
        EvidenceReference,
        EvidenceReference,
        EvidenceReference,
        AssessmentIntentBinding,
    ) {
        let report_reference = store
            .write_evidence(
                run_id,
                "attempt-1",
                "agent-report.json",
                "application/json",
                &assessment_report(run_id),
            )
            .unwrap();
        let manifest_reference = store
            .write_evidence(
                run_id,
                "attempt-1",
                "snapshot-manifest.json",
                "application/json",
                b"{}",
            )
            .unwrap();
        let revision = store.get_run(run_id).unwrap().unwrap().revision;
        let finalization_reference = store
            .write_verified_finalization_inputs(
                run_id,
                revision,
                "attempt-1",
                valid_host_finalization_inputs(store, run_id, report_reference.clone(), None, None),
            )
            .unwrap();
        let (input_bytes, mut request) = assessment_payload_and_request(
            store,
            run_id,
            contract,
            &report_reference,
            &manifest_reference,
        );
        request.judge_input_evidence_id = store
            .write_evidence(
                run_id,
                "attempt-1",
                "judge-input.json",
                "application/json",
                &input_bytes,
            )
            .unwrap()
            .evidence_id;
        let claim = store
            .bind_assessment_intent_from_finalization_inputs(
                run_id,
                store.get_run(run_id).unwrap().unwrap().revision,
                "attempt-1",
                request,
            )
            .unwrap();
        assert!(claim.newly_created);
        (
            report_reference,
            manifest_reference,
            finalization_reference,
            claim.binding,
        )
    }

    #[test]
    fn approved_one_shot_claim_uses_persisted_start_identity_without_interactive_finish() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let (store, run_id, contract, _) =
            create_one_shot_finalizing_run(temp.path(), "run-c2-one-shot-finalization");
        let report_reference = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "agent-report.json",
                "application/json",
                &assessment_report(&run_id),
            )
            .unwrap();
        let manifest_reference = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "snapshot-manifest.json",
                "application/json",
                b"{}",
            )
            .unwrap();
        let before = store.get_run(&run_id).unwrap().unwrap();
        assert_eq!(before.snapshot.execution_mode, ExecutionMode::OneShot);
        assert_eq!(before.state, RunState::Finalizing);
        let inputs =
            valid_host_finalization_inputs(&store, &run_id, report_reference.clone(), None, None);

        let finalization_reference = store
            .write_verified_finalization_inputs(
                &run_id,
                before.revision,
                "attempt-1",
                inputs.clone(),
            )
            .unwrap();
        assert_eq!(
            store.get_run(&run_id).unwrap().unwrap().revision,
            before.revision + 1,
            "the one-shot input index advances the CAS revision exactly once"
        );
        let saved = store
            .read_saved_verified_finalization_inputs(&run_id, "attempt-1")
            .unwrap();
        assert_eq!(saved.index_reference(), &finalization_reference);

        let (input_bytes, mut request) = assessment_payload_and_request(
            &store,
            &run_id,
            &contract,
            &report_reference,
            &manifest_reference,
        );
        request.judge_input_evidence_id = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "judge-input.json",
                "application/json",
                &input_bytes,
            )
            .unwrap()
            .evidence_id;
        let revision = store.get_run(&run_id).unwrap().unwrap().revision;
        let claim = store
            .bind_assessment_intent_from_finalization_inputs(
                &run_id,
                revision,
                "attempt-1",
                request.clone(),
            )
            .unwrap();
        assert!(claim.newly_created);
        let retry = store
            .bind_assessment_intent_from_finalization_inputs(
                &run_id,
                revision,
                "attempt-1",
                request,
            )
            .unwrap();
        assert!(!retry.newly_created);
        assert_eq!(retry.binding, claim.binding);
        assert_eq!(store.get_run(&run_id).unwrap().unwrap().revision, revision);

        let connection = store.lock_connection().unwrap();
        let final_row_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM agent_run_execution_evidence
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), "attempt-1"],
                |row| row.get(0),
            )
            .unwrap();
        let finish_request_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM agent_run_interactive_finish_requests
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), "attempt-1"],
                |row| row.get(0),
            )
            .unwrap();
        let finish_effect_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM agent_run_operations
                 WHERE run_id = ?1 AND attempt_id = ?2 AND kind_json = ?3",
                params![
                    run_id.as_str(),
                    "attempt-1",
                    serde_json::to_string(&DurableOperationKind::FinishInteractive).unwrap(),
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            final_row_count, 0,
            "the judge claim precedes final evidence"
        );
        assert_eq!(finish_request_count, 0);
        assert_eq!(finish_effect_count, 0);
        drop(connection);

        let reopened = RunStore::open(&paths).unwrap();
        assert_eq!(
            reopened
                .read_saved_verified_finalization_inputs(&run_id, "attempt-1")
                .unwrap(),
            saved
        );
        assert_eq!(
            reopened.assessment_intent(&run_id, "attempt-1").unwrap(),
            claim.binding
        );
    }

    #[test]
    fn one_shot_finalization_rejects_unapproved_mode_and_unmatched_start_identity() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let contract = semantic_interactive_contract();
        let mut wrong_mode = approved_one_shot_snapshot("run-c2-one-shot-wrong-mode");
        wrong_mode.approval.as_mut().unwrap().applicable_modes = vec![ExecutionMode::Interactive];
        let mode_store = RunStore::open(&paths).unwrap();
        assert!(matches!(
            mode_store.create_run_with_approval_contract(wrong_mode, contract.clone()),
            Err(CodingAgentError::Model(RunModelError::Invalid(_)))
        ));

        for (suffix, mismatch) in [
            ("missing-workflow", 0_u8),
            ("wrong-parent", 1_u8),
            ("wrong-child", 2_u8),
            ("wrong-worker", 3_u8),
            ("wrong-sandbox", 4_u8),
            ("missing-receipt", 5_u8),
            ("wrong-method", 6_u8),
            ("missing-start-claim", 7_u8),
            ("missing-intent", 8_u8),
            ("wrong-intent-config", 9_u8),
            ("wrong-intent-worker", 10_u8),
        ] {
            let root = temp.path().join(format!("case-{suffix}"));
            let method_name = if mismatch == 6 {
                "run_with_client"
            } else {
                "run"
            };
            let run_name = format!("run-c2-one-shot-{suffix}");
            let (store, run_id, _, _) = match mismatch {
                9 => create_one_shot_finalizing_run_with_intent_image(
                    &root,
                    &run_name,
                    method_name,
                    Some(&"3".repeat(64)),
                ),
                10 => create_one_shot_finalizing_run_with_intent_metadata(
                    &root,
                    &run_name,
                    method_name,
                    None,
                    Some(8),
                ),
                _ => create_one_shot_finalizing_run_with_method(&root, &run_name, method_name),
            };
            let report_reference = store
                .write_evidence(
                    &run_id,
                    "attempt-1",
                    "agent-report.json",
                    "application/json",
                    &assessment_report(&run_id),
                )
                .unwrap();
            let revision = store.get_run(&run_id).unwrap().unwrap().revision;
            let mut inputs =
                valid_host_finalization_inputs(&store, &run_id, report_reference, None, None);
            match mismatch {
                0 => inputs.identity.workflow_execution_id.clear(),
                1 => inputs.identity.parent_job_id = "other-parent".into(),
                2 => inputs.identity.child_sandbox_job_id = "other-child".into(),
                3 => inputs.identity.worker_id += 1,
                4 => inputs.identity.sandbox_job_id = "other-sandbox".into(),
                5 => {
                    let receipt_id = store
                        .evidence(&run_id)
                        .unwrap()
                        .into_iter()
                        .find(|reference| reference.name == "dispatch-receipt")
                        .unwrap()
                        .evidence_id;
                    store
                        .lock_connection()
                        .unwrap()
                        .execute(
                            "DELETE FROM agent_run_evidence
                             WHERE run_id = ?1 AND evidence_id = ?2",
                            params![run_id.as_str(), receipt_id],
                        )
                        .unwrap();
                }
                6 => {}
                7 => {
                    store
                        .lock_connection()
                        .unwrap()
                        .execute(
                            "DELETE FROM agent_run_start_claims
                             WHERE run_id = ?1 AND attempt_id = ?2",
                            params![run_id.as_str(), "attempt-1"],
                        )
                        .unwrap();
                }
                8 => {
                    let intent_id = store
                        .evidence(&run_id)
                        .unwrap()
                        .into_iter()
                        .find(|reference| reference.name == "dispatch-intent")
                        .unwrap()
                        .evidence_id;
                    store
                        .lock_connection()
                        .unwrap()
                        .execute(
                            "DELETE FROM agent_run_evidence
                             WHERE run_id = ?1 AND evidence_id = ?2",
                            params![run_id.as_str(), intent_id],
                        )
                        .unwrap();
                }
                9 | 10 => {}
                _ => unreachable!(),
            }
            assert!(matches!(
                store.write_verified_finalization_inputs(&run_id, revision, "attempt-1", inputs,),
                Err(CodingAgentError::Conflict(_) | CodingAgentError::Corrupt(_))
            ));
            let index_count: i64 = store
                .lock_connection()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM agent_run_finalization_inputs
                     WHERE run_id = ?1 AND attempt_id = ?2",
                    params![run_id.as_str(), "attempt-1"],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(index_count, 0, "unmatched start identity cannot be indexed");
            assert!(matches!(
                store.assessment_intent(&run_id, "attempt-1"),
                Err(CodingAgentError::NotFound(_))
            ));
        }
    }

    #[test]
    fn one_shot_host_failures_remain_exact_and_block_pre_judge_claims() {
        for (suffix, failure) in [
            ("unknown-exit", 0_u8),
            ("active-writers", 1_u8),
            ("unknown-freeze", 2_u8),
            ("nonzero-exit", 3_u8),
            ("unknown-invocation", 4_u8),
            ("shell-not-stopped", 5_u8),
            ("fake-freeze", 6_u8),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (store, run_id, contract, _) = create_one_shot_finalizing_run(
                temp.path(),
                &format!("run-c2-one-shot-failure-{suffix}"),
            );
            let report_reference = store
                .write_evidence(
                    &run_id,
                    "attempt-1",
                    "agent-report.json",
                    "application/json",
                    &assessment_report(&run_id),
                )
                .unwrap();
            let manifest_reference = store
                .write_evidence(
                    &run_id,
                    "attempt-1",
                    "snapshot-manifest.json",
                    "application/json",
                    b"{}",
                )
                .unwrap();
            let revision = store.get_run(&run_id).unwrap().unwrap().revision;
            let mut inputs = valid_host_finalization_inputs(
                &store,
                &run_id,
                report_reference.clone(),
                None,
                None,
            );
            match failure {
                0 => inputs.cli_exit = ProcessExitObservation::Unknown,
                1 => inputs.child_writers = ChildWritersStatus::Active,
                2 => inputs.workspace_freeze = WorkspaceFreezeStatus::Unknown,
                3 => inputs.cli_exit = ProcessExitObservation::ExitedNormally { exit_code: 7 },
                4 => inputs.invocation_kind = ExecutionInvocationKind::Unknown,
                5 => {
                    inputs.invocation_kind = ExecutionInvocationKind::ShellWrapper;
                    inputs.shell_observation = ShellProcessObservation::NotSpawned;
                }
                6 => {
                    inputs.workspace_freeze = WorkspaceFreezeStatus::HostVerified {
                        commit_id: "a".repeat(40),
                    };
                }
                _ => unreachable!(),
            }
            let write_result =
                store.write_verified_finalization_inputs(&run_id, revision, "attempt-1", inputs);
            if failure == 6 {
                assert!(matches!(write_result, Err(CodingAgentError::Conflict(_))));
                let index_count: i64 = store
                    .lock_connection()
                    .unwrap()
                    .query_row(
                        "SELECT COUNT(*) FROM agent_run_finalization_inputs
                         WHERE run_id = ?1 AND attempt_id = ?2",
                        params![run_id.as_str(), "attempt-1"],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(index_count, 0, "a mismatched frozen commit is not indexed");
                continue;
            }
            write_result.unwrap();
            let saved = store
                .read_saved_verified_finalization_inputs(&run_id, "attempt-1")
                .unwrap();
            match failure {
                0 => assert_eq!(saved.cli_exit, ProcessExitObservation::Unknown),
                1 => assert_eq!(saved.child_writers, ChildWritersStatus::Active),
                2 => assert_eq!(saved.workspace_freeze, WorkspaceFreezeStatus::Unknown),
                3 => assert_eq!(
                    saved.cli_exit,
                    ProcessExitObservation::ExitedNormally { exit_code: 7 }
                ),
                4 => assert_eq!(saved.invocation_kind, ExecutionInvocationKind::Unknown),
                5 => assert_eq!(saved.invocation_kind, ExecutionInvocationKind::ShellWrapper),
                _ => unreachable!(),
            }

            let (input_bytes, mut request) = assessment_payload_and_request(
                &store,
                &run_id,
                &contract,
                &report_reference,
                &manifest_reference,
            );
            request.judge_input_evidence_id = store
                .write_evidence(
                    &run_id,
                    "attempt-1",
                    "judge-input.json",
                    "application/json",
                    &input_bytes,
                )
                .unwrap()
                .evidence_id;
            assert!(matches!(
                store.bind_assessment_intent_from_finalization_inputs(
                    &run_id,
                    store.get_run(&run_id).unwrap().unwrap().revision,
                    "attempt-1",
                    request,
                ),
                Err(CodingAgentError::Conflict(_))
            ));
            assert!(matches!(
                store.assessment_intent(&run_id, "attempt-1"),
                Err(CodingAgentError::NotFound(_))
            ));
        }
    }

    #[test]
    fn interactive_finalizing_still_requires_its_existing_finish_request() {
        let temp = tempfile::tempdir().unwrap();
        let store = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
        let contract = semantic_interactive_contract();
        let mut approved = snapshot(
            "run-c2-interactive-finish-still-required",
            RunPurpose::Implementation,
            false,
        );
        approved.execution_mode = ExecutionMode::Interactive;
        approved.approval = Some(RunApproval {
            approved_by: "test-user".into(),
            approved_at_ms: 42,
            auto_done_approved: false,
            settings_version: Some(3),
            applicable_modes: vec![ExecutionMode::Interactive],
        });
        let run_id = store
            .create_run_with_approval_contract(approved, contract)
            .unwrap()
            .snapshot
            .run_id;
        prepare_first_attempt(&store, &run_id);
        mark_current_attempt_running(&store, &run_id);
        let run = store.get_run(&run_id).unwrap().unwrap();
        assert!(matches!(
            store.transition(&run_id, run.revision, RunState::Finalizing),
            Err(CodingAgentError::Conflict(_))
        ));
        let connection = store.lock_connection().unwrap();
        let finish_request_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM agent_run_interactive_finish_requests
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), "attempt-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(finish_request_count, 0);
    }

    fn assert_attempt_scoped_finalization_foreign_keys(
        connection: &Connection,
        run_id: &RunId,
        attempt_one_evidence_id: &str,
    ) {
        let foreign_keys_enabled: i64 = connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys_enabled, 1);
        connection
            .execute(
                "INSERT INTO agent_run_attempts
                    (run_id, attempt_id, ordinal, created_at_ms)
                 VALUES (?1, 'attempt-2', 1, ?2)",
                params![
                    run_id.as_str(),
                    sql_i64(now_ms(), "test attempt timestamp").unwrap()
                ],
            )
            .unwrap();
        let attempt_two_evidence_id = "evidence-attempt-two";
        connection
            .execute(
                "INSERT INTO agent_run_evidence
                    (run_id, evidence_id, attempt_id, name, media_type, sha256,
                     size_bytes, save_confirmed, created_at_ms)
                 VALUES (?1, ?2, 'attempt-2', 'attempt-two-output', 'text/plain', ?3,
                         0, 1, ?4)",
                params![
                    run_id.as_str(),
                    attempt_two_evidence_id,
                    format!("{:x}", Sha256::digest(b"")),
                    sql_i64(now_ms(), "test evidence timestamp").unwrap(),
                ],
            )
            .unwrap();

        let cross_attempt_index = connection.execute(
            "INSERT INTO agent_run_finalization_inputs (run_id, attempt_id, evidence_id)
             VALUES (?1, 'attempt-2', ?2)",
            params![run_id.as_str(), attempt_one_evidence_id],
        );
        assert!(
            cross_attempt_index.is_err(),
            "a finalization index must not refer to another attempt's evidence"
        );
        connection
            .execute(
                "INSERT INTO agent_run_finalization_inputs (run_id, attempt_id, evidence_id)
                 VALUES (?1, 'attempt-2', ?2)",
                params![run_id.as_str(), attempt_two_evidence_id],
            )
            .unwrap();

        let cross_attempt_intent = connection.execute(
            "INSERT INTO agent_run_assessment_intents
                (run_id, attempt_id, assessment_id, record_json, created_at_ms,
                 finalization_evidence_id)
             VALUES (?1, 'attempt-1', 'assessment-cross-attempt', '{}', 0, ?2)",
            params![run_id.as_str(), attempt_two_evidence_id],
        );
        assert!(
            cross_attempt_intent.is_err(),
            "an intent must link to the finalization index row for its own attempt"
        );
    }

    #[test]
    fn finalization_input_write_rechecks_final_row_after_blob_durability() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let (store, run_id, _) =
            create_interactive_semantic_run(temp.path(), "run-c2-finalization-write-race");
        let racing_store = RunStore::open(&paths).unwrap();
        let report_reference = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "agent-report.json",
                "application/json",
                &assessment_report(&run_id),
            )
            .unwrap();
        let before = store.get_run(&run_id).unwrap().unwrap();
        let inputs = valid_host_finalization_inputs(&store, &run_id, report_reference, None, None);
        let mut final_evidence = successful_evidence();
        final_evidence.final_exit_code = Some(1);
        final_evidence.known_failure = true;
        final_evidence.acceptance_conditions_met = false;

        assert!(matches!(
            store.write_verified_finalization_inputs_with_before_index(
                &run_id,
                before.revision,
                "attempt-1",
                inputs,
                || racing_store.record_execution_evidence(&run_id, "attempt-1", final_evidence,),
            ),
            Err(CodingAgentError::Conflict(_))
        ));
        assert_eq!(
            store.get_run(&run_id).unwrap().unwrap().revision,
            before.revision
        );
        let connection = store.lock_connection().unwrap();
        let final_row_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM agent_run_execution_evidence
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), "attempt-1"],
                |row| row.get(0),
            )
            .unwrap();
        let index_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM agent_run_finalization_inputs
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), "attempt-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(final_row_count, 1);
        assert_eq!(
            index_count, 0,
            "a completed final row must block a new index"
        );
        drop(connection);
        assert!(matches!(
            store.assessment_intent(&run_id, "attempt-1"),
            Err(CodingAgentError::NotFound(_))
        ));
        assert!(matches!(
            store.read_saved_verified_finalization_inputs(&run_id, "attempt-1"),
            Err(CodingAgentError::NotFound(_))
        ));
    }

    #[test]
    fn finalization_and_intent_foreign_keys_bind_the_exact_attempt_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let (store, run_id, contract) =
            create_interactive_semantic_run(temp.path(), "run-c2-attempt-evidence-fk");
        let (_, _, finalization_reference, _) =
            setup_linked_finalization_intent(&store, &run_id, &contract);
        let connection = store.lock_connection().unwrap();
        assert_attempt_scoped_finalization_foreign_keys(
            &connection,
            &run_id,
            &finalization_reference.evidence_id,
        );
    }

    #[test]
    fn assessment_intent_loader_rejects_missing_link_and_target_drift() {
        let temp = tempfile::tempdir().unwrap();
        let (store, run_id, contract) =
            create_interactive_semantic_run(temp.path(), "run-c2-intent-link-corrupt");
        let (_, _, _, binding) = setup_linked_finalization_intent(&store, &run_id, &contract);

        let drifted_binding = {
            let mut drifted = binding.clone();
            drifted.target_full_commit_id = "a".repeat(40);
            serde_json::to_string(&drifted).unwrap()
        };
        store
            .lock_connection()
            .unwrap()
            .execute(
                "UPDATE agent_run_assessment_intents SET record_json = ?1
                 WHERE run_id = ?2 AND attempt_id = ?3",
                params![drifted_binding, run_id.as_str(), "attempt-1"],
            )
            .unwrap();
        assert!(matches!(
            store.assessment_intent(&run_id, "attempt-1"),
            Err(CodingAgentError::Corrupt(_))
        ));

        store
            .lock_connection()
            .unwrap()
            .execute(
                "UPDATE agent_run_assessment_intents SET record_json = ?1,
                     finalization_evidence_id = NULL
                 WHERE run_id = ?2 AND attempt_id = ?3",
                params![
                    serde_json::to_string(&binding).unwrap(),
                    run_id.as_str(),
                    "attempt-1",
                ],
            )
            .unwrap();
        assert!(matches!(
            store.assessment_intent(&run_id, "attempt-1"),
            Err(CodingAgentError::Corrupt(_))
        ));
        assert!(matches!(
            store.record_execution_evidence(&run_id, "attempt-1", successful_evidence()),
            Err(CodingAgentError::Corrupt(_))
        ));
        let final_row_count: i64 = store
            .lock_connection()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM agent_run_execution_evidence
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), "attempt-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(final_row_count, 0);
    }

    #[test]
    fn verified_finalization_inputs_are_saved_before_an_exact_pre_judge_claim() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let (store, run_id, contract) =
            create_interactive_semantic_run(temp.path(), "run-c2-finalization-inputs");
        let report_bytes = assessment_report(&run_id);
        let report_reference = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "agent-report.json",
                "application/json",
                &report_bytes,
            )
            .unwrap();
        let stdout_reference = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "stdout.bin",
                "application/octet-stream",
                b"final stdout",
            )
            .unwrap();
        let manifest_reference = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "snapshot-manifest.json",
                "application/json",
                b"{}",
            )
            .unwrap();
        let before_inputs = store.get_run(&run_id).unwrap().unwrap();
        let typed_inputs = valid_host_finalization_inputs(
            &store,
            &run_id,
            report_reference.clone(),
            Some(stdout_reference.clone()),
            None,
        );
        let exact_retry = typed_inputs.clone();

        let finalization_reference = store
            .write_verified_finalization_inputs(
                &run_id,
                before_inputs.revision,
                "attempt-1",
                typed_inputs,
            )
            .unwrap();
        assert_eq!(
            store
                .write_verified_finalization_inputs(
                    &run_id,
                    before_inputs.revision,
                    "attempt-1",
                    exact_retry,
                )
                .unwrap(),
            finalization_reference,
            "an exact retry returns the one immutable indexed evidence reference"
        );
        assert_eq!(
            store.get_run(&run_id).unwrap().unwrap().revision,
            before_inputs.revision + 1,
            "saving the immutable input index advances the run CAS revision exactly once"
        );
        assert_eq!(
            finalization_reference.name,
            VERIFIED_FINALIZATION_INPUTS_EVIDENCE_NAME
        );
        let saved = store
            .read_saved_verified_finalization_inputs(&run_id, "attempt-1")
            .unwrap();
        assert_eq!(
            saved.report_reference().evidence_id,
            report_reference.evidence_id
        );
        assert_eq!(
            saved
                .stdout_reference()
                .map(|reference| reference.evidence_id.as_str()),
            Some(stdout_reference.evidence_id.as_str())
        );

        let (input_bytes, mut request) = assessment_payload_and_request(
            &store,
            &run_id,
            &contract,
            &report_reference,
            &manifest_reference,
        );
        request.judge_input_evidence_id = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "judge-input.json",
                "application/json",
                &input_bytes,
            )
            .unwrap()
            .evidence_id;

        let final_assessment = RunEvidenceAssessment {
            final_exit_code: Some(0),
            end_present: true,
            stream_error: false,
            cancelled: false,
            known_failure: false,
            stdout_reference: Some(stdout_reference.evidence_id.clone()),
            stdout_bytes: stdout_reference.size_bytes,
            stderr_reference: None,
            stderr_bytes: 0,
            report_reference: Some(report_reference.evidence_id.clone()),
            report_save_confirmed: true,
            evidence_integrity_verified: true,
            commit_id: Some("e".repeat(40)),
            commit_consistent: true,
            acceptance_conditions_met: false,
            validation: ValidationEvidence {
                required: false,
                state: ValidationState::NotRun,
            },
        };
        assert!(matches!(
            store.record_execution_evidence(&run_id, "attempt-1", final_assessment.clone()),
            Err(CodingAgentError::Conflict(_))
        ));

        let final_row_before_claim: bool = store
            .lock_connection()
            .unwrap()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_run_execution_evidence
                 WHERE run_id = ?1 AND attempt_id = ?2)",
                params![run_id.as_str(), "attempt-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!final_row_before_claim);

        let revision = store.get_run(&run_id).unwrap().unwrap().revision;
        let claim = store
            .bind_assessment_intent_from_finalization_inputs(
                &run_id,
                revision,
                "attempt-1",
                request.clone(),
            )
            .unwrap();
        assert!(claim.newly_created);
        assert_eq!(claim.binding.expected_run_revision, revision);
        assert_eq!(
            claim.binding.report_evidence_id,
            report_reference.evidence_id
        );
        assert_eq!(
            claim.binding.report_digest, report_reference.sha256,
            "the pre-judge claim is bound to the validated canonical report bytes"
        );
        assert_eq!(
            store.assessment_intent(&run_id, "attempt-1").unwrap(),
            claim.binding,
            "the intent row reopens with its exact finalization-evidence index link"
        );

        store
            .record_execution_evidence(&run_id, "attempt-1", final_assessment)
            .unwrap();
        store
            .record_interactive_finalization(
                &run_id,
                "attempt-1",
                InteractiveFinalizationEvidence::from_host_observations_with_invocation(
                    2,
                    ExecutionInvocationKind::DirectProcess,
                    ProcessExitObservation::ExitedNormally { exit_code: 0 },
                    ShellProcessObservation::NotSpawned,
                    ChildWritersStatus::Stopped,
                    WorkspaceFreezeStatus::HostVerified {
                        commit_id: "e".repeat(40),
                    },
                ),
            )
            .unwrap();
        let verifying = store.transition(
            &run_id,
            store.get_run(&run_id).unwrap().unwrap().revision,
            RunState::Verifying,
        );
        assert!(
            verifying.is_ok(),
            "the unchanged Verifying guard sees final assessment evidence"
        );

        let recovered_store = RunStore::open(&paths).unwrap();
        assert_eq!(
            recovered_store
                .read_saved_verified_finalization_inputs(&run_id, "attempt-1")
                .unwrap(),
            saved,
            "the saved host inputs are durable across a store reopen"
        );
        let recovery = recovered_store
            .bind_assessment_intent_from_finalization_inputs(
                &run_id,
                recovered_store.get_run(&run_id).unwrap().unwrap().revision,
                "attempt-1",
                request,
            )
            .unwrap();
        assert!(
            !recovery.newly_created,
            "recovery must never reauthorize dispatch"
        );
        assert_eq!(recovery.binding, claim.binding);
        assert_eq!(saved.index_reference(), &finalization_reference);
    }

    #[test]
    fn finalization_input_claim_rejects_stale_or_mismatched_host_facts_without_an_intent() {
        let temp = tempfile::tempdir().unwrap();
        let (store, run_id, contract) =
            create_interactive_semantic_run(temp.path(), "run-c2-finalization-reject");
        let report_bytes = assessment_report(&run_id);
        let report_reference = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "agent-report.json",
                "application/json",
                &report_bytes,
            )
            .unwrap();
        let manifest_reference = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "snapshot-manifest.json",
                "application/json",
                b"{}",
            )
            .unwrap();
        let before_inputs = store.get_run(&run_id).unwrap().unwrap();
        assert!(matches!(
            store.write_verified_finalization_inputs(
                &run_id,
                before_inputs.revision + 1,
                "attempt-1",
                valid_host_finalization_inputs(
                    &store,
                    &run_id,
                    report_reference.clone(),
                    None,
                    None,
                ),
            ),
            Err(CodingAgentError::Conflict(_))
        ));
        assert!(
            store
                .read_saved_verified_finalization_inputs(&run_id, "attempt-1")
                .is_err()
        );

        store
            .write_verified_finalization_inputs(
                &run_id,
                before_inputs.revision,
                "attempt-1",
                valid_host_finalization_inputs(
                    &store,
                    &run_id,
                    report_reference.clone(),
                    None,
                    None,
                ),
            )
            .unwrap();
        let (input_bytes, mut request) = assessment_payload_and_request(
            &store,
            &run_id,
            &contract,
            &report_reference,
            &manifest_reference,
        );
        request.judge_input_evidence_id = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "judge-input.json",
                "application/json",
                &input_bytes,
            )
            .unwrap()
            .evidence_id;
        request.target_full_commit_id = "a".repeat(40);

        assert!(matches!(
            store.bind_assessment_intent_from_finalization_inputs(
                &run_id,
                store.get_run(&run_id).unwrap().unwrap().revision,
                "attempt-1",
                request,
            ),
            Err(CodingAgentError::Conflict(_) | CodingAgentError::Invalid(_))
        ));
        assert!(matches!(
            store.assessment_intent(&run_id, "attempt-1"),
            Err(CodingAgentError::NotFound(_))
        ));
    }

    #[test]
    fn finalization_inputs_are_immutable_and_unresolved_cancellation_fences_the_claim() {
        let temp = tempfile::tempdir().unwrap();
        let (store, run_id, contract) =
            create_interactive_semantic_run(temp.path(), "run-c2-finalization-cancel");
        let report_bytes = assessment_report(&run_id);
        let report_reference = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "agent-report.json",
                "application/json",
                &report_bytes,
            )
            .unwrap();
        let manifest_reference = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "snapshot-manifest.json",
                "application/json",
                b"{}",
            )
            .unwrap();
        let before_inputs = store.get_run(&run_id).unwrap().unwrap();
        let original =
            valid_host_finalization_inputs(&store, &run_id, report_reference.clone(), None, None);
        store
            .write_verified_finalization_inputs(
                &run_id,
                before_inputs.revision,
                "attempt-1",
                original.clone(),
            )
            .unwrap();
        let current = store.get_run(&run_id).unwrap().unwrap();
        assert!(matches!(
            store.write_verified_finalization_inputs(&run_id, current.revision, "attempt-1", {
                let mut changed = original;
                changed.cli_exit = ProcessExitObservation::ExitedNormally { exit_code: 1 };
                changed
            },),
            Err(CodingAgentError::Conflict(_))
        ));

        let (input_bytes, mut request) = assessment_payload_and_request(
            &store,
            &run_id,
            &contract,
            &report_reference,
            &manifest_reference,
        );
        request.judge_input_evidence_id = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "judge-input.json",
                "application/json",
                &input_bytes,
            )
            .unwrap()
            .evidence_id;
        store
            .claim_effect(
                &run_id,
                "attempt-1",
                "cancel:attempt-1",
                DurableOperationRequest::CancelAgent,
            )
            .unwrap();
        assert!(matches!(
            store.bind_assessment_intent_from_finalization_inputs(
                &run_id,
                store.get_run(&run_id).unwrap().unwrap().revision,
                "attempt-1",
                request,
            ),
            Err(CodingAgentError::Conflict(_))
        ));
        assert!(matches!(
            store.assessment_intent(&run_id, "attempt-1"),
            Err(CodingAgentError::NotFound(_))
        ));
    }

    #[test]
    fn unknown_exit_live_writers_and_unverified_freeze_cannot_claim_a_judge_intent() {
        for (suffix, mutate) in [
            ("unknown-exit", 0_u8),
            ("active-writers", 1_u8),
            ("unknown-freeze", 2_u8),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (store, run_id, contract) = create_interactive_semantic_run(
                temp.path(),
                &format!("run-c2-finalization-{suffix}"),
            );
            let report_bytes = assessment_report(&run_id);
            let report_reference = store
                .write_evidence(
                    &run_id,
                    "attempt-1",
                    "agent-report.json",
                    "application/json",
                    &report_bytes,
                )
                .unwrap();
            let manifest_reference = store
                .write_evidence(
                    &run_id,
                    "attempt-1",
                    "snapshot-manifest.json",
                    "application/json",
                    b"{}",
                )
                .unwrap();
            let current = store.get_run(&run_id).unwrap().unwrap();
            let mut inputs = valid_host_finalization_inputs(
                &store,
                &run_id,
                report_reference.clone(),
                None,
                None,
            );
            match mutate {
                0 => inputs.cli_exit = ProcessExitObservation::Unknown,
                1 => inputs.child_writers = ChildWritersStatus::Active,
                2 => inputs.workspace_freeze = WorkspaceFreezeStatus::Unknown,
                _ => unreachable!(),
            }
            store
                .write_verified_finalization_inputs(&run_id, current.revision, "attempt-1", inputs)
                .unwrap();
            let (input_bytes, mut request) = assessment_payload_and_request(
                &store,
                &run_id,
                &contract,
                &report_reference,
                &manifest_reference,
            );
            request.judge_input_evidence_id = store
                .write_evidence(
                    &run_id,
                    "attempt-1",
                    "judge-input.json",
                    "application/json",
                    &input_bytes,
                )
                .unwrap()
                .evidence_id;
            assert!(matches!(
                store.bind_assessment_intent_from_finalization_inputs(
                    &run_id,
                    store.get_run(&run_id).unwrap().unwrap().revision,
                    "attempt-1",
                    request,
                ),
                Err(CodingAgentError::Conflict(_))
            ));
            assert!(matches!(
                store.assessment_intent(&run_id, "attempt-1"),
                Err(CodingAgentError::NotFound(_))
            ));
        }
    }

    #[test]
    fn cancel_effect_claim_is_atomic_across_store_connections() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let primary = RunStore::open(&paths).unwrap();
        let (store, run_id) = create_run(temp.path(), "run-cancel-effect-claim");
        prepare_first_attempt(&store, &run_id);
        mark_current_attempt_running(&store, &run_id);

        let barrier = Arc::new(Barrier::new(3));
        let first_paths = paths.clone();
        let first_run = run_id.clone();
        let first_barrier = barrier.clone();
        let first = std::thread::spawn(move || {
            let store = RunStore::open(&first_paths).unwrap();
            first_barrier.wait();
            store.claim_effect(
                &first_run,
                "attempt-1",
                "cancel:attempt-1",
                DurableOperationRequest::CancelAgent,
            )
        });
        let second_paths = paths.clone();
        let second_run = run_id.clone();
        let second_barrier = barrier.clone();
        let second = std::thread::spawn(move || {
            let store = RunStore::open(&second_paths).unwrap();
            second_barrier.wait();
            store.claim_effect(
                &second_run,
                "attempt-1",
                "cancel:attempt-1",
                DurableOperationRequest::CancelAgent,
            )
        });
        barrier.wait();
        let first = first.join().unwrap().unwrap();
        let second = second.join().unwrap().unwrap();

        assert_ne!(first.newly_created, second.newly_created);
        assert_eq!(first.operation, second.operation);
        assert!(
            !primary
                .claim_effect(
                    &run_id,
                    "attempt-1",
                    "cancel:attempt-1",
                    DurableOperationRequest::CancelAgent,
                )
                .unwrap()
                .newly_created
        );
        assert!(matches!(
            primary.claim_effect(
                &run_id,
                "attempt-1",
                "cancel:attempt-1",
                DurableOperationRequest::FinishInteractive { profile_version: 2 },
            ),
            Err(CodingAgentError::Conflict(_))
        ));
    }

    #[test]
    fn cancel_claim_coexists_with_finish_claim_and_has_one_atomic_owner() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let primary = RunStore::open(&paths).unwrap();
        let mut approved = snapshot(
            "run-cancel-during-finish-claim",
            RunPurpose::Implementation,
            true,
        );
        approved.execution_mode = ExecutionMode::Interactive;
        let run_id = primary.create_run(approved).unwrap().snapshot.run_id;
        prepare_first_attempt(&primary, &run_id);
        mark_current_attempt_running(&primary, &run_id);
        primary
            .request_interactive_finish(&run_id, "attempt-1")
            .unwrap();
        let finish = primary
            .claim_effect(
                &run_id,
                "attempt-1",
                "finish:attempt-1",
                DurableOperationRequest::FinishInteractive { profile_version: 2 },
            )
            .unwrap();
        assert!(finish.newly_created);

        let barrier = Arc::new(Barrier::new(3));
        let first_paths = paths.clone();
        let first_run = run_id.clone();
        let first_barrier = Arc::clone(&barrier);
        let first = std::thread::spawn(move || {
            let store = RunStore::open(&first_paths).unwrap();
            first_barrier.wait();
            store.claim_effect(
                &first_run,
                "attempt-1",
                "cancel:attempt-1",
                DurableOperationRequest::CancelAgent,
            )
        });
        let second_paths = paths.clone();
        let second_run = run_id.clone();
        let second_barrier = Arc::clone(&barrier);
        let second = std::thread::spawn(move || {
            let store = RunStore::open(&second_paths).unwrap();
            second_barrier.wait();
            store.claim_effect(
                &second_run,
                "attempt-1",
                "cancel:attempt-1",
                DurableOperationRequest::CancelAgent,
            )
        });
        barrier.wait();
        let first = first.join().unwrap().unwrap();
        let second = second.join().unwrap().unwrap();

        assert_ne!(first.newly_created, second.newly_created);
        assert_eq!(first.operation, second.operation);
        assert!(matches!(
            first.operation.request,
            DurableOperationRequest::CancelAgent
        ));
        let finish_after = primary
            .operation(&run_id, "finish:attempt-1")
            .unwrap()
            .unwrap();
        assert_eq!(finish.operation, finish_after);
        assert!(
            !primary
                .claim_effect(
                    &run_id,
                    "attempt-1",
                    "cancel:attempt-1",
                    DurableOperationRequest::CancelAgent,
                )
                .unwrap()
                .newly_created
        );
    }

    #[test]
    fn cancellation_claim_prevents_a_later_interactive_finish_request() {
        let temp = tempfile::tempdir().unwrap();
        let store = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
        let mut approved = snapshot(
            "run-finish-after-cancel-claim",
            RunPurpose::Implementation,
            true,
        );
        approved.execution_mode = ExecutionMode::Interactive;
        let run_id = store.create_run(approved).unwrap().snapshot.run_id;
        prepare_first_attempt(&store, &run_id);
        mark_current_attempt_running(&store, &run_id);
        let cancel = store
            .claim_effect(
                &run_id,
                "attempt-1",
                "cancel:attempt-1",
                DurableOperationRequest::CancelAgent,
            )
            .unwrap();
        assert!(cancel.newly_created);

        assert!(matches!(
            store.request_interactive_finish(&run_id, "attempt-1"),
            Err(CodingAgentError::Conflict(_))
        ));
        assert!(matches!(
            store.claim_effect(
                &run_id,
                "attempt-1",
                "finish:attempt-1",
                DurableOperationRequest::FinishInteractive { profile_version: 2 },
            ),
            Err(CodingAgentError::Conflict(_))
        ));
        assert_eq!(
            store.get_run(&run_id).unwrap().unwrap().state,
            RunState::Running
        );
    }

    #[test]
    fn interactive_finish_effect_claim_is_new_only_for_first_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let store = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
        let mut approved = snapshot("run-finish-effect-claim", RunPurpose::Implementation, false);
        approved.execution_mode = ExecutionMode::Interactive;
        let run_id = store.create_run(approved).unwrap().snapshot.run_id;
        prepare_first_attempt(&store, &run_id);
        mark_current_attempt_running(&store, &run_id);
        store
            .request_interactive_finish(&run_id, "attempt-1")
            .unwrap();

        let first = store
            .claim_effect(
                &run_id,
                "attempt-1",
                "finish:attempt-1",
                DurableOperationRequest::FinishInteractive { profile_version: 2 },
            )
            .unwrap();
        let retry = store
            .claim_effect(
                &run_id,
                "attempt-1",
                "finish:attempt-1",
                DurableOperationRequest::FinishInteractive { profile_version: 2 },
            )
            .unwrap();

        assert!(first.newly_created);
        assert!(!retry.newly_created);
        assert_eq!(first.operation, retry.operation);
    }

    #[test]
    fn concurrent_start_attempts_consume_one_preparation_phase_and_bind_running_to_winner() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let primary = RunStore::open(&paths).unwrap();
        let mut approved = snapshot("run-start-claim-race", RunPurpose::Implementation, false);
        approved.attempt_ids = vec!["attempt-1".into(), "attempt-2".into(), "attempt-3".into()];
        let run_id = primary.create_run(approved).unwrap().snapshot.run_id;
        transition_to(&primary, &run_id, RunState::Queued);
        let preparing = transition_to(&primary, &run_id, RunState::Preparing);
        let expected_revision = preparing.revision;

        let barrier = Arc::new(Barrier::new(3));
        let first_paths = paths.clone();
        let first_run = run_id.clone();
        let first_barrier = barrier.clone();
        let first = std::thread::spawn(move || {
            let store = RunStore::open(&first_paths).unwrap();
            first_barrier.wait();
            store.start_attempt(
                &first_run,
                expected_revision,
                "attempt-1",
                "dispatch:attempt-1",
            )
        });
        let second_paths = paths.clone();
        let second_run = run_id.clone();
        let second_barrier = barrier.clone();
        let second = std::thread::spawn(move || {
            let store = RunStore::open(&second_paths).unwrap();
            second_barrier.wait();
            store.start_attempt(
                &second_run,
                expected_revision,
                "attempt-2",
                "dispatch:attempt-2",
            )
        });
        barrier.wait();
        let results = [first.join().unwrap(), second.join().unwrap()];
        let winners = results
            .iter()
            .filter_map(|result| result.as_ref().ok())
            .collect::<Vec<_>>();
        assert_eq!(winners.len(), 1, "only one start intent may win this phase");
        assert!(winners[0].newly_created);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(CodingAgentError::Conflict(_))))
                .count(),
            1
        );
        let winner = winners[0];
        assert_eq!(winner.run_revision, expected_revision + 1);
        assert_eq!(winner.phase_generation, 0);

        let after_claim = primary.get_run(&run_id).unwrap().unwrap();
        assert_eq!(after_claim.revision, winner.run_revision);
        assert_eq!(
            after_claim.current_attempt_id.as_deref(),
            Some(winner.operation.attempt_id.as_str())
        );
        let next_attempt = if winner.operation.attempt_id == "attempt-1" {
            "attempt-2"
        } else {
            "attempt-3"
        };
        let losing_attempt = if winner.operation.attempt_id == "attempt-1" {
            "attempt-2"
        } else {
            "attempt-1"
        };
        assert!(matches!(
            primary.start_attempt(
                &run_id,
                after_claim.revision,
                next_attempt,
                "dispatch:second-key-same-phase"
            ),
            Err(CodingAgentError::Conflict(_))
        ));
        assert!(
            !primary
                .start_attempt(
                    &run_id,
                    expected_revision,
                    &winner.operation.attempt_id,
                    &winner.operation.idempotency_key
                )
                .unwrap()
                .newly_created
        );

        assert!(matches!(
            primary.transition(&run_id, expected_revision, RunState::Running),
            Err(CodingAgentError::Conflict(_))
        ));
        assert!(matches!(
            primary.transition(&run_id, winner.run_revision, RunState::Running),
            Err(CodingAgentError::Conflict(_))
        ));
        assert!(matches!(
            primary.mark_attempt_running(&run_id, winner.run_revision, losing_attempt),
            Err(CodingAgentError::Conflict(_))
        ));
        let running = primary
            .mark_attempt_running(&run_id, winner.run_revision, &winner.operation.attempt_id)
            .unwrap();
        assert_eq!(running.state, RunState::Running);
        assert_eq!(
            running.current_attempt_id.as_deref(),
            Some(winner.operation.attempt_id.as_str())
        );
    }

    #[test]
    fn retries_require_a_confirmed_terminal_attempt_and_a_new_phase_generation() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let mut approved = snapshot(
            "run-explicit-retry-phase",
            RunPurpose::Implementation,
            false,
        );
        approved.attempt_ids = vec!["attempt-1".into(), "attempt-2".into()];
        let run_id = store.create_run(approved).unwrap().snapshot.run_id;
        transition_to(&store, &run_id, RunState::Queued);
        let preparing = transition_to(&store, &run_id, RunState::Preparing);
        let first = store
            .start_attempt(
                &run_id,
                preparing.revision,
                "attempt-1",
                "dispatch:attempt-1",
            )
            .unwrap();
        store
            .complete_operation(
                &run_id,
                "dispatch:attempt-1",
                DurableOperationResult {
                    outcome: DurableOperationOutcome::Succeeded,
                    result_digest: Some("a".repeat(64)),
                    evidence_ids: Vec::new(),
                },
            )
            .unwrap();
        let running = store
            .mark_attempt_running(&run_id, first.run_revision, "attempt-1")
            .unwrap();
        let unknown = store
            .transition(&run_id, running.revision, RunState::Unknown)
            .unwrap();
        assert!(matches!(
            store.begin_retry_preparation(&run_id, unknown.revision, "attempt-1"),
            Err(CodingAgentError::Conflict(_))
        ));
        assert!(matches!(
            store.transition(&run_id, unknown.revision, RunState::Preparing),
            Err(CodingAgentError::Conflict(_))
        ));

        let failed = store
            .transition(&run_id, unknown.revision, RunState::Failed)
            .unwrap();
        let preparing_retry = store
            .begin_retry_preparation(&run_id, failed.revision, "attempt-1")
            .unwrap();
        assert_eq!(preparing_retry.state, RunState::Preparing);
        assert_eq!(preparing_retry.current_attempt_id, None);
        assert_eq!(preparing_retry.start_phase_generation, 1);

        let second = store
            .start_attempt(
                &run_id,
                preparing_retry.revision,
                "attempt-2",
                "dispatch:attempt-2",
            )
            .unwrap();
        assert!(second.newly_created);
        assert_eq!(second.phase_generation, 1);
        assert_eq!(second.run_revision, preparing_retry.revision + 1);
        assert!(matches!(
            store.mark_attempt_running(&run_id, second.run_revision, "attempt-1"),
            Err(CodingAgentError::Conflict(_))
        ));
        assert_eq!(
            store
                .mark_attempt_running(&run_id, second.run_revision, "attempt-2")
                .unwrap()
                .state,
            RunState::Running
        );
    }

    fn interactive_run_with_assessment(
        root: &std::path::Path,
        run_name: &str,
    ) -> (RunStore, RunId) {
        let store = RunStore::open(&DataPaths::with_root(root)).unwrap();
        let mut approved = snapshot(run_name, RunPurpose::Implementation, false);
        approved.execution_mode = ExecutionMode::Interactive;
        approved.publication_plan = PublicationPlan::PushBranch;
        approved.allowed_operations = vec![AllowedOperation::PublishBranch];
        let run_id = store.create_run(approved).unwrap().snapshot.run_id;
        prepare_first_attempt(&store, &run_id);
        mark_current_attempt_running(&store, &run_id);
        let report = store
            .write_evidence(&run_id, "attempt-1", "report", "text/markdown", b"report")
            .unwrap();
        let mut assessment = successful_evidence();
        assessment.report_reference = Some(report.evidence_id);
        assessment.report_save_confirmed = true;
        store
            .record_execution_evidence(&run_id, "attempt-1", assessment)
            .unwrap();
        (store, run_id)
    }

    #[test]
    fn interactive_finalization_persistence_preserves_typed_invocation_evidence() {
        let cases = [
            (
                "direct",
                ExecutionInvocationKind::DirectProcess,
                ShellProcessObservation::NotSpawned,
                true,
            ),
            (
                "wrapper",
                ExecutionInvocationKind::ShellWrapper,
                ShellProcessObservation::Spawned {
                    exit: ProcessExitObservation::ExitedNormally { exit_code: 0 },
                },
                true,
            ),
            (
                "unknown",
                ExecutionInvocationKind::Unknown,
                ShellProcessObservation::Unknown,
                false,
            ),
        ];

        for (name, invocation_kind, shell_observation, successful) in cases {
            let temp = tempfile::tempdir().unwrap();
            let (store, run_id) =
                interactive_run_with_assessment(temp.path(), &format!("interactive-{name}"));
            store
                .request_interactive_finish(&run_id, "attempt-1")
                .unwrap();
            let evidence = InteractiveFinalizationEvidence::from_host_observations_with_invocation(
                2,
                invocation_kind,
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                shell_observation.clone(),
                ChildWritersStatus::Stopped,
                WorkspaceFreezeStatus::HostVerified {
                    commit_id: "f".repeat(40),
                },
            );
            store
                .record_interactive_finalization(&run_id, "attempt-1", evidence.clone())
                .unwrap();
            store
                .record_interactive_finalization(&run_id, "attempt-1", evidence.clone())
                .expect("an exact duplicate remains idempotent");
            if invocation_kind == ExecutionInvocationKind::DirectProcess {
                let changed_invocation =
                    InteractiveFinalizationEvidence::from_host_observations_with_invocation(
                        2,
                        ExecutionInvocationKind::Unknown,
                        ProcessExitObservation::ExitedNormally { exit_code: 0 },
                        ShellProcessObservation::NotSpawned,
                        ChildWritersStatus::Stopped,
                        WorkspaceFreezeStatus::HostVerified {
                            commit_id: "f".repeat(40),
                        },
                    );
                assert!(matches!(
                    store
                        .record_interactive_finalization(&run_id, "attempt-1", changed_invocation,),
                    Err(CodingAgentError::Conflict(_))
                ));
            }

            let stored_json: String = store
                .connection
                .lock()
                .unwrap()
                .query_row(
                    "SELECT evidence_json FROM agent_run_interactive_finalizations
                     WHERE run_id = ?1 AND attempt_id = ?2",
                    params![run_id.as_str(), "attempt-1"],
                    |row| row.get(0),
                )
                .unwrap();
            let stored: serde_json::Value = serde_json::from_str(&stored_json).unwrap();
            assert_eq!(
                stored["invocation_kind"],
                serde_json::to_value(invocation_kind).unwrap(),
                "persisted invocation kind for {name}"
            );
            assert_eq!(
                stored["shell_observation"],
                serde_json::to_value(&shell_observation).unwrap(),
                "persisted shell observation for {name}"
            );
            if invocation_kind == ExecutionInvocationKind::DirectProcess {
                assert_eq!(stored["shell_exit"], "unknown");
            }

            drop(store);
            let reopened = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
            let restored = reopened
                .get_run(&run_id)
                .unwrap()
                .unwrap()
                .interactive_finalization
                .unwrap();
            assert_eq!(restored.invocation_kind(), invocation_kind, "{name}");
            assert_eq!(restored.shell_observation(), &shell_observation, "{name}");
            assert_eq!(
                restored.is_successful_for(2, &successful_evidence()),
                successful,
                "{name}"
            );
        }
    }

    #[test]
    fn legacy_interactive_finalization_without_invocation_metadata_stays_unknown() {
        let cases = [
            (
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ShellProcessObservation::Spawned {
                    exit: ProcessExitObservation::ExitedNormally { exit_code: 0 },
                },
            ),
            (
                ProcessExitObservation::Unknown,
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ShellProcessObservation::Spawned {
                    exit: ProcessExitObservation::ExitedNormally { exit_code: 0 },
                },
            ),
            (
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ProcessExitObservation::Unknown,
                ShellProcessObservation::Unknown,
            ),
        ];

        for (index, (cli_exit, shell_exit, expected_shell)) in cases.into_iter().enumerate() {
            let temp = tempfile::tempdir().unwrap();
            let (store, run_id) = interactive_run_with_assessment(
                temp.path(),
                &format!("interactive-legacy-{index}"),
            );
            store
                .request_interactive_finish(&run_id, "attempt-1")
                .unwrap();
            let legacy_json = serde_json::json!({
                "profile_version": 2,
                "cli_exit": serde_json::to_value(cli_exit).unwrap(),
                "shell_exit": serde_json::to_value(shell_exit).unwrap(),
                "child_writers": "stopped",
                "workspace_freeze": {
                    "state": "host_verified",
                    "commit_id": "f".repeat(40),
                },
            });
            store
                .connection
                .lock()
                .unwrap()
                .execute(
                    "INSERT INTO agent_run_interactive_finalizations
                        (run_id, attempt_id, evidence_json, recorded_at_ms)
                     VALUES (?1, ?2, ?3, 1)",
                    params![run_id.as_str(), "attempt-1", legacy_json.to_string()],
                )
                .unwrap();

            let restored = store.get_run(&run_id).unwrap().unwrap();
            let evidence = restored.interactive_finalization.unwrap();
            assert_eq!(
                evidence.invocation_kind(),
                ExecutionInvocationKind::Unknown,
                "legacy case {index}"
            );
            assert_eq!(
                evidence.shell_observation(),
                &expected_shell,
                "legacy case {index}"
            );
            assert!(
                !evidence.is_successful_for(2, &successful_evidence()),
                "legacy case {index}"
            );
            assert!(matches!(
                store.transition(&run_id, restored.revision, RunState::Verifying),
                Err(CodingAgentError::Conflict(_))
            ));
        }
    }

    #[test]
    fn malformed_typed_interactive_finalization_is_rejected_on_read() {
        let malformed = [
            (
                "direct-with-shell",
                serde_json::json!({
                    "invocation_kind": "direct_process",
                    "shell_observation": {
                        "state": "spawned",
                        "exit": { "exited_normally": { "exit_code": 0 } },
                    },
                    "shell_exit": { "exited_normally": { "exit_code": 0 } },
                }),
            ),
            (
                "wrapper-without-shell",
                serde_json::json!({
                    "invocation_kind": "shell_wrapper",
                    "shell_observation": { "state": "not_spawned" },
                    "shell_exit": "unknown",
                }),
            ),
            (
                "partial-metadata",
                serde_json::json!({ "invocation_kind": "direct_process" }),
            ),
            (
                "both-explicit-null",
                serde_json::json!({
                    "invocation_kind": null,
                    "shell_observation": null,
                }),
            ),
            (
                "null-kind-present-shell",
                serde_json::json!({
                    "invocation_kind": null,
                    "shell_observation": { "state": "unknown" },
                }),
            ),
            (
                "present-kind-null-shell",
                serde_json::json!({
                    "invocation_kind": "direct_process",
                    "shell_observation": null,
                }),
            ),
            (
                "invalid-invocation-type",
                serde_json::json!({
                    "invocation_kind": 1,
                    "shell_observation": { "state": "unknown" },
                }),
            ),
        ];

        for (index, (name, metadata)) in malformed.into_iter().enumerate() {
            let temp = tempfile::tempdir().unwrap();
            let (store, run_id) = interactive_run_with_assessment(
                temp.path(),
                &format!("interactive-malformed-{index}"),
            );
            store
                .request_interactive_finish(&run_id, "attempt-1")
                .unwrap();
            let mut raw = serde_json::json!({
                "profile_version": 2,
                "cli_exit": { "exited_normally": { "exit_code": 0 } },
                "shell_exit": "unknown",
                "child_writers": "stopped",
                "workspace_freeze": {
                    "state": "host_verified",
                    "commit_id": "f".repeat(40),
                },
            });
            for (key, value) in metadata.as_object().unwrap() {
                raw[key] = value.clone();
            }
            store
                .connection
                .lock()
                .unwrap()
                .execute(
                    "INSERT INTO agent_run_interactive_finalizations
                        (run_id, attempt_id, evidence_json, recorded_at_ms)
                     VALUES (?1, ?2, ?3, 1)",
                    params![run_id.as_str(), "attempt-1", raw.to_string()],
                )
                .unwrap();

            assert!(
                matches!(store.get_run(&run_id), Err(CodingAgentError::Corrupt(_))),
                "malformed evidence {name} must be rejected, not normalized"
            );
        }
    }

    #[test]
    fn record_interactive_finalization_rejects_invocation_shell_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let (store, run_id) =
            interactive_run_with_assessment(temp.path(), "interactive-mismatched-shell");
        store
            .request_interactive_finish(&run_id, "attempt-1")
            .unwrap();
        let contradictory = InteractiveFinalizationEvidence::from_host_observations_with_invocation(
            2,
            ExecutionInvocationKind::DirectProcess,
            ProcessExitObservation::ExitedNormally { exit_code: 0 },
            ShellProcessObservation::Spawned {
                exit: ProcessExitObservation::ExitedNormally { exit_code: 0 },
            },
            ChildWritersStatus::Stopped,
            WorkspaceFreezeStatus::HostVerified {
                commit_id: "f".repeat(40),
            },
        );

        assert!(matches!(
            store.record_interactive_finalization(&run_id, "attempt-1", contradictory),
            Err(CodingAgentError::Corrupt(_))
        ));
        assert!(
            store
                .get_run(&run_id)
                .unwrap()
                .unwrap()
                .interactive_finalization
                .is_none()
        );
    }

    #[test]
    fn approved_snapshot_reopens_unchanged_and_unapproved_auto_done_is_cleared() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let created = store
            .create_run(snapshot("run-immutable", RunPurpose::Implementation, true))
            .unwrap();

        assert!(!created.snapshot.auto_done);
        assert_eq!(created.state, RunState::ApprovalPending);
        assert_eq!(created.stage, RunStage::Approval);
        assert_eq!(created.revision, 0);
        assert_eq!(created.snapshot.attempt_ids, vec!["attempt-1"]);
        let persisted = store.get_run(&created.snapshot.run_id).unwrap().unwrap();
        assert_eq!(persisted.snapshot, created.snapshot);

        drop(store);
        let reopened = RunStore::open(&paths).unwrap();
        assert_eq!(
            reopened.get_run(&created.snapshot.run_id).unwrap(),
            Some(created)
        );
        let json = serde_json::to_string(&persisted.snapshot).unwrap();
        assert!(!json.contains("private-key"));
        assert!(json.contains("keychain:agent-home"));
    }

    #[test]
    fn explicit_approval_is_snapshotted_for_both_task_purposes() {
        let temp = tempfile::tempdir().unwrap();
        let store = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
        let mut implementation = snapshot("run-approved-auto", RunPurpose::Implementation, true);
        implementation.approval = Some(RunApproval {
            approved_by: "human:local-user".into(),
            approved_at_ms: 123,
            auto_done_approved: true,
            settings_version: Some(9),
            applicable_modes: vec![ExecutionMode::OneShot],
        });
        let accepted = store.create_run(implementation).unwrap();
        assert!(accepted.snapshot.auto_done);
        assert_eq!(
            accepted
                .snapshot
                .approval
                .as_ref()
                .unwrap()
                .settings_version,
            Some(9)
        );

        let mut investigation = snapshot(
            "run-approved-investigation",
            RunPurpose::Investigation,
            true,
        );
        investigation.approval = accepted.snapshot.approval;
        let excluded = store.create_run(investigation).unwrap();
        assert!(excluded.snapshot.auto_done);
    }

    #[test]
    fn stale_revision_is_rejected_and_concurrent_writers_cannot_lose_a_transition() {
        let temp = tempfile::tempdir().unwrap();
        let (store, run_id) = create_run(temp.path(), "run-cas");
        let queued = store.transition(&run_id, 0, RunState::Queued).unwrap();
        assert_eq!(queued.revision, 1);
        assert!(matches!(
            store.transition(&run_id, 0, RunState::Failed),
            Err(CodingAgentError::Conflict(_))
        ));

        let race_temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(race_temp.path());
        let primary = RunStore::open(&paths).unwrap();
        let race_id = primary
            .create_run(snapshot("run-race", RunPurpose::Implementation, false))
            .unwrap()
            .snapshot
            .run_id;
        let barrier = Arc::new(Barrier::new(3));
        let mut joins = Vec::new();
        for target in [RunState::Queued, RunState::Failed] {
            let paths = paths.clone();
            let run_id = race_id.clone();
            let barrier = barrier.clone();
            joins.push(std::thread::spawn(move || {
                let store = RunStore::open(&paths).unwrap();
                barrier.wait();
                store.transition(&run_id, 0, target)
            }));
        }
        barrier.wait();
        let results = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(CodingAgentError::Conflict(_))))
                .count(),
            1
        );
        assert_eq!(primary.get_run(&race_id).unwrap().unwrap().revision, 1);
    }

    #[test]
    fn operation_intents_are_durable_idempotent_and_reject_key_payload_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let (store, run_id) = create_run(temp.path(), "run-operations");
        assert!(matches!(
            store.start_attempt(&run_id, 0, "attempt-1", "dispatch:attempt-1"),
            Err(CodingAgentError::Conflict(_))
        ));
        store.transition(&run_id, 0, RunState::Queued).unwrap();
        let preparing = store.transition(&run_id, 1, RunState::Preparing).unwrap();
        assert!(matches!(
            store.begin_operation(
                &run_id,
                "attempt-1",
                "dispatch:generic-start",
                DurableOperationRequest::StartAgent {
                    workflow_definition_hash: "c".repeat(64),
                    attempt_id: "attempt-1".into(),
                    execution_mode: ExecutionMode::OneShot,
                },
            ),
            Err(CodingAgentError::Invalid(_))
        ));
        let first = store
            .start_attempt(
                &run_id,
                preparing.revision,
                "attempt-1",
                "dispatch:attempt-1",
            )
            .unwrap();
        let repeated = store
            .start_attempt(
                &run_id,
                preparing.revision,
                "attempt-1",
                "dispatch:attempt-1",
            )
            .unwrap();
        assert!(first.newly_created);
        assert!(!repeated.newly_created);
        assert_eq!(
            first.operation.operation_id,
            repeated.operation.operation_id
        );
        assert_eq!(repeated.operation.status, DurableOperationStatus::Intent);

        let conflicting = DurableOperationRequest::VerifyEvidence {
            evidence_ids: Vec::new(),
            commit_id: None,
        };
        assert!(matches!(
            store.begin_operation(&run_id, "attempt-1", "dispatch:attempt-1", conflicting),
            Err(CodingAgentError::Conflict(_))
        ));
        assert_eq!(store.unresolved_operations().unwrap().len(), 1);

        let result = DurableOperationResult {
            outcome: DurableOperationOutcome::Succeeded,
            result_digest: Some("e".repeat(64)),
            evidence_ids: Vec::new(),
        };
        let completed = store
            .complete_operation(&run_id, "dispatch:attempt-1", result.clone())
            .unwrap();
        assert_eq!(completed.status, DurableOperationStatus::Completed);
        drop(store);

        let reopened = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
        assert_eq!(
            reopened
                .operation(&run_id, "dispatch:attempt-1")
                .unwrap()
                .unwrap()
                .result,
            Some(result.clone())
        );
        assert_eq!(
            reopened
                .complete_operation(&run_id, "dispatch:attempt-1", result)
                .unwrap(),
            completed
        );
        assert!(matches!(
            reopened.complete_operation(
                &run_id,
                "dispatch:attempt-1",
                DurableOperationResult {
                    outcome: DurableOperationOutcome::Failed,
                    result_digest: None,
                    evidence_ids: Vec::new(),
                }
            ),
            Err(CodingAgentError::Conflict(_))
        ));
    }

    #[test]
    fn evidence_bytes_are_atomically_referenced_verified_and_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let (store, run_id) = create_run(temp.path(), "run-evidence");
        prepare_first_attempt(&store, &run_id);
        let stdout = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "stdout",
                "application/octet-stream",
                b"stream bytes",
            )
            .unwrap();
        let report = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "report",
                "text/markdown",
                b"checked report",
            )
            .unwrap();
        assert!(stdout.save_confirmed);
        assert!(store.verify_evidence(&run_id, &stdout.evidence_id).unwrap());

        let mut evidence = successful_evidence();
        evidence.stdout_reference = Some(stdout.evidence_id.clone());
        evidence.stdout_bytes = stdout.size_bytes;
        evidence.report_reference = Some(report.evidence_id.clone());
        store
            .record_execution_evidence(&run_id, "attempt-1", evidence.clone())
            .unwrap();
        let record = store.get_run(&run_id).unwrap().unwrap();
        assert_eq!(record.execution_evidence, Some(evidence));
        assert!(
            record
                .evidence
                .iter()
                .any(|item| item.evidence_id == report.evidence_id)
        );

        let path = DataPaths::with_root(temp.path())
            .coding_agent_runs_dir()
            .join(run_id.as_str())
            .join("evidence")
            .join(format!("{}.blob", stdout.evidence_id));
        std::fs::write(path, b"tampered bytes").unwrap();
        assert!(!store.verify_evidence(&run_id, &stdout.evidence_id).unwrap());

        let broken_id = RunId::new("run-file-obstacle").unwrap();
        store
            .create_run(snapshot(
                "run-file-obstacle",
                RunPurpose::Implementation,
                false,
            ))
            .unwrap();
        prepare_first_attempt(&store, &broken_id);
        let obstacle = DataPaths::with_root(temp.path())
            .coding_agent_runs_dir()
            .join(broken_id.as_str());
        std::fs::write(&obstacle, b"not a directory").unwrap();
        assert!(
            store
                .write_evidence(&broken_id, "attempt-1", "report", "text/plain", b"no")
                .is_err()
        );
        assert!(store.evidence(&broken_id).unwrap().is_empty());
    }

    #[test]
    fn publication_cannot_advance_when_end_or_success_evidence_is_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let mut approved = snapshot("run-gates", RunPurpose::Implementation, false);
        approved.publication_plan = PublicationPlan::PushBranch;
        approved.allowed_operations = vec![AllowedOperation::PublishBranch];
        let run_id = store.create_run(approved).unwrap().snapshot.run_id;
        prepare_first_attempt(&store, &run_id);
        mark_current_attempt_running(&store, &run_id);
        let report = store
            .write_evidence(&run_id, "attempt-1", "report", "text/markdown", b"report")
            .unwrap();
        let mut missing_end = successful_evidence();
        missing_end.end_present = false;
        missing_end.report_reference = Some(report.evidence_id);
        missing_end.report_save_confirmed = true;
        store
            .record_execution_evidence(&run_id, "attempt-1", missing_end)
            .unwrap();
        transition_to(&store, &run_id, RunState::Verifying);

        let revision = store.get_run(&run_id).unwrap().unwrap().revision;
        assert!(matches!(
            store.transition(&run_id, revision, RunState::Publishing),
            Err(CodingAgentError::Conflict(_))
        ));
        assert_eq!(
            store.get_run(&run_id).unwrap().unwrap().state,
            RunState::Verifying
        );
    }

    #[test]
    fn publication_intents_recheck_exact_commit_and_durable_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let mut approved = snapshot(
            "run-publication-intent-gate",
            RunPurpose::Implementation,
            false,
        );
        approved.publication_plan = PublicationPlan::PushBranch;
        approved.allowed_operations = vec![AllowedOperation::PublishBranch];
        let run_id = store.create_run(approved).unwrap().snapshot.run_id;
        store.transition(&run_id, 0, RunState::Queued).unwrap();
        let preparing = store.transition(&run_id, 1, RunState::Preparing).unwrap();
        let start = store
            .start_attempt(
                &run_id,
                preparing.revision,
                "attempt-1",
                "dispatch:attempt-1",
            )
            .unwrap();
        store
            .mark_attempt_running(&run_id, start.run_revision, "attempt-1")
            .unwrap();
        let report = store
            .write_evidence(&run_id, "attempt-1", "report", "text/markdown", b"report")
            .unwrap();
        let mut evidence = successful_evidence();
        evidence.report_reference = Some(report.evidence_id.clone());
        evidence.report_save_confirmed = true;
        store
            .record_execution_evidence(&run_id, "attempt-1", evidence)
            .unwrap();
        transition_to(&store, &run_id, RunState::Verifying);
        let publishing = transition_to(&store, &run_id, RunState::Publishing);

        let wrong_commit = DurableOperationRequest::PublishBranch {
            branch: "coding-agent/issue-72".into(),
            commit_id: "0".repeat(40),
        };
        assert!(matches!(
            store.claim_publication_effect(
                &run_id,
                publishing.revision,
                "attempt-1",
                "publish:wrong-commit",
                wrong_commit,
            ),
            Err(CodingAgentError::Conflict(_))
        ));

        let publish = DurableOperationRequest::PublishBranch {
            branch: "coding-agent/issue-72".into(),
            commit_id: "f".repeat(40),
        };
        let claim = store
            .claim_publication_effect(
                &run_id,
                publishing.revision,
                "attempt-1",
                "publish:confirmed",
                publish.clone(),
            )
            .unwrap();
        assert_eq!(claim.operation.status, DurableOperationStatus::Intent);
        assert!(matches!(
            store.complete_operation(
                &run_id,
                "publish:confirmed",
                DurableOperationResult {
                    outcome: DurableOperationOutcome::Succeeded,
                    result_digest: Some("e".repeat(64)),
                    evidence_ids: Vec::new(),
                },
            ),
            Err(CodingAgentError::Conflict(_))
        ));
        let completed = store
            .complete_publication_effect(
                claim.permit.unwrap(),
                DurableOperationResult {
                    outcome: DurableOperationOutcome::Succeeded,
                    result_digest: Some("e".repeat(64)),
                    evidence_ids: Vec::new(),
                },
            )
            .unwrap();
        assert_eq!(completed.status, DurableOperationStatus::Completed);

        let report_path = paths
            .coding_agent_runs_dir()
            .join(run_id.as_str())
            .join("evidence")
            .join(format!("{}.blob", report.evidence_id));
        std::fs::write(report_path, b"tampered report").unwrap();

        assert_eq!(
            store
                .claim_publication_effect(
                    &run_id,
                    publishing.revision,
                    "attempt-1",
                    "publish:confirmed",
                    publish.clone(),
                )
                .unwrap()
                .operation,
            completed
        );
        assert!(matches!(
            store.claim_publication_effect(
                &run_id,
                publishing.revision,
                "attempt-1",
                "publish:after-tamper",
                publish,
            ),
            Err(CodingAgentError::Conflict(_))
        ));
    }

    #[test]
    fn a_new_attempt_cannot_reuse_prior_assessment_for_transition_or_publication() {
        let temp = tempfile::tempdir().unwrap();
        let store = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
        let mut approved = snapshot("run-attempt-binding", RunPurpose::Implementation, false);
        approved.attempt_ids = vec!["attempt-1".into(), "attempt-2".into()];
        approved.publication_plan = PublicationPlan::PushBranch;
        approved.allowed_operations = vec![AllowedOperation::PublishBranch];
        let run_id = store.create_run(approved).unwrap().snapshot.run_id;
        store.transition(&run_id, 0, RunState::Queued).unwrap();
        let preparing = store.transition(&run_id, 1, RunState::Preparing).unwrap();
        let first_start = store
            .start_attempt(
                &run_id,
                preparing.revision,
                "attempt-1",
                "dispatch:attempt-1",
            )
            .unwrap();
        store
            .complete_operation(
                &run_id,
                "dispatch:attempt-1",
                DurableOperationResult {
                    outcome: DurableOperationOutcome::Succeeded,
                    result_digest: Some("a".repeat(64)),
                    evidence_ids: Vec::new(),
                },
            )
            .unwrap();
        store
            .mark_attempt_running(&run_id, first_start.run_revision, "attempt-1")
            .unwrap();
        let first_report = store
            .write_evidence(&run_id, "attempt-1", "report", "text/markdown", b"first")
            .unwrap();
        let mut first_evidence = successful_evidence();
        first_evidence.report_reference = Some(first_report.evidence_id);
        first_evidence.report_save_confirmed = true;
        store
            .record_execution_evidence(&run_id, "attempt-1", first_evidence.clone())
            .unwrap();
        transition_to(&store, &run_id, RunState::Verifying);
        let unknown = transition_to(&store, &run_id, RunState::Unknown);
        assert!(matches!(
            store.start_attempt(&run_id, unknown.revision, "attempt-2", "dispatch:attempt-2"),
            Err(CodingAgentError::Conflict(_))
        ));
        let failed = store
            .transition(&run_id, unknown.revision, RunState::Failed)
            .unwrap();
        let retry_preparing = store
            .begin_retry_preparation(&run_id, failed.revision, "attempt-1")
            .unwrap();

        let second_start = store
            .start_attempt(
                &run_id,
                retry_preparing.revision,
                "attempt-2",
                "dispatch:attempt-2",
            )
            .unwrap();
        assert!(second_start.newly_created);
        assert_eq!(
            second_start.operation.status,
            DurableOperationStatus::Intent
        );
        let repeated_start = store
            .start_attempt(
                &run_id,
                retry_preparing.revision,
                "attempt-2",
                "dispatch:attempt-2",
            )
            .unwrap();
        assert!(!repeated_start.newly_created);
        assert_eq!(repeated_start.operation, second_start.operation);
        let second_run = store.get_run(&run_id).unwrap().unwrap();
        assert_eq!(second_run.current_attempt_id.as_deref(), Some("attempt-2"));
        assert!(second_run.execution_evidence.is_none());
        assert!(matches!(
            store.record_execution_evidence(&run_id, "attempt-1", first_evidence),
            Err(CodingAgentError::Conflict(_))
        ));

        let running_second = store
            .mark_attempt_running(&run_id, second_start.run_revision, "attempt-2")
            .unwrap();
        let unknown_second = store
            .transition(&run_id, running_second.revision, RunState::Unknown)
            .unwrap();
        assert!(matches!(
            store.transition(&run_id, unknown_second.revision, RunState::Publishing),
            Err(CodingAgentError::Conflict(_))
        ));
        let second_report = store
            .write_evidence(&run_id, "attempt-2", "report", "text/markdown", b"second")
            .unwrap();
        let mut second_evidence = successful_evidence();
        second_evidence.commit_id = Some("9".repeat(40));
        second_evidence.report_reference = Some(second_report.evidence_id);
        second_evidence.report_save_confirmed = true;
        store
            .record_execution_evidence(&run_id, "attempt-2", second_evidence.clone())
            .unwrap();
        transition_to(&store, &run_id, RunState::Verifying);
        let publishing = transition_to(&store, &run_id, RunState::Publishing);

        let published = store
            .claim_publication_effect(
                &run_id,
                publishing.revision,
                "attempt-2",
                "publish:attempt-2",
                DurableOperationRequest::PublishBranch {
                    branch: "coding-agent/issue-72".into(),
                    commit_id: "9".repeat(40),
                },
            )
            .unwrap();
        assert_eq!(published.operation.attempt_id, "attempt-2");
        let final_run = store.get_run(&run_id).unwrap().unwrap();
        assert_eq!(final_run.current_attempt_id.as_deref(), Some("attempt-2"));
        assert_eq!(final_run.execution_evidence, Some(second_evidence));
    }

    #[test]
    fn interactive_verification_requires_finish_exit_writer_and_host_freeze_proof() {
        let missing = {
            let temp = tempfile::tempdir().unwrap();
            let (store, run_id) =
                interactive_run_with_assessment(temp.path(), "interactive-no-proof");
            store
                .request_interactive_finish(&run_id, "attempt-1")
                .unwrap();
            let revision = store.get_run(&run_id).unwrap().unwrap().revision;
            assert!(matches!(
                store.transition(&run_id, revision, RunState::Verifying),
                Err(CodingAgentError::Conflict(_))
            ));
            store.get_run(&run_id).unwrap().unwrap()
        };
        assert!(missing.interactive_finish_request.is_some());
        assert!(missing.interactive_finalization.is_none());

        let cases = [
            (
                "cli-nonzero",
                ProcessExitObservation::ExitedNormally { exit_code: 130 },
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ChildWritersStatus::Stopped,
                WorkspaceFreezeStatus::HostVerified {
                    commit_id: "f".repeat(40),
                },
            ),
            (
                "cli-unknown",
                ProcessExitObservation::Unknown,
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ChildWritersStatus::Stopped,
                WorkspaceFreezeStatus::HostVerified {
                    commit_id: "f".repeat(40),
                },
            ),
            (
                "shell-killed",
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ProcessExitObservation::Killed,
                ChildWritersStatus::Stopped,
                WorkspaceFreezeStatus::HostVerified {
                    commit_id: "f".repeat(40),
                },
            ),
            (
                "shell-nonzero",
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ProcessExitObservation::ExitedNormally { exit_code: 1 },
                ChildWritersStatus::Stopped,
                WorkspaceFreezeStatus::HostVerified {
                    commit_id: "f".repeat(40),
                },
            ),
            (
                "eof-disconnect",
                ProcessExitObservation::Disconnected,
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ChildWritersStatus::Stopped,
                WorkspaceFreezeStatus::HostVerified {
                    commit_id: "f".repeat(40),
                },
            ),
            (
                "signal-cancel",
                ProcessExitObservation::Signaled { signal: 2 },
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ChildWritersStatus::Stopped,
                WorkspaceFreezeStatus::HostVerified {
                    commit_id: "f".repeat(40),
                },
            ),
            (
                "writer-active",
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ChildWritersStatus::Active,
                WorkspaceFreezeStatus::HostVerified {
                    commit_id: "f".repeat(40),
                },
            ),
            (
                "writer-unknown",
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ChildWritersStatus::Unknown,
                WorkspaceFreezeStatus::HostVerified {
                    commit_id: "f".repeat(40),
                },
            ),
            (
                "workspace-not-frozen",
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ChildWritersStatus::Stopped,
                WorkspaceFreezeStatus::NotFrozen,
            ),
            (
                "workspace-unknown",
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ChildWritersStatus::Stopped,
                WorkspaceFreezeStatus::Unknown,
            ),
            (
                "workspace-commit-mismatch",
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ProcessExitObservation::ExitedNormally { exit_code: 0 },
                ChildWritersStatus::Stopped,
                WorkspaceFreezeStatus::HostVerified {
                    commit_id: "0".repeat(40),
                },
            ),
        ];
        for (index, (name, cli, shell, writers, freeze)) in cases.into_iter().enumerate() {
            let temp = tempfile::tempdir().unwrap();
            let (store, run_id) =
                interactive_run_with_assessment(temp.path(), &format!("interactive-bad-{index}"));
            store
                .request_interactive_finish(&run_id, "attempt-1")
                .unwrap();
            store
                .record_interactive_finalization(
                    &run_id,
                    "attempt-1",
                    InteractiveFinalizationEvidence::from_host_observations(
                        2, cli, shell, writers, freeze,
                    ),
                )
                .unwrap();
            let revision = store.get_run(&run_id).unwrap().unwrap().revision;
            assert!(
                matches!(
                    store.transition(&run_id, revision, RunState::Verifying),
                    Err(CodingAgentError::Conflict(_))
                ),
                "accepted incomplete interactive proof {name}"
            );
        }

        let temp = tempfile::tempdir().unwrap();
        let (store, run_id) = interactive_run_with_assessment(temp.path(), "interactive-complete");
        let finish = store
            .request_interactive_finish(&run_id, "attempt-1")
            .unwrap();
        assert_eq!(finish.profile_version, 2);
        assert_eq!(
            store
                .request_interactive_finish(&run_id, "attempt-1")
                .unwrap(),
            finish
        );
        store
            .record_interactive_finalization(
                &run_id,
                "attempt-1",
                InteractiveFinalizationEvidence::from_host_observations(
                    2,
                    ProcessExitObservation::ExitedNormally { exit_code: 0 },
                    ProcessExitObservation::ExitedNormally { exit_code: 0 },
                    ChildWritersStatus::Stopped,
                    WorkspaceFreezeStatus::HostVerified {
                        commit_id: "f".repeat(40),
                    },
                ),
            )
            .unwrap();
        drop(store);
        let store = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
        let finalized = store.get_run(&run_id).unwrap().unwrap();
        assert_eq!(finalized.interactive_finish_request, Some(finish));
        assert!(finalized.interactive_finalization.is_some());
        let encoded = serde_json::to_value(&finalized).unwrap();
        let untrusted_round_trip: AgentRunRecord = serde_json::from_value(encoded).unwrap();
        assert!(untrusted_round_trip.interactive_finalization.is_none());
        transition_to(&store, &run_id, RunState::Verifying);
        let publishing = transition_to(&store, &run_id, RunState::Publishing);
        let intent = store
            .claim_publication_effect(
                &run_id,
                publishing.revision,
                "attempt-1",
                "interactive-publish",
                DurableOperationRequest::PublishBranch {
                    branch: "coding-agent/issue-72".into(),
                    commit_id: "f".repeat(40),
                },
            )
            .unwrap();
        assert_eq!(intent.operation.status, DurableOperationStatus::Intent);
    }

    #[test]
    fn interactive_finish_and_profile_evidence_are_bound_to_current_attempt_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let store = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
        let mut approved = snapshot("interactive-binding", RunPurpose::Implementation, false);
        approved.execution_mode = ExecutionMode::Interactive;
        approved.attempt_ids = vec!["attempt-1".into(), "attempt-2".into()];
        let run_id = store.create_run(approved).unwrap().snapshot.run_id;
        transition_to(&store, &run_id, RunState::Queued);
        let preparing = transition_to(&store, &run_id, RunState::Preparing);
        let started = store
            .start_attempt(
                &run_id,
                preparing.revision,
                "attempt-1",
                "dispatch:attempt-1",
            )
            .unwrap();
        store
            .mark_attempt_running(&run_id, started.run_revision, "attempt-1")
            .unwrap();

        assert!(matches!(
            store.request_interactive_finish(&run_id, "attempt-2"),
            Err(CodingAgentError::Conflict(_))
        ));
        store
            .request_interactive_finish(&run_id, "attempt-1")
            .unwrap();
        let mismatched_profile = InteractiveFinalizationEvidence::from_host_observations(
            3,
            ProcessExitObservation::ExitedNormally { exit_code: 0 },
            ProcessExitObservation::ExitedNormally { exit_code: 0 },
            ChildWritersStatus::Stopped,
            WorkspaceFreezeStatus::HostVerified {
                commit_id: "f".repeat(40),
            },
        );
        assert!(matches!(
            store.record_interactive_finalization(&run_id, "attempt-1", mismatched_profile),
            Err(CodingAgentError::Conflict(_))
        ));
        let wrong_attempt = InteractiveFinalizationEvidence::from_host_observations(
            2,
            ProcessExitObservation::ExitedNormally { exit_code: 0 },
            ProcessExitObservation::ExitedNormally { exit_code: 0 },
            ChildWritersStatus::Stopped,
            WorkspaceFreezeStatus::HostVerified {
                commit_id: "f".repeat(40),
            },
        );
        assert!(matches!(
            store.record_interactive_finalization(&run_id, "attempt-2", wrong_attempt),
            Err(CodingAgentError::Conflict(_))
        ));
    }

    #[test]
    fn version_one_run_store_migrates_without_selecting_a_current_attempt() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        paths.ensure_coding_agent_runs().unwrap();
        let connection = Connection::open(paths.coding_agent_runs_db_path()).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE agent_runs (
                     run_id TEXT PRIMARY KEY,
                     snapshot_json TEXT NOT NULL,
                     revision INTEGER NOT NULL,
                     state_json TEXT NOT NULL,
                     stage_json TEXT NOT NULL,
                     created_at_ms INTEGER NOT NULL
                 );
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let store = RunStore::open(&paths).unwrap();
        let version: i64 = store
            .connection
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, STORE_SCHEMA_VERSION);
    }

    #[test]
    fn migrated_legacy_start_intent_is_receipt_only_and_cannot_authorize_publication() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let mut approved = snapshot("run-legacy-start-intent", RunPurpose::Implementation, false);
        approved.attempt_ids = vec!["attempt-1".into(), "attempt-2".into()];
        approved.publication_plan = PublicationPlan::PushBranch;
        approved.allowed_operations = vec![AllowedOperation::PublishBranch];
        let run_id = store.create_run(approved).unwrap().snapshot.run_id;
        transition_to(&store, &run_id, RunState::Queued);
        let preparing = transition_to(&store, &run_id, RunState::Preparing);
        let start = store
            .start_attempt(&run_id, preparing.revision, "attempt-1", "dispatch:legacy")
            .unwrap();
        let report = store
            .write_evidence(&run_id, "attempt-1", "report", "text/markdown", b"report")
            .unwrap();
        let mut assessment = successful_evidence();
        assessment.report_reference = Some(report.evidence_id);
        assessment.report_save_confirmed = true;
        store
            .record_execution_evidence(&run_id, "attempt-1", assessment.clone())
            .unwrap();

        {
            let connection = store.connection.lock().unwrap();
            connection
                .execute(
                    "UPDATE agent_runs SET current_attempt_id = NULL, state_json = ?1,
                         stage_json = ?2 WHERE run_id = ?3",
                    params![
                        serde_json::to_string(&RunState::Preparing).unwrap(),
                        serde_json::to_string(&RunState::Preparing.stage()).unwrap(),
                        run_id.as_str(),
                    ],
                )
                .unwrap();
            connection
                .execute_batch(
                    "DROP TABLE agent_run_start_claims;
                     ALTER TABLE agent_runs DROP COLUMN start_phase_generation;
                     PRAGMA user_version = 3;",
                )
                .unwrap();
        }
        drop(store);

        let migrated = RunStore::open(&paths).unwrap();
        let run = migrated.get_run(&run_id).unwrap().unwrap();
        assert_eq!(run.state, RunState::Preparing);
        assert_eq!(run.current_attempt_id, None);
        assert_eq!(run.execution_evidence, None);
        assert_eq!(run.start_phase_generation, 0);
        let stored_assessment: String = migrated
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT result_json FROM agent_run_execution_evidence
                 WHERE run_id = ?1 AND attempt_id = 'attempt-1'",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<RunEvidenceAssessment>(&stored_assessment).unwrap(),
            assessment
        );

        let replay = migrated
            .start_attempt(&run_id, run.revision, "attempt-1", "dispatch:legacy")
            .unwrap();
        assert!(!replay.newly_created);
        assert_eq!(replay.operation.attempt_id, "attempt-1");
        assert!(matches!(
            migrated.mark_attempt_running(&run_id, replay.run_revision, "attempt-1"),
            Err(CodingAgentError::Conflict(_))
        ));
        assert!(matches!(
            migrated.start_attempt(&run_id, run.revision, "attempt-2", "dispatch:legacy-retry"),
            Err(CodingAgentError::Conflict(_))
        ));
        migrated
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE agent_runs SET state_json = ?1, stage_json = ?2 WHERE run_id = ?3",
                params![
                    serde_json::to_string(&RunState::Publishing).unwrap(),
                    serde_json::to_string(&RunState::Publishing.stage()).unwrap(),
                    run_id.as_str(),
                ],
            )
            .unwrap();
        assert!(matches!(
            migrated.claim_publication_effect(
                &run_id,
                run.revision,
                "attempt-1",
                "publish:legacy-attempt",
                DurableOperationRequest::PublishBranch {
                    branch: "coding-agent/issue-72".into(),
                    commit_id: assessment.commit_id.unwrap(),
                },
            ),
            Err(CodingAgentError::Conflict(_))
        ));
        assert_eq!(
            migrated
                .get_run(&run_id)
                .unwrap()
                .unwrap()
                .current_attempt_id,
            None
        );
        assert_eq!(
            migrated
                .operation(&run_id, "dispatch:legacy")
                .unwrap()
                .unwrap()
                .status,
            DurableOperationStatus::Intent
        );
        assert_eq!(start.phase_generation, 0);
    }

    #[test]
    fn evidence_retention_is_explicit_terminal_only_and_keeps_unresolved_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let (store, run_id) = create_run(temp.path(), "run-retention");
        prepare_first_attempt(&store, &run_id);
        let artifact = store
            .write_evidence(&run_id, "attempt-1", "report", "text/markdown", b"report")
            .unwrap();
        transition_to(&store, &run_id, RunState::Unknown);
        assert!(matches!(
            store.purge_resolved_evidence(&run_id),
            Err(CodingAgentError::Conflict(_))
        ));
        assert_eq!(
            store.read_evidence(&run_id, &artifact.evidence_id).unwrap(),
            b"report"
        );

        transition_to(&store, &run_id, RunState::Failed);
        assert_eq!(store.purge_resolved_evidence(&run_id).unwrap(), 1);
        assert_eq!(
            store.evidence(&run_id).unwrap()[0].evidence_id,
            artifact.evidence_id
        );
        assert!(
            !store
                .verify_evidence(&run_id, &artifact.evidence_id)
                .unwrap()
        );
        assert_eq!(store.purge_resolved_evidence(&run_id).unwrap(), 0);

        let unresolved_temp = tempfile::tempdir().unwrap();
        let (unresolved, unresolved_id) =
            create_run(unresolved_temp.path(), "run-retention-intent");
        transition_to(&unresolved, &unresolved_id, RunState::Queued);
        let preparing = transition_to(&unresolved, &unresolved_id, RunState::Preparing);
        unresolved
            .start_attempt(
                &unresolved_id,
                preparing.revision,
                "attempt-1",
                "dispatch:attempt-1",
            )
            .unwrap();
        let unresolved_artifact = unresolved
            .write_evidence(
                &unresolved_id,
                "attempt-1",
                "report",
                "text/markdown",
                b"unconfirmed",
            )
            .unwrap();
        transition_to(&unresolved, &unresolved_id, RunState::Failed);
        assert!(matches!(
            unresolved.purge_resolved_evidence(&unresolved_id),
            Err(CodingAgentError::Conflict(_))
        ));
        assert_eq!(
            unresolved
                .read_evidence(&unresolved_id, &unresolved_artifact.evidence_id)
                .unwrap(),
            b"unconfirmed"
        );
    }

    #[test]
    fn failed_database_writes_never_publish_completion_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let store = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
        let mut approved = snapshot("run-evidence-db-failure", RunPurpose::Implementation, false);
        approved.publication_plan = PublicationPlan::PushBranch;
        approved.allowed_operations = vec![AllowedOperation::PublishBranch];
        let run_id = store.create_run(approved).unwrap().snapshot.run_id;
        prepare_first_attempt(&store, &run_id);
        mark_current_attempt_running(&store, &run_id);
        let report = store
            .write_evidence(&run_id, "attempt-1", "report", "text/markdown", b"report")
            .unwrap();
        let mut assessment = successful_evidence();
        assessment.report_reference = Some(report.evidence_id);
        assessment.report_save_confirmed = true;
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_execution_assessment
                 BEFORE INSERT ON agent_run_execution_evidence
                 BEGIN SELECT RAISE(ABORT, 'injected durable-write failure'); END;",
            )
            .unwrap();
        assert!(
            store
                .record_execution_evidence(&run_id, "attempt-1", assessment)
                .is_err()
        );
        transition_to(&store, &run_id, RunState::Verifying);
        let revision = store.get_run(&run_id).unwrap().unwrap().revision;
        assert!(matches!(
            store.transition(&run_id, revision, RunState::Publishing),
            Err(CodingAgentError::Conflict(_))
        ));
    }

    #[test]
    fn failed_start_intent_write_does_not_select_an_attempt() {
        let temp = tempfile::tempdir().unwrap();
        let (store, run_id) = create_run(temp.path(), "run-start-transaction");
        transition_to(&store, &run_id, RunState::Queued);
        let preparing = transition_to(&store, &run_id, RunState::Preparing);
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_start_claim_update
                 BEFORE UPDATE OF current_attempt_id ON agent_runs
                 WHEN NEW.current_attempt_id IS NOT NULL
                 BEGIN SELECT RAISE(ABORT, 'injected claim-update failure'); END;",
            )
            .unwrap();

        assert!(
            store
                .start_attempt(&run_id, preparing.revision, "attempt-1", "dispatch:failed")
                .is_err()
        );
        let run = store.get_run(&run_id).unwrap().unwrap();
        assert!(run.current_attempt_id.is_none());
        assert_eq!(run.revision, preparing.revision);
        assert_eq!(run.start_phase_generation, 0);
        assert!(
            store
                .operation(&run_id, "dispatch:failed")
                .unwrap()
                .is_none()
        );
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_start_claim_update;")
            .unwrap();
        let after_rollback = store
            .start_attempt(&run_id, preparing.revision, "attempt-1", "dispatch:failed")
            .unwrap();
        assert!(after_rollback.newly_created);
        assert_eq!(after_rollback.phase_generation, 0);
    }

    #[test]
    fn human_semantic_reassessment_supports_approved_completion_only_when_verified() {
        use crate::coding_agent::assessment::AssessmentUse;
        use crate::coding_agent::coordinator::SemanticReassessmentRecord;
        for (case, usable) in [
            ("pass", true),
            ("auto-disabled", true),
            ("required-not-run", false),
            ("required-failed", false),
            ("missing-publication-receipt", true),
            ("fail", false),
            ("inconclusive", false),
            ("other-head", false),
            ("dirty", false),
            ("other-issue-revision", false),
            ("unconfirmed-operation", false),
            ("report-only-investigation", true),
            ("report-only-other-head", false),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
            let purpose = if case.starts_with("report-only") {
                RunPurpose::Investigation
            } else {
                RunPurpose::Implementation
            };
            let mut approved = snapshot("human-reassessment", purpose, true);
            let base_commit = approved.base_commit.clone();
            if case == "auto-disabled" {
                approved.auto_done = false;
            }
            if case == "missing-publication-receipt" {
                approved.publication_plan = PublicationPlan::PushBranch;
                approved
                    .allowed_operations
                    .push(AllowedOperation::PublishBranch);
            }
            approved.approval = Some(RunApproval {
                approved_by: "human:local-user".into(),
                approved_at_ms: 123,
                auto_done_approved: case != "auto-disabled",
                settings_version: Some(4),
                applicable_modes: vec![ExecutionMode::OneShot],
            });
            let run_id = store.create_run(approved).unwrap().snapshot.run_id;
            prepare_first_attempt(&store, &run_id);
            mark_current_attempt_running(&store, &run_id);
            let report = store
                .write_evidence(&run_id, "attempt-1", "report", "text/markdown", b"report")
                .unwrap();
            let mut evidence = successful_evidence();
            evidence.report_reference = Some(report.evidence_id.clone());
            evidence.report_save_confirmed = true;
            evidence.acceptance_conditions_met = false;
            if case.starts_with("required-") {
                evidence.validation.required = true;
                evidence.validation.state = if case == "required-not-run" {
                    ValidationState::NotRun
                } else {
                    ValidationState::Failed
                };
            }
            if case.starts_with("report-only") {
                evidence.commit_id = None;
            }
            store
                .record_execution_evidence(&run_id, "attempt-1", evidence)
                .unwrap();
            transition_to(&store, &run_id, RunState::Verifying);
            let waiting = transition_to(&store, &run_id, RunState::AwaitingReview);
            assert!(
                store
                    .completion_candidate(&run_id, CompletionActionKind::HumanDone)
                    .is_err()
            );
            let mut assessment =
                crate::coding_agent::semantic_judge::SemanticAssessment::inconclusive(
                    "Independent judgment",
                );
            assessment.outcome = match case {
                "fail" => AssessmentUse::Fail,
                "inconclusive" => AssessmentUse::Inconclusive,
                _ => AssessmentUse::Pass,
            };
            let record = SemanticReassessmentRecord {
                run_id: run_id.to_string(),
                attempt_id: "attempt-1".into(),
                request_key: "human-request".into(),
                expected_run_revision: waiting.revision,
                issue_revision: 7,
                source_sha256: "a".repeat(64),
                source_head_commit: if case == "other-head" || case == "report-only-other-head" {
                    "e".repeat(40)
                } else if case == "report-only-investigation" {
                    base_commit
                } else {
                    "f".repeat(40)
                },
                source_is_clean: case != "dirty",
                report_reference: report.evidence_id.clone(),
                assessment,
            };
            let operation_key = format!(
                "human-semantic:{:x}",
                Sha256::digest(record.request_key.as_bytes())
            );
            let claim = store
                .claim_effect(
                    &run_id,
                    "attempt-1",
                    &operation_key,
                    DurableOperationRequest::ReassessSemantics {
                        expected_run_revision: waiting.revision,
                        issue_revision: if case == "other-issue-revision" { 8 } else { 7 },
                        source_sha256: "a".repeat(64),
                        report_reference: report.evidence_id,
                    },
                )
                .unwrap();
            let result = store
                .write_evidence(
                    &run_id,
                    "attempt-1",
                    "human-semantic-result",
                    "application/json",
                    &serde_json::to_vec(&record).unwrap(),
                )
                .unwrap();
            if case != "unconfirmed-operation" {
                store
                    .complete_operation(
                        &run_id,
                        &claim.operation.idempotency_key,
                        DurableOperationResult {
                            outcome: DurableOperationOutcome::Succeeded,
                            result_digest: Some(result.sha256),
                            evidence_ids: vec![result.evidence_id],
                        },
                    )
                    .unwrap();
            }
            assert_eq!(
                store
                    .completion_candidate(&run_id, CompletionActionKind::HumanDone)
                    .is_ok(),
                usable,
                "{case}"
            );
            assert_eq!(
                store
                    .completion_candidate(&run_id, CompletionActionKind::AutoDone)
                    .is_ok(),
                usable && case != "auto-disabled" && case != "missing-publication-receipt",
                "{case}: approved auto-DONE must require a verified reassessment"
            );
        }
    }

    #[test]
    fn required_validation_can_leave_run_awaiting_review_but_not_done() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let mut approved = snapshot("run-required-validation", RunPurpose::Implementation, true);
        approved.approval = Some(RunApproval {
            approved_by: "human:local-user".into(),
            approved_at_ms: 123,
            auto_done_approved: true,
            settings_version: Some(4),
            applicable_modes: vec![ExecutionMode::OneShot],
        });
        let run_id = store.create_run(approved).unwrap().snapshot.run_id;
        prepare_first_attempt(&store, &run_id);
        mark_current_attempt_running(&store, &run_id);
        let report = store
            .write_evidence(&run_id, "attempt-1", "report", "text/markdown", b"report")
            .unwrap();
        let mut evidence = successful_evidence();
        evidence.report_reference = Some(report.evidence_id);
        evidence.report_save_confirmed = true;
        evidence.validation.required = true;
        evidence.validation.state = ValidationState::NotRun;
        store
            .record_execution_evidence(&run_id, "attempt-1", evidence)
            .unwrap();
        transition_to(&store, &run_id, RunState::Verifying);
        transition_to(&store, &run_id, RunState::AwaitingReview);

        let revision = store.get_run(&run_id).unwrap().unwrap().revision;
        assert!(matches!(
            store.transition(&run_id, revision, RunState::Done),
            Err(CodingAgentError::Conflict(_))
        ));
        assert_eq!(
            store.get_run(&run_id).unwrap().unwrap().state,
            RunState::AwaitingReview
        );
    }

    fn successful_evidence() -> RunEvidenceAssessment {
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
            report_reference: None,
            report_save_confirmed: false,
            evidence_integrity_verified: true,
            commit_id: Some("f".repeat(40)),
            commit_consistent: true,
            acceptance_conditions_met: true,
            validation: ValidationEvidence {
                required: false,
                state: ValidationState::NotRun,
            },
        }
    }

    fn assessment_contract() -> ApprovalAssessmentContract {
        ApprovalAssessmentContract::from_issue_acceptance(
            RunPurpose::Implementation,
            vec!["Implement the approved behavior".into()],
            vec![ApprovalCriterionPolicy {
                required: true,
                method: CriterionEvaluationMethod::SemanticJudge,
            }],
            vec![ApprovalValidationDraft {
                required: true,
                command: None,
            }],
            Some(ProviderSelection {
                provider_id: "ollama".into(),
                model_id: "qwen2.5-coder".into(),
                model_version: "7b".into(),
                config_version: "settings-4".into(),
            }),
            Some("coding-agent-judge-v1".into()),
            AssessmentInputScope {
                allowed_fields: vec![
                    ApprovedJudgeInputField::IssueBody,
                    ApprovedJudgeInputField::Report,
                ],
                allowed_sources: vec![AllowedEvidenceSource {
                    kind: EvidenceSourceKind::Report,
                    identity: "attempt-report".into(),
                    max_bytes: 128 * 1024,
                }],
                max_input_bytes: 32 * 1024,
            },
        )
        .unwrap()
    }

    fn assessment_target_snapshot_digest() -> String {
        super::super::validation::ValidationTargetSnapshot::new(
            "a".repeat(40),
            "b".repeat(40),
            "c".repeat(64),
        )
        .unwrap()
        .digest()
        .into()
    }

    fn approved_snapshot(run_id: &str) -> AgentRunSnapshot {
        let mut snapshot = snapshot(run_id, RunPurpose::Implementation, false);
        snapshot.approval = Some(RunApproval {
            approved_by: "user-1".into(),
            approved_at_ms: 42,
            auto_done_approved: false,
            settings_version: Some(3),
            applicable_modes: vec![ExecutionMode::OneShot],
        });
        snapshot
    }

    fn valid_assessment_report(run_id: &str, attempt_id: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "run_id": run_id,
            "attempt_id": attempt_id,
            "task_kind": "implementation",
            "status": "completed",
            "summary": "The implementation is complete.",
            "acceptance": [{
                "criterion_id": "AC-1",
                "assessment": "unknown",
                "evidence": []
            }],
            "validation": [{
                "validation_id": "V-1",
                "claimed_result": "not_run",
                "evidence": [],
                "reason": "No guest validation was claimed."
            }],
            "investigation": null,
            "implementation": {
                "change_summary": "Implemented the behavior.",
                "changed_files": [],
                "implementation_notes": []
            },
            "unresolved": []
        }))
        .unwrap()
    }

    fn setup_assessment_intent(
        store: &RunStore,
        run_id: &RunId,
    ) -> (EvidenceReference, EvidenceReference, EvidenceReference, u64) {
        setup_assessment_intent_with_report(
            store,
            run_id,
            valid_assessment_report(run_id.as_str(), "attempt-1"),
        )
    }

    fn setup_assessment_intent_with_report(
        store: &RunStore,
        run_id: &RunId,
        report_bytes: Vec<u8>,
    ) -> (EvidenceReference, EvidenceReference, EvidenceReference, u64) {
        prepare_first_attempt(store, run_id);
        mark_current_attempt_running(store, run_id);
        let report = store
            .write_evidence(
                run_id,
                "attempt-1",
                "agent-report",
                "application/json",
                &report_bytes,
            )
            .unwrap();
        let input = store
            .write_evidence(
                run_id,
                "attempt-1",
                "judge-input",
                "application/json",
                br#"{"scope":"approved","body":"exact payload"}"#,
            )
            .unwrap();
        let manifest = store
            .write_evidence(
                run_id,
                "attempt-1",
                "target-snapshot-manifest",
                "application/octet-stream",
                b"opaque host snapshot manifest",
            )
            .unwrap();
        let mut evidence = successful_evidence();
        evidence.report_reference = Some(report.evidence_id.clone());
        evidence.report_save_confirmed = true;
        store
            .record_execution_evidence(run_id, "attempt-1", evidence)
            .unwrap();
        let verifying = transition_to(store, run_id, RunState::Verifying);
        (report, input, manifest, verifying.revision)
    }

    #[test]
    fn approval_contract_is_atomically_snapshotted_and_legacy_runs_stay_incomplete() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let run = store
            .create_run_with_approval_contract(
                approved_snapshot("run-assessment-contract"),
                assessment_contract(),
            )
            .unwrap();
        let run_id = run.snapshot.run_id.clone();

        let status = store.approval_assessment_contract(&run_id).unwrap();
        let stored_digest = match status {
            ApprovalAssessmentContractStatus::Versioned { contract, digest } => {
                assert_eq!(
                    contract.criteria[0].statement,
                    "Implement the approved behavior"
                );
                assert!(contract.validation_plan[0].command.is_none());
                digest
            }
            ApprovalAssessmentContractStatus::LegacyIncomplete => {
                panic!("new approval contract was not stored")
            }
        };

        let snapshot_json: String = store
            .lock_connection()
            .unwrap()
            .query_row(
                "SELECT snapshot_json FROM agent_runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        let encoded: serde_json::Value = serde_json::from_str(&snapshot_json).unwrap();
        assert_eq!(
            encoded["approval_assessment_contract_digest"],
            stored_digest
        );
        assert!(
            encoded["approval_assessment_contract"]
                .get("report_digest")
                .is_none()
        );
        assert!(
            encoded["approval_assessment_contract"]
                .get("artifact_digest")
                .is_none()
        );

        let legacy_id = store
            .create_run(snapshot(
                "run-assessment-legacy",
                RunPurpose::Implementation,
                false,
            ))
            .unwrap()
            .snapshot
            .run_id;
        assert_eq!(
            store.approval_assessment_contract(&legacy_id).unwrap(),
            ApprovalAssessmentContractStatus::LegacyIncomplete
        );
        drop(store);

        let reopened = RunStore::open(&paths).unwrap();
        assert!(matches!(
            reopened.approval_assessment_contract(&run_id).unwrap(),
            ApprovalAssessmentContractStatus::Versioned { digest, .. } if digest == stored_digest
        ));
        assert_eq!(
            reopened.approval_assessment_contract(&legacy_id).unwrap(),
            ApprovalAssessmentContractStatus::LegacyIncomplete
        );
    }

    #[test]
    fn schema_seven_migration_preserves_legacy_evidence_and_history_without_approval() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let run = store
            .create_run(snapshot(
                "run-assessment-schema-seven",
                RunPurpose::Implementation,
                false,
            ))
            .unwrap();
        let run_id = run.snapshot.run_id;
        prepare_first_attempt(&store, &run_id);
        mark_current_attempt_running(&store, &run_id);
        let evidence = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "legacy-output",
                "text/plain",
                b"legacy evidence bytes",
            )
            .unwrap();
        let transitions_before: i64 = store
            .lock_connection()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM agent_run_transitions WHERE run_id = ?1",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        drop(store);

        let legacy_database = Connection::open(paths.coding_agent_runs_db_path()).unwrap();
        legacy_database
            .execute_batch(
                "DROP TABLE agent_run_assessment_intents;
                 PRAGMA user_version = 7;",
            )
            .unwrap();
        drop(legacy_database);

        let migrated = RunStore::open(&paths).unwrap();
        let schema_version: i64 = migrated
            .lock_connection()
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(schema_version, STORE_SCHEMA_VERSION);
        assert_eq!(migrated.evidence(&run_id).unwrap(), vec![evidence.clone()]);
        assert_eq!(
            migrated
                .read_evidence(&run_id, &evidence.evidence_id)
                .unwrap(),
            b"legacy evidence bytes"
        );
        let transitions_after: i64 = migrated
            .lock_connection()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM agent_run_transitions WHERE run_id = ?1",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(transitions_after, transitions_before);
        assert_eq!(
            migrated.approval_assessment_contract(&run_id).unwrap(),
            ApprovalAssessmentContractStatus::LegacyIncomplete
        );
    }

    #[test]
    fn schema_eight_migration_preserves_existing_assessment_intent_without_finalization_link() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let run = store
            .create_run_with_approval_contract(
                approved_snapshot("run-assessment-schema-eight"),
                assessment_contract(),
            )
            .unwrap();
        let run_id = run.snapshot.run_id;
        let (report, input, manifest, revision) = setup_assessment_intent(&store, &run_id);
        let request = AssessmentIntentRequest {
            assessment_id: "assessment-schema-eight".into(),
            judge_input_evidence_id: input.evidence_id,
            report_evidence_id: report.evidence_id,
            snapshot_manifest_evidence_id: manifest.evidence_id,
            target_full_commit_id: "a".repeat(40),
            target_full_tree_id: "b".repeat(40),
            target_untracked_manifest_digest: "c".repeat(64),
            target_snapshot_digest: assessment_target_snapshot_digest(),
            system_prompt_digest: "e".repeat(64),
            validation_receipts: Vec::new(),
        };
        let original = store
            .bind_assessment_intent(&run_id, revision, "attempt-1", request)
            .unwrap()
            .binding;
        drop(store);

        // Recreate the v8 table shape while retaining its pre-existing C1 row.
        let legacy_database = Connection::open(paths.coding_agent_runs_db_path()).unwrap();
        legacy_database
            .execute_batch(
                "CREATE TABLE agent_run_assessment_intents_v8 (
                     run_id TEXT NOT NULL,
                     attempt_id TEXT NOT NULL,
                     assessment_id TEXT NOT NULL,
                     record_json TEXT NOT NULL,
                     created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
                     PRIMARY KEY(run_id, attempt_id),
                     UNIQUE(run_id, assessment_id),
                     FOREIGN KEY(run_id, attempt_id)
                         REFERENCES agent_run_attempts(run_id, attempt_id) ON DELETE RESTRICT
                 );
                 INSERT INTO agent_run_assessment_intents_v8
                     (run_id, attempt_id, assessment_id, record_json, created_at_ms)
                 SELECT run_id, attempt_id, assessment_id, record_json, created_at_ms
                 FROM agent_run_assessment_intents;
                 DROP TABLE agent_run_assessment_intents;
                 ALTER TABLE agent_run_assessment_intents_v8
                     RENAME TO agent_run_assessment_intents;
                 PRAGMA user_version = 8;",
            )
            .unwrap();
        drop(legacy_database);

        let migrated = RunStore::open(&paths).unwrap();
        let schema_version: i64 = migrated
            .lock_connection()
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(schema_version, STORE_SCHEMA_VERSION);
        assert_eq!(
            migrated.assessment_intent(&run_id, "attempt-1").unwrap(),
            original
        );
        let finalization_evidence_id: Option<String> = migrated
            .lock_connection()
            .unwrap()
            .query_row(
                "SELECT finalization_evidence_id FROM agent_run_assessment_intents
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), "attempt-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(finalization_evidence_id, None);
    }

    #[test]
    fn schema_nine_migration_preserves_linked_inputs_and_repairs_attempt_foreign_keys() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let (store, run_id, contract) =
            create_interactive_semantic_run(temp.path(), "run-assessment-schema-nine");
        let (_, _, _, original_intent) =
            setup_linked_finalization_intent(&store, &run_id, &contract);
        let original_inputs = store
            .read_saved_verified_finalization_inputs(&run_id, "attempt-1")
            .unwrap();
        drop(store);

        let legacy_database = Connection::open(paths.coding_agent_runs_db_path()).unwrap();
        legacy_database
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 CREATE TABLE agent_run_finalization_inputs_v9 (
                     run_id TEXT NOT NULL,
                     attempt_id TEXT NOT NULL,
                     evidence_id TEXT NOT NULL,
                     PRIMARY KEY(run_id, attempt_id),
                     FOREIGN KEY(run_id, attempt_id)
                         REFERENCES agent_run_attempts(run_id, attempt_id) ON DELETE RESTRICT,
                     FOREIGN KEY(run_id, evidence_id)
                         REFERENCES agent_run_evidence(run_id, evidence_id) ON DELETE RESTRICT
                 );
                 INSERT INTO agent_run_finalization_inputs_v9
                     (run_id, attempt_id, evidence_id)
                 SELECT run_id, attempt_id, evidence_id FROM agent_run_finalization_inputs;
                 DROP TABLE agent_run_finalization_inputs;
                 ALTER TABLE agent_run_finalization_inputs_v9
                     RENAME TO agent_run_finalization_inputs;
                 CREATE TABLE agent_run_assessment_intents_v9 (
                     run_id TEXT NOT NULL,
                     attempt_id TEXT NOT NULL,
                     assessment_id TEXT NOT NULL,
                     record_json TEXT NOT NULL,
                     created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
                     finalization_evidence_id TEXT,
                     PRIMARY KEY(run_id, attempt_id),
                     UNIQUE(run_id, assessment_id),
                     FOREIGN KEY(run_id, attempt_id)
                         REFERENCES agent_run_attempts(run_id, attempt_id) ON DELETE RESTRICT,
                     FOREIGN KEY(run_id, finalization_evidence_id)
                         REFERENCES agent_run_evidence(run_id, evidence_id) ON DELETE RESTRICT
                 );
                 INSERT INTO agent_run_assessment_intents_v9
                     (run_id, attempt_id, assessment_id, record_json, created_at_ms,
                      finalization_evidence_id)
                 SELECT run_id, attempt_id, assessment_id, record_json, created_at_ms,
                        finalization_evidence_id
                 FROM agent_run_assessment_intents;
                 DROP TABLE agent_run_assessment_intents;
                 ALTER TABLE agent_run_assessment_intents_v9
                     RENAME TO agent_run_assessment_intents;
                 PRAGMA user_version = 9;",
            )
            .unwrap();
        drop(legacy_database);

        let migrated = RunStore::open(&paths).unwrap();
        let connection = migrated.lock_connection().unwrap();
        let schema_version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(schema_version, STORE_SCHEMA_VERSION);
        let foreign_key_violations: i64 = connection
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(foreign_key_violations, 0);
        drop(connection);
        assert_eq!(
            migrated.assessment_intent(&run_id, "attempt-1").unwrap(),
            original_intent
        );
        assert_eq!(
            migrated
                .read_saved_verified_finalization_inputs(&run_id, "attempt-1")
                .unwrap(),
            original_inputs
        );
        let connection = migrated.lock_connection().unwrap();
        assert_attempt_scoped_finalization_foreign_keys(
            &connection,
            &run_id,
            &original_inputs.index_reference().evidence_id,
        );
    }

    #[test]
    fn approval_contract_rejects_a_run_mode_outside_the_explicit_approval() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let mut snapshot = approved_snapshot("run-assessment-mode-mismatch");
        snapshot.execution_mode = ExecutionMode::Interactive;
        let run_id = snapshot.run_id.clone();

        assert!(matches!(
            store.create_run_with_approval_contract(snapshot, assessment_contract()),
            Err(CodingAgentError::Model(RunModelError::Invalid(_)))
        ));
        assert!(store.get_run(&run_id).unwrap().is_none());
    }

    #[test]
    fn assessment_intent_is_report_validated_attempt_scoped_and_compare_and_swap_immutable() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let run = store
            .create_run_with_approval_contract(
                approved_snapshot("run-assessment-intent"),
                assessment_contract(),
            )
            .unwrap();
        let run_id = run.snapshot.run_id;
        let (report, input, manifest, revision) = setup_assessment_intent(&store, &run_id);
        let request = AssessmentIntentRequest {
            assessment_id: "assessment-1".into(),
            judge_input_evidence_id: input.evidence_id.clone(),
            report_evidence_id: report.evidence_id.clone(),
            snapshot_manifest_evidence_id: manifest.evidence_id.clone(),
            target_full_commit_id: "a".repeat(40),
            target_full_tree_id: "b".repeat(40),
            target_untracked_manifest_digest: "c".repeat(64),
            target_snapshot_digest: assessment_target_snapshot_digest(),
            system_prompt_digest: "e".repeat(64),
            validation_receipts: Vec::new(),
        };

        assert!(matches!(
            store.bind_assessment_intent(&run_id, revision - 1, "attempt-1", request.clone()),
            Err(CodingAgentError::Conflict(_))
        ));
        assert!(matches!(
            store.assessment_intent(&run_id, "attempt-1"),
            Err(CodingAgentError::NotFound(_))
        ));
        assert!(matches!(
            store.bind_assessment_intent(&run_id, revision, "other-attempt", request.clone()),
            Err(CodingAgentError::Conflict(_))
        ));

        let claim = store
            .bind_assessment_intent(&run_id, revision, "attempt-1", request.clone())
            .unwrap();
        assert!(claim.newly_created);
        let intent = claim.binding;
        assert_eq!(intent.attempt_id, "attempt-1");
        assert_eq!(
            intent.approval_contract_digest,
            assessment_contract().digest().unwrap()
        );
        assert_eq!(intent.judge_input_evidence_id, input.evidence_id);
        assert_eq!(intent.judge_input_digest, input.sha256);
        assert_eq!(intent.report_digest, report.sha256);
        assert_eq!(intent.snapshot_manifest_digest, manifest.sha256);
        assert_eq!(intent.target_full_commit_id, "a".repeat(40));
        assert_eq!(intent.target_full_tree_id, "b".repeat(40));
        assert_eq!(intent.target_untracked_manifest_digest, "c".repeat(64));
        assert_eq!(intent.system_prompt_digest, "e".repeat(64));
        let repeated = store
            .bind_assessment_intent(&run_id, revision, "attempt-1", request.clone())
            .unwrap();
        assert!(!repeated.newly_created);
        assert_eq!(repeated.binding, intent);

        let alternate_input = store
            .write_evidence(
                &run_id,
                "attempt-1",
                "judge-input",
                "application/json",
                br#"{"scope":"approved","body":"changed payload"}"#,
            )
            .unwrap();
        let mut changed = request.clone();
        changed.judge_input_evidence_id = alternate_input.evidence_id;
        assert!(matches!(
            store.bind_assessment_intent(&run_id, revision, "attempt-1", changed),
            Err(CodingAgentError::Conflict(_))
        ));

        drop(store);
        let reopened = RunStore::open(&paths).unwrap();
        assert_eq!(
            reopened.assessment_intent(&run_id, "attempt-1").unwrap(),
            intent
        );
    }

    #[test]
    fn concurrent_intent_claims_across_store_handles_create_at_most_one_intent() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let run = store
            .create_run_with_approval_contract(
                approved_snapshot("run-assessment-concurrent-intent"),
                assessment_contract(),
            )
            .unwrap();
        let run_id = run.snapshot.run_id;
        let (report, input, manifest, revision) = setup_assessment_intent(&store, &run_id);
        let request = AssessmentIntentRequest {
            assessment_id: "assessment-concurrent-a".into(),
            judge_input_evidence_id: input.evidence_id,
            report_evidence_id: report.evidence_id,
            snapshot_manifest_evidence_id: manifest.evidence_id,
            target_full_commit_id: "a".repeat(40),
            target_full_tree_id: "b".repeat(40),
            target_untracked_manifest_digest: "c".repeat(64),
            target_snapshot_digest: assessment_target_snapshot_digest(),
            system_prompt_digest: "e".repeat(64),
            validation_receipts: Vec::new(),
        };
        let mut competing_request = request.clone();
        competing_request.assessment_id = "assessment-concurrent-b".into();

        let barrier = Arc::new(Barrier::new(3));
        let first_paths = paths.clone();
        let first_run = run_id.clone();
        let first_barrier = barrier.clone();
        let first = std::thread::spawn(move || {
            let store = RunStore::open(&first_paths).unwrap();
            first_barrier.wait();
            store.bind_assessment_intent(&first_run, revision, "attempt-1", request)
        });
        let second_paths = paths.clone();
        let second_run = run_id.clone();
        let second_barrier = barrier.clone();
        let second = std::thread::spawn(move || {
            let store = RunStore::open(&second_paths).unwrap();
            second_barrier.wait();
            store.bind_assessment_intent(&second_run, revision, "attempt-1", competing_request)
        });
        barrier.wait();

        let results = [first.join().unwrap(), second.join().unwrap()];
        let created = results
            .iter()
            .filter_map(|result| result.as_ref().ok())
            .filter(|claim| claim.newly_created)
            .count();
        let conflicts = results
            .iter()
            .filter(|result| matches!(result, Err(CodingAgentError::Conflict(_))))
            .count();
        assert_eq!(created, 1);
        assert_eq!(conflicts, 1);

        let persisted = store.assessment_intent(&run_id, "attempt-1").unwrap();
        let retry = store
            .bind_assessment_intent(
                &run_id,
                revision,
                "attempt-1",
                AssessmentIntentRequest {
                    assessment_id: persisted.assessment_id.clone(),
                    judge_input_evidence_id: persisted.judge_input_evidence_id.clone(),
                    report_evidence_id: persisted.report_evidence_id.clone(),
                    snapshot_manifest_evidence_id: persisted.snapshot_manifest_evidence_id.clone(),
                    target_full_commit_id: persisted.target_full_commit_id.clone(),
                    target_full_tree_id: persisted.target_full_tree_id.clone(),
                    target_untracked_manifest_digest: persisted
                        .target_untracked_manifest_digest
                        .clone(),
                    target_snapshot_digest: persisted.target_snapshot_digest.clone(),
                    system_prompt_digest: persisted.system_prompt_digest.clone(),
                    validation_receipts: Vec::new(),
                },
            )
            .unwrap();
        assert!(!retry.newly_created);
        assert_eq!(retry.binding, persisted);
    }

    #[test]
    fn assessment_intent_rejects_a_valid_but_unapproved_scope_digest() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let run = store
            .create_run_with_approval_contract(
                approved_snapshot("run-assessment-corrupt-scope"),
                assessment_contract(),
            )
            .unwrap();
        let run_id = run.snapshot.run_id;
        let (report, input, manifest, revision) = setup_assessment_intent(&store, &run_id);
        let request = AssessmentIntentRequest {
            assessment_id: "assessment-corrupt-scope".into(),
            judge_input_evidence_id: input.evidence_id,
            report_evidence_id: report.evidence_id,
            snapshot_manifest_evidence_id: manifest.evidence_id,
            target_full_commit_id: "a".repeat(40),
            target_full_tree_id: "b".repeat(40),
            target_untracked_manifest_digest: "c".repeat(64),
            target_snapshot_digest: assessment_target_snapshot_digest(),
            system_prompt_digest: "e".repeat(64),
            validation_receipts: Vec::new(),
        };
        let created = store
            .bind_assessment_intent(&run_id, revision, "attempt-1", request.clone())
            .unwrap();
        assert!(created.newly_created);

        let forged_scope_digest = "0".repeat(64);
        assert_ne!(
            forged_scope_digest,
            assessment_contract().judge_input_scope_digest()
        );
        store
            .lock_connection()
            .unwrap()
            .execute(
                "UPDATE agent_run_assessment_intents
                 SET record_json = json_set(record_json, '$.judge_input_scope_digest', ?1)
                 WHERE run_id = ?2 AND attempt_id = ?3",
                params![forged_scope_digest, run_id.as_str(), "attempt-1"],
            )
            .unwrap();

        assert!(matches!(
            store.assessment_intent(&run_id, "attempt-1"),
            Err(CodingAgentError::Corrupt(_))
        ));
        assert!(matches!(
            store.bind_assessment_intent(&run_id, revision, "attempt-1", request),
            Err(CodingAgentError::Corrupt(_))
        ));
        let raw_scope_digest: String = store
            .lock_connection()
            .unwrap()
            .query_row(
                "SELECT json_extract(record_json, '$.judge_input_scope_digest')
                 FROM agent_run_assessment_intents
                 WHERE run_id = ?1 AND attempt_id = ?2",
                params![run_id.as_str(), "attempt-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw_scope_digest, forged_scope_digest);

        drop(store);
        let reopened = RunStore::open(&paths).unwrap();
        assert!(matches!(
            reopened.assessment_intent(&run_id, "attempt-1"),
            Err(CodingAgentError::Corrupt(_))
        ));
        assert_eq!(
            reopened.approval_assessment_contract(&run_id).unwrap(),
            ApprovalAssessmentContractStatus::Versioned {
                contract: Box::new(assessment_contract()),
                digest: assessment_contract().digest().unwrap(),
            }
        );
    }

    #[test]
    fn assessment_intent_rejects_report_ids_not_in_the_approval_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let run = store
            .create_run_with_approval_contract(
                approved_snapshot("run-assessment-unknown-report"),
                assessment_contract(),
            )
            .unwrap();
        let run_id = run.snapshot.run_id;
        let mut report = serde_json::from_slice::<serde_json::Value>(&valid_assessment_report(
            run_id.as_str(),
            "attempt-1",
        ))
        .unwrap();
        report["acceptance"][0]["criterion_id"] = "AC-unknown".into();
        let (report, input, manifest, revision) = setup_assessment_intent_with_report(
            &store,
            &run_id,
            serde_json::to_vec(&report).unwrap(),
        );
        let request = AssessmentIntentRequest {
            assessment_id: "assessment-invalid-report".into(),
            judge_input_evidence_id: input.evidence_id,
            report_evidence_id: report.evidence_id,
            snapshot_manifest_evidence_id: manifest.evidence_id,
            target_full_commit_id: "a".repeat(40),
            target_full_tree_id: "b".repeat(40),
            target_untracked_manifest_digest: "c".repeat(64),
            target_snapshot_digest: assessment_target_snapshot_digest(),
            system_prompt_digest: "e".repeat(64),
            validation_receipts: Vec::new(),
        };

        assert!(matches!(
            store.bind_assessment_intent(&run_id, revision, "attempt-1", request),
            Err(CodingAgentError::Conflict(_))
        ));
        assert!(matches!(
            store.assessment_intent(&run_id, "attempt-1"),
            Err(CodingAgentError::NotFound(_))
        ));
    }

    #[test]
    fn approval_snapshot_rejects_present_null_or_corrupt_contract_instead_of_defaulting() {
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let store = RunStore::open(&paths).unwrap();
        let run = store
            .create_run_with_approval_contract(
                approved_snapshot("run-assessment-null"),
                assessment_contract(),
            )
            .unwrap();
        let run_id = run.snapshot.run_id;
        let original_snapshot_json: String = store
            .lock_connection()
            .unwrap()
            .query_row(
                "SELECT snapshot_json FROM agent_runs WHERE run_id = ?1",
                [run_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        store
            .lock_connection()
            .unwrap()
            .execute(
                "UPDATE agent_runs
                 SET snapshot_json = json_set(snapshot_json, '$.approval_assessment_contract', NULL)
                 WHERE run_id = ?1",
                [run_id.as_str()],
            )
            .unwrap();

        assert!(matches!(
            store.approval_assessment_contract(&run_id),
            Err(CodingAgentError::Corrupt(_))
        ));

        store
            .lock_connection()
            .unwrap()
            .execute(
                "UPDATE agent_runs
                 SET snapshot_json = json_set(?1, '$.approval_assessment_contract_digest', ?2)
                 WHERE run_id = ?3",
                params![original_snapshot_json, "0".repeat(64), run_id.as_str()],
            )
            .unwrap();
        assert!(matches!(
            store.approval_assessment_contract(&run_id),
            Err(CodingAgentError::Corrupt(_))
        ));
    }
    #[test]
    fn delegation_plans_are_durable_idempotent_proposals_without_execution() {
        use super::super::delegation::DelegationPlanRequest;
        let temp = tempfile::tempdir().unwrap();
        let paths = DataPaths::with_root(temp.path());
        let request = DelegationPlanRequest {
            issue_id: "local-issue-72".into(),
            profile_id: "opencode-default".into(),
            purpose: RunPurpose::Implementation,
            prompt: Some("Please resolve the Issue".into()),
        };
        let store = RunStore::open(&paths).unwrap();
        let plan = store
            .create_delegation_plan("chat:one", request.clone(), 4, 3)
            .unwrap();
        assert_eq!(plan.run_id, None);
        assert!(store.recoverable_runs().unwrap().is_empty());
        assert_eq!(
            store
                .create_delegation_plan("chat:one", request.clone(), 4, 3)
                .unwrap(),
            plan
        );
        let mut changed = request.clone();
        changed.purpose = RunPurpose::Investigation;
        assert!(matches!(
            store.create_delegation_plan("chat:one", changed, 4, 3),
            Err(CodingAgentError::Conflict(_))
        ));
        drop(store);
        let reopened = RunStore::open(&paths).unwrap();
        assert_eq!(
            reopened
                .create_delegation_plan("chat:one", request, 5, 4)
                .unwrap(),
            plan
        );
        assert_eq!(
            reopened.delegation_plans("local-issue-72").unwrap(),
            vec![plan.clone()]
        );
        assert!(
            reopened
                .delegation_plans("another-issue")
                .unwrap()
                .is_empty()
        );
        assert!(
            reopened
                .delegation_plan(&plan.delegation_plan_id, "another-issue")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn delegation_plans_link_only_an_approved_run_with_matching_scope() {
        use super::super::delegation::DelegationPlanRequest;
        let temp = tempfile::tempdir().unwrap();
        let store = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
        let request = DelegationPlanRequest {
            issue_id: "local-issue-72".into(),
            profile_id: "opencode-default".into(),
            purpose: RunPurpose::Implementation,
            prompt: None,
        };
        let plan = store
            .create_delegation_plan("mcp:one", request, 4, 3)
            .unwrap();
        let pending = store
            .create_run(approved_snapshot("delegated-run"))
            .unwrap();
        assert!(
            store
                .link_delegation_plan(&plan.delegation_plan_id, &pending.snapshot.run_id)
                .is_err()
        );
        store
            .transition(&pending.snapshot.run_id, pending.revision, RunState::Queued)
            .unwrap();
        for field in ["issue", "profile", "purpose", "approval"] {
            let mut snapshot = approved_snapshot(&format!("wrong-{field}"));
            match field {
                "issue" => snapshot.issue_id = "another-issue".into(),
                "profile" => snapshot.profile.profile_id = "other-profile".into(),
                "purpose" => snapshot.purpose = RunPurpose::Investigation,
                "approval" => snapshot.approval = None,
                _ => unreachable!(),
            }
            let run = store.create_run(snapshot).unwrap();
            if field != "approval" {
                store
                    .transition(&run.snapshot.run_id, run.revision, RunState::Queued)
                    .unwrap();
            }
            assert!(
                store
                    .link_delegation_plan(&plan.delegation_plan_id, &run.snapshot.run_id)
                    .is_err(),
                "{field}"
            );
        }
        let linked = store
            .link_delegation_plan(&plan.delegation_plan_id, &pending.snapshot.run_id)
            .unwrap();
        assert_eq!(linked.run_id.as_ref(), Some(&pending.snapshot.run_id));
        let status = super::super::delegation::read_delegation_status(
            &store,
            &super::super::delegation::DelegationStatusRequest {
                issue_id: "local-issue-72".into(),
                delegation_plan_id: Some(plan.delegation_plan_id.clone()),
                run_id: None,
            },
        )
        .unwrap();
        assert_eq!(status.run_id.as_ref(), Some(&pending.snapshot.run_id));
        assert_eq!(status.state, Some(RunState::Queued));
        assert!(status.report_references.is_empty());
        assert!(
            super::super::delegation::read_delegation_status(
                &store,
                &super::super::delegation::DelegationStatusRequest {
                    issue_id: "another-issue".into(),
                    delegation_plan_id: Some(plan.delegation_plan_id.clone()),
                    run_id: None,
                }
            )
            .is_err()
        );

        assert_eq!(
            store
                .link_delegation_plan(&plan.delegation_plan_id, &pending.snapshot.run_id)
                .unwrap(),
            linked
        );
        let another = store
            .create_run(approved_snapshot("different-run"))
            .unwrap();
        store
            .transition(&another.snapshot.run_id, another.revision, RunState::Queued)
            .unwrap();
        assert!(
            store
                .link_delegation_plan(&plan.delegation_plan_id, &another.snapshot.run_id)
                .is_err()
        );
    }

    #[test]
    fn delegation_request_rejects_execution_authority_and_unbounded_inputs() {
        use super::super::delegation::DelegationPlanRequest;
        let input = serde_json::json!({"issueId":"issue", "profileId":"profile", "purpose":"implementation"});
        let request: DelegationPlanRequest = serde_json::from_value(input.clone()).unwrap();
        assert!(request.validate().is_ok());
        for field in [
            "command",
            "mounts",
            "approvedBy",
            "userConfirmed",
            "autoDone",
            "executionMode",
        ] {
            let mut changed = input.clone();
            changed[field] = serde_json::json!(true);
            assert!(
                serde_json::from_value::<DelegationPlanRequest>(changed).is_err(),
                "{field}"
            );
        }
        for field in ["issue", "profile", "prompt"] {
            let mut changed = request.clone();
            match field {
                "issue" => changed.issue_id.clear(),
                "profile" => changed.profile_id = "\n".into(),
                "prompt" => changed.prompt = Some("x".repeat(48 * 1024 + 1)),
                _ => unreachable!(),
            }
            assert!(changed.validate().is_err(), "{field}");
        }
    }

    #[test]
    fn delegation_pending_listing_survives_many_linked_plans_and_conflicts_preserve_queued_runs() {
        use super::super::delegation::DelegationPlanRequest;
        let temp = tempfile::tempdir().unwrap();
        let store = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
        let request = DelegationPlanRequest {
            issue_id: "local-issue-72".into(),
            profile_id: "opencode-default".into(),
            purpose: RunPurpose::Implementation,
            prompt: None,
        };
        let pending = store
            .create_delegation_plan("pending", request.clone(), 4, 3)
            .unwrap();
        let run = store.create_run(approved_snapshot("linked-run")).unwrap();
        store
            .transition(&run.snapshot.run_id, run.revision, RunState::Queued)
            .unwrap();
        let mut last = None;
        for index in 0..101 {
            let plan = store
                .create_delegation_plan(&format!("linked:{index}"), request.clone(), 4, 3)
                .unwrap();
            store
                .link_delegation_plan(&plan.delegation_plan_id, &run.snapshot.run_id)
                .unwrap();
            last = Some(plan);
        }
        assert_eq!(
            store.delegation_plans("local-issue-72").unwrap(),
            vec![pending]
        );
        let last = last.unwrap();
        let other = store.create_run(approved_snapshot("queued-other")).unwrap();
        store
            .transition(&other.snapshot.run_id, other.revision, RunState::Queued)
            .unwrap();
        assert!(
            crate::commands::coding_agent::link_delegation_for_start(
                &store,
                &last.delegation_plan_id,
                &other.snapshot.run_id
            )
            .is_err()
        );
        assert_eq!(
            store
                .get_run(&other.snapshot.run_id)
                .unwrap()
                .unwrap()
                .state,
            RunState::Queued
        );
        assert_eq!(
            store.get_run(&run.snapshot.run_id).unwrap().unwrap().state,
            RunState::Queued
        );
    }

    #[test]
    fn delegation_status_exposes_only_verified_current_reports_without_mutating_the_run() {
        use super::super::delegation::{DelegationStatusRequest, read_delegation_status};
        let temp = tempfile::tempdir().unwrap();
        let store = RunStore::open(&DataPaths::with_root(temp.path())).unwrap();
        let mut input = snapshot("status-report-run", RunPurpose::Implementation, false);
        input.attempt_ids.push("attempt-2".into());
        let run = store.create_run(input).unwrap();
        let id = &run.snapshot.run_id;
        prepare_first_attempt(&store, id);
        store
            .write_evidence(id, "attempt-1", "agent-report", "application/json", b"{}")
            .unwrap();
        mark_current_attempt_running(&store, id);
        let failed = transition_to(&store, id, RunState::Failed);
        let preparing = store
            .begin_retry_preparation(id, failed.revision, "attempt-1")
            .unwrap();
        store
            .start_attempt(id, preparing.revision, "attempt-2", "dispatch:attempt-2")
            .unwrap();
        let valid = store
            .write_evidence(id, "attempt-2", "agent-report", "application/json", b"{}")
            .unwrap();
        let corrupted = store
            .write_evidence(
                id,
                "attempt-2",
                "issue-result-report",
                "application/json",
                b"{\"report\":1}",
            )
            .unwrap();
        fs::write(store.evidence_path(id, &corrupted.evidence_id), b"tampered").unwrap();
        for name in ["agent-report-raw", "terminal-output"] {
            store
                .write_evidence(id, "attempt-2", name, "application/json", b"{}")
                .unwrap();
        }
        let before = store.get_run(id).unwrap().unwrap();
        let request = DelegationStatusRequest {
            issue_id: "local-issue-72".into(),
            delegation_plan_id: None,
            run_id: Some(id.clone()),
        };
        let status = read_delegation_status(&store, &request).unwrap();
        assert_eq!(status.state, Some(before.state));
        assert_eq!(status.report_references, vec![valid]);
        assert_eq!(
            store.get_run(id).unwrap().unwrap().revision,
            before.revision
        );
        assert!(
            read_delegation_status(
                &store,
                &DelegationStatusRequest {
                    issue_id: "other".into(),
                    ..request
                }
            )
            .is_err()
        );
    }
}
