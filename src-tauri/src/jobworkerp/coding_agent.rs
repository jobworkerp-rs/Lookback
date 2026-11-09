//! Narrow jobworkerp transport for the sandbox-backed coding agent.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures::{StreamExt, stream};
use jobworkerp_client::{
    client::helper::UseJobworkerpClientHelper,
    command::job::JOB_ID_HEADER_NAME,
    jobworkerp::{
        data::{
            FeedDataTransport, JobId, MethodSchema, QueueType, ResponseType, ResultOutputItem,
            RetryPolicy, RetryType, Runner, RunnerData, RunnerId, RunnerType, StreamErrorCode,
            StreamingOutputType, Trailer, Worker, WorkerData, WorkerId, result_output_item::Item,
        },
        service::{
            ClientStreamRequest, JobRequest, RunnerNameRequest, WorkerNameRequest,
            client_stream_request, job_request,
        },
    },
    proto::JobworkerpProto,
    stream_error::stream_error_from_trailer,
};
use prost::Message;
use prost_reflect::{DynamicMessage, MessageDescriptor};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Streaming};

use crate::{
    error::{AppError, AppResult},
    grpc::proto::jobworkerp::runner::sandbox::{
        SandboxExecResult, sandbox_exec_result::Result as SandboxResult,
    },
    jobworkerp::JobworkerpHandle,
};

const SANDBOX_RUNNER_NAME: &str = "SANDBOX";
const SANDBOX_CLIENT_STREAM_METHOD: &str = "run_with_client";
const SANDBOX_ONESHOT_METHOD: &str = "run";
/// `StreamError.origin` of failures jobworkerp detects outside the runner.
const WORKER_STREAM_ORIGIN: &str = "WORKER";
const NATIVE_SANDBOX_RUNNER_NAME: &str = "SANDBOX";
const NATIVE_SANDBOX_METHOD: &str = "run_with_client";
pub(crate) const NATIVE_INTERACTIVE_CHANNEL: &str = "native-interactive";
pub(crate) const CODING_AGENT_ONE_SHOT_CHANNEL: &str = "coding-agent-one-shot";

#[derive(Clone, Copy)]
enum NativeWorkerRoute {
    Interactive,
    OneShot,
}

impl NativeWorkerRoute {
    fn for_method(method: &MethodSchema) -> Self {
        if method.require_client_stream {
            Self::Interactive
        } else {
            Self::OneShot
        }
    }
    fn prefix(self) -> &'static str {
        match self {
            Self::Interactive => "lookback.interactive.",
            Self::OneShot => "lookback.coding-agent.one-shot.",
        }
    }
    fn description(self) -> &'static str {
        match self {
            Self::Interactive => "Lookback native interactive coding-agent worker",
            Self::OneShot => "Lookback automatic coding-agent worker",
        }
    }
    fn channel(self) -> &'static str {
        match self {
            Self::Interactive => NATIVE_INTERACTIVE_CHANNEL,
            Self::OneShot => CODING_AGENT_ONE_SHOT_CHANNEL,
        }
    }
    fn method(self) -> &'static str {
        match self {
            Self::Interactive => NATIVE_SANDBOX_METHOD,
            Self::OneShot => SANDBOX_ONESHOT_METHOD,
        }
    }
}

const FIXED_SANDBOX_RESULT_PROTO: &str =
    include_str!("../../../proto/jobworkerp/runner/sandbox_result.proto");
const NATIVE_RPC_TIMEOUT: Duration = Duration::from_secs(10);
const NATIVE_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const NATIVE_VERSION_JOB_TIMEOUT_MS: u64 = 60_000;
const MAX_NATIVE_JOB_ARGS_BYTES: usize = 64 * 1024;
const MAX_NATIVE_JOB_ARG_COUNT: usize = 1024;

/// Host-side paths used by the local Microsandbox backend.
///
/// These paths are deliberately not serialized into WorkerData. Supplying them
/// records which sidecar context this native worker is bound to; it is not a
/// proof that the SDK, backend, VM, or agent runtime is ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostWorkerContext {
    pub microsandbox_sdk_home: PathBuf,
    pub backend_config_path: PathBuf,
}

/// Immutable inputs for reconciling the app-owned native interactive worker.
///
/// `runner_settings_json` must already be the profile/repository-scoped,
/// session-independent SANDBOX settings assembled by the host manager. In
/// particular, per-session working directories and prompts are job arguments,
/// never part of this worker definition. `use_static` is the explicit profile
/// policy and participates in the worker's stable identity; it is not defaulted.
#[derive(Debug, Clone)]
pub struct NativeInteractiveWorkerSpec {
    pub native_profile_id: String,
    pub repository_id: coding_agent_executor::RepositoryId,
    /// Profile-selected Worker static-pool policy; never inferred or defaulted.
    pub use_static: bool,
    pub workspace_binding: NativeWorkspaceBinding,
    pub runner_settings_json: Value,
    pub host_context: HostWorkerContext,
}

/// Native profile setup inputs, independent of an Issue or repository.
#[derive(Debug, Clone)]
pub struct NativeProfileSettingsWorkerSpec {
    pub native_profile_id: String,
    pub use_static: bool,
    pub workspace_binding: NativeWorkspaceBinding,
    pub runner_settings_json: Value,
    pub host_context: HostWorkerContext,
}

#[derive(Debug, Clone)]
pub struct NativeProfileSettingsWorkerMatch(NativeWorkerMatch);

impl NativeProfileSettingsWorkerMatch {
    pub fn worker_id(&self) -> &WorkerId {
        self.0.worker_id()
    }
    pub fn settings_digest(&self) -> &str {
        self.0.settings_digest()
    }
}

struct NativeWorkerRegistrationSpec {
    native_profile_id: String,
    repository_id: Option<coding_agent_executor::RepositoryId>,
    use_static: bool,
    workspace_binding: NativeWorkspaceBinding,
    runner_settings_json: Value,
    host_context: HostWorkerContext,
}

impl From<NativeInteractiveWorkerSpec> for NativeWorkerRegistrationSpec {
    fn from(spec: NativeInteractiveWorkerSpec) -> Self {
        Self {
            native_profile_id: spec.native_profile_id,
            repository_id: Some(spec.repository_id),
            use_static: spec.use_static,
            workspace_binding: spec.workspace_binding,
            runner_settings_json: spec.runner_settings_json,
            host_context: spec.host_context,
        }
    }
}

impl From<NativeProfileSettingsWorkerSpec> for NativeWorkerRegistrationSpec {
    fn from(spec: NativeProfileSettingsWorkerSpec) -> Self {
        Self {
            native_profile_id: spec.native_profile_id,
            repository_id: None,
            use_static: spec.use_static,
            workspace_binding: spec.workspace_binding,
            runner_settings_json: spec.runner_settings_json,
            host_context: spec.host_context,
        }
    }
}

/// Stable host-to-guest workspace mount shared by native sessions for one
/// profile/repository worker. Session working directories remain job arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeWorkspaceBinding {
    pub canonical_host_root: PathBuf,
    pub guest_root: String,
}

/// A verified app-owned worker identity that host code can pass to the native
/// terminal transport without receiving an untyped worker-name string.
/// Verification describes the persisted Worker and runner configuration only;
/// it does not prove that the Microsandbox SDK or VM is ready to execute.
#[derive(Debug, Clone)]
pub struct NativeWorkerMatch {
    worker_id: WorkerId,
    worker_name: String,
    runner_id: RunnerId,
    settings_digest: String,
    method_schema_digest: String,
    method_schema: MethodSchema,
    settings_descriptor: MessageDescriptor,
    normalized_runner_settings: Value,
    guest_root: String,
    host_context: HostWorkerContext,
    scope: NativeWorkerScope,
}

/// Scope extracted from the validated immutable native-worker configuration.
/// This binds a host-prepared context to the worker that actually owns its VM settings.
#[derive(Debug, Clone)]
pub struct NativeWorkerScope {
    native_profile_id: String,
    repository_id: Option<coding_agent_executor::RepositoryId>,
    canonical_host_root: PathBuf,
    guest_root: String,
    image_digest: String,
    cpu_millis: u32,
    memory_mib: u64,
    root_disk_mib: u64,
    use_static: bool,
}

impl NativeWorkerScope {
    pub fn native_profile_id(&self) -> &str {
        &self.native_profile_id
    }

    pub fn repository_id(&self) -> Option<&coding_agent_executor::RepositoryId> {
        self.repository_id.as_ref()
    }

    pub fn canonical_host_root(&self) -> &Path {
        &self.canonical_host_root
    }

    pub fn guest_root(&self) -> &str {
        &self.guest_root
    }

    pub fn image_digest(&self) -> &str {
        &self.image_digest
    }

    pub fn cpu_millis(&self) -> u32 {
        self.cpu_millis
    }

    pub fn memory_mib(&self) -> u64 {
        self.memory_mib
    }

    pub fn root_disk_mib(&self) -> u64 {
        self.root_disk_mib
    }

    pub fn use_static(&self) -> bool {
        self.use_static
    }
}

impl NativeWorkerMatch {
    pub fn worker_name(&self) -> &str {
        &self.worker_name
    }

    pub fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    pub fn runner_id(&self) -> &RunnerId {
        &self.runner_id
    }

    pub fn settings_digest(&self) -> &str {
        &self.settings_digest
    }

    pub fn method_schema_digest(&self) -> &str {
        // This is the full live SANDBOX method-map contract, not just the
        // selected `run_with_client` entry; that binds one-shot schemas too.
        &self.method_schema_digest
    }

    pub fn method_schema(&self) -> &MethodSchema {
        &self.method_schema
    }

    pub fn settings_descriptor(&self) -> &MessageDescriptor {
        &self.settings_descriptor
    }

    pub fn using(&self) -> &'static str {
        NativeWorkerRoute::for_method(&self.method_schema).method()
    }

    pub fn host_context(&self) -> &HostWorkerContext {
        &self.host_context
    }

    pub fn scope(&self) -> &NativeWorkerScope {
        &self.scope
    }
}

/// Native-only bounds for the SANDBOX CLI `--version` probe. Probe calls
/// are deliberately capped at the server-side 60-second job limit.
#[derive(Debug, Clone, Copy)]
pub struct NativeVersionProbeOptions {
    pub timeout: Duration,
}

impl Default for NativeVersionProbeOptions {
    fn default() -> Self {
        Self {
            timeout: NATIVE_VERSION_PROBE_TIMEOUT,
        }
    }
}

#[derive(Serialize)]
struct MethodSchemaFingerprint<'a> {
    args_proto: &'a str,
    result_proto: &'a str,
    description: &'a Option<String>,
    output_type: i32,
    require_client_stream: bool,
    client_stream_data_proto: &'a Option<String>,
}

#[derive(Serialize)]
struct NativeWorkerFingerprint<'a> {
    version: u8,
    native_profile_id: &'a str,
    repository_id: &'a Option<coding_agent_executor::RepositoryId>,
    workspace_host_root: &'a str,
    workspace_guest_root: &'a str,
    sdk_home: &'a str,
    backend_config_path: &'a str,
    runner_id: i64,
    runner_type: i32,
    runner_name: &'a str,
    runner_definition: &'a str,
    runner_settings_proto: &'a str,
    method_schemas: BTreeMap<String, MethodSchemaFingerprint<'a>>,
    normalized_runner_settings: &'a Value,
    description: &'static str,
    retry_type: i32,
    retry_interval: u32,
    retry_max_interval: u32,
    retry_max_count: u32,
    retry_basis: f32,
    periodic_interval: u32,
    channel: &'static str,
    queue_type: i32,
    response_type: i32,
    store_success: bool,
    store_failure: bool,
    use_static: bool,
    broadcast_results: bool,
}

struct NativeWorkerMatchParts {
    worker_id: WorkerId,
    worker_name: String,
    runner_id: RunnerId,
    settings_digest: String,
    method_schema_digest: String,
    method_schema: MethodSchema,
    settings_descriptor: MessageDescriptor,
    normalized_runner_settings: Value,
    guest_root: String,
    host_context: HostWorkerContext,
    scope: NativeWorkerScope,
}

struct NativeWorkerDigestInput<'a> {
    route: NativeWorkerRoute,
    spec: &'a NativeWorkerRegistrationSpec,
    sdk_home: &'a str,
    backend_config_path: &'a str,
    workspace_host_root: &'a str,
    workspace_guest_root: &'a str,
    runner_id: &'a RunnerId,
    runner: &'a RunnerData,
    method: &'a MethodSchema,
    method_schema_digest: &'a str,
    normalized_runner_settings: &'a Value,
}

impl JobworkerpHandle {
    /// Find or create the dedicated worker for an immutable profile,
    /// repository, and runner-settings snapshot. Session-specific prompts and
    /// working directories do not participate in the worker identity.
    pub async fn ensure_native_interactive_worker(
        &self,
        spec: NativeInteractiveWorkerSpec,
    ) -> AppResult<NativeWorkerMatch> {
        self.ensure_native_worker(spec.into(), NativeWorkerRoute::Interactive)
            .await
    }

    pub async fn ensure_native_one_shot_worker(
        &self,
        spec: NativeInteractiveWorkerSpec,
    ) -> AppResult<NativeWorkerMatch> {
        self.ensure_native_worker(spec.into(), NativeWorkerRoute::OneShot)
            .await
    }

    pub async fn ensure_native_profile_settings_worker(
        &self,
        spec: NativeProfileSettingsWorkerSpec,
    ) -> AppResult<NativeProfileSettingsWorkerMatch> {
        self.ensure_native_worker(spec.into(), NativeWorkerRoute::Interactive)
            .await
            .map(NativeProfileSettingsWorkerMatch)
    }

    pub async fn start_native_profile_settings_terminal(
        &self,
        matched: &NativeProfileSettingsWorkerMatch,
    ) -> AppResult<CodingAgentStream> {
        self.start_native_interactive_session(
            &matched.0,
            "/bin/sh",
            &["-c".into(), PROFILE_SETTINGS_SHELL_SCRIPT.into()],
            &matched.0.guest_root,
        )
        .await
    }

    pub(crate) async fn start_native_profile_settings_terminal_with_dispatch(
        &self,
        matched: &NativeProfileSettingsWorkerMatch,
        before_dispatch: impl FnOnce() + Send,
    ) -> AppResult<CodingAgentStream> {
        self.start_native_interactive_session_before_dispatch(
            &matched.0,
            "/bin/sh",
            &["-c".into(), PROFILE_SETTINGS_SHELL_SCRIPT.into()],
            &matched.0.guest_root,
            before_dispatch,
        )
        .await
    }

    async fn ensure_native_worker(
        &self,
        spec: NativeWorkerRegistrationSpec,
        route: NativeWorkerRoute,
    ) -> AppResult<NativeWorkerMatch> {
        let (sdk_home, backend_config_path) = validate_native_spec(&spec)?;
        let (runner_id, runner_data) = self.find_native_sandbox_runner().await?;
        let (settings_descriptor, method_schema, method_schema_digest) =
            validate_native_runner_schema_for_route(&runner_data, route)?;
        let (runner_settings, normalized_runner_settings) =
            encode_native_settings(&settings_descriptor, &spec.runner_settings_json)?;
        if matches!(route, NativeWorkerRoute::Interactive) {
            validate_native_interactive_deadlines(&normalized_runner_settings)?;
        }
        let (workspace_host_root, workspace_guest_root) =
            validate_workspace_binding(&spec.workspace_binding, &normalized_runner_settings)?;
        let scope = native_worker_scope(
            &spec,
            &workspace_host_root,
            &workspace_guest_root,
            &normalized_runner_settings,
        )?;
        let digest = native_worker_digest(NativeWorkerDigestInput {
            route,
            spec: &spec,
            sdk_home: &sdk_home,
            backend_config_path: &backend_config_path,
            workspace_host_root: &workspace_host_root,
            workspace_guest_root: &workspace_guest_root,
            runner_id: &runner_id,
            runner: &runner_data,
            method: &method_schema,
            method_schema_digest: &method_schema_digest,
            normalized_runner_settings: &normalized_runner_settings,
        })?;
        let worker_name = format!("{}{}", route.prefix(), &digest[..32]);
        let expected = native_worker_data(
            worker_name,
            runner_id,
            runner_settings,
            spec.use_static,
            route,
        );

        if let Some((worker_id, worker_data)) = self.find_native_worker(&expected.name).await? {
            verify_native_worker(
                &worker_id,
                &worker_data,
                &expected,
                &settings_descriptor,
                &normalized_runner_settings,
            )?;
            return Ok(native_worker_match(NativeWorkerMatchParts {
                worker_id,
                worker_name: expected.name.clone(),
                runner_id,
                settings_digest: digest,
                method_schema_digest,
                method_schema,
                settings_descriptor,
                normalized_runner_settings,
                guest_root: workspace_guest_root,
                host_context: spec.host_context,
                scope,
            }));
        }

        let mut client = self
            .native_interactive_client()
            .await?
            .jobworkerp_client
            .worker_client()
            .await;
        let create_result =
            tokio::time::timeout(NATIVE_RPC_TIMEOUT, client.create(expected.clone()))
                .await
                .map_err(|_| native_worker_error("worker Create timed out"))?;
        let created_id = match create_result {
            Ok(response) => response.into_inner().id,
            Err(status) if status.code() == tonic::Code::AlreadyExists => {
                let Some((worker_id, worker_data)) =
                    self.find_native_worker(&expected.name).await?
                else {
                    return Err(native_worker_error(
                        "creation raced with another writer but no worker was found",
                    ));
                };
                verify_native_worker(
                    &worker_id,
                    &worker_data,
                    &expected,
                    &settings_descriptor,
                    &normalized_runner_settings,
                )?;
                return Ok(native_worker_match(NativeWorkerMatchParts {
                    worker_id,
                    worker_name: expected.name.clone(),
                    runner_id,
                    settings_digest: digest,
                    method_schema_digest,
                    method_schema,
                    settings_descriptor,
                    normalized_runner_settings,
                    guest_root: workspace_guest_root,
                    host_context: spec.host_context,
                    scope,
                }));
            }
            Err(status) => {
                return Err(native_worker_error(format!(
                    "Create RPC failed ({:?})",
                    status.code()
                )));
            }
        }
        .ok_or_else(|| native_worker_error("Create RPC returned no worker id"))?;

        let (worker_id, worker_data) = self
            .find_native_worker(&expected.name)
            .await?
            .ok_or_else(|| native_worker_error("created worker was not visible by name"))?;
        if worker_id != created_id {
            return Err(native_worker_error(
                "Create and FindByName returned different worker ids",
            ));
        }
        verify_native_worker(
            &worker_id,
            &worker_data,
            &expected,
            &settings_descriptor,
            &normalized_runner_settings,
        )?;

        Ok(native_worker_match(NativeWorkerMatchParts {
            worker_id,
            worker_name: expected.name.clone(),
            runner_id,
            settings_digest: digest,
            method_schema_digest,
            method_schema,
            settings_descriptor,
            normalized_runner_settings,
            guest_root: workspace_guest_root,
            host_context: spec.host_context,
            scope,
        }))
    }

    async fn find_native_sandbox_runner(&self) -> AppResult<(RunnerId, RunnerData)> {
        let mut client = self
            .native_interactive_client()
            .await?
            .jobworkerp_client
            .runner_client()
            .await;
        let runner = tokio::time::timeout(
            NATIVE_RPC_TIMEOUT,
            client.find_by_name(RunnerNameRequest {
                name: NATIVE_SANDBOX_RUNNER_NAME.to_owned(),
            }),
        )
        .await
        .map_err(|_| native_worker_error("SANDBOX runner lookup timed out"))?
        .map_err(|status| {
            native_worker_error(format!(
                "SANDBOX runner lookup failed ({:?})",
                status.code()
            ))
        })?
        .into_inner()
        .data
        .ok_or_else(|| native_worker_error("SANDBOX runner was not found"))?;
        let runner_id = runner
            .id
            .ok_or_else(|| native_worker_error("SANDBOX runner had no id"))?;
        let runner_data = runner
            .data
            .ok_or_else(|| native_worker_error("SANDBOX runner had no data"))?;
        if runner_id.value <= 0
            || runner_data.name != NATIVE_SANDBOX_RUNNER_NAME
            || runner_data.runner_type != RunnerType::Sandbox as i32
        {
            return Err(native_worker_error(
                "runner lookup returned a non-SANDBOX runner",
            ));
        }
        Ok((runner_id, runner_data))
    }

    async fn find_native_worker(&self, name: &str) -> AppResult<Option<(WorkerId, WorkerData)>> {
        let mut client = self
            .native_interactive_client()
            .await?
            .jobworkerp_client
            .worker_client()
            .await;
        let worker = tokio::time::timeout(
            NATIVE_RPC_TIMEOUT,
            client.find_by_name(WorkerNameRequest {
                name: name.to_owned(),
            }),
        )
        .await
        .map_err(|_| native_worker_error("worker lookup timed out"))?
        .map_err(|status| {
            native_worker_error(format!("worker lookup failed ({:?})", status.code()))
        })?
        .into_inner()
        .data;
        match worker {
            None => Ok(None),
            Some(Worker {
                id: Some(worker_id),
                data: Some(worker_data),
            }) => Ok(Some((worker_id, worker_data))),
            Some(_) => Err(native_worker_error(
                "FindByName returned incomplete worker data",
            )),
        }
    }

    async fn find_native_runner(&self, runner_id: &RunnerId) -> AppResult<Runner> {
        let mut client = self
            .native_interactive_client()
            .await?
            .jobworkerp_client
            .runner_client()
            .await;
        tokio::time::timeout(NATIVE_RPC_TIMEOUT, client.find(*runner_id))
            .await
            .map_err(|_| native_worker_error("SANDBOX runner lookup timed out"))?
            .map_err(|status| {
                native_worker_error(format!(
                    "SANDBOX runner lookup failed ({:?})",
                    status.code()
                ))
            })?
            .into_inner()
            .data
            .ok_or_else(|| native_worker_error("SANDBOX runner was not found"))
    }

    async fn verify_native_worker_match(
        &self,
        matched: &NativeWorkerMatch,
    ) -> AppResult<(RunnerData, WorkerData)> {
        let Some((worker_id, worker_data)) = self
            .find_native_worker(&matched.worker_name)
            .await
            .map_err(|_| transport_error("matched worker lookup failed"))?
        else {
            return Err(transport_error("matched worker is no longer registered"));
        };
        if worker_id != matched.worker_id {
            return Err(transport_error(
                "matched worker name now resolves to a different worker id",
            ));
        }
        let expected_worker = native_worker_data(
            matched.worker_name.clone(),
            matched.runner_id,
            Vec::new(),
            matched.scope.use_static(),
            NativeWorkerRoute::for_method(&matched.method_schema),
        );
        verify_native_worker(
            &worker_id,
            &worker_data,
            &expected_worker,
            &matched.settings_descriptor,
            &matched.normalized_runner_settings,
        )
        .map_err(|_| transport_error("matched worker configuration has drifted"))?;

        let runner = self
            .find_native_runner(&matched.runner_id)
            .await
            .map_err(|_| transport_error("matched SANDBOX runner lookup failed"))?;
        if runner.id.as_ref() != Some(&matched.runner_id) {
            return Err(transport_error("matched SANDBOX runner id has drifted"));
        }
        let runner_data = runner
            .data
            .ok_or_else(|| transport_error("matched SANDBOX runner is no longer registered"))?;
        if runner_data.name != NATIVE_SANDBOX_RUNNER_NAME
            || runner_data.runner_type != RunnerType::Sandbox as i32
        {
            return Err(transport_error(
                "matched SANDBOX runner identity has drifted",
            ));
        }
        let (_, method_schema, schema_digest) = validate_native_runner_schema_for_route(
            &runner_data,
            NativeWorkerRoute::for_method(&matched.method_schema),
        )
        .map_err(|_| transport_error("matched SANDBOX method schema is invalid"))?;
        if schema_digest != matched.method_schema_digest || method_schema != matched.method_schema {
            return Err(transport_error("matched SANDBOX method schema has drifted"));
        }
        Ok((runner_data, worker_data))
    }

    /// Re-read the matched native Worker and SANDBOX `run` contract without
    /// creating a job. Doctor lease validation uses the returned live schema
    /// digest to reject a stale observation before native startup can claim it.
    pub(crate) async fn revalidate_native_version_probe_match(
        &self,
        matched: &NativeWorkerMatch,
    ) -> AppResult<String> {
        let (runner, worker_data) = self.verify_native_worker_match(matched).await?;
        let (method, run_schema_digest) = validate_native_oneshot_runner_schema(&runner)?;
        ensure_method_output(&worker_data, &method, false)?;
        Ok(run_schema_digest)
    }
}

fn native_worker_match(parts: NativeWorkerMatchParts) -> NativeWorkerMatch {
    NativeWorkerMatch {
        worker_id: parts.worker_id,
        worker_name: parts.worker_name,
        runner_id: parts.runner_id,
        settings_digest: parts.settings_digest,
        method_schema_digest: parts.method_schema_digest,
        method_schema: parts.method_schema,
        settings_descriptor: parts.settings_descriptor,
        normalized_runner_settings: parts.normalized_runner_settings,
        guest_root: parts.guest_root,
        host_context: parts.host_context,
        scope: parts.scope,
    }
}

fn validate_native_spec(spec: &NativeWorkerRegistrationSpec) -> AppResult<(String, String)> {
    if !valid_identity_text(&spec.native_profile_id)
        || spec.repository_id.as_ref().is_some_and(|repository| {
            !valid_identity_text(&repository.host)
                || !valid_identity_text(&repository.owner)
                || !valid_identity_text(&repository.name)
        })
        || !spec.runner_settings_json.is_object()
    {
        return Err(native_worker_error(
            "profile, repository, or settings are invalid",
        ));
    }
    let sdk_home =
        canonical_absolute_path(&spec.host_context.microsandbox_sdk_home, "host SDK home")?;
    let backend_config_path = canonical_absolute_path(
        &spec.host_context.backend_config_path,
        "backend config path",
    )?;
    Ok((sdk_home, backend_config_path))
}

fn valid_identity_text(value: &str) -> bool {
    !value.trim().is_empty() && !value.chars().any(char::is_control)
}

fn canonical_absolute_path(path: &Path, label: &str) -> AppResult<String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(native_worker_error(format!(
            "{label} must be absolute and normalized"
        )));
    }
    path.to_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| native_worker_error(format!("{label} is not valid UTF-8")))
}

fn validate_workspace_binding(
    binding: &NativeWorkspaceBinding,
    runner_settings: &Value,
) -> AppResult<(String, String)> {
    let host_root = canonical_absolute_path(&binding.canonical_host_root, "workspace root")?;
    reject_symlinked_existing_path_components(&binding.canonical_host_root)?;
    if binding.guest_root != "/workspace"
        || !contains_writable_workspace_mount(runner_settings, &host_root, &binding.guest_root)
    {
        return Err(native_worker_error(
            "fixed runner settings do not bind the canonical repository root at /workspace",
        ));
    }
    Ok((host_root, binding.guest_root.clone()))
}

fn native_worker_scope(
    spec: &NativeWorkerRegistrationSpec,
    host_root: &str,
    guest_root: &str,
    settings: &Value,
) -> AppResult<NativeWorkerScope> {
    let vm = settings
        .get("vm")
        .and_then(Value::as_object)
        .ok_or_else(|| native_worker_error("native VM scope is missing"))?;
    let field =
        |camel_case: &str, snake_case: &str| vm.get(camel_case).or_else(|| vm.get(snake_case));
    let image_ref = field("image", "image")
        .and_then(Value::as_str)
        .ok_or_else(|| native_worker_error("native VM image is missing"))?;
    let image_ref = image_ref.strip_prefix("oci://").unwrap_or(image_ref);
    let image_digest = image_ref
        .rsplit_once("@sha256:")
        .map(|(_, digest)| digest)
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(|digest| format!("sha256:{digest}"))
        .ok_or_else(|| native_worker_error("native VM image must be pinned by SHA-256"))?;

    let cpus = field("cpus", "cpus")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| native_worker_error("native VM CPU limit is missing"))?;
    let cpu_millis = cpus * 1000.0;
    let rounded_cpu_millis = cpu_millis.round();
    if rounded_cpu_millis > f64::from(u32::MAX)
        || (cpu_millis - rounded_cpu_millis).abs() > 0.000_001
    {
        return Err(native_worker_error(
            "native VM CPU limit is not representable in millicpus",
        ));
    }
    let cpu_millis = rounded_cpu_millis as u32;
    let memory_mib = native_vm_unsigned_setting(field("memoryMib", "memory_mib"))
        .ok_or_else(|| native_worker_error("native VM memory limit is missing"))?;
    let root_disk_mib = native_vm_unsigned_setting(field("rootDiskMib", "root_disk_mib"))
        .ok_or_else(|| native_worker_error("native VM root disk limit is missing"))?;

    Ok(NativeWorkerScope {
        native_profile_id: spec.native_profile_id.clone(),
        repository_id: spec.repository_id.clone(),
        canonical_host_root: PathBuf::from(host_root),
        guest_root: guest_root.to_owned(),
        image_digest,
        cpu_millis,
        memory_mib,
        root_disk_mib,
        use_static: spec.use_static,
    })
}

fn native_vm_unsigned_setting(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64)
}

fn reject_symlinked_existing_path_components(path: &Path) -> AppResult<()> {
    let mut component_path = PathBuf::new();
    for component in path.components() {
        component_path.push(component.as_os_str());
        match std::fs::symlink_metadata(&component_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(native_worker_error(
                    "workspace root cannot contain a symlink component",
                ));
            }
            Ok(_) => {}
            // Do not require the repository tree to exist yet or inspect anything below
            // its first missing component; the fixed mount binding is not a readiness probe.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => {
                return Err(native_worker_error(
                    "workspace root components could not be inspected",
                ));
            }
        }
    }
    Ok(())
}

fn contains_writable_workspace_mount(settings: &Value, host_root: &str, guest_root: &str) -> bool {
    let Some(mounts) = settings
        .get("vm")
        .and_then(|vm| vm.get("mounts"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    mounts.iter().any(|mount| {
        let Some(fields) = mount.as_object() else {
            return false;
        };
        let host_path = fields
            .get("hostPath")
            .or_else(|| fields.get("host_path"))
            .and_then(Value::as_str);
        let guest_path = fields
            .get("guestPath")
            .or_else(|| fields.get("guest_path"))
            .and_then(Value::as_str);
        host_path == Some(host_root)
            && guest_path == Some(guest_root)
            && fields.get("writable").and_then(Value::as_bool) == Some(true)
    })
}

fn validate_native_runner_schema_for_route(
    runner: &RunnerData,
    route: NativeWorkerRoute,
) -> AppResult<(MessageDescriptor, MethodSchema, String)> {
    match route {
        NativeWorkerRoute::Interactive => validate_native_runner_schema(runner),
        NativeWorkerRoute::OneShot => {
            let descriptor = native_settings_descriptor(runner)?;
            let (method, _) = validate_native_oneshot_runner_schema(runner)?;
            Ok((descriptor, method, runner_contract_digest(runner)?))
        }
    }
}

fn native_settings_descriptor(runner: &RunnerData) -> AppResult<MessageDescriptor> {
    JobworkerpProto::parse_runner_settings_schema_descriptor(runner)
        .map_err(|_| native_worker_error("SANDBOX settings descriptor is invalid"))?
        .filter(|descriptor| descriptor.name() == "SandboxRunnerSettings")
        .ok_or_else(|| native_worker_error("SANDBOX settings descriptor is unsupported"))
}

fn validate_native_runner_schema(
    runner: &RunnerData,
) -> AppResult<(MessageDescriptor, MethodSchema, String)> {
    let settings_descriptor = native_settings_descriptor(runner)?;
    let method_map = runner
        .method_proto_map
        .as_ref()
        .ok_or_else(|| native_worker_error("SANDBOX runner has no method descriptors"))?;
    let method = method_map
        .schemas
        .get(NATIVE_SANDBOX_METHOD)
        .cloned()
        .ok_or_else(|| native_worker_error("SANDBOX run_with_client method is unavailable"))?;
    if !method.require_client_stream
        || !matches!(
            StreamingOutputType::try_from(method.output_type),
            Ok(StreamingOutputType::Streaming | StreamingOutputType::Both)
        )
    {
        return Err(native_worker_error(
            "SANDBOX run_with_client is not a client-streaming output method",
        ));
    }
    let args_descriptor =
        JobworkerpProto::parse_job_args_schema_descriptor(runner, Some(NATIVE_SANDBOX_METHOD))
            .map_err(|_| native_worker_error("SANDBOX run_with_client args descriptor is invalid"))?
            .ok_or_else(|| {
                native_worker_error("SANDBOX run_with_client args descriptor is missing")
            })?;
    let is_string_field = |name: &str| {
        args_descriptor
            .get_field_by_name(name)
            .is_some_and(|field| !field.is_list() && field.kind() == prost_reflect::Kind::String)
    };
    if !is_string_field("command")
        || !is_string_field("working_dir")
        || !args_descriptor
            .get_field_by_name("args")
            .is_some_and(|field| field.is_list() && field.kind() == prost_reflect::Kind::String)
    {
        return Err(native_worker_error(
            "SANDBOX run_with_client args do not match the native command contract",
        ));
    }
    let result_descriptor =
        JobworkerpProto::parse_result_schema_descriptor(runner, Some(NATIVE_SANDBOX_METHOD))
            .map_err(|_| {
                native_worker_error("SANDBOX run_with_client result descriptor is invalid")
            })?
            .ok_or_else(|| native_worker_error("SANDBOX run_with_client result is missing"))?;
    validate_sandbox_result_wire_contract(runner, NATIVE_SANDBOX_METHOD, &result_descriptor)
        .map_err(|_| {
            native_worker_error(
                "SANDBOX run_with_client result is incompatible with the native decoder",
            )
        })?;
    validate_native_oneshot_runner_schema(runner).map_err(|_| {
        native_worker_error("SANDBOX run method is incompatible with the native decoder")
    })?;

    let schema_digest = runner_contract_digest(runner)?;
    Ok((settings_descriptor, method, schema_digest))
}

fn validate_native_oneshot_runner_schema(runner: &RunnerData) -> AppResult<(MethodSchema, String)> {
    let method = runner
        .method_proto_map
        .as_ref()
        .and_then(|method_map| method_map.schemas.get(SANDBOX_ONESHOT_METHOD))
        .cloned()
        .ok_or_else(|| transport_error("SANDBOX run method is unavailable"))?;
    if method.require_client_stream
        || !matches!(
            StreamingOutputType::try_from(method.output_type),
            Ok(StreamingOutputType::Streaming | StreamingOutputType::Both)
        )
    {
        return Err(transport_error(
            "SANDBOX run is not a one-shot streaming output method",
        ));
    }

    let args_descriptor =
        JobworkerpProto::parse_job_args_schema_descriptor(runner, Some(SANDBOX_ONESHOT_METHOD))
            .map_err(|_| transport_error("SANDBOX run args descriptor is invalid"))?
            .ok_or_else(|| transport_error("SANDBOX run args descriptor is missing"))?;
    let is_string_field = |name: &str| {
        args_descriptor
            .get_field_by_name(name)
            .is_some_and(|field| !field.is_list() && field.kind() == prost_reflect::Kind::String)
    };
    if !is_string_field("command")
        || !is_string_field("working_dir")
        || !args_descriptor
            .get_field_by_name("args")
            .is_some_and(|field| field.is_list() && field.kind() == prost_reflect::Kind::String)
    {
        return Err(transport_error(
            "SANDBOX run args do not match the native command contract",
        ));
    }

    let result_descriptor =
        JobworkerpProto::parse_result_schema_descriptor(runner, Some(SANDBOX_ONESHOT_METHOD))
            .map_err(|_| transport_error("SANDBOX run result descriptor is invalid"))?
            .ok_or_else(|| transport_error("SANDBOX run result descriptor is missing"))?;
    validate_sandbox_result_wire_contract(runner, SANDBOX_ONESHOT_METHOD, &result_descriptor)?;
    let schema_digest = native_method_schema_digest(&method)?;
    Ok((method, schema_digest))
}

fn validate_sandbox_result_wire_contract(
    runner: &RunnerData,
    using: &str,
    live_descriptor: &MessageDescriptor,
) -> AppResult<()> {
    let mut fixed_runner = runner.clone();
    let fixed_method = fixed_runner
        .method_proto_map
        .as_mut()
        .and_then(|method_map| method_map.schemas.get_mut(using))
        .ok_or_else(|| transport_error("SANDBOX result method is unavailable"))?;
    fixed_method.result_proto = FIXED_SANDBOX_RESULT_PROTO.to_owned();
    let fixed_descriptor =
        JobworkerpProto::parse_result_schema_descriptor(&fixed_runner, Some(using))
            .map_err(|_| transport_error("fixed SANDBOX result contract is invalid"))?
            .ok_or_else(|| transport_error("fixed SANDBOX result contract is missing"))?;

    if !message_wire_contract_matches(live_descriptor, &fixed_descriptor, 0) {
        return Err(transport_error(
            "live SANDBOX result schema is incompatible with the native decoder",
        ));
    }
    Ok(())
}

fn message_wire_contract_matches(
    live: &MessageDescriptor,
    fixed: &MessageDescriptor,
    depth: usize,
) -> bool {
    const MAX_RESULT_SCHEMA_NESTING: usize = 8;
    if depth > MAX_RESULT_SCHEMA_NESTING || live.fields().len() != fixed.fields().len() {
        return false;
    }

    fixed.fields().all(|fixed_field| {
        let Some(live_field) = live.get_field(fixed_field.number()) else {
            return false;
        };
        if live_field.cardinality() != fixed_field.cardinality()
            || live_field.is_map() != fixed_field.is_map()
            || live_field.is_group() != fixed_field.is_group()
            || live_field.is_packed() != fixed_field.is_packed()
            || live_field.supports_presence() != fixed_field.supports_presence()
            || live_field.is_required() != fixed_field.is_required()
            || oneof_field_numbers(&live_field) != oneof_field_numbers(&fixed_field)
        {
            return false;
        }

        let live_kind = live_field.kind();
        let fixed_kind = fixed_field.kind();
        let types_match = match (live_kind, fixed_kind) {
            (prost_reflect::Kind::Message(live), prost_reflect::Kind::Message(fixed)) => {
                message_wire_contract_matches(&live, &fixed, depth + 1)
            }
            (prost_reflect::Kind::Enum(live), prost_reflect::Kind::Enum(fixed)) => {
                let live_numbers: BTreeSet<_> = live.values().map(|value| value.number()).collect();
                let fixed_numbers: BTreeSet<_> =
                    fixed.values().map(|value| value.number()).collect();
                live_numbers == fixed_numbers
            }
            (live, fixed) => live == fixed,
        };
        if !types_match {
            return false;
        }

        // Names and packages are not on the wire. Scalar default and presence
        // semantics are, so compare them while intentionally recursing into
        // message fields without comparing descriptor names.
        matches!(fixed_field.kind(), prost_reflect::Kind::Message(_))
            || live_field.default_value() == fixed_field.default_value()
    })
}

fn oneof_field_numbers(field: &prost_reflect::FieldDescriptor) -> Option<BTreeSet<u32>> {
    field.containing_oneof().map(|oneof| {
        oneof
            .fields()
            .map(|member| member.number())
            .collect::<BTreeSet<_>>()
    })
}

fn native_method_schema_digest(method: &MethodSchema) -> AppResult<String> {
    let fingerprint = MethodSchemaFingerprint {
        args_proto: &method.args_proto,
        result_proto: &method.result_proto,
        description: &method.description,
        output_type: method.output_type,
        require_client_stream: method.require_client_stream,
        client_stream_data_proto: &method.client_stream_data_proto,
    };
    let value = serde_json::to_value(fingerprint)
        .map_err(|_| transport_error("SANDBOX run schema could not be encoded"))?;
    digest_json(&value).map_err(|_| transport_error("SANDBOX run schema could not be hashed"))
}

fn runner_contract_digest(runner: &RunnerData) -> AppResult<String> {
    let mut method_schemas = BTreeMap::new();
    for (name, method) in runner
        .method_proto_map
        .as_ref()
        .into_iter()
        .flat_map(|method_map| method_map.schemas.iter())
    {
        method_schemas.insert(
            name.clone(),
            MethodSchemaFingerprint {
                args_proto: &method.args_proto,
                result_proto: &method.result_proto,
                description: &method.description,
                output_type: method.output_type,
                require_client_stream: method.require_client_stream,
                client_stream_data_proto: &method.client_stream_data_proto,
            },
        );
    }
    let fingerprint = serde_json::json!({
        "name": runner.name,
        "runner_type": runner.runner_type,
        "definition": runner.definition,
        "settings_proto": runner.runner_settings_proto,
        "methods": method_schemas,
    });
    digest_json(&fingerprint)
}

fn encode_native_settings(
    descriptor: &MessageDescriptor,
    settings_json: &Value,
) -> AppResult<(Vec<u8>, Value)> {
    let encoded =
        JobworkerpProto::json_value_to_message(descriptor.clone(), settings_json, true, false)
            .map_err(|_| {
                native_worker_error("settings do not match the live SANDBOX descriptor")
            })?;
    let normalized = DynamicMessage::decode(descriptor.clone(), encoded.as_slice())
        .map_err(|_| native_worker_error("encoded SANDBOX settings could not be decoded"))?
        .serialize(serde_json::value::Serializer)
        .map_err(|_| native_worker_error("encoded SANDBOX settings could not be normalized"))?;
    Ok((encoded, canonicalize_json(normalized)))
}

fn validate_native_interactive_deadlines(settings: &Value) -> AppResult<()> {
    let vm = settings.get("vm").and_then(Value::as_object);
    let configured =
        |object: Option<&serde_json::Map<String, Value>>, camel_case: &str, snake_case: &str| {
            object
                .and_then(|fields| fields.get(camel_case).or_else(|| fields.get(snake_case)))
                .is_some_and(|value| !value.is_null())
        };
    if configured(
        settings.as_object(),
        "defaultExecTimeoutMs",
        "default_exec_timeout_ms",
    ) || configured(vm, "maxDurationSec", "max_duration_sec")
        || configured(vm, "idleTimeoutSec", "idle_timeout_sec")
    {
        return Err(native_worker_error(
            "interactive SANDBOX worker must not set execution or idle deadlines",
        ));
    }
    Ok(())
}

fn native_worker_digest(input: NativeWorkerDigestInput<'_>) -> AppResult<String> {
    let NativeWorkerDigestInput {
        route,
        spec,
        sdk_home,
        backend_config_path,
        workspace_host_root,
        workspace_guest_root,
        runner_id,
        runner,
        method,
        method_schema_digest,
        normalized_runner_settings,
    } = input;
    let method_schemas = runner
        .method_proto_map
        .as_ref()
        .into_iter()
        .flat_map(|method_map| method_map.schemas.iter())
        .map(|(name, schema)| {
            (
                name.clone(),
                MethodSchemaFingerprint {
                    args_proto: &schema.args_proto,
                    result_proto: &schema.result_proto,
                    description: &schema.description,
                    output_type: schema.output_type,
                    require_client_stream: schema.require_client_stream,
                    client_stream_data_proto: &schema.client_stream_data_proto,
                },
            )
        })
        .collect();
    let fingerprint = NativeWorkerFingerprint {
        version: 1,
        native_profile_id: &spec.native_profile_id,
        repository_id: &spec.repository_id,
        workspace_host_root,
        workspace_guest_root,
        sdk_home,
        backend_config_path,
        runner_id: runner_id.value,
        runner_type: runner.runner_type,
        runner_name: &runner.name,
        runner_definition: &runner.definition,
        runner_settings_proto: &runner.runner_settings_proto,
        method_schemas,
        normalized_runner_settings,
        description: route.description(),
        retry_type: RetryType::None as i32,
        retry_interval: 0,
        retry_max_interval: 0,
        retry_max_count: 0,
        retry_basis: 1.0,
        periodic_interval: 0,
        channel: route.channel(),
        queue_type: QueueType::Normal as i32,
        response_type: ResponseType::Direct as i32,
        store_success: false,
        store_failure: true,
        use_static: spec.use_static,
        broadcast_results: true,
    };
    let json = serde_json::to_value(fingerprint)
        .map_err(|_| native_worker_error("worker identity could not be encoded"))?;
    let canonical = canonicalize_json(json);
    let mut bytes = serde_json::to_vec(&canonical)
        .map_err(|_| native_worker_error("worker identity could not be hashed"))?;
    bytes.extend_from_slice(method_schema_digest.as_bytes());
    // The selected method is part of the contract even when another SANDBOX
    // method is added or reordered in the live method map.
    bytes.extend_from_slice(method.args_proto.as_bytes());
    Ok(hex_digest(&Sha256::digest(bytes)))
}

fn native_worker_data(
    name: String,
    runner_id: RunnerId,
    runner_settings: Vec<u8>,
    use_static: bool,
    route: NativeWorkerRoute,
) -> WorkerData {
    WorkerData {
        name,
        description: route.description().to_owned(),
        runner_id: Some(runner_id),
        runner_settings,
        retry_policy: Some(RetryPolicy {
            r#type: RetryType::None as i32,
            basis: 1.0,
            ..Default::default()
        }),
        periodic_interval: 0,
        channel: Some(route.channel().to_owned()),
        queue_type: QueueType::Normal as i32,
        response_type: ResponseType::Direct as i32,
        store_success: false,
        store_failure: true,
        use_static,
        broadcast_results: true,
    }
}

fn verify_native_worker(
    worker_id: &WorkerId,
    actual: &WorkerData,
    expected: &WorkerData,
    settings_descriptor: &MessageDescriptor,
    expected_settings: &Value,
) -> AppResult<()> {
    if worker_id.value <= 0
        || actual.name != expected.name
        || actual.description != expected.description
        || actual.runner_id != expected.runner_id
        || actual.retry_policy != expected.retry_policy
        || actual.periodic_interval != expected.periodic_interval
        || actual.channel != expected.channel
        || actual.queue_type != expected.queue_type
        || actual.response_type != expected.response_type
        || actual.store_success != expected.store_success
        || actual.store_failure != expected.store_failure
        || actual.use_static != expected.use_static
        || actual.broadcast_results != expected.broadcast_results
    {
        return Err(native_worker_error(
            "an existing worker with this identity has different fixed settings",
        ));
    }
    let actual_settings = DynamicMessage::decode(
        settings_descriptor.clone(),
        actual.runner_settings.as_slice(),
    )
    .map_err(|_| native_worker_error("existing worker settings are not decodable"))?
    .serialize(serde_json::value::Serializer)
    .map_err(|_| native_worker_error("existing worker settings are not normalizable"))?;
    if canonicalize_json(actual_settings) != *expected_settings {
        return Err(native_worker_error(
            "an existing worker with this identity has different runner settings",
        ));
    }
    Ok(())
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        Value::Object(values) => {
            let mut sorted = BTreeMap::new();
            for (key, value) in values {
                sorted.insert(key, canonicalize_json(value));
            }
            Value::Object(sorted.into_iter().collect())
        }
        other => other,
    }
}

fn digest_json(value: &Value) -> AppResult<String> {
    let canonical = canonicalize_json(value.clone());
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|_| native_worker_error("runner contract could not be hashed"))?;
    Ok(hex_digest(&Sha256::digest(bytes)))
}

fn hex_digest(digest: &[u8]) -> String {
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn native_worker_error(message: impl Into<String>) -> AppError {
    AppError::Jobworkerp(format!("native interactive worker: {}", message.into()))
}

/// Limits the bounded stdin queue and each binary feed frame.
#[derive(Debug, Clone, Copy)]
pub struct CodingAgentTransportOptions {
    pub input_buffer_capacity: usize,
    pub max_input_chunk_bytes: usize,
    /// Maximum time to wait for clean gRPC EOF after a valid SANDBOX End.
    pub terminal_drain_timeout: Duration,
}

impl Default for CodingAgentTransportOptions {
    fn default() -> Self {
        Self {
            input_buffer_capacity: 8,
            max_input_chunk_bytes: 64 * 1024,
            terminal_drain_timeout: Duration::from_secs(5),
        }
    }
}

/// A binary output chunk or the final sandbox process status.
#[derive(Clone, PartialEq, Eq)]
pub enum CodingAgentEvent {
    Output {
        stream: String,
        data: Vec<u8>,
    },
    Exit {
        exit_code: i32,
        execution_time_ms: u64,
        sandbox_id: String,
    },
}

impl std::fmt::Debug for CodingAgentEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Output { stream, data } => f
                .debug_struct("Output")
                .field("stream", stream)
                .field("data_len", &data.len())
                .finish(),
            Self::Exit {
                exit_code,
                execution_time_ms,
                sandbox_id,
            } => f
                .debug_struct("Exit")
                .field("exit_code", exit_code)
                .field("execution_time_ms", execution_time_ms)
                .field("sandbox_id", sandbox_id)
                .finish(),
        }
    }
}

/// Unique owner of one job's result stream.
///
/// Obtain a cloneable [`CodingAgentInput`] with [`Self::input_handle`] to send
/// stdin concurrently while this value drains output. Dropping either handle
/// never sends JobService/Delete. The client input stream closes when it is
/// explicitly finalized or its last input handle is dropped.
pub struct CodingAgentStream {
    job_id: JobId,
    input: Option<CodingAgentInput>,
    output: Streaming<ResultOutputItem>,
    terminal_drain_timeout: Duration,
    saw_exit: bool,
    ended: bool,
}

impl std::fmt::Debug for CodingAgentStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodingAgentStream")
            .field("job_id", &self.job_id)
            .field("accepts_input", &self.input.is_some())
            .field("saw_exit", &self.saw_exit)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

/// Authentication/settings shell. `/bin/sh` is dash on Debian-based agent
/// images, which has no line editing (arrow keys only echo `^[[D`), so bash is
/// used whenever the image provides it.
pub const PROFILE_SETTINGS_SHELL_SCRIPT: &str =
    "if command -v bash >/dev/null 2>&1; then exec bash -i; fi; exec sh -i";

/// Cloneable, bounded stdin writer for `run_with_client` SANDBOX jobs.
///
/// Calls are serialized across clones, so the single final frame is ordered
/// after all earlier writes. Its lock is never shared with the output reader.
#[derive(Clone)]
pub struct CodingAgentInput {
    sender: Arc<Mutex<Option<mpsc::Sender<FeedDataTransport>>>>,
    max_chunk_bytes: usize,
}

impl std::fmt::Debug for CodingAgentInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodingAgentInput")
            .field("max_chunk_bytes", &self.max_chunk_bytes)
            .finish_non_exhaustive()
    }
}

impl CodingAgentInput {
    /// Send one opaque stdin chunk. A bounded queue applies backpressure;
    /// chunk contents are never included in error messages.
    pub async fn send_input(&self, bytes: Vec<u8>) -> AppResult<()> {
        if bytes.len() > self.max_chunk_bytes {
            return Err(transport_error(format!(
                "stdin chunk exceeds the configured limit of {} bytes",
                self.max_chunk_bytes
            )));
        }
        let sender = self.sender.lock().await;
        let sender = sender
            .as_ref()
            .ok_or_else(|| transport_error("client input is already finalized"))?;
        sender
            .send(FeedDataTransport {
                data: bytes,
                is_final: false,
            })
            .await
            .map_err(|_| transport_error("client input stream is closed"))
    }

    /// Mark stdin EOF once with a final `FeedDataTransport` frame.
    /// This is not cancellation; use `JobworkerpHandle::cancel` separately.
    pub async fn finish_input(&self) -> AppResult<()> {
        let mut sender_guard = self.sender.lock().await;
        let sender = sender_guard
            .as_ref()
            .ok_or_else(|| transport_error("client input is already finalized"))?;
        if sender
            .send(FeedDataTransport {
                data: Vec::new(),
                is_final: true,
            })
            .await
            .is_err()
        {
            *sender_guard = None;
            return Err(transport_error("client input stream is closed"));
        }
        *sender_guard = None;
        Ok(())
    }
}

impl CodingAgentStream {
    /// The server-assigned identity from the response's `x-job-id-bin` header.
    pub fn job_id(&self) -> &JobId {
        &self.job_id
    }

    /// Return a cloneable bounded writer for client-streaming methods.
    /// The returned handle can be moved to an input task while `self` remains
    /// the unique output owner.
    pub fn input_handle(&self) -> Option<CodingAgentInput> {
        self.input.clone()
    }

    /// Receive the next decoded sandbox result. Returns `None` only after a
    /// valid explicit End item; transport close without End is an error.
    pub async fn next_event(&mut self) -> AppResult<Option<CodingAgentEvent>> {
        if self.ended {
            return Ok(None);
        }

        let Some(item) = self
            .output
            .message()
            .await
            .map_err(safe_stream_status_error)?
        else {
            return Err(transport_error(
                "sandbox result stream closed without a terminal End item",
            ));
        };
        match item.item {
            Some(Item::Data(bytes)) | Some(Item::FinalCollected(bytes)) => {
                self.decode_result(&bytes).map(Some)
            }
            Some(Item::End(trailer)) => {
                check_sandbox_stream_error(&trailer)?;
                if !self.saw_exit {
                    return Err(transport_error(
                        "sandbox result stream ended without an exit result",
                    ));
                }
                match tokio::time::timeout(self.terminal_drain_timeout, self.output.message()).await
                {
                    Err(_) => Err(transport_error(
                        "sandbox result stream did not close after End within the configured timeout",
                    )),
                    Ok(Err(status)) => Err(safe_stream_status_error(status)),
                    Ok(Ok(Some(_))) => Err(transport_error(
                        "sandbox result stream contained an item after End",
                    )),
                    Ok(Ok(None)) => {
                        self.ended = true;
                        Ok(None)
                    }
                }
            }
            None => Err(transport_error(
                "sandbox result stream contained an unrecognized result item",
            )),
        }
    }

    fn decode_result(&mut self, bytes: &[u8]) -> AppResult<CodingAgentEvent> {
        let result = SandboxExecResult::decode(bytes)
            .map_err(|_| transport_error("sandbox result contained malformed protobuf data"))?;
        match result.result {
            Some(SandboxResult::Output(output)) if !self.saw_exit => Ok(CodingAgentEvent::Output {
                stream: output.stream,
                data: output.data,
            }),
            Some(SandboxResult::Exit(exit)) if !self.saw_exit => {
                self.saw_exit = true;
                Ok(CodingAgentEvent::Exit {
                    exit_code: exit.exit_code,
                    execution_time_ms: exit.execution_time_ms,
                    sandbox_id: exit.sandbox_id,
                })
            }
            Some(SandboxResult::Output(_)) => Err(transport_error(
                "sandbox result included output after the exit result",
            )),
            Some(SandboxResult::Exit(_)) => Err(transport_error(
                "sandbox result included more than one exit result",
            )),
            None => Err(transport_error(
                "sandbox result contained no recognized result field",
            )),
        }
    }
}

/// One-shot SANDBOX version-probe result stream. The job id and digests are
/// evidence of which registered configuration was checked, not a claim that
/// the SDK, VM, or a particular executable version is ready.
pub struct NativeVersionProbe {
    stream: CodingAgentStream,
    worker_id: WorkerId,
    settings_digest: String,
    matched_runner_schema_digest: String,
    run_schema_digest: String,
    deadline: tokio::time::Instant,
}

impl std::fmt::Debug for NativeVersionProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeVersionProbe")
            .field("job_id", self.stream.job_id())
            .field("worker_id", &self.worker_id)
            .field("settings_digest", &self.settings_digest)
            .field(
                "matched_runner_schema_digest",
                &self.matched_runner_schema_digest,
            )
            .field("run_schema_digest", &self.run_schema_digest)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl NativeVersionProbe {
    /// The positive, server-assigned job id returned by EnqueueForStream.
    pub fn job_id(&self) -> &JobId {
        self.stream.job_id()
    }

    pub fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    pub fn settings_digest(&self) -> &str {
        &self.settings_digest
    }

    /// Full live SANDBOX method-map digest to which the match was originally
    /// bound, including both `run_with_client` and `run`.
    pub fn matched_runner_schema_digest(&self) -> &str {
        &self.matched_runner_schema_digest
    }

    /// Digest of the live SANDBOX `run` method schema used for this probe.
    pub fn run_schema_digest(&self) -> &str {
        &self.run_schema_digest
    }

    pub fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }

    /// Drain one decoded event, bounded by the probe's original overall
    /// deadline. A nonzero exit remains an ordinary Exit event for the caller
    /// to interpret; no SDK/VM readiness inference is made here.
    pub async fn next_event(&mut self) -> AppResult<Option<CodingAgentEvent>> {
        tokio::time::timeout_at(self.deadline, self.stream.next_event())
            .await
            .map_err(|_| transport_error("native version probe deadline exceeded"))?
    }
}

impl JobworkerpHandle {
    /// Dispatch the one-shot `--version` command for the selected executable on
    /// an already matched native worker. Only the verified Worker ID is used;
    /// this does not create per-probe workers or infer runtime readiness.
    pub async fn start_native_version_probe(
        &self,
        matched: &NativeWorkerMatch,
        prepared_cwd: &str,
    ) -> AppResult<NativeVersionProbe> {
        self.start_native_version_probe_with_options(
            matched,
            prepared_cwd,
            NativeVersionProbeOptions::default(),
        )
        .await
    }

    /// Native-only options variant, useful for callers that need a shorter
    /// bounded wait. The server job timeout remains fixed at 60 seconds.
    pub async fn start_native_version_probe_with_options(
        &self,
        matched: &NativeWorkerMatch,
        prepared_cwd: &str,
        options: NativeVersionProbeOptions,
    ) -> AppResult<NativeVersionProbe> {
        self.start_native_agent_version_probe(matched, prepared_cwd, "opencode", options)
            .await
    }

    /// Probe the selected executable with literal argv and the live SANDBOX run schema.
    pub async fn start_native_agent_version_probe(
        &self,
        matched: &NativeWorkerMatch,
        prepared_cwd: &str,
        command: &str,
        options: NativeVersionProbeOptions,
    ) -> AppResult<NativeVersionProbe> {
        if !matches!(command, "opencode" | "codex" | "claude") {
            return Err(transport_error("unsupported native agent version probe"));
        }
        if options.timeout.is_zero() || options.timeout > NATIVE_VERSION_PROBE_TIMEOUT {
            return Err(transport_error(
                "native version probe timeout must be greater than zero and at most 60 seconds",
            ));
        }
        let deadline = tokio::time::Instant::now() + options.timeout;
        let working_dir = validate_native_guest_working_dir(&matched.guest_root, prepared_cwd)?;
        const ARGS: [&str; 1] = ["--version"];
        validate_native_command(command, &[ARGS[0].to_owned()])?;

        let start = async {
            let (runner, worker_data) = self.verify_native_worker_match(matched).await?;
            let (method, run_schema_digest) = validate_native_oneshot_runner_schema(&runner)?;
            ensure_method_output(&worker_data, &method, false)?;

            let args = vec![ARGS[0].to_owned()];
            let args = NativeSandboxJobArgs {
                command,
                args: &args,
                working_dir: &working_dir,
            };
            let argument_json = serde_json::to_value(args).map_err(|_| {
                transport_error("native version probe arguments could not be encoded")
            })?;
            let json_size = serde_json::to_vec(&argument_json)
                .map_err(|_| {
                    transport_error("native version probe arguments could not be bounded")
                })?
                .len();
            if json_size > MAX_NATIVE_JOB_ARGS_BYTES {
                return Err(transport_error(
                    "native version probe arguments exceed the configured size limit",
                ));
            }
            let args_descriptor = JobworkerpProto::parse_job_args_schema_descriptor(
                &runner,
                Some(SANDBOX_ONESHOT_METHOD),
            )
            .map_err(|_| transport_error("SANDBOX run args schema is invalid"))?
            .ok_or_else(|| transport_error("SANDBOX run args schema is missing"))?;
            let encoded_args = JobworkerpProto::json_value_to_message(
                args_descriptor,
                &argument_json,
                true,
                false,
            )
            .map_err(|_| {
                transport_error("native version probe arguments do not match the live schema")
            })?;
            if encoded_args.len() > MAX_NATIVE_JOB_ARGS_BYTES {
                return Err(transport_error(
                    "encoded native version probe arguments exceed the configured size limit",
                ));
            }

            let mut job_request = make_job_request(
                matched.worker_id,
                encoded_args,
                Some(SANDBOX_ONESHOT_METHOD),
            );
            job_request.timeout = Some(NATIVE_VERSION_JOB_TIMEOUT_MS);
            let mut client = self
                .native_interactive_client()
                .await?
                .jobworkerp_client
                .job_client()
                .await;
            let response = tokio::time::timeout_at(
                deadline,
                tokio::time::timeout(
                    NATIVE_RPC_TIMEOUT,
                    client.enqueue_for_stream(Request::new(job_request)),
                ),
            )
            .await
            .map_err(|_| transport_error("native version probe deadline exceeded"))?
            .map_err(|_| {
                transport_error(
                    "native version probe enqueue open timed out; job acceptance may be unknown",
                )
            })?
            .map_err(|status| safe_rpc_error("enqueue native version probe", status))?;
            let job_id = job_id_from_metadata(response.metadata())?;

            Ok(NativeVersionProbe {
                stream: CodingAgentStream {
                    job_id,
                    input: None,
                    output: response.into_inner(),
                    terminal_drain_timeout: CodingAgentTransportOptions::default()
                        .terminal_drain_timeout,
                    saw_exit: false,
                    ended: false,
                },
                worker_id: matched.worker_id,
                settings_digest: matched.settings_digest.clone(),
                matched_runner_schema_digest: matched.method_schema_digest.clone(),
                run_schema_digest,
                deadline,
            })
        };

        tokio::time::timeout_at(deadline, start)
            .await
            .map_err(|_| transport_error("native version probe deadline exceeded"))?
    }

    /// Start a native interactive session on the already-matched dedicated
    /// SANDBOX worker. `command` is one executable token and `args` remain
    /// literal argv entries; this method never joins them into a shell command.
    /// The caller must be trusted native launch code, not renderer input.
    ///
    /// Only `command`, `args`, and a normalized guest `working_dir` are
    /// accepted as job arguments. Worker and runner state are reread before
    /// enqueue, and an enqueue failure is never retried because acceptance can
    /// be ambiguous.
    pub async fn start_native_interactive_session(
        &self,
        matched: &NativeWorkerMatch,
        command: &str,
        args: &[String],
        working_dir: &str,
    ) -> AppResult<CodingAgentStream> {
        self.start_native_interactive_session_before_dispatch(
            matched,
            command,
            args,
            working_dir,
            || {},
        )
        .await
    }

    async fn start_native_interactive_session_before_dispatch(
        &self,
        matched: &NativeWorkerMatch,
        command: &str,
        args: &[String],
        working_dir: &str,
        before_dispatch: impl FnOnce() + Send,
    ) -> AppResult<CodingAgentStream> {
        if matched.using() != NATIVE_SANDBOX_METHOD {
            return Err(transport_error(
                "automatic worker cannot start an interactive session",
            ));
        }
        let options = CodingAgentTransportOptions::default();
        validate_options(options)?;
        validate_native_command(command, args)?;
        let working_dir = validate_native_guest_working_dir(&matched.guest_root, working_dir)?;

        let (runner, _worker_data) = self.verify_native_worker_match(matched).await?;

        let argument_json = NativeSandboxJobArgs {
            command,
            args,
            working_dir: &working_dir,
        };
        let argument_json = serde_json::to_value(argument_json)
            .map_err(|_| transport_error("native sandbox arguments could not be encoded"))?;
        let json_size = serde_json::to_vec(&argument_json)
            .map_err(|_| transport_error("native sandbox arguments could not be bounded"))?
            .len();
        if json_size > MAX_NATIVE_JOB_ARGS_BYTES {
            return Err(transport_error(
                "native sandbox arguments exceed the configured size limit",
            ));
        }
        let args_descriptor =
            JobworkerpProto::parse_job_args_schema_descriptor(&runner, Some(NATIVE_SANDBOX_METHOD))
                .map_err(|_| transport_error("SANDBOX run_with_client args schema is invalid"))?
                .ok_or_else(|| transport_error("SANDBOX run_with_client args schema is missing"))?;
        let encoded_args =
            JobworkerpProto::json_value_to_message(args_descriptor, &argument_json, true, false)
                .map_err(|_| {
                    transport_error("native sandbox arguments do not match the live schema")
                })?;
        if encoded_args.len() > MAX_NATIVE_JOB_ARGS_BYTES {
            return Err(transport_error(
                "encoded native sandbox arguments exceed the configured size limit",
            ));
        }

        let mut job_request =
            make_job_request(matched.worker_id, encoded_args, Some(NATIVE_SANDBOX_METHOD));
        // JobRequest.timeout is an optional duration in milliseconds. Some(0) is
        // explicit on the wire and asks the server not to apply its default.
        job_request.timeout = Some(0);
        let (input_tx, input_rx) = mpsc::channel(options.input_buffer_capacity);
        let input_frames = ReceiverStream::new(input_rx).map(|feed_data| ClientStreamRequest {
            request: Some(client_stream_request::Request::FeedData(feed_data)),
        });
        let first_frame = stream::once(async move {
            ClientStreamRequest {
                request: Some(client_stream_request::Request::JobRequest(job_request)),
            }
        });
        let mut client = self
            .native_interactive_client()
            .await?
            .jobworkerp_client
            .job_client()
            .await;
        before_dispatch();
        let response = tokio::time::timeout(
            NATIVE_RPC_TIMEOUT,
            client.enqueue_with_client_stream(Request::new(first_frame.chain(input_frames))),
        )
        .await
        .map_err(|_| transport_error("enqueue open timed out; job acceptance may be unknown"))?
        .map_err(|status| safe_rpc_error("enqueue native interactive session", status))?;
        let job_id = job_id_from_metadata(response.metadata())?;

        Ok(CodingAgentStream {
            job_id,
            input: Some(CodingAgentInput {
                sender: Arc::new(Mutex::new(Some(input_tx))),
                max_chunk_bytes: options.max_input_chunk_bytes,
            }),
            output: response.into_inner(),
            terminal_drain_timeout: options.terminal_drain_timeout,
            saw_exit: false,
            ended: false,
        })
    }

    /// Start the SANDBOX `run_with_client` method with client-stream input.
    ///
    /// The returned stream contains the server-assigned job id. If opening the
    /// RPC fails before that id is received, acceptance may be unknown; this
    /// method never retries or resubmits the command.
    pub async fn start_coding_agent(
        &self,
        worker_name: &str,
        args: Value,
    ) -> AppResult<CodingAgentStream> {
        self.start_coding_agent_with_options(
            worker_name,
            args,
            CodingAgentTransportOptions::default(),
        )
        .await
    }

    /// Start a client-streaming SANDBOX `run_with_client` job with explicit limits.
    /// See [`Self::start_coding_agent`] for the no-automatic-retry contract.
    pub async fn start_coding_agent_with_options(
        &self,
        worker_name: &str,
        args: Value,
        options: CodingAgentTransportOptions,
    ) -> AppResult<CodingAgentStream> {
        validate_options(options)?;
        let (worker_id, worker_data, runner_data, method) = self
            .resolve_worker_method(worker_name, Some(SANDBOX_CLIENT_STREAM_METHOD))
            .await?;
        ensure_method_output(&worker_data, &method, true)?;
        let args = encode_args(&runner_data, Some(SANDBOX_CLIENT_STREAM_METHOD), &args)?;
        let job_request = make_job_request(worker_id, args, Some(SANDBOX_CLIENT_STREAM_METHOD));

        let (input_tx, input_rx) = mpsc::channel(options.input_buffer_capacity);
        let input_frames = ReceiverStream::new(input_rx).map(|feed_data| ClientStreamRequest {
            request: Some(client_stream_request::Request::FeedData(feed_data)),
        });
        let first_frame = stream::once(async move {
            ClientStreamRequest {
                request: Some(client_stream_request::Request::JobRequest(job_request)),
            }
        });
        let mut client = self.inner.jobworkerp_client.job_client().await;
        let response = client
            .enqueue_with_client_stream(Request::new(first_frame.chain(input_frames)))
            .await
            .map_err(|status| safe_rpc_error("enqueue client stream", status))?;
        let job_id = job_id_from_metadata(response.metadata())?;

        Ok(CodingAgentStream {
            job_id,
            input: Some(CodingAgentInput {
                sender: Arc::new(Mutex::new(Some(input_tx))),
                max_chunk_bytes: options.max_input_chunk_bytes,
            }),
            output: response.into_inner(),
            terminal_drain_timeout: options.terminal_drain_timeout,
            saw_exit: false,
            ended: false,
        })
    }

    /// Start the SANDBOX `run` method that does not require client-stream input. This
    /// deliberately uses EnqueueForStream and never falls back to unary enqueue.
    /// An enqueue error or missing job-id metadata is not retried because the
    /// server may already have accepted the request.
    pub async fn start_coding_agent_once(
        &self,
        worker_name: &str,
        args: Value,
    ) -> AppResult<CodingAgentStream> {
        self.start_coding_agent_once_with_options(
            worker_name,
            args,
            CodingAgentTransportOptions::default(),
        )
        .await
    }

    /// One-shot variant with an explicit terminal-drain timeout. No input
    /// handle is returned for this method.
    pub async fn start_coding_agent_once_with_options(
        &self,
        worker_name: &str,
        args: Value,
        options: CodingAgentTransportOptions,
    ) -> AppResult<CodingAgentStream> {
        validate_options(options)?;
        let (worker_id, worker_data, runner_data, method) = self
            .resolve_worker_method(worker_name, Some(SANDBOX_ONESHOT_METHOD))
            .await?;
        ensure_method_output(&worker_data, &method, false)?;
        let args = encode_args(&runner_data, Some(SANDBOX_ONESHOT_METHOD), &args)?;
        let job_request = make_job_request(worker_id, args, Some(SANDBOX_ONESHOT_METHOD));
        let mut client = self.inner.jobworkerp_client.job_client().await;
        let response = client
            .enqueue_for_stream(Request::new(job_request))
            .await
            .map_err(|status| safe_rpc_error("enqueue sandbox stream", status))?;
        let job_id = job_id_from_metadata(response.metadata())?;

        Ok(CodingAgentStream {
            job_id,
            input: None,
            output: response.into_inner(),
            terminal_drain_timeout: options.terminal_drain_timeout,
            saw_exit: false,
            ended: false,
        })
    }

    async fn resolve_worker_method(
        &self,
        worker_name: &str,
        using: Option<&str>,
    ) -> AppResult<(
        jobworkerp_client::jobworkerp::data::WorkerId,
        WorkerData,
        RunnerData,
        MethodSchema,
    )> {
        let (worker_id, worker_data) = self
            .inner
            .find_worker_by_name(None, Arc::new(HashMap::new()), worker_name)
            .await
            .map_err(|_| transport_error("worker lookup failed"))?
            .ok_or_else(|| transport_error("worker was not found"))?;
        let runner_id = worker_data
            .runner_id
            .ok_or_else(|| transport_error("worker has no runner id"))?;
        let runner = self
            .inner
            .jobworkerp_client
            .runner_client()
            .await
            .find(runner_id)
            .await
            .map_err(|_| transport_error("runner lookup failed"))?
            .into_inner()
            .data
            .and_then(|runner| runner.data)
            .ok_or_else(|| transport_error("runner was not found"))?;
        if runner.name != SANDBOX_RUNNER_NAME {
            return Err(transport_error(
                "worker is not backed by the SANDBOX runner",
            ));
        }
        let method = select_method_schema(&runner, using)?;
        if using == Some(SANDBOX_CLIENT_STREAM_METHOD) || using == Some(SANDBOX_ONESHOT_METHOD) {
            let result_descriptor = JobworkerpProto::parse_result_schema_descriptor(&runner, using)
                .map_err(|_| transport_error("SANDBOX result schema is invalid"))?
                .ok_or_else(|| transport_error("SANDBOX result schema is missing"))?;
            validate_sandbox_result_wire_contract(
                &runner,
                using.expect("a recognized SANDBOX method was selected"),
                &result_descriptor,
            )?;
        }
        Ok((worker_id, worker_data, runner, method))
    }
}

fn validate_options(options: CodingAgentTransportOptions) -> AppResult<()> {
    if options.input_buffer_capacity == 0 {
        return Err(transport_error(
            "stdin queue capacity must be greater than zero",
        ));
    }
    if options.max_input_chunk_bytes == 0 {
        return Err(transport_error(
            "stdin chunk limit must be greater than zero",
        ));
    }
    if options.terminal_drain_timeout.is_zero() {
        return Err(transport_error(
            "terminal drain timeout must be greater than zero",
        ));
    }
    Ok(())
}

fn select_method_schema(runner: &RunnerData, using: Option<&str>) -> AppResult<MethodSchema> {
    let schemas = runner
        .method_proto_map
        .as_ref()
        .map(|method_map| &method_map.schemas)
        .ok_or_else(|| transport_error("runner has no method schema"))?;
    let schema = match using {
        Some(using) => schemas
            .get(using)
            .ok_or_else(|| transport_error(format!("runner method is not registered: {using}")))?,
        None if schemas.len() == 1 => schemas
            .values()
            .next()
            .expect("a one-entry schema map contains one value"),
        None => {
            return Err(transport_error(
                "runner method must be selected with the using parameter",
            ));
        }
    };
    Ok(schema.clone())
}

fn ensure_method_output(
    worker: &WorkerData,
    method: &MethodSchema,
    client_stream: bool,
) -> AppResult<()> {
    let response_type = ResponseType::try_from(worker.response_type)
        .map_err(|_| transport_error("worker has an unknown response type"))?;
    if response_type != ResponseType::Direct {
        return Err(transport_error(
            "worker must use DIRECT response mode to return sandbox results",
        ));
    }
    if method.require_client_stream != client_stream {
        let message = if method.require_client_stream {
            "worker method requires client streaming; one-shot transport is unsupported"
        } else {
            "worker method does not require client streaming; use the one-shot transport"
        };
        return Err(transport_error(message));
    }
    let output_type = StreamingOutputType::try_from(method.output_type)
        .map_err(|_| transport_error("worker method has an unknown output type"))?;
    if output_type == StreamingOutputType::NonStreaming {
        return Err(transport_error(
            "worker method does not support streamed output",
        ));
    }
    Ok(())
}

fn encode_args(runner: &RunnerData, using: Option<&str>, args: &Value) -> AppResult<Vec<u8>> {
    let descriptor = JobworkerpProto::parse_job_args_schema_descriptor(runner, using)
        .map_err(|_| transport_error("worker args schema is invalid"))?;
    match descriptor {
        Some(descriptor) => JobworkerpProto::json_value_to_message(descriptor, args, true, true)
            .map_err(|_| transport_error("worker arguments do not match the registered schema")),
        None => serde_json::to_vec(args)
            .map_err(|_| transport_error("worker arguments could not be encoded")),
    }
}

#[derive(Serialize)]
struct NativeSandboxJobArgs<'a> {
    command: &'a str,
    args: &'a [String],
    working_dir: &'a str,
}

fn validate_native_command(command: &str, args: &[String]) -> AppResult<()> {
    if command.trim().is_empty()
        || command.chars().any(char::is_control)
        || args.len() > MAX_NATIVE_JOB_ARG_COUNT
        || args.iter().any(|argument| argument.contains('\0'))
    {
        return Err(transport_error("native command or argv is invalid"));
    }
    let raw_size = args.iter().fold(command.len(), |size, argument| {
        size.saturating_add(argument.len())
    });
    if raw_size > MAX_NATIVE_JOB_ARGS_BYTES {
        return Err(transport_error(
            "native sandbox arguments exceed the configured size limit",
        ));
    }
    Ok(())
}

fn validate_native_guest_working_dir(guest_root: &str, working_dir: &str) -> AppResult<String> {
    let path = Path::new(working_dir);
    if guest_root != "/workspace"
        || !path.is_absolute()
        || working_dir.chars().any(char::is_control)
    {
        return Err(transport_error(
            "session working directory must be an absolute guest workspace path",
        ));
    }

    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(component) => {
                let Some(component) = component.to_str() else {
                    return Err(transport_error(
                        "session working directory is not valid UTF-8",
                    ));
                };
                components.push(component);
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(transport_error(
                    "session working directory must not contain dot components",
                ));
            }
        }
    }
    let normalized = format!("/{}", components.join("/"));
    let root = Path::new(guest_root);
    if normalized != working_dir || !path.starts_with(root) {
        return Err(transport_error(
            "session working directory must be normalized inside /workspace",
        ));
    }
    Ok(normalized)
}

pub(crate) fn make_job_request(
    worker_id: jobworkerp_client::jobworkerp::data::WorkerId,
    args: Vec<u8>,
    using: Option<&str>,
) -> JobRequest {
    JobRequest {
        worker: Some(job_request::Worker::WorkerId(worker_id)),
        args,
        using: using.map(str::to_owned),
        ..Default::default()
    }
}

fn job_id_from_metadata(metadata: &tonic::metadata::MetadataMap) -> AppResult<JobId> {
    let encoded = metadata
        .get_bin(JOB_ID_HEADER_NAME)
        .ok_or_else(|| transport_error("enqueue response omitted the server-assigned job id"))?
        .to_bytes()
        .map_err(|_| transport_error("enqueue response contained an invalid job id header"))?;
    let job_id = JobId::decode(encoded)
        .map_err(|_| transport_error("enqueue response contained an invalid job id header"))?;
    if job_id.value <= 0 {
        return Err(transport_error(
            "enqueue response contained an invalid job id",
        ));
    }
    Ok(job_id)
}

fn transport_error(message: impl Into<String>) -> AppError {
    AppError::Jobworkerp(format!("sandbox transport: {}", message.into()))
}

fn safe_rpc_error(action: &str, status: tonic::Status) -> AppError {
    transport_error(format!("{action} failed with gRPC {:?}", status.code()))
}

fn safe_stream_status_error(status: tonic::Status) -> AppError {
    safe_rpc_error("sandbox result stream", status)
}

/// Classify the failure an End reports. Only the fixed classification is
/// surfaced: the message and problem may echo command output or secrets.
fn check_sandbox_stream_error(trailer: &Trailer) -> AppResult<()> {
    let Some(error) = stream_error_from_trailer(trailer) else {
        return Ok(());
    };
    let classification = match (
        error.origin.as_str(),
        StreamErrorCode::try_from(error.code).ok(),
    ) {
        (SANDBOX_RUNNER_NAME, Some(StreamErrorCode::ExecutionFailed)) => "sandbox execution failed",
        // Under the WORKER origin jobworkerp reports a job timeout, or a cancel
        // that arrived before the stream started; both still stop the command.
        (SANDBOX_RUNNER_NAME | WORKER_STREAM_ORIGIN, Some(StreamErrorCode::Timeout)) => {
            "sandbox command timed out"
        }
        (SANDBOX_RUNNER_NAME | WORKER_STREAM_ORIGIN, Some(StreamErrorCode::Cancelled)) => {
            "sandbox command was cancelled"
        }
        _ => return Err(unrecognized_stream_error()),
    };
    Err(transport_error(classification))
}

fn unrecognized_stream_error() -> AppError {
    transport_error("sandbox reported an unrecognized stream error")
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn profile_settings_shell_prefers_bash_and_falls_back_to_sh() {
        use std::os::unix::fs::PermissionsExt;
        let run = |with_bash: bool| {
            let bin = tempfile::tempdir().unwrap();
            let mut shells = vec!["sh"];
            if with_bash {
                shells.push("bash");
            }
            for name in shells {
                let path = bin.path().join(name);
                std::fs::write(&path, format!("#!/bin/sh\necho {name} \"$@\"\n")).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", super::PROFILE_SETTINGS_SHELL_SCRIPT])
                .env_clear()
                .env("PATH", bin.path())
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap()
        };
        assert_eq!(run(true), "bash -i\n");
        assert_eq!(run(false), "sh -i\n");
    }

    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn bounded_stdin_sender_waits_for_queue_capacity() {
        let (sender, mut receiver) = mpsc::channel(1);
        let input = CodingAgentInput {
            sender: Arc::new(Mutex::new(Some(sender))),
            max_chunk_bytes: 8,
        };
        input.send_input(vec![1]).await.unwrap();

        let blocked_send = input.send_input(vec![2]);
        tokio::pin!(blocked_send);
        tokio::select! {
            biased;
            result = &mut blocked_send => panic!("full queue unexpectedly accepted data: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }

        assert_eq!(receiver.recv().await.unwrap().data, vec![1]);
        tokio::time::timeout(Duration::from_secs(1), blocked_send)
            .await
            .expect("sender should resume when queue capacity is released")
            .unwrap();
        assert_eq!(receiver.recv().await.unwrap().data, vec![2]);
    }

    #[tokio::test]
    async fn cloned_input_writers_order_data_before_one_final_frame() {
        let (sender, mut receiver) = mpsc::channel(1);
        let input = CodingAgentInput {
            sender: Arc::new(Mutex::new(Some(sender))),
            max_chunk_bytes: 8,
        };
        input.send_input(vec![1]).await.unwrap();

        let second_input = input.clone();
        let second_writer = tokio::spawn(async move { second_input.send_input(vec![2]).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if input.sender.try_lock().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the second writer should hold the ordering lock while backpressured");

        let final_input = input.clone();
        let final_writer = tokio::spawn(async move { final_input.finish_input().await });
        assert_eq!(receiver.recv().await.unwrap().data, vec![1]);
        assert_eq!(receiver.recv().await.unwrap().data, vec![2]);
        let final_frame = receiver.recv().await.unwrap();
        assert!(final_frame.is_final);
        assert!(final_frame.data.is_empty());
        second_writer.await.unwrap().unwrap();
        final_writer.await.unwrap().unwrap();
        assert!(input.send_input(vec![3]).await.is_err());
        assert!(input.finish_input().await.is_err());
    }
}
