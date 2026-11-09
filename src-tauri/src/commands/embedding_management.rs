//! Application coordination for Memories-owned embedding-space migration.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{Emitter, State};
use tokio::time::{Instant, sleep, timeout};
use tonic::transport::Channel;

use crate::error::{AppError, AppResult};
use crate::grpc::proto::llm_memory::service::embedding_space_service_client::EmbeddingSpaceServiceClient;
use crate::sidecar::embedding_migration::{
    EmbeddingBackup, EmbeddingInspect, EmbeddingMigrationResult, EmbeddingPlan, EmbeddingRequest,
    EmbeddingTarget, NextAction, PlanDecision, SpaceValue,
};

use super::embedding_settings::{
    EmbeddingPlanPermit, EmbeddingSettings, SetEmbeddingSettingsRequest, embedding_space_changed,
    prepare_embedding_candidate_runtime, save_embedding_settings,
    validate_pending_chunk_configuration_change,
};
use super::{AppState, connection};

const WORKER_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(30);
const WORKER_REGISTRATION_POLL_INTERVAL: Duration = Duration::from_millis(500);
const WORKER_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const MANAGEMENT_RPC_TIMEOUT: Duration = Duration::from_secs(30);
const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(15);
const DISPATCH_DRAIN_TIMEOUT: Duration = Duration::from_secs(15);
const JOURNAL_VERSION: u32 = 1;
static NEXT_JOURNAL_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EmbeddingSettingsApproval {
    pub permit: EmbeddingPlanPermit,
    pub rebuild_pending: bool,
    pub decision: PlanDecision,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SwitchEmbeddingSpaceRequest {
    pub settings: SetEmbeddingSettingsRequest,
    pub expected_space: SpaceValue,
    pub backup: EmbeddingBackup,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EmbeddingRecoveryAction {
    Resume,
    Finalize,
    Restore,
    Abandon,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RecoverEmbeddingMigrationRequest {
    pub action: EmbeddingRecoveryAction,
    pub attempt_id: String,
    #[serde(default)]
    pub accept_failed: Option<u64>,
    #[serde(default)]
    pub settings: Option<SetEmbeddingSettingsRequest>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EmbeddingMigrationOperationResponse {
    pub result: EmbeddingMigrationResult,
    pub restarted: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum JournalPhase {
    Prepared,
    Switched,
    RebuildPending,
    Finalized,
    Restored,
    Abandoned,
    ReconcilePending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EmbeddingMigrationJournal {
    version: u32,
    phase: JournalPhase,
    old_settings: EmbeddingSettings,
    new_settings: EmbeddingSettings,
    expected_space: SpaceValue,
    target_space: Option<String>,
    attempt_id: Option<String>,
    #[serde(default)]
    attempt: Option<crate::sidecar::memories_migration::EmbeddingAttemptInfo>,
    backup: Option<String>,
    #[serde(default)]
    accepted_failed: Option<u64>,
    #[serde(default)]
    last_verification_counts: Option<crate::sidecar::memories_migration::VerificationCounts>,
    paused_periodic_scheduler_ids: Vec<i64>,
}

fn journal_path(data: &crate::data::DataPaths) -> PathBuf {
    data.root.join("embedding-migration-journal.json")
}

fn sync_parent_directory(path: &Path) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn save_journal(
    data: &crate::data::DataPaths,
    journal: &EmbeddingMigrationJournal,
) -> AppResult<()> {
    let path = journal_path(data);
    let parent = path.parent().ok_or_else(|| {
        AppError::Config("embedding migration journal has no parent directory".into())
    })?;
    fs::create_dir_all(parent)?;
    let json = serde_json::to_vec_pretty(journal).map_err(|error| {
        AppError::Config(format!("serialize embedding migration journal: {error}"))
    })?;
    let unique = NEXT_JOURNAL_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_extension(format!(
        "json.tmp-{}-{timestamp}-{unique}",
        std::process::id()
    ));
    let result = (|| -> AppResult<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&json)?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        sync_parent_directory(&path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn load_journal(data: &crate::data::DataPaths) -> AppResult<Option<EmbeddingMigrationJournal>> {
    let path = journal_path(data);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let journal: EmbeddingMigrationJournal = serde_json::from_slice(&bytes).map_err(|error| {
        AppError::Config(format!("embedding migration journal is invalid: {error}"))
    })?;
    if journal.version != JOURNAL_VERSION {
        return Err(AppError::Config(format!(
            "unsupported embedding migration journal version {}",
            journal.version
        )));
    }
    Ok(Some(journal))
}

fn remove_journal(data: &crate::data::DataPaths) -> AppResult<()> {
    let path = journal_path(data);
    match fs::remove_file(&path) {
        Ok(()) => sync_parent_directory(&path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn verified_backup_path(path: &str) -> bool {
    let path = path.trim();
    !path.is_empty() && !path.eq_ignore_ascii_case("none") && !path.eq_ignore_ascii_case("unknown")
}

fn prepared_journal_cleanup_is_authorized(
    journal: &EmbeddingMigrationJournal,
    inspect: &EmbeddingInspect,
) -> bool {
    if journal.phase != JournalPhase::Prepared || inspect.attempt.is_some() {
        return false;
    }
    let same_concrete_space = matches!(
        (&journal.expected_space, &inspect.space),
        (SpaceValue::Id(expected), SpaceValue::Id(actual)) if expected == actual
    );
    if !same_concrete_space {
        return false;
    }
    match inspect.state {
        crate::sidecar::embedding_migration::EmbeddingState::Consistent => {
            inspect.next_action == NextAction::None
        }
        crate::sidecar::embedding_migration::EmbeddingState::Incomplete
        | crate::sidecar::embedding_migration::EmbeddingState::Unverified => {
            inspect.next_action == NextAction::Reconcile
        }
        _ => false,
    }
}

async fn settle_prepared_journal<F, Fut>(
    data: &crate::data::DataPaths,
    mut journal: EmbeddingMigrationJournal,
    inspect: &EmbeddingInspect,
    restore_schedulers: F,
) -> AppResult<bool>
where
    F: FnOnce(Vec<i64>) -> Fut,
    Fut: std::future::Future<Output = AppResult<()>>,
{
    if inspect.attempt.is_some() {
        if update_journal_attempt_from_inspect(&mut journal, inspect)? {
            save_journal(data, &journal)?;
        }
        return Ok(false);
    }
    if !prepared_journal_cleanup_is_authorized(&journal, inspect) {
        return Ok(false);
    }

    restore_scheduler_snapshot_and_consume(data, &mut journal, restore_schedulers).await?;
    remove_journal(data)?;
    Ok(true)
}

fn rebuild_pending_inspect_matches_journal(
    journal: &EmbeddingMigrationJournal,
    inspect: &EmbeddingInspect,
) -> bool {
    use crate::sidecar::memories_migration::AttemptStage;

    let Some(target_space) = journal.target_space.as_deref() else {
        return false;
    };
    let Some(attempt_id) = journal.attempt_id.as_deref() else {
        return false;
    };
    journal.phase == JournalPhase::RebuildPending
        && matches!(inspect.space, SpaceValue::Id(ref space) if space == target_space)
        && inspect.attempt.as_ref().is_some_and(|attempt| {
            attempt.attempt == attempt_id && attempt.stage == AttemptStage::RebuildPending
        })
        && matches!(
            (inspect.state, inspect.next_action),
            (
                crate::sidecar::embedding_migration::EmbeddingState::Migrating,
                NextAction::StartRebuild
            ) | (
                crate::sidecar::embedding_migration::EmbeddingState::Failed,
                NextAction::ResolveFailed
            )
        )
}

fn startup_rebuild_pending_is_authorized(
    journal: &EmbeddingMigrationJournal,
    inspect: &EmbeddingInspect,
    space: &crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceResponse,
) -> bool {
    if !rebuild_pending_inspect_matches_journal(journal, inspect) {
        return false;
    }
    let Some(target_space) = journal.target_space.as_deref() else {
        return false;
    };
    if space
        .current_space
        .as_ref()
        .is_none_or(|current| current.space_id != target_space)
        || space.tables.is_empty()
    {
        return false;
    }
    space.tables.iter().all(|table| {
        table
            .record
            .as_ref()
            .and_then(|record| record.migration_marker.as_ref())
            .is_some_and(|marker| {
                marker.state == "pending"
                    && journal.attempt_id.as_deref() == Some(marker.attempt_id.as_str())
            })
    })
}

async fn settle_rebuild_pending_journal_after_startup<F, Fut>(
    data: &crate::data::DataPaths,
    mut journal: EmbeddingMigrationJournal,
    inspect: &EmbeddingInspect,
    space: &crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceResponse,
    restore_schedulers: F,
) -> AppResult<bool>
where
    F: FnOnce(Vec<i64>) -> Fut,
    Fut: std::future::Future<Output = AppResult<()>>,
{
    if !startup_rebuild_pending_is_authorized(&journal, inspect, space) {
        return Ok(false);
    }
    restore_scheduler_snapshot_and_consume(data, &mut journal, restore_schedulers).await?;
    // Keep the migration journal: finalize/restore still needs its attempt and
    // backup record, but the pre-restart scheduler snapshot is now consumed.
    save_journal(data, &journal)?;
    Ok(true)
}

async fn restore_scheduler_snapshot_and_consume<F, Fut>(
    data: &crate::data::DataPaths,
    journal: &mut EmbeddingMigrationJournal,
    restore_schedulers: F,
) -> AppResult<()>
where
    F: FnOnce(Vec<i64>) -> Fut,
    Fut: std::future::Future<Output = AppResult<()>>,
{
    restore_schedulers(journal.paused_periodic_scheduler_ids.clone()).await?;
    clear_scheduler_pause_ledger(data, journal)
}

async fn recover_scheduler_snapshot_on_startup_with<I, IFut, Q, QFut, R, RFut, O, OFut>(
    data: &crate::data::DataPaths,
    mut journal: EmbeddingMigrationJournal,
    inspect_cli: I,
    query_space: Q,
    restore_schedulers: R,
    open_admission: O,
) -> AppResult<(bool, EmbeddingMigrationJournal, EmbeddingInspect)>
where
    I: FnOnce() -> IFut,
    IFut: std::future::Future<Output = AppResult<EmbeddingMigrationResult>>,
    Q: FnOnce() -> QFut,
    QFut: std::future::Future<
            Output = AppResult<crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceResponse>,
        >,
    R: FnOnce(Vec<i64>) -> RFut,
    RFut: std::future::Future<Output = AppResult<()>>,
    O: FnOnce() -> OFut,
    OFut: std::future::Future<Output = AppResult<()>>,
{
    let result = inspect_cli().await?;
    let inspect = inspect_view(&result)?.clone();
    if update_journal_attempt_from_inspect(&mut journal, &inspect)? {
        save_journal(data, &journal)?;
    }

    let mut restore_schedulers = Some(restore_schedulers);
    let recovered = if journal.phase == JournalPhase::Prepared {
        settle_prepared_journal(data, journal.clone(), &inspect, |ids| {
            restore_schedulers
                .take()
                .expect("restore callback is consumed once")(ids)
        })
        .await?
    } else if rebuild_pending_inspect_matches_journal(&journal, &inspect) {
        let space = query_space().await?;
        settle_rebuild_pending_journal_after_startup(
            data,
            journal.clone(),
            &inspect,
            &space,
            |ids| {
                restore_schedulers
                    .take()
                    .expect("restore callback is consumed once")(ids)
            },
        )
        .await?
    } else {
        false
    };
    if recovered {
        open_admission().await?;
    }
    Ok((recovered, journal, inspect))
}

fn clear_scheduler_pause_ledger(
    data: &crate::data::DataPaths,
    journal: &mut EmbeddingMigrationJournal,
) -> AppResult<()> {
    if journal.paused_periodic_scheduler_ids.is_empty() {
        return Ok(());
    }
    let mut consumed = journal.clone();
    consumed.paused_periodic_scheduler_ids.clear();
    save_journal(data, &consumed)?;
    *journal = consumed;
    Ok(())
}

fn merge_paused_scheduler_ids(journal: &mut EmbeddingMigrationJournal, ids: &[i64]) {
    for id in ids {
        if !journal.paused_periodic_scheduler_ids.contains(id) {
            journal.paused_periodic_scheduler_ids.push(*id);
        }
    }
}

/// Reconcile a crash that happened before the embedding switch CLI recorded an
/// attempt. The on-disk journal keeps normal local writers gated until the
/// fresh inspect proves that the prepared switch never changed the space and
/// every scheduler that was enabled before the pause has been restored.
pub(crate) async fn recover_embedding_journal_after_startup(state: &AppState) -> AppResult<()> {
    let _recovery = state.database_migration_recovery.lock().await;
    let _dispatch_gate = state.lock_llm_settings_dispatch_gate().await;
    let Some(journal) = load_journal(&state.data)? else {
        state.open_embedding_migration_admission().await;
        return Ok(());
    };
    if !state.is_service_local(connection::Service::Memories) {
        return Ok(());
    }

    let data = state.data.clone();
    let (scheduler_snapshot_recovered, journal, inspect) =
        recover_scheduler_snapshot_on_startup_with(
            &data,
            journal,
            || async { inspect_cli(state).await },
            || async { embedding_space_for_migration_guard(state).await },
            |ids| async move { restore_scheduler_ids_after_startup(state, ids).await },
            || async {
                state.open_embedding_migration_admission().await;
                Ok(())
            },
        )
        .await?;
    if scheduler_snapshot_recovered {
        return Ok(());
    }
    let recovered = settle_terminal_journal_after_startup(state, journal, &inspect).await?;
    if recovered {
        state.open_embedding_migration_admission().await;
    }
    Ok(())
}

async fn settle_terminal_journal_after_startup(
    state: &AppState,
    mut journal: EmbeddingMigrationJournal,
    inspect: &EmbeddingInspect,
) -> AppResult<bool> {
    if inspect.attempt.is_some() {
        if update_journal_attempt_from_inspect(&mut journal, inspect)? {
            save_journal(&state.data, &journal)?;
        }
        return Ok(false);
    }

    let (action, use_old_settings) = match journal.phase {
        JournalPhase::Finalized => (EmbeddingRecoveryAction::Finalize, false),
        JournalPhase::Restored => (EmbeddingRecoveryAction::Restore, true),
        JournalPhase::Abandoned => (EmbeddingRecoveryAction::Abandon, true),
        JournalPhase::ReconcilePending if journal.backup.is_some() => {
            (EmbeddingRecoveryAction::Restore, true)
        }
        JournalPhase::ReconcilePending => (EmbeddingRecoveryAction::Abandon, false),
        JournalPhase::Prepared | JournalPhase::Switched | JournalPhase::RebuildPending => {
            return Ok(false);
        }
    };
    if inspect.next_action == NextAction::Apply {
        // An apply is a sidecar-stopped operation. Keep the writer gate and
        // journal for the explicit recovery transaction rather than guessing
        // that startup already completed it.
        return Ok(false);
    }
    let Some(attempt_id) = journal.attempt_id.clone() else {
        return Ok(false);
    };
    let request = RecoverEmbeddingMigrationRequest {
        action,
        attempt_id,
        accept_failed: None,
        settings: None,
    };
    if terminal_recovery_retry_authorized(&request, &journal, inspect).is_err() {
        return Ok(false);
    }

    let settings =
        journal_or_requested_settings(Some(&journal), &request, state, use_old_settings)?;
    let plan = recovery_plan_for_settings(state, &settings).await?;
    if !plan.executable
        || plan.current_space != inspect.space
        || plan.decision == PlanDecision::ReembedRequired
    {
        return Ok(false);
    }
    if let SpaceValue::Id(space) = &inspect.space
        && plan.target_space != *space
        && matches!(
            plan.decision,
            PlanDecision::NoChange | PlanDecision::ReconcileRequired
        )
    {
        return Ok(false);
    }

    if inspect.next_action == NextAction::Reconcile {
        wait_for_registered_worker(state).await?;
        start_reconcile(state).await?;
    }
    restore_scheduler_snapshot_and_consume(&state.data, &mut journal, |ids| async move {
        restore_scheduler_ids_after_startup(state, ids).await
    })
    .await?;
    remove_journal(&state.data)?;
    Ok(true)
}

async fn restore_scheduler_ids_after_startup(state: &AppState, ids: Vec<i64>) -> AppResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut paused = super::periodic_execution::PausedPeriodicSchedules::default();
    paused.include_scheduler_ids(&ids)?;
    paused.resume_after_restart(state).await
}

fn journal_attempt_from_inspect(
    data: &crate::data::DataPaths,
    inspect: &EmbeddingInspect,
) -> AppResult<Option<EmbeddingMigrationJournal>> {
    let Some(mut journal) = load_journal(data)? else {
        return Ok(None);
    };
    if update_journal_attempt_from_inspect(&mut journal, inspect)? {
        save_journal(data, &journal)?;
    }
    Ok(Some(journal))
}

fn update_journal_attempt_from_inspect(
    journal: &mut EmbeddingMigrationJournal,
    inspect: &EmbeddingInspect,
) -> AppResult<bool> {
    let Some(attempt) = inspect.attempt.as_ref() else {
        return Ok(false);
    };
    if journal
        .attempt_id
        .as_deref()
        .is_some_and(|journal_id| journal_id != attempt.attempt)
    {
        return Err(AppError::Config(
            "Memories inspect found a different attempt than the application journal".into(),
        ));
    }

    let previous_attempt_id = journal.attempt_id.clone();
    let previous_attempt = journal.attempt.clone();
    let previous_backup = journal.backup.clone();
    let previous_phase = journal.phase;
    journal.attempt_id = Some(attempt.attempt.clone());
    journal.attempt = Some(attempt.clone());
    match attempt.backup_mode {
        crate::sidecar::memories_migration::BackupMode::None => journal.backup = None,
        crate::sidecar::memories_migration::BackupMode::Backup => {
            if let Some(backup) = inspect
                .backup
                .as_deref()
                .filter(|path| verified_backup_path(path))
            {
                journal.backup = Some(backup.to_string());
            }
        }
    }
    if attempt.stage == crate::sidecar::memories_migration::AttemptStage::RebuildPending {
        journal.phase = JournalPhase::RebuildPending;
    }
    Ok(journal.attempt_id != previous_attempt_id
        || journal.attempt != previous_attempt
        || journal.backup != previous_backup
        || journal.phase != previous_phase)
}

fn embedding_runtime_for_request_with_env<F>(
    req: &SetEmbeddingSettingsRequest,
    env_lookup: F,
) -> AppResult<super::embedding_settings::EmbeddingRuntime>
where
    F: Fn(&str) -> Option<String>,
{
    prepare_embedding_candidate_runtime(req, env_lookup)
}

fn target_for_runtime(runtime: &super::embedding_settings::EmbeddingRuntime) -> EmbeddingTarget {
    EmbeddingTarget {
        model_id: runtime.model_id.clone(),
        tokenizer_model_id: runtime.tokenizer_id.clone().unwrap_or_default(),
        revision: runtime.model_revision.clone(),
        dimension: runtime.vector_size,
    }
}

fn plan_target_with_env<F>(
    req: &SetEmbeddingSettingsRequest,
    env_lookup: F,
) -> AppResult<EmbeddingTarget>
where
    F: Fn(&str) -> Option<String>,
{
    let runtime = embedding_runtime_for_request_with_env(req, env_lookup)?;
    Ok(target_for_runtime(&runtime))
}

fn plan_target(
    req: &SetEmbeddingSettingsRequest,
    sidecars: &crate::sidecar::Sidecars,
) -> AppResult<EmbeddingTarget> {
    plan_target_with_env(req, |key| sidecars.embedding_env_lookup(key))
}

fn permit_for_plan(plan: &EmbeddingPlan) -> AppResult<EmbeddingPlanPermit> {
    if !plan.executable {
        return Err(AppError::Config(format!(
            "Memories embedding plan is not executable ({:?}); settings and other batch files were not changed",
            plan.reason
        )));
    }
    match plan.decision {
        PlanDecision::NoChange => Ok(EmbeddingPlanPermit::NoChange),
        PlanDecision::ReconcileRequired => Ok(EmbeddingPlanPermit::Reconcile),
        PlanDecision::AdoptOnStart => Ok(EmbeddingPlanPermit::Adopt),
        PlanDecision::ReembedRequired => Err(AppError::Config(
            "embedding rebuild is required; continue through the embedding migration UI".into(),
        )),
    }
}

fn validate_pending_rebuild_settings_change(
    old: &super::embedding_settings::EmbeddingRuntime,
    new: &super::embedding_settings::EmbeddingRuntime,
) -> AppResult<()> {
    validate_pending_chunk_configuration_change(old, new, true)?;
    if embedding_space_changed(old, new) {
        return Err(AppError::Config(
            "embedding identity cannot change while a rebuild is pending".into(),
        ));
    }
    Ok(())
}

fn cli_failure(result: &EmbeddingMigrationResult, action: &str) -> AppError {
    match result {
        EmbeddingMigrationResult::Failed(failure) => AppError::EmbeddingMigrationFailed {
            failure: Some(Box::new(failure.clone())),
            detail: format!("Memories rejected embedding {action}"),
            inspect_required: true,
        },
        _ => AppError::Config(format!(
            "Memories did not return an embedding {action} result"
        )),
    }
}

fn inspect_view(result: &EmbeddingMigrationResult) -> AppResult<&EmbeddingInspect> {
    match result {
        EmbeddingMigrationResult::Inspect(inspect) => Ok(inspect),
        EmbeddingMigrationResult::Failed(_) => Err(cli_failure(result, "inspect")),
        _ => Err(AppError::Config(
            "Memories returned an unexpected result for embedding inspect".into(),
        )),
    }
}

fn plan_view(result: &EmbeddingMigrationResult) -> AppResult<&EmbeddingPlan> {
    match result {
        EmbeddingMigrationResult::Plan(plan) => Ok(plan),
        EmbeddingMigrationResult::Failed(_) => Err(cli_failure(result, "plan")),
        _ => Err(AppError::Config(
            "Memories returned an unexpected result for embedding plan".into(),
        )),
    }
}

fn require_local_embedding_management(state: &AppState) -> AppResult<()> {
    if state.is_service_local(connection::Service::Memories) {
        Ok(())
    } else {
        Err(AppError::Config(
            "embedding migration management is local-only; remote embedding configuration is read-only".into(),
        ))
    }
}

fn rebuild_is_pending(
    inspect: &EmbeddingInspect,
    space: Option<&crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceResponse>,
) -> AppResult<bool> {
    use crate::sidecar::memories_migration::AttemptStage;

    if matches!(
        inspect.state,
        crate::sidecar::embedding_migration::EmbeddingState::Unavailable
            | crate::sidecar::embedding_migration::EmbeddingState::Unknown
    ) {
        return Err(AppError::Config(
            "Memories embedding state is unavailable or unknown; settings were not changed".into(),
        ));
    }
    let attempt_pending = match inspect.attempt.as_ref().map(|attempt| attempt.stage) {
        Some(AttemptStage::RebuildPending) => true,
        Some(_) => {
            return Err(AppError::Config(
                "an embedding migration is already in progress; ordinary settings cannot be saved until it is recovered".into(),
            ));
        }
        None => false,
    };
    if attempt_pending {
        return Ok(true);
    }

    let Some(space) = space else {
        return Ok(false);
    };
    let mut pending_marker = false;
    for marker in space
        .tables
        .iter()
        .filter_map(|table| table.record.as_ref()?.migration_marker.as_ref())
    {
        match marker.state.as_str() {
            "pending" => pending_marker = true,
            "switching" | "committing" | "restoring" | "discarding" => {
                return Err(AppError::Config(format!(
                    "Memories reports an active embedding migration marker ({}) without a rebuild-pending attempt",
                    marker.state
                )));
            }
            other => {
                return Err(AppError::Config(format!(
                    "Memories returned an unknown embedding migration marker state {other:?}"
                )));
            }
        }
    }
    Ok(pending_marker)
}

async fn inspect_cli(state: &AppState) -> AppResult<EmbeddingMigrationResult> {
    require_local_embedding_management(state)?;
    state
        .sidecars
        .run_embedding_cli(EmbeddingRequest::Inspect)
        .await
}

async fn plan_cli(
    state: &AppState,
    req: &SetEmbeddingSettingsRequest,
) -> AppResult<EmbeddingMigrationResult> {
    let runtime = embedding_runtime_for_request_with_env(req, |key| {
        state.sidecars.embedding_env_lookup(key)
    })?;
    let target = target_for_runtime(&runtime);
    state
        .sidecars
        .run_embedding_cli_with_runtime(EmbeddingRequest::Plan { target }, &runtime)
        .await
}

pub(crate) async fn approve_embedding_settings_change(
    state: &AppState,
    req: &SetEmbeddingSettingsRequest,
) -> AppResult<EmbeddingSettingsApproval> {
    require_local_embedding_management(state)?;
    let inspection = inspect_cli(state).await?;
    let inspect = inspect_view(&inspection)?;
    if inspect.state == crate::sidecar::embedding_migration::EmbeddingState::Unavailable {
        return Err(AppError::Config(
            "Memories embedding state is unavailable; settings were not changed".into(),
        ));
    }
    let old_settings =
        super::embedding_settings::load_embedding_settings(&state.data.embedding_settings_path());
    let old_runtime =
        super::embedding_settings::resolve_embedding_runtime_with_env(&old_settings, |key| {
            state.sidecars.embedding_env_lookup(key)
        });
    let new_runtime =
        prepare_embedding_candidate_runtime(req, |key| state.sidecars.embedding_env_lookup(key))?;
    let mut pending = rebuild_is_pending(inspect, None)?;
    if !pending && old_settings != super::embedding_settings::settings_from_request(req) {
        let marker_snapshot = embedding_space_for_migration_guard(state).await?;
        pending = rebuild_is_pending(inspect, Some(&marker_snapshot))?;
    }
    if pending {
        validate_pending_rebuild_settings_change(&old_runtime, &new_runtime)?;
        return Ok(EmbeddingSettingsApproval {
            permit: EmbeddingPlanPermit::NoChange,
            rebuild_pending: true,
            decision: PlanDecision::NoChange,
        });
    }
    let plan_result = plan_cli(state, req).await?;
    let plan = plan_view(&plan_result)?;
    let permit = permit_for_plan(plan)?;
    Ok(EmbeddingSettingsApproval {
        permit,
        rebuild_pending: pending,
        decision: plan.decision,
    })
}

/// Setup's selected root can differ from the currently running root. Inspect
/// and plan that root explicitly before any setup files are persisted; using
/// the live AppState path here would authorize settings against the wrong
/// Memories database.
pub(crate) async fn approve_embedding_settings_change_for_data(
    sidecars: &crate::sidecar::Sidecars,
    data: &crate::data::DataPaths,
    req: &SetEmbeddingSettingsRequest,
) -> AppResult<EmbeddingSettingsApproval> {
    if load_journal(data)?.is_some() {
        return Err(AppError::Config(
            "target-root embedding migration journal exists; recover it before setup settings are changed"
                .into(),
        ));
    }
    if !connection::load_connection_config(&data.connection_config_path())
        .memories
        .is_local()
    {
        return Err(AppError::Config(
            "target-root embedding migration approval is available only for local Memories roots"
                .into(),
        ));
    }

    let old_settings =
        super::embedding_settings::load_embedding_settings(&data.embedding_settings_path());
    let old_runtime =
        super::embedding_settings::resolve_embedding_runtime_with_env(&old_settings, |key| {
            sidecars.embedding_env_lookup(key)
        });
    let new_runtime =
        prepare_embedding_candidate_runtime(req, |key| sidecars.embedding_env_lookup(key))?;
    let inspection = sidecars
        .run_embedding_cli_for_data(data, &old_runtime, EmbeddingRequest::Inspect)
        .await?;
    let inspect = inspect_view(&inspection)?;
    if let Some(approval) = target_root_approval(inspect, &old_runtime, &new_runtime, None)? {
        return Ok(approval);
    }

    let target = plan_target(req, sidecars)?;
    let plan_result = sidecars
        .run_embedding_cli_for_data(data, &new_runtime, EmbeddingRequest::Plan { target })
        .await?;
    let plan = plan_view(&plan_result)?;
    target_root_approval(inspect, &old_runtime, &new_runtime, Some(plan))?
        .ok_or_else(|| AppError::Config("target-root embedding plan was not evaluated".into()))
}

fn target_root_approval(
    inspect: &EmbeddingInspect,
    old_runtime: &super::embedding_settings::EmbeddingRuntime,
    new_runtime: &super::embedding_settings::EmbeddingRuntime,
    plan: Option<&EmbeddingPlan>,
) -> AppResult<Option<EmbeddingSettingsApproval>> {
    if inspect.state == crate::sidecar::embedding_migration::EmbeddingState::Unavailable {
        return Err(AppError::Config(
            "target-root Memories embedding state is unavailable; setup settings were not changed"
                .into(),
        ));
    }
    if let Some(attempt) = inspect.attempt.as_ref() {
        if attempt.stage == crate::sidecar::memories_migration::AttemptStage::RebuildPending {
            validate_pending_rebuild_settings_change(old_runtime, new_runtime)?;
            return Ok(Some(EmbeddingSettingsApproval {
                permit: EmbeddingPlanPermit::NoChange,
                rebuild_pending: true,
                decision: PlanDecision::NoChange,
            }));
        }
        return Err(AppError::Config(
            "target-root embedding migration is active; recover it before setup settings are changed"
                .into(),
        ));
    }
    let Some(plan) = plan else {
        return Ok(None);
    };
    let permit = permit_for_plan(plan)?;
    Ok(Some(EmbeddingSettingsApproval {
        permit,
        rebuild_pending: false,
        decision: plan.decision,
    }))
}

async fn embedding_space_for_migration_guard(
    state: &AppState,
) -> AppResult<crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceResponse> {
    let snapshot = state.embedding_connection_snapshot()?;
    state.ensure_embedding_connection_current(&snapshot)?;
    let channel = timeout(MANAGEMENT_RPC_TIMEOUT, state.memories_channel())
        .await
        .map_err(|_| AppError::SidecarNotReady("embedding space lookup timed out".into()))??;
    state.ensure_embedding_connection_current(&snapshot)?;
    let response = get_embedding_space(&channel, MANAGEMENT_RPC_TIMEOUT).await?;
    state.ensure_embedding_connection_current(&snapshot)?;
    Ok(response)
}

#[tauri::command]
pub(crate) async fn inspect_embedding_migration(
    state: State<'_, AppState>,
) -> AppResult<EmbeddingMigrationResult> {
    let _recovery = state.database_migration_recovery.lock().await;
    let result = inspect_cli(&state).await?;
    if let Ok(inspect) = inspect_view(&result) {
        journal_attempt_from_inspect(&state.data, inspect)?;
    }
    Ok(result)
}

#[tauri::command]
pub(crate) async fn plan_embedding_change(
    state: State<'_, AppState>,
    req: SetEmbeddingSettingsRequest,
) -> AppResult<EmbeddingMigrationResult> {
    let _recovery = state.database_migration_recovery.lock().await;
    let inspection = inspect_cli(&state).await?;
    let inspect = match inspect_view(&inspection) {
        Ok(inspect) => inspect,
        Err(_) => return Ok(inspection),
    };
    journal_attempt_from_inspect(&state.data, inspect)?;
    if inspect.state == crate::sidecar::embedding_migration::EmbeddingState::Unavailable {
        return Ok(inspection);
    }
    plan_cli(&state, &req).await
}

fn recovery_action_allowed(
    action: EmbeddingRecoveryAction,
    next: NextAction,
    attempt: &crate::sidecar::memories_migration::EmbeddingAttemptInfo,
    has_verified_backup: bool,
) -> bool {
    use crate::sidecar::memories_migration::{AttemptStage, BackupMode};

    let rebuild_pending = attempt.stage == AttemptStage::RebuildPending;
    let no_backup = attempt.backup_mode == BackupMode::None;
    match action {
        EmbeddingRecoveryAction::Resume => match next {
            NextAction::ResumeOrAbandon => {
                attempt.stage == AttemptStage::Backup
                    || (attempt.stage == AttemptStage::Replace && no_backup)
            }
            NextAction::ResumeOrRestore => {
                attempt.stage == AttemptStage::Replace && has_verified_backup
            }
            NextAction::StartRebuild => rebuild_pending,
            _ => false,
        },
        EmbeddingRecoveryAction::Finalize => {
            (next == NextAction::Finalize && attempt.stage == AttemptStage::Commit)
                || (rebuild_pending
                    && matches!(next, NextAction::StartRebuild | NextAction::ResolveFailed))
        }
        EmbeddingRecoveryAction::Restore => {
            has_verified_backup
                && ((next == NextAction::Restore
                    && matches!(
                        attempt.stage,
                        AttemptStage::Replace
                            | AttemptStage::RebuildPending
                            | AttemptStage::Commit
                            | AttemptStage::Restoring
                    ))
                    || (next == NextAction::ResumeOrRestore
                        && attempt.stage == AttemptStage::Replace)
                    || (next == NextAction::Finalize && attempt.stage == AttemptStage::Commit)
                    || (next == NextAction::StartRebuild && rebuild_pending))
        }
        EmbeddingRecoveryAction::Abandon => {
            (next == NextAction::Abandon
                && no_backup
                && matches!(
                    attempt.stage,
                    AttemptStage::Backup
                        | AttemptStage::Replace
                        | AttemptStage::RebuildPending
                        | AttemptStage::Discarding
                ))
                || (next == NextAction::ResumeOrAbandon
                    && (attempt.stage == AttemptStage::Backup
                        || (attempt.stage == AttemptStage::Replace && no_backup)))
                || (next == NextAction::StartRebuild && rebuild_pending && no_backup)
        }
    }
}

fn validate_recovery_action(
    request: &RecoverEmbeddingMigrationRequest,
    inspect: &EmbeddingInspect,
) -> AppResult<()> {
    if request.attempt_id.trim().is_empty() {
        return Err(AppError::Config(
            "embedding recovery attempt_id must not be empty".into(),
        ));
    }
    let Some(attempt) = inspect.attempt.as_ref() else {
        return Err(AppError::Config(
            "Memories inspect did not report an active embedding attempt; no recovery mutation was sent".into(),
        ));
    };
    if attempt.attempt != request.attempt_id {
        return Err(AppError::Config(
            "requested embedding attempt does not match fresh Memories inspect".into(),
        ));
    }
    let has_verified_backup = attempt.backup_mode
        == crate::sidecar::memories_migration::BackupMode::Backup
        && inspect
            .backup
            .as_ref()
            .is_some_and(|backup| verified_backup_path(backup));
    if !recovery_action_allowed(
        request.action,
        inspect.next_action,
        attempt,
        has_verified_backup,
    ) {
        return Err(AppError::Config(format!(
            "embedding recovery action {:?} is not allowed by fresh Memories next_action {:?}",
            request.action, inspect.next_action
        )));
    }
    if request.action != EmbeddingRecoveryAction::Finalize && request.accept_failed.is_some() {
        return Err(AppError::Config(
            "accept_failed is valid only for finalize".into(),
        ));
    }
    Ok(())
}

fn validate_finalize_approval(
    request: &RecoverEmbeddingMigrationRequest,
    journal: &EmbeddingMigrationJournal,
) -> AppResult<()> {
    let Some(accepted) = request.accept_failed else {
        return Ok(());
    };
    let verified = journal
        .last_verification_counts
        .as_ref()
        .map(verification_failed_count);
    let Some(verified) = verified else {
        return Err(AppError::Config(
            "an explicit failed-count approval requires a prior durable Memories finalize verification".into(),
        ));
    };
    if accepted != verified {
        return Err(AppError::Config(format!(
            "accepted failed count {accepted} does not match the last durable Memories verification count {verified}"
        )));
    }
    Ok(())
}

fn verification_failed_count(
    counts: &crate::sidecar::memories_migration::VerificationCounts,
) -> u64 {
    [
        counts.failed_memory_text,
        counts.failed_memory_media,
        counts.failed_thread,
        counts.failed_reflection_intent,
    ]
    .into_iter()
    .fold(0, u64::saturating_add)
}

fn terminal_recovery_retry_authorized(
    request: &RecoverEmbeddingMigrationRequest,
    journal: &EmbeddingMigrationJournal,
    inspect: &EmbeddingInspect,
) -> AppResult<bool> {
    if inspect.attempt.is_some() {
        return Ok(false);
    }
    if journal.attempt_id.as_deref() != Some(request.attempt_id.as_str()) {
        return Err(AppError::Config(
            "requested embedding attempt does not match the durable application journal".into(),
        ));
    }
    let available = !matches!(
        inspect.state,
        crate::sidecar::embedding_migration::EmbeddingState::Unavailable
            | crate::sidecar::embedding_migration::EmbeddingState::Unknown
    );
    let target_matches = journal.target_space.as_deref()
        == match &inspect.space {
            SpaceValue::Id(space) => Some(space.as_str()),
            SpaceValue::Unknown | SpaceValue::None => None,
        };
    let action_is_consistent = match request.action {
        EmbeddingRecoveryAction::Finalize => {
            matches!(
                journal.phase,
                JournalPhase::Switched | JournalPhase::RebuildPending | JournalPhase::Finalized
            ) && target_matches
                && matches!(
                    inspect.next_action,
                    NextAction::None
                        | NextAction::Apply
                        | NextAction::Reconcile
                        | NextAction::ResolveFailed
                )
        }
        EmbeddingRecoveryAction::Restore => {
            matches!(
                journal.phase,
                JournalPhase::Switched
                    | JournalPhase::RebuildPending
                    | JournalPhase::Restored
                    | JournalPhase::ReconcilePending
            ) && inspect.space == journal.expected_space
                && matches!(
                    inspect.next_action,
                    NextAction::None | NextAction::Reconcile
                )
        }
        EmbeddingRecoveryAction::Abandon => {
            matches!(
                journal.phase,
                JournalPhase::Switched
                    | JournalPhase::RebuildPending
                    | JournalPhase::Abandoned
                    | JournalPhase::ReconcilePending
            ) && matches!(
                inspect.next_action,
                NextAction::None | NextAction::Reconcile
            ) && journal.backup.is_none()
        }
        EmbeddingRecoveryAction::Resume => false,
    };
    if !available || !action_is_consistent {
        return Err(AppError::Config(
            "fresh Memories inspect does not authorize retrying this completed recovery action"
                .into(),
        ));
    }
    Ok(true)
}

fn recovery_cli_request(request: &RecoverEmbeddingMigrationRequest) -> EmbeddingRequest {
    match request.action {
        EmbeddingRecoveryAction::Resume => EmbeddingRequest::Resume {
            attempt: request.attempt_id.clone(),
        },
        EmbeddingRecoveryAction::Finalize => EmbeddingRequest::Finalize {
            attempt: request.attempt_id.clone(),
            accept_failed: request.accept_failed,
        },
        EmbeddingRecoveryAction::Restore => EmbeddingRequest::Restore {
            attempt: request.attempt_id.clone(),
        },
        EmbeddingRecoveryAction::Abandon => EmbeddingRequest::Abandon {
            attempt: request.attempt_id.clone(),
        },
    }
}

fn save_chosen_settings(state: &AppState, settings: &EmbeddingSettings) -> AppResult<()> {
    save_embedding_settings(&state.data.embedding_settings_path(), settings)
}

fn journal_or_requested_settings(
    journal: Option<&EmbeddingMigrationJournal>,
    request: &RecoverEmbeddingMigrationRequest,
    state: &AppState,
    use_old: bool,
) -> AppResult<EmbeddingSettings> {
    if let Some(settings) = request.settings.as_ref() {
        return Ok(super::embedding_settings::settings_from_request(settings));
    }
    if let Some(journal) = journal {
        return Ok(if use_old {
            journal.old_settings.clone()
        } else {
            journal.new_settings.clone()
        });
    }
    let _ = state;
    Err(AppError::Config(
        "embedding migration journal is missing; choose settings explicitly before recovery".into(),
    ))
}

pub(crate) async fn wait_for_registered_worker(state: &AppState) -> AppResult<String> {
    require_local_embedding_management(state)?;
    let snapshot = state.embedding_connection_snapshot()?;
    let deadline = Instant::now() + WORKER_REGISTRATION_TIMEOUT;
    loop {
        state.ensure_embedding_connection_current(&snapshot)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(AppError::SidecarNotReady(
                "timed out waiting for the embedding worker to register".into(),
            ));
        }
        let channel = timeout(
            remaining.min(WORKER_LOOKUP_TIMEOUT),
            state.memories_channel(),
        )
        .await
        .map_err(|_| AppError::SidecarNotReady("embedding space lookup timed out".into()))??;
        let response = get_embedding_space(&channel, remaining.min(WORKER_LOOKUP_TIMEOUT)).await?;
        state.ensure_embedding_connection_current(&snapshot)?;
        if response.mm_embedding_worker_name.trim().is_empty() {
            return Err(AppError::Config(
                "Memories returned an empty embedding worker name".into(),
            ));
        }
        match response.registration_state {
            1 => return Ok(response.mm_embedding_worker_name),
            2 => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(AppError::SidecarNotReady(
                        "timed out waiting for the embedding worker to register".into(),
                    ));
                }
                sleep(WORKER_REGISTRATION_POLL_INTERVAL.min(remaining)).await;
            }
            0 => {
                return Err(AppError::Config(
                    "Memories returned an unspecified embedding worker registration state".into(),
                ));
            }
            other => {
                return Err(AppError::Config(format!(
                    "Memories returned unknown embedding worker registration state {other}"
                )));
            }
        }
    }
}

async fn get_embedding_space(
    channel: &Channel,
    budget: Duration,
) -> AppResult<crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceResponse> {
    use crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceRequest;

    let mut client = EmbeddingSpaceServiceClient::new(channel.clone());
    timeout(
        budget,
        client.get_embedding_space(GetEmbeddingSpaceRequest {}),
    )
    .await
    .map_err(|_| AppError::SidecarNotReady("embedding space lookup timed out".into()))?
    .map(|response| response.into_inner())
    .map_err(AppError::from)
}

async fn start_rebuild(state: &AppState, attempt_id: &str) -> AppResult<()> {
    super::embedding_space::start_embedding_rebuild_unlocked(
        state,
        super::embedding_space::StartEmbeddingRebuildRequest {
            attempt_id: attempt_id.to_string(),
            retry_failed: false,
        },
    )
    .await?;
    Ok(())
}

pub(crate) async fn start_reconcile(state: &AppState) -> AppResult<()> {
    super::embedding_space::start_embedding_reconcile_unlocked(
        state,
        super::embedding_space::StartEmbeddingReconcileRequest {
            retry_failed: false,
            force: false,
            force_table: String::new(),
            force_entity_ids: Vec::new(),
        },
    )
    .await?;
    Ok(())
}

async fn wait_for_embedding_writes(state: &AppState) -> AppResult<()> {
    let deadline = Instant::now() + WRITER_DRAIN_TIMEOUT;
    loop {
        if !state.search_index_maintenance.status().await.writing {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(AppError::Config(
                "timed out draining direct Memories embedding writes".into(),
            ));
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn quiesce_embedding_writers(
    state: &AppState,
    journal: &std::sync::Arc<parking_lot::Mutex<EmbeddingMigrationJournal>>,
) -> AppResult<super::periodic_execution::PausedPeriodicSchedules> {
    state.close_embedding_migration_admission().await;
    state
        .cancel_dispatches_and_wait(DISPATCH_DRAIN_TIMEOUT)
        .await?;
    wait_for_embedding_writes(state).await?;
    let journal_for_snapshot = journal.clone();
    let data = state.data.clone();
    let mut paused = super::periodic_execution::pause_periodic_schedules_for_restart_with(
        state,
        move |paused| {
            let mut updated = journal_for_snapshot.lock().clone();
            merge_paused_scheduler_ids(&mut updated, &paused.enabled_scheduler_ids_vec()?);
            save_journal(&data, &updated)?;
            *journal_for_snapshot.lock() = updated;
            Ok(())
        },
    )
    .await?;
    let previously_enabled = journal.lock().paused_periodic_scheduler_ids.clone();
    paused.include_scheduler_ids(&previously_enabled)?;
    Ok(paused)
}

fn target_for_result(plan: &EmbeddingPlan) -> String {
    plan.target_space.clone()
}

async fn fresh_plan_for_switch(
    state: &AppState,
    settings: &SetEmbeddingSettingsRequest,
    expected_space: &SpaceValue,
) -> AppResult<(EmbeddingPlan, EmbeddingInspect)> {
    let inspection_result = inspect_cli(state).await?;
    let inspect = fresh_switch_inspect_result(inspection_result, expected_space)?;
    let plan_result = plan_cli(state, settings).await?;
    fresh_switch_plan_result(inspect, plan_result)
}

fn fresh_switch_inspect_result(
    inspection_result: EmbeddingMigrationResult,
    expected_space: &SpaceValue,
) -> AppResult<EmbeddingInspect> {
    let inspect = inspect_view(&inspection_result)?.clone();
    if inspect.state == crate::sidecar::embedding_migration::EmbeddingState::Unavailable {
        return Err(AppError::Config(
            "Memories embedding state is unavailable; switch was not attempted".into(),
        ));
    }
    if &inspect.space != expected_space {
        return Err(AppError::Config(
            "embedding space changed after planning; inspect and plan again before switching"
                .into(),
        ));
    }
    Ok(inspect)
}

fn fresh_switch_plan_result(
    inspect: EmbeddingInspect,
    plan_result: EmbeddingMigrationResult,
) -> AppResult<(EmbeddingPlan, EmbeddingInspect)> {
    let plan = plan_view(&plan_result)?.clone();
    if !plan.executable || plan.decision != PlanDecision::ReembedRequired {
        return Err(AppError::Config(format!(
            "embedding switch requires an executable reembed_required plan; got {:?} (executable={})",
            plan.decision, plan.executable
        )));
    }
    Ok((plan, inspect))
}

async fn prepare_prepared_switch_retry_with<I, IFut, R, RFut, P, PFut>(
    data: &crate::data::DataPaths,
    mut journal: EmbeddingMigrationJournal,
    settings: &SetEmbeddingSettingsRequest,
    expected_space: SpaceValue,
    inspect_cli: I,
    reconcile_and_restore_schedulers: R,
    plan_cli: P,
) -> AppResult<(EmbeddingMigrationJournal, EmbeddingPlan, EmbeddingInspect)>
where
    I: FnOnce() -> IFut,
    IFut: std::future::Future<Output = AppResult<EmbeddingMigrationResult>>,
    R: FnOnce(EmbeddingMigrationJournal, EmbeddingInspect, Vec<i64>) -> RFut,
    RFut: std::future::Future<Output = AppResult<EmbeddingInspect>>,
    P: FnOnce() -> PFut,
    PFut: std::future::Future<Output = AppResult<EmbeddingMigrationResult>>,
{
    if journal.phase != JournalPhase::Prepared
        || journal.attempt_id.is_some()
        || journal.attempt.is_some()
    {
        return Err(AppError::Config(
            "an embedding migration attempt is already journaled; recover it before retrying a switch".into(),
        ));
    }
    if journal.new_settings != super::embedding_settings::settings_from_request(settings)
        || journal.expected_space != expected_space
    {
        return Err(AppError::Config(
            "prepared embedding switch retry does not match the original settings and expected space".into(),
        ));
    }
    if journal.target_space.is_none() {
        return Err(AppError::Config(
            "prepared embedding switch journal is missing its planned target".into(),
        ));
    }

    let inspect_result = inspect_cli().await?;
    let inspect = inspect_view(&inspect_result)?.clone();
    if update_journal_attempt_from_inspect(&mut journal, &inspect)? {
        save_journal(data, &journal)?;
    }
    if inspect.attempt.is_some() {
        return Err(AppError::Config(
            "Memories has an active embedding attempt; recover it before retrying a switch".into(),
        ));
    }
    if !prepared_journal_cleanup_is_authorized(&journal, &inspect) {
        return Err(AppError::Config(
            "prepared embedding switch cannot be retried until a fresh inspect confirms the original space is safe to reconcile".into(),
        ));
    }

    let fresh_inspect = reconcile_and_restore_schedulers(
        journal.clone(),
        inspect.clone(),
        journal.paused_periodic_scheduler_ids.clone(),
    )
    .await?;
    if fresh_inspect.attempt.is_some() {
        return Err(AppError::Config(
            "Memories started an embedding attempt while reconciling the prepared switch; recover it before retrying".into(),
        ));
    }
    if !prepared_journal_cleanup_is_authorized(&journal, &fresh_inspect) {
        return Err(AppError::Config(
            "Memories embedding state is not safe after prepared-switch reconciliation".into(),
        ));
    }
    clear_scheduler_pause_ledger(data, &mut journal)?;
    remove_journal(data)?;

    let plan_result = plan_cli().await?;
    let (plan, fresh_inspect) = fresh_switch_plan_result(fresh_inspect, plan_result)?;
    if fresh_inspect.space != expected_space
        || plan.current_space != expected_space
        || plan.target_space != journal.target_space.as_deref().unwrap_or_default()
    {
        return Err(AppError::Config(
            "fresh embedding plan does not match the prepared switch target and expected space"
                .into(),
        ));
    }
    Ok((journal, plan, fresh_inspect))
}

async fn reconcile_and_restore_prepared_switch_schedulers(
    app: &tauri::AppHandle,
    state: &AppState,
    journal: EmbeddingMigrationJournal,
    inspected: EmbeddingInspect,
    scheduler_ids: Vec<i64>,
) -> AppResult<EmbeddingInspect> {
    let requires_reconcile = inspected.next_action == NextAction::Reconcile;
    let state_ref = state;
    let data = state.data.clone();
    let sidecar_warnings = super::stage_plugins_for_restart(app, &data.plugins_dir());
    let journal_for_inspect = journal.clone();
    let journal_for_post = journal;
    let post_reconcile_inspect = std::sync::Arc::new(parking_lot::Mutex::new(None));
    let inspect_for_post = post_reconcile_inspect.clone();
    let transaction = state
        .sidecars
        .embedding_migration_transaction(
            sidecar_warnings,
            move || async move {
                state_ref.close_embedding_migration_admission().await;
                state_ref
                    .cancel_dispatches_and_wait(DISPATCH_DRAIN_TIMEOUT)
                    .await?;
                wait_for_embedding_writes(state_ref).await?;
                state_ref.invalidate_clients().await;
                Ok(())
            },
            move || async move {
                state_ref.sidecars.run_local_apply_for_maintenance().await?;
                let fresh_result = inspect_cli(state_ref).await?;
                let fresh_inspect = inspect_view(&fresh_result)?.clone();
                let mut journal = journal_for_inspect;
                if update_journal_attempt_from_inspect(&mut journal, &fresh_inspect)? {
                    save_journal(&state_ref.data, &journal)?;
                }
                if fresh_inspect.attempt.is_some()
                    || !prepared_journal_cleanup_is_authorized(&journal, &fresh_inspect)
                {
                    return Err(AppError::Config(
                        "Memories embedding state changed before prepared-switch reconciliation"
                            .into(),
                    ));
                }
                Ok(crate::sidecar::lifecycle::EmbeddingMaintenanceDecision::Start(()))
            },
            move |()| async move {
                wait_for_registered_worker(state_ref).await?;
                if requires_reconcile {
                    start_reconcile(state_ref).await?;
                }
                let result = inspect_cli(state_ref).await?;
                let inspect = inspect_view(&result)?.clone();
                let mut journal = journal_for_post;
                if update_journal_attempt_from_inspect(&mut journal, &inspect)? {
                    save_journal(&state_ref.data, &journal)?;
                }
                if inspect.attempt.is_some()
                    || !prepared_journal_cleanup_is_authorized(&journal, &inspect)
                {
                    return Err(AppError::Config(
                        "Memories embedding state is not safe after prepared-switch reconciliation"
                            .into(),
                    ));
                }
                *inspect_for_post.lock() = Some(inspect);
                restore_scheduler_ids_after_startup(state_ref, scheduler_ids).await
            },
        )
        .await?;
    match transaction {
        crate::sidecar::lifecycle::EmbeddingMaintenanceOutcome::Started { report, .. } => {
            let _ = app.emit("sidecar://ready", &report);
            post_reconcile_inspect.lock().take().ok_or_else(|| {
                AppError::Config(
                    "prepared embedding retry did not retain its post-reconcile inspect".into(),
                )
            })
        }
        crate::sidecar::lifecycle::EmbeddingMaintenanceOutcome::Stopped { .. } => Err(
            AppError::Config("prepared embedding retry did not restore sidecars".into()),
        ),
    }
}

fn make_journal(
    old_settings: EmbeddingSettings,
    new_settings: EmbeddingSettings,
    expected_space: SpaceValue,
    target_space: Option<String>,
    paused_periodic_scheduler_ids: Vec<i64>,
) -> EmbeddingMigrationJournal {
    EmbeddingMigrationJournal {
        version: JOURNAL_VERSION,
        phase: JournalPhase::Prepared,
        old_settings,
        new_settings,
        expected_space,
        target_space,
        attempt_id: None,
        attempt: None,
        backup: None,
        accepted_failed: None,
        last_verification_counts: None,
        paused_periodic_scheduler_ids,
    }
}

#[tauri::command]
pub(crate) async fn switch_embedding_space(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    req: SwitchEmbeddingSpaceRequest,
) -> AppResult<EmbeddingMigrationOperationResponse> {
    require_local_embedding_management(&state)?;
    let _recovery = state.database_migration_recovery.lock().await;
    let _dispatch_gate = state.lock_llm_settings_dispatch_gate().await;
    let target_runtime = embedding_runtime_for_request_with_env(&req.settings, |key| {
        state.sidecars.embedding_env_lookup(key)
    })?;
    let target_settings = super::embedding_settings::settings_from_request(&req.settings);
    let old_settings =
        super::embedding_settings::load_embedding_settings(&state.data.embedding_settings_path());
    let existing_journal = load_journal(&state.data)?;
    let mut prepared_retry_recovered = false;
    let (plan, inspect) = if let Some(journal) = existing_journal {
        if journal.phase != JournalPhase::Prepared
            || journal.attempt_id.is_some()
            || journal.attempt.is_some()
        {
            state.close_embedding_migration_admission().await;
            return Err(AppError::Config(
                "an embedding migration attempt is already journaled; inspect and recover it before starting another switch".into(),
            ));
        }
        let data = state.data.clone();
        let settings = req.settings.clone();
        let expected_space = req.expected_space.clone();
        let retry = prepare_prepared_switch_retry_with(
            &data,
            journal,
            &settings,
            expected_space,
            || async { inspect_cli(&state).await },
            |journal, inspect, scheduler_ids| async {
                reconcile_and_restore_prepared_switch_schedulers(
                    &app,
                    &state,
                    journal,
                    inspect,
                    scheduler_ids,
                )
                .await
            },
            || async { plan_cli(&state, &settings).await },
        )
        .await;
        let (cleared, plan, inspect) = match retry {
            Ok(prepared) => prepared,
            Err(error) => {
                if load_journal(&data)?.is_none() {
                    state.open_embedding_migration_admission().await;
                } else {
                    state.close_embedding_migration_admission().await;
                }
                return Err(error);
            }
        };
        debug_assert!(cleared.paused_periodic_scheduler_ids.is_empty());
        prepared_retry_recovered = true;
        (plan, inspect)
    } else {
        fresh_plan_for_switch(&state, &req.settings, &req.expected_space).await?
    };
    let journal = make_journal(
        old_settings,
        target_settings.clone(),
        inspect.space,
        Some(target_for_result(&plan)),
        Vec::new(),
    );
    // The pre-stop phase is the durable recovery authority: a crash at any
    // point after this write can be reconciled against a fresh Memories
    // inspect without inventing scheduler state.
    if let Err(error) = save_journal(&state.data, &journal) {
        if prepared_retry_recovered {
            state.open_embedding_migration_admission().await;
        }
        return Err(error);
    }

    let sidecars = state.sidecars.clone();
    let data = state.data.clone();
    let sidecar_warnings = super::stage_plugins_for_restart(&app, &data.plugins_dir());
    let cli_request = EmbeddingRequest::Switch {
        target: target_for_runtime(&target_runtime),
        expected_space: req.expected_space,
        backup: req.backup,
        maintenance_window_ack: true,
    };
    let state_ref = &*state;
    let journal_for_cli = std::sync::Arc::new(parking_lot::Mutex::new(journal.clone()));
    let journal_for_quiesce = journal_for_cli.clone();
    let journal_for_post = journal_for_cli.clone();
    let paused_schedules = std::sync::Arc::new(parking_lot::Mutex::new(None));
    let paused_for_quiesce = paused_schedules.clone();
    let paused_for_post = paused_schedules.clone();
    let transaction = sidecars
        .embedding_migration_transaction(
            sidecar_warnings,
            move || {
                let paused_for_quiesce = paused_for_quiesce.clone();
                let journal_for_quiesce = journal_for_quiesce.clone();
                let state_ref = state_ref;
                async move {
                    match quiesce_embedding_writers(state_ref, &journal_for_quiesce).await {
                        Ok(paused) => {
                            *paused_for_quiesce.lock() = Some(paused);
                            state_ref.invalidate_clients().await;
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                }
            },
            move || {
                let journal_for_cli = journal_for_cli.clone();
                let state_ref = state_ref;
                let target_runtime = target_runtime.clone();
                async move {
                    let mut journal = journal_for_cli.lock().clone();
                    save_journal(&state_ref.data, &journal)?;
                    state_ref.sidecars.run_local_apply_for_maintenance().await?;

                    // Re-inspect after every writer and sidecar has stopped.
                    let current = inspect_cli(state_ref).await?;
                    let current_inspect = inspect_view(&current)?.clone();
                    if current_inspect.space != journal.expected_space {
                        return Err(AppError::Config(
                            "embedding space changed before switch; no migration CLI was sent".into(),
                        ));
                    }
                    let current_plan = plan_cli(state_ref, &req.settings).await?;
                    let current_plan = plan_view(&current_plan)?;
                    if !current_plan.executable
                        || current_plan.decision != PlanDecision::ReembedRequired
                        || current_plan.current_space != journal.expected_space
                        || current_plan.target_space != journal.target_space.as_deref().unwrap_or_default()
                    {
                        return Err(AppError::Config(
                            "embedding plan changed before switch; no migration CLI was sent".into(),
                        ));
                    }
                    let result = state_ref
                        .sidecars
                        .run_embedding_cli_with_runtime(cli_request, &target_runtime)
                        .await?;
                    let EmbeddingMigrationResult::Switched(success) = &result else {
                        if matches!(result, EmbeddingMigrationResult::Failed(_)) {
                            return Ok(crate::sidecar::lifecycle::EmbeddingMaintenanceDecision::KeepStopped(result));
                        }
                        return Err(cli_failure(&result, "switch"));
                    };
                    journal.attempt_id = Some(success.attempt.clone());
                    if let Some(backup) = success
                        .backup
                        .as_deref()
                        .filter(|backup| verified_backup_path(backup))
                    {
                        journal.backup = Some(backup.to_string());
                    }
                    journal.phase = JournalPhase::Switched;
                    save_journal(&state_ref.data, &journal)?;
                    let switched_inspection = inspect_cli(state_ref).await?;
                    let switched_inspect = inspect_view(&switched_inspection)?;
                    if update_journal_attempt_from_inspect(&mut journal, switched_inspect)? {
                        save_journal(&state_ref.data, &journal)?;
                    }
                    save_chosen_settings(state_ref, &target_settings)?;
                    *journal_for_cli.lock() = journal;
                    Ok(crate::sidecar::lifecycle::EmbeddingMaintenanceDecision::Start(result))
                }
            },
            move |result: EmbeddingMigrationResult| {
                let state_ref = state_ref;
                let paused_for_post = paused_for_post.clone();
                let journal_for_post = journal_for_post.clone();
                async move {
                    let EmbeddingMigrationResult::Switched(success) = &result else {
                        return Err(cli_failure(&result, "switch"));
                    };
                    wait_for_registered_worker(state_ref).await?;
                    start_rebuild(state_ref, &success.attempt).await?;
                    let mut journal = journal_for_post.lock().clone();
                    journal.phase = JournalPhase::RebuildPending;
                    journal.attempt_id = Some(success.attempt.clone());
                    save_journal(&state_ref.data, &journal)?;
                    let paused = paused_for_post
                        .lock()
                        .take()
                        .ok_or_else(|| AppError::Config("periodic schedulers were not paused".into()))?;
                    paused.resume_after_restart(state_ref).await?;
                    clear_scheduler_pause_ledger(&state_ref.data, &mut journal)?;
                    *journal_for_post.lock() = journal;
                    state_ref.open_embedding_migration_admission().await;
                    Ok(())
                }
            },
        )
        .await;
    match transaction {
        Ok(crate::sidecar::lifecycle::EmbeddingMaintenanceOutcome::Started { value, report }) => {
            let _ = app.emit("sidecar://ready", &report);
            Ok(EmbeddingMigrationOperationResponse {
                result: value,
                restarted: true,
            })
        }
        Ok(crate::sidecar::lifecycle::EmbeddingMaintenanceOutcome::Stopped { value }) => {
            Ok(EmbeddingMigrationOperationResponse {
                result: value,
                restarted: false,
            })
        }
        Err(error) => Err(error),
    }
}

async fn recovery_plan_for_settings(
    state: &AppState,
    settings: &EmbeddingSettings,
) -> AppResult<EmbeddingPlan> {
    let request = SetEmbeddingSettingsRequest {
        preset_id: settings.preset_id.clone(),
        custom_model_id: settings.custom_model_id.clone(),
        custom_tokenizer_id: settings.custom_tokenizer_id.clone(),
        custom_vector_size: settings.custom_vector_size,
        custom_dtype: settings.custom_dtype.clone(),
        custom_max_sequence_length: settings.custom_max_sequence_length,
        custom_is_multimodal: settings.custom_is_multimodal,
        model_revision: settings.model_revision.clone(),
    };
    let result = plan_cli(state, &request).await?;
    plan_view(&result).cloned()
}

#[tauri::command]
pub(crate) async fn recover_embedding_migration(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    req: RecoverEmbeddingMigrationRequest,
) -> AppResult<EmbeddingMigrationOperationResponse> {
    require_local_embedding_management(&state)?;
    if req.settings.is_some()
        && !matches!(
            req.action,
            EmbeddingRecoveryAction::Restore | EmbeddingRecoveryAction::Abandon
        )
    {
        return Err(AppError::Config(
            "recovery settings may be supplied only for restore or abandon".into(),
        ));
    }
    let _recovery = state.database_migration_recovery.lock().await;
    let _dispatch_gate = state.lock_llm_settings_dispatch_gate().await;
    let inspection_result = inspect_cli(&state).await?;
    let inspect = inspect_view(&inspection_result)?.clone();
    let journal = journal_attempt_from_inspect(&state.data, &inspect)?;
    let replay_without_active_attempt = match (journal.as_ref(), inspect.attempt.as_ref()) {
        (Some(journal), None) => terminal_recovery_retry_authorized(&req, journal, &inspect)?,
        _ => false,
    };
    if !replay_without_active_attempt {
        validate_recovery_action(&req, &inspect)?;
        if let Some(journal) = journal.as_ref()
            && journal.attempt_id.as_deref() != Some(req.attempt_id.as_str())
        {
            return Err(AppError::Config(
                "recovery attempt does not match the durable application journal".into(),
            ));
        }
    }
    let mut journal = if let Some(journal) = journal {
        journal
    } else {
        let settings = journal_or_requested_settings(None, &req, &state, false)?;
        make_journal(
            settings.clone(),
            settings,
            inspect.space.clone(),
            None,
            Vec::new(),
        )
    };
    journal.attempt_id = Some(req.attempt_id.clone());
    if update_journal_attempt_from_inspect(&mut journal, &inspect)? {
        save_journal(&state.data, &journal)?;
    } else {
        // Persist the exact requested attempt before quiescing, even when the
        // inspect payload did not add a newer backup or stage observation.
        save_journal(&state.data, &journal)?;
    }
    if !replay_without_active_attempt {
        validate_finalize_approval(&req, &journal)?;
    }

    let sidecars = state.sidecars.clone();
    let sidecar_warnings = super::stage_plugins_for_restart(&app, &state.data.plugins_dir());
    let cli_request = recovery_cli_request(&req);
    let state_ref = &*state;
    let journal_state = std::sync::Arc::new(parking_lot::Mutex::new(journal.clone()));
    let journal_for_post = journal_state.clone();
    let journal_for_quiesce = journal_state.clone();
    let paused_schedules = std::sync::Arc::new(parking_lot::Mutex::new(None));
    let paused_for_quiesce = paused_schedules.clone();
    let paused_for_post = paused_schedules.clone();
    let replay_for_operation = replay_without_active_attempt;
    let resume_rebuild = req.action == EmbeddingRecoveryAction::Resume;
    let finalize = req.action == EmbeddingRecoveryAction::Finalize;
    let restore = req.action == EmbeddingRecoveryAction::Restore;
    let abandon = req.action == EmbeddingRecoveryAction::Abandon;
    let request_for_preflight = req.clone();
    let reconcile_after_start = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reconcile_for_post = reconcile_after_start.clone();

    let transaction = sidecars
        .embedding_migration_transaction(
            sidecar_warnings,
            move || {
                let paused_for_quiesce = paused_for_quiesce.clone();
                let journal_for_quiesce = journal_for_quiesce.clone();
                let state_ref = state_ref;
                async move {
                    let paused = match quiesce_embedding_writers(state_ref, &journal_for_quiesce).await {
                        Ok(paused) => paused,
                        Err(error) => return Err(error),
                    };
                    *paused_for_quiesce.lock() = Some(paused);
                    state_ref.invalidate_clients().await;
                    Ok(())
                }
            },
            move || {
                let state_ref = state_ref;
                let journal_state = journal_state.clone();
                let request_for_preflight = request_for_preflight.clone();
                let reconcile_after_start = reconcile_after_start.clone();
                let replay_without_active_attempt = replay_for_operation;
                async move {
                    let fresh = inspect_cli(state_ref).await?;
                    let fresh_inspect = inspect_view(&fresh)?.clone();
                    let mut journal = journal_state.lock().clone();
                    update_journal_attempt_from_inspect(&mut journal, &fresh_inspect)?;
                    save_journal(&state_ref.data, &journal)?;
                    let result = if replay_without_active_attempt {
                        if !terminal_recovery_retry_authorized(
                            &request_for_preflight,
                            &journal,
                            &fresh_inspect,
                        )? {
                            return Err(AppError::Config(
                                "embedding attempt became active while replaying a completed recovery".into(),
                            ));
                        }
                        state_ref.sidecars.run_embedding_cli(cli_request).await?
                    } else {
                        validate_recovery_action(&request_for_preflight, &fresh_inspect)?;
                        if fresh_inspect
                            .attempt
                            .as_ref()
                            .map(|attempt| attempt.attempt.as_str())
                            != Some(request_for_preflight.attempt_id.as_str())
                        {
                            return Err(AppError::Config(
                                "embedding attempt changed before recovery; no recovery CLI was sent".into(),
                            ));
                        }
                        state_ref.sidecars.run_embedding_cli(cli_request).await?
                    };
                    if matches!(result, EmbeddingMigrationResult::Failed(_)) {
                        if finalize
                            && let EmbeddingMigrationResult::Failed(failure) = &result
                            && failure.error_code
                                == crate::sidecar::memories_migration::CliErrorCode::RebuildHasFailures
                        {
                            journal.last_verification_counts =
                                failure.verification_counts.clone();
                            save_journal(&state_ref.data, &journal)?;
                            *journal_state.lock() = journal;
                        }
                        return Ok(crate::sidecar::lifecycle::EmbeddingMaintenanceDecision::KeepStopped(result));
                    }

                    if resume_rebuild
                        && let EmbeddingMigrationResult::Switched(success) = &result
                    {
                        journal.attempt_id = Some(success.attempt.clone());
                        if let Some(backup) = success
                            .backup
                            .as_deref()
                            .filter(|backup| verified_backup_path(backup))
                        {
                            journal.backup = Some(backup.to_string());
                        }
                        let resumed_inspection = inspect_cli(state_ref).await?;
                        let resumed_inspect = inspect_view(&resumed_inspection)?;
                        update_journal_attempt_from_inspect(&mut journal, resumed_inspect)?;
                        save_journal(&state_ref.data, &journal)?;
                        *journal_state.lock() = journal.clone();
                    }

                    let (settings, phase, needs_reconcile, allow_reembed) = match &result {
                        EmbeddingMigrationResult::Switched(success) if resume_rebuild => {
                            let settings = journal_or_requested_settings(
                                Some(&journal),
                                &request_for_preflight,
                                state_ref,
                                false,
                            )?;
                            (settings, JournalPhase::Switched, false, false)
                        }
                        EmbeddingMigrationResult::Finalized(success) if finalize => {
                            let settings = journal_or_requested_settings(
                                Some(&journal),
                                &request_for_preflight,
                                state_ref,
                                false,
                            )?;
                            journal.accepted_failed = Some(success.accepted_failed);
                            (
                                settings,
                                JournalPhase::Finalized,
                                success.next_action == NextAction::Reconcile,
                                false,
                            )
                        }
                        EmbeddingMigrationResult::Restored(success) if restore => {
                            let settings = journal_or_requested_settings(
                                Some(&journal),
                                &request_for_preflight,
                                state_ref,
                                true,
                            )?;
                            (
                                settings,
                                JournalPhase::ReconcilePending,
                                success.next_action == NextAction::Reconcile,
                                false,
                            )
                        }
                        EmbeddingMigrationResult::Abandoned(success) if abandon => {
                            let use_old = success.outcome
                                != crate::sidecar::embedding_migration::AbandonOutcome::Discarded
                                && success.next_action == NextAction::None;
                            let settings = journal_or_requested_settings(
                                Some(&journal),
                                &request_for_preflight,
                                state_ref,
                                use_old,
                            )?;
                            (
                                settings,
                                if success.next_action == NextAction::Reconcile {
                                    JournalPhase::ReconcilePending
                                } else {
                                    JournalPhase::Abandoned
                                },
                                success.next_action == NextAction::Reconcile,
                                success.outcome
                                    == crate::sidecar::embedding_migration::AbandonOutcome::Discarded,
                            )
                        }
                        _ => return Err(cli_failure(&result, "recovery")),
                    };

                    if let Some(settings) = request_for_preflight.settings.as_ref()
                        && (restore || abandon)
                    {
                        journal.new_settings = super::embedding_settings::settings_from_request(settings);
                    }
                    save_chosen_settings(state_ref, &settings)?;
                    journal.phase = phase;
                    journal.attempt_id = Some(request_for_preflight.attempt_id.clone());
                    save_journal(&state_ref.data, &journal)?;
                    *journal_state.lock() = journal.clone();

                    // Applying the regular schema/workflow migrations is part
                    // of each recovery continuation and must precede startup.
                    state_ref.sidecars.run_local_apply_for_maintenance().await?;
                    if needs_reconcile {
                        let post_apply = inspect_cli(state_ref).await?;
                        let post_apply_inspect = inspect_view(&post_apply)?;
                        if post_apply_inspect.state == crate::sidecar::embedding_migration::EmbeddingState::Unavailable
                            || post_apply_inspect.attempt.is_some()
                        {
                            return Err(AppError::Config(
                                "Memories inspect did not confirm a stable restored embedding state".into(),
                            ));
                        }
                        let plan = recovery_plan_for_settings(state_ref, &settings).await?;
                        let target_mismatch = match &post_apply_inspect.space {
                            SpaceValue::Id(space) => plan.target_space != *space,
                            SpaceValue::Unknown | SpaceValue::None => false,
                        };
                        if !plan.executable
                            || plan.current_space != post_apply_inspect.space
                            || (!allow_reembed
                                && plan.decision == PlanDecision::ReembedRequired)
                            || (!allow_reembed && target_mismatch)
                        {
                            journal.phase = JournalPhase::ReconcilePending;
                            save_journal(&state_ref.data, &journal)?;
                            *journal_state.lock() = journal.clone();
                            return Ok(crate::sidecar::lifecycle::EmbeddingMaintenanceDecision::KeepStopped(
                                EmbeddingMigrationResult::Plan(plan),
                            ));
                        }
                    }
                    reconcile_after_start.store(needs_reconcile, Ordering::Release);
                    Ok(crate::sidecar::lifecycle::EmbeddingMaintenanceDecision::Start(result))
                }
            },
            move |result: EmbeddingMigrationResult| {
                let state_ref = state_ref;
                let journal_for_post = journal_for_post.clone();
                let paused_for_post = paused_for_post.clone();
                async move {
                    let needs_reconcile = reconcile_for_post.load(Ordering::Acquire);
                    if resume_rebuild || needs_reconcile {
                        wait_for_registered_worker(state_ref).await?;
                    }
                    if let EmbeddingMigrationResult::Switched(success) = &result
                        && resume_rebuild
                    {
                        start_rebuild(state_ref, &success.attempt).await?;
                        let mut journal = journal_for_post.lock().clone();
                        journal.phase = JournalPhase::RebuildPending;
                        save_journal(&state_ref.data, &journal)?;
                    }
                    if reconcile_for_post.load(Ordering::Acquire) {
                        start_reconcile(state_ref).await?;
                    }
                    let paused = paused_for_post
                        .lock()
                        .take()
                        .ok_or_else(|| AppError::Config("periodic schedulers were not paused".into()))?;
                    paused.resume_after_restart(state_ref).await?;
                    let mut journal = journal_for_post.lock().clone();
                    clear_scheduler_pause_ledger(&state_ref.data, &mut journal)?;
                    *journal_for_post.lock() = journal;
                    state_ref.open_embedding_migration_admission().await;
                    Ok(())
                }
            },
        )
        .await;

    match transaction {
        Ok(crate::sidecar::lifecycle::EmbeddingMaintenanceOutcome::Started { value, report }) => {
            let _ = app.emit("sidecar://ready", &report);
            if finalize || restore || abandon {
                remove_journal(&state.data)?;
            }
            Ok(EmbeddingMigrationOperationResponse {
                result: value,
                restarted: true,
            })
        }
        Ok(crate::sidecar::lifecycle::EmbeddingMaintenanceOutcome::Stopped { value }) => {
            Ok(EmbeddingMigrationOperationResponse {
                result: value,
                restarted: false,
            })
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(decision: PlanDecision, executable: bool) -> EmbeddingPlan {
        EmbeddingPlan {
            decision,
            executable,
            reason: None,
            current_space: SpaceValue::Unknown,
            target_space: "target".into(),
            memory_text: 0,
            memory_media: 0,
            thread: 0,
            reflection_intent: 0,
            failed_transient: None,
            failed_permanent: Some(0),
        }
    }

    #[test]
    fn plan_target_uses_the_same_effective_revision_as_the_runtime() {
        let request = SetEmbeddingSettingsRequest {
            preset_id: Some("qwen3-embedding-0-6b".into()),
            custom_model_id: None,
            custom_tokenizer_id: None,
            custom_vector_size: None,
            custom_dtype: None,
            custom_max_sequence_length: None,
            custom_is_multimodal: None,
            model_revision: Some("new-r".into()),
        };

        let target = plan_target_with_env(&request, |key| match key {
            "MEMORY_EMBEDDING_MODEL_REVISION" => Some("env-file-old-r".into()),
            _ => None,
        })
        .unwrap();

        assert_eq!(target.revision, "new-r");

        let request_without_saved_revision = SetEmbeddingSettingsRequest {
            model_revision: None,
            ..request
        };
        let target = plan_target_with_env(&request_without_saved_revision, |key| match key {
            "MEMORY_EMBEDDING_MODEL_REVISION" => Some("env-file-default-r".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(target.revision, "env-file-default-r");
    }

    #[test]
    fn only_executable_non_rebuild_plans_authorize_ordinary_settings_saves() {
        assert_eq!(
            permit_for_plan(&plan(PlanDecision::NoChange, true)).unwrap(),
            EmbeddingPlanPermit::NoChange
        );
        assert_eq!(
            permit_for_plan(&plan(PlanDecision::ReconcileRequired, true)).unwrap(),
            EmbeddingPlanPermit::Reconcile
        );
        assert_eq!(
            permit_for_plan(&plan(PlanDecision::AdoptOnStart, true)).unwrap(),
            EmbeddingPlanPermit::Adopt
        );
        assert!(permit_for_plan(&plan(PlanDecision::ReembedRequired, true)).is_err());

        for decision in [
            PlanDecision::NoChange,
            PlanDecision::ReconcileRequired,
            PlanDecision::AdoptOnStart,
            PlanDecision::ReembedRequired,
        ] {
            assert!(
                permit_for_plan(&plan(decision, false)).is_err(),
                "a non-executable {decision:?} plan must not authorize a save"
            );
        }
    }

    #[test]
    fn target_root_approval_allows_cli_adoption_but_guards_pending_chunk_changes() {
        use crate::sidecar::memories_migration::AttemptStage;

        let old = super::super::embedding_settings::resolve_embedding_runtime(
            &EmbeddingSettings::default(),
        );
        let mut changed_chunk = old.clone();
        changed_chunk.max_sequence_length += 1;

        let mut fresh = inspect_with_stage(None);
        fresh.state = crate::sidecar::embedding_migration::EmbeddingState::Unknown;
        fresh.space = SpaceValue::Unknown;
        fresh.next_action = NextAction::Switch;
        let adoption = plan(PlanDecision::AdoptOnStart, true);
        let approval = target_root_approval(&fresh, &old, &changed_chunk, Some(&adoption))
            .unwrap()
            .unwrap();
        assert_eq!(approval.permit, EmbeddingPlanPermit::Adopt);
        assert!(!approval.rebuild_pending);

        fresh.state = crate::sidecar::embedding_migration::EmbeddingState::Migrating;
        fresh.attempt = Some(crate::sidecar::memories_migration::EmbeddingAttemptInfo {
            attempt: "attempt-a".into(),
            stage: AttemptStage::RebuildPending,
            backup_mode: crate::sidecar::memories_migration::BackupMode::Backup,
        });
        assert!(target_root_approval(&fresh, &old, &changed_chunk, None).is_err());

        changed_chunk.max_sequence_length = old.max_sequence_length;
        let pending = target_root_approval(&fresh, &old, &changed_chunk, None)
            .unwrap()
            .unwrap();
        assert!(pending.rebuild_pending);
        assert_eq!(pending.permit, EmbeddingPlanPermit::NoChange);
    }

    #[test]
    fn unknown_inspect_counts_are_not_interpreted_as_empty_data() {
        let mut inspect = EmbeddingInspect {
            state: crate::sidecar::embedding_migration::EmbeddingState::Unknown,
            space: SpaceValue::Unknown,
            required: None,
            complete: Some(0),
            missing: None,
            stale: None,
            unverified: None,
            failed_transient: None,
            failed_permanent: None,
            orphan: None,
            backup_supported: None,
            attempt: None,
            backup: None,
            unavailable_reason: None,
            next_action: NextAction::ManualRecovery,
        };
        assert_eq!(inspect.required, None);
        assert!(permit_for_plan(&plan(PlanDecision::AdoptOnStart, false)).is_err());
        inspect.complete = None;
        assert_eq!(inspect.complete, None);
    }

    #[test]
    fn recovery_actions_follow_fresh_next_action_and_require_exact_attempt() {
        let request = RecoverEmbeddingMigrationRequest {
            action: EmbeddingRecoveryAction::Finalize,
            attempt_id: "attempt-a".into(),
            accept_failed: Some(2),
            settings: None,
        };
        let inspect = EmbeddingInspect {
            state: crate::sidecar::embedding_migration::EmbeddingState::Failed,
            space: SpaceValue::Id("target".into()),
            required: Some(4),
            complete: Some(2),
            missing: Some(0),
            stale: Some(0),
            unverified: Some(0),
            failed_transient: Some(1),
            failed_permanent: Some(1),
            orphan: Some(0),
            backup_supported: Some(true),
            attempt: Some(crate::sidecar::memories_migration::EmbeddingAttemptInfo {
                attempt: "attempt-a".into(),
                stage: crate::sidecar::memories_migration::AttemptStage::RebuildPending,
                backup_mode: crate::sidecar::memories_migration::BackupMode::Backup,
            }),
            backup: Some("/backup".into()),
            unavailable_reason: None,
            next_action: NextAction::ResolveFailed,
        };
        assert!(validate_recovery_action(&request, &inspect).is_ok());

        let mut stale_request = request.clone();
        stale_request.attempt_id = "another-attempt".into();
        assert!(validate_recovery_action(&stale_request, &inspect).is_err());

        let mut no_approval = request.clone();
        no_approval.accept_failed = None;
        assert!(validate_recovery_action(&no_approval, &inspect).is_ok());

        let mut ordinary_failed = inspect.clone();
        ordinary_failed.attempt = None;
        ordinary_failed.next_action = NextAction::ResolveFailed;
        assert!(validate_recovery_action(&request, &ordinary_failed).is_err());
    }

    #[test]
    fn finalize_uses_cli_verified_counts_instead_of_inspect_unknowns_or_zero_defaults() {
        use crate::sidecar::memories_migration::VerificationCounts;

        let mut inspect = inspect_with_stage(Some(
            crate::sidecar::memories_migration::AttemptStage::RebuildPending,
        ));
        inspect.next_action = NextAction::StartRebuild;
        inspect.failed_transient = None;
        inspect.failed_permanent = None;
        let mut request = RecoverEmbeddingMigrationRequest {
            action: EmbeddingRecoveryAction::Finalize,
            attempt_id: "attempt-a".into(),
            accept_failed: None,
            settings: None,
        };

        assert!(validate_recovery_action(&request, &inspect).is_ok());

        let mut journal = make_journal(
            EmbeddingSettings::default(),
            EmbeddingSettings::default(),
            SpaceValue::Id("old-space".into()),
            Some("space".into()),
            Vec::new(),
        );
        journal.attempt_id = Some("attempt-a".into());
        request.accept_failed = Some(0);
        assert!(validate_finalize_approval(&request, &journal).is_err());
        request.accept_failed = Some(3);
        assert!(validate_finalize_approval(&request, &journal).is_err());

        journal.last_verification_counts = Some(VerificationCounts {
            missing_memory_text: 0,
            stale_memory_text: 0,
            unverified_memory_text: 0,
            failed_memory_text: 1,
            missing_memory_media: 0,
            stale_memory_media: 0,
            unverified_memory_media: 0,
            failed_memory_media: 1,
            missing_thread: 0,
            stale_thread: 0,
            unverified_thread: 0,
            failed_thread: 1,
            missing_reflection_intent: 0,
            stale_reflection_intent: 0,
            unverified_reflection_intent: 0,
            failed_reflection_intent: 0,
            orphan: 0,
        });
        assert!(validate_finalize_approval(&request, &journal).is_ok());
        request.accept_failed = Some(2);
        assert!(validate_finalize_approval(&request, &journal).is_err());
    }

    #[test]
    fn terminal_recovery_retries_are_limited_to_journal_supported_idempotent_states() {
        let mut journal = make_journal(
            EmbeddingSettings::default(),
            EmbeddingSettings::default(),
            SpaceValue::Id("old-space".into()),
            Some("target-space".into()),
            vec![17],
        );
        journal.phase = JournalPhase::RebuildPending;
        journal.attempt_id = Some("attempt-a".into());

        let mut failed_finalize = inspect_with_stage(None);
        failed_finalize.state = crate::sidecar::embedding_migration::EmbeddingState::Failed;
        failed_finalize.space = SpaceValue::Id("target-space".into());
        failed_finalize.next_action = NextAction::ResolveFailed;
        let finalize = RecoverEmbeddingMigrationRequest {
            action: EmbeddingRecoveryAction::Finalize,
            attempt_id: "attempt-a".into(),
            accept_failed: Some(2),
            settings: None,
        };
        assert!(terminal_recovery_retry_authorized(&finalize, &journal, &failed_finalize).unwrap());

        journal.phase = JournalPhase::Restored;
        let mut restored_consistent = inspect_with_stage(None);
        restored_consistent.state = crate::sidecar::embedding_migration::EmbeddingState::Consistent;
        restored_consistent.space = SpaceValue::Id("old-space".into());
        restored_consistent.next_action = NextAction::None;
        let restore = RecoverEmbeddingMigrationRequest {
            action: EmbeddingRecoveryAction::Restore,
            attempt_id: "attempt-a".into(),
            accept_failed: None,
            settings: None,
        };
        assert!(
            terminal_recovery_retry_authorized(&restore, &journal, &restored_consistent).unwrap()
        );
        assert!(matches!(
            recovery_cli_request(&restore),
            EmbeddingRequest::Restore { attempt } if attempt == "attempt-a"
        ));

        let mut restored_incomplete = restored_consistent.clone();
        restored_incomplete.state = crate::sidecar::embedding_migration::EmbeddingState::Incomplete;
        restored_incomplete.next_action = NextAction::Reconcile;
        journal.phase = JournalPhase::ReconcilePending;
        assert!(
            terminal_recovery_retry_authorized(&restore, &journal, &restored_incomplete).unwrap()
        );

        restored_incomplete.space = SpaceValue::Id("another-space".into());
        assert!(
            terminal_recovery_retry_authorized(&restore, &journal, &restored_incomplete).is_err()
        );
    }

    #[test]
    fn resource_corrupt_commit_can_restore_only_with_a_verified_backup() {
        use crate::sidecar::embedding_migration::UnavailableReason;
        use crate::sidecar::memories_migration::{AttemptStage, BackupMode};

        let mut inspect = inspect_with_stage(Some(AttemptStage::Commit));
        inspect.state = crate::sidecar::embedding_migration::EmbeddingState::Unavailable;
        inspect.unavailable_reason = Some(UnavailableReason::ResourceCorrupt);
        inspect.next_action = NextAction::Restore;
        inspect.backup = Some("/verified/backup".into());
        inspect.attempt.as_mut().unwrap().backup_mode = BackupMode::Backup;
        let restore = RecoverEmbeddingMigrationRequest {
            action: EmbeddingRecoveryAction::Restore,
            attempt_id: "attempt-a".into(),
            accept_failed: None,
            settings: None,
        };

        assert!(validate_recovery_action(&restore, &inspect).is_ok());

        inspect.backup = Some("unknown".into());
        assert!(validate_recovery_action(&restore, &inspect).is_err());
        inspect.backup = Some("/verified/backup".into());
        inspect.attempt.as_mut().unwrap().backup_mode = BackupMode::None;
        assert!(validate_recovery_action(&restore, &inspect).is_err());
    }

    #[test]
    fn rebuild_pending_start_action_allows_finalize_and_backup_appropriate_cancel() {
        use crate::sidecar::memories_migration::BackupMode;

        let mut with_backup = inspect_with_stage(Some(
            crate::sidecar::memories_migration::AttemptStage::RebuildPending,
        ));
        with_backup.next_action = NextAction::StartRebuild;
        with_backup.backup = Some("/verified/backup".into());
        let finalize = RecoverEmbeddingMigrationRequest {
            action: EmbeddingRecoveryAction::Finalize,
            attempt_id: "attempt-a".into(),
            accept_failed: None,
            settings: None,
        };
        assert!(validate_recovery_action(&finalize, &with_backup).is_ok());

        let restore = RecoverEmbeddingMigrationRequest {
            action: EmbeddingRecoveryAction::Restore,
            ..finalize.clone()
        };
        assert!(validate_recovery_action(&restore, &with_backup).is_ok());
        let abandon = RecoverEmbeddingMigrationRequest {
            action: EmbeddingRecoveryAction::Abandon,
            ..finalize.clone()
        };
        assert!(validate_recovery_action(&abandon, &with_backup).is_err());

        let mut without_backup = with_backup;
        without_backup.backup = None;
        without_backup.attempt.as_mut().unwrap().backup_mode = BackupMode::None;
        let abandon = RecoverEmbeddingMigrationRequest {
            action: EmbeddingRecoveryAction::Abandon,
            ..finalize.clone()
        };
        assert!(validate_recovery_action(&abandon, &without_backup).is_ok());
        assert!(validate_recovery_action(&restore, &without_backup).is_err());
    }

    #[test]
    fn restore_is_allowed_during_backup_commit_without_finalize_approval_counts() {
        use crate::sidecar::memories_migration::{AttemptStage, BackupMode};

        let mut inspect = inspect_with_stage(Some(AttemptStage::Commit));
        inspect.next_action = NextAction::Finalize;
        inspect.failed_transient = None;
        inspect.failed_permanent = None;
        inspect.backup = Some("/verified/backup".into());
        let restore = RecoverEmbeddingMigrationRequest {
            action: EmbeddingRecoveryAction::Restore,
            attempt_id: "attempt-a".into(),
            accept_failed: None,
            settings: None,
        };

        assert!(validate_recovery_action(&restore, &inspect).is_ok());
        inspect.backup = Some("unknown".into());
        assert!(validate_recovery_action(&restore, &inspect).is_err());
        inspect.backup = Some("/verified/backup".into());
        inspect.attempt.as_mut().unwrap().backup_mode = BackupMode::None;
        inspect.backup = None;
        assert!(validate_recovery_action(&restore, &inspect).is_err());
    }

    #[test]
    fn journal_is_atomically_written_and_corruption_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(root.path());
        let journal = make_journal(
            EmbeddingSettings::default(),
            EmbeddingSettings {
                preset_id: Some("qwen3-embedding-0-6b".into()),
                ..Default::default()
            },
            SpaceValue::Unknown,
            Some("planned-target".into()),
            vec![13],
        );
        save_journal(&data, &journal).unwrap();
        let loaded = load_journal(&data).unwrap().unwrap();
        assert_eq!(loaded.old_settings, EmbeddingSettings::default());
        assert_eq!(
            loaded.new_settings.preset_id.as_deref(),
            Some("qwen3-embedding-0-6b")
        );
        assert_eq!(loaded.expected_space, SpaceValue::Unknown);
        assert_eq!(loaded.paused_periodic_scheduler_ids, [13]);

        fs::write(journal_path(&data), b"{").unwrap();
        assert!(load_journal(&data).is_err());
    }

    #[test]
    fn journal_does_not_claim_attempt_id_before_cli_returns_it() {
        let journal = make_journal(
            EmbeddingSettings::default(),
            EmbeddingSettings::default(),
            SpaceValue::None,
            Some("target".into()),
            Vec::new(),
        );
        assert_eq!(journal.attempt_id, None);
    }

    #[test]
    fn journal_retains_verified_attempt_backup_until_inspect_proves_no_backup() {
        use crate::sidecar::memories_migration::{AttemptStage, BackupMode};

        let mut journal = make_journal(
            EmbeddingSettings::default(),
            EmbeddingSettings::default(),
            SpaceValue::Id("old-space".into()),
            Some("target-space".into()),
            Vec::new(),
        );
        journal.attempt_id = Some("attempt-a".into());
        journal.backup = Some("/verified/backup".into());
        let mut inspect = inspect_with_stage(Some(AttemptStage::Replace));
        inspect.backup = None;

        update_journal_attempt_from_inspect(&mut journal, &inspect).unwrap();
        assert_eq!(
            journal.attempt.as_ref().unwrap().stage,
            AttemptStage::Replace
        );
        assert_eq!(journal.backup.as_deref(), Some("/verified/backup"));

        inspect.attempt.as_mut().unwrap().backup_mode = BackupMode::None;
        update_journal_attempt_from_inspect(&mut journal, &inspect).unwrap();
        assert_eq!(
            journal.attempt.as_ref().unwrap().backup_mode,
            BackupMode::None
        );
        assert_eq!(journal.backup, None);
    }

    #[tokio::test]
    async fn prepared_recovery_restores_exact_scheduler_snapshot_before_clearing_journal() {
        let root = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(root.path());
        let mut journal = make_journal(
            EmbeddingSettings::default(),
            EmbeddingSettings::default(),
            SpaceValue::Id("space".into()),
            Some("target".into()),
            vec![17, 29],
        );
        journal.phase = JournalPhase::Prepared;
        save_journal(&data, &journal).unwrap();
        let inspect = inspect_with_stage(None);
        let data_for_restore = data.clone();

        let recovered =
            settle_prepared_journal(&data, journal.clone(), &inspect, |ids| async move {
                assert_eq!(ids, [17, 29]);
                let persisted = load_journal(&data_for_restore).unwrap().unwrap();
                assert_eq!(persisted.paused_periodic_scheduler_ids, [17, 29]);
                Ok(())
            })
            .await
            .unwrap();

        assert!(recovered);
        assert!(load_journal(&data).unwrap().is_none());

        save_journal(&data, &journal).unwrap();
        let mut unresolved_inspect = inspect.clone();
        unresolved_inspect.state = crate::sidecar::embedding_migration::EmbeddingState::Unknown;
        unresolved_inspect.next_action = NextAction::ManualRecovery;
        let restore_called = std::sync::atomic::AtomicBool::new(false);
        let unresolved =
            settle_prepared_journal(&data, journal.clone(), &unresolved_inspect, |_| async {
                restore_called.store(true, Ordering::Release);
                Ok(())
            })
            .await
            .unwrap();
        assert!(!unresolved);
        assert!(!restore_called.load(Ordering::Acquire));
        assert!(load_journal(&data).unwrap().is_some());

        save_journal(&data, &journal).unwrap();
        let failed_restore = settle_prepared_journal(&data, journal, &inspect, |_| async {
            Err(AppError::Config("scheduler restore failed".into()))
        })
        .await;
        assert!(failed_restore.is_err());
        assert!(load_journal(&data).unwrap().is_some());
    }

    #[tokio::test]
    async fn prepared_incomplete_or_unverified_same_space_can_resume_switch_retry() {
        let root = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(root.path());
        let mut journal = make_journal(
            EmbeddingSettings::default(),
            EmbeddingSettings::default(),
            SpaceValue::Id("original-space".into()),
            Some("target-space".into()),
            vec![17],
        );
        journal.phase = JournalPhase::Prepared;
        save_journal(&data, &journal).unwrap();

        for state in [
            crate::sidecar::embedding_migration::EmbeddingState::Incomplete,
            crate::sidecar::embedding_migration::EmbeddingState::Unverified,
        ] {
            let mut inspect = inspect_with_stage(None);
            inspect.state = state;
            inspect.space = SpaceValue::Id("original-space".into());
            inspect.next_action = NextAction::Reconcile;
            let (restored, _, _) = recover_scheduler_snapshot_on_startup_with(
                &data,
                journal.clone(),
                || async { Ok(EmbeddingMigrationResult::Inspect(inspect.clone())) },
                || async { panic!("a prepared switch must not query rebuild markers") },
                |ids| async move {
                    assert_eq!(ids, [17]);
                    Ok(())
                },
                || async { Ok(()) },
            )
            .await
            .unwrap();

            assert!(
                restored,
                "{state:?} at the unchanged concrete space is safe to resume"
            );
            assert!(load_journal(&data).unwrap().is_none());

            // The switch preflight consumes fresh CLI results after startup
            // cleanup; a same-space reconcile plan must still allow retry.
            let inspect_result = EmbeddingMigrationResult::Inspect(inspect);
            let plan_result = EmbeddingMigrationResult::Plan(EmbeddingPlan {
                decision: PlanDecision::ReembedRequired,
                executable: true,
                reason: None,
                current_space: SpaceValue::Id("original-space".into()),
                target_space: "target-space".into(),
                memory_text: 1,
                memory_media: 0,
                thread: 0,
                reflection_intent: 0,
                failed_transient: None,
                failed_permanent: Some(0),
            });
            let fresh = fresh_switch_inspect_result(
                inspect_result,
                &SpaceValue::Id("original-space".into()),
            )
            .unwrap();
            assert!(fresh_switch_plan_result(fresh, plan_result).is_ok());

            save_journal(&data, &journal).unwrap();
        }

        for (state, space) in [
            (
                crate::sidecar::embedding_migration::EmbeddingState::Unknown,
                SpaceValue::Id("original-space".into()),
            ),
            (
                crate::sidecar::embedding_migration::EmbeddingState::Incomplete,
                SpaceValue::Unknown,
            ),
            (
                crate::sidecar::embedding_migration::EmbeddingState::Unverified,
                SpaceValue::Id("different-space".into()),
            ),
        ] {
            let mut inspect = inspect_with_stage(None);
            inspect.state = state;
            inspect.space = space;
            inspect.next_action =
                if state == crate::sidecar::embedding_migration::EmbeddingState::Unknown {
                    NextAction::ManualRecovery
                } else {
                    NextAction::Reconcile
                };
            let restored = settle_prepared_journal(&data, journal.clone(), &inspect, |_| async {
                panic!("unsafe prepared state must not restore schedulers")
            })
            .await
            .unwrap();
            assert!(!restored);
            assert!(load_journal(&data).unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn prepared_switch_retry_reconciles_then_runs_a_fresh_plan_in_the_same_call() {
        let request = SetEmbeddingSettingsRequest {
            preset_id: Some("qwen3-embedding-0-6b".into()),
            custom_model_id: None,
            custom_tokenizer_id: None,
            custom_vector_size: None,
            custom_dtype: None,
            custom_max_sequence_length: None,
            custom_is_multimodal: None,
            model_revision: Some("candidate-r".into()),
        };
        let root = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(root.path());
        let mut journal = make_journal(
            EmbeddingSettings::default(),
            super::super::embedding_settings::settings_from_request(&request),
            SpaceValue::Id("original-space".into()),
            Some("planned-target".into()),
            vec![17, 29],
        );
        journal.phase = JournalPhase::Prepared;
        save_journal(&data, &journal).unwrap();

        let calls = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let calls_for_inspect = calls.clone();
        let calls_for_recovery = calls.clone();
        let calls_for_plan = calls.clone();
        let data_for_recovery = data.clone();
        let data_for_plan = data.clone();
        let mut inspect = inspect_with_stage(None);
        inspect.state = crate::sidecar::embedding_migration::EmbeddingState::Unverified;
        inspect.space = SpaceValue::Id("original-space".into());
        inspect.next_action = NextAction::Reconcile;
        let plan = EmbeddingPlan {
            decision: PlanDecision::ReembedRequired,
            executable: true,
            reason: None,
            current_space: SpaceValue::Id("original-space".into()),
            target_space: "planned-target".into(),
            memory_text: 1,
            memory_media: 0,
            thread: 0,
            reflection_intent: 0,
            failed_transient: None,
            failed_permanent: Some(0),
        };

        let (prepared, fresh_plan, fresh_inspect) = prepare_prepared_switch_retry_with(
            &data,
            journal,
            &request,
            SpaceValue::Id("original-space".into()),
            || async move {
                calls_for_inspect.lock().push("inspect");
                Ok(EmbeddingMigrationResult::Inspect(inspect))
            },
            |_, inspected, ids| async move {
                calls_for_recovery.lock().push("reconcile");
                assert_eq!(
                    inspected.state,
                    crate::sidecar::embedding_migration::EmbeddingState::Unverified
                );
                assert_eq!(ids, [17, 29]);
                assert_eq!(
                    load_journal(&data_for_recovery)
                        .unwrap()
                        .unwrap()
                        .paused_periodic_scheduler_ids,
                    [17, 29]
                );
                calls_for_recovery.lock().push("resume");
                calls_for_recovery.lock().push("post-inspect");
                Ok(inspected)
            },
            || async move {
                assert!(load_journal(&data_for_plan).unwrap().is_none());
                calls_for_plan.lock().push("plan");
                Ok(EmbeddingMigrationResult::Plan(plan))
            },
        )
        .await
        .unwrap();

        assert_eq!(
            *calls.lock(),
            ["inspect", "reconcile", "resume", "post-inspect", "plan"]
        );
        assert_eq!(
            fresh_inspect.state,
            crate::sidecar::embedding_migration::EmbeddingState::Unverified
        );
        assert_eq!(fresh_plan.target_space, "planned-target");
        assert!(prepared.paused_periodic_scheduler_ids.is_empty());
        assert_eq!(prepared.phase, JournalPhase::Prepared);
        assert!(load_journal(&data).unwrap().is_none());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn operation_in_progress_retry_reuses_the_production_preflight_and_calls_plan_then_switch_cli()
     {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(temp.path().join("data"));
        data.ensure().unwrap();
        let call_log = temp.path().join("cli-calls");
        let first_switch = temp.path().join("first-switch");
        let tool = temp.path().join("memories-db-migrate");
        std::fs::write(
            &tool,
            format!(
                r#"#!/bin/sh
case "$2" in
  inspect)
    printf '%s\n' inspect >> '{}'
    printf '%s\n' 'embedding_inspect status=completed state=unverified space=original-space required=1 complete=0 missing=1 stale=0 unverified=0 failed_transient=0 failed_permanent=0 orphan=0 backup_supported=false next_action=reconcile'
    ;;
  plan)
    printf '%s\n' plan >> '{}'
    printf '%s\n' 'embedding_plan status=completed decision=reembed_required executable=true current_space=original-space target_space=planned-target memory_text=1 memory_media=0 thread=0 reflection_intent=0 failed_transient=0 failed_permanent=0'
    ;;
  switch)
    printf '%s\n' switch >> '{}'
    if [ ! -e '{}' ]; then
      touch '{}'
      printf '%s\n' 'embedding_switch status=failed stage=preflight error_code=operation_in_progress resolution=retry'
      exit 1
    else
      printf '%s\n' 'embedding_switch status=completed outcome=switched attempt=attempt-retry source_space=original-space target_space=planned-target backup=none next_action=start_rebuild'
    fi
    ;;
  *) exit 9 ;;
esac
"#,
                call_log.display(),
                call_log.display(),
                call_log.display(),
                first_switch.display(),
                first_switch.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let lance_language_model_home = data.lance_language_model_home();
        let sidecars = std::sync::Arc::new(crate::sidecar::Sidecars::new(
            crate::sidecar::lifecycle::SidecarConfig {
                jobworkerp_bin: PathBuf::from("/bin/true"),
                memories_bin: PathBuf::from("/bin/true"),
                memories_db_migrate_bin: tool,
                startup_database_migration_enabled: false,
                conductor_bin: PathBuf::from("/bin/true"),
                data: data.clone(),
                worker_yaml_paths: Vec::new(),
                issue_tracker_server_bin: None,
                function_set_yaml_paths: Vec::new(),
                reflection_dispatch_enabled: false,
                auto_embedding_enabled: true,
                workflows_dir: Some(
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../workers/workflows"),
                ),
                lance_language_model_home,
                lindera_dict_staged: false,
                llm_model: None,
                llm_hf_repo: None,
                llm_ctx_size: None,
                llm_kv_cache_type: None,
                env_file: None,
            },
        ));
        let request = SetEmbeddingSettingsRequest {
            preset_id: Some("qwen3-embedding-0-6b".into()),
            custom_model_id: None,
            custom_tokenizer_id: None,
            custom_vector_size: None,
            custom_dtype: None,
            custom_max_sequence_length: None,
            custom_is_multimodal: None,
            model_revision: Some("candidate-r".into()),
        };
        let runtime =
            prepare_embedding_candidate_runtime(&request, |key| sidecars.embedding_env_lookup(key))
                .unwrap();
        let target = target_for_runtime(&runtime);
        let expected_space = SpaceValue::Id("original-space".into());
        let backup = EmbeddingBackup::NoBackupUnsafe;
        let mut journal = make_journal(
            EmbeddingSettings::default(),
            super::super::embedding_settings::settings_from_request(&request),
            expected_space.clone(),
            Some("planned-target".into()),
            vec![17, 29],
        );
        journal.phase = JournalPhase::Prepared;
        save_journal(&data, &journal).unwrap();

        let initial_switch = sidecars
            .run_embedding_cli_with_runtime(
                EmbeddingRequest::Switch {
                    target: target.clone(),
                    expected_space: expected_space.clone(),
                    backup: backup.clone(),
                    maintenance_window_ack: true,
                },
                &runtime,
            )
            .await
            .unwrap();
        let EmbeddingMigrationResult::Failed(failure) = initial_switch else {
            panic!("the first switch must fail with operation_in_progress");
        };
        assert_eq!(
            failure.error_code,
            crate::sidecar::memories_migration::CliErrorCode::OperationInProgress
        );
        assert!(load_journal(&data).unwrap().is_some());

        let recovered_events = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let events_for_recovery = recovered_events.clone();
        let sidecars_for_inspect = sidecars.clone();
        let sidecars_for_plan = sidecars.clone();
        let runtime_for_plan = runtime.clone();
        let plan_target = target.clone();
        let (_, plan, inspect) = prepare_prepared_switch_retry_with(
            &data,
            load_journal(&data).unwrap().unwrap(),
            &request,
            expected_space.clone(),
            || async move {
                sidecars_for_inspect
                    .run_embedding_cli(EmbeddingRequest::Inspect)
                    .await
            },
            move |_, inspect, ids| async move {
                assert_eq!(
                    inspect.state,
                    crate::sidecar::embedding_migration::EmbeddingState::Unverified
                );
                assert_eq!(inspect.next_action, NextAction::Reconcile);
                assert_eq!(ids, [17, 29]);
                events_for_recovery.lock().push("reconcile-and-restore");
                Ok(inspect)
            },
            || async move {
                sidecars_for_plan
                    .run_embedding_cli_with_runtime(
                        EmbeddingRequest::Plan {
                            target: plan_target,
                        },
                        &runtime_for_plan,
                    )
                    .await
            },
        )
        .await
        .unwrap();
        assert_eq!(
            inspect.state,
            crate::sidecar::embedding_migration::EmbeddingState::Unverified
        );
        assert_eq!(plan.target_space, "planned-target");
        assert!(load_journal(&data).unwrap().is_none());

        let retry_switch = sidecars
            .run_embedding_cli_with_runtime(
                EmbeddingRequest::Switch {
                    target,
                    expected_space,
                    backup,
                    maintenance_window_ack: true,
                },
                &runtime,
            )
            .await
            .unwrap();
        assert!(matches!(
            retry_switch,
            EmbeddingMigrationResult::Switched(_)
        ));
        assert_eq!(*recovered_events.lock(), ["reconcile-and-restore"]);
        assert_eq!(
            std::fs::read_to_string(call_log)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["switch", "inspect", "plan", "switch"]
        );
    }

    #[tokio::test]
    async fn prepared_switch_retry_rejects_request_drift_attempts_and_plan_target_drift() {
        let request = SetEmbeddingSettingsRequest {
            preset_id: Some("qwen3-embedding-0-6b".into()),
            custom_model_id: None,
            custom_tokenizer_id: None,
            custom_vector_size: None,
            custom_dtype: None,
            custom_max_sequence_length: None,
            custom_is_multimodal: None,
            model_revision: Some("candidate-r".into()),
        };
        let root = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(root.path());
        let make_prepared = || {
            let mut journal = make_journal(
                EmbeddingSettings::default(),
                super::super::embedding_settings::settings_from_request(&request),
                SpaceValue::Id("original-space".into()),
                Some("planned-target".into()),
                vec![17],
            );
            journal.phase = JournalPhase::Prepared;
            journal
        };

        let changed_request = SetEmbeddingSettingsRequest {
            model_revision: Some("different-r".into()),
            ..request.clone()
        };
        let mismatch = prepare_prepared_switch_retry_with(
            &data,
            make_prepared(),
            &changed_request,
            SpaceValue::Id("original-space".into()),
            || async { panic!("request drift must fail before inspect") },
            |_, _, _| async { panic!("request drift must not reconcile or resume schedulers") },
            || async { panic!("request drift must not plan") },
        )
        .await;
        assert!(mismatch.is_err());

        let expected_drift = prepare_prepared_switch_retry_with(
            &data,
            make_prepared(),
            &request,
            SpaceValue::Id("different-space".into()),
            || async { panic!("expected-space drift must fail before inspect") },
            |_, _, _| async { panic!("expected-space drift must not reconcile or resume") },
            || async { panic!("expected-space drift must not plan") },
        )
        .await;
        assert!(expected_drift.is_err());

        let mut active_inspect = inspect_with_stage(Some(
            crate::sidecar::memories_migration::AttemptStage::Replace,
        ));
        active_inspect.state = crate::sidecar::embedding_migration::EmbeddingState::Migrating;
        active_inspect.space = SpaceValue::Id("original-space".into());
        active_inspect.next_action = NextAction::ResumeOrAbandon;
        let resume_called = std::sync::atomic::AtomicBool::new(false);
        let plan_called = std::sync::atomic::AtomicBool::new(false);
        let mut active_journal = make_prepared();
        save_journal(&data, &active_journal).unwrap();
        let active_error = prepare_prepared_switch_retry_with(
            &data,
            active_journal.clone(),
            &request,
            SpaceValue::Id("original-space".into()),
            || async { Ok(EmbeddingMigrationResult::Inspect(active_inspect)) },
            |_, _, _| async {
                resume_called.store(true, Ordering::Release);
                panic!("active attempts must not reconcile or resume schedulers")
            },
            || async {
                plan_called.store(true, Ordering::Release);
                panic!("active attempts must not be planned again")
            },
        )
        .await;
        assert!(active_error.is_err());
        assert!(!resume_called.load(Ordering::Acquire));
        assert!(!plan_called.load(Ordering::Acquire));
        active_journal = load_journal(&data).unwrap().unwrap();
        assert_eq!(active_journal.attempt_id.as_deref(), Some("attempt-a"));

        let mut same_space_inspect = inspect_with_stage(None);
        same_space_inspect.space = SpaceValue::Id("original-space".into());
        let prepared = make_prepared();
        save_journal(&data, &prepared).unwrap();
        let target_drift = prepare_prepared_switch_retry_with(
            &data,
            prepared,
            &request,
            SpaceValue::Id("original-space".into()),
            || async { Ok(EmbeddingMigrationResult::Inspect(same_space_inspect)) },
            |_, inspect, ids| async move {
                assert_eq!(ids, [17]);
                Ok(inspect)
            },
            || async {
                Ok(EmbeddingMigrationResult::Plan(EmbeddingPlan {
                    decision: PlanDecision::ReembedRequired,
                    executable: true,
                    reason: None,
                    current_space: SpaceValue::Id("original-space".into()),
                    target_space: "different-target".into(),
                    memory_text: 1,
                    memory_media: 0,
                    thread: 0,
                    reflection_intent: 0,
                    failed_transient: None,
                    failed_permanent: Some(0),
                }))
            },
        )
        .await;
        assert!(target_drift.is_err());
        assert!(load_journal(&data).unwrap().is_none());
    }

    #[tokio::test]
    async fn startup_rebuild_pending_recovery_restores_only_journaled_schedulers_and_consumes_ledger()
     {
        let root = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(root.path());
        let mut journal = make_journal(
            EmbeddingSettings::default(),
            EmbeddingSettings::default(),
            SpaceValue::Id("old-space".into()),
            Some("target-space".into()),
            vec![17, 29],
        );
        journal.phase = JournalPhase::RebuildPending;
        journal.attempt_id = Some("attempt-a".into());
        save_journal(&data, &journal).unwrap();
        let mut inspect = inspect_with_stage(Some(
            crate::sidecar::memories_migration::AttemptStage::RebuildPending,
        ));
        inspect.state = crate::sidecar::embedding_migration::EmbeddingState::Migrating;
        inspect.space = SpaceValue::Id("target-space".into());
        inspect.next_action = NextAction::StartRebuild;
        inspect.attempt.as_mut().unwrap().attempt = "attempt-a".into();
        let marker_snapshot = space_response(2, Some("pending"));
        let persisted_during_restore = data.clone();
        let mut enabled_ids = Vec::new();

        let mut admission_open = false;
        let (recovered, _, recovered_inspect) = recover_scheduler_snapshot_on_startup_with(
            &data,
            journal,
            || async { Ok(EmbeddingMigrationResult::Inspect(inspect.clone())) },
            || async { Ok(marker_snapshot.clone()) },
            |ids| async {
                assert_eq!(ids, [17, 29]);
                enabled_ids.extend(ids);
                let persisted = load_journal(&persisted_during_restore).unwrap().unwrap();
                assert_eq!(persisted.paused_periodic_scheduler_ids, [17, 29]);
                Ok(())
            },
            || async {
                admission_open = true;
                Ok(())
            },
        )
        .await
        .unwrap();

        assert!(recovered);
        assert!(
            admission_open,
            "writers reopen only after durable scheduler restoration"
        );
        assert_eq!(
            recovered_inspect.attempt.as_ref().unwrap().attempt,
            "attempt-a"
        );
        assert_eq!(enabled_ids, [17, 29]);
        assert!(
            !enabled_ids.contains(&31),
            "a scheduler outside the durable enabled snapshot stays disabled"
        );
        let retained = load_journal(&data).unwrap().unwrap();
        assert_eq!(retained.phase, JournalPhase::RebuildPending);
        assert!(retained.paused_periodic_scheduler_ids.is_empty());
        assert_eq!(retained.attempt_id.as_deref(), Some("attempt-a"));

        let mut unsafe_inspect = inspect.clone();
        unsafe_inspect.attempt.as_mut().unwrap().attempt = "another-attempt".into();
        let retained_with_ledger = EmbeddingMigrationJournal {
            paused_periodic_scheduler_ids: vec![17],
            ..retained
        };
        save_journal(&data, &retained_with_ledger).unwrap();
        let restore_called = std::sync::atomic::AtomicBool::new(false);
        assert!(
            !settle_rebuild_pending_journal_after_startup(
                &data,
                retained_with_ledger.clone(),
                &unsafe_inspect,
                &marker_snapshot,
                |_| async {
                    restore_called.store(true, Ordering::Release);
                    Ok(())
                },
            )
            .await
            .unwrap()
        );
        assert!(!restore_called.load(Ordering::Acquire));
        assert_eq!(
            load_journal(&data)
                .unwrap()
                .unwrap()
                .paused_periodic_scheduler_ids,
            [17]
        );

        let mut mixed_markers = marker_snapshot;
        mixed_markers.tables[0]
            .record
            .as_mut()
            .unwrap()
            .migration_marker
            .as_mut()
            .unwrap()
            .state = "switching".into();
        assert!(!startup_rebuild_pending_is_authorized(
            &retained_with_ledger,
            &inspect,
            &mixed_markers,
        ));
    }

    #[tokio::test]
    async fn scheduler_pause_ledger_survives_restore_failure_and_is_consumed_after_retry() {
        let root = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(root.path());
        let mut journal = make_journal(
            EmbeddingSettings::default(),
            EmbeddingSettings::default(),
            SpaceValue::Id("old-space".into()),
            Some("target-space".into()),
            vec![13],
        );
        save_journal(&data, &journal).unwrap();

        let mut enabled_ids = std::collections::BTreeSet::new();
        let failed = restore_scheduler_snapshot_and_consume(&data, &mut journal, |ids| async {
            assert_eq!(ids, [13]);
            enabled_ids.extend(ids);
            Err(AppError::Config(
                "simulated crash after restore before ledger consume".into(),
            ))
        })
        .await;
        assert!(failed.is_err());
        assert_eq!(enabled_ids.iter().copied().collect::<Vec<_>>(), [13]);
        assert_eq!(
            load_journal(&data)
                .unwrap()
                .unwrap()
                .paused_periodic_scheduler_ids,
            [13]
        );

        restore_scheduler_snapshot_and_consume(&data, &mut journal, |ids| async {
            assert_eq!(ids, [13]);
            enabled_ids.extend(ids);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(
            enabled_ids.iter().copied().collect::<Vec<_>>(),
            [13],
            "crash recovery must be idempotent"
        );
        assert!(journal.paused_periodic_scheduler_ids.is_empty());
        assert!(
            load_journal(&data)
                .unwrap()
                .unwrap()
                .paused_periodic_scheduler_ids
                .is_empty()
        );

        // A user disables the schedule after normal resume. The next
        // maintenance snapshot must be based only on current enabled state,
        // not the consumed pre-rebuild snapshot.
        enabled_ids.clear();
        merge_paused_scheduler_ids(&mut journal, &[]);
        assert!(journal.paused_periodic_scheduler_ids.is_empty());

        let mut originally_disabled = make_journal(
            EmbeddingSettings::default(),
            EmbeddingSettings::default(),
            SpaceValue::Id("old-space".into()),
            Some("target-space".into()),
            Vec::new(),
        );
        let toggled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let toggled_for_restore = toggled.clone();
        restore_scheduler_snapshot_and_consume(&data, &mut originally_disabled, |ids| async move {
            assert!(ids.is_empty());
            toggled_for_restore.store(!ids.is_empty(), Ordering::Release);
            Ok(())
        })
        .await
        .unwrap();
        assert!(
            !toggled.load(Ordering::Acquire),
            "an initially disabled schedule must not be enabled"
        );
    }

    #[test]
    fn terminal_recovery_retry_requires_matching_journal_and_fresh_state() {
        let request = RecoverEmbeddingMigrationRequest {
            action: EmbeddingRecoveryAction::Finalize,
            attempt_id: "attempt-a".into(),
            accept_failed: None,
            settings: None,
        };
        let mut journal = make_journal(
            EmbeddingSettings::default(),
            EmbeddingSettings::default(),
            SpaceValue::Id("old-space".into()),
            Some("target-space".into()),
            vec![13],
        );
        journal.phase = JournalPhase::RebuildPending;
        journal.attempt_id = Some("attempt-a".into());
        let inspect = EmbeddingInspect {
            state: crate::sidecar::embedding_migration::EmbeddingState::Failed,
            space: SpaceValue::Id("target-space".into()),
            required: Some(1),
            complete: Some(0),
            missing: Some(1),
            stale: Some(0),
            unverified: Some(0),
            failed_transient: Some(0),
            failed_permanent: Some(0),
            orphan: Some(0),
            backup_supported: Some(true),
            attempt: None,
            backup: None,
            unavailable_reason: None,
            next_action: NextAction::Apply,
        };

        assert!(terminal_recovery_retry_authorized(&request, &journal, &inspect).unwrap());
        let mut stale_request = request.clone();
        stale_request.attempt_id = "other".into();
        assert!(terminal_recovery_retry_authorized(&stale_request, &journal, &inspect).is_err());

        let mut stale_space = inspect;
        stale_space.space = SpaceValue::Id("old-space".into());
        assert!(terminal_recovery_retry_authorized(&request, &journal, &stale_space).is_err());

        let mut ordinary_failed = stale_space;
        ordinary_failed.space = SpaceValue::Id("target-space".into());
        ordinary_failed.state = crate::sidecar::embedding_migration::EmbeddingState::Failed;
        ordinary_failed.next_action = NextAction::ResolveFailed;
        assert!(terminal_recovery_retry_authorized(&request, &journal, &ordinary_failed).is_ok());
    }

    #[test]
    fn pending_rebuild_allows_execution_tuning_but_rejects_identity_or_chunk_changes() {
        let mut old = super::super::embedding_settings::resolve_embedding_runtime(
            &EmbeddingSettings::default(),
        );
        let mut new = old.clone();
        new.dtype = if old.dtype == "F16" { "F32" } else { "F16" }.into();
        assert!(validate_pending_rebuild_settings_change(&old, &new).is_ok());

        new.max_sequence_length += 1;
        assert!(validate_pending_rebuild_settings_change(&old, &new).is_err());
        new.max_sequence_length = old.max_sequence_length;
        new.model_revision = "different-revision".into();
        assert!(validate_pending_rebuild_settings_change(&old, &new).is_err());

        old.dtype = new.dtype.clone();
        assert!(validate_pending_rebuild_settings_change(&old, &new).is_err());
    }

    fn inspect_with_stage(
        stage: Option<crate::sidecar::memories_migration::AttemptStage>,
    ) -> EmbeddingInspect {
        EmbeddingInspect {
            state: crate::sidecar::embedding_migration::EmbeddingState::Consistent,
            space: SpaceValue::Id("space".into()),
            required: Some(0),
            complete: Some(0),
            missing: Some(0),
            stale: Some(0),
            unverified: Some(0),
            failed_transient: Some(0),
            failed_permanent: Some(0),
            orphan: Some(0),
            backup_supported: Some(true),
            attempt: stage.map(
                |stage| crate::sidecar::memories_migration::EmbeddingAttemptInfo {
                    attempt: "attempt-a".into(),
                    stage,
                    backup_mode: crate::sidecar::memories_migration::BackupMode::Backup,
                },
            ),
            backup: None,
            unavailable_reason: None,
            next_action: NextAction::None,
        }
    }

    fn space_response(
        registration_state: i32,
        marker_state: Option<&str>,
    ) -> crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceResponse {
        use crate::grpc::proto::llm_memory::{data, service};

        service::GetEmbeddingSpaceResponse {
            registration_state,
            current_space: Some(data::EmbeddingSpace {
                space_id: "target-space".into(),
                ..Default::default()
            }),
            tables: marker_state
                .map(|state| {
                    ["memory", "thread"]
                        .into_iter()
                        .map(|table| service::EmbeddingTableRecord {
                            table: table.into(),
                            record: Some(data::EmbeddingSpaceRecord {
                                migration_marker: Some(data::EmbeddingMigrationMarker {
                                    state: state.into(),
                                    attempt_id: "attempt-a".into(),
                                }),
                                ..Default::default()
                            }),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            ..Default::default()
        }
    }

    #[test]
    fn chunk_guard_uses_rebuild_stage_or_pending_marker_not_worker_registration() {
        use crate::sidecar::memories_migration::AttemptStage;

        let inspect = inspect_with_stage(None);
        let registered_pending_marker = space_response(1, Some("pending"));
        assert!(rebuild_is_pending(&inspect, Some(&registered_pending_marker)).unwrap());

        let old = super::super::embedding_settings::resolve_embedding_runtime(
            &EmbeddingSettings::default(),
        );
        let mut chunk_change = old.clone();
        chunk_change.max_sequence_length += 1;
        assert!(validate_pending_rebuild_settings_change(&old, &chunk_change).is_err());
        let mut dtype_change = old.clone();
        dtype_change.dtype = if old.dtype == "F16" { "F32" } else { "F16" }.into();
        assert!(validate_pending_rebuild_settings_change(&old, &dtype_change).is_ok());

        let registration_pending_without_marker = space_response(2, None);
        assert!(!rebuild_is_pending(&inspect, Some(&registration_pending_without_marker)).unwrap());
        assert!(
            super::super::embedding_settings::validate_pending_chunk_configuration_change(
                &old,
                &chunk_change,
                false,
            )
            .is_ok()
        );

        let inspect_pending_stage = inspect_with_stage(Some(AttemptStage::RebuildPending));
        let no_marker = space_response(2, None);
        assert!(rebuild_is_pending(&inspect_pending_stage, Some(&no_marker)).unwrap());

        let unknown_marker = space_response(1, Some("unexpected"));
        assert!(rebuild_is_pending(&inspect, Some(&unknown_marker)).is_err());

        let mut unknown_inspect = inspect;
        unknown_inspect.state = crate::sidecar::embedding_migration::EmbeddingState::Unknown;
        assert!(rebuild_is_pending(&unknown_inspect, Some(&space_response(2, None))).is_err());
    }
}
