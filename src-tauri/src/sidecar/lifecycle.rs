//! Sidecar process lifecycle.
//!
//! Spawn jobworkerp `all-in-one` and memories `memories-front` as child
//! processes when the Tauri app launches; stop them — via SIGTERM with a
//! short grace window before SIGKILL — when the app exits or `Drop` runs.
//!
//! Health is determined by polling the gRPC port until a TCP connection
//! succeeds (poor man's health check; gRPC `tonic_health` Check is the
//! follow-up once we know what services are registered).

use super::query_embedding_worker::MemoriesQueryEmbedding;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::future::Future;
#[cfg(target_os = "linux")]
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep, timeout};
use tracing::{Level, debug, info, warn};

use crate::data::DataPaths;
use crate::error::{AppError, AppResult};
use crate::sidecar::ports;
use crate::sidecar::readiness::{ReadinessRetryError, retry_until_ready};
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

fn apply_memories_child_env(command: &mut Command, vars: BTreeMap<OsString, OsString>) {
    for (key, value) in vars {
        // base_cmd owns the filter, format and native-log destination for every
        // sidecar. Reapplying Memories' env file must not undo that policy.
        if [
            "RUST_LOG",
            "LOG_APP_NAME",
            "LOG_FILE_DIR",
            "LOG_USE_JSON",
            "LOG_USE_STDOUT",
        ]
        .iter()
        .any(|managed| key == OsStr::new(managed))
        {
            continue;
        }
        command.env(key, value);
    }
}
const MEMORIES_START_TIMEOUT: Duration = Duration::from_secs(60);
const SIDECAR_GRPC_READY_TIMEOUT: Duration = Duration::from_secs(30);
const SIDECAR_GRPC_READY_RETRY_INTERVAL: Duration = Duration::from_millis(200);
const FULL_RESTART_POST_START_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(target_os = "linux")]
const MAX_MANAGED_SDK_CONFIG_BYTES: usize = 1024 * 1024;
const MANAGED_SDK_CONFIG_PATH: &str = "/etc/microsandbox/managed.json";

/// Process-local identity for one jobworkerp spawn. This identifies a managed
/// child replacement only; it is not evidence of a Microsandbox VM generation.
static NEXT_JOBWORKERP_SPAWN_GENERATION: AtomicU64 = AtomicU64::new(1);

/// The worker manifests, with the Issue extraction and gateway workers while
/// Issue tracking is on.
fn issue_worker_yamls(base: &[PathBuf], issue_tracking_enabled: bool) -> Vec<PathBuf> {
    let mut paths = base.to_vec();
    paths.extend(issue_tracker::extra_issue_tracker_worker_yaml_paths(
        issue_tracking_enabled,
    ));
    paths
}

fn worker_paths_with_issue_chat(base: &[PathBuf], issue_tracking_enabled: bool) -> Vec<PathBuf> {
    let mut paths = issue_worker_yamls(base, issue_tracking_enabled);
    if issue_tracking_enabled
        && std::env::var("LOOKBACK_ISSUE_CHAT_WORKER_PORT").is_ok()
        && let Ok(path) = crate::data::paths::lookback_issue_chat_workers_yaml()
    {
        paths.push(path);
    }
    paths
}

const LOOKBACK_RAG_WORKER_NAMES: [&str; 3] = [
    "lookback_recall",
    "lookback_get_context",
    "lookback_get_reflections",
];
/// The cross-thread Issue resolution embeds its search query with the same
/// worker and reads its own last successful run from jobworkerp.
const CROSS_THREAD_RESOLUTION_WORKER_NAMES: [&str; 3] = [
    "lookback-cross-thread-issue-resolution",
    "lookback-cross-thread-issue-resolution-manual",
    "lookback-cross-thread-issue-resolution-candidate",
];
const LOOKBACK_MM_EMBEDDING_WORKER_PLACEHOLDER: &str = "LOOKBACK_MM_EMBEDDING_WORKER";
const MEMORY_EMBEDDING_QUERY_PREFIX_PLACEHOLDER: &str = "MEMORY_EMBEDDING_QUERY_PREFIX";
const LOOKBACK_JOBWORKERP_GRPC_HOST_PLACEHOLDER: &str = "LOOKBACK_JOBWORKERP_GRPC_HOST";
const LOOKBACK_JOBWORKERP_GRPC_PORT_PLACEHOLDER: &str = "LOOKBACK_JOBWORKERP_GRPC_PORT";
const LOOKBACK_JOBWORKERP_GRPC_TLS_PLACEHOLDER: &str = "LOOKBACK_JOBWORKERP_GRPC_TLS";

/// The directory a manifest's relative `$file` includes resolve against.
fn yaml_base_dir(yaml: &Path) -> &Path {
    yaml.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Workers whose workflows need values only known once Memories is up.
fn is_memories_dependent_worker(worker: &serde_yaml::Value) -> bool {
    worker
        .get("name")
        .and_then(serde_yaml::Value::as_str)
        .is_some_and(|name| {
            LOOKBACK_RAG_WORKER_NAMES.contains(&name)
                || CROSS_THREAD_RESOLUTION_WORKER_NAMES.contains(&name)
        })
}

fn parse_expanded_worker_yaml(yaml_path: &Path) -> AppResult<serde_yaml::Value> {
    let raw = std::fs::read_to_string(yaml_path).map_err(|error| {
        AppError::WorkerRegistration(format!(
            "read workers YAML {}: {error}",
            yaml_path.display()
        ))
    })?;
    let expanded = jobworkerp_client::client::yaml_common::expand_env(&raw).map_err(|error| {
        AppError::WorkerRegistration(format!(
            "expand workers YAML {}: {error:#}",
            yaml_path.display()
        ))
    })?;
    serde_yaml::from_str(&expanded).map_err(|error| {
        AppError::WorkerRegistration(format!(
            "parse workers YAML {}: {error}",
            yaml_path.display()
        ))
    })
}

/// Keep RAG workers out of the pre-Memories apply. Their included workflow
/// needs the exact embedding worker name returned by Memories at runtime.
fn worker_yaml_without_memories_dependent(yaml_path: &Path) -> AppResult<Option<String>> {
    let mut document = parse_expanded_worker_yaml(yaml_path)?;
    let workers = document
        .get_mut("workers")
        .and_then(serde_yaml::Value::as_sequence_mut)
        .ok_or_else(|| {
            AppError::WorkerRegistration(format!(
                "workers YAML {} has no workers list",
                yaml_path.display()
            ))
        })?;
    let original_len = workers.len();
    workers.retain(|worker| !is_memories_dependent_worker(worker));
    if workers.len() == original_len {
        return Ok(None);
    }
    serde_yaml::to_string(&document).map(Some).map_err(|error| {
        AppError::WorkerRegistration(format!(
            "render workers YAML without RAG tools {}: {error}",
            yaml_path.display()
        ))
    })
}

/// Resolve the `$file:` overlays of the workers that embed queries (RAG tools,
/// cross-thread Issue resolution) only after Memories can supply its
/// configured, space-scoped worker name. This avoids deriving a suffix or
/// setting process-global environment variables in the running app.
/// `jobworkerp` is jobworkerp as its own workflows reach it.
async fn memories_dependent_worker_yaml(
    yaml_path: &Path,
    query_embedding: &MemoriesQueryEmbedding,
    jobworkerp: &crate::commands::connection::GrpcAddress,
) -> AppResult<Option<String>> {
    let mm_embedding_worker_name = query_embedding.worker_name.as_str();
    if mm_embedding_worker_name.trim().is_empty() {
        return Err(AppError::WorkerRegistration(
            "Memories returned an empty mm-embedding worker name".into(),
        ));
    }

    let mut document = parse_expanded_worker_yaml(yaml_path)?;
    let workers = document
        .get_mut("workers")
        .and_then(serde_yaml::Value::as_sequence_mut)
        .ok_or_else(|| {
            AppError::WorkerRegistration(format!(
                "workers YAML {} has no workers list",
                yaml_path.display()
            ))
        })?;
    workers.retain(is_memories_dependent_worker);
    if workers.is_empty() {
        return Ok(None);
    }

    let mut overrides = std::collections::HashMap::from([
        (
            LOOKBACK_MM_EMBEDDING_WORKER_PLACEHOLDER.to_string(),
            mm_embedding_worker_name.to_string(),
        ),
        (
            LOOKBACK_JOBWORKERP_GRPC_HOST_PLACEHOLDER.to_string(),
            jobworkerp.host.clone(),
        ),
        (
            LOOKBACK_JOBWORKERP_GRPC_PORT_PLACEHOLDER.to_string(),
            jobworkerp.port.to_string(),
        ),
        (
            LOOKBACK_JOBWORKERP_GRPC_TLS_PLACEHOLDER.to_string(),
            jobworkerp.tls.to_string(),
        ),
    ]);
    if let Some(prefix) = &query_embedding.query_prefix {
        overrides.insert(
            MEMORY_EMBEDDING_QUERY_PREFIX_PLACEHOLDER.to_string(),
            prefix.clone(),
        );
    }
    let base_dir = yaml_base_dir(yaml_path);
    jobworkerp_client::client::yaml_common::resolve_includes_with_overrides(
        &mut document,
        base_dir,
        &overrides,
    )
    .await
    .map_err(|error| {
        AppError::WorkerRegistration(format!(
            "resolve RAG workflow includes from {}: {error:#}",
            yaml_path.display()
        ))
    })?;
    serde_yaml::to_string(&document).map(Some).map_err(|error| {
        AppError::WorkerRegistration(format!(
            "render RAG workers YAML {}: {error}",
            yaml_path.display()
        ))
    })
}

fn mm_embedding_worker_name_from_response(
    response: &crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceResponse,
) -> AppResult<String> {
    // The protobuf contract returns this name for both registered and pending
    // states. Pending is valid here: it describes the current configured name,
    // not proof that the runner is already available.
    match response.registration_state {
        1 | 2 => {}
        0 => {
            return Err(AppError::WorkerRegistration(
                "Memories returned an unspecified embedding worker registration state".into(),
            ));
        }
        other => {
            return Err(AppError::WorkerRegistration(format!(
                "Memories returned an unknown embedding worker registration state: {other}"
            )));
        }
    }
    if response.mm_embedding_worker_name.trim().is_empty() {
        return Err(AppError::WorkerRegistration(
            "Memories returned an empty mm-embedding worker name".into(),
        ));
    }
    Ok(response.mm_embedding_worker_name.clone())
}

async fn current_embedding_space(
    memories_url: &str,
) -> AppResult<crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceResponse> {
    use crate::grpc::proto::llm_memory::service::{
        GetEmbeddingSpaceRequest, embedding_space_service_client::EmbeddingSpaceServiceClient,
    };

    let channel = crate::grpc::connect(memories_url).await?;
    let mut client = EmbeddingSpaceServiceClient::new(channel);
    let response = client
        .get_embedding_space(GetEmbeddingSpaceRequest {})
        .await
        .map_err(|error| {
            AppError::WorkerRegistration(format!(
                "query Memories GetEmbeddingSpace for the current worker name: {error}"
            ))
        })?
        .into_inner();
    mm_embedding_worker_name_from_response(&response)?;
    Ok(response)
}

fn record_worker_registration_result(
    yaml: &Path,
    result: AppResult<
        std::collections::HashMap<String, jobworkerp_client::jobworkerp::data::WorkerId>,
    >,
    issue_tracking_enabled: bool,
    warnings: &mut Vec<SidecarWarning>,
) -> AppResult<()> {
    match result {
        Ok(map) => info!(yaml = %yaml.display(), count = map.len(), "workers registered"),
        Err(error) => {
            if !issue_tracking_enabled {
                return Err(error);
            }
            warn!(yaml = %yaml.display(), %error, "worker registration failed (continuing)");
            warnings.push(SidecarWarning {
                kind: SidecarWarningKind::WorkerApplyFailed,
                message: error.to_string(),
                detail: Some(yaml.display().to_string()),
            });
        }
    }
    Ok(())
}

async fn register_runtime_workers_before_memories(
    handle: &crate::jobworkerp::JobworkerpHandle,
    worker_yamls: &[PathBuf],
    issue_tracking_enabled: bool,
    warnings: &mut Vec<SidecarWarning>,
) -> AppResult<()> {
    for yaml in worker_yamls {
        let result = match worker_yaml_without_memories_dependent(yaml) {
            Ok(Some(body)) => {
                let base_dir = yaml_base_dir(yaml);
                handle.register_workers_from_yaml_str(&body, base_dir).await
            }
            Ok(None) => handle.register_workers_from_yaml(yaml).await,
            Err(error) => Err(error),
        };
        record_worker_registration_result(yaml, result, issue_tracking_enabled, warnings)?;
    }
    Ok(())
}

async fn register_runtime_memories_dependent_workers(
    handle: &crate::jobworkerp::JobworkerpHandle,
    worker_yamls: &[PathBuf],
    query_embedding: &MemoriesQueryEmbedding,
    jobworkerp: &crate::commands::connection::GrpcAddress,
    issue_tracking_enabled: bool,
    warnings: &mut Vec<SidecarWarning>,
) -> AppResult<()> {
    for yaml in worker_yamls {
        let result = match memories_dependent_worker_yaml(yaml, query_embedding, jobworkerp).await {
            Ok(Some(body)) => {
                let base_dir = yaml_base_dir(yaml);
                handle.register_workers_from_yaml_str(&body, base_dir).await
            }
            Ok(None) => continue,
            Err(error) => Err(error),
        };
        record_worker_registration_result(yaml, result, issue_tracking_enabled, warnings)?;
    }
    Ok(())
}

async fn register_runtime_function_sets(
    handle: &crate::jobworkerp::JobworkerpHandle,
    function_sets: &[PathBuf],
    data_root: &Path,
    issue_tracking_enabled: bool,
    warnings: &mut Vec<SidecarWarning>,
) -> AppResult<()> {
    if function_sets.len() != 1 {
        return Err(AppError::WorkerRegistration(
            "Issue registration requires exactly one canonical FunctionSet manifest".into(),
        ));
    }
    let staged_sets = function_sets
        .iter()
        .map(|source| {
            issue_tool_registration::stage_function_set_manifest(
                source,
                data_root,
                issue_tracking_enabled,
            )
        })
        .collect::<AppResult<Vec<_>>>()?;
    register_staged_function_sets(handle, &staged_sets, issue_tracking_enabled, warnings).await?;
    if !issue_tracking_enabled {
        for name in issue_tool_registration::ISSUE_WORKER_NAMES {
            handle.delete_worker_by_name_if_present(name).await?;
        }
        for name in issue_tool_registration::ISSUE_WORKER_NAMES {
            if handle.worker_id_by_name(name).await?.is_some() {
                return Err(AppError::WorkerRegistration(format!(
                    "disabled Issue worker is still registered: {name}"
                )));
            }
        }
        for set_name in ["lookback-chat", "lookback-mcp"] {
            let json = handle.fetch_function_set_as_tools_json(set_name).await?;
            validate_mcp_toolset_json_for_issue_tracking(&json, false)?;
        }
    } else {
        verify_enabled_issue_registration(handle).await?;
    }
    Ok(())
}

/// Register staged function-set manifests. With `warn_on_failure` a failed
/// manifest becomes a warning instead of an error.
async fn register_staged_function_sets(
    handle: &crate::jobworkerp::JobworkerpHandle,
    staged_sets: &[PathBuf],
    warn_on_failure: bool,
    warnings: &mut Vec<SidecarWarning>,
) -> AppResult<()> {
    for yaml in staged_sets {
        match handle.register_function_sets_from_yaml(yaml).await {
            Ok(map) => info!(yaml = %yaml.display(), count = map.len(), "function sets registered"),
            Err(error) => {
                if !warn_on_failure {
                    return Err(error);
                }
                warn!(yaml = %yaml.display(), %error, "function set registration failed (continuing)");
                warnings.push(SidecarWarning {
                    kind: SidecarWarningKind::WorkerApplyFailed,
                    message: error.to_string(),
                    detail: Some(yaml.display().to_string()),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
async fn register_runtime_tools(
    handle: &crate::jobworkerp::JobworkerpHandle,
    base_workers: &[PathBuf],
    function_sets: &[PathBuf],
    data_root: &Path,
    issue_tracking_enabled: bool,
    warnings: &mut Vec<SidecarWarning>,
) -> AppResult<()> {
    for yaml in worker_paths_with_issue_chat(base_workers, issue_tracking_enabled) {
        let result = handle.register_workers_from_yaml(&yaml).await;
        record_worker_registration_result(&yaml, result, issue_tracking_enabled, warnings)?;
    }
    register_runtime_function_sets(
        handle,
        function_sets,
        data_root,
        issue_tracking_enabled,
        warnings,
    )
    .await
}

async fn verify_enabled_issue_registration(
    handle: &crate::jobworkerp::JobworkerpHandle,
) -> AppResult<()> {
    for name in issue_tool_registration::ISSUE_WORKER_NAMES {
        if handle.worker_id_by_name(name).await?.is_none() {
            return Err(AppError::WorkerRegistration(format!(
                "enabled Issue worker is not registered: {name}"
            )));
        }
    }
    for (set, issue_names) in [
        (
            "lookback-chat",
            &issue_tool_registration::ISSUE_WORKER_NAMES[..5],
        ),
        (
            "lookback-mcp",
            &issue_tool_registration::ISSUE_WORKER_NAMES[5..10],
        ),
    ] {
        let json = handle.fetch_function_set_as_tools_json(set).await?;
        let tools: Vec<serde_json::Value> = serde_json::from_str(&json)
            .map_err(|error| AppError::WorkerRegistration(format!("parse {set} tools: {error}")))?;
        let expected = MCP_EXPECTED_TOOL_NAMES[..3]
            .iter()
            .copied()
            .chain(issue_names.iter().copied())
            .chain(
                issue_tool_registration::issue_agent_tool_names()
                    .iter()
                    .copied(),
            )
            .collect::<std::collections::HashSet<_>>();
        let actual = tools
            .iter()
            .filter_map(|tool| {
                tool.pointer("/function/name")
                    .and_then(serde_json::Value::as_str)
            })
            .collect::<std::collections::HashSet<_>>();
        if actual != expected || tools.len() != expected.len() {
            return Err(AppError::WorkerRegistration(format!(
                "enabled FunctionSet {set} does not expose the expected Issue and memory tools"
            )));
        }
    }
    Ok(())
}

/// Bound post-start work so a stalled scheduler resume cannot hold the
/// lifecycle lock forever.
async fn run_full_restart_post_start<F, Fut>(post_start: F, budget: Duration) -> AppResult<()>
where
    F: Fn() -> Fut,
    Fut: Future<Output = AppResult<()>>,
{
    timeout(budget, post_start())
        .await
        .map_err(|_| AppError::Config("full restart post-start continuation timed out".into()))?
}

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

/// Return a fresh jobworkerp handle only after a RunnerService RPC succeeds.
/// A TCP listener may accept connections before its HTTP/2 service is ready.
async fn wait_for_jobworkerp_grpc_ready(
    server_url: &str,
) -> AppResult<crate::jobworkerp::JobworkerpHandle> {
    retry_until_ready(
        SIDECAR_GRPC_READY_TIMEOUT,
        SIDECAR_GRPC_READY_RETRY_INTERVAL,
        || async {
            let handle = crate::jobworkerp::JobworkerpHandle::connect(server_url).await?;
            handle.probe_runner_service().await?;
            Ok::<_, AppError>(handle)
        },
        |_| true,
    )
    .await
    .map_err(|error| match error {
        ReadinessRetryError::Probe(error) => AppError::SidecarNotReady(format!(
            "jobworkerp gRPC readiness probe failed within {:?}: {error}",
            SIDECAR_GRPC_READY_TIMEOUT
        )),
        ReadinessRetryError::TimedOut => AppError::SidecarNotReady(format!(
            "jobworkerp gRPC readiness probe did not respond within {:?}",
            SIDECAR_GRPC_READY_TIMEOUT
        )),
    })
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

const MCP_TOKEN_STATE_IDLE: u8 = 0;
const MCP_TOKEN_STATE_LAUNCH_OR_LIVE: u8 = 1;
const MCP_TOKEN_STATE_MUTATING: u8 = 2;

/// Secret files owned by one jobworkerp launch. The values are deliberately
/// not retained here: the child reads them from disk and the lifecycle only
/// needs to keep the paths alive until the child is stopped.
struct McpLaunchSecrets {
    auth_token_file: PathBuf,
}

impl Drop for McpLaunchSecrets {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.auth_token_file);
    }
}

/// These keys must be removed even before the MCP settings module projects
/// them into its managed-key list. This prevents an inherited secret path or
/// auth flag from enabling an unconfigured child.
const MCP_SECRET_ENV_KEYS: [&str; 2] = ["MCP_AUTH_ENABLED", "MCP_AUTH_TOKEN_FILE"];

fn mcp_auth_enabled(mcp_enabled: bool, allow_unauthenticated_requests: bool) -> bool {
    mcp_enabled && !allow_unauthenticated_requests
}

fn mcp_launch_env_vars(
    auth_enabled: bool,
    secrets: Option<&McpLaunchSecrets>,
) -> Vec<(&'static str, String)> {
    if !auth_enabled {
        return vec![("MCP_AUTH_ENABLED", "false".to_string())];
    }
    let Some(secrets) = secrets else {
        // Keep authentication enabled so the MCP server fails closed when an
        // unexpected launch path omitted the required secret file.
        return vec![("MCP_AUTH_ENABLED", "true".to_string())];
    };
    vec![
        ("MCP_AUTH_ENABLED", "true".to_string()),
        (
            "MCP_AUTH_TOKEN_FILE",
            secrets.auth_token_file.to_string_lossy().into_owned(),
        ),
    ]
}

fn apply_mcp_launch_auth(
    cmd: &mut Command,
    auth_enabled: bool,
    secrets: Option<&McpLaunchSecrets>,
) {
    for (key, value) in mcp_launch_env_vars(auth_enabled, secrets) {
        cmd.env(key, value);
    }
}

fn mcp_listener_port(enabled: bool, listener_ready: bool, port: u16) -> Option<u16> {
    (enabled && listener_ready).then_some(port)
}

/// Keep the address of a live MCP listener across a partial restart. When MCP
/// is enabled from an off state there is no previous address, so use the same
/// preferred-port-with-OS-fallback policy as a full sidecar start.
fn select_restart_mcp_port(existing_port: Option<u16>) -> AppResult<u16> {
    select_restart_mcp_port_with(existing_port, ports::pick)
}

fn select_restart_mcp_port_with(
    existing_port: Option<u16>,
    pick_port: impl FnOnce(u16) -> AppResult<u16>,
) -> AppResult<u16> {
    match existing_port {
        Some(port) => Ok(port),
        None => pick_port(crate::commands::mcp_settings::MCP_DEFAULT_PORT),
    }
}

/// A connection restart keeps a live local memories port, but Remote → Local
/// has no prior local child to reuse. In that case, select a fresh port with
/// the same preferred-port-with-OS-fallback policy as initial startup.
fn select_restart_memories_port(
    mode: crate::commands::connection::ConnectionMode,
    existing_port: Option<u16>,
) -> AppResult<Option<u16>> {
    select_restart_memories_port_with(mode, existing_port, ports::pick)
}

fn select_restart_memories_port_with(
    mode: crate::commands::connection::ConnectionMode,
    existing_port: Option<u16>,
    pick_port: impl FnOnce(u16) -> AppResult<u16>,
) -> AppResult<Option<u16>> {
    match mode {
        crate::commands::connection::ConnectionMode::Local => {
            Ok(Some(existing_port.unwrap_or(pick_port(9010)?)))
        }
        crate::commands::connection::ConnectionMode::Remote => Ok(None),
    }
}

/// A selected restart advertises a local memories endpoint only after it
/// satisfies the same RPC contract checked during a full startup. Keeping the
/// validator injectable makes the error boundary explicit without requiring a
/// spawned gRPC fixture in lifecycle tests.
async fn validate_selected_local_memories_services_with<F, Fut>(
    port: u16,
    validate: F,
) -> AppResult<()>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = AppResult<()>>,
{
    validate(loopback_url(port)).await
}

/// Derive the endpoint substituted into worker YAML callback placeholders.
/// This must be shared by full and selected restarts: registration expands
/// `MEMORY_GRPC_*` in this process, not in the jobworkerp child.
fn resolve_worker_memories_callback(
    connection: &crate::commands::connection::ConnectionConfig,
    local_memories_port: Option<u16>,
) -> AppResult<crate::commands::connection::GrpcAddress> {
    crate::commands::connection::parse_grpc_address(&crate::commands::connection::memories_url(
        connection,
        local_memories_port,
    )?)
}

fn mcp_runtime_readiness(
    enabled: bool,
    listener_ready: bool,
    toolset_error: Option<String>,
) -> (McpRuntimeStatus, Option<String>) {
    if !enabled {
        return (McpRuntimeStatus::Disabled, None);
    }
    if let Some(reason) = toolset_error {
        return (McpRuntimeStatus::Unavailable, Some(reason));
    }
    if listener_ready {
        (McpRuntimeStatus::Ready, None)
    } else {
        (
            McpRuntimeStatus::Unavailable,
            Some("MCP listener did not become ready".to_string()),
        )
    }
}

fn mcp_unavailable_warning(error: &AppError) -> SidecarWarning {
    SidecarWarning {
        kind: SidecarWarningKind::McpUnavailable,
        message: format!("MCP listener did not become ready: {error}"),
        detail: None,
    }
}

fn selected_restart_mcp_listener_readiness(
    token_rotation: bool,
    listener_result: AppResult<()>,
    warnings: &mut Vec<SidecarWarning>,
) -> AppResult<bool> {
    match listener_result {
        Ok(()) => Ok(true),
        // A rotated credential must never be committed when its replacement
        // listener is unavailable: the rollback path restores the old token
        // before it brings the selected sidecars back.
        Err(error) if token_rotation => Err(error),
        Err(error) => {
            warn!(error = %error, "MCP listener did not become ready after partial restart (continuing without MCP)");
            warnings.push(mcp_unavailable_warning(&error));
            Ok(false)
        }
    }
}

const MCP_EXPECTED_TOOL_NAMES: [&str; 10] = [
    "lookback_recall",
    "lookback_get_context",
    "lookback_get_reflections",
    "lookback_issue_list_ready",
    "lookback_issue_transition",
    "lookback_issue_claim",
    "lookback_issue_heartbeat",
    "lookback_issue_release",
    "lookback_issue_agent_plan",
    "lookback_issue_agent_get",
];

#[cfg(test)]
fn validate_mcp_toolset_json(raw: &str) -> AppResult<()> {
    validate_mcp_toolset_json_for_issue_tracking(raw, true)
}

fn validate_mcp_toolset_json_for_issue_tracking(
    raw: &str,
    issue_tracking_enabled: bool,
) -> AppResult<()> {
    let tools: Vec<serde_json::Value> = serde_json::from_str(raw).map_err(|error| {
        AppError::WorkerRegistration(format!("parse MCP FunctionSet tools: {error}"))
    })?;
    let names: std::collections::HashSet<&str> = tools
        .iter()
        .filter_map(|tool| {
            tool.pointer("/function/name")
                .and_then(serde_json::Value::as_str)
        })
        .collect();
    let expected = if issue_tracking_enabled {
        &MCP_EXPECTED_TOOL_NAMES[..]
    } else {
        &MCP_EXPECTED_TOOL_NAMES[..3]
    };
    let missing: Vec<_> = expected
        .iter()
        .copied()
        .filter(|name| !names.contains(name))
        .collect();
    if missing.is_empty() && names.len() == expected.len() && tools.len() == expected.len() {
        Ok(())
    } else {
        Err(AppError::WorkerRegistration(format!(
            "MCP FunctionSet does not match the configured Issue policy: missing [{}], unexpected [{}]",
            missing.join(", "),
            names
                .iter()
                .filter(|name| !expected.contains(name))
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

async fn validate_mcp_toolset_for_issue_tracking(
    handle: &crate::jobworkerp::JobworkerpHandle,
    issue_tracking_enabled: bool,
) -> AppResult<()> {
    let tools = handle
        .fetch_function_set_as_tools_json(crate::commands::mcp_settings::MCP_FUNCTION_SET_NAME)
        .await?;
    validate_mcp_toolset_json_for_issue_tracking(&tools, issue_tracking_enabled)
}

fn mcp_toolset_unavailable_warning(reason: &str) -> SidecarWarning {
    SidecarWarning {
        kind: SidecarWarningKind::McpToolsetUnavailable,
        message: format!("MCP toolset is unavailable: {reason}"),
        detail: None,
    }
}

/// Resolved runtime config for a single launch. Populated after sidecars
/// finished port selection.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SidecarEndpoints {
    /// `None` while jobworkerp is a remote server.
    pub jobworkerp_port: Option<u16>,
    /// Issue Tracker gRPC bind port when its health check currently passes.
    /// `None` means the process is unavailable or still starting.
    pub issue_tracker_port: Option<u16>,
    pub memories_port: Option<u16>,
    /// `None` while conductor is a remote server.
    pub conductor_port: Option<u16>,
    /// MCP HTTP server bind port when enabled, else `None`. jobworkerp boots
    /// the MCP server inside the same `all-in-one`
    /// process as the gRPC front, so this is not a separate child — it is
    /// the port `MCP_ADDR` bound to. Surfaced so the UI can print the
    /// connection URL for an external MCP client.
    #[serde(default)]
    pub mcp_server_port: Option<u16>,
}

/// A resident sidecar that can be independently selected for a configuration
/// restart. The declaration order is also the startup dependency order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarKind {
    Jobworkerp,
    Memories,
    Conductor,
    IssueTracker,
}

/// The sidecars whose spawn-time configuration must be refreshed.
///
/// A plan is deliberately a set, not a sequence: callers can union the
/// effects of a single Settings save and restart each selected child once.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct RestartPlan {
    jobworkerp: bool,
    memories: bool,
    conductor: bool,
    issue_tracker: bool,
}

impl RestartPlan {
    pub const fn empty() -> Self {
        Self {
            jobworkerp: false,
            memories: false,
            conductor: false,
            issue_tracker: false,
        }
    }

    pub const fn jobworkerp_only() -> Self {
        Self {
            jobworkerp: true,
            ..Self::empty()
        }
    }

    pub const fn embedding() -> Self {
        Self {
            jobworkerp: true,
            memories: true,
            ..Self::empty()
        }
    }

    pub const fn timezone() -> Self {
        Self {
            jobworkerp: true,
            conductor: true,
            ..Self::empty()
        }
    }

    /// Connection changes retain the existing conservative semantics.
    pub const fn connection() -> Self {
        Self {
            jobworkerp: true,
            memories: true,
            conductor: true,
            ..Self::empty()
        }
    }

    /// Reserved for a future Issue Tracker-specific settings card.
    pub const fn issue_tracker() -> Self {
        Self {
            issue_tracker: true,
            ..Self::empty()
        }
    }

    pub const fn includes(&self, kind: SidecarKind) -> bool {
        match kind {
            SidecarKind::Jobworkerp => self.jobworkerp,
            SidecarKind::Memories => self.memories,
            SidecarKind::Conductor => self.conductor,
            SidecarKind::IssueTracker => self.issue_tracker,
        }
    }

    pub const fn is_empty(&self) -> bool {
        !self.jobworkerp && !self.memories && !self.conductor && !self.issue_tracker
    }

    pub const fn union(&self, other: &Self) -> Self {
        Self {
            jobworkerp: self.jobworkerp || other.jobworkerp,
            memories: self.memories || other.memories,
            conductor: self.conductor || other.conductor,
            issue_tracker: self.issue_tracker || other.issue_tracker,
        }
    }
}

impl SidecarEndpoints {
    pub fn jobworkerp_url(&self) -> Option<String> {
        self.jobworkerp_port.map(loopback_url)
    }

    pub fn memories_url(&self) -> Option<String> {
        self.memories_port.map(loopback_url)
    }

    pub fn conductor_url(&self) -> Option<String> {
        self.conductor_port.map(loopback_url)
    }

    pub fn require_jobworkerp_port(&self) -> AppResult<u16> {
        self.jobworkerp_port
            .ok_or_else(|| AppError::SidecarNotReady("local jobworkerp not started".into()))
    }

    pub fn require_jobworkerp_url(&self) -> AppResult<String> {
        self.require_jobworkerp_port().map(loopback_url)
    }
}

pub(crate) fn loopback_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Which resident services this app runs itself; the rest are remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalTopology {
    pub jobworkerp: bool,
    pub memories: bool,
    pub conductor: bool,
}

impl LocalTopology {
    pub const ALL_LOCAL: Self = Self {
        jobworkerp: true,
        memories: true,
        conductor: true,
    };

    pub fn of(connection: &crate::commands::connection::ConnectionConfig) -> Self {
        Self {
            jobworkerp: connection.jobworkerp.is_local(),
            memories: connection.memories.is_local(),
            conductor: connection.conductor.is_local(),
        }
    }
}

/// Discriminator on `SidecarWarning`. Kept as a string-enum so the
/// frontend can branch on `kind` directly. Add new variants here when
/// surfacing a different non-fatal startup condition.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarWarningKind {
    /// The standalone Issue Tracker server could not start or become ready.
    IssueTrackerUnavailable,
    /// conductor stored the periodic schedulers without their job timeout,
    /// so a run is cut off at conductor's default until it is updated.
    ConductorOutdated,
    /// The remote jobworkerp lacks Lookback's workers, channels or plugins,
    /// or its workers came from another app version.
    RemoteJobworkerpUnprepared,
    /// The remote jobworkerp could not be reached at startup. The app still
    /// opens so the connection can be changed in Settings.
    RemoteJobworkerpUnavailable,
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
    /// A configuration written before MCP bearer authentication existed was
    /// disabled because it cannot safely expose an unauthenticated listener.
    McpAuthenticationMigration,
    /// MCP is optional; its listener did not become ready, but the other
    /// sidecars remain available.
    McpUnavailable,
    /// MCP listener is reachable but its configured FunctionSet cannot be
    /// resolved to the expected stable tool surface.
    McpToolsetUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpRuntimeStatus {
    Disabled,
    Ready,
    Unavailable,
}

#[derive(Debug, Clone)]
struct McpRuntimeState {
    status: McpRuntimeStatus,
    unavailable_reason: Option<String>,
}

impl Default for McpRuntimeState {
    fn default() -> Self {
        Self {
            status: McpRuntimeStatus::Disabled,
            unavailable_reason: None,
        }
    }
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

pub(crate) enum EmbeddingMaintenanceDecision<T> {
    Start(T),
    KeepStopped(T),
}

pub(crate) enum EmbeddingMaintenanceOutcome<T> {
    Started {
        value: T,
        report: SidecarStartReport,
    },
    Stopped {
        value: T,
    },
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
    match failure.as_ref() {
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

/// Retry only vector-dimension startup failures with the vector store
/// disabled. Both full startup and selected memories restarts use this policy
/// so a Settings save cannot turn a recoverable mismatch into a hard failure.
async fn retry_degraded_memories_start<F, Fut>(
    initial_start: AppResult<()>,
    retry_with_vectors_disabled: F,
) -> AppResult<Option<DegradedInfo>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = AppResult<()>>,
{
    let Err(error) = initial_start else {
        return Ok(None);
    };
    let Some(info) = plan_degraded_retry(&error) else {
        return Err(error);
    };
    retry_with_vectors_disabled().await?;
    Ok(Some(info))
}

fn conductor_drops_job_timeout_warning(schedulers: &[String]) -> SidecarWarning {
    SidecarWarning {
        kind: SidecarWarningKind::ConductorOutdated,
        message: format!(
            "conductor が定期実行のタイムアウトを保存しませんでした（{}）。conductor を更新しないと、実行が既定のタイムアウトで打ち切られます",
            schedulers.join(", ")
        ),
        detail: None,
    }
}

fn periodic_runtime_refresh_warning(error: &AppError) -> SidecarWarning {
    SidecarWarning {
        kind: SidecarWarningKind::WorkerApplyFailed,
        message: format!("定期実行の runtime 更新に失敗しました: {error}"),
        detail: None,
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

    /// Standalone private Issue Tracker gRPC server binary.
    pub issue_tracker_server_bin: Option<PathBuf>,

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

/// The coding agent works on this machine's repositories, so its channels
/// never belong on a remote jobworkerp.
const LOCAL_ONLY_CHANNELS: [&str; 2] = [
    crate::jobworkerp::coding_agent::NATIVE_INTERACTIVE_CHANNEL,
    crate::jobworkerp::coding_agent::CODING_AGENT_ONE_SHOT_CHANNEL,
];

/// The channels Lookback's workers run on, with their concurrency.
fn jobworkerp_channels() -> [(&'static str, u32); 18] {
    use crate::jobworkerp::coding_agent::{
        CODING_AGENT_ONE_SHOT_CHANNEL, NATIVE_INTERACTIVE_CHANNEL,
    };
    [
        ("llm", 1),
        ("llm_external", 2),
        ("llm_workflow", 1),
        ("llm_batch", 2),
        ("llm_periodic", 1),
        ("thread_group_summary", 1),
        ("embedding", 1),
        ("embedding_workflow", 1),
        ("rag", 2),
        ("issue_tracker", 1),
        ("issue_extract", 1),
        ("issue_resolution_manual", 1),
        ("issue_resolution_group", 1),
        // Both resolution callers (group, cross-thread) may wait on a child at once.
        ("issue_resolution_shared", 2),
        ("issue_resolution_cross", 1),
        ("issue_resolution_cross_candidate", 1),
        (NATIVE_INTERACTIVE_CHANNEL, 4),
        (CODING_AGENT_ONE_SHOT_CHANNEL, 4),
    ]
}

fn jobworkerp_channel_env_vars() -> [(&'static str, String); 3] {
    channel_env_vars(&jobworkerp_channels())
}

/// Keep jobworkerp's envy-based worker config complete. Supplying channels
/// without the required default concurrency makes the whole config fall back
/// to the default channel only, so named worker registration then fails.
fn channel_env_vars(channels: &[(&str, u32)]) -> [(&'static str, String); 3] {
    [
        ("WORKER_DEFAULT_CONCURRENCY", "4".into()),
        (
            "WORKER_CHANNELS",
            channels
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(","),
        ),
        (
            "WORKER_CHANNEL_CONCURRENCIES",
            channels
                .iter()
                .map(|(_, concurrency)| concurrency.to_string())
                .collect::<Vec<_>>()
                .join(","),
        ),
    ]
}

fn experimental_issues_env_vars(enabled: bool) -> [(&'static str, &'static str); 1] {
    [(
        "LOOKBACK_EXPERIMENTAL_ISSUES_ENABLED",
        if enabled { "true" } else { "false" },
    )]
}

#[cfg(target_os = "linux")]
const MSB_CHILD_ENV_KEYS: [&str; 5] = [
    "MSB_HOME",
    "MSB_CONFIG_PATH",
    "MSB_PATH",
    "MSB_LIBKRUNFW_PATH",
    "MSB_AGENTD_PATH",
];

#[cfg(target_os = "linux")]
const MSB_CANONICAL_SOCKET_HASH_PLACEHOLDER: &str = "000000000000000000000000";

#[cfg(target_os = "linux")]
struct JobworkerpMsbSdkPaths {
    home: PathBuf,
    config_path: PathBuf,
    representative_socket_path_supported: bool,
}

fn managed_child_executable_path(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .filter(|path| path.is_absolute())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Select the default Microsandbox home and config file below the native user's private app
/// directory. Do not create or rewrite the file: SDK 0.7.6 treats a missing config as empty, and
/// an existing explicit `config.home` remains authoritative over `MSB_HOME`.
#[cfg(target_os = "linux")]
fn jobworkerp_msb_sdk_paths(native_home: &Path) -> JobworkerpMsbSdkPaths {
    let home = native_home.join(".local/share/lookback/msb");
    let config_path = home.join("config.json");
    // The SDK checks Unix socket lengths when an agent sandbox is actually spawned. Use a
    // canonical endpoint with the SDK's fixed 24-byte hash for an advisory diagnostic, never a
    // startup gate.
    let socket_path = home
        .join("run")
        .join("sandboxes")
        .join(MSB_CANONICAL_SOCKET_HASH_PLACEHOLDER)
        .join("agent.sock");
    let representative_socket_path_supported = unix_socket_path_supported(&socket_path)
        && unix_socket_path_supported(&socket_path.with_file_name("control.sock"));

    JobworkerpMsbSdkPaths {
        home,
        config_path,
        representative_socket_path_supported,
    }
}

#[cfg(target_os = "linux")]
fn unix_socket_path_supported(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    // Linux sockaddr_un contains only integer fields and a byte array, so a zeroed value is valid.
    let sun_path_capacity = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
        .sun_path
        .len();
    path.as_os_str().as_bytes().len() < sun_path_capacity
}

/// jobworkerp leaves tasks unbounded unless a default is configured; it then
/// applies only to run/call tasks. One hour detects a hung LLM or child call
/// (measured calls finish within minutes), while longer child batches carry
/// their own explicit timeout in the workflow YAML.
const JOBWORKERP_TASK_DEFAULT_TIMEOUT_SEC: &str = "3600";

fn jobworkerp_workflow_env_vars() -> [(&'static str, &'static str); 1] {
    [(
        "WORKFLOW_TASK_DEFAULT_TIMEOUT_SEC",
        JOBWORKERP_TASK_DEFAULT_TIMEOUT_SEC,
    )]
}

fn apply_jobworkerp_workflow_environment(cmd: &mut Command) {
    cmd.envs(jobworkerp_workflow_env_vars());
}

/// Clear process / .env runtime selectors before pinning the SDK config location.
/// Do not set binary or firmware paths until the app has staged and validated that runtime pair.
#[cfg(target_os = "linux")]
fn apply_jobworkerp_msb_sdk_environment(cmd: &mut Command, paths: Option<&JobworkerpMsbSdkPaths>) {
    for key in MSB_CHILD_ENV_KEYS {
        cmd.env_remove(key);
    }
    if let Some(paths) = paths {
        cmd.env("MSB_HOME", &paths.home)
            .env("MSB_CONFIG_PATH", &paths.config_path);
    }
}

/// Origin of the `HOME` value observed on jobworkerp's spawn command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedChildHomeSource {
    /// Set explicitly by the command builder or its env-file layer.
    ExplicitlySet,
    /// Not overridden by the command; the child inherits the host's HOME.
    InheritedAtSpawn,
    /// Explicitly removed from the child environment.
    Removed,
}

/// What this lifecycle observer knows about the sandbox runtime itself.
/// Selecting SDK environment variables does not verify a library, CLI, VM,
/// firmware, or sandbox operation, so the current lifecycle reports no such
/// observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedJobworkerpRuntimeObservation {
    NotObserved,
}

/// A bounded, opaque fingerprint of the one SDK config file selected for the
/// managed jobworkerp child. It records a source only; it does not prove the
/// SDK loaded it or that this is the SDK's complete source closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedJobworkerpSdkSourceStamp {
    /// The selected file was securely opened and fingerprinted before spawn.
    Present(ManagedJobworkerpSdkSourceFileStamp),
    /// The selected file or an ancestor directory did not exist. The SDK's
    /// empty-config fallback remains a valid source state; startup is not gated.
    Missing { selected_path: PathBuf },
    /// The source could not be safely or reliably observed.
    Unavailable {
        reason: ManagedJobworkerpSdkSourceUnavailableReason,
    },
}

/// Short native-facing name for [`ManagedJobworkerpSdkSourceStamp`].
pub type ManagedSdkConfigSourceStamp = ManagedJobworkerpSdkSourceStamp;

/// Non-secret file identity and content fingerprint for the selected SDK
/// config. The file bytes are never retained or formatted into diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedJobworkerpSdkSourceFileStamp {
    pub selected_path: PathBuf,
    pub sha256: [u8; 32],
    pub length: u64,
    pub device: u64,
    pub inode: u64,
    pub parent_device: u64,
    pub parent_inode: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
    pub changed_seconds: i64,
    pub changed_nanoseconds: i64,
    pub mode: u32,
    pub owner: u32,
}

/// Fixed classifications for source-stamp failures; raw OS errors and file
/// contents are deliberately excluded from snapshots and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedJobworkerpSdkSourceUnavailableReason {
    SelectedConfigPathNotApplied,
    UnsupportedPlatform,
    NonAbsolutePath,
    InvalidPath,
    Symlink,
    UnsafePathComponent,
    UntrustedDirectoryOwner,
    UnsafeDirectoryPermissions,
    NotRegularFile,
    UntrustedFileOwner,
    UnsafeFilePermissions,
    FileTooLarge,
    AccessDenied,
    ReadFailed,
    ChangedDuringRead,
    ManagedChildUnavailable,
    ManagedChildReplaced,
}

/// Result of checking the currently selected config source against the
/// source stamped before the managed child was spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedJobworkerpSdkSourceCheck {
    Matches,
    Changed,
    Unavailable(ManagedJobworkerpSdkSourceUnavailableReason),
}

/// Spawn-time facts for the live jobworkerp child managed by [`Sidecars`].
/// These are observations of the command and child, not proof that the SDK or
/// any Microsandbox assets are usable. `generation_id` identifies this
/// jobworkerp replacement, not a physical sandbox/VM generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedJobworkerpSpawnSnapshot {
    /// Process-local identity assigned for each jobworkerp spawn.
    pub generation_id: u64,
    /// PID returned by the managed child handle.
    pub child_pid: u32,
    /// Selected binary path resolved against the child's working directory
    /// and canonicalized when available.
    pub configured_binary_path: PathBuf,
    /// Live executable path reported by the OS at spawn time, when available.
    /// This is path identity only, not a content/image verification.
    pub actual_running_executable: Option<PathBuf>,
    /// `MSB_HOME` explicitly applied to the jobworkerp command, if any.
    pub applied_sdk_home: Option<PathBuf>,
    /// `MSB_CONFIG_PATH` explicitly applied to the jobworkerp command, if any.
    pub applied_sdk_config_path: Option<PathBuf>,
    /// `HOME` effective on the child command, including inherited HOME when
    /// not overridden. This path can differ from the SDK's selected home.
    pub child_home: Option<PathBuf>,
    /// Whether the child HOME came from an explicit command override, host
    /// inheritance, or removal.
    pub child_home_source: ManagedChildHomeSource,
    /// `None` until a future native doctor reads and interprets the selected
    /// SDK config. This lifecycle does not infer it from `MSB_HOME`.
    pub effective_sdk_home: Option<PathBuf>,
    /// SDK/runtime/assets are deliberately not validated by sidecar startup.
    pub runtime_observation: ManagedJobworkerpRuntimeObservation,
    /// Selected config source observed before this child was spawned. This is
    /// intentionally not serialized or exposed as SDK/runtime readiness.
    pub(crate) sdk_config_source: ManagedJobworkerpSdkSourceStamp,
}

/// Read-only source stamps bound to one live managed jobworkerp generation.
/// Kept as a separate snapshot so existing spawn-fact constructors do not need
/// to know about the machine-level SDK override source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedJobworkerpSdkSourceSnapshot {
    pub generation_id: u64,
    pub child_pid: u32,
    pub selected_config_source: ManagedJobworkerpSdkSourceStamp,
    pub managed_sdk_config_source: ManagedJobworkerpSdkSourceStamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagedChildHome {
    path: Option<PathBuf>,
    source: ManagedChildHomeSource,
}

#[derive(Debug, Clone)]
struct JobworkerpSpawnObservation {
    snapshot: ManagedJobworkerpSpawnSnapshot,
    managed_sdk_config_source: ManagedJobworkerpSdkSourceStamp,
    startup_complete: bool,
}

/// Holds the existing sidecar lifecycle lock across native sandbox admission
/// validation and opening a job. Native callers must acquire their shared
/// settings/admission lock first, then this guard, then any short registry
/// lock. Keep this lease brief: it prevents sidecar replacement, not external
/// edits to the SDK config file.
pub struct ManagedJobworkerpAdmissionGuard<'a> {
    _lifecycle_guard: tokio::sync::MutexGuard<'a, ()>,
}

fn command_child_home(cmd: &Command) -> ManagedChildHome {
    let key = std::ffi::OsStr::new("HOME");
    match cmd
        .as_std()
        .get_envs()
        .find(|(candidate, _)| *candidate == key)
    {
        Some((_, Some(value))) => ManagedChildHome {
            path: Some(PathBuf::from(value)),
            source: ManagedChildHomeSource::ExplicitlySet,
        },
        Some((_, None)) => ManagedChildHome {
            path: None,
            source: ManagedChildHomeSource::Removed,
        },
        None => ManagedChildHome {
            path: std::env::var_os("HOME").map(PathBuf::from),
            source: ManagedChildHomeSource::InheritedAtSpawn,
        },
    }
}

fn command_path_env(cmd: &Command, name: &str) -> Option<PathBuf> {
    let key = std::ffi::OsStr::new(name);
    cmd.as_std()
        .get_envs()
        .find(|(candidate, _)| *candidate == key)
        .and_then(|(_, value)| value.map(PathBuf::from))
}

fn native_configured_binary_path(binary: &Path, child_cwd: &Path) -> PathBuf {
    let resolved = if binary.is_absolute() {
        binary.to_path_buf()
    } else {
        child_cwd.join(binary)
    };
    std::fs::canonicalize(&resolved).unwrap_or(resolved)
}

fn capture_jobworkerp_sdk_source(path: Option<&Path>) -> ManagedJobworkerpSdkSourceStamp {
    let Some(path) = path else {
        return ManagedJobworkerpSdkSourceStamp::Unavailable {
            reason: ManagedJobworkerpSdkSourceUnavailableReason::SelectedConfigPathNotApplied,
        };
    };

    #[cfg(target_os = "linux")]
    {
        capture_linux_jobworkerp_sdk_source(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        ManagedJobworkerpSdkSourceStamp::Unavailable {
            reason: ManagedJobworkerpSdkSourceUnavailableReason::UnsupportedPlatform,
        }
    }
}

fn compare_jobworkerp_sdk_source_stamps(
    expected: &ManagedJobworkerpSdkSourceStamp,
    current: &ManagedJobworkerpSdkSourceStamp,
) -> ManagedJobworkerpSdkSourceCheck {
    use ManagedJobworkerpSdkSourceCheck::{Changed, Matches, Unavailable};
    use ManagedJobworkerpSdkSourceStamp::{Missing, Present, Unavailable as UnavailableStamp};

    match (expected, current) {
        (Present(expected), Present(current)) if expected == current => Matches,
        (
            Missing {
                selected_path: expected,
            },
            Missing {
                selected_path: current,
            },
        ) if expected == current => Matches,
        (UnavailableStamp { reason }, _) | (_, UnavailableStamp { reason }) => Unavailable(*reason),
        (Present(_), Present(_) | Missing { .. })
        | (Missing { .. }, Present(_) | Missing { .. }) => Changed,
    }
}

fn source_stamp_path(stamp: &ManagedJobworkerpSdkSourceStamp) -> Option<&Path> {
    match stamp {
        ManagedJobworkerpSdkSourceStamp::Present(file) => Some(&file.selected_path),
        ManagedJobworkerpSdkSourceStamp::Missing { selected_path } => Some(selected_path),
        ManagedJobworkerpSdkSourceStamp::Unavailable { .. } => None,
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeSdkSourceMetadata {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    mode: u32,
    owner: u32,
}

#[cfg(target_os = "linux")]
impl NativeSdkSourceMetadata {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            mode: metadata.mode() & 0o7777,
            owner: metadata.uid(),
        }
    }
}

#[cfg(target_os = "linux")]
fn capture_linux_jobworkerp_sdk_source(path: &Path) -> ManagedJobworkerpSdkSourceStamp {
    capture_linux_jobworkerp_sdk_source_with_hook(path, || {})
}

#[cfg(target_os = "linux")]
fn capture_linux_jobworkerp_sdk_source_with_hook<F>(
    path: &Path,
    after_initial_read: F,
) -> ManagedJobworkerpSdkSourceStamp
where
    F: FnOnce(),
{
    use std::os::fd::{AsRawFd, FromRawFd};

    use ManagedJobworkerpSdkSourceStamp::{Missing, Present, Unavailable};
    use ManagedJobworkerpSdkSourceUnavailableReason as Reason;

    let native_uid = unsafe { libc::getuid() };
    let initial = match resolve_native_sdk_config_path(path, native_uid) {
        Ok(resolution) => resolution,
        Err(reason) => return Unavailable { reason },
    };
    match initial {
        NativeSdkPathResolution::Missing => {
            after_initial_read();
            match resolve_native_sdk_config_path(path, native_uid) {
                Ok(NativeSdkPathResolution::Missing) => Missing {
                    selected_path: path.to_path_buf(),
                },
                Ok(NativeSdkPathResolution::Present(_)) => Unavailable {
                    reason: Reason::ChangedDuringRead,
                },
                Err(reason) => Unavailable { reason },
            }
        }
        NativeSdkPathResolution::Present(initial) => {
            let config_fd = unsafe {
                libc::openat(
                    initial.parent.as_raw_fd(),
                    initial.filename.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                )
            };
            if config_fd < 0 {
                return Unavailable {
                    reason: Reason::ChangedDuringRead,
                };
            }
            // SAFETY: `config_fd` is a fresh read-only descriptor returned by
            // `openat` and is transferred to this File exactly once.
            let mut config_file = unsafe { std::fs::File::from_raw_fd(config_fd) };
            let before_read = match config_file.metadata() {
                Ok(metadata) => NativeSdkSourceMetadata::from_metadata(&metadata),
                Err(_) => {
                    return Unavailable {
                        reason: Reason::ReadFailed,
                    };
                }
            };
            if before_read != initial.file_metadata {
                return Unavailable {
                    reason: Reason::ChangedDuringRead,
                };
            }

            let mut hasher = Sha256::new();
            let mut total = 0_u64;
            let mut buffer = [0_u8; 16 * 1024];
            loop {
                let read = match config_file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        return Unavailable {
                            reason: Reason::ReadFailed,
                        };
                    }
                };
                total += read as u64;
                if total > MAX_MANAGED_SDK_CONFIG_BYTES as u64 {
                    return Unavailable {
                        reason: Reason::FileTooLarge,
                    };
                }
                hasher.update(&buffer[..read]);
            }

            let after_read = match config_file.metadata() {
                Ok(metadata) => NativeSdkSourceMetadata::from_metadata(&metadata),
                Err(_) => {
                    return Unavailable {
                        reason: Reason::ReadFailed,
                    };
                }
            };
            if after_read != before_read || total != before_read.length {
                return Unavailable {
                    reason: Reason::ChangedDuringRead,
                };
            }

            let sha256: [u8; 32] = hasher.finalize().into();
            after_initial_read();
            match resolve_native_sdk_config_path(path, native_uid) {
                Ok(NativeSdkPathResolution::Present(current))
                    if current.parent_identity == initial.parent_identity
                        && current.file_metadata == after_read =>
                {
                    Present(ManagedJobworkerpSdkSourceFileStamp {
                        selected_path: path.to_path_buf(),
                        sha256,
                        length: after_read.length,
                        device: after_read.device,
                        inode: after_read.inode,
                        parent_device: initial.parent_identity.device,
                        parent_inode: initial.parent_identity.inode,
                        modified_seconds: after_read.modified_seconds,
                        modified_nanoseconds: after_read.modified_nanoseconds,
                        changed_seconds: after_read.changed_seconds,
                        changed_nanoseconds: after_read.changed_nanoseconds,
                        mode: after_read.mode,
                        owner: after_read.owner,
                    })
                }
                Ok(NativeSdkPathResolution::Present(_) | NativeSdkPathResolution::Missing) => {
                    Unavailable {
                        reason: Reason::ChangedDuringRead,
                    }
                }
                Err(reason) => Unavailable { reason },
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeSdkDirectoryIdentity {
    device: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
impl NativeSdkDirectoryIdentity {
    fn from_file(
        directory: &std::fs::File,
    ) -> Result<Self, ManagedJobworkerpSdkSourceUnavailableReason> {
        use std::os::unix::fs::MetadataExt;

        let metadata = directory
            .metadata()
            .map_err(|_| ManagedJobworkerpSdkSourceUnavailableReason::ReadFailed)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[cfg(target_os = "linux")]
struct NativeSdkResolvedFile {
    parent: std::fs::File,
    filename: std::ffi::CString,
    // Keep the O_PATH descriptor alive through the content read so the
    // descriptor's identity can be compared with both read-FD fstats.
    _path_file: std::fs::File,
    parent_identity: NativeSdkDirectoryIdentity,
    file_metadata: NativeSdkSourceMetadata,
}

#[cfg(target_os = "linux")]
enum NativeSdkPathResolution {
    Missing,
    Present(NativeSdkResolvedFile),
}

#[cfg(target_os = "linux")]
fn resolve_native_sdk_config_path(
    path: &Path,
    native_uid: u32,
) -> Result<NativeSdkPathResolution, ManagedJobworkerpSdkSourceUnavailableReason> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};

    use ManagedJobworkerpSdkSourceUnavailableReason as Reason;

    let components = native_absolute_path_components(path)?;
    let root_path = CString::new("/").expect("root path has no NUL");
    // SAFETY: `root_path` is NUL-terminated and the flags request a read-only
    // directory descriptor with no symlink traversal.
    let root_fd = unsafe {
        libc::open(
            root_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if root_fd < 0 {
        return Err(native_sdk_path_error(&std::io::Error::last_os_error()));
    }
    // SAFETY: `root_fd` is a fresh descriptor returned by `open` and ownership
    // is transferred to this File exactly once.
    let mut directory = unsafe { std::fs::File::from_raw_fd(root_fd) };
    validate_native_sdk_directory(&directory, native_uid)?;

    for component in &components[..components.len() - 1] {
        let next_fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if next_fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(NativeSdkPathResolution::Missing);
            }
            return Err(native_sdk_path_error(&error));
        }
        // SAFETY: `next_fd` is a fresh descriptor returned by `openat` and is
        // transferred to this File exactly once.
        let next = unsafe { std::fs::File::from_raw_fd(next_fd) };
        validate_native_sdk_directory(&next, native_uid)?;
        directory = next;
    }

    let parent_identity = NativeSdkDirectoryIdentity::from_file(&directory)?;
    let filename = components
        .last()
        .expect("absolute config path has a file name")
        .clone();
    let path_fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            filename.as_ptr(),
            libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if path_fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(NativeSdkPathResolution::Missing);
        }
        return Err(native_sdk_path_error(&error));
    }
    // SAFETY: `path_fd` is a fresh O_PATH descriptor returned by `openat` and
    // is transferred to this File exactly once.
    let path_file = unsafe { std::fs::File::from_raw_fd(path_fd) };
    let path_metadata = path_file.metadata().map_err(|_| Reason::ReadFailed)?;
    if path_metadata.file_type().is_symlink() {
        return Err(Reason::Symlink);
    }
    if !path_metadata.is_file() {
        return Err(Reason::NotRegularFile);
    }
    validate_native_sdk_file(&path_metadata, native_uid)?;
    if path_metadata.len() > MAX_MANAGED_SDK_CONFIG_BYTES as u64 {
        return Err(Reason::FileTooLarge);
    }
    Ok(NativeSdkPathResolution::Present(NativeSdkResolvedFile {
        parent: directory,
        filename,
        _path_file: path_file,
        parent_identity,
        file_metadata: NativeSdkSourceMetadata::from_metadata(&path_metadata),
    }))
}

#[cfg(target_os = "linux")]
fn native_absolute_path_components(
    path: &Path,
) -> Result<Vec<std::ffi::CString>, ManagedJobworkerpSdkSourceUnavailableReason> {
    use std::os::unix::ffi::OsStrExt;

    use ManagedJobworkerpSdkSourceUnavailableReason as Reason;

    if !path.is_absolute() {
        return Err(Reason::NonAbsolutePath);
    }
    let bytes = path.as_os_str().as_bytes();
    if bytes.first() != Some(&b'/') || bytes.contains(&0) {
        return Err(Reason::InvalidPath);
    }
    let mut components = Vec::new();
    for component in bytes[1..].split(|byte| *byte == b'/') {
        if component.is_empty() || component == b"." || component == b".." {
            return Err(Reason::InvalidPath);
        }
        components.push(std::ffi::CString::new(component).map_err(|_| Reason::InvalidPath)?);
    }
    if components.is_empty() {
        return Err(Reason::InvalidPath);
    }
    Ok(components)
}

#[cfg(target_os = "linux")]
fn native_sdk_path_error(error: &std::io::Error) -> ManagedJobworkerpSdkSourceUnavailableReason {
    use ManagedJobworkerpSdkSourceUnavailableReason as Reason;

    match error.raw_os_error() {
        Some(libc::ELOOP) => Reason::Symlink,
        Some(libc::ENOTDIR) => Reason::UnsafePathComponent,
        Some(libc::EACCES) | Some(libc::EPERM) => Reason::AccessDenied,
        _ => Reason::ReadFailed,
    }
}

#[cfg(target_os = "linux")]
fn validate_native_sdk_directory(
    directory: &std::fs::File,
    native_uid: u32,
) -> Result<(), ManagedJobworkerpSdkSourceUnavailableReason> {
    use std::os::unix::fs::MetadataExt;

    use ManagedJobworkerpSdkSourceUnavailableReason as Reason;

    let metadata = directory.metadata().map_err(|_| Reason::ReadFailed)?;
    if !metadata.is_dir() {
        return Err(Reason::UnsafePathComponent);
    }
    if metadata.uid() != native_uid && metadata.uid() != 0 {
        return Err(Reason::UntrustedDirectoryOwner);
    }
    let mode = metadata.mode() & 0o7777;
    let root_sticky_directory = metadata.uid() == 0 && mode & 0o1000 != 0 && mode & !0o1777 == 0;
    if mode & 0o022 != 0 && !root_sticky_directory {
        return Err(Reason::UnsafeDirectoryPermissions);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_native_sdk_file(
    file: &std::fs::Metadata,
    native_uid: u32,
) -> Result<(), ManagedJobworkerpSdkSourceUnavailableReason> {
    use std::os::unix::fs::MetadataExt;

    use ManagedJobworkerpSdkSourceUnavailableReason as Reason;

    if file.uid() != native_uid && file.uid() != 0 {
        return Err(Reason::UntrustedFileOwner);
    }
    if file.mode() & 0o7777 & !0o644 != 0 {
        return Err(Reason::UnsafeFilePermissions);
    }
    Ok(())
}

/// Resolve the real account home via NSS instead of trusting a GUI-inherited HOME variable.
#[cfg(target_os = "linux")]
fn native_linux_user_home() -> std::io::Result<PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStringExt;

    const MAX_PASSWD_BUFFER: usize = 1024 * 1024;
    const FALLBACK_PASSWD_BUFFER: usize = 16 * 1024;

    // SAFETY: sysconf has no pointer arguments or side effects.
    let suggested_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut buffer_size = if suggested_size > 0 {
        (suggested_size as usize).min(MAX_PASSWD_BUFFER)
    } else {
        FALLBACK_PASSWD_BUFFER
    }
    .max(1024);

    loop {
        let mut buffer = vec![0_u8; buffer_size];
        let mut passwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: the output struct and scratch buffer are writable for the supplied sizes, and
        // `result` points to a valid output pointer. The function writes the struct only on success.
        let error = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if error == libc::ERANGE && buffer_size < MAX_PASSWD_BUFFER {
            buffer_size = buffer_size.saturating_mul(2).min(MAX_PASSWD_BUFFER);
            continue;
        }
        if error != 0 {
            return Err(std::io::Error::from_raw_os_error(error));
        }
        if result.is_null() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "NSS returned no passwd entry for the current uid",
            ));
        }

        // SAFETY: getpwuid_r returned success and a non-null result pointing into `passwd`.
        let passwd = unsafe { passwd.assume_init() };
        if passwd.pw_dir.is_null() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "NSS returned a null home directory",
            ));
        }
        // SAFETY: successful getpwuid_r initialized pw_dir as a NUL-terminated string in `buffer`.
        let home = unsafe { CStr::from_ptr(passwd.pw_dir) };
        let home = PathBuf::from(std::ffi::OsString::from_vec(home.to_bytes().to_vec()));
        if !home.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "NSS returned a non-absolute home directory",
            ));
        }
        return Ok(home);
    }
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
    /// Managed embedding env inherited when this Sidecars instance was
    /// created. `apply_embedding_env` mirrors the selected runtime into the
    /// host process for child startup; those mirror values must not later be
    /// mistaken for the user's external overrides when resolving settings.
    external_embedding_env: BTreeMap<OsString, OsString>,
    embedding_relaunch_prepared: Mutex<bool>,
    /// Serializes lifecycle operations on this instance. The database
    /// migration, child spawning, and lock release all inspect and mutate shared state,
    /// so checking `current_endpoints` alone cannot make concurrent starts or
    /// stops safe.
    lifecycle_lock: Arc<tokio::sync::Mutex<()>>,
    /// Spawn-time jobworkerp facts plus the lifecycle startup boundary for
    /// that exact child. The public snapshot accessor additionally checks the
    /// corresponding managed Child handle is still live.
    jobworkerp_spawn_observation: Mutex<Option<JobworkerpSpawnObservation>>,
    /// Exposed through the startup snapshot while the memories migration tool
    /// is changing the database. Kept independent of lifecycle_lock
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
    /// MCP HTTP server's bound port when enabled at the last `start_inner`,
    /// else `None`. jobworkerp boots the MCP server in
    /// the same process as the gRPC front (it is NOT a separate child in
    /// `state`), so the live port is not derivable from the process list —
    /// it is stashed here so `get_mcp_settings` / `get_sidecar_status` can
    /// hand the UI the connection URL. Cleared on stop.
    active_mcp_port: Arc<Mutex<Option<u16>>>,
    /// The services the last `start_inner` ran locally. A remote service has
    /// no child in `state`, so the process list alone cannot tell a remote
    /// jobworkerp from a crashed one. Cleared on stop.
    started_topology: Arc<Mutex<Option<LocalTopology>>>,
    /// Readiness reported to Settings. A TCP listener alone is insufficient:
    /// the fixed FunctionSet must also resolve to the expected tool surface.
    mcp_runtime_state: Arc<Mutex<McpRuntimeState>>,
    issue_tracker_port: Arc<Mutex<Option<u16>>>,
    /// The remote Issue tracker's URL while the tracker is remote; the local
    /// one is known by `issue_tracker_port` instead.
    issue_tracker_remote_url: Arc<Mutex<Option<String>>>,
    issue_tracker_gateway_control_url: Arc<Mutex<Option<String>>>,
    issue_tracker_gateway_worker_url: Arc<Mutex<Option<String>>>,
    issue_tracker_gateway_registration_lock: Arc<tokio::sync::Mutex<()>>,
    issue_tracker_maintenance_stop: Arc<AtomicBool>,
    issue_tracker_maintenance_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Prevents changing the keyring token while a launch has captured it or
    /// a child can still accept the old credential.
    mcp_token_state: Arc<AtomicU8>,
    /// Spawn-scoped MCP credentials. Kept until all children stop because
    /// jobworkerp reads the 0600 files lazily while serving requests.
    active_mcp_secrets: Arc<Mutex<Option<McpLaunchSecrets>>>,
    /// `Some` when the last `start_inner` had to restart the memories child
    /// with the vector store disabled because the local LanceDB could not
    /// be opened at the configured embedding dimension (degraded mode).
    /// Read by the command-layer gate (`ensure_local_embedding_available`)
    /// to refuse embedding-dependent dispatches, and surfaced to the UI via
    /// the `VectorStoreDegraded` warning. Reset to `None` at the top of
    /// every `start_inner` (so a subsequent clean start clears it) and on
    /// `stop` / `stop_blocking`.
    degraded: Arc<Mutex<Option<DegradedInfo>>>,
    /// Shared command-layer readiness boundary for the private Issue tracker
    /// worker. Bound by `AppState` after construction so every existing
    /// sidecar start/restart route updates the same state.
    issue_tracker_availability: Arc<Mutex<Option<Arc<crate::commands::IssueTrackerAvailability>>>>,
    /// Test-only failure injection after the first child is published. This
    /// keeps transaction recovery tests deterministic without changing the
    /// production startup path.
    #[cfg(test)]
    start_failure_injection: Arc<AtomicBool>,
    /// Test-only failure injection for proving that stop errors keep
    /// ownership of children and the data-root lock.
    #[cfg(test)]
    stop_failure_injection: Arc<AtomicBool>,
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

mod issue_tool_registration;
mod remote_jobworkerp;
pub use remote_jobworkerp::RemoteJobworkerpReadiness;
#[path = "issue_tracker.rs"]
mod issue_tracker;
pub(crate) use issue_tracker::published_issue_tracker_for_jobworkerp;

impl Sidecars {
    pub(crate) fn owns_instance_lock(&self) -> bool {
        self.instance_lock.lock().is_some()
    }
    pub fn new(config: SidecarConfig) -> Self {
        let effective_hf_home = resolve_effective_hf_home(&config);
        let external_embedding_env = crate::commands::embedding_settings::EMBEDDING_ENV_KEYS
            .iter()
            .filter_map(|key| std::env::var_os(key).map(|value| (OsString::from(key), value)))
            .collect();
        Self {
            config,
            external_embedding_env,
            embedding_relaunch_prepared: Mutex::new(false),
            lifecycle_lock: Arc::new(tokio::sync::Mutex::new(())),
            jobworkerp_spawn_observation: Mutex::new(None),
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
            started_topology: Arc::new(Mutex::new(None)),
            mcp_runtime_state: Arc::new(Mutex::new(McpRuntimeState::default())),
            issue_tracker_port: Arc::new(Mutex::new(None)),
            issue_tracker_remote_url: Arc::new(Mutex::new(None)),
            issue_tracker_gateway_control_url: Arc::new(Mutex::new(None)),
            issue_tracker_gateway_worker_url: Arc::new(Mutex::new(None)),
            issue_tracker_gateway_registration_lock: Arc::new(tokio::sync::Mutex::new(())),
            issue_tracker_maintenance_stop: Arc::new(AtomicBool::new(false)),
            issue_tracker_maintenance_task: Arc::new(Mutex::new(None)),
            mcp_token_state: Arc::new(AtomicU8::new(MCP_TOKEN_STATE_IDLE)),
            active_mcp_secrets: Arc::new(Mutex::new(None)),
            degraded: Arc::new(Mutex::new(None)),
            issue_tracker_availability: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            start_failure_injection: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            stop_failure_injection: Arc::new(AtomicBool::new(false)),
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

    /// Test-only: make the next start fail after jobworkerp is published as a
    /// partial child, allowing full-restart cleanup to be observed.
    #[cfg(test)]
    pub(crate) fn fail_next_start_after_jobworkerp_for_test(&self) {
        self.start_failure_injection.store(true, Ordering::Release);
    }

    #[cfg(test)]
    fn fail_next_stop_for_test(&self) {
        self.stop_failure_injection.store(true, Ordering::Release);
    }

    fn should_inject_start_failure(&self) -> bool {
        #[cfg(test)]
        {
            self.start_failure_injection.swap(false, Ordering::AcqRel)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    async fn stop_process(&self, process: &mut Process) -> AppResult<()> {
        #[cfg(test)]
        if self.stop_failure_injection.swap(false, Ordering::AcqRel) {
            return Err(AppError::Config(format!(
                "injected stop failure for {}",
                process.name
            )));
        }
        stop_child(&mut process.child, process.name).await
    }

    /// MCP HTTP server bind port the running jobworkerp child is listening
    /// on, or `None` when MCP is disabled / the sidecars are stopped.
    pub fn active_mcp_port(&self) -> Option<u16> {
        *self.active_mcp_port.lock()
    }

    pub fn mcp_runtime_state(&self) -> (McpRuntimeStatus, Option<String>) {
        let state = self.mcp_runtime_state.lock();
        (state.status, state.unavailable_reason.clone())
    }

    fn set_mcp_runtime_state(&self, status: McpRuntimeStatus, reason: Option<String>) {
        *self.mcp_runtime_state.lock() = McpRuntimeState {
            status,
            unavailable_reason: reason,
        };
    }

    /// Whether changing the keyring token cannot desynchronise a spawned or
    /// currently starting MCP listener.
    pub fn mcp_token_mutation_allowed(&self) -> bool {
        self.mcp_token_state.load(Ordering::Acquire) == MCP_TOKEN_STATE_IDLE
    }

    /// Run a keyring mutation only when no launch can observe the previous
    /// token. The compare-and-swap makes the check and reservation atomic.
    pub fn with_mcp_token_mutation<T>(
        &self,
        operation: impl FnOnce() -> AppResult<T>,
    ) -> AppResult<T> {
        let _reservation = self.reserve_mcp_token_mutation_from_idle()?;
        operation()
    }

    fn reserve_mcp_token_mutation_from_idle(&self) -> AppResult<McpTokenMutationReservation<'_>> {
        self.mcp_token_state
            .compare_exchange(
                MCP_TOKEN_STATE_IDLE,
                MCP_TOKEN_STATE_MUTATING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| {
                AppError::Config(
                    "MCP を停止してから token を再生成してください。稼働中または起動中の listener は現在の token を保持します。"
                        .to_string(),
                )
            })?;
        Ok(McpTokenMutationReservation {
            sidecars: self,
            active: true,
        })
    }

    /// Reserve a settings-save token rotation before stopping jobworkerp.
    ///
    /// A live listener already blocks standalone mutations, but replacing its
    /// `LaunchOrLive` state with `Mutating` before stop closes the gap between
    /// process shutdown and replacement launch. `stop_restart_targets` only
    /// releases the launch state, so it deliberately leaves this reservation
    /// intact until it is promoted to the replacement listener.
    fn reserve_mcp_token_mutation_for_restart(&self) -> AppResult<McpTokenMutationReservation<'_>> {
        let mut observed = self.mcp_token_state.load(Ordering::Acquire);
        loop {
            if observed == MCP_TOKEN_STATE_MUTATING {
                return Err(AppError::Config(
                    "MCP token is being generated or regenerated".to_string(),
                ));
            }
            match self.mcp_token_state.compare_exchange(
                observed,
                MCP_TOKEN_STATE_MUTATING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(McpTokenMutationReservation {
                        sidecars: self,
                        active: true,
                    });
                }
                Err(actual) => observed = actual,
            }
        }
    }

    fn reserve_mcp_token_for_launch(&self, enabled: bool) -> AppResult<()> {
        if !enabled {
            return Ok(());
        }
        self.mcp_token_state
            .compare_exchange(
                MCP_TOKEN_STATE_IDLE,
                MCP_TOKEN_STATE_LAUNCH_OR_LIVE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| {
                AppError::Config("MCP token is being generated or regenerated".to_string())
            })?;
        Ok(())
    }

    fn mcp_launch_reservation<'a>(&'a self, enabled: bool) -> AppResult<McpLaunchReservation<'a>> {
        self.reserve_mcp_token_for_launch(enabled)?;
        Ok(McpLaunchReservation {
            sidecars: self,
            active: enabled,
        })
    }

    fn release_mcp_token_after_stop(&self) {
        let _ = self.mcp_token_state.compare_exchange(
            MCP_TOKEN_STATE_LAUNCH_OR_LIVE,
            MCP_TOKEN_STATE_IDLE,
            Ordering::Release,
            Ordering::Relaxed,
        );
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
        self.last_report_with_issue_tracking(self.issue_tracking_enabled())
    }

    /// Spawn-time facts for the currently managed jobworkerp child, but only
    /// after this exact child's lifecycle startup completed and while its
    /// `Child` handle remains live. This deliberately does not reuse
    /// `last_report` as process health or SDK/runtime readiness evidence.
    pub fn managed_jobworkerp_spawn_snapshot(&self) -> Option<ManagedJobworkerpSpawnSnapshot> {
        let snapshot = {
            let observation = self.jobworkerp_spawn_observation.lock();
            let observation = observation.as_ref()?;
            if !observation.startup_complete {
                return None;
            }
            observation.snapshot.clone()
        };

        let mut state = self.state.lock();
        let process = state
            .as_mut()?
            .iter_mut()
            .find(|process| process.name == "jobworkerp")?;
        if process.child.id() != Some(snapshot.child_pid)
            || !matches!(process.child.try_wait(), Ok(None))
        {
            return None;
        }
        let observation = self.jobworkerp_spawn_observation.lock();
        if !observation.as_ref().is_some_and(|observation| {
            observation.startup_complete
                && observation.snapshot.generation_id == snapshot.generation_id
                && observation.snapshot.child_pid == snapshot.child_pid
        }) {
            return None;
        }
        Some(snapshot)
    }

    /// The selected and fixed machine-level SDK config source stamps for the
    /// currently managed live jobworkerp generation. This is a separate
    /// context snapshot so existing spawn-fact consumers need not construct
    /// machine-source metadata themselves.
    pub fn managed_jobworkerp_sdk_source_snapshot(
        &self,
    ) -> Option<ManagedJobworkerpSdkSourceSnapshot> {
        let snapshot = self.managed_jobworkerp_spawn_snapshot()?;
        let observation = self.jobworkerp_spawn_observation.lock();
        let observation = observation.as_ref()?;
        if !observation.startup_complete
            || observation.snapshot.generation_id != snapshot.generation_id
            || observation.snapshot.child_pid != snapshot.child_pid
        {
            return None;
        }
        Some(ManagedJobworkerpSdkSourceSnapshot {
            generation_id: snapshot.generation_id,
            child_pid: snapshot.child_pid,
            selected_config_source: snapshot.sdk_config_source,
            managed_sdk_config_source: observation.managed_sdk_config_source.clone(),
        })
    }

    /// Re-read both the selected config and the fixed machine override source
    /// for a previously captured snapshot, after confirming that the same
    /// managed child generation is still live.
    /// Callers should hold [`lock_native_jobworkerp_admission`](Self::lock_native_jobworkerp_admission)
    /// while validating and opening a native job so lifecycle replacement
    /// cannot race that short admission boundary.
    pub fn managed_jobworkerp_sdk_source_check(
        &self,
        expected: &ManagedJobworkerpSpawnSnapshot,
    ) -> ManagedJobworkerpSdkSourceCheck {
        self.managed_jobworkerp_sdk_source_check_at(
            expected,
            expected.applied_sdk_config_path.as_deref(),
            Path::new(MANAGED_SDK_CONFIG_PATH),
        )
    }

    fn managed_jobworkerp_sdk_source_check_at(
        &self,
        expected: &ManagedJobworkerpSpawnSnapshot,
        selected_source_path: Option<&Path>,
        managed_source_path: &Path,
    ) -> ManagedJobworkerpSdkSourceCheck {
        let Some(current) = self.managed_jobworkerp_spawn_snapshot() else {
            return ManagedJobworkerpSdkSourceCheck::Unavailable(
                ManagedJobworkerpSdkSourceUnavailableReason::ManagedChildUnavailable,
            );
        };
        if current.generation_id != expected.generation_id
            || current.child_pid != expected.child_pid
        {
            return ManagedJobworkerpSdkSourceCheck::Unavailable(
                ManagedJobworkerpSdkSourceUnavailableReason::ManagedChildReplaced,
            );
        }
        let managed_source_at_spawn = {
            let observation = self.jobworkerp_spawn_observation.lock();
            let Some(observation) = observation.as_ref() else {
                return ManagedJobworkerpSdkSourceCheck::Unavailable(
                    ManagedJobworkerpSdkSourceUnavailableReason::ManagedChildUnavailable,
                );
            };
            if !observation.startup_complete
                || observation.snapshot.generation_id != expected.generation_id
                || observation.snapshot.child_pid != expected.child_pid
            {
                return ManagedJobworkerpSdkSourceCheck::Unavailable(
                    ManagedJobworkerpSdkSourceUnavailableReason::ManagedChildReplaced,
                );
            }
            if observation.snapshot.sdk_config_source != expected.sdk_config_source {
                return ManagedJobworkerpSdkSourceCheck::Changed;
            }
            observation.managed_sdk_config_source.clone()
        };
        if current.applied_sdk_config_path != expected.applied_sdk_config_path {
            return ManagedJobworkerpSdkSourceCheck::Changed;
        }
        if selected_source_path != current.applied_sdk_config_path.as_deref()
            || source_stamp_path(&managed_source_at_spawn)
                .is_some_and(|path| path != managed_source_path)
        {
            return ManagedJobworkerpSdkSourceCheck::Changed;
        }

        let selected_source = capture_jobworkerp_sdk_source(selected_source_path);
        let managed_source = capture_jobworkerp_sdk_source(Some(managed_source_path));
        let selected_check =
            compare_jobworkerp_sdk_source_stamps(&expected.sdk_config_source, &selected_source);
        let managed_check =
            compare_jobworkerp_sdk_source_stamps(&managed_source_at_spawn, &managed_source);
        match (selected_check, managed_check) {
            (ManagedJobworkerpSdkSourceCheck::Unavailable(reason), _)
            | (_, ManagedJobworkerpSdkSourceCheck::Unavailable(reason)) => {
                ManagedJobworkerpSdkSourceCheck::Unavailable(reason)
            }
            (ManagedJobworkerpSdkSourceCheck::Changed, _)
            | (_, ManagedJobworkerpSdkSourceCheck::Changed) => {
                ManagedJobworkerpSdkSourceCheck::Changed
            }
            _ => ManagedJobworkerpSdkSourceCheck::Matches,
        }
    }

    #[cfg(test)]
    fn managed_jobworkerp_sdk_source_check_for_test(
        &self,
        expected: &ManagedJobworkerpSpawnSnapshot,
        selected_source_path: PathBuf,
        managed_source_path: PathBuf,
    ) -> ManagedJobworkerpSdkSourceCheck {
        self.managed_jobworkerp_sdk_source_check_at(
            expected,
            Some(&selected_source_path),
            &managed_source_path,
        )
    }

    /// Hold the existing lifecycle lock during a short native source-check +
    /// job-open admission section. This does not lock SDK files or claim the
    /// SDK runtime is ready; callers recheck the source after any longer probe.
    pub async fn lock_native_jobworkerp_admission(&self) -> ManagedJobworkerpAdmissionGuard<'_> {
        ManagedJobworkerpAdmissionGuard {
            _lifecycle_guard: self.lifecycle_lock.lock().await,
        }
    }

    fn record_jobworkerp_spawn(
        &self,
        snapshot: ManagedJobworkerpSpawnSnapshot,
        managed_sdk_config_source: ManagedJobworkerpSdkSourceStamp,
    ) {
        *self.jobworkerp_spawn_observation.lock() = Some(JobworkerpSpawnObservation {
            snapshot,
            managed_sdk_config_source,
            startup_complete: false,
        });
    }

    fn mark_jobworkerp_startup_complete(&self) {
        let Some((generation_id, child_pid)) = self
            .jobworkerp_spawn_observation
            .lock()
            .as_ref()
            .map(|observation| {
                (
                    observation.snapshot.generation_id,
                    observation.snapshot.child_pid,
                )
            })
        else {
            return;
        };

        let child_is_live = {
            let mut state = self.state.lock();
            state
                .as_mut()
                .and_then(|processes| {
                    processes
                        .iter_mut()
                        .find(|process| process.name == "jobworkerp")
                })
                .is_some_and(|process| {
                    process.child.id() == Some(child_pid)
                        && matches!(process.child.try_wait(), Ok(None))
                })
        };
        if !child_is_live {
            return;
        }

        if let Some(observation) = self.jobworkerp_spawn_observation.lock().as_mut()
            && observation.snapshot.generation_id == generation_id
            && observation.snapshot.child_pid == child_pid
        {
            observation.startup_complete = true;
        }
    }

    pub(crate) fn last_report_with_issue_tracking(
        &self,
        issue_tracking_enabled: bool,
    ) -> Option<SidecarStartReport> {
        self.last_report
            .lock()
            .clone()
            .map(|report| self.with_current_issue_tracker_status(report, issue_tracking_enabled))
    }

    fn issue_tracker_runtime_port(&self) -> Option<u16> {
        self.issue_tracker_runtime_port_with_issue_tracking(self.issue_tracking_enabled())
    }

    fn issue_tracker_runtime_port_with_issue_tracking(
        &self,
        issue_tracking_enabled: bool,
    ) -> Option<u16> {
        if !issue_tracking_enabled {
            return None;
        }
        let availability = self.issue_tracker_availability.lock().clone()?;
        availability
            .snapshot()
            .endpoint_connected
            .then(|| *self.issue_tracker_port.lock())
            .flatten()
    }

    pub fn issue_tracker_gateway_control_url(&self) -> Option<String> {
        self.issue_tracker_gateway_control_url_with_issue_tracking(self.issue_tracking_enabled())
    }

    pub fn issue_tracker_gateway_worker_url(&self) -> Option<String> {
        self.issue_tracker_gateway_worker_url_with_issue_tracking(self.issue_tracking_enabled())
    }

    fn issue_tracker_gateway_control_url_with_issue_tracking(
        &self,
        enabled: bool,
    ) -> Option<String> {
        self.issue_tracker_gateway_endpoint(enabled, &self.issue_tracker_gateway_control_url)
    }

    fn issue_tracker_gateway_worker_url_with_issue_tracking(
        &self,
        enabled: bool,
    ) -> Option<String> {
        self.issue_tracker_gateway_endpoint(enabled, &self.issue_tracker_gateway_worker_url)
    }

    fn issue_tracker_gateway_endpoint(
        &self,
        enabled: bool,
        endpoint: &Mutex<Option<String>>,
    ) -> Option<String> {
        if !enabled {
            return None;
        }
        let availability = self.issue_tracker_availability.lock().clone()?;
        if !availability.snapshot().gateway_ready {
            return None;
        }
        endpoint.lock().clone()
    }

    pub fn issue_tracker_availability(
        &self,
    ) -> Option<crate::commands::IssueTrackerAvailabilitySnapshot> {
        self.issue_tracker_availability_with_issue_tracking(self.issue_tracking_enabled())
    }

    pub(crate) fn issue_tracker_availability_with_issue_tracking(
        &self,
        // The start/stop paths already reflect the disabled state in the snapshot, so reads need no effective-value override; this parameter preserves symmetry.
        _issue_tracking_enabled: bool,
    ) -> Option<crate::commands::IssueTrackerAvailabilitySnapshot> {
        self.issue_tracker_availability
            .lock()
            .as_ref()
            .map(|availability| availability.snapshot())
    }

    /// Serializes Gateway policy registration from maintenance and Settings.
    pub(crate) async fn lock_issue_tracker_gateway_registration(
        &self,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.issue_tracker_gateway_registration_lock)
            .lock_owned()
            .await
    }

    fn with_current_issue_tracker_status(
        &self,
        mut report: SidecarStartReport,
        issue_tracking_enabled: bool,
    ) -> SidecarStartReport {
        if !issue_tracking_enabled {
            report
                .warnings
                .retain(|warning| warning.kind != SidecarWarningKind::IssueTrackerUnavailable);
            report.endpoints.issue_tracker_port = None;
            return report;
        }
        let Some(availability) = self.issue_tracker_availability.lock().clone() else {
            return report;
        };
        report
            .warnings
            .retain(|warning| warning.kind != SidecarWarningKind::IssueTrackerUnavailable);
        let snapshot = availability.snapshot();
        report.endpoints.issue_tracker_port = snapshot
            .endpoint_connected
            .then(|| *self.issue_tracker_port.lock())
            .flatten();
        let degraded =
            !snapshot.endpoint_connected || !snapshot.gateway_ready || !snapshot.mutation_ready;
        if degraded {
            let fallback_reason = if !snapshot.endpoint_connected {
                "Issue Tracker endpoint is unavailable"
            } else if !snapshot.mutation_ready && !snapshot.gateway_ready {
                "Issue Tracker mutations and Gateway are unavailable"
            } else if !snapshot.mutation_ready {
                "Issue Tracker mutations are unavailable"
            } else {
                "Issue Tracker Gateway is unavailable"
            };
            report.warnings.push(SidecarWarning {
                kind: SidecarWarningKind::IssueTrackerUnavailable,
                message: snapshot.reason.unwrap_or_else(|| fallback_reason.into()),
                detail: None,
            });
        }
        report
    }

    fn issue_tracking_enabled(&self) -> bool {
        crate::commands::issue_tracker_settings::effective_issue_tracking_enabled(&self.config.data)
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

    /// Whether the memories migration tool is currently changing the local
    /// database during startup.
    pub fn database_migration_in_progress(&self) -> bool {
        self.database_migration_in_progress.load(Ordering::Acquire)
    }

    fn database_migration_activity(&self) -> DatabaseMigrationActivity {
        DatabaseMigrationActivity::start(Arc::clone(&self.database_migration_in_progress))
    }

    /// Resolve the shared local memories runtime used by both resident
    /// sidecars and migration tools, including staged embedding workers.
    pub(crate) fn current_embedding_runtime(
        &self,
    ) -> crate::commands::embedding_settings::EmbeddingRuntime {
        self.embedding_runtime_for_data(&self.config.data)
    }

    fn embedding_runtime_for_data(
        &self,
        data: &crate::data::DataPaths,
    ) -> crate::commands::embedding_settings::EmbeddingRuntime {
        let settings = crate::commands::embedding_settings::load_embedding_settings(
            &data.embedding_settings_path(),
        );
        crate::commands::embedding_settings::resolve_embedding_runtime_with_env(&settings, |key| {
            self.embedding_env_lookup(key)
        })
    }

    pub(crate) fn embedding_env_lookup(&self, key: &str) -> Option<String> {
        self.memories_env_file_vars()
            .get(OsStr::new(key))
            .cloned()
            .or_else(|| self.external_embedding_env.get(OsStr::new(key)).cloned())
            .and_then(|value| value.into_string().ok())
    }

    /// Tauri's relaunch inherits this process's environment. Restore external
    /// inputs so the next process cannot mistake worker-expansion mirrors for
    /// user configuration, especially when it opens a different data root.
    pub(crate) fn restore_embedding_inputs_for_relaunch(&self) {
        let mut prepared = self.embedding_relaunch_prepared.lock();
        *prepared = true;
        // Serialize restoration with runtime mirrors, including startup tasks
        // that resume after relaunch preparation. Captured inputs stay stable.
        unsafe {
            for key in crate::commands::embedding_settings::EMBEDDING_ENV_KEYS {
                match self.external_embedding_env.get(OsStr::new(key)) {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn mirror_embedding_env(&self, vars: &[(&'static str, String)]) -> AppResult<()> {
        let prepared = self.embedding_relaunch_prepared.lock();
        if *prepared {
            return Err(AppError::Config(
                "embedding runtime is preparing to relaunch".into(),
            ));
        }
        // All production mirrors share the relaunch guard so no late startup
        // can replace the restored inputs before the next process inherits them.
        unsafe { apply_embedding_env(vars) };
        Ok(())
    }

    fn stage_embedding_workers_yaml(
        &self,
        runtime: &crate::commands::embedding_settings::EmbeddingRuntime,
    ) -> AppResult<PathBuf> {
        self.stage_embedding_workers_yaml_for_data(runtime, &self.config.data)
    }

    fn stage_embedding_workers_yaml_for_data(
        &self,
        runtime: &crate::commands::embedding_settings::EmbeddingRuntime,
        data: &crate::data::DataPaths,
    ) -> AppResult<PathBuf> {
        crate::commands::embedding_workers_yaml::stage_auto_embedding_workers_yaml(
            runtime,
            data,
            self.config.workflows_dir.as_deref(),
        )
    }

    fn memories_env_file_vars(&self) -> BTreeMap<OsString, OsString> {
        self.config
            .env_file
            .as_ref()
            .and_then(|path| dotenvy::from_path_iter(path).ok())
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect()
    }

    fn lance_language_model_home_for_data(&self, data: &crate::data::DataPaths) -> PathBuf {
        if data.root == self.config.data.root {
            self.config.lance_language_model_home.clone()
        } else {
            data.lance_language_model_home()
        }
    }

    fn memories_migration_extra_env(
        &self,
        data: &crate::data::DataPaths,
        runtime: &crate::commands::embedding_settings::EmbeddingRuntime,
        staged_workers_yaml: &Path,
        database_url: &str,
        vector_enabled: bool,
    ) -> BTreeMap<OsString, OsString> {
        let mut vars = BTreeMap::new();
        // The memories child inherits the process environment after the app
        // has cleared and rebuilt its managed embedding keys. Start with
        // env_file defaults, then apply the already-resolved runtime last.
        // That runtime folds env_file into its resolver while keeping saved
        // settings authoritative for managed identity fields.
        vars.extend(self.memories_env_file_vars());
        for &key in crate::commands::embedding_settings::EMBEDDING_ENV_KEYS {
            vars.insert(OsString::from(key), OsString::new());
        }
        for (key, value) in
            crate::commands::embedding_settings::resolve_embedding_env_vars_from_runtime(runtime)
        {
            vars.insert(OsString::from(key), OsString::from(value));
        }

        for (key, value) in memories_sqlite_env_vars(data) {
            vars.insert(OsString::from(key), OsString::from(value));
        }
        vars.insert(
            OsString::from("MEMORIES_ATLAS_DATABASE_URL"),
            OsString::from(database_url),
        );
        for (key, value) in memories_cache_env_vars() {
            vars.insert(OsString::from(key), OsString::from(value));
        }

        let vector_enabled_value = if vector_enabled { "true" } else { "false" };
        vars.insert(
            OsString::from("MEMORY_AUTO_EMBEDDING_ENABLED"),
            OsString::from(vector_enabled_value),
        );
        vars.insert(
            OsString::from("MEMORY_VECTOR_ENABLED"),
            OsString::from(vector_enabled_value),
        );
        vars.insert(
            OsString::from("THREAD_VECTOR_ENABLED"),
            OsString::from(vector_enabled_value),
        );
        vars.insert(
            OsString::from("REFLECTION_INTENT_VECTOR_ENABLED"),
            OsString::from(vector_enabled_value),
        );
        vars.insert(
            OsString::from("MEMORY_REFLECTION_DISPATCH_ENABLED"),
            OsString::from(if self.config.reflection_dispatch_enabled {
                "true"
            } else {
                "false"
            }),
        );
        vars.insert(
            OsString::from("MEMORY_FTS_TOKENIZER"),
            OsString::from(effective_memory_fts_tokenizer(
                self.config.env_file.as_deref(),
                self.config.lindera_dict_staged,
                crate::commands::process_env_lookup,
            )),
        );
        vars.insert(
            OsString::from("LANCE_LANGUAGE_MODEL_HOME"),
            self.lance_language_model_home_for_data(data)
                .into_os_string(),
        );
        let target_app_settings = crate::data::paths::load_app_settings(&data.app_settings_path());
        vars.insert(
            OsString::from("HF_HOME"),
            data.effective_hf_home(self.config.env_file.as_deref(), Some(&target_app_settings))
                .into_os_string(),
        );
        vars.insert(
            OsString::from("MEMORY_WORKERS_YAML"),
            staged_workers_yaml.as_os_str().to_owned(),
        );
        if let Some(workflows) = &self.config.workflows_dir {
            vars.insert(
                OsString::from("REFLECTION_INTENT_WORKERS_YAML"),
                workflows
                    .join("thread-reflection")
                    .join("auto-reflection-intent-embedding-workers.yaml")
                    .into_os_string(),
            );
            vars.insert(
                OsString::from("REFLECTION_WORKERS_YAML"),
                workflows
                    .join("thread-reflection")
                    .join("auto-reflection-summary-embedding-workers.yaml")
                    .into_os_string(),
            );
        }

        // THREAD_LANCEDB_* / THREAD_VECTOR_SIZE are deliberately not set:
        // memories derives thread storage from MEMORY_*, so the tool and the
        // server resolve the same store without Lookback copying defaults.
        if vector_enabled {
            vars.insert(
                OsString::from("MEMORY_VECTOR_SIZE"),
                OsString::from(runtime.vector_size.to_string()),
            );
            vars.insert(
                OsString::from("MEMORY_LANCEDB_URI"),
                data.memories_lancedb_uri().into_os_string(),
            );
        } else {
            // Empty overrides are the migration tool's equivalent of the
            // server Command's env_remove calls: the memories config treats
            // these optional vector settings as absent while vectors are off.
            for key in [
                "MEMORY_VECTOR_SIZE",
                "MEMORY_LANCEDB_URI",
                "REFLECTION_VECTOR_SIZE",
                "REFLECTION_LANCEDB_URI",
            ] {
                vars.insert(OsString::from(key), OsString::new());
            }
        }

        // memories rejects a document prefix because its internal chunking
        // would invalidate persisted offsets. The query prefix stays inherited
        // for retrieval, matching strip_memories_document_embedding_prefix.
        vars.insert(
            OsString::from(MEMORIES_UNSUPPORTED_DOCUMENT_PREFIX_ENV_KEY),
            OsString::new(),
        );
        vars
    }

    /// Environment shared by local migration coordinators and embedding
    /// maintenance. Staging is fallible because the CLI and memories child
    /// must use the same selected worker definition, never a bundled fallback.
    fn memories_migration_env(
        &self,
    ) -> AppResult<crate::sidecar::memories_migration::MigrationEnvironment> {
        let runtime = self.current_embedding_runtime();
        self.memories_migration_env_for_data(&self.config.data, &runtime)
    }

    fn memories_migration_env_for_data(
        &self,
        data: &crate::data::DataPaths,
        runtime: &crate::commands::embedding_settings::EmbeddingRuntime,
    ) -> AppResult<crate::sidecar::memories_migration::MigrationEnvironment> {
        let staged_workers_yaml = self.stage_embedding_workers_yaml_for_data(runtime, data)?;
        let database_url = data.memories_sqlite_url();
        let extra_env = self.memories_migration_extra_env(
            data,
            runtime,
            &staged_workers_yaml,
            &database_url,
            self.config.auto_embedding_enabled,
        );
        Ok(crate::sidecar::memories_migration::MigrationEnvironment {
            database_url,
            thread_vector_enabled: self.config.auto_embedding_enabled,
            memory_fts_tokenizer: effective_memory_fts_tokenizer(
                self.config.env_file.as_deref(),
                self.config.lindera_dict_staged,
                crate::commands::process_env_lookup,
            ),
            lance_language_model_home: self.lance_language_model_home_for_data(data),
            extra_env,
        })
    }

    /// Run an embedding lifecycle operation with the active app configuration.
    /// Callers coordinating mutations must hold `with_maintenance_lock`; this
    /// method deliberately does not stop resident children so Inspect/Plan can
    /// run while the service is available.
    pub(crate) async fn run_embedding_cli(
        &self,
        request: crate::sidecar::embedding_migration::EmbeddingRequest,
    ) -> AppResult<crate::sidecar::embedding_migration::EmbeddingMigrationResult> {
        let runtime = self.current_embedding_runtime();
        self.run_embedding_cli_with_runtime(request, &runtime).await
    }

    /// Run an embedding lifecycle operation with the exact candidate runtime
    /// used to construct its plan or switch target.
    pub(crate) async fn run_embedding_cli_with_runtime(
        &self,
        request: crate::sidecar::embedding_migration::EmbeddingRequest,
        runtime: &crate::commands::embedding_settings::EmbeddingRuntime,
    ) -> AppResult<crate::sidecar::embedding_migration::EmbeddingMigrationResult> {
        self.run_embedding_cli_for_data_inner(&self.config.data, runtime, request)
            .await
    }

    /// Run a read-only embedding inspect/plan against a validated candidate
    /// root. Setup uses this before committing the selected root or settings;
    /// it must not accidentally approve against the currently active database.
    pub(crate) async fn run_embedding_cli_for_data(
        &self,
        data: &crate::data::DataPaths,
        runtime: &crate::commands::embedding_settings::EmbeddingRuntime,
        request: crate::sidecar::embedding_migration::EmbeddingRequest,
    ) -> AppResult<crate::sidecar::embedding_migration::EmbeddingMigrationResult> {
        if !matches!(
            &request,
            crate::sidecar::embedding_migration::EmbeddingRequest::Inspect
                | crate::sidecar::embedding_migration::EmbeddingRequest::Plan { .. }
        ) {
            return Err(AppError::Config(
                "candidate-root embedding CLI calls are restricted to inspect and plan".into(),
            ));
        }
        if !data.root.is_absolute() || !data.root.is_dir() {
            return Err(AppError::Config(
                "embedding plan data root must be an existing absolute directory".into(),
            ));
        }
        self.run_embedding_cli_for_data_inner(data, runtime, request)
            .await
    }

    async fn run_embedding_cli_for_data_inner(
        &self,
        data: &crate::data::DataPaths,
        runtime: &crate::commands::embedding_settings::EmbeddingRuntime,
        request: crate::sidecar::embedding_migration::EmbeddingRequest,
    ) -> AppResult<crate::sidecar::embedding_migration::EmbeddingMigrationResult> {
        let env = self.memories_migration_env_for_data(data, runtime)?;
        crate::sidecar::embedding_migration::run_embedding(
            &self.config.memories_db_migrate_bin,
            &env,
            request,
            data,
        )
        .await
    }

    /// Runs the memories migration tool with the user's backup choice before
    /// any sidecar opens the database. The "migrating" notice starts only
    /// once the tool reaches a stage that can change data.
    async fn run_memories_migration(&self) -> AppResult<()> {
        use crate::sidecar::memories_migration::{BackupPolicy, run_local_apply};

        let settings = crate::data::paths::load_app_settings(&self.config.data.app_settings_path());
        let policy = BackupPolicy::from_settings(settings.migration_backup, &self.config.data);
        let env = self.memories_migration_env()?;
        let mut activity = None;
        let outcome = run_local_apply(
            &self.config.memories_db_migrate_bin,
            &env,
            &policy,
            &self.config.data,
            &mut || {
                activity.get_or_insert_with(|| self.database_migration_activity());
            },
        )
        .await?;
        info!(?outcome, "memories database migration finished");
        Ok(())
    }

    /// Run the normal Memories local-apply coordinator while the lifecycle
    /// lock is held and all resident writers have stopped.
    pub(crate) async fn run_local_apply_for_maintenance(&self) -> AppResult<()> {
        self.run_memories_migration().await
    }

    /// Restores a backup the memories migration tool reported. Callers must
    /// hold the maintenance lock so no sidecar has the database open.
    pub(crate) async fn restore_memories_backup(&self, backup: &Path) -> AppResult<()> {
        crate::sidecar::memories_migration::run_local_restore(
            &self.config.memories_db_migrate_bin,
            &self.memories_migration_env()?,
            backup,
            &self.config.data,
        )
        .await
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
                                | SidecarWarningKind::RemoteJobworkerpUnavailable
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
        let issue_tracking_enabled =
            crate::commands::issue_tracker_settings::effective_issue_tracking_enabled(
                &self.config.data,
            );
        if !issue_tracking_enabled {
            self.reset_issue_tracker_when_disabled();
        } else if !was_running {
            self.mark_issue_tracker_unavailable("sidecars are starting");
        }
        let result = self.start_inner(warnings, None).await;
        if result.is_err() {
            self.cleanup_failed_start().await;
        }
        match &result {
            Ok(report) => {
                *self.last_start_error.lock() = None;
                *self.last_start_failure.lock() = None;
                if Self::should_spawn_startup_orphan_sweep(was_running, true)
                    && let Some(jobworkerp_url) = report.endpoints.jobworkerp_url()
                {
                    crate::maintenance::spawn_startup_orphan_sweep(jobworkerp_url);
                }
            }
            Err(e) => {
                if issue_tracking_enabled {
                    self.mark_issue_tracker_unavailable(e.to_string());
                } else {
                    self.reset_issue_tracker_when_disabled();
                }
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

    /// Own every post-spawn startup failure at the lifecycle boundary so a
    /// newly activated MCP listener cannot outlive a failed sidecar launch.
    async fn cleanup_failed_start(&self) {
        if !self.owns_instance_lock() && self.state.lock().is_none() {
            // A competing instance may own the PID record. Nothing from this
            // launch has started, so cleanup must not remove that record.
            self.clear_active_mcp_secrets();
            self.release_mcp_token_after_stop();
            return;
        }
        if let Err(error) = self.stop_inner_locked(true).await {
            warn!(%error, "failed to finish sidecar cleanup after startup failure");
        }
    }

    fn should_spawn_startup_orphan_sweep(was_running: bool, start_succeeded: bool) -> bool {
        !was_running && start_succeeded
    }

    async fn start_inner(
        &self,
        mut warnings: Vec<SidecarWarning>,
        token_mutation: Option<&mut McpTokenMutationReservation<'_>>,
    ) -> AppResult<SidecarStartReport> {
        let issue_tracking_enabled =
            crate::commands::issue_tracker_settings::effective_issue_tracking_enabled(
                &self.config.data,
            );
        if !issue_tracking_enabled {
            self.reset_issue_tracker_when_disabled();
        }
        if let Some(endpoints) = self.current_endpoints() {
            let connection = crate::commands::connection::load_connection_config(
                &self.config.data.connection_config_path(),
            );
            if connection.memories.is_local() && endpoints.memories_port.is_none() {
                return Err(AppError::Config(
                    "local memories child is missing from tracked sidecar state; refusing to report the partial set as running"
                        .into(),
                ));
            }
            let report = SidecarStartReport {
                endpoints,
                warnings,
            };
            *self.last_report.lock() = Some(report.clone());
            return Ok(report);
        }
        if self.state.lock().is_some() {
            return Err(AppError::Config(
                "sidecar children remain tracked after an incomplete stop; refusing to spawn replacements"
                    .into(),
            ));
        }

        *self.jobworkerp_spawn_observation.lock() = None;

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
        let remote_memories = !connection.memories.is_local();
        if remote_memories {
            crate::commands::connection::validate_remote_memories_services(
                connection.memories.require_remote_url("memories")?,
            )
            .await?;
        }
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
        crate::commands::mcp_settings::recover_stale_mcp_secret_files(&self.config.data)?;

        if remote_memories {
            self.config.data.ensure_runtime()?;
        } else {
            self.config.data.ensure()?;
            if self.config.startup_database_migration_enabled {
                self.run_memories_migration().await?;
            }
        }

        let topology = LocalTopology::of(&connection);
        *self.started_topology.lock() = Some(topology);
        if !topology.jobworkerp {
            return self
                .start_with_remote_jobworkerp(
                    &connection,
                    topology,
                    issue_tracking_enabled,
                    &pids_path,
                    warnings,
                )
                .await;
        }

        let jw_port = ports::pick(9000)?;
        // A remote tracker needs no local server or ports.
        let remote_issue_tracker =
            issue_tracking_enabled && !connection.issue_tracker.endpoint.is_local();
        let issue_tracker_start = issue_tracker::issue_tracker_start_decision(
            issue_tracking_enabled && connection.issue_tracker.endpoint.is_local(),
            self.config.issue_tracker_server_bin.is_some(),
        );
        // Gateway worker definitions use these ports only when the tracker is
        // enabled. Avoid reserving loopback ports for a disabled integration.
        let (
            issue_tracker_port,
            issue_tracker_gateway_control_port,
            issue_tracker_gateway_worker_port,
        ) = match issue_tracker_start {
            issue_tracker::IssueTrackerStartDecision::Disabled => (None, None, None),
            issue_tracker::IssueTrackerStartDecision::Start
            | issue_tracker::IssueTrackerStartDecision::Unavailable => (
                Some(ports::pick(0)?),
                Some(ports::pick(0)?),
                Some(ports::pick(0)?),
            ),
        };
        let mem_port = (!remote_memories).then(|| ports::pick(9010)).transpose()?;
        let memories_callback = resolve_worker_memories_callback(&connection, mem_port)?;
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
        // Resolve runtime + env vars from the SAME env-aware projection so
        // the staged YAML, vector store dimension, and worker-YAML expansion
        // all select one model runtime.
        let embedding_runtime = self.current_embedding_runtime();
        let staged_embedding_workers_yaml =
            self.stage_embedding_workers_yaml(&embedding_runtime)?;

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
        let mcp_settings_path = self.config.data.mcp_settings_path();
        let mut mcp_settings = crate::commands::mcp_settings::load_mcp_settings(&mcp_settings_path);
        if mcp_settings.enabled {
            let token_configured = crate::commands::mcp_settings::load_mcp_auth_token()?.is_some();
            if crate::commands::mcp_settings::migrate_legacy_enabled_mcp_without_auth_token(
                &mcp_settings_path,
                token_configured,
            )? {
                warnings.push(SidecarWarning {
                    kind: SidecarWarningKind::McpAuthenticationMigration,
                    message: "MCP was disabled because an existing enabled setting has no bearer token; generate and copy a token before enabling MCP again".to_string(),
                    detail: None,
                });
                mcp_settings = crate::commands::mcp_settings::load_mcp_settings(&mcp_settings_path);
            }
        }
        let mcp_runtime = crate::commands::mcp_settings::resolve_mcp_runtime(&mcp_settings);
        self.set_mcp_runtime_state(McpRuntimeStatus::Disabled, None);
        let mcp_requires_token = mcp_auth_enabled(
            mcp_runtime.enabled,
            mcp_runtime.allow_unauthenticated_requests,
        );
        let mcp_launch_reservation = match token_mutation {
            Some(mutation) => mutation.promote_to_launch(mcp_requires_token)?,
            None => self.mcp_launch_reservation(mcp_requires_token)?,
        };
        let mcp_launch_secrets = if mcp_auth_enabled(
            mcp_runtime.enabled,
            mcp_runtime.allow_unauthenticated_requests,
        ) {
            match self.prepare_mcp_launch_secrets() {
                Ok(secrets) => Some(secrets),
                Err(error) => {
                    // The instance lock was acquired before settings and
                    // secret preparation. Release it on this pre-spawn
                    // failure so a keyring/file error cannot wedge restart.
                    return Err(error);
                }
            }
        } else {
            None
        };

        // jobworkerp's workflows read the tracker endpoint from the
        // environment inherited at spawn, while the tracker starts only once
        // jobworkerp listens, so publish the reserved ports first. It is
        // published even if the tracker never becomes ready: worker YAML
        // expansion keeps the gateway workers so MCP clients see a stable
        // tool list, and only the eventual GRPC call fails. A selected restart
        // respawns jobworkerp with the endpoint published here.
        let remote_issue_tracker = remote_issue_tracker
            && self.publish_remote_issue_tracker_endpoint(&connection, &mut warnings);
        if let (Some(primary_port), Some(gateway_worker_port)) =
            (issue_tracker_port, issue_tracker_gateway_worker_port)
        {
            issue_tracker::publish_issue_tracker_endpoint_env(
                &crate::commands::connection::GrpcAddress::loopback(primary_port),
                &crate::commands::connection::GrpcAddress::loopback(gateway_worker_port),
            );
        }
        let jw_child = self.spawn_jobworkerp(
            jw_port,
            mcp_server_port,
            &mcp_runtime,
            mcp_launch_secrets.as_ref(),
            &rust_log,
            &llm_settings,
            llm_model.as_deref(),
            llm_hf_repo.as_deref(),
            llm_ctx_size,
            Some(llm_kv_cache_type.as_str()),
            llm_mtp_enabled,
            llm_mtp_draft_model.as_deref(),
            issue_tracking_enabled,
        )?;
        let mut processes = vec![Process {
            name: "jobworkerp",
            child: jw_child,
            port: jw_port,
        }];

        // Stash early so we drop processes even if health check fails.
        *self.state.lock() = Some(std::mem::take(&mut processes));
        // From this point a child can still accept the launch token even if
        // later startup fails. Stop lifecycle now owns the reservation; only
        // failures before the first spawn are released by the RAII guard.
        mcp_launch_reservation.commit();

        if self.should_inject_start_failure() {
            return Err(AppError::Config("injected startup failure".to_string()));
        }

        wait_for_tcp_or_failure(("127.0.0.1", jw_port), Duration::from_secs(30), None).await?;

        if remote_issue_tracker {
            self.connect_remote_issue_tracker(&connection.issue_tracker, &mut warnings)
                .await;
        } else if let (
            Some(issue_tracker_port),
            Some(issue_tracker_gateway_control_port),
            Some(issue_tracker_gateway_worker_port),
        ) = (
            issue_tracker_port,
            issue_tracker_gateway_control_port,
            issue_tracker_gateway_worker_port,
        ) {
            self.start_issue_tracker_after_jobworkerp(
                issue_tracker_port,
                issue_tracker_gateway_control_port,
                issue_tracker_gateway_worker_port,
                &rust_log,
                issue_tracking_enabled,
                &mut warnings,
            )
            .await?;
        }

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
        self.apply_worker_registration_env(&memories_callback)?;

        // Apply LLM and batch workers before Memories spawns so its first
        // workflow dispatch can resolve `workerName: memories-llm`. RAG
        // workers are deferred until Memories returns its current embedding
        // worker name below.
        // Non-fatal: memories can still boot; failures surface as
        // `WorkerApplyFailed` warnings so the UI can disable LLM-tagged
        // flows instead of letting the user wait for jobs that will
        // forever return `WorkerNotFound`.
        let mut mcp_toolset_error = None;
        let server_url = loopback_url(jw_port);
        let handle = wait_for_jobworkerp_grpc_ready(&server_url).await?;
        let worker_yamls =
            worker_paths_with_issue_chat(&self.config.worker_yaml_paths, issue_tracking_enabled);
        register_runtime_workers_before_memories(
            &handle,
            &worker_yamls,
            issue_tracking_enabled,
            &mut warnings,
        )
        .await?;

        // Register the language-specific generation workers
        // (`memories-<feature>-single-<lang>`) BEFORE memories spawns: the
        // batches resolve them by name on their first dispatch. jobworkerp is
        // already accepting connections (TCP wait above) and the named
        // `memories-llm` worker is registered, so the upsert can connect and
        // the single workflows can route their LLM step. Fail-soft: a failure
        // is a `WorkerApplyFailed` warning, not a startup abort.
        crate::sidecar::generation_workers::register_generation_workers(
            &loopback_url(jw_port),
            &self.config.data.log_dir(),
            &mut warnings,
        )
        .await;

        if let Some(mem_port) = mem_port
            && let Err(error) = self
                .start_local_memories(
                    mem_port,
                    jw_port,
                    &rust_log,
                    &embedding_runtime,
                    &staged_embedding_workers_yaml,
                    &mut warnings,
                )
                .await
        {
            return Err(error);
        }

        let memories_url = crate::commands::connection::memories_url(&connection, mem_port)?;
        let embedding_space = current_embedding_space(&memories_url).await?;
        let query_embedding = super::query_embedding_worker::prepare_query_embedding(
            &handle,
            &embedding_space,
            &embedding_runtime,
            self.config.workflows_dir.as_deref(),
            &mut warnings,
        )
        .await;
        register_runtime_memories_dependent_workers(
            &handle,
            &worker_yamls,
            &query_embedding,
            &crate::commands::connection::GrpcAddress::loopback(jw_port),
            issue_tracking_enabled,
            &mut warnings,
        )
        .await?;
        register_runtime_function_sets(
            &handle,
            &self.config.function_set_yaml_paths,
            &self.config.data.root,
            issue_tracking_enabled,
            &mut warnings,
        )
        .await?;
        if mcp_runtime.enabled
            && let Err(error) =
                validate_mcp_toolset_for_issue_tracking(&handle, issue_tracking_enabled).await
        {
            if !issue_tracking_enabled {
                return Err(error);
            }
            let reason = error.to_string();
            warn!(error = %reason, "MCP toolset validation failed");
            warnings.push(mcp_toolset_unavailable_warning(&reason));
            mcp_toolset_error = Some(reason);
        }

        self.start_local_conductor(
            conductor_port,
            &rust_log,
            &crate::commands::connection::GrpcAddress::loopback(jw_port),
            &crate::commands::periodic_tasks::PeriodicServices::new(
                &connection,
                memories_callback.clone(),
            ),
            &mut warnings,
        )
        .await?;

        // Record the live PIDs so a crash of this host (which skips Drop /
        // kill_on_drop) is recoverable on the next launch. Best-effort: a
        // write failure only loses crash-recovery for this session.
        self.record_pids(&pids_path);

        // Advertise the MCP bound port only now that the whole trio is up: a
        // `spawn_memories` / `spawn_conductor` can fail after jobworkerp is
        // live, so publish the port only after all required sidecars are up.
        //
        // The MCP server binds a SEPARATE socket inside the jobworkerp process
        // (`tokio::join!` with the gRPC front), so the gRPC check above does
        // NOT prove the MCP port bound (a `ports::pick` clash or MCP startup
        // error could leave gRPC up but MCP dead). Confirm it whenever MCP is
        // enabled, independently of FunctionSet registration timing.
        let mcp_listener_ready = if mcp_runtime.enabled {
            match wait_for_tcp_or_failure(("127.0.0.1", mcp_server_port), MCP_HEALTH_TIMEOUT, None)
                .await
            {
                Ok(()) => true,
                Err(error) => {
                    warn!(error = %error, "MCP listener did not become ready (continuing without MCP)");
                    warnings.push(mcp_unavailable_warning(&error));
                    false
                }
            }
        } else {
            false
        };
        let (mcp_runtime_status, mcp_unavailable_reason) =
            mcp_runtime_readiness(mcp_runtime.enabled, mcp_listener_ready, mcp_toolset_error);
        let mcp_ready = mcp_runtime_status == McpRuntimeStatus::Ready;
        *self.active_mcp_port.lock() =
            mcp_listener_port(mcp_runtime.enabled, mcp_ready, mcp_server_port);
        self.set_mcp_runtime_state(mcp_runtime_status, mcp_unavailable_reason);
        *self.active_mcp_secrets.lock() = mcp_launch_secrets;

        info!(warning_count = warnings.len(), "sidecars ready");
        let report = SidecarStartReport {
            endpoints: SidecarEndpoints {
                jobworkerp_port: Some(jw_port),
                issue_tracker_port: *self.issue_tracker_port.lock(),
                memories_port: mem_port,
                conductor_port: Some(conductor_port),
                // Just stashed above (only when MCP is enabled). Read it back
                // rather than recomputing so the report and `get_mcp_settings`
                // always agree on the live port.
                mcp_server_port: *self.active_mcp_port.lock(),
            },
            warnings,
        };
        self.mark_jobworkerp_startup_complete();
        *self.last_report.lock() = Some(report.clone());
        Ok(report)
    }

    /// Start what this app still runs while jobworkerp is remote: conductor,
    /// when it is local. memories is remote by validation, and MCP is served
    /// by the remote jobworkerp. No workers are registered here; the remote
    /// jobworkerp must already have Lookback's workers.
    async fn start_with_remote_jobworkerp(
        &self,
        connection: &crate::commands::connection::ConnectionConfig,
        topology: LocalTopology,
        issue_tracking_enabled: bool,
        pids_path: &Path,
        mut warnings: Vec<SidecarWarning>,
    ) -> AppResult<SidecarStartReport> {
        let jobworkerp_url = connection.jobworkerp.require_remote_url("jobworkerp")?;
        // Validation requires a remote tracker beside a remote jobworkerp.
        // Its address is published for registering the remote jobworkerp's
        // workers.
        if issue_tracking_enabled
            && self.publish_remote_issue_tracker_endpoint(connection, &mut warnings)
        {
            self.connect_remote_issue_tracker(&connection.issue_tracker, &mut warnings)
                .await;
        }
        *self.state.lock() = Some(Vec::new());
        let memories_callback = resolve_worker_memories_callback(connection, None)?;
        let jobworkerp = crate::commands::connection::jobworkerp_for_dependents(connection, None)?;
        let services =
            crate::commands::periodic_tasks::PeriodicServices::new(connection, memories_callback);
        // Checking the remote jobworkerp only feeds a warning, so it runs
        // beside conductor's start rather than ahead of it.
        let startup_warning = self.remote_jobworkerp_startup_warning(connection, jobworkerp_url);
        let conductor = async {
            if topology.conductor {
                let port = ports::pick(9020)?;
                self.start_local_conductor(
                    port,
                    &effective_rust_log(),
                    &jobworkerp,
                    &services,
                    &mut warnings,
                )
                .await?;
                Ok(Some(port))
            } else {
                self.refresh_periodic_runtime_after_conductor_start(
                    connection.conductor.require_remote_url("conductor")?,
                    &jobworkerp,
                    &services,
                    &crate::commands::connection::conductor_identity(connection),
                    &mut warnings,
                )
                .await;
                AppResult::Ok(None)
            }
        };
        let (startup_warning, conductor_port) = tokio::join!(startup_warning, conductor);
        let conductor_port = conductor_port?;
        warnings.extend(startup_warning);
        let endpoints = SidecarEndpoints {
            jobworkerp_port: None,
            issue_tracker_port: None,
            memories_port: None,
            conductor_port,
            mcp_server_port: None,
        };
        self.record_pids(pids_path);
        *self.active_mcp_port.lock() = None;
        self.set_mcp_runtime_state(McpRuntimeStatus::Disabled, None);

        info!(
            warning_count = warnings.len(),
            "sidecars ready with a remote jobworkerp"
        );
        let report = SidecarStartReport {
            endpoints,
            warnings,
        };
        *self.last_report.lock() = Some(report.clone());
        Ok(report)
    }

    /// Set the values worker YAML expansion reads from this process's
    /// environment: the memories callback (as jobworkerp reaches it), the
    /// LLM settings and the embedding runtime. Registration expands
    /// `%{...}` here, not in the jobworkerp child, so this runs before every
    /// registration, local or remote.
    ///
    /// The Settings inspect / register commands also call this after boot;
    /// that is the same env-mutation contract the settings-apply path
    /// already relies on (`set_var` while other tasks may read the env).
    fn apply_worker_registration_env(
        &self,
        memories_callback: &crate::commands::connection::GrpcAddress,
    ) -> AppResult<()> {
        let llm_settings =
            crate::commands::llm_settings::load_llm_settings(&self.config.data.llm_settings_path());
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
        let embedding_env_vars =
            crate::commands::embedding_settings::resolve_embedding_env_vars_from_runtime(
                &self.current_embedding_runtime(),
            );
        unsafe {
            apply_memories_callback_env(memories_callback);
            // RAG retrieval endpoint for `lookback-recall.yaml`'s
            // `memories_grpc_*` input DEFAULTS. The chat path injects these
            // per-dispatch (chat.rs) so the defaults never apply there; the
            // MCP path runs the workflow directly with no Tauri in the loop,
            // so these resolved defaults are the only endpoint it gets: the
            // memories address as the target jobworkerp reaches it.
            apply_rag_memories_env(memories_callback);
            apply_external_llm_env(
                llm_settings.provider_model.as_deref(),
                llm_settings.base_url.as_deref(),
            );
            // Same reason as the External-LLM block above, for the Local
            // (llama-cpp) placeholders in `llm-workers.yaml`. The Tauri
            // process runs `expand_env` against ITS OWN env when it
            // registers the worker YAMLs; without mirroring here the
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
            // Mirror embedding settings into the host env for jobworkerp's
            // worker-YAML expansion. Memories receives the same resolved
            // values through the staged worker manifest and child environment.
            self.mirror_embedding_env(&embedding_env_vars)?;
        }
        Ok(())
    }

    /// Spawn conductor at `port`, wait for it, and point its schedulers at
    /// `jobworkerp` as conductor reaches it.
    async fn start_local_conductor(
        &self,
        port: u16,
        rust_log: &str,
        jobworkerp: &crate::commands::connection::GrpcAddress,
        services: &crate::commands::periodic_tasks::PeriodicServices,
        warnings: &mut Vec<SidecarWarning>,
    ) -> AppResult<()> {
        let child = self.spawn_conductor(port, rust_log)?;
        self.state
            .lock()
            .as_mut()
            .expect("running sidecar state")
            .push(Process {
                name: "conductor",
                child,
                port,
            });
        wait_for_tcp_or_failure(("127.0.0.1", port), Duration::from_secs(30), None).await?;
        self.refresh_periodic_runtime_after_conductor_start(
            &loopback_url(port),
            jobworkerp,
            services,
            crate::commands::connection::LOCAL_CONDUCTOR_IDENTITY,
            warnings,
        )
        .await;
        Ok(())
    }

    /// The services the running set keeps locally. State published without
    /// a recorded topology (unit tests) runs everything locally.
    fn running_topology(&self) -> LocalTopology {
        self.started_topology
            .lock()
            .unwrap_or(LocalTopology::ALL_LOCAL)
    }

    pub fn current_endpoints(&self) -> Option<SidecarEndpoints> {
        let guard = self.state.lock();
        let procs = guard.as_ref()?;
        let topology = self.running_topology();
        let jw = procs
            .iter()
            .find(|p| p.name == "jobworkerp")
            .map(|p| p.port);
        let mem = procs.iter().find(|p| p.name == "memories").map(|p| p.port);
        let conductor = procs.iter().find(|p| p.name == "conductor").map(|p| p.port);
        if (topology.conductor && conductor.is_none()) || (topology.jobworkerp && jw.is_none()) {
            return None;
        }
        Some(SidecarEndpoints {
            jobworkerp_port: jw,
            issue_tracker_port: self.issue_tracker_runtime_port(),
            memories_port: mem,
            conductor_port: conductor,
            mcp_server_port: *self.active_mcp_port.lock(),
        })
    }

    /// Ports for a selected restart. A failed replacement is removed before
    /// returning so its MCP token reservation is released; rollback then uses
    /// the last successful report to reconstruct the same fixed endpoints.
    fn restart_endpoints(&self) -> Option<SidecarEndpoints> {
        if let Some(endpoints) = self.current_endpoints() {
            return Some(endpoints);
        }
        // `stop()` clears the process state entirely. Only a selected restart
        // leaves `Some(Vec::new())` or its non-target children behind, which
        // is the recovery state allowed to reuse a prior endpoint report.
        if self.state.lock().is_none() {
            return None;
        }
        self.last_report().map(|report| report.endpoints)
    }

    pub async fn stop(&self) -> AppResult<()> {
        self.stop_inner(true).await
    }

    /// Stop and start the complete sidecar set while holding the lifecycle
    /// lock and the data-root instance lock. The caller owns persistence and
    /// rollback; this primitive keeps the running-state transition atomic and
    /// never re-enters the public stop/start wrappers.
    pub(crate) async fn full_restart_with<F>(
        &self,
        warnings: Vec<SidecarWarning>,
        token_rotation: bool,
        before_start: F,
    ) -> AppResult<SidecarStartReport>
    where
        F: FnOnce() -> AppResult<()>,
    {
        let _lifecycle_guard = self.lifecycle_lock.lock().await;
        if !self.owns_instance_lock() {
            return Err(self.full_restart_ownership_error());
        }
        let mut token_mutation = if token_rotation {
            Some(self.reserve_mcp_token_mutation_for_restart()?)
        } else {
            None
        };
        self.full_restart_locked(warnings, token_mutation.as_mut(), before_start)
            .await
    }

    /// Body of `full_restart_with` for callers that already hold the
    /// lifecycle lock and the data-root instance lock.
    async fn full_restart_locked<F>(
        &self,
        warnings: Vec<SidecarWarning>,
        token_mutation: Option<&mut McpTokenMutationReservation<'_>>,
        before_start: F,
    ) -> AppResult<SidecarStartReport>
    where
        F: FnOnce() -> AppResult<()>,
    {
        if !self.owns_instance_lock() {
            return Err(self.full_restart_ownership_error());
        }
        self.stop_inner_locked(false).await?;
        let result = match before_start() {
            Ok(()) => self.start_inner(warnings, token_mutation).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(report) => {
                *self.last_start_error.lock() = None;
                *self.last_start_failure.lock() = None;
                Ok(report)
            }
            Err(error) => {
                // `start_inner` owns the first child only after it publishes
                // process state. Clean that partial startup while retaining
                // the transaction owner's instance lock for a retry. The same
                // path also handles a pre-start callback failure so callers
                // observe one consistent failure record.
                if let Err(cleanup_error) = self.stop_inner_locked(false).await {
                    warn!(
                        error = %cleanup_error,
                        "failed to clean up a full-restart startup failure"
                    );
                }
                self.record_full_restart_failure(&error);
                Err(error)
            }
        }
    }

    fn record_full_restart_failure(&self, error: &AppError) {
        *self.last_start_error.lock() = Some(error.to_string());
        *self.last_start_failure.lock() =
            Some(crate::sidecar::startup_error::SidecarErrorPayload::from_app_error(error));
    }

    /// Full-restart primitive with an internal recovery continuation. The
    /// settings transaction supplies rollback and old-configuration restart so
    /// the lifecycle lock also covers failure recovery.
    pub(crate) async fn full_restart_with_transaction<F, R, RFut, P, PFut>(
        &self,
        warnings: Vec<SidecarWarning>,
        token_rotation: bool,
        before_start: F,
        recover: R,
        post_start: P,
    ) -> AppResult<SidecarStartReport>
    where
        F: FnOnce() -> AppResult<()>,
        R: FnOnce() -> RFut,
        RFut: Future<Output = AppResult<Vec<SidecarWarning>>>,
        P: Fn() -> PFut,
        PFut: Future<Output = AppResult<()>>,
    {
        let _lifecycle_guard = self.lifecycle_lock.lock().await;
        if !self.owns_instance_lock() {
            return Err(self.full_restart_ownership_error());
        }

        let mut token_mutation = if token_rotation {
            Some(self.reserve_mcp_token_mutation_for_restart()?)
        } else {
            None
        };
        self.stop_inner_locked(false).await?;

        let result = match before_start() {
            Ok(()) => self.start_inner(warnings, token_mutation.as_mut()).await,
            Err(error) => Err(error),
        };
        let result = match result {
            Ok(report) => {
                match run_full_restart_post_start(&post_start, FULL_RESTART_POST_START_TIMEOUT)
                    .await
                {
                    Ok(()) => Ok(report),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        let Err(error) = result else {
            return result;
        };

        let cleanup_result = self.stop_inner_locked(false).await;
        drop(token_mutation);
        let original_error = error;
        let recovery_result = match cleanup_result {
            Ok(()) => {
                async {
                    let recovery_warnings = recover().await?;
                    if let Err(error) = self.start_inner(recovery_warnings, None).await {
                        let _ = self.stop_inner_locked(false).await;
                        return Err(error);
                    }
                    if let Err(error) =
                        run_full_restart_post_start(&post_start, FULL_RESTART_POST_START_TIMEOUT)
                            .await
                    {
                        let _ = self.stop_inner_locked(false).await;
                        return Err(error);
                    }
                    Ok(())
                }
                .await
            }
            Err(cleanup_error) => Err(cleanup_error),
        };
        if recovery_result.is_ok() {
            self.record_full_restart_failure(&original_error);
            return Err(AppError::Config(original_error.to_string()));
        }

        let recovery_error = recovery_result
            .expect_err("recovery result was checked to be an error")
            .to_string();
        let error = format!(
            "{}; フル再起動の復旧に失敗しました: {recovery_error}",
            original_error
        );
        let failure = match &original_error {
            AppError::SidecarStartupFailed(failure) => {
                crate::sidecar::startup_error::SidecarErrorPayload::RecoveryFailed {
                    failure: failure.as_ref().clone(),
                    recovery_error,
                }
            }
            _ => crate::sidecar::startup_error::SidecarErrorPayload::Raw {
                message: error.clone(),
            },
        };
        let returned_error = AppError::Config(error.clone());
        *self.last_start_error.lock() = Some(error);
        *self.last_start_failure.lock() = Some(failure);
        Err(returned_error)
    }

    fn full_restart_ownership_error(&self) -> AppError {
        if self.state.lock().is_none() {
            AppError::Config("sidecars are not running; no restart target exists".to_string())
        } else {
            AppError::AnotherInstanceRunning
        }
    }

    /// Restart the sidecars selected by a settings change. While jobworkerp
    /// is remote, or when the saved connection changes which services run
    /// locally, the selection cannot be honoured and the whole set restarts.
    ///
    /// `before_start` runs after the selected processes have stopped and
    /// before their replacement observes persisted configuration. This makes
    /// keyring-backed MCP token rotation atomic with the jobworkerp launch.
    /// An empty plan is a no-op and returns the current report.
    pub async fn restart_plan_with<F>(
        &self,
        plan: RestartPlan,
        warnings: Vec<SidecarWarning>,
        before_start: F,
    ) -> AppResult<SidecarStartReport>
    where
        F: FnOnce() -> AppResult<()>,
    {
        let _lifecycle_guard = self.lifecycle_lock.lock().await;
        self.restart_plan_with_locked(plan, warnings, before_start, None)
            .await
    }

    /// Restart selected sidecars while serializing a keyring-backed MCP token
    /// rotation with the replacement listener. The reservation starts before
    /// jobworkerp stops and is promoted directly into the new listener's
    /// launch reservation, so standalone rotation commands cannot alter the
    /// token observed by either the listener or the save response.
    pub async fn restart_plan_with_mcp_token_mutation<F>(
        &self,
        plan: RestartPlan,
        warnings: Vec<SidecarWarning>,
        before_start: F,
    ) -> AppResult<SidecarStartReport>
    where
        F: FnOnce() -> AppResult<()>,
    {
        let _lifecycle_guard = self.lifecycle_lock.lock().await;
        if !plan.includes(SidecarKind::Jobworkerp) {
            return Err(AppError::Config(
                "MCP token rotation requires a jobworkerp restart".to_string(),
            ));
        }
        let mut token_mutation = self.reserve_mcp_token_mutation_for_restart()?;
        self.restart_plan_with_locked(plan, warnings, before_start, Some(&mut token_mutation))
            .await
    }

    async fn restart_plan_with_locked<F>(
        &self,
        plan: RestartPlan,
        warnings: Vec<SidecarWarning>,
        before_start: F,
        token_mutation: Option<&mut McpTokenMutationReservation<'_>>,
    ) -> AppResult<SidecarStartReport>
    where
        F: FnOnce() -> AppResult<()>,
    {
        if plan.is_empty() {
            return self.last_report().ok_or_else(|| {
                AppError::Config("sidecars are not running; no restart target exists".to_string())
            });
        }
        // Selected restarts reuse the running local jobworkerp. When it is
        // remote, or the set of local services changes, the children to run
        // differ, so the whole set is restarted from the saved configuration.
        let wanted = LocalTopology::of(&crate::commands::connection::load_connection_config(
            &self.config.data.connection_config_path(),
        ));
        if wanted != self.running_topology() || !wanted.jobworkerp {
            return self
                .full_restart_locked(warnings, token_mutation, before_start)
                .await;
        }
        let endpoints = self.restart_endpoints().ok_or_else(|| {
            AppError::Config("sidecars are not running; no restart target exists".to_string())
        })?;

        // Do not use `stop_inner_locked` here. It deliberately tears down
        // every child, clears the Issue Tracker maintenance loop, and releases
        // all launch-scoped state for application shutdown. A settings save
        // must leave every non-selected resident child completely untouched.
        self.stop_restart_targets(&plan).await?;
        self.reset_selected_restart_attempt_state(&plan);
        before_start()?;
        let result = self
            .start_restart_targets(&plan, &endpoints, warnings, token_mutation)
            .await;
        match &result {
            Ok(_) => self.complete_successful_selected_restart(),
            Err(error) => {
                // A spawned replacement can still retain the newly rotated token
                // after its later readiness check fails. Stop every selected
                // partial child before returning so the rollback can atomically
                // restore the old keyring value, while non-target children stay
                // alive throughout the transaction.
                match self.cleanup_failed_restart_targets(&plan).await {
                    Ok(()) => {
                        *self.last_start_error.lock() = Some(error.to_string());
                        *self.last_start_failure.lock() = Some(
                            crate::sidecar::startup_error::SidecarErrorPayload::from_app_error(
                                error,
                            ),
                        );
                        return result;
                    }
                    Err(cleanup_error) => {
                        let reported_error = AppError::Config(format!(
                            "{error}; failed to stop selected sidecars after restart failure: {cleanup_error}"
                        ));
                        *self.last_start_error.lock() = Some(reported_error.to_string());
                        *self.last_start_failure.lock() = Some(
                            crate::sidecar::startup_error::SidecarErrorPayload::from_app_error(
                                &reported_error,
                            ),
                        );
                        return Err(reported_error);
                    }
                }
            }
        }
        result
    }

    /// Reset per-attempt diagnostics that would otherwise make a healthy
    /// replacement fail its own readiness check based on an older child.
    fn reset_selected_restart_attempt_state(&self, plan: &RestartPlan) {
        *self.startup_failure.lock() = None;
        if plan.includes(SidecarKind::Memories) {
            *self.degraded.lock() = None;
        }
    }

    /// Commit state that is only valid after every selected replacement has
    /// completed successfully. Degraded mode is reset when a memories
    /// replacement begins, then re-established only when its retry is needed.
    fn complete_successful_selected_restart(&self) {
        *self.last_start_error.lock() = None;
        *self.last_start_failure.lock() = None;
    }

    /// Stop partial replacement children after a selected restart fails.
    ///
    /// This shares the normal selected-stop path so jobworkerp also releases
    /// its MCP token launch reservation before callers restore a credential.
    async fn cleanup_failed_restart_targets(&self, plan: &RestartPlan) -> AppResult<()> {
        self.stop_restart_targets(plan).await
    }

    /// Stop only the children selected by a settings restart. Output pumps and
    /// native-log scopes have the same ownership boundary as their child, so
    /// they are joined/closed by name rather than globally.
    async fn stop_restart_targets(&self, plan: &RestartPlan) -> AppResult<()> {
        let names: Vec<&'static str> = [
            (SidecarKind::Jobworkerp, "jobworkerp"),
            (SidecarKind::Memories, "memories"),
            (SidecarKind::Conductor, "conductor"),
            (SidecarKind::IssueTracker, "issue-tracker"),
        ]
        .into_iter()
        .filter_map(|(kind, name)| plan.includes(kind).then_some(name))
        .collect();
        let mut stopped = Vec::new();
        if let Some(procs) = self.state.lock().as_mut() {
            let mut keep = Vec::with_capacity(procs.len());
            for proc in std::mem::take(procs) {
                if names.contains(&proc.name) {
                    stopped.push(proc);
                } else {
                    keep.push(proc);
                }
            }
            *procs = keep;
        }
        let jobworkerp_was_tracked = stopped.iter().any(|process| process.name == "jobworkerp");
        if plan.includes(SidecarKind::IssueTracker) {
            self.mark_issue_tracker_unavailable("Issue Tracker is restarting");
            *self.issue_tracker_port.lock() = None;
            *self.issue_tracker_gateway_control_url.lock() = None;
            *self.issue_tracker_gateway_worker_url.lock() = None;
            self.stop_issue_tracker_maintenance().await;
            self.mark_issue_tracker_unavailable("Issue Tracker is restarting");
        }
        let mut stopped = stopped.into_iter();
        while let Some(mut proc) = stopped.next() {
            let name = proc.name;
            if let Err(error) = self.stop_process(&mut proc).await {
                let mut retained = vec![proc];
                retained.extend(stopped);
                let mut state = self.state.lock();
                if let Some(processes) = state.as_mut() {
                    processes.extend(retained);
                } else {
                    *state = Some(retained);
                }
                return Err(error);
            }
            if self.join_output_tasks_for(Some(name)).await {
                self.close_native_log_dirs(Some(name));
            }
            if name == "jobworkerp" {
                *self.jobworkerp_spawn_observation.lock() = None;
                *self.active_mcp_port.lock() = None;
                self.set_mcp_runtime_state(McpRuntimeStatus::Disabled, None);
                self.clear_active_mcp_secrets();
                self.release_mcp_token_after_stop();
            }
        }
        if plan.includes(SidecarKind::Jobworkerp) && !jobworkerp_was_tracked {
            *self.jobworkerp_spawn_observation.lock() = None;
            *self.active_mcp_port.lock() = None;
            self.set_mcp_runtime_state(McpRuntimeStatus::Disabled, None);
            self.clear_active_mcp_secrets();
            self.release_mcp_token_after_stop();
        }
        Ok(())
    }

    /// Stop the selected children before a caller rolls persisted settings
    /// back after a post-restart operation fails. This keeps replacement
    /// resources (notably the MCP token and vector-store handles) from
    /// surviving into restoration.
    pub(crate) async fn stop_restart_plan(&self, plan: RestartPlan) -> AppResult<()> {
        let _lifecycle_guard = self.lifecycle_lock.lock().await;
        self.stop_restart_targets(&plan).await
    }

    /// Start a local memories child and verify the RPC services consumed by
    /// the host. A vector-dimension mismatch gets exactly one retry with the
    /// vector store disabled; lifecycle cleanup remains the caller's concern.
    async fn start_local_memories(
        &self,
        port: u16,
        jobworkerp_port: u16,
        rust_log: &str,
        embedding_runtime: &crate::commands::embedding_settings::EmbeddingRuntime,
        staged_embedding_workers_yaml: &Path,
        warnings: &mut Vec<SidecarWarning>,
    ) -> AppResult<()> {
        self.spawn_and_register_memories(
            port,
            jobworkerp_port,
            rust_log,
            embedding_runtime,
            staged_embedding_workers_yaml,
            None,
        )?;
        let degraded = retry_degraded_memories_start(
            wait_for_tcp_or_failure(
                ("127.0.0.1", port),
                MEMORIES_START_TIMEOUT,
                Some(&self.startup_failure),
            )
            .await,
            || async {
                // The failed child and its structured failure must be removed
                // before retrying, otherwise the next readiness wait observes
                // stale state instead of the vector-disabled replacement.
                self.kill_memories_child().await?;
                *self.startup_failure.lock() = None;
                self.spawn_and_register_memories(
                    port,
                    jobworkerp_port,
                    rust_log,
                    embedding_runtime,
                    staged_embedding_workers_yaml,
                    Some(false),
                )?;
                wait_for_tcp_or_failure(
                    ("127.0.0.1", port),
                    MEMORIES_START_TIMEOUT,
                    Some(&self.startup_failure),
                )
                .await
            },
        )
        .await?;
        if let Some(info) = degraded {
            warn!(
                expected_dim = info.expected_dim,
                actual_dim = info.actual_dim,
                reason = info.reason,
                "memories vector dimension mismatch; restarting with vector store disabled (degraded)"
            );
            *self.degraded.lock() = Some(info.clone());
            warnings.push(info.into_warning());
        }
        validate_selected_local_memories_services_with(port, |url| async move {
            crate::commands::connection::validate_local_memories_services(&url).await
        })
        .await
    }

    /// Refresh conductor runtime inputs after its listener is ready. Failure
    /// is intentionally advisory: a scheduler refresh must not discard an
    /// otherwise usable sidecar startup or selected restart.
    async fn refresh_periodic_runtime_after_conductor_start(
        &self,
        conductor_url: &str,
        jobworkerp: &crate::commands::connection::GrpcAddress,
        services: &crate::commands::periodic_tasks::PeriodicServices,
        conductor_identity: &str,
        warnings: &mut Vec<SidecarWarning>,
    ) {
        let llm_settings =
            crate::commands::llm_settings::load_llm_settings(&self.config.data.llm_settings_path());
        let periodic_output_language = crate::commands::resolve_periodic_output_language(
            &crate::data::paths::load_app_settings(&self.config.data.app_settings_path()),
        );
        let memories_import_bin = crate::resolve_memories_import_bin_path()
            .unwrap_or_else(|_| PathBuf::from("memories-import"));
        if let Err(error) = crate::commands::periodic_tasks::refresh_lookback_periodic_runtime(
            crate::commands::periodic_tasks::PeriodicRuntimeRefreshContext {
                conductor_url,
                jobworkerp,
                services,
                llm_settings: &llm_settings,
                output_language: &periodic_output_language,
                memories_import_bin: &memories_import_bin,
                seed_marker_path: &self.config.data.periodic_defaults_seed_path(),
                conductor_identity,
            },
        )
        .await
        {
            warn!(error = %error, "periodic scheduler runtime refresh failed (continuing)");
            warnings.push(periodic_runtime_refresh_warning(&error));
            return;
        }
        match crate::commands::periodic_tasks::schedulers_without_job_timeout(conductor_url).await {
            Ok(lacking) if !lacking.is_empty() => {
                warn!(schedulers = ?lacking, "conductor dropped the periodic job timeout");
                warnings.push(conductor_drops_job_timeout_warning(&lacking));
            }
            Ok(_) => {}
            Err(error) => warn!(%error, "could not read back the periodic schedulers"),
        }
    }

    /// Recreate selected children at their previous ports. This deliberately
    /// shares the concrete spawn functions with initial startup; the compact
    /// path below is only responsible for selecting children and preserving
    /// the already-running ones.
    async fn start_restart_targets(
        &self,
        plan: &RestartPlan,
        endpoints: &SidecarEndpoints,
        mut warnings: Vec<SidecarWarning>,
        token_mutation: Option<&mut McpTokenMutationReservation<'_>>,
    ) -> AppResult<SidecarStartReport> {
        let token_rotation = token_mutation.is_some();
        // `restart_plan_with_locked` restarts fully unless jobworkerp is
        // local, and a local jobworkerp keeps conductor local too.
        let jw_port = endpoints
            .jobworkerp_port
            .expect("a selected restart runs only with a local jobworkerp");
        let conductor_port = endpoints
            .conductor_port
            .expect("a local jobworkerp keeps conductor local");
        let issue_tracking_enabled =
            crate::commands::issue_tracker_settings::effective_issue_tracking_enabled(
                &self.config.data,
            );
        if !issue_tracking_enabled {
            self.reset_issue_tracker_when_disabled();
        }
        let rust_log = effective_rust_log();
        let llm_settings =
            crate::commands::llm_settings::load_llm_settings(&self.config.data.llm_settings_path());
        let (model, repo, ctx) = crate::commands::llm_settings::resolve_local_llm_env_triple(
            &llm_settings,
            crate::commands::process_env_lookup,
        );
        let kv = crate::commands::llm_settings::resolve_kv_cache_type_with_env(
            &llm_settings,
            crate::commands::process_env_lookup,
        )
        .runner_value()
        .to_string();
        let (mtp_enabled, mtp_draft) = crate::commands::llm_settings::resolve_local_llm_mtp_env(
            &llm_settings,
            crate::commands::process_env_lookup,
        );
        let embedding_runtime = self.current_embedding_runtime();
        let staged_workers = self.stage_embedding_workers_yaml(&embedding_runtime)?;
        let connection = crate::commands::connection::load_connection_config(
            &self.config.data.connection_config_path(),
        );
        let memories_port =
            select_restart_memories_port(connection.memories.mode, endpoints.memories_port)?;
        let memories_callback = resolve_worker_memories_callback(&connection, memories_port)?;
        let mut replacement_mcp: Option<(bool, u16)> = None;

        if plan.includes(SidecarKind::Jobworkerp) {
            *self.effective_hf_home.lock() = resolve_effective_hf_home(&self.config);
            let settings = crate::commands::mcp_settings::load_mcp_settings(
                &self.config.data.mcp_settings_path(),
            );
            let runtime = crate::commands::mcp_settings::resolve_mcp_runtime(&settings);
            let reservation = match token_mutation {
                Some(mutation) => mutation.promote_to_launch(mcp_auth_enabled(
                    runtime.enabled,
                    runtime.allow_unauthenticated_requests,
                ))?,
                None => self.mcp_launch_reservation(mcp_auth_enabled(
                    runtime.enabled,
                    runtime.allow_unauthenticated_requests,
                ))?,
            };
            let secrets =
                if mcp_auth_enabled(runtime.enabled, runtime.allow_unauthenticated_requests) {
                    Some(self.prepare_mcp_launch_secrets()?)
                } else {
                    None
                };
            let mcp_port = select_restart_mcp_port(endpoints.mcp_server_port)?;
            let child = self.spawn_jobworkerp(
                jw_port,
                mcp_port,
                &runtime,
                secrets.as_ref(),
                &rust_log,
                &llm_settings,
                model.as_deref(),
                repo.as_deref(),
                ctx,
                Some(&kv),
                mtp_enabled,
                mtp_draft.as_deref(),
                issue_tracking_enabled,
            )?;
            self.state
                .lock()
                .as_mut()
                .expect("running sidecar state")
                .push(Process {
                    name: "jobworkerp",
                    child,
                    port: jw_port,
                });
            reservation.commit();
            // The child may already read this file even if a later Memories
            // or worker-registration step fails, so transfer its lifetime
            // to the running sidecar immediately.
            *self.active_mcp_secrets.lock() = secrets;
            wait_for_tcp_or_failure(("127.0.0.1", jw_port), Duration::from_secs(30), None).await?;
            // Worker YAML is expanded by this host process, while the LLM
            // child receives its values at spawn. Mirror the launch-time
            // values before re-registering exactly as initial startup does;
            // otherwise a replacement would silently keep YAML defaults.
            unsafe {
                apply_memories_callback_env(&memories_callback);
                apply_rag_memories_env(&memories_callback);
                apply_external_llm_env(
                    llm_settings.provider_model.as_deref(),
                    llm_settings.base_url.as_deref(),
                );
                apply_local_llm_env(
                    model.as_deref(),
                    repo.as_deref(),
                    ctx,
                    Some(&kv),
                    mtp_enabled,
                    mtp_draft.as_deref(),
                );
                self.mirror_embedding_env(
                    &crate::commands::embedding_settings::resolve_embedding_env_vars_from_runtime(
                        &embedding_runtime,
                    ),
                )?;
            }
            // Register the LLM workers before Memories can dispatch its first
            // workflow. RAG workers and FunctionSets are applied after the
            // current embedding worker name is queried below.
            let server_url = loopback_url(jw_port);
            let handle = wait_for_jobworkerp_grpc_ready(&server_url).await?;
            register_runtime_workers_before_memories(
                &handle,
                &worker_paths_with_issue_chat(
                    &self.config.worker_yaml_paths,
                    issue_tracking_enabled,
                ),
                issue_tracking_enabled,
                &mut warnings,
            )
            .await?;
            crate::sidecar::generation_workers::register_generation_workers(
                &loopback_url(jw_port),
                &self.config.data.log_dir(),
                &mut warnings,
            )
            .await;
            replacement_mcp = Some((runtime.enabled, mcp_port));
        }
        if plan.includes(SidecarKind::Memories)
            && let Some(port) = memories_port
        {
            self.start_local_memories(
                port,
                jw_port,
                &rust_log,
                &embedding_runtime,
                &staged_workers,
                &mut warnings,
            )
            .await?;
        }
        if plan.includes(SidecarKind::Jobworkerp) || plan.includes(SidecarKind::Memories) {
            // Memories owns space identity and the worker name. Query it only
            // after the selected Memories restart (if any) has become ready.
            let memories_url =
                crate::commands::connection::memories_url(&connection, memories_port)?;
            let embedding_space = current_embedding_space(&memories_url).await?;
            let server_url = loopback_url(jw_port);
            let handle = wait_for_jobworkerp_grpc_ready(&server_url).await?;
            let query_embedding = super::query_embedding_worker::prepare_query_embedding(
                &handle,
                &embedding_space,
                &embedding_runtime,
                self.config.workflows_dir.as_deref(),
                &mut warnings,
            )
            .await;
            register_runtime_memories_dependent_workers(
                &handle,
                &worker_paths_with_issue_chat(
                    &self.config.worker_yaml_paths,
                    issue_tracking_enabled,
                ),
                &query_embedding,
                &crate::commands::connection::GrpcAddress::loopback(jw_port),
                issue_tracking_enabled,
                &mut warnings,
            )
            .await?;
            register_runtime_function_sets(
                &handle,
                &self.config.function_set_yaml_paths,
                &self.config.data.root,
                issue_tracking_enabled,
                &mut warnings,
            )
            .await?;

            if let Some((true, mcp_port)) = replacement_mcp {
                let mcp_toolset_error = match validate_mcp_toolset_for_issue_tracking(
                    &handle,
                    issue_tracking_enabled,
                )
                .await
                {
                    Ok(()) => None,
                    Err(error) => {
                        if !issue_tracking_enabled {
                            return Err(error);
                        }
                        let reason = error.to_string();
                        warn!(error = %reason, "MCP toolset validation failed after partial restart");
                        warnings.push(mcp_toolset_unavailable_warning(&reason));
                        Some(reason)
                    }
                };
                let listener_ready = selected_restart_mcp_listener_readiness(
                    token_rotation,
                    wait_for_tcp_or_failure(("127.0.0.1", mcp_port), MCP_HEALTH_TIMEOUT, None)
                        .await,
                    &mut warnings,
                )?;
                let (mcp_runtime_status, mcp_unavailable_reason) =
                    mcp_runtime_readiness(true, listener_ready, mcp_toolset_error);
                *self.active_mcp_port.lock() = mcp_listener_port(
                    true,
                    mcp_runtime_status == McpRuntimeStatus::Ready,
                    mcp_port,
                );
                self.set_mcp_runtime_state(mcp_runtime_status, mcp_unavailable_reason);
            }
        }
        if plan.includes(SidecarKind::Conductor) {
            self.start_local_conductor(
                conductor_port,
                &rust_log,
                &crate::commands::connection::GrpcAddress::loopback(jw_port),
                &crate::commands::periodic_tasks::PeriodicServices::new(
                    &connection,
                    memories_callback.clone(),
                ),
                &mut warnings,
            )
            .await?;
        }
        self.record_pids(&self.config.data.sidecar_pids_path());
        let report = SidecarStartReport {
            endpoints: self.current_endpoints().ok_or_else(|| {
                AppError::Config(
                    "selected sidecar restart did not restore required endpoints".to_string(),
                )
            })?,
            warnings,
        };
        if plan.includes(SidecarKind::Jobworkerp) {
            self.mark_jobworkerp_startup_complete();
        }
        *self.last_report.lock() = Some(report.clone());
        Ok(report)
    }

    /// Restart using the persisted configuration without an intervening
    /// credential mutation.
    pub async fn restart_plan(
        &self,
        plan: RestartPlan,
        warnings: Vec<SidecarWarning>,
    ) -> AppResult<SidecarStartReport> {
        self.restart_plan_with(plan, warnings, || Ok(())).await
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

    /// Run one Memories-owned embedding operation while the resident services
    /// are stopped, then perform the normal startup (including local apply)
    /// without releasing the lifecycle lock between those phases.
    pub(crate) async fn embedding_migration_transaction<T, Q, QFut, F, Fut, P, PFut>(
        &self,
        warnings: Vec<SidecarWarning>,
        quiesce: Q,
        before_start: F,
        post_start: P,
    ) -> AppResult<EmbeddingMaintenanceOutcome<T>>
    where
        T: Clone,
        Q: FnOnce() -> QFut,
        QFut: Future<Output = AppResult<()>>,
        F: FnOnce() -> Fut,
        Fut: Future<Output = AppResult<EmbeddingMaintenanceDecision<T>>>,
        P: FnOnce(T) -> PFut,
        PFut: Future<Output = AppResult<()>>,
    {
        let _lifecycle_guard = self.lifecycle_lock.lock().await;
        if !self.owns_instance_lock() {
            return Err(AppError::AnotherInstanceRunning);
        }
        quiesce().await?;
        self.stop_inner_locked(false).await?;

        let value = match before_start().await? {
            EmbeddingMaintenanceDecision::Start(value) => value,
            EmbeddingMaintenanceDecision::KeepStopped(value) => {
                return Ok(EmbeddingMaintenanceOutcome::Stopped { value });
            }
        };

        let report = match self.start_inner(warnings, None).await {
            Ok(report) => {
                *self.last_start_error.lock() = None;
                *self.last_start_failure.lock() = None;
                report
            }
            Err(error) => {
                if let Err(cleanup_error) = self.stop_inner_locked(false).await {
                    warn!(error = %cleanup_error, "failed to clean up an embedding migration startup failure");
                }
                self.record_full_restart_failure(&error);
                return Err(error);
            }
        };

        if let Err(error) = post_start(value.clone()).await {
            if let Err(cleanup_error) = self.stop_inner_locked(false).await {
                warn!(error = %cleanup_error, "failed to stop sidecars after embedding migration continuation failure");
            }
            self.record_full_restart_failure(&error);
            return Err(error);
        }

        Ok(EmbeddingMaintenanceOutcome::Started { value, report })
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
        if !crate::commands::issue_tracker_settings::effective_issue_tracking_enabled(
            &self.config.data,
        ) {
            self.reset_issue_tracker_when_disabled();
        }
        // Invalidate the snapshot in lockstep with the process list, so a
        // `get_sidecar_status` after stop (or after purge_all_data) can't
        // hand the frontend stale endpoints and promote it back to ready.
        *self.last_report.lock() = None;
        *self.started_topology.lock() = None;
        *self.jobworkerp_spawn_observation.lock() = None;
        *self.last_start_error.lock() = None;
        *self.last_start_failure.lock() = None;
        // No live child ⇒ no injected key env. A subsequent hot-reload gate
        // must not think a stopped child still carries a provider key.
        *self.spawned_external_key_env.lock() = None;
        // No live child ⇒ no MCP server listening; `get_mcp_settings` must
        // report `active_port: None` after a stop.
        *self.active_mcp_port.lock() = None;
        self.set_mcp_runtime_state(McpRuntimeStatus::Disabled, None);
        // No live child ⇒ the degraded gate must not keep refusing
        // dispatches after a stop / purge.
        *self.degraded.lock() = None;
        *self.issue_tracker_port.lock() = None;
        *self.issue_tracker_remote_url.lock() = None;
        *self.issue_tracker_gateway_control_url.lock() = None;
        *self.issue_tracker_gateway_worker_url.lock() = None;
        self.stop_issue_tracker_maintenance().await;
        self.mark_issue_tracker_unavailable("sidecars are stopped");
        let Some(mut procs) = self.state.lock().take() else {
            if self.join_output_tasks().await {
                self.close_native_log_dirs(None);
            } else {
                warn!("sidecar output pumps did not finish; leaving native logs active");
            }
            self.clear_active_mcp_secrets();
            self.release_mcp_token_after_stop();
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
        let mut stopped_count = 0;
        for index in 0..procs.len() {
            if let Err(error) = self.stop_process(&mut procs[index]).await {
                // Keep every child not yet confirmed stopped visible to the
                // lifecycle. In particular, do not clear the PID record or
                // release the instance lock while a child may still be live.
                let remaining = procs.drain(index..).collect();
                *self.state.lock() = Some(remaining);
                return Err(error);
            }
            stopped_count += 1;
        }
        if stopped_count != 0 {
            debug!(stopped_count, "all tracked sidecars confirmed stopped");
        }
        if self.join_output_tasks().await {
            self.close_native_log_dirs(None);
        } else {
            warn!("sidecar output pumps did not finish; leaving native logs active");
        }
        self.clear_active_mcp_secrets();
        self.release_mcp_token_after_stop();
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
    async fn kill_memories_child(&self) -> AppResult<()> {
        let removed = self.state.lock().as_mut().and_then(|procs| {
            procs
                .iter()
                .position(|proc| proc.name == "memories")
                .map(|index| (index, procs.remove(index)))
        });
        if let Some((index, mut process)) = removed
            && let Err(error) = self.stop_process(&mut process).await
        {
            let mut state = self.state.lock();
            if let Some(processes) = state.as_mut() {
                processes.insert(index.min(processes.len()), process);
            } else {
                *state = Some(vec![process]);
            }
            return Err(error);
        }
        if self.join_output_tasks_for(Some("memories")).await {
            self.close_native_log_dirs(Some("memories"));
        } else {
            warn!("memories output pumps did not finish; leaving native logs active");
        }
        Ok(())
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

    fn clear_active_mcp_secrets(&self) {
        // Drop only after children/output pumps stop so no serving child can
        // observe its credential files disappearing mid-request.
        let _ = self.active_mcp_secrets.lock().take();
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
        if !crate::commands::issue_tracker_settings::effective_issue_tracking_enabled(
            &self.config.data,
        ) {
            self.reset_issue_tracker_when_disabled();
        }
        *self.last_report.lock() = None;
        *self.last_start_error.lock() = None;
        *self.last_start_failure.lock() = None;
        // No live child ⇒ no injected key env. A subsequent hot-reload gate
        // must not think a stopped child still carries a provider key.
        *self.spawned_external_key_env.lock() = None;
        // No live child ⇒ no MCP server listening; mirror `stop`.
        *self.active_mcp_port.lock() = None;
        self.set_mcp_runtime_state(McpRuntimeStatus::Disabled, None);
        // Mirror `stop`: clear the degraded gate.
        *self.degraded.lock() = None;
        *self.issue_tracker_port.lock() = None;
        *self.issue_tracker_gateway_control_url.lock() = None;
        *self.issue_tracker_gateway_worker_url.lock() = None;
        self.abort_issue_tracker_maintenance();
        let Some(mut procs) = self.state.lock().take() else {
            self.clear_active_mcp_secrets();
            self.release_mcp_token_after_stop();
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
        self.clear_active_mcp_secrets();
        self.release_mcp_token_after_stop();
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

    /// macOS-only: make the staged plugins dir a dynamic-library search path
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
        let plugins_dir = self.config.data.plugins_dir();
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

    fn prepare_mcp_launch_secrets(&self) -> AppResult<McpLaunchSecrets> {
        let auth_token = crate::commands::mcp_settings::require_mcp_auth_token()?;
        let auth_token_file = crate::commands::mcp_settings::write_mcp_secret_file(
            &self.config.data,
            "auth-",
            &auth_token,
        )?;
        Ok(McpLaunchSecrets { auth_token_file })
    }

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
        mcp_launch_secrets: Option<&McpLaunchSecrets>,
        rust_log: &str,
        llm_settings: &crate::commands::llm_settings::LlmSettings,
        llm_model: Option<&str>,
        llm_hf_repo: Option<&str>,
        llm_ctx_size: Option<u32>,
        llm_kv_cache_type: Option<&str>,
        llm_mtp_enabled: Option<bool>,
        llm_mtp_draft_model: Option<&str>,
        issue_tracking_enabled: bool,
    ) -> AppResult<Child> {
        if !issue_tracking_enabled {
            issue_tracker::clear_issue_tracker_endpoint_env();
        }
        // cwd is pinned to the per-app data dir so `dotenvy::dotenv()` in the
        // child cannot read a surprise `.env` from the parent workspace
        // (which would silently override GRPC_ADDR back to 0.0.0.0:9000).
        let mut cmd = self.base_cmd(
            &self.config.jobworkerp_bin,
            &self.config.data.root,
            rust_log,
        );
        #[cfg(target_os = "linux")]
        let msb_paths = match native_linux_user_home() {
            Ok(native_home) => {
                let paths = jobworkerp_msb_sdk_paths(&native_home);
                if !paths.representative_socket_path_supported {
                    warn!(
                        msb_home = %paths.home.display(),
                        "representative Microsandbox Unix socket path exceeds sockaddr_un capacity; jobworkerp still starts, but sandbox operations may report the SDK path-length error"
                    );
                }
                Some(paths)
            }
            Err(error) => {
                warn!(
                    %error,
                    "could not resolve the native user home for jobworkerp Microsandbox configuration"
                );
                None
            }
        };
        #[cfg(target_os = "linux")]
        apply_jobworkerp_msb_sdk_environment(&mut cmd, msb_paths.as_ref());
        apply_jobworkerp_workflow_environment(&mut cmd);
        if !issue_tracking_enabled {
            for key in issue_tracker::ISSUE_TRACKER_ENDPOINT_ENV_KEYS {
                cmd.env_remove(key);
            }
        }
        cmd.env("STORAGE_TYPE", "Standalone")
            .env("STORAGE_RESTORE_AT_STARTUP", "false")
            .env("GRPC_ADDR", format!("127.0.0.1:{port}"))
            .env("SQLITE_URL", self.config.data.sqlite_url())
            .env(
                "PLUGINS_RUNNER_DIR",
                self.config.data.plugins_dir().to_string_lossy().as_ref(),
            );

        // MCP server env. When disabled this is just
        // `MCP_ENABLED=false` (the long-standing default — jobworkerp boots
        // gRPC-only). When enabled it adds `MCP_SET_NAME=lookback-mcp`
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
        for key in MCP_SECRET_ENV_KEYS {
            cmd.env_remove(key);
        }
        for (key, value) in crate::commands::mcp_settings::mcp_env_vars(mcp_runtime) {
            cmd.env(key, value);
        }
        if mcp_runtime.enabled {
            apply_mcp_launch_auth(
                &mut cmd,
                mcp_auth_enabled(
                    mcp_runtime.enabled,
                    mcp_runtime.allow_unauthenticated_requests,
                ),
                mcp_launch_secrets,
            );
        }

        for (key, value) in jobworkerp_channel_env_vars() {
            cmd.env(key, value);
        }
        for (key, value) in experimental_issues_env_vars(issue_tracking_enabled) {
            cmd.env(key, value);
        }
        cmd
            // Channels in a strict parent-→-child hierarchy:
            //   llm                (1) — LLMPromptRunner jobs (GPU bound).
            //   llm_workflow       (1) — single workflows whose body
            //                            contains an LLM step.
            //   llm_batch          (2) — batch workflows that fan out into
            //                            many `llm_workflow` jobs.
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

        // Capture only non-secret facts relevant to native jobworkerp/SDK
        // diagnosis. HOME is distinct from the NSS-selected MSB_HOME, and no
        // unrelated command environment is copied into this snapshot.
        let child_home = command_child_home(&cmd);
        let applied_sdk_home = command_path_env(&cmd, "MSB_HOME");
        let applied_sdk_config_path = command_path_env(&cmd, "MSB_CONFIG_PATH");
        let sdk_config_source = capture_jobworkerp_sdk_source(applied_sdk_config_path.as_deref());
        let managed_sdk_config_source =
            capture_jobworkerp_sdk_source(Some(Path::new(MANAGED_SDK_CONFIG_PATH)));
        let configured_binary_path =
            native_configured_binary_path(&self.config.jobworkerp_bin, &self.config.data.root);

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
            self.record_jobworkerp_spawn(
                ManagedJobworkerpSpawnSnapshot {
                    generation_id: NEXT_JOBWORKERP_SPAWN_GENERATION.fetch_add(1, Ordering::Relaxed),
                    child_pid: pid,
                    configured_binary_path,
                    actual_running_executable: managed_child_executable_path(pid),
                    applied_sdk_home,
                    applied_sdk_config_path,
                    child_home: child_home.path,
                    child_home_source: child_home.source,
                    effective_sdk_home: None,
                    runtime_observation: ManagedJobworkerpRuntimeObservation::NotObserved,
                    sdk_config_source,
                },
                managed_sdk_config_source,
            );
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
        staged_embedding_workers_yaml: &std::path::Path,
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
        let database_url = self.config.data.memories_sqlite_url();
        apply_memories_child_env(
            &mut cmd,
            self.memories_migration_extra_env(
                &self.config.data,
                embedding_runtime,
                staged_embedding_workers_yaml,
                &database_url,
                auto_embedding_on,
            ),
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
            .env("JOBWORKERP_ADDR", loopback_url(jw_port))
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
            &self.config.data.memories_lancedb_uri(),
        );
        // Prefixes are intentionally inherited by jobworkerp, whose workflow
        // YAML expands them into `embed_text` calls. Remove only the document
        // prefix from memories: its internally chunked embedding path rejects
        // it to protect persisted chunk offsets, while the query prefix is
        // needed for retrieval embeddings.
        strip_memories_document_embedding_prefix(&mut cmd);

        // Point the embedding dispatchers at the staged agent-app YAML
        // (Metal device + unified `memories-mm-embedding` names) instead of
        // memories' compile-time defaults. Staging is required because
        // conditional fields such as `tokenizer_model_id` depend on runtime.
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
        staged_embedding_workers_yaml: &std::path::Path,
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

/// Holds a settings-save token mutation across stopping the old listener and
/// starting its replacement. Promotion transfers the reservation to the
/// replacement launch; otherwise `Drop` makes a failed or disabled restart
/// available for rollback or a later standalone mutation.
struct McpTokenMutationReservation<'a> {
    sidecars: &'a Sidecars,
    active: bool,
}

impl<'a> McpTokenMutationReservation<'a> {
    fn promote_to_launch(&mut self, enabled: bool) -> AppResult<McpLaunchReservation<'a>> {
        if !enabled {
            return Ok(McpLaunchReservation {
                sidecars: self.sidecars,
                active: false,
            });
        }
        self.sidecars
            .mcp_token_state
            .compare_exchange(
                MCP_TOKEN_STATE_MUTATING,
                MCP_TOKEN_STATE_LAUNCH_OR_LIVE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| {
                AppError::Config(
                    "MCP token rotation reservation was lost before listener launch".to_string(),
                )
            })?;
        self.active = false;
        Ok(McpLaunchReservation {
            sidecars: self.sidecars,
            active: true,
        })
    }
}

impl Drop for McpTokenMutationReservation<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.sidecars.mcp_token_state.compare_exchange(
                MCP_TOKEN_STATE_MUTATING,
                MCP_TOKEN_STATE_IDLE,
                Ordering::Release,
                Ordering::Relaxed,
            );
        }
    }
}

/// Releases an MCP token reservation on every failed launch path. A successful
/// launch transfers ownership to the regular stop lifecycle.
struct McpLaunchReservation<'a> {
    sidecars: &'a Sidecars,
    active: bool,
}

impl McpLaunchReservation<'_> {
    fn commit(mut self) {
        self.active = false;
    }
}

impl Drop for McpLaunchReservation<'_> {
    fn drop(&mut self) {
        if self.active {
            self.sidecars.release_mcp_token_after_stop();
        }
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

/// Return the callback values consumed while expanding worker YAML in this
/// process. Keeping this projection pure makes Local/Remote restart behavior
/// testable without mutating process-wide environment state.
fn memories_callback_env_vars(
    callback: &crate::commands::connection::GrpcAddress,
) -> [(&'static str, String); 3] {
    [
        ("MEMORY_GRPC_HOST", callback.host.clone()),
        ("MEMORY_GRPC_PORT", callback.port.to_string()),
        ("MEMORY_GRPC_TLS", callback.tls.to_string()),
    ]
}

/// Mirror the active embedding-upsert callback into THIS process's env so the
/// worker-YAML loader substitutes `MEMORY_GRPC_*` before registering workers.
/// The child process received its own environment at spawn, but registration
/// expands YAML in the Tauri process, so this must run for both full and
/// selected jobworkerp restarts.
///
/// # Safety
///
/// See [`apply_external_llm_env`] — the caller must uphold the same
/// single-thread invariant for process environment mutation.
unsafe fn apply_memories_callback_env(callback: &crate::commands::connection::GrpcAddress) {
    unsafe {
        for (key, value) in memories_callback_env_vars(callback) {
            std::env::set_var(key, value);
        }
    }
}

/// Mirror the active Memories endpoint into `LOOKBACK_RAG_MEMORIES_*` so the
/// worker-YAML loader substitutes the callback defaults for MCP recall. Chat
/// dispatch overrides these values per request, but MCP runs the workflow
/// directly and therefore relies on the registered defaults.
///
/// # Safety
///
/// See [`apply_external_llm_env`] — the caller must uphold the same
/// single-thread invariant for process environment mutation.
unsafe fn apply_rag_memories_env(callback: &crate::commands::connection::GrpcAddress) {
    unsafe {
        std::env::set_var("LOOKBACK_RAG_MEMORIES_HOST", &callback.host);
        std::env::set_var("LOOKBACK_RAG_MEMORIES_PORT", callback.port.to_string());
        std::env::set_var("LOOKBACK_RAG_MEMORIES_TLS", callback.tls.to_string());
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

async fn stop_child(child: &mut Child, name: &str) -> AppResult<()> {
    let pid = child.id();
    debug!(?name, ?pid, "stopping sidecar");

    match child.try_wait() {
        Ok(Some(status)) => {
            info!(?name, ?status, "sidecar already exited");
            return Ok(());
        }
        Ok(None) => {}
        Err(error) => {
            return Err(AppError::Config(format!(
                "check {name} sidecar exit status failed: {error}"
            )));
        }
    }

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
        Ok(Ok(status)) => {
            info!(?name, ?status, "sidecar exited");
            Ok(())
        }
        Ok(Err(error)) => Err(AppError::Config(format!(
            "wait for {name} sidecar exit failed: {error}"
        ))),
        Err(_) => {
            warn!(?name, "sidecar did not exit within 5s; sending SIGKILL");
            if let Err(error) = child.start_kill() {
                // The process may have exited between the grace timeout and
                // SIGKILL. Treat only a confirmed exit as successful cleanup.
                return match child.try_wait() {
                    Ok(Some(status)) => {
                        info!(?name, ?status, "sidecar exited during forced stop");
                        Ok(())
                    }
                    Ok(None) => Err(AppError::Config(format!(
                        "send SIGKILL to {name} sidecar failed: {error}"
                    ))),
                    Err(wait_error) => Err(AppError::Config(format!(
                        "send SIGKILL to {name} sidecar failed: {error}; checking exit status failed: {wait_error}"
                    ))),
                };
            }
            child
                .wait()
                .await
                .map(|status| {
                    info!(?name, ?status, "sidecar exited after SIGKILL");
                })
                .map_err(|error| {
                    AppError::Config(format!("wait for killed {name} sidecar failed: {error}"))
                })
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
            return Err(AppError::SidecarStartupFailed(Box::new(failure)));
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
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn jobworkerp_bounds_run_and_call_tasks_with_a_default_timeout() {
        let mut cmd = Command::new("jobworkerp");
        apply_jobworkerp_workflow_environment(&mut cmd);
        let value = cmd
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("WORKFLOW_TASK_DEFAULT_TIMEOUT_SEC"))
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned());
        assert_eq!(value.as_deref(), Some(JOBWORKERP_TASK_DEFAULT_TIMEOUT_SEC));
    }

    #[test]
    fn pre_memories_worker_apply_defers_rag_tools_but_keeps_llm_workers() {
        let llm_workers = Path::new(env!("CARGO_MANIFEST_DIR")).join("../workers/llm-workers.yaml");

        let filtered = worker_yaml_without_memories_dependent(&llm_workers)
            .unwrap()
            .expect("llm worker manifest contains RAG workers to defer");
        let document: serde_yaml::Value = serde_yaml::from_str(&filtered).unwrap();
        let names: Vec<_> = document["workers"]
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(|worker| worker["name"].as_str())
            .collect();

        assert!(names.contains(&"memories-llm"));
        for name in LOOKBACK_RAG_WORKER_NAMES {
            assert!(!names.contains(&name), "{name} must wait for Memories");
        }
    }

    #[tokio::test]
    async fn cross_thread_resolution_waits_for_memories_and_gets_its_endpoints() {
        let issue_workers = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../workers/lookback-issue-extract-workers.yaml");

        let early = worker_yaml_without_memories_dependent(&issue_workers)
            .unwrap()
            .expect("the Issue manifest holds workers that wait for Memories");
        let early: serde_yaml::Value = serde_yaml::from_str(&early).unwrap();
        let early_names: Vec<_> = early["workers"]
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(|worker| worker["name"].as_str())
            .collect();
        assert!(early_names.contains(&"lookback-issue-resolution-judge"));
        assert!(early_names.contains(&"lookback-cross-thread-issue-resolution-watermark"));
        for name in CROSS_THREAD_RESOLUTION_WORKER_NAMES {
            assert!(
                !early_names.contains(&name),
                "{name} must wait for Memories"
            );
        }

        let rendered = memories_dependent_worker_yaml(
            &issue_workers,
            &MemoriesQueryEmbedding::named("memories-mm-embedding-abc"),
            &crate::commands::connection::GrpcAddress::loopback(41234),
        )
        .await
        .unwrap()
        .expect("cross-thread workers are registered after Memories starts");
        let document: serde_yaml::Value = serde_yaml::from_str(&rendered).unwrap();
        let workers = document["workers"].as_sequence().unwrap();
        let names: Vec<_> = workers
            .iter()
            .filter_map(|worker| worker["name"].as_str())
            .collect();
        assert_eq!(names, CROSS_THREAD_RESOLUTION_WORKER_NAMES);
        let rendered_workflows = serde_yaml::to_string(workers).unwrap();
        assert!(rendered_workflows.contains("memories-mm-embedding-abc"));
        assert!(rendered_workflows.contains("${ 41234 }"));
        assert!(!rendered_workflows.contains("LOOKBACK_JOBWORKERP_GRPC_PORT"));
        assert!(!rendered_workflows.contains("LOOKBACK_MM_EMBEDDING_WORKER"));

        // A remote jobworkerp's own workflows dial it at its address, TLS
        // included.
        let remote = memories_dependent_worker_yaml(
            &issue_workers,
            &MemoriesQueryEmbedding::named("memories-mm-embedding-abc"),
            &crate::commands::connection::GrpcAddress {
                host: "jobworkerp.example".to_string(),
                port: 443,
                tls: true,
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert!(remote.contains("jobworkerp.example"));
        assert!(remote.contains("${ 443 }"));
        assert!(remote.contains("${ true }"));
        assert!(!remote.contains("LOOKBACK_JOBWORKERP_GRPC_TLS"));
    }

    #[tokio::test]
    async fn memories_space_response_resolves_real_rag_file_include_without_env_leaks() {
        use crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceResponse;

        let llm_workers = Path::new(env!("CARGO_MANIFEST_DIR")).join("../workers/llm-workers.yaml");
        let response = GetEmbeddingSpaceResponse {
            mm_embedding_worker_name: "memories-mm-embedding-0123456789abcdef".into(),
            registration_state: 2,
            ..Default::default()
        };
        let worker_name = mm_embedding_worker_name_from_response(&response).unwrap();

        let rendered = memories_dependent_worker_yaml(
            &llm_workers,
            &MemoriesQueryEmbedding::named(&worker_name),
            &crate::commands::connection::GrpcAddress::loopback(9000),
        )
        .await
        .unwrap()
        .expect("RAG workers are extracted from the real llm-workers.yaml");
        let document: serde_yaml::Value = serde_yaml::from_str(&rendered).unwrap();
        let recall = document["workers"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|worker| worker["name"].as_str() == Some("lookback_recall"))
            .unwrap();
        let workflow_text = recall["settings"]["workflow_data"].as_str().unwrap();
        let workflow: serde_yaml::Value = serde_yaml::from_str(workflow_text).unwrap();
        let workflow_config = workflow_text
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(
            workflow["do"][0]["embedQuery"]["run"]["worker"]["name"].as_str(),
            Some(worker_name.as_str())
        );
        assert!(!workflow_config.contains("%{"));
    }

    #[tokio::test]
    async fn memories_dependent_workers_embed_queries_with_the_space_query_prefix() {
        let llm_workers = Path::new(env!("CARGO_MANIFEST_DIR")).join("../workers/llm-workers.yaml");
        let embed_args = |rendered: &str| {
            let document: serde_yaml::Value = serde_yaml::from_str(rendered).unwrap();
            let recall = document["workers"]
                .as_sequence()
                .unwrap()
                .iter()
                .find(|worker| worker["name"].as_str() == Some("lookback_recall"))
                .unwrap()
                .clone();
            let workflow: serde_yaml::Value =
                serde_yaml::from_str(recall["settings"]["workflow_data"].as_str().unwrap())
                    .unwrap();
            workflow["do"][0]["embedQuery"]["run"]["worker"]["arguments"]["prefix"].clone()
        };

        let with_prefix = memories_dependent_worker_yaml(
            &llm_workers,
            &MemoriesQueryEmbedding {
                worker_name: "memories-mm-embedding-0123456789abcdef".into(),
                query_prefix: Some("検索クエリ: ".into()),
            },
            &crate::commands::connection::GrpcAddress::loopback(9000),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(embed_args(&with_prefix).as_str(), Some("検索クエリ: "));

        // A known space without a prefix must not inherit one from the env.
        let without_prefix = memories_dependent_worker_yaml(
            &llm_workers,
            &MemoriesQueryEmbedding {
                worker_name: "memories-mm-embedding-0123456789abcdef".into(),
                query_prefix: Some(String::new()),
            },
            &crate::commands::connection::GrpcAddress::loopback(9000),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(embed_args(&without_prefix).as_str(), Some(""));
    }

    #[test]
    fn invalid_memories_embedding_worker_response_is_rejected() {
        use crate::grpc::proto::llm_memory::service::GetEmbeddingSpaceResponse;

        let empty_name = GetEmbeddingSpaceResponse {
            registration_state: 1,
            ..Default::default()
        };
        assert!(mm_embedding_worker_name_from_response(&empty_name).is_err());

        let unknown_state = GetEmbeddingSpaceResponse {
            mm_embedding_worker_name: "worker-from-memories".into(),
            registration_state: 9,
            ..Default::default()
        };
        assert!(mm_embedding_worker_name_from_response(&unknown_state).is_err());
    }

    #[tokio::test]
    async fn runtime_rag_extraction_registers_all_rag_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("workers.yaml");
        std::fs::write(
            &manifest,
            "workers:\n  - name: lookback_recall\n  - name: lookback_get_context\n  - name: lookback_get_reflections\n",
        ).unwrap();
        let deferred = worker_yaml_without_memories_dependent(&manifest)
            .unwrap()
            .unwrap();
        let deferred: serde_yaml::Value = serde_yaml::from_str(&deferred).unwrap();
        assert!(deferred["workers"].as_sequence().unwrap().is_empty());
        let rendered = memories_dependent_worker_yaml(
            &manifest,
            &MemoriesQueryEmbedding::named("memories-mm-embedding-current"),
            &crate::commands::connection::GrpcAddress::loopback(9000),
        )
        .await
        .unwrap()
        .expect("all RAG workers must be registered after Memories starts");
        let document: serde_yaml::Value = serde_yaml::from_str(&rendered).unwrap();
        let names: Vec<_> = document["workers"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|worker| worker["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, LOOKBACK_RAG_WORKER_NAMES);
    }

    #[tokio::test]
    async fn runtime_rag_extraction_skips_manifest_without_rag() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = temp.path().join("workers.yaml");
        for yaml in ["workers: []\n", "workers:\n  - name: memories-llm\n"] {
            std::fs::write(&manifest, yaml).unwrap();
            assert!(
                worker_yaml_without_memories_dependent(&manifest)
                    .unwrap()
                    .is_none()
            );
            assert!(
                memories_dependent_worker_yaml(
                    &manifest,
                    &MemoriesQueryEmbedding::named("memories-mm-embedding-current"),
                    &crate::commands::connection::GrpcAddress::loopback(9000),
                )
                .await
                .unwrap()
                .is_none()
            );
        }
    }

    async fn real_mcp_tool_names(port: u16) -> Vec<String> {
        let url = format!("http://127.0.0.1:{port}/mcp");
        wait_for_tcp_or_failure(("127.0.0.1", port), Duration::from_secs(10), None)
            .await
            .unwrap();
        for (id, method, params) in [
            (
                1,
                "initialize",
                serde_json::json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"registration-test","version":"1"}}),
            ),
            (2, "tools/list", serde_json::json!({})),
        ] {
            let request =
                serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
            let response = Command::new("curl")
                .args([
                    "--silent",
                    "--show-error",
                    "--fail-with-body",
                    "--max-time",
                    "5",
                ])
                .args([
                    "-H",
                    "Accept: application/json, text/event-stream",
                    "-H",
                    "Content-Type: application/json",
                ])
                .args(["--data", &request.to_string(), &url])
                .output()
                .await
                .unwrap();
            let body = String::from_utf8(response.stdout).unwrap();
            assert!(
                response.status.success(),
                "MCP {method}: {body} {}",
                String::from_utf8_lossy(&response.stderr)
            );
            let payload = body
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .next_back()
                .unwrap_or(&body);
            let value: serde_json::Value = serde_json::from_str(payload).unwrap();
            assert!(value.get("error").is_none(), "MCP {method}: {value}");
            if method == "tools/list" {
                return value["result"]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tool| tool["name"].as_str().unwrap().to_owned())
                    .collect();
            }
        }
        unreachable!()
    }

    #[tokio::test]
    #[ignore = "requires LOOKBACK_JOBWORKERP_BIN pointing to a built all-in-one"]
    async fn real_enabled_registration_accepts_grpc_issue_methods() {
        let root = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(root.path());
        data.ensure().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut child = Command::new(std::env::var("LOOKBACK_JOBWORKERP_BIN").unwrap())
            .current_dir(root.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("STORAGE_TYPE", "Standalone")
            .env("STORAGE_RESTORE_AT_STARTUP", "false")
            .env("GRPC_ADDR", address.to_string())
            .env("SQLITE_URL", data.sqlite_url())
            .env("MCP_ENABLED", "false")
            .env("AG_UI_ENABLED", "false")
            .env("WORKER_INSTANCE_ENABLED", "false")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let handle = wait_for_jobworkerp_grpc_ready(&format!("http://{address}"))
            .await
            .unwrap();
        let memory_names = &MCP_EXPECTED_TOOL_NAMES[..3];
        let mut yaml = String::from("workers:\n");
        for name in memory_names {
            yaml.push_str(&format!("  - name: {name}\n    runner: COMMAND\n"));
        }
        for (index, name) in issue_tool_registration::ISSUE_WORKER_NAMES
            .iter()
            .enumerate()
        {
            if !(10..14).contains(&index) {
                yaml.push_str(&format!(
                    "  - name: {name}\n    runner: GRPC\n    settings:\n      host: 127.0.0.1\n      port: 9\n      tls: false\n      use_reflection: false\n      method: fixture.IssueService/Get\n      as_json: true\n"
                ));
            } else {
                yaml.push_str(&format!("  - name: {name}\n    runner: COMMAND\n"));
            }
        }
        let workers = root.path().join("workers.yaml");
        std::fs::write(&workers, yaml).unwrap();
        handle.register_workers_from_yaml(&workers).await.unwrap();
        let source = crate::data::paths::function_sets_yaml().unwrap();
        let legacy = root.path().join("legacy-function-sets.yaml");
        let legacy_yaml = std::fs::read_to_string(&source).unwrap()
            .replace("      - type: WORKER\n        name: lookback_issue_agent_plan\n        using: unary\n", "")
            .replace("      - type: WORKER\n        name: lookback_issue_agent_get\n        using: unary\n", "");
        std::fs::write(&legacy, legacy_yaml).unwrap();
        handle
            .register_function_sets_from_yaml(&legacy)
            .await
            .unwrap();
        for set in ["lookback-chat", "lookback-mcp"] {
            let tools = handle.fetch_function_set_as_tools_json(set).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Vec<serde_json::Value>>(&tools)
                    .unwrap()
                    .len(),
                8
            );
        }
        let manifest =
            issue_tool_registration::stage_function_set_manifest(&source, root.path(), true)
                .unwrap();
        handle
            .register_function_sets_from_yaml(&manifest)
            .await
            .unwrap();
        let database = rusqlite::Connection::open(data.sqlite_path()).unwrap();
        for name in ["lookback-chat", "lookback-mcp"] {
            let targets: i64 = database.query_row(
                "SELECT COUNT(*) FROM function_set_target t JOIN function_set s ON s.id = t.set_id WHERE s.name = ?1",
                [name], |row| row.get(0),
            ).unwrap();
            assert_eq!(targets, 10, "updated {name} targets must be persisted");
        }
        let result = verify_enabled_issue_registration(&handle).await;
        let tools = handle
            .fetch_function_set_as_tools_json("lookback-chat")
            .await
            .unwrap();
        let mcp_tools = handle
            .fetch_function_set_as_tools_json("lookback-mcp")
            .await
            .unwrap();
        let fresh = crate::jobworkerp::JobworkerpHandle::connect(&format!("http://{address}"))
            .await
            .unwrap();
        let fresh_result = verify_enabled_issue_registration(&fresh).await;
        let _ = child.kill().await;
        let _ = child.wait().await;
        let names: Vec<String> = serde_json::from_str::<Vec<serde_json::Value>>(&tools)
            .unwrap()
            .into_iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
            .collect();
        assert!(
            result.is_ok(),
            "{result:?}; fresh client: {fresh_result:?}; resolved tools: {names:?}"
        );
        assert!(fresh_result.is_ok(), "{fresh_result:?}");
        validate_mcp_toolset_json_for_issue_tracking(&mcp_tools, true).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires LOOKBACK_JOBWORKERP_BIN pointing to a built all-in-one and curl"]
    async fn real_disabled_registration_reconciles_persistent_issue_workers() {
        use std::process::Stdio;

        let root = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(root.path());
        data.ensure().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mcp_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let spawn = || {
            Command::new(std::env::var("LOOKBACK_JOBWORKERP_BIN").unwrap())
                .current_dir(root.path())
                .env("STORAGE_TYPE", "Standalone")
                .env("STORAGE_RESTORE_AT_STARTUP", "false")
                .env("GRPC_ADDR", format!("127.0.0.1:{port}"))
                .env("SQLITE_URL", data.sqlite_url())
                .env("MCP_ENABLED", "true")
                .env("MCP_ADDR", format!("127.0.0.1:{mcp_port}"))
                .env("MCP_SET_NAME", "lookback-mcp")
                .env("MCP_AUTH_ENABLED", "false")
                .env("WORKER_CHANNELS", "default,issue_tracker")
                .env("WORKER_CHANNEL_CONCURRENCIES", "2,2")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut child = spawn();
        let url = format!("http://127.0.0.1:{port}");
        let handle = wait_for_jobworkerp_grpc_ready(&url).await.unwrap();
        assert!(
            register_runtime_tools(&handle, &[], &[], root.path(), false, &mut Vec::new())
                .await
                .is_err()
        );
        let all_workers = root.path().join("all-workers.yaml");
        let memory_workers = root.path().join("memory-workers.yaml");
        let memory_names = [
            "lookback_recall",
            "lookback_get_context",
            "lookback_get_reflections",
            "unrelated_worker",
        ];
        let memory_yaml = format!(
            "workers:\n{}",
            memory_names
                .iter()
                .map(|name| format!("  - name: {name}\n    runner: COMMAND\n"))
                .collect::<String>()
        );
        std::fs::write(&memory_workers, &memory_yaml).unwrap();
        let all_yaml = format!(
            "{memory_yaml}{}",
            issue_tool_registration::ISSUE_WORKER_NAMES
                .iter()
                .map(|name| format!("  - name: {name}\n    runner: COMMAND\n"))
                .collect::<String>()
        );
        std::fs::write(&all_workers, all_yaml).unwrap();
        let canonical = root.path().join("function-sets.yaml");
        std::fs::write(
            &canonical,
            std::fs::read_to_string(crate::data::paths::function_sets_yaml().unwrap())
                .unwrap()
                .replace("using: unary", "using: run"),
        )
        .unwrap();
        register_runtime_tools(
            &handle,
            std::slice::from_ref(&memory_workers),
            std::slice::from_ref(&canonical),
            root.path(),
            false,
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(real_mcp_tool_names(mcp_port).await.len(), 3);
        handle
            .register_workers_from_yaml(&all_workers)
            .await
            .unwrap();
        let enabled_manifest =
            issue_tool_registration::stage_function_set_manifest(&canonical, root.path(), true)
                .unwrap();
        handle
            .register_function_sets_from_yaml(&enabled_manifest)
            .await
            .unwrap();
        for set in ["lookback-chat", "lookback-mcp"] {
            assert_eq!(
                serde_json::from_str::<Vec<serde_json::Value>>(
                    &handle.fetch_function_set_as_tools_json(set).await.unwrap()
                )
                .unwrap()
                .len(),
                MCP_EXPECTED_TOOL_NAMES.len()
            );
        }
        assert_eq!(real_mcp_tool_names(mcp_port).await.len(), 10);
        handle
            .delete_worker_by_name_if_present(issue_tool_registration::ISSUE_WORKER_NAMES[10])
            .await
            .unwrap();
        assert!(verify_enabled_issue_registration(&handle).await.is_err());
        handle
            .register_workers_from_yaml(&all_workers)
            .await
            .unwrap();
        verify_enabled_issue_registration(&handle).await.unwrap();
        let incomplete_set = root.path().join("incomplete-function-sets.yaml");
        std::fs::write(
            &incomplete_set,
            std::fs::read_to_string(&canonical).unwrap().replace(
                "      - type: WORKER\n        name: lookback_issue_cancel\n        using: run\n",
                "",
            ),
        )
        .unwrap();
        handle
            .register_function_sets_from_yaml(&incomplete_set)
            .await
            .unwrap();
        assert!(verify_enabled_issue_registration(&handle).await.is_err());
        handle
            .register_function_sets_from_yaml(&enabled_manifest)
            .await
            .unwrap();
        verify_enabled_issue_registration(&handle).await.unwrap();
        let invalid_manifest = root.path().join("invalid-function-sets.yaml");
        std::fs::write(&invalid_manifest, "function_sets: []").unwrap();
        assert!(
            register_runtime_tools(
                &handle,
                std::slice::from_ref(&memory_workers),
                &[invalid_manifest],
                root.path(),
                false,
                &mut Vec::new(),
            )
            .await
            .is_err()
        );
        assert!(
            handle
                .worker_id_by_name(issue_tool_registration::ISSUE_WORKER_NAMES[0])
                .await
                .unwrap()
                .is_some()
        );
        let mut warnings = Vec::new();
        register_runtime_tools(
            &handle,
            std::slice::from_ref(&memory_workers),
            std::slice::from_ref(&canonical),
            root.path(),
            false,
            &mut warnings,
        )
        .await
        .unwrap();
        register_runtime_tools(
            &handle,
            std::slice::from_ref(&memory_workers),
            std::slice::from_ref(&canonical),
            root.path(),
            false,
            &mut warnings,
        )
        .await
        .unwrap();
        for name in issue_tool_registration::ISSUE_WORKER_NAMES {
            assert!(
                handle.worker_id_by_name(name).await.unwrap().is_none(),
                "{name}"
            );
        }
        assert!(
            handle
                .worker_id_by_name("unrelated_worker")
                .await
                .unwrap()
                .is_some()
        );
        for set in ["lookback-chat", "lookback-mcp"] {
            let tools = handle.fetch_function_set_as_tools_json(set).await.unwrap();
            validate_mcp_toolset_json_for_issue_tracking(&tools, false).unwrap();
            let ids = handle.function_set_worker_ids_by_name(set).await.unwrap();
            let expected = futures::future::join_all(
                [
                    "lookback_recall",
                    "lookback_get_context",
                    "lookback_get_reflections",
                ]
                .iter()
                .map(|name| handle.worker_id_by_name(name)),
            )
            .await
            .into_iter()
            .map(|id| id.unwrap().unwrap())
            .collect::<Vec<_>>();
            assert_eq!(ids, expected, "{set} persisted target IDs");
        }
        assert_eq!(real_mcp_tool_names(mcp_port).await.len(), 3);
        handle
            .register_workers_from_yaml(&all_workers)
            .await
            .unwrap();
        let enabled_manifest =
            issue_tool_registration::stage_function_set_manifest(&canonical, root.path(), true)
                .unwrap();
        handle
            .register_function_sets_from_yaml(&enabled_manifest)
            .await
            .unwrap();
        let _ = child.kill().await;
        let _ = child.wait().await;
        drop(handle);
        let mut child = spawn();
        let handle = wait_for_jobworkerp_grpc_ready(&url).await.unwrap();
        for name in issue_tool_registration::ISSUE_WORKER_NAMES {
            assert!(
                handle.worker_id_by_name(name).await.unwrap().is_some(),
                "{name} persisted from ON before OFF startup"
            );
        }
        register_runtime_tools(
            &handle,
            std::slice::from_ref(&memory_workers),
            std::slice::from_ref(&canonical),
            root.path(),
            false,
            &mut warnings,
        )
        .await
        .unwrap();
        for name in issue_tool_registration::ISSUE_WORKER_NAMES {
            assert!(
                handle.worker_id_by_name(name).await.unwrap().is_none(),
                "{name} removed after restart"
            );
            assert!(
                handle
                    .dispatch_unary(name, serde_json::json!({}), Some("run"))
                    .await
                    .is_err(),
                "{name} must reject direct calls"
            );
        }
        for set in ["lookback-chat", "lookback-mcp"] {
            validate_mcp_toolset_json_for_issue_tracking(
                &handle.fetch_function_set_as_tools_json(set).await.unwrap(),
                false,
            )
            .unwrap();
        }
        assert_eq!(real_mcp_tool_names(mcp_port).await.len(), 3);
        handle
            .register_workers_from_yaml(&all_workers)
            .await
            .unwrap();
        let enabled_manifest =
            issue_tool_registration::stage_function_set_manifest(&canonical, root.path(), true)
                .unwrap();
        handle
            .register_function_sets_from_yaml(&enabled_manifest)
            .await
            .unwrap();
        for name in issue_tool_registration::ISSUE_WORKER_NAMES {
            assert!(
                handle.worker_id_by_name(name).await.unwrap().is_some(),
                "{name} restored"
            );
        }
        for set in ["lookback-chat", "lookback-mcp"] {
            assert_eq!(
                serde_json::from_str::<Vec<serde_json::Value>>(
                    &handle.fetch_function_set_as_tools_json(set).await.unwrap()
                )
                .unwrap()
                .len(),
                MCP_EXPECTED_TOOL_NAMES.len()
            );
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    // Process environment is shared across Rust tests, so tests that change
    // timezone-related variables must not run concurrently.
    static TIMEZONE_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn restart_plan_maps_each_settings_dependency_to_its_sidecars() {
        let jobworkerp = RestartPlan::jobworkerp_only();
        assert!(jobworkerp.includes(SidecarKind::Jobworkerp));
        assert!(!jobworkerp.includes(SidecarKind::Memories));
        assert!(!jobworkerp.includes(SidecarKind::Conductor));

        let embedding = RestartPlan::embedding();
        assert!(embedding.includes(SidecarKind::Jobworkerp));
        assert!(embedding.includes(SidecarKind::Memories));
        assert!(!embedding.includes(SidecarKind::Conductor));

        let timezone = RestartPlan::timezone();
        assert!(timezone.includes(SidecarKind::Jobworkerp));
        assert!(timezone.includes(SidecarKind::Conductor));
        assert!(!timezone.includes(SidecarKind::Memories));
    }

    #[test]
    fn restart_plan_unions_a_batch_once_and_preserves_hot_reload_noop() {
        let embedding = RestartPlan::embedding();
        let timezone = RestartPlan::timezone();
        let batch = RestartPlan::jobworkerp_only()
            .union(&embedding)
            .union(&timezone);
        assert!(batch.includes(SidecarKind::Jobworkerp));
        assert!(batch.includes(SidecarKind::Memories));
        assert!(batch.includes(SidecarKind::Conductor));
        assert!(!batch.includes(SidecarKind::IssueTracker));
        assert!(RestartPlan::empty().is_empty());
    }

    #[test]
    fn successful_embedding_restart_clears_degraded_state() {
        let sidecars = test_sidecars();
        sidecars.set_degraded_for_test(Some(DegradedInfo {
            reason: "embedding_dimension_mismatch",
            expected_dim: 2048,
            actual_dim: 768,
        }));

        sidecars.reset_selected_restart_attempt_state(&RestartPlan::embedding());
        sidecars.complete_successful_selected_restart();

        assert!(
            sidecars.degraded().is_none(),
            "a healthy memories replacement must reopen embedding-dependent operations"
        );
    }

    #[test]
    fn successful_non_memories_restart_preserves_degraded_state() {
        let sidecars = test_sidecars();
        sidecars.set_degraded_for_test(Some(DegradedInfo {
            reason: "embedding_dimension_mismatch",
            expected_dim: 2048,
            actual_dim: 768,
        }));

        sidecars.reset_selected_restart_attempt_state(&RestartPlan::jobworkerp_only());
        sidecars.complete_successful_selected_restart();

        assert!(
            sidecars.degraded().is_some(),
            "a restart that leaves memories running cannot repair its vector store"
        );
    }

    #[test]
    fn successful_degraded_memories_retry_preserves_its_new_gate() {
        let sidecars = test_sidecars();
        sidecars.set_degraded_for_test(Some(DegradedInfo {
            reason: "embedding_dimension_mismatch",
            expected_dim: 2048,
            actual_dim: 768,
        }));

        sidecars.reset_selected_restart_attempt_state(&RestartPlan::embedding());
        sidecars.set_degraded_for_test(Some(DegradedInfo {
            reason: "lancedb_schema_mismatch",
            expected_dim: 1024,
            actual_dim: 512,
        }));
        sidecars.complete_successful_selected_restart();

        assert_eq!(
            sidecars.degraded(),
            Some(DegradedInfo {
                reason: "lancedb_schema_mismatch",
                expected_dim: 1024,
                actual_dim: 512,
            }),
            "the retry result must keep embedding operations gated"
        );
    }

    #[test]
    fn successful_selected_restart_clears_stale_failure_snapshots() {
        let sidecars = test_sidecars();
        *sidecars.last_start_error.lock() = Some("previous replacement failed".to_string());
        *sidecars.last_start_failure.lock() =
            Some(crate::sidecar::startup_error::SidecarErrorPayload::Raw {
                message: "previous replacement failed".to_string(),
            });

        sidecars.complete_successful_selected_restart();

        assert!(sidecars.last_start_error().is_none());
        assert!(sidecars.last_start_failure().is_none());
    }

    #[tokio::test]
    async fn failed_jobworkerp_restart_releases_token_mutation_before_rollback() {
        let sidecars = test_sidecars();
        sidecars
            .reserve_mcp_token_for_launch(true)
            .expect("simulate the replacement listener retaining the rotated token");
        assert!(
            !sidecars.mcp_token_mutation_allowed(),
            "a partially started listener must block credential mutation"
        );

        sidecars
            .cleanup_failed_restart_targets(&RestartPlan::jobworkerp_only())
            .await
            .unwrap();

        assert!(
            sidecars.mcp_token_mutation_allowed(),
            "rollback must be able to restore the old token before restarting jobworkerp"
        );
        sidecars
            .with_mcp_token_mutation(|| Ok(()))
            .expect("the old token restoration mutation must acquire the released reservation");
    }

    #[test]
    fn rollback_restart_reuses_the_last_known_endpoints_after_partial_cleanup() {
        let sidecars = test_sidecars();
        let expected = SidecarEndpoints {
            jobworkerp_port: Some(39_100),
            issue_tracker_port: None,
            memories_port: Some(39_101),
            conductor_port: Some(39_102),
            mcp_server_port: Some(39_103),
        };
        *sidecars.last_report.lock() = Some(SidecarStartReport {
            endpoints: expected.clone(),
            warnings: Vec::new(),
        });
        *sidecars.state.lock() = Some(Vec::new());

        let recovered = sidecars
            .restart_endpoints()
            .expect("cleanup must retain the old endpoints for rollback");
        assert_eq!(recovered.jobworkerp_port, expected.jobworkerp_port);
        assert_eq!(recovered.memories_port, expected.memories_port);
        assert_eq!(recovered.conductor_port, expected.conductor_port);
        assert_eq!(recovered.mcp_server_port, expected.mcp_server_port);
    }

    #[test]
    fn restart_mcp_port_preserves_the_existing_listener_port_without_reselecting() {
        assert_eq!(
            select_restart_mcp_port_with(Some(39_123), |_| {
                panic!("a live MCP listener port must be preserved")
            })
            .expect("existing MCP port"),
            39_123
        );
    }

    #[test]
    fn restart_mcp_port_uses_the_default_port_picker_during_enablement() {
        let default_port = crate::commands::mcp_settings::MCP_DEFAULT_PORT;
        let selected = select_restart_mcp_port_with(None, |preferred| {
            assert_eq!(preferred, default_port);
            Ok(49_152)
        })
        .expect("fallback MCP port");

        assert_eq!(selected, 49_152);
    }

    #[test]
    fn a_remote_jobworkerp_runs_with_no_local_jobworkerp_child() {
        let sidecars = test_sidecars();
        *sidecars.state.lock() = Some(Vec::new());
        // Without a recorded topology everything is expected locally.
        assert!(sidecars.current_endpoints().is_none());

        *sidecars.started_topology.lock() = Some(LocalTopology {
            jobworkerp: false,
            memories: false,
            conductor: false,
        });
        let endpoints = sidecars
            .current_endpoints()
            .expect("remote services are running");
        assert_eq!(endpoints.jobworkerp_port, None);
        assert_eq!(endpoints.conductor_port, None);
        assert_eq!(endpoints.jobworkerp_url(), None);

        // A local conductor beside a remote jobworkerp must still be running.
        *sidecars.started_topology.lock() = Some(LocalTopology {
            jobworkerp: false,
            memories: false,
            conductor: true,
        });
        assert!(sidecars.current_endpoints().is_none());
    }

    #[test]
    fn local_topology_follows_each_service_setting() {
        let connection = crate::commands::connection::ConnectionConfig {
            memories: crate::commands::connection::EndpointConfig::remote("https://m:9000"),
            ..Default::default()
        };
        assert_eq!(
            LocalTopology::of(&connection),
            LocalTopology {
                jobworkerp: true,
                memories: false,
                conductor: true,
            }
        );
        assert_eq!(
            LocalTopology::of(&crate::commands::connection::ConnectionConfig::default()),
            LocalTopology::ALL_LOCAL
        );
    }

    #[test]
    fn connection_restart_uses_local_callback_for_worker_env() {
        let connection = crate::commands::connection::ConnectionConfig {
            jobworkerp: crate::commands::connection::EndpointConfig::local_with_draft(
                "https://old-jobworkerp.example.test",
            ),
            memories: crate::commands::connection::EndpointConfig::local_with_draft(
                "https://old-memories.example.test:9443",
            ),
            ..Default::default()
        };

        let callback = resolve_worker_memories_callback(&connection, Some(39_101))
            .expect("a local restart has a live memories port");
        let env = memories_callback_env_vars(&callback);

        assert_eq!(callback.host, "127.0.0.1");
        assert_eq!(callback.port, 39_101);
        assert!(!callback.tls);
        assert_eq!(
            env,
            [
                ("MEMORY_GRPC_HOST", "127.0.0.1".to_string()),
                ("MEMORY_GRPC_PORT", "39101".to_string()),
                ("MEMORY_GRPC_TLS", "false".to_string()),
            ]
        );
    }

    #[test]
    fn connection_restart_replaces_remote_callback_after_memories_url_change() {
        let old_connection = crate::commands::connection::ConnectionConfig::remote_pair(
            "https://jobworkerp.example.test",
            "https://old-memories.example.test:9443",
        );
        let new_connection = crate::commands::connection::ConnectionConfig {
            memories: crate::commands::connection::EndpointConfig::remote(
                "http://new-memories.example.test:8111",
            ),
            ..old_connection.clone()
        };

        let old_callback = resolve_worker_memories_callback(&old_connection, None)
            .expect("the old remote memories URL is valid");
        let new_callback = resolve_worker_memories_callback(&new_connection, None)
            .expect("the replacement remote memories URL is valid");

        assert_eq!(
            memories_callback_env_vars(&new_callback),
            [
                ("MEMORY_GRPC_HOST", "new-memories.example.test".to_string()),
                ("MEMORY_GRPC_PORT", "8111".to_string()),
                ("MEMORY_GRPC_TLS", "false".to_string()),
            ]
        );
        assert_ne!(new_callback, old_callback);
    }

    #[test]
    fn remote_to_local_restart_selects_a_new_memories_port_when_none_exists() {
        let port = select_restart_memories_port_with(
            crate::commands::connection::ConnectionMode::Local,
            None,
            |preferred| {
                assert_eq!(preferred, 9010);
                Ok(49_153)
            },
        )
        .expect("a Remote-to-Local restart chooses a local memories port");

        assert_eq!(port, Some(49_153));
    }

    #[test]
    fn connection_restart_keeps_issue_tracker_outside_the_plan() {
        let connection = RestartPlan::connection();
        assert!(connection.includes(SidecarKind::Jobworkerp));
        assert!(connection.includes(SidecarKind::Memories));
        assert!(connection.includes(SidecarKind::Conductor));
        assert!(!connection.includes(SidecarKind::IssueTracker));
        assert!(RestartPlan::issue_tracker().includes(SidecarKind::IssueTracker));
    }

    #[tokio::test]
    async fn selected_restart_local_memories_requires_service_contract_validation() {
        let validated_url = Arc::new(Mutex::new(None));
        let recorded_url = Arc::clone(&validated_url);

        validate_selected_local_memories_services_with(39_101, move |url| {
            let recorded_url = Arc::clone(&recorded_url);
            async move {
                *recorded_url.lock() = Some(url);
                Ok(())
            }
        })
        .await
        .expect("a compatible local memories service passes selected-restart validation");

        assert_eq!(
            validated_url.lock().as_deref(),
            Some("http://127.0.0.1:39101")
        );
    }

    #[tokio::test]
    async fn selected_restart_local_memories_propagates_service_contract_failure() {
        let error = validate_selected_local_memories_services_with(39_101, |_| async {
            Err(AppError::Config(
                "missing local memories service contract".to_string(),
            ))
        })
        .await
        .expect_err("an incompatible memories service must fail the selected restart");

        assert!(
            matches!(error, AppError::Config(ref message) if message == "missing local memories service contract"),
            "unexpected error: {error:?}"
        );
    }

    #[tokio::test]
    async fn full_restart_reports_missing_target_when_sidecars_are_stopped() {
        let sidecars = test_sidecars();

        let error = sidecars
            .full_restart_with(Vec::new(), false, || Ok(()))
            .await
            .expect_err("a full restart must not start an unowned data root");

        assert!(
            matches!(error, AppError::Config(ref message) if message == "sidecars are not running; no restart target exists")
        );
    }

    #[tokio::test]
    async fn full_restart_cleans_partial_children_and_records_start_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        data.ensure().unwrap();
        let sidecars = test_sidecars_with_data(data.clone());
        let outcome = reaper::reap_recorded(&data.sidecar_pids_path(), &data.sidecar_lock_path());
        *sidecars.instance_lock.lock() = outcome.lock;
        sidecars.fail_next_start_after_jobworkerp_for_test();

        let error = sidecars
            .full_restart_with(Vec::new(), false, || Ok(()))
            .await
            .expect_err("the injected startup failure must be returned");

        assert!(error.to_string().contains("injected startup failure"));
        assert!(
            sidecars.state.lock().is_none(),
            "partial children must be stopped"
        );
        assert!(sidecars.last_start_error().is_some());
        assert!(sidecars.last_start_failure().is_some());
        assert!(
            sidecars.owns_instance_lock(),
            "the full-restart owner keeps its lock for retry"
        );
        sidecars.stop().await.unwrap();
    }

    #[tokio::test]
    async fn full_restart_records_before_start_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        data.ensure().unwrap();
        let sidecars = test_sidecars_with_data(data.clone());
        let outcome = reaper::reap_recorded(&data.sidecar_pids_path(), &data.sidecar_lock_path());
        *sidecars.instance_lock.lock() = outcome.lock;

        let error = sidecars
            .full_restart_with(Vec::new(), false, || {
                Err(AppError::Config("before start failed".into()))
            })
            .await
            .expect_err("before_start failure must be returned");

        assert!(error.to_string().contains("before start failed"));
        assert!(
            sidecars
                .last_start_error()
                .is_some_and(|message| message.contains("before start failed"))
        );
        assert!(sidecars.last_start_failure().is_some());
        assert!(sidecars.owns_instance_lock());
        sidecars.stop().await.unwrap();
    }

    #[tokio::test]
    async fn full_restart_transaction_recovers_after_injected_start_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        data.ensure().unwrap();
        let sidecars = Arc::new(test_sidecars_with_data(data.clone()));
        let outcome = reaper::reap_recorded(&data.sidecar_pids_path(), &data.sidecar_lock_path());
        *sidecars.instance_lock.lock() = outcome.lock;
        sidecars.fail_next_start_after_jobworkerp_for_test();
        let recovered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resumed = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let recovery_sidecars = Arc::clone(&sidecars);
        let recovery_flag = Arc::clone(&recovered);
        let resume_flag = Arc::clone(&resumed);
        let error = sidecars
            .full_restart_with_transaction(
                Vec::new(),
                false,
                || Ok(()),
                move || {
                    seed_running_sidecars_for_test(&recovery_sidecars);
                    recovery_flag.store(true, std::sync::atomic::Ordering::Release);
                    async { Ok(Vec::new()) }
                },
                move || {
                    let resume_flag = Arc::clone(&resume_flag);
                    async move {
                        resume_flag.store(true, std::sync::atomic::Ordering::Release);
                        Ok(())
                    }
                },
            )
            .await
            .expect_err("the original startup failure remains the transaction result");

        assert!(error.to_string().contains("injected startup failure"));
        assert!(recovered.load(std::sync::atomic::Ordering::Acquire));
        assert!(resumed.load(std::sync::atomic::Ordering::Acquire));
        assert!(
            sidecars.current_endpoints().is_some(),
            "recovery must restart all children"
        );
        assert!(sidecars.last_start_error().is_some());
        sidecars.stop().await.unwrap();
    }

    #[tokio::test]
    async fn full_restart_transaction_cleans_up_when_recovery_start_also_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        data.ensure().unwrap();
        let sidecars = Arc::new(test_sidecars_with_data(data.clone()));
        let outcome = reaper::reap_recorded(&data.sidecar_pids_path(), &data.sidecar_lock_path());
        *sidecars.instance_lock.lock() = outcome.lock;
        sidecars.fail_next_start_after_jobworkerp_for_test();

        let recovery_sidecars = Arc::clone(&sidecars);
        let error = sidecars
            .full_restart_with_transaction(
                Vec::new(),
                false,
                || Ok(()),
                move || {
                    recovery_sidecars.fail_next_start_after_jobworkerp_for_test();
                    async { Ok(Vec::new()) }
                },
                || async { Ok(()) },
            )
            .await
            .expect_err("a second injected startup failure must fail recovery");

        assert!(error.to_string().contains("フル再起動の復旧に失敗しました"));
        assert!(
            sidecars.state.lock().is_none(),
            "failed recovery must leave no child"
        );
        assert!(sidecars.last_start_error().is_some_and(|message| {
            message.contains("フル再起動の復旧に失敗しました")
        }));
        assert!(matches!(
            sidecars.last_start_failure(),
            Some(crate::sidecar::startup_error::SidecarErrorPayload::Raw { message })
                if message.contains("フル再起動の復旧に失敗しました")
        ));
        assert!(sidecars.owns_instance_lock());
        sidecars.stop().await.unwrap();
    }

    #[tokio::test]
    async fn full_restart_recovery_failure_keeps_original_structured_failure() {
        use crate::sidecar::startup_error::{SidecarErrorPayload, StartupFailure};

        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        data.ensure().unwrap();
        let sidecars = test_sidecars_with_data(data.clone());
        let outcome = reaper::reap_recorded(&data.sidecar_pids_path(), &data.sidecar_lock_path());
        *sidecars.instance_lock.lock() = outcome.lock;
        let original_failure = StartupFailure::LancedbInitFailed {
            uri: "/data/memories".into(),
            message: "database unavailable".into(),
        };

        let error = sidecars
            .full_restart_with_transaction(
                Vec::new(),
                false,
                {
                    let original_failure = original_failure.clone();
                    move || Err(AppError::SidecarStartupFailed(Box::new(original_failure)))
                },
                || async { Err(AppError::Config("rollback failed".into())) },
                || async { Ok(()) },
            )
            .await
            .expect_err("a failed recovery hook must fail the transaction");

        assert!(error.to_string().contains("rollback failed"));
        assert!(sidecars.last_start_error().is_some_and(|message| {
            message.contains("フル再起動の復旧に失敗しました")
        }));
        let failure_snapshot = sidecars.last_start_failure();
        assert!(
            matches!(
                failure_snapshot.as_ref(),
                Some(SidecarErrorPayload::RecoveryFailed {
                    failure: actual,
                    recovery_error,
                }) if *actual == original_failure && recovery_error == "config: rollback failed"
            ),
            "unexpected failure snapshot: {failure_snapshot:?}"
        );
        sidecars.stop().await.unwrap();
    }

    #[tokio::test]
    async fn full_restart_recovery_success_keeps_original_structured_failure() {
        use crate::sidecar::startup_error::{SidecarErrorPayload, StartupFailure};

        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        data.ensure().unwrap();
        let sidecars = Arc::new(test_sidecars_with_data(data.clone()));
        let outcome = reaper::reap_recorded(&data.sidecar_pids_path(), &data.sidecar_lock_path());
        *sidecars.instance_lock.lock() = outcome.lock;
        let original_failure = StartupFailure::ConfigLoadFailed {
            component: "memories".into(),
            message: "invalid configuration".into(),
        };
        let recovery_sidecars = Arc::clone(&sidecars);

        let error = sidecars
            .full_restart_with_transaction(
                Vec::new(),
                false,
                {
                    let original_failure = original_failure.clone();
                    move || Err(AppError::SidecarStartupFailed(Box::new(original_failure)))
                },
                move || {
                    seed_running_sidecars_for_test(&recovery_sidecars);
                    async { Ok(Vec::new()) }
                },
                || async { Ok(()) },
            )
            .await
            .expect_err("the original startup failure remains the transaction result");

        assert!(error.to_string().contains("sidecar startup failed"));
        assert!(matches!(
            sidecars.last_start_failure(),
            Some(SidecarErrorPayload::Structured { failure: actual })
                if actual == original_failure
        ));
        sidecars.stop().await.unwrap();
    }

    #[tokio::test]
    async fn full_restart_transaction_recovers_after_post_start_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        data.ensure().unwrap();
        let sidecars = Arc::new(test_sidecars_with_data(data.clone()));
        let outcome = reaper::reap_recorded(&data.sidecar_pids_path(), &data.sidecar_lock_path());
        *sidecars.instance_lock.lock() = outcome.lock;
        let recovery_sidecars = Arc::clone(&sidecars);
        let post_attempt = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let post_attempt_for_hook = Arc::clone(&post_attempt);

        let error = sidecars
            .full_restart_with_transaction(
                Vec::new(),
                false,
                {
                    let sidecars = Arc::clone(&sidecars);
                    move || {
                        seed_running_sidecars_for_test(&sidecars);
                        Ok(())
                    }
                },
                move || {
                    seed_running_sidecars_for_test(&recovery_sidecars);
                    async { Ok(Vec::new()) }
                },
                move || {
                    let attempt =
                        post_attempt_for_hook.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    async move {
                        if attempt == 0 {
                            Err(AppError::Config("periodic resume failed".into()))
                        } else {
                            Ok(())
                        }
                    }
                },
            )
            .await
            .expect_err("the first post-start failure remains the transaction result");

        assert!(error.to_string().contains("periodic resume failed"));
        assert_eq!(
            post_attempt.load(std::sync::atomic::Ordering::Acquire),
            2,
            "recovery must retry the post-start continuation"
        );
        assert!(sidecars.current_endpoints().is_some());
        sidecars.stop().await.unwrap();
    }

    #[tokio::test]
    async fn full_restart_post_start_timeout_is_reported() {
        let error = run_full_restart_post_start(
            std::future::pending::<AppResult<()>>,
            Duration::from_millis(1),
        )
        .await
        .expect_err("a hung post-start continuation must be bounded");

        assert!(matches!(error, AppError::Config(message) if message.contains("timed out")));
    }

    #[tokio::test]
    async fn full_restart_recovery_hook_runs_before_releasing_the_transaction() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        data.ensure().unwrap();
        let sidecars = test_sidecars_with_data(data.clone());
        let outcome = reaper::reap_recorded(&data.sidecar_pids_path(), &data.sidecar_lock_path());
        *sidecars.instance_lock.lock() = outcome.lock;
        let recovered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let recovered_for_hook = Arc::clone(&recovered);

        let error = sidecars
            .full_restart_with_transaction(
                Vec::new(),
                false,
                || Err(AppError::Config("before start failed".into())),
                move || {
                    let recovered = Arc::clone(&recovered_for_hook);
                    async move {
                        recovered.store(true, std::sync::atomic::Ordering::Release);
                        Err(AppError::Config("rollback failed".into()))
                    }
                },
                || async { Ok(()) },
            )
            .await
            .expect_err("a failed recovery hook must fail the transaction");

        assert!(error.to_string().contains("rollback failed"));
        assert!(recovered.load(std::sync::atomic::Ordering::Acquire));
        assert!(sidecars.owns_instance_lock());
        sidecars.stop().await.unwrap();
    }

    #[tokio::test]
    async fn full_restart_serializes_start_and_stop_around_before_start() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        data.ensure().unwrap();
        let mut sidecars = test_sidecars_with_data(data.clone());
        sidecars.config.jobworkerp_bin = PathBuf::from("/definitely/missing/jobworkerp");
        let outcome = reaper::reap_recorded(&data.sidecar_pids_path(), &data.sidecar_lock_path());
        *sidecars.instance_lock.lock() = outcome.lock;
        assert!(sidecars.owns_instance_lock());

        let sidecars = Arc::new(sidecars);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let restarting = Arc::clone(&sidecars);
        let full_restart = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                restarting
                    .full_restart_with(Vec::new(), false, move || {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Err(AppError::Config("before start failed".into()))
                    })
                    .await
            })
        });

        entered_rx.await.unwrap();
        let mut start = {
            let sidecars = Arc::clone(&sidecars);
            tokio::spawn(async move { sidecars.start().await })
        };
        let mut stop = {
            let sidecars = Arc::clone(&sidecars);
            tokio::spawn(async move { sidecars.stop().await })
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut start)
                .await
                .is_err(),
            "start must wait for the full restart transaction"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut stop)
                .await
                .is_err(),
            "stop must wait for the full restart transaction"
        );

        release_tx.send(()).unwrap();
        assert!(full_restart.await.unwrap().is_err());
        assert!(start.await.unwrap().is_err());
        assert!(stop.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn full_restart_token_reservation_is_released_when_before_start_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path());
        data.ensure().unwrap();
        let sidecars = test_sidecars_with_data(data.clone());
        let outcome = reaper::reap_recorded(&data.sidecar_pids_path(), &data.sidecar_lock_path());
        *sidecars.instance_lock.lock() = outcome.lock;

        let error = sidecars
            .full_restart_with(Vec::new(), true, || {
                Err(AppError::Config("before start failed".into()))
            })
            .await
            .expect_err("a failed full restart must return its before-start error");

        assert!(error.to_string().contains("before start failed"));
        assert!(sidecars.mcp_token_mutation_allowed());
        assert!(sidecars.owns_instance_lock());
        sidecars.stop().await.unwrap();
    }

    #[test]
    fn periodic_runtime_refresh_failure_is_non_fatal_worker_apply_warning() {
        let warning = periodic_runtime_refresh_warning(&AppError::Config(
            "periodic endpoint is unavailable".to_string(),
        ));

        assert_eq!(warning.kind, SidecarWarningKind::WorkerApplyFailed);
        assert!(
            warning
                .message
                .contains("定期実行の runtime 更新に失敗しました")
        );
        assert!(warning.message.contains("periodic endpoint is unavailable"));
        assert_eq!(warning.detail, None);
    }

    #[tokio::test]
    async fn selected_restart_stops_only_its_child_and_keeps_other_pid_and_port() {
        let sidecars = test_sidecars();
        let jobworkerp = Command::new("sleep").arg("30").spawn().unwrap();
        let memories = Command::new("sleep").arg("30").spawn().unwrap();
        let conductor = Command::new("sleep").arg("30").spawn().unwrap();
        let jobworkerp_pid = jobworkerp.id().unwrap();
        let memories_pid = memories.id().unwrap();
        let conductor_pid = conductor.id().unwrap();
        *sidecars.state.lock() = Some(vec![
            Process {
                name: "jobworkerp",
                child: jobworkerp,
                port: 19000,
            },
            Process {
                name: "memories",
                child: memories,
                port: 19010,
            },
            Process {
                name: "conductor",
                child: conductor,
                port: 19020,
            },
        ]);

        sidecars
            .stop_restart_targets(&RestartPlan::jobworkerp_only())
            .await
            .unwrap();

        {
            let guard = sidecars.state.lock();
            let procs = guard.as_ref().unwrap();
            assert_eq!(procs.len(), 2);
            assert!(
                procs
                    .iter()
                    .all(|proc| proc.child.id() != Some(jobworkerp_pid))
            );
            assert!(procs.iter().any(|proc| proc.name == "memories"
                && proc.child.id() == Some(memories_pid)
                && proc.port == 19010));
            assert!(procs.iter().any(|proc| proc.name == "conductor"
                && proc.child.id() == Some(conductor_pid)
                && proc.port == 19020));
        }
        sidecars.stop().await.unwrap();
    }

    #[test]
    fn issue_gateway_workers_register_even_when_tracker_is_unavailable() {
        assert!(issue_tracker::extra_issue_tracker_worker_yaml_paths(false).is_empty());

        let paths = issue_tracker::extra_issue_tracker_worker_yaml_paths(true);
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with("lookback-issue-extract-workers.yaml")),
            "{paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with("lookback-issue-gateway-workers.yaml")),
            "{paths:?}"
        );
    }

    #[test]
    fn replacement_worker_path_builder_includes_private_issue_chat_workers() {
        let previous = std::env::var_os("LOOKBACK_ISSUE_CHAT_WORKER_PORT");
        unsafe { std::env::set_var("LOOKBACK_ISSUE_CHAT_WORKER_PORT", "12345") };
        let paths = worker_paths_with_issue_chat(&[], true);
        if let Some(value) = previous {
            unsafe { std::env::set_var("LOOKBACK_ISSUE_CHAT_WORKER_PORT", value) };
        } else {
            unsafe { std::env::remove_var("LOOKBACK_ISSUE_CHAT_WORKER_PORT") };
        }
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with("lookback-issue-chat-workers.yaml"))
        );
    }

    #[test]
    fn disabled_tracker_never_registers_chat_workers_even_with_a_stale_facade_port() {
        let previous = std::env::var_os("LOOKBACK_ISSUE_CHAT_WORKER_PORT");
        unsafe { std::env::set_var("LOOKBACK_ISSUE_CHAT_WORKER_PORT", "12345") };
        let paths = worker_paths_with_issue_chat(&[], false);
        if let Some(value) = previous {
            unsafe { std::env::set_var("LOOKBACK_ISSUE_CHAT_WORKER_PORT", value) };
        } else {
            unsafe { std::env::remove_var("LOOKBACK_ISSUE_CHAT_WORKER_PORT") };
        }
        assert!(
            !paths
                .iter()
                .any(|path| path.ends_with("lookback-issue-chat-workers.yaml"))
        );
        assert!(
            !paths
                .iter()
                .any(|path| path.ends_with("lookback-issue-gateway-workers.yaml"))
        );
    }

    #[test]
    fn disabled_issue_tracker_preserves_availability_without_a_warning() {
        let _env_guard = crate::commands::issue_tracker_settings::ISSUE_TRACKER_ENV_TEST_LOCK
            .lock()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(directory.path());
        crate::commands::issue_tracker_settings::save_issue_tracker_settings(
            &data.issue_tracker_settings_path(),
            &crate::commands::issue_tracker_settings::IssueTrackerSettings {
                enabled: Some(false),
            },
        )
        .unwrap();
        let sidecars = test_sidecars_with_data(data);
        let availability = Arc::new(crate::commands::IssueTrackerAvailability::default());
        sidecars.bind_issue_tracker_availability(Arc::clone(&availability));
        availability.mark_ready();
        *sidecars.issue_tracker_port.lock() = Some(39_011);
        *sidecars.last_report.lock() = Some(SidecarStartReport {
            endpoints: SidecarEndpoints {
                jobworkerp_port: Some(9000),
                issue_tracker_port: Some(39_011),
                memories_port: Some(9010),
                conductor_port: Some(9020),
                mcp_server_port: None,
            },
            warnings: Vec::new(),
        });

        let report = sidecars.last_report().expect("status report");
        let snapshot = sidecars.issue_tracker_availability().expect("availability");

        assert_eq!(report.endpoints.issue_tracker_port, None);
        assert!(
            !report
                .warnings
                .iter()
                .any(|warning| warning.kind == SidecarWarningKind::IssueTrackerUnavailable)
        );
        assert!(snapshot.endpoint_connected);
        assert!(snapshot.gateway_ready);
        assert!(snapshot.mutation_ready);
    }

    #[test]
    fn disabled_getters_do_not_reset_issue_tracker_state() {
        let _env_guard = crate::commands::issue_tracker_settings::ISSUE_TRACKER_ENV_TEST_LOCK
            .lock()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(directory.path());
        crate::commands::issue_tracker_settings::save_issue_tracker_settings(
            &data.issue_tracker_settings_path(),
            &crate::commands::issue_tracker_settings::IssueTrackerSettings {
                enabled: Some(false),
            },
        )
        .unwrap();
        let sidecars = test_sidecars_with_data(data);
        let availability = Arc::new(crate::commands::IssueTrackerAvailability::default());
        sidecars.bind_issue_tracker_availability(Arc::clone(&availability));
        availability.mark_ready();
        *sidecars.issue_tracker_port.lock() = Some(39_011);
        *sidecars.issue_tracker_gateway_control_url.lock() =
            Some("http://127.0.0.1:39_012".to_string());
        *sidecars.last_report.lock() = Some(SidecarStartReport {
            endpoints: SidecarEndpoints {
                jobworkerp_port: Some(9000),
                issue_tracker_port: Some(39_011),
                memories_port: Some(9010),
                conductor_port: Some(9020),
                mcp_server_port: None,
            },
            warnings: Vec::new(),
        });

        let keys = [
            "ISSUE_TRACKER_GRPC_HOST",
            "ISSUE_TRACKER_GRPC_PORT",
            "ISSUE_TRACKER_GATEWAY_WORKER_HOST",
            "ISSUE_TRACKER_GATEWAY_WORKER_PORT",
        ];
        let previous = keys
            .iter()
            .map(|key| (*key, std::env::var(key).ok()))
            .collect::<Vec<_>>();
        unsafe {
            for key in keys {
                std::env::set_var(key, "preserved");
            }
        }

        let report = sidecars.last_report().expect("status report");
        assert_eq!(report.endpoints.issue_tracker_port, None);
        assert!(
            !report
                .warnings
                .iter()
                .any(|warning| warning.kind == SidecarWarningKind::IssueTrackerUnavailable)
        );
        assert_eq!(sidecars.issue_tracker_url(), None);
        assert_eq!(sidecars.issue_tracker_gateway_control_url(), None);
        assert_eq!(
            sidecars.issue_tracker_availability().unwrap(),
            availability.snapshot()
        );
        assert_eq!(*sidecars.issue_tracker_port.lock(), Some(39_011));
        assert_eq!(
            sidecars.issue_tracker_gateway_control_url.lock().clone(),
            Some("http://127.0.0.1:39_012".to_string())
        );
        assert!(
            keys.iter()
                .all(|key| std::env::var(key).as_deref() == Ok("preserved"))
        );

        unsafe {
            for (key, value) in previous {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn issue_tracker_start_decision_distinguishes_disabled_and_unavailable() {
        assert_eq!(
            issue_tracker::issue_tracker_start_decision(false, true),
            issue_tracker::IssueTrackerStartDecision::Disabled
        );
        assert_eq!(
            issue_tracker::issue_tracker_start_decision(true, true),
            issue_tracker::IssueTrackerStartDecision::Start
        );
        assert_eq!(
            issue_tracker::issue_tracker_start_decision(true, false),
            issue_tracker::IssueTrackerStartDecision::Unavailable
        );
    }

    #[test]
    fn disabled_issue_tracker_clears_inherited_endpoint_environment() {
        let _guard = crate::commands::issue_tracker_settings::ISSUE_TRACKER_ENV_TEST_LOCK
            .lock()
            .unwrap();
        let keys = issue_tracker::ISSUE_TRACKER_ENDPOINT_ENV_KEYS;
        let previous = keys.map(|key| (key, std::env::var(key).ok()));
        unsafe {
            for key in keys {
                std::env::set_var(key, "stale");
            }
        }
        issue_tracker::clear_issue_tracker_endpoint_env();
        let cleared = keys.map(|key| std::env::var(key).ok());

        // Restore before asserting so a failure cannot leak into other tests.
        unsafe {
            for (key, value) in previous {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        assert_eq!(cleared, [None, None, None, None, None, None]);
    }

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

    // Env-touching: serialized because process environment is global.
    #[test]
    fn resolve_timezone_auto_prefers_tz_env() {
        let _env_lock = TIMEZONE_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // "Auto" case = no explicit app-settings timezone (None).
        // SAFETY: this test holds the module's environment mutation lock.
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

    // Env-touching: serialized because process environment is global.
    #[test]
    fn resolve_timezone_prefers_explicit_app_setting_over_env() {
        let _env_lock = TIMEZONE_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: this test holds the module's environment mutation lock.
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

    // Env-touching: serialized because process environment is global.
    #[test]
    fn conductor_timezone_prefers_cron_var_over_tz() {
        let _env_lock = TIMEZONE_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().to_path_buf());
        // SAFETY: this test holds the module's environment mutation lock.
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

    // Env-touching: serialized because process environment is global.
    #[test]
    fn conductor_timezone_honors_app_setting_when_no_cron_var() {
        let _env_lock = TIMEZONE_ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().to_path_buf());
        std::fs::create_dir_all(&data.root).unwrap();
        // Persist an explicit GUI timezone selection.
        crate::data::paths::save_app_settings(
            &data.app_settings_path(),
            &app_settings_with_tz(Some("Europe/Berlin")),
        )
        .unwrap();
        // SAFETY: this test holds the module's environment mutation lock.
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
        let env: std::collections::HashMap<_, _> =
            jobworkerp_channel_env_vars().into_iter().collect();
        let channels: Vec<_> = env["WORKER_CHANNELS"].split(',').collect();
        let concurrency: Vec<_> = env["WORKER_CHANNEL_CONCURRENCIES"]
            .split(',')
            .map(|n| n.parse::<usize>().unwrap())
            .collect();
        assert_eq!(channels.len(), concurrency.len());
        assert_eq!(
            channels.len(),
            channels
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
        );
        assert!(concurrency.iter().all(|n| *n > 0));
        assert!(env["WORKER_DEFAULT_CONCURRENCY"].parse::<usize>().unwrap() > 0);
        for needed in [
            "native-interactive",
            "coding-agent-one-shot",
            "issue_tracker",
            "issue_extract",
            "issue_resolution_manual",
            "issue_resolution_group",
            "issue_resolution_shared",
            "issue_resolution_cross",
            "issue_resolution_cross_candidate",
        ] {
            assert!(channels.contains(&needed), "no consumer for {needed}");
        }
    }

    #[test]
    fn experimental_issues_gate_is_always_passed_to_jobworkerp() {
        assert_eq!(
            experimental_issues_env_vars(false),
            [("LOOKBACK_EXPERIMENTAL_ISSUES_ENABLED", "false")]
        );
        assert_eq!(
            experimental_issues_env_vars(true),
            [("LOOKBACK_EXPERIMENTAL_ISSUES_ENABLED", "true")]
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
            AppError::SidecarStartupFailed(failure) => match *failure {
                crate::sidecar::startup_error::StartupFailure::LancedbSchemaMismatch {
                    expected_dim,
                    actual_dim,
                    ..
                } => {
                    assert_eq!(expected_dim, 2048);
                    assert_eq!(actual_dim, 768);
                }
                other => panic!("wrong startup failure: {other:?}"),
            },
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
            jobworkerp_port: Some(9000),
            issue_tracker_port: None,
            memories_port: Some(9010),
            conductor_port: Some(9020),
            mcp_server_port: None,
        };
        assert_eq!(
            eps.jobworkerp_url().as_deref(),
            Some("http://127.0.0.1:9000")
        );
        assert_eq!(eps.memories_url().as_deref(), Some("http://127.0.0.1:9010"));
        assert_eq!(
            eps.conductor_url().as_deref(),
            Some("http://127.0.0.1:9020")
        );
    }

    #[test]
    fn sidecar_endpoints_serializes_mcp_port_as_null_when_disabled() {
        // The frontend keys off `mcp_server_port` being null to know MCP is
        // off; a missing field would deserialise as undefined on the TS side
        // and break the on/off rendering. Pin the null shape.
        let eps = SidecarEndpoints {
            jobworkerp_port: Some(9000),
            issue_tracker_port: None,
            memories_port: Some(9010),
            conductor_port: Some(9020),
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
    fn mcp_toolset_validation_requires_the_stable_full_tool_surface() {
        let tools = MCP_EXPECTED_TOOL_NAMES
            .iter()
            .map(|name| serde_json::json!({"function": {"name": name}}))
            .collect::<Vec<_>>();
        assert!(validate_mcp_toolset_json(&serde_json::to_string(&tools).unwrap()).is_ok());

        for name in MCP_EXPECTED_TOOL_NAMES {
            let incomplete: Vec<_> = tools
                .iter()
                .filter(|tool| tool["function"]["name"] != name)
                .collect();
            let error = validate_mcp_toolset_json(&serde_json::to_string(&incomplete).unwrap())
                .expect_err("a missing tool must make the MCP toolset unavailable");
            assert!(error.to_string().contains(name));
        }
        for extra in [
            "lookback_issue_agent_approve",
            "lookback_issue_agent_start",
            "arbitrary_shell",
            "lookback_issue_agent_plan",
        ] {
            let mut expanded = tools.clone();
            expanded.push(serde_json::json!({"function":{"name":extra}}));
            assert!(
                validate_mcp_toolset_json(&serde_json::to_string(&expanded).unwrap()).is_err(),
                "{extra}"
            );
        }
    }

    #[test]
    fn disabled_tracker_requires_only_memory_tools_in_the_mcp_set() {
        let tools = [
            "lookback_recall",
            "lookback_get_context",
            "lookback_get_reflections",
        ]
        .map(|name| serde_json::json!({"function": {"name": name}}));
        let raw = serde_json::to_string(&tools).unwrap();
        assert!(validate_mcp_toolset_json_for_issue_tracking(&raw, false).is_ok());
        let mut unsafe_tools = tools.to_vec();
        unsafe_tools.push(serde_json::json!({"function": {"name": "lookback_issue_list_ready"}}));
        assert!(
            validate_mcp_toolset_json_for_issue_tracking(
                &serde_json::to_string(&unsafe_tools).unwrap(),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn malformed_mcp_toolset_json_is_unavailable() {
        assert!(validate_mcp_toolset_json("not-json").is_err());
    }

    #[test]
    fn mcp_readiness_requires_a_valid_toolset_after_partial_restart() {
        let (status, reason) = mcp_runtime_readiness(true, true, None);
        assert_eq!(status, McpRuntimeStatus::Ready);
        assert_eq!(reason, None);
        assert_eq!(
            mcp_listener_port(true, status == McpRuntimeStatus::Ready, 39_010),
            Some(39_010)
        );

        let (status, reason) = mcp_runtime_readiness(
            true,
            true,
            Some("MCP FunctionSet is missing required tools: lookback_issue_release".to_string()),
        );
        assert_eq!(status, McpRuntimeStatus::Unavailable);
        assert_eq!(
            reason.as_deref(),
            Some("MCP FunctionSet is missing required tools: lookback_issue_release")
        );
        assert_eq!(
            mcp_listener_port(true, status == McpRuntimeStatus::Ready, 39_010),
            None
        );

        let (status, reason) = mcp_runtime_readiness(true, false, None);
        assert_eq!(status, McpRuntimeStatus::Unavailable);
        assert_eq!(reason.as_deref(), Some("MCP listener did not become ready"));

        let (status, reason) = mcp_runtime_readiness(false, true, None);
        assert_eq!(status, McpRuntimeStatus::Disabled);
        assert_eq!(reason, None);
    }

    #[test]
    #[cfg(any(debug_assertions, feature = "experimental-issues"))]
    fn last_report_tracks_issue_tracker_health_transitions() {
        let sidecars = test_sidecars();
        crate::commands::issue_tracker_settings::save_issue_tracker_settings(
            &sidecars.config.data.issue_tracker_settings_path(),
            &crate::commands::issue_tracker_settings::IssueTrackerSettings {
                enabled: Some(true),
            },
        )
        .unwrap();
        let availability = Arc::new(crate::commands::IssueTrackerAvailability::default());
        sidecars.bind_issue_tracker_availability(Arc::clone(&availability));
        *sidecars.issue_tracker_port.lock() = Some(39_011);
        *sidecars.last_report.lock() = Some(SidecarStartReport {
            endpoints: SidecarEndpoints {
                jobworkerp_port: Some(9000),
                issue_tracker_port: Some(39_011),
                memories_port: Some(9010),
                conductor_port: Some(9020),
                mcp_server_port: None,
            },
            warnings: Vec::new(),
        });

        let unavailable = sidecars.last_report().expect("status report");
        assert_eq!(unavailable.endpoints.issue_tracker_port, None);
        assert_eq!(sidecars.issue_tracker_url(), None);
        assert!(
            unavailable
                .warnings
                .iter()
                .any(|warning| warning.kind == SidecarWarningKind::IssueTrackerUnavailable)
        );

        availability.mark_ready();
        let ready = sidecars.last_report().expect("status report");
        assert_eq!(ready.endpoints.issue_tracker_port, Some(39_011));
        assert_eq!(
            sidecars.issue_tracker_url().as_deref(),
            Some("http://127.0.0.1:39011")
        );
        assert!(
            !ready
                .warnings
                .iter()
                .any(|warning| warning.kind == SidecarWarningKind::IssueTrackerUnavailable)
        );

        availability.mark_gateway_unavailable("Gateway registration failed");
        let degraded = sidecars.last_report().expect("status report");
        assert_eq!(degraded.endpoints.issue_tracker_port, Some(39_011));
        assert!(degraded.warnings.iter().any(|warning| {
            warning.kind == SidecarWarningKind::IssueTrackerUnavailable
                && warning.message == "Gateway registration failed"
        }));
    }

    #[test]
    fn issue_tracker_maintenance_health_failure_keeps_the_endpoint_for_recovery() {
        let availability = Arc::new(crate::commands::IssueTrackerAvailability::default());
        let issue_tracker_port = Arc::new(Mutex::new(None));

        issue_tracker::mark_issue_tracker_maintenance_ready(
            Some(&availability),
            &issue_tracker_port,
            39_011,
        );

        assert_eq!(*issue_tracker_port.lock(), Some(39_011));
        let ready = availability.snapshot();
        assert!(ready.endpoint_connected);
        assert!(ready.gateway_ready);
        assert!(ready.mutation_ready);

        issue_tracker::mark_issue_tracker_maintenance_unavailable(
            Some(&availability),
            &issue_tracker_port,
            "Issue Tracker health check failed",
        );

        assert_eq!(*issue_tracker_port.lock(), Some(39_011));
        let degraded = availability.snapshot();
        assert!(degraded.endpoint_connected);
        assert!(degraded.gateway_ready);
        assert!(!degraded.mutation_ready);
        assert_eq!(
            degraded.reason.as_deref(),
            Some("Issue Tracker health check failed")
        );
    }

    #[test]
    fn issue_tracker_gateway_failure_does_not_hide_primary_mutations() {
        let availability = crate::commands::IssueTrackerAvailability::default();
        availability.mark_ready();
        availability.mark_gateway_unavailable("Gateway registration failed");
        let snapshot = availability.snapshot();
        assert!(snapshot.endpoint_connected);
        assert!(!snapshot.gateway_ready);
        assert!(snapshot.mutation_ready);
        assert_eq!(
            snapshot.reason.as_deref(),
            Some("Gateway registration failed")
        );
    }

    #[test]
    fn automatic_issue_worker_endpoint_requires_a_ready_gateway_and_enabled_tracker() {
        let sidecars = test_sidecars();
        let availability = Arc::new(crate::commands::IssueTrackerAvailability::default());
        sidecars.bind_issue_tracker_availability(Arc::clone(&availability));
        *sidecars.issue_tracker_gateway_worker_url.lock() = Some("http://127.0.0.1:39999".into());
        assert!(
            sidecars
                .issue_tracker_gateway_worker_url_with_issue_tracking(true)
                .is_none()
        );
        availability.mark_ready();
        assert_eq!(
            sidecars
                .issue_tracker_gateway_worker_url_with_issue_tracking(true)
                .as_deref(),
            Some("http://127.0.0.1:39999")
        );
        assert!(
            sidecars
                .issue_tracker_gateway_worker_url_with_issue_tracking(false)
                .is_none()
        );
        availability.mark_gateway_unavailable("policy unavailable");
        assert!(
            sidecars
                .issue_tracker_gateway_worker_url_with_issue_tracking(true)
                .is_none()
        );
        availability.mark_ready();
        sidecars.reset_issue_tracker_when_disabled();
        assert!(sidecars.issue_tracker_gateway_worker_url.lock().is_none());
    }

    #[tokio::test]
    async fn issue_tracker_replacement_cannot_reuse_previous_gateway_readiness() {
        let sidecars = test_sidecars();
        let availability = Arc::new(crate::commands::IssueTrackerAvailability::default());
        sidecars.bind_issue_tracker_availability(Arc::clone(&availability));
        availability.mark_ready();
        *sidecars.issue_tracker_gateway_worker_url.lock() = Some("http://127.0.0.1:39999".into());
        sidecars
            .stop_restart_targets(&RestartPlan::issue_tracker())
            .await
            .unwrap();
        *sidecars.issue_tracker_gateway_worker_url.lock() = Some("http://127.0.0.1:39998".into());
        assert!(
            sidecars
                .issue_tracker_gateway_worker_url_with_issue_tracking(true)
                .is_none()
        );
        availability.mark_gateway_ready();
        assert!(
            sidecars
                .issue_tracker_gateway_worker_url_with_issue_tracking(true)
                .is_some()
        );
    }

    #[test]
    fn issue_tracker_uses_the_separate_persisted_work_eligibility_setting() {
        let root = tempfile::tempdir().unwrap();
        let registry = crate::issue_work_policy::WorkEligibilityRegistry::open(root.path());
        registry
            .update(crate::issue_work_policy::WorkEligibility::IncludeAutoRegistered)
            .unwrap();

        assert_eq!(
            issue_tracker::configured_work_eligibility(root.path()) as i32,
            crate::grpc::proto::issue_tracker::manager::v1::WorkEligibilityMode::IncludeAutoRegistered
                as i32
        );
    }

    #[tokio::test]
    async fn issue_tracker_gateway_registration_lock_serializes_settings_and_maintenance() {
        let sidecars = Arc::new(test_sidecars());
        let guard = sidecars.lock_issue_tracker_gateway_registration().await;
        let (acquired_tx, mut acquired_rx) = tokio::sync::oneshot::channel();
        let waiting_sidecars = Arc::clone(&sidecars);
        let waiting = tokio::spawn(async move {
            let _guard = waiting_sidecars
                .lock_issue_tracker_gateway_registration()
                .await;
            acquired_tx.send(()).unwrap();
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut acquired_rx)
                .await
                .is_err(),
            "a concurrent registration must wait for the active update"
        );
        drop(guard);
        waiting.await.unwrap();
        acquired_rx.await.unwrap();
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
            issue_tracker_server_bin: None,
            function_set_yaml_paths: Vec::new(),
            reflection_dispatch_enabled: false,
            auto_embedding_enabled: true,
            workflows_dir: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../workers/workflows"),
            ),
            lance_language_model_home: lance_home,
            lindera_dict_staged: false,
            llm_model: None,
            llm_hf_repo: None,
            llm_ctx_size: None,
            llm_kv_cache_type: None,
            env_file: None,
        })
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn startup_does_not_spawn_when_embedding_workers_cannot_be_staged() {
        let temp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(temp.path().join("data"));
        let marker = temp.path().join("jobworkerp-spawned");
        let binary = temp.path().join("jobworkerp");
        write_executable(
            &binary,
            &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        );
        let mut sidecars = test_sidecars_with_data(data);
        sidecars.config.jobworkerp_bin = binary;
        sidecars.config.workflows_dir = None;

        let error = sidecars.start().await.unwrap_err();

        assert!(error.to_string().contains("no bundled workflows dir"));
        assert!(!marker.exists(), "staging failure must happen before spawn");
        assert!(sidecars.state.lock().is_none());
        assert!(!sidecars.owns_instance_lock());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn selected_memories_restart_does_not_spawn_when_embedding_workers_cannot_be_staged() {
        let temp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(temp.path().join("data"));
        let marker = temp.path().join("memories-spawned");
        let binary = temp.path().join("memories");
        write_executable(
            &binary,
            &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        );
        let mut sidecars = test_sidecars_with_data(data);
        sidecars.config.memories_bin = binary;
        sidecars.config.workflows_dir = None;
        seed_running_sidecars_for_test(&sidecars);

        let error = sidecars
            .restart_plan_with(
                RestartPlan {
                    memories: true,
                    ..RestartPlan::empty()
                },
                Vec::new(),
                || Ok(()),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("no bundled workflows dir"));
        assert!(!marker.exists(), "staging failure must happen before spawn");
        let state = sidecars.state.lock();
        let processes = state
            .as_ref()
            .expect("non-selected children remain tracked");
        assert!(processes.iter().any(|process| process.name == "jobworkerp"));
        assert!(processes.iter().any(|process| process.name == "conductor"));
        assert!(!processes.iter().any(|process| process.name == "memories"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn embedding_cli_does_not_run_when_worker_yaml_staging_fails() {
        let temp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(temp.path().join("data"));
        data.ensure().unwrap();
        let marker = temp.path().join("cli-ran");
        let tool = temp.path().join("memories-db-migrate");
        write_executable(
            &tool,
            &format!("#!/bin/sh\ntouch '{}'\necho unexpected\n", marker.display()),
        );
        let mut sidecars = test_sidecars_with_data(data);
        sidecars.config.memories_db_migrate_bin = tool;
        sidecars.config.workflows_dir = None;

        let error = sidecars
            .run_embedding_cli(crate::sidecar::embedding_migration::EmbeddingRequest::Inspect)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("no bundled workflows dir"));
        assert!(!marker.exists(), "CLI must not run without staged YAML");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn start_refuses_to_spawn_over_an_incomplete_tracked_child_set() {
        let temp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(temp.path().join("data"));
        let marker = temp.path().join("jobworkerp-spawned");
        let binary = temp.path().join("jobworkerp");
        write_executable(
            &binary,
            &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        );
        let mut sidecars = test_sidecars_with_data(data);
        sidecars.config.jobworkerp_bin = binary;
        *sidecars.state.lock() = Some(vec![Process {
            name: "jobworkerp",
            child: Command::new("sleep").arg("60").spawn().unwrap(),
            port: 39_010,
        }]);

        let error = sidecars.start_inner(Vec::new(), None).await.unwrap_err();

        assert!(error.to_string().contains("children remain tracked"));
        assert!(!marker.exists());
        assert_eq!(sidecars.state.lock().as_ref().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn migration_environment_matches_memory_runtime_and_explicit_env_file_values() {
        let temp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(temp.path().join("data"));
        data.ensure().unwrap();
        let env_file = temp.path().join("memories.env");
        std::fs::write(
            &env_file,
            concat!(
                "MEMORY_EMBEDDING_STATE_DIR=/configured/state\n",
                "LOOKBACK_EMBEDDING_MODEL_ID=fixture/model-from-env-file\n",
                "MEMORY_EMBEDDING_MODEL_REVISION=env-file-default-r\n",
                "MEMORY_DISTANCE_TYPE=dot\n",
                "THREAD_LANCEDB_URI=/configured/threads.lancedb\n",
                "MEMORY_EMBEDDING_QUERY_PREFIX=fixture-query\n",
                "MEMORY_EMBEDDING_DOCUMENT_PREFIX=unsupported-document-prefix\n",
            ),
        )
        .unwrap();
        let mut sidecars = test_sidecars_with_data(data.clone());
        sidecars.config.env_file = Some(env_file);

        let env_default_runtime = sidecars.current_embedding_runtime();
        assert_eq!(env_default_runtime.model_id, "fixture/model-from-env-file");
        assert_eq!(env_default_runtime.model_revision, "env-file-default-r");

        crate::commands::embedding_settings::save_embedding_settings(
            &data.embedding_settings_path(),
            &crate::commands::embedding_settings::EmbeddingSettings {
                preset_id: Some("qwen3-embedding-0-6b".into()),
                model_revision: Some("saved-new-r".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let saved_runtime = sidecars.current_embedding_runtime();
        assert_eq!(saved_runtime.model_revision, "saved-new-r");

        let env = sidecars.memories_migration_env().unwrap();
        let values = env
            .extra_env
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(env.database_url, data.memories_sqlite_url());
        assert_eq!(values["SQLITE_URL"], data.memories_sqlite_url());
        assert_eq!(
            values["MEMORY_VECTOR_SIZE"],
            saved_runtime.vector_size.to_string()
        );
        // An operator override must reach the tool unchanged.
        assert_eq!(values["THREAD_LANCEDB_URI"], "/configured/threads.lancedb");
        assert_eq!(
            values["MEMORY_LANCEDB_URI"],
            data.memories_lancedb_uri().to_string_lossy()
        );
        assert_eq!(values["MEMORY_VECTOR_ENABLED"], "true");
        assert_eq!(values["THREAD_VECTOR_ENABLED"], "true");
        assert_eq!(values["REFLECTION_INTENT_VECTOR_ENABLED"], "true");
        assert_eq!(values["MEMORY_DISTANCE_TYPE"], "dot");
        assert_eq!(values["MEMORY_EMBEDDING_MODEL_REVISION"], "saved-new-r");
        assert_eq!(
            values["LOOKBACK_EMBEDDING_MODEL_ID"],
            saved_runtime.model_id
        );
        assert_eq!(values["MEMORY_EMBEDDING_STATE_DIR"], "/configured/state");
        assert_eq!(values["MEMORY_EMBEDDING_QUERY_PREFIX"], "fixture-query");
        assert_eq!(values["MEMORY_EMBEDDING_DOCUMENT_PREFIX"], "");
        let staged = PathBuf::from(&values["MEMORY_WORKERS_YAML"]);
        assert!(staged.is_file());
        assert!(
            staged
                .parent()
                .unwrap()
                .join("auto-embedding.yaml")
                .is_file()
        );

        sidecars.config.env_file = None;
        let default_env = sidecars.memories_migration_env().unwrap();
        assert!(
            !default_env
                .extra_env
                .contains_key(OsStr::new("MEMORY_EMBEDDING_STATE_DIR"))
        );
        // Without an override, memories derives thread storage from MEMORY_*.
        for key in [
            "THREAD_LANCEDB_URI",
            "THREAD_LANCEDB_TABLE",
            "THREAD_VECTOR_SIZE",
        ] {
            assert!(!default_env.extra_env.contains_key(OsStr::new(key)));
        }
        let reflection_workers = PathBuf::from(
            default_env.extra_env[OsStr::new("REFLECTION_INTENT_WORKERS_YAML")].clone(),
        );
        assert!(reflection_workers.is_file());

        sidecars.config.auto_embedding_enabled = false;
        let disabled = sidecars.memories_migration_env().unwrap();
        assert!(!disabled.thread_vector_enabled);
        assert_eq!(
            disabled.extra_env[OsStr::new("MEMORY_VECTOR_ENABLED")],
            "false"
        );
        assert_eq!(
            disabled.extra_env[OsStr::new("THREAD_VECTOR_ENABLED")],
            "false"
        );
        assert_eq!(
            disabled.extra_env[OsStr::new("REFLECTION_INTENT_VECTOR_ENABLED")],
            "false"
        );
        assert_eq!(disabled.extra_env[OsStr::new("MEMORY_VECTOR_SIZE")], "");
        assert_eq!(disabled.extra_env[OsStr::new("MEMORY_LANCEDB_URI")], "");
    }

    struct RestoreEnvironment(Vec<(&'static str, Option<OsString>)>);

    impl Drop for RestoreEnvironment {
        fn drop(&mut self) {
            unsafe {
                for (key, value) in self.0.drain(..) {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn relaunch_rejects_embedding_mirror_from_waiting_startup() {
        let keys = crate::commands::embedding_settings::EMBEDDING_ENV_KEYS;
        let _restore = RestoreEnvironment(
            keys.iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect(),
        );
        unsafe {
            for key in keys {
                std::env::remove_var(key);
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(temp.path().join("old-root"));
        data.ensure().unwrap();
        let sidecars = std::sync::Arc::new(test_sidecars_with_data(data));
        let (waiting, waited) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let producer = {
            let sidecars = sidecars.clone();
            tokio::spawn(async move {
                let runtime = vec![("MEMORY_EMBEDDING_MODEL_REVISION", "old-r".into())];
                waiting.send(()).unwrap();
                resumed.await.unwrap();
                sidecars.mirror_embedding_env(&runtime)
            })
        };
        waited.await.unwrap();
        sidecars.restore_embedding_inputs_for_relaunch();
        resume.send(()).unwrap();
        assert!(producer.await.unwrap().is_err());
        assert!(std::env::var_os("MEMORY_EMBEDDING_MODEL_REVISION").is_none());
        let next_root = DataPaths::with_root(temp.path().join("next-root"));
        next_root.ensure().unwrap();
        assert_eq!(
            test_sidecars_with_data(next_root)
                .current_embedding_runtime()
                .model_revision,
            "unversioned"
        );
    }

    #[test]
    fn embedding_runtime_uses_original_external_env_after_process_mirroring() {
        let keys = crate::commands::embedding_settings::EMBEDDING_ENV_KEYS;
        let _restore = RestoreEnvironment(
            keys.iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect(),
        );
        let external_env = [
            ("LOOKBACK_EMBEDDING_MODEL_ID", "external/model"),
            ("LOOKBACK_EMBEDDING_TOKENIZER_ID", "external/tokenizer"),
            ("LOOKBACK_EMBEDDING_DTYPE", "F32"),
            ("LOOKBACK_EMBEDDING_MAX_SEQ_LEN", "777"),
            ("LOOKBACK_EMBEDDING_VECTOR_SIZE", "777"),
            ("MEMORY_EMBEDDING_MODEL_REVISION", "external-r"),
            ("MEMORY_DISTANCE_TYPE", "dot"),
            ("MEMORY_EMBEDDING_DOCUMENT_PREFIX", "external-document"),
            ("MEMORY_EMBEDDING_QUERY_PREFIX", "external-query"),
        ];
        unsafe {
            for key in keys {
                std::env::remove_var(key);
            }
            for (key, value) in external_env {
                std::env::set_var(key, value);
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(temp.path().join("data"));
        data.ensure().unwrap();
        crate::commands::embedding_settings::save_embedding_settings(
            &data.embedding_settings_path(),
            &crate::commands::embedding_settings::EmbeddingSettings {
                preset_id: Some("qwen3-embedding-0-6b".into()),
                model_revision: Some("saved-r".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let sidecars = test_sidecars_with_data(data.clone());
        let saved_runtime = sidecars.current_embedding_runtime();
        assert_eq!(saved_runtime.model_id, "Qwen/Qwen3-Embedding-0.6B");
        assert_eq!(saved_runtime.vector_size, 1024);
        assert_eq!(saved_runtime.model_revision, "saved-r");

        // Simulate the next sidecar start mirroring the selected runtime into
        // the host process, then clearing only the persisted revision.
        unsafe {
            apply_embedding_env(&[
                ("LOOKBACK_EMBEDDING_MODEL_ID", "mirrored/model".into()),
                (
                    "LOOKBACK_EMBEDDING_TOKENIZER_ID",
                    "mirrored/tokenizer".into(),
                ),
                ("LOOKBACK_EMBEDDING_DTYPE", "BF16".into()),
                ("LOOKBACK_EMBEDDING_MAX_SEQ_LEN", "999".into()),
                ("LOOKBACK_EMBEDDING_VECTOR_SIZE", "999".into()),
                ("MEMORY_EMBEDDING_MODEL_REVISION", "saved-r".into()),
                ("MEMORY_DISTANCE_TYPE", "l2".into()),
                (
                    "MEMORY_EMBEDDING_DOCUMENT_PREFIX",
                    "mirrored-document".into(),
                ),
                ("MEMORY_EMBEDDING_QUERY_PREFIX", "mirrored-query".into()),
            ]);
        }
        crate::commands::embedding_settings::save_embedding_settings(
            &data.embedding_settings_path(),
            &crate::commands::embedding_settings::EmbeddingSettings {
                preset_id: Some("qwen3-embedding-0-6b".into()),
                model_revision: None,
                ..Default::default()
            },
        )
        .unwrap();
        let cleared_revision = sidecars.current_embedding_runtime();
        assert_eq!(cleared_revision.model_id, "Qwen/Qwen3-Embedding-0.6B");
        assert_eq!(cleared_revision.vector_size, 1024);
        assert_eq!(cleared_revision.model_revision, "external-r");
        assert_eq!(cleared_revision.distance_type, "dot");
        assert_eq!(
            cleared_revision.query_prefix.as_deref(),
            Some("external-query")
        );
        let candidate_request = crate::commands::embedding_settings::SetEmbeddingSettingsRequest {
            preset_id: Some("qwen3-embedding-0-6b".into()),
            custom_model_id: None,
            custom_tokenizer_id: None,
            custom_vector_size: None,
            custom_dtype: None,
            custom_max_sequence_length: None,
            custom_is_multimodal: None,
            model_revision: None,
        };
        let candidate = crate::commands::embedding_settings::prepare_embedding_candidate_runtime(
            &candidate_request,
            |key| sidecars.embedding_env_lookup(key),
        )
        .unwrap();
        assert_eq!(candidate, cleared_revision);
        let persisted =
            crate::commands::embedding_settings::apply_embedding_settings_to_disk_with_env(
                &data,
                &candidate_request,
                None,
                false,
                |key| sidecars.embedding_env_lookup(key),
            )
            .unwrap();
        assert_eq!(persisted.new_runtime, cleared_revision);

        // Tauri's relaunch inherits the host environment. A newly constructed
        // Sidecars must capture the original inputs, not the previous runtime.
        sidecars.restore_embedding_inputs_for_relaunch();
        let relaunched = test_sidecars_with_data(data.clone());
        assert_eq!(relaunched.current_embedding_runtime(), cleared_revision);
        for (key, value) in external_env {
            assert_eq!(std::env::var(key).unwrap(), value);
        }

        // With no external revision captured, a later mirrored saved revision
        // must not become a fallback when the user clears the saved field.
        unsafe { std::env::remove_var("MEMORY_EMBEDDING_MODEL_REVISION") };
        let no_external_revision = test_sidecars_with_data(data.clone());
        unsafe {
            apply_embedding_env(&[("MEMORY_EMBEDDING_MODEL_REVISION", "mirrored-r".into())]);
        }
        let no_revision_settings = crate::commands::embedding_settings::EmbeddingSettings {
            preset_id: Some("qwen3-embedding-0-6b".into()),
            model_revision: None,
            ..Default::default()
        };
        crate::commands::embedding_settings::save_embedding_settings(
            &data.embedding_settings_path(),
            &no_revision_settings,
        )
        .unwrap();
        assert_eq!(
            no_external_revision
                .current_embedding_runtime()
                .model_revision,
            "unversioned"
        );

        no_external_revision.restore_embedding_inputs_for_relaunch();
        assert!(std::env::var_os("MEMORY_EMBEDDING_MODEL_REVISION").is_none());
        let next_root = DataPaths::with_root(temp.path().join("next-root"));
        next_root.ensure().unwrap();
        crate::commands::embedding_settings::save_embedding_settings(
            &next_root.embedding_settings_path(),
            &no_revision_settings,
        )
        .unwrap();
        let relaunched = test_sidecars_with_data(next_root);
        assert_eq!(
            relaunched.current_embedding_runtime().model_revision,
            "unversioned"
        );

        crate::commands::embedding_settings::save_embedding_settings(
            &data.embedding_settings_path(),
            &Default::default(),
        )
        .unwrap();
        let external_runtime = sidecars.current_embedding_runtime();
        assert_eq!(external_runtime.model_id, "external/model");
        assert_eq!(
            external_runtime.tokenizer_id.as_deref(),
            Some("external/tokenizer")
        );
        assert_eq!(external_runtime.vector_size, 777);
        assert_eq!(external_runtime.max_sequence_length, 777);
        assert_eq!(external_runtime.model_revision, "external-r");
        assert_eq!(external_runtime.distance_type, "dot");

        // A fresh application instance in the original external environment
        // resolves the same persisted unset revision without inheriting the
        // previous Sidecars instance's selected runtime.
        unsafe {
            for key in keys {
                std::env::remove_var(key);
            }
            for (key, value) in external_env {
                std::env::set_var(key, value);
            }
        }
        crate::commands::embedding_settings::save_embedding_settings(
            &data.embedding_settings_path(),
            &Default::default(),
        )
        .unwrap();
        let fresh_sidecars = test_sidecars_with_data(data);
        assert_eq!(fresh_sidecars.current_embedding_runtime(), external_runtime);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn embedding_cli_inspect_uses_shared_env_without_stopping_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(temp.path().join("data"));
        data.ensure().unwrap();
        let capture = temp.path().join("embedding-env");
        let tool = temp.path().join("memories-db-migrate");
        write_executable(
            &tool,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$MEMORY_WORKERS_YAML" "$MEMORY_VECTOR_SIZE" "$MEMORY_DISTANCE_TYPE" "$MEMORY_EMBEDDING_STATE_DIR" "$SQLITE_URL" > '{}'
printf '%s\n' 'embedding_inspect status=completed state=consistent space=none required=0 complete=0 missing=0 stale=0 unverified=0 failed_transient=0 failed_permanent=0 orphan=0 backup_supported=false next_action=none'
"#,
                capture.display()
            ),
        );
        let env_file = temp.path().join("memories.env");
        std::fs::write(
            &env_file,
            "MEMORY_EMBEDDING_STATE_DIR=/explicit/state\nMEMORY_DISTANCE_TYPE=l2\n",
        )
        .unwrap();
        let mut sidecars = test_sidecars_with_data(data.clone());
        sidecars.config.memories_db_migrate_bin = tool;
        sidecars.config.env_file = Some(env_file);
        seed_running_sidecars_for_test(&sidecars);

        let result = sidecars
            .run_embedding_cli(crate::sidecar::embedding_migration::EmbeddingRequest::Inspect)
            .await
            .unwrap();

        assert!(matches!(
            result,
            crate::sidecar::embedding_migration::EmbeddingMigrationResult::Inspect(_)
        ));
        let captured = std::fs::read_to_string(capture).unwrap();
        let values = captured.lines().collect::<Vec<_>>();
        assert!(Path::new(values[0]).is_file());
        assert_eq!(
            values[1],
            sidecars.current_embedding_runtime().vector_size.to_string()
        );
        assert_eq!(values[2], "l2");
        assert_eq!(values[3], "/explicit/state");
        assert_eq!(values[4], data.memories_sqlite_url());
        assert_eq!(sidecars.state.lock().as_ref().unwrap().len(), 3);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn embedding_plan_cli_args_and_environment_share_saved_revision_over_env_file() {
        let temp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(temp.path().join("data"));
        data.ensure().unwrap();
        crate::commands::embedding_settings::save_embedding_settings(
            &data.embedding_settings_path(),
            &crate::commands::embedding_settings::EmbeddingSettings {
                preset_id: Some("qwen3-embedding-0-6b".into()),
                model_revision: Some("saved-new-r".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let env_file = temp.path().join("memories.env");
        std::fs::write(
            &env_file,
            "MEMORY_EMBEDDING_MODEL_REVISION=env-file-old-r\nMEMORY_EMBEDDING_QUERY_PREFIX=env-query\n",
        )
        .unwrap();
        let capture_prefix = temp.path().join("embedding-cli");
        let tool = temp.path().join("memories-db-migrate");
        write_executable(
            &tool,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}-'$2'-args'\nprintf '%s\\n' \"$MEMORY_EMBEDDING_MODEL_REVISION\" \"$MEMORY_EMBEDDING_QUERY_PREFIX\" > '{}-'$2'-env'\ncase \"$2\" in\n  plan) printf '%s\\n' 'embedding_plan status=completed decision=reembed_required executable=true current_space=none target_space=target memory_text=1 memory_media=0 thread=0 reflection_intent=0 failed_transient=0 failed_permanent=0' ;;\n  switch) printf '%s\\n' 'embedding_switch status=completed outcome=switched attempt=attempt-a source_space=none target_space=target backup=none next_action=start_rebuild' ;;\nesac\n",
                capture_prefix.display(),
                capture_prefix.display(),
            ),
        );
        let mut sidecars = test_sidecars_with_data(data);
        sidecars.config.memories_db_migrate_bin = tool;
        sidecars.config.env_file = Some(env_file);
        seed_running_sidecars_for_test(&sidecars);
        let runtime = sidecars.current_embedding_runtime();
        let target = crate::sidecar::embedding_migration::EmbeddingTarget {
            model_id: runtime.model_id.clone(),
            tokenizer_model_id: runtime.tokenizer_id.clone().unwrap_or_default(),
            revision: runtime.model_revision.clone(),
            dimension: runtime.vector_size,
        };

        let result = sidecars
            .run_embedding_cli_with_runtime(
                crate::sidecar::embedding_migration::EmbeddingRequest::Plan { target },
                &runtime,
            )
            .await
            .unwrap();

        assert!(matches!(
            result,
            crate::sidecar::embedding_migration::EmbeddingMigrationResult::Plan(_)
        ));
        let args =
            std::fs::read_to_string(format!("{}-plan-args", capture_prefix.display())).unwrap();
        assert!(
            args.lines()
                .any(|arg| arg == "--model-id=Qwen/Qwen3-Embedding-0.6B")
        );
        assert!(args.lines().any(|arg| arg == "--revision=saved-new-r"));
        assert!(args.lines().any(|arg| arg == "--dimension=1024"));
        assert_eq!(
            std::fs::read_to_string(format!("{}-plan-env", capture_prefix.display()))
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["saved-new-r", "env-query"]
        );

        let switch_result = sidecars
            .run_embedding_cli_with_runtime(
                crate::sidecar::embedding_migration::EmbeddingRequest::Switch {
                    target: crate::sidecar::embedding_migration::EmbeddingTarget {
                        model_id: runtime.model_id.clone(),
                        tokenizer_model_id: runtime.tokenizer_id.clone().unwrap_or_default(),
                        revision: runtime.model_revision.clone(),
                        dimension: runtime.vector_size,
                    },
                    expected_space: crate::sidecar::embedding_migration::SpaceValue::None,
                    backup: crate::sidecar::embedding_migration::EmbeddingBackup::NoBackupUnsafe,
                    maintenance_window_ack: true,
                },
                &runtime,
            )
            .await
            .unwrap();
        assert!(matches!(
            switch_result,
            crate::sidecar::embedding_migration::EmbeddingMigrationResult::Switched(_)
        ));
        let switch_args =
            std::fs::read_to_string(format!("{}-switch-args", capture_prefix.display())).unwrap();
        assert!(
            switch_args
                .lines()
                .any(|arg| arg == "--revision=saved-new-r")
        );
        assert_eq!(
            std::fs::read_to_string(format!("{}-switch-env", capture_prefix.display()))
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["saved-new-r", "env-query"]
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn target_root_embedding_plan_uses_target_database_and_model_home() {
        let temp = tempfile::tempdir().unwrap();
        let current = DataPaths::with_root(temp.path().join("current"));
        let target = DataPaths::with_root(temp.path().join("target"));
        current.ensure().unwrap();
        target.ensure().unwrap();
        crate::data::paths::save_app_settings(
            &target.app_settings_path(),
            &crate::data::paths::AppSettings {
                hf_home_mode: crate::data::paths::HfHomeMode::DataRoot,
                ..Default::default()
            },
        )
        .unwrap();
        crate::commands::embedding_settings::save_embedding_settings(
            &target.embedding_settings_path(),
            &crate::commands::embedding_settings::EmbeddingSettings {
                preset_id: Some("qwen3-vl-embedding-2b".into()),
                model_revision: Some("target-r".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let env_file = temp.path().join("target.env");
        std::fs::write(
            &env_file,
            concat!(
                "LOOKBACK_EMBEDDING_MODEL_ID=env-file/model\n",
                "LOOKBACK_EMBEDDING_TOKENIZER_ID=env-file/tokenizer\n",
                "MEMORY_EMBEDDING_MODEL_REVISION=env-file-old-r\n",
                "MEMORY_DISTANCE_TYPE=dot\n",
            ),
        )
        .unwrap();
        let capture_prefix = temp.path().join("target-plan");
        let tool = temp.path().join("memories-db-migrate");
        write_executable(
            &tool,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$SQLITE_URL\" \"$LANCE_LANGUAGE_MODEL_HOME\" \"$HF_HOME\" \"$MEMORY_WORKERS_YAML\" \"$MEMORY_VECTOR_SIZE\" \"$MEMORY_EMBEDDING_MODEL_REVISION\" \"$LOOKBACK_EMBEDDING_MODEL_ID\" \"$LOOKBACK_EMBEDDING_TOKENIZER_ID\" \"$MEMORY_DISTANCE_TYPE\" > '{}'\nprintf '%s\\n' \"$@\" > '{}-args'\nprintf '%s\\n' 'embedding_plan status=completed decision=reembed_required executable=true current_space=none target_space=target memory_text=1 memory_media=0 thread=0 reflection_intent=0 failed_transient=0 failed_permanent=0'\n",
                capture_prefix.display(),
                capture_prefix.display(),
            ),
        );
        let mut sidecars = test_sidecars_with_data(current.clone());
        sidecars.config.memories_db_migrate_bin = tool;
        sidecars.config.env_file = Some(env_file);
        let current_runtime = sidecars.current_embedding_runtime();
        let runtime = sidecars.embedding_runtime_for_data(&target);
        assert_ne!(current_runtime.model_id, runtime.model_id);
        let target_request = crate::sidecar::embedding_migration::EmbeddingTarget {
            model_id: runtime.model_id.clone(),
            tokenizer_model_id: runtime.tokenizer_id.clone().unwrap_or_default(),
            revision: runtime.model_revision.clone(),
            dimension: runtime.vector_size,
        };

        let result = sidecars
            .run_embedding_cli_for_data(
                &target,
                &runtime,
                crate::sidecar::embedding_migration::EmbeddingRequest::Plan {
                    target: target_request,
                },
            )
            .await
            .unwrap();

        assert!(matches!(
            result,
            crate::sidecar::embedding_migration::EmbeddingMigrationResult::Plan(_)
        ));
        let values = std::fs::read_to_string(capture_prefix)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(values[0], target.memories_sqlite_url());
        assert_eq!(
            values[1],
            target.lance_language_model_home().display().to_string()
        );
        assert_eq!(values[2], target.models_dir().display().to_string());
        let staged_workers = PathBuf::from(&values[3]);
        assert!(staged_workers.starts_with(&target.root));
        assert!(staged_workers.is_file());
        assert_eq!(values[4], "2048");
        assert_eq!(values[5], "target-r");
        assert_eq!(values[6], runtime.model_id);
        assert_eq!(values[7], "");
        assert_eq!(values[8], "dot");
        assert!(!values[0].contains(&current.root.display().to_string()));
        let args = std::fs::read_to_string(temp.path().join("target-plan-args"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(
            args.iter()
                .any(|arg| arg == "--model-id=Qwen/Qwen3-VL-Embedding-2B")
        );
        assert!(args.iter().any(|arg| arg == "--tokenizer-model-id="));
        assert!(args.iter().any(|arg| arg == "--revision=target-r"));
        assert!(args.iter().any(|arg| arg == "--dimension=2048"));
    }

    #[tokio::test]
    async fn maintenance_stop_failure_preserves_children_and_skips_operation() {
        let temp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(temp.path().join("data"));
        data.ensure().unwrap();
        let sidecars = test_sidecars_with_data(data.clone());
        let lock = reaper::acquire_instance_lock(&data.sidecar_lock_path()).unwrap();
        *sidecars.instance_lock.lock() = Some(lock);
        let pids_path = data.sidecar_pids_path();
        std::fs::write(&pids_path, "keep-pid-record\n").unwrap();
        let child = Command::new("sleep").arg("60").spawn().unwrap();
        *sidecars.state.lock() = Some(vec![Process {
            name: "jobworkerp",
            child,
            port: 39_010,
        }]);
        sidecars.fail_next_stop_for_test();
        let called = std::sync::atomic::AtomicBool::new(false);

        let result = sidecars
            .with_maintenance_lock(|| async {
                called.store(true, Ordering::Release);
                Ok(())
            })
            .await;

        assert!(result.is_err());
        assert!(!called.load(Ordering::Acquire));
        assert!(sidecars.owns_instance_lock());
        assert_eq!(
            std::fs::read_to_string(pids_path).unwrap(),
            "keep-pid-record\n"
        );
        let mut state = sidecars.state.lock();
        let processes = state.as_mut().expect("failed child stays tracked");
        assert_eq!(processes.len(), 1);
        assert!(processes[0].child.try_wait().unwrap().is_none());
    }

    #[tokio::test]
    async fn selected_stop_failure_is_visible_and_prevents_settings_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(temp.path().join("data"));
        let sidecars = test_sidecars_with_data(data);
        seed_running_sidecars_for_test(&sidecars);
        sidecars.fail_next_stop_for_test();
        let before_start_called = std::sync::atomic::AtomicBool::new(false);

        let error = sidecars
            .restart_plan_with(RestartPlan::jobworkerp_only(), Vec::new(), || {
                before_start_called.store(true, Ordering::Release);
                Ok(())
            })
            .await
            .unwrap_err();

        assert!(error.to_string().contains("injected stop failure"));
        assert!(!before_start_called.load(Ordering::Acquire));
        let state = sidecars.state.lock();
        let processes = state.as_ref().expect("all children remain tracked");
        assert_eq!(processes.len(), 3);
        assert!(processes.iter().any(|process| process.name == "jobworkerp"));
    }

    #[tokio::test]
    async fn embedding_stop_failure_keeps_prepared_journal_and_never_restarts_children() {
        let temp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(temp.path().join("data"));
        data.ensure().unwrap();
        let sidecars = test_sidecars_with_data(data.clone());
        let lock = reaper::acquire_instance_lock(&data.sidecar_lock_path()).unwrap();
        *sidecars.instance_lock.lock() = Some(lock);
        seed_running_sidecars_for_test(&sidecars);
        sidecars.fail_next_stop_for_test();
        let journal_path = data.root.join("embedding-migration-journal.json");
        let before_start_called = std::sync::atomic::AtomicBool::new(false);

        let result = sidecars
            .embedding_migration_transaction(
                Vec::new(),
                || async {
                    std::fs::write(&journal_path, "prepared; enabled scheduler ids=[17]")?;
                    Ok(())
                },
                || async {
                    before_start_called.store(true, Ordering::Release);
                    Ok(EmbeddingMaintenanceDecision::Start(
                        "unexpected".to_string(),
                    ))
                },
                |_| async { Ok(()) },
            )
            .await;

        assert!(result.is_err());
        assert!(!before_start_called.load(Ordering::Acquire));
        assert_eq!(
            std::fs::read_to_string(journal_path).unwrap(),
            "prepared; enabled scheduler ids=[17]"
        );
        let mut state = sidecars.state.lock();
        let processes = state.as_mut().expect("children remain tracked");
        assert_eq!(processes.len(), 3);
        assert!(
            processes
                .iter_mut()
                .all(|process| process.child.try_wait().unwrap().is_none())
        );
    }

    #[tokio::test]
    async fn stop_child_accepts_already_exited_child_and_stops_live_child() {
        let mut exited = Command::new("true").spawn().unwrap();
        let _ = exited.wait().await.unwrap();
        stop_child(&mut exited, "already-exited").await.unwrap();

        let mut live = Command::new("sleep").arg("60").spawn().unwrap();
        stop_child(&mut live, "live").await.unwrap();
        assert!(live.try_wait().unwrap().is_some());
    }

    fn seed_running_sidecars_for_test(sidecars: &Sidecars) {
        let jobworkerp = Command::new("sleep").arg("60").spawn().unwrap();
        let memories = Command::new("sleep").arg("60").spawn().unwrap();
        let conductor = Command::new("sleep").arg("60").spawn().unwrap();
        *sidecars.state.lock() = Some(vec![
            Process {
                name: "jobworkerp",
                child: jobworkerp,
                port: 39_000,
            },
            Process {
                name: "memories",
                child: memories,
                port: 39_001,
            },
            Process {
                name: "conductor",
                child: conductor,
                port: 39_002,
            },
        ]);
    }

    async fn wait_for_issue_tracker_availability(
        availability: &crate::commands::IssueTrackerAvailability,
        expected_ready: bool,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if availability.require_ready().is_ok() == expected_ready {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Issue Tracker availability did not become {expected_ready}: {:?}",
                availability.snapshot()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    #[ignore = "requires a staged issue-tracker-server; opt in with LOOKBACK_ISSUE_TRACKER_SERVER_BIN"]
    async fn issue_tracker_sidecar_maintenance_recovers_after_server_restart() {
        let binary = PathBuf::from(
            std::env::var("LOOKBACK_ISSUE_TRACKER_SERVER_BIN")
                .expect("LOOKBACK_ISSUE_TRACKER_SERVER_BIN is required"),
        );
        let temp = tempfile::tempdir().expect("temporary data root");
        let data = DataPaths::with_root(temp.path());
        let mut sidecars = test_sidecars_with_data(data.clone());
        sidecars.config.issue_tracker_server_bin = Some(binary.clone());
        let availability = Arc::new(crate::commands::IssueTrackerAvailability::default());
        sidecars.bind_issue_tracker_availability(Arc::clone(&availability));

        let port = ports::pick(0).expect("reserve Issue Tracker port");
        let config = crate::grpc::issue_tracker::render_server_config(&data, port)
            .expect("render Issue Tracker config");
        std::fs::write(config.config_path(), config.toml()).expect("write Issue Tracker config");
        let child = sidecars
            .spawn_issue_tracker_server(&binary, config.config_path(), "warn")
            .expect("spawn Issue Tracker server");
        *sidecars.state.lock() = Some(vec![Process {
            name: "issue-tracker",
            child,
            port,
        }]);

        wait_for_tcp_or_failure(("127.0.0.1", port), Duration::from_secs(5), None)
            .await
            .expect("Issue Tracker listener");
        let service =
            crate::grpc::issue_tracker::IssueTrackerGrpcService::new(config.primary_url());
        service
            .run_outbox_cycle("issue-tracker-sidecar-e2e-bootstrap")
            .await
            .expect("bootstrap outbox");
        Sidecars::wait_for_issue_tracker_health(&service, &config.primary_url())
            .await
            .expect("manager readiness");
        *sidecars.issue_tracker_port.lock() = Some(port);
        sidecars.mark_issue_tracker_ready();
        {
            let _env_guard = crate::commands::issue_tracker_settings::ISSUE_TRACKER_ENV_TEST_LOCK
                .lock()
                .unwrap();
            let previous_interval =
                std::env::var_os("LOOKBACK_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS");
            unsafe {
                std::env::set_var("LOOKBACK_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS", "1");
            }
            sidecars.spawn_issue_tracker_maintenance(
                service,
                port,
                config.gateway_control_url(),
                data.root.clone(),
            );
            unsafe {
                match previous_interval {
                    Some(value) => {
                        std::env::set_var("LOOKBACK_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS", value)
                    }
                    None => {
                        std::env::remove_var("LOOKBACK_ISSUE_TRACKER_MAINTENANCE_INTERVAL_SECS")
                    }
                }
            }
        }
        wait_for_issue_tracker_availability(&availability, true).await;

        let mut crashed = sidecars
            .state
            .lock()
            .as_mut()
            .expect("tracked sidecar process")
            .pop()
            .expect("Issue Tracker process")
            .child;
        crashed.kill().await.expect("stop Issue Tracker server");
        crashed.wait().await.expect("reap Issue Tracker server");
        wait_for_issue_tracker_availability(&availability, false).await;
        *sidecars.issue_tracker_port.lock() = None;

        let child = sidecars
            .spawn_issue_tracker_server(&binary, config.config_path(), "warn")
            .expect("restart Issue Tracker server");
        sidecars
            .state
            .lock()
            .as_mut()
            .expect("tracked sidecar process")
            .push(Process {
                name: "issue-tracker",
                child,
                port,
            });
        wait_for_tcp_or_failure(("127.0.0.1", port), Duration::from_secs(5), None)
            .await
            .expect("restarted Issue Tracker listener");
        wait_for_issue_tracker_availability(&availability, true).await;
        assert_eq!(
            sidecars.issue_tracker_url().as_deref(),
            Some(format!("http://127.0.0.1:{port}").as_str()),
            "maintenance recovery must restore the endpoint advertised to the UI"
        );

        sidecars.stop().await.expect("stop Issue Tracker lifecycle");
        assert!(sidecars.issue_tracker_maintenance_task.lock().is_none());
        assert!(sidecars.issue_tracker_url().is_none());
    }

    #[test]
    fn authenticated_mcp_env_contains_only_the_auth_token_file() {
        let secrets = McpLaunchSecrets {
            auth_token_file: PathBuf::from("/private/auth-token"),
        };
        let token = "token-value-must-not-cross-the-file-boundary";

        let env: std::collections::HashMap<_, _> = mcp_launch_env_vars(true, Some(&secrets))
            .into_iter()
            .collect();
        assert_eq!(
            env.get("MCP_AUTH_ENABLED").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            env.get("MCP_AUTH_TOKEN_FILE").map(String::as_str),
            Some("/private/auth-token")
        );
        assert!(!env.values().any(|value| value == token));
    }

    #[test]
    fn unauthenticated_mcp_env_explicitly_disables_auth_without_a_secret_file() {
        let env: std::collections::HashMap<_, _> =
            mcp_launch_env_vars(false, None).into_iter().collect();
        assert_eq!(
            env.get("MCP_AUTH_ENABLED").map(String::as_str),
            Some("false")
        );
        assert!(!env.contains_key("MCP_AUTH_TOKEN_FILE"));
    }

    #[test]
    fn authenticated_mcp_without_a_secret_file_fails_closed() {
        let env: std::collections::HashMap<_, _> =
            mcp_launch_env_vars(true, None).into_iter().collect();
        assert_eq!(
            env.get("MCP_AUTH_ENABLED").map(String::as_str),
            Some("true")
        );
        assert!(!env.contains_key("MCP_AUTH_TOKEN_FILE"));
    }

    #[test]
    fn unauthenticated_mcp_never_requires_launch_secrets() {
        assert!(mcp_auth_enabled(true, false));
        assert!(!mcp_auth_enabled(true, true));
        assert!(!mcp_auth_enabled(false, false));
    }

    #[test]
    fn mcp_secret_files_are_removed_when_launch_guard_is_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let auth_token_file = directory.path().join("auth-token");
        std::fs::write(&auth_token_file, "token").unwrap();

        {
            let _secrets = McpLaunchSecrets {
                auth_token_file: auth_token_file.clone(),
            };
        }

        assert!(!auth_token_file.exists());
    }

    #[test]
    fn mcp_listener_is_advertised_only_after_a_successful_health_check() {
        assert_eq!(mcp_listener_port(true, true, 39010), Some(39010));
        assert_eq!(mcp_listener_port(true, false, 39010), None);
        assert_eq!(mcp_listener_port(false, true, 39010), None);
    }

    #[test]
    fn unavailable_mcp_listener_becomes_a_non_fatal_warning() {
        let warning = mcp_unavailable_warning(&AppError::Config("port conflict".into()));
        assert_eq!(warning.kind, SidecarWarningKind::McpUnavailable);
        assert!(warning.message.contains("port conflict"));
        assert!(warning.detail.is_none());
    }

    #[test]
    fn selected_restart_listener_failure_is_fatal_during_token_rotation() {
        let mut warnings = Vec::new();
        let error = selected_restart_mcp_listener_readiness(
            true,
            Err(AppError::SidecarNotReady(
                "listener port conflict".to_string(),
            )),
            &mut warnings,
        )
        .expect_err("token rotation must roll back when its listener is unavailable");

        assert!(error.to_string().contains("listener port conflict"));
        assert!(warnings.is_empty());
    }

    #[test]
    fn selected_restart_listener_failure_without_token_rotation_remains_a_warning() {
        let mut warnings = Vec::new();
        let ready = selected_restart_mcp_listener_readiness(
            false,
            Err(AppError::SidecarNotReady(
                "listener port conflict".to_string(),
            )),
            &mut warnings,
        )
        .expect("ordinary MCP enablement must continue with a warning");

        assert!(!ready);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].kind, SidecarWarningKind::McpUnavailable);
        assert!(warnings[0].message.contains("listener port conflict"));
    }

    #[test]
    fn selected_restart_listener_success_is_ready_during_token_rotation() {
        let mut warnings = Vec::new();
        let ready = selected_restart_mcp_listener_readiness(true, Ok(()), &mut warnings)
            .expect("a ready replacement listener must allow token rotation");

        assert!(ready);
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn stop_removes_active_mcp_secret_files() {
        let directory = tempfile::tempdir().unwrap();
        let auth_token_file = directory.path().join("auth-token");
        std::fs::write(&auth_token_file, "token").unwrap();

        let sidecars = test_sidecars();
        *sidecars.active_mcp_secrets.lock() = Some(McpLaunchSecrets {
            auth_token_file: auth_token_file.clone(),
        });
        sidecars.stop().await.unwrap();

        assert!(!auth_token_file.exists());
    }

    /// Writes an executable fake `memories-db-migrate` from a shell body.
    #[cfg(unix)]
    fn fake_migration_tool(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let tool = dir.join("memories-db-migrate");
        std::fs::write(&tool, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        tool
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn migration_activity_is_visible_only_while_the_tool_changes_data() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().join("data"));
        data.ensure().unwrap();
        let changing = tmp.path().join("changing");
        let tool = fake_migration_tool(
            tmp.path(),
            &format!(
                "echo 'local_progress stage=plan'\nsleep 0.3\necho 'local_progress stage=post_migrate'\n: > '{}'\nsleep 0.5\necho 'local_apply status=completed outcome=migrated'",
                changing.display()
            ),
        );
        let sidecars = std::sync::Arc::new(test_sidecars_with_migration(data, tool));

        let start = {
            let sidecars = std::sync::Arc::clone(&sidecars);
            tokio::spawn(async move { sidecars.start().await })
        };
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !sidecars.database_migration_in_progress(),
            "inspection stages must not claim a migration"
        );
        for _ in 0..200 {
            if changing.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(sidecars.database_migration_in_progress());

        assert!(
            start.await.unwrap().is_err(),
            "missing sidecars fail after migration"
        );
        assert!(!sidecars.database_migration_in_progress());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn migration_tool_receives_the_users_backup_choice() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().join("data"));
        data.ensure().unwrap();
        let args_log = tmp.path().join("args");
        let tool = fake_migration_tool(
            tmp.path(),
            &format!(
                "printf '%s\\n' \"$@\" > '{}'\necho 'local_apply status=completed outcome=no_op'",
                args_log.display()
            ),
        );
        let sidecars = test_sidecars_with_migration(data.clone(), tool);

        assert!(sidecars.start().await.is_err());
        let default_args = std::fs::read_to_string(&args_log).unwrap();
        assert_eq!(
            default_args.lines().collect::<Vec<_>>(),
            [
                "local",
                "apply",
                "--maintenance-window-ack",
                "--backup-dir",
                data.database_migration_backups_dir().to_str().unwrap(),
                "--backup-keep",
                "1",
            ]
        );

        crate::data::paths::save_app_settings(
            &data.app_settings_path(),
            &crate::data::paths::AppSettings {
                migration_backup: crate::data::paths::MigrationBackupSettings {
                    enabled: false,
                    keep: 4,
                },
                ..Default::default()
            },
        )
        .unwrap();
        assert!(sidecars.start().await.is_err());
        let unsafe_args = std::fs::read_to_string(&args_log).unwrap();
        assert_eq!(
            unsafe_args.lines().collect::<Vec<_>>(),
            [
                "local",
                "apply",
                "--maintenance-window-ack",
                "--no-backup-unsafe"
            ]
        );
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
            issue_tracker_server_bin: None,
            function_set_yaml_paths: Vec::new(),
            reflection_dispatch_enabled: false,
            auto_embedding_enabled: false,
            workflows_dir: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../workers/workflows"),
            ),
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
            issue_tracker_server_bin: None,
            function_set_yaml_paths: Vec::new(),
            reflection_dispatch_enabled: false,
            auto_embedding_enabled: false,
            workflows_dir: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../workers/workflows"),
            ),
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
            Err(AppError::DatabaseMigrationFailed { error_code, .. })
                if error_code == crate::sidecar::memories_migration::TOOL_UNAVAILABLE
        ));
        assert!(
            !sidecars.database_migration_in_progress(),
            "a failed migration must clear the startup UI signal"
        );
        assert!(sidecars.current_endpoints().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_starts_never_run_the_migration_tool_in_parallel() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(tmp.path().join("data"));
        data.ensure().unwrap();
        let command_log = tmp.path().join("migration-commands.log");
        let tool = fake_migration_tool(
            tmp.path(),
            &format!(
                "echo begin >> '{log}'\nsleep 0.1\necho end >> '{log}'\necho 'local_apply status=completed outcome=no_op'",
                log = command_log.display()
            ),
        );
        // Fail after the migration so both callers finish promptly.
        let sidecars = test_sidecars_with_migration(data, tool);

        let (first, second) = tokio::join!(sidecars.start(), sidecars.start());
        assert!(first.is_err());
        assert!(second.is_err());
        let commands = std::fs::read_to_string(command_log).unwrap();
        assert_eq!(
            commands.lines().collect::<Vec<_>>(),
            ["begin", "end", "begin", "end"],
            "the second start must wait for the first migration run"
        );
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
                "#!/bin/sh\nif [ ! -f '{}' ]; then : > '{}'; sleep 0.5; fi\necho 'local_apply status=completed outcome=no_op'\n",
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
        assert!(started_marker.exists(), "startup migration did not begin");
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
                jobworkerp_port: Some(9000),
                issue_tracker_port: None,
                memories_port: Some(9010),
                conductor_port: Some(9020),
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
            issue_tracker_server_bin: None,
            function_set_yaml_paths: Vec::new(),
            reflection_dispatch_enabled: false,
            auto_embedding_enabled: true,
            workflows_dir: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../workers/workflows"),
            ),
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
        let schema =
            AppError::SidecarStartupFailed(Box::new(StartupFailure::LancedbSchemaMismatch {
                table: "memories".into(),
                uri: "/x".into(),
                expected_dim: 2048,
                actual_dim: 768,
                expected_fingerprint: String::new(),
                actual_fingerprint: String::new(),
            }));
        let got = plan_degraded_retry(&schema).expect("schema mismatch is recoverable");
        assert_eq!(got.reason, "lancedb_schema_mismatch");
        assert_eq!((got.expected_dim, got.actual_dim), (2048, 768));

        let dim =
            AppError::SidecarStartupFailed(Box::new(StartupFailure::EmbeddingDimensionMismatch {
                expected_dim: 1024,
                actual_dim: 512,
                runner_name: "memories-mm-embedding".into(),
            }));
        let got = plan_degraded_retry(&dim).expect("embedding dim mismatch is recoverable");
        assert_eq!(got.reason, "embedding_dimension_mismatch");
        assert_eq!((got.expected_dim, got.actual_dim), (1024, 512));
    }

    #[tokio::test]
    async fn memories_start_retries_once_with_vectors_disabled_for_dimension_mismatch() {
        use crate::sidecar::startup_error::StartupFailure;

        let retry_called = Arc::new(AtomicBool::new(false));
        let called_from_retry = Arc::clone(&retry_called);
        let initial_failure =
            AppError::SidecarStartupFailed(Box::new(StartupFailure::EmbeddingDimensionMismatch {
                expected_dim: 1024,
                actual_dim: 512,
                runner_name: "memories-mm-embedding".into(),
            }));

        let degraded = retry_degraded_memories_start(Err(initial_failure), move || async move {
            called_from_retry.store(true, Ordering::Release);
            Ok(())
        })
        .await
        .expect("dimension mismatch must retry with vectors disabled");

        assert!(retry_called.load(Ordering::Acquire));
        assert_eq!(
            degraded,
            Some(DegradedInfo {
                reason: "embedding_dimension_mismatch",
                expected_dim: 1024,
                actual_dim: 512,
            })
        );
    }

    #[tokio::test]
    async fn memories_start_does_not_retry_non_degraded_failures() {
        use crate::sidecar::startup_error::StartupFailure;

        let retry_called = Arc::new(AtomicBool::new(false));
        let called_from_retry = Arc::clone(&retry_called);
        let initial_failure =
            AppError::SidecarStartupFailed(Box::new(StartupFailure::LancedbInitFailed {
                uri: "/x".into(),
                message: "permission denied".into(),
            }));

        let error = retry_degraded_memories_start(Err(initial_failure), move || async move {
            called_from_retry.store(true, Ordering::Release);
            Ok(())
        })
        .await
        .expect_err("non-dimension failures must be returned without a retry");

        assert!(matches!(error, AppError::SidecarStartupFailed(_)));
        assert!(!retry_called.load(Ordering::Acquire));
    }

    #[test]
    fn plan_degraded_retry_leaves_other_failures_fatal() {
        use crate::sidecar::startup_error::StartupFailure;
        // Everything that is NOT a dimension mismatch stays fatal: a
        // vector-disabled restart would not fix these and would hide the
        // real problem behind a degraded banner.
        let fatal = [
            AppError::SidecarStartupFailed(Box::new(StartupFailure::LancedbInitFailed {
                uri: "/x".into(),
                message: "permission denied".into(),
            })),
            AppError::SidecarStartupFailed(Box::new(StartupFailure::RdbPoolInitFailed {
                url_sanitized: "sqlite:".into(),
                message: "unable to open".into(),
            })),
            AppError::SidecarStartupFailed(Box::new(StartupFailure::EnvVarInvalid {
                name: "GRPC_ADDR".into(),
                message: "bad".into(),
            })),
            AppError::SidecarStartupFailed(Box::new(StartupFailure::MediaConfigConflict {
                backend: "inline".into(),
                image_search_mode: "clip".into(),
            })),
            AppError::SidecarStartupFailed(Box::new(StartupFailure::ConfigLoadFailed {
                component: "VectorDBConfig".into(),
                message: "x".into(),
            })),
            AppError::SidecarStartupFailed(Box::new(StartupFailure::Other {
                component: "front".into(),
                message: "x".into(),
            })),
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
        let payload = SidecarErrorPayload::from_app_error(&AppError::SidecarStartupFailed(
            Box::new(failure.clone()),
        ));
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
                jobworkerp_port: Some(9000),
                issue_tracker_port: None,
                memories_port: Some(9010),
                conductor_port: Some(9020),
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
    fn issue_tracker_availability_binding_receives_lifecycle_updates() {
        let sidecars = test_sidecars();
        let availability = Arc::new(crate::commands::IssueTrackerAvailability::default());
        sidecars.bind_issue_tracker_availability(Arc::clone(&availability));

        sidecars.mark_issue_tracker_unavailable("worker registration failed");
        assert!(matches!(
            availability.require_ready(),
            Err(AppError::SidecarNotReady(message)) if message.contains("worker registration failed")
        ));

        sidecars.mark_issue_tracker_ready();
        assert!(availability.require_ready().is_ok());
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
            SidecarWarningKind::RemoteJobworkerpUnavailable,
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
        let root = tempfile::tempdir().unwrap();
        let data = DataPaths::with_root(root.path().to_path_buf());
        let lance_home = data.lance_language_model_home();
        let sidecars = Sidecars::new(SidecarConfig {
            jobworkerp_bin: PathBuf::from("/nonexistent/all-in-one-xyz"),
            memories_bin: PathBuf::from("/nonexistent/front-xyz"),
            memories_db_migrate_bin: PathBuf::from("/bin/true"),
            startup_database_migration_enabled: false,
            conductor_bin: PathBuf::from("/nonexistent/conductor-main-xyz"),
            data: data.clone(),
            worker_yaml_paths: Vec::new(),
            issue_tracker_server_bin: None,
            function_set_yaml_paths: Vec::new(),
            reflection_dispatch_enabled: false,
            auto_embedding_enabled: false,
            workflows_dir: Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../workers/workflows"),
            ),
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
        assert!(
            !sidecars.owns_instance_lock(),
            "the outer startup boundary must release its lock after any error"
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

    #[cfg(target_os = "linux")]
    #[test]
    fn jobworkerp_msb_home_is_private_and_socket_path_is_bounded() {
        let paths = jobworkerp_msb_sdk_paths(Path::new("/home/sutr"));

        assert_eq!(
            paths.home,
            PathBuf::from("/home/sutr/.local/share/lookback/msb")
        );
        assert_eq!(
            paths.config_path,
            PathBuf::from("/home/sutr/.local/share/lookback/msb/config.json")
        );
        assert!(paths.representative_socket_path_supported);

        let long_home = PathBuf::from(format!("/{}", "h".repeat(180)));
        let long_paths = jobworkerp_msb_sdk_paths(&long_home);
        assert!(!long_paths.representative_socket_path_supported);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn jobworkerp_msb_environment_overrides_child_dotenv_without_mutating_parent() {
        let directory = tempfile::tempdir().unwrap();
        let env_file = directory.path().join("hostile.env");
        std::fs::write(
            &env_file,
            concat!(
                "MSB_HOME=/tmp/hostile-msb-home\n",
                "MSB_CONFIG_PATH=/tmp/hostile-config.json\n",
                "MSB_PATH=/tmp/hostile-msb\n",
                "MSB_LIBKRUNFW_PATH=/tmp/hostile-libkrunfw.so\n",
                "MSB_AGENTD_PATH=/tmp/hostile-agentd\n",
            ),
        )
        .unwrap();

        let process_env_before = MSB_CHILD_ENV_KEYS.map(|key| (key, std::env::var_os(key)));
        let data = DataPaths::with_root(directory.path().join("lookback-data"));
        let mut sidecars = test_sidecars_with_data(data);
        sidecars.config.env_file = Some(env_file);
        let mut command = sidecars.base_cmd(Path::new("/bin/sh"), directory.path(), "warn");
        let paths = jobworkerp_msb_sdk_paths(Path::new("/home/sutr"));
        apply_jobworkerp_msb_sdk_environment(&mut command, Some(&paths));
        command.args([
            "-c",
            "printf '%s\\n' \"$MSB_HOME\" \"$MSB_CONFIG_PATH\" \"${MSB_PATH-unset}\" \"${MSB_LIBKRUNFW_PATH-unset}\" \"${MSB_AGENTD_PATH-unset}\"",
        ]);

        let output = command.output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            concat!(
                "/home/sutr/.local/share/lookback/msb\n",
                "/home/sutr/.local/share/lookback/msb/config.json\n",
                "unset\n",
                "unset\n",
                "unset\n",
            )
        );
        assert_eq!(
            MSB_CHILD_ENV_KEYS.map(|key| (key, std::env::var_os(key))),
            process_env_before
        );
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

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn memories_child_preserves_shared_logging_policy_over_env_file() {
        let directory = tempfile::tempdir().unwrap();
        let env_file = directory.path().join("logging.env");
        std::fs::write(
            &env_file,
            concat!(
                "LOG_APP_NAME=jobworkerp\n",
                "LOG_FILE_DIR=./log/\n",
                "LOG_USE_JSON=false\n",
                "LOG_USE_STDOUT=false\n",
                "LOG_LEVEL=debug\n",
                "RUST_LOG=trace\n",
            ),
        )
        .unwrap();
        let data = DataPaths::with_root(directory.path().join("lookback-data"));
        data.ensure().unwrap();
        let mut sidecars = test_sidecars_with_data(data.clone());
        sidecars.config.env_file = Some(env_file);
        let runtime = sidecars.current_embedding_runtime();
        let mut command = sidecars.base_cmd(Path::new("/bin/sh"), directory.path(), "warn");
        let expected_log_dir = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("LOG_FILE_DIR"))
            .unwrap()
            .1
            .unwrap()
            .to_string_lossy()
            .into_owned();
        apply_memories_child_env(
            &mut command,
            sidecars.memories_migration_extra_env(
                &data,
                &runtime,
                &directory.path().join("embedding.yaml"),
                &data.memories_sqlite_url(),
                true,
            ),
        );
        command.args(["-c", "printf '%s\\n' \"$LOG_APP_NAME\" \"$LOG_FILE_DIR\" \"$LOG_USE_JSON\" \"$LOG_USE_STDOUT\" \"$LOG_LEVEL\" \"$RUST_LOG\""]);
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let expected = [
            ("LOG_APP_NAME", "Lookback"),
            ("LOG_FILE_DIR", expected_log_dir.as_str()),
            ("LOG_USE_JSON", "true"),
            ("LOG_USE_STDOUT", "true"),
            ("LOG_LEVEL", "debug"),
            ("RUST_LOG", "warn"),
        ];
        let values: Vec<_> = stdout.lines().collect();
        assert_eq!(
            values.len(),
            expected.len(),
            "unexpected child output: {stdout:?}"
        );
        for (actual, (key, expected_value)) in values.iter().zip(expected) {
            assert_eq!(*actual, expected_value, "unexpected child value for {key}");
        }
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

    #[test]
    fn token_mutation_is_locked_for_the_entire_mcp_launch_lifetime() {
        let sidecars = test_sidecars();
        assert!(sidecars.mcp_token_mutation_allowed());
        sidecars.reserve_mcp_token_for_launch(true).unwrap();
        assert!(!sidecars.mcp_token_mutation_allowed());
        assert!(sidecars.with_mcp_token_mutation(|| Ok(())).is_err());
        sidecars.release_mcp_token_after_stop();
        assert!(sidecars.mcp_token_mutation_allowed());
    }

    #[test]
    fn unauthenticated_mcp_launch_does_not_lock_token_mutation() {
        let sidecars = test_sidecars();
        let reservation = sidecars
            .mcp_launch_reservation(mcp_auth_enabled(true, true))
            .unwrap();

        assert!(
            sidecars.mcp_token_mutation_allowed(),
            "a listener that does not read a token must not block token generation or rotation"
        );
        sidecars
            .with_mcp_token_mutation(|| Ok(()))
            .expect("unauthenticated MCP must allow a standalone token mutation");
        drop(reservation);
    }

    #[test]
    fn unauthenticated_mcp_restart_releases_the_rotation_reservation() {
        let sidecars = test_sidecars();
        let mut rotation = sidecars.reserve_mcp_token_mutation_for_restart().unwrap();
        let launch = rotation
            .promote_to_launch(mcp_auth_enabled(true, true))
            .unwrap();

        drop(launch);
        drop(rotation);
        assert!(
            sidecars.mcp_token_mutation_allowed(),
            "an unauthenticated replacement must not retain a token reservation after restart"
        );
    }

    #[test]
    fn token_mutation_reservation_excludes_launch_reservation() {
        let sidecars = test_sidecars();
        sidecars
            .with_mcp_token_mutation(|| {
                assert!(sidecars.reserve_mcp_token_for_launch(true).is_err());
                Ok(())
            })
            .unwrap();
        assert!(sidecars.reserve_mcp_token_for_launch(true).is_ok());
        sidecars.release_mcp_token_after_stop();
    }

    #[test]
    fn settings_token_rotation_serializes_stop_launch_and_response_token() {
        let _keyring_guard = crate::commands::mcp_settings::MCP_KEYRING_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::commands::mcp_settings::configure_mcp_auth_token_for_test();
        let sidecars = test_sidecars();

        // A running listener owns the normal launch reservation. The save
        // upgrades it before stop, so releasing the old listener cannot open
        // a keyring-mutation window.
        sidecars.reserve_mcp_token_for_launch(true).unwrap();
        let mut rotation = sidecars.reserve_mcp_token_mutation_for_restart().unwrap();
        sidecars.release_mcp_token_after_stop();
        assert!(
            sidecars
                .with_mcp_token_mutation(|| {
                    crate::commands::mcp_settings::rotate_mcp_auth_token_after_stop()
                })
                .is_err(),
            "an external rotation must be rejected after the old listener stops"
        );

        let response_token = crate::commands::mcp_settings::rotate_mcp_auth_token_after_stop()
            .expect("the settings reservation owns this rotation");
        let launch_reservation = rotation.promote_to_launch(true).unwrap();
        assert!(
            sidecars
                .with_mcp_token_mutation(|| {
                    crate::commands::mcp_settings::rotate_mcp_auth_token_after_stop()
                })
                .is_err(),
            "an external rotation must stay rejected while the replacement launches"
        );
        let listener_token = crate::commands::mcp_settings::require_mcp_auth_token()
            .expect("the replacement listener reads the rotated token");
        assert_eq!(listener_token, response_token);

        launch_reservation.commit();
        sidecars.release_mcp_token_after_stop();
        crate::commands::mcp_settings::delete_mcp_auth_token().unwrap();
    }

    #[test]
    fn failed_pre_spawn_mcp_setup_releases_its_token_reservation() {
        let sidecars = test_sidecars();
        {
            let _reservation = sidecars.mcp_launch_reservation(true).unwrap();
            assert!(!sidecars.mcp_token_mutation_allowed());
        }
        assert!(sidecars.mcp_token_mutation_allowed());
    }

    #[tokio::test]
    async fn failed_start_cleanup_stops_published_mcp_listener_and_releases_reservation() {
        let directory = tempfile::tempdir().unwrap();
        let auth_token_file = directory.path().join("auth-token");
        std::fs::write(&auth_token_file, "token").unwrap();
        let sidecars = test_sidecars();
        sidecars.reserve_mcp_token_for_launch(true).unwrap();
        *sidecars.state.lock() = Some(Vec::new());
        *sidecars.active_mcp_port.lock() = Some(crate::commands::mcp_settings::MCP_DEFAULT_PORT);
        *sidecars.active_mcp_secrets.lock() = Some(McpLaunchSecrets {
            auth_token_file: auth_token_file.clone(),
        });

        sidecars.cleanup_failed_start().await;

        assert!(sidecars.state.lock().is_none());
        assert_eq!(*sidecars.active_mcp_port.lock(), None);
        assert!(sidecars.mcp_token_mutation_allowed());
        assert!(!auth_token_file.exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn managed_child_executable_is_a_path_not_a_process_name() {
        let mut child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let observed = managed_child_executable_path(child.id().unwrap());
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        assert_eq!(observed, Some(std::fs::canonicalize("/bin/sleep").unwrap()));
        assert!(managed_child_executable_path(0).is_none());
    }

    #[tokio::test]
    async fn managed_jobworkerp_snapshot_requires_completed_startup_and_the_same_live_child() {
        let sidecars = test_sidecars();
        let child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let child_pid = child.id().unwrap();
        let snapshot = ManagedJobworkerpSpawnSnapshot {
            generation_id: 42,
            child_pid,
            configured_binary_path: PathBuf::from("/bin/sleep"),
            actual_running_executable: None,
            applied_sdk_home: Some(PathBuf::from("/native/home/.local/share/lookback/msb")),
            applied_sdk_config_path: Some(PathBuf::from(
                "/native/home/.local/share/lookback/msb/config.json",
            )),
            child_home: Some(PathBuf::from("/gui/home")),
            child_home_source: ManagedChildHomeSource::ExplicitlySet,
            effective_sdk_home: None,
            runtime_observation: ManagedJobworkerpRuntimeObservation::NotObserved,
            sdk_config_source: ManagedJobworkerpSdkSourceStamp::Missing {
                selected_path: PathBuf::from("/native/home/.local/share/lookback/msb/config.json"),
            },
        };
        *sidecars.state.lock() = Some(vec![Process {
            name: "jobworkerp",
            child,
            port: 39_000,
        }]);
        *sidecars.last_report.lock() = Some(SidecarStartReport {
            endpoints: SidecarEndpoints {
                jobworkerp_port: Some(39_000),
                issue_tracker_port: None,
                memories_port: None,
                conductor_port: Some(39_002),
                mcp_server_port: None,
            },
            warnings: Vec::new(),
        });
        sidecars.record_jobworkerp_spawn(
            snapshot.clone(),
            ManagedJobworkerpSdkSourceStamp::Missing {
                selected_path: PathBuf::from("/etc/microsandbox/managed.json"),
            },
        );

        assert!(
            sidecars.managed_jobworkerp_spawn_snapshot().is_none(),
            "spawn facts are not a startup-success or runtime-readiness claim"
        );

        sidecars.mark_jobworkerp_startup_complete();
        assert_eq!(
            sidecars.managed_jobworkerp_spawn_snapshot(),
            Some(snapshot),
            "the completed startup facts belong to the still-live managed child"
        );

        sidecars
            .state
            .lock()
            .as_mut()
            .unwrap()
            .iter_mut()
            .find(|process| process.name == "jobworkerp")
            .unwrap()
            .child
            .start_kill()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            sidecars.managed_jobworkerp_spawn_snapshot().is_none(),
            "an exited child must not return a cached snapshot"
        );

        let replacement = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        sidecars.state.lock().as_mut().unwrap()[0].child = replacement;
        assert!(
            sidecars.managed_jobworkerp_spawn_snapshot().is_none(),
            "facts from an earlier process generation must not describe a replacement"
        );
        sidecars.stop().await.unwrap();
    }

    #[test]
    fn jobworkerp_spawn_facts_use_applied_paths_without_inferencing_sdk_runtime() {
        let mut command = Command::new("/bin/true");
        command.env("HOME", "/gui/home");
        #[cfg(target_os = "linux")]
        {
            let paths = JobworkerpMsbSdkPaths {
                home: PathBuf::from("/native/home/.local/share/lookback/msb"),
                config_path: PathBuf::from("/native/home/.local/share/lookback/msb/config.json"),
                representative_socket_path_supported: true,
            };
            apply_jobworkerp_msb_sdk_environment(&mut command, Some(&paths));
        }

        let child_home = command_child_home(&command);
        assert_eq!(child_home.path, Some(PathBuf::from("/gui/home")));
        assert_eq!(child_home.source, ManagedChildHomeSource::ExplicitlySet);

        let mut inherited_home_command = Command::new("/bin/true");
        let inherited_home = std::env::var_os("HOME").map(PathBuf::from);
        assert_eq!(
            command_child_home(&inherited_home_command),
            ManagedChildHome {
                path: inherited_home,
                source: ManagedChildHomeSource::InheritedAtSpawn,
            }
        );
        inherited_home_command.env_remove("HOME");
        assert_eq!(
            command_child_home(&inherited_home_command),
            ManagedChildHome {
                path: None,
                source: ManagedChildHomeSource::Removed,
            }
        );

        #[cfg(target_os = "linux")]
        {
            assert_eq!(
                command_path_env(&command, "MSB_HOME"),
                Some(PathBuf::from("/native/home/.local/share/lookback/msb"))
            );
            assert_eq!(
                command_path_env(&command, "MSB_CONFIG_PATH"),
                Some(PathBuf::from(
                    "/native/home/.local/share/lookback/msb/config.json"
                ))
            );
            assert_eq!(command_path_env(&command, "MSB_PATH"), None);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_sdk_source_stamp_tracks_missing_content_and_file_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("private/msb/config.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();

        let absent_home_config = directory
            .path()
            .join("not-created/.local/share/lookback/msb/config.json");
        let absent_home = capture_jobworkerp_sdk_source(Some(&absent_home_config));
        assert!(matches!(
            &absent_home,
            ManagedJobworkerpSdkSourceStamp::Missing { selected_path }
                if selected_path == &absent_home_config
        ));
        assert!(
            !absent_home_config.parent().unwrap().exists(),
            "capturing an empty-config fallback must not create SDK home directories"
        );
        std::fs::create_dir_all(absent_home_config.parent().unwrap()).unwrap();
        std::fs::write(&absent_home_config, b"{}").unwrap();
        std::fs::set_permissions(&absent_home_config, std::fs::Permissions::from_mode(0o600))
            .unwrap();
        assert_eq!(
            compare_jobworkerp_sdk_source_stamps(
                &absent_home,
                &capture_jobworkerp_sdk_source(Some(&absent_home_config))
            ),
            ManagedJobworkerpSdkSourceCheck::Changed
        );

        let missing = capture_jobworkerp_sdk_source(Some(&config));
        assert!(matches!(
            &missing,
            ManagedJobworkerpSdkSourceStamp::Missing { selected_path } if selected_path == &config
        ));
        assert_eq!(
            compare_jobworkerp_sdk_source_stamps(
                &missing,
                &capture_jobworkerp_sdk_source(Some(&config))
            ),
            ManagedJobworkerpSdkSourceCheck::Matches
        );

        std::fs::write(&config, br#"{"home":"same"}"#).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        let original = capture_jobworkerp_sdk_source(Some(&config));
        assert!(matches!(
            &original,
            ManagedJobworkerpSdkSourceStamp::Present(file)
                if file.selected_path == config && file.length == 15
        ));
        assert!(
            !format!("{original:?}").contains(r#"{"home":"same"}"#),
            "the source stamp may expose its digest, never raw config bytes"
        );

        // Replace the inode while keeping the exact bytes. A digest-only check
        // would miss this change to the source selected by the running SDK.
        let replacement_path = directory.path().join("private/msb/replacement.json");
        std::fs::write(&replacement_path, br#"{"home":"same"}"#).unwrap();
        std::fs::set_permissions(&replacement_path, std::fs::Permissions::from_mode(0o600))
            .unwrap();
        std::fs::rename(&replacement_path, &config).unwrap();
        let replaced = capture_jobworkerp_sdk_source(Some(&config));
        assert_eq!(
            compare_jobworkerp_sdk_source_stamps(&original, &replaced),
            ManagedJobworkerpSdkSourceCheck::Changed
        );

        std::fs::write(&config, br#"{"home":"other"}"#).unwrap();
        let changed_contents = capture_jobworkerp_sdk_source(Some(&config));
        assert_eq!(
            compare_jobworkerp_sdk_source_stamps(&replaced, &changed_contents),
            ManagedJobworkerpSdkSourceCheck::Changed
        );

        std::fs::remove_file(&config).unwrap();
        let removed = capture_jobworkerp_sdk_source(Some(&config));
        assert!(matches!(
            removed,
            ManagedJobworkerpSdkSourceStamp::Missing { .. }
        ));
        assert_eq!(
            compare_jobworkerp_sdk_source_stamps(&changed_contents, &removed),
            ManagedJobworkerpSdkSourceCheck::Changed
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_sdk_source_capture_re_resolves_selected_path_after_read() {
        let directory = tempfile::tempdir().unwrap();
        let sdk_home = directory.path().join("native/.local/share/lookback/msb");
        std::fs::create_dir_all(&sdk_home).unwrap();
        let config = sdk_home.join("config.json");
        std::fs::write(&config, br#"{"home":"old"}"#).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();

        // The callback runs after the source was securely read but before the
        // independent root-originated path walk. Retaining the first parent
        // FD alone would keep stamping this now-unselected old config.
        let old_sdk_home = directory
            .path()
            .join("native/.local/share/lookback/msb-old");
        let replaced_parent = capture_linux_jobworkerp_sdk_source_with_hook(&config, || {
            std::fs::rename(&sdk_home, &old_sdk_home).unwrap();
            std::fs::create_dir(&sdk_home).unwrap();
            std::fs::write(&config, br#"{"home":"new"}"#).unwrap();
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        });
        assert!(matches!(
            replaced_parent,
            ManagedJobworkerpSdkSourceStamp::Unavailable {
                reason: ManagedJobworkerpSdkSourceUnavailableReason::ChangedDuringRead
            }
        ));

        let symlink_case = tempfile::tempdir().unwrap();
        let symlink_sdk_home = symlink_case.path().join("native/.local/share/lookback/msb");
        std::fs::create_dir_all(&symlink_sdk_home).unwrap();
        let symlink_config = symlink_sdk_home.join("config.json");
        std::fs::write(&symlink_config, br#"{"home":"original"}"#).unwrap();
        std::fs::set_permissions(&symlink_config, std::fs::Permissions::from_mode(0o600)).unwrap();
        let moved_sdk_home = symlink_case
            .path()
            .join("native/.local/share/lookback/msb-moved");
        let symlink_parent = capture_linux_jobworkerp_sdk_source_with_hook(&symlink_config, || {
            std::fs::rename(&symlink_sdk_home, &moved_sdk_home).unwrap();
            std::os::unix::fs::symlink(&moved_sdk_home, &symlink_sdk_home).unwrap();
        });
        assert!(matches!(
            symlink_parent,
            ManagedJobworkerpSdkSourceStamp::Unavailable {
                reason: ManagedJobworkerpSdkSourceUnavailableReason::Symlink
                    | ManagedJobworkerpSdkSourceUnavailableReason::UnsafePathComponent
            }
        ));

        let missing_case = tempfile::tempdir().unwrap();
        let missing_config = missing_case
            .path()
            .join("not-created/.local/share/lookback/msb/config.json");
        let created_during_capture =
            capture_linux_jobworkerp_sdk_source_with_hook(&missing_config, || {
                std::fs::create_dir_all(missing_config.parent().unwrap()).unwrap();
                std::fs::write(&missing_config, b"{}").unwrap();
                std::fs::set_permissions(&missing_config, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            });
        assert!(matches!(
            created_during_capture,
            ManagedJobworkerpSdkSourceStamp::Unavailable {
                reason: ManagedJobworkerpSdkSourceUnavailableReason::ChangedDuringRead
            }
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_sdk_source_stamp_rejects_symlinks_unsafe_modes_and_oversized_files() {
        let directory = tempfile::tempdir().unwrap();
        let private = directory.path().join("private");
        std::fs::create_dir(&private).unwrap();
        let config = private.join("config.json");
        let secret = b"opaque sdk config value";
        std::fs::write(&config, secret).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        let symlink = directory.path().join("linked-config.json");
        std::os::unix::fs::symlink(&config, &symlink).unwrap();

        let symlink_stamp = capture_jobworkerp_sdk_source(Some(&symlink));
        assert!(matches!(
            symlink_stamp,
            ManagedJobworkerpSdkSourceStamp::Unavailable {
                reason: ManagedJobworkerpSdkSourceUnavailableReason::Symlink
            }
        ));
        assert!(
            !format!("{symlink_stamp:?}").contains("opaque sdk config value"),
            "source diagnostics must not retain or print config contents"
        );

        let linked_directory = directory.path().join("linked-private");
        std::os::unix::fs::symlink(&private, &linked_directory).unwrap();
        let symlink_ancestor_stamp =
            capture_jobworkerp_sdk_source(Some(&linked_directory.join("config.json")));
        assert!(matches!(
            symlink_ancestor_stamp,
            ManagedJobworkerpSdkSourceStamp::Unavailable {
                reason: ManagedJobworkerpSdkSourceUnavailableReason::Symlink
                    | ManagedJobworkerpSdkSourceUnavailableReason::UnsafePathComponent
            }
        ));

        let unsafe_config = private.join("unsafe.json");
        std::fs::write(&unsafe_config, b"{}").unwrap();
        std::fs::set_permissions(&unsafe_config, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(matches!(
            capture_jobworkerp_sdk_source(Some(&unsafe_config)),
            ManagedJobworkerpSdkSourceStamp::Unavailable {
                reason: ManagedJobworkerpSdkSourceUnavailableReason::UnsafeFilePermissions
            }
        ));

        let oversized = private.join("large.json");
        std::fs::write(&oversized, vec![b'x'; MAX_MANAGED_SDK_CONFIG_BYTES + 1]).unwrap();
        std::fs::set_permissions(&oversized, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            capture_jobworkerp_sdk_source(Some(&oversized)),
            ManagedJobworkerpSdkSourceStamp::Unavailable {
                reason: ManagedJobworkerpSdkSourceUnavailableReason::FileTooLarge
            }
        ));

        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            capture_jobworkerp_sdk_source(Some(&config)),
            ManagedJobworkerpSdkSourceStamp::Unavailable {
                reason: ManagedJobworkerpSdkSourceUnavailableReason::UnsafeDirectoryPermissions
            }
        ));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn managed_sdk_source_recheck_uses_current_managed_child_and_detects_file_change() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("private/msb/config.json");
        let managed_config = directory.path().join("machine/microsandbox/managed.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::create_dir_all(managed_config.parent().unwrap()).unwrap();
        std::fs::write(&config, br#"{"home":"first"}"#).unwrap();
        std::fs::write(&managed_config, br#"{"home":"machine-first"}"#).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&managed_config, std::fs::Permissions::from_mode(0o644)).unwrap();
        let child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let snapshot = ManagedJobworkerpSpawnSnapshot {
            generation_id: 73,
            child_pid: child.id().unwrap(),
            configured_binary_path: PathBuf::from("/bin/sleep"),
            actual_running_executable: None,
            applied_sdk_home: Some(config.parent().unwrap().to_path_buf()),
            applied_sdk_config_path: Some(config.clone()),
            child_home: None,
            child_home_source: ManagedChildHomeSource::Removed,
            effective_sdk_home: None,
            runtime_observation: ManagedJobworkerpRuntimeObservation::NotObserved,
            sdk_config_source: capture_jobworkerp_sdk_source(Some(&config)),
        };
        let sidecars = test_sidecars();
        *sidecars.state.lock() = Some(vec![Process {
            name: "jobworkerp",
            child,
            port: 39_000,
        }]);
        sidecars.record_jobworkerp_spawn(
            snapshot.clone(),
            capture_jobworkerp_sdk_source(Some(&managed_config)),
        );
        sidecars.mark_jobworkerp_startup_complete();
        let source_snapshot = sidecars
            .managed_jobworkerp_sdk_source_snapshot()
            .expect("both SDK sources are tied to the ready managed child");
        assert_eq!(source_snapshot.generation_id, snapshot.generation_id);
        assert_eq!(source_snapshot.child_pid, snapshot.child_pid);
        assert_eq!(
            source_snapshot.managed_sdk_config_source,
            capture_jobworkerp_sdk_source(Some(&managed_config))
        );
        assert_eq!(
            sidecars.managed_jobworkerp_sdk_source_check_for_test(
                &snapshot,
                config.clone(),
                managed_config.clone(),
            ),
            ManagedJobworkerpSdkSourceCheck::Matches
        );

        // The selected config stays byte-for-byte stable; changing the fixed
        // machine override alone must still invalidate the child source.
        std::fs::write(&managed_config, br#"{"home":"machine-second"}"#).unwrap();
        assert_eq!(
            sidecars.managed_jobworkerp_sdk_source_check_for_test(
                &snapshot,
                config.clone(),
                managed_config.clone(),
            ),
            ManagedJobworkerpSdkSourceCheck::Changed
        );

        std::fs::write(&config, br#"{"home":"second"}"#).unwrap();
        assert_eq!(
            sidecars.managed_jobworkerp_sdk_source_check_for_test(
                &snapshot,
                config,
                managed_config,
            ),
            ManagedJobworkerpSdkSourceCheck::Changed
        );
        sidecars.stop().await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn managed_machine_source_check_detects_delete_insert_symlink_and_unavailable_baseline() {
        async fn ready_sidecars_with_sources(
            selected: &Path,
            managed: &Path,
            generation_id: u64,
        ) -> (Sidecars, ManagedJobworkerpSpawnSnapshot) {
            let sidecars = test_sidecars();
            let child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
            let snapshot = ManagedJobworkerpSpawnSnapshot {
                generation_id,
                child_pid: child.id().unwrap(),
                configured_binary_path: PathBuf::from("/bin/sleep"),
                actual_running_executable: None,
                applied_sdk_home: selected.parent().map(Path::to_path_buf),
                applied_sdk_config_path: Some(selected.to_path_buf()),
                child_home: None,
                child_home_source: ManagedChildHomeSource::Removed,
                effective_sdk_home: None,
                runtime_observation: ManagedJobworkerpRuntimeObservation::NotObserved,
                sdk_config_source: capture_jobworkerp_sdk_source(Some(selected)),
            };
            *sidecars.state.lock() = Some(vec![Process {
                name: "jobworkerp",
                child,
                port: 39_000,
            }]);
            sidecars.record_jobworkerp_spawn(
                snapshot.clone(),
                capture_jobworkerp_sdk_source(Some(managed)),
            );
            sidecars.mark_jobworkerp_startup_complete();
            (sidecars, snapshot)
        }

        let directory = tempfile::tempdir().unwrap();
        let selected = directory.path().join("selected/msb/config.json");
        let managed = directory.path().join("machine/microsandbox/managed.json");
        let target = directory.path().join("machine/managed-target.json");
        std::fs::create_dir_all(selected.parent().unwrap()).unwrap();
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        std::fs::write(&selected, br#"{"home":"user-home"}"#).unwrap();
        std::fs::set_permissions(&selected, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&managed, br#"{"home":"machine-home"}"#).unwrap();
        std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o644)).unwrap();

        let (sidecars, snapshot) = ready_sidecars_with_sources(&selected, &managed, 91).await;
        std::fs::remove_file(&managed).unwrap();
        assert_eq!(
            sidecars.managed_jobworkerp_sdk_source_check_for_test(
                &snapshot,
                selected.clone(),
                managed.clone(),
            ),
            ManagedJobworkerpSdkSourceCheck::Changed
        );
        sidecars.stop().await.unwrap();

        // A missing managed override is a valid empty-config baseline, but it
        // cannot continue matching once the selected machine file appears.
        let (sidecars, missing_snapshot) =
            ready_sidecars_with_sources(&selected, &managed, 92).await;
        assert_eq!(
            sidecars.managed_jobworkerp_sdk_source_check_for_test(
                &missing_snapshot,
                selected.clone(),
                managed.clone(),
            ),
            ManagedJobworkerpSdkSourceCheck::Matches
        );
        std::fs::write(&managed, br#"{"home":"new-machine-home"}"#).unwrap();
        std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            sidecars.managed_jobworkerp_sdk_source_check_for_test(
                &missing_snapshot,
                selected.clone(),
                managed.clone(),
            ),
            ManagedJobworkerpSdkSourceCheck::Changed
        );
        sidecars.stop().await.unwrap();

        // Replacing an originally valid machine override with a symlink must
        // not match, even though the selected user config is unchanged.
        std::fs::write(&managed, br#"{"home":"original-machine-home"}"#).unwrap();
        std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (sidecars, present_snapshot) =
            ready_sidecars_with_sources(&selected, &managed, 95).await;
        std::fs::write(&target, br#"{"home":"symlink-target"}"#).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::remove_file(&managed).unwrap();
        std::os::unix::fs::symlink(&target, &managed).unwrap();
        assert!(matches!(
            sidecars.managed_jobworkerp_sdk_source_check_for_test(
                &present_snapshot,
                selected.clone(),
                managed.clone(),
            ),
            ManagedJobworkerpSdkSourceCheck::Unavailable(
                ManagedJobworkerpSdkSourceUnavailableReason::Symlink
                    | ManagedJobworkerpSdkSourceUnavailableReason::UnsafePathComponent
            )
        ));
        sidecars.stop().await.unwrap();

        std::fs::remove_file(&managed).unwrap();
        let (sidecars, missing_snapshot) =
            ready_sidecars_with_sources(&selected, &managed, 93).await;
        std::fs::write(&target, br#"{"home":"symlink-target"}"#).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink(&target, &managed).unwrap();
        assert!(matches!(
            sidecars.managed_jobworkerp_sdk_source_check_for_test(
                &missing_snapshot,
                selected.clone(),
                managed.clone(),
            ),
            ManagedJobworkerpSdkSourceCheck::Unavailable(
                ManagedJobworkerpSdkSourceUnavailableReason::Symlink
                    | ManagedJobworkerpSdkSourceUnavailableReason::UnsafePathComponent
            )
        ));
        sidecars.stop().await.unwrap();

        std::fs::remove_file(&managed).unwrap();
        std::fs::write(&managed, br#"{"home":"restricted"}"#).unwrap();
        std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o666)).unwrap();
        let (sidecars, unavailable_snapshot) =
            ready_sidecars_with_sources(&selected, &managed, 94).await;
        std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            sidecars.managed_jobworkerp_sdk_source_check_for_test(
                &unavailable_snapshot,
                selected,
                managed,
            ),
            ManagedJobworkerpSdkSourceCheck::Unavailable(
                ManagedJobworkerpSdkSourceUnavailableReason::UnsafeFilePermissions
            )
        );
        sidecars.stop().await.unwrap();
    }

    #[tokio::test]
    async fn native_jobworkerp_admission_guard_serializes_with_lifecycle_stop() {
        let sidecars = Arc::new(test_sidecars());
        let child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        *sidecars.state.lock() = Some(vec![Process {
            name: "jobworkerp",
            child,
            port: 39_000,
        }]);

        let admission = sidecars.lock_native_jobworkerp_admission().await;
        let sidecars_to_stop = Arc::clone(&sidecars);
        let mut stopping = tokio::spawn(async move { sidecars_to_stop.stop().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut stopping)
                .await
                .is_err(),
            "a lifecycle replacement must wait while native admission validates and opens"
        );

        drop(admission);
        stopping.await.unwrap().unwrap();
        assert!(sidecars.state.lock().is_none());
    }
}
