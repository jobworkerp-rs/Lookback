//! Private typed gRPC client and runtime configuration for Issue Tracker.

use std::{collections::HashSet, fmt::Write as _, path::PathBuf, sync::Arc, time::Duration};

use parking_lot::RwLock;
use prost::Message;
use tonic::Code;

use crate::{
    coding_agent::model::PrimaryTrackerIdentity,
    data::DataPaths,
    error::{AppError, AppResult},
};

use crate::grpc::proto::issue_tracker::{manager::v1, rpc::v1 as rpc, v1 as issue};

type ManagerConsumerClient = crate::grpc::proto::issue_tracker::rpc::v1::manager_consumer_service_client::ManagerConsumerServiceClient<tonic::transport::Channel>;

/// Durable manager method selected by the Gateway-control RPC implementation.
///
/// The public RPC is named `RegisterGateway`, but its server-side manager
/// dispatch key is part of `ManagerWriteContext`'s contract.
const MANAGER_REGISTER_GATEWAY_METHOD: &str = "manager_register_gateway";
const GATEWAY_REGISTRATION_TARGET: &str = "gateway/lookback-local";
const AWAITING_REVIEW_STATE_CONTRACT: &str = "awaiting_review/v1";
const MAX_AWAITING_REVIEW_ISSUE_ID_BYTES: usize = 4_096;
const MAX_AWAITING_REVIEW_CAPABILITY_ID_BYTES: usize = 256;
const MAX_AWAITING_REVIEW_IDEMPOTENCY_KEY_BYTES: usize = 4_096;
const MAX_AWAITING_REVIEW_RUN_ID_BYTES: usize = 256;
const MAX_AWAITING_REVIEW_EVIDENCE_REFERENCES: usize = 1_000;
const MAX_AWAITING_REVIEW_EVIDENCE_REFERENCE_BYTES: usize = 2_048;
const MAX_AWAITING_REVIEW_REPORT_EVIDENCE: usize = 1_000;
const MAX_AWAITING_REVIEW_REPORT_TEXT_BYTES: usize = 4_096;
const MAX_AWAITING_REVIEW_REPORT_URI_BYTES: usize = 4_096;
const MAX_AWAITING_REVIEW_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const ISSUE_HISTORY_PAGE_SIZE: u32 = 100;
const MAX_ISSUE_HISTORY_ENTRIES: usize = 10_000;
const MAX_ISSUE_HISTORY_PAGES: usize = 100;
const MAX_ISSUE_HISTORY_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ISSUE_HISTORY_ISSUE_ID_BYTES: usize = 4_096;
const MAX_ISSUE_HISTORY_CURSOR_BYTES: usize = 4_096;

/// Caller-controlled metadata and a typed manager write payload.
#[derive(Debug, Clone)]
pub struct ManagerWriteRequest<T> {
    pub correlation_id: String,
    pub idempotency_key: String,
    pub actor_audit: Option<v1::ActorAudit>,
    pub payload: T,
}

/// Errors produced at the typed manager boundary.
#[derive(Debug)]
pub enum IssueTrackerClientError {
    InvalidRequest(String),
    NotHealthy(String),
    Manager(Box<v1::ManagerError>),
    Gateway(AwaitingReviewFailureKind),
    Transport(AppError),
    UnconfirmedOperation(String),
}

/// Fixed, non-sensitive classifications for the dedicated awaiting-review
/// Gateway path. Upstream messages, codes, metadata, and request credentials
/// are deliberately not retained in this error value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwaitingReviewFailureKind {
    Unconfirmed,
    InvalidArgument,
    Forbidden,
    PreconditionFailed,
    RevisionConflict,
    IdempotencyConflict,
    NotFound,
    Unavailable,
    UnknownGatewayError,
    ConnectionFailed,
    RpcUnimplemented,
    RpcUnavailable,
    RpcDeadlineExceeded,
    RpcFailure,
}

impl AwaitingReviewFailureKind {
    fn from_gateway_code(code: &str) -> Self {
        match code {
            "unconfirmed" => Self::Unconfirmed,
            "invalid_argument" => Self::InvalidArgument,
            "forbidden" => Self::Forbidden,
            "precondition_failed" => Self::PreconditionFailed,
            "revision_conflict" => Self::RevisionConflict,
            "idempotency_conflict" => Self::IdempotencyConflict,
            "not_found" => Self::NotFound,
            "unavailable" => Self::Unavailable,
            _ => Self::UnknownGatewayError,
        }
    }
}

impl std::fmt::Display for AwaitingReviewFailureKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Unconfirmed => "awaiting-review operation is unconfirmed",
            Self::InvalidArgument => "awaiting-review request was rejected",
            Self::Forbidden => "awaiting-review claim is not authorized",
            Self::PreconditionFailed => "awaiting-review precondition failed",
            Self::RevisionConflict => "awaiting-review revision conflict",
            Self::IdempotencyConflict => "awaiting-review idempotency conflict",
            Self::NotFound => "awaiting-review issue was not found",
            Self::Unavailable => "Issue Tracker Gateway is unavailable",
            Self::UnknownGatewayError => "Issue Tracker Gateway returned an unknown error",
            Self::ConnectionFailed => "Issue Tracker Gateway connection failed",
            Self::RpcUnimplemented => {
                "Issue Tracker server does not implement a required RPC; update the Issue Tracker server"
            }
            Self::RpcUnavailable => "Issue Tracker Gateway is unavailable",
            Self::RpcDeadlineExceeded => "awaiting-review operation is unconfirmed",
            Self::RpcFailure => "awaiting-review Gateway transport failed",
        };
        formatter.write_str(message)
    }
}

impl std::fmt::Display for IssueTrackerClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) => {
                write!(formatter, "invalid Issue Tracker request: {message}")
            }
            Self::NotHealthy(message) => {
                write!(formatter, "Issue Tracker manager is unavailable: {message}")
            }
            Self::Manager(error) => write!(
                formatter,
                "Issue Tracker manager rejected request: {}",
                error.message
            ),
            Self::Gateway(kind) => kind.fmt(formatter),
            Self::Transport(error) => error.fmt(formatter),
            Self::UnconfirmedOperation(message) => write!(
                formatter,
                "Issue Tracker operation is unconfirmed: {message}"
            ),
        }
    }
}

impl std::error::Error for IssueTrackerClientError {}

type ManagerResult<T> = Result<T, IssueTrackerClientError>;

trait ManagerOutcome<T> {
    fn into_result(self) -> ManagerResult<T>;
}

fn map_manager_outcome<T, O: ManagerOutcome<T>>(outcome: Option<O>) -> ManagerResult<T> {
    outcome
        .ok_or_else(|| {
            IssueTrackerClientError::Transport(AppError::IssueTrackerServer(
                "Issue Tracker response has no outcome".into(),
            ))
        })?
        .into_result()
}

pub fn gateway_registration_matches(
    registration: &v1::GatewayRegistration,
    expected: v1::WorkEligibilityMode,
) -> bool {
    registration.policy.as_ref().map(|policy| policy.mode) == Some(expected as i32)
}

/// Immutable launch inputs for the local Issue Tracker server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueTrackerServerConfig {
    config_path: PathBuf,
    primary_port: u16,
    gateway_control_port: u16,
    gateway_worker_port: u16,
    toml: String,
}

fn toml_basic_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str(r"\\"),
            '"' => escaped.push_str("\\\""),
            '\u{0008}' => escaped.push_str(r"\b"),
            '\t' => escaped.push_str(r"\t"),
            '\n' => escaped.push_str(r"\n"),
            '\u{000C}' => escaped.push_str(r"\f"),
            '\r' => escaped.push_str(r"\r"),
            character if character < '\u{0020}' || character == '\u{007F}' => {
                write!(escaped, r"\u{:04X}", character as u32)
                    .expect("writing an escaped TOML string cannot fail");
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

impl IssueTrackerServerConfig {
    pub fn new(
        data: &DataPaths,
        primary_port: u16,
        gateway_control_port: u16,
        gateway_worker_port: u16,
    ) -> AppResult<Self> {
        if primary_port == 0 || gateway_control_port == 0 || gateway_worker_port == 0 {
            return Err(AppError::Config(
                "Issue Tracker listener ports must be non-zero".into(),
            ));
        }
        if primary_port == gateway_control_port
            || primary_port == gateway_worker_port
            || gateway_control_port == gateway_worker_port
        {
            return Err(AppError::Config(
                "Issue Tracker listener ports must be distinct".into(),
            ));
        }
        data.ensure_issue_tracker_runtime()?;
        let tracker_root = data.issue_tracker_taskchampion_root();
        let artifacts = data.issue_tracker_artifacts_dir();
        let backups = data.issue_tracker_backup_dir();
        let exports = data.issue_tracker_export_dir();
        let tracker_root = toml_basic_string(&tracker_root.display().to_string());
        let artifacts = toml_basic_string(&artifacts.display().to_string());
        let backups = toml_basic_string(&backups.display().to_string());
        let exports = toml_basic_string(&exports.display().to_string());
        let toml = format!(
            r#"[server]
listen = "127.0.0.1:{primary_port}"

[[gateway_endpoints]]
gateway_id = "lookback-local"
control_listen = "127.0.0.1:{gateway_control_port}"
worker_listen = "127.0.0.1:{gateway_worker_port}"

[tracker]
data_root = {}
secret_filter_mode = 1
ownership_maintenance_interval = {{ millis = 30000 }}
ownership_recovery_batch_size = 100

[tracker.lease_policy]
default_lease = {{ millis = 600000 }}
minimum_lease = {{ millis = 10000 }}
maximum_lease = {{ millis = 1800000 }}
maximum_heartbeat_interval = {{ millis = 1000 }}

[tracker.pagination_policy]
default_page_size = 50
maximum_page_size = 100

[tracker.resource_limits]
max_string_bytes = 4096
max_body_bytes = 65536
max_uri_bytes = 4096
max_token_bytes = 4096
max_handle_bytes = 4096
max_repeated_item_count = 100
max_export_bytes = 1048576
max_export_item_count = 10000
max_artifact_bytes = 1048576
max_artifact_count = 100
max_total_count_scan_items = 10000

[tracker.manager_settings]
data_root = {}
store_identities = []
default_page_size = 50
maximum_page_size = 100
maximum_payload_bytes = 65536
maximum_repeated_item_count = 100
# The standalone server deserializes protobuf enum fields as their numeric value.
secret_filter_mode = 2

[tracker.manager_settings.operation_lease]
millis = 30000
[tracker.manager_settings.outbox_lease]
millis = 30000
[tracker.manager_settings.resource_limits]
max_string_bytes = 4096
max_body_bytes = 65536
max_uri_bytes = 4096
max_repeated_item_count = 100
max_page_size = 100
[tracker.manager_settings.retry_budget]
max_attempts = 3
[tracker.manager_settings.retry_budget.base_backoff]
millis = 100
[tracker.manager_settings.retry_budget.max_backoff]
millis = 1000

[maintenance]
recovery_batch_size = 32
delivery_batch_size = 32

# Runtime directories are created by agent-app before this file is written.
# artifact_root = {}
# backup_root = {}
# export_root = {}
"#,
            tracker_root, tracker_root, artifacts, backups, exports,
        );
        Ok(Self {
            config_path: data
                .issue_tracker_v3_dir()
                .join("issue-tracker-server.toml"),
            primary_port,
            gateway_control_port,
            gateway_worker_port,
            toml,
        })
    }

    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }
    pub fn primary_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.primary_port)
    }
    /// Compatibility alias for callers that only need the primary listener.
    pub fn listen_url(&self) -> String {
        self.primary_url()
    }
    pub fn gateway_control_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.gateway_control_port)
    }
    pub fn gateway_worker_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.gateway_worker_port)
    }
    pub fn toml(&self) -> &str {
        &self.toml
    }
}

pub fn render_server_config(
    data: &DataPaths,
    primary_port: u16,
) -> AppResult<IssueTrackerServerConfig> {
    let control_port = primary_port
        .checked_add(1)
        .ok_or_else(|| AppError::Config("Issue Tracker control listener port overflow".into()))?;
    let worker_port = primary_port
        .checked_add(2)
        .ok_or_else(|| AppError::Config("Issue Tracker worker listener port overflow".into()))?;
    render_server_config_with_gateway_ports(data, primary_port, control_port, worker_port)
}

pub fn render_server_config_with_gateway_ports(
    data: &DataPaths,
    primary_port: u16,
    gateway_control_port: u16,
    gateway_worker_port: u16,
) -> AppResult<IssueTrackerServerConfig> {
    IssueTrackerServerConfig::new(
        data,
        primary_port,
        gateway_control_port,
        gateway_worker_port,
    )
}

const MANAGER_CONTRACT_VERSION: &str = "issue-tracker-manager/v14";

const REQUIRED_MANAGER_FEATURES: &[v1::ManagerFeature] = &[
    v1::ManagerFeature::DurableOperations,
    v1::ManagerFeature::IssueAdminActions,
    v1::ManagerFeature::OwnershipRecovery,
    v1::ManagerFeature::OwnershipDiagnostics,
    v1::ManagerFeature::IssueOperationResolution,
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagerIdentity {
    schema_fingerprint: String,
    data_root_id: String,
    contract_fingerprint: String,
    backend_contract_fingerprint: String,
    root_operation_history_max_window_millis: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagerHealthSnapshot {
    pub health: v1::HealthStatus,
    pub activation: v1::ActivationStatus,
}

/// Typed private client for the standalone Issue Tracker gRPC server.
#[derive(Clone)]
pub struct IssueTrackerGrpcService {
    endpoint: String,
    manager_identity: Arc<RwLock<Option<ManagerIdentity>>>,
}

impl IssueTrackerGrpcService {
    /// Encodes the final manager request (including the server identity
    /// context) for the client pending journal. The following RPC call builds
    /// the same deterministic bytes from the same request value.
    pub fn encode_apply_issue_admin_action(
        &self,
        mut request: ManagerWriteRequest<v1::ApplyIssueAdminActionArgs>,
    ) -> ManagerResult<Vec<u8>> {
        let target = format!("issue:{}", request.payload.issue_id);
        request.payload.context =
            Some(self.write_context("manager_apply_issue_admin_action", &target, &request)?);
        let mut bytes = Vec::new();
        request.payload.encode(&mut bytes).map_err(|error| {
            IssueTrackerClientError::Transport(AppError::IssueTrackerServer(format!(
                "failed to encode admin request: {error}"
            )))
        })?;
        Ok(bytes)
    }

    pub fn encode_resolve_issue_operation(
        &self,
        mut request: ManagerWriteRequest<v1::ResolveIssueOperationArgs>,
    ) -> ManagerResult<Vec<u8>> {
        let target = format!("issue:{}", request.payload.issue_id);
        request.payload.context =
            Some(self.write_context("manager_resolve_issue_operation", &target, &request)?);
        let mut bytes = Vec::new();
        request.payload.encode(&mut bytes).map_err(|error| {
            IssueTrackerClientError::Transport(AppError::IssueTrackerServer(format!(
                "failed to encode resolution request: {error}"
            )))
        })?;
        Ok(bytes)
    }

    pub(crate) fn encode_transition_issue(
        &self,
        mut request: ManagerWriteRequest<v1::TransitionIssueArgs>,
    ) -> ManagerResult<Vec<u8>> {
        let target = format!("issue:{}", request.payload.issue_id);
        request.payload.context =
            Some(self.write_context("manager_transition_issue", &target, &request)?);
        Ok(request.payload.encode_to_vec())
    }

    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            manager_identity: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn contract_probe(&self, correlation_id: &str) -> AppResult<ManagerHealthSnapshot> {
        use crate::grpc::proto::issue_tracker::{
            manager::v1, rpc::v1::manager_info_service_client::ManagerInfoServiceClient,
        };
        require_lifecycle_correlation_id(correlation_id)?;
        let channel = crate::grpc::connect(&self.endpoint).await?;
        let mut client = ManagerInfoServiceClient::new(channel);
        let response = client
            .get_contract_info(v1::GetContractInfoArgs {
                context: Some(v1::ManagerReadContext {
                    correlation_id: correlation_id.into(),
                    schema_fingerprint: String::new(),
                    data_root_id: String::new(),
                    contract_fingerprint: String::new(),
                }),
            })
            .await
            .map_err(map_status)?
            .into_inner();
        let Some(v1::get_contract_info_result::Outcome::Success(info)) = response.outcome else {
            return Err(AppError::IssueTrackerServer(
                "Issue Tracker manager rejected health request".into(),
            ));
        };
        if info.contract_version != MANAGER_CONTRACT_VERSION {
            return Err(AppError::IssueTrackerServer(format!(
                "Issue Tracker manager contract is incompatible: {}",
                info.contract_version
            )));
        }
        let healthy = v1::HealthStatus::try_from(info.health).map_err(|_| {
            AppError::IssueTrackerServer("Issue Tracker manager returned unknown health".into())
        })?;
        let ready = v1::ActivationStatus::try_from(info.activation).map_err(|_| {
            AppError::IssueTrackerServer("Issue Tracker manager returned unknown activation".into())
        })?;
        for required in REQUIRED_MANAGER_FEATURES {
            if !info.features.contains(&(*required as i32)) {
                return Err(AppError::IssueTrackerServer(format!(
                    "Issue Tracker manager is missing required feature {required:?}"
                )));
            }
        }
        let mut manager_store = None;
        let mut backend_store = None;
        for store in info.store_identities {
            match v1::StoreRole::try_from(store.role).ok() {
                Some(v1::StoreRole::Manager) => manager_store = Some(store),
                Some(v1::StoreRole::Taskchampion) => backend_store = Some(store),
                _ => {}
            }
        }
        let manager_store = manager_store.ok_or_else(|| {
            AppError::IssueTrackerServer(
                "Issue Tracker manager health response has no manager store identity".into(),
            )
        })?;
        let backend_store = backend_store.ok_or_else(|| {
            AppError::IssueTrackerServer(
                "Issue Tracker manager health response has no backend store identity".into(),
            )
        })?;
        let data_root_id = manager_store.data_root_id.clone();
        if data_root_id.trim().is_empty()
            || backend_store.data_root_id != data_root_id
            || manager_store.store_pair_id.trim().is_empty()
            || backend_store.store_pair_id != manager_store.store_pair_id
            || manager_store.contract_fingerprint != info.contract_fingerprint
            || manager_store.schema_fingerprint != info.schema_fingerprint
            || backend_store.contract_fingerprint.trim().is_empty()
            || backend_store.schema_fingerprint.trim().is_empty()
        {
            return Err(AppError::IssueTrackerServer(
                "Issue Tracker manager health response has inconsistent store identities".into(),
            ));
        }
        if info.contract_fingerprint.trim().is_empty() || info.schema_fingerprint.trim().is_empty()
        {
            return Err(AppError::IssueTrackerServer(
                "Issue Tracker manager health response has an incomplete contract identity".into(),
            ));
        }
        let root_operation_history_max_window_millis = info
            .root_operation_history_max_window
            .as_ref()
            .map(|window| window.millis)
            .filter(|window| *window > 0)
            .ok_or_else(|| {
                AppError::IssueTrackerServer(
                    "Issue Tracker manager health response has no root history time window".into(),
                )
            })?;
        *self.manager_identity.write() = Some(ManagerIdentity {
            schema_fingerprint: info.schema_fingerprint,
            data_root_id,
            contract_fingerprint: info.contract_fingerprint,
            backend_contract_fingerprint: backend_store.contract_fingerprint,
            root_operation_history_max_window_millis,
        });
        Ok(ManagerHealthSnapshot {
            health: healthy,
            activation: ready,
        })
    }

    pub fn identity_snapshot(&self) -> Option<(String, String, String)> {
        self.manager_identity.read().as_ref().map(|identity| {
            (
                identity.schema_fingerprint.clone(),
                identity.data_root_id.clone(),
                identity.contract_fingerprint.clone(),
            )
        })
    }

    pub async fn health(&self, correlation_id: &str) -> AppResult<()> {
        let snapshot = self.contract_probe(correlation_id).await?;
        if snapshot.health == v1::HealthStatus::Healthy
            && snapshot.activation == v1::ActivationStatus::Ready
        {
            Ok(())
        } else {
            Err(AppError::IssueTrackerServer(format!(
                "manager is not ready (health={:?}, activation={:?})",
                snapshot.health, snapshot.activation
            )))
        }
    }

    pub async fn run_outbox_cycle(&self, correlation_id: &str) -> AppResult<()> {
        use crate::grpc::proto::issue_tracker::rpc::v1::{
            RunOutboxCycleArgs, outbox_maintenance_service_client::OutboxMaintenanceServiceClient,
            run_outbox_cycle_result,
        };
        require_lifecycle_correlation_id(correlation_id)?;
        let channel = crate::grpc::connect(&self.endpoint).await?;
        let mut client = OutboxMaintenanceServiceClient::new(channel);
        let response = client
            .run_outbox_cycle(RunOutboxCycleArgs {
                correlation_id: correlation_id.into(),
            })
            .await
            .map_err(map_status)?
            .into_inner();
        match response.outcome {
            Some(run_outbox_cycle_result::Outcome::Success(_)) => Ok(()),
            Some(run_outbox_cycle_result::Outcome::Error(error)) => {
                Err(AppError::IssueTrackerServer(format!(
                    "Issue Tracker outbox cycle failed: {}",
                    error.message
                )))
            }
            None => Err(AppError::IssueTrackerServer(
                "Issue Tracker outbox cycle has no outcome".into(),
            )),
        }
    }

    /// Registers the policy that the endpoint-bound Gateway must enforce for
    /// both readiness listings and claim admission.
    pub async fn register_gateway_policy(
        &self,
        control_endpoint: &str,
        correlation_id: &str,
        idempotency_key: &str,
        mode: v1::WorkEligibilityMode,
    ) -> AppResult<v1::GatewayRegistration> {
        use crate::grpc::proto::issue_tracker::{
            manager::v1::register_gateway_result,
            rpc::v1::gateway_control_service_client::GatewayControlServiceClient,
        };
        require_lifecycle_correlation_id(correlation_id)?;
        let identity = self.manager_identity.read().clone().ok_or_else(|| {
            AppError::IssueTrackerServer("manager identity is unavailable".into())
        })?;
        let channel = crate::grpc::connect(control_endpoint).await?;
        let mut client = GatewayControlServiceClient::new(channel);
        let response = client
            .register_gateway(v1::RegisterGatewayArgs {
                context: Some(v1::ManagerWriteContext {
                    method: MANAGER_REGISTER_GATEWAY_METHOD.into(),
                    target: GATEWAY_REGISTRATION_TARGET.into(),
                    schema_fingerprint: identity.schema_fingerprint,
                    idempotency_key: idempotency_key.into(),
                    actor_audit: Some(v1::ActorAudit {
                        actor_id: "lookback".into(),
                        actor_kind: crate::grpc::proto::issue_tracker::v1::ActorKind::System as i32,
                        ..Default::default()
                    }),
                    correlation_id: correlation_id.into(),
                    data_root_id: identity.data_root_id,
                    contract_fingerprint: identity.contract_fingerprint,
                }),
                policy: Some(v1::WorkEligibilityPolicy { mode: mode as i32 }),
            })
            .await
            .map_err(map_status)?
            .into_inner();
        match response.outcome {
            Some(register_gateway_result::Outcome::Success(registration)) => Ok(registration),
            Some(register_gateway_result::Outcome::Error(error)) => {
                Err(AppError::IssueTrackerServer(format!(
                    "Issue Tracker Gateway registration failed: {}",
                    error.message
                )))
            }
            None => Err(AppError::IssueTrackerServer(
                "Issue Tracker Gateway registration has no outcome".into(),
            )),
        }
    }

    /// Registers a Gateway policy and reconciles an uncertain response before
    /// accepting it. This keeps callers from treating a transport failure as
    /// proof that the server did not persist the registration.
    pub async fn register_and_verify_gateway_policy(
        &self,
        control_endpoint: &str,
        correlation_id: &str,
        idempotency_key: &str,
        expected: v1::WorkEligibilityMode,
    ) -> AppResult<v1::GatewayRegistration> {
        let registration = match self
            .register_gateway_policy(control_endpoint, correlation_id, idempotency_key, expected)
            .await
        {
            Ok(registration) => registration,
            Err(register_error) => self
                .get_gateway_registration(control_endpoint, correlation_id)
                .await
                .map_err(|_| register_error)?,
        };
        if gateway_registration_matches(&registration, expected) {
            Ok(registration)
        } else {
            Err(AppError::IssueTrackerServer(
                "Gateway registration does not match the requested work-eligibility policy".into(),
            ))
        }
    }

    /// Reads the endpoint-bound Gateway registration after a completed or
    /// uncertain RegisterGateway call.
    pub async fn get_gateway_registration(
        &self,
        control_endpoint: &str,
        correlation_id: &str,
    ) -> AppResult<v1::GatewayRegistration> {
        use crate::grpc::proto::issue_tracker::{
            manager::v1::get_gateway_registration_result,
            rpc::v1::gateway_control_service_client::GatewayControlServiceClient,
        };
        let context = self
            .read_context(correlation_id)
            .map_err(|error| AppError::IssueTrackerServer(error.to_string()))?;
        let channel = crate::grpc::connect(control_endpoint).await?;
        let mut client = GatewayControlServiceClient::new(channel);
        let response = client
            .get_gateway_registration(v1::GetGatewayRegistrationArgs {
                context: Some(context),
            })
            .await
            .map_err(map_status)?
            .into_inner();
        match response.outcome {
            Some(get_gateway_registration_result::Outcome::Success(registration)) => {
                Ok(registration)
            }
            Some(get_gateway_registration_result::Outcome::Error(error)) => {
                Err(AppError::IssueTrackerServer(format!(
                    "Issue Tracker Gateway registration is unavailable: {}",
                    error.message
                )))
            }
            None => Err(AppError::IssueTrackerServer(
                "Issue Tracker Gateway registration has no outcome".into(),
            )),
        }
    }

    async fn manager_channel(&self) -> ManagerResult<tonic::transport::Channel> {
        crate::grpc::connect(&self.endpoint)
            .await
            .map_err(IssueTrackerClientError::Transport)
    }

    fn read_context(&self, correlation_id: &str) -> ManagerResult<v1::ManagerReadContext> {
        if correlation_id_is_blank(correlation_id) {
            return Err(IssueTrackerClientError::InvalidRequest(
                "correlation ID must not be empty".into(),
            ));
        }
        let identity = self.manager_identity.read().clone().ok_or_else(|| {
            IssueTrackerClientError::NotHealthy(
                "call health successfully before issuing manager requests".into(),
            )
        })?;
        Ok(v1::ManagerReadContext {
            correlation_id: correlation_id.into(),
            schema_fingerprint: identity.schema_fingerprint,
            data_root_id: identity.data_root_id,
            contract_fingerprint: identity.contract_fingerprint,
        })
    }

    pub fn root_operation_history_max_window_millis(&self) -> ManagerResult<u64> {
        self.manager_identity
            .read()
            .as_ref()
            .map(|identity| identity.root_operation_history_max_window_millis)
            .ok_or_else(|| {
                IssueTrackerClientError::NotHealthy(
                    "call health successfully before issuing manager requests".into(),
                )
            })
    }

    pub fn primary_tracker_identity(&self) -> ManagerResult<PrimaryTrackerIdentity> {
        self.manager_identity
            .read()
            .as_ref()
            .map(|identity| PrimaryTrackerIdentity {
                schema_fingerprint: identity.schema_fingerprint.clone(),
                data_root_id: identity.data_root_id.clone(),
                contract_fingerprint: identity.contract_fingerprint.clone(),
            })
            .ok_or_else(|| {
                IssueTrackerClientError::NotHealthy(
                    "call health successfully before issuing manager requests".into(),
                )
            })
    }

    /// Only a health-probed v14 Manager is trusted to replay a matching scoped
    /// idempotency key before evaluating the mutation's current Issue CAS.
    pub(crate) fn supports_same_payload_manager_replay(
        &self,
        expected: &PrimaryTrackerIdentity,
    ) -> bool {
        self.manager_identity
            .read()
            .as_ref()
            .is_some_and(|identity| {
                identity.schema_fingerprint == expected.schema_fingerprint
                    && identity.data_root_id == expected.data_root_id
                    && identity.contract_fingerprint == expected.contract_fingerprint
                    && !identity.backend_contract_fingerprint.trim().is_empty()
            })
    }

    pub(crate) fn primary_backend_contract_fingerprint(&self) -> ManagerResult<String> {
        self.manager_identity
            .read()
            .as_ref()
            .map(|identity| identity.backend_contract_fingerprint.clone())
            .ok_or_else(|| {
                IssueTrackerClientError::NotHealthy(
                    "call health successfully before issuing manager requests".into(),
                )
            })
    }

    fn write_context<T>(
        &self,
        method: &'static str,
        target: &str,
        request: &ManagerWriteRequest<T>,
    ) -> ManagerResult<v1::ManagerWriteContext> {
        if request.correlation_id.trim().is_empty()
            || request.idempotency_key.trim().is_empty()
            || method.is_empty()
            || target.is_empty()
        {
            return Err(IssueTrackerClientError::InvalidRequest(
                "correlation ID, idempotency key, method, and target must not be empty".into(),
            ));
        }
        let actor_audit = request
            .actor_audit
            .clone()
            .filter(|actor| !actor.actor_id.trim().is_empty())
            .ok_or_else(|| {
                IssueTrackerClientError::InvalidRequest(
                    "actor audit with actor ID is required".into(),
                )
            })?;
        let read = self.read_context(&request.correlation_id)?;
        Ok(v1::ManagerWriteContext {
            method: method.into(),
            target: target.into(),
            schema_fingerprint: read.schema_fingerprint,
            idempotency_key: request.idempotency_key.clone(),
            actor_audit: Some(actor_audit),
            correlation_id: read.correlation_id,
            data_root_id: read.data_root_id,
            contract_fingerprint: read.contract_fingerprint,
        })
    }

    pub async fn get_operation(
        &self,
        correlation_id: &str,
        operation_id: impl Into<String>,
    ) -> ManagerResult<v1::DurableOperation> {
        let operation_id = operation_id.into();
        if operation_id.trim().is_empty() {
            return Err(IssueTrackerClientError::InvalidRequest(
                "operation ID must not be empty".into(),
            ));
        }
        let mut client = ManagerConsumerClient::new(self.manager_channel().await?);
        let response = client
            .get_operation(v1::GetOperationArgs {
                context: Some(self.read_context(correlation_id)?),
                operation_id,
            })
            .await
            .map_err(map_rpc_status)?
            .into_inner();
        map_manager_outcome(response.outcome)
    }

    /// Reads the backend operation named by the manager's actual
    /// `downstream_operation_id`; the caller must never synthesize this ID.
    pub async fn get_primary_operation(
        &self,
        correlation_id: &str,
        downstream_operation_id: &str,
    ) -> ManagerResult<issue::OperationRecord> {
        use issue::get_operation_result::Outcome;
        use rpc::operation_service_client::OperationServiceClient;

        if downstream_operation_id.trim().is_empty()
            || downstream_operation_id.len() > MAX_ISSUE_HISTORY_CURSOR_BYTES
            || downstream_operation_id.chars().any(char::is_control)
        {
            return Err(IssueTrackerClientError::InvalidRequest(
                "downstream operation ID is invalid".into(),
            ));
        }
        let mut client = OperationServiceClient::new(self.manager_channel().await?);
        let response = client
            .get_operation(issue::GetOperationArgs {
                context: Some(issue::RequestContext {
                    correlation_id: correlation_id.into(),
                    ..Default::default()
                }),
                downstream_operation_id: downstream_operation_id.into(),
            })
            .await
            .map_err(map_rpc_status)?
            .into_inner();
        match response.outcome {
            Some(Outcome::Success(operation)) => Ok(operation),
            Some(Outcome::Error(_)) | None => Err(IssueTrackerClientError::UnconfirmedOperation(
                "primary operation record was not returned".into(),
            )),
        }
    }

    /// Waits only for a durable terminal manager state; it never infers success.
    pub async fn await_operation(
        &self,
        correlation_id: &str,
        operation_id: impl Into<String>,
        timeout: Duration,
    ) -> ManagerResult<v1::DurableOperation> {
        let operation_id = operation_id.into();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let operation = self
                .get_operation(correlation_id, operation_id.clone())
                .await?;
            match v1::DurableOperationState::try_from(operation.state).ok() {
                Some(v1::DurableOperationState::Committed) => match operation.terminal_snapshot {
                    Some(v1::durable_operation::TerminalSnapshot::ResultSnapshot(ref snapshot))
                        if !snapshot.schema.trim().is_empty() && !snapshot.payload.is_empty() =>
                    {
                        return Ok(operation);
                    }
                    _ => {
                        return Err(IssueTrackerClientError::UnconfirmedOperation(format!(
                            "{operation_id} is COMMITTED without a valid result snapshot"
                        )));
                    }
                },
                Some(v1::DurableOperationState::Failed)
                | Some(v1::DurableOperationState::Unconfirmed)
                | Some(v1::DurableOperationState::ManualResolution) => {
                    return Err(IssueTrackerClientError::UnconfirmedOperation(format!(
                        "{operation_id} reached a non-success terminal state"
                    )));
                }
                Some(v1::DurableOperationState::Received)
                | Some(v1::DurableOperationState::Pending) => {}
                _ => {
                    return Err(IssueTrackerClientError::UnconfirmedOperation(format!(
                        "{operation_id} has an unknown state"
                    )));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(IssueTrackerClientError::UnconfirmedOperation(format!(
                    "timed out waiting for {operation_id}"
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Uses the Gateway worker endpoint's one-time claim capability to record
    /// the dedicated, versioned completion-to-review transition. Pass `None`
    /// only to replay the same exact idempotency request after uncertain
    /// delivery; this method never retries or creates a replacement capability.
    pub async fn complete_execution_to_awaiting_review(
        &self,
        gateway_worker_endpoint: &str,
        issue_id: &str,
        capability_id: Option<&str>,
        expected_revision: u64,
        idempotency_key: &str,
        completion: issue::AwaitingReviewCompletion,
    ) -> ManagerResult<rpc::IssueGatewayAwaitingReviewMutation> {
        let request = build_awaiting_review_request(
            issue_id,
            capability_id,
            expected_revision,
            idempotency_key,
            completion,
        )?;
        let channel = crate::grpc::connect(gateway_worker_endpoint)
            .await
            .map_err(|_| {
                IssueTrackerClientError::Gateway(AwaitingReviewFailureKind::ConnectionFailed)
            })?;
        let mut client = rpc::issue_gateway_client::IssueGatewayClient::new(channel);
        let response = client
            .complete_execution_to_awaiting_review(request.clone())
            .await
            .map_err(map_awaiting_review_rpc_status)?
            .into_inner();
        map_awaiting_review_response(response.outcome, &request)
    }
}

fn build_issue_history_request(
    correlation_id: &str,
    issue_id: &str,
    page_token: Option<&str>,
) -> ManagerResult<issue::ListIssueHistoryArgs> {
    if correlation_id.trim().is_empty()
        || correlation_id.len() > MAX_ISSUE_HISTORY_CURSOR_BYTES
        || correlation_id.chars().any(char::is_control)
        || issue_id.trim().is_empty()
        || issue_id.len() > MAX_ISSUE_HISTORY_ISSUE_ID_BYTES
        || issue_id.chars().any(char::is_control)
        || page_token.is_some_and(|token| {
            token.is_empty()
                || token.len() > MAX_ISSUE_HISTORY_CURSOR_BYTES
                || token.chars().any(char::is_control)
        })
    {
        return Err(IssueTrackerClientError::InvalidRequest(
            "canonical Issue history request identity or cursor is invalid".into(),
        ));
    }
    Ok(issue::ListIssueHistoryArgs {
        context: Some(issue::RequestContext {
            correlation_id: correlation_id.into(),
            ..Default::default()
        }),
        issue_id: issue_id.into(),
        page: Some(issue::PageRequest {
            page_size: ISSUE_HISTORY_PAGE_SIZE,
            page_token: page_token.map(str::to_owned),
        }),
    })
}

fn build_trusted_issue_history_request(
    correlation_id: &str,
    issue_id: &str,
    page_token: Option<&str>,
) -> ManagerResult<v1::ListIssueHistoryArgs> {
    let native_request = build_issue_history_request(correlation_id, issue_id, page_token)?;
    let page = native_request
        .page
        .expect("validated Issue history request always has a page");
    Ok(v1::ListIssueHistoryArgs {
        issue_id: native_request.issue_id,
        page: Some(v1::PageRequest {
            page_size: page.page_size,
            page_token: page.page_token,
        }),
        ..Default::default()
    })
}

fn build_trusted_downstream_operation_request(
    correlation_id: &str,
    issue_id: &str,
    manager_operation_id: &str,
    downstream_operation_id: &str,
) -> ManagerResult<v1::GetDownstreamOperationArgs> {
    if [
        correlation_id,
        issue_id,
        manager_operation_id,
        downstream_operation_id,
    ]
    .into_iter()
    .any(|value| {
        value.trim().is_empty()
            || value.len() > MAX_ISSUE_HISTORY_CURSOR_BYTES
            || value.chars().any(char::is_control)
    }) {
        return Err(IssueTrackerClientError::InvalidRequest(
            "trusted downstream operation identity is invalid".into(),
        ));
    }
    Ok(v1::GetDownstreamOperationArgs {
        issue_id: issue_id.into(),
        manager_operation_id: manager_operation_id.into(),
        downstream_operation_id: downstream_operation_id.into(),
        ..Default::default()
    })
}

pub(crate) fn build_awaiting_review_request(
    issue_id: &str,
    capability_id: Option<&str>,
    expected_revision: u64,
    idempotency_key: &str,
    completion: issue::AwaitingReviewCompletion,
) -> ManagerResult<rpc::IssueGatewayCompleteExecutionToAwaitingReviewRequest> {
    if issue_id.trim().is_empty()
        || issue_id.len() > MAX_AWAITING_REVIEW_ISSUE_ID_BYTES
        || issue_id.chars().any(char::is_control)
        || capability_id.is_some_and(|capability| {
            capability.trim().is_empty()
                || capability.len() > MAX_AWAITING_REVIEW_CAPABILITY_ID_BYTES
                || capability.chars().any(char::is_control)
        })
        || idempotency_key.trim().is_empty()
        || idempotency_key.len() > MAX_AWAITING_REVIEW_IDEMPOTENCY_KEY_BYTES
        || idempotency_key.chars().any(char::is_control)
        || expected_revision == u64::MAX
    {
        return Err(IssueTrackerClientError::InvalidRequest(
            "awaiting-review request identity, capability, or revision is invalid".into(),
        ));
    }
    if completion.run_id.trim().is_empty()
        || completion.run_id.len() > MAX_AWAITING_REVIEW_RUN_ID_BYTES
        || completion.run_id.chars().any(char::is_control)
    {
        return Err(IssueTrackerClientError::InvalidRequest(
            "awaiting-review run ID is invalid".into(),
        ));
    }
    let commit = completion.full_commit_id.as_bytes();
    if !matches!(commit.len(), 40 | 64) || !commit.iter().all(u8::is_ascii_hexdigit) {
        return Err(IssueTrackerClientError::InvalidRequest(
            "awaiting-review requires a full commit ID".into(),
        ));
    }
    if !awaiting_review_evidence_references_valid(&completion.evidence_references) {
        return Err(IssueTrackerClientError::InvalidRequest(
            "awaiting-review evidence references are required and bounded".into(),
        ));
    }
    if !awaiting_review_report_is_valid(completion.report.as_ref()) {
        return Err(IssueTrackerClientError::InvalidRequest(
            "awaiting-review completion report is invalid or exceeds limits".into(),
        ));
    }
    let request = rpc::IssueGatewayCompleteExecutionToAwaitingReviewRequest {
        issue_id: issue_id.into(),
        capability_id: capability_id.unwrap_or_default().into(),
        expected_revision: Some(expected_revision),
        idempotency_key: idempotency_key.into(),
        state_contract_version: AWAITING_REVIEW_STATE_CONTRACT.into(),
        completion: Some(completion),
    };
    require_awaiting_review_request_size(request.encoded_len())?;
    Ok(request)
}

pub(crate) fn awaiting_review_evidence_references_valid(references: &[String]) -> bool {
    !references.is_empty()
        && references.len() <= MAX_AWAITING_REVIEW_EVIDENCE_REFERENCES
        && references.iter().all(|reference| {
            !reference.trim().is_empty()
                && reference.len() <= MAX_AWAITING_REVIEW_EVIDENCE_REFERENCE_BYTES
                && !reference.chars().any(char::is_control)
        })
}

fn awaiting_review_report_is_valid(report: Option<&issue::CompletionReport>) -> bool {
    let Some(report) = report else {
        return false;
    };
    match report.result.as_ref() {
        Some(issue::completion_report::Result::Verified(verified)) => {
            !verified.evidence.is_empty()
                && verified.evidence.len() <= MAX_AWAITING_REVIEW_REPORT_EVIDENCE
                && verified.evidence.iter().all(|evidence| {
                    issue::CompletionEvidenceKind::try_from(evidence.kind)
                        .is_ok_and(|kind| kind != issue::CompletionEvidenceKind::Unspecified)
                        && bounded_nonempty_control_free(
                            &evidence.summary,
                            MAX_AWAITING_REVIEW_REPORT_TEXT_BYTES,
                        )
                        && evidence.uri.as_ref().is_none_or(|uri| {
                            bounded_nonempty_control_free(uri, MAX_AWAITING_REVIEW_REPORT_URI_BYTES)
                        })
                })
        }
        Some(issue::completion_report::Result::Unverified(unverified)) => {
            bounded_nonempty_control_free(&unverified.reason, MAX_AWAITING_REVIEW_REPORT_TEXT_BYTES)
                && bounded_nonempty_control_free(
                    &unverified.residual_risk,
                    MAX_AWAITING_REVIEW_REPORT_TEXT_BYTES,
                )
        }
        None => false,
    }
}

fn bounded_nonempty_control_free(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn require_awaiting_review_request_size(encoded_len: usize) -> ManagerResult<()> {
    if encoded_len > MAX_AWAITING_REVIEW_REQUEST_BYTES {
        return Err(IssueTrackerClientError::InvalidRequest(
            "awaiting-review request exceeds the encoded size limit".into(),
        ));
    }
    Ok(())
}

fn map_awaiting_review_response(
    outcome: Option<rpc::issue_gateway_complete_execution_to_awaiting_review_response::Outcome>,
    request: &rpc::IssueGatewayCompleteExecutionToAwaitingReviewRequest,
) -> ManagerResult<rpc::IssueGatewayAwaitingReviewMutation> {
    use rpc::issue_gateway_complete_execution_to_awaiting_review_response::Outcome;

    match outcome {
        Some(Outcome::Success(result)) => {
            let completion = request.completion.as_ref();
            let revision_matches = request
                .expected_revision
                .and_then(|revision| revision.checked_add(1))
                == Some(result.revision);
            if completion.is_some_and(|completion| {
                result.issue_id == request.issue_id
                    && revision_matches
                    && result.run_id == completion.run_id
                    && result.full_commit_id == completion.full_commit_id
                    && result.evidence_references == completion.evidence_references
                    && result.lease_closed
            }) {
                Ok(result)
            } else {
                Err(IssueTrackerClientError::UnconfirmedOperation(
                    "Gateway response does not confirm the requested awaiting-review transition"
                        .into(),
                ))
            }
        }
        Some(Outcome::Error(error)) => Err(IssueTrackerClientError::Gateway(
            AwaitingReviewFailureKind::from_gateway_code(&error.code),
        )),
        None => Err(IssueTrackerClientError::UnconfirmedOperation(
            "Gateway response has no awaiting-review outcome".into(),
        )),
    }
}

macro_rules! manager_write_method {
    ($name:ident, $client:path, $rpc:ident, $args:ty, $method:literal, $target:expr $(,)?) => {
        pub async fn $name(
            &self,
            mut request: ManagerWriteRequest<$args>,
        ) -> ManagerResult<v1::ManagerMutation> {
            let target = $target(&request.payload);
            request.payload.context = Some(self.write_context($method, &target, &request)?);
            let mut client = <$client>::new(self.manager_channel().await?);
            let response = client
                .$rpc(request.payload)
                .await
                .map_err(map_rpc_status)?
                .into_inner();
            map_manager_outcome(response.outcome)
        }
    };
}

macro_rules! manager_read_method {
    ($name:ident, $client:path, $rpc:ident, $args:ty, $result:ty $(,)?) => {
        pub async fn $name(&self, correlation_id: &str, mut args: $args) -> ManagerResult<$result> {
            args.context = Some(self.read_context(correlation_id)?);
            let mut client = <$client>::new(self.manager_channel().await?);
            let response = client
                .$rpc(args)
                .await
                .map_err(map_rpc_status)?
                .into_inner();
            map_manager_outcome(response.outcome)
        }
    };
}

macro_rules! impl_manager_outcome {
    ($outcome:path, $success:ty) => {
        impl ManagerOutcome<$success> for $outcome {
            fn into_result(self) -> ManagerResult<$success> {
                match self {
                    Self::Success(value) => Ok(value),
                    Self::Error(error) => Err(IssueTrackerClientError::Manager(Box::new(error))),
                }
            }
        }
    };
}

impl_manager_outcome!(v1::get_operation_result::Outcome, v1::DurableOperation);
impl_manager_outcome!(v1::get_issue_with_sync_result::Outcome, v1::IssueWithSync);
impl_manager_outcome!(
    v1::get_issue_relations_result::Outcome,
    v1::IssueRelationsWithSync
);
impl_manager_outcome!(
    v1::list_issues_with_sync_result::Outcome,
    v1::IssueWithSyncPage
);
impl_manager_outcome!(v1::get_candidate_result::Outcome, v1::Candidate);
impl_manager_outcome!(v1::list_candidates_result::Outcome, v1::CandidatePage);
impl_manager_outcome!(
    v1::list_review_candidates_result::Outcome,
    v1::ReviewCandidatePage
);
impl_manager_outcome!(v1::get_extraction_run_result::Outcome, v1::ExtractionRun);
impl_manager_outcome!(
    v1::list_extraction_runs_result::Outcome,
    v1::ExtractionRunPage
);
impl_manager_outcome!(v1::create_issue_result::Outcome, v1::ManagerMutation);
impl_manager_outcome!(v1::update_issue_result::Outcome, v1::ManagerMutation);
impl_manager_outcome!(v1::transition_issue_result::Outcome, v1::ManagerMutation);
impl_manager_outcome!(v1::update_candidate_result::Outcome, v1::ManagerMutation);
impl_manager_outcome!(v1::add_issue_relation_result::Outcome, v1::ManagerMutation);
impl_manager_outcome!(v1::resolve_candidate_result::Outcome, v1::ResolutionReceipt);
impl_manager_outcome!(
    v1::list_operation_history_result::Outcome,
    v1::OperationHistoryPage
);
impl_manager_outcome!(
    v1::list_ownership_diagnostics_result::Outcome,
    v1::OwnershipDiagnosticsPage
);
impl_manager_outcome!(
    v1::apply_issue_admin_action_result::Outcome,
    v1::ManagerMutation
);
impl_manager_outcome!(
    v1::resolve_issue_operation_result::Outcome,
    v1::ManagerMutation
);

fn issue_relation_target(args: &v1::AddIssueRelationArgs) -> String {
    let issue_id = match crate::grpc::proto::issue_tracker::v1::RelationKind::try_from(args.kind) {
        Ok(crate::grpc::proto::issue_tracker::v1::RelationKind::Blocks) => &args.target_issue_id,
        _ => &args.source_issue_id,
    };
    format!("issue:{issue_id}")
}

impl IssueTrackerGrpcService {
    manager_read_method!(
        get_issue_with_sync,
        ManagerConsumerClient,
        get_issue_with_sync,
        v1::GetIssueWithSyncArgs,
        v1::IssueWithSync,
    );
    manager_read_method!(
        get_issue_relations,
        ManagerConsumerClient,
        get_issue_relations,
        v1::GetIssueRelationsArgs,
        v1::IssueRelationsWithSync,
    );
    manager_read_method!(
        list_issues_with_sync,
        ManagerConsumerClient,
        list_issues_with_sync,
        v1::ListIssuesWithSyncArgs,
        v1::IssueWithSyncPage,
    );
    manager_write_method!(
        create_issue,
        ManagerConsumerClient,
        create_issue,
        v1::CreateIssueArgs,
        "manager_create_issue",
        |args: &v1::CreateIssueArgs| format!("issue:{}", args.issue_id)
    );
    manager_write_method!(
        update_issue,
        ManagerConsumerClient,
        update_issue,
        v1::UpdateIssueArgs,
        "manager_update_issue",
        |args: &v1::UpdateIssueArgs| format!("issue:{}", args.issue_id)
    );
    manager_write_method!(
        add_issue_relation,
        ManagerConsumerClient,
        add_issue_relation,
        v1::AddIssueRelationArgs,
        "manager_add_issue_relation",
        issue_relation_target
    );
    manager_write_method!(
        transition_issue,
        ManagerConsumerClient,
        transition_issue,
        v1::TransitionIssueArgs,
        "manager_transition_issue",
        |args: &v1::TransitionIssueArgs| format!("issue:{}", args.issue_id)
    );
    manager_read_method!(
        get_candidate,
        ManagerConsumerClient,
        get_candidate,
        v1::GetCandidateArgs,
        v1::Candidate,
    );
    manager_read_method!(
        list_candidates,
        ManagerConsumerClient,
        list_candidates,
        v1::ListCandidatesArgs,
        v1::CandidatePage,
    );
    manager_read_method!(
        list_review_candidates,
        ManagerConsumerClient,
        list_review_candidates,
        v1::ListReviewCandidatesArgs,
        v1::ReviewCandidatePage,
    );
    manager_write_method!(
        update_candidate,
        ManagerConsumerClient,
        update_candidate,
        v1::UpdateCandidateArgs,
        "manager_update_candidate",
        |args: &v1::UpdateCandidateArgs| format!("candidate/{}", args.candidate_id)
    );
    manager_read_method!(
        get_extraction_run,
        ManagerConsumerClient,
        get_extraction_run,
        v1::GetExtractionRunArgs,
        v1::ExtractionRun,
    );
    manager_read_method!(
        list_extraction_runs,
        ManagerConsumerClient,
        list_extraction_runs,
        v1::ListExtractionRunsArgs,
        v1::ExtractionRunPage,
    );
    pub async fn resolve_candidate(
        &self,
        mut request: ManagerWriteRequest<v1::ResolveCandidateArgs>,
    ) -> ManagerResult<v1::ResolutionReceipt> {
        let target = format!("candidate/{}", request.payload.candidate_id);
        request.payload.context =
            Some(self.write_context("manager_resolve_candidate", &target, &request)?);
        let mut client = ManagerConsumerClient::new(self.manager_channel().await?);
        let response = client
            .resolve_candidate(request.payload)
            .await
            .map_err(map_rpc_status)?
            .into_inner();
        map_manager_outcome(response.outcome)
    }

    manager_read_method!(
        list_operation_history,
        ManagerConsumerClient,
        list_operation_history,
        v1::ListOperationHistoryArgs,
        v1::OperationHistoryPage,
    );

    pub async fn list_ownership_diagnostics(
        &self,
        correlation_id: &str,
        args: v1::ListOwnershipDiagnosticsArgs,
    ) -> ManagerResult<v1::OwnershipDiagnosticsPage> {
        let mut client = ManagerConsumerClient::new(self.manager_channel().await?);
        let mut args = args;
        args.context = Some(self.read_context(correlation_id)?);
        let response = client
            .list_ownership_diagnostics(args)
            .await
            .map_err(map_rpc_status)?
            .into_inner();
        map_manager_outcome(response.outcome)
    }

    /// Reads the complete canonical history for one Issue through a single
    /// server-pinned pagination snapshot. Completion authorization uses this
    /// history to prove the run marker and the exact resulting state revision.
    pub async fn list_issue_history(
        &self,
        correlation_id: &str,
        issue_id: &str,
    ) -> ManagerResult<Vec<issue::IssueHistoryEntry>> {
        use issue::list_issue_history_result::Outcome;
        use rpc::activity_service_client::ActivityServiceClient;

        let mut entries = Vec::new();
        let mut page_token = None;
        let mut seen_tokens = HashSet::new();
        let mut snapshot = None;
        let mut page_count = 0usize;
        let mut history_response_bytes = 0usize;
        loop {
            page_count = next_issue_history_page_count(page_count)?;
            let request =
                build_issue_history_request(correlation_id, issue_id, page_token.as_deref())?;
            let mut client = ActivityServiceClient::new(self.manager_channel().await?);
            let response = client
                .list_issue_history(request)
                .await
                .map_err(map_rpc_status)?
                .into_inner();
            let response_encoded_len = response.encoded_len();
            let page = match response.outcome {
                Some(Outcome::Success(page)) => page,
                Some(Outcome::Error(_)) | None => {
                    return Err(IssueTrackerClientError::UnconfirmedOperation(
                        "canonical Issue history was not returned".into(),
                    ));
                }
            };
            let page_info = page.page.ok_or_else(|| {
                IssueTrackerClientError::UnconfirmedOperation(
                    "canonical Issue history page has no snapshot".into(),
                )
            })?;
            let page_snapshot = page_info.snapshot.ok_or_else(|| {
                IssueTrackerClientError::UnconfirmedOperation(
                    "canonical Issue history page has no snapshot".into(),
                )
            })?;
            if page_snapshot.snapshot_id.trim().is_empty()
                || snapshot
                    .as_ref()
                    .is_some_and(|previous| previous != &page_snapshot)
            {
                return Err(IssueTrackerClientError::UnconfirmedOperation(
                    "canonical Issue history pagination changed snapshots".into(),
                ));
            }
            snapshot = Some(page_snapshot);
            append_issue_history_page(
                &mut entries,
                page.entries,
                response_encoded_len,
                &mut history_response_bytes,
            )?;
            page_token = page_info.next_page_token;
            let Some(token) = page_token.as_ref() else {
                break;
            };
            if !seen_tokens.insert(token.clone()) {
                return Err(IssueTrackerClientError::UnconfirmedOperation(
                    "canonical Issue history pagination repeated a cursor".into(),
                ));
            }
        }
        Ok(entries)
    }

    /// Reads canonical Issue history through the primary trusted Manager
    /// boundary. The native ActivityService wrapper remains available for
    /// existing non-completion callers, but completion proof uses this route.
    pub async fn list_trusted_issue_history(
        &self,
        correlation_id: &str,
        issue_id: &str,
    ) -> ManagerResult<Vec<issue::IssueHistoryEntry>> {
        use rpc::manager_trusted_read_service_client::ManagerTrustedReadServiceClient;
        use v1::list_issue_history_result::Outcome;

        let mut entries = Vec::new();
        let mut page_token = None;
        let mut seen_tokens = HashSet::new();
        let mut snapshot = None;
        let mut page_count = 0usize;
        let mut history_response_bytes = 0usize;
        loop {
            page_count = next_issue_history_page_count(page_count)?;
            let mut request = build_trusted_issue_history_request(
                correlation_id,
                issue_id,
                page_token.as_deref(),
            )?;
            request.context = Some(self.read_context(correlation_id)?);
            let mut client = ManagerTrustedReadServiceClient::new(self.manager_channel().await?);
            let response = client
                .list_issue_history(request)
                .await
                .map_err(map_trusted_read_status)?
                .into_inner();
            let response_encoded_len = response.encoded_len();
            let page = match response.outcome {
                Some(Outcome::Success(page)) => page,
                Some(Outcome::Error(_)) | None => {
                    return Err(trusted_read_unconfirmed(
                        "trusted canonical Issue history was not returned",
                    ));
                }
            };
            let page_info = page.page.ok_or_else(|| {
                trusted_read_unconfirmed("trusted Issue history page has no snapshot")
            })?;
            let page_snapshot = page_info.snapshot.ok_or_else(|| {
                trusted_read_unconfirmed("trusted Issue history page has no snapshot")
            })?;
            if page_snapshot.snapshot_id.trim().is_empty()
                || snapshot
                    .as_ref()
                    .is_some_and(|previous| previous != &page_snapshot)
            {
                return Err(trusted_read_unconfirmed(
                    "trusted Issue history pagination changed snapshots",
                ));
            }
            snapshot = Some(page_snapshot);
            append_issue_history_page(
                &mut entries,
                page.entries,
                response_encoded_len,
                &mut history_response_bytes,
            )?;
            page_token = page_info.next_page_token;
            let Some(token) = page_token.as_ref() else {
                break;
            };
            if !seen_tokens.insert(token.clone()) {
                return Err(trusted_read_unconfirmed(
                    "trusted Issue history pagination repeated a cursor",
                ));
            }
        }
        Ok(entries)
    }

    /// Reads a native operation together with the Manager row that binds its
    /// canonical Issue scope and request identity.
    pub async fn get_trusted_primary_operation(
        &self,
        correlation_id: &str,
        issue_id: &str,
        manager_operation_id: &str,
        downstream_operation_id: &str,
    ) -> ManagerResult<v1::TrustedDownstreamOperation> {
        use rpc::manager_trusted_read_service_client::ManagerTrustedReadServiceClient;
        use v1::get_downstream_operation_result::Outcome;

        let mut request = build_trusted_downstream_operation_request(
            correlation_id,
            issue_id,
            manager_operation_id,
            downstream_operation_id,
        )?;
        request.context = Some(self.read_context(correlation_id)?);
        let mut client = ManagerTrustedReadServiceClient::new(self.manager_channel().await?);
        let response = client
            .get_downstream_operation(request)
            .await
            .map_err(map_trusted_read_status)?
            .into_inner();
        match response.outcome {
            Some(Outcome::Success(operation)) => Ok(operation),
            Some(Outcome::Error(_)) | None => Err(trusted_read_unconfirmed(
                "trusted downstream operation proof was not returned",
            )),
        }
    }

    pub async fn apply_issue_admin_action(
        &self,
        mut request: ManagerWriteRequest<v1::ApplyIssueAdminActionArgs>,
    ) -> ManagerResult<v1::ManagerMutation> {
        let target = format!("issue:{}", request.payload.issue_id);
        request.payload.context =
            Some(self.write_context("manager_apply_issue_admin_action", &target, &request)?);
        let mut client = ManagerConsumerClient::new(self.manager_channel().await?);
        let response = client
            .apply_issue_admin_action(request.payload)
            .await
            .map_err(map_rpc_status)?
            .into_inner();
        map_manager_outcome::<v1::ManagerMutation, _>(response.outcome)
    }

    pub async fn resolve_issue_operation(
        &self,
        mut request: ManagerWriteRequest<v1::ResolveIssueOperationArgs>,
    ) -> ManagerResult<v1::ManagerMutation> {
        let target = format!("issue:{}", request.payload.issue_id);
        request.payload.context =
            Some(self.write_context("manager_resolve_issue_operation", &target, &request)?);
        let mut client = ManagerConsumerClient::new(self.manager_channel().await?);
        let response = client
            .resolve_issue_operation(request.payload)
            .await
            .map_err(map_rpc_status)?
            .into_inner();
        map_manager_outcome::<v1::ManagerMutation, _>(response.outcome)
    }
}

fn next_issue_history_page_count(completed_pages: usize) -> ManagerResult<usize> {
    if completed_pages >= MAX_ISSUE_HISTORY_PAGES {
        return Err(IssueTrackerClientError::InvalidRequest(
            "canonical Issue history exceeds its page limit".into(),
        ));
    }
    Ok(completed_pages + 1)
}

fn append_issue_history_page(
    entries: &mut Vec<issue::IssueHistoryEntry>,
    page_entries: Vec<issue::IssueHistoryEntry>,
    response_encoded_len: usize,
    history_response_bytes: &mut usize,
) -> ManagerResult<()> {
    let next_bytes = history_response_bytes
        .checked_add(response_encoded_len)
        .filter(|bytes| *bytes <= MAX_ISSUE_HISTORY_RESPONSE_BYTES)
        .ok_or_else(|| {
            IssueTrackerClientError::InvalidRequest(
                "canonical Issue history exceeds its encoded-size limit".into(),
            )
        })?;
    let next_entry_count = entries
        .len()
        .checked_add(page_entries.len())
        .filter(|count| *count <= MAX_ISSUE_HISTORY_ENTRIES)
        .ok_or_else(|| {
            IssueTrackerClientError::InvalidRequest(
                "canonical Issue history exceeds its entry limit".into(),
            )
        })?;
    *history_response_bytes = next_bytes;
    entries.reserve(next_entry_count - entries.len());
    entries.extend(page_entries);
    Ok(())
}

fn map_status(status: tonic::Status) -> AppError {
    let diagnostic = match status.code() {
        Code::Unimplemented => {
            "Issue Tracker server does not implement a required RPC; update the Issue Tracker server"
        }
        Code::Unavailable => "Issue Tracker gRPC service is unavailable",
        Code::DeadlineExceeded => "Issue Tracker gRPC request exceeded its deadline",
        Code::Cancelled => "Issue Tracker gRPC request was cancelled",
        _ => "Issue Tracker gRPC transport failed",
    };
    AppError::IssueTrackerServer(diagnostic.into())
}

fn map_rpc_status(status: tonic::Status) -> IssueTrackerClientError {
    IssueTrackerClientError::Transport(map_status(status))
}

fn map_trusted_read_status(status: tonic::Status) -> IssueTrackerClientError {
    let diagnostic = match status.code() {
        Code::Unimplemented | Code::Unavailable => {
            "trusted primary Issue Tracker evidence service is unavailable"
        }
        _ => "trusted primary Issue Tracker evidence read failed",
    };
    trusted_read_unconfirmed(diagnostic)
}

fn trusted_read_unconfirmed(message: &'static str) -> IssueTrackerClientError {
    IssueTrackerClientError::UnconfirmedOperation(message.into())
}

fn map_awaiting_review_rpc_status(status: tonic::Status) -> IssueTrackerClientError {
    let kind = match status.code() {
        Code::Unimplemented => AwaitingReviewFailureKind::RpcUnimplemented,
        Code::Unavailable => AwaitingReviewFailureKind::RpcUnavailable,
        Code::DeadlineExceeded => AwaitingReviewFailureKind::RpcDeadlineExceeded,
        _ => AwaitingReviewFailureKind::RpcFailure,
    };
    IssueTrackerClientError::Gateway(kind)
}

fn correlation_id_is_blank(correlation_id: &str) -> bool {
    correlation_id.trim().is_empty()
}

fn require_lifecycle_correlation_id(correlation_id: &str) -> AppResult<()> {
    if correlation_id_is_blank(correlation_id) {
        return Err(AppError::Config(
            "Issue Tracker correlation ID must not be empty".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn gateway_registration_match_requires_the_expected_policy_mode() {
        let registration = v1::GatewayRegistration {
            policy: Some(v1::WorkEligibilityPolicy {
                mode: v1::WorkEligibilityMode::ReviewedOnly as i32,
            }),
            ..Default::default()
        };

        assert!(gateway_registration_matches(
            &registration,
            v1::WorkEligibilityMode::ReviewedOnly
        ));
        assert!(!gateway_registration_matches(
            &registration,
            v1::WorkEligibilityMode::IncludeAutoRegistered
        ));
    }

    #[test]
    fn manager_contract_requires_v14() {
        assert_eq!(MANAGER_CONTRACT_VERSION, "issue-tracker-manager/v14");
    }

    #[test]
    fn add_issue_relation_uses_the_server_relation_anchor_as_its_manager_target() {
        let args = v1::AddIssueRelationArgs {
            source_issue_id: "new-issue".into(),
            target_issue_id: "stale-issue".into(),
            kind: crate::grpc::proto::issue_tracker::v1::RelationKind::Supersedes as i32,
            ..Default::default()
        };

        assert_eq!(issue_relation_target(&args), "issue:new-issue");

        let blocked = v1::AddIssueRelationArgs {
            kind: crate::grpc::proto::issue_tracker::v1::RelationKind::Blocks as i32,
            ..args
        };
        assert_eq!(issue_relation_target(&blocked), "issue:stale-issue");
    }

    #[test]
    fn read_context_uses_the_identity_returned_by_health() {
        let service = IssueTrackerGrpcService::new("http://127.0.0.1:50052");
        *service.manager_identity.write() = Some(ManagerIdentity {
            schema_fingerprint: "schema".into(),
            data_root_id: "root".into(),
            contract_fingerprint: "contract".into(),
            backend_contract_fingerprint: "backend-contract".into(),
            root_operation_history_max_window_millis: 1,
        });

        let context = service.read_context("correlation").unwrap();
        assert_eq!(context.correlation_id, "correlation");
        assert_eq!(context.schema_fingerprint, "schema");
        assert_eq!(context.data_root_id, "root");
        assert_eq!(context.contract_fingerprint, "contract");
        assert_eq!(
            service.primary_backend_contract_fingerprint().unwrap(),
            "backend-contract"
        );
        let identity = PrimaryTrackerIdentity {
            schema_fingerprint: "schema".into(),
            data_root_id: "root".into(),
            contract_fingerprint: "contract".into(),
        };
        assert!(service.supports_same_payload_manager_replay(&identity));
        assert!(
            !service.supports_same_payload_manager_replay(&PrimaryTrackerIdentity {
                data_root_id: "different-root".into(),
                ..identity.clone()
            })
        );
        assert!(
            !IssueTrackerGrpcService::new("http://127.0.0.1:50053")
                .supports_same_payload_manager_replay(&identity)
        );
    }

    #[test]
    fn issue_history_reads_are_bounded_and_preserve_the_server_cursor() {
        let first = build_issue_history_request("correlation", "issue-1", None).unwrap();
        assert_eq!(first.issue_id, "issue-1");
        assert_eq!(first.page.as_ref().unwrap().page_size, 100);
        assert!(first.page.as_ref().unwrap().page_token.is_none());

        let next = build_issue_history_request("correlation", "issue-1", Some("cursor-1")).unwrap();
        assert_eq!(
            next.page.as_ref().unwrap().page_token.as_deref(),
            Some("cursor-1")
        );
        assert!(build_issue_history_request(" ", "issue-1", None).is_err());
        assert!(build_issue_history_request("correlation", "", None).is_err());
        assert!(build_issue_history_request("correlation", &"i".repeat(4_097), None).is_err());
        let mut pages = 0;
        for _ in 0..MAX_ISSUE_HISTORY_PAGES {
            pages = next_issue_history_page_count(pages).unwrap();
        }
        assert!(next_issue_history_page_count(pages).is_err());
    }

    #[test]
    fn trusted_read_requests_keep_manager_binding_optional_only_for_history() {
        let history =
            build_trusted_issue_history_request("correlation", "issue-1", Some("cursor-1"))
                .unwrap();
        assert!(history.context.is_none());
        assert_eq!(history.issue_id, "issue-1");
        assert_eq!(
            history.page.as_ref().unwrap().page_token.as_deref(),
            Some("cursor-1")
        );
        assert!(history.manager_operation_id.is_none());
        assert!(history.downstream_operation_id.is_none());

        let operation = build_trusted_downstream_operation_request(
            "correlation",
            "issue-1",
            "manager-op-1",
            "tracker-op-1",
        )
        .unwrap();
        assert_eq!(operation.issue_id, "issue-1");
        assert_eq!(operation.manager_operation_id, "manager-op-1");
        assert_eq!(operation.downstream_operation_id, "tracker-op-1");

        assert!(
            build_trusted_downstream_operation_request(
                "correlation",
                "issue-1",
                " ",
                "tracker-op-1",
            )
            .is_err()
        );
        assert!(
            build_trusted_downstream_operation_request(
                &"c".repeat(MAX_ISSUE_HISTORY_CURSOR_BYTES + 1),
                "issue-1",
                "manager-op-1",
                "tracker-op-1",
            )
            .is_err()
        );

        let error = map_trusted_read_status(tonic::Status::unimplemented("secret server detail"));
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("unavailable"));
        assert!(!diagnostic.contains("secret server detail"));
    }

    #[test]
    fn transition_issue_wire_encoding_includes_the_exact_manager_context_and_payload() {
        let service = IssueTrackerGrpcService::new("http://127.0.0.1:50052");
        *service.manager_identity.write() = Some(ManagerIdentity {
            schema_fingerprint: "schema-fingerprint".into(),
            data_root_id: "data-root".into(),
            contract_fingerprint: "contract".into(),
            backend_contract_fingerprint: "backend-contract".into(),
            root_operation_history_max_window_millis: 1,
        });
        let actor = v1::ActorAudit {
            actor_id: "lookback-user".into(),
            actor_kind: crate::grpc::proto::issue_tracker::v1::ActorKind::Human as i32,
            display_name: Some("Lookback coding agent".into()),
            reason: Some("approved [coding-agent-intent-sha256=abc]".into()),
            ..Default::default()
        };
        let request = ManagerWriteRequest {
            correlation_id: "coding-agent-completion-auth".into(),
            idempotency_key: "coding-agent-completion-v1-derived".into(),
            actor_audit: Some(actor.clone()),
            payload: v1::TransitionIssueArgs {
                issue_id: "issue-17".into(),
                expected_revision: Some(10),
                target_state: crate::grpc::proto::issue_tracker::v1::IssueState::Done as i32,
                ..Default::default()
            },
        };

        let encoded = service.encode_transition_issue(request.clone()).unwrap();
        let decoded = v1::TransitionIssueArgs::decode(encoded.as_slice()).unwrap();
        let context = decoded.context.as_ref().unwrap();
        assert_eq!(context.method, "manager_transition_issue");
        assert_eq!(context.target, "issue:issue-17");
        assert_eq!(context.idempotency_key, request.idempotency_key);
        assert_eq!(context.correlation_id, request.correlation_id);
        assert_eq!(context.schema_fingerprint, "schema-fingerprint");
        assert_eq!(context.data_root_id, "data-root");
        assert_eq!(context.contract_fingerprint, "contract");
        assert_eq!(context.actor_audit.as_ref(), Some(&actor));
        assert_eq!(decoded.issue_id, "issue-17");
        assert_eq!(decoded.expected_revision, Some(10));
        assert_eq!(
            decoded.target_state,
            crate::grpc::proto::issue_tracker::v1::IssueState::Done as i32
        );
        assert!(decoded.lease_handle.is_none());
        assert!(decoded.deferred_review_at.is_none());
        assert!(decoded.claim_revision.is_none());
        assert!(decoded.terminal_report.is_none());
        assert!(decoded.awaiting_review.is_none());
    }

    #[test]
    fn issue_history_rejects_oversized_response_before_appending_entries() {
        let entry = issue::IssueHistoryEntry {
            reason: "x".repeat(MAX_ISSUE_HISTORY_RESPONSE_BYTES + 1),
            ..Default::default()
        };
        let response = issue::ListIssueHistoryResult {
            outcome: Some(issue::list_issue_history_result::Outcome::Success(
                issue::HistoryPage {
                    entries: vec![entry.clone()],
                    page: Some(issue::PageInfo {
                        next_page_token: Some("cursor".into()),
                        snapshot: Some(issue::PageSnapshot {
                            snapshot_id: "snapshot".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                },
            )),
        };
        let encoded_len = response.encoded_len();
        assert!(encoded_len > MAX_ISSUE_HISTORY_RESPONSE_BYTES);

        let mut entries = Vec::new();
        let mut accumulated = 0;
        assert!(matches!(
            append_issue_history_page(
                &mut entries,
                vec![entry],
                encoded_len,
                &mut accumulated,
            ),
            Err(IssueTrackerClientError::InvalidRequest(message))
                if message == "canonical Issue history exceeds its encoded-size limit"
        ));
        assert!(entries.is_empty());
        assert_eq!(accumulated, 0);
    }

    #[test]
    fn issue_history_budget_accumulates_page_wrappers_and_cursors() {
        let make_page = |reason_size: usize, cursor: &str| {
            let entry = issue::IssueHistoryEntry {
                reason: "x".repeat(reason_size),
                ..Default::default()
            };
            let response = issue::ListIssueHistoryResult {
                outcome: Some(issue::list_issue_history_result::Outcome::Success(
                    issue::HistoryPage {
                        entries: vec![entry.clone()],
                        page: Some(issue::PageInfo {
                            next_page_token: Some(cursor.into()),
                            snapshot: Some(issue::PageSnapshot {
                                snapshot_id: "snapshot".into(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                    },
                )),
            };
            (entry, response.encoded_len())
        };
        let (first, first_len) = make_page(MAX_ISSUE_HISTORY_RESPONSE_BYTES / 2 + 100, "cursor-1");
        let (second, second_len) =
            make_page(MAX_ISSUE_HISTORY_RESPONSE_BYTES / 2 + 100, "cursor-2");
        assert!(first_len < MAX_ISSUE_HISTORY_RESPONSE_BYTES);
        assert!(second_len < MAX_ISSUE_HISTORY_RESPONSE_BYTES);

        let mut entries = Vec::new();
        let mut accumulated = 0;
        append_issue_history_page(&mut entries, vec![first], first_len, &mut accumulated).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            append_issue_history_page(&mut entries, vec![second], second_len, &mut accumulated),
            Err(IssueTrackerClientError::InvalidRequest(message))
                if message == "canonical Issue history exceeds its encoded-size limit"
        ));
        assert_eq!(entries.len(), 1, "the over-budget page is not appended");
        assert_eq!(accumulated, first_len);
    }

    #[tokio::test]
    async fn health_and_outbox_reject_empty_correlation_as_config() {
        let service = IssueTrackerGrpcService::new("http://127.0.0.1:1");
        assert!(matches!(
            service.health("   ").await,
            Err(AppError::Config(_))
        ));
        assert!(matches!(
            service.run_outbox_cycle("").await,
            Err(AppError::Config(_))
        ));
    }

    #[test]
    fn write_context_rejects_incomplete_audit_scope() {
        let service = IssueTrackerGrpcService::new("http://127.0.0.1:50052");
        let request = ManagerWriteRequest {
            correlation_id: "correlation".into(),
            idempotency_key: String::new(),
            actor_audit: None,
            payload: (),
        };

        assert!(matches!(
            service.write_context("candidate.create", "candidate", &request),
            Err(IssueTrackerClientError::InvalidRequest(_))
        ));
    }

    #[test]
    fn write_context_preserves_the_runner_manager_method_key_and_target() {
        let service = IssueTrackerGrpcService::new("http://127.0.0.1:50052");
        *service.manager_identity.write() = Some(ManagerIdentity {
            schema_fingerprint: "schema".into(),
            data_root_id: "root".into(),
            contract_fingerprint: "contract".into(),
            backend_contract_fingerprint: "backend-contract".into(),
            root_operation_history_max_window_millis: 1,
        });
        let request = ManagerWriteRequest {
            correlation_id: "correlation".into(),
            idempotency_key: "idempotency".into(),
            actor_audit: Some(v1::ActorAudit {
                actor_id: "lookback".into(),
                actor_kind: crate::grpc::proto::issue_tracker::v1::ActorKind::System as i32,
                ..Default::default()
            }),
            payload: (),
        };

        let context = service
            .write_context("manager_create_issue", "issue:issue-1", &request)
            .unwrap();

        assert_eq!(context.method, "manager_create_issue");
        assert_eq!(context.target, "issue:issue-1");
    }

    #[test]
    fn gateway_registration_uses_the_server_manager_dispatch_key() {
        assert_eq!(MANAGER_REGISTER_GATEWAY_METHOD, "manager_register_gateway");
        assert_eq!(GATEWAY_REGISTRATION_TARGET, "gateway/lookback-local");
    }

    #[test]
    fn review_candidate_outcome_maps_success_and_manager_error() {
        let success =
            v1::list_review_candidates_result::Outcome::Success(v1::ReviewCandidatePage::default())
                .into_result()
                .unwrap();
        assert!(success.rows.is_empty());

        let error = v1::list_review_candidates_result::Outcome::Error(v1::ManagerError {
            code: v1::ManagerErrorCode::PageTokenExpired as i32,
            message: "expired".into(),
            ..Default::default()
        })
        .into_result();
        assert!(matches!(error, Err(IssueTrackerClientError::Manager(_))));
    }

    #[test]
    fn unimplemented_rpc_requests_server_update_without_fallback() {
        let error = map_status(tonic::Status::unimplemented("ListReviewCandidates"));
        assert_eq!(
            error.to_string(),
            "issue tracker server: Issue Tracker server does not implement a required RPC; update the Issue Tracker server"
        );

        let error = map_status(tonic::Status::unimplemented(
            "CompleteExecutionToAwaitingReview",
        ));
        assert_eq!(
            error.to_string(),
            "issue tracker server: Issue Tracker server does not implement a required RPC; update the Issue Tracker server"
        );
    }

    #[test]
    fn grpc_status_diagnostics_never_include_untrusted_status_text() {
        let secret = "capability-secret-that-must-not-escape";
        let status = tonic::Status::unknown(format!("{secret} {}", "x".repeat(100_000)));

        let error = map_rpc_status(status);
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(!display.contains(secret));
        assert!(!debug.contains(secret));
        assert!(display.len() < 256);
        assert!(debug.len() < 512);

        let mut metadata = tonic::metadata::MetadataMap::new();
        metadata.insert(
            "x-capability-secret",
            secret.parse().expect("test secret is valid ASCII metadata"),
        );
        let status =
            tonic::Status::with_metadata(Code::Unknown, "upstream message is ignored", metadata);
        let error = map_awaiting_review_rpc_status(status);
        assert!(!error.to_string().contains(secret));
        assert!(!format!("{error:?}").contains(secret));
        assert!(std::error::Error::source(&error).is_none());

        let error = map_awaiting_review_rpc_status(tonic::Status::unknown(format!(
            "{secret} {}",
            "z".repeat(100_000)
        )));
        assert!(!error.to_string().contains(secret));
        assert!(!format!("{error:?}").contains(secret));
        assert!(std::error::Error::source(&error).is_none());
        let unimplemented = map_awaiting_review_rpc_status(tonic::Status::unimplemented(format!(
            "{secret} arbitrary RPC detail"
        )));
        assert_eq!(
            unimplemented.to_string(),
            "Issue Tracker server does not implement a required RPC; update the Issue Tracker server"
        );
    }

    #[test]
    fn awaiting_review_gateway_request_carries_required_cap_revision_run_and_evidence() {
        let completion = crate::grpc::proto::issue_tracker::v1::AwaitingReviewCompletion {
            run_id: "run-17".into(),
            full_commit_id: "b".repeat(40),
            evidence_references: vec!["artifact://run-17/report.json".into()],
            report: Some(crate::grpc::proto::issue_tracker::v1::CompletionReport {
                result: Some(
                    crate::grpc::proto::issue_tracker::v1::completion_report::Result::Verified(
                        crate::grpc::proto::issue_tracker::v1::VerifiedCompletion {
                            evidence: vec![crate::grpc::proto::issue_tracker::v1::CompletionEvidence {
                                kind: crate::grpc::proto::issue_tracker::v1::CompletionEvidenceKind::TestResult as i32,
                                summary: "checks passed".into(),
                                uri: None,
                            }],
                        },
                    ),
                ),
            }),
        };
        let request = build_awaiting_review_request(
            "issue-1",
            Some("process-local-capability"),
            42,
            "issue-1-run-17-awaiting-review",
            completion.clone(),
        )
        .expect("complete trusted worker request");

        assert_eq!(request.issue_id, "issue-1");
        assert_eq!(request.capability_id, "process-local-capability");
        assert_eq!(request.expected_revision, Some(42));
        assert_eq!(request.idempotency_key, "issue-1-run-17-awaiting-review");
        assert_eq!(request.state_contract_version, "awaiting_review/v1");
        assert_eq!(request.completion, Some(completion));
    }

    #[test]
    fn awaiting_review_response_maps_unconfirmed_and_rejects_unverified_success() {
        use crate::grpc::proto::issue_tracker::rpc::v1::{
            IssueGatewayAwaitingReviewMutation,
            issue_gateway_complete_execution_to_awaiting_review_response::Outcome,
        };

        let request = build_awaiting_review_request(
            "issue-1",
            Some("capability-secret-that-must-not-escape"),
            8,
            "awaiting-key",
            awaiting_review_test_completion(),
        )
        .unwrap();
        let accepted = IssueGatewayAwaitingReviewMutation {
            issue_id: "issue-1".into(),
            revision: 9,
            run_id: "run-17".into(),
            full_commit_id: "b".repeat(40),
            evidence_references: vec!["artifact://run-17/report.json".into()],
            lease_closed: true,
        };
        assert!(
            map_awaiting_review_response(Some(Outcome::Success(accepted.clone())), &request,)
                .is_ok()
        );

        let mut wrong_revision = accepted.clone();
        wrong_revision.revision += 1;
        assert!(matches!(
            map_awaiting_review_response(Some(Outcome::Success(wrong_revision)), &request),
            Err(IssueTrackerClientError::UnconfirmedOperation(_))
        ));
        for wrong_result in [
            IssueGatewayAwaitingReviewMutation {
                issue_id: "other-issue".into(),
                ..accepted.clone()
            },
            IssueGatewayAwaitingReviewMutation {
                run_id: "other-run".into(),
                ..accepted.clone()
            },
            IssueGatewayAwaitingReviewMutation {
                full_commit_id: "c".repeat(40),
                ..accepted.clone()
            },
            IssueGatewayAwaitingReviewMutation {
                evidence_references: vec!["artifact://different/report.json".into()],
                ..accepted.clone()
            },
        ] {
            assert!(matches!(
                map_awaiting_review_response(Some(Outcome::Success(wrong_result)), &request),
                Err(IssueTrackerClientError::UnconfirmedOperation(_))
            ));
        }
        let mut unverified = accepted;
        unverified.lease_closed = false;
        assert!(matches!(
            map_awaiting_review_response(Some(Outcome::Success(unverified)), &request),
            Err(IssueTrackerClientError::UnconfirmedOperation(_))
        ));
        let secret = "capability-secret-that-must-not-escape";
        let unconfirmed = map_awaiting_review_response(
            Some(Outcome::Error(
                crate::grpc::proto::issue_tracker::rpc::v1::IssueGatewayError {
                    code: "unconfirmed".into(),
                    message: format!("{secret} {}", "m".repeat(100_000)),
                    ..Default::default()
                },
            )),
            &request,
        )
        .unwrap_err();
        assert!(!unconfirmed.to_string().contains(secret));
        assert!(!format!("{unconfirmed:?}").contains(secret));
        assert!(unconfirmed.to_string().len() < 256);
        let gateway_error = map_awaiting_review_response(
            Some(Outcome::Error(
                crate::grpc::proto::issue_tracker::rpc::v1::IssueGatewayError {
                    code: format!("revision_conflict-{secret}"),
                    message: format!("{secret} {}", "g".repeat(100_000)),
                    ..Default::default()
                },
            )),
            &request,
        );
        let gateway_error = gateway_error.unwrap_err();
        assert!(!gateway_error.to_string().contains(secret));
        assert!(!format!("{gateway_error:?}").contains(secret));
        assert!(gateway_error.to_string().len() < 256);
        assert!(std::error::Error::source(&gateway_error).is_none());
        assert!(matches!(
            map_awaiting_review_response(None, &request),
            Err(IssueTrackerClientError::UnconfirmedOperation(_))
        ));
    }

    #[test]
    fn awaiting_review_request_requires_write_identity_and_allows_capability_free_replay() {
        let completion = awaiting_review_test_completion();
        assert!(
            build_awaiting_review_request("issue-1", Some(""), 1, "key", completion.clone())
                .is_err()
        );
        assert!(
            build_awaiting_review_request("issue-1", Some("cap"), 1, "", completion.clone())
                .is_err()
        );
        assert!(
            build_awaiting_review_request(
                "issue-1",
                Some("cap"),
                1,
                "key",
                crate::grpc::proto::issue_tracker::v1::AwaitingReviewCompletion::default()
            )
            .is_err()
        );

        let replay = build_awaiting_review_request(
            "issue-1",
            None,
            0,
            "same-original-idempotency-key",
            completion,
        )
        .expect("identical replay may omit the consumed process-local capability");
        assert!(replay.capability_id.is_empty());
        assert_eq!(replay.expected_revision, Some(0));
        assert!(
            build_awaiting_review_request(
                "issue-1",
                None,
                u64::MAX,
                "key",
                awaiting_review_test_completion(),
            )
            .is_err()
        );
    }

    #[test]
    fn awaiting_review_request_enforces_exact_utf8_byte_limits_without_rewriting_values() {
        let completion = awaiting_review_test_completion();
        let make = |issue_id: &str, capability: Option<&str>, key: &str, completion| {
            build_awaiting_review_request(issue_id, capability, 1, key, completion)
        };

        assert!(make("legacy:issue-7", Some("cap"), "key", completion.clone()).is_ok());
        assert!(make(&"i".repeat(4_096), Some("c"), "key", completion.clone()).is_ok());
        assert!(make(&"i".repeat(4_097), Some("c"), "key", completion.clone()).is_err());
        assert!(make("issue-1", Some(&"c".repeat(256)), "key", completion.clone()).is_ok());
        assert!(make("issue-1", Some(&"c".repeat(257)), "key", completion.clone()).is_err());
        assert!(
            make(
                "issue-1",
                Some("cap"),
                &"k".repeat(4_096),
                completion.clone()
            )
            .is_ok()
        );
        assert!(
            make(
                "issue-1",
                Some("cap"),
                &"k".repeat(4_097),
                completion.clone()
            )
            .is_err()
        );
        assert!(
            make(
                "issue-1",
                Some("cap"),
                &"é".repeat(2_048),
                completion.clone()
            )
            .is_ok()
        );
        assert!(
            make(
                "issue-1",
                Some("cap"),
                &"é".repeat(2_049),
                completion.clone()
            )
            .is_err()
        );

        let mut exact_run_id = completion.clone();
        exact_run_id.run_id = "r".repeat(256);
        assert!(make("issue-1", Some("cap"), "key", exact_run_id).is_ok());
        let mut oversized_run_id = completion.clone();
        oversized_run_id.run_id = "r".repeat(257);
        assert!(make("issue-1", Some("cap"), "key", oversized_run_id).is_err());

        let mut exact_reference = completion.clone();
        exact_reference.evidence_references = vec!["é".repeat(1_024)];
        let exact_request = make("issue-1", Some("cap"), "key", exact_reference.clone())
            .expect("2048-byte reference is accepted");
        assert_eq!(
            exact_request
                .completion
                .as_ref()
                .unwrap()
                .evidence_references[0],
            exact_reference.evidence_references[0]
        );
        let mut oversized_reference = completion;
        oversized_reference.evidence_references = vec!["é".repeat(1_025)];
        assert!(make("issue-1", Some("cap"), "key", oversized_reference).is_err());

        let secret_capability = "capability-secret-that-must-not-escape".repeat(10);
        let error = make(
            "issue-1",
            Some(&secret_capability),
            "key",
            awaiting_review_test_completion(),
        )
        .unwrap_err();
        assert!(!error.to_string().contains(&secret_capability));
        assert!(!format!("{error:?}").contains(&secret_capability));
    }

    #[test]
    fn awaiting_review_report_fields_and_nested_evidence_counts_are_bounded() {
        use crate::grpc::proto::issue_tracker::v1::{
            CompletionEvidence, CompletionEvidenceKind, CompletionReport,
            completion_report::Result as CompletionResult,
        };

        let build = |report| {
            let mut completion = awaiting_review_test_completion();
            completion.report = Some(report);
            build_awaiting_review_request("issue-1", Some("cap"), 1, "key", completion)
        };
        let unverified = |reason: String, residual_risk: String| CompletionReport {
            result: Some(CompletionResult::Unverified(
                crate::grpc::proto::issue_tracker::v1::UnverifiedCompletion {
                    reason,
                    residual_risk,
                },
            )),
        };
        let verified = |evidence| CompletionReport {
            result: Some(CompletionResult::Verified(
                crate::grpc::proto::issue_tracker::v1::VerifiedCompletion { evidence },
            )),
        };

        assert!(build(unverified("r".repeat(4_096), "risk".into())).is_ok());
        assert!(build(unverified("r".repeat(4_097), "risk".into())).is_err());
        assert!(build(unverified("reason".into(), "x".repeat(4_096))).is_ok());
        assert!(build(unverified("reason".into(), "x".repeat(4_097))).is_err());
        assert!(build(unverified("é".repeat(2_048), "risk".into())).is_ok());
        assert!(build(unverified("é".repeat(2_049), "risk".into())).is_err());
        assert!(build(unverified("reason\nsecret".into(), "risk".into())).is_err());
        assert!(build(unverified("reason".into(), " \t ".into())).is_err());

        let one_evidence = |summary: String, uri: Option<String>| CompletionEvidence {
            kind: CompletionEvidenceKind::TestResult as i32,
            summary,
            uri,
        };
        assert!(build(verified(vec![one_evidence("s".repeat(4_096), None)])).is_ok());
        assert!(build(verified(vec![one_evidence("s".repeat(4_097), None)])).is_err());
        let opaque_uri = "opaque+scheme:artifact/path?query#fragment";
        let request = build(verified(vec![one_evidence(
            "checks passed".into(),
            Some(opaque_uri.into()),
        )]))
        .expect("server-accepted opaque URI syntax remains opaque");
        assert_eq!(
            request.completion.unwrap().report.unwrap().result,
            Some(CompletionResult::Verified(
                crate::grpc::proto::issue_tracker::v1::VerifiedCompletion {
                    evidence: vec![one_evidence(
                        "checks passed".into(),
                        Some(opaque_uri.into())
                    )],
                }
            ))
        );
        assert!(
            build(verified(vec![one_evidence(
                "checks passed".into(),
                Some("u".repeat(4_096)),
            )]))
            .is_ok()
        );
        assert!(
            build(verified(vec![one_evidence(
                "checks passed".into(),
                Some("u".repeat(4_097)),
            )]))
            .is_err()
        );
        assert!(build(verified(vec![one_evidence("summary\nsecret".into(), None)])).is_err());
        assert!(
            build(verified(vec![one_evidence(
                "summary".into(),
                Some("uri\u{007f}".into())
            )]))
            .is_err()
        );
        assert!(build(verified(Vec::new())).is_err());
        assert!(
            build(verified(vec![CompletionEvidence {
                kind: 0,
                summary: "summary".into(),
                uri: None,
            }]))
            .is_err()
        );

        let max_evidence = vec![one_evidence("ok".into(), None); 1_000];
        assert!(build(verified(max_evidence)).is_ok());
        let over_evidence = vec![one_evidence("ok".into(), None); 1_001];
        assert!(build(verified(over_evidence)).is_err());
    }

    #[test]
    fn awaiting_review_gateway_code_whitelist_discards_unknown_values() {
        for (code, expected) in [
            ("unconfirmed", AwaitingReviewFailureKind::Unconfirmed),
            (
                "invalid_argument",
                AwaitingReviewFailureKind::InvalidArgument,
            ),
            ("forbidden", AwaitingReviewFailureKind::Forbidden),
            (
                "precondition_failed",
                AwaitingReviewFailureKind::PreconditionFailed,
            ),
            (
                "revision_conflict",
                AwaitingReviewFailureKind::RevisionConflict,
            ),
            (
                "idempotency_conflict",
                AwaitingReviewFailureKind::IdempotencyConflict,
            ),
            ("not_found", AwaitingReviewFailureKind::NotFound),
            ("unavailable", AwaitingReviewFailureKind::Unavailable),
            (
                "attacker-controlled-code",
                AwaitingReviewFailureKind::UnknownGatewayError,
            ),
        ] {
            assert_eq!(AwaitingReviewFailureKind::from_gateway_code(code), expected);
        }
    }

    #[test]
    fn awaiting_review_request_enforces_outer_evidence_count_and_encoded_size_boundary() {
        use crate::grpc::proto::issue_tracker::v1::{
            CompletionEvidence, CompletionEvidenceKind, CompletionReport,
            completion_report::Result as CompletionResult,
        };

        let with_references = |count: usize| {
            let mut completion = awaiting_review_test_completion();
            completion.evidence_references = vec!["ref".into(); count];
            completion
        };
        assert!(build_awaiting_review_request(
            "issue-1",
            Some("cap"),
            1,
            "key",
            with_references(1_000),
        )
        .is_ok());
        assert!(build_awaiting_review_request(
            "issue-1",
            Some("cap"),
            1,
            "key",
            with_references(1_001),
        )
        .is_err());

        let mut completion = with_references(1_000);
        completion.evidence_references = vec!["r".repeat(2_048); 1_000];
        completion.report = Some(CompletionReport {
            result: Some(CompletionResult::Verified(
                crate::grpc::proto::issue_tracker::v1::VerifiedCompletion {
                    evidence: vec![
                        CompletionEvidence {
                            kind: CompletionEvidenceKind::TestResult as i32,
                            summary: "s".repeat(2_048),
                            uri: None,
                        };
                        1_000
                    ],
                },
            )),
        });
        let mut exact_candidate =
            build_awaiting_review_request("issue-1", Some("cap"), 1, "key", completion)
                .expect("base request is below the encoded-size limit");
        let base_len = exact_candidate.encoded_len();
        let max_encoded_len = 4 * 1024 * 1024;
        assert!(base_len < max_encoded_len, "base len was {base_len}");
        let increase = max_encoded_len - base_len;
        let quotient = increase / 1_000;
        let remainder = increase % 1_000;
        let Some(CompletionResult::Verified(report)) = exact_candidate
            .completion
            .as_mut()
            .and_then(|completion| completion.report.as_mut())
            .and_then(|report| report.result.as_mut())
        else {
            unreachable!();
        };
        for (index, evidence) in report.evidence.iter_mut().enumerate() {
            let extra = quotient + usize::from(index < remainder);
            evidence.summary.push_str(&"x".repeat(extra));
        }
        let mut candidate_len = exact_candidate.encoded_len();
        if candidate_len > max_encoded_len {
            let reduction = candidate_len - max_encoded_len;
            let Some(CompletionResult::Verified(report)) = exact_candidate
                .completion
                .as_mut()
                .and_then(|completion| completion.report.as_mut())
                .and_then(|report| report.result.as_mut())
            else {
                unreachable!();
            };
            let summary = &mut report.evidence[0].summary;
            summary.truncate(summary.len() - reduction);
            candidate_len = exact_candidate.encoded_len();
        }
        assert_eq!(candidate_len, max_encoded_len);
        let exact_completion = exact_candidate.completion.clone().unwrap();
        let exact = build_awaiting_review_request(
            "issue-1",
            Some("cap"),
            1,
            "key",
            exact_completion.clone(),
        )
        .expect("request exactly at the protobuf size limit is accepted");
        assert_eq!(exact.encoded_len(), max_encoded_len);

        let Some(CompletionResult::Verified(report)) = exact_candidate
            .completion
            .as_mut()
            .and_then(|completion| completion.report.as_mut())
            .and_then(|report| report.result.as_mut())
        else {
            unreachable!();
        };
        report.evidence[0].summary.push('x');
        let oversized_completion = exact_candidate.completion.unwrap();
        assert!(
            build_awaiting_review_request("issue-1", Some("cap"), 1, "key", oversized_completion,)
                .is_err()
        );
    }

    fn awaiting_review_test_completion()
    -> crate::grpc::proto::issue_tracker::v1::AwaitingReviewCompletion {
        crate::grpc::proto::issue_tracker::v1::AwaitingReviewCompletion {
            run_id: "run-17".into(),
            full_commit_id: "b".repeat(40),
            evidence_references: vec!["artifact://run-17/report.json".into()],
            report: Some(crate::grpc::proto::issue_tracker::v1::CompletionReport {
                result: Some(
                    crate::grpc::proto::issue_tracker::v1::completion_report::Result::Unverified(
                        crate::grpc::proto::issue_tracker::v1::UnverifiedCompletion {
                            reason: "execution finished".into(),
                            residual_risk: "human review remains".into(),
                        },
                    ),
                ),
            }),
        }
    }

    #[test]
    fn config_is_written_below_the_private_root() {
        let root = tempdir().unwrap();
        let data = DataPaths::with_root(root.path());
        let config = IssueTrackerServerConfig::new(&data, 50052, 50053, 50054).unwrap();
        assert!(
            config
                .config_path()
                .starts_with(data.issue_tracker_v3_dir())
        );
        assert_eq!(config.primary_url(), "http://127.0.0.1:50052");
        assert_eq!(config.gateway_control_url(), "http://127.0.0.1:50053");
        assert_eq!(config.gateway_worker_url(), "http://127.0.0.1:50054");
    }

    #[test]
    fn toml_basic_string_round_trips_special_characters() {
        let original =
            "C:\\Users\\x \"quoted\"\nwith space\u{0008}\t\u{000C}\r\u{0000}\u{001F}\u{007F}";
        let document = format!("data_root = {}", toml_basic_string(original));
        let parsed: toml::Value = toml::from_str(&document).unwrap();

        assert_eq!(parsed["data_root"].as_str(), Some(original));
    }
}
