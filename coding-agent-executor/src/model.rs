use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 2;
pub const EXECUTOR_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MAX_SOURCE_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorRequest {
    pub protocol_version: u32,
    pub operation: Operation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    CheckRepository(CheckRepositoryRequest),
    ResolveBase(ResolveBaseRequest),
    Prepare(PrepareRequest),
    InspectSource(PrepareRequest),
    Capture(CaptureRequest),
    Seal(SealRequest),
    Resolve(ResolveRequest),
    Push(PushRequest),
    PushWorkspace(WorkspacePushRequest),
    CreatePullRequest(CreatePullRequestRequest),
    LookupPush(PushRequest),
    LookupPullRequest(CreatePullRequestRequest),
}

/// Performs read-only Git and forge checks for one trusted repository identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRepositoryRequest {
    pub repository: RepositoryId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<CheckRepositoryChecks>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRepositoryChecks {
    pub git_read: bool,
    pub forge_api: bool,
}

/// Publishes the observed commit without requiring a separately sealed artifact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePushRequest {
    pub source: PrepareRequest,
    pub expected_commit: String,
    pub source_digest: String,
}

/// Resolves an allowlisted remote base ref without creating a workspace.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveBaseRequest {
    pub run_id: String,
    pub attempt_id: String,
    pub operation_id: String,
    pub repository: RepositoryId,
    pub base_branch: String,
}

/// A host-resolved, immutable base commit bound to one native operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BaseResolutionReceipt {
    pub run_id: String,
    pub attempt_id: String,
    pub operation_id: String,
    pub repository: RepositoryId,
    pub base_branch: String,
    pub base_commit: String,
    pub trusted_config_digest: String,
    pub protocol_version: u32,
    pub executor_version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareRequest {
    #[serde(default)]
    pub issue_id: Option<String>,
    pub run_id: String,
    pub attempt_id: String,
    pub repository: RepositoryId,
    pub base_branch: String,
    pub base_commit: String,
}

impl PrepareRequest {
    pub fn expected_branch(&self) -> String {
        match self.issue_id.as_deref() {
            None => crate::expected_branch(&self.run_id),
            Some(issue_id) => {
                let normalized = issue_id
                    .chars()
                    .take(64)
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                            c
                        } else {
                            '-'
                        }
                    })
                    .collect::<String>();
                let normalized = normalized.trim_matches(['-', '_']);
                let normalized = if normalized.is_empty() {
                    "issue"
                } else {
                    normalized
                };
                format!("agent/issue-{normalized}-{}", self.run_id)
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    pub run_id: String,
    pub attempt_id: String,
    pub repository: RepositoryId,
    pub base_branch: String,
    pub base_commit: String,
    pub approved_snapshot_digest: String,
    pub scope_digest: String,
    pub intent_approval_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SealRequest {
    pub run_id: String,
    pub attempt_id: String,
    pub manifest_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    pub run_id: String,
    pub attempt_id: String,
    pub artifact_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PushRequest {
    pub run_id: String,
    pub attempt_id: String,
    pub repository: RepositoryId,
    pub branch: String,
    pub base_branch: String,
    pub base_commit: String,
    pub artifact_digest: String,
    pub frozen_commit: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreatePullRequestRequest {
    pub run_id: String,
    pub attempt_id: String,
    pub repository: RepositoryId,
    pub branch: String,
    pub base_branch: String,
    pub base_commit: String,
    pub artifact_digest: String,
    pub frozen_commit: String,
    pub pull_request: PullRequestRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequestRequest {
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryId {
    pub host: String,
    pub owner: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedConfig {
    pub protocol_version: u32,
    pub expected_executor_version: String,
    pub workspace_root: PathBuf,
    pub publisher_root: PathBuf,
    pub state_root: PathBuf,
    pub credential_root: PathBuf,
    pub max_import_bytes: u64,
    pub repositories: Vec<RepositoryConfig>,
    pub credential_sources: Vec<CredentialSourceConfig>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    pub id: RepositoryId,
    /// Host-managed clone URL. It is never accepted from an execution request.
    pub clone_url: String,
    /// Explicit host SSH directory; never inferred from the process home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_directory: Option<PathBuf>,
    pub allowed_base_branches: Vec<String>,
    pub read_credential_source: Option<String>,
    pub publish_credential_source: Option<String>,
    pub forge: Option<ForgeConfig>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeConfig {
    pub kind: ForgeKind,
    pub api_base_url: String,
    pub web_base_url: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeKind {
    #[serde(rename = "github")]
    GitHub,
    #[serde(rename = "gitea")]
    Gitea,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSourceConfig {
    pub id: String,
    pub username: String,
    /// Relative to `TrustedConfig::credential_root`; never serialized into requests.
    pub token_file: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorResult {
    RepositoryChecked(RepositoryCheckResult),
    BaseResolved(BaseResolutionReceipt),
    Prepared(PreparedWorkspace),
    SourceInspected(SourceInspection),
    Captured(CaptureReceipt),
    Sealed(SealedArtifactReceipt),
    Resolved(ResolvedArtifact),
    Pushed(PushReceipt),
    PullRequestCreated(PullRequestReceipt),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryCheckResult {
    pub repository: RepositoryId,
    pub trusted_config_digest: String,
    pub protocol_version: u32,
    pub executor_version: String,
    pub stages: Vec<RepositoryCheckStage>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryCheckStage {
    pub stage: String,
    pub status: String,
    pub code: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceInspection {
    pub run_id: String,
    pub attempt_id: String,
    pub base_commit: String,
    pub head_commit: String,
    pub branch: String,
    pub tracked_diff: Vec<u8>,
    #[serde(default)]
    pub uncommitted_diff: Option<Vec<u8>>,
    pub new_files: Vec<SourceFile>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub path: String,
    pub is_symlink: bool,
    pub content: Vec<u8>,
}

/// Describes the host-private storage boundary only, not guest-writer quiescence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureGuarantee {
    HostIsolatedAfterSeal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureReceipt {
    pub run_id: String,
    pub attempt_id: String,
    pub repository: RepositoryId,
    pub base_branch: String,
    pub base_commit: String,
    pub approved_snapshot_digest: String,
    pub scope_digest: String,
    pub intent_approval_digest: String,
    pub manifest_digest: String,
    pub finalized_bytes_digest: String,
    pub file_count: u64,
    pub byte_count: u64,
    pub capture_guarantee: CaptureGuarantee,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SealedArtifactReceipt {
    pub run_id: String,
    pub attempt_id: String,
    pub repository: RepositoryId,
    pub base_branch: String,
    pub base_commit: String,
    pub approved_snapshot_digest: String,
    pub scope_digest: String,
    pub intent_approval_digest: String,
    pub manifest_digest: String,
    pub finalized_bytes_digest: String,
    pub tree: String,
    pub commit: String,
    pub artifact_digest: String,
    pub protocol_version: u32,
    pub executor_version: String,
    pub capture_guarantee: CaptureGuarantee,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedArtifact {
    pub run_id: String,
    pub attempt_id: String,
    pub repository: RepositoryId,
    pub branch: String,
    pub base_branch: String,
    pub base_commit: String,
    pub approved_snapshot_digest: String,
    pub scope_digest: String,
    pub intent_approval_digest: String,
    pub manifest_digest: String,
    pub finalized_bytes_digest: String,
    pub tree: String,
    pub commit: String,
    pub artifact_digest: String,
    pub protocol_version: u32,
    pub executor_version: String,
    pub capture_guarantee: CaptureGuarantee,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedWorkspace {
    pub run_id: String,
    pub attempt_id: String,
    pub repository: RepositoryId,
    pub base_branch: String,
    pub base_commit: String,
    pub branch: String,
    pub workspace_path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PushReceipt {
    pub run_id: String,
    pub attempt_id: String,
    pub repository: RepositoryId,
    pub branch: String,
    pub commit: String,
    pub remote_head: String,
    pub artifact_digest: String,
    pub already_present: bool,
    pub secret_scan: SecretScanStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretScanStatus {
    CleanSupportedFormatsV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequestReceipt {
    pub run_id: String,
    pub attempt_id: String,
    pub repository: RepositoryId,
    pub number: u64,
    pub url: String,
    pub head_commit: String,
    pub artifact_digest: String,
    pub base_branch: String,
    pub already_present: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorErrorCode {
    InvalidRequest,
    IncompatibleProtocol,
    IncompatibleExecutor,
    InvalidConfiguration,
    InvalidRunId,
    InvalidCommit,
    UnsupportedBaseTree,
    InvalidBranch,
    BranchNotApproved,
    RepositoryNotApproved,
    BaseBranchNotApproved,
    IdempotencyConflict,
    UnsafeWorkspace,
    WorkspaceNotPrepared,
    WorkspaceAlreadyExists,
    ArtifactNotSealed,
    ArtifactMismatch,
    ArtifactIntegrityFailure,
    ArtifactTooLarge,
    GitOperationFailed,
    CommitNotBasedOnApprovedBase,
    NoChangesToPublish,
    RemoteBranchConflict,
    SecretDetected,
    SecretScanFailed,
    PullRequestNotApproved,
    PullRequestConflict,
    ForgeUnavailable,
    ForgeRejected,
    ResponseTooLarge,
    IoFailure,
}

impl fmt::Display for ExecutorErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRequest => "invalid_request",
            Self::IncompatibleProtocol => "incompatible_protocol",
            Self::IncompatibleExecutor => "incompatible_executor",
            Self::InvalidConfiguration => "invalid_configuration",
            Self::InvalidRunId => "invalid_run_id",
            Self::InvalidCommit => "invalid_commit",
            Self::UnsupportedBaseTree => "unsupported_base_tree",
            Self::InvalidBranch => "invalid_branch",
            Self::BranchNotApproved => "branch_not_approved",
            Self::RepositoryNotApproved => "repository_not_approved",
            Self::BaseBranchNotApproved => "base_branch_not_approved",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::UnsafeWorkspace => "unsafe_workspace",
            Self::WorkspaceNotPrepared => "workspace_not_prepared",
            Self::WorkspaceAlreadyExists => "workspace_already_exists",
            Self::ArtifactNotSealed => "artifact_not_sealed",
            Self::ArtifactMismatch => "artifact_mismatch",
            Self::ArtifactIntegrityFailure => "artifact_integrity_failure",
            Self::ArtifactTooLarge => "artifact_too_large",
            Self::GitOperationFailed => "git_operation_failed",
            Self::CommitNotBasedOnApprovedBase => "commit_not_based_on_approved_base",
            Self::NoChangesToPublish => "no_changes_to_publish",
            Self::RemoteBranchConflict => "remote_branch_conflict",
            Self::SecretDetected => "secret_detected",
            Self::SecretScanFailed => "secret_scan_failed",
            Self::PullRequestNotApproved => "pull_request_not_approved",
            Self::PullRequestConflict => "pull_request_conflict",
            Self::ForgeUnavailable => "forge_unavailable",
            Self::ForgeRejected => "forge_rejected",
            Self::ResponseTooLarge => "response_too_large",
            Self::IoFailure => "io_failure",
        })
    }
}

impl ExecutorErrorCode {
    pub fn public_message(self) -> &'static str {
        match self {
            Self::InvalidRequest => "The request is invalid.",
            Self::IncompatibleProtocol => "The request protocol version is not supported.",
            Self::IncompatibleExecutor => {
                "The executor version does not match trusted configuration."
            }
            Self::InvalidConfiguration => "Trusted executor configuration is invalid.",
            Self::InvalidRunId => "The run identifier is invalid.",
            Self::InvalidCommit => "A full commit identifier is required.",
            Self::UnsupportedBaseTree => {
                "The approved base contains unsupported tracked entry types."
            }
            Self::InvalidBranch => "The requested branch name is invalid.",
            Self::BranchNotApproved => "The branch is not the run's approved branch.",
            Self::RepositoryNotApproved => "The repository is not in the trusted allowlist.",
            Self::BaseBranchNotApproved => "The base branch is not in the trusted allowlist.",
            Self::IdempotencyConflict => {
                "The run identifier was already used for a different request."
            }
            Self::UnsafeWorkspace => "The workspace Git metadata is unsafe or unsupported.",
            Self::WorkspaceNotPrepared => "The run has no completed trusted workspace preparation.",
            Self::WorkspaceAlreadyExists => {
                "The run workspace already exists without a matching receipt."
            }
            Self::ArtifactNotSealed => "The attempt has no complete sealed artifact receipt.",
            Self::ArtifactMismatch => "The artifact selector does not match the sealed receipt.",
            Self::ArtifactIntegrityFailure => "The sealed artifact failed host integrity checks.",
            Self::ArtifactTooLarge => {
                "A workspace, Git tree, or artifact exceeds an executor safety limit."
            }
            Self::GitOperationFailed => "A required Git operation failed.",
            Self::CommitNotBasedOnApprovedBase => {
                "The frozen commit is not based on the approved base commit."
            }
            Self::NoChangesToPublish => {
                "The frozen commit contains no change from the approved base."
            }
            Self::RemoteBranchConflict => "The remote run branch points to a different commit.",
            Self::SecretDetected => {
                "The frozen commit contains a detected secret; publication was blocked."
            }
            Self::SecretScanFailed => {
                "The frozen commit could not be completely scanned; publication was blocked."
            }
            Self::PullRequestNotApproved => {
                "Pull-request creation is not configured for this repository."
            }
            Self::PullRequestConflict => {
                "An existing pull request does not match the approved run."
            }
            Self::ForgeUnavailable => {
                "The configured forge could not be reconciled; retry may be safe."
            }
            Self::ForgeRejected => "The configured forge rejected the operation.",
            Self::ResponseTooLarge => "A forge response exceeded the configured safety limit.",
            Self::IoFailure => "A required host-side file operation failed.",
        }
    }

    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::ForgeUnavailable | Self::IoFailure | Self::GitOperationFailed
        )
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("{code}")]
pub struct ExecutorError {
    pub code: ExecutorErrorCode,
}

impl ExecutorError {
    pub const fn new(code: ExecutorErrorCode) -> Self {
        Self { code }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutorResponse {
    pub protocol_version: u32,
    #[serde(flatten)]
    pub outcome: ResponseOutcome,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseOutcome {
    Ok { result: Box<ExecutorResult> },
    Error { error: PublicError },
}

#[derive(Clone, Debug, Serialize)]
pub struct PublicError {
    pub code: ExecutorErrorCode,
    pub message: &'static str,
    pub retryable: bool,
}

impl From<ExecutorError> for PublicError {
    fn from(error: ExecutorError) -> Self {
        Self {
            code: error.code,
            message: error.code.public_message(),
            retryable: error.code.retryable(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CapabilityDocument {
    pub protocol_version: u32,
    pub executor_version: &'static str,
    pub features: Vec<&'static str>,
}

impl CapabilityDocument {
    pub fn current() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            executor_version: EXECUTOR_VERSION,
            features: vec![
                "resolve_base",
                "check_repository",
                "inspect_source",
                "prepare",
                "capture",
                "seal",
                "resolve",
                "artifact_only_publication",
                "push",
                "push_workspace",
                "lookup_publication",
                "pull_request",
                "secret_scan_supported_formats_v1",
            ],
        }
    }
}

pub(crate) fn invalid_configuration() -> ExecutorError {
    ExecutorError::new(ExecutorErrorCode::InvalidConfiguration)
}
