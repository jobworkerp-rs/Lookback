//! Fail-closed checks for observing a jobworkerp SANDBOX microVM's physical stop.
//!
//! Microsandbox 0.7.6 exposes no run-bound native process-exit token through its
//! public lifecycle API. This module therefore verifies effective paths and then
//! reports physical-exit observation as unsupported. SDK destroy success,
//! terminal catalog state, stream End, and digest strings supplied by callers
//! are not physical-exit proof.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use jobworkerp_client::jobworkerp::data::JobId;
use microsandbox::{LocalBackend, config::GlobalConfig};
use thiserror::Error;

use crate::jobworkerp::coding_agent::CodingAgentEvent;

/// Microsandbox SDK version pinned for this capability check.
pub const MICROSANDBOX_SDK_VERSION: &str = "0.7.6";

const DEFAULT_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(120);
const REQUIRED_RUNNER_NAME: &str = "SANDBOX";

/// Explicit Microsandbox child paths and a bound for resolving their effective config.
///
/// These are builder inputs for the same child paths. SDK environment and
/// machine-managed overrides still participate in lazy config resolution; the
/// resulting effective paths are checked against the worker snapshot before
/// catalog access. The observer does not mutate process environment or the
/// SDK process-global backend.
#[derive(Clone)]
pub struct MicrosandboxObservationConfig {
    home: PathBuf,
    config_path: PathBuf,
    timeout: Duration,
}

impl MicrosandboxObservationConfig {
    /// Use the same explicit home and config file supplied to the worker child.
    pub fn new(home: impl Into<PathBuf>, config_path: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            config_path: config_path.into(),
            timeout: DEFAULT_OBSERVATION_TIMEOUT,
        }
    }

    /// Set a bounded budget for resolving the SDK's effective configuration.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }
}

impl std::fmt::Debug for MicrosandboxObservationConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MicrosandboxObservationConfig")
            .field("paths_configured", &true)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Expected worker binding carried by the immutable execution identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerIdentity {
    worker_id: i64,
    worker_name: String,
    runtime_instance_id: String,
}

impl WorkerIdentity {
    pub fn new(
        worker_id: i64,
        worker_name: impl Into<String>,
        runtime_instance_id: impl Into<String>,
    ) -> Self {
        Self {
            worker_id,
            worker_name: worker_name.into(),
            runtime_instance_id: runtime_instance_id.into(),
        }
    }

    pub fn worker_id(&self) -> i64 {
        self.worker_id
    }

    pub fn worker_name(&self) -> &str {
        &self.worker_name
    }

    pub fn runtime_instance_id(&self) -> &str {
        &self.runtime_instance_id
    }
}

/// Caller-supplied SHA-256 claims for four runtime packages.
///
/// This module does not hash the selected files. Equal strings are not verified
/// provenance and cannot authorize a physical-stop proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeArtifactDigests {
    pub runtime: String,
    pub msb: String,
    pub firmware: String,
    pub agentd: String,
}

impl RuntimeArtifactDigests {
    pub fn new(
        runtime: impl Into<String>,
        msb: impl Into<String>,
        firmware: impl Into<String>,
        agentd: impl Into<String>,
    ) -> Self {
        Self {
            runtime: runtime.into(),
            msb: msb.into(),
            firmware: firmware.into(),
            agentd: agentd.into(),
        }
    }

    fn is_valid_sha256(&self) -> bool {
        [&self.runtime, &self.msb, &self.firmware, &self.agentd]
            .into_iter()
            .all(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
    }
}

/// Caller-supplied worker/runtime facts used for fail-closed consistency checks.
///
/// Optional facts deliberately make incomplete snapshots non-authoritative.
/// Artifact digest fields are caller claims only; this module does not verify
/// their corresponding files or treat them as provenance proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerRuntimeContext {
    pub worker_id: i64,
    pub worker_name: String,
    pub runner_name: String,
    pub is_static_worker: Option<bool>,
    pub worker_snapshot_id: Option<String>,
    pub runtime_instance_id: Option<String>,
    pub microsandbox_sdk_version: Option<String>,
    pub child_msb_home: Option<PathBuf>,
    pub child_msb_config_path: Option<PathBuf>,
    pub child_msb_sandboxes_dir: Option<PathBuf>,
    pub child_msb_run_dir: Option<PathBuf>,
    pub pinned_artifacts: Option<RuntimeArtifactDigests>,
    pub observed_artifacts: Option<RuntimeArtifactDigests>,
}

/// Reserved token for a future run-bound native process-exit observer.
///
/// This module does not construct this type. SDK 0.7.6's public lifecycle result
/// does not expose enough evidence to bind native process exit to the selected
/// run; a successful `destroy_with` is insufficient.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalStopProof {
    worker: WorkerIdentity,
    worker_snapshot_id: String,
    runtime_instance_id: String,
    job_id: i64,
    sandbox_id: String,
    exit_code: i32,
    stopped_at: SystemTime,
}

impl PhysicalStopProof {
    pub fn worker(&self) -> &WorkerIdentity {
        &self.worker
    }

    pub fn worker_snapshot_id(&self) -> &str {
        &self.worker_snapshot_id
    }

    pub fn runtime_instance_id(&self) -> &str {
        &self.runtime_instance_id
    }

    pub fn job_id(&self) -> i64 {
        self.job_id
    }

    pub fn sandbox_id(&self) -> &str {
        &self.sandbox_id
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    pub fn stopped_at(&self) -> SystemTime {
        self.stopped_at
    }
}

/// Safe outcome codes for a runtime observation that did not produce proof.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum RuntimeObservationError {
    #[error("runtime observation configuration is invalid")]
    InvalidConfiguration,
    #[error("a typed SANDBOX exit result is required")]
    TypedExitRequired,
    #[error("jobworkerp job identity is invalid")]
    InvalidJobIdentity,
    #[error("SANDBOX exit did not contain a canonical local sandbox identity")]
    InvalidSandboxIdentity,
    #[error("worker identity does not match the execution")]
    WorkerMismatch,
    #[error("static workers cannot prove per-run physical stop")]
    StaticWorker,
    #[error("worker/runtime claims are missing or inconsistent")]
    UntrustedContext,
    #[error("effective Microsandbox paths differ from the worker's paths")]
    RuntimePathsMismatch,
    #[error("Microsandbox SDK operation failed")]
    SdkFailure,
    #[error("runtime observation exceeded its timeout")]
    TimedOut,
    #[error("the SDK does not expose run-bound native process-exit evidence")]
    UnsupportedPhysicalExitObservation,
}

/// Check worker bindings and SDK-resolved paths without opening the catalog.
///
/// Microsandbox 0.7.6 does not expose a run-bound native process-exit token.
/// After a lazy backend resolves and validates its effective paths, this method
/// returns `UnsupportedPhysicalExitObservation` before catalog/database access
/// or any destructive lifecycle operation.
pub async fn observe_sandbox_stop(
    config: &MicrosandboxObservationConfig,
    expected_worker: &WorkerIdentity,
    runtime: &WorkerRuntimeContext,
    job_id: &JobId,
    exit: &CodingAgentEvent,
) -> Result<PhysicalStopProof, RuntimeObservationError> {
    validate_request(config, expected_worker, runtime, job_id, exit)?;
    let config_path = config.config_path.clone();
    let home = config.home.clone();
    let backend = tokio::time::timeout(
        config.timeout,
        tokio::task::spawn_blocking(move || {
            LocalBackend::builder()
                .config_path(config_path)
                .home(home)
                .build_lazy()
        }),
    )
    .await
    .map_err(|_| RuntimeObservationError::TimedOut)?
    .map_err(|_| RuntimeObservationError::SdkFailure)?
    .map_err(|_| RuntimeObservationError::SdkFailure)?;

    validate_effective_paths(config, expected_worker, runtime, backend.config())?;
    Err(RuntimeObservationError::UnsupportedPhysicalExitObservation)
}

fn validate_request(
    config: &MicrosandboxObservationConfig,
    expected_worker: &WorkerIdentity,
    runtime: &WorkerRuntimeContext,
    job_id: &JobId,
    exit: &CodingAgentEvent,
) -> Result<(), RuntimeObservationError> {
    if !config.home.is_absolute()
        || config.home == Path::new("/")
        || !config.config_path.is_absolute()
        || config.config_path == config.home
        || config.timeout.is_zero()
        || config.timeout > MAX_OBSERVATION_TIMEOUT
    {
        return Err(RuntimeObservationError::InvalidConfiguration);
    }

    let sandbox_id = match exit {
        CodingAgentEvent::Exit { sandbox_id, .. } => sandbox_id,
        CodingAgentEvent::Output { .. } => {
            return Err(RuntimeObservationError::TypedExitRequired);
        }
    };
    if job_id.value <= 0 {
        return Err(RuntimeObservationError::InvalidJobIdentity);
    }
    if !is_canonical_local_sandbox_id(sandbox_id) {
        return Err(RuntimeObservationError::InvalidSandboxIdentity);
    }
    if expected_worker.worker_id <= 0
        || !valid_identifier(&expected_worker.worker_name)
        || !valid_identifier(&expected_worker.runtime_instance_id)
        || runtime.worker_id != expected_worker.worker_id
        || runtime.worker_name != expected_worker.worker_name
    {
        return Err(RuntimeObservationError::WorkerMismatch);
    }
    if runtime
        .runtime_instance_id
        .as_deref()
        .is_some_and(|observed| observed != expected_worker.runtime_instance_id.as_str())
    {
        return Err(RuntimeObservationError::WorkerMismatch);
    }
    if runtime.is_static_worker == Some(true) {
        return Err(RuntimeObservationError::StaticWorker);
    }
    let (Some(worker_snapshot_id), Some(runtime_instance_id)) = (
        runtime.worker_snapshot_id.as_deref(),
        runtime.runtime_instance_id.as_deref(),
    ) else {
        return Err(RuntimeObservationError::UntrustedContext);
    };
    if runtime.is_static_worker != Some(false)
        || runtime.runner_name != REQUIRED_RUNNER_NAME
        || !valid_identifier(&runtime.worker_name)
        || !valid_identifier(worker_snapshot_id)
        || !valid_identifier(runtime_instance_id)
        || runtime.microsandbox_sdk_version.as_deref() != Some(MICROSANDBOX_SDK_VERSION)
        || !runtime
            .child_msb_home
            .as_deref()
            .is_some_and(Path::is_absolute)
        || !runtime
            .child_msb_config_path
            .as_deref()
            .is_some_and(Path::is_absolute)
        || !runtime
            .child_msb_sandboxes_dir
            .as_deref()
            .is_some_and(Path::is_absolute)
        || !runtime
            .child_msb_run_dir
            .as_deref()
            .is_some_and(Path::is_absolute)
    {
        return Err(RuntimeObservationError::UntrustedContext);
    }
    let (Some(pinned_artifacts), Some(observed_artifacts)) =
        (&runtime.pinned_artifacts, &runtime.observed_artifacts)
    else {
        return Err(RuntimeObservationError::UntrustedContext);
    };
    if !pinned_artifacts.is_valid_sha256()
        || !observed_artifacts.is_valid_sha256()
        || pinned_artifacts != observed_artifacts
    {
        return Err(RuntimeObservationError::UntrustedContext);
    }

    let _ = (sandbox_id, worker_snapshot_id, runtime_instance_id);
    Ok(())
}

/// Compare the child path snapshot with the effective paths produced by the
/// SDK lazy backend. The lazy backend has not opened or migrated the catalog.
///
/// This public check is also a resolved-config seam for tests: callers may pass
/// a locally constructed `GlobalConfig`; no process environment or machine-wide
/// config file is modified by this function.
pub fn validate_effective_paths(
    config: &MicrosandboxObservationConfig,
    expected_worker: &WorkerIdentity,
    runtime: &WorkerRuntimeContext,
    effective: &GlobalConfig,
) -> Result<(), RuntimeObservationError> {
    if expected_worker.worker_id != runtime.worker_id
        || expected_worker.worker_name != runtime.worker_name
    {
        return Err(RuntimeObservationError::WorkerMismatch);
    }
    let child_home = required_path(&runtime.child_msb_home)?;
    let child_config = required_path(&runtime.child_msb_config_path)?;
    let child_sandboxes = required_path(&runtime.child_msb_sandboxes_dir)?;
    let child_run = required_path(&runtime.child_msb_run_dir)?;

    if !same_directory_path(&config.home, child_home)
        || !same_config_path(&config.config_path, child_config)
        || !same_directory_path(&effective.home(), child_home)
        || !same_directory_path(&effective.sandboxes_dir(), child_sandboxes)
        || !same_directory_path(&effective.run_dir(), child_run)
    {
        return Err(RuntimeObservationError::RuntimePathsMismatch);
    }
    Ok(())
}

fn required_path(path: &Option<PathBuf>) -> Result<&Path, RuntimeObservationError> {
    path.as_deref()
        .filter(|path| path.is_absolute())
        .ok_or(RuntimeObservationError::UntrustedContext)
}

fn same_directory_path(left: &Path, right: &Path) -> bool {
    left.is_absolute()
        && right.is_absolute()
        && canonical_directory_path(left)
            .zip(canonical_directory_path(right))
            .is_some_and(|(left, right)| left == right)
}

fn canonical_directory_path(path: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(path).ok()?;
    fs::metadata(&canonical).ok()?.is_dir().then_some(canonical)
}

fn same_config_path(left: &Path, right: &Path) -> bool {
    canonical_config_path(left)
        .ok()
        .zip(canonical_config_path(right).ok())
        .is_some_and(|(left, right)| left == right)
}

fn canonical_config_path(path: &Path) -> std::io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    if path.exists() {
        let canonical = fs::canonicalize(path)?;
        return if fs::metadata(&canonical)?.is_file() {
            Ok(canonical)
        } else {
            Err(std::io::ErrorKind::InvalidInput.into())
        };
    }
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    Ok(fs::canonicalize(parent)?.join(name))
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

fn is_canonical_local_sandbox_id(value: &str) -> bool {
    value
        .strip_prefix("local:")
        .and_then(|id| id.parse::<i32>().ok())
        .is_some_and(|id| id > 0 && format!("local:{id}") == value)
}
