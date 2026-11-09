use std::future::Future;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::State;
use tonic::transport::Channel;

use crate::error::{AppError, AppResult};
use crate::grpc::proto::llm_memory::service::{
    embedding_maintenance_service_client::EmbeddingMaintenanceServiceClient,
    embedding_space_service_client::EmbeddingSpaceServiceClient,
};
use crate::grpc::proto::llm_memory::{data as mem_data, service as mem_svc};

use super::AppState;
use super::connection::{ConnectionMode, ResolvedConnection};

const REGISTRATION_REGISTERED: i32 = 1;
const REGISTRATION_PENDING: i32 = 2;
const REGISTRATION_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
const REGISTRATION_POLL_INTERVAL: Duration = Duration::from_millis(500);
const REGISTRATION_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const EMBEDDING_RPC_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECTION_CHANGE_CHECK_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy)]
struct WorkerWaitPolicy {
    poll_interval: Duration,
    timeout: Duration,
}

impl WorkerWaitPolicy {
    const fn standard() -> Self {
        Self {
            poll_interval: REGISTRATION_POLL_INTERVAL,
            timeout: REGISTRATION_WAIT_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRegistrationState {
    Registered,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EmbeddingWorkerRegistration {
    worker_name: String,
    registration_state: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingConnectionSnapshot {
    pub(super) generation: u64,
    /// Both services: memories names the embedding worker and jobworkerp
    /// runs it, so a change of either invalidates an in-flight embedding.
    pub(super) resolved: ResolvedConnection,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EmbeddingSpaceValue {
    pub space_id: String,
    pub model_id: String,
    pub tokenizer_model_id: String,
    pub revision: String,
    pub dimension: u32,
    pub distance: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EmbeddingMigrationMarkerView {
    pub state: String,
    pub attempt_id: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EmbeddingSpaceRecordView {
    pub space: Option<EmbeddingSpaceValue>,
    pub legacy_accept: bool,
    pub migration_marker: Option<EmbeddingMigrationMarkerView>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EmbeddingTableRecordView {
    pub table: String,
    pub record: Option<EmbeddingSpaceRecordView>,
}

/// Read-only description of the embedding space served by the selected Memories endpoint.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EmbeddingSpaceView {
    pub current_space: Option<EmbeddingSpaceValue>,
    pub mm_embedding_worker_name: String,
    pub registration_state: WorkerRegistrationState,
    pub tables: Vec<EmbeddingTableRecordView>,
    pub rpc_migration_supported: bool,
    pub write_rejections: std::collections::HashMap<String, u64>,
}

fn registration_state(value: i32) -> AppResult<WorkerRegistrationState> {
    match value {
        REGISTRATION_REGISTERED => Ok(WorkerRegistrationState::Registered),
        REGISTRATION_PENDING => Ok(WorkerRegistrationState::Pending),
        0 => Err(AppError::Config(
            "Memories returned an unspecified embedding worker registration state".into(),
        )),
        other => Err(AppError::Config(format!(
            "Memories returned an unknown embedding worker registration state: {other}"
        ))),
    }
}

fn worker_registration_from_response(
    response: &mem_svc::GetEmbeddingSpaceResponse,
) -> AppResult<EmbeddingWorkerRegistration> {
    registration_state(response.registration_state)?;
    if response.mm_embedding_worker_name.trim().is_empty() {
        return Err(AppError::Config(
            "Memories returned an empty embedding worker name".into(),
        ));
    }
    Ok(EmbeddingWorkerRegistration {
        worker_name: response.mm_embedding_worker_name.clone(),
        registration_state: response.registration_state,
    })
}

fn space_value(space: mem_data::EmbeddingSpace) -> EmbeddingSpaceValue {
    EmbeddingSpaceValue {
        space_id: space.space_id,
        model_id: space.model_id,
        tokenizer_model_id: space.tokenizer_model_id,
        revision: space.revision,
        dimension: space.dimension,
        distance: space.distance,
    }
}

fn space_record(record: mem_data::EmbeddingSpaceRecord) -> EmbeddingSpaceRecordView {
    EmbeddingSpaceRecordView {
        space: record.space.map(space_value),
        legacy_accept: record.legacy_accept,
        migration_marker: record
            .migration_marker
            .map(|marker| EmbeddingMigrationMarkerView {
                state: marker.state,
                attempt_id: marker.attempt_id,
            }),
    }
}

fn embedding_space_dto(
    response: mem_svc::GetEmbeddingSpaceResponse,
) -> AppResult<EmbeddingSpaceView> {
    let registration_state = registration_state(response.registration_state)?;
    if response.mm_embedding_worker_name.trim().is_empty() {
        return Err(AppError::Config(
            "Memories returned an empty embedding worker name".into(),
        ));
    }
    Ok(EmbeddingSpaceView {
        current_space: response.current_space.map(space_value),
        mm_embedding_worker_name: response.mm_embedding_worker_name,
        registration_state,
        tables: response
            .tables
            .into_iter()
            .map(|table| EmbeddingTableRecordView {
                table: table.table,
                record: table.record.map(space_record),
            })
            .collect(),
        rpc_migration_supported: response.rpc_migration_supported,
        write_rejections: response.write_rejections,
    })
}

#[tauri::command]
pub async fn get_embedding_space(state: State<'_, AppState>) -> AppResult<EmbeddingSpaceView> {
    let snapshot = state.embedding_connection_snapshot()?;
    let channel = await_embedding_context(
        &state,
        &snapshot,
        EMBEDDING_RPC_TIMEOUT,
        state.memories_channel(),
        "embedding space connection",
    )
    .await?;
    let mut client = EmbeddingSpaceServiceClient::new(channel);
    let response = await_embedding_context(
        &state,
        &snapshot,
        EMBEDDING_RPC_TIMEOUT,
        async {
            client
                .get_embedding_space(mem_svc::GetEmbeddingSpaceRequest {})
                .await
                .map(|response| response.into_inner())
                .map_err(AppError::from)
        },
        "embedding space lookup",
    )
    .await?;
    embedding_space_dto(response)
}

#[async_trait::async_trait]
#[allow(clippy::double_must_use)]
trait EmbeddingWorkerOperation {
    type Output: Send;

    async fn lookup(&mut self, timeout: Duration) -> AppResult<EmbeddingWorkerRegistration>;
    async fn dispatch(&mut self, worker_name: &str) -> AppResult<Self::Output>;
    fn ensure_current(&self) -> AppResult<()>;
    async fn wait(&mut self, duration: Duration) -> AppResult<()>;
}

pub(super) fn embedding_connection_changed() -> AppError {
    AppError::SidecarNotReady(
        "embedding operation was cancelled because the connection changed".into(),
    )
}

fn registration_timeout() -> AppError {
    AppError::SidecarNotReady("timed out waiting for the embedding worker to be registered".into())
}

fn validated_worker_name(registration: &EmbeddingWorkerRegistration) -> AppResult<&str> {
    if registration.worker_name.trim().is_empty() {
        Err(AppError::Config(
            "Memories returned an empty embedding worker name".into(),
        ))
    } else {
        Ok(&registration.worker_name)
    }
}

async fn wait_for_registered_worker<O: EmbeddingWorkerOperation>(
    operation: &mut O,
    deadline: tokio::time::Instant,
    poll_interval: Duration,
) -> AppResult<String> {
    loop {
        operation.ensure_current()?;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(registration_timeout());
        }
        let lookup_timeout = remaining.min(REGISTRATION_LOOKUP_TIMEOUT);
        let registration = tokio::time::timeout(lookup_timeout, operation.lookup(lookup_timeout))
            .await
            .map_err(|_| AppError::SidecarNotReady("embedding space lookup timed out".into()))??;
        operation.ensure_current()?;
        match registration_state(registration.registration_state)? {
            WorkerRegistrationState::Registered => {
                return Ok(validated_worker_name(&registration)?.to_string());
            }
            WorkerRegistrationState::Pending => {
                validated_worker_name(&registration)?;
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(registration_timeout());
                }
                operation.wait(remaining.min(poll_interval)).await?;
                operation.ensure_current()?;
            }
        }
    }
}

fn is_worker_not_found(error: &AppError, worker_name: &str) -> bool {
    let AppError::Jobworkerp(message) = error else {
        return false;
    };
    if message.strip_prefix("worker not found: ") == Some(worker_name) {
        return true;
    }
    let names_expected_worker = ["load_worker ", "find worker "].into_iter().any(|prefix| {
        message
            .strip_prefix(prefix)
            .and_then(|remaining| remaining.strip_prefix(worker_name))
            .is_some_and(|remaining| remaining.starts_with(':'))
    });
    if !names_expected_worker {
        return false;
    }
    let lowercase = message.to_ascii_lowercase();
    lowercase.contains("not found") || lowercase.contains("notfound")
}

async fn run_registered_embedding_operation<O: EmbeddingWorkerOperation>(
    operation: &mut O,
    policy: WorkerWaitPolicy,
) -> AppResult<O::Output> {
    let deadline = tokio::time::Instant::now() + policy.timeout;
    let worker_name = wait_for_registered_worker(operation, deadline, policy.poll_interval).await?;
    operation.ensure_current()?;
    match operation.dispatch(&worker_name).await {
        Ok(output) => {
            operation.ensure_current()?;
            Ok(output)
        }
        Err(error) if is_worker_not_found(&error, &worker_name) => {
            operation.ensure_current()?;
            let refreshed_name =
                wait_for_registered_worker(operation, deadline, policy.poll_interval).await?;
            operation.ensure_current()?;
            let result = operation.dispatch(&refreshed_name).await;
            operation.ensure_current()?;
            result
        }
        Err(error) => {
            operation.ensure_current()?;
            Err(error)
        }
    }
}

struct AppEmbeddingWorkerContext<'a> {
    state: &'a AppState,
    snapshot: EmbeddingConnectionSnapshot,
}

impl AppEmbeddingWorkerContext<'_> {
    fn ensure_current(&self) -> AppResult<()> {
        self.state
            .ensure_embedding_connection_current(&self.snapshot)
    }

    async fn lookup(&self, timeout: Duration) -> AppResult<EmbeddingWorkerRegistration> {
        self.ensure_current()?;
        let channel = await_embedding_context(
            self.state,
            &self.snapshot,
            timeout,
            self.state.memories_channel(),
            "embedding space connection",
        )
        .await?;
        let mut client = EmbeddingSpaceServiceClient::new(channel);
        let response = await_embedding_context(
            self.state,
            &self.snapshot,
            timeout,
            async {
                client
                    .get_embedding_space(mem_svc::GetEmbeddingSpaceRequest {})
                    .await
                    .map(|response| response.into_inner())
                    .map_err(AppError::from)
            },
            "embedding space lookup",
        )
        .await?;
        worker_registration_from_response(&response)
    }

    async fn wait(&self, duration: Duration) -> AppResult<()> {
        self.ensure_current()?;
        tokio::select! {
            biased;
            _ = wait_for_embedding_context_change(self.state, &self.snapshot) => {
                Err(embedding_connection_changed())
            }
            _ = tokio::time::sleep(duration) => {
                self.ensure_current()
            }
        }
    }

    async fn current_jobworkerp(
        &self,
        timeout: Duration,
    ) -> AppResult<crate::jobworkerp::JobworkerpHandle> {
        await_embedding_context(
            self.state,
            &self.snapshot,
            timeout,
            self.state.jobworkerp(),
            "jobworkerp connection",
        )
        .await
    }
}

struct QueryEmbeddingOperation<'a> {
    context: AppEmbeddingWorkerContext<'a>,
    text: String,
    query_prefix: Option<String>,
}

#[async_trait::async_trait]
impl EmbeddingWorkerOperation for QueryEmbeddingOperation<'_> {
    type Output = Vec<f32>;

    async fn lookup(&mut self, timeout: Duration) -> AppResult<EmbeddingWorkerRegistration> {
        self.context.lookup(timeout).await
    }

    async fn dispatch(&mut self, worker_name: &str) -> AppResult<Self::Output> {
        self.context.ensure_current()?;
        let handle = self
            .context
            .current_jobworkerp(EMBEDDING_RPC_TIMEOUT)
            .await?;
        self.context.ensure_current()?;
        let text = self.text.clone();
        let prefix = self.query_prefix.clone();
        let worker_name = worker_name.to_string();
        await_embedding_context(
            self.context.state,
            &self.context.snapshot,
            EMBEDDING_RPC_TIMEOUT,
            async move {
                crate::jobworkerp::embedding::embed_query(
                    &handle,
                    &worker_name,
                    &text,
                    prefix.as_deref(),
                )
                .await
            },
            "query embedding dispatch",
        )
        .await
    }

    fn ensure_current(&self) -> AppResult<()> {
        self.context.ensure_current()
    }

    async fn wait(&mut self, duration: Duration) -> AppResult<()> {
        self.context.wait(duration).await
    }
}

fn effective_query_prefix(state: &AppState) -> Option<String> {
    let settings =
        super::embedding_settings::load_embedding_settings(&state.data.embedding_settings_path());
    super::embedding_settings::resolve_embedding_runtime_with_env(&settings, |key| {
        state.sidecars.embedding_env_lookup(key)
    })
    .query_prefix
}

/// Resolve the current worker, then keep the vector search on the same
/// connection generation so a reconnect cannot pair an old vector with a new
/// Memories endpoint. Remote mode uses the read-only lookup and configured
/// remote jobworkerp target but never invokes management operations.
pub async fn with_query_embedding<T, F, Fut>(
    state: &AppState,
    text: &str,
    search: F,
) -> AppResult<T>
where
    F: FnOnce(Vec<f32>) -> Fut,
    Fut: Future<Output = AppResult<T>>,
{
    crate::jobworkerp::embedding::validate_query_text(text)?;
    let snapshot = state.embedding_connection_snapshot()?;
    let mut operation = QueryEmbeddingOperation {
        context: AppEmbeddingWorkerContext { state, snapshot },
        text: text.trim().to_string(),
        query_prefix: effective_query_prefix(state),
    };
    let vector =
        run_registered_embedding_operation(&mut operation, WorkerWaitPolicy::standard()).await?;
    await_embedding_context(
        state,
        &operation.context.snapshot,
        EMBEDDING_RPC_TIMEOUT,
        search(vector),
        "embedding search",
    )
    .await
}

struct LoadEmbeddingWorkerOperation<'a> {
    context: AppEmbeddingWorkerContext<'a>,
    timeout_ms: Option<u64>,
}

#[async_trait::async_trait]
impl EmbeddingWorkerOperation for LoadEmbeddingWorkerOperation<'_> {
    type Output = ();

    async fn lookup(&mut self, timeout: Duration) -> AppResult<EmbeddingWorkerRegistration> {
        self.context.lookup(timeout).await
    }

    async fn dispatch(&mut self, worker_name: &str) -> AppResult<Self::Output> {
        self.context.ensure_current()?;
        let handle = self
            .context
            .current_jobworkerp(EMBEDDING_RPC_TIMEOUT)
            .await?;
        self.context.ensure_current()?;
        let worker_name = worker_name.to_string();
        let timeout_ms = self
            .timeout_ms
            .unwrap_or_else(|| EMBEDDING_RPC_TIMEOUT.as_secs().saturating_mul(1000));
        let timeout = Duration::from_millis(timeout_ms);
        await_embedding_context(
            self.context.state,
            &self.context.snapshot,
            timeout,
            async move { handle.load_worker(&worker_name, Some(timeout_ms)).await },
            "embedding model load",
        )
        .await
    }

    fn ensure_current(&self) -> AppResult<()> {
        self.context.ensure_current()
    }

    async fn wait(&mut self, duration: Duration) -> AppResult<()> {
        self.context.wait(duration).await
    }
}

pub(crate) async fn load_embedding_worker(
    state: &AppState,
    timeout_ms: Option<u64>,
) -> AppResult<()> {
    let snapshot = state.embedding_connection_snapshot()?;
    let mut operation = LoadEmbeddingWorkerOperation {
        context: AppEmbeddingWorkerContext { state, snapshot },
        timeout_ms,
    };
    run_registered_embedding_operation(&mut operation, WorkerWaitPolicy::standard())
        .await
        .map(|_| ())
}

async fn wait_for_embedding_context_change(
    state: &AppState,
    snapshot: &EmbeddingConnectionSnapshot,
) {
    loop {
        if state.ensure_embedding_connection_current(snapshot).is_err() {
            return;
        }
        tokio::time::sleep(CONNECTION_CHANGE_CHECK_INTERVAL).await;
    }
}

async fn await_embedding_context<T, F>(
    state: &AppState,
    snapshot: &EmbeddingConnectionSnapshot,
    timeout: Duration,
    future: F,
    action: &str,
) -> AppResult<T>
where
    F: Future<Output = AppResult<T>>,
{
    state.ensure_embedding_connection_current(snapshot)?;
    let result = tokio::select! {
        biased;
        _ = wait_for_embedding_context_change(state, snapshot) => Err(embedding_connection_changed()),
        result = tokio::time::timeout(timeout, future) => match result {
            Ok(result) => result,
            Err(_) => Err(AppError::SidecarNotReady(format!("{action} timed out"))),
        },
    }?;
    state.ensure_embedding_connection_current(snapshot)?;
    Ok(result)
}

fn management_allowed(mode: ConnectionMode) -> AppResult<()> {
    if mode == ConnectionMode::Local {
        Ok(())
    } else {
        Err(AppError::Config(
            "embedding migration management is available only for the local Memories sidecar; remote embedding state is read-only".into(),
        ))
    }
}

async fn local_maintenance_client(
    state: &AppState,
) -> AppResult<(
    EmbeddingConnectionSnapshot,
    EmbeddingMaintenanceServiceClient<Channel>,
)> {
    let snapshot = state.embedding_connection_snapshot()?;
    management_allowed(snapshot.resolved.memories_mode)?;
    let endpoints = state
        .sidecars
        .current_endpoints()
        .ok_or_else(|| AppError::SidecarNotReady("local Memories sidecar is not started".into()))?;
    let url = endpoints
        .memories_url()
        .ok_or_else(|| AppError::SidecarNotReady("local Memories sidecar is not started".into()))?;
    if url != snapshot.resolved.targets.memories_url {
        return Err(embedding_connection_changed());
    }
    let channel = await_embedding_context(
        state,
        &snapshot,
        EMBEDDING_RPC_TIMEOUT,
        crate::grpc::connect(&url),
        "local embedding maintenance connection",
    )
    .await?;
    Ok((snapshot, EmbeddingMaintenanceServiceClient::new(channel)))
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct StartEmbeddingRebuildRequest {
    pub attempt_id: String,
    #[serde(default)]
    pub retry_failed: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct StartEmbeddingReconcileRequest {
    #[serde(default)]
    pub retry_failed: bool,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub force_table: String,
    #[serde(default)]
    pub force_entity_ids: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StartEmbeddingRebuildResult {
    pub attempt_id: String,
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StartEmbeddingReconcileResult {
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EmbeddingRebuildKindProgressView {
    pub kind: String,
    pub targets: u64,
    pub complete: u64,
    pub missing: u64,
    pub stale: u64,
    pub unverified: u64,
    pub failed_transient: u64,
    pub failed_permanent: u64,
    pub dispatched: u64,
    pub failed_to_dispatch: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EmbeddingRebuildProgressView {
    pub attempt_id: String,
    pub status: String,
    pub kinds: Vec<EmbeddingRebuildKindProgressView>,
    pub orphan: u64,
    pub caption_missing: u64,
    pub captions_requested: u64,
    pub completion_expected: bool,
    pub failed: u64,
    pub write_rejections: std::collections::HashMap<String, u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EmbeddingReconcileKindProgressView {
    pub kind: String,
    pub missing: u64,
    pub stale: u64,
    pub unverified: u64,
    pub failed: u64,
    pub orphan: u64,
    pub dispatched: u64,
    pub complete: u64,
    pub failed_to_dispatch: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EmbeddingReconcileProgressView {
    pub task_id: String,
    pub status: String,
    pub kinds: Vec<EmbeddingReconcileKindProgressView>,
    pub captions_requested: u64,
    pub error: Option<String>,
}

fn rebuild_progress_dto(
    response: mem_svc::GetEmbeddingRebuildProgressResponse,
) -> EmbeddingRebuildProgressView {
    EmbeddingRebuildProgressView {
        attempt_id: response.attempt_id,
        status: response.status,
        kinds: response
            .kinds
            .into_iter()
            .map(|kind| EmbeddingRebuildKindProgressView {
                kind: kind.kind,
                targets: kind.targets,
                complete: kind.complete,
                missing: kind.missing,
                stale: kind.stale,
                unverified: kind.unverified,
                failed_transient: kind.failed_transient,
                failed_permanent: kind.failed_permanent,
                dispatched: kind.dispatched,
                failed_to_dispatch: kind.failed_to_dispatch,
            })
            .collect(),
        orphan: response.orphan,
        caption_missing: response.caption_missing,
        captions_requested: response.captions_requested,
        completion_expected: response.completion_expected,
        failed: response.failed,
        write_rejections: response.write_rejections,
        error: response.error,
    }
}

fn reconcile_progress_dto(
    response: mem_svc::GetEmbeddingReconcileProgressResponse,
) -> EmbeddingReconcileProgressView {
    EmbeddingReconcileProgressView {
        task_id: response.task_id,
        status: response.status,
        kinds: response
            .kinds
            .into_iter()
            .map(|kind| EmbeddingReconcileKindProgressView {
                kind: kind.kind,
                missing: kind.missing,
                stale: kind.stale,
                unverified: kind.unverified,
                failed: kind.failed,
                orphan: kind.orphan,
                dispatched: kind.dispatched,
                complete: kind.complete,
                failed_to_dispatch: kind.failed_to_dispatch,
            })
            .collect(),
        captions_requested: response.captions_requested,
        error: response.error,
    }
}

pub(super) async fn with_embedding_maintenance_admission<T, F, Fut>(
    state: &AppState,
    operation: F,
) -> AppResult<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = AppResult<T>>,
{
    let _recovery = state.database_migration_recovery.lock().await;
    let _dispatch_gate = state.lock_llm_settings_dispatch_gate().await;
    state.ensure_local_embedding_available()?;
    operation().await
}

pub(crate) async fn start_embedding_rebuild_unlocked(
    state: &AppState,
    req: StartEmbeddingRebuildRequest,
) -> AppResult<StartEmbeddingRebuildResult> {
    if req.attempt_id.trim().is_empty() {
        return Err(AppError::Config(
            "embedding rebuild attempt_id must not be empty".into(),
        ));
    }
    let (snapshot, mut client) = local_maintenance_client(state).await?;
    let response = await_embedding_context(
        state,
        &snapshot,
        EMBEDDING_RPC_TIMEOUT,
        async {
            client
                .start_rebuild(mem_svc::StartEmbeddingRebuildRequest {
                    attempt_id: req.attempt_id,
                    retry_failed: req.retry_failed,
                })
                .await
                .map(|response| response.into_inner())
                .map_err(AppError::from)
        },
        "embedding rebuild start",
    )
    .await?;
    Ok(StartEmbeddingRebuildResult {
        attempt_id: response.attempt_id,
        task_id: response.task_id,
    })
}

pub(crate) async fn start_embedding_reconcile_unlocked(
    state: &AppState,
    req: StartEmbeddingReconcileRequest,
) -> AppResult<StartEmbeddingReconcileResult> {
    let (snapshot, mut client) = local_maintenance_client(state).await?;
    let response = await_embedding_context(
        state,
        &snapshot,
        EMBEDDING_RPC_TIMEOUT,
        async {
            client
                .start_reconcile(mem_svc::StartEmbeddingReconcileRequest {
                    retry_failed: req.retry_failed,
                    force: req.force,
                    force_table: req.force_table,
                    force_entity_ids: req.force_entity_ids,
                })
                .await
                .map(|response| response.into_inner())
                .map_err(AppError::from)
        },
        "embedding reconcile start",
    )
    .await?;
    Ok(StartEmbeddingReconcileResult {
        task_id: response.task_id,
    })
}

#[tauri::command]
pub async fn start_embedding_rebuild(
    state: State<'_, AppState>,
    req: StartEmbeddingRebuildRequest,
) -> AppResult<StartEmbeddingRebuildResult> {
    with_embedding_maintenance_admission(&state, || start_embedding_rebuild_unlocked(&state, req))
        .await
}

#[tauri::command]
pub async fn get_embedding_rebuild_progress(
    state: State<'_, AppState>,
    attempt_id: String,
) -> AppResult<EmbeddingRebuildProgressView> {
    if attempt_id.trim().is_empty() {
        return Err(AppError::Config(
            "embedding rebuild attempt_id must not be empty".into(),
        ));
    }
    let (snapshot, mut client) = local_maintenance_client(&state).await?;
    let response = await_embedding_context(
        &state,
        &snapshot,
        EMBEDDING_RPC_TIMEOUT,
        async {
            client
                .get_rebuild_progress(mem_svc::GetEmbeddingRebuildProgressRequest { attempt_id })
                .await
                .map(|response| response.into_inner())
                .map_err(AppError::from)
        },
        "embedding rebuild progress",
    )
    .await?;
    Ok(rebuild_progress_dto(response))
}

#[tauri::command]
pub async fn start_embedding_reconcile(
    state: State<'_, AppState>,
    req: StartEmbeddingReconcileRequest,
) -> AppResult<StartEmbeddingReconcileResult> {
    with_embedding_maintenance_admission(&state, || start_embedding_reconcile_unlocked(&state, req))
        .await
}

#[tauri::command]
pub async fn get_embedding_reconcile_progress(
    state: State<'_, AppState>,
    task_id: String,
) -> AppResult<EmbeddingReconcileProgressView> {
    if task_id.trim().is_empty() {
        return Err(AppError::Config(
            "embedding reconcile task_id must not be empty".into(),
        ));
    }
    let (snapshot, mut client) = local_maintenance_client(&state).await?;
    let response = await_embedding_context(
        &state,
        &snapshot,
        EMBEDDING_RPC_TIMEOUT,
        async {
            client
                .get_reconcile_progress(mem_svc::GetEmbeddingReconcileProgressRequest { task_id })
                .await
                .map(|response| response.into_inner())
                .map_err(AppError::from)
        },
        "embedding reconcile progress",
    )
    .await?;
    Ok(reconcile_progress_dto(response))
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    use super::*;

    struct FakeWorkerOperation {
        lookups: VecDeque<AppResult<EmbeddingWorkerRegistration>>,
        dispatched_to: Vec<String>,
        current: Arc<AtomicBool>,
        cancel_during_wait: bool,
        dispatch_error: Option<String>,
    }

    impl FakeWorkerOperation {
        fn new(lookups: impl IntoIterator<Item = AppResult<EmbeddingWorkerRegistration>>) -> Self {
            Self {
                lookups: lookups.into_iter().collect(),
                dispatched_to: Vec::new(),
                current: Arc::new(AtomicBool::new(true)),
                cancel_during_wait: false,
                dispatch_error: None,
            }
        }

        fn registration(name: &str, state: i32) -> AppResult<EmbeddingWorkerRegistration> {
            Ok(EmbeddingWorkerRegistration {
                worker_name: name.to_string(),
                registration_state: state,
            })
        }
    }

    #[async_trait::async_trait]
    impl EmbeddingWorkerOperation for FakeWorkerOperation {
        type Output = String;

        async fn lookup(&mut self, _timeout: Duration) -> AppResult<EmbeddingWorkerRegistration> {
            self.lookups
                .pop_front()
                .unwrap_or_else(|| Err(AppError::Config("unexpected fake lookup".into())))
        }

        async fn dispatch(&mut self, worker_name: &str) -> AppResult<Self::Output> {
            self.dispatched_to.push(worker_name.to_string());
            match self.dispatch_error.as_deref() {
                Some(message) => Err(AppError::Jobworkerp(message.to_string())),
                None => Ok(worker_name.to_string()),
            }
        }

        fn ensure_current(&self) -> AppResult<()> {
            self.current
                .load(Ordering::Acquire)
                .then_some(())
                .ok_or_else(embedding_connection_changed)
        }

        async fn wait(&mut self, duration: Duration) -> AppResult<()> {
            tokio::time::sleep(duration).await;
            if self.cancel_during_wait {
                self.current.store(false, Ordering::Release);
            }
            Ok(())
        }
    }

    fn quick_policy() -> WorkerWaitPolicy {
        WorkerWaitPolicy {
            poll_interval: Duration::from_millis(1),
            timeout: Duration::from_millis(20),
        }
    }

    #[tokio::test]
    async fn pending_registration_is_rechecked_before_dispatch() {
        let mut operation = FakeWorkerOperation::new([
            FakeWorkerOperation::registration("pending-name", REGISTRATION_PENDING),
            FakeWorkerOperation::registration("current-name", REGISTRATION_REGISTERED),
        ]);

        let dispatched = run_registered_embedding_operation(&mut operation, quick_policy())
            .await
            .expect("registered worker should be dispatched");

        assert_eq!(dispatched, "current-name");
        assert_eq!(operation.dispatched_to, ["current-name"]);
    }

    #[tokio::test]
    async fn pending_registration_times_out_without_dispatch() {
        let mut operation = FakeWorkerOperation::new([FakeWorkerOperation::registration(
            "still-pending",
            REGISTRATION_PENDING,
        )]);
        let policy = WorkerWaitPolicy {
            poll_interval: Duration::from_millis(2),
            timeout: Duration::from_millis(3),
        };

        let error = run_registered_embedding_operation(&mut operation, policy)
            .await
            .expect_err("pending registration is bounded");

        assert!(error.to_string().contains("timed out"));
        assert!(operation.dispatched_to.is_empty());
    }

    #[tokio::test]
    async fn unspecified_and_unknown_registration_states_are_rejected() {
        for state in [0, 9] {
            let mut operation = FakeWorkerOperation::new([FakeWorkerOperation::registration(
                "not-registered",
                state,
            )]);

            let error = run_registered_embedding_operation(&mut operation, quick_policy())
                .await
                .expect_err("only explicit registered or pending states are valid");

            assert!(matches!(error, AppError::Config(_)));
            assert!(operation.dispatched_to.is_empty());
        }
    }

    #[tokio::test]
    async fn non_not_found_dispatch_errors_are_not_retried() {
        let mut operation = FakeWorkerOperation::new([FakeWorkerOperation::registration(
            "registered-worker",
            REGISTRATION_REGISTERED,
        )]);
        operation.dispatch_error = Some("temporary transport failure".into());

        let error = run_registered_embedding_operation(&mut operation, quick_policy())
            .await
            .expect_err("non-not-found errors are returned immediately");

        assert!(error.to_string().contains("temporary transport failure"));
        assert_eq!(operation.dispatched_to, ["registered-worker"]);
        assert!(operation.lookups.is_empty());
    }

    struct NotFoundOnceOperation {
        lookups: VecDeque<AppResult<EmbeddingWorkerRegistration>>,
        dispatched_to: Vec<String>,
    }

    #[async_trait::async_trait]
    impl EmbeddingWorkerOperation for NotFoundOnceOperation {
        type Output = String;

        async fn lookup(&mut self, _timeout: Duration) -> AppResult<EmbeddingWorkerRegistration> {
            self.lookups
                .pop_front()
                .unwrap_or_else(|| Err(AppError::Config("unexpected fake lookup".into())))
        }

        async fn dispatch(&mut self, worker_name: &str) -> AppResult<Self::Output> {
            self.dispatched_to.push(worker_name.to_string());
            if worker_name == "renamed-worker" {
                Ok(worker_name.to_string())
            } else {
                Err(AppError::Jobworkerp(format!(
                    "worker not found: {worker_name}"
                )))
            }
        }

        fn ensure_current(&self) -> AppResult<()> {
            Ok(())
        }

        async fn wait(&mut self, _duration: Duration) -> AppResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn missing_registered_worker_is_looked_up_again_and_renamed_worker_retried_once() {
        let mut operation = NotFoundOnceOperation {
            lookups: [
                FakeWorkerOperation::registration("stale-worker", REGISTRATION_REGISTERED),
                FakeWorkerOperation::registration("renamed-worker", REGISTRATION_REGISTERED),
            ]
            .into(),
            dispatched_to: Vec::new(),
        };

        let dispatched = run_registered_embedding_operation(&mut operation, quick_policy())
            .await
            .expect("one rename retry should succeed");

        assert_eq!(dispatched, "renamed-worker");
        assert_eq!(operation.dispatched_to, ["stale-worker", "renamed-worker"]);
    }

    #[tokio::test]
    async fn missing_worker_is_not_retried_more_than_once() {
        let mut operation = FakeWorkerOperation::new([
            FakeWorkerOperation::registration("first-worker", REGISTRATION_REGISTERED),
            FakeWorkerOperation::registration("second-worker", REGISTRATION_REGISTERED),
        ]);
        operation.dispatched_to.clear();
        let mut always_missing = AlwaysMissingDispatch { inner: operation };

        let error = run_registered_embedding_operation(&mut always_missing, quick_policy())
            .await
            .expect_err("a second missing worker must be returned to the caller");

        assert!(error.to_string().contains("worker not found"));
        assert_eq!(always_missing.inner.dispatched_to.len(), 2);
    }

    struct AlwaysMissingDispatch {
        inner: FakeWorkerOperation,
    }

    #[async_trait::async_trait]
    impl EmbeddingWorkerOperation for AlwaysMissingDispatch {
        type Output = String;

        async fn lookup(&mut self, timeout: Duration) -> AppResult<EmbeddingWorkerRegistration> {
            self.inner.lookup(timeout).await
        }

        async fn dispatch(&mut self, worker_name: &str) -> AppResult<Self::Output> {
            self.inner.dispatched_to.push(worker_name.to_string());
            Err(AppError::Jobworkerp(format!(
                "worker not found: {worker_name}"
            )))
        }

        fn ensure_current(&self) -> AppResult<()> {
            self.inner.ensure_current()
        }

        async fn wait(&mut self, duration: Duration) -> AppResult<()> {
            self.inner.wait(duration).await
        }
    }

    #[tokio::test]
    async fn connection_epoch_change_during_pending_wait_cancels_without_dispatch() {
        let mut operation = FakeWorkerOperation::new([FakeWorkerOperation::registration(
            "pending-worker",
            REGISTRATION_PENDING,
        )]);
        operation.cancel_during_wait = true;

        let error = run_registered_embedding_operation(&mut operation, quick_policy())
            .await
            .expect_err("connection changes cancel the pending wait");

        assert!(error.to_string().contains("connection changed"));
        assert!(operation.dispatched_to.is_empty());
    }

    #[test]
    fn remote_embedding_space_access_is_read_only_for_management() {
        assert!(management_allowed(ConnectionMode::Local).is_ok());
        assert!(matches!(
            management_allowed(ConnectionMode::Remote),
            Err(AppError::Config(_))
        ));
    }

    #[test]
    fn embedding_space_dto_preserves_tables_markers_rejections_and_space() {
        let response = mem_svc::GetEmbeddingSpaceResponse {
            current_space: Some(mem_data::EmbeddingSpace {
                space_id: "space-a".into(),
                model_id: "org/model".into(),
                tokenizer_model_id: "org/tokenizer".into(),
                revision: "rev-1".into(),
                dimension: 768,
                distance: "cosine".into(),
            }),
            mm_embedding_worker_name: "memories-mm-embedding-space-a".into(),
            registration_state: REGISTRATION_PENDING,
            tables: vec![mem_svc::EmbeddingTableRecord {
                table: "thread".into(),
                record: Some(mem_data::EmbeddingSpaceRecord {
                    space: None,
                    legacy_accept: true,
                    migration_marker: Some(mem_data::EmbeddingMigrationMarker {
                        state: "pending".into(),
                        attempt_id: "attempt-a".into(),
                    }),
                }),
            }],
            rpc_migration_supported: false,
            write_rejections: HashMap::from([("space_mismatch".into(), 3)]),
        };

        let dto = embedding_space_dto(response).expect("valid pending status");
        let json = serde_json::to_value(dto).expect("DTO serializes");

        assert_eq!(json["current_space"]["space_id"], "space-a");
        assert_eq!(json["current_space"]["revision"], "rev-1");
        assert_eq!(
            json["mm_embedding_worker_name"],
            "memories-mm-embedding-space-a"
        );
        assert_eq!(json["registration_state"], "pending");
        assert_eq!(json["tables"][0]["table"], "thread");
        assert_eq!(json["tables"][0]["record"]["legacy_accept"], true);
        assert_eq!(
            json["tables"][0]["record"]["migration_marker"]["attempt_id"],
            "attempt-a"
        );
        assert_eq!(json["rpc_migration_supported"], false);
        assert_eq!(json["write_rejections"]["space_mismatch"], 3);
    }
}
