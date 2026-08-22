//! Lookback-owned coordination for explicit LanceDB index maintenance.
//!
//! The memories service owns LanceDB DDL.  This module owns only the durable
//! admission and scheduling information needed to request that service
//! safely from a desktop client.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveTime, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::data::DataPaths;
use crate::error::{AppError, AppResult};
use crate::grpc::proto::llm_memory::service as svc;
use crate::grpc::proto::llm_memory::service::search_index_maintenance_service_client::SearchIndexMaintenanceServiceClient;

const COMPACTION_THRESHOLD_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintenanceTable {
    Memory,
    Thread,
}

impl MaintenanceTable {
    fn target(self) -> i32 {
        match self {
            Self::Memory => svc::SearchIndexMaintenanceTarget::MemoryTable as i32,
            Self::Thread => svc::SearchIndexMaintenanceTarget::ThreadTable as i32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct TableMaintenanceState {
    pub write_generation: u64,
    pub dirty: bool,
    pub ready_runtime_secs: u64,
    pub last_result: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct PersistentState {
    #[serde(default)]
    pub memory: TableMaintenanceState,
    #[serde(default)]
    pub thread: TableMaintenanceState,
    #[serde(default)]
    pub reconcile_pending: bool,
    #[serde(default)]
    pub reconcile_generation: u64,
    /// A malformed state must block optimization rather than silently
    /// forgetting the generations that protect it.
    #[serde(default)]
    pub recovery_required: Option<String>,
    #[serde(default)]
    pub schedule: MaintenanceSchedule,
    #[serde(default)]
    pub attempts: Vec<OptimizeAttempt>,
    #[serde(default)]
    pub last_window_id: Option<String>,
    #[serde(default)]
    pub observed_periodic_execution_ids: Vec<String>,
}

/// Returns the local-start-date identity of an active `[start,end)` window.
/// `None` means outside the window or a nonexistent local wall-clock time.
fn maintenance_window_id_at(
    now: DateTime<Utc>,
    timezone: Tz,
    schedule: &MaintenanceSchedule,
) -> Option<String> {
    if !schedule.enabled {
        return None;
    }
    let (Some(start), Some(end)) = (schedule.start.as_deref(), schedule.end.as_deref()) else {
        return None;
    };
    let start = NaiveTime::parse_from_str(start, "%H:%M").ok()?;
    let end = NaiveTime::parse_from_str(end, "%H:%M").ok()?;
    if start == end {
        return None;
    }
    let local = now.with_timezone(&timezone);
    let time = NaiveTime::from_hms_opt(local.hour(), local.minute(), 0)?;
    let overnight = end < start;
    let active = if overnight {
        time >= start || time < end
    } else {
        time >= start && time < end
    };
    if !active {
        return None;
    }
    let date = if overnight && time < end {
        local.date_naive().pred_opt()?
    } else {
        local.date_naive()
    };
    Some(format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month(),
        date.day()
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OptimizeAttempt {
    pub table: MaintenanceTable,
    pub task_id: Option<String>,
    pub generation_at_start: u64,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct AcceptedOptimizeTask {
    pub table: MaintenanceTable,
    pub task_id: String,
    pub generation_at_start: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct MaintenanceSchedule {
    pub enabled: bool,
    pub start: Option<String>,
    pub end: Option<String>,
}

impl PersistentState {
    fn table_mut(&mut self, table: MaintenanceTable) -> &mut TableMaintenanceState {
        match table {
            MaintenanceTable::Memory => &mut self.memory,
            MaintenanceTable::Thread => &mut self.thread,
        }
    }

    pub fn mark_write(&mut self, tables: &[MaintenanceTable]) {
        for table in tables {
            let entry = self.table_mut(*table);
            entry.write_generation = entry.write_generation.saturating_add(1);
            entry.dirty = true;
        }
    }

    /// Returns whether both successful sub-actions can safely reset runtime.
    pub fn complete_optimize(
        &mut self,
        table: MaintenanceTable,
        generation_at_start: u64,
        sub_actions_succeeded: bool,
    ) -> bool {
        let entry = self.table_mut(table);
        if sub_actions_succeeded && entry.write_generation == generation_at_start {
            entry.ready_runtime_secs = 0;
            entry.dirty = false;
            entry.last_result = Some("succeeded".into());
            self.attempts.retain(|attempt| attempt.table != table);
            true
        } else {
            entry.last_result = Some(if sub_actions_succeeded {
                "generation_changed".into()
            } else {
                "failed".into()
            });
            self.attempts.retain(|attempt| attempt.table != table);
            false
        }
    }
}

/// Save through a sibling temporary file so a crash cannot turn a valid state
/// into a partially-written success record.
pub fn save_state(path: &Path, state: &PersistentState) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("maintenance state has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|e| AppError::Config(format!("maintenance state serialization failed: {e}")))?;
    fs::write(&temp, bytes)?;
    fs::rename(temp, path)?;
    Ok(())
}

pub fn load_state(path: &Path) -> AppResult<PersistentState> {
    if !path.exists() {
        return Ok(PersistentState::default());
    }
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| AppError::Config(format!("maintenance state is invalid: {e}")))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct MaintenanceStatus {
    pub memory: TableMaintenanceState,
    pub thread: TableMaintenanceState,
    pub reconcile_pending: bool,
    pub writing: bool,
    pub optimizing: bool,
    pub active_optimizations: Vec<ActiveOptimization>,
    pub memory_eligible: bool,
    pub thread_eligible: bool,
    pub recovery_required: Option<String>,
    pub schedule: MaintenanceSchedule,
}

/// User-visible projection of a task that still belongs to this optimization
/// run. The generation guard remains internal so the UI cannot mistake it for
/// a server task identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ActiveOptimization {
    pub table: MaintenanceTable,
    pub task_id: Option<String>,
    pub state: String,
}

/// Shared, process-local admission state.  There is one coordinator per
/// Local data root; remote connections never call its mutating methods.
pub struct SearchIndexMaintenanceCoordinator {
    data: DataPaths,
    state: Mutex<PersistentState>,
    admission: Mutex<Admission>,
    reconcile_in_flight: Mutex<bool>,
    active_periodic_execution_ids: Mutex<HashSet<String>>,
}

#[derive(Default)]
struct Admission {
    active_writes: u32,
    optimizing: bool,
}

impl SearchIndexMaintenanceCoordinator {
    pub fn new(data: DataPaths) -> Self {
        let state = load_state(&data.search_index_maintenance_path()).unwrap_or_else(|error| {
            tracing::warn!(%error, "ignoring unreadable search-index maintenance state");
            PersistentState {
                recovery_required: Some(error.to_string()),
                ..PersistentState::default()
            }
        });
        Self {
            data,
            state: Mutex::new(state),
            admission: Mutex::new(Admission::default()),
            reconcile_in_flight: Mutex::new(false),
            active_periodic_execution_ids: Mutex::new(HashSet::new()),
        }
    }

    fn save_locked(&self, state: &PersistentState) -> AppResult<()> {
        save_state(&self.data.search_index_maintenance_path(), state)
    }

    /// Must run before registering/dispatching a runner that can write.
    pub async fn begin_write(&self, tables: &[MaintenanceTable]) -> AppResult<()> {
        let mut admission = self.admission.lock().await;
        if admission.optimizing {
            return Err(AppError::Config(
                "最適化の開始中は書込みを開始できません".into(),
            ));
        }
        let mut state = self.state.lock().await;
        if let Some(reason) = &state.recovery_required {
            return Err(AppError::Config(format!(
                "index メンテナンス状態の復旧が必要です: {reason}"
            )));
        }
        state.mark_write(tables);
        // Persist first: callers are forbidden to dispatch if this fails.
        self.save_locked(&state)?;
        admission.active_writes = admission.active_writes.saturating_add(1);
        Ok(())
    }

    /// Called exactly once after a logical write group reaches terminal.
    pub async fn finish_write(&self) -> AppResult<()> {
        let mut admission = self.admission.lock().await;
        admission.active_writes = admission.active_writes.saturating_sub(1);
        let mut state = self.state.lock().await;
        state.reconcile_pending = true;
        state.reconcile_generation = state.reconcile_generation.saturating_add(1);
        self.save_locked(&state)
    }

    pub async fn status(&self) -> MaintenanceStatus {
        let admission = self.admission.lock().await;
        let state = self.state.lock().await.clone();
        MaintenanceStatus {
            memory_eligible: state.memory.dirty
                && state.memory.ready_runtime_secs >= COMPACTION_THRESHOLD_SECS,
            thread_eligible: state.thread.dirty
                && state.thread.ready_runtime_secs >= COMPACTION_THRESHOLD_SECS,
            memory: state.memory,
            thread: state.thread,
            reconcile_pending: state.reconcile_pending,
            writing: admission.active_writes > 0,
            optimizing: admission.optimizing,
            active_optimizations: state
                .attempts
                .into_iter()
                .filter(|attempt| matches!(attempt.state.as_str(), "attempting" | "accepted"))
                .map(|attempt| ActiveOptimization {
                    table: attempt.table,
                    task_id: attempt.task_id,
                    state: attempt.state,
                })
                .collect(),
            recovery_required: state.recovery_required,
            schedule: state.schedule,
        }
    }

    /// Check the one non-mutating method during sidecar readiness.
    pub async fn verify_service(&self, channel: tonic::transport::Channel) -> AppResult<()> {
        let mut client = SearchIndexMaintenanceServiceClient::new(channel.clone());
        client
            .get_search_index_maintenance_status(svc::GetSearchIndexMaintenanceStatusRequest {
                task_id: None,
                target: None,
            })
            .await?;
        // There is no dedicated capability RPC. An unspecified target/action
        // must be rejected before task creation, which proves the mutating
        // method is present without accidentally scheduling maintenance.
        match client
            .start_search_index_maintenance(svc::StartSearchIndexMaintenanceRequest {
                target: svc::SearchIndexMaintenanceTarget::Unspecified as i32,
                action: svc::SearchIndexMaintenanceAction::Unspecified as i32,
                force: false,
                optimize_actions: vec![],
            })
            .await
        {
            Err(status) if status.code() == tonic::Code::InvalidArgument => Ok(()),
            Err(status) => Err(status.into()),
            Ok(_) => Err(AppError::Config(
                "maintenance service が不正な capability request を拒否しませんでした".into(),
            )),
        }?;
        Ok(())
    }

    /// Reconcile only after an accepted logical write terminal or readiness.
    pub async fn reconcile(&self, channel: tonic::transport::Channel) -> AppResult<()> {
        let mut in_flight = self.reconcile_in_flight.lock().await;
        if *in_flight {
            // The terminal edge already persisted a newer generation. The
            // active worker observes it after its RPC response and runs once
            // more instead of allowing concurrent reconcile RPCs.
            return Ok(());
        }
        *in_flight = true;
        let snapshot = self.state.lock().await.reconcile_generation;
        let mut client = SearchIndexMaintenanceServiceClient::new(channel.clone());
        let response = client
            .reconcile_search_indices(svc::ReconcileSearchIndicesRequest {})
            .await
            .map(|response| response.into_inner());
        *in_flight = false;
        let response = response?;
        let mut state = self.state.lock().await;
        // A task or retryable skip means a later pass is still necessary.
        let retry_needed = response.started_task_id.is_some()
            || response.skipped_targets.iter().any(|skipped| {
                matches!(
                    svc::SearchIndexMaintenanceSkipReason::try_from(skipped.reason).ok(),
                    Some(svc::SearchIndexMaintenanceSkipReason::RunningSkip)
                        | Some(svc::SearchIndexMaintenanceSkipReason::CheckRunning)
                        | Some(svc::SearchIndexMaintenanceSkipReason::ReconcileRunning)
                        | Some(svc::SearchIndexMaintenanceSkipReason::Backoff)
                        | Some(svc::SearchIndexMaintenanceSkipReason::ObservationUnavailable)
                )
            });
        // Never clear a terminal signal which arrived while this RPC was
        // running. A later caller (or the hourly scheduler) then performs
        // the required successor reconcile.
        let newer_terminal = state.reconcile_generation != snapshot;
        state.reconcile_pending = retry_needed || newer_terminal;
        self.save_locked(&state)?;
        drop(state);
        if newer_terminal {
            return Box::pin(self.reconcile(channel)).await;
        }
        Ok(())
    }

    /// Atomically admits the first manual table and persists its attempt.
    /// This is intentionally short-lived: callers hand terminal observation to
    /// a background worker only after this method returns an accepted task.
    pub async fn begin_manual_optimize(
        &self,
        channel: tonic::transport::Channel,
    ) -> AppResult<Option<AcceptedOptimizeTask>> {
        self.begin_optimize_table(channel, MaintenanceTable::Memory, false)
            .await
    }

    async fn begin_optimize_table(
        &self,
        channel: tonic::transport::Channel,
        table: MaintenanceTable,
        continues_existing_run: bool,
    ) -> AppResult<Option<AcceptedOptimizeTask>> {
        let mut admission = self.admission.lock().await;
        if admission.active_writes > 0 || (!continues_existing_run && admission.optimizing) {
            return Err(AppError::Config(
                "書込み処理または最適化の実行中は開始できません".into(),
            ));
        }
        if !continues_existing_run {
            admission.optimizing = true;
        }
        let generation_at_start = self.state.lock().await.table_mut(table).write_generation;
        {
            let mut state = self.state.lock().await;
            state.attempts.retain(|attempt| attempt.table != table);
            state.attempts.push(OptimizeAttempt {
                table,
                task_id: None,
                generation_at_start,
                state: "attempting".into(),
            });
            self.save_locked(&state)?;
        }
        let mut client = SearchIndexMaintenanceServiceClient::new(channel.clone());
        let response = client
            .start_search_index_maintenance(svc::StartSearchIndexMaintenanceRequest {
                target: table.target(),
                action: svc::SearchIndexMaintenanceAction::Optimize as i32,
                force: false,
                optimize_actions: vec![
                    svc::SearchIndexOptimizeAction::Compact as i32,
                    svc::SearchIndexOptimizeAction::Index as i32,
                ],
            })
            .await
            .map(|response| response.into_inner());
        let mut state = self.state.lock().await;
        let result = match response {
            Ok(response) => {
                let accepted =
                    svc::SearchIndexMaintenanceDisposition::try_from(response.disposition).ok()
                        == Some(svc::SearchIndexMaintenanceDisposition::Accepted);
                let attempt = state
                    .attempts
                    .iter_mut()
                    .find(|attempt| attempt.table == table);
                if accepted {
                    if let (Some(task_id), Some(attempt)) = (response.task_id, attempt) {
                        attempt.task_id = Some(task_id.clone());
                        attempt.state = "accepted".into();
                        state.table_mut(table).last_result = Some("accepted".into());
                        Some(AcceptedOptimizeTask {
                            table,
                            task_id,
                            generation_at_start,
                        })
                    } else {
                        state.table_mut(table).last_result = Some("unknown".into());
                        None
                    }
                } else {
                    if let Some(attempt) = attempt {
                        attempt.state = "already_running".into();
                    }
                    state.table_mut(table).last_result = Some("already_running".into());
                    None
                }
            }
            Err(error) => {
                if let Some(attempt) = state
                    .attempts
                    .iter_mut()
                    .find(|attempt| attempt.table == table)
                {
                    attempt.state = "unknown".into();
                }
                if !continues_existing_run {
                    admission.optimizing = false;
                }
                self.save_locked(&state)?;
                return Err(error.into());
            }
        };
        self.save_locked(&state)?;
        if result.is_none() && !continues_existing_run {
            admission.optimizing = false;
        }
        Ok(result)
    }

    pub async fn observe_accepted_optimize(
        self: Arc<Self>,
        channel: tonic::transport::Channel,
        accepted: AcceptedOptimizeTask,
    ) {
        let followups = if accepted.table == MaintenanceTable::Memory {
            vec![MaintenanceTable::Thread]
        } else {
            Vec::new()
        };
        self.clone()
            .observe_accepted_optimize_with_followups(channel, accepted, followups)
            .await;
        self.finish_optimization().await;
    }

    async fn observe_accepted_optimize_with_followups(
        self: Arc<Self>,
        channel: tonic::transport::Channel,
        accepted: AcceptedOptimizeTask,
        mut followups: Vec<MaintenanceTable>,
    ) {
        let mut client = SearchIndexMaintenanceServiceClient::new(channel.clone());
        if let Err(error) = self
            .wait_for_optimize_terminal(
                &mut client,
                accepted.table,
                &accepted.task_id,
                accepted.generation_at_start,
            )
            .await
        {
            tracing::warn!(%error, "manual optimize terminal observation failed");
            if let Err(mark_error) = self.mark_attempt_unknown(accepted.table).await {
                tracing::warn!(%mark_error, "failed to persist unknown optimize state");
            }
        }
        if let Some(next_table) = followups.first().copied() {
            followups.remove(0);
            match self
                .begin_optimize_table(channel.clone(), next_table, true)
                .await
            {
                Ok(Some(next)) => {
                    Box::pin(
                        self.observe_accepted_optimize_with_followups(channel, next, followups),
                    )
                    .await;
                }
                Ok(None) => {}
                Err(error) => {
                    let mut state = self.state.lock().await;
                    state.table_mut(next_table).last_result = Some("deferred_writing".into());
                    if let Err(save_error) = self.save_locked(&state) {
                        tracing::warn!(%save_error, "failed to persist deferred thread optimize");
                    }
                    tracing::info!(%error, "follow-up optimize deferred after prior task");
                }
            }
        }
    }

    /// Evaluates one local wall-clock window. A window is recorded only when
    /// an eligible table exists; starting the app mid-window is therefore
    /// supported without catch-up after the window ends.
    pub async fn start_scheduled_optimize(
        self: Arc<Self>,
        channel: tonic::transport::Channel,
        now: DateTime<Utc>,
        timezone: &str,
    ) -> AppResult<()> {
        let timezone: Tz = timezone
            .parse()
            .map_err(|_| AppError::Config("保守時間帯のタイムゾーンが不正です".into()))?;
        let mut state = self.state.lock().await;
        let Some(window_id) = maintenance_window_id_at(now, timezone, &state.schedule) else {
            return Ok(());
        };
        if state.last_window_id.as_deref() == Some(window_id.as_str()) {
            return Ok(());
        }
        let mut tables = Vec::new();
        if state.memory.dirty && state.memory.ready_runtime_secs >= COMPACTION_THRESHOLD_SECS {
            tables.push(MaintenanceTable::Memory);
        }
        if state.thread.dirty && state.thread.ready_runtime_secs >= COMPACTION_THRESHOLD_SECS {
            tables.push(MaintenanceTable::Thread);
        }
        if tables.is_empty() {
            return Ok(());
        }
        state.last_window_id = Some(window_id);
        self.save_locked(&state)?;
        drop(state);
        let first = tables.remove(0);
        if let Some(accepted) = self
            .begin_optimize_table(channel.clone(), first, false)
            .await?
        {
            let coordinator = self.clone();
            tauri::async_runtime::spawn(async move {
                coordinator
                    .clone()
                    .observe_accepted_optimize_with_followups(channel, accepted, tables)
                    .await;
                coordinator.finish_optimization().await;
            });
        }
        Ok(())
    }

    async fn finish_optimization(&self) {
        self.admission.lock().await.optimizing = false;
    }

    async fn mark_attempt_unknown(&self, table: MaintenanceTable) -> AppResult<()> {
        let mut state = self.state.lock().await;
        if let Some(attempt) = state
            .attempts
            .iter_mut()
            .find(|attempt| attempt.table == table)
        {
            attempt.state = "unknown".into();
        }
        state.table_mut(table).last_result = Some("unknown".into());
        self.save_locked(&state)
    }

    async fn wait_for_optimize_terminal(
        &self,
        client: &mut SearchIndexMaintenanceServiceClient<tonic::transport::Channel>,
        table: MaintenanceTable,
        task_id: &str,
        generation_at_start: u64,
    ) -> AppResult<()> {
        for _ in 0..60 {
            let response = client
                .get_search_index_maintenance_status(svc::GetSearchIndexMaintenanceStatusRequest {
                    task_id: Some(task_id.to_string()),
                    target: Some(table.target()),
                })
                .await?
                .into_inner();
            if let Some(task) = response
                .last_results
                .into_iter()
                .find(|task| task.task_id == task_id)
            {
                let success = svc::SearchIndexMaintenanceStatus::try_from(task.status).ok()
                    == Some(svc::SearchIndexMaintenanceStatus::Succeeded)
                    && task.sub_actions.iter().any(|action| {
                        svc::SearchIndexOptimizeAction::try_from(action.action).ok()
                            == Some(svc::SearchIndexOptimizeAction::Compact)
                            && svc::SearchIndexMaintenanceStatus::try_from(action.status).ok()
                                == Some(svc::SearchIndexMaintenanceStatus::Succeeded)
                    })
                    && task.sub_actions.iter().any(|action| {
                        svc::SearchIndexOptimizeAction::try_from(action.action).ok()
                            == Some(svc::SearchIndexOptimizeAction::Index)
                            && svc::SearchIndexMaintenanceStatus::try_from(action.status).ok()
                                == Some(svc::SearchIndexMaintenanceStatus::Succeeded)
                    });
                let mut state = self.state.lock().await;
                state.complete_optimize(table, generation_at_start, success);
                return self.save_locked(&state);
            }
            if response
                .running_tasks
                .iter()
                .any(|task| task.task_id == task_id)
            {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            // History can be truncated; never infer success from disappearance.
            return self.mark_attempt_unknown(table).await;
        }
        self.mark_attempt_unknown(table).await
    }

    pub async fn add_ready_runtime(&self, seconds: u64) -> AppResult<()> {
        let mut state = self.state.lock().await;
        if state.memory.dirty {
            state.memory.ready_runtime_secs =
                state.memory.ready_runtime_secs.saturating_add(seconds);
        }
        if state.thread.dirty {
            state.thread.ready_runtime_secs =
                state.thread.ready_runtime_secs.saturating_add(seconds);
        }
        self.save_locked(&state)
    }

    pub async fn set_schedule(&self, schedule: MaintenanceSchedule) -> AppResult<()> {
        let mut state = self.state.lock().await;
        if state.recovery_required.is_some() {
            return Err(AppError::Config(
                "index メンテナンス状態の復旧が必要です".into(),
            ));
        }
        state.schedule = schedule;
        self.save_locked(&state)
    }

    /// Periodic jobs are dispatched by conductor, outside Lookback's direct
    /// runner hook. Their terminal execution id is therefore the durable
    /// deduplication key. We conservatively dirty both tables because each
    /// supported periodic workflow may write summaries, personality, or
    /// reflections.
    pub async fn record_periodic_terminal(&self, execution_id: &str) -> AppResult<bool> {
        let mut state = self.state.lock().await;
        if state
            .observed_periodic_execution_ids
            .iter()
            .any(|known| known == execution_id)
        {
            return Ok(false);
        }
        state
            .observed_periodic_execution_ids
            .push(execution_id.to_string());
        if state.observed_periodic_execution_ids.len() > 256 {
            state.observed_periodic_execution_ids.remove(0);
        }
        state.mark_write(&[MaintenanceTable::Memory, MaintenanceTable::Thread]);
        state.reconcile_pending = true;
        state.reconcile_generation = state.reconcile_generation.saturating_add(1);
        self.save_locked(&state)?;
        Ok(true)
    }

    pub async fn begin_periodic_execution(&self, execution_id: &str) -> AppResult<bool> {
        let mut active = self.active_periodic_execution_ids.lock().await;
        if !active.insert(execution_id.to_string()) {
            return Ok(false);
        }
        drop(active);
        if let Err(error) = self
            .begin_write(&[MaintenanceTable::Memory, MaintenanceTable::Thread])
            .await
        {
            self.active_periodic_execution_ids
                .lock()
                .await
                .remove(execution_id);
            return Err(error);
        }
        Ok(true)
    }

    pub async fn finish_periodic_execution(&self, execution_id: &str) -> AppResult<bool> {
        if self
            .active_periodic_execution_ids
            .lock()
            .await
            .remove(execution_id)
        {
            self.finish_write().await?;
            return Ok(true);
        }
        self.record_periodic_terminal(execution_id).await
    }
}

pub type SharedSearchIndexMaintenanceCoordinator = Arc<SearchIndexMaintenanceCoordinator>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_dispatch_marks_only_requested_tables_dirty() {
        let mut state = PersistentState::default();
        state.mark_write(&[MaintenanceTable::Memory]);

        assert_eq!(state.memory.write_generation, 1);
        assert!(state.memory.dirty);
        assert_eq!(state.thread.write_generation, 0);
        assert!(!state.thread.dirty);
    }

    #[test]
    fn optimization_resets_elapsed_only_when_generation_is_unchanged() {
        let mut state = PersistentState::default();
        state.memory.dirty = true;
        state.memory.write_generation = 4;
        state.memory.ready_runtime_secs = 86_400;

        assert!(state.complete_optimize(MaintenanceTable::Memory, 4, true));
        assert_eq!(state.memory.ready_runtime_secs, 0);
        assert!(!state.memory.dirty);

        state.memory.dirty = true;
        state.memory.ready_runtime_secs = 86_400;
        state.memory.write_generation = 5;
        assert!(!state.complete_optimize(MaintenanceTable::Memory, 4, true));
        assert_eq!(state.memory.ready_runtime_secs, 86_400);
        assert!(state.memory.dirty);
    }

    #[test]
    fn atomic_round_trip_preserves_pending_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("maintenance.json");
        let state = PersistentState {
            reconcile_pending: true,
            reconcile_generation: 2,
            ..Default::default()
        };
        save_state(&path, &state).unwrap();

        assert_eq!(load_state(&path).unwrap(), state);
    }

    #[test]
    fn schedule_round_trip_is_persistent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("maintenance.json");
        let state = PersistentState {
            schedule: MaintenanceSchedule {
                enabled: true,
                start: Some("23:00".into()),
                end: Some("02:00".into()),
            },
            ..PersistentState::default()
        };
        save_state(&path, &state).unwrap();
        assert_eq!(load_state(&path).unwrap().schedule, state.schedule);
    }

    #[test]
    fn accepted_attempt_resets_only_after_both_actions_and_same_generation() {
        let mut state = PersistentState::default();
        state.memory.dirty = true;
        state.memory.write_generation = 8;
        state.memory.ready_runtime_secs = 86_400;
        state.attempts.push(OptimizeAttempt {
            table: MaintenanceTable::Memory,
            task_id: Some("task-1".into()),
            generation_at_start: 8,
            state: "accepted".into(),
        });

        assert!(state.complete_optimize(MaintenanceTable::Memory, 8, true));
        assert_eq!(state.memory.ready_runtime_secs, 0);
        assert!(state.attempts.is_empty());
    }

    #[test]
    fn window_identity_uses_start_date_and_half_open_boundaries() {
        let schedule = MaintenanceSchedule {
            enabled: true,
            start: Some("23:00".into()),
            end: Some("02:00".into()),
        };
        let tz: Tz = "Asia/Tokyo".parse().unwrap();
        let at = "2026-08-04T16:00:00Z".parse::<DateTime<Utc>>().unwrap(); // 01:00 JST
        assert_eq!(
            maintenance_window_id_at(at, tz, &schedule).as_deref(),
            Some("2026-08-04")
        );
        let end = "2026-08-04T17:00:00Z".parse::<DateTime<Utc>>().unwrap(); // 02:00 JST
        assert_eq!(maintenance_window_id_at(end, tz, &schedule), None);
    }

    #[test]
    fn dst_repeated_wall_clock_has_one_stable_window_identity() {
        let schedule = MaintenanceSchedule {
            enabled: true,
            start: Some("01:00".into()),
            end: Some("03:00".into()),
        };
        let tz: Tz = "America/New_York".parse().unwrap();
        let first = "2026-11-01T05:30:00Z".parse::<DateTime<Utc>>().unwrap();
        let second = "2026-11-01T06:30:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            maintenance_window_id_at(first, tz, &schedule),
            maintenance_window_id_at(second, tz, &schedule)
        );
    }

    #[tokio::test]
    async fn periodic_terminal_is_deduplicated_by_execution_id() {
        let coordinator = SearchIndexMaintenanceCoordinator::new(DataPaths::with_root(
            tempfile::tempdir().unwrap().path(),
        ));
        assert!(coordinator.record_periodic_terminal("42").await.unwrap());
        assert!(!coordinator.record_periodic_terminal("42").await.unwrap());
        assert_eq!(coordinator.status().await.memory.write_generation, 1);
    }

    #[tokio::test]
    async fn periodic_active_scope_blocks_manual_writes_until_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator = SearchIndexMaintenanceCoordinator::new(DataPaths::with_root(dir.path()));
        assert!(coordinator.begin_periodic_execution("run-1").await.unwrap());
        assert!(coordinator.status().await.writing);
        assert!(
            coordinator
                .finish_periodic_execution("run-1")
                .await
                .unwrap()
        );
        assert!(!coordinator.status().await.writing);
    }

    #[tokio::test]
    async fn status_keeps_accepted_optimization_visible_until_observation_finishes() {
        let coordinator = SearchIndexMaintenanceCoordinator::new(DataPaths::with_root(
            tempfile::tempdir().unwrap().path(),
        ));
        coordinator.admission.lock().await.optimizing = true;
        coordinator
            .state
            .lock()
            .await
            .attempts
            .push(OptimizeAttempt {
                table: MaintenanceTable::Memory,
                task_id: Some("maintenance-1".into()),
                generation_at_start: 3,
                state: "accepted".into(),
            });

        let status = coordinator.status().await;
        assert!(status.optimizing);
        assert_eq!(status.active_optimizations.len(), 1);
        assert_eq!(
            status.active_optimizations[0].table,
            MaintenanceTable::Memory
        );
        assert_eq!(
            status.active_optimizations[0].task_id.as_deref(),
            Some("maintenance-1")
        );
    }

    #[tokio::test]
    async fn status_does_not_present_an_unobserved_task_as_running() {
        let coordinator = SearchIndexMaintenanceCoordinator::new(DataPaths::with_root(
            tempfile::tempdir().unwrap().path(),
        ));
        coordinator
            .state
            .lock()
            .await
            .attempts
            .push(OptimizeAttempt {
                table: MaintenanceTable::Memory,
                task_id: Some("maintenance-1".into()),
                generation_at_start: 3,
                state: "unknown".into(),
            });

        let status = coordinator.status().await;
        assert!(!status.optimizing);
        assert!(status.active_optimizations.is_empty());
    }
}
