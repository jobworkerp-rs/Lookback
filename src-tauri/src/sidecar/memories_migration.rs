//! Shared process adapter for the Memories-owned database migration CLI.
//!
//! Lookback only stops every writer, invokes `memories-db-migrate local apply`
//! or `local restore`, and reports the tool's structured result. Deciding what
//! to migrate, taking and pruning backups, restoring, and judging whether a
//! retry is safe all belong to Memories, so no task generation or storage
//! layout knowledge lives here. Embedding-space requests are typed in the
//! sibling adapter and reuse this module's child runner and output decoder.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tracing::warn;

use crate::data::DataPaths;
use crate::data::paths::MigrationBackupSettings;
use crate::error::{AppError, AppResult};

macro_rules! cli_vocabulary {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub fn parse(value: &str) -> Option<Self> {
                match value {
                    $($value => Some(Self::$variant),)+
                    _ => None,
                }
            }

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $value,)+
                }
            }
        }
    };
}

cli_vocabulary!(CliCommand {
    Inspect => "embedding_inspect",
    Plan => "embedding_plan",
    Switch => "embedding_switch",
    Abandon => "embedding_abandon",
    Finalize => "embedding_finalize",
    Restore => "embedding_restore",
    LocalApply => "local_apply",
    LocalRestore => "local_restore",
});

cli_vocabulary!(CliStage {
    Open => "open",
    Preflight => "preflight",
    Record => "record",
    Backup => "backup",
    Replace => "replace",
    Cleanup => "cleanup",
    Discard => "discard",
    Verify => "verify",
    Commit => "commit",
    Retention => "retention",
    Validate => "validate",
    WriterCheck => "writer_check",
    Plan => "plan",
    Baseline => "baseline",
    SchemaApply => "schema_apply",
    PostMigrate => "post_migrate",
    RestoreValidate => "restore_validate",
    RestorePreflight => "restore_preflight",
    RestoreReplace => "restore_replace",
});

cli_vocabulary!(CliErrorCode {
    StorageMismatch => "storage_mismatch",
    OperationInProgress => "operation_in_progress",
    WriterActive => "writer_active",
    EmbeddingSwitchInProgress => "embedding_switch_in_progress",
    FinalizeInProgress => "finalize_in_progress",
    CancelInProgress => "cancel_in_progress",
    ApplyRequired => "apply_required",
    ApplyRestoreRequired => "apply_restore_required",
    SpaceChanged => "space_changed",
    NoReembedNeeded => "no_reembed_needed",
    BackupOptionRequired => "backup_option_required",
    InsufficientSpace => "insufficient_space",
    UnsupportedResource => "unsupported_resource",
    ResourceNotWritable => "resource_not_writable",
    ResourceUnavailable => "resource_unavailable",
    ResourceCorrupt => "resource_corrupt",
    SwitchStateUnverifiable => "switch_state_unverifiable",
    AbandonNotAllowed => "abandon_not_allowed",
    RestoreNotAvailable => "restore_not_available",
    AttemptNotFound => "attempt_not_found",
    AttemptFinished => "attempt_finished",
    RebuildNotPending => "rebuild_not_pending",
    RebuildIncomplete => "rebuild_incomplete",
    RebuildHasFailures => "rebuild_has_failures",
    DbNewerThanTool => "db_newer_than_tool",
    ToolUpdateRequired => "tool_update_required",
    UnsupportedBackend => "unsupported_backend",
    InvalidTarget => "invalid_target",
    LegacySchemaUnsupported => "legacy_schema_unsupported",
    IntegrityViolation => "integrity_violation",
    BaselineSchemaMismatch => "baseline_schema_mismatch",
    RestoreRequired => "restore_required",
    RestoreRequiredNoBackup => "restore_required_no_backup",
    BackupIncomplete => "backup_incomplete",
    BackupNewerThanTool => "backup_newer_than_tool",
    MigrationFailed => "migration_failed",
    VerifyFailed => "verify_failed",
    BackupFailed => "backup_failed",
    RestoreFailed => "restore_failed",
    BundleInvalid => "bundle_invalid",
    EmbeddingMigrationInProgress => "embedding_migration_in_progress",
    BackupSpaceMismatch => "backup_space_mismatch",
});

cli_vocabulary!(CliResolution {
    Retry => "retry",
    PlanRequired => "plan_required",
    CheckEnvironment => "check_environment",
    ManualRecovery => "manual_recovery",
    ToolUpdateRequired => "tool_update_required",
    ResumeOrAbandon => "resume_or_abandon",
    AbandonRequired => "abandon_required",
    ResumeOrRestore => "resume_or_restore",
    ContinueOrRestore => "continue_or_restore",
    ContinueOrAbandon => "continue_or_abandon",
    ResumeRebuild => "resume_rebuild",
    ResolveFailures => "resolve_failures",
    FinalizeRequired => "finalize_required",
    RestoreRequired => "restore_required",
    ApplyRequired => "apply_required",
    ApplyRestoreRequired => "apply_restore_required",
    LegacyUpgradeRequired => "legacy_upgrade_required",
});

cli_vocabulary!(AttemptStage {
    Backup => "backup",
    Replace => "replace",
    RebuildPending => "rebuild_pending",
    Commit => "commit",
    Restoring => "restoring",
    Discarding => "discarding",
});

cli_vocabulary!(BackupMode {
    Backup => "backup",
    None => "none",
});

cli_vocabulary!(CancelOperation {
    Restore => "restore",
    Abandon => "abandon",
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingAttemptInfo {
    pub attempt: String,
    pub stage: AttemptStage,
    pub backup_mode: BackupMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationCounts {
    pub missing_memory_text: u64,
    pub stale_memory_text: u64,
    pub unverified_memory_text: u64,
    pub failed_memory_text: u64,
    pub missing_memory_media: u64,
    pub stale_memory_media: u64,
    pub unverified_memory_media: u64,
    pub failed_memory_media: u64,
    pub missing_thread: u64,
    pub stale_thread: u64,
    pub unverified_thread: u64,
    pub failed_thread: u64,
    pub missing_reflection_intent: u64,
    pub stale_reflection_intent: u64,
    pub unverified_reflection_intent: u64,
    pub failed_reflection_intent: u64,
    pub orphan: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingMigrationFailure {
    pub command: CliCommand,
    pub stage: CliStage,
    pub error_code: CliErrorCode,
    pub resolution: CliResolution,
    pub attempt: Option<EmbeddingAttemptInfo>,
    pub backup: Option<String>,
    pub cancel_operation: Option<CancelOperation>,
    pub verification_counts: Option<VerificationCounts>,
}

/// Error code Lookback reports when the tool could not be started at all.
pub const TOOL_UNAVAILABLE: &str = "tool_unavailable";
/// Error code Lookback reports when the tool ended without the structured
/// result line its contract promises (for example after a crash).
pub const TOOL_OUTPUT_UNRECOGNIZED: &str = "tool_output_unrecognized";

/// Stages after which the tool may change data. Earlier stages only inspect,
/// so they must not surface the "migrating" notice on an ordinary startup.
const MUTATING_STAGES: &[&str] = &[
    "backup",
    "baseline",
    "schema_apply",
    "post_migrate",
    "verify",
];
const DETAIL_LINES: usize = 20;

#[derive(Debug, Clone)]
pub struct MigrationEnvironment {
    pub database_url: String,
    pub thread_vector_enabled: bool,
    pub memory_fts_tokenizer: String,
    pub lance_language_model_home: PathBuf,
    /// Additional effective Memories settings shared with the CLI process.
    pub extra_env: BTreeMap<OsString, OsString>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupPolicy {
    Backup { dir: PathBuf, keep: u32 },
    NoBackupUnsafe,
}

impl BackupPolicy {
    pub fn from_settings(settings: MigrationBackupSettings, data: &DataPaths) -> Self {
        if settings.enabled {
            Self::Backup {
                dir: data.database_migration_backups_dir(),
                keep: settings.keep,
            }
        } else {
            Self::NoBackupUnsafe
        }
    }

    fn args(&self) -> Vec<OsString> {
        match self {
            Self::Backup { dir, keep } => vec![
                "--backup-dir".into(),
                dir.into(),
                "--backup-keep".into(),
                keep.to_string().into(),
            ],
            Self::NoBackupUnsafe => vec!["--no-backup-unsafe".into()],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    NoOp,
    Migrated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolResult {
    Completed(BTreeMap<String, String>),
    Failed(BTreeMap<String, String>),
}

/// Parses a `<token> key=value ...` line. Values are percent-encoded by the
/// tool so a path with spaces still forms a single field.
pub(super) fn structured_line(line: &str) -> Option<(&str, BTreeMap<String, String>)> {
    let mut parts = line.split_ascii_whitespace();
    let token = parts.next()?;
    let mut fields = BTreeMap::new();
    for part in parts {
        let (key, value) = part.split_once('=')?;
        if key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || fields.contains_key(key)
        {
            return None;
        }
        fields.insert(key.to_string(), decode_value(value)?);
    }
    Some((token, fields))
}

/// Decode only the byte spelling emitted by the migration CLI. The generic
/// percent decoder accepts malformed `%` sequences literally, which could
/// make a corrupt result look like a valid recovery path or identifier.
fn decode_value(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            decoded.push((hex_value(high)? << 4) | hex_value(low)?);
            index += 3;
        } else {
            if !matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':')
            {
                return None;
            }
            decoded.push(byte);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded).ok()?;
    (!decoded.contains('\0')).then_some(decoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Only the last non-empty stdout line is the tool's result; every earlier
/// line is progress or diagnostics and must not drive decisions.
fn final_result(stdout: &str, token: &str) -> Option<ToolResult> {
    let line = stdout.lines().rev().find(|line| !line.trim().is_empty())?;
    let (found, fields) = structured_line(line.trim())?;
    if found != token {
        return None;
    }
    match fields.get("status")?.as_str() {
        "completed" => Some(ToolResult::Completed(fields)),
        "failed"
            if fields.contains_key("stage")
                && fields.contains_key("error_code")
                && fields.contains_key("resolution") =>
        {
            Some(ToolResult::Failed(fields))
        }
        _ => None,
    }
}

pub(super) fn parse_embedding_failure(
    command: CliCommand,
    mut fields: BTreeMap<String, String>,
) -> Result<EmbeddingMigrationFailure, String> {
    if fields.remove("status").as_deref() != Some("failed") {
        return Err("failure status is missing".into());
    }
    let stage = fields
        .remove("stage")
        .and_then(|value| CliStage::parse(&value))
        .ok_or_else(|| "missing or unknown failure stage".to_string())?;
    if !stage_allowed_for(command, stage) {
        return Err(format!(
            "stage {} is invalid for {}",
            stage.as_str(),
            command.as_str()
        ));
    }
    let error_code = fields
        .remove("error_code")
        .and_then(|value| CliErrorCode::parse(&value))
        .ok_or_else(|| "missing or unknown failure error_code".to_string())?;
    if !error_code_allowed_for(command, error_code) {
        return Err(format!(
            "error code {} is invalid for {}",
            error_code.as_str(),
            command.as_str()
        ));
    }
    let resolution = fields
        .remove("resolution")
        .and_then(|value| CliResolution::parse(&value))
        .ok_or_else(|| "missing or unknown failure resolution".to_string())?;
    if !resolution_allowed_for(command, error_code, resolution) {
        return Err(format!(
            "resolution {} is invalid for {}",
            resolution.as_str(),
            error_code.as_str()
        ));
    }

    let attempt = fields.remove("attempt");
    let attempt_stage = fields
        .remove("attempt_stage")
        .map(|value| AttemptStage::parse(&value).ok_or_else(|| "unknown attempt_stage".to_string()))
        .transpose()?;
    let backup_mode = fields
        .remove("backup_mode")
        .map(|value| BackupMode::parse(&value).ok_or_else(|| "unknown backup_mode".to_string()))
        .transpose()?;
    let attempt_info = match (attempt, attempt_stage, backup_mode) {
        (Some(attempt), Some(stage), Some(backup_mode)) if !attempt.is_empty() => {
            Some(EmbeddingAttemptInfo {
                attempt,
                stage,
                backup_mode,
            })
        }
        (None, None, None) => None,
        _ => return Err("attempt, attempt_stage, and backup_mode must appear together".into()),
    };
    if (resolution_requires_attempt(resolution) || error_code == CliErrorCode::CancelInProgress)
        && attempt_info.is_none()
    {
        return Err(format!(
            "resolution {} requires attempt fields",
            resolution.as_str()
        ));
    }
    let backup = fields.remove("backup");
    if resolution_requires_backup(resolution)
        && !backup
            .as_deref()
            .is_some_and(|path| !path.is_empty() && !matches!(path, "none" | "unknown"))
    {
        return Err(format!(
            "resolution {} requires a concrete backup path",
            resolution.as_str()
        ));
    }
    if resolution_requires_attempt_backup(resolution)
        && attempt_info
            .as_ref()
            .is_some_and(|attempt| attempt.backup_mode != BackupMode::Backup)
    {
        return Err(format!(
            "resolution {} requires a backup-mode attempt",
            resolution.as_str()
        ));
    }
    if backup.as_ref().is_some_and(String::is_empty) {
        return Err("backup path must not be empty".into());
    }
    let cancel_operation = fields
        .remove("cancel_operation")
        .map(|value| {
            CancelOperation::parse(&value).ok_or_else(|| "unknown cancel_operation".to_string())
        })
        .transpose()?;
    if (error_code == CliErrorCode::CancelInProgress) != cancel_operation.is_some() {
        return Err("cancel_in_progress must include exactly one cancel_operation".into());
    }
    if let Some(operation) = cancel_operation {
        let resolution_matches = match operation {
            CancelOperation::Restore => matches!(
                resolution,
                CliResolution::RestoreRequired | CliResolution::ManualRecovery
            ),
            CancelOperation::Abandon => matches!(
                resolution,
                CliResolution::AbandonRequired | CliResolution::ManualRecovery
            ),
        };
        if !resolution_matches {
            return Err("cancel_operation and resolution disagree".into());
        }
    }

    let verification_counts = if matches!(
        error_code,
        CliErrorCode::RebuildIncomplete | CliErrorCode::RebuildHasFailures
    ) {
        Some(VerificationCounts {
            missing_memory_text: take_count(&mut fields, "missing_memory_text")?,
            stale_memory_text: take_count(&mut fields, "stale_memory_text")?,
            unverified_memory_text: take_count(&mut fields, "unverified_memory_text")?,
            failed_memory_text: take_count(&mut fields, "failed_memory_text")?,
            missing_memory_media: take_count(&mut fields, "missing_memory_media")?,
            stale_memory_media: take_count(&mut fields, "stale_memory_media")?,
            unverified_memory_media: take_count(&mut fields, "unverified_memory_media")?,
            failed_memory_media: take_count(&mut fields, "failed_memory_media")?,
            missing_thread: take_count(&mut fields, "missing_thread")?,
            stale_thread: take_count(&mut fields, "stale_thread")?,
            unverified_thread: take_count(&mut fields, "unverified_thread")?,
            failed_thread: take_count(&mut fields, "failed_thread")?,
            missing_reflection_intent: take_count(&mut fields, "missing_reflection_intent")?,
            stale_reflection_intent: take_count(&mut fields, "stale_reflection_intent")?,
            unverified_reflection_intent: take_count(&mut fields, "unverified_reflection_intent")?,
            failed_reflection_intent: take_count(&mut fields, "failed_reflection_intent")?,
            orphan: take_count(&mut fields, "orphan")?,
        })
    } else {
        None
    };
    if !fields.is_empty() {
        return Err(format!(
            "unknown failure fields: {}",
            fields.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    Ok(EmbeddingMigrationFailure {
        command,
        stage,
        error_code,
        resolution,
        attempt: attempt_info,
        backup,
        cancel_operation,
        verification_counts,
    })
}

fn take_count(fields: &mut BTreeMap<String, String>, name: &str) -> Result<u64, String> {
    let value = fields
        .remove(name)
        .ok_or_else(|| format!("missing verification count {name}"))?;
    parse_unsigned(&value).ok_or_else(|| format!("invalid verification count {name}"))
}

fn parse_unsigned(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn resolution_requires_attempt(resolution: CliResolution) -> bool {
    matches!(
        resolution,
        CliResolution::ResumeOrAbandon
            | CliResolution::AbandonRequired
            | CliResolution::ResumeOrRestore
            | CliResolution::ContinueOrRestore
            | CliResolution::ContinueOrAbandon
            | CliResolution::ResumeRebuild
            | CliResolution::ResolveFailures
            | CliResolution::FinalizeRequired
            | CliResolution::RestoreRequired
    )
}

fn resolution_requires_backup(resolution: CliResolution) -> bool {
    matches!(
        resolution,
        CliResolution::ResumeOrRestore
            | CliResolution::ContinueOrRestore
            | CliResolution::RestoreRequired
            | CliResolution::ApplyRestoreRequired
    )
}

fn resolution_requires_attempt_backup(resolution: CliResolution) -> bool {
    matches!(
        resolution,
        CliResolution::ResumeOrRestore
            | CliResolution::ContinueOrRestore
            | CliResolution::RestoreRequired
    )
}

fn stage_allowed_for(command: CliCommand, stage: CliStage) -> bool {
    match command {
        CliCommand::Plan => stage == CliStage::Open,
        CliCommand::Switch => matches!(
            stage,
            CliStage::Preflight | CliStage::Record | CliStage::Backup | CliStage::Replace
        ),
        CliCommand::Abandon => matches!(
            stage,
            CliStage::Preflight | CliStage::Cleanup | CliStage::Discard
        ),
        CliCommand::Finalize => matches!(
            stage,
            CliStage::Preflight
                | CliStage::Verify
                | CliStage::Commit
                | CliStage::Cleanup
                | CliStage::Retention
        ),
        CliCommand::Restore => matches!(
            stage,
            CliStage::Preflight | CliStage::Validate | CliStage::Replace | CliStage::Cleanup
        ),
        CliCommand::LocalApply => matches!(
            stage,
            CliStage::Preflight
                | CliStage::WriterCheck
                | CliStage::Plan
                | CliStage::Backup
                | CliStage::Baseline
                | CliStage::SchemaApply
                | CliStage::PostMigrate
                | CliStage::Verify
                | CliStage::Retention
        ),
        CliCommand::LocalRestore => matches!(
            stage,
            CliStage::Preflight
                | CliStage::RestoreValidate
                | CliStage::RestorePreflight
                | CliStage::RestoreReplace
                | CliStage::Cleanup
        ),
        CliCommand::Inspect => false,
    }
}

fn error_code_allowed_for(command: CliCommand, code: CliErrorCode) -> bool {
    match command {
        CliCommand::LocalApply | CliCommand::LocalRestore => matches!(
            code,
            CliErrorCode::OperationInProgress
                | CliErrorCode::StorageMismatch
                | CliErrorCode::EmbeddingMigrationInProgress
                | CliErrorCode::BackupSpaceMismatch
        ),
        CliCommand::Inspect
        | CliCommand::Plan
        | CliCommand::Switch
        | CliCommand::Abandon
        | CliCommand::Finalize
        | CliCommand::Restore => !matches!(
            code,
            CliErrorCode::UnsupportedBackend
                | CliErrorCode::InvalidTarget
                | CliErrorCode::LegacySchemaUnsupported
                | CliErrorCode::IntegrityViolation
                | CliErrorCode::BaselineSchemaMismatch
                | CliErrorCode::RestoreRequired
                | CliErrorCode::RestoreRequiredNoBackup
                | CliErrorCode::BackupNewerThanTool
                | CliErrorCode::MigrationFailed
                | CliErrorCode::VerifyFailed
                | CliErrorCode::BackupFailed
                | CliErrorCode::RestoreFailed
                | CliErrorCode::BundleInvalid
                | CliErrorCode::EmbeddingMigrationInProgress
                | CliErrorCode::BackupSpaceMismatch
        ),
    }
}

fn resolution_allowed_for(
    command: CliCommand,
    code: CliErrorCode,
    resolution: CliResolution,
) -> bool {
    use CliErrorCode as E;
    use CliResolution as R;
    if matches!(command, CliCommand::LocalApply | CliCommand::LocalRestore) {
        return match code {
            E::OperationInProgress => resolution == R::Retry,
            E::StorageMismatch => resolution == R::CheckEnvironment,
            E::EmbeddingMigrationInProgress => matches!(
                resolution,
                R::AbandonRequired
                    | R::RestoreRequired
                    | R::ContinueOrRestore
                    | R::ContinueOrAbandon
                    | R::FinalizeRequired
            ),
            E::BackupSpaceMismatch => resolution == R::ManualRecovery,
            _ => false,
        };
    }
    match code {
        E::StorageMismatch => resolution == R::CheckEnvironment,
        E::OperationInProgress => resolution == R::Retry,
        E::WriterActive => matches!(resolution, R::Retry | R::ResumeOrAbandon),
        E::EmbeddingSwitchInProgress => matches!(
            resolution,
            R::ResumeOrAbandon
                | R::ResumeOrRestore
                | R::ContinueOrRestore
                | R::ContinueOrAbandon
                | R::FinalizeRequired
        ),
        E::FinalizeInProgress => resolution == R::FinalizeRequired,
        E::CancelInProgress => matches!(
            resolution,
            R::RestoreRequired | R::AbandonRequired | R::ManualRecovery
        ),
        E::ApplyRequired => resolution == R::ApplyRequired,
        E::ApplyRestoreRequired => resolution == R::ApplyRestoreRequired,
        E::SpaceChanged | E::NoReembedNeeded | E::AttemptNotFound | E::AttemptFinished => {
            resolution == R::PlanRequired
        }
        E::RebuildNotPending => matches!(
            resolution,
            R::PlanRequired | R::ResumeOrAbandon | R::ResumeOrRestore
        ),
        E::BackupOptionRequired
        | E::UnsupportedResource
        | E::DbNewerThanTool
        | E::ToolUpdateRequired => resolution == R::ToolUpdateRequired,
        E::InsufficientSpace => matches!(resolution, R::Retry | R::ResumeOrAbandon),
        E::ResourceNotWritable | E::ResourceUnavailable => resolution == R::CheckEnvironment,
        E::ResourceCorrupt => matches!(resolution, R::RestoreRequired | R::ManualRecovery),
        E::SwitchStateUnverifiable => matches!(
            resolution,
            R::AbandonRequired | R::RestoreRequired | R::ManualRecovery
        ),
        E::AbandonNotAllowed => matches!(
            resolution,
            R::ResumeOrRestore | R::ContinueOrRestore | R::FinalizeRequired
        ),
        E::RestoreNotAvailable => matches!(
            resolution,
            R::AbandonRequired | R::ResumeOrAbandon | R::ContinueOrAbandon | R::FinalizeRequired
        ),
        E::RebuildIncomplete => resolution == R::ResumeRebuild,
        E::RebuildHasFailures => resolution == R::ResolveFailures,
        E::UnsupportedBackend
        | E::InvalidTarget
        | E::LegacySchemaUnsupported
        | E::IntegrityViolation
        | E::BaselineSchemaMismatch
        | E::RestoreRequired
        | E::RestoreRequiredNoBackup
        | E::BackupIncomplete
        | E::BackupNewerThanTool
        | E::MigrationFailed
        | E::VerifyFailed
        | E::BackupFailed
        | E::RestoreFailed
        | E::BundleInvalid
        | E::EmbeddingMigrationInProgress
        | E::BackupSpaceMismatch => false,
    }
}

pub(super) fn is_embedding_related_failure(fields: &BTreeMap<String, String>) -> bool {
    [
        "attempt",
        "attempt_stage",
        "backup_mode",
        "cancel_operation",
    ]
    .iter()
    .any(|key| fields.contains_key(*key))
        || matches!(
            fields.get("error_code").map(String::as_str),
            Some(
                "operation_in_progress"
                    | "storage_mismatch"
                    | "embedding_migration_in_progress"
                    | "backup_space_mismatch"
            )
        )
}

fn mutating_progress(line: &str) -> bool {
    matches!(
        structured_line(line.trim()),
        Some(("local_progress", fields))
            if fields.get("stage").is_some_and(|stage| MUTATING_STAGES.contains(&stage.as_str()))
    )
}

/// Runs `local apply`. `on_change_started` fires once the tool enters a stage
/// that can change data, so a no-op startup never claims to be migrating.
pub async fn run_local_apply(
    tool: &Path,
    env: &MigrationEnvironment,
    policy: &BackupPolicy,
    data: &DataPaths,
    on_change_started: &mut (dyn FnMut() + Send),
) -> AppResult<ApplyOutcome> {
    let mut args: Vec<OsString> = vec![
        "local".into(),
        "apply".into(),
        "--maintenance-window-ack".into(),
    ];
    args.extend(policy.args());
    let mut signalled = false;
    let run = run_tool(tool, env, &args, data, &mut |line| {
        if !signalled && mutating_progress(line) {
            signalled = true;
            on_change_started();
        }
    })
    .await?;
    let outcome = match run.result("local_apply")? {
        ToolResult::Completed(fields) => match fields.get("outcome").map(String::as_str) {
            Some("no_op") => ApplyOutcome::NoOp,
            Some("migrated") => ApplyOutcome::Migrated,
            _ => return Err(run.unrecognized("apply")),
        },
        ToolResult::Failed(fields) => {
            return Err(run.local_failure(fields, CliCommand::LocalApply, None));
        }
    };
    if outcome == ApplyOutcome::Migrated {
        remove_legacy_gate_files(data);
    }
    Ok(outcome)
}

/// Runs `local restore` for a backup the tool reported earlier.
pub async fn run_local_restore(
    tool: &Path,
    env: &MigrationEnvironment,
    backup: &Path,
    data: &DataPaths,
) -> AppResult<()> {
    let args: Vec<OsString> = vec![
        "local".into(),
        "restore".into(),
        "--backup".into(),
        backup.into(),
        "--maintenance-window-ack".into(),
    ];
    let run = run_tool(tool, env, &args, data, &mut |_| {}).await?;
    match run.result("local_restore")? {
        ToolResult::Completed(fields)
            if fields.get("next_action").map(String::as_str) == Some("apply") =>
        {
            Ok(())
        }
        ToolResult::Completed(_) => Err(run.unrecognized("restore")),
        ToolResult::Failed(fields) => Err(run.local_failure(
            fields,
            CliCommand::LocalRestore,
            Some(backup.display().to_string()),
        )),
    }
}

/// The receipt and attempt journal of the removed startup gate. They lost
/// their meaning once Memories took over migration state, so they are
/// dropped after a migration actually ran instead of on every startup.
fn remove_legacy_gate_files(data: &DataPaths) {
    for path in [
        data.database_migration_receipt_path(),
        data.database_migration_attempt_path(),
    ] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                warn!(%error, path = %path.display(), "could not remove legacy migration file")
            }
        }
    }
}

pub(super) struct ToolRun {
    pub(super) status: ExitStatus,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

impl ToolRun {
    /// A success line is trusted only together with a successful exit, and
    /// a failure line only together with a failing one.
    fn result(&self, token: &str) -> AppResult<ToolResult> {
        match final_result(&self.stdout, token) {
            Some(result @ ToolResult::Completed(_)) if self.status.success() => Ok(result),
            Some(result @ ToolResult::Failed(_)) if !self.status.success() => Ok(result),
            _ => Err(self.unrecognized("result")),
        }
    }

    pub(super) fn detail(&self) -> String {
        self.stderr
            .lines()
            .take(DETAIL_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn unrecognized(&self, stage: &str) -> AppError {
        AppError::DatabaseMigrationFailed {
            stage: stage.into(),
            error_code: TOOL_OUTPUT_UNRECOGNIZED.into(),
            resolution: "retry".into(),
            backup_path: String::new(),
            detail: format!("tool exited with {}: {}", self.status, self.detail()),
        }
    }

    fn failed(
        &self,
        mut fields: BTreeMap<String, String>,
        fallback_backup: Option<String>,
    ) -> AppError {
        let stage = fields.remove("stage").unwrap_or_else(|| "result".into());
        let error_code = fields
            .remove("error_code")
            .unwrap_or_else(|| TOOL_OUTPUT_UNRECOGNIZED.into());
        let resolution = fields
            .remove("resolution")
            .unwrap_or_else(|| "retry".into());
        let backup = fields.remove("backup");
        AppError::DatabaseMigrationFailed {
            stage,
            error_code,
            resolution,
            backup_path: backup.or(fallback_backup).unwrap_or_default(),
            detail: self.detail(),
        }
    }

    fn local_failure(
        &self,
        fields: BTreeMap<String, String>,
        command: CliCommand,
        fallback_backup: Option<String>,
    ) -> AppError {
        if is_embedding_related_failure(&fields) {
            match parse_embedding_failure(command, fields) {
                Ok(failure) => AppError::EmbeddingMigrationFailed {
                    failure: Some(Box::new(failure)),
                    detail: self.detail(),
                    inspect_required: false,
                },
                Err(reason) => AppError::EmbeddingMigrationFailed {
                    failure: None,
                    detail: format!(
                        "unrecognized embedding migration refusal ({reason}): {}",
                        self.detail()
                    ),
                    inspect_required: true,
                },
            }
        } else {
            self.failed(fields, fallback_backup)
        }
    }
}

pub(super) async fn run_tool(
    tool: &Path,
    env: &MigrationEnvironment,
    args: &[OsString],
    data: &DataPaths,
    on_line: &mut (dyn FnMut(&str) + Send),
) -> AppResult<ToolRun> {
    let log_path = data.log_dir().join(format!(
        "database-migration-{}-{}.log",
        Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
        std::process::id()
    ));
    let _log_guard = MigrationLogGuard::new(log_path.clone())?;
    let command_line = args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    append_migration_log(&log_path, &format!("command={command_line} event=start\n"))?;
    let effective_database_url = env
        .extra_env
        .get(std::ffi::OsStr::new("MEMORIES_ATLAS_DATABASE_URL"))
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| env.database_url.clone());
    let mut child = Command::new(tool)
        .args(args)
        .current_dir(tool.parent().unwrap_or_else(|| Path::new(".")))
        .env("MEMORIES_ATLAS_DATABASE_URL", &env.database_url)
        .env(
            "THREAD_VECTOR_ENABLED",
            env.thread_vector_enabled.to_string(),
        )
        .env("MEMORY_FTS_TOKENIZER", &env.memory_fts_tokenizer)
        .env(
            "LANCE_LANGUAGE_MODEL_HOME",
            env.lance_language_model_home.as_os_str(),
        )
        .envs(env.extra_env.iter())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            let _ = append_migration_log(&log_path, &format!("event=spawn_failed error={error}\n"));
            AppError::DatabaseMigrationFailed {
                stage: "start".into(),
                error_code: TOOL_UNAVAILABLE.into(),
                resolution: "tool_update_required".into(),
                backup_path: String::new(),
                detail: format!("{}: {error}", tool.display()),
            }
        })?;

    let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
    let stderr_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buffer).await;
        buffer
    });
    let mut stdout = String::new();
    let mut lines = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();
    while let Some(line) = lines.next_line().await? {
        let line = redact(&line, &effective_database_url);
        append_migration_log(&log_path, &format!("stdout: {line}\n"))?;
        on_line(&line);
        stdout.push_str(&line);
        stdout.push('\n');
    }
    let status = child.wait().await?;
    let stderr = redact(
        &String::from_utf8_lossy(&stderr_task.await.unwrap_or_default()),
        &effective_database_url,
    );
    append_migration_log(
        &log_path,
        &format!("event=exit status={status}\nstderr:\n{stderr}\n"),
    )?;
    Ok(ToolRun {
        status,
        stdout,
        stderr,
    })
}

fn redact(output: &str, database_url: &str) -> String {
    if database_url.is_empty() {
        output.to_string()
    } else {
        output.replace(database_url, "<redacted-database-url>")
    }
}

/// Marks the audit log active while the tool runs so log retention never
/// prunes a log that a crashed run left half written.
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn env() -> MigrationEnvironment {
        MigrationEnvironment {
            database_url: "sqlite:///secret/default.sqlite3?mode=rwc".into(),
            thread_vector_enabled: true,
            memory_fts_tokenizer: "lindera/ipadic".into(),
            lance_language_model_home: "/data/lance_language_models".into(),
            extra_env: BTreeMap::new(),
        }
    }

    /// Writes an executable fake tool that records its arguments and prints
    /// `stdout` before exiting with `code`.
    fn fake_tool(dir: &Path, stdout: &str, code: i32) -> PathBuf {
        let tool = dir.join("memories-db-migrate");
        let args_file = dir.join("args");
        let env_file = dir.join("env");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nenv > '{}'\ncat <<'EOF'\n{stdout}\nEOF\necho diagnostic for $MEMORIES_ATLAS_DATABASE_URL >&2\nexit {code}\n",
            args_file.display(),
            env_file.display()
        );
        fs::write(&tool, script).unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        tool
    }

    fn recorded_env(dir: &Path) -> std::collections::HashMap<String, String> {
        fs::read_to_string(dir.join("env"))
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn recorded_args(dir: &Path) -> Vec<String> {
        fs::read_to_string(dir.join("args"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn data_root() -> (tempfile::TempDir, DataPaths) {
        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("root"));
        fs::create_dir_all(&data.root).unwrap();
        (dir, data)
    }

    fn seed_legacy_artifacts(data: &DataPaths) {
        fs::write(data.database_migration_receipt_path(), "{}").unwrap();
        fs::write(data.database_migration_attempt_path(), "{}").unwrap();
    }

    fn legacy_artifacts_present(data: &DataPaths) -> (bool, bool) {
        (
            data.database_migration_receipt_path().exists(),
            data.database_migration_attempt_path().exists(),
        )
    }

    async fn apply(
        tool: &Path,
        policy: &BackupPolicy,
        data: &DataPaths,
    ) -> (AppResult<ApplyOutcome>, usize) {
        let mut started = 0;
        let result = run_local_apply(tool, &env(), policy, data, &mut || started += 1).await;
        (result, started)
    }

    #[test]
    fn final_result_reads_completed_and_failed_lines() {
        assert_eq!(
            final_result(
                "local_progress stage=plan\nlocal_apply status=completed outcome=no_op\n",
                "local_apply"
            ),
            Some(ToolResult::Completed(BTreeMap::from([
                ("outcome".into(), "no_op".into()),
                ("status".into(), "completed".into()),
            ])))
        );
        assert_eq!(
            final_result(
                "local_apply status=failed stage=writer_check error_code=writer_active resolution=retry",
                "local_apply"
            ),
            Some(ToolResult::Failed(BTreeMap::from([
                ("status".into(), "failed".into()),
                ("stage".into(), "writer_check".into()),
                ("error_code".into(), "writer_active".into()),
                ("resolution".into(), "retry".into()),
            ])))
        );
    }

    #[test]
    fn final_result_percent_decodes_values() {
        assert_eq!(
            final_result(
                "local_apply status=failed stage=preflight error_code=restore_required resolution=restore_required backup=/Users/a%20b/%E6%97%A5%25/x\n\n",
                "local_apply"
            ),
            Some(ToolResult::Failed(BTreeMap::from([
                ("status".into(), "failed".into()),
                ("stage".into(), "preflight".into()),
                ("error_code".into(), "restore_required".into()),
                ("resolution".into(), "restore_required".into()),
                ("backup".into(), "/Users/a b/日%/x".into()),
            ])))
        );
    }

    #[test]
    fn final_result_rejects_missing_or_foreign_last_lines() {
        assert_eq!(final_result("", "local_apply"), None);
        assert_eq!(
            final_result("local_progress stage=post_migrate\n", "local_apply"),
            None
        );
        assert_eq!(
            final_result(
                "local_restore status=completed next_action=apply",
                "local_apply"
            ),
            None
        );
        assert_eq!(
            final_result("local_apply status=failed stage=plan", "local_apply"),
            None,
            "a failure without error_code is not a contract line"
        );
        assert_eq!(
            final_result("local_apply status=completed broken", "local_apply"),
            None
        );
    }

    #[test]
    fn structured_lines_reject_duplicate_keys_and_malformed_percent_encoding() {
        assert_eq!(
            structured_line("local_apply status=completed status=failed"),
            None
        );
        for malformed in ["%", "%2", "%GG", "%E3%28%A1"] {
            assert_eq!(
                structured_line(&format!("local_apply path={malformed}")),
                None,
                "malformed encoded value {malformed:?} must not be accepted"
            );
        }
    }

    #[test]
    fn apply_restore_resolution_requires_a_concrete_backup_without_an_attempt() {
        let base = "embedding_switch status=failed stage=preflight error_code=apply_restore_required resolution=apply_restore_required";
        let (_, fields) = structured_line(base).unwrap();
        assert!(parse_embedding_failure(CliCommand::Switch, fields).is_err());

        for backup in ["", "none", "unknown"] {
            let line = format!("{base} backup={backup}");
            let (_, fields) = structured_line(&line).unwrap();
            assert!(
                parse_embedding_failure(CliCommand::Switch, fields).is_err(),
                "backup={backup:?} is not a concrete restore source"
            );
        }

        let line = format!("{base} backup=/local%20backup");
        let (_, fields) = structured_line(&line).unwrap();
        let failure = parse_embedding_failure(CliCommand::Switch, fields).unwrap();
        assert_eq!(failure.attempt, None);
        assert_eq!(failure.backup.as_deref(), Some("/local backup"));
    }

    #[test]
    fn only_stages_that_can_change_data_count_as_migration_progress() {
        for stage in [
            "backup",
            "baseline",
            "schema_apply",
            "post_migrate",
            "verify",
        ] {
            assert!(mutating_progress(&format!("local_progress stage={stage}")));
        }
        for line in [
            "local_progress stage=preflight",
            "local_progress stage=writer_check",
            "local_progress stage=plan",
            "local_progress stage=retention",
            "post_migrate_run task_identity=x status=completed",
            "local_apply status=completed outcome=migrated",
        ] {
            assert!(!mutating_progress(line), "{line}");
        }
    }

    #[test]
    fn backup_policy_follows_the_user_setting() {
        let data = DataPaths::with_root("/data");
        assert_eq!(
            BackupPolicy::from_settings(MigrationBackupSettings::default(), &data),
            BackupPolicy::Backup {
                dir: data.database_migration_backups_dir(),
                keep: 1,
            }
        );
        assert_eq!(
            BackupPolicy::from_settings(
                MigrationBackupSettings {
                    enabled: false,
                    keep: 3,
                },
                &data
            ),
            BackupPolicy::NoBackupUnsafe
        );
        assert_eq!(
            BackupPolicy::Backup {
                dir: "/data/backups".into(),
                keep: 0,
            }
            .args(),
            vec![
                OsString::from("--backup-dir"),
                "/data/backups".into(),
                "--backup-keep".into(),
                "0".into(),
            ]
        );
        assert_eq!(
            BackupPolicy::NoBackupUnsafe.args(),
            vec![OsString::from("--no-backup-unsafe")]
        );
    }

    #[tokio::test]
    async fn migrated_apply_reports_progress_and_removes_legacy_gate_files() {
        let (dir, data) = data_root();
        seed_legacy_artifacts(&data);
        let tool = fake_tool(
            dir.path(),
            "local_progress stage=preflight\nlocal_progress stage=plan\nlocal_progress stage=backup\nlocal_progress stage=post_migrate\nlocal_apply status=completed outcome=migrated backup=/b/1",
            0,
        );
        let policy = BackupPolicy::Backup {
            dir: "/b".into(),
            keep: 2,
        };

        let (result, started) = apply(&tool, &policy, &data).await;

        assert_eq!(result.unwrap(), ApplyOutcome::Migrated);
        assert_eq!(started, 1, "the activity signal starts once");
        assert_eq!(legacy_artifacts_present(&data), (false, false));
        assert_eq!(
            recorded_args(dir.path()),
            [
                "local",
                "apply",
                "--maintenance-window-ack",
                "--backup-dir",
                "/b",
                "--backup-keep",
                "2"
            ]
        );
    }

    #[tokio::test]
    async fn run_tool_does_not_inject_thread_vector_env() {
        let (dir, data) = data_root();
        let tool = fake_tool(dir.path(), "local_apply status=completed outcome=no_op", 0);

        run_local_apply(
            &tool,
            &env(),
            &BackupPolicy::NoBackupUnsafe,
            &data,
            &mut || {},
        )
        .await
        .unwrap();

        // Only inherited process values may reach the tool, never app defaults.
        let recorded = recorded_env(dir.path());
        for key in [
            "THREAD_LANCEDB_URI",
            "THREAD_LANCEDB_TABLE",
            "THREAD_VECTOR_SIZE",
        ] {
            assert_eq!(recorded.get(key).cloned(), std::env::var(key).ok(), "{key}");
        }
    }

    #[tokio::test]
    async fn no_op_apply_neither_signals_migration_nor_touches_legacy_files() {
        let (dir, data) = data_root();
        seed_legacy_artifacts(&data);
        let tool = fake_tool(
            dir.path(),
            "local_progress stage=preflight\nlocal_progress stage=writer_check\nlocal_progress stage=plan\nlocal_apply status=completed outcome=no_op",
            0,
        );

        let (result, started) = apply(&tool, &BackupPolicy::NoBackupUnsafe, &data).await;

        assert_eq!(result.unwrap(), ApplyOutcome::NoOp);
        assert_eq!(started, 0);
        assert_eq!(legacy_artifacts_present(&data), (true, true));
        assert_eq!(
            recorded_args(dir.path()),
            [
                "local",
                "apply",
                "--maintenance-window-ack",
                "--no-backup-unsafe"
            ]
        );
    }

    #[tokio::test]
    async fn migrated_apply_succeeds_without_legacy_files() {
        let (dir, data) = data_root();
        let tool = fake_tool(
            dir.path(),
            "local_apply status=completed outcome=migrated",
            0,
        );

        let (result, _) = apply(&tool, &BackupPolicy::NoBackupUnsafe, &data).await;

        assert_eq!(result.unwrap(), ApplyOutcome::Migrated);
    }

    #[tokio::test]
    async fn failed_apply_carries_the_tool_classification_and_keeps_legacy_files() {
        let (dir, data) = data_root();
        seed_legacy_artifacts(&data);
        let tool = fake_tool(
            dir.path(),
            "local_progress stage=preflight\nlocal_apply status=failed stage=preflight error_code=restore_required resolution=restore_required backup=/b/old%201",
            1,
        );

        let (result, _) = apply(&tool, &BackupPolicy::NoBackupUnsafe, &data).await;

        match result.unwrap_err() {
            AppError::DatabaseMigrationFailed {
                stage,
                error_code,
                resolution,
                backup_path,
                detail,
            } => {
                assert_eq!(stage, "preflight");
                assert_eq!(error_code, "restore_required");
                assert_eq!(resolution, "restore_required");
                assert_eq!(backup_path, "/b/old 1");
                assert!(detail.contains("diagnostic for"));
                assert!(
                    !detail.contains("/secret/default.sqlite3"),
                    "the database URL must not leak into the UI: {detail}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(legacy_artifacts_present(&data), (true, true));
    }

    #[tokio::test]
    async fn local_apply_embedding_refusal_retains_attempt_stage_mode_and_backup() {
        let (dir, data) = data_root();
        let tool = fake_tool(
            dir.path(),
            "local_apply status=failed stage=preflight error_code=embedding_migration_in_progress resolution=continue_or_restore attempt=try%201 attempt_stage=rebuild_pending backup_mode=backup backup=/embedding%20backup",
            1,
        );

        let error = apply(&tool, &BackupPolicy::NoBackupUnsafe, &data)
            .await
            .0
            .unwrap_err();

        match error {
            AppError::EmbeddingMigrationFailed {
                failure: Some(failure),
                inspect_required,
                ..
            } => {
                assert!(!inspect_required);
                assert_eq!(failure.command, CliCommand::LocalApply);
                assert_eq!(
                    failure.error_code,
                    CliErrorCode::EmbeddingMigrationInProgress
                );
                assert_eq!(failure.resolution, CliResolution::ContinueOrRestore);
                let attempt = failure.attempt.unwrap();
                assert_eq!(attempt.attempt, "try 1");
                assert_eq!(attempt.stage, AttemptStage::RebuildPending);
                assert_eq!(attempt.backup_mode, BackupMode::Backup);
                assert_eq!(failure.backup.as_deref(), Some("/embedding backup"));
            }
            other => panic!("embedding recovery metadata was lost: {other:?}"),
        }
    }

    #[tokio::test]
    async fn local_restore_does_not_use_schema_backup_as_embedding_migration_backup() {
        let (dir, data) = data_root();
        let tool = fake_tool(
            dir.path(),
            "local_restore status=failed stage=preflight error_code=embedding_migration_in_progress resolution=continue_or_abandon attempt=a1 attempt_stage=replace backup_mode=none",
            1,
        );

        let error = run_local_restore(&tool, &env(), Path::new("/schema-backup"), &data)
            .await
            .unwrap_err();

        match error {
            AppError::EmbeddingMigrationFailed {
                failure: Some(failure),
                ..
            } => {
                assert_eq!(failure.resolution, CliResolution::ContinueOrAbandon);
                assert_eq!(
                    failure.attempt.as_ref().unwrap().backup_mode,
                    BackupMode::None
                );
                assert_eq!(failure.backup, None);
            }
            other => panic!("unexpected embedding refusal shape: {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_result_line_is_an_unrecognized_failure() {
        let (dir, data) = data_root();
        let tool = fake_tool(dir.path(), "local_progress stage=post_migrate", 101);

        let (result, _) = apply(&tool, &BackupPolicy::NoBackupUnsafe, &data).await;

        match result.unwrap_err() {
            AppError::DatabaseMigrationFailed {
                error_code,
                resolution,
                ..
            } => {
                assert_eq!(error_code, TOOL_OUTPUT_UNRECOGNIZED);
                assert_eq!(resolution, "retry");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn success_line_with_failing_exit_status_is_not_trusted() {
        let (dir, data) = data_root();
        let tool = fake_tool(dir.path(), "local_apply status=completed outcome=no_op", 1);

        let (result, _) = apply(&tool, &BackupPolicy::NoBackupUnsafe, &data).await;

        assert!(matches!(
            result,
            Err(AppError::DatabaseMigrationFailed { ref error_code, .. })
                if error_code == TOOL_OUTPUT_UNRECOGNIZED
        ));
    }

    #[tokio::test]
    async fn missing_tool_is_reported_as_unavailable() {
        let (dir, data) = data_root();

        let (result, _) = apply(
            &dir.path().join("absent"),
            &BackupPolicy::NoBackupUnsafe,
            &data,
        )
        .await;

        assert!(matches!(
            result,
            Err(AppError::DatabaseMigrationFailed { ref error_code, .. })
                if error_code == TOOL_UNAVAILABLE
        ));
    }

    #[tokio::test]
    async fn apply_writes_a_redacted_closed_audit_log() {
        let (dir, data) = data_root();
        let tool = fake_tool(dir.path(), "local_apply status=completed outcome=no_op", 0);

        apply(&tool, &BackupPolicy::NoBackupUnsafe, &data)
            .await
            .0
            .unwrap();

        let logs: Vec<_> = fs::read_dir(data.log_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        let log = logs
            .iter()
            .find(|name| name.starts_with("database-migration-") && name.ends_with(".log"))
            .expect("audit log");
        assert!(logs.contains(&format!("{log}.closed")), "{logs:?}");
        let body = fs::read_to_string(data.log_dir().join(log)).unwrap();
        assert!(body.contains("local_apply status=completed outcome=no_op"));
        assert!(!body.contains("/secret/default.sqlite3"), "{body}");
    }

    #[tokio::test]
    async fn restore_passes_the_backup_and_accepts_the_completion_line() {
        let (dir, data) = data_root();
        let tool = fake_tool(
            dir.path(),
            "local_restore status=completed next_action=apply",
            0,
        );

        run_local_restore(&tool, &env(), Path::new("/b/memories-backup-1"), &data)
            .await
            .unwrap();

        assert_eq!(
            recorded_args(dir.path()),
            [
                "local",
                "restore",
                "--backup",
                "/b/memories-backup-1",
                "--maintenance-window-ack"
            ]
        );
    }

    #[tokio::test]
    async fn failed_restore_reports_the_tool_error_code() {
        let (dir, data) = data_root();
        let tool = fake_tool(
            dir.path(),
            "local_restore status=failed stage=restore_validate error_code=backup_incomplete resolution=retry",
            1,
        );

        let error = run_local_restore(&tool, &env(), Path::new("/b/1"), &data)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AppError::DatabaseMigrationFailed { ref stage, ref error_code, ref backup_path, .. }
                if stage == "restore_validate"
                    && error_code == "backup_incomplete"
                    && backup_path == "/b/1"
        ));
    }
}
