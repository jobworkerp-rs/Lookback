//! FR-CONFIG-1: connection-target override.
//!
//! By default the app talks to the bundled local sidecars on their
//! dynamically chosen ports (ARCH-7). The user can override this to point at
//! an already-running remote memories / jobworkerp instead. The override is a
//! small JSON file under the data root (see `DataPaths::connection_config_path`)
//! — there is no general settings store in this app, and pulling in
//! `tauri-plugin-store` for one toggle isn't worth it.
//!
//! The local sidecars are still spawned even in remote mode; the override only
//! changes which URLs the gRPC clients connect to. This keeps the
//! start / retry / purge / model-status paths untouched (the alternative —
//! skipping the spawn in remote mode — branches all of those for marginal
//! resource savings).

use std::path::Path;

use serde::{Deserialize, Serialize};
use tauri::Emitter;
use tonic::transport::Endpoint;

use crate::error::{AppError, AppResult};
use crate::sidecar::SidecarEndpoints;

use super::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionMode {
    #[default]
    Local,
    Remote,
}

/// Persisted connection preference. The remote URLs are kept even while in
/// local mode so the form can re-display the last entered values when the user
/// switches back to remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ConnectionConfig {
    #[serde(default)]
    pub mode: ConnectionMode,
    #[serde(default)]
    pub remote_jobworkerp_url: Option<String>,
    #[serde(default)]
    pub remote_memories_url: Option<String>,
}

/// The concrete URLs the gRPC clients should dial after applying the override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTargets {
    pub jobworkerp_url: String,
    pub memories_url: String,
}

/// The memories endpoint decomposed for the workflow runner's gRPC callback
/// (`memories_grpc_host` / `memories_grpc_port` / `memories_grpc_tls`). The
/// batch/single workflows dial memories back at these coordinates, so a remote
/// override (incl. HTTPS) must propagate here too, not only into the gRPC
/// clients the app uses directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoriesCallback {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

impl MemoriesCallback {
    /// Overlay the `memories_grpc_{host,port,tls}` workflow-input fields
    /// onto `obj`, with the resolved values winning over any caller-
    /// supplied entry (so a hallucinated endpoint from an LLM tool call
    /// can't redirect the search). The summary/reflection/import batches
    /// historically inlined this same three-key write; centralising it
    /// here keeps the wire shape — and any future endpoint field —
    /// in one place.
    pub fn inject_into(&self, obj: &mut serde_json::Map<String, serde_json::Value>) {
        obj.insert(
            "memories_grpc_host".to_string(),
            serde_json::Value::String(self.host.clone()),
        );
        obj.insert("memories_grpc_port".to_string(), self.port.into());
        obj.insert("memories_grpc_tls".to_string(), self.tls.into());
    }
}

impl ResolvedTargets {
    /// Decompose `memories_url` into the host/port/tls a workflow callback
    /// needs. Falls back to the scheme's default port (80/443) when the URL
    /// omits one.
    pub fn memories_callback(&self) -> AppResult<MemoriesCallback> {
        parse_callback(&self.memories_url)
    }
}

/// Split a `http(s)://host[:port]` URL into callback coordinates. Uses the
/// `url` crate (not hand splitting on `:`) so IPv6 literals like
/// `http://[::1]:9010` parse correctly — a naive `rsplit_once(':')` would treat
/// the address colons as the port delimiter, failing the portless form and
/// leaking brackets/userinfo into the host. Kept as a free pure function so
/// it's unit-testable without a live sidecar.
pub fn parse_callback(url_str: &str) -> AppResult<MemoriesCallback> {
    let url = url::Url::parse(url_str)
        .map_err(|e| AppError::Config(format!("invalid memories URL {url_str}: {e}")))?;
    let tls = match url.scheme() {
        "https" => true,
        "http" => false,
        other => {
            return Err(AppError::Config(format!(
                "memories URL must be http or https, got {other}: {url_str}"
            )));
        }
    };
    // `host()` distinguishes IPv6 so we can re-bracket it. The downstream
    // workflow runner builds the endpoint as `format!("{host}:{port}")`
    // (runner/src/runner/grpc/common.rs), which only round-trips an IPv6 host
    // when it carries brackets — `::1:9010` would be ambiguous.
    let host = match url.host() {
        Some(url::Host::Ipv6(addr)) => format!("[{addr}]"),
        Some(h) => h.to_string(),
        None => {
            return Err(AppError::Config(format!(
                "missing host in memories URL: {url_str}"
            )));
        }
    };
    // `port_or_known_default` falls back to 80/443 for http/https.
    let port = url
        .port_or_known_default()
        .ok_or_else(|| AppError::Config(format!("missing port in memories URL: {url_str}")))?;
    Ok(MemoriesCallback { host, port, tls })
}

/// Choose the effective gRPC targets. Local mode reads the live sidecar
/// endpoints (dynamic ports), remote mode uses the configured URLs.
pub fn resolve_targets(
    cfg: &ConnectionConfig,
    local: Option<&SidecarEndpoints>,
) -> AppResult<ResolvedTargets> {
    match cfg.mode {
        ConnectionMode::Local => {
            let eps = local
                .ok_or_else(|| AppError::SidecarNotReady("local sidecars not started".into()))?;
            Ok(ResolvedTargets {
                jobworkerp_url: eps.jobworkerp_url(),
                memories_url: eps.memories_url(),
            })
        }
        ConnectionMode::Remote => {
            let jobworkerp_url = non_empty(cfg.remote_jobworkerp_url.as_deref())
                .ok_or_else(|| AppError::Config("remote jobworkerp URL not configured".into()))?;
            let memories_url = non_empty(cfg.remote_memories_url.as_deref())
                .ok_or_else(|| AppError::Config("remote memories URL not configured".into()))?;
            Ok(ResolvedTargets {
                jobworkerp_url: jobworkerp_url.to_string(),
                memories_url: memories_url.to_string(),
            })
        }
    }
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Validate a remote URL with the same rules `grpc::connect` applies, so a
/// malformed URL fails fast on save rather than at first dispatch.
pub fn validate_remote_url(url: &str) -> AppResult<()> {
    if url.trim().is_empty() {
        return Err(AppError::Config("remote URL is empty".into()));
    }
    Endpoint::from_shared(url.to_string())
        .map(|_| ())
        .map_err(|e| AppError::Config(format!("invalid remote URL {url}: {e}")))
}

/// Read the persisted config. A missing file or unparseable JSON falls back to
/// `Default` (local mode) — connection prefs must never block startup.
pub fn load_connection_config(path: &Path) -> ConnectionConfig {
    let Ok(bytes) = std::fs::read(path) else {
        return ConnectionConfig::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save_connection_config(path: &Path, cfg: &ConnectionConfig) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| AppError::Config(format!("serialize connection config: {e}")))?;
    std::fs::write(path, json)?;
    Ok(())
}

#[tauri::command]
pub fn get_connection_config(state: tauri::State<'_, AppState>) -> AppResult<ConnectionConfig> {
    Ok(load_connection_config(&state.data.connection_config_path()))
}

#[tauri::command]
pub async fn set_connection_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    cfg: ConnectionConfig,
) -> AppResult<()> {
    if cfg.mode == ConnectionMode::Remote {
        // Validate before persisting so a typo can't lock the app into an
        // unusable remote target on next launch.
        let jobworkerp = non_empty(cfg.remote_jobworkerp_url.as_deref())
            .ok_or_else(|| AppError::Config("remote jobworkerp URL not configured".into()))?;
        validate_remote_url(jobworkerp)?;

        let memories = non_empty(cfg.remote_memories_url.as_deref())
            .ok_or_else(|| AppError::Config("remote memories URL not configured".into()))?;
        validate_remote_url(memories)?;
        // The memories URL is additionally decomposed into host/port/tls for the
        // workflow callback at dispatch time. Run that same parse here so a URL
        // that `Endpoint::from_shared` tolerates but `parse_callback` can't
        // (e.g. an unsupported scheme) is rejected on save, not mid-import.
        parse_callback(memories)?;
    }
    let config_path = state.data.connection_config_path();
    let old_cfg = load_connection_config(&config_path);
    save_connection_config(&config_path, &cfg)?;
    // Drop cached gRPC clients so the next command reconnects to the new
    // target instead of the previous one (mirrors retry_model_setup).
    state.invalidate_clients().await;

    // When the MCP server is enabled, the connection target is baked into the
    // `lookback_recall` workflow's input defaults at sidecar start
    // (`lifecycle::apply_rag_memories_env`) — the MCP path can't inject it
    // per-dispatch like chat does. A client-cache drop alone leaves those
    // defaults pointing at the OLD memories, so an external MCP search would
    // keep hitting the previous target until the next restart. Restart here so
    // the workflow is re-registered against the NEW connection. Chat / browse
    // need no restart (they inject / reconnect per-call), so this is gated on
    // MCP being on — the common no-MCP case stays a lightweight cache drop.
    if super::mcp_settings::load_mcp_settings(&state.data.mcp_settings_path()).enabled {
        state.sidecars.stop().await?;
        // `start_with_warnings` returns the start Result (unlike
        // `stage_and_start_sidecars`, which only emits an event), so a failed
        // restart is propagated to the caller instead of being swallowed —
        // otherwise the UI clears its dirty state on a save that left the
        // sidecar down with stale MCP defaults. On failure roll the config
        // file back and bring the sidecar up on the OLD target (mirrors
        // `set_embedding_settings`'s rollback).
        let plugin_warnings = match crate::plugins::stage_plugins(&app, &state.data.plugins_dir()) {
            Ok(_) => Vec::new(),
            Err(e) => vec![crate::sidecar::SidecarWarning {
                kind: crate::sidecar::SidecarWarningKind::PluginsStageFailed,
                message: e.to_string(),
                detail: None,
            }],
        };
        match state.sidecars.start_with_warnings(plugin_warnings).await {
            Ok(report) => {
                let _ = app.emit("sidecar://ready", &report);
            }
            Err(e) => {
                tracing::warn!(error = %e, "sidecar failed to restart after a connection change; rolling back");
                super::emit_event(
                    &app,
                    "sidecar://error",
                    crate::sidecar::startup_error::SidecarErrorPayload::Raw {
                        message: format!(
                            "接続先の変更に失敗しました: {e}; 元の接続先にロールバックします"
                        ),
                    },
                );
                let _ = state.sidecars.stop().await;
                let _ = save_connection_config(&config_path, &old_cfg);
                state.invalidate_clients().await;
                crate::stage_and_start_sidecars(&app, &state.sidecars, &state.data).await;
                return Err(AppError::Config(format!(
                    "接続先の変更に失敗しました。元の接続先にロールバックしました: {e}"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_eps() -> SidecarEndpoints {
        SidecarEndpoints {
            jobworkerp_port: 9000,
            memories_port: 9010,
            conductor_port: 9020,
            mcp_server_port: None,
        }
    }

    #[test]
    fn resolve_local_uses_live_endpoints() {
        let cfg = ConnectionConfig::default();
        let eps = local_eps();
        let t = resolve_targets(&cfg, Some(&eps)).unwrap();
        assert_eq!(t.jobworkerp_url, "http://127.0.0.1:9000");
        assert_eq!(t.memories_url, "http://127.0.0.1:9010");
    }

    #[test]
    fn resolve_local_errors_when_sidecars_down() {
        let cfg = ConnectionConfig::default();
        let err = resolve_targets(&cfg, None).unwrap_err();
        assert!(matches!(err, AppError::SidecarNotReady(_)));
    }

    #[test]
    fn resolve_remote_uses_configured_urls() {
        let cfg = ConnectionConfig {
            mode: ConnectionMode::Remote,
            remote_jobworkerp_url: Some("http://10.0.0.2:9000".into()),
            remote_memories_url: Some("http://10.0.0.2:9010".into()),
        };
        // local endpoints are ignored in remote mode.
        let eps = local_eps();
        let t = resolve_targets(&cfg, Some(&eps)).unwrap();
        assert_eq!(t.jobworkerp_url, "http://10.0.0.2:9000");
        assert_eq!(t.memories_url, "http://10.0.0.2:9010");
    }

    #[test]
    fn resolve_remote_errors_when_url_missing() {
        let cfg = ConnectionConfig {
            mode: ConnectionMode::Remote,
            remote_jobworkerp_url: None,
            remote_memories_url: Some("http://10.0.0.2:9010".into()),
        };
        let err = resolve_targets(&cfg, None).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    #[test]
    fn resolve_remote_errors_when_url_blank() {
        let cfg = ConnectionConfig {
            mode: ConnectionMode::Remote,
            remote_jobworkerp_url: Some("http://10.0.0.2:9000".into()),
            remote_memories_url: Some("   ".into()),
        };
        let err = resolve_targets(&cfg, None).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    #[test]
    fn validate_accepts_http_and_https() {
        assert!(validate_remote_url("http://127.0.0.1:9000").is_ok());
        assert!(validate_remote_url("https://example.com:443").is_ok());
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(matches!(validate_remote_url(""), Err(AppError::Config(_))));
        assert!(matches!(
            validate_remote_url("   "),
            Err(AppError::Config(_))
        ));
    }

    #[test]
    fn validate_rejects_malformed() {
        // A string with control characters / spaces can't be a valid URI.
        assert!(matches!(
            validate_remote_url("not a url"),
            Err(AppError::Config(_))
        ));
    }

    #[test]
    fn serde_roundtrips_default() {
        let cfg = ConnectionConfig::default();
        let s = serde_json::to_string(&cfg).unwrap();
        let back: ConnectionConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(cfg, back);
        assert_eq!(back.mode, ConnectionMode::Local);
    }

    #[test]
    fn serde_roundtrips_remote() {
        let cfg = ConnectionConfig {
            mode: ConnectionMode::Remote,
            remote_jobworkerp_url: Some("http://h:9000".into()),
            remote_memories_url: Some("http://h:9010".into()),
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: ConnectionConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn serde_fills_defaults_for_missing_fields() {
        // Forward-compat: an empty object deserializes to the default config.
        let back: ConnectionConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(back, ConnectionConfig::default());
    }

    #[test]
    fn serde_reads_remote_mode_string() {
        let back: ConnectionConfig =
            serde_json::from_str(r#"{"mode":"remote","remote_memories_url":"http://h:9010"}"#)
                .unwrap();
        assert_eq!(back.mode, ConnectionMode::Remote);
        assert_eq!(back.remote_memories_url.as_deref(), Some("http://h:9010"));
        assert_eq!(back.remote_jobworkerp_url, None);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connection.json");
        let cfg = ConnectionConfig {
            mode: ConnectionMode::Remote,
            remote_jobworkerp_url: Some("http://h:9000".into()),
            remote_memories_url: Some("http://h:9010".into()),
        };
        save_connection_config(&path, &cfg).unwrap();
        let back = load_connection_config(&path);
        assert_eq!(cfg, back);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert_eq!(load_connection_config(&path), ConnectionConfig::default());
    }

    #[test]
    fn load_corrupt_json_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connection.json");
        std::fs::write(&path, b"{not json").unwrap();
        // Robustness: a corrupted file must not crash; fall back to local.
        assert_eq!(load_connection_config(&path), ConnectionConfig::default());
    }

    #[test]
    fn parse_callback_http_with_explicit_port() {
        let c = parse_callback("http://127.0.0.1:9010").unwrap();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 9010);
        assert!(!c.tls);
    }

    #[test]
    fn parse_callback_https_with_explicit_port() {
        let c = parse_callback("https://memories.example.com:8443").unwrap();
        assert_eq!(c.host, "memories.example.com");
        assert_eq!(c.port, 8443);
        assert!(c.tls);
    }

    #[test]
    fn parse_callback_defaults_port_by_scheme() {
        let http = parse_callback("http://host").unwrap();
        assert_eq!(http.port, 80);
        assert!(!http.tls);
        let https = parse_callback("https://host").unwrap();
        assert_eq!(https.port, 443);
        assert!(https.tls);
    }

    #[test]
    fn parse_callback_strips_path() {
        let c = parse_callback("https://host:9010/some/path?x=1").unwrap();
        assert_eq!(c.host, "host");
        assert_eq!(c.port, 9010);
    }

    #[test]
    fn parse_callback_rejects_missing_scheme() {
        assert!(matches!(
            parse_callback("127.0.0.1:9010"),
            Err(AppError::Config(_))
        ));
    }

    #[test]
    fn parse_callback_rejects_bad_port() {
        assert!(matches!(
            parse_callback("http://host:notaport"),
            Err(AppError::Config(_))
        ));
    }

    #[test]
    fn parse_callback_ipv6_with_explicit_port() {
        // IPv6 literal with a port: host must keep its brackets so the
        // downstream runner's `format!("{host}:{port}")` round-trips.
        let c = parse_callback("http://[::1]:9010").unwrap();
        assert_eq!(c.host, "[::1]");
        assert_eq!(c.port, 9010);
        assert!(!c.tls);
    }

    #[test]
    fn parse_callback_ipv6_default_port() {
        // Portless IPv6 must NOT fail (the old rsplit_once(':') treated the
        // address colons as a port delimiter and errored).
        let c = parse_callback("https://[2001:db8::1]").unwrap();
        assert_eq!(c.host, "[2001:db8::1]");
        assert_eq!(c.port, 443);
        assert!(c.tls);
    }

    #[test]
    fn parse_callback_drops_userinfo() {
        // Userinfo must not leak into the host (old split kept user:pass@host).
        let c = parse_callback("http://user:pass@host:9010").unwrap();
        assert_eq!(c.host, "host");
        assert_eq!(c.port, 9010);
    }

    #[test]
    fn save_time_validation_accepts_what_dispatch_can_parse() {
        // The set_connection_config guard runs validate_remote_url AND
        // parse_callback on the memories URL, so a save that succeeds must not
        // later fail in memories_callback(). Cover the cases the old splitter
        // broke (IPv6) plus the common ones.
        for url in [
            "http://127.0.0.1:9010",
            "https://memories.example.com:8443",
            "http://[::1]:9010",
            "https://[2001:db8::1]",
            "http://host",
        ] {
            validate_remote_url(url).unwrap_or_else(|e| panic!("validate {url}: {e}"));
            parse_callback(url).unwrap_or_else(|e| panic!("parse {url}: {e}"));
        }
    }

    #[test]
    fn resolved_targets_memories_callback_roundtrip() {
        let t = ResolvedTargets {
            jobworkerp_url: "http://127.0.0.1:9000".into(),
            memories_url: "http://127.0.0.1:9010".into(),
        };
        let c = t.memories_callback().unwrap();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 9010);
        assert!(!c.tls);
    }
}
