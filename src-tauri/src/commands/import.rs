//! Tauri commands backing Import. Drives the 4-step
//! pipeline (import → summary → personality → reflection) directly via
//! `jobworkerp::run_named_stream` so `import://step` events surface
//! per-chunk progress to the toast. The three post-import generation
//! steps are individually selectable from the dialog (`run_summary` /
//! `run_personality` / `run_reflection`); an unselected step is emitted
//! as a skipped `Waiting` rather than dispatched.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager, State};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{info, warn};

use super::connection::{ConnectionMode, MemoriesCallback, ResolvedTargets};
use super::{
    AppState, StepOutcome, StepStatus, cancel_dispatch_inner, emit_event, emit_generated_refresh,
    generated_refresh_scopes_from_position, json_as_i64,
};
use crate::error::{AppError, AppResult};
use crate::grpc::proto::llm_memory::data as mem_data;
use crate::grpc::proto::llm_memory::service as mem_svc;
use crate::grpc::proto::llm_memory::service::thread_service_client::ThreadServiceClient;
use crate::jobworkerp::{JobworkerpHandle, StreamEvent, run_cancellable_named_stream};

/// Completes the single maintenance scope after every source and selected
/// downstream generation step reaches a terminal outcome, including cancel
/// and early failure paths in the detached import driver.
struct ImportMaintenanceGuard {
    app: AppHandle,
    registered: bool,
}

impl Drop for ImportMaintenanceGuard {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        let app = self.app.clone();
        tauri::async_runtime::spawn(async move {
            if let Some(state) = app.try_state::<AppState>()
                && let Err(error) = super::finish_search_index_write(&state, true).await
            {
                tracing::warn!(%error, "import terminal maintenance reconcile failed");
            }
        });
    }
}

/// Single source of truth for the reflection `prompt_version`. Bumping this
/// string makes regeneration produce fresh reflections: the tuple
/// (thread_id, prompt_version, reflector_id) is the idempotency key in
/// FinalizeReflection, so `thread-reflection-single.yaml`'s `skipIfExisting`
/// step short-circuits any thread that already has a reflection under the same
/// version. Bump this in lockstep with a reflector prompt change. Shared by the
/// import pipeline (here) and the manual dispatch (`reflection_dispatch.rs`).
pub(super) const REFLECTION_PROMPT_VERSION: &str = "20260525-reflexion";
const PERSONALITY_MAX_CONTEXT_CHARS: i64 = 150_000;
const PERSONALITY_MERGE_MAX_SIGNALS: i64 = 100;
static IMPORT_ACTIVE: AtomicBool = AtomicBool::new(false);

struct ImportExecutionGuard;

impl ImportExecutionGuard {
    fn acquire() -> AppResult<Self> {
        IMPORT_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| AppError::Config("another import is already running".into()))?;
        Ok(Self)
    }
}

impl Drop for ImportExecutionGuard {
    fn drop(&mut self) {
        IMPORT_ACTIVE.store(false, Ordering::Release);
    }
}

/// Closes the native child-log scope after the detached import driver has
/// finished, including cancellation and early-return paths. Retention only
/// considers scopes marked closed, so this guard prevents deleting a running
/// import's native file.
struct NativeImportLogGuard(PathBuf);

impl Drop for NativeImportLogGuard {
    fn drop(&mut self) {
        if let Err(error) = crate::log_rotation::mark_native_log_dir_closed(&self.0) {
            tracing::debug!(path = %self.0.display(), %error, "could not mark native import log closed");
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum ImportSource {
    ClaudeCode,
    Codex,
    Plain,
}

impl ImportSource {
    fn cli_subcommand(self) -> &'static str {
        match self {
            ImportSource::ClaudeCode => "claude-code",
            ImportSource::Codex => "codex",
            ImportSource::Plain => "plain",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PendingImportSession {
    memories_endpoint_id: String,
    user_id: i64,
    source: ImportSource,
    session_key: String,
    thread_id: String,
}

const PENDING_IMPORT_FORMAT_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingImportScope {
    memories_endpoint_id: String,
    user_id: i64,
}

impl PendingImportScope {
    fn new(mode: ConnectionMode, memories_url: &str, user_id: i64) -> AppResult<Self> {
        Ok(Self {
            memories_endpoint_id: pending_memories_endpoint_id(mode, memories_url)?,
            user_id,
        })
    }

    fn contains(&self, session: &PendingImportSession) -> bool {
        session.memories_endpoint_id == self.memories_endpoint_id && session.user_id == self.user_id
    }
}

fn pending_memories_endpoint_id(mode: ConnectionMode, memories_url: &str) -> AppResult<String> {
    if mode == ConnectionMode::Local {
        return Ok("local".into());
    }
    let url = url::Url::parse(memories_url)
        .map_err(|error| AppError::Config(format!("invalid memories URL: {error}")))?;
    let scheme = match url.scheme() {
        "http" | "https" => url.scheme(),
        other => {
            return Err(AppError::Config(format!(
                "memories URL must be http or https, got {other}"
            )));
        }
    };
    let host = match url.host() {
        Some(url::Host::Domain(host)) => host.to_ascii_lowercase(),
        Some(url::Host::Ipv4(host)) => host.to_string(),
        Some(url::Host::Ipv6(host)) => format!("[{host}]"),
        None => return Err(AppError::Config("memories URL has no host".into())),
    };
    let port = url
        .port_or_known_default()
        .ok_or_else(|| AppError::Config("memories URL has no effective port".into()))?;
    let digest = Sha256::digest(format!("{scheme}://{host}:{port}").as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(format!("remote-sha256:{encoded}"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PendingImportState {
    format_version: u32,
    sessions: Vec<PendingImportSession>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifiedImportThread {
    thread_id: String,
    user_id: i64,
    memory_kind: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReconciledImportTargets {
    target_thread_ids: Vec<String>,
    candidate_intersection: Vec<String>,
    supplemented_ids: Vec<String>,
}

fn reconcile_import_targets(
    emitted_ids: &[String],
    candidate_ids: &[String],
    verified: &[VerifiedImportThread],
    expected_user_id: i64,
) -> AppResult<ReconciledImportTargets> {
    let candidates = candidate_ids
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    let verified_by_id = verified
        .iter()
        .map(|thread| (&thread.thread_id, thread))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut target_thread_ids = emitted_ids.to_vec();
    target_thread_ids.sort();
    target_thread_ids.dedup();
    let mut candidate_intersection = Vec::new();
    let mut supplemented_ids = Vec::new();
    for thread_id in &target_thread_ids {
        let thread = verified_by_id.get(thread_id).ok_or_else(|| {
            AppError::Config(format!("import target thread {thread_id} was not found"))
        })?;
        if thread.user_id != expected_user_id
            || thread.memory_kind != mem_data::MemoryKind::Raw as i32
        {
            return Err(AppError::Config(format!(
                "import target thread {thread_id} is not owned RAW data"
            )));
        }
        if candidates.contains(thread_id) {
            candidate_intersection.push(thread_id.clone());
        } else {
            supplemented_ids.push(thread_id.clone());
        }
    }
    Ok(ReconciledImportTargets {
        target_thread_ids,
        candidate_intersection,
        supplemented_ids,
    })
}

async fn verify_import_threads(
    memories_url: &str,
    thread_ids: &[String],
) -> AppResult<Vec<VerifiedImportThread>> {
    let mut client = ThreadServiceClient::new(crate::grpc::connect(memories_url).await?);
    let mut verified = Vec::with_capacity(thread_ids.len());
    for thread_id in thread_ids {
        let numeric_id = thread_id.parse::<i64>().map_err(|_| {
            AppError::Config(format!("invalid pending import thread id {thread_id}"))
        })?;
        let response = client
            .find(mem_data::ThreadId { value: numeric_id })
            .await?
            .into_inner();
        let thread = response.data.ok_or_else(|| {
            AppError::Config(format!("import target thread {thread_id} is absent"))
        })?;
        let data = thread.data.ok_or_else(|| {
            AppError::Config(format!("import target thread {thread_id} has no data"))
        })?;
        verified.push(VerifiedImportThread {
            thread_id: thread_id.clone(),
            user_id: data.user_id.map(|id| id.value).unwrap_or_default(),
            memory_kind: data.memory_kind,
        });
    }
    Ok(verified)
}

async fn find_import_window_candidates(
    memories_url: &str,
    user_id: i64,
    after: i64,
    before: i64,
) -> AppResult<Vec<String>> {
    if before <= after {
        return Ok(Vec::new());
    }
    let mut client = ThreadServiceClient::new(crate::grpc::connect(memories_url).await?);
    let request = mem_svc::FindThreadListByUserIdRequest {
        user_id: Some(mem_data::UserId { value: user_id }),
        limit: None,
        offset: None,
        sort: None,
        memory_kinds: vec![mem_data::MemoryKind::Raw as i32],
        thread_created_after: Some(after),
        thread_created_before: Some(before),
        thread_updated_after: None,
        thread_updated_before: None,
        first_message_after: None,
        first_message_before: None,
        last_message_after: None,
        last_message_before: None,
    };
    let mut stream = client
        .find_thread_list_by_user_id(request)
        .await?
        .into_inner();
    let mut ids = Vec::new();
    while let Some(thread) = tokio_stream::StreamExt::next(&mut stream).await {
        if let Some(id) = thread?.id {
            ids.push(id.value.to_string());
        }
    }
    Ok(ids)
}

impl Default for PendingImportState {
    fn default() -> Self {
        Self {
            format_version: PENDING_IMPORT_FORMAT_VERSION,
            sessions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct ImportCreationEventWire {
    version: u32,
    event: String,
    source: ImportSource,
    session_key: String,
    thread_id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct ImportSessionCompletedEvent {
    source: ImportSource,
    session_key: String,
    thread_id: String,
    imported_count: u64,
    success: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum ImportStdoutLine {
    Creation(ImportCreationEventWire),
    SessionCompleted(ImportSessionCompletedEvent),
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionDisposition {
    TrackPending,
    IgnoreExisting,
}

fn classify_session_completion(
    pending: &PendingImportState,
    scope: &PendingImportScope,
    completed: &ImportSessionCompletedEvent,
) -> AppResult<CompletionDisposition> {
    if !completed.success {
        return Err(AppError::Config(format!(
            "memories-import session failed: {}/{}",
            completed.source.cli_subcommand(),
            completed.session_key
        )));
    }
    let pending_for_key = pending.sessions.iter().find(|session| {
        scope.contains(session)
            && session.source == completed.source
            && session.session_key == completed.session_key
    });
    match pending_for_key {
        Some(session) if session.thread_id == completed.thread_id => {
            Ok(CompletionDisposition::TrackPending)
        }
        Some(_) => Err(AppError::Config(format!(
            "memories-import changed thread_id for pending session {}",
            completed.session_key
        ))),
        None => Ok(CompletionDisposition::IgnoreExisting),
    }
}

fn parse_import_stdout_line(line: &str) -> AppResult<ImportStdoutLine> {
    let value = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(value) => value,
        Err(error) if line.contains("memories-import-event") => {
            return Err(AppError::Config(format!(
                "malformed memories-import event: {error}"
            )));
        }
        Err(_) => return Ok(ImportStdoutLine::Text(line.to_string())),
    };
    if value.get("schema").and_then(|value| value.as_str()) != Some("memories-import-event") {
        return Ok(ImportStdoutLine::Text(line.to_string()));
    }
    let version = value.get("version").and_then(|value| value.as_u64());
    let event_name = value.get("event").and_then(|value| value.as_str());
    if version != Some(1) {
        return Err(AppError::Config(format!(
            "unsupported memories-import event version {:?}",
            version
        )));
    }
    if event_name == Some("session_completed") {
        let event: ImportSessionCompletedEvent = serde_json::from_value(value)
            .map_err(|error| AppError::Config(format!("invalid memories-import event: {error}")))?;
        validate_import_event_identity(&event.session_key, &event.thread_id)?;
        return Ok(ImportStdoutLine::SessionCompleted(event));
    }
    let event: ImportCreationEventWire = serde_json::from_value(value)
        .map_err(|error| AppError::Config(format!("invalid memories-import event: {error}")))?;
    if event.event != "thread_created" {
        return Err(AppError::Config(format!(
            "unsupported memories-import event {}@{}",
            event.event, event.version
        )));
    }
    validate_import_event_identity(&event.session_key, &event.thread_id)?;
    Ok(ImportStdoutLine::Creation(event))
}

fn reported_imported_count(lines: &[String]) -> Option<usize> {
    lines.iter().find_map(|line| {
        line.trim()
            .strip_prefix("Memories imported:")
            .and_then(|count| count.trim().parse().ok())
    })
}

fn validate_import_event_identity(session_key: &str, thread_id: &str) -> AppResult<()> {
    if session_key.trim().is_empty() {
        return Err(AppError::Config(
            "memories-import event session_key must not be empty".into(),
        ));
    }
    let parsed_id = thread_id.parse::<i64>().map_err(|_| {
        AppError::Config("memories-import event thread_id must be a decimal i64 string".into())
    })?;
    if parsed_id <= 0 || thread_id != parsed_id.to_string() {
        return Err(AppError::Config(
            "memories-import event thread_id must be a canonical positive decimal string".into(),
        ));
    }
    Ok(())
}

fn validate_import_event_source(
    expected_source: ImportSource,
    event_source: ImportSource,
) -> AppResult<()> {
    if event_source == expected_source {
        return Ok(());
    }
    Err(AppError::Config(format!(
        "memories-import lifecycle event source mismatch: expected {}, got {}",
        expected_source.cli_subcommand(),
        event_source.cli_subcommand()
    )))
}

fn load_pending_imports(path: &Path) -> AppResult<PendingImportState> {
    match std::fs::read(path) {
        Ok(raw) => {
            let value: serde_json::Value = serde_json::from_slice(&raw)
                .map_err(|error| AppError::Config(format!("read import pending state: {error}")))?;
            let format_version = value
                .get("format_version")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    AppError::Config("import pending state format_version is missing".into())
                })?;
            if format_version == 1 {
                let sessions = value
                    .get("sessions")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| {
                        AppError::Config("legacy import pending state sessions is invalid".into())
                    })?;
                if sessions.is_empty() {
                    return Ok(PendingImportState::default());
                }
                return Err(AppError::Config(format!(
                    "import pending state format_version=1 contains sessions without a Memories endpoint or user scope; inspect and move aside {} before importing",
                    path.display()
                )));
            }
            if format_version != u64::from(PENDING_IMPORT_FORMAT_VERSION) {
                return Err(AppError::Config(format!(
                    "unsupported import pending state format_version: {format_version}"
                )));
            }
            serde_json::from_value(value)
                .map_err(|error| AppError::Config(format!("read import pending state: {error}")))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(PendingImportState::default())
        }
        Err(error) => Err(error.into()),
    }
}

fn save_pending_imports(path: &Path, state: &PendingImportState) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("import pending path has no parent".into()))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".import-pending-{}.tmp", std::process::id()));
    let raw = serde_json::to_vec_pretty(state)
        .map_err(|error| AppError::Config(format!("serialize import pending state: {error}")))?;
    let mut file = std::fs::File::create(&tmp)?;
    use std::io::Write;
    file.write_all(&raw)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn record_pending_import(
    path: &Path,
    state: &mut PendingImportState,
    session: PendingImportSession,
) -> AppResult<()> {
    if let Some(existing) = state.sessions.iter_mut().find(|existing| {
        existing.memories_endpoint_id == session.memories_endpoint_id
            && existing.user_id == session.user_id
            && existing.source == session.source
            && existing.session_key == session.session_key
    }) {
        if existing.thread_id != session.thread_id {
            return Err(AppError::Config(format!(
                "memories-import changed thread_id for stable session key {}",
                session.session_key
            )));
        }
    } else {
        state.sessions.push(session);
    }
    state.sessions.sort_by(|left, right| {
        (
            &left.memories_endpoint_id,
            left.user_id,
            &left.source,
            &left.session_key,
        )
            .cmp(&(
                &right.memories_endpoint_id,
                right.user_id,
                &right.source,
                &right.session_key,
            ))
    });
    save_pending_imports(path, state)
}

fn completed_pending_state(
    current: &PendingImportState,
    targeted: &[PendingImportSession],
    downstream_completed: bool,
) -> PendingImportState {
    if !downstream_completed {
        return current.clone();
    }
    let mut next = current.clone();
    next.sessions.retain(|session| !targeted.contains(session));
    next
}

fn select_targeted_sessions(
    pending: &PendingImportState,
    scope: &PendingImportScope,
    completions: &[ImportSessionCompletedEvent],
    selected_sources: &[ImportSource],
) -> Vec<PendingImportSession> {
    pending
        .sessions
        .iter()
        .filter(|session| {
            scope.contains(session)
                && selected_sources.contains(&session.source)
                && completions.iter().any(|completed| {
                    completed.success
                        && completed.source == session.source
                        && completed.session_key == session.session_key
                        && completed.thread_id == session.thread_id
                })
        })
        .cloned()
        .collect()
}

fn should_dispatch_import_targets(target_thread_ids: &[String]) -> bool {
    !target_thread_ids.is_empty()
}

/// How `memories-import plain` groups discovered files into threads. Mirrors
/// the CLI's `--thread-strategy` values; serde decodes the kebab-case wire
/// form the dialog sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThreadStrategy {
    PerFile,
    PerDir,
    Single,
}

impl ThreadStrategy {
    fn cli_value(self) -> &'static str {
        match self {
            ThreadStrategy::PerFile => "per-file",
            ThreadStrategy::PerDir => "per-dir",
            ThreadStrategy::Single => "single",
        }
    }
}

/// Plain-source-specific parameters. Unlike claude-code / codex (which
/// auto-discover their well-known log roots), the plain importer needs an
/// explicit directory plus a grouping strategy, so these travel as a separate
/// optional block referenced only when `"plain"` is among `sources`.
#[derive(Debug, Clone, Deserialize)]
pub struct PlainImportConfig {
    /// Root directory to walk recursively.
    pub root: PathBuf,
    /// Channel / external_id namespace prefix. `None` lets the CLI apply its
    /// own default (`plain`); when set it must match `^[a-z0-9_-]{1,32}$`.
    #[serde(default)]
    pub source_name: Option<String>,
    pub thread_strategy: ThreadStrategy,
}

impl PlainImportConfig {
    /// `^[a-z0-9_-]{1,32}$` without pulling in the regex crate — the same
    /// charset the memories CLI enforces on `--source-name`.
    fn source_name_is_valid(name: &str) -> bool {
        (1..=32).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartImportRequest {
    pub sources: Vec<ImportSource>,
    /// ISO-8601 string (e.g. `2026-05-01T00:00:00Z`) passed verbatim as `--since`.
    /// `None` means "all".
    pub since: Option<String>,
    pub user_id: Option<i64>,
    pub dry_run: bool,
    pub labels: Vec<String>,
    /// Path to the `memories-import` binary. Resolved by the frontend from
    /// `get_settings()` (or built-in default).
    pub memories_import_bin: PathBuf,
    /// Post-import generation toggles. Required (not Option) so the dialog
    /// always sends its checkbox state; all-true reproduces the legacy
    /// "run everything" behaviour. Each gates the matching downstream
    /// `run_stream_step`; an unselected step is emitted as `Waiting`
    /// ("スキップ") instead of being dispatched.
    pub run_summary: bool,
    pub run_personality: bool,
    pub run_reflection: bool,
    /// Frontend-supplied dispatch id (UUID) used as the cancel key. The
    /// same id is returned in the response so the toast can wire its
    /// Stop button. Older callers may omit it; the backend then
    /// synthesizes a timestamp-shaped id as it did before cancel landed.
    #[serde(default)]
    pub dispatch_id: Option<String>,
    /// Plain-source parameters. Required when `sources` contains `"plain"`,
    /// ignored otherwise. `#[serde(default)]` keeps older request shapes (and
    /// the claude/codex-only flow) decoding without it.
    #[serde(default)]
    pub plain: Option<PlainImportConfig>,
}

/// Which post-import generation steps to run, decoded from the request
/// flags. Extracted as a pure value so the gating logic is unit-testable
/// without spawning the sidecar-dependent import task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DownstreamPlan {
    summary: bool,
    personality: bool,
    reflection: bool,
}

impl DownstreamPlan {
    fn from_request(req: &StartImportRequest) -> Self {
        Self {
            summary: req.run_summary,
            personality: req.run_personality,
            reflection: req.run_reflection,
        }
    }
}

/// Return downstream steps in their execution order with their selection state.
/// Keeping this table in one place prevents terminal no-op paths from drifting
/// away from the normal import pipeline.
fn downstream_steps(plan: DownstreamPlan) -> [(ImportStep, bool); 3] {
    [
        (ImportStep::ThreadSummary, plan.summary),
        (ImportStep::ThreadPersonality, plan.personality),
        (ImportStep::Reflection, plan.reflection),
    ]
}

/// Decide the terminal state for each downstream step when no import targets
/// exist. This pure table keeps the no-op behavior testable independently of
/// Tauri event emission.
fn no_target_downstream_decisions(
    plan: DownstreamPlan,
) -> [(ImportStep, StepStatus, &'static str); 3] {
    downstream_steps(plan).map(|(step, selected)| {
        if selected {
            (step, StepStatus::Done, "対象スレッドなし")
        } else {
            (step, StepStatus::Waiting, "スキップ")
        }
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportStepUpdate {
    pub job_id: String,
    pub step: ImportStep,
    pub status: StepStatus,
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice_code: Option<ImportNoticeCode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportStep {
    ThreadImport,
    ThreadSummary,
    ThreadPersonality,
    Reflection,
}

/// Stable machine-readable notices attached to an import step. Optional
/// serialization keeps older event consumers compatible with the original
/// `ImportStepUpdate` shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportNoticeCode {
    NoImportableLogSources,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManualImportSourceSelection {
    sources: Vec<ImportSource>,
    no_importable_log_sources: bool,
}

/// Filter only the well-known Claude Code / Codex roots for the manual import
/// command. Plain imports intentionally pass through untouched: their root is
/// validated by `validate_plain`, and periodic imports do not use this path.
fn filter_manual_import_sources(
    requested: &[ImportSource],
    home_dir: Option<&Path>,
) -> ManualImportSourceSelection {
    let mut sources = Vec::with_capacity(requested.len());
    for source in requested {
        let root = match source {
            ImportSource::ClaudeCode => home_dir.map(|home| home.join(".claude/projects")),
            ImportSource::Codex => home_dir.map(|home| home.join(".codex/sessions")),
            ImportSource::Plain => None,
        };
        let importable = match source {
            ImportSource::Plain => true,
            ImportSource::ClaudeCode | ImportSource::Codex => {
                root.as_ref().map(|path| path.is_dir()).unwrap_or(false)
            }
        };
        if importable {
            sources.push(*source);
        } else {
            match root {
                Some(path) => warn!(
                    source = source.cli_subcommand(),
                    path = %path.display(),
                    "manual import source root is missing; skipping source"
                ),
                None => warn!(
                    source = source.cli_subcommand(),
                    "manual import source root is unavailable; skipping source"
                ),
            }
        }
    }
    let no_importable_log_sources = !requested.is_empty() && sources.is_empty();
    ManualImportSourceSelection {
        sources,
        no_importable_log_sources,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StartImportResponse {
    pub job_id: String,
}

/// Outcome of a single `memories-import` child, fed into the aggregator.
#[derive(Debug, Clone)]
enum ChildOutcome {
    /// `summary` is the per-source `<label> summary:` block scraped from
    /// stdout. `None` when the child produced no recognisable summary
    /// block (rare — e.g. a non-zero exit before the summary printed).
    Success {
        summary: Option<String>,
    },
    Failure(String),
}

/// Tracks per-job completion across all spawned `memories-import` children
/// (one per selected source). Lets us emit the `thread-import` terminal step
/// exactly once — when the last child finishes — instead of letting the
/// fastest source clobber the slower one's eventual result.
struct ImportAggregator {
    total: usize,
    completed: AtomicUsize,
    failures: parking_lot::Mutex<Vec<String>>,
    summaries: parking_lot::Mutex<Vec<String>>,
}

/// What the aggregator decided once the final child reported in. `None` means
/// "not the last child yet — say nothing".
#[derive(Debug, Clone, PartialEq, Eq)]
enum AggregatedState {
    AllSucceeded { summaries: Vec<String> },
    AnyFailed(Vec<String>),
}

impl ImportAggregator {
    fn new(total: usize) -> Self {
        Self {
            total,
            completed: AtomicUsize::new(0),
            failures: parking_lot::Mutex::new(Vec::new()),
            summaries: parking_lot::Mutex::new(Vec::new()),
        }
    }

    fn record(&self, outcome: ChildOutcome) -> Option<AggregatedState> {
        match outcome {
            ChildOutcome::Failure(msg) => self.failures.lock().push(msg),
            ChildOutcome::Success { summary: Some(s) } => self.summaries.lock().push(s),
            ChildOutcome::Success { summary: None } => {}
        }
        let done = self.completed.fetch_add(1, Ordering::AcqRel) + 1;
        if done < self.total {
            return None;
        }
        let failures = std::mem::take(&mut *self.failures.lock());
        let summaries = std::mem::take(&mut *self.summaries.lock());
        Some(if failures.is_empty() {
            AggregatedState::AllSucceeded { summaries }
        } else {
            AggregatedState::AnyFailed(failures)
        })
    }
}

/// Cross-checks the `plain` source against its config block: presence on both
/// sides must agree, and when present the directory must exist and the
/// (optional) source name must match the CLI's charset. Pure so the dialog's
/// guard rails are unit-testable without spawning the importer.
fn validate_plain(req: &StartImportRequest) -> AppResult<()> {
    let wants_plain = req.sources.iter().any(|s| matches!(s, ImportSource::Plain));
    let cfg = match (wants_plain, &req.plain) {
        (false, None) => return Ok(()),
        (false, Some(_)) => {
            return Err(AppError::Config(
                "plain config provided but 'plain' is not in sources".into(),
            ));
        }
        (true, None) => {
            return Err(AppError::Config(
                "plain source selected but no plain config was provided".into(),
            ));
        }
        (true, Some(cfg)) => cfg,
    };

    if let Some(name) = &cfg.source_name
        && !PlainImportConfig::source_name_is_valid(name)
    {
        return Err(AppError::Config(format!(
            "invalid plain source-name '{name}': must match ^[a-z0-9_-]{{1,32}}$"
        )));
    }
    // The importer runs as a local child against a local path, so the root is
    // on this machine — stat it here for an early, clear error.
    if !cfg.root.is_dir() {
        return Err(AppError::Config(format!(
            "plain root is not a directory: {}",
            cfg.root.display()
        )));
    }
    Ok(())
}

#[tauri::command]
pub async fn start_import(
    app: AppHandle,
    state: State<'_, AppState>,
    req: StartImportRequest,
) -> AppResult<StartImportResponse> {
    let import_execution = ImportExecutionGuard::acquire()?;
    // Validate up front so a half-spawned set of children can't be left
    // behind when an invalid source slips into the middle of the list.
    validate_plain(&req)?;
    let job_id = req
        .dispatch_id
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("import-{}", chrono::Utc::now().timestamp_millis()));
    let source_selection = filter_manual_import_sources(&req.sources, dirs::home_dir().as_deref());
    let sources = source_selection.sources;
    let downstream = DownstreamPlan::from_request(&req);
    if source_selection.no_importable_log_sources {
        emit_no_importable_log_sources(&app, &job_id, downstream);
        return Ok(StartImportResponse { job_id });
    }

    // Import writes memory embeddings into the local LanceDB; refuse when it
    // is degraded (local mode only — a remote import writes to the remote
    // vector store, which is unaffected).
    state.ensure_local_embedding_available()?;

    // Honor the connection override: import targets the same
    // memories / jobworkerp the browse clients use — local sidecar by default,
    // or a remote server (incl. HTTPS) when configured.
    let resolved_connection = state.resolve_connection()?;
    let connection_mode = resolved_connection.mode;
    let targets = resolved_connection.targets;
    let memories_url_for_validation = targets.memories_url.clone();
    let use_local_candidate_window = connection_mode == ConnectionMode::Local;
    let import_started_after = chrono::Utc::now().timestamp_millis().saturating_sub(1);
    let callback = targets.memories_callback()?;
    let user_id = req.user_id.unwrap_or(1);
    let pending_scope = PendingImportScope::new(connection_mode, &targets.memories_url, user_id)?;

    emit_step(
        &app,
        &job_id,
        ImportStep::ThreadImport,
        StepStatus::Active,
        None,
    );

    // dry-run skips dispatch entirely (workflows would fan out new jobs
    // against threads that were never imported). Failing eagerly when
    // the workflows bundle is missing avoids spawning the importer
    // pointlessly.
    let dispatch_ctx = if req.dry_run {
        None
    } else {
        let d = BatchDispatch::resolve(
            &callback,
            user_id,
            None,
            state.active_llm_worker_name().to_string(),
            state.active_output_language(),
        )?;
        let h = state.jobworkerp().await?;
        Some((d, h))
    };

    let selected_sources = sources.clone();
    let pending_path = state.data.import_pending_path();
    let pending_imports = Arc::new(parking_lot::Mutex::new(load_pending_imports(
        &pending_path,
    )?));
    let completed_sessions = Arc::new(parking_lot::Mutex::new(
        Vec::<ImportSessionCompletedEvent>::new(),
    ));
    let app_for_task = app.clone();
    let job_id_for_task = job_id.clone();
    let aggregator = Arc::new(ImportAggregator::new(sources.len()));
    let dry_run = req.dry_run;
    // Persist the conservative dirty generation before the import process is
    // dispatched. Dry-runs do not create a scope because they cannot write.
    let maintenance_registered = if dry_run {
        false
    } else {
        super::begin_search_index_write(
            &state,
            &[
                crate::search_index_maintenance::MaintenanceTable::Memory,
                crate::search_index_maintenance::MaintenanceTable::Thread,
            ],
        )
        .await?
    };
    let log_dir = state.data.log_dir();
    let native_log_dir = crate::log_rotation::native_log_dir(&log_dir, &format!("import-{job_id}"))
        .map_err(|error| {
            AppError::Config(format!(
                "could not create native import log directory: {error}"
            ))
        })?;
    let plan = ImportPlan::new(
        req.memories_import_bin.clone(),
        user_id,
        &targets,
        req.since.clone(),
        req.dry_run,
        req.labels.clone(),
        native_log_dir,
        req.plain.clone(),
    );
    let entry = state.dispatch_register(&job_id).await;
    let native_log_dir_for_task = plan.native_log_dir.clone();

    tokio::spawn(async move {
        let _import_execution = import_execution;
        let _native_import_log = NativeImportLogGuard(native_log_dir_for_task);
        let _maintenance = ImportMaintenanceGuard {
            app: app_for_task.clone(),
            registered: maintenance_registered,
        };
        let cancel = entry.token.clone();
        let current_job_id = entry.current_job_id.clone();
        let current_step = entry.current_step.clone();
        // RAII so a panic anywhere in the import driver still purges the
        // AppState entry — mirrors the chat path's `ChatInFlightGuard`.
        let _cleanup = ImportCleanupGuard::new(app_for_task.clone(), job_id_for_task.clone());

        // Step 1: thread-import (memories-import CLI, sequential per
        // source). The aggregator's `record` drains itself on the last
        // call; `take_final` is the fallback for the case where the
        // last `record` somehow didn't surface (shouldn't happen with
        // serial execution but failing-open beats freezing the toast).
        *current_step.lock().await = Some("thread-import".into());
        let mut import_state: Option<AggregatedState> = None;
        let mut cancelled_mid_import = false;
        for source in sources {
            if cancel.is_cancelled() {
                cancelled_mid_import = true;
                break;
            }
            let cmd = plan.build_command(source);
            let outcome = run_one_source(
                cmd,
                ImportLogContext {
                    aggregator: aggregator.clone(),
                    pending_path: pending_path.clone(),
                    pending_imports: pending_imports.clone(),
                    completed_sessions: completed_sessions.clone(),
                    pending_scope: pending_scope.clone(),
                    expected_source: source,
                    dry_run,
                    cancel: cancel.clone(),
                    native_log_dir: Some(plan.native_log_dir.clone()),
                },
            )
            .await;
            if cancel.is_cancelled() {
                cancelled_mid_import = true;
                break;
            }
            if let Some(state) = outcome {
                import_state = Some(state);
            }
        }
        if cancelled_mid_import {
            emit_step(
                &app_for_task,
                &job_id_for_task,
                ImportStep::ThreadImport,
                StepStatus::Failed,
                Some(CANCELLED_ACTIVE_MESSAGE.into()),
            );
            mark_downstream_skipped_by_cancel(&app_for_task, &job_id_for_task);
            return;
        }
        let import_state = import_state.unwrap_or_else(|| aggregator.take_final());
        let succeeded = matches!(import_state, AggregatedState::AllSucceeded { .. });
        emit_thread_import_terminal(&app_for_task, &job_id_for_task, import_state, dry_run);

        let Some((mut dispatch, handle)) = dispatch_ctx else {
            mark_downstream(
                &app_for_task,
                &job_id_for_task,
                StepStatus::Waiting,
                "dry-run",
            );
            return;
        };
        if !succeeded {
            mark_downstream(
                &app_for_task,
                &job_id_for_task,
                StepStatus::Waiting,
                "skipped: import failed",
            );
            return;
        }
        let current_completions = completed_sessions.lock().clone();
        let targeted_sessions = select_targeted_sessions(
            &pending_imports.lock(),
            &pending_scope,
            &current_completions,
            &selected_sources,
        );
        let mut target_thread_ids = targeted_sessions
            .iter()
            .map(|session| session.thread_id.clone())
            .collect::<Vec<_>>();
        target_thread_ids.sort();
        target_thread_ids.dedup();
        let candidate_ids = if use_local_candidate_window {
            match find_import_window_candidates(
                &memories_url_for_validation,
                user_id,
                import_started_after,
                chrono::Utc::now().timestamp_millis(),
            )
            .await
            {
                Ok(ids) => ids,
                Err(error) => {
                    mark_downstream(
                        &app_for_task,
                        &job_id_for_task,
                        StepStatus::Failed,
                        &error.to_string(),
                    );
                    return;
                }
            }
        } else {
            Vec::new()
        };
        let verified =
            match verify_import_threads(&memories_url_for_validation, &target_thread_ids).await {
                Ok(verified) => verified,
                Err(error) => {
                    mark_downstream(
                        &app_for_task,
                        &job_id_for_task,
                        StepStatus::Failed,
                        &error.to_string(),
                    );
                    return;
                }
            };
        let targets = match reconcile_import_targets(
            &target_thread_ids,
            &candidate_ids,
            &verified,
            user_id,
        ) {
            Ok(targets) => targets,
            Err(error) => {
                mark_downstream(
                    &app_for_task,
                    &job_id_for_task,
                    StepStatus::Failed,
                    &error.to_string(),
                );
                return;
            }
        };
        info!(
            target_count = targets.target_thread_ids.len(),
            candidate_intersection_count = targets.candidate_intersection.len(),
            supplemented_count = targets.supplemented_ids.len(),
            unrelated_candidate_count = candidate_ids
                .len()
                .saturating_sub(targets.candidate_intersection.len()),
            "resolved import generation target IDs"
        );
        if !should_dispatch_import_targets(&targets.target_thread_ids) {
            emit_no_target_downstream(&app_for_task, &job_id_for_task, downstream);
            return;
        }
        dispatch.target_thread_ids = Some(targets.target_thread_ids);

        // Step 2-4 run sequentially; each step's stream
        // blocks until the worker's stream closes. They're independent
        // workflows so a failed summary does not skip personality /
        // reflection. Each step is individually gated by the dialog's
        // checkboxes; an unselected step is surfaced as `Waiting`
        // ("スキップ") so the toast distinguishes it from a stuck step.
        // A user-triggered cancel between/within steps converts the
        // remaining ones to `Failed` + "中断によりスキップ" via the
        // `downstream_after_cancel` decision table — see
        // `decide_after_cancel` for the pure logic + tests.
        let mut downstream_completed = true;
        for (step, run, worker, input) in [
            (
                ImportStep::ThreadSummary,
                downstream.summary,
                "memories-summarize-batch",
                dispatch.summarize_input(),
            ),
            (
                ImportStep::ThreadPersonality,
                downstream.personality,
                "memories-personality-batch",
                dispatch.personality_input(),
            ),
            (
                ImportStep::Reflection,
                downstream.reflection,
                "memories-reflection-batch",
                dispatch.reflection_input(),
            ),
        ] {
            if cancel.is_cancelled() {
                downstream_completed = false;
                emit_step(
                    &app_for_task,
                    &job_id_for_task,
                    step,
                    StepStatus::Failed,
                    Some(CANCELLED_SKIP_MESSAGE.into()),
                );
                continue;
            }
            if !run {
                emit_skipped(&app_for_task, &job_id_for_task, step);
                continue;
            }
            *current_step.lock().await = Some(step_label(step).into());
            let completed = run_cancellable_step(
                &app_for_task,
                &job_id_for_task,
                step,
                &handle,
                worker,
                input,
                cancel.clone(),
                current_job_id.clone(),
            )
            .await;
            downstream_completed &= completed;
        }
        if downstream_completed {
            let mut guard = pending_imports.lock();
            let next = completed_pending_state(&guard, &targeted_sessions, true);
            if let Err(error) = save_pending_imports(&pending_path, &next) {
                warn!(error = %error, "could not clear completed import pending state");
            } else {
                *guard = next;
            }
        }
    });

    Ok(StartImportResponse { job_id })
}

/// Cancel an in-flight import pipeline. Idempotent: an unknown id is a
/// no-op so the toast can fire-and-forget on every Stop click. Mirrors
/// `chat_cancel` semantics — flips the token (so the next-step gate
/// short-circuits) and `JobService/Delete`s the live workflow job (so
/// the LLM slot is released immediately instead of running to
/// completion).
#[tauri::command]
pub async fn start_import_cancel(state: State<'_, AppState>, dispatch_id: String) -> AppResult<()> {
    cancel_dispatch_inner(&state, &dispatch_id).await
}

/// Stable label for the `current_step` slot — used by tests and for the
/// log line a future deferred-item review can read off the trace.
fn step_label(step: ImportStep) -> &'static str {
    match step {
        ImportStep::ThreadImport => "thread-import",
        ImportStep::ThreadSummary => "thread-summary",
        ImportStep::ThreadPersonality => "thread-personality",
        ImportStep::Reflection => "reflection",
    }
}

/// User-facing terminal message for the step that the cancel actually
/// interrupted. Distinguishes the active step from the downstream
/// "scheduled but skipped" ones so the toast surfaces both clearly.
const CANCELLED_ACTIVE_MESSAGE: &str = "中断";
const CANCELLED_SKIP_MESSAGE: &str = "中断によりスキップ";

/// Mark the downstream summary/personality/reflection steps as
/// `Failed("中断によりスキップ")` when the active thread-import was
/// cancelled mid-flight (before any downstream had a chance to run).
fn mark_downstream_skipped_by_cancel(app: &AppHandle, job_id: &str) {
    for step in [
        ImportStep::ThreadSummary,
        ImportStep::ThreadPersonality,
        ImportStep::Reflection,
    ] {
        emit_step(
            app,
            job_id,
            step,
            StepStatus::Failed,
            Some(CANCELLED_SKIP_MESSAGE.into()),
        );
    }
}

/// RAII purger for `AppState::dispatch_in_flight`. Matches `chat.rs`'s
/// `ChatInFlightGuard` — release builds compile out via `panic = "abort"`
/// but a dev/test panic must still leave the map clean for the next run.
struct ImportCleanupGuard {
    app: AppHandle,
    job_id: String,
}

impl ImportCleanupGuard {
    fn new(app: AppHandle, job_id: String) -> Self {
        Self { app, job_id }
    }
}

impl Drop for ImportCleanupGuard {
    fn drop(&mut self) {
        // Bare `tokio::spawn` would panic if the runtime is already shut
        // down (e.g. RunEvent::ExitRequested); the map vanishes with the
        // process so a missed cleanup is harmless.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        use tauri::Manager;
        let app = self.app.clone();
        let job_id = std::mem::take(&mut self.job_id);
        handle.spawn(async move {
            if let Some(state) = app.try_state::<AppState>() {
                let _ = state.dispatch_take(&job_id).await;
            }
        });
    }
}

impl ImportAggregator {
    /// Forced terminal extraction — used after the source loop completes
    /// regardless of whether the last `record` returned `Some` (single
    /// source case always returns `Some`; multi-source needs this to
    /// catch the silent path).
    fn take_final(&self) -> AggregatedState {
        let failures = std::mem::take(&mut *self.failures.lock());
        let summaries = std::mem::take(&mut *self.summaries.lock());
        if failures.is_empty() {
            AggregatedState::AllSucceeded { summaries }
        } else {
            AggregatedState::AnyFailed(failures)
        }
    }
}

/// Per-job invocation context shared across every source's CLI invocation.
struct ImportPlan {
    bin: PathBuf,
    user_id: i64,
    memories_url: String,
    jobworkerp_addr: String,
    since: Option<String>,
    dry_run: bool,
    labels: Vec<String>,
    /// Dedicated directory for this import's native tracing output. The
    /// parent-captured stdout/stderr files remain the canonical logs.
    native_log_dir: PathBuf,
    /// Plain-source parameters, consumed by `build_command`'s plain arm.
    /// `None` for a request that doesn't select the plain source.
    plain: Option<PlainImportConfig>,
}

impl ImportPlan {
    #[allow(clippy::too_many_arguments)]
    fn new(
        bin: PathBuf,
        user_id: i64,
        targets: &ResolvedTargets,
        since: Option<String>,
        dry_run: bool,
        labels: Vec<String>,
        native_log_dir: PathBuf,
        plain: Option<PlainImportConfig>,
    ) -> Self {
        Self {
            bin,
            user_id,
            memories_url: targets.memories_url.clone(),
            jobworkerp_addr: targets.jobworkerp_url.clone(),
            since,
            dry_run,
            labels,
            native_log_dir,
            plain,
        }
    }

    fn build_command(&self, source: ImportSource) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--user-id")
            .arg(self.user_id.to_string())
            .arg("--server-url")
            .arg(&self.memories_url)
            .arg("--events-jsonl");

        if let Some(since) = self.since.as_deref() {
            cmd.arg("--since").arg(since);
        }
        if self.dry_run {
            cmd.arg("--dry-run");
        }

        cmd.arg(source.cli_subcommand());

        if !self.labels.is_empty() {
            cmd.arg("--labels").arg(self.labels.join(","));
        }

        match source {
            ImportSource::ClaudeCode => {
                cmd.arg("--all-projects");
            }
            ImportSource::Codex => {
                cmd.arg("--all-sessions");
            }
            ImportSource::Plain => {
                // `validate_plain` guarantees the config is present whenever
                // the plain source reaches the plan. These are subcommand-scoped
                // args, so they must follow the `plain` token emitted above.
                let cfg = self
                    .plain
                    .as_ref()
                    .expect("validated: plain config present when plain source is selected");
                cmd.arg("--root")
                    .arg(&cfg.root)
                    .arg("--thread-strategy")
                    .arg(cfg.thread_strategy.cli_value());
                if let Some(name) = &cfg.source_name {
                    cmd.arg("--source-name").arg(name);
                }
            }
        }

        cmd.env("JOBWORKERP_ADDR", &self.jobworkerp_addr)
            .env("RUST_LOG", "info")
            // Force the child's native tracing log into this import's isolated
            // data-root scope; otherwise it defaults to cwd, which is `/` for
            // a Finder-launched .app and panics on the unwritable root.
            //
            // The child reads its log config via `envy::prefixed("LOG_")` into
            // a struct whose `use_json` / `use_stdout` are plain `bool` (no
            // serde default), so envy treats them as REQUIRED: missing either
            // makes the whole deserialize fail and the importer silently falls
            // back to `current_dir()` — which is exactly why LOG_FILE_DIR alone
            // had no effect. We must set all three for the dir to take.
            .env("LOG_FILE_DIR", &self.native_log_dir)
            .env("LOG_USE_JSON", "true")
            .env("LOG_APP_NAME", "Lookback")
            .env("LOG_USE_STDOUT", "true")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        cmd
    }
}

/// Period-summary granularity. Each kind reads the layer below's output and
/// writes its own: daily aggregates per-thread `summary` memories, weekly
/// aggregates `daily_summary`, monthly aggregates `weekly_summary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PeriodKind {
    Daily,
    Weekly,
    Monthly,
}

/// All per-kind constants for a period layer, kept in one table so the
/// worker name, workflow path, window field, and the source→output label
/// chain can't drift apart. Field keys (`*_label_key`) match the batch
/// workflow's input schema.
struct PeriodSpec {
    worker_name: &'static str,
    job_prefix: &'static str,
    last_n_field: &'static str,
    source_label_key: &'static str,
    source_label: &'static str,
    out_label_key: &'static str,
    out_label: &'static str,
}

impl PeriodKind {
    fn spec(self) -> PeriodSpec {
        match self {
            PeriodKind::Daily => PeriodSpec {
                worker_name: "memories-daily-summary-batch",
                job_prefix: "daily",
                last_n_field: "last_n_days",
                source_label_key: "summary_label",
                source_label: "summary",
                out_label_key: "daily_label",
                out_label: "daily_summary",
            },
            PeriodKind::Weekly => PeriodSpec {
                worker_name: "memories-weekly-summary-batch",
                job_prefix: "weekly",
                last_n_field: "last_n_weeks",
                source_label_key: "daily_label",
                source_label: "daily_summary",
                out_label_key: "weekly_label",
                out_label: "weekly_summary",
            },
            PeriodKind::Monthly => PeriodSpec {
                worker_name: "memories-monthly-summary-batch",
                job_prefix: "monthly",
                last_n_field: "last_n_months",
                source_label_key: "weekly_label",
                source_label: "weekly_summary",
                out_label_key: "monthly_label",
                out_label: "monthly_summary",
            },
        }
    }

    /// Worker name the dispatch targets (`memories-{kind}-summary-batch`).
    pub(super) fn worker_name(self) -> &'static str {
        self.spec().worker_name
    }

    /// `job_id` prefix used to correlate the `summary://step` progress slot.
    pub(super) fn job_prefix(self) -> &'static str {
        self.spec().job_prefix
    }
}

/// Staged `generate_summaries` request. The frontend already expanded the
/// dialog's range into per-layer inputs; Rust forwards them verbatim.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct GenerateSummariesRequest {
    pub user_id: Option<i64>,
    pub run_per_thread: bool,
    pub run_daily: bool,
    pub run_weekly: bool,
    pub run_monthly: bool,
    /// Abort the staged pipeline on the first observed stage failure.
    /// Absent clients retain the legacy best-effort behaviour.
    #[serde(default)]
    pub stop_on_error: bool,
    /// Per-thread window (epoch ms). `None` = unbounded.
    pub last_message_after_ms: Option<i64>,
    pub last_message_before_ms: Option<i64>,
    /// Period range tokens; empty string = no range (batch falls back).
    #[serde(default)]
    pub daily_start: String,
    #[serde(default)]
    pub daily_end: String,
    #[serde(default)]
    pub weekly_start: String,
    #[serde(default)]
    pub weekly_end: String,
    #[serde(default)]
    pub monthly_start: String,
    #[serde(default)]
    pub monthly_end: String,
    /// Day-boundary tz for the period single workflows (not used for the
    /// range expansion, which is UTC-only).
    pub timezone_offset_hours: i32,
    /// Frontend-supplied dispatch id used as the cancel key. Older
    /// callers omit it; the analysis dispatch falls back to a
    /// timestamp-shaped id when absent.
    #[serde(default)]
    pub dispatch_id: Option<String>,
}

/// Which periods a batch run targets. `Auto` lets the batch pick its default
/// fallback ("last completed period only"); `LastN` back-fills N periods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeriodRange {
    Auto,
    LastN(i32),
}

impl PeriodRange {
    /// Value for the batch's `last_n_*` field (0 = Auto fallback).
    fn last_n(self) -> i32 {
        match self {
            PeriodRange::Auto => 0,
            PeriodRange::LastN(n) => n,
        }
    }
}

/// Resolved workflow input builders for the 3 batch workers. Pure JSON
/// construction so the tests can pin the wire-shape without spawning
/// a sidecar. `pub(super)` so the standalone `analysis_dispatch` commands
/// can reuse `summarize_input` / `personality_input` as the single source
/// of truth for the workflow-input shape (which must match the YAML schema).
pub(super) struct BatchDispatch {
    user_id: i64,
    /// memories gRPC callback coordinates the fanned-out workflows dial back
    /// at. Carries host/port/tls so a remote (incl. HTTPS) target propagates
    /// into the workflow input, not just into the app's own gRPC clients.
    callback: MemoriesCallback,
    workflows_dir: PathBuf,
    /// Optional conversation-time lower bound for manual batch dispatch.
    /// Import dispatch instead fills the thread-created window below.
    last_message_after_ms: Option<i64>,
    /// Optional inclusive upper bound forwarded into the per-thread
    /// summary batch's `last_message_before_ms`. Set only by the range-mode
    /// generate dialog; `None` keeps the legacy "no upper bound" behaviour.
    last_message_before_ms: Option<i64>,
    thread_created_after_ms: Option<i64>,
    thread_created_before_ms: Option<i64>,
    target_thread_ids: Option<Vec<String>>,
    /// Named worker that handles LLM completion. Defaults to `memories-llm`
    /// (local) or `memories-llm-external` (genai) based on the active LLM
    /// settings. Propagated to every workflow input as `llm_worker_name`.
    llm_worker_name: String,
    /// Output language ("ja" | "en") forwarded as `output_language` so each
    /// batch resolves the matching language-specific single worker
    /// (`memories-<feature>-single-<lang>`).
    output_language: String,
}

impl BatchDispatch {
    pub(super) fn resolve(
        callback: &MemoriesCallback,
        user_id: i64,
        last_message_after_ms: Option<i64>,
        llm_worker_name: String,
        output_language: String,
    ) -> AppResult<Self> {
        Self::resolve_with_window(
            callback,
            user_id,
            last_message_after_ms,
            None,
            llm_worker_name,
            output_language,
        )
    }

    pub(super) fn resolve_with_window(
        callback: &MemoriesCallback,
        user_id: i64,
        last_message_after_ms: Option<i64>,
        last_message_before_ms: Option<i64>,
        llm_worker_name: String,
        output_language: String,
    ) -> AppResult<Self> {
        let dir = crate::data::paths::workflows_bundle_dir()?;
        Ok(Self {
            user_id,
            callback: callback.clone(),
            workflows_dir: dir,
            last_message_after_ms,
            last_message_before_ms,
            thread_created_after_ms: None,
            thread_created_before_ms: None,
            target_thread_ids: None,
            llm_worker_name,
            output_language,
        })
    }

    /// Absolute path to a bundled workflow YAML (`<dir>/<sub>/<file>`), for
    /// both `*-single.yaml` and the `*-batch.yaml` the pipeline calls.
    fn workflow_path(&self, sub: &str, file: &str) -> String {
        self.workflows_dir
            .join(sub)
            .join(file)
            .to_string_lossy()
            .into_owned()
    }

    /// The gRPC callback fields every batch input shares.
    fn callback_fields(&self) -> serde_json::Value {
        serde_json::json!({
            "memories_grpc_host": self.callback.host,
            "memories_grpc_port": self.callback.port,
            "memories_grpc_tls": self.callback.tls,
        })
    }

    /// Inject `llm_worker_name` into a workflow input so the workflow
    /// routes LLM calls to the active worker (local or external).
    fn inject_llm_worker_name(&self, v: &mut serde_json::Value) {
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "llm_worker_name".to_string(),
                serde_json::Value::String(self.llm_worker_name.clone()),
            );
        }
    }

    /// Inject `output_language` so the batch picks the matching
    /// `memories-<feature>-single-<lang>` worker for each fan-out.
    fn inject_output_language(&self, v: &mut serde_json::Value) {
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "output_language".to_string(),
                serde_json::Value::String(self.output_language.clone()),
            );
        }
    }

    pub(super) fn summarize_input(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "user_id": self.user_id,
            "memory_thread_label_prefix": "summary",
            "force_resummarize": false,
            "min_message_count": 4,
            "max_context_chars": 200_000,
        });
        self.merge_callback(&mut v);
        self.inject_last_message_after_ms(&mut v);
        self.inject_last_message_before_ms(&mut v);
        self.inject_thread_created_window(&mut v);
        self.inject_target_thread_ids(&mut v);
        self.inject_llm_worker_name(&mut v);
        self.inject_output_language(&mut v);
        v
    }

    /// Import pipeline default: do not force re-extraction. Re-runs skip
    /// up-to-date threads via thread-personality-single's `existing_signal`
    /// check, and the batch may short-circuit at `target_signal_count`.
    pub(super) fn personality_input(&self) -> serde_json::Value {
        self.personality_input_with(false)
    }

    /// `force_reextract = true` makes the per-thread pass re-run even on
    /// threads that already have a stored signal, and disables the batch's
    /// `target_signal_count` early-break. Used by the standalone
    /// "Force 再抽出" path on the Personality tab so a prompt change can be
    /// applied to the historical thread set.
    pub(super) fn personality_input_with(&self, force_reextract: bool) -> serde_json::Value {
        let mut v = serde_json::json!({
            "user_id": self.user_id,
            // `merge_enabled` makes the batch run the layer-2 merge itself
            // after the signal pass (calling `memories-user-personality-merge-
            // <lang>` by name), so both this and the standalone button (which
            // share this input) produce a profile. The merge YAML path is no
            // longer relayed — the batch resolves the lang-worker by name.
            "merge_enabled": true,
            "force_reextract": force_reextract,
            "min_message_count": 4,
            "min_user_messages": 2,
            "max_context_chars": PERSONALITY_MAX_CONTEXT_CHARS,
        });
        self.merge_callback(&mut v);
        self.inject_last_message_after_ms(&mut v);
        self.inject_thread_created_window(&mut v);
        self.inject_target_thread_ids(&mut v);
        self.inject_llm_worker_name(&mut v);
        self.inject_output_language(&mut v);
        v
    }

    /// Build the input for a standalone Layer-2 merge dispatch (the
    /// `memories-user-personality-merge-<lang>` worker). Mirrors what
    /// `thread-personality-batch.yaml::userPersonalityMerge` passes to the
    /// merge YAML when it runs as the batch's tail, MINUS anything specific
    /// to the per-thread fan-out (no `single_workflow_path`,
    /// `merge_workflow_path`, `target_signal_count`, etc — the merge YAML
    /// only needs the inputs declared in its schema).
    ///
    /// Use case: a previous batch produced valid layer-1 signals but never
    /// emitted a profile (e.g. external-LLM 429 storm during per-thread
    /// extraction left enough signals AND a populated personality store but
    /// the merge never landed). This input lets the Personality tab run
    /// the merge in isolation.
    pub(super) fn merge_only_input(&self, force_remerge: bool) -> serde_json::Value {
        let mut v = serde_json::json!({
            "user_id": self.user_id,
            "max_context_chars": PERSONALITY_MAX_CONTEXT_CHARS,
            "force_remerge": force_remerge,
            // Hard-coded because the merge-only path has no batch parent.
            "max_signals": PERSONALITY_MERGE_MAX_SIGNALS,
        });
        self.merge_callback(&mut v);
        self.inject_llm_worker_name(&mut v);
        // The merge-only dispatch targets `memories-user-personality-merge-
        // <lang>` directly, so it needs the language too.
        self.inject_output_language(&mut v);
        v
    }

    fn reflection_input(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "user_id": self.user_id,
            "prompt_version": REFLECTION_PROMPT_VERSION,
        });
        self.merge_callback(&mut v);
        self.inject_last_message_after_ms(&mut v);
        self.inject_thread_created_window(&mut v);
        self.inject_target_thread_ids(&mut v);
        self.inject_llm_worker_name(&mut v);
        self.inject_output_language(&mut v);
        v
    }

    /// Build the input for a period (daily/weekly/monthly) work-summary batch.
    /// Period summaries use the same real owner as their source lineage and
    /// scope their rows by period.
    pub(super) fn period_input(&self, kind: PeriodKind, range: PeriodRange) -> serde_json::Value {
        let spec = kind.spec();
        let mut v = serde_json::json!({
            "user_id": self.user_id,
            "timezone_offset_hours": 9,
            "min_thread_count": 1,
            "max_context_chars": 200_000,
            "force_resummarize": false,
        });
        if let Some(obj) = v.as_object_mut() {
            obj.insert(spec.source_label_key.into(), spec.source_label.into());
            obj.insert(spec.out_label_key.into(), spec.out_label.into());
            obj.insert(spec.last_n_field.into(), range.last_n().into());
        }
        self.merge_callback(&mut v);
        self.inject_llm_worker_name(&mut v);
        self.inject_output_language(&mut v);
        v
    }

    /// Build the staged summaries-pipeline input. Forwards the request's period
    /// tokens and per-thread epoch bounds verbatim (no conversion lives here).
    pub(super) fn pipeline_input(&self, req: &GenerateSummariesRequest) -> serde_json::Value {
        let mut v = serde_json::json!({
            "run_per_thread": req.run_per_thread,
            "run_daily": req.run_daily,
            "run_weekly": req.run_weekly,
            "run_monthly": req.run_monthly,
            "stop_on_error": req.stop_on_error,
            "daily_start": req.daily_start,
            "daily_end": req.daily_end,
            "weekly_start": req.weekly_start,
            "weekly_end": req.weekly_end,
            "monthly_start": req.monthly_start,
            "monthly_end": req.monthly_end,
            "timezone_offset_hours": req.timezone_offset_hours,
            "user_id": self.user_id,
            "per_thread_batch_yaml": self.workflow_path("thread-summary", "thread-summary-batch.yaml"),
            "daily_batch_yaml": self.workflow_path("daily-work-summary", "daily-work-summary-batch.yaml"),
            "weekly_batch_yaml": self.workflow_path("weekly-work-summary", "weekly-work-summary-batch.yaml"),
            "monthly_batch_yaml": self.workflow_path("monthly-work-summary", "monthly-work-summary-batch.yaml"),
        });
        // Omit absent epoch bounds so the pipeline's `== null` guards keep
        // the per-thread batch unbounded.
        if let Some(ms) = req.last_message_after_ms {
            v["last_message_after_ms"] = serde_json::Value::from(ms);
        }
        if let Some(ms) = req.last_message_before_ms {
            v["last_message_before_ms"] = serde_json::Value::from(ms);
        }
        self.merge_callback(&mut v);
        self.inject_llm_worker_name(&mut v);
        self.inject_output_language(&mut v);
        v
    }

    fn merge_callback(&self, v: &mut serde_json::Value) {
        if let (Some(obj), Some(cb)) = (v.as_object_mut(), self.callback_fields().as_object()) {
            for (k, val) in cb {
                obj.insert(k.clone(), val.clone());
            }
        }
    }

    fn inject_last_message_after_ms(&self, v: &mut serde_json::Value) {
        if let Some(ms) = self.last_message_after_ms {
            v["last_message_after_ms"] = serde_json::Value::from(ms);
        }
    }

    fn inject_last_message_before_ms(&self, v: &mut serde_json::Value) {
        if let Some(ms) = self.last_message_before_ms {
            v["last_message_before_ms"] = serde_json::Value::from(ms);
        }
    }

    fn inject_thread_created_window(&self, v: &mut serde_json::Value) {
        if let Some(ms) = self.thread_created_after_ms {
            v["thread_created_after_ms"] = serde_json::Value::from(ms);
        }
        if let Some(ms) = self.thread_created_before_ms {
            v["thread_created_before_ms"] = serde_json::Value::from(ms);
        }
    }

    fn inject_target_thread_ids(&self, value: &mut serde_json::Value) {
        if let Some(ids) = &self.target_thread_ids {
            value["target_thread_ids"] = serde_json::json!(ids);
        }
    }
}

/// Condense a WORKFLOW chunk for the toast, carrying the last-seen progress
/// counter forward. Returns `(digest, progress_to_carry)`. Each chunk is a
/// `WorkflowResult` JSON (`{id, output, position, status, errorMessage}`) whose
/// `output` is the *entire* intermediate workflow context — for the summary
/// batch that's the full thread list (tens of KB), which floods the toast.
///
/// The `(processed/total)` counter is published by the batch's `reportProgress`
/// step (see each `*-batch.yaml`), but only the WorkflowResult for *that* set
/// step carries it in `output`; the long-running `invokeSingle` chunks that
/// follow (LLM generation) do not. Showing the counter only on the brief
/// reportProgress chunk would make it flicker and vanish. So we thread the last
/// counter through every chunk: `carried` is the value from the previous chunk,
/// and we return the updated value for the next call. This keeps `(N/M)` pinned
/// while a single thread is being processed.
///
/// The digest leads with `(N/M)` when known (most useful), then the current
/// step mapped from the JSON-pointer `position` via [`describe_position`]. The
/// full JSON goes to the log only. Non-JSON chunks fall back to a length-capped
/// raw string so nothing is silently dropped. Pure so it's unit-tested.
pub(super) fn summarize_workflow_chunk(
    raw: &str,
    carried: Option<(i64, i64)>,
) -> (String, Option<(i64, i64)>) {
    const MAX_RAW: usize = 200;
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return (cap(raw, MAX_RAW), carried);
    };
    let Some(status) = v.get("status").and_then(|s| s.as_str()) else {
        return (cap(raw, MAX_RAW), carried);
    };
    let position = v.get("position").and_then(|p| p.as_str()).unwrap_or("");
    let step = describe_position(position);
    // `output` is a context snapshot (JSON *string*). The counter only appears
    // on the reportProgress chunk; fall back to the carried value otherwise.
    let progress = v
        .get("output")
        .and_then(|o| o.as_str())
        .and_then(extract_progress)
        .or(carried);

    let digest = match (progress, step) {
        (Some((p, t)), Some(desc)) => format!("({p}/{t}) {desc}"),
        (Some((p, t)), None) => format!("({p}/{t})"),
        (None, Some(desc)) => format!("{status} · {desc}"),
        (None, None) => status.to_string(),
    };
    (digest, progress)
}

/// Pull `(processed, total)` from a workflow context snapshot. The batch's
/// `reportProgress` step sets these via jq, which may serialize them as numbers
/// or numeric strings depending on the runtime, so accept both. Returns `None`
/// unless both are present and total is non-zero (a `(n/0)` would be nonsense).
fn extract_progress(output_json: &str) -> Option<(i64, i64)> {
    let v: serde_json::Value = serde_json::from_str(output_json).ok()?;
    let processed = json_as_i64(v.get("progress_processed")?)?;
    let total = json_as_i64(v.get("progress_total")?)?;
    (total > 0).then_some((processed, total))
}

/// Decision for the terminal Done event of a batch workflow. Carries the
/// step status the toast should render plus an optional summary message
/// (e.g. "成功 12 / 失敗 3 (Gemini 429 ...)") that goes into the detail
/// dialog when the step degraded.
///
/// Pure so unit tests can pin each branch (all-failed / partial / no-counts
/// / all-success) without spinning up a workflow.
/// Read a batch workflow's terminal `WorkflowResult` JSON and classify it
/// into Done / Warning / Failed based on the `failed_count` / `processed_*`
/// counters the batch YAMLs export.
///
/// Why this lives next to `summarize_workflow_chunk`: the toast currently
/// treats every Done event as "緑 / 完了", which lies when the LLM provider
/// rate-limited part of the batch (every item failed but the workflow itself
/// reported `Completed`). The counters are already in the batch's
/// `output:` block; we just need to read them.
///
/// Decision table:
/// - `failed_count > 0` AND `failed_count >= processed`  → Failed (all items died)
/// - `failed_count > 0` AND processed > 0                → Warning (partial)
/// - `failed_count == 0` OR counters absent              → Done (unchanged)
///
/// Counters absent means an older batch YAML that hasn't been updated yet —
/// fall back to Done so a YAML drift doesn't downgrade a successful run to
/// Warning. `last_error` (when present) gets appended to the message so the
/// detail dialog surfaces the underlying failure cause.
pub(super) fn summarize_workflow_outcome(raw: Option<&str>) -> StepOutcome {
    let Some(raw) = raw else {
        return StepOutcome {
            status: StepStatus::Done,
            message: None,
        };
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return StepOutcome {
            status: StepStatus::Done,
            message: Some(raw.to_string()),
        };
    };
    // The terminal chunk's `output` is the workflow's exported `output:`
    // block serialized as a JSON string. A workflow without an `output:`
    // block (or one whose serializer drops the field on Completed) leaves
    // this absent — handled by the "counters absent → Done" fallback.
    let output_obj = v
        .get("output")
        .and_then(|o| o.as_str())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let Some(output) = output_obj else {
        return StepOutcome {
            status: StepStatus::Done,
            message: None,
        };
    };

    let failed = output
        .get("failed_count")
        .and_then(json_as_i64)
        .unwrap_or(0);
    let succeeded = output.get("succeeded_count").and_then(json_as_i64);
    // Period batches (daily/weekly/monthly) name their total `processed_days`
    // / `processed_weeks` / `processed_months` instead of `processed_threads`.
    // Accept all so a single classifier handles every batch shape.
    let processed = output
        .get("processed_threads")
        .or_else(|| output.get("processed_dates"))
        .or_else(|| output.get("processed_weeks"))
        .or_else(|| output.get("processed_months"))
        .and_then(json_as_i64);
    // Personality batch only: the count of already-processed threads we
    // deliberately did NOT re-extract because the signal budget was full.
    // Surfacing this in the digest is what replaces the misleading
    // mid-flight "既処理のため再抽出を省略中" label (which fired on every
    // iteration regardless of the `if:` guard outcome).
    let skipped = output
        .get("budget_skipped_count")
        .and_then(json_as_i64)
        .filter(|n| *n > 0);

    // No per-item failure: status stays Done. We still attach a message when
    // the budget skipped something so the user sees "完了 · 中略 N 件" in
    // the toast instead of bare 完了 (the YAML guard intentionally left
    // some threads alone, and that fact is worth surfacing).
    if failed <= 0 {
        return StepOutcome {
            status: StepStatus::Done,
            message: skipped.map(|n| format!("中略 {n} 件 (既存シグナル充足)")),
        };
    }
    let last_error = output
        .get("last_error")
        .map(|e| match e {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .map(|s| cap(&s, 400));
    let counter_line = match (succeeded, processed) {
        (Some(s), Some(p)) => format!("成功 {s} / 失敗 {failed} (合計 {p})"),
        (Some(s), None) => format!("成功 {s} / 失敗 {failed}"),
        (None, Some(p)) => format!("失敗 {failed} / 合計 {p}"),
        (None, None) => format!("失敗 {failed}"),
    };
    let mut lines: Vec<String> = vec![counter_line];
    if let Some(n) = skipped {
        lines.push(format!("中略 {n} 件 (既存シグナル充足)"));
    }
    if let Some(err) = last_error {
        lines.push(format!("直近のエラー: {err}"));
    }
    let message = Some(lines.join("\n"));
    let status = match processed {
        // "every item failed" — surface as a hard step failure so the toast
        // matches a workflow-level failure visually.
        Some(p) if p > 0 && failed >= p => StepStatus::Failed,
        _ => StepStatus::Warning,
    };
    StepOutcome { status, message }
}

/// Turn a workflow `position` JSON pointer into a short Japanese label for the
/// toast. The raw pointer (e.g.
/// `/ROOT/do/6/summarizeEach/for/do/0/.../run/do/0/logError`) overflows the
/// toast, so we keep only meaningful *step names* (dropping the structural
/// `ROOT`/`do`/`for`/`try`/`catch` tokens and numeric indices) and translate
/// the deepest known one — that's the most specific "where are we now" signal.
///
/// The workflows are bundled and version-controlled with this app, so the step
/// names are a closed, known set ([`step_description`]). Unknown names pass
/// through verbatim (capped) rather than being hidden, so a workflow edit that
/// adds a step still shows *something* until the table is updated.
fn describe_position(position: &str) -> Option<String> {
    let steps: Vec<&str> = position
        .split('/')
        .filter(|seg| !seg.is_empty())
        .filter(|seg| !is_structural_segment(seg))
        .collect();
    let deepest = steps.last()?;
    Some(match step_description(deepest) {
        Some(desc) => desc.to_string(),
        None => cap(deepest, 40),
    })
}

/// Structural tokens in a workflow position pointer that carry no semantic
/// meaning for the user (control-flow scaffolding + array indices).
fn is_structural_segment(seg: &str) -> bool {
    // `run` wraps a function/sub-workflow call; the meaningful step name is the
    // segment *before* it (e.g. `generateSummary/run`), so treat it as
    // scaffolding too.
    matches!(
        seg,
        "ROOT" | "do" | "for" | "try" | "catch" | "then" | "else" | "run"
    ) || seg.chars().all(|c| c.is_ascii_digit())
}

/// Map a known workflow step name to a Japanese label. Covers the batch
/// fan-out steps and the per-thread single-workflow steps across summary /
/// personality / reflection. Returns `None` for names not in the table.
fn step_description(step: &str) -> Option<&'static str> {
    let desc = match step {
        // ---- shared batch steps (summary / personality / reflection) ----
        "fetchByLabels" | "fetchByUser" => "対象スレッドを取得中",
        "filterThreads" => "対象スレッドを絞り込み中",
        "sortTargetThreadsNewestFirst" => "対象スレッドを並べ替え中",
        "resolveLastMessageAfter" | "resolveLastMessageBefore" => "対象期間を解決中",
        "summarizeEach" => "スレッドを要約中",
        "personalityEach" => "パーソナリティを抽出中",
        // ---- personality batch budget bookkeeping (early-break path) ----
        // `countExistingValidSignals` was renamed to
        // `fetchProcessedSignalMemories` to reflect that it returns both the
        // count AND the source-thread set used by `classifyThread`.
        //
        // The toast text deliberately avoids "予算" / "超過" wording — that
        // framing implied a cost/spend metaphor (none of this affects local
        // LLM cost) and left "what is exceeded" undefined. The actual
        // semantics: the loop processes new threads unconditionally, plus
        // already-processed threads only until cumulative valid-signal
        // count reaches `target_signal_count`; once that count is reached,
        // remaining already-processed threads are left alone (not "exceeded"
        // — just not re-extracted in this run, the existing extraction stays
        // valid).
        "fetchProcessedSignalMemories" => "既存シグナルを確認中",
        "initSignalBudget" => "処理上限を設定中",
        "classifyThread" => "対象スレッドを判定中",
        // `skipIfBudgetExhausted` is a `set:` step guarded by `if:`. The
        // jobworkerp WORKFLOW runner emits a position chunk for the step
        // regardless of whether the guard fires, so the toast briefly shows
        // this label even for NEW threads that will go on to actually run
        // the extraction. Keep the wording neutral ("再抽出要否を判定中")
        // so a transient chunk doesn't lie to the user; the actual skip
        // counter is surfaced post-batch via `budget_skipped_count` in the
        // workflow outcome digest (see `summarize_workflow_outcome`).
        "skipIfBudgetExhausted" => "再抽出要否を判定中",
        "recordSignalOutcome" => "抽出結果を記録中",
        // ---- personality batch / single shared guards ----
        "normalizePersonalityUserId" => "オーナーIDを解決中",
        "failOnSelfOwnedPersonality" | "failOnSummaryUserCollision" => "設定を検証中",
        "reflectEach" => "自省を生成中",
        "invokeSingleWithRetry" | "invokeSingle" => "個別スレッドを処理中",
        "reportProgress" => "進捗を更新中",
        "logError" | "logMergeError" => "エラーを記録中",
        // ---- summary single ----
        "fetchThread" => "スレッドを取得中",
        "fetchMessages" => "メッセージを取得中",
        "truncateMessages" => "メッセージを整形中",
        "generateSummary" => "要約を生成中 (LLM)",
        "updateThreadDescription" => "スレッド概要を更新中",
        "createMemoryThread" => "メモリスレッドを作成中",
        "updateSummary" | "addSummaryMemory" => "要約を保存中",
        "cleanupStaleSummaries" => "古い要約を整理中",
        // ---- personality single ----
        "generatePersonality" => "パーソナリティを生成中 (LLM)",
        "updateSignal" | "addSignalMemory" => "パーソナリティを保存中",
        "applySignalLabel" => "ラベルを付与中",
        // Personality single's new/renamed transition steps.
        "computeTruncationLevel" => "メッセージを整形中",
        "checkEligibility" => "再抽出要否を判定中",
        "skipIfNotEligible" => "更新不要のためスキップ中",
        "findExistingSignal" | "findMemoryThread" => "既存シグナルを確認中",
        // ---- personality merge (layer 2) ----
        "userPersonalityMerge" | "invokeMerge" => "プロファイルを統合中",
        "mergeProfile" => "プロファイルを統合中 (LLM)",
        "findProfileThread" => "シグナルを収集中",
        // findRecentSignals replaces the legacy
        // findSignalThreads + fetchSignalMemories N+1 fan-out.
        "findRecentSignals" => "シグナルを取得中",
        "confirmEntryDates" => "プロファイルを整形中",
        "updateProfile" | "addProfileMemory" => "プロファイルを保存中",
        "createProfileThread" => "プロファイルスレッドを作成中",
        "touchProfileThreadUpdatedAt" => "プロファイル更新時刻を反映中",
        "resolveProfileThreadId" => "プロファイルスレッドを解決中",
        "countTotalPersonalityMemories" | "computeNoSignalCount" => "no_signal 数を集計中",
        "buildSignalEntries" => "シグナルを整形中",
        "capSignals" | "truncateSignals" => "シグナルを切り詰め中",
        "buildSignalsLookup" => "シグナル索引を構築中",
        "skipIfNoSignals" => "シグナル不足のためスキップ中",
        "findExistingProfile" | "initExistingProfile" => "既存プロファイルを確認中",
        "initOutputVars"
        | "initCreatedMemoryThreadId"
        | "initCreatedProfileThreadId"
        | "initExistingSignal"
        | "initAccumulatedSignals" => "変数を初期化中",
        // ---- reflection single ----
        "fetchMemoriesAll" => "メモリを取得中",
        "prepareLlmInput" => "入力を準備中",
        "callReflectorLlm" | "callReflectorLlmWithFallback" => "自省を生成中 (LLM)",
        "validateLlmOutput" => "出力を検証中",
        "computeHeuristicScore" => "スコアを計算中",
        "buildParsedOutput" => "結果を構築中",
        "callFinalizeReflection" | "callFinalizeReflectionWithGuard" => "自省を確定中",
        _ => return None,
    };
    Some(desc)
}

/// Pull a short, human-readable reason out of a failed WORKFLOW chunk for the
/// toast / error dialog. Prefers `errorMessage`, then `status`, capped so a
/// payload-as-error (the server sometimes mirrors `output` into
/// `errorMessage`) can't blow up the UI. The untruncated text is logged
/// separately by the caller.
pub(super) fn summarize_workflow_error(raw: &str) -> String {
    const MAX_ERR: usize = 300;
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(msg) = v.get("errorMessage").and_then(|m| m.as_str()) {
            return cap(msg, MAX_ERR);
        }
        if let Some(status) = v.get("status").and_then(|s| s.as_str()) {
            return status.to_string();
        }
    }
    cap(raw, MAX_ERR)
}

/// Truncate `s` to at most `max` chars, appending an ellipsis + original
/// length when cut so the reader knows content was elided.
fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}… ({} chars total)", s.chars().count())
}

/// Cancellable variant of the original `run_stream_step`. Parks the
/// dispatched JobId into `current_job_id` so `start_import_cancel` can
/// issue `JobService/Delete` against it, and converts a Failed event
/// arriving *after* the cancel token fires into a "中断" message
/// (the underlying gRPC text is "stream error: …" once Delete closes
/// the stream from the server side).
// Eight arguments are inherent here: emit destinations (app/job_id/step),
// dispatch inputs (handle/worker/input), and the cancel handles (token +
// parked JobId). Bundling them into a struct buys nothing — every caller
// holds them as locals already and the struct would have the same fields.
#[allow(clippy::too_many_arguments)]
async fn run_cancellable_step(
    app: &AppHandle,
    job_id: &str,
    step: ImportStep,
    handle: &JobworkerpHandle,
    worker_name: &str,
    input: serde_json::Value,
    cancel: tokio_util::sync::CancellationToken,
    current_job_id: std::sync::Arc<
        tokio::sync::Mutex<Option<jobworkerp_client::jobworkerp::data::JobId>>,
    >,
) -> bool {
    // WORKFLOW runner exposes `create` + `run`; we always want `run`
    // (execute the pre-defined workflow). Omitting `using` makes the
    // server raise "Multiple methods available, 'using' required".
    let args = super::wrap_workflow_run_args(&input);
    // The `(N/M)` counter only rides the brief reportProgress chunk, so we
    // remember it across chunks and keep showing it during the long
    // invokeSingle generation that follows.
    let mut last_progress: Option<(i64, i64)> = None;
    let park_job_id = current_job_id.clone();
    let cancel_for_emit = cancel.clone();
    let completed = Arc::new(AtomicBool::new(false));
    let completed_for_event = completed.clone();
    run_cancellable_named_stream(
        handle,
        worker_name,
        args,
        Some("run"),
        None,
        cancel.clone(),
        // Async park: `blocking_lock` panics on a tokio runtime worker
        // ("Cannot block the current thread from within a runtime").
        // The callback is FnOnce — the trailer JobId is surfaced once
        // per dispatch.
        move |jid| async move {
            *park_job_id.lock().await = Some(jid);
        },
        |ev| {
            // The full chunk payload (a WorkflowResult whose `output` is the whole
            // intermediate context) goes to the log only; the toast gets a digest.
            let (status, message) = match ev {
                StreamEvent::Active(msg) => {
                    if let Some(raw) = msg {
                        tracing::debug!(target: "import", step = ?step, "{raw}");
                        emit_generated_refresh(
                            app,
                            job_id,
                            generated_refresh_scopes_from_position(raw),
                        );
                    }
                    let digest = msg.map(|raw| {
                        let (d, p) = summarize_workflow_chunk(raw, last_progress);
                        last_progress = p;
                        d
                    });
                    (StepStatus::Active, digest)
                }
                StreamEvent::Done(msg) => {
                    if let Some(raw) = msg {
                        tracing::info!(target: "import", step = ?step, "workflow done: {raw}");
                    }
                    emit_import_terminal_refresh(app, job_id, step);
                    // The batch ended without a workflow-level error, but a
                    // per-item failure (e.g. a Gemini 429 storm) can still leave
                    // `failed_count > 0` in the exported output. Branch on
                    // those counters so a partially-failed run renders Warning
                    // / Failed instead of unconditional 緑.
                    let outcome = summarize_workflow_outcome(msg);
                    completed_for_event
                        .store(outcome.status == StepStatus::Done, Ordering::Release);
                    let digest = match &outcome.message {
                        Some(m) => Some(m.clone()),
                        None => msg.map(|raw| summarize_workflow_chunk(raw, last_progress).0),
                    };
                    (outcome.status, digest)
                }
                StreamEvent::Failed(msg) => {
                    tracing::error!(target: "import", step = ?step, "workflow failed: {msg}");
                    emit_import_terminal_refresh(app, job_id, step);
                    // Distinguish user-triggered cancel from a real
                    // worker failure: once the token is cancelled, the
                    // server's stream close is the expected shape, not
                    // an error worth showing.
                    let text = if cancel_for_emit.is_cancelled() {
                        CANCELLED_ACTIVE_MESSAGE.to_string()
                    } else {
                        summarize_workflow_error(msg)
                    };
                    (StepStatus::Failed, Some(text))
                }
            };
            emit_step(app, job_id, step, status, message);
        },
    )
    .await;
    // Clear the parking slot so a (late) cancel after this step has
    // already finished doesn't try to Delete its now-stale JobId.
    *current_job_id.lock().await = None;
    completed.load(Ordering::Acquire)
}

fn emit_import_terminal_refresh(app: &AppHandle, job_id: &str, step: ImportStep) {
    if let Some(scope) = import_terminal_refresh_scope(step) {
        emit_generated_refresh(app, job_id, vec![scope]);
    }
}

fn import_terminal_refresh_scope(step: ImportStep) -> Option<super::GeneratedRefreshScope> {
    match step {
        ImportStep::ThreadSummary => Some(super::GeneratedRefreshScope::ThreadSummary),
        ImportStep::ThreadPersonality => Some(super::GeneratedRefreshScope::Personality),
        ImportStep::Reflection => Some(super::GeneratedRefreshScope::Reflection),
        ImportStep::ThreadImport => None,
    }
}

/// Emit a single downstream step as intentionally skipped. Used when the
/// import succeeded but the user unchecked this generation step in the
/// dialog — distinct from the dry-run / import-failure cases that
/// `mark_downstream` covers for *all three* steps at once.
fn emit_skipped(app: &AppHandle, job_id: &str, step: ImportStep) {
    emit_step(
        app,
        job_id,
        step,
        StepStatus::Waiting,
        Some("スキップ".into()),
    );
}

fn mark_downstream(app: &AppHandle, job_id: &str, status: StepStatus, msg: &str) {
    for step in [
        ImportStep::ThreadSummary,
        ImportStep::ThreadPersonality,
        ImportStep::Reflection,
    ] {
        emit_step(app, job_id, step, status, Some(msg.into()));
    }
}

/// Finish a manual import without touching sidecars when every selected
/// auto-discovered log root is absent. This is a successful no-op, while the
/// notice code lets the toast explain why no child process was started.
fn emit_no_importable_log_sources(app: &AppHandle, job_id: &str, downstream: DownstreamPlan) {
    emit_step_with_notice(
        app,
        job_id,
        ImportStep::ThreadImport,
        StepStatus::Done,
        None,
        Some(ImportNoticeCode::NoImportableLogSources),
    );
    emit_no_target_downstream(app, job_id, downstream);
}

/// Complete selected downstream steps when the import produced no target
/// threads; unselected steps retain the normal explicit skip state.
fn emit_no_target_downstream(app: &AppHandle, job_id: &str, downstream: DownstreamPlan) {
    for (step, status, message) in no_target_downstream_decisions(downstream) {
        emit_step(app, job_id, step, status, Some(message.into()));
    }
}

#[derive(Clone)]
struct ImportLogContext {
    aggregator: Arc<ImportAggregator>,
    pending_path: PathBuf,
    pending_imports: Arc<parking_lot::Mutex<PendingImportState>>,
    completed_sessions: Arc<parking_lot::Mutex<Vec<ImportSessionCompletedEvent>>>,
    pending_scope: PendingImportScope,
    expected_source: ImportSource,
    dry_run: bool,
    cancel: tokio_util::sync::CancellationToken,
    /// Native tracing scope shared by the sequential source children.
    native_log_dir: Option<PathBuf>,
}

async fn run_one_source(mut cmd: Command, context: ImportLogContext) -> Option<AggregatedState> {
    if context.cancel.is_cancelled() {
        return None;
    }
    let bin_for_msg = cmd.as_std().get_program().to_string_lossy().into_owned();
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let outcome = ChildOutcome::Failure(format!("spawn {bin_for_msg} failed: {e}"));
            return context.aggregator.record(outcome);
        }
    };
    if let Some(pid) = child.id()
        && let Some(path) = context.native_log_dir.as_deref()
        && let Err(error) = crate::log_rotation::mark_native_log_dir_pid(path, pid)
    {
        tracing::debug!(%error, pid, path = %path.display(), "could not record native import child PID");
    }
    forward_import_logs(child, context).await
}

const DRY_RUN_LIFECYCLE_EVENT_ERROR: &str =
    "memories-import emitted lifecycle event during dry-run";

/// Pipe CLI output to tracing, await child, and feed the per-child outcome
/// into the aggregator. Terminal `thread-import` step is emitted from the
/// caller after the source loop finishes (see `start_import`).
async fn forward_import_logs(
    mut child: tokio::process::Child,
    context: ImportLogContext,
) -> Option<AggregatedState> {
    let stdout_lines = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let event_error = Arc::new(parking_lot::Mutex::new(None::<String>));
    let created_sessions = Arc::new(parking_lot::Mutex::new(Vec::<PendingImportSession>::new()));
    let completed_in_child = Arc::new(parking_lot::Mutex::new(
        Vec::<ImportSessionCompletedEvent>::new(),
    ));
    let session_event_count = Arc::new(AtomicUsize::new(0));
    let event_ingestion = Arc::new(tokio::sync::Mutex::new(()));
    let pending_path = context.pending_path.clone();
    let pending_imports = context.pending_imports.clone();
    let completed_sessions = context.completed_sessions.clone();
    let pending_scope = context.pending_scope.clone();
    let expected_source = context.expected_source;
    let dry_run = context.dry_run;
    let cancel = context.cancel.clone();
    let mut stdout_reader = child.stdout.take().map(|stdout| {
        let lines = stdout_lines.clone();
        let event_error = event_error.clone();
        let created_sessions = created_sessions.clone();
        let completed_in_child = completed_in_child.clone();
        let session_event_count = session_event_count.clone();
        let pending_scope = pending_scope.clone();
        let event_ingestion = event_ingestion.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                match parse_import_stdout_line(&line) {
                    Ok(ImportStdoutLine::Creation(event)) => {
                        if dry_run {
                            *event_error.lock() = Some(DRY_RUN_LIFECYCLE_EVENT_ERROR.into());
                            continue;
                        }
                        if let Err(error) = validate_import_event_source(expected_source, event.source)
                        {
                            *event_error.lock() = Some(error.to_string());
                            continue;
                        }
                        let _ingestion = event_ingestion.lock().await;
                        if cancel.is_cancelled() {
                            continue;
                        }
                        let session = PendingImportSession {
                            memories_endpoint_id: pending_scope.memories_endpoint_id.clone(),
                            user_id: pending_scope.user_id,
                            source: event.source,
                            session_key: event.session_key,
                            thread_id: event.thread_id,
                        };
                        let result = record_pending_import(
                            &pending_path,
                            &mut pending_imports.lock(),
                            session.clone(),
                        );
                        if let Err(error) = result {
                            *event_error.lock() = Some(error.to_string());
                            continue;
                        }
                        created_sessions.lock().push(session);
                    }
                    Ok(ImportStdoutLine::SessionCompleted(completed)) => {
                        if dry_run {
                            *event_error.lock() = Some(DRY_RUN_LIFECYCLE_EVENT_ERROR.into());
                            continue;
                        }
                        if let Err(error) =
                            validate_import_event_source(expected_source, completed.source)
                        {
                            *event_error.lock() = Some(error.to_string());
                            continue;
                        }
                        let _ingestion = event_ingestion.lock().await;
                        if cancel.is_cancelled() {
                            continue;
                        }
                        session_event_count.fetch_add(1, Ordering::AcqRel);
                        match classify_session_completion(
                            &pending_imports.lock(),
                            &pending_scope,
                            &completed,
                        ) {
                            Ok(CompletionDisposition::TrackPending) => {
                                completed_in_child.lock().push(completed.clone());
                                completed_sessions.lock().push(completed);
                            }
                            Ok(CompletionDisposition::IgnoreExisting) => {
                                info!(
                                    target: "memories-import",
                                    source = completed.source.cli_subcommand(),
                                    session_key = completed.session_key,
                                    "existing non-pending session completed; excluding it from generation"
                                );
                            }
                            Err(error) => *event_error.lock() = Some(error.to_string()),
                        }
                    }
                    Ok(ImportStdoutLine::Text(line)) => {
                        info!(target: "memories-import", "{line}");
                        lines.lock().push(line);
                    }
                    Err(error) => {
                        *event_error.lock() = Some(error.to_string());
                    }
                }
            }
        })
    });
    // Keep the stderr tail so a non-zero exit can surface *why* it failed
    // (e.g. a TLS handshake error against a remote server). Without this the
    // UI only ever saw `exit status: 1` and the cause lived in stderr alone.
    let stderr_lines = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let mut stderr_reader = child.stderr.take().map(|stderr| {
        let lines = stderr_lines.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                warn!(target: "memories-import", "{line}");
                lines.lock().push(line);
            }
        })
    });

    let status = tokio::select! {
        status = child.wait() => Some(status),
        _ = cancel.cancelled() => {
            // Serializing cancellation with event ingestion prevents a buffered
            // lifecycle line from mutating pending state after this import ends.
            let _ingestion = event_ingestion.lock().await;
            abort_and_join_output_reader(&mut stdout_reader).await;
            abort_and_join_output_reader(&mut stderr_reader).await;
            if let Err(error) = child.start_kill() {
                warn!(?error, "failed to kill cancelled memories-import child");
            }
            if let Err(error) = child.wait().await {
                warn!(?error, "failed to wait for cancelled memories-import child");
            }
            None
        }
    };
    let status = status?;
    if let Some(handle) = stdout_reader {
        let _ = handle.await;
    }
    if let Some(handle) = stderr_reader {
        let _ = handle.await;
    }

    let event_failure = event_error.lock().clone();
    let missing_completion = created_sessions
        .lock()
        .iter()
        .find(|created| {
            !completed_in_child.lock().iter().any(|completed| {
                completed.source == created.source
                    && completed.session_key == created.session_key
                    && completed.thread_id == created.thread_id
            })
        })
        .cloned();
    let event_failure = event_failure.or_else(|| {
        missing_completion.map(|missing| {
            format!(
                "memories-import omitted session_completed for {}/{}",
                missing.source.cli_subcommand(),
                missing.session_key
            )
        })
    });
    let event_failure = event_failure.or_else(|| {
        let reported = reported_imported_count(&stdout_lines.lock()).unwrap_or_default();
        (!dry_run && reported > 0 && session_event_count.load(Ordering::Acquire) == 0)
            .then(|| "memories-import wrote memories without versioned session events".to_string())
    });
    let outcome = match (status, event_failure) {
        (_, Some(error)) => ChildOutcome::Failure(error),
        (Ok(s), None) if s.success() => {
            info!("memories-import succeeded");
            let summary = extract_summary_block(&stdout_lines.lock());
            ChildOutcome::Success { summary }
        }
        (Ok(s), None) => {
            warn!(?s, "memories-import exited non-zero");
            ChildOutcome::Failure(failure_message(&format!("exit {s}"), &stderr_lines.lock()))
        }
        (Err(e), None) => {
            warn!(error = ?e, "memories-import wait failed");
            ChildOutcome::Failure(format!("wait: {e}"))
        }
    };

    context.aggregator.record(outcome)
}

async fn abort_and_join_output_reader(reader: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(handle) = reader.take() {
        handle.abort();
        let _ = handle.await;
    }
}

/// Number of trailing stderr lines folded into a failure message. Enough to
/// carry a gRPC connect / TLS error (usually 1-2 lines) without dumping a full
/// backtrace into the toast.
const STDERR_TAIL_LINES: usize = 5;

/// Build the user-facing failure string: the exit reason plus the tail of the
/// child's stderr, when it produced any. Pure so the formatting is unit-tested
/// without spawning a process.
fn failure_message(reason: &str, stderr_lines: &[String]) -> String {
    let tail: Vec<&str> = stderr_lines
        .iter()
        .rev()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .take(STDERR_TAIL_LINES)
        .collect();
    if tail.is_empty() {
        return reason.to_string();
    }
    let tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
    format!("{reason}\n{tail}")
}

fn emit_thread_import_terminal(
    app: &AppHandle,
    job_id: &str,
    state: AggregatedState,
    dry_run: bool,
) {
    let summaries = match state {
        AggregatedState::AnyFailed(failures) => {
            emit_step(
                app,
                job_id,
                ImportStep::ThreadImport,
                StepStatus::Failed,
                Some(failures.join("; ")),
            );
            return;
        }
        AggregatedState::AllSucceeded { summaries } => summaries,
    };

    let import_message = (dry_run && !summaries.is_empty()).then(|| summaries.join("\n\n"));
    emit_step(
        app,
        job_id,
        ImportStep::ThreadImport,
        StepStatus::Done,
        import_message,
    );
}

/// Scrape the trailing `<label> summary:` block printed by
/// `memories-import` (see `agent-chat-import/src/main.rs::print_summary`).
fn extract_summary_block(lines: &[String]) -> Option<String> {
    let start = lines
        .iter()
        .rposition(|l| l.trim_end().ends_with(" summary:"))?;
    let mut out = Vec::with_capacity(8);
    out.push(lines[start].trim_end().to_string());
    for line in &lines[start + 1..] {
        if line.starts_with("  ") {
            out.push(line.trim_end().to_string());
        } else {
            break;
        }
    }
    Some(out.join("\n"))
}

fn emit_step(
    app: &AppHandle,
    job_id: &str,
    step: ImportStep,
    status: StepStatus,
    message: Option<String>,
) {
    emit_step_with_notice(app, job_id, step, status, message, None);
}

fn emit_step_with_notice(
    app: &AppHandle,
    job_id: &str,
    step: ImportStep,
    status: StepStatus,
    message: Option<String>,
    notice_code: Option<ImportNoticeCode>,
) {
    emit_event(
        app,
        "import://step",
        ImportStepUpdate {
            job_id: job_id.to_string(),
            step,
            status,
            message,
            notice_code,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_terminal_refresh_scope_covers_each_generation_step_only() {
        assert_eq!(
            import_terminal_refresh_scope(ImportStep::ThreadSummary),
            Some(super::super::GeneratedRefreshScope::ThreadSummary)
        );
        assert_eq!(
            import_terminal_refresh_scope(ImportStep::ThreadPersonality),
            Some(super::super::GeneratedRefreshScope::Personality)
        );
        assert_eq!(
            import_terminal_refresh_scope(ImportStep::Reflection),
            Some(super::super::GeneratedRefreshScope::Reflection)
        );
        assert_eq!(
            import_terminal_refresh_scope(ImportStep::ThreadImport),
            None
        );
    }
    use std::ffi::OsStr;

    fn local_scope(user_id: i64) -> PendingImportScope {
        PendingImportScope::new(ConnectionMode::Local, "http://127.0.0.1:9010", user_id).unwrap()
    }

    fn import_log_context(
        aggregator: Arc<ImportAggregator>,
        pending_path: PathBuf,
        pending_imports: Arc<parking_lot::Mutex<PendingImportState>>,
        completed_sessions: Arc<parking_lot::Mutex<Vec<ImportSessionCompletedEvent>>>,
        pending_scope: PendingImportScope,
    ) -> ImportLogContext {
        ImportLogContext {
            aggregator,
            pending_path,
            pending_imports,
            completed_sessions,
            pending_scope,
            expected_source: ImportSource::ClaudeCode,
            dry_run: false,
            cancel: tokio_util::sync::CancellationToken::new(),
            native_log_dir: None,
        }
    }

    fn pending_session(
        scope: &PendingImportScope,
        source: ImportSource,
        session_key: &str,
        thread_id: &str,
    ) -> PendingImportSession {
        PendingImportSession {
            memories_endpoint_id: scope.memories_endpoint_id.clone(),
            user_id: scope.user_id,
            source,
            session_key: session_key.into(),
            thread_id: thread_id.into(),
        }
    }

    fn dummy_targets() -> ResolvedTargets {
        ResolvedTargets {
            jobworkerp_url: "http://127.0.0.1:9000".into(),
            memories_url: "http://127.0.0.1:9010".into(),
        }
    }

    fn dummy_callback() -> MemoriesCallback {
        MemoriesCallback {
            host: "127.0.0.1".into(),
            port: 9010,
            tls: false,
        }
    }

    fn make_plan() -> ImportPlan {
        make_plan_with_plain(None)
    }

    fn make_plan_with_plain(plain: Option<PlainImportConfig>) -> ImportPlan {
        ImportPlan::new(
            PathBuf::from("/usr/local/bin/memories-import"),
            1,
            &dummy_targets(),
            Some("2026-05-01T00:00:00Z".into()),
            false,
            vec!["lookback".into()],
            PathBuf::from("/tmp/lookback-log/native/import-test"),
            plain,
        )
    }

    #[test]
    fn native_import_log_guard_marks_scope_closed_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let path = crate::log_rotation::native_log_dir(root.path(), "import-test").unwrap();
        {
            let _guard = NativeImportLogGuard(path.clone());
            assert!(path.join(".active").is_file());
        }
        assert!(!path.join(".active").exists());
        assert!(path.join(".closed").is_file());
    }

    fn plan_dry_run() -> ImportPlan {
        ImportPlan::new(
            PathBuf::from("/usr/local/bin/memories-import"),
            1,
            &dummy_targets(),
            Some("2026-05-01T00:00:00Z".into()),
            true,
            vec!["lookback".into()],
            PathBuf::from("/tmp/lookback-log/native/import-test"),
            None,
        )
    }

    #[test]
    fn manual_source_filter_skips_only_missing_log_roots() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude/projects")).unwrap();

        let selection = filter_manual_import_sources(
            &[
                ImportSource::ClaudeCode,
                ImportSource::Codex,
                ImportSource::Plain,
            ],
            Some(home.path()),
        );

        assert_eq!(
            selection.sources,
            vec![ImportSource::ClaudeCode, ImportSource::Plain]
        );
        assert!(!selection.no_importable_log_sources);
    }

    #[test]
    fn manual_source_filter_preserves_requested_order_when_all_roots_exist() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude/projects")).unwrap();
        std::fs::create_dir_all(home.path().join(".codex/sessions")).unwrap();

        let selection = filter_manual_import_sources(
            &[
                ImportSource::Codex,
                ImportSource::Plain,
                ImportSource::ClaudeCode,
            ],
            Some(home.path()),
        );

        assert_eq!(
            selection.sources,
            vec![
                ImportSource::Codex,
                ImportSource::Plain,
                ImportSource::ClaudeCode
            ]
        );
        assert!(!selection.no_importable_log_sources);
    }

    #[test]
    fn manual_source_filter_keeps_codex_when_it_is_the_only_existing_log_root() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".codex/sessions")).unwrap();

        let selection = filter_manual_import_sources(
            &[ImportSource::ClaudeCode, ImportSource::Codex],
            Some(home.path()),
        );

        assert_eq!(selection.sources, vec![ImportSource::Codex]);
        assert!(!selection.no_importable_log_sources);
    }

    #[test]
    fn manual_source_filter_marks_notice_when_all_log_roots_are_missing() {
        let home = tempfile::tempdir().unwrap();
        let selection = filter_manual_import_sources(
            &[ImportSource::ClaudeCode, ImportSource::Codex],
            Some(home.path()),
        );

        assert!(selection.sources.is_empty());
        assert!(selection.no_importable_log_sources);
    }

    #[test]
    fn manual_source_filter_keeps_plain_and_does_not_notice_when_only_plain_is_selected() {
        let home = tempfile::tempdir().unwrap();
        let selection = filter_manual_import_sources(&[ImportSource::Plain], Some(home.path()));

        assert_eq!(selection.sources, vec![ImportSource::Plain]);
        assert!(!selection.no_importable_log_sources);
    }

    #[test]
    fn manual_source_filter_handles_missing_home_without_affecting_plain() {
        let selection = filter_manual_import_sources(
            &[
                ImportSource::ClaudeCode,
                ImportSource::Codex,
                ImportSource::Plain,
            ],
            None,
        );

        assert_eq!(selection.sources, vec![ImportSource::Plain]);
        assert!(!selection.no_importable_log_sources);
    }

    #[test]
    fn import_step_notice_code_is_optional_for_backward_compatibility() {
        let without_notice = ImportStepUpdate {
            job_id: "job".into(),
            step: ImportStep::ThreadImport,
            status: StepStatus::Done,
            message: None,
            notice_code: None,
        };
        let value = serde_json::to_value(without_notice).unwrap();
        assert!(value.get("notice_code").is_none());

        let with_notice = ImportStepUpdate {
            job_id: "job".into(),
            step: ImportStep::ThreadImport,
            status: StepStatus::Done,
            message: None,
            notice_code: Some(ImportNoticeCode::NoImportableLogSources),
        };
        assert_eq!(
            serde_json::to_value(with_notice).unwrap()["notice_code"],
            "no-importable-log-sources"
        );
    }

    fn make_request(
        run_summary: bool,
        run_personality: bool,
        run_reflection: bool,
    ) -> StartImportRequest {
        StartImportRequest {
            sources: vec![ImportSource::ClaudeCode],
            since: None,
            user_id: Some(1),
            dry_run: false,
            labels: vec![],
            memories_import_bin: PathBuf::from("/usr/local/bin/memories-import"),
            run_summary,
            run_personality,
            run_reflection,
            dispatch_id: None,
            plain: None,
        }
    }

    #[test]
    fn downstream_plan_from_request_copies_flags() {
        let all = DownstreamPlan::from_request(&make_request(true, true, true));
        assert_eq!(
            all,
            DownstreamPlan {
                summary: true,
                personality: true,
                reflection: true
            }
        );

        let none = DownstreamPlan::from_request(&make_request(false, false, false));
        assert_eq!(
            none,
            DownstreamPlan {
                summary: false,
                personality: false,
                reflection: false
            }
        );

        // Mixed: only personality off, proving each flag maps independently
        // (not a single all-or-nothing toggle).
        let mixed = DownstreamPlan::from_request(&make_request(true, false, true));
        assert_eq!(
            mixed,
            DownstreamPlan {
                summary: true,
                personality: false,
                reflection: true
            }
        );
    }

    #[test]
    fn downstream_plan_all_true_equals_legacy() {
        // Invariant: all three flags on reproduces the pre-selection
        // behaviour where every downstream step always ran.
        let plan = DownstreamPlan::from_request(&make_request(true, true, true));
        assert!(plan.summary && plan.personality && plan.reflection);
    }

    #[test]
    fn downstream_steps_preserve_pipeline_order_and_selection() {
        let plan = DownstreamPlan::from_request(&make_request(true, false, true));
        assert_eq!(
            downstream_steps(plan),
            [
                (ImportStep::ThreadSummary, true),
                (ImportStep::ThreadPersonality, false),
                (ImportStep::Reflection, true),
            ]
        );
    }

    #[test]
    fn no_target_downstream_decisions_cover_all_selection_combinations() {
        let cases = [
            (false, false, false),
            (false, false, true),
            (false, true, false),
            (false, true, true),
            (true, false, false),
            (true, false, true),
            (true, true, false),
            (true, true, true),
        ];

        for (summary, personality, reflection) in cases {
            let plan = DownstreamPlan {
                summary,
                personality,
                reflection,
            };
            let expected_state = |step, selected| {
                if selected {
                    (step, StepStatus::Done, "対象スレッドなし")
                } else {
                    (step, StepStatus::Waiting, "スキップ")
                }
            };
            let expected = [
                expected_state(ImportStep::ThreadSummary, summary),
                expected_state(ImportStep::ThreadPersonality, personality),
                expected_state(ImportStep::Reflection, reflection),
            ];
            assert_eq!(no_target_downstream_decisions(plan), expected);
        }
    }

    #[test]
    fn build_command_passes_user_id_server_url_since_and_subcommand() {
        let plan = make_plan();
        let cmd = plan.build_command(ImportSource::ClaudeCode);
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(args.windows(2).any(|w| w[0] == "--user-id" && w[1] == "1"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--server-url" && w[1] == "http://127.0.0.1:9010")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--since" && w[1] == "2026-05-01T00:00:00Z")
        );
        assert!(args.iter().any(|a| *a == "claude-code"));
        assert!(args.iter().any(|a| *a == "--all-projects"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--labels" && w[1] == "lookback")
        );
    }

    #[test]
    fn build_command_enables_versioned_events_for_every_import_source() {
        let plain = make_plan_with_plain(Some(plain_cfg(Some("notes"), ThreadStrategy::PerFile)));
        for (plan, source) in [
            (make_plan(), ImportSource::ClaudeCode),
            (make_plan(), ImportSource::Codex),
            (plain, ImportSource::Plain),
        ] {
            let command = plan.build_command(source);
            let args = command.as_std().get_args().collect::<Vec<_>>();
            assert!(
                args.iter().any(|arg| *arg == "--events-jsonl"),
                "missing event flag for {source:?}"
            );
        }
    }

    #[test]
    fn build_command_does_not_leak_summarize_or_personality_flags() {
        // Dispatch flags live on `BatchDispatch::*_input` now; the CLI
        // must stay a pure importer so its exit doesn't block on the
        // downstream workflows.
        let plan = make_plan();
        let cmd = plan.build_command(ImportSource::ClaudeCode);
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(!args.iter().any(|a| *a == "--summarize-workflow"));
        assert!(!args.iter().any(|a| *a == "--summarize-after-json"));
        assert!(!args.iter().any(|a| *a == "--personality-workflow"));
        assert!(
            !args
                .iter()
                .any(|a| *a == "--extract-personality-after-json")
        );
    }

    #[test]
    fn build_command_marks_dry_run() {
        let plan = plan_dry_run();
        let cmd = plan.build_command(ImportSource::ClaudeCode);
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(args.iter().any(|a| *a == "--dry-run"));
    }

    fn plain_cfg(source_name: Option<&str>, strategy: ThreadStrategy) -> PlainImportConfig {
        PlainImportConfig {
            root: PathBuf::from("/tmp/notes"),
            source_name: source_name.map(str::to_string),
            thread_strategy: strategy,
        }
    }

    #[test]
    fn build_command_plain_emits_root_strategy_source_name() {
        let plan = make_plan_with_plain(Some(plain_cfg(Some("notes"), ThreadStrategy::PerDir)));
        let cmd = plan.build_command(ImportSource::Plain);
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(args.iter().any(|a| *a == "plain"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--root" && w[1] == "/tmp/notes")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--thread-strategy" && w[1] == "per-dir")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--source-name" && w[1] == "notes")
        );
        // Plain must not leak the other sources' discovery flags.
        assert!(!args.iter().any(|a| *a == "--all-projects"));
        assert!(!args.iter().any(|a| *a == "--all-sessions"));
    }

    #[test]
    fn build_command_plain_omits_source_name_when_absent() {
        let plan = make_plan_with_plain(Some(plain_cfg(None, ThreadStrategy::Single)));
        let cmd = plan.build_command(ImportSource::Plain);
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(!args.iter().any(|a| *a == "--source-name"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--thread-strategy" && w[1] == "single")
        );
    }

    #[test]
    fn build_command_plain_passes_global_user_id_server_url_since() {
        // The plain source shares the global header path (user-id / server-url
        // / since) with the other sources.
        let plan = make_plan_with_plain(Some(plain_cfg(Some("notes"), ThreadStrategy::PerFile)));
        let cmd = plan.build_command(ImportSource::Plain);
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(args.windows(2).any(|w| w[0] == "--user-id" && w[1] == "1"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--server-url" && w[1] == "http://127.0.0.1:9010")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--since" && w[1] == "2026-05-01T00:00:00Z")
        );
    }

    fn plain_request(plain: Option<PlainImportConfig>) -> StartImportRequest {
        StartImportRequest {
            sources: vec![ImportSource::Plain],
            since: None,
            user_id: Some(1),
            dry_run: false,
            labels: vec![],
            memories_import_bin: PathBuf::from("/usr/local/bin/memories-import"),
            run_summary: false,
            run_personality: false,
            run_reflection: false,
            dispatch_id: None,
            plain,
        }
    }

    #[test]
    fn validate_plain_accepts_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let req = plain_request(Some(PlainImportConfig {
            root: dir.path().to_path_buf(),
            source_name: Some("notes_01".into()),
            thread_strategy: ThreadStrategy::PerDir,
        }));
        assert!(validate_plain(&req).is_ok());
    }

    #[test]
    fn validate_plain_allows_no_plain_source() {
        // The claude/codex-only flow carries no plain block and must pass.
        assert!(validate_plain(&make_request(true, true, true)).is_ok());
    }

    #[test]
    fn validate_plain_rejects_config_without_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = make_request(false, false, false);
        req.plain = Some(PlainImportConfig {
            root: dir.path().to_path_buf(),
            source_name: None,
            thread_strategy: ThreadStrategy::PerFile,
        });
        assert!(validate_plain(&req).is_err());
    }

    #[test]
    fn validate_plain_rejects_source_without_config() {
        assert!(validate_plain(&plain_request(None)).is_err());
    }

    #[test]
    fn validate_plain_rejects_bad_source_name() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "Notes",
            "",
            "a".repeat(33).as_str(),
            "with space",
            "dot.name",
        ] {
            let req = plain_request(Some(PlainImportConfig {
                root: dir.path().to_path_buf(),
                source_name: Some(bad.to_string()),
                thread_strategy: ThreadStrategy::PerFile,
            }));
            assert!(
                validate_plain(&req).is_err(),
                "source-name {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_plain_rejects_missing_root() {
        let req = plain_request(Some(PlainImportConfig {
            root: PathBuf::from("/nonexistent/lookback/plain/root"),
            source_name: Some("notes".into()),
            thread_strategy: ThreadStrategy::PerFile,
        }));
        assert!(validate_plain(&req).is_err());
    }

    #[test]
    fn thread_strategy_decodes_known_values() {
        for (wire, expected) in [
            ("per-file", ThreadStrategy::PerFile),
            ("per-dir", ThreadStrategy::PerDir),
            ("single", ThreadStrategy::Single),
        ] {
            let s: ThreadStrategy =
                serde_json::from_value(serde_json::Value::String(wire.into())).unwrap();
            assert_eq!(s, expected);
            assert_eq!(s.cli_value(), wire);
        }
    }

    #[test]
    fn thread_strategy_rejects_unknown_value() {
        let r: Result<ThreadStrategy, _> =
            serde_json::from_value(serde_json::Value::String("per-week".into()));
        assert!(r.is_err());
    }

    #[test]
    fn plain_config_decodes_from_request_json() {
        let json = serde_json::json!({
            "sources": ["plain"],
            "since": null,
            "user_id": 1,
            "dry_run": false,
            "labels": [],
            "memories_import_bin": "/usr/local/bin/memories-import",
            "run_summary": false,
            "run_personality": false,
            "run_reflection": false,
            "plain": {
                "root": "/tmp/notes",
                "source_name": "notes",
                "thread_strategy": "per-dir"
            }
        });
        let req: StartImportRequest = serde_json::from_value(json).unwrap();
        let plain = req.plain.expect("plain block decoded");
        assert_eq!(plain.root, PathBuf::from("/tmp/notes"));
        assert_eq!(plain.source_name.as_deref(), Some("notes"));
        assert_eq!(plain.thread_strategy, ThreadStrategy::PerDir);
    }

    #[test]
    fn request_without_plain_defaults_to_none() {
        let json = serde_json::json!({
            "sources": ["claude-code"],
            "since": null,
            "user_id": 1,
            "dry_run": false,
            "labels": [],
            "memories_import_bin": "/usr/local/bin/memories-import",
            "run_summary": true,
            "run_personality": true,
            "run_reflection": true
        });
        let req: StartImportRequest = serde_json::from_value(json).unwrap();
        assert!(req.plain.is_none());
    }

    #[test]
    fn build_command_pins_native_log_file_dir_to_import_scope() {
        // Regression: a Finder-launched .app inherits cwd `/`, where the
        // child's command-utils tracing init panics creating its log file.
        // LOG_FILE_DIR must point at this import's isolated native log dir — and because
        // envy treats the config's `use_json`/`use_stdout` bools as required,
        // LOG_USE_JSON/LOG_USE_STDOUT must accompany it or the whole config
        // deserialize fails and the dir is silently dropped.
        let plan = make_plan();
        let cmd = plan.build_command(ImportSource::ClaudeCode);
        let env_of = |key: &str| -> Option<std::ffi::OsString> {
            cmd.as_std()
                .get_envs()
                .find(|(k, _)| *k == OsStr::new(key))
                .and_then(|(_, v)| v)
                .map(|v| v.to_os_string())
        };
        assert_eq!(
            env_of("LOG_FILE_DIR").as_deref(),
            Some(OsStr::new("/tmp/lookback-log/native/import-test"))
        );
        assert_eq!(env_of("LOG_USE_JSON").as_deref(), Some(OsStr::new("true")));
        assert_eq!(
            env_of("LOG_USE_STDOUT").as_deref(),
            Some(OsStr::new("true"))
        );
    }

    fn dispatch() -> BatchDispatch {
        dispatch_with(None)
    }

    fn dispatch_with(last_message_after_ms: Option<i64>) -> BatchDispatch {
        dispatch_with_window(last_message_after_ms, None)
    }

    fn dispatch_with_window(
        last_message_after_ms: Option<i64>,
        last_message_before_ms: Option<i64>,
    ) -> BatchDispatch {
        BatchDispatch {
            user_id: 1,
            callback: dummy_callback(),
            workflows_dir: PathBuf::from("/x/workflows"),
            last_message_after_ms,
            last_message_before_ms,
            thread_created_after_ms: None,
            thread_created_before_ms: None,
            target_thread_ids: None,
            llm_worker_name: "memories-llm".to_string(),
            output_language: "ja".to_string(),
        }
    }

    #[test]
    fn batch_dispatch_summarize_input_matches_workflow_schema() {
        let v = dispatch().summarize_input();
        assert_eq!(v["user_id"], 1);
        assert_eq!(v["memories_grpc_host"], "127.0.0.1");
        assert_eq!(v["memories_grpc_port"], 9010);
        assert_eq!(v["memories_grpc_tls"], false);
        // The batch resolves its language-specific single worker by name from
        // `output_language`; the old single-yaml path relay is gone.
        assert!(v.get("single_workflow_path").is_none());
        assert_eq!(v["output_language"], "ja");
        assert!(v.get("summary_user_id").is_none());
        assert_eq!(v["max_context_chars"], 200_000);
    }

    #[test]
    fn import_dispatch_uses_only_the_authoritative_thread_ids() {
        let mut dispatch = dispatch();
        dispatch.target_thread_ids = Some(vec!["9007199254740993".into(), "42".into()]);

        for input in [
            dispatch.summarize_input(),
            dispatch.personality_input(),
            dispatch.reflection_input(),
        ] {
            assert_eq!(
                input["target_thread_ids"],
                serde_json::json!(["9007199254740993", "42"])
            );
            assert!(input.get("thread_created_after_ms").is_none());
            assert!(input.get("thread_created_before_ms").is_none());
            assert!(input.get("last_message_after_ms").is_none());
            assert!(input.get("last_message_before_ms").is_none());
        }
    }

    #[test]
    fn importer_jsonl_parser_separates_events_from_text_and_preserves_large_ids() {
        let line = r#"{"schema":"memories-import-event","version":1,"event":"thread_created","source":"codex","session_key":"session/a","thread_id":"9007199254740993"}"#;
        assert_eq!(
            parse_import_stdout_line(line).unwrap(),
            ImportStdoutLine::Creation(ImportCreationEventWire {
                version: 1,
                event: "thread_created".into(),
                source: ImportSource::Codex,
                session_key: "session/a".into(),
                thread_id: "9007199254740993".into(),
            })
        );
        assert_eq!(
            parse_import_stdout_line("codex summary:").unwrap(),
            ImportStdoutLine::Text("codex summary:".into())
        );
    }

    #[test]
    fn importer_summary_exposes_missing_event_protocol_without_parsing_event_lines_as_text() {
        assert_eq!(
            reported_imported_count(&["codex summary:".into(), "  Memories imported: 3".into(),]),
            Some(3)
        );
        assert_eq!(reported_imported_count(&["unrelated".into()]), None);
    }

    #[test]
    fn importer_session_completion_is_versioned_and_typed() {
        let line = r#"{"schema":"memories-import-event","version":1,"event":"session_completed","source":"claude-code","session_key":"project/session","thread_id":"42","imported_count":3,"success":true}"#;
        assert_eq!(
            parse_import_stdout_line(line).unwrap(),
            ImportStdoutLine::SessionCompleted(ImportSessionCompletedEvent {
                source: ImportSource::ClaudeCode,
                session_key: "project/session".into(),
                thread_id: "42".into(),
                imported_count: 3,
                success: true,
            })
        );
        assert!(parse_import_stdout_line(&line.replace("\"42\"", "42")).is_err());
        assert!(
            parse_import_stdout_line(
                r#"{"schema":"memories-import-event","version":1,"event":"thread_created""#
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_importer_events_drive_pending_target_and_clear_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let pending = Arc::new(parking_lot::Mutex::new(PendingImportState::default()));
        let completions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let scope = local_scope(1);
        let aggregator = Arc::new(ImportAggregator::new(1));
        let script = concat!(
            "printf '%s\\n' '",
            "{\"schema\":\"memories-import-event\",\"version\":1,\"event\":\"thread_created\",\"source\":\"claude-code\",\"session_key\":\"claude_code:session\",\"thread_id\":\"9007199254740993\"}",
            "' '",
            "{\"schema\":\"memories-import-event\",\"version\":1,\"event\":\"session_completed\",\"source\":\"claude-code\",\"session_key\":\"claude_code:session\",\"thread_id\":\"9007199254740993\",\"imported_count\":3,\"success\":true}",
            "' 'claude-code summary:' '  Memories imported: 3'"
        );
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().unwrap();

        let state = forward_import_logs(
            child,
            import_log_context(
                aggregator,
                pending_path.clone(),
                pending.clone(),
                completions.clone(),
                scope.clone(),
            ),
        )
        .await;
        assert!(matches!(state, Some(AggregatedState::AllSucceeded { .. })));
        let targeted = select_targeted_sessions(
            &pending.lock(),
            &scope,
            &completions.lock(),
            &[ImportSource::ClaudeCode],
        );
        assert_eq!(targeted.len(), 1);
        assert_eq!(targeted[0].thread_id, "9007199254740993");
        assert_eq!(
            load_pending_imports(&pending_path).unwrap().sessions,
            targeted
        );

        let cleared = completed_pending_state(&pending.lock(), &targeted, true);
        save_pending_imports(&pending_path, &cleared).unwrap();
        assert!(
            load_pending_imports(&pending_path)
                .unwrap()
                .sessions
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dry_run_summary_without_lifecycle_events_succeeds_without_state_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let pending = Arc::new(parking_lot::Mutex::new(PendingImportState::default()));
        let completions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let aggregator = Arc::new(ImportAggregator::new(1));
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("printf '%s\\n' '[dry-run] claude-code summary:' '  Memories imported: 3'")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let state = forward_import_logs(
            command.spawn().unwrap(),
            ImportLogContext {
                dry_run: true,
                ..import_log_context(
                    aggregator,
                    pending_path.clone(),
                    pending.clone(),
                    completions.clone(),
                    local_scope(1),
                )
            },
        )
        .await;

        assert!(matches!(state, Some(AggregatedState::AllSucceeded { .. })));
        assert!(pending.lock().sessions.is_empty());
        assert!(completions.lock().is_empty());
        assert!(!pending_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_import_summary_without_lifecycle_events_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let pending = Arc::new(parking_lot::Mutex::new(PendingImportState::default()));
        let completions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let aggregator = Arc::new(ImportAggregator::new(1));
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("printf '%s\\n' 'claude-code summary:' '  Memories imported: 3'")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let state = forward_import_logs(
            command.spawn().unwrap(),
            import_log_context(
                aggregator,
                pending_path,
                pending,
                completions,
                local_scope(1),
            ),
        )
        .await;

        assert!(matches!(
            state,
            Some(AggregatedState::AnyFailed(errors))
                if errors == ["memories-import wrote memories without versioned session events"]
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dry_run_lifecycle_event_fails_before_pending_state_is_mutated() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let pending = Arc::new(parking_lot::Mutex::new(PendingImportState::default()));
        let completions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let aggregator = Arc::new(ImportAggregator::new(1));
        let event = concat!(
            "{\"schema\":\"memories-import-event\",\"version\":1,",
            "\"event\":\"thread_created\",\"source\":\"claude-code\",",
            "\"session_key\":\"claude_code:session\",",
            "\"thread_id\":\"9007199254740993\"}"
        );
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!("printf '%s\\n' '{event}'"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let state = forward_import_logs(
            command.spawn().unwrap(),
            ImportLogContext {
                dry_run: true,
                ..import_log_context(
                    aggregator,
                    pending_path.clone(),
                    pending.clone(),
                    completions.clone(),
                    local_scope(1),
                )
            },
        )
        .await;

        assert!(matches!(
            state,
            Some(AggregatedState::AnyFailed(errors))
                if errors == ["memories-import emitted lifecycle event during dry-run"]
        ));
        assert!(pending.lock().sessions.is_empty());
        assert!(completions.lock().is_empty());
        assert!(!pending_path.exists());
    }

    #[test]
    fn existing_non_pending_completion_is_success_but_not_a_generation_target() {
        let scope = local_scope(1);
        let completion = ImportSessionCompletedEvent {
            source: ImportSource::Codex,
            session_key: "existing".into(),
            thread_id: "42".into(),
            imported_count: 3,
            success: true,
        };
        assert_eq!(
            classify_session_completion(&PendingImportState::default(), &scope, &completion)
                .unwrap(),
            CompletionDisposition::IgnoreExisting
        );
        let pending = PendingImportState {
            format_version: PENDING_IMPORT_FORMAT_VERSION,
            sessions: vec![pending_session(
                &scope,
                ImportSource::Codex,
                "existing",
                "42",
            )],
        };
        assert_eq!(
            classify_session_completion(&pending, &scope, &completion).unwrap(),
            CompletionDisposition::TrackPending
        );
        let mismatched = ImportSessionCompletedEvent {
            thread_id: "43".into(),
            ..completion
        };
        assert!(classify_session_completion(&pending, &scope, &mismatched).is_err());
    }

    #[test]
    fn pending_success_completion_targets_retry_regardless_of_imported_count() {
        let scope = local_scope(1);
        let session = pending_session(&scope, ImportSource::Codex, "codex:partial", "42");
        let pending = PendingImportState {
            format_version: PENDING_IMPORT_FORMAT_VERSION,
            sessions: vec![session.clone()],
        };
        let zero = ImportSessionCompletedEvent {
            source: session.source,
            session_key: session.session_key.clone(),
            thread_id: session.thread_id.clone(),
            imported_count: 0,
            success: true,
        };

        assert_eq!(
            classify_session_completion(&pending, &scope, &zero).unwrap(),
            CompletionDisposition::TrackPending
        );
        assert_eq!(
            select_targeted_sessions(
                &pending,
                &scope,
                std::slice::from_ref(&zero),
                &[ImportSource::Codex],
            ),
            vec![session.clone()]
        );

        let failed = ImportSessionCompletedEvent {
            success: false,
            ..zero.clone()
        };
        assert!(
            select_targeted_sessions(&pending, &scope, &[failed], &[ImportSource::Codex])
                .is_empty()
        );

        let wrong_source = ImportSessionCompletedEvent {
            source: ImportSource::ClaudeCode,
            ..zero.clone()
        };
        assert!(
            select_targeted_sessions(
                &pending,
                &scope,
                &[wrong_source],
                &[ImportSource::Codex, ImportSource::ClaudeCode],
            )
            .is_empty()
        );

        assert!(
            select_targeted_sessions(&pending, &local_scope(2), &[zero], &[ImportSource::Codex],)
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mismatched_lifecycle_event_source_fails_before_pending_state_is_mutated() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let pending = Arc::new(parking_lot::Mutex::new(PendingImportState::default()));
        let completions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let aggregator = Arc::new(ImportAggregator::new(1));
        let event = concat!(
            "{\"schema\":\"memories-import-event\",\"version\":1,",
            "\"event\":\"thread_created\",\"source\":\"codex\",",
            "\"session_key\":\"codex:session\",\"thread_id\":\"42\"}"
        );
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!("printf '%s\\n' '{event}'"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let state = forward_import_logs(
            command.spawn().unwrap(),
            import_log_context(
                aggregator,
                pending_path.clone(),
                pending.clone(),
                completions.clone(),
                local_scope(1),
            ),
        )
        .await;

        assert!(
            matches!(
                state,
                Some(AggregatedState::AnyFailed(ref errors))
                    if errors.as_slice() == ["config: memories-import lifecycle event source mismatch: expected claude-code, got codex"]
            ),
            "unexpected importer state: {state:?}"
        );
        assert!(pending.lock().sessions.is_empty());
        assert!(completions.lock().is_empty());
        assert!(!pending_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_joins_output_readers_before_the_next_import_updates_pending_state() {
        let tmp = tempfile::tempdir().unwrap();
        let pending_path = tmp.path().join("import-pending.json");
        let pending = Arc::new(parking_lot::Mutex::new(PendingImportState::default()));
        let completions = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let aggregator = Arc::new(ImportAggregator::new(1));
        let old_event = concat!(
            "{\"schema\":\"memories-import-event\",\"version\":1,",
            "\"event\":\"thread_created\",\"source\":\"claude-code\",",
            "\"session_key\":\"old-session\",\"thread_id\":\"42\"}"
        );
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!("printf '%s\\n' '{old_event}'; sleep 5"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let cancel = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(forward_import_logs(
            command.spawn().unwrap(),
            ImportLogContext {
                cancel: cancel.clone(),
                ..import_log_context(
                    aggregator,
                    pending_path.clone(),
                    pending.clone(),
                    completions,
                    local_scope(1),
                )
            },
        ));

        cancel.cancel();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("cancellation must wait for reader shutdown")
            .unwrap();
        assert_eq!(outcome, None);

        let new_session =
            pending_session(&local_scope(1), ImportSource::Codex, "new-session", "43");
        record_pending_import(&pending_path, &mut pending.lock(), new_session.clone()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            load_pending_imports(&pending_path).unwrap().sessions,
            vec![new_session]
        );
    }

    #[test]
    fn pending_state_is_atomic_deduplicated_and_rejects_id_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        let scope = local_scope(1);
        let session = pending_session(&scope, ImportSource::Codex, "stable", "9007199254740993");
        let mut state = PendingImportState::default();
        record_pending_import(&path, &mut state, session.clone()).unwrap();
        record_pending_import(&path, &mut state, session.clone()).unwrap();
        assert_eq!(
            load_pending_imports(&path).unwrap().sessions,
            vec![session.clone()]
        );
        assert!(
            record_pending_import(
                &path,
                &mut state,
                PendingImportSession {
                    thread_id: "43".into(),
                    ..session
                }
            )
            .is_err()
        );
    }

    #[test]
    fn remote_databases_keep_same_session_with_different_thread_ids_separate() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        let mut state = PendingImportState::default();
        let remote_a =
            PendingImportScope::new(ConnectionMode::Remote, "https://remote-a.example:443", 7)
                .unwrap();
        let remote_b =
            PendingImportScope::new(ConnectionMode::Remote, "https://remote-b.example:443", 7)
                .unwrap();
        record_pending_import(
            &path,
            &mut state,
            pending_session(&remote_a, ImportSource::Codex, "codex:same-session", "42"),
        )
        .unwrap();

        let recorded_b = record_pending_import(
            &path,
            &mut state,
            pending_session(&remote_b, ImportSource::Codex, "codex:same-session", "43"),
        );

        assert!(
            recorded_b.is_ok(),
            "a different Memories DB is a separate key"
        );
        assert_eq!(state.sessions.len(), 2);
        let completion_b = ImportSessionCompletedEvent {
            source: ImportSource::Codex,
            session_key: "codex:same-session".into(),
            thread_id: "43".into(),
            imported_count: 1,
            success: true,
        };
        assert_eq!(
            classify_session_completion(&state, &remote_b, &completion_b).unwrap(),
            CompletionDisposition::TrackPending
        );
        let targeted_b =
            select_targeted_sessions(&state, &remote_b, &[completion_b], &[ImportSource::Codex]);
        assert_eq!(
            targeted_b,
            vec![pending_session(
                &remote_b,
                ImportSource::Codex,
                "codex:same-session",
                "43",
            )]
        );
        let after_b_completed = completed_pending_state(&state, &targeted_b, true);
        assert_eq!(after_b_completed.sessions.len(), 1);
        assert!(
            after_b_completed
                .sessions
                .iter()
                .any(|session| remote_a.contains(session))
        );
    }

    #[test]
    fn pending_state_loader_upgrades_empty_format_version_one() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        std::fs::write(&path, r#"{"format_version":1,"sessions":[]}"#).unwrap();

        let state = load_pending_imports(&path).unwrap();
        assert_eq!(state, PendingImportState::default());
    }

    #[test]
    fn pending_state_loader_fails_closed_for_unscoped_format_version_one() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("import-pending.json");
        std::fs::write(
            &path,
            r#"{"format_version":1,"sessions":[{"source":"codex","session_key":"stable","thread_id":"42"}]}"#,
        )
        .unwrap();

        let error = load_pending_imports(&path).unwrap_err().to_string();
        assert!(error.contains("without a Memories endpoint or user scope"));
        assert!(error.contains("inspect and move aside"));
        assert!(error.contains("import-pending.json"));
    }

    #[test]
    fn pending_state_loader_rejects_unknown_format_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        std::fs::write(&path, r#"{"format_version":3,"sessions":[]}"#).unwrap();

        let error = load_pending_imports(&path).unwrap_err().to_string();
        assert!(error.contains("unsupported import pending state format_version"));
    }

    #[test]
    fn pending_state_loader_rejects_malformed_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        std::fs::write(&path, r#"{"format_version":1,"sessions":"#).unwrap();

        let error = load_pending_imports(&path).unwrap_err().to_string();
        assert!(error.contains("read import pending state"));
    }

    #[test]
    fn pending_endpoint_identity_normalizes_remote_authority_without_secrets() {
        let variants = [
            "https://EXAMPLE.com",
            "https://example.com:443/tenant/path?token=secret#fragment",
            "https://user:password@example.com/other",
        ];
        let identities =
            variants.map(|url| pending_memories_endpoint_id(ConnectionMode::Remote, url).unwrap());
        assert_eq!(identities[0], identities[1]);
        assert_eq!(identities[1], identities[2]);
        assert!(identities[0].starts_with("remote-sha256:"));
        assert!(!identities[0].contains("example"));
        assert!(!identities[0].contains("user"));
        assert!(!identities[0].contains("password"));
        assert!(!identities[0].contains("secret"));
        assert_ne!(
            identities[0],
            pending_memories_endpoint_id(ConnectionMode::Remote, "http://example.com:443").unwrap()
        );
    }

    #[test]
    fn pending_state_persists_only_the_remote_endpoint_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("import-pending.json");
        let scope = PendingImportScope::new(
            ConnectionMode::Remote,
            "https://alice:private-token@Secret-Host.example/path?api_key=hidden",
            1,
        )
        .unwrap();
        let mut state = PendingImportState::default();
        record_pending_import(
            &path,
            &mut state,
            pending_session(&scope, ImportSource::Codex, "session", "42"),
        )
        .unwrap();

        let persisted = std::fs::read_to_string(path).unwrap();
        assert!(persisted.contains("remote-sha256:"));
        for secret in ["alice", "private-token", "Secret-Host", "api_key", "hidden"] {
            assert!(!persisted.contains(secret));
        }
    }

    #[test]
    fn pending_endpoint_identity_normalizes_ipv6_and_default_port() {
        let compact =
            pending_memories_endpoint_id(ConnectionMode::Remote, "https://[2001:db8::1]/path")
                .unwrap();
        let expanded = pending_memories_endpoint_id(
            ConnectionMode::Remote,
            "https://[2001:0db8:0:0:0:0:0:1]:443",
        )
        .unwrap();
        assert_eq!(compact, expanded);
        assert_ne!(
            compact,
            pending_memories_endpoint_id(ConnectionMode::Remote, "https://[2001:db8::1]:444",)
                .unwrap()
        );
    }

    #[test]
    fn local_pending_endpoint_identity_ignores_dynamic_sidecar_port() {
        assert_eq!(
            pending_memories_endpoint_id(ConnectionMode::Local, "http://127.0.0.1:49152").unwrap(),
            pending_memories_endpoint_id(ConnectionMode::Local, "http://127.0.0.1:61000").unwrap()
        );
    }

    #[test]
    fn same_remote_database_separates_pending_sessions_by_user() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pending.json");
        let user_a =
            PendingImportScope::new(ConnectionMode::Remote, "https://example.com", 1).unwrap();
        let user_b =
            PendingImportScope::new(ConnectionMode::Remote, "https://example.com:443/path", 2)
                .unwrap();
        let mut state = PendingImportState::default();
        record_pending_import(
            &path,
            &mut state,
            pending_session(&user_a, ImportSource::Codex, "same", "42"),
        )
        .unwrap();
        record_pending_import(
            &path,
            &mut state,
            pending_session(&user_b, ImportSource::Codex, "same", "43"),
        )
        .unwrap();
        assert_eq!(state.sessions.len(), 2);
    }

    #[test]
    fn target_reconciliation_deduplicates_intersects_and_supplements() {
        let emitted = vec!["42".into(), "9007199254740993".into(), "42".into()];
        let candidates = vec!["42".into(), "999".into()];
        let verified = vec![
            VerifiedImportThread {
                thread_id: "42".into(),
                user_id: 1,
                memory_kind: mem_data::MemoryKind::Raw as i32,
            },
            VerifiedImportThread {
                thread_id: "9007199254740993".into(),
                user_id: 1,
                memory_kind: mem_data::MemoryKind::Raw as i32,
            },
        ];
        let result = reconcile_import_targets(&emitted, &candidates, &verified, 1).unwrap();
        assert_eq!(result.target_thread_ids, vec!["42", "9007199254740993"]);
        assert_eq!(result.candidate_intersection, vec!["42"]);
        assert_eq!(result.supplemented_ids, vec!["9007199254740993"]);
    }

    #[test]
    fn target_reconciliation_fails_closed_on_owner_kind_or_missing_id() {
        let emitted = vec!["42".into()];
        for verified in [
            Vec::new(),
            vec![VerifiedImportThread {
                thread_id: "42".into(),
                user_id: 2,
                memory_kind: mem_data::MemoryKind::Raw as i32,
            }],
            vec![VerifiedImportThread {
                thread_id: "42".into(),
                user_id: 1,
                memory_kind: mem_data::MemoryKind::ThreadSummary as i32,
            }],
        ] {
            assert!(reconcile_import_targets(&emitted, &[], &verified, 1).is_err());
        }
    }

    #[test]
    fn clock_reversal_does_not_drop_authoritative_emitted_ids() {
        let emitted = vec!["42".into()];
        let verified = vec![VerifiedImportThread {
            thread_id: "42".into(),
            user_id: 1,
            memory_kind: mem_data::MemoryKind::Raw as i32,
        }];
        let result = reconcile_import_targets(&emitted, &[], &verified, 1).unwrap();
        assert_eq!(result.supplemented_ids, vec!["42"]);
    }

    #[test]
    fn pending_is_kept_on_partial_failure_or_cancel_and_cleared_after_retry_success() {
        let scope = local_scope(1);
        let session = pending_session(&scope, ImportSource::Plain, "file", "42");
        let state = PendingImportState {
            format_version: PENDING_IMPORT_FORMAT_VERSION,
            sessions: vec![session.clone()],
        };
        assert_eq!(
            completed_pending_state(&state, std::slice::from_ref(&session), false),
            state
        );
        assert!(
            completed_pending_state(&state, &[session], true)
                .sessions
                .is_empty()
        );
    }

    #[test]
    fn downstream_failure_then_app_restart_targets_duplicate_retry_with_positive_count() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("import-pending.json");
        let scope = local_scope(1);
        let session = pending_session(&scope, ImportSource::Codex, "codex:partial", "42");
        let mut initial = PendingImportState::default();
        record_pending_import(&path, &mut initial, session.clone()).unwrap();

        let retained = completed_pending_state(&initial, std::slice::from_ref(&session), false);
        save_pending_imports(&path, &retained).unwrap();
        let after_restart = load_pending_imports(&path).unwrap();
        let duplicate_retry = ImportSessionCompletedEvent {
            source: ImportSource::Codex,
            session_key: "codex:partial".into(),
            thread_id: "42".into(),
            // Memories counts duplicate AddMemoriesBatch outcomes in the
            // cumulative completion count on a retry.
            imported_count: 1,
            success: true,
        };

        assert_eq!(
            select_targeted_sessions(
                &after_restart,
                &scope,
                &[duplicate_retry],
                &[ImportSource::Codex],
            ),
            vec![session]
        );
    }

    #[test]
    fn concurrent_import_guard_rejects_second_start_before_work() {
        let first = ImportExecutionGuard::acquire().unwrap();
        assert!(ImportExecutionGuard::acquire().is_err());
        drop(first);
        assert!(ImportExecutionGuard::acquire().is_ok());
    }

    #[test]
    fn empty_authoritative_target_set_dispatches_no_generation_jobs() {
        assert!(!should_dispatch_import_targets(&[]));
        assert!(should_dispatch_import_targets(&["42".into()]));
    }

    #[test]
    fn batch_dispatch_daily_input_matches_workflow_schema() {
        let v = dispatch().period_input(PeriodKind::Daily, PeriodRange::Auto);
        assert_eq!(v["user_id"], 1);
        assert!(v.get("single_workflow_path").is_none());
        assert_eq!(v["output_language"], "ja");
        // Daily reads per-thread `summary` and writes `daily_summary`.
        assert_eq!(v["summary_label"], "summary");
        assert_eq!(v["daily_label"], "daily_summary");
        assert_eq!(v["last_n_days"], 0); // Auto -> fallback
        assert_eq!(v["timezone_offset_hours"], 9);
        assert_eq!(v["max_context_chars"], 200_000);
        assert_eq!(v["force_resummarize"], false);
        // Callback propagated.
        assert_eq!(v["memories_grpc_host"], "127.0.0.1");
        assert_eq!(v["memories_grpc_port"], 9010);
        assert_eq!(v["memories_grpc_tls"], false);
    }

    #[test]
    fn batch_dispatch_weekly_input_matches_workflow_schema() {
        let v = dispatch().period_input(PeriodKind::Weekly, PeriodRange::LastN(4));
        assert_eq!(v["user_id"], 1);
        assert!(v.get("single_workflow_path").is_none());
        assert_eq!(v["output_language"], "ja");
        // Weekly reads `daily_summary` and writes `weekly_summary`.
        assert_eq!(v["daily_label"], "daily_summary");
        assert_eq!(v["weekly_label"], "weekly_summary");
        assert_eq!(v["last_n_weeks"], 4);
    }

    #[test]
    fn batch_dispatch_monthly_input_matches_workflow_schema() {
        let v = dispatch().period_input(PeriodKind::Monthly, PeriodRange::LastN(3));
        assert_eq!(v["user_id"], 1);
        assert!(v.get("single_workflow_path").is_none());
        assert_eq!(v["output_language"], "ja");
        // Monthly reads `weekly_summary` and writes `monthly_summary`.
        assert_eq!(v["weekly_label"], "weekly_summary");
        assert_eq!(v["monthly_label"], "monthly_summary");
        assert_eq!(v["last_n_months"], 3);
    }

    #[test]
    fn period_kind_worker_names_and_prefixes() {
        assert_eq!(
            PeriodKind::Daily.worker_name(),
            "memories-daily-summary-batch"
        );
        assert_eq!(
            PeriodKind::Weekly.worker_name(),
            "memories-weekly-summary-batch"
        );
        assert_eq!(
            PeriodKind::Monthly.worker_name(),
            "memories-monthly-summary-batch"
        );
        assert_eq!(PeriodKind::Daily.job_prefix(), "daily");
        assert_eq!(PeriodKind::Weekly.job_prefix(), "weekly");
        assert_eq!(PeriodKind::Monthly.job_prefix(), "monthly");
    }

    /// Test helper: digest with no carried progress (the common case).
    fn digest(raw: &str) -> String {
        summarize_workflow_chunk(raw, None).0
    }

    #[test]
    fn summarize_workflow_chunk_condenses_running_result() {
        // A real Running chunk whose `output` is the whole thread list must
        // collapse to a one-line `status · <step label>` digest mapped from the
        // position pointer, not the payload nor the raw pointer.
        let raw = r#"{"id":"abc","output":"{\"target_threads\":[ ...huge... ]}","position":"/ROOT/do/5/filterThreads","status":"Running","errorMessage":"{\"target_threads\": ...}"}"#;
        assert_eq!(digest(raw), "Running · 対象スレッドを絞り込み中");
    }

    #[test]
    fn summarize_workflow_chunk_maps_deep_nested_position() {
        // The user-reported long pointer must collapse to the deepest known
        // step label (logError), never the full path.
        let raw = r#"{"position":"/ROOT/do/6/summarizeEach/for/do/0/invokeSingleWithRetry/try/do/0/invokeSingle/run/do/0/logError","status":"Running","output":"{}"}"#;
        assert_eq!(digest(raw), "Running · エラーを記録中");
    }

    #[test]
    fn summarize_workflow_chunk_shows_progress_counter() {
        // When the loop context carries progress, lead with `(N/M)` + step.
        // `output` is a JSON *string* (the context snapshot), so build it via
        // serde to get the escaping exactly right.
        let output = serde_json::json!({
            "progress_processed": 3,
            "progress_total": 36,
            "target_threads": ["...elided..."]
        })
        .to_string();
        let chunk = serde_json::json!({
            "status": "Running",
            "position": "/ROOT/do/6/summarizeEach/for/do/0/invokeSingleWithRetry/try/do/0/invokeSingle/run/do/8/generateSummary/run",
            "output": output,
        })
        .to_string();
        let (d, p) = summarize_workflow_chunk(&chunk, None);
        assert_eq!(d, "(3/36) 要約を生成中 (LLM)");
        assert_eq!(p, Some((3, 36)));
    }

    #[test]
    fn summarize_workflow_chunk_carries_progress_to_later_chunks() {
        // The counter only appears on the reportProgress chunk; a following
        // invokeSingle chunk (no progress in output) must still show (N/M) by
        // carrying the previous value forward.
        let no_progress = serde_json::json!({
            "status": "Running",
            "position": "/ROOT/do/6/summarizeEach/for/do/0/invokeSingleWithRetry/try/do/0/invokeSingle/run/do/8/generateSummary/run",
            "output": "{\"thread\":{}}",
        })
        .to_string();
        // Without a carried value: no counter, just the step.
        let (d0, _) = summarize_workflow_chunk(&no_progress, None);
        assert_eq!(d0, "Running · 要約を生成中 (LLM)");
        // With a carried value from an earlier reportProgress chunk: counter shown.
        let (d1, p1) = summarize_workflow_chunk(&no_progress, Some((4, 36)));
        assert_eq!(d1, "(4/36) 要約を生成中 (LLM)");
        assert_eq!(p1, Some((4, 36)), "carried value must persist");
    }

    #[test]
    fn summarize_workflow_chunk_progress_accepts_string_numbers() {
        // jq `set` may serialize the counter as numeric strings; accept those.
        let output = r#"{"progress_processed":"5","progress_total":"10"}"#;
        let chunk = serde_json::json!({
            "status": "Running",
            "position": "/ROOT/do/6/summarizeEach",
            "output": output,
        })
        .to_string();
        assert_eq!(digest(&chunk), "(5/10) スレッドを要約中");
    }

    #[test]
    fn extract_progress_rejects_zero_total() {
        // `(n/0)` is meaningless — treat as no progress so the caller falls
        // back to the status/step digest.
        assert_eq!(
            extract_progress(r#"{"progress_processed":1,"progress_total":0}"#),
            None
        );
        assert_eq!(extract_progress(r#"{"progress_processed":2}"#), None);
        assert_eq!(extract_progress("{}"), None);
    }

    #[test]
    fn summarize_workflow_chunk_status_only_when_no_position() {
        let raw = r#"{"status":"Completed","output":"{}"}"#;
        assert_eq!(digest(raw), "Completed");
    }

    /// Build a fake `WorkflowResult` terminal chunk whose `output` is the
    /// batch's `output:` block serialized as a JSON string — same shape as
    /// what the WORKFLOW runner emits on the wire.
    fn done_chunk(output: &serde_json::Value) -> String {
        let output_str = output.to_string();
        let env = serde_json::json!({
            "id": "wf-id",
            "status": "Completed",
            "position": "/ROOT",
            "output": output_str,
            "errorMessage": null,
        });
        env.to_string()
    }

    #[test]
    fn summarize_workflow_outcome_done_when_no_failures() {
        let raw = done_chunk(&serde_json::json!({
            "completed": true,
            "succeeded_count": 12,
            "failed_count": 0,
            "processed_threads": 12,
        }));
        let outcome = summarize_workflow_outcome(Some(&raw));
        assert_eq!(outcome.status, StepStatus::Done);
        assert!(outcome.message.is_none());
    }

    #[test]
    fn summarize_workflow_outcome_warning_when_partial_failure() {
        let raw = done_chunk(&serde_json::json!({
            "completed": false,
            "succeeded_count": 9,
            "failed_count": 3,
            "processed_threads": 12,
            "last_error": "429 Too Many Requests",
        }));
        let outcome = summarize_workflow_outcome(Some(&raw));
        assert_eq!(outcome.status, StepStatus::Warning);
        let msg = outcome.message.expect("warning carries a digest");
        assert!(msg.contains("成功 9"));
        assert!(msg.contains("失敗 3"));
        assert!(msg.contains("429"));
    }

    #[test]
    fn summarize_workflow_outcome_accepts_numeric_and_string_failed_counts() {
        for failed_count in [serde_json::json!(3), serde_json::json!("3")] {
            let raw = done_chunk(&serde_json::json!({
                "succeeded_count": 9,
                "failed_count": failed_count,
                "processed_threads": 12,
            }));

            assert_eq!(
                summarize_workflow_outcome(Some(&raw)).status,
                StepStatus::Warning
            );
        }
    }

    #[test]
    fn summarize_workflow_outcome_failed_when_every_item_died() {
        let raw = done_chunk(&serde_json::json!({
            "completed": false,
            "succeeded_count": 0,
            "failed_count": 12,
            "processed_threads": 12,
            "last_error": "RelativeUrlWithoutBase",
        }));
        let outcome = summarize_workflow_outcome(Some(&raw));
        // Every input failed — the batch ran to completion technically, but
        // there's no successful output; surface it as a hard step failure.
        assert_eq!(outcome.status, StepStatus::Failed);
        let msg = outcome.message.expect("failed carries a digest");
        assert!(msg.contains("失敗 12"));
    }

    #[test]
    fn summarize_workflow_outcome_handles_period_batch_counter_names() {
        // daily / weekly / monthly batches use `processed_dates` /
        // `processed_weeks` / `processed_months` instead of
        // `processed_threads`. The classifier must accept all three so a
        // period batch failure is rendered correctly.
        let raw = done_chunk(&serde_json::json!({
            "succeeded_count": 0,
            "failed_count": 7,
            "processed_dates": 7,
        }));
        assert_eq!(
            summarize_workflow_outcome(Some(&raw)).status,
            StepStatus::Failed
        );
    }

    #[test]
    fn summarize_workflow_outcome_falls_back_to_done_when_counters_missing() {
        // A batch YAML that hasn't been updated yet (or whose `output:` was
        // dropped) — don't downgrade to Warning, the previous behaviour wins.
        let raw = done_chunk(&serde_json::json!({
            "completed": true,
        }));
        assert_eq!(
            summarize_workflow_outcome(Some(&raw)).status,
            StepStatus::Done
        );
    }

    #[test]
    fn summarize_workflow_outcome_done_attaches_skipped_digest() {
        // Personality batch's normal path: every NEW thread extracted
        // successfully, plus the budget gate decided not to re-extract
        // several already-processed threads. The step must stay green
        // (Done) but the toast should still surface the "中略 N 件" line so
        // the user knows the existing-signal cap silenced part of the run
        // — same information the mid-flight `skipIfBudgetExhausted` label
        // used to (mis)claim.
        let raw = done_chunk(&serde_json::json!({
            "completed": true,
            "succeeded_count": 12,
            "failed_count": 0,
            "processed_threads": 12,
            "budget_skipped_count": 3,
        }));
        let outcome = summarize_workflow_outcome(Some(&raw));
        assert_eq!(outcome.status, StepStatus::Done);
        let msg = outcome.message.expect("skipped count carries a digest");
        assert!(msg.contains("中略 3"));
    }

    #[test]
    fn summarize_workflow_outcome_warning_includes_skipped_when_present() {
        // The warning path must surface BOTH the partial-failure counter
        // and the skip counter so the detail dialog reflects the full
        // breakdown (success / failure / skipped).
        let raw = done_chunk(&serde_json::json!({
            "completed": false,
            "succeeded_count": 9,
            "failed_count": 3,
            "processed_threads": 14,
            "budget_skipped_count": 2,
            "last_error": "429 Too Many Requests",
        }));
        let outcome = summarize_workflow_outcome(Some(&raw));
        assert_eq!(outcome.status, StepStatus::Warning);
        let msg = outcome.message.expect("warning carries a digest");
        assert!(msg.contains("成功 9"));
        assert!(msg.contains("失敗 3"));
        assert!(msg.contains("中略 2"));
        assert!(msg.contains("429"));
    }

    #[test]
    fn summarize_workflow_outcome_done_skips_skipped_digest_when_zero() {
        // budget_skipped_count == 0 means the budget gate never fired —
        // don't pollute the toast with a "中略 0 件" line.
        let raw = done_chunk(&serde_json::json!({
            "completed": true,
            "succeeded_count": 12,
            "failed_count": 0,
            "processed_threads": 12,
            "budget_skipped_count": 0,
        }));
        let outcome = summarize_workflow_outcome(Some(&raw));
        assert_eq!(outcome.status, StepStatus::Done);
        assert!(outcome.message.is_none());
    }

    #[test]
    fn summarize_workflow_outcome_done_for_none_or_invalid_chunk() {
        // No chunk at all (a stream that closed without a final body) /
        // non-JSON chunk — treat as Done (the workflow itself reported no
        // error). The latter still surfaces the raw text in the message so
        // it isn't silently swallowed.
        assert_eq!(summarize_workflow_outcome(None).status, StepStatus::Done);
        let outcome = summarize_workflow_outcome(Some("not json"));
        assert_eq!(outcome.status, StepStatus::Done);
        assert_eq!(outcome.message.as_deref(), Some("not json"));
    }

    #[test]
    fn summarize_workflow_chunk_falls_back_for_non_json() {
        // Plain (non-JSON) chunk passes through, length-capped.
        assert_eq!(digest("dispatching x"), "dispatching x");
    }

    #[test]
    fn describe_position_maps_known_deepest_step() {
        assert_eq!(
            describe_position("/ROOT/do/6/summarizeEach").as_deref(),
            Some("スレッドを要約中")
        );
        assert_eq!(
            describe_position("/ROOT/do/8/generateSummary/run").as_deref(),
            Some("要約を生成中 (LLM)")
        );
    }

    #[test]
    fn describe_position_maps_personality_batch_budget_steps() {
        // The early-break path adds new steps in thread-personality-batch
        // that the digest must label, or the toast falls back to the raw
        // step name (looking like a workflow bug).
        assert_eq!(
            describe_position("/ROOT/do/6/sortTargetThreadsNewestFirst").as_deref(),
            Some("対象スレッドを並べ替え中"),
        );
        // Renamed from `countExistingValidSignals` — the step now fetches
        // both the count and the processed-thread set used by the
        // per-iteration `classifyThread` guard.
        assert_eq!(
            describe_position("/ROOT/do/7/fetchProcessedSignalMemories").as_deref(),
            Some("既存シグナルを確認中"),
        );
        assert_eq!(
            describe_position("/ROOT/do/8/initSignalBudget").as_deref(),
            Some("処理上限を設定中"),
        );
        assert_eq!(
            describe_position("/ROOT/do/9/personalityEach/for/do/1/classifyThread").as_deref(),
            Some("対象スレッドを判定中"),
        );
        // Regression guard: this step runs for EVERY iteration of the
        // personality batch loop (only the `set:` body is `if:`-gated, the
        // step itself emits a position chunk regardless), so a label that
        // claimed "省略中" would lie to the user whenever a NEW thread was
        // about to be extracted. The label must stay neutral.
        assert_eq!(
            describe_position("/ROOT/do/9/personalityEach/for/do/2/skipIfBudgetExhausted")
                .as_deref(),
            Some("再抽出要否を判定中"),
        );
    }

    #[test]
    fn describe_position_maps_layer_two_recent_signals_step() {
        // findRecentSignals replaces the legacy findSignalThreads +
        // fetchSignalMemories pair. Both legacy names are deliberately
        // dropped from the table; if the toast still renders them, the
        // workflow YAML is out of sync.
        assert_eq!(
            describe_position("/ROOT/do/5/findRecentSignals").as_deref(),
            Some("シグナルを取得中"),
        );
        // Legacy names must NOT map any more — if they did, a toast
        // surfacing them would hide a stale YAML.
        assert_eq!(
            describe_position("/ROOT/do/5/findSignalThreads").as_deref(),
            Some("findSignalThreads"),
        );
        assert_eq!(
            describe_position("/ROOT/do/5/fetchSignalMemories").as_deref(),
            Some("fetchSignalMemories"),
        );
    }

    #[test]
    fn describe_position_passes_unknown_step_through() {
        // An unmapped step name (e.g. a future workflow edit) still shows
        // *something* rather than vanishing.
        assert_eq!(
            describe_position("/ROOT/do/0/someBrandNewStep").as_deref(),
            Some("someBrandNewStep")
        );
    }

    #[test]
    fn describe_position_none_when_only_structural() {
        // Pointer with no meaningful step (only ROOT/do/index) → None so the
        // caller shows the bare status.
        assert_eq!(describe_position("/ROOT/do/3"), None);
        assert_eq!(describe_position(""), None);
    }

    #[test]
    fn is_structural_segment_classifies_scaffolding_and_indices() {
        for s in [
            "ROOT", "do", "for", "try", "catch", "then", "else", "run", "0", "42",
        ] {
            assert!(is_structural_segment(s), "{s} should be structural");
        }
        for s in ["summarizeEach", "generateSummary", "logError"] {
            assert!(!is_structural_segment(s), "{s} should be a step name");
        }
    }

    #[test]
    fn batch_dispatch_summarize_input_includes_last_message_before_ms_when_set() {
        let d = dispatch_with_window(Some(1_746_057_600_000), Some(1_748_735_999_999));
        let v = d.summarize_input();
        assert_eq!(v["last_message_after_ms"], 1_746_057_600_000_i64);
        assert_eq!(v["last_message_before_ms"], 1_748_735_999_999_i64);
    }

    #[test]
    fn batch_dispatch_summarize_input_omits_last_message_before_ms_when_none() {
        let v = dispatch_with_window(Some(1), None).summarize_input();
        assert!(
            v.get("last_message_before_ms").is_none(),
            "must omit last_message_before_ms when None: {v}"
        );
    }

    fn pipeline_req() -> GenerateSummariesRequest {
        GenerateSummariesRequest {
            user_id: Some(1),
            run_per_thread: true,
            run_daily: true,
            run_weekly: true,
            run_monthly: false,
            stop_on_error: false,
            last_message_after_ms: Some(1_746_057_600_000),
            last_message_before_ms: Some(1_748_735_999_999),
            daily_start: "2026-03-01".into(),
            daily_end: "2026-05-31".into(),
            weekly_start: "2026-W09".into(),
            weekly_end: "2026-W22".into(),
            monthly_start: "2026-03".into(),
            monthly_end: "2026-05".into(),
            timezone_offset_hours: 9,
            dispatch_id: None,
        }
    }

    #[test]
    fn pipeline_input_forwards_flags_and_tokens_verbatim() {
        let v = dispatch().pipeline_input(&pipeline_req());
        // Stage gates.
        assert_eq!(v["run_per_thread"], true);
        assert_eq!(v["run_daily"], true);
        assert_eq!(v["run_weekly"], true);
        assert_eq!(v["run_monthly"], false);
        assert_eq!(v["stop_on_error"], false);
        // Period tokens are forwarded as-is — NO conversion in Rust.
        assert_eq!(v["daily_start"], "2026-03-01");
        assert_eq!(v["daily_end"], "2026-05-31");
        assert_eq!(v["weekly_start"], "2026-W09");
        assert_eq!(v["weekly_end"], "2026-W22");
        assert_eq!(v["monthly_start"], "2026-03");
        assert_eq!(v["monthly_end"], "2026-05");
        // Per-thread epoch bounds forwarded as-is.
        assert_eq!(v["last_message_after_ms"], 1_746_057_600_000_i64);
        assert_eq!(v["last_message_before_ms"], 1_748_735_999_999_i64);
        assert_eq!(v["timezone_offset_hours"], 9);
        assert_eq!(v["user_id"], 1);
        assert!(v.get("summary_user_id").is_none());
    }

    #[test]
    fn generate_summaries_request_defaults_stop_on_error_to_false_and_forwards_true() {
        let defaulted: GenerateSummariesRequest = serde_json::from_str(
            r#"{"run_per_thread":false,"run_daily":false,"run_weekly":false,"run_monthly":false,"timezone_offset_hours":9}"#,
        )
        .unwrap();
        assert!(!defaulted.stop_on_error);

        let req: GenerateSummariesRequest = serde_json::from_str(
            r#"{"run_per_thread":false,"run_daily":false,"run_weekly":false,"run_monthly":false,"timezone_offset_hours":9,"stop_on_error":true}"#,
        )
        .unwrap();
        assert_eq!(dispatch().pipeline_input(&req)["stop_on_error"], true);
    }

    #[test]
    fn summaries_pipeline_schema_exposes_best_effort_stop_switch_and_failure_output() {
        let yaml =
            include_str!("../../../workers/workflows/summaries-pipeline/summaries-pipeline.yaml");
        assert!(yaml.contains("stop_on_error: { type: boolean, default: false }"));
        assert!(yaml.contains("per_day_failed_dates: \"${ $per_day_failed_dates }\""));
        assert!(yaml.contains("per_thread_execution_failed"));
        assert!(yaml.contains("daily_execution_failed"));
        assert!(yaml.contains("execution_failures: \"${ $execution_failures }\""));
    }

    #[test]
    fn pipeline_input_includes_batch_yaml_paths_and_output_language() {
        let v = dispatch().pipeline_input(&pipeline_req());
        assert_eq!(
            v["per_thread_batch_yaml"],
            "/x/workflows/thread-summary/thread-summary-batch.yaml"
        );
        assert_eq!(
            v["daily_batch_yaml"],
            "/x/workflows/daily-work-summary/daily-work-summary-batch.yaml"
        );
        assert_eq!(
            v["weekly_batch_yaml"],
            "/x/workflows/weekly-work-summary/weekly-work-summary-batch.yaml"
        );
        assert_eq!(
            v["monthly_batch_yaml"],
            "/x/workflows/monthly-work-summary/monthly-work-summary-batch.yaml"
        );
        // Each layer's batch resolves its single worker by name from
        // `output_language`; the `*_single_yaml` relay is gone.
        assert!(v.get("per_thread_single_yaml").is_none());
        assert!(v.get("daily_single_yaml").is_none());
        assert!(v.get("weekly_single_yaml").is_none());
        assert!(v.get("monthly_single_yaml").is_none());
        assert_eq!(v["output_language"], "ja");
        // Callback merged in.
        assert_eq!(v["memories_grpc_host"], "127.0.0.1");
        assert_eq!(v["memories_grpc_port"], 9010);
    }

    #[test]
    fn pipeline_input_omits_epoch_bounds_when_unbounded() {
        // per-thread-only recovery run: no range → bounds dropped so the
        // batch stays unbounded (mirrors enqueueSummaryJob({})).
        let req = GenerateSummariesRequest {
            run_per_thread: true,
            last_message_after_ms: None,
            last_message_before_ms: None,
            ..Default::default()
        };
        let v = dispatch().pipeline_input(&req);
        assert!(
            v.get("last_message_after_ms").is_none(),
            "unbounded run must omit last_message_after_ms: {v}"
        );
        assert!(
            v.get("last_message_before_ms").is_none(),
            "unbounded run must omit last_message_before_ms: {v}"
        );
        // Empty period tokens are still forwarded (the batch treats "" as
        // "no range" and falls back).
        assert_eq!(v["daily_start"], "");
        assert_eq!(v["daily_end"], "");
    }

    #[test]
    fn summarize_workflow_error_prefers_error_message_capped() {
        let big = "x".repeat(1000);
        let raw = format!(r#"{{"status":"Faulted","errorMessage":"{big}"}}"#);
        let out = summarize_workflow_error(&raw);
        assert!(out.starts_with(&"x".repeat(300)));
        assert!(out.contains("chars total"));
        assert!(out.len() < big.len());
    }

    #[test]
    fn summarize_workflow_error_uses_status_when_no_message() {
        let raw = r#"{"status":"Cancelled"}"#;
        assert_eq!(summarize_workflow_error(raw), "Cancelled");
    }

    #[test]
    fn cap_leaves_short_strings_untouched() {
        assert_eq!(cap("short", 200), "short");
    }

    #[test]
    fn cap_truncates_and_reports_length() {
        let out = cap(&"a".repeat(10), 4);
        assert_eq!(out, "aaaa… (10 chars total)");
    }

    #[test]
    fn batch_dispatch_propagates_remote_tls_callback() {
        // A remote HTTPS memories must flow host/port/tls into every batch
        // input so the fanned-out workflows dial back over TLS.
        let d = BatchDispatch {
            user_id: 1,
            callback: MemoriesCallback {
                host: "memories.example.com".into(),
                port: 8443,
                tls: true,
            },
            workflows_dir: PathBuf::from("/x/workflows"),
            last_message_after_ms: None,
            last_message_before_ms: None,
            thread_created_after_ms: None,
            thread_created_before_ms: None,
            target_thread_ids: None,
            llm_worker_name: "memories-llm".to_string(),
            output_language: "ja".to_string(),
        };
        for v in [
            d.summarize_input(),
            d.personality_input(),
            d.reflection_input(),
        ] {
            assert_eq!(v["memories_grpc_host"], "memories.example.com");
            assert_eq!(v["memories_grpc_port"], 8443);
            assert_eq!(v["memories_grpc_tls"], true);
        }
    }

    #[test]
    fn batch_dispatch_personality_input_matches_workflow_schema() {
        let v = dispatch().personality_input();
        assert!(v.get("single_workflow_path").is_none());
        assert_eq!(v["output_language"], "ja");
        assert!(v.get("personality_user_id").is_none());
        assert_eq!(v["min_user_messages"], 2);
        assert_eq!(v["max_context_chars"], 150_000);
    }

    #[test]
    fn batch_dispatch_personality_input_defaults_force_reextract_false() {
        // The import pipeline must never force re-extraction silently; only
        // the Personality tab's "Force 再抽出" checkbox does that explicitly
        // via `personality_input_with(true)`.
        let v = dispatch().personality_input();
        assert_eq!(v["force_reextract"], false);
    }

    #[test]
    fn batch_dispatch_personality_input_with_true_sets_force_reextract() {
        // Drift guard for the standalone Force re-extract path. Layer-1
        // batch reads `force_reextract` to disable the
        // `target_signal_count` short-circuit AND forwards it to single, so
        // both legs flip with this one flag.
        let v = dispatch().personality_input_with(true);
        assert_eq!(v["force_reextract"], true);
    }

    #[test]
    fn batch_dispatch_personality_input_enables_merge() {
        // The batch chains layer-2 merge itself (so both the standalone button
        // and the import path produce a profile). It now resolves the
        // `memories-user-personality-merge-<lang>` worker by name, gated by the
        // `merge_enabled` flag — the old merge YAML path relay is gone.
        let v = dispatch().personality_input();
        assert_eq!(v["merge_enabled"], true);
        assert!(v.get("merge_workflow_path").is_none());
    }

    #[test]
    fn batch_dispatch_merge_only_input_omits_per_thread_fields() {
        // The merge-only dispatch targets user-personality-merge directly,
        // so it must NOT carry per-thread batch fields — those are unused
        // by the merge YAML and a stray `single_workflow_path` would just
        // be ignored, but a stray `merge_workflow_path` is doubly confusing
        // (it's the dispatch target, not an input). Lock the shape so a
        // future "tidy up" copy/paste from `personality_input_with` cannot
        // accidentally re-introduce them.
        let v = dispatch().merge_only_input(false);
        assert!(v.get("single_workflow_path").is_none());
        assert!(v.get("merge_workflow_path").is_none());
        assert!(v.get("force_reextract").is_none());
    }

    #[test]
    fn batch_dispatch_merge_only_input_forwards_force_remerge_and_callback() {
        // Force-on must reach the merge YAML so the eligibility short-circuit
        // can be bypassed. The callback (memories_grpc_host / port / tls)
        // and llm_worker_name must ride along — without them the merge has
        // no path back to memories nor a way to call the LLM.
        let v = dispatch().merge_only_input(true);
        assert_eq!(v["force_remerge"], true);
        assert_eq!(v["user_id"], 1);
        assert!(v.get("personality_user_id").is_none());
        assert!(v.get("summary_user_id").is_none());
        assert_eq!(v["max_signals"], 100);
        assert_eq!(v["max_context_chars"], 150_000);
        assert_eq!(v["memories_grpc_host"], "127.0.0.1");
        assert_eq!(v["memories_grpc_port"], 9010);
        assert_eq!(v["memories_grpc_tls"], false);
        assert_eq!(v["llm_worker_name"], "memories-llm");
    }

    #[test]
    fn batch_dispatch_merge_only_input_defaults_force_false() {
        let v = dispatch().merge_only_input(false);
        assert_eq!(v["force_remerge"], false);
    }

    #[test]
    fn batch_dispatch_reflection_input_matches_workflow_schema() {
        let v = dispatch().reflection_input();
        assert!(v.get("single_workflow_path").is_none());
        assert_eq!(v["output_language"], "ja");
        assert_eq!(v["prompt_version"], REFLECTION_PROMPT_VERSION);
    }

    #[test]
    fn batch_dispatch_inputs_include_last_message_after_ms_when_set() {
        let d = dispatch_with(Some(1_746_057_600_000));
        for v in [
            d.summarize_input(),
            d.personality_input(),
            d.reflection_input(),
        ] {
            assert_eq!(
                v["last_message_after_ms"], 1_746_057_600_000_i64,
                "input missing last_message_after_ms: {v}"
            );
        }
    }

    #[test]
    fn batch_dispatch_inputs_omit_last_message_after_ms_when_none() {
        let d = dispatch_with(None);
        for v in [
            d.summarize_input(),
            d.personality_input(),
            d.reflection_input(),
        ] {
            assert!(
                v.get("last_message_after_ms").is_none(),
                "input must omit last_message_after_ms when None: {v}"
            );
        }
    }

    fn ok() -> ChildOutcome {
        ChildOutcome::Success { summary: None }
    }

    fn ok_with(s: &str) -> ChildOutcome {
        ChildOutcome::Success {
            summary: Some(s.into()),
        }
    }

    #[test]
    fn aggregator_emits_summaries_on_final_record() {
        let agg = ImportAggregator::new(2);
        assert!(
            agg.record(ok_with("claude-code summary:\n  Sessions: 5"))
                .is_none()
        );
        let state = agg.record(ok_with("codex summary:\n  Sessions: 3"));
        assert_eq!(
            state,
            Some(AggregatedState::AllSucceeded {
                summaries: vec![
                    "claude-code summary:\n  Sessions: 5".into(),
                    "codex summary:\n  Sessions: 3".into(),
                ]
            })
        );
    }

    #[test]
    fn aggregator_emits_failures_on_final_record() {
        let agg = ImportAggregator::new(2);
        assert!(agg.record(ChildOutcome::Failure("a".into())).is_none());
        let state = agg.record(ok());
        assert_eq!(state, Some(AggregatedState::AnyFailed(vec!["a".into()])));
    }

    #[test]
    fn take_final_recovers_when_record_already_drained() {
        // Defensive path: after `record` returned Some it drained the
        // buffers. A second `take_final` from the same aggregator must
        // return an empty AllSucceeded — start_import's fallback relies
        // on this when the per-source loop somehow doesn't capture the
        // first Some.
        let agg = ImportAggregator::new(1);
        let _ = agg.record(ok_with("x"));
        assert_eq!(
            agg.take_final(),
            AggregatedState::AllSucceeded { summaries: vec![] }
        );
    }

    #[test]
    fn extract_summary_block_captures_label_and_indented_body() {
        let lines: Vec<String> = [
            "Starting import…",
            "Processing session abc",
            "",
            "[dry-run] claude-code summary:",
            "  Sessions processed: 587",
            "  Threads created: 587",
            "  Memories imported: 12345",
            "  Memories skipped (duplicate): 10",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = extract_summary_block(&lines).expect("summary block present");
        assert_eq!(
            out,
            "[dry-run] claude-code summary:\n  Sessions processed: 587\n  Threads created: 587\n  Memories imported: 12345\n  Memories skipped (duplicate): 10"
        );
    }

    #[test]
    fn extract_summary_block_stops_at_non_indented_line() {
        let lines: Vec<String> = [
            "claude-code summary:",
            "  Sessions processed: 1",
            "thread-summary-batch dispatch:",
            "  irrelevant: true",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = extract_summary_block(&lines).unwrap();
        assert_eq!(out, "claude-code summary:\n  Sessions processed: 1");
    }

    #[test]
    fn extract_summary_block_returns_none_when_no_marker() {
        let lines: Vec<String> = ["nothing", "to see"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(extract_summary_block(&lines).is_none());
    }

    #[test]
    fn extract_summary_block_uses_last_marker_for_multi_source_output() {
        let lines: Vec<String> = [
            "[dry-run] claude-code summary:",
            "  Sessions processed: 5",
            "[dry-run] codex summary:",
            "  Sessions processed: 3",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = extract_summary_block(&lines).unwrap();
        assert_eq!(out, "[dry-run] codex summary:\n  Sessions processed: 3");
    }

    #[test]
    fn failure_message_appends_stderr_tail() {
        let stderr = [
            "Error: connect to https://memories.example.com:9000".to_string(),
            "  Caused by: invalid peer certificate: UnknownIssuer".to_string(),
        ];
        let msg = failure_message("exit status: 1", &stderr);
        assert_eq!(
            msg,
            "exit status: 1\nError: connect to https://memories.example.com:9000\n  Caused by: invalid peer certificate: UnknownIssuer"
        );
    }

    #[test]
    fn failure_message_keeps_only_the_last_lines_in_order() {
        // More than STDERR_TAIL_LINES: keep the final 5, oldest-first.
        let stderr: Vec<String> = (1..=8).map(|n| format!("line {n}")).collect();
        let msg = failure_message("exit status: 1", &stderr);
        assert_eq!(
            msg,
            "exit status: 1\nline 4\nline 5\nline 6\nline 7\nline 8"
        );
    }

    #[test]
    fn failure_message_is_reason_only_when_stderr_empty() {
        // No stderr (or only blank lines) → just the exit reason, no trailing
        // newline that would render as an empty line in the toast.
        assert_eq!(failure_message("exit status: 1", &[]), "exit status: 1");
        let blanks = ["".to_string(), "   ".to_string()];
        assert_eq!(failure_message("wait: x", &blanks), "wait: x");
    }

    #[test]
    fn start_import_request_accepts_dispatch_id_for_cancel() {
        // Older callers omit dispatch_id; the backend then synthesises
        // an `import-<ts>` id. New callers send a UUID and the
        // response should carry it back so the toast's Stop click can
        // target the right entry.
        let raw = r#"{
            "sources": ["claude-code"],
            "since": null,
            "user_id": 1,
            "dry_run": true,
            "labels": [],
            "memories_import_bin": "/bin/true",
            "run_summary": true,
            "run_personality": true,
            "run_reflection": true,
            "dispatch_id": "abc-uuid"
        }"#;
        let req: StartImportRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.dispatch_id.as_deref(), Some("abc-uuid"));

        let raw_old = r#"{
            "sources": ["claude-code"],
            "since": null,
            "user_id": 1,
            "dry_run": true,
            "labels": [],
            "memories_import_bin": "/bin/true",
            "run_summary": true,
            "run_personality": true,
            "run_reflection": true
        }"#;
        let req_old: StartImportRequest = serde_json::from_str(raw_old).unwrap();
        assert!(req_old.dispatch_id.is_none());
    }

    #[test]
    fn step_label_returns_stable_strings_for_current_step_slot() {
        // The `current_step` slot's value rides the trace log; pin
        // the labels so a future refactor that moves the enum
        // can't silently change the trace contract.
        assert_eq!(step_label(ImportStep::ThreadImport), "thread-import");
        assert_eq!(step_label(ImportStep::ThreadSummary), "thread-summary");
        assert_eq!(
            step_label(ImportStep::ThreadPersonality),
            "thread-personality"
        );
        assert_eq!(step_label(ImportStep::Reflection), "reflection");
    }

    #[test]
    fn cancelled_messages_distinguish_active_step_from_skipped_ones() {
        // The toast surfaces both — the active step (the one that was
        // actually torn down by JobService/Delete) shows "中断", the
        // remaining ones show "中断によりスキップ". Pin the strings
        // because the UI looks them up to colour the row.
        assert_eq!(CANCELLED_ACTIVE_MESSAGE, "中断");
        assert_eq!(CANCELLED_SKIP_MESSAGE, "中断によりスキップ");
    }
}
