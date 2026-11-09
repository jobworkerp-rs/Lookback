pub mod analysis_dispatch;
pub mod app_settings;
pub mod apply_settings;
pub mod background_jobs;
pub mod chat;
pub(crate) mod client_cache;
pub mod coding_agent;
#[cfg(test)]
pub(crate) mod concurrency_probe;
pub mod connection;
pub mod embedding_management;
pub mod embedding_presets;
pub mod embedding_settings;
pub mod embedding_space;
pub mod embedding_workers_yaml;
pub mod embeddings;
pub mod import;
pub mod issue_auto_policy;
pub(crate) mod issue_chat_mutations;
pub mod issue_chat_recovery;
pub(crate) mod issue_resolution_diagnostics;
pub mod issue_review_policy;
pub mod issue_tracker_settings;
pub mod issue_work_policy;
pub mod issues;
pub mod llm_presets;
pub mod llm_settings;
pub mod logs;
pub mod mcp_settings;
pub(crate) mod memories_rpc_support;
pub mod model;
pub mod native_terminal;
pub mod periodic_execution;
pub mod periodic_tasks;
pub mod personality;
pub mod profile_terminal;
pub mod recovery;
pub mod reflection_dispatch;
pub mod reflections;
pub mod search;
pub mod search_index_maintenance;
pub mod settings;
pub mod setup;
pub(crate) mod staged_progress;
pub mod summaries;
pub(crate) mod summary_calendar;
pub mod summary_recovery;
pub(crate) mod thread_group_owner;
pub(crate) mod thread_group_recovery;
pub mod thread_groups;
pub mod threads;
pub mod ui_diagnostics;
pub mod window_lifecycle;

use std::collections::HashMap;
use std::fmt::Display;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use jobworkerp_client::jobworkerp::data::JobId;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;
use tracing::warn;

use crate::data::DataPaths;
use crate::error::{AppError, AppResult};
use crate::grpc;
use crate::sidecar::{SidecarEndpoints, Sidecars};

/// Production `env_lookup` for `resolve_*_with_env`-style projectors.
/// Centralised so every "the sidecar is actually launching right now"
/// caller wires the same `std::env::var` source, instead of repeating
/// the closure at each site (lifecycle, set_embedding_settings,
/// embedding_identity, …) and risking one of them silently going
/// env-blind.
pub(crate) fn process_env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Stage plugins before a command-triggered sidecar restart.
///
/// Plugin artifacts are optional for the sidecars' browse-only paths, so a
/// staging failure must be reported as a startup warning instead of stopping
/// a settings save or its rollback restart.
pub(crate) fn stage_plugins_for_restart(
    app: &AppHandle,
    plugins_dir: &Path,
) -> Vec<crate::sidecar::SidecarWarning> {
    plugin_staging_warnings(crate::plugins::stage_plugins(app, plugins_dir))
}

fn plugin_staging_warnings<T, E: Display>(
    stage_result: Result<T, E>,
) -> Vec<crate::sidecar::SidecarWarning> {
    match stage_result {
        Ok(_) => Vec::new(),
        Err(error) => vec![crate::sidecar::SidecarWarning {
            kind: crate::sidecar::SidecarWarningKind::PluginsStageFailed,
            message: error.to_string(),
            detail: None,
        }],
    }
}

/// Validate a HuggingFace `org/name` repo identifier. Hand-rolled (no
/// regex dep): two non-empty `[A-Za-z0-9_.-]` segments separated by
/// exactly one `/`. Shared by `llm_settings` (LLM Custom row) and
/// `embedding_settings` (Embedding Custom row); a future HF rule
/// change should only need to edit this one site.
pub(crate) fn is_valid_hf_repo(s: &str) -> bool {
    let (org, name) = match s.split_once('/') {
        Some(parts) => parts,
        None => return false,
    };
    if org.is_empty() || name.is_empty() {
        return false;
    }
    if name.contains('/') {
        return false;
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    org.chars().all(allowed) && name.chars().all(allowed)
}

/// Output languages the generation workers are registered for. Mirrors
/// memories' `SUPPORTED_LANGUAGES` (`agent-chat-import/src/common/language.rs`).
pub(super) const SUPPORTED_LANGUAGES: [&str; 2] = ["ja", "en"];

/// The default generation language when neither an explicit request value nor
/// a persisted setting is usable.
pub(super) const DEFAULT_OUTPUT_LANGUAGE: &str = "ja";

/// Resolve the output language for a generation dispatch.
///
/// Order (multilingual-generation-spec §1):
/// 1. `explicit` — the value the frontend passes per dispatch (immediate
///    reflection of the UI locale).
/// 2. `persisted` — `app-settings.json`'s `output_language`, used by headless
///    paths (conductor periodic runs) that never see the frontend.
/// 3. `"ja"`.
///
/// Unsupported values are ignored (whitelist), so a stale or hand-edited
/// setting can't route to a worker that was never registered.
pub(super) fn resolve_output_language(explicit: Option<&str>, persisted: Option<&str>) -> String {
    let pick = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|s| SUPPORTED_LANGUAGES.contains(s))
            .map(str::to_string)
    };
    pick(explicit)
        .or_else(|| pick(persisted))
        .unwrap_or_else(|| DEFAULT_OUTPUT_LANGUAGE.to_string())
}

/// Resolve the persisted output language from `AppSettings`. `pub(crate)` so
/// the sidecar lifecycle (outside `commands`) can bake it into the conductor
/// periodic scheduler args at refresh time.
pub(crate) fn resolve_periodic_output_language(
    settings: &crate::data::paths::AppSettings,
) -> String {
    resolve_output_language(None, settings.output_language.as_deref())
}

/// Parse the trailing i64 from external ids shaped like `<prefix><id>`
/// (e.g. `summary:42`, `personality_profile:1`). Returns `None` for
/// malformed values so callers can fall back to displaying without the
/// cross-link.
pub(super) fn parse_i64_after_prefix(prefix: &str, ext: Option<&str>) -> Option<i64> {
    ext?.strip_prefix(prefix)?.parse::<i64>().ok()
}

/// Wrap a workflow input object into the WORKFLOW runner's `run`-method args
/// (`WorkflowRunArgs`). Its `input` field is a *string* carrying the workflow
/// input as JSON, NOT a nested object: passing the bare object lets
/// `enqueue_stream_with_json` drop every field (none map onto
/// `WorkflowRunArgs`), leaving `input` empty so the workflow fails schema
/// validation with `instance: String("")` (expected object). `workflow_url` /
/// `workflow_data` are pre-set in the worker's `WorkflowRunnerSettings`, so
/// only `input` is supplied here. Pure so the wire-shape is unit-tested.
/// Shared by the import pipeline and the manual reflection dispatch so both
/// paths produce the identical wire shape.
pub(super) fn wrap_workflow_run_args(input: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "input": input.to_string() })
}

/// Emit a Tauri event and log when the bridge fails (rare, but the
/// silent `let _ = app.emit(...)` pattern made the previous step
/// regression invisible for half an hour during e2e).
pub(crate) fn emit_event<P: Serialize + Clone>(app: &AppHandle, event: &str, payload: P) {
    if let Err(e) = app.emit(event, payload) {
        warn!(error = %e, event, "failed to emit Tauri event");
    }
}

pub(super) const GENERATED_REFRESH_EVENT: &str = "generated://refresh";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum GeneratedRefreshScope {
    ThreadSummary,
    ThreadGroupSummary,
    DailySummary,
    WeeklySummary,
    MonthlySummary,
    Personality,
    Reflection,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct GeneratedRefreshUpdate {
    pub job_id: String,
    pub scopes: Vec<GeneratedRefreshScope>,
}

pub(super) fn emit_generated_refresh(
    app: &AppHandle,
    job_id: &str,
    scopes: Vec<GeneratedRefreshScope>,
) {
    if scopes.is_empty() {
        return;
    }
    emit_event(
        app,
        GENERATED_REFRESH_EVENT,
        GeneratedRefreshUpdate {
            job_id: job_id.to_string(),
            scopes,
        },
    );
}

/// Return the generated data scopes persisted by a workflow progress chunk.
///
/// A `position` is the stable workflow-engine breadcrumb emitted after each
/// YAML step. Keeping the mapping here gives every streaming dispatch the
/// same immediate-refresh behavior while terminal refreshes remain a safety
/// net for chunks that are not observed by the client.
pub(super) fn generated_refresh_scopes_from_position(raw: &str) -> Vec<GeneratedRefreshScope> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    let position = v.get("position").and_then(|p| p.as_str()).unwrap_or("");
    let has_step = |step| position.split('/').any(|segment| segment == step);
    if has_step("summarizeEach") && (has_step("recordGenerated") || has_step("recordSuccess")) {
        vec![GeneratedRefreshScope::ThreadSummary]
    } else if has_step("reflectEach") && has_step("recordSuccess") {
        vec![GeneratedRefreshScope::Reflection]
    } else if (has_step("personalityEach") && has_step("recordSignalOutcome"))
        || has_step("updateProfile")
        || has_step("addProfileMemory")
    {
        vec![GeneratedRefreshScope::Personality]
    } else {
        Vec::new()
    }
}

/// Status of a single step in a staged dispatch (import, summary generation,
/// or reflection). Shared so the frontend gets one `StepStatus` string-enum
/// to switch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepStatus {
    Waiting,
    Active,
    Done,
    /// Workflow finished successfully (no fatal error) but some items
    /// failed — e.g. a thread-summary batch where the LLM rate-limited
    /// part of the inputs. Surfaced distinctly from `Done` so the toast
    /// can render a warning badge instead of the green "all good" check.
    Warning,
    Failed,
    /// The user requested cancellation. This is terminal, but distinct from
    /// a worker failure so staged operations can report an intentional interruption.
    Cancelled,
}

/// The terminal state and optional detail for a dispatch step.
///
/// Classifiers remain with their respective workflows because their terminal
/// contracts differ, while event consumers share this stable payload shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StepOutcome {
    pub status: StepStatus,
    pub message: Option<String>,
}

/// Coerce a JSON integer or a trimmed integer string to `i64`.
///
/// Workflow engines can serialize jq-produced counters in either form.
pub(super) fn json_as_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

/// Tauri-managed state container. Held inside `AppHandle::state()`.
///
/// The gRPC clients are cached behind `Mutex<Option<..>>` (not `OnceCell`)
/// so a sidecar restart can invalidate them: ports are re-selected on each
/// start, and even an unchanged port gets a fresh TCP connection,
/// so a cached client would otherwise keep pointing at the stopped sidecar.
/// `retry_model_setup` calls `invalidate_clients()` around the restart.
/// See `AppState::periodic_targets`.
pub(crate) struct PeriodicTargets {
    pub conductor_url: String,
    pub conductor_identity: String,
    pub jobworkerp: connection::GrpcAddress,
    pub services: periodic_tasks::PeriodicServices,
}

pub struct AppState {
    pub sidecars: Arc<Sidecars>,
    pub data: DataPaths,
    /// Durable coding-agent state is app-owned and independent from UI
    /// subscriptions or the generic dispatch-in-flight registry.
    pub coding_agent_run_store: Arc<crate::coding_agent::RunStore>,
    pub coding_agent_coordinator: Arc<crate::coding_agent::CodingAgentCoordinator>,
    pub coding_agent_execution: Arc<crate::coding_agent::native_execution::NativeExecutionAdapter>,
    pub coding_agent_settings: Arc<crate::coding_agent::settings::CodingAgentSettingsStore>,
    pub coding_agent_backend: Arc<crate::coding_agent::backend::CodingAgentBackend>,
    pub native_terminal_registry: Arc<crate::native_terminal::registry::NativeRuntimeRegistry>,
    pub native_terminal_admission: Arc<tokio::sync::Mutex<()>>,
    pub coding_agent_accepting: Arc<AtomicBool>,
    pub coding_agent_shutdown_lock: tokio::sync::Mutex<()>,
    pub window_lifecycle: crate::window_lifecycle::WindowLifecycle,
    coding_agent_tray_icon: parking_lot::Mutex<Option<tauri::tray::TrayIcon>>,
    pub coding_agent_run_owners: coding_agent::CodingAgentRunOwnerRegistry,
    /// Shared with the sidecar lifecycle, which updates it after Issue tracker
    /// worker registration and health checks complete.
    pub issue_tracker_availability: Arc<IssueTrackerAvailability>,
    memories_channel: Mutex<Option<Channel>>,
    jobworkerp: Mutex<Option<crate::jobworkerp::JobworkerpHandle>>,
    /// Always-local jobworkerp client for observability polls; separate from
    /// `jobworkerp`, which remote browse mode redirects to the remote service.
    local_jobworkerp: client_cache::UrlKeyedClientCache<crate::jobworkerp::JobworkerpHandle>,
    conductor_channel: client_cache::UrlKeyedClientCache<Channel>,
    unsupported_memories_rpcs: memories_rpc_support::UnsupportedMemoriesRpcs,
    /// Invalidates embedding registration waits and in-flight dispatches when
    /// cached endpoints are replaced or the application begins shutting down.
    embedding_connection_epoch: AtomicU64,
    /// Local-only LanceDB maintenance admission and persistent state.
    pub search_index_maintenance:
        crate::search_index_maintenance::SharedSearchIndexMaintenanceCoordinator,
    /// Serializes restore and retry commands within this process. The
    /// instance lock only excludes other processes and cannot protect the
    /// shared migration staging path from concurrent Tauri invocations.
    pub database_migration_recovery: Mutex<()>,
    /// Serializes the read-modify-write cycle for the durable summary
    /// recovery state. File-level atomic replacement alone cannot protect two
    /// concurrent in-process writers from losing each other's records.
    pub summary_recovery: Mutex<()>,
    /// Serialises durable ThreadGroup summary recovery writes.
    pub thread_group_recovery: Mutex<()>,
    /// Latest in-process staged snapshots, isolated by operation kind and job
    /// ID, for webview hydration.
    pub import_progress: Arc<import::ImportProgressRegistry>,
    /// In-flight long-running dispatches keyed by the UI-side dispatch id.
    /// Shared by the RAG chat agent loop (`chat://step`), the import
    /// pipeline (`staged://step`), and the analysis-tab dispatches
    /// (`summary://step`, `personality://step`). Each entry carries a
    /// cancellation token (so the driver bails out between steps) and the
    /// currently-running jobworkerp `JobId` so the cancel command can
    /// issue `JobService/Delete` against the live job — see
    /// OPEN-CHAT-2 / DECIDE-CHAT-4 for the chat origin and
    /// `plans/import-workflow-jobservice-delete-job-id-joyful-badger.md`
    /// for the generalisation to import/analysis.
    dispatch_in_flight: Mutex<HashMap<String, DispatchCancelEntry>>,
    /// Serializes the admission of a new long-running dispatch with an LLM
    /// settings restart. The settings path holds this across its empty-check
    /// and sidecar replacement; registration holds it only while inserting.
    llm_settings_dispatch_gate: Arc<Mutex<()>>,
    /// Closes direct memories-write admission while an embedding migration is
    /// quiescing writers. This gate also serializes the close with begin_write.
    embedding_write_admission_gate: Arc<Mutex<()>>,
    embedding_migration_admission: AtomicBool,
}

/// Per-dispatch cancellation handle stored in
/// [`AppState::dispatch_in_flight`]. Cloned out of the map so the
/// background driver task and the cancel command can each hold one
/// without serialising on the outer mutex. Shared between chat / import /
/// analysis — keys never collide because they are UUIDs / timestamped
/// ids the frontend already treats as globally unique.
#[derive(Clone)]
pub struct DispatchCancelEntry {
    pub token: CancellationToken,
    /// Currently-running jobworkerp job — set by the driver on every
    /// hop / step dispatch and cleared on completion. The cancel command
    /// `take()`s it to issue `JobService/Delete` against the running job
    /// so the server-side LLM / workflow releases its GPU slot
    /// immediately rather than running to natural completion.
    pub current_job_id: Arc<Mutex<Option<JobId>>>,
    /// Optional label of the step currently executing. Used by the
    /// import pipeline so a cancel arriving mid-step can decide which
    /// downstream steps still need a "skipped" emit. Chat leaves it
    /// `None` — there is only one hop running at a time.
    pub current_step: Arc<Mutex<Option<String>>>,
}

impl DispatchCancelEntry {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            current_job_id: Arc::new(Mutex::new(None)),
            current_step: Arc::new(Mutex::new(None)),
        }
    }
}

/// Stable error text shared by both LLM-settings mutation commands. The UI
/// receives this only when replacing the shared LLM worker would interrupt a
/// live generation.
pub(crate) const LLM_SETTINGS_CHANGE_WHILE_GENERATING_ERROR: &str = "cannot change LLM settings while a generation is running; wait for it to finish or cancel it first";

/// Availability of the private local Issue tracker worker.
///
/// The primary endpoint remains usable for reads and recovery while Gateway
/// registration or normal mutation admission is temporarily unavailable.
#[derive(Debug)]
pub struct IssueTrackerAvailability {
    state: parking_lot::Mutex<IssueTrackerAvailabilityState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IssueTrackerAvailabilityState {
    endpoint_connected: bool,
    gateway_ready: bool,
    mutation_ready: bool,
    reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct IssueTrackerAvailabilitySnapshot {
    pub endpoint_connected: bool,
    pub gateway_ready: bool,
    pub mutation_ready: bool,
    pub reason: Option<String>,
}

impl Default for IssueTrackerAvailability {
    fn default() -> Self {
        Self {
            state: parking_lot::Mutex::new(IssueTrackerAvailabilityState {
                endpoint_connected: false,
                gateway_ready: false,
                mutation_ready: false,
                reason: Some("Issue tracker worker is not ready".to_string()),
            }),
        }
    }
}

impl IssueTrackerAvailability {
    pub fn mark_endpoint_connected(&self) {
        let mut state = self.state.lock();
        state.endpoint_connected = true;
        state.reason = None;
    }

    pub fn mark_endpoint_unavailable(&self, reason: impl Into<String>) {
        let mut state = self.state.lock();
        state.endpoint_connected = false;
        state.gateway_ready = false;
        state.mutation_ready = false;
        state.reason = Some(reason.into());
    }

    pub fn mark_gateway_ready(&self) {
        let mut state = self.state.lock();
        state.endpoint_connected = true;
        state.gateway_ready = true;
        state.reason = None;
    }

    pub fn mark_gateway_unavailable(&self, reason: impl Into<String>) {
        let mut state = self.state.lock();
        state.gateway_ready = false;
        state.reason = Some(reason.into());
    }

    pub fn mark_mutation_ready(&self) {
        let mut state = self.state.lock();
        state.endpoint_connected = true;
        state.mutation_ready = true;
        state.reason = None;
    }

    pub fn mark_mutation_unavailable(&self, reason: impl Into<String>) {
        let mut state = self.state.lock();
        state.mutation_ready = false;
        state.reason = Some(reason.into());
    }

    pub fn mark_ready(&self) {
        *self.state.lock() = IssueTrackerAvailabilityState {
            endpoint_connected: true,
            gateway_ready: true,
            mutation_ready: true,
            reason: None,
        };
    }

    pub fn mark_unavailable(&self, reason: impl Into<String>) {
        self.mark_endpoint_unavailable(reason);
    }

    pub fn require_endpoint_connected(&self) -> AppResult<()> {
        let state = self.state.lock();
        if state.endpoint_connected {
            Ok(())
        } else {
            Err(AppError::SidecarNotReady(format!(
                "Issue tracker endpoint is unavailable: {}",
                state.reason.as_deref().unwrap_or("unknown reason")
            )))
        }
    }

    pub fn require_gateway_ready(&self) -> AppResult<()> {
        let state = self.state.lock();
        if state.gateway_ready {
            Ok(())
        } else {
            Err(AppError::SidecarNotReady(format!(
                "Issue tracker Gateway is unavailable: {}",
                state.reason.as_deref().unwrap_or("unknown reason")
            )))
        }
    }

    pub fn require_ready(&self) -> AppResult<()> {
        self.require_endpoint_connected()
    }

    pub fn snapshot(&self) -> IssueTrackerAvailabilitySnapshot {
        let state = self.state.lock();
        IssueTrackerAvailabilitySnapshot {
            endpoint_connected: state.endpoint_connected,
            gateway_ready: state.gateway_ready,
            mutation_ready: state.mutation_ready,
            reason: state.reason.clone(),
        }
    }
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingAgentShutdownError {
    Storage,
    RunUnavailable,
    SidecarStop,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodingAgentShutdownRun {
    pub run_id: String,
    pub revision: u64,
    pub state: crate::coding_agent::RunState,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodingAgentShutdownOutcome {
    pub cancellation_attempts: usize,
    pub unconfirmed_run_ids: Vec<String>,
    pub unconfirmed_native_session_ids: Vec<String>,
}

#[doc(hidden)]
#[async_trait::async_trait]
// async_trait makes each async Result method return a must-use boxed future.
#[allow(clippy::double_must_use)]
pub trait CodingAgentShutdownOperations: Send + Sync {
    fn recoverable_runs(&self) -> Result<Vec<CodingAgentShutdownRun>, CodingAgentShutdownError>;
    async fn cancel(&self, run_id: &str) -> Result<(), CodingAgentShutdownError>;
    async fn reconcile(&self, run_id: &str) -> Result<(), CodingAgentShutdownError>;
    fn current_run(&self, run_id: &str)
    -> Result<CodingAgentShutdownRun, CodingAgentShutdownError>;
    fn persist_unknown(
        &self,
        run: &CodingAgentShutdownRun,
    ) -> Result<CodingAgentShutdownRun, CodingAgentShutdownError>;
}

#[doc(hidden)]
pub async fn reconcile_coding_agent_shutdown_runs(
    operations: &impl CodingAgentShutdownOperations,
    deadline: tokio::time::Instant,
) -> Result<CodingAgentShutdownOutcome, CodingAgentShutdownError> {
    let mut outcome = CodingAgentShutdownOutcome {
        cancellation_attempts: 0,
        unconfirmed_run_ids: Vec::new(),
        unconfirmed_native_session_ids: Vec::new(),
    };
    for run in operations.recoverable_runs()? {
        if shutdown_run_is_quiescent(run.state) {
            continue;
        }
        if tokio::time::Instant::now() < deadline {
            outcome.cancellation_attempts += 1;
            let _ = tokio::time::timeout_at(deadline, operations.cancel(&run.run_id)).await;
            if tokio::time::Instant::now() < deadline {
                let _ = tokio::time::timeout_at(deadline, operations.reconcile(&run.run_id)).await;
            }
        }
        let current = operations.current_run(&run.run_id)?;
        if shutdown_run_is_quiescent(current.state) {
            continue;
        }
        let current = if current.state == crate::coding_agent::RunState::Unknown {
            current
        } else {
            operations.persist_unknown(&current)?
        };
        // A terminal state may win between the first read and the CAS used to
        // persist Unknown. It is already reconciled; do not turn that race into
        // a false storage failure or overwrite its evidence.
        if shutdown_run_is_quiescent(current.state) {
            continue;
        }
        if current.state != crate::coding_agent::RunState::Unknown {
            return Err(CodingAgentShutdownError::Storage);
        }
        outcome.unconfirmed_run_ids.push(current.run_id);
    }
    outcome.unconfirmed_run_ids.sort();
    outcome.unconfirmed_run_ids.dedup();
    Ok(outcome)
}

#[doc(hidden)]
pub async fn reconcile_coding_agent_shutdown_then_stop<O, F, Fut>(
    operations: &O,
    deadline: tokio::time::Instant,
    stop_sidecars: F,
) -> Result<CodingAgentShutdownOutcome, CodingAgentShutdownError>
where
    O: CodingAgentShutdownOperations,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), CodingAgentShutdownError>>,
{
    let outcome = reconcile_coding_agent_shutdown_runs(operations, deadline).await?;
    stop_sidecars().await?;
    Ok(outcome)
}

fn shutdown_run_is_quiescent(state: crate::coding_agent::RunState) -> bool {
    state.is_terminal()
        || matches!(
            state,
            crate::coding_agent::RunState::ApprovalPending
                | crate::coding_agent::RunState::Queued
                | crate::coding_agent::RunState::AwaitingReview
        )
}

struct CoordinatorShutdownOperations<'a> {
    coordinator: &'a crate::coding_agent::CodingAgentCoordinator,
    store: &'a crate::coding_agent::RunStore,
}

#[async_trait::async_trait]
impl CodingAgentShutdownOperations for CoordinatorShutdownOperations<'_> {
    fn recoverable_runs(&self) -> Result<Vec<CodingAgentShutdownRun>, CodingAgentShutdownError> {
        self.coordinator
            .recoverable_runs()
            .map(|runs| runs.iter().map(shutdown_run_snapshot).collect())
            .map_err(|_| CodingAgentShutdownError::Storage)
    }

    async fn cancel(&self, run_id: &str) -> Result<(), CodingAgentShutdownError> {
        let run_id = crate::coding_agent::RunId::new(run_id.to_owned())
            .map_err(|_| CodingAgentShutdownError::RunUnavailable)?;
        self.coordinator
            .cancel(&run_id)
            .await
            .map(|_| ())
            .map_err(|_| CodingAgentShutdownError::RunUnavailable)
    }

    async fn reconcile(&self, run_id: &str) -> Result<(), CodingAgentShutdownError> {
        let run_id = crate::coding_agent::RunId::new(run_id.to_owned())
            .map_err(|_| CodingAgentShutdownError::RunUnavailable)?;
        self.coordinator
            .reconcile(&run_id)
            .await
            .map(|_| ())
            .map_err(|_| CodingAgentShutdownError::RunUnavailable)
    }

    fn current_run(
        &self,
        run_id: &str,
    ) -> Result<CodingAgentShutdownRun, CodingAgentShutdownError> {
        let run_id = crate::coding_agent::RunId::new(run_id.to_owned())
            .map_err(|_| CodingAgentShutdownError::RunUnavailable)?;
        self.coordinator
            .get_run(&run_id)
            .map_err(|_| CodingAgentShutdownError::Storage)?
            .map(|run| shutdown_run_snapshot(&run))
            .ok_or(CodingAgentShutdownError::RunUnavailable)
    }

    fn persist_unknown(
        &self,
        run: &CodingAgentShutdownRun,
    ) -> Result<CodingAgentShutdownRun, CodingAgentShutdownError> {
        let run_id = crate::coding_agent::RunId::new(run.run_id.clone())
            .map_err(|_| CodingAgentShutdownError::RunUnavailable)?;
        for _ in 0..3 {
            let current = self
                .coordinator
                .get_run(&run_id)
                .map_err(|_| CodingAgentShutdownError::Storage)?
                .ok_or(CodingAgentShutdownError::RunUnavailable)?;
            if shutdown_run_is_quiescent(current.state)
                || current.state == crate::coding_agent::RunState::Unknown
            {
                return Ok(shutdown_run_snapshot(&current));
            }
            match self.store.transition(
                &run_id,
                current.revision,
                crate::coding_agent::RunState::Unknown,
            ) {
                Ok(updated) => return Ok(shutdown_run_snapshot(&updated)),
                Err(crate::coding_agent::CodingAgentError::Conflict(_)) => continue,
                Err(_) => return Err(CodingAgentShutdownError::Storage),
            }
        }
        Err(CodingAgentShutdownError::Storage)
    }
}

fn shutdown_run_snapshot(run: &crate::coding_agent::AgentRunRecord) -> CodingAgentShutdownRun {
    CodingAgentShutdownRun {
        run_id: run.snapshot.run_id.to_string(),
        revision: run.revision,
        state: run.state,
    }
}

pub(crate) fn jobworkerp_connect_error(url: &str, error: AppError) -> AppError {
    tracing::warn!(
        endpoint = "jobworkerp",
        url = %url,
        error = %error,
        "gRPC connection failed"
    );
    connection::target_connect_error("jobworkerp", url, error)
}

impl AppState {
    pub fn new(sidecars: Arc<Sidecars>, data: DataPaths) -> Self {
        Self::try_new(sidecars, data).expect("app state coding-agent storage initialization failed")
    }

    pub fn try_new(sidecars: Arc<Sidecars>, data: DataPaths) -> AppResult<Self> {
        Self::try_new_with_resources(sidecars, data, None)
    }

    pub fn try_new_with_resources(
        sidecars: Arc<Sidecars>,
        data: DataPaths,
        resource_dir: Option<std::path::PathBuf>,
    ) -> AppResult<Self> {
        let embedding_migration_recovery_required =
            data.root.join("embedding-migration-journal.json").exists();
        let run_store = Arc::new(crate::coding_agent::RunStore::open(&data).map_err(|_| {
            AppError::Config("coding-agent run storage could not be initialized".into())
        })?);
        let issue_tracker_availability = Arc::new(IssueTrackerAvailability::default());
        sidecars.bind_issue_tracker_availability(Arc::clone(&issue_tracker_availability));
        let coding_agent_settings = Arc::new(
            crate::coding_agent::settings::CodingAgentSettingsStore::new(
                data.coding_agent_settings_path(),
            ),
        );
        let native_terminal_admission = Arc::new(tokio::sync::Mutex::new(()));
        let coding_agent_accepting = Arc::new(AtomicBool::new(true));
        // Native application policy ceilings, not observed host/SDK capacity
        // and not an additional admission semaphore or VM quota.
        let native_limits = crate::coding_agent::HostResourceLimits {
            cpu_millis: 16_000,
            memory_mib: 32_768,
            root_disk_mib: 65_536,
            max_parallel_runs: 16,
        };
        let native_terminal_registry = Arc::new(
            crate::native_terminal::registry::NativeRuntimeRegistry::new_with_accepting(
                data.clone(),
                Arc::clone(&sidecars),
                Arc::clone(&issue_tracker_availability),
                Arc::clone(&coding_agent_settings),
                Arc::clone(&native_terminal_admission),
                native_limits,
                resource_dir,
                Arc::clone(&coding_agent_accepting),
            ),
        );
        let llm_settings_dispatch_gate = Arc::new(Mutex::new(()));
        let coding_agent_execution = Arc::new(
            crate::coding_agent::native_execution::NativeExecutionAdapter::new(
                Arc::clone(&native_terminal_registry),
                Arc::clone(&llm_settings_dispatch_gate),
            )
            .with_publication_store(run_store.clone()),
        );
        let coordinator = Arc::new(
            crate::coding_agent::CodingAgentCoordinator::new(
                run_store.clone(),
                Some(coding_agent_execution.clone()),
                None,
            )
            .with_workflow_publication_adapter(coding_agent_execution.clone()),
        );
        let unresolved = coordinator.recover_on_startup().map_err(|_| {
            AppError::Config("coding-agent startup recovery could not be read".into())
        })?;
        if !unresolved.is_empty() {
            tracing::warn!(
                count = unresolved.len(),
                "coding-agent runs require explicit recovery; no run was dispatched"
            );
        }
        Ok(Self {
            sidecars,
            data: data.clone(),
            coding_agent_run_store: run_store,
            coding_agent_coordinator: coordinator,
            coding_agent_execution,
            coding_agent_settings,
            native_terminal_registry,
            native_terminal_admission,
            coding_agent_backend: Arc::new(crate::coding_agent::backend::CodingAgentBackend::new()),
            coding_agent_accepting,
            coding_agent_shutdown_lock: tokio::sync::Mutex::new(()),
            window_lifecycle: crate::window_lifecycle::WindowLifecycle::default(),
            coding_agent_tray_icon: parking_lot::Mutex::new(None),
            coding_agent_run_owners: coding_agent::CodingAgentRunOwnerRegistry::default(),
            issue_tracker_availability,
            memories_channel: Mutex::new(None),
            jobworkerp: Mutex::new(None),
            local_jobworkerp: client_cache::UrlKeyedClientCache::new(),
            conductor_channel: client_cache::UrlKeyedClientCache::new(),
            unsupported_memories_rpcs: memories_rpc_support::UnsupportedMemoriesRpcs::new(),
            embedding_connection_epoch: AtomicU64::new(0),
            search_index_maintenance: Arc::new(
                crate::search_index_maintenance::SearchIndexMaintenanceCoordinator::new(
                    data.clone(),
                ),
            ),
            database_migration_recovery: Mutex::new(()),
            summary_recovery: Mutex::new(()),
            thread_group_recovery: Mutex::new(()),
            import_progress: Arc::new(import::ImportProgressRegistry::default()),
            dispatch_in_flight: Mutex::new(HashMap::new()),
            llm_settings_dispatch_gate,
            embedding_write_admission_gate: Arc::new(Mutex::new(())),
            embedding_migration_admission: AtomicBool::new(embedding_migration_recovery_required),
        })
    }

    pub fn ensure_coding_agent_accepting(&self) -> AppResult<()> {
        if self.coding_agent_accepting.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(AppError::Config(
                "coding-agent is shutting down and no longer accepts operations".into(),
            ))
        }
    }

    pub fn claim_coding_agent_run(&self, run_id: &str, window_label: &str) -> AppResult<()> {
        self.coding_agent_run_owners
            .claim(run_id, window_label)
            .map_err(|_| {
                AppError::Config("coding-agent run is owned by another application window".into())
            })
    }

    pub fn require_coding_agent_run_owner(
        &self,
        run_id: &str,
        window_label: &str,
    ) -> AppResult<()> {
        if self
            .coding_agent_run_owners
            .is_owned_by(run_id, window_label)
        {
            Ok(())
        } else {
            Err(AppError::Config(
                "coding-agent run is not attached to this application window".into(),
            ))
        }
    }

    /// Attempt cancellation and reconciliation, durably record Unknown on
    /// uncertainty, then stop sidecars only after that state is committed.
    pub async fn shutdown_for_quit(
        &self,
        window_labels: &[String],
    ) -> AppResult<CodingAgentShutdownOutcome> {
        let _shutdown = self.coding_agent_shutdown_lock.lock().await;
        self.coding_agent_accepting.store(false, Ordering::Release);
        self.embedding_connection_epoch
            .fetch_add(1, Ordering::AcqRel);
        // An admitted start holds this lock through claim/enqueue-open. Wait
        // for it before taking shutdown snapshots; queued starts must recheck
        // this same accepting flag after acquiring admission. Do not retain
        // the guard while cancelling or draining managed sessions.
        {
            let _admission = self.native_terminal_admission.lock().await;
        }
        let native_deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        let mut unconfirmed_native_session_ids = Vec::new();
        for label in window_labels {
            let remaining = native_deadline.saturating_duration_since(tokio::time::Instant::now());
            unconfirmed_native_session_ids.extend(
                self.native_terminal_registry
                    .profile_settings_terminals
                    .shutdown(label, remaining)
                    .await,
            );
            let remaining = native_deadline.saturating_duration_since(tokio::time::Instant::now());
            let records = match self
                .native_terminal_registry
                .shutdown_for_window(label, remaining)
                .await
            {
                Ok(records) => records,
                Err(_) => {
                    self.coding_agent_accepting.store(true, Ordering::Release);
                    return Err(AppError::Config(
                        "native session shutdown could not be recorded; application remains open"
                            .into(),
                    ));
                }
            };
            unconfirmed_native_session_ids.extend(
                records
                    .into_iter()
                    .filter(|record| {
                        !matches!(
                            record.state,
                            crate::native_terminal::wire::NativeTerminalSessionStateWire::Closed
                                | crate::native_terminal::wire::NativeTerminalSessionStateWire::Cancelled
                        )
                    })
                    .map(|record| record.session_id),
            );
        }
        let operations = CoordinatorShutdownOperations {
            coordinator: &self.coding_agent_coordinator,
            store: &self.coding_agent_run_store,
        };
        let outcome = reconcile_coding_agent_shutdown_then_stop(
            &operations,
            tokio::time::Instant::now() + Duration::from_secs(8),
            || async {
                self.sidecars
                    .stop()
                    .await
                    .map_err(|_| CodingAgentShutdownError::SidecarStop)
            },
        )
        .await;
        match outcome {
            Ok(mut outcome) => {
                unconfirmed_native_session_ids.sort();
                unconfirmed_native_session_ids.dedup();
                outcome.unconfirmed_native_session_ids = unconfirmed_native_session_ids;
                Ok(outcome)
            }
            Err(CodingAgentShutdownError::Storage) => {
                self.coding_agent_accepting.store(true, Ordering::Release);
                Err(AppError::Config(
                    "coding-agent shutdown state could not be persisted; application remains open"
                        .into(),
                ))
            }
            Err(
                CodingAgentShutdownError::SidecarStop | CodingAgentShutdownError::RunUnavailable,
            ) => {
                self.coding_agent_accepting.store(true, Ordering::Release);
                Err(AppError::Config(
                    "application services could not stop cleanly; application remains open".into(),
                ))
            }
        }
    }

    pub(crate) fn set_coding_agent_tray_icon(&self, icon: Option<tauri::tray::TrayIcon>) {
        *self.coding_agent_tray_icon.lock() = icon;
    }

    /// Re-evaluate the native restore path at every close request. The
    /// existence of a Tauri TrayIcon alone is not evidence of a visible host.
    pub(crate) fn coding_agent_tray_availability(&self) -> crate::TrayAvailability {
        let tray_icon = self.coding_agent_tray_icon.lock().clone();
        crate::tray_availability_for_icon(
            tray_icon.as_ref(),
            crate::platform_tray_host_availability(tray_icon.as_ref()),
        )
    }

    pub fn require_endpoints(&self) -> AppResult<SidecarEndpoints> {
        self.sidecars
            .current_endpoints()
            .ok_or_else(|| AppError::SidecarNotReady("not started".into()))
    }

    /// Resolve the gRPC targets: a local service uses its live sidecar port,
    /// a remote one its configured URL. Read on every (re)connect so a
    /// `set_connection_config` +
    /// `invalidate_clients` takes effect on the next command. Exposed to the
    /// import / reflection-dispatch commands so they target the same endpoints
    /// (incl. the workflow callback host/port/tls) as the browse-only clients.
    pub(super) fn resolve_targets(&self) -> AppResult<connection::ResolvedTargets> {
        Ok(self.resolve_connection()?.targets)
    }

    /// Resolve the modes and URLs from the same persisted configuration
    /// snapshot.
    pub(super) fn resolve_connection(&self) -> AppResult<connection::ResolvedConnection> {
        let cfg = connection::load_connection_config(&self.data.connection_config_path());
        let local = self.sidecars.current_endpoints();
        connection::resolve_connection(&cfg, local.as_ref())
    }

    pub(super) fn embedding_connection_snapshot(
        &self,
    ) -> AppResult<embedding_space::EmbeddingConnectionSnapshot> {
        let resolved = self.resolve_connection()?;
        let snapshot = embedding_space::EmbeddingConnectionSnapshot {
            generation: self.embedding_connection_epoch.load(Ordering::Acquire),
            resolved,
        };
        self.ensure_embedding_connection_current(&snapshot)?;
        Ok(snapshot)
    }

    pub(super) fn ensure_embedding_connection_current(
        &self,
        snapshot: &embedding_space::EmbeddingConnectionSnapshot,
    ) -> AppResult<()> {
        if self.embedding_connection_epoch.load(Ordering::Acquire) != snapshot.generation {
            return Err(embedding_space::embedding_connection_changed());
        }
        if self.resolve_connection()? != snapshot.resolved {
            return Err(embedding_space::embedding_connection_changed());
        }
        Ok(())
    }

    /// Whether `service` is the local sidecar or a remote server. Read on
    /// every call, same read-on-dispatch pattern as `resolve_targets()`, so a
    /// `set_connection_config` takes effect on the next command without a
    /// restart.
    pub(crate) fn service_mode(&self, service: connection::Service) -> connection::ConnectionMode {
        connection::load_connection_config(&self.data.connection_config_path()).mode(service)
    }

    pub(crate) fn is_service_local(&self, service: connection::Service) -> bool {
        self.service_mode(service) == connection::ConnectionMode::Local
    }

    /// Refuse an embedding-dependent dispatch when the local vector store is
    /// degraded (a dimension mismatch forced the memories child to restart
    /// with vectors disabled). Only gates while memories is **local**: a
    /// remote memories keeps its own vector store, unaffected by the local
    /// LanceDB. Used by both the embed-dependent
    /// search commands and the generation / import / write paths (their gate
    /// scope collapses to the same "local + degraded" rule).
    pub(super) fn ensure_local_embedding_available(&self) -> AppResult<()> {
        if self.is_service_local(connection::Service::Memories) {
            if self.embedding_migration_admission.load(Ordering::Acquire) {
                return Err(AppError::Config(
                    "local embedding writes are paused for an embedding migration".into(),
                ));
            }
            if let Some(info) = self.sidecars.degraded() {
                return Err(AppError::VectorStoreDegraded {
                    expected_dim: info.expected_dim,
                    actual_dim: info.actual_dim,
                });
            }
        }
        Ok(())
    }

    pub(crate) async fn close_embedding_migration_admission(&self) {
        let _write_gate = self.embedding_write_admission_gate.lock().await;
        self.embedding_migration_admission
            .store(true, Ordering::Release);
    }

    pub(crate) async fn open_embedding_migration_admission(&self) {
        let _write_gate = self.embedding_write_admission_gate.lock().await;
        self.embedding_migration_admission
            .store(false, Ordering::Release);
    }

    /// Snapshot of `llm-settings.json`. Read on every call (same
    /// read-on-dispatch pattern as `resolve_targets()` reading
    /// `connection.json`). Callers that need several derived values
    /// (chat: worker name + external flag + generation overrides) should
    /// take ONE snapshot and project from it rather than calling the
    /// per-field accessors below, which each re-read the file.
    pub(super) fn llm_settings_snapshot(&self) -> llm_settings::LlmSettings {
        llm_settings::load_llm_settings(&self.data.llm_settings_path())
    }

    /// Worker name chat / workflow dispatches should target. Returns
    /// `&'static str` so dispatch sites don't allocate per call — the only
    /// two possible values (`memories-llm` / `memories-llm-external`) are
    /// compile-time literals.
    pub(super) fn active_llm_worker_name(&self) -> &'static str {
        llm_settings::worker_name_for(self.llm_settings_snapshot().mode)
    }

    /// Whether the active LLM mode is external (genai).
    pub(super) fn is_external_llm(&self) -> bool {
        self.llm_settings_snapshot().mode == llm_settings::LlmMode::External
    }

    /// The output language ("ja" | "en") generation dispatches should target.
    /// Read from the persisted `app-settings.json` value the frontend keeps in
    /// sync with the UI locale (`set_output_language`); falls back to `"ja"`.
    /// Headless paths (conductor periodic) and the frontend dispatch commands
    /// share this single source so import-time and later regeneration agree.
    pub(super) fn active_output_language(&self) -> String {
        let persisted =
            crate::data::paths::load_app_settings(&self.data.app_settings_path()).output_language;
        resolve_output_language(None, persisted.as_deref())
    }

    /// Drop the cached gRPC clients so the next `memories_channel()` /
    /// `jobworkerp()` reconnects against the current endpoints. Called by
    /// `retry_model_setup` so post-restart commands don't talk to the
    /// stopped sidecar's (possibly stale) port.
    pub async fn invalidate_clients(&self) {
        self.embedding_connection_epoch
            .fetch_add(1, Ordering::AcqRel);
        *self.memories_channel.lock().await = None;
        *self.jobworkerp.lock().await = None;
        self.local_jobworkerp.invalidate().await;
        self.conductor_channel.invalidate().await;
        self.unsupported_memories_rpcs.invalidate();
    }

    /// Reused client for the always-local jobworkerp sidecar at `url`.
    pub(crate) async fn local_jobworkerp(
        &self,
        url: &str,
    ) -> AppResult<crate::jobworkerp::JobworkerpHandle> {
        self.local_jobworkerp
            .get_or_connect(url, || crate::jobworkerp::JobworkerpHandle::connect(url))
            .await
    }

    /// conductor's URL, local or remote, read on every call like
    /// `resolve_targets()`.
    pub(crate) fn conductor_url(&self) -> AppResult<String> {
        let cfg = connection::load_connection_config(&self.data.connection_config_path());
        connection::conductor_url(&cfg, self.sidecars.current_endpoints().as_ref())
    }

    /// What conductor's schedulers need, from one configuration snapshot:
    /// conductor's URL, jobworkerp as conductor reaches it, and the services
    /// the scheduled workflows reach from the jobworkerp host.
    pub(crate) fn periodic_targets(&self) -> AppResult<PeriodicTargets> {
        let cfg = connection::load_connection_config(&self.data.connection_config_path());
        let local = self.sidecars.current_endpoints();
        Ok(PeriodicTargets {
            conductor_url: connection::conductor_url(&cfg, local.as_ref())?,
            conductor_identity: connection::conductor_identity(&cfg),
            jobworkerp: connection::jobworkerp_for_dependents(&cfg, local.as_ref())?,
            services: periodic_tasks::PeriodicServices::new(
                &cfg,
                connection::resolve_targets(&cfg, local.as_ref())?.memories_callback()?,
            ),
        })
    }

    /// Reused channel to conductor.
    pub(crate) async fn conductor_channel(&self) -> AppResult<Channel> {
        let url = self.conductor_url()?;
        self.conductor_channel
            .get_or_connect(&url, || grpc::connect(&url))
            .await
    }

    /// Lazily establish — and then reuse — a single tonic channel to the
    /// memories sidecar. Tonic channels multiplex via HTTP/2 so all command
    /// handlers can share one. The `Mutex` serializes the connect so a burst
    /// of commands on startup opens exactly one channel.
    pub(crate) async fn memories_channel(&self) -> AppResult<Channel> {
        Ok(self.memories_endpoint().await?.1)
    }

    /// The memories URL together with its channel, resolved once so RPC
    /// support recorded in `unsupported_memories_rpcs` is attributed to the
    /// server that actually answered.
    pub(crate) async fn memories_endpoint(&self) -> AppResult<(String, Channel)> {
        let target = self.resolve_targets()?;
        let mut guard = self.memories_channel.lock().await;
        if let Some(ch) = guard.as_ref() {
            return Ok((target.memories_url, ch.clone()));
        }
        let ch = grpc::connect(&target.memories_url).await.map_err(|e| {
            tracing::warn!(
                endpoint = "memories",
                url = %target.memories_url,
                error = %e,
                "gRPC connection failed"
            );
            connection::target_connect_error("memories", &target.memories_url, e)
        })?;
        *guard = Some(ch.clone());
        Ok((target.memories_url, ch))
    }

    /// Support cache for memories RPCs that older servers may lack; see
    /// `memories_rpc_support`.
    pub(crate) fn unsupported_memories_rpcs(
        &self,
    ) -> &memories_rpc_support::UnsupportedMemoriesRpcs {
        &self.unsupported_memories_rpcs
    }

    /// Lazily establish — and reuse — a single jobworkerp client. All
    /// `dispatch_stream` calls share the same gRPC connection through
    /// `JobworkerpClientWrapper`'s internal channel.
    pub async fn jobworkerp(&self) -> AppResult<crate::jobworkerp::JobworkerpHandle> {
        let target = self.resolve_targets()?;
        let mut guard = self.jobworkerp.lock().await;
        if let Some(h) = guard.as_ref() {
            return Ok(h.clone());
        }
        let h = crate::jobworkerp::JobworkerpHandle::connect(&target.jobworkerp_url)
            .await
            .map_err(|e| jobworkerp_connect_error(&target.jobworkerp_url, e))?;
        *guard = Some(h.clone());
        Ok(h)
    }

    /// Dedicated jobworkerp client for a long-running workflow job. Its
    /// request deadline and default job deadline are both `timeout_sec`: the
    /// shared client's three-hour deadline would close a longer result stream
    /// while the server-side job is still running.
    pub async fn jobworkerp_with_timeout(
        &self,
        timeout_sec: u64,
    ) -> AppResult<crate::jobworkerp::JobworkerpHandle> {
        let url = self.resolve_targets()?.jobworkerp_url;
        crate::jobworkerp::JobworkerpHandle::connect_with_timeout(&url, timeout_sec)
            .await
            .map_err(|e| jobworkerp_connect_error(&url, e))
    }

    /// Open Lookback's private local Issue tracker service.
    ///
    /// This deliberately bypasses `jobworkerp()`: that helper may honour a
    /// remote browsing configuration, while the Issue tracker owns state in
    /// this application's local data root. The returned service is Rust-only
    /// and checks the endpoint contract before any manager call. Individual
    /// manager business errors do not alter the lifecycle boundary.
    pub async fn issue_tracker_service(
        &self,
    ) -> AppResult<crate::grpc::issue_tracker::IssueTrackerGrpcService> {
        self.issue_tracker_availability
            .require_endpoint_connected()?;
        let endpoint = self.sidecars.issue_tracker_url().ok_or_else(|| {
            AppError::SidecarNotReady("Issue Tracker server endpoint is unavailable".into())
        })?;
        let service = crate::grpc::issue_tracker::IssueTrackerGrpcService::new(endpoint);
        service
            .contract_probe("app-state-issue-tracker-service")
            .await?;
        Ok(service)
    }

    pub async fn issue_tracker_gateway_control_url(&self) -> AppResult<String> {
        self.issue_tracker_availability.require_gateway_ready()?;
        self.sidecars
            .issue_tracker_gateway_control_url()
            .ok_or_else(|| {
                AppError::SidecarNotReady(
                    "Issue Tracker Gateway control endpoint is unavailable".into(),
                )
            })
    }

    /// Insert a fresh [`DispatchCancelEntry`] for `dispatch_id` and
    /// return a clone the driver task can wire into its hop / step
    /// dispatch sites. An existing entry for the same id (e.g. a stale
    /// run the previous driver forgot to remove) is overwritten — the
    /// live driver owns the new token and the stale token never fires.
    pub async fn dispatch_register(&self, dispatch_id: &str) -> DispatchCancelEntry {
        let _settings_gate = self.llm_settings_dispatch_gate.lock().await;
        self.dispatch_register_admitted(dispatch_id).await
    }

    /// Register a dispatch while the caller holds
    /// [`Self::lock_llm_settings_dispatch_gate`]. This lets a command keep its
    /// LLM settings snapshot, sidecar connection, and cancellation entry in
    /// one admission interval.
    pub(crate) async fn dispatch_register_admitted(
        &self,
        dispatch_id: &str,
    ) -> DispatchCancelEntry {
        let entry = DispatchCancelEntry::new();
        if self.is_service_local(connection::Service::Memories)
            && self.embedding_migration_admission.load(Ordering::Acquire)
        {
            entry.token.cancel();
            return entry;
        }
        self.dispatch_in_flight
            .lock()
            .await
            .insert(dispatch_id.to_string(), entry.clone());
        entry
    }

    /// Acquire the gate that excludes dispatch admission while an LLM settings
    /// change performs its in-flight check and sidecar restart.
    pub async fn lock_llm_settings_dispatch_gate(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.llm_settings_dispatch_gate.lock().await
    }

    /// LLM worker settings are process-wide. Replacing that worker while any
    /// long-running dispatch is active can split a workflow across model
    /// configurations, so callers must reject the settings change first.
    pub async fn has_in_flight_dispatches(&self) -> bool {
        self.coding_agent_execution.has_active_semantic_judges()
            || !self.dispatch_in_flight.lock().await.is_empty()
    }

    pub(crate) async fn cancel_dispatches_and_wait(&self, budget: Duration) -> AppResult<()> {
        let deadline = tokio::time::Instant::now() + budget;
        let dispatch_ids = self
            .dispatch_in_flight
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for dispatch_id in dispatch_ids {
            cancel_dispatch_inner(self, &dispatch_id).await?;
        }

        loop {
            let no_dispatches = self.dispatch_in_flight.lock().await.is_empty();
            let no_semantic_judges = !self.coding_agent_execution.has_active_semantic_judges();
            if no_dispatches && no_semantic_judges {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AppError::Config(
                    "timed out draining in-flight embedding writers".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Look up the cancel handle for `dispatch_id` without removing it
    /// from the map. The cancel command uses this to flip the token +
    /// Delete the running job, then leaves the driver's own deferred
    /// [`dispatch_take`](Self::dispatch_take) call to clean the map.
    pub async fn dispatch_get(&self, dispatch_id: &str) -> Option<DispatchCancelEntry> {
        self.dispatch_in_flight
            .lock()
            .await
            .get(dispatch_id)
            .cloned()
    }

    /// Remove the cancel entry — called by the driver when it finishes
    /// (Done / cancelled / budget) so the map doesn't grow unboundedly
    /// across a long session.
    pub async fn dispatch_take(&self, dispatch_id: &str) -> Option<DispatchCancelEntry> {
        self.dispatch_in_flight.lock().await.remove(dispatch_id)
    }
}

/// Register a direct memories mutation before it is sent.  Workflow dispatches
/// use the same pair around their complete logical group.
pub(super) async fn begin_search_index_write(
    state: &AppState,
    tables: &[crate::search_index_maintenance::MaintenanceTable],
) -> AppResult<bool> {
    if !state.is_service_local(connection::Service::Memories) {
        return Ok(false);
    }
    let _admission = state.embedding_write_admission_gate.lock().await;
    if state.embedding_migration_admission.load(Ordering::Acquire) {
        return Err(AppError::Config(
            "local embedding writes are paused for an embedding migration".into(),
        ));
    }
    state.search_index_maintenance.begin_write(tables).await?;
    Ok(true)
}

pub(super) async fn finish_search_index_write(state: &AppState, registered: bool) -> AppResult<()> {
    if registered {
        state.search_index_maintenance.finish_write().await?;
        // The terminal edge does not wait for the hourly recovery cadence.
        // A failed RPC intentionally leaves `reconcile_pending` durable.
        let channel = state.memories_channel().await?;
        state.search_index_maintenance.reconcile(channel).await?;
    }
    Ok(())
}

/// Cancel an in-flight dispatch keyed by `dispatch_id`. Shared by
/// `chat_cancel`, `import_cancel`, and `analysis_cancel` — the only
/// per-domain differences (UI event names, terminal messages) live in
/// the driver task, not the cancel command.
///
/// Idempotent: an unknown or already-completed id is a no-op so the UI
/// can fire-and-forget on every Stop click.
pub(crate) async fn cancel_dispatch_inner(state: &AppState, dispatch_id: &str) -> AppResult<()> {
    let Some(entry) = state.dispatch_get(dispatch_id).await else {
        tracing::debug!(dispatch_id = %dispatch_id, "cancel_dispatch: no in-flight entry");
        return Ok(());
    };
    let deleted_job_id = cancel_dispatch_entry_with(&entry, |jid| async move {
        let job_id_value = jid.value;
        let handle = state.jobworkerp().await?;
        // Best-effort: the token is already flipped so the driver will
        // surface the cancelled terminal event even if Delete fails
        // (e.g. the job finished between the lock release and the gRPC
        // call landing).
        if let Err(error) = handle.cancel(jid).await {
            tracing::warn!(dispatch_id = %dispatch_id, job_id = job_id_value, err = %error, "JobService/Delete failed");
        }
        Ok(())
    })
    .await?;
    if let Some(job_id_value) = deleted_job_id {
        tracing::info!(dispatch_id = %dispatch_id, job_id = job_id_value, "cancel_dispatch issued JobService/Delete");
    } else {
        tracing::info!(dispatch_id = %dispatch_id, "cancel_dispatch flipped token; no live JobId");
    }
    Ok(())
}

async fn cancel_dispatch_entry_with<F, Fut>(
    entry: &DispatchCancelEntry,
    delete_job: F,
) -> AppResult<Option<i64>>
where
    F: FnOnce(JobId) -> Fut,
    Fut: std::future::Future<Output = AppResult<()>>,
{
    // Keep the token edge and ownership transfer of the live JobId in one
    // critical section. The stream cleanup path uses this same slot lock, so
    // it cannot clear a JobId after observing cancellation but before the
    // cancel command has claimed it for JobService/Delete.
    let live_jid = {
        let mut current_job_id = entry.current_job_id.lock().await;
        entry.token.cancel();
        current_job_id.take()
    };
    let Some(jid) = live_jid else {
        return Ok(None);
    };
    let job_id_value = jid.value;
    delete_job(jid).await?;
    Ok(Some(job_id_value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecar::{SidecarConfig, SidecarWarningKind};

    #[test]
    fn plugin_staging_failure_becomes_non_fatal_warning() {
        let warnings = plugin_staging_warnings(Err::<(), _>("staging source is unavailable"));

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].kind, SidecarWarningKind::PluginsStageFailed);
        assert_eq!(warnings[0].message, "staging source is unavailable");
        assert_eq!(warnings[0].detail, None);
    }

    #[test]
    fn successful_plugin_staging_has_no_warning() {
        assert!(plugin_staging_warnings::<(), &str>(Ok(())).is_empty());
    }

    #[test]
    fn resolve_output_language_prefers_explicit() {
        assert_eq!(resolve_output_language(Some("en"), Some("ja")), "en");
        assert_eq!(resolve_output_language(Some("ja"), None), "ja");
    }

    #[test]
    fn resolve_output_language_falls_back_to_persisted_then_default() {
        // No explicit → persisted wins.
        assert_eq!(resolve_output_language(None, Some("en")), "en");
        // Neither → "ja".
        assert_eq!(resolve_output_language(None, None), "ja");
    }

    #[test]
    fn resolve_output_language_ignores_unsupported_and_blank() {
        // An unsupported / hand-edited value is dropped (it would route to a
        // worker that was never registered), falling through to the next stage.
        assert_eq!(resolve_output_language(Some("fr"), Some("en")), "en");
        assert_eq!(resolve_output_language(Some(""), None), "ja");
        assert_eq!(resolve_output_language(None, Some("zz")), "ja");
        // Whitespace is trimmed before the whitelist check.
        assert_eq!(resolve_output_language(Some(" en "), None), "en");
    }

    #[test]
    fn generated_refresh_scopes_detect_all_persisted_output_steps() {
        let cases = [
            (
                "/ROOT/do/4/summarizeEach/for/do/1/recordGenerated",
                GeneratedRefreshScope::ThreadSummary,
            ),
            (
                "/ROOT/do/4/summarizeEach/for/do/1/recordSuccess",
                GeneratedRefreshScope::ThreadSummary,
            ),
            (
                "/ROOT/do/3/reflectEach/for/do/1/recordSuccess",
                GeneratedRefreshScope::Reflection,
            ),
            (
                "/ROOT/do/7/personalityEach/for/do/1/recordSignalOutcome",
                GeneratedRefreshScope::Personality,
            ),
            (
                "/ROOT/do/8/updateProfile",
                GeneratedRefreshScope::Personality,
            ),
            (
                "/ROOT/do/8/addProfileMemory",
                GeneratedRefreshScope::Personality,
            ),
        ];

        for (position, expected) in cases {
            let chunk = serde_json::json!({ "position": position }).to_string();
            assert_eq!(
                generated_refresh_scopes_from_position(&chunk),
                vec![expected]
            );
        }
    }

    #[test]
    fn generated_refresh_scopes_ignore_non_persisted_or_invalid_chunks() {
        let chunk = serde_json::json!({
            "status": "Running",
            "position": "/ROOT/do/4/summarizeEach/for/do/0/reportProgress",
            "output": "{}"
        })
        .to_string();

        assert!(generated_refresh_scopes_from_position(&chunk).is_empty());
        assert!(generated_refresh_scopes_from_position("not json").is_empty());
        assert!(
            generated_refresh_scopes_from_position(r#"{"position":"/ROOT/do/1/unrelated"}"#)
                .is_empty()
        );
        assert!(
            generated_refresh_scopes_from_position(
                r#"{"position":"/ROOT/do/1/not-recordSuccess/summarizeEachElse"}"#
            )
            .is_empty()
        );
    }

    #[test]
    fn generation_workflows_keep_the_refresh_completion_steps() {
        for (workflow, completion_step) in [
            (
                include_str!("../../../workers/workflows/thread-summary/thread-summary-batch.yaml"),
                "- recordGenerated:",
            ),
            (
                include_str!(
                    "../../../workers/workflows/thread-reflection/thread-reflection-batch.yaml"
                ),
                "- recordSuccess:",
            ),
            (
                include_str!(
                    "../../../workers/workflows/personality/thread-personality-batch.yaml"
                ),
                "- recordSignalOutcome:",
            ),
            (
                include_str!(
                    "../../../workers/lang-workers/workers/personality/user-personality-merge.yaml"
                ),
                "- updateProfile:",
            ),
            (
                include_str!(
                    "../../../workers/lang-workers/workers/personality/user-personality-merge.yaml"
                ),
                "- addProfileMemory:",
            ),
        ] {
            assert!(
                workflow.contains(completion_step),
                "missing {completion_step}"
            );
        }
    }

    fn dummy_state() -> AppState {
        let data = DataPaths::with_root("/tmp/lookback-appstate-test");
        let lance_home = data.lance_language_model_home();
        let sidecars = Arc::new(Sidecars::new(SidecarConfig {
            jobworkerp_bin: std::path::PathBuf::from("/bin/true"),
            memories_bin: std::path::PathBuf::from("/bin/true"),
            memories_db_migrate_bin: std::path::PathBuf::from("/bin/true"),
            startup_database_migration_enabled: false,
            conductor_bin: std::path::PathBuf::from("/bin/true"),
            data: data.clone(),
            worker_yaml_paths: Vec::new(),
            issue_tracker_server_bin: None,
            function_set_yaml_paths: Vec::new(),
            reflection_dispatch_enabled: false,
            auto_embedding_enabled: false,
            workflows_dir: None,
            lance_language_model_home: lance_home,
            lindera_dict_staged: false,
            llm_model: None,
            llm_hf_repo: None,
            llm_ctx_size: None,
            llm_kv_cache_type: None,
            env_file: None,
        }));
        AppState::new(sidecars, data)
    }

    #[tokio::test]
    async fn native_feature_missing_resources_does_not_prevent_app_state_startup() {
        let state = dummy_state();
        let status = state
            .native_terminal_registry
            .status("issue-42")
            .await
            .unwrap();
        assert!(!status.available);
        assert!(!status.blockers.is_empty());
        assert!(state.ensure_coding_agent_accepting().is_ok());
    }

    #[tokio::test]
    async fn settings_admission_rechecks_shutdown_after_waiting_for_the_shared_lock() {
        let state = Arc::new(dummy_state());
        let admission = state.native_terminal_admission.lock().await;
        let waiting = Arc::clone(&state);
        let task =
            tokio::spawn(async move { coding_agent::settings_admission(&waiting).await.is_ok() });
        tokio::task::yield_now().await;
        state.coding_agent_accepting.store(false, Ordering::Release);
        drop(admission);
        assert!(!task.await.unwrap());
        assert_eq!(state.coding_agent_settings.load().unwrap().revision, 0);
    }

    #[tokio::test]
    async fn profile_settings_start_rejects_stale_or_missing_profile_without_issue_state() {
        let state = dummy_state();
        let admission = coding_agent::settings_admission(&state).await.unwrap();
        for revision in [0, u64::MAX] {
            assert!(
                state
                    .native_terminal_registry
                    .start_profile_settings_terminal(
                        "main",
                        revision,
                        "not-registered",
                        &std::collections::HashSet::new(),
                        &admission,
                    )
                    .await
                    .is_err()
            );
        }
        assert!(
            state
                .native_terminal_registry
                .profile_settings_terminals
                .list("main")
                .is_empty()
        );
        assert_eq!(state.coding_agent_settings.load().unwrap().revision, 0);
    }

    #[tokio::test]
    async fn issue_tracker_service_requires_local_sidecars() {
        assert!(matches!(
            dummy_state().issue_tracker_service().await,
            Err(AppError::SidecarNotReady(_))
        ));
    }

    #[tokio::test]
    async fn issue_tracker_service_fails_immediately_when_worker_is_unavailable() {
        let state = dummy_state();
        state
            .issue_tracker_availability
            .mark_unavailable("worker registration failed");

        assert!(matches!(
            state.issue_tracker_service().await,
            Err(AppError::SidecarNotReady(message)) if message.contains("worker registration failed")
        ));
    }

    #[test]
    fn issue_tracker_availability_keeps_the_latest_readiness_reason() {
        let availability = IssueTrackerAvailability::default();
        availability.mark_unavailable("health check failed");

        assert!(matches!(
            availability.require_ready(),
            Err(AppError::SidecarNotReady(message)) if message.contains("health check failed")
        ));

        availability.mark_ready();
        assert!(availability.require_ready().is_ok());
    }

    /// AppState rooted at a caller-controlled temp dir so the connection
    /// config file can be written per-test (the shared `dummy_state` uses a
    /// fixed path and can't isolate connection mode).
    fn state_in(root: &std::path::Path) -> AppState {
        let data = DataPaths::with_root(root.to_path_buf());
        let lance_home = data.lance_language_model_home();
        let sidecars = Arc::new(Sidecars::new(SidecarConfig {
            jobworkerp_bin: std::path::PathBuf::from("/bin/true"),
            memories_bin: std::path::PathBuf::from("/bin/true"),
            memories_db_migrate_bin: std::path::PathBuf::from("/bin/true"),
            startup_database_migration_enabled: false,
            conductor_bin: std::path::PathBuf::from("/bin/true"),
            data: data.clone(),
            worker_yaml_paths: Vec::new(),
            issue_tracker_server_bin: None,
            function_set_yaml_paths: Vec::new(),
            reflection_dispatch_enabled: false,
            auto_embedding_enabled: false,
            workflows_dir: None,
            lance_language_model_home: lance_home,
            lindera_dict_staged: false,
            llm_model: None,
            llm_hf_repo: None,
            llm_ctx_size: None,
            llm_kv_cache_type: None,
            env_file: None,
        }));
        AppState::new(sidecars, data)
    }

    fn set_mode(state: &AppState, mode: connection::ConnectionMode) {
        let cfg = match mode {
            connection::ConnectionMode::Remote => {
                connection::ConnectionConfig::remote_pair("http://h:9000", "http://h:9010")
            }
            connection::ConnectionMode::Local => connection::ConnectionConfig::default(),
        };
        connection::save_connection_config(&state.data.connection_config_path(), &cfg).unwrap();
    }

    fn degraded_info() -> crate::sidecar::lifecycle::DegradedInfo {
        crate::sidecar::lifecycle::DegradedInfo {
            reason: "embedding_dimension_mismatch",
            expected_dim: 2048,
            actual_dim: 768,
        }
    }

    #[test]
    fn ensure_local_embedding_available_ok_when_not_degraded() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_in(tmp.path());
        set_mode(&state, connection::ConnectionMode::Local);
        // No degraded flag set → available even in local mode.
        assert!(state.ensure_local_embedding_available().is_ok());
    }

    #[tokio::test]
    async fn embedding_migration_admission_blocks_local_dispatches_until_reopened() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_in(tmp.path());
        set_mode(&state, connection::ConnectionMode::Local);

        state.close_embedding_migration_admission().await;
        assert!(state.embedding_migration_admission.load(Ordering::Acquire));
        assert!(state.ensure_local_embedding_available().is_err());
        let rejected = state.dispatch_register("migration-blocked").await;
        assert!(rejected.token.is_cancelled());
        assert!(state.dispatch_get("migration-blocked").await.is_none());

        state.open_embedding_migration_admission().await;
        assert!(!state.embedding_migration_admission.load(Ordering::Acquire));
        assert!(state.ensure_local_embedding_available().is_ok());
        let admitted = state.dispatch_register("migration-reopened").await;
        assert!(!admitted.token.is_cancelled());
    }

    #[tokio::test]
    async fn public_embedding_maintenance_gate_rejects_before_rpc_adapter_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_in(tmp.path());
        set_mode(&state, connection::ConnectionMode::Local);
        state.close_embedding_migration_admission().await;
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_by_rpc = called.clone();

        let result = super::embedding_space::with_embedding_maintenance_admission(
            &state,
            move || async move {
                called_by_rpc.store(true, Ordering::Release);
                Ok(())
            },
        )
        .await;

        assert!(result.is_err());
        assert!(!called.load(Ordering::Acquire));
    }

    #[test]
    fn durable_embedding_journal_starts_local_admission_closed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("embedding-migration-journal.json"),
            b"pending recovery",
        )
        .unwrap();
        let state = state_in(tmp.path());
        set_mode(&state, connection::ConnectionMode::Local);

        assert!(state.embedding_migration_admission.load(Ordering::Acquire));
        assert!(state.ensure_local_embedding_available().is_err());
    }

    #[test]
    fn ensure_local_embedding_available_errors_local_degraded() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state_in(tmp.path());
        set_mode(&state, connection::ConnectionMode::Local);
        state.sidecars.set_degraded_for_test(Some(degraded_info()));
        let err = state.ensure_local_embedding_available().unwrap_err();
        match err {
            AppError::VectorStoreDegraded {
                expected_dim,
                actual_dim,
            } => {
                assert_eq!((expected_dim, actual_dim), (2048, 768));
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn ensure_local_embedding_available_ok_remote_even_when_degraded() {
        // Remote mode routes embedding to the remote sidecar, so a degraded
        // LOCAL vector store must NOT gate remote-targeted dispatches.
        let tmp = tempfile::tempdir().unwrap();
        let state = state_in(tmp.path());
        set_mode(&state, connection::ConnectionMode::Remote);
        state.sidecars.set_degraded_for_test(Some(degraded_info()));
        assert!(state.ensure_local_embedding_available().is_ok());
    }

    #[tokio::test]
    async fn invalidate_clients_leaves_both_caches_empty() {
        // A real Channel/Handle needs a live sidecar, so we can't pre-seed
        // the caches here; this guards the basic contract that invalidate is
        // safe to call (idempotent) and both slots end up `None` — the state
        // a post-restart command relies on to force a reconnect.
        let tmp = tempfile::tempdir().unwrap();
        let state = state_in(tmp.path());
        set_mode(&state, connection::ConnectionMode::Remote);
        let embedding_snapshot = state.embedding_connection_snapshot().unwrap();
        state.unsupported_memories_rpcs.mark_unsupported(
            "http://memories",
            memories_rpc_support::MemoriesRpc::CountThreadGroups,
        );
        state.invalidate_clients().await;
        state.invalidate_clients().await; // idempotent
        assert!(!state.unsupported_memories_rpcs.is_unsupported(
            "http://memories",
            memories_rpc_support::MemoriesRpc::CountThreadGroups,
        ));
        assert!(state.memories_channel.lock().await.is_none());
        assert!(state.jobworkerp.lock().await.is_none());
        assert!(
            state
                .ensure_embedding_connection_current(&embedding_snapshot)
                .is_err()
        );
    }

    #[tokio::test]
    async fn dispatch_register_returns_fresh_entry_and_indexes_it() {
        let state = dummy_state();
        let entry = state.dispatch_register("turn-1").await;
        // The same entry must be visible to a subsequent `dispatch_get` —
        // cancel_dispatch_inner relies on this lookup to find the live token.
        let looked_up = state.dispatch_get("turn-1").await.expect("entry present");
        assert!(!entry.token.is_cancelled());
        assert!(!looked_up.token.is_cancelled());
        // Tokens clone shared state, so a cancel on one side is observable
        // on the other (the driver / cancel command side).
        looked_up.token.cancel();
        assert!(entry.token.is_cancelled());
    }

    #[tokio::test]
    async fn dispatch_register_initialises_current_step_as_none() {
        // current_step starts unset; the import pipeline writes a label
        // when it enters each step so a mid-step cancel knows which
        // downstream emits still need to fire.
        let state = dummy_state();
        let entry = state.dispatch_register("turn-step").await;
        assert!(entry.current_step.lock().await.is_none());
        assert!(entry.current_job_id.lock().await.is_none());
    }

    #[tokio::test]
    async fn dispatch_get_returns_none_for_unknown_id() {
        let state = dummy_state();
        assert!(state.dispatch_get("never-registered").await.is_none());
    }

    #[tokio::test]
    async fn dispatch_take_removes_entry_and_subsequent_get_misses() {
        let state = dummy_state();
        state.dispatch_register("turn-2").await;
        let taken = state.dispatch_take("turn-2").await;
        assert!(taken.is_some());
        // Once the driver has finished and called `dispatch_take`, a late
        // cancel lookup must miss so the click is a no-op instead of a
        // phantom Delete against a recycled JobId.
        assert!(state.dispatch_get("turn-2").await.is_none());
    }

    #[tokio::test]
    async fn in_flight_dispatch_check_tracks_register_and_completion() {
        let state = dummy_state();
        assert!(!state.has_in_flight_dispatches().await);
        state.dispatch_register("summary-1").await;
        assert!(state.has_in_flight_dispatches().await);
        state.dispatch_take("summary-1").await;
        assert!(!state.has_in_flight_dispatches().await);
    }

    #[tokio::test]
    async fn settings_gate_blocks_dispatch_admission_until_released() {
        let state = Arc::new(dummy_state());
        let gate = state.lock_llm_settings_dispatch_gate().await;
        let registering = {
            let state = Arc::clone(&state);
            tokio::spawn(async move { state.dispatch_register("summary-gated").await })
        };
        tokio::task::yield_now().await;
        assert!(!state.has_in_flight_dispatches().await);
        drop(gate);
        registering.await.unwrap();
        assert!(state.has_in_flight_dispatches().await);
    }

    #[tokio::test]
    async fn dispatch_register_overwrites_stale_entry_and_isolates_tokens() {
        // Safety net for the panic-residue path: if the previous driver's
        // RAII guard missed (or a future jobId scheme reuses ids) the fresh
        // registration must hand the new driver an independent token —
        // otherwise an inherited cancel could abort it.
        let state = dummy_state();
        let stale = state.dispatch_register("turn-3").await;
        let fresh = state.dispatch_register("turn-3").await;
        stale.token.cancel();
        assert!(stale.token.is_cancelled());
        assert!(!fresh.token.is_cancelled());
    }

    #[tokio::test]
    async fn dispatch_registration_and_cleanup_share_one_map() {
        let state = dummy_state();
        let entry = state.dispatch_register("chat-1").await;
        let via_dispatch = state.dispatch_get("chat-1").await.expect("entry present");
        via_dispatch.token.cancel();
        assert!(entry.token.is_cancelled());
        state.dispatch_take("chat-1").await;
        assert!(state.dispatch_get("chat-1").await.is_none());
    }

    #[tokio::test]
    async fn cancel_dispatch_inner_is_noop_for_unknown_id() {
        // A late Stop click after the driver already cleaned up its entry
        // must not error or attempt a phantom JobService/Delete.
        let state = dummy_state();
        cancel_dispatch_inner(&state, "no-such-id")
            .await
            .expect("unknown id is a no-op, not an error");
    }

    #[tokio::test]
    async fn cancel_dispatch_inner_flips_token_when_no_live_job_id() {
        // The "no live JobId" branch is reachable without a real
        // jobworkerp sidecar — the driver hasn't reached its first
        // dispatch yet, so cancel only needs to flip the token. The
        // live-JobId branch is covered end-to-end by the import e2e.
        let state = dummy_state();
        let entry = state.dispatch_register("dispatch-1").await;
        cancel_dispatch_inner(&state, "dispatch-1")
            .await
            .expect("cancel succeeds without a live job");
        assert!(entry.token.is_cancelled());
        // Map entry stays — the driver's deferred dispatch_take cleans it
        // up, mirroring the chat path. A second cancel is still a no-op
        // (token already cancelled, no live JobId).
        cancel_dispatch_inner(&state, "dispatch-1")
            .await
            .expect("second cancel is idempotent");
    }

    #[tokio::test]
    async fn cancellation_claims_live_job_and_invokes_delete_callback() {
        let entry = DispatchCancelEntry::new();
        *entry.current_job_id.lock().await = Some(JobId { value: 4242 });
        let slot_guard = entry.current_job_id.lock().await;
        let delete_called = Arc::new(AtomicBool::new(false));
        let delete_called_for_callback = Arc::clone(&delete_called);
        let entry_for_cancel = entry.clone();

        let cancel = tokio::spawn(async move {
            cancel_dispatch_entry_with(&entry_for_cancel, move |jid| async move {
                assert_eq!(jid.value, 4242);
                delete_called_for_callback.store(true, Ordering::Release);
                Ok(())
            })
            .await
            .unwrap()
        });
        tokio::task::yield_now().await;

        // The cancellation token must not wake stream cleanup until the
        // cancel command can atomically take ownership of the parked ID.
        assert!(!entry.token.is_cancelled());
        assert_eq!(slot_guard.as_ref().map(|jid| jid.value), Some(4242));
        drop(slot_guard);

        assert_eq!(cancel.await.unwrap(), Some(4242));
        assert!(delete_called.load(Ordering::Acquire));
        assert!(entry.current_job_id.lock().await.is_none());
    }

    #[test]
    fn parse_i64_after_prefix_handles_summary_and_personality_shapes() {
        // Summary form (used by commands/summaries.rs).
        assert_eq!(
            parse_i64_after_prefix("summary:", Some("summary:42")),
            Some(42)
        );
        assert_eq!(
            parse_i64_after_prefix("summary:", Some("summary:0")),
            Some(0)
        );
        // Personality profile form (used by commands/personality.rs).
        assert_eq!(
            parse_i64_after_prefix("personality_profile:", Some("personality_profile:200000")),
            Some(200_000)
        );
        // Wrong prefix returns None — caller falls back gracefully.
        assert_eq!(
            parse_i64_after_prefix("personality_profile:", Some("personality:42")),
            None
        );
        // Defensive None handling.
        assert_eq!(parse_i64_after_prefix("summary:", None), None);
        assert_eq!(
            parse_i64_after_prefix("summary:", Some("not-a-summary")),
            None
        );
        assert_eq!(
            parse_i64_after_prefix("summary:", Some("summary:abc")),
            None
        );
    }

    #[test]
    fn wrap_workflow_run_args_nests_input_as_json_string() {
        // The WORKFLOW `run` args (`WorkflowRunArgs`) expects the workflow
        // input under a single `input` *string* field (JSON-encoded), not a
        // nested object — otherwise enqueue drops the fields and the workflow
        // sees an empty input. Pin both the shape and that it round-trips.
        let input = serde_json::json!({ "user_id": 1, "single_workflow_path": "/x.yaml" });
        let args = wrap_workflow_run_args(&input);

        // Only `input` is present (workflow_url/_data come from settings).
        let obj = args.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        let input_field = obj["input"].as_str().expect("input must be a string");

        // The string is the JSON encoding of the original object.
        let parsed: serde_json::Value = serde_json::from_str(input_field).unwrap();
        assert_eq!(parsed, input);
    }
}
