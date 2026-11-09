//! Independent native state and immutable context for interactive agent sessions.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::coding_agent::{
    AgentProfileSnapshot, ProfileError, RepositoryError, RepositoryIdentity, ResolvedRepository,
    is_valid_branch_name,
};
use coding_agent_executor::{BaseResolutionReceipt, PreparedWorkspace, RepositoryId};

pub mod authority;
pub mod bootstrap;
pub mod doctor;
pub mod doctor_store;
pub mod launch;
pub mod manager;
pub mod output;
pub mod preparation;
pub mod profile_settings;
pub mod registry;
pub mod store;
pub mod wire;
pub use authority::ManagedNativeSessionAuthority;
pub use doctor::{
    NativeCliProbeObservation, NativeCliProbeResult, NativeCliProbeValidationError,
    NativeRuntimeDoctor, NativeRuntimeDoctorError, NativeSourceInspection, ObservedRuntimeLease,
    validate_native_cli_probe_result,
};
pub use doctor_store::{
    NativeProbeCancelClaim, NativeProbeEvidence, NativeProbeIntent, NativeProbeOperationState,
    NativeRuntimeDoctorStore, NativeRuntimeDoctorStoreError,
};
pub use manager::{
    NativeAuthorityError, NativeAuthoritySnapshot, NativeSessionAuthority,
    NativeTerminalAttachment, NativeTerminalManager, NativeTerminalManagerError,
    NativeTerminalSubscription,
};
pub use output::{
    NativeTerminalOutputError, NativeTerminalOutputEvent, NativeTerminalOutputRecord,
    NativeTerminalOutputStore,
};
pub use preparation::{
    NativePreparationAuthority, NativePreparationError, NativePreparationRuntime,
    NativeSessionPreparer,
};
pub use registry::{
    NativeRuntimeRegistry, NativeRuntimeRegistryError, NativeTerminalAttachResult,
    NativeTerminalOutputSink,
};
pub use store::{NativeTerminalStore, NativeTerminalStoreError};
pub use wire::{
    NativeTerminalAttachArgs, NativeTerminalAttachRequest, NativeTerminalAttachmentWire,
    NativeTerminalDetachArgs, NativeTerminalDetachRequest, NativeTerminalEventWire,
    NativeTerminalInputArgs, NativeTerminalInputDeliveryWire, NativeTerminalInputRequest,
    NativeTerminalOutputArgs, NativeTerminalOutputEventWire, NativeTerminalOutputPageWire,
    NativeTerminalOutputRecordWire, NativeTerminalOutputRequest, NativeTerminalPrepareArgs,
    NativeTerminalPrepareRequest, NativeTerminalPreviewWire, NativeTerminalProfileWire,
    NativeTerminalSessionArgs, NativeTerminalSessionRequest, NativeTerminalSessionStateWire,
    NativeTerminalSessionWire, NativeTerminalStartArgs, NativeTerminalStartRequest,
    NativeTerminalStatusWire,
};

pub const MAX_NATIVE_PROMPT_BYTES: usize = 64 * 1024;
pub const MAX_NATIVE_CONTEXT_BYTES: usize = 256 * 1024;
pub const MAX_NATIVE_PREVIEW_BYTES: usize = 128 * 1024;

const MAX_ISSUE_ID_BYTES: usize = 512;
const MAX_ISSUE_FIELD_BYTES: usize = 64 * 1024;
const MAX_ISSUE_CRITERIA: usize = 128;
const MAX_OWNER_LABEL_BYTES: usize = 256;
const CONTEXT_DIGEST_DOMAIN: &[u8] = b"lookback/native-terminal/session-context/v1\0";
const PREVIEW_DIGEST_DOMAIN: &[u8] = b"lookback/native-terminal/preview/v1\0";

/// Canonical Issue data copied as immutable session context without joining criteria.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanonicalIssueSnapshot {
    issue_id: String,
    revision: u64,
    title: String,
    description: String,
    acceptance_criteria: Vec<String>,
}

impl CanonicalIssueSnapshot {
    pub fn new(
        issue_id: impl Into<String>,
        revision: u64,
        title: impl Into<String>,
        description: impl Into<String>,
        acceptance_criteria: Vec<String>,
    ) -> Result<Self, NativeContextError> {
        let issue = Self {
            issue_id: issue_id.into(),
            revision,
            title: title.into(),
            description: description.into(),
            acceptance_criteria,
        };
        issue.validate()?;
        Ok(issue)
    }

    pub fn issue_id(&self) -> &str {
        &self.issue_id
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn acceptance_criteria(&self) -> &[String] {
        &self.acceptance_criteria
    }

    fn validate(&self) -> Result<(), NativeContextError> {
        if self.issue_id.is_empty()
            || self.issue_id.len() > MAX_ISSUE_ID_BYTES
            || self.issue_id.chars().any(char::is_control)
            || self.revision == 0
            || self.title.len() > MAX_ISSUE_FIELD_BYTES
            || self.description.len() > MAX_ISSUE_FIELD_BYTES
            || self.acceptance_criteria.len() > MAX_ISSUE_CRITERIA
            || self
                .acceptance_criteria
                .iter()
                .any(|criterion| criterion.len() > MAX_ISSUE_FIELD_BYTES)
        {
            return Err(NativeContextError::InvalidContext);
        }
        Ok(())
    }
}

/// Snapshot of the actual registered worker identity and safe configuration pin.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeWorkerBinding {
    pub worker_id: i64,
    pub runner_id: i64,
    pub worker_name: String,
    pub settings_digest: String,
    pub image_digest: String,
}

impl NativeWorkerBinding {
    fn validate(&self, expected_image_digest: &str) -> Result<(), NativeContextError> {
        if self.worker_id <= 0
            || self.runner_id <= 0
            || self.worker_name.is_empty()
            || self.worker_name.len() > MAX_OWNER_LABEL_BYTES
            || self.worker_name.chars().any(char::is_control)
            || !is_sha256(&self.settings_digest)
            || self.image_digest != expected_image_digest
        {
            return Err(NativeContextError::InvalidContext);
        }
        Ok(())
    }
}

/// A renderer-facing immutable launch preview; ownership remains a native-store fact.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeTerminalPreview {
    preview_id: String,
    preview_digest: String,
    expected_settings_revision: u64,
    expected_issue_revision: u64,
    canonical_repository: Option<RepositoryIdentity>,
    profile_id: String,
    expected_cli_version: String,
    safe_image_digest: String,
    worker_binding: Option<NativeWorkerBinding>,
    command: String,
    args: Vec<String>,
    working_directory: String,
    initial_prompt_digest: String,
}

impl std::fmt::Debug for NativeTerminalPreview {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeTerminalPreview")
            .field("preview_id", &self.preview_id)
            .field("preview_digest", &self.preview_digest)
            .field(
                "expected_settings_revision",
                &self.expected_settings_revision,
            )
            .field("expected_issue_revision", &self.expected_issue_revision)
            .field("canonical_repository", &self.canonical_repository)
            .field("profile_id", &self.profile_id)
            .field("expected_cli_version", &self.expected_cli_version)
            .field("safe_image_digest", &self.safe_image_digest)
            .field("worker_binding", &self.worker_binding)
            .field("command", &self.command)
            .field("argument_count", &self.args.len())
            .field("working_directory", &self.working_directory)
            .field("initial_prompt_digest", &self.initial_prompt_digest)
            .finish()
    }
}

impl NativeTerminalPreview {
    #[allow(clippy::too_many_arguments)]
    pub fn from_launch(
        preview_id: impl Into<String>,
        expected_settings_revision: u64,
        expected_issue_revision: u64,
        canonical_repository: Option<RepositoryIdentity>,
        launch_plan: &launch::NativeTerminalLaunchPlan,
        worker_binding: Option<NativeWorkerBinding>,
    ) -> Result<Self, NativeContextError> {
        launch_plan.validate()?;
        let preview_id = preview_id.into();
        if !is_canonical_uuid(&preview_id) || expected_issue_revision == 0 {
            return Err(NativeContextError::InvalidContext);
        }
        if let Some(repository) = canonical_repository.as_ref() {
            repository
                .validate()
                .map_err(|_| NativeContextError::InvalidContext)?;
        }
        if let Some(binding) = worker_binding.as_ref() {
            binding.validate(launch_plan.image_digest())?;
        }
        let mut preview = Self {
            preview_id,
            preview_digest: String::new(),
            expected_settings_revision,
            expected_issue_revision,
            canonical_repository,
            profile_id: launch_plan.profile_id().to_owned(),
            expected_cli_version: launch_plan.expected_cli_version().to_owned(),
            safe_image_digest: launch_plan.image_digest().to_owned(),
            worker_binding,
            command: launch_plan.command().to_owned(),
            args: launch_plan.args().to_vec(),
            working_directory: launch_plan.working_directory().to_owned(),
            initial_prompt_digest: launch_plan.initial_prompt().digest().to_owned(),
        };
        preview.preview_digest = preview.compute_digest()?;
        preview.validate()?;
        Ok(preview)
    }

    pub fn preview_id(&self) -> &str {
        &self.preview_id
    }

    pub fn preview_digest(&self) -> &str {
        &self.preview_digest
    }

    pub fn expected_settings_revision(&self) -> u64 {
        self.expected_settings_revision
    }

    pub fn expected_issue_revision(&self) -> u64 {
        self.expected_issue_revision
    }

    pub fn canonical_repository(&self) -> Option<&RepositoryIdentity> {
        self.canonical_repository.as_ref()
    }

    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    pub fn expected_cli_version(&self) -> &str {
        &self.expected_cli_version
    }

    pub fn safe_image_digest(&self) -> &str {
        &self.safe_image_digest
    }

    pub fn worker_binding(&self) -> Option<&NativeWorkerBinding> {
        self.worker_binding.as_ref()
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn working_directory(&self) -> &str {
        &self.working_directory
    }

    pub fn initial_prompt_digest(&self) -> &str {
        &self.initial_prompt_digest
    }

    pub(crate) fn validate(&self) -> Result<(), NativeContextError> {
        launch::validate_preview_launch_fields(
            &self.command,
            &self.args,
            &self.working_directory,
            &self.initial_prompt_digest,
        )
        .map_err(|_| NativeContextError::InvalidContext)?;
        if !is_canonical_uuid(&self.preview_id)
            || self.expected_issue_revision == 0
            || self.profile_id.is_empty()
            || self.profile_id.len() > MAX_OWNER_LABEL_BYTES
            || self.profile_id.chars().any(char::is_control)
            || !launch::NativeAgentKind::from_program(&self.command)
                .is_some_and(|_| launch::cli_version_hint_valid(&self.expected_cli_version))
            || !is_image_digest(&self.safe_image_digest)
            || self.working_directory.is_empty()
            || self.working_directory.len() > 4096
            || self.working_directory.chars().any(char::is_control)
            || !is_sha256(&self.initial_prompt_digest)
            || !is_sha256(&self.preview_digest)
            || self.compute_digest()? != self.preview_digest
        {
            return Err(NativeContextError::InvalidContext);
        }
        if let Some(repository) = self.canonical_repository.as_ref() {
            repository
                .validate()
                .map_err(|_| NativeContextError::InvalidContext)?;
        }
        if let Some(binding) = self.worker_binding.as_ref() {
            binding.validate(&self.safe_image_digest)?;
        }
        let bytes = serde_json::to_vec(self).map_err(|_| NativeContextError::InvalidContext)?;
        if bytes.len() > MAX_NATIVE_PREVIEW_BYTES {
            return Err(NativeContextError::InvalidContext);
        }
        Ok(())
    }

    fn compute_digest(&self) -> Result<String, NativeContextError> {
        let mut unsigned = self.clone();
        unsigned.preview_digest.clear();
        let bytes =
            serde_json::to_vec(&unsigned).map_err(|_| NativeContextError::InvalidContext)?;
        Ok(domain_digest(PREVIEW_DIGEST_DOMAIN, &bytes))
    }
}

/// Trusted inputs materialized after repository, base and workspace preparation.
pub struct NativeSessionContextInput {
    pub session_id: String,
    pub attempt_id: String,
    pub owner_window_label: String,
    pub canonical_issue: CanonicalIssueSnapshot,
    pub repository: ResolvedRepository,
    pub base_resolution_receipt: BaseResolutionReceipt,
    pub prepared_workspace: PreparedWorkspace,
    pub profile: AgentProfileSnapshot,
    pub cli_profile: launch::NativeCliProfile,
    pub workspace_mount_binding: launch::WorkspaceMountBinding,
    pub launch_plan: launch::NativeTerminalLaunchPlan,
    pub preview: NativeTerminalPreview,
}

/// One native interactive session. It intentionally has no AgentRun/workflow outcome fields.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeSessionContext {
    session_id: String,
    attempt_id: String,
    owner_window_label: String,
    canonical_issue: CanonicalIssueSnapshot,
    repository: ResolvedRepository,
    base_resolution_receipt: BaseResolutionReceipt,
    prepared_workspace: PreparedWorkspace,
    profile: AgentProfileSnapshot,
    cli_profile: launch::NativeCliProfile,
    workspace_mount_binding: launch::WorkspaceMountBinding,
    launch_plan: launch::NativeTerminalLaunchPlan,
    preview: NativeTerminalPreview,
}

impl std::fmt::Debug for NativeSessionContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeSessionContext")
            .field("session_id", &self.session_id)
            .field("attempt_id", &self.attempt_id)
            .field("owner_window_label", &self.owner_window_label)
            .field("issue_id", &self.canonical_issue.issue_id())
            .field("issue_revision", &self.canonical_issue.revision())
            .field("repository", &self.repository.identity)
            .field("profile_id", &self.profile.profile_id)
            .field("preview_id", &self.preview.preview_id())
            .field(
                "initial_prompt_digest",
                &self.launch_plan.initial_prompt().digest(),
            )
            .finish()
    }
}

impl NativeSessionContext {
    pub fn new(input: NativeSessionContextInput) -> Result<Self, NativeContextError> {
        let context = Self {
            session_id: input.session_id,
            attempt_id: input.attempt_id,
            owner_window_label: input.owner_window_label,
            canonical_issue: input.canonical_issue,
            repository: input.repository,
            base_resolution_receipt: input.base_resolution_receipt,
            prepared_workspace: input.prepared_workspace,
            profile: input.profile,
            cli_profile: input.cli_profile,
            workspace_mount_binding: input.workspace_mount_binding,
            launch_plan: input.launch_plan,
            preview: input.preview,
        };
        context.validate_with_path_check(true)?;
        Ok(context)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    pub fn owner_window_label(&self) -> &str {
        &self.owner_window_label
    }

    pub fn canonical_issue(&self) -> &CanonicalIssueSnapshot {
        &self.canonical_issue
    }

    pub fn repository(&self) -> &ResolvedRepository {
        &self.repository
    }

    pub fn base_resolution_receipt(&self) -> &BaseResolutionReceipt {
        &self.base_resolution_receipt
    }

    pub fn prepared_workspace(&self) -> &PreparedWorkspace {
        &self.prepared_workspace
    }

    pub fn profile(&self) -> &AgentProfileSnapshot {
        &self.profile
    }

    pub fn cli_profile(&self) -> &launch::NativeCliProfile {
        &self.cli_profile
    }

    pub fn workspace_mount_binding(&self) -> &launch::WorkspaceMountBinding {
        &self.workspace_mount_binding
    }

    pub fn launch_plan(&self) -> &launch::NativeTerminalLaunchPlan {
        &self.launch_plan
    }

    pub fn preview(&self) -> &NativeTerminalPreview {
        &self.preview
    }

    pub fn initial_prompt(&self) -> &launch::NativeInitialPrompt {
        self.launch_plan.initial_prompt()
    }

    pub fn digest(&self) -> Result<String, NativeContextError> {
        let bytes = serde_json::to_vec(self).map_err(|_| NativeContextError::InvalidContext)?;
        if bytes.len() > MAX_NATIVE_CONTEXT_BYTES {
            return Err(NativeContextError::InvalidContext);
        }
        Ok(domain_digest(CONTEXT_DIGEST_DOMAIN, &bytes))
    }

    pub(crate) fn validate(&self) -> Result<(), NativeContextError> {
        self.validate_with_path_check(false)
    }

    fn validate_with_path_check(&self, check_filesystem: bool) -> Result<(), NativeContextError> {
        if !is_canonical_uuid(&self.session_id)
            || !is_canonical_uuid(&self.attempt_id)
            || self.owner_window_label.is_empty()
            || self.owner_window_label.len() > MAX_OWNER_LABEL_BYTES
            || self.owner_window_label.chars().any(char::is_control)
            || self.session_id != self.base_resolution_receipt.run_id
            || self.session_id != self.prepared_workspace.run_id
            || self.attempt_id != self.base_resolution_receipt.attempt_id
            || self.attempt_id != self.prepared_workspace.attempt_id
            || self.canonical_issue.revision() != self.preview.expected_issue_revision
            || self.preview.profile_id() != self.profile.profile_id
            || self.preview.canonical_repository() != Some(&self.repository.identity)
            || self.preview.expected_cli_version() != self.cli_profile.cli_version
            || self.preview.safe_image_digest() != self.profile.image_digest
            || !self
                .profile
                .allowed_repositories
                .contains(&self.repository.identity)
        {
            return Err(NativeContextError::InvalidContext);
        }
        self.canonical_issue.validate()?;
        self.profile.validate_snapshot()?;
        self.repository.validate()?;
        self.preview.validate()?;
        validate_prepared_receipts(
            &self.repository,
            &self.base_resolution_receipt,
            &self.prepared_workspace,
            &self.session_id,
            &self.attempt_id,
        )?;

        let rebuild = if check_filesystem {
            launch::build_launch_plan
        } else {
            launch::rebuild_stored_launch_plan
        };
        let rebuilt = rebuild(
            &self.profile,
            &self.cli_profile,
            &self.prepared_workspace,
            &self.workspace_mount_binding,
            &self.canonical_issue,
            self.launch_plan
                .initial_prompt()
                .edited()
                .then(|| self.launch_plan.initial_prompt().text()),
        )?;
        if rebuilt != self.launch_plan {
            return Err(NativeContextError::InvalidContext);
        }
        let expected_preview = NativeTerminalPreview::from_launch(
            self.preview.preview_id.clone(),
            self.preview.expected_settings_revision,
            self.preview.expected_issue_revision,
            Some(self.repository.identity.clone()),
            &self.launch_plan,
            self.preview.worker_binding.clone(),
        )?;
        if expected_preview != self.preview {
            return Err(NativeContextError::InvalidContext);
        }
        let bytes = serde_json::to_vec(self).map_err(|_| NativeContextError::InvalidContext)?;
        if bytes.len() > MAX_NATIVE_CONTEXT_BYTES {
            return Err(NativeContextError::InvalidContext);
        }
        Ok(())
    }
}

/// Durable session lifecycle is not an Issue workflow outcome.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Running,
    Closed,
    Cancelled,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeTerminalObservation {
    pub exit_code: Option<i32>,
    pub end_received: bool,
    pub stream_eof: bool,
    pub stream_error: bool,
}

#[derive(Clone, Debug)]
pub enum StartClaimOutcome {
    Claimed(NativeSessionRecord),
    Existing(NativeSessionRecord),
}

#[derive(Clone, Debug)]
pub struct NativeSessionRecord {
    pub session_id: String,
    pub attempt_id: String,
    pub preview_id: String,
    pub preview_digest: String,
    pub context_digest: String,
    pub profile_id: String,
    pub expected_settings_revision: u64,
    pub expected_issue_revision: u64,
    pub native_job_id: Option<i64>,
    pub state: SessionState,
    pub exit_code: Option<i32>,
    pub end_received: bool,
    pub stream_eof: bool,
    pub stream_error: bool,
    pub cancel_requested: bool,
    pub context: NativeSessionContext,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobAcceptance {
    pub cancel_dispatch_job_id: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCancelClaim {
    Dispatch { job_id: i64 },
    PendingAcceptance,
    AlreadyRequested,
    Terminal,
}

#[derive(Debug, Error)]
pub enum NativeContextError {
    #[error("native terminal context is invalid")]
    InvalidContext,
    #[error("native terminal launch plan is invalid")]
    Launch(#[from] launch::NativeLaunchError),
    #[error("native terminal profile is invalid")]
    Profile(#[from] ProfileError),
    #[error("native terminal repository is invalid")]
    Repository(#[from] RepositoryError),
}

fn validate_prepared_receipts(
    repository: &ResolvedRepository,
    base: &BaseResolutionReceipt,
    prepared: &PreparedWorkspace,
    session_id: &str,
    attempt_id: &str,
) -> Result<(), NativeContextError> {
    let expected_repository = RepositoryId {
        host: repository.identity.host.clone(),
        owner: repository.identity.organization.clone(),
        name: repository.identity.repository.clone(),
    };
    if base.repository != expected_repository
        || prepared.repository != expected_repository
        || base.protocol_version != coding_agent_executor::PROTOCOL_VERSION
        || base.base_branch != prepared.base_branch
        || base.base_commit != prepared.base_commit
        || !is_full_commit_id(&base.base_commit)
        || !is_sha256(&base.trusted_config_digest)
        || !is_canonical_uuid(&base.operation_id)
        || base.executor_version.is_empty()
        || base.executor_version.len() > 128
        || base.executor_version.chars().any(char::is_control)
        || !is_valid_branch_name(&base.base_branch)
        || !is_valid_branch_name(&prepared.branch)
        || session_id != base.run_id
        || session_id != prepared.run_id
        || attempt_id != base.attempt_id
        || attempt_id != prepared.attempt_id
    {
        return Err(NativeContextError::InvalidContext);
    }
    Ok(())
}

pub(crate) fn is_canonical_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
        && bytes[14].is_ascii_digit()
        && matches!(bytes[14], b'1'..=b'8')
        && matches!(bytes[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b')
        && value == value.to_ascii_lowercase()
}

pub(crate) fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn is_image_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn is_full_commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn domain_digest(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}
