//! Host-owned bootstrap for the independent native interactive executor.
//!
//! The caller supplies the application's already-resolved resource directory and
//! `DataPaths`; neither an executable path nor a storage root is accepted from a
//! renderer. This module verifies package provenance and host policy, then creates
//! a repository-isolated helper configuration and private native stores. It does
//! not start a VM, prove runtime compatibility, prepare a job, or publish results.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use coding_agent_executor::{
    CheckRepositoryChecks, CheckRepositoryRequest, CredentialSourceConfig, EXECUTOR_VERSION,
    ExecutorErrorCode, PROTOCOL_VERSION, PrepareRequest, RepositoryCheckResult,
    RepositoryCheckStage, RepositoryConfig, RepositoryId, ResolveBaseRequest, TrustedConfig,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    coding_agent::{
        NativeExecutorClient, NativeExecutorError,
        executor_client::NativeDiagnosticTransportReason,
        settings::{CodingAgentSettings, CodingAgentSettingsStore},
    },
    data::DataPaths,
    sidecar::ManagedJobworkerpSpawnSnapshot,
};

use super::{
    NativeRuntimeDoctorStore, NativeRuntimeDoctorStoreError, NativeTerminalOutputStore,
    NativeTerminalStore, output::NativeTerminalOutputError, store::NativeTerminalStoreError,
};

/// Fixed app-data policy location. It is not configurable by a renderer request.
pub const TRUSTED_HOST_POLICY_RELATIVE_PATH: &str = "coding-agent/native/trusted-host.json";

const PACKAGE_DIRECTORY: &str = "coding-agent";
const PACKAGE_MANIFEST: &str = "coding-agent-artifact-manifest.json";
const EXECUTOR_RELATIVE_PATH: &str = "bin/coding-agent-executor";
const MAX_POLICY_BYTES: u64 = 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_EXECUTOR_BYTES: u64 = 256 * 1024 * 1024;
const MAX_RUNTIME_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_CONFIG_ARTIFACT_BYTES: u64 = 1024 * 1024;
const MAX_REPOSITORIES: usize = 256;
const MAX_CREDENTIAL_SOURCES: usize = 128;
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const DEFAULT_NATIVE_MAX_IMPORT_BYTES: u64 = 16 * 1024 * 1024;
#[cfg(test)]
const CONFIGURATION_ARTIFACTS: [&str; 4] = [
    "workers/lookback-coding-agent-workers.yaml",
    "workers/workflows/coding-agent/prepare.yaml",
    "workers/workflows/coding-agent/one-shot.yaml",
    "workers/workflows/coding-agent/publish.yaml",
];
const EXECUTOR_FEATURES: [&str; 13] = [
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
const CORE_EXECUTOR_FEATURES: [&str; 1] = ["prepare"];

/// Stable, path- and secret-free reasons that allow the app to keep running while
/// the optional native interactive backend remains unavailable.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativeBootstrapUnavailable {
    #[error("Native interactive workspaces are not supported on this platform.")]
    UnsupportedPlatform,
    #[error(
        "Install the protected native workspace policy in application data to enable interactive workspaces."
    )]
    TrustedPolicyMissing,
    #[error("The protected native workspace policy has unsafe file permissions.")]
    UnsafeTrustedPolicy,
    #[error(
        "The protected native workspace policy is invalid; repair it before enabling interactive workspaces."
    )]
    InvalidTrustedPolicy,
    #[error("The bundled native executor package is unavailable or failed provenance checks.")]
    PackageUnavailable,
    #[error("The bundled native executor does not match the installed host package policy.")]
    PackageIdentityMismatch,
    #[error("The requested repository is not approved for native interactive workspaces.")]
    RepositoryNotApproved,
    #[error("An approved repository credential is unavailable or unsafe.")]
    CredentialUnavailable,
    #[error("Private native workspace storage could not be safely initialized.")]
    StorageUnavailable,
    #[error("The bundled native executor could not verify its protocol capabilities.")]
    ExecutorUnavailable,
}

impl NativeBootstrapUnavailable {
    /// Display-safe actionable text. It never includes a path, URL, token, or
    /// subprocess diagnostic.
    pub fn user_message(self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => {
                "Native interactive workspaces are supported only by this app's Linux KVM package."
            }
            Self::TrustedPolicyMissing => {
                "Install the protected native workspace policy in application data to enable interactive workspaces."
            }
            Self::UnsafeTrustedPolicy => {
                "The native workspace policy must be owned by this user in a private application-data directory."
            }
            Self::InvalidTrustedPolicy => {
                "Repair the protected native workspace policy before using an interactive workspace."
            }
            Self::PackageUnavailable => {
                "The app's bundled native executor package is missing or failed its integrity checks. Reinstall the app package."
            }
            Self::PackageIdentityMismatch => {
                "The bundled native executor does not match the installed package's approved source and version pins. Reinstall or update the trusted host policy."
            }
            Self::RepositoryNotApproved => {
                "Approve this repository in native workspace settings before opening an interactive workspace."
            }
            Self::CredentialUnavailable => {
                "An approved repository credential is missing or unsafe. Repair the host credential mapping."
            }
            Self::StorageUnavailable => {
                "Private native workspace storage could not be initialized. Check application-data permissions and available disk space."
            }
            Self::ExecutorUnavailable => {
                "The bundled executor could not verify its protocol capabilities. Reinstall the app package."
            }
        }
    }
}

/// The helper has passed its bounded capability probe, but the separate Linux
/// VM/SDK runtime compatibility gate has not been observed by this bootstrap.
/// This is intentionally not a boolean `ready` signal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExecutorRuntimeStatus {
    NotObserved,
}

/// A package root selected by native app-resource lookup, with the parsed
/// manifest and artifact digests pinned for a later recheck. This type is not a
/// deserializable request DTO and deliberately has no path-taking setters.
#[derive(Clone)]
pub struct NativePackageSelection {
    package_root: PathBuf,
    manifest: ArtifactManifest,
    manifest_sha256: String,
    executor_sha256: String,
    package_version: String,
    source_policy: PackageSourcePolicy,
}

/// How strictly manifest source provenance is checked. Release builds accept
/// only clean, pinned sources. Development builds additionally admit a package
/// staged from a working tree (uncommitted changes, floating refs) so that
/// `pnpm tauri dev` can exercise the native backend without a release build.
/// The policy is an explicit value rather than a `cfg!` read inside validation
/// so tests can exercise both modes regardless of the build profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PackageSourcePolicy {
    pub allow_development_sources: bool,
}

impl PackageSourcePolicy {
    pub const fn strict() -> Self {
        Self {
            allow_development_sources: false,
        }
    }

    pub const fn development() -> Self {
        Self {
            allow_development_sources: true,
        }
    }

    /// The policy for this binary: only debug builds admit development sources.
    pub const fn for_build() -> Self {
        Self {
            allow_development_sources: cfg!(debug_assertions),
        }
    }
}

/// Digest and inode identity observed from the managed child's actual Linux
/// executable file descriptor. This is package consistency evidence, not a
/// signature, SDK, VM, or agent-readiness attestation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeRuntimeImageObservation {
    child_pid: u32,
    child_generation_id: u64,
    device: u64,
    inode: u64,
    sha256: String,
    package_manifest_sha256: String,
}

impl NativeRuntimeImageObservation {
    pub fn child_pid(&self) -> u32 {
        self.child_pid
    }

    pub fn child_generation_id(&self) -> u64 {
        self.child_generation_id
    }

    pub fn device(&self) -> u64 {
        self.device
    }

    pub fn inode(&self) -> u64 {
        self.inode
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn package_manifest_sha256(&self) -> &str {
        &self.package_manifest_sha256
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativeRuntimeImageUnavailable {
    #[error("managed runtime image verification is supported only on Linux")]
    UnsupportedPlatform,
    #[error("the managed runtime child identity is invalid")]
    InvalidChildIdentity,
    #[error("the managed child's actual runtime image is unavailable or unsafe")]
    UnsafeExecutable,
    #[error("the managed child's actual runtime image exceeds the bounded size")]
    ExecutableTooLarge,
    #[error("the managed child's actual runtime image changed during verification")]
    ExecutableChanged,
    #[error("the running runtime image does not match the selected package manifest")]
    ImageDigestMismatch,
    #[error("the selected native package changed during runtime image verification")]
    PackageChanged,
}

impl std::fmt::Debug for NativePackageSelection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativePackageSelection")
            .field("package_version", &self.package_version)
            .field("package_manifest_sha256", &self.manifest_sha256)
            .field("executor_sha256", &self.executor_sha256)
            .finish_non_exhaustive()
    }
}

impl NativePackageSelection {
    /// Select and inspect only the fixed `coding-agent` directory beneath the
    /// app's native resource directory. Pass the `resource_dir()` returned by
    /// Tauri; never pass renderer or settings data here.
    ///
    /// The manifest's SHA-256 values establish consistency between this fixed
    /// installed resource tree and its descriptor; they are not a signature or
    /// an independent attestation. The caller must obtain `resource_dir` from
    /// the native app handle, and the fixed host policy separately pins the
    /// package's semantic source/build identity.
    pub fn from_app_resources(resource_dir: &Path) -> Result<Self, NativeBootstrapUnavailable> {
        Self::from_app_resources_with_policy(resource_dir, PackageSourcePolicy::for_build())
    }

    /// `from_app_resources` with an explicit source policy.
    pub fn from_app_resources_with_policy(
        resource_dir: &Path,
        source_policy: PackageSourcePolicy,
    ) -> Result<Self, NativeBootstrapUnavailable> {
        if !cfg!(target_os = "linux") {
            return Err(NativeBootstrapUnavailable::UnsupportedPlatform);
        }
        let package_root = resource_dir.join(PACKAGE_DIRECTORY);
        validate_package_root(&package_root)?;
        let manifest_path = package_root.join(PACKAGE_MANIFEST);
        let manifest_bytes =
            read_package_file(&package_root, &manifest_path, MAX_MANIFEST_BYTES, false)
                .map_err(|_| NativeBootstrapUnavailable::PackageUnavailable)?;
        let manifest: ArtifactManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|_| NativeBootstrapUnavailable::PackageUnavailable)?;
        validate_manifest(&manifest, source_policy)?;
        verify_artifact_files(&package_root, &manifest)?;
        Ok(Self {
            package_root,
            executor_sha256: manifest.artifacts.executor.sha256.clone(),
            package_version: manifest.executor.version.clone(),
            manifest_sha256: sha256(&manifest_bytes),
            manifest,
            source_policy,
        })
    }

    /// Non-secret package version for diagnostics and test seams.
    pub fn package_version(&self) -> &str {
        &self.package_version
    }

    pub(crate) fn package_fingerprint(&self) -> &str {
        &self.manifest_sha256
    }

    pub fn source_policy(&self) -> PackageSourcePolicy {
        self.source_policy
    }

    /// Whether the manifest records working-tree sources (uncommitted changes
    /// or a floating ref). Such a package can only have been admitted by a
    /// development source policy; the renderer labels it accordingly.
    pub(crate) fn development_sources(&self) -> bool {
        [
            &self.manifest.sources.agent_app,
            &self.manifest.sources.jobworkerp_runtime,
        ]
        .into_iter()
        .any(|source| {
            source.dirty
                || floating_source_ref(&source.reference)
                || source.submodules.iter().any(|submodule| submodule.dirty)
        })
    }

    /// Digest of the selected installed manifest, for native cache identity.
    pub(crate) fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }

    fn verify_unchanged(&self) -> Result<(), NativeBootstrapUnavailable> {
        self.verify_manifest_unchanged()?;
        verify_artifact_files(&self.package_root, &self.manifest)
    }

    /// Detects a replaced or removed package without hashing its artifacts.
    fn verify_manifest_unchanged(&self) -> Result<(), NativeBootstrapUnavailable> {
        validate_package_root(&self.package_root)?;
        let bytes = read_package_file(
            &self.package_root,
            &self.package_root.join(PACKAGE_MANIFEST),
            MAX_MANIFEST_BYTES,
            false,
        )
        .map_err(|_| NativeBootstrapUnavailable::PackageUnavailable)?;
        // The selected manifest was parsed from bytes with this digest.
        if sha256(&bytes) != self.manifest_sha256 {
            return Err(NativeBootstrapUnavailable::PackageUnavailable);
        }
        Ok(())
    }

    /// Verify the package runtime digest against the executable held open by
    /// the managed Linux child. `actual_running_executable` and the configured
    /// binary pathname are deliberately not used as content evidence.
    pub fn verify_managed_runtime(
        &self,
        snapshot: &ManagedJobworkerpSpawnSnapshot,
    ) -> Result<NativeRuntimeImageObservation, NativeRuntimeImageUnavailable> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = snapshot;
            return Err(NativeRuntimeImageUnavailable::UnsupportedPlatform);
        }

        #[cfg(target_os = "linux")]
        {
            if snapshot.child_pid == 0 || snapshot.generation_id == 0 {
                return Err(NativeRuntimeImageUnavailable::InvalidChildIdentity);
            }

            self.verify_unchanged()
                .map_err(|_| NativeRuntimeImageUnavailable::PackageChanged)?;
            let executable = hash_managed_process_executable(snapshot.child_pid)?;
            if executable.sha256 != self.manifest.artifacts.runtime.sha256 {
                return Err(NativeRuntimeImageUnavailable::ImageDigestMismatch);
            }
            self.verify_unchanged()
                .map_err(|_| NativeRuntimeImageUnavailable::PackageChanged)?;

            Ok(NativeRuntimeImageObservation {
                child_pid: snapshot.child_pid,
                child_generation_id: snapshot.generation_id,
                device: executable.device,
                inode: executable.inode,
                sha256: executable.sha256,
                package_manifest_sha256: self.manifest_sha256.clone(),
            })
        }
    }
}

/// The only operations exposed here create or prepare an approved workspace.
/// Publication APIs on the general coding-agent client are intentionally not
/// re-exported by this interactive-session boundary.
#[derive(Clone)]
pub struct NativeWorkspaceExecutor {
    client: NativeExecutorClient,
    supports_resolve_base: bool,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum NativeWorkspaceExecutorError {
    #[error("The compatible executor does not advertise optional base resolution.")]
    ResolveBaseUnavailable,
    #[error("The native executor could not complete the workspace operation.")]
    Executor(#[from] NativeExecutorError),
}

impl std::fmt::Debug for NativeWorkspaceExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeWorkspaceExecutor")
            .field("supports_resolve_base", &self.supports_resolve_base)
            .finish_non_exhaustive()
    }
}

impl NativeWorkspaceExecutor {
    pub(crate) fn publication_client(&self) -> &NativeExecutorClient {
        &self.client
    }

    pub(crate) fn with_preparation_workflow(
        mut self,
        handle: crate::jobworkerp::JobworkerpHandle,
    ) -> Self {
        self.client = self.client.with_preparation_workflow(handle);
        self
    }

    /// Resolve an allowlisted repository base ref when the helper advertises
    /// that optional capability. Older compatible helpers remain usable for
    /// other supported operations.
    pub async fn resolve_base(
        &self,
        request: ResolveBaseRequest,
    ) -> Result<coding_agent_executor::BaseResolutionReceipt, NativeWorkspaceExecutorError> {
        if !self.supports_resolve_base {
            return Err(NativeWorkspaceExecutorError::ResolveBaseUnavailable);
        }
        self.client
            .resolve_base(request)
            .await
            .map_err(NativeWorkspaceExecutorError::from)
    }

    /// Prepare one fresh native workspace using only the selected repository's
    /// trusted configuration.
    pub async fn prepare(
        &self,
        request: PrepareRequest,
    ) -> Result<coding_agent_executor::PreparedWorkspace, NativeWorkspaceExecutorError> {
        self.client
            .prepare(request)
            .await
            .map_err(NativeWorkspaceExecutorError::from)
    }

    pub async fn inspect_source(
        &self,
        request: PrepareRequest,
    ) -> Result<coding_agent_executor::SourceInspection, NativeWorkspaceExecutorError> {
        self.client
            .inspect_source(request)
            .await
            .map_err(NativeWorkspaceExecutorError::from)
    }

    pub fn supports_resolve_base(&self) -> bool {
        self.supports_resolve_base
    }
}

/// Repository-scoped bootstrap result. It owns no jobworkerp handle, VM, CLI
/// process, lifecycle record, or runtime-readiness claim.
pub struct ConfiguredNativeRepository {
    repository: RepositoryId,
    config_digest: String,
    workspace_root: PathBuf,
    preparation_policy: NativeRepositoryPreparationPolicy,
    executor: NativeWorkspaceExecutor,
    session_store: NativeTerminalStore,
    output_store: NativeTerminalOutputStore,
    doctor_store: NativeRuntimeDoctorStore,
}

/// Host-owned branch policy and excluded native roots handed directly to the
/// preparation owner. None of these values are supplied by a renderer.
pub(crate) struct NativeRepositoryPreparationPolicy {
    pub(crate) allowed_base_branches: Vec<String>,
    pub(crate) protected_host_roots: Vec<PathBuf>,
}

impl std::fmt::Debug for ConfiguredNativeRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfiguredNativeRepository")
            .field("repository", &self.repository)
            .field("trusted_config_digest", &self.config_digest)
            .field("runtime_status", &NativeExecutorRuntimeStatus::NotObserved)
            .field("executor", &self.executor)
            .finish_non_exhaustive()
    }
}

impl ConfiguredNativeRepository {
    pub fn repository(&self) -> &RepositoryId {
        &self.repository
    }

    pub fn trusted_config_digest(&self) -> &str {
        &self.config_digest
    }

    /// Host-only path for preparing the native workspace mount; never return
    /// this as a renderer-selected path.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn executor(&self) -> &NativeWorkspaceExecutor {
        &self.executor
    }

    pub fn session_store(&self) -> &NativeTerminalStore {
        &self.session_store
    }

    pub fn output_store(&self) -> &NativeTerminalOutputStore {
        &self.output_store
    }

    pub fn runtime_status(&self) -> NativeExecutorRuntimeStatus {
        NativeExecutorRuntimeStatus::NotObserved
    }

    /// Transfer the already-open stores and selected repository resources to
    /// the native preparation owner without reopening or duplicating them.
    pub(crate) fn into_parts(
        self,
    ) -> (
        RepositoryId,
        PathBuf,
        NativeRepositoryPreparationPolicy,
        NativeWorkspaceExecutor,
        Arc<NativeTerminalStore>,
        Arc<NativeTerminalOutputStore>,
        Arc<NativeRuntimeDoctorStore>,
    ) {
        (
            self.repository,
            self.workspace_root,
            self.preparation_policy,
            self.executor,
            Arc::new(self.session_store),
            Arc::new(self.output_store),
            Arc::new(self.doctor_store),
        )
    }
}

/// A read-only projection of the protected host policy. Credential contents,
/// Native policy data is never projected into renderer requests.
#[derive(Clone, Debug)]
pub(crate) struct NativeRepositoryPolicyEntry {
    pub(crate) repository: RepositoryId,
    clone_url: String,
    pub(crate) ssh_directory: Option<PathBuf>,
    pub(crate) read_credential_source: Option<String>,
    pub(crate) publish_credential_source: Option<String>,
    pub(crate) forge: Option<coding_agent_executor::ForgeConfig>,
    pub(crate) allowed_base_branches: Vec<String>,
    pub(crate) max_import_bytes: u64,
}

impl NativeRepositoryPolicyEntry {
    pub(crate) fn allows_source(&self, repository: &RepositoryId, clone_url: &str) -> bool {
        if self.repository != *repository {
            return false;
        }
        clone_urls_match(repository, &self.clone_url, clone_url)
    }

    pub(crate) fn matches_host(
        &self,
        host: &crate::coding_agent::RepositoryHostConfig,
        repository: &RepositoryId,
        clone_url: &str,
    ) -> bool {
        self.allows_source(repository, clone_url)
            // A null host reference inherits the fixed/effective policy source;
            // an explicit renderer reference must still match exactly.
            && host
                .read_credential_ref
                .as_ref()
                .map(|reference| {
                    self.read_credential_source.as_deref() == Some(reference.as_str())
                })
                .unwrap_or(true)
            && host
                .publication_credential_ref
                .as_ref()
                .map(|reference| {
                    self.publish_credential_source.as_deref() == Some(reference.as_str())
                })
                .unwrap_or(true)
            && (self.ssh_directory.as_deref() == host.ssh_directory()
                // Legacy fixed policy entries may carry the SSH directory
                // while old saved host JSON has no explicit field.
                || (host.ssh_directory().is_none()
                    && self.ssh_directory.is_some()
                    && self.clone_url.starts_with("ssh:")))
            && self.forge.as_ref().is_none_or(|forge| {
                let same_kind = matches!(
                    (&forge.kind, &host.forge),
                    (
                        coding_agent_executor::ForgeKind::GitHub,
                        crate::coding_agent::ForgeKind::GitHub
                    ) | (
                        coding_agent_executor::ForgeKind::Gitea,
                        crate::coding_agent::ForgeKind::Gitea
                    )
                );
                same_kind && forge.api_base_url == host.api_base_url
            })
    }
}

pub(crate) fn clone_urls_match(
    repository: &RepositoryId,
    configured_url: &str,
    selected_url: &str,
) -> bool {
    if configured_url == selected_url {
        return true;
    }
    let (Ok(mut configured), Ok(mut selected)) = (
        url::Url::parse(configured_url),
        url::Url::parse(selected_url),
    ) else {
        return false;
    };
    let expected_path = format!("/{}/{}", repository.owner, repository.name);
    let suffixed_path = format!("{expected_path}.git");
    if [configured.path(), selected.path()]
        .iter()
        .any(|path| *path != expected_path && *path != suffixed_path)
    {
        return false;
    }
    configured.set_path(&expected_path);
    selected.set_path(&expected_path);
    configured == selected
}

#[derive(Clone, Debug)]
pub(crate) struct NativeRepositoryPolicySnapshot {
    pub(crate) policy_sha256: String,
    pub(crate) repositories: Vec<NativeRepositoryPolicyEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativePolicySource {
    ProtectedFile,
    VirtualPackageBaseline,
}

/// How much of the selected package a policy read re-verifies.
#[derive(Clone, Copy)]
enum PackageRecheck {
    /// Re-hash every artifact; required before anything relies on the package.
    Artifacts,
    /// Re-read only the manifest. Artifacts were hashed at selection and are
    /// hashed again by every path that executes or approves them.
    ManifestOnly,
}

/// Read current protected repository eligibility without starting the helper,
/// creating workspace state, reading credentials, or opening a jobworkerp job.
pub(crate) fn read_repository_policy(
    data_paths: &DataPaths,
    package: &NativePackageSelection,
) -> Result<NativeRepositoryPolicySnapshot, NativeBootstrapUnavailable> {
    repository_policy_snapshot(data_paths, package, PackageRecheck::Artifacts)
}

/// [`read_repository_policy`] without re-hashing package artifacts. Only for
/// display and for prechecks whose execution step re-reads the policy with
/// [`read_repository_policy`] semantics (as `execute_repository_check` does);
/// never let its result alone authorize execution, approval, or publication.
pub(crate) fn read_repository_policy_for_diagnostic(
    data_paths: &DataPaths,
    package: &NativePackageSelection,
) -> Result<NativeRepositoryPolicySnapshot, NativeBootstrapUnavailable> {
    repository_policy_snapshot(data_paths, package, PackageRecheck::ManifestOnly)
}

fn repository_policy_snapshot(
    data_paths: &DataPaths,
    package: &NativePackageSelection,
    recheck: PackageRecheck,
) -> Result<NativeRepositoryPolicySnapshot, NativeBootstrapUnavailable> {
    let (_host_root, policy_bytes, policy, settings_revision, source) =
        load_validated_repository_policy(data_paths, package, recheck)?;
    Ok(NativeRepositoryPolicySnapshot {
        policy_sha256: effective_policy_digest(&policy_bytes, settings_revision, &policy, source),
        repositories: policy
            .repositories
            .into_iter()
            .map(|approved| NativeRepositoryPolicyEntry {
                repository: approved.id,
                clone_url: approved.clone_url,
                ssh_directory: approved.ssh_directory,
                read_credential_source: approved.read_credential_source,
                publish_credential_source: approved.publish_credential_source,
                forge: approved.forge,
                allowed_base_branches: approved.allowed_base_branches,
                max_import_bytes: policy.max_import_bytes,
            })
            .collect(),
    })
}

fn load_validated_repository_policy(
    data_paths: &DataPaths,
    package: &NativePackageSelection,
    recheck: PackageRecheck,
) -> Result<
    (PathBuf, Vec<u8>, TrustedHostPolicy, u64, NativePolicySource),
    NativeBootstrapUnavailable,
> {
    if !cfg!(target_os = "linux") {
        return Err(NativeBootstrapUnavailable::UnsupportedPlatform);
    }
    let host_root = native_host_root(data_paths)?;
    let policy_path = data_paths.root.join(TRUSTED_HOST_POLICY_RELATIVE_PATH);
    let (policy_bytes, source) = match read_host_policy_file(&host_root, &policy_path) {
        Ok(bytes) => (bytes, NativePolicySource::ProtectedFile),
        Err(NativeBootstrapUnavailable::TrustedPolicyMissing) => {
            let policy = virtual_package_policy(package)?;
            (
                serde_json::to_vec(&policy)
                    .map_err(|_| NativeBootstrapUnavailable::InvalidTrustedPolicy)?,
                NativePolicySource::VirtualPackageBaseline,
            )
        }
        Err(error) => return Err(error),
    };
    let policy: TrustedHostPolicy = serde_json::from_slice(&policy_bytes)
        .map_err(|_| NativeBootstrapUnavailable::InvalidTrustedPolicy)?;
    validate_policy_structure(&policy, package.source_policy)?;
    if policy.expected_package_identity != package.manifest.package_identity() {
        return Err(NativeBootstrapUnavailable::PackageIdentityMismatch);
    }
    match recheck {
        PackageRecheck::Artifacts => package.verify_unchanged()?,
        PackageRecheck::ManifestOnly => package.verify_manifest_unchanged()?,
    }
    ensure_package_is_separate(package, &host_root)?;
    let settings = CodingAgentSettingsStore::new(data_paths.coding_agent_settings_path())
        .load()
        .map_err(|_| NativeBootstrapUnavailable::InvalidTrustedPolicy)?;
    let policy = merge_gui_repository_policy(policy, &settings, package.source_policy)?;
    if policy.repositories.iter().any(|repository| {
        repository
            .ssh_directory
            .as_deref()
            .is_some_and(|directory| {
                roots_overlap(directory, &host_root)
                    || roots_overlap(directory, &package.package_root)
            })
    }) {
        return Err(NativeBootstrapUnavailable::InvalidTrustedPolicy);
    }
    Ok((host_root, policy_bytes, policy, settings.revision, source))
}

/// Build the initial protected policy from the verified installed package.
/// Repository authority remains empty until the GUI explicitly saves approvals.
fn virtual_package_policy(
    package: &NativePackageSelection,
) -> Result<TrustedHostPolicy, NativeBootstrapUnavailable> {
    Ok(TrustedHostPolicy {
        version: 1,
        max_import_bytes: DEFAULT_NATIVE_MAX_IMPORT_BYTES,
        expected_package_identity: package.manifest.package_identity(),
        repositories: Vec::new(),
        credential_sources: Vec::new(),
    })
}

fn merge_gui_repository_policy(
    mut policy: TrustedHostPolicy,
    settings: &CodingAgentSettings,
    source_policy: PackageSourcePolicy,
) -> Result<TrustedHostPolicy, NativeBootstrapUnavailable> {
    let Some(approvals) = &settings.native_repository_approvals else {
        // `None` is deliberately the legacy repository mode. Keep its
        // repository/token references, but allow an explicitly saved SSH
        // directory to bind the same legacy entry for execution.
        for repository in &mut policy.repositories {
            if !repository.clone_url.starts_with("ssh:") {
                continue;
            }
            let Some(host) = settings
                .repository_hosts
                .iter()
                .find(|host| host.label_host == repository.id.host)
            else {
                continue;
            };
            let host_config = host
                .to_repository_host_config_for_execution()
                .map_err(|_| NativeBootstrapUnavailable::InvalidTrustedPolicy)?;
            if let Some(directory) = host_config.ssh_directory() {
                repository.ssh_directory = Some(directory.to_path_buf());
            }
        }
        validate_policy(&policy, source_policy)?;
        return Ok(policy);
    };
    if approvals.is_empty() {
        policy.repositories.clear();
        validate_policy(&policy, source_policy)?;
        return Ok(policy);
    }

    let mut effective = Vec::with_capacity(approvals.len());
    for approval in approvals {
        let identity = approval
            .repository
            .to_repository_identity()
            .map_err(|_| NativeBootstrapUnavailable::InvalidTrustedPolicy)?;
        let host = settings
            .repository_hosts
            .iter()
            .find(|host| host.label_host == identity.host)
            .ok_or(NativeBootstrapUnavailable::InvalidTrustedPolicy)?;
        let host_config = host
            .to_repository_host_config_for_execution()
            .map_err(|_| NativeBootstrapUnavailable::InvalidTrustedPolicy)?;
        let previous = policy.repositories.iter().find(|entry| {
            entry.id.host == identity.host
                && entry.id.owner == identity.organization
                && entry.id.name == identity.repository
        });
        let clone_url = host_config
            .clone_url_for(&identity)
            .map_err(|_| NativeBootstrapUnavailable::InvalidTrustedPolicy)?;
        let forge = Some(coding_agent_executor::ForgeConfig {
            kind: match host.forge {
                crate::coding_agent::ForgeKind::GitHub => coding_agent_executor::ForgeKind::GitHub,
                crate::coding_agent::ForgeKind::Gitea => coding_agent_executor::ForgeKind::Gitea,
            },
            api_base_url: host.api_base_url.clone(),
            web_base_url: previous
                .and_then(|entry| entry.forge.as_ref())
                .map(|forge| forge.web_base_url.clone())
                .unwrap_or_else(|| host.api_base_url.clone()),
        });
        let ssh_directory = host_config
            .ssh_directory()
            .map(Path::to_path_buf)
            .or_else(|| {
                clone_url
                    .starts_with("ssh:")
                    .then(|| previous.and_then(|entry| entry.ssh_directory.clone()))
                    .flatten()
            });
        effective.push(HostRepository {
            id: RepositoryId {
                host: identity.host,
                owner: identity.organization,
                name: identity.repository,
            },
            clone_url,
            allowed_base_branches: approval.allowed_base_branches.clone(),
            read_credential_source: host
                .read_credential_ref
                .as_ref()
                .map(|value| value.as_str().to_owned())
                .or_else(|| previous.and_then(|entry| entry.read_credential_source.clone())),
            publish_credential_source: host
                .publication_credential_ref
                .as_ref()
                .map(|value| value.as_str().to_owned())
                .or_else(|| previous.and_then(|entry| entry.publish_credential_source.clone())),
            forge,
            ssh_directory,
        });
    }
    effective.sort_by(|left, right| left.id.cmp(&right.id));
    if effective
        .iter()
        .map(|repository| &repository.id)
        .collect::<BTreeSet<_>>()
        .len()
        != effective.len()
    {
        return Err(NativeBootstrapUnavailable::InvalidTrustedPolicy);
    }
    policy.repositories = effective;
    validate_policy(&policy, source_policy)?;
    Ok(policy)
}

fn effective_policy_digest(
    policy_bytes: &[u8],
    settings_revision: u64,
    policy: &TrustedHostPolicy,
    source: NativePolicySource,
) -> String {
    let repositories = policy
        .repositories
        .iter()
        .map(|repository| {
            serde_json::json!({
                "id": repository.id,
                "clone_url": repository.clone_url,
                "ssh_directory": repository.ssh_directory,
                "allowed_base_branches": repository.allowed_base_branches,
                "read_credential_source": repository.read_credential_source,
                "publish_credential_source": repository.publish_credential_source,
                "forge": repository.forge,
            })
        })
        .collect::<Vec<_>>();
    let material = serde_json::json!({
        "source": match source {
            NativePolicySource::ProtectedFile => "protected_file",
            NativePolicySource::VirtualPackageBaseline => "virtual_package_baseline",
        },
        "fixed_policy_sha256": sha256(policy_bytes),
        "settings_revision": settings_revision,
        "repositories": repositories,
    });
    sha256(&serde_json::to_vec(&material).expect("policy digest material is serializable"))
}

/// Build a private, short-lived executor configuration for the repository
/// diagnostic. This path intentionally does not create native session stores,
/// workspaces, receipts, or a jobworkerp operation.
pub(crate) async fn execute_repository_check(
    data_paths: &DataPaths,
    package: &NativePackageSelection,
    expected_policy_sha256: &str,
    repository: &RepositoryId,
) -> Result<RepositoryCheckResult, NativeRepositoryCheckError> {
    let (host_root, policy_bytes, policy, settings_revision, source) =
        load_validated_repository_policy(data_paths, package, PackageRecheck::Artifacts)
            .map_err(NativeRepositoryCheckError::Bootstrap)?;
    let effective_digest =
        effective_policy_digest(&policy_bytes, settings_revision, &policy, source);
    let legacy_digest_is_safe =
        settings_revision == 0 && sha256(&policy_bytes) == expected_policy_sha256;
    if effective_digest != expected_policy_sha256 && !legacy_digest_is_safe {
        return Err(NativeRepositoryCheckError::PolicyChanged);
    }
    let approved = policy
        .repositories
        .iter()
        .find(|entry| entry.id == *repository)
        .ok_or(NativeRepositoryCheckError::Bootstrap(
            NativeBootstrapUnavailable::RepositoryNotApproved,
        ))?;
    if !package
        .manifest
        .executor
        .capabilities
        .iter()
        .any(|feature| feature == "check_repository")
    {
        return Err(NativeRepositoryCheckError::UnsupportedCapability);
    }

    let checks_root = host_root.join("checks");
    ensure_private_dir_under(&host_root, &checks_root)
        .map_err(NativeRepositoryCheckError::Bootstrap)?;
    let mut temporary_builder = tempfile::Builder::new();
    temporary_builder.prefix("repository-check-");
    #[cfg(unix)]
    temporary_builder.permissions(fs::Permissions::from_mode(0o700));
    let temporary = temporary_builder.tempdir_in(&checks_root).map_err(|_| {
        NativeRepositoryCheckError::Bootstrap(NativeBootstrapUnavailable::StorageUnavailable)
    })?;
    validate_private_directory(temporary.path(), true).map_err(|_| {
        NativeRepositoryCheckError::Bootstrap(NativeBootstrapUnavailable::StorageUnavailable)
    })?;
    let workspace_root = temporary.path().join("workspace");
    let publisher_root = temporary.path().join("publisher");
    let state_root = temporary.path().join("state");
    let credential_root = temporary.path().join("credentials");
    let config_root = temporary.path().join("config");
    for root in [
        &workspace_root,
        &publisher_root,
        &state_root,
        &credential_root,
        &config_root,
    ] {
        create_private_directory(root).map_err(|_| {
            NativeRepositoryCheckError::Bootstrap(NativeBootstrapUnavailable::StorageUnavailable)
        })?;
    }

    // Probe the helper with the same private roots but no credential references;
    // capability discovery must not force any token snapshot.
    let probe_config = TrustedConfig {
        protocol_version: PROTOCOL_VERSION,
        expected_executor_version: package.manifest.executor.version.clone(),
        workspace_root: workspace_root.clone(),
        publisher_root: publisher_root.clone(),
        state_root: state_root.clone(),
        credential_root: credential_root.clone(),
        max_import_bytes: policy.max_import_bytes,
        repositories: vec![RepositoryConfig {
            id: approved.id.clone(),
            clone_url: approved.clone_url.clone(),
            ssh_directory: approved.ssh_directory.clone(),
            allowed_base_branches: approved.allowed_base_branches.clone(),
            read_credential_source: None,
            publish_credential_source: None,
            forge: approved.forge.clone(),
        }],
        credential_sources: Vec::new(),
    };
    let probe_bytes = serde_json::to_vec(&probe_config).map_err(|_| {
        NativeRepositoryCheckError::Bootstrap(NativeBootstrapUnavailable::InvalidTrustedPolicy)
    })?;
    let probe_digest = sha256(&probe_bytes);
    let probe_path = config_root.join(format!("probe-{probe_digest}.json"));
    write_immutable_private_file(&probe_path, &probe_bytes)
        .map_err(NativeRepositoryCheckError::Bootstrap)?;
    let executable = package.package_root.join(EXECUTOR_RELATIVE_PATH);
    let probed = NativeExecutorClient::probe(
        executable.clone(),
        probe_path,
        package.executor_sha256.clone(),
    )
    .await
    .map_err(NativeRepositoryCheckError::Executor)?;
    let expected_features = package
        .manifest
        .executor
        .capabilities
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if probed.protocol_version() != package.manifest.executor.protocol_version
        || probed.executor_version() != package.manifest.executor.version
        || probed.feature_set() != &expected_features
        || !probed.has_feature("check_repository")
    {
        return Err(NativeRepositoryCheckError::Bootstrap(
            NativeBootstrapUnavailable::ExecutorUnavailable,
        ));
    }

    let credential_plan =
        build_repository_check_credentials(&policy, approved, &host_root, &credential_root);
    let trusted_config = TrustedConfig {
        protocol_version: PROTOCOL_VERSION,
        expected_executor_version: package.manifest.executor.version.clone(),
        workspace_root,
        publisher_root,
        state_root,
        credential_root,
        max_import_bytes: policy.max_import_bytes,
        repositories: vec![RepositoryConfig {
            id: approved.id.clone(),
            clone_url: approved.clone_url.clone(),
            ssh_directory: approved.ssh_directory.clone(),
            allowed_base_branches: approved.allowed_base_branches.clone(),
            read_credential_source: credential_plan
                .git_read
                .then(|| approved.read_credential_source.clone())
                .flatten(),
            publish_credential_source: credential_plan
                .forge_api
                .then(|| approved.publish_credential_source.clone())
                .flatten(),
            forge: approved.forge.clone(),
        }],
        credential_sources: credential_plan.credential_sources.clone(),
    };
    let config_bytes = serde_json::to_vec(&trusted_config).map_err(|_| {
        NativeRepositoryCheckError::Bootstrap(NativeBootstrapUnavailable::InvalidTrustedPolicy)
    })?;
    let config_digest = sha256(&config_bytes);
    let config_path = config_root.join(format!("trusted-{config_digest}.json"));
    write_immutable_private_file(&config_path, &config_bytes)
        .map_err(NativeRepositoryCheckError::Bootstrap)?;
    if !credential_plan.git_read && !credential_plan.forge_api {
        return Ok(repository_check_result(
            repository,
            &config_digest,
            credential_plan
                .git_stage
                .expect("disabled Git branch has a local stage"),
            credential_plan
                .forge_stage
                .expect("disabled Forge branch has a local stage"),
        ));
    }
    let client = NativeExecutorClient::connect_with_capabilities(
        executable,
        config_path,
        package.executor_sha256.clone(),
        probed,
    )
    .map_err(NativeRepositoryCheckError::Executor)?;
    match client
        .check_repository(CheckRepositoryRequest {
            repository: repository.clone(),
            checks: Some(CheckRepositoryChecks {
                git_read: credential_plan.git_read,
                forge_api: credential_plan.forge_api,
            }),
        })
        .await
    {
        Ok(mut result) => {
            if !credential_plan.git_read {
                result.stages[0] = credential_plan
                    .git_stage
                    .clone()
                    .expect("disabled Git branch has a local stage");
            }
            if !credential_plan.forge_api {
                result.stages[1] = credential_plan
                    .forge_stage
                    .clone()
                    .expect("disabled Forge branch has a local stage");
            }
            Ok(result)
        }
        Err(error) => Ok(repository_check_result_for_executor_error(
            repository,
            &config_digest,
            &credential_plan,
            &error,
        )),
    }
}

#[derive(Debug, Error)]
pub(crate) enum NativeRepositoryCheckError {
    #[error("native repository check bootstrap is unavailable")]
    Bootstrap(NativeBootstrapUnavailable),
    #[error("native repository check executor failed")]
    Executor(NativeExecutorError),
    #[error("native repository check capability is unavailable")]
    UnsupportedCapability,
    #[error("native repository policy changed during the check")]
    PolicyChanged,
}

#[derive(Clone, Debug)]
struct RepositoryCheckCredentialPlan {
    credential_sources: Vec<CredentialSourceConfig>,
    git_read: bool,
    forge_api: bool,
    git_stage: Option<RepositoryCheckStage>,
    forge_stage: Option<RepositoryCheckStage>,
}

fn build_repository_check_credentials(
    policy: &TrustedHostPolicy,
    repository: &HostRepository,
    host_root: &Path,
    credential_root: &Path,
) -> RepositoryCheckCredentialPlan {
    let publication_credential_source = repository
        .forge
        .as_ref()
        .and(repository.publish_credential_source.as_deref());
    let credential_ids = [
        repository.read_credential_source.as_deref(),
        publication_credential_source,
    ]
    .into_iter()
    .flatten()
    .collect::<BTreeSet<_>>();
    let source_root = host_root.join("credential-sources");
    let source_root_available =
        credential_ids.is_empty() || validate_private_directory(&source_root, true).is_ok();
    let mut outcomes = BTreeMap::new();
    for id in credential_ids {
        let outcome = if source_root_available {
            build_credential_source(policy, id, &source_root, credential_root)
        } else {
            Err(NativeBootstrapUnavailable::CredentialUnavailable)
        };
        outcomes.insert(id, outcome);
    }
    let credential_sources = outcomes
        .values()
        .filter_map(|outcome| outcome.as_ref().ok().cloned())
        .collect::<Vec<_>>();

    let git_read = repository
        .read_credential_source
        .as_deref()
        .is_none_or(|id| outcomes.get(id).is_some_and(Result::is_ok));
    let git_stage =
        (!git_read).then(|| diagnostic_stage("git_read", "failed", "credential_unavailable"));

    let (forge_api, forge_stage) = match (
        repository.forge.as_ref(),
        repository.publish_credential_source.as_deref(),
    ) {
        (None, _) => (
            false,
            Some(diagnostic_stage(
                "forge_api",
                "skipped",
                "forge_not_configured",
            )),
        ),
        (Some(_), None) => (
            false,
            Some(diagnostic_stage(
                "forge_api",
                "skipped",
                "credential_not_configured",
            )),
        ),
        (Some(_), Some(id)) if outcomes.get(id).is_some_and(Result::is_ok) => (true, None),
        (Some(_), Some(_)) => (
            false,
            Some(diagnostic_stage(
                "forge_api",
                "failed",
                "credential_unavailable",
            )),
        ),
    };

    RepositoryCheckCredentialPlan {
        credential_sources,
        git_read,
        forge_api,
        git_stage,
        forge_stage,
    }
}

fn repository_check_result(
    repository: &RepositoryId,
    trusted_config_digest: &str,
    git_stage: RepositoryCheckStage,
    forge_stage: RepositoryCheckStage,
) -> RepositoryCheckResult {
    RepositoryCheckResult {
        repository: repository.clone(),
        trusted_config_digest: trusted_config_digest.to_owned(),
        protocol_version: PROTOCOL_VERSION,
        executor_version: EXECUTOR_VERSION.to_owned(),
        stages: vec![git_stage, forge_stage],
    }
}

fn repository_check_result_for_executor_error(
    repository: &RepositoryId,
    trusted_config_digest: &str,
    plan: &RepositoryCheckCredentialPlan,
    error: &NativeExecutorError,
) -> RepositoryCheckResult {
    let git_stage = if plan.git_read {
        diagnostic_stage("git_read", "failed", executor_error_code(error))
    } else {
        plan.git_stage
            .clone()
            .expect("disabled Git branch has a local stage")
    };
    let forge_stage = if plan.forge_api {
        diagnostic_stage("forge_api", "failed", executor_error_code(error))
    } else {
        plan.forge_stage
            .clone()
            .expect("disabled Forge branch has a local stage")
    };
    repository_check_result(repository, trusted_config_digest, git_stage, forge_stage)
}

fn executor_error_code(error: &NativeExecutorError) -> &'static str {
    match error {
        NativeExecutorError::DiagnosticTransport { reason } => match reason {
            NativeDiagnosticTransportReason::Timeout => "timeout",
            NativeDiagnosticTransportReason::ResponseTooLarge => "response_too_large",
            NativeDiagnosticTransportReason::Unavailable => "unavailable",
        },
        NativeExecutorError::ExecutorRejected(ExecutorErrorCode::GitOperationFailed) => {
            "git_failed"
        }
        NativeExecutorError::ExecutorRejected(ExecutorErrorCode::ResponseTooLarge) => {
            "response_too_large"
        }
        _ => "unavailable",
    }
}

fn diagnostic_stage(stage: &str, status: &str, code: &str) -> RepositoryCheckStage {
    RepositoryCheckStage {
        stage: stage.to_owned(),
        status: status.to_owned(),
        code: code.to_owned(),
    }
}

/// Discover only bounded, canonical repository partitions under this app's
/// private native data root. This is used to freeze profile edits from durable
/// unresolved sessions even when their repository was removed from the current
/// host policy; it never scans HOME or any workspace tree.
pub(crate) fn existing_session_store_paths(
    data_paths: &DataPaths,
) -> Result<Vec<PathBuf>, NativeBootstrapUnavailable> {
    validate_data_root(&data_paths.root)?;
    let host_root = data_paths.root.join("coding-agent/native");
    match fs::symlink_metadata(&host_root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(NativeBootstrapUnavailable::UnsafeTrustedPolicy),
        Ok(_) => {}
    }
    let host_root = native_host_root(data_paths)?;
    let repositories_root = host_root.join("repositories");
    match fs::symlink_metadata(&repositories_root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(NativeBootstrapUnavailable::StorageUnavailable),
        Ok(_) => {}
    }
    validate_private_directory(&repositories_root, true)
        .map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?;

    let entries = fs::read_dir(&repositories_root)
        .map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?;
    if entries.len() > MAX_REPOSITORIES {
        return Err(NativeBootstrapUnavailable::StorageUnavailable);
    }
    let mut databases = Vec::new();
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?;
        if name.len() != 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(NativeBootstrapUnavailable::StorageUnavailable);
        }
        let repository_root = entry.path();
        validate_private_directory(&repository_root, true)
            .map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?;
        let sessions_root = repository_root.join("sessions");
        match fs::symlink_metadata(&sessions_root) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(NativeBootstrapUnavailable::StorageUnavailable),
            Ok(_) => {}
        }
        validate_private_directory(&sessions_root, true)
            .map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?;
        let database_path = sessions_root.join("native-terminal.sqlite3");
        match fs::symlink_metadata(&database_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(NativeBootstrapUnavailable::StorageUnavailable),
            Ok(metadata) => validate_private_file(&metadata, 0o600)
                .map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?,
        }
        databases.push(database_path);
    }
    databases.sort();
    Ok(databases)
}

/// Load only the fixed host policy and configure one approved repository.
/// Callers should treat errors as an unavailable optional feature; no app
/// startup path needs to fail when this backend is not provisioned.
pub async fn configure_repository(
    data_paths: &DataPaths,
    package: &NativePackageSelection,
    repository: &RepositoryId,
) -> Result<ConfiguredNativeRepository, NativeBootstrapUnavailable> {
    let (host_root, _policy_bytes, policy, _settings_revision, _source) =
        load_validated_repository_policy(data_paths, package, PackageRecheck::Artifacts)?;

    let approved_repository = policy
        .repositories
        .iter()
        .find(|entry| entry.id == *repository)
        .ok_or(NativeBootstrapUnavailable::RepositoryNotApproved)?;

    let partition = repository_partition(repository)?;
    let repo_root = host_root.join("repositories").join(&partition);
    let workspace_root = repo_root.join("workspace");
    let publisher_root = repo_root.join("publisher");
    let state_root = repo_root.join("state");
    let credential_root = repo_root.join("credentials");
    let session_root = repo_root.join("sessions");
    let output_root = repo_root.join("output");
    let doctor_path = state_root.join("native-terminal-doctor.sqlite3");
    let config_root = host_root.join("configs").join(&partition);
    for root in [
        &workspace_root,
        &publisher_root,
        &state_root,
        &credential_root,
        &session_root,
        &output_root,
        &config_root,
    ] {
        ensure_private_dir_under(&host_root, root)?;
    }
    validate_all_roots_disjoint(&[
        &workspace_root,
        &publisher_root,
        &state_root,
        &credential_root,
        &session_root,
        &output_root,
        &config_root,
        &package.package_root,
    ])?;

    let credential_sources =
        build_credential_sources(&policy, approved_repository, &host_root, &credential_root)?;
    let trusted_config = TrustedConfig {
        protocol_version: PROTOCOL_VERSION,
        expected_executor_version: package.manifest.executor.version.clone(),
        workspace_root: workspace_root.clone(),
        publisher_root,
        state_root,
        credential_root,
        max_import_bytes: policy.max_import_bytes,
        repositories: vec![RepositoryConfig {
            id: approved_repository.id.clone(),
            clone_url: approved_repository.clone_url.clone(),
            ssh_directory: approved_repository.ssh_directory.clone(),
            allowed_base_branches: approved_repository.allowed_base_branches.clone(),
            read_credential_source: approved_repository.read_credential_source.clone(),
            publish_credential_source: approved_repository.publish_credential_source.clone(),
            forge: approved_repository.forge.clone(),
        }],
        credential_sources,
    };
    let config_bytes = serde_json::to_vec(&trusted_config)
        .map_err(|_| NativeBootstrapUnavailable::InvalidTrustedPolicy)?;
    let config_digest = sha256(&config_bytes);
    let config_path = config_root.join(format!("trusted-{config_digest}.json"));
    write_immutable_private_file(&config_path, &config_bytes)?;

    let executable = package.package_root.join(EXECUTOR_RELATIVE_PATH);
    let client =
        NativeExecutorClient::connect(executable, config_path, package.executor_sha256.clone())
            .await
            .map_err(map_executor_error)?;
    let probed = client.verified_capabilities().map_err(map_executor_error)?;
    let expected_features = package
        .manifest
        .executor
        .capabilities
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if probed.protocol_version() != package.manifest.executor.protocol_version
        || probed.executor_version() != package.manifest.executor.version
        || probed.feature_set() != &expected_features
    {
        return Err(NativeBootstrapUnavailable::ExecutorUnavailable);
    }

    let session_path = session_root.join("native-terminal.sqlite3");
    let output_path = output_root.join("native-terminal-output.sqlite3");
    let session_store = NativeTerminalStore::open(&session_path).map_err(map_store_error)?;
    let output_store = NativeTerminalOutputStore::open(&output_path).map_err(map_output_error)?;
    let doctor_store =
        NativeRuntimeDoctorStore::open(&doctor_path).map_err(map_doctor_store_error)?;
    output_store
        .ensure_disjoint_from_workspace(&workspace_root)
        .map_err(map_output_error)?;

    Ok(ConfiguredNativeRepository {
        repository: repository.clone(),
        config_digest,
        workspace_root,
        preparation_policy: NativeRepositoryPreparationPolicy {
            allowed_base_branches: approved_repository.allowed_base_branches.clone(),
            protected_host_roots: vec![host_root, package.package_root.clone()],
        },
        executor: NativeWorkspaceExecutor {
            supports_resolve_base: probed.has_feature("resolve_base"),
            client,
        },
        session_store,
        output_store,
        doctor_store,
    })
}

fn map_executor_error(error: NativeExecutorError) -> NativeBootstrapUnavailable {
    match error {
        NativeExecutorError::InvalidPackage => NativeBootstrapUnavailable::PackageUnavailable,
        NativeExecutorError::InvalidTrustedConfiguration => {
            NativeBootstrapUnavailable::InvalidTrustedPolicy
        }
        NativeExecutorError::IncompatibleProtocol => {
            NativeBootstrapUnavailable::ExecutorUnavailable
        }
        NativeExecutorError::UnsupportedCapability { .. } => {
            NativeBootstrapUnavailable::ExecutorUnavailable
        }
        NativeExecutorError::Unavailable
        | NativeExecutorError::ExecutorRejected(_)
        | NativeExecutorError::DiagnosticTransport { .. }
        | NativeExecutorError::InvalidRequest
        | NativeExecutorError::OutcomeUnknown { .. } => {
            NativeBootstrapUnavailable::ExecutorUnavailable
        }
    }
}

fn map_store_error(_: NativeTerminalStoreError) -> NativeBootstrapUnavailable {
    NativeBootstrapUnavailable::StorageUnavailable
}

fn map_output_error(_: NativeTerminalOutputError) -> NativeBootstrapUnavailable {
    NativeBootstrapUnavailable::StorageUnavailable
}

fn map_doctor_store_error(_: NativeRuntimeDoctorStoreError) -> NativeBootstrapUnavailable {
    NativeBootstrapUnavailable::StorageUnavailable
}

fn native_host_root(data_paths: &DataPaths) -> Result<PathBuf, NativeBootstrapUnavailable> {
    validate_data_root(&data_paths.root)?;
    let parent = data_paths.root.join("coding-agent");
    match fs::symlink_metadata(&parent) {
        Ok(_) => validate_private_directory(&parent, true)
            .map_err(|_| NativeBootstrapUnavailable::UnsafeTrustedPolicy)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_private_directory(&parent)
                .map_err(|_| NativeBootstrapUnavailable::UnsafeTrustedPolicy)?;
        }
        Err(_) => return Err(NativeBootstrapUnavailable::UnsafeTrustedPolicy),
    }
    let host_root = parent.join("native");
    match fs::symlink_metadata(&host_root) {
        Ok(_) => validate_private_directory(&host_root, true)
            .map_err(|_| NativeBootstrapUnavailable::UnsafeTrustedPolicy)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_private_directory(&host_root)
                .map_err(|_| NativeBootstrapUnavailable::UnsafeTrustedPolicy)?;
        }
        Err(_) => return Err(NativeBootstrapUnavailable::UnsafeTrustedPolicy),
    }
    validate_non_writable_directory(&parent)?;
    Ok(host_root)
}

fn read_host_policy_file(
    host_root: &Path,
    policy_path: &Path,
) -> Result<Vec<u8>, NativeBootstrapUnavailable> {
    let metadata = match fs::symlink_metadata(policy_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(NativeBootstrapUnavailable::TrustedPolicyMissing);
        }
        Err(_) => return Err(NativeBootstrapUnavailable::UnsafeTrustedPolicy),
    };
    validate_private_file(&metadata, 0o600)
        .map_err(|_| NativeBootstrapUnavailable::UnsafeTrustedPolicy)?;
    if !policy_path.starts_with(host_root) || !is_canonical(policy_path) {
        return Err(NativeBootstrapUnavailable::UnsafeTrustedPolicy);
    }
    open_nofollow(policy_path)
        .and_then(|mut file| {
            let metadata = file.metadata()?;
            validate_private_file(&metadata, 0o600)
                .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "unsafe"))?;
            read_bounded(&mut file, MAX_POLICY_BYTES)
        })
        .map_err(|_| NativeBootstrapUnavailable::UnsafeTrustedPolicy)
}

fn validate_data_root(root: &Path) -> Result<(), NativeBootstrapUnavailable> {
    if !root.is_absolute()
        || root
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
        || !is_canonical(root)
        || validate_trusted_path_ancestors(root).is_err()
    {
        return Err(NativeBootstrapUnavailable::UnsafeTrustedPolicy);
    }
    let metadata =
        fs::symlink_metadata(root).map_err(|_| NativeBootstrapUnavailable::UnsafeTrustedPolicy)?;
    validate_non_writable_directory_metadata(&metadata)
        .map_err(|_| NativeBootstrapUnavailable::UnsafeTrustedPolicy)
}

fn validate_package_root(package_root: &Path) -> Result<(), NativeBootstrapUnavailable> {
    if !package_root.is_absolute()
        || package_root
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
        || !is_canonical(package_root)
        || validate_trusted_path_ancestors(package_root).is_err()
    {
        return Err(NativeBootstrapUnavailable::PackageUnavailable);
    }
    let metadata = fs::symlink_metadata(package_root)
        .map_err(|_| NativeBootstrapUnavailable::PackageUnavailable)?;
    validate_package_directory_metadata(&metadata)
        .map_err(|_| NativeBootstrapUnavailable::PackageUnavailable)
}

fn ensure_package_is_separate(
    package: &NativePackageSelection,
    host_root: &Path,
) -> Result<(), NativeBootstrapUnavailable> {
    if roots_overlap(&package.package_root, host_root) {
        Err(NativeBootstrapUnavailable::PackageUnavailable)
    } else {
        Ok(())
    }
}

fn validate_manifest(
    manifest: &ArtifactManifest,
    source_policy: PackageSourcePolicy,
) -> Result<(), NativeBootstrapUnavailable> {
    if manifest.schema_version != 1
        || manifest.profile.name != "linux-kvm"
        || manifest.profile.version != 1
        || manifest.target.triple
            != supported_target_triple().ok_or(NativeBootstrapUnavailable::UnsupportedPlatform)?
        || manifest.executor.version != EXECUTOR_VERSION
        || manifest.executor.protocol_version != PROTOCOL_VERSION
        || manifest.runtime_compatibility.protocol_version != serde_json::Value::Null
        || manifest.runtime_compatibility.config_version != serde_json::Value::Null
        || manifest.artifacts.runtime.path != "runtime/all-in-one"
        || manifest.artifacts.executor.path != EXECUTOR_RELATIVE_PATH
        || manifest.artifacts.configuration.len() > 256
    {
        return Err(NativeBootstrapUnavailable::PackageUnavailable);
    }
    validate_package_identity(&manifest.package_identity(), source_policy)
        .map_err(|_| NativeBootstrapUnavailable::PackageUnavailable)?;

    let capabilities = &manifest.executor.capabilities;
    let features = capabilities
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if capabilities.len() < CORE_EXECUTOR_FEATURES.len()
        || capabilities.len() > EXECUTOR_FEATURES.len()
        || features.len() != capabilities.len()
        || features
            .iter()
            .any(|feature| !EXECUTOR_FEATURES.contains(feature))
        || CORE_EXECUTOR_FEATURES
            .iter()
            .any(|required| !features.contains(required))
        || features.contains("push") != features.contains("secret_scan_supported_formats_v1")
        || (features.contains("pull_request") && !features.contains("push"))
    {
        return Err(NativeBootstrapUnavailable::PackageUnavailable);
    }

    let paths = manifest
        .artifacts
        .configuration
        .iter()
        .map(|artifact| artifact.path.as_str())
        .collect::<BTreeSet<_>>();
    if paths.len() != manifest.artifacts.configuration.len() {
        return Err(NativeBootstrapUnavailable::PackageUnavailable);
    }
    for artifact in manifest.artifacts.all() {
        if !safe_artifact_path(&artifact.path) || !is_lower_hex(&artifact.sha256, 64) {
            return Err(NativeBootstrapUnavailable::PackageUnavailable);
        }
    }
    Ok(())
}

fn validate_trusted_path_ancestors(path: &Path) -> Result<(), ()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(part) => current.push(part),
            _ => return Err(()),
        }
        let metadata = fs::symlink_metadata(&current).map_err(|_| ())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let owner = metadata.uid();
            let mode = metadata.mode();
            let owned_by_trusted_user = owner == unsafe { libc::geteuid() } || owner == 0;
            let protected_sticky_directory = owner == 0 && mode & 0o1000 != 0;
            if !owned_by_trusted_user || (mode & 0o0022 != 0 && !protected_sticky_directory) {
                return Err(());
            }
        }
        #[cfg(not(unix))]
        return Err(());
    }
    Ok(())
}

fn verify_artifact_files(
    package_root: &Path,
    manifest: &ArtifactManifest,
) -> Result<(), NativeBootstrapUnavailable> {
    for artifact in manifest.artifacts.all() {
        let max_bytes = if artifact.path == EXECUTOR_RELATIVE_PATH {
            MAX_EXECUTOR_BYTES
        } else if artifact.path == "runtime/all-in-one" {
            MAX_RUNTIME_BYTES
        } else {
            MAX_CONFIG_ARTIFACT_BYTES
        };
        let digest = hash_package_file(package_root, &package_root.join(&artifact.path), max_bytes)
            .map_err(|_| NativeBootstrapUnavailable::PackageUnavailable)?;
        if digest != artifact.sha256 {
            return Err(NativeBootstrapUnavailable::PackageUnavailable);
        }
    }
    Ok(())
}

fn hash_package_file(package_root: &Path, path: &Path, max_bytes: u64) -> io::Result<String> {
    validate_package_file_path(package_root, path)?;
    let mut file = open_nofollow(path)?;
    let metadata = file.metadata()?;
    validate_package_file_metadata(&metadata, path.ends_with(EXECUTOR_RELATIVE_PATH))?;
    if metadata.len() > max_bytes {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "size"));
    }
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > max_bytes {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "size"));
        }
        hasher.update(&buffer[..count]);
    }
    if total != metadata.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "changed"));
    }
    Ok(hex(&hasher.finalize()))
}

#[cfg(target_os = "linux")]
struct ManagedExecutableDigest {
    device: u64,
    inode: u64,
    sha256: String,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ManagedExecutableMetadata {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    mode: u32,
    owner: u32,
}

#[cfg(target_os = "linux")]
impl ManagedExecutableMetadata {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            mode: metadata.mode(),
            owner: metadata.uid(),
        }
    }
}

#[cfg(target_os = "linux")]
fn hash_managed_process_executable(
    child_pid: u32,
) -> Result<ManagedExecutableDigest, NativeRuntimeImageUnavailable> {
    const MAX_RUNNING_RUNTIME_BYTES: u64 = 1024 * 1024 * 1024;

    use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt};

    let executable_path = PathBuf::from(format!("/proc/{child_pid}/exe"));
    let open = || {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(&executable_path)
    };
    let mut executable = open().map_err(|_| NativeRuntimeImageUnavailable::UnsafeExecutable)?;
    let before_metadata = executable
        .metadata()
        .map_err(|_| NativeRuntimeImageUnavailable::UnsafeExecutable)?;
    validate_package_file_metadata(&before_metadata, true)
        .map_err(|_| NativeRuntimeImageUnavailable::UnsafeExecutable)?;
    if before_metadata.len() > MAX_RUNNING_RUNTIME_BYTES {
        return Err(NativeRuntimeImageUnavailable::ExecutableTooLarge);
    }
    let before = ManagedExecutableMetadata::from_metadata(&before_metadata);

    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = executable
            .read(&mut buffer)
            .map_err(|_| NativeRuntimeImageUnavailable::UnsafeExecutable)?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count as u64);
        if total > MAX_RUNNING_RUNTIME_BYTES {
            return Err(NativeRuntimeImageUnavailable::ExecutableTooLarge);
        }
        hasher.update(&buffer[..count]);
    }
    let after_metadata = executable
        .metadata()
        .map_err(|_| NativeRuntimeImageUnavailable::UnsafeExecutable)?;
    let after = ManagedExecutableMetadata::from_metadata(&after_metadata);
    if before != after || total != before.length {
        return Err(NativeRuntimeImageUnavailable::ExecutableChanged);
    }

    // Reopen the proc link after hashing so an exec replacement cannot be
    // mistaken for the file whose descriptor was just consumed.
    let current = open().map_err(|_| NativeRuntimeImageUnavailable::ExecutableChanged)?;
    let current_metadata = current
        .metadata()
        .map_err(|_| NativeRuntimeImageUnavailable::ExecutableChanged)?;
    if ManagedExecutableMetadata::from_metadata(&current_metadata) != after {
        return Err(NativeRuntimeImageUnavailable::ExecutableChanged);
    }

    Ok(ManagedExecutableDigest {
        device: after.device,
        inode: after.inode,
        sha256: hex(&hasher.finalize()),
    })
}

fn read_package_file(
    package_root: &Path,
    path: &Path,
    max_bytes: u64,
    executable: bool,
) -> io::Result<Vec<u8>> {
    validate_package_file_path(package_root, path)?;
    let mut file = open_nofollow(path)?;
    validate_package_file_metadata(&file.metadata()?, executable)?;
    read_bounded(&mut file, max_bytes)
}

fn validate_package_file_path(package_root: &Path, path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || !path.starts_with(package_root)
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
        || !is_canonical(path)
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path"));
    }
    let components = path.components().collect::<Vec<_>>();
    let mut current = PathBuf::new();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "symlink"));
        }
        if index + 1 < components.len() && !metadata.is_dir() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "component"));
        }
        if current.starts_with(package_root) && metadata.is_dir() {
            validate_package_directory_metadata(&metadata)
                .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "directory"))?;
        }
    }
    Ok(())
}

fn read_bounded(reader: &mut impl Read, max_bytes: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "size"));
    }
    Ok(bytes)
}

fn validate_policy(
    policy: &TrustedHostPolicy,
    source_policy: PackageSourcePolicy,
) -> Result<(), NativeBootstrapUnavailable> {
    validate_policy_with_mode(policy, true, source_policy)
}

fn validate_policy_structure(
    policy: &TrustedHostPolicy,
    source_policy: PackageSourcePolicy,
) -> Result<(), NativeBootstrapUnavailable> {
    validate_policy_with_mode(policy, false, source_policy)
}

fn validate_policy_with_mode(
    policy: &TrustedHostPolicy,
    require_ssh_directory_metadata: bool,
    source_policy: PackageSourcePolicy,
) -> Result<(), NativeBootstrapUnavailable> {
    if policy.version != 1
        || policy.max_import_bytes == 0
        || policy.max_import_bytes > 1024 * 1024 * 1024
        || policy.repositories.len() > MAX_REPOSITORIES
        || policy.credential_sources.len() > MAX_CREDENTIAL_SOURCES
    {
        return Err(NativeBootstrapUnavailable::InvalidTrustedPolicy);
    }
    validate_package_identity(&policy.expected_package_identity, source_policy)
        .map_err(|_| NativeBootstrapUnavailable::InvalidTrustedPolicy)?;

    let mut repositories = BTreeSet::new();
    for repository in &policy.repositories {
        if !valid_repository_id(&repository.id)
            || !repositories.insert(repository.id.clone())
            || !clone_url_matches_repository(&repository.clone_url, &repository.id)
            || repository.allowed_base_branches.is_empty()
            || repository.allowed_base_branches.len() > 128
        {
            return Err(NativeBootstrapUnavailable::InvalidTrustedPolicy);
        }
        let uses_ssh = repository.clone_url.starts_with("ssh:");
        let ssh_directory_invalid = repository.ssh_directory.as_ref().is_some_and(|directory| {
            if require_ssh_directory_metadata {
                coding_agent_executor::validate_ssh_directory(directory).is_err()
            } else {
                !is_structurally_safe_ssh_directory(directory)
            }
        });
        if uses_ssh != repository.ssh_directory.is_some() || ssh_directory_invalid {
            return Err(NativeBootstrapUnavailable::InvalidTrustedPolicy);
        }
        let mut branches = BTreeSet::new();
        for branch in &repository.allowed_base_branches {
            if !valid_branch(branch) || !branches.insert(branch) {
                return Err(NativeBootstrapUnavailable::InvalidTrustedPolicy);
            }
        }
    }

    let mut credentials = BTreeSet::new();
    for credential in &policy.credential_sources {
        if !valid_identifier(&credential.id)
            || !credentials.insert(credential.id.as_str())
            || credential.username.is_empty()
            || credential.username.len() > 255
            || credential.username.chars().any(char::is_control)
            || !safe_relative_path(&credential.token_file)
        {
            return Err(NativeBootstrapUnavailable::InvalidTrustedPolicy);
        }
    }
    for repository in &policy.repositories {
        if [
            repository.read_credential_source.as_deref(),
            repository.publish_credential_source.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|id| !credentials.contains(id))
        {
            return Err(NativeBootstrapUnavailable::InvalidTrustedPolicy);
        }
    }
    Ok(())
}

fn is_structurally_safe_ssh_directory(path: &Path) -> bool {
    path.is_absolute()
        && path.parent().is_some()
        && !path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
}

fn build_credential_sources(
    policy: &TrustedHostPolicy,
    repository: &HostRepository,
    host_root: &Path,
    credential_root: &Path,
) -> Result<Vec<CredentialSourceConfig>, NativeBootstrapUnavailable> {
    let credential_ids = [
        repository.read_credential_source.as_deref(),
        repository.publish_credential_source.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<BTreeSet<_>>();
    if credential_ids.is_empty() {
        return Ok(Vec::new());
    }
    let source_root = host_root.join("credential-sources");
    validate_private_directory(&source_root, true)
        .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
    credential_ids
        .into_iter()
        .map(|id| build_credential_source(policy, id, &source_root, credential_root))
        .collect()
}

fn build_credential_source(
    policy: &TrustedHostPolicy,
    credential_id: &str,
    source_root: &Path,
    credential_root: &Path,
) -> Result<CredentialSourceConfig, NativeBootstrapUnavailable> {
    let source = policy
        .credential_sources
        .iter()
        .find(|source| source.id == credential_id)
        .ok_or(NativeBootstrapUnavailable::InvalidTrustedPolicy)?;
    let source_path = source_root.join(&source.token_file);
    let mut source_file = open_host_secret_file(source_root, &source_path)?;
    let mut token = read_bounded(&mut source_file, MAX_CREDENTIAL_BYTES as u64)
        .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
    while matches!(token.last(), Some(b'\n' | b'\r')) {
        token.pop();
    }
    if token.is_empty() || token.iter().any(|byte| matches!(*byte, 0 | b'\n' | b'\r')) {
        token.fill(0);
        return Err(NativeBootstrapUnavailable::CredentialUnavailable);
    }

    let relative_token_path = PathBuf::from(format!("credential-{}.token", sha256(&token)));
    let token_path = credential_root.join(&relative_token_path);
    let write_result = write_immutable_credential(&token_path, &token);
    token.fill(0);
    write_result?;
    Ok(CredentialSourceConfig {
        id: source.id.clone(),
        username: source.username.clone(),
        token_file: relative_token_path,
    })
}

fn write_immutable_credential(path: &Path, bytes: &[u8]) -> Result<(), NativeBootstrapUnavailable> {
    write_immutable_credential_with(path, bytes, |file, bytes| file.write_all(bytes))
}

fn write_immutable_credential_with(
    path: &Path,
    bytes: &[u8],
    write_contents: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
) -> Result<(), NativeBootstrapUnavailable> {
    write_immutable_credential_with_sync(path, bytes, write_contents, sync_directory)
}

fn write_immutable_credential_with_sync(
    path: &Path,
    bytes: &[u8],
    write_contents: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
    mut sync_parent: impl FnMut(&Path) -> Result<(), NativeBootstrapUnavailable>,
) -> Result<(), NativeBootstrapUnavailable> {
    let parent = path
        .parent()
        .ok_or(NativeBootstrapUnavailable::CredentialUnavailable)?;
    let temporary_path = parent.join(format!(
        ".credential-{}.tmp",
        random_private_name().map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?
    ));
    let _temporary = PrivateTemporaryFile::new(temporary_path.clone());
    let mut file = create_private_new_file(&temporary_path)
        .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
    write_contents(&mut file, bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
    drop(file);

    match fs::hard_link(&temporary_path, path) {
        Ok(()) => {
            sync_parent(parent)?;
            fs::remove_file(&temporary_path)
                .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
            sync_parent(parent)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut existing = open_nofollow(path)
                .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
            validate_private_file(
                &existing
                    .metadata()
                    .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?,
                0o600,
            )
            .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
            let mut stored = read_bounded(&mut existing, MAX_CREDENTIAL_BYTES as u64)
                .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
            let matches = stored == bytes && sha256(&stored) == sha256(bytes);
            stored.fill(0);
            if matches {
                // Another publisher may have linked this name without syncing
                // the directory yet. Adoption must establish durability too.
                sync_parent(parent)
            } else {
                Err(NativeBootstrapUnavailable::CredentialUnavailable)
            }
        }
        Err(_) => Err(NativeBootstrapUnavailable::CredentialUnavailable),
    }
}

struct PrivateTemporaryFile {
    path: PathBuf,
}

impl PrivateTemporaryFile {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for PrivateTemporaryFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn sync_directory(path: &Path) -> Result<(), NativeBootstrapUnavailable> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)
}

fn open_host_secret_file(root: &Path, path: &Path) -> Result<File, NativeBootstrapUnavailable> {
    if !path.starts_with(root) || !safe_absolute_child(root, path) {
        return Err(NativeBootstrapUnavailable::CredentialUnavailable);
    }
    let mut current = root.to_path_buf();
    let relative = path
        .strip_prefix(root)
        .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(NativeBootstrapUnavailable::CredentialUnavailable);
        };
        current.push(part);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
        if metadata.file_type().is_symlink()
            || (current != path && !metadata.is_dir())
            || (current == path && !metadata.is_file())
        {
            return Err(NativeBootstrapUnavailable::CredentialUnavailable);
        }
        if current != path {
            validate_private_directory(&current, true)
                .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
        } else {
            validate_private_file(&metadata, 0o600)
                .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
        }
    }
    if !is_canonical(path) {
        return Err(NativeBootstrapUnavailable::CredentialUnavailable);
    }
    let file =
        open_nofollow(path).map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
    validate_private_file(
        &file
            .metadata()
            .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?,
        0o600,
    )
    .map_err(|_| NativeBootstrapUnavailable::CredentialUnavailable)?;
    Ok(file)
}

fn ensure_private_dir_under(
    host_root: &Path,
    directory: &Path,
) -> Result<(), NativeBootstrapUnavailable> {
    if !directory.starts_with(host_root) || directory == host_root {
        return Err(NativeBootstrapUnavailable::StorageUnavailable);
    }
    let relative = directory
        .strip_prefix(host_root)
        .map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?;
    let mut current = host_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(NativeBootstrapUnavailable::StorageUnavailable);
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => validate_private_directory_metadata(&metadata)
                .map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_private_directory(&current)
                    .map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?;
            }
            Err(_) => return Err(NativeBootstrapUnavailable::StorageUnavailable),
        }
    }
    if !is_canonical(directory) {
        return Err(NativeBootstrapUnavailable::StorageUnavailable);
    }
    Ok(())
}

fn write_immutable_private_file(
    path: &Path,
    bytes: &[u8],
) -> Result<(), NativeBootstrapUnavailable> {
    let parent = path
        .parent()
        .ok_or(NativeBootstrapUnavailable::StorageUnavailable)?;
    let temporary = parent.join(format!(".trusted-{}.tmp", random_private_name()?));
    let mut file = create_private_new_file(&temporary)
        .map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?;
    let write_result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err(NativeBootstrapUnavailable::StorageUnavailable);
    }
    match fs::hard_link(&temporary, path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let existing = read_host_policy_file(parent, path)
                .map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?;
            if existing != bytes {
                let _ = fs::remove_file(&temporary);
                return Err(NativeBootstrapUnavailable::StorageUnavailable);
            }
        }
        Err(_) => {
            let _ = fs::remove_file(&temporary);
            return Err(NativeBootstrapUnavailable::StorageUnavailable);
        }
    }
    fs::remove_file(&temporary).map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?;
    Ok(())
}

fn create_private_new_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    validate_private_file(&file.metadata()?, 0o600)
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "unsafe file"))?;
    Ok(file)
}

fn random_private_name() -> Result<String, NativeBootstrapUnavailable> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| NativeBootstrapUnavailable::StorageUnavailable)?;
    Ok(hex(&random))
}

fn validate_all_roots_disjoint(roots: &[&Path]) -> Result<(), NativeBootstrapUnavailable> {
    for (index, root) in roots.iter().enumerate() {
        if !is_canonical(root) {
            return Err(NativeBootstrapUnavailable::StorageUnavailable);
        }
        for other in roots.iter().skip(index + 1) {
            if roots_overlap(root, other) {
                return Err(NativeBootstrapUnavailable::StorageUnavailable);
            }
        }
    }
    Ok(())
}

fn repository_partition(repository: &RepositoryId) -> Result<String, NativeBootstrapUnavailable> {
    let bytes = serde_json::to_vec(repository)
        .map_err(|_| NativeBootstrapUnavailable::InvalidTrustedPolicy)?;
    Ok(sha256(&bytes))
}

fn roots_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn is_canonical(path: &Path) -> bool {
    path.canonicalize().is_ok_and(|canonical| canonical == path)
}

fn validate_package_identity(
    identity: &PackageIdentity,
    source_policy: PackageSourcePolicy,
) -> Result<(), ()> {
    if identity.profile.name != "linux-kvm"
        || identity.profile.version != 1
        || identity.target.triple.is_empty()
        || identity.target.triple.len() > 80
        || identity.target.triple.chars().any(char::is_control)
        || !valid_source(&identity.sources.agent_app, source_policy)
        || !valid_source(&identity.sources.jobworkerp_runtime, source_policy)
        || !valid_client_source(&identity.sources.jobworkerp_client)
        || !valid_semver(&identity.executor.version)
        || identity.executor.protocol_version != PROTOCOL_VERSION
        || !valid_manifest_capabilities(&identity.executor.capabilities)
    {
        return Err(());
    }
    Ok(())
}

fn valid_source(source: &RuntimeSource, source_policy: PackageSourcePolicy) -> bool {
    // A development policy tolerates working-tree provenance (dirty trees and
    // floating refs) but never a malformed ref, URL, or revision.
    let clean = source_policy.allow_development_sources || !source.dirty;
    let pinned_ref =
        source_policy.allow_development_sources || !floating_source_ref(&source.reference);
    valid_source_url(&source.repository)
        && valid_source_ref_syntax(&source.reference)
        && pinned_ref
        && valid_revision(&source.revision)
        && clean
        && source.submodules.len() <= 512
        && source.submodules.iter().all(|submodule| {
            safe_submodule_path(&submodule.path)
                && valid_revision(&submodule.revision)
                && (source_policy.allow_development_sources || !submodule.dirty)
        })
        && source
            .submodules
            .iter()
            .map(|submodule| submodule.path.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            == source.submodules.len()
}

fn valid_client_source(source: &ClientSource) -> bool {
    valid_source_url(&source.repository)
        && valid_revision(&source.revision)
        && is_lower_hex(&source.lockfile_sha256, 64)
}

fn valid_manifest_capabilities(capabilities: &[String]) -> bool {
    let values = capabilities
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    capabilities.len() >= CORE_EXECUTOR_FEATURES.len()
        && capabilities.len() <= EXECUTOR_FEATURES.len()
        && values.len() == capabilities.len()
        && values
            .iter()
            .all(|feature| EXECUTOR_FEATURES.contains(feature))
        && CORE_EXECUTOR_FEATURES
            .iter()
            .all(|feature| values.contains(feature))
        && values.contains("push") == values.contains("secret_scan_supported_formats_v1")
        && (!values.contains("pull_request") || values.contains("push"))
}

fn valid_source_url(value: &str) -> bool {
    if value.is_empty() || value.len() > 2048 || value.chars().any(char::is_control) {
        return false;
    }
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    if url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return false;
    }
    match url.scheme() {
        "https" => url.host_str().is_some() && url.path().len() > 1 && url.username().is_empty(),
        "ssh" => {
            url.host_str().is_some()
                && url.path().len() > 1
                && (url.username().is_empty() || url.username() == "git")
        }
        _ => false,
    }
}

/// A ref that does not pin a release: branch heads and symbolic names.
fn floating_source_ref(value: &str) -> bool {
    matches!(value, "main" | "master" | "HEAD" | "latest")
        || ["main", "master", "HEAD", "latest"]
            .iter()
            .any(|suffix| value.ends_with(&format!("/{suffix}")))
}

fn valid_source_ref_syntax(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/-".contains(&byte))
        && !value.contains("//")
        && !value
            .split('/')
            .any(|component| component == "." || component == "..")
}

fn valid_revision(value: &str) -> bool {
    is_lower_hex(value, 40) || is_lower_hex(value, 64)
}

fn valid_semver(value: &str) -> bool {
    let separator = value.find(['-', '+']);
    let (base, suffix) = match separator {
        Some(index) => (&value[..index], Some(&value[index + 1..])),
        None => (value, None),
    };
    let mut parts = base.split('.');
    let base_valid = (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    }) && parts.next().is_none();
    base_valid
        && value.len() <= 128
        && suffix.is_none_or(|suffix| {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b".-".contains(&byte))
        })
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".+-".contains(&byte))
}

fn valid_repository_id(repository: &RepositoryId) -> bool {
    if repository.host.is_empty()
        || repository.host.len() > 255
        || repository.host.contains('/')
        || repository.host.contains('@')
        || repository.host.chars().any(char::is_control)
        || !safe_repository_segment(&repository.owner)
        || !safe_repository_segment(&repository.name)
    {
        return false;
    }
    let Ok(host) = url::Url::parse(&format!("https://{}", repository.host)) else {
        return false;
    };
    host.host_str().is_some()
        && host.path() == "/"
        && (!repository.host.contains(':') || host.port().is_some())
        && url_authority(&host) == repository.host
}

fn safe_repository_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        && !value.contains(' ')
}

fn valid_clone_url(value: &str) -> bool {
    if value.len() > 2048 || value.chars().any(char::is_control) {
        return false;
    }
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    if (!url.username().is_empty()
        && !(url.scheme() == "ssh"
            && url
                .username()
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))))
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    match url.scheme() {
        "ssh" => url.host_str().is_some_and(|host| !host.starts_with('-')) && url.path() != "/",
        "https" => url.host_str().is_some(),
        "http" => url.host_str().is_some_and(is_loopback_host),
        "file" => url.host_str().is_none() || url.host_str() == Some("localhost"),
        _ => false,
    }
}

fn clone_url_matches_repository(value: &str, repository: &RepositoryId) -> bool {
    if !valid_clone_url(value) {
        return false;
    }
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    if url.scheme() == "file" {
        return true;
    }
    let expected_path = format!("/{}/{}", repository.owner, repository.name);
    (url.scheme() == "ssh" || url_authority(&url) == repository.host)
        && (url.path() == expected_path || url.path() == format!("{expected_path}.git"))
}

fn url_authority(url: &url::Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch.len() <= 255
        && !branch.starts_with('/')
        && !branch.ends_with('/')
        && !branch.starts_with('-')
        && !branch.contains("//")
        && !branch.contains("..")
        && !branch.contains("@{")
        && branch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/_-.".contains(&byte))
        && branch.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && !part.ends_with('.')
                && !part.ends_with(".lock")
        })
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn safe_artifact_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.starts_with('/')
        && !value.contains('\\')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/-".contains(&byte))
}

fn safe_submodule_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.starts_with('/')
        && !value.contains('\\')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/ -".contains(&byte))
}

fn safe_relative_path(path: &Path) -> bool {
    path.is_relative()
        && !path.as_os_str().is_empty()
        && path.components().all(|component| {
            matches!(component, Component::Normal(part) if part.to_string_lossy().bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)))
        })
}

fn safe_absolute_child(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).ok().is_some_and(safe_relative_path)
}

fn supported_target_triple() -> Option<&'static str> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    {
        Some("x86_64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    {
        Some("aarch64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "musl"))]
    {
        Some("x86_64-unknown-linux-musl")
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "musl"))]
    {
        Some("aarch64-unknown-linux-musl")
    }
    #[cfg(not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64"),
        any(target_env = "gnu", target_env = "musl")
    )))]
    {
        None
    }
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn open_nofollow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    options.open(path)
}

fn validate_private_directory(path: &Path, strict_mode: bool) -> Result<(), ()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ())?;
    validate_private_directory_metadata(&metadata)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if strict_mode && metadata.mode() & 0o7777 != 0o700 {
            return Err(());
        }
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let component_metadata = fs::symlink_metadata(&current).map_err(|_| ())?;
        if component_metadata.file_type().is_symlink() {
            return Err(());
        }
    }
    if !is_canonical(path) {
        return Err(());
    }
    Ok(())
}

fn validate_private_directory_metadata(metadata: &fs::Metadata) -> Result<(), ()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o7777 != 0o700 {
            return Err(());
        }
    }
    #[cfg(not(unix))]
    return Err(());
    Ok(())
}

fn validate_private_file(metadata: &fs::Metadata, mode: u32) -> Result<(), ()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o7777 != mode {
            return Err(());
        }
    }
    #[cfg(not(unix))]
    return Err(());
    Ok(())
}

fn validate_non_writable_directory(path: &Path) -> Result<(), NativeBootstrapUnavailable> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| NativeBootstrapUnavailable::UnsafeTrustedPolicy)?;
    validate_non_writable_directory_metadata(&metadata)
        .map_err(|_| NativeBootstrapUnavailable::UnsafeTrustedPolicy)?;
    if !is_canonical(path) {
        return Err(NativeBootstrapUnavailable::UnsafeTrustedPolicy);
    }
    Ok(())
}

fn validate_non_writable_directory_metadata(metadata: &fs::Metadata) -> Result<(), ()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let sticky = metadata.mode() & 0o1000 != 0;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o700 != 0o700
            || (metadata.mode() & 0o022 != 0 && !sticky)
        {
            return Err(());
        }
    }
    #[cfg(not(unix))]
    return Err(());
    Ok(())
}

fn validate_package_directory_metadata(metadata: &fs::Metadata) -> Result<(), ()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (metadata.uid() != unsafe { libc::geteuid() } && metadata.uid() != 0)
            || metadata.mode() & 0o7022 != 0
        {
            return Err(());
        }
    }
    #[cfg(not(unix))]
    return Err(());
    Ok(())
}

fn validate_package_file_metadata(metadata: &fs::Metadata, executable: bool) -> io::Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (metadata.uid() != unsafe { libc::geteuid() } && metadata.uid() != 0)
            || metadata.mode() & 0o7022 != 0
            || (executable && metadata.mode() & 0o111 == 0)
        {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "file"));
        }
    }
    #[cfg(not(unix))]
    return Err(io::Error::new(io::ErrorKind::Unsupported, "platform"));
    Ok(())
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(path)?;
    }
    #[cfg(not(unix))]
    return Err(io::Error::new(io::ErrorKind::Unsupported, "platform"));
    validate_private_directory(path, true)
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "directory"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ArtifactManifest {
    schema_version: u32,
    profile: ProfileIdentity,
    target: TargetIdentity,
    sources: PackageSources,
    executor: ExecutorIdentity,
    runtime_compatibility: RuntimeCompatibility,
    artifacts: PackageArtifacts,
}

impl ArtifactManifest {
    fn package_identity(&self) -> PackageIdentity {
        PackageIdentity {
            profile: self.profile.clone(),
            target: self.target.clone(),
            sources: self.sources.clone(),
            executor: self.executor.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PackageIdentity {
    profile: ProfileIdentity,
    target: TargetIdentity,
    sources: PackageSources,
    executor: ExecutorIdentity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfileIdentity {
    name: String,
    version: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct TargetIdentity {
    triple: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PackageSources {
    agent_app: RuntimeSource,
    jobworkerp_runtime: RuntimeSource,
    jobworkerp_client: ClientSource,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimeSource {
    repository: String,
    #[serde(rename = "ref")]
    reference: String,
    revision: String,
    dirty: bool,
    submodules: Vec<SubmoduleIdentity>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SubmoduleIdentity {
    path: String,
    revision: String,
    dirty: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ClientSource {
    repository: String,
    revision: String,
    lockfile_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ExecutorIdentity {
    version: String,
    protocol_version: u32,
    capabilities: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct RuntimeCompatibility {
    protocol_version: serde_json::Value,
    config_version: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct PackageArtifacts {
    runtime: ArtifactFile,
    executor: ArtifactFile,
    configuration: Vec<ArtifactFile>,
}

impl PackageArtifacts {
    fn all(&self) -> Vec<&ArtifactFile> {
        let mut artifacts = vec![&self.runtime, &self.executor];
        artifacts.extend(self.configuration.iter());
        artifacts
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ArtifactFile {
    path: String,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustedHostPolicy {
    version: u32,
    max_import_bytes: u64,
    expected_package_identity: PackageIdentity,
    repositories: Vec<HostRepository>,
    credential_sources: Vec<HostCredentialSource>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostRepository {
    id: RepositoryId,
    clone_url: String,
    allowed_base_branches: Vec<String>,
    read_credential_source: Option<String>,
    #[serde(default)]
    publish_credential_source: Option<String>,
    #[serde(default)]
    forge: Option<coding_agent_executor::ForgeConfig>,
    #[serde(default)]
    ssh_directory: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostCredentialSource {
    id: String,
    username: String,
    /// Relative to the fixed `<native>/credential-sources` directory.
    token_file: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    #[cfg(target_os = "linux")]
    struct TokenAccessWatcher {
        fd: i32,
        _watch_descriptor: i32,
    }

    #[cfg(target_os = "linux")]
    impl TokenAccessWatcher {
        fn new(path: &Path) -> Self {
            let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
            assert!(fd >= 0, "inotify initialization failed");
            let path = CString::new(path.as_os_str().as_bytes()).unwrap();
            let watch_descriptor = unsafe {
                libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_OPEN | libc::IN_ACCESS)
            };
            assert!(watch_descriptor >= 0, "inotify watch setup failed");
            Self {
                fd,
                _watch_descriptor: watch_descriptor,
            }
        }

        fn access_events(&self) -> usize {
            let mut buffer = [0_u8; 4096];
            let mut events = 0;
            loop {
                let count =
                    unsafe { libc::read(self.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
                if count <= 0 {
                    break;
                }
                let mut offset = 0;
                let count = count as usize;
                while offset + 16 <= count {
                    let mask =
                        u32::from_ne_bytes(buffer[offset + 4..offset + 8].try_into().unwrap());
                    let name_length =
                        u32::from_ne_bytes(buffer[offset + 12..offset + 16].try_into().unwrap())
                            as usize;
                    if mask & (libc::IN_OPEN | libc::IN_ACCESS) != 0 {
                        events += 1;
                    }
                    offset += 16 + name_length;
                }
            }
            events
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for TokenAccessWatcher {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.fd);
            }
        }
    }

    fn private_temp_dir() -> tempfile::TempDir {
        for base in [
            Path::new("/dev"),
            Path::new(env!("CARGO_MANIFEST_DIR")),
            Path::new("/tmp"),
        ] {
            if validate_trusted_path_ancestors(base).is_err() {
                continue;
            }
            if let Ok(directory) = tempfile::Builder::new()
                .prefix("coding-agent-")
                .tempdir_in(base)
            {
                fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
                return directory;
            }
        }
        panic!("no writable private test directory is available");
    }

    #[test]
    fn partial_temporary_write_is_not_published_and_retry_succeeds() {
        let directory = private_temp_dir();
        let destination = directory.path().join("credential-snapshot.token");
        let expected = b"retryable private credential";

        let failed = write_immutable_credential_with(&destination, expected, |file, contents| {
            file.write_all(&contents[..8])?;
            Err(io::Error::other(
                "injected write failure after partial data",
            ))
        });

        assert_eq!(
            failed,
            Err(NativeBootstrapUnavailable::CredentialUnavailable)
        );
        assert!(!destination.exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);

        write_immutable_credential(&destination, expected).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn adopting_a_link_from_an_unsynced_publisher_requires_parent_sync() {
        let directory = private_temp_dir();
        let source = directory.path().join("publisher-a.token");
        let destination = directory.path().join("credential-snapshot.token");
        let expected = b"one immutable snapshot";
        let mut file = create_private_new_file(&source).unwrap();
        file.write_all(expected).unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::hard_link(&source, &destination).unwrap();
        // Publisher A has linked the final name but has not synced its parent.
        let mut sync_calls = 0;
        let result = write_immutable_credential_with_sync(
            &destination,
            expected,
            |file, bytes| file.write_all(bytes),
            |parent| {
                assert_eq!(parent, directory.path());
                sync_calls += 1;
                Err(NativeBootstrapUnavailable::CredentialUnavailable)
            },
        );
        assert_eq!(sync_calls, 1);
        assert_eq!(
            result,
            Err(NativeBootstrapUnavailable::CredentialUnavailable)
        );
        assert_eq!(fs::read(&destination).unwrap(), expected);
        write_immutable_credential(&destination, expected).unwrap();
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn concurrent_credential_snapshot_publishers_reconcile_the_same_bytes() {
        let directory = private_temp_dir();
        let destination = directory.path().join("credential-snapshot.token");
        let expected = b"one immutable snapshot".to_vec();
        let publishers = (0..2)
            .map(|_| {
                let destination = destination.clone();
                let expected = expected.clone();
                std::thread::spawn(move || write_immutable_credential(&destination, &expected))
            })
            .collect::<Vec<_>>();

        for publisher in publishers {
            publisher.join().unwrap().unwrap();
        }

        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn credential_branches_are_independent_and_shared_source_is_snapshotted_once() {
        let root = private_temp_dir();
        let credential_root = root.path().join("credentials");
        create_private_directory(&credential_root).unwrap();
        let source_root = root.path().join("credential-sources");
        create_private_directory(&source_root).unwrap();
        let shared_path = source_root.join("shared.token");
        fs::write(&shared_path, b"shared-token").unwrap();
        fs::set_permissions(&shared_path, fs::Permissions::from_mode(0o600)).unwrap();
        let repository = test_host_repository(Some("shared"), Some("missing"), true);
        let policy = test_policy(
            repository.clone(),
            vec![
                HostCredentialSource {
                    id: "shared".into(),
                    username: "user".into(),
                    token_file: "shared.token".into(),
                },
                HostCredentialSource {
                    id: "missing".into(),
                    username: "user".into(),
                    token_file: "missing.token".into(),
                },
            ],
        );

        let plan =
            build_repository_check_credentials(&policy, &repository, root.path(), &credential_root);

        assert!(plan.git_read);
        assert!(!plan.forge_api);
        assert_eq!(plan.credential_sources.len(), 1);
        assert_eq!(plan.credential_sources[0].id, "shared");
        assert_eq!(
            plan.forge_stage.as_ref().map(|stage| (
                stage.stage.as_str(),
                stage.status.as_str(),
                stage.code.as_str()
            )),
            Some(("forge_api", "failed", "credential_unavailable"))
        );
        assert!(plan.git_stage.is_none());
    }

    #[test]
    fn anonymous_git_remains_enabled_when_only_publication_credential_is_missing() {
        let root = private_temp_dir();
        let credential_root = root.path().join("credentials");
        create_private_directory(&credential_root).unwrap();
        let source_root = root.path().join("credential-sources");
        create_private_directory(&source_root).unwrap();
        let repository = test_host_repository(None, Some("missing"), true);
        let policy = test_policy(
            repository.clone(),
            vec![HostCredentialSource {
                id: "missing".into(),
                username: "user".into(),
                token_file: "missing.token".into(),
            }],
        );

        let plan =
            build_repository_check_credentials(&policy, &repository, root.path(), &credential_root);

        assert!(plan.git_read);
        assert!(!plan.forge_api);
        assert!(plan.git_stage.is_none());
        assert_eq!(
            plan.forge_stage.as_ref().map(|stage| stage.code.as_str()),
            Some("credential_unavailable")
        );
    }

    #[test]
    fn shared_snapshot_failure_disables_both_diagnostic_branches() {
        let root = private_temp_dir();
        let credential_root = root.path().join("credentials");
        create_private_directory(&credential_root).unwrap();
        let source_root = root.path().join("credential-sources");
        create_private_directory(&source_root).unwrap();
        let repository = test_host_repository(Some("shared"), Some("shared"), true);
        let policy = test_policy(
            repository.clone(),
            vec![HostCredentialSource {
                id: "shared".into(),
                username: "user".into(),
                token_file: "missing.token".into(),
            }],
        );

        let plan =
            build_repository_check_credentials(&policy, &repository, root.path(), &credential_root);

        assert!(!plan.git_read);
        assert!(!plan.forge_api);
        assert!(plan.credential_sources.is_empty());
        assert_eq!(
            plan.git_stage.as_ref().unwrap().code,
            "credential_unavailable"
        );
        assert_eq!(
            plan.forge_stage.as_ref().unwrap().code,
            "credential_unavailable"
        );
    }

    #[test]
    fn executor_timeout_preserves_local_git_failure_and_reports_forge_timeout() {
        let repository = RepositoryId {
            host: "example.test".into(),
            owner: "owner".into(),
            name: "repo".into(),
        };
        let plan = RepositoryCheckCredentialPlan {
            credential_sources: Vec::new(),
            git_read: false,
            forge_api: true,
            git_stage: Some(diagnostic_stage(
                "git_read",
                "failed",
                "credential_unavailable",
            )),
            forge_stage: None,
        };

        let result = repository_check_result_for_executor_error(
            &repository,
            &"a".repeat(64),
            &plan,
            &NativeExecutorError::DiagnosticTransport {
                reason: NativeDiagnosticTransportReason::Timeout,
            },
        );

        assert_eq!(result.stages[0].code, "credential_unavailable");
        assert_eq!(result.stages[1].code, "timeout");
    }

    #[test]
    fn executor_failure_preserves_local_forge_skip_and_reports_git_failure() {
        let repository = RepositoryId {
            host: "example.test".into(),
            owner: "owner".into(),
            name: "repo".into(),
        };
        let plan = RepositoryCheckCredentialPlan {
            credential_sources: Vec::new(),
            git_read: true,
            forge_api: false,
            git_stage: None,
            forge_stage: Some(diagnostic_stage(
                "forge_api",
                "skipped",
                "credential_not_configured",
            )),
        };

        let result = repository_check_result_for_executor_error(
            &repository,
            &"b".repeat(64),
            &plan,
            &NativeExecutorError::DiagnosticTransport {
                reason: NativeDiagnosticTransportReason::ResponseTooLarge,
            },
        );

        assert_eq!(result.stages[0].code, "response_too_large");
        assert_eq!(result.stages[1].code, "credential_not_configured");
    }

    #[test]
    fn non_transport_executor_failure_keeps_local_stages_for_enabled_and_disabled_branches() {
        let repository = RepositoryId {
            host: "example.test".into(),
            owner: "owner".into(),
            name: "repo".into(),
        };
        let plan = RepositoryCheckCredentialPlan {
            credential_sources: Vec::new(),
            git_read: false,
            forge_api: true,
            git_stage: Some(diagnostic_stage(
                "git_read",
                "failed",
                "credential_unavailable",
            )),
            forge_stage: None,
        };

        let result = repository_check_result_for_executor_error(
            &repository,
            &"c".repeat(64),
            &plan,
            &NativeExecutorError::IncompatibleProtocol,
        );

        assert_eq!(result.stages[0].code, "credential_unavailable");
        assert_eq!(result.stages[1].code, "unavailable");
    }

    fn test_host_repository(
        read_credential_source: Option<&str>,
        publish_credential_source: Option<&str>,
        with_forge: bool,
    ) -> HostRepository {
        HostRepository {
            id: RepositoryId {
                host: "example.test".into(),
                owner: "owner".into(),
                name: "repo".into(),
            },
            clone_url: "https://example.test/owner/repo.git".into(),
            allowed_base_branches: vec!["main".into()],
            read_credential_source: read_credential_source.map(str::to_owned),
            publish_credential_source: publish_credential_source.map(str::to_owned),
            forge: with_forge.then(|| coding_agent_executor::ForgeConfig {
                kind: coding_agent_executor::ForgeKind::GitHub,
                api_base_url: "https://example.test/api".into(),
                web_base_url: "https://example.test".into(),
            }),
            ssh_directory: None,
        }
    }

    fn test_policy(
        repository: HostRepository,
        credential_sources: Vec<HostCredentialSource>,
    ) -> TrustedHostPolicy {
        TrustedHostPolicy {
            version: 1,
            max_import_bytes: 1024,
            expected_package_identity: serde_json::from_value(serde_json::json!({
                "profile": {"name": "linux-kvm", "version": 1},
                "target": {"triple": "x86_64-unknown-linux-gnu"},
                "sources": {
                    "agent_app": {
                        "repository": "https://example.test/agent.git",
                        "ref": "release/v1",
                        "revision": "a".repeat(40),
                        "dirty": false,
                        "submodules": []
                    },
                    "jobworkerp_runtime": {
                        "repository": "https://example.test/runtime.git",
                        "ref": "release/v1",
                        "revision": "b".repeat(40),
                        "dirty": false,
                        "submodules": []
                    },
                    "jobworkerp_client": {
                        "repository": "https://example.test/client.git",
                        "revision": "c".repeat(40),
                        "lockfile_sha256": "d".repeat(64)
                    }
                },
                "executor": {
                    "version": EXECUTOR_VERSION,
                    "protocol_version": PROTOCOL_VERSION,
                    "capabilities": ["prepare"]
                }
            }))
            .unwrap(),
            repositories: vec![repository],
            credential_sources,
        }
    }

    #[cfg(target_os = "linux")]
    fn runtime_test_selection(runtime_bytes: &[u8]) -> (tempfile::TempDir, NativePackageSelection) {
        runtime_test_selection_with_executor(
            runtime_bytes,
            b"test executor artifact",
            &CORE_EXECUTOR_FEATURES,
        )
    }

    #[cfg(target_os = "linux")]
    fn runtime_test_selection_with_executor(
        runtime_bytes: &[u8],
        executor_bytes: &[u8],
        capabilities: &[&str],
    ) -> (tempfile::TempDir, NativePackageSelection) {
        let (directory, resources) =
            write_test_package(runtime_bytes, executor_bytes, capabilities, |_| {});
        let selection = NativePackageSelection::from_app_resources(&resources).unwrap();
        (directory, selection)
    }

    /// Write a complete package tree and return the app resource directory it
    /// belongs to. `mutate` edits the manifest before it is written.
    #[cfg(target_os = "linux")]
    fn write_test_package(
        runtime_bytes: &[u8],
        executor_bytes: &[u8],
        capabilities: &[&str],
        mutate: impl FnOnce(&mut serde_json::Value),
    ) -> (tempfile::TempDir, PathBuf) {
        let directory = private_temp_dir();
        let resources = directory.path().join("resources");
        let package_root = resources.join(PACKAGE_DIRECTORY);
        fs::create_dir_all(package_root.join("bin")).unwrap();
        fs::create_dir_all(package_root.join("runtime")).unwrap();
        for artifact in CONFIGURATION_ARTIFACTS {
            let path = package_root.join(artifact);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
        }

        let runtime_path = package_root.join("runtime/all-in-one");
        fs::write(&runtime_path, runtime_bytes).unwrap();
        fs::set_permissions(&runtime_path, fs::Permissions::from_mode(0o700)).unwrap();
        let executor_path = package_root.join(EXECUTOR_RELATIVE_PATH);
        fs::write(&executor_path, executor_bytes).unwrap();
        fs::set_permissions(&executor_path, fs::Permissions::from_mode(0o700)).unwrap();

        let configurations = CONFIGURATION_ARTIFACTS
            .iter()
            .map(|artifact| {
                let path = package_root.join(artifact);
                fs::write(&path, b"{}").unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                serde_json::json!({ "path": artifact, "sha256": sha256(b"{}") })
            })
            .collect::<Vec<_>>();
        let mut manifest = serde_json::json!({
            "schema_version": 1,
            "profile": { "name": "linux-kvm", "version": 1 },
            "target": { "triple": supported_target_triple().unwrap() },
            "sources": {
                "agent_app": {
                    "repository": "https://github.com/lookback/agent-app.git",
                    "ref": "release/v1.0.0",
                    "revision": "a".repeat(40),
                    "dirty": false,
                    "submodules": []
                },
                "jobworkerp_runtime": {
                    "repository": "https://github.com/jobworkerp-rs/jobworkerp-rs.git",
                    "ref": "release/v1.0.0",
                    "revision": "b".repeat(40),
                    "dirty": false,
                    "submodules": []
                },
                "jobworkerp_client": {
                    "repository": "https://github.com/jobworkerp-rs/jobworkerp-client-rs.git",
                    "revision": "c".repeat(40),
                    "lockfile_sha256": "d".repeat(64)
                }
            },
            "executor": {
                "version": EXECUTOR_VERSION,
                "protocol_version": PROTOCOL_VERSION,
                "capabilities": capabilities
            },
            "runtime_compatibility": {
                "protocol_version": null,
                "config_version": null
            },
            "artifacts": {
                "runtime": {
                    "path": "runtime/all-in-one",
                    "sha256": sha256(runtime_bytes)
                },
                "executor": {
                    "path": EXECUTOR_RELATIVE_PATH,
                    "sha256": sha256(executor_bytes)
                },
                "configuration": configurations
            }
        });
        mutate(&mut manifest);
        let manifest_path = package_root.join(PACKAGE_MANIFEST);
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600)).unwrap();
        (directory, resources)
    }

    /// Provenance as recorded by development staging: a dirty app tree on a
    /// branch head and a runtime checkout reached over SSH.
    #[cfg(target_os = "linux")]
    fn development_manifest(manifest: &mut serde_json::Value) {
        manifest["sources"]["agent_app"]["dirty"] = serde_json::json!(true);
        manifest["sources"]["agent_app"]["ref"] = serde_json::json!("main");
        manifest["sources"]["jobworkerp_runtime"]["repository"] =
            serde_json::json!("ssh://git@gitea.example/team/runtime.git");
        manifest["sources"]["jobworkerp_runtime"]["ref"] = serde_json::json!("HEAD");
    }

    #[test]
    fn build_source_policy_follows_debug_assertions() {
        assert_eq!(
            PackageSourcePolicy::for_build().allow_development_sources,
            cfg!(debug_assertions)
        );
        assert!(!PackageSourcePolicy::strict().allow_development_sources);
        assert!(PackageSourcePolicy::development().allow_development_sources);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn strict_source_policy_rejects_working_tree_provenance() {
        let (_directory, resources) = write_test_package(
            b"fixture runtime",
            b"fixture executor",
            &CORE_EXECUTOR_FEATURES,
            development_manifest,
        );
        assert_eq!(
            NativePackageSelection::from_app_resources_with_policy(
                &resources,
                PackageSourcePolicy::strict()
            )
            .unwrap_err(),
            NativeBootstrapUnavailable::PackageUnavailable
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn development_source_policy_admits_working_tree_provenance_and_labels_it() {
        let (directory, resources) = write_test_package(
            b"fixture runtime",
            b"fixture executor",
            &CORE_EXECUTOR_FEATURES,
            development_manifest,
        );
        let package = NativePackageSelection::from_app_resources_with_policy(
            &resources,
            PackageSourcePolicy::development(),
        )
        .unwrap();
        assert!(package.development_sources());
        assert_eq!(package.source_policy(), PackageSourcePolicy::development());

        // The virtual baseline embeds the same identity, so the relaxed policy
        // must also reach the repository policy read.
        let data_root = directory.path().join("app-data");
        create_private_directory(&data_root).unwrap();
        let snapshot = read_repository_policy(&DataPaths::with_root(&data_root), &package)
            .expect("development package must provide its virtual baseline");
        assert!(snapshot.repositories.is_empty());

        let (_release_directory, release) = runtime_test_selection(b"fixture runtime");
        assert!(!release.development_sources());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn development_source_policy_still_rejects_malformed_sources() {
        let malformed: [fn(&mut serde_json::Value); 3] = [
            |manifest| {
                manifest["sources"]["agent_app"]["repository"] =
                    serde_json::json!("git@gitea.example:team/app.git");
            },
            |manifest| {
                manifest["sources"]["agent_app"]["revision"] = serde_json::json!("not-a-sha");
            },
            |manifest| {
                manifest["sources"]["jobworkerp_runtime"]["ref"] = serde_json::json!("../main");
            },
        ];
        for mutate in malformed {
            let (_directory, resources) = write_test_package(
                b"fixture runtime",
                b"fixture executor",
                &CORE_EXECUTOR_FEATURES,
                |manifest| {
                    development_manifest(manifest);
                    mutate(manifest);
                },
            );
            assert_eq!(
                NativePackageSelection::from_app_resources_with_policy(
                    &resources,
                    PackageSourcePolicy::development()
                )
                .unwrap_err(),
                NativeBootstrapUnavailable::PackageUnavailable
            );
        }
    }

    #[cfg(target_os = "linux")]
    fn diagnostic_executor_script(
        capabilities: &[&str],
        marker: &Path,
        response: bool,
        hold_operation: bool,
    ) -> Vec<u8> {
        let capability_document = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "executor_version": EXECUTOR_VERSION,
            "features": capabilities,
        })
        .to_string();
        let response_template = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "status": "ok",
            "result": {
                "repository_checked": {
                    "repository": {
                        "host": "example.test",
                        "owner": "owner",
                        "name": "repo"
                    },
                    "trusted_config_digest": "__DIGEST__",
                    "protocol_version": PROTOCOL_VERSION,
                    "executor_version": EXECUTOR_VERSION,
                    "stages": [
                        {"stage": "git_read", "status": "passed", "code": "git_readable"},
                        {"stage": "forge_api", "status": "skipped", "code": "prerequisite_failed"}
                    ]
                }
            }
        })
        .to_string()
        .replace("__DIGEST__", "%s");
        let marker = marker.to_string_lossy().replace('\'', "'\\''");
        let operation = if response {
            let hold = if hold_operation { "sleep 1\n" } else { "" };
            format!(
                "{hold}digest=$(sha256sum \"$2\" | cut -d ' ' -f1)\nprintf '%s\\n' \"$(printf '{response_template}' \"$digest\")\""
            )
        } else {
            "printf invalid-response-marker\\n".into()
        };
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--capabilities\" ]; then printf '%s\\n' '{capability_document}'; exit 0; fi\nprintf invoked >> '{marker}'\n{operation}\n"
        )
        .into_bytes()
    }

    #[cfg(target_os = "linux")]
    fn install_test_policy(
        data_paths: &DataPaths,
        selection: &NativePackageSelection,
        read_credential: Option<&str>,
        token_file: Option<&str>,
    ) -> PathBuf {
        let host_root = data_paths.root.join("coding-agent/native");
        ensure_private_test_directory(&data_paths.root.join("coding-agent"));
        ensure_private_test_directory(&host_root);
        let source_root = host_root.join("credential-sources");
        ensure_private_test_directory(&source_root);
        if let Some(token_file) = token_file {
            let token_path = source_root.join(token_file);
            fs::write(&token_path, b"diagnostic-token\n").unwrap();
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(selection.package_root.join(PACKAGE_MANIFEST)).unwrap(),
        )
        .unwrap();
        let credential_sources = read_credential
            .zip(token_file)
            .map(|(id, token_file)| {
                serde_json::json!([{
                    "id": id,
                    "username": "user",
                    "token_file": token_file,
                }])
            })
            .unwrap_or_else(|| serde_json::json!([]));
        let policy = serde_json::json!({
            "version": 1,
            "max_import_bytes": 1024,
            "expected_package_identity": {
                "profile": manifest["profile"].clone(),
                "target": manifest["target"].clone(),
                "sources": manifest["sources"].clone(),
                "executor": manifest["executor"].clone(),
            },
            "repositories": [{
                "id": {"host": "example.test", "owner": "owner", "name": "repo"},
                "clone_url": "https://example.test/owner/repo.git",
                "allowed_base_branches": ["main"],
                "read_credential_source": read_credential,
                "publish_credential_source": null,
                "forge": null,
                "ssh_directory": null,
            }],
            "credential_sources": credential_sources,
        });
        let policy_path = data_paths.root.join(TRUSTED_HOST_POLICY_RELATIVE_PATH);
        fs::write(&policy_path, serde_json::to_vec(&policy).unwrap()).unwrap();
        fs::set_permissions(&policy_path, fs::Permissions::from_mode(0o600)).unwrap();
        policy_path
    }

    #[cfg(target_os = "linux")]
    fn ensure_private_test_directory(path: &Path) {
        if path.is_dir() {
            validate_private_directory(path, true).unwrap();
        } else {
            create_private_directory(path).unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    fn current_process_snapshot() -> ManagedJobworkerpSpawnSnapshot {
        ManagedJobworkerpSpawnSnapshot {
            generation_id: 1,
            child_pid: std::process::id(),
            configured_binary_path: PathBuf::from("/ignored/configured/path"),
            actual_running_executable: Some(PathBuf::from("/ignored/reported/path")),
            applied_sdk_home: None,
            applied_sdk_config_path: None,
            child_home: None,
            child_home_source: crate::sidecar::ManagedChildHomeSource::Removed,
            effective_sdk_home: None,
            runtime_observation: crate::sidecar::ManagedJobworkerpRuntimeObservation::NotObserved,
            sdk_config_source: crate::sidecar::ManagedJobworkerpSdkSourceStamp::Unavailable {
                reason: crate::sidecar::ManagedJobworkerpSdkSourceUnavailableReason::SelectedConfigPathNotApplied,
            },
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn package_accepts_dynamic_workflows_without_yaml_but_rejects_duplicate_assets() {
        let (_directory, selection) = runtime_test_selection(b"fixture runtime");
        let manifest_path = selection.package_root.join(PACKAGE_MANIFEST);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["artifacts"]["configuration"] = serde_json::json!([]);
        manifest["executor"]["capabilities"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("inspect_source"));
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(
            NativePackageSelection::from_app_resources(selection.package_root.parent().unwrap())
                .is_ok()
        );
        let (_directory, selection) = runtime_test_selection(b"fixture runtime");
        let manifest_path = selection.package_root.join(PACKAGE_MANIFEST);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        let duplicated = manifest["artifacts"]["configuration"][0].clone();
        manifest["artifacts"]["configuration"]
            .as_array_mut()
            .unwrap()
            .push(duplicated);
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(
            NativePackageSelection::from_app_resources(selection.package_root.parent().unwrap())
                .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_host_policy_uses_a_safe_virtual_package_baseline() {
        let (directory, package) = runtime_test_selection(b"fixture runtime");
        let data_root = directory.path().join("app-data");
        create_private_directory(&data_root).unwrap();
        let data_paths = DataPaths::with_root(&data_root);

        let snapshot = read_repository_policy(&data_paths, &package)
            .expect("a verified package can provide the initial empty policy");

        assert!(snapshot.repositories.is_empty());
        assert_eq!(snapshot.policy_sha256.len(), 64);
        assert!(
            !data_root
                .join(TRUSTED_HOST_POLICY_RELATIVE_PATH)
                .try_exists()
                .unwrap(),
            "the GUI baseline must not materialize a hand-editable policy file"
        );
        assert!(data_root.join("coding-agent/native").is_dir());
    }

    #[cfg(target_os = "linux")]
    fn diagnostic_fixture() -> (tempfile::TempDir, NativePackageSelection, DataPaths) {
        let (directory, package) = runtime_test_selection(b"fixture runtime");
        let data_root = directory.path().join("app-data");
        create_private_directory(&data_root).unwrap();
        let data_paths = DataPaths::with_root(&data_root);
        (directory, package, data_paths)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn diagnostic_policy_read_skips_artifact_rehash_but_gating_read_rejects_changed_bytes() {
        let (_directory, package, data_paths) = diagnostic_fixture();
        let before = read_repository_policy(&data_paths, &package).unwrap();
        // Same length, different bytes: only a content hash can notice this.
        fs::write(
            package.package_root.join("runtime/all-in-one"),
            b"FIXTURE RUNTIME",
        )
        .unwrap();

        assert_eq!(
            read_repository_policy(&data_paths, &package).map(|snapshot| snapshot.policy_sha256),
            Err(NativeBootstrapUnavailable::PackageUnavailable)
        );
        let diagnostic = read_repository_policy_for_diagnostic(&data_paths, &package)
            .expect("the display path must not re-hash package artifacts");
        assert_eq!(diagnostic.policy_sha256, before.policy_sha256);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn diagnostic_policy_read_still_reports_a_replaced_manifest() {
        let (_directory, package, data_paths) = diagnostic_fixture();
        let manifest_path = package.package_root.join(PACKAGE_MANIFEST);
        let mut manifest = fs::read(&manifest_path).unwrap();
        manifest.push(b'\n');
        fs::write(&manifest_path, manifest).unwrap();

        assert_eq!(
            read_repository_policy_for_diagnostic(&data_paths, &package)
                .map(|snapshot| snapshot.policy_sha256),
            Err(NativeBootstrapUnavailable::PackageUnavailable)
        );
        assert_eq!(
            read_repository_policy(&data_paths, &package).map(|snapshot| snapshot.policy_sha256),
            Err(NativeBootstrapUnavailable::PackageUnavailable)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn diagnostic_policy_read_reports_a_removed_package() {
        let (_directory, package, data_paths) = diagnostic_fixture();
        fs::remove_dir_all(&package.package_root).unwrap();

        assert!(read_repository_policy_for_diagnostic(&data_paths, &package).is_err());
        assert!(read_repository_policy(&data_paths, &package).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn policy_digest_changes_when_the_authority_source_changes_from_virtual_to_file() {
        let (directory, package) = runtime_test_selection(b"fixture runtime");
        let data_root = directory.path().join("app-data");
        create_private_directory(&data_root).unwrap();
        let data_paths = DataPaths::with_root(&data_root);

        let virtual_snapshot = read_repository_policy(&data_paths, &package).unwrap();
        let policy_path = data_root.join(TRUSTED_HOST_POLICY_RELATIVE_PATH);
        install_test_policy(&data_paths, &package, None, None);
        let file_snapshot = read_repository_policy(&data_paths, &package).unwrap();

        assert!(virtual_snapshot.repositories.is_empty());
        assert_eq!(
            file_snapshot.repositories.len(),
            1,
            "the protected-file snapshot must reflect the installed test policy"
        );
        assert_ne!(virtual_snapshot.policy_sha256, file_snapshot.policy_sha256);
        assert!(policy_path.is_file());

        let policy = virtual_package_policy(&package).unwrap();
        assert_ne!(
            effective_policy_digest(
                b"same-policy-bytes",
                0,
                &policy,
                NativePolicySource::VirtualPackageBaseline
            ),
            effective_policy_digest(
                b"same-policy-bytes",
                0,
                &policy,
                NativePolicySource::ProtectedFile
            )
        );
    }

    #[cfg(all(target_os = "linux", unix))]
    #[test]
    fn unsafe_or_malformed_policy_is_not_treated_as_virtual_baseline() {
        let (directory, package) = runtime_test_selection(b"fixture runtime");
        let data_root = directory.path().join("app-data");
        create_private_directory(&data_root).unwrap();
        let host_root = data_root.join("coding-agent/native");
        create_private_directory(&data_root.join("coding-agent")).unwrap();
        create_private_directory(&host_root).unwrap();
        let policy_path = data_root.join(TRUSTED_HOST_POLICY_RELATIVE_PATH);

        std::os::unix::fs::symlink("missing-policy.json", &policy_path).unwrap();
        assert!(matches!(
            read_repository_policy(&DataPaths::with_root(&data_root), &package),
            Err(NativeBootstrapUnavailable::UnsafeTrustedPolicy)
        ));
        fs::remove_file(&policy_path).unwrap();
        fs::write(&policy_path, b"not-json").unwrap();
        fs::set_permissions(&policy_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            read_repository_policy(&DataPaths::with_root(&data_root), &package),
            Err(NativeBootstrapUnavailable::InvalidTrustedPolicy)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_runtime_image_uses_the_live_proc_executable_not_reported_paths() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let helper_directory = private_temp_dir();
        let helper_path = helper_directory.path().join("sleep");
        fs::copy("/usr/bin/sleep", &helper_path).unwrap();
        fs::set_permissions(&helper_path, fs::Permissions::from_mode(0o700)).unwrap();
        // Keep the fixture within the production image-size limit regardless of debug symbols.
        let child = ChildGuard(
            std::process::Command::new(&helper_path)
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let mut snapshot = current_process_snapshot();
        snapshot.child_pid = child.0.id();
        let current_executable = fs::read(format!("/proc/{}/exe", snapshot.child_pid)).unwrap();
        let (_directory, matching_selection) = runtime_test_selection(&current_executable);
        let observation = matching_selection
            .verify_managed_runtime(&snapshot)
            .expect("the opened live executable matches its selected manifest digest");
        assert_eq!(observation.sha256(), sha256(&current_executable));
        assert_eq!(observation.child_pid(), child.0.id());

        let (_directory, mismatched_selection) = runtime_test_selection(b"other runtime image");
        assert_eq!(
            mismatched_selection.verify_managed_runtime(&snapshot),
            Err(NativeRuntimeImageUnavailable::ImageDigestMismatch)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_runtime_check_reports_package_bytes_changed_after_selection() {
        let snapshot = current_process_snapshot();
        let (_directory, selection) = runtime_test_selection(b"fixture runtime");
        fs::write(
            selection.package_root.join(EXECUTOR_RELATIVE_PATH),
            b"TEST EXECUTOR ARTIFACT",
        )
        .unwrap();

        assert_eq!(
            selection.verify_managed_runtime(&snapshot),
            Err(NativeRuntimeImageUnavailable::PackageChanged)
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn repository_check_pipeline_probes_then_runs_and_cleans_private_state() {
        let data_directory = private_temp_dir();
        let data_paths = DataPaths::with_root(data_directory.path());
        let marker = data_directory.path().join("operation-marker");
        let helper =
            diagnostic_executor_script(&["prepare", "check_repository"], &marker, true, true);
        let (_package_directory, package) = runtime_test_selection_with_executor(
            b"fixture runtime",
            &helper,
            &["prepare", "check_repository"],
        );
        let policy_path =
            install_test_policy(&data_paths, &package, Some("read"), Some("read.token"));
        let source_path = data_directory
            .path()
            .join("coding-agent/native/credential-sources/read.token");
        let source_before = fs::read(&source_path).unwrap();
        let repository = RepositoryId {
            host: "example.test".into(),
            owner: "owner".into(),
            name: "repo".into(),
        };

        let policy_digest = sha256(&fs::read(&policy_path).unwrap());
        let check = tokio::spawn({
            let data_paths = data_paths.clone();
            let package = package.clone();
            let repository = repository.clone();
            async move {
                execute_repository_check(&data_paths, &package, &policy_digest, &repository).await
            }
        });
        for _ in 0..200 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(marker.exists());
        let checks_root = data_directory.path().join("coding-agent/native/checks");
        let temporary = fs::read_dir(&checks_root)
            .unwrap()
            .next()
            .expect("temporary diagnostic root exists")
            .unwrap()
            .path();
        assert!(fs::read_dir(temporary.join("credentials")).unwrap().count() > 0);
        assert!(fs::read_dir(temporary.join("config")).unwrap().count() > 0);
        let result = check.await.unwrap().unwrap();

        assert_eq!(result.repository, repository);
        assert_eq!(result.stages[0].code, "git_readable");
        assert_eq!(result.stages[1].code, "forge_not_configured");
        assert_eq!(fs::read(&source_path).unwrap(), source_before);
        let host_root = data_directory.path().join("coding-agent/native");
        assert_eq!(fs::read_dir(&checks_root).unwrap().count(), 0);
        assert!(!host_root.join("repositories").exists());
        assert!(!host_root.join("sessions").exists());
        assert!(!host_root.join("native-terminal.sqlite3").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn capability_mismatch_stops_before_credential_snapshot_or_operation() {
        let data_directory = private_temp_dir();
        let data_paths = DataPaths::with_root(data_directory.path());
        let marker = data_directory.path().join("operation-marker");
        let helper = diagnostic_executor_script(&["prepare"], &marker, true, false);
        let (_package_directory, package) = runtime_test_selection_with_executor(
            b"fixture runtime",
            &helper,
            &["prepare", "check_repository"],
        );
        let policy_path =
            install_test_policy(&data_paths, &package, Some("read"), Some("read.token"));
        let source_path = data_directory
            .path()
            .join("coding-agent/native/credential-sources/read.token");
        let access_watcher = TokenAccessWatcher::new(&source_path);
        let repository = RepositoryId {
            host: "example.test".into(),
            owner: "owner".into(),
            name: "repo".into(),
        };

        let result = execute_repository_check(
            &data_paths,
            &package,
            &sha256(&fs::read(&policy_path).unwrap()),
            &repository,
        )
        .await;

        assert!(matches!(
            result,
            Err(NativeRepositoryCheckError::Bootstrap(
                NativeBootstrapUnavailable::ExecutorUnavailable
            ))
        ));
        assert!(!marker.exists());
        assert_eq!(access_watcher.access_events(), 0);
        fs::read(&source_path).unwrap();
        assert!(access_watcher.access_events() > 0);
        let host_root = data_directory.path().join("coding-agent/native");
        assert_eq!(fs::read_dir(host_root.join("checks")).unwrap().count(), 0);
        assert!(!host_root.join("repositories").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn repository_check_invalid_response_cleans_snapshot_and_preserves_source() {
        let data_directory = private_temp_dir();
        let data_paths = DataPaths::with_root(data_directory.path());
        let marker = data_directory.path().join("operation-marker");
        let helper =
            diagnostic_executor_script(&["prepare", "check_repository"], &marker, false, false);
        let (_package_directory, package) = runtime_test_selection_with_executor(
            b"fixture runtime",
            &helper,
            &["prepare", "check_repository"],
        );
        let policy_path =
            install_test_policy(&data_paths, &package, Some("read"), Some("read.token"));
        let source_path = data_directory
            .path()
            .join("coding-agent/native/credential-sources/read.token");
        let source_before = fs::read(&source_path).unwrap();
        let repository = RepositoryId {
            host: "example.test".into(),
            owner: "owner".into(),
            name: "repo".into(),
        };

        let result = execute_repository_check(
            &data_paths,
            &package,
            &sha256(&fs::read(&policy_path).unwrap()),
            &repository,
        )
        .await
        .unwrap();

        assert_eq!(result.stages[0].code, "unavailable");
        assert_eq!(result.stages[1].code, "forge_not_configured");
        assert!(marker.exists());
        assert_eq!(fs::read(&source_path).unwrap(), source_before);
        let host_root = data_directory.path().join("coding-agent/native");
        assert_eq!(fs::read_dir(host_root.join("checks")).unwrap().count(), 0);
    }

    #[test]
    fn repository_policy_requires_the_selected_clone_transport_and_identity() {
        let repository = RepositoryId {
            host: "forge.example".into(),
            owner: "Team".into(),
            name: "App".into(),
        };
        let clone_url = "ssh://deploy@ssh.example:22/Team/App.git";
        let entry = NativeRepositoryPolicyEntry {
            repository: repository.clone(),
            clone_url: clone_url.into(),
            ssh_directory: None,
            read_credential_source: None,
            publish_credential_source: None,
            forge: None,
            allowed_base_branches: vec!["main".into()],
            max_import_bytes: 1024,
        };
        assert!(entry.allows_source(&repository, clone_url));
        use crate::coding_agent::repository::{
            CloneTransport, ForgeKind, RepositoryHostConfig, RepositoryResolution,
            resolve_repository,
        };
        let host = RepositoryHostConfig {
            label_host: repository.host.clone(),
            forge: ForgeKind::Gitea,
            api_base_url: "https://forge.example/api/v1".into(),
            clone_transport: CloneTransport::Ssh {
                host: "ssh.example".into(),
                user: "deploy".into(),
                port: Some(22),
                ssh_directory: None,
            },
            read_credential_ref: None,
            publication_credential_ref: None,
        };
        let labels = vec!["repo:forge.example/Team/App".into()];
        let RepositoryResolution::Ready(resolved) = resolve_repository(&labels, &[host], None)
        else {
            panic!("expected configured repository")
        };
        assert!(
            entry.allows_source(&repository, &resolved.clone_url),
            "app-resolved URL and optional .git suffix must agree"
        );

        assert!(!entry.allows_source(&repository, "ssh://deploy@other.example:22/Team/App.git"));
        assert!(!entry.allows_source(&repository, "ssh://other@ssh.example:22/Team/App.git"));
        let mut wrong = repository.clone();
        wrong.host = "another-forge.example".into();
        assert!(!entry.allows_source(&wrong, clone_url));
        assert!(!entry.allows_source(&repository, "ssh://deploy@ssh.example:23/Team/App"));
        let repository = RepositoryId {
            name: "App.git".into(),
            ..repository
        };
        let entry = NativeRepositoryPolicyEntry {
            repository: repository.clone(),
            clone_url: "ssh://deploy@ssh.example:22/Team/App.git.git".into(),
            ssh_directory: None,
            read_credential_source: None,
            publish_credential_source: None,
            forge: None,
            allowed_base_branches: vec!["main".into()],
            max_import_bytes: 1024,
        };
        assert!(entry.allows_source(&repository, "ssh://deploy@ssh.example:22/Team/App.git"));
        assert!(!entry.allows_source(&repository, "ssh://deploy@ssh.example:22/Team/App"));

        let mut path_bound = entry.clone();
        path_bound.ssh_directory = Some(PathBuf::from("/home/user/.ssh"));
        path_bound.forge = Some(coding_agent_executor::ForgeConfig {
            kind: coding_agent_executor::ForgeKind::Gitea,
            api_base_url: "https://forge.example/api/v1".into(),
            web_base_url: "https://forge.example".into(),
        });
        let host = RepositoryHostConfig {
            label_host: repository.host.clone(),
            forge: ForgeKind::Gitea,
            api_base_url: "https://forge.example/api/v1".into(),
            clone_transport: CloneTransport::Ssh {
                host: "ssh.example".into(),
                user: "deploy".into(),
                port: Some(22),
                ssh_directory: None,
            },
            read_credential_ref: None,
            publication_credential_ref: None,
        };
        assert!(path_bound.matches_host(&host, &repository, clone_url));
        let mut mismatched_host = host.clone();
        mismatched_host.clone_transport = CloneTransport::Ssh {
            host: "ssh.example".into(),
            user: "deploy".into(),
            port: Some(22),
            ssh_directory: Some(PathBuf::from("/home/other/.ssh")),
        };
        assert!(!path_bound.matches_host(&mismatched_host, &repository, clone_url));
    }

    #[test]
    fn effective_policy_merges_gui_approvals_and_keeps_legacy_fallback() {
        let fixed = test_host_repository(None, None, false);
        let policy = test_policy(fixed.clone(), Vec::new());
        let legacy = CodingAgentSettings::default();
        let merged =
            merge_gui_repository_policy(policy, &legacy, PackageSourcePolicy::strict()).unwrap();
        assert_eq!(merged.repositories.len(), 1);
        assert_eq!(merged.repositories[0].id.name, "repo");

        let mut settings = CodingAgentSettings::default();
        settings
            .repository_hosts
            .push(crate::coding_agent::settings::RepositoryHostSettings {
                label_host: "example.test".into(),
                forge: crate::coding_agent::ForgeKind::GitHub,
                api_base_url: "https://example.test/api".into(),
                clone_transport: crate::coding_agent::settings::CloneTransportSettings::Https {
                    base_url: "https://example.test".into(),
                },
                read_credential_ref: None,
                publication_credential_ref: None,
            });
        settings.native_repository_approvals = Some(vec![
            crate::coding_agent::settings::NativeRepositoryApprovalSettings {
                repository: crate::coding_agent::settings::RepositoryIdentitySettings {
                    host: "example.test".into(),
                    organization: "owner".into(),
                    repository: "new-repo".into(),
                },
                allowed_base_branches: vec!["release/base".into()],
            },
        ]);
        let fixed_policy = test_policy(fixed.clone(), Vec::new());
        let merged =
            merge_gui_repository_policy(fixed_policy, &settings, PackageSourcePolicy::strict())
                .unwrap();
        assert_eq!(merged.repositories.len(), 1);
        assert_eq!(merged.repositories[0].id.name, "new-repo");
        assert_eq!(
            merged.repositories[0].allowed_base_branches,
            ["release/base"]
        );
        assert_eq!(
            merged.repositories[0].clone_url,
            "https://example.test/owner/new-repo"
        );
        let mut top_level = settings.clone();
        top_level.native_repository_approvals = Some(vec![
            crate::coding_agent::settings::NativeRepositoryApprovalSettings {
                repository: crate::coding_agent::settings::RepositoryIdentitySettings {
                    host: "example.test".into(),
                    organization: "owner".into(),
                    repository: "top-level-repo".into(),
                },
                allowed_base_branches: vec!["main".into()],
            },
        ]);
        let merged = merge_gui_repository_policy(
            test_policy(fixed.clone(), Vec::new()),
            &top_level,
            PackageSourcePolicy::strict(),
        )
        .unwrap();
        assert_eq!(merged.repositories[0].id.name, "top-level-repo");
        assert_ne!(
            effective_policy_digest(b"fixed", 0, &merged, NativePolicySource::ProtectedFile),
            effective_policy_digest(b"fixed", 1, &merged, NativePolicySource::ProtectedFile)
        );

        settings.native_repository_approvals = Some(Vec::new());
        let empty = merge_gui_repository_policy(
            test_policy(fixed, Vec::new()),
            &settings,
            PackageSourcePolicy::strict(),
        )
        .unwrap();
        assert!(empty.repositories.is_empty());
    }

    #[test]
    fn gui_approval_inherits_legacy_credentials_when_host_fields_are_null() {
        let fixed = test_host_repository(Some("legacy-read"), Some("legacy-publish"), true);
        let credentials = vec![
            HostCredentialSource {
                id: "legacy-read".into(),
                username: "reader".into(),
                token_file: PathBuf::from("read-token"),
            },
            HostCredentialSource {
                id: "legacy-publish".into(),
                username: "publisher".into(),
                token_file: PathBuf::from("publish-token"),
            },
            HostCredentialSource {
                id: "gui-read".into(),
                username: "gui-reader".into(),
                token_file: PathBuf::from("gui-read-token"),
            },
        ];
        let mut settings = CodingAgentSettings::default();
        settings
            .repository_hosts
            .push(crate::coding_agent::settings::RepositoryHostSettings {
                label_host: "example.test".into(),
                forge: crate::coding_agent::ForgeKind::GitHub,
                api_base_url: "https://example.test/api".into(),
                clone_transport: crate::coding_agent::settings::CloneTransportSettings::Https {
                    base_url: "https://example.test".into(),
                },
                read_credential_ref: None,
                publication_credential_ref: None,
            });
        settings.native_repository_approvals = Some(vec![
            crate::coding_agent::settings::NativeRepositoryApprovalSettings {
                repository: crate::coding_agent::settings::RepositoryIdentitySettings {
                    host: "example.test".into(),
                    organization: "owner".into(),
                    repository: "repo".into(),
                },
                allowed_base_branches: vec!["main".into()],
            },
        ]);
        let mut policy = test_policy(fixed, credentials);
        let inherited =
            merge_gui_repository_policy(policy.clone(), &settings, PackageSourcePolicy::strict())
                .unwrap();
        assert_eq!(
            inherited.repositories[0].read_credential_source.as_deref(),
            Some("legacy-read")
        );
        assert_eq!(
            inherited.repositories[0]
                .publish_credential_source
                .as_deref(),
            Some("legacy-publish")
        );

        settings.repository_hosts[0].read_credential_ref = Some("gui-read".into());
        policy.repositories[0].read_credential_source = Some("legacy-read".into());
        let overridden =
            merge_gui_repository_policy(policy, &settings, PackageSourcePolicy::strict()).unwrap();
        assert_eq!(
            overridden.repositories[0].read_credential_source.as_deref(),
            Some("gui-read")
        );
    }

    #[cfg(unix)]
    #[test]
    fn degraded_fixed_ssh_metadata_is_replaced_before_effective_validation() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let replacement = temp.path().join("replacement-ssh");
        fs::create_dir(&replacement).unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();

        let mut fixed = test_host_repository(None, None, false);
        fixed.clone_url = "ssh://git@example.test/owner/repo".into();
        fixed.ssh_directory = Some(temp.path().join("deleted-ssh"));
        let policy = test_policy(fixed, Vec::new());
        validate_policy_structure(&policy, PackageSourcePolicy::strict()).unwrap();

        let mut settings = CodingAgentSettings::default();
        settings
            .repository_hosts
            .push(crate::coding_agent::settings::RepositoryHostSettings {
                label_host: "example.test".into(),
                forge: crate::coding_agent::ForgeKind::GitHub,
                api_base_url: "https://example.test/api".into(),
                clone_transport: crate::coding_agent::settings::CloneTransportSettings::Ssh {
                    host: "example.test".into(),
                    user: "git".into(),
                    port: None,
                    ssh_directory: Some(replacement.clone()),
                },
                read_credential_ref: None,
                publication_credential_ref: None,
            });
        let merged =
            merge_gui_repository_policy(policy, &settings, PackageSourcePolicy::strict()).unwrap();
        assert_eq!(merged.repositories[0].ssh_directory, Some(replacement));

        let deny_all_policy = test_policy(
            HostRepository {
                id: RepositoryId {
                    host: "example.test".into(),
                    owner: "owner".into(),
                    name: "repo".into(),
                },
                clone_url: "ssh://git@example.test/owner/repo".into(),
                allowed_base_branches: vec!["main".into()],
                read_credential_source: None,
                publish_credential_source: None,
                forge: None,
                ssh_directory: Some(temp.path().join("deleted-again")),
            },
            Vec::new(),
        );
        validate_policy_structure(&deny_all_policy, PackageSourcePolicy::strict()).unwrap();
        let deny_settings = CodingAgentSettings {
            native_repository_approvals: Some(Vec::new()),
            ..Default::default()
        };
        let denied = merge_gui_repository_policy(
            deny_all_policy,
            &deny_settings,
            PackageSourcePolicy::strict(),
        )
        .unwrap();
        assert!(denied.repositories.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_repository_uses_saved_ssh_directory_when_legacy_json_omits_it() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let ssh_directory = temp.path().join("ssh");
        fs::create_dir(&ssh_directory).unwrap();
        fs::set_permissions(&ssh_directory, fs::Permissions::from_mode(0o700)).unwrap();
        let mut fixed = test_host_repository(None, None, false);
        fixed.clone_url = "ssh://git@example.test/owner/repo".into();
        fixed.ssh_directory = Some(ssh_directory.clone());
        let policy = test_policy(fixed, Vec::new());
        let mut settings = CodingAgentSettings::default();
        settings
            .repository_hosts
            .push(crate::coding_agent::settings::RepositoryHostSettings {
                label_host: "example.test".into(),
                forge: crate::coding_agent::ForgeKind::GitHub,
                api_base_url: "https://example.test/api".into(),
                clone_transport: crate::coding_agent::settings::CloneTransportSettings::Ssh {
                    host: "example.test".into(),
                    user: "git".into(),
                    port: None,
                    ssh_directory: None,
                },
                read_credential_ref: None,
                publication_credential_ref: None,
            });
        let merged =
            merge_gui_repository_policy(policy, &settings, PackageSourcePolicy::strict()).unwrap();
        assert_eq!(merged.repositories[0].ssh_directory, Some(ssh_directory));
    }
}
