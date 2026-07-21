//! Connection-target override.
//!
//! By default the app talks to the bundled local sidecars on their
//! dynamically chosen ports. The user can override this to point at
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

use prost::Message;
use prost_types::FileDescriptorProto;
use serde::{Deserialize, Serialize};
use tauri::Emitter;
use tonic::transport::Endpoint;
use tonic_reflection::pb::v1::{
    ServerReflectionRequest, server_reflection_client::ServerReflectionClient,
    server_reflection_request::MessageRequest,
};

use crate::error::{AppError, AppResult};
use crate::grpc;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectionTestReport {
    pub jobworkerp_url: String,
    pub memories_url: String,
}

const REMOTE_REQUIRED_MEMORIES_SERVICES: [&str; 6] = [
    "llm_memory.service.MemoryService",
    "llm_memory.service.ThreadService",
    "llm_memory.service.ReflectionService",
    "llm_memory.service.MemoryVectorService",
    "llm_memory.service.ThreadVectorService",
    "llm_memory.service.ReflectionVectorService",
];

const REMOTE_REQUIRED_MEMORIES_SCHEMA_FIELDS: [(&str, &str, i32); 2] = [
    (
        "llm_memory.service.FindDistinctLabelsRequest",
        "memory_kinds",
        8,
    ),
    (
        "llm_memory.service.FindCoOccurringLabelsRequest",
        "memory_kinds",
        9,
    ),
];

/// Confirms that a remote memories endpoint exposes the service surface
/// Lookback needs without introducing a version-maintenance RPC.
pub async fn validate_remote_memories_services(url: &str) -> AppResult<()> {
    let channel = crate::grpc::connect(url).await?;
    let request = ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::ListServices(String::new())),
    };
    let mut client = ServerReflectionClient::new(channel);
    let response = client
        .server_reflection_info(tokio_stream::iter([request]))
        .await?
        .into_inner()
        .message()
        .await?
        .ok_or_else(|| {
            AppError::Config("remote memories reflection returned no services".into())
        })?;
    let services = match response.message_response {
        Some(tonic_reflection::pb::v1::server_reflection_response::MessageResponse::ListServicesResponse(list)) => list
            .service
            .into_iter()
            .map(|service| service.name)
            .collect::<std::collections::BTreeSet<_>>(),
        Some(tonic_reflection::pb::v1::server_reflection_response::MessageResponse::ErrorResponse(error)) => {
            return Err(AppError::Config(format!(
                "remote memories reflection failed for {url}: {}",
                error.error_message
            )));
        }
        _ => return Err(AppError::Config(format!("remote memories reflection returned an invalid response for {url}"))),
    };
    let missing = missing_remote_memories_services(&services);
    if !missing.is_empty() {
        Err(AppError::Config(format!(
            "remote memories at {url} is incompatible; missing services: {}",
            missing.join(", ")
        )))?;
    }
    let mut descriptors = Vec::new();
    for (message, ..) in REMOTE_REQUIRED_MEMORIES_SCHEMA_FIELDS {
        descriptors.extend(reflect_file_containing_symbol(&mut client, message, url).await?);
    }
    let missing = missing_remote_memories_schema_fields(&descriptors);
    if missing.is_empty() {
        Ok(())
    } else {
        Err(AppError::Config(format!(
            "remote memories at {url} is incompatible; missing reflection fields: {}",
            missing.join(", ")
        )))
    }
}

async fn reflect_file_containing_symbol(
    client: &mut ServerReflectionClient<tonic::transport::Channel>,
    symbol: &str,
    url: &str,
) -> AppResult<Vec<FileDescriptorProto>> {
    let request = ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::FileContainingSymbol(symbol.to_string())),
    };
    let response = client
        .server_reflection_info(tokio_stream::iter([request]))
        .await?
        .into_inner()
        .message()
        .await?
        .ok_or_else(|| {
            AppError::Config(format!(
                "remote memories reflection returned no descriptor for {symbol}"
            ))
        })?;
    match response.message_response {
        Some(tonic_reflection::pb::v1::server_reflection_response::MessageResponse::FileDescriptorResponse(response)) => response
            .file_descriptor_proto
            .into_iter()
            .map(|encoded| FileDescriptorProto::decode(encoded.as_ref()).map_err(|error| AppError::Config(format!(
                "remote memories reflection returned an invalid descriptor for {symbol} at {url}: {error}"
            ))))
            .collect(),
        Some(tonic_reflection::pb::v1::server_reflection_response::MessageResponse::ErrorResponse(error)) => {
            Err(AppError::Config(format!(
                "remote memories reflection failed for {symbol} at {url}: {}",
                error.error_message
            )))
        }
        _ => Err(AppError::Config(format!(
            "remote memories reflection returned an invalid descriptor response for {symbol} at {url}"
        ))),
    }
}

fn missing_remote_memories_services(
    services: &std::collections::BTreeSet<String>,
) -> Vec<&'static str> {
    REMOTE_REQUIRED_MEMORIES_SERVICES
        .iter()
        .filter(|required| !services.contains(**required))
        .copied()
        .collect()
}

fn missing_remote_memories_schema_fields(files: &[FileDescriptorProto]) -> Vec<String> {
    REMOTE_REQUIRED_MEMORIES_SCHEMA_FIELDS
        .iter()
        .filter(|(message, field, number)| {
            !descriptor_has_memory_kind_field(files, message, field, *number)
        })
        .map(|(message, field, _)| format!("{message}.{field}"))
        .collect()
}

fn descriptor_has_memory_kind_field(
    files: &[FileDescriptorProto],
    target_message: &str,
    target_field: &str,
    target_number: i32,
) -> bool {
    files.iter().any(|file| {
        let package = file.package.as_deref().unwrap_or_default();
        file.message_type.iter().any(|message| {
            let Some(name) = message.name.as_deref() else {
                return false;
            };
            let full_name = if package.is_empty() {
                name.to_string()
            } else {
                format!("{package}.{name}")
            };
            full_name == target_message
                && message.field.iter().any(|field| {
                    field.name.as_deref() == Some(target_field)
                        && field.number == Some(target_number)
                        && field.label
                            == Some(prost_types::field_descriptor_proto::Label::Repeated as i32)
                        && field.r#type
                            == Some(prost_types::field_descriptor_proto::Type::Enum as i32)
                        && field.type_name.as_deref() == Some(".llm_memory.data.MemoryKind")
                })
        })
    })
}

/// Returns whether the saved configuration changes either endpoint that is
/// currently active. Remote URL fields are intentionally ignored in local
/// mode: they are only draft values until the mode actually changes.
fn active_connection_targets_changed(old: &ConnectionConfig, new: &ConnectionConfig) -> bool {
    old.mode != new.mode
        || (old.mode == ConnectionMode::Remote
            && (old.remote_jobworkerp_url != new.remote_jobworkerp_url
                || old.remote_memories_url != new.remote_memories_url))
}

fn should_restart_sidecars_for_connection_change(
    old: &ConnectionConfig,
    new: &ConnectionConfig,
    sidecars_running: bool,
    mcp_enabled: bool,
) -> bool {
    sidecars_running && (mcp_enabled || active_connection_targets_changed(old, new))
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
            let memories_url = eps.memories_url().ok_or_else(|| {
                AppError::SidecarNotReady("local memories sidecar not started".into())
            })?;
            Ok(ResolvedTargets {
                jobworkerp_url: eps.jobworkerp_url(),
                memories_url,
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

pub fn target_connect_error(
    endpoint_name: &'static str,
    url: &str,
    error: impl std::fmt::Display,
) -> AppError {
    AppError::Config(format!(
        "{endpoint_name} connection failed ({url}): {error}"
    ))
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
pub async fn test_connection_config(
    state: tauri::State<'_, AppState>,
    cfg: ConnectionConfig,
) -> AppResult<ConnectionTestReport> {
    if cfg.mode == ConnectionMode::Remote {
        let jobworkerp = non_empty(cfg.remote_jobworkerp_url.as_deref())
            .ok_or_else(|| AppError::Config("remote jobworkerp URL not configured".into()))?;
        validate_remote_url(jobworkerp)?;

        let memories = non_empty(cfg.remote_memories_url.as_deref())
            .ok_or_else(|| AppError::Config("remote memories URL not configured".into()))?;
        validate_remote_url(memories)?;
        parse_callback(memories)?;
    }

    let local = state.sidecars.current_endpoints();
    let targets = resolve_targets(&cfg, local.as_ref())?;

    crate::jobworkerp::JobworkerpHandle::connect(&targets.jobworkerp_url)
        .await
        .map_err(|e| {
            tracing::warn!(
                endpoint = "jobworkerp",
                url = %targets.jobworkerp_url,
                error = %e,
                "connection test failed"
            );
            target_connect_error("jobworkerp", &targets.jobworkerp_url, e)
        })?;
    grpc::connect(&targets.memories_url).await.map_err(|e| {
        tracing::warn!(
            endpoint = "memories",
            url = %targets.memories_url,
            error = %e,
            "connection test failed"
        );
        target_connect_error("memories", &targets.memories_url, e)
    })?;
    if cfg.mode == ConnectionMode::Remote {
        validate_remote_memories_services(&targets.memories_url).await?;
    }

    Ok(ConnectionTestReport {
        jobworkerp_url: targets.jobworkerp_url,
        memories_url: targets.memories_url,
    })
}

#[tauri::command]
pub async fn set_connection_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    cfg: ConnectionConfig,
) -> AppResult<()> {
    if cfg.mode == ConnectionMode::Remote {
        // Validate the complete startup contract before persisting so an
        // incompatible remote target cannot lock the app into a failed boot.
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
        validate_remote_memories_services(memories).await?;
    }
    let config_path = state.data.connection_config_path();
    let old_cfg = load_connection_config(&config_path);
    let sidecars_running = state.sidecars.current_endpoints().is_some();
    let mcp_enabled =
        super::mcp_settings::load_mcp_settings(&state.data.mcp_settings_path()).enabled;
    save_connection_config(&config_path, &cfg)?;
    // Drop cached gRPC clients so the next command reconnects to the new
    // target instead of the previous one (mirrors retry_model_setup).
    state.invalidate_clients().await;

    // Workflow runtime is assembled at sidecar start. Besides the MCP recall
    // workflow, periodic summary/reflection jobs also retain the memories
    // callback in conductor. Restart every running sidecar set when its active
    // connection target changes so no scheduled write continues to the old
    // memories endpoint. MCP retains its existing always-refresh behavior.
    if should_restart_sidecars_for_connection_change(&old_cfg, &cfg, sidecars_running, mcp_enabled)
    {
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
    use prost_types::{DescriptorProto, FieldDescriptorProto, FileDescriptorProto};

    #[test]
    fn remote_reflection_requires_the_lookback_service_surface() {
        let services = REMOTE_REQUIRED_MEMORIES_SERVICES
            .iter()
            .map(|service| (*service).to_string())
            .collect();
        assert!(missing_remote_memories_services(&services).is_empty());

        let missing = missing_remote_memories_services(&std::collections::BTreeSet::new());
        assert_eq!(missing, REMOTE_REQUIRED_MEMORIES_SERVICES);
    }

    #[test]
    fn remote_reflection_requires_raw_label_filter_fields() {
        let legacy = vec![thread_label_request_descriptors(&[])];
        assert_eq!(
            missing_remote_memories_schema_fields(&legacy),
            vec![
                "llm_memory.service.FindDistinctLabelsRequest.memory_kinds",
                "llm_memory.service.FindCoOccurringLabelsRequest.memory_kinds",
            ]
        );

        let current = vec![thread_label_request_descriptors(&[
            "FindDistinctLabelsRequest",
            "FindCoOccurringLabelsRequest",
        ])];
        assert!(missing_remote_memories_schema_fields(&current).is_empty());

        let mut malformed = thread_label_request_descriptors(&[
            "FindDistinctLabelsRequest",
            "FindCoOccurringLabelsRequest",
        ]);
        malformed.message_type[1].field[0].number = Some(8);
        assert_eq!(
            missing_remote_memories_schema_fields(&[malformed]),
            vec!["llm_memory.service.FindCoOccurringLabelsRequest.memory_kinds"]
        );
    }

    fn thread_label_request_descriptors(with_memory_kinds: &[&str]) -> FileDescriptorProto {
        let message = |name: &str| DescriptorProto {
            name: Some(name.to_string()),
            field: with_memory_kinds
                .contains(&name)
                .then(|| FieldDescriptorProto {
                    name: Some("memory_kinds".to_string()),
                    number: Some(if name == "FindDistinctLabelsRequest" {
                        8
                    } else {
                        9
                    }),
                    label: Some(prost_types::field_descriptor_proto::Label::Repeated as i32),
                    r#type: Some(prost_types::field_descriptor_proto::Type::Enum as i32),
                    type_name: Some(".llm_memory.data.MemoryKind".to_string()),
                    ..Default::default()
                })
                .into_iter()
                .collect(),
            ..Default::default()
        };
        FileDescriptorProto {
            package: Some("llm_memory.service".to_string()),
            message_type: vec![
                message("FindDistinctLabelsRequest"),
                message("FindCoOccurringLabelsRequest"),
            ],
            ..Default::default()
        }
    }

    fn local_eps() -> SidecarEndpoints {
        SidecarEndpoints {
            jobworkerp_port: 9000,
            memories_port: Some(9010),
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
    fn target_connect_error_includes_endpoint_and_url() {
        let err = target_connect_error("memories", "http://10.0.0.2:9010", "connection refused");
        let msg = err.to_string();
        assert!(msg.contains("memories connection failed"));
        assert!(msg.contains("http://10.0.0.2:9010"));
        assert!(msg.contains("connection refused"));
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
        // The URL-shape portion of set_connection_config's guard runs
        // validate_remote_url and parse_callback before its live Reflection
        // check. A URL that passes this pure validation must not later fail in
        // memories_callback(). Cover the cases the old splitter broke (IPv6)
        // plus the common ones.
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

    #[test]
    fn running_sidecars_restart_when_active_connection_target_changes() {
        let local = ConnectionConfig::default();
        let remote_a = ConnectionConfig {
            mode: ConnectionMode::Remote,
            remote_jobworkerp_url: Some("http://remote-a:9000".into()),
            remote_memories_url: Some("http://remote-a:9010".into()),
        };
        let remote_b = ConnectionConfig {
            remote_memories_url: Some("http://remote-b:9010".into()),
            ..remote_a.clone()
        };

        assert!(should_restart_sidecars_for_connection_change(
            &local, &remote_a, true, false
        ));
        assert!(should_restart_sidecars_for_connection_change(
            &remote_a, &remote_b, true, false
        ));
        assert!(!should_restart_sidecars_for_connection_change(
            &remote_a, &remote_b, false, false
        ));
    }

    #[test]
    fn local_mode_does_not_restart_for_dormant_remote_url_edits() {
        let local = ConnectionConfig::default();
        let edited_remote = ConnectionConfig {
            remote_jobworkerp_url: Some("http://remote:9000".into()),
            remote_memories_url: Some("http://remote:9010".into()),
            ..local.clone()
        };

        assert!(!should_restart_sidecars_for_connection_change(
            &local,
            &edited_remote,
            true,
            false
        ));
        // Preserve the existing MCP invariant: its workflow defaults are
        // rebuilt whenever MCP settings request a restart.
        assert!(should_restart_sidecars_for_connection_change(
            &local,
            &edited_remote,
            true,
            true
        ));
    }
}
