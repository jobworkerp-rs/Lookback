use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::jobworkerp::{JobworkerpHandle, coding_agent::CodingAgentEvent};

const MAX_VALIDATION_IDENTIFIER_BYTES: usize = 256;
const MAX_ARGV_ITEMS: usize = 128;
const MAX_ARG_BYTES: usize = 4096;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_ENV_ITEMS: usize = 128;
const MAX_ENV_VALUE_BYTES: usize = 16 * 1024;
const MAX_SANDBOX_ID_BYTES: usize = 256;
const MAX_OUTPUT_REFERENCE_BYTES: usize = 4096;
const VALIDATION_RECEIPT_SCHEMA_VERSION: u32 = 1;
const VALIDATION_RECORD_SCHEMA_VERSION: u32 = 1;
pub(super) const MAX_VALIDATION_RECORD_BYTES: usize = 128 * 1024;

/// Maximum combined stdout and stderr held in memory while a validation runs.
pub const MAX_VALIDATION_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// A validated, immutable description of the SANDBOX job approved by the host.
///
/// Environment values are intentionally omitted from `Debug` output and from
/// receipts. Their values are included in `command_definition_digest`.
#[derive(Clone)]
pub struct ApprovedValidationPlan {
    run_id: String,
    attempt_id: String,
    validation_id: String,
    required: bool,
    worker_name: String,
    expected_sandbox_id: Option<String>,
    argv: Vec<String>,
    working_directory: String,
    environment: BTreeMap<String, String>,
    permission_profile_digest: String,
    timeout_ms: Option<u64>,
    target: ValidationTargetSnapshot,
    command_definition_digest: String,
    binding_digest: String,
}

impl fmt::Debug for ApprovedValidationPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApprovedValidationPlan")
            .field("run_id", &self.run_id)
            .field("attempt_id", &self.attempt_id)
            .field("validation_id", &self.validation_id)
            .field("required", &self.required)
            .field("worker_name", &self.worker_name)
            .field("expected_sandbox_id", &self.expected_sandbox_id)
            .field("program", &self.argv.first())
            .field("argument_count", &self.argv.len().saturating_sub(1))
            .field("working_directory", &self.working_directory)
            .field(
                "environment_keys",
                &self.environment.keys().collect::<Vec<_>>(),
            )
            .field("permission_profile_digest", &self.permission_profile_digest)
            .field("timeout_ms", &self.timeout_ms)
            .field("target_snapshot_digest", &self.target.digest)
            .field("command_definition_digest", &self.command_definition_digest)
            .finish_non_exhaustive()
    }
}

impl ApprovedValidationPlan {
    /// Build from host approval data, not fields parsed from an agent report.
    /// `permission_profile_digest` identifies the effective worker permission
    /// profile and `target` identifies the complete pinned source snapshot.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run_id: impl Into<String>,
        attempt_id: impl Into<String>,
        validation_id: impl Into<String>,
        required: bool,
        worker_name: impl Into<String>,
        expected_sandbox_id: impl Into<String>,
        argv: Vec<String>,
        working_directory: impl Into<String>,
        environment: BTreeMap<String, String>,
        permission_profile_digest: impl Into<String>,
        timeout_ms: Option<u64>,
        target: ValidationTargetSnapshot,
    ) -> Result<Self, ValidationPlanError> {
        Self::new_with_sandbox_expectation(
            run_id.into(),
            attempt_id.into(),
            validation_id.into(),
            required,
            worker_name.into(),
            Some(expected_sandbox_id.into()),
            argv,
            working_directory.into(),
            environment,
            permission_profile_digest.into(),
            timeout_ms,
            target,
        )
    }

    /// Build a validation plan when the static-pool VM identity is learned only
    /// from the actual SANDBOX exit event.
    #[allow(clippy::too_many_arguments)]
    pub fn new_for_worker(
        run_id: impl Into<String>,
        attempt_id: impl Into<String>,
        validation_id: impl Into<String>,
        required: bool,
        worker_name: impl Into<String>,
        argv: Vec<String>,
        working_directory: impl Into<String>,
        environment: BTreeMap<String, String>,
        permission_profile_digest: impl Into<String>,
        timeout_ms: Option<u64>,
        target: ValidationTargetSnapshot,
    ) -> Result<Self, ValidationPlanError> {
        Self::new_with_sandbox_expectation(
            run_id.into(),
            attempt_id.into(),
            validation_id.into(),
            required,
            worker_name.into(),
            None,
            argv,
            working_directory.into(),
            environment,
            permission_profile_digest.into(),
            timeout_ms,
            target,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_sandbox_expectation(
        run_id: String,
        attempt_id: String,
        validation_id: String,
        required: bool,
        worker_name: String,
        expected_sandbox_id: Option<String>,
        argv: Vec<String>,
        working_directory: String,
        environment: BTreeMap<String, String>,
        permission_profile_digest: String,
        timeout_ms: Option<u64>,
        target: ValidationTargetSnapshot,
    ) -> Result<Self, ValidationPlanError> {
        let permission_profile_digest = normalize_sha256(permission_profile_digest)?;

        if !valid_identifier(&run_id)
            || !valid_identifier(&attempt_id)
            || !valid_identifier(&validation_id)
            || !valid_identifier(&worker_name)
            || expected_sandbox_id
                .as_deref()
                .is_some_and(|sandbox_id| !valid_sandbox_id(sandbox_id))
            || !valid_text(&working_directory, MAX_ARG_BYTES)
            || !working_directory.starts_with('/')
            || argv.is_empty()
            || argv.len() > MAX_ARGV_ITEMS
            || argv.iter().any(|arg| !valid_text(arg, MAX_ARG_BYTES))
            || argv[0].is_empty()
            || argv.iter().map(String::len).sum::<usize>() > MAX_COMMAND_BYTES
            || environment.len() > MAX_ENV_ITEMS
            || environment.iter().any(|(key, value)| {
                !valid_environment_key(key) || !valid_text(value, MAX_ENV_VALUE_BYTES)
            })
            || timeout_ms == Some(0)
        {
            return Err(ValidationPlanError::InvalidPlan);
        }

        let command_definition_digest = digest_serializable(&CommandDefinition {
            worker_name: &worker_name,
            argv: &argv,
            working_directory: &working_directory,
            environment: &environment,
            permission_profile_digest: &permission_profile_digest,
            timeout_ms,
        });
        let binding_digest = digest_serializable(&PlanBinding {
            run_id: &run_id,
            attempt_id: &attempt_id,
            validation_id: &validation_id,
            required,
            expected_sandbox_id: expected_sandbox_id.as_deref(),
            command_definition_digest: &command_definition_digest,
            target_snapshot_digest: &target.digest,
        });

        Ok(Self {
            run_id,
            attempt_id,
            validation_id,
            required,
            worker_name,
            expected_sandbox_id,
            argv,
            working_directory,
            environment,
            permission_profile_digest,
            timeout_ms,
            target,
            command_definition_digest,
            binding_digest,
        })
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    pub fn validation_id(&self) -> &str {
        &self.validation_id
    }

    pub fn required(&self) -> bool {
        self.required
    }

    pub fn worker_name(&self) -> &str {
        &self.worker_name
    }

    pub fn expected_sandbox_id(&self) -> Option<&str> {
        self.expected_sandbox_id.as_deref()
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn working_directory(&self) -> &str {
        &self.working_directory
    }

    pub fn permission_profile_digest(&self) -> &str {
        &self.permission_profile_digest
    }

    pub fn command_definition_digest(&self) -> &str {
        &self.command_definition_digest
    }

    pub fn binding_digest(&self) -> &str {
        &self.binding_digest
    }

    pub fn target(&self) -> &ValidationTargetSnapshot {
        &self.target
    }

    fn sandbox_arguments(&self) -> serde_json::Value {
        let mut arguments = serde_json::Map::new();
        arguments.insert("command".to_owned(), serde_json::json!(self.argv[0]));
        arguments.insert("args".to_owned(), serde_json::json!(&self.argv[1..]));
        arguments.insert(
            "working_dir".to_owned(),
            serde_json::json!(self.working_directory),
        );
        arguments.insert("env".to_owned(), serde_json::json!(self.environment));
        // SANDBOX must report non-zero process exits so the host can classify them.
        arguments.insert(
            "treat_nonzero_as_error".to_owned(),
            serde_json::json!(false),
        );
        if let Some(timeout_ms) = self.timeout_ms {
            arguments.insert("timeout_ms".to_owned(), serde_json::json!(timeout_ms));
        }
        serde_json::Value::Object(arguments)
    }
}

/// Host-owned digest binding a validation to the complete pinned source target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationTargetSnapshot {
    full_commit_id: String,
    full_tree_id: String,
    untracked_manifest_digest: String,
    digest: String,
}

impl ValidationTargetSnapshot {
    /// `untracked_manifest_digest` must cover the complete in-scope untracked-file manifest.
    pub fn new(
        full_commit_id: impl Into<String>,
        full_tree_id: impl Into<String>,
        untracked_manifest_digest: impl Into<String>,
    ) -> Result<Self, ValidationPlanError> {
        let full_commit_id = normalize_git_id(full_commit_id.into())?;
        let full_tree_id = normalize_git_id(full_tree_id.into())?;
        let untracked_manifest_digest = normalize_sha256(untracked_manifest_digest.into())?;
        if full_commit_id.len() != full_tree_id.len() {
            return Err(ValidationPlanError::InvalidPlan);
        }
        let digest = digest_serializable(&TargetBinding {
            full_commit_id: &full_commit_id,
            full_tree_id: &full_tree_id,
            untracked_manifest_digest: &untracked_manifest_digest,
        });
        Ok(Self {
            full_commit_id,
            full_tree_id,
            untracked_manifest_digest,
            digest,
        })
    }

    pub fn full_commit_id(&self) -> &str {
        &self.full_commit_id
    }

    pub fn full_tree_id(&self) -> &str {
        &self.full_tree_id
    }

    pub fn untracked_manifest_digest(&self) -> &str {
        &self.untracked_manifest_digest
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationOutcome {
    Passed,
    Failed,
    Unknown,
    NotRun,
}

/// A validation result's output is stored outside agent-controlled logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationOutputEvidence {
    reference: String,
    stdout_sha256: String,
    stderr_sha256: String,
    stdout_bytes: usize,
    stderr_bytes: usize,
    capture_complete: bool,
}

impl ValidationOutputEvidence {
    pub fn reference(&self) -> &str {
        &self.reference
    }

    pub fn stdout_sha256(&self) -> &str {
        &self.stdout_sha256
    }

    pub fn stderr_sha256(&self) -> &str {
        &self.stderr_sha256
    }

    pub fn stdout_bytes(&self) -> usize {
        self.stdout_bytes
    }

    pub fn stderr_bytes(&self) -> usize {
        self.stderr_bytes
    }

    pub fn capture_complete(&self) -> bool {
        self.capture_complete
    }
}

/// Output-store context minted only after the SANDBOX server assigned a job ID.
pub struct ValidationOutputContext<'a> {
    run_id: &'a str,
    attempt_id: &'a str,
    validation_id: &'a str,
    job_id: &'a str,
    binding_digest: &'a str,
}

impl ValidationOutputContext<'_> {
    pub fn run_id(&self) -> &str {
        self.run_id
    }

    pub fn attempt_id(&self) -> &str {
        self.attempt_id
    }

    pub fn validation_id(&self) -> &str {
        self.validation_id
    }

    pub fn job_id(&self) -> &str {
        self.job_id
    }

    pub fn binding_digest(&self) -> &str {
        self.binding_digest
    }

    pub fn output_reference(&self) -> String {
        output_reference_for_binding(self.binding_digest, self.job_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("validation evidence could not be persisted")]
pub struct ValidationOutputStoreError;

/// The host store must durably persist output and a host-issued validation
/// record. Implementations must not use agent-supplied paths or references.
pub trait ValidationOutputStore {
    fn persist_validation_output(
        &mut self,
        context: &ValidationOutputContext<'_>,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<String, ValidationOutputStoreError>;

    fn persist_validation_record(
        &mut self,
        record: &ValidationRecord,
    ) -> Result<(), ValidationOutputStoreError>;
}

/// Host-issued evidence for one completed SANDBOX validation job.
///
/// It deliberately has no public constructor or `Deserialize` implementation:
/// agent JSON and log text cannot be turned into a trusted receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationReceipt {
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
    output: ValidationOutputEvidence,
    outcome: ValidationOutcome,
}

impl ValidationReceipt {
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn receipt_id(&self) -> &str {
        &self.receipt_id
    }

    pub fn binding_digest(&self) -> &str {
        &self.binding_digest
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    pub fn validation_id(&self) -> &str {
        &self.validation_id
    }

    pub fn required(&self) -> bool {
        self.required
    }

    pub fn worker_name(&self) -> &str {
        &self.worker_name
    }

    pub fn expected_sandbox_id(&self) -> Option<&str> {
        self.expected_sandbox_id.as_deref()
    }

    pub fn sandbox_id(&self) -> &str {
        &self.sandbox_id
    }

    pub fn command_definition_digest(&self) -> &str {
        &self.command_definition_digest
    }

    pub fn permission_profile_digest(&self) -> &str {
        &self.permission_profile_digest
    }

    pub fn child_job_id(&self) -> &str {
        &self.child_job_id
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    pub fn execution_time_ms(&self) -> u64 {
        self.execution_time_ms
    }

    pub fn end_observed(&self) -> bool {
        self.end_observed
    }

    pub fn stream_error_observed(&self) -> bool {
        self.stream_error_observed
    }

    pub fn target_full_commit_id(&self) -> &str {
        &self.target_full_commit_id
    }

    pub fn target_full_tree_id(&self) -> &str {
        &self.target_full_tree_id
    }

    pub fn target_untracked_manifest_digest(&self) -> &str {
        &self.target_untracked_manifest_digest
    }

    pub fn target_snapshot_digest(&self) -> &str {
        &self.target_snapshot_digest
    }

    pub fn output(&self) -> &ValidationOutputEvidence {
        &self.output
    }

    pub fn outcome(&self) -> ValidationOutcome {
        self.outcome
    }
}

/// Host-observed execution state; incomplete streams never carry a receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationRecord {
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
    receipt: Option<ValidationReceipt>,
}

impl ValidationRecord {
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    pub fn validation_id(&self) -> &str {
        &self.validation_id
    }

    pub fn required(&self) -> bool {
        self.required
    }

    pub fn binding_digest(&self) -> &str {
        &self.binding_digest
    }

    pub fn outcome(&self) -> ValidationOutcome {
        self.outcome
    }

    pub fn child_job_id(&self) -> Option<&str> {
        self.child_job_id.as_deref()
    }

    pub fn sandbox_id(&self) -> Option<&str> {
        self.sandbox_id.as_deref()
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    pub fn end_observed(&self) -> bool {
        self.end_observed
    }

    pub fn transport_error_observed(&self) -> bool {
        self.transport_error_observed
    }

    pub fn receipt(&self) -> Option<&ValidationReceipt> {
        self.receipt.as_ref()
    }

    pub fn execution_time_ms(&self) -> Option<u64> {
        self.execution_time_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ValidationPlanError {
    #[error("approved validation plan is invalid")]
    InvalidPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("approved validation could not be dispatched")]
pub struct ValidationDispatchError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ValidationRunError {
    #[error(transparent)]
    Dispatch(#[from] ValidationDispatchError),
    #[error("validation record could not be persisted")]
    Persistence,
}

/// Start the approved command as a SANDBOX `run` job and issue a receipt only
/// after its server-assigned job ID, exit result, clean End, and durable output
/// reference have all been observed.
pub async fn run_approved_validation(
    handle: &JobworkerpHandle,
    plan: &ApprovedValidationPlan,
    output_store: &mut dyn ValidationOutputStore,
) -> Result<ValidationRecord, ValidationRunError> {
    let mut stream = handle
        .start_coding_agent_once(plan.worker_name(), plan.sandbox_arguments())
        .await
        .map_err(|_| ValidationRunError::Dispatch(ValidationDispatchError))?;
    let job_id = stream.job_id().value.to_string();
    if !is_canonical_job_id(&job_id) {
        return Err(ValidationRunError::Dispatch(ValidationDispatchError));
    }
    let mut capture = BoundedOutputCapture::default();
    let mut exit = None;

    loop {
        match stream.next_event().await {
            Ok(Some(CodingAgentEvent::Output { stream, data })) => {
                capture.push(&stream, &data);
            }
            Ok(Some(CodingAgentEvent::Exit {
                exit_code,
                execution_time_ms,
                sandbox_id,
            })) => {
                if exit.is_some() {
                    return persist_record(
                        output_store,
                        incomplete_record(plan, job_id, exit, false, true),
                    );
                }
                exit = Some(ObservedExit {
                    exit_code,
                    execution_time_ms,
                    sandbox_id,
                });
            }
            Ok(None) => break,
            Err(_) => {
                return persist_record(
                    output_store,
                    incomplete_record(plan, job_id, exit, false, true),
                );
            }
        }
    }

    let Some(exit) = exit else {
        return persist_record(
            output_store,
            incomplete_record(plan, job_id, None, true, false),
        );
    };
    if !capture.is_complete() {
        return persist_record(
            output_store,
            incomplete_record(plan, job_id, Some(exit), true, false),
        );
    }

    let context = ValidationOutputContext {
        run_id: &plan.run_id,
        attempt_id: &plan.attempt_id,
        validation_id: &plan.validation_id,
        job_id: &job_id,
        binding_digest: &plan.binding_digest,
    };
    let output_reference = output_store
        .persist_validation_output(&context, &capture.stdout, &capture.stderr)
        .ok()
        .filter(|reference| valid_output_reference(reference));
    let output = output_reference.map(|reference| ValidationOutputEvidence {
        reference,
        stdout_sha256: capture.stdout_digest(),
        stderr_sha256: capture.stderr_digest(),
        stdout_bytes: capture.stdout_bytes,
        stderr_bytes: capture.stderr_bytes,
        capture_complete: true,
    });

    let sandbox_valid = valid_sandbox_id(&exit.sandbox_id);
    let sandbox_matches = plan
        .expected_sandbox_id
        .as_deref()
        .is_none_or(|expected| exit.sandbox_id == expected);
    let execution_outcome = if !sandbox_valid {
        ValidationOutcome::Unknown
    } else if exit.exit_code == 0 && sandbox_matches {
        ValidationOutcome::Passed
    } else {
        ValidationOutcome::Failed
    };
    let outcome = if output.is_none() && execution_outcome == ValidationOutcome::Passed {
        ValidationOutcome::Unknown
    } else {
        execution_outcome
    };
    let receipt = output.and_then(|output| {
        (outcome != ValidationOutcome::Unknown && output.capture_complete)
            .then(|| issue_receipt(plan, &job_id, &exit, output, execution_outcome))
    });

    persist_record(
        output_store,
        ValidationRecord {
            schema_version: VALIDATION_RECORD_SCHEMA_VERSION,
            run_id: plan.run_id.clone(),
            attempt_id: plan.attempt_id.clone(),
            validation_id: plan.validation_id.clone(),
            required: plan.required,
            binding_digest: plan.binding_digest.clone(),
            outcome,
            child_job_id: Some(job_id),
            sandbox_id: sandbox_valid.then_some(exit.sandbox_id),
            exit_code: Some(exit.exit_code),
            execution_time_ms: Some(exit.execution_time_ms),
            end_observed: true,
            transport_error_observed: false,
            receipt,
        },
    )
}

fn persist_record(
    store: &mut dyn ValidationOutputStore,
    record: ValidationRecord,
) -> Result<ValidationRecord, ValidationRunError> {
    store
        .persist_validation_record(&record)
        .map_err(|_| ValidationRunError::Persistence)?;
    Ok(record)
}

/// Record an explicit host-side skip. A skipped validation is never a receipt.
pub fn record_validation_not_run(plan: &ApprovedValidationPlan) -> ValidationRecord {
    ValidationRecord {
        schema_version: VALIDATION_RECORD_SCHEMA_VERSION,
        run_id: plan.run_id.clone(),
        attempt_id: plan.attempt_id.clone(),
        validation_id: plan.validation_id.clone(),
        required: plan.required,
        binding_digest: plan.binding_digest.clone(),
        outcome: ValidationOutcome::NotRun,
        child_job_id: None,
        sandbox_id: None,
        exit_code: None,
        execution_time_ms: None,
        end_observed: false,
        transport_error_observed: false,
        receipt: None,
    }
}

/// Find only a host-issued passed receipt bound to this exact approved plan.
pub fn find_reusable_validation<'a>(
    plan: &ApprovedValidationPlan,
    receipts: &'a [ValidationReceipt],
) -> Option<&'a ValidationReceipt> {
    receipts.iter().find(|receipt| {
        receipt.outcome == ValidationOutcome::Passed
            && receipt.exit_code == 0
            && receipt.end_observed
            && !receipt.stream_error_observed
            && receipt.output.capture_complete
            && plan
                .expected_sandbox_id
                .as_deref()
                .is_none_or(|expected| receipt.sandbox_id == expected)
            && receipt.expected_sandbox_id == plan.expected_sandbox_id
            && receipt.binding_digest == plan.binding_digest
            && receipt.command_definition_digest == plan.command_definition_digest
            && receipt.target_snapshot_digest == plan.target.digest
            && receipt_integrity_valid(receipt)
    })
}

fn incomplete_record(
    plan: &ApprovedValidationPlan,
    job_id: String,
    exit: Option<ObservedExit>,
    end_observed: bool,
    transport_error_observed: bool,
) -> ValidationRecord {
    ValidationRecord {
        schema_version: VALIDATION_RECORD_SCHEMA_VERSION,
        run_id: plan.run_id.clone(),
        attempt_id: plan.attempt_id.clone(),
        validation_id: plan.validation_id.clone(),
        required: plan.required,
        binding_digest: plan.binding_digest.clone(),
        outcome: ValidationOutcome::Unknown,
        child_job_id: Some(job_id),
        sandbox_id: exit
            .as_ref()
            .filter(|exit| valid_sandbox_id(&exit.sandbox_id))
            .map(|exit| exit.sandbox_id.clone()),
        exit_code: exit.as_ref().map(|exit| exit.exit_code),
        execution_time_ms: exit.map(|exit| exit.execution_time_ms),
        end_observed,
        transport_error_observed,
        receipt: None,
    }
}

#[derive(Debug, Clone)]
struct ObservedExit {
    exit_code: i32,
    execution_time_ms: u64,
    sandbox_id: String,
}

fn issue_receipt(
    plan: &ApprovedValidationPlan,
    job_id: &str,
    exit: &ObservedExit,
    output: ValidationOutputEvidence,
    outcome: ValidationOutcome,
) -> ValidationReceipt {
    let payload = ReceiptPayload {
        schema_version: VALIDATION_RECEIPT_SCHEMA_VERSION,
        binding_digest: &plan.binding_digest,
        run_id: &plan.run_id,
        attempt_id: &plan.attempt_id,
        validation_id: &plan.validation_id,
        required: plan.required,
        worker_name: &plan.worker_name,
        expected_sandbox_id: plan.expected_sandbox_id.as_deref(),
        sandbox_id: &exit.sandbox_id,
        command_definition_digest: &plan.command_definition_digest,
        permission_profile_digest: &plan.permission_profile_digest,
        child_job_id: job_id,
        exit_code: exit.exit_code,
        execution_time_ms: exit.execution_time_ms,
        end_observed: true,
        stream_error_observed: false,
        target_full_commit_id: &plan.target.full_commit_id,
        target_full_tree_id: &plan.target.full_tree_id,
        target_untracked_manifest_digest: &plan.target.untracked_manifest_digest,
        target_snapshot_digest: &plan.target.digest,
        output: &output,
        outcome,
    };
    let receipt_id = digest_serializable(&payload);
    ValidationReceipt {
        schema_version: VALIDATION_RECEIPT_SCHEMA_VERSION,
        receipt_id,
        binding_digest: plan.binding_digest.clone(),
        run_id: plan.run_id.clone(),
        attempt_id: plan.attempt_id.clone(),
        validation_id: plan.validation_id.clone(),
        required: plan.required,
        worker_name: plan.worker_name.clone(),
        expected_sandbox_id: plan.expected_sandbox_id.clone(),
        sandbox_id: exit.sandbox_id.clone(),
        command_definition_digest: plan.command_definition_digest.clone(),
        permission_profile_digest: plan.permission_profile_digest.clone(),
        child_job_id: job_id.to_owned(),
        exit_code: exit.exit_code,
        execution_time_ms: exit.execution_time_ms,
        end_observed: true,
        stream_error_observed: false,
        target_full_commit_id: plan.target.full_commit_id.clone(),
        target_full_tree_id: plan.target.full_tree_id.clone(),
        target_untracked_manifest_digest: plan.target.untracked_manifest_digest.clone(),
        target_snapshot_digest: plan.target.digest.clone(),
        output,
        outcome,
    }
}

fn receipt_integrity_valid(receipt: &ValidationReceipt) -> bool {
    receipt.receipt_id
        == digest_serializable(&ReceiptPayload {
            schema_version: receipt.schema_version,
            binding_digest: &receipt.binding_digest,
            run_id: &receipt.run_id,
            attempt_id: &receipt.attempt_id,
            validation_id: &receipt.validation_id,
            required: receipt.required,
            worker_name: &receipt.worker_name,
            expected_sandbox_id: receipt.expected_sandbox_id.as_deref(),
            sandbox_id: &receipt.sandbox_id,
            command_definition_digest: &receipt.command_definition_digest,
            permission_profile_digest: &receipt.permission_profile_digest,
            child_job_id: &receipt.child_job_id,
            exit_code: receipt.exit_code,
            execution_time_ms: receipt.execution_time_ms,
            end_observed: receipt.end_observed,
            stream_error_observed: receipt.stream_error_observed,
            target_full_commit_id: &receipt.target_full_commit_id,
            target_full_tree_id: &receipt.target_full_tree_id,
            target_untracked_manifest_digest: &receipt.target_untracked_manifest_digest,
            target_snapshot_digest: &receipt.target_snapshot_digest,
            output: &receipt.output,
            outcome: receipt.outcome,
        })
}

#[derive(Serialize)]
struct CommandDefinition<'a> {
    worker_name: &'a str,
    argv: &'a [String],
    working_directory: &'a str,
    environment: &'a BTreeMap<String, String>,
    permission_profile_digest: &'a str,
    timeout_ms: Option<u64>,
}

#[derive(Serialize)]
struct PlanBinding<'a> {
    run_id: &'a str,
    attempt_id: &'a str,
    validation_id: &'a str,
    required: bool,
    expected_sandbox_id: Option<&'a str>,
    command_definition_digest: &'a str,
    target_snapshot_digest: &'a str,
}

#[derive(Serialize)]
struct TargetBinding<'a> {
    full_commit_id: &'a str,
    full_tree_id: &'a str,
    untracked_manifest_digest: &'a str,
}

#[derive(Serialize)]
struct ReceiptPayload<'a> {
    schema_version: u32,
    binding_digest: &'a str,
    run_id: &'a str,
    attempt_id: &'a str,
    validation_id: &'a str,
    required: bool,
    worker_name: &'a str,
    expected_sandbox_id: Option<&'a str>,
    sandbox_id: &'a str,
    command_definition_digest: &'a str,
    permission_profile_digest: &'a str,
    child_job_id: &'a str,
    exit_code: i32,
    execution_time_ms: u64,
    end_observed: bool,
    stream_error_observed: bool,
    target_full_commit_id: &'a str,
    target_full_tree_id: &'a str,
    target_untracked_manifest_digest: &'a str,
    target_snapshot_digest: &'a str,
    output: &'a ValidationOutputEvidence,
    outcome: ValidationOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PersistedValidationRecord {
    pub schema_version: u32,
    pub run_id: String,
    pub attempt_id: String,
    pub validation_id: String,
    pub required: bool,
    pub binding_digest: String,
    pub outcome: ValidationOutcome,
    pub child_job_id: Option<String>,
    pub sandbox_id: Option<String>,
    pub exit_code: Option<i32>,
    pub execution_time_ms: Option<u64>,
    pub end_observed: bool,
    pub transport_error_observed: bool,
    pub receipt_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedValidationReceipt {
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
    output: PersistedOutputEvidence,
    outcome: ValidationOutcome,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedOutputEvidence {
    reference: String,
    stdout_sha256: String,
    stderr_sha256: String,
    stdout_bytes: usize,
    stderr_bytes: usize,
    capture_complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("stored validation evidence is invalid")]
pub(super) struct ValidationRestoreError;

pub(super) fn persisted_validation_record(record: &ValidationRecord) -> PersistedValidationRecord {
    PersistedValidationRecord {
        schema_version: record.schema_version,
        run_id: record.run_id.clone(),
        attempt_id: record.attempt_id.clone(),
        validation_id: record.validation_id.clone(),
        required: record.required,
        binding_digest: record.binding_digest.clone(),
        outcome: record.outcome,
        child_job_id: record.child_job_id.clone(),
        sandbox_id: record.sandbox_id.clone(),
        exit_code: record.exit_code,
        execution_time_ms: record.execution_time_ms,
        end_observed: record.end_observed,
        transport_error_observed: record.transport_error_observed,
        receipt_id: record
            .receipt
            .as_ref()
            .map(|receipt| receipt.receipt_id.clone()),
    }
}

pub(super) fn restore_validation_receipt(
    value: serde_json::Value,
    plan: &ApprovedValidationPlan,
    job_id: &str,
    persisted_output: &ValidationOutputEvidence,
) -> Result<ValidationReceipt, ValidationRestoreError> {
    let stored: PersistedValidationReceipt =
        serde_json::from_value(value).map_err(|_| ValidationRestoreError)?;
    let target = ValidationTargetSnapshot::new(
        stored.target_full_commit_id.clone(),
        stored.target_full_tree_id.clone(),
        stored.target_untracked_manifest_digest.clone(),
    )
    .map_err(|_| ValidationRestoreError)?;
    let output = ValidationOutputEvidence {
        reference: stored.output.reference,
        stdout_sha256: stored.output.stdout_sha256,
        stderr_sha256: stored.output.stderr_sha256,
        stdout_bytes: stored.output.stdout_bytes,
        stderr_bytes: stored.output.stderr_bytes,
        capture_complete: stored.output.capture_complete,
    };
    let sandbox_matches = plan
        .expected_sandbox_id
        .as_deref()
        .is_none_or(|expected| stored.sandbox_id == expected);
    let outcome_matches = match stored.outcome {
        ValidationOutcome::Passed => stored.exit_code == 0 && sandbox_matches,
        ValidationOutcome::Failed => stored.exit_code != 0 || !sandbox_matches,
        ValidationOutcome::Unknown | ValidationOutcome::NotRun => false,
    };
    let receipt = ValidationReceipt {
        schema_version: stored.schema_version,
        receipt_id: stored.receipt_id,
        binding_digest: stored.binding_digest,
        run_id: stored.run_id,
        attempt_id: stored.attempt_id,
        validation_id: stored.validation_id,
        required: stored.required,
        worker_name: stored.worker_name,
        expected_sandbox_id: stored.expected_sandbox_id,
        sandbox_id: stored.sandbox_id,
        command_definition_digest: stored.command_definition_digest,
        permission_profile_digest: stored.permission_profile_digest,
        child_job_id: stored.child_job_id,
        exit_code: stored.exit_code,
        execution_time_ms: stored.execution_time_ms,
        end_observed: stored.end_observed,
        stream_error_observed: stored.stream_error_observed,
        target_full_commit_id: stored.target_full_commit_id,
        target_full_tree_id: stored.target_full_tree_id,
        target_untracked_manifest_digest: stored.target_untracked_manifest_digest,
        target_snapshot_digest: stored.target_snapshot_digest,
        output,
        outcome: stored.outcome,
    };
    if receipt.schema_version != VALIDATION_RECEIPT_SCHEMA_VERSION
        || receipt.run_id != plan.run_id
        || receipt.attempt_id != plan.attempt_id
        || receipt.validation_id != plan.validation_id
        || receipt.required != plan.required
        || receipt.worker_name != plan.worker_name
        || receipt.expected_sandbox_id != plan.expected_sandbox_id
        || receipt.sandbox_id.is_empty()
        || !valid_sandbox_id(&receipt.sandbox_id)
        || receipt
            .expected_sandbox_id
            .as_deref()
            .is_some_and(|expected| !valid_sandbox_id(expected))
        || receipt.command_definition_digest != plan.command_definition_digest
        || receipt.permission_profile_digest != plan.permission_profile_digest
        || receipt.child_job_id != job_id
        || !is_canonical_job_id(&receipt.child_job_id)
        || receipt.binding_digest != plan.binding_digest
        || !receipt.end_observed
        || receipt.stream_error_observed
        || receipt.target_full_commit_id != plan.target.full_commit_id
        || receipt.target_full_tree_id != plan.target.full_tree_id
        || receipt.target_untracked_manifest_digest != plan.target.untracked_manifest_digest
        || receipt.target_snapshot_digest != plan.target.digest
        || target.digest != plan.target.digest
        || receipt.output != *persisted_output
        || !receipt.output.capture_complete
        || !outcome_matches
        || !receipt_integrity_valid(&receipt)
    {
        return Err(ValidationRestoreError);
    }
    Ok(receipt)
}

pub(super) fn restore_validation_record(
    stored: PersistedValidationRecord,
    plan: &ApprovedValidationPlan,
    receipt: Option<ValidationReceipt>,
) -> Result<ValidationRecord, ValidationRestoreError> {
    if stored.schema_version != VALIDATION_RECORD_SCHEMA_VERSION
        || stored.run_id != plan.run_id
        || stored.attempt_id != plan.attempt_id
        || stored.validation_id != plan.validation_id
        || stored.required != plan.required
        || stored.binding_digest != plan.binding_digest
        || stored
            .child_job_id
            .as_deref()
            .is_some_and(|job_id| !is_canonical_job_id(job_id))
        || stored
            .sandbox_id
            .as_deref()
            .is_some_and(|sandbox_id| !valid_sandbox_id(sandbox_id))
        || stored.receipt_id.as_deref()
            != receipt.as_ref().map(|receipt| receipt.receipt_id.as_str())
    {
        return Err(ValidationRestoreError);
    }
    let receipt_matches_record = receipt.as_ref().is_some_and(|receipt| {
        receipt.run_id == stored.run_id
            && receipt.attempt_id == stored.attempt_id
            && receipt.validation_id == stored.validation_id
            && receipt.required == stored.required
            && Some(receipt.child_job_id.as_str()) == stored.child_job_id.as_deref()
            && Some(receipt.sandbox_id.as_str()) == stored.sandbox_id.as_deref()
            && Some(receipt.exit_code) == stored.exit_code
            && Some(receipt.execution_time_ms) == stored.execution_time_ms
            && receipt.end_observed == stored.end_observed
            && !stored.transport_error_observed
            && receipt.outcome == stored.outcome
    });
    let state_is_consistent = match stored.outcome {
        ValidationOutcome::Passed => receipt_matches_record,
        ValidationOutcome::Failed => {
            stored.end_observed
                && stored.exit_code.is_some()
                && stored.execution_time_ms.is_some()
                && stored.sandbox_id.is_some()
                && !stored.transport_error_observed
                && receipt.as_ref().is_none_or(|receipt| {
                    receipt_matches_record && receipt.outcome == ValidationOutcome::Failed
                })
        }
        ValidationOutcome::Unknown => {
            receipt.is_none()
                && stored.child_job_id.is_some()
                && stored.exit_code.is_some() == stored.execution_time_ms.is_some()
                && stored
                    .sandbox_id
                    .as_ref()
                    .is_none_or(|_| stored.exit_code.is_some())
                && (stored.end_observed != stored.transport_error_observed)
        }
        ValidationOutcome::NotRun => {
            receipt.is_none()
                && stored.child_job_id.is_none()
                && stored.sandbox_id.is_none()
                && stored.exit_code.is_none()
                && stored.execution_time_ms.is_none()
                && !stored.end_observed
                && !stored.transport_error_observed
        }
    };
    if !state_is_consistent {
        return Err(ValidationRestoreError);
    }
    Ok(ValidationRecord {
        schema_version: stored.schema_version,
        run_id: stored.run_id,
        attempt_id: stored.attempt_id,
        validation_id: stored.validation_id,
        required: stored.required,
        binding_digest: stored.binding_digest,
        outcome: stored.outcome,
        child_job_id: stored.child_job_id,
        sandbox_id: stored.sandbox_id,
        exit_code: stored.exit_code,
        execution_time_ms: stored.execution_time_ms,
        end_observed: stored.end_observed,
        transport_error_observed: stored.transport_error_observed,
        receipt,
    })
}

pub(super) fn persisted_output_evidence(
    reference: String,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<ValidationOutputEvidence, ValidationRestoreError> {
    if !valid_output_reference(&reference)
        || stdout.len().saturating_add(stderr.len()) > MAX_VALIDATION_OUTPUT_BYTES
    {
        return Err(ValidationRestoreError);
    }
    Ok(ValidationOutputEvidence {
        reference,
        stdout_sha256: sha256(stdout),
        stderr_sha256: sha256(stderr),
        stdout_bytes: stdout.len(),
        stderr_bytes: stderr.len(),
        capture_complete: true,
    })
}

pub(super) fn output_reference_for_binding(binding_digest: &str, job_id: &str) -> String {
    let token = digest_serializable(&OutputKey {
        binding_digest,
        job_id,
    });
    format!("validation-output:v1:{token}")
}

#[derive(Serialize)]
struct OutputKey<'a> {
    binding_digest: &'a str,
    job_id: &'a str,
}

#[derive(Default)]
struct BoundedOutputCapture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_hash: Sha256,
    stderr_hash: Sha256,
    stdout_bytes: usize,
    stderr_bytes: usize,
    truncated: bool,
    invalid_stream: bool,
}

impl BoundedOutputCapture {
    fn push(&mut self, stream: &str, bytes: &[u8]) {
        let captured = self.stdout.len().saturating_add(self.stderr.len());
        let remaining = MAX_VALIDATION_OUTPUT_BYTES.saturating_sub(captured);
        let (buffer, hash, observed) = match stream {
            "stdout" => (
                &mut self.stdout,
                &mut self.stdout_hash,
                &mut self.stdout_bytes,
            ),
            "stderr" => (
                &mut self.stderr,
                &mut self.stderr_hash,
                &mut self.stderr_bytes,
            ),
            _ => {
                self.invalid_stream = true;
                return;
            }
        };
        hash.update(bytes);
        *observed = observed.saturating_add(bytes.len());
        let stored = bytes.len().min(remaining);
        buffer.extend_from_slice(&bytes[..stored]);
        if stored != bytes.len() {
            self.truncated = true;
        }
    }

    fn is_complete(&self) -> bool {
        !self.truncated && !self.invalid_stream
    }

    fn stdout_digest(&self) -> String {
        format!("{:x}", self.stdout_hash.clone().finalize())
    }

    fn stderr_digest(&self) -> String {
        format!("{:x}", self.stderr_hash.clone().finalize())
    }
}

fn valid_output_reference(reference: &str) -> bool {
    reference.starts_with("validation-output:v1:")
        && reference.len() <= MAX_OUTPUT_REFERENCE_BYTES
        && reference
            .strip_prefix("validation-output:v1:")
            .is_some_and(|token| {
                token.len() == 64
                    && token
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_VALIDATION_IDENTIFIER_BYTES
        && !value.chars().any(char::is_control)
}

fn valid_sandbox_id(value: &str) -> bool {
    valid_identifier(value) && value.len() <= MAX_SANDBOX_ID_BYTES
}

fn is_canonical_job_id(value: &str) -> bool {
    value
        .parse::<i64>()
        .is_ok_and(|job_id| job_id > 0 && job_id.to_string() == value)
}

fn valid_text(value: &str, max_bytes: usize) -> bool {
    value.len() <= max_bytes && !value.contains('\0')
}

fn valid_environment_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn normalize_sha256(value: String) -> Result<String, ValidationPlanError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ValidationPlanError::InvalidPlan);
    }
    Ok(value.to_ascii_lowercase())
}

fn normalize_git_id(value: String) -> Result<String, ValidationPlanError> {
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ValidationPlanError::InvalidPlan);
    }
    Ok(value.to_ascii_lowercase())
}

fn digest_serializable(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("validation digest payload is serializable");
    format!("{:x}", Sha256::digest(bytes))
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::BoundedOutputCapture;

    #[test]
    fn output_capture_accepts_the_exact_limit_and_rejects_one_more_byte() {
        let mut capture = BoundedOutputCapture::default();
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..(super::MAX_VALIDATION_OUTPUT_BYTES / chunk.len()) {
            capture.push("stdout", &chunk);
        }

        assert!(capture.is_complete());
        assert_eq!(capture.stdout.len(), super::MAX_VALIDATION_OUTPUT_BYTES);
        assert_eq!(capture.stdout_bytes, super::MAX_VALIDATION_OUTPUT_BYTES);

        capture.push("stderr", b"!");

        assert!(!capture.is_complete());
        assert_eq!(capture.stdout.len(), super::MAX_VALIDATION_OUTPUT_BYTES);
        assert_eq!(capture.stderr_bytes, 1);
    }
}
