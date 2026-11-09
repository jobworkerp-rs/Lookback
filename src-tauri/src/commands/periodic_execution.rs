//! Periodic-execution status / history / cancel, backed by conductor's
//! `ExecutionStatusService`.
//!
//! Conductor resolves a job's runtime status at read time and returns a fully
//! resolved `ExecutionRuntimeStatus` (with `status_source` set) for every
//! existing `ExecutionRef`, including all fallbacks (job_id-less →
//! enqueue_failed/terminal, no stored JobResult → `result_status` snapshot,
//! jobworkerp unreachable → UNAVAILABLE). So this layer only converts the proto
//! enums into the UI's snake_case unions and derives `active` / `cancelable`; it
//! does NOT re-implement the fallback chain.
//!
//! Like the rest of `periodic_tasks`, these commands always talk to the LOCAL
//! sidecar conductor (`require_endpoints().conductor_url()`), independent of the
//! local/remote connection mode — periodic execution is local-only and the
//! `job_id` shown is a local jobworkerp identifier (display-only).

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep, timeout};
use tokio_stream::StreamExt as _;
use tonic::transport::Channel;
use tracing::warn;

use crate::error::{AppError, AppResult};
use crate::error_messages::{CANCEL_NOT_MANAGED, CANCEL_STOP_FAILED};
use crate::grpc;
use crate::grpc::proto::jobworkerp_conductor::data::{
    CronScheduler, CronSchedulerId, ExecutionRef, ExecutionRefId, ExecutionRuntimeStatus,
    ExecutionSourceType, ExecutionStatusSource, ResolvedExecutionStatus,
};
use crate::grpc::proto::jobworkerp_conductor::service::{
    ExecutionRuntimeStatusRequest, ExecutionSourceRequest,
    cron_scheduler_service_client::CronSchedulerServiceClient,
    execution_status_service_client::ExecutionStatusServiceClient,
};

use super::AppState;
use super::periodic_tasks::{
    display_name, is_lookback_managed, list_schedulers, parse_scheduler_id,
};

const DEFAULT_HISTORY_LIMIT: i32 = 20;
const MAX_HISTORY_LIMIT: i32 = 100;
const PERIODIC_CANCEL_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
const PERIODIC_CANCEL_SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(200);

// jobworkerp `ResultStatus` wire numbers (proto/jobworkerp/data/common.proto).
// These never appear in conductor proto — they leak through `result_status`
// (ExecutionRef field 11) — so the mapping is an implicit wire contract pinned
// here and asserted by a unit test (drift detection, `memory-role-mapping`
// style). Used only on the defensive path; the normal path reads conductor's
// already-resolved status.
const RESULT_STATUS_SUCCESS: i32 = 0;
const RESULT_STATUS_CANCELLED: i32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodicExecutionStatus {
    Pending,
    Running,
    WaitResult,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
    Unavailable,
    EnqueueFailed,
    NotStarted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodicExecutionStatusSource {
    JobProcessingStatus,
    JobResult,
    ExecutionRef,
    Unavailable,
    Unspecified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeriodicExecutionRuntime {
    pub execution_ref_id: String,
    pub scheduler_id: String,
    pub scheduler_name: String,
    pub job_id: Option<String>,
    pub status: PeriodicExecutionStatus,
    pub status_source: PeriodicExecutionStatusSource,
    pub triggered_at_ms: i64,
    pub observed_at_ms: Option<i64>,
    pub detail: Option<String>,
    pub enqueue_error: Option<String>,
    pub active: bool,
    pub cancelable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeriodicExecutionSummary {
    pub scheduler_id: String,
    pub status: PeriodicExecutionStatus,
    pub runtime: Option<PeriodicExecutionRuntime>,
    pub active: bool,
    pub cancelable: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeriodicExecutionHistoryEntry {
    pub execution_ref_id: String,
    pub scheduler_id: String,
    pub scheduler_name: String,
    pub job_id: Option<String>,
    pub status: PeriodicExecutionStatus,
    pub status_source: PeriodicExecutionStatusSource,
    pub triggered_at_ms: i64,
    pub observed_at_ms: Option<i64>,
    pub detail: Option<String>,
    pub enqueue_error: Option<String>,
    pub active: bool,
    pub cancelable: bool,
    pub trigger_context_json: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ListPeriodicTaskStatusesRequest {
    pub scheduler_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ListPeriodicExecutionHistoryRequest {
    pub scheduler_id: String,
    pub limit: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CancelPeriodicExecutionRequest {
    pub execution_ref_id: String,
}

// ---- pure conversions ----------------------------------------------------

fn map_status(proto: ResolvedExecutionStatus) -> PeriodicExecutionStatus {
    match proto {
        ResolvedExecutionStatus::Pending => PeriodicExecutionStatus::Pending,
        ResolvedExecutionStatus::Running => PeriodicExecutionStatus::Running,
        ResolvedExecutionStatus::WaitResult => PeriodicExecutionStatus::WaitResult,
        ResolvedExecutionStatus::Cancelling => PeriodicExecutionStatus::Cancelling,
        ResolvedExecutionStatus::Succeeded => PeriodicExecutionStatus::Succeeded,
        ResolvedExecutionStatus::Failed => PeriodicExecutionStatus::Failed,
        ResolvedExecutionStatus::Cancelled => PeriodicExecutionStatus::Cancelled,
        ResolvedExecutionStatus::Unavailable => PeriodicExecutionStatus::Unavailable,
        ResolvedExecutionStatus::EnqueueFailed => PeriodicExecutionStatus::EnqueueFailed,
        // UNKNOWN and the proto default UNSPECIFIED both surface as `unknown`.
        ResolvedExecutionStatus::Unknown | ResolvedExecutionStatus::Unspecified => {
            PeriodicExecutionStatus::Unknown
        }
    }
}

fn map_status_source(proto: ExecutionStatusSource) -> PeriodicExecutionStatusSource {
    match proto {
        ExecutionStatusSource::JobProcessingStatus => {
            PeriodicExecutionStatusSource::JobProcessingStatus
        }
        ExecutionStatusSource::JobResult => PeriodicExecutionStatusSource::JobResult,
        ExecutionStatusSource::ExecutionRef => PeriodicExecutionStatusSource::ExecutionRef,
        ExecutionStatusSource::Unavailable => PeriodicExecutionStatusSource::Unavailable,
        ExecutionStatusSource::Unspecified => PeriodicExecutionStatusSource::Unspecified,
    }
}

/// `cancelling` is active (the job is winding down) but NOT cancelable — there
/// is nothing left to request a stop for.
fn is_active(status: PeriodicExecutionStatus) -> bool {
    matches!(
        status,
        PeriodicExecutionStatus::Pending
            | PeriodicExecutionStatus::Running
            | PeriodicExecutionStatus::WaitResult
            | PeriodicExecutionStatus::Cancelling
    )
}

fn is_cancelable(status: PeriodicExecutionStatus) -> bool {
    matches!(
        status,
        PeriodicExecutionStatus::Pending
            | PeriodicExecutionStatus::Running
            | PeriodicExecutionStatus::WaitResult
    )
}

fn should_pause_scheduler(
    scheduler: &crate::grpc::proto::jobworkerp_conductor::data::CronScheduler,
) -> bool {
    scheduler
        .data
        .as_ref()
        .is_some_and(|data| data.enabled && is_lookback_managed(&data.name))
}

fn should_wait_for_terminal(status: PeriodicExecutionStatus) -> AppResult<bool> {
    match status {
        PeriodicExecutionStatus::Pending
        | PeriodicExecutionStatus::Running
        | PeriodicExecutionStatus::WaitResult
        | PeriodicExecutionStatus::Cancelling => Ok(true),
        PeriodicExecutionStatus::Succeeded
        | PeriodicExecutionStatus::Failed
        | PeriodicExecutionStatus::Cancelled
        | PeriodicExecutionStatus::EnqueueFailed
        | PeriodicExecutionStatus::NotStarted => Ok(false),
        PeriodicExecutionStatus::Unknown | PeriodicExecutionStatus::Unavailable => Err(
            AppError::Config("periodic execution status is unavailable".into()),
        ),
    }
}

/// jobworkerp terminal `result_status` → UI status. Defensive/drift-pinned: the
/// normal path uses conductor's resolved status, but this keeps the wire
/// contract explicit and testable. Only exercised by tests today (conductor
/// resolves `result_status` itself), so it is dead outside `cfg(test)`.
#[cfg_attr(not(test), allow(dead_code))]
fn result_status_to_status(n: i32) -> PeriodicExecutionStatus {
    match n {
        RESULT_STATUS_SUCCESS => PeriodicExecutionStatus::Succeeded,
        RESULT_STATUS_CANCELLED => PeriodicExecutionStatus::Cancelled,
        // ERROR_AND_RETRY(1) / FATAL_ERROR(2) / ABORT(3) / MAX_RETRY(4) /
        // OTHER_ERROR(5) all map to failed.
        1..=5 => PeriodicExecutionStatus::Failed,
        _ => PeriodicExecutionStatus::Unknown,
    }
}

fn secs_to_ms(secs: i64) -> i64 {
    secs.saturating_mul(1000)
}

/// epoch seconds → ms, treating a non-positive (unset/zero) value as absent.
fn opt_secs_to_ms(secs: i64) -> Option<i64> {
    (secs > 0).then(|| secs_to_ms(secs))
}

fn job_id_string(job_id: Option<i64>) -> Option<String> {
    job_id.map(|v| v.to_string())
}

/// Build the runtime view from a conductor `ExecutionRuntimeStatus`. Returns
/// `None` when `execution_ref` is missing (the caller maps that to an
/// `unavailable` summary / history row), which is the only structurally invalid
/// case conductor can return.
fn runtime_from_proto(
    rts: &ExecutionRuntimeStatus,
    scheduler_id: &str,
    scheduler_name: &str,
) -> Option<PeriodicExecutionRuntime> {
    let exec_ref = rts.execution_ref.as_ref()?;
    let exec_ref_id = exec_ref.id.as_ref()?;
    let status = map_status(rts.resolved_status());
    Some(PeriodicExecutionRuntime {
        execution_ref_id: exec_ref_id.value.to_string(),
        scheduler_id: scheduler_id.to_string(),
        scheduler_name: scheduler_name.to_string(),
        job_id: job_id_string(exec_ref.job_id),
        status,
        status_source: map_status_source(rts.status_source()),
        triggered_at_ms: secs_to_ms(exec_ref.triggered_at),
        observed_at_ms: opt_secs_to_ms(rts.observed_at),
        detail: rts.detail.clone(),
        enqueue_error: exec_ref.enqueue_error.clone(),
        active: is_active(status),
        cancelable: is_cancelable(status),
    })
}

fn should_skip_pause(sidecars_running: bool) -> bool {
    !sidecars_running
}

fn lookback_scheduler_ids(schedulers: &[CronScheduler]) -> AppResult<Vec<i64>> {
    schedulers
        .iter()
        .filter(|scheduler| {
            scheduler
                .data
                .as_ref()
                .is_some_and(|data| is_lookback_managed(&data.name))
        })
        .map(|scheduler| {
            scheduler
                .id
                .as_ref()
                .map(|id| id.value)
                .ok_or_else(|| AppError::Config("periodic scheduler did not include an id".into()))
        })
        .collect()
}

fn summary_from_runtime(
    scheduler_id: &str,
    runtime: PeriodicExecutionRuntime,
) -> PeriodicExecutionSummary {
    PeriodicExecutionSummary {
        scheduler_id: scheduler_id.to_string(),
        status: runtime.status,
        active: runtime.active,
        cancelable: runtime.cancelable,
        error: None,
        runtime: Some(runtime),
    }
}

fn summary_not_started(scheduler_id: &str) -> PeriodicExecutionSummary {
    PeriodicExecutionSummary {
        scheduler_id: scheduler_id.to_string(),
        status: PeriodicExecutionStatus::NotStarted,
        runtime: None,
        active: false,
        cancelable: false,
        error: None,
    }
}

fn summary_unavailable(scheduler_id: &str, error: impl Into<String>) -> PeriodicExecutionSummary {
    PeriodicExecutionSummary {
        scheduler_id: scheduler_id.to_string(),
        status: PeriodicExecutionStatus::Unavailable,
        runtime: None,
        active: false,
        cancelable: false,
        error: Some(error.into()),
    }
}

/// Build a history row from an `ExecutionRef` and its resolved runtime status.
/// `runtime` is `None` only on the defensive empty-`FindRuntimeStatus` path —
/// that row is reported as `unavailable` rather than synthesizing a terminal
/// status from `result_status`.
fn history_entry_from(
    scheduler_id: &str,
    scheduler_name: &str,
    exec_ref: &ExecutionRef,
    runtime: Option<&ExecutionRuntimeStatus>,
) -> Option<PeriodicExecutionHistoryEntry> {
    let exec_ref_id = exec_ref.id.as_ref()?;
    let (status, status_source, observed_at_ms, detail) = match runtime {
        Some(rts) => (
            map_status(rts.resolved_status()),
            map_status_source(rts.status_source()),
            opt_secs_to_ms(rts.observed_at),
            rts.detail.clone(),
        ),
        None => (
            PeriodicExecutionStatus::Unavailable,
            PeriodicExecutionStatusSource::Unavailable,
            None,
            Some("runtime status unavailable".to_string()),
        ),
    };
    Some(PeriodicExecutionHistoryEntry {
        execution_ref_id: exec_ref_id.value.to_string(),
        scheduler_id: scheduler_id.to_string(),
        scheduler_name: scheduler_name.to_string(),
        job_id: job_id_string(exec_ref.job_id),
        status,
        status_source,
        triggered_at_ms: secs_to_ms(exec_ref.triggered_at),
        observed_at_ms,
        detail,
        enqueue_error: exec_ref.enqueue_error.clone(),
        active: is_active(status),
        cancelable: is_cancelable(status),
        trigger_context_json: exec_ref.trigger_context_json.clone(),
        created_at_ms: secs_to_ms(exec_ref.created_at),
    })
}

/// Dedup preserving first-seen order (matches the array contract: input order
/// kept, duplicates collapsed to their first occurrence).
fn dedup_preserve_order(ids: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    // Borrow into the set (no owned key copy); the single clone happens once,
    // only for the ids that survive dedup.
    ids.iter()
        .filter(|id| seen.insert(id.as_str()))
        .cloned()
        .collect()
}

/// Validate / default the history `limit`. `None` → 20; outside `1..=100` is a
/// command error.
fn resolve_history_limit(limit: Option<i32>) -> AppResult<i32> {
    match limit {
        None => Ok(DEFAULT_HISTORY_LIMIT),
        Some(n) if (1..=MAX_HISTORY_LIMIT).contains(&n) => Ok(n),
        Some(n) => Err(AppError::Config(format!(
            "limit must be between 1 and {MAX_HISTORY_LIMIT}, got {n}"
        ))),
    }
}

// ---- gRPC plumbing -------------------------------------------------------

/// Reuse the cached conductor Channel and build both clients from it (tonic Channels
/// clone cheaply and multiplex). The raw Channel is returned too so callers that
/// need a third client (e.g. `list_schedulers`, which takes a Channel) reuse the
/// same connection.
async fn conductor_clients(
    state: &AppState,
) -> AppResult<(
    Channel,
    CronSchedulerServiceClient<Channel>,
    ExecutionStatusServiceClient<Channel>,
)> {
    let channel = state.conductor_channel().await?;
    Ok((
        channel.clone(),
        CronSchedulerServiceClient::new(channel.clone()),
        ExecutionStatusServiceClient::new(channel),
    ))
}

fn source_request(source_id: i64, limit: Option<i32>) -> ExecutionSourceRequest {
    ExecutionSourceRequest {
        source_type: ExecutionSourceType::CronScheduler as i32,
        source_id,
        limit,
        offset: None,
    }
}

fn ref_id(value: i64) -> ExecutionRefId {
    ExecutionRefId { value }
}

/// Enabled Lookback schedulers that were paused for a sidecar restart.
///
/// Only the enabled bit is restored. Restart refreshes the scheduler's runtime
/// arguments, so restoring a pre-restart record would otherwise overwrite the
/// newly resolved LLM contract.
#[derive(Debug, Default)]
pub(crate) struct PausedPeriodicSchedules {
    original: Vec<CronScheduler>,
}

impl PausedPeriodicSchedules {
    pub(crate) fn enabled_scheduler_ids_vec(&self) -> AppResult<Vec<i64>> {
        self.original
            .iter()
            .map(|scheduler| {
                scheduler.id.as_ref().map(|id| id.value).ok_or_else(|| {
                    AppError::Config("periodic scheduler did not include an id".into())
                })
            })
            .collect()
    }

    pub(crate) fn include_scheduler_ids(&mut self, ids: &[i64]) -> AppResult<()> {
        let mut existing = self.enabled_scheduler_ids()?;
        for id in ids {
            if existing.insert(*id) {
                self.original.push(CronScheduler {
                    id: Some(CronSchedulerId { value: *id }),
                    data: None,
                });
            }
        }
        Ok(())
    }

    fn enabled_scheduler_ids(&self) -> AppResult<HashSet<i64>> {
        Ok(self.enabled_scheduler_ids_vec()?.into_iter().collect())
    }

    /// Refresh the paused scheduler inputs before re-enabling them so a
    /// jobworkerp-only restart cannot leave an old model contract behind.
    pub(crate) async fn resume_after_restart(&self, state: &AppState) -> AppResult<()> {
        super::periodic_tasks::refresh_lookback_periodic_runtime_from_state(
            state,
            &self.enabled_scheduler_ids()?,
        )
        .await?;
        self.restore_with_budget_validation(state, true).await
    }

    async fn restore_without_budget_validation(&self, state: &AppState) -> AppResult<()> {
        self.restore_with_budget_validation(state, false).await
    }

    async fn restore_with_budget_validation(
        &self,
        state: &AppState,
        validate_budget: bool,
    ) -> AppResult<()> {
        if self.original.is_empty() {
            return Ok(());
        }
        let channel = grpc::connect(&state.conductor_url()?).await?;
        let mut cron = CronSchedulerServiceClient::new(channel);
        let settings = validate_budget.then(|| state.llm_settings_snapshot());
        for scheduler in &self.original {
            let id = scheduler.id.ok_or_else(|| {
                AppError::Config("periodic scheduler did not include an id".into())
            })?;
            let mut data = super::periodic_tasks::ensure_lookback_scheduler(&mut cron, id).await?;
            if let Some(settings) = settings.as_ref() {
                let input = data
                    .args
                    .as_deref()
                    .and_then(super::periodic_tasks::unwrap_worker_args)
                    .ok_or_else(|| {
                        AppError::Config("invalid Lookback periodic task arguments".into())
                    })?;
                if let Err(error) =
                    super::periodic_tasks::validate_enabled_periodic_task(settings, &input.task)
                {
                    warn!(
                        scheduler_id = id.value,
                        scheduler_name = %data.name,
                        error = %error,
                        "leaving periodic scheduler disabled because its new LLM budget is insufficient"
                    );
                    continue;
                }
            }
            data.enabled = true;
            cron.update(CronScheduler {
                id: Some(id),
                data: Some(data),
            })
            .await?;
        }
        Ok(())
    }
}

async fn active_periodic_execution_refs(
    status: &mut ExecutionStatusServiceClient<Channel>,
    scheduler_id: i64,
    deadline: Instant,
) -> AppResult<Vec<(i64, PeriodicExecutionStatus)>> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| AppError::Config("timed out checking periodic executions".into()))?;
    let refs = timeout(remaining, async {
        let response = status
            .find_list_by_source(source_request(scheduler_id, None))
            .await?;
        Ok::<_, tonic::Status>(response.into_inner().collect::<Vec<_>>().await)
    })
    .await
    .map_err(|_| AppError::Config("timed out checking periodic executions".into()))??;
    let mut active = Vec::new();
    for item in refs {
        let execution_ref = item?;
        let id = execution_ref.id.ok_or_else(|| {
            AppError::Config("periodic execution ref did not include an id".into())
        })?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| AppError::Config("timed out checking periodic executions".into()))?;
        let runtime = timeout(
            remaining,
            status.find_runtime_status(ExecutionRuntimeStatusRequest {
                id: Some(ref_id(id.value)),
            }),
        )
        .await
        .map_err(|_| AppError::Config("timed out checking periodic executions".into()))??
        .into_inner()
        .data
        .ok_or_else(|| AppError::Config("periodic execution runtime is unavailable".into()))?;
        let execution_status = map_status(runtime.resolved_status());
        if should_wait_for_terminal(execution_status)? {
            active.push((id.value, execution_status));
        }
    }
    Ok(active)
}

fn retain_active_execution_refs(
    active: &mut Vec<(i64, PeriodicExecutionStatus)>,
    resolved: impl IntoIterator<Item = (i64, PeriodicExecutionStatus)>,
) -> AppResult<()> {
    active.clear();
    for (id, execution_status) in resolved {
        if should_wait_for_terminal(execution_status)? {
            active.push((id, execution_status));
        }
    }
    Ok(())
}

async fn wait_for_periodic_executions_to_stop(
    status: &mut ExecutionStatusServiceClient<Channel>,
    active: &mut Vec<(i64, PeriodicExecutionStatus)>,
    deadline: Instant,
) -> AppResult<()> {
    loop {
        if active.is_empty() {
            return Ok(());
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                AppError::Config("timed out waiting for periodic executions to stop".into())
            })?;
        sleep(PERIODIC_CANCEL_SETTLE_POLL_INTERVAL.min(remaining)).await;

        let mut resolved = Vec::with_capacity(active.len());
        for (execution_ref_id, _) in active.iter() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    AppError::Config("timed out waiting for periodic executions to stop".into())
                })?;
            let runtime = timeout(
                remaining,
                status.find_runtime_status(ExecutionRuntimeStatusRequest {
                    id: Some(ref_id(*execution_ref_id)),
                }),
            )
            .await
            .map_err(|_| {
                AppError::Config("timed out waiting for periodic executions to stop".into())
            })??
            .into_inner()
            .data
            .ok_or_else(|| AppError::Config("periodic execution runtime is unavailable".into()))?;
            resolved.push((*execution_ref_id, map_status(runtime.resolved_status())));
        }
        retain_active_execution_refs(active, resolved)?;
    }
}

/// Stop normal Lookback periodic work before replacing jobworkerp.
///
/// This is intentionally a best-effort coordination protocol rather than a
/// distributed transaction: scheduler notifications are asynchronous and a
/// conductor outage can make an execution unobservable. In either case this
/// function fails before a restart, leaving the caller to keep the current
/// sidecars alive.
pub(crate) async fn pause_periodic_schedules_for_restart(
    state: &AppState,
) -> AppResult<PausedPeriodicSchedules> {
    pause_periodic_schedules_for_restart_with(state, |_| Ok(())).await
}

pub(crate) async fn pause_periodic_schedules_for_restart_with<F>(
    state: &AppState,
    before_pause: F,
) -> AppResult<PausedPeriodicSchedules>
where
    F: FnOnce(&PausedPeriodicSchedules) -> AppResult<()>,
{
    if should_skip_pause(state.sidecars.current_endpoints().is_some()) {
        let paused = PausedPeriodicSchedules::default();
        before_pause(&paused)?;
        return Ok(paused);
    }
    let (channel, mut cron, mut status) = conductor_clients(state).await?;
    let schedulers = list_schedulers(channel, None, None).await?;
    let original: Vec<CronScheduler> = schedulers
        .iter()
        .filter(|scheduler| should_pause_scheduler(scheduler))
        .cloned()
        .collect();
    let paused = PausedPeriodicSchedules { original };
    let scheduler_ids = lookback_scheduler_ids(&schedulers)?;
    let deadline = Instant::now() + PERIODIC_CANCEL_SETTLE_TIMEOUT;

    let pause_result = persist_snapshot_before_pause(
        || before_pause(&paused),
        || async {
            for scheduler in &paused.original {
                let mut disabled = scheduler.clone();
                let data = disabled.data.as_mut().ok_or_else(|| {
                    AppError::Config("periodic scheduler did not include data".into())
                })?;
                data.enabled = false;
                cron.update(disabled).await?;
            }

            let mut active = Vec::new();
            for scheduler_id in &scheduler_ids {
                active.extend(
                    active_periodic_execution_refs(&mut status, *scheduler_id, deadline).await?,
                );
            }
            for (execution_ref_id, execution_status) in &active {
                if is_cancelable(*execution_status)
                    && !status
                        .cancel_execution(ref_id(*execution_ref_id))
                        .await?
                        .into_inner()
                        .is_success
                {
                    return Err(AppError::Config(
                        "failed to cancel periodic execution".into(),
                    ));
                }
            }
            wait_for_periodic_executions_to_stop(&mut status, &mut active, deadline).await
        },
    )
    .await;

    if let Err(error) = pause_result {
        if let Err(restore_error) = paused.restore_without_budget_validation(state).await {
            return Err(AppError::Config(format!(
                "{error}; failed to restore periodic schedules: {restore_error}"
            )));
        }
        return Err(error);
    }
    Ok(paused)
}

async fn persist_snapshot_before_pause<T, P, Pause, Fut>(
    persist_snapshot: P,
    pause: Pause,
) -> AppResult<T>
where
    P: FnOnce() -> AppResult<()>,
    Pause: FnOnce() -> Fut,
    Fut: Future<Output = AppResult<T>>,
{
    persist_snapshot()?;
    pause().await
}

// ---- commands ------------------------------------------------------------

#[tauri::command]
pub async fn list_periodic_task_statuses(
    state: tauri::State<'_, AppState>,
    req: ListPeriodicTaskStatusesRequest,
) -> AppResult<Vec<PeriodicExecutionSummary>> {
    let ids = dedup_preserve_order(&req.scheduler_ids);
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let (channel, _cron, status) = conductor_clients(&state).await?;

    // One FindList resolves id → display name for every Lookback-managed
    // scheduler (ownership check), avoiding a per-id Find (2N → N+1).
    let schedulers = list_schedulers(channel, None, None).await?;
    let managed: HashMap<i64, String> = schedulers
        .into_iter()
        .filter_map(|s| {
            let id = s.id?.value;
            let name = s.data?.name;
            is_lookback_managed(&name).then(|| (id, display_name(&name).to_string()))
        })
        .collect();

    Ok(resolve_summaries(&ids, &managed, |id_value| {
        let mut status = status.clone();
        async move {
            status
                .find_latest_runtime_status_by_source(source_request(id_value, None))
                .await
                .map(|resp| resp.into_inner().data)
        }
    })
    .await)
}

/// Per-id lookups are independent; they run concurrently and the summaries
/// keep request order.
async fn resolve_summaries<F, Fut>(
    ids: &[String],
    managed: &HashMap<i64, String>,
    find_latest: F,
) -> Vec<PeriodicExecutionSummary>
where
    F: Fn(i64) -> Fut,
    Fut: std::future::Future<Output = Result<Option<ExecutionRuntimeStatus>, tonic::Status>>,
{
    futures::future::join_all(
        ids.iter()
            .map(|id| resolve_one_summary(managed, id, &find_latest)),
    )
    .await
}

/// Resolve a single scheduler's summary, never failing the whole command: an
/// invalid id, an unmanaged/missing scheduler, or a per-id RPC error all
/// collapse to an `unavailable` summary carrying the id and an error string.
async fn resolve_one_summary<F, Fut>(
    managed: &HashMap<i64, String>,
    id: &str,
    find_latest: F,
) -> PeriodicExecutionSummary
where
    F: Fn(i64) -> Fut,
    Fut: std::future::Future<Output = Result<Option<ExecutionRuntimeStatus>, tonic::Status>>,
{
    let Ok(id_value) = id.parse::<i64>() else {
        return summary_unavailable(id, "invalid scheduler id");
    };
    let Some(name) = managed.get(&id_value) else {
        return summary_unavailable(id, "scheduler not found or not Lookback-managed");
    };
    let latest = match find_latest(id_value).await {
        Ok(latest) => latest,
        Err(e) => return summary_unavailable(id, e.message().to_string()),
    };
    // Empty (never run) → not_started; present but ref-less → unavailable.
    let Some(rts) = latest else {
        return summary_not_started(id);
    };
    match runtime_from_proto(&rts, id, name) {
        Some(runtime) => summary_from_runtime(id, runtime),
        None => summary_unavailable(id, "execution ref missing"),
    }
}

#[tauri::command]
pub async fn list_periodic_execution_history(
    state: tauri::State<'_, AppState>,
    req: ListPeriodicExecutionHistoryRequest,
) -> AppResult<Vec<PeriodicExecutionHistoryEntry>> {
    let limit = resolve_history_limit(req.limit)?;
    let id_value = parse_scheduler_id(&req.scheduler_id)?;
    let (_channel, mut cron, mut status) = conductor_clients(&state).await?;

    // Ownership: a missing / non-Lookback scheduler is a command error here
    // (single-target command, unlike the status list).
    let data = super::periodic_tasks::ensure_lookback_scheduler(
        &mut cron,
        CronSchedulerId { value: id_value },
    )
    .await?;
    let scheduler_name = display_name(&data.name).to_string();

    let refs = status
        .find_list_by_source(source_request(id_value, Some(limit)))
        .await?
        .into_inner()
        .collect::<Vec<_>>()
        .await;

    let mut addressable = Vec::with_capacity(refs.len());
    for item in refs {
        let exec_ref = item?;
        let Some(exec_ref_id) = exec_ref.id.as_ref().map(|i| i.value) else {
            // Defensive: conductor's id is NOT NULL, so this never fires in
            // practice. A row we can't address by id can't be acted on, so skip.
            warn!("skipping execution ref with missing id during history fetch");
            continue;
        };
        addressable.push((exec_ref, exec_ref_id));
    }
    let ref_ids: Vec<i64> = addressable.iter().map(|(_, id)| *id).collect();
    let runtimes = fetch_runtimes(&ref_ids, |exec_ref_id| {
        let mut status = status.clone();
        async move {
            status
                .find_runtime_status(ExecutionRuntimeStatusRequest {
                    id: Some(ref_id(exec_ref_id)),
                })
                .await
                .map(|response| response.into_inner().data)
        }
    })
    .await?;
    Ok(addressable
        .iter()
        .zip(runtimes)
        .filter_map(|((exec_ref, _), runtime)| {
            history_entry_from(
                &req.scheduler_id,
                &scheduler_name,
                exec_ref,
                runtime.as_ref(),
            )
        })
        .collect())
}

/// Runtime lookups are independent; they run concurrently and keep ref
/// order. Any failed lookup fails the history read as before.
async fn fetch_runtimes<F, Fut>(
    ref_ids: &[i64],
    find_runtime: F,
) -> Result<Vec<Option<ExecutionRuntimeStatus>>, tonic::Status>
where
    F: Fn(i64) -> Fut,
    Fut: std::future::Future<Output = Result<Option<ExecutionRuntimeStatus>, tonic::Status>>,
{
    futures::future::try_join_all(ref_ids.iter().map(|&id| find_runtime(id))).await
}

#[tauri::command]
pub async fn cancel_periodic_execution(
    state: tauri::State<'_, AppState>,
    req: CancelPeriodicExecutionRequest,
) -> AppResult<()> {
    let ref_id_value = req
        .execution_ref_id
        .parse::<i64>()
        .map_err(|e| AppError::Config(format!("invalid execution ref id: {e}")))?;
    let (_channel, mut cron, mut status) = conductor_clients(&state).await?;

    let exec_ref = status
        .find_execution_ref(ref_id(ref_id_value))
        .await?
        .into_inner()
        .data
        .ok_or_else(|| AppError::Config("execution ref not found".into()))?;
    if exec_ref.source_type() != ExecutionSourceType::CronScheduler {
        return Err(AppError::Config(CANCEL_NOT_MANAGED.into()));
    }
    // Prove the backing scheduler is Lookback's before issuing the stop.
    super::periodic_tasks::ensure_lookback_scheduler(
        &mut cron,
        CronSchedulerId {
            value: exec_ref.source_id,
        },
    )
    .await?;

    let is_success = status
        .cancel_execution(ref_id(ref_id_value))
        .await?
        .into_inner()
        .is_success;
    if !is_success {
        return Err(AppError::Config(CANCEL_STOP_FAILED.into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::proto::jobworkerp_conductor::data::ExecutionRefId;

    #[tokio::test]
    async fn summaries_resolve_concurrently_in_request_order_without_failing_the_batch() {
        // Only the two managed ids reach the RPC, and they must overlap.
        let gate = crate::commands::concurrency_probe::ConcurrencyGate::new(2);
        let managed = HashMap::from([(1_i64, "one".to_string()), (2, "two".to_string())]);
        let ids: Vec<String> = ["1", "x", "2", "9"].map(str::to_owned).to_vec();
        let summaries = resolve_summaries(&ids, &managed, |id| {
            let gate = &gate;
            async move {
                let result = if id == 1 {
                    Err(tonic::Status::unavailable("conductor down"))
                } else {
                    // Never run.
                    Ok(None)
                };
                gate.call(result).await
            }
        })
        .await;

        assert_eq!(
            summaries
                .iter()
                .map(|s| s.scheduler_id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "x", "2", "9"]
        );
        assert_eq!(summaries[0].status, PeriodicExecutionStatus::Unavailable);
        assert_eq!(summaries[0].error.as_deref(), Some("conductor down"));
        assert_eq!(summaries[1].error.as_deref(), Some("invalid scheduler id"));
        assert_eq!(summaries[2].status, PeriodicExecutionStatus::NotStarted);
        assert_eq!(
            summaries[3].error.as_deref(),
            Some("scheduler not found or not Lookback-managed")
        );
    }

    #[tokio::test]
    async fn history_runtimes_resolve_concurrently_in_ref_order_and_fail_on_any_error() {
        let gate = crate::commands::concurrency_probe::ConcurrencyGate::new(3);
        let runtimes = fetch_runtimes(&[1, 2, 3], |ref_id| {
            let gate = &gate;
            async move {
                let runtime = ExecutionRuntimeStatus {
                    observed_at: ref_id,
                    ..Default::default()
                };
                Ok(gate.call(Some(runtime)).await)
            }
        })
        .await
        .unwrap();
        assert_eq!(
            runtimes
                .iter()
                .map(|runtime| runtime.as_ref().map(|r| r.observed_at))
                .collect::<Vec<_>>(),
            vec![Some(1), Some(2), Some(3)]
        );

        let err = fetch_runtimes(&[1, 2], |ref_id| async move {
            if ref_id == 2 {
                Err(tonic::Status::unavailable("down"))
            } else {
                Ok(None)
            }
        })
        .await
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    fn exec_ref(id: i64) -> ExecutionRef {
        ExecutionRef {
            id: Some(ExecutionRefId { value: id }),
            source_type: ExecutionSourceType::CronScheduler as i32,
            source_id: 1,
            source_name: "lookback-periodic:x".into(),
            jobworkerp_server_id: None,
            job_id: Some(12345),
            triggered_at: 1_700_000_000,
            trigger_context_json: Some("{\"k\":1}".into()),
            enqueue_error: None,
            created_at: 1_700_000_001,
            result_status: None,
        }
    }

    fn runtime_status(
        status: ResolvedExecutionStatus,
        source: ExecutionStatusSource,
    ) -> ExecutionRuntimeStatus {
        ExecutionRuntimeStatus {
            execution_ref: Some(exec_ref(7)),
            resolved_status: status as i32,
            status_source: source as i32,
            observed_at: 1_700_000_005,
            detail: Some("d".into()),
        }
    }

    #[tokio::test]
    async fn pause_snapshot_is_durable_before_scheduler_disable_and_survives_crash() {
        use std::sync::{Arc, Mutex};

        let journal_ids = Arc::new(Mutex::new(None));
        let ids_for_persist = journal_ids.clone();
        let ids_for_disable = journal_ids.clone();
        let result = persist_snapshot_before_pause(
            || {
                *ids_for_persist.lock().unwrap() = Some(vec![17, 29]);
                Ok(())
            },
            || async move {
                assert_eq!(*ids_for_disable.lock().unwrap(), Some(vec![17, 29]));
                Err::<(), _>(AppError::Config("simulated crash after pause".into()))
            },
        )
        .await;

        assert!(result.is_err());
        assert_eq!(*journal_ids.lock().unwrap(), Some(vec![17, 29]));
    }

    #[test]
    fn maps_each_resolved_status() {
        use PeriodicExecutionStatus as S;
        use ResolvedExecutionStatus as R;
        assert_eq!(map_status(R::Pending), S::Pending);
        assert_eq!(map_status(R::Running), S::Running);
        assert_eq!(map_status(R::WaitResult), S::WaitResult);
        assert_eq!(map_status(R::Cancelling), S::Cancelling);
        assert_eq!(map_status(R::Succeeded), S::Succeeded);
        assert_eq!(map_status(R::Failed), S::Failed);
        assert_eq!(map_status(R::Cancelled), S::Cancelled);
        assert_eq!(map_status(R::Unavailable), S::Unavailable);
        assert_eq!(map_status(R::EnqueueFailed), S::EnqueueFailed);
        assert_eq!(map_status(R::Unknown), S::Unknown);
        // Proto default UNSPECIFIED collapses to unknown.
        assert_eq!(map_status(R::Unspecified), S::Unknown);
    }

    #[test]
    fn maps_status_source_to_snake_case_union() {
        use ExecutionStatusSource as P;
        use PeriodicExecutionStatusSource as U;
        assert_eq!(
            map_status_source(P::JobProcessingStatus),
            U::JobProcessingStatus
        );
        assert_eq!(map_status_source(P::JobResult), U::JobResult);
        assert_eq!(map_status_source(P::ExecutionRef), U::ExecutionRef);
        assert_eq!(map_status_source(P::Unavailable), U::Unavailable);
        assert_eq!(map_status_source(P::Unspecified), U::Unspecified);
        // Serde emits the documented wire strings.
        assert_eq!(
            serde_json::to_string(&U::JobProcessingStatus).unwrap(),
            "\"job_processing_status\""
        );
    }

    #[test]
    fn cancelling_is_active_but_not_cancelable() {
        assert!(is_active(PeriodicExecutionStatus::Cancelling));
        assert!(!is_cancelable(PeriodicExecutionStatus::Cancelling));
        for s in [
            PeriodicExecutionStatus::Pending,
            PeriodicExecutionStatus::Running,
            PeriodicExecutionStatus::WaitResult,
        ] {
            assert!(is_active(s) && is_cancelable(s));
        }
    }

    #[test]
    fn terminal_statuses_are_neither_active_nor_cancelable() {
        for s in [
            PeriodicExecutionStatus::Succeeded,
            PeriodicExecutionStatus::Failed,
            PeriodicExecutionStatus::Cancelled,
            PeriodicExecutionStatus::EnqueueFailed,
            PeriodicExecutionStatus::Unknown,
            PeriodicExecutionStatus::Unavailable,
            PeriodicExecutionStatus::NotStarted,
        ] {
            assert!(!is_active(s));
            assert!(!is_cancelable(s));
        }
    }

    #[test]
    fn result_status_table_pins_wire_literals() {
        assert_eq!(
            result_status_to_status(0),
            PeriodicExecutionStatus::Succeeded
        );
        assert_eq!(
            result_status_to_status(6),
            PeriodicExecutionStatus::Cancelled
        );
        for n in 1..=5 {
            assert_eq!(result_status_to_status(n), PeriodicExecutionStatus::Failed);
        }
        assert_eq!(
            result_status_to_status(99),
            PeriodicExecutionStatus::Unknown
        );
        assert_eq!(
            result_status_to_status(-1),
            PeriodicExecutionStatus::Unknown
        );
    }

    #[test]
    fn runtime_converts_epoch_seconds_to_ms() {
        let rts = runtime_status(
            ResolvedExecutionStatus::Running,
            ExecutionStatusSource::JobProcessingStatus,
        );
        let rt = runtime_from_proto(&rts, "7", "x").unwrap();
        assert_eq!(rt.triggered_at_ms, 1_700_000_000_000);
        assert_eq!(rt.observed_at_ms, Some(1_700_000_005_000));
        assert_eq!(rt.job_id.as_deref(), Some("12345"));
        assert_eq!(rt.execution_ref_id, "7");
        assert!(rt.active && rt.cancelable);
        assert_eq!(rt.status, PeriodicExecutionStatus::Running);
    }

    #[test]
    fn runtime_missing_execution_ref_is_none() {
        let rts = ExecutionRuntimeStatus {
            execution_ref: None,
            resolved_status: ResolvedExecutionStatus::Running as i32,
            status_source: ExecutionStatusSource::JobProcessingStatus as i32,
            observed_at: 1,
            detail: None,
        };
        assert!(runtime_from_proto(&rts, "7", "x").is_none());
    }

    #[test]
    fn observed_at_zero_is_absent() {
        let mut rts = runtime_status(
            ResolvedExecutionStatus::Succeeded,
            ExecutionStatusSource::JobResult,
        );
        rts.observed_at = 0;
        let rt = runtime_from_proto(&rts, "7", "x").unwrap();
        assert_eq!(rt.observed_at_ms, None);
    }

    #[test]
    fn summary_not_started_has_no_runtime_or_status_source() {
        let s = summary_not_started("3");
        assert_eq!(s.status, PeriodicExecutionStatus::NotStarted);
        assert!(s.runtime.is_none());
        assert!(!s.active && !s.cancelable);
        assert!(s.error.is_none());
        // The serialized summary must not carry a status_source field.
        let json = serde_json::to_value(&s).unwrap();
        assert!(json.get("status_source").is_none());
    }

    #[test]
    fn summary_unavailable_carries_error_and_scheduler_id_field() {
        let s = summary_unavailable("not-a-number", "invalid scheduler id");
        assert_eq!(s.status, PeriodicExecutionStatus::Unavailable);
        assert_eq!(s.error.as_deref(), Some("invalid scheduler id"));
        let json = serde_json::to_value(&s).unwrap();
        // The invalid id is a value of `scheduler_id`, NOT an object key.
        assert_eq!(json["scheduler_id"], "not-a-number");
    }

    #[test]
    fn history_entry_carries_trigger_context_and_created_at() {
        let er = exec_ref(7);
        let rts = runtime_status(
            ResolvedExecutionStatus::Succeeded,
            ExecutionStatusSource::JobResult,
        );
        let entry = history_entry_from("1", "x", &er, Some(&rts)).unwrap();
        assert_eq!(entry.execution_ref_id, "7");
        assert_eq!(entry.trigger_context_json.as_deref(), Some("{\"k\":1}"));
        assert_eq!(entry.created_at_ms, 1_700_000_001_000);
        assert_eq!(entry.status, PeriodicExecutionStatus::Succeeded);
        assert_eq!(
            entry.status_source,
            PeriodicExecutionStatusSource::JobResult
        );
        assert!(!entry.active && !entry.cancelable);
    }

    #[test]
    fn history_entry_without_runtime_is_unavailable_not_synthesized_terminal() {
        let er = exec_ref(7);
        let entry = history_entry_from("1", "x", &er, None).unwrap();
        assert_eq!(entry.status, PeriodicExecutionStatus::Unavailable);
        assert_eq!(
            entry.status_source,
            PeriodicExecutionStatusSource::Unavailable
        );
    }

    #[test]
    fn dedup_preserves_first_seen_order() {
        let ids = vec![
            "3".to_string(),
            "1".to_string(),
            "3".to_string(),
            "2".to_string(),
            "1".to_string(),
        ];
        assert_eq!(dedup_preserve_order(&ids), vec!["3", "1", "2"]);
    }

    #[test]
    fn history_limit_defaults_and_validates_range() {
        assert_eq!(resolve_history_limit(None).unwrap(), DEFAULT_HISTORY_LIMIT);
        assert_eq!(resolve_history_limit(Some(1)).unwrap(), 1);
        assert_eq!(resolve_history_limit(Some(100)).unwrap(), 100);
        assert!(resolve_history_limit(Some(0)).is_err());
        assert!(resolve_history_limit(Some(-1)).is_err());
        assert!(resolve_history_limit(Some(101)).is_err());
    }

    #[test]
    fn paused_schedule_journal_ids_preserve_the_original_enabled_schedules() {
        let paused = PausedPeriodicSchedules {
            original: vec![
                CronScheduler {
                    id: Some(CronSchedulerId { value: 13 }),
                    data: None,
                },
                CronScheduler {
                    id: Some(CronSchedulerId { value: 27 }),
                    data: None,
                },
            ],
        };

        assert_eq!(paused.enabled_scheduler_ids_vec().unwrap(), [13, 27]);
    }

    #[test]
    fn recovered_paused_schedule_ids_are_added_without_duplicates() {
        let mut paused = PausedPeriodicSchedules::default();
        paused.include_scheduler_ids(&[13, 27, 13]).unwrap();

        assert_eq!(paused.enabled_scheduler_ids_vec().unwrap(), [13, 27]);
    }

    #[test]
    fn pause_targets_only_enabled_lookback_schedulers() {
        use crate::grpc::proto::jobworkerp_conductor::data::{CronScheduler, CronSchedulerData};

        let scheduler = |name: &str, enabled: bool| CronScheduler {
            id: None,
            data: Some(CronSchedulerData {
                name: name.into(),
                enabled,
                ..Default::default()
            }),
        };

        assert!(should_pause_scheduler(&scheduler(
            "lookback-periodic:daily",
            true
        )));
        assert!(!should_pause_scheduler(&scheduler(
            "lookback-periodic:daily",
            false
        )));
        assert!(!should_pause_scheduler(&scheduler("third-party", true)));
        assert!(!should_pause_scheduler(&CronScheduler::default()));
    }

    #[test]
    fn quiesce_targets_enabled_and_disabled_lookback_schedulers() {
        use crate::grpc::proto::jobworkerp_conductor::data::{CronScheduler, CronSchedulerData};

        let scheduler = |id, name: &str, enabled| CronScheduler {
            id: Some(CronSchedulerId { value: id }),
            data: Some(CronSchedulerData {
                name: name.into(),
                enabled,
                ..Default::default()
            }),
        };
        let enabled = scheduler(1, "lookback-periodic:enabled", true);
        let disabled = scheduler(2, "lookback-periodic:disabled", false);
        let external = scheduler(3, "external", true);

        assert_eq!(
            lookback_scheduler_ids(&[enabled.clone(), disabled, external]).unwrap(),
            vec![1, 2]
        );
        assert!(should_pause_scheduler(&enabled));
    }

    #[test]
    fn pause_is_skipped_when_sidecars_are_not_running() {
        assert!(should_skip_pause(false));
        assert!(!should_skip_pause(true));
    }

    #[test]
    fn restart_waits_for_active_execution_and_rejects_indeterminate_status() {
        for status in [
            PeriodicExecutionStatus::Pending,
            PeriodicExecutionStatus::Running,
            PeriodicExecutionStatus::WaitResult,
            PeriodicExecutionStatus::Cancelling,
        ] {
            assert!(should_wait_for_terminal(status).unwrap());
        }
        for status in [
            PeriodicExecutionStatus::Succeeded,
            PeriodicExecutionStatus::Failed,
            PeriodicExecutionStatus::Cancelled,
            PeriodicExecutionStatus::EnqueueFailed,
            PeriodicExecutionStatus::NotStarted,
        ] {
            assert!(!should_wait_for_terminal(status).unwrap());
        }
        assert!(should_wait_for_terminal(PeriodicExecutionStatus::Unknown).is_err());
        assert!(should_wait_for_terminal(PeriodicExecutionStatus::Unavailable).is_err());
    }

    #[test]
    fn settled_polling_retains_only_previously_active_execution_ids() {
        let mut active = vec![
            (10, PeriodicExecutionStatus::Cancelling),
            (11, PeriodicExecutionStatus::Running),
        ];
        retain_active_execution_refs(
            &mut active,
            [
                (10, PeriodicExecutionStatus::Cancelled),
                (11, PeriodicExecutionStatus::WaitResult),
            ],
        )
        .unwrap();
        assert_eq!(active, vec![(11, PeriodicExecutionStatus::WaitResult)]);
    }
}
