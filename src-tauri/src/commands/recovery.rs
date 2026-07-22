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
//!
//! The memory_kind migration is two-phase: `preview_memory_kind_migration`
//! non-destructively inspects a SQLite backup and returns what would be
//! deleted, and `migrate_memory_kind` only performs the destructive apply
//! (and, if needed, the unresolved-record prune) after the frontend echoes
//! that preview back as an explicit approval.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use url::Url;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryKindRedispatchStatus {
    pub pending: bool,
    pub error: Option<String>,
}

/// Outcome of a non-destructive migration inspection performed against a
/// SQLite backup: what would be deleted, and whether the user must confirm.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MemoryKindMigrationPreview {
    pub warning_count: usize,
    pub total_record_count: u64,
    pub unresolved_memory_count: usize,
    pub unresolved_thread_count: usize,
    pub planned_memory_delete_count: usize,
    pub planned_thread_delete_count: usize,
    pub planned_memory_ids: Vec<i64>,
    pub planned_thread_ids: Vec<i64>,
    pub related_deletion_counts: BTreeMap<String, u64>,
    pub requires_confirmation: bool,
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
    approval: MemoryKindMigrationPreview,
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

    let result =
        migrate_memory_kind_inner(&app, &state, &db_path, &work_dir, &mut marker, &approval).await;
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

/// Inspect a SQLite copy before asking the user to approve deletion. The
/// production database and LanceDB are never opened for writing here.
#[tauri::command]
pub async fn preview_memory_kind_migration(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<MemoryKindMigrationPreview> {
    let db_path = state.data.memories_sqlite_path();
    ensure_migration_required(&db_path)?;
    let _lock = acquire_migration_lock(&state.data)?;
    ensure_migration_required(&db_path)?;
    let work_dir = migration_work_dir(&state.data.memory_kind_migration_work_dir())?;
    let preview_db = work_dir.join("preview.sqlite3");
    backup_sqlite(&db_path, &preview_db)?;
    let total_record_count = sqlite_memory_thread_count(&db_path)?;
    let toolkit = crate::data::paths::bundled_resource_path(&app, "migration-toolkit")
        .ok_or_else(|| AppError::Config("bundled memory-kind toolkit is missing".into()))?;
    let (expand_sql, _) = migration_sql_paths(&toolkit)?;
    apply_expand_sql(&preview_db, &expand_sql)?;
    let binary = resolve_migration_binary(&app)?;
    let mapping = work_dir.join("mapping.json");
    fs::write(&mapping, "{}")?;
    let audit = work_dir.join("client-preview-audit.json");
    let mut command =
        client_migration_command_for_db(binary, &state.data, &preview_db, &mapping, &audit);
    let output = run_migration_command("memory-kind preview", &mut command)?;
    if !output.status.success() {
        return Err(AppError::Config(format!(
            "memory-kind preview failed (exit_status={}, {})",
            output.status,
            audit_summary(&audit),
        )));
    }
    preview_from_audit(&audit, &preview_db, total_record_count)
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
    _approval: &MemoryKindMigrationPreview,
) -> AppResult<Option<String>> {
    let toolkit = crate::data::paths::bundled_resource_path(app, "migration-toolkit")
        .ok_or_else(|| AppError::Config("bundled memory-kind toolkit is missing".into()))?;
    let binary = resolve_migration_binary(app)?;
    let (expand_sql, contract_sql) = migration_sql_paths(&toolkit)?;

    apply_expand_sql(db_path, &expand_sql)?;
    marker.phase = MigrationPhase::Expanded;
    write_marker(&state.data, marker)?;

    let mapping = work_dir.join("mapping.json");
    std::fs::write(&mapping, "{}")?;
    let audit = work_dir.join("client-apply-audit.json");
    let mut command = client_migration_command(binary.clone(), &state.data, &mapping, &audit);
    let output = run_migration_command("memory-kind client apply", &mut command)?;
    let stdout_log = work_dir.join("client-apply.stdout.log");
    let stderr_log = work_dir.join("client-apply.stderr.log");
    fs::write(&stdout_log, &output.stdout)?;
    fs::write(&stderr_log, &output.stderr)?;
    if !output.status.success() {
        return Err(AppError::Config(format!(
            "memory-kind client migration did not produce a clean audit \
                 (exit_status={}, {}; stdout={}; stderr={})",
            output.status,
            audit_summary(&audit),
            stdout_log.display(),
            stderr_log.display(),
        )));
    }
    let final_audit = if completed_audit_is_clean(&audit)? {
        audit
    } else {
        prune_unresolved_and_reapply(&state.data, work_dir, binary, &mapping, &audit)?
    };
    debug_assert!(completed_audit_is_clean(&final_audit).unwrap_or(false));
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

/// Deletes the unresolved memory/thread rows confirmed by the migration UI,
/// then re-runs `client-apply` and returns the path of the retry audit, which
/// must itself be clean. Only called when the first `client-apply` audit's
/// only warnings are `unresolved_preflight`.
fn prune_unresolved_and_reapply(
    data: &crate::data::DataPaths,
    work_dir: &Path,
    binary: PathBuf,
    mapping: &Path,
    audit: &Path,
) -> AppResult<PathBuf> {
    ensure_only_unresolved_preflight_warnings(audit)?;
    let dump = work_dir.join("unresolved-records.json");
    let mut command = client_prune_command(binary.clone(), data, audit, &dump);
    let prune = run_migration_command("memory-kind unresolved prune", &mut command)?;
    let (stdout_log, stderr_log) = write_migration_command_logs(
        work_dir,
        "client-prune-unresolved",
        &prune.stdout,
        &prune.stderr,
    )?;
    if !prune.status.success() {
        let stderr_detail = command_stderr_detail(&prune.stderr);
        return Err(AppError::Config(format!(
            "memory-kind unresolved prune failed (exit_status={}; {}; stdout={}; stderr={}; detail={stderr_detail})",
            prune.status,
            audit_summary(audit),
            stdout_log.display(),
            stderr_log.display(),
        )));
    }
    let retry_audit = work_dir.join("client-apply-after-prune-audit.json");
    let mut command = client_migration_command(binary, data, mapping, &retry_audit);
    let retry = run_migration_command("memory-kind client apply after prune", &mut command)?;
    if !retry.status.success() || !completed_audit_is_clean(&retry_audit)? {
        return Err(AppError::Config(format!(
            "memory-kind migration did not produce a clean audit after unresolved prune (exit_status={}, {})",
            retry.status,
            audit_summary(&retry_audit),
        )));
    }
    ignore_migration_dump_reveal_error(reveal_migration_dump(&dump));
    Ok(retry_audit)
}

/// `client-apply` against the live production database.
fn client_migration_command(
    binary: PathBuf,
    data: &crate::data::DataPaths,
    mapping: &Path,
    audit: &Path,
) -> Command {
    client_migration_command_with_url(binary, data, data.memories_sqlite_url(), mapping, audit)
}

/// `client-apply` against an arbitrary SQLite file (the read-only preview
/// copy), never the live production database.
fn client_migration_command_for_db(
    binary: PathBuf,
    data: &crate::data::DataPaths,
    database: &Path,
    mapping: &Path,
    audit: &Path,
) -> Command {
    client_migration_command_with_url(
        binary,
        data,
        format!("sqlite://{}", database.display()),
        mapping,
        audit,
    )
}

fn client_migration_command_with_url(
    binary: PathBuf,
    data: &crate::data::DataPaths,
    sqlite_url: String,
    mapping: &Path,
    audit: &Path,
) -> Command {
    let mut command = sqlite_command(binary, data, sqlite_url);
    command
        .args(["client-apply", "--mapping"])
        .arg(mapping)
        .arg("--output")
        .arg(audit);
    command
}

/// Shared `migrate-memory-kind` invocation setup. A bundled app launched from
/// Finder has `/` as its cwd, so an incomplete SQLite config would otherwise
/// fall back to an unwritable relative `default.sqlite3` path.
fn sqlite_command(binary: PathBuf, data: &crate::data::DataPaths, sqlite_url: String) -> Command {
    let mut command = Command::new(binary);
    command
        .current_dir(&data.root)
        .env("SQLITE_URL", sqlite_url)
        .env(
            "SQLITE_MAX_CONNECTIONS",
            crate::data::DataPaths::MEMORIES_SQLITE_MAX_CONNECTIONS.to_string(),
        );
    command
}

/// Resolve the packaged sidecar using the same multi-layout lookup as the
/// normal sidecars, before considering development-only fallbacks.
fn resolve_migration_binary(app: &AppHandle) -> AppResult<PathBuf> {
    crate::resolve_bin_for_app(
        app,
        "LOOKBACK_MIGRATE_MEMORY_KIND_BIN",
        "migrate-memory-kind",
        "migrate-memory-kind",
        "../../memories/target/release/migrate-memory-kind",
    )
    .map_err(|error| AppError::Config(format!("resolve migrate-memory-kind: {error}")))
}

/// Run a migration child with enough context to distinguish a missing
/// executable from an audit or SQLite failure. `Command::output` otherwise
/// returns only a bare OS error when the process cannot be spawned.
fn run_migration_command(phase: &str, command: &mut Command) -> AppResult<std::process::Output> {
    let program = command.get_program().to_string_lossy().into_owned();
    let current_dir = command
        .get_current_dir()
        .map_or_else(|| "<inherit>".into(), |path| path.display().to_string());
    command.output().map_err(|error| {
        AppError::Config(format!(
            "start {phase} command {} (working directory {current_dir}): {error}",
            program
        ))
    })
}

fn write_migration_command_logs(
    work_dir: &Path,
    command: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> AppResult<(PathBuf, PathBuf)> {
    let stdout_log = work_dir.join(format!("{command}.stdout.log"));
    let stderr_log = work_dir.join(format!("{command}.stderr.log"));
    fs::write(&stdout_log, stdout)?;
    fs::write(&stderr_log, stderr)?;
    Ok((stdout_log, stderr_log))
}

fn command_stderr_detail(stderr: &[u8]) -> String {
    const MAX_CHARS: usize = 4_000;
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        "<empty>".into()
    } else {
        detail.chars().take(MAX_CHARS).collect()
    }
}

fn sqlite_memory_thread_count(path: &Path) -> AppResult<u64> {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| AppError::Config(format!("open SQLite count source: {error}")))?;
    connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM memory) + (SELECT COUNT(*) FROM thread)",
            [],
            |row| row.get(0),
        )
        .map_err(|error| AppError::Config(format!("count migration records: {error}")))
}

fn preview_from_audit(
    path: &Path,
    preview_db: &Path,
    total_record_count: u64,
) -> AppResult<MemoryKindMigrationPreview> {
    let audit: serde_json::Value = serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| AppError::Config(format!("parse client preview audit: {error}")))?;
    ensure_audit_completed_and_clean(
        &audit,
        "client preview audit is not completed and failure-free",
    )?;
    let warnings = audit
        .get("warnings")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| AppError::Config("client preview audit has no warnings array".into()))?;
    // `memory`/`thread` count warnings as reported; `memory_ids`/`thread_ids`
    // dedupe them into concrete delete targets. They diverge if the audit
    // ever repeats a warning for the same id — kept separate rather than
    // reading `unresolved_*_count` off `.len()` so that divergence surfaces
    // in the preview instead of silently collapsing.
    let mut memory = 0;
    let mut thread = 0;
    let mut memory_ids = BTreeSet::new();
    let mut thread_ids = BTreeSet::new();
    for warning in warnings {
        if warning.get("check").and_then(serde_json::Value::as_str) != Some("unresolved_preflight")
        {
            return Err(AppError::Config(
                "client preview contains a warning that cannot be deleted automatically".into(),
            ));
        }
        match warning.get("entity").and_then(serde_json::Value::as_str) {
            Some("memory") => {
                memory += 1;
                memory_ids.insert(preview_warning_id(warning)?);
            }
            Some("thread") => {
                thread += 1;
                thread_ids.insert(preview_warning_id(warning)?);
            }
            _ => {
                return Err(AppError::Config(
                    "client preview has an unsupported unresolved entity".into(),
                ));
            }
        }
    }
    // `client-prune-unresolved` removes memberships for an unresolved thread
    // but deletes memory rows only when the memory itself has a warning.
    // Keep this preview in lockstep with that destructive contract.
    let planned_memory_ids = memory_ids.into_iter().collect::<Vec<_>>();
    let planned_thread_ids = thread_ids.into_iter().collect::<Vec<_>>();
    Ok(MemoryKindMigrationPreview {
        warning_count: warnings.len(),
        total_record_count,
        unresolved_memory_count: memory,
        unresolved_thread_count: thread,
        planned_memory_delete_count: planned_memory_ids.len(),
        planned_thread_delete_count: planned_thread_ids.len(),
        related_deletion_counts: preview_related_deletion_counts(
            preview_db,
            &planned_memory_ids,
            &planned_thread_ids,
        )?,
        planned_memory_ids,
        planned_thread_ids,
        requires_confirmation: !warnings.is_empty(),
    })
}

fn preview_related_deletion_counts(
    database: &Path,
    memory_ids: &[i64],
    thread_ids: &[i64],
) -> AppResult<BTreeMap<String, u64>> {
    if memory_ids.is_empty() && thread_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut uri = Url::from_file_path(database).map_err(|_| {
        AppError::Config(format!(
            "encode SQLite related-delete preview path: {}",
            database.display()
        ))
    })?;
    uri.query_pairs_mut().append_pair("immutable", "1");
    let connection = rusqlite::Connection::open_with_flags(
        uri.as_str(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| AppError::Config(format!("open SQLite related-delete preview: {error}")))?;
    let memory_filter = id_list(memory_ids);
    let thread_filter = id_list(thread_ids);
    let mut counts = BTreeMap::new();
    for (table, predicate) in [
        (
            "thread_memory",
            format!("memory_id IN ({memory_filter}) OR thread_id IN ({thread_filter})"),
        ),
        ("memory_rating", format!("memory_id IN ({memory_filter})")),
        (
            "reflection_failure_mode",
            format!("memory_id IN ({memory_filter})"),
        ),
        ("reflection_tool", format!("memory_id IN ({memory_filter})")),
        (
            "reflection_tool_outcome",
            format!("memory_id IN ({memory_filter})"),
        ),
        ("reflection_fact", format!("memory_id IN ({memory_filter})")),
        (
            "reflection_applied_target",
            format!("memory_id IN ({memory_filter})"),
        ),
        (
            "reflection_few_shot_usage",
            format!("memory_id IN ({memory_filter}) OR used_in_thread_id IN ({thread_filter})"),
        ),
        ("thread_label", format!("thread_id IN ({thread_filter})")),
        (
            "thread_aggregate_key",
            format!("thread_id IN ({thread_filter})"),
        ),
        (
            "thread_reflection_index",
            format!(
                "memory_id IN ({memory_filter}) OR thread_id IN ({thread_filter}) OR origin_thread_id IN ({thread_filter})"
            ),
        ),
    ] {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [table],
                |row| row.get(0),
            )
            .map_err(|error| AppError::Config(format!("inspect preview table {table}: {error}")))?;
        if exists {
            let count: u64 = connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"),
                    [],
                    |row| row.get(0),
                )
                .map_err(|error| {
                    AppError::Config(format!("count preview related rows in {table}: {error}"))
                })?;
            if count > 0 {
                counts.insert(table.into(), count);
            }
        }
    }
    Ok(counts)
}

fn id_list(ids: &[i64]) -> String {
    if ids.is_empty() {
        "NULL".into()
    } else {
        ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
    }
}

fn preview_warning_id(warning: &serde_json::Value) -> AppResult<i64> {
    warning
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| {
            AppError::Config("client preview unresolved warning has no numeric ID".into())
        })
}

/// Both the preview and the post-apply prune gate read the same audit shape
/// and must agree on what counts as "clean" — a schema change here must not
/// drift between the two call sites.
fn ensure_audit_completed_and_clean(audit: &serde_json::Value, message: &str) -> AppResult<()> {
    if audit.get("status").and_then(serde_json::Value::as_str) != Some("completed")
        || audit
            .get("failures")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|items| !items.is_empty())
    {
        return Err(AppError::Config(message.into()));
    }
    Ok(())
}

fn ensure_only_unresolved_preflight_warnings(path: &Path) -> AppResult<()> {
    let audit: serde_json::Value = serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| AppError::Config(format!("parse client migration audit: {error}")))?;
    ensure_audit_completed_and_clean(
        &audit,
        "client migration audit is not eligible for unresolved prune",
    )?;
    let warnings = audit
        .get("warnings")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| AppError::Config("client migration audit has no warnings array".into()))?;
    if warnings.is_empty()
        || warnings.iter().any(|warning| {
            warning.get("check").and_then(serde_json::Value::as_str) != Some("unresolved_preflight")
        })
    {
        return Err(AppError::Config(
            "client migration audit has warnings that cannot be deleted automatically".into(),
        ));
    }
    Ok(())
}

/// Invokes `client-prune-unresolved`, which removes memberships for an
/// unresolved thread but deletes memory rows only when the memory itself has
/// a warning. `preview_from_audit`'s delete-target computation must stay in
/// lockstep with this contract.
fn client_prune_command(
    binary: PathBuf,
    data: &crate::data::DataPaths,
    audit: &Path,
    dump: &Path,
) -> Command {
    let mut command = sqlite_command(binary, data, data.memories_sqlite_url());
    command
        .args(["client-prune-unresolved", "--force", "--audit"])
        .arg(audit)
        .arg("--output")
        .arg(dump);
    command
}

fn reveal_migration_dump(dump: &Path) -> AppResult<()> {
    let (program, argument) = if cfg!(target_os = "macos") {
        ("open", dump.to_path_buf())
    } else if cfg!(target_os = "linux") {
        ("xdg-open", dump.parent().unwrap_or(dump).to_path_buf())
    } else {
        return Ok(());
    };
    let status = Command::new(program)
        .arg(argument)
        .status()
        .map_err(|error| AppError::Config(format!("open migration dump: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::Config(format!(
            "{program} migration dump exited with {status}"
        )))
    }
}

/// Showing the dump is a convenience after the durable migration has
/// succeeded. An unavailable desktop opener must never turn that success
/// into a database rollback.
fn ignore_migration_dump_reveal_error(result: AppResult<()>) {
    if let Err(error) = result {
        eprintln!("memory-kind migration completed, but could not reveal its dump: {error}");
    }
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

    #[test]
    fn dump_reveal_failure_is_ignored_after_a_successful_migration() {
        ignore_migration_dump_reveal_error(Err(AppError::Config("no opener".into())));
    }

    #[test]
    fn live_unresolved_audit_is_eligible_even_if_the_preview_was_different() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("live-audit.json");
        fs::write(
            &audit,
            r#"{"status":"completed","failures":[],"warnings":[{"entity":"memory","id":11,"check":"unresolved_preflight"}]}"#,
        )
        .unwrap();

        assert!(ensure_only_unresolved_preflight_warnings(&audit).is_ok());
    }

    #[test]
    fn live_audit_with_a_non_prunable_warning_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("live-audit.json");
        fs::write(
            &audit,
            r#"{"status":"completed","failures":[],"warnings":[{"entity":"memory","id":11,"check":"unresolved_preflight"},{"entity":"memory","id":12,"check":"unexpected"}]}"#,
        )
        .unwrap();

        assert!(ensure_only_unresolved_preflight_warnings(&audit).is_err());
    }

    #[test]
    fn prune_command_logs_are_persisted_for_recovery() {
        let dir = tempfile::tempdir().unwrap();

        let (stdout, stderr) = write_migration_command_logs(
            dir.path(),
            "client-prune-unresolved",
            b"prune output",
            b"prune failure",
        )
        .unwrap();

        assert_eq!(fs::read(stdout).unwrap(), b"prune output");
        assert_eq!(fs::read(stderr).unwrap(), b"prune failure");
    }

    #[test]
    fn command_stderr_detail_is_bounded_and_preserves_a_nonempty_cause() {
        assert_eq!(command_stderr_detail(b"\n cause \n"), "cause");
        assert_eq!(command_stderr_detail(b"\n"), "<empty>");
        assert_eq!(
            command_stderr_detail(&vec![b'x'; 4_001]).chars().count(),
            4_000
        );
    }

    #[test]
    fn preview_does_not_count_memories_only_reached_through_an_unresolved_thread() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("preview.sqlite3");
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute_batch(
                "CREATE TABLE thread_memory (thread_id INTEGER NOT NULL, memory_id INTEGER NOT NULL); \
                 CREATE TABLE reflection_few_shot_usage (memory_id INTEGER NOT NULL, used_in_thread_id INTEGER NOT NULL); \
                 INSERT INTO thread_memory (thread_id, memory_id) VALUES (7, 10), (7, 11); \
                 INSERT INTO reflection_few_shot_usage (memory_id, used_in_thread_id) VALUES (99, 7);",
            )
            .unwrap();
        let audit = dir.path().join("audit.json");
        fs::write(
            &audit,
            r#"{"status":"completed","failures":[],"warnings":[{"entity":"thread","id":7,"check":"unresolved_preflight"}]}"#,
        )
        .unwrap();

        let preview = preview_from_audit(&audit, &database, 3).unwrap();

        assert_eq!(preview.planned_thread_delete_count, 1);
        assert_eq!(preview.planned_memory_delete_count, 0);
        assert_eq!(
            preview.related_deletion_counts.get("thread_memory"),
            Some(&2)
        );
        assert_eq!(
            preview
                .related_deletion_counts
                .get("reflection_few_shot_usage"),
            Some(&1)
        );
    }

    #[test]
    fn clean_preview_does_not_reopen_the_preview_database() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.json");
        fs::write(
            &audit,
            r#"{"status":"completed","failures":[],"warnings":[]}"#,
        )
        .unwrap();

        let preview = preview_from_audit(&audit, &dir.path().join("missing.sqlite3"), 3).unwrap();

        assert!(!preview.requires_confirmation);
        assert!(preview.related_deletion_counts.is_empty());
    }

    #[test]
    fn client_migration_command_uses_the_complete_sqlite_configuration() {
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path().join("data root"));
        let database = data.memories_sqlite_path();
        let mapping = tmp.path().join("mapping.json");
        let audit = tmp.path().join("audit.json");
        let command = client_migration_command(
            PathBuf::from("/tool/migrate-memory-kind"),
            &data,
            &mapping,
            &audit,
        );

        assert_eq!(command.get_current_dir(), Some(data.root.as_path()));
        let sqlite_url = command
            .get_envs()
            .find_map(|(key, value)| (key == "SQLITE_URL").then_some(value))
            .flatten();
        assert_eq!(
            sqlite_url,
            Some(std::ffi::OsStr::new(&format!(
                "sqlite://{}?mode=rwc",
                database.display()
            )))
        );
        let max_connections = command
            .get_envs()
            .find_map(|(key, value)| (key == "SQLITE_MAX_CONNECTIONS").then_some(value))
            .flatten();
        assert_eq!(max_connections, Some(std::ffi::OsStr::new("5")));
    }

    #[test]
    fn client_prune_command_enables_client_recovery_force_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path().join("data root"));
        let audit = tmp.path().join("audit.json");
        let dump = tmp.path().join("dump.json");
        let command = client_prune_command(
            PathBuf::from("/tool/migrate-memory-kind"),
            &data,
            &audit,
            &dump,
        );
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(arguments.iter().any(|argument| argument == "--force"));
    }

    #[test]
    fn migration_command_failure_describes_the_program_and_working_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path().join("data root"));
        std::fs::create_dir_all(&data.root).unwrap();
        let mapping = tmp.path().join("mapping.json");
        let audit = tmp.path().join("audit.json");
        let mut command = client_migration_command(
            PathBuf::from("/definitely/missing/migrate-memory-kind"),
            &data,
            &mapping,
            &audit,
        );

        let error = run_migration_command("client apply", &mut command).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("client apply"), "{message}");
        assert!(
            message.contains("/definitely/missing/migrate-memory-kind"),
            "{message}"
        );
        assert!(
            message.contains(&data.root.display().to_string()),
            "{message}"
        );
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
