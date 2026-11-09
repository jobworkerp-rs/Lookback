//! Connection targets, one per service.
//!
//! jobworkerp, memories, the Issue tracker and conductor are each either the
//! app-managed local sidecar (on its dynamically chosen port) or a remote
//! server at a configured URL. A remote service must not depend on a local
//! one it cannot reach: a remote jobworkerp needs a remote memories and
//! Issue tracker, and a remote conductor needs a remote jobworkerp. The
//! preference is a small JSON file under the data root (see
//! `DataPaths::connection_config_path`); there is no general settings store
//! in this app, and pulling in `tauri-plugin-store` for it isn't worth it.

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
use crate::grpc::proto::llm_memory::service::thread_group_service_client::ThreadGroupServiceClient;
use crate::grpc::proto::llm_memory::service::thread_service_client::ThreadServiceClient;
use crate::grpc::proto::llm_memory::{data as mem_data, service as mem_svc};
use crate::sidecar::{RestartPlan, SidecarEndpoints};

use super::AppState;

/// Whether one service is the app-managed local sidecar or a remote server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionMode {
    #[default]
    Local,
    Remote,
}

/// The services whose connection target is chosen independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Jobworkerp,
    Memories,
    IssueTracker,
    Conductor,
}

/// One service's target. The remote URL is kept while the service is local so
/// the form can re-display it when the user switches back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EndpointConfig {
    pub mode: ConnectionMode,
    pub remote_url: Option<String>,
}

impl EndpointConfig {
    pub fn is_local(&self) -> bool {
        self.mode == ConnectionMode::Local
    }

    /// The configured remote URL, trimmed; `None` while it is blank.
    pub fn remote_url(&self) -> Option<&str> {
        non_empty(self.remote_url.as_deref())
    }

    /// The remote URL while the service is remote; a local service's URL is
    /// only a draft.
    pub fn active_remote_url(&self) -> Option<&str> {
        self.remote_url().filter(|_| !self.is_local())
    }

    /// The remote URL a remote service cannot work without.
    pub fn require_remote_url(&self, name: &str) -> AppResult<&str> {
        require_url(self.remote_url.as_deref(), name)
    }

    #[cfg(test)]
    pub fn remote(url: &str) -> Self {
        Self {
            mode: ConnectionMode::Remote,
            remote_url: Some(url.to_string()),
        }
    }

    /// Local, with a remote URL kept as a draft.
    #[cfg(test)]
    pub fn local_with_draft(url: &str) -> Self {
        Self {
            mode: ConnectionMode::Local,
            remote_url: Some(url.to_string()),
        }
    }
}

fn require_url<'a>(url: Option<&'a str>, name: &str) -> AppResult<&'a str> {
    non_empty(url).ok_or_else(|| AppError::Config(format!("remote {name} URL not configured")))
}

/// The Issue tracker exposes its gateway on separate listeners, so a remote
/// tracker needs those addresses too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct IssueTrackerEndpointConfig {
    #[serde(flatten)]
    pub endpoint: EndpointConfig,
    pub remote_gateway_control_url: Option<String>,
    pub remote_gateway_worker_url: Option<String>,
}

/// Addresses for the remote side to use where they differ from the ones the
/// app dials, e.g. behind a proxy. Unset values fall back to the app's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WorkerViewOverrides {
    /// jobworkerp as conductor and its own workflows reach it.
    pub jobworkerp_url: Option<String>,
    /// memories as jobworkerp workflows reach it.
    pub memories_url: Option<String>,
    /// The Issue tracker as jobworkerp workflows reach it.
    pub issue_tracker_url: Option<String>,
    /// The tracker's gateway worker listener as jobworkerp reaches it.
    pub issue_tracker_gateway_worker_url: Option<String>,
}

/// Paths on the jobworkerp host for the periodic import, which runs there and
/// reads that host's session files. Unset values fall back to this machine's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct JobworkerpHostPaths {
    pub memories_import_bin: Option<String>,
    pub opencode_db_path: Option<String>,
    pub import_working_dir: Option<String>,
}

/// Persisted connection preference, one target per service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(from = "ConnectionConfigWire")]
pub struct ConnectionConfig {
    pub jobworkerp: EndpointConfig,
    pub memories: EndpointConfig,
    pub issue_tracker: IssueTrackerEndpointConfig,
    pub conductor: EndpointConfig,
    pub worker_view: WorkerViewOverrides,
    pub jobworkerp_host_paths: JobworkerpHostPaths,
}

/// Accepts both the per-service shape and the earlier single-mode one
/// (`mode` + `remote_jobworkerp_url` + `remote_memories_url`), which set
/// jobworkerp and memories together. The legacy keys apply only where the
/// per-service entry is absent, which serde can tell only through this
/// mirror. The file is rewritten in the new shape on the next save; nothing
/// is rewritten on load.
#[derive(Deserialize, Default)]
#[serde(default)]
struct ConnectionConfigWire {
    jobworkerp: Option<EndpointConfig>,
    memories: Option<EndpointConfig>,
    issue_tracker: IssueTrackerEndpointConfig,
    conductor: EndpointConfig,
    worker_view: WorkerViewOverrides,
    jobworkerp_host_paths: JobworkerpHostPaths,
    mode: ConnectionMode,
    remote_jobworkerp_url: Option<String>,
    remote_memories_url: Option<String>,
}

impl From<ConnectionConfigWire> for ConnectionConfig {
    fn from(wire: ConnectionConfigWire) -> Self {
        let legacy = |remote_url| EndpointConfig {
            mode: wire.mode,
            remote_url,
        };
        Self {
            jobworkerp: wire
                .jobworkerp
                .unwrap_or_else(|| legacy(wire.remote_jobworkerp_url)),
            memories: wire
                .memories
                .unwrap_or_else(|| legacy(wire.remote_memories_url)),
            issue_tracker: wire.issue_tracker,
            conductor: wire.conductor,
            worker_view: wire.worker_view,
            jobworkerp_host_paths: wire.jobworkerp_host_paths,
        }
    }
}

impl ConnectionConfig {
    pub fn mode(&self, service: Service) -> ConnectionMode {
        match service {
            Service::Jobworkerp => self.jobworkerp.mode,
            Service::Memories => self.memories.mode,
            Service::IssueTracker => self.issue_tracker.endpoint.mode,
            Service::Conductor => self.conductor.mode,
        }
    }

    /// Remote jobworkerp and memories, the earlier "remote mode".
    #[cfg(test)]
    pub fn remote_pair(jobworkerp_url: &str, memories_url: &str) -> Self {
        Self {
            jobworkerp: EndpointConfig::remote(jobworkerp_url),
            memories: EndpointConfig::remote(memories_url),
            ..Self::default()
        }
    }
}

/// The concrete URLs the gRPC clients should dial after applying the override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTargets {
    pub jobworkerp_url: String,
    pub memories_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConnection {
    pub jobworkerp_mode: ConnectionMode,
    pub memories_mode: ConnectionMode,
    pub targets: ResolvedTargets,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequiredFieldKind {
    RepeatedMemoryKind,
    OptionalString,
    OptionalInt64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RequiredSchemaField {
    message: &'static str,
    field: &'static str,
    number: i32,
    kind: RequiredFieldKind,
}

const fn required_schema_field(
    message: &'static str,
    field: &'static str,
    number: i32,
    kind: RequiredFieldKind,
) -> RequiredSchemaField {
    RequiredSchemaField {
        message,
        field,
        number,
        kind,
    }
}

const REMOTE_REQUIRED_MEMORIES_SCHEMA_FIELDS: [RequiredSchemaField; 25] = [
    required_schema_field(
        "llm_memory.service.SearchReflectionsRequest",
        "cursor_after_memory_id",
        8,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindMemoryListRequest",
        "external_id_prefix",
        14,
        RequiredFieldKind::OptionalString,
    ),
    required_schema_field(
        "llm_memory.service.FindMemoryListRequest",
        "memory_kinds",
        15,
        RequiredFieldKind::RepeatedMemoryKind,
    ),
    required_schema_field(
        "llm_memory.service.MemoryCountCondition",
        "memory_kinds",
        12,
        RequiredFieldKind::RepeatedMemoryKind,
    ),
    required_schema_field(
        "llm_memory.service.MemoryCountCondition",
        "external_id_prefix",
        11,
        RequiredFieldKind::OptionalString,
    ),
    required_schema_field(
        "llm_memory.service.FindDistinctLabelsRequest",
        "memory_kinds",
        8,
        RequiredFieldKind::RepeatedMemoryKind,
    ),
    required_schema_field(
        "llm_memory.service.FindCoOccurringLabelsRequest",
        "memory_kinds",
        9,
        RequiredFieldKind::RepeatedMemoryKind,
    ),
    required_schema_field(
        "llm_memory.data.ThreadData",
        "first_message_at",
        13,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.data.ThreadData",
        "last_message_at",
        14,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByUserIdRequest",
        "thread_created_after",
        10,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByUserIdRequest",
        "thread_created_before",
        11,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByUserIdRequest",
        "thread_updated_after",
        12,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByUserIdRequest",
        "thread_updated_before",
        13,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByUserIdRequest",
        "first_message_after",
        14,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByUserIdRequest",
        "first_message_before",
        15,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByUserIdRequest",
        "last_message_after",
        16,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByUserIdRequest",
        "last_message_before",
        17,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByLabelsRequest",
        "thread_created_after",
        12,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByLabelsRequest",
        "thread_created_before",
        13,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByLabelsRequest",
        "thread_updated_after",
        14,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByLabelsRequest",
        "thread_updated_before",
        15,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByLabelsRequest",
        "first_message_after",
        16,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByLabelsRequest",
        "first_message_before",
        17,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByLabelsRequest",
        "last_message_after",
        18,
        RequiredFieldKind::OptionalInt64,
    ),
    required_schema_field(
        "llm_memory.service.FindThreadListByLabelsRequest",
        "last_message_before",
        19,
        RequiredFieldKind::OptionalInt64,
    ),
];

fn required_memories_schema_fields() -> Vec<RequiredSchemaField> {
    let mut fields = REMOTE_REQUIRED_MEMORIES_SCHEMA_FIELDS.to_vec();
    let names = [
        "thread_created_after",
        "thread_created_before",
        "thread_updated_after",
        "thread_updated_before",
        "first_message_after",
        "first_message_before",
        "last_message_after",
        "last_message_before",
    ];
    for (message, first_number) in [
        ("llm_memory.service.FindDistinctLabelsRequest", 9),
        ("llm_memory.service.SearchLabelsRequest", 8),
        ("llm_memory.service.FindCoOccurringLabelsRequest", 10),
        ("llm_memory.data.ThreadSearchFilter", 10),
    ] {
        fields.extend(names.iter().enumerate().map(|(index, name)| {
            required_schema_field(
                message,
                name,
                first_number + index as i32,
                RequiredFieldKind::OptionalInt64,
            )
        }));
    }
    fields
}

/// Confirms that a remote memories endpoint exposes the service surface
/// Lookback needs without introducing a version-maintenance RPC.
pub async fn validate_remote_memories_services(url: &str) -> AppResult<()> {
    let channel = crate::grpc::connect(url).await?;
    validate_memories_services(channel, url).await
}

/// Require the explicit ThreadGroup owner contract before owner-sensitive
/// reads or writes. This stays separate from the general remote startup probe
/// so unrelated app features can continue using older Memories servers.
pub async fn require_thread_group_owner_capability(
    channel: tonic::transport::Channel,
) -> AppResult<()> {
    let mut client = ThreadGroupServiceClient::new(channel);
    let response = client
        .get_thread_group_capabilities(tonic::Request::new(
            mem_svc::ThreadGroupCapabilitiesRequest {},
        ))
        .await
        .map(|response| response.into_inner());
    validate_thread_group_owner_capability_response(response)
}

/// Workflow callbacks bypass the app's cached gRPC channel, so dispatch must
/// verify the same capability against the exact callback endpoint.
pub async fn require_thread_group_owner_capability_at_url(url: &str) -> AppResult<()> {
    let channel = grpc::connect(url).await?;
    require_thread_group_owner_capability(channel).await
}

fn validate_thread_group_owner_capability_response(
    response: Result<mem_svc::ThreadGroupCapabilitiesResponse, tonic::Status>,
) -> AppResult<()> {
    match response {
        Ok(response) if response.supports_independent_group_owner_scope => Ok(()),
        Ok(_) => Err(AppError::Config(
            "connected Memories server does not support independent ThreadGroup group ownership; upgrade the server before using owner-scoped ThreadGroup operations".into(),
        )),
        Err(status) if status.code() == tonic::Code::Unimplemented => Err(AppError::Config(
            format!(
                "connected Memories server does not support independent ThreadGroup group ownership (UNIMPLEMENTED: {}); upgrade the server before using owner-scoped ThreadGroup operations",
                status.message()
            ),
        )),
        Err(status) => Err(AppError::Config(format!(
            "could not verify independent ThreadGroup group ownership support: gRPC code={:?}, message={}",
            status.code(),
            status.message()
        ))),
    }
}

async fn validate_memories_services(
    channel: tonic::transport::Channel,
    url: &str,
) -> AppResult<()> {
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
    let symbols = required_memories_schema_fields()
        .into_iter()
        .map(|required| required.message)
        .collect::<std::collections::BTreeSet<_>>();
    for message in symbols {
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

/// Verifies the bundled front immediately after its TCP port opens and before
/// Lookback advertises readiness. Reflection catches stale protobuf surfaces;
/// the empty-filter RPC also catches a binary that advertises the fields but
/// cannot query them against the migrated database.
pub async fn validate_local_memories_services(url: &str) -> AppResult<()> {
    let channel = crate::grpc::connect_startup_probe(url).await?;
    validate_memories_services(channel, url).await?;
    let mut client = ThreadServiceClient::new(crate::grpc::connect_startup_probe(url).await?);
    let request = mem_svc::FindThreadListByUserIdRequest {
        user_id: Some(mem_data::UserId { value: 1 }),
        limit: Some(100),
        offset: None,
        sort: None,
        memory_kinds: vec![mem_data::MemoryKind::Raw as i32],
        thread_created_after: None,
        thread_created_before: None,
        thread_updated_after: None,
        thread_updated_before: None,
        first_message_after: None,
        first_message_before: None,
        last_message_after: None,
        last_message_before: None,
    };
    let mut threads = client
        .find_thread_list_by_user_id(request)
        .await?
        .into_inner();
    while let Some(thread) = tokio_stream::StreamExt::next(&mut threads).await {
        let thread = thread?;
        if let Some(data) = thread.data {
            let has_memory = if data.first_message_at.is_none() && data.last_message_at.is_none() {
                let thread_id = thread.id.ok_or_else(|| {
                    AppError::Config("local memories returned a thread without an id".into())
                })?;
                let mut memories = client
                    .find_memories_by_thread_id(mem_svc::FindMemoriesByThreadIdRequest {
                        thread_id: Some(thread_id),
                        limit: Some(1),
                        offset: None,
                        roles: Vec::new(),
                        content_types: Vec::new(),
                    })
                    .await?
                    .into_inner();
                tokio_stream::StreamExt::next(&mut memories)
                    .await
                    .transpose()?
                    .is_some()
            } else {
                false
            };
            validate_thread_message_extrema(
                data.first_message_at,
                data.last_message_at,
                has_memory,
            )?;
        }
    }
    Ok(())
}

fn validate_thread_message_extrema(
    first: Option<i64>,
    last: Option<i64>,
    has_memory: bool,
) -> AppResult<()> {
    match (first, last, has_memory) {
        (None, None, false) => Ok(()),
        (None, None, true) => Err(AppError::Config(
            "local memories returned a non-empty thread without message extrema".into(),
        )),
        (Some(first), Some(last), _) if first <= last => Ok(()),
        (Some(first), Some(last), _) => Err(AppError::Config(format!(
            "local memories returned invalid message extrema: first_message_at={first} > last_message_at={last}"
        ))),
        _ => Err(AppError::Config(
            "local memories returned only one thread message-extrema field".into(),
        )),
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
    required_memories_schema_fields()
        .into_iter()
        .filter(|required| !descriptor_has_required_field(files, *required))
        .map(|required| format!("{}.{}", required.message, required.field))
        .collect()
}

fn descriptor_has_required_field(
    files: &[FileDescriptorProto],
    required: RequiredSchemaField,
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
            full_name == required.message
                && message.field.iter().any(|field| {
                    if field.name.as_deref() != Some(required.field)
                        || field.number != Some(required.number)
                    {
                        return false;
                    }
                    match required.kind {
                        RequiredFieldKind::RepeatedMemoryKind => {
                            field.label
                                == Some(prost_types::field_descriptor_proto::Label::Repeated as i32)
                                && field.r#type
                                    == Some(prost_types::field_descriptor_proto::Type::Enum as i32)
                                && field.type_name.as_deref() == Some(".llm_memory.data.MemoryKind")
                        }
                        RequiredFieldKind::OptionalString => {
                            field.label
                                == Some(prost_types::field_descriptor_proto::Label::Optional as i32)
                                && field.r#type
                                    == Some(
                                        prost_types::field_descriptor_proto::Type::String as i32,
                                    )
                        }
                        RequiredFieldKind::OptionalInt64 => {
                            field.label
                                == Some(prost_types::field_descriptor_proto::Label::Optional as i32)
                                && field.r#type
                                    == Some(prost_types::field_descriptor_proto::Type::Int64 as i32)
                        }
                    }
                })
        })
    })
}

/// Returns whether the saved configuration changes a target that is
/// currently active. A remote URL edited while its service stays local is
/// only a draft until the mode actually changes. The remote-side addresses
/// and host paths reach registered workers and conductor's schedulers, so a
/// change there also needs the restart that rewrites them.
fn active_connection_targets_changed(old: &ConnectionConfig, new: &ConnectionConfig) -> bool {
    let endpoint_changed = |old: &EndpointConfig, new: &EndpointConfig| {
        old.mode != new.mode || old.active_remote_url() != new.active_remote_url()
    };
    let gateway = |tracker: &IssueTrackerEndpointConfig| {
        (
            tracker.remote_gateway_control_url.clone(),
            tracker.remote_gateway_worker_url.clone(),
        )
    };
    let tracker_changed =
        endpoint_changed(&old.issue_tracker.endpoint, &new.issue_tracker.endpoint)
            || (!new.issue_tracker.endpoint.is_local()
                && gateway(&old.issue_tracker) != gateway(&new.issue_tracker));
    endpoint_changed(&old.jobworkerp, &new.jobworkerp)
        || endpoint_changed(&old.memories, &new.memories)
        || endpoint_changed(&old.conductor, &new.conductor)
        || tracker_changed
        || old.worker_view != new.worker_view
        || old.jobworkerp_host_paths != new.jobworkerp_host_paths
}

fn should_restart_sidecars_for_connection_change(
    old: &ConnectionConfig,
    new: &ConnectionConfig,
    sidecars_running: bool,
    mcp_enabled: bool,
) -> bool {
    sidecars_running && (mcp_enabled || active_connection_targets_changed(old, new))
}

/// A gRPC endpoint decomposed into the host/port/tls that workflow inputs and
/// conductor's jobworkerp server record carry. For memories it is the
/// workflow runner's callback (`memories_grpc_host` / `_port` / `_tls`): the
/// batch/single workflows dial memories back at these coordinates, so a remote
/// target (incl. HTTPS) must propagate here too, not only into the gRPC
/// clients the app uses directly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrpcAddress {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

impl GrpcAddress {
    pub fn loopback(port: u16) -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port,
            tls: false,
        }
    }

    /// Overlay the `memories_grpc_{host,port,tls}` workflow-input fields
    /// onto `obj`, with the resolved values winning over any caller-
    /// supplied entry (so a hallucinated endpoint from an LLM tool call
    /// can't redirect the search). The summary/reflection/import batches
    /// historically inlined this same three-key write; centralising it
    /// here keeps the wire shape — and any future endpoint field —
    /// in one place.
    pub fn inject_memories_into(&self, obj: &mut serde_json::Map<String, serde_json::Value>) {
        obj.insert(
            "memories_grpc_host".to_string(),
            serde_json::Value::String(self.host.clone()),
        );
        obj.insert("memories_grpc_port".to_string(), self.port.into());
        obj.insert("memories_grpc_tls".to_string(), self.tls.into());
    }

    /// Overlay the `issue_tracker_grpc_{host,port,tls}` workflow-input fields.
    pub fn inject_issue_tracker_into(&self, obj: &mut serde_json::Map<String, serde_json::Value>) {
        obj.insert(
            "issue_tracker_grpc_host".to_string(),
            self.host.clone().into(),
        );
        obj.insert("issue_tracker_grpc_port".to_string(), self.port.into());
        obj.insert("issue_tracker_grpc_tls".to_string(), self.tls.into());
    }
}

impl ResolvedTargets {
    /// Decompose `memories_url` into the host/port/tls a workflow callback
    /// needs. Falls back to the scheme's default port (80/443) when the URL
    /// omits one.
    pub fn memories_callback(&self) -> AppResult<GrpcAddress> {
        parse_grpc_address(&self.memories_url)
    }
}

/// Split a `http(s)://host[:port]` URL into a `GrpcAddress`. Uses the
/// `url` crate (not hand splitting on `:`) so IPv6 literals like
/// `http://[::1]:9010` parse correctly — a naive `rsplit_once(':')` would treat
/// the address colons as the port delimiter, failing the portless form and
/// leaking brackets/userinfo into the host. Kept as a free pure function so
/// it's unit-testable without a live sidecar.
pub fn parse_grpc_address(url_str: &str) -> AppResult<GrpcAddress> {
    let url = url::Url::parse(url_str)
        .map_err(|e| AppError::Config(format!("invalid URL {url_str}: {e}")))?;
    let tls = match url.scheme() {
        "https" => true,
        "http" => false,
        other => {
            return Err(AppError::Config(format!(
                "URL must be http or https, got {other}: {url_str}"
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
            return Err(AppError::Config(format!("missing host in URL: {url_str}")));
        }
    };
    // `port_or_known_default` falls back to 80/443 for http/https.
    let port = url
        .port_or_known_default()
        .ok_or_else(|| AppError::Config(format!("missing port in URL: {url_str}")))?;
    Ok(GrpcAddress { host, port, tls })
}

/// Choose the effective gRPC targets: a local service is the live sidecar
/// (dynamic port), a remote one is its configured URL.
pub fn resolve_targets(
    cfg: &ConnectionConfig,
    local: Option<&SidecarEndpoints>,
) -> AppResult<ResolvedTargets> {
    let sidecars =
        || local.ok_or_else(|| AppError::SidecarNotReady("local sidecars not started".into()));
    let jobworkerp_url = if cfg.jobworkerp.is_local() {
        sidecars()?.require_jobworkerp_url()?
    } else {
        cfg.jobworkerp.require_remote_url("jobworkerp")?.to_string()
    };
    let local_memories_port = if cfg.memories.is_local() {
        sidecars()?.memories_port
    } else {
        None
    };
    Ok(ResolvedTargets {
        jobworkerp_url,
        memories_url: memories_url(cfg, local_memories_port)?,
    })
}

/// A remote Issue tracker as jobworkerp workflows reach it: its primary
/// service and its gateway worker listener, at the addresses configured for
/// the remote side (falling back to the app's URLs).
pub fn remote_issue_tracker_for_jobworkerp(
    cfg: &ConnectionConfig,
) -> AppResult<(GrpcAddress, GrpcAddress)> {
    let tracker = &cfg.issue_tracker;
    Ok((
        remote_side_address(
            &cfg.worker_view.issue_tracker_url,
            &tracker.endpoint.remote_url,
            "Issue tracker",
        )?,
        remote_side_address(
            &cfg.worker_view.issue_tracker_gateway_worker_url,
            &tracker.remote_gateway_worker_url,
            "Issue tracker gateway worker",
        )?,
    ))
}

/// The address the remote side uses: its override, else the app's URL.
fn remote_side_address(
    override_url: &Option<String>,
    url: &Option<String>,
    name: &str,
) -> AppResult<GrpcAddress> {
    let url = match non_empty(override_url.as_deref()) {
        Some(url) => url,
        None => require_url(url.as_deref(), name)?,
    };
    parse_grpc_address(url)
}

/// Identity of the configured conductor for state kept per conductor: the
/// local sidecar's port changes, so it is one fixed name.
pub const LOCAL_CONDUCTOR_IDENTITY: &str = "local";

pub fn conductor_identity(cfg: &ConnectionConfig) -> String {
    cfg.conductor
        .active_remote_url()
        .unwrap_or(LOCAL_CONDUCTOR_IDENTITY)
        .to_string()
}

/// A remote memories as jobworkerp workflows reach it.
pub fn remote_memories_for_jobworkerp(cfg: &ConnectionConfig) -> AppResult<GrpcAddress> {
    remote_side_address(
        &cfg.worker_view.memories_url,
        &cfg.memories.remote_url,
        "memories",
    )
}

/// conductor's URL: the local sidecar or the configured remote server.
pub fn conductor_url(
    cfg: &ConnectionConfig,
    local: Option<&SidecarEndpoints>,
) -> AppResult<String> {
    if cfg.conductor.is_local() {
        local
            .and_then(SidecarEndpoints::conductor_url)
            .ok_or_else(|| AppError::SidecarNotReady("local conductor not started".into()))
    } else {
        Ok(cfg.conductor.require_remote_url("conductor")?.to_string())
    }
}

/// jobworkerp as the services depending on it reach it (conductor and
/// jobworkerp's own workflows): the local sidecar, or the remote one at the
/// address configured for the remote side (falling back to the app's URL).
pub fn jobworkerp_for_dependents(
    cfg: &ConnectionConfig,
    local: Option<&SidecarEndpoints>,
) -> AppResult<GrpcAddress> {
    if cfg.jobworkerp.is_local() {
        let local =
            local.ok_or_else(|| AppError::SidecarNotReady("local sidecars not started".into()))?;
        return Ok(GrpcAddress::loopback(local.require_jobworkerp_port()?));
    }
    remote_side_address(
        &cfg.worker_view.jobworkerp_url,
        &cfg.jobworkerp.remote_url,
        "jobworkerp",
    )
}

/// The memories URL alone, for callers that know the local memories port
/// (or that memories is remote) without the full sidecar endpoints.
pub fn memories_url(cfg: &ConnectionConfig, local_port: Option<u16>) -> AppResult<String> {
    if cfg.memories.is_local() {
        let port = local_port.ok_or_else(|| {
            AppError::SidecarNotReady("local memories sidecar not started".into())
        })?;
        Ok(crate::sidecar::lifecycle::loopback_url(port))
    } else {
        Ok(cfg.memories.require_remote_url("memories")?.to_string())
    }
}

/// Resolve the modes and endpoints from one immutable configuration snapshot.
/// Callers that use a mode as part of persisted identity must not reload
/// `connection.json` separately after resolving the URLs.
pub fn resolve_connection(
    cfg: &ConnectionConfig,
    local: Option<&SidecarEndpoints>,
) -> AppResult<ResolvedConnection> {
    Ok(ResolvedConnection {
        jobworkerp_mode: cfg.jobworkerp.mode,
        memories_mode: cfg.memories.mode,
        targets: resolve_targets(cfg, local)?,
    })
}

pub(crate) fn non_empty(s: Option<&str>) -> Option<&str> {
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

/// Validate every constraint shared by connection testing and persistence:
/// each remote service needs a usable URL, and a remote service must not
/// depend on a local one it cannot reach. `issue_tracking_enabled` decides
/// whether the tracker target matters at all.
pub(crate) fn validate_connection_config(
    cfg: &ConnectionConfig,
    issue_tracking_enabled: bool,
) -> AppResult<()> {
    let tracker = &cfg.issue_tracker;
    let tracker_remote = issue_tracking_enabled && !tracker.endpoint.is_local();
    let mut remote_urls = Vec::new();
    for (endpoint, name) in [
        (&cfg.jobworkerp, "jobworkerp"),
        (&cfg.memories, "memories"),
        (&cfg.conductor, "conductor"),
    ] {
        if !endpoint.is_local() {
            remote_urls.push(endpoint.require_remote_url(name)?);
        }
    }
    if tracker_remote {
        for (url, name) in [
            (&tracker.endpoint.remote_url, "Issue tracker"),
            (
                &tracker.remote_gateway_control_url,
                "Issue tracker gateway control",
            ),
            (
                &tracker.remote_gateway_worker_url,
                "Issue tracker gateway worker",
            ),
        ] {
            remote_urls.push(require_url(url.as_deref(), name)?);
        }
    }
    for url in remote_urls {
        validate_remote_url(url)?;
    }
    if let Some(url) = cfg.memories.active_remote_url() {
        // Workflow callbacks decompose the memories target independently from
        // tonic, so reject values they cannot represent before saving them.
        parse_grpc_address(url)?;
    }
    if !cfg.jobworkerp.is_local() {
        if cfg.memories.is_local() {
            return Err(AppError::Config(
                "a remote jobworkerp needs a remote memories".into(),
            ));
        }
        if issue_tracking_enabled && !tracker_remote {
            return Err(AppError::Config(
                "a remote jobworkerp needs a remote Issue tracker".into(),
            ));
        }
    } else if !cfg.conductor.is_local() {
        return Err(AppError::Config(
            "a remote conductor needs a remote jobworkerp".into(),
        ));
    }
    Ok(())
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
    validate_connection_config(
        &cfg,
        super::issue_tracker_settings::effective_issue_tracking_enabled(&state.data),
    )?;

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
    if cfg.memories.active_remote_url().is_some() {
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
    // Validate the complete startup contract before persisting so an
    // incompatible remote target cannot lock the app into a failed boot.
    validate_connection_config(
        &cfg,
        super::issue_tracker_settings::effective_issue_tracking_enabled(&state.data),
    )?;
    if let Some(memories_url) = cfg.memories.active_remote_url() {
        validate_remote_memories_services(memories_url).await?;
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
        // `start_with_warnings` returns the start Result (unlike
        // `stage_and_start_sidecars`, which only emits an event), so a failed
        // restart is propagated to the caller instead of being swallowed —
        // otherwise the UI clears its dirty state on a save that left the
        // sidecar down with stale MCP defaults. On failure roll the config
        // file back and bring the sidecar up on the OLD target (mirrors
        // `set_embedding_settings`'s rollback).
        let plugin_warnings = super::stage_plugins_for_restart(&app, &state.data.plugins_dir());
        let restart_plan = RestartPlan::connection();
        match state
            .sidecars
            .restart_plan(restart_plan.clone(), plugin_warnings)
            .await
        {
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
                let _ = save_connection_config(&config_path, &old_cfg);
                state.invalidate_clients().await;
                let rollback_warnings =
                    super::stage_plugins_for_restart(&app, &state.data.plugins_dir());
                if let Ok(report) = state
                    .sidecars
                    .restart_plan(restart_plan, rollback_warnings)
                    .await
                {
                    let _ = app.emit("sidecar://ready", &report);
                }
                return Err(AppError::Config(format!(
                    "接続先の変更に失敗しました。元の接続先にロールバックしました: {e}"
                )));
            }
        }
    }
    Ok(())
}

/// What the configured remote jobworkerp still lacks for Lookback.
#[tauri::command]
pub async fn inspect_remote_jobworkerp(
    state: tauri::State<'_, AppState>,
) -> AppResult<RemoteJobworkerpReadinessReport> {
    Ok(state.sidecars.inspect_remote_jobworkerp().await?.into())
}

/// Register Lookback's workers on the configured remote jobworkerp, then
/// report what is still missing. Steps that failed come back as warnings.
#[tauri::command]
pub async fn register_remote_jobworkerp_workers(
    state: tauri::State<'_, AppState>,
) -> AppResult<RemoteJobworkerpRegistration> {
    let (warnings, readiness) = state.sidecars.register_remote_jobworkerp_workers().await?;
    Ok(RemoteJobworkerpRegistration {
        warnings,
        readiness: readiness.into(),
    })
}

/// The readiness with its verdict, so the UI does not re-derive which gaps
/// count.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteJobworkerpReadinessReport {
    #[serde(flatten)]
    pub readiness: crate::sidecar::lifecycle::RemoteJobworkerpReadiness,
    pub ready: bool,
}

impl From<crate::sidecar::lifecycle::RemoteJobworkerpReadiness>
    for RemoteJobworkerpReadinessReport
{
    fn from(readiness: crate::sidecar::lifecycle::RemoteJobworkerpReadiness) -> Self {
        Self {
            ready: readiness.is_ready(),
            readiness,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteJobworkerpRegistration {
    pub warnings: Vec<crate::sidecar::SidecarWarning>,
    pub readiness: RemoteJobworkerpReadinessReport,
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost_types::{DescriptorProto, FieldDescriptorProto, FileDescriptorProto};

    #[test]
    fn the_readiness_report_carries_its_verdict_beside_the_gaps() {
        let ready = serde_json::to_value(RemoteJobworkerpReadinessReport::from(
            crate::sidecar::lifecycle::RemoteJobworkerpReadiness::default(),
        ))
        .unwrap();
        assert_eq!(ready["ready"], true);
        let gap = serde_json::to_value(RemoteJobworkerpReadinessReport::from(
            crate::sidecar::lifecycle::RemoteJobworkerpReadiness {
                missing_runners: vec!["LLMPromptRunner".into()],
                ..Default::default()
            },
        ))
        .unwrap();
        assert_eq!(gap["ready"], false);
        assert_eq!(gap["missing_runners"][0], "LLMPromptRunner");
    }

    #[test]
    fn thread_group_owner_capability_accepts_current_server_contract() {
        let response = mem_svc::ThreadGroupCapabilitiesResponse {
            supports_independent_group_owner_scope: true,
        };

        assert!(validate_thread_group_owner_capability_response(Ok(response)).is_ok());
    }

    #[test]
    fn thread_group_owner_capability_rejects_legacy_unimplemented_server() {
        let error = validate_thread_group_owner_capability_response(Err(
            tonic::Status::unimplemented("unknown RPC"),
        ))
        .unwrap_err();

        assert!(matches!(error, AppError::Config(message)
            if message.contains("does not support independent ThreadGroup group ownership")
                && message.contains("UNIMPLEMENTED")));
    }

    #[test]
    fn thread_group_owner_capability_rejects_false_capability() {
        let response = mem_svc::ThreadGroupCapabilitiesResponse {
            supports_independent_group_owner_scope: false,
        };

        assert!(matches!(
            validate_thread_group_owner_capability_response(Ok(response)),
            Err(AppError::Config(message))
                if message.contains("does not support independent ThreadGroup group ownership")
        ));
    }

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
    fn local_smoke_accepts_empty_or_ordered_message_extrema() {
        assert!(validate_thread_message_extrema(None, None, false).is_ok());
        assert!(validate_thread_message_extrema(None, None, true).is_err());
        assert!(validate_thread_message_extrema(Some(100), Some(100), true).is_ok());
        assert!(validate_thread_message_extrema(Some(100), Some(200), true).is_ok());
        assert!(validate_thread_message_extrema(Some(200), Some(100), true).is_err());
        assert!(validate_thread_message_extrema(Some(100), None, true).is_err());
        assert!(validate_thread_message_extrema(None, Some(100), true).is_err());
    }

    #[test]
    fn remote_reflection_requires_all_thread_time_filter_fields() {
        let legacy = vec![FileDescriptorProto::default()];
        let missing = missing_remote_memories_schema_fields(&legacy);
        assert!(missing.contains(&"llm_memory.data.ThreadData.first_message_at".to_string()));
        assert!(missing.contains(
            &"llm_memory.service.FindThreadListByUserIdRequest.last_message_after".to_string()
        ));

        let current = required_schema_descriptors();
        assert!(missing_remote_memories_schema_fields(&current).is_empty());

        let mut malformed = current;
        for file in &mut malformed {
            for message in &mut file.message_type {
                for field in &mut message.field {
                    if field.name.as_deref() == Some("last_message_after") {
                        field.number = Some(999);
                        break;
                    }
                }
            }
        }
        assert_eq!(
            missing_remote_memories_schema_fields(&malformed)
                .into_iter()
                .filter(|field| field.ends_with(".last_message_after"))
                .count(),
            6
        );
    }

    #[test]
    fn remote_reflection_rejects_wrong_summary_filter_field_shapes() {
        let mut malformed = required_schema_descriptors();
        for file in &mut malformed {
            for message in &mut file.message_type {
                let Some(name) = message.name.as_deref() else {
                    continue;
                };
                if name == "FindMemoryListRequest" || name == "MemoryCountCondition" {
                    for field in &mut message.field {
                        if field.name.as_deref() == Some("external_id_prefix") {
                            field.r#type =
                                Some(prost_types::field_descriptor_proto::Type::Int64 as i32);
                        }
                    }
                }
            }
        }
        let missing = missing_remote_memories_schema_fields(&malformed);
        assert!(
            missing.contains(
                &"llm_memory.service.FindMemoryListRequest.external_id_prefix".to_string()
            )
        );
        assert!(
            missing.contains(
                &"llm_memory.service.MemoryCountCondition.external_id_prefix".to_string()
            )
        );
    }

    #[test]
    fn remote_reflection_requires_reflection_memory_cursor_field() {
        let mut malformed = required_schema_descriptors();
        for file in &mut malformed {
            for message in &mut file.message_type {
                if message.name.as_deref() == Some("SearchReflectionsRequest") {
                    message
                        .field
                        .retain(|field| field.name.as_deref() != Some("cursor_after_memory_id"));
                }
            }
        }
        let missing = missing_remote_memories_schema_fields(&malformed);
        assert!(missing.contains(
            &"llm_memory.service.SearchReflectionsRequest.cursor_after_memory_id".to_string()
        ));
    }

    fn required_schema_descriptors() -> Vec<FileDescriptorProto> {
        let mut files = std::collections::BTreeMap::<
            String,
            std::collections::BTreeMap<String, Vec<FieldDescriptorProto>>,
        >::new();
        for required in required_memories_schema_fields() {
            let (package, message) = required.message.rsplit_once('.').unwrap();
            files
                .entry(package.to_string())
                .or_default()
                .entry(message.to_string())
                .or_default()
                .push(FieldDescriptorProto {
                    name: Some(required.field.to_string()),
                    number: Some(required.number),
                    label: Some(if required.kind == RequiredFieldKind::RepeatedMemoryKind {
                        prost_types::field_descriptor_proto::Label::Repeated as i32
                    } else {
                        prost_types::field_descriptor_proto::Label::Optional as i32
                    }),
                    r#type: Some(if required.kind == RequiredFieldKind::RepeatedMemoryKind {
                        prost_types::field_descriptor_proto::Type::Enum as i32
                    } else if required.kind == RequiredFieldKind::OptionalString {
                        prost_types::field_descriptor_proto::Type::String as i32
                    } else {
                        prost_types::field_descriptor_proto::Type::Int64 as i32
                    }),
                    type_name: (required.kind == RequiredFieldKind::RepeatedMemoryKind)
                        .then(|| ".llm_memory.data.MemoryKind".to_string()),
                    ..Default::default()
                });
        }
        files
            .into_iter()
            .map(|(package, messages)| FileDescriptorProto {
                package: Some(package),
                message_type: messages
                    .into_iter()
                    .map(|(name, field)| DescriptorProto {
                        name: Some(name),
                        field,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .collect()
    }

    fn local_eps() -> SidecarEndpoints {
        SidecarEndpoints {
            jobworkerp_port: Some(9000),
            issue_tracker_port: None,
            memories_port: Some(9010),
            conductor_port: Some(9020),
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
        let cfg = ConnectionConfig::remote_pair("http://10.0.0.2:9000", "http://10.0.0.2:9010");
        // local endpoints are ignored in remote mode.
        let eps = local_eps();
        let t = resolve_targets(&cfg, Some(&eps)).unwrap();
        assert_eq!(t.jobworkerp_url, "http://10.0.0.2:9000");
        assert_eq!(t.memories_url, "http://10.0.0.2:9010");
    }

    #[test]
    fn resolved_connection_keeps_modes_and_urls_in_one_snapshot() {
        let cfg = ConnectionConfig {
            memories: EndpointConfig::remote("https://remote-a:9010"),
            ..ConnectionConfig::default()
        };
        let resolved = resolve_connection(&cfg, Some(&local_eps())).unwrap();
        assert_eq!(resolved.jobworkerp_mode, ConnectionMode::Local);
        assert_eq!(resolved.memories_mode, ConnectionMode::Remote);
        assert_eq!(resolved.targets.memories_url, "https://remote-a:9010");
    }

    #[test]
    fn a_local_jobworkerp_follows_the_live_sidecar_beside_a_remote_memories() {
        let cfg = ConnectionConfig {
            memories: EndpointConfig::remote("https://memories.example:9000"),
            ..ConnectionConfig::default()
        };
        let eps = SidecarEndpoints {
            jobworkerp_port: Some(43599),
            memories_port: None,
            ..local_eps()
        };
        let t = resolve_targets(&cfg, Some(&eps)).unwrap();
        assert_eq!(t.jobworkerp_url, "http://127.0.0.1:43599");
        assert_eq!(t.memories_url, "https://memories.example:9000");
        // The local jobworkerp needs the running sidecars.
        assert!(matches!(
            resolve_targets(&cfg, None),
            Err(AppError::SidecarNotReady(_))
        ));
    }

    #[test]
    fn a_remote_tracker_is_reached_at_the_remote_side_addresses() {
        let tracker = IssueTrackerEndpointConfig {
            endpoint: EndpointConfig::remote("https://tracker.example"),
            remote_gateway_control_url: Some("https://tracker.example:8443".into()),
            remote_gateway_worker_url: Some("https://tracker.example:8444".into()),
        };
        let cfg = ConnectionConfig {
            issue_tracker: tracker.clone(),
            worker_view: WorkerViewOverrides {
                issue_tracker_url: Some("http://tracker.internal:7000".into()),
                ..WorkerViewOverrides::default()
            },
            ..ConnectionConfig::remote_pair("https://j.example", "https://m.example")
        };
        let (primary, gateway_worker) = remote_issue_tracker_for_jobworkerp(&cfg).unwrap();
        assert_eq!(
            primary,
            GrpcAddress {
                host: "tracker.internal".into(),
                port: 7000,
                tls: false,
            }
        );
        // No override: the app's URL, TLS included.
        assert_eq!(
            gateway_worker,
            GrpcAddress {
                host: "tracker.example".into(),
                port: 8444,
                tls: true,
            }
        );
        let missing = ConnectionConfig {
            issue_tracker: IssueTrackerEndpointConfig {
                remote_gateway_worker_url: None,
                ..tracker
            },
            ..ConnectionConfig::default()
        };
        assert!(remote_issue_tracker_for_jobworkerp(&missing).is_err());
    }

    #[test]
    fn a_conductor_is_identified_by_its_remote_url() {
        assert_eq!(conductor_identity(&ConnectionConfig::default()), "local");
        let remote = ConnectionConfig {
            conductor: EndpointConfig::remote("https://conductor.example"),
            ..ConnectionConfig::default()
        };
        assert_eq!(conductor_identity(&remote), "https://conductor.example");
        let draft = ConnectionConfig {
            conductor: EndpointConfig::local_with_draft("https://conductor.example"),
            ..ConnectionConfig::default()
        };
        assert_eq!(conductor_identity(&draft), "local");
    }

    #[test]
    fn conductor_targets_the_configured_jobworkerp() {
        let local = ConnectionConfig::default();
        assert_eq!(
            conductor_url(&local, Some(&local_eps())).unwrap(),
            "http://127.0.0.1:9020"
        );
        assert_eq!(
            jobworkerp_for_dependents(&local, Some(&local_eps())).unwrap(),
            GrpcAddress::loopback(9000)
        );

        // A local conductor enqueues to a remote jobworkerp at the address
        // configured for the remote side, TLS included.
        let remote = ConnectionConfig {
            worker_view: WorkerViewOverrides {
                jobworkerp_url: Some("https://jobworkerp.internal:8443".into()),
                ..WorkerViewOverrides::default()
            },
            ..ConnectionConfig::remote_pair(
                "https://jobworkerp.example",
                "https://memories.example",
            )
        };
        assert_eq!(
            jobworkerp_for_dependents(&remote, None).unwrap(),
            GrpcAddress {
                host: "jobworkerp.internal".into(),
                port: 8443,
                tls: true,
            }
        );
        let without_override =
            ConnectionConfig::remote_pair("https://jobworkerp.example", "https://memories.example");
        assert_eq!(
            jobworkerp_for_dependents(&without_override, None)
                .unwrap()
                .host,
            "jobworkerp.example"
        );

        let remote_conductor = ConnectionConfig {
            conductor: EndpointConfig::remote("https://conductor.example"),
            ..remote
        };
        assert_eq!(
            conductor_url(&remote_conductor, None).unwrap(),
            "https://conductor.example"
        );
        assert!(matches!(
            conductor_url(&ConnectionConfig::default(), None),
            Err(AppError::SidecarNotReady(_))
        ));
    }

    #[test]
    fn resolve_remote_errors_when_url_missing() {
        let cfg = ConnectionConfig {
            jobworkerp: EndpointConfig {
                mode: ConnectionMode::Remote,
                remote_url: None,
            },
            memories: EndpointConfig::remote("http://10.0.0.2:9010"),
            ..ConnectionConfig::default()
        };
        let err = resolve_targets(&cfg, None).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    #[test]
    fn resolve_remote_errors_when_url_blank() {
        let cfg = ConnectionConfig::remote_pair("http://10.0.0.2:9000", "   ");
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
        assert!(back.jobworkerp.is_local() && back.memories.is_local());
    }

    #[test]
    fn serde_roundtrips_remote() {
        let cfg = ConnectionConfig {
            issue_tracker: IssueTrackerEndpointConfig {
                endpoint: EndpointConfig::remote("https://tracker.example"),
                remote_gateway_control_url: Some("https://tracker.example:8443".into()),
                remote_gateway_worker_url: Some("https://tracker.example:8444".into()),
            },
            conductor: EndpointConfig::remote("https://conductor.example"),
            worker_view: WorkerViewOverrides {
                memories_url: Some("http://memories.internal:9010".into()),
                ..WorkerViewOverrides::default()
            },
            jobworkerp_host_paths: JobworkerpHostPaths {
                memories_import_bin: Some("/opt/lookback/memories-import".into()),
                ..JobworkerpHostPaths::default()
            },
            ..ConnectionConfig::remote_pair("http://h:9000", "http://h:9010")
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
    fn the_earlier_single_mode_applies_to_jobworkerp_and_memories() {
        let back: ConnectionConfig = serde_json::from_str(
            r#"{"mode":"remote","remote_jobworkerp_url":"http://localhost:9000","remote_memories_url":"http://h:9010"}"#,
        )
        .unwrap();
        // Read as written, even when the jobworkerp URL is a loopback one:
        // that may really be another jobworkerp.
        assert_eq!(
            back,
            ConnectionConfig::remote_pair("http://localhost:9000", "http://h:9010")
        );

        let local: ConnectionConfig =
            serde_json::from_str(r#"{"mode":"local","remote_memories_url":"http://h:9010"}"#)
                .unwrap();
        assert!(local.jobworkerp.is_local() && local.memories.is_local());
        assert_eq!(local.memories.remote_url(), Some("http://h:9010"));
    }

    #[test]
    fn a_saved_config_uses_the_per_service_shape() {
        let json = serde_json::to_value(ConnectionConfig::remote_pair(
            "http://h:9000",
            "http://h:9010",
        ))
        .unwrap();
        assert_eq!(json["jobworkerp"]["mode"], "remote");
        assert_eq!(json["memories"]["remote_url"], "http://h:9010");
        assert!(json.get("mode").is_none());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connection.json");
        let cfg = ConnectionConfig::remote_pair("http://h:9000", "http://h:9010");
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
        let c = parse_grpc_address("http://127.0.0.1:9010").unwrap();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 9010);
        assert!(!c.tls);
    }

    #[test]
    fn parse_callback_https_with_explicit_port() {
        let c = parse_grpc_address("https://memories.example.com:8443").unwrap();
        assert_eq!(c.host, "memories.example.com");
        assert_eq!(c.port, 8443);
        assert!(c.tls);
    }

    #[test]
    fn parse_callback_defaults_port_by_scheme() {
        let http = parse_grpc_address("http://host").unwrap();
        assert_eq!(http.port, 80);
        assert!(!http.tls);
        let https = parse_grpc_address("https://host").unwrap();
        assert_eq!(https.port, 443);
        assert!(https.tls);
    }

    #[test]
    fn parse_callback_strips_path() {
        let c = parse_grpc_address("https://host:9010/some/path?x=1").unwrap();
        assert_eq!(c.host, "host");
        assert_eq!(c.port, 9010);
    }

    #[test]
    fn parse_callback_rejects_missing_scheme() {
        assert!(matches!(
            parse_grpc_address("127.0.0.1:9010"),
            Err(AppError::Config(_))
        ));
    }

    #[test]
    fn parse_callback_rejects_bad_port() {
        assert!(matches!(
            parse_grpc_address("http://host:notaport"),
            Err(AppError::Config(_))
        ));
    }

    #[test]
    fn parse_callback_ipv6_with_explicit_port() {
        // IPv6 literal with a port: host must keep its brackets so the
        // downstream runner's `format!("{host}:{port}")` round-trips.
        let c = parse_grpc_address("http://[::1]:9010").unwrap();
        assert_eq!(c.host, "[::1]");
        assert_eq!(c.port, 9010);
        assert!(!c.tls);
    }

    #[test]
    fn parse_callback_ipv6_default_port() {
        // Portless IPv6 must NOT fail (the old rsplit_once(':') treated the
        // address colons as a port delimiter and errored).
        let c = parse_grpc_address("https://[2001:db8::1]").unwrap();
        assert_eq!(c.host, "[2001:db8::1]");
        assert_eq!(c.port, 443);
        assert!(c.tls);
    }

    #[test]
    fn parse_callback_drops_userinfo() {
        // Userinfo must not leak into the host (old split kept user:pass@host).
        let c = parse_grpc_address("http://user:pass@host:9010").unwrap();
        assert_eq!(c.host, "host");
        assert_eq!(c.port, 9010);
    }

    #[test]
    fn save_time_validation_accepts_what_dispatch_can_parse() {
        // The URL-shape portion of set_connection_config's guard runs
        // validate_remote_url and parse_grpc_address before its live Reflection
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
            parse_grpc_address(url).unwrap_or_else(|e| panic!("parse {url}: {e}"));
        }
    }

    #[test]
    fn remote_config_validation_requires_and_validates_each_remote_url() {
        let valid = ConnectionConfig::remote_pair(
            "http://jobworkerp.example:9000",
            "https://[2001:db8::1]",
        );
        assert!(validate_connection_config(&valid, false).is_ok());

        let missing_memories = ConnectionConfig {
            memories: EndpointConfig {
                mode: ConnectionMode::Remote,
                remote_url: None,
            },
            ..valid.clone()
        };
        assert!(matches!(
            validate_connection_config(&missing_memories, false),
            Err(AppError::Config(message)) if message.contains("remote memories URL not configured")
        ));

        let invalid_memories = ConnectionConfig {
            memories: EndpointConfig::remote("ftp://memories.example"),
            ..valid
        };
        assert!(validate_connection_config(&invalid_memories, false).is_err());

        // A local service's draft remote URL is not validated.
        let draft = ConnectionConfig {
            jobworkerp: EndpointConfig::local_with_draft("not a url"),
            ..ConnectionConfig::default()
        };
        assert!(validate_connection_config(&draft, false).is_ok());
    }

    #[test]
    fn a_remote_service_cannot_depend_on_a_local_one() {
        let remote_jobworkerp_local_memories = ConnectionConfig {
            jobworkerp: EndpointConfig::remote("http://jobworkerp.example:9000"),
            ..ConnectionConfig::default()
        };
        assert!(validate_connection_config(&remote_jobworkerp_local_memories, false).is_err());

        let remote_pair =
            ConnectionConfig::remote_pair("http://jobworkerp.example:9000", "http://memories:9010");
        // The local tracker matters only while Issue tracking is enabled.
        assert!(validate_connection_config(&remote_pair, false).is_ok());
        assert!(matches!(
            validate_connection_config(&remote_pair, true),
            Err(AppError::Config(message)) if message.contains("remote Issue tracker")
        ));
        let remote_tracker = ConnectionConfig {
            issue_tracker: IssueTrackerEndpointConfig {
                endpoint: EndpointConfig::remote("http://tracker:7000"),
                remote_gateway_control_url: Some("http://tracker:7001".into()),
                remote_gateway_worker_url: Some("http://tracker:7002".into()),
            },
            ..remote_pair.clone()
        };
        assert!(validate_connection_config(&remote_tracker, true).is_ok());
        let tracker_without_gateway = ConnectionConfig {
            issue_tracker: IssueTrackerEndpointConfig {
                remote_gateway_worker_url: None,
                ..remote_tracker.issue_tracker.clone()
            },
            ..remote_pair.clone()
        };
        assert!(validate_connection_config(&tracker_without_gateway, true).is_err());

        let remote_conductor_local_jobworkerp = ConnectionConfig {
            conductor: EndpointConfig::remote("http://conductor:9020"),
            ..ConnectionConfig::default()
        };
        assert!(validate_connection_config(&remote_conductor_local_jobworkerp, false).is_err());
        let local_conductor_remote_jobworkerp = remote_pair;
        assert!(validate_connection_config(&local_conductor_remote_jobworkerp, false).is_ok());
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
        let remote_a =
            ConnectionConfig::remote_pair("http://remote-a:9000", "http://remote-a:9010");
        let remote_b = ConnectionConfig {
            memories: EndpointConfig::remote("http://remote-b:9010"),
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

        // Each service counts, not only jobworkerp and memories.
        let remote_conductor = ConnectionConfig {
            conductor: EndpointConfig::remote("http://conductor:9020"),
            ..remote_a.clone()
        };
        assert!(should_restart_sidecars_for_connection_change(
            &remote_a,
            &remote_conductor,
            true,
            false
        ));
        let moved_import_bin = ConnectionConfig {
            jobworkerp_host_paths: JobworkerpHostPaths {
                memories_import_bin: Some("/opt/memories-import".into()),
                ..JobworkerpHostPaths::default()
            },
            ..remote_a.clone()
        };
        assert!(should_restart_sidecars_for_connection_change(
            &remote_a,
            &moved_import_bin,
            true,
            false
        ));
    }

    #[test]
    fn local_mode_does_not_restart_for_dormant_remote_url_edits() {
        let local = ConnectionConfig::default();
        let edited_remote = ConnectionConfig {
            jobworkerp: EndpointConfig::local_with_draft("http://remote:9000"),
            memories: EndpointConfig::local_with_draft("http://remote:9010"),
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
