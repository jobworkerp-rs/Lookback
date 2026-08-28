//! Sidecar process lifecycle.
//!
//! Spawn jobworkerp `all-in-one` and memories `memories-front` as child
//! processes when the Tauri app launches; stop them — via SIGTERM with a
//! short grace window before SIGKILL — when the app exits or `Drop` runs.
//!
//! Health is determined by polling the gRPC port until a TCP connection
//! succeeds (poor man's health check; gRPC `tonic_health` Check is the
//! follow-up once we know what services are registered).

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep, timeout};
use tracing::{Level, debug, info, warn};

use crate::data::DataPaths;
use crate::error::{AppError, AppResult};
use crate::sidecar::ports;
use crate::sidecar::reaper::{self, PidEntry};

/// Default `RUST_LOG` filters for the resident sidecars.
///
/// Development builds use an `info` baseline. Release bundles use `warn` so a
/// packaged app does not write routine progress and request
/// details to the user's log directory. The `app`, `worker_app`, and `lance`
/// target suppressions apply in both profiles: memories' instrumented gRPC
/// handlers can inline full request payloads, jobworkerp's application target
/// is similarly noisy, and LanceDB emits an INFO line for many internal
/// operations. `LOOKBACK_RUST_LOG` overrides the selected profile.
const DEFAULT_SIDECAR_LOG_DEV: &str = "info,app=warn,worker_app=warn,lance=warn";
const DEFAULT_SIDECAR_LOG_RELEASE: &str = "warn,app=warn,worker_app=warn,lance=warn";
const MEMORIES_START_TIMEOUT: Duration = Duration::from_secs(60);

/// The document prefix is consumed while expanding Lookback's jobworkerp
/// workflow YAMLs. The memories frontend rejects it because its internally
/// chunked text embedding path cannot preserve persisted offsets after
/// prepending text. The query prefix remains valid because it does not alter
/// persisted document chunks.
const MEMORIES_UNSUPPORTED_DOCUMENT_PREFIX_ENV_KEY: &str = "MEMORY_EMBEDDING_DOCUMENT_PREFIX";

/// Environment that makes index maintenance explicitly Lookback-driven.
/// Kept as a pure projector so spawn policy is covered without a child.
pub(crate) fn search_index_maintenance_env_vars(grpc_addr: &str) -> [(&'static str, String); 16] {
    [
        ("SEARCH_INDEX_MAINTENANCE_GRPC_ADDR", grpc_addr.to_string()),
        ("MEMORY_INDEX_UPDATE_INTERVAL_SECS", "0".into()),
        ("THREAD_INDEX_UPDATE_INTERVAL_SECS", "0".into()),
        ("MEMORY_COMPACTION_INTERVAL_SECS", "0".into()),
        ("THREAD_COMPACTION_INTERVAL_SECS", "0".into()),
        ("MEMORY_PRUNE_INTERVAL_SECS", "0".into()),
        ("THREAD_PRUNE_INTERVAL_SECS", "0".into()),
        ("MEMORY_INDEX_UPDATE_UNINDEXED_ROWS", "1000".into()),
        ("THREAD_INDEX_UPDATE_UNINDEXED_ROWS", "1000".into()),
        ("MEMORY_PRUNE_OLDER_THAN_SECS", "300".into()),
        ("THREAD_PRUNE_OLDER_THAN_SECS", "300".into()),
        ("SEARCH_INDEX_MAINTENANCE_CHECK_DEADLINE_SECS", "30".into()),
        ("SEARCH_INDEX_MAINTENANCE_BACKOFF_INITIAL_SECS", "5".into()),
        ("SEARCH_INDEX_MAINTENANCE_BACKOFF_MULTIPLIER", "2".into()),
        ("SEARCH_INDEX_MAINTENANCE_BACKOFF_MAX_SECS", "300".into()),
        ("SEARCH_INDEX_MAINTENANCE_TASK_HISTORY_LIMIT", "256".into()),
    ]
}

fn validate_search_index_maintenance_listener(
    grpc_addr: &str,
    maintenance_addr: &str,
) -> AppResult<()> {
    let grpc: SocketAddr = grpc_addr.parse().map_err(|_| {
        AppError::Config("GRPC_ADDR は loopback address である必要があります".into())
    })?;
    let maintenance: SocketAddr = maintenance_addr
        .parse()
        .map_err(|_| AppError::Config("SEARCH_INDEX_MAINTENANCE_GRPC_ADDR が不正です".into()))?;
    if !grpc.ip().is_loopback() || grpc != maintenance {
        return Err(AppError::Config(
            "maintenance listener は GRPC_ADDR と同一の loopback address である必要があります"
                .into(),
        ));
    }
    Ok(())
}

fn is_legacy_maintenance_env(key: &str) -> bool {
    matches!(
        key,
        "MEMORY_AUTO_OPTIMIZE_INTERVAL" | "THREAD_AUTO_OPTIMIZE_INTERVAL"
    ) || ((key.starts_with("MEMORY_") || key.starts_with("THREAD_")) && key.contains("_OPTIMIZE_"))
}

fn validate_no_legacy_maintenance_env<I>(entries: I) -> AppResult<()>
where
    I: IntoIterator<Item = (String, String)>,
{
    if let Some((key, _)) = entries
        .into_iter()
        .find(|(key, _)| is_legacy_maintenance_env(key))
    {
        return Err(AppError::Config(format!(
            "旧 index maintenance 環境変数 {key} は Local sidecar で使用できません"
        )));
    }
    Ok(())
}

/// TCP health-check budget for the in-process MCP HTTP server. Much shorter
/// than the per-sidecar 30s because by the time we check it the jobworkerp
/// process is already up (its gRPC port passed) and the MCP server binds
/// alongside it via `tokio::join!` — so it is either listening within a few
/// hundred ms or it failed to bind (port clash / startup error), which more
/// waiting won't fix.
const MCP_HEALTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolved runtime config for a single launch. Populated after sidecars
/// finished port selection.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SidecarEndpoints {
    pub jobworkerp_port: u16,
    pub memories_port: Option<u16>,
    pub conductor_port: u16,
    /// MCP HTTP server bind port when the user enabled it (and the sidecar
    /// is up), else `None`. jobworkerp boots the MCP server inside the same
    /// `all-in-one` process as the gRPC front, so this is not a separate
    /// child — it is the port `MCP_ADDR` bound to. Surfaced so the UI can
    /// print the connection URL for an external MCP client.
    #[serde(default)]
    pub mcp_server_port: Option<u16>,
}

impl SidecarEndpoints {
    pub fn jobworkerp_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.jobworkerp_port)
    }

    pub fn memories_url(&self) -> Option<String> {
        self.memories_port
            .map(|port| format!("http://127.0.0.1:{port}"))
    }

    pub fn conductor_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.conductor_port)
    }
}

/// Discriminator on `SidecarWarning`. Kept as a string-enum so the
/// frontend can branch on `kind` directly. Add new variants here when
/// surfacing a different non-fatal startup condition.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarWarningKind {
    /// `jobworkerp-client worker apply` returned non-zero. Most commonly
    /// because a runner dylib (LLMPromptRunner, MultimodalEmbeddingRunner)
    /// is missing from `PLUGINS_RUNNER_DIR`. Import still works; summary /
    /// personality / reflection / semantic search will fail.
    WorkerApplyFailed,
    /// `plugins::stage_plugins` couldn't resolve a source directory or
    /// failed to copy. Same downstream impact as `WorkerApplyFailed`.
    PluginsStageFailed,
    /// The local LanceDB could not be opened at the configured embedding
    /// dimension, so the memories sidecar was restarted with the vector
    /// store disabled (degraded mode). Startup is allowed to proceed —
    /// browse / FTS keep working — but embedding-dependent features
    /// (semantic / hybrid / intent search, import, generation) are
    /// unavailable until the user switches the embedding model back to the
    /// matching dimension in Settings. `SidecarWarning.detail` carries a
    /// JSON blob (`DegradedInfo`) with `expected_dim` / `actual_dim`.
    VectorStoreDegraded,
}

/// Non-fatal startup condition that the UI should surface to the user.
/// Emitted in `SidecarStartReport.warnings` alongside the ready event.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SidecarWarning {
    pub kind: SidecarWarningKind,
    pub message: String,
    /// Source artifact when applicable (e.g. the YAML that failed to
    /// apply). Helps the user know which file to check.
    pub detail: Option<String>,
}

/// Payload of the `sidecar://ready` event. Carries the endpoints the UI
/// needs to connect plus any non-fatal warnings collected during startup.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SidecarStartReport {
    #[serde(flatten)]
    pub endpoints: SidecarEndpoints,
    pub warnings: Vec<SidecarWarning>,
}

/// Diagnostics for a degraded (vector-store-disabled) startup. Recorded on
/// `Sidecars::degraded` and serialized into the `SidecarWarning.detail` of
/// the `VectorStoreDegraded` warning so the frontend can render the
/// dimension mismatch. `reason` mirrors the originating `StartupFailure`
/// code (`lancedb_schema_mismatch` / `embedding_dimension_mismatch`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DegradedInfo {
    pub reason: &'static str,
    pub expected_dim: u32,
    pub actual_dim: u32,
}

impl DegradedInfo {
    /// Build the `VectorStoreDegraded` warning for this diagnostic. The
    /// human-readable `message` is a fallback; the frontend renders its own
    /// i18n copy off the JSON `detail` (dims). Keeping the construction on
    /// the type keeps the presentation out of `start_inner`.
    fn into_warning(self) -> SidecarWarning {
        let message = format!(
            "ローカルのベクトルストアが次元不一致（期待 {} / 実際 {}）で無効化されました",
            self.expected_dim, self.actual_dim
        );
        let detail = serde_json::to_string(&self).ok();
        SidecarWarning {
            kind: SidecarWarningKind::VectorStoreDegraded,
            message,
            detail,
        }
    }
}

/// Decide whether a memories startup failure is a local vector-dimension
/// mismatch that should trigger a degraded (vector-disabled) restart, or a
/// genuinely fatal error that must surface to BootError. Pure so the policy
/// is unit-tested without spinning up a real sidecar.
///
/// Only the two dimension-mismatch codes are recoverable this way:
/// disabling the vector store sidesteps the LanceDB open that produced them.
/// Every other `StartupFailure` (RDB pool, env var, media conflict, LanceDB
/// init IO error, …) is left fatal — restarting with vectors off would not
/// fix them and would silently hide the real problem.
pub(crate) fn plan_degraded_retry(err: &AppError) -> Option<DegradedInfo> {
    use crate::sidecar::startup_error::StartupFailure;
    let AppError::SidecarStartupFailed(failure) = err else {
        return None;
    };
    match failure {
        StartupFailure::LancedbSchemaMismatch {
            expected_dim,
            actual_dim,
            ..
        } => Some(DegradedInfo {
            reason: "lancedb_schema_mismatch",
            expected_dim: *expected_dim,
            actual_dim: *actual_dim,
        }),
        StartupFailure::EmbeddingDimensionMismatch {
            expected_dim,
            actual_dim,
            ..
        } => Some(DegradedInfo {
            reason: "embedding_dimension_mismatch",
            expected_dim: *expected_dim,
            actual_dim: *actual_dim,
        }),
        _ => None,
    }
}

fn apply_memories_vector_env(
    cmd: &mut Command,
    vector_on: bool,
    vector_size: u32,
    memory_lancedb_uri: &Path,
) {
    if vector_on {
        cmd.env("MEMORY_VECTOR_ENABLED", "true")
            // ThreadVectorDBConfig deliberately inherits its size and URI
            // from MEMORY_*; this flag is the only separate prerequisite.
            .env("THREAD_VECTOR_ENABLED", "true")
            .env("MEMORY_VECTOR_SIZE", vector_size.to_string())
            .env(
                "MEMORY_LANCEDB_URI",
                memory_lancedb_uri.to_string_lossy().as_ref(),
            )
            // Opt-in ON TOP OF MEMORY_VECTOR_ENABLED: creates the
            // reflection_intent LanceDB table at startup. Without it
            // `FindSimilarByIntentText` / `FindSimilarTrajectories` return
            // Unimplemented (memories dot.env §Reflection).
            .env("REFLECTION_INTENT_VECTOR_ENABLED", "true");
    } else {
        // Degraded restart must win over env_file / inherited env; otherwise a
        // template with vectors enabled reopens the mismatched LanceDB.
        cmd.env("MEMORY_VECTOR_ENABLED", "false")
            .env("THREAD_VECTOR_ENABLED", "false")
            .env("REFLECTION_INTENT_VECTOR_ENABLED", "false")
            .env_remove("MEMORY_VECTOR_SIZE")
            .env_remove("MEMORY_LANCEDB_URI")
            .env_remove("REFLECTION_VECTOR_SIZE")
            .env_remove("REFLECTION_LANCEDB_URI");
    }
}

#[derive(Debug, Clone)]
pub struct SidecarConfig {
    /// Path to the jobworkerp `all-in-one` binary. When sidecar is bundled
    /// via Tauri externalBin, this points inside the .app bundle. In dev,
    /// we point at the cargo target dir.
    pub jobworkerp_bin: PathBuf,

    /// Path to the memories `memories-front` binary.
    pub memories_bin: PathBuf,

    /// Memories-owned coordinator from the official SQLite migration bundle.
    pub memories_db_migrate_bin: PathBuf,

    /// Release-owned switch; it is never loaded from user settings.
    pub startup_database_migration_enabled: bool,

    /// Path to the conductor `conductor-main` binary.
    pub conductor_bin: PathBuf,

    /// Where all persistent state lives (sqlite, lancedb, plugins, logs).
    pub data: DataPaths,

    /// Worker YAMLs applied via the in-process `jobworkerp-client` crate
    /// before memories spawns, so LLM-containing workflows can resolve
    /// the `memories-llm` named worker on their first dispatch.
    pub worker_yaml_paths: Vec<PathBuf>,

    /// Function-set YAMLs applied AFTER `worker_yaml_paths`. Each set's
    /// target list references workers by name, so target → WorkerId
    /// resolution requires the named workers already to exist. Used by
    /// the RAG chat (`lookback-rag` set; rag-chat-design.md
    /// DECIDE-CHAT-9). Empty means "no sets to register".
    pub function_set_yaml_paths: Vec<PathBuf>,

    /// Sets `MEMORY_REFLECTION_DISPATCH_ENABLED` on the memories child.
    /// Toggling this off keeps the Rust import driver and manual reflection
    /// dispatch from invoking reflection generation.
    pub reflection_dispatch_enabled: bool,

    /// Sets `MEMORY_AUTO_EMBEDDING_ENABLED` on the memories child. When
    /// true, memories registers the `memories-mm-embedding` worker (and the
    /// reflection intent dispatcher) so Hybrid / Semantic / intent search
    /// have vectors to query. Requires the `embedding` / `embedding_workflow`
    /// jobworkerp channels and the Metal embedding plugin. Also gates the
    /// vector-store env (`MEMORY_VECTOR_ENABLED` / `THREAD_VECTOR_ENABLED` /
    /// `MEMORY_VECTOR_SIZE` / `MEMORY_LANCEDB_URI` /
    /// `REFLECTION_INTENT_VECTOR_ENABLED`).
    pub auto_embedding_enabled: bool,

    /// Bundled worker/workflow YAML directory (the agent-app copies with
    /// unified `memories-mm-embedding` names). When set,
    /// `MEMORY_WORKERS_YAML` / `REFLECTION_INTENT_WORKERS_YAML` /
    /// `REFLECTION_WORKERS_YAML` point the memories dispatchers at these
    /// instead of memories' compile-time `CARGO_MANIFEST_DIR`-relative
    /// defaults, which may not match the packaged embedding plugin backend.
    pub workflows_dir: Option<PathBuf>,

    /// `LANCE_LANGUAGE_MODEL_HOME` for the Lindera FTS dictionary. Always
    /// injected so a lindera-feature `front` build finds the dictionary.
    pub lance_language_model_home: PathBuf,

    /// True once the Lindera IPADIC dictionary has been staged under
    /// `lance_language_model_home`. When false we force
    /// `MEMORY_FTS_TOKENIZER=ngram` so a lindera-feature build doesn't fail
    /// FTS index creation looking for an absent dictionary (spec §3.R3).
    pub lindera_dict_staged: bool,

    /// Override default LLM model settings. When set, passed
    /// to the memories-llm Worker via env-var expansion in its YAML
    /// (`%{LOOKBACK_LLM_MODEL:-…}` / `%{LOOKBACK_LLM_HF_REPO:-…}` /
    /// `%{LOOKBACK_LLM_CTX_SIZE:-…}` /
    /// `%{LOOKBACK_LLM_KV_CACHE_TYPE:-…}` in `workers/llm-workers.yaml`). The
    /// env names must match the YAML placeholders exactly — see
    /// `jobworkerp_env_names_match_yaml_placeholders` for the pinning
    /// regression test.
    pub llm_model: Option<String>,
    pub llm_hf_repo: Option<String>,
    pub llm_ctx_size: Option<u32>,
    pub llm_kv_cache_type: Option<String>,

    /// Optional `.env` file inherited by both sidecar children. jobworkerp /
    /// memories read a lot of optional knobs (MEMORY_CACHE_*, channel
    /// concurrencies, etc.) from `dotenvy::dotenv()` against their own cwd —
    /// enumerating them all in code would mean drift between this app and
    /// the upstream `.env` template. Instead we parse the file once here
    /// and forward each KEY=VALUE as a process env var, then layer our
    /// Tauri-specific overrides on top.
    pub env_file: Option<PathBuf>,
}

/// Build the `(name, value)` env pairs forwarded to the jobworkerp child
/// to drive the local LLM Worker's `%{LOOKBACK_LLM_*:-…}` YAML
/// placeholders. Pure so the env-name → YAML-placeholder alignment is
/// unit-tested — getting these names wrong silently keeps the previous
/// model loaded (the placeholders fall back to their `:-default`).
fn jobworkerp_llm_env_vars(
    model: Option<&str>,
    hf_repo: Option<&str>,
    ctx_size: Option<u32>,
    kv_cache_type: Option<&str>,
    mtp_enabled: Option<bool>,
    mtp_draft_model: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::with_capacity(6);
    if let Some(model) = model.filter(|s| !s.is_empty()) {
        out.push(("LOOKBACK_LLM_MODEL", model.to_string()));
    }
    if let Some(repo) = hf_repo.filter(|s| !s.is_empty()) {
        out.push(("LOOKBACK_LLM_HF_REPO", repo.to_string()));
    }
    if let Some(ctx) = ctx_size {
        out.push(("LOOKBACK_LLM_CTX_SIZE", ctx.to_string()));
    }
    if let Some(kv) = kv_cache_type.filter(|s| !s.is_empty()) {
        out.push(("LOOKBACK_LLM_KV_CACHE_TYPE", kv.to_string()));
    }
    if let Some(enabled) = mtp_enabled {
        out.push(("LOOKBACK_LLM_MTP_ENABLED", enabled.to_string()));
    }
    if let Some(draft) = mtp_draft_model.filter(|s| !s.is_empty()) {
        out.push(("LOOKBACK_LLM_MTP_DRAFT_MODEL", draft.to_string()));
    }
    out
}

/// Keep memories' envy-based cache config complete. Upstream switches from
/// `Default` to full deserialization as soon as any `MEMORY_CACHE_*` variable
/// exists, so inheriting only one variable from a GUI launch environment
/// otherwise makes the sidecar exit before binding its gRPC port.
fn memories_cache_env_vars() -> [(&'static str, &'static str); 3] {
    [
        ("MEMORY_CACHE_NUM_COUNTERS", "12960"),
        ("MEMORY_CACHE_MAX_COST", "12960"),
        ("MEMORY_CACHE_USE_METRICS", "true"),
    ]
}

fn memories_sqlite_env_vars(data: &DataPaths) -> [(&'static str, String); 2] {
    [
        ("SQLITE_URL", data.memories_sqlite_url()),
        // `front` deserializes both fields of RdbUrlConfigImpl. Supplying the
        // URL alone makes it silently fall back to its default database.
        (
            "SQLITE_MAX_CONNECTIONS",
            crate::data::DataPaths::MEMORIES_SQLITE_MAX_CONNECTIONS.to_string(),
        ),
    ]
}

fn effective_memory_fts_tokenizer(
    env_file: Option<&Path>,
    lindera_dict_staged: bool,
    process_env: impl Fn(&str) -> Option<String>,
) -> String {
    if !lindera_dict_staged {
        return "ngram".into();
    }
    let from_file = env_file
        .and_then(|path| dotenvy::from_path_iter(path).ok())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .find_map(|(key, value)| (key == "MEMORY_FTS_TOKENIZER").then_some(value));
    from_file
        .or_else(|| process_env("MEMORY_FTS_TOKENIZER"))
        .unwrap_or_else(|| "lindera/ipadic".into())
}

/// Keep jobworkerp's envy-based worker config complete. Supplying channels
/// without the required default concurrency makes the whole config fall back
/// to the default channel only, so named worker registration then fails.
fn jobworkerp_channel_env_vars() -> [(&'static str, &'static str); 3] {
    [
        ("WORKER_DEFAULT_CONCURRENCY", "4"),
        (
            "WORKER_CHANNELS",
            "llm,llm_external,llm_workflow,llm_batch,llm_pipeline,llm_periodic,embedding,embedding_workflow,rag",
        ),
        ("WORKER_CHANNEL_CONCURRENCIES", "1,2,1,2,1,1,1,1,2"),
    ]
}

/// Read the host's IANA timezone name from the `/etc/localtime` symlink
/// (`.../zoneinfo/<Area>/<Location>` → `<Area>/<Location>`). This is the
/// canonical source on macOS and most Linux distros; a GUI launch inherits
/// no shell `TZ`, so without this the sidecars would always fall back to the
/// JST default even on a machine set to another zone. Returns `None` when
/// `/etc/localtime` is absent or not a `zoneinfo` symlink (e.g. a bind mount)
/// so the caller can fall back.
/// Last-resort timezone when no explicit setting, `TZ` env, or host zone is
/// available — preserves the fixtures' historical JST wall-clock semantics.
/// Must stay in sync with the frontend's `resolveTimezoneOffsetHours` fallback
/// (`dateInput.ts`, 9 = JST) and the workflow YAML `timezone_offset_hours` default.
const DEFAULT_TIMEZONE: &str = "Asia/Tokyo";

fn system_timezone_name() -> Option<String> {
    let target = std::fs::read_link("/etc/localtime").ok()?;
    let s = target.to_str()?;
    // Split on the last "zoneinfo/" so nested paths like
    // "/var/db/timezone/zoneinfo/America/New_York" still yield the IANA name.
    let name = s.rsplit_once("zoneinfo/").map(|(_, rest)| rest)?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// IANA timezone name forwarded to the sidecars' `TZ` env. The
/// agent-chat summary / import workflows read `env.TZ` inside the
/// jobworkerp worker's jq to make their day/week/month boundaries
/// DST-aware (memories 5e996f5); a DMG/GUI launch inherits an
/// essentially empty env, so the value must be resolved and injected
/// explicitly rather than relying on an inherited shell `TZ`.
///
/// Resolution order: explicit `app-settings.json` `timezone` (a GUI
/// selection — honoured over the env so the choice is deterministic even
/// under a DMG launch) → `TZ` env (dev shell override / the "Auto" case)
/// → the host's `/etc/localtime` zone (so a machine set to e.g.
/// `America/New_York` gets DST-aware, west-of-UTC boundaries even under a
/// GUI launch) → `Asia/Tokyo` as a last resort, preserving the historical
/// JST wall-clock semantics of the fixtures. Pass `None` for `app_settings`
/// to skip the persisted layer (env/OS only). Shared with
/// `conductor_env_vars`, which additionally honours `CONDUCTOR_CRON_TIMEZONE`
/// on top of this.
pub(crate) fn resolve_timezone(app_settings: Option<&crate::data::paths::AppSettings>) -> String {
    app_settings
        .and_then(|s| s.timezone.clone())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("TZ").ok().filter(|s| !s.trim().is_empty()))
        .or_else(system_timezone_name)
        .unwrap_or_else(|| DEFAULT_TIMEZONE.to_string())
}

fn conductor_env_vars(data: &DataPaths, port: u16) -> Vec<(&'static str, String)> {
    let app_settings = crate::data::paths::load_app_settings(&data.app_settings_path());
    let timezone = std::env::var("CONDUCTOR_CRON_TIMEZONE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| resolve_timezone(Some(&app_settings)));
    vec![
        ("GRPC_ADDR", format!("127.0.0.1:{port}")),
        (
            "SQLITE_URL",
            format!(
                "sqlite://{}/conductor.sqlite3?mode=rwc",
                data.root.display()
            ),
        ),
        ("SQLITE_MAX_CONNECTIONS", "5".to_string()),
        ("CONDUCTOR_CRON_TIMEZONE", timezone),
        ("NOTIFICATION_TYPE", "channel".to_string()),
        // conductor-main's AppModule init does `envy::prefixed("MEMORY_CACHE_")
        // .from_env::<MemoryCacheConfig>().expect(...)` (conductor/api/app/src/
        // module.rs), and `num_counters` / `max_cost` / `use_metrics` are all
        // REQUIRED fields with no serde default on that `.expect()` path — so a
        // launch environment missing them panics the process before it binds
        // its gRPC port. A DMG/GUI launch inherits an essentially empty env (no
        // shell `.env`, no `conductor/dot.env`), so the full set must be passed
        // explicitly. Values mirror conductor's own `dot.env`. Same class of
        // fix as `memories_cache_env_vars`, which the memories child needs for
        // the identical envy "any one key flips to full deserialization" reason.
        ("MEMORY_CACHE_NUM_COUNTERS", "12960".to_string()),
        ("MEMORY_CACHE_MAX_COST", "1000000".to_string()),
        ("MEMORY_CACHE_USE_METRICS", "true".to_string()),
    ]
}

/// Re-resolve the HF cache root from the persisted `app-settings.json` +
/// the (legacy) `.env` template. Called both at `Sidecars::new` and at
/// every `start_inner`, so a Settings change takes effect on the next
/// sidecar restart without touching the app process.
///
/// Falls back to `AppSettings::default()` (= `HfHomeMode::Global`) when
/// the user hasn't opened Settings yet, matching what the HfHomeCard
/// shows as the selected mode. Without this alignment a fresh install
/// renders "OS グローバル" as the default in the UI while the sidecar
/// silently uses `<data>/models` — the user thinks their
/// `~/.cache/huggingface` cache will be reused, but it isn't, so every
/// model gets re-downloaded.
///
/// A shell `HF_HOME` is still honoured: `HfHomeMode::Global` consults
/// the env var first inside `global_hf_home()`.
fn resolve_effective_hf_home(config: &SidecarConfig) -> PathBuf {
    let app_settings = crate::data::paths::load_app_settings(&config.data.app_settings_path());
    config
        .data
        .effective_hf_home(config.env_file.as_deref(), Some(&app_settings))
}

/// One running sidecar child plus the port it's listening on.
struct Process {
    name: &'static str,
    child: Child,
    port: u16,
}

type OutputTask = (&'static str, tokio::task::JoinHandle<()>);

pub struct Sidecars {
    config: SidecarConfig,
    /// Serializes lifecycle operations on this instance. The migration gate,
    /// child spawning, and lock release all inspect and mutate shared state,
    /// so checking `current_endpoints` alone cannot make concurrent starts or
    /// stops safe.
    lifecycle_lock: Arc<tokio::sync::Mutex<()>>,
    /// Exposed through the startup snapshot while the local database
    /// migration gate owns the database. Kept independent of lifecycle_lock
    /// so the UI can observe progress without waiting for startup to finish.
    database_migration_in_progress: Arc<AtomicBool>,
    /// Resolved `HF_HOME` precedence (app-settings.json → shell env →
    /// `.env` template → `<data>/models` fallback). Updated at every
    /// `start_inner` so a `set_hf_home`-triggered restart picks up the
    /// new mode without an app relaunch. The `get_model_status`
    /// readiness scan (polled every 3 s) reads this so the scan tracks
    /// whichever cache the sidecar was last spawned with.
    effective_hf_home: Arc<Mutex<PathBuf>>,
    state: Arc<Mutex<Option<Vec<Process>>>>,
    /// Handles for stdout/stderr pumps. They are joined after children exit
    /// so the final buffered lines are flushed before a restart or shutdown.
    output_tasks: Arc<Mutex<Vec<OutputTask>>>,
    /// Native log directories allocated for resident sidecars. They remain
    /// active while a child is running and are marked closed only after the
    /// child/output pump has stopped, allowing retention to prune safely.
    native_log_dirs: Arc<Mutex<Vec<(String, PathBuf)>>>,
    /// Last successful start report, kept so `get_sidecar_status` can hand
    /// the frontend an immediate snapshot (endpoints + warnings) instead of
    /// racing the one-shot `sidecar://ready` event on mount.
    last_report: Arc<Mutex<Option<SidecarStartReport>>>,
    /// Last hard-startup-failure message (spawn error, port pick, TCP health
    /// check timeout). Unlike `last_report.warnings` (non-fatal), this is the
    /// fatal path where `start_with_warnings` returns `Err`. Kept so
    /// `get_model_status` can surface the `failed` state when the sidecars
    /// never came up at all. Cleared on a successful start and
    /// on stop, mirroring `last_report`'s lifecycle.
    last_start_error: Arc<Mutex<Option<String>>>,
    /// Frontend-shaped snapshot of the last hard-startup failure. Same
    /// lifecycle as `last_start_error`, but carries the structured
    /// payload (`SidecarErrorPayload::Structured { failure: ... }` etc.)
    /// rather than the stringified Display. Lets `get_sidecar_status`
    /// hand BootError a recovery-actionable payload when the React tree
    /// mounted *after* the one-shot `sidecar://error` event already
    /// fired — without this, a memories child that died at init before
    /// the listener attached would leave the UI stuck on the boot
    /// spinner.
    last_start_failure: Arc<Mutex<Option<crate::sidecar::startup_error::SidecarErrorPayload>>>,
    /// Per-instance advisory lock (over `<root>/sidecar.lock`), held while this
    /// instance has sidecars up or performs an exclusive maintenance action.
    /// A concurrent launch can only reap recorded orphans if it can take this
    /// lock — so we never SIGKILL another live instance's children. Acquired in
    /// `start_inner` (via `reaper::reap_recorded`) and normally released on
    /// `stop` / `stop_blocking` / Drop.
    instance_lock: Arc<Mutex<Option<reaper::InstanceLock>>>,
    /// Structured startup failure parsed out of the memories child's
    /// stdout (via `parse_stdout_line`) or, fallback, its stderr panic
    /// preamble (`parse_stderr_panic_line`). Reset to `None` at the top
    /// of every `start_inner` so a previous failure does not bleed into
    /// the next attempt. `wait_for_tcp_or_failure` polls this slot so a
    /// structured failure short-circuits the 30 s TCP wait, and
    /// `stage_and_start_sidecars` lifts it into `SidecarErrorPayload`
    /// for the frontend.
    startup_failure: crate::sidecar::startup_error::StartupFailureSlot,
    /// Name of the provider API-key env var (`OPENAI_API_KEY`, …) that was
    /// injected into the jobworkerp CHILD at its last spawn, or `None` if no
    /// key was injected (no provider model configured, or no Keychain key).
    /// A genai API key reaches the worker ONLY through this spawn-time env
    /// (the `GenaiRunnerSettings` proto has no key field — see
    /// `llm_settings::hot_reload_safe`), so a Local→External switch can be
    /// hot-reloaded WITHOUT a restart only when the running child already
    /// carries the key the new provider needs. This records what it carries
    /// so that gate can be evaluated. Updated on every `start_inner`.
    spawned_external_key_env: Arc<Mutex<Option<String>>>,
    /// MCP HTTP server's bound port when the user enabled it at the last
    /// `start_inner`, else `None`. jobworkerp boots the MCP server in the
    /// same process as the gRPC front (it is NOT a separate child in
    /// `state`), so the live port is not derivable from the process list —
    /// it is stashed here so `get_mcp_settings` / `get_sidecar_status` can
    /// hand the UI the connection URL. Cleared on stop.
    active_mcp_port: Arc<Mutex<Option<u16>>>,
    /// `Some` when the last `start_inner` had to restart the memories child
    /// with the vector store disabled because the local LanceDB could not
    /// be opened at the configured embedding dimension (degraded mode).
    /// Read by the command-layer gate (`ensure_local_embedding_available`)
    /// to refuse embedding-dependent dispatches, and surfaced to the UI via
    /// the `VectorStoreDegraded` warning. Reset to `None` at the top of
    /// every `start_inner` (so a subsequent clean start clears it) and on
    /// `stop` / `stop_blocking`.
    degraded: Arc<Mutex<Option<DegradedInfo>>>,
}

/// Return the signed resource plugin directory for a bundled macOS sidecar.
/// Development executables do not have this layout and intentionally fall
/// back to the writable data directory.
#[cfg(any(target_os = "macos", test))]
fn macos_bundle_plugins_dir(jobworkerp_bin: &Path) -> Option<PathBuf> {
    let macos_dir = jobworkerp_bin.parent()?;
    let contents_dir = macos_dir.parent()?;
    if macos_dir.file_name()? != "MacOS" || contents_dir.file_name()? != "Contents" {
        return None;
    }
    let plugins = contents_dir.join("Resources/plugins");
    plugins.is_dir().then_some(plugins)
}

/// Clears the migration activity signal even if the startup future is dropped
/// while the coordinator is still running.
struct DatabaseMigrationActivity {
    in_progress: Arc<AtomicBool>,
}

impl DatabaseMigrationActivity {
    fn start(in_progress: Arc<AtomicBool>) -> Self {
        in_progress.store(true, Ordering::Release);
        Self { in_progress }
    }
}

impl Drop for DatabaseMigrationActivity {
    fn drop(&mut self) {
        self.in_progress.store(false, Ordering::Release);
    }
}

impl Sidecars {
    pub(crate) fn owns_instance_lock(&self) -> bool {
        self.instance_lock.lock().is_some()
    }
    pub fn new(config: SidecarConfig) -> Self {
        let effective_hf_home = resolve_effective_hf_home(&config);
        Self {
            config,
            lifecycle_lock: Arc::new(tokio::sync::Mutex::new(())),
            database_migration_in_progress: Arc::new(AtomicBool::new(false)),
            effective_hf_home: Arc::new(Mutex::new(effective_hf_home)),
            state: Arc::new(Mutex::new(None)),
            output_tasks: Arc::new(Mutex::new(Vec::new())),
            native_log_dirs: Arc::new(Mutex::new(Vec::new())),
            last_report: Arc::new(Mutex::new(None)),
            last_start_error: Arc::new(Mutex::new(None)),
            last_start_failure: Arc::new(Mutex::new(None)),
            instance_lock: Arc::new(Mutex::new(None)),
            startup_failure: Arc::new(Mutex::new(None)),
            spawned_external_key_env: Arc::new(Mutex::new(None)),
            active_mcp_port: Arc::new(Mutex::new(None)),
            degraded: Arc::new(Mutex::new(None)),
        }
    }

    fn plugin_runner_dir(&self) -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            return macos_bundle_plugins_dir(&self.config.jobworkerp_bin)
                .unwrap_or_else(|| self.config.data.plugins_dir());
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.config.data.plugins_dir()
        }
    }

    /// Degraded-mode diagnostics from the last start, or `None` when the
    /// vector store came up normally. `Some` means the memories child is
    /// running with `MEMORY_VECTOR_ENABLED=false` because the local
    /// LanceDB dimension did not match the configured embedding model.
    pub fn degraded(&self) -> Option<DegradedInfo> {
        self.degraded.lock().clone()
    }

    /// Test-only: seed the degraded flag so command-layer gate tests can
    /// simulate a vector-disabled restart without spinning up a real
    /// dimension-mismatched sidecar.
    #[cfg(test)]
    pub(crate) fn set_degraded_for_test(&self, info: Option<DegradedInfo>) {
        *self.degraded.lock() = info;
    }

    /// MCP HTTP server bind port the running jobworkerp child is listening
    /// on, or `None` when MCP is disabled / the sidecars are stopped.
    pub fn active_mcp_port(&self) -> Option<u16> {
        *self.active_mcp_port.lock()
    }

    /// Cache root the sidecar uses for HuggingFace model downloads.
    /// Refreshed on every `start_inner` so a `set_hf_home`-triggered
    /// restart updates the readiness scan target too.
    pub fn effective_hf_home(&self) -> PathBuf {
        self.effective_hf_home.lock().clone()
    }

    /// Name of the provider API-key env var the running jobworkerp child was
    /// spawned with (`OPENAI_API_KEY`, …), or `None` if none was injected.
    /// A Local→External hot-reload reads this to decide whether the child
    /// already carries the key the target provider needs — if not, it must
    /// restart (the genai key can't be pushed to a live child). `None` once
    /// the sidecars are stopped.
    pub fn spawned_external_key_env(&self) -> Option<String> {
        self.spawned_external_key_env.lock().clone()
    }

    /// Snapshot of the most recent successful start, or `None` before the
    /// sidecars have come up. Used by the `get_sidecar_status` command.
    pub fn last_report(&self) -> Option<SidecarStartReport> {
        self.last_report.lock().clone()
    }

    /// Message from the most recent hard startup failure, or `None` when the
    /// sidecars are starting / running. Used by `get_model_status` to drive
    /// the `failed` state when the sidecars never came up.
    pub fn last_start_error(&self) -> Option<String> {
        self.last_start_error.lock().clone()
    }

    /// Structured snapshot of the most recent hard startup failure, or
    /// `None` when the sidecars are starting / running cleanly. Same
    /// lifecycle as `last_start_error`; used by `get_sidecar_status` so
    /// a React tree that mounts *after* the one-shot `sidecar://error`
    /// event already fired can still drive the BootError recovery UI
    /// from the snapshot.
    pub fn last_start_failure(&self) -> Option<crate::sidecar::startup_error::SidecarErrorPayload> {
        self.last_start_failure.lock().clone()
    }

    /// Whether the local startup migration gate is currently checking or
    /// migrating the memories database.
    pub fn database_migration_in_progress(&self) -> bool {
        self.database_migration_in_progress.load(Ordering::Acquire)
    }

    fn database_migration_activity(&self) -> DatabaseMigrationActivity {
        DatabaseMigrationActivity::start(Arc::clone(&self.database_migration_in_progress))
    }

    fn database_migration_activity_if_enabled(
        &self,
        enabled: bool,
    ) -> Option<DatabaseMigrationActivity> {
        enabled.then(|| self.database_migration_activity())
    }

    /// The reason the LLM model can never finish preparing, or `None` if no
    /// such blocking condition is known. Folds the two failure sources
    /// (fatal first) so callers like `get_model_status` don't reach into the
    /// `Sidecars` internals to assemble the verdict:
    ///   1. a hard startup failure (`last_start_error`) — the sidecars never
    ///      came up; or
    ///   2. a `WorkerApplyFailed` / `PluginsStageFailed` warning — the
    ///      LLMPromptRunner couldn't be registered (e.g. a missing dylib).
    pub fn llm_blocking_error(&self) -> Option<String> {
        self.last_start_error().or_else(|| {
            self.last_report().and_then(|r| {
                r.warnings
                    .into_iter()
                    .find(|w| {
                        matches!(
                            w.kind,
                            SidecarWarningKind::WorkerApplyFailed
                                | SidecarWarningKind::PluginsStageFailed
                        )
                    })
                    .map(|w| w.message)
            })
        })
    }

    /// Bring sidecars up. Idempotent — if already running, returns the
    /// existing endpoints with no warnings (warnings are only collected
    /// on the first launch).
    pub async fn start(&self) -> AppResult<SidecarStartReport> {
        self.start_with_warnings(Vec::new()).await
    }

    /// Same as [`start`] but seeds the warnings vector. Used by
    /// `lib.rs::setup` to thread plugin-staging warnings through to the
    /// `sidecar://ready` event payload.
    ///
    /// Records the outcome in `last_start_error`: cleared on success, set to
    /// the error message on a hard failure (spawn / port / TCP timeout) so
    /// `get_model_status` can surface a `failed` state even though the
    /// `sidecar://error` event is one-shot.
    pub async fn start_with_warnings(
        &self,
        warnings: Vec<SidecarWarning>,
    ) -> AppResult<SidecarStartReport> {
        // Hold the lifecycle lock across the running-state check and every
        // startup side effect. Otherwise two callers can both pass the
        // idempotency check before either migration or spawn has published
        // state, executing the coordinator and children twice.
        let _lifecycle_guard = self.lifecycle_lock.lock().await;
        let was_running = self.current_endpoints().is_some();
        let result = self.start_inner(warnings).await;
        match &result {
            Ok(report) => {
                *self.last_start_error.lock() = None;
                *self.last_start_failure.lock() = None;
                if Self::should_spawn_startup_orphan_sweep(was_running, true) {
                    crate::maintenance::spawn_startup_orphan_sweep(
                        report.endpoints.jobworkerp_url(),
                    );
                }
            }
            Err(e) => {
                *self.last_start_error.lock() = Some(e.to_string());
                // Same lift the `sidecar://error` emit does, but kept
                // here so a snapshot request that wins the race against
                // the listener also gets the structured payload.
                *self.last_start_failure.lock() =
                    Some(crate::sidecar::startup_error::SidecarErrorPayload::from_app_error(e));
            }
        }
        result
    }

    fn should_spawn_startup_orphan_sweep(was_running: bool, start_succeeded: bool) -> bool {
        !was_running && start_succeeded
    }

    async fn start_inner(
        &self,
        mut warnings: Vec<SidecarWarning>,
    ) -> AppResult<SidecarStartReport> {
        if let Some(endpoints) = self.current_endpoints() {
            let report = SidecarStartReport {
                endpoints,
                warnings,
            };
            *self.last_report.lock() = Some(report.clone());
            return Ok(report);
        }

        // Clear any structured failure recorded by a previous attempt; we
        // do NOT want a stale `LancedbSchemaMismatch` to short-circuit
        // the new attempt's TCP wait. `start_inner` is the only writer
        // of `None` here (other writes come from `pipe_lines` on the
        // memories child's stdout).
        *self.startup_failure.lock() = None;
        // Clear any degraded flag from a previous start: a clean start (or
        // the restart the user triggers after fixing the embedding
        // dimension in Settings) must return to full functionality. Only
        // set again below if the memories child still can't open the
        // LanceDB at the configured dimension.
        *self.degraded.lock() = None;

        let connection = crate::commands::connection::load_connection_config(
            &self.config.data.connection_config_path(),
        );
        let remote_memories = matches!(
            connection.mode,
            crate::commands::connection::ConnectionMode::Remote
        );
        let remote_callback = if remote_memories {
            let target = crate::commands::connection::resolve_targets(&connection, None)?;
            crate::commands::connection::validate_remote_memories_services(&target.memories_url)
                .await?;
            Some(target.memories_callback()?)
        } else {
            None
        };
        // Reap any sidecars stranded by a previous crash (no PR_SET_PDEATHSIG
        // on macOS) BEFORE picking ports, so this launch can reclaim 9000/9010
        // instead of falling back to random ports behind the orphans. The reap
        // is gated on the per-instance advisory lock: if another live instance
        // holds it, `lock` comes back `None`.
        let pids_path = self.config.data.sidecar_pids_path();
        let lock_path = self.config.data.sidecar_lock_path();
        if self.instance_lock.lock().is_none() {
            let outcome = reaper::reap_recorded(&pids_path, &lock_path);

            // No lock means another live instance owns this data root. We must NOT
            // spawn a second set of sidecars or modify its database.
            let Some(lock) = outcome.lock else {
                return Err(AppError::AnotherInstanceRunning);
            };
            if outcome.reaped > 0 {
                info!(
                    reaped = outcome.reaped,
                    "reaped orphaned sidecars from a previous launch"
                );
            }
            *self.instance_lock.lock() = Some(lock);
        }

        // After reaping recorded children, reconcile only native scopes whose
        // recorded child PID is no longer alive. Unrecorded or live scopes
        // remain active so an orphan cannot be pruned while still writing.
        match crate::log_rotation::reconcile_native_log_dirs_after_reap(&self.config.data.log_dir())
        {
            Ok(reconciled) if reconciled > 0 => {
                info!(reconciled, "closed stale native log directories");
            }
            Ok(_) => {}
            Err(error) => warn!(%error, "could not reconcile stale native log directories"),
        }
        match crate::log_rotation::reconcile_migration_logs(&self.config.data.log_dir()) {
            Ok(reconciled) if reconciled > 0 => {
                info!(reconciled, "closed stale migration log markers");
            }
            Ok(_) => {}
            Err(error) => warn!(%error, "could not reconcile stale migration logs"),
        }

        if remote_memories {
            self.config.data.ensure_runtime()?;
        } else {
            let memory_db = self.config.data.memories_sqlite_path();
            match crate::sidecar::memory_kind_gate::inspect(&memory_db) {
                Ok(crate::sidecar::memory_kind_gate::GateState::Fresh)
                | Ok(crate::sidecar::memory_kind_gate::GateState::ContractReady) => {}
                Ok(crate::sidecar::memory_kind_gate::GateState::MigrationRequired) => {
                    return Err(AppError::MemoryKindMigrationRequired {
                        db_path: memory_db.display().to_string(),
                    });
                }
                Ok(crate::sidecar::memory_kind_gate::GateState::DatabaseSchemaInvalid) => {
                    return Err(AppError::MemoryKindDatabaseSchemaInvalid {
                        db_path: memory_db.display().to_string(),
                        reason: "memory and thread tables must both have the memory_kind column"
                            .into(),
                    });
                }
                Err(reason) => {
                    return Err(memory_kind_gate_error(&memory_db, reason));
                }
            }
            self.config.data.ensure()?;
        }

        if !remote_memories {
            let embedding_settings = crate::commands::embedding_settings::load_embedding_settings(
                &self.config.data.embedding_settings_path(),
            );
            let embedding_runtime =
                crate::commands::embedding_settings::resolve_embedding_runtime_with_env(
                    &embedding_settings,
                    crate::commands::process_env_lookup,
                );
            let migration_env = crate::sidecar::migration_gate::MigrationEnvironment {
                database_url: self.config.data.memories_sqlite_url(),
                thread_vector_enabled: self.config.auto_embedding_enabled,
                thread_lancedb_uri: self.config.data.lancedb_dir().join("memories.lancedb"),
                thread_lancedb_table: "threads".into(),
                thread_vector_size: embedding_runtime.vector_size,
                memory_fts_tokenizer: effective_memory_fts_tokenizer(
                    self.config.env_file.as_deref(),
                    self.config.lindera_dict_staged,
                    crate::commands::process_env_lookup,
                ),
                lance_language_model_home: self.config.lance_language_model_home.clone(),
            };
            let release = crate::sidecar::migration_gate::MigrationRelease {
                enabled: self.config.startup_database_migration_enabled,
                ..crate::sidecar::migration_gate::STARTUP_MIGRATION
            };
            // Keep this alive across every return from an enabled gate. Its
            // Drop implementation also clears the UI signal if startup is
            // aborted. Disabled releases still call the gate for its normal
            // no-op path, but must never claim migration is active.
            let _migration_activity = self.database_migration_activity_if_enabled(release.enabled);
            crate::sidecar::migration_gate::run_startup_gate(
                &self.config.data,
                &self.config.memories_db_migrate_bin,
                release,
                &migration_env,
            )
            .await?;
        }

        let jw_port = ports::pick(9000)?;
        let mem_port = (!remote_memories).then(|| ports::pick(9010)).transpose()?;
        let conductor_port = ports::pick(9020)?;
        // MCP server inside jobworkerp listens on 8000 by default. Prefer
        // `MCP_DEFAULT_PORT` (a private-range port chosen to avoid the 9000
        // sidecar trio, the 8000 mcp default, and common dev servers) so an
        // enabled MCP server has a predictable target an external client can
        // be configured against; `ports::pick` falls back to an OS-assigned
        // port if it is already taken.
        let mcp_server_port = ports::pick(crate::commands::mcp_settings::MCP_DEFAULT_PORT)?;
        // Evaluate once so every resident sidecar gets a stable value even
        // when LOOKBACK_RUST_LOG is mid-flight modified by a test harness.
        // The selected profile is info for dev builds and warn for release
        // bundles; an explicit LOOKBACK_RUST_LOG overrides it.
        let rust_log = effective_rust_log();

        info!(
            jobworkerp_port = jw_port,
            memories_port = ?mem_port,
            conductor_port,
            mcp_server_port,
            "starting sidecars"
        );

        // Load once and reuse for both jobworkerp's child env (provider
        // model / base_url / api key) and the Tauri process env that the
        // worker-YAML loader expands later in this function.
        let llm_settings =
            crate::commands::llm_settings::load_llm_settings(&self.config.data.llm_settings_path());

        // Re-resolve the Local-LLM triple from the JUST-LOADED settings
        // file rather than reaching into `self.config.llm_*` — that field
        // is frozen at app boot, so on a `set_llm_settings`-triggered
        // restart (which reuses the same `Sidecars` instance) the cached
        // triple still reflects the OLD preset. Re-resolving here is what
        // makes a Settings change take effect without a full app relaunch.
        let (llm_model, llm_hf_repo, llm_ctx_size) =
            crate::commands::llm_settings::resolve_local_llm_env_triple(
                &llm_settings,
                crate::commands::process_env_lookup,
            );
        let llm_kv_cache_type = crate::commands::llm_settings::resolve_kv_cache_type_with_env(
            &llm_settings,
            crate::commands::process_env_lookup,
        )
        .runner_value()
        .to_string();
        let (llm_mtp_enabled, llm_mtp_draft_model) =
            crate::commands::llm_settings::resolve_local_llm_mtp_env(
                &llm_settings,
                crate::commands::process_env_lookup,
            );

        // Same pattern for the embedding model: re-resolve from the JUST-
        // LOADED `embedding-settings.json` so a `set_embedding_settings`
        // restart picks up the new preset / custom values without a full
        // app relaunch. Stage the worker YAML against the new runtime —
        // the `tokenizer_model_id:` line is conditionally included, which
        // `expand_env` can't express. The staged file replaces the
        // committed bundle's auto-embedding-workers.yaml for
        // `MEMORY_WORKERS_YAML`.
        let embedding_settings = crate::commands::embedding_settings::load_embedding_settings(
            &self.config.data.embedding_settings_path(),
        );
        // Resolve runtime + env vars from the SAME env-aware projection so
        // a dev embedding env override flows into all three
        // consumers consistently: the staged YAML, `MEMORY_VECTOR_SIZE` on
        // the memories child, and the process env that the YAML loader's
        // `expand_env` reads. Without this, env overrides reached
        // `apply_embedding_env` (Tauri process env) but the staged YAML
        // and `MEMORY_VECTOR_SIZE` still carried the file-only defaults,
        // so the sidecar opened LanceDB at the wrong dim.
        let embedding_runtime =
            crate::commands::embedding_settings::resolve_embedding_runtime_with_env(
                &embedding_settings,
                crate::commands::process_env_lookup,
            );
        // Derive env vars from the already-resolved runtime so the
        // second `resolve_embedding_runtime_with_env` that the
        // settings-shaped overload would do is avoided.
        let embedding_env_vars =
            crate::commands::embedding_settings::resolve_embedding_env_vars_from_runtime(
                &embedding_runtime,
            );
        let staged_embedding_workers_yaml =
            crate::commands::embedding_workers_yaml::stage_auto_embedding_workers_yaml(
                &embedding_runtime,
                &self.config.data,
                self.config.workflows_dir.as_deref(),
            )
            .map_err(|e| {
                tracing::warn!(error = %e, "stage_auto_embedding_workers_yaml failed; falling back to bundled YAML");
                e
            })
            .ok();

        // Same pattern for HF_HOME: re-resolve from the JUST-LOADED
        // `app-settings.json` so a `set_hf_home`-triggered restart picks
        // up the new mode. Stored in the `Mutex<PathBuf>` so the
        // `get_model_status` readiness scan reads the same value as the
        // spawn-time injection below.
        *self.effective_hf_home.lock() = resolve_effective_hf_home(&self.config);

        // Same pattern for MCP: re-resolve from the JUST-LOADED
        // `mcp-settings.json` so a `set_mcp_settings`-triggered restart picks
        // up the new enable flag. `MCP_ENABLED` is read at jobworkerp spawn
        // time, so this is the only point the toggle takes effect. The live
        // bound port is recorded only AFTER every child has started (near the
        // end of this fn) — recording it any earlier would leave
        // `get_mcp_settings` advertising an MCP URL even though a later
        // `spawn_*` `?`-returned (binary missing / perms) without running
        // `stop_blocking` to clear it.
        let mcp_settings =
            crate::commands::mcp_settings::load_mcp_settings(&self.config.data.mcp_settings_path());
        let mcp_runtime = crate::commands::mcp_settings::resolve_mcp_runtime(&mcp_settings);

        let jw_child = self.spawn_jobworkerp(
            jw_port,
            mcp_server_port,
            &mcp_runtime,
            &rust_log,
            &llm_settings,
            llm_model.as_deref(),
            llm_hf_repo.as_deref(),
            llm_ctx_size,
            Some(llm_kv_cache_type.as_str()),
            llm_mtp_enabled,
            llm_mtp_draft_model.as_deref(),
        )?;
        let mut processes = vec![Process {
            name: "jobworkerp",
            child: jw_child,
            port: jw_port,
        }];

        // Stash early so we drop processes even if health check fails.
        *self.state.lock() = Some(std::mem::take(&mut processes));

        if let Err(e) =
            wait_for_tcp_or_failure(("127.0.0.1", jw_port), Duration::from_secs(30), None).await
        {
            self.stop_blocking();
            return Err(e);
        }

        let memories_callback =
            remote_callback.unwrap_or_else(|| crate::commands::connection::MemoriesCallback {
                host: "127.0.0.1".to_string(),
                port: mem_port.expect("local mode always picks a memories port"),
                tls: false,
            });

        // Publish the active memories endpoint into THIS process's
        // env so the worker-YAML loader (which expands `%{...}` via
        // `std::env::var` on the calling process) can substitute it into
        // the `auto-embedding-workers.yaml` family's `%{MEMORY_GRPC_HOST}`
        // / `%{MEMORY_GRPC_PORT}` placeholders. `memories_callback` follows
        // the active connection target, so in remote mode this points
        // embedding upserts at the remote memories endpoint too.
        //
        // NOTE: `MEMORY_GRPC_*` here is the embedding-upsert callback, not
        // RAG retrieval. The chat path injects `memories_grpc_{host,port,tls}`
        // for `lookback_recall` per-dispatch (chat.rs); the MCP path can't, so
        // those are resolved into `LOOKBACK_RAG_MEMORIES_*` below and consumed
        // as the workflow-input defaults — see `apply_rag_memories_env`.
        //
        // The same plumbing is required for the External LLM placeholders
        // in `llm-workers.yaml` (`%{LOOKBACK_EXTERNAL_LLM_MODEL}` /
        // `%{LOOKBACK_EXTERNAL_LLM_BASE_URL}`): without setting them on the
        // Tauri process env here, the YAML expansion always falls back to
        // the literal default (`gpt-4o`) and a user-selected provider model
        // never reaches the registered `memories-llm-external` worker —
        // chat then hits OpenAI regardless of what Settings shows. None /
        // empty values clear the env so a sidecar restart after switching
        // back to Local mode doesn't leave a stale model name behind.
        //
        // SAFETY: `set_var` is single-threaded-safe by Rust's contract;
        // we're inside `Sidecars::start` which runs once at app boot
        // before any worker threads have touched the env. Existing
        // sidecar children already inherited their own envs at
        // `spawn_jobworkerp` time (line above). The post-boot
        // `set_llm_settings` path also routes through here (stop →
        // stage_and_start) while sidecars are down, so the same
        // single-thread invariant holds.
        unsafe {
            std::env::set_var("MEMORY_GRPC_HOST", &memories_callback.host);
            std::env::set_var("MEMORY_GRPC_PORT", memories_callback.port.to_string());
            std::env::set_var(
                "MEMORY_GRPC_TLS",
                if memories_callback.tls {
                    "true"
                } else {
                    "false"
                },
            );
            // RAG retrieval endpoint for `lookback-recall.yaml`'s
            // `memories_grpc_*` input DEFAULTS. The chat path injects these
            // per-dispatch (chat.rs) so the defaults never apply there; the
            // MCP path runs the workflow directly with no Tauri in the loop,
            // so these resolved defaults are the only endpoint it gets.
            // Resolve from the SAME active connection the chat / browse
            // clients use: local → the live `mem_port`; remote → the
            // configured memories URL (host/port/tls), so MCP search in
            // remote mode targets the configured remote memories rather than
            // the empty local sidecar. Cleared-to-default on a parse failure
            // so a malformed remote URL can't wedge worker registration.
            apply_rag_memories_env(&self.config.data, memories_callback.port);
            apply_external_llm_env(
                llm_settings.provider_model.as_deref(),
                llm_settings.base_url.as_deref(),
            );
            // Same reason as the External-LLM block above, for the Local
            // (llama-cpp) placeholders in `llm-workers.yaml`. The Tauri
            // process runs `expand_env` against ITS OWN env when applying
            // the worker YAMLs below; without mirroring here the
            // `%{LOOKBACK_LLM_MODEL:-gemma-4-E2B-…}` placeholder always
            // falls back to its YAML default and the user-selected preset
            // never reaches the registered `memories-llm` worker — chat
            // then runs against the YAML fallback regardless of what Settings
            // shows.
            apply_local_llm_env(
                llm_model.as_deref(),
                llm_hf_repo.as_deref(),
                llm_ctx_size,
                Some(llm_kv_cache_type.as_str()),
                llm_mtp_enabled,
                llm_mtp_draft_model.as_deref(),
            );
            // Mirror the embedding runtime into the process env so the
            // bundled (non-staged) `auto-embedding-workers.yaml`'s `%{...}`
            // placeholders also resolve. The staged path injects the values
            // verbatim into the YAML file the loader reads, so this is the
            // back-compat / fallback path.
            apply_embedding_env(&embedding_env_vars);
        }

        // Apply named workers before memories spawns so its first
        // workflow dispatch can resolve `workerName: memories-llm`.
        // Non-fatal: memories can still boot; failures surface as
        // `WorkerApplyFailed` warnings so the UI can disable LLM-tagged
        // flows instead of letting the user wait for jobs that will
        // forever return `WorkerNotFound`.
        if !self.config.worker_yaml_paths.is_empty()
            || !self.config.function_set_yaml_paths.is_empty()
        {
            let server_url = format!("http://127.0.0.1:{jw_port}");
            match crate::jobworkerp::JobworkerpHandle::connect(&server_url).await {
                Ok(handle) => {
                    for yaml in &self.config.worker_yaml_paths {
                        match handle.register_workers_from_yaml(yaml).await {
                            Ok(map) => info!(
                                yaml = %yaml.display(),
                                count = map.len(),
                                "workers registered",
                            ),
                            Err(e) => {
                                warn!(
                                    yaml = %yaml.display(),
                                    error = %e,
                                    "worker registration failed (continuing)",
                                );
                                warnings.push(SidecarWarning {
                                    kind: SidecarWarningKind::WorkerApplyFailed,
                                    message: e.to_string(),
                                    detail: Some(yaml.display().to_string()),
                                });
                            }
                        }
                    }
                    // Function sets resolve target worker names against
                    // the live registry, so they MUST apply after the
                    // worker YAMLs in the same startup.
                    for yaml in &self.config.function_set_yaml_paths {
                        match handle.register_function_sets_from_yaml(yaml).await {
                            Ok(map) => info!(
                                yaml = %yaml.display(),
                                count = map.len(),
                                "function sets registered",
                            ),
                            Err(e) => {
                                warn!(
                                    yaml = %yaml.display(),
                                    error = %e,
                                    "function set registration failed (continuing)",
                                );
                                warnings.push(SidecarWarning {
                                    kind: SidecarWarningKind::WorkerApplyFailed,
                                    message: e.to_string(),
                                    detail: Some(yaml.display().to_string()),
                                });
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "could not connect for worker registration");
                    warnings.push(SidecarWarning {
                        kind: SidecarWarningKind::WorkerApplyFailed,
                        message: e.to_string(),
                        detail: None,
                    });
                }
            }
        }

        // Register the language-specific generation workers
        // (`memories-<feature>-single-<lang>`) BEFORE memories spawns: the
        // batches resolve them by name on their first dispatch. jobworkerp is
        // already accepting connections (TCP wait above) and the named
        // `memories-llm` worker is registered, so the upsert can connect and
        // the single workflows can route their LLM step. Fail-soft: a failure
        // is a `WorkerApplyFailed` warning, not a startup abort.
        crate::sidecar::generation_workers::register_generation_workers(
            jw_port,
            &self.config.data.log_dir(),
            &mut warnings,
        )
        .await;

        if let Some(mem_port) = mem_port {
            self.spawn_and_register_memories(
                mem_port,
                jw_port,
                &rust_log,
                &embedding_runtime,
                staged_embedding_workers_yaml.as_deref(),
                None,
            )?;

            if let Err(e) = wait_for_tcp_or_failure(
                ("127.0.0.1", mem_port),
                MEMORIES_START_TIMEOUT,
                Some(&self.startup_failure),
            )
            .await
            {
                // A dimension-mismatch on the local LanceDB (the memories child
                // `exit(1)`s via `StartupError::*Mismatch::fatal()`) is NOT
                // fatal to the whole app: retry the memories child once with the
                // vector store disabled (degraded mode) so browse / FTS keep
                // working and only embedding-dependent features are gated. Any
                // other startup failure stays fatal → BootError.
                let Some(info) = plan_degraded_retry(&e) else {
                    self.stop_blocking();
                    return Err(e);
                };
                warn!(
                    expected_dim = info.expected_dim,
                    actual_dim = info.actual_dim,
                    reason = info.reason,
                    "memories vector dimension mismatch; restarting with vector store disabled (degraded)"
                );
                // Kill the dead memories child (jobworkerp stays up — its workers
                // are already registered) and clear the structured-failure slot:
                // otherwise the re-spawn's TCP wait would short-circuit on the
                // stale failure.
                self.kill_memories_child().await;
                *self.startup_failure.lock() = None;

                self.spawn_and_register_memories(
                    mem_port,
                    jw_port,
                    &rust_log,
                    &embedding_runtime,
                    staged_embedding_workers_yaml.as_deref(),
                    Some(false),
                )?;

                // Second wait: with vector disabled the LanceDB is never opened,
                // so a mismatch can't recur. A failure here is a genuine problem
                // → fatal.
                if let Err(e2) = wait_for_tcp_or_failure(
                    ("127.0.0.1", mem_port),
                    MEMORIES_START_TIMEOUT,
                    Some(&self.startup_failure),
                )
                .await
                {
                    self.stop_blocking();
                    return Err(e2);
                }

                *self.degraded.lock() = Some(info.clone());
                warnings.push(info.into_warning());
            }

            if let Err(error) = crate::commands::connection::validate_local_memories_services(
                &format!("http://127.0.0.1:{mem_port}"),
            )
            .await
            {
                self.stop_blocking();
                return Err(error);
            }
        }

        let conductor_child = self.spawn_conductor(conductor_port, &rust_log)?;
        {
            let mut guard = self.state.lock();
            if let Some(procs) = guard.as_mut() {
                procs.push(Process {
                    name: "conductor",
                    child: conductor_child,
                    port: conductor_port,
                });
            }
        }

        if let Err(e) =
            wait_for_tcp_or_failure(("127.0.0.1", conductor_port), Duration::from_secs(30), None)
                .await
        {
            self.stop_blocking();
            return Err(e);
        }

        let llm_worker_name = crate::commands::llm_settings::worker_name_for(llm_settings.mode);
        // The conductor wrapper bakes the output language into each scheduler's
        // args at refresh time (it never goes through the Rust dispatch
        // builders), so resolve it from the persisted setting the frontend keeps
        // in sync with the UI locale.
        let periodic_output_language = crate::commands::resolve_periodic_output_language(
            &crate::data::paths::load_app_settings(&self.config.data.app_settings_path()),
        );
        let memories_import_bin = crate::resolve_memories_import_bin_path()
            .unwrap_or_else(|_| std::path::PathBuf::from("memories-import"));
        let periodic_refresh = crate::commands::periodic_tasks::refresh_lookback_periodic_runtime(
            &format!("http://127.0.0.1:{conductor_port}"),
            jw_port,
            &memories_callback,
            llm_worker_name,
            &periodic_output_language,
            &memories_import_bin,
            &self.config.data.periodic_defaults_seed_path(),
        )
        .await;
        if let Err(e) = periodic_refresh {
            warn!(error = %e, "periodic scheduler runtime refresh failed (continuing)");
            warnings.push(SidecarWarning {
                kind: SidecarWarningKind::WorkerApplyFailed,
                message: format!("定期実行の runtime 更新に失敗しました: {e}"),
                detail: None,
            });
        }

        // Record the live PIDs so a crash of this host (which skips Drop /
        // kill_on_drop) is recoverable on the next launch. Best-effort: a
        // write failure only loses crash-recovery for this session.
        self.record_pids(&pids_path);

        // Advertise the MCP bound port only now that the whole trio is up: a
        // `spawn_memories` / `spawn_conductor` failure `?`-returns WITHOUT
        // `stop_blocking`, so recording it earlier would strand a phantom URL
        // that `get_mcp_settings` keeps advertising past a failed start. The
        // `active_port` contract is "sidecars are up"; `stop` / `stop_blocking`
        // clear it.
        //
        // The MCP server binds a SEPARATE socket inside the jobworkerp process
        // (`tokio::join!` with the gRPC front), so the gRPC check above does
        // NOT prove the MCP port bound (a `ports::pick` clash or MCP startup
        // error could leave gRPC up but MCP dead). Confirm it before publishing;
        // a failure is fatal since the user explicitly enabled MCP.
        if mcp_runtime.enabled
            && let Err(e) =
                wait_for_tcp_or_failure(("127.0.0.1", mcp_server_port), MCP_HEALTH_TIMEOUT, None)
                    .await
        {
            self.stop_blocking();
            return Err(e);
        }
        *self.active_mcp_port.lock() = mcp_runtime.enabled.then_some(mcp_server_port);

        info!(warning_count = warnings.len(), "sidecars ready");
        let report = SidecarStartReport {
            endpoints: SidecarEndpoints {
                jobworkerp_port: jw_port,
                memories_port: mem_port,
                conductor_port,
                // Just stashed above (only when MCP is enabled). Read it back
                // rather than recomputing so the report and `get_mcp_settings`
                // always agree on the live port.
                mcp_server_port: *self.active_mcp_port.lock(),
            },
            warnings,
        };
        *self.last_report.lock() = Some(report.clone());
        Ok(report)
    }

    pub fn current_endpoints(&self) -> Option<SidecarEndpoints> {
        let guard = self.state.lock();
        let procs = guard.as_ref()?;
        let jw = procs.iter().find(|p| p.name == "jobworkerp")?.port;
        let mem = procs.iter().find(|p| p.name == "memories").map(|p| p.port);
        let conductor = procs.iter().find(|p| p.name == "conductor")?.port;
        Some(SidecarEndpoints {
            jobworkerp_port: jw,
            memories_port: mem,
            conductor_port: conductor,
            mcp_server_port: *self.active_mcp_port.lock(),
        })
    }

    pub async fn stop(&self) -> AppResult<()> {
        self.stop_inner(true).await
    }

    /// Stop every child while retaining the data-root lock for an immediate
    /// in-process maintenance action followed by `start_inner`.
    #[allow(dead_code)]
    pub(crate) async fn stop_for_maintenance(&self) -> AppResult<()> {
        self.stop_inner(false).await
    }

    /// Stop the sidecars for an exclusive maintenance operation and keep the
    /// lifecycle lock held until that operation completes. Database restore
    /// must not leave a window where a concurrent start can reopen the live
    /// database before the restored files are installed.
    pub(crate) async fn with_maintenance_lock<F, Fut, T>(&self, operation: F) -> AppResult<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = AppResult<T>>,
    {
        let _lifecycle_guard = self.lifecycle_lock.lock().await;
        if !self.owns_instance_lock() {
            return Err(AppError::AnotherInstanceRunning);
        }
        self.stop_inner_locked(false).await?;
        operation().await
    }

    async fn stop_inner(&self, release_lock: bool) -> AppResult<()> {
        // Startup may hold the instance lock before child state is published
        // (for example while the migration coordinator is running). Serialize
        // stop with startup so the empty-state branch below cannot release
        // that lock while the startup operation is still in flight.
        let _lifecycle_guard = self.lifecycle_lock.lock().await;
        self.stop_inner_locked(release_lock).await
    }

    async fn stop_inner_locked(&self, release_lock: bool) -> AppResult<()> {
        // Invalidate the snapshot in lockstep with the process list, so a
        // `get_sidecar_status` after stop (or after purge_all_data) can't
        // hand the frontend stale endpoints and promote it back to ready.
        *self.last_report.lock() = None;
        *self.last_start_error.lock() = None;
        *self.last_start_failure.lock() = None;
        // No live child ⇒ no injected key env. A subsequent hot-reload gate
        // must not think a stopped child still carries a provider key.
        *self.spawned_external_key_env.lock() = None;
        // No live child ⇒ no MCP server listening; `get_mcp_settings` must
        // report `active_port: None` after a stop.
        *self.active_mcp_port.lock() = None;
        // No live child ⇒ the degraded gate must not keep refusing
        // dispatches after a stop / purge.
        *self.degraded.lock() = None;
        let Some(mut procs) = self.state.lock().take() else {
            if self.join_output_tasks().await {
                self.close_native_log_dirs(None);
            } else {
                warn!("sidecar output pumps did not finish; leaving native logs active");
            }
            if release_lock {
                self.release_instance_lock();
            } else {
                reaper::clear_pids(&self.config.data.sidecar_pids_path());
            }
            return Ok(());
        };
        // Stop the children FIRST, then release the lock + PID record. Doing it
        // in this order is load-bearing: the instance lock means "this data
        // root's sidecars are alive", so releasing it before the processes are
        // actually dead would let a concurrent launch take the lock (and find
        // no PID file to reap) during the SIGTERM grace window — two instances
        // would then run against the same DB / LanceDB. See `release_instance_lock`.
        for proc in procs.iter_mut() {
            stop_child(&mut proc.child, proc.name).await;
        }
        if self.join_output_tasks().await {
            self.close_native_log_dirs(None);
        } else {
            warn!("sidecar output pumps did not finish; leaving native logs active");
        }
        if release_lock {
            self.release_instance_lock();
        } else {
            reaper::clear_pids(&self.config.data.sidecar_pids_path());
        }
        Ok(())
    }

    /// SIGKILL and remove only the `memories` child from the process list,
    /// leaving jobworkerp (and its already-registered workers) running.
    /// Used by the degraded-restart path so a dimension-mismatched memories
    /// child can be replaced without tearing the whole sidecar set down.
    /// Waits for the child and its output pumps so native logs are marked
    /// closed only after their final bytes have been drained.
    async fn kill_memories_child(&self) {
        let child = self.state.lock().as_mut().and_then(|procs| {
            procs
                .iter()
                .position(|proc| proc.name == "memories")
                .map(|index| procs.remove(index).child)
        });
        if let Some(mut child) = child {
            stop_child(&mut child, "memories").await;
        }
        if self.join_output_tasks_for(Some("memories")).await {
            self.close_native_log_dirs(Some("memories"));
        } else {
            warn!("memories output pumps did not finish; leaving native logs active");
        }
    }

    async fn join_output_tasks(&self) -> bool {
        self.join_output_tasks_for(None).await
    }

    async fn join_output_tasks_for(&self, scope_filter: Option<&str>) -> bool {
        let tasks = std::mem::take(&mut *self.output_tasks.lock());
        let mut selected = Vec::new();
        let mut keep = Vec::new();
        for (name, task) in tasks {
            if scope_filter.is_none_or(|filter| name.contains(filter)) {
                selected.push((name, task));
            } else {
                keep.push((name, task));
            }
        }
        *self.output_tasks.lock() = keep;
        let mut complete = true;
        for (_, mut task) in selected {
            match timeout(Duration::from_secs(5), &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    complete = false;
                    warn!(?error, "sidecar output pump failed while stopping");
                }
                Err(_) => {
                    task.abort();
                    let _ = task.await;
                    warn!("sidecar output pump timed out; aborted and joined");
                }
            }
        }
        complete
    }

    fn close_native_log_dirs(&self, scope_filter: Option<&str>) {
        let mut dirs = self.native_log_dirs.lock();
        let mut keep = Vec::with_capacity(dirs.len());
        for (scope, path) in dirs.drain(..) {
            if scope_filter.is_none_or(|filter| scope.contains(filter)) {
                let _ = crate::log_rotation::mark_native_log_dir_closed(&path);
            } else {
                keep.push((scope, path));
            }
        }
        *dirs = keep;
    }

    /// Attach the actual child PID to the newest active native-log scope for
    /// this executable. The scope is allocated before spawn so it can be
    /// passed in the child's environment; only a successful spawn can provide
    /// the PID used by crash reconciliation.
    fn mark_native_log_dir_pid(&self, scope: &str, pid: u32) {
        let path = self
            .native_log_dirs
            .lock()
            .iter()
            .rev()
            .find(|(candidate, path)| candidate == scope && path.join(".active").is_file())
            .map(|(_, path)| path.clone());
        if let Some(path) = path
            && let Err(error) = crate::log_rotation::mark_native_log_dir_pid(&path, pid)
        {
            warn!(%error, path = %path.display(), pid, "could not record native child PID");
        }
    }

    /// Drop the crash-recovery PID record and the per-instance advisory lock.
    /// MUST be called only after the child processes are confirmed stopped:
    /// the lock advertises "sidecars alive on this data root", so releasing it
    /// while they're still dying opens a window for a second instance to start
    /// against the same DB. The children are gone here so they're not orphans,
    /// which is why clearing the PID file (rather than leaving it for reaping)
    /// is correct.
    fn release_instance_lock(&self) {
        reaper::clear_pids(&self.config.data.sidecar_pids_path());
        *self.instance_lock.lock() = None;
    }

    /// Snapshot the running children's `(pid, live-exe)` to the PID file so a
    /// crashed launch is reapable next time. The exe is read back from `ps`
    /// (via `reaper::live_exe`) rather than taken from `SidecarConfig` so it
    /// matches what reaping will compare against. A child with no pid (already
    /// exited) or no resolvable exe is skipped — it can't be reaped anyway.
    fn record_pids(&self, pids_path: &std::path::Path) {
        let guard = self.state.lock();
        let Some(procs) = guard.as_ref() else {
            return;
        };
        let entries: Vec<PidEntry> = procs
            .iter()
            .filter_map(|p| {
                let pid = p.child.id()?;
                let exe = reaper::live_exe(pid)?;
                Some(PidEntry { pid, exe })
            })
            .collect();
        if let Err(e) = reaper::write_pids(pids_path, &entries) {
            warn!(error = ?e, "failed to record sidecar pids (crash recovery disabled this session)");
        }
    }

    /// Used by Drop. Doesn't await — best-effort kill from sync context.
    fn stop_blocking(&self) {
        *self.last_report.lock() = None;
        *self.last_start_error.lock() = None;
        *self.last_start_failure.lock() = None;
        // No live child ⇒ no injected key env. A subsequent hot-reload gate
        // must not think a stopped child still carries a provider key.
        *self.spawned_external_key_env.lock() = None;
        // No live child ⇒ no MCP server listening; mirror `stop`.
        *self.active_mcp_port.lock() = None;
        // Mirror `stop`: clear the degraded gate.
        *self.degraded.lock() = None;
        let Some(mut procs) = self.state.lock().take() else {
            self.release_instance_lock();
            return;
        };
        // SIGKILL the children before releasing the lock + PID record, mirroring
        // `stop`'s ordering: the lock must outlive the live processes so no
        // concurrent launch starts against the same data root while they exit.
        for proc in procs.iter_mut() {
            // Tokio's Child::start_kill is sync and just sends SIGKILL on Unix.
            let _ = proc.child.start_kill();
        }
        for (_, task) in self.output_tasks.lock().drain(..) {
            task.abort();
        }
        // SIGKILL + task abort does not prove that the final native bytes were
        // drained; leave `.active` markers for next-launch reconciliation.
        self.release_instance_lock();
    }

    /// Apply our optional `env_file` to `cmd`. Entries already present in
    /// the process env are NOT overridden (dotenvy semantics), and our
    /// later `.env(...)` calls on the same builder always win — matching
    /// the precedence operators expect from a `.env` template.
    fn apply_env_file(&self, cmd: &mut Command) {
        let Some(path) = self.config.env_file.as_ref() else {
            return;
        };
        match dotenvy::from_path_iter(path) {
            Ok(iter) => {
                for entry in iter {
                    match entry {
                        Ok((k, v)) => {
                            cmd.env(k, v);
                        }
                        Err(e) => warn!(?path, error = ?e, "env file parse error"),
                    }
                }
            }
            Err(e) => warn!(?path, error = ?e, "env file not readable"),
        }
    }

    /// Bring up a sidecar Command with the bits that are identical between
    /// jobworkerp and memories: env_file layering, RUST_LOG, cwd, piped IO,
    /// kill_on_drop. Callers layer their own sidecar-specific env on top.
    fn base_cmd(&self, bin: &std::path::Path, cwd: &std::path::Path, rust_log: &str) -> Command {
        let mut cmd = Command::new(bin);
        self.apply_env_file(&mut cmd);
        let native_scope = bin
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("sidecar")
            .to_string();
        let native_log_dir =
            crate::log_rotation::native_log_dir(&self.config.data.log_dir(), &native_scope)
                .inspect(|path| {
                    self.native_log_dirs
                        .lock()
                        .push((native_scope.clone(), path.clone()));
                })
                .unwrap_or_else(|_| self.config.data.log_dir());
        cmd.current_dir(cwd)
            .env("RUST_LOG", rust_log)
            .env("LOG_APP_NAME", "Lookback")
            .env("LOG_FILE_DIR", native_log_dir)
            .env("LOG_USE_JSON", "true")
            .env("LOG_USE_STDOUT", "true")
            .env("USE_GRPC_WEB", "true") // for debug ui
            .env("JOB_STATUS_RDB_INDEXING", "true") // for debug ui
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        self.apply_macos_dyld_path(&mut cmd);
        cmd
    }

    /// macOS-only: make the signed bundled plugin dir a dynamic-library search path
    /// for the sidecar children.
    ///
    /// Plugins like `libmm_embedding_runner.dylib` may bundle sibling dylibs
    /// alongside themselves and link them via `@rpath/...` (or a bare basename)
    /// expecting them to be found in the directory the plugin itself lives in.
    /// `dlopen` won't search that directory by default — the loader needs
    /// `DYLD_LIBRARY_PATH` to discover the siblings. This mirrors the
    /// `DYLD_LIBRARY_PATH=plugins/` invocation used to run the bare
    /// `all-in-one` binary in the parent workspace; codifying it here means
    /// the GUI launch (which can't rely on a shell-set env) gets the same
    /// resolution behaviour. No-op on Linux / Windows.
    #[cfg(target_os = "macos")]
    fn apply_macos_dyld_path(&self, cmd: &mut Command) {
        let plugins_dir = self.plugin_runner_dir();
        let plugins_str = plugins_dir.to_string_lossy().into_owned();
        let value = match std::env::var_os("DYLD_LIBRARY_PATH") {
            Some(existing) if !existing.is_empty() => {
                let mut combined = std::ffi::OsString::from(&plugins_str);
                combined.push(":");
                combined.push(&existing);
                combined
            }
            _ => std::ffi::OsString::from(plugins_str),
        };
        cmd.env("DYLD_LIBRARY_PATH", value);
    }

    #[cfg(not(target_os = "macos"))]
    fn apply_macos_dyld_path(&self, _cmd: &mut Command) {}

    // Split into a struct just to silence clippy::too_many_arguments would
    // hide a load-bearing distinction: the LLM triple is what makes a
    // `set_llm_settings` restart actually swap the model (the parent
    // `LlmSettings` covers the External path; the Local placeholders need
    // the explicit triple — see `start_inner` where these are re-resolved
    // from the just-loaded file). Allow the arity here.
    #[allow(clippy::too_many_arguments)]
    fn spawn_jobworkerp(
        &self,
        port: u16,
        mcp_server_port: u16,
        mcp_runtime: &crate::commands::mcp_settings::McpRuntime,
        rust_log: &str,
        llm_settings: &crate::commands::llm_settings::LlmSettings,
        llm_model: Option<&str>,
        llm_hf_repo: Option<&str>,
        llm_ctx_size: Option<u32>,
        llm_kv_cache_type: Option<&str>,
        llm_mtp_enabled: Option<bool>,
        llm_mtp_draft_model: Option<&str>,
    ) -> AppResult<Child> {
        // cwd is pinned to the per-app data dir so `dotenvy::dotenv()` in the
        // child cannot read a surprise `.env` from the parent workspace
        // (which would silently override GRPC_ADDR back to 0.0.0.0:9000).
        let mut cmd = self.base_cmd(
            &self.config.jobworkerp_bin,
            &self.config.data.root,
            rust_log,
        );
        cmd.env("STORAGE_TYPE", "Standalone")
            .env("STORAGE_RESTORE_AT_STARTUP", "false")
            .env("GRPC_ADDR", format!("127.0.0.1:{port}"))
            .env("SQLITE_URL", self.config.data.sqlite_url())
            .env("PLUGINS_RUNNER_DIR", self.plugin_runner_dir());

        // MCP server env. When disabled this is just
        // `MCP_ENABLED=false` (the long-standing default — jobworkerp boots
        // gRPC-only). When enabled it adds `MCP_SET_NAME=lookback-mcp-rag`
        // plus any advanced overrides the user set. `MCP_ADDR` is attached
        // separately below since the bound port is decided at spawn time.
        //
        // CLEAR every MCP-managed key from the inherited env FIRST: the child
        // inherits the parent process env and the `.env` template (applied in
        // `base_cmd`), so a stray `MCP_STREAMING` / `MCP_TIMEOUT_SEC` /
        // `MCP_EXCLUDE_*` there would otherwise survive for any field the user left
        // unset — defeating the "unset ⇒ jobworkerp default" the settings file
        // expresses, and bypassing the card's validation. `mcp_env_vars`
        // re-sets only the keys the user actually configured.
        for key in crate::commands::mcp_settings::MCP_MANAGED_ENV_KEYS {
            cmd.env_remove(key);
        }
        for (key, value) in crate::commands::mcp_settings::mcp_env_vars(mcp_runtime) {
            cmd.env(key, value);
        }

        for (key, value) in jobworkerp_channel_env_vars() {
            cmd.env(key, value);
        }
        cmd
            // Channels in a strict parent-→-child hierarchy:
            //   llm                (1) — LLMPromptRunner jobs (GPU bound).
            //   llm_workflow       (1) — single workflows whose body
            //                            contains an LLM step.
            //   llm_batch          (2) — batch workflows that fan out into
            //                            many `llm_workflow` jobs.
            //   llm_pipeline       (1) — summaries-pipeline parent, on its own
            //                            channel so it never competes for the
            //                            `llm_batch` slots its children need.
            //   embedding          (1) — MultimodalEmbeddingRunner (GPU
            //                            bound; shared by memory / thread /
            //                            reflection-intent embedding).
            //   embedding_workflow (1) — auto-embedding / reflection-intent
            //                            workflows that fan out to the
            //                            `embedding` channel; serialized so
            //                            the GPU queue depth stays bounded.
            //   rag                (2) — RAG retrieval tools (lookback_recall),
            //                            non-GPU gRPC searches that can run
            //                            in parallel while the `llm` slot is
            //                            held by the chat LLM. Kept off the
            //                            `llm` channel so the chat LLM never
            //                            deadlocks waiting for its own tool
            //                            (DECIDE-CHAT-3).
            // Concurrency MUST strictly increase from child to parent for
            // each hierarchy; the embedding pair mirrors the llm pair.
            // See workers/README.md for the full rationale.
            // MCP Server listens on 127.0.0.1:8000 by default and would clash if
            // the user already runs a jobworkerp dev server. We pin it to the
            // preferred `MCP_DEFAULT_PORT` (or an OS-picked fallback) so an
            // enabled MCP server has a predictable target for external clients.
            // Always set even when MCP is disabled — harmless then, and it keeps
            // the bound port stable across an enable toggle.
            .env("MCP_ADDR", format!("127.0.0.1:{mcp_server_port}"));

        // Timezone for the agent-chat summary / import workflows. Their jq
        // reads `env.TZ` to make the day/week/month boundary DST-aware
        // (memories 5e996f5); when unset it falls back to the fixed
        // `timezone_offset_hours` input. A DMG/GUI launch inherits an
        // essentially empty env, so `TZ` is resolved and injected explicitly
        // here rather than relying on an inherited shell value — otherwise the
        // DST-aware path never activates. `resolve_timezone` prefers the
        // user's `app-settings.json` selection (re-read on every restart, like
        // HF_HOME), then the env, then the OS zone, then `Asia/Tokyo` — which
        // matches the workflows' `timezone_offset_hours` JST default, so the
        // boundary is identical whether or not any of those was set.
        let app_settings =
            crate::data::paths::load_app_settings(&self.config.data.app_settings_path());
        cmd.env("TZ", resolve_timezone(Some(&app_settings)));

        // Hugging Face cache root. Re-resolved on every `start_inner`
        // (app-settings.json → shell env → `.env` template →
        // `<data>/models` fallback) and forwarded verbatim — a user who
        // maintains a shared HF cache keeps their existing GGUFs, while
        // a fresh install lands files under the app data root. The
        // same value is surfaced to `get_model_status` so the readiness
        // scan walks the exact directory the sidecar is populating
        // readiness scan.
        cmd.env("HF_HOME", self.effective_hf_home.lock().as_path());

        // Inject the LLM model/repo/ctx_size into env vars whose names match
        // the placeholders in `workers/llm-workers.yaml`. Misalignment
        // silently falls back to the YAML `:-default`; pinned by
        // `jobworkerp_env_names_match_yaml_placeholders`. Sourced from the
        // start-time re-resolved triple, NOT `self.config.llm_*`, so a
        // `set_llm_settings` restart picks up the new preset (the cached
        // config is frozen at app boot).
        for (k, v) in jobworkerp_llm_env_vars(
            llm_model,
            llm_hf_repo,
            llm_ctx_size,
            llm_kv_cache_type,
            llm_mtp_enabled,
            llm_mtp_draft_model,
        ) {
            cmd.env(k, v);
        }

        // External LLM (genai) env vars: model name, base_url, and API key.
        // Normalize a persisted `Some("")` (an empty proxy URL the frontend
        // may have shipped historically) into None — without this the
        // env-var routing below picks `OPENAI_API_KEY` for a Gemini /
        // Anthropic model and the provider responds with "API key not
        // valid" because it never received its expected env var.
        let base_url_for_routing = llm_settings.base_url.as_deref().filter(|u| !u.is_empty());
        // Track which provider key env var (if any) we inject, so a later
        // Local→External hot-reload can tell whether the running child already
        // carries the key the new provider needs (see `spawned_external_key_env`).
        let mut injected_key_env: Option<String> = None;
        if let Some(model) = &llm_settings.provider_model {
            cmd.env("LOOKBACK_EXTERNAL_LLM_MODEL", model);
            if let Some(api_key) = crate::commands::llm_settings::load_api_key() {
                let env_var = crate::commands::llm_settings::provider_env_var_for_model(
                    model,
                    base_url_for_routing,
                );
                // Diagnostic only — key VALUE never logged. Lets a misrouted
                // injection be diagnosed from the sidecar log without
                // re-installing a debug build.
                info!(
                    target: "lookback::llm",
                    model = %model,
                    env_var = env_var,
                    key_len = api_key.len(),
                    base_url_set = base_url_for_routing.is_some(),
                    "injecting external LLM API key into jobworkerp child env"
                );
                cmd.env(env_var, &api_key);
                injected_key_env = Some(env_var.to_string());
            } else {
                warn!(
                    target: "lookback::llm",
                    model = %model,
                    "External LLM model configured but no API key found in Keychain — \
                     provider will reject the request"
                );
            }
        }
        if let Some(base_url) = base_url_for_routing {
            cmd.env("LOOKBACK_EXTERNAL_LLM_BASE_URL", base_url);
        }
        // Record the injected key env so a Local→External hot-reload can gate
        // on "the child already has this provider's key" (the genai key can't
        // be pushed to a running child — env is fixed at spawn).
        *self.spawned_external_key_env.lock() = injected_key_env;

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                let scope = self
                    .config
                    .jobworkerp_bin
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("sidecar");
                self.close_native_log_dirs(Some(scope));
                return Err(spawn_err("jobworkerp", &self.config.jobworkerp_bin, error));
            }
        };
        if let Some(pid) = child.id() {
            let scope = self
                .config
                .jobworkerp_bin
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("sidecar");
            self.mark_native_log_dir_pid(scope, pid);
        }
        // jobworkerp does not (yet) emit structured startup-error rows on
        // the `app::startup_error` target, so the per-line scanner is a
        // no-op. Pass `None` to make that invariant explicit rather than
        // wiring the slot only to be ignored.
        self.output_tasks.lock().extend(forward_output(
            &mut child,
            "jobworkerp",
            &self.config.data.log_dir(),
            None,
        ));
        Ok(child)
    }

    /// Spawn the memories gRPC front. `auto_embedding_override` forces the
    /// `MEMORY_AUTO_EMBEDDING_ENABLED` / vector-store env on (`Some(true)`)
    /// or off (`Some(false)`) regardless of `config.auto_embedding_enabled`;
    /// `None` uses the config value. The degraded-restart path passes
    /// `Some(false)` so a dimension-mismatched LanceDB is never opened.
    fn spawn_memories(
        &self,
        port: u16,
        jw_port: u16,
        rust_log: &str,
        embedding_runtime: &crate::commands::embedding_settings::EmbeddingRuntime,
        staged_embedding_workers_yaml: Option<&std::path::Path>,
        auto_embedding_override: Option<bool>,
    ) -> AppResult<Child> {
        let env_entries = std::env::vars().chain(
            self.config
                .env_file
                .as_ref()
                .and_then(|path| dotenvy::from_path_iter(path).ok())
                .into_iter()
                .flatten()
                .filter_map(Result::ok),
        );
        validate_no_legacy_maintenance_env(env_entries)?;
        let auto_embedding_on =
            auto_embedding_override.unwrap_or(self.config.auto_embedding_enabled);
        // memories' gRPC frontend reads `GRPC_ADDR` as its *listen* address.
        // `MEMORY_GRPC_HOST` / `MEMORY_GRPC_PORT` are a SEPARATE concern: the
        // callback address the auto-embedding / reflection-intent workflows
        // (running on jobworkerp) use to call this server back. Both are
        // REQUIRED when auto-embedding is on — the YAML reads them as
        // `%{MEMORY_GRPC_HOST}/%{MEMORY_GRPC_PORT}` with no default.
        let mut cmd = self.base_cmd(
            &self.config.memories_bin,
            &self.config.data.memories_data_dir(),
            rust_log,
        );
        let auto_embedding = if auto_embedding_on { "true" } else { "false" };
        let grpc_addr = format!("127.0.0.1:{port}");
        validate_search_index_maintenance_listener(&grpc_addr, &grpc_addr)?;
        cmd.env("GRPC_ADDR", &grpc_addr)
            .env("MEMORY_AUTO_EMBEDDING_ENABLED", auto_embedding)
            .env("MEMORY_GRPC_HOST", "127.0.0.1")
            .env("MEMORY_GRPC_PORT", port.to_string())
            .env("MEMORY_RAG_TOOLS_ENABLED", "false")
            .env("USE_GRPC_WEB", "false")
            .env("JOBWORKERP_ADDR", format!("http://127.0.0.1:{jw_port}"))
            .env(
                "MEMORY_REFLECTION_DISPATCH_ENABLED",
                if self.config.reflection_dispatch_enabled {
                    "true"
                } else {
                    "false"
                },
            )
            // Pin the FTS dictionary home so the lindera-feature `front`
            // build resolves `<this>/lindera/ipadic/config.yml`.
            .env(
                "LANCE_LANGUAGE_MODEL_HOME",
                self.config.lance_language_model_home.as_os_str(),
            );
        for (key, value) in search_index_maintenance_env_vars(&grpc_addr) {
            cmd.env(key, value);
        }
        for (key, value) in memories_sqlite_env_vars(&self.config.data) {
            cmd.env(key, value);
        }
        for (key, value) in memories_cache_env_vars() {
            cmd.env(key, value);
        }

        // Vector store (LanceDB) wiring. Required for all local vector paths:
        // memory, thread, and the reflection intent store.
        // memories reads `MEMORY_LANCEDB_URI` (NOT the unused `LANCEDB_PATH`),
        // defaulting to a cwd-relative path; we pin it under the app data
        // root. `REFLECTION_LANCEDB_URI` defaults to
        // `${MEMORY_LANCEDB_URI}/reflection_intent`, so it needs no explicit
        // value. `REFLECTION_VECTOR_SIZE` falls back to `MEMORY_VECTOR_SIZE`.
        apply_memories_vector_env(
            &mut cmd,
            auto_embedding_on,
            embedding_runtime.vector_size,
            &self.config.data.lancedb_dir().join("memories.lancedb"),
        );
        // Prefixes are intentionally inherited by jobworkerp, whose workflow
        // YAML expands them into `embed_text` calls. Remove only the document
        // prefix from memories: its internally chunked embedding path rejects
        // it to protect persisted chunk offsets, while the query prefix is
        // needed for retrieval embeddings.
        strip_memories_document_embedding_prefix(&mut cmd);

        // Point the embedding dispatchers at the agent-app's bundled YAMLs
        // (Metal device + unified `memories-mm-embedding` names) instead of
        // memories' compile-time CUDA defaults. Prefer the staged YAML
        // (where conditional fields like `tokenizer_model_id:` are present
        // or absent per the runtime) over the committed bundle.
        if let Some(staged) = staged_embedding_workers_yaml {
            cmd.env("MEMORY_WORKERS_YAML", staged);
        } else if let Some(workflows) = &self.config.workflows_dir {
            cmd.env(
                "MEMORY_WORKERS_YAML",
                workflows.join("auto-embedding-workers.yaml"),
            );
        }
        if let Some(workflows) = &self.config.workflows_dir {
            cmd.env(
                "REFLECTION_INTENT_WORKERS_YAML",
                workflows
                    .join("thread-reflection")
                    .join("auto-reflection-intent-embedding-workers.yaml"),
            )
            // Summary embedding dispatcher: when reflection dispatch is
            // enabled, memories' module init constructs BOTH the intent and
            // summary dispatchers. Without this the summary dispatcher falls
            // back to its compile-time `CARGO_MANIFEST_DIR`-relative default,
            // which does not exist in a bundled .app, so it fails to register
            // (`auto-reflection-summary-embedding` load error at startup).
            .env(
                "REFLECTION_WORKERS_YAML",
                workflows
                    .join("thread-reflection")
                    .join("auto-reflection-summary-embedding-workers.yaml"),
            );
        }

        // Without a staged Lindera dictionary, a lindera-feature build would
        // fail FTS index creation. Fall back to the ngram tokenizer so
        // Japanese 2-gram search still works dictionary-free (spec §3.R3).
        cmd.env(
            "MEMORY_FTS_TOKENIZER",
            effective_memory_fts_tokenizer(
                self.config.env_file.as_deref(),
                self.config.lindera_dict_staged,
                crate::commands::process_env_lookup,
            ),
        );

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                let scope = self
                    .config
                    .memories_bin
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("sidecar");
                self.close_native_log_dirs(Some(scope));
                return Err(spawn_err("memories", &self.config.memories_bin, error));
            }
        };
        if let Some(pid) = child.id() {
            let scope = self
                .config
                .memories_bin
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("sidecar");
            self.mark_native_log_dir_pid(scope, pid);
        }
        // The memories sidecar emits structured startup-error rows on
        // its tracing stdout (target `app::startup_error`); wire the
        // shared slot so `wait_for_tcp_or_failure` can short-circuit
        // the 30 s TCP wait the moment a row arrives.
        self.output_tasks.lock().extend(forward_output(
            &mut child,
            "memories",
            &self.config.data.log_dir(),
            Some(self.startup_failure.clone()),
        ));
        Ok(child)
    }

    /// Spawn the memories child and register it in the process list. Wraps
    /// `spawn_memories` + the `state` push so the initial start and the
    /// degraded restart don't duplicate the lock/push boilerplate.
    fn spawn_and_register_memories(
        &self,
        mem_port: u16,
        jw_port: u16,
        rust_log: &str,
        embedding_runtime: &crate::commands::embedding_settings::EmbeddingRuntime,
        staged_embedding_workers_yaml: Option<&std::path::Path>,
        auto_embedding_override: Option<bool>,
    ) -> AppResult<()> {
        let child = self.spawn_memories(
            mem_port,
            jw_port,
            rust_log,
            embedding_runtime,
            staged_embedding_workers_yaml,
            auto_embedding_override,
        )?;
        if let Some(procs) = self.state.lock().as_mut() {
            procs.push(Process {
                name: "memories",
                child,
                port: mem_port,
            });
        }
        Ok(())
    }

    fn spawn_conductor(&self, port: u16, rust_log: &str) -> AppResult<Child> {
        let mut cmd = self.base_cmd(&self.config.conductor_bin, &self.config.data.root, rust_log);
        for (key, value) in conductor_env_vars(&self.config.data, port) {
            cmd.env(key, value);
        }
        cmd.env("USE_GRPC_WEB", "true");

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                let scope = self
                    .config
                    .conductor_bin
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("sidecar");
                self.close_native_log_dirs(Some(scope));
                return Err(spawn_err("conductor", &self.config.conductor_bin, error));
            }
        };
        if let Some(pid) = child.id() {
            let scope = self
                .config
                .conductor_bin
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("sidecar");
            self.mark_native_log_dir_pid(scope, pid);
        }
        self.output_tasks.lock().extend(forward_output(
            &mut child,
            "conductor",
            &self.config.data.log_dir(),
            None,
        ));
        Ok(child)
    }
}

/// Mirror the External-LLM settings into THIS process's env so the
/// `worker_yaml_paths` loader's `expand_env` can substitute them into the
/// `llm-workers.yaml` placeholders (`%{LOOKBACK_EXTERNAL_LLM_MODEL:-gpt-4o}` /
/// `%{LOOKBACK_EXTERNAL_LLM_BASE_URL:-}`). The values were already set on the
/// jobworkerp CHILD's env in `spawn_jobworkerp`, but the YAML expansion runs
/// in the Tauri process; without this, the placeholders always fall back to
/// the literal defaults and a user-configured Gemini / Claude / proxy URL
/// silently turns into the default OpenAI model.
///
/// An unset / empty value clears the env var so a switch from External back
/// to Local mode (followed by a sidecar restart) does not leave a stale
/// model name in place.
///
/// # Safety
///
/// `std::env::set_var` / `remove_var` are documented as not thread-safe.
/// All current callers run during `Sidecars::start` (app boot, or after
/// `Sidecars::stop` in the `set_llm_settings` restart path) where the
/// sidecar children — the only other env-touching threads in this process —
/// are not yet (or no longer) running.
pub(crate) unsafe fn apply_external_llm_env(provider_model: Option<&str>, base_url: Option<&str>) {
    unsafe {
        set_or_clear_env("LOOKBACK_EXTERNAL_LLM_MODEL", provider_model);
        set_or_clear_env("LOOKBACK_EXTERNAL_LLM_BASE_URL", base_url);
    }
}

/// `Some(non-empty) → set_var`, anything else (`None`, `Some("")`) →
/// `remove_var`. The empty-as-unset convention is shared with the
/// `apply_*_llm_env` helpers so a settings revert (`Some("")` from the
/// frontend) and a fresh boot (`None` from the resolver) collapse to the
/// same "no override" terminal state.
///
/// # Safety
///
/// `std::env::set_var` / `remove_var` are not thread-safe. Callers must
/// uphold the single-thread invariant documented on each `apply_*` site.
unsafe fn set_or_clear_env(var: &str, value: Option<&str>) {
    unsafe {
        match value {
            Some(v) if !v.is_empty() => std::env::set_var(var, v),
            _ => std::env::remove_var(var),
        }
    }
}

/// Resolve the memories endpoint for `lookback-recall.yaml`'s
/// `memories_grpc_*` input DEFAULTS and mirror it into THIS process's env
/// (`LOOKBACK_RAG_MEMORIES_HOST` / `_PORT` / `_TLS`) so the worker-YAML
/// loader's `expand_env` substitutes it when `lookback_recall` is registered.
///
/// Resolution matches `commands::connection::resolve_targets`:
/// - **Local** mode → the live local sidecar (`127.0.0.1:<mem_port>`, no TLS).
/// - **Remote** mode → the configured memories URL decomposed via
///   `parse_callback` (host / port / TLS, incl. HTTPS).
///
/// This is what lets the MCP path (which runs the workflow directly, with no
/// Tauri to inject per-dispatch like chat does) search the SAME memories the
/// browse clients use. A malformed / missing remote URL falls back to the
/// local sidecar so a bad config can't wedge worker registration; the chat
/// path is unaffected either way because it overrides these per-dispatch.
///
/// # Safety
///
/// See [`apply_external_llm_env`] — same single-thread invariant: only called
/// from `Sidecars::start` while sidecar children are not (yet) up.
unsafe fn apply_rag_memories_env(data: &DataPaths, mem_port: u16) {
    use crate::commands::connection::{self, ConnectionMode};
    let cfg = connection::load_connection_config(&data.connection_config_path());
    // Local sidecar by default; only a successfully-parsed remote URL moves
    // off it (a parse failure logs and keeps the local fallback).
    let (host, port, tls) = match cfg.mode {
        ConnectionMode::Remote => match cfg
            .remote_memories_url
            .as_deref()
            .filter(|u| !u.is_empty())
            .map(connection::parse_callback)
        {
            Some(Ok(cb)) => (cb.host, cb.port, cb.tls),
            Some(Err(e)) => {
                warn!(error = %e, "invalid remote memories URL; RAG/MCP search falls back to the local sidecar");
                ("127.0.0.1".to_string(), mem_port, false)
            }
            None => {
                warn!(
                    "remote mode without a memories URL; RAG/MCP search falls back to the local sidecar"
                );
                ("127.0.0.1".to_string(), mem_port, false)
            }
        },
        ConnectionMode::Local => ("127.0.0.1".to_string(), mem_port, false),
    };
    unsafe {
        std::env::set_var("LOOKBACK_RAG_MEMORIES_HOST", host);
        std::env::set_var("LOOKBACK_RAG_MEMORIES_PORT", port.to_string());
        std::env::set_var("LOOKBACK_RAG_MEMORIES_TLS", tls.to_string());
    }
}

/// Mirror the Local-LLM runtime selection into THIS process's env so the
/// `worker_yaml_paths` loader's `expand_env` can substitute it into the
/// `llm-workers.yaml` placeholders
/// (`%{LOOKBACK_LLM_MODEL:-gemma-4-E2B-…}` /
/// `%{LOOKBACK_LLM_HF_REPO:-unsloth/gemma-4-E2B-it-qat-GGUF}` /
/// `%{LOOKBACK_LLM_CTX_SIZE:-131072}` / `%{LOOKBACK_LLM_MTP_*:-…}`).
///
/// These values are already set on the jobworkerp CHILD's env in
/// `spawn_jobworkerp`, but the YAML expansion (the call that ultimately
/// determines what `runner_settings.model` gets upserted as) runs in the
/// Tauri process via `register_workers_from_yaml`. Without this mirror the
/// placeholders always fall back to their YAML default and a user-selected
/// preset never reaches the registered `memories-llm` worker — chat then
/// runs against the YAML fallback regardless of what Settings shows.
///
/// `None` / empty values clear the env var so a subsequent restart with a
/// reverted (default) preset does not leave a stale model name behind.
///
/// # Safety
///
/// See [`apply_external_llm_env`] — same single-thread invariant: only
/// called from `Sidecars::start` while sidecar children are not (yet) up.
pub(crate) unsafe fn apply_local_llm_env(
    model: Option<&str>,
    hf_repo: Option<&str>,
    ctx_size: Option<u32>,
    kv_cache_type: Option<&str>,
    mtp_enabled: Option<bool>,
    mtp_draft_model: Option<&str>,
) {
    let ctx = ctx_size.map(|v| v.to_string());
    let mtp_enabled = mtp_enabled.map(|v| v.to_string());
    unsafe {
        set_or_clear_env("LOOKBACK_LLM_MODEL", model);
        set_or_clear_env("LOOKBACK_LLM_HF_REPO", hf_repo);
        set_or_clear_env("LOOKBACK_LLM_CTX_SIZE", ctx.as_deref());
        set_or_clear_env("LOOKBACK_LLM_KV_CACHE_TYPE", kv_cache_type);
        set_or_clear_env("LOOKBACK_LLM_MTP_ENABLED", mtp_enabled.as_deref());
        set_or_clear_env("LOOKBACK_LLM_MTP_DRAFT_MODEL", mtp_draft_model);
    }
}

/// Mirror the embedding-runtime selection into THIS process's env so the
/// staged worker YAML's `%{...}` placeholders (and the inline-runtime
/// fields the renderer writes verbatim) reach the registered embedding
/// worker. The actual model_id / dtype / max_sequence_length values are
/// already baked into the staged YAML; this helper is the back-compat
/// path for an unsaved-settings install whose YAML still has the original
/// `%{LOOKBACK_EMBEDDING_*:-default}` placeholders.
///
/// `None` / empty values clear the env so a Settings revert collapses
/// back to the YAML defaults.
///
/// # Safety
///
/// Same single-thread invariant as [`apply_local_llm_env`]: only called
/// from `Sidecars::start` while sidecar children are not (yet) up.
pub(crate) unsafe fn apply_embedding_env(vars: &[(&'static str, String)]) {
    // Clear ALL managed keys first so a stale leftover from a previous
    // restart (e.g. a tokenizer override the user cleared) is removed
    // even when the new resolution doesn't include it.
    unsafe {
        for &k in crate::commands::embedding_settings::EMBEDDING_ENV_KEYS {
            std::env::remove_var(k);
        }
        for (k, v) in vars {
            if !v.is_empty() {
                std::env::set_var(k, v);
            }
        }
    }
}

/// Remove the document embedding prefix from the memories child command.
/// `Command` otherwise inherits the parent environment, which is required by
/// jobworkerp YAML expansion. The query prefix remains for retrieval queries.
fn strip_memories_document_embedding_prefix(cmd: &mut Command) {
    cmd.env_remove(MEMORIES_UNSUPPORTED_DOCUMENT_PREFIX_ENV_KEY);
}

/// Select the default filter for a resident sidecar profile.
///
/// Keeping the profile decision in a pure function makes the dev/release
/// policy testable without spawning any of the external binaries.
fn default_sidecar_log(debug_assertions: bool) -> &'static str {
    if debug_assertions {
        DEFAULT_SIDECAR_LOG_DEV
    } else {
        DEFAULT_SIDECAR_LOG_RELEASE
    }
}

/// Resolve a sidecar filter from an optional explicit override and build
/// profile. `LOOKBACK_RUST_LOG` is intentionally handled by the caller so the
/// policy itself remains pure and can be tested for both profiles.
fn effective_rust_log_for(lookback_override: Option<&str>, debug_assertions: bool) -> String {
    lookback_override
        .map(str::to_owned)
        .unwrap_or_else(|| default_sidecar_log(debug_assertions).to_owned())
}

fn effective_rust_log() -> String {
    effective_rust_log_for(
        std::env::var("LOOKBACK_RUST_LOG").ok().as_deref(),
        cfg!(debug_assertions),
    )
}

/// Preserve the database location and SQLite's original diagnostic when the
/// preflight gate cannot complete inspection. This is not evidence that the
/// schema contract itself is invalid.
fn memory_kind_gate_error(db_path: &Path, reason: String) -> AppError {
    AppError::MemoryKindDatabaseCheckFailed {
        db_path: db_path.display().to_string(),
        reason,
    }
}

/// Surface the binary path that failed to spawn — the raw `io::Error` only
/// carries `No such file or directory` without naming the missing file, which
/// is essentially undebuggable.
fn spawn_err(name: &str, bin: &std::path::Path, e: std::io::Error) -> AppError {
    AppError::Config(format!("spawn {name} ({}) failed: {e}", bin.display()))
}

impl Drop for Sidecars {
    fn drop(&mut self) {
        self.stop_blocking();
    }
}

async fn stop_child(child: &mut Child, name: &str) {
    let pid = child.id();
    debug!(?name, ?pid, "stopping sidecar");

    // SIGTERM first (graceful). On Unix, this triggers `tokio::signal`
    // handlers inside the jobworkerp/memories binaries.
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: pid was returned by tokio::process::Child::id() for a
        // still-tracked child; libc_kill is a thin wrapper around kill(2).
        unsafe {
            libc_kill(pid as i32, 15 /* SIGTERM */);
        }
    }

    match timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) => info!(?name, ?status, "sidecar exited"),
        Ok(Err(e)) => warn!(?name, error = ?e, "wait failed"),
        Err(_) => {
            warn!(?name, "sidecar did not exit within 5s; sending SIGKILL");
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SAFETY: `kill(2)` is async-signal-safe; passing a pid we just received
    // from `child.id()` is always a valid input. We ignore the return value
    // because the worst case (ESRCH) is also our fallback path (SIGKILL).
    unsafe {
        let _ = kill(pid, sig);
    }
}

/// Block until a TCP connect succeeds against `addr`, polling once every
/// 200 ms up to the deadline. Used as a lightweight "service is listening"
/// check in lieu of the gRPC Health Check (which requires we have the
/// generated client available at startup — circular dependency).
///
/// When `failure_slot` is `Some`, the loop additionally polls the shared
/// slot each iteration: as soon as the per-stream scanner writes a
/// structured `app::startup_error` row (or a `thread '...' panicked at
/// ...` line on stderr), we abandon the TCP wait and surface
/// `AppError::SidecarStartupFailed`, so a memories child that died at
/// init doesn't burn the full 30 s timeout. The jobworkerp wait path
/// passes `None` because it has no structured-failure signal to consult.
async fn wait_for_tcp_or_failure(
    addr: (&str, u16),
    within: Duration,
    failure_slot: Option<&crate::sidecar::startup_error::StartupFailureSlot>,
) -> AppResult<()> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(slot) = failure_slot
            && let Some(failure) = slot.lock().clone()
        {
            return Err(AppError::SidecarStartupFailed(failure));
        }
        match TcpStream::connect((addr.0, addr.1)).await {
            Ok(_) => return Ok(()),
            Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(200)).await,
            Err(e) => {
                return Err(AppError::SidecarNotReady(format!(
                    "tcp connect to {}:{} failed within {:?}: {}",
                    addr.0, addr.1, within, e
                )));
            }
        }
    }
}

/// Signature of the per-line structured-failure parser. Passed in from
/// `forward_output` so stdout and stderr can each plug in the parser
/// that fits their stream (`parse_stdout_line` for tracing JSON,
/// `parse_stderr_panic_line` for panic preambles) without `pipe_lines`
/// needing to know which is which.
type StartupFailureParser = fn(&str) -> Option<crate::sidecar::startup_error::StartupFailure>;

/// Plumb child stdout/stderr to (a) a log file under `<data>/log/<name>.log`,
/// (b) the tracing subscriber so it shows up in `pnpm tauri dev`, and
/// (c) — when `structured_failure_sink` is `Some` — a structured
/// startup-failure scanner that writes the first matching failure into
/// the shared slot so the TCP wait can short-circuit.
fn forward_output(
    child: &mut Child,
    name: &'static str,
    log_dir: &std::path::Path,
    structured_failure_sink: Option<crate::sidecar::startup_error::StartupFailureSlot>,
) -> Vec<OutputTask> {
    let log_dir = log_dir.to_path_buf();
    let _ = std::fs::create_dir_all(&log_dir);
    let mut tasks = Vec::with_capacity(2);
    if let Some(stdout) = child.stdout.take() {
        let path = log_dir.join(format!("{name}.stdout.log"));
        let sink = structured_failure_sink.clone();
        let parser = sink.map(|s| {
            (
                s,
                crate::sidecar::startup_error::parse_stdout_line as StartupFailureParser,
            )
        });
        tasks.push((
            name,
            tokio::spawn(pipe_lines(stdout, path, name, Level::DEBUG, parser)),
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        let path = log_dir.join(format!("{name}.stderr.log"));
        let parser = structured_failure_sink.map(|s| {
            (
                s,
                crate::sidecar::startup_error::parse_stderr_panic_line as StartupFailureParser,
            )
        });
        tasks.push((
            name,
            tokio::spawn(pipe_lines(stderr, path, name, Level::WARN, parser)),
        ));
    }
    tasks
}

async fn pipe_lines<R>(
    stream: R,
    path: std::path::PathBuf,
    name: &'static str,
    level: Level,
    scanner: Option<(
        crate::sidecar::startup_error::StartupFailureSlot,
        StartupFailureParser,
    )>,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(stream).lines();
    let file = match crate::log_rotation::RotatingFile::new(path.clone()) {
        Ok(file) => file,
        Err(e) => {
            warn!(?path, error = ?e, "open log file failed");
            return;
        }
    };
    let file = Arc::new(std::sync::Mutex::new(file));
    // Once the slot is filled the wait loop bails out on its next 200 ms
    // tick, but child stdout keeps streaming for the rest of the process
    // lifetime. Cache the "scanning is still useful" verdict locally so
    // we skip the contains-check + JSON parse on every subsequent line
    // — memories alone emits thousands of log lines per import.
    let mut scanner_active = scanner.is_some();
    while let Ok(Some(line)) = reader.next_line().await {
        // stderr is forwarded at WARN, but llguidance (the structured-output
        // constraint engine inside the LLM sidecar) prints recoverable
        // grammar-backtracking notices straight to stderr on every
        // schema-constrained generation. They are benign — the sampler
        // rejects the offending token and continues — but at WARN they drown
        // the terminal. Demote those known lines to DEBUG. The full line is
        // still written to the log file below, so nothing is lost.
        let effective = if level == Level::WARN && is_benign_sidecar_noise(&line) {
            Level::DEBUG
        } else {
            level
        };
        match effective {
            Level::WARN => warn!(target: "sidecar", source = name, "{line}"),
            _ => debug!(target: "sidecar", source = name, "{line}"),
        }

        // Structured startup-failure detection. The cheap `contains`
        // pre-filter avoids invoking `serde_json::from_str` on every
        // normal log line; only the small fraction that mention the
        // startup-error target or a panic preamble reach the parser.
        // First matching row wins (the wait loop returns on the first
        // `Some`); after that the scanner deactivates locally so no
        // line in the long steady state pays the cost again.
        if scanner_active
            && let Some((sink, parser)) = scanner.as_ref()
            && (line.contains(crate::sidecar::startup_error::STARTUP_ERROR_TARGET)
                || line.contains(PANIC_PREAMBLE_NEEDLE))
        {
            let mut guard = sink.lock();
            if guard.is_none() {
                if let Some(failure) = parser(&line) {
                    tracing::warn!(
                        source = name,
                        code = failure.code(),
                        "sidecar reported structured startup failure",
                    );
                    *guard = Some(failure);
                    scanner_active = false;
                }
            } else {
                scanner_active = false;
            }
        }

        let mut buf = line.into_bytes();
        buf.push(b'\n');
        let file_for_write = Arc::clone(&file);
        let write_result = tokio::task::spawn_blocking(move || {
            let mut file = file_for_write
                .lock()
                .map_err(|_| std::io::Error::other("rotating log writer lock poisoned"))?;
            std::io::Write::write_all(&mut *file, &buf)
        })
        .await;
        match write_result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!(?path, ?error, "sidecar log write task failed");
                break;
            }
            Err(error) => {
                warn!(?path, ?error, "sidecar log write task panicked");
                break;
            }
        }
    }
    let file_for_flush = Arc::clone(&file);
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(mut file) = file_for_flush.lock() {
            let _ = std::io::Write::flush(&mut *file);
        }
    })
    .await;
}

/// Leading substring of every Rust panic preamble (`thread 'main' (123)
/// panicked at ...`). Cheap pre-filter for the stderr scanner — see
/// `parse_stderr_panic_line` for the actual match.
const PANIC_PREAMBLE_NEEDLE: &str = "thread '";

/// Recognize llguidance's recoverable grammar-backtracking notices so they can
/// be demoted from WARN to DEBUG. These are emitted to stderr by the
/// `llguidance` crate (via `LlamaSampler::llguidance`, whose internal
/// `ParserFactory` defaults to `stderr_log_level = 1` and offers no injection
/// point for us to silence it). Matched on llguidance-specific phrasing so a
/// genuine sidecar error is never hidden.
fn is_benign_sidecar_noise(line: &str) -> bool {
    let trimmed = line.trim();
    // A `stop()` with an empty reason emits just "Warning: ; stopping",
    // which arrives here as a near-empty line.
    if trimmed.is_empty() || trimmed == "Warning:" {
        return true;
    }
    trimmed.contains("doesn't satisfy the grammar")
        || trimmed.contains("fails parse")
        || (trimmed.contains("Parser Error:") && trimmed.contains("stopping"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benign_noise_matches_llguidance_grammar_notices() {
        // Exact lines seen forwarded from the jobworkerp sidecar's stderr.
        for line in [
            r#"Warning: Parser Error: token "{" doesn't satisfy the grammar; byte '{' fails parse; stopping"#,
            "Warning: Parser Error: expecting 'H' (forced bytes), got ' '; stopping",
            "token \"x\" doesn't satisfy the grammar",
            "byte '{' fails parse",
            // A `stop()` with an empty reason forwards as a near-empty line.
            "",
            "   ",
            "Warning:",
        ] {
            assert!(is_benign_sidecar_noise(line), "should be benign: {line:?}");
        }
    }

    #[test]
    fn benign_noise_does_not_swallow_real_errors() {
        for line in [
            "ERROR worker_app: failed to bind gRPC port 9000: address in use",
            "panicked at 'unwrap on None'",
            "Error: model file not found",
            "thread 'main' panicked",
            // "Parser Error" without the llguidance "stopping" suffix is not
            // a recoverable backtrack notice and must stay at WARN.
            "Parser Error: invalid grammar supplied by config",
        ] {
            assert!(
                !is_benign_sidecar_noise(line),
                "must not be demoted: {line:?}"
            );
        }
    }

    #[test]
    fn memory_kind_gate_error_preserves_database_path_and_reason() {
        let db_path = Path::new("/tmp/lookback/default.sqlite3");
        let error = memory_kind_gate_error(db_path, "unable to open database file".into());

        let AppError::MemoryKindDatabaseCheckFailed {
            db_path: actual_path,
            reason,
        } = error
        else {
            panic!("memory_kind gate I/O failures must use the dedicated error variant");
        };
        assert_eq!(actual_path, db_path.to_string_lossy());
        assert_eq!(reason, "unable to open database file");
    }

    fn app_settings_with_tz(tz: Option<&str>) -> crate::data::paths::AppSettings {
        crate::data::paths::AppSettings {
            timezone: tz.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn search_index_maintenance_env_uses_the_same_loopback_listener() {
        let env = search_index_maintenance_env_vars("127.0.0.1:9010");
        assert!(env.iter().any(|(key, value)| {
            *key == "SEARCH_INDEX_MAINTENANCE_GRPC_ADDR" && value == "127.0.0.1:9010"
        }));
        assert!(
            env.iter()
                .any(|(key, value)| { *key == "MEMORY_COMPACTION_INTERVAL_SECS" && value == "0" })
        );
    }

    #[test]
    fn maintenance_listener_rejects_non_loopback_or_mismatch() {
        assert!(
            validate_search_index_maintenance_listener("127.0.0.1:9010", "127.0.0.1:9010").is_ok()
        );
        assert!(
            validate_search_index_maintenance_listener("0.0.0.0:9010", "0.0.0.0:9010").is_err()
        );
        assert!(
            validate_search_index_maintenance_listener("127.0.0.1:9010", "127.0.0.1:9011").is_err()
        );
    }

    #[test]
    fn legacy_maintenance_environment_is_rejected() {
        assert!(
            validate_no_legacy_maintenance_env(vec![(
                "MEMORY_AUTO_OPTIMIZE_INTERVAL".into(),
                "10".into()
            )])
            .is_err()
        );
        assert!(
            validate_no_legacy_maintenance_env(vec![(
                "MEMORY_INDEX_UPDATE_INTERVAL_SECS".into(),
                "0".into()
            )])
            .is_ok()
        );
    }

    // Env-touching: relies on `--test-threads=1` (repo-wide convention).
    #[test]
    fn resolve_timezone_auto_prefers_tz_env() {
        // "Auto" case = no explicit app-settings timezone (None).
        // SAFETY: single-threaded test run; `set_var`/`remove_var` are the
        // documented env-mutation path the rest of this module uses.
        unsafe {
            std::env::set_var("TZ", "America/New_York");
        }
        assert_eq!(resolve_timezone(None), "America/New_York");
        assert_eq!(
            resolve_timezone(Some(&app_settings_with_tz(None))),
            "America/New_York",
            "a None app-settings timezone means Auto — env TZ is used"
        );

        // A blank TZ is treated as unset — the result then comes from the
        // host zone / JST fallback, which are exercised separately below.
        unsafe {
            std::env::set_var("TZ", "   ");
        }
        assert_ne!(
            resolve_timezone(None),
            "   ",
            "a blank TZ must not be forwarded verbatim"
        );

        unsafe {
            std::env::remove_var("TZ");
        }
        // With TZ unset the resolver falls back to the host zone (if
        // /etc/localtime is a zoneinfo symlink) or JST — either way a
        // non-empty IANA-shaped name, never blank.
        let resolved = resolve_timezone(None);
        assert!(
            !resolved.trim().is_empty(),
            "resolve_timezone must never return blank, got {resolved:?}"
        );
    }

    // Env-touching: relies on `--test-threads=1`.
    #[test]
    fn resolve_timezone_prefers_explicit_app_setting_over_env() {
        // SAFETY: single-threaded test run.
        unsafe {
            std::env::set_var("TZ", "America/New_York");
        }
        // An explicit GUI selection must win over the env so a DMG launch
        // honours the user's choice deterministically.
        assert_eq!(
            resolve_timezone(Some(&app_settings_with_tz(Some("Europe/Paris")))),
            "Europe/Paris"
        );
        // A blank app-settings timezone is treated as Auto → env wins.
        assert_eq!(
            resolve_timezone(Some(&app_settings_with_tz(Some("   ")))),
            "America/New_York",
            "a blank app-settings timezone falls through to the env"
        );
        unsafe {
            std::env::remove_var("TZ");
        }
    }

    #[test]
    fn system_timezone_name_is_none_or_iana_shaped() {
        // On CI /etc/localtime may be absent (None) or a zoneinfo symlink
        // (an "Area/Location" name). It must never surface the raw path or
        // an empty string.
        if let Some(name) = system_timezone_name() {
            assert!(!name.is_empty());
            assert!(
                !name.contains("zoneinfo"),
                "the zoneinfo prefix must be stripped, got {name:?}"
            );
        }
    }

    // Env-touching: relies on `--test-threads=1`.
    #[test]
    fn conductor_timezone_prefers_cron_var_over_tz() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().to_path_buf());
        // SAFETY: single-threaded test run.
        unsafe {
            std::env::set_var("CONDUCTOR_CRON_TIMEZONE", "Europe/Paris");
            std::env::set_var("TZ", "America/New_York");
        }
        let vars = conductor_env_vars(&data, 1234);
        let tz = vars
            .iter()
            .find(|(k, _)| *k == "CONDUCTOR_CRON_TIMEZONE")
            .map(|(_, v)| v.as_str());
        assert_eq!(
            tz,
            Some("Europe/Paris"),
            "CONDUCTOR_CRON_TIMEZONE must win over TZ for the cron scheduler"
        );

        unsafe {
            std::env::remove_var("CONDUCTOR_CRON_TIMEZONE");
        }
        let vars = conductor_env_vars(&data, 1234);
        let tz = vars
            .iter()
            .find(|(k, _)| *k == "CONDUCTOR_CRON_TIMEZONE")
            .map(|(_, v)| v.as_str());
        assert_eq!(
            tz,
            Some("America/New_York"),
            "without the cron var, conductor falls back to TZ via resolve_timezone"
        );

        unsafe {
            std::env::remove_var("TZ");
        }
    }

    // Env-touching: relies on `--test-threads=1`.
    #[test]
    fn conductor_timezone_honors_app_setting_when_no_cron_var() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().to_path_buf());
        std::fs::create_dir_all(&data.root).unwrap();
        // Persist an explicit GUI timezone selection.
        crate::data::paths::save_app_settings(
            &data.app_settings_path(),
            &app_settings_with_tz(Some("Europe/Berlin")),
        )
        .unwrap();
        // SAFETY: single-threaded test run.
        unsafe {
            std::env::remove_var("CONDUCTOR_CRON_TIMEZONE");
            std::env::set_var("TZ", "America/New_York");
        }
        let vars = conductor_env_vars(&data, 1234);
        let tz = vars
            .iter()
            .find(|(k, _)| *k == "CONDUCTOR_CRON_TIMEZONE")
            .map(|(_, v)| v.as_str());
        assert_eq!(
            tz,
            Some("Europe/Berlin"),
            "without the cron var, the explicit app-settings timezone wins over env TZ"
        );
        unsafe {
            std::env::remove_var("TZ");
        }
    }

    #[test]
    fn memories_cache_defaults_are_complete() {
        assert_eq!(
            memories_cache_env_vars(),
            [
                ("MEMORY_CACHE_NUM_COUNTERS", "12960"),
                ("MEMORY_CACHE_MAX_COST", "12960"),
                ("MEMORY_CACHE_USE_METRICS", "true"),
            ]
        );
    }

    #[test]
    fn memories_sqlite_env_is_complete_for_rdb_url_config() {
        let data = DataPaths::with_root("/tmp/lookback-memories-rdb-test");
        let envs: std::collections::HashMap<&str, String> =
            memories_sqlite_env_vars(&data).into_iter().collect();

        assert_eq!(
            envs.get("SQLITE_URL").map(String::as_str),
            Some("sqlite:///tmp/lookback-memories-rdb-test/memories/default.sqlite3?mode=rwc")
        );
        assert_eq!(
            envs.get("SQLITE_MAX_CONNECTIONS").map(String::as_str),
            Some("5")
        );
    }

    #[test]
    fn migration_and_front_share_effective_fts_tokenizer_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("lookback.env");
        std::fs::write(&env_file, "MEMORY_FTS_TOKENIZER=simple\n").unwrap();
        assert_eq!(
            effective_memory_fts_tokenizer(Some(&env_file), true, |_| Some("ngram".into())),
            "simple"
        );
        assert_eq!(
            effective_memory_fts_tokenizer(None, true, |_| Some("simple".into())),
            "simple"
        );
        assert_eq!(
            effective_memory_fts_tokenizer(None, true, |_| None),
            "lindera/ipadic"
        );
        assert_eq!(
            effective_memory_fts_tokenizer(Some(&env_file), false, |_| Some("simple".into())),
            "ngram"
        );
    }

    #[test]
    fn jobworkerp_channel_config_is_complete() {
        assert_eq!(
            jobworkerp_channel_env_vars(),
            [
                ("WORKER_DEFAULT_CONCURRENCY", "4"),
                (
                    "WORKER_CHANNELS",
                    "llm,llm_external,llm_workflow,llm_batch,llm_pipeline,llm_periodic,embedding,embedding_workflow,rag"
                ),
                ("WORKER_CHANNEL_CONCURRENCIES", "1,2,1,2,1,1,1,1,2"),
            ]
        );
    }

    #[test]
    fn conductor_env_config_is_complete() {
        let data = DataPaths::with_root("/tmp/lookback-conductor-env-test");
        let envs: std::collections::HashMap<&str, String> =
            conductor_env_vars(&data, 9020).into_iter().collect();
        assert_eq!(
            envs.get("GRPC_ADDR").map(String::as_str),
            Some("127.0.0.1:9020")
        );
        assert!(
            envs.get("SQLITE_URL")
                .is_some_and(|v| v.ends_with("/conductor.sqlite3?mode=rwc")),
            "{envs:?}"
        );
        assert_eq!(
            envs.get("SQLITE_MAX_CONNECTIONS").map(String::as_str),
            Some("5")
        );
        assert_eq!(
            envs.get("NOTIFICATION_TYPE").map(String::as_str),
            Some("channel")
        );
        assert!(envs.contains_key("CONDUCTOR_CRON_TIMEZONE"));
        // Regression: conductor-main `.expect()`s a fully-populated
        // MEMORY_CACHE_* set at AppModule init (num_counters is a required
        // field), so a DMG/GUI launch with an empty env panicked it before it
        // bound its gRPC port. All three keys must be present.
        assert_eq!(
            envs.get("MEMORY_CACHE_NUM_COUNTERS").map(String::as_str),
            Some("12960")
        );
        assert_eq!(
            envs.get("MEMORY_CACHE_MAX_COST").map(String::as_str),
            Some("1000000")
        );
        assert_eq!(
            envs.get("MEMORY_CACHE_USE_METRICS").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn memories_start_timeout_allows_full_service_initialization() {
        assert!(MEMORIES_START_TIMEOUT >= Duration::from_secs(60));
    }

    #[tokio::test]
    async fn wait_for_tcp_returns_err_when_nothing_listens() {
        // Bind then drop to learn a free port, then wait on it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let res =
            wait_for_tcp_or_failure(("127.0.0.1", port), Duration::from_millis(500), None).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn wait_for_tcp_returns_ok_when_listener_present() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Hold the listener open while waiting.
        let res = wait_for_tcp_or_failure(("127.0.0.1", port), Duration::from_secs(1), None).await;
        assert!(res.is_ok(), "{res:?}");
        drop(listener);
    }

    #[tokio::test]
    async fn wait_for_tcp_or_failure_returns_early_on_structured_failure() {
        // Simulate: the per-stream scanner sees a structured row on
        // memories' stdout shortly after the wait starts. The wait must
        // abandon the TCP poll and surface `SidecarStartupFailed`
        // without waiting for the deadline (10 s here; we expect to
        // return in well under 1 s).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener); // free port → TCP poll will keep failing
        let slot: crate::sidecar::startup_error::StartupFailureSlot =
            std::sync::Arc::new(parking_lot::Mutex::new(None));
        let slot_writer = slot.clone();
        let writer_task = tokio::spawn(async move {
            // Slight delay so the wait loop is already polling.
            tokio::time::sleep(Duration::from_millis(50)).await;
            *slot_writer.lock() = Some(
                crate::sidecar::startup_error::StartupFailure::LancedbSchemaMismatch {
                    table: "memories".into(),
                    uri: "/tmp/x".into(),
                    expected_dim: 2048,
                    actual_dim: 768,
                    expected_fingerprint: String::new(),
                    actual_fingerprint: String::new(),
                },
            );
        });
        let start = Instant::now();
        let res =
            wait_for_tcp_or_failure(("127.0.0.1", port), Duration::from_secs(10), Some(&slot))
                .await;
        writer_task.await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "expected early abort, took {elapsed:?}",
        );
        match res.expect_err("must error") {
            AppError::SidecarStartupFailed(
                crate::sidecar::startup_error::StartupFailure::LancedbSchemaMismatch {
                    expected_dim,
                    actual_dim,
                    ..
                },
            ) => {
                assert_eq!(expected_dim, 2048);
                assert_eq!(actual_dim, 768);
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn wait_for_tcp_or_failure_returns_timeout_when_slot_empty() {
        // Slot stays empty → behaves identically to the legacy
        // `wait_for_tcp`: surfaces `SidecarNotReady` after the deadline.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let slot: crate::sidecar::startup_error::StartupFailureSlot =
            std::sync::Arc::new(parking_lot::Mutex::new(None));
        let res =
            wait_for_tcp_or_failure(("127.0.0.1", port), Duration::from_millis(500), Some(&slot))
                .await;
        match res.expect_err("must error") {
            AppError::SidecarNotReady(_) => {}
            other => panic!("expected SidecarNotReady, got {other:?}"),
        }
    }

    #[test]
    fn sidecar_endpoints_format_urls() {
        let eps = SidecarEndpoints {
            jobworkerp_port: 9000,
            memories_port: Some(9010),
            conductor_port: 9020,
            mcp_server_port: None,
        };
        assert_eq!(eps.jobworkerp_url(), "http://127.0.0.1:9000");
        assert_eq!(eps.memories_url().as_deref(), Some("http://127.0.0.1:9010"));
        assert_eq!(eps.conductor_url(), "http://127.0.0.1:9020");
    }

    #[test]
    fn sidecar_endpoints_serializes_mcp_port_as_null_when_disabled() {
        // The frontend keys off `mcp_server_port` being null to know MCP is
        // off; a missing field would deserialise as undefined on the TS side
        // and break the on/off rendering. Pin the null shape.
        let eps = SidecarEndpoints {
            jobworkerp_port: 9000,
            memories_port: Some(9010),
            conductor_port: 9020,
            mcp_server_port: None,
        };
        let v = serde_json::to_value(&eps).unwrap();
        assert!(v.get("mcp_server_port").unwrap().is_null());

        let on = SidecarEndpoints {
            mcp_server_port: Some(39010),
            ..eps
        };
        assert_eq!(
            serde_json::to_value(&on).unwrap().get("mcp_server_port"),
            Some(&serde_json::json!(39010))
        );
    }

    #[test]
    fn macos_bundle_plugins_dir_uses_signed_resource_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("Lookback.app/Contents/MacOS/all-in-one");
        let plugins = dir.path().join("Lookback.app/Contents/Resources/plugins");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&plugins).unwrap();

        assert_eq!(macos_bundle_plugins_dir(&executable), Some(plugins));
    }

    #[test]
    fn macos_bundle_plugins_dir_rejects_an_unbundled_executable() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("target/release/all-in-one");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();

        assert_eq!(macos_bundle_plugins_dir(&executable), None);
    }

    fn test_sidecars() -> Sidecars {
        test_sidecars_with_data(DataPaths::with_root("/tmp/lookback-lifecycle-test"))
    }

    fn test_sidecars_with_data(data: DataPaths) -> Sidecars {
        let lance_home = data.lance_language_model_home();
        Sidecars::new(SidecarConfig {
            jobworkerp_bin: PathBuf::from("/bin/true"),
            memories_bin: PathBuf::from("/bin/true"),
            memories_db_migrate_bin: PathBuf::from("/bin/true"),
            startup_database_migration_enabled: false,
            conductor_bin: PathBuf::from("/bin/true"),
            data,
            worker_yaml_paths: Vec::new(),
            function_set_yaml_paths: Vec::new(),
            reflection_dispatch_enabled: false,
            auto_embedding_enabled: true,
            workflows_dir: None,
            lance_language_model_home: lance_home,
            lindera_dict_staged: false,
            llm_model: None,
            llm_hf_repo: None,
            llm_ctx_size: None,
            llm_kv_cache_type: None,
            env_file: None,
        })
    }

    #[test]
    fn database_migration_activity_is_visible_only_for_an_enabled_local_gate() {
        let sidecars = test_sidecars();
        assert!(!sidecars.database_migration_in_progress());

        let disabled = sidecars.database_migration_activity_if_enabled(false);
        assert!(disabled.is_none());
        assert!(!sidecars.database_migration_in_progress());

        {
            let _activity = sidecars
                .database_migration_activity_if_enabled(true)
                .expect("enabled local gate must report migration activity");
            assert!(sidecars.database_migration_in_progress());
        }
        assert!(!sidecars.database_migration_in_progress());
    }

    #[cfg(unix)]
    fn test_sidecars_with_migration(data: DataPaths, coordinator: PathBuf) -> Sidecars {
        let lance_home = data.lance_language_model_home();
        Sidecars::new(SidecarConfig {
            jobworkerp_bin: PathBuf::from("/definitely/missing/jobworkerp"),
            memories_bin: PathBuf::from("/definitely/missing/front"),
            memories_db_migrate_bin: coordinator,
            startup_database_migration_enabled: true,
            conductor_bin: PathBuf::from("/definitely/missing/conductor"),
            data,
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
        })
    }

    #[tokio::test]
    async fn maintenance_stop_retains_instance_lock_until_normal_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        data.ensure().unwrap();
        let sidecars = test_sidecars_with_data(data.clone());
        let outcome = reaper::reap_recorded(&data.sidecar_pids_path(), &data.sidecar_lock_path());
        *sidecars.instance_lock.lock() = outcome.lock;
        assert!(sidecars.owns_instance_lock());

        sidecars.stop_for_maintenance().await.unwrap();
        assert!(sidecars.owns_instance_lock());

        sidecars.stop().await.unwrap();
        assert!(!sidecars.owns_instance_lock());
    }

    #[tokio::test]
    async fn maintenance_operation_blocks_start_until_restore_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        data.ensure().unwrap();
        let mut sidecars = test_sidecars_with_data(data.clone());
        // Keep the post-maintenance start fast and deterministic: the test
        // only needs to observe that it waits for the maintenance operation.
        sidecars.config.jobworkerp_bin = PathBuf::from("/definitely/missing/jobworkerp");
        let outcome = reaper::reap_recorded(&data.sidecar_pids_path(), &data.sidecar_lock_path());
        *sidecars.instance_lock.lock() = outcome.lock;
        assert!(sidecars.owns_instance_lock());

        let sidecars = std::sync::Arc::new(sidecars);
        let operation_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let maintenance = {
            let sidecars = std::sync::Arc::clone(&sidecars);
            let operation_started = std::sync::Arc::clone(&operation_started);
            tokio::spawn(async move {
                sidecars
                    .with_maintenance_lock(|| async move {
                        operation_started.notify_one();
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        Ok(())
                    })
                    .await
            })
        };
        operation_started.notified().await;

        let mut start = {
            let sidecars = std::sync::Arc::clone(&sidecars);
            tokio::spawn(async move { sidecars.start().await })
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut start)
                .await
                .is_err(),
            "start must wait for an in-flight database restore"
        );

        assert!(maintenance.await.unwrap().is_ok());
        assert!(start.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn maintenance_operation_releases_lifecycle_lock_after_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        data.ensure().unwrap();
        let mut sidecars = test_sidecars_with_data(data.clone());
        sidecars.config.jobworkerp_bin = PathBuf::from("/definitely/missing/jobworkerp");
        let outcome = reaper::reap_recorded(&data.sidecar_pids_path(), &data.sidecar_lock_path());
        *sidecars.instance_lock.lock() = outcome.lock;

        let result = sidecars
            .with_maintenance_lock(|| async {
                Err::<(), _>(AppError::Config("restore failed".into()))
            })
            .await;
        assert!(result.is_err());
        assert!(sidecars.lifecycle_lock.try_lock().is_ok());
    }

    #[tokio::test]
    async fn migration_failure_prevents_the_first_sidecar_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        let lance_home = data.lance_language_model_home();
        let sidecars = Sidecars::new(SidecarConfig {
            jobworkerp_bin: PathBuf::from("/definitely/missing/jobworkerp"),
            memories_bin: PathBuf::from("/definitely/missing/front"),
            memories_db_migrate_bin: PathBuf::from("/definitely/missing/memories-db-migrate"),
            startup_database_migration_enabled: true,
            conductor_bin: PathBuf::from("/definitely/missing/conductor"),
            data,
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
        });
        assert!(matches!(
            sidecars.start().await,
            Err(AppError::DatabaseMigrationFailed { phase, .. }) if phase == "schema validate"
        ));
        assert!(
            !sidecars.database_migration_in_progress(),
            "a failed migration gate must clear the startup UI signal"
        );
        assert!(sidecars.current_endpoints().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_start_runs_database_migration_gate_once() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().join("data"));
        data.ensure().unwrap();
        let command_log = tmp.path().join("migration-commands.log");
        let coordinator = tmp.path().join("memories-db-migrate");
        std::fs::write(
            &coordinator,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nsleep 0.02\nif [ \"$1 $2\" = 'schema status' ]; then echo 'schema_status status=managed pending_count=0'; fi\n",
                command_log.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&coordinator).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&coordinator, permissions).unwrap();

        // Fail after the migration gate so both callers finish promptly;
        // the command log still records every coordinator invocation.
        let sidecars = test_sidecars_with_migration(data, coordinator);

        let (first, second) = tokio::join!(sidecars.start(), sidecars.start());
        assert!(first.is_err());
        assert!(second.is_err());
        let invocations = std::fs::read_to_string(command_log)
            .unwrap()
            .lines()
            .count();
        assert_eq!(invocations, 11, "migration coordinator must run once");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_waits_for_start_before_releasing_instance_lock() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().join("data"));
        data.ensure().unwrap();
        let started_marker = tmp.path().join("migration-started");
        let coordinator = tmp.path().join("memories-db-migrate");
        std::fs::write(
            &coordinator,
            format!(
                "#!/bin/sh\nif [ ! -f '{}' ]; then : > '{}'; sleep 0.5; fi\nif [ \"$1 $2\" = 'schema status' ]; then echo 'schema_status status=managed pending_count=0'; fi\n",
                started_marker.display(),
                started_marker.display(),
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&coordinator).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&coordinator, permissions).unwrap();

        let sidecars = std::sync::Arc::new(test_sidecars_with_migration(data, coordinator));

        let start_task = {
            let sidecars = std::sync::Arc::clone(&sidecars);
            tokio::spawn(async move { sidecars.start().await })
        };
        for _ in 0..200 {
            if started_marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(started_marker.exists(), "startup gate did not begin");
        assert!(sidecars.owns_instance_lock());

        let mut stop_task = {
            let sidecars = std::sync::Arc::clone(&sidecars);
            tokio::spawn(async move { sidecars.stop().await })
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut stop_task)
                .await
                .is_err(),
            "stop must wait for an in-flight start"
        );

        assert!(start_task.await.unwrap().is_err());
        assert!(stop_task.await.unwrap().is_ok());
        assert!(!sidecars.owns_instance_lock());
    }

    #[tokio::test]
    async fn stop_clears_last_report_snapshot() {
        let sidecars = test_sidecars();
        // Seed a snapshot as a successful start would.
        *sidecars.last_report.lock() = Some(SidecarStartReport {
            endpoints: SidecarEndpoints {
                jobworkerp_port: 9000,
                memories_port: Some(9010),
                conductor_port: 9020,
                mcp_server_port: None,
            },
            warnings: Vec::new(),
        });
        assert!(sidecars.last_report().is_some());

        // stop() must drop the snapshot so a post-purge get_sidecar_status
        // can't promote a re-mounted frontend back to ready.
        sidecars.stop().await.unwrap();
        assert!(sidecars.last_report().is_none());
    }

    #[tokio::test]
    async fn failed_start_cleanup_keeps_the_completed_migration_receipt() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        std::fs::create_dir_all(data.database_migration_receipt_path().parent().unwrap()).unwrap();
        std::fs::write(
            data.database_migration_receipt_path(),
            br#"{"format_version":1,"migration_id":"thread-message-times-v1"}"#,
        )
        .unwrap();
        let lance_home = data.lance_language_model_home();
        let sidecars = Sidecars::new(SidecarConfig {
            jobworkerp_bin: PathBuf::from("/bin/true"),
            memories_bin: PathBuf::from("/bin/true"),
            memories_db_migrate_bin: PathBuf::from("/bin/true"),
            startup_database_migration_enabled: false,
            conductor_bin: PathBuf::from("/bin/true"),
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
        });

        sidecars.stop().await.unwrap();

        assert!(data.database_migration_receipt_path().is_file());
    }

    #[test]
    fn orphan_sweep_runs_only_after_a_new_successful_start() {
        assert!(Sidecars::should_spawn_startup_orphan_sweep(false, true));
        assert!(!Sidecars::should_spawn_startup_orphan_sweep(true, true));
        assert!(!Sidecars::should_spawn_startup_orphan_sweep(false, false));
    }

    #[tokio::test]
    async fn failed_start_does_not_leave_a_phantom_mcp_port() {
        // Regression: `active_mcp_port` used to be recorded BEFORE the spawn,
        // so a spawn failure (missing binary) `?`-returned from `start_inner`
        // without clearing it — `get_mcp_settings` then advertised an MCP URL
        // nothing was listening on. It must stay `None` on a failed start.
        //
        // The record point has since moved AGAIN: it is now set only after
        // every child (jobworkerp + memories + conductor) is up, because a
        // later `spawn_memories` / `spawn_conductor` `?`-return (jobworkerp
        // already healthy) likewise skips `stop_blocking` and would otherwise
        // strand the port. This test exercises the earliest failure (jobworkerp
        // spawn); the same `None`-on-failure invariant covers the later spawns
        // since the only `Some(_)` write now lives past all of them.
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().to_path_buf());
        // Enable MCP so the record path is exercised.
        crate::commands::mcp_settings::save_mcp_settings(
            &data.mcp_settings_path(),
            &crate::commands::mcp_settings::McpSettings {
                enabled: true,
                ..Default::default()
            },
        )
        .unwrap();
        let lance_home = data.lance_language_model_home();
        let sidecars = Sidecars::new(SidecarConfig {
            // A path that cannot spawn ⇒ `spawn_jobworkerp` errors before the
            // TCP health check, taking the early `?`-return path.
            jobworkerp_bin: PathBuf::from("/nonexistent/jobworkerp-bin"),
            memories_bin: PathBuf::from("/bin/true"),
            memories_db_migrate_bin: PathBuf::from("/bin/true"),
            startup_database_migration_enabled: false,
            conductor_bin: PathBuf::from("/bin/true"),
            data,
            worker_yaml_paths: Vec::new(),
            function_set_yaml_paths: Vec::new(),
            reflection_dispatch_enabled: false,
            auto_embedding_enabled: true,
            workflows_dir: None,
            lance_language_model_home: lance_home,
            lindera_dict_staged: false,
            llm_model: None,
            llm_hf_repo: None,
            llm_ctx_size: None,
            llm_kv_cache_type: None,
            env_file: None,
        });
        let result = sidecars.start().await;
        assert!(result.is_err(), "spawn of a missing binary must fail");
        assert_eq!(
            sidecars.active_mcp_port(),
            None,
            "a failed start must not advertise an MCP port"
        );
    }

    #[tokio::test]
    async fn stop_clears_last_start_error() {
        let sidecars = test_sidecars();
        *sidecars.last_start_error.lock() = Some("spawn failed".into());
        assert!(sidecars.last_start_error().is_some());
        // stop() clears the retained startup error so a post-stop /
        // post-purge get_model_status doesn't keep reporting `failed`.
        sidecars.stop().await.unwrap();
        assert!(sidecars.last_start_error().is_none());
    }

    #[tokio::test]
    async fn stop_clears_degraded_flag() {
        // A degraded start records `DegradedInfo`; a subsequent stop (or the
        // restart the user triggers after fixing the embedding dimension)
        // must clear it so the command-layer gate stops refusing dispatches.
        let sidecars = test_sidecars();
        *sidecars.degraded.lock() = Some(DegradedInfo {
            reason: "embedding_dimension_mismatch",
            expected_dim: 2048,
            actual_dim: 768,
        });
        assert!(sidecars.degraded().is_some());
        sidecars.stop().await.unwrap();
        assert!(sidecars.degraded().is_none());
    }

    #[test]
    fn plan_degraded_retry_recovers_dimension_mismatches() {
        use crate::sidecar::startup_error::StartupFailure;
        // The two dimension-mismatch codes are recoverable via a
        // vector-disabled restart; the info carries the dims through.
        let schema = AppError::SidecarStartupFailed(StartupFailure::LancedbSchemaMismatch {
            table: "memories".into(),
            uri: "/x".into(),
            expected_dim: 2048,
            actual_dim: 768,
            expected_fingerprint: String::new(),
            actual_fingerprint: String::new(),
        });
        let got = plan_degraded_retry(&schema).expect("schema mismatch is recoverable");
        assert_eq!(got.reason, "lancedb_schema_mismatch");
        assert_eq!((got.expected_dim, got.actual_dim), (2048, 768));

        let dim = AppError::SidecarStartupFailed(StartupFailure::EmbeddingDimensionMismatch {
            expected_dim: 1024,
            actual_dim: 512,
            runner_name: "memories-mm-embedding".into(),
        });
        let got = plan_degraded_retry(&dim).expect("embedding dim mismatch is recoverable");
        assert_eq!(got.reason, "embedding_dimension_mismatch");
        assert_eq!((got.expected_dim, got.actual_dim), (1024, 512));
    }

    #[test]
    fn plan_degraded_retry_leaves_other_failures_fatal() {
        use crate::sidecar::startup_error::StartupFailure;
        // Everything that is NOT a dimension mismatch stays fatal: a
        // vector-disabled restart would not fix these and would hide the
        // real problem behind a degraded banner.
        let fatal = [
            AppError::SidecarStartupFailed(StartupFailure::LancedbInitFailed {
                uri: "/x".into(),
                message: "permission denied".into(),
            }),
            AppError::SidecarStartupFailed(StartupFailure::RdbPoolInitFailed {
                url_sanitized: "sqlite:".into(),
                message: "unable to open".into(),
            }),
            AppError::SidecarStartupFailed(StartupFailure::EnvVarInvalid {
                name: "GRPC_ADDR".into(),
                message: "bad".into(),
            }),
            AppError::SidecarStartupFailed(StartupFailure::MediaConfigConflict {
                backend: "inline".into(),
                image_search_mode: "clip".into(),
            }),
            AppError::SidecarStartupFailed(StartupFailure::ConfigLoadFailed {
                component: "VectorDBConfig".into(),
                message: "x".into(),
            }),
            AppError::SidecarStartupFailed(StartupFailure::Other {
                component: "front".into(),
                message: "x".into(),
            }),
            // A non-startup error must also stay fatal.
            AppError::SidecarNotReady("nope".into()),
        ];
        for e in &fatal {
            assert!(
                plan_degraded_retry(e).is_none(),
                "must not degrade for {e:?}"
            );
        }
    }

    #[test]
    fn degraded_vector_env_overrides_env_file_enabling_vectors() {
        let mut cmd = Command::new("/bin/true");
        cmd.env("MEMORY_VECTOR_ENABLED", "true")
            .env("THREAD_VECTOR_ENABLED", "true")
            .env("REFLECTION_INTENT_VECTOR_ENABLED", "true")
            .env("MEMORY_VECTOR_SIZE", "2048");

        apply_memories_vector_env(&mut cmd, false, 768, Path::new("/tmp/memories.lancedb"));

        let env = cmd
            .as_std()
            .get_envs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            env.get(std::ffi::OsStr::new("MEMORY_VECTOR_ENABLED")),
            Some(&Some(std::ffi::OsStr::new("false")))
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("REFLECTION_INTENT_VECTOR_ENABLED")),
            Some(&Some(std::ffi::OsStr::new("false")))
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("THREAD_VECTOR_ENABLED")),
            Some(&Some(std::ffi::OsStr::new("false")))
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("MEMORY_VECTOR_SIZE")),
            Some(&None)
        );
    }

    #[test]
    fn enabled_vector_env_also_enables_thread_repository() {
        let mut cmd = Command::new("/bin/true");
        cmd.env("THREAD_VECTOR_ENABLED", "false");

        apply_memories_vector_env(&mut cmd, true, 768, Path::new("/tmp/memories.lancedb"));

        let env = cmd
            .as_std()
            .get_envs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            env.get(std::ffi::OsStr::new("THREAD_VECTOR_ENABLED")),
            Some(&Some(std::ffi::OsStr::new("true")))
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("MEMORY_VECTOR_SIZE")),
            Some(&Some(std::ffi::OsStr::new("768")))
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("MEMORY_LANCEDB_URI")),
            Some(&Some(std::ffi::OsStr::new("/tmp/memories.lancedb")))
        );
    }

    #[test]
    fn memories_command_removes_document_prefix_but_keeps_query_prefix() {
        let mut cmd = Command::new("/bin/true");
        cmd.env("MEMORY_EMBEDDING_DOCUMENT_PREFIX", "検索文書: ")
            .env("MEMORY_EMBEDDING_QUERY_PREFIX", "検索クエリ: ")
            .env(
                "LOOKBACK_EMBEDDING_MODEL_ID",
                "sirasagi62/ruri-v3-310m-ONNX",
            );

        strip_memories_document_embedding_prefix(&mut cmd);

        let env = cmd
            .as_std()
            .get_envs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            env.get(std::ffi::OsStr::new("MEMORY_EMBEDDING_DOCUMENT_PREFIX")),
            Some(&None)
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("MEMORY_EMBEDDING_QUERY_PREFIX")),
            Some(&Some(std::ffi::OsStr::new("検索クエリ: ")))
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("LOOKBACK_EMBEDDING_MODEL_ID")),
            Some(&Some(std::ffi::OsStr::new("sirasagi62/ruri-v3-310m-ONNX")))
        );
    }

    #[tokio::test]
    async fn stop_clears_last_start_failure_snapshot() {
        // The structured snapshot must follow the same lifecycle as
        // `last_start_error`: a post-stop `get_sidecar_status` from a
        // re-mounted frontend should NOT keep surfacing the previous
        // failure's BootError after the user picked a recovery action.
        let sidecars = test_sidecars();
        *sidecars.last_start_failure.lock() =
            Some(crate::sidecar::startup_error::SidecarErrorPayload::Raw {
                message: "stale".into(),
            });
        assert!(sidecars.last_start_failure().is_some());
        sidecars.stop().await.unwrap();
        assert!(sidecars.last_start_failure().is_none());
    }

    #[test]
    fn start_with_warnings_records_structured_failure_snapshot() {
        // Pin the lifecycle parity: when `start_with_warnings` returns
        // `Err`, the structured snapshot must carry the same code the
        // `sidecar://error` event would emit. The mount-time
        // `get_sidecar_status` then reads identical data to what a
        // listener would have seen, closing the boot-time race window.
        use crate::sidecar::startup_error::{SidecarErrorPayload, StartupFailure};
        let sidecars = test_sidecars();
        let failure = StartupFailure::LancedbSchemaMismatch {
            table: "memories".into(),
            uri: "/tmp/x".into(),
            expected_dim: 2048,
            actual_dim: 768,
            expected_fingerprint: String::new(),
            actual_fingerprint: String::new(),
        };
        let payload =
            SidecarErrorPayload::from_app_error(&AppError::SidecarStartupFailed(failure.clone()));
        // Drive the same write `start_with_warnings` does on Err. We
        // exercise the lift directly here because spinning a real spawn
        // failure inside a unit test requires the full bin layout.
        *sidecars.last_start_failure.lock() = Some(payload);

        match sidecars.last_start_failure().expect("snapshot present") {
            SidecarErrorPayload::Structured { failure: snap } => assert_eq!(snap, failure),
            other => panic!("expected Structured, got {other:?}"),
        }
    }

    fn report_with_warning(kind: SidecarWarningKind, msg: &str) -> SidecarStartReport {
        SidecarStartReport {
            endpoints: SidecarEndpoints {
                jobworkerp_port: 9000,
                memories_port: Some(9010),
                conductor_port: 9020,
                mcp_server_port: None,
            },
            warnings: vec![SidecarWarning {
                kind,
                message: msg.to_string(),
                detail: None,
            }],
        }
    }

    #[test]
    fn llm_blocking_error_none_when_clean() {
        let sidecars = test_sidecars();
        assert!(sidecars.llm_blocking_error().is_none());
    }

    #[test]
    fn llm_blocking_error_prefers_hard_startup_failure() {
        let sidecars = test_sidecars();
        *sidecars.last_start_error.lock() = Some("tcp timeout".into());
        *sidecars.last_report.lock() = Some(report_with_warning(
            SidecarWarningKind::WorkerApplyFailed,
            "w",
        ));
        // Fatal startup error wins over a non-fatal warning.
        assert_eq!(
            sidecars.llm_blocking_error().as_deref(),
            Some("tcp timeout")
        );
    }

    #[test]
    fn llm_blocking_error_picks_worker_or_plugin_warning() {
        for kind in [
            SidecarWarningKind::WorkerApplyFailed,
            SidecarWarningKind::PluginsStageFailed,
        ] {
            let sidecars = test_sidecars();
            *sidecars.last_report.lock() = Some(report_with_warning(kind, "dylib missing"));
            assert_eq!(
                sidecars.llm_blocking_error().as_deref(),
                Some("dylib missing")
            );
        }
    }

    #[test]
    fn sidecar_log_profile_uses_info_for_dev_and_warn_for_release() {
        let dev = default_sidecar_log(true);
        let release = default_sidecar_log(false);

        assert!(
            dev.starts_with("info,"),
            "dev sidecars should use info: {dev}"
        );
        assert!(
            release.starts_with("warn,"),
            "release sidecars should use warn: {release}"
        );
    }

    #[test]
    fn sidecar_log_profiles_keep_noisy_target_suppressions() {
        for profile in [default_sidecar_log(true), default_sidecar_log(false)] {
            for target in ["app=warn", "worker_app=warn", "lance=warn"] {
                assert!(profile.contains(target), "{target} missing from {profile}");
            }
        }
    }

    #[test]
    fn sidecar_log_override_applies_to_both_profiles() {
        for debug_assertions in [true, false] {
            assert_eq!(
                effective_rust_log_for(Some("trace"), debug_assertions),
                "trace"
            );
        }
    }

    #[test]
    fn lookback_rust_log_override_applies_to_both_sidecars() {
        let saved = std::env::var("LOOKBACK_RUST_LOG").ok();
        unsafe { std::env::set_var("LOOKBACK_RUST_LOG", "trace") };

        assert_eq!(effective_rust_log(), "trace");

        match saved {
            Some(v) => unsafe { std::env::set_var("LOOKBACK_RUST_LOG", v) },
            None => unsafe { std::env::remove_var("LOOKBACK_RUST_LOG") },
        }
    }

    #[test]
    fn apply_external_llm_env_sets_and_clears_vars() {
        // Env-touching test: relies on `--test-threads=1`. Save / restore
        // anything we mutate so neighbouring tests see a clean environment.
        let saved_model = std::env::var("LOOKBACK_EXTERNAL_LLM_MODEL").ok();
        let saved_base = std::env::var("LOOKBACK_EXTERNAL_LLM_BASE_URL").ok();

        // 1) Setting both populates the env vars verbatim. This is the path
        //    the External-LLM YAML loader (Tauri-side `expand_env`) needs —
        //    without it the placeholder fell back to `gpt-4o` and a user's
        //    Gemini selection silently routed to OpenAI.
        unsafe { apply_external_llm_env(Some("gemini-3.1-flash-lite"), Some("https://x.test/v1")) };
        assert_eq!(
            std::env::var("LOOKBACK_EXTERNAL_LLM_MODEL").unwrap(),
            "gemini-3.1-flash-lite"
        );
        assert_eq!(
            std::env::var("LOOKBACK_EXTERNAL_LLM_BASE_URL").unwrap(),
            "https://x.test/v1"
        );

        // 2) None / empty must REMOVE the var — otherwise a switch back to
        //    Local mode would leak the last External model name into the
        //    next YAML expansion (and into the LLM runner via env merge).
        unsafe { apply_external_llm_env(None, Some("")) };
        assert!(std::env::var("LOOKBACK_EXTERNAL_LLM_MODEL").is_err());
        assert!(std::env::var("LOOKBACK_EXTERNAL_LLM_BASE_URL").is_err());

        unsafe { apply_external_llm_env(Some(""), None) };
        assert!(std::env::var("LOOKBACK_EXTERNAL_LLM_MODEL").is_err());
        assert!(std::env::var("LOOKBACK_EXTERNAL_LLM_BASE_URL").is_err());

        // Restore the original environment.
        unsafe {
            match saved_model {
                Some(v) => std::env::set_var("LOOKBACK_EXTERNAL_LLM_MODEL", v),
                None => std::env::remove_var("LOOKBACK_EXTERNAL_LLM_MODEL"),
            }
            match saved_base {
                Some(v) => std::env::set_var("LOOKBACK_EXTERNAL_LLM_BASE_URL", v),
                None => std::env::remove_var("LOOKBACK_EXTERNAL_LLM_BASE_URL"),
            }
        }
    }

    #[test]
    fn apply_local_llm_env_sets_and_clears_vars() {
        // Regression for the bug where switching presets in Settings
        // (e.g. Qwen3.6-27B → Qwen3.6-35B-A3B) had no effect because the
        // YAML placeholders `%{LOOKBACK_LLM_MODEL:-…}` were resolved against
        // the Tauri process env — which had never been populated for the
        // Local-LLM case (only the jobworkerp CHILD env was). The placeholder
        // therefore always fell back to the YAML default and the upsert
        // shipped the default model to `memories-llm`.
        let saved_model = std::env::var("LOOKBACK_LLM_MODEL").ok();
        let saved_repo = std::env::var("LOOKBACK_LLM_HF_REPO").ok();
        let saved_ctx = std::env::var("LOOKBACK_LLM_CTX_SIZE").ok();
        let saved_kv = std::env::var("LOOKBACK_LLM_KV_CACHE_TYPE").ok();
        let saved_mtp_enabled = std::env::var("LOOKBACK_LLM_MTP_ENABLED").ok();
        let saved_mtp_draft = std::env::var("LOOKBACK_LLM_MTP_DRAFT_MODEL").ok();

        // 1) Setting all local LLM fields populates the env vars verbatim.
        unsafe {
            apply_local_llm_env(
                Some("Qwen3.6-35B-A3B-UD-IQ4_NL.gguf"),
                Some("unsloth/Qwen3.6-35B-A3B-GGUF"),
                Some(262_144),
                Some("KV_CACHE_TYPE_Q4_0"),
                Some(true),
                Some("mtp-draft.gguf"),
            )
        };
        assert_eq!(
            std::env::var("LOOKBACK_LLM_MODEL").unwrap(),
            "Qwen3.6-35B-A3B-UD-IQ4_NL.gguf"
        );
        assert_eq!(
            std::env::var("LOOKBACK_LLM_HF_REPO").unwrap(),
            "unsloth/Qwen3.6-35B-A3B-GGUF"
        );
        assert_eq!(std::env::var("LOOKBACK_LLM_CTX_SIZE").unwrap(), "262144");
        assert_eq!(
            std::env::var("LOOKBACK_LLM_KV_CACHE_TYPE").unwrap(),
            "KV_CACHE_TYPE_Q4_0"
        );
        assert_eq!(std::env::var("LOOKBACK_LLM_MTP_ENABLED").unwrap(), "true");
        assert_eq!(
            std::env::var("LOOKBACK_LLM_MTP_DRAFT_MODEL").unwrap(),
            "mtp-draft.gguf"
        );

        // 2) None / empty must REMOVE the var so a later restart with a
        //    reverted (default) selection does not see a stale model name.
        unsafe { apply_local_llm_env(None, None, None, None, None, None) };
        assert!(std::env::var("LOOKBACK_LLM_MODEL").is_err());
        assert!(std::env::var("LOOKBACK_LLM_HF_REPO").is_err());
        assert!(std::env::var("LOOKBACK_LLM_CTX_SIZE").is_err());
        assert!(std::env::var("LOOKBACK_LLM_KV_CACHE_TYPE").is_err());
        assert!(std::env::var("LOOKBACK_LLM_MTP_ENABLED").is_err());
        assert!(std::env::var("LOOKBACK_LLM_MTP_DRAFT_MODEL").is_err());

        unsafe { apply_local_llm_env(Some(""), Some(""), None, Some(""), Some(false), Some("")) };
        assert!(std::env::var("LOOKBACK_LLM_MODEL").is_err());
        assert!(std::env::var("LOOKBACK_LLM_HF_REPO").is_err());
        assert!(std::env::var("LOOKBACK_LLM_CTX_SIZE").is_err());
        assert!(std::env::var("LOOKBACK_LLM_KV_CACHE_TYPE").is_err());
        assert_eq!(std::env::var("LOOKBACK_LLM_MTP_ENABLED").unwrap(), "false");
        assert!(std::env::var("LOOKBACK_LLM_MTP_DRAFT_MODEL").is_err());

        // Restore the original environment.
        unsafe {
            match saved_model {
                Some(v) => std::env::set_var("LOOKBACK_LLM_MODEL", v),
                None => std::env::remove_var("LOOKBACK_LLM_MODEL"),
            }
            match saved_repo {
                Some(v) => std::env::set_var("LOOKBACK_LLM_HF_REPO", v),
                None => std::env::remove_var("LOOKBACK_LLM_HF_REPO"),
            }
            match saved_ctx {
                Some(v) => std::env::set_var("LOOKBACK_LLM_CTX_SIZE", v),
                None => std::env::remove_var("LOOKBACK_LLM_CTX_SIZE"),
            }
            match saved_kv {
                Some(v) => std::env::set_var("LOOKBACK_LLM_KV_CACHE_TYPE", v),
                None => std::env::remove_var("LOOKBACK_LLM_KV_CACHE_TYPE"),
            }
            match saved_mtp_enabled {
                Some(v) => std::env::set_var("LOOKBACK_LLM_MTP_ENABLED", v),
                None => std::env::remove_var("LOOKBACK_LLM_MTP_ENABLED"),
            }
            match saved_mtp_draft {
                Some(v) => std::env::set_var("LOOKBACK_LLM_MTP_DRAFT_MODEL", v),
                None => std::env::remove_var("LOOKBACK_LLM_MTP_DRAFT_MODEL"),
            }
        }
    }

    #[test]
    fn apply_embedding_env_sets_and_clears_vars() {
        let keys = crate::commands::embedding_settings::EMBEDDING_ENV_KEYS;
        let saved: Vec<(&str, Option<String>)> =
            keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();

        // 1) Full set populates every key.
        let vars: Vec<(&'static str, String)> = vec![
            (
                "LOOKBACK_EMBEDDING_MODEL_ID",
                "Qwen/Qwen3-Embedding-4B".into(),
            ),
            (
                "LOOKBACK_EMBEDDING_TOKENIZER_ID",
                "Qwen/Qwen3-Tokenizer".into(),
            ),
            ("LOOKBACK_EMBEDDING_DTYPE", "BF16".into()),
            ("LOOKBACK_EMBEDDING_MAX_SEQ_LEN", "32768".into()),
            ("LOOKBACK_EMBEDDING_VECTOR_SIZE", "2560".into()),
        ];
        unsafe { apply_embedding_env(&vars) };
        assert_eq!(
            std::env::var("LOOKBACK_EMBEDDING_MODEL_ID").unwrap(),
            "Qwen/Qwen3-Embedding-4B"
        );
        assert_eq!(
            std::env::var("LOOKBACK_EMBEDDING_TOKENIZER_ID").unwrap(),
            "Qwen/Qwen3-Tokenizer"
        );
        assert_eq!(std::env::var("LOOKBACK_EMBEDDING_DTYPE").unwrap(), "BF16");
        assert_eq!(
            std::env::var("LOOKBACK_EMBEDDING_VECTOR_SIZE").unwrap(),
            "2560"
        );

        // 2) Re-applying WITHOUT the tokenizer entry must REMOVE the
        //    leftover (a settings save that clears the field would
        //    otherwise leak the previous tokenizer into the next restart).
        let vars2: Vec<(&'static str, String)> = vec![
            (
                "LOOKBACK_EMBEDDING_MODEL_ID",
                "Qwen/Qwen3-VL-Embedding-2B".into(),
            ),
            ("LOOKBACK_EMBEDDING_DTYPE", "F16".into()),
            ("LOOKBACK_EMBEDDING_MAX_SEQ_LEN", "8192".into()),
            ("LOOKBACK_EMBEDDING_VECTOR_SIZE", "2048".into()),
        ];
        unsafe { apply_embedding_env(&vars2) };
        assert!(std::env::var("LOOKBACK_EMBEDDING_TOKENIZER_ID").is_err());
        assert_eq!(
            std::env::var("LOOKBACK_EMBEDDING_MODEL_ID").unwrap(),
            "Qwen/Qwen3-VL-Embedding-2B"
        );

        // 3) Empty input clears every managed key.
        unsafe { apply_embedding_env(&[]) };
        for k in keys {
            assert!(
                std::env::var(k).is_err(),
                "{k} should be unset after empty apply"
            );
        }

        // Restore the original environment.
        unsafe {
            for (k, v) in saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[tokio::test]
    async fn start_records_last_start_error_on_spawn_failure() {
        // A non-existent jobworkerp binary makes spawn fail fast (no 30s TCP
        // wait), so `start_with_warnings` returns Err and the message is
        // retained for get_model_status to surface as `failed`.
        let data = DataPaths::with_root(
            std::env::temp_dir().join(format!("lookback-start-err-{}", std::process::id())),
        );
        let lance_home = data.lance_language_model_home();
        let sidecars = Sidecars::new(SidecarConfig {
            jobworkerp_bin: PathBuf::from("/nonexistent/all-in-one-xyz"),
            memories_bin: PathBuf::from("/nonexistent/front-xyz"),
            memories_db_migrate_bin: PathBuf::from("/bin/true"),
            startup_database_migration_enabled: false,
            conductor_bin: PathBuf::from("/nonexistent/conductor-main-xyz"),
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
        });

        let res = sidecars.start_with_warnings(Vec::new()).await;
        assert!(res.is_err(), "spawn of a missing binary must fail");
        assert!(
            sidecars.last_start_error().is_some(),
            "hard startup failure must be retained for get_model_status"
        );
        // last_report stays None on a hard failure (no successful endpoints).
        assert!(sidecars.last_report().is_none());

        let native_root = data.log_dir().join("native");
        let native_dirs = std::fs::read_dir(native_root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        assert!(
            native_dirs
                .iter()
                .any(|path| path.join(".closed").is_file()),
            "spawn failure must close its native scope"
        );
        assert!(
            native_dirs
                .iter()
                .all(|path| !path.join(".active").exists())
        );

        let _ = std::fs::remove_dir_all(&data.root);
    }

    #[test]
    fn jobworkerp_env_names_match_yaml_placeholders() {
        // Pinning regression. The jobworkerp child reads model/repo/ctx via
        // env-var expansion in `workers/llm-workers.yaml`, where the
        // placeholders are `%{LOOKBACK_LLM_MODEL:-…}` /
        // `%{LOOKBACK_LLM_HF_REPO:-…}` / `%{LOOKBACK_LLM_CTX_SIZE:-…}`.
        // An earlier version injected these as `MEMORIES_LLM_MODEL` /
        // `MEMORIES_LLM_HF_REPO`, which the YAML never references — so
        // setting `llm_model` via UI / env silently had no effect and the
        // hardcoded default was always loaded. Lock in the names here.
        let envs = jobworkerp_llm_env_vars(
            Some("Qwen3.5-9B-UD-Q4_K_XL.gguf"),
            Some("unsloth/Qwen3.5-9B-GGUF"),
            Some(32_768),
            Some("KV_CACHE_TYPE_Q4_0"),
            Some(true),
            Some("mtp-draft.gguf"),
        );
        let map: std::collections::HashMap<&str, String> = envs.into_iter().collect();
        assert_eq!(
            map.get("LOOKBACK_LLM_MODEL").map(String::as_str),
            Some("Qwen3.5-9B-UD-Q4_K_XL.gguf"),
        );
        assert_eq!(
            map.get("LOOKBACK_LLM_HF_REPO").map(String::as_str),
            Some("unsloth/Qwen3.5-9B-GGUF"),
        );
        assert_eq!(
            map.get("LOOKBACK_LLM_CTX_SIZE").map(String::as_str),
            Some("32768"),
        );
        assert_eq!(
            map.get("LOOKBACK_LLM_KV_CACHE_TYPE").map(String::as_str),
            Some("KV_CACHE_TYPE_Q4_0"),
        );
        assert_eq!(
            map.get("LOOKBACK_LLM_MTP_ENABLED").map(String::as_str),
            Some("true"),
        );
        assert_eq!(
            map.get("LOOKBACK_LLM_MTP_DRAFT_MODEL").map(String::as_str),
            Some("mtp-draft.gguf"),
        );
        // Names that shouldn't appear — defensive.
        assert!(!map.contains_key("MEMORIES_LLM_MODEL"));
        assert!(!map.contains_key("MEMORIES_LLM_HF_REPO"));
    }

    #[test]
    fn jobworkerp_env_omits_unset_fields() {
        // A pre-feature user / a fresh launch with no overrides must not
        // inject empty env vars (`LOOKBACK_LLM_MODEL=""` would override
        // the YAML's `:-default` with an empty string and the plugin
        // would fail to resolve any model).
        let envs = jobworkerp_llm_env_vars(None, None, None, None, None, None);
        assert!(envs.is_empty());
    }

    #[test]
    fn jobworkerp_env_omits_empty_string_overrides() {
        // Empty string overrides come from a fat-fingered Settings save
        // before our validation gate ran. Treat them as "unset" so the
        // YAML's `:-default` survives instead of being clobbered.
        let envs = jobworkerp_llm_env_vars(Some(""), Some(""), None, Some(""), None, Some(""));
        assert!(envs.is_empty());
    }

    #[test]
    fn jobworkerp_env_ctx_size_serialises_as_decimal() {
        // The YAML expects a stringified integer for the unquoted
        // `ctx_size: "%{...}"` placeholder; the proto-JSON coercion
        // accepts the decimal form but rejects hex / underscores /
        // floats.
        let envs = jobworkerp_llm_env_vars(None, None, Some(262_144), None, None, None);
        let v = envs
            .into_iter()
            .find(|(k, _)| *k == "LOOKBACK_LLM_CTX_SIZE")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(v, "262144");
    }
}
