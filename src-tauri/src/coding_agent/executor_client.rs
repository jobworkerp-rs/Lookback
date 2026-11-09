use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use coding_agent_executor::{
    BaseResolutionReceipt, CaptureGuarantee, CaptureReceipt, CaptureRequest, CheckRepositoryChecks,
    CheckRepositoryRequest, CreatePullRequestRequest, EXECUTOR_VERSION, ExecutorErrorCode,
    ExecutorRequest, ExecutorResult, ForgeKind, Operation, PROTOCOL_VERSION, PreparedWorkspace,
    PullRequestReceipt, PushReceipt, PushRequest, RepositoryCheckResult, RepositoryConfig,
    RepositoryId, ResolveBaseRequest, ResolveRequest, ResolvedArtifact, SealRequest,
    SealedArtifactReceipt, TrustedConfig, WorkspacePushRequest,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

const MAX_EXECUTOR_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(150);
const TERMINATION_GRACE: Duration = Duration::from_secs(1);
const VERIFIED_NATIVE_FEATURES: &[&str] = &[
    "check_repository",
    "resolve_base",
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
];
const CORE_NATIVE_FEATURES: &[&str] = &["prepare"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VerifiedNativeCapabilities {
    executor_version: String,
    protocol_version: u32,
    features: BTreeSet<String>,
    private_config_binding: String,
    executable_path: PathBuf,
    executable_sha256: String,
}

impl VerifiedNativeCapabilities {
    pub(crate) fn protocol_version(&self) -> u32 {
        self.protocol_version
    }

    pub(crate) fn executor_version(&self) -> &str {
        &self.executor_version
    }

    pub(crate) fn has_feature(&self, feature: &str) -> bool {
        self.features.contains(feature)
    }

    pub(crate) fn feature_set(&self) -> &BTreeSet<String> {
        &self.features
    }
}

/// Non-secret allowlist metadata used to bind recovery to approved endpoints.
/// Token contents and token paths never leave the checked native client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeRepositoryPublicationConfig {
    pub(crate) clone_url: String,
    pub(crate) api_base_url: Option<String>,
    pub(crate) allowed_base_branches: BTreeSet<String>,
    pub(crate) forge_kind: Option<ForgeKind>,
    pub(crate) web_base_url: Option<String>,
    pub(crate) read_credential_reference: Option<String>,
    pub(crate) publication_credential_reference: Option<String>,
}

/// Native subprocess transport errors never include child stderr or request contents.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum NativeExecutorError {
    #[error("The approved executor package is invalid.")]
    InvalidPackage,
    #[error("Trusted executor configuration is invalid or changed.")]
    InvalidTrustedConfiguration,
    #[error("The executor does not support the required protocol and operations.")]
    IncompatibleProtocol,
    #[error("The executor does not advertise the optional capability `{capability}`.")]
    UnsupportedCapability { capability: &'static str },
    #[error("The executor could not be started.")]
    Unavailable,
    #[error("The executor rejected the operation: {0}.")]
    ExecutorRejected(ExecutorErrorCode),
    #[error("The repository diagnostic transport failed: {reason}.")]
    DiagnosticTransport {
        reason: NativeDiagnosticTransportReason,
    },
    #[error("The request is invalid.")]
    InvalidRequest,
    #[error("The result of {operation} is unknown; reconcile it before retrying.")]
    OutcomeUnknown { operation: &'static str },
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativeDiagnosticTransportReason {
    #[error("timeout")]
    Timeout,
    #[error("response too large")]
    ResponseTooLarge,
    #[error("unavailable")]
    Unavailable,
}

pub(crate) struct PendingWorkspacePush {
    request: WorkspacePushRequest,
    stream: crate::jobworkerp::DispatchStream,
}

impl PendingWorkspacePush {
    pub(crate) fn parent_job_id(&self) -> Option<i64> {
        self.stream.job_id.as_ref().map(|id| id.value)
    }

    pub(crate) async fn finish(self) -> Result<PushReceipt, NativeExecutorError> {
        let output = super::preparation_workflow::finish_workflow(self.stream, 150).await?;
        let ExecutorResult::Pushed(receipt) = decode_response(&output.output, "push_workspace")?
        else {
            return Err(outcome_unknown("push_workspace"));
        };
        let branch = self.request.source.expected_branch();
        let expected = PushRequest {
            run_id: self.request.source.run_id,
            attempt_id: self.request.source.attempt_id,
            branch,
            repository: self.request.source.repository,
            base_branch: self.request.source.base_branch,
            base_commit: self.request.source.base_commit,
            artifact_digest: self.request.source_digest,
            frozen_commit: self.request.expected_commit,
        };
        if !push_receipt_matches(&expected, &receipt) {
            return Err(outcome_unknown("push_workspace"));
        }
        Ok(receipt)
    }
}

pub(crate) struct PendingPullRequest {
    request: CreatePullRequestRequest,
    config: TrustedConfig,
    stream: crate::jobworkerp::DispatchStream,
}

impl PendingPullRequest {
    pub(crate) fn parent_job_id(&self) -> Option<i64> {
        self.stream.job_id.as_ref().map(|id| id.value)
    }

    pub(crate) async fn finish(self) -> Result<PullRequestReceipt, NativeExecutorError> {
        let output = super::preparation_workflow::finish_workflow(self.stream, 150).await?;
        let ExecutorResult::PullRequestCreated(receipt) =
            decode_response(&output.output, "create_pull_request")?
        else {
            return Err(outcome_unknown("create_pull_request"));
        };
        if !pull_request_receipt_matches(&self.request, &receipt, &self.config) {
            return Err(outcome_unknown("create_pull_request"));
        }
        Ok(receipt)
    }
}

/// Host-provided paths and digest pin for one installed, approved executor package.
/// The caller must keep both the package and trusted configuration outside guest-writable mounts.
#[derive(Clone)]
pub struct NativeExecutorClient {
    executable: PathBuf,
    trusted_config: PathBuf,
    expected_executable_sha256: String,
    trusted_config_sha256: String,
    verified_capabilities: VerifiedNativeCapabilities,
    preparation_workflow: Option<crate::jobworkerp::JobworkerpHandle>,
}

impl NativeExecutorClient {
    /// Verifies the installed package before probing its protocol capabilities.
    pub async fn connect(
        executable: PathBuf,
        trusted_config: PathBuf,
        expected_executable_sha256: String,
    ) -> Result<Self, NativeExecutorError> {
        let capabilities = Self::probe(
            executable.clone(),
            trusted_config.clone(),
            expected_executable_sha256.clone(),
        )
        .await?;
        Self::connect_with_capabilities(
            executable,
            trusted_config,
            expected_executable_sha256,
            capabilities,
        )
    }

    /// Verify the host inputs and probe only the helper's non-secret protocol
    /// capabilities. The supplied trusted config is never opened for tokens.
    pub(crate) async fn probe(
        executable: PathBuf,
        trusted_config: PathBuf,
        expected_executable_sha256: String,
    ) -> Result<VerifiedNativeCapabilities, NativeExecutorError> {
        let (config, config_sha256) =
            checked_inputs(&executable, &trusted_config, &expected_executable_sha256)?;
        let client = Self::with_verified_capabilities(
            executable.clone(),
            trusted_config,
            expected_executable_sha256.clone(),
            config_sha256,
            VerifiedNativeCapabilities {
                executor_version: String::new(),
                protocol_version: 0,
                features: BTreeSet::new(),
                private_config_binding: String::new(),
                executable_path: PathBuf::new(),
                executable_sha256: String::new(),
            },
        );
        let mut capabilities = client.probe_capabilities(&config).await?;
        capabilities.private_config_binding = private_config_binding(&config);
        capabilities.executable_path = executable;
        capabilities.executable_sha256 = expected_executable_sha256;
        Ok(capabilities)
    }

    /// Rebind a previously probed helper to a freshly written trusted config.
    /// Inputs and config paths are revalidated, but the helper is not probed a
    /// second time after credential snapshots are installed.
    pub(crate) fn connect_with_capabilities(
        executable: PathBuf,
        trusted_config: PathBuf,
        expected_executable_sha256: String,
        verified_capabilities: VerifiedNativeCapabilities,
    ) -> Result<Self, NativeExecutorError> {
        let (config, config_sha256) =
            checked_inputs(&executable, &trusted_config, &expected_executable_sha256)?;
        if verified_capabilities.executable_path != executable
            || verified_capabilities.executable_sha256 != expected_executable_sha256
            || verified_capabilities.private_config_binding != private_config_binding(&config)
        {
            return Err(NativeExecutorError::InvalidTrustedConfiguration);
        }
        Ok(Self::with_verified_capabilities(
            executable,
            trusted_config,
            expected_executable_sha256,
            config_sha256,
            verified_capabilities,
        ))
    }

    fn with_verified_capabilities(
        executable: PathBuf,
        trusted_config: PathBuf,
        expected_executable_sha256: String,
        trusted_config_sha256: String,
        verified_capabilities: VerifiedNativeCapabilities,
    ) -> Self {
        Self {
            executable,
            trusted_config,
            expected_executable_sha256,
            trusted_config_sha256,
            verified_capabilities,
            preparation_workflow: None,
        }
    }

    /// Bind workspace mutations to registered WORKFLOW jobs without a direct fallback.
    pub fn with_preparation_workflow(
        mut self,
        handle: crate::jobworkerp::JobworkerpHandle,
    ) -> Self {
        self.preparation_workflow = Some(handle);
        self
    }

    pub(crate) fn verified_capabilities(
        &self,
    ) -> Result<VerifiedNativeCapabilities, NativeExecutorError> {
        self.checked_config()?;
        Ok(self.verified_capabilities.clone())
    }

    /// Return only host-approved repository policy identifiers and URLs. The
    /// helper remains the sole reader of credential bytes and clone URLs.
    pub(crate) fn publication_repository_config(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<NativeRepositoryPublicationConfig, NativeExecutorError> {
        let config = self.checked_config()?;
        let matching = config
            .repositories
            .iter()
            .filter(|repository| repository.id == *repository_id)
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            return Err(NativeExecutorError::InvalidTrustedConfiguration);
        }
        let repository = matching[0];
        let credential_ids = config
            .credential_sources
            .iter()
            .map(|credential| credential.id.as_str())
            .collect::<BTreeSet<_>>();
        for reference in [
            repository.read_credential_source.as_deref(),
            repository.publish_credential_source.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !credential_ids.contains(reference) {
                return Err(NativeExecutorError::InvalidTrustedConfiguration);
            }
        }
        let allowed_base_branches = repository
            .allowed_base_branches
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if allowed_base_branches.len() != repository.allowed_base_branches.len()
            || allowed_base_branches.is_empty()
        {
            return Err(NativeExecutorError::InvalidTrustedConfiguration);
        }
        Ok(NativeRepositoryPublicationConfig {
            clone_url: repository.clone_url.clone(),
            api_base_url: repository
                .forge
                .as_ref()
                .map(|forge| forge.api_base_url.clone()),
            allowed_base_branches,
            forge_kind: repository.forge.as_ref().map(|forge| forge.kind),
            web_base_url: repository
                .forge
                .as_ref()
                .map(|forge| forge.web_base_url.clone()),
            read_credential_reference: repository.read_credential_source.clone(),
            publication_credential_reference: repository.publish_credential_source.clone(),
        })
    }

    pub async fn prepare(
        &self,
        request: coding_agent_executor::PrepareRequest,
    ) -> Result<PreparedWorkspace, NativeExecutorError> {
        let config = self.checked_config()?;
        let operation = Operation::Prepare(request.clone());
        validate_operation(&operation)?;
        let result = self.execute(operation).await?;
        let ExecutorResult::Prepared(receipt) = result else {
            return Err(outcome_unknown("prepare"));
        };
        if !prepared_receipt_matches(&request, &receipt, &config) {
            return Err(outcome_unknown("prepare"));
        }
        Ok(receipt)
    }

    /// Read the approved workspace without changing its HEAD, index, or worktree.
    pub async fn inspect_source(
        &self,
        request: coding_agent_executor::PrepareRequest,
    ) -> Result<coding_agent_executor::SourceInspection, NativeExecutorError> {
        let config = self.checked_config()?;
        self.require_capabilities(&["inspect_source"])?;
        let operation = Operation::InspectSource(request.clone());
        validate_operation(&operation)?;
        let result = self.execute(operation).await?;
        let ExecutorResult::SourceInspected(inspection) = result else {
            return Err(outcome_unknown("inspect_source"));
        };
        let total_bytes = inspection.new_files.iter().try_fold(
            inspection
                .tracked_diff
                .len()
                .saturating_add(inspection.uncommitted_diff.as_ref().map_or(0, Vec::len)),
            |total, file| {
                total
                    .checked_add(file.path.len())?
                    .checked_add(file.content.len())
            },
        );
        if inspection.run_id != request.run_id
            || inspection.attempt_id != request.attempt_id
            || inspection.base_commit != request.base_commit
            || inspection.branch != request.expected_branch()
            || !valid_commit(&inspection.head_commit)
            || !total_bytes.is_some_and(|size| size as u64 <= config.max_import_bytes)
            || inspection.new_files.iter().any(|file| {
                file.path.is_empty()
                    || file.path.starts_with('/')
                    || file.path.contains('\0')
                    || file.path.split('/').any(|part| {
                        part.is_empty() || matches!(part, "." | ".." | ".git" | ".coding-agent")
                    })
            })
        {
            return Err(outcome_unknown("inspect_source"));
        }
        Ok(inspection)
    }

    /// Resolves one approved base ref using a separately advertised optional capability.
    pub async fn resolve_base(
        &self,
        request: ResolveBaseRequest,
    ) -> Result<BaseResolutionReceipt, NativeExecutorError> {
        let config = self.checked_config()?;
        if !self.verified_capabilities.features.contains("resolve_base") {
            return Err(NativeExecutorError::IncompatibleProtocol);
        }
        let operation = Operation::ResolveBase(request.clone());
        validate_operation(&operation)?;
        let repository = config
            .repositories
            .iter()
            .find(|repository| repository.id == request.repository)
            .ok_or(NativeExecutorError::ExecutorRejected(
                ExecutorErrorCode::RepositoryNotApproved,
            ))?;
        if !request.base_branch.is_empty()
            && !repository
                .allowed_base_branches
                .iter()
                .any(|branch| branch == &request.base_branch)
        {
            return Err(NativeExecutorError::ExecutorRejected(
                ExecutorErrorCode::BaseBranchNotApproved,
            ));
        }

        let result = self.execute(operation).await?;
        let ExecutorResult::BaseResolved(receipt) = result else {
            return Err(outcome_unknown("resolve_base"));
        };
        if !base_resolution_receipt_matches(&request, &receipt, &self.trusted_config_sha256)
            || !approved_repository(&config, &receipt.repository, &receipt.base_branch)
        {
            return Err(outcome_unknown("resolve_base"));
        }
        Ok(receipt)
    }

    /// Performs bounded read-only Git and Forge checks for one approved
    /// repository. The executor remains the only process that reads tokens or
    /// invokes Git/HTTP; this client accepts only a sanitized result.
    pub async fn check_repository(
        &self,
        request: CheckRepositoryRequest,
    ) -> Result<RepositoryCheckResult, NativeExecutorError> {
        let config = self.checked_config()?;
        self.require_capabilities(&["check_repository"])?;
        let operation = Operation::CheckRepository(request.clone());
        validate_operation(&operation)?;
        let result = self.execute(operation).await?;
        let ExecutorResult::RepositoryChecked(receipt) = result else {
            return Err(outcome_unknown("check_repository"));
        };
        if !repository_check_result_matches(
            &request,
            &receipt,
            &self.trusted_config_sha256,
            &config,
        ) {
            return Err(outcome_unknown("check_repository"));
        }
        Ok(receipt)
    }

    pub async fn capture(
        &self,
        request: CaptureRequest,
    ) -> Result<CaptureReceipt, NativeExecutorError> {
        self.checked_config()?;
        let operation = Operation::Capture(request.clone());
        validate_operation(&operation)?;
        self.require_capabilities(&["capture"])?;
        let result = self.execute(operation).await?;
        let ExecutorResult::Captured(receipt) = result else {
            return Err(outcome_unknown("capture"));
        };
        if !capture_receipt_matches(&request, &receipt) {
            return Err(outcome_unknown("capture"));
        }
        Ok(receipt)
    }

    pub async fn seal(
        &self,
        request: SealRequest,
    ) -> Result<SealedArtifactReceipt, NativeExecutorError> {
        let config = self.checked_config()?;
        let operation = Operation::Seal(request.clone());
        validate_operation(&operation)?;
        self.require_capabilities(&["seal"])?;
        let result = self.execute(operation).await?;
        let ExecutorResult::Sealed(receipt) = result else {
            return Err(outcome_unknown("seal"));
        };
        if !sealed_receipt_matches(&request, &receipt, &config) {
            return Err(outcome_unknown("seal"));
        }
        Ok(receipt)
    }

    pub async fn resolve(
        &self,
        request: ResolveRequest,
    ) -> Result<ResolvedArtifact, NativeExecutorError> {
        let config = self.checked_config()?;
        let operation = Operation::Resolve(request.clone());
        validate_operation(&operation)?;
        self.require_capabilities(&["resolve"])?;
        let result = self.execute(operation).await?;
        let ExecutorResult::Resolved(receipt) = result else {
            return Err(outcome_unknown("resolve"));
        };
        if !resolved_receipt_matches(&request, &receipt, &config) {
            return Err(outcome_unknown("resolve"));
        }
        Ok(receipt)
    }

    pub async fn push(&self, request: PushRequest) -> Result<PushReceipt, NativeExecutorError> {
        self.execute_push(
            Operation::Push(request.clone()),
            &request,
            &["push", "secret_scan_supported_formats_v1"],
        )
        .await
    }

    pub async fn lookup_push(
        &self,
        request: PushRequest,
    ) -> Result<PushReceipt, NativeExecutorError> {
        self.execute_push(
            Operation::LookupPush(request.clone()),
            &request,
            &["lookup_publication"],
        )
        .await
    }

    async fn execute_push(
        &self,
        operation: Operation,
        request: &PushRequest,
        capabilities: &[&'static str],
    ) -> Result<PushReceipt, NativeExecutorError> {
        self.checked_config()?;
        validate_operation(&operation)?;
        self.require_capabilities(capabilities)?;
        let name = operation_name(&operation);
        let result = self.execute(operation).await?;
        let ExecutorResult::Pushed(receipt) = result else {
            return Err(outcome_unknown(name));
        };
        if !push_receipt_matches(request, &receipt) {
            return Err(outcome_unknown(name));
        }
        Ok(receipt)
    }

    pub async fn push_workspace(
        &self,
        request: WorkspacePushRequest,
    ) -> Result<PushReceipt, NativeExecutorError> {
        self.begin_workspace_push(request).await?.finish().await
    }

    pub(crate) async fn begin_workspace_push(
        &self,
        request: WorkspacePushRequest,
    ) -> Result<PendingWorkspacePush, NativeExecutorError> {
        self.checked_config()?;
        validate_operation(&Operation::PushWorkspace(request.clone()))?;
        self.require_capabilities(&["push", "push_workspace"])?;
        let stream = self
            .start_publication_workflow(
                Operation::PushWorkspace(request.clone()),
                "publish-workspace",
                None,
            )
            .await?;
        Ok(PendingWorkspacePush { request, stream })
    }

    pub async fn create_pull_request_workflow(
        &self,
        request: CreatePullRequestRequest,
    ) -> Result<PullRequestReceipt, NativeExecutorError> {
        self.begin_pull_request_workflow(request, None)
            .await?
            .finish()
            .await
    }

    pub(crate) async fn begin_pull_request_workflow(
        &self,
        request: CreatePullRequestRequest,
        cancellation: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<PendingPullRequest, NativeExecutorError> {
        let config = self.checked_config()?;
        validate_operation(&Operation::CreatePullRequest(request.clone()))?;
        self.require_capabilities(&["push", "pull_request"])?;
        let stream = self
            .start_publication_workflow(
                Operation::CreatePullRequest(request.clone()),
                "publish-pull-request",
                cancellation,
            )
            .await?;
        Ok(PendingPullRequest {
            request,
            config,
            stream,
        })
    }

    async fn start_publication_workflow(
        &self,
        operation: Operation,
        route: &str,
        cancellation: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<crate::jobworkerp::DispatchStream, NativeExecutorError> {
        let handle = self
            .preparation_workflow
            .as_ref()
            .ok_or(NativeExecutorError::Unavailable)?;
        let definition = super::preparation_workflow::preparation_definition(
            &self.executable,
            &self.trusted_config,
        )?;
        let input = serde_json::to_string(&ExecutorRequest {
            protocol_version: PROTOCOL_VERSION,
            operation,
        })
        .map_err(|_| NativeExecutorError::InvalidRequest)?;
        if input.len() + 1 > MAX_REQUEST_BYTES {
            return Err(NativeExecutorError::InvalidRequest);
        }
        let registration =
            super::preparation_workflow::register_workflow(handle, definition, route);
        let registered = match cancellation {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => return Err(NativeExecutorError::Unavailable),
                result = registration => result?,
            },
            None => registration.await?,
        };
        super::preparation_workflow::start_registered_workflow_with_cancellation(
            handle,
            &registered,
            serde_json::json!({"executor_request_json":format!("{input}\n")}),
            150,
            cancellation,
        )
        .await
    }

    pub async fn create_pull_request_workflow_with_cancellation(
        &self,
        request: CreatePullRequestRequest,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<PullRequestReceipt, NativeExecutorError> {
        self.begin_pull_request_workflow(request, Some(cancellation))
            .await?
            .finish()
            .await
    }

    pub async fn create_pull_request(
        &self,
        request: CreatePullRequestRequest,
    ) -> Result<PullRequestReceipt, NativeExecutorError> {
        self.execute_pull_request(
            Operation::CreatePullRequest(request.clone()),
            &request,
            &["pull_request", "push", "secret_scan_supported_formats_v1"],
        )
        .await
    }

    pub async fn lookup_pull_request(
        &self,
        request: CreatePullRequestRequest,
    ) -> Result<PullRequestReceipt, NativeExecutorError> {
        self.execute_pull_request(
            Operation::LookupPullRequest(request.clone()),
            &request,
            &["lookup_publication"],
        )
        .await
    }

    async fn execute_pull_request(
        &self,
        operation: Operation,
        request: &CreatePullRequestRequest,
        capabilities: &[&'static str],
    ) -> Result<PullRequestReceipt, NativeExecutorError> {
        let config = self.checked_config()?;
        validate_operation(&operation)?;
        self.require_capabilities(capabilities)?;
        let name = operation_name(&operation);
        let result = self.execute(operation).await?;
        let ExecutorResult::PullRequestCreated(receipt) = result else {
            return Err(outcome_unknown(name));
        };
        if !pull_request_receipt_matches(request, &receipt, &config) {
            return Err(outcome_unknown(name));
        }
        Ok(receipt)
    }

    async fn probe_capabilities(
        &self,
        config: &TrustedConfig,
    ) -> Result<VerifiedNativeCapabilities, NativeExecutorError> {
        let output = self
            .invoke(
                &["--capabilities".into()],
                None,
                &config.state_root.join("git-home"),
                InvocationPurpose::Probe,
            )
            .await?;
        let payload = single_json_line(&output).ok_or(NativeExecutorError::IncompatibleProtocol)?;
        let document: CapabilityDocument = serde_json::from_slice(payload)
            .map_err(|_| NativeExecutorError::IncompatibleProtocol)?;
        parse_capabilities(&document).ok_or(NativeExecutorError::IncompatibleProtocol)
    }

    fn require_capabilities(
        &self,
        capabilities: &[&'static str],
    ) -> Result<(), NativeExecutorError> {
        for capability in capabilities {
            if !self.verified_capabilities.features.contains(*capability) {
                return Err(NativeExecutorError::UnsupportedCapability { capability });
            }
        }
        Ok(())
    }

    async fn execute(&self, operation: Operation) -> Result<ExecutorResult, NativeExecutorError> {
        let config = self.checked_config()?;
        let request = ExecutorRequest {
            protocol_version: PROTOCOL_VERSION,
            operation,
        };
        let mut bytes =
            serde_json::to_vec(&request).map_err(|_| NativeExecutorError::InvalidRequest)?;
        if bytes.len() + 1 > MAX_REQUEST_BYTES {
            return Err(NativeExecutorError::InvalidRequest);
        }
        bytes.push(b'\n');
        if matches!(
            request.operation,
            Operation::ResolveBase(_) | Operation::Prepare(_) | Operation::InspectSource(_)
        ) && let Some(handle) = &self.preparation_workflow
        {
            let output = super::preparation_workflow::execute_preparation_workflow(
                handle,
                &self.executable,
                &self.trusted_config,
                &bytes,
                operation_response_limit(operation_name(&request.operation)),
            )
            .await?;
            return decode_response(&output, operation_name(&request.operation));
        }
        let args = vec![
            "--config".into(),
            self.trusted_config.as_os_str().to_owned(),
            "--stdio".into(),
        ];
        let operation_name = operation_name(&request.operation);
        let output = self
            .invoke(
                &args,
                Some(&bytes),
                &config.state_root.join("git-home"),
                InvocationPurpose::Operation(operation_name),
            )
            .await?;
        decode_response(&output, operation_name)
    }

    fn checked_config(&self) -> Result<TrustedConfig, NativeExecutorError> {
        verify_executable(&self.executable, &self.expected_executable_sha256)?;
        let bytes = read_secure_file(&self.trusted_config, SecureFileKind::Configuration)?;
        if sha256(&bytes) != self.trusted_config_sha256 {
            return Err(NativeExecutorError::InvalidTrustedConfiguration);
        }
        let config = parse_trusted_config(&bytes)?;
        validate_host_path_separation(&self.executable, &self.trusted_config, &config)?;
        Ok(config)
    }

    async fn invoke(
        &self,
        args: &[std::ffi::OsString],
        input: Option<&[u8]>,
        home: &Path,
        purpose: InvocationPurpose,
    ) -> Result<Vec<u8>, NativeExecutorError> {
        let mut command = Command::new(&self.executable);
        command
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/local/bin")
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home)
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .current_dir("/")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|_| NativeExecutorError::Unavailable)?;
        let mut process_group = ProcessGroupGuard::new(child.id());
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| process_failure(purpose))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| process_failure(purpose))?;
        let stdin = child.stdin.take();
        let communication = async {
            let write = async {
                if let (Some(mut stdin), Some(input)) = (stdin, input) {
                    stdin
                        .write_all(input)
                        .await
                        .map_err(|_| InvocationFailure::Unavailable)?;
                    stdin
                        .shutdown()
                        .await
                        .map_err(|_| InvocationFailure::Unavailable)?;
                }
                Ok::<(), InvocationFailure>(())
            };
            let read_stdout = async {
                read_limited(stdout, purpose.response_limit() + 1)
                    .await
                    .map_err(|error| {
                        if error.kind() == io::ErrorKind::InvalidData {
                            InvocationFailure::ResponseTooLarge
                        } else {
                            InvocationFailure::Unavailable
                        }
                    })
            };
            let read_stderr = async {
                read_limited(stderr, MAX_STDERR_BYTES)
                    .await
                    .map_err(|_| InvocationFailure::Unavailable)
            };
            let wait = async {
                child
                    .wait()
                    .await
                    .map_err(|_| InvocationFailure::Unavailable)
            };
            tokio::try_join!(write, read_stdout, read_stderr, wait)
        };

        let result = tokio::time::timeout(purpose.timeout(), communication).await;
        let ((), output, _stderr, status) = match result {
            Ok(Ok(output)) => output,
            Ok(Err(failure)) => {
                terminate_process_tree(&mut child, &mut process_group).await;
                return Err(transport_failure(purpose, failure.reason()));
            }
            Err(_) => {
                terminate_process_tree(&mut child, &mut process_group).await;
                return Err(transport_failure(
                    purpose,
                    NativeDiagnosticTransportReason::Timeout,
                ));
            }
        };
        process_group.kill_remaining();
        if output.len() > purpose.response_limit() {
            return Err(transport_failure(
                purpose,
                NativeDiagnosticTransportReason::ResponseTooLarge,
            ));
        }
        if !status.success() {
            return Err(process_failure(purpose));
        }
        Ok(output)
    }
}

#[derive(Clone, Copy)]
enum InvocationFailure {
    Unavailable,
    ResponseTooLarge,
}

impl InvocationFailure {
    fn reason(self) -> NativeDiagnosticTransportReason {
        match self {
            Self::Unavailable => NativeDiagnosticTransportReason::Unavailable,
            Self::ResponseTooLarge => NativeDiagnosticTransportReason::ResponseTooLarge,
        }
    }
}

#[derive(Clone, Copy)]
enum InvocationPurpose {
    Probe,
    Operation(&'static str),
    #[cfg(test)]
    TestOperation {
        operation: &'static str,
        timeout: Duration,
        response_limit: usize,
    },
}

impl InvocationPurpose {
    fn response_limit(self) -> usize {
        match self {
            Self::Probe => MAX_RESPONSE_BYTES,
            Self::Operation(operation) => operation_response_limit(operation),
            #[cfg(test)]
            Self::TestOperation { response_limit, .. } => response_limit,
        }
    }

    fn timeout(self) -> Duration {
        match self {
            Self::Probe => PROBE_TIMEOUT,
            Self::Operation("check_repository") => Duration::from_secs(35),
            Self::Operation(_) => OPERATION_TIMEOUT,
            #[cfg(test)]
            Self::TestOperation { timeout, .. } => timeout,
        }
    }
}

fn operation_response_limit(operation: &str) -> usize {
    if operation == "inspect_source" {
        coding_agent_executor::MAX_SOURCE_RESPONSE_BYTES
    } else {
        MAX_RESPONSE_BYTES
    }
}

fn process_failure(purpose: InvocationPurpose) -> NativeExecutorError {
    transport_failure(purpose, NativeDiagnosticTransportReason::Unavailable)
}

fn transport_failure(
    purpose: InvocationPurpose,
    reason: NativeDiagnosticTransportReason,
) -> NativeExecutorError {
    match purpose {
        InvocationPurpose::Probe => NativeExecutorError::IncompatibleProtocol,
        InvocationPurpose::Operation("check_repository") => {
            NativeExecutorError::DiagnosticTransport { reason }
        }
        InvocationPurpose::Operation(operation) => outcome_unknown(operation),
        #[cfg(test)]
        InvocationPurpose::TestOperation { operation, .. } if operation == "check_repository" => {
            NativeExecutorError::DiagnosticTransport { reason }
        }
        #[cfg(test)]
        InvocationPurpose::TestOperation { operation, .. } => outcome_unknown(operation),
    }
}

#[cfg(unix)]
struct ProcessGroupGuard(Option<u32>);

#[cfg(not(unix))]
struct ProcessGroupGuard;

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        #[cfg(unix)]
        {
            Self(pid)
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            Self
        }
    }

    fn kill_remaining(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.take().and_then(|pid| i32::try_from(pid).ok()) {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill_remaining();
    }
}

async fn terminate_process_tree(child: &mut Child, process_group: &mut ProcessGroupGuard) {
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
    }
    let _ = tokio::time::timeout(TERMINATION_GRACE, child.wait()).await;
    process_group.kill_remaining();
    let _ = child.kill().await;
    let _ = tokio::time::timeout(TERMINATION_GRACE, child.wait()).await;
}

async fn read_limited<R>(reader: R, limit: usize) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "limit exceeded"));
    }
    Ok(bytes)
}

fn parse_trusted_config(bytes: &[u8]) -> Result<TrustedConfig, NativeExecutorError> {
    let config: TrustedConfig = serde_json::from_slice(bytes)
        .map_err(|_| NativeExecutorError::InvalidTrustedConfiguration)?;
    if config.protocol_version != PROTOCOL_VERSION
        || config.expected_executor_version != EXECUTOR_VERSION
    {
        return Err(NativeExecutorError::InvalidTrustedConfiguration);
    }
    Ok(config)
}

fn checked_inputs(
    executable: &Path,
    trusted_config: &Path,
    expected_executable_sha256: &str,
) -> Result<(TrustedConfig, String), NativeExecutorError> {
    if !is_sha256(expected_executable_sha256) {
        return Err(NativeExecutorError::InvalidPackage);
    }
    let config_bytes = read_secure_file(trusted_config, SecureFileKind::Configuration)?;
    let config = parse_trusted_config(&config_bytes)?;
    validate_host_path_separation(executable, trusted_config, &config)?;
    verify_executable(executable, expected_executable_sha256)?;
    Ok((config, sha256(&config_bytes)))
}

fn private_config_binding(config: &TrustedConfig) -> String {
    let repositories = config
        .repositories
        .iter()
        .map(|repository| RepositoryConfig {
            id: repository.id.clone(),
            clone_url: repository.clone_url.clone(),
            ssh_directory: repository.ssh_directory.clone(),
            allowed_base_branches: repository.allowed_base_branches.clone(),
            read_credential_source: None,
            publish_credential_source: None,
            forge: repository.forge.clone(),
        })
        .collect::<Vec<_>>();
    let fields = (
        &config.workspace_root,
        &config.publisher_root,
        &config.state_root,
        &config.credential_root,
        config.max_import_bytes,
        repositories,
    );
    let bytes = serde_json::to_vec(&fields).unwrap_or_default();
    sha256(&bytes)
}

fn validate_host_path_separation(
    executable: &Path,
    trusted_config: &Path,
    config: &TrustedConfig,
) -> Result<(), NativeExecutorError> {
    for protected_root in [
        &config.workspace_root,
        &config.publisher_root,
        &config.state_root,
        &config.credential_root,
    ] {
        let canonical_root = fs::canonicalize(protected_root)
            .map_err(|_| NativeExecutorError::InvalidTrustedConfiguration)?;
        if canonical_root != *protected_root
            || !fs::symlink_metadata(&canonical_root)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        {
            return Err(NativeExecutorError::InvalidTrustedConfiguration);
        }
        for host_file in [executable, trusted_config] {
            if host_file.starts_with(&canonical_root) || canonical_root.starts_with(host_file) {
                return Err(NativeExecutorError::InvalidTrustedConfiguration);
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum SecureFileKind {
    Executor,
    Configuration,
}

fn verify_executable(path: &Path, expected_sha256: &str) -> Result<(), NativeExecutorError> {
    let bytes = read_secure_file(path, SecureFileKind::Executor)?;
    if sha256(&bytes) != expected_sha256 {
        return Err(NativeExecutorError::InvalidPackage);
    }
    Ok(())
}

fn read_secure_file(path: &Path, kind: SecureFileKind) -> Result<Vec<u8>, NativeExecutorError> {
    let invalid = || match kind {
        SecureFileKind::Executor => NativeExecutorError::InvalidPackage,
        SecureFileKind::Configuration => NativeExecutorError::InvalidTrustedConfiguration,
    };
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid());
    }
    verify_path_directories(path, kind).map_err(|_| invalid())?;
    let file = open_read_nofollow(path).map_err(|_| invalid())?;
    let metadata = file.metadata().map_err(|_| invalid())?;
    let path_metadata = fs::symlink_metadata(path).map_err(|_| invalid())?;
    let limit = match kind {
        SecureFileKind::Executor => MAX_EXECUTOR_BYTES,
        SecureFileKind::Configuration => MAX_CONFIG_BYTES,
    };
    if !metadata.is_file()
        || path_metadata.file_type().is_symlink()
        || metadata.len() > limit
        || !same_file(&metadata, &path_metadata)
        || !safe_file_metadata(&metadata, kind)
    {
        return Err(invalid());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid())?;
    if bytes.len() as u64 > limit || bytes.len() as u64 != metadata.len() {
        return Err(invalid());
    }
    Ok(bytes)
}

fn verify_path_directories(path: &Path, kind: SecureFileKind) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing parent"))?;
    let mut current = PathBuf::from("/");
    for component in parent.components() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::RootDir) {
                continue;
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid parent",
            ));
        };
        current.push(name);
        let metadata = fs::symlink_metadata(&current)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || !safe_directory(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe parent",
            ));
        }
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "symlink"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        match kind {
            SecureFileKind::Executor if mode & 0o111 != 0 && mode & 0o022 == 0 => {}
            SecureFileKind::Configuration if mode & 0o077 == 0 => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unsafe mode",
                ));
            }
        }
    }
    Ok(())
}

fn safe_file_metadata(metadata: &fs::Metadata, kind: SecureFileKind) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mode = metadata.permissions().mode();
        let owner_ok = trusted_owner(metadata.uid());
        let mode_ok = match kind {
            SecureFileKind::Executor => mode & 0o111 != 0 && mode & 0o022 == 0,
            SecureFileKind::Configuration => mode & 0o077 == 0,
        };
        owner_ok && mode_ok && metadata.nlink() == 1
    }
    #[cfg(not(unix))]
    {
        let _ = (metadata, kind);
        false
    }
}

fn safe_directory(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mode = metadata.permissions().mode();
        let protected_sticky_directory = mode & 0o1000 != 0 && metadata.uid() == 0;
        trusted_owner(metadata.uid()) && (mode & 0o022 == 0 || protected_sticky_directory)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

#[cfg(unix)]
fn trusted_owner(uid: u32) -> bool {
    uid == unsafe { libc::geteuid() } || uid == 0
}

#[cfg(not(unix))]
fn trusted_owner(_uid: u32) -> bool {
    false
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        left.dev() == right.dev() && left.ino() == right.ino() && left.nlink() == 1
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        false
    }
}

fn open_read_nofollow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    options.open(path)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilityDocument {
    protocol_version: u32,
    executor_version: String,
    features: Vec<String>,
}

fn parse_capabilities(document: &CapabilityDocument) -> Option<VerifiedNativeCapabilities> {
    if document.protocol_version != PROTOCOL_VERSION
        || document.executor_version != EXECUTOR_VERSION
    {
        return None;
    }
    if document.features.is_empty() {
        return None;
    }
    let feature_set = document
        .features
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if feature_set.len() != document.features.len()
        || feature_set
            .iter()
            .any(|feature| !VERIFIED_NATIVE_FEATURES.contains(feature))
    {
        return None;
    }
    if !CORE_NATIVE_FEATURES
        .iter()
        .all(|required| feature_set.contains(required))
        || feature_set.contains("push") != feature_set.contains("secret_scan_supported_formats_v1")
        || (feature_set.contains("pull_request") && !feature_set.contains("push"))
    {
        return None;
    }
    Some(VerifiedNativeCapabilities {
        executor_version: EXECUTOR_VERSION.into(),
        protocol_version: PROTOCOL_VERSION,
        features: feature_set.into_iter().map(str::to_owned).collect(),
        private_config_binding: String::new(),
        executable_path: PathBuf::new(),
        executable_sha256: String::new(),
    })
}

fn single_json_line(output: &[u8]) -> Option<&[u8]> {
    let payload = output.strip_suffix(b"\n")?;
    if payload.is_empty() || payload.contains(&b'\n') || payload.contains(&b'\r') {
        return None;
    }
    Some(payload)
}

fn decode_response(
    output: &[u8],
    operation: &'static str,
) -> Result<ExecutorResult, NativeExecutorError> {
    let payload = single_json_line(output).ok_or_else(|| outcome_unknown(operation))?;
    let response: Value =
        serde_json::from_slice(payload).map_err(|_| outcome_unknown(operation))?;
    let Some(object) = response.as_object() else {
        return Err(outcome_unknown(operation));
    };
    if object.get("protocol_version").and_then(Value::as_u64) != Some(u64::from(PROTOCOL_VERSION)) {
        return Err(outcome_unknown(operation));
    }
    match object.get("status").and_then(Value::as_str) {
        Some("ok") if object.len() == 3 && object.contains_key("result") => {
            serde_json::from_value::<ExecutorResult>(object["result"].clone())
                .map_err(|_| outcome_unknown(operation))
        }
        Some("error") if object.len() == 3 && object.contains_key("error") => {
            let Some(error) = object["error"].as_object() else {
                return Err(outcome_unknown(operation));
            };
            if error.len() != 3 {
                return Err(outcome_unknown(operation));
            }
            let code: ExecutorErrorCode = serde_json::from_value(
                error
                    .get("code")
                    .cloned()
                    .ok_or_else(|| outcome_unknown(operation))?,
            )
            .map_err(|_| outcome_unknown(operation))?;
            if error.get("message").and_then(Value::as_str) != Some(code.public_message())
                || error.get("retryable").and_then(Value::as_bool) != Some(code.retryable())
            {
                return Err(outcome_unknown(operation));
            }
            if publication_error_may_have_effect(operation, code) {
                return Err(outcome_unknown(operation));
            }
            Err(NativeExecutorError::ExecutorRejected(code))
        }
        _ => Err(outcome_unknown(operation)),
    }
}

fn publication_error_may_have_effect(operation: &'static str, code: ExecutorErrorCode) -> bool {
    if !matches!(operation, "push" | "create_pull_request") {
        return false;
    }

    // This denial is emitted before any forge request. Other publication error codes can also
    // be emitted after a remote update or POST, including non-retryable response-size errors.
    !matches!(
        (operation, code),
        (
            "create_pull_request",
            ExecutorErrorCode::PullRequestNotApproved
        )
    )
}

fn validate_operation(operation: &Operation) -> Result<(), NativeExecutorError> {
    let valid_id = |value: &str| {
        !value.is_empty()
            && value.len() <= 80
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    let valid_digest = |value: &str| value.len() == 64 && value.bytes().all(is_lower_hex);
    let valid_branch =
        |run_id: &str, branch: &str| coding_agent_executor::branch_belongs_to_run(branch, run_id);
    let valid_identity = |run_id: &str, attempt_id: &str| valid_id(run_id) && valid_id(attempt_id);
    let valid_repo = |repository: &RepositoryId| {
        !repository.host.is_empty()
            && !repository.owner.is_empty()
            && !repository.name.is_empty()
            && [
                repository.host.as_str(),
                repository.owner.as_str(),
                repository.name.as_str(),
            ]
            .iter()
            .all(|part| !part.chars().any(char::is_control))
    };
    let valid_base = |branch: &str, commit: &str| {
        !branch.is_empty() && !branch.chars().any(char::is_control) && valid_commit(commit)
    };
    let valid_base_branch = |branch: &str| {
        !branch.is_empty()
            && branch.len() <= 255
            && !branch.starts_with('/')
            && !branch.ends_with('/')
            && !branch.starts_with('-')
            && !branch.contains("//")
            && !branch.contains("..")
            && !branch.contains("@{")
            && branch.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.')
            })
            && branch.split('/').all(|part| {
                !part.is_empty()
                    && !part.starts_with('.')
                    && !part.ends_with('.')
                    && !part.ends_with(".lock")
            })
    };
    let valid = match operation {
        Operation::CheckRepository(request) => valid_repo(&request.repository),
        Operation::ResolveBase(request) => {
            valid_identity(&request.run_id, &request.attempt_id)
                && valid_id(&request.operation_id)
                && valid_repo(&request.repository)
                && (request.base_branch.is_empty() || valid_base_branch(&request.base_branch))
        }
        Operation::Prepare(request) | Operation::InspectSource(request) => {
            valid_identity(&request.run_id, &request.attempt_id)
                && valid_repo(&request.repository)
                && valid_base(&request.base_branch, &request.base_commit)
        }
        Operation::Capture(request) => {
            valid_identity(&request.run_id, &request.attempt_id)
                && valid_repo(&request.repository)
                && valid_base(&request.base_branch, &request.base_commit)
                && valid_digest(&request.approved_snapshot_digest)
                && valid_digest(&request.scope_digest)
                && valid_digest(&request.intent_approval_digest)
        }
        Operation::Seal(request) => {
            valid_identity(&request.run_id, &request.attempt_id)
                && valid_digest(&request.manifest_digest)
        }
        Operation::Resolve(request) => {
            valid_identity(&request.run_id, &request.attempt_id)
                && valid_digest(&request.artifact_digest)
        }
        Operation::Push(request) | Operation::LookupPush(request) => {
            valid_identity(&request.run_id, &request.attempt_id)
                && valid_repo(&request.repository)
                && valid_branch(&request.run_id, &request.branch)
                && valid_base(&request.base_branch, &request.base_commit)
                && valid_digest(&request.artifact_digest)
                && valid_commit(&request.frozen_commit)
        }
        Operation::PushWorkspace(request) => {
            valid_identity(&request.source.run_id, &request.source.attempt_id)
                && valid_repo(&request.source.repository)
                && valid_base(&request.source.base_branch, &request.source.base_commit)
                && valid_digest(&request.source_digest)
                && valid_commit(&request.expected_commit)
        }
        Operation::CreatePullRequest(request) | Operation::LookupPullRequest(request) => {
            valid_identity(&request.run_id, &request.attempt_id)
                && valid_repo(&request.repository)
                && valid_branch(&request.run_id, &request.branch)
                && valid_base(&request.base_branch, &request.base_commit)
                && valid_digest(&request.artifact_digest)
                && valid_commit(&request.frozen_commit)
        }
    };
    if valid {
        Ok(())
    } else {
        Err(NativeExecutorError::InvalidRequest)
    }
}

fn prepared_receipt_matches(
    request: &coding_agent_executor::PrepareRequest,
    receipt: &PreparedWorkspace,
    config: &TrustedConfig,
) -> bool {
    receipt.run_id == request.run_id
        && receipt.attempt_id == request.attempt_id
        && receipt.repository == request.repository
        && receipt.base_branch == request.base_branch
        && receipt.base_commit == request.base_commit
        && receipt.branch == request.expected_branch()
        && config
            .workspace_root
            .join(&request.run_id)
            .join(&request.attempt_id)
            .to_str()
            == Some(receipt.workspace_path.as_str())
}

fn base_resolution_receipt_matches(
    request: &ResolveBaseRequest,
    receipt: &BaseResolutionReceipt,
    trusted_config_digest: &str,
) -> bool {
    receipt.run_id == request.run_id
        && receipt.attempt_id == request.attempt_id
        && receipt.operation_id == request.operation_id
        && receipt.repository == request.repository
        && (request.base_branch.is_empty() || receipt.base_branch == request.base_branch)
        && is_commit(&receipt.base_commit)
        && receipt.trusted_config_digest == trusted_config_digest
        && receipt.protocol_version == PROTOCOL_VERSION
        && receipt.executor_version == EXECUTOR_VERSION
}

fn capture_receipt_matches(request: &CaptureRequest, receipt: &CaptureReceipt) -> bool {
    receipt.run_id == request.run_id
        && receipt.attempt_id == request.attempt_id
        && receipt.repository == request.repository
        && receipt.base_branch == request.base_branch
        && receipt.base_commit == request.base_commit
        && receipt.approved_snapshot_digest == request.approved_snapshot_digest
        && receipt.scope_digest == request.scope_digest
        && receipt.intent_approval_digest == request.intent_approval_digest
        && is_sha256(&receipt.manifest_digest)
        && is_sha256(&receipt.finalized_bytes_digest)
        && receipt.capture_guarantee == CaptureGuarantee::HostIsolatedAfterSeal
}

fn sealed_receipt_matches(
    request: &SealRequest,
    receipt: &SealedArtifactReceipt,
    config: &TrustedConfig,
) -> bool {
    receipt.run_id == request.run_id
        && receipt.attempt_id == request.attempt_id
        && receipt.manifest_digest == request.manifest_digest
        && approved_repository(config, &receipt.repository, &receipt.base_branch)
        && valid_artifact_fields(
            &receipt.base_commit,
            &receipt.approved_snapshot_digest,
            &receipt.scope_digest,
            &receipt.intent_approval_digest,
            &receipt.finalized_bytes_digest,
            &receipt.tree,
            &receipt.commit,
            &receipt.artifact_digest,
        )
        && receipt.protocol_version == PROTOCOL_VERSION
        && receipt.executor_version == EXECUTOR_VERSION
        && receipt.capture_guarantee == CaptureGuarantee::HostIsolatedAfterSeal
}

fn resolved_receipt_matches(
    request: &ResolveRequest,
    receipt: &ResolvedArtifact,
    config: &TrustedConfig,
) -> bool {
    receipt.run_id == request.run_id
        && receipt.attempt_id == request.attempt_id
        && receipt.artifact_digest == request.artifact_digest
        && coding_agent_executor::branch_belongs_to_run(&receipt.branch, &request.run_id)
        && approved_repository(config, &receipt.repository, &receipt.base_branch)
        && valid_artifact_fields(
            &receipt.base_commit,
            &receipt.approved_snapshot_digest,
            &receipt.scope_digest,
            &receipt.intent_approval_digest,
            &receipt.finalized_bytes_digest,
            &receipt.tree,
            &receipt.commit,
            &receipt.artifact_digest,
        )
        && receipt.protocol_version == PROTOCOL_VERSION
        && receipt.executor_version == EXECUTOR_VERSION
        && receipt.capture_guarantee == CaptureGuarantee::HostIsolatedAfterSeal
}

fn push_receipt_matches(request: &PushRequest, receipt: &PushReceipt) -> bool {
    receipt.run_id == request.run_id
        && receipt.attempt_id == request.attempt_id
        && receipt.repository == request.repository
        && receipt.branch == request.branch
        && receipt.commit == request.frozen_commit
        && receipt.remote_head == request.frozen_commit
        && receipt.artifact_digest == request.artifact_digest
        && receipt.secret_scan == coding_agent_executor::SecretScanStatus::CleanSupportedFormatsV1
}

fn pull_request_receipt_matches(
    request: &CreatePullRequestRequest,
    receipt: &PullRequestReceipt,
    config: &TrustedConfig,
) -> bool {
    let repository = config
        .repositories
        .iter()
        .find(|repository| repository.id == request.repository);
    let Some(forge) = repository.and_then(|repository| repository.forge.as_ref()) else {
        return false;
    };
    let Ok(url) = url::Url::parse(&receipt.url) else {
        return false;
    };
    let Ok(web_base) = url::Url::parse(&forge.web_base_url) else {
        return false;
    };
    receipt.run_id == request.run_id
        && receipt.attempt_id == request.attempt_id
        && receipt.repository == request.repository
        && receipt.number > 0
        && receipt.head_commit == request.frozen_commit
        && receipt.artifact_digest == request.artifact_digest
        && receipt.base_branch == request.base_branch
        && url.scheme() == web_base.scheme()
        && url.host_str() == web_base.host_str()
        && url.port_or_known_default() == web_base.port_or_known_default()
        && url.username().is_empty()
        && url.password().is_none()
}

fn approved_repository(
    config: &TrustedConfig,
    repository: &RepositoryId,
    base_branch: &str,
) -> bool {
    config
        .repositories
        .iter()
        .any(|approved: &RepositoryConfig| {
            approved.id == *repository
                && approved
                    .allowed_base_branches
                    .iter()
                    .any(|branch| branch == base_branch)
        })
}

fn repository_check_result_matches(
    request: &CheckRepositoryRequest,
    result: &RepositoryCheckResult,
    trusted_config_digest: &str,
    config: &TrustedConfig,
) -> bool {
    if result.repository != request.repository
        || result.trusted_config_digest != trusted_config_digest
        || result.protocol_version != PROTOCOL_VERSION
        || result.executor_version != EXECUTOR_VERSION
        || result.stages.len() != 2
    {
        return false;
    }
    let checks = request.checks.clone().unwrap_or(CheckRepositoryChecks {
        git_read: true,
        forge_api: true,
    });
    let expected: [(&str, bool, &[&str]); 2] = [
        (
            "git_read",
            checks.git_read,
            &[
                "git_readable",
                "git_failed",
                "credential_unavailable",
                "timeout",
                "response_too_large",
                "unavailable",
            ],
        ),
        (
            "forge_api",
            checks.forge_api,
            &[
                "forge_readable",
                "forge_not_configured",
                "credential_not_configured",
                "credential_unavailable",
                "authentication_failed",
                "access_denied",
                "repository_not_found",
                "redirect_rejected",
                "invalid_response",
                "timeout",
                "response_too_large",
                "unavailable",
            ],
        ),
    ];
    result
        .stages
        .iter()
        .zip(expected)
        .all(|(stage, (name, enabled, codes))| {
            if !enabled {
                return stage.stage == name
                    && stage.status == "skipped"
                    && stage.code == "prerequisite_failed";
            }
            if stage.stage != name
                || !codes.contains(&stage.code.as_str())
                || !matches!(stage.status.as_str(), "passed" | "failed" | "skipped")
            {
                return false;
            }
            match stage.status.as_str() {
                "passed" => stage.code.ends_with("readable"),
                "failed" => !stage.code.ends_with("readable"),
                "skipped" => matches!(
                    stage.code.as_str(),
                    "forge_not_configured" | "credential_not_configured"
                ),
                _ => false,
            }
        })
        && approved_repository_without_branch(config, &result.repository)
}

fn approved_repository_without_branch(config: &TrustedConfig, repository: &RepositoryId) -> bool {
    config
        .repositories
        .iter()
        .any(|approved| approved.id == *repository)
}

#[allow(clippy::too_many_arguments)]
fn valid_artifact_fields(
    base_commit: &str,
    approved_snapshot_digest: &str,
    scope_digest: &str,
    intent_approval_digest: &str,
    finalized_bytes_digest: &str,
    tree: &str,
    commit: &str,
    artifact_digest: &str,
) -> bool {
    is_commit(base_commit)
        && is_sha256(approved_snapshot_digest)
        && is_sha256(scope_digest)
        && is_sha256(intent_approval_digest)
        && is_sha256(finalized_bytes_digest)
        && is_commit(tree)
        && is_commit(commit)
        && is_sha256(artifact_digest)
}

fn is_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(is_lower_hex)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(is_lower_hex)
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn valid_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(is_lower_hex)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn outcome_unknown(operation: &'static str) -> NativeExecutorError {
    NativeExecutorError::OutcomeUnknown { operation }
}

fn operation_name(operation: &Operation) -> &'static str {
    match operation {
        Operation::CheckRepository(_) => "check_repository",
        Operation::ResolveBase(_) => "resolve_base",
        Operation::Prepare(_) => "prepare",
        Operation::InspectSource(_) => "inspect_source",
        Operation::Capture(_) => "capture",
        Operation::Seal(_) => "seal",
        Operation::Resolve(_) => "resolve",
        Operation::Push(_) => "push",
        Operation::PushWorkspace(_) => "push_workspace",
        Operation::LookupPush(_) => "lookup_push",
        Operation::LookupPullRequest(_) => "lookup_pull_request",
        Operation::CreatePullRequest(_) => "create_pull_request",
    }
}

#[cfg(all(test, unix))]
fn secure_test_tempdir(prefix: &str) -> tempfile::TempDir {
    let candidates = [
        Path::new("/dev"),
        Path::new(env!("CARGO_MANIFEST_DIR")),
        Path::new("/tmp"),
    ];
    for base in candidates {
        if !secure_test_parent_chain(base) {
            continue;
        }
        if let Ok(directory) = tempfile::Builder::new().prefix(prefix).tempdir_in(base) {
            return directory;
        }
    }
    panic!("no writable secure test directory is available");
}

#[cfg(all(test, unix))]
fn secure_test_parent_chain(path: &Path) -> bool {
    let mut current = PathBuf::from("/");
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        let Ok(metadata) = fs::symlink_metadata(&current) else {
            return false;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() || !safe_directory(&metadata) {
            return false;
        }
    }
    true
}

#[cfg(all(test, unix))]
mod metadata_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn repository_metadata_requires_valid_configuration_without_legacy_publication_features()
    {
        let root = secure_test_tempdir("coding-agent-metadata-");
        let executable = root.path().join("helper");
        let document = serde_json::json!({"protocol_version": PROTOCOL_VERSION,"executor_version":EXECUTOR_VERSION,
            "features":["prepare","push_workspace","lookup_publication"]});
        fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", document),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let repository = RepositoryId {
            host: "example.test".into(),
            owner: "owner".into(),
            name: "repo".into(),
        };
        let config = TrustedConfig {
            protocol_version: PROTOCOL_VERSION,
            expected_executor_version: EXECUTOR_VERSION.into(),
            workspace_root: root.path().join("workspace"),
            publisher_root: root.path().join("publisher"),
            state_root: root.path().join("state"),
            credential_root: root.path().join("credentials"),
            max_import_bytes: 1024,
            repositories: vec![RepositoryConfig {
                id: repository.clone(),
                clone_url: "https://example.test/owner/repo.git".into(),
                ssh_directory: None,
                allowed_base_branches: vec!["main".into()],
                read_credential_source: None,
                publish_credential_source: None,
                forge: None,
            }],
            credential_sources: vec![],
        };
        for path in [
            &config.workspace_root,
            &config.publisher_root,
            &config.state_root,
            &config.credential_root,
        ] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = root.path().join("config.json");
        fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let client = NativeExecutorClient::connect(
            executable.clone(),
            path.clone(),
            format!("{:x}", Sha256::digest(fs::read(&executable).unwrap())),
        )
        .await
        .unwrap();
        let metadata = client
            .publication_repository_config(&repository)
            .expect("metadata does not dispatch a legacy publication operation");
        assert!(metadata.allowed_base_branches.contains("main"));
        assert_eq!(metadata.publication_credential_reference, None);
        let mut unknown = repository.clone();
        unknown.name = "another".into();
        assert_eq!(
            client.publication_repository_config(&unknown).unwrap_err(),
            NativeExecutorError::InvalidTrustedConfiguration
        );
        fs::write(&path, b"{}").unwrap();
        assert!(client.publication_repository_config(&repository).is_err());
    }
}

#[cfg(all(test, unix))]
mod transport_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn test_client(executable: PathBuf) -> NativeExecutorClient {
        NativeExecutorClient {
            executable,
            trusted_config: PathBuf::new(),
            expected_executable_sha256: String::new(),
            trusted_config_sha256: String::new(),
            verified_capabilities: VerifiedNativeCapabilities {
                executor_version: String::new(),
                protocol_version: 0,
                features: BTreeSet::new(),
                private_config_binding: String::new(),
                executable_path: PathBuf::new(),
                executable_sha256: String::new(),
            },
            preparation_workflow: None,
        }
    }

    fn write_helper(root: &Path, body: &str) -> PathBuf {
        let executable = root.join("diagnostic-helper");
        fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        executable
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    #[tokio::test]
    async fn diagnostic_timeout_kills_the_helper_process_group() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("child-survived");
        let marker = shell_quote(&marker);
        let executable = write_helper(
            root.path(),
            &format!("(sleep 1; printf survived > {marker}) &\nwait"),
        );
        let client = test_client(executable);

        let result = client
            .invoke(
                &[],
                None,
                root.path(),
                InvocationPurpose::TestOperation {
                    operation: "check_repository",
                    timeout: Duration::from_millis(50),
                    response_limit: 64,
                },
            )
            .await;

        assert_eq!(
            result,
            Err(NativeExecutorError::DiagnosticTransport {
                reason: NativeDiagnosticTransportReason::Timeout,
            })
        );
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(!root.path().join("child-survived").exists());
    }

    #[tokio::test]
    async fn diagnostic_stdout_over_limit_is_classified_without_child_output() {
        let root = tempfile::tempdir().unwrap();
        let executable = write_helper(root.path(), "/usr/bin/head -c 128 /dev/zero");
        let client = test_client(executable);

        let result = client
            .invoke(
                &[],
                None,
                root.path(),
                InvocationPurpose::TestOperation {
                    operation: "check_repository",
                    timeout: Duration::from_secs(1),
                    response_limit: 64,
                },
            )
            .await;

        assert_eq!(
            result,
            Err(NativeExecutorError::DiagnosticTransport {
                reason: NativeDiagnosticTransportReason::ResponseTooLarge,
            })
        );
    }

    #[tokio::test]
    async fn mutation_transport_failure_remains_outcome_unknown() {
        let root = tempfile::tempdir().unwrap();
        let executable = write_helper(root.path(), "sleep 1");
        let client = test_client(executable);

        let result = client
            .invoke(
                &[],
                None,
                root.path(),
                InvocationPurpose::TestOperation {
                    operation: "prepare",
                    timeout: Duration::from_millis(50),
                    response_limit: 64,
                },
            )
            .await;

        assert_eq!(
            result,
            Err(NativeExecutorError::OutcomeUnknown {
                operation: "prepare"
            })
        );
    }

    #[tokio::test]
    async fn capability_probe_does_not_require_credential_contents() {
        let root = secure_test_tempdir("coding-agent-probe-");
        let operation_marker = shell_quote(&root.path().join("operation-invoked"));
        let executable = write_helper(
            root.path(),
            &format!(
                "if [ \"$1\" = \"--capabilities\" ]; then printf '%s\\n' '{}'; else printf invoked > {operation_marker}; fi",
                serde_json::json!({
                    "protocol_version": PROTOCOL_VERSION,
                    "executor_version": EXECUTOR_VERSION,
                    "features": ["prepare"],
                })
            ),
        );
        let roots = [
            root.path().join("workspace"),
            root.path().join("publisher"),
            root.path().join("state"),
            root.path().join("credentials"),
        ];
        for path in &roots {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let repository = RepositoryId {
            host: "example.test".into(),
            owner: "owner".into(),
            name: "repo".into(),
        };
        let config = TrustedConfig {
            protocol_version: PROTOCOL_VERSION,
            expected_executor_version: EXECUTOR_VERSION.into(),
            workspace_root: roots[0].clone(),
            publisher_root: roots[1].clone(),
            state_root: roots[2].clone(),
            credential_root: roots[3].clone(),
            max_import_bytes: 1024,
            repositories: vec![RepositoryConfig {
                id: repository,
                clone_url: "https://example.test/owner/repo.git".into(),
                ssh_directory: None,
                allowed_base_branches: vec!["main".into()],
                read_credential_source: Some("read".into()),
                publish_credential_source: None,
                forge: None,
            }],
            credential_sources: vec![coding_agent_executor::CredentialSourceConfig {
                id: "read".into(),
                username: "user".into(),
                token_file: PathBuf::from("missing.token"),
            }],
        };
        let config_path = root.path().join("trusted.json");
        fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();

        let capabilities = NativeExecutorClient::probe(
            executable.clone(),
            config_path.clone(),
            sha256(&fs::read(&executable).unwrap()),
        )
        .await
        .expect("capability probe must not read token contents");

        assert!(capabilities.feature_set().contains("prepare"));

        let mut snapshot_config = config;
        snapshot_config.repositories[0].read_credential_source = Some("read".into());
        fs::write(&config_path, serde_json::to_vec(&snapshot_config).unwrap()).unwrap();
        let executable_digest = sha256(&fs::read(&executable).unwrap());
        let client = NativeExecutorClient::connect_with_capabilities(
            executable.clone(),
            config_path.clone(),
            executable_digest.clone(),
            capabilities.clone(),
        )
        .unwrap();
        let check_result = client
            .check_repository(CheckRepositoryRequest {
                repository: RepositoryId {
                    host: "example.test".into(),
                    owner: "owner".into(),
                    name: "repo".into(),
                },
                checks: None,
            })
            .await;
        assert_eq!(
            check_result.err(),
            Some(NativeExecutorError::UnsupportedCapability {
                capability: "check_repository"
            })
        );
        assert!(!root.path().join("operation-invoked").exists());

        let replacement = root.path().join("replacement-helper");
        fs::copy(&executable, &replacement).unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            NativeExecutorClient::connect_with_capabilities(
                replacement,
                config_path.clone(),
                executable_digest.clone(),
                capabilities.clone(),
            )
            .err(),
            Some(NativeExecutorError::InvalidTrustedConfiguration)
        );

        fs::write(&executable, b"changed helper").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            NativeExecutorClient::connect_with_capabilities(
                executable,
                config_path,
                executable_digest,
                capabilities,
            )
            .err(),
            Some(NativeExecutorError::InvalidPackage)
        );
    }
}

#[cfg(test)]
mod repository_check_binding_tests {
    use super::*;
    use coding_agent_executor::RepositoryCheckStage;

    fn repository() -> RepositoryId {
        RepositoryId {
            host: "example.test".into(),
            owner: "owner".into(),
            name: "repo".into(),
        }
    }

    fn config(repository: &RepositoryId) -> TrustedConfig {
        TrustedConfig {
            protocol_version: PROTOCOL_VERSION,
            expected_executor_version: EXECUTOR_VERSION.into(),
            workspace_root: PathBuf::from("/tmp/check-workspace"),
            publisher_root: PathBuf::from("/tmp/check-publisher"),
            state_root: PathBuf::from("/tmp/check-state"),
            credential_root: PathBuf::from("/tmp/check-credentials"),
            max_import_bytes: 1024,
            repositories: vec![RepositoryConfig {
                id: repository.clone(),
                clone_url: "https://example.test/owner/repo.git".into(),
                ssh_directory: None,
                allowed_base_branches: vec!["main".into()],
                read_credential_source: None,
                publish_credential_source: None,
                forge: None,
            }],
            credential_sources: Vec::new(),
        }
    }

    fn result(repository: RepositoryId) -> RepositoryCheckResult {
        RepositoryCheckResult {
            repository,
            trusted_config_digest: "a".repeat(64),
            protocol_version: PROTOCOL_VERSION,
            executor_version: EXECUTOR_VERSION.into(),
            stages: vec![
                RepositoryCheckStage {
                    stage: "git_read".into(),
                    status: "skipped".into(),
                    code: "prerequisite_failed".into(),
                },
                RepositoryCheckStage {
                    stage: "forge_api".into(),
                    status: "passed".into(),
                    code: "forge_readable".into(),
                },
            ],
        }
    }

    #[test]
    fn check_result_binding_rejects_tampered_identity_and_stage_contract() {
        let repository = repository();
        let request = CheckRepositoryRequest {
            repository: repository.clone(),
            checks: Some(CheckRepositoryChecks {
                git_read: false,
                forge_api: true,
            }),
        };
        let trusted_config_digest = "a".repeat(64);
        let config = config(&repository);
        assert!(repository_check_result_matches(
            &request,
            &result(repository.clone()),
            &trusted_config_digest,
            &config,
        ));

        let mut tampered = vec![result(repository.clone()); 8];
        tampered[0].repository.name = "other".into();
        tampered[1].trusted_config_digest = "b".repeat(64);
        tampered[2].protocol_version = 1;
        tampered[3].executor_version = "other".into();
        tampered[4].stages.swap(0, 1);
        tampered[5].stages[1].stage = "git_read".into();
        tampered[6].stages[0].status = "passed".into();
        tampered[7].stages[1].code = "unknown".into();

        for candidate in tampered {
            assert!(!repository_check_result_matches(
                &request,
                &candidate,
                &trusted_config_digest,
                &config,
            ));
        }
    }
}
