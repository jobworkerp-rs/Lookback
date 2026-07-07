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

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::data::DataPaths;
use crate::error::{AppError, AppResult};

/// FunctionSet exposed to MCP clients when the server is enabled. Defined in
/// `workers/function-sets.yaml`, kept SEPARATE from the chat-loop's
/// `lookback-rag` set so the MCP-visible tool surface can grow independently
/// of the in-app chat tool set. Passed to jobworkerp via the `MCP_SET_NAME`
/// env var (not to be confused with this constant's name, which is the
/// function-set VALUE, not the env key).
pub const MCP_FUNCTION_SET_NAME: &str = "lookback-mcp-rag";

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
];

/// `MCP_TIMEOUT_SEC` upper guard — a typo'd huge value would let a hung
/// tool call wedge the MCP client indefinitely.
const REQUEST_TIMEOUT_SEC_MAX: u32 = 3600;

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
}

/// Frontend-facing response. Adds derived fields the UI renders without a
/// second roundtrip.
#[derive(Debug, Clone, Serialize)]
pub struct McpSettingsResponse {
    pub enabled: bool,
    pub exclude_runner_as_tool: Option<bool>,
    pub exclude_worker_as_tool: Option<bool>,
    pub streaming: Option<bool>,
    pub request_timeout_sec: Option<u32>,
    /// FunctionSet name exposed over MCP — constant, surfaced so the UI can
    /// show it in the client-config hint without hardcoding the value twice.
    pub set_name: String,
    /// The MCP server's actual bound port when it is currently running,
    /// else `None` (server off, or sidecars stopped). Surfaced so the UI can
    /// print the connection URL for the user to paste into their MCP client.
    pub active_port: Option<u16>,
}

/// Request from the frontend.
#[derive(Debug, Clone, Deserialize)]
pub struct SetMcpSettingsRequest {
    pub enabled: bool,
    #[serde(default)]
    pub exclude_runner_as_tool: Option<bool>,
    #[serde(default)]
    pub exclude_worker_as_tool: Option<bool>,
    #[serde(default)]
    pub streaming: Option<bool>,
    #[serde(default)]
    pub request_timeout_sec: Option<u32>,
}

impl SetMcpSettingsRequest {
    fn into_settings(self) -> McpSettings {
        McpSettings {
            enabled: self.enabled,
            exclude_runner_as_tool: self.exclude_runner_as_tool,
            exclude_worker_as_tool: self.exclude_worker_as_tool,
            streaming: self.streaming,
            request_timeout_sec: self.request_timeout_sec,
        }
    }
}

/// Resolved runtime values consumed by `sidecar/lifecycle.rs`. `set_name` is
/// constant (there is no per-user choice), carried here so the env producer
/// and the UI hint share one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRuntime {
    pub enabled: bool,
    pub set_name: String,
    pub exclude_runner_as_tool: Option<bool>,
    pub exclude_worker_as_tool: Option<bool>,
    pub streaming: Option<bool>,
    pub request_timeout_sec: Option<u32>,
}

/// Project `McpSettings` into runtime values. Pure so the (small) mapping is
/// unit-testable. No env-override path: unlike embedding, MCP has no dev
/// shell knobs — the file is the only source.
pub fn resolve_mcp_runtime(settings: &McpSettings) -> McpRuntime {
    McpRuntime {
        enabled: settings.enabled,
        set_name: MCP_FUNCTION_SET_NAME.to_string(),
        exclude_runner_as_tool: settings.exclude_runner_as_tool,
        exclude_worker_as_tool: settings.exclude_worker_as_tool,
        streaming: settings.streaming,
        request_timeout_sec: settings.request_timeout_sec,
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
    validate_set_request(req).map_err(AppError::Config)
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

// ── Tauri commands ───────────────────────────────────────────────────

#[tauri::command]
pub fn get_mcp_settings(
    state: tauri::State<'_, super::AppState>,
) -> AppResult<McpSettingsResponse> {
    let settings = load_mcp_settings(&state.data.mcp_settings_path());
    let active_port = state.sidecars.active_mcp_port();
    Ok(McpSettingsResponse {
        enabled: settings.enabled,
        exclude_runner_as_tool: settings.exclude_runner_as_tool,
        exclude_worker_as_tool: settings.exclude_worker_as_tool,
        streaming: settings.streaming,
        request_timeout_sec: settings.request_timeout_sec,
        set_name: MCP_FUNCTION_SET_NAME.to_string(),
        active_port,
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
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_paths_in(tmp: &Path) -> DataPaths {
        DataPaths::with_root(tmp.to_path_buf())
    }

    fn enabled_req() -> SetMcpSettingsRequest {
        SetMcpSettingsRequest {
            enabled: true,
            exclude_runner_as_tool: None,
            exclude_worker_as_tool: None,
            streaming: None,
            request_timeout_sec: None,
        }
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
        assert_eq!(rt.set_name, MCP_FUNCTION_SET_NAME);
        assert!(!rt.enabled);
    }

    #[test]
    fn resolve_carries_advanced_fields() {
        let rt = resolve_mcp_runtime(&McpSettings {
            enabled: true,
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
    }

    #[test]
    fn env_vars_enabled_emits_set_advanced_keys() {
        let rt = resolve_mcp_runtime(&McpSettings {
            enabled: true,
            exclude_runner_as_tool: Some(true),
            exclude_worker_as_tool: Some(false),
            streaming: Some(false),
            request_timeout_sec: Some(120),
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
            exclude_runner_as_tool: Some(true),
            exclude_worker_as_tool: Some(false),
            streaming: Some(true),
            request_timeout_sec: Some(60),
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
        assert!(validate_mcp_request(&enabled_req()).is_ok());
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
    fn apply_to_disk_noop_when_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        apply_mcp_settings_to_disk(&data, &enabled_req()).unwrap();
        let outcome = apply_mcp_settings_to_disk(&data, &enabled_req()).unwrap();
        assert!(!outcome.changed, "re-applying the same settings is a no-op");
    }

    #[test]
    fn apply_to_disk_persists_and_reports_change() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        let outcome = apply_mcp_settings_to_disk(&data, &enabled_req()).unwrap();
        assert!(outcome.changed);
        assert!(
            !outcome.old_settings.enabled,
            "old value captured for rollback"
        );
        assert!(load_mcp_settings(&data.mcp_settings_path()).enabled);
    }

    #[test]
    fn function_sets_yaml_defines_the_mcp_set_targeting_lookback_recall() {
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
        // The lookback_recall worker must be a target somewhere in the file
        // (both lookback-rag and lookback-mcp-rag reference it).
        assert!(
            raw.contains("name: lookback_recall"),
            "function-sets.yaml must target the lookback_recall worker"
        );
    }

    #[test]
    fn apply_to_disk_allows_remote_enable() {
        // MCP runs in the local sidecar regardless of remote browse mode, so
        // enabling it while connected to a remote memories must succeed and
        // persist. The MCP-exposed search follows the active connection via
        // the workflow-input defaults resolved at sidecar start.
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        super::super::connection::save_connection_config(
            &data.connection_config_path(),
            &super::super::connection::ConnectionConfig {
                mode: super::super::connection::ConnectionMode::Remote,
                remote_jobworkerp_url: Some("http://h:9000".into()),
                remote_memories_url: Some("http://h:9010".into()),
            },
        )
        .unwrap();
        let outcome = apply_mcp_settings_to_disk(&data, &enabled_req()).unwrap();
        assert!(outcome.changed);
        assert!(load_mcp_settings(&data.mcp_settings_path()).enabled);
    }
}
