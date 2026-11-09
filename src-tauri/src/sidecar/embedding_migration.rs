use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::data::DataPaths;
use crate::error::{AppError, AppResult};
use crate::sidecar::memories_migration::{
    AttemptStage, BackupMode, CliCommand, CliErrorCode, EmbeddingAttemptInfo,
    EmbeddingMigrationFailure, MigrationEnvironment, ToolRun, parse_embedding_failure, run_tool,
    structured_line,
};
use serde::{Deserialize, Serialize};

macro_rules! output_vocabulary {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub(crate) enum $name {
            $($variant),+
        }

        impl $name {
            fn parse(value: &str) -> Option<Self> {
                match value {
                    $($value => Some(Self::$variant),)+
                    _ => None,
                }
            }

        }
    };
}

output_vocabulary!(EmbeddingState {
    Consistent => "consistent",
    Incomplete => "incomplete",
    Unverified => "unverified",
    Failed => "failed",
    Unknown => "unknown",
    Migrating => "migrating",
    RebuildInconsistent => "rebuild_inconsistent",
    Unavailable => "unavailable",
});

output_vocabulary!(UnavailableReason {
    ResourceUnavailable => "resource_unavailable",
    ResourceCorrupt => "resource_corrupt",
});

output_vocabulary!(NextAction {
    None => "none",
    Wait => "wait",
    Reconcile => "reconcile",
    ResolveFailed => "resolve_failed",
    Switch => "switch",
    CheckEnvironment => "check_environment",
    ManualRecovery => "manual_recovery",
    Restore => "restore",
    Abandon => "abandon",
    ResumeOrAbandon => "resume_or_abandon",
    ResumeOrRestore => "resume_or_restore",
    StartRebuild => "start_rebuild",
    Finalize => "finalize",
    Apply => "apply",
});

output_vocabulary!(PlanDecision {
    NoChange => "no_change",
    ReconcileRequired => "reconcile_required",
    AdoptOnStart => "adopt_on_start",
    ReembedRequired => "reembed_required",
});

output_vocabulary!(SwitchOutcome {
    Switched => "switched",
    AlreadySwitched => "already_switched",
});

output_vocabulary!(AbandonOutcome {
    Abandoned => "abandoned",
    Discarded => "discarded",
    AlreadyAbandoned => "already_abandoned",
});

output_vocabulary!(FinalizeOutcome {
    Finalized => "finalized",
    AlreadyCompleted => "already_completed",
});

output_vocabulary!(RestoreOutcome {
    Restored => "restored",
    AlreadyRestored => "already_restored",
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum SpaceValue {
    Unknown,
    None,
    Id(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EmbeddingTarget {
    pub model_id: String,
    pub tokenizer_model_id: String,
    pub revision: String,
    pub dimension: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(crate) enum EmbeddingBackup {
    Directory { path: PathBuf, keep: Option<u64> },
    NoBackupUnsafe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub(crate) enum EmbeddingRequest {
    Inspect,
    Plan {
        target: EmbeddingTarget,
    },
    Switch {
        target: EmbeddingTarget,
        expected_space: SpaceValue,
        backup: EmbeddingBackup,
        maintenance_window_ack: bool,
    },
    Resume {
        attempt: String,
    },
    Abandon {
        attempt: String,
    },
    Finalize {
        attempt: String,
        accept_failed: Option<u64>,
    },
    Restore {
        attempt: String,
    },
}

impl EmbeddingRequest {
    fn command(&self) -> CliCommand {
        match self {
            Self::Inspect => CliCommand::Inspect,
            Self::Plan { .. } => CliCommand::Plan,
            Self::Switch { .. } | Self::Resume { .. } => CliCommand::Switch,
            Self::Abandon { .. } => CliCommand::Abandon,
            Self::Finalize { .. } => CliCommand::Finalize,
            Self::Restore { .. } => CliCommand::Restore,
        }
    }

    fn is_mutating(&self) -> bool {
        !matches!(self, Self::Inspect | Self::Plan { .. })
    }

    fn expected_attempt(&self) -> Option<&str> {
        match self {
            Self::Resume { attempt }
            | Self::Abandon { attempt }
            | Self::Finalize { attempt, .. }
            | Self::Restore { attempt } => Some(attempt),
            Self::Inspect | Self::Plan { .. } | Self::Switch { .. } => None,
        }
    }

    fn expected_space(&self) -> Option<&SpaceValue> {
        match self {
            Self::Switch { expected_space, .. } => Some(expected_space),
            _ => None,
        }
    }

    fn args(&self) -> Result<Vec<OsString>, String> {
        let mut args = vec![OsString::from("embedding")];
        match self {
            Self::Inspect => args.push("inspect".into()),
            Self::Plan { target } => {
                args.push("plan".into());
                push_target_args(&mut args, target)?;
            }
            Self::Switch {
                target,
                expected_space,
                backup,
                maintenance_window_ack,
            } => {
                if !maintenance_window_ack {
                    return Err("switch requires maintenance-window acknowledgement".into());
                }
                args.push("switch".into());
                push_target_args(&mut args, target)?;
                let expected_space = space_arg(expected_space)?;
                push_option(&mut args, "--expected-space", &expected_space);
                args.push("--maintenance-window-ack".into());
                match backup {
                    EmbeddingBackup::Directory { path, keep } => {
                        if path.as_os_str().is_empty() {
                            return Err("backup directory must not be empty".into());
                        }
                        push_option(&mut args, "--backup-dir", path.as_os_str());
                        if let Some(keep) = keep {
                            push_option(&mut args, "--backup-keep", OsStr::new(&keep.to_string()));
                        }
                    }
                    EmbeddingBackup::NoBackupUnsafe => {
                        args.push("--no-backup-unsafe".into());
                    }
                }
            }
            Self::Resume { attempt } => {
                require_attempt(attempt)?;
                args.push("switch".into());
                push_option(&mut args, "--resume", OsStr::new(attempt));
            }
            Self::Abandon { attempt } => {
                require_attempt(attempt)?;
                args.push("abandon".into());
                push_option(&mut args, "--attempt", OsStr::new(attempt));
            }
            Self::Finalize {
                attempt,
                accept_failed,
            } => {
                require_attempt(attempt)?;
                args.push("finalize".into());
                push_option(&mut args, "--attempt", OsStr::new(attempt));
                if let Some(count) = accept_failed {
                    push_option(&mut args, "--accept-failed", OsStr::new(&count.to_string()));
                }
            }
            Self::Restore { attempt } => {
                require_attempt(attempt)?;
                args.push("restore".into());
                push_option(&mut args, "--attempt", OsStr::new(attempt));
            }
        }
        Ok(args)
    }
}

fn require_attempt(attempt: &str) -> Result<(), String> {
    if attempt.is_empty() {
        Err("attempt ID must not be empty".into())
    } else {
        Ok(())
    }
}

fn push_target_args(args: &mut Vec<OsString>, target: &EmbeddingTarget) -> Result<(), String> {
    if target.model_id.is_empty() {
        return Err("target model ID must not be empty".into());
    }
    push_option(args, "--model-id", OsStr::new(&target.model_id));
    push_option(
        args,
        "--tokenizer-model-id",
        OsStr::new(&target.tokenizer_model_id),
    );
    push_option(args, "--revision", OsStr::new(&target.revision));
    push_option(
        args,
        "--dimension",
        OsStr::new(&target.dimension.to_string()),
    );
    Ok(())
}

fn push_option(args: &mut Vec<OsString>, name: &str, value: &OsStr) {
    let mut argument = OsString::from(name);
    argument.push("=");
    argument.push(value);
    args.push(argument);
}

fn space_arg(space: &SpaceValue) -> Result<OsString, String> {
    match space {
        SpaceValue::Unknown => Ok("unknown".into()),
        SpaceValue::None => Ok("none".into()),
        SpaceValue::Id(id) if id.is_empty() || matches!(id.as_str(), "unknown" | "none") => {
            Err("expected space ID is empty or reserved".into())
        }
        SpaceValue::Id(id) => Ok(id.into()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EmbeddingInspect {
    pub state: EmbeddingState,
    pub space: SpaceValue,
    pub required: Option<u64>,
    pub complete: Option<u64>,
    pub missing: Option<u64>,
    pub stale: Option<u64>,
    pub unverified: Option<u64>,
    pub failed_transient: Option<u64>,
    pub failed_permanent: Option<u64>,
    pub orphan: Option<u64>,
    pub backup_supported: Option<bool>,
    pub attempt: Option<EmbeddingAttemptInfo>,
    pub backup: Option<String>,
    pub unavailable_reason: Option<UnavailableReason>,
    pub next_action: NextAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EmbeddingPlan {
    pub decision: PlanDecision,
    pub executable: bool,
    pub reason: Option<CliErrorCode>,
    pub current_space: SpaceValue,
    pub target_space: String,
    pub memory_text: u64,
    pub memory_media: u64,
    pub thread: u64,
    pub reflection_intent: u64,
    pub failed_transient: Option<u64>,
    pub failed_permanent: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EmbeddingSwitchSuccess {
    pub outcome: SwitchOutcome,
    pub attempt: String,
    pub source_space: SpaceValue,
    pub target_space: String,
    pub backup: Option<String>,
    pub next_action: NextAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EmbeddingAbandonSuccess {
    pub outcome: AbandonOutcome,
    pub attempt: String,
    pub next_action: NextAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EmbeddingFinalizeSuccess {
    pub outcome: FinalizeOutcome,
    pub attempt: String,
    pub space: String,
    pub accepted_failed: u64,
    pub next_action: NextAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EmbeddingRestoreSuccess {
    pub outcome: RestoreOutcome,
    pub attempt: String,
    pub space: SpaceValue,
    pub next_action: NextAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub(crate) enum EmbeddingMigrationResult {
    Inspect(EmbeddingInspect),
    Plan(EmbeddingPlan),
    Switched(EmbeddingSwitchSuccess),
    Abandoned(EmbeddingAbandonSuccess),
    Finalized(EmbeddingFinalizeSuccess),
    Restored(EmbeddingRestoreSuccess),
    Failed(EmbeddingMigrationFailure),
}

/// Runs a Memories-owned embedding migration operation. Process management,
/// logging, output redaction, and percent decoding are shared with local apply.
pub(crate) async fn run_embedding(
    tool: &Path,
    migration_env: &MigrationEnvironment,
    request: EmbeddingRequest,
    data: &DataPaths,
) -> AppResult<EmbeddingMigrationResult> {
    let expected_command = request.command();
    let expected_attempt = request.expected_attempt().map(str::to_string);
    let expected_space = request.expected_space().cloned();
    let inspect_required = request.is_mutating();
    let args = request
        .args()
        .map_err(|detail| protocol_error(detail, false))?;
    let run = run_tool(tool, migration_env, &args, data, &mut |_| {})
        .await
        .map_err(|error| protocol_error(error.to_string(), inspect_required))?;
    let result = parse_final_output(&run, expected_command)?;
    if let (
        Some(expected_space),
        EmbeddingMigrationResult::Switched(EmbeddingSwitchSuccess { source_space, .. }),
    ) = (&expected_space, &result)
        && expected_space != source_space
    {
        return Err(protocol_error(
            "CLI switch response does not match expected_space".into(),
            true,
        ));
    }
    if let Some(attempt) = expected_attempt.as_deref() {
        let response_attempt = match &result {
            EmbeddingMigrationResult::Switched(success) => Some(success.attempt.as_str()),
            EmbeddingMigrationResult::Abandoned(success) => Some(success.attempt.as_str()),
            EmbeddingMigrationResult::Finalized(success) => Some(success.attempt.as_str()),
            EmbeddingMigrationResult::Restored(success) => Some(success.attempt.as_str()),
            EmbeddingMigrationResult::Failed(failure) => failure
                .attempt
                .as_ref()
                .map(|details| details.attempt.as_str()),
            EmbeddingMigrationResult::Inspect(_) | EmbeddingMigrationResult::Plan(_) => None,
        };
        if response_attempt.is_some_and(|found| found != attempt) {
            return Err(protocol_error(
                "CLI response names a different embedding attempt".into(),
                inspect_required,
            ));
        }
    }
    Ok(result)
}

fn protocol_error(detail: String, inspect_required: bool) -> AppError {
    let detail = if inspect_required {
        format!("{detail}; inspect is required before any mutation is retried")
    } else {
        detail
    };
    AppError::EmbeddingMigrationFailed {
        failure: None,
        detail,
        inspect_required,
    }
}

fn parse_final_output(
    run: &ToolRun,
    expected_command: CliCommand,
) -> AppResult<EmbeddingMigrationResult> {
    let Some(line) = run
        .stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
    else {
        return Err(protocol_error(
            format!(
                "embedding CLI exited without a result line: {}",
                run.detail()
            ),
            expected_command != CliCommand::Inspect && expected_command != CliCommand::Plan,
        ));
    };
    let parsed = parse_result(line.trim(), expected_command.as_str()).map_err(|reason| {
        protocol_error(
            format!(
                "unrecognized embedding CLI output ({reason}): {}",
                run.detail()
            ),
            expected_command != CliCommand::Inspect && expected_command != CliCommand::Plan,
        )
    })?;
    match (&parsed, run.status.success()) {
        (EmbeddingMigrationResult::Failed(_), false)
        | (EmbeddingMigrationResult::Inspect(_), true)
        | (EmbeddingMigrationResult::Plan(_), true)
        | (EmbeddingMigrationResult::Switched(_), true)
        | (EmbeddingMigrationResult::Abandoned(_), true)
        | (EmbeddingMigrationResult::Finalized(_), true)
        | (EmbeddingMigrationResult::Restored(_), true) => Ok(parsed),
        _ => Err(protocol_error(
            format!(
                "embedding CLI exit status disagrees with its final result line ({}): {}",
                run.status,
                run.detail()
            ),
            expected_command != CliCommand::Inspect && expected_command != CliCommand::Plan,
        )),
    }
}

fn parse_result(line: &str, expected_token: &str) -> Result<EmbeddingMigrationResult, String> {
    let (token, mut fields) = structured_line(line)
        .ok_or_else(|| "malformed structured line or percent encoding".to_string())?;
    if token != expected_token {
        return Err(format!("foreign command token {token}"));
    }
    let command =
        CliCommand::parse(token).ok_or_else(|| format!("unknown command token {token}"))?;
    match fields.get("status").map(String::as_str) {
        Some("completed") => {
            fields.remove("status");
            parse_success(command, fields)
        }
        Some("failed") => {
            parse_embedding_failure(command, fields).map(EmbeddingMigrationResult::Failed)
        }
        _ => Err("missing or unknown status".into()),
    }
}

fn parse_success(
    command: CliCommand,
    mut fields: BTreeMap<String, String>,
) -> Result<EmbeddingMigrationResult, String> {
    match command {
        CliCommand::Inspect => parse_inspect(&mut fields).map(EmbeddingMigrationResult::Inspect),
        CliCommand::Plan => parse_plan(&mut fields).map(EmbeddingMigrationResult::Plan),
        CliCommand::Switch => parse_switch(&mut fields).map(EmbeddingMigrationResult::Switched),
        CliCommand::Abandon => parse_abandon(&mut fields).map(EmbeddingMigrationResult::Abandoned),
        CliCommand::Finalize => {
            parse_finalize(&mut fields).map(EmbeddingMigrationResult::Finalized)
        }
        CliCommand::Restore => parse_restore(&mut fields).map(EmbeddingMigrationResult::Restored),
        CliCommand::LocalApply | CliCommand::LocalRestore => {
            Err("local command token is invalid for embedding output".into())
        }
    }
}

fn parse_inspect(fields: &mut BTreeMap<String, String>) -> Result<EmbeddingInspect, String> {
    let state = take_enum::<EmbeddingState>(fields, "state")?;
    let space = take_space(fields, "space")?;
    let required = take_optional_count(fields, "required")?;
    let complete = take_optional_count(fields, "complete")?;
    let missing = take_optional_count(fields, "missing")?;
    let stale = take_optional_count(fields, "stale")?;
    let unverified = take_optional_count(fields, "unverified")?;
    let failed_transient = take_optional_count(fields, "failed_transient")?;
    let failed_permanent = take_optional_count(fields, "failed_permanent")?;
    let orphan = take_optional_count(fields, "orphan")?;
    let backup_supported = match take_required(fields, "backup_supported")?.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        "unknown" => None,
        _ => return Err("invalid backup_supported value".into()),
    };
    let attempt = take_attempt_info(fields)?;
    let backup = fields.remove("backup");
    let unavailable_reason = fields
        .remove("unavailable_reason")
        .map(|value| {
            UnavailableReason::parse(&value)
                .ok_or_else(|| format!("unknown unavailable_reason {value}"))
        })
        .transpose()?;
    if (state == EmbeddingState::Unavailable) != unavailable_reason.is_some() {
        return Err("unavailable_reason must be present only for unavailable state".into());
    }
    let next_action = take_enum(fields, "next_action")?;
    validate_inspect_recovery_fields(
        state,
        unavailable_reason,
        next_action,
        attempt.as_ref(),
        backup.as_deref(),
    )?;
    ensure_empty(fields)?;
    Ok(EmbeddingInspect {
        state,
        space,
        required,
        complete,
        missing,
        stale,
        unverified,
        failed_transient,
        failed_permanent,
        orphan,
        backup_supported,
        attempt,
        backup,
        unavailable_reason,
        next_action,
    })
}

fn validate_inspect_recovery_fields(
    state: EmbeddingState,
    unavailable_reason: Option<UnavailableReason>,
    next_action: NextAction,
    attempt: Option<&EmbeddingAttemptInfo>,
    backup: Option<&str>,
) -> Result<(), String> {
    let valid_state_action = match state {
        EmbeddingState::Consistent => next_action == NextAction::None,
        EmbeddingState::Incomplete | EmbeddingState::Unverified => {
            next_action == NextAction::Reconcile
        }
        EmbeddingState::Failed => next_action == NextAction::ResolveFailed,
        EmbeddingState::Unknown => matches!(
            next_action,
            NextAction::Switch
                | NextAction::Wait
                | NextAction::CheckEnvironment
                | NextAction::ManualRecovery
        ),
        EmbeddingState::Migrating => attempt.is_some_and(|attempt| {
            matches!(
                (attempt.stage, attempt.backup_mode, next_action),
                (AttemptStage::Backup, _, NextAction::ResumeOrAbandon)
                    | (
                        AttemptStage::Replace,
                        BackupMode::Backup,
                        NextAction::ResumeOrRestore
                    )
                    | (
                        AttemptStage::Replace,
                        BackupMode::None,
                        NextAction::ResumeOrAbandon
                    )
                    | (AttemptStage::RebuildPending, _, NextAction::StartRebuild)
                    | (AttemptStage::Commit, _, NextAction::Finalize)
                    | (
                        AttemptStage::Restoring,
                        BackupMode::Backup,
                        NextAction::Restore
                    )
                    | (AttemptStage::Discarding, _, NextAction::Abandon)
            )
        }),
        EmbeddingState::RebuildInconsistent => match next_action {
            NextAction::ManualRecovery => true,
            NextAction::Abandon => {
                attempt.is_some_and(|attempt| attempt.backup_mode == BackupMode::None)
            }
            NextAction::Restore => {
                attempt.is_some_and(|attempt| attempt.backup_mode == BackupMode::Backup)
            }
            _ => false,
        },
        EmbeddingState::Unavailable => match (unavailable_reason, next_action) {
            (_, NextAction::CheckEnvironment | NextAction::ManualRecovery) => true,
            (Some(UnavailableReason::ResourceCorrupt), NextAction::Abandon) => {
                attempt.is_some_and(|attempt| attempt.backup_mode == BackupMode::None)
            }
            (Some(UnavailableReason::ResourceCorrupt), NextAction::Restore) => {
                attempt.is_some_and(|attempt| attempt.backup_mode == BackupMode::Backup)
            }
            _ => false,
        },
    };
    if !valid_state_action {
        return Err("next_action is inconsistent with inspect state and attempt".into());
    }

    let migration_state = matches!(
        state,
        EmbeddingState::Migrating | EmbeddingState::RebuildInconsistent
    );
    let attempt_required = matches!(
        next_action,
        NextAction::ResumeOrAbandon
            | NextAction::Abandon
            | NextAction::ResumeOrRestore
            | NextAction::StartRebuild
            | NextAction::Finalize
            | NextAction::Restore
    ) || (next_action == NextAction::ResolveFailed && migration_state);
    if attempt_required && attempt.is_none() {
        return Err("next_action requires attempt, attempt_stage, and backup_mode".into());
    }

    if matches!(
        next_action,
        NextAction::ResumeOrRestore | NextAction::Restore
    ) {
        if !backup.is_some_and(|path| !path.is_empty() && !matches!(path, "none" | "unknown")) {
            return Err("next_action requires a concrete backup path".into());
        }
        if attempt.is_none_or(|attempt| attempt.backup_mode != BackupMode::Backup) {
            return Err("next_action requires a backup-mode attempt".into());
        }
    }
    Ok(())
}

fn parse_plan(fields: &mut BTreeMap<String, String>) -> Result<EmbeddingPlan, String> {
    let decision = take_enum(fields, "decision")?;
    let executable = match take_required(fields, "executable")?.as_str() {
        "true" => true,
        "false" => false,
        _ => return Err("invalid executable value".into()),
    };
    let reason = fields
        .remove("reason")
        .map(|value| parse_embedding_error_code(&value))
        .transpose()?;
    let current_space = take_space(fields, "current_space")?;
    let target_space = take_required(fields, "target_space")?;
    if target_space.is_empty() || matches!(target_space.as_str(), "unknown" | "none") {
        return Err("target_space must be a concrete space ID".into());
    }
    let memory_text = take_integer(fields, "memory_text")?;
    let memory_media = take_integer(fields, "memory_media")?;
    let thread = take_integer(fields, "thread")?;
    let reflection_intent = take_integer(fields, "reflection_intent")?;
    let failed_transient = take_optional_count(fields, "failed_transient")?;
    let failed_permanent = take_optional_count(fields, "failed_permanent")?;
    ensure_empty(fields)?;
    Ok(EmbeddingPlan {
        decision,
        executable,
        reason,
        current_space,
        target_space,
        memory_text,
        memory_media,
        thread,
        reflection_intent,
        failed_transient,
        failed_permanent,
    })
}

fn parse_switch(fields: &mut BTreeMap<String, String>) -> Result<EmbeddingSwitchSuccess, String> {
    let outcome = take_enum(fields, "outcome")?;
    let attempt = take_identifier(fields, "attempt")?;
    let source_space = take_space(fields, "source_space")?;
    let target_space = take_concrete_identifier(fields, "target_space")?;
    let backup = take_switch_backup(fields)?;
    let next_action = take_enum(fields, "next_action")?;
    if next_action != NextAction::StartRebuild {
        return Err("switch next_action must be start_rebuild".into());
    }
    ensure_empty(fields)?;
    Ok(EmbeddingSwitchSuccess {
        outcome,
        attempt,
        source_space,
        target_space,
        backup,
        next_action,
    })
}

fn parse_abandon(fields: &mut BTreeMap<String, String>) -> Result<EmbeddingAbandonSuccess, String> {
    let outcome = take_enum(fields, "outcome")?;
    let attempt = take_identifier(fields, "attempt")?;
    let next_action = take_enum(fields, "next_action")?;
    let valid_action = match outcome {
        AbandonOutcome::Abandoned => next_action == NextAction::None,
        AbandonOutcome::Discarded => next_action == NextAction::Reconcile,
        AbandonOutcome::AlreadyAbandoned => {
            matches!(next_action, NextAction::None | NextAction::Reconcile)
        }
    };
    if !valid_action {
        return Err("abandon outcome and next_action disagree".into());
    }
    ensure_empty(fields)?;
    Ok(EmbeddingAbandonSuccess {
        outcome,
        attempt,
        next_action,
    })
}

fn parse_finalize(
    fields: &mut BTreeMap<String, String>,
) -> Result<EmbeddingFinalizeSuccess, String> {
    let outcome = take_enum(fields, "outcome")?;
    let attempt = take_identifier(fields, "attempt")?;
    let space = take_concrete_identifier(fields, "space")?;
    let accepted_failed = take_integer(fields, "accepted_failed")?;
    let next_action = take_enum(fields, "next_action")?;
    if next_action != NextAction::Apply {
        return Err("finalize next_action must be apply".into());
    }
    ensure_empty(fields)?;
    Ok(EmbeddingFinalizeSuccess {
        outcome,
        attempt,
        space,
        accepted_failed,
        next_action,
    })
}

fn parse_restore(fields: &mut BTreeMap<String, String>) -> Result<EmbeddingRestoreSuccess, String> {
    let outcome = take_enum(fields, "outcome")?;
    let attempt = take_identifier(fields, "attempt")?;
    let space = take_space(fields, "space")?;
    let next_action = take_enum(fields, "next_action")?;
    if next_action != NextAction::Reconcile {
        return Err("restore next_action must be reconcile".into());
    }
    ensure_empty(fields)?;
    Ok(EmbeddingRestoreSuccess {
        outcome,
        attempt,
        space,
        next_action,
    })
}

fn take_attempt_info(
    fields: &mut BTreeMap<String, String>,
) -> Result<Option<EmbeddingAttemptInfo>, String> {
    let attempt = fields.remove("attempt");
    let stage = fields
        .remove("attempt_stage")
        .map(|value| AttemptStage::parse(&value).ok_or_else(|| "unknown attempt_stage".to_string()))
        .transpose()?;
    let backup_mode = fields
        .remove("backup_mode")
        .map(|value| BackupMode::parse(&value).ok_or_else(|| "unknown backup_mode".to_string()))
        .transpose()?;
    match (attempt, stage, backup_mode) {
        (Some(attempt), Some(stage), Some(backup_mode)) if !attempt.is_empty() => {
            Ok(Some(EmbeddingAttemptInfo {
                attempt,
                stage,
                backup_mode,
            }))
        }
        (None, None, None) => Ok(None),
        _ => Err("attempt, attempt_stage, and backup_mode must appear together".into()),
    }
}

fn take_space(fields: &mut BTreeMap<String, String>, name: &str) -> Result<SpaceValue, String> {
    match take_required(fields, name)?.as_str() {
        "unknown" => Ok(SpaceValue::Unknown),
        "none" => Ok(SpaceValue::None),
        "" => Err(format!("{name} must not be empty")),
        value => Ok(SpaceValue::Id(value.to_string())),
    }
}

fn take_identifier(fields: &mut BTreeMap<String, String>, name: &str) -> Result<String, String> {
    let value = take_required(fields, name)?;
    if value.is_empty() {
        Err(format!("{name} must not be empty"))
    } else {
        Ok(value)
    }
}

fn take_concrete_identifier(
    fields: &mut BTreeMap<String, String>,
    name: &str,
) -> Result<String, String> {
    let value = take_identifier(fields, name)?;
    if matches!(value.as_str(), "none" | "unknown") {
        Err(format!("{name} must be a concrete identifier"))
    } else {
        Ok(value)
    }
}

fn take_switch_backup(fields: &mut BTreeMap<String, String>) -> Result<Option<String>, String> {
    let backup = take_required(fields, "backup")?;
    match backup.as_str() {
        "none" => Ok(None),
        "" | "unknown" => Err("backup must be `none` or a concrete path".into()),
        _ => Ok(Some(backup)),
    }
}

fn take_required(fields: &mut BTreeMap<String, String>, name: &str) -> Result<String, String> {
    fields
        .remove(name)
        .ok_or_else(|| format!("missing required field {name}"))
}

fn take_integer(fields: &mut BTreeMap<String, String>, name: &str) -> Result<u64, String> {
    let value = take_required(fields, name)?;
    parse_unsigned(&value).ok_or_else(|| format!("invalid integer field {name}"))
}

fn take_optional_count(
    fields: &mut BTreeMap<String, String>,
    name: &str,
) -> Result<Option<u64>, String> {
    let value = take_required(fields, name)?;
    if value == "unknown" {
        Ok(None)
    } else {
        parse_unsigned(&value)
            .map(Some)
            .ok_or_else(|| format!("invalid count field {name}"))
    }
}

fn parse_unsigned(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn take_enum<T>(fields: &mut BTreeMap<String, String>, name: &str) -> Result<T, String>
where
    T: ParseOutputEnum,
{
    let value = take_required(fields, name)?;
    T::parse_output(&value).ok_or_else(|| format!("unknown {name} value {value}"))
}

trait ParseOutputEnum: Sized {
    fn parse_output(value: &str) -> Option<Self>;
}

macro_rules! impl_output_enum_parser {
    ($($name:ty),+ $(,)?) => {
        $(impl ParseOutputEnum for $name {
            fn parse_output(value: &str) -> Option<Self> {
                Self::parse(value)
            }
        })+
    };
}

impl_output_enum_parser!(
    EmbeddingState,
    NextAction,
    PlanDecision,
    SwitchOutcome,
    AbandonOutcome,
    FinalizeOutcome,
    RestoreOutcome,
);

fn parse_embedding_error_code(value: &str) -> Result<CliErrorCode, String> {
    let code = CliErrorCode::parse(value).ok_or_else(|| format!("unknown error code {value}"))?;
    if matches!(
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
    ) {
        Err(format!("non-embedding error code {value}"))
    } else {
        Ok(code)
    }
}

fn ensure_empty(fields: &BTreeMap<String, String>) -> Result<(), String> {
    if fields.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "unexpected result fields: {}",
            fields.keys().cloned().collect::<Vec<_>>().join(", ")
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::sidecar::memories_migration::{CliResolution, CliStage};
    use std::os::unix::fs::PermissionsExt;

    const INSPECT_WITH_UNKNOWN_COUNTS: &str = concat!(
        "embedding_inspect status=completed state=migrating space=unknown ",
        "required=unknown complete=0 missing=unknown stale=unknown unverified=unknown ",
        "failed_transient=unknown failed_permanent=0 orphan=unknown backup_supported=false ",
        "attempt=attempt%201 attempt_stage=rebuild_pending backup_mode=backup ",
        "backup=/backup%20dir/old next_action=start_rebuild"
    );
    const SWITCH_SUCCESS_WITH_SHELL_METACHARS: &str = concat!(
        "embedding_switch status=completed outcome=already_switched ",
        "attempt=attempt%3B%20touch%20injected-file source_space=source ",
        "target_space=target backup=none next_action=start_rebuild"
    );

    fn data_root() -> (tempfile::TempDir, DataPaths) {
        let dir = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(dir.path().join("root"));
        std::fs::create_dir_all(&data.root).unwrap();
        (dir, data)
    }

    fn env(database_url: &str) -> MigrationEnvironment {
        MigrationEnvironment {
            database_url: database_url.into(),
            thread_vector_enabled: true,
            memory_fts_tokenizer: "lindera/ipadic".into(),
            lance_language_model_home: "/data/lance_language_models".into(),
            extra_env: BTreeMap::from([(
                OsString::from("MEMORY_EMBEDDING_MODEL_REVISION"),
                OsString::from("model-revision-42"),
            )]),
        }
    }

    fn fake_tool(dir: &Path, stdout: &str, code: i32, stderr: &str) -> PathBuf {
        let tool = dir.join("memories-db-migrate");
        let args_file = dir.join("args");
        let env_file = dir.join("revision");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' \"$MEMORY_EMBEDDING_MODEL_REVISION\" > '{}'\ncat <<'EOF'\n{}\nEOF\nprintf '%s\\n' '{}' >&2\nexit {}\n",
            args_file.display(),
            env_file.display(),
            stdout,
            stderr,
            code,
        );
        std::fs::write(&tool, script).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        tool
    }

    fn recorded_args(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("args"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn inspect_fixture(
        state: &str,
        next_action: &str,
        attempt_fields: &str,
        backup: Option<&str>,
    ) -> String {
        inspect_fixture_with_reason(state, next_action, attempt_fields, backup, None)
    }

    fn inspect_fixture_with_reason(
        state: &str,
        next_action: &str,
        attempt_fields: &str,
        backup: Option<&str>,
        unavailable_reason: Option<&str>,
    ) -> String {
        let backup = backup
            .map(|path| format!(" backup={path}"))
            .unwrap_or_default();
        let unavailable_reason = unavailable_reason
            .map(|reason| format!(" unavailable_reason={reason}"))
            .unwrap_or_default();
        format!(
            "embedding_inspect status=completed state={state} space=unknown required=unknown complete=unknown missing=unknown stale=unknown unverified=unknown failed_transient=unknown failed_permanent=unknown orphan=unknown backup_supported=unknown {attempt_fields}{backup}{unavailable_reason} next_action={next_action}"
        )
    }

    #[test]
    fn inspect_fixture_preserves_unknown_zero_attempt_backup_and_action() {
        let result = parse_result(INSPECT_WITH_UNKNOWN_COUNTS, "embedding_inspect").unwrap();
        let EmbeddingMigrationResult::Inspect(inspect) = result else {
            panic!("expected inspect result, got {result:?}");
        };

        assert_eq!(inspect.state, EmbeddingState::Migrating);
        assert_eq!(inspect.space, SpaceValue::Unknown);
        assert_eq!(inspect.required, None);
        assert_eq!(inspect.complete, Some(0));
        assert_eq!(inspect.failed_permanent, Some(0));
        assert_eq!(inspect.backup_supported, Some(false));
        assert_eq!(inspect.attempt.as_ref().unwrap().attempt, "attempt 1");
        assert_eq!(
            inspect.attempt.as_ref().unwrap().stage,
            AttemptStage::RebuildPending
        );
        assert_eq!(inspect.backup.as_deref(), Some("/backup dir/old"));
        assert_eq!(inspect.next_action, NextAction::StartRebuild);
    }

    #[test]
    fn plan_fixture_preserves_reason_spaces_and_all_counts() {
        let line = concat!(
            "embedding_plan status=completed decision=reembed_required executable=false ",
            "reason=resource_unavailable current_space=none target_space=target%20id ",
            "memory_text=0 memory_media=2 thread=3 reflection_intent=4 ",
            "failed_transient=unknown failed_permanent=5"
        );
        let result = parse_result(line, "embedding_plan").unwrap();
        let EmbeddingMigrationResult::Plan(plan) = result else {
            panic!("expected plan result, got {result:?}");
        };

        assert_eq!(plan.decision, PlanDecision::ReembedRequired);
        assert!(!plan.executable);
        assert_eq!(plan.reason, Some(CliErrorCode::ResourceUnavailable));
        assert_eq!(plan.current_space, SpaceValue::None);
        assert_eq!(plan.target_space, "target id");
        assert_eq!(plan.memory_text, 0);
        assert_eq!(plan.failed_transient, None);
        assert_eq!(plan.failed_permanent, Some(5));
    }

    #[test]
    fn mutation_success_fixture_preserves_outcome_backup_and_spaces() {
        let line = concat!(
            "embedding_switch status=completed outcome=switched attempt=attempt%201 ",
            "source_space=unknown target_space=target%20space backup=none ",
            "next_action=start_rebuild"
        );
        let result = parse_result(line, "embedding_switch").unwrap();
        let EmbeddingMigrationResult::Switched(success) = result else {
            panic!("expected switch result, got {result:?}");
        };

        assert_eq!(success.outcome, SwitchOutcome::Switched);
        assert_eq!(success.attempt, "attempt 1");
        assert_eq!(success.source_space, SpaceValue::Unknown);
        assert_eq!(success.target_space, "target space");
        assert_eq!(success.backup, None);
        assert_eq!(success.next_action, NextAction::StartRebuild);
    }

    #[test]
    fn failed_finalize_fixture_preserves_recovery_and_verification_counts() {
        let line = concat!(
            "embedding_finalize status=failed stage=verify error_code=rebuild_incomplete ",
            "resolution=resume_rebuild attempt=a%201 attempt_stage=rebuild_pending ",
            "backup_mode=backup backup=/backup%20dir ",
            "missing_memory_text=1 stale_memory_text=2 unverified_memory_text=3 ",
            "failed_memory_text=4 missing_memory_media=5 stale_memory_media=6 ",
            "unverified_memory_media=7 failed_memory_media=8 missing_thread=9 ",
            "stale_thread=10 unverified_thread=11 failed_thread=12 ",
            "missing_reflection_intent=13 stale_reflection_intent=14 ",
            "unverified_reflection_intent=15 failed_reflection_intent=16 orphan=17"
        );
        let result = parse_result(line, "embedding_finalize").unwrap();
        let EmbeddingMigrationResult::Failed(failure) = result else {
            panic!("expected failure, got {result:?}");
        };

        assert_eq!(failure.stage, CliStage::Verify);
        assert_eq!(failure.error_code, CliErrorCode::RebuildIncomplete);
        assert_eq!(failure.resolution, CliResolution::ResumeRebuild);
        assert_eq!(failure.attempt.as_ref().unwrap().attempt, "a 1");
        assert_eq!(
            failure.attempt.as_ref().unwrap().backup_mode,
            BackupMode::Backup
        );
        assert_eq!(failure.backup.as_deref(), Some("/backup dir"));
        assert_eq!(failure.verification_counts.as_ref().unwrap().orphan, 17);
        assert_eq!(
            failure
                .verification_counts
                .as_ref()
                .unwrap()
                .failed_reflection_intent,
            16
        );
    }

    #[test]
    fn parser_rejects_foreign_tokens_unknown_enums_duplicate_keys_bad_numbers_and_encoding() {
        let invalid = [
            (
                "foreign status=completed state=consistent space=none required=0 complete=0 missing=0 stale=0 unverified=0 failed_transient=0 failed_permanent=0 orphan=0 backup_supported=false next_action=none",
                "embedding_inspect",
            ),
            (
                "embedding_inspect status=completed state=maybe space=none required=0 complete=0 missing=0 stale=0 unverified=0 failed_transient=0 failed_permanent=0 orphan=0 backup_supported=false next_action=none",
                "embedding_inspect",
            ),
            (
                "embedding_inspect status=completed state=consistent space=none required=0 required=1 complete=0 missing=0 stale=0 unverified=0 failed_transient=0 failed_permanent=0 orphan=0 backup_supported=false next_action=none",
                "embedding_inspect",
            ),
            (
                "embedding_inspect status=completed state=consistent space=none required=not-a-number complete=0 missing=0 stale=0 unverified=0 failed_transient=0 failed_permanent=0 orphan=0 backup_supported=false next_action=none",
                "embedding_inspect",
            ),
            (
                "embedding_inspect status=completed state=consistent space=none required=+1 complete=0 missing=0 stale=0 unverified=0 failed_transient=0 failed_permanent=0 orphan=0 backup_supported=false next_action=none",
                "embedding_inspect",
            ),
            (
                "embedding_inspect status=completed state=consistent space=bad%QQ required=0 complete=0 missing=0 stale=0 unverified=0 failed_transient=0 failed_permanent=0 orphan=0 backup_supported=false next_action=none",
                "embedding_inspect",
            ),
            (
                "embedding_switch status=completed outcome=future_outcome attempt=a source_space=none target_space=t backup=none next_action=start_rebuild",
                "embedding_switch",
            ),
            (
                "embedding_finalize status=failed stage=verify error_code=rebuild_incomplete resolution=resume_rebuild",
                "embedding_finalize",
            ),
            (
                "embedding_switch status=failed stage=preflight error_code=attempt_not_found resolution=future_resolution",
                "embedding_switch",
            ),
        ];
        for (line, expected_token) in invalid {
            assert!(
                parse_result(line, expected_token).is_err(),
                "invalid contract line accepted: {line}"
            );
        }
    }

    #[test]
    fn only_the_last_nonempty_line_is_parsed() {
        let line = format!("{INSPECT_WITH_UNKNOWN_COUNTS}\n\nembedding_progress stage=backup\n");
        assert!(parse_result(&line, "embedding_inspect").is_err());
    }

    #[test]
    fn inspect_recovery_actions_require_attempt_fields() {
        for action in [
            "resume_or_abandon",
            "abandon",
            "resume_or_restore",
            "start_rebuild",
            "finalize",
            "restore",
        ] {
            let malformed = inspect_fixture("migrating", action, "", None);
            assert!(
                parse_result(&malformed, "embedding_inspect").is_err(),
                "{action} without attempt metadata must be rejected"
            );

            let (attempt_fields, backup) = match action {
                "resume_or_abandon" => ("attempt=a1 attempt_stage=backup backup_mode=backup", None),
                "abandon" => ("attempt=a1 attempt_stage=discarding backup_mode=none", None),
                "resume_or_restore" => (
                    "attempt=a1 attempt_stage=replace backup_mode=backup",
                    Some("/backup%20dir"),
                ),
                "start_rebuild" => (
                    "attempt=a1 attempt_stage=rebuild_pending backup_mode=none",
                    None,
                ),
                "finalize" => ("attempt=a1 attempt_stage=commit backup_mode=backup", None),
                "restore" => (
                    "attempt=a1 attempt_stage=restoring backup_mode=backup",
                    Some("/backup%20dir"),
                ),
                _ => unreachable!(),
            };
            let valid = inspect_fixture("migrating", action, attempt_fields, backup);
            assert!(
                parse_result(&valid, "embedding_inspect").is_ok(),
                "valid metadata for {action} must be accepted"
            );

            for incomplete_attempt in [
                "attempt=a1",
                "attempt=a1 attempt_stage=replace",
                "attempt=a1 backup_mode=backup",
                "attempt= attempt_stage=replace backup_mode=backup",
            ] {
                let malformed = inspect_fixture("migrating", action, incomplete_attempt, backup);
                assert!(
                    parse_result(&malformed, "embedding_inspect").is_err(),
                    "{action} with incomplete attempt fields {incomplete_attempt:?} must be rejected"
                );
            }
        }

        let orphaned_markers_without_attempt =
            inspect_fixture("rebuild_inconsistent", "abandon", "", None);
        assert!(
            parse_result(&orphaned_markers_without_attempt, "embedding_inspect").is_err(),
            "an inconsistent rebuild cannot recommend abandon without attempt metadata"
        );
    }

    #[test]
    fn inspect_restore_actions_require_a_concrete_backup() {
        for (action, attempt_fields) in [
            (
                "resume_or_restore",
                "attempt=a1 attempt_stage=replace backup_mode=backup",
            ),
            (
                "restore",
                "attempt=a1 attempt_stage=restoring backup_mode=backup",
            ),
        ] {
            for backup in [None, Some(""), Some("none"), Some("unknown")] {
                let malformed = inspect_fixture("migrating", action, attempt_fields, backup);
                assert!(
                    parse_result(&malformed, "embedding_inspect").is_err(),
                    "{action} with backup {backup:?} must be rejected"
                );
            }
            let valid = inspect_fixture("migrating", action, attempt_fields, Some("/backup%20dir"));
            assert!(parse_result(&valid, "embedding_inspect").is_ok());

            let no_backup_attempt = "attempt=a1 attempt_stage=replace backup_mode=none";
            let invalid_mode = inspect_fixture(
                "migrating",
                action,
                no_backup_attempt,
                Some("/backup%20dir"),
            );
            assert!(
                parse_result(&invalid_mode, "embedding_inspect").is_err(),
                "{action} cannot claim restore availability for a no-backup attempt"
            );
        }
    }

    #[test]
    fn resolve_failed_is_the_ordinary_failed_action_and_needs_no_attempt() {
        let ordinary_failure = inspect_fixture("failed", "resolve_failed", "", None);
        assert!(parse_result(&ordinary_failure, "embedding_inspect").is_ok());

        let inconsistent_without_attempt =
            inspect_fixture("rebuild_inconsistent", "manual_recovery", "", None);
        assert!(parse_result(&inconsistent_without_attempt, "embedding_inspect").is_ok());

        for migration_state in ["migrating", "rebuild_inconsistent"] {
            let missing_attempt = inspect_fixture(migration_state, "resolve_failed", "", None);
            assert!(
                parse_result(&missing_attempt, "embedding_inspect").is_err(),
                "{migration_state} has a different recovery action from resolve_failed"
            );

            let attempt_fields = "attempt=a1 attempt_stage=rebuild_pending backup_mode=none";
            let with_attempt =
                inspect_fixture(migration_state, "resolve_failed", attempt_fields, None);
            assert!(
                parse_result(&with_attempt, "embedding_inspect").is_err(),
                "migration recovery action cannot be inferred from resolve_failed"
            );
        }
    }

    #[test]
    fn inspect_state_and_action_table_accepts_contract_combinations() {
        let valid_cases = [
            (
                "consistent with a stale attempt remains non-operational",
                "consistent",
                "none",
                "attempt=stale attempt_stage=commit backup_mode=backup",
                Some("/stale%20backup"),
                None,
            ),
            (
                "incomplete reconciles",
                "incomplete",
                "reconcile",
                "",
                None,
                None,
            ),
            (
                "unverified reconciles",
                "unverified",
                "reconcile",
                "",
                None,
                None,
            ),
            (
                "ordinary failures resolve without an attempt",
                "failed",
                "resolve_failed",
                "",
                None,
                None,
            ),
            (
                "unknown space may switch",
                "unknown",
                "switch",
                "",
                None,
                None,
            ),
            (
                "a higher-priority operation lock waits",
                "unknown",
                "wait",
                "",
                None,
                None,
            ),
            (
                "a higher-priority mismatch checks the environment",
                "unknown",
                "check_environment",
                "",
                None,
                None,
            ),
            (
                "an unreadable attempt allows manual recovery",
                "unknown",
                "manual_recovery",
                "",
                None,
                None,
            ),
            (
                "backup stage permits either backup mode",
                "migrating",
                "resume_or_abandon",
                "attempt=a1 attempt_stage=backup backup_mode=backup",
                None,
                None,
            ),
            (
                "backup stage permits no-backup mode",
                "migrating",
                "resume_or_abandon",
                "attempt=a2 attempt_stage=backup backup_mode=none",
                None,
                None,
            ),
            (
                "replace with backup resumes or restores",
                "migrating",
                "resume_or_restore",
                "attempt=a3 attempt_stage=replace backup_mode=backup",
                Some("/backup%20dir"),
                None,
            ),
            (
                "replace without backup resumes or abandons",
                "migrating",
                "resume_or_abandon",
                "attempt=a4 attempt_stage=replace backup_mode=none",
                None,
                None,
            ),
            (
                "rebuild pending starts without known counts",
                "migrating",
                "start_rebuild",
                "attempt=a5 attempt_stage=rebuild_pending backup_mode=none",
                None,
                None,
            ),
            (
                "commit finalizes without requiring an RPC result",
                "migrating",
                "finalize",
                "attempt=a6 attempt_stage=commit backup_mode=backup",
                Some("/backup%20dir"),
                None,
            ),
            (
                "restoring resumes restore",
                "migrating",
                "restore",
                "attempt=a7 attempt_stage=restoring backup_mode=backup",
                Some("/backup%20dir"),
                None,
            ),
            (
                "discarding resumes abandon",
                "migrating",
                "abandon",
                "attempt=a8 attempt_stage=discarding backup_mode=none",
                None,
                None,
            ),
            (
                "inconsistent no-backup attempt abandons",
                "rebuild_inconsistent",
                "abandon",
                "attempt=a9 attempt_stage=replace backup_mode=none",
                None,
                None,
            ),
            (
                "inconsistent backed-up attempt restores",
                "rebuild_inconsistent",
                "restore",
                "attempt=a10 attempt_stage=replace backup_mode=backup",
                Some("/backup%20dir"),
                None,
            ),
            (
                "inconsistent state without a usable attempt is manual",
                "rebuild_inconsistent",
                "manual_recovery",
                "",
                None,
                None,
            ),
            (
                "unavailable resource checks environment",
                "unavailable",
                "check_environment",
                "",
                None,
                Some("resource_unavailable"),
            ),
            (
                "corrupt unavailable resource can require manual recovery",
                "unavailable",
                "manual_recovery",
                "",
                None,
                Some("resource_corrupt"),
            ),
            (
                "corrupt unavailable resource can abandon an unbacked attempt",
                "unavailable",
                "abandon",
                "attempt=a11 attempt_stage=replace backup_mode=none",
                None,
                Some("resource_corrupt"),
            ),
            (
                "corrupt unavailable resource can restore a verified backup",
                "unavailable",
                "restore",
                "attempt=a12 attempt_stage=replace backup_mode=backup",
                Some("/backup%20dir"),
                Some("resource_corrupt"),
            ),
        ];

        for (label, state, action, attempt, backup, reason) in valid_cases {
            let line = inspect_fixture_with_reason(state, action, attempt, backup, reason);
            assert!(
                parse_result(&line, "embedding_inspect").is_ok(),
                "contract case should be accepted ({label}): {line}"
            );
        }
    }

    #[test]
    fn inspect_state_and_action_table_rejects_contradictions_but_keeps_safe_diagnostics() {
        let invalid_cases = [
            ("consistent", "reconcile", "", None, None),
            ("incomplete", "none", "", None, None),
            ("failed", "none", "", None, None),
            (
                "migrating",
                "start_rebuild",
                "attempt=a1 attempt_stage=backup backup_mode=backup",
                None,
                None,
            ),
            (
                "migrating",
                "resume_or_abandon",
                "attempt=a1 attempt_stage=replace backup_mode=backup",
                Some("/backup"),
                None,
            ),
            (
                "migrating",
                "resume_or_restore",
                "attempt=a1 attempt_stage=replace backup_mode=none",
                Some("/backup"),
                None,
            ),
            (
                "migrating",
                "finalize",
                "attempt=a1 attempt_stage=rebuild_pending backup_mode=none",
                None,
                None,
            ),
            (
                "migrating",
                "start_rebuild",
                "attempt=a1 attempt_stage=commit backup_mode=none",
                None,
                None,
            ),
            (
                "rebuild_inconsistent",
                "restore",
                "attempt=a1 attempt_stage=replace backup_mode=none",
                Some("/backup"),
                None,
            ),
            (
                "unavailable",
                "restore",
                "attempt=a1 attempt_stage=replace backup_mode=backup",
                None,
                Some("resource_corrupt"),
            ),
        ];

        for (state, action, attempt, backup, reason) in invalid_cases {
            let line = inspect_fixture_with_reason(state, action, attempt, backup, reason);
            assert!(
                parse_result(&line, "embedding_inspect").is_err(),
                "contradictory state/action should be rejected: {line}"
            );
        }

        for (state, action, reason) in [
            ("unknown", "wait", None),
            ("unknown", "check_environment", None),
            ("unknown", "manual_recovery", None),
            (
                "unavailable",
                "check_environment",
                Some("resource_unavailable"),
            ),
            ("unavailable", "manual_recovery", Some("resource_corrupt")),
        ] {
            let line = inspect_fixture_with_reason(state, action, "", None, reason);
            assert!(
                parse_result(&line, "embedding_inspect").is_ok(),
                "safe diagnostics must remain valid: {line}"
            );
        }
    }

    #[test]
    fn switch_and_finalize_reject_empty_or_reserved_mutation_identifiers() {
        for target in ["", "none", "unknown", "%6Eone", "%75nknown"] {
            let line = format!(
                "embedding_switch status=completed outcome=switched attempt=a source_space=unknown target_space={target} backup=none next_action=start_rebuild"
            );
            assert!(
                parse_result(&line, "embedding_switch").is_err(),
                "switch target must be a concrete identifier: {target:?}"
            );
        }

        for space in ["", "none", "unknown", "%6Eone", "%75nknown"] {
            let line = format!(
                "embedding_finalize status=completed outcome=finalized attempt=a space={space} accepted_failed=0 next_action=apply"
            );
            assert!(
                parse_result(&line, "embedding_finalize").is_err(),
                "finalized space must be a concrete identifier: {space:?}"
            );
        }

        for backup in ["", "unknown", "%75nknown"] {
            let line = format!(
                "embedding_switch status=completed outcome=switched attempt=a source_space=unknown target_space=target backup={backup} next_action=start_rebuild"
            );
            assert!(
                parse_result(&line, "embedding_switch").is_err(),
                "switch backup must be `none` or a concrete path: {backup:?}"
            );
        }

        let no_backup = parse_result(
            "embedding_switch status=completed outcome=switched attempt=a source_space=unknown target_space=target backup=none next_action=start_rebuild",
            "embedding_switch",
        );
        assert!(
            no_backup.is_ok(),
            "the literal `none` backup sentinel is valid"
        );
    }

    #[test]
    fn all_mutation_completion_shapes_keep_their_typed_outcomes() {
        let abandon = parse_result(
            "embedding_abandon status=completed outcome=discarded attempt=a next_action=reconcile",
            "embedding_abandon",
        )
        .unwrap();
        assert!(matches!(
            abandon,
            EmbeddingMigrationResult::Abandoned(EmbeddingAbandonSuccess {
                outcome: AbandonOutcome::Discarded,
                next_action: NextAction::Reconcile,
                ..
            })
        ));

        let finalize = parse_result(
            "embedding_finalize status=completed outcome=finalized attempt=a space=space accepted_failed=0 next_action=apply",
            "embedding_finalize",
        )
        .unwrap();
        assert!(matches!(
            finalize,
            EmbeddingMigrationResult::Finalized(EmbeddingFinalizeSuccess {
                accepted_failed: 0,
                next_action: NextAction::Apply,
                ..
            })
        ));

        let restore = parse_result(
            "embedding_restore status=completed outcome=already_restored attempt=a space=none next_action=reconcile",
            "embedding_restore",
        )
        .unwrap();
        assert!(matches!(
            restore,
            EmbeddingMigrationResult::Restored(EmbeddingRestoreSuccess {
                outcome: RestoreOutcome::AlreadyRestored,
                space: SpaceValue::None,
                ..
            })
        ));
    }

    #[test]
    fn serialized_dtos_keep_vocabularies_and_tag_space_values_explicit() {
        assert_eq!(
            serde_json::to_string(&EmbeddingState::RebuildInconsistent).unwrap(),
            "\"rebuild_inconsistent\""
        );
        assert_eq!(
            serde_json::to_string(&NextAction::StartRebuild).unwrap(),
            "\"start_rebuild\""
        );
        assert_eq!(
            serde_json::to_string(&PlanDecision::AdoptOnStart).unwrap(),
            "\"adopt_on_start\""
        );

        for (space, expected) in [
            (
                SpaceValue::Unknown,
                serde_json::json!({ "kind": "unknown" }),
            ),
            (SpaceValue::None, serde_json::json!({ "kind": "none" })),
            (
                SpaceValue::Id("space-1".into()),
                serde_json::json!({ "kind": "id", "value": "space-1" }),
            ),
        ] {
            let encoded = serde_json::to_value(&space).unwrap();
            assert_eq!(encoded, expected);
            let decoded: SpaceValue = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded, space);
        }

        let failure = EmbeddingMigrationFailure {
            command: CliCommand::LocalApply,
            stage: CliStage::Preflight,
            error_code: CliErrorCode::EmbeddingMigrationInProgress,
            resolution: CliResolution::ContinueOrRestore,
            attempt: Some(EmbeddingAttemptInfo {
                attempt: "attempt-1".into(),
                stage: AttemptStage::RebuildPending,
                backup_mode: BackupMode::Backup,
            }),
            backup: Some("/backup".into()),
            cancel_operation: None,
            verification_counts: None,
        };
        let encoded = serde_json::to_value(failure).unwrap();
        assert_eq!(encoded["command"], "local_apply");
        assert_eq!(encoded["error_code"], "embedding_migration_in_progress");
        assert_eq!(encoded["resolution"], "continue_or_restore");
        assert_eq!(encoded["attempt"]["stage"], "rebuild_pending");
        assert_eq!(encoded["attempt"]["backup_mode"], "backup");
    }

    #[test]
    fn embedding_requests_round_trip_for_approved_journal_reuse() {
        let request = EmbeddingRequest::Switch {
            target: EmbeddingTarget {
                model_id: "model".into(),
                tokenizer_model_id: "tokenizer".into(),
                revision: "revision-7".into(),
                dimension: 768,
            },
            expected_space: SpaceValue::Unknown,
            backup: EmbeddingBackup::Directory {
                path: "/backups/embedding".into(),
                keep: Some(2),
            },
            maintenance_window_ack: true,
        };

        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["operation"], "switch");
        assert_eq!(encoded["expected_space"]["kind"], "unknown");
        assert_eq!(encoded["backup"]["mode"], "directory");
        let decoded: EmbeddingRequest = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn serialized_cli_result_exposes_typed_decision_and_next_action() {
        let result = EmbeddingMigrationResult::Plan(EmbeddingPlan {
            decision: PlanDecision::ReconcileRequired,
            executable: true,
            reason: None,
            current_space: SpaceValue::None,
            target_space: "target".into(),
            memory_text: 1,
            memory_media: 2,
            thread: 3,
            reflection_intent: 4,
            failed_transient: Some(0),
            failed_permanent: None,
        });

        let encoded = serde_json::to_value(result).unwrap();
        assert_eq!(encoded["kind"], "plan");
        assert_eq!(encoded["data"]["decision"], "reconcile_required");
        assert_eq!(encoded["data"]["current_space"]["kind"], "none");
    }

    #[tokio::test]
    async fn run_uses_os_arguments_and_matching_extra_environment() {
        let (dir, data) = data_root();
        let tool = fake_tool(dir.path(), SWITCH_SUCCESS_WITH_SHELL_METACHARS, 0, "");
        let attempt = "attempt; touch injected-file".to_string();
        let request = EmbeddingRequest::Resume {
            attempt: attempt.clone(),
        };

        let result = run_embedding(&tool, &env("sqlite:///secret/db.sqlite3"), request, &data)
            .await
            .unwrap();

        assert!(matches!(
            result,
            EmbeddingMigrationResult::Switched(EmbeddingSwitchSuccess {
                outcome: SwitchOutcome::AlreadySwitched,
                ref attempt,
                ..
            }) if attempt == "attempt; touch injected-file"
        ));
        assert_eq!(
            recorded_args(dir.path()),
            [
                "embedding",
                "switch",
                "--resume=attempt; touch injected-file"
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("revision")).unwrap(),
            "model-revision-42"
        );
        assert!(!dir.path().join("injected-file").exists());
    }

    #[tokio::test]
    async fn switch_arguments_keep_target_expected_space_and_backup_selection_separate() {
        let (dir, data) = data_root();
        let tool = fake_tool(
            dir.path(),
            "embedding_switch status=completed outcome=switched attempt=a1 source_space=unknown target_space=target backup=/backup%20space next_action=start_rebuild",
            0,
            "",
        );
        let request = EmbeddingRequest::Switch {
            target: EmbeddingTarget {
                model_id: "model name".into(),
                tokenizer_model_id: "tokenizer id".into(),
                revision: "revision 2".into(),
                dimension: 384,
            },
            expected_space: SpaceValue::Unknown,
            backup: EmbeddingBackup::Directory {
                path: "/backup space".into(),
                keep: Some(0),
            },
            maintenance_window_ack: true,
        };

        let result = run_embedding(&tool, &env("sqlite:///db.sqlite3"), request, &data)
            .await
            .unwrap();

        assert!(matches!(result, EmbeddingMigrationResult::Switched(_)));
        assert_eq!(
            recorded_args(dir.path()),
            [
                "embedding",
                "switch",
                "--model-id=model name",
                "--tokenizer-model-id=tokenizer id",
                "--revision=revision 2",
                "--dimension=384",
                "--expected-space=unknown",
                "--maintenance-window-ack",
                "--backup-dir=/backup space",
                "--backup-keep=0",
            ]
        );
        assert!(
            !recorded_args(dir.path())
                .iter()
                .any(|argument| argument.starts_with("--distance"))
        );
        let no_backup_args = EmbeddingRequest::Switch {
            target: EmbeddingTarget {
                model_id: "m".into(),
                tokenizer_model_id: String::new(),
                revision: "r".into(),
                dimension: 1,
            },
            expected_space: SpaceValue::None,
            backup: EmbeddingBackup::NoBackupUnsafe,
            maintenance_window_ack: true,
        }
        .args()
        .unwrap();
        assert!(
            no_backup_args
                .iter()
                .any(|argument| argument == OsStr::new("--no-backup-unsafe"))
        );
    }

    #[tokio::test]
    async fn switch_without_maintenance_ack_is_rejected_before_spawn() {
        let (dir, data) = data_root();
        let tool = fake_tool(dir.path(), "", 0, "");
        let result = run_embedding(
            &tool,
            &env("sqlite:///db.sqlite3"),
            EmbeddingRequest::Switch {
                target: EmbeddingTarget {
                    model_id: "m".into(),
                    tokenizer_model_id: String::new(),
                    revision: "r".into(),
                    dimension: 1,
                },
                expected_space: SpaceValue::None,
                backup: EmbeddingBackup::NoBackupUnsafe,
                maintenance_window_ack: false,
            },
            &data,
        )
        .await;

        assert!(matches!(
            result,
            Err(AppError::EmbeddingMigrationFailed { .. })
        ));
        assert!(!dir.path().join("args").exists());
    }

    #[tokio::test]
    async fn switch_response_must_echo_the_expected_current_space() {
        let (dir, data) = data_root();
        let tool = fake_tool(
            dir.path(),
            "embedding_switch status=completed outcome=switched attempt=a1 source_space=other target_space=target backup=none next_action=start_rebuild",
            0,
            "",
        );
        let result = run_embedding(
            &tool,
            &env("sqlite:///db.sqlite3"),
            EmbeddingRequest::Switch {
                target: EmbeddingTarget {
                    model_id: "m".into(),
                    tokenizer_model_id: String::new(),
                    revision: "r".into(),
                    dimension: 1,
                },
                expected_space: SpaceValue::Unknown,
                backup: EmbeddingBackup::NoBackupUnsafe,
                maintenance_window_ack: true,
            },
            &data,
        )
        .await;

        assert!(matches!(
            result,
            Err(AppError::EmbeddingMigrationFailed {
                inspect_required: true,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn mutating_missing_or_mismatched_result_requires_inspection() {
        let (dir, data) = data_root();
        let tool = fake_tool(dir.path(), "embedding_progress stage=replace", 1, "");
        let result = run_embedding(
            &tool,
            &env("sqlite:///db.sqlite3"),
            EmbeddingRequest::Resume {
                attempt: "a1".into(),
            },
            &data,
        )
        .await;

        assert!(matches!(
            result,
            Err(AppError::EmbeddingMigrationFailed {
                inspect_required: true,
                ref detail,
                ..
            }) if detail.contains("inspect is required")
        ));
    }

    #[tokio::test]
    async fn result_status_must_match_process_exit_code() {
        let (success_dir, success_data) = data_root();
        let success_tool = fake_tool(
            success_dir.path(),
            SWITCH_SUCCESS_WITH_SHELL_METACHARS,
            1,
            "",
        );
        let success_mismatch = run_embedding(
            &success_tool,
            &env("sqlite:///db.sqlite3"),
            EmbeddingRequest::Resume {
                attempt: "attempt; touch injected-file".into(),
            },
            &success_data,
        )
        .await;
        assert!(matches!(
            success_mismatch,
            Err(AppError::EmbeddingMigrationFailed {
                inspect_required: true,
                ..
            })
        ));

        let (failure_dir, failure_data) = data_root();
        let failure_tool = fake_tool(
            failure_dir.path(),
            "embedding_switch status=failed stage=preflight error_code=attempt_not_found resolution=plan_required",
            0,
            "",
        );
        let failure_mismatch = run_embedding(
            &failure_tool,
            &env("sqlite:///db.sqlite3"),
            EmbeddingRequest::Resume {
                attempt: "a1".into(),
            },
            &failure_data,
        )
        .await;
        assert!(matches!(
            failure_mismatch,
            Err(AppError::EmbeddingMigrationFailed {
                inspect_required: true,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn tool_output_and_audit_log_redact_the_database_url() {
        let (dir, data) = data_root();
        let secret = "sqlite:///secret/private.sqlite3?mode=rwc";
        let tool = fake_tool(dir.path(), INSPECT_WITH_UNKNOWN_COUNTS, 0, secret);

        run_embedding(&tool, &env(secret), EmbeddingRequest::Inspect, &data)
            .await
            .unwrap();

        let log = std::fs::read_dir(data.log_dir())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|extension| extension == "log"))
            .unwrap();
        let body = std::fs::read_to_string(log).unwrap();
        assert!(!body.contains(secret), "database URL leaked into audit log");
        assert!(body.contains("<redacted-database-url>"));
    }

    #[tokio::test]
    async fn malformed_inspect_is_a_typed_error_without_mutation_recovery_hint() {
        let (dir, data) = data_root();
        let malformed = inspect_fixture("migrating", "start_rebuild", "", None);
        let tool = fake_tool(dir.path(), &malformed, 0, "");

        let error = run_embedding(
            &tool,
            &env("sqlite:///db.sqlite3"),
            EmbeddingRequest::Inspect,
            &data,
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            AppError::EmbeddingMigrationFailed {
                failure: None,
                inspect_required: false,
                ..
            }
        ));
    }
}
