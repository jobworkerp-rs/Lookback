use std::{
    fs::{self, File},
    path::{Component, Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

use super::{
    JobAcceptance, MAX_NATIVE_CONTEXT_BYTES, MAX_NATIVE_PREVIEW_BYTES, NativeCancelClaim,
    NativeSessionContext, NativeSessionRecord, NativeTerminalObservation, NativeTerminalPreview,
    SessionState, StartClaimOutcome, is_canonical_uuid,
};

const DATABASE_FILE_NAME: &str = "native-terminal.sqlite3";
const DATABASE_SCHEMA_VERSION: i64 = 1;
const MAX_DATABASE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_STORED_ROWS: i64 = 10_000;
const MAX_WINDOW_LABEL_BYTES: usize = 256;
const BUSY_TIMEOUT_MS: u64 = 10_000;

/// A separate SQLite database for native interactive sessions, never AgentRun state.
pub struct NativeTerminalStore {
    connection: Mutex<Connection>,
    database_path: PathBuf,
}

impl std::fmt::Debug for NativeTerminalStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeTerminalStore")
            .field("database_path", &"<private native-session database>")
            .finish_non_exhaustive()
    }
}

impl NativeTerminalStore {
    /// Open the explicitly selected private `native-terminal.sqlite3` database.
    /// The caller creates and owns its private parent; no home or environment paths are inferred.
    pub fn open(database_path: &Path) -> Result<Self, NativeTerminalStoreError> {
        validate_database_path(database_path)?;
        #[cfg(not(unix))]
        return Err(NativeTerminalStoreError::UnsupportedPlatform);

        #[cfg(unix)]
        {
            let existed = match fs::symlink_metadata(database_path) {
                Ok(metadata) => {
                    validate_existing_database(&metadata)?;
                    true
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(_) => return Err(NativeTerminalStoreError::UnsafeStore),
            };
            let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW;
            let connection = Connection::open_with_flags(database_path, flags)?;
            connection.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
            if !existed {
                set_private_file(database_path)?;
                sync_parent(database_path)?;
            }
            connection.execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA trusted_schema = OFF;",
            )?;
            let page_size: i64 =
                connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
            if page_size <= 0 {
                return Err(NativeTerminalStoreError::CorruptStore);
            }
            connection.pragma_update(
                None,
                "max_page_count",
                i64::try_from(MAX_DATABASE_BYTES / page_size as u64)
                    .map_err(|_| NativeTerminalStoreError::UnsafeStore)?,
            )?;
            connection.pragma_update(None, "journal_size_limit", 4 * 1024 * 1024_i64)?;
            initialize_schema(&connection)?;
            set_private_file(database_path)?;
            ensure_private_sqlite_sidecars(database_path)?;

            Ok(Self {
                connection: Mutex::new(connection),
                database_path: database_path.to_path_buf(),
            })
        }
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Persist an immutable preview owned by the native caller's actual window label.
    pub fn persist_preview(
        &self,
        preview: &NativeTerminalPreview,
        owner_window_label: &str,
    ) -> Result<bool, NativeTerminalStoreError> {
        preview
            .validate()
            .map_err(|_| NativeTerminalStoreError::InvalidContext)?;
        validate_owner_window_label(owner_window_label)?;
        let json = serde_json::to_vec(preview)?;
        if json.len() > MAX_NATIVE_PREVIEW_BYTES {
            return Err(NativeTerminalStoreError::CapacityExceeded);
        }

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = transaction
            .query_row(
                "SELECT owner_window_label, preview_digest, preview_json
                   FROM native_terminal_previews WHERE preview_id = ?1",
                [preview.preview_id()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?;
        let inserted = match existing {
            Some((owner, digest, stored_json)) => {
                if owner != owner_window_label {
                    return Err(NativeTerminalStoreError::OwnerMismatch);
                }
                if digest != preview.preview_digest() || stored_json != json {
                    return Err(NativeTerminalStoreError::Conflict);
                }
                false
            }
            None => {
                ensure_capacity(&transaction, "native_terminal_previews")?;
                transaction.execute(
                    "INSERT INTO native_terminal_previews
                        (preview_id, owner_window_label, preview_digest,
                         expected_settings_revision, expected_issue_revision,
                         preview_json, created_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        preview.preview_id(),
                        owner_window_label,
                        preview.preview_digest(),
                        sql_revision(preview.expected_settings_revision())?,
                        sql_revision(preview.expected_issue_revision())?,
                        json,
                        now_ms(),
                    ],
                )?;
                true
            }
        };
        transaction.commit()?;
        Ok(inserted)
    }

    pub fn get_preview(
        &self,
        preview_id: &str,
        owner_window_label: &str,
    ) -> Result<Option<NativeTerminalPreview>, NativeTerminalStoreError> {
        if !is_canonical_uuid(preview_id) {
            return Err(NativeTerminalStoreError::InvalidContext);
        }
        validate_owner_window_label(owner_window_label)?;
        let connection = self.lock_connection()?;
        let stored = connection
            .query_row(
                "SELECT owner_window_label, preview_digest, preview_json
                   FROM native_terminal_previews WHERE preview_id = ?1",
                [preview_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?;
        match stored {
            Some((owner, _, _)) if owner != owner_window_label => {
                Err(NativeTerminalStoreError::OwnerMismatch)
            }
            Some((_, digest, json)) => {
                let preview = deserialize_stored_preview(&json)?;
                preview
                    .validate()
                    .map_err(|_| NativeTerminalStoreError::CorruptStore)?;
                if digest != preview.preview_digest() {
                    return Err(NativeTerminalStoreError::CorruptStore);
                }
                Ok(Some(preview))
            }
            None => Ok(None),
        }
    }

    /// Durably claim one native start before the caller invokes any worker RPC.
    pub fn claim_start(
        &self,
        context: NativeSessionContext,
        owner_window_label: &str,
    ) -> Result<StartClaimOutcome, NativeTerminalStoreError> {
        context
            .validate()
            .map_err(|_| NativeTerminalStoreError::InvalidContext)?;
        validate_owner_window_label(owner_window_label)?;
        if context.owner_window_label() != owner_window_label {
            return Err(NativeTerminalStoreError::OwnerMismatch);
        }
        let context_json = serde_json::to_vec(&context)?;
        if context_json.len() > MAX_NATIVE_CONTEXT_BYTES {
            return Err(NativeTerminalStoreError::CapacityExceeded);
        }
        let context_digest = context
            .digest()
            .map_err(|_| NativeTerminalStoreError::InvalidContext)?;

        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let preview = transaction
            .query_row(
                "SELECT owner_window_label, preview_digest, preview_json
                   FROM native_terminal_previews WHERE preview_id = ?1",
                [context.preview().preview_id()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or(NativeTerminalStoreError::NotFound)?;
        if preview.0 != owner_window_label {
            return Err(NativeTerminalStoreError::OwnerMismatch);
        }
        let stored_preview = deserialize_stored_preview(&preview.2)?;
        stored_preview
            .validate()
            .map_err(|_| NativeTerminalStoreError::CorruptStore)?;
        if preview.1 != context.preview().preview_digest() || stored_preview != *context.preview() {
            return Err(NativeTerminalStoreError::Conflict);
        }

        if let Some(existing) = load_session_from_connection(&transaction, context.session_id())? {
            if existing.context.owner_window_label() != owner_window_label {
                return Err(NativeTerminalStoreError::OwnerMismatch);
            }
            if existing.context_digest != context_digest
                || existing.context != context
                || existing.preview_id != context.preview().preview_id()
            {
                return Err(NativeTerminalStoreError::Conflict);
            }
            transaction.commit()?;
            return Ok(StartClaimOutcome::Existing(existing));
        }
        let preview_already_claimed: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM native_terminal_sessions WHERE preview_id = ?1)",
            [context.preview().preview_id()],
            |row| row.get(0),
        )?;
        if preview_already_claimed {
            return Err(NativeTerminalStoreError::Conflict);
        }
        ensure_capacity(&transaction, "native_terminal_sessions")?;
        transaction.execute(
            "INSERT INTO native_terminal_sessions
                (session_id, attempt_id, preview_id, preview_digest, context_digest,
                 profile_id, expected_settings_revision, expected_issue_revision,
                 native_job_id, state, exit_code, end_received, stream_eof, stream_error,
                 cancel_requested, cancel_dispatched, context_json, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, 'starting', NULL,
                     0, 0, 0, 0, 0, ?9, ?10, ?10)",
            params![
                context.session_id(),
                context.attempt_id(),
                context.preview().preview_id(),
                context.preview().preview_digest(),
                context_digest,
                context.profile().profile_id,
                sql_revision(context.preview().expected_settings_revision())?,
                sql_revision(context.preview().expected_issue_revision())?,
                context_json,
                now_ms(),
            ],
        )?;
        transaction.commit()?;
        Ok(StartClaimOutcome::Claimed(record_from_context(
            context,
            context_digest,
            NativeRecordFacts::starting(),
        )))
    }

    pub fn get_session(
        &self,
        session_id: &str,
    ) -> Result<Option<NativeSessionRecord>, NativeTerminalStoreError> {
        if !is_canonical_uuid(session_id) {
            return Err(NativeTerminalStoreError::InvalidContext);
        }
        let connection = self.lock_connection()?;
        load_session_from_connection(&connection, session_id)
    }

    pub fn get_session_for_window(
        &self,
        session_id: &str,
        owner_window_label: &str,
    ) -> Result<Option<NativeSessionRecord>, NativeTerminalStoreError> {
        validate_owner_window_label(owner_window_label)?;
        let session = self.get_session(session_id)?;
        if session
            .as_ref()
            .is_some_and(|record| record.context.owner_window_label() != owner_window_label)
        {
            return Err(NativeTerminalStoreError::OwnerMismatch);
        }
        Ok(session)
    }

    /// Enumerate one window's sessions only after validating the complete bounded snapshot.
    pub fn list_sessions_for_window(
        &self,
        owner_window_label: &str,
    ) -> Result<Vec<NativeSessionRecord>, NativeTerminalStoreError> {
        validate_owner_window_label(owner_window_label)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let session_ids = {
            let mut statement = transaction.prepare(
                "SELECT session_id FROM native_terminal_sessions ORDER BY session_id LIMIT ?1",
            )?;
            statement
                .query_map([MAX_STORED_ROWS + 1], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        if session_ids.len() > MAX_STORED_ROWS as usize {
            return Err(NativeTerminalStoreError::CorruptStore);
        }

        let mut owned = Vec::new();
        for session_id in session_ids {
            let record = load_session_from_connection(&transaction, &session_id)?
                .ok_or(NativeTerminalStoreError::CorruptStore)?;
            if record.context.owner_window_label() == owner_window_label {
                owned.push(record);
            }
        }
        transaction.commit()?;
        Ok(owned)
    }

    /// Persist the actual positive worker job ID; this never retries a start RPC.
    pub fn record_job_accepted(
        &self,
        session_id: &str,
        native_job_id: i64,
    ) -> Result<JobAcceptance, NativeTerminalStoreError> {
        if !is_canonical_uuid(session_id) || native_job_id <= 0 {
            return Err(NativeTerminalStoreError::InvalidContext);
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = load_mutable_session(&transaction, session_id)?;
        if let Some(stored_job_id) = row.record.native_job_id {
            if stored_job_id != native_job_id {
                return Err(NativeTerminalStoreError::Conflict);
            }
            let new_cancel_dispatch = row.record.cancel_requested && !row.cancel_dispatched;
            if new_cancel_dispatch {
                transaction.execute(
                    "UPDATE native_terminal_sessions
                        SET cancel_dispatched = 1, updated_at_ms = ?2 WHERE session_id = ?1",
                    params![session_id, now_ms()],
                )?;
            }
            transaction.commit()?;
            return Ok(JobAcceptance {
                cancel_dispatch_job_id: new_cancel_dispatch.then_some(native_job_id),
            });
        }
        if matches!(
            row.record.state,
            SessionState::Closed | SessionState::Cancelled
        ) {
            return Err(NativeTerminalStoreError::Conflict);
        }
        let new_cancel_dispatch = row.record.cancel_requested && !row.cancel_dispatched;
        let complete_terminal_evidence = has_complete_terminal_evidence(
            Some(native_job_id),
            row.record.exit_code,
            row.record.end_received,
            row.record.stream_eof,
            row.record.stream_error,
        );
        let next_state = match row.record.state {
            SessionState::Starting => SessionState::Running,
            SessionState::Unknown if complete_terminal_evidence => {
                if row.record.cancel_requested {
                    SessionState::Cancelled
                } else {
                    SessionState::Closed
                }
            }
            state => state,
        };
        transaction.execute(
            "UPDATE native_terminal_sessions
                SET native_job_id = ?2, state = ?3, cancel_dispatched = ?4,
                    updated_at_ms = ?5
              WHERE session_id = ?1",
            params![
                session_id,
                native_job_id,
                state_as_str(next_state),
                i64::from(row.cancel_dispatched || new_cancel_dispatch),
                now_ms(),
            ],
        )?;
        transaction.commit()?;
        Ok(JobAcceptance {
            cancel_dispatch_job_id: new_cancel_dispatch.then_some(native_job_id),
        })
    }

    /// Mark an ambiguous enqueue or stream failure Unknown without resubmitting it.
    pub fn mark_start_unknown(
        &self,
        session_id: &str,
    ) -> Result<NativeSessionRecord, NativeTerminalStoreError> {
        self.set_unknown(session_id)
    }

    pub fn claim_cancel(
        &self,
        session_id: &str,
        owner_window_label: &str,
    ) -> Result<NativeCancelClaim, NativeTerminalStoreError> {
        if !is_canonical_uuid(session_id) {
            return Err(NativeTerminalStoreError::InvalidContext);
        }
        validate_owner_window_label(owner_window_label)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = load_mutable_session(&transaction, session_id)?;
        if row.record.context.owner_window_label() != owner_window_label {
            return Err(NativeTerminalStoreError::OwnerMismatch);
        }
        if matches!(
            row.record.state,
            SessionState::Closed | SessionState::Cancelled
        ) {
            transaction.commit()?;
            return Ok(NativeCancelClaim::Terminal);
        }
        if row.record.cancel_requested {
            if row.cancel_dispatched {
                transaction.commit()?;
                return Ok(NativeCancelClaim::AlreadyRequested);
            }
            if let Some(job_id) = row.record.native_job_id {
                transaction.execute(
                    "UPDATE native_terminal_sessions
                        SET cancel_dispatched = 1, updated_at_ms = ?2 WHERE session_id = ?1",
                    params![session_id, now_ms()],
                )?;
                transaction.commit()?;
                return Ok(NativeCancelClaim::Dispatch { job_id });
            }
            transaction.commit()?;
            return Ok(NativeCancelClaim::PendingAcceptance);
        }

        let job_id = row.record.native_job_id;
        transaction.execute(
            "UPDATE native_terminal_sessions
                SET cancel_requested = 1, cancel_dispatched = ?2, updated_at_ms = ?3
              WHERE session_id = ?1",
            params![session_id, i64::from(job_id.is_some()), now_ms()],
        )?;
        transaction.commit()?;
        Ok(match job_id {
            Some(job_id) => NativeCancelClaim::Dispatch { job_id },
            None => NativeCancelClaim::PendingAcceptance,
        })
    }

    /// Store host-observed terminal facts; incomplete stream evidence remains Unknown.
    pub fn record_terminal_observation(
        &self,
        session_id: &str,
        observation: NativeTerminalObservation,
    ) -> Result<NativeSessionRecord, NativeTerminalStoreError> {
        if !is_canonical_uuid(session_id) {
            return Err(NativeTerminalStoreError::InvalidContext);
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = load_mutable_session(&transaction, session_id)?;
        let exit_code = match (row.record.exit_code, observation.exit_code) {
            (Some(existing), Some(observed)) if existing != observed => {
                return Err(NativeTerminalStoreError::Conflict);
            }
            (Some(existing), _) => Some(existing),
            (None, observed) => observed,
        };
        let end_received = row.record.end_received || observation.end_received;
        let stream_eof = row.record.stream_eof || observation.stream_eof;
        let stream_error = row.record.stream_error || observation.stream_error;
        let complete = has_complete_terminal_evidence(
            row.record.native_job_id,
            exit_code,
            end_received,
            stream_eof,
            stream_error,
        );
        let state = if complete {
            if row.record.cancel_requested {
                SessionState::Cancelled
            } else {
                SessionState::Closed
            }
        } else {
            SessionState::Unknown
        };
        if matches!(
            row.record.state,
            SessionState::Closed | SessionState::Cancelled
        ) {
            let same = row.record.state == state
                && row.record.exit_code == exit_code
                && row.record.end_received == end_received
                && row.record.stream_eof == stream_eof
                && row.record.stream_error == stream_error;
            if same {
                transaction.commit()?;
                return Ok(row.record);
            }
            return Err(NativeTerminalStoreError::Conflict);
        }
        transaction.execute(
            "UPDATE native_terminal_sessions
                SET state = ?2, exit_code = ?3, end_received = ?4, stream_eof = ?5,
                    stream_error = ?6, updated_at_ms = ?7
              WHERE session_id = ?1",
            params![
                session_id,
                state_as_str(state),
                exit_code,
                i64::from(end_received),
                i64::from(stream_eof),
                i64::from(stream_error),
                now_ms(),
            ],
        )?;
        transaction.commit()?;
        load_session_from_connection(&connection, session_id)?
            .ok_or(NativeTerminalStoreError::CorruptStore)
    }

    /// Recover interrupted local lifecycle records as Unknown, never by re-enqueueing.
    pub fn mark_unresolved_unknown(&self) -> Result<usize, NativeTerminalStoreError> {
        let connection = self.lock_connection()?;
        let changed = connection.execute(
            "UPDATE native_terminal_sessions
                SET state = 'unknown', updated_at_ms = ?1
              WHERE state IN ('starting', 'running')",
            [now_ms()],
        )?;
        Ok(changed)
    }

    /// Profiles remain frozen while any session is active or unresolved.
    pub fn unresolved_profile_ids(&self) -> Result<Vec<String>, NativeTerminalStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let session_ids = {
            let mut statement = transaction.prepare(
                "SELECT session_id FROM native_terminal_sessions ORDER BY session_id LIMIT ?1",
            )?;
            statement
                .query_map([MAX_STORED_ROWS + 1], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        if session_ids.len() > MAX_STORED_ROWS as usize {
            return Err(NativeTerminalStoreError::CorruptStore);
        }

        // Validate every row in this read snapshot before using any lifecycle state
        // or denormalized profile ID to decide which profiles remain frozen.
        let mut validated_sessions = Vec::with_capacity(session_ids.len());
        for session_id in session_ids {
            let record = load_session_from_connection(&transaction, &session_id)?
                .ok_or(NativeTerminalStoreError::CorruptStore)?;
            validated_sessions.push((record.state, record.profile_id));
        }
        transaction.commit()?;

        let mut profile_ids = validated_sessions
            .into_iter()
            .filter_map(|(state, profile_id)| {
                matches!(
                    state,
                    SessionState::Starting | SessionState::Running | SessionState::Unknown
                )
                .then_some(profile_id)
            })
            .collect::<Vec<_>>();
        profile_ids.sort_unstable();
        profile_ids.dedup();
        Ok(profile_ids)
    }

    fn set_unknown(
        &self,
        session_id: &str,
    ) -> Result<NativeSessionRecord, NativeTerminalStoreError> {
        if !is_canonical_uuid(session_id) {
            return Err(NativeTerminalStoreError::InvalidContext);
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = load_mutable_session(&transaction, session_id)?;
        if matches!(
            row.record.state,
            SessionState::Closed | SessionState::Cancelled
        ) {
            return Err(NativeTerminalStoreError::Conflict);
        }
        transaction.execute(
            "UPDATE native_terminal_sessions
                SET state = 'unknown', updated_at_ms = ?2 WHERE session_id = ?1",
            params![session_id, now_ms()],
        )?;
        transaction.commit()?;
        load_session_from_connection(&connection, session_id)?
            .ok_or(NativeTerminalStoreError::CorruptStore)
    }

    fn lock_connection(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Connection>, NativeTerminalStoreError> {
        self.connection
            .lock()
            .map_err(|_| NativeTerminalStoreError::Unavailable)
    }
}

#[derive(Debug, Error)]
pub enum NativeTerminalStoreError {
    #[error("native terminal store path is unsafe")]
    UnsafeStore,
    #[error("native terminal store is corrupt")]
    CorruptStore,
    #[error("native terminal store schema is unsupported")]
    UnsupportedSchema,
    #[error("native terminal store is full or a record exceeds its byte bound")]
    CapacityExceeded,
    #[error("native terminal context is invalid")]
    InvalidContext,
    #[error("native terminal session or preview conflicts with an existing immutable record")]
    Conflict,
    #[error("native terminal session or preview is not found")]
    NotFound,
    #[error("native terminal session belongs to a different webview window")]
    OwnerMismatch,
    #[error("native terminal store is unavailable")]
    Unavailable,
    #[error("native terminal store requires Unix private-file semantics")]
    UnsupportedPlatform,
    #[error("native terminal SQLite operation failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error("native terminal record serialization failed")]
    Serialization(#[from] serde_json::Error),
    #[error("native terminal store I/O failed")]
    Io(#[from] std::io::Error),
}

struct MutableSession {
    record: NativeSessionRecord,
    cancel_dispatched: bool,
}

#[derive(Clone, Copy)]
struct NativeRecordFacts {
    state: SessionState,
    native_job_id: Option<i64>,
    exit_code: Option<i32>,
    end_received: bool,
    stream_eof: bool,
    stream_error: bool,
    cancel_requested: bool,
}

impl NativeRecordFacts {
    fn starting() -> Self {
        Self {
            state: SessionState::Starting,
            native_job_id: None,
            exit_code: None,
            end_received: false,
            stream_eof: false,
            stream_error: false,
            cancel_requested: false,
        }
    }
}

fn record_from_context(
    context: NativeSessionContext,
    context_digest: String,
    facts: NativeRecordFacts,
) -> NativeSessionRecord {
    NativeSessionRecord {
        session_id: context.session_id().to_owned(),
        attempt_id: context.attempt_id().to_owned(),
        preview_id: context.preview().preview_id().to_owned(),
        preview_digest: context.preview().preview_digest().to_owned(),
        profile_id: context.profile().profile_id.clone(),
        expected_settings_revision: context.preview().expected_settings_revision(),
        expected_issue_revision: context.preview().expected_issue_revision(),
        native_job_id: facts.native_job_id,
        state: facts.state,
        exit_code: facts.exit_code,
        end_received: facts.end_received,
        stream_eof: facts.stream_eof,
        stream_error: facts.stream_error,
        cancel_requested: facts.cancel_requested,
        context_digest,
        context,
    }
}

fn load_session_from_connection(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<NativeSessionRecord>, NativeTerminalStoreError> {
    let values = connection
        .query_row(
            "SELECT attempt_id, preview_id, preview_digest, context_digest, profile_id,
                    expected_settings_revision, expected_issue_revision, native_job_id,
                    state, exit_code, end_received, stream_eof, stream_error,
                    cancel_requested, cancel_dispatched, context_json
               FROM native_terminal_sessions WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<i32>>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, Vec<u8>>(15)?,
                ))
            },
        )
        .optional()?;
    let Some((
        attempt_id,
        preview_id,
        preview_digest,
        context_digest,
        profile_id,
        settings_revision,
        issue_revision,
        native_job_id,
        state,
        exit_code,
        end_received,
        stream_eof,
        stream_error,
        cancel_requested,
        cancel_dispatched,
        context_json,
    )) = values
    else {
        return Ok(None);
    };
    if context_json.len() > MAX_NATIVE_CONTEXT_BYTES
        || settings_revision < 0
        || issue_revision < 0
        || native_job_id.is_some_and(|job_id| job_id <= 0)
    {
        return Err(NativeTerminalStoreError::CorruptStore);
    }
    let context = deserialize_stored_context(&context_json)?;
    context
        .validate()
        .map_err(|_| NativeTerminalStoreError::CorruptStore)?;
    let actual_digest = context
        .digest()
        .map_err(|_| NativeTerminalStoreError::CorruptStore)?;
    let state = parse_state(&state)?;
    let end_received = db_bool(end_received)?;
    let stream_eof = db_bool(stream_eof)?;
    let stream_error = db_bool(stream_error)?;
    let cancel_requested = db_bool(cancel_requested)?;
    let cancel_dispatched = db_bool(cancel_dispatched)?;
    let has_terminal_observation =
        exit_code.is_some() || end_received || stream_eof || stream_error;
    let has_complete_terminal_evidence = has_complete_terminal_evidence(
        native_job_id,
        exit_code,
        end_received,
        stream_eof,
        stream_error,
    );
    let valid_state_facts = match state {
        SessionState::Starting => native_job_id.is_none() && !has_terminal_observation,
        SessionState::Running => native_job_id.is_some() && !has_terminal_observation,
        SessionState::Closed => {
            has_complete_terminal_evidence && !cancel_requested && !cancel_dispatched
        }
        SessionState::Cancelled => {
            has_complete_terminal_evidence && cancel_requested && cancel_dispatched
        }
        SessionState::Unknown => !has_complete_terminal_evidence,
    };
    if actual_digest != context_digest
        || context.session_id() != session_id
        || context.attempt_id() != attempt_id
        || context.preview().preview_id() != preview_id
        || context.preview().preview_digest() != preview_digest
        || context.profile().profile_id != profile_id
        || context.preview().expected_settings_revision() != settings_revision as u64
        || context.preview().expected_issue_revision() != issue_revision as u64
        || (cancel_dispatched && (!cancel_requested || native_job_id.is_none()))
        || !valid_state_facts
    {
        return Err(NativeTerminalStoreError::CorruptStore);
    }
    Ok(Some(record_from_context(
        context,
        context_digest,
        NativeRecordFacts {
            state,
            native_job_id,
            exit_code,
            end_received,
            stream_eof,
            stream_error,
            cancel_requested,
        },
    )))
}

fn deserialize_stored_context(
    context_json: &[u8],
) -> Result<NativeSessionContext, NativeTerminalStoreError> {
    if context_json.len() > MAX_NATIVE_CONTEXT_BYTES {
        return Err(NativeTerminalStoreError::CorruptStore);
    }
    let context: NativeSessionContext =
        serde_json::from_slice(context_json).map_err(|_| NativeTerminalStoreError::CorruptStore)?;
    let stored: serde_json::Value =
        serde_json::from_slice(context_json).map_err(|_| NativeTerminalStoreError::CorruptStore)?;
    let canonical =
        serde_json::to_value(&context).map_err(|_| NativeTerminalStoreError::CorruptStore)?;
    if stored != canonical {
        return Err(NativeTerminalStoreError::CorruptStore);
    }
    Ok(context)
}

fn deserialize_stored_preview(
    preview_json: &[u8],
) -> Result<NativeTerminalPreview, NativeTerminalStoreError> {
    if preview_json.len() > MAX_NATIVE_PREVIEW_BYTES {
        return Err(NativeTerminalStoreError::CorruptStore);
    }
    serde_json::from_slice(preview_json).map_err(|_| NativeTerminalStoreError::CorruptStore)
}

fn has_complete_terminal_evidence(
    native_job_id: Option<i64>,
    exit_code: Option<i32>,
    end_received: bool,
    stream_eof: bool,
    stream_error: bool,
) -> bool {
    native_job_id.is_some() && exit_code.is_some() && end_received && stream_eof && !stream_error
}

fn load_mutable_session(
    connection: &Connection,
    session_id: &str,
) -> Result<MutableSession, NativeTerminalStoreError> {
    let record = load_session_from_connection(connection, session_id)?
        .ok_or(NativeTerminalStoreError::NotFound)?;
    let cancel_dispatched = connection.query_row(
        "SELECT cancel_dispatched FROM native_terminal_sessions WHERE session_id = ?1",
        [session_id],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(MutableSession {
        record,
        cancel_dispatched: db_bool(cancel_dispatched)?,
    })
}

fn initialize_schema(connection: &Connection) -> Result<(), NativeTerminalStoreError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        0 => {
            connection.execute_batch("BEGIN IMMEDIATE;")?;
            let schema_result = connection.execute_batch(
                "CREATE TABLE native_terminal_previews (
                    preview_id TEXT PRIMARY KEY NOT NULL,
                    owner_window_label TEXT NOT NULL,
                    preview_digest TEXT NOT NULL,
                    expected_settings_revision INTEGER NOT NULL,
                    expected_issue_revision INTEGER NOT NULL,
                    preview_json BLOB NOT NULL,
                    created_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE native_terminal_sessions (
                    session_id TEXT PRIMARY KEY NOT NULL,
                    attempt_id TEXT NOT NULL UNIQUE,
                    preview_id TEXT NOT NULL UNIQUE,
                    preview_digest TEXT NOT NULL,
                    context_digest TEXT NOT NULL,
                    profile_id TEXT NOT NULL,
                    expected_settings_revision INTEGER NOT NULL,
                    expected_issue_revision INTEGER NOT NULL,
                    native_job_id INTEGER CHECK (native_job_id IS NULL OR native_job_id > 0),
                    state TEXT NOT NULL CHECK (state IN ('starting', 'running', 'closed', 'cancelled', 'unknown')),
                    exit_code INTEGER,
                    end_received INTEGER NOT NULL DEFAULT 0 CHECK (end_received IN (0, 1)),
                    stream_eof INTEGER NOT NULL DEFAULT 0 CHECK (stream_eof IN (0, 1)),
                    stream_error INTEGER NOT NULL DEFAULT 0 CHECK (stream_error IN (0, 1)),
                    cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK (cancel_requested IN (0, 1)),
                    cancel_dispatched INTEGER NOT NULL DEFAULT 0 CHECK (cancel_dispatched IN (0, 1)),
                    context_json BLOB NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL,
                    FOREIGN KEY (preview_id) REFERENCES native_terminal_previews(preview_id)
                 );
                 CREATE INDEX native_terminal_sessions_profile_state
                    ON native_terminal_sessions(profile_id, state);
                  PRAGMA user_version = 1;",
            );
            if let Err(error) = schema_result {
                let _ = connection.execute_batch("ROLLBACK;");
                return Err(error.into());
            }
            if let Err(error) = connection.execute_batch("COMMIT;") {
                let _ = connection.execute_batch("ROLLBACK;");
                return Err(error.into());
            }
        }
        DATABASE_SCHEMA_VERSION => {}
        _ => return Err(NativeTerminalStoreError::UnsupportedSchema),
    }
    Ok(())
}

fn ensure_capacity(connection: &Connection, table: &str) -> Result<(), NativeTerminalStoreError> {
    let sql = match table {
        "native_terminal_previews" => "SELECT COUNT(*) FROM native_terminal_previews WHERE 1 = ?1",
        "native_terminal_sessions" => "SELECT COUNT(*) FROM native_terminal_sessions WHERE 1 = ?1",
        _ => return Err(NativeTerminalStoreError::CorruptStore),
    };
    let count: i64 = connection.query_row(sql, [1], |row| row.get(0))?;
    if count >= MAX_STORED_ROWS {
        return Err(NativeTerminalStoreError::CapacityExceeded);
    }
    Ok(())
}

fn state_as_str(state: SessionState) -> &'static str {
    match state {
        SessionState::Starting => "starting",
        SessionState::Running => "running",
        SessionState::Closed => "closed",
        SessionState::Cancelled => "cancelled",
        SessionState::Unknown => "unknown",
    }
}

fn parse_state(value: &str) -> Result<SessionState, NativeTerminalStoreError> {
    match value {
        "starting" => Ok(SessionState::Starting),
        "running" => Ok(SessionState::Running),
        "closed" => Ok(SessionState::Closed),
        "cancelled" => Ok(SessionState::Cancelled),
        "unknown" => Ok(SessionState::Unknown),
        _ => Err(NativeTerminalStoreError::CorruptStore),
    }
}

fn db_bool(value: i64) -> Result<bool, NativeTerminalStoreError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(NativeTerminalStoreError::CorruptStore),
    }
}

fn sql_revision(value: u64) -> Result<i64, NativeTerminalStoreError> {
    i64::try_from(value).map_err(|_| NativeTerminalStoreError::InvalidContext)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn validate_database_path(path: &Path) -> Result<(), NativeTerminalStoreError> {
    if !path.is_absolute()
        || path.file_name().and_then(|name| name.to_str()) != Some(DATABASE_FILE_NAME)
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(NativeTerminalStoreError::UnsafeStore);
    }
    let parent = path.parent().ok_or(NativeTerminalStoreError::UnsafeStore)?;
    validate_private_parent(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_existing_database(&metadata)?;
            if metadata.len() > MAX_DATABASE_BYTES {
                return Err(NativeTerminalStoreError::CapacityExceeded);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(NativeTerminalStoreError::UnsafeStore),
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_parent(path: &Path) -> Result<(), NativeTerminalStoreError> {
    use std::os::unix::fs::MetadataExt;

    let canonical = path
        .canonicalize()
        .map_err(|_| NativeTerminalStoreError::UnsafeStore)?;
    if canonical != path {
        return Err(NativeTerminalStoreError::UnsafeStore);
    }
    let mut current = PathBuf::new();
    let components = path.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| NativeTerminalStoreError::UnsafeStore)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            return Err(NativeTerminalStoreError::UnsafeStore);
        }
        let final_component = index + 1 == components.len();
        if final_component {
            if metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
                || metadata.mode() & 0o700 != 0o700
            {
                return Err(NativeTerminalStoreError::UnsafeStore);
            }
        } else {
            let sticky = metadata.mode() & 0o1000 != 0;
            if metadata.mode() & 0o022 != 0 && !sticky {
                return Err(NativeTerminalStoreError::UnsafeStore);
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_parent(_path: &Path) -> Result<(), NativeTerminalStoreError> {
    Err(NativeTerminalStoreError::UnsupportedPlatform)
}

#[cfg(unix)]
fn validate_existing_database(metadata: &fs::Metadata) -> Result<(), NativeTerminalStoreError> {
    use std::os::unix::fs::MetadataExt;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(NativeTerminalStoreError::UnsafeStore);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_existing_database(_metadata: &fs::Metadata) -> Result<(), NativeTerminalStoreError> {
    Err(NativeTerminalStoreError::UnsupportedPlatform)
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> Result<(), NativeTerminalStoreError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(NativeTerminalStoreError::UnsafeStore);
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> Result<(), NativeTerminalStoreError> {
    Err(NativeTerminalStoreError::UnsupportedPlatform)
}

#[cfg(unix)]
fn ensure_private_sqlite_sidecars(database_path: &Path) -> Result<(), NativeTerminalStoreError> {
    for suffix in ["-wal", "-shm"] {
        let mut path = database_path.as_os_str().to_owned();
        path.push(suffix);
        let path = PathBuf::from(path);
        match fs::symlink_metadata(&path) {
            Ok(_) => set_private_file(&path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(NativeTerminalStoreError::UnsafeStore),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_sqlite_sidecars(_database_path: &Path) -> Result<(), NativeTerminalStoreError> {
    Err(NativeTerminalStoreError::UnsupportedPlatform)
}

fn sync_parent(path: &Path) -> Result<(), NativeTerminalStoreError> {
    let parent = path.parent().ok_or(NativeTerminalStoreError::UnsafeStore)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn validate_owner_window_label(value: &str) -> Result<(), NativeTerminalStoreError> {
    if value.is_empty()
        || value.len() > MAX_WINDOW_LABEL_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(NativeTerminalStoreError::InvalidContext);
    }
    Ok(())
}
