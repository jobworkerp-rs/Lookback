//! Recovery commands invoked from the [`BootError`](../../../src/components/BootError.tsx)
//! frontend when the sidecar surfaces a structured startup failure. Each
//! command either rewrites local state (evacuate / purge lancedb,
//! null-out a stale embedding preset) and then re-runs the standard
//! sidecar startup pipeline, or — for the unrecoverable variants —
//! exposes the data dir / quits the app so the user can intervene
//! manually.
//!
//! The lancedb-touching commands reuse [`super::embedding_settings::evacuate_vectordb`]
//! so the rename-vs-delete semantics stay identical to the embedding-swap
//! path. `stage_and_start_sidecars` is called explicitly afterwards
//! rather than relying on a `Sidecars::start` shortcut, because the
//! Tauri side normally stages plugins on the same pre-start hop and the
//! restart must mirror that.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use super::embedding_settings::{
    EvacuateMode, evacuate_vectordb, load_embedding_settings, save_embedding_settings,
};
use super::{AppState, embedding_presets};
use crate::error::{AppError, AppResult};
use crate::grpc::proto::llm_memory::service as mem_svc;
use crate::grpc::proto::llm_memory::service::memory_vector_service_client::MemoryVectorServiceClient;
use crate::grpc::proto::llm_memory::service::reflection_vector_service_client::ReflectionVectorServiceClient;
use crate::grpc::proto::llm_memory::service::thread_vector_service_client::ThreadVectorServiceClient;
use crate::sidecar::memory_kind_gate::{self, GateState};

/// Durable state for the migration.  The marker is deliberately small and
/// human-readable: it is the recovery audit record, not an opaque journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MigrationPhase {
    BackupVerified,
    Expanded,
    ClientApplied,
    Contracted,
    RawAccepted,
    RedispatchPending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MigrationMarker {
    phase: MigrationPhase,
    attempt_dir: PathBuf,
    sqlite_backup: PathBuf,
    lancedb_archive: Option<PathBuf>,
    sqlite_sha256: String,
    #[serde(default)]
    redispatch_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryKindRedispatchStatus {
    pub pending: bool,
    pub error: Option<String>,
}

struct MigrationLock(PathBuf);

impl Drop for MigrationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Returns a boot-blocking explanation only while a DB-changing phase is
/// unfinished. Redispatch is safe to retry against the contracted database,
/// so its marker intentionally does not prevent normal startup.
pub fn migration_startup_blocker(data: &crate::data::DataPaths) -> AppResult<Option<String>> {
    let path = data.memory_kind_migration_marker_path();
    if !path.exists() {
        return Ok(None);
    }
    let mut marker: MigrationMarker = serde_json::from_slice(&fs::read(&path)?)
        .map_err(|e| AppError::Config(format!("read migration marker {}: {e}", path.display())))?;
    // The contract script commits before the phase marker is advanced. If the
    // process dies in that gap (including immediately after ClientApplied),
    // the database itself is the source of truth: a clean contract can boot
    // and only needs its asynchronous vector redispatch retried.
    if !matches!(marker.phase, MigrationPhase::RedispatchPending)
        && memory_kind_gate::inspect(&data.memories_sqlite_path()).map_err(AppError::Config)?
            == GateState::ContractReady
    {
        marker.phase = MigrationPhase::RedispatchPending;
        write_marker(data, &marker)?;
        return Ok(None);
    }
    Ok(match marker.phase {
        MigrationPhase::RedispatchPending => None,
        // RawAccepted means the SQLite contract has already passed the gate.
        // A process loss after that point must converge to a retryable vector
        // redispatch, never strand an otherwise usable database at boot.
        MigrationPhase::RawAccepted => {
            marker.phase = MigrationPhase::RedispatchPending;
            write_marker(data, &marker)?;
            None
        }
        phase => Some(format!(
            "memory_kind migration recovery is required (unfinished phase: {phase:?}); inspect {}",
            marker.attempt_dir.display()
        )),
    })
}

fn acquire_migration_lock(data: &crate::data::DataPaths) -> AppResult<MigrationLock> {
    let root = data.memory_kind_migration_work_dir();
    fs::create_dir_all(&root)?;
    let lock = data.memory_kind_migration_lock_path();
    match File::options().write(true).create_new(true).open(&lock) {
        Ok(mut file) => {
            writeln!(file, "{}", std::process::id())?;
            file.sync_all()?;
            Ok(MigrationLock(lock))
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // The lock is a regular file so a power loss can leave it behind.
            // Treat only a numeric PID that no longer exists as stale; malformed
            // evidence stays locked for safety and requires operator review.
            let pid = fs::read_to_string(&lock)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok());
            if pid.is_some_and(|pid| crate::sidecar::reaper::live_exe(pid).is_none()) {
                fs::remove_file(&lock)?;
                return acquire_migration_lock(data);
            }
            Err(AppError::Config(
                "another memory_kind migration is already running; wait for it to finish".into(),
            ))
        }
        Err(error) => Err(error.into()),
    }
}

fn write_marker(data: &crate::data::DataPaths, marker: &MigrationMarker) -> AppResult<()> {
    let path = data.memory_kind_migration_marker_path();
    let tmp = path.with_extension("json.tmp");
    let encoded = serde_json::to_vec_pretty(marker)
        .map_err(|e| AppError::Config(format!("encode migration marker: {e}")))?;
    let mut file = File::create(&tmp)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    fs::rename(&tmp, &path)?;
    File::open(data.memory_kind_migration_work_dir())?.sync_all()?;
    Ok(())
}

fn sha256_file(path: &Path) -> AppResult<String> {
    let digest = crate::data::fsutil::sha256(path)?;
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Result of a recovery action. `restarted` is `true` when the sidecar
/// completed its TCP-listen probe after the rewrite; `restart_error`
/// carries the message of a second failure so the UI can render
/// "applied the fix but the restart still failed: ...".
///
/// `backup_path` is `Some` only after [`recover_evacuate_lancedb`] —
/// the other commands have nothing to back up.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryResult {
    pub restarted: bool,
    pub backup_path: Option<PathBuf>,
    pub restart_error: Option<String>,
}

/// Run the bundled, client-only memory-kind conversion. The caller reaches
/// this command only after the normal startup gate has refused the legacy DB,
/// so there are no Lookback sidecars writing the SQLite file while its backup
/// and SQL migrations run.
#[tauri::command]
pub async fn migrate_memory_kind(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<RecoveryResult> {
    let db_path = state.data.memories_sqlite_path();
    ensure_migration_required(&db_path)?;

    let _lock = acquire_migration_lock(&state.data)?;
    // A concurrent request can inspect the legacy DB, then be descheduled
    // before acquiring this lock. Re-check while holding it so it cannot
    // archive a store that the earlier request has already migrated and
    // restarted.
    ensure_migration_required(&db_path)?;
    let work_dir = migration_work_dir(&state.data.memory_kind_migration_work_dir())?;
    let backup_path = work_dir.join("default.sqlite3.before-migration");
    backup_sqlite(&db_path, &backup_path)?;
    let mut marker = MigrationMarker {
        phase: MigrationPhase::BackupVerified,
        attempt_dir: work_dir.clone(),
        sqlite_backup: backup_path.clone(),
        lancedb_archive: planned_lancedb_archive(&state.data, &work_dir),
        sqlite_sha256: sha256_file(&backup_path)?,
        redispatch_error: None,
    };
    // The recovery record must reach disk before moving the live vector tree.
    // Otherwise a process loss could strand the only vector copy in an
    // unreferenced attempt directory and let a later boot create an empty DB.
    write_marker(&state.data, &marker)?;
    if let Err(error) = archive_lancedb(&state.data, marker.lancedb_archive.as_deref()) {
        if let Err(cleanup_error) = cleanup_failed_lancedb_archive(&state.data, &marker) {
            return Err(AppError::Config(format!(
                "{error}; LanceDB archive rollback also failed: {cleanup_error}. \\
                 Recovery marker retained at {}",
                state.data.memory_kind_migration_marker_path().display()
            )));
        }
        return Err(error);
    }

    let result = migrate_memory_kind_inner(&app, &state, &db_path, &work_dir, &mut marker).await;
    let restart_error = match result {
        Ok(restart_error) => restart_error,
        Err(error) => {
            // No sidecar may write while the two original stores are put
            // back. Both archives were made before any SQL was applied.
            state.sidecars.stop().await?;
            restore_sqlite(&backup_path, &db_path, &marker.sqlite_sha256)?;
            restore_lancedb_archive(&state.data, marker.lancedb_archive.as_deref())?;
            let _ = fs::remove_file(state.data.memory_kind_migration_marker_path());
            return Err(error);
        }
    };

    let Some(error) = restart_error else {
        fs::remove_file(state.data.memory_kind_migration_marker_path())?;
        return Ok(RecoveryResult {
            restarted: true,
            backup_path: Some(backup_path),
            restart_error: None,
        });
    };
    marker.phase = MigrationPhase::RedispatchPending;
    marker.redispatch_error = Some(error.clone());
    write_marker(&state.data, &marker)?;
    Ok(RecoveryResult {
        restarted: true,
        backup_path: Some(backup_path),
        restart_error: Some(error),
    })
}

fn ensure_migration_required(db_path: &Path) -> AppResult<()> {
    if memory_kind_gate::inspect(db_path).map_err(AppError::Config)? == GateState::MigrationRequired
    {
        Ok(())
    } else {
        Err(AppError::Config(
            "memory-kind migration is not required for the current database".into(),
        ))
    }
}

fn memory_kind_redispatch_status(
    data: &crate::data::DataPaths,
) -> AppResult<MemoryKindRedispatchStatus> {
    let path = data.memory_kind_migration_marker_path();
    if !path.exists() {
        return Ok(MemoryKindRedispatchStatus {
            pending: false,
            error: None,
        });
    }
    let marker: MigrationMarker = serde_json::from_slice(&fs::read(&path)?).map_err(|error| {
        AppError::Config(format!("read migration marker {}: {error}", path.display()))
    })?;
    let pending = matches!(marker.phase, MigrationPhase::RedispatchPending);
    Ok(MemoryKindRedispatchStatus {
        pending,
        error: pending.then_some(marker.redispatch_error).flatten(),
    })
}

#[tauri::command]
pub fn get_memory_kind_redispatch_status(
    state: tauri::State<'_, AppState>,
) -> AppResult<MemoryKindRedispatchStatus> {
    memory_kind_redispatch_status(&state.data)
}

/// Retry only the asynchronous vector work after the relational migration has
/// reached its accepted contract. The durable marker is deleted exclusively
/// after all three enqueue RPCs acknowledge zero failed jobs.
#[tauri::command]
pub async fn retry_memory_kind_redispatch(state: tauri::State<'_, AppState>) -> AppResult<()> {
    let path = state.data.memory_kind_migration_marker_path();
    let mut marker: MigrationMarker =
        serde_json::from_slice(&fs::read(&path)?).map_err(|error| {
            AppError::Config(format!("read migration marker {}: {error}", path.display()))
        })?;
    if !matches!(marker.phase, MigrationPhase::RedispatchPending) {
        return Err(AppError::Config(
            "memory-kind vector redispatch is not pending".into(),
        ));
    }
    match redispatch_migrated_vectors(&state).await {
        Ok(()) => {
            fs::remove_file(path)?;
            Ok(())
        }
        Err(error) => {
            marker.redispatch_error = Some(error.to_string());
            write_marker(&state.data, &marker)?;
            Err(error)
        }
    }
}

async fn migrate_memory_kind_inner(
    app: &AppHandle,
    state: &AppState,
    db_path: &Path,
    work_dir: &Path,
    marker: &mut MigrationMarker,
) -> AppResult<Option<String>> {
    let toolkit = crate::data::paths::bundled_resource_path(app, "migration-toolkit")
        .ok_or_else(|| AppError::Config("bundled memory-kind toolkit is missing".into()))?;
    let binary = crate::resolve_bin(
        "LOOKBACK_MIGRATE_MEMORY_KIND_BIN",
        "migrate-memory-kind",
        "migrate-memory-kind",
        "../../memories/target/release/migrate-memory-kind",
    )
    .map_err(|error| AppError::Config(format!("resolve migrate-memory-kind: {error}")))?;
    let (expand_sql, contract_sql) = migration_sql_paths(&toolkit)?;

    apply_expand_sql(db_path, &expand_sql)?;
    marker.phase = MigrationPhase::Expanded;
    write_marker(&state.data, marker)?;

    let mapping = work_dir.join("mapping.json");
    std::fs::write(&mapping, "{}")?;
    let audit = work_dir.join("client-apply-audit.json");
    let output = Command::new(binary)
        .env("SQLITE_URL", format!("sqlite://{}", db_path.display()))
        .args(["client-apply", "--mapping"])
        .arg(&mapping)
        .arg("--output")
        .arg(&audit)
        .output()?;
    let stdout_log = work_dir.join("client-apply.stdout.log");
    let stderr_log = work_dir.join("client-apply.stderr.log");
    fs::write(&stdout_log, &output.stdout)?;
    fs::write(&stderr_log, &output.stderr)?;
    if !output.status.success() || !completed_audit_is_clean(&audit)? {
        return Err(AppError::Config(format!(
            "memory-kind client migration did not produce a clean audit \
                 (exit_status={}, {}; stdout={}; stderr={})",
            output.status,
            audit_summary(&audit),
            stdout_log.display(),
            stderr_log.display(),
        )));
    }
    marker.phase = MigrationPhase::ClientApplied;
    write_marker(&state.data, marker)?;
    apply_sql(db_path, &contract_sql)?;
    marker.phase = MigrationPhase::Contracted;
    write_marker(&state.data, marker)?;
    if memory_kind_gate::inspect(db_path).map_err(AppError::Config)? != GateState::ContractReady {
        return Err(AppError::Config(
            "memory-kind migration did not satisfy the Lookback data contract".into(),
        ));
    }
    marker.phase = MigrationPhase::RawAccepted;
    write_marker(&state.data, marker)?;

    state.invalidate_clients().await;
    crate::stage_and_start_sidecars(app, &state.sidecars, &state.data).await;
    if let Some(error) = state.sidecars.last_start_error() {
        return Err(AppError::Config(format!(
            "restart after migration: {error}"
        )));
    }
    // Redispatch enqueues asynchronous work. Once the RDB contract and
    // sidecar startup succeeded, an enqueue error must not roll RAW back.
    // The settings UI can expose the returned message as a retry notice.
    Ok(redispatch_migrated_vectors(state)
        .await
        .err()
        .map(|error| error.to_string()))
}

fn migration_sql_paths(toolkit: &Path) -> AppResult<(PathBuf, PathBuf)> {
    let expand = toolkit.join("sqlite/011_add_memory_kind.sql");
    let contract = toolkit.join("sqlite/012_contract_memory_kind.sql");
    for path in [&expand, &contract] {
        if !path.is_file() {
            return Err(AppError::Config(format!(
                "bundled memory-kind migration SQL is missing: {}",
                path.display()
            )));
        }
    }
    Ok((expand, contract))
}

fn migration_work_dir(data_root: &Path) -> AppResult<PathBuf> {
    fs::create_dir_all(data_root)?;
    let id = format!(
        "attempt-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    );
    let path = data_root.join(id);
    std::fs::create_dir(&path)?;
    Ok(path)
}

fn backup_sqlite(source: &Path, destination: &Path) -> AppResult<()> {
    let source =
        rusqlite::Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| AppError::Config(format!("open SQLite backup source: {error}")))?;
    let quoted = destination.display().to_string().replace('\'', "''");
    source
        .execute_batch(&format!("VACUUM INTO '{quoted}'"))
        .map_err(|error| AppError::Config(format!("create SQLite backup: {error}")))?;
    Ok(())
}

fn restore_sqlite(backup: &Path, destination: &Path, expected_sha256: &str) -> AppResult<()> {
    if sha256_file(backup)? != expected_sha256 {
        return Err(AppError::Config(format!(
            "SQLite migration backup hash does not match: {}",
            backup.display()
        )));
    }
    for suffix in ["-wal", "-shm"] {
        let sidecar = sqlite_sidecar_path(destination, suffix);
        match fs::remove_file(&sidecar) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(AppError::Config(format!(
                    "remove stale SQLite sidecar {}: {error}",
                    sidecar.display()
                )));
            }
        }
    }
    fs::copy(backup, destination)?;
    Ok(())
}

fn sqlite_sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

/// Move the complete LanceDB tree into the attempt directory before any RDB
/// mutation. `rename` keeps the archive on the data root's filesystem and is
/// atomic; the new empty directory is the only tree a migrated sidecar sees.
fn planned_lancedb_archive(data: &crate::data::DataPaths, work_dir: &Path) -> Option<PathBuf> {
    let source = data.lancedb_dir();
    if !source.exists() {
        return None;
    }
    Some(work_dir.join("lancedb.before-migration"))
}

fn archive_lancedb(data: &crate::data::DataPaths, archive: Option<&Path>) -> AppResult<()> {
    let Some(archive) = archive else {
        return Ok(());
    };
    let source = data.lancedb_dir();
    fs::rename(&source, archive)
        .map_err(|e| AppError::Config(format!("archive LanceDB {}: {e}", source.display())))?;
    fs::create_dir_all(&source)?;
    Ok(())
}

fn restore_lancedb_archive(data: &crate::data::DataPaths, archive: Option<&Path>) -> AppResult<()> {
    let Some(archive) = archive else {
        return Ok(());
    };
    if !archive.exists() {
        return Err(AppError::Config(format!(
            "LanceDB migration archive is missing: {}",
            archive.display()
        )));
    }
    let destination = data.lancedb_dir();
    if destination.exists() {
        fs::remove_dir_all(&destination)?;
    }
    fs::rename(archive, destination)?;
    Ok(())
}

/// Restore an archive only when the live tree disappeared, then clear the
/// marker once the original store is provably present again. If restoring
/// fails, deliberately keep the marker so boot cannot create a new empty
/// LanceDB over the sole remaining copy.
fn cleanup_failed_lancedb_archive(
    data: &crate::data::DataPaths,
    marker: &MigrationMarker,
) -> AppResult<()> {
    let source = data.lancedb_dir();
    if let Some(archive) = marker
        .lancedb_archive
        .as_deref()
        .filter(|path| path.exists())
    {
        restore_lancedb_archive(data, Some(archive))?;
    }
    if !source.exists() {
        return Err(AppError::Config(format!(
            "LanceDB archive did not leave a recoverable live tree at {}",
            source.display()
        )));
    }
    fs::remove_file(data.memory_kind_migration_marker_path())?;
    Ok(())
}

fn apply_sql(db_path: &Path, sql_path: &Path) -> AppResult<()> {
    let sql = std::fs::read_to_string(sql_path)?;
    let connection = rusqlite::Connection::open(db_path)
        .map_err(|error| AppError::Config(format!("open SQLite migration target: {error}")))?;
    connection
        .execute_batch(&sql)
        .map_err(|error| AppError::Config(format!("apply {}: {error}", sql_path.display())))?;
    Ok(())
}

/// Apply the staged expand script without re-adding a column produced by an
/// earlier rollout. The script remains the single source for its indexes and
/// SQL text; only its two known `ADD COLUMN` statements are skipped when the
/// corresponding schema evidence already exists.
fn apply_expand_sql(db_path: &Path, sql_path: &Path) -> AppResult<()> {
    let sql = fs::read_to_string(sql_path)?;
    let connection = rusqlite::Connection::open(db_path)
        .map_err(|error| AppError::Config(format!("open SQLite migration target: {error}")))?;
    let has_thread_kind = sqlite_column_exists(&connection, "thread", "memory_kind")?;
    let has_memory_kind = sqlite_column_exists(&connection, "memory", "memory_kind")?;

    for statement in sql.split_inclusive(';') {
        let adds_thread_kind = statement.contains("ALTER TABLE `thread` ADD COLUMN `memory_kind`");
        let adds_memory_kind = statement.contains("ALTER TABLE `memory` ADD COLUMN `memory_kind`");
        if (adds_thread_kind && has_thread_kind) || (adds_memory_kind && has_memory_kind) {
            continue;
        }
        connection
            .execute_batch(statement)
            .map_err(|error| AppError::Config(format!("apply {}: {error}", sql_path.display())))?;
    }
    Ok(())
}

fn sqlite_column_exists(
    connection: &rusqlite::Connection,
    table: &str,
    column: &str,
) -> AppResult<bool> {
    connection
        .query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1)"),
            [column],
            |row| row.get(0),
        )
        .map_err(|error| AppError::Config(format!("inspect {table} schema: {error}")))
}

fn completed_audit_is_clean(path: &Path) -> AppResult<bool> {
    let audit: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)
        .map_err(|error| AppError::Config(format!("parse client migration audit: {error}")))?;
    Ok(
        audit.get("status").and_then(serde_json::Value::as_str) == Some("completed")
            && audit
                .get("warnings")
                .and_then(serde_json::Value::as_array)
                .is_some_and(Vec::is_empty)
            && audit
                .get("failures")
                .and_then(serde_json::Value::as_array)
                .is_some_and(Vec::is_empty),
    )
}

fn audit_summary(path: &Path) -> String {
    match fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    {
        Some(audit) => format!(
            "audit={} status={} warnings={} failures={}",
            path.display(),
            audit
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("missing"),
            audit
                .get("warnings")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len),
            audit
                .get("failures")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len),
        ),
        None => format!("audit={} unavailable", path.display()),
    }
}

async fn redispatch_migrated_vectors(state: &AppState) -> AppResult<()> {
    let channel = state.memories_channel().await?;
    let memory = MemoryVectorServiceClient::new(channel.clone())
        .redispatch_embeddings(mem_svc::RedispatchEmbeddingsRequest {
            user_id: Some(super::summaries::LOOKBACK_USER_ID),
            thread_id: None,
            batch_size: None,
            kinds: Vec::new(),
            memory_kinds: Vec::new(),
        })
        .await?
        .into_inner();
    ensure_redispatch_succeeded("memory", memory.failed_count)?;

    let thread = ThreadVectorServiceClient::new(channel.clone())
        .redispatch_embeddings(mem_svc::ThreadRedispatchEmbeddingsRequest {
            user_id: Some(super::summaries::LOOKBACK_USER_ID),
            batch_size: None,
        })
        .await?
        .into_inner();
    ensure_redispatch_succeeded("thread", thread.failed_count)?;

    let reflection = ReflectionVectorServiceClient::new(channel)
        .redispatch_reflection_embeddings(mem_svc::RedispatchReflectionEmbeddingsRequest {
            kind: 2,
            filter: None,
            batch_size: None,
        })
        .await?
        .into_inner();
    ensure_redispatch_succeeded("reflection", reflection.failed_count)?;
    Ok(())
}

fn ensure_redispatch_succeeded(scope: &str, failed_count: u32) -> AppResult<()> {
    if failed_count == 0 {
        return Ok(());
    }
    Err(AppError::Config(format!(
        "{scope} Redispatch accepted the RPC but failed to enqueue {failed_count} jobs"
    )))
}

/// Stop the sidecars, rename the existing lancedb tree under
/// `<data>/lancedb-backup/lancedb-<ts>/`, then re-run the standard
/// startup pipeline. Use this when the user wants the dimension
/// mismatch fixed without losing the existing vectors (they can be
/// restored manually from the backup).
#[tauri::command]
pub async fn recover_evacuate_lancedb(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<RecoveryResult> {
    run_lancedb_recovery(&app, &state, EvacuateMode::Evacuate).await
}

/// Same as [`recover_evacuate_lancedb`] but `rm -rf`s the existing
/// lancedb tree instead of renaming it. Last-resort branch when the
/// disk is too full to hold a copy.
#[tauri::command]
pub async fn recover_purge_lancedb(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<RecoveryResult> {
    run_lancedb_recovery(&app, &state, EvacuateMode::Delete).await
}

async fn run_lancedb_recovery(
    app: &AppHandle,
    state: &AppState,
    mode: EvacuateMode,
) -> AppResult<RecoveryResult> {
    // Drop cached gRPC clients first — they hold a `Channel` against the
    // old sidecar's port, which becomes unusable as soon as we stop the
    // process below.
    state.invalidate_clients().await;
    state.sidecars.stop().await?;

    // Re-running the failed startup right away would re-hit the same
    // schema-mismatch panic; the evacuation step is what makes the
    // restart viable.
    let backup_path = evacuate_vectordb(&state.data, mode)?;

    crate::stage_and_start_sidecars(app, &state.sidecars, &state.data).await;
    let restart_error = state.sidecars.last_start_error();
    Ok(RecoveryResult {
        restarted: restart_error.is_none(),
        backup_path,
        restart_error,
    })
}

/// Null out a `preset_id` that no longer exists in the curated list
/// (typical after a release that retired a preset — e.g. removing Ruri
/// without `evacuate`ing first) and re-run the standard startup
/// pipeline. The lancedb directory is intentionally NOT touched; if the
/// stored vectors are also at the wrong dimension the BootError UI will
/// surface again and the user can pick the evacuate / purge action
/// next.
#[tauri::command]
pub async fn recover_reset_embedding_settings(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<RecoveryResult> {
    reset_embedding_preset_inner(&state.data.embedding_settings_path())?;
    state.invalidate_clients().await;
    state.sidecars.stop().await?;
    crate::stage_and_start_sidecars(app.app_handle(), &state.sidecars, &state.data).await;
    let restart_error = state.sidecars.last_start_error();
    Ok(RecoveryResult {
        restarted: restart_error.is_none(),
        backup_path: None,
        restart_error,
    })
}

/// Pure helper extracted so the rewrite policy ("null out only unknown
/// presets, keep null / curated / custom alone") can be unit-tested
/// without spinning up a Tauri AppHandle.
fn reset_embedding_preset_inner(path: &std::path::Path) -> AppResult<()> {
    let mut settings = load_embedding_settings(path);
    let needs_reset = match settings.preset_id.as_deref() {
        None => false,
        Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID) => false,
        Some(id) => embedding_presets::find_preset(id).is_none(),
    };
    if needs_reset {
        settings.preset_id = None;
        save_embedding_settings(path, &settings)?;
    }
    Ok(())
}

/// Open the log directory in Finder. Used as an escape hatch on the
/// BootError UI when the failure is not auto-recoverable (e.g. corrupt
/// LanceDB, env var typo) so the user can collect logs before
/// reporting. Lookback is macOS-only (see README "リリースビルド"); the
/// `open` invocation is therefore not gated behind a cross-platform
/// shim.
///
/// Uses `std::process::Command` rather than `tauri-plugin-shell::open`
/// so we don't have to widen the shell capability scope to the data
/// root.
#[tauri::command]
pub fn open_log_dir(state: tauri::State<'_, AppState>) -> AppResult<()> {
    let dir = state.data.log_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| AppError::Config(format!("create log dir {}: {e}", dir.display())))?;
    }
    let status = std::process::Command::new("open")
        .arg(&dir)
        .status()
        .map_err(|e| AppError::Config(format!("spawn `open` failed: {e}")))?;
    if !status.success() {
        return Err(AppError::Config(format!(
            "`open {}` exited with {status}",
            dir.display()
        )));
    }
    Ok(())
}

/// Open the bundled client-migration runbook. The migration must remain an
/// operator-controlled maintenance action, but the blocked-start screen must
/// provide a direct route to the exact runbook shipped with this release.
#[tauri::command]
pub fn open_memory_kind_migration_guide(app: AppHandle) -> AppResult<()> {
    let guide = crate::data::paths::bundled_resource_path(
        &app,
        "migration-toolkit/lookback-migration-guide_ja.md",
    )
    .ok_or_else(|| AppError::Config("bundled memory-kind migration guide is missing".into()))?;
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else {
        return Err(AppError::Config(
            "opening the memory-kind migration guide is unsupported on this platform".into(),
        ));
    };
    let status = std::process::Command::new(opener)
        .arg(&guide)
        .status()
        .map_err(|e| AppError::Config(format!("spawn `{opener}` failed: {e}")))?;
    if !status.success() {
        return Err(AppError::Config(format!(
            "`{opener} {}` exited with {status}",
            guide.display()
        )));
    }
    Ok(())
}

/// Cleanly quit the app from the BootError UI. Stops the sidecars
/// first so a structured-error popup doesn't leave orphan children
/// behind.
#[tauri::command]
pub async fn quit_app(state: tauri::State<'_, AppState>, app: AppHandle) -> AppResult<()> {
    // Best-effort stop: a partial start may have left some children
    // alive; subsequent launches reap orphans by PID file so a failure
    // here isn't fatal.
    let _ = state.sidecars.stop().await;
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::embedding_settings::EmbeddingSettings;
    use super::*;

    #[test]
    fn completed_audit_requires_success_without_warnings_or_failures() {
        let dir = tempfile::tempdir().unwrap();
        let clean = dir.path().join("clean.json");
        fs::write(
            &clean,
            r#"{"status":"completed","warnings":[],"failures":[]}"#,
        )
        .unwrap();
        assert!(completed_audit_is_clean(&clean).unwrap());

        let warning = dir.path().join("warning.json");
        fs::write(
            &warning,
            r#"{"status":"completed","warnings":[{}],"failures":[]}"#,
        )
        .unwrap();
        assert!(!completed_audit_is_clean(&warning).unwrap());
    }

    #[test]
    fn audit_summary_preserves_the_actionable_counts() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.json");
        fs::write(
            &audit,
            r#"{"status":"completed","warnings":[{}],"failures":[{},{}]}"#,
        )
        .unwrap();

        let summary = audit_summary(&audit);
        assert!(summary.contains("status=completed"));
        assert!(summary.contains("warnings=1"));
        assert!(summary.contains("failures=2"));
    }

    #[test]
    fn audit_summary_reports_a_missing_audit_file() {
        let dir = tempfile::tempdir().unwrap();
        let summary = audit_summary(&dir.path().join("missing.json"));
        assert!(summary.contains("unavailable"));
    }

    fn write_settings(path: &std::path::Path, preset: Option<&str>) {
        let s = EmbeddingSettings {
            preset_id: preset.map(String::from),
            ..Default::default()
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        save_embedding_settings(path, &s).unwrap();
    }

    #[test]
    fn reset_nulls_unknown_preset_id() {
        // Today's actual incident: a release removed `ruri-v3-310m` from
        // the curated list while a user still had it stored. Resolve
        // falls back to default (different dim) and the LanceDB schema
        // mismatch panic follows. The recovery command must rewrite the
        // saved preset to `null` so the next launch resolves to the
        // default preset cleanly.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("embedding-settings.json");
        write_settings(&p, Some("ruri-v3-310m"));
        reset_embedding_preset_inner(&p).unwrap();
        let after = load_embedding_settings(&p);
        assert!(after.preset_id.is_none());
    }

    #[test]
    fn reset_keeps_curated_preset_id() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("embedding-settings.json");
        let curated = embedding_presets::default_preset().id;
        write_settings(&p, Some(curated));
        reset_embedding_preset_inner(&p).unwrap();
        let after = load_embedding_settings(&p);
        assert_eq!(after.preset_id.as_deref(), Some(curated));
    }

    #[test]
    fn reset_keeps_custom_sentinel() {
        // "custom" routes through the free-text branch; treat it as
        // valid so a user with a deliberate Custom config isn't reset
        // out from under them.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("embedding-settings.json");
        write_settings(&p, Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID));
        reset_embedding_preset_inner(&p).unwrap();
        let after = load_embedding_settings(&p);
        assert_eq!(
            after.preset_id.as_deref(),
            Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID)
        );
    }

    #[test]
    fn reset_keeps_null_preset_id() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("embedding-settings.json");
        write_settings(&p, None);
        reset_embedding_preset_inner(&p).unwrap();
        let after = load_embedding_settings(&p);
        assert!(after.preset_id.is_none());
    }

    #[test]
    fn active_marker_blocks_startup_but_pending_redispatch_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path());
        fs::create_dir_all(data.memory_kind_migration_work_dir()).unwrap();
        let marker = MigrationMarker {
            phase: MigrationPhase::ClientApplied,
            attempt_dir: tmp.path().join("attempt"),
            sqlite_backup: tmp.path().join("backup.sqlite3"),
            lancedb_archive: None,
            sqlite_sha256: "hash".into(),
            redispatch_error: None,
        };
        write_marker(&data, &marker).unwrap();
        assert!(migration_startup_blocker(&data).unwrap().is_some());

        write_marker(
            &data,
            &MigrationMarker {
                phase: MigrationPhase::RedispatchPending,
                ..marker
            },
        )
        .unwrap();
        assert!(migration_startup_blocker(&data).unwrap().is_none());
    }

    #[test]
    fn committed_contract_promotes_pre_raw_marker_to_redispatch_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path());
        fs::create_dir_all(data.memory_kind_migration_work_dir()).unwrap();
        fs::create_dir_all(data.memories_data_dir()).unwrap();
        rusqlite::Connection::open(data.memories_sqlite_path())
            .unwrap()
            .execute_batch(
                "CREATE TABLE memory (user_id INTEGER, memory_kind INTEGER NOT NULL, external_id TEXT); CREATE TABLE thread (user_id INTEGER, memory_kind INTEGER NOT NULL);",
            )
            .unwrap();

        for phase in [MigrationPhase::ClientApplied, MigrationPhase::Contracted] {
            write_marker(
                &data,
                &MigrationMarker {
                    phase,
                    attempt_dir: tmp.path().join("attempt"),
                    sqlite_backup: tmp.path().join("backup.sqlite3"),
                    lancedb_archive: None,
                    sqlite_sha256: "hash".into(),
                    redispatch_error: None,
                },
            )
            .unwrap();

            assert!(migration_startup_blocker(&data).unwrap().is_none());
            let persisted: MigrationMarker = serde_json::from_slice(
                &fs::read(data.memory_kind_migration_marker_path()).unwrap(),
            )
            .unwrap();
            assert!(matches!(persisted.phase, MigrationPhase::RedispatchPending));
        }
    }

    #[test]
    fn migration_recheck_rejects_a_database_already_contracted_by_another_run() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("default.sqlite3");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE memory (user_id INTEGER, memory_kind INTEGER NOT NULL, external_id TEXT); CREATE TABLE thread (user_id INTEGER, memory_kind INTEGER NOT NULL);",
            )
            .unwrap();

        let error = ensure_migration_required(&db).unwrap_err();
        assert!(error.to_string().contains("not required"));
    }

    #[test]
    fn failed_lancedb_archive_restores_the_tree_before_removing_its_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path());
        fs::create_dir_all(data.memory_kind_migration_work_dir()).unwrap();
        let archive = tmp.path().join("attempt/lancedb.before-migration");
        fs::create_dir_all(&archive).unwrap();
        fs::write(archive.join("vectors.bin"), "original vectors").unwrap();
        let marker = MigrationMarker {
            phase: MigrationPhase::BackupVerified,
            attempt_dir: tmp.path().join("attempt"),
            sqlite_backup: tmp.path().join("backup.sqlite3"),
            lancedb_archive: Some(archive.clone()),
            sqlite_sha256: "hash".into(),
            redispatch_error: None,
        };
        write_marker(&data, &marker).unwrap();

        cleanup_failed_lancedb_archive(&data, &marker).unwrap();

        assert!(data.lancedb_dir().join("vectors.bin").exists());
        assert!(!archive.exists());
        assert!(!data.memory_kind_migration_marker_path().exists());
    }

    #[test]
    fn raw_accepted_marker_converges_to_redispatch_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path());
        fs::create_dir_all(data.memory_kind_migration_work_dir()).unwrap();
        write_marker(
            &data,
            &MigrationMarker {
                phase: MigrationPhase::RawAccepted,
                attempt_dir: tmp.path().join("attempt"),
                sqlite_backup: tmp.path().join("backup.sqlite3"),
                lancedb_archive: None,
                sqlite_sha256: "hash".into(),
                redispatch_error: None,
            },
        )
        .unwrap();

        assert!(migration_startup_blocker(&data).unwrap().is_none());
        let persisted: MigrationMarker =
            serde_json::from_slice(&fs::read(data.memory_kind_migration_marker_path()).unwrap())
                .unwrap();
        assert!(matches!(persisted.phase, MigrationPhase::RedispatchPending));
    }

    #[test]
    fn sqlite_restore_rejects_a_tampered_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let backup = tmp.path().join("backup.sqlite3");
        fs::write(&backup, "original").unwrap();
        let expected = sha256_file(&backup).unwrap();
        fs::write(&backup, "tampered").unwrap();
        let error =
            restore_sqlite(&backup, &tmp.path().join("target.sqlite3"), &expected).unwrap_err();
        assert!(error.to_string().contains("hash does not match"));
    }

    #[test]
    fn sqlite_restore_removes_stale_wal_and_shm_files() {
        let tmp = tempfile::tempdir().unwrap();
        let backup = tmp.path().join("backup.sqlite3");
        let destination = tmp.path().join("default.sqlite3");
        fs::write(&backup, "original database").unwrap();
        fs::write(&destination, "partially migrated database").unwrap();
        fs::write(sqlite_sidecar_path(&destination, "-wal"), "stale WAL").unwrap();
        fs::write(sqlite_sidecar_path(&destination, "-shm"), "stale SHM").unwrap();

        restore_sqlite(&backup, &destination, &sha256_file(&backup).unwrap()).unwrap();

        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "original database"
        );
        assert!(!sqlite_sidecar_path(&destination, "-wal").exists());
        assert!(!sqlite_sidecar_path(&destination, "-shm").exists());
    }

    fn migration_schema(path: &Path, memory_has_kind: bool, thread_has_kind: bool) {
        let memory_kind = if memory_has_kind {
            ", memory_kind INTEGER NOT NULL"
        } else {
            ""
        };
        let thread_kind = if thread_has_kind {
            ", memory_kind INTEGER NOT NULL"
        } else {
            ""
        };
        rusqlite::Connection::open(path)
            .unwrap()
            .execute_batch(&format!(
                "CREATE TABLE memory (user_id INTEGER, updated_at INTEGER{memory_kind}); CREATE TABLE thread (user_id INTEGER, updated_at INTEGER{thread_kind});"
            ))
            .unwrap();
    }

    fn expand_sql_path(dir: &Path) -> PathBuf {
        let path = dir.join("011_add_memory_kind.sql");
        fs::write(
            &path,
            include_str!("../../migration-toolkit/sqlite/011_add_memory_kind.sql"),
        )
        .unwrap();
        path
    }

    #[test]
    fn expand_sql_accepts_an_already_added_memory_kind_column() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("default.sqlite3");
        migration_schema(&db, true, true);

        apply_expand_sql(&db, &expand_sql_path(tmp.path())).unwrap();

        let connection = rusqlite::Connection::open(&db).unwrap();
        assert!(sqlite_column_exists(&connection, "memory", "memory_kind").unwrap());
        assert!(sqlite_column_exists(&connection, "thread", "memory_kind").unwrap());
    }

    #[test]
    fn expand_sql_adds_only_the_missing_memory_kind_column() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("default.sqlite3");
        migration_schema(&db, true, false);

        apply_expand_sql(&db, &expand_sql_path(tmp.path())).unwrap();

        let connection = rusqlite::Connection::open(&db).unwrap();
        assert!(sqlite_column_exists(&connection, "memory", "memory_kind").unwrap());
        assert!(sqlite_column_exists(&connection, "thread", "memory_kind").unwrap());
    }

    #[test]
    fn migration_marker_is_durable_before_lancedb_is_archived() {
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path());
        let source = data.lancedb_dir();
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(data.memory_kind_migration_work_dir()).unwrap();
        fs::write(source.join("vectors.bin"), "original vectors").unwrap();
        let work_dir = tmp.path().join("attempt");
        fs::create_dir(&work_dir).unwrap();

        let archive = planned_lancedb_archive(&data, &work_dir);
        let marker = MigrationMarker {
            phase: MigrationPhase::BackupVerified,
            attempt_dir: work_dir.clone(),
            sqlite_backup: work_dir.join("default.sqlite3.before-migration"),
            lancedb_archive: archive.clone(),
            sqlite_sha256: "hash".into(),
            redispatch_error: None,
        };
        write_marker(&data, &marker).unwrap();

        // A crash at this point must still leave both the live tree and a
        // durable recovery record, rather than an untracked archive.
        assert!(source.join("vectors.bin").exists());
        let persisted: MigrationMarker =
            serde_json::from_slice(&fs::read(data.memory_kind_migration_marker_path()).unwrap())
                .unwrap();
        assert_eq!(persisted.lancedb_archive, archive);

        archive_lancedb(&data, archive.as_deref()).unwrap();
        assert!(!source.join("vectors.bin").exists());
        assert!(archive.unwrap().join("vectors.bin").exists());
    }

    #[test]
    fn stale_migration_pid_lock_is_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path());
        fs::create_dir_all(data.memory_kind_migration_work_dir()).unwrap();
        fs::write(data.memory_kind_migration_lock_path(), "99999999\n").unwrap();
        let lock = acquire_migration_lock(&data).unwrap();
        assert!(data.memory_kind_migration_lock_path().exists());
        drop(lock);
        assert!(!data.memory_kind_migration_lock_path().exists());
    }

    #[test]
    fn redispatch_requires_every_rpc_to_report_zero_failed_jobs() {
        assert!(ensure_redispatch_succeeded("thread", 0).is_ok());
        let error = ensure_redispatch_succeeded("reflection", 2).unwrap_err();
        assert!(error.to_string().contains("reflection"));
        assert!(error.to_string().contains("2 jobs"));
    }

    #[test]
    fn redispatch_pending_status_preserves_a_retryable_failure_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path());
        fs::create_dir_all(data.memory_kind_migration_work_dir()).unwrap();
        write_marker(
            &data,
            &MigrationMarker {
                phase: MigrationPhase::RedispatchPending,
                attempt_dir: tmp.path().join("attempt"),
                sqlite_backup: tmp.path().join("backup.sqlite3"),
                lancedb_archive: None,
                sqlite_sha256: "hash".into(),
                redispatch_error: Some("memory RPC unavailable".into()),
            },
        )
        .unwrap();

        let status = memory_kind_redispatch_status(&data).unwrap();
        assert!(status.pending);
        assert_eq!(status.error.as_deref(), Some("memory RPC unavailable"));
    }

    #[test]
    fn migration_sql_paths_do_not_require_a_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let toolkit = tmp.path().join("toolkit");
        fs::create_dir(&toolkit).unwrap();
        fs::create_dir(toolkit.join("sqlite")).unwrap();
        let expand = toolkit.join("sqlite/011_add_memory_kind.sql");
        let contract = toolkit.join("sqlite/012_contract_memory_kind.sql");
        fs::write(&expand, "-- expand").unwrap();
        fs::write(&contract, "-- contract").unwrap();

        assert_eq!(migration_sql_paths(&toolkit).unwrap(), (expand, contract));
    }

    #[test]
    fn migration_sql_paths_reject_missing_required_sql() {
        let tmp = tempfile::tempdir().unwrap();
        let toolkit = tmp.path().join("toolkit");
        fs::create_dir_all(toolkit.join("sqlite")).unwrap();
        fs::write(toolkit.join("sqlite/011_add_memory_kind.sql"), "-- expand").unwrap();

        let error = migration_sql_paths(&toolkit).unwrap_err();
        assert!(error.to_string().contains("012_contract_memory_kind.sql"));
    }
}
