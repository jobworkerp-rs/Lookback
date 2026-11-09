//! Private, durable journal for one-shot native runtime observations.

use std::{
    fs::{self, File},
    path::{Component, Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

use super::{is_canonical_uuid, is_sha256};

const JOURNAL_SCHEMA_VERSION: i64 = 2;
const MAX_JOURNAL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DATABASE_BYTES: u64 = 8 * 1024 * 1024;
// Before a write/checkpoint, budget for a full database-sized WAL transaction
// and a full database-sized main-file expansion, plus SQLite sidecar and page
// framing overhead. This is deliberately independent of current page count:
// recovery can checkpoint legacy WAL frames before application validation.
const MAX_WRITE_RESERVE_BYTES: u64 = 17 * 1024 * 1024;
const MAX_OPERATIONS: i64 = 10_000;
const BUSY_TIMEOUT_MS: u64 = 10_000;
const MAX_OWNER_BYTES: usize = 256;

pub struct NativeRuntimeDoctorStore {
    connection: Mutex<Connection>,
    database_path: PathBuf,
}

impl std::fmt::Debug for NativeRuntimeDoctorStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeRuntimeDoctorStore")
            .field("database_path", &"<private native-doctor journal>")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum NativeRuntimeDoctorStoreError {
    #[error("native runtime doctor journal is unavailable or unsafe")]
    UnsafeStore,
    #[error("native runtime doctor journal is corrupt or has an unsupported schema")]
    CorruptStore,
    #[error("native runtime doctor journal request is invalid")]
    InvalidOperation,
    #[error("native runtime doctor operation already exists or conflicts")]
    Conflict,
    #[error("native runtime doctor journal capacity is exhausted")]
    CapacityExceeded,
    #[error("native runtime doctor journal database operation failed")]
    Database,
    #[error("native runtime doctor journal is unsupported on this platform")]
    UnsupportedPlatform,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeProbeIntent {
    operation_id: String,
    owner_window_label: String,
    preview_id: String,
    preview_digest: String,
    context_digest: String,
    runtime_image_sha256: String,
    package_manifest_sha256: String,
    sdk_config_sha256: Option<String>,
    worker_id: Option<i64>,
    runner_id: Option<i64>,
    worker_settings_digest: Option<String>,
    matched_runner_schema_digest: Option<String>,
    run_schema_digest: Option<String>,
    prepared_cwd: Option<String>,
}

impl NativeProbeIntent {
    pub fn new(
        operation_id: impl Into<String>,
        owner_window_label: impl Into<String>,
        preview_id: impl Into<String>,
        preview_digest: impl Into<String>,
        context_digest: impl Into<String>,
        runtime_image_sha256: impl Into<String>,
        package_manifest_sha256: impl Into<String>,
    ) -> Result<Self, NativeRuntimeDoctorStoreError> {
        let intent = Self {
            operation_id: operation_id.into(),
            owner_window_label: owner_window_label.into(),
            preview_id: preview_id.into(),
            preview_digest: preview_digest.into(),
            context_digest: context_digest.into(),
            runtime_image_sha256: runtime_image_sha256.into(),
            package_manifest_sha256: package_manifest_sha256.into(),
            sdk_config_sha256: None,
            worker_id: None,
            runner_id: None,
            worker_settings_digest: None,
            matched_runner_schema_digest: None,
            run_schema_digest: None,
            prepared_cwd: None,
        };
        intent.validate()?;
        Ok(intent)
    }

    pub(crate) fn with_probe_binding(
        mut self,
        sdk_config_sha256: String,
        worker_id: i64,
        runner_id: i64,
        worker_settings_digest: String,
        matched_runner_schema_digest: String,
        prepared_cwd: String,
    ) -> Result<Self, NativeRuntimeDoctorStoreError> {
        self.sdk_config_sha256 = Some(sdk_config_sha256);
        self.worker_id = Some(worker_id);
        self.runner_id = Some(runner_id);
        self.worker_settings_digest = Some(worker_settings_digest);
        self.matched_runner_schema_digest = Some(matched_runner_schema_digest);
        self.prepared_cwd = Some(prepared_cwd);
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<(), NativeRuntimeDoctorStoreError> {
        if !is_canonical_uuid(&self.operation_id)
            || !valid_owner(&self.owner_window_label)
            || !is_canonical_uuid(&self.preview_id)
            || !is_sha256(&self.preview_digest)
            || !is_sha256(&self.context_digest)
            || !is_sha256(&self.runtime_image_sha256)
            || !is_sha256(&self.package_manifest_sha256)
            || self
                .sdk_config_sha256
                .as_deref()
                .is_some_and(|digest| !is_sha256(digest))
            || self.worker_id.is_some_and(|value| value <= 0)
            || self.runner_id.is_some_and(|value| value <= 0)
            || self
                .worker_settings_digest
                .as_deref()
                .is_some_and(|digest| !is_sha256(digest))
            || self
                .matched_runner_schema_digest
                .as_deref()
                .is_some_and(|digest| !is_sha256(digest))
            || self
                .run_schema_digest
                .as_deref()
                .is_some_and(|digest| !is_sha256(digest))
            || self.prepared_cwd.as_ref().is_some_and(|cwd| {
                cwd.is_empty() || cwd.len() > 4096 || cwd.chars().any(char::is_control)
            })
        {
            return Err(NativeRuntimeDoctorStoreError::InvalidOperation);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeProbeOperationState {
    Intent,
    Accepted { job_id: i64 },
    Unknown { job_id: Option<i64> },
    Observed { job_id: i64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeProbeCancelClaim {
    Dispatch { job_id: i64 },
    PendingAcceptance,
    AlreadyClaimed,
    NotFound,
}

impl NativeProbeCancelClaim {
    pub fn dispatch_job_id(self) -> Option<i64> {
        match self {
            Self::Dispatch { job_id } => Some(job_id),
            Self::PendingAcceptance | Self::AlreadyClaimed | Self::NotFound => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeProbeEvidence {
    pub stdout_sha256: String,
    /// Legacy column name; true means the executable-specific version format parsed.
    pub stdout_matches_pinned_version: bool,
    pub stderr_empty: bool,
    pub stream_error: bool,
    pub cli_version: String,
    pub exit_code: i32,
    pub end_received: bool,
    pub stream_eof: bool,
}

impl NativeProbeEvidence {
    fn validate(&self) -> Result<(), NativeRuntimeDoctorStoreError> {
        if !is_sha256(&self.stdout_sha256)
            || !self.stdout_matches_pinned_version
            || self.stream_error
            || !super::launch::verified_cli_version(&self.cli_version)
            || self.exit_code != 0
            || !self.end_received
            || !self.stream_eof
        {
            return Err(NativeRuntimeDoctorStoreError::InvalidOperation);
        }
        Ok(())
    }
}

/// Bounded partial terminal facts retained only for an Unknown outcome. No
/// raw subprocess bytes or stderr contents are persisted.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NativeProbeUnknownEvidence {
    pub stdout_sha256: Option<String>,
    /// Legacy column name; true means the executable-specific version format parsed.
    pub stdout_matches_pinned_version: Option<bool>,
    pub stderr_empty: Option<bool>,
    pub stream_error: Option<bool>,
    pub cli_version: Option<String>,
    pub exit_code: Option<i32>,
    pub end_received: Option<bool>,
    pub stream_eof: Option<bool>,
}

impl NativeProbeUnknownEvidence {
    fn validate(&self) -> Result<(), NativeRuntimeDoctorStoreError> {
        if self
            .stdout_sha256
            .as_deref()
            .is_some_and(|digest| !is_sha256(digest))
            || self
                .cli_version
                .as_deref()
                .is_some_and(|version| !super::launch::verified_cli_version(version))
            || (self.stdout_matches_pinned_version == Some(true)
                && (self.stdout_sha256.is_none()
                    || !self
                        .cli_version
                        .as_deref()
                        .is_some_and(super::launch::verified_cli_version)))
            || (self.cli_version.is_some() && self.stdout_matches_pinned_version != Some(true))
        {
            return Err(NativeRuntimeDoctorStoreError::InvalidOperation);
        }
        Ok(())
    }
}

impl NativeRuntimeDoctorStore {
    /// Open one caller-selected database under an already-private native data directory.
    pub fn open(database_path: &Path) -> Result<Self, NativeRuntimeDoctorStoreError> {
        validate_journal_path(database_path)?;
        #[cfg(not(unix))]
        return Err(NativeRuntimeDoctorStoreError::UnsupportedPlatform);

        #[cfg(unix)]
        {
            let existed = match fs::symlink_metadata(database_path) {
                Ok(metadata) => {
                    validate_private_file(&metadata)?;
                    if metadata.len() > MAX_JOURNAL_BYTES {
                        return Err(NativeRuntimeDoctorStoreError::CapacityExceeded);
                    }
                    true
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(_) => return Err(NativeRuntimeDoctorStoreError::UnsafeStore),
            };
            validate_existing_sidecar(&sidecar_path(database_path, "-wal"))?;
            validate_existing_sidecar(&sidecar_path(database_path, "-shm"))?;
            validate_journal_capacity(database_path)?;
            ensure_journal_write_capacity(database_path)?;

            let connection = Connection::open_with_flags(
                database_path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_FULL_MUTEX
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )
            .map_err(map_sqlite_error)?;
            connection
                .busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))
                .map_err(map_sqlite_error)?;
            if !existed {
                set_private_file(database_path)?;
                sync_parent(database_path)?;
            }
            connection
                .execute_batch(
                    "PRAGMA foreign_keys = ON;
                     PRAGMA journal_mode = WAL;
                     PRAGMA synchronous = FULL;
                     PRAGMA trusted_schema = OFF;
                     PRAGMA temp_store = MEMORY;",
                )
                .map_err(map_sqlite_error)?;
            set_private_file(database_path)?;
            ensure_private_sqlite_sidecars(database_path)?;
            validate_journal_capacity(database_path)?;
            ensure_journal_write_capacity(database_path)?;
            connection
                .pragma_update(None, "journal_size_limit", 4 * 1024 * 1024_i64)
                .map_err(map_sqlite_error)?;
            let page_size: i64 = connection
                .pragma_query_value(None, "page_size", |row| row.get(0))
                .map_err(map_sqlite_error)?;
            if page_size <= 0 {
                return Err(NativeRuntimeDoctorStoreError::CorruptStore);
            }
            connection
                .pragma_update(
                    None,
                    "max_page_count",
                    i64::try_from(MAX_DATABASE_BYTES / page_size as u64)
                        .map_err(|_| NativeRuntimeDoctorStoreError::UnsafeStore)?,
                )
                .map_err(map_sqlite_error)?;
            let page_count: i64 = connection
                .pragma_query_value(None, "page_count", |row| row.get(0))
                .map_err(map_sqlite_error)?;
            let max_page_count: i64 = connection
                .pragma_query_value(None, "max_page_count", |row| row.get(0))
                .map_err(map_sqlite_error)?;
            if page_count < 0
                || page_count > max_page_count
                || u64::try_from(page_count)
                    .ok()
                    .and_then(|pages| pages.checked_mul(page_size as u64))
                    .is_none_or(|database_bytes| database_bytes > MAX_DATABASE_BYTES)
            {
                return Err(NativeRuntimeDoctorStoreError::CapacityExceeded);
            }
            initialize_or_validate_schema(&connection)?;
            set_private_file(database_path)?;
            ensure_private_sqlite_sidecars(database_path)?;
            validate_journal_capacity(database_path)?;

            let store = Self {
                connection: Mutex::new(connection),
                database_path: database_path.to_path_buf(),
            };
            {
                let connection = store.lock_connection()?;
                validate_all_rows(&connection)?;
            }
            store.mark_interrupted_unknown()?;
            Ok(store)
        }
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Durably reserve a fresh operation before any probe enqueue attempt.
    pub fn begin_probe(
        &self,
        intent: NativeProbeIntent,
    ) -> Result<(), NativeRuntimeDoctorStoreError> {
        intent.validate()?;
        let mut connection = self.lock_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        self.ensure_write_capacity()?;
        validate_all_rows(&transaction)?;
        let unresolved_for_preview: bool = transaction
            .query_row(
                "SELECT EXISTS (
                    SELECT 1 FROM native_runtime_probe
                     WHERE owner_window_label = ?1 AND preview_id = ?2
                       AND state IN ('intent', 'accepted', 'unknown')
                 )",
                params![intent.owner_window_label, intent.preview_id],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        if unresolved_for_preview {
            return Err(NativeRuntimeDoctorStoreError::Conflict);
        }
        let count: i64 = transaction
            .query_row("SELECT COUNT(*) FROM native_runtime_probe", [], |row| {
                row.get(0)
            })
            .map_err(map_sqlite_error)?;
        if count >= MAX_OPERATIONS {
            return Err(NativeRuntimeDoctorStoreError::CapacityExceeded);
        }
        transaction
            .execute(
                "INSERT INTO native_runtime_probe (
                    operation_id, owner_window_label, preview_id, preview_digest,
                    context_digest, runtime_image_sha256, package_manifest_sha256,
                    sdk_config_sha256, worker_id, runner_id, worker_settings_digest,
                    matched_runner_schema_digest, run_schema_digest, prepared_cwd,
                    state, created_at_ms
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                    'intent', ?15
                 )",
                params![
                    intent.operation_id,
                    intent.owner_window_label,
                    intent.preview_id,
                    intent.preview_digest,
                    intent.context_digest,
                    intent.runtime_image_sha256,
                    intent.package_manifest_sha256,
                    intent.sdk_config_sha256,
                    intent.worker_id,
                    intent.runner_id,
                    intent.worker_settings_digest,
                    intent.matched_runner_schema_digest,
                    intent.run_schema_digest,
                    intent.prepared_cwd,
                    now_ms(),
                ],
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(code, _)
                    if code.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    NativeRuntimeDoctorStoreError::Conflict
                }
                other => map_sqlite_error(other),
            })?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.validate_committed_capacity()
    }

    /// Save the positive server job id before consuming any result-stream bytes.
    /// Exact retries are idempotent, including after an ambiguous SQLite commit.
    /// Returns true only for the transaction that atomically claimed a pending
    /// cancellation dispatch.
    pub fn record_job_accepted(
        &self,
        operation_id: &str,
        job_id: i64,
        run_schema_digest: &str,
    ) -> Result<bool, NativeRuntimeDoctorStoreError> {
        if !is_canonical_uuid(operation_id) || job_id <= 0 || !is_sha256(run_schema_digest) {
            return Err(NativeRuntimeDoctorStoreError::InvalidOperation);
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        self.ensure_write_capacity()?;
        validate_all_rows(&transaction)?;
        let row = read_operation_for_transition(&transaction, operation_id)?
            .ok_or(NativeRuntimeDoctorStoreError::Conflict)?;

        if row.job_id.is_some_and(|existing| existing != job_id)
            || row
                .run_schema_digest
                .as_deref()
                .is_some_and(|existing| existing != run_schema_digest)
            || !matches!(row.state.as_str(), "intent" | "accepted" | "unknown")
        {
            return Err(NativeRuntimeDoctorStoreError::Conflict);
        }
        if row.job_id.is_none() {
            transaction
                .execute(
                    "UPDATE native_runtime_probe
                        SET state = 'accepted', job_id = ?2, run_schema_digest = ?3
                      WHERE operation_id = ?1 AND job_id IS NULL
                        AND state IN ('intent', 'unknown')",
                    params![operation_id, job_id, run_schema_digest],
                )
                .map_err(map_sqlite_error)?;
        } else if row.run_schema_digest.as_deref() != Some(run_schema_digest) {
            return Err(NativeRuntimeDoctorStoreError::Conflict);
        }

        let dispatch_claimed = claim_pending_cancel_dispatch(&transaction, operation_id)?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.validate_committed_capacity()?;
        Ok(dispatch_claimed)
    }

    /// Preserve a known accepted server identity as Unknown. This is used when
    /// stream evidence cannot be trusted or acceptance durability is recovered
    /// only after the observation deadline. It never changes a genuine
    /// Observed row back into an unresolved state.
    pub fn record_unknown_job_id(
        &self,
        operation_id: &str,
        job_id: i64,
        run_schema_digest: &str,
    ) -> Result<bool, NativeRuntimeDoctorStoreError> {
        if !is_canonical_uuid(operation_id) || job_id <= 0 || !is_sha256(run_schema_digest) {
            return Err(NativeRuntimeDoctorStoreError::InvalidOperation);
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        self.ensure_write_capacity()?;
        validate_all_rows(&transaction)?;
        let row = read_operation_for_transition(&transaction, operation_id)?
            .ok_or(NativeRuntimeDoctorStoreError::Conflict)?;
        if !matches!(row.state.as_str(), "intent" | "accepted" | "unknown")
            || row.job_id.is_some_and(|existing| existing != job_id)
            || row
                .run_schema_digest
                .as_deref()
                .is_some_and(|existing| existing != run_schema_digest)
        {
            return Err(NativeRuntimeDoctorStoreError::Conflict);
        }
        if row.job_id.is_none() {
            transaction
                .execute(
                    "UPDATE native_runtime_probe
                        SET state = 'unknown', job_id = ?2, run_schema_digest = ?3
                      WHERE operation_id = ?1 AND job_id IS NULL
                        AND state IN ('intent', 'accepted', 'unknown')",
                    params![operation_id, job_id, run_schema_digest],
                )
                .map_err(map_sqlite_error)?;
        } else if row.state != "unknown" {
            transaction
                .execute(
                    "UPDATE native_runtime_probe SET state = 'unknown'
                      WHERE operation_id = ?1 AND state = 'accepted'",
                    [operation_id],
                )
                .map_err(map_sqlite_error)?;
        }
        let dispatch_claimed = claim_pending_cancel_dispatch(&transaction, operation_id)?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.validate_committed_capacity()?;
        Ok(dispatch_claimed)
    }

    /// Atomically claim the sole JobService/Delete dispatch for this probe.
    pub fn claim_cancel(
        &self,
        operation_id: &str,
    ) -> Result<NativeProbeCancelClaim, NativeRuntimeDoctorStoreError> {
        if !is_canonical_uuid(operation_id) {
            return Err(NativeRuntimeDoctorStoreError::InvalidOperation);
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        self.ensure_write_capacity()?;
        validate_all_rows(&transaction)?;
        let row = transaction
            .query_row(
                "SELECT state, job_id, cancel_requested, cancel_dispatch_claimed
                   FROM native_runtime_probe WHERE operation_id = ?1",
                [operation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, bool>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some((state, job_id, requested, dispatch_claimed)) = row else {
            return Ok(NativeProbeCancelClaim::NotFound);
        };
        if state == "observed" {
            return Ok(NativeProbeCancelClaim::AlreadyClaimed);
        }
        if !matches!(state.as_str(), "intent" | "accepted" | "unknown") {
            return Err(NativeRuntimeDoctorStoreError::CorruptStore);
        }
        let has_job_id = job_id.is_some();
        let dispatch_job_id = if requested {
            match (job_id, dispatch_claimed) {
                (Some(id), false) => {
                    transaction
                        .execute(
                            "UPDATE native_runtime_probe SET cancel_dispatch_claimed = 1
                              WHERE operation_id = ?1 AND cancel_requested = 1
                                AND cancel_dispatch_claimed = 0 AND job_id = ?2",
                            params![operation_id, id],
                        )
                        .map_err(map_sqlite_error)?;
                    Some(id)
                }
                _ => None,
            }
        } else {
            transaction
                .execute(
                    "UPDATE native_runtime_probe SET cancel_requested = 1,
                            cancel_dispatch_claimed = CASE WHEN job_id IS NULL THEN 0 ELSE 1 END
                      WHERE operation_id = ?1",
                    [operation_id],
                )
                .map_err(map_sqlite_error)?;
            job_id
        };
        transaction.commit().map_err(map_sqlite_error)?;
        self.validate_committed_capacity()?;
        Ok(match dispatch_job_id {
            Some(job_id) => NativeProbeCancelClaim::Dispatch { job_id },
            None if requested || !has_job_id => {
                if requested {
                    NativeProbeCancelClaim::AlreadyClaimed
                } else {
                    NativeProbeCancelClaim::PendingAcceptance
                }
            }
            None => NativeProbeCancelClaim::AlreadyClaimed,
        })
    }

    /// Mark ambiguous acceptance or incomplete stream evidence without making
    /// the operation eligible for automatic replay.
    pub fn mark_unknown(&self, operation_id: &str) -> Result<(), NativeRuntimeDoctorStoreError> {
        if !is_canonical_uuid(operation_id) {
            return Err(NativeRuntimeDoctorStoreError::InvalidOperation);
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        self.ensure_write_capacity()?;
        validate_all_rows(&transaction)?;
        transaction
            .execute(
                "UPDATE native_runtime_probe SET state = 'unknown'
                  WHERE operation_id = ?1 AND state IN ('intent', 'accepted')",
                [operation_id],
            )
            .map_err(map_sqlite_error)?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.validate_committed_capacity()?;
        Ok(())
    }

    /// Persist only digests and terminal protocol facts, never subprocess bytes.
    /// A concurrent cancellation claim prevents a successful lease observation.
    pub fn finish_probe(
        &self,
        operation_id: &str,
        evidence: &NativeProbeEvidence,
    ) -> Result<bool, NativeRuntimeDoctorStoreError> {
        if !is_canonical_uuid(operation_id) {
            return Err(NativeRuntimeDoctorStoreError::InvalidOperation);
        }
        evidence.validate()?;
        let mut connection = self.lock_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        self.ensure_write_capacity()?;
        validate_all_rows(&transaction)?;
        let changed = transaction
            .execute(
                "UPDATE native_runtime_probe
                    SET state = CASE WHEN cancel_requested = 0 THEN 'observed' ELSE 'unknown' END,
                        stdout_sha256 = ?2, stdout_matches_pinned_version = ?3,
                        stderr_empty = ?4, stream_error = ?5, cli_version = ?6,
                        exit_code = ?7, end_received = ?8, stream_eof = ?9
                  WHERE operation_id = ?1 AND state = 'accepted' AND job_id IS NOT NULL
                    AND run_schema_digest IS NOT NULL",
                params![
                    operation_id,
                    evidence.stdout_sha256,
                    evidence.stdout_matches_pinned_version,
                    evidence.stderr_empty,
                    evidence.stream_error,
                    evidence.cli_version,
                    evidence.exit_code,
                    evidence.end_received,
                    evidence.stream_eof,
                ],
            )
            .map_err(map_sqlite_error)?;
        if changed != 1 {
            return Err(NativeRuntimeDoctorStoreError::Conflict);
        }
        let observed: bool = transaction
            .query_row(
                "SELECT state = 'observed' FROM native_runtime_probe WHERE operation_id = ?1",
                [operation_id],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.validate_committed_capacity()?;
        Ok(observed)
    }

    /// Preserve bounded parse/transport facts while leaving the operation
    /// explicitly Unknown. This can never transition an operation to Observed.
    pub fn finish_probe_unknown(
        &self,
        operation_id: &str,
        evidence: &NativeProbeUnknownEvidence,
    ) -> Result<(), NativeRuntimeDoctorStoreError> {
        if !is_canonical_uuid(operation_id) {
            return Err(NativeRuntimeDoctorStoreError::InvalidOperation);
        }
        evidence.validate()?;
        let mut connection = self.lock_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        self.ensure_write_capacity()?;
        validate_all_rows(&transaction)?;
        let changed = transaction
            .execute(
                "UPDATE native_runtime_probe
                    SET state = 'unknown', stdout_sha256 = ?2,
                        stdout_matches_pinned_version = ?3, stderr_empty = ?4,
                        stream_error = ?5, cli_version = ?6, exit_code = ?7,
                        end_received = ?8, stream_eof = ?9
                  WHERE operation_id = ?1 AND state = 'accepted'
                    AND job_id IS NOT NULL AND run_schema_digest IS NOT NULL",
                params![
                    operation_id,
                    evidence.stdout_sha256,
                    evidence.stdout_matches_pinned_version,
                    evidence.stderr_empty,
                    evidence.stream_error,
                    evidence.cli_version,
                    evidence.exit_code,
                    evidence.end_received,
                    evidence.stream_eof,
                ],
            )
            .map_err(map_sqlite_error)?;
        if changed != 1 {
            return Err(NativeRuntimeDoctorStoreError::Conflict);
        }
        transaction.commit().map_err(map_sqlite_error)?;
        self.validate_committed_capacity()
    }

    pub fn operation_state(
        &self,
        operation_id: &str,
    ) -> Result<Option<NativeProbeOperationState>, NativeRuntimeDoctorStoreError> {
        if !is_canonical_uuid(operation_id) {
            return Err(NativeRuntimeDoctorStoreError::InvalidOperation);
        }
        let connection = self.lock_connection()?;
        validate_all_rows(&connection)?;
        let row = connection
            .query_row(
                "SELECT state, job_id FROM native_runtime_probe WHERE operation_id = ?1",
                [operation_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        row.map(|(state, job_id)| decode_state(&state, job_id))
            .transpose()
    }

    fn mark_interrupted_unknown(&self) -> Result<(), NativeRuntimeDoctorStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        self.ensure_write_capacity()?;
        validate_all_rows(&transaction)?;
        transaction
            .execute(
                "UPDATE native_runtime_probe
                    SET state = 'unknown'
                  WHERE state IN ('intent', 'accepted')",
                [],
            )
            .map_err(map_sqlite_error)?;
        transaction.commit().map_err(map_sqlite_error)?;
        self.validate_committed_capacity()?;
        Ok(())
    }

    fn ensure_write_capacity(&self) -> Result<(), NativeRuntimeDoctorStoreError> {
        ensure_journal_write_capacity(&self.database_path)
    }

    fn validate_committed_capacity(&self) -> Result<(), NativeRuntimeDoctorStoreError> {
        validate_journal_capacity(&self.database_path)
    }

    fn lock_connection(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Connection>, NativeRuntimeDoctorStoreError> {
        self.connection
            .lock()
            .map_err(|_| NativeRuntimeDoctorStoreError::CorruptStore)
    }
}

fn decode_state(
    state: &str,
    job_id: Option<i64>,
) -> Result<NativeProbeOperationState, NativeRuntimeDoctorStoreError> {
    match (state, job_id) {
        ("intent", None) => Ok(NativeProbeOperationState::Intent),
        ("accepted", Some(job_id)) if job_id > 0 => {
            Ok(NativeProbeOperationState::Accepted { job_id })
        }
        ("unknown", None) => Ok(NativeProbeOperationState::Unknown { job_id: None }),
        ("unknown", Some(job_id)) if job_id > 0 => Ok(NativeProbeOperationState::Unknown {
            job_id: Some(job_id),
        }),
        ("observed", Some(job_id)) if job_id > 0 => {
            Ok(NativeProbeOperationState::Observed { job_id })
        }
        _ => Err(NativeRuntimeDoctorStoreError::CorruptStore),
    }
}

struct ProbeTransitionRow {
    state: String,
    job_id: Option<i64>,
    run_schema_digest: Option<String>,
    cancel_requested: bool,
    cancel_dispatch_claimed: bool,
}

fn read_operation_for_transition(
    connection: &Connection,
    operation_id: &str,
) -> Result<Option<ProbeTransitionRow>, NativeRuntimeDoctorStoreError> {
    connection
        .query_row(
            "SELECT state, job_id, run_schema_digest, cancel_requested,
                    cancel_dispatch_claimed
               FROM native_runtime_probe WHERE operation_id = ?1",
            [operation_id],
            |row| {
                Ok(ProbeTransitionRow {
                    state: row.get(0)?,
                    job_id: row.get(1)?,
                    run_schema_digest: row.get(2)?,
                    cancel_requested: row.get(3)?,
                    cancel_dispatch_claimed: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|_| NativeRuntimeDoctorStoreError::CorruptStore)
}

fn claim_pending_cancel_dispatch(
    connection: &Connection,
    operation_id: &str,
) -> Result<bool, NativeRuntimeDoctorStoreError> {
    let row = read_operation_for_transition(connection, operation_id)?
        .ok_or(NativeRuntimeDoctorStoreError::Conflict)?;
    if !row.cancel_requested || row.cancel_dispatch_claimed || row.job_id.is_none() {
        return Ok(false);
    }
    let changed = connection
        .execute(
            "UPDATE native_runtime_probe SET cancel_dispatch_claimed = 1
              WHERE operation_id = ?1 AND cancel_requested = 1
                AND cancel_dispatch_claimed = 0 AND job_id IS NOT NULL",
            [operation_id],
        )
        .map_err(map_sqlite_error)?;
    if changed != 1 {
        return Err(NativeRuntimeDoctorStoreError::Conflict);
    }
    Ok(true)
}

struct JournalRow {
    operation_id: String,
    owner_window_label: String,
    preview_id: String,
    preview_digest: String,
    context_digest: String,
    runtime_image_sha256: String,
    package_manifest_sha256: String,
    sdk_config_sha256: Option<String>,
    worker_id: Option<i64>,
    runner_id: Option<i64>,
    worker_settings_digest: Option<String>,
    matched_runner_schema_digest: Option<String>,
    run_schema_digest: Option<String>,
    prepared_cwd: Option<String>,
    state: String,
    job_id: Option<i64>,
    cancel_requested: i64,
    cancel_dispatch_claimed: i64,
    stdout_sha256: Option<String>,
    stdout_matches_pinned_version: Option<i64>,
    stderr_empty: Option<i64>,
    stream_error: Option<i64>,
    cli_version: Option<String>,
    exit_code: Option<i64>,
    end_received: Option<i64>,
    stream_eof: Option<i64>,
    created_at_ms: i64,
}

fn validate_all_rows(connection: &Connection) -> Result<(), NativeRuntimeDoctorStoreError> {
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM native_runtime_probe", [], |row| {
            row.get(0)
        })
        .map_err(map_sqlite_error)?;
    if !(0..=MAX_OPERATIONS).contains(&count) {
        return Err(NativeRuntimeDoctorStoreError::CapacityExceeded);
    }
    let mut statement = connection
        .prepare(
            "SELECT operation_id, owner_window_label, preview_id, preview_digest,
                    context_digest, runtime_image_sha256, package_manifest_sha256,
                    sdk_config_sha256, worker_id, runner_id, worker_settings_digest,
                    matched_runner_schema_digest, run_schema_digest, prepared_cwd,
                    state, job_id, cancel_requested, cancel_dispatch_claimed,
                    stdout_sha256, stdout_matches_pinned_version, stderr_empty,
                    stream_error, cli_version, exit_code, end_received, stream_eof,
                    created_at_ms
               FROM native_runtime_probe ORDER BY operation_id",
        )
        .map_err(map_sqlite_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok(JournalRow {
                operation_id: row.get(0)?,
                owner_window_label: row.get(1)?,
                preview_id: row.get(2)?,
                preview_digest: row.get(3)?,
                context_digest: row.get(4)?,
                runtime_image_sha256: row.get(5)?,
                package_manifest_sha256: row.get(6)?,
                sdk_config_sha256: row.get(7)?,
                worker_id: row.get(8)?,
                runner_id: row.get(9)?,
                worker_settings_digest: row.get(10)?,
                matched_runner_schema_digest: row.get(11)?,
                run_schema_digest: row.get(12)?,
                prepared_cwd: row.get(13)?,
                state: row.get(14)?,
                job_id: row.get(15)?,
                cancel_requested: row.get(16)?,
                cancel_dispatch_claimed: row.get(17)?,
                stdout_sha256: row.get(18)?,
                stdout_matches_pinned_version: row.get(19)?,
                stderr_empty: row.get(20)?,
                stream_error: row.get(21)?,
                cli_version: row.get(22)?,
                exit_code: row.get(23)?,
                end_received: row.get(24)?,
                stream_eof: row.get(25)?,
                created_at_ms: row.get(26)?,
            })
        })
        .map_err(|_| NativeRuntimeDoctorStoreError::CorruptStore)?;
    for row in rows {
        validate_journal_row(row.map_err(|_| NativeRuntimeDoctorStoreError::CorruptStore)?)?;
    }
    Ok(())
}

fn validate_journal_row(row: JournalRow) -> Result<(), NativeRuntimeDoctorStoreError> {
    let invalid = || NativeRuntimeDoctorStoreError::CorruptStore;
    if !is_canonical_uuid(&row.operation_id)
        || !valid_owner(&row.owner_window_label)
        || !is_canonical_uuid(&row.preview_id)
        || !is_sha256(&row.preview_digest)
        || !is_sha256(&row.context_digest)
        || !is_sha256(&row.runtime_image_sha256)
        || !is_sha256(&row.package_manifest_sha256)
        || row
            .sdk_config_sha256
            .as_deref()
            .is_some_and(|digest| !is_sha256(digest))
        || row.worker_id.is_some_and(|id| id <= 0)
        || row.runner_id.is_some_and(|id| id <= 0)
        || row
            .worker_settings_digest
            .as_deref()
            .is_some_and(|digest| !is_sha256(digest))
        || row
            .matched_runner_schema_digest
            .as_deref()
            .is_some_and(|digest| !is_sha256(digest))
        || row
            .run_schema_digest
            .as_deref()
            .is_some_and(|digest| !is_sha256(digest))
        || row.prepared_cwd.as_ref().is_some_and(|cwd| {
            cwd.is_empty() || cwd.len() > 4096 || cwd.chars().any(char::is_control)
        })
        || row.job_id.is_some_and(|id| id <= 0)
        || !matches!(row.cancel_requested, 0 | 1)
        || !matches!(row.cancel_dispatch_claimed, 0 | 1)
        || row.created_at_ms <= 0
    {
        return Err(invalid());
    }
    let binding_count = [
        row.sdk_config_sha256.is_some(),
        row.worker_id.is_some(),
        row.runner_id.is_some(),
        row.worker_settings_digest.is_some(),
        row.matched_runner_schema_digest.is_some(),
        row.prepared_cwd.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    if binding_count != 0 && binding_count != 6 {
        return Err(invalid());
    }

    let evidence = [
        row.stdout_sha256.is_some(),
        row.stdout_matches_pinned_version.is_some(),
        row.stderr_empty.is_some(),
        row.stream_error.is_some(),
        row.cli_version.is_some(),
        row.exit_code.is_some(),
        row.end_received.is_some(),
        row.stream_eof.is_some(),
    ];
    let evidence_count = evidence.into_iter().filter(|present| *present).count();
    let no_evidence = evidence_count == 0;
    let partial_facts_valid = row.stdout_sha256.as_deref().is_none_or(is_sha256)
        && matches!(row.stdout_matches_pinned_version, None | Some(0) | Some(1))
        && matches!(row.stderr_empty, None | Some(0) | Some(1))
        && matches!(row.stream_error, None | Some(0) | Some(1))
        && row
            .cli_version
            .as_deref()
            .is_none_or(super::launch::verified_cli_version)
        && row.exit_code.is_none_or(|code| i32::try_from(code).is_ok())
        && matches!(row.end_received, None | Some(0) | Some(1))
        && matches!(row.stream_eof, None | Some(0) | Some(1))
        && (row.stdout_matches_pinned_version != Some(1)
            || (row.stdout_sha256.is_some()
                && row
                    .cli_version
                    .as_deref()
                    .is_some_and(super::launch::verified_cli_version)))
        && (row.cli_version.is_none() || row.stdout_matches_pinned_version == Some(1));
    let complete_evidence = evidence_count == evidence.len()
        && partial_facts_valid
        && row.stdout_matches_pinned_version == Some(1)
        && row.stream_error == Some(0)
        && row
            .cli_version
            .as_deref()
            .is_some_and(super::launch::verified_cli_version)
        && row.exit_code == Some(0)
        && row.end_received == Some(1)
        && row.stream_eof == Some(1);
    if !no_evidence && !partial_facts_valid {
        return Err(invalid());
    }

    let has_job = row.job_id.is_some();
    let valid_job_binding = has_job && row.run_schema_digest.as_deref().is_some_and(is_sha256);
    let requested = row.cancel_requested == 1;
    let dispatch_claimed = row.cancel_dispatch_claimed == 1;
    if dispatch_claimed && (!has_job || !requested) || (requested && has_job && !dispatch_claimed) {
        return Err(invalid());
    }
    match row.state.as_str() {
        "intent"
            if !has_job && row.run_schema_digest.is_none() && no_evidence && !dispatch_claimed =>
        {
            Ok(())
        }
        "accepted" if valid_job_binding && no_evidence => Ok(()),
        "unknown"
            if ((!has_job
                && row.run_schema_digest.is_none()
                && no_evidence
                && !dispatch_claimed)
                || (valid_job_binding && (no_evidence || partial_facts_valid))) =>
        {
            Ok(())
        }
        "observed" if valid_job_binding && complete_evidence && !requested && !dispatch_claimed => {
            Ok(())
        }
        _ => Err(invalid()),
    }
}

fn initialize_or_validate_schema(
    connection: &Connection,
) -> Result<(), NativeRuntimeDoctorStoreError> {
    let integrity: String = connection
        .pragma_query_value(None, "integrity_check", |row| row.get(0))
        .map_err(map_sqlite_error)?;
    if integrity != "ok" {
        return Err(NativeRuntimeDoctorStoreError::CorruptStore);
    }
    let table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .map_err(map_sqlite_error)?;
    let user_version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(map_sqlite_error)?;
    if table_count == 0 && user_version == 0 {
        connection
            .execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE native_runtime_probe (
                    operation_id TEXT PRIMARY KEY,
                    owner_window_label TEXT NOT NULL,
                    preview_id TEXT NOT NULL,
                    preview_digest TEXT NOT NULL,
                    context_digest TEXT NOT NULL,
                    runtime_image_sha256 TEXT NOT NULL,
                    package_manifest_sha256 TEXT NOT NULL,
                    sdk_config_sha256 TEXT,
                    worker_id INTEGER,
                    runner_id INTEGER,
                    worker_settings_digest TEXT,
                    matched_runner_schema_digest TEXT,
                    run_schema_digest TEXT,
                    prepared_cwd TEXT,
                    state TEXT NOT NULL CHECK (state IN ('intent', 'accepted', 'unknown', 'observed')),
                    job_id INTEGER,
                    cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK (cancel_requested IN (0, 1)),
                    stdout_sha256 TEXT,
                    cli_version TEXT,
                    exit_code INTEGER,
                    end_received INTEGER CHECK (end_received IN (0, 1)),
                    stream_eof INTEGER CHECK (stream_eof IN (0, 1)),
                    created_at_ms INTEGER NOT NULL,
                    cancel_dispatch_claimed INTEGER NOT NULL DEFAULT 0
                        CHECK (cancel_dispatch_claimed IN (0, 1)),
                    stdout_matches_pinned_version INTEGER
                        CHECK (stdout_matches_pinned_version IN (0, 1)),
                    stderr_empty INTEGER CHECK (stderr_empty IN (0, 1)),
                    stream_error INTEGER CHECK (stream_error IN (0, 1)),
                    CHECK (job_id IS NULL OR job_id > 0),
                    CHECK (worker_id IS NULL OR worker_id > 0),
                    CHECK (runner_id IS NULL OR runner_id > 0)
                 ) STRICT;
                  CREATE INDEX native_runtime_probe_preview
                     ON native_runtime_probe(owner_window_label, preview_id, created_at_ms);
                  PRAGMA user_version = 2;
                  COMMIT;",
            )
            .map_err(map_sqlite_error)?;
    } else if user_version == 1 && table_count == 1 {
        let expected_v1_columns = expected_journal_columns(false);
        if read_journal_columns(connection)? != expected_v1_columns {
            return Err(NativeRuntimeDoctorStoreError::CorruptStore);
        }
        connection
            .execute_batch(
                "BEGIN IMMEDIATE;
                 ALTER TABLE native_runtime_probe
                    ADD COLUMN cancel_dispatch_claimed INTEGER NOT NULL DEFAULT 0
                    CHECK (cancel_dispatch_claimed IN (0, 1));
                 ALTER TABLE native_runtime_probe
                    ADD COLUMN stdout_matches_pinned_version INTEGER
                    CHECK (stdout_matches_pinned_version IN (0, 1));
                 ALTER TABLE native_runtime_probe
                    ADD COLUMN stderr_empty INTEGER CHECK (stderr_empty IN (0, 1));
                 ALTER TABLE native_runtime_probe
                    ADD COLUMN stream_error INTEGER CHECK (stream_error IN (0, 1));
                 UPDATE native_runtime_probe
                    SET cancel_dispatch_claimed = 1
                  WHERE cancel_requested = 1 AND job_id IS NOT NULL;
                 UPDATE native_runtime_probe
                    SET state = CASE WHEN state = 'observed' THEN 'unknown' ELSE state END,
                        stdout_sha256 = NULL, cli_version = NULL, exit_code = NULL,
                        end_received = NULL, stream_eof = NULL;
                 PRAGMA user_version = 2;
                 COMMIT;",
            )
            .map_err(map_sqlite_error)?;
    } else if user_version != JOURNAL_SCHEMA_VERSION || table_count != 1 {
        return Err(NativeRuntimeDoctorStoreError::CorruptStore);
    }

    if read_journal_columns(connection)? != expected_journal_columns(true) {
        return Err(NativeRuntimeDoctorStoreError::CorruptStore);
    }
    Ok(())
}

fn expected_journal_columns(include_dispatch_claim: bool) -> Vec<String> {
    let mut columns = vec![
        "operation_id",
        "owner_window_label",
        "preview_id",
        "preview_digest",
        "context_digest",
        "runtime_image_sha256",
        "package_manifest_sha256",
        "sdk_config_sha256",
        "worker_id",
        "runner_id",
        "worker_settings_digest",
        "matched_runner_schema_digest",
        "run_schema_digest",
        "prepared_cwd",
        "state",
        "job_id",
        "cancel_requested",
        "stdout_sha256",
        "cli_version",
        "exit_code",
        "end_received",
        "stream_eof",
        "created_at_ms",
    ];
    if include_dispatch_claim {
        columns.extend([
            "cancel_dispatch_claimed",
            "stdout_matches_pinned_version",
            "stderr_empty",
            "stream_error",
        ]);
    }
    columns.into_iter().map(str::to_owned).collect()
}

fn read_journal_columns(
    connection: &Connection,
) -> Result<Vec<String>, NativeRuntimeDoctorStoreError> {
    let mut statement = connection
        .prepare("PRAGMA table_info(native_runtime_probe)")
        .map_err(map_sqlite_error)?;
    statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(map_sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| NativeRuntimeDoctorStoreError::CorruptStore)
}

fn validate_journal_path(path: &Path) -> Result<(), NativeRuntimeDoctorStoreError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(NativeRuntimeDoctorStoreError::UnsafeStore);
    }
    let parent = path
        .parent()
        .ok_or(NativeRuntimeDoctorStoreError::UnsafeStore)?;
    if fs::canonicalize(parent).ok().as_deref() != Some(parent) {
        return Err(NativeRuntimeDoctorStoreError::UnsafeStore);
    }
    validate_private_directory_chain(parent)?;
    if path.file_name().is_none() || path.file_name().is_some_and(|name| name.is_empty()) {
        return Err(NativeRuntimeDoctorStoreError::UnsafeStore);
    }
    Ok(())
}

fn validate_private_directory_chain(path: &Path) -> Result<(), NativeRuntimeDoctorStoreError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(part) => current.push(part),
            _ => return Err(NativeRuntimeDoctorStoreError::UnsafeStore),
        }
        let metadata = fs::symlink_metadata(&current)
            .map_err(|_| NativeRuntimeDoctorStoreError::UnsafeStore)?;
        validate_directory_metadata(&metadata, current == path)?;
    }
    Ok(())
}

fn validate_directory_metadata(
    metadata: &fs::Metadata,
    private_leaf: bool,
) -> Result<(), NativeRuntimeDoctorStoreError> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(NativeRuntimeDoctorStoreError::UnsafeStore);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let euid = unsafe { libc::geteuid() };
        let root_sticky = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
        if (metadata.uid() != euid && metadata.uid() != 0)
            || (metadata.mode() & 0o0022 != 0 && !root_sticky)
            || (private_leaf && (metadata.uid() != euid || metadata.mode() & 0o077 != 0))
        {
            return Err(NativeRuntimeDoctorStoreError::UnsafeStore);
        }
    }
    #[cfg(not(unix))]
    return Err(NativeRuntimeDoctorStoreError::UnsupportedPlatform);
    Ok(())
}

#[cfg(unix)]
fn validate_private_file(metadata: &fs::Metadata) -> Result<(), NativeRuntimeDoctorStoreError> {
    use std::os::unix::fs::MetadataExt;

    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(NativeRuntimeDoctorStoreError::UnsafeStore);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_file(_: &fs::Metadata) -> Result<(), NativeRuntimeDoctorStoreError> {
    Err(NativeRuntimeDoctorStoreError::UnsupportedPlatform)
}

fn validate_existing_sidecar(path: &Path) -> Result<(), NativeRuntimeDoctorStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_private_file(&metadata)?;
            if metadata.len() > MAX_JOURNAL_BYTES {
                return Err(NativeRuntimeDoctorStoreError::CapacityExceeded);
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(NativeRuntimeDoctorStoreError::UnsafeStore),
    }
}

fn validate_journal_capacity(database: &Path) -> Result<(), NativeRuntimeDoctorStoreError> {
    let total = journal_physical_bytes(database)?;
    if total > MAX_JOURNAL_BYTES {
        return Err(NativeRuntimeDoctorStoreError::CapacityExceeded);
    }
    Ok(())
}

fn ensure_journal_write_capacity(database: &Path) -> Result<(), NativeRuntimeDoctorStoreError> {
    ensure_journal_write_capacity_with_limit(database, MAX_JOURNAL_BYTES, MAX_WRITE_RESERVE_BYTES)
}

fn ensure_journal_write_capacity_with_limit(
    database: &Path,
    maximum_bytes: u64,
    write_reserve_bytes: u64,
) -> Result<(), NativeRuntimeDoctorStoreError> {
    let total = journal_physical_bytes(database)?;
    if total
        .checked_add(write_reserve_bytes)
        .is_none_or(|projected| projected > maximum_bytes)
    {
        return Err(NativeRuntimeDoctorStoreError::CapacityExceeded);
    }
    Ok(())
}

fn journal_physical_bytes(database: &Path) -> Result<u64, NativeRuntimeDoctorStoreError> {
    let mut total = 0_u64;
    for path in [
        database.to_path_buf(),
        sidecar_path(database, "-wal"),
        sidecar_path(database, "-shm"),
    ] {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                validate_private_file(&metadata)?;
                total = total
                    .checked_add(metadata.len())
                    .ok_or(NativeRuntimeDoctorStoreError::CapacityExceeded)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(NativeRuntimeDoctorStoreError::UnsafeStore),
        }
    }
    Ok(total)
}

fn sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

fn set_private_file(path: &Path) -> Result<(), NativeRuntimeDoctorStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| NativeRuntimeDoctorStoreError::UnsafeStore)?;
        let metadata =
            fs::symlink_metadata(path).map_err(|_| NativeRuntimeDoctorStoreError::UnsafeStore)?;
        validate_private_file(&metadata)
    }
    #[cfg(not(unix))]
    Err(NativeRuntimeDoctorStoreError::UnsupportedPlatform)
}

fn ensure_private_sqlite_sidecars(database: &Path) -> Result<(), NativeRuntimeDoctorStoreError> {
    for suffix in ["-wal", "-shm"] {
        let path = sidecar_path(database, suffix);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(NativeRuntimeDoctorStoreError::UnsafeStore);
                }
                set_private_file(&path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(NativeRuntimeDoctorStoreError::UnsafeStore),
        }
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<(), NativeRuntimeDoctorStoreError> {
    let parent = path
        .parent()
        .ok_or(NativeRuntimeDoctorStoreError::UnsafeStore)?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| NativeRuntimeDoctorStoreError::UnsafeStore)
}

fn map_sqlite_error(error: rusqlite::Error) -> NativeRuntimeDoctorStoreError {
    match error {
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
            ) =>
        {
            NativeRuntimeDoctorStoreError::CorruptStore
        }
        rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::DiskFull => {
            NativeRuntimeDoctorStoreError::CapacityExceeded
        }
        _ => NativeRuntimeDoctorStoreError::Database,
    }
}

fn valid_owner(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_OWNER_BYTES && !value.chars().any(char::is_control)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn write_capacity_reserves_aggregate_database_wal_and_shm_growth() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("journal.sqlite3");
        let wal = sidecar_path(&database, "-wal");
        let shm = sidecar_path(&database, "-shm");
        for (path, length) in [(&database, 400_u64), (&wal, 400), (&shm, 100)] {
            File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(path)
                .unwrap()
                .set_len(length)
                .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        ensure_journal_write_capacity_with_limit(&database, 1_000, 100).unwrap();
        File::options()
            .write(true)
            .open(&wal)
            .unwrap()
            .set_len(401)
            .unwrap();
        assert_eq!(
            ensure_journal_write_capacity_with_limit(&database, 1_000, 100),
            Err(NativeRuntimeDoctorStoreError::CapacityExceeded)
        );
    }

    #[test]
    fn unknown_probe_accepts_an_unregistered_actual_version_as_parsed_evidence() {
        let evidence = NativeProbeUnknownEvidence {
            stdout_sha256: Some("f".repeat(64)),
            stdout_matches_pinned_version: Some(true),
            stderr_empty: Some(false),
            stream_error: Some(false),
            cli_version: Some("0.200.0-nightly.4+linux".into()),
            exit_code: Some(0),
            end_received: Some(false),
            stream_eof: Some(true),
        };
        assert!(evidence.validate().is_ok());
    }
}
