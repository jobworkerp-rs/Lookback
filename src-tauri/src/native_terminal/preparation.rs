use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use coding_agent_executor::{
    BaseResolutionReceipt, PrepareRequest, PreparedWorkspace, RepositoryId, ResolveBaseRequest,
};
use serde_json::json;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::{
    coding_agent::{AgentProfileSnapshot, HostResourceLimits, RepositoryIdentity},
    jobworkerp::{
        JobworkerpHandle,
        coding_agent::{
            HostWorkerContext, NativeInteractiveWorkerSpec, NativeProfileSettingsWorkerSpec,
            NativeWorkerMatch, NativeWorkspaceBinding,
        },
    },
};

use super::{
    ManagedNativeSessionAuthority, NativeAuthorityError, NativeAuthoritySnapshot,
    NativeSessionAuthority, NativeSessionContext, NativeSessionContextInput, NativeTerminalManager,
    NativeTerminalPreview, NativeWorkerBinding,
    bootstrap::{
        ConfiguredNativeRepository, NativeWorkspaceExecutor, NativeWorkspaceExecutorError,
    },
    doctor_store::NativeRuntimeDoctorStore,
    launch::{NativeAgentKind, NativeCliProfile, WorkspaceMountBinding, build_launch_plan},
};

const NATIVE_WORKER_NAME_PREFIX: &str = "lookback.interactive.";
const NATIVE_GUEST_WORKSPACE_ROOT: &str = "/workspace";
const MAX_OWNER_WINDOW_LABEL_BYTES: usize = 256;
const AUTHORITY_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Native-only SDK locations and host resource limits selected by trusted host
/// configuration. This is not a runtime readiness observation.
#[derive(Clone)]
pub struct NativePreparationRuntime {
    host_worker_context: HostWorkerContext,
    host_limits: HostResourceLimits,
}

impl std::fmt::Debug for NativePreparationRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativePreparationRuntime")
            .field("host_worker_context", &"<private native runtime context>")
            .field("host_limits", &self.host_limits)
            .finish()
    }
}

impl NativePreparationRuntime {
    pub fn new(
        host_worker_context: HostWorkerContext,
        host_limits: HostResourceLimits,
    ) -> Result<Self, NativePreparationError> {
        if host_limits.cpu_millis == 0
            || host_limits.memory_mib == 0
            || host_limits.root_disk_mib == 0
            || host_limits.max_parallel_runs == 0
            || !canonical_existing_host_path(&host_worker_context.microsandbox_sdk_home)
                .is_some_and(|path| path.is_dir())
            || !valid_sdk_config_location(&host_worker_context.backend_config_path)
        {
            return Err(NativePreparationError::RuntimeContextUnavailable);
        }
        Ok(Self {
            host_worker_context,
            host_limits,
        })
    }
}

/// Host-side native preparation inputs. The owner window remains a separate
/// trusted argument to `prepare` and is never selected by this request value.
pub struct NativePrepareRequest<'a> {
    pub issue_id: &'a str,
    pub profile_id: &'a str,
    pub base_branch: &'a str,
    pub edited_prompt: Option<&'a str>,
}

/// Preparation-time canonical data source. The production implementation below
/// delegates to the host-managed Issue Tracker and settings authority.
#[async_trait]
#[allow(clippy::double_must_use)]
pub trait NativePreparationAuthority: NativeSessionAuthority {
    async fn read_for_preparation(
        &self,
        issue_id: &str,
        profile_id: &str,
        host_limits: HostResourceLimits,
    ) -> Result<NativeAuthoritySnapshot, NativeAuthorityError>;
}

#[async_trait]
impl NativePreparationAuthority for ManagedNativeSessionAuthority {
    async fn read_for_preparation(
        &self,
        issue_id: &str,
        profile_id: &str,
        host_limits: HostResourceLimits,
    ) -> Result<NativeAuthoritySnapshot, NativeAuthorityError> {
        ManagedNativeSessionAuthority::read_for_preparation(self, issue_id, profile_id, host_limits)
            .await
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativePreparationError {
    #[error("native preparation host runtime context is unavailable")]
    RuntimeContextUnavailable,
    #[error("native preparation request is invalid")]
    InvalidRequest,
    #[error("canonical native session authority is unavailable")]
    AuthorityUnavailable,
    #[error("canonical native session authority returned inconsistent data")]
    InvalidAuthoritySnapshot,
    #[error("the selected profile has no writable home mount for the settings shell")]
    ProfileHomeUnavailable,
    #[error("the current Issue repository differs from the configured native repository")]
    RepositoryMismatch,
    #[error("the current profile does not match the verified interactive CLI recipe")]
    UnsupportedCliProfile,
    #[error("this native agent profile uses an unsupported host credential reference")]
    UnsupportedProfileCredentialReference,
    #[error("the requested base branch is not approved by native host policy")]
    UnapprovedBaseBranch,
    #[error("a persistent profile mount overlaps a protected native host path")]
    UnsafePersistentMount,
    #[error("canonical native session authority changed during preparation")]
    AuthorityChanged,
    #[error("the edited initial prompt is invalid or exceeds the byte limit")]
    InvalidPrompt,
    #[error("native session identifiers could not be generated")]
    IdentityUnavailable,
    #[error("the approved native workspace could not be prepared")]
    WorkspaceUnavailable,
    #[error("the native interactive worker could not be verified")]
    WorkerUnavailable,
    #[error("the native terminal context could not be validated")]
    InvalidContext,
    #[error("the native terminal preview could not be staged")]
    PreviewUnavailable,
}

pub struct PreparedAgentWorkspace {
    pub authority: NativeAuthoritySnapshot,
    pub base_resolution: BaseResolutionReceipt,
    pub workspace: PreparedWorkspace,
    pub worker: NativeWorkerMatch,
    pub mount_binding: WorkspaceMountBinding,
}

/// Owns the configured native repository's real executor and stores, and stages
/// immutable previews without starting a Jobworkerp session.
pub struct NativeSessionPreparer<A> {
    repository: RepositoryId,
    workspace_root: std::path::PathBuf,
    allowed_base_branches: Vec<String>,
    protected_host_roots: Vec<PathBuf>,
    executor: NativeWorkspaceExecutor,
    authority: Arc<A>,
    jobworkerp: JobworkerpHandle,
    runtime: NativePreparationRuntime,
    manager: NativeTerminalManager,
    doctor_store: Arc<NativeRuntimeDoctorStore>,
}

impl<A> std::fmt::Debug for NativeSessionPreparer<A> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeSessionPreparer")
            .field("repository", &self.repository)
            .field("workspace_root", &"<private native workspace root>")
            .finish_non_exhaustive()
    }
}

impl<A> NativeSessionPreparer<A>
where
    A: NativePreparationAuthority + 'static,
{
    pub fn new(
        configured_repository: ConfiguredNativeRepository,
        authority: Arc<A>,
        jobworkerp: JobworkerpHandle,
        runtime: NativePreparationRuntime,
        shared_admission: Arc<Mutex<()>>,
    ) -> Self {
        let (
            repository,
            workspace_root,
            preparation_policy,
            executor,
            session_store,
            output_store,
            doctor_store,
        ) = configured_repository.into_parts();
        let executor = executor.with_preparation_workflow(jobworkerp.clone());
        let mut protected_host_roots = preparation_policy.protected_host_roots;
        protected_host_roots.push(runtime.host_worker_context.microsandbox_sdk_home.clone());
        protected_host_roots.push(runtime.host_worker_context.backend_config_path.clone());
        let manager_authority: Arc<dyn NativeSessionAuthority> = authority.clone();
        let manager = NativeTerminalManager::new(
            session_store,
            output_store,
            jobworkerp.clone(),
            manager_authority,
            shared_admission,
        );
        Self {
            repository,
            workspace_root,
            allowed_base_branches: preparation_policy.allowed_base_branches,
            protected_host_roots,
            executor,
            authority,
            jobworkerp,
            runtime,
            manager,
            doctor_store,
        }
    }

    /// Return a clone backed by the configured repository's same durable stores
    /// and shared in-memory session state.
    pub fn manager(&self) -> NativeTerminalManager {
        self.manager.clone()
    }

    pub(crate) fn doctor_store(&self) -> Arc<NativeRuntimeDoctorStore> {
        Arc::clone(&self.doctor_store)
    }

    /// Branch choices copied from this repository's verified host policy.
    pub fn allowed_base_branches(&self) -> &[String] {
        &self.allowed_base_branches
    }

    /// Re-read host authority, prepare a fresh repository workspace, verify the
    /// stable native worker, and stage a preview without starting an agent session.
    pub async fn prepare(
        &self,
        owner_window_label: &str,
        request: NativePrepareRequest<'_>,
    ) -> Result<NativeTerminalPreview, NativePreparationError> {
        validate_request(
            owner_window_label,
            request.issue_id,
            request.profile_id,
            request.edited_prompt,
        )?;
        let issue_id = request.issue_id;
        let profile_id = request.profile_id;
        let edited_prompt = request.edited_prompt;
        let prepared = self
            .prepare_workspace(request, false, self.authority.as_ref())
            .await?;
        let authority = prepared.authority;
        let initial_authority = authority.clone();
        let profile = &authority.profile;
        let cli_profile = native_cli_profile(profile)?;
        let matched = prepared.worker;
        let worker_binding = native_worker_binding(&matched, profile)?;
        let prepared_workspace = prepared.workspace;
        let base_resolution_receipt = prepared.base_resolution;
        let session_id = prepared_workspace.run_id.clone();
        let attempt_id = prepared_workspace.attempt_id.clone();
        let workspace_mount_binding = prepared.mount_binding;
        let launch_plan = build_launch_plan(
            profile,
            &cli_profile,
            &prepared_workspace,
            &workspace_mount_binding,
            &authority.canonical_issue,
            edited_prompt,
        )
        .map_err(map_launch_error)?;
        let preview = NativeTerminalPreview::from_launch(
            new_uuid()?,
            authority.settings_revision,
            authority.canonical_issue.revision(),
            Some(authority.repository.identity.clone()),
            &launch_plan,
            Some(worker_binding),
        )
        .map_err(|_| NativePreparationError::InvalidContext)?;
        let context = NativeSessionContext::new(NativeSessionContextInput {
            session_id,
            attempt_id,
            owner_window_label: owner_window_label.to_owned(),
            canonical_issue: authority.canonical_issue,
            repository: authority.repository,
            base_resolution_receipt,
            prepared_workspace,
            profile: authority.profile,
            cli_profile,
            workspace_mount_binding,
            launch_plan,
            preview: preview.clone(),
        })
        .map_err(|_| NativePreparationError::InvalidContext)?;

        // Keep the same admission lock used by Settings CAS from the fresh
        // canonical read through synchronous preview persistence and staging.
        // The long executor and worker RPCs above deliberately run unlocked.
        let admission = self.manager.admission_lock();
        let _admission_guard = admission.lock().await;
        self.verify_authority(issue_id, profile_id, &initial_authority)
            .await?;
        self.manager
            .register_preview(context, &matched)
            .map_err(|_| NativePreparationError::PreviewUnavailable)?;
        Ok(preview)
    }

    pub async fn prepare_automatic(
        &self,
        request: NativePrepareRequest<'_>,
    ) -> Result<PreparedAgentWorkspace, NativePreparationError> {
        self.prepare_automatic_with_authority(request, self.authority.as_ref())
            .await
    }

    pub(crate) async fn prepare_automatic_with_authority(
        &self,
        request: NativePrepareRequest<'_>,
        authority: &A,
    ) -> Result<PreparedAgentWorkspace, NativePreparationError> {
        let issue_id = request.issue_id;
        let profile_id = request.profile_id;
        let prepared = self.prepare_workspace(request, true, authority).await?;
        let admission = self.manager.admission_lock();
        let _guard = admission.lock().await;
        self.verify_authority_with(issue_id, profile_id, &prepared.authority, authority)
            .await?;
        Ok(prepared)
    }

    pub(crate) fn workspace_executor(&self) -> NativeWorkspaceExecutor {
        self.executor.clone()
    }

    pub(crate) async fn verify_authority(
        &self,
        issue_id: &str,
        profile_id: &str,
        initial_authority: &NativeAuthoritySnapshot,
    ) -> Result<(), NativePreparationError> {
        self.verify_authority_with(
            issue_id,
            profile_id,
            initial_authority,
            self.authority.as_ref(),
        )
        .await
    }

    async fn verify_authority_with(
        &self,
        issue_id: &str,
        profile_id: &str,
        initial_authority: &NativeAuthoritySnapshot,
        authority: &A,
    ) -> Result<(), NativePreparationError> {
        let current = tokio::time::timeout(
            AUTHORITY_READ_TIMEOUT,
            authority.read_for_preparation(issue_id, profile_id, self.runtime.host_limits),
        )
        .await
        .map_err(|_| NativePreparationError::AuthorityUnavailable)?
        .map_err(|_| NativePreparationError::AuthorityUnavailable)?;
        if !same_authority_snapshot(initial_authority, &current) {
            return Err(NativePreparationError::AuthorityChanged);
        }
        Ok(())
    }

    async fn prepare_workspace(
        &self,
        request: NativePrepareRequest<'_>,
        automatic: bool,
        preparation_authority: &A,
    ) -> Result<PreparedAgentWorkspace, NativePreparationError> {
        let NativePrepareRequest {
            issue_id,
            profile_id,
            base_branch,
            edited_prompt,
        } = request;
        validate_request("workspace-preparation", issue_id, profile_id, edited_prompt)?;
        if !base_branch.is_empty()
            && !self
                .allowed_base_branches
                .iter()
                .any(|approved| approved == base_branch)
        {
            return Err(NativePreparationError::UnapprovedBaseBranch);
        }
        let authority = preparation_authority.read_for_preparation(
            issue_id,
            profile_id,
            self.runtime.host_limits,
        );
        let authority = tokio::time::timeout(AUTHORITY_READ_TIMEOUT, authority)
            .await
            .map_err(|_| NativePreparationError::AuthorityUnavailable)?
            .map_err(|_| NativePreparationError::AuthorityUnavailable)?;
        validate_authority_snapshot(
            &authority,
            issue_id,
            profile_id,
            self.runtime.host_limits,
            &self.repository,
        )?;
        let profile = &authority.profile;
        if profile.credential_reference.is_some() {
            return Err(NativePreparationError::UnsupportedProfileCredentialReference);
        }
        validate_persistent_mounts(profile, &self.protected_host_roots)?;
        prepare_runtime_tool_homes(profile)?;
        let session_id = new_uuid()?;
        let attempt_id = new_uuid()?;
        let repository = repository_id(&authority.repository.identity);
        if !self.executor.supports_resolve_base() {
            return Err(NativePreparationError::WorkspaceUnavailable);
        }
        let workspace_mount_binding = WorkspaceMountBinding {
            canonical_host_root: self.workspace_root.clone(),
            guest_root: NATIVE_GUEST_WORKSPACE_ROOT.to_owned(),
        };
        let worker_spec = native_worker_spec(
            profile,
            &authority.repository.identity,
            &workspace_mount_binding,
            &self.runtime.host_worker_context,
        )?;
        let matched = if automatic {
            self.jobworkerp
                .ensure_native_one_shot_worker(worker_spec)
                .await
        } else {
            self.jobworkerp
                .ensure_native_interactive_worker(worker_spec)
                .await
        }
        .map_err(|_| NativePreparationError::WorkerUnavailable)?;

        let base_resolution_receipt = self
            .executor
            .resolve_base(ResolveBaseRequest {
                run_id: session_id.clone(),
                attempt_id: attempt_id.clone(),
                operation_id: new_uuid()?,
                repository: repository.clone(),
                base_branch: base_branch.to_owned(),
            })
            .await
            .map_err(map_executor_error)?;
        let prepared_workspace = self
            .executor
            .prepare(PrepareRequest {
                issue_id: Some(authority.canonical_issue.issue_id().to_owned()),
                run_id: session_id.clone(),
                attempt_id: attempt_id.clone(),
                repository,
                base_branch: base_resolution_receipt.base_branch.clone(),
                base_commit: base_resolution_receipt.base_commit.clone(),
            })
            .await
            .map_err(map_executor_error)?;
        Ok(PreparedAgentWorkspace {
            authority,
            base_resolution: base_resolution_receipt,
            workspace: prepared_workspace,
            worker: matched,
            mount_binding: workspace_mount_binding,
        })
    }
}

fn validate_request(
    owner_window_label: &str,
    issue_id: &str,
    profile_id: &str,
    edited_prompt: Option<&str>,
) -> Result<(), NativePreparationError> {
    if owner_window_label.is_empty()
        || owner_window_label.len() > MAX_OWNER_WINDOW_LABEL_BYTES
        || owner_window_label.chars().any(char::is_control)
        || issue_id.is_empty()
        || issue_id.len() > super::MAX_ISSUE_ID_BYTES
        || issue_id.chars().any(char::is_control)
        || profile_id.is_empty()
        || profile_id.len() > super::MAX_OWNER_LABEL_BYTES
        || profile_id.chars().any(char::is_control)
    {
        return Err(NativePreparationError::InvalidRequest);
    }
    if edited_prompt.is_some_and(|prompt| {
        prompt.len() > super::MAX_NATIVE_PROMPT_BYTES || prompt.contains('\0')
    }) {
        return Err(NativePreparationError::InvalidPrompt);
    }
    Ok(())
}

pub(crate) fn same_authority_snapshot(
    expected: &NativeAuthoritySnapshot,
    current: &NativeAuthoritySnapshot,
) -> bool {
    expected.canonical_issue == current.canonical_issue
        && expected.repository == current.repository
        && expected.profile == current.profile
        && expected.settings_revision == current.settings_revision
}

pub(crate) fn same_authority_at_owned_revision(
    expected: &NativeAuthoritySnapshot,
    current: &NativeAuthoritySnapshot,
    owned_revision: u64,
) -> bool {
    if owned_revision < expected.canonical_issue.revision() {
        return false;
    }
    let issue = &expected.canonical_issue;
    let Ok(canonical_issue) = super::CanonicalIssueSnapshot::new(
        issue.issue_id(),
        owned_revision,
        issue.title(),
        issue.description(),
        issue.acceptance_criteria().to_vec(),
    ) else {
        return false;
    };
    let mut owned = expected.clone();
    owned.canonical_issue = canonical_issue;
    same_authority_snapshot(&owned, current)
}

fn validate_persistent_mounts(
    profile: &AgentProfileSnapshot,
    protected_roots: &[PathBuf],
) -> Result<(), NativePreparationError> {
    for mount in &profile.persistent_mounts {
        let source = canonical_existing_host_path(&mount.host_path)
            .ok_or(NativePreparationError::UnsafePersistentMount)?;
        if source.as_os_str() != mount.host_path.as_os_str()
            || !profile
                .canonical_mount_allowed_roots
                .iter()
                .any(|root| source.starts_with(root))
            || protected_roots
                .iter()
                .any(|root| paths_overlap(&source, root))
            || guest_paths_overlap(&mount.guest_path, NATIVE_GUEST_WORKSPACE_ROOT)
        {
            return Err(NativePreparationError::UnsafePersistentMount);
        }
    }
    if let Some(overlay) = &profile.managed_home_overlay {
        validate_overlay_mount(
            &overlay.parent.host_path,
            &overlay.parent.guest_path,
            profile,
            protected_roots,
        )?;
        for child in &overlay.children {
            let guest_path = child.guest_path(&profile.container_home);
            validate_overlay_mount(child.host_path(), &guest_path, profile, protected_roots)?;
        }
    }
    Ok(())
}

fn validate_overlay_mount(
    host_path: &Path,
    guest_path: &str,
    profile: &AgentProfileSnapshot,
    protected_roots: &[PathBuf],
) -> Result<(), NativePreparationError> {
    let source = canonical_existing_host_path(host_path)
        .ok_or(NativePreparationError::UnsafePersistentMount)?;
    if source.as_os_str() != host_path.as_os_str()
        || !profile
            .canonical_mount_allowed_roots
            .iter()
            .any(|root| root == &source)
        || protected_roots
            .iter()
            .any(|root| paths_overlap(&source, root))
        || guest_paths_overlap(guest_path, NATIVE_GUEST_WORKSPACE_ROOT)
    {
        return Err(NativePreparationError::UnsafePersistentMount);
    }
    Ok(())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn guest_paths_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn validate_authority_snapshot(
    snapshot: &NativeAuthoritySnapshot,
    issue_id: &str,
    profile_id: &str,
    host_limits: HostResourceLimits,
    configured_repository: &RepositoryId,
) -> Result<(), NativePreparationError> {
    if snapshot.canonical_issue.issue_id() != issue_id
        || snapshot.profile.profile_id != profile_id
        || snapshot.profile.validated_host_limits != host_limits
        || snapshot.profile.validate_snapshot().is_err()
        || snapshot.repository.validate().is_err()
    {
        return Err(NativePreparationError::InvalidAuthoritySnapshot);
    }
    if repository_id(&snapshot.repository.identity) != *configured_repository {
        return Err(NativePreparationError::RepositoryMismatch);
    }
    if !snapshot
        .profile
        .allowed_repositories
        .contains(&snapshot.repository.identity)
    {
        return Err(NativePreparationError::InvalidAuthoritySnapshot);
    }
    if NativeAgentKind::from_command(&snapshot.profile.command).is_none() {
        return Err(NativePreparationError::UnsupportedCliProfile);
    }
    Ok(())
}

fn native_cli_profile(
    profile: &AgentProfileSnapshot,
) -> Result<NativeCliProfile, NativePreparationError> {
    NativeCliProfile::from_command_and_image(&profile.command, &profile.image_digest)
        .ok_or(NativePreparationError::UnsupportedCliProfile)
}

fn native_worker_spec(
    profile: &AgentProfileSnapshot,
    repository: &RepositoryIdentity,
    workspace: &WorkspaceMountBinding,
    host_context: &HostWorkerContext,
) -> Result<NativeInteractiveWorkerSpec, NativePreparationError> {
    let workspace_binding = NativeWorkspaceBinding {
        canonical_host_root: workspace.canonical_host_root.clone(),
        guest_root: workspace.guest_root.clone(),
    };
    Ok(NativeInteractiveWorkerSpec {
        native_profile_id: profile.profile_id.clone(),
        repository_id: RepositoryId {
            host: repository.host.clone(),
            owner: repository.organization.clone(),
            name: repository.repository.clone(),
        },
        use_static: profile.resources.use_static,
        runner_settings_json: native_runner_settings(profile, &workspace_binding)?,
        workspace_binding,
        host_context: host_context.clone(),
    })
}

/// Create the tool homes (`CODEX_HOME`, ...) inside the profile's writable
/// mounts so the VM finds them; call after the mounts were validated and
/// right before the worker that mounts them is ensured.
pub(crate) fn prepare_runtime_tool_homes(
    profile: &AgentProfileSnapshot,
) -> Result<(), NativePreparationError> {
    profile
        .ensure_runtime_environment_directories()
        .map_err(|_| NativePreparationError::ProfileHomeUnavailable)
}

pub fn profile_settings_worker_spec(
    profile: &AgentProfileSnapshot,
    workspace: &NativeWorkspaceBinding,
    host_context: &HostWorkerContext,
    protected_roots: &[PathBuf],
) -> Result<NativeProfileSettingsWorkerSpec, NativePreparationError> {
    if profile.validate_snapshot().is_err()
        || profile.credential_reference.is_some()
        || workspace.guest_root != NATIVE_GUEST_WORKSPACE_ROOT
        || canonical_existing_host_path(&workspace.canonical_host_root).as_ref()
            != Some(&workspace.canonical_host_root)
        || !workspace.canonical_host_root.is_dir()
    {
        return Err(NativePreparationError::InvalidAuthoritySnapshot);
    }
    // A missing writable home is the one precondition the user repairs from the
    // Settings UI ("prepare dedicated home"), so it is reported on its own.
    let writable_home_mount = profile.persistent_mounts.iter().any(|mount| {
        !mount.read_only && Path::new(&mount.guest_path).starts_with(&profile.container_home)
    });
    let writable_shared_home = profile
        .managed_home_overlay
        .as_ref()
        .is_some_and(|overlay| {
            !overlay.parent.read_only && overlay.parent.guest_path == profile.container_home
        });
    if !writable_home_mount && !writable_shared_home {
        return Err(NativePreparationError::ProfileHomeUnavailable);
    }
    let mut protected = protected_roots.to_vec();
    protected.extend([
        host_context.microsandbox_sdk_home.clone(),
        host_context.backend_config_path.clone(),
        workspace.canonical_host_root.clone(),
    ]);
    validate_persistent_mounts(profile, &protected)?;
    Ok(NativeProfileSettingsWorkerSpec {
        native_profile_id: profile.profile_id.clone(),
        use_static: profile.resources.use_static,
        workspace_binding: workspace.clone(),
        runner_settings_json: native_runner_settings(profile, workspace)?,
        host_context: host_context.clone(),
    })
}

fn native_runner_settings(
    profile: &AgentProfileSnapshot,
    workspace: &NativeWorkspaceBinding,
) -> Result<serde_json::Value, NativePreparationError> {
    crate::coding_agent::profile::validate_runtime_environment(&profile.runtime_environment)
        .map_err(|_| NativePreparationError::InvalidAuthoritySnapshot)?;
    let host_root = native_host_path(&workspace.canonical_host_root)?;
    let mut mounts = vec![json!({
        "host_path": host_root,
        "guest_path": workspace.guest_root,
        "writable": true,
        "executable": true,
    })];
    let mut allowed_host_mounts = vec![json!({
        "host_root": host_root,
        "allow_write": true,
        "allow_exec": true,
    })];
    if let Some(overlay) = &profile.managed_home_overlay {
        let host_path = native_host_path(&overlay.parent.host_path)?;
        mounts.push(json!({
            "host_path": host_path,
            "guest_path": overlay.parent.guest_path,
            "writable": true,
            "executable": false,
        }));
        allowed_host_mounts.push(json!({
            "host_root": host_path,
            "allow_write": true,
            "allow_exec": false,
        }));
        for child in &overlay.children {
            let host_path = native_host_path(child.host_path())?;
            mounts.push(json!({
                "host_path": host_path,
                "guest_path": child.guest_path(&profile.container_home),
                "writable": true,
                "executable": false,
            }));
            allowed_host_mounts.push(json!({
                "host_root": host_path,
                "allow_write": true,
                "allow_exec": false,
            }));
        }
    }
    for mount in &profile.persistent_mounts {
        let host_path = native_host_path(&mount.host_path)?;
        mounts.push(json!({
            "host_path": host_path,
            "guest_path": mount.guest_path,
            "writable": !mount.read_only,
            "executable": mount.executable,
        }));
        allowed_host_mounts.push(json!({
            "host_root": host_path,
            "allow_write": !mount.read_only,
            "allow_exec": mount.executable,
        }));
    }
    let image_ref = profile.image_ref.clone();
    let mut environment = serde_json::Map::new();
    environment.insert("HOME".into(), json!(profile.container_home));
    for (key, value) in &profile.runtime_environment {
        environment.insert(key.clone(), json!(value));
    }
    Ok(json!({
        "vm": {
            "image": image_ref,
            "cpus": profile.resources.cpu_millis / 1000,
            "memory_mib": profile.resources.memory_mib,
            "root_disk_mib": profile.resources.root_disk_mib,
            "mounts": mounts,
            "env": environment,
        },
        "allowed_images": [profile.image_ref],
        "allowed_host_mounts": allowed_host_mounts,
        "network": profile.network_access.runner_config(),
    }))
}

fn native_worker_binding(
    matched: &NativeWorkerMatch,
    profile: &AgentProfileSnapshot,
) -> Result<NativeWorkerBinding, NativePreparationError> {
    let settings_digest = matched.settings_digest();
    if settings_digest.len() != 64
        || !settings_digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        || matched.worker_id().value <= 0
        || matched.runner_id().value <= 0
    {
        return Err(NativePreparationError::WorkerUnavailable);
    }
    Ok(NativeWorkerBinding {
        worker_id: matched.worker_id().value,
        runner_id: matched.runner_id().value,
        worker_name: format!("{NATIVE_WORKER_NAME_PREFIX}{}", &settings_digest[..32]),
        settings_digest: settings_digest.to_owned(),
        image_digest: profile.image_digest.clone(),
    })
}

fn repository_id(identity: &RepositoryIdentity) -> RepositoryId {
    RepositoryId {
        host: identity.host.clone(),
        owner: identity.organization.clone(),
        name: identity.repository.clone(),
    }
}

fn native_host_path(path: &Path) -> Result<&str, NativePreparationError> {
    if !valid_native_host_path(path) {
        return Err(NativePreparationError::RuntimeContextUnavailable);
    }
    path.to_str()
        .ok_or(NativePreparationError::RuntimeContextUnavailable)
}

fn valid_native_host_path(path: &Path) -> bool {
    path.is_absolute()
        && !path.as_os_str().is_empty()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
        && path.to_str().is_some_and(|value| !value.is_empty())
}

fn valid_sdk_config_location(path: &Path) -> bool {
    if !valid_native_host_path(path) {
        return false;
    }
    match fs::symlink_metadata(path) {
        Ok(_) => canonical_existing_host_path(path).is_some_and(|path| path.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // SDK 0.7.6 accepts an absent config as empty. This only validates
            // its location; Doctor must still check the selected source stamp.
            path.parent()
                .and_then(canonical_existing_host_path)
                .is_some_and(|parent| parent.is_dir())
        }
        Err(_) => false,
    }
}

/// Return an existing path only if each component is a real, non-symlink path
/// and the supplied spelling is already canonical. Mount roots must not be
/// resolved through host-managed credential/configuration symlinks.
fn canonical_existing_host_path(path: &Path) -> Option<PathBuf> {
    if !valid_native_host_path(path) {
        return None;
    }
    let mut current = PathBuf::new();
    let mut final_metadata = None;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        match component {
            std::path::Component::RootDir => current.push(component.as_os_str()),
            std::path::Component::Normal(part) => {
                current.push(part);
                let metadata = fs::symlink_metadata(&current).ok()?;
                if metadata.file_type().is_symlink()
                    || (components.peek().is_some() && !metadata.is_dir())
                {
                    return None;
                }
                final_metadata = Some(metadata);
            }
            _ => return None,
        }
    }
    let canonical = fs::canonicalize(path).ok()?;
    let metadata = final_metadata?;
    if !metadata.is_dir() && !metadata.is_file() {
        return None;
    }
    (canonical.as_os_str() == path.as_os_str()).then_some(canonical)
}

fn map_executor_error(_: NativeWorkspaceExecutorError) -> NativePreparationError {
    NativePreparationError::WorkspaceUnavailable
}

fn map_launch_error(error: super::launch::NativeLaunchError) -> NativePreparationError {
    match error {
        super::launch::NativeLaunchError::InvalidPrompt => NativePreparationError::InvalidPrompt,
        super::launch::NativeLaunchError::InvalidWorkspacePath => {
            NativePreparationError::WorkspaceUnavailable
        }
        super::launch::NativeLaunchError::UnsupportedProfile
        | super::launch::NativeLaunchError::PinMismatch
        | super::launch::NativeLaunchError::ProfileCommandMismatch => {
            NativePreparationError::UnsupportedCliProfile
        }
        super::launch::NativeLaunchError::InvalidPlan => NativePreparationError::InvalidContext,
    }
}

fn new_uuid() -> Result<String, NativePreparationError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| NativePreparationError::IdentityUnavailable)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
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
        bytes[15],
    ))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use super::*;

    #[test]
    fn native_runner_environment_keeps_container_home_and_adds_runtime_paths() {
        let workspace_root = tempfile::tempdir().unwrap();
        let profile = AgentProfileSnapshot {
            profile_id: "profile".into(),
            version: 1,
            display_name: "Profile".into(),
            image_ref: "registry.example/agent@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            image_digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            command: vec!["codex".into(), "exec".into()],
            container_home: "/home/codespace/.agent-home".into(),
            managed_home_overlay: None,
            runtime_environment: BTreeMap::from([
                ("CARGO_HOME".into(), "/home/codespace/.cargo".into()),
                ("RUSTUP_HOME".into(), "/home/codespace/.rustup".into()),
                (
                    "CODEX_HOME".into(),
                    "/home/codespace/.agent-home/.codex".into(),
                ),
            ]),
            resources: crate::coding_agent::SandboxResources {
                cpu_millis: 1000,
                memory_mib: 1024,
                root_disk_mib: 4096,
                max_parallel_runs: 1,
                use_static: false,
            },
            network_access: Default::default(),
            validated_host_limits: HostResourceLimits {
                cpu_millis: 1000,
                memory_mib: 1024,
                root_disk_mib: 4096,
                max_parallel_runs: 1,
            },
            canonical_mount_allowed_roots: vec![workspace_root.path().to_path_buf()],
            persistent_mounts: Vec::new(),
            allowed_repositories: Vec::new(),
            credential_reference: None,
        };
        let workspace = NativeWorkspaceBinding {
            canonical_host_root: workspace_root.path().to_path_buf(),
            guest_root: "/workspace".into(),
        };

        let settings = native_runner_settings(&profile, &workspace).unwrap();
        let environment = settings["vm"]["env"].as_object().unwrap();
        assert_eq!(environment["HOME"], "/home/codespace/.agent-home");
        assert_eq!(environment["CARGO_HOME"], "/home/codespace/.cargo");
        assert_eq!(environment["RUSTUP_HOME"], "/home/codespace/.rustup");
        assert_eq!(
            environment["CODEX_HOME"],
            "/home/codespace/.agent-home/.codex"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_runner_settings_uses_the_effective_host_agent_mounts() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let workspace_root = root.path().join("workspace");
        let codex = root.path().join("codex");
        fs::create_dir(&workspace_root).unwrap();
        fs::create_dir(&codex).unwrap();
        let profile_homes = root.path().join("profile-homes");
        let shared_homes = root.path().join("profile-shared-homes");
        let shared_home =
            shared_homes.join(crate::coding_agent::assessment::sha256_hex(b"profile"));
        fs::create_dir(&profile_homes).unwrap();
        fs::create_dir(&shared_homes).unwrap();
        fs::create_dir(&shared_home).unwrap();
        for path in [&workspace_root, &codex] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        for path in [&profile_homes, &shared_homes, &shared_home] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let image = "registry.example/agent@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let configured = crate::coding_agent::settings::AgentProfileSettings {
            profile_id: "profile".into(),
            version: 1,
            display_name: "Profile".into(),
            image_ref: image.into(),
            allowed_image_refs: vec![image.into()],
            command: vec!["codex".into(), "exec".into()],
            container_home: "/home/codespace".into(),
            host_configuration: Some(
                crate::coding_agent::settings::HostAgentConfigurationSettings {
                    codex_directory: Some(codex.clone()),
                    claude_directory: None,
                    claude_settings_file: None,
                },
            ),
            runtime_environment: BTreeMap::new(),
            resources: crate::coding_agent::settings::SandboxResourcesSettings {
                cpu_millis: 1000,
                memory_mib: 1024,
                root_disk_mib: 4096,
                max_parallel_runs: 1,
                use_static: false,
            },
            network_access: Default::default(),
            mount_allowed_roots: vec![root.path().to_path_buf()],
            persistent_mounts: Vec::new(),
            allowed_repositories: Vec::new(),
            credential_reference: None,
        };
        let profile = configured
            .to_agent_profile_with_managed_home_root(&profile_homes)
            .unwrap()
            .validate_and_snapshot(&HostResourceLimits {
                cpu_millis: 1000,
                memory_mib: 1024,
                root_disk_mib: 4096,
                max_parallel_runs: 1,
            })
            .unwrap();
        let workspace = NativeWorkspaceBinding {
            canonical_host_root: workspace_root,
            guest_root: "/workspace".into(),
        };

        let settings = native_runner_settings(&profile, &workspace).unwrap();
        let mounts = settings["vm"]["mounts"].as_array().unwrap();
        assert!(mounts.iter().any(|mount| {
            mount["host_path"] == serde_json::Value::String(codex.to_string_lossy().into_owned())
                && mount["guest_path"] == "/home/codespace/.codex"
        }));
        assert_eq!(
            settings["vm"]["env"]["CODEX_HOME"],
            "/home/codespace/.codex"
        );
        let mounts = settings["vm"]["mounts"].as_array().unwrap();
        let shared_index = mounts
            .iter()
            .position(|mount| mount["guest_path"] == "/home/codespace")
            .unwrap();
        let codex_index = mounts
            .iter()
            .position(|mount| mount["guest_path"] == "/home/codespace/.codex")
            .unwrap();
        assert!(shared_index < codex_index);
        assert_eq!(mounts[shared_index]["writable"], true);
        assert_eq!(mounts[shared_index]["executable"], false);
        assert_eq!(mounts[codex_index]["writable"], true);
        assert_eq!(mounts[codex_index]["executable"], false);
    }
}
