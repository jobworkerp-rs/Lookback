//! Lookback's pre-sidecar adapter for the Memories-owned migration coordinator.
//!
//! This module deliberately does not inspect Memories tables. It only caches a
//! completed release contract and evaluates the coordinator's published status
//! line and process exit status.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use chrono::Utc;
use rusqlite::{Connection, DatabaseName, OpenFlags};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::data::DataPaths;
use crate::error::{AppError, AppResult};

pub const STARTUP_MIGRATION: MigrationRelease = MigrationRelease {
    enabled: true,
    migration_id: "thread-message-times-v1",
    expected_schema_contract: "20260803000003",
};

// Logical SQLite backup and filesystem copies can grow while the migration is
// offline. Reserve 10% of the measured source size, with a 64 MiB floor for
// filesystem metadata, SQLite page rounding, and the immutable journal.
const BACKUP_MIN_FREE_MARGIN_BYTES: u64 = 64 * 1024 * 1024;
const BACKUP_MARGIN_DIVISOR: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationRelease {
    pub enabled: bool,
    pub migration_id: &'static str,
    pub expected_schema_contract: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationReceipt {
    pub format_version: u32,
    pub migration_id: String,
    pub schema_contract: String,
    pub completed_at: String,
    pub attempt_id: String,
}

impl MigrationReceipt {
    pub fn matches(&self, release: &MigrationRelease) -> bool {
        self.format_version == 1
            && self.migration_id == release.migration_id
            && self.schema_contract == release.expected_schema_contract
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    BackupComplete,
    Running,
    Failed,
    RetryRequested,
    Completed,
    RestoreInProgress,
    Restored,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationAttempt {
    pub format_version: u32,
    pub attempt_id: String,
    pub migration_id: String,
    pub backup_dir: PathBuf,
    pub state: AttemptState,
    pub phase: String,
    pub updated_at: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaStatus {
    Uninitialized(usize),
    BaselineRequired,
    Pending(usize),
    Managed,
}

impl SchemaStatus {
    pub fn is_complete(self) -> bool {
        self == Self::Managed
    }
}

pub fn parse_schema_status(output: &str) -> Result<SchemaStatus, String> {
    let line = output
        .lines()
        .find(|line| line.starts_with("schema_status "))
        .ok_or_else(|| "schema_status structured line is missing".to_string())?;
    let mut status = None;
    let mut pending = None;
    for part in line.split_ascii_whitespace().skip(1) {
        if let Some(value) = part.strip_prefix("status=") {
            status = Some(value);
        } else if let Some(value) = part.strip_prefix("pending_count=") {
            pending = Some(value);
        }
    }
    match (status, pending) {
        (Some("uninitialized"), Some(value)) => value
            .parse()
            .map(SchemaStatus::Uninitialized)
            .map_err(|_| "uninitialized pending_count must be numeric".into()),
        (Some("baseline_required"), Some("unknown")) => Ok(SchemaStatus::BaselineRequired),
        (Some("pending"), Some(value)) => value
            .parse()
            .map(SchemaStatus::Pending)
            .map_err(|_| "pending pending_count must be numeric".into()),
        (Some("managed"), Some("0")) => Ok(SchemaStatus::Managed),
        (Some("schema_corrupt"), _) => Err("schema is corrupt".into()),
        (Some(value), _) => Err(format!("unsupported schema_status: {value}")),
        _ => Err("malformed schema_status structured line".into()),
    }
}

pub fn coordinator_plan(initial: SchemaStatus) -> Vec<Vec<&'static str>> {
    let mut commands = Vec::new();
    if initial == SchemaStatus::BaselineRequired {
        commands.push(vec!["schema", "baseline"]);
        commands.extend(preflight_commands());
    }
    commands.extend([
        vec!["schema", "apply"],
        vec!["post-migrate", "run", "--all-required", "--dry-run"],
        vec![
            "post-migrate",
            "run",
            "--all-required",
            "--maintenance-window-ack",
        ],
        vec!["schema", "validate"],
        vec!["schema", "status"],
        vec!["schema", "verify"],
        vec!["post-migrate", "verify"],
    ]);
    commands
}

fn preflight_commands() -> Vec<Vec<&'static str>> {
    vec![
        vec!["schema", "validate"],
        vec!["schema", "status"],
        vec!["schema", "apply", "--dry-run"],
        vec!["post-migrate", "status"],
    ]
}

#[derive(Debug, Clone)]
pub struct MigrationEnvironment {
    pub database_url: String,
    pub thread_vector_enabled: bool,
    pub thread_lancedb_uri: PathBuf,
    pub thread_lancedb_table: String,
    pub thread_vector_size: u32,
    pub memory_fts_tokenizer: String,
    pub lance_language_model_home: PathBuf,
}

struct MigrationLogGuard(PathBuf);

impl MigrationLogGuard {
    fn new(path: PathBuf) -> AppResult<Self> {
        crate::log_rotation::mark_migration_log_active(&path)?;
        Ok(Self(path))
    }
}

impl Drop for MigrationLogGuard {
    fn drop(&mut self) {
        let _ = crate::log_rotation::mark_migration_log_closed(&self.0);
    }
}

pub async fn run_startup_gate(
    data: &DataPaths,
    coordinator: &Path,
    release: MigrationRelease,
    env: &MigrationEnvironment,
) -> AppResult<()> {
    run_startup_gate_with_space_probe(data, coordinator, release, env, &|path| {
        fs2::available_space(path)
    })
    .await
}

async fn run_startup_gate_with_space_probe(
    data: &DataPaths,
    coordinator: &Path,
    release: MigrationRelease,
    env: &MigrationEnvironment,
    available_space: &(dyn Fn(&Path) -> std::io::Result<u64> + Send + Sync),
) -> AppResult<()> {
    if !release.enabled {
        return Ok(());
    }
    let active_attempt = load_attempt_for_startup(data)?;
    // Keep an unfinished generation recoverable until restore plus explicit retry acknowledges it.
    if let Some(attempt) = active_attempt.as_ref().filter(|attempt| {
        attempt.migration_id != release.migration_id
            && !matches!(
                attempt.state,
                AttemptState::RetryRequested | AttemptState::Completed
            )
    }) {
        return Err(AppError::DatabaseMigrationFailed {
            phase: "migration recovery".into(),
            reason: format!(
                "migration attempt {} from release {} requires recovery before starting release {}",
                attempt.attempt_id, attempt.migration_id, release.migration_id
            ),
            backup_path: attempt.backup_dir.display().to_string(),
        });
    }
    if let Some(attempt) = active_attempt.as_ref().filter(|attempt| {
        attempt.migration_id == release.migration_id
            && matches!(
                attempt.state,
                AttemptState::RestoreInProgress | AttemptState::Restored
            )
    }) {
        let (phase, reason) = match attempt.state {
            AttemptState::RestoreInProgress => (
                "restore pending recovery",
                "migration backup restore was interrupted; select restore to complete recovery",
            ),
            AttemptState::Restored => (
                "restore pending retry",
                "migration backup was restored; select migration retry to run the coordinator",
            ),
            _ => unreachable!("restore state was filtered above"),
        };
        return Err(AppError::DatabaseMigrationFailed {
            phase: phase.into(),
            reason: reason.into(),
            backup_path: attempt.backup_dir.display().to_string(),
        });
    }
    let retry_requested = active_attempt.as_ref().is_some_and(|attempt| {
        attempt.migration_id == release.migration_id
            && attempt.state == AttemptState::RetryRequested
            && attempt.backup_dir.is_dir()
    });
    if !retry_requested
        && load_receipt(&data.database_migration_receipt_path())
            .is_some_and(|receipt| receipt.matches(&release))
    {
        return Ok(());
    }

    let previous_attempt = active_attempt.filter(|attempt| {
        attempt.migration_id == release.migration_id
            && matches!(
                attempt.state,
                AttemptState::Failed | AttemptState::RetryRequested
            )
            && attempt.backup_dir.is_dir()
    });
    let mut attempt = if let Some(mut attempt) = previous_attempt {
        attempt.state = AttemptState::Running;
        attempt.phase = "retry".into();
        attempt.updated_at = Utc::now().to_rfc3339();
        attempt.error = None;
        attempt
    } else {
        let attempt_id = format!(
            "{}-{}",
            Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
            std::process::id()
        );
        let backup_dir = create_backup_with_space_probe(data, &attempt_id, available_space)
            .map_err(|error| AppError::DatabaseMigrationFailed {
                phase: "backup".into(),
                reason: error.to_string(),
                backup_path: String::new(),
            })?;
        MigrationAttempt {
            format_version: 1,
            attempt_id,
            migration_id: release.migration_id.into(),
            backup_dir,
            state: AttemptState::BackupComplete,
            phase: "backup".into(),
            updated_at: Utc::now().to_rfc3339(),
            error: None,
        }
    };
    save_attempt(data, &attempt).map_err(|error| attach_backup_path(error, &attempt.backup_dir))?;
    let immutable_journal = attempt.backup_dir.join("attempt.json");
    if !immutable_journal.exists() {
        atomic_json(&immutable_journal, &attempt)
            .map_err(|error| attach_backup_path(error, &attempt.backup_dir))?;
    }

    let outcome = run_coordinator(coordinator, env, &mut attempt, data).await;
    if let Err(error) = outcome {
        let error = attach_backup_path(error, &attempt.backup_dir);
        attempt.state = AttemptState::Failed;
        attempt.error = Some(error.to_string());
        attempt.updated_at = Utc::now().to_rfc3339();
        let _ = save_attempt(data, &attempt);
        return Err(error);
    }

    let receipt = MigrationReceipt {
        format_version: 1,
        migration_id: release.migration_id.into(),
        schema_contract: release.expected_schema_contract.into(),
        completed_at: Utc::now().to_rfc3339(),
        attempt_id: attempt.attempt_id.clone(),
    };
    if let Err(error) = atomic_json(&data.database_migration_receipt_path(), &receipt) {
        attempt.state = AttemptState::Failed;
        attempt.phase = "receipt".into();
        attempt.error = Some(error.to_string());
        attempt.updated_at = Utc::now().to_rfc3339();
        let _ = save_attempt(data, &attempt);
        return Err(attach_backup_path(error, &attempt.backup_dir));
    }
    attempt.state = AttemptState::Completed;
    attempt.phase = "completed".into();
    attempt.updated_at = Utc::now().to_rfc3339();
    save_attempt(data, &attempt)
}

fn attach_backup_path(error: AppError, backup: &Path) -> AppError {
    match error {
        AppError::DatabaseMigrationFailed { phase, reason, .. } => {
            AppError::DatabaseMigrationFailed {
                phase,
                reason,
                backup_path: backup.display().to_string(),
            }
        }
        other => AppError::DatabaseMigrationFailed {
            phase: "migration".into(),
            reason: other.to_string(),
            backup_path: backup.display().to_string(),
        },
    }
}

async fn run_coordinator(
    coordinator: &Path,
    env: &MigrationEnvironment,
    attempt: &mut MigrationAttempt,
    data: &DataPaths,
) -> AppResult<()> {
    let log_path = data
        .log_dir()
        .join(format!("database-migration-{}.log", attempt.attempt_id));
    let _log_guard = MigrationLogGuard::new(log_path.clone())?;
    let initial = execute_preflight(coordinator, env, attempt, data, &log_path).await?;
    if initial == SchemaStatus::BaselineRequired {
        let args = ["schema", "baseline"];
        run_command(coordinator, env, &args, &log_path).await?;
        update_attempt_phase(attempt, data, &args)?;
        let repeated = execute_preflight(coordinator, env, attempt, data, &log_path).await?;
        if repeated == SchemaStatus::BaselineRequired {
            return Err(migration_error(
                &args,
                "schema still requires baseline after successful baseline".into(),
                "",
            ));
        }
    }
    for args in coordinator_plan(SchemaStatus::Managed) {
        let output = run_command(coordinator, env, &args, &log_path).await?;
        update_attempt_phase(attempt, data, &args)?;
        if args == ["schema", "status"] {
            let status = parse_schema_status(&output.stdout)
                .map_err(|reason| migration_error(&args, reason, &output.stderr))?;
            if !status.is_complete() {
                return Err(migration_error(
                    &args,
                    "final schema status is not managed/pending_count=0".into(),
                    &output.stderr,
                ));
            }
        }
    }
    Ok(())
}

async fn execute_preflight(
    coordinator: &Path,
    env: &MigrationEnvironment,
    attempt: &mut MigrationAttempt,
    data: &DataPaths,
    log_path: &Path,
) -> AppResult<SchemaStatus> {
    let mut status = None;
    for args in preflight_commands() {
        let output = run_command(coordinator, env, &args, log_path).await?;
        update_attempt_phase(attempt, data, &args)?;
        if args == ["schema", "status"] {
            status = Some(
                parse_schema_status(&output.stdout)
                    .map_err(|reason| migration_error(&args, reason, &output.stderr))?,
            );
        }
    }
    status.ok_or_else(|| AppError::DatabaseMigrationFailed {
        phase: "preflight".into(),
        reason: "schema status command was not executed".into(),
        backup_path: attempt.backup_dir.display().to_string(),
    })
}

struct CoordinatorOutput {
    stdout: String,
    stderr: String,
}

async fn run_command(
    coordinator: &Path,
    env: &MigrationEnvironment,
    args: &[&str],
    log_path: &Path,
) -> AppResult<CoordinatorOutput> {
    append_migration_log(log_path, &format!("phase={} event=start\n", args.join(" ")))?;
    let output = Command::new(coordinator)
        .args(args)
        .current_dir(coordinator.parent().unwrap_or_else(|| Path::new(".")))
        .env("MEMORIES_ATLAS_DATABASE_URL", &env.database_url)
        .env(
            "THREAD_VECTOR_ENABLED",
            env.thread_vector_enabled.to_string(),
        )
        .env("THREAD_LANCEDB_URI", &env.thread_lancedb_uri)
        .env("THREAD_LANCEDB_TABLE", &env.thread_lancedb_table)
        .env("THREAD_VECTOR_SIZE", env.thread_vector_size.to_string())
        .env("MEMORY_FTS_TOKENIZER", &env.memory_fts_tokenizer)
        .env(
            "LANCE_LANGUAGE_MODEL_HOME",
            env.lance_language_model_home.as_os_str(),
        )
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| {
            let _ = append_migration_log(
                log_path,
                &format!(
                    "phase={} event=spawn_failed error={}\n",
                    args.join(" "),
                    error
                ),
            );
            AppError::DatabaseMigrationFailed {
                phase: args.join(" "),
                reason: error.to_string(),
                backup_path: String::new(),
            }
        })?;
    let stdout =
        redact_coordinator_output(&String::from_utf8_lossy(&output.stdout), &env.database_url);
    let stderr =
        redact_coordinator_output(&String::from_utf8_lossy(&output.stderr), &env.database_url);
    append_migration_log(
        log_path,
        &format!(
            "phase={} event=exit status={}\nstdout:\n{}\nstderr:\n{}\n",
            args.join(" "),
            output.status,
            stdout,
            stderr
        ),
    )?;
    if !output.status.success() {
        return Err(migration_error(
            args,
            format!("coordinator exited with {}", output.status),
            &stderr,
        ));
    }
    Ok(CoordinatorOutput { stdout, stderr })
}

fn redact_coordinator_output(output: &str, database_url: &str) -> String {
    if database_url.is_empty() {
        output.to_string()
    } else {
        output.replace(database_url, "<redacted-database-url>")
    }
}

fn append_migration_log(path: &Path, body: &str) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(body.as_bytes())?;
    file.sync_data()?;
    Ok(())
}

fn migration_error(args: &[&str], reason: String, stderr: &str) -> AppError {
    let detail = stderr.lines().take(20).collect::<Vec<_>>().join("\n");
    AppError::DatabaseMigrationFailed {
        phase: args.join(" "),
        reason: if detail.is_empty() {
            reason
        } else {
            format!("{reason}: {detail}")
        },
        backup_path: String::new(),
    }
}

fn update_attempt_phase(
    attempt: &mut MigrationAttempt,
    data: &DataPaths,
    args: &[&str],
) -> AppResult<()> {
    attempt.state = AttemptState::Running;
    attempt.phase = args.join(" ");
    attempt.updated_at = Utc::now().to_rfc3339();
    save_attempt(data, attempt)
}

fn save_attempt(data: &DataPaths, attempt: &MigrationAttempt) -> AppResult<()> {
    atomic_json(&data.database_migration_attempt_path(), attempt)
}

fn load_attempt(data: &DataPaths) -> Option<MigrationAttempt> {
    load_attempt_for_startup(data).ok().flatten()
}

fn load_attempt_for_startup(data: &DataPaths) -> AppResult<Option<MigrationAttempt>> {
    let path = data.database_migration_attempt_path();
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::DatabaseMigrationFailed {
                phase: "attempt journal".into(),
                reason: format!("inspect migration attempt journal: {error}"),
                backup_path: String::new(),
            });
        }
    }
    let raw = fs::read(&path).map_err(|error| AppError::DatabaseMigrationFailed {
        phase: "attempt journal".into(),
        reason: format!("read migration attempt journal: {error}"),
        backup_path: String::new(),
    })?;
    serde_json::from_slice(&raw)
        .map(Some)
        .map_err(|error| AppError::DatabaseMigrationFailed {
            phase: "attempt journal".into(),
            reason: format!("parse migration attempt journal: {error}"),
            backup_path: String::new(),
        })
}

/// Make a user-selected retry the only transition that resumes a restored
/// generation. A normal application launch must remain stopped at that point.
pub fn request_explicit_retry(data: &DataPaths) -> AppResult<()> {
    let Some(mut attempt) = load_attempt(data) else {
        return Ok(());
    };
    if attempt.state == AttemptState::Restored {
        attempt.state = AttemptState::RetryRequested;
        attempt.phase = "retry requested".into();
        attempt.updated_at = Utc::now().to_rfc3339();
        attempt.error = None;
        save_attempt(data, &attempt)?;
        let log_path = data
            .log_dir()
            .join(format!("database-migration-{}.log", attempt.attempt_id));
        let _log_guard = MigrationLogGuard::new(log_path.clone())?;
        append_migration_log(&log_path, "phase=retry requested event=accepted\n")?;
    }
    Ok(())
}

fn load_receipt(path: &Path) -> Option<MigrationReceipt> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn atomic_json(path: &Path, value: &impl Serialize) -> AppResult<()> {
    let parent = path.parent().ok_or_else(|| {
        AppError::Config(format!(
            "atomic JSON path has no parent: {}",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let mut file = File::create(&temp)?;
    serde_json::to_writer_pretty(&mut file, value)
        .map_err(|error| AppError::Config(error.to_string()))?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BackupSourceInventory {
    sqlite_bytes: u64,
    lancedb_bytes: u64,
    receipt_bytes: u64,
}

impl BackupSourceInventory {
    fn total_bytes(self) -> AppResult<u64> {
        self.sqlite_bytes
            .checked_add(self.lancedb_bytes)
            .and_then(|total| total.checked_add(self.receipt_bytes))
            .ok_or_else(|| AppError::Config("backup source size overflow".into()))
    }
}

fn measured_path_bytes(path: &Path) -> AppResult<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(AppError::Config(format!(
                "read backup source metadata {}: {error}",
                path.display()
            )));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(AppError::Config(format!(
            "backup source contains unsupported symlink: {}",
            path.display()
        )));
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    if !metadata.is_dir() {
        return Err(AppError::Config(format!(
            "backup source has unsupported file type: {}",
            path.display()
        )));
    }
    let mut total = 0_u64;
    for entry in fs::read_dir(path).map_err(|error| {
        AppError::Config(format!(
            "read backup source directory {}: {error}",
            path.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            AppError::Config(format!(
                "read backup source entry {}: {error}",
                path.display()
            ))
        })?;
        total = total
            .checked_add(measured_path_bytes(&entry.path())?)
            .ok_or_else(|| AppError::Config("backup source size overflow".into()))?;
    }
    Ok(total)
}

fn backup_source_inventory(data: &DataPaths) -> AppResult<BackupSourceInventory> {
    let sqlite = data.memories_sqlite_path();
    let sqlite_wal = PathBuf::from(format!("{}-wal", sqlite.display()));
    Ok(BackupSourceInventory {
        sqlite_bytes: measured_path_bytes(&sqlite)?
            .checked_add(measured_path_bytes(&sqlite_wal)?)
            .ok_or_else(|| AppError::Config("SQLite backup source size overflow".into()))?,
        lancedb_bytes: measured_path_bytes(&data.lancedb_dir())?,
        receipt_bytes: measured_path_bytes(&data.database_migration_receipt_path())?,
    })
}

fn backup_required_bytes(source_bytes: u64) -> AppResult<u64> {
    let proportional_margin = source_bytes.div_ceil(BACKUP_MARGIN_DIVISOR);
    source_bytes
        .checked_add(proportional_margin.max(BACKUP_MIN_FREE_MARGIN_BYTES))
        .ok_or_else(|| AppError::Config("backup required size overflow".into()))
}

fn validate_backup_capacity(source_bytes: u64, available_bytes: u64) -> AppResult<u64> {
    let required_bytes = backup_required_bytes(source_bytes)?;
    if available_bytes < required_bytes {
        return Err(AppError::Config(format!(
            "insufficient backup space: required_bytes={required_bytes} available_bytes={available_bytes} source_bytes={source_bytes}"
        )));
    }
    Ok(required_bytes)
}

#[cfg(test)]
fn create_backup(data: &DataPaths, attempt_id: &str) -> AppResult<PathBuf> {
    create_backup_with_space_probe(data, attempt_id, &|path| fs2::available_space(path))
}

fn create_backup_with_space_probe(
    data: &DataPaths,
    attempt_id: &str,
    available_space: &(dyn Fn(&Path) -> std::io::Result<u64> + Send + Sync),
) -> AppResult<PathBuf> {
    let source_bytes = backup_source_inventory(data)?.total_bytes()?;
    let backup_parent = data.database_migration_backups_dir();
    let capacity_probe_path = if backup_parent.exists() {
        backup_parent.as_path()
    } else {
        data.root.as_path()
    };
    let available_bytes = available_space(capacity_probe_path).map_err(|error| {
        AppError::Config(format!(
            "read backup destination free space {}: {error}",
            capacity_probe_path.display()
        ))
    })?;
    validate_backup_capacity(source_bytes, available_bytes)?;

    let root = data.database_migration_backups_dir().join(attempt_id);
    fs::create_dir_all(&root)?;
    let source_db = data.memories_sqlite_path();
    if source_db.exists() {
        let destination = root.join("default.sqlite3");
        let source =
            Connection::open_with_flags(&source_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|error| AppError::Config(format!("open SQLite backup source: {error}")))?;
        source
            .backup(DatabaseName::Main, &destination, None)
            .map_err(|error| AppError::Config(format!("SQLite backup failed: {error}")))?;
        sync_file(&destination)?;
    } else {
        File::create(root.join("sqlite.absent"))?.sync_all()?;
    }
    if data.lancedb_dir().exists() {
        copy_tree(&data.lancedb_dir(), &root.join("lancedb"))?;
    } else {
        File::create(root.join("lancedb.absent"))?.sync_all()?;
    }
    let receipt = data.database_migration_receipt_path();
    if receipt.exists() {
        let destination = root.join("database-migration-receipt.json");
        fs::copy(receipt, &destination)?;
        sync_file(&destination)?;
    } else {
        File::create(root.join("receipt.absent"))?.sync_all()?;
    }
    File::open(&root)?.sync_all()?;
    Ok(root)
}

fn copy_tree(source: &Path, destination: &Path) -> AppResult<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
            sync_file(&target)?;
        }
    }
    sync_directory(destination)?;
    Ok(())
}

fn sync_file(path: &Path) -> AppResult<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn sync_directory(path: &Path) -> AppResult<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn sync_existing_tree(path: &Path) -> AppResult<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_file() {
        return sync_file(path);
    }
    if !metadata.is_dir() {
        return Err(AppError::Config(format!(
            "sync restore path has unsupported file type: {}",
            path.display()
        )));
    }
    for entry in fs::read_dir(path)? {
        sync_existing_tree(&entry?.path())?;
    }
    sync_directory(path)
}

fn sync_restored_live_paths(data: &DataPaths) -> AppResult<()> {
    sync_existing_tree(&data.memories_sqlite_path())?;
    sync_existing_tree(&PathBuf::from(format!(
        "{}-wal",
        data.memories_sqlite_path().display()
    )))?;
    sync_existing_tree(&PathBuf::from(format!(
        "{}-shm",
        data.memories_sqlite_path().display()
    )))?;
    sync_existing_tree(&data.lancedb_dir())?;
    sync_existing_tree(&data.database_migration_receipt_path())?;
    sync_directory(data.memories_data_dir().as_path())?;
    sync_directory(data.root.as_path())?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreSwitchStep {
    EvacuateSqlite,
    EvacuateSqliteWal,
    EvacuateSqliteShm,
    EvacuateLanceDb,
    EvacuateReceipt,
    InstallSqlite,
    InstallLanceDb,
    InstallReceipt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreStageStep {
    StageSqlite,
    VerifySqlite,
    CopyLanceDb,
    CopyReceipt,
}

impl RestoreSwitchStep {
    #[cfg(test)]
    const ALL: [Self; 8] = [
        Self::EvacuateSqlite,
        Self::EvacuateSqliteWal,
        Self::EvacuateSqliteShm,
        Self::EvacuateLanceDb,
        Self::EvacuateReceipt,
        Self::InstallSqlite,
        Self::InstallLanceDb,
        Self::InstallReceipt,
    ];
}

struct RestoreSwitchItem {
    live: PathBuf,
    staged: PathBuf,
    rollback: PathBuf,
    evacuate_step: RestoreSwitchStep,
    install_step: Option<RestoreSwitchStep>,
}

fn remove_restore_path(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn rollback_evacuated(items: &[RestoreSwitchItem], count: usize) -> std::io::Result<()> {
    let mut errors = Vec::new();
    for item in items[..count].iter().rev() {
        if item.rollback.exists()
            && let Err(error) = fs::rename(&item.rollback, &item.live)
        {
            errors.push(format!("{}: {error}", item.live.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(std::io::Error::other(errors.join("; ")))
    }
}

fn rollback_installed(items: &[RestoreSwitchItem]) -> std::io::Result<()> {
    let mut errors = Vec::new();
    for item in items {
        if let Err(error) = remove_restore_path(&item.live) {
            errors.push(format!("remove {}: {error}", item.live.display()));
        }
    }
    if let Err(error) = rollback_evacuated(items, items.len()) {
        errors.push(format!("restore originals: {error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(std::io::Error::other(errors.join("; ")))
    }
}

fn transactional_restore_switch(
    items: &[RestoreSwitchItem],
    hook: &mut dyn FnMut(RestoreSwitchStep) -> std::io::Result<()>,
) -> std::io::Result<()> {
    for (evacuated, item) in items.iter().enumerate() {
        let result = if item.live.exists() {
            hook(item.evacuate_step).and_then(|()| fs::rename(&item.live, &item.rollback))
        } else {
            Ok(())
        };
        if let Err(error) = result {
            let rollback = rollback_evacuated(items, evacuated);
            return Err(std::io::Error::other(match rollback {
                Ok(()) => format!("evacuation failed and was rolled back: {error}"),
                Err(rollback_error) => {
                    format!("evacuation failed: {error}; rollback also failed: {rollback_error}")
                }
            }));
        }
    }

    for item in items {
        let result = if item.staged.exists() {
            match item.install_step {
                Some(install_step) => {
                    hook(install_step).and_then(|()| fs::rename(&item.staged, &item.live))
                }
                None => Err(std::io::Error::other(format!(
                    "evacuation-only restore item unexpectedly has staged data: {}",
                    item.staged.display()
                ))),
            }
        } else {
            Ok(())
        };
        if let Err(error) = result {
            let rollback = rollback_installed(items);
            return Err(std::io::Error::other(match rollback {
                Ok(()) => format!("installation failed and was rolled back: {error}"),
                Err(rollback_error) => {
                    format!("installation failed: {error}; rollback also failed: {rollback_error}")
                }
            }));
        }
    }
    Ok(())
}

/// Restore the latest immutable pre-migration generation selected by the user.
pub fn restore_backup(data: &DataPaths, requested_backup: &Path) -> AppResult<PathBuf> {
    restore_backup_with_switch_hook(data, requested_backup, &mut |_| Ok(()))
}

fn immutable_sqlite_uri(path: &Path) -> AppResult<String> {
    let url = url::Url::from_file_path(path).map_err(|()| {
        AppError::Config(format!(
            "build immutable SQLite URI from non-absolute path: {}",
            path.display()
        ))
    })?;
    Ok(format!("{url}?immutable=1"))
}

fn open_immutable_sqlite(path: &Path, operation: &str) -> AppResult<Connection> {
    let uri = immutable_sqlite_uri(path)?;
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| AppError::Config(format!("{operation}: {error}")))
}

fn stage_sqlite_restore(source_path: &Path, staged_path: &Path) -> AppResult<()> {
    let source = open_immutable_sqlite(source_path, "open SQLite restore source")?;
    source
        .backup(DatabaseName::Main, staged_path, None)
        .map_err(|error| AppError::Config(format!("stage SQLite restore: {error}")))
}

fn verify_staged_sqlite_restore(staged_path: &Path) -> AppResult<()> {
    let staged = open_immutable_sqlite(staged_path, "open staged SQLite restore")?;
    let integrity: String = staged
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|error| AppError::Config(format!("verify staged SQLite restore: {error}")))?;
    if integrity != "ok" {
        return Err(AppError::Config(format!(
            "verify staged SQLite restore: integrity_check returned {integrity}"
        )));
    }
    Ok(())
}

fn cleanup_restore_staging(staging: &Path) -> std::io::Result<()> {
    if staging.exists() {
        fs::remove_dir_all(staging)?;
    }
    Ok(())
}

fn record_restore_failure(
    data: &DataPaths,
    attempt: &mut MigrationAttempt,
    phase: &str,
    error: &str,
) {
    attempt.state = AttemptState::Failed;
    attempt.phase = format!("restore {phase}");
    attempt.updated_at = Utc::now().to_rfc3339();
    attempt.error = Some(error.into());
    let _ = save_attempt(data, attempt);
    let _ = append_migration_log(
        &data
            .log_dir()
            .join(format!("database-migration-{}.log", attempt.attempt_id)),
        &format!("phase=restore {phase} event=failed error={error}\n"),
    );
}

fn record_restore_incomplete(
    data: &DataPaths,
    attempt: &mut MigrationAttempt,
    phase: &str,
    error: &str,
) {
    debug_assert_eq!(attempt.state, AttemptState::RestoreInProgress);
    attempt.phase = format!("restore {phase}");
    attempt.updated_at = Utc::now().to_rfc3339();
    attempt.error = Some(error.into());
    let _ = save_attempt(data, attempt);
    let _ = append_migration_log(
        &data
            .log_dir()
            .join(format!("database-migration-{}.log", attempt.attempt_id)),
        &format!("phase=restore {phase} event=incomplete error={error}\n"),
    );
}

fn restore_stage_error(
    data: &DataPaths,
    attempt: &mut MigrationAttempt,
    staging: &Path,
    error: AppError,
) -> AppError {
    let mut detail = error.to_string();
    if let Err(cleanup_error) = cleanup_restore_staging(staging) {
        detail.push_str(&format!("; staging cleanup failed: {cleanup_error}"));
    }
    let failure = format!("restore stage failed: {detail}");
    record_restore_failure(data, attempt, "stage", &failure);
    AppError::Config(failure)
}

fn restore_backup_with_switch_hook(
    data: &DataPaths,
    requested_backup: &Path,
    hook: &mut dyn FnMut(RestoreSwitchStep) -> std::io::Result<()>,
) -> AppResult<PathBuf> {
    restore_backup_with_hooks(
        data,
        requested_backup,
        &mut |_| Ok(()),
        &mut || Ok(()),
        hook,
        &mut || Ok(()),
    )
}

fn restore_backup_with_hooks(
    data: &DataPaths,
    requested_backup: &Path,
    stage_hook: &mut dyn FnMut(RestoreStageStep) -> std::io::Result<()>,
    before_switch_hook: &mut dyn FnMut() -> std::io::Result<()>,
    switch_hook: &mut dyn FnMut(RestoreSwitchStep) -> std::io::Result<()>,
    cleanup_hook: &mut dyn FnMut() -> std::io::Result<()>,
) -> AppResult<PathBuf> {
    let raw = fs::read(data.database_migration_attempt_path())?;
    let mut attempt: MigrationAttempt = serde_json::from_slice(&raw)
        .map_err(|error| AppError::Config(format!("invalid migration attempt journal: {error}")))?;
    let log_path = data
        .log_dir()
        .join(format!("database-migration-{}.log", attempt.attempt_id));
    let _log_guard = MigrationLogGuard::new(log_path)?;
    let backup = attempt.backup_dir.clone();
    if backup != requested_backup {
        return Err(AppError::Config(format!(
            "requested backup does not match the active migration attempt: {}",
            requested_backup.display()
        )));
    }
    if !backup.is_dir() {
        return Err(AppError::Config(format!(
            "migration backup is missing: {}",
            backup.display()
        )));
    }

    let sqlite_valid =
        backup.join("default.sqlite3").is_file() || backup.join("sqlite.absent").is_file();
    let lance_valid = backup.join("lancedb").is_dir() || backup.join("lancedb.absent").is_file();
    let receipt_valid = backup.join("database-migration-receipt.json").is_file()
        || backup.join("receipt.absent").is_file();
    let journal_valid = fs::read(backup.join("attempt.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice::<MigrationAttempt>(&raw).ok())
        .is_some_and(|saved| {
            saved.attempt_id == attempt.attempt_id && saved.backup_dir == attempt.backup_dir
        });
    if !(sqlite_valid && lance_valid && receipt_valid && journal_valid) {
        return Err(AppError::Config(
            "migration backup generation is incomplete; no restore was applied".into(),
        ));
    }

    let staging = data
        .root
        .join(format!(".migration-restore-{}", attempt.attempt_id));
    let staged_db = staging.join("default.sqlite3");
    let db_backup = backup.join("default.sqlite3");
    let staged_lance = staging.join("lancedb");
    let lance_backup = backup.join("lancedb");
    let staged_receipt = staging.join("database-migration-receipt.json");
    let receipt_backup = backup.join("database-migration-receipt.json");
    let stage = (|| -> AppResult<()> {
        cleanup_restore_staging(&staging)?;
        fs::create_dir_all(&staging)?;
        if db_backup.is_file() {
            stage_sqlite_restore(&db_backup, &staged_db)?;
            sync_file(&staged_db)?;
            stage_hook(RestoreStageStep::StageSqlite)?;
            verify_staged_sqlite_restore(&staged_db)?;
            stage_hook(RestoreStageStep::VerifySqlite)?;
        }
        if lance_backup.is_dir() {
            stage_hook(RestoreStageStep::CopyLanceDb)?;
            copy_tree(&lance_backup, &staged_lance)?;
        }
        if receipt_backup.is_file() {
            stage_hook(RestoreStageStep::CopyReceipt)?;
            fs::copy(&receipt_backup, &staged_receipt)?;
            sync_file(&staged_receipt)?;
        }
        sync_existing_tree(&staging)?;
        Ok(())
    })();
    if let Err(error) = stage {
        return Err(restore_stage_error(data, &mut attempt, &staging, error));
    }

    // Persist the fail-closed state before any live path is moved. A crash
    // after this point must block a later ordinary launch from rerunning the
    // coordinator against an unknown restore boundary.
    attempt.state = AttemptState::RestoreInProgress;
    attempt.phase = "restore switch".into();
    attempt.updated_at = Utc::now().to_rfc3339();
    attempt.error = None;
    if let Err(error) = save_attempt(data, &attempt) {
        let failure = format!("prepare restore switch: {error}");
        let failure = if let Err(cleanup_error) = cleanup_restore_staging(&staging) {
            format!("{failure}; staging cleanup failed: {cleanup_error}")
        } else {
            failure
        };
        let _ = append_migration_log(
            &data
                .log_dir()
                .join(format!("database-migration-{}.log", attempt.attempt_id)),
            &format!("phase=restore prepare event=failed error={failure}\n"),
        );
        return Err(AppError::Config(failure));
    }
    if let Err(error) = before_switch_hook() {
        let failure = format!("restore switch interrupted after durable prepare: {error}");
        let failure = if let Err(cleanup_error) = cleanup_restore_staging(&staging) {
            format!("{failure}; staging cleanup failed: {cleanup_error}")
        } else {
            failure
        };
        record_restore_incomplete(data, &mut attempt, "switch", &failure);
        return Err(AppError::Config(failure));
    }

    if let Err(error) = fs::create_dir_all(data.memories_data_dir()) {
        let failure = format!("prepare restore live directory: {error}");
        record_restore_incomplete(data, &mut attempt, "prepare", &failure);
        return Err(AppError::Config(failure));
    }
    let rollback = staging.join("rollback");
    if let Err(error) = fs::create_dir_all(&rollback) {
        let failure = format!("prepare restore rollback directory: {error}");
        record_restore_incomplete(data, &mut attempt, "prepare", &failure);
        return Err(AppError::Config(failure));
    }
    let live_db = data.memories_sqlite_path();
    let live_db_wal = PathBuf::from(format!("{}-wal", live_db.display()));
    let live_db_shm = PathBuf::from(format!("{}-shm", live_db.display()));
    let live_lance = data.lancedb_dir();
    let live_receipt = data.database_migration_receipt_path();
    let rollback_db = rollback.join("default.sqlite3");
    let rollback_db_wal = rollback.join("default.sqlite3-wal");
    let rollback_db_shm = rollback.join("default.sqlite3-shm");
    let rollback_lance = rollback.join("lancedb");
    let rollback_receipt = rollback.join("database-migration-receipt.json");
    let items = [
        RestoreSwitchItem {
            live: live_db,
            staged: staged_db,
            rollback: rollback_db,
            evacuate_step: RestoreSwitchStep::EvacuateSqlite,
            install_step: Some(RestoreSwitchStep::InstallSqlite),
        },
        RestoreSwitchItem {
            live: live_db_wal,
            staged: staging.join("default.sqlite3-wal"),
            rollback: rollback_db_wal,
            evacuate_step: RestoreSwitchStep::EvacuateSqliteWal,
            install_step: None,
        },
        RestoreSwitchItem {
            live: live_db_shm,
            staged: staging.join("default.sqlite3-shm"),
            rollback: rollback_db_shm,
            evacuate_step: RestoreSwitchStep::EvacuateSqliteShm,
            install_step: None,
        },
        RestoreSwitchItem {
            live: live_lance,
            staged: staged_lance,
            rollback: rollback_lance,
            evacuate_step: RestoreSwitchStep::EvacuateLanceDb,
            install_step: Some(RestoreSwitchStep::InstallLanceDb),
        },
        RestoreSwitchItem {
            live: live_receipt,
            staged: staged_receipt,
            rollback: rollback_receipt,
            evacuate_step: RestoreSwitchStep::EvacuateReceipt,
            install_step: Some(RestoreSwitchStep::InstallReceipt),
        },
    ];
    if let Err(error) = transactional_restore_switch(&items, switch_hook) {
        let failure = format!("restore switch failed: {error}");
        record_restore_incomplete(data, &mut attempt, "switch", &failure);
        return Err(AppError::Config(failure));
    }
    if let Err(error) = sync_restored_live_paths(data) {
        let failure = format!("sync restored live paths: {error}");
        record_restore_incomplete(data, &mut attempt, "sync", &failure);
        return Err(AppError::Config(failure));
    }
    attempt.state = AttemptState::Restored;
    attempt.phase = "restored".into();
    attempt.updated_at = Utc::now().to_rfc3339();
    attempt.error = None;
    if let Err(error) = save_attempt(data, &attempt) {
        attempt.state = AttemptState::RestoreInProgress;
        let failure = format!("record restored state: {error}");
        record_restore_incomplete(data, &mut attempt, "record", &failure);
        return Err(AppError::Config(failure));
    }
    if let Err(error) = cleanup_hook().and_then(|()| cleanup_restore_staging(&staging)) {
        let failure = format!("restore cleanup failed: {error}");
        let _ = append_migration_log(
            &data
                .log_dir()
                .join(format!("database-migration-{}.log", attempt.attempt_id)),
            &format!("phase=restore cleanup event=failed error={failure}\n"),
        );
        return Err(AppError::Config(failure));
    }
    if let Err(error) = sync_directory(data.root.as_path()) {
        let failure = format!("sync restore cleanup: {error}");
        let _ = append_migration_log(
            &data
                .log_dir()
                .join(format!("database-migration-{}.log", attempt.attempt_id)),
            &format!("phase=restore sync event=failed error={failure}\n"),
        );
        return Err(AppError::Config(failure));
    }
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restore_fixture() -> (tempfile::TempDir, DataPaths, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        {
            let db = Connection::open(data.memories_sqlite_path()).unwrap();
            db.execute("CREATE TABLE sample(value TEXT)", []).unwrap();
            db.execute("INSERT INTO sample VALUES ('before')", [])
                .unwrap();
        }
        fs::write(data.lancedb_dir().join("part"), b"before-vector").unwrap();
        fs::write(data.database_migration_receipt_path(), b"before-receipt").unwrap();
        let backup = create_backup(&data, "restore-attempt").unwrap();
        let attempt = MigrationAttempt {
            format_version: 1,
            attempt_id: "restore-attempt".into(),
            migration_id: STARTUP_MIGRATION.migration_id.into(),
            backup_dir: backup.clone(),
            state: AttemptState::Failed,
            phase: "schema apply".into(),
            updated_at: "now".into(),
            error: Some("failed".into()),
        };
        save_attempt(&data, &attempt).unwrap();
        atomic_json(&backup.join("attempt.json"), &attempt).unwrap();
        {
            let db = Connection::open(data.memories_sqlite_path()).unwrap();
            db.execute("UPDATE sample SET value='after'", []).unwrap();
        }
        fs::write(
            PathBuf::from(format!("{}-wal", data.memories_sqlite_path().display())),
            b"after-wal",
        )
        .unwrap();
        fs::write(
            PathBuf::from(format!("{}-shm", data.memories_sqlite_path().display())),
            b"after-shm",
        )
        .unwrap();
        fs::write(data.lancedb_dir().join("part"), b"after-vector").unwrap();
        fs::write(data.database_migration_receipt_path(), b"after-receipt").unwrap();
        (dir, data, backup)
    }

    fn assert_live_restore_sources_unchanged(data: &DataPaths) {
        assert_eq!(
            fs::read(PathBuf::from(format!(
                "{}-wal",
                data.memories_sqlite_path().display()
            )))
            .unwrap(),
            b"after-wal"
        );
        assert_eq!(
            fs::read(PathBuf::from(format!(
                "{}-shm",
                data.memories_sqlite_path().display()
            )))
            .unwrap(),
            b"after-shm"
        );
        let db = Connection::open(data.memories_sqlite_path()).unwrap();
        let value: String = db
            .query_row("SELECT value FROM sample", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "after");
        assert_eq!(
            fs::read(data.lancedb_dir().join("part")).unwrap(),
            b"after-vector"
        );
        assert_eq!(
            fs::read(data.database_migration_receipt_path()).unwrap(),
            b"after-receipt"
        );
    }

    #[test]
    fn matching_receipt_skips_only_the_same_release_contract() {
        let receipt = MigrationReceipt {
            format_version: 1,
            migration_id: "thread-message-times-v1".into(),
            schema_contract: "20260803000003".into(),
            completed_at: "2026-08-04T00:00:00Z".into(),
            attempt_id: "attempt-1".into(),
        };
        assert!(receipt.matches(&STARTUP_MIGRATION));
        let other = MigrationRelease {
            expected_schema_contract: "20260803000004",
            ..STARTUP_MIGRATION
        };
        assert!(!receipt.matches(&other));
    }

    #[tokio::test]
    async fn disabled_release_does_not_resolve_or_execute_coordinator() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path());
        let env = MigrationEnvironment {
            database_url: data.memories_sqlite_url(),
            thread_vector_enabled: false,
            thread_lancedb_uri: data.lancedb_dir(),
            thread_lancedb_table: "threads".into(),
            thread_vector_size: 4,
            memory_fts_tokenizer: "simple".into(),
            lance_language_model_home: data.lance_language_model_home(),
        };
        run_startup_gate(
            &data,
            Path::new("/definitely/missing/coordinator"),
            MigrationRelease {
                enabled: false,
                ..STARTUP_MIGRATION
            },
            &env,
        )
        .await
        .unwrap();
        assert!(!data.database_migration_attempt_path().exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn corrupt_attempt_journal_blocks_startup_before_coordinator_execution() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        fs::write(data.database_migration_attempt_path(), b"not json").unwrap();
        let coordinator_log = dir.path().join("coordinator.log");
        let coordinator = dir.path().join("memories-db-migrate");
        fs::write(
            &coordinator,
            format!("#!/bin/sh\ntouch '{}'\n", coordinator_log.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&coordinator).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&coordinator, permissions).unwrap();
        let env = MigrationEnvironment {
            database_url: data.memories_sqlite_url(),
            thread_vector_enabled: false,
            thread_lancedb_uri: data.lancedb_dir(),
            thread_lancedb_table: "threads".into(),
            thread_vector_size: 4,
            memory_fts_tokenizer: "simple".into(),
            lance_language_model_home: data.lance_language_model_home(),
        };

        let error = run_startup_gate(&data, &coordinator, STARTUP_MIGRATION, &env)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AppError::DatabaseMigrationFailed { ref phase, .. } if phase == "attempt journal"
        ));
        assert!(!coordinator_log.exists());

        fs::remove_file(data.database_migration_attempt_path()).unwrap();
        fs::create_dir(data.database_migration_attempt_path()).unwrap();
        let error = run_startup_gate(&data, &coordinator, STARTUP_MIGRATION, &env)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AppError::DatabaseMigrationFailed { ref phase, .. } if phase == "attempt journal"
        ));
        assert!(!coordinator_log.exists());

        fs::remove_dir(data.database_migration_attempt_path()).unwrap();
        std::os::unix::fs::symlink(
            data.root.join("missing-attempt-journal"),
            data.database_migration_attempt_path(),
        )
        .unwrap();
        let error = run_startup_gate(&data, &coordinator, STARTUP_MIGRATION, &env)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AppError::DatabaseMigrationFailed { ref phase, .. } if phase == "attempt journal"
        ));
        assert!(!coordinator_log.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_attempt_from_another_release_blocks_startup_without_overwriting_journal() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        let backup = create_backup(&data, "old-release-attempt").unwrap();
        let old_attempt = MigrationAttempt {
            format_version: 1,
            attempt_id: "old-release-attempt".into(),
            migration_id: "old-release".into(),
            backup_dir: backup.clone(),
            state: AttemptState::Failed,
            phase: "schema apply".into(),
            updated_at: "now".into(),
            error: Some("old migration failed".into()),
        };
        save_attempt(&data, &old_attempt).unwrap();
        let coordinator_log = dir.path().join("coordinator.log");
        let coordinator = dir.path().join("memories-db-migrate");
        fs::write(
            &coordinator,
            format!("#!/bin/sh\ntouch '{}'\n", coordinator_log.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&coordinator).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&coordinator, permissions).unwrap();
        let env = MigrationEnvironment {
            database_url: data.memories_sqlite_url(),
            thread_vector_enabled: false,
            thread_lancedb_uri: data.lancedb_dir(),
            thread_lancedb_table: "threads".into(),
            thread_vector_size: 4,
            memory_fts_tokenizer: "simple".into(),
            lance_language_model_home: data.lance_language_model_home(),
        };
        let release = MigrationRelease {
            migration_id: "new-release",
            expected_schema_contract: "new-contract",
            ..STARTUP_MIGRATION
        };

        let error = run_startup_gate(&data, &coordinator, release, &env)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AppError::DatabaseMigrationFailed { ref phase, .. } if phase == "migration recovery"
        ));
        assert!(!coordinator_log.exists());
        assert_eq!(load_attempt(&data).unwrap(), old_attempt);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restored_failed_attempt_from_another_release_allows_new_generation_after_explicit_retry()
     {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, data, backup) = restore_fixture();
        let env = MigrationEnvironment {
            database_url: data.memories_sqlite_url(),
            thread_vector_enabled: false,
            thread_lancedb_uri: data.lancedb_dir(),
            thread_lancedb_table: "threads".into(),
            thread_vector_size: 4,
            memory_fts_tokenizer: "simple".into(),
            lance_language_model_home: data.lance_language_model_home(),
        };
        let release = MigrationRelease {
            migration_id: "new-release",
            expected_schema_contract: "new-contract",
            ..STARTUP_MIGRATION
        };

        let blocked = run_startup_gate(
            &data,
            Path::new("/definitely/missing/coordinator"),
            release,
            &env,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            blocked,
            AppError::DatabaseMigrationFailed { ref phase, .. } if phase == "migration recovery"
        ));
        assert_eq!(load_attempt(&data).unwrap().backup_dir, backup);

        restore_backup(&data, &backup).unwrap();
        request_explicit_retry(&data).unwrap();
        assert_eq!(
            load_attempt(&data).unwrap().state,
            AttemptState::RetryRequested
        );

        let coordinator = data.root.join("memories-db-migrate");
        fs::write(
            &coordinator,
            "#!/bin/sh\nif [ \"$1 $2\" = 'schema status' ]; then echo 'schema_status status=managed pending_count=0'; fi\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&coordinator).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&coordinator, permissions).unwrap();

        run_startup_gate(&data, &coordinator, release, &env)
            .await
            .unwrap();

        let current = load_attempt(&data).unwrap();
        assert_eq!(current.migration_id, release.migration_id);
        assert_eq!(current.state, AttemptState::Completed);
        assert_ne!(current.attempt_id, "restore-attempt");
        assert!(backup.exists());
        assert_eq!(
            fs::read_dir(data.database_migration_backups_dir())
                .unwrap()
                .count(),
            2
        );
        assert_eq!(
            load_receipt(&data.database_migration_receipt_path())
                .unwrap()
                .migration_id,
            release.migration_id
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_progress_attempt_from_another_release_blocks_startup_until_backup_restore() {
        for state in [AttemptState::BackupComplete, AttemptState::Running] {
            let (_dir, data, backup) = restore_fixture();
            let mut attempt = load_attempt(&data).unwrap();
            attempt.state = state.clone();
            attempt.phase = "migration phase".into();
            save_attempt(&data, &attempt).unwrap();
            atomic_json(&backup.join("attempt.json"), &attempt).unwrap();
            let env = MigrationEnvironment {
                database_url: data.memories_sqlite_url(),
                thread_vector_enabled: false,
                thread_lancedb_uri: data.lancedb_dir(),
                thread_lancedb_table: "threads".into(),
                thread_vector_size: 4,
                memory_fts_tokenizer: "simple".into(),
                lance_language_model_home: data.lance_language_model_home(),
            };
            let release = MigrationRelease {
                migration_id: "new-release",
                expected_schema_contract: "new-contract",
                ..STARTUP_MIGRATION
            };

            let error = run_startup_gate(
                &data,
                Path::new("/definitely/missing/coordinator"),
                release,
                &env,
            )
            .await
            .unwrap_err();
            assert!(matches!(
                error,
                AppError::DatabaseMigrationFailed { ref phase, .. } if phase == "migration recovery"
            ));
            assert_eq!(load_attempt(&data).unwrap().state, state);
            assert_eq!(load_attempt(&data).unwrap().backup_dir, backup);

            restore_backup(&data, &backup).unwrap();
            assert_eq!(load_attempt(&data).unwrap().state, AttemptState::Restored);
        }
    }

    #[test]
    fn schema_status_rejects_unknown_pending_count() {
        assert_eq!(
            parse_schema_status("schema_status status=baseline_required pending_count=unknown\n"),
            Ok(SchemaStatus::BaselineRequired)
        );
        assert!(
            !parse_schema_status("schema_status status=baseline_required pending_count=unknown\n")
                .unwrap()
                .is_complete()
        );
        assert!(parse_schema_status("noise\n").is_err());
        assert!(
            parse_schema_status("schema_status status=managed pending_count=unknown\n").is_err()
        );
        assert!(
            parse_schema_status("schema_status status=schema_corrupt pending_count=unknown\n")
                .is_err()
        );
    }

    #[test]
    fn coordinator_plan_repeats_preflight_after_baseline() {
        let commands = coordinator_plan(SchemaStatus::BaselineRequired);
        assert_eq!(commands[0], vec!["schema", "baseline"]);
        assert_eq!(commands[1], vec!["schema", "validate"]);
        assert_eq!(commands.last().unwrap(), &vec!["post-migrate", "verify"]);
    }

    #[test]
    fn receipt_write_is_atomic_and_corruption_is_not_a_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("receipt.json");
        fs::write(&path, b"{").unwrap();
        assert!(load_receipt(&path).is_none());
        let receipt = MigrationReceipt {
            format_version: 1,
            migration_id: STARTUP_MIGRATION.migration_id.into(),
            schema_contract: STARTUP_MIGRATION.expected_schema_contract.into(),
            completed_at: "now".into(),
            attempt_id: "a".into(),
        };
        atomic_json(&path, &receipt).unwrap();
        assert_eq!(load_receipt(&path), Some(receipt));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn backup_captures_committed_wal_and_companion_state() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        let db = Connection::open(data.memories_sqlite_path()).unwrap();
        db.pragma_update(None, "journal_mode", "WAL").unwrap();
        db.execute("CREATE TABLE sample(value TEXT)", []).unwrap();
        db.execute("INSERT INTO sample VALUES ('committed')", [])
            .unwrap();
        fs::write(data.lancedb_dir().join("part"), b"vector").unwrap();
        fs::write(data.database_migration_receipt_path(), b"old").unwrap();
        let backup = create_backup(&data, "attempt").unwrap();
        let copied = Connection::open(backup.join("default.sqlite3")).unwrap();
        let value: String = copied
            .query_row("SELECT value FROM sample", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "committed");
        assert_eq!(fs::read(backup.join("lancedb/part")).unwrap(), b"vector");
        assert_eq!(
            fs::read(backup.join("database-migration-receipt.json")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn backup_capacity_boundary_requires_source_bytes_and_documented_margin() {
        let source_bytes = 1_000;
        let required = backup_required_bytes(source_bytes).unwrap();
        assert_eq!(required, source_bytes + BACKUP_MIN_FREE_MARGIN_BYTES);
        assert!(validate_backup_capacity(source_bytes, required).is_ok());
        assert!(validate_backup_capacity(source_bytes, required - 1).is_err());

        let large = BACKUP_MIN_FREE_MARGIN_BYTES * 20;
        assert_eq!(
            backup_required_bytes(large).unwrap(),
            large + large.div_ceil(10)
        );
    }

    #[test]
    fn backup_inventory_counts_sqlite_wal_lancedb_and_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        fs::write(data.memories_sqlite_path(), vec![0_u8; 11]).unwrap();
        fs::write(
            data.memories_sqlite_path().with_extension("sqlite3-wal"),
            vec![0_u8; 13],
        )
        .unwrap();
        fs::create_dir_all(data.lancedb_dir().join("nested")).unwrap();
        fs::write(data.lancedb_dir().join("a"), vec![0_u8; 17]).unwrap();
        fs::write(data.lancedb_dir().join("nested/b"), vec![0_u8; 19]).unwrap();
        fs::write(data.database_migration_receipt_path(), vec![0_u8; 23]).unwrap();

        let inventory = backup_source_inventory(&data).unwrap();
        assert_eq!(inventory.sqlite_bytes, 24);
        assert_eq!(inventory.lancedb_bytes, 36);
        assert_eq!(inventory.receipt_bytes, 23);
        assert_eq!(inventory.total_bytes().unwrap(), 83);
    }

    #[cfg(unix)]
    #[test]
    fn backup_inventory_fails_on_unreadable_source_metadata() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        symlink(
            dir.path().join("missing-target"),
            data.lancedb_dir().join("dangling"),
        )
        .unwrap();
        assert!(backup_source_inventory(&data).is_err());
    }

    #[test]
    fn insufficient_capacity_creates_no_backup_and_does_not_change_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        fs::write(data.memories_sqlite_path(), b"db").unwrap();
        fs::write(data.lancedb_dir().join("part"), b"vector").unwrap();
        fs::write(data.database_migration_receipt_path(), b"receipt-before").unwrap();

        let error = create_backup_with_space_probe(&data, "no-space", &|_| Ok(0)).unwrap_err();
        assert!(error.to_string().contains("insufficient backup space"));
        assert!(
            !data
                .database_migration_backups_dir()
                .join("no-space")
                .exists()
        );
        assert_eq!(
            fs::read(data.database_migration_receipt_path()).unwrap(),
            b"receipt-before"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn insufficient_capacity_stops_before_backup_and_coordinator_spawn() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        fs::write(data.memories_sqlite_path(), b"db").unwrap();
        fs::write(data.database_migration_receipt_path(), b"receipt-before").unwrap();
        let coordinator_log = dir.path().join("coordinator.log");
        let coordinator = dir.path().join("memories-db-migrate");
        fs::write(
            &coordinator,
            format!(
                "#!/bin/sh\necho called >> '{}'\n",
                coordinator_log.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&coordinator).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&coordinator, permissions).unwrap();
        let env = MigrationEnvironment {
            database_url: data.memories_sqlite_url(),
            thread_vector_enabled: false,
            thread_lancedb_uri: data.lancedb_dir(),
            thread_lancedb_table: "threads".into(),
            thread_vector_size: 4,
            memory_fts_tokenizer: "simple".into(),
            lance_language_model_home: data.lance_language_model_home(),
        };

        let error = run_startup_gate_with_space_probe(
            &data,
            &coordinator,
            STARTUP_MIGRATION,
            &env,
            &|_| Ok(0),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            AppError::DatabaseMigrationFailed { ref phase, ref backup_path, .. }
                if phase == "backup" && backup_path.is_empty()
        ));
        assert!(!coordinator_log.exists());
        assert!(!data.database_migration_backups_dir().exists());
        assert_eq!(
            fs::read(data.database_migration_receipt_path()).unwrap(),
            b"receipt-before"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_coordinator_pins_full_order_and_receipt_skip() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        let log = dir.path().join("commands.log");
        let environment_log = dir.path().join("environment.log");
        let script = dir.path().join("memories-db-migrate");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf 'url=%s tokenizer=%s home=%s\\n' \"$MEMORIES_ATLAS_DATABASE_URL\" \"$MEMORY_FTS_TOKENIZER\" \"$LANCE_LANGUAGE_MODEL_HOME\" >> '{}'\necho \"target=$MEMORIES_ATLAS_DATABASE_URL\" >&2\nif [ \"$1 $2\" = 'schema status' ]; then echo 'schema_status status=managed pending_count=0'; fi\n",
                log.display(),
                environment_log.display(),
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let env = MigrationEnvironment {
            database_url: data.memories_sqlite_url(),
            thread_vector_enabled: false,
            thread_lancedb_uri: data.lancedb_dir(),
            thread_lancedb_table: "threads".into(),
            thread_vector_size: 4,
            memory_fts_tokenizer: "simple".into(),
            lance_language_model_home: dir.path().join("辞書 # with spaces"),
        };
        assert!(env.database_url.starts_with("sqlite:///"));
        assert!(!env.database_url.starts_with("sqlite://file:"));
        run_startup_gate(&data, &script, STARTUP_MIGRATION, &env)
            .await
            .unwrap();
        let expected = [
            "schema validate",
            "schema status",
            "schema apply --dry-run",
            "post-migrate status",
            "schema apply",
            "post-migrate run --all-required --dry-run",
            "post-migrate run --all-required --maintenance-window-ack",
            "schema validate",
            "schema status",
            "schema verify",
            "post-migrate verify",
        ]
        .join("\n")
            + "\n";
        assert_eq!(fs::read_to_string(&log).unwrap(), expected);
        assert_eq!(
            fs::read_to_string(&environment_log).unwrap(),
            format!(
                "url={} tokenizer=simple home={}\n",
                env.database_url,
                env.lance_language_model_home.display()
            )
            .repeat(11)
        );
        let audit_log = data.log_dir().join(format!(
            "database-migration-{}.log",
            load_receipt(&data.database_migration_receipt_path())
                .unwrap()
                .attempt_id
        ));
        let audit = fs::read_to_string(audit_log).unwrap();
        assert!(audit.contains("phase=schema validate event=start"));
        assert!(audit.contains("schema_status status=managed pending_count=0"));
        assert!(!audit.contains("MEMORIES_ATLAS_DATABASE_URL"));
        assert!(!audit.contains(&env.database_url));
        assert!(audit.contains("<redacted-database-url>"));

        run_startup_gate(&data, &script, STARTUP_MIGRATION, &env)
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_command_keeps_receipt_absent_and_retry_reuses_original_backup() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        let fail = dir.path().join("fail");
        fs::write(&fail, b"1").unwrap();
        let script = dir.path().join("memories-db-migrate");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ \"$1 $2\" = 'schema status' ]; then echo 'schema_status status=managed pending_count=0'; fi\nif [ \"$1 $2\" = 'schema verify' ] && [ -f '{}' ]; then echo verify-failed >&2; exit 9; fi\n",
                fail.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let env = MigrationEnvironment {
            database_url: data.memories_sqlite_url(),
            thread_vector_enabled: false,
            thread_lancedb_uri: data.lancedb_dir(),
            thread_lancedb_table: "threads".into(),
            thread_vector_size: 4,
            memory_fts_tokenizer: "simple".into(),
            lance_language_model_home: data.lance_language_model_home(),
        };
        let error = run_startup_gate(&data, &script, STARTUP_MIGRATION, &env)
            .await
            .unwrap_err();
        let AppError::DatabaseMigrationFailed { backup_path, .. } = error else {
            panic!("migration failure must stay structured");
        };
        assert!(!backup_path.is_empty());
        assert!(!data.database_migration_receipt_path().exists());
        assert_eq!(
            fs::read_dir(data.database_migration_backups_dir())
                .unwrap()
                .count(),
            1
        );
        fs::remove_file(fail).unwrap();
        run_startup_gate(&data, &script, STARTUP_MIGRATION, &env)
            .await
            .unwrap();
        assert_eq!(
            fs::read_dir(data.database_migration_backups_dir())
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            load_receipt(&data.database_migration_receipt_path())
                .unwrap()
                .attempt_id,
            Path::new(&backup_path)
                .file_name()
                .unwrap()
                .to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restored_generation_blocks_normal_start_until_explicit_retry() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        let backup = create_backup(&data, "restored-attempt").unwrap();
        let attempt = MigrationAttempt {
            format_version: 1,
            attempt_id: "restored-attempt".into(),
            migration_id: STARTUP_MIGRATION.migration_id.into(),
            backup_dir: backup,
            state: AttemptState::Restored,
            phase: "restored".into(),
            updated_at: "now".into(),
            error: None,
        };
        save_attempt(&data, &attempt).unwrap();
        let log = dir.path().join("coordinator.log");
        let script = dir.path().join("memories-db-migrate");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1 $2\" = 'schema status' ]; then echo 'schema_status status=managed pending_count=0'; fi\n",
                log.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let env = MigrationEnvironment {
            database_url: data.memories_sqlite_url(),
            thread_vector_enabled: false,
            thread_lancedb_uri: data.lancedb_dir(),
            thread_lancedb_table: "threads".into(),
            thread_vector_size: 4,
            memory_fts_tokenizer: "simple".into(),
            lance_language_model_home: data.lance_language_model_home(),
        };

        let error = run_startup_gate(&data, &script, STARTUP_MIGRATION, &env)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AppError::DatabaseMigrationFailed { ref phase, .. } if phase == "restore pending retry"
        ));
        assert!(!log.exists());

        request_explicit_retry(&data).unwrap();
        assert_eq!(
            load_attempt(&data).unwrap().state,
            AttemptState::RetryRequested
        );
        run_startup_gate(&data, &script, STARTUP_MIGRATION, &env)
            .await
            .unwrap();
        assert!(log.exists());
        assert_eq!(
            load_receipt(&data.database_migration_receipt_path())
                .unwrap()
                .attempt_id,
            "restored-attempt"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn baseline_required_runs_baseline_then_the_complete_preflight_again() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        let state = dir.path().join("baselined");
        let log = dir.path().join("commands.log");
        let script = dir.path().join("memories-db-migrate");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nif [ \"$1 $2\" = 'schema baseline' ]; then touch '{}'; fi\nif [ \"$1 $2\" = 'schema status' ]; then if [ -f '{}' ]; then echo 'schema_status status=managed pending_count=0'; else echo 'schema_status status=baseline_required pending_count=unknown'; fi; fi\n",
                log.display(), state.display(), state.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let env = MigrationEnvironment {
            database_url: data.memories_sqlite_url(),
            thread_vector_enabled: false,
            thread_lancedb_uri: data.lancedb_dir(),
            thread_lancedb_table: "threads".into(),
            thread_vector_size: 4,
            memory_fts_tokenizer: "simple".into(),
            lance_language_model_home: data.lance_language_model_home(),
        };
        run_startup_gate(&data, &script, STARTUP_MIGRATION, &env)
            .await
            .unwrap();
        let commands = fs::read_to_string(log).unwrap();
        assert_eq!(commands.matches("schema validate\n").count(), 3);
        assert_eq!(commands.matches("schema apply --dry-run\n").count(), 2);
        assert_eq!(commands.matches("post-migrate status\n").count(), 2);
        assert!(
            commands.find("schema baseline\n").unwrap()
                > commands.find("post-migrate status\n").unwrap()
        );
    }

    #[test]
    fn explicit_restore_uses_one_generation_and_rejects_incomplete_generation() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        let db = Connection::open(data.memories_sqlite_path()).unwrap();
        db.execute("CREATE TABLE sample(value TEXT)", []).unwrap();
        db.execute("INSERT INTO sample VALUES ('before')", [])
            .unwrap();
        fs::write(data.lancedb_dir().join("part"), b"before-vector").unwrap();
        fs::write(data.database_migration_receipt_path(), b"before-receipt").unwrap();
        let backup = create_backup(&data, "restore-attempt").unwrap();
        let attempt = MigrationAttempt {
            format_version: 1,
            attempt_id: "restore-attempt".into(),
            migration_id: STARTUP_MIGRATION.migration_id.into(),
            backup_dir: backup.clone(),
            state: AttemptState::Failed,
            phase: "schema apply".into(),
            updated_at: "now".into(),
            error: Some("failed".into()),
        };
        save_attempt(&data, &attempt).unwrap();
        db.execute("UPDATE sample SET value='after'", []).unwrap();
        drop(db);
        let live_wal = PathBuf::from(format!("{}-wal", data.memories_sqlite_path().display()));
        let live_shm = PathBuf::from(format!("{}-shm", data.memories_sqlite_path().display()));
        fs::write(&live_wal, b"stale-wal").unwrap();
        fs::write(&live_shm, b"stale-shm").unwrap();
        fs::write(data.lancedb_dir().join("part"), b"after-vector").unwrap();
        fs::write(data.database_migration_receipt_path(), b"after-receipt").unwrap();

        atomic_json(&backup.join("attempt.json"), &attempt).unwrap();
        assert_eq!(restore_backup(&data, &backup).unwrap(), backup);
        let restored_attempt = load_attempt(&data).unwrap();
        assert_eq!(restored_attempt.state, AttemptState::Restored);
        assert_eq!(restored_attempt.phase, "restored");
        assert!(!live_wal.exists());
        assert!(!live_shm.exists());
        let restored = Connection::open(data.memories_sqlite_path()).unwrap();
        let value: String = restored
            .query_row("SELECT value FROM sample", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "before");
        assert_eq!(
            fs::read(data.lancedb_dir().join("part")).unwrap(),
            b"before-vector"
        );
        assert_eq!(
            fs::read(data.database_migration_receipt_path()).unwrap(),
            b"before-receipt"
        );

        fs::remove_file(backup.join("receipt.absent")).ok();
        fs::remove_file(backup.join("database-migration-receipt.json")).unwrap();
        fs::write(data.lancedb_dir().join("part"), b"unchanged").unwrap();
        assert!(restore_backup(&data, &backup).is_err());
        assert_eq!(
            fs::read(data.lancedb_dir().join("part")).unwrap(),
            b"unchanged"
        );
    }

    #[test]
    fn restore_reads_wal_mode_backup_without_wal_or_shm_files() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("data"));
        data.ensure().unwrap();
        {
            let db = Connection::open(data.memories_sqlite_path()).unwrap();
            db.pragma_update(None, "journal_mode", "WAL").unwrap();
            db.execute("CREATE TABLE sample(value TEXT)", []).unwrap();
            db.execute("INSERT INTO sample VALUES ('before')", [])
                .unwrap();
        }
        fs::write(data.lancedb_dir().join("part"), b"before-vector").unwrap();
        let backup = create_backup(&data, "wal-restore-attempt").unwrap();
        assert!(!backup.join("default.sqlite3-wal").exists());
        assert!(!backup.join("default.sqlite3-shm").exists());
        let attempt = MigrationAttempt {
            format_version: 1,
            attempt_id: "wal-restore-attempt".into(),
            migration_id: STARTUP_MIGRATION.migration_id.into(),
            backup_dir: backup.clone(),
            state: AttemptState::Failed,
            phase: "schema apply".into(),
            updated_at: "now".into(),
            error: Some("failed".into()),
        };
        save_attempt(&data, &attempt).unwrap();
        atomic_json(&backup.join("attempt.json"), &attempt).unwrap();
        {
            let db = Connection::open(data.memories_sqlite_path()).unwrap();
            db.execute("UPDATE sample SET value='after'", []).unwrap();
        }

        restore_backup(&data, &backup).unwrap();

        let restored = Connection::open(data.memories_sqlite_path()).unwrap();
        let value: String = restored
            .query_row("SELECT value FROM sample", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "before");
    }

    #[test]
    fn restore_stage_failure_keeps_live_data_and_records_auditable_failure() {
        let (_dir, data, backup) = restore_fixture();
        fs::write(backup.join("default.sqlite3"), b"not a SQLite database").unwrap();

        let error = restore_backup(&data, &backup).unwrap_err();

        assert!(error.to_string().contains("restore stage failed"));
        assert_live_restore_sources_unchanged(&data);
        assert!(
            !data
                .root
                .join(".migration-restore-restore-attempt")
                .exists()
        );
        let attempt: MigrationAttempt =
            serde_json::from_slice(&fs::read(data.database_migration_attempt_path()).unwrap())
                .unwrap();
        assert_eq!(attempt.state, AttemptState::Failed);
        assert_eq!(attempt.phase, "restore stage");
        assert!(attempt.error.unwrap().contains("restore stage failed"));
        let audit = fs::read_to_string(
            data.log_dir()
                .join("database-migration-restore-attempt.log"),
        )
        .unwrap();
        assert!(audit.contains("phase=restore stage event=failed"));
    }

    #[test]
    fn restore_copy_failure_keeps_live_data_and_cleans_completed_sqlite_stage() {
        let (_dir, data, backup) = restore_fixture();
        let mut stage_hook = |step| {
            if step == RestoreStageStep::CopyLanceDb {
                Err(std::io::Error::other("injected LanceDB copy failure"))
            } else {
                Ok(())
            }
        };
        let mut before_switch_hook = || Ok(());
        let mut switch_hook = |_| Ok(());
        let mut cleanup_hook = || Ok(());

        let error = restore_backup_with_hooks(
            &data,
            &backup,
            &mut stage_hook,
            &mut before_switch_hook,
            &mut switch_hook,
            &mut cleanup_hook,
        )
        .unwrap_err();

        assert!(error.to_string().contains("restore stage failed"));
        assert_live_restore_sources_unchanged(&data);
        assert!(
            !data
                .root
                .join(".migration-restore-restore-attempt")
                .exists()
        );
        let attempt = load_attempt(&data).unwrap();
        assert_eq!(attempt.state, AttemptState::Failed);
        assert_eq!(attempt.phase, "restore stage");
        assert!(
            attempt
                .error
                .unwrap()
                .contains("injected LanceDB copy failure")
        );
    }

    #[tokio::test]
    async fn durable_restore_prepare_blocks_normal_gate_after_interruption() {
        let (_dir, data, backup) = restore_fixture();
        let mut stage_hook = |_| Ok(());
        let mut before_switch_hook = || Err(std::io::Error::other("injected crash boundary"));
        let mut switch_hook = |_| Ok(());
        let mut cleanup_hook = || Ok(());

        let error = restore_backup_with_hooks(
            &data,
            &backup,
            &mut stage_hook,
            &mut before_switch_hook,
            &mut switch_hook,
            &mut cleanup_hook,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("restore switch interrupted after durable prepare")
        );
        assert_live_restore_sources_unchanged(&data);
        assert_eq!(
            load_attempt(&data).unwrap().state,
            AttemptState::RestoreInProgress
        );
        let env = MigrationEnvironment {
            database_url: data.memories_sqlite_url(),
            thread_vector_enabled: false,
            thread_lancedb_uri: data.lancedb_dir(),
            thread_lancedb_table: "threads".into(),
            thread_vector_size: 4,
            memory_fts_tokenizer: "simple".into(),
            lance_language_model_home: data.lance_language_model_home(),
        };
        let gate_error = run_startup_gate(
            &data,
            Path::new("/definitely/missing/coordinator"),
            STARTUP_MIGRATION,
            &env,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            gate_error,
            AppError::DatabaseMigrationFailed { ref phase, .. } if phase == "restore pending recovery"
        ));
    }

    #[tokio::test]
    async fn restore_cleanup_failure_keeps_restored_state_and_blocks_automatic_gate() {
        let (_dir, data, backup) = restore_fixture();
        let mut stage_hook = |_| Ok(());
        let mut before_switch_hook = || Ok(());
        let mut switch_hook = |_| Ok(());
        let mut cleanup_hook = || Err(std::io::Error::other("injected cleanup failure"));

        let error = restore_backup_with_hooks(
            &data,
            &backup,
            &mut stage_hook,
            &mut before_switch_hook,
            &mut switch_hook,
            &mut cleanup_hook,
        )
        .unwrap_err();

        assert!(error.to_string().contains("restore cleanup failed"));
        assert_eq!(load_attempt(&data).unwrap().state, AttemptState::Restored);
        let env = MigrationEnvironment {
            database_url: data.memories_sqlite_url(),
            thread_vector_enabled: false,
            thread_lancedb_uri: data.lancedb_dir(),
            thread_lancedb_table: "threads".into(),
            thread_vector_size: 4,
            memory_fts_tokenizer: "simple".into(),
            lance_language_model_home: data.lance_language_model_home(),
        };
        let gate_error = run_startup_gate(
            &data,
            Path::new("/definitely/missing/coordinator"),
            STARTUP_MIGRATION,
            &env,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            gate_error,
            AppError::DatabaseMigrationFailed { ref phase, .. } if phase == "restore pending retry"
        ));
    }

    #[test]
    fn restore_switch_rolls_back_every_evacuate_and_install_failure() {
        for failed_step in RestoreSwitchStep::ALL {
            let (_dir, data, backup) = restore_fixture();
            let mut hook = |step| {
                if step == failed_step {
                    Err(std::io::Error::other(format!(
                        "injected restore failure at {step:?}"
                    )))
                } else {
                    Ok(())
                }
            };

            assert!(restore_backup_with_switch_hook(&data, &backup, &mut hook).is_err());
            assert_live_restore_sources_unchanged(&data);
            assert_eq!(
                load_attempt(&data).unwrap().state,
                AttemptState::RestoreInProgress
            );
        }
    }
}
