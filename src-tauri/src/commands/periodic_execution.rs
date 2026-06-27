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

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt as _;
use tonic::transport::Channel;
use tracing::warn;

use crate::error::{AppError, AppResult};
use crate::grpc;
use crate::grpc::proto::jobworkerp_conductor::data::{
    CronSchedulerId, ExecutionRef, ExecutionRefId, ExecutionRuntimeStatus, ExecutionSourceType,
    ExecutionStatusSource, ResolvedExecutionStatus,
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

/// Open one conductor Channel and build both clients from it (tonic Channels
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
    let endpoints = state.require_endpoints()?;
    let channel = grpc::connect(&endpoints.conductor_url()).await?;
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
    let (channel, _cron, mut status) = conductor_clients(&state).await?;

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

    let mut out = Vec::with_capacity(ids.len());
    for id in &ids {
        out.push(resolve_one_summary(&mut status, &managed, id).await);
    }
    Ok(out)
}

/// Resolve a single scheduler's summary, never failing the whole command: an
/// invalid id, an unmanaged/missing scheduler, or a per-id RPC error all
/// collapse to an `unavailable` summary carrying the id and an error string.
async fn resolve_one_summary(
    status: &mut ExecutionStatusServiceClient<Channel>,
    managed: &HashMap<i64, String>,
    id: &str,
) -> PeriodicExecutionSummary {
    let Ok(id_value) = id.parse::<i64>() else {
        return summary_unavailable(id, "invalid scheduler id");
    };
    let Some(name) = managed.get(&id_value) else {
        return summary_unavailable(id, "scheduler not found or not Lookback-managed");
    };
    let resp = match status
        .find_latest_runtime_status_by_source(source_request(id_value, None))
        .await
    {
        Ok(resp) => resp,
        Err(e) => return summary_unavailable(id, e.message().to_string()),
    };
    // Empty (never run) → not_started; present but ref-less → unavailable.
    let Some(rts) = resp.into_inner().data else {
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

    let mut out = Vec::with_capacity(refs.len());
    for item in refs {
        let exec_ref = item?;
        let Some(exec_ref_id) = exec_ref.id.as_ref().map(|i| i.value) else {
            // Defensive: conductor's id is NOT NULL, so this never fires in
            // practice. A row we can't address by id can't be acted on, so skip.
            warn!("skipping execution ref with missing id during history fetch");
            continue;
        };
        let runtime = status
            .find_runtime_status(ExecutionRuntimeStatusRequest {
                id: Some(ref_id(exec_ref_id)),
            })
            .await?
            .into_inner()
            .data;
        if let Some(entry) = history_entry_from(
            &req.scheduler_id,
            &scheduler_name,
            &exec_ref,
            runtime.as_ref(),
        ) {
            out.push(entry);
        }
    }
    Ok(out)
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
        return Err(AppError::Config(
            "この実行は Lookback 管理対象ではありません".into(),
        ));
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
        return Err(AppError::Config("停止要求を送信できませんでした".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::proto::jobworkerp_conductor::data::ExecutionRefId;

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
}
