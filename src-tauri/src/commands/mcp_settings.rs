//! MCP server settings: persisted enable flag + advanced overrides, runtime
//! resolution, and the spawn-time env vars that turn jobworkerp's in-process
//! MCP HTTP server on or off.
//!
//! Mirrors [`super::embedding_settings`] in shape (load → resolve → env-inject
//! → restart) so the Settings cards share their pattern. Unlike embedding,
//! MCP carries NO vectordb concern — toggling it is a pure sidecar restart.
//!
//! The jobworkerp `all-in-one` binary reads `MCP_ENABLED` at spawn time
//! (`worker-main/src/main.rs`) and, when enabled, boots an MCP HTTP server
//! ALONGSIDE the gRPC front (`tokio::join!`), so enabling MCP never disturbs
//! the gRPC clients agent-app uses for browsing / import / chat. The exposed
//! tool surface is narrowed to a single FunctionSet via the `MCP_SET_NAME`
//! env var (every MCP knob is `MCP_`-prefixed in the implementation, see
//! `mcp-server/src/config.rs`).
//!
//! Because `MCP_ENABLED` only takes effect at process spawn, a toggle CANNOT
//! be hot-reloaded; the change pipeline always restarts the sidecar. This is
//! enforced by `apply_settings`'s `is_llm_only_change` guard.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::data::DataPaths;
use crate::error::{AppError, AppResult};

const MCP_TOKEN_KEYRING_SERVICE: &str = "lookback";
const MCP_TOKEN_KEYRING_ACCOUNT: &str = "mcp-auth-token";
const MCP_TOKEN_BYTES: usize = 32;

/// Cache only confirmed keyring states to avoid repeated platform prompts
/// during a sidecar restart. The token never enters settings or responses.
static MCP_TOKEN_CACHE: RwLock<Option<Option<String>>> = RwLock::new(None);

#[cfg(test)]
pub(crate) static MCP_KEYRING_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
static MCP_KEYRING_TEST_INIT: std::sync::Once = std::sync::Once::new();

/// Composite FunctionSet exposed to MCP clients when the server is enabled.
/// It is defined in `workers/function-sets.yaml` and kept SEPARATE from the
/// chat-loop's `lookback-rag` set so the MCP-visible tool surface can grow
/// independently of the in-app chat tool set. The set includes RAG retrieval
/// and curated `lookback_issue_*` workers. Passed to jobworkerp via the `MCP_SET_NAME`
/// env var (not to be confused with this constant's name, which is the
/// function-set VALUE, not the env key).
pub const MCP_FUNCTION_SET_NAME: &str = "lookback-mcp";

/// Preferred bind port for the MCP HTTP server. Lives in the private port
/// range and deliberately avoids the jobworkerp sidecar trio (9000 / 9010 /
/// 9020), the mcp-server default (8000), and common dev servers
/// (3000 / 5173 / 8080) so an external MCP client gets a predictable, rarely-
/// clashing target. `ports::pick` falls back to an OS-assigned port if this
/// one is already taken, and the actual bound port is surfaced to the UI.
pub const MCP_DEFAULT_PORT: u16 = 39010;

/// Every jobworkerp env var the MCP card OWNS. `mcp_env_vars` emits only the
/// keys the user actually set, so the spawn path must CLEAR the rest from the
/// child's inherited environment first — otherwise a value in the parent
/// process env or the `.env` template (e.g. `MCP_TIMEOUT_SEC=999999`)
/// would silently win over the "unset ⇒ jobworkerp default" the settings file
/// expresses, bypassing the UI / validation. `MCP_ADDR` is intentionally NOT
/// listed: the lifecycle always sets it to the resolved bound port, so it is
/// never left to inheritance. Every knob is `MCP_`-prefixed to match the
/// implementation (`mcp-server/src/config.rs`). Keep in sync with `mcp_env_vars`.
pub const MCP_MANAGED_ENV_KEYS: &[&str] = &[
    "MCP_ENABLED",
    "MCP_SET_NAME",
    "MCP_EXCLUDE_RUNNER",
    "MCP_EXCLUDE_WORKER",
    "MCP_STREAMING",
    "MCP_TIMEOUT_SEC",
    "MCP_AUTH_ENABLED",
    "MCP_AUTH_TOKEN_FILE",
    "LOOKBACK_MCP_RAG_DEFAULT_PERIOD_SCOPE",
];

/// `MCP_TIMEOUT_SEC` upper guard — a typo'd huge value would let a hung
/// tool call wedge the MCP client indefinitely.
const REQUEST_TIMEOUT_SEC_MAX: u32 = 3600;

/// The period-summary layers included by an MCP recall when its caller does
/// not supply an explicit `summary_scope:*` label. This is a retrieval
/// default, not an authorization boundary: raw memories remain searchable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpRagDefaultPeriodScope {
    #[default]
    OverallOnly,
    OverallAndPath,
}

impl McpRagDefaultPeriodScope {
    fn as_env_value(self) -> &'static str {
        match self {
            Self::OverallOnly => "overall_only",
            Self::OverallAndPath => "overall_and_path",
        }
    }
}

/// Persisted (non-secret) MCP server config.
///
/// `enabled` defaults to `false`, and every advanced field is `Option` so a
/// missing `mcp-settings.json` (every install before this feature) and a
/// `{}` document both deserialise to "MCP off, jobworkerp defaults" — the
/// exact behaviour the app shipped with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct McpSettings {
    #[serde(default)]
    pub enabled: bool,
    /// When enabled, start the MCP listener without Bearer authentication.
    /// This remains opt-in so settings created before this option stay secure.
    #[serde(default)]
    pub allow_unauthenticated_requests: bool,
    /// `Some(_)` ⇒ emit `MCP_EXCLUDE_RUNNER`; `None` ⇒ leave jobworkerp's
    /// default (false). Kept optional so we never pin a value the user did
    /// not choose.
    #[serde(default)]
    pub exclude_runner_as_tool: Option<bool>,
    #[serde(default)]
    pub exclude_worker_as_tool: Option<bool>,
    /// `Some(_)` ⇒ emit `MCP_STREAMING`; `None` ⇒ jobworkerp default (true).
    #[serde(default)]
    pub streaming: Option<bool>,
    /// `Some(_)` ⇒ emit `MCP_TIMEOUT_SEC`; `None` ⇒ jobworkerp default (60).
    #[serde(default)]
    pub request_timeout_sec: Option<u32>,
    /// Default v2 period-summary rollup scope for direct MCP recall calls.
    /// Missing values in existing settings files deliberately resolve to
    /// `overall_only` to avoid changing broad-recall behavior on upgrade.
    #[serde(default)]
    pub rag_default_period_scope: McpRagDefaultPeriodScope,
}

/// Frontend-facing response. Adds derived fields the UI renders without a
/// second roundtrip.
#[derive(Debug, Clone, Serialize)]
pub struct McpSettingsResponse {
    pub enabled: bool,
    pub allow_unauthenticated_requests: bool,
    pub exclude_runner_as_tool: Option<bool>,
    pub exclude_worker_as_tool: Option<bool>,
    pub streaming: Option<bool>,
    pub request_timeout_sec: Option<u32>,
    pub rag_default_period_scope: McpRagDefaultPeriodScope,
    /// FunctionSet name exposed over MCP — constant, surfaced so the UI can
    /// show it in the client-config hint without hardcoding the value twice.
    pub set_name: String,
    /// The MCP server's actual bound port when it is currently running,
    /// else `None` (server off, or sidecars stopped). Surfaced so the UI can
    /// print the connection URL for the user to paste into their MCP client.
    pub active_port: Option<u16>,
    /// Whether a bearer token is available, without exposing its value.
    pub auth_token_configured: bool,
    /// Runtime readiness of the listener and its fixed FunctionSet.
    pub runtime_status: crate::sidecar::McpRuntimeStatus,
    /// Safe diagnostic text when the runtime status is unavailable.
    pub unavailable_reason: Option<String>,
}

/// Request from the frontend.
#[derive(Debug, Clone, Deserialize)]
pub struct SetMcpSettingsRequest {
    pub enabled: bool,
    /// Explicitly allow requests without a Bearer token.
    #[serde(default)]
    pub allow_unauthenticated_requests: bool,
    /// Request a replacement bearer token as part of the next settings save.
    /// This is deliberately non-persistent: the token itself lives only in
    /// the OS keyring and changing it requires a jobworkerp restart.
    #[serde(default)]
    pub rotate_auth_token: bool,
    #[serde(default)]
    pub exclude_runner_as_tool: Option<bool>,
    #[serde(default)]
    pub exclude_worker_as_tool: Option<bool>,
    #[serde(default)]
    pub streaming: Option<bool>,
    #[serde(default)]
    pub request_timeout_sec: Option<u32>,
    #[serde(default)]
    pub rag_default_period_scope: McpRagDefaultPeriodScope,
}

impl SetMcpSettingsRequest {
    fn into_settings(self) -> McpSettings {
        McpSettings {
            enabled: self.enabled,
            allow_unauthenticated_requests: self.allow_unauthenticated_requests,
            exclude_runner_as_tool: self.exclude_runner_as_tool,
            exclude_worker_as_tool: self.exclude_worker_as_tool,
            streaming: self.streaming,
            request_timeout_sec: self.request_timeout_sec,
            rag_default_period_scope: self.rag_default_period_scope,
        }
    }
}

/// Resolved runtime values consumed by `sidecar/lifecycle.rs`. `set_name` is
/// constant (there is no per-user choice), carried here so the env producer
/// and the UI hint share one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRuntime {
    pub enabled: bool,
    pub allow_unauthenticated_requests: bool,
    pub set_name: String,
    pub exclude_runner_as_tool: Option<bool>,
    pub exclude_worker_as_tool: Option<bool>,
    pub streaming: Option<bool>,
    pub request_timeout_sec: Option<u32>,
    pub rag_default_period_scope: McpRagDefaultPeriodScope,
}

/// Project `McpSettings` into runtime values. Pure so the (small) mapping is
/// unit-testable. No env-override path: unlike embedding, MCP has no dev
/// shell knobs — the file is the only source.
pub fn resolve_mcp_runtime(settings: &McpSettings) -> McpRuntime {
    McpRuntime {
        enabled: settings.enabled,
        allow_unauthenticated_requests: settings.allow_unauthenticated_requests,
        set_name: MCP_FUNCTION_SET_NAME.to_string(),
        exclude_runner_as_tool: settings.exclude_runner_as_tool,
        exclude_worker_as_tool: settings.exclude_worker_as_tool,
        streaming: settings.streaming,
        request_timeout_sec: settings.request_timeout_sec,
        rag_default_period_scope: settings.rag_default_period_scope,
    }
}

/// Spawn-time env vars for the jobworkerp child.
///
/// Disabled ⇒ only `MCP_ENABLED=false` (jobworkerp boots gRPC-only). Enabled
/// ⇒ `MCP_ENABLED=true` + `MCP_SET_NAME`, plus each advanced key ONLY when
/// the user set it — an unset advanced field must not pin jobworkerp's
/// default to a literal, so the key is simply omitted.
///
/// `MCP_ADDR` is intentionally NOT produced here — the lifecycle attaches it
/// separately (see the `MCP_ADDR` comment in `spawn_jobworkerp`). Pure so the
/// on/off matrix is unit-testable.
pub fn mcp_env_vars(runtime: &McpRuntime) -> Vec<(&'static str, String)> {
    if !runtime.enabled {
        return vec![("MCP_ENABLED", "false".to_string())];
    }
    let mut out: Vec<(&'static str, String)> = vec![
        ("MCP_ENABLED", "true".to_string()),
        ("MCP_SET_NAME", runtime.set_name.clone()),
        (
            "LOOKBACK_MCP_RAG_DEFAULT_PERIOD_SCOPE",
            runtime.rag_default_period_scope.as_env_value().to_string(),
        ),
    ];
    if let Some(v) = runtime.exclude_runner_as_tool {
        out.push(("MCP_EXCLUDE_RUNNER", v.to_string()));
    }
    if let Some(v) = runtime.exclude_worker_as_tool {
        out.push(("MCP_EXCLUDE_WORKER", v.to_string()));
    }
    if let Some(v) = runtime.streaming {
        out.push(("MCP_STREAMING", v.to_string()));
    }
    if let Some(v) = runtime.request_timeout_sec {
        out.push(("MCP_TIMEOUT_SEC", v.to_string()));
    }
    out
}

// ── persistence ──────────────────────────────────────────────────────

pub fn load_mcp_settings(path: &Path) -> McpSettings {
    let Ok(bytes) = std::fs::read(path) else {
        return McpSettings::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save_mcp_settings(path: &Path, settings: &McpSettings) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| AppError::Config(format!("serialize mcp settings: {e}")))?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Disable only a legacy MCP configuration that cannot start safely because
/// it has no bearer token and did not explicitly opt into unauthenticated use.
pub fn migrate_legacy_enabled_mcp_without_auth_token(
    path: &Path,
    auth_token_configured: bool,
) -> AppResult<bool> {
    let mut settings = load_mcp_settings(path);
    if !settings.enabled || settings.allow_unauthenticated_requests || auth_token_configured {
        return Ok(false);
    }

    settings.enabled = false;
    save_mcp_settings(path, &settings)?;
    Ok(true)
}

fn validate_set_request(req: &SetMcpSettingsRequest) -> Result<(), String> {
    if let Some(t) = req.request_timeout_sec
        && !(1..=REQUEST_TIMEOUT_SEC_MAX).contains(&t)
    {
        return Err(format!(
            "invalid request_timeout_sec {t}: must be in [1, {REQUEST_TIMEOUT_SEC_MAX}]"
        ));
    }
    Ok(())
}

/// Validate WITHOUT persisting. Split out so the unified `apply_settings` can
/// validate the whole batch before any file is written.
///
/// MCP is NOT gated by connection mode. The MCP server runs inside the LOCAL
/// jobworkerp sidecar, which is always up regardless of remote-browse mode, so
/// it can be enabled in either mode. The endpoint the MCP-exposed
/// `lookback_recall` searches follows the active connection (resolved into the
/// workflow defaults at sidecar start — see `lifecycle::apply_rag_memories_env`),
/// so in remote mode MCP search hits the configured remote memories.
pub fn validate_mcp_request(req: &SetMcpSettingsRequest) -> AppResult<()> {
    validate_set_request(req).map_err(AppError::Config)?;
    if req.rotate_auth_token && !req.enabled {
        return Err(AppError::Config(
            "MCP が無効な間は token を再生成できません。初回生成を使用してください".to_string(),
        ));
    }
    if req.enabled && !req.allow_unauthenticated_requests && !req.rotate_auth_token {
        require_mcp_auth_token()?;
    }
    Ok(())
}

/// Outcome of persisting MCP settings to disk WITHOUT restarting the sidecar.
pub struct McpApplyOutcome {
    /// `false` ⇒ no-op (old == new); the caller should skip the restart.
    pub changed: bool,
    /// Pre-save settings, retained so the caller can roll the file back.
    pub old_settings: McpSettings,
}

/// Validate, then persist `mcp-settings.json` — WITHOUT restarting the
/// sidecar. The caller owns the stop → restart → rollback sequence so MCP can
/// share the unified single restart with the other settings cards.
pub fn apply_mcp_settings_to_disk(
    data: &DataPaths,
    req: &SetMcpSettingsRequest,
) -> AppResult<McpApplyOutcome> {
    validate_mcp_request(req)?;

    let path = data.mcp_settings_path();
    let old_settings = load_mcp_settings(&path);
    let new_settings = req.clone().into_settings();

    if old_settings == new_settings {
        return Ok(McpApplyOutcome {
            changed: false,
            old_settings,
        });
    }
    save_mcp_settings(&path, &new_settings)?;
    Ok(McpApplyOutcome {
        changed: true,
        old_settings,
    })
}

// ── authentication token ────────────────────────────────────────────

fn mcp_token_keyring_entry() -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(MCP_TOKEN_KEYRING_SERVICE, MCP_TOKEN_KEYRING_ACCOUNT)
}

fn cache_mcp_token(value: Option<String>) {
    if let Ok(mut cache) = MCP_TOKEN_CACHE.write() {
        *cache = Some(value);
    }
}

/// Result of reading the MCP token from the OS keyring.
///
/// `Transient` is deliberately distinct from `NotStored`: rotating a token
/// after a locked or denied keyring would invalidate an existing client.
enum McpKeyringRead {
    Found(String),
    NotStored,
    Transient(String),
}

fn classify_mcp_keyring_read(result: keyring::Result<String>) -> McpKeyringRead {
    match result {
        Ok(token) if !token.is_empty() => McpKeyringRead::Found(token),
        Ok(_) | Err(keyring::Error::NoEntry) => McpKeyringRead::NotStored,
        Err(error) => McpKeyringRead::Transient(error.to_string()),
    }
}

/// Load the MCP bearer token from the OS keyring.
///
/// A transient keyring failure is returned as an error (fail closed) instead
/// of being treated as an absent token, which would rotate credentials while
/// the keyring is temporarily locked or inaccessible.
pub fn load_mcp_auth_token() -> AppResult<Option<String>> {
    if let Ok(cache) = MCP_TOKEN_CACHE.read()
        && let Some(cached) = cache.as_ref()
    {
        return Ok(cached.clone());
    }

    let entry = mcp_token_keyring_entry()
        .map_err(|error| AppError::Config(format!("MCP keyring init: {error}")))?;
    match classify_mcp_keyring_read(entry.get_password()) {
        McpKeyringRead::Found(token) => {
            cache_mcp_token(Some(token.clone()));
            Ok(Some(token))
        }
        McpKeyringRead::NotStored => {
            cache_mcp_token(None);
            Ok(None)
        }
        McpKeyringRead::Transient(error) => {
            Err(AppError::Config(format!("MCP keyring read: {error}")))
        }
    }
}

fn random_secret() -> AppResult<String> {
    let mut bytes = [0_u8; MCP_TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|error| AppError::Config(format!("MCP secret entropy unavailable: {error}")))?;

    let mut encoded = String::with_capacity(MCP_TOKEN_BYTES * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(encoded)
}

/// Generate and store a replacement MCP bearer token.
fn rotate_mcp_auth_token_in_keyring() -> AppResult<String> {
    let token = random_secret()?;
    mcp_token_keyring_entry()
        .map_err(|error| AppError::Config(format!("MCP keyring init: {error}")))?
        .set_password(&token)
        .map_err(|error| AppError::Config(format!("MCP keyring set: {error}")))?;
    cache_mcp_token(Some(token.clone()));
    Ok(token)
}

/// Snapshot of the keyring value before a deferred token rotation.
///
/// The value is intentionally opaque to settings persistence. It exists only
/// long enough for `apply_settings` to restore the previous credential when
/// the replacement listener fails to start.
#[derive(Debug, Clone)]
pub struct McpAuthTokenSnapshot(Option<String>);

pub fn snapshot_mcp_auth_token() -> AppResult<McpAuthTokenSnapshot> {
    Ok(McpAuthTokenSnapshot(load_mcp_auth_token()?))
}

/// Restore a token snapshot after a failed deferred rotation.
pub fn restore_mcp_auth_token(snapshot: McpAuthTokenSnapshot) -> AppResult<()> {
    let entry = mcp_token_keyring_entry()
        .map_err(|error| AppError::Config(format!("MCP keyring init: {error}")))?;
    let token = snapshot.0;
    match token.as_deref() {
        Some(token) => entry
            .set_password(token)
            .map_err(|error| AppError::Config(format!("MCP keyring set: {error}")))?,
        None => match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(error) => return Err(AppError::Config(format!("MCP keyring delete: {error}"))),
        },
    }
    cache_mcp_token(token);
    Ok(())
}

/// Rotate a token after the MCP listener has been stopped. The caller must
/// reserve the lifecycle token mutation guard around this operation.
pub fn rotate_mcp_auth_token_after_stop() -> AppResult<String> {
    rotate_mcp_auth_token_in_keyring()
}

/// Replace the bearer token only while no listener is running. The next
/// sidecar start is the atomic point at which the replacement becomes valid.
#[tauri::command]
pub fn rotate_mcp_auth_token(state: tauri::State<'_, super::AppState>) -> AppResult<String> {
    state
        .sidecars
        .with_mcp_token_mutation(rotate_mcp_auth_token_in_keyring)
}

/// Create the first bearer token without ever returning a token that was
/// already configured. Replacement must use the explicit rotate command.
fn generate_mcp_auth_token_in_keyring() -> AppResult<String> {
    if load_mcp_auth_token()?.is_some() {
        return Err(AppError::Config(
            "MCP bearer token is already configured; rotate it explicitly".to_string(),
        ));
    }
    rotate_mcp_auth_token_in_keyring()
}

#[tauri::command]
pub fn generate_mcp_auth_token(state: tauri::State<'_, super::AppState>) -> AppResult<String> {
    state
        .sidecars
        .with_mcp_token_mutation(generate_mcp_auth_token_in_keyring)
}

/// Load the token required to start an authenticated MCP listener.
///
/// First-time generation is an explicit UI action because that command is the
/// only path allowed to reveal the token once to the user. Launch must never
/// create an unusable credential behind the user's back.
pub fn require_mcp_auth_token() -> AppResult<String> {
    load_mcp_auth_token()?.ok_or_else(|| {
        AppError::Config(
            "MCP bearer token is not configured; generate one before enabling MCP".to_string(),
        )
    })
}

/// Delete the MCP bearer token. Missing entries already satisfy the desired
/// state and are therefore treated as success.
pub fn delete_mcp_auth_token() -> Result<(), String> {
    let entry = mcp_token_keyring_entry().map_err(|error| format!("MCP keyring init: {error}"))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {
            cache_mcp_token(None);
            Ok(())
        }
        Err(error) => Err(format!("MCP keyring delete: {error}")),
    }
}

#[cfg(test)]
pub(crate) fn configure_mcp_auth_token_for_test() {
    // The caller holds MCP_KEYRING_TEST_LOCK because the mock credential
    // backend and token cache are process-global test state.
    MCP_KEYRING_TEST_INIT.call_once(|| {
        keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
    });
    delete_mcp_auth_token().expect("clear MCP test token");
    cache_mcp_token(None);
    rotate_mcp_auth_token_in_keyring().expect("configure MCP test token");
}

/// Write a child-consumed secret outside the environment with owner-only
/// permissions. The caller owns the resulting path and removes it after use.
pub fn write_mcp_secret_file(data: &DataPaths, prefix: &str, secret: &str) -> AppResult<PathBuf> {
    std::fs::create_dir_all(data.mcp_runtime_dir())?;
    let mut file = tempfile::Builder::new()
        .prefix(prefix)
        .tempfile_in(data.mcp_runtime_dir())
        .map_err(|error| AppError::Config(format!("create MCP secret file: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(secret.as_bytes())
        .map_err(|error| AppError::Config(format!("write MCP secret file: {error}")))?;
    file.flush()
        .map_err(|error| AppError::Config(format!("flush MCP secret file: {error}")))?;
    let (_, path) = file
        .keep()
        .map_err(|error| AppError::Config(format!("persist MCP secret file: {error}")))?;
    Ok(path)
}

/// Remove child-only secrets left behind when a previous process did not run
/// its normal shutdown cleanup. Callers must hold the instance lock and reap
/// recorded children before calling this function.
pub(crate) fn recover_stale_mcp_secret_files(data: &DataPaths) -> AppResult<()> {
    let runtime_dir = data.mcp_runtime_dir();
    let entries = match std::fs::read_dir(&runtime_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(AppError::Config(format!(
                "read MCP runtime directory {}: {error}",
                runtime_dir.display()
            )));
        }
    };

    for entry in entries {
        let entry =
            entry.map_err(|error| AppError::Config(format!("read MCP secret entry: {error}")))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("auth-") {
            continue;
        }

        let path = entry.path();
        let file_type = std::fs::symlink_metadata(&path)
            .map_err(|error| {
                AppError::Config(format!(
                    "inspect MCP secret file {}: {error}",
                    path.display()
                ))
            })?
            .file_type();
        if !file_type.is_file() && !file_type.is_symlink() {
            return Err(AppError::Config(format!(
                "refusing to remove non-file MCP secret entry {}",
                path.display()
            )));
        }
        std::fs::remove_file(&path).map_err(|error| {
            AppError::Config(format!(
                "remove stale MCP secret file {}: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

// ── Tauri commands ───────────────────────────────────────────────────

#[tauri::command]
pub fn get_mcp_settings(
    state: tauri::State<'_, super::AppState>,
) -> AppResult<McpSettingsResponse> {
    let settings = load_mcp_settings(&state.data.mcp_settings_path());
    let active_port = state.sidecars.active_mcp_port();
    let (runtime_status, unavailable_reason) = state.sidecars.mcp_runtime_state();
    Ok(McpSettingsResponse {
        enabled: settings.enabled,
        allow_unauthenticated_requests: settings.allow_unauthenticated_requests,
        exclude_runner_as_tool: settings.exclude_runner_as_tool,
        exclude_worker_as_tool: settings.exclude_worker_as_tool,
        streaming: settings.streaming,
        request_timeout_sec: settings.request_timeout_sec,
        rag_default_period_scope: settings.rag_default_period_scope,
        set_name: MCP_FUNCTION_SET_NAME.to_string(),
        active_port,
        auth_token_configured: load_mcp_auth_token()?.is_some(),
        runtime_status,
        unavailable_reason,
    })
}

/// Thin wrapper that funnels a single MCP change through the unified
/// `apply_settings` pipeline (validate → persist → single restart →
/// rollback), so the MCP card and a multi-card save share one code path.
#[tauri::command]
pub async fn set_mcp_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, super::AppState>,
    req: SetMcpSettingsRequest,
) -> AppResult<super::apply_settings::ApplySettingsResponse> {
    super::apply_settings::apply_settings(
        app,
        state,
        super::apply_settings::ApplySettingsRequest {
            llm: None,
            embedding: None,
            hf_home: None,
            mcp: Some(req),
            timezone: None,
            issue_tracker: None,
            migration_backup: None,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    fn lock_mcp_keyring_test() -> MutexGuard<'static, ()> {
        super::MCP_KEYRING_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn install_mock_keyring() {
        super::MCP_KEYRING_TEST_INIT.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
    }

    fn data_paths_in(tmp: &Path) -> DataPaths {
        DataPaths::with_root(tmp.to_path_buf())
    }

    fn enabled_req() -> SetMcpSettingsRequest {
        SetMcpSettingsRequest {
            enabled: true,
            allow_unauthenticated_requests: false,
            rotate_auth_token: false,
            exclude_runner_as_tool: None,
            exclude_worker_as_tool: None,
            streaming: None,
            request_timeout_sec: None,
            rag_default_period_scope: McpRagDefaultPeriodScope::OverallOnly,
        }
    }

    // ── authentication token ─────────────────────────────────────────

    #[test]
    fn persisted_mcp_settings_never_contain_auth_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-settings.json");
        save_mcp_settings(
            &path,
            &McpSettings {
                enabled: true,
                ..Default::default()
            },
        )
        .unwrap();

        let persisted = std::fs::read_to_string(path).unwrap();
        assert!(!persisted.contains("token"));
        assert!(!persisted.contains("secret"));
    }

    #[test]
    fn classify_mcp_keyring_read_distinguishes_absence_and_transient_failures() {
        assert!(matches!(
            classify_mcp_keyring_read(Ok("token".to_string())),
            McpKeyringRead::Found(token) if token == "token"
        ));
        assert!(matches!(
            classify_mcp_keyring_read(Ok(String::new())),
            McpKeyringRead::NotStored
        ));
        assert!(matches!(
            classify_mcp_keyring_read(Err(keyring::Error::NoEntry)),
            McpKeyringRead::NotStored
        ));
        assert!(matches!(
            classify_mcp_keyring_read(Err(keyring::Error::PlatformFailure(Box::new(
                std::io::Error::other("keychain locked")
            )))),
            McpKeyringRead::Transient(error) if error.contains("keychain locked")
        ));
    }

    #[test]
    fn response_serializes_only_whether_the_token_is_configured() {
        let response = McpSettingsResponse {
            enabled: true,
            allow_unauthenticated_requests: false,
            exclude_runner_as_tool: None,
            exclude_worker_as_tool: None,
            streaming: None,
            request_timeout_sec: None,
            rag_default_period_scope: McpRagDefaultPeriodScope::OverallOnly,
            set_name: MCP_FUNCTION_SET_NAME.to_string(),
            active_port: Some(MCP_DEFAULT_PORT),
            auth_token_configured: true,
            runtime_status: crate::sidecar::McpRuntimeStatus::Ready,
            unavailable_reason: None,
        };

        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["auth_token_configured"], true);
        assert_eq!(json["allow_unauthenticated_requests"], false);
        assert!(json.get("auth_token").is_none());
        assert!(json.get("token").is_none());
    }

    #[test]
    fn launch_requires_an_explicitly_generated_token_and_never_rotates_implicitly() {
        let _guard = lock_mcp_keyring_test();
        install_mock_keyring();
        cache_mcp_token(None);

        assert!(
            require_mcp_auth_token()
                .unwrap_err()
                .to_string()
                .contains("not configured")
        );

        let first = generate_mcp_auth_token_in_keyring().unwrap();
        assert_eq!(first.len(), MCP_TOKEN_BYTES * 2);
        assert_eq!(require_mcp_auth_token().unwrap(), first);

        let second = rotate_mcp_auth_token_in_keyring().unwrap();
        assert_ne!(first, second);
        assert_eq!(
            load_mcp_auth_token().unwrap().as_deref(),
            Some(second.as_str())
        );

        delete_mcp_auth_token().unwrap();
        assert_eq!(load_mcp_auth_token().unwrap(), None);
    }

    #[test]
    fn keyring_initial_token_generation_refuses_to_replace_an_existing_token() {
        let _guard = lock_mcp_keyring_test();
        install_mock_keyring();
        cache_mcp_token(None);
        delete_mcp_auth_token().unwrap();

        let first = generate_mcp_auth_token_in_keyring().unwrap();
        let error = generate_mcp_auth_token_in_keyring().unwrap_err();

        assert!(error.to_string().contains("already configured"));
        assert_eq!(
            load_mcp_auth_token().unwrap().as_deref(),
            Some(first.as_str())
        );
        delete_mcp_auth_token().unwrap();
    }

    #[test]
    fn deferred_rotation_is_not_persisted_in_mcp_settings() {
        let request = SetMcpSettingsRequest {
            rotate_auth_token: true,
            ..enabled_req()
        };

        let settings = request.into_settings();
        let json = serde_json::to_value(settings).unwrap();
        assert!(json.get("rotate_auth_token").is_none());
    }

    #[test]
    fn deferred_rotation_allows_enabling_before_the_new_token_exists() {
        let _guard = lock_mcp_keyring_test();
        install_mock_keyring();
        cache_mcp_token(None);
        delete_mcp_auth_token().unwrap();

        validate_mcp_request(&SetMcpSettingsRequest {
            rotate_auth_token: true,
            ..enabled_req()
        })
        .unwrap();

        assert!(validate_mcp_request(&enabled_req()).is_err());
        assert!(
            validate_mcp_request(&SetMcpSettingsRequest {
                enabled: false,
                rotate_auth_token: true,
                ..enabled_req()
            })
            .is_err()
        );
    }

    #[test]
    fn token_snapshot_restores_previous_credential_after_failed_rotation() {
        let _guard = lock_mcp_keyring_test();
        install_mock_keyring();
        cache_mcp_token(None);
        delete_mcp_auth_token().unwrap();

        let old = generate_mcp_auth_token_in_keyring().unwrap();
        let snapshot = snapshot_mcp_auth_token().unwrap();
        let replacement = rotate_mcp_auth_token_after_stop().unwrap();
        assert_ne!(old, replacement);

        restore_mcp_auth_token(snapshot).unwrap();
        assert_eq!(
            load_mcp_auth_token().unwrap().as_deref(),
            Some(old.as_str())
        );
        delete_mcp_auth_token().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn secret_file_is_private_and_does_not_include_the_secret_in_its_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let data = data_paths_in(dir.path());
        let secret = "never-in-a-path";
        let path = write_mcp_secret_file(&data, "auth-", secret).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), secret);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!path.to_string_lossy().contains(secret));
    }

    #[test]
    fn stale_mcp_secret_recovery_removes_only_owned_files() {
        let dir = tempfile::tempdir().unwrap();
        let data = data_paths_in(dir.path());
        std::fs::create_dir_all(data.mcp_runtime_dir()).unwrap();
        let auth = data.mcp_runtime_dir().join("auth-stale");
        let unrelated = data.mcp_runtime_dir().join("keep.txt");
        let nested = data.mcp_runtime_dir().join("keep-directory");
        std::fs::write(&auth, "old-token").unwrap();
        std::fs::write(&unrelated, "keep").unwrap();
        std::fs::create_dir(&nested).unwrap();

        recover_stale_mcp_secret_files(&data).unwrap();

        assert!(!auth.exists());
        assert_eq!(std::fs::read_to_string(unrelated).unwrap(), "keep");
        assert!(nested.is_dir());
    }

    #[test]
    fn stale_mcp_secret_recovery_rejects_owned_directories() {
        let dir = tempfile::tempdir().unwrap();
        let data = data_paths_in(dir.path());
        let stale_dir = data.mcp_runtime_dir().join("auth-not-a-file");
        std::fs::create_dir_all(&stale_dir).unwrap();

        let error = recover_stale_mcp_secret_files(&data).unwrap_err();

        assert!(error.to_string().contains("non-file MCP secret entry"));
        assert!(stale_dir.is_dir());
    }

    #[test]
    fn legacy_enabled_mcp_without_token_is_disabled_but_preserves_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-settings.json");
        save_mcp_settings(
            &path,
            &McpSettings {
                enabled: true,
                streaming: Some(false),
                request_timeout_sec: Some(120),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(migrate_legacy_enabled_mcp_without_auth_token(&path, false).unwrap());

        assert_eq!(
            load_mcp_settings(&path),
            McpSettings {
                enabled: false,
                streaming: Some(false),
                request_timeout_sec: Some(120),
                ..Default::default()
            }
        );
    }

    #[test]
    fn authenticated_or_disabled_mcp_settings_do_not_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let authenticated = dir.path().join("authenticated.json");
        let disabled = dir.path().join("disabled.json");
        save_mcp_settings(
            &authenticated,
            &McpSettings {
                enabled: true,
                ..Default::default()
            },
        )
        .unwrap();
        save_mcp_settings(&disabled, &McpSettings::default()).unwrap();

        assert!(!migrate_legacy_enabled_mcp_without_auth_token(&authenticated, true).unwrap());
        assert!(!migrate_legacy_enabled_mcp_without_auth_token(&disabled, false).unwrap());
        assert!(load_mcp_settings(&authenticated).enabled);
        assert!(!load_mcp_settings(&disabled).enabled);
    }

    #[test]
    fn explicitly_unauthenticated_mcp_settings_do_not_migrate_without_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unauthenticated.json");
        save_mcp_settings(
            &path,
            &McpSettings {
                enabled: true,
                allow_unauthenticated_requests: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(!migrate_legacy_enabled_mcp_without_auth_token(&path, false).unwrap());
        assert!(load_mcp_settings(&path).enabled);
    }

    #[cfg(unix)]
    #[test]
    fn stale_mcp_secret_recovery_unlinks_a_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let data = data_paths_in(dir.path());
        std::fs::create_dir_all(data.mcp_runtime_dir()).unwrap();
        let target = dir.path().join("outside-token");
        let stale_link = data.mcp_runtime_dir().join("auth-stale-link");
        std::fs::write(&target, "keep").unwrap();
        symlink(&target, &stale_link).unwrap();

        recover_stale_mcp_secret_files(&data).unwrap();

        assert!(!stale_link.exists());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "keep");
    }

    // ── default / load / save ─────────────────────────────────────────

    #[test]
    fn load_missing_file_returns_default_disabled() {
        // Backward compat: every install before this feature has no
        // mcp-settings.json, and must keep MCP off.
        let dir = tempfile::tempdir().unwrap();
        let s = load_mcp_settings(&dir.path().join("nope.json"));
        assert_eq!(s, McpSettings::default());
        assert!(!s.enabled);
    }

    #[test]
    fn load_corrupt_json_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-settings.json");
        std::fs::write(&path, b"{corrupt").unwrap();
        assert_eq!(load_mcp_settings(&path), McpSettings::default());
    }

    #[test]
    fn empty_json_object_is_disabled_default() {
        let back: McpSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(back, McpSettings::default());
        assert!(!back.enabled);
        assert_eq!(
            back.rag_default_period_scope,
            McpRagDefaultPeriodScope::OverallOnly
        );
    }

    #[test]
    fn legacy_mcp_settings_default_to_overall_only_period_recall() {
        let back: McpSettings = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert_eq!(
            back.rag_default_period_scope,
            McpRagDefaultPeriodScope::OverallOnly
        );
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-settings.json");
        let settings = McpSettings {
            enabled: true,
            streaming: Some(false),
            request_timeout_sec: Some(120),
            ..Default::default()
        };
        save_mcp_settings(&path, &settings).unwrap();
        assert_eq!(load_mcp_settings(&path), settings);
    }

    // ── resolve ───────────────────────────────────────────────────────

    #[test]
    fn resolve_set_name_is_constant() {
        let rt = resolve_mcp_runtime(&McpSettings::default());
        assert_eq!(MCP_FUNCTION_SET_NAME, "lookback-mcp");
        assert_eq!(rt.set_name, MCP_FUNCTION_SET_NAME);
        assert!(!rt.enabled);
    }

    #[test]
    fn resolve_carries_advanced_fields() {
        let rt = resolve_mcp_runtime(&McpSettings {
            enabled: true,
            allow_unauthenticated_requests: false,
            exclude_runner_as_tool: Some(true),
            streaming: Some(false),
            request_timeout_sec: Some(90),
            ..Default::default()
        });
        assert!(rt.enabled);
        assert_eq!(rt.exclude_runner_as_tool, Some(true));
        assert_eq!(rt.streaming, Some(false));
        assert_eq!(rt.request_timeout_sec, Some(90));
    }

    #[test]
    fn resolve_carries_mcp_period_recall_scope() {
        let rt = resolve_mcp_runtime(&McpSettings {
            enabled: true,
            rag_default_period_scope: McpRagDefaultPeriodScope::OverallAndPath,
            ..Default::default()
        });
        assert_eq!(
            rt.rag_default_period_scope,
            McpRagDefaultPeriodScope::OverallAndPath
        );
    }

    // ── mcp_env_vars ──────────────────────────────────────────────────

    #[test]
    fn env_vars_disabled_emits_only_mcp_enabled_false() {
        let rt = resolve_mcp_runtime(&McpSettings::default());
        let vars = mcp_env_vars(&rt);
        assert_eq!(vars, vec![("MCP_ENABLED", "false".to_string())]);
    }

    #[test]
    fn env_vars_enabled_minimal_emits_only_enabled_and_set_name() {
        // Advanced fields unset ⇒ jobworkerp keeps its own defaults; we must
        // NOT pin them to a literal here.
        let rt = resolve_mcp_runtime(&McpSettings {
            enabled: true,
            ..Default::default()
        });
        let map: std::collections::HashMap<&str, String> = mcp_env_vars(&rt).into_iter().collect();
        assert_eq!(map.get("MCP_ENABLED").map(String::as_str), Some("true"));
        assert_eq!(
            map.get("MCP_SET_NAME").map(String::as_str),
            Some(MCP_FUNCTION_SET_NAME)
        );
        assert!(!map.contains_key("MCP_EXCLUDE_RUNNER"));
        assert!(!map.contains_key("MCP_EXCLUDE_WORKER"));
        assert!(!map.contains_key("MCP_STREAMING"));
        assert!(!map.contains_key("MCP_TIMEOUT_SEC"));
        assert_eq!(
            map.get("LOOKBACK_MCP_RAG_DEFAULT_PERIOD_SCOPE")
                .map(String::as_str),
            Some("overall_only")
        );
    }

    #[test]
    fn env_vars_expose_selected_period_recall_scope_to_mcp_workflow() {
        let rt = resolve_mcp_runtime(&McpSettings {
            enabled: true,
            rag_default_period_scope: McpRagDefaultPeriodScope::OverallAndPath,
            ..Default::default()
        });
        let map: std::collections::HashMap<&str, String> = mcp_env_vars(&rt).into_iter().collect();
        assert_eq!(
            map.get("LOOKBACK_MCP_RAG_DEFAULT_PERIOD_SCOPE")
                .map(String::as_str),
            Some("overall_and_path")
        );
    }

    #[test]
    fn env_vars_enabled_emits_set_advanced_keys() {
        let rt = resolve_mcp_runtime(&McpSettings {
            enabled: true,
            allow_unauthenticated_requests: false,
            exclude_runner_as_tool: Some(true),
            exclude_worker_as_tool: Some(false),
            streaming: Some(false),
            request_timeout_sec: Some(120),
            rag_default_period_scope: McpRagDefaultPeriodScope::OverallOnly,
        });
        let map: std::collections::HashMap<&str, String> = mcp_env_vars(&rt).into_iter().collect();
        assert_eq!(
            map.get("MCP_EXCLUDE_RUNNER").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            map.get("MCP_EXCLUDE_WORKER").map(String::as_str),
            Some("false")
        );
        assert_eq!(map.get("MCP_STREAMING").map(String::as_str), Some("false"));
        assert_eq!(map.get("MCP_TIMEOUT_SEC").map(String::as_str), Some("120"));
    }

    #[test]
    fn managed_keys_cover_every_key_env_vars_can_emit() {
        // The spawn path clears MCP_MANAGED_ENV_KEYS from the inherited env
        // before re-setting the configured ones. If `mcp_env_vars` ever emits
        // a key not in that list, an unset value for it would leak from the
        // parent env / `.env` template. Pin the superset relationship so a
        // new key can't be added to one without the other.
        let rt = resolve_mcp_runtime(&McpSettings {
            enabled: true,
            allow_unauthenticated_requests: false,
            exclude_runner_as_tool: Some(true),
            exclude_worker_as_tool: Some(false),
            streaming: Some(true),
            request_timeout_sec: Some(60),
            rag_default_period_scope: McpRagDefaultPeriodScope::OverallOnly,
        });
        for (key, _) in mcp_env_vars(&rt) {
            assert!(
                MCP_MANAGED_ENV_KEYS.contains(&key),
                "mcp_env_vars emits {key:?} but it is missing from MCP_MANAGED_ENV_KEYS \
                 — an unset value for it would leak from the inherited env"
            );
        }
    }

    // ── default port ──────────────────────────────────────────────────

    #[test]
    fn default_port_does_not_clash_with_known_ports() {
        // jobworkerp sidecar trio + mcp-server default + common dev servers.
        for taken in [9000u16, 9010, 9020, 8000, 3000, 5173, 8080] {
            assert_ne!(MCP_DEFAULT_PORT, taken, "MCP default must not clash");
        }
    }

    // ── validation ────────────────────────────────────────────────────

    #[test]
    fn validate_accepts_enable() {
        // MCP is not gated by connection mode — the local sidecar (where the
        // MCP server runs) is up in both local and remote browse modes.
        let _guard = lock_mcp_keyring_test();
        configure_mcp_auth_token_for_test();
        assert!(validate_mcp_request(&enabled_req()).is_ok());
        delete_mcp_auth_token().unwrap();
    }

    #[test]
    fn validate_accepts_unauthenticated_enable_without_a_token() {
        let _guard = lock_mcp_keyring_test();
        install_mock_keyring();
        delete_mcp_auth_token().unwrap();
        cache_mcp_token(None);

        let request = SetMcpSettingsRequest {
            allow_unauthenticated_requests: true,
            ..enabled_req()
        };
        assert!(validate_mcp_request(&request).is_ok());
    }

    #[test]
    fn validate_rejects_out_of_range_timeout() {
        let req = SetMcpSettingsRequest {
            request_timeout_sec: Some(0),
            ..enabled_req()
        };
        assert!(validate_mcp_request(&req).is_err());
        let req = SetMcpSettingsRequest {
            request_timeout_sec: Some(REQUEST_TIMEOUT_SEC_MAX + 1),
            ..enabled_req()
        };
        assert!(validate_mcp_request(&req).is_err());
    }

    // ── apply_to_disk ─────────────────────────────────────────────────

    #[test]
    fn apply_to_disk_rejects_enabling_mcp_without_a_bearer_token() {
        let _guard = lock_mcp_keyring_test();
        install_mock_keyring();
        delete_mcp_auth_token().unwrap();
        cache_mcp_token(None);

        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        let error = match apply_mcp_settings_to_disk(&data, &enabled_req()) {
            Ok(_) => panic!("MCP enable must be rejected without a bearer token"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("MCP bearer token is not configured"),
            "enabling MCP must require a token that the user generated explicitly"
        );
        assert!(
            !data.mcp_settings_path().exists(),
            "a rejected enable request must not write an enabled settings file"
        );
    }

    #[test]
    fn apply_to_disk_allows_unauthenticated_enable_without_a_bearer_token() {
        let _guard = lock_mcp_keyring_test();
        install_mock_keyring();
        delete_mcp_auth_token().unwrap();
        cache_mcp_token(None);
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());

        let outcome = apply_mcp_settings_to_disk(
            &data,
            &SetMcpSettingsRequest {
                allow_unauthenticated_requests: true,
                ..enabled_req()
            },
        )
        .unwrap();

        assert!(outcome.changed);
        let settings = load_mcp_settings(&data.mcp_settings_path());
        assert!(settings.enabled);
        assert!(settings.allow_unauthenticated_requests);
    }

    #[test]
    fn apply_to_disk_noop_when_unchanged() {
        let _guard = lock_mcp_keyring_test();
        configure_mcp_auth_token_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        apply_mcp_settings_to_disk(&data, &enabled_req()).unwrap();
        let outcome = apply_mcp_settings_to_disk(&data, &enabled_req()).unwrap();
        assert!(!outcome.changed, "re-applying the same settings is a no-op");
        delete_mcp_auth_token().unwrap();
    }

    #[test]
    fn apply_to_disk_persists_and_reports_change() {
        let _guard = lock_mcp_keyring_test();
        configure_mcp_auth_token_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        let outcome = apply_mcp_settings_to_disk(&data, &enabled_req()).unwrap();
        assert!(outcome.changed);
        assert!(
            !outcome.old_settings.enabled,
            "old value captured for rollback"
        );
        assert!(load_mcp_settings(&data.mcp_settings_path()).enabled);
        delete_mcp_auth_token().unwrap();
    }

    #[test]
    fn function_sets_yaml_defines_stable_lookback_mcp_tools() {
        // Contract guard: `MCP_FUNCTION_SET_NAME` is the function-set VALUE we
        // pass to jobworkerp via the `MCP_SET_NAME` env var. If the committed
        // function-sets.yaml ever renames or drops that set, the MCP server
        // would expose NO tools at runtime with no compile error. Pin the name
        // → target binding here so a drift is a test failure. The bundled YAML
        // lives next to this crate at
        // `<CARGO_MANIFEST_DIR>/../workers/function-sets.yaml`.
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../workers/function-sets.yaml");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        // Coarse but dependency-free: assert the set name and its target
        // worker both appear. A structured YAML parse would pull in the
        // jobworkerp function-set schema, which this crate does not depend on.
        assert!(
            raw.contains(&format!("name: {MCP_FUNCTION_SET_NAME}")),
            "function-sets.yaml must define a set named {MCP_FUNCTION_SET_NAME}"
        );
        assert!(
            !raw.contains(&format!("name: {}-rag", MCP_FUNCTION_SET_NAME)),
            "function-sets.yaml must not retain the former MCP set name"
        );
        for worker in [
            "lookback_issue_list_ready",
            "lookback_issue_transition",
            "lookback_issue_claim",
            "lookback_issue_heartbeat",
            "lookback_issue_release",
        ] {
            assert!(
                raw.contains(&format!("name: {worker}")),
                "function-sets.yaml must target the {worker} worker"
            );
        }
        assert!(
            !raw.contains("name: lookback_issue_accept"),
            "candidate accept must not be an MCP Issue Gateway target"
        );
        assert!(
            !raw.contains("manager_"),
            "raw manager methods must not appear in lookback-mcp"
        );
        for worker in [
            "lookback_recall",
            "lookback_get_context",
            "lookback_get_reflections",
        ] {
            assert!(
                raw.contains(&format!("name: {worker}")),
                "function-sets.yaml must target the {worker} worker"
            );
        }
    }

    #[test]
    fn apply_to_disk_allows_remote_enable() {
        // MCP runs in the local sidecar regardless of remote browse mode, so
        // enabling it while connected to a remote memories must succeed and
        // persist. The MCP-exposed search follows the active connection via
        // the workflow-input defaults resolved at sidecar start.
        let _guard = lock_mcp_keyring_test();
        configure_mcp_auth_token_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        super::super::connection::save_connection_config(
            &data.connection_config_path(),
            &super::super::connection::ConnectionConfig::remote_pair(
                "http://h:9000",
                "http://h:9010",
            ),
        )
        .unwrap();
        let outcome = apply_mcp_settings_to_disk(&data, &enabled_req()).unwrap();
        assert!(outcome.changed);
        assert!(load_mcp_settings(&data.mcp_settings_path()).enabled);
        delete_mcp_auth_token().unwrap();
    }
}
