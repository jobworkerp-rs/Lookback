//! Local periodic task management backed by conductor's CronSchedulerService.

use std::{collections::HashSet, path::Path};

use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt as _;
use tonic::transport::Channel;
use tracing::warn;

use crate::error::{AppError, AppResult};
use crate::error_messages::{
    CANCEL_NOT_MANAGED, VALIDATION_NAME_REQUIRED, VALIDATION_SOURCE_DUPLICATE,
    VALIDATION_SOURCE_INVALID, VALIDATION_SOURCE_MISMATCH, VALIDATION_SOURCE_REQUIRED,
};
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
const PERIODIC_JOB_TIMEOUT_SEC: u32 = {
    assert!(crate::jobworkerp::job_timeouts::PERIODIC_RUN <= u32::MAX as u64);
    crate::jobworkerp::job_timeouts::PERIODIC_RUN as u32
};

// Source vocabulary shared as a wire contract with the frontend
// (`src/lib/periodicTask.ts`) and the periodic-run workflow YAML. Keep the three
// layers in sync; the YAML cannot import these so a workflow test asserts the
// literal match.
const SOURCE_CODEX: &str = "codex";
const SOURCE_CLAUDE_CODE: &str = "claude-code";
const SOURCE_OPENCODE: &str = "opencode";
const SOURCE_COMBINED: &str = "codex+claude-code";
const ALLOWED_INTERVAL_HOURS: &[u16] = &[1, 2, 3, 4, 6, 8, 12, 24];
const INTERVAL_HOURS_ERROR: &str = "interval_hours must be one of 1,2,3,4,6,8,12,24";

fn default_true() -> bool {
    true
}

fn default_memories_import_bin() -> String {
    "memories-import".to_string()
}

fn default_llm_context_window_tokens() -> u32 {
    131_072
}

fn default_summary_max_completion_tokens() -> u32 {
    32_768
}

fn default_summary_input_byte_budget() -> u32 {
    200_000
}

fn default_summary_output_guidance() -> serde_json::Value {
    super::import::summary_output_guidance_for_context(
        &super::llm_settings::SummaryOutputGuidance::default(),
        default_llm_context_window_tokens(),
    )
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    /// Whether the periodic run aggregates the current active primary groups.
    /// Missing values are resolved by the custom deserializer for legacy rows:
    /// regular tasks follow `force_thread_summary`, while weekly/monthly tasks
    /// remain opted out.
    pub run_summary_thread_group: bool,
    #[serde(default = "default_true")]
    pub run_summary_daily: bool,
    #[serde(default)]
    pub run_personality: bool,
    #[serde(default)]
    pub run_reflection: bool,
    /// Preserve best-effort periodic execution unless the user explicitly
    /// asks the path fan-out to stop before producing a partial overall row.
    #[serde(default)]
    pub stop_on_error: bool,
}

#[derive(Debug, Deserialize)]
struct PeriodicTaskArgsWire {
    name: String,
    source: String,
    #[serde(default)]
    sources: Vec<String>,
    task_kind: PeriodicTaskKind,
    hour: u8,
    minute: u8,
    #[serde(default)]
    interval_hours: Option<u16>,
    #[serde(default)]
    interval_days: Option<u16>,
    #[serde(default)]
    weekly_day: Option<u8>,
    #[serde(default)]
    monthly_day: Option<u8>,
    lookback_days: u16,
    #[serde(default = "default_true")]
    force_thread_summary: bool,
    #[serde(default)]
    run_summary_thread_group: Option<bool>,
    #[serde(default = "default_true")]
    run_summary_daily: bool,
    #[serde(default)]
    run_personality: bool,
    #[serde(default)]
    run_reflection: bool,
    #[serde(default)]
    stop_on_error: bool,
}

impl<'de> Deserialize<'de> for PeriodicTaskArgs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = PeriodicTaskArgsWire::deserialize(deserializer)?;
        let run_summary_thread_group = wire
            .run_summary_thread_group
            .unwrap_or(wire.task_kind == PeriodicTaskKind::Regular && wire.force_thread_summary);
        Ok(Self {
            name: wire.name,
            source: wire.source,
            sources: wire.sources,
            task_kind: wire.task_kind,
            hour: wire.hour,
            minute: wire.minute,
            interval_hours: wire.interval_hours,
            interval_days: wire.interval_days,
            weekly_day: wire.weekly_day,
            monthly_day: wire.monthly_day,
            lookback_days: wire.lookback_days,
            force_thread_summary: wire.force_thread_summary,
            run_summary_thread_group,
            run_summary_daily: wire.run_summary_daily,
            run_personality: wire.run_personality,
            run_reflection: wire.run_reflection,
            stop_on_error: wire.stop_on_error,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeriodicRuntime {
    pub memories_grpc_host: String,
    pub memories_grpc_port: u16,
    pub memories_grpc_tls: bool,
    pub llm_worker_name: String,
    /// Snapshot of the model-dependent period-summary contract. Keep defaults
    /// for schedulers persisted before these fields existed; startup refresh
    /// rewrites managed schedulers with the active model's actual values.
    #[serde(default = "default_llm_context_window_tokens")]
    pub llm_context_window_tokens: u32,
    #[serde(default)]
    pub llm_expected_model: String,
    #[serde(default = "default_summary_max_completion_tokens")]
    pub summary_max_completion_tokens: u32,
    #[serde(default = "default_summary_input_byte_budget")]
    pub summary_input_byte_budget: u32,
    /// Prompt-only writing targets resolved from the active settings and the
    /// runtime's effective context contract. Persist this with the scheduler
    /// so conductor dispatches match manual period-summary generation.
    #[serde(default = "default_summary_output_guidance")]
    pub summary_output_guidance: serde_json::Value,
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
    /// Auto-issue policy snapshot JSON forwarded into thread-summary-batch
    /// (and then extract). Empty on older persisted schedulers until refresh.
    #[serde(default)]
    pub policy_json: String,
    /// Absolute cwd snapshot used for regular importer commands.
    #[serde(default)]
    pub import_working_dir: String,
    /// Resolved OpenCode database path captured with the runtime.
    #[serde(default)]
    pub opencode_db_path: String,
    /// The Issue tracker as jobworkerp reaches it; empty while Issue
    /// tracking is off.
    #[serde(default)]
    pub issue_tracker_grpc_host: String,
    #[serde(default)]
    pub issue_tracker_grpc_port: u16,
    #[serde(default)]
    pub issue_tracker_grpc_tls: bool,
}

/// Records the conductors that already received the default schedulers, so
/// switching to another conductor seeds it too while a user's deletions on a
/// seeded one stay deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PeriodicDefaultsSeedMarker {
    version: u32,
    /// Markers written before this field existed seeded the local conductor.
    #[serde(default = "seeded_local_conductor")]
    conductors: Vec<String>,
}

fn seeded_local_conductor() -> Vec<String> {
    vec![super::connection::LOCAL_CONDUCTOR_IDENTITY.to_string()]
}

fn read_seed_marker(path: &Path) -> PeriodicDefaultsSeedMarker {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(PeriodicDefaultsSeedMarker {
            version: DEFAULT_PERIODIC_SEED_VERSION,
            conductors: Vec::new(),
        })
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
        return Err(AppError::Config(VALIDATION_NAME_REQUIRED.into()));
    }
    let sources = effective_sources(task);
    if sources.is_empty() {
        return Err(AppError::Config(VALIDATION_SOURCE_REQUIRED.into()));
    }
    // `source` is a display/legacy column derived from the canonical `sources`
    // list: singular value for one selection, "+"-joined for multiple. The
    // legacy combined token ("codex+claude-code") stays valid so schedulers
    // persisted before multi-source selections remain loadable.
    let joined = sources.join("+");
    let source_matches = task.source.trim() == joined
        || (sources.len() == 2
            && sources[0] == SOURCE_CODEX
            && sources[1] == SOURCE_CLAUDE_CODE
            && task.source.trim() == SOURCE_COMBINED);
    if !source_matches {
        return Err(AppError::Config(VALIDATION_SOURCE_MISMATCH.into()));
    }
    let mut seen = HashSet::new();
    for source in &sources {
        if !matches!(
            source.as_str(),
            SOURCE_CODEX | SOURCE_CLAUDE_CODE | SOURCE_OPENCODE
        ) {
            return Err(AppError::Config(VALIDATION_SOURCE_INVALID.into()));
        }
        if !seen.insert(source.as_str()) {
            return Err(AppError::Config(VALIDATION_SOURCE_DUPLICATE.into()));
        }
    }
    if task.hour > 23 {
        return Err(AppError::Config("hour must be 0..23".into()));
    }
    if task.minute > 59 {
        return Err(AppError::Config("minute must be 0..59".into()));
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
    if task.run_summary_thread_group && !task.force_thread_summary {
        return Err(AppError::Config(
            "run_summary_thread_group requires force_thread_summary".into(),
        ));
    }
    Ok(())
}

pub fn effective_sources(task: &PeriodicTaskArgs) -> Vec<String> {
    let mut sources = if !task.sources.is_empty() {
        task.sources
            .iter()
            .filter_map(|source| {
                let source = source.trim();
                (!source.is_empty()).then(|| source.to_owned())
            })
            .collect::<Vec<_>>()
    } else {
        match task.source.trim() {
            SOURCE_COMBINED => vec![SOURCE_CODEX.into(), SOURCE_CLAUDE_CODE.into()],
            "" => vec![],
            source => vec![source.to_owned()],
        }
    };
    sources.sort_by_key(|source| match source.as_str() {
        SOURCE_CODEX => 0,
        SOURCE_CLAUDE_CODE => 1,
        SOURCE_OPENCODE => 2,
        _ => 3,
    });
    sources
}

fn default_periodic_tasks() -> Vec<PeriodicTaskArgs> {
    vec![
        PeriodicTaskArgs {
            name: "Daily import and summaries".to_string(),
            source: "codex+claude-code+opencode".to_string(),
            sources: vec![
                SOURCE_CODEX.to_string(),
                SOURCE_CLAUDE_CODE.to_string(),
                SOURCE_OPENCODE.to_string(),
            ],
            task_kind: PeriodicTaskKind::Regular,
            hour: 0,
            minute: 0,
            interval_hours: None,
            interval_days: Some(1),
            weekly_day: None,
            monthly_day: None,
            lookback_days: 1,
            force_thread_summary: true,
            run_summary_thread_group: true,
            run_summary_daily: true,
            run_personality: false,
            run_reflection: false,
            stop_on_error: false,
        },
        PeriodicTaskArgs {
            name: "Weekly summary".to_string(),
            source: "codex+claude-code+opencode".to_string(),
            sources: vec![
                SOURCE_CODEX.to_string(),
                SOURCE_CLAUDE_CODE.to_string(),
                SOURCE_OPENCODE.to_string(),
            ],
            task_kind: PeriodicTaskKind::Weekly,
            hour: 1,
            minute: 0,
            interval_hours: None,
            interval_days: None,
            weekly_day: Some(1),
            monthly_day: None,
            lookback_days: 7,
            force_thread_summary: true,
            run_summary_thread_group: false,
            run_summary_daily: true,
            run_personality: false,
            run_reflection: false,
            stop_on_error: false,
        },
        PeriodicTaskArgs {
            name: "Monthly summary".to_string(),
            source: "codex+claude-code+opencode".to_string(),
            sources: vec![
                SOURCE_CODEX.to_string(),
                SOURCE_CLAUDE_CODE.to_string(),
                SOURCE_OPENCODE.to_string(),
            ],
            task_kind: PeriodicTaskKind::Monthly,
            hour: 2,
            minute: 0,
            interval_hours: None,
            interval_days: None,
            weekly_day: None,
            monthly_day: Some(1),
            lookback_days: 31,
            force_thread_summary: true,
            run_summary_thread_group: false,
            run_summary_daily: true,
            run_personality: false,
            run_reflection: false,
            stop_on_error: false,
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
        return Err(AppError::Config(CANCEL_NOT_MANAGED.into()));
    }
    Ok(data)
}

pub fn wrap_worker_args(mut task: PeriodicTaskArgs, runtime: PeriodicRuntime) -> AppResult<String> {
    validate_task(&task)?;
    validate_runtime_for_task(&task, &runtime)?;
    if !task.sources.is_empty() {
        task.sources = effective_sources(&task);
    }
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
    (input.schema_version == SCHEMA_VERSION
        && validate_runtime_for_task(&input.task, &input.runtime).is_ok())
    .then_some(input)
}

fn validate_runtime_for_task(task: &PeriodicTaskArgs, runtime: &PeriodicRuntime) -> AppResult<()> {
    if task.task_kind == PeriodicTaskKind::Regular
        && effective_sources(task)
            .iter()
            .any(|source| source == SOURCE_OPENCODE)
    {
        for (name, value) in [
            ("import_working_dir", &runtime.import_working_dir),
            ("opencode_db_path", &runtime.opencode_db_path),
        ] {
            if value.trim().is_empty() || !Path::new(value).is_absolute() {
                return Err(AppError::Config(format!(
                    "OpenCode periodic runtime requires an absolute {name}"
                )));
            }
        }
    }
    Ok(())
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

pub(super) fn current_runtime(
    services: &PeriodicServices,
    llm_worker_name: &str,
    output_language: &str,
    memories_import_bin: &str,
    summary_llm_contract: &super::import::SummaryLlmContract,
    summary_output_guidance: serde_json::Value,
) -> AppResult<PeriodicRuntime> {
    let import_working_dir = std::env::current_dir()
        .map_err(|error| AppError::Config(format!("could not resolve import cwd: {error}")))?;
    let data_paths = crate::data::DataPaths::resolve().ok();
    let memories = &services.memories;
    let tracker = services.issue_tracker.clone().unwrap_or_default();
    let mut runtime = PeriodicRuntime {
        memories_grpc_host: memories.host.clone(),
        memories_grpc_port: memories.port,
        memories_grpc_tls: memories.tls,
        llm_worker_name: llm_worker_name.to_string(),
        llm_context_window_tokens: summary_llm_contract.context_window_tokens,
        llm_expected_model: summary_llm_contract.expected_model.clone(),
        summary_max_completion_tokens: summary_llm_contract.budget.max_completion_tokens,
        summary_input_byte_budget: summary_llm_contract.budget.input_byte_budget,
        summary_output_guidance,
        output_language: runtime_output_language(output_language),
        memories_import_bin: memories_import_bin.to_string(),
        policy_json: issue_extract_policy_json(data_paths.as_ref()),
        import_working_dir: import_working_dir.display().to_string(),
        opencode_db_path: super::import::resolve_opencode_db_path(&import_working_dir)
            .display()
            .to_string(),
        issue_tracker_grpc_host: tracker.host,
        issue_tracker_grpc_port: tracker.port,
        issue_tracker_grpc_tls: tracker.tls,
    };
    if let Some(paths) = &services.jobworkerp_host_paths {
        apply_jobworkerp_host_paths(&mut runtime, paths);
    }
    Ok(runtime)
}

/// The services a scheduled run reaches from the jobworkerp host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PeriodicServices {
    pub memories: super::connection::GrpcAddress,
    /// The Issue tracker as jobworkerp reaches it; `None` while tracking is off.
    pub issue_tracker: Option<super::connection::GrpcAddress>,
    /// Paths on a remote jobworkerp host; `None` while jobworkerp is local.
    pub jobworkerp_host_paths: Option<super::connection::JobworkerpHostPaths>,
}

impl PeriodicServices {
    pub(crate) fn new(
        connection: &super::connection::ConnectionConfig,
        memories: super::connection::GrpcAddress,
    ) -> Self {
        Self {
            memories,
            issue_tracker: crate::sidecar::lifecycle::published_issue_tracker_for_jobworkerp(),
            jobworkerp_host_paths: (!connection.jobworkerp.is_local())
                .then(|| connection.jobworkerp_host_paths.clone()),
        }
    }
}

/// The periodic import runs on the jobworkerp host and reads its files, so a
/// remote jobworkerp takes that host's paths in place of this machine's.
fn apply_jobworkerp_host_paths(
    runtime: &mut PeriodicRuntime,
    paths: &super::connection::JobworkerpHostPaths,
) {
    for (target, path) in [
        (&mut runtime.memories_import_bin, &paths.memories_import_bin),
        (&mut runtime.opencode_db_path, &paths.opencode_db_path),
        (&mut runtime.import_working_dir, &paths.import_working_dir),
    ] {
        if let Some(path) = super::connection::non_empty(path.as_deref()) {
            *target = path.to_string();
        }
    }
}

fn period_summary_budget_requirement(
    task: &PeriodicTaskArgs,
    enabled: bool,
) -> super::import::SummaryBudgetRequirement {
    if enabled && (task.task_kind != PeriodicTaskKind::Regular || task.run_summary_daily) {
        super::import::SummaryBudgetRequirement::RequiredForPeriodSummaries
    } else {
        super::import::SummaryBudgetRequirement::SchemaPlaceholder
    }
}

fn enabled_for_runtime_refresh(
    persisted_enabled: bool,
    scheduler_id: i64,
    enabled_overrides: &HashSet<i64>,
) -> bool {
    persisted_enabled || enabled_overrides.contains(&scheduler_id)
}

pub(super) fn validate_enabled_periodic_task(
    settings: &super::llm_settings::LlmSettings,
    task: &PeriodicTaskArgs,
) -> AppResult<()> {
    super::import::summary_llm_contract_with(
        settings,
        super::process_env_lookup,
        period_summary_budget_requirement(task, true),
    )
    .map(|_| ())
}

/// Scheduler refresh must not leave unrelated tasks on stale sidecar ports or
/// model contracts merely because one enabled period-summary task no longer
/// has enough configured context. Such a task is persisted disabled with a
/// schema-valid placeholder contract; saving it as enabled again runs the
/// normal strict validation in `current_periodic_context`.
fn refresh_summary_llm_contract(
    settings: &super::llm_settings::LlmSettings,
    task: &PeriodicTaskArgs,
    enabled: bool,
) -> AppResult<(super::import::SummaryLlmContract, bool)> {
    let requirement = period_summary_budget_requirement(task, enabled);
    match super::import::summary_llm_contract_with(settings, super::process_env_lookup, requirement)
    {
        Ok(contract) => Ok((contract, false)),
        Err(error) => {
            warn!(
                task_name = %task.name,
                task_kind = ?task.task_kind,
                error = %error,
                "disabling periodic task because its configured context cannot run period summaries"
            );
            let placeholder = super::import::summary_llm_contract_with(
                settings,
                super::process_env_lookup,
                super::import::SummaryBudgetRequirement::SchemaPlaceholder,
            )?;
            Ok((placeholder, true))
        }
    }
}

fn issue_extract_policy_json(data_paths: Option<&crate::data::DataPaths>) -> String {
    data_paths
        .and_then(|paths| {
            let enabled =
                crate::commands::issue_tracker_settings::effective_issue_tracking_enabled(paths);
            crate::issue_extract::snapshot_json(&paths.root, enabled)
        })
        .unwrap_or_default()
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

/// Point conductor's Lookback server record at `jobworkerp` (local or remote).
/// The record keeps its original name so existing schedulers stay attached.
async fn ensure_jobworkerp_server(
    channel: Channel,
    jobworkerp: &super::connection::GrpcAddress,
) -> AppResult<JobworkerpServerId> {
    let mut client = JobworkerpServerServiceClient::new(channel);
    let data = JobworkerpServerData {
        name: LOOKBACK_JOBWORKERP_SERVER_NAME.to_string(),
        host: jobworkerp.host.clone(),
        port: jobworkerp.port.to_string(),
        ssl_enabled: jobworkerp.tls,
        description: Some("Lookback jobworkerp".to_string()),
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

/// Users never see the job timeout, so the app corrects one that would cut
/// the run short. Schedulers saved before conductor had the field read back
/// unset, which means the client's 20-minute default and cancels the run in
/// the middle of a summary stage.
fn periodic_job_timeout_sec(stored: Option<u32>) -> u32 {
    match stored {
        Some(sec) if sec == 0 || sec >= PERIODIC_JOB_TIMEOUT_SEC => sec,
        _ => PERIODIC_JOB_TIMEOUT_SEC,
    }
}

/// `previous` is the scheduler as saved, `None` when creating one.
fn scheduler_data(
    task: PeriodicTaskArgs,
    enabled: bool,
    description: Option<String>,
    jobworkerp_server_id: JobworkerpServerId,
    runtime: PeriodicRuntime,
    previous: Option<&CronSchedulerData>,
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
        created_at: previous.map_or(0, |previous| previous.created_at),
        updated_at: 0,
        args: Some(args),
        job_timeout_sec: Some(periodic_job_timeout_sec(
            previous.and_then(|previous| previous.job_timeout_sec),
        )),
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
        data.description.clone(),
        jobworkerp_server_id,
        runtime,
        Some(&data),
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
    enabled: bool,
) -> AppResult<(Channel, JobworkerpServerId, PeriodicRuntime)> {
    let targets = state.periodic_targets()?;
    let channel = conductor_channel(&targets.conductor_url).await?;
    let server_id = ensure_jobworkerp_server(channel.clone(), &targets.jobworkerp).await?;
    let services = targets.services;
    let memories_import_bin = memories_import_bin_for_task(task)?;
    let llm_settings = state.llm_settings_snapshot();
    let summary_llm_contract = super::import::summary_llm_contract_with(
        &llm_settings,
        super::process_env_lookup,
        period_summary_budget_requirement(task, enabled),
    )?;
    let summary_output_guidance = super::import::summary_output_guidance_for_context(
        &llm_settings.summary_output_guidance,
        summary_llm_contract.context_window_tokens,
    );
    let runtime = current_runtime(
        &services,
        super::llm_settings::worker_name_for(llm_settings.mode),
        &state.active_output_language(),
        &memories_import_bin,
        &summary_llm_contract,
        summary_output_guidance,
    )?;
    let runtime = PeriodicRuntime { ..runtime };
    Ok((channel, server_id, runtime))
}

pub(crate) async fn refresh_lookback_periodic_runtime(
    context: PeriodicRuntimeRefreshContext<'_>,
) -> AppResult<()> {
    refresh_lookback_periodic_runtime_with_enabled_overrides(context, &HashSet::new()).await
}

/// What a scheduler refresh needs: what it writes into conductor, and the
/// local marker of conductors already given the default schedulers.
pub(crate) struct PeriodicRuntimeRefreshContext<'a> {
    pub conductor_url: &'a str,
    pub jobworkerp: &'a super::connection::GrpcAddress,
    pub services: &'a PeriodicServices,
    pub llm_settings: &'a super::llm_settings::LlmSettings,
    pub output_language: &'a str,
    pub memories_import_bin: &'a Path,
    pub seed_marker_path: &'a Path,
    /// See `connection::conductor_identity`.
    pub conductor_identity: &'a str,
}

/// Refresh runtime values after a temporary scheduler pause. `enabled_overrides`
/// preserves the pre-pause enabled state for strict period-summary validation.
async fn refresh_lookback_periodic_runtime_with_enabled_overrides(
    context: PeriodicRuntimeRefreshContext<'_>,
    enabled_overrides: &HashSet<i64>,
) -> AppResult<()> {
    let channel = conductor_channel(context.conductor_url).await?;
    let server_id = ensure_jobworkerp_server(channel.clone(), context.jobworkerp).await?;
    let memories_import_bin = context.memories_import_bin.to_string_lossy().into_owned();
    let schedulers = list_schedulers(channel.clone(), None, None).await?;
    let mut client = CronSchedulerServiceClient::new(channel);
    for scheduler in &schedulers {
        let Some(id) = scheduler.id else {
            continue;
        };
        let Some(mut data) = scheduler.data.clone() else {
            continue;
        };
        if !data.name.starts_with(LOOKBACK_PERIODIC_PREFIX) {
            continue;
        }
        let Some(input) = data.args.as_deref().and_then(unwrap_worker_args) else {
            continue;
        };
        let enabled_for_validation =
            enabled_for_runtime_refresh(data.enabled, id.value, enabled_overrides);
        let (summary_llm_contract, disable_task) = refresh_summary_llm_contract(
            context.llm_settings,
            &input.task,
            enabled_for_validation,
        )?;
        if disable_task {
            data.enabled = false;
        }
        let runtime = current_runtime(
            context.services,
            super::llm_settings::worker_name_for(context.llm_settings.mode),
            context.output_language,
            &memories_import_bin,
            &summary_llm_contract,
            super::import::summary_output_guidance_for_context(
                &context.llm_settings.summary_output_guidance,
                summary_llm_contract.context_window_tokens,
            ),
        )?;
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
    // Seeded defaults are disabled. They should not prevent a sidecar refresh
    // when the configured context is too small for period summaries.
    let seed_contract = super::import::summary_llm_contract_with(
        context.llm_settings,
        super::process_env_lookup,
        super::import::SummaryBudgetRequirement::SchemaPlaceholder,
    )?;
    let seed_runtime = current_runtime(
        context.services,
        super::llm_settings::worker_name_for(context.llm_settings.mode),
        context.output_language,
        &memories_import_bin,
        &seed_contract,
        super::import::summary_output_guidance_for_context(
            &context.llm_settings.summary_output_guidance,
            seed_contract.context_window_tokens,
        ),
    )?;
    seed_default_periodic_tasks(
        &mut client,
        &schedulers,
        server_id,
        seed_runtime,
        context.seed_marker_path,
        context.conductor_identity,
    )
    .await?;
    Ok(())
}

/// Rebuild the runtime snapshots stored in managed Lookback schedulers from
/// the current application settings. Call after a worker restart and after a
/// prompt-only setting change; neither caller may leave a stale snapshot.
pub(crate) async fn refresh_lookback_periodic_runtime_from_state(
    state: &AppState,
    enabled_overrides: &HashSet<i64>,
) -> AppResult<()> {
    let targets = state.periodic_targets()?;
    let llm_settings = state.llm_settings_snapshot();
    let memories_import_bin = crate::resolve_memories_import_bin_path()
        .unwrap_or_else(|_| std::path::PathBuf::from("memories-import"));
    let output_language = state.active_output_language();
    let seed_marker_path = state.data.periodic_defaults_seed_path();
    refresh_lookback_periodic_runtime_with_enabled_overrides(
        PeriodicRuntimeRefreshContext {
            conductor_url: &targets.conductor_url,
            jobworkerp: &targets.jobworkerp,
            services: &targets.services,
            llm_settings: &llm_settings,
            output_language: &output_language,
            memories_import_bin: &memories_import_bin,
            seed_marker_path: &seed_marker_path,
            conductor_identity: &targets.conductor_identity,
        },
        enabled_overrides,
    )
    .await
}

async fn seed_default_periodic_tasks(
    client: &mut CronSchedulerServiceClient<Channel>,
    schedulers: &[CronScheduler],
    jobworkerp_server_id: JobworkerpServerId,
    runtime: PeriodicRuntime,
    seed_marker_path: &Path,
    conductor_identity: &str,
) -> AppResult<()> {
    let mut marker = read_seed_marker(seed_marker_path);
    if marker
        .conductors
        .iter()
        .any(|seeded| seeded == conductor_identity)
    {
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
            None,
        )?;
        client.create(data).await?;
    }
    marker.conductors.push(conductor_identity.to_string());
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

/// Names of the managed schedulers conductor stored without a job timeout.
/// A conductor older than the field drops it silently, and its default then
/// cancels a run in the middle of a stage; checked once conductor starts.
pub(crate) async fn schedulers_without_job_timeout(conductor_url: &str) -> AppResult<Vec<String>> {
    Ok(schedulers_lacking_job_timeout(
        &list_schedulers(conductor_channel(conductor_url).await?, None, None).await?,
    ))
}

fn schedulers_lacking_job_timeout(schedulers: &[CronScheduler]) -> Vec<String> {
    schedulers
        .iter()
        .filter_map(|scheduler| scheduler.data.as_ref())
        .filter(|data| is_lookback_managed(&data.name) && data.job_timeout_sec.is_none())
        .map(|data| display_name(&data.name).to_string())
        .collect()
}

#[tauri::command]
pub async fn list_periodic_tasks(
    state: tauri::State<'_, AppState>,
    req: ListPeriodicTasksRequest,
) -> AppResult<Vec<PeriodicTaskEntry>> {
    let schedulers = list_schedulers(
        conductor_channel(&state.conductor_url()?).await?,
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
    let _settings_gate = state.lock_llm_settings_dispatch_gate().await;
    let (channel, server_id, runtime) =
        current_periodic_context(&state, &req.task, req.enabled).await?;
    let mut client = CronSchedulerServiceClient::new(channel);
    let data = scheduler_data(
        req.task,
        req.enabled,
        req.description,
        server_id,
        runtime,
        None,
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
    let _settings_gate = state.lock_llm_settings_dispatch_gate().await;
    let id_value = parse_scheduler_id(
        req.id
            .as_deref()
            .ok_or_else(|| AppError::Config("id is required for update".into()))?,
    )?;
    let (channel, server_id, runtime) =
        current_periodic_context(&state, &req.task, req.enabled).await?;
    let mut client = CronSchedulerServiceClient::new(channel);
    let id = CronSchedulerId { value: id_value };
    // Prove ownership AND read the stored scheduler in one Find: a missing or
    // non-Lookback scheduler must error here rather than silently proceeding to
    // create-via-update with created_at=0.
    let existing = ensure_lookback_scheduler(&mut client, id).await?;
    let data = scheduler_data(
        req.task,
        req.enabled,
        req.description,
        server_id,
        runtime,
        Some(&existing),
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
    let _settings_gate = state.lock_llm_settings_dispatch_gate().await;
    let id = parse_scheduler_id(&req.id)?;
    let mut client = cron_client(&state.conductor_url()?).await?;
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
    let _settings_gate = state.lock_llm_settings_dispatch_gate().await;
    let id_value = parse_scheduler_id(&req.id)?;
    let mut client = cron_client(&state.conductor_url()?).await?;
    let id = CronSchedulerId { value: id_value };
    // Reject a missing or non-Lookback scheduler before toggling.
    let mut data = ensure_lookback_scheduler(&mut client, id).await?;
    if req.enabled {
        let input = data
            .args
            .as_deref()
            .and_then(unwrap_worker_args)
            .ok_or_else(|| AppError::Config("invalid Lookback periodic task arguments".into()))?;
        // A refresh may have disabled this task after a context reduction.
        // Do not allow a direct enable toggle to bypass the same strict
        // period-summary budget validation used by create/update.
        let settings = state.llm_settings_snapshot();
        validate_enabled_periodic_task(&settings, &input.task)?;
    }
    data.enabled = req.enabled;
    data.job_timeout_sec = Some(periodic_job_timeout_sec(data.job_timeout_sec));
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
    use std::{collections::BTreeMap, sync::Arc};

    use command_utils::util::jq::execute_jq;
    use serde_json::json;

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
            run_summary_thread_group: false,
            run_summary_daily: true,
            run_personality: false,
            run_reflection: false,
            stop_on_error: false,
        }
    }

    fn config_message(result: AppResult<()>) -> String {
        match result {
            Err(AppError::Config(message)) => message,
            Err(error) => panic!("expected a config error, got {error:?}"),
            Ok(()) => panic!("expected validation to fail"),
        }
    }

    fn runtime(port: u16) -> PeriodicRuntime {
        PeriodicRuntime {
            memories_grpc_host: "127.0.0.1".into(),
            memories_grpc_port: port,
            memories_grpc_tls: false,
            llm_worker_name: "memories-llm".into(),
            llm_context_window_tokens: 131_072,
            llm_expected_model: String::new(),
            summary_max_completion_tokens: 32_768,
            summary_input_byte_budget: 200_000,
            summary_output_guidance: default_summary_output_guidance(),
            output_language: "ja".into(),
            memories_import_bin: "/bin/memories-import".into(),
            policy_json: String::new(),
            import_working_dir: "/work/app".into(),
            opencode_db_path: "/work/app/.local/share/opencode/opencode.db".into(),
            issue_tracker_grpc_host: String::new(),
            issue_tracker_grpc_port: 0,
            issue_tracker_grpc_tls: false,
        }
    }

    #[test]
    fn defaults_are_seeded_once_per_conductor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("periodic-defaults-seeded.json");
        assert!(read_seed_marker(&path).conductors.is_empty());
        // A marker from before per-conductor seeding covers the local one.
        std::fs::write(&path, br#"{"version":1}"#).unwrap();
        assert_eq!(read_seed_marker(&path).conductors, ["local"]);
    }

    #[test]
    fn a_conductor_that_drops_the_job_timeout_is_detected() {
        let scheduler = |name: &str, job_timeout_sec| CronScheduler {
            id: None,
            data: Some(CronSchedulerData {
                name: name.to_string(),
                job_timeout_sec,
                ..Default::default()
            }),
        };
        let lacking = schedulers_lacking_job_timeout(&[
            scheduler(&scheduler_name("daily"), None),
            scheduler(&scheduler_name("weekly"), Some(PERIODIC_JOB_TIMEOUT_SEC)),
            scheduler("someone-else", None),
        ]);
        assert_eq!(lacking, ["daily"]);
    }

    #[test]
    fn a_remote_jobworkerp_imports_with_its_own_host_paths() {
        let mut remote = runtime(9010);
        apply_jobworkerp_host_paths(
            &mut remote,
            &super::super::connection::JobworkerpHostPaths {
                memories_import_bin: Some("/opt/lookback/memories-import".into()),
                opencode_db_path: Some("  ".into()),
                import_working_dir: None,
            },
        );
        assert_eq!(remote.memories_import_bin, "/opt/lookback/memories-import");
        // Unset or blank paths keep this machine's.
        assert_eq!(remote.opencode_db_path, runtime(9010).opencode_db_path);
        assert_eq!(remote.import_working_dir, "/work/app");
    }

    fn summary_contract() -> super::super::import::SummaryLlmContract {
        super::super::import::summary_llm_contract_with(
            &super::super::llm_settings::LlmSettings::default(),
            |_| None,
            super::super::import::SummaryBudgetRequirement::RequiredForPeriodSummaries,
        )
        .unwrap()
    }

    #[test]
    fn refresh_disables_only_enabled_period_task_without_budget() {
        let settings = super::super::llm_settings::LlmSettings {
            mode: super::super::llm_settings::LlmMode::External,
            local_ctx_size: Some(4_096),
            ..Default::default()
        };
        let task = task(PeriodicTaskKind::Weekly);

        let (contract, disable_task) =
            refresh_summary_llm_contract(&settings, &task, true).unwrap();
        assert!(disable_task);
        assert_eq!(contract.context_window_tokens, 4_096);
        assert_eq!(contract.budget.max_completion_tokens, 1);
        assert_eq!(contract.budget.input_byte_budget, 0);

        let (_, disable_already_disabled) =
            refresh_summary_llm_contract(&settings, &task, false).unwrap();
        assert!(!disable_already_disabled);
        assert_eq!(
            period_summary_budget_requirement(&task, false),
            super::super::import::SummaryBudgetRequirement::SchemaPlaceholder
        );
        assert_eq!(
            period_summary_budget_requirement(&task, true),
            super::super::import::SummaryBudgetRequirement::RequiredForPeriodSummaries
        );

        let overrides = HashSet::from([42]);
        assert!(enabled_for_runtime_refresh(false, 42, &overrides));
        assert!(!enabled_for_runtime_refresh(false, 7, &overrides));
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
            Some(&CronSchedulerData {
                created_at: 123,
                ..Default::default()
            }),
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
    fn new_schedulers_wait_as_long_as_the_periodic_run() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        let data = scheduler_data(
            t,
            false,
            None,
            JobworkerpServerId { value: 42 },
            runtime(9010),
            None,
        )
        .unwrap();

        assert_eq!(data.job_timeout_sec, Some(PERIODIC_JOB_TIMEOUT_SEC));
    }

    #[test]
    fn only_a_job_timeout_too_short_for_the_periodic_run_is_replaced() {
        for (stored, expected) in [
            (None, PERIODIC_JOB_TIMEOUT_SEC),
            (Some(1200), PERIODIC_JOB_TIMEOUT_SEC),
            (Some(PERIODIC_JOB_TIMEOUT_SEC - 1), PERIODIC_JOB_TIMEOUT_SEC),
            (Some(PERIODIC_JOB_TIMEOUT_SEC), PERIODIC_JOB_TIMEOUT_SEC),
            (
                Some(PERIODIC_JOB_TIMEOUT_SEC + 1),
                PERIODIC_JOB_TIMEOUT_SEC + 1,
            ),
            // 0 disables the timeout, which never cuts the run short.
            (Some(0), 0),
        ] {
            assert_eq!(periodic_job_timeout_sec(stored), expected, "{stored:?}");
        }
    }

    #[test]
    fn runtime_refresh_repairs_the_job_timeout_of_existing_schedulers() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        let refreshed = |job_timeout_sec| {
            refreshed_scheduler_data(
                &CronSchedulerId { value: 9 },
                CronSchedulerData {
                    name: scheduler_name("upgraded"),
                    crontab: "0 0 9 * * *".into(),
                    enabled: true,
                    args: Some(wrap_worker_args(t.clone(), runtime(9010)).unwrap()),
                    job_timeout_sec,
                    ..Default::default()
                },
                JobworkerpServerId { value: 99 },
                runtime(9020),
            )
            .unwrap()
            .job_timeout_sec
        };

        // A scheduler created before the column existed reads back as unset.
        assert_eq!(refreshed(None), Some(PERIODIC_JOB_TIMEOUT_SEC));
        assert_eq!(refreshed(Some(0)), Some(0));
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
            None,
        )
        .unwrap();

        let outer: ConductorWorkerArgs =
            serde_json::from_str(data.args.as_deref().unwrap()).unwrap();
        let input: serde_json::Value = serde_json::from_str(&outer.input).unwrap();
        // The wrapper forwards `output_language` into each batch (which resolves
        // its language-specific single worker by name); the old per-feature
        // `*-single.yaml` path relay is gone.
        assert_eq!(input["runtime"]["output_language"], "ja");
        assert_eq!(input["runtime"]["llm_context_window_tokens"], 131_072);
        assert_eq!(input["runtime"]["summary_max_completion_tokens"], 32_768);
        assert_eq!(input["runtime"]["summary_input_byte_budget"], 200_000);
        assert_eq!(
            input["runtime"]["summary_output_guidance"]["daily"]["item_chars"],
            300
        );
        assert!(input["runtime"].get("workflow_paths").is_none());
    }

    #[test]
    fn legacy_periodic_runtime_gets_a_renderable_guidance_default() {
        let legacy: PeriodicRuntime = serde_json::from_value(serde_json::json!({
            "memories_grpc_host": "127.0.0.1",
            "memories_grpc_port": 9010,
            "memories_grpc_tls": false,
            "llm_worker_name": "memories-llm"
        }))
        .unwrap();

        assert_eq!(legacy.summary_output_guidance["daily"]["item_chars"], 300);
        assert_eq!(
            legacy.summary_output_guidance["weekly"]["total_chars"],
            30_000
        );
        assert_eq!(
            legacy.summary_output_guidance["max_purpose_groups"]["monthly"],
            50
        );
    }

    #[test]
    fn runtime_constructors_normalize_blank_output_language_to_ja() {
        // The resolved value is never blank, but an old persisted scheduler can
        // carry "" — the wrapper must never relay it to a batch (it would build
        // a nonexistent `memories-<feature>-single-` worker name).
        let cb = super::super::connection::GrpcAddress {
            host: "127.0.0.1".into(),
            port: 9010,
            tls: false,
        };
        let import_bin = "/bin/memories-import";
        assert_eq!(
            current_runtime(
                &PeriodicServices {
                    memories: cb.clone(),
                    ..Default::default()
                },
                "memories-llm",
                "",
                import_bin,
                &summary_contract(),
                default_summary_output_guidance(),
            )
            .unwrap()
            .output_language,
            "ja"
        );
        assert_eq!(
            current_runtime(
                &PeriodicServices {
                    memories: cb.clone(),
                    ..Default::default()
                },
                "memories-llm",
                "  ",
                import_bin,
                &summary_contract(),
                default_summary_output_guidance(),
            )
            .unwrap()
            .output_language,
            "ja"
        );
        assert_eq!(
            current_runtime(
                &PeriodicServices {
                    memories: cb.clone(),
                    ..Default::default()
                },
                "memories-llm",
                "en",
                import_bin,
                &summary_contract(),
                default_summary_output_guidance(),
            )
            .unwrap()
            .output_language,
            "en"
        );
        assert_eq!(
            current_runtime(
                &PeriodicServices {
                    memories: cb.clone(),
                    ..Default::default()
                },
                "memories-llm",
                "",
                import_bin,
                &summary_contract(),
                default_summary_output_guidance(),
            )
            .unwrap()
            .output_language,
            "ja"
        );
        assert_eq!(
            current_runtime(
                &PeriodicServices {
                    memories: cb.clone(),
                    ..Default::default()
                },
                "memories-llm",
                "ja",
                import_bin,
                &summary_contract(),
                default_summary_output_guidance(),
            )
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
        assert_eq!(rt.policy_json, "");
    }

    #[test]
    fn empty_data_root_policy_snapshot_is_auto_register() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::data::DataPaths::with_root(dir.path());
        let json = issue_extract_policy_json(Some(&paths));
        let policy: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(policy["version"], 1, "{json}");
        assert_eq!(
            policy["extraction_enabled"],
            crate::commands::issue_tracker_settings::effective_issue_tracking_enabled(&paths),
            "{json}"
        );
        assert_eq!(policy["auto_register_min_confidence"], 0.90, "{json}");
    }

    #[test]
    fn periodic_policy_snapshot_uses_the_saved_issue_tracker_toggle() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::data::DataPaths::with_root(dir.path());
        crate::commands::issue_tracker_settings::save_issue_tracker_settings(
            &paths.issue_tracker_settings_path(),
            &crate::commands::issue_tracker_settings::IssueTrackerSettings {
                enabled: Some(false),
            },
        )
        .unwrap();

        let policy: serde_json::Value =
            serde_json::from_str(&issue_extract_policy_json(Some(&paths))).unwrap();

        assert_eq!(policy["extraction_enabled"], false);
    }

    #[test]
    fn periodic_policy_snapshot_falls_back_to_an_empty_string_without_a_data_root() {
        assert_eq!(issue_extract_policy_json(None), "");
    }

    #[test]
    fn wrap_worker_args_forwards_policy_snapshot() {
        let mut runtime = runtime(9010);
        runtime.policy_json = r#"{"version":1,"extraction_enabled":false,"explicit_only":true,"allowed_kinds":[],"extract_cap":5,"auto_register_priority_range":{"lower":"CANDIDATE_PRIORITY_P0","upper":"CANDIDATE_PRIORITY_P2"}}"#.into();
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        let args = wrap_worker_args(t, runtime).unwrap();
        let outer: ConductorWorkerArgs = serde_json::from_str(&args).unwrap();
        let input: serde_json::Value = serde_json::from_str(&outer.input).unwrap();
        assert!(
            input["runtime"]["policy_json"]
                .as_str()
                .unwrap_or("")
                .contains("\"extraction_enabled\":false"),
            "{input}"
        );
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
            None,
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
            None,
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
    fn periodic_task_accepts_opencode_and_canonicalizes_source_order() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.source = "opencode".into();
        t.sources = vec!["opencode".into()];
        t.interval_hours = Some(24);

        assert!(validate_task(&t).is_ok());
        assert_eq!(effective_sources(&t), vec!["opencode".to_string()]);

        t.source = SOURCE_COMBINED.into();
        t.sources = vec![SOURCE_CLAUDE_CODE.into(), SOURCE_CODEX.into()];
        assert_eq!(
            effective_sources(&t),
            vec![SOURCE_CODEX.to_string(), SOURCE_CLAUDE_CODE.to_string()]
        );
    }

    #[test]
    fn periodic_task_rejects_duplicate_sources() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.source = "opencode+opencode".into();
        t.sources = vec!["opencode".into(), "opencode".into()];
        t.interval_hours = Some(24);

        assert_eq!(
            config_message(validate_task(&t)),
            crate::error_messages::VALIDATION_SOURCE_DUPLICATE
        );
    }

    #[test]
    fn seeded_default_tasks_import_every_supported_source() {
        // Seed defaults must include opencode so a fresh install imports all
        // agent logs, matching the UI's all-source default draft.
        for task in default_periodic_tasks() {
            assert_eq!(
                effective_sources(&task),
                vec![
                    SOURCE_CODEX.to_string(),
                    SOURCE_CLAUDE_CODE.to_string(),
                    SOURCE_OPENCODE.to_string()
                ],
                "task {} must default to every supported source",
                task.name
            );
            assert_eq!(task.source, "codex+claude-code+opencode");
        }
    }

    #[test]
    fn periodic_task_accepts_all_three_sources_with_joined_display_source() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.source = "codex+claude-code+opencode".into();
        t.sources = vec!["codex".into(), "claude-code".into(), "opencode".into()];
        t.interval_hours = Some(24);

        assert!(validate_task(&t).is_ok());
        assert_eq!(
            effective_sources(&t),
            vec![
                SOURCE_CODEX.to_string(),
                SOURCE_CLAUDE_CODE.to_string(),
                SOURCE_OPENCODE.to_string()
            ]
        );
    }

    #[test]
    fn periodic_task_accepts_a_two_source_subset_beyond_the_legacy_pair() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.source = "codex+opencode".into();
        t.sources = vec!["codex".into(), "opencode".into()];
        t.interval_hours = Some(24);

        assert!(validate_task(&t).is_ok());
    }

    #[test]
    fn periodic_task_rejects_mismatched_source_for_multi_source_selection() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.source = "codex".into();
        t.sources = vec!["codex".into(), "opencode".into()];
        t.interval_hours = Some(24);

        assert_eq!(
            config_message(validate_task(&t)),
            crate::error_messages::VALIDATION_SOURCE_MISMATCH
        );
    }

    #[test]
    fn periodic_task_rejects_source_and_sources_mismatch() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.sources = vec!["claude-code".into()];
        t.interval_hours = Some(24);

        assert_eq!(
            config_message(validate_task(&t)),
            crate::error_messages::VALIDATION_SOURCE_MISMATCH
        );
    }

    #[test]
    fn periodic_runtime_serializes_import_context() {
        let value = serde_json::to_value(runtime(9010)).unwrap();

        assert_eq!(value["import_working_dir"], "/work/app");
        assert_eq!(
            value["opencode_db_path"],
            "/work/app/.local/share/opencode/opencode.db"
        );
    }

    #[test]
    fn opencode_regular_task_rejects_legacy_runtime_without_absolute_paths() {
        let mut task = task(PeriodicTaskKind::Regular);
        task.source = SOURCE_OPENCODE.into();
        task.sources = vec![SOURCE_OPENCODE.into()];
        task.interval_hours = Some(24);
        let runtime = PeriodicRuntime {
            import_working_dir: String::new(),
            opencode_db_path: String::new(),
            ..runtime(9010)
        };

        assert!(validate_runtime_for_task(&task, &runtime).is_err());
        assert!(wrap_worker_args(task, runtime).is_err());
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
        assert!(yaml.contains("runImportOpenCode"));
        assert!(yaml.contains("--opencode-db"));
        assert!(yaml.contains("opencode_db_path"));
        assert!(yaml.contains("import_working_dir"));
        assert!(yaml.contains("agent:opencode"));
        assert!(yaml.contains("--since"));
        assert!(yaml.contains("--server-url"));
        assert!(yaml.contains("runnerName: COMMAND"));
        assert!(yaml.contains("treat_nonzero_as_error: true"));
        assert!(yaml.contains("interval_hours: { type: [integer, \"null\"], minimum: 1 }"));
        assert!(yaml.contains("interval_days: { type: [integer, \"null\"], minimum: 1 }"));
        assert!(yaml.contains("lookback_days: { type: integer, minimum: 0 }"));
        assert!(yaml.contains("start_date: (if $workflow.input.task.lookback_days == 0"));
        assert!(yaml.contains("end_date: (if $workflow.input.task.lookback_days == 0"));
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
        for stage in ["dailyForRegular", "weeklyStage", "monthlyStage"] {
            let start = yaml
                .find(&format!("  - {stage}:"))
                .unwrap_or_else(|| panic!("missing {stage}"));
            let end = yaml[start + 1..]
                .find("\n  - ")
                .map(|offset| start + 1 + offset)
                .unwrap_or(yaml.len());
            assert!(
                yaml[start..end].contains("user_id: 1"),
                "{stage} must pass the required owner to its summary batch"
            );
        }
        let weekly_stage = yaml.find("  - weeklyStage:").unwrap();
        let monthly_stage = yaml.find("  - monthlyStage:").unwrap();
        let weekly_body = &yaml[weekly_stage..monthly_stage];
        assert!(
            weekly_body.find("workerName: memories-daily-summary-batch")
                < weekly_body.find("workerName: memories-weekly-summary-batch"),
            "periodic weekly must generate its daily prerequisites first"
        );
        assert!(
            weekly_body.contains("start_date: $periodic_week_start_date")
                && weekly_body.contains("end_date: $periodic_week_end_date")
                && weekly_body.contains("start_week: $periodic_week_target")
                && weekly_body.contains("end_week: $periodic_week_target")
                && !weekly_body.contains("last_n_days: 7")
                && !weekly_body.contains("last_n_weeks: 1"),
            "periodic weekly must rebuild the exact completed ISO week, not a run-relative window"
        );
        let monthly_body = &yaml[monthly_stage..];
        assert!(
            monthly_body.find("workerName: memories-daily-summary-batch")
                < monthly_body.find("workerName: memories-weekly-summary-batch")
                && monthly_body.find("workerName: memories-weekly-summary-batch")
                    < monthly_body.find("workerName: memories-monthly-summary-batch"),
            "periodic monthly must generate daily and weekly prerequisites first"
        );
        assert!(
            monthly_body.contains("start_date: $periodic_month_start_date")
                && monthly_body.contains("end_date: $periodic_month_end_date")
                && monthly_body.contains("start_week: $periodic_month_start_week")
                && monthly_body.contains("end_week: $periodic_month_end_week")
                && monthly_body.contains("start_month: $periodic_month_target")
                && monthly_body.contains("end_month: $periodic_month_target")
                && !monthly_body.contains("last_n_days: 31")
                && !monthly_body.contains("last_n_weeks: 5")
                && !monthly_body.contains("last_n_months: 1"),
            "periodic monthly must rebuild the exact prior month and its covering ISO weeks"
        );
        assert!(
            yaml.contains("- resolvePeriodicMonthDependencyDateRange:")
                && yaml.contains("periodic_month_start_date: >-")
                && yaml.contains("$periodic_month_start_week + \"-1\""),
            "periodic monthly daily dependencies must start on the leading ISO week's Monday"
        );
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
    fn periodic_wrapper_root_probe_contract_is_structured() {
        let workflow = periodic_workflow_fixture();
        let codex_probe = workflow_step(&workflow, "probeCodexImportRoot");
        let claude_probe = workflow_step(&workflow, "probeClaudeCodeImportRoot");

        let codex_probe_if = yaml_field(codex_probe, "if").as_str().unwrap();
        assert!(codex_probe_if.contains(r#"index("codex")"#));
        assert!(codex_probe_if.contains("!= null"));
        let claude_probe_if = yaml_field(claude_probe, "if").as_str().unwrap();
        assert!(claude_probe_if.contains(r#"index("claude-code")"#));
        assert!(claude_probe_if.contains("!= null"));
        for (probe, root, export_key) in [
            (
                codex_probe,
                "$HOME/.codex/sessions",
                "codex_import_root_status",
            ),
            (
                claude_probe,
                "$HOME/.claude/projects",
                "claude_code_import_root_status",
            ),
        ] {
            let arguments = yaml_field(
                yaml_field(
                    yaml_field(yaml_field(probe, "run"), "function"),
                    "arguments",
                ),
                "args",
            )
            .as_sequence()
            .unwrap();
            assert_eq!(
                yaml_field(yaml_field(probe, "run"), "function")["arguments"]["command"],
                "/bin/sh"
            );
            assert!(arguments[1].as_str().unwrap().contains(root));
            assert!(arguments[1].as_str().unwrap().contains("printf present"));
            assert_eq!(
                yaml_field(yaml_field(probe, "export"), "as")[export_key],
                "${ .stdout }"
            );
        }

        let skipped = workflow_step(&workflow, "collectSkippedImportSources");
        assert!(
            yaml_field(skipped, "if")
                .as_str()
                .unwrap()
                .contains("regular")
        );
        assert!(
            yaml_field(yaml_field(skipped, "set"), "skipped_import_sources")
                .as_str()
                .unwrap()
                .contains("codex_import_root_status")
        );
        assert!(
            yaml_field(yaml_field(skipped, "set"), "skipped_import_sources")
                .as_str()
                .unwrap()
                .contains("claude_code_import_root_status")
        );

        let codex_import = workflow_step(&workflow, "runImportCodex");
        let claude_import = workflow_step(&workflow, "runImportClaudeCode");
        assert!(
            yaml_field(codex_import, "if")
                .as_str()
                .unwrap()
                .contains(r#"index("codex")"#)
        );
        assert!(
            yaml_field(codex_import, "if")
                .as_str()
                .unwrap()
                .contains(r#"(($codex_import_root_status // "") == "present")"#)
        );
        assert!(
            yaml_field(claude_import, "if")
                .as_str()
                .unwrap()
                .contains(r#"index("claude-code")"#)
        );
        assert!(
            yaml_field(claude_import, "if")
                .as_str()
                .unwrap()
                .contains(r#"(($claude_code_import_root_status // "") == "present")"#)
        );
        let skipped_expression = yaml_field(yaml_field(skipped, "set"), "skipped_import_sources")
            .as_str()
            .unwrap();
        assert!(skipped_expression.matches(r#"== "missing""#).count() >= 2);
        assert!(skipped_expression.contains(r#"then ["codex"] else [] end"#));
        assert!(skipped_expression.contains(r#"then ["claude-code"] else [] end"#));

        assert!(
            workflow_step_index(&workflow, "probeCodexImportRoot")
                < workflow_step_index(&workflow, "collectSkippedImportSources")
        );
        assert!(
            workflow_step_index(&workflow, "probeClaudeCodeImportRoot")
                < workflow_step_index(&workflow, "collectSkippedImportSources")
        );
        assert!(
            workflow_step_index(&workflow, "collectSkippedImportSources")
                < workflow_step_index(&workflow, "runImportCodex")
        );
        assert!(
            workflow_step_index(&workflow, "probeClaudeCodeImportRoot")
                < workflow_step_index(&workflow, "runImportClaudeCode")
        );

        let output = yaml_field(yaml_field(&workflow, "output"), "as");
        assert_eq!(
            yaml_field(output, "skipped_import_sources").as_str(),
            Some("${ $skipped_import_sources }")
        );
    }

    #[test]
    fn collect_skipped_import_sources_jq_evaluates_import_root_states() {
        assert_eq!(
            evaluate_skipped_import_sources(
                &["codex", "claude-code"],
                json!("present"),
                json!("present")
            ),
            json!([])
        );
        assert_eq!(
            evaluate_skipped_import_sources(
                &["codex", "claude-code"],
                json!("missing"),
                json!("present")
            ),
            json!(["codex"])
        );
        assert_eq!(
            evaluate_skipped_import_sources(
                &["codex", "claude-code"],
                json!("present"),
                json!("missing")
            ),
            json!(["claude-code"])
        );
        assert_eq!(
            evaluate_skipped_import_sources(
                &["codex", "claude-code"],
                json!("missing"),
                json!("missing")
            ),
            json!(["codex", "claude-code"])
        );
        assert_eq!(
            evaluate_skipped_import_sources(&["codex"], json!("present"), json!("missing")),
            json!([])
        );
        assert_eq!(
            evaluate_skipped_import_sources(&["claude-code"], json!("missing"), json!("missing")),
            json!(["claude-code"])
        );
        assert_eq!(
            evaluate_skipped_import_sources(
                &["codex", "claude-code"],
                serde_json::Value::Null,
                json!("present")
            ),
            json!([])
        );
        assert_eq!(
            evaluate_skipped_import_sources(&["codex", "claude-code"], json!(""), json!("present")),
            json!([])
        );
    }

    #[test]
    fn regular_daily_input_uses_current_day_range_for_zero_lookback_only() {
        let current_day = evaluate_regular_daily_input(0, "2026-08-11");
        assert_eq!(current_day["start_date"], "2026-08-11");
        assert_eq!(current_day["end_date"], "2026-08-11");
        assert_eq!(current_day["last_n_days"], 0);

        let previous_days = evaluate_regular_daily_input(1, "2026-08-10");
        assert_eq!(previous_days["start_date"], "");
        assert_eq!(previous_days["end_date"], "");
        assert_eq!(previous_days["last_n_days"], 1);
    }

    fn evaluate_skipped_import_sources(
        source_filter_sources: &[&str],
        codex_import_root_status: serde_json::Value,
        claude_code_import_root_status: serde_json::Value,
    ) -> serde_json::Value {
        let workflow = periodic_workflow_fixture();
        let skipped = workflow_step(&workflow, "collectSkippedImportSources");
        let expression = yaml_field(yaml_field(skipped, "set"), "skipped_import_sources")
            .as_str()
            .expect("skipped import expression must be a string")
            .trim()
            .strip_prefix("${")
            .and_then(|expression| expression.strip_suffix('}'))
            .expect("skipped import expression must be wrapped in ${ ... }")
            .trim();
        let values = BTreeMap::from([
            (
                "source_filter_sources".to_owned(),
                Arc::new(json!(source_filter_sources)),
            ),
            (
                "codex_import_root_status".to_owned(),
                Arc::new(codex_import_root_status),
            ),
            (
                "claude_code_import_root_status".to_owned(),
                Arc::new(claude_code_import_root_status),
            ),
            (
                "opencode_import_root_status".to_owned(),
                Arc::new(json!("missing")),
            ),
        ]);
        execute_jq(serde_json::Value::Null, expression, &values)
            .expect("collectSkippedImportSources jq expression must parse and evaluate")
    }

    fn evaluate_regular_daily_input(
        lookback_days: u16,
        regular_import_start_date: &str,
    ) -> serde_json::Value {
        let workflow = periodic_workflow_fixture();
        let daily = workflow_step(&workflow, "dailyForRegular");
        let run_daily = yaml_field(
            yaml_field(daily, "do")
                .as_sequence()
                .and_then(|steps| steps.first())
                .expect("daily stage must contain a batch step"),
            "runDailyBatch",
        );
        let input = yaml_field(
            yaml_field(yaml_field(run_daily, "run"), "function"),
            "arguments",
        )["input"]
            .as_str()
            .expect("daily batch input must be a jq expression")
            .trim();
        let expression = input
            .strip_prefix("${")
            .and_then(|value| value.strip_suffix('}'))
            .map(str::trim)
            .expect("daily batch input must use expression interpolation");
        let values = BTreeMap::from([
            (
                "workflow".to_owned(),
                Arc::new(json!({
                    "input": {
                        "task": { "lookback_days": lookback_days },
                        "runtime": {
                            "memories_grpc_host": "memories.example.test",
                            "memories_grpc_port": 9000,
                            "memories_grpc_tls": true,
                            "llm_worker_name": "memories-llm",
                            "output_language": "ja"
                        }
                    }
                })),
            ),
            (
                "regular_import_start_date".to_owned(),
                Arc::new(json!(regular_import_start_date)),
            ),
            ("source_filter_labels".to_owned(), Arc::new(json!([]))),
            (
                "source_filter_match_mode".to_owned(),
                Arc::new(json!("LABEL_ALL")),
            ),
            (
                "source_scope_digest".to_owned(),
                Arc::new(json!("source-scope-digest")),
            ),
            (
                "generation_id".to_owned(),
                Arc::new(json!("periodic-generation-1")),
            ),
            (
                "generation_started_at".to_owned(),
                Arc::new(json!("2026-09-03T07:06:32Z")),
            ),
        ]);
        let encoded = execute_jq(serde_json::Value::Null, expression, &values)
            .expect("daily batch input expression must parse and evaluate");
        serde_json::from_str(
            encoded
                .as_str()
                .expect("daily batch input expression must encode JSON"),
        )
        .expect("daily batch input must be valid JSON")
    }

    fn periodic_workflow_fixture() -> serde_yaml::Value {
        serde_yaml::from_str(include_str!(
            "../../../workers/workflows/lookback-periodic-run.yaml"
        ))
        .expect("periodic workflow must remain valid YAML")
    }

    fn yaml_field<'a>(value: &'a serde_yaml::Value, key: &str) -> &'a serde_yaml::Value {
        value
            .as_mapping()
            .and_then(|mapping| mapping.get(serde_yaml::Value::String(key.to_owned())))
            .unwrap_or_else(|| panic!("missing YAML field {key}"))
    }

    fn workflow_step<'a>(workflow: &'a serde_yaml::Value, name: &str) -> &'a serde_yaml::Value {
        workflow["do"]
            .as_sequence()
            .and_then(|steps| {
                steps.iter().find_map(|step| {
                    step.as_mapping()
                        .and_then(|mapping| mapping.get(serde_yaml::Value::String(name.to_owned())))
                })
            })
            .unwrap_or_else(|| panic!("missing workflow step {name}"))
    }

    fn workflow_step_index(workflow: &serde_yaml::Value, name: &str) -> usize {
        workflow["do"]
            .as_sequence()
            .and_then(|steps| {
                steps.iter().position(|step| {
                    step.as_mapping().is_some_and(|mapping| {
                        mapping.contains_key(serde_yaml::Value::String(name.to_owned()))
                    })
                })
            })
            .unwrap_or_else(|| panic!("missing workflow step {name}"))
    }

    #[test]
    fn default_periodic_tasks_match_requested_disabled_templates() {
        let defaults = default_periodic_tasks();
        assert_eq!(defaults.len(), 3);

        let daily = &defaults[0];
        assert_eq!(daily.name, "Daily import and summaries");
        assert_eq!(daily.source, "codex+claude-code+opencode");
        assert_eq!(
            daily.sources,
            vec![
                SOURCE_CODEX.to_string(),
                SOURCE_CLAUDE_CODE.to_string(),
                SOURCE_OPENCODE.to_string()
            ]
        );
        assert_eq!(daily.task_kind, PeriodicTaskKind::Regular);
        assert_eq!(daily.interval_days, Some(1));
        assert_eq!(daily.hour, 0);
        assert_eq!(daily.minute, 0);
        assert_eq!(daily.lookback_days, 1);
        assert!(daily.force_thread_summary);
        assert!(daily.run_summary_thread_group);
        assert!(daily.run_summary_daily);
        assert!(!daily.run_personality);
        assert!(!daily.run_reflection);
        assert_eq!(crontab_for_task(daily).unwrap(), "0 0 0 */1 * *");

        let weekly = &defaults[1];
        assert_eq!(weekly.name, "Weekly summary");
        assert_eq!(weekly.task_kind, PeriodicTaskKind::Weekly);
        assert_eq!(weekly.weekly_day, Some(1));
        assert_eq!(weekly.hour, 1);
        assert!(!weekly.run_summary_thread_group);
        assert_eq!(crontab_for_task(weekly).unwrap(), "0 0 1 * * 1");

        let monthly = &defaults[2];
        assert_eq!(monthly.name, "Monthly summary");
        assert_eq!(monthly.task_kind, PeriodicTaskKind::Monthly);
        assert_eq!(monthly.monthly_day, Some(1));
        assert_eq!(monthly.hour, 2);
        assert!(!monthly.run_summary_thread_group);
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

        assert!(weekly.contains("source_thread_labels_override"));
        assert!(weekly.contains("\"summary_scope:path\""));
        assert!(weekly.contains("daily_thread_filter_match_mode"));
        assert!(
            !weekly.contains("[$workflow.input.daily_label] + $sorted_extra_labels"),
            "weekly input must not match combined daily summaries via source labels"
        );
        assert!(
            !weekly.contains("then $sorted_extra_labels"),
            "weekly combined input must not include single-source daily summaries"
        );
        assert!(monthly.contains("source_thread_labels_override"));
        assert!(monthly.contains("\"summary_scope:path\""));
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
    fn validation_accepts_zero_lookback_for_current_day() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        t.lookback_days = 0;
        assert!(validate_task(&t).is_ok());
    }

    #[test]
    fn validation_rejects_missing_source() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        t.source.clear();
        t.sources.clear();
        assert_eq!(
            config_message(validate_task(&t)),
            crate::error_messages::VALIDATION_SOURCE_REQUIRED
        );
    }

    #[test]
    fn validation_rejects_missing_name() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        t.name.clear();

        assert_eq!(
            config_message(validate_task(&t)),
            crate::error_messages::VALIDATION_NAME_REQUIRED
        );
    }

    #[test]
    fn validation_rejects_plain_source_for_periodic_ui_contract() {
        let mut t = task(PeriodicTaskKind::Regular);
        t.interval_hours = Some(24);
        t.source = "plain".into();
        t.sources = vec!["plain".into()];

        assert_eq!(
            config_message(validate_task(&t)),
            crate::error_messages::VALIDATION_SOURCE_INVALID
        );
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
    fn thread_group_flag_is_backward_compatible_by_task_kind_and_force_thread_summary() {
        let legacy = |task_kind: &str, force_thread_summary: bool| {
            serde_json::from_value::<PeriodicTaskArgs>(serde_json::json!({
                "name": "legacy",
                "source": "codex",
                "task_kind": task_kind,
                "hour": 9,
                "minute": 0,
                "interval_hours": 24,
                "lookback_days": 7,
                "force_thread_summary": force_thread_summary,
            }))
            .unwrap()
        };

        assert!(legacy("regular", true).run_summary_thread_group);
        assert!(!legacy("regular", false).run_summary_thread_group);
        assert!(!legacy("weekly", true).run_summary_thread_group);
        assert!(!legacy("monthly", true).run_summary_thread_group);
    }

    #[test]
    fn stored_tasks_with_the_removed_replay_option_still_load() {
        let task: PeriodicTaskArgs = serde_json::from_value(serde_json::json!({
            "name": "stored",
            "source": "codex",
            "task_kind": "regular",
            "hour": 9,
            "minute": 0,
            "interval_hours": 24,
            "lookback_days": 1,
            "force_thread_summary": true,
            "include_incomplete_past_periods": true,
        }))
        .unwrap();

        let saved = serde_json::to_value(&task).unwrap();
        assert!(saved.get("include_incomplete_past_periods").is_none());
    }

    #[test]
    fn periodic_workflow_no_longer_replays_or_reads_recovery_state() {
        let yaml = include_str!("../../../workers/workflows/lookback-periodic-run.yaml");

        for removed in [
            "include_incomplete_past_periods",
            "summary_recovery_state_path",
            "replayIncompletePeriods",
            "loadIncompleteRecoveryPeriods",
            "replay_daily",
        ] {
            assert!(
                !yaml.contains(removed),
                "{removed} is still in the workflow"
            );
        }
        let workflow = periodic_workflow_fixture();
        let output = yaml_field(yaml_field(&workflow, "output"), "as");
        for removed in [
            "memories_endpoint_id",
            "generation_id",
            "generation_started_at",
        ] {
            assert!(
                output.get(removed).is_none(),
                "{removed} is still in the workflow output"
            );
        }
    }

    #[test]
    fn explicit_thread_group_flag_wins_over_legacy_default() {
        let task: PeriodicTaskArgs = serde_json::from_value(serde_json::json!({
            "name": "explicit",
            "source": "codex",
            "task_kind": "weekly",
            "hour": 9,
            "minute": 0,
            "weekly_day": 1,
            "lookback_days": 7,
            "force_thread_summary": true,
            "run_summary_thread_group": true,
        }))
        .unwrap();

        assert!(task.run_summary_thread_group);
    }

    #[test]
    fn thread_group_requires_thread_summary() {
        let mut task = task(PeriodicTaskKind::Weekly);
        task.weekly_day = Some(1);
        task.force_thread_summary = false;
        task.run_summary_thread_group = true;

        assert_eq!(
            config_message(validate_task(&task)),
            "run_summary_thread_group requires force_thread_summary"
        );
    }

    #[test]
    fn periodic_workflow_runs_thread_group_before_period_output_without_replay() {
        let workflow = periodic_workflow_fixture();
        let thread_group = workflow_step(&workflow, "threadGroupForPeriodic");
        let thread_summary = workflow_step(&workflow, "threadSummaryForTask");

        assert!(
            yaml_field(thread_group, "if")
                .as_str()
                .unwrap()
                .contains("run_summary_thread_group")
        );
        assert!(
            yaml_field(thread_summary, "if")
                .as_str()
                .unwrap()
                .contains("force_thread_summary")
        );
        assert!(
            workflow_step_index(&workflow, "threadSummaryForTask")
                < workflow_step_index(&workflow, "threadGroupForPeriodic")
        );
        assert!(
            workflow_step_index(&workflow, "threadGroupForPeriodic")
                < workflow_step_index(&workflow, "dailyForRegular")
        );
        assert!(
            workflow_step_index(&workflow, "threadGroupForPeriodic")
                < workflow_step_index(&workflow, "weeklyStage")
        );
        assert!(
            workflow_step_index(&workflow, "threadGroupForPeriodic")
                < workflow_step_index(&workflow, "monthlyStage")
        );

        let output = yaml_field(yaml_field(&workflow, "output"), "as");
        assert_eq!(
            yaml_field(output, "thread_group").as_str(),
            Some("${ $thread_group_result }")
        );
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
