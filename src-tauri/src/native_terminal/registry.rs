//! Concrete native registry joining protected repository policy, canonical
//! Issue/profile authority, per-repository preparation services, strict runtime
//! observation, owner-bound sessions, and output subscriptions.
//!
//! This module deliberately owns no Issue workflow, report, judge, quota, or
//! publication operation. A successful `prepare` returns only after a private
//! preview-bound CLI version observation; `start` is the only path that opens
//! the interactive `run_with_client` stream.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use async_trait::async_trait;
use parking_lot::Mutex as ParkingMutex;
use serde::Serialize;
use thiserror::Error;
use tokio::{
    sync::{Mutex, oneshot},
    task::JoinHandle,
};

use crate::{
    coding_agent::{
        HostResourceLimits, RepositoryIdentity,
        settings::{
            CodingAgentSettingsStore, NativeRepositoryApprovalSettings, RepositoryIdentitySettings,
            SettingsError,
        },
    },
    commands::IssueTrackerAvailability,
    data::DataPaths,
    jobworkerp::JobworkerpHandle,
    sidecar::Sidecars,
};

use super::manager::NativeSessionManagementOwnership;
use super::{
    NativeSessionRecord, NativeTerminalManager, NativeTerminalManagerError,
    NativeTerminalOutputEvent, NativeTerminalOutputRecord, NativeTerminalStore,
    NativeTerminalSubscription, ObservedRuntimeLease,
    authority::ManagedNativeSessionAuthority,
    bootstrap::{
        NativeBootstrapUnavailable, NativePackageSelection, NativeRepositoryPolicySnapshot,
        configure_repository, existing_session_store_paths, read_repository_policy,
        read_repository_policy_for_diagnostic,
    },
    doctor::{
        NativePreparationSourceContext, NativeRuntimeDoctor, NativeRuntimeDoctorError,
        inspect_current_preparation_source, inspect_current_preparation_source_admitted,
    },
    is_canonical_uuid, is_sha256,
    launch::NativeAgentKind,
    preparation::{
        NativePreparationError, NativePreparationRuntime, NativePrepareRequest,
        NativeSessionPreparer, PreparedAgentWorkspace,
    },
    wire::{
        NativeTerminalAttachmentWire, NativeTerminalInputDeliveryWire,
        NativeTerminalOutputEventWire, NativeTerminalOutputPageWire,
        NativeTerminalOutputRecordWire, NativeTerminalPreviewWire, NativeTerminalProfileWire,
        NativeTerminalSessionWire, NativeTerminalStatusWire, NativeTerminalWireError,
    },
};

const MAX_REGISTRY_ISSUE_ID_BYTES: usize = 512;
const MAX_OUTPUT_PAGE_EVENTS: usize = 16;
const MAX_NATIVE_INPUT_BYTES: usize = 64 * 1024;
const JOBWORKERP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const OUTPUT_SINK_TIMEOUT: Duration = Duration::from_secs(2);

/// Native-only sink supplied by the owning webview command. The sink is never
/// renderer selected; its payload is a deliberately narrow output event DTO.
#[async_trait]
#[allow(clippy::double_must_use)]
pub trait NativeTerminalOutputSink: Send + Sync {
    async fn emit(&self, event: NativeTerminalOutputEventWire) -> Result<(), ()>;
}

/// Result from attach includes an in-memory native subscription which is not
/// serialized. A Tauri adapter returns `attachment` and leaves the subscription
/// task owned by the registry until explicit detach or stream termination.
pub struct NativeTerminalAttachResult {
    pub attachment: NativeTerminalAttachmentWire,
}

impl std::fmt::Debug for NativeTerminalAttachResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeTerminalAttachResult")
            .field("attachment", &self.attachment)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeRuntimeRegistryError {
    #[error("native terminal request is invalid")]
    InvalidRequest,
    #[error("native interactive workspaces are unavailable")]
    Unavailable,
    #[error("the application is shutting down and no longer accepts native terminal operations")]
    ShuttingDown,
    #[error("native runtime source is unavailable or changed")]
    SourceUnavailable,
    #[error("canonical Issue or profile authority is unavailable")]
    AuthorityUnavailable,
    #[error("the Issue repository is not approved for native interactive workspaces")]
    RepositoryUnavailable,
    #[error("the requested base branch is not approved by native host policy")]
    BranchUnavailable,
    #[error("the selected profile is not supported for the native OpenCode terminal")]
    ProfileUnavailable,
    #[error("the selected profile has no writable home mount for the settings shell")]
    ProfileHomeUnavailable,
    #[error("native workspace preparation failed")]
    PreparationFailed,
    #[error("the preview did not pass strict native runtime observation")]
    RuntimeObservationFailed,
    #[error("the preview has no current private runtime observation")]
    RuntimeNotObserved,
    #[error("native start acceptance or outcome is unknown; do not retry this preview")]
    StartOutcomeUnknown,
    #[error("native terminal preview or session was not found")]
    NotFound,
    #[error("native terminal resource belongs to another window")]
    OwnerMismatch,
    #[error("native terminal request conflicts with its immutable record")]
    Conflict,
    #[error("native terminal output or session storage is unavailable")]
    StorageUnavailable,
    #[error("native terminal input exceeds the bounded frame size")]
    InputTooLarge,
    #[error("native terminal output subscription is unavailable")]
    SubscriptionUnavailable,
    #[error("native terminal record cannot be represented by the renderer contract")]
    InvalidRecord,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeSetupState {
    NotApproved,
    Approved,
    /// A different package was approved earlier; the current one can be
    /// approved from the GUI with the same explicit action.
    ReapprovalRequired,
    /// The protected legacy host policy pins another package identity. This
    /// cannot be repaired from the Settings UI.
    PackageMismatch,
    Unavailable,
}

/// Why the native setup is unavailable, as a stable renderer-facing code. The
/// renderer turns each code into an explanation and a concrete next step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeSetupUnavailableReason {
    UnsupportedPlatform,
    PackageMissing,
    TrustedPolicyUnsafe,
    TrustedPolicyInvalid,
    StorageUnavailable,
    ExecutorUnavailable,
    SettingsUnavailable,
}

impl From<NativeBootstrapUnavailable> for NativeSetupUnavailableReason {
    fn from(error: NativeBootstrapUnavailable) -> Self {
        match error {
            NativeBootstrapUnavailable::UnsupportedPlatform => Self::UnsupportedPlatform,
            NativeBootstrapUnavailable::PackageUnavailable => Self::PackageMissing,
            NativeBootstrapUnavailable::UnsafeTrustedPolicy => Self::TrustedPolicyUnsafe,
            NativeBootstrapUnavailable::InvalidTrustedPolicy => Self::TrustedPolicyInvalid,
            NativeBootstrapUnavailable::StorageUnavailable => Self::StorageUnavailable,
            NativeBootstrapUnavailable::ExecutorUnavailable => Self::ExecutorUnavailable,
            // The setup snapshot never performs repository or credential
            // lookups, substitutes a virtual baseline for a missing policy, and
            // reports an identity mismatch as its own state. These variants can
            // only reach this conversion through a future policy-loading change,
            // so they are reported as a policy problem rather than silently
            // treated as "package missing".
            NativeBootstrapUnavailable::TrustedPolicyMissing
            | NativeBootstrapUnavailable::PackageIdentityMismatch
            | NativeBootstrapUnavailable::RepositoryNotApproved
            | NativeBootstrapUnavailable::CredentialUnavailable => Self::TrustedPolicyInvalid,
        }
    }
}

/// Whether the installed package was produced from pinned clean sources or
/// staged by development tooling from a working tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeSetupPackageProvenance {
    Release,
    Development,
}

/// Which list currently acts as the repository approval authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeSetupApprovalSource {
    /// `nativeRepositoryApprovals` has been saved from the GUI (possibly empty).
    Gui,
    /// No GUI list exists yet; the legacy protected policy (or its empty
    /// virtual baseline) is the effective authority.
    LegacyPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeSetupSnapshot {
    pub(crate) settings_revision: u64,
    pub(crate) state: NativeSetupState,
    pub(crate) package_fingerprint: Option<String>,
    /// Executor version from the manifest, shown beside the fingerprint so the
    /// user can tell what the approval action refers to.
    pub(crate) package_version: Option<String>,
    pub(crate) repository_approvals: Vec<NativeRepositoryApprovalSettings>,
    pub(crate) unavailable_reason: Option<NativeSetupUnavailableReason>,
    pub(crate) package_provenance: Option<NativeSetupPackageProvenance>,
    pub(crate) approval_source: NativeSetupApprovalSource,
}

impl NativeRuntimeRegistryError {
    /// Stable display text safe for a renderer. It excludes paths, subprocess
    /// output, config digests, credential sources, and raw authority payloads.
    pub fn user_message(self) -> &'static str {
        match self {
            Self::InvalidRequest => "The native terminal request is invalid.",
            Self::Unavailable => {
                "Native interactive workspaces are unavailable. Check the protected native host policy and installed app package."
            }
            Self::ShuttingDown => {
                "The application is shutting down and no longer accepts native terminal operations."
            }
            Self::SourceUnavailable => {
                "The managed native runtime SDK source is unavailable or changed. Restart the local worker and try again."
            }
            Self::AuthorityUnavailable => {
                "The current Issue or coding-agent settings could not be verified. Refresh the Issue and try again."
            }
            Self::RepositoryUnavailable => {
                "This Issue's repository is not approved in native workspace settings."
            }
            Self::BranchUnavailable => {
                "Choose a base branch approved in native workspace settings."
            }
            Self::ProfileUnavailable => {
                "Choose a current OpenCode profile without unsupported host credentials."
            }
            Self::ProfileHomeUnavailable => {
                "Prepare a dedicated home for this profile (or add a writable mount inside its container home) before opening the authentication and settings shell."
            }
            Self::PreparationFailed => {
                "The native workspace could not be prepared. No interactive job was started."
            }
            Self::RuntimeObservationFailed => {
                "The prepared OpenCode runtime did not pass its bounded version check. No interactive job was started."
            }
            Self::RuntimeNotObserved => {
                "This preview has no current private runtime observation. Prepare a fresh preview before starting."
            }
            Self::StartOutcomeUnknown => {
                "Native start acceptance or outcome is unknown. Check the session list; do not retry this preview."
            }
            Self::NotFound => "The native terminal preview or session was not found.",
            Self::OwnerMismatch => "This native terminal belongs to another app window.",
            Self::Conflict => "The native terminal request conflicts with its immutable record.",
            Self::StorageUnavailable => "Private native terminal state is unavailable.",
            Self::InputTooLarge => "Native terminal input exceeds the 64 KiB limit.",
            Self::SubscriptionUnavailable => {
                "The native terminal output subscription is unavailable."
            }
            Self::InvalidRecord => "The native terminal record is invalid.",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RepositoryServiceKey {
    host: String,
    organization: String,
    repository: String,
    policy_sha256: String,
    config_sha256: String,
    package_manifest_sha256: String,
    source_digest: String,
    child_generation_id: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PreviewOwnerKey {
    owner_window_label: String,
    preview_id: String,
}

struct RepositoryServices {
    preparer: NativeSessionPreparer<ManagedNativeSessionAuthority>,
    manager: NativeTerminalManager,
    doctor: NativeRuntimeDoctor,
}

struct RegistrySubscription {
    owner_window_label: String,
    session_id: String,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

/// Concrete app-owned registry. Optional native assets are selected once from
/// the Tauri-provided resource directory; a missing package or host policy is
/// retained as an unavailable state rather than making app construction fail.
pub struct NativeRuntimeRegistry {
    data_paths: DataPaths,
    sidecars: Arc<Sidecars>,
    issue_tracker_availability: Arc<IssueTrackerAvailability>,
    settings: Arc<CodingAgentSettingsStore>,
    authority: Arc<ManagedNativeSessionAuthority>,
    shared_admission: Arc<Mutex<()>>,
    accepting: Arc<AtomicBool>,
    host_limits: HostResourceLimits,
    host_limits_valid: bool,
    package: Option<Arc<NativePackageSelection>>,
    package_error: Option<NativeBootstrapUnavailable>,
    handles: Mutex<HashMap<u64, JobworkerpHandle>>,
    service_build_lock: Mutex<()>,
    services: Mutex<HashMap<RepositoryServiceKey, Arc<RepositoryServices>>>,
    preview_services: ParkingMutex<HashMap<PreviewOwnerKey, RepositoryServiceKey>>,
    observed_leases: ParkingMutex<HashMap<PreviewOwnerKey, Arc<ObservedRuntimeLease>>>,
    subscriptions: Arc<ParkingMutex<HashMap<String, RegistrySubscription>>>,
    startup_recovery_complete: ParkingMutex<bool>,
    startup_recovery_lock: Mutex<()>,
    pub profile_settings_terminals: Arc<super::profile_settings::ProfileSettingsTerminals>,
}

impl std::fmt::Debug for NativeRuntimeRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeRuntimeRegistry")
            .field("package_available", &self.package.is_some())
            .field("host_limits_valid", &self.host_limits_valid)
            .field(
                "cached_repositories",
                &self.services.try_lock().map(|s| s.len()).ok(),
            )
            .finish_non_exhaustive()
    }
}

impl NativeRuntimeRegistry {
    pub(crate) fn native_setup(&self) -> NativeSetupSnapshot {
        let package = self.package.as_deref();
        let package_provenance = package.map(|package| {
            if package.development_sources() {
                NativeSetupPackageProvenance::Development
            } else {
                NativeSetupPackageProvenance::Release
            }
        });
        let settings = self.settings.load().ok();
        let approval_source = match &settings {
            Some(settings) if settings.native_repository_approvals.is_some() => {
                NativeSetupApprovalSource::Gui
            }
            _ => NativeSetupApprovalSource::LegacyPolicy,
        };
        let (state, repository_approvals, unavailable_reason) = match (&settings, package) {
            (None, _) => (
                NativeSetupState::Unavailable,
                Vec::new(),
                Some(NativeSetupUnavailableReason::SettingsUnavailable),
            ),
            (Some(settings), None) => (
                NativeSetupState::Unavailable,
                flatten_native_approvals(settings),
                Some(
                    self.package_error
                        .map_or(NativeSetupUnavailableReason::PackageMissing, Into::into),
                ),
            ),
            // The protected policy decides whether approval is possible at all,
            // so it is read before comparing the saved fingerprint: a legacy
            // identity mismatch must not be reported as a GUI-repairable
            // re-approval.
            (Some(settings), Some(package)) => {
                match read_repository_policy_for_diagnostic(&self.data_paths, package) {
                    Ok(policy) => {
                        let state = match settings.native_backend_package_fingerprint.as_deref() {
                            Some(approved) if approved == package.package_fingerprint() => {
                                NativeSetupState::Approved
                            }
                            Some(_) => NativeSetupState::ReapprovalRequired,
                            None => NativeSetupState::NotApproved,
                        };
                        (state, effective_setup_approvals(settings, policy), None)
                    }
                    Err(NativeBootstrapUnavailable::PackageIdentityMismatch) => (
                        NativeSetupState::PackageMismatch,
                        flatten_native_approvals(settings),
                        None,
                    ),
                    Err(error) => (
                        NativeSetupState::Unavailable,
                        flatten_native_approvals(settings),
                        Some(error.into()),
                    ),
                }
            }
        };
        NativeSetupSnapshot {
            settings_revision: settings.as_ref().map_or(0, |settings| settings.revision),
            state,
            package_fingerprint: package.map(|package| package.package_fingerprint().to_owned()),
            package_version: package.map(|package| package.package_version().to_owned()),
            repository_approvals,
            unavailable_reason,
            package_provenance,
            approval_source,
        }
    }

    pub(crate) fn approve_native_backend(
        &self,
        expected_revision: u64,
        expected_package_fingerprint: Option<&str>,
    ) -> Result<NativeSetupSnapshot, NativeRuntimeRegistryError> {
        let package = self
            .package
            .as_deref()
            .ok_or(NativeRuntimeRegistryError::Unavailable)?;
        if expected_package_fingerprint != Some(package.package_fingerprint()) {
            return Err(NativeRuntimeRegistryError::Conflict);
        }
        let current = self
            .settings
            .load()
            .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?;
        if current.revision != expected_revision {
            return Err(NativeRuntimeRegistryError::Conflict);
        }
        // The explicit action approves only the verified package identity.
        // Repository approvals, including legacy `None`, remain untouched.
        read_repository_policy(&self.data_paths, package).map_err(map_bootstrap_error)?;
        self.settings
            .compare_and_swap_native_baseline(
                expected_revision,
                package.package_fingerprint().to_owned(),
            )
            .map_err(map_settings_error_for_native_setup)?;
        Ok(self.native_setup())
    }

    pub(crate) async fn check_repository_host(
        &self,
        request: crate::coding_agent::repository_check::RepositoryHostCheckRequest,
    ) -> Result<
        crate::coding_agent::repository_check::RepositoryHostCheckResponse,
        NativeRuntimeRegistryError,
    > {
        crate::coding_agent::repository_check::check_repository_host(
            &self.data_paths,
            &self.settings,
            self.package.as_deref(),
            self.package_error,
            request,
        )
        .await
        .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)
    }

    pub(crate) async fn publication_lookup_client(
        &self,
        repository: &coding_agent_executor::RepositoryId,
    ) -> Result<
        crate::coding_agent::executor_client::NativeExecutorClient,
        NativeRuntimeRegistryError,
    > {
        let package = self
            .package
            .as_ref()
            .ok_or(NativeRuntimeRegistryError::SourceUnavailable)?;
        self.require_approved_package(package)?;
        let configured = configure_repository(&self.data_paths, package, repository)
            .await
            .map_err(map_bootstrap_error)?;
        Ok(configured.executor().publication_client().clone())
    }

    fn require_approved_package(
        &self,
        package: &NativePackageSelection,
    ) -> Result<(), NativeRuntimeRegistryError> {
        let settings = self
            .settings
            .load()
            .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?;
        if settings.native_backend_package_fingerprint.as_deref()
            != Some(package.package_fingerprint())
        {
            return Err(NativeRuntimeRegistryError::Unavailable);
        }
        Ok(())
    }

    pub(crate) fn llm_settings_snapshot(&self) -> crate::commands::llm_settings::LlmSettings {
        crate::commands::llm_settings::load_llm_settings(&self.data_paths.llm_settings_path())
    }

    pub(crate) fn semantic_judge_policy(
        &self,
    ) -> Result<crate::coding_agent::semantic_judge::SemanticJudgePolicy, NativeRuntimeRegistryError>
    {
        crate::coding_agent::settings::CodingAgentSettingsStore::new(
            self.data_paths.coding_agent_settings_path(),
        )
        .load()
        .map(|settings| settings.semantic_judge_policy)
        .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)
    }

    pub(crate) async fn semantic_judge_handle(
        &self,
    ) -> Result<JobworkerpHandle, NativeRuntimeRegistryError> {
        let snapshot = self
            .sidecars
            .managed_jobworkerp_spawn_snapshot()
            .ok_or(NativeRuntimeRegistryError::SourceUnavailable)?;
        self.jobworkerp_for_generation(snapshot.generation_id).await
    }

    pub(crate) async fn claim_automatic_issue(
        &self,
        generation: u64,
        digest: &str,
        prepared: &PreparedAgentWorkspace,
    ) -> Result<crate::coding_agent::issue_gateway::ClaimedAutomaticIssue, NativeRuntimeRegistryError>
    {
        let _admission = self.shared_admission.lock().await;
        let _lifecycle = self.sidecars.lock_native_jobworkerp_admission().await;
        self.verify_automatic_source(generation, digest).await?;
        self.verify_automatic_authority(prepared).await?;
        let endpoint = self
            .sidecars
            .issue_tracker_gateway_worker_url()
            .ok_or(NativeRuntimeRegistryError::AuthorityUnavailable)?;
        let gateway = crate::coding_agent::issue_gateway::AutomaticIssueGateway::new(endpoint)
            .map_err(|_| NativeRuntimeRegistryError::AuthorityUnavailable)?;
        let claim = gateway
            .claim(
                &prepared.workspace.run_id,
                prepared.authority.canonical_issue.issue_id(),
                prepared.authority.canonical_issue.revision(),
            )
            .await
            .map_err(|_| NativeRuntimeRegistryError::AuthorityUnavailable)?;
        Ok(crate::coding_agent::issue_gateway::ClaimedAutomaticIssue::new(gateway, claim))
    }

    pub(crate) async fn verify_automatic_owned_authority(
        &self,
        prepared: &PreparedAgentWorkspace,
        owned: &crate::coding_agent::issue_gateway::OwnedIssueRevision,
    ) -> Result<(), NativeRuntimeRegistryError> {
        if owned.run_id() != prepared.workspace.run_id
            || owned.issue_id() != prepared.authority.canonical_issue.issue_id()
        {
            return Err(NativeRuntimeRegistryError::AuthorityUnavailable);
        }
        self.verify_automatic_authority_at_revision(prepared, Some(owned.revision()))
            .await
    }

    pub(crate) async fn admit_automatic_publication<T>(
        &self,
        generation: u64,
        digest: &str,
        prepared: &PreparedAgentWorkspace,
        owned: &crate::coding_agent::issue_gateway::OwnedIssueRevision,
        cancellation: &tokio_util::sync::CancellationToken,
        dispatch: impl std::future::Future<Output = Result<T, crate::coding_agent::NativeExecutorError>>,
    ) -> Result<T, NativeRuntimeRegistryError> {
        let (_admission, _lifecycle) = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(NativeRuntimeRegistryError::AuthorityUnavailable),
            result = async {
                let admission = self.shared_admission.lock().await;
                let lifecycle = self.sidecars.lock_native_jobworkerp_admission().await;
                self.verify_automatic_source(generation, digest).await?;
                self.verify_automatic_owned_authority(prepared, owned).await?;
                Ok::<_, NativeRuntimeRegistryError>((admission,lifecycle))
            } => result?,
        };
        if cancellation.is_cancelled() {
            return Err(NativeRuntimeRegistryError::AuthorityUnavailable);
        }
        dispatch
            .await
            .map_err(|_| NativeRuntimeRegistryError::SourceUnavailable)
    }

    /// Compatibility constructor with a private, accepting-by-default flag.
    /// Production AppState must use `new_with_accepting` with its shared quit
    /// flag; this constructor does not synchronize acceptance with app quit.
    /// `resource_dir` must come directly from Tauri's `AppHandle::path()`;
    /// callers must not derive it from a renderer, current directory, or an
    /// arbitrary executable path. Host resource limits are trusted Rust-side
    /// application policy and are never accepted in request DTOs.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        data_paths: DataPaths,
        sidecars: Arc<Sidecars>,
        issue_tracker_availability: Arc<IssueTrackerAvailability>,
        settings: Arc<CodingAgentSettingsStore>,
        shared_admission: Arc<Mutex<()>>,
        host_limits: HostResourceLimits,
        resource_dir: Option<PathBuf>,
    ) -> Self {
        Self::new_with_accepting(
            data_paths,
            sidecars,
            issue_tracker_availability,
            settings,
            shared_admission,
            host_limits,
            resource_dir,
            Arc::new(AtomicBool::new(true)),
        )
    }

    /// Construct the registry with the app's shared quit-acceptance flag.
    /// The Doctor rechecks this flag only after acquiring the same Settings
    /// admission guard that the root quit barrier drains.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_accepting(
        data_paths: DataPaths,
        sidecars: Arc<Sidecars>,
        issue_tracker_availability: Arc<IssueTrackerAvailability>,
        settings: Arc<CodingAgentSettingsStore>,
        shared_admission: Arc<Mutex<()>>,
        host_limits: HostResourceLimits,
        resource_dir: Option<PathBuf>,
        accepting: Arc<AtomicBool>,
    ) -> Self {
        let package_result = resource_dir
            .as_deref()
            .ok_or(NativeBootstrapUnavailable::PackageUnavailable)
            .and_then(NativePackageSelection::from_app_resources);
        let (package, package_error) = match package_result {
            Ok(package) => (Some(Arc::new(package)), None),
            Err(error) => (None, Some(error)),
        };
        let host_limits_valid = host_limits.cpu_millis > 0
            && host_limits.memory_mib > 0
            && host_limits.root_disk_mib > 0
            && host_limits.max_parallel_runs > 0;
        let authority = Arc::new(ManagedNativeSessionAuthority::new(
            Arc::clone(&sidecars),
            Arc::clone(&issue_tracker_availability),
            Arc::clone(&settings),
        ));
        Self {
            data_paths,
            sidecars,
            issue_tracker_availability,
            settings,
            authority,
            shared_admission,
            accepting,
            host_limits,
            host_limits_valid,
            package,
            package_error,
            handles: Mutex::new(HashMap::new()),
            service_build_lock: Mutex::new(()),
            services: Mutex::new(HashMap::new()),
            preview_services: ParkingMutex::new(HashMap::new()),
            observed_leases: ParkingMutex::new(HashMap::new()),
            subscriptions: Arc::new(ParkingMutex::new(HashMap::new())),
            startup_recovery_complete: ParkingMutex::new(false),
            startup_recovery_lock: Mutex::new(()),
            profile_settings_terminals: Arc::new(
                super::profile_settings::ProfileSettingsTerminals::new(),
            ),
        }
    }

    /// Report whether host-owned sources are sufficient to prepare a new
    /// preview. This never enqueues, prepares a workspace, performs a CLI
    /// probe, or claims runtime/TUI readiness.
    pub async fn status(
        &self,
        issue_id: &str,
    ) -> Result<NativeTerminalStatusWire, NativeRuntimeRegistryError> {
        validate_issue_id(issue_id)?;
        let mut blockers = Vec::new();
        if let Some(error) = self.package_error {
            blockers.push(error.user_message().to_owned());
        }
        if !self.host_limits_valid {
            blockers.push("Trusted native host resource limits are invalid.".to_owned());
        }
        if !self
            .issue_tracker_availability
            .snapshot()
            .endpoint_connected
        {
            blockers.push("The local Issue tracker endpoint is unavailable.".to_owned());
        }
        let package_policy = self
            .package
            .as_deref()
            .map(|package| read_repository_policy(&self.data_paths, package));
        let policy = match package_policy {
            Some(Ok(policy)) => Some(policy),
            Some(Err(error)) => {
                blockers.push(error.user_message().to_owned());
                None
            }
            None => None,
        };

        let source_context = match inspect_current_preparation_source(&self.sidecars).await {
            Ok(context) => Some(context),
            Err(error) => {
                blockers.push(source_blocker(error).to_owned());
                None
            }
        };
        let grpc_available = if let Some(source) = source_context.as_ref() {
            self.probe_runner_service(source.child_generation_id).await
        } else {
            false
        };
        if source_context.is_some() && !grpc_available {
            blockers.push("The managed local jobworkerp gRPC service is unavailable.".to_owned());
        }
        let settings = match self.settings.load() {
            Ok(settings) => Some(settings),
            Err(_) => {
                blockers.push("Coding-agent settings are unavailable or invalid.".to_owned());
                None
            }
        };
        let native_backend_approved =
            settings
                .as_ref()
                .zip(self.package.as_deref())
                .is_some_and(|(settings, package)| {
                    settings.native_backend_package_fingerprint.as_deref()
                        == Some(package.package_fingerprint())
                });
        if !native_backend_approved {
            blockers.push("The native backend has not been explicitly approved.".to_owned());
        }
        let mut profiles = Vec::new();
        if self.host_limits_valid
            && self
                .issue_tracker_availability
                .snapshot()
                .endpoint_connected
            && let (Some(settings), Some(policy)) = (settings.as_ref(), policy.as_ref())
        {
            for configured_profile in &settings.profiles {
                if profile_has_unsupported_credential(configured_profile) {
                    continue;
                }
                let result = tokio::time::timeout(
                    Duration::from_secs(10),
                    self.authority.read_for_preparation(
                        issue_id,
                        &configured_profile.profile_id,
                        self.host_limits,
                    ),
                )
                .await;
                let Ok(Ok(snapshot)) = result else {
                    continue;
                };
                let Some(repository_policy) = policy.repositories.iter().find(|entry| {
                    repository_identity_matches(&snapshot.repository.identity, &entry.repository)
                }) else {
                    continue;
                };
                if !repository_policy.allows_source(
                    &repository_id(&snapshot.repository.identity),
                    &snapshot.repository.clone_url,
                ) || repository_policy.allowed_base_branches.is_empty()
                    || repository_policy.max_import_bytes == 0
                    || snapshot.profile.credential_reference.is_some()
                    || NativeAgentKind::from_command(&snapshot.profile.command).is_none()
                {
                    continue;
                }
                profiles.push(NativeTerminalProfileWire {
                    profile_id: snapshot.profile.profile_id,
                    label: snapshot.profile.display_name,
                    image_uri: snapshot.profile.image_ref,
                    expected_cli_version: String::new(),
                    allowed_base_branches: repository_policy.allowed_base_branches.clone(),
                });
            }
        }
        profiles.sort_by(|left, right| left.profile_id.cmp(&right.profile_id));
        profiles.dedup_by(|left, right| left.profile_id == right.profile_id);
        if profiles.is_empty() {
            blockers.push(
                "No current coding agent profile is eligible for this Issue's approved repository."
                    .to_owned(),
            );
        }

        // `available` means only that a new native preview can be prepared.
        // The preview itself still requires a strict private CLI observation.
        let available = self.package.is_some()
            && self.host_limits_valid
            && policy.is_some()
            && source_context.is_some()
            && grpc_available
            && self
                .issue_tracker_availability
                .snapshot()
                .endpoint_connected
            && settings.is_some()
            && native_backend_approved
            && !profiles.is_empty();
        blockers.sort();
        blockers.dedup();
        Ok(NativeTerminalStatusWire {
            available,
            blockers,
            profiles,
        })
    }

    /// Prepare a fresh host-approved workspace and automatically require the
    /// strict preview-scoped CLI version observation before returning a preview.
    /// This does not open the interactive TUI stream or report SDK/provider auth.
    pub async fn prepare(
        &self,
        owner_window_label: &str,
        request: super::wire::NativeTerminalPrepareRequest,
    ) -> Result<NativeTerminalPreviewWire, NativeRuntimeRegistryError> {
        validate_owner(owner_window_label)?;
        let (key, services, _, _, _) = self.preparation_services(&request, None).await?;
        let preview = services
            .preparer
            .prepare(
                owner_window_label,
                NativePrepareRequest {
                    issue_id: &request.issue_id,
                    profile_id: &request.profile_id,
                    base_branch: &request.base_branch,
                    edited_prompt: request.prompt.as_deref(),
                },
            )
            .await
            .map_err(map_preparation_error)?;

        // The TUI is still not started. This strictly validates a separate
        // direct CLI version job against the actual managed child and
        // current protected sources before granting the private start lease.
        let lease = services
            .doctor
            .observe_preview(
                owner_window_label,
                preview.preview_id(),
                preview.preview_digest(),
            )
            .await
            .map_err(map_observation_error)?;
        let (context, _) = services
            .manager
            .staged_native_candidate(
                owner_window_label,
                preview.preview_id(),
                preview.preview_digest(),
            )
            .map_err(map_manager_error)?;
        let wire =
            NativeTerminalPreviewWire::from_context(&context, true).map_err(map_wire_error)?;
        let owner_key = PreviewOwnerKey {
            owner_window_label: owner_window_label.to_owned(),
            preview_id: preview.preview_id().to_owned(),
        };
        self.preview_services.lock().insert(owner_key.clone(), key);
        self.observed_leases
            .lock()
            .insert(owner_key, Arc::new(lease));
        Ok(wire)
    }

    /// Prepare the automatic worker and workspace without creating a terminal
    /// preview, session, or interactive start lease. CLI execution is separate.
    pub async fn prepare_automatic(
        &self,
        request: super::wire::NativeTerminalPrepareRequest,
    ) -> Result<(PreparedAgentWorkspace, JobworkerpHandle), NativeRuntimeRegistryError> {
        let (prepared, handle, _, _) = self.prepare_automatic_with_source(request, None).await?;
        Ok((prepared, handle))
    }

    pub(crate) async fn prepare_automatic_with_source(
        &self,
        request: super::wire::NativeTerminalPrepareRequest,
        manual_repository: Option<crate::coding_agent::ResolvedRepository>,
    ) -> Result<
        (
            PreparedAgentWorkspace,
            JobworkerpHandle,
            super::doctor::NativePreparationSourceContext,
            super::bootstrap::NativeWorkspaceExecutor,
        ),
        NativeRuntimeRegistryError,
    > {
        let (_, services, handle, source, authority) = self
            .preparation_services(&request, manual_repository)
            .await?;
        let prepared = services
            .preparer
            .prepare_automatic_with_authority(
                NativePrepareRequest {
                    issue_id: &request.issue_id,
                    profile_id: &request.profile_id,
                    base_branch: &request.base_branch,
                    edited_prompt: request.prompt.as_deref(),
                },
                &authority,
            )
            .await
            .map_err(map_preparation_error)?;
        let current = inspect_current_preparation_source(&self.sidecars)
            .await
            .map_err(|_| NativeRuntimeRegistryError::SourceUnavailable)?;
        if current.child_generation_id != source.child_generation_id
            || current.source_digest != source.source_digest
        {
            return Err(NativeRuntimeRegistryError::SourceUnavailable);
        }
        if !self.accepting.load(std::sync::atomic::Ordering::Acquire) {
            return Err(NativeRuntimeRegistryError::ShuttingDown);
        }
        Ok((
            prepared,
            handle,
            source,
            services.preparer.workspace_executor(),
        ))
    }

    pub(crate) async fn verify_automatic_authority(
        &self,
        prepared: &PreparedAgentWorkspace,
    ) -> Result<(), NativeRuntimeRegistryError> {
        self.verify_automatic_authority_at_revision(prepared, None)
            .await
    }

    pub(crate) async fn verify_automatic_authority_at_revision(
        &self,
        prepared: &PreparedAgentWorkspace,
        owned_revision: Option<u64>,
    ) -> Result<(), NativeRuntimeRegistryError> {
        let authority = self.authority.with_manual_repository(
            (prepared.authority.repository.provenance.selection
                == crate::coding_agent::RepositorySelection::ManualApproved)
                .then(|| prepared.authority.repository.clone()),
        );
        let current = tokio::time::timeout(Duration::from_secs(10), async {
            if owned_revision.is_some() {
                authority
                    .read_for_automatic_execution(&prepared.authority)
                    .await
            } else {
                authority
                    .read_for_preparation(
                        prepared.authority.canonical_issue.issue_id(),
                        &prepared.authority.profile.profile_id,
                        self.host_limits,
                    )
                    .await
            }
        })
        .await
        .map_err(|_| NativeRuntimeRegistryError::AuthorityUnavailable)?
        .map_err(|_| NativeRuntimeRegistryError::AuthorityUnavailable)?;
        if !super::preparation::same_authority_at_owned_revision(
            &prepared.authority,
            &current,
            owned_revision.unwrap_or(prepared.authority.canonical_issue.revision()),
        ) {
            return Err(NativeRuntimeRegistryError::AuthorityUnavailable);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_automatic_workflow(
        &self,
        generation: u64,
        digest: &str,
        prepared: &PreparedAgentWorkspace,
        workflow: &crate::coding_agent::one_shot_workflow::RegisteredOneShotWorkflow,
        handle: &JobworkerpHandle,
        prompt: &str,
        cwd: &str,
        owned: &crate::coding_agent::issue_gateway::OwnedIssueRevision,
    ) -> Result<
        crate::coding_agent::one_shot_workflow::OneShotWorkflowExecution,
        NativeRuntimeRegistryError,
    > {
        let _admission = self.shared_admission.lock().await;
        let _lifecycle = self.sidecars.lock_native_jobworkerp_admission().await;
        self.verify_automatic_source(generation, digest).await?;
        self.verify_automatic_owned_authority(prepared, owned)
            .await?;
        workflow
            .start(
                handle,
                &prepared.authority.profile.command,
                prompt,
                cwd,
                3600,
            )
            .await
            .map_err(|_| NativeRuntimeRegistryError::SourceUnavailable)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_automatic_commit(
        &self,
        generation: u64,
        digest: &str,
        prepared: &PreparedAgentWorkspace,
        workflow: &crate::coding_agent::one_shot_workflow::RegisteredOneShotWorkflow,
        handle: &JobworkerpHandle,
        cwd: &str,
        owned: &crate::coding_agent::issue_gateway::OwnedIssueRevision,
    ) -> Result<
        crate::coding_agent::one_shot_workflow::OneShotWorkflowExecution,
        NativeRuntimeRegistryError,
    > {
        let _admission = self.shared_admission.lock().await;
        let _lifecycle = self.sidecars.lock_native_jobworkerp_admission().await;
        self.verify_automatic_source(generation, digest).await?;
        self.verify_automatic_owned_authority(prepared, owned)
            .await?;
        crate::coding_agent::workspace_commit::start_commit_workspace(
            handle,
            workflow.sandbox_worker(),
            cwd,
            &prepared.workspace.branch,
            &format!(
                "Resolve Issue {}",
                prepared.authority.canonical_issue.issue_id()
            ),
        )
        .await
        .map_err(|_| NativeRuntimeRegistryError::SourceUnavailable)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_automatic_validation(
        &self,
        generation: u64,
        digest: &str,
        prepared: &PreparedAgentWorkspace,
        workflow: &crate::coding_agent::one_shot_workflow::RegisteredOneShotWorkflow,
        handle: &JobworkerpHandle,
        cwd: &str,
        command: &crate::coding_agent::model::ApprovalAssessmentCommand,
        owned: &crate::coding_agent::issue_gateway::OwnedIssueRevision,
    ) -> Result<
        crate::coding_agent::one_shot_workflow::OneShotWorkflowExecution,
        NativeRuntimeRegistryError,
    > {
        let _admission = self.shared_admission.lock().await;
        let _lifecycle = self.sidecars.lock_native_jobworkerp_admission().await;
        self.verify_automatic_source(generation, digest).await?;
        self.verify_automatic_owned_authority(prepared, owned)
            .await?;
        crate::coding_agent::workspace_validation::start_workspace_validation(
            handle,
            workflow.sandbox_worker(),
            command,
            cwd,
        )
        .await
        .map_err(|_| NativeRuntimeRegistryError::SourceUnavailable)
    }

    pub(crate) async fn verify_automatic_source(
        &self,
        generation: u64,
        digest: &str,
    ) -> Result<(), NativeRuntimeRegistryError> {
        if !self.accepting.load(std::sync::atomic::Ordering::Acquire) {
            return Err(NativeRuntimeRegistryError::ShuttingDown);
        }
        let current = inspect_current_preparation_source(&self.sidecars)
            .await
            .map_err(|_| NativeRuntimeRegistryError::SourceUnavailable)?;
        if current.child_generation_id != generation || current.source_digest != digest {
            return Err(NativeRuntimeRegistryError::SourceUnavailable);
        }
        Ok(())
    }

    async fn preparation_services(
        &self,
        request: &super::wire::NativeTerminalPrepareRequest,
        manual_repository: Option<crate::coding_agent::ResolvedRepository>,
    ) -> Result<
        (
            RepositoryServiceKey,
            Arc<RepositoryServices>,
            JobworkerpHandle,
            NativePreparationSourceContext,
            ManagedNativeSessionAuthority,
        ),
        NativeRuntimeRegistryError,
    > {
        validate_issue_id(&request.issue_id)?;
        if request.profile_id.is_empty()
            || request.profile_id.len() > 256
            || request.profile_id.chars().any(char::is_control)
            || request.base_branch.len() > 256
            || request.base_branch.chars().any(char::is_control)
            || request.prompt.as_ref().is_some_and(|prompt| {
                prompt.len() > super::MAX_NATIVE_PROMPT_BYTES || prompt.contains('\0')
            })
        {
            return Err(NativeRuntimeRegistryError::InvalidRequest);
        }
        if !self.accepting.load(std::sync::atomic::Ordering::Acquire) {
            return Err(NativeRuntimeRegistryError::ShuttingDown);
        }
        if !self.host_limits_valid {
            return Err(NativeRuntimeRegistryError::Unavailable);
        }
        let package = self
            .package
            .as_deref()
            .ok_or(NativeRuntimeRegistryError::Unavailable)?;
        if self
            .settings
            .load()
            .map_err(|_| NativeRuntimeRegistryError::AuthorityUnavailable)?
            .native_backend_package_fingerprint
            .as_deref()
            != Some(package.package_fingerprint())
        {
            return Err(NativeRuntimeRegistryError::Unavailable);
        }
        let authority = self.authority.with_manual_repository(manual_repository);
        let initial_authority = tokio::time::timeout(
            Duration::from_secs(10),
            authority.read_for_preparation(
                &request.issue_id,
                &request.profile_id,
                self.host_limits,
            ),
        )
        .await
        .map_err(|_| NativeRuntimeRegistryError::AuthorityUnavailable)?
        .map_err(|_| NativeRuntimeRegistryError::AuthorityUnavailable)?;
        if initial_authority.profile.credential_reference.is_some() {
            return Err(NativeRuntimeRegistryError::ProfileUnavailable);
        }

        let policy =
            read_repository_policy(&self.data_paths, package).map_err(map_bootstrap_error)?;
        let repository = repository_id(&initial_authority.repository.identity);
        let repository_policy = policy
            .repositories
            .iter()
            .find(|entry| entry.repository == repository)
            .ok_or(NativeRuntimeRegistryError::RepositoryUnavailable)?;
        if !repository_policy.allows_source(&repository, &initial_authority.repository.clone_url) {
            return Err(NativeRuntimeRegistryError::RepositoryUnavailable);
        }
        validate_selected_base_branch(
            &request.base_branch,
            &repository_policy.allowed_base_branches,
        )?;

        let source = inspect_current_preparation_source(&self.sidecars)
            .await
            .map_err(|_| NativeRuntimeRegistryError::SourceUnavailable)?;
        let runtime =
            NativePreparationRuntime::new(source.host_worker_context.clone(), self.host_limits)
                .map_err(|_| NativeRuntimeRegistryError::SourceUnavailable)?;
        let handle = self
            .jobworkerp_for_generation(source.child_generation_id)
            .await?;
        let (key, services) = self
            .services_for(
                &initial_authority.repository.identity,
                &source,
                &repository,
                package,
                runtime,
                handle.clone(),
                &policy,
            )
            .await?;
        Ok((key, services, handle, source, authority))
    }

    /// Start only through a private preview lease and the Doctor's atomic
    /// Settings-admission/lifecycle guarded path.
    pub async fn start(
        &self,
        owner_window_label: &str,
        request: super::wire::NativeTerminalStartRequest,
    ) -> Result<NativeTerminalSessionWire, NativeRuntimeRegistryError> {
        validate_owner(owner_window_label)?;
        if !is_canonical_uuid(&request.preview_id) || !is_sha256(&request.preview_digest) {
            return Err(NativeRuntimeRegistryError::InvalidRequest);
        }
        let owner_key = PreviewOwnerKey {
            owner_window_label: owner_window_label.to_owned(),
            preview_id: request.preview_id.clone(),
        };
        let service_key = self
            .preview_services
            .lock()
            .get(&owner_key)
            .cloned()
            .ok_or(NativeRuntimeRegistryError::NotFound)?;
        let package = self
            .package
            .as_deref()
            .ok_or(NativeRuntimeRegistryError::Unavailable)?;
        let current_policy =
            read_repository_policy(&self.data_paths, package).map_err(map_bootstrap_error)?;
        if current_policy.policy_sha256 != service_key.policy_sha256 {
            return Err(NativeRuntimeRegistryError::Conflict);
        }
        let services = self
            .services
            .lock()
            .await
            .get(&service_key)
            .cloned()
            .ok_or(NativeRuntimeRegistryError::NotFound)?;
        let lease = self.observed_leases.lock().get(&owner_key).cloned();
        let record = services
            .doctor
            .start_observed(
                lease.as_deref(),
                owner_window_label,
                &request.preview_id,
                &request.preview_digest,
            )
            .await
            .map_err(map_start_error)?;
        NativeTerminalSessionWire::from_record(&record).map_err(map_wire_error)
    }

    pub async fn list(
        &self,
        owner_window_label: &str,
        issue_id: &str,
    ) -> Result<Vec<NativeTerminalSessionWire>, NativeRuntimeRegistryError> {
        validate_owner(owner_window_label)?;
        validate_issue_id(issue_id)?;
        let mut records = HashMap::<String, NativeSessionRecord>::new();
        for service in self.cached_services().await {
            for record in service
                .manager
                .list_sessions_for_issue(owner_window_label, issue_id)
                .map_err(map_manager_error)?
            {
                records.insert(record.session_id.clone(), record);
            }
        }
        for record in self.durable_records_for_window(owner_window_label, Some(issue_id))? {
            records.insert(record.session_id.clone(), record);
        }
        let mut sessions = records
            .values()
            .map(NativeTerminalSessionWire::from_record)
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_wire_error)?;
        sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        Ok(sessions)
    }

    pub async fn output_page(
        &self,
        owner_window_label: &str,
        request: super::wire::NativeTerminalOutputRequest,
    ) -> Result<NativeTerminalOutputPageWire, NativeRuntimeRegistryError> {
        validate_owner(owner_window_label)?;
        validate_session_id(&request.session_id)?;
        let after_sequence = parse_sequence(&request.after_sequence)?;
        let events = if let Some(manager) = self
            .cached_manager_for_session(owner_window_label, &request.session_id, false)
            .await?
        {
            manager
                .output_page(
                    owner_window_label,
                    &request.session_id,
                    after_sequence,
                    MAX_OUTPUT_PAGE_EVENTS,
                )
                .map_err(map_manager_error)?
        } else {
            self.locate_session(owner_window_label, &request.session_id)?
                .ok_or(NativeRuntimeRegistryError::NotFound)?;
            Vec::new()
        };
        output_page_wire(&request.session_id, after_sequence, events)
    }

    /// Subscribe before reading the manager's live display buffer. The
    /// registry owns the receiver task by an unguessable native subscription
    /// identifier; detaching it never cancels the worker session.
    pub async fn attach(
        &self,
        owner_window_label: &str,
        request: super::wire::NativeTerminalAttachRequest,
        sink: Arc<dyn NativeTerminalOutputSink>,
    ) -> Result<NativeTerminalAttachResult, NativeRuntimeRegistryError> {
        validate_owner(owner_window_label)?;
        validate_session_id(&request.session_id)?;
        let after_sequence = parse_sequence(&request.after_sequence)?;
        let cached_manager = if after_sequence <= i64::MAX as u64 {
            self.cached_manager_for_session(owner_window_label, &request.session_id, false)
                .await?
        } else {
            None
        };
        if let Some(manager) = cached_manager {
            let live = manager
                .attach(
                    owner_window_label,
                    &request.session_id,
                    after_sequence,
                    MAX_OUTPUT_PAGE_EVENTS,
                )
                .map_err(map_manager_error)?;
            let terminal_snapshot = matches!(
                live.record.state,
                super::SessionState::Closed | super::SessionState::Cancelled
            ) || live
                .events
                .last()
                .is_some_and(|event| matches!(event.event, NativeTerminalOutputEvent::End));
            let subscription_id = if terminal_snapshot {
                None
            } else {
                Some(new_uuid().ok_or(NativeRuntimeRegistryError::Unavailable)?)
            };
            let attachment = NativeTerminalAttachmentWire {
                session: NativeTerminalSessionWire::from_record(&live.record)
                    .map_err(map_wire_error)?,
                events: live
                    .events
                    .iter()
                    .map(|event| {
                        NativeTerminalOutputRecordWire::from_record(&request.session_id, event)
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(map_wire_error)?,
                next_sequence: live.next_sequence.to_string(),
                subscription_id: subscription_id.clone(),
            };
            if let Some(subscription_id) = subscription_id {
                self.spawn_subscription(
                    subscription_id,
                    owner_window_label.to_owned(),
                    request.session_id,
                    live.next_sequence,
                    live.subscription,
                    manager,
                    sink,
                );
            }
            return Ok(NativeTerminalAttachResult { attachment });
        }

        // A recovered session has no live feed or transcript to replay.
        let (_, record) = self
            .locate_session(owner_window_label, &request.session_id)?
            .ok_or(NativeRuntimeRegistryError::NotFound)?;
        let events: Vec<NativeTerminalOutputRecord> = Vec::new();
        let next_sequence = events.last().map_or(after_sequence, |event| event.sequence);
        Ok(NativeTerminalAttachResult {
            attachment: NativeTerminalAttachmentWire {
                session: NativeTerminalSessionWire::from_record(&record).map_err(map_wire_error)?,
                events: events
                    .iter()
                    .map(|event| {
                        NativeTerminalOutputRecordWire::from_record(&request.session_id, event)
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(map_wire_error)?,
                next_sequence: next_sequence.to_string(),
                subscription_id: None,
            },
        })
    }

    pub async fn send_input(
        &self,
        owner_window_label: &str,
        request: super::wire::NativeTerminalInputRequest,
    ) -> Result<NativeTerminalInputDeliveryWire, NativeRuntimeRegistryError> {
        validate_owner(owner_window_label)?;
        validate_session_id(&request.session_id)?;
        if request.bytes.len() > MAX_NATIVE_INPUT_BYTES {
            return Err(NativeRuntimeRegistryError::InputTooLarge);
        }
        let Some(manager) = self
            .cached_manager_for_session(owner_window_label, &request.session_id, false)
            .await?
        else {
            self.locate_session(owner_window_label, &request.session_id)?
                .ok_or(NativeRuntimeRegistryError::NotFound)?;
            return Ok(NativeTerminalInputDeliveryWire::Unavailable);
        };
        match manager
            .send_input(owner_window_label, &request.session_id, request.bytes)
            .await
        {
            Ok(()) => Ok(NativeTerminalInputDeliveryWire::Accepted),
            Err(NativeTerminalManagerError::Transport) => {
                Ok(NativeTerminalInputDeliveryWire::Unknown)
            }
            Err(NativeTerminalManagerError::SessionNotRunning) => {
                Ok(NativeTerminalInputDeliveryWire::Unavailable)
            }
            Err(error) => Err(map_manager_error(error)),
        }
    }

    pub async fn cancel(
        &self,
        owner_window_label: &str,
        session_id: &str,
    ) -> Result<NativeTerminalSessionWire, NativeRuntimeRegistryError> {
        validate_owner(owner_window_label)?;
        validate_session_id(session_id)?;
        let manager = self
            .manager_for_session(owner_window_label, session_id)
            .await?;
        let record = manager
            .cancel(owner_window_label, session_id)
            .await
            .map_err(map_manager_error)?;
        NativeTerminalSessionWire::from_record(&record).map_err(map_wire_error)
    }

    /// Stop output forwarding for one owner-bound subscription only. This is
    /// not a process/session cancellation operation.
    pub async fn detach(
        &self,
        owner_window_label: &str,
        request: super::wire::NativeTerminalDetachRequest,
    ) -> Result<(), NativeRuntimeRegistryError> {
        validate_owner(owner_window_label)?;
        validate_session_id(&request.session_id)?;
        if !is_canonical_uuid(&request.subscription_id) {
            return Err(NativeRuntimeRegistryError::InvalidRequest);
        }
        let entry = {
            let mut subscriptions = self.subscriptions.lock();
            let Some(entry) = subscriptions.get(&request.subscription_id) else {
                return Err(NativeRuntimeRegistryError::NotFound);
            };
            if entry.owner_window_label != owner_window_label {
                return Err(NativeRuntimeRegistryError::NotFound);
            }
            if entry.session_id != request.session_id {
                return Err(NativeRuntimeRegistryError::Conflict);
            }
            subscriptions.remove(&request.subscription_id)
        };
        if let Some(mut entry) = entry {
            if let Some(stop) = entry.stop.take() {
                let _ = stop.send(());
            }
            if let Some(task) = entry.task.take() {
                task.abort();
                let _ = task.await;
            }
        }
        Ok(())
    }

    /// Cancel and drain only this app window's managed native sessions. The
    /// manager records actual terminal observations; timeout leaves Unknown and
    /// does not claim physical VM shutdown.
    pub async fn shutdown_for_window(
        &self,
        owner_window_label: &str,
        drain_timeout: Duration,
    ) -> Result<Vec<NativeTerminalSessionWire>, NativeRuntimeRegistryError> {
        validate_owner(owner_window_label)?;
        self.detach_window(owner_window_label).await;
        let records = shutdown_cached_managers(
            owner_window_label,
            drain_timeout,
            self.cached_services()
                .await
                .into_iter()
                .map(|service| service.manager.clone()),
        )
        .await?;
        records
            .values()
            .map(NativeTerminalSessionWire::from_record)
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_wire_error)
    }

    /// Reconcile interrupted records for this window without replaying an
    /// enqueue. The root may call this once when a repository service is first
    /// loaded for a window.
    pub async fn recover_for_window(
        &self,
        owner_window_label: &str,
    ) -> Result<Vec<NativeTerminalSessionWire>, NativeRuntimeRegistryError> {
        validate_owner(owner_window_label)?;
        let _recovery_guard = self.startup_recovery_lock.lock().await;
        let first_recovery = !*self.startup_recovery_complete.lock();
        let paths = existing_session_store_paths(&self.data_paths).map_err(map_bootstrap_error)?;
        let mut records = HashMap::<String, NativeSessionRecord>::new();
        for path in paths {
            let store = NativeTerminalStore::open(&path)
                .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?;
            if first_recovery {
                store
                    .mark_unresolved_unknown()
                    .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?;
            }
            for record in store
                .list_sessions_for_window(owner_window_label)
                .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?
            {
                records.entry(record.session_id.clone()).or_insert(record);
            }
        }
        let mut sessions = records
            .values()
            .map(NativeTerminalSessionWire::from_record)
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_wire_error)?;
        sessions.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        *self.startup_recovery_complete.lock() = true;
        Ok(sessions)
    }

    /// Union all durable unresolved profile IDs from the bounded native
    /// repository namespace. This scan is confined to the private app-data
    /// native root and deliberately remains independent of current policy so a
    /// removed repository cannot silently unfreeze an old profile.
    pub fn unresolved_profile_ids(&self) -> Result<HashSet<String>, NativeRuntimeRegistryError> {
        let paths = existing_session_store_paths(&self.data_paths).map_err(map_bootstrap_error)?;
        let mut profile_ids = self.profile_settings_terminals.unresolved_profile_ids();
        for path in paths {
            let store = NativeTerminalStore::open(&path)
                .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?;
            profile_ids.extend(
                store
                    .unresolved_profile_ids()
                    .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?,
            );
        }
        Ok(profile_ids)
    }

    /// The settings command retains its shared admission guard through acceptance.
    pub(crate) async fn start_profile_settings_terminal(
        &self,
        owner: &str,
        expected_revision: u64,
        profile_id: &str,
        unresolved_profiles: &HashSet<String>,
        _admission: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<super::profile_settings::ProfileSettingsSession, NativeRuntimeRegistryError> {
        validate_owner(owner)?;
        if !self.accepting.load(std::sync::atomic::Ordering::Acquire) {
            return Err(NativeRuntimeRegistryError::ShuttingDown);
        }
        let settings = self
            .settings
            .load()
            .map_err(|_| NativeRuntimeRegistryError::AuthorityUnavailable)?;
        if settings.revision != expected_revision || unresolved_profiles.contains(profile_id) {
            return Err(NativeRuntimeRegistryError::Conflict);
        }
        let mut selected = settings
            .profiles
            .iter()
            .find(|profile| profile.profile_id == profile_id)
            .cloned()
            .ok_or(NativeRuntimeRegistryError::ProfileUnavailable)?;
        let original = selected.clone();
        let resolved = crate::coding_agent::image::resolve_image_reference(
            &selected.image_ref,
            &crate::coding_agent::image::OciImageDigestResolver::default(),
        )
        .await
        .map_err(|_| NativeRuntimeRegistryError::ProfileUnavailable)?;
        selected.image_ref = resolved.reference;
        selected.allowed_image_refs = vec![selected.image_ref.clone()];
        let managed_home_root = self
            .settings
            .managed_profile_home_root()
            .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?;
        let profile = selected
            .to_agent_profile_with_managed_home_root(&managed_home_root)
            .map_err(|_| NativeRuntimeRegistryError::ProfileUnavailable)?
            .validate_and_snapshot(&self.host_limits)
            .map_err(|_| NativeRuntimeRegistryError::ProfileUnavailable)?;
        let lifecycle = self.sidecars.lock_native_jobworkerp_admission().await;
        let source = inspect_current_preparation_source_admitted(&self.sidecars, &lifecycle)
            .map_err(|_| NativeRuntimeRegistryError::SourceUnavailable)?;
        let handle = self
            .jobworkerp_for_generation(source.child_generation_id)
            .await?;
        let parent = self
            .data_paths
            .coding_agent_settings_path()
            .parent()
            .ok_or(NativeRuntimeRegistryError::StorageUnavailable)?
            .canonicalize()
            .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?;
        let workspaces = parent.join("profile-shell-workspaces");
        crate::coding_agent::settings::ensure_owned_private_directory(&workspaces)
            .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?;
        let workspace = workspaces.join(crate::coding_agent::assessment::sha256_hex(
            profile_id.as_bytes(),
        ));
        crate::coding_agent::settings::ensure_owned_private_directory(&workspace)
            .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?;
        let binding = crate::jobworkerp::coding_agent::NativeWorkspaceBinding {
            canonical_host_root: workspace,
            guest_root: "/workspace".into(),
        };
        let protected = vec![
            self.data_paths.db_dir(),
            self.data_paths.coding_agent_settings_path(),
            parent.join("native"),
            workspaces,
        ];
        let spec = super::preparation::profile_settings_worker_spec(
            &profile,
            &binding,
            &source.host_worker_context,
            &protected,
        )
        .map_err(map_preparation_error)?;
        super::preparation::prepare_runtime_tool_homes(&profile).map_err(map_preparation_error)?;
        let matched = handle
            .ensure_native_profile_settings_worker(spec)
            .await
            .map_err(|_| NativeRuntimeRegistryError::SourceUnavailable)?;
        let current = self
            .settings
            .load()
            .map_err(|_| NativeRuntimeRegistryError::AuthorityUnavailable)?;
        let current_source =
            inspect_current_preparation_source_admitted(&self.sidecars, &lifecycle)
                .map_err(|_| NativeRuntimeRegistryError::SourceUnavailable)?;
        if current.revision != expected_revision
            || current
                .profiles
                .iter()
                .find(|profile| profile.profile_id == profile_id)
                != Some(&original)
            || current_source.child_generation_id != source.child_generation_id
            || current_source.source_digest != source.source_digest
            || !self.accepting.load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(NativeRuntimeRegistryError::Conflict);
        }
        let session = self
            .profile_settings_terminals
            .reserve(
                owner,
                profile_id,
                profile.version,
                expected_revision,
                &profile.image_ref,
            )
            .map_err(|_| NativeRuntimeRegistryError::Conflict)?;
        let start = self
            .profile_settings_terminals
            .start_outcome_guard(&session.session_id);
        let dispatched = start.dispatch_marker();
        let stream = handle
            .start_native_profile_settings_terminal_with_dispatch(&matched, || {
                dispatched.store(true, std::sync::atomic::Ordering::Release)
            })
            .await
            .map_err(|_| {
                if dispatched.load(std::sync::atomic::Ordering::Acquire) {
                    NativeRuntimeRegistryError::StartOutcomeUnknown
                } else {
                    NativeRuntimeRegistryError::SourceUnavailable
                }
            })?;
        self.profile_settings_terminals
            .observe(&session.session_id, handle, stream)
            .map_err(|_| NativeRuntimeRegistryError::StartOutcomeUnknown)?;
        start.accepted();
        self.profile_settings_terminals
            .session(owner, &session.session_id)
            .map_err(|_| NativeRuntimeRegistryError::InvalidRecord)
    }

    async fn jobworkerp_for_generation(
        &self,
        generation_id: u64,
    ) -> Result<JobworkerpHandle, NativeRuntimeRegistryError> {
        let snapshot = self
            .sidecars
            .managed_jobworkerp_spawn_snapshot()
            .ok_or(NativeRuntimeRegistryError::SourceUnavailable)?;
        if snapshot.generation_id != generation_id {
            return Err(NativeRuntimeRegistryError::SourceUnavailable);
        }
        if let Some(handle) = self.handles.lock().await.get(&generation_id).cloned() {
            return Ok(handle);
        }
        let endpoint = self
            .sidecars
            .current_endpoints()
            .and_then(|endpoints| endpoints.jobworkerp_url())
            .ok_or(NativeRuntimeRegistryError::SourceUnavailable)?;
        let handle = tokio::time::timeout(
            JOBWORKERP_CONNECT_TIMEOUT,
            JobworkerpHandle::connect(&endpoint),
        )
        .await
        .map_err(|_| NativeRuntimeRegistryError::Unavailable)?
        .map_err(|_| NativeRuntimeRegistryError::Unavailable)?;
        let current = self
            .sidecars
            .managed_jobworkerp_spawn_snapshot()
            .ok_or(NativeRuntimeRegistryError::SourceUnavailable)?;
        if current.generation_id != generation_id {
            return Err(NativeRuntimeRegistryError::SourceUnavailable);
        }
        let mut handles = self.handles.lock().await;
        Ok(handles.entry(generation_id).or_insert(handle).clone())
    }

    async fn probe_runner_service(&self, generation_id: u64) -> bool {
        let _lifecycle_guard = self.sidecars.lock_native_jobworkerp_admission().await;
        let Some(snapshot) = self.sidecars.managed_jobworkerp_spawn_snapshot() else {
            return false;
        };
        if snapshot.generation_id != generation_id {
            return false;
        }
        let Ok(handle) = self.jobworkerp_for_generation(generation_id).await else {
            return false;
        };
        tokio::time::timeout(Duration::from_secs(10), handle.probe_runner_service())
            .await
            .is_ok_and(|result| result.is_ok())
    }

    #[allow(clippy::too_many_arguments)]
    async fn services_for(
        &self,
        identity: &RepositoryIdentity,
        source: &NativePreparationSourceContext,
        repository: &coding_agent_executor::RepositoryId,
        package: &NativePackageSelection,
        runtime: NativePreparationRuntime,
        jobworkerp: JobworkerpHandle,
        expected_policy: &NativeRepositoryPolicySnapshot,
    ) -> Result<(RepositoryServiceKey, Arc<RepositoryServices>), NativeRuntimeRegistryError> {
        let _build = self.service_build_lock.lock().await;
        self.require_approved_package(package)?;
        let configured = configure_repository(&self.data_paths, package, repository)
            .await
            .map_err(map_bootstrap_error)?;
        let current_policy =
            read_repository_policy(&self.data_paths, package).map_err(map_bootstrap_error)?;
        if current_policy.policy_sha256 != expected_policy.policy_sha256 {
            return Err(NativeRuntimeRegistryError::SourceUnavailable);
        }
        let key = repository_service_key(
            identity,
            expected_policy,
            package,
            source,
            configured.trusted_config_digest(),
        );
        if let Some(services) = self.services.lock().await.get(&key).cloned() {
            drop(configured);
            return Ok((key, services));
        }
        let preparer = NativeSessionPreparer::new(
            configured,
            Arc::clone(&self.authority),
            jobworkerp.clone(),
            runtime,
            Arc::clone(&self.shared_admission),
        );
        let manager = preparer.manager();
        let doctor = NativeRuntimeDoctor::new_with_shared_journal_and_accepting(
            Arc::clone(&self.sidecars),
            package.clone(),
            jobworkerp,
            manager.clone(),
            preparer.doctor_store(),
            Arc::clone(&self.accepting),
        );
        let services = Arc::new(RepositoryServices {
            preparer,
            manager,
            doctor,
        });
        self.services
            .lock()
            .await
            .insert(key.clone(), Arc::clone(&services));
        Ok((key, services))
    }

    async fn cached_services(&self) -> Vec<Arc<RepositoryServices>> {
        self.services.lock().await.values().cloned().collect()
    }

    fn durable_records_for_window(
        &self,
        owner_window_label: &str,
        issue_id: Option<&str>,
    ) -> Result<Vec<NativeSessionRecord>, NativeRuntimeRegistryError> {
        let paths = existing_session_store_paths(&self.data_paths).map_err(map_bootstrap_error)?;
        let mut records = Vec::new();
        for path in paths {
            let store = NativeTerminalStore::open(&path)
                .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?;
            let mut repository_records = store
                .list_sessions_for_window(owner_window_label)
                .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?;
            records.extend(repository_records.drain(..).filter(|record| {
                issue_id
                    .is_none_or(|issue_id| record.context.canonical_issue().issue_id() == issue_id)
            }));
        }
        Ok(records)
    }

    /// Find the durable record and its repository partition without relying on
    /// an in-memory manager. This is the read-only path used after restart for
    /// replay and owner checks; it never implies that the native job is live.
    fn locate_session(
        &self,
        owner_window_label: &str,
        session_id: &str,
    ) -> Result<Option<(PathBuf, NativeSessionRecord)>, NativeRuntimeRegistryError> {
        let paths = existing_session_store_paths(&self.data_paths).map_err(map_bootstrap_error)?;
        let mut found = None;
        for path in paths {
            let store = NativeTerminalStore::open(&path)
                .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?;
            let record = store
                .get_session(session_id)
                .map_err(|_| NativeRuntimeRegistryError::StorageUnavailable)?;
            let Some(record) = record else {
                continue;
            };
            if found.is_some() {
                return Err(NativeRuntimeRegistryError::StorageUnavailable);
            }
            if record.context.owner_window_label() != owner_window_label {
                return Err(NativeRuntimeRegistryError::OwnerMismatch);
            }
            found = Some((path, record));
        }
        Ok(found)
    }

    async fn cached_manager_for_session(
        &self,
        owner_window_label: &str,
        session_id: &str,
        allow_starting: bool,
    ) -> Result<Option<NativeTerminalManager>, NativeRuntimeRegistryError> {
        let services = self.cached_services().await;
        resolve_cached_manager(
            owner_window_label,
            session_id,
            services.iter().map(|service| &service.manager),
            allow_starting,
        )
    }

    async fn manager_for_session(
        &self,
        owner_window_label: &str,
        session_id: &str,
    ) -> Result<NativeTerminalManager, NativeRuntimeRegistryError> {
        if let Some(manager) = self
            .cached_manager_for_session(owner_window_label, session_id, true)
            .await?
        {
            return Ok(manager);
        }
        if self
            .locate_session(owner_window_label, session_id)?
            .is_some()
        {
            // A durable record recovered after restart is not evidence of a
            // live manager or cancellable native job.
            Err(NativeRuntimeRegistryError::Unavailable)
        } else {
            Err(NativeRuntimeRegistryError::NotFound)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_subscription(
        &self,
        subscription_id: String,
        owner_window_label: String,
        session_id: String,
        mut cursor: u64,
        mut subscription: NativeTerminalSubscription,
        manager: NativeTerminalManager,
        sink: Arc<dyn NativeTerminalOutputSink>,
    ) {
        let (stop_sender, mut stop_receiver) = oneshot::channel();
        self.subscriptions.lock().insert(
            subscription_id.clone(),
            RegistrySubscription {
                owner_window_label: owner_window_label.clone(),
                session_id: session_id.clone(),
                stop: Some(stop_sender),
                task: None,
            },
        );
        let subscriptions = Arc::clone(&self.subscriptions);
        let task_subscription_id = subscription_id.clone();
        let emitter_subscription_id = subscription_id.clone();
        let cleanup_owner = owner_window_label.clone();
        let cleanup_session = session_id.clone();
        let task = tokio::spawn(async move {
            'forward: loop {
                let next = tokio::select! {
                    _ = &mut stop_receiver => break,
                    result = subscription.recv() => result,
                };
                match next {
                    Ok(record) => {
                        let terminal =
                            matches!(record.event, super::NativeTerminalOutputEvent::End);
                        if emit_output_record(&sink, &emitter_subscription_id, &session_id, &record)
                            .await
                            .is_err()
                        {
                            break;
                        }
                        cursor = record.sequence;
                        if terminal {
                            break;
                        }
                    }
                    Err(NativeTerminalManagerError::OutputGap) => {
                        match replay_after_gap(
                            &manager,
                            &owner_window_label,
                            &session_id,
                            &emitter_subscription_id,
                            cursor,
                            &sink,
                        )
                        .await
                        {
                            Some(ReplayAfterGap::Continue {
                                subscription: next_subscription,
                                cursor: next_cursor,
                            }) => {
                                subscription = next_subscription;
                                cursor = next_cursor;
                            }
                            Some(ReplayAfterGap::Ended) | None => break 'forward,
                        }
                    }
                    Err(_) => break,
                }
            }
            let mut subscriptions = subscriptions.lock();
            if subscriptions
                .get(&task_subscription_id)
                .is_some_and(|entry| {
                    entry.owner_window_label == cleanup_owner && entry.session_id == cleanup_session
                })
            {
                subscriptions.remove(&task_subscription_id);
            }
        });
        if let Some(entry) = self.subscriptions.lock().get_mut(&subscription_id) {
            entry.task = Some(task);
        }
    }

    async fn detach_window(&self, owner_window_label: &str) {
        let ids = self
            .subscriptions
            .lock()
            .iter()
            .filter(|(_, entry)| entry.owner_window_label == owner_window_label)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            let entry = self.subscriptions.lock().remove(&id);
            if let Some(mut entry) = entry {
                if let Some(stop) = entry.stop.take() {
                    let _ = stop.send(());
                }
                if let Some(task) = entry.task.take() {
                    task.abort();
                    let _ = task.await;
                }
            }
        }
    }
}

fn sorted_approvals(
    mut approvals: Vec<NativeRepositoryApprovalSettings>,
) -> Vec<NativeRepositoryApprovalSettings> {
    approvals.sort_by(|left, right| {
        (
            &left.repository.host,
            &left.repository.organization,
            &left.repository.repository,
        )
            .cmp(&(
                &right.repository.host,
                &right.repository.organization,
                &right.repository.repository,
            ))
    });
    approvals
}

fn flatten_native_approvals(
    settings: &crate::coding_agent::settings::CodingAgentSettings,
) -> Vec<NativeRepositoryApprovalSettings> {
    sorted_approvals(
        settings
            .native_repository_approvals
            .clone()
            .unwrap_or_default(),
    )
}

fn effective_setup_approvals(
    settings: &crate::coding_agent::settings::CodingAgentSettings,
    policy: NativeRepositoryPolicySnapshot,
) -> Vec<NativeRepositoryApprovalSettings> {
    if settings.native_repository_approvals.is_some() {
        return flatten_native_approvals(settings);
    }
    let approvals = policy
        .repositories
        .into_iter()
        .map(|entry| NativeRepositoryApprovalSettings {
            repository: RepositoryIdentitySettings {
                host: entry.repository.host,
                organization: entry.repository.owner,
                repository: entry.repository.name,
            },
            allowed_base_branches: entry.allowed_base_branches,
        })
        .collect::<Vec<_>>();
    sorted_approvals(approvals)
}

fn map_settings_error_for_native_setup(error: SettingsError) -> NativeRuntimeRegistryError {
    match error {
        SettingsError::RevisionConflict { .. } => NativeRuntimeRegistryError::Conflict,
        _ => NativeRuntimeRegistryError::StorageUnavailable,
    }
}

enum ReplayAfterGap {
    Continue {
        subscription: NativeTerminalSubscription,
        cursor: u64,
    },
    Ended,
}

async fn shutdown_cached_managers(
    owner_window_label: &str,
    drain_timeout: Duration,
    managers: impl IntoIterator<Item = NativeTerminalManager>,
) -> Result<HashMap<String, NativeSessionRecord>, NativeRuntimeRegistryError> {
    let deadline = tokio::time::Instant::now() + drain_timeout;
    let mut shutdown_tasks = tokio::task::JoinSet::new();
    for manager in managers {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let owner_window_label = owner_window_label.to_owned();
        shutdown_tasks.spawn(async move {
            let result = manager
                .shutdown_for_window(&owner_window_label, remaining)
                .await;
            (manager, result)
        });
    }
    let mut records = HashMap::<String, (bool, NativeSessionRecord)>::new();
    let mut shutdown_error = None;
    while let Some(joined) = shutdown_tasks.join_next().await {
        match joined {
            Ok((manager, Ok(service_records))) => {
                for record in service_records {
                    let managed = match manager
                        .session_management_ownership(owner_window_label, &record.session_id)
                    {
                        Ok(ownership) => ownership != NativeSessionManagementOwnership::None,
                        Err(error) => {
                            shutdown_error.get_or_insert_with(|| map_manager_error(error));
                            false
                        }
                    };
                    let should_replace = records
                        .get(&record.session_id)
                        .is_none_or(|(currently_managed, _)| managed || !currently_managed);
                    if should_replace {
                        records.insert(record.session_id.clone(), (managed, record));
                    }
                }
            }
            Ok((_manager, Err(error))) => {
                shutdown_error.get_or_insert_with(|| map_manager_error(error));
            }
            Err(_) => {
                shutdown_error.get_or_insert(NativeRuntimeRegistryError::Unavailable);
            }
        }
    }
    if let Some(error) = shutdown_error {
        return Err(error);
    }
    Ok(records
        .into_iter()
        .map(|(session_id, (_, record))| (session_id, record))
        .collect())
}

async fn replay_after_gap(
    manager: &NativeTerminalManager,
    owner_window_label: &str,
    session_id: &str,
    subscription_id: &str,
    mut cursor: u64,
    sink: &Arc<dyn NativeTerminalOutputSink>,
) -> Option<ReplayAfterGap> {
    loop {
        let attachment = manager
            .attach(
                owner_window_label,
                session_id,
                cursor,
                MAX_OUTPUT_PAGE_EVENTS,
            )
            .ok()?;
        let terminal_record = matches!(
            attachment.record.state,
            super::SessionState::Closed | super::SessionState::Cancelled
        );
        let mut terminal = false;
        for record in &attachment.events {
            terminal |= matches!(record.event, super::NativeTerminalOutputEvent::End);
            emit_output_record(sink, subscription_id, session_id, record)
                .await
                .ok()?;
            cursor = record.sequence;
        }
        let has_more = attachment.events.len() == MAX_OUTPUT_PAGE_EVENTS;
        let subscription = attachment.subscription;
        if terminal {
            drop(subscription);
            return Some(ReplayAfterGap::Ended);
        }
        if !has_more {
            if terminal_record {
                drop(subscription);
                return Some(ReplayAfterGap::Ended);
            }
            return Some(ReplayAfterGap::Continue {
                subscription,
                cursor,
            });
        }
        drop(subscription);
    }
}

/// Durable repository-service entries can share a session database while only
/// one manager owns this process's output/input stream. Inspect every candidate
/// so HashMap iteration order cannot select a durable-only manager over the
/// actual in-memory owner. Each manager performs its strict owner-bound store
/// read before consulting its RAM ownership state.
fn resolve_cached_manager<'a>(
    owner_window_label: &str,
    session_id: &str,
    managers: impl IntoIterator<Item = &'a NativeTerminalManager>,
    allow_starting: bool,
) -> Result<Option<NativeTerminalManager>, NativeRuntimeRegistryError> {
    let mut selected = None;
    for manager in managers {
        let ownership = match manager.session_management_ownership(owner_window_label, session_id) {
            Ok(ownership) => ownership,
            Err(NativeTerminalManagerError::NotFound) => continue,
            Err(error) => return Err(map_manager_error(error)),
        };
        if ownership == NativeSessionManagementOwnership::Managed
            || (allow_starting && ownership == NativeSessionManagementOwnership::Starting)
        {
            if selected.is_some() {
                return Err(NativeRuntimeRegistryError::Conflict);
            }
            selected = Some(manager.clone());
        }
    }
    Ok(selected)
}

async fn emit_output_record(
    sink: &Arc<dyn NativeTerminalOutputSink>,
    subscription_id: &str,
    session_id: &str,
    record: &NativeTerminalOutputRecord,
) -> Result<(), NativeRuntimeRegistryError> {
    let record =
        NativeTerminalOutputRecordWire::from_record(session_id, record).map_err(map_wire_error)?;
    let event = NativeTerminalOutputEventWire {
        subscription_id: subscription_id.to_owned(),
        record,
    };
    tokio::time::timeout(OUTPUT_SINK_TIMEOUT, sink.emit(event))
        .await
        .map_err(|_| NativeRuntimeRegistryError::SubscriptionUnavailable)?
        .map_err(|_| NativeRuntimeRegistryError::SubscriptionUnavailable)
}

fn output_page_wire(
    session_id: &str,
    after_sequence: u64,
    events: Vec<NativeTerminalOutputRecord>,
) -> Result<NativeTerminalOutputPageWire, NativeRuntimeRegistryError> {
    let next_sequence = events
        .last()
        .map(|event| event.sequence)
        .unwrap_or(after_sequence);
    Ok(NativeTerminalOutputPageWire {
        events: events
            .iter()
            .map(|event| NativeTerminalOutputRecordWire::from_record(session_id, event))
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_wire_error)?,
        next_sequence: next_sequence.to_string(),
    })
}

fn profile_has_unsupported_credential(
    profile: &crate::coding_agent::settings::AgentProfileSettings,
) -> bool {
    profile.credential_reference.is_some()
}

fn repository_identity_matches(
    identity: &RepositoryIdentity,
    repository: &coding_agent_executor::RepositoryId,
) -> bool {
    identity.host == repository.host
        && identity.organization == repository.owner
        && identity.repository == repository.name
}

fn repository_id(identity: &RepositoryIdentity) -> coding_agent_executor::RepositoryId {
    coding_agent_executor::RepositoryId {
        host: identity.host.clone(),
        owner: identity.organization.clone(),
        name: identity.repository.clone(),
    }
}

fn repository_service_key(
    identity: &RepositoryIdentity,
    policy: &NativeRepositoryPolicySnapshot,
    package: &NativePackageSelection,
    source: &NativePreparationSourceContext,
    config_sha256: &str,
) -> RepositoryServiceKey {
    RepositoryServiceKey {
        host: identity.host.clone(),
        organization: identity.organization.clone(),
        repository: identity.repository.clone(),
        policy_sha256: policy.policy_sha256.clone(),
        config_sha256: config_sha256.to_owned(),
        package_manifest_sha256: package.manifest_sha256().to_owned(),
        source_digest: source.source_digest.clone(),
        child_generation_id: source.child_generation_id,
    }
}

fn validate_selected_base_branch(
    branch: &str,
    allowed: &[String],
) -> Result<(), NativeRuntimeRegistryError> {
    if branch.len() > 256 || branch.chars().any(char::is_control) {
        return Err(NativeRuntimeRegistryError::InvalidRequest);
    }
    if allowed.is_empty()
        || (!branch.is_empty() && !allowed.iter().any(|candidate| candidate == branch))
    {
        return Err(NativeRuntimeRegistryError::BranchUnavailable);
    }
    Ok(())
}

fn validate_issue_id(issue_id: &str) -> Result<(), NativeRuntimeRegistryError> {
    if issue_id.is_empty()
        || issue_id.len() > MAX_REGISTRY_ISSUE_ID_BYTES
        || issue_id.chars().any(char::is_control)
    {
        Err(NativeRuntimeRegistryError::InvalidRequest)
    } else {
        Ok(())
    }
}

fn validate_owner(owner: &str) -> Result<(), NativeRuntimeRegistryError> {
    if owner.is_empty() || owner.len() > 256 || owner.chars().any(char::is_control) {
        Err(NativeRuntimeRegistryError::InvalidRequest)
    } else {
        Ok(())
    }
}

fn validate_session_id(session_id: &str) -> Result<(), NativeRuntimeRegistryError> {
    if is_canonical_uuid(session_id) {
        Ok(())
    } else {
        Err(NativeRuntimeRegistryError::InvalidRequest)
    }
}

pub(crate) fn parse_sequence(sequence: &str) -> Result<u64, NativeRuntimeRegistryError> {
    if sequence.is_empty()
        || (sequence.len() > 1 && sequence.starts_with('0'))
        || !sequence.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(NativeRuntimeRegistryError::InvalidRequest);
    }
    let parsed = sequence
        .parse::<u64>()
        .map_err(|_| NativeRuntimeRegistryError::InvalidRequest)?;
    Ok(parsed)
}

fn source_blocker(error: NativeRuntimeDoctorError) -> &'static str {
    match error {
        NativeRuntimeDoctorError::ManagedChildUnavailable => {
            "The managed local jobworkerp runtime is unavailable."
        }
        NativeRuntimeDoctorError::SdkSourceChanged => {
            "The selected native SDK source changed. Restart the local worker and try again."
        }
        NativeRuntimeDoctorError::SdkSourceUnavailable
        | NativeRuntimeDoctorError::SdkConfigUnavailable
        | NativeRuntimeDoctorError::SdkPathOverrideUnavailable
        | NativeRuntimeDoctorError::SdkContextMismatch => {
            "The selected native SDK configuration source is unavailable or unsafe."
        }
        _ => "The managed native runtime source could not be verified.",
    }
}

fn map_bootstrap_error(error: NativeBootstrapUnavailable) -> NativeRuntimeRegistryError {
    match error {
        NativeBootstrapUnavailable::RepositoryNotApproved => {
            NativeRuntimeRegistryError::RepositoryUnavailable
        }
        NativeBootstrapUnavailable::CredentialUnavailable => {
            NativeRuntimeRegistryError::RepositoryUnavailable
        }
        NativeBootstrapUnavailable::StorageUnavailable => {
            NativeRuntimeRegistryError::StorageUnavailable
        }
        _ => NativeRuntimeRegistryError::Unavailable,
    }
}

fn map_preparation_error(error: NativePreparationError) -> NativeRuntimeRegistryError {
    match error {
        NativePreparationError::InvalidRequest | NativePreparationError::InvalidPrompt => {
            NativeRuntimeRegistryError::InvalidRequest
        }
        NativePreparationError::RuntimeContextUnavailable => {
            NativeRuntimeRegistryError::SourceUnavailable
        }
        NativePreparationError::AuthorityUnavailable | NativePreparationError::AuthorityChanged => {
            NativeRuntimeRegistryError::AuthorityUnavailable
        }
        NativePreparationError::RepositoryMismatch => {
            NativeRuntimeRegistryError::RepositoryUnavailable
        }
        NativePreparationError::UnapprovedBaseBranch => {
            NativeRuntimeRegistryError::BranchUnavailable
        }
        NativePreparationError::UnsupportedCliProfile
        | NativePreparationError::UnsupportedProfileCredentialReference => {
            NativeRuntimeRegistryError::ProfileUnavailable
        }
        NativePreparationError::InvalidAuthoritySnapshot => {
            NativeRuntimeRegistryError::AuthorityUnavailable
        }
        NativePreparationError::ProfileHomeUnavailable => {
            NativeRuntimeRegistryError::ProfileHomeUnavailable
        }
        NativePreparationError::UnsafePersistentMount
        | NativePreparationError::PreviewUnavailable
        | NativePreparationError::IdentityUnavailable
        | NativePreparationError::WorkspaceUnavailable
        | NativePreparationError::WorkerUnavailable
        | NativePreparationError::InvalidContext => NativeRuntimeRegistryError::PreparationFailed,
    }
}

fn map_observation_error(error: NativeRuntimeDoctorError) -> NativeRuntimeRegistryError {
    match error {
        NativeRuntimeDoctorError::NotAccepting => NativeRuntimeRegistryError::ShuttingDown,
        _ => NativeRuntimeRegistryError::RuntimeObservationFailed,
    }
}

fn map_start_error(error: NativeRuntimeDoctorError) -> NativeRuntimeRegistryError {
    match error {
        NativeRuntimeDoctorError::NotAccepting => NativeRuntimeRegistryError::ShuttingDown,
        NativeRuntimeDoctorError::StartOutcomeUnknown => {
            NativeRuntimeRegistryError::StartOutcomeUnknown
        }
        NativeRuntimeDoctorError::ProbeRejected | NativeRuntimeDoctorError::ProbeOutcomeUnknown => {
            NativeRuntimeRegistryError::RuntimeNotObserved
        }
        NativeRuntimeDoctorError::PreviewUnavailable => NativeRuntimeRegistryError::Conflict,
        NativeRuntimeDoctorError::ManagedChildUnavailable
        | NativeRuntimeDoctorError::SdkSourceChanged
        | NativeRuntimeDoctorError::SdkSourceUnavailable
        | NativeRuntimeDoctorError::SdkConfigUnavailable
        | NativeRuntimeDoctorError::SdkPathOverrideUnavailable
        | NativeRuntimeDoctorError::SdkContextMismatch
        | NativeRuntimeDoctorError::RuntimeImageUnavailable => {
            NativeRuntimeRegistryError::SourceUnavailable
        }
        NativeRuntimeDoctorError::JournalUnavailable => {
            NativeRuntimeRegistryError::StorageUnavailable
        }
        NativeRuntimeDoctorError::InvalidRequest | NativeRuntimeDoctorError::TaskFailed => {
            NativeRuntimeRegistryError::InvalidRequest
        }
    }
}

fn map_manager_error(error: NativeTerminalManagerError) -> NativeRuntimeRegistryError {
    match error {
        NativeTerminalManagerError::InvalidRequest => NativeRuntimeRegistryError::InvalidRequest,
        NativeTerminalManagerError::AdmissionClosed => NativeRuntimeRegistryError::ShuttingDown,
        NativeTerminalManagerError::NotFound => NativeRuntimeRegistryError::NotFound,
        NativeTerminalManagerError::OwnerMismatch => NativeRuntimeRegistryError::OwnerMismatch,
        NativeTerminalManagerError::Conflict => NativeRuntimeRegistryError::Conflict,
        NativeTerminalManagerError::WorkerMismatch
        | NativeTerminalManagerError::AuthorityUnavailable
        | NativeTerminalManagerError::AuthorityChanged
        | NativeTerminalManagerError::SessionNotRunning => NativeRuntimeRegistryError::Unavailable,
        NativeTerminalManagerError::OutputGap => {
            NativeRuntimeRegistryError::SubscriptionUnavailable
        }
        NativeTerminalManagerError::Transport => NativeRuntimeRegistryError::Unavailable,
        NativeTerminalManagerError::Store(_) | NativeTerminalManagerError::Output(_) => {
            NativeRuntimeRegistryError::StorageUnavailable
        }
    }
}

fn map_wire_error(_: NativeTerminalWireError) -> NativeRuntimeRegistryError {
    NativeRuntimeRegistryError::InvalidRecord
}

pub(super) fn new_uuid() -> Option<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Some(format!(
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

#[cfg(all(test, unix))]
#[path = "../../tests/fixtures/native_interactive_rpc.rs"]
pub(crate) mod native_interactive_rpc_test_fixture;

#[cfg(all(test, unix))]
pub(crate) mod native_terminal_package_test_fixture {
    use crate as lookback_tauri_lib;

    include!("../../tests/fixtures/native_terminal_package.rs");
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn selected_base_allows_remote_head_and_rejects_unapproved_or_malformed_overrides() {
        let allowed = vec!["trunk".to_owned()];
        assert!(super::validate_selected_base_branch("", &allowed).is_ok());
        assert!(super::validate_selected_base_branch("trunk", &allowed).is_ok());
        assert_eq!(
            super::validate_selected_base_branch("main", &allowed),
            Err(super::NativeRuntimeRegistryError::BranchUnavailable)
        );
        for branch in ["trunk\n", "trunk\0", &"a".repeat(257)] {
            assert_eq!(
                super::validate_selected_base_branch(branch, &allowed),
                Err(super::NativeRuntimeRegistryError::InvalidRequest)
            );
        }
        assert_eq!(
            super::validate_selected_base_branch("", &[]),
            Err(super::NativeRuntimeRegistryError::BranchUnavailable)
        );
    }

    use std::{
        collections::HashSet,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex as StdMutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use async_trait::async_trait;
    use coding_agent_executor::{
        BaseResolutionReceipt, PROTOCOL_VERSION, PreparedWorkspace, RepositoryId,
    };
    use jobworkerp_client::jobworkerp::data::{ResultOutputItem, result_output_item::Item};
    use prost::Message;
    use serde_json::json;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio::sync::Notify;

    use crate::native_terminal::wire::NativeTerminalEventWire;
    use crate::{
        coding_agent::{
            AgentProfile, AgentProfileSnapshot, CloneTransport, ForgeKind, HostResourceLimits,
            RepositoryHostConfig, RepositoryResolution, ResolvedRepository, SandboxResources,
            resolve_repository,
            settings::{
                CodingAgentSettings, NativeRepositoryApprovalSettings, RepositoryHostSettings,
                RepositoryIdentitySettings,
            },
        },
        grpc::proto::jobworkerp::runner::sandbox::{
            SandboxExecExit, SandboxExecOutput, SandboxExecResult,
            sandbox_exec_result::Result as SandboxResult,
        },
        jobworkerp::{
            JobworkerpHandle,
            coding_agent::{
                HostWorkerContext, NativeInteractiveWorkerSpec, NativeWorkspaceBinding,
            },
        },
        native_terminal::{
            CanonicalIssueSnapshot, NativeAuthorityError, NativeAuthoritySnapshot,
            NativeSessionAuthority, NativeSessionContext, NativeSessionContextInput,
            NativeTerminalManager, NativeTerminalManagerError, NativeTerminalOutputStore,
            NativeTerminalPreview, NativeTerminalStore, NativeWorkerBinding, SessionState,
            launch::{NativeAgentKind, NativeCliProfile, WorkspaceMountBinding, build_launch_plan},
        },
        sidecar::{SidecarConfig, Sidecars},
    };

    use super::native_interactive_rpc_test_fixture as native_interactive_rpc;
    use super::*;

    const TEST_OWNER: &str = "main";
    const TEST_IMAGE: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TEST_CLI_VERSION: &str = "1.18.3";

    #[test]
    fn replay_cursor_accepts_the_full_canonical_u64_decimal_range() {
        assert_eq!(parse_sequence(&u64::MAX.to_string()).unwrap(), u64::MAX);
        assert!(parse_sequence("18446744073709551616").is_err());
        assert!(parse_sequence("00").is_err());
    }

    #[tokio::test]
    async fn cached_manager_selection_uses_ram_owner_and_cold_cancel_never_deletes() {
        let setup = manager_setup().await;
        let old_manager = reopen_manager(&setup);
        let cold_manager = reopen_manager(&setup);
        let output_gate = setup.fixture.block_next_output();
        set_clean_result_stream(&setup.fixture, 143);

        let started = setup
            .manager
            .start(
                TEST_OWNER,
                setup.context.preview().preview_id(),
                setup.context.preview().preview_digest(),
            )
            .await
            .unwrap();
        assert_eq!(started.state, SessionState::Running);
        output_gate.wait_until_entered().await;

        let selected = resolve_cached_manager(
            TEST_OWNER,
            setup.context.session_id(),
            [&old_manager, &setup.manager],
            false,
        )
        .unwrap()
        .expect("the process that opened the native stream owns this session");
        assert!(matches!(
            resolve_cached_manager(
                "another-window",
                setup.context.session_id(),
                [&old_manager, &setup.manager],
                false,
            ),
            Err(NativeRuntimeRegistryError::OwnerMismatch)
        ));
        assert!(matches!(
            old_manager
                .send_input(TEST_OWNER, setup.context.session_id(), b"old".to_vec())
                .await,
            Err(NativeTerminalManagerError::SessionNotRunning)
        ));
        selected
            .send_input(TEST_OWNER, setup.context.session_id(), b"live".to_vec())
            .await
            .unwrap();

        assert!(
            resolve_cached_manager(
                TEST_OWNER,
                setup.context.session_id(),
                [&old_manager, &cold_manager],
                true,
            )
            .unwrap()
            .is_none()
        );
        assert!(matches!(
            cold_manager
                .cancel(TEST_OWNER, setup.context.session_id())
                .await,
            Err(NativeTerminalManagerError::SessionNotRunning)
        ));
        assert_eq!(setup.fixture.counts().job_delete_calls, 0);

        assert!(matches!(
            selected
                .cancel(TEST_OWNER, setup.context.session_id())
                .await,
            Err(NativeTerminalManagerError::Transport)
        ));
        assert_eq!(setup.fixture.counts().job_delete_calls, 1);
        output_gate.resume();
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let record = setup
                    .manager
                    .session(TEST_OWNER, setup.context.session_id())
                    .unwrap()
                    .unwrap();
                if matches!(
                    record.state,
                    SessionState::Cancelled | SessionState::Unknown
                ) {
                    break record;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the gated real output stream is drained after cancellation");
        assert!(matches!(
            terminal.state,
            SessionState::Cancelled | SessionState::Unknown
        ));
    }

    #[tokio::test]
    async fn cached_start_reservation_preserves_cancel_before_job_acceptance() {
        let setup = manager_setup().await;
        let older_manager = reopen_manager(&setup);
        let enqueue_gate = setup.fixture.block_next_enqueue();
        let manager = setup.manager.clone();
        let preview_id = setup.context.preview().preview_id().to_owned();
        let preview_digest = setup.context.preview().preview_digest().to_owned();
        let start = tokio::spawn(async move {
            manager
                .start(TEST_OWNER, &preview_id, &preview_digest)
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            enqueue_gate.wait_until_entered(),
        )
        .await
        .expect("the start request reaches the pending acceptance boundary");

        let selected = resolve_cached_manager(
            TEST_OWNER,
            setup.context.session_id(),
            [&older_manager, &setup.manager],
            true,
        )
        .unwrap()
        .expect("the start-RPC owner retains pending cancellation authority");
        let pending = selected
            .cancel(TEST_OWNER, setup.context.session_id())
            .await
            .unwrap();
        assert_eq!(pending.native_job_id, None);
        assert!(pending.cancel_requested);
        assert_eq!(setup.fixture.counts().job_delete_calls, 0);

        enqueue_gate.resume();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), start)
                .await
                .unwrap()
                .unwrap(),
            Err(NativeTerminalManagerError::Transport)
        ));
        assert_eq!(setup.fixture.counts().job_delete_calls, 1);
    }

    #[tokio::test]
    async fn quit_closing_shared_admission_rejects_a_start_queued_behind_the_barrier() {
        let setup = manager_setup().await;
        let accepting = Arc::new(AtomicBool::new(true));
        let admission = setup.manager.admission_lock();
        let barrier_guard = admission.lock().await;
        let manager = setup.manager.clone();
        let accepting_for_start = Arc::clone(&accepting);
        let waiting_start = tokio::spawn(async move {
            manager
                .lock_admission_if_accepting(&accepting_for_start)
                .await
        });

        accepting.store(false, Ordering::Release);
        drop(barrier_guard);

        assert!(waiting_start.await.unwrap().is_err());
        assert_eq!(setup.fixture.counts().job_enqueue_calls, 0);
        assert!(
            setup
                .manager
                .session(TEST_OWNER, setup.context.session_id())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn registry_shutdown_merges_the_live_owners_late_unknown_record() {
        let setup = manager_setup().await;
        let cold_manager = reopen_manager(&setup);
        let enqueue_gate = setup.fixture.block_next_enqueue();
        set_clean_result_stream(&setup.fixture, 7);
        let manager = setup.manager.clone();
        let preview_id = setup.context.preview().preview_id().to_owned();
        let preview_digest = setup.context.preview().preview_digest().to_owned();
        let start = tokio::spawn(async move {
            manager
                .start(TEST_OWNER, &preview_id, &preview_digest)
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            enqueue_gate.wait_until_entered(),
        )
        .await
        .expect("the start RPC reaches its pending acceptance boundary");

        let returned = shutdown_cached_managers(
            TEST_OWNER,
            std::time::Duration::from_millis(30),
            [cold_manager, setup.manager.clone()],
        )
        .await
        .unwrap();
        let returned = returned
            .values()
            .find(|record| record.session_id == setup.context.session_id())
            .unwrap();
        assert_eq!(returned.state, SessionState::Unknown);
        assert!(returned.cancel_requested);
        assert_eq!(returned.native_job_id, None);
        assert_eq!(setup.fixture.counts().job_delete_calls, 0);

        enqueue_gate.resume();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), start)
                .await
                .unwrap()
                .unwrap(),
            Err(NativeTerminalManagerError::Transport)
        ));
        assert_eq!(setup.fixture.counts().job_delete_calls, 1);
    }

    #[tokio::test]
    async fn replay_gap_end_terminates_the_owned_subscription_exactly_once() {
        let setup = manager_setup().await;
        let output_gate = setup.fixture.block_next_output();
        set_many_output_stream(&setup.fixture, 70);
        setup
            .manager
            .start(
                TEST_OWNER,
                setup.context.preview().preview_id(),
                setup.context.preview().preview_digest(),
            )
            .await
            .unwrap();
        output_gate.wait_until_entered().await;
        let attachment = setup
            .manager
            .attach(
                TEST_OWNER,
                setup.context.session_id(),
                0,
                MAX_OUTPUT_PAGE_EVENTS,
            )
            .unwrap();
        let sink = Arc::new(BlockingOutputSink::default());
        let registry = test_registry(setup.root.path());
        let subscription_id = "00000000-0000-4000-8000-000000000099".to_owned();
        registry.spawn_subscription(
            subscription_id.clone(),
            TEST_OWNER.to_owned(),
            setup.context.session_id().to_owned(),
            attachment.next_sequence,
            attachment.subscription,
            setup.manager.clone(),
            sink.clone(),
        );

        output_gate.resume();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sink.first_event.notified(),
        )
        .await
        .expect("the native stream reaches the subscription sink");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if setup
                    .manager
                    .session(TEST_OWNER, setup.context.session_id())
                    .unwrap()
                    .unwrap()
                    .state
                    == SessionState::Closed
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the result stream is durably drained through End");
        sink.release.notify_one();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !registry.subscriptions.lock().contains_key(&subscription_id) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("replay of End terminates and removes the subscription task");
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 72);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.record.event == NativeTerminalEventWire::End)
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .enumerate()
                .all(|(index, event)| { event.record.sequence == (index as u64 + 1).to_string() })
        );
    }

    #[derive(Clone)]
    struct TestAuthority(Arc<StdMutex<NativeAuthoritySnapshot>>);

    #[async_trait]
    impl NativeSessionAuthority for TestAuthority {
        async fn read_current(
            &self,
            _expected: &NativeSessionContext,
        ) -> Result<NativeAuthoritySnapshot, NativeAuthorityError> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    struct ManagerSetup {
        _server: native_interactive_rpc::TestServer,
        fixture: native_interactive_rpc::RpcFixture,
        handle: JobworkerpHandle,
        context: NativeSessionContext,
        authority: TestAuthority,
        manager: NativeTerminalManager,
        root: tempfile::TempDir,
    }

    async fn manager_setup() -> ManagerSetup {
        let root = tempfile::tempdir().unwrap();
        private_dir(root.path());
        let workspace_root = root.path().join("workspace-mount");
        private_dir(&workspace_root);
        let fixture = native_interactive_rpc::RpcFixture::default();
        let server = native_interactive_rpc::start_server(fixture.clone()).await;
        let handle = JobworkerpHandle::connect(&server.url).await.unwrap();
        let matched = handle
            .ensure_native_interactive_worker(worker_spec(&workspace_root))
            .await
            .unwrap();
        let worker_name = fixture.created_workers()[0].name.clone();
        let context = make_test_context(root.path(), &matched, &worker_name);
        let authority = TestAuthority(Arc::new(StdMutex::new(NativeAuthoritySnapshot::from_host(
            context.canonical_issue().clone(),
            context.repository().clone(),
            context.profile().clone(),
            context.preview().expected_settings_revision(),
        ))));
        let manager = new_manager(root.path(), handle.clone(), authority.clone());
        manager.register_preview(context.clone(), &matched).unwrap();

        ManagerSetup {
            _server: server,
            fixture,
            handle,
            context,
            authority,
            manager,
            root,
        }
    }

    fn private_dir(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn new_manager(
        root: &Path,
        handle: JobworkerpHandle,
        authority: TestAuthority,
    ) -> NativeTerminalManager {
        let app_root = root.join("app-private");
        private_dir(&app_root);
        NativeTerminalManager::new(
            Arc::new(NativeTerminalStore::open(&app_root.join("native-terminal.sqlite3")).unwrap()),
            Arc::new(
                NativeTerminalOutputStore::open(&app_root.join("native-terminal-output.sqlite3"))
                    .unwrap(),
            ),
            handle,
            Arc::new(authority),
            Arc::new(AsyncMutex::new(())),
        )
    }

    fn reopen_manager(setup: &ManagerSetup) -> NativeTerminalManager {
        new_manager(
            setup.root.path(),
            setup.handle.clone(),
            setup.authority.clone(),
        )
    }

    fn test_repository() -> ResolvedRepository {
        let config = RepositoryHostConfig {
            label_host: "forge.example".into(),
            forge: ForgeKind::Gitea,
            api_base_url: "https://forge.example/api/v1".into(),
            clone_transport: CloneTransport::Https {
                base_url: "https://forge.example/git".into(),
            },
            read_credential_ref: None,
            publication_credential_ref: None,
        };
        match resolve_repository(&["repo:forge.example/team/repo".into()], &[config], None) {
            RepositoryResolution::Ready(repository) => repository,
            other => panic!("unexpected repository resolution: {other:?}"),
        }
    }

    fn profile(root: &Path, repository: &ResolvedRepository) -> AgentProfileSnapshot {
        let image_ref = format!("registry.example/agents/opencode@{TEST_IMAGE}");
        AgentProfile {
            profile_id: "opencode-native".into(),
            version: 3,
            display_name: "OpenCode".into(),
            image_ref: image_ref.clone(),
            allowed_image_refs: vec![image_ref],
            command: vec!["opencode".into(), "run".into()],
            container_home: "/home/codespace".into(),
            managed_home_overlay: None,
            runtime_environment: Default::default(),
            network_access: Default::default(),
            resources: SandboxResources {
                cpu_millis: 1000,
                memory_mib: 2048,
                root_disk_mib: 8192,
                max_parallel_runs: 1,
                use_static: true,
            },
            mount_allowed_roots: vec![root.to_path_buf()],
            persistent_mounts: Vec::new(),
            allowed_repositories: vec![repository.identity.clone()],
            credential_reference: None,
        }
        .validate_and_snapshot(&HostResourceLimits {
            cpu_millis: 4000,
            memory_mib: 8192,
            root_disk_mib: 32768,
            max_parallel_runs: 2,
        })
        .unwrap()
    }

    fn worker_spec(workspace_root: &Path) -> NativeInteractiveWorkerSpec {
        let root = workspace_root.to_string_lossy().into_owned();
        let image_ref = format!("registry.example/agents/opencode@{TEST_IMAGE}");
        NativeInteractiveWorkerSpec {
            native_profile_id: "opencode-native".into(),
            repository_id: RepositoryId {
                host: "forge.example".into(),
                owner: "team".into(),
                name: "repo".into(),
            },
            use_static: true,
            workspace_binding: NativeWorkspaceBinding {
                canonical_host_root: workspace_root.to_path_buf(),
                guest_root: "/workspace".into(),
            },
            runner_settings_json: json!({
                "vm": {
                    "image": image_ref,
                    "cpus": 1,
                    "memory_mib": 2048,
                    "root_disk_mib": 8192,
                    "mounts": [{
                        "host_path": root,
                        "guest_path": "/workspace",
                        "writable": true,
                        "executable": true
                    }],
                    "env": {"HOME": "/home/codespace"}
                },
                "allowed_images": [image_ref],
                "allowed_host_mounts": [{
                    "host_root": root,
                    "allow_write": true,
                    "allow_exec": true
                }],
                "network": {"enabled": false}
            }),
            host_context: HostWorkerContext {
                microsandbox_sdk_home: PathBuf::from("/home/test/.local/share/lookback/msb"),
                backend_config_path: PathBuf::from(
                    "/home/test/.local/share/lookback/msb/config.json",
                ),
            },
        }
    }

    fn make_test_context(
        root: &Path,
        matched: &crate::jobworkerp::coding_agent::NativeWorkerMatch,
        worker_name: &str,
    ) -> NativeSessionContext {
        let repository = test_repository();
        let workspace_root = root.join("workspace-mount");
        let workspace_path = workspace_root.join("issue-42");
        private_dir(&workspace_path);
        let repository_id = RepositoryId {
            host: repository.identity.host.clone(),
            owner: repository.identity.organization.clone(),
            name: repository.identity.repository.clone(),
        };
        let session_id = "00000000-0000-4000-8000-000000000001".to_owned();
        let attempt_id = "00000000-0000-4000-8000-000000000002".to_owned();
        let issue = CanonicalIssueSnapshot::new(
            "issue-42",
            11,
            "Run the native interactive agent",
            "Keep the exact issue body.\nSecond line.",
            vec![
                "Preserve the exit status".into(),
                "Keep raw terminal output".into(),
            ],
        )
        .unwrap();
        let profile = profile(&workspace_root, &repository);
        let base_commit = "b".repeat(40);
        let base_resolution_receipt = BaseResolutionReceipt {
            run_id: session_id.clone(),
            attempt_id: attempt_id.clone(),
            operation_id: "00000000-0000-4000-8000-000000000090".into(),
            repository: repository_id.clone(),
            base_branch: "main".into(),
            base_commit: base_commit.clone(),
            trusted_config_digest: "c".repeat(64),
            protocol_version: PROTOCOL_VERSION,
            executor_version: "0.2.0".into(),
        };
        let prepared_workspace = PreparedWorkspace {
            run_id: session_id.clone(),
            attempt_id: attempt_id.clone(),
            repository: repository_id,
            base_branch: "main".into(),
            base_commit,
            branch: "native/issue-42".into(),
            workspace_path: workspace_path.to_string_lossy().into_owned(),
        };
        let cli_profile = NativeCliProfile {
            kind: NativeAgentKind::OpenCode,
            cli_version: TEST_CLI_VERSION.into(),
            runtime_image_digest: profile.image_digest.clone(),
            expected_profile_command: profile.command.clone(),
        };
        let workspace_mount_binding = WorkspaceMountBinding {
            canonical_host_root: workspace_root,
            guest_root: "/workspace".into(),
        };
        let worker_binding = NativeWorkerBinding {
            worker_id: matched.worker_id().value,
            runner_id: matched.runner_id().value,
            worker_name: worker_name.into(),
            settings_digest: matched.settings_digest().into(),
            image_digest: profile.image_digest.clone(),
        };
        let launch_plan = build_launch_plan(
            &profile,
            &cli_profile,
            &prepared_workspace,
            &workspace_mount_binding,
            &issue,
            None,
        )
        .unwrap();
        let preview = NativeTerminalPreview::from_launch(
            "00000000-0000-4000-8000-000000000003",
            14,
            issue.revision(),
            Some(repository.identity.clone()),
            &launch_plan,
            Some(worker_binding),
        )
        .unwrap();

        NativeSessionContext::new(NativeSessionContextInput {
            session_id,
            attempt_id,
            owner_window_label: TEST_OWNER.into(),
            canonical_issue: issue,
            repository,
            base_resolution_receipt,
            prepared_workspace,
            profile,
            cli_profile,
            workspace_mount_binding,
            launch_plan,
            preview,
        })
        .unwrap()
    }

    fn result_item(result: SandboxResult) -> ResultOutputItem {
        ResultOutputItem {
            item: Some(Item::Data(
                SandboxExecResult {
                    result: Some(result),
                }
                .encode_to_vec(),
            )),
        }
    }

    fn set_clean_result_stream(fixture: &native_interactive_rpc::RpcFixture, exit_code: i32) {
        fixture.set_job_result_items(vec![
            result_item(SandboxResult::Output(SandboxExecOutput {
                stream: "stdout".into(),
                data: b"registry owner".to_vec(),
            })),
            result_item(SandboxResult::Exit(SandboxExecExit {
                exit_code,
                execution_time_ms: 321,
                sandbox_id: "native-sandbox".into(),
            })),
            ResultOutputItem {
                item: Some(Item::End(Default::default())),
            },
        ]);
    }

    fn set_many_output_stream(fixture: &native_interactive_rpc::RpcFixture, count: usize) {
        let mut items = (0..count)
            .map(|index| {
                result_item(SandboxResult::Output(SandboxExecOutput {
                    stream: "stdout".into(),
                    data: format!("chunk-{index}").into_bytes(),
                }))
            })
            .collect::<Vec<_>>();
        items.push(result_item(SandboxResult::Exit(SandboxExecExit {
            exit_code: 0,
            execution_time_ms: 321,
            sandbox_id: "native-sandbox".into(),
        })));
        items.push(ResultOutputItem {
            item: Some(Item::End(Default::default())),
        });
        fixture.set_job_result_items(items);
    }

    #[derive(Default)]
    struct BlockingOutputSink {
        first_event: Notify,
        release: Notify,
        events: StdMutex<Vec<super::super::wire::NativeTerminalOutputEventWire>>,
    }

    #[async_trait]
    impl NativeTerminalOutputSink for BlockingOutputSink {
        async fn emit(
            &self,
            event: super::super::wire::NativeTerminalOutputEventWire,
        ) -> Result<(), ()> {
            let first = {
                let mut events = self.events.lock().unwrap();
                let first = events.is_empty();
                events.push(event);
                first
            };
            if first {
                self.first_event.notify_one();
                self.release.notified().await;
            }
            Ok(())
        }
    }

    fn test_registry(root: &Path) -> NativeRuntimeRegistry {
        test_registry_with_accepting(root, Arc::new(AtomicBool::new(true)))
    }

    fn test_registry_with_accepting(
        root: &Path,
        accepting: Arc<AtomicBool>,
    ) -> NativeRuntimeRegistry {
        let data = DataPaths::with_root(root.join("registry-sidecar-data"));
        NativeRuntimeRegistry::new_with_accepting(
            DataPaths::with_root(root.join("registry-app-data")),
            Arc::new(Sidecars::new(SidecarConfig {
                jobworkerp_bin: "/bin/true".into(),
                memories_bin: "/bin/true".into(),
                memories_db_migrate_bin: "/bin/true".into(),
                startup_database_migration_enabled: false,
                conductor_bin: "/bin/true".into(),
                data: data.clone(),
                worker_yaml_paths: Vec::new(),
                issue_tracker_server_bin: None,
                function_set_yaml_paths: Vec::new(),
                reflection_dispatch_enabled: false,
                auto_embedding_enabled: false,
                workflows_dir: None,
                lance_language_model_home: data.lance_language_model_home(),
                lindera_dict_staged: false,
                llm_model: None,
                llm_hf_repo: None,
                llm_ctx_size: None,
                llm_kv_cache_type: None,
                env_file: None,
            })),
            Arc::new(IssueTrackerAvailability::default()),
            Arc::new(CodingAgentSettingsStore::new(root.join("settings.json"))),
            Arc::new(AsyncMutex::new(())),
            HostResourceLimits {
                cpu_millis: 4000,
                memory_mib: 8192,
                root_disk_mib: 32768,
                max_parallel_runs: 2,
            },
            None,
            accepting,
        )
    }

    #[tokio::test]
    async fn automatic_preparation_refuses_shutdown_before_accessing_missing_assets() {
        let root = tempfile::tempdir().unwrap();
        let registry = test_registry_with_accepting(root.path(), Arc::new(AtomicBool::new(false)));
        let request = super::super::wire::NativeTerminalPrepareRequest {
            issue_id: "issue-42".into(),
            profile_id: "profile-1".into(),
            base_branch: String::new(),
            prompt: None,
        };
        assert!(matches!(
            registry.prepare_automatic(request).await,
            Err(NativeRuntimeRegistryError::ShuttingDown)
        ));
        assert!(!root.path().join("registry-app-data").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn publication_lookup_requires_package_approval_before_configuring_repository() {
        for mismatched_fingerprint in [None, Some("a".repeat(64))] {
            let fixture = native_terminal_package_test_fixture::NativeFixture::new();
            let settings = Arc::new(CodingAgentSettingsStore::new(
                fixture.data_paths.coding_agent_settings_path(),
            ));
            if let Some(fingerprint) = mismatched_fingerprint {
                settings
                    .compare_and_swap_native_baseline(0, fingerprint)
                    .unwrap();
            }
            let registry = NativeRuntimeRegistry::new(
                fixture.data_paths.clone(),
                native_terminal_package_test_fixture::sidecars(fixture.temp.path()),
                Arc::new(IssueTrackerAvailability::default()),
                settings,
                Arc::new(AsyncMutex::new(())),
                HostResourceLimits {
                    cpu_millis: 4000,
                    memory_mib: 8192,
                    root_disk_mib: 32768,
                    max_parallel_runs: 2,
                },
                Some(fixture.app_resource_dir.clone()),
            );
            let repository = native_terminal_package_test_fixture::NativeFixture::repository(
                native_terminal_package_test_fixture::PRIMARY_REPOSITORY,
            );

            let result = registry.publication_lookup_client(&repository).await;
            match result {
                Err(error) => assert_eq!(error, NativeRuntimeRegistryError::Unavailable),
                Ok(_) => panic!("unapproved package reached configure_repository"),
            }
            assert!(
                !fixture
                    .data_paths
                    .root
                    .join("coding-agent/native/configs")
                    .exists(),
                "approval rejection must happen before configure_repository creates executor state"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn publication_lookup_fails_closed_for_a_broken_existing_policy() {
        let fixture = native_terminal_package_test_fixture::NativeFixture::new();
        let selection = fixture.selection().unwrap();
        fs::write(fixture.policy_path(), b"not-json").unwrap();
        let settings = Arc::new(CodingAgentSettingsStore::new(
            fixture.data_paths.coding_agent_settings_path(),
        ));
        settings
            .compare_and_swap_native_baseline(0, selection.package_fingerprint().to_owned())
            .unwrap();
        let registry = NativeRuntimeRegistry::new(
            fixture.data_paths.clone(),
            native_terminal_package_test_fixture::sidecars(fixture.temp.path()),
            Arc::new(IssueTrackerAvailability::default()),
            settings,
            Arc::new(AsyncMutex::new(())),
            HostResourceLimits {
                cpu_millis: 4000,
                memory_mib: 8192,
                root_disk_mib: 32768,
                max_parallel_runs: 2,
            },
            Some(fixture.app_resource_dir.clone()),
        );
        let repository = native_terminal_package_test_fixture::NativeFixture::repository(
            native_terminal_package_test_fixture::PRIMARY_REPOSITORY,
        );

        assert!(matches!(
            registry.publication_lookup_client(&repository).await,
            Err(NativeRuntimeRegistryError::Unavailable)
        ));
        assert!(
            !fixture
                .data_paths
                .root
                .join("coding-agent/native/configs")
                .exists(),
            "malformed policy must be rejected before executor configuration"
        );
    }

    fn fixture_registry(
        fixture: &native_terminal_package_test_fixture::NativeFixture,
        settings: Arc<CodingAgentSettingsStore>,
    ) -> NativeRuntimeRegistry {
        NativeRuntimeRegistry::new(
            fixture.data_paths.clone(),
            native_terminal_package_test_fixture::sidecars(fixture.temp.path()),
            Arc::new(IssueTrackerAvailability::default()),
            settings,
            Arc::new(AsyncMutex::new(())),
            HostResourceLimits {
                cpu_millis: 4000,
                memory_mib: 8192,
                root_disk_mib: 32768,
                max_parallel_runs: 2,
            },
            Some(fixture.app_resource_dir.clone()),
        )
    }

    fn fixture_settings(
        fixture: &native_terminal_package_test_fixture::NativeFixture,
    ) -> Arc<CodingAgentSettingsStore> {
        Arc::new(CodingAgentSettingsStore::new(
            fixture.data_paths.coding_agent_settings_path(),
        ))
    }

    #[test]
    fn missing_profile_home_maps_to_an_actionable_shell_error() {
        let error = map_preparation_error(NativePreparationError::ProfileHomeUnavailable);
        assert_eq!(error, NativeRuntimeRegistryError::ProfileHomeUnavailable);
        let message = error.user_message();
        assert!(message.contains("dedicated home"), "{message}");
        assert!(!message.contains("Issue"), "{message}");
    }

    #[test]
    fn native_setup_without_package_reports_package_missing_reason() {
        let temp = tempfile::tempdir().unwrap();
        let registry = test_registry(temp.path());

        let setup = registry.native_setup();

        assert_eq!(setup.state, NativeSetupState::Unavailable);
        assert_eq!(
            setup.unavailable_reason,
            Some(NativeSetupUnavailableReason::PackageMissing)
        );
        assert_eq!(setup.package_fingerprint, None);
        assert_eq!(setup.package_version, None);
        assert_eq!(setup.package_provenance, None);
        assert_eq!(
            setup.approval_source,
            NativeSetupApprovalSource::LegacyPolicy
        );
        assert!(setup.repository_approvals.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_setup_reports_reapproval_when_a_different_package_was_approved() {
        let fixture = native_terminal_package_test_fixture::NativeFixture::new();
        fs::remove_file(fixture.policy_path()).unwrap();
        let settings = fixture_settings(&fixture);
        settings
            .compare_and_swap_native_baseline(0, "f".repeat(64))
            .unwrap();
        let registry = fixture_registry(&fixture, Arc::clone(&settings));

        let setup = registry.native_setup();
        assert_eq!(setup.state, NativeSetupState::ReapprovalRequired);
        assert_eq!(setup.unavailable_reason, None);
        assert_eq!(
            setup.package_version.as_deref(),
            Some(coding_agent_executor::EXECUTOR_VERSION)
        );
        assert_eq!(
            setup.package_provenance,
            Some(NativeSetupPackageProvenance::Release)
        );
        let fingerprint = setup.package_fingerprint.clone().unwrap();

        let approved = registry
            .approve_native_backend(setup.settings_revision, Some(&fingerprint))
            .unwrap();
        assert_eq!(approved.state, NativeSetupState::Approved);
        assert_eq!(
            settings
                .load()
                .unwrap()
                .native_backend_package_fingerprint
                .as_deref(),
            Some(fingerprint.as_str())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_setup_display_does_not_rehash_artifacts_but_approval_does() {
        let fixture = native_terminal_package_test_fixture::NativeFixture::new();
        fs::remove_file(fixture.policy_path()).unwrap();
        let registry = fixture_registry(&fixture, fixture_settings(&fixture));
        let runtime = fixture.resource_root.join("runtime/all-in-one");
        let mut bytes = fs::read(&runtime).unwrap();
        bytes[0] ^= 0xff;
        fs::write(&runtime, bytes).unwrap();

        let setup = registry.native_setup();
        assert_eq!(setup.state, NativeSetupState::NotApproved);
        assert_eq!(setup.unavailable_reason, None);

        assert!(
            registry
                .approve_native_backend(
                    setup.settings_revision,
                    setup.package_fingerprint.as_deref()
                )
                .is_err(),
            "approval must re-hash the package it records"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_setup_reports_package_mismatch_only_for_a_legacy_identity_mismatch() {
        let fixture = native_terminal_package_test_fixture::NativeFixture::new();
        let mut pinned_elsewhere = fixture.manifest.clone();
        pinned_elsewhere["sources"]["agent_app"]["revision"] = json!("e".repeat(40));
        native_terminal_package_test_fixture::write_host_policy(
            &fixture.data_paths,
            &pinned_elsewhere,
            &[native_terminal_package_test_fixture::PRIMARY_REPOSITORY],
            Some("read-token"),
        );
        let registry = fixture_registry(&fixture, fixture_settings(&fixture));

        let setup = registry.native_setup();
        assert_eq!(setup.state, NativeSetupState::PackageMismatch);
        assert_eq!(setup.unavailable_reason, None);
        let fingerprint = setup.package_fingerprint.clone().unwrap();

        assert_eq!(
            registry.approve_native_backend(setup.settings_revision, Some(&fingerprint)),
            Err(NativeRuntimeRegistryError::Unavailable)
        );
    }

    #[tokio::test]
    async fn profile_settings_start_does_not_deadlock_on_the_jobworkerp_admission_lock() {
        use crate::coding_agent::settings::{AgentProfileSettings, SandboxResourcesSettings};
        const IMAGE: &str = "registry.example/agent@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let fixture = native_terminal_package_test_fixture::NativeFixture::new();
        let settings = fixture_settings(&fixture);
        let current = settings.load().unwrap();
        let saved = settings
            .compare_and_swap(
                current.revision,
                CodingAgentSettings {
                    profiles: vec![AgentProfileSettings {
                        profile_id: "profile-shell".into(),
                        version: 1,
                        display_name: "Shell".into(),
                        image_ref: IMAGE.into(),
                        allowed_image_refs: vec![IMAGE.into()],
                        command: vec!["codex".into()],
                        container_home: "/home/agent".into(),
                        host_configuration: None,
                        runtime_environment: Default::default(),
                        network_access: Default::default(),
                        resources: SandboxResourcesSettings {
                            cpu_millis: 1000,
                            memory_mib: 2048,
                            root_disk_mib: 8192,
                            max_parallel_runs: 1,
                            use_static: false,
                        },
                        mount_allowed_roots: Vec::new(),
                        persistent_mounts: Vec::new(),
                        allowed_repositories: Vec::new(),
                        credential_reference: None,
                    }],
                    active_profile_id: Some("profile-shell".into()),
                    ..current
                },
                &HashSet::new(),
            )
            .unwrap();
        let registry = fixture_registry(&fixture, settings);
        let admission_lock = AsyncMutex::new(());
        let admission = admission_lock.lock().await;

        // A digest-pinned image needs no registry lookup, so the call reaches the
        // sidecar inspection, which fails fast here because no child is managed.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            registry.start_profile_settings_terminal(
                "main",
                saved.revision,
                "profile-shell",
                &HashSet::new(),
                &admission,
            ),
        )
        .await
        .expect("starting the settings shell must not wait on a lock it already holds");
        assert_eq!(
            result.map(|session| session.session_id),
            Err(NativeRuntimeRegistryError::SourceUnavailable)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_setup_reports_the_gui_list_as_approval_source_once_saved() {
        let fixture = native_terminal_package_test_fixture::NativeFixture::new();
        fs::remove_file(fixture.policy_path()).unwrap();
        let settings = fixture_settings(&fixture);
        let current = settings.load().unwrap();
        settings
            .compare_and_swap(
                current.revision,
                CodingAgentSettings {
                    native_repository_approvals: Some(Vec::new()),
                    ..current
                },
                &HashSet::new(),
            )
            .unwrap();
        let registry = fixture_registry(&fixture, settings);

        let setup = registry.native_setup();
        assert_eq!(setup.state, NativeSetupState::NotApproved);
        assert_eq!(setup.approval_source, NativeSetupApprovalSource::Gui);
        assert!(setup.repository_approvals.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn fresh_gui_setup_approves_package_then_checks_and_configures_saved_ssh_repository() {
        let mut fixture = native_terminal_package_test_fixture::NativeFixture::new();
        fs::remove_file(fixture.policy_path()).unwrap();
        let check_marker = fixture.temp.path().join("repository-check-invoked");
        fixture.install_repository_check_executor(&check_marker);
        let settings = Arc::new(CodingAgentSettingsStore::new(
            fixture.data_paths.coding_agent_settings_path(),
        ));
        let registry = NativeRuntimeRegistry::new(
            fixture.data_paths.clone(),
            native_terminal_package_test_fixture::sidecars(fixture.temp.path()),
            Arc::new(IssueTrackerAvailability::default()),
            Arc::clone(&settings),
            Arc::new(AsyncMutex::new(())),
            HostResourceLimits {
                cpu_millis: 4000,
                memory_mib: 8192,
                root_disk_mib: 32768,
                max_parallel_runs: 2,
            },
            Some(fixture.app_resource_dir.clone()),
        );
        let repository = native_terminal_package_test_fixture::NativeFixture::repository(
            native_terminal_package_test_fixture::PRIMARY_REPOSITORY,
        );
        let approval = NativeRepositoryApprovalSettings {
            repository: RepositoryIdentitySettings {
                host: repository.host.clone(),
                organization: repository.owner.clone(),
                repository: repository.name.clone(),
            },
            allowed_base_branches: vec!["main".into()],
        };
        let initial = RepositoryHostSettings {
            label_host: repository.host.clone(),
            forge: ForgeKind::GitHub,
            api_base_url: "https://api.github.com".into(),
            clone_transport: crate::coding_agent::settings::CloneTransportSettings::Https {
                base_url: "https://github.com".into(),
            },
            read_credential_ref: None,
            publication_credential_ref: None,
        };

        let setup = registry.native_setup();
        assert_eq!(setup.state, NativeSetupState::NotApproved);
        let fingerprint = setup.package_fingerprint.clone().unwrap();

        let approved = registry
            .approve_native_backend(setup.settings_revision, Some(&fingerprint))
            .unwrap();
        assert_eq!(approved.state, NativeSetupState::Approved);

        let ssh_directory = fixture.temp.path().join("saved-ssh");
        fs::create_dir(&ssh_directory).unwrap();
        fs::set_permissions(&ssh_directory, fs::Permissions::from_mode(0o700)).unwrap();
        let final_host = RepositoryHostSettings {
            clone_transport: crate::coding_agent::settings::CloneTransportSettings::Ssh {
                host: "github.com".into(),
                user: "git".into(),
                port: Some(22),
                ssh_directory: Some(ssh_directory.clone()),
            },
            ..initial
        };
        let final_settings = settings.load().unwrap();
        let saved = settings
            .compare_and_swap(
                final_settings.revision,
                CodingAgentSettings {
                    repository_hosts: vec![final_host.clone()],
                    native_repository_approvals: Some(vec![approval.clone()]),
                    native_backend_package_fingerprint: final_settings
                        .native_backend_package_fingerprint,
                    ..final_settings
                },
                &HashSet::new(),
            )
            .unwrap();
        let checked = registry
            .check_repository_host(
                crate::coding_agent::repository_check::RepositoryHostCheckRequest {
                    expected_revision: saved.revision,
                    host: final_host,
                    repository: Some(approval.repository),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            checked.stages[0].code,
            crate::coding_agent::repository_check::CheckCode::SettingsValid
        );
        assert_eq!(
            checked.stages[1].code,
            crate::coding_agent::repository_check::CheckCode::PolicyMatched
        );
        assert_eq!(checked.stages.len(), 4);
        assert_eq!(
            checked.stages[2].stage,
            crate::coding_agent::repository_check::CheckStageName::GitRead
        );
        assert_eq!(
            checked.stages[2].status,
            crate::coding_agent::repository_check::CheckStageStatus::Passed
        );
        assert_eq!(
            checked.stages[2].code,
            crate::coding_agent::repository_check::CheckCode::GitReadable
        );
        assert!(
            check_marker.is_file(),
            "repository check must invoke the executor"
        );

        let selection = fixture.selection().unwrap();
        let configured = configure_repository(&fixture.data_paths, &selection, &repository)
            .await
            .unwrap();
        let trusted =
            native_terminal_package_test_fixture::NativeFixture::configured_trusted_config(
                &fixture,
                &repository,
            );
        assert_eq!(
            trusted.repositories[0].ssh_directory.as_ref(),
            Some(&ssh_directory)
        );
        assert_eq!(configured.repository(), &repository);

        let prepare_error = registry
            .prepare(
                "fresh-gui",
                super::super::wire::NativeTerminalPrepareRequest {
                    issue_id: "issue-42".into(),
                    profile_id: "opencode-native".into(),
                    base_branch: "main".into(),
                    prompt: Some("prepare after GUI approval".into()),
                },
            )
            .await
            .expect_err("the existing unavailable Issue Tracker must stop the real prepare path");
        assert_eq!(
            prepare_error,
            NativeRuntimeRegistryError::AuthorityUnavailable,
            "prepare must reach the authority boundary after policy approval, not fail closed as policy unavailable"
        );
    }

    #[test]
    fn registry_retains_the_exact_root_quit_acceptance_arc() {
        let root = tempfile::tempdir().unwrap();
        let accepting = Arc::new(AtomicBool::new(false));
        let registry = test_registry_with_accepting(root.path(), Arc::clone(&accepting));

        assert!(Arc::ptr_eq(&registry.accepting, &accepting));
    }
}
