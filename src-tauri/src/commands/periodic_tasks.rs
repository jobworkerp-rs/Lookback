//! Local periodic task management backed by conductor's CronSchedulerService.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt as _;
use tonic::transport::Channel;
use tracing::warn;

use crate::error::{AppError, AppResult};
use crate::grpc;
use crate::grpc::proto::jobworkerp_conductor::data::{
    CronScheduler, CronSchedulerData, CronSchedulerId, JobworkerpServer, JobworkerpServerData,
    JobworkerpServerId, WorkerExecution, cron_scheduler_data,
};
use crate::grpc::proto::jobworkerp_conductor::service::{
    FindByNameRequest, FindListRequest, cron_scheduler_service_client::CronSchedulerServiceClient,
    jobworkerp_server_service_client::JobworkerpServerServiceClient,
};

use super::AppState;

pub const LOOKBACK_PERIODIC_PREFIX: &str = "lookback-periodic:";
pub const LOOKBACK_JOBWORKERP_SERVER_NAME: &str = "lookback-local-jobworkerp";
pub const LOOKBACK_PERIODIC_WORKER: &str = "memories-lookback-periodic-run";
const SCHEMA_VERSION: u32 = 1;
const DEFAULT_PERIODIC_SEED_VERSION: u32 = 1;

// Source vocabulary shared as a wire contract with the frontend
// (`src/lib/periodicTask.ts`) and the periodic-run workflow YAML. Keep the three
// layers in sync; the YAML cannot import these so a workflow test asserts the
// literal match.
const SOURCE_CODEX: &str = "codex";
const SOURCE_CLAUDE_CODE: &str = "claude-code";
const SOURCE_COMBINED: &str = "codex+claude-code";
const ALLOWED_INTERVAL_HOURS: &[u16] = &[1, 2, 3, 4, 6, 8, 12, 24];
const INTERVAL_HOURS_ERROR: &str = "interval_hours must be one of 1,2,3,4,6,8,12,24";

fn default_true() -> bool {
    true
}

fn default_memories_import_bin() -> String {
    "memories-import".to_string()
}

fn memories_import_bin_for_task(task: &PeriodicTaskArgs) -> AppResult<String> {
    memories_import_bin_for_task_result(task, crate::resolve_memories_import_bin_path())
}

fn memories_import_bin_for_task_result(
    task: &PeriodicTaskArgs,
    resolved: Result<std::path::PathBuf, AppError>,
) -> AppResult<String> {
    match resolved {
        Ok(path) => Ok(path.to_string_lossy().into_owned()),
        Err(e) if task.task_kind == PeriodicTaskKind::Regular => Err(e),
        Err(_) => Ok(default_memories_import_bin()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodicTaskKind {
    Regular,
    Weekly,
    Monthly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeriodicTaskArgs {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub sources: Vec<String>,
    pub task_kind: PeriodicTaskKind,
    pub hour: u8,
    pub minute: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_hours: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_days: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_day: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monthly_day: Option<u8>,
    pub lookback_days: u16,
    // Default-true to match the YAML schema (`force_thread_summary` /
    // `run_summary_daily` both default true): a row missing these keys (legacy
    // scheduler / hand-built input) must still run the basic summary stages, not
    // deserialize to a no-op false.
    #[serde(default = "default_true")]
    pub force_thread_summary: bool,
    #[serde(default = "default_true")]
    pub run_summary_daily: bool,
    #[serde(default)]
    pub run_personality: bool,
    #[serde(default)]
    pub run_reflection: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeriodicRuntime {
    pub memories_grpc_host: String,
    pub memories_grpc_port: u16,
    pub memories_grpc_tls: bool,
    pub llm_worker_name: String,
    /// Output language ("ja" | "en") the wrapper forwards into each batch as
    /// `output_language`, so the batches resolve the matching language-specific
    /// single worker (`memories-<feature>-single-<lang>`). Replaces the old
    /// per-feature `*-single.yaml` path relay (those workers are now registered
    /// by name at sidecar start). A scheduler persisted by an older build lacks
    /// this field and deserializes to `"ja"` (via the default fn) rather than to
    /// `""` — an empty value is NOT rescued by the YAML's `default`/jq `//` and
    /// would resolve a nonexistent `memories-<feature>-single-` worker. The
    /// runtime constructors also normalize an empty input to `"ja"`.
    #[serde(default = "default_output_language")]
    pub output_language: String,
    /// Absolute path to the bundled/importer CLI used by headless periodic
    /// `regular` tasks. Older schedulers miss this field and fall back to the
    /// command name so they remain deserializable until the startup refresh
    /// rewrites them with the resolved path.
    #[serde(default = "default_memories_import_bin")]
    pub memories_import_bin: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PeriodicDefaultsSeedMarker {
    version: u32,
}

/// Default for [`PeriodicRuntime::output_language`] when an older scheduler's
/// persisted args omit it. Mirrors the dispatch-side `DEFAULT_OUTPUT_LANGUAGE`.
fn default_output_language() -> String {
    "ja".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeriodicWorkflowInput {
    pub schema_version: u32,
    pub task: PeriodicTaskArgs,
    pub runtime: PeriodicRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConductorWorkerArgs {
    pub input: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodicTaskStatus {
    Supported,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeriodicTaskEntry {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub crontab: String,
    pub description: Option<String>,
    pub task: Option<PeriodicTaskArgs>,
    pub status: PeriodicTaskStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ListPeriodicTasksRequest {
    pub limit: Option<i32>,
    pub offset: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SavePeriodicTaskRequest {
    pub id: Option<String>,
    pub task: PeriodicTaskArgs,
    pub enabled: bool,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PeriodicTaskIdRequest {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SetPeriodicTaskEnabledRequest {
    pub id: String,
    pub enabled: bool,
}

pub fn validate_task(task: &PeriodicTaskArgs) -> AppResult<()> {
    if task.name.trim().is_empty() {
        return Err(AppError::Config("定期実行名を入力してください".into()));
    }
    let sources = effective_sources(task);
    if sources.is_empty() {
        return Err(AppError::Config("対象 source を選択してください".into()));
    }
    for source in &sources {
        if !matches!(source.as_str(), SOURCE_CODEX | SOURCE_CLAUDE_CODE) {
            return Err(AppError::Config(
                "source は codex / claude-code / codex + claude-code から選択してください".into(),
            ));
        }
    }
    if task.hour > 23 {
        return Err(AppError::Config("hour must be 0..23".into()));
    }
    if task.minute > 59 {
        return Err(AppError::Config("minute must be 0..59".into()));
    }
    if task.lookback_days == 0 {
        return Err(AppError::Config(
            "lookback_days must be greater than 0".into(),
        ));
    }
    match task.task_kind {
        PeriodicTaskKind::Regular => {
            let has_hours = task.interval_hours.is_some();
            let has_days = task.interval_days.is_some();
            if has_hours == has_days {
                return Err(AppError::Config(
                    "regular task requires exactly one interval".into(),
                ));
            }
            if matches!(task.interval_hours, Some(0)) || matches!(task.interval_days, Some(0)) {
                return Err(AppError::Config("interval must be greater than 0".into()));
            }
            if task
                .interval_hours
                .is_some_and(|hours| !ALLOWED_INTERVAL_HOURS.contains(&hours))
            {
                return Err(AppError::Config(INTERVAL_HOURS_ERROR.into()));
            }
            if task.run_summary_daily && !task.force_thread_summary {
                return Err(AppError::Config(
                    "run_summary_daily requires force_thread_summary".into(),
                ));
            }
        }
        PeriodicTaskKind::Weekly => match task.weekly_day {
            Some(0..=6) => {}
            _ => return Err(AppError::Config("weekly_day must be 0..6".into())),
        },
        PeriodicTaskKind::Monthly => match task.monthly_day {
            Some(1..=28) => {}
            _ => return Err(AppError::Config("monthly_day must be 1..28".into())),
        },
    }
    Ok(())
}

pub fn effective_sources(task: &PeriodicTaskArgs) -> Vec<String> {
    if !task.sources.is_empty() {
        return task
            .sources
            .iter()
            .filter_map(|source| {
                let source = source.trim();
                (!source.is_empty()).then(|| source.to_owned())
            })
            .collect();
    }
    match task.source.trim() {
        SOURCE_COMBINED => vec![SOURCE_CODEX.into(), SOURCE_CLAUDE_CODE.into()],
        "" => vec![],
        source => vec![source.to_owned()],
    }
}

fn default_periodic_tasks() -> Vec<PeriodicTaskArgs> {
    vec![
        PeriodicTaskArgs {
            name: "Daily import and summaries".to_string(),
            source: SOURCE_COMBINED.to_string(),
            sources: vec![SOURCE_CODEX.to_string(), SOURCE_CLAUDE_CODE.to_string()],
            task_kind: PeriodicTaskKind::Regular,
            hour: 0,
            minute: 0,
            interval_hours: None,
            interval_days: Some(1),
            weekly_day: None,
            monthly_day: None,
            lookback_days: 1,
            force_thread_summary: true,
            run_summary_daily: true,
            run_personality: false,
            run_reflection: false,
        },
        PeriodicTaskArgs {
            name: "Weekly summary".to_string(),
            source: SOURCE_COMBINED.to_string(),
            sources: vec![SOURCE_CODEX.to_string(), SOURCE_CLAUDE_CODE.to_string()],
            task_kind: PeriodicTaskKind::Weekly,
            hour: 1,
            minute: 0,
            interval_hours: None,
            interval_days: None,
            weekly_day: Some(1),
            monthly_day: None,
            lookback_days: 7,
            force_thread_summary: true,
            run_summary_daily: true,
            run_personality: false,
            run_reflection: false,
        },
        PeriodicTaskArgs {
            name: "Monthly summary".to_string(),
            source: SOURCE_COMBINED.to_string(),
            sources: vec![SOURCE_CODEX.to_string(), SOURCE_CLAUDE_CODE.to_string()],
            task_kind: PeriodicTaskKind::Monthly,
            hour: 2,
            minute: 0,
            interval_hours: None,
            interval_days: None,
            weekly_day: None,
            monthly_day: Some(1),
            lookback_days: 31,
            force_thread_summary: true,
            run_summary_daily: true,
            run_personality: false,
            run_reflection: false,
        },
    ]
}

fn default_tasks_to_seed(
    defaults: &[PeriodicTaskArgs],
    existing_scheduler_names: impl IntoIterator<Item = String>,
) -> Vec<&PeriodicTaskArgs> {
    let existing = existing_scheduler_names
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    defaults
        .iter()
        .filter(|task| !existing.contains(&scheduler_name(&task.name)))
        .collect()
}

pub fn crontab_for_task(task: &PeriodicTaskArgs) -> AppResult<String> {
    validate_task(task)?;
    let cron = match task.task_kind {
        PeriodicTaskKind::Regular => {
            if let Some(hours) = task.interval_hours {
                let hour_field = hour_field_for_interval(task.hour, hours);
                format!("0 {} {} * * *", task.minute, hour_field)
            } else {
                let days = task.interval_days.expect("validated interval_days");
                format!("0 {} {} */{} * *", task.minute, task.hour, days)
            }
        }
        PeriodicTaskKind::Weekly => {
            let weekday = task.weekly_day.expect("validated weekly_day");
            format!("0 {} {} * * {}", task.minute, task.hour, weekday)
        }
        PeriodicTaskKind::Monthly => {
            let day = task.monthly_day.expect("validated monthly_day");
            format!("0 {} {} {} * *", task.minute, task.hour, day)
        }
    };
    Ok(cron)
}

// `interval_hours` is pre-validated against ALLOWED_INTERVAL_HOURS by the sole
// caller (`crontab_for_task` runs `validate_task` first), so `24 / interval_hours`
// always divides evenly and no allow-list re-check is needed here.
fn hour_field_for_interval(start_hour: u8, interval_hours: u16) -> String {
    let count = 24 / interval_hours;
    let mut hours = (0..count)
        .map(|index| (u16::from(start_hour) + index * interval_hours) % 24)
        .collect::<Vec<_>>();
    hours.sort_unstable();
    hours
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

pub fn scheduler_name(task_name: &str) -> String {
    format!("{LOOKBACK_PERIODIC_PREFIX}{}", task_name.trim())
}

/// Whether a scheduler is Lookback-managed, identified solely by the
/// `lookback-periodic:` name prefix. This is the ownership predicate every
/// periodic command (status / history / cancel and the existing mutations)
/// gates on — conductor's CronSchedulerService is a generic admin API, so a raw
/// scheduler id from the UI must be proven to belong to Lookback before any
/// operation touches it.
pub fn is_lookback_managed(scheduler_name: &str) -> bool {
    scheduler_name.starts_with(LOOKBACK_PERIODIC_PREFIX)
}

/// The Lookback UI display name for a scheduler: the `lookback-periodic:` prefix
/// stripped off. Matches `PeriodicTaskEntry.name` so the status / history cards
/// show the same label as the list. Falls back to the raw name when the prefix
/// is absent (defensive — callers should have already proven ownership).
pub fn display_name(scheduler_name: &str) -> &str {
    scheduler_name
        .strip_prefix(LOOKBACK_PERIODIC_PREFIX)
        .unwrap_or(scheduler_name)
}

/// Find a scheduler by id and prove it is Lookback-managed before any mutation /
/// execution operation. Returns the scheduler's data so callers can reuse its
/// `created_at` / `name` (e.g. update must preserve the original `created_at`,
/// not reset it to 0). Errors when the scheduler is missing or not Lookback's.
pub(super) async fn ensure_lookback_scheduler(
    client: &mut CronSchedulerServiceClient<Channel>,
    id: CronSchedulerId,
) -> AppResult<CronSchedulerData> {
    let data = client
        .find(id)
        .await?
        .into_inner()
        .data
        .and_then(|s| s.data)
        .ok_or_else(|| AppError::Config("scheduler not found".into()))?;
    if !is_lookback_managed(&data.name) {
        return Err(AppError::Config(
            "この実行は Lookback 管理対象ではありません".into(),
        ));
    }
    Ok(data)
}

pub fn wrap_worker_args(task: PeriodicTaskArgs, runtime: PeriodicRuntime) -> AppResult<String> {
    validate_task(&task)?;
    let input = PeriodicWorkflowInput {
        schema_version: SCHEMA_VERSION,
        task,
        runtime,
    };
    let worker_args = ConductorWorkerArgs {
        input: serde_json::to_string(&input)
            .map_err(|e| AppError::Config(format!("serialize periodic input: {e}")))?,
    };
    serde_json::to_string(&worker_args)
        .map_err(|e| AppError::Config(format!("serialize conductor args: {e}")))
}

pub fn unwrap_worker_args(args: &str) -> Option<PeriodicWorkflowInput> {
    let outer: ConductorWorkerArgs = serde_json::from_str(args).ok()?;
    let input: PeriodicWorkflowInput = serde_json::from_str(&outer.input).ok()?;
    (input.schema_version == SCHEMA_VERSION).then_some(input)
}

/// Normalize the output language baked into a runtime: an empty (or
/// blank-only) value becomes `"ja"` so the wrapper never relays `""` to a
/// batch, which would resolve a nonexistent `memories-<feature>-single-`
/// worker. Callers pass an already-resolved value, but this guards the
/// boundary regardless.
fn runtime_output_language(output_language: &str) -> String {
    let trimmed = output_language.trim();
    if trimmed.is_empty() {
        super::DEFAULT_OUTPUT_LANGUAGE.to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn current_runtime(
    memories: &super::connection::MemoriesCallback,
    llm_worker_name: &str,
    output_language: &str,
    memories_import_bin: &str,
) -> AppResult<PeriodicRuntime> {
    Ok(PeriodicRuntime {
        memories_grpc_host: memories.host.clone(),
        memories_grpc_port: memories.port,
        memories_grpc_tls: memories.tls,
        llm_worker_name: llm_worker_name.to_string(),
        output_language: runtime_output_language(output_language),
        memories_import_bin: memories_import_bin.to_string(),
    })
}

/// Runtime for the local sidecar. `refresh_lookback_periodic_runtime` only ever
/// rewrites schedulers at startup with the freshly-bound local memories port, so
/// the endpoint is always plaintext loopback — never the configured remote.
fn local_runtime(
    memories_port: u16,
    llm_worker_name: &str,
    output_language: &str,
    memories_import_bin: &str,
) -> AppResult<PeriodicRuntime> {
    Ok(PeriodicRuntime {
        memories_grpc_host: "127.0.0.1".to_string(),
        memories_grpc_port: memories_port,
        memories_grpc_tls: false,
        llm_worker_name: llm_worker_name.to_string(),
        output_language: runtime_output_language(output_language),
        memories_import_bin: memories_import_bin.to_string(),
    })
}

async fn conductor_channel(url: &str) -> AppResult<Channel> {
    grpc::connect(url).await
}

pub(super) fn parse_scheduler_id(id: &str) -> AppResult<i64> {
    id.parse::<i64>()
        .map_err(|e| AppError::Config(format!("invalid scheduler id: {e}")))
}

async fn cron_client(url: &str) -> AppResult<CronSchedulerServiceClient<Channel>> {
    Ok(CronSchedulerServiceClient::new(
        conductor_channel(url).await?,
    ))
}

async fn ensure_local_jobworkerp_server(
    channel: Channel,
    jobworkerp_port: u16,
) -> AppResult<JobworkerpServerId> {
    let mut client = JobworkerpServerServiceClient::new(channel);
    let data = JobworkerpServerData {
        name: LOOKBACK_JOBWORKERP_SERVER_NAME.to_string(),
        host: "127.0.0.1".to_string(),
        port: jobworkerp_port.to_string(),
        ssl_enabled: false,
        description: Some("Lookback local jobworkerp sidecar".to_string()),
        enabled: true,
        created_at: 0,
        updated_at: 0,
    };
    let found = client
        .find_by_name(FindByNameRequest {
            name: LOOKBACK_JOBWORKERP_SERVER_NAME.to_string(),
        })
        .await?
        .into_inner()
        .data;

    match found {
        Some(existing) => {
            let id = existing
                .id
                .ok_or_else(|| AppError::Config("conductor returned server without id".into()))?;
            client
                .update(JobworkerpServer {
                    id: Some(id),
                    data: Some(JobworkerpServerData {
                        created_at: existing.data.as_ref().map_or(0, |d| d.created_at),
                        ..data
                    }),
                })
                .await?;
            Ok(id)
        }
        None => {
            let id =
                client.create(data).await?.into_inner().id.ok_or_else(|| {
                    AppError::Config("conductor create server returned no id".into())
                })?;
            Ok(id)
        }
    }
}

fn scheduler_to_entry(scheduler: CronScheduler) -> PeriodicTaskEntry {
    let id = scheduler
        .id
        .map(|id| id.value.to_string())
        .unwrap_or_default();
    let Some(data) = scheduler.data else {
        return PeriodicTaskEntry {
            id,
            name: String::new(),
            enabled: false,
            crontab: String::new(),
            description: None,
            task: None,
            status: PeriodicTaskStatus::Unsupported,
        };
    };
    let task = data
        .args
        .as_deref()
        .and_then(unwrap_worker_args)
        .map(|i| i.task);
    let supported = is_lookback_managed(&data.name)
        && task
            .as_ref()
            .is_some_and(|task| validate_task(task).is_ok());
    PeriodicTaskEntry {
        id,
        name: display_name(&data.name).to_string(),
        enabled: data.enabled,
        crontab: data.crontab,
        description: data.description,
        task,
        status: if supported {
            PeriodicTaskStatus::Supported
        } else {
            PeriodicTaskStatus::Unsupported
        },
    }
}

fn scheduler_data(
    task: PeriodicTaskArgs,
    enabled: bool,
    description: Option<String>,
    jobworkerp_server_id: JobworkerpServerId,
    runtime: PeriodicRuntime,
    created_at: i64,
) -> AppResult<CronSchedulerData> {
    let crontab = crontab_for_task(&task)?;
    let args = wrap_worker_args(task.clone(), runtime)?;
    Ok(CronSchedulerData {
        name: scheduler_name(&task.name),
        jobworkerp_server_id: Some(jobworkerp_server_id),
        workflow_url: String::new(),
        channel: None,
        crontab,
        enabled,
        description,
        created_at,
        updated_at: 0,
        args: Some(args),
        execution_target: Some(cron_scheduler_data::ExecutionTarget::Worker(
            WorkerExecution {
                worker_name: LOOKBACK_PERIODIC_WORKER.to_string(),
                r#using: Some("run".to_string()),
            },
        )),
    })
}

fn refreshed_scheduler_data(
    scheduler_id: &CronSchedulerId,
    data: CronSchedulerData,
    jobworkerp_server_id: JobworkerpServerId,
    runtime: PeriodicRuntime,
) -> Option<CronSchedulerData> {
    let input = data.args.as_deref().and_then(unwrap_worker_args)?;
    match scheduler_data(
        input.task,
        data.enabled,
        data.description,
        jobworkerp_server_id,
        runtime,
        data.created_at,
    ) {
        Ok(refreshed) => Some(refreshed),
        Err(e) => {
            warn!(
                scheduler_id = scheduler_id.value,
                scheduler_name = %data.name,
                error = %e,
                "skipping unsupported periodic scheduler during runtime refresh"
            );
            None
        }
    }
}

pub(super) async fn list_schedulers(
    channel: Channel,
    limit: Option<i32>,
    offset: Option<i64>,
) -> AppResult<Vec<CronScheduler>> {
    let mut client = CronSchedulerServiceClient::new(channel);
    let mut stream = client
        .find_list(FindListRequest { limit, offset })
        .await?
        .into_inner();
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item?);
    }
    Ok(out)
}

/// Opens a single conductor Channel, ensures the local jobworkerp server is
/// registered, and resolves the runtime — returning the live Channel so the
/// caller reuses it (tonic Channels clone cheaply and multiplex) instead of
/// opening a second eager connection per command.
async fn current_periodic_context(
    state: &AppState,
    task: &PeriodicTaskArgs,
) -> AppResult<(Channel, JobworkerpServerId, PeriodicRuntime)> {
    let endpoints = state.require_endpoints()?;
    let channel = conductor_channel(&endpoints.conductor_url()).await?;
    let server_id =
        ensure_local_jobworkerp_server(channel.clone(), endpoints.jobworkerp_port).await?;
    let memories = state.resolve_targets()?.memories_callback()?;
    let memories_import_bin = memories_import_bin_for_task(task)?;
    let runtime = current_runtime(
        &memories,
        state.active_llm_worker_name(),
        &state.active_output_language(),
        &memories_import_bin,
    )?;
    Ok((channel, server_id, runtime))
}

pub async fn refresh_lookback_periodic_runtime(
    conductor_url: &str,
    jobworkerp_port: u16,
    memories_port: u16,
    llm_worker_name: &str,
    output_language: &str,
    memories_import_bin: &Path,
    seed_marker_path: &Path,
) -> AppResult<()> {
    let channel = conductor_channel(conductor_url).await?;
    let server_id = ensure_local_jobworkerp_server(channel.clone(), jobworkerp_port).await?;
    let memories_import_bin = memories_import_bin.to_string_lossy().into_owned();
    let runtime = local_runtime(
        memories_port,
        llm_worker_name,
        output_language,
        &memories_import_bin,
    )?;
    let schedulers = list_schedulers(channel.clone(), None, None).await?;
    let mut client = CronSchedulerServiceClient::new(channel);
    for scheduler in &schedulers {
        let Some(id) = scheduler.id else {
            continue;
        };
        let Some(data) = scheduler.data.clone() else {
            continue;
        };
        if !data.name.starts_with(LOOKBACK_PERIODIC_PREFIX) {
            continue;
        }
        let Some(refreshed) = refreshed_scheduler_data(&id, data, server_id, runtime.clone())
        else {
            continue;
        };
        client
            .update(CronScheduler {
                id: Some(id),
                data: Some(refreshed),
            })
            .await?;
    }
    seed_default_periodic_tasks(
        &mut client,
        &schedulers,
        server_id,
        runtime,
        seed_marker_path,
    )
    .await?;
    Ok(())
}

async fn seed_default_periodic_tasks(
    client: &mut CronSchedulerServiceClient<Channel>,
    schedulers: &[CronScheduler],
    jobworkerp_server_id: JobworkerpServerId,
    runtime: PeriodicRuntime,
    seed_marker_path: &Path,
) -> AppResult<()> {
    if seed_marker_path.exists() {
        return Ok(());
    }
    let existing_names = schedulers.iter().filter_map(|scheduler| {
        scheduler
            .data
            .as_ref()
            .map(|data| data.name.clone())
            .filter(|name| is_lookback_managed(name))
    });
    for task in default_tasks_to_seed(&default_periodic_tasks(), existing_names) {
        let data = scheduler_data(
            task.clone(),
            false,
            Some("Seeded disabled default schedule".to_string()),
            jobworkerp_server_id,
            runtime.clone(),
            0,
        )?;
        client.create(data).await?;
    }
    let marker = PeriodicDefaultsSeedMarker {
        version: DEFAULT_PERIODIC_SEED_VERSION,
    };
    if let Some(parent) = seed_marker_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        seed_marker_path,
        serde_json::to_vec_pretty(&marker)
            .map_err(|e| AppError::Config(format!("serialize periodic seed marker: {e}")))?,
    )?;
    Ok(())
}

#[tauri::command]
pub async fn list_periodic_tasks(
    state: tauri::State<'_, AppState>,
    req: ListPeriodicTasksRequest,
) -> AppResult<Vec<PeriodicTaskEntry>> {
    let endpoints = state.require_endpoints()?;
    let schedulers = list_schedulers(
        conductor_channel(&endpoints.conductor_url()).await?,
        req.limit,
        req.offset,
    )
    .await?;
    Ok(schedulers
        .into_iter()
        .filter(|s| {
            s.data
                .as_ref()
                .is_some_and(|d| d.name.starts_with(LOOKBACK_PERIODIC_PREFIX))
        })
        .map(scheduler_to_entry)
        .collect())
}

#[tauri::command]
pub async fn create_periodic_task(
    state: tauri::State<'_, AppState>,
    req: SavePeriodicTaskRequest,
) -> AppResult<String> {
    let (channel, server_id, runtime) = current_periodic_context(&state, &req.task).await?;
    let mut client = CronSchedulerServiceClient::new(channel);
    let data = scheduler_data(
        req.task,
        req.enabled,
        req.description,
        server_id,
        runtime,
        0,
    )?;
    let id = client
        .create(data)
        .await?
        .into_inner()
        .id
        .ok_or_else(|| AppError::Config("conductor create scheduler returned no id".into()))?;
    Ok(id.value.to_string())
}

#[tauri::command]
pub async fn update_periodic_task(
    state: tauri::State<'_, AppState>,
    req: SavePeriodicTaskRequest,
) -> AppResult<()> {
    let id_value = parse_scheduler_id(
        req.id
            .as_deref()
            .ok_or_else(|| AppError::Config("id is required for update".into()))?,
    )?;
    let (channel, server_id, runtime) = current_periodic_context(&state, &req.task).await?;
    let mut client = CronSchedulerServiceClient::new(channel);
    let id = CronSchedulerId { value: id_value };
    // Prove ownership AND read the original created_at in one Find: a missing or
    // non-Lookback scheduler must error here rather than silently proceeding to
    // create-via-update with created_at=0.
    let existing = ensure_lookback_scheduler(&mut client, id).await?.created_at;
    let data = scheduler_data(
        req.task,
        req.enabled,
        req.description,
        server_id,
        runtime,
        existing,
    )?;
    client
        .update(CronScheduler {
            id: Some(id),
            data: Some(data),
        })
        .await?;
    Ok(())
}

#[tauri::command]
pub async fn delete_periodic_task(
    state: tauri::State<'_, AppState>,
    req: PeriodicTaskIdRequest,
) -> AppResult<()> {
    let id = parse_scheduler_id(&req.id)?;
    let endpoints = state.require_endpoints()?;
    let mut client = cron_client(&endpoints.conductor_url()).await?;
    let id = CronSchedulerId { value: id };
    // Reject a missing or non-Lookback scheduler before deleting.
    ensure_lookback_scheduler(&mut client, id).await?;
    client.delete(id).await?;
    Ok(())
}

#[tauri::command]
pub async fn set_enabled_periodic_task(
    state: tauri::State<'_, AppState>,
    req: SetPeriodicTaskEnabledRequest,
) -> AppResult<()> {
    let id_value = parse_scheduler_id(&req.id)?;
    let endpoints = state.require_endpoints()?;
    let mut client = cron_client(&endpoints.conductor_url()).await?;
    let id = CronSchedulerId { value: id_value };
    // Reject a missing or non-Lookback scheduler before toggling.
    let mut data = ensure_lookback_scheduler(&mut client, id).await?;
    data.enabled = req.enabled;
    client
        .update(CronScheduler {
            id: Some(id),
            data: Some(data),
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(kind: PeriodicTaskKind) -> PeriodicTaskArgs {
        PeriodicTaskArgs {
            name: "朝の要約".into(),
            source: "codex".into(),
            sources: vec!["codex".into()],
            task_kind: kind,
            hour: 9,
            minute: 30,
            interval_hours: None,
            interval_days: None,
            weekly_day: None,
            monthly_day: None,
            lookback_days: 7,
            force_thread_summary: true,
            run_summary_daily: true,
            run_personality: false,
            run_reflection: false,
        }
    }

    fn runtime(port: u16) -> PeriodicRuntime {
        PeriodicRuntime {
            memories_grpc_host: "127.0.0.1".into(),
            memories_grpc_port: port,
            memories_grpc_tls: false,
            llm_worker_name: "memories-llm".into(),
            output_language: "ja".into(),
            memories_import_bin: "/bin/memories-import".into(),
        }
    }

    fn wrapped_args_without_validation(task: PeriodicTaskArgs, runtime: PeriodicRuntime) -> String {
        let input = PeriodicWorkflowInput {
            schema_version: SCHEMA_VERSION,
            task,
            runtime,
        };
        let outer = ConductorWorkerArgs {
            input: serde_json::to_string(&input).unwrap(),
        };
        serde_json::to_string(&outer).unwrap()
    }

    #[test]
    fn scheduler_data_targets_periodic_worker_with_wrapped_args() {
        let mut t = task(PeriodicTaskKind::Weekly);
        t.weekly_day = Some(1);
        let data = scheduler_data(
            t.clone(),
            true,
            Some("desc".into()),
            JobworkerpServerId { value: 42 },
            runtime(9010),
            123,
        )
        .unwrap();

        assert_eq!(data.name, "lookback-periodic:朝の要約");
        assert_eq!(data.crontab, "0 30 9 * * 1");
        assert_eq!(data.jobworkerp_server_id.unwrap().value, 42);
        assert_eq!(data.created_at, 123);
        assert_eq!(
            unwrap_worker_args(data.args.as_deref().unwrap())
                .unwrap()
                .task,
            t
        );
        match data.execution_target.unwrap() {
            cron_scheduler_data::ExecutionTarget::Worker(worker) => {
                assert_eq!(worker.worker_name, LOOKBACK_PERIODIC_WORKER);
                assert_eq!(worker.r#using.as_deref(), Some("run"));
            }
            other => panic!("unexpected target: {other:?}"),
        }
    }

    #[test]
    fn scheduler_data_wire_args_include_output_language() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        let data = scheduler_data(
            t,
            true,
            None,
            JobworkerpServerId { value: 42 },
            runtime(9010),
            0,
        )
        .unwrap();

        let outer: ConductorWorkerArgs =
            serde_json::from_str(data.args.as_deref().unwrap()).unwrap();
        let input: serde_json::Value = serde_json::from_str(&outer.input).unwrap();
        // The wrapper forwards `output_language` into each batch (which resolves
        // its language-specific single worker by name); the old per-feature
        // `*-single.yaml` path relay is gone.
        assert_eq!(input["runtime"]["output_language"], "ja");
        assert!(input["runtime"].get("workflow_paths").is_none());
    }

    #[test]
    fn runtime_constructors_normalize_blank_output_language_to_ja() {
        // The resolved value is never blank, but an old persisted scheduler can
        // carry "" — the wrapper must never relay it to a batch (it would build
        // a nonexistent `memories-<feature>-single-` worker name).
        let cb = super::super::connection::MemoriesCallback {
            host: "127.0.0.1".into(),
            port: 9010,
            tls: false,
        };
        let import_bin = "/bin/memories-import";
        assert_eq!(
            current_runtime(&cb, "memories-llm", "", import_bin)
                .unwrap()
                .output_language,
            "ja"
        );
        assert_eq!(
            current_runtime(&cb, "memories-llm", "  ", import_bin)
                .unwrap()
                .output_language,
            "ja"
        );
        assert_eq!(
            current_runtime(&cb, "memories-llm", "en", import_bin)
                .unwrap()
                .output_language,
            "en"
        );
        assert_eq!(
            local_runtime(9010, "memories-llm", "", import_bin)
                .unwrap()
                .output_language,
            "ja"
        );
        assert_eq!(
            local_runtime(9010, "memories-llm", "ja", import_bin)
                .unwrap()
                .memories_import_bin,
            "/bin/memories-import"
        );
    }

    #[test]
    fn importer_resolution_is_required_only_for_regular_tasks() {
        let mut regular = task(PeriodicTaskKind::Regular);
        regular.interval_days = Some(1);
        let weekly = {
            let mut t = task(PeriodicTaskKind::Weekly);
            t.weekly_day = Some(1);
            t
        };
        let missing = || AppError::Config("missing importer".to_string());

        assert!(memories_import_bin_for_task_result(&regular, Err(missing())).is_err());
        assert_eq!(
            memories_import_bin_for_task_result(&weekly, Err(missing())).unwrap(),
            "memories-import"
        );
        assert_eq!(
            memories_import_bin_for_task_result(
                &weekly,
                Ok(std::path::PathBuf::from("/bin/memories-import")),
            )
            .unwrap(),
            "/bin/memories-import"
        );
    }

    #[test]
    fn periodic_runtime_deserializes_missing_output_language_to_ja() {
        // A scheduler persisted before this field existed must load with the
        // field defaulting to "ja", not "" — the latter dangles against an
        // unregistered worker.
        let json = r#"{
            "memories_grpc_host": "127.0.0.1",
            "memories_grpc_port": 9010,
            "memories_grpc_tls": false,
            "llm_worker_name": "memories-llm"
        }"#;
        let rt: PeriodicRuntime = serde_json::from_str(json).unwrap();
        assert_eq!(rt.output_language, "ja");
        assert_eq!(rt.memories_import_bin, "memories-import");
    }

    #[test]
    fn scheduler_data_omits_null_optional_task_fields_for_workflow_schema() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        let data = scheduler_data(
            t,
            true,
            None,
            JobworkerpServerId { value: 42 },
            runtime(9010),
            0,
        )
        .unwrap();

        let outer: ConductorWorkerArgs =
            serde_json::from_str(data.args.as_deref().unwrap()).unwrap();
        let input: serde_json::Value = serde_json::from_str(&outer.input).unwrap();
        let task = &input["task"];
        assert_eq!(task["interval_hours"], 24);
        assert!(task.get("interval_days").is_none());
        assert!(task.get("weekly_day").is_none());
        assert!(task.get("monthly_day").is_none());
    }

    #[test]
    fn scheduler_data_preserves_regular_generation_flags() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        t.force_thread_summary = false;
        t.run_summary_daily = false;
        t.run_personality = true;
        t.run_reflection = true;
        let data = scheduler_data(
            t,
            true,
            None,
            JobworkerpServerId { value: 42 },
            runtime(9010),
            0,
        )
        .unwrap();

        let outer: ConductorWorkerArgs =
            serde_json::from_str(data.args.as_deref().unwrap()).unwrap();
        let input: serde_json::Value = serde_json::from_str(&outer.input).unwrap();
        let task = &input["task"];
        assert_eq!(task["force_thread_summary"], false);
        assert_eq!(task["run_summary_daily"], false);
        assert_eq!(task["run_personality"], true);
        assert_eq!(task["run_reflection"], true);
    }

    #[test]
    fn wrap_worker_args_preserves_combined_sources() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.source = "codex+claude-code".into();
        t.sources = vec!["codex".into(), "claude-code".into()];
        t.interval_hours = Some(24);

        let args = wrap_worker_args(t.clone(), runtime(9010)).unwrap();
        let outer: ConductorWorkerArgs = serde_json::from_str(&args).unwrap();
        let input: serde_json::Value = serde_json::from_str(&outer.input).unwrap();

        assert_eq!(
            effective_sources(&t),
            vec!["codex".to_string(), "claude-code".to_string()]
        );
        assert_eq!(input["task"]["source"], "codex+claude-code");
        assert_eq!(
            input["task"]["sources"],
            serde_json::json!(["codex", "claude-code"])
        );
    }

    #[test]
    fn periodic_wrapper_uses_label_any_for_combined_sources() {
        let yaml = include_str!("../../../workers/workflows/lookback-periodic-run.yaml");

        assert!(yaml.contains("source_filter_match_mode"));
        assert!(yaml.contains("- buildRegularImportArgs:"));
        assert!(yaml.contains("- runImportCodex:"));
        assert!(yaml.contains(r#"index(\"codex\")"#));
        assert!(yaml.contains("- runImportClaudeCode:"));
        assert!(yaml.contains(r#"index(\"claude-code\")"#));
        assert!(yaml.contains("memories_import_bin"));
        assert!(yaml.contains("--all-sessions"));
        assert!(yaml.contains("--all-projects"));
        assert!(yaml.contains("--since"));
        assert!(yaml.contains("--server-url"));
        assert!(yaml.contains("runnerName: COMMAND"));
        assert!(yaml.contains("treat_nonzero_as_error: true"));
        assert!(yaml.contains("interval_hours: { type: [integer, \"null\"], minimum: 1 }"));
        assert!(yaml.contains("interval_days: { type: [integer, \"null\"], minimum: 1 }"));
        assert!(yaml.contains("elif $workflow.input.task.source == \"codex+claude-code\""));
        assert!(yaml.contains("then [\"codex\", \"claude-code\"]"));
        assert!(yaml.contains("- resolveSourceFilterLabels:"));
        assert!(yaml.contains("source_filter_labels: >-"));
        assert!(yaml.contains(r#"elif . == "codex" then "agent:codex""#));
        assert!(yaml.contains(r#"elif . == "claude-code" then "agent:claude_code""#));
        assert!(yaml.contains("labels_filter_match_mode: $source_filter_match_mode"));
        assert!(yaml.contains("extra_labels_match_mode: $source_filter_match_mode"));
        assert!(yaml.contains("($workflow.input.task.run_summary_daily // true)"));
        assert!(yaml.contains("($workflow.input.task.run_personality // false)"));
        assert!(yaml.contains("($workflow.input.task.run_reflection // false)"));
        assert!(yaml.contains("workerName: memories-personality-batch"));
        assert!(yaml.contains("workerName: memories-reflection-batch"));
        assert!(
            !yaml.contains("then []"),
            "combined source must not become an empty all-sources label filter"
        );
        let source_index = yaml.find("source_filter_sources: >-").unwrap();
        let labels_step_index = yaml.find("- resolveSourceFilterLabels:").unwrap();
        let labels_index = yaml.find("source_filter_labels: >-").unwrap();
        assert!(
            source_index < labels_step_index && labels_step_index < labels_index,
            "workflow set keys are evaluated in parallel; source_filter_sources must be resolved in an earlier step"
        );
    }

    #[test]
    fn default_periodic_tasks_match_requested_disabled_templates() {
        let defaults = default_periodic_tasks();
        assert_eq!(defaults.len(), 3);

        let daily = &defaults[0];
        assert_eq!(daily.name, "Daily import and summaries");
        assert_eq!(daily.source, SOURCE_COMBINED);
        assert_eq!(
            daily.sources,
            vec![SOURCE_CODEX.to_string(), SOURCE_CLAUDE_CODE.to_string()]
        );
        assert_eq!(daily.task_kind, PeriodicTaskKind::Regular);
        assert_eq!(daily.interval_days, Some(1));
        assert_eq!(daily.hour, 0);
        assert_eq!(daily.minute, 0);
        assert_eq!(daily.lookback_days, 1);
        assert!(daily.force_thread_summary);
        assert!(daily.run_summary_daily);
        assert!(!daily.run_personality);
        assert!(!daily.run_reflection);
        assert_eq!(crontab_for_task(daily).unwrap(), "0 0 0 */1 * *");

        let weekly = &defaults[1];
        assert_eq!(weekly.name, "Weekly summary");
        assert_eq!(weekly.task_kind, PeriodicTaskKind::Weekly);
        assert_eq!(weekly.weekly_day, Some(1));
        assert_eq!(weekly.hour, 1);
        assert_eq!(crontab_for_task(weekly).unwrap(), "0 0 1 * * 1");

        let monthly = &defaults[2];
        assert_eq!(monthly.name, "Monthly summary");
        assert_eq!(monthly.task_kind, PeriodicTaskKind::Monthly);
        assert_eq!(monthly.monthly_day, Some(1));
        assert_eq!(monthly.hour, 2);
        assert_eq!(crontab_for_task(monthly).unwrap(), "0 0 2 1 * *");
    }

    #[test]
    fn default_tasks_to_seed_skips_existing_scheduler_names() {
        let defaults = default_periodic_tasks();
        let to_seed = default_tasks_to_seed(
            &defaults,
            vec![scheduler_name("Daily import and summaries")],
        );

        assert_eq!(
            to_seed
                .iter()
                .map(|task| task.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Weekly summary", "Monthly summary"]
        );
    }

    #[test]
    fn period_rollups_keep_scope_for_single_and_combined_sources() {
        let weekly = include_str!(
            "../../../workers/lang-workers/workers/weekly-work-summary/weekly-work-summary-single.yaml"
        );
        let monthly = include_str!(
            "../../../workers/lang-workers/workers/monthly-work-summary/monthly-work-summary-single.yaml"
        );

        assert!(weekly.contains("${ [$workflow.input.daily_label, \"scope:\" + $scope_key] }"));
        assert!(weekly.contains("daily_thread_filter_match_mode"));
        assert!(
            !weekly.contains("[$workflow.input.daily_label] + $sorted_extra_labels"),
            "weekly input must not match combined daily summaries via source labels"
        );
        assert!(
            !weekly.contains("then $sorted_extra_labels"),
            "weekly combined input must not include single-source daily summaries"
        );
        assert!(monthly.contains("${ [$workflow.input.weekly_label, \"scope:\" + $scope_key] }"));
        assert!(monthly.contains("weekly_thread_filter_match_mode"));
        assert!(
            !monthly.contains("[$workflow.input.weekly_label] + $sorted_extra_labels"),
            "monthly input must not match combined weekly summaries via source labels"
        );
        assert!(
            !monthly.contains("then $sorted_extra_labels"),
            "monthly combined input must not include single-source weekly summaries"
        );
    }

    #[test]
    fn is_lookback_managed_only_accepts_prefixed_names() {
        assert!(is_lookback_managed("lookback-periodic:朝の要約"));
        assert!(is_lookback_managed(&scheduler_name("x")));
        assert!(!is_lookback_managed("some-other-scheduler"));
        assert!(!is_lookback_managed(""));
        // A bare prefix substring elsewhere must not match.
        assert!(!is_lookback_managed("prefixed lookback-periodic:"));
    }

    #[test]
    fn display_name_strips_prefix_and_passes_through_unprefixed() {
        assert_eq!(display_name("lookback-periodic:朝の要約"), "朝の要約");
        assert_eq!(display_name(&scheduler_name("週次")), "週次");
        // Defensive fallback: an unprefixed name is returned unchanged.
        assert_eq!(display_name("raw-name"), "raw-name");
    }

    #[test]
    fn scheduler_to_entry_keeps_unsupported_prefixed_rows_visible() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        let legacy_args = serde_json::to_string(&t).unwrap();
        let entry = scheduler_to_entry(CronScheduler {
            id: Some(CronSchedulerId { value: 7 }),
            data: Some(CronSchedulerData {
                name: scheduler_name("legacy"),
                crontab: "0 0 9 * * *".into(),
                enabled: true,
                args: Some(legacy_args),
                ..Default::default()
            }),
        });

        assert_eq!(entry.id, "7");
        assert_eq!(entry.name, "legacy");
        assert_eq!(entry.status, PeriodicTaskStatus::Unsupported);
        assert!(entry.task.is_none());
    }

    #[test]
    fn scheduler_to_entry_marks_wrapped_plain_scheduler_unsupported() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        t.source = "plain".into();
        t.sources = vec!["plain".into()];
        let entry = scheduler_to_entry(CronScheduler {
            id: Some(CronSchedulerId { value: 8 }),
            data: Some(CronSchedulerData {
                name: scheduler_name("plain legacy"),
                crontab: "0 0 9 * * *".into(),
                enabled: true,
                args: Some(wrapped_args_without_validation(t, runtime(9010))),
                ..Default::default()
            }),
        });

        assert_eq!(entry.id, "8");
        assert_eq!(entry.name, "plain legacy");
        assert_eq!(entry.status, PeriodicTaskStatus::Unsupported);
    }

    #[test]
    fn refreshed_scheduler_data_skips_unsupported_plain_but_keeps_valid_tasks() {
        let mut plain = task(PeriodicTaskKind::Regular);
        plain.interval_hours = Some(24);
        plain.source = "plain".into();
        plain.sources = vec!["plain".into()];
        let skipped = refreshed_scheduler_data(
            &CronSchedulerId { value: 8 },
            CronSchedulerData {
                name: scheduler_name("plain legacy"),
                crontab: "0 0 9 * * *".into(),
                enabled: true,
                args: Some(wrapped_args_without_validation(plain, runtime(9010))),
                created_at: 123,
                ..Default::default()
            },
            JobworkerpServerId { value: 42 },
            runtime(9020),
        );
        assert!(skipped.is_none());

        let mut valid = task(PeriodicTaskKind::Regular);
        valid.interval_hours = Some(24);
        let refreshed = refreshed_scheduler_data(
            &CronSchedulerId { value: 9 },
            CronSchedulerData {
                name: scheduler_name("valid"),
                crontab: "0 0 9 * * *".into(),
                enabled: true,
                args: Some(wrap_worker_args(valid, runtime(9010)).unwrap()),
                created_at: 456,
                ..Default::default()
            },
            JobworkerpServerId { value: 99 },
            runtime(9020),
        )
        .unwrap();

        assert_eq!(refreshed.jobworkerp_server_id.unwrap().value, 99);
        assert_eq!(
            unwrap_worker_args(refreshed.args.as_deref().unwrap())
                .unwrap()
                .runtime
                .memories_grpc_port,
            9020
        );
        assert_eq!(refreshed.created_at, 456);
    }

    #[test]
    fn crontab_regular_hours_uses_baseline_hour_list() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(6);
        assert_eq!(crontab_for_task(&t).unwrap(), "0 30 3,9,15,21 * * *");

        t.interval_hours = Some(8);
        assert_eq!(crontab_for_task(&t).unwrap(), "0 30 1,9,17 * * *");

        t.interval_hours = Some(24);
        assert_eq!(crontab_for_task(&t).unwrap(), "0 30 9 * * *");
    }

    #[test]
    fn crontab_regular_days_uses_hour_and_six_fields() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_days = Some(3);
        assert_eq!(crontab_for_task(&t).unwrap(), "0 30 9 */3 * *");
    }

    #[test]
    fn crontab_weekly_and_monthly_boundaries() {
        let mut weekly = task(PeriodicTaskKind::Weekly);
        weekly.minute = 0;
        weekly.weekly_day = Some(1);
        assert_eq!(crontab_for_task(&weekly).unwrap(), "0 0 9 * * 1");

        let mut monthly = task(PeriodicTaskKind::Monthly);
        monthly.monthly_day = Some(28);
        assert_eq!(crontab_for_task(&monthly).unwrap(), "0 30 9 28 * *");

        monthly.monthly_day = Some(29);
        assert!(crontab_for_task(&monthly).is_err());
    }

    #[test]
    fn validation_rejects_missing_source_and_zero_lookback() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        t.source.clear();
        t.sources.clear();
        assert!(validate_task(&t).is_err());

        t.source = "codex".into();
        t.lookback_days = 0;
        assert!(validate_task(&t).is_err());
    }

    #[test]
    fn validation_rejects_plain_source_for_periodic_ui_contract() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        t.source = "plain".into();
        t.sources = vec!["plain".into()];

        assert!(validate_task(&t).is_err());
    }

    #[test]
    fn validation_rejects_unsupported_hour_interval() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(5);

        assert!(validate_task(&t).is_err());
    }

    #[test]
    fn validation_rejects_daily_without_thread_summary() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        t.force_thread_summary = false;
        t.run_summary_daily = true;

        assert!(validate_task(&t).is_err());
    }

    #[test]
    fn missing_generation_flags_default_to_running_summary_stages() {
        // A row missing the generation flags (legacy scheduler / hand-built
        // input) must deserialize to the basic summary stages enabled, not to a
        // no-op false. Mirrors the YAML schema defaults and the `// true`
        // fallbacks in the periodic-run stage `if:` guards.
        let json = serde_json::json!({
            "name": "朝の要約",
            "source": "codex",
            "task_kind": "regular",
            "hour": 9,
            "minute": 0,
            "interval_hours": 24,
            "lookback_days": 7
        })
        .to_string();

        let task: PeriodicTaskArgs = serde_json::from_str(&json).unwrap();
        assert!(task.force_thread_summary);
        assert!(task.run_summary_daily);
        assert!(!task.run_personality);
        assert!(!task.run_reflection);
        // The thread/daily pair must satisfy the daily-requires-thread rule.
        assert!(validate_task(&task).is_ok());
    }

    #[test]
    fn wrap_and_unwrap_preserves_task_and_runtime() {
        let mut t = task(PeriodicTaskKind::Weekly);
        t.weekly_day = Some(1);
        let args = wrap_worker_args(t.clone(), runtime(9010)).unwrap();
        let input = unwrap_worker_args(&args).unwrap();
        assert_eq!(input.task, t);
        assert_eq!(input.runtime.memories_grpc_port, 9010);
        // The wrapper forwards `output_language` into each batch, so it must
        // survive the wrap/unwrap round-trip.
        assert_eq!(
            input.runtime.output_language,
            runtime(9010).output_language,
            "output_language must survive wrap/unwrap"
        );
    }

    #[test]
    fn unwrap_rejects_legacy_direct_task_and_unknown_schema() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        let legacy = serde_json::to_string(&t).unwrap();
        assert!(unwrap_worker_args(&legacy).is_none());

        let mut input = PeriodicWorkflowInput {
            schema_version: 99,
            task: t,
            runtime: runtime(9010),
        };
        let outer = ConductorWorkerArgs {
            input: serde_json::to_string(&input).unwrap(),
        };
        assert!(unwrap_worker_args(&serde_json::to_string(&outer).unwrap()).is_none());

        input.schema_version = 1;
        assert!(unwrap_worker_args("not json").is_none());
    }
}
