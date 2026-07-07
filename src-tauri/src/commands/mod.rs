pub mod analysis_dispatch;
pub mod app_settings;
pub mod apply_settings;
pub mod chat;
pub mod connection;
pub mod embedding_presets;
pub mod embedding_settings;
pub mod embedding_workers_yaml;
pub mod embeddings;
pub mod import;
pub mod llm_presets;
pub mod llm_settings;
pub mod logs;
pub mod mcp_settings;
pub mod model;
pub mod periodic_execution;
pub mod periodic_tasks;
pub mod personality;
pub mod recovery;
pub mod reflection_dispatch;
pub mod reflections;
pub mod search;
pub mod settings;
pub mod setup;
pub mod summaries;
pub mod threads;

use std::collections::HashMap;
use std::sync::Arc;

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

pub(super) fn thread_summary_single_completed(raw: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    let position = v.get("position").and_then(|p| p.as_str()).unwrap_or("");
    position.contains("summarizeEach") && position.contains("recordSuccess")
}

/// Status of a single step in a streaming dispatch (import pipeline,
/// reflection generation). Shared between `commands::import` and
/// `commands::reflection_dispatch` so the frontend gets a single
/// `StepStatus` string-enum to switch on.
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
}

/// Tauri-managed state container. Held inside `AppHandle::state()`.
///
/// The gRPC clients are cached behind `Mutex<Option<..>>` (not `OnceCell`)
/// so a sidecar restart can invalidate them: ports are re-selected on each
/// start, and even an unchanged port gets a fresh TCP connection,
/// so a cached client would otherwise keep pointing at the stopped sidecar.
/// `retry_model_setup` calls `invalidate_clients()` around the restart.
pub struct AppState {
    pub sidecars: Arc<Sidecars>,
    pub data: DataPaths,
    memories_channel: Mutex<Option<Channel>>,
    jobworkerp: Mutex<Option<crate::jobworkerp::JobworkerpHandle>>,
    /// In-flight long-running dispatches keyed by the UI-side dispatch id.
    /// Shared by the RAG chat agent loop (`chat://step`), the import
    /// pipeline (`import://step`), and the analysis-tab dispatches
    /// (`summary://step`, `personality://step`). Each entry carries a
    /// cancellation token (so the driver bails out between steps) and the
    /// currently-running jobworkerp `JobId` so the cancel command can
    /// issue `JobService/Delete` against the live job — see
    /// OPEN-CHAT-2 / DECIDE-CHAT-4 for the chat origin and
    /// `plans/import-workflow-jobservice-delete-job-id-joyful-badger.md`
    /// for the generalisation to import/analysis.
    dispatch_in_flight: Mutex<HashMap<String, DispatchCancelEntry>>,
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

/// Back-compat alias so chat.rs (the original caller) keeps compiling
/// while the rest of the cancel pipeline migrates onto the generic name.
pub type ChatCancelEntry = DispatchCancelEntry;

impl AppState {
    pub fn new(sidecars: Arc<Sidecars>, data: DataPaths) -> Self {
        Self {
            sidecars,
            data,
            memories_channel: Mutex::new(None),
            jobworkerp: Mutex::new(None),
            dispatch_in_flight: Mutex::new(HashMap::new()),
        }
    }

    pub fn require_endpoints(&self) -> AppResult<SidecarEndpoints> {
        self.sidecars
            .current_endpoints()
            .ok_or_else(|| AppError::SidecarNotReady("not started".into()))
    }

    /// Resolve the gRPC targets honoring the connection override.
    /// Local mode uses the live sidecar ports; remote mode uses the configured
    /// URLs. Read on every (re)connect so a `set_connection_config` +
    /// `invalidate_clients` takes effect on the next command. Exposed to the
    /// import / reflection-dispatch commands so they target the same endpoints
    /// (incl. the workflow callback host/port/tls) as the browse-only clients.
    pub(super) fn resolve_targets(&self) -> AppResult<connection::ResolvedTargets> {
        let cfg = connection::load_connection_config(&self.data.connection_config_path());
        let local = self.sidecars.current_endpoints();
        connection::resolve_targets(&cfg, local.as_ref())
    }

    /// The active connection mode (local live sidecars / remote configured
    /// URLs). Read on every call, same read-on-dispatch pattern as
    /// `resolve_targets()`, so a `set_connection_config` takes effect on the
    /// next command without a restart.
    pub(super) fn connection_mode(&self) -> connection::ConnectionMode {
        connection::load_connection_config(&self.data.connection_config_path()).mode
    }

    /// Refuse an embedding-dependent dispatch when the local vector store is
    /// degraded (a dimension mismatch forced the memories child to restart
    /// with vectors disabled). Only gates in **local** connection mode:
    /// remote mode routes embedding query generation and index writes to the
    /// remote sidecar (`resolve_targets()` follows the config), which is
    /// unaffected by the local LanceDB. Used by both the embed-dependent
    /// search commands and the generation / import / write paths (their gate
    /// scope collapses to the same "local + degraded" rule).
    pub(super) fn ensure_local_embedding_available(&self) -> AppResult<()> {
        if self.connection_mode() == connection::ConnectionMode::Local
            && let Some(info) = self.sidecars.degraded()
        {
            return Err(AppError::VectorStoreDegraded {
                expected_dim: info.expected_dim,
                actual_dim: info.actual_dim,
            });
        }
        Ok(())
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
        *self.memories_channel.lock().await = None;
        *self.jobworkerp.lock().await = None;
    }

    /// Lazily establish — and then reuse — a single tonic channel to the
    /// memories sidecar. Tonic channels multiplex via HTTP/2 so all command
    /// handlers can share one. The `Mutex` serializes the connect so a burst
    /// of commands on startup opens exactly one channel.
    pub async fn memories_channel(&self) -> AppResult<Channel> {
        let target = self.resolve_targets()?;
        let mut guard = self.memories_channel.lock().await;
        if let Some(ch) = guard.as_ref() {
            return Ok(ch.clone());
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
        Ok(ch)
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
            .map_err(|e| {
                tracing::warn!(
                    endpoint = "jobworkerp",
                    url = %target.jobworkerp_url,
                    error = %e,
                    "gRPC connection failed"
                );
                connection::target_connect_error("jobworkerp", &target.jobworkerp_url, e)
            })?;
        *guard = Some(h.clone());
        Ok(h)
    }

    /// Insert a fresh [`DispatchCancelEntry`] for `dispatch_id` and
    /// return a clone the driver task can wire into its hop / step
    /// dispatch sites. An existing entry for the same id (e.g. a stale
    /// run the previous driver forgot to remove) is overwritten — the
    /// live driver owns the new token and the stale token never fires.
    pub async fn dispatch_register(&self, dispatch_id: &str) -> DispatchCancelEntry {
        let entry = DispatchCancelEntry::new();
        self.dispatch_in_flight
            .lock()
            .await
            .insert(dispatch_id.to_string(), entry.clone());
        entry
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

    /// Back-compat shim so `chat_ask` keeps calling `chat_register`. The
    /// underlying map is shared with import/analysis since dispatch ids
    /// are globally unique.
    pub async fn chat_register(&self, ui_job_id: &str) -> DispatchCancelEntry {
        self.dispatch_register(ui_job_id).await
    }

    /// Back-compat shim — see [`chat_register`](Self::chat_register).
    pub async fn chat_get(&self, ui_job_id: &str) -> Option<DispatchCancelEntry> {
        self.dispatch_get(ui_job_id).await
    }

    /// Back-compat shim — see [`chat_register`](Self::chat_register).
    pub async fn chat_take(&self, ui_job_id: &str) -> Option<DispatchCancelEntry> {
        self.dispatch_take(ui_job_id).await
    }
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
    entry.token.cancel();
    let live_jid = entry.current_job_id.lock().await.take();
    if let Some(jid) = live_jid {
        let job_id_value = jid.value;
        let handle = state.jobworkerp().await?;
        // Best-effort: the token is already flipped so the driver will
        // surface the cancelled terminal event even if Delete fails
        // (e.g. the job finished between the lock release and the gRPC
        // call landing).
        if let Err(e) = handle.cancel(jid).await {
            tracing::warn!(dispatch_id = %dispatch_id, job_id = job_id_value, err = %e, "JobService/Delete failed");
        } else {
            tracing::info!(dispatch_id = %dispatch_id, job_id = job_id_value, "cancel_dispatch issued JobService/Delete");
        }
    } else {
        tracing::info!(dispatch_id = %dispatch_id, "cancel_dispatch flipped token; no live JobId");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecar::SidecarConfig;

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

    fn dummy_state() -> AppState {
        let data = DataPaths::with_root("/tmp/lookback-appstate-test");
        let lance_home = data.lance_language_model_home();
        let sidecars = Arc::new(Sidecars::new(SidecarConfig {
            jobworkerp_bin: std::path::PathBuf::from("/bin/true"),
            memories_bin: std::path::PathBuf::from("/bin/true"),
            conductor_bin: std::path::PathBuf::from("/bin/true"),
            data: data.clone(),
            worker_yaml_paths: Vec::new(),
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

    /// AppState rooted at a caller-controlled temp dir so the connection
    /// config file can be written per-test (the shared `dummy_state` uses a
    /// fixed path and can't isolate connection mode).
    fn state_in(root: &std::path::Path) -> AppState {
        let data = DataPaths::with_root(root.to_path_buf());
        let lance_home = data.lance_language_model_home();
        let sidecars = Arc::new(Sidecars::new(SidecarConfig {
            jobworkerp_bin: std::path::PathBuf::from("/bin/true"),
            memories_bin: std::path::PathBuf::from("/bin/true"),
            conductor_bin: std::path::PathBuf::from("/bin/true"),
            data: data.clone(),
            worker_yaml_paths: Vec::new(),
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
        let (remote_jobworkerp_url, remote_memories_url) = match mode {
            connection::ConnectionMode::Remote => (
                Some("http://h:9000".to_string()),
                Some("http://h:9010".to_string()),
            ),
            connection::ConnectionMode::Local => (None, None),
        };
        connection::save_connection_config(
            &state.data.connection_config_path(),
            &connection::ConnectionConfig {
                mode,
                remote_jobworkerp_url,
                remote_memories_url,
            },
        )
        .unwrap();
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
        let state = dummy_state();
        state.invalidate_clients().await;
        state.invalidate_clients().await; // idempotent
        assert!(state.memories_channel.lock().await.is_none());
        assert!(state.jobworkerp.lock().await.is_none());
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
    async fn chat_register_shim_routes_through_dispatch_map() {
        // Back-compat: chat.rs still calls chat_register/chat_get. The
        // shim must hit the same map so a chat dispatch is cancellable
        // by both the legacy name and the generic one.
        let state = dummy_state();
        let entry = state.chat_register("chat-1").await;
        let via_dispatch = state.dispatch_get("chat-1").await.expect("entry present");
        via_dispatch.token.cancel();
        assert!(entry.token.is_cancelled());
        // Cleanup via the legacy alias must drop the shared entry.
        state.chat_take("chat-1").await;
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
