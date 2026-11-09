use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
};

use serde::{Deserialize, Serialize, de::Deserializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::assessment::{AllowedEvidenceSource, EvidenceSourceKind, ProviderSelection};
use super::profile::AgentProfileSnapshot;
use super::repository::{RepositoryIdentity, ResolvedRepository};
use super::validation::ValidationReceipt;

const APPROVAL_ASSESSMENT_CONTRACT_VERSION: u32 = 1;
const MAX_APPROVAL_ITEMS: usize = 128;
const MAX_APPROVAL_IDENTIFIER_BYTES: usize = 256;
const MAX_APPROVAL_TEXT_BYTES: usize = 16 * 1024;
const MAX_APPROVAL_INPUT_BYTES: usize = 128 * 1024;
const MAX_VALIDATION_COMMAND_BYTES: usize = 64 * 1024;
const MAX_VALIDATION_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;
const CURRENT_APPROVED_PROMPT_POLICY_VERSION: &str = "coding-agent-judge-v1";
const APPROVAL_CONTRACT_DOMAIN: &[u8] = b"jobworkerp-rs/coding-agent/approval-assessment/v1\0";
const ASSESSMENT_SCOPE_DOMAIN: &[u8] = b"jobworkerp-rs/coding-agent/judge-input-scope/v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    OneShot,
    Interactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPurpose {
    Investigation,
    Implementation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowedOperation {
    Verify,
    PublishBranch,
    CreatePullRequest,
    IssueStateUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationPlan {
    NoPublication,
    PushBranch,
    CreatePullRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunApproval {
    pub approved_by: String,
    pub approved_at_ms: u64,
    pub auto_done_approved: bool,
    pub settings_version: Option<u64>,
    pub applicable_modes: Vec<ExecutionMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDefinitionSnapshot {
    pub definition_hash: String,
    pub template_id: Option<String>,
    pub materialized_definition_hash: Option<String>,
    #[serde(default)]
    pub semantic_judge_worker_digest: Option<String>,
    pub registered_worker_id: Option<String>,
    pub workflow_execution_id: Option<String>,
    pub parent_job_id: Option<String>,
    pub child_sandbox_job_id: Option<String>,
}

/// Evaluation semantics explicitly approved for one canonical Issue condition.
/// `Unknown` is retained as unknown and is never translated into a skipped check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionEvaluationMethod {
    Deterministic,
    SemanticJudge,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalCriterionPolicy {
    pub required: bool,
    pub method: CriterionEvaluationMethod,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedAcceptanceCriterion {
    pub criterion_id: String,
    pub statement: String,
    pub required: bool,
    pub method: CriterionEvaluationMethod,
}

/// Only these source categories can enter a semantic judge's approved scope.
/// Credential and host-capability fields have no representable variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovedJudgeInputField {
    IssueTitle,
    IssueBody,
    RepositoryIdentity,
    RepositorySummary,
    ThreadGroupContext,
    BoundedFile,
    BoundedDiff,
    Report,
    ValidationResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssessmentInputScope {
    pub allowed_fields: Vec<ApprovedJudgeInputField>,
    pub allowed_sources: Vec<AllowedEvidenceSource>,
    pub max_input_bytes: usize,
}

/// Canonicalize the approval-time scope and the evaluator's prepared scope
/// through one versioned, domain-separated digest contract.
///
/// Schema version 1 fixes the judge payload envelope. The digest covers allowed
/// content categories, source identities and bounds, and prompt policy, but not
/// any later evidence bytes; those are committed by the exact input digest.
pub fn canonical_judge_input_scope_digest(
    prompt_policy_version: Option<&str>,
    allowed_fields: &[ApprovedJudgeInputField],
    allowed_sources: &[AllowedEvidenceSource],
    max_input_bytes: usize,
) -> Result<String, serde_json::Error> {
    #[derive(Serialize)]
    struct CanonicalScope {
        schema_version: u32,
        prompt_policy_version: Option<String>,
        allowed_fields: Vec<ApprovedJudgeInputField>,
        allowed_sources: Vec<AllowedEvidenceSource>,
        max_input_bytes: usize,
    }

    let mut fields = allowed_fields.to_vec();
    fields.sort();
    let mut sources = allowed_sources.to_vec();
    sources.sort();
    let bytes = serde_json::to_vec(&CanonicalScope {
        schema_version: 1,
        prompt_policy_version: prompt_policy_version.map(str::to_owned),
        allowed_fields: fields,
        allowed_sources: sources,
        max_input_bytes,
    })?;
    Ok(domain_digest(ASSESSMENT_SCOPE_DOMAIN, &bytes))
}

/// Map each approved evidence source category to the content fields that the
/// schema-v1 evaluator may receive. New source categories require an explicit
/// mapping before they can share a scope digest with an approval contract.
pub fn judge_input_fields_for_sources(
    sources: &[AllowedEvidenceSource],
) -> Vec<ApprovedJudgeInputField> {
    let mut fields = BTreeSet::new();
    for source in sources {
        match &source.kind {
            EvidenceSourceKind::IssueContext => {
                fields.insert(ApprovedJudgeInputField::IssueTitle);
                fields.insert(ApprovedJudgeInputField::IssueBody);
            }
            EvidenceSourceKind::RepositoryContext => {
                // Schema v1 only admits the repository identity from this
                // source. `RepositorySummary` remains in an approval digest
                // and therefore cannot match until it has its own source map.
                fields.insert(ApprovedJudgeInputField::RepositoryIdentity);
            }
            EvidenceSourceKind::ThreadGroupContext => {
                fields.insert(ApprovedJudgeInputField::ThreadGroupContext);
            }
            EvidenceSourceKind::File => {
                fields.insert(ApprovedJudgeInputField::BoundedFile);
            }
            EvidenceSourceKind::Diff => {
                fields.insert(ApprovedJudgeInputField::BoundedDiff);
            }
            EvidenceSourceKind::Report => {
                fields.insert(ApprovedJudgeInputField::Report);
            }
            EvidenceSourceKind::ValidationResult => {
                fields.insert(ApprovedJudgeInputField::ValidationResult);
            }
        }
    }
    fields.into_iter().collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationPermission {
    ReadWorkspace,
    WriteBuildOutput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAssessmentCommand {
    pub argv: Vec<String>,
    /// Guest path only; host paths and traversal outside `/workspace` are rejected.
    pub working_directory: String,
    pub permissions: Vec<ValidationPermission>,
    pub profile_digest: String,
    pub timeout_ms: u64,
}

pub fn validation_profile_digest(profile_id: &str, version: u64) -> Result<String, RunModelError> {
    if profile_id.is_empty()
        || profile_id.len() > 256
        || profile_id.chars().any(char::is_control)
        || version == 0
    {
        return Err(RunModelError::Invalid(
            "validation profile identity is invalid".into(),
        ));
    }
    let bytes = serde_json::to_vec(&("coding-agent-validation-profile-v1", profile_id, version))
        .map_err(|_| RunModelError::Invalid("validation profile cannot be encoded".into()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalValidationDraft {
    pub required: bool,
    #[serde(deserialize_with = "deserialize_explicit_option")]
    pub command: Option<ApprovalAssessmentCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedValidationDefinition {
    pub validation_id: String,
    pub required: bool,
    #[serde(deserialize_with = "deserialize_explicit_option")]
    pub command: Option<ApprovalAssessmentCommand>,
}

/// Versioned approval-time assessment policy. Future report, artifact, and
/// receipt bytes are deliberately absent; they are bound only by later intents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAssessmentContract {
    pub schema_version: u32,
    pub purpose: RunPurpose,
    pub criteria: Vec<ApprovedAcceptanceCriterion>,
    pub validation_plan: Vec<ApprovedValidationDefinition>,
    #[serde(deserialize_with = "deserialize_explicit_option")]
    pub provider: Option<ProviderSelection>,
    #[serde(deserialize_with = "deserialize_explicit_option")]
    pub prompt_policy_version: Option<String>,
    pub input_scope: AssessmentInputScope,
    judge_input_scope_digest: String,
}

impl ApprovalAssessmentContract {
    /// Build stable IDs from canonical Issue strings without normalizing text.
    #[allow(clippy::too_many_arguments)]
    pub fn from_issue_acceptance(
        purpose: RunPurpose,
        canonical_issue_acceptance: Vec<String>,
        criterion_policies: Vec<ApprovalCriterionPolicy>,
        validation_drafts: Vec<ApprovalValidationDraft>,
        provider: Option<ProviderSelection>,
        prompt_policy_version: Option<String>,
        input_scope: AssessmentInputScope,
    ) -> Result<Self, RunModelError> {
        if canonical_issue_acceptance.len() != criterion_policies.len() {
            return Err(RunModelError::Invalid(
                "each canonical acceptance condition requires explicit method and requiredness"
                    .into(),
            ));
        }
        if canonical_issue_acceptance.len() > MAX_APPROVAL_ITEMS
            || validation_drafts.len() > MAX_APPROVAL_ITEMS
        {
            return Err(RunModelError::Invalid(
                "assessment criteria and validations exceed the bounded item limit".into(),
            ));
        }
        let criteria = canonical_issue_acceptance
            .into_iter()
            .zip(criterion_policies)
            .enumerate()
            .map(|(index, (statement, policy))| ApprovedAcceptanceCriterion {
                criterion_id: format!("AC-{}", index + 1),
                statement,
                required: policy.required,
                method: policy.method,
            })
            .collect();
        let validation_plan = validation_drafts
            .into_iter()
            .enumerate()
            .map(|(index, draft)| ApprovedValidationDefinition {
                validation_id: format!("V-{}", index + 1),
                required: draft.required,
                command: draft.command,
            })
            .collect();
        let mut contract = Self {
            schema_version: APPROVAL_ASSESSMENT_CONTRACT_VERSION,
            purpose,
            criteria,
            validation_plan,
            provider,
            prompt_policy_version,
            input_scope,
            judge_input_scope_digest: String::new(),
        };
        contract.validate_contents()?;
        contract.judge_input_scope_digest = contract
            .compute_scope_digest()
            .map_err(|error| RunModelError::Invalid(error.to_string()))?;
        Ok(contract)
    }

    pub fn judge_input_scope_digest(&self) -> &str {
        &self.judge_input_scope_digest
    }

    pub fn digest(&self) -> Result<String, serde_json::Error> {
        let mut canonical = self.clone();
        canonical.input_scope.allowed_fields.sort();
        canonical.input_scope.allowed_sources.sort();
        let bytes = serde_json::to_vec(&canonical)?;
        Ok(domain_digest(APPROVAL_CONTRACT_DOMAIN, &bytes))
    }

    pub fn has_unknown_criterion_methods(&self) -> bool {
        self.criteria
            .iter()
            .any(|criterion| criterion.method == CriterionEvaluationMethod::Unknown)
    }

    pub fn can_dispatch_semantic_assessment(&self) -> bool {
        self.validate().is_ok()
            && !self.has_unknown_criterion_methods()
            && self
                .criteria
                .iter()
                .any(|criterion| criterion.method == CriterionEvaluationMethod::SemanticJudge)
            && self.provider.is_some()
            && self.prompt_policy_version.as_deref() == Some(CURRENT_APPROVED_PROMPT_POLICY_VERSION)
            && self
                .input_scope
                .allowed_fields
                .contains(&ApprovedJudgeInputField::Report)
            && self
                .input_scope
                .allowed_sources
                .iter()
                .filter(|source| source.kind == EvidenceSourceKind::Report)
                .count()
                == 1
    }

    pub fn validate(&self) -> Result<(), RunModelError> {
        self.validate_contents()?;
        let expected_scope_digest = self
            .compute_scope_digest()
            .map_err(|error| RunModelError::Invalid(error.to_string()))?;
        if self.judge_input_scope_digest != expected_scope_digest {
            return Err(RunModelError::Invalid(
                "approved judge input scope digest does not match its scope".into(),
            ));
        }
        Ok(())
    }

    pub fn validate_for_snapshot(&self, snapshot: &AgentRunSnapshot) -> Result<(), RunModelError> {
        self.validate()?;
        if self.purpose != snapshot.purpose
            || snapshot.approval.as_ref().is_none_or(|approval| {
                approval.approved_at_ms == 0
                    || approval.approved_by.is_empty()
                    || !approval.applicable_modes.contains(&snapshot.execution_mode)
            })
        {
            return Err(RunModelError::Invalid(
                "assessment contract must match the explicitly approved run snapshot".into(),
            ));
        }
        Ok(())
    }

    fn validate_contents(&self) -> Result<(), RunModelError> {
        if self.schema_version != APPROVAL_ASSESSMENT_CONTRACT_VERSION
            || self.criteria.len() > MAX_APPROVAL_ITEMS
            || self.validation_plan.len() > MAX_APPROVAL_ITEMS
        {
            return Err(RunModelError::Invalid(
                "unsupported assessment contract version or item count".into(),
            ));
        }
        let mut criterion_ids = HashSet::new();
        for (index, criterion) in self.criteria.iter().enumerate() {
            if !valid_contract_identifier(&criterion.criterion_id)
                || criterion.criterion_id != format!("AC-{}", index + 1)
                || !criterion_ids.insert(&criterion.criterion_id)
                || !valid_approval_statement(&criterion.statement)
            {
                return Err(RunModelError::Invalid(
                    "assessment criteria require unique bounded IDs and statements".into(),
                ));
            }
        }
        let mut validation_ids = HashSet::new();
        for (index, validation) in self.validation_plan.iter().enumerate() {
            if !valid_contract_identifier(&validation.validation_id)
                || validation.validation_id != format!("V-{}", index + 1)
                || !validation_ids.insert(&validation.validation_id)
            {
                return Err(RunModelError::Invalid(
                    "assessment validations require unique bounded IDs".into(),
                ));
            }
            if let Some(command) = &validation.command {
                validate_approval_command(command)?;
            }
        }
        if let Some(provider) = &self.provider
            && [
                provider.provider_id.as_str(),
                provider.model_id.as_str(),
                provider.model_version.as_str(),
                provider.config_version.as_str(),
            ]
            .iter()
            .any(|value| !valid_contract_identifier(value))
        {
            return Err(RunModelError::Invalid(
                "selected assessment provider and version identities must be explicit".into(),
            ));
        }
        if self
            .prompt_policy_version
            .as_deref()
            .is_some_and(|version| !valid_contract_identifier(version))
        {
            return Err(RunModelError::Invalid(
                "assessment prompt policy version is invalid".into(),
            ));
        }
        validate_input_scope(&self.input_scope)?;
        Ok(())
    }

    fn compute_scope_digest(&self) -> Result<String, serde_json::Error> {
        canonical_judge_input_scope_digest(
            self.prompt_policy_version.as_deref(),
            &self.input_scope.allowed_fields,
            &self.input_scope.allowed_sources,
            self.input_scope.max_input_bytes,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssessmentValidationReceiptReference {
    pub validation_id: String,
    pub required: bool,
    pub receipt_id: String,
    pub digest: String,
    pub command_definition_digest: String,
    pub permission_profile_digest: String,
    pub target_full_commit_id: String,
    pub target_full_tree_id: String,
    pub target_untracked_manifest_digest: String,
    pub target_snapshot_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssessmentIntentRequest {
    pub assessment_id: String,
    pub judge_input_evidence_id: String,
    pub report_evidence_id: String,
    pub snapshot_manifest_evidence_id: String,
    pub target_full_commit_id: String,
    pub target_full_tree_id: String,
    pub target_untracked_manifest_digest: String,
    pub target_snapshot_digest: String,
    pub system_prompt_digest: String,
    /// Values come from the typed host-issued receipt API, never guest JSON.
    pub validation_receipts: Vec<ValidationReceipt>,
}

/// Exact payload/evidence binding persisted before a semantic evaluator is dispatched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssessmentIntentBinding {
    pub run_id: RunId,
    pub attempt_id: String,
    pub assessment_id: String,
    pub expected_run_revision: u64,
    pub approval_snapshot_digest: String,
    pub approval_contract_digest: String,
    pub judge_input_scope_digest: String,
    pub system_prompt_digest: String,
    pub judge_input_evidence_id: String,
    pub judge_input_digest: String,
    pub report_evidence_id: String,
    pub report_digest: String,
    pub snapshot_manifest_evidence_id: String,
    pub snapshot_manifest_digest: String,
    pub target_full_commit_id: String,
    pub target_full_tree_id: String,
    pub target_untracked_manifest_digest: String,
    pub target_snapshot_digest: String,
    pub validation_receipts: Vec<AssessmentValidationReceiptReference>,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssessmentIntentClaim {
    pub binding: AssessmentIntentBinding,
    /// Existing intents are for reconciliation only and never authorize dispatch.
    pub newly_created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalAssessmentContractStatus {
    LegacyIncomplete,
    Versioned {
        contract: Box<ApprovalAssessmentContract>,
        digest: String,
    },
}

/// The durable representation adds approval data beside the source-compatible snapshot.
/// Missing keys are the only legacy form; explicit nulls are corrupt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAgentRunSnapshot {
    #[serde(flatten)]
    pub snapshot: AgentRunSnapshot,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_non_null_option"
    )]
    pub approval_assessment_contract: Option<ApprovalAssessmentContract>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_non_null_option"
    )]
    pub approval_assessment_contract_digest: Option<String>,
}

impl StoredAgentRunSnapshot {
    pub fn new(
        snapshot: AgentRunSnapshot,
        approval_assessment_contract: Option<ApprovalAssessmentContract>,
    ) -> Result<Self, RunModelError> {
        let digest = if let Some(contract) = &approval_assessment_contract {
            contract.validate_for_snapshot(&snapshot)?;
            Some(
                contract
                    .digest()
                    .map_err(|error| RunModelError::Invalid(error.to_string()))?,
            )
        } else {
            None
        };
        Ok(Self {
            snapshot,
            approval_assessment_contract,
            approval_assessment_contract_digest: digest,
        })
    }

    pub fn validate(&self) -> Result<(), RunModelError> {
        match (
            &self.approval_assessment_contract,
            &self.approval_assessment_contract_digest,
        ) {
            (None, None) => Ok(()),
            (Some(contract), Some(digest)) => {
                contract.validate_for_snapshot(&self.snapshot)?;
                let expected = contract
                    .digest()
                    .map_err(|error| RunModelError::Invalid(error.to_string()))?;
                if &expected != digest || !is_sha256(digest) {
                    return Err(RunModelError::Invalid(
                        "stored assessment contract digest is corrupt".into(),
                    ));
                }
                Ok(())
            }
            _ => Err(RunModelError::Invalid(
                "stored assessment contract and digest must be present together".into(),
            )),
        }
    }

    pub fn contract_status(&self) -> Result<ApprovalAssessmentContractStatus, RunModelError> {
        self.validate()?;
        match (
            &self.approval_assessment_contract,
            &self.approval_assessment_contract_digest,
        ) {
            (Some(contract), Some(digest)) => Ok(ApprovalAssessmentContractStatus::Versioned {
                contract: Box::new(contract.clone()),
                digest: digest.clone(),
            }),
            (None, None) => Ok(ApprovalAssessmentContractStatus::LegacyIncomplete),
            _ => unreachable!("validate checked the optional contract pair"),
        }
    }
}

fn validate_input_scope(scope: &AssessmentInputScope) -> Result<(), RunModelError> {
    if scope.allowed_fields.len() > 32
        || scope.allowed_sources.len() > MAX_APPROVAL_ITEMS
        || scope.max_input_bytes == 0
        || scope.max_input_bytes > MAX_APPROVAL_INPUT_BYTES
    {
        return Err(RunModelError::Invalid(
            "assessment input scope exceeds its explicit bounds".into(),
        ));
    }
    let fields = scope
        .allowed_fields
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if fields.len() != scope.allowed_fields.len() {
        return Err(RunModelError::Invalid(
            "assessment input fields must be unique".into(),
        ));
    }
    let mut sources = BTreeSet::new();
    for source in &scope.allowed_sources {
        let max_source_bytes = match source.kind {
            EvidenceSourceKind::Report => 1024 * 1024,
            EvidenceSourceKind::IssueContext
            | EvidenceSourceKind::RepositoryContext
            | EvidenceSourceKind::ThreadGroupContext
            | EvidenceSourceKind::File
            | EvidenceSourceKind::Diff
            | EvidenceSourceKind::ValidationResult => 32 * 1024,
        };
        if !valid_contract_identifier(&source.identity)
            || source.max_bytes == 0
            || source.max_bytes > max_source_bytes
            || !sources.insert((source.kind.clone(), source.identity.as_str()))
        {
            return Err(RunModelError::Invalid(
                "assessment sources require unique identities and bounded inputs".into(),
            ));
        }
    }
    Ok(())
}

fn validate_approval_command(command: &ApprovalAssessmentCommand) -> Result<(), RunModelError> {
    let argv_bytes = command.argv.iter().map(String::len).sum::<usize>();
    if command.argv.is_empty()
        || command.argv.len() > MAX_APPROVAL_ITEMS
        || command.argv[0].is_empty()
        || command.argv.iter().any(|argument| {
            argument.len() > MAX_APPROVAL_TEXT_BYTES || argument.contains(char::from(0))
        })
        || argv_bytes > MAX_VALIDATION_COMMAND_BYTES
        || !valid_guest_working_directory(&command.working_directory)
        || command.permissions.is_empty()
        || command.permissions.len() > 2
        || !command
            .permissions
            .contains(&ValidationPermission::ReadWorkspace)
        || command
            .permissions
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != command.permissions.len()
        || !is_sha256(&command.profile_digest)
        || command.timeout_ms == 0
        || command.timeout_ms > MAX_VALIDATION_TIMEOUT_MS
    {
        return Err(RunModelError::Invalid(
            "approved validation command must be bounded and guest-workspace scoped".into(),
        ));
    }
    Ok(())
}

fn valid_guest_working_directory(value: &str) -> bool {
    if value.len() > MAX_APPROVAL_TEXT_BYTES
        || (value != "/workspace" && !value.starts_with("/workspace/"))
    {
        return false;
    }
    if value
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b'\\')
    {
        return false;
    }
    value
        .split('/')
        .skip(1)
        .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn valid_approval_statement(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_APPROVAL_TEXT_BYTES
        && value
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
}

fn valid_contract_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_APPROVAL_IDENTIFIER_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/@+".contains(&byte))
}

fn domain_digest(domain: &[u8], bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

fn deserialize_explicit_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn deserialize_non_null_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    pub fn new(value: impl Into<String>) -> Result<Self, RunModelError> {
        let value = value.into();
        if !valid_safe_id(&value) {
            return Err(RunModelError::Invalid(
                "run id must be a bounded path-safe identifier".into(),
            ));
        }
        Ok(Self(value))
    }

    pub fn generate() -> Result<Self, RunModelError> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|error| {
            RunModelError::Invalid(format!("unable to generate run identifier: {error}"))
        })?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let id = format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
            bytes[4],
            bytes[5],
            bytes[6],
            bytes[7],
            bytes[8],
            bytes[9],
            bytes[10],
            bytes[11],
            bytes[12],
            bytes[13],
            bytes[14],
            bytes[15]
        );
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRunSnapshot {
    pub run_id: RunId,
    pub issue_id: String,
    pub issue_revision: u64,
    pub execution_mode: ExecutionMode,
    pub purpose: RunPurpose,
    pub repository: ResolvedRepository,
    pub profile: AgentProfileSnapshot,
    pub base_commit: String,
    pub workspace_branch: String,
    #[serde(default = "super::settings::semantic_judge_enabled_default")]
    pub semantic_judge_enabled: bool,
    /// Effective approval frozen with the run, independent of later settings.
    pub auto_done: bool,
    pub approval: Option<RunApproval>,
    pub allowed_operations: Vec<AllowedOperation>,
    pub publication_plan: PublicationPlan,
    pub workflow: WorkflowDefinitionSnapshot,
    pub attempt_ids: Vec<String>,
}

impl AgentRunSnapshot {
    /// Validate and freeze caller-provided approval data before it enters the
    /// durable store. Missing approval is fail-closed rather than inherited
    /// from a live application setting.
    pub fn normalize_for_storage(mut self) -> Result<Self, RunModelError> {
        if !valid_safe_id(self.run_id.as_str())
            || self.issue_id.is_empty()
            || self.issue_id.len() > 512
            || self.issue_id.chars().any(char::is_control)
            || self.issue_revision == 0
        {
            return Err(RunModelError::Invalid(
                "run and issue identity/revision are required".into(),
            ));
        }
        self.repository
            .validate()
            .map_err(|_| RunModelError::Invalid("invalid repository snapshot".into()))?;
        self.profile
            .validate_snapshot()
            .map_err(|error| RunModelError::Profile(error.to_string()))?;
        if !self
            .profile
            .allowed_repositories
            .contains(&self.repository.identity)
        {
            return Err(RunModelError::Invalid(
                "selected repository is not permitted by the profile snapshot".into(),
            ));
        }
        if !is_sha256(&self.workflow.definition_hash)
            || self
                .workflow
                .materialized_definition_hash
                .as_deref()
                .is_some_and(|hash| !is_sha256(hash))
            || self
                .workflow
                .semantic_judge_worker_digest
                .as_deref()
                .is_some_and(|hash| !is_sha256(hash))
            || !is_full_commit_id(&self.base_commit)
            || !is_valid_branch_name(&self.workspace_branch)
        {
            return Err(RunModelError::Invalid(
                "workflow digest, base commit, or workspace branch is invalid".into(),
            ));
        }
        if self.attempt_ids.is_empty() {
            return Err(RunModelError::Invalid(
                "at least one stable attempt id is required".into(),
            ));
        }
        let mut attempt_ids = BTreeSet::new();
        if self
            .attempt_ids
            .iter()
            .any(|attempt_id| !valid_safe_id(attempt_id) || !attempt_ids.insert(attempt_id))
        {
            return Err(RunModelError::Invalid(
                "attempt ids must be unique safe identifiers".into(),
            ));
        }
        if !self.semantic_judge_enabled && self.auto_done {
            return Err(RunModelError::Invalid(
                "automatic completion requires semantic assessment".into(),
            ));
        }
        if self.auto_done
            && (!self.approval.as_ref().is_some_and(|approval| {
                approval.auto_done_approved
                    && approval.approved_at_ms > 0
                    && !approval.approved_by.is_empty()
                    && approval.applicable_modes.contains(&self.execution_mode)
            }))
        {
            self.auto_done = false;
        }
        if let Some(approval) = &self.approval
            && (approval.approved_at_ms == 0
                || approval.approved_by.is_empty()
                || approval.approved_by.len() > 200
                || approval.approved_by.chars().any(char::is_control))
        {
            return Err(RunModelError::Invalid(
                "run approval must include a valid actor and timestamp".into(),
            ));
        }
        Ok(self)
    }

    pub fn primary_attempt_id(&self) -> &str {
        &self.attempt_ids[0]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    ApprovalPending,
    Queued,
    Preparing,
    Running,
    Finalizing,
    Verifying,
    Publishing,
    AwaitingReview,
    InvestigationCompleted,
    Done,
    Failed,
    Cancelled,
    Unknown,
}

impl RunState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::InvestigationCompleted | Self::Done | Self::Failed | Self::Cancelled
        )
    }

    pub fn stage(self) -> RunStage {
        match self {
            Self::ApprovalPending => RunStage::Approval,
            Self::Queued => RunStage::Queue,
            Self::Preparing => RunStage::Preparation,
            Self::Running => RunStage::Execution,
            Self::Finalizing => RunStage::Finalization,
            Self::Verifying => RunStage::Verification,
            Self::Publishing => RunStage::Publication,
            Self::AwaitingReview => RunStage::Review,
            Self::InvestigationCompleted | Self::Done => RunStage::Complete,
            Self::Failed => RunStage::Failure,
            Self::Cancelled => RunStage::Cancellation,
            Self::Unknown => RunStage::Reconciliation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStage {
    Approval,
    Queue,
    Preparation,
    Execution,
    Finalization,
    Verification,
    Publication,
    Review,
    Complete,
    Failure,
    Cancellation,
    Reconciliation,
}

pub fn can_transition(from: RunState, to: RunState) -> bool {
    if from == to || from.is_terminal() {
        return false;
    }
    if matches!(
        to,
        RunState::Failed | RunState::Cancelled | RunState::Unknown
    ) {
        return true;
    }
    match from {
        RunState::ApprovalPending => to == RunState::Queued,
        RunState::Queued => to == RunState::Preparing,
        RunState::Preparing => to == RunState::Running,
        RunState::Running => matches!(to, RunState::Finalizing | RunState::Verifying),
        RunState::Finalizing => to == RunState::Verifying,
        RunState::Verifying => matches!(
            to,
            RunState::Publishing | RunState::AwaitingReview | RunState::InvestigationCompleted
        ),
        RunState::Publishing => {
            matches!(
                to,
                RunState::AwaitingReview | RunState::InvestigationCompleted
            )
        }
        RunState::AwaitingReview => to == RunState::Done,
        RunState::Unknown => matches!(
            to,
            RunState::Queued
                | RunState::Finalizing
                | RunState::Verifying
                | RunState::Publishing
                | RunState::AwaitingReview
                | RunState::InvestigationCompleted
        ),
        RunState::InvestigationCompleted
        | RunState::Done
        | RunState::Failed
        | RunState::Cancelled => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationState {
    NotRun,
    Passed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationEvidence {
    pub required: bool,
    pub state: ValidationState,
}

/// Summary of durable run output used by pure gate evaluation. Evidence ids
/// are opaque references; bytes are never included in a report or log value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvidenceAssessment {
    pub final_exit_code: Option<i32>,
    pub end_present: bool,
    pub stream_error: bool,
    pub cancelled: bool,
    pub known_failure: bool,
    pub stdout_reference: Option<String>,
    pub stdout_bytes: u64,
    pub stderr_reference: Option<String>,
    pub stderr_bytes: u64,
    pub report_reference: Option<String>,
    pub report_save_confirmed: bool,
    pub evidence_integrity_verified: bool,
    pub commit_id: Option<String>,
    pub commit_consistent: bool,
    pub acceptance_conditions_met: bool,
    pub validation: ValidationEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateBlocker {
    AutoDoneNotApproved,
    InvestigationCannotAutoComplete,
    OperationNotApproved,
    MissingExitCode,
    UnsuccessfulExit,
    EndNotObserved,
    StreamError,
    Cancelled,
    KnownFailure,
    RequiredEvidenceMissing,
    EvidenceIntegrityUnknown,
    CommitInconsistent,
    AcceptanceConditionsUnmet,
    RequiredValidationNotRun,
    ValidationFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    NotRequired,
    Allowed,
    Blocked(Vec<GateBlocker>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionGateDecision {
    pub publication: GateDecision,
    pub auto_done: GateDecision,
}

pub fn evaluate_completion_gates(
    purpose: RunPurpose,
    effective_auto_done: bool,
    publication_plan: PublicationPlan,
    allowed_operations: &[AllowedOperation],
    evidence: &RunEvidenceAssessment,
) -> CompletionGateDecision {
    let mut common = evidence_blockers(evidence);
    if purpose == RunPurpose::Investigation
        && evidence.commit_id.is_none()
        && evidence.commit_consistent
    {
        common.retain(|blocker| *blocker != GateBlocker::CommitInconsistent);
    }
    let publication = match publication_plan {
        PublicationPlan::NoPublication => GateDecision::NotRequired,
        plan => {
            let mut blockers = common.clone();
            let needed = match plan {
                PublicationPlan::NoPublication => unreachable!(),
                PublicationPlan::PushBranch => vec![AllowedOperation::PublishBranch],
                PublicationPlan::CreatePullRequest => vec![
                    AllowedOperation::PublishBranch,
                    AllowedOperation::CreatePullRequest,
                ],
            };
            if needed
                .iter()
                .any(|operation| !allowed_operations.contains(operation))
            {
                blockers.push(GateBlocker::OperationNotApproved);
            }
            gate_result(blockers)
        }
    };

    let mut auto_done = common;
    if !effective_auto_done {
        auto_done.push(GateBlocker::AutoDoneNotApproved);
    }
    if !evidence.acceptance_conditions_met {
        auto_done.push(GateBlocker::AcceptanceConditionsUnmet);
    }
    if evidence.validation.required && evidence.validation.state != ValidationState::Passed {
        auto_done.push(GateBlocker::RequiredValidationNotRun);
    }
    CompletionGateDecision {
        publication,
        auto_done: gate_result(auto_done),
    }
}

fn evidence_blockers(evidence: &RunEvidenceAssessment) -> Vec<GateBlocker> {
    let mut blockers = Vec::new();
    match evidence.final_exit_code {
        None => blockers.push(GateBlocker::MissingExitCode),
        Some(0) => {}
        Some(_) => blockers.push(GateBlocker::UnsuccessfulExit),
    }
    if !evidence.end_present {
        blockers.push(GateBlocker::EndNotObserved);
    }
    if evidence.stream_error {
        blockers.push(GateBlocker::StreamError);
    }
    if evidence.cancelled {
        blockers.push(GateBlocker::Cancelled);
    }
    if evidence.known_failure || evidence.validation.state == ValidationState::Failed {
        blockers.push(GateBlocker::KnownFailure);
    }
    if evidence
        .report_reference
        .as_deref()
        .is_none_or(str::is_empty)
        || !evidence.report_save_confirmed
    {
        blockers.push(GateBlocker::RequiredEvidenceMissing);
    }
    if !evidence.evidence_integrity_verified {
        blockers.push(GateBlocker::EvidenceIntegrityUnknown);
    }
    if !evidence.commit_consistent || evidence.commit_id.as_deref().is_none_or(str::is_empty) {
        blockers.push(GateBlocker::CommitInconsistent);
    }
    blockers
}

fn gate_result(blockers: Vec<GateBlocker>) -> GateDecision {
    if blockers.is_empty() {
        GateDecision::Allowed
    } else {
        GateDecision::Blocked(blockers)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableOperationKind {
    PrepareWorkspace,
    StartAgent,
    VerifyEvidence,
    ReassessSemantics,
    PublishBranch,
    CreatePullRequest,
    UpdateIssueState,
    CancelAgent,
    FinishInteractive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DurableOperationRequest {
    PrepareWorkspace {
        repository: RepositoryIdentity,
        base_commit: String,
        branch: String,
    },
    StartAgent {
        workflow_definition_hash: String,
        attempt_id: String,
        execution_mode: ExecutionMode,
    },
    VerifyEvidence {
        evidence_ids: Vec<String>,
        commit_id: Option<String>,
    },
    ReassessSemantics {
        expected_run_revision: u64,
        issue_revision: u64,
        source_sha256: String,
        report_reference: String,
    },
    PublishBranch {
        branch: String,
        commit_id: String,
    },
    CreatePullRequest {
        branch: String,
        base_branch: String,
        commit_id: String,
    },
    UpdateIssueState {
        expected_revision: u64,
        target_state: String,
    },
    CancelAgent,
    FinishInteractive {
        profile_version: u64,
    },
}

impl DurableOperationRequest {
    pub fn kind(&self) -> DurableOperationKind {
        match self {
            Self::PrepareWorkspace { .. } => DurableOperationKind::PrepareWorkspace,
            Self::StartAgent { .. } => DurableOperationKind::StartAgent,
            Self::VerifyEvidence { .. } => DurableOperationKind::VerifyEvidence,
            Self::ReassessSemantics { .. } => DurableOperationKind::ReassessSemantics,
            Self::PublishBranch { .. } => DurableOperationKind::PublishBranch,
            Self::CreatePullRequest { .. } => DurableOperationKind::CreatePullRequest,
            Self::UpdateIssueState { .. } => DurableOperationKind::UpdateIssueState,
            Self::CancelAgent => DurableOperationKind::CancelAgent,
            Self::FinishInteractive { .. } => DurableOperationKind::FinishInteractive,
        }
    }

    pub fn payload_hash(&self) -> Result<String, serde_json::Error> {
        serde_json::to_vec(self).map(|bytes| format!("{:x}", Sha256::digest(bytes)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableOperationStatus {
    Intent,
    Completed,
    ResultUnconfirmed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableOperationOutcome {
    Succeeded,
    Failed,
    ResultUnconfirmed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableOperationResult {
    pub outcome: DurableOperationOutcome,
    pub result_digest: Option<String>,
    pub evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableOperationRecord {
    pub operation_id: String,
    pub run_id: RunId,
    pub attempt_id: String,
    pub idempotency_key: String,
    pub kind: DurableOperationKind,
    pub payload_hash: String,
    pub request: DurableOperationRequest,
    pub status: DurableOperationStatus,
    pub result: Option<DurableOperationResult>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Result of atomically claiming a durable intent before performing its
/// external effect. Only the caller that created the intent may dispatch it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableOperationClaim {
    pub operation: DurableOperationRecord,
    pub newly_created: bool,
}

/// One attempt-scoped publication admission. Existing intents have no permit
/// and can only be reconciled, never dispatched again.
#[derive(Debug)]
pub struct PublicationEffectClaim {
    pub operation: DurableOperationRecord,
    pub newly_created: bool,
    pub permit: Option<PublicationEffectPermit>,
}

/// Non-serializable authority to record one result for a freshly admitted
/// publication intent. It is tied to the `RunStore` instance that validated it
/// and is not a retry/reconciliation token.
#[derive(Debug)]
pub struct PublicationEffectPermit {
    pub(crate) run_id: RunId,
    pub(crate) attempt_id: String,
    pub(crate) idempotency_key: String,
    pub(crate) operation_id: String,
    pub(crate) payload_hash: String,
    pub(crate) phase_generation: u64,
    pub(crate) expected_revision: u64,
    pub(crate) store_identity: Arc<()>,
}

/// Start-attempt idempotency result for the coordinator. Only a newly inserted
/// intent may authorize first dispatch; an existing intent must be reconciled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartAttemptResult {
    pub operation: DurableOperationRecord,
    pub newly_created: bool,
    /// Revision reserved atomically with the first StartAgent intent. This is
    /// the expected revision for `mark_attempt_running`.
    pub run_revision: u64,
    /// Preparation generation consumed by this start claim.
    pub phase_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceReference {
    pub evidence_id: String,
    pub attempt_id: String,
    pub name: String,
    pub media_type: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub save_confirmed: bool,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAttempt {
    pub attempt_id: String,
    pub ordinal: u32,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractiveFinishRequest {
    pub run_id: RunId,
    pub attempt_id: String,
    pub profile_version: u64,
    pub requested_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessExitObservation {
    ExitedNormally { exit_code: i32 },
    Signaled { signal: i32 },
    Killed,
    Disconnected,
    Unknown,
}

/// How the approved command was launched by the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionInvocationKind {
    /// Legacy or incomplete evidence did not identify the invocation shape.
    #[default]
    Unknown,
    DirectProcess,
    ShellWrapper,
}

/// Host observation of an optional outer shell process.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ShellProcessObservation {
    NotSpawned,
    Spawned {
        exit: ProcessExitObservation,
    },
    #[default]
    Unknown,
}

impl ShellProcessObservation {
    pub(crate) fn is_quiesced_for(&self, invocation_kind: ExecutionInvocationKind) -> bool {
        match (invocation_kind, self) {
            (ExecutionInvocationKind::DirectProcess, Self::NotSpawned) => true,
            (ExecutionInvocationKind::ShellWrapper, Self::Spawned { exit }) => {
                process_exit_is_stopped(exit)
            }
            (ExecutionInvocationKind::Unknown, _)
            | (ExecutionInvocationKind::DirectProcess, Self::Spawned { .. } | Self::Unknown)
            | (ExecutionInvocationKind::ShellWrapper, Self::NotSpawned | Self::Unknown) => false,
        }
    }

    pub(crate) fn is_successful_for(&self, invocation_kind: ExecutionInvocationKind) -> bool {
        match (invocation_kind, self) {
            (ExecutionInvocationKind::DirectProcess, Self::NotSpawned) => true,
            (ExecutionInvocationKind::ShellWrapper, Self::Spawned { exit }) => {
                exit == &ProcessExitObservation::ExitedNormally { exit_code: 0 }
            }
            (ExecutionInvocationKind::Unknown, _)
            | (ExecutionInvocationKind::DirectProcess, Self::Spawned { .. } | Self::Unknown)
            | (ExecutionInvocationKind::ShellWrapper, Self::NotSpawned | Self::Unknown) => false,
        }
    }
}

fn process_exit_is_stopped(exit: &ProcessExitObservation) -> bool {
    match exit {
        ProcessExitObservation::ExitedNormally { .. } | ProcessExitObservation::Killed => true,
        ProcessExitObservation::Signaled { signal } => *signal > 0,
        ProcessExitObservation::Disconnected | ProcessExitObservation::Unknown => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildWritersStatus {
    Stopped,
    Active,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkspaceFreezeStatus {
    HostVerified { commit_id: String },
    NotFrozen,
    Unknown,
}

/// Host-observed interactive shutdown evidence. The trusted host adapter must
/// create this only after persisting a finish request, observing the invocation's
/// process exits, stopping child writers, freezing the workspace, and verifying
/// the frozen commit. Never populate it from guest-reported booleans or UI input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InteractiveFinalizationEvidence {
    profile_version: u64,
    #[serde(default)]
    invocation_kind: ExecutionInvocationKind,
    cli_exit: ProcessExitObservation,
    #[serde(default)]
    shell_observation: ShellProcessObservation,
    child_writers: ChildWritersStatus,
    workspace_freeze: WorkspaceFreezeStatus,
}

impl InteractiveFinalizationEvidence {
    pub(crate) fn from_host_observations(
        profile_version: u64,
        cli_exit: ProcessExitObservation,
        shell_exit: ProcessExitObservation,
        child_writers: ChildWritersStatus,
        workspace_freeze: WorkspaceFreezeStatus,
    ) -> Self {
        let legacy_processes_stopped =
            process_exit_is_stopped(&cli_exit) && process_exit_is_stopped(&shell_exit);
        Self::from_host_observations_with_invocation(
            profile_version,
            if legacy_processes_stopped {
                ExecutionInvocationKind::ShellWrapper
            } else {
                ExecutionInvocationKind::Unknown
            },
            cli_exit,
            if legacy_processes_stopped {
                ShellProcessObservation::Spawned { exit: shell_exit }
            } else {
                ShellProcessObservation::Unknown
            },
            child_writers,
            workspace_freeze,
        )
    }

    pub(crate) fn from_host_observations_with_invocation(
        profile_version: u64,
        invocation_kind: ExecutionInvocationKind,
        cli_exit: ProcessExitObservation,
        shell_observation: ShellProcessObservation,
        child_writers: ChildWritersStatus,
        workspace_freeze: WorkspaceFreezeStatus,
    ) -> Self {
        Self {
            profile_version,
            invocation_kind,
            cli_exit,
            shell_observation,
            child_writers,
            workspace_freeze,
        }
    }

    pub fn profile_version(&self) -> u64 {
        self.profile_version
    }

    pub fn cli_exit(&self) -> &ProcessExitObservation {
        &self.cli_exit
    }

    pub fn invocation_kind(&self) -> ExecutionInvocationKind {
        self.invocation_kind
    }

    pub fn shell_observation(&self) -> &ShellProcessObservation {
        &self.shell_observation
    }

    /// Compatibility view for callers that still persist the old shell-exit field.
    /// A direct invocation has no shell exit, so it is exposed as unknown, never success.
    pub fn shell_exit(&self) -> &ProcessExitObservation {
        static UNKNOWN_EXIT: ProcessExitObservation = ProcessExitObservation::Unknown;
        match &self.shell_observation {
            ShellProcessObservation::Spawned { exit } => exit,
            ShellProcessObservation::NotSpawned | ShellProcessObservation::Unknown => &UNKNOWN_EXIT,
        }
    }

    pub fn child_writers(&self) -> ChildWritersStatus {
        self.child_writers
    }

    pub fn workspace_freeze(&self) -> &WorkspaceFreezeStatus {
        &self.workspace_freeze
    }

    pub(crate) fn is_successful_for(
        &self,
        profile_version: u64,
        assessment: &RunEvidenceAssessment,
    ) -> bool {
        self.profile_version == profile_version
            && self.cli_exit == ProcessExitObservation::ExitedNormally { exit_code: 0 }
            && self
                .shell_observation
                .is_successful_for(self.invocation_kind)
            && self.child_writers == ChildWritersStatus::Stopped
            && match &self.workspace_freeze {
                WorkspaceFreezeStatus::HostVerified { commit_id } => {
                    valid_full_commit_id(commit_id)
                        && assessment.commit_id.as_deref() == Some(commit_id)
                        && assessment.commit_consistent
                }
                WorkspaceFreezeStatus::NotFrozen | WorkspaceFreezeStatus::Unknown => false,
            }
    }
}

#[cfg(test)]
mod invocation_evidence_tests {
    use super::{
        ChildWritersStatus, ExecutionInvocationKind, InteractiveFinalizationEvidence,
        ProcessExitObservation, ShellProcessObservation, WorkspaceFreezeStatus,
    };

    #[test]
    fn direct_finalization_serializes_no_synthetic_shell_exit() {
        let evidence = InteractiveFinalizationEvidence::from_host_observations_with_invocation(
            1,
            ExecutionInvocationKind::DirectProcess,
            ProcessExitObservation::ExitedNormally { exit_code: 0 },
            ShellProcessObservation::NotSpawned,
            ChildWritersStatus::Stopped,
            WorkspaceFreezeStatus::HostVerified {
                commit_id: "a".repeat(40),
            },
        );

        let serialized = serde_json::to_value(&evidence).unwrap();

        assert_eq!(serialized["invocation_kind"], "direct_process");
        assert_eq!(serialized["shell_observation"]["state"], "not_spawned");
        assert!(serialized.get("shell_exit").is_none());
        assert_eq!(evidence.shell_exit(), &ProcessExitObservation::Unknown);
    }

    #[test]
    fn legacy_evidence_requires_observed_process_stops_before_wrapper_fallback() {
        let actual_shell = InteractiveFinalizationEvidence::from_host_observations(
            1,
            ProcessExitObservation::ExitedNormally { exit_code: 0 },
            ProcessExitObservation::ExitedNormally { exit_code: 0 },
            ChildWritersStatus::Stopped,
            WorkspaceFreezeStatus::HostVerified {
                commit_id: "a".repeat(40),
            },
        );
        let missing_shell = InteractiveFinalizationEvidence::from_host_observations(
            1,
            ProcessExitObservation::ExitedNormally { exit_code: 0 },
            ProcessExitObservation::Unknown,
            ChildWritersStatus::Stopped,
            WorkspaceFreezeStatus::HostVerified {
                commit_id: "a".repeat(40),
            },
        );

        assert_eq!(
            actual_shell.invocation_kind(),
            ExecutionInvocationKind::ShellWrapper
        );
        assert!(matches!(
            actual_shell.shell_observation(),
            ShellProcessObservation::Spawned {
                exit: ProcessExitObservation::ExitedNormally { exit_code: 0 }
            }
        ));
        assert_eq!(
            missing_shell.invocation_kind(),
            ExecutionInvocationKind::Unknown
        );
        assert_eq!(
            missing_shell.shell_observation(),
            &ShellProcessObservation::Unknown
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRunRecord {
    pub snapshot: AgentRunSnapshot,
    pub revision: u64,
    pub state: RunState,
    pub stage: RunStage,
    /// Monotonic preparation generation; retry preparation advances it.
    pub start_phase_generation: u64,
    /// Attempt selected by the most recent durable StartAgent intent.
    pub current_attempt_id: Option<String>,
    pub attempts: Vec<RunAttempt>,
    pub evidence: Vec<EvidenceReference>,
    /// Assessment for `current_attempt_id` only; prior attempts never substitute.
    pub execution_evidence: Option<RunEvidenceAssessment>,
    pub interactive_finish_request: Option<InteractiveFinishRequest>,
    /// Serialized for read-only inspection, but never accepted from serialized
    /// input as host proof.
    #[serde(skip_deserializing)]
    pub interactive_finalization: Option<InteractiveFinalizationEvidence>,
}

/// Durable audit record for an application-authorized Issue completion action.
/// It contains no bearer capability or authentication material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCompletionAuthorization {
    pub authorization_id: String,
    pub run_id: RunId,
    pub issue_id: String,
    /// Attempt whose evidence and report manifest were validated for this
    /// authorization. Legacy records bind the attempt through their evidence
    /// digest and local run revision instead.
    #[serde(default)]
    pub attempt_id: String,
    pub action: CompletionActionKind,
    pub actor_id: String,
    pub policy_identity: String,
    pub policy_version: u64,
    pub snapshot_sha256: String,
    pub approved_settings_version: Option<u64>,
    pub execution_mode: ExecutionMode,
    pub commit_id: String,
    pub evidence_sha256: String,
    pub report_sha256: String,
    /// Digest of the exact local transition intent (run, snapshot, evidence,
    /// report, actor, revision, idempotency key, and server identity).
    #[serde(default)]
    pub intent_sha256: String,
    /// SHA-256 of the exact encoded manager TransitionIssueArgs request,
    /// persisted before dispatch and recomputed during receipt validation.
    #[serde(default)]
    pub expected_manager_request_digest: String,
    pub completion_report_protobuf: Vec<u8>,
    pub audit_reason: String,
    pub marker_revision: u64,
    pub target_issue_revision: u64,
    pub run_revision: u64,
    pub intent_idempotency_key: String,
    /// Manager identity against which the immutable transition intent was
    /// prepared. A lookup against a different tracker data root is unconfirmed.
    #[serde(default)]
    pub primary_tracker_identity: Option<PrimaryTrackerIdentity>,
    /// Actual manager/downstream identities returned by the primary tracker.
    #[serde(default)]
    pub primary_operation: Option<PrimaryOperationReceipt>,
    /// Number of RPC dispatches for this immutable Manager request. The first
    /// dispatch may be followed by at most one explicit recovery replay with
    /// the exact same scoped key and encoded request digest.
    #[serde(default)]
    pub dispatch_attempts: u8,
    pub status: CompletionAuthorizationStatus,
    pub resulting_issue_revision: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Stable identity from the primary manager health contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrimaryTrackerIdentity {
    pub schema_fingerprint: String,
    pub data_root_id: String,
    pub contract_fingerprint: String,
}

/// Server-issued identity for the manager operation and its backend operation.
/// IDs are recorded only from typed tracker responses/lookups, never synthesized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrimaryOperationReceipt {
    pub manager_operation_id: String,
    pub downstream_operation_id: String,
    /// Exact run-scoped key echoed in the manager's immutable operation audit.
    pub manager_idempotency_key: String,
    pub idempotency_scope: String,
    /// Server SHA-256 of the exact serialized manager TransitionIssueArgs bytes
    /// sent by this client, including ManagerWriteContext.
    pub request_digest: String,
    /// The server labels its persisted outbox request envelope with this
    /// schema, even though request_digest hashes the admitted manager args.
    pub request_schema: String,
    /// TaskChampion contract identity reported alongside the manager identity;
    /// the downstream OperationRecord uses this value, not the manager one.
    #[serde(default)]
    pub backend_contract_fingerprint: String,
    pub manager_sequence: u64,
    pub generation: u64,
    pub server_identity: PrimaryTrackerIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionActionKind {
    AutoDone,
    HumanDone,
    HumanCorrection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionAuthorizationStatus {
    Prepared,
    Dispatched,
    Completed,
    Revoked,
}

#[derive(Debug, Error)]
pub enum RunModelError {
    #[error("invalid agent run: {0}")]
    Invalid(String),
    #[error("invalid embedded profile snapshot: {0}")]
    Profile(String),
}

fn valid_safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn is_full_commit_id(value: &str) -> bool {
    valid_full_commit_id(value)
}

fn valid_full_commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Validate a branch as a safe `refs/heads/<branch>` name without invoking Git.
pub fn is_valid_branch_name(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 240
        || value == "@"
        || value.starts_with('-')
        || value.contains("..")
        || value.contains("@{")
        || value
            .chars()
            .any(|character| character.is_control() || " ~^:?*[\\".contains(character))
    {
        return false;
    }
    value.split('/').all(|component| {
        !component.is_empty()
            && !component.starts_with('.')
            && !component.ends_with('.')
            && !component.ends_with(".lock")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence() -> RunEvidenceAssessment {
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
            report_reference: Some("evidence-1".into()),
            report_save_confirmed: true,
            evidence_integrity_verified: true,
            commit_id: Some("a".repeat(40)),
            commit_consistent: true,
            acceptance_conditions_met: true,
            validation: ValidationEvidence {
                required: true,
                state: ValidationState::Passed,
            },
        }
    }

    #[test]
    fn transition_contract_keeps_interactive_execution_in_intermediate_states() {
        assert!(can_transition(RunState::ApprovalPending, RunState::Queued));
        assert!(can_transition(RunState::Running, RunState::Finalizing));
        assert!(can_transition(RunState::Finalizing, RunState::Verifying));
        assert!(!can_transition(RunState::Running, RunState::Done));
        assert!(!can_transition(RunState::Running, RunState::AwaitingReview));
        assert!(!can_transition(RunState::Unknown, RunState::Preparing));
        assert!(!can_transition(RunState::Unknown, RunState::Running));
    }

    #[test]
    fn branch_names_reject_git_ref_unsafe_components_without_git_commands() {
        for branch in [
            "@",
            ".hidden",
            "feature/.hidden",
            "feature/foo.lock",
            "feature/..",
            "feature//child",
            "/feature",
            "feature/",
            "name@{upstream}",
            "with space",
            "with\\backslash",
        ] {
            assert!(
                !is_valid_branch_name(branch),
                "accepted unsafe branch {branch:?}"
            );
        }
        for branch in ["feature/issue-72", "topic/foo@bar", "fix.v2/issue-72"] {
            assert!(
                is_valid_branch_name(branch),
                "rejected valid branch {branch:?}"
            );
        }
    }

    #[test]
    fn investigation_uses_the_same_auto_completion_gates_as_implementation() {
        let decision = evaluate_completion_gates(
            RunPurpose::Investigation,
            true,
            PublicationPlan::NoPublication,
            &[],
            &evidence(),
        );

        assert_eq!(decision.publication, GateDecision::NotRequired);
        assert_eq!(decision.auto_done, GateDecision::Allowed);
    }

    #[test]
    fn investigation_without_a_new_commit_still_requires_source_consistency() {
        let mut report_only = evidence();
        report_only.commit_id = None;
        let decision = evaluate_completion_gates(
            RunPurpose::Investigation,
            true,
            PublicationPlan::NoPublication,
            &[],
            &report_only,
        );
        assert_eq!(decision.auto_done, GateDecision::Allowed);
        report_only.commit_consistent = false;
        let decision = evaluate_completion_gates(
            RunPurpose::Investigation,
            true,
            PublicationPlan::NoPublication,
            &[],
            &report_only,
        );
        assert!(matches!(decision.auto_done, GateDecision::Blocked(_)));
        let mut inconsistent = evidence();
        inconsistent.commit_consistent = false;
        let decision = evaluate_completion_gates(
            RunPurpose::Investigation,
            true,
            PublicationPlan::PushBranch,
            &[AllowedOperation::PublishBranch],
            &inconsistent,
        );
        assert!(matches!(decision.publication, GateDecision::Blocked(_)));
        assert!(matches!(decision.auto_done, GateDecision::Blocked(_)));
    }

    #[test]
    fn execution_failure_blocks_both_publication_and_auto_done() {
        let mut failed = evidence();
        failed.final_exit_code = Some(7);
        let decision = evaluate_completion_gates(
            RunPurpose::Implementation,
            true,
            PublicationPlan::PushBranch,
            &[AllowedOperation::PublishBranch],
            &failed,
        );

        assert!(matches!(decision.publication, GateDecision::Blocked(_)));
        assert!(matches!(decision.auto_done, GateDecision::Blocked(_)));
    }

    #[test]
    fn cancellation_and_stream_failure_independently_block_success_gates() {
        let mut cancelled = evidence();
        cancelled.cancelled = true;
        let mut stream_error = evidence();
        stream_error.stream_error = true;
        for (failed, blocker) in [
            (cancelled, GateBlocker::Cancelled),
            (stream_error, GateBlocker::StreamError),
        ] {
            let decision = evaluate_completion_gates(
                RunPurpose::Implementation,
                true,
                PublicationPlan::PushBranch,
                &[AllowedOperation::PublishBranch],
                &failed,
            );
            assert!(matches!(
                decision.publication,
                GateDecision::Blocked(ref blockers) if blockers.contains(&blocker)
            ));
            assert!(matches!(
                decision.auto_done,
                GateDecision::Blocked(ref blockers) if blockers.contains(&blocker)
            ));
        }
    }

    #[test]
    fn required_validation_not_run_blocks_auto_done_but_not_approved_publication() {
        let mut pending_validation = evidence();
        pending_validation.validation.state = ValidationState::NotRun;
        let decision = evaluate_completion_gates(
            RunPurpose::Implementation,
            true,
            PublicationPlan::PushBranch,
            &[AllowedOperation::PublishBranch],
            &pending_validation,
        );

        assert_eq!(decision.publication, GateDecision::Allowed);
        assert!(matches!(decision.auto_done, GateDecision::Blocked(_)));
    }
}

#[cfg(test)]
mod approval_assessment_contract_tests {
    use super::*;
    use crate::coding_agent::assessment::{
        AllowedEvidenceSource, EvidenceSourceKind, ProviderSelection,
    };

    fn scope() -> AssessmentInputScope {
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
        }
    }

    fn provider() -> ProviderSelection {
        ProviderSelection {
            provider_id: "ollama".into(),
            model_id: "qwen2.5-coder".into(),
            model_version: "7b".into(),
            config_version: "settings-4".into(),
        }
    }

    #[test]
    fn contract_builder_preserves_canonical_issue_order_and_newlines_with_stable_ids() {
        let contract = ApprovalAssessmentContract::from_issue_acceptance(
            RunPurpose::Implementation,
            vec!["first line\nsecond line".into(), "follow-up".into()],
            vec![
                ApprovalCriterionPolicy {
                    required: true,
                    method: CriterionEvaluationMethod::SemanticJudge,
                },
                ApprovalCriterionPolicy {
                    required: false,
                    method: CriterionEvaluationMethod::Unknown,
                },
            ],
            vec![ApprovalValidationDraft {
                required: true,
                command: None,
            }],
            Some(provider()),
            Some("coding-agent-judge-v1".into()),
            scope(),
        )
        .unwrap();

        assert_eq!(contract.criteria[0].criterion_id, "AC-1");
        assert_eq!(contract.criteria[0].statement, "first line\nsecond line");
        assert!(contract.criteria[0].required);
        assert_eq!(contract.criteria[1].criterion_id, "AC-2");
        assert_eq!(
            contract.criteria[1].method,
            CriterionEvaluationMethod::Unknown
        );
        assert_eq!(contract.validation_plan[0].validation_id, "V-1");
        assert!(contract.validation_plan[0].required);
        assert!(contract.validation_plan[0].command.is_none());
        assert!(!contract.can_dispatch_semantic_assessment());
        assert_eq!(contract.judge_input_scope_digest().len(), 64);
        assert_eq!(contract.digest().unwrap().len(), 64);

        let encoded = serde_json::to_value(&contract).unwrap();
        assert_eq!(
            encoded["validation_plan"][0]["command"],
            serde_json::Value::Null
        );
        assert!(serde_json::from_value::<ApprovalAssessmentContract>(encoded).is_ok());
    }

    #[test]
    fn contract_digests_are_stable_but_preserve_approved_criterion_order() {
        let make = |criteria: Vec<&str>| {
            ApprovalAssessmentContract::from_issue_acceptance(
                RunPurpose::Investigation,
                criteria.into_iter().map(str::to_owned).collect(),
                vec![
                    ApprovalCriterionPolicy {
                        required: true,
                        method: CriterionEvaluationMethod::Deterministic,
                    },
                    ApprovalCriterionPolicy {
                        required: false,
                        method: CriterionEvaluationMethod::SemanticJudge,
                    },
                ],
                Vec::new(),
                None,
                None,
                scope(),
            )
            .unwrap()
        };
        let first = make(vec!["one", "two"]);
        let same = make(vec!["one", "two"]);
        let reordered = make(vec!["two", "one"]);
        let mut reordered_scope = scope();
        reordered_scope.allowed_fields.reverse();
        let reordered_scope_contract = ApprovalAssessmentContract::from_issue_acceptance(
            RunPurpose::Investigation,
            vec!["one".into(), "two".into()],
            vec![
                ApprovalCriterionPolicy {
                    required: true,
                    method: CriterionEvaluationMethod::Deterministic,
                },
                ApprovalCriterionPolicy {
                    required: false,
                    method: CriterionEvaluationMethod::SemanticJudge,
                },
            ],
            Vec::new(),
            None,
            None,
            reordered_scope,
        )
        .unwrap();

        assert_eq!(first.digest().unwrap(), same.digest().unwrap());
        assert_eq!(
            first.digest().unwrap(),
            reordered_scope_contract.digest().unwrap()
        );
        assert_eq!(
            first.judge_input_scope_digest(),
            same.judge_input_scope_digest()
        );
        assert_ne!(first.digest().unwrap(), reordered.digest().unwrap());
        assert_eq!(
            first.judge_input_scope_digest(),
            reordered.judge_input_scope_digest()
        );
    }

    #[test]
    fn judge_input_scope_digest_canonicalizes_policy_but_binds_every_scope_dimension() {
        let fields = vec![
            ApprovedJudgeInputField::IssueTitle,
            ApprovedJudgeInputField::IssueBody,
            ApprovedJudgeInputField::Report,
        ];
        let sources = vec![
            AllowedEvidenceSource {
                kind: EvidenceSourceKind::IssueContext,
                identity: "issue:17@revision-3".into(),
                max_bytes: 4096,
            },
            AllowedEvidenceSource {
                kind: EvidenceSourceKind::Report,
                identity: "agent-report".into(),
                max_bytes: 16 * 1024,
            },
        ];
        let digest = canonical_judge_input_scope_digest(
            Some("coding-agent-judge-v1"),
            &fields,
            &sources,
            32 * 1024,
        )
        .unwrap();
        let mut reordered_fields = fields.clone();
        reordered_fields.reverse();
        let mut reordered_sources = sources.clone();
        reordered_sources.reverse();
        assert_eq!(
            digest,
            canonical_judge_input_scope_digest(
                Some("coding-agent-judge-v1"),
                &reordered_fields,
                &reordered_sources,
                32 * 1024,
            )
            .unwrap()
        );

        #[derive(Serialize)]
        struct PreviouslyPersistedScopeV1 {
            schema_version: u32,
            prompt_policy_version: Option<String>,
            allowed_fields: Vec<ApprovedJudgeInputField>,
            allowed_sources: Vec<AllowedEvidenceSource>,
            max_input_bytes: usize,
        }
        let mut legacy_fields = fields.clone();
        legacy_fields.sort();
        let mut legacy_sources = sources.clone();
        legacy_sources.sort();
        let legacy_bytes = serde_json::to_vec(&PreviouslyPersistedScopeV1 {
            schema_version: 1,
            prompt_policy_version: Some("coding-agent-judge-v1".into()),
            allowed_fields: legacy_fields,
            allowed_sources: legacy_sources,
            max_input_bytes: 32 * 1024,
        })
        .unwrap();
        let legacy_digest = domain_digest(ASSESSMENT_SCOPE_DOMAIN, &legacy_bytes);
        assert_eq!(
            digest, legacy_digest,
            "the existing v1 contract encoding remains readable without digest rewrites"
        );

        let mut changed_identity = sources.clone();
        changed_identity[1].identity = "agent-report-other".into();
        let mut changed_limit = sources.clone();
        changed_limit[1].max_bytes -= 1;
        let mut changed_kind = sources.clone();
        changed_kind[1].kind = EvidenceSourceKind::File;
        let mut changed_field = fields.clone();
        changed_field.pop();
        for changed in [
            canonical_judge_input_scope_digest(
                Some("coding-agent-judge-v1"),
                &fields,
                &changed_identity,
                32 * 1024,
            )
            .unwrap(),
            canonical_judge_input_scope_digest(
                Some("coding-agent-judge-v1"),
                &fields,
                &changed_limit,
                32 * 1024,
            )
            .unwrap(),
            canonical_judge_input_scope_digest(
                Some("coding-agent-judge-v1"),
                &fields,
                &changed_kind,
                32 * 1024,
            )
            .unwrap(),
            canonical_judge_input_scope_digest(
                Some("coding-agent-judge-v1"),
                &changed_field,
                &sources,
                32 * 1024,
            )
            .unwrap(),
            canonical_judge_input_scope_digest(
                Some("coding-agent-judge-v1"),
                &fields,
                &sources,
                32 * 1024 - 1,
            )
            .unwrap(),
            canonical_judge_input_scope_digest(None, &fields, &sources, 32 * 1024).unwrap(),
        ] {
            assert_ne!(digest, changed);
        }
    }

    #[test]
    fn source_categories_have_an_explicit_assessment_field_mapping() {
        let sources = vec![
            AllowedEvidenceSource {
                kind: EvidenceSourceKind::IssueContext,
                identity: "issue:17".into(),
                max_bytes: 1024,
            },
            AllowedEvidenceSource {
                kind: EvidenceSourceKind::RepositoryContext,
                identity: "repo:forge/org/name".into(),
                max_bytes: 1024,
            },
            AllowedEvidenceSource {
                kind: EvidenceSourceKind::ThreadGroupContext,
                identity: "thread-group:17".into(),
                max_bytes: 1024,
            },
            AllowedEvidenceSource {
                kind: EvidenceSourceKind::File,
                identity: "repo:src/lib.rs".into(),
                max_bytes: 1024,
            },
            AllowedEvidenceSource {
                kind: EvidenceSourceKind::Diff,
                identity: "diff:attempt-1".into(),
                max_bytes: 1024,
            },
            AllowedEvidenceSource {
                kind: EvidenceSourceKind::Report,
                identity: "agent-report".into(),
                max_bytes: 16 * 1024,
            },
            AllowedEvidenceSource {
                kind: EvidenceSourceKind::ValidationResult,
                identity: "V-1".into(),
                max_bytes: 1024,
            },
        ];
        assert_eq!(
            judge_input_fields_for_sources(&sources),
            vec![
                ApprovedJudgeInputField::IssueTitle,
                ApprovedJudgeInputField::IssueBody,
                ApprovedJudgeInputField::RepositoryIdentity,
                ApprovedJudgeInputField::ThreadGroupContext,
                ApprovedJudgeInputField::BoundedFile,
                ApprovedJudgeInputField::BoundedDiff,
                ApprovedJudgeInputField::Report,
                ApprovedJudgeInputField::ValidationResult,
            ]
        );

        let repository_source = [AllowedEvidenceSource {
            kind: EvidenceSourceKind::RepositoryContext,
            identity: "repo:forge/org/name".into(),
            max_bytes: 1024,
        }];
        let approved_with_unmapped_summary = [
            ApprovedJudgeInputField::RepositoryIdentity,
            ApprovedJudgeInputField::RepositorySummary,
        ];
        let prepared_repository_fields = judge_input_fields_for_sources(&repository_source);
        assert_ne!(
            canonical_judge_input_scope_digest(
                Some("coding-agent-judge-v1"),
                &approved_with_unmapped_summary,
                &repository_source,
                4096,
            )
            .unwrap(),
            canonical_judge_input_scope_digest(
                Some("coding-agent-judge-v1"),
                &prepared_repository_fields,
                &repository_source,
                4096,
            )
            .unwrap(),
            "an approval-only field without a v1 source mapping must not be dropped"
        );
    }

    #[test]
    fn contract_rejects_ambiguous_parallel_policy_and_unsafe_validation_commands() {
        let mismatch = ApprovalAssessmentContract::from_issue_acceptance(
            RunPurpose::Implementation,
            vec!["criterion".into()],
            Vec::new(),
            Vec::new(),
            None,
            None,
            scope(),
        );
        assert!(mismatch.is_err());

        let unsafe_command = ApprovalAssessmentContract::from_issue_acceptance(
            RunPurpose::Implementation,
            Vec::new(),
            Vec::new(),
            vec![ApprovalValidationDraft {
                required: true,
                command: Some(ApprovalAssessmentCommand {
                    argv: vec!["cargo".into(), "test".into()],
                    working_directory: "/etc".into(),
                    permissions: vec![ValidationPermission::ReadWorkspace],
                    profile_digest: "a".repeat(64),
                    timeout_ms: 1000,
                }),
            }],
            None,
            None,
            scope(),
        );
        assert!(unsafe_command.is_err());

        let valid_command = ApprovalAssessmentContract::from_issue_acceptance(
            RunPurpose::Implementation,
            vec!["Check the implementation".into()],
            vec![ApprovalCriterionPolicy {
                required: true,
                method: CriterionEvaluationMethod::SemanticJudge,
            }],
            vec![ApprovalValidationDraft {
                required: true,
                command: Some(ApprovalAssessmentCommand {
                    argv: vec!["cargo".into(), "test".into(), "-p runner".into()],
                    working_directory: "/workspace/project".into(),
                    permissions: vec![ValidationPermission::ReadWorkspace],
                    profile_digest: "a".repeat(64),
                    timeout_ms: 30_000,
                }),
            }],
            Some(provider()),
            Some("coding-agent-judge-v1".into()),
            scope(),
        )
        .unwrap();
        assert!(valid_command.can_dispatch_semantic_assessment());
        assert_eq!(
            valid_command.validation_plan[0]
                .command
                .as_ref()
                .unwrap()
                .working_directory,
            "/workspace/project"
        );
        assert_eq!(valid_command.provider.as_ref().unwrap().model_version, "7b");

        let mut encoded = serde_json::to_value(
            ApprovalAssessmentContract::from_issue_acceptance(
                RunPurpose::Implementation,
                Vec::new(),
                Vec::new(),
                vec![ApprovalValidationDraft {
                    required: false,
                    command: None,
                }],
                None,
                None,
                scope(),
            )
            .unwrap(),
        )
        .unwrap();
        encoded["validation_plan"][0]
            .as_object_mut()
            .unwrap()
            .remove("command");
        assert!(serde_json::from_value::<ApprovalAssessmentContract>(encoded).is_err());

        let mut duplicate_scope = scope();
        duplicate_scope
            .allowed_fields
            .push(ApprovedJudgeInputField::Report);
        assert!(
            ApprovalAssessmentContract::from_issue_acceptance(
                RunPurpose::Implementation,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
                None,
                duplicate_scope,
            )
            .is_err()
        );
    }

    #[test]
    fn validation_working_directory_is_bounded_by_utf8_bytes() {
        fn guest_directory(byte_len: usize) -> String {
            let mut path = "/workspace".to_owned();
            while path.len() < byte_len {
                let remaining = byte_len - path.len();
                let component_len = (remaining - 1).min(200);
                path.push('/');
                path.push_str(&"a".repeat(component_len));
            }
            assert_eq!(path.len(), byte_len);
            path
        }

        let build_contract = |working_directory: String| {
            ApprovalAssessmentContract::from_issue_acceptance(
                RunPurpose::Implementation,
                Vec::new(),
                Vec::new(),
                vec![ApprovalValidationDraft {
                    required: true,
                    command: Some(ApprovalAssessmentCommand {
                        argv: vec!["cargo".into(), "test".into()],
                        working_directory,
                        permissions: vec![ValidationPermission::ReadWorkspace],
                        profile_digest: "a".repeat(64),
                        timeout_ms: 30_000,
                    }),
                }],
                None,
                None,
                scope(),
            )
        };

        let exact_limit_path = guest_directory(MAX_APPROVAL_TEXT_BYTES);
        assert!(build_contract(exact_limit_path).is_ok());

        let mut multibyte_path = guest_directory(MAX_APPROVAL_TEXT_BYTES - 1);
        multibyte_path.push('é');
        assert_eq!(multibyte_path.len(), MAX_APPROVAL_TEXT_BYTES + 1);
        assert!(build_contract(multibyte_path).is_err());
    }
}
