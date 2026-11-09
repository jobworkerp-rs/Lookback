//! Native-only, preview-scoped runtime observations for interactive sessions.

use std::{
    path::{Component, Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

#[cfg(target_os = "linux")]
use std::{
    ffi::CString,
    fs::{self, File},
    io::Read,
    os::fd::{AsRawFd, FromRawFd},
};

use jobworkerp_client::jobworkerp::data::JobId;
use parking_lot::Mutex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    jobworkerp::{
        JobworkerpHandle,
        coding_agent::{
            CodingAgentEvent, HostWorkerContext, NativeVersionProbeOptions, NativeWorkerMatch,
        },
    },
    sidecar::{
        ManagedJobworkerpAdmissionGuard, ManagedJobworkerpSdkSourceCheck,
        ManagedJobworkerpSdkSourceStamp, ManagedJobworkerpSpawnSnapshot, Sidecars,
    },
};

use super::{
    NativeSessionContext,
    bootstrap::{
        NativePackageSelection, NativeRuntimeImageObservation, NativeRuntimeImageUnavailable,
    },
    doctor_store::{
        NativeProbeCancelClaim, NativeProbeEvidence, NativeProbeIntent, NativeRuntimeDoctorStore,
    },
    is_canonical_uuid, is_sha256,
    manager::{NativeManagerAdmissionGuard, NativeTerminalManager, NativeTerminalManagerError},
};

const MAX_NATIVE_SDK_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_NATIVE_PROBE_STDOUT_BYTES: usize = 64 * 1024;
const NATIVE_PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const PROBE_CANCEL_GRACE: Duration = Duration::from_secs(2);
const PROBE_CANCEL_RPC_TIMEOUT: Duration = Duration::from_secs(1);
const NATIVE_SDK_DECLARED_PIN: &str = "0.7.6";
const MANAGED_MICROSANDBOX_CONFIG: &str = "/etc/microsandbox/managed.json";
const SDK_HOME_DOMAIN: &[u8] = b"lookback/native-doctor/sdk-home/v1\0";
const SDK_SOURCE_DOMAIN: &[u8] = b"lookback/native-doctor/sdk-source/v1\0";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeSourceInspection {
    NotObserved,
    Observed {
        selected_config_sha256: String,
        effective_home_sha256: String,
        sdk_declared_pin: String,
    },
}

/// Separate observation of the fixed CLI version probe; it is not a runtime
/// readiness or physical VM lifecycle result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeCliProbeObservation {
    NotObserved,
    Completed {
        job_id: i64,
        version: Option<String>,
        exit_code: Option<i32>,
        end_received: bool,
        stream_eof: bool,
        stdout_sha256: Option<String>,
        rejection: Option<NativeCliProbeValidationError>,
    },
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum NativeCliProbeValidationError {
    #[error("native CLI probe output exceeded the bounded size")]
    OutputTooLarge,
    #[error("native CLI probe returned a nonzero exit code")]
    NonzeroExit,
    #[error("native CLI probe did not produce a clean Exit, End, and EOF")]
    IncompleteStream,
    #[error("native CLI probe output did not match the executable's version format")]
    InvalidVersionOutput,
    #[error("native CLI probe stream reported a transport error")]
    StreamError,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeCliProbeResult {
    version: String,
    exit_code: i32,
    end_received: bool,
    stream_eof: bool,
}

impl NativeCliProbeResult {
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    pub fn end_received(&self) -> bool {
        self.end_received
    }

    pub fn stream_eof(&self) -> bool {
        self.stream_eof
    }
}

/// Validate the exact one-shot output and its decoded terminal protocol facts.
/// The caller never receives or logs untrusted raw subprocess bytes on failure.
pub fn validate_native_cli_probe_result(
    stdout: &[u8],
    stderr: &[u8],
    exit_code: Option<i32>,
    end_received: bool,
    stream_eof: bool,
    stream_error: bool,
) -> Result<NativeCliProbeResult, NativeCliProbeValidationError> {
    validate_native_cli_probe_for_profile(
        &super::launch::NativeCliProfile {
            kind: super::launch::NativeAgentKind::OpenCode,
            cli_version: String::new(),
            runtime_image_digest: String::new(),
            expected_profile_command: vec![],
        },
        stdout,
        stderr,
        exit_code,
        end_received,
        stream_eof,
        stream_error,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn validate_native_cli_probe_for_profile(
    profile: &super::launch::NativeCliProfile,
    stdout: &[u8],
    stderr: &[u8],
    exit_code: Option<i32>,
    end_received: bool,
    stream_eof: bool,
    stream_error: bool,
) -> Result<NativeCliProbeResult, NativeCliProbeValidationError> {
    if !super::launch::cli_version_hint_valid(&profile.cli_version) {
        return Err(NativeCliProbeValidationError::InvalidVersionOutput);
    }
    if stdout.len() > MAX_NATIVE_PROBE_STDOUT_BYTES || stderr.len() > MAX_NATIVE_PROBE_STDOUT_BYTES
    {
        return Err(NativeCliProbeValidationError::OutputTooLarge);
    }
    if stream_error {
        return Err(NativeCliProbeValidationError::StreamError);
    }
    if !end_received || !stream_eof || exit_code.is_none() {
        return Err(NativeCliProbeValidationError::IncompleteStream);
    }
    let exit_code = exit_code.expect("checked above");
    if exit_code != 0 {
        return Err(NativeCliProbeValidationError::NonzeroExit);
    }
    let Some(version) = parse_native_cli_version(profile.kind, stdout) else {
        return Err(NativeCliProbeValidationError::InvalidVersionOutput);
    };
    Ok(NativeCliProbeResult {
        version,
        exit_code,
        end_received,
        stream_eof,
    })
}

fn parse_native_cli_version(kind: super::launch::NativeAgentKind, stdout: &[u8]) -> Option<String> {
    let output = std::str::from_utf8(stdout).ok()?;
    let line = output
        .strip_suffix("\r\n")
        .or_else(|| output.strip_suffix('\n'))
        .unwrap_or(output);
    if line
        .chars()
        .any(|character| character == '\r' || character == '\n')
    {
        return None;
    }
    let version = match kind {
        super::launch::NativeAgentKind::OpenCode => line,
        super::launch::NativeAgentKind::Codex => line.strip_prefix("codex-cli ")?,
        super::launch::NativeAgentKind::ClaudeCode => line.strip_suffix(" (Claude Code)")?,
    };
    super::launch::verified_cli_version(version).then(|| version.to_owned())
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativeRuntimeDoctorError {
    #[error("native runtime doctor request is invalid")]
    InvalidRequest,
    #[error("the managed jobworkerp child is unavailable or was replaced")]
    ManagedChildUnavailable,
    #[error("the managed jobworkerp SDK source changed during observation")]
    SdkSourceChanged,
    #[error("the selected SDK configuration source is unavailable or unsafe")]
    SdkSourceUnavailable,
    #[error("the selected SDK configuration is malformed or cannot be boundedly resolved")]
    SdkConfigUnavailable,
    #[error("Microsandbox managed or selected path overrides are not safely supported")]
    SdkPathOverrideUnavailable,
    #[error("the observed SDK context differs from the staged native worker")]
    SdkContextMismatch,
    #[error("the managed runtime image did not pass the package digest check")]
    RuntimeImageUnavailable,
    #[error("the staged native preview or worker is unavailable or changed")]
    PreviewUnavailable,
    #[error("the probe could not be opened with an unambiguous accepted job identity")]
    ProbeOutcomeUnknown,
    #[error("the native CLI probe did not produce strict version-format evidence")]
    ProbeRejected,
    #[error("the native runtime doctor journal is unavailable")]
    JournalUnavailable,
    #[error("native runtime doctor task failed")]
    TaskFailed,
    #[error("native interactive start acceptance or outcome may be unknown")]
    StartOutcomeUnknown,
    #[error("the application is shutting down and no longer accepts native operations")]
    NotAccepting,
}

/// Private, non-serializable authority for one exact staged preview and one
/// clean, completed native CLI version probe. It does not assert VM stop or
/// claim physical runtime identity beyond the observations recorded here.
pub struct ObservedRuntimeLease {
    owner_window_label: String,
    preview_id: String,
    preview_digest: String,
    context_digest: String,
    expected_settings_revision: u64,
    expected_issue_revision: u64,
    profile_id: String,
    worker_id: i64,
    runner_id: i64,
    worker_settings_digest: String,
    matched_runner_schema_digest: String,
    run_schema_digest: String,
    runtime_image: NativeRuntimeImageObservation,
    sdk_source: SdkConfigurationProof,
    sdk_declared_pin: String,
    prepared_cwd: String,
    cli_version: String,
    probe_job_id: i64,
    operation_id: String,
    stdout_sha256: String,
    exit_code: i32,
    end_received: bool,
    stream_eof: bool,
}

impl std::fmt::Debug for ObservedRuntimeLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObservedRuntimeLease")
            .field("preview_id", &self.preview_id)
            .field("preview_digest", &self.preview_digest)
            .field("context_digest", &self.context_digest)
            .field("worker_id", &self.worker_id)
            .field("runner_id", &self.runner_id)
            .field("runtime_image_sha256", &self.runtime_image.sha256())
            .field("sdk_declared_pin", &self.sdk_declared_pin)
            .field("cli_version", &self.cli_version)
            .field("probe_job_id", &self.probe_job_id)
            .field("operation_id", &self.operation_id)
            .field("exit_code", &self.exit_code)
            .field("end_received", &self.end_received)
            .field("stream_eof", &self.stream_eof)
            .finish_non_exhaustive()
    }
}

impl ObservedRuntimeLease {
    /// Revalidate owner, fresh canonical authority, staged immutable context,
    /// live Worker/run-schema, managed child generation, selected source stamp,
    /// and actual package runtime image under the Settings-admission-then-
    /// lifecycle lock order used by native startup. The guards are released
    /// before this method returns; validation is not an atomic start claim, so
    /// the eventual start path must repeat its authority checks while claiming.
    pub async fn validate_for_preview(
        &self,
        doctor: &NativeRuntimeDoctor,
        owner_window_label: &str,
        preview_id: &str,
        preview_digest: &str,
    ) -> Result<(), NativeRuntimeDoctorError> {
        validate_preview_request(owner_window_label, preview_id, preview_digest)?;
        if owner_window_label != self.owner_window_label
            || preview_id != self.preview_id
            || preview_digest != self.preview_digest
        {
            return Err(NativeRuntimeDoctorError::PreviewUnavailable);
        }
        if !is_sha256(&self.run_schema_digest)
            || !is_sha256(&self.stdout_sha256)
            || !super::launch::verified_cli_version(&self.cli_version)
            || self.sdk_declared_pin != NATIVE_SDK_DECLARED_PIN
            || self.exit_code != 0
            || !self.end_received
            || !self.stream_eof
        {
            return Err(NativeRuntimeDoctorError::ProbeRejected);
        }
        let admission_guard = doctor.manager.lock_admission().await;
        let lifecycle_guard = doctor.sidecars.lock_native_jobworkerp_admission().await;
        self.validate_for_preview_admitted(
            doctor,
            owner_window_label,
            preview_id,
            preview_digest,
            &admission_guard,
            &lifecycle_guard,
        )
        .await
    }

    async fn validate_for_preview_admitted(
        &self,
        doctor: &NativeRuntimeDoctor,
        owner_window_label: &str,
        preview_id: &str,
        preview_digest: &str,
        admission_guard: &NativeManagerAdmissionGuard,
        lifecycle_guard: &ManagedJobworkerpAdmissionGuard<'_>,
    ) -> Result<(), NativeRuntimeDoctorError> {
        validate_preview_request(owner_window_label, preview_id, preview_digest)?;
        if owner_window_label != self.owner_window_label
            || preview_id != self.preview_id
            || preview_digest != self.preview_digest
        {
            return Err(NativeRuntimeDoctorError::PreviewUnavailable);
        }
        if !is_sha256(&self.run_schema_digest)
            || !is_sha256(&self.stdout_sha256)
            || !super::launch::verified_cli_version(&self.cli_version)
            || self.sdk_declared_pin != NATIVE_SDK_DECLARED_PIN
            || self.exit_code != 0
            || !self.end_received
            || !self.stream_eof
        {
            return Err(NativeRuntimeDoctorError::ProbeRejected);
        }
        doctor
            .validate_lease_under_fresh_authority(self, admission_guard, lifecycle_guard)
            .await
    }
}

#[derive(Clone)]
pub struct NativeRuntimeDoctor {
    sidecars: Arc<Sidecars>,
    package: Arc<NativePackageSelection>,
    jobworkerp: JobworkerpHandle,
    manager: NativeTerminalManager,
    journal: Arc<NativeRuntimeDoctorStore>,
    accepting: Arc<AtomicBool>,
    source_inspection: Arc<Mutex<NativeSourceInspection>>,
    cli_probe_observation: Arc<Mutex<NativeCliProbeObservation>>,
    unresolved_known_probes: Arc<Mutex<std::collections::HashMap<String, KnownProbeIdentity>>>,
}

/// Current source facts for preparing a worker, not a readiness observation.
/// Paths are available only to native preparation and never enter wire DTOs.
pub(crate) struct NativePreparationSourceContext {
    pub(crate) host_worker_context: HostWorkerContext,
    pub(crate) source_digest: String,
    pub(crate) child_generation_id: u64,
}

#[derive(Clone)]
struct KnownProbeIdentity {
    job_id: i64,
    run_schema_digest: String,
}

impl std::fmt::Debug for NativeRuntimeDoctor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeRuntimeDoctor")
            .field("source_inspection", &self.source_inspection.lock())
            .field("cli_probe_observation", &self.cli_probe_observation.lock())
            .finish_non_exhaustive()
    }
}

impl NativeRuntimeDoctor {
    pub fn new(
        sidecars: Arc<Sidecars>,
        package: NativePackageSelection,
        jobworkerp: JobworkerpHandle,
        manager: NativeTerminalManager,
        journal: NativeRuntimeDoctorStore,
    ) -> Self {
        Self::new_with_shared_journal(sidecars, package, jobworkerp, manager, Arc::new(journal))
    }

    pub(crate) fn new_with_shared_journal(
        sidecars: Arc<Sidecars>,
        package: NativePackageSelection,
        jobworkerp: JobworkerpHandle,
        manager: NativeTerminalManager,
        journal: Arc<NativeRuntimeDoctorStore>,
    ) -> Self {
        Self::new_with_shared_journal_and_accepting(
            sidecars,
            package,
            jobworkerp,
            manager,
            journal,
            Arc::new(AtomicBool::new(true)),
        )
    }

    pub(crate) fn new_with_shared_journal_and_accepting(
        sidecars: Arc<Sidecars>,
        package: NativePackageSelection,
        jobworkerp: JobworkerpHandle,
        manager: NativeTerminalManager,
        journal: Arc<NativeRuntimeDoctorStore>,
        accepting: Arc<AtomicBool>,
    ) -> Self {
        Self {
            sidecars,
            package: Arc::new(package),
            jobworkerp,
            manager,
            journal,
            accepting,
            source_inspection: Arc::new(Mutex::new(NativeSourceInspection::NotObserved)),
            cli_probe_observation: Arc::new(Mutex::new(NativeCliProbeObservation::NotObserved)),
            unresolved_known_probes: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    pub fn source_inspection(&self) -> NativeSourceInspection {
        self.source_inspection.lock().clone()
    }

    pub fn cli_probe_observation(&self) -> NativeCliProbeObservation {
        self.cli_probe_observation.lock().clone()
    }

    /// Return only server-assigned IDs whose acceptance could not yet be
    /// journaled. Trusted native diagnostics may label these unresolved; they
    /// are never readiness evidence.
    #[allow(dead_code)] // consumed by the separate trusted-native diagnostics wiring
    pub(crate) fn unresolved_known_probe_ids(&self) -> Vec<i64> {
        let mut job_ids = self
            .unresolved_known_probes
            .lock()
            .values()
            .map(|identity| identity.job_id)
            .collect::<Vec<_>>();
        job_ids.sort_unstable();
        job_ids
    }

    fn remember_unresolved_probe(
        &self,
        operation_id: String,
        job_id: i64,
        run_schema_digest: String,
    ) {
        self.unresolved_known_probes.lock().insert(
            operation_id,
            KnownProbeIdentity {
                job_id,
                run_schema_digest,
            },
        );
    }

    fn forget_resolved_probe(&self, operation_id: &str, expected: &KnownProbeIdentity) {
        let mut unresolved = self.unresolved_known_probes.lock();
        if unresolved.get(operation_id).is_some_and(|current| {
            current.job_id == expected.job_id
                && current.run_schema_digest == expected.run_schema_digest
        }) {
            unresolved.remove(operation_id);
        }
    }

    /// Retry durable acceptance for an already-enqueued probe without ever
    /// replaying Enqueue. If the journal remains unavailable, the task keeps
    /// ownership of the stream until its original deadline and retains only
    /// the known job identity in the private in-memory failure registry.
    async fn track_unpersisted_probe(
        &self,
        operation_id: String,
        job_id: i64,
        run_schema_digest: String,
        mut probe: crate::jobworkerp::coding_agent::NativeVersionProbe,
    ) {
        let identity = KnownProbeIdentity {
            job_id,
            run_schema_digest: run_schema_digest.clone(),
        };
        loop {
            match self
                .journal
                .record_job_accepted(&operation_id, job_id, &run_schema_digest)
            {
                Ok(cancel_dispatch_claimed) => {
                    self.forget_resolved_probe(&operation_id, &identity);
                    if cancel_dispatch_claimed {
                        // The journal CAS is the sole authority to make this
                        // bounded Delete attempt; an RPC timeout is not retried.
                        let _ = dispatch_claimed_cancel(&self.jobworkerp, job_id).await;
                    }
                    let _ = drain_probe(
                        &mut probe,
                        &self.jobworkerp,
                        &self.journal,
                        &operation_id,
                        job_id,
                        cancel_dispatch_claimed,
                    )
                    .await;
                    // This caller already received Unknown/Unavailable, so
                    // terminal stream facts must never be promoted to a lease.
                    let _ = self.journal.mark_unknown(&operation_id);
                    return;
                }
                Err(_) => {
                    let remaining = probe
                        .deadline()
                        .saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
                }
            }
        }

        // Do not read subprocess output after the deadline when acceptance
        // could not be durably confirmed. Persist the known native identity as
        // Unknown if storage recovered; otherwise retain it for reconciliation.
        if let Ok(cancel_dispatch_claimed) =
            self.journal
                .record_unknown_job_id(&operation_id, job_id, &run_schema_digest)
        {
            self.forget_resolved_probe(&operation_id, &identity);
            if cancel_dispatch_claimed {
                let _ = dispatch_claimed_cancel(&self.jobworkerp, job_id).await;
            }
        }
        // Dropping at the operation deadline bounds stream ownership without
        // claiming that cancellation or physical shutdown succeeded.
        drop(probe);
    }

    /// Explicitly retry persistence of accepted identities retained after a
    /// journal failure. Any pending cancellation is dispatched only when the
    /// durable journal atomically grants its one-shot claim.
    #[allow(dead_code)] // consumed by the separate trusted-native diagnostics wiring
    pub(crate) async fn reconcile_unresolved_known_probes(&self) -> usize {
        let unresolved = self.unresolved_known_probes.lock().clone();
        let mut reconciled = 0;
        for (operation_id, identity) in unresolved {
            if let Ok(cancel_dispatch_claimed) = self.journal.record_unknown_job_id(
                &operation_id,
                identity.job_id,
                &identity.run_schema_digest,
            ) {
                self.forget_resolved_probe(&operation_id, &identity);
                if cancel_dispatch_claimed {
                    let _ = dispatch_claimed_cancel(&self.jobworkerp, identity.job_id).await;
                }
                reconciled += 1;
            }
        }
        reconciled
    }

    /// Start one explicit, durable preview probe. The owned task outlives a
    /// dropped caller future, so a known stream is still drained to its fixed
    /// deadline instead of being treated as closed or ready.
    pub async fn observe_preview(
        &self,
        owner_window_label: impl Into<String>,
        preview_id: impl Into<String>,
        preview_digest: impl Into<String>,
    ) -> Result<ObservedRuntimeLease, NativeRuntimeDoctorError> {
        let doctor = self.clone();
        let owner_window_label = owner_window_label.into();
        let preview_id = preview_id.into();
        let preview_digest = preview_digest.into();
        tokio::spawn(async move {
            doctor
                .observe_preview_owned(&owner_window_label, &preview_id, &preview_digest)
                .await
        })
        .await
        .map_err(|_| NativeRuntimeDoctorError::TaskFailed)?
    }

    /// Atomic native start entry point. The same Settings-admission guard and
    /// sidecar lifecycle guard cover fresh source/authority validation, the
    /// durable start claim, and opening the direct interactive stream.
    pub(crate) async fn start_observed(
        &self,
        lease: Option<&ObservedRuntimeLease>,
        owner_window_label: &str,
        preview_id: &str,
        preview_digest: &str,
    ) -> Result<super::NativeSessionRecord, NativeRuntimeDoctorError> {
        validate_preview_request(owner_window_label, preview_id, preview_digest)?;
        let admission_guard = self
            .manager
            .lock_admission_if_accepting(&self.accepting)
            .await
            .map_err(map_manager_error)?;
        let lifecycle_guard = self.sidecars.lock_native_jobworkerp_admission().await;
        if let Some(record) = self
            .manager
            .existing_session_for_preview(owner_window_label, preview_id, preview_digest)
            .map_err(map_manager_error)?
        {
            return Ok(record);
        }

        let lease = lease.ok_or(NativeRuntimeDoctorError::ProbeRejected)?;
        lease
            .validate_for_preview_admitted(
                self,
                owner_window_label,
                preview_id,
                preview_digest,
                &admission_guard,
                &lifecycle_guard,
            )
            .await?;
        self.manager
            .start_admitted(
                &admission_guard,
                owner_window_label,
                preview_id,
                preview_digest,
            )
            .await
            .map_err(map_start_manager_error)
    }

    async fn observe_preview_owned(
        &self,
        owner_window_label: &str,
        preview_id: &str,
        preview_digest: &str,
    ) -> Result<ObservedRuntimeLease, NativeRuntimeDoctorError> {
        validate_preview_request(owner_window_label, preview_id, preview_digest)?;
        let admission_guard = self
            .manager
            .lock_admission_if_accepting(&self.accepting)
            .await
            .map_err(map_manager_error)?;
        let lifecycle_guard = self.sidecars.lock_native_jobworkerp_admission().await;
        let snapshot = self
            .sidecars
            .managed_jobworkerp_spawn_snapshot()
            .ok_or(NativeRuntimeDoctorError::ManagedChildUnavailable)?;
        let (context, matched) = self
            .manager
            .staged_native_candidate_with_current_authority(
                &admission_guard,
                owner_window_label,
                preview_id,
                preview_digest,
            )
            .await
            .map_err(map_manager_error)?;
        let source = self.inspect_snapshot_sources(&snapshot, &matched)?;
        *self.source_inspection.lock() = NativeSourceInspection::Observed {
            selected_config_sha256: source.selected_config_sha256.clone(),
            effective_home_sha256: digest_path(SDK_HOME_DOMAIN, &source.effective_sdk_home),
            sdk_declared_pin: NATIVE_SDK_DECLARED_PIN.to_owned(),
        };
        let runtime_image = self
            .package
            .verify_managed_runtime(&snapshot)
            .map_err(map_runtime_image_error)?;
        let context_digest = context
            .digest()
            .map_err(|_| NativeRuntimeDoctorError::PreviewUnavailable)?;
        let operation_id = new_uuid().ok_or(NativeRuntimeDoctorError::JournalUnavailable)?;
        let worker_id = matched.worker_id().value;
        let runner_id = matched.runner_id().value;
        let prepared_cwd = context.launch_plan().working_directory().to_owned();
        let intent = NativeProbeIntent::new(
            operation_id.clone(),
            owner_window_label,
            preview_id,
            preview_digest,
            context_digest.clone(),
            runtime_image.sha256().to_owned(),
            runtime_image.package_manifest_sha256().to_owned(),
        )
        .and_then(|intent| {
            intent.with_probe_binding(
                source.selected_config_sha256.clone(),
                worker_id,
                runner_id,
                matched.settings_digest().to_owned(),
                matched.method_schema_digest().to_owned(),
                prepared_cwd.clone(),
            )
        })
        .map_err(|_| NativeRuntimeDoctorError::JournalUnavailable)?;
        self.journal
            .begin_probe(intent)
            .map_err(|_| NativeRuntimeDoctorError::JournalUnavailable)?;

        let mut probe = match self
            .jobworkerp
            .start_native_agent_version_probe(
                &matched,
                &prepared_cwd,
                context.launch_plan().command(),
                NativeVersionProbeOptions {
                    timeout: NATIVE_PROBE_TIMEOUT,
                },
            )
            .await
        {
            Ok(probe) => probe,
            Err(_) => {
                let _ = self.journal.mark_unknown(&operation_id);
                return Err(NativeRuntimeDoctorError::ProbeOutcomeUnknown);
            }
        };
        let job_id = probe.job_id().value;
        let run_schema_digest = probe.run_schema_digest().to_owned();
        if job_id <= 0 {
            let _ = self.journal.mark_unknown(&operation_id);
            return Err(NativeRuntimeDoctorError::ProbeOutcomeUnknown);
        }
        let cancellation_dispatch_claimed =
            match self
                .journal
                .record_job_accepted(&operation_id, job_id, &run_schema_digest)
            {
                Ok(dispatch_claimed) => dispatch_claimed,
                Err(_) => {
                    self.remember_unresolved_probe(
                        operation_id.clone(),
                        job_id,
                        run_schema_digest.clone(),
                    );
                    let doctor = self.clone();
                    tokio::spawn(async move {
                        doctor
                            .track_unpersisted_probe(operation_id, job_id, run_schema_digest, probe)
                            .await;
                    });
                    drop(lifecycle_guard);
                    drop(admission_guard);
                    return Err(NativeRuntimeDoctorError::JournalUnavailable);
                }
            };
        let probe_binding_matches = probe.worker_id().value == worker_id
            && probe.settings_digest() == matched.settings_digest()
            && probe.matched_runner_schema_digest() == matched.method_schema_digest();
        drop(lifecycle_guard);
        drop(admission_guard);

        if cancellation_dispatch_claimed {
            let _ = dispatch_claimed_cancel(&self.jobworkerp, job_id).await;
        }
        if !probe_binding_matches {
            let _ = self.journal.mark_unknown(&operation_id);
            let _ =
                dispatch_cancel_once(&self.jobworkerp, &self.journal, &operation_id, job_id).await;
        }
        let drain = drain_probe(
            &mut probe,
            &self.jobworkerp,
            &self.journal,
            &operation_id,
            job_id,
            cancellation_dispatch_claimed || !probe_binding_matches,
        )
        .await;
        if !probe_binding_matches {
            return Err(NativeRuntimeDoctorError::ProbeOutcomeUnknown);
        }
        let output_digest = sha256(&drain.stdout);
        let validation = validate_native_cli_probe_for_profile(
            context.cli_profile(),
            &drain.stdout,
            &drain.stderr,
            drain.exit_code,
            drain.end_received,
            drain.stream_eof,
            drain.stream_error,
        );
        let (version, rejection) = match &validation {
            Ok(result) => (Some(result.version().to_owned()), None),
            Err(error) => (None, Some(error.clone())),
        };
        *self.cli_probe_observation.lock() = NativeCliProbeObservation::Completed {
            job_id,
            version: version.clone(),
            exit_code: drain.exit_code,
            end_received: drain.end_received,
            stream_eof: drain.stream_eof,
            stdout_sha256: Some(output_digest.clone()),
            rejection: rejection.clone(),
        };
        let Some(result) = validation.ok() else {
            let observed_version =
                parse_native_cli_version(context.cli_profile().kind, &drain.stdout);
            let unknown_evidence =
                crate::native_terminal::doctor_store::NativeProbeUnknownEvidence {
                    stdout_sha256: (drain.stdout.len() <= MAX_NATIVE_PROBE_STDOUT_BYTES)
                        .then(|| sha256(&drain.stdout)),
                    // This legacy column records successful version-format parsing.
                    stdout_matches_pinned_version: Some(observed_version.is_some()),
                    stderr_empty: Some(drain.stderr.is_empty()),
                    stream_error: Some(drain.stream_error),
                    cli_version: observed_version,
                    exit_code: drain.exit_code,
                    end_received: Some(drain.end_received),
                    stream_eof: Some(drain.stream_eof),
                };
            return match self
                .journal
                .finish_probe_unknown(&operation_id, &unknown_evidence)
            {
                Ok(()) => Err(NativeRuntimeDoctorError::ProbeRejected),
                Err(_) => {
                    let _ = self.journal.mark_unknown(&operation_id);
                    Err(NativeRuntimeDoctorError::JournalUnavailable)
                }
            };
        };
        let evidence = NativeProbeEvidence {
            stdout_sha256: output_digest.clone(),
            stdout_matches_pinned_version: true,
            stderr_empty: drain.stderr.is_empty(),
            stream_error: false,
            cli_version: result.version().to_owned(),
            exit_code: result.exit_code(),
            end_received: result.end_received(),
            stream_eof: result.stream_eof(),
        };
        let observed = match self.journal.finish_probe(&operation_id, &evidence) {
            Ok(observed) => observed,
            Err(_) => {
                let _ = self.journal.mark_unknown(&operation_id);
                return Err(NativeRuntimeDoctorError::JournalUnavailable);
            }
        };
        if !observed {
            return Err(NativeRuntimeDoctorError::ProbeOutcomeUnknown);
        }

        self.revalidate_after_probe(
            owner_window_label,
            preview_id,
            preview_digest,
            &context,
            &matched,
            &snapshot,
            &source,
            &runtime_image,
            &run_schema_digest,
        )
        .await?;

        Ok(ObservedRuntimeLease {
            owner_window_label: owner_window_label.to_owned(),
            preview_id: preview_id.to_owned(),
            preview_digest: preview_digest.to_owned(),
            context_digest,
            expected_settings_revision: context.preview().expected_settings_revision(),
            expected_issue_revision: context.preview().expected_issue_revision(),
            profile_id: context.profile().profile_id.clone(),
            worker_id,
            runner_id,
            worker_settings_digest: matched.settings_digest().to_owned(),
            matched_runner_schema_digest: matched.method_schema_digest().to_owned(),
            run_schema_digest,
            runtime_image,
            sdk_source: source,
            sdk_declared_pin: NATIVE_SDK_DECLARED_PIN.to_owned(),
            prepared_cwd,
            cli_version: result.version().to_owned(),
            probe_job_id: job_id,
            operation_id,
            stdout_sha256: output_digest,
            exit_code: result.exit_code(),
            end_received: result.end_received(),
            stream_eof: result.stream_eof(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn revalidate_after_probe(
        &self,
        owner_window_label: &str,
        preview_id: &str,
        preview_digest: &str,
        context: &NativeSessionContext,
        matched: &NativeWorkerMatch,
        expected_snapshot: &ManagedJobworkerpSpawnSnapshot,
        expected_source: &SdkConfigurationProof,
        expected_image: &NativeRuntimeImageObservation,
        expected_run_schema_digest: &str,
    ) -> Result<(), NativeRuntimeDoctorError> {
        let admission_guard = self.manager.lock_admission().await;
        let _lifecycle_guard = self.sidecars.lock_native_jobworkerp_admission().await;
        let current_snapshot = self
            .sidecars
            .managed_jobworkerp_spawn_snapshot()
            .ok_or(NativeRuntimeDoctorError::ManagedChildUnavailable)?;
        if current_snapshot.child_pid != expected_snapshot.child_pid
            || current_snapshot.generation_id != expected_snapshot.generation_id
        {
            return Err(NativeRuntimeDoctorError::ManagedChildUnavailable);
        }
        let (current_context, current_match) = self
            .manager
            .staged_native_candidate_with_current_authority(
                &admission_guard,
                owner_window_label,
                preview_id,
                preview_digest,
            )
            .await
            .map_err(map_manager_error)?;
        if current_context.digest().ok() != context.digest().ok()
            || current_match.worker_id() != matched.worker_id()
            || current_match.runner_id() != matched.runner_id()
            || current_match.settings_digest() != matched.settings_digest()
            || current_match.method_schema_digest() != matched.method_schema_digest()
        {
            return Err(NativeRuntimeDoctorError::PreviewUnavailable);
        }
        let current_run_schema_digest = self
            .jobworkerp
            .revalidate_native_version_probe_match(&current_match)
            .await
            .map_err(|_| NativeRuntimeDoctorError::PreviewUnavailable)?;
        if current_run_schema_digest != expected_run_schema_digest {
            return Err(NativeRuntimeDoctorError::PreviewUnavailable);
        }
        let current_source = self.inspect_snapshot_sources(&current_snapshot, &current_match)?;
        if &current_source != expected_source {
            return Err(NativeRuntimeDoctorError::SdkSourceChanged);
        }
        let current_image = self
            .package
            .verify_managed_runtime(&current_snapshot)
            .map_err(map_runtime_image_error)?;
        if &current_image != expected_image {
            return Err(NativeRuntimeDoctorError::RuntimeImageUnavailable);
        }
        Ok(())
    }

    async fn validate_lease_under_fresh_authority(
        &self,
        lease: &ObservedRuntimeLease,
        admission_guard: &NativeManagerAdmissionGuard,
        _lifecycle_guard: &ManagedJobworkerpAdmissionGuard<'_>,
    ) -> Result<(), NativeRuntimeDoctorError> {
        match self
            .journal
            .operation_state(&lease.operation_id)
            .map_err(|_| NativeRuntimeDoctorError::JournalUnavailable)?
        {
            Some(crate::native_terminal::doctor_store::NativeProbeOperationState::Observed {
                job_id,
            }) if job_id == lease.probe_job_id => {}
            _ => return Err(NativeRuntimeDoctorError::ProbeOutcomeUnknown),
        }
        let snapshot = self
            .sidecars
            .managed_jobworkerp_spawn_snapshot()
            .ok_or(NativeRuntimeDoctorError::ManagedChildUnavailable)?;
        if snapshot.child_pid != lease.runtime_image.child_pid()
            || snapshot.generation_id != lease.runtime_image.child_generation_id()
        {
            return Err(NativeRuntimeDoctorError::ManagedChildUnavailable);
        }
        let (context, matched) = self
            .manager
            .staged_native_candidate_with_current_authority(
                admission_guard,
                &lease.owner_window_label,
                &lease.preview_id,
                &lease.preview_digest,
            )
            .await
            .map_err(map_manager_error)?;
        if context.digest().ok().as_deref() != Some(&lease.context_digest)
            || context.preview().expected_settings_revision() != lease.expected_settings_revision
            || context.preview().expected_issue_revision() != lease.expected_issue_revision
            || context.profile().profile_id != lease.profile_id
            || matched.worker_id().value != lease.worker_id
            || matched.runner_id().value != lease.runner_id
            || matched.settings_digest() != lease.worker_settings_digest
            || matched.method_schema_digest() != lease.matched_runner_schema_digest
            || context.launch_plan().working_directory() != lease.prepared_cwd
        {
            return Err(NativeRuntimeDoctorError::PreviewUnavailable);
        }
        let run_schema_digest = self
            .jobworkerp
            .revalidate_native_version_probe_match(&matched)
            .await
            .map_err(|_| NativeRuntimeDoctorError::PreviewUnavailable)?;
        if run_schema_digest != lease.run_schema_digest {
            return Err(NativeRuntimeDoctorError::PreviewUnavailable);
        }
        let source = self.inspect_snapshot_sources(&snapshot, &matched)?;
        if source != lease.sdk_source {
            return Err(NativeRuntimeDoctorError::SdkSourceChanged);
        }
        let image = self
            .package
            .verify_managed_runtime(&snapshot)
            .map_err(map_runtime_image_error)?;
        if image != lease.runtime_image {
            return Err(NativeRuntimeDoctorError::RuntimeImageUnavailable);
        }
        Ok(())
    }

    fn inspect_snapshot_sources(
        &self,
        snapshot: &ManagedJobworkerpSpawnSnapshot,
        matched: &NativeWorkerMatch,
    ) -> Result<SdkConfigurationProof, NativeRuntimeDoctorError> {
        self.check_sdk_config_source(snapshot)?;
        let config_path = snapshot
            .applied_sdk_config_path
            .as_deref()
            .ok_or(NativeRuntimeDoctorError::SdkSourceUnavailable)?;
        if config_path != matched.host_context().backend_config_path {
            return Err(NativeRuntimeDoctorError::SdkContextMismatch);
        }
        let selected = read_secure_json(config_path)?;
        verify_selected_config_stamp(snapshot, config_path, &selected)?;
        let managed_path = Path::new(MANAGED_MICROSANDBOX_CONFIG);
        let managed = read_secure_json(managed_path)?;
        let selected_value = parse_config_object(&selected)?;
        let managed_config = parse_managed_config(&managed)?;
        let selected_home = parse_config_home(&selected_value)?
            .map(|home| resolve_config_path(config_path, home))
            .transpose()?;
        if parse_paths_overrides(selected_value.get("paths"))? || managed_config.paths_override {
            return Err(NativeRuntimeDoctorError::SdkPathOverrideUnavailable);
        }
        let effective_home = match managed_config.home {
            HomeOverride::Unspecified => selected_home,
            HomeOverride::Clear => None,
            HomeOverride::Set(home) => Some(resolve_config_path(managed_path, home)?),
        }
        .or_else(|| snapshot.applied_sdk_home.as_ref().map(PathBuf::from))
        .ok_or(NativeRuntimeDoctorError::SdkConfigUnavailable)?;
        let effective_home = validate_trusted_sdk_directory(&effective_home)?;
        if effective_home != matched.host_context().microsandbox_sdk_home {
            return Err(NativeRuntimeDoctorError::SdkContextMismatch);
        }
        let current_config = read_secure_json(config_path)?;
        let current_managed = read_secure_json(managed_path)?;
        if current_config != selected || current_managed != managed {
            return Err(NativeRuntimeDoctorError::SdkSourceChanged);
        }
        self.check_sdk_config_source(snapshot)?;
        let source_digest = composite_sdk_source_digest(&selected, &managed, &effective_home);
        Ok(SdkConfigurationProof {
            selected_path: config_path.to_path_buf(),
            selected_config_sha256: selected.digest.clone(),
            source_digest,
            effective_sdk_home: effective_home,
            selected_file: selected.identity,
            managed_file: managed.identity,
        })
    }

    fn check_sdk_config_source(
        &self,
        snapshot: &ManagedJobworkerpSpawnSnapshot,
    ) -> Result<(), NativeRuntimeDoctorError> {
        match self.sidecars.managed_jobworkerp_sdk_source_check(snapshot) {
            ManagedJobworkerpSdkSourceCheck::Matches => Ok(()),
            ManagedJobworkerpSdkSourceCheck::Changed => {
                Err(NativeRuntimeDoctorError::SdkSourceChanged)
            }
            ManagedJobworkerpSdkSourceCheck::Unavailable(_) => {
                Err(NativeRuntimeDoctorError::SdkSourceUnavailable)
            }
        }
    }
}

/// Inspect only the actual managed-child selected SDK sources before worker
/// configuration. The caller gets the resolved host context needed by native
/// preparation, but no SDK/runtime readiness claim. This uses the same secure
/// file readers and spawn-time stamps as preview observation.
pub(crate) async fn inspect_current_preparation_source(
    sidecars: &Sidecars,
) -> Result<NativePreparationSourceContext, NativeRuntimeDoctorError> {
    let lifecycle_guard = sidecars.lock_native_jobworkerp_admission().await;
    inspect_current_preparation_source_admitted(sidecars, &lifecycle_guard)
}

/// Same inspection for a caller that already holds the admission guard. The
/// lifecycle lock is not reentrant, so re-acquiring it would deadlock.
pub(crate) fn inspect_current_preparation_source_admitted(
    sidecars: &Sidecars,
    _lifecycle_guard: &ManagedJobworkerpAdmissionGuard<'_>,
) -> Result<NativePreparationSourceContext, NativeRuntimeDoctorError> {
    let snapshot = sidecars
        .managed_jobworkerp_spawn_snapshot()
        .ok_or(NativeRuntimeDoctorError::ManagedChildUnavailable)?;
    let selected_path = snapshot
        .applied_sdk_config_path
        .as_deref()
        .ok_or(NativeRuntimeDoctorError::SdkSourceUnavailable)?;
    if !matches!(
        sidecars.managed_jobworkerp_sdk_source_check(&snapshot),
        ManagedJobworkerpSdkSourceCheck::Matches
    ) {
        return Err(NativeRuntimeDoctorError::SdkSourceChanged);
    }

    let selected = read_secure_json(selected_path)?;
    verify_selected_config_stamp(&snapshot, selected_path, &selected)?;
    let managed_path = Path::new(MANAGED_MICROSANDBOX_CONFIG);
    let managed = read_secure_json(managed_path)?;
    let selected_value = parse_config_object(&selected)?;
    let managed_config = parse_managed_config(&managed)?;
    let selected_home = parse_config_home(&selected_value)?
        .map(|home| resolve_config_path(selected_path, home))
        .transpose()?;
    if parse_paths_overrides(selected_value.get("paths"))? || managed_config.paths_override {
        return Err(NativeRuntimeDoctorError::SdkPathOverrideUnavailable);
    }
    let effective_home = match managed_config.home {
        HomeOverride::Unspecified => selected_home,
        HomeOverride::Clear => None,
        HomeOverride::Set(home) => Some(resolve_config_path(managed_path, home)?),
    }
    .or_else(|| snapshot.applied_sdk_home.as_ref().map(PathBuf::from))
    .ok_or(NativeRuntimeDoctorError::SdkConfigUnavailable)?;
    let effective_home = validate_trusted_sdk_directory(&effective_home)?;

    let current_selected = read_secure_json(selected_path)?;
    let current_managed = read_secure_json(managed_path)?;
    if current_selected != selected || current_managed != managed {
        return Err(NativeRuntimeDoctorError::SdkSourceChanged);
    }
    match sidecars.managed_jobworkerp_sdk_source_check(&snapshot) {
        ManagedJobworkerpSdkSourceCheck::Matches => {}
        ManagedJobworkerpSdkSourceCheck::Changed
        | ManagedJobworkerpSdkSourceCheck::Unavailable(_) => {
            return Err(NativeRuntimeDoctorError::SdkSourceChanged);
        }
    }
    let source_digest = composite_sdk_source_digest(&selected, &managed, &effective_home);
    Ok(NativePreparationSourceContext {
        host_worker_context: HostWorkerContext {
            microsandbox_sdk_home: effective_home,
            backend_config_path: selected_path.to_path_buf(),
        },
        source_digest,
        child_generation_id: snapshot.generation_id,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SdkConfigurationProof {
    selected_path: PathBuf,
    selected_config_sha256: String,
    source_digest: String,
    effective_sdk_home: PathBuf,
    selected_file: ConfigFileIdentity,
    managed_file: ConfigFileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SecureJsonFile {
    bytes: Vec<u8>,
    digest: String,
    identity: ConfigFileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ConfigFileIdentity {
    Missing,
    Present {
        stat: SecureFileStat,
        parent_device: u64,
        parent_inode: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SecureFileStat {
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

#[derive(Clone, Copy)]
enum ConfigTrust {
    User,
    Machine,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManagedConfigObservation {
    home: HomeOverride,
    paths_override: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum HomeOverride {
    Unspecified,
    Clear,
    Set(PathBuf),
}

struct NativeProbeDrain {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
    end_received: bool,
    stream_eof: bool,
    stream_error: bool,
}

async fn drain_probe(
    probe: &mut crate::jobworkerp::coding_agent::NativeVersionProbe,
    jobworkerp: &JobworkerpHandle,
    journal: &NativeRuntimeDoctorStore,
    operation_id: &str,
    job_id: i64,
    cancellation_already_claimed: bool,
) -> NativeProbeDrain {
    let mut drain = NativeProbeDrain {
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit_code: None,
        end_received: false,
        stream_eof: false,
        stream_error: false,
    };
    let mut cancel_claim_attempted = cancellation_already_claimed;
    let mut cancel_deadline =
        cancellation_already_claimed.then(|| tokio::time::Instant::now() + PROBE_CANCEL_GRACE);

    loop {
        let next_event = if let Some(deadline) = cancel_deadline {
            tokio::time::timeout_at(deadline, probe.next_event())
                .await
                .ok()
        } else {
            Some(probe.next_event().await)
        };
        let Some(next_event) = next_event else {
            drain.stream_error = true;
            break;
        };
        match next_event {
            Ok(Some(CodingAgentEvent::Output { stream, data })) => match stream.as_str() {
                "stdout" => {
                    if append_bounded_probe_output(&mut drain.stdout, &data)
                        && !cancel_claim_attempted
                    {
                        cancel_claim_attempted = true;
                        cancel_deadline = Some(tokio::time::Instant::now() + PROBE_CANCEL_GRACE);
                        if !dispatch_cancel_once(jobworkerp, journal, operation_id, job_id).await {
                            drain.stream_error = true;
                        }
                    }
                }
                "stderr" => {
                    if append_bounded_probe_output(&mut drain.stderr, &data)
                        && !cancel_claim_attempted
                    {
                        cancel_claim_attempted = true;
                        cancel_deadline = Some(tokio::time::Instant::now() + PROBE_CANCEL_GRACE);
                        if !dispatch_cancel_once(jobworkerp, journal, operation_id, job_id).await {
                            drain.stream_error = true;
                        }
                    }
                }
                _ => {
                    drain.stream_error = true;
                    if !cancel_claim_attempted {
                        cancel_claim_attempted = true;
                        cancel_deadline = Some(tokio::time::Instant::now() + PROBE_CANCEL_GRACE);
                        if !dispatch_cancel_once(jobworkerp, journal, operation_id, job_id).await {
                            drain.stream_error = true;
                        }
                    }
                }
            },
            Ok(Some(CodingAgentEvent::Exit { exit_code, .. })) => {
                drain.exit_code = Some(exit_code);
            }
            Ok(None) => {
                // CodingAgentStream yields None only after a valid sandbox End
                // and clean gRPC EOF, and only after it has observed Exit.
                drain.end_received = drain.exit_code.is_some();
                drain.stream_eof = drain.end_received;
                break;
            }
            Err(_) => {
                drain.stream_error = true;
                if !cancel_claim_attempted
                    && !dispatch_cancel_once(jobworkerp, journal, operation_id, job_id).await
                {
                    drain.stream_error = true;
                }
                break;
            }
        }
    }

    drain
}

fn append_bounded_probe_output(target: &mut Vec<u8>, bytes: &[u8]) -> bool {
    let limit_with_sentinel = MAX_NATIVE_PROBE_STDOUT_BYTES.saturating_add(1);
    let available = limit_with_sentinel.saturating_sub(target.len());
    target.extend_from_slice(&bytes[..bytes.len().min(available)]);
    target.len() > MAX_NATIVE_PROBE_STDOUT_BYTES || bytes.len() > available
}

async fn dispatch_cancel_once(
    jobworkerp: &JobworkerpHandle,
    journal: &NativeRuntimeDoctorStore,
    operation_id: &str,
    job_id: i64,
) -> bool {
    match journal.claim_cancel(operation_id) {
        Ok(NativeProbeCancelClaim::Dispatch { job_id: claimed }) if claimed == job_id => {
            dispatch_claimed_cancel(jobworkerp, claimed).await
        }
        Ok(NativeProbeCancelClaim::AlreadyClaimed) => true,
        Ok(
            NativeProbeCancelClaim::Dispatch { .. }
            | NativeProbeCancelClaim::PendingAcceptance
            | NativeProbeCancelClaim::NotFound,
        )
        | Err(_) => false,
    }
}

async fn dispatch_claimed_cancel(jobworkerp: &JobworkerpHandle, job_id: i64) -> bool {
    tokio::time::timeout(
        PROBE_CANCEL_RPC_TIMEOUT,
        jobworkerp.cancel(JobId { value: job_id }),
    )
    .await
    .is_ok_and(|result| result.is_ok())
}

fn validate_preview_request(
    owner_window_label: &str,
    preview_id: &str,
    preview_digest: &str,
) -> Result<(), NativeRuntimeDoctorError> {
    if owner_window_label.is_empty()
        || owner_window_label.len() > MAX_OWNER_WINDOW_LABEL_BYTES
        || owner_window_label.chars().any(char::is_control)
        || !is_canonical_uuid(preview_id)
        || !is_sha256(preview_digest)
    {
        return Err(NativeRuntimeDoctorError::InvalidRequest);
    }
    Ok(())
}

fn map_manager_error(error: NativeTerminalManagerError) -> NativeRuntimeDoctorError {
    match error {
        NativeTerminalManagerError::AdmissionClosed => NativeRuntimeDoctorError::NotAccepting,
        NativeTerminalManagerError::InvalidRequest => NativeRuntimeDoctorError::InvalidRequest,
        NativeTerminalManagerError::NotFound
        | NativeTerminalManagerError::OwnerMismatch
        | NativeTerminalManagerError::Conflict
        | NativeTerminalManagerError::WorkerMismatch
        | NativeTerminalManagerError::AuthorityUnavailable
        | NativeTerminalManagerError::AuthorityChanged
        | NativeTerminalManagerError::SessionNotRunning
        | NativeTerminalManagerError::OutputGap
        | NativeTerminalManagerError::Transport
        | NativeTerminalManagerError::Store(_)
        | NativeTerminalManagerError::Output(_) => NativeRuntimeDoctorError::PreviewUnavailable,
    }
}

fn map_start_manager_error(error: NativeTerminalManagerError) -> NativeRuntimeDoctorError {
    match error {
        NativeTerminalManagerError::Transport
        | NativeTerminalManagerError::Store(_)
        | NativeTerminalManagerError::Output(_) => NativeRuntimeDoctorError::StartOutcomeUnknown,
        other => map_manager_error(other),
    }
}

fn map_runtime_image_error(_: NativeRuntimeImageUnavailable) -> NativeRuntimeDoctorError {
    NativeRuntimeDoctorError::RuntimeImageUnavailable
}

fn new_uuid() -> Option<String> {
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

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn digest_path(domain: &[u8], path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(path.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    hasher.update(path.as_os_str().to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn composite_sdk_source_digest(
    selected: &SecureJsonFile,
    managed: &SecureJsonFile,
    effective_home: &Path,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SDK_SOURCE_DOMAIN);
    hasher.update(selected.digest.as_bytes());
    update_config_identity(&mut hasher, &selected.identity);
    hasher.update(managed.digest.as_bytes());
    update_config_identity(&mut hasher, &managed.identity);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(effective_home.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    hasher.update(effective_home.as_os_str().to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn update_config_identity(hasher: &mut Sha256, identity: &ConfigFileIdentity) {
    match identity {
        ConfigFileIdentity::Missing => hasher.update([0]),
        ConfigFileIdentity::Present {
            stat,
            parent_device,
            parent_inode,
        } => {
            hasher.update([1]);
            for value in [
                stat.device,
                stat.inode,
                stat.length,
                *parent_device,
                *parent_inode,
            ] {
                hasher.update(value.to_le_bytes());
            }
            for value in [
                stat.modified_seconds,
                stat.modified_nanoseconds,
                stat.changed_seconds,
                stat.changed_nanoseconds,
            ] {
                hasher.update(value.to_le_bytes());
            }
            hasher.update(stat.mode.to_le_bytes());
            hasher.update(stat.owner.to_le_bytes());
        }
    }
}

fn verify_selected_config_stamp(
    snapshot: &ManagedJobworkerpSpawnSnapshot,
    selected_path: &Path,
    selected: &SecureJsonFile,
) -> Result<(), NativeRuntimeDoctorError> {
    match &snapshot.sdk_config_source {
        ManagedJobworkerpSdkSourceStamp::Missing {
            selected_path: stamped_path,
        } if stamped_path == selected_path && selected.identity == ConfigFileIdentity::Missing => {
            Ok(())
        }
        ManagedJobworkerpSdkSourceStamp::Present(stamp)
            if stamp.selected_path == selected_path && stamp_matches_file(stamp, selected) =>
        {
            Ok(())
        }
        ManagedJobworkerpSdkSourceStamp::Unavailable { .. } => {
            Err(NativeRuntimeDoctorError::SdkSourceUnavailable)
        }
        _ => Err(NativeRuntimeDoctorError::SdkSourceChanged),
    }
}

fn stamp_matches_file(
    stamp: &crate::sidecar::ManagedJobworkerpSdkSourceFileStamp,
    selected: &SecureJsonFile,
) -> bool {
    let ConfigFileIdentity::Present {
        stat,
        parent_device,
        parent_inode,
    } = &selected.identity
    else {
        return false;
    };
    selected.digest == sha256_array(&stamp.sha256)
        && stat.length == stamp.length
        && stat.device == stamp.device
        && stat.inode == stamp.inode
        && *parent_device == stamp.parent_device
        && *parent_inode == stamp.parent_inode
        && stat.modified_seconds == stamp.modified_seconds
        && stat.modified_nanoseconds == stamp.modified_nanoseconds
        && stat.changed_seconds == stamp.changed_seconds
        && stat.changed_nanoseconds == stamp.changed_nanoseconds
        && stat.mode == stamp.mode
        && stat.owner == stamp.owner
}

fn sha256_array(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(64);
    for byte in bytes {
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn parse_config_object(file: &SecureJsonFile) -> Result<Value, NativeRuntimeDoctorError> {
    let value = if file.identity == ConfigFileIdentity::Missing {
        Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_slice(&file.bytes)
            .map_err(|_| NativeRuntimeDoctorError::SdkConfigUnavailable)?
    };
    let object = value
        .as_object()
        .ok_or(NativeRuntimeDoctorError::SdkConfigUnavailable)?;
    if object
        .get("version")
        .is_some_and(|version| version.as_u64() != Some(1))
    {
        return Err(NativeRuntimeDoctorError::SdkConfigUnavailable);
    }
    Ok(value)
}

fn parse_config_home(value: &Value) -> Result<Option<PathBuf>, NativeRuntimeDoctorError> {
    match value.get("home") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(path)) => Ok(Some(PathBuf::from(path))),
        Some(_) => Err(NativeRuntimeDoctorError::SdkConfigUnavailable),
    }
}

fn parse_managed_config(
    file: &SecureJsonFile,
) -> Result<ManagedConfigObservation, NativeRuntimeDoctorError> {
    let value = parse_config_object(file)?;
    let object = value
        .as_object()
        .ok_or(NativeRuntimeDoctorError::SdkConfigUnavailable)?;
    let overrides = match object.get("overrides") {
        None => None,
        Some(Value::Object(overrides)) => Some(overrides),
        Some(_) => return Err(NativeRuntimeDoctorError::SdkConfigUnavailable),
    };
    let home = match overrides.and_then(|values| values.get("home")) {
        None => HomeOverride::Unspecified,
        Some(Value::Null) => HomeOverride::Clear,
        Some(Value::String(path)) => HomeOverride::Set(PathBuf::from(path)),
        Some(_) => return Err(NativeRuntimeDoctorError::SdkConfigUnavailable),
    };
    let paths_override = parse_paths_overrides(overrides.and_then(|values| values.get("paths")))?;
    Ok(ManagedConfigObservation {
        home,
        paths_override,
    })
}

fn parse_paths_overrides(value: Option<&Value>) -> Result<bool, NativeRuntimeDoctorError> {
    let Some(value) = value else {
        return Ok(false);
    };
    let paths = value
        .as_object()
        .ok_or(NativeRuntimeDoctorError::SdkConfigUnavailable)?;
    let mut override_present = false;
    for key in [
        "msb",
        "libkrunfw",
        "agentd",
        "cache",
        "sandboxes",
        "volumes",
        "snapshots",
        "logs",
        "secrets",
    ] {
        match paths.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::String(_)) => override_present = true,
            Some(_) => return Err(NativeRuntimeDoctorError::SdkConfigUnavailable),
        }
    }
    Ok(override_present)
}

fn resolve_config_path(
    _source_config: &Path,
    path: PathBuf,
) -> Result<PathBuf, NativeRuntimeDoctorError> {
    validate_absolute_path_syntax(&path)?;
    Ok(path)
}

fn validate_trusted_sdk_directory(path: &Path) -> Result<PathBuf, NativeRuntimeDoctorError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Err(NativeRuntimeDoctorError::SdkConfigUnavailable)
    }
    #[cfg(target_os = "linux")]
    {
        let (mut directories, filename) = path_components(path)?;
        directories.push(filename);
        let root =
            open_root_directory().map_err(|_| NativeRuntimeDoctorError::SdkConfigUnavailable)?;
        validate_config_directory(&root, ConfigTrust::User)
            .map_err(|_| NativeRuntimeDoctorError::SdkConfigUnavailable)?;
        let mut directory = root;
        for component in directories {
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    component.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::NotFound {
                    // The selected SDK may create its home later. The existing
                    // prefix was checked without following links; no writes occur.
                    return Ok(path.to_path_buf());
                }
                return Err(NativeRuntimeDoctorError::SdkConfigUnavailable);
            }
            let next = unsafe { File::from_raw_fd(fd) };
            validate_config_directory(&next, ConfigTrust::User)
                .map_err(|_| NativeRuntimeDoctorError::SdkConfigUnavailable)?;
            directory = next;
        }
        Ok(path.to_path_buf())
    }
}

fn validate_absolute_path_syntax(path: &Path) -> Result<(), NativeRuntimeDoctorError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
        || path.components().count() < 2
    {
        return Err(NativeRuntimeDoctorError::SdkConfigUnavailable);
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        if bytes.contains(&0)
            || bytes[1..]
                .split(|byte| *byte == b'/')
                .any(|part| part.is_empty() || part == b"." || part == b"..")
        {
            return Err(NativeRuntimeDoctorError::SdkConfigUnavailable);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_secure_json(path: &Path) -> Result<SecureJsonFile, NativeRuntimeDoctorError> {
    let trust = if path == Path::new(MANAGED_MICROSANDBOX_CONFIG) {
        ConfigTrust::Machine
    } else {
        ConfigTrust::User
    };
    let (parent, filename) = match open_config_parent(path, trust)? {
        Some(parent) => parent,
        None => return Ok(empty_secure_json()),
    };
    let parent_metadata = parent
        .metadata()
        .map_err(|_| NativeRuntimeDoctorError::SdkSourceUnavailable)?;
    let (parent_device, parent_inode) = metadata_identity(&parent_metadata);
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            filename.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(empty_secure_json());
        }
        return Err(NativeRuntimeDoctorError::SdkSourceUnavailable);
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let before_metadata = file
        .metadata()
        .map_err(|_| NativeRuntimeDoctorError::SdkSourceUnavailable)?;
    if !before_metadata.is_file() {
        return Err(NativeRuntimeDoctorError::SdkSourceUnavailable);
    }
    validate_config_file(&before_metadata, trust)
        .map_err(|_| NativeRuntimeDoctorError::SdkSourceUnavailable)?;
    if before_metadata.len() > MAX_NATIVE_SDK_CONFIG_BYTES {
        return Err(NativeRuntimeDoctorError::SdkSourceUnavailable);
    }
    let before = SecureFileStat::from_metadata(&before_metadata);
    let mut bytes = Vec::with_capacity(before.length as usize);
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| NativeRuntimeDoctorError::SdkSourceUnavailable)?;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > MAX_NATIVE_SDK_CONFIG_BYTES as usize {
            return Err(NativeRuntimeDoctorError::SdkSourceUnavailable);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    let after_metadata = file
        .metadata()
        .map_err(|_| NativeRuntimeDoctorError::SdkSourceUnavailable)?;
    if SecureFileStat::from_metadata(&after_metadata) != before
        || bytes.len() as u64 != before.length
    {
        return Err(NativeRuntimeDoctorError::SdkSourceChanged);
    }
    let path_fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            filename.as_ptr(),
            libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if path_fd < 0 {
        return Err(NativeRuntimeDoctorError::SdkSourceChanged);
    }
    let path_file = unsafe { File::from_raw_fd(path_fd) };
    let current_metadata = path_file
        .metadata()
        .map_err(|_| NativeRuntimeDoctorError::SdkSourceChanged)?;
    if !current_metadata.is_file() || SecureFileStat::from_metadata(&current_metadata) != before {
        return Err(NativeRuntimeDoctorError::SdkSourceChanged);
    }
    let current_parent_metadata = parent
        .metadata()
        .map_err(|_| NativeRuntimeDoctorError::SdkSourceChanged)?;
    if metadata_identity(&current_parent_metadata) != (parent_device, parent_inode) {
        return Err(NativeRuntimeDoctorError::SdkSourceChanged);
    }

    let digest = sha256(&bytes);
    Ok(SecureJsonFile {
        bytes,
        digest,
        identity: ConfigFileIdentity::Present {
            stat: before,
            parent_device,
            parent_inode,
        },
    })
}

#[cfg(not(target_os = "linux"))]
fn read_secure_json(_: &Path) -> Result<SecureJsonFile, NativeRuntimeDoctorError> {
    Err(NativeRuntimeDoctorError::SdkSourceUnavailable)
}

fn empty_secure_json() -> SecureJsonFile {
    let bytes = Vec::new();
    SecureJsonFile {
        digest: sha256(&bytes),
        bytes,
        identity: ConfigFileIdentity::Missing,
    }
}

#[cfg(target_os = "linux")]
fn open_config_parent(
    path: &Path,
    trust: ConfigTrust,
) -> Result<Option<(File, CString)>, NativeRuntimeDoctorError> {
    let (directories, filename) = path_components(path)?;
    let mut directory =
        open_root_directory().map_err(|_| NativeRuntimeDoctorError::SdkSourceUnavailable)?;
    validate_config_directory(&directory, trust)
        .map_err(|_| NativeRuntimeDoctorError::SdkSourceUnavailable)?;
    for component in directories {
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(NativeRuntimeDoctorError::SdkSourceUnavailable);
        }
        let next = unsafe { File::from_raw_fd(fd) };
        validate_config_directory(&next, trust)
            .map_err(|_| NativeRuntimeDoctorError::SdkSourceUnavailable)?;
        directory = next;
    }
    Ok(Some((directory, filename)))
}

#[cfg(target_os = "linux")]
fn path_components(path: &Path) -> Result<(Vec<CString>, CString), NativeRuntimeDoctorError> {
    use std::os::unix::ffi::OsStrExt;

    validate_absolute_path_syntax(path)?;
    let bytes = path.as_os_str().as_bytes();
    let parts = bytes[1..].split(|byte| *byte == b'/').collect::<Vec<_>>();
    let (file, directories) = parts
        .split_last()
        .ok_or(NativeRuntimeDoctorError::SdkSourceUnavailable)?;
    let directories = directories
        .iter()
        .map(|part| CString::new(*part).map_err(|_| NativeRuntimeDoctorError::SdkSourceUnavailable))
        .collect::<Result<Vec<_>, _>>()?;
    let filename =
        CString::new(*file).map_err(|_| NativeRuntimeDoctorError::SdkSourceUnavailable)?;
    Ok((directories, filename))
}

#[cfg(target_os = "linux")]
fn open_root_directory() -> std::io::Result<File> {
    let root = CString::new("/").expect("root path has no NUL bytes");
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn validate_config_directory(
    directory: &File,
    trust: ConfigTrust,
) -> Result<(), NativeRuntimeDoctorError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = directory
        .metadata()
        .map_err(|_| NativeRuntimeDoctorError::SdkSourceUnavailable)?;
    if !metadata.is_dir() {
        return Err(NativeRuntimeDoctorError::SdkSourceUnavailable);
    }
    let uid = unsafe { libc::geteuid() };
    let owner_trusted = match trust {
        ConfigTrust::User => metadata.uid() == uid || metadata.uid() == 0,
        ConfigTrust::Machine => metadata.uid() == 0,
    };
    let mode = metadata.mode() & 0o7777;
    let root_sticky = metadata.uid() == 0 && mode & 0o1000 != 0 && mode & !0o1777 == 0;
    if !owner_trusted || (mode & 0o022 != 0 && !root_sticky) {
        return Err(NativeRuntimeDoctorError::SdkSourceUnavailable);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_config_file(
    metadata: &fs::Metadata,
    trust: ConfigTrust,
) -> Result<(), NativeRuntimeDoctorError> {
    use std::os::unix::fs::MetadataExt;

    let owner_trusted = match trust {
        ConfigTrust::User => metadata.uid() == unsafe { libc::geteuid() } || metadata.uid() == 0,
        ConfigTrust::Machine => metadata.uid() == 0,
    };
    let mode = metadata.mode() & 0o7777;
    let permissions_trusted = match trust {
        ConfigTrust::User => mode & !0o644 == 0,
        ConfigTrust::Machine => mode & 0o022 == 0,
    };
    if !metadata.is_file() || !owner_trusted || !permissions_trusted {
        return Err(NativeRuntimeDoctorError::SdkSourceUnavailable);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn metadata_identity(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(target_os = "linux")]
impl SecureFileStat {
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
            mode: metadata.mode() & 0o7777,
            owner: metadata.uid(),
        }
    }
}

const MAX_OWNER_WINDOW_LABEL_BYTES: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_profile(
        kind: super::super::launch::NativeAgentKind,
        hint: &str,
    ) -> super::super::launch::NativeCliProfile {
        super::super::launch::NativeCliProfile {
            kind,
            cli_version: hint.to_owned(),
            runtime_image_digest: String::new(),
            expected_profile_command: Vec::new(),
        }
    }

    #[test]
    fn probe_extracts_actual_versions_without_matching_the_profile_hint() {
        let cases = [
            (
                super::super::launch::NativeAgentKind::OpenCode,
                "1.18.3",
                "2.4.0-beta.2+build.9",
            ),
            (
                super::super::launch::NativeAgentKind::Codex,
                "0.160.1",
                "0.200.0-nightly.4",
            ),
            (
                super::super::launch::NativeAgentKind::ClaudeCode,
                "2.1.292",
                "3.0.0-rc.1+linux",
            ),
        ];
        for (kind, hint, version) in cases {
            let stdout = match kind {
                super::super::launch::NativeAgentKind::OpenCode => version.to_owned(),
                super::super::launch::NativeAgentKind::Codex => format!("codex-cli {version}"),
                super::super::launch::NativeAgentKind::ClaudeCode => {
                    format!("{version} (Claude Code)")
                }
            };
            let result = validate_native_cli_probe_for_profile(
                &probe_profile(kind, hint),
                stdout.as_bytes(),
                b"warning: PATH was adjusted",
                Some(0),
                true,
                true,
                false,
            )
            .unwrap();
            assert_eq!(result.version(), version);
        }
    }

    #[test]
    fn probe_keeps_actual_version_out_of_result_when_stream_is_incomplete() {
        let profile = probe_profile(super::super::launch::NativeAgentKind::OpenCode, "");
        assert_eq!(
            validate_native_cli_probe_for_profile(
                &profile,
                b"2.5.0\n",
                b"",
                Some(0),
                false,
                true,
                false,
            ),
            Err(NativeCliProbeValidationError::IncompleteStream)
        );
    }

    #[test]
    fn probe_rejects_malformed_output_and_terminal_protocol_failures() {
        let profile = probe_profile(super::super::launch::NativeAgentKind::OpenCode, "");
        let valid = |stdout: &[u8], stderr: &[u8], exit_code, end, eof, stream_error| {
            validate_native_cli_probe_for_profile(
                &profile,
                stdout,
                stderr,
                exit_code,
                end,
                eof,
                stream_error,
            )
        };
        assert_eq!(
            valid(b"", b"", Some(0), true, true, false),
            Err(NativeCliProbeValidationError::InvalidVersionOutput)
        );
        assert_eq!(
            valid(b"2.5.0\nsecond", b"", Some(0), true, true, false),
            Err(NativeCliProbeValidationError::InvalidVersionOutput)
        );
        assert_eq!(
            valid(b"2.5.0\t", b"", Some(0), true, true, false),
            Err(NativeCliProbeValidationError::InvalidVersionOutput)
        );
        assert_eq!(
            valid(&[0xff], b"", Some(0), true, true, false),
            Err(NativeCliProbeValidationError::InvalidVersionOutput)
        );
        assert_eq!(
            valid(
                b"2.5.0",
                &vec![b'w'; MAX_NATIVE_PROBE_STDOUT_BYTES + 1],
                Some(0),
                true,
                true,
                false
            ),
            Err(NativeCliProbeValidationError::OutputTooLarge)
        );
        assert_eq!(
            valid(b"2.5.0", b"", Some(1), true, true, false),
            Err(NativeCliProbeValidationError::NonzeroExit)
        );
        assert_eq!(
            valid(b"2.5.0", b"", Some(0), true, true, true),
            Err(NativeCliProbeValidationError::StreamError)
        );
        assert_eq!(
            valid(b"2.5.0", b"", None, true, true, false),
            Err(NativeCliProbeValidationError::IncompleteStream)
        );
    }

    #[test]
    fn probe_requires_the_kind_specific_output_shape() {
        assert_eq!(
            validate_native_cli_probe_for_profile(
                &probe_profile(super::super::launch::NativeAgentKind::Codex, ""),
                b"2.5.0",
                b"",
                Some(0),
                true,
                true,
                false,
            ),
            Err(NativeCliProbeValidationError::InvalidVersionOutput)
        );
        assert_eq!(
            validate_native_cli_probe_for_profile(
                &probe_profile(super::super::launch::NativeAgentKind::ClaudeCode, ""),
                b"3.0.0",
                b"",
                Some(0),
                true,
                true,
                false,
            ),
            Err(NativeCliProbeValidationError::InvalidVersionOutput)
        );
    }

    fn present_json(bytes: &[u8]) -> SecureJsonFile {
        SecureJsonFile {
            bytes: bytes.to_vec(),
            digest: sha256(bytes),
            identity: ConfigFileIdentity::Present {
                stat: SecureFileStat {
                    device: 1,
                    inode: 2,
                    length: bytes.len() as u64,
                    modified_seconds: 3,
                    modified_nanoseconds: 4,
                    changed_seconds: 5,
                    changed_nanoseconds: 6,
                    mode: 0o600,
                    owner: 1000,
                },
                parent_device: 7,
                parent_inode: 8,
            },
        }
    }

    #[test]
    fn selected_home_is_overridden_by_managed_home_and_path_overrides_fail_closed() {
        let selected = present_json(br#"{"home":"/selected/sdk","paths":{"cache":null}}"#);
        let selected = parse_config_object(&selected).unwrap();
        assert_eq!(
            parse_config_home(&selected).unwrap(),
            Some(PathBuf::from("/selected/sdk"))
        );
        assert!(!parse_paths_overrides(selected.get("paths")).unwrap());

        let managed = present_json(
            br#"{"version":1,"overrides":{"home":"/managed/sdk","paths":{"cache":"/cache"}}}"#,
        );
        let managed = parse_managed_config(&managed).unwrap();
        assert_eq!(
            managed.home,
            HomeOverride::Set(PathBuf::from("/managed/sdk"))
        );
        assert!(managed.paths_override);

        assert!(matches!(
            resolve_config_path(
                Path::new("/etc/microsandbox/managed.json"),
                PathBuf::from("relative")
            ),
            Err(NativeRuntimeDoctorError::SdkConfigUnavailable)
        ));
        assert!(matches!(
            resolve_config_path(
                Path::new("/etc/microsandbox/managed.json"),
                PathBuf::from("/selected/../other")
            ),
            Err(NativeRuntimeDoctorError::SdkConfigUnavailable)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_selected_and_managed_configs_are_empty_without_writes() {
        let root = tempfile::tempdir().unwrap();
        let selected_path = root.path().join("config.json");
        let selected = read_secure_json(&selected_path).unwrap();
        assert_eq!(selected.identity, ConfigFileIdentity::Missing);
        assert!(
            parse_config_object(&selected)
                .unwrap()
                .as_object()
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_config_home(&parse_config_object(&selected).unwrap())
                .unwrap()
                .is_none()
        );
        assert!(!selected_path.exists());

        let managed = parse_managed_config(&empty_secure_json()).unwrap();
        assert_eq!(managed.home, HomeOverride::Unspecified);
        assert!(!managed.paths_override);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn selected_sdk_config_symlink_is_not_followed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.json");
        let link = root.path().join("config.json");
        std::fs::write(&target, br#"{"home":"/safe"}"#).unwrap();
        symlink(&target, &link).unwrap();

        assert!(matches!(
            read_secure_json(&link),
            Err(NativeRuntimeDoctorError::SdkSourceUnavailable)
        ));
    }
}
