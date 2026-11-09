use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use prost::Message;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, transport::Server};

use crate::{
    commands::{AppState, issue_chat_mutations as mutations},
    error::AppError,
    grpc::{
        LOOKBACK_CHAT_ISSUE_DESCRIPTOR_SET, issue_chat_proto,
        issue_tracker::IssueTrackerClientError,
        proto::issue_tracker::{manager::v1 as manager, v1 as issue},
    },
};

const PERMIT_TTL: Duration = Duration::from_secs(30);
const MAX_PENDING_PERMITS: usize = 4096;
const MAX_PAGE_TOKEN_BYTES: usize = 4096;
const MAX_ISSUE_ID_CHARS: usize = 256;
const MAX_SUMMARY_DESCRIPTION_CHARS: usize = 240;
const MAX_MUTATION_INPUT_BYTES: usize = 64 * 1024;
static CORRELATION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub const DEFAULT_PAGE_SIZE: u32 = 20;
pub const MAX_PAGE_SIZE: u32 = 50;
pub const MAX_QUERY_CHARS: usize = 256;

type RequestDigest = [u8; 32];

/// Chat Issue methods exposed by the loopback facade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueChatMethod {
    ListIssues,
    GetIssue,
    CreateIssue,
    UpdateIssue,
    CancelIssue,
}

impl IssueChatMethod {
    fn rpc_name(self) -> &'static str {
        match self {
            Self::ListIssues => "lookback.chat.issue.v1.LookbackChatIssueService/ListIssues",
            Self::GetIssue => "lookback.chat.issue.v1.LookbackChatIssueService/GetIssue",
            Self::CreateIssue => "lookback.chat.issue.v1.LookbackChatIssueService/CreateIssue",
            Self::UpdateIssue => "lookback.chat.issue.v1.LookbackChatIssueService/UpdateIssue",
            Self::CancelIssue => "lookback.chat.issue.v1.LookbackChatIssueService/CancelIssue",
        }
    }
}

/// Typed request for registering one short-lived permission before dispatch.
#[derive(Debug, Clone)]
pub enum IssueChatRequest {
    List(ListIssuesRequest),
    Get(GetIssueRequest),
    Create(CreateIssueRequest),
    Update(UpdateIssueRequest),
    Cancel(CancelIssueRequest),
}

use issue_chat_proto::{
    CancelIssueRequest, CreateIssueRequest, GetIssueRequest, IssueChatMutationResponse,
    IssueChatMutationResult, ListIssuesRequest, UpdateIssueRequest, issue_chat_mutation_response,
};

/// Validate chat-generated mutation arguments and create the exact worker
/// arguments/request pair that the caller must authorize and dispatch.
///
/// The LLM supplies only `inputJson`. This app-side constructor injects the
/// current speech and tool-call IDs as separate protobuf fields, ensuring the
/// LLM cannot choose operation identity and that the permit digest covers the
/// same bytes sent to the facade.
pub fn prepare_issue_chat_mutation(
    method: IssueChatMethod,
    input: &serde_json::Value,
    speech_id: &str,
    tool_call_id: &str,
) -> Result<(IssueChatRequest, serde_json::Value), IssueChatError> {
    let input_object = input
        .as_object()
        .ok_or_else(|| invalid_request("mutation arguments must be a JSON object"))?;
    if ["speech_id", "tool_call_id", "speechId", "toolCallId"]
        .iter()
        .any(|field| input_object.contains_key(*field))
    {
        return Err(invalid_request(
            "speech and tool-call identifiers are injected by the chat app",
        ));
    }
    let input_json = serde_json::to_string(input)
        .map_err(|_| invalid_request("mutation arguments are not valid JSON"))?;
    if input_json.len() > MAX_MUTATION_INPUT_BYTES {
        return Err(invalid_request("mutation arguments exceed the size limit"));
    }
    let _validated = validate_mutation_input(method, &input_json, speech_id, tool_call_id)?;
    let worker_arguments = serde_json::json!({
        "inputJson": input_json.clone(),
        "speechId": speech_id,
        "toolCallId": tool_call_id,
    });
    let request = match method {
        IssueChatMethod::CreateIssue => IssueChatRequest::Create(CreateIssueRequest {
            speech_id: speech_id.to_owned(),
            tool_call_id: tool_call_id.to_owned(),
            input_json,
        }),
        IssueChatMethod::UpdateIssue => IssueChatRequest::Update(UpdateIssueRequest {
            speech_id: speech_id.to_owned(),
            tool_call_id: tool_call_id.to_owned(),
            input_json,
        }),
        IssueChatMethod::CancelIssue => IssueChatRequest::Cancel(CancelIssueRequest {
            speech_id: speech_id.to_owned(),
            tool_call_id: tool_call_id.to_owned(),
            input_json,
        }),
        IssueChatMethod::ListIssues | IssueChatMethod::GetIssue => {
            return Err(invalid_request("method is not an Issue mutation"));
        }
    };
    Ok((request, worker_arguments))
}

enum ValidatedMutationInput {
    Create(mutations::ValidatedCreateIssueInput),
    Update(mutations::ValidatedUpdateIssueInput),
    Cancel(mutations::ValidatedCancelIssueInput),
}

fn validate_mutation_input(
    method: IssueChatMethod,
    input_json: &str,
    speech_id: &str,
    tool_call_id: &str,
) -> Result<ValidatedMutationInput, IssueChatError> {
    match method {
        IssueChatMethod::CreateIssue => mutations::validate_create_issue_input(
            input_json_with_context(input_json, speech_id, tool_call_id)?,
        )
        .map(ValidatedMutationInput::Create)
        .map_err(map_validation_error),
        IssueChatMethod::UpdateIssue => mutations::validate_update_issue_input(
            input_json_with_context(input_json, speech_id, tool_call_id)?,
        )
        .map(ValidatedMutationInput::Update)
        .map_err(map_validation_error),
        IssueChatMethod::CancelIssue => mutations::validate_cancel_issue_input(
            input_json_with_context(input_json, speech_id, tool_call_id)?,
        )
        .map(ValidatedMutationInput::Cancel)
        .map_err(map_validation_error),
        IssueChatMethod::ListIssues | IssueChatMethod::GetIssue => {
            Err(invalid_request("method is not an Issue mutation"))
        }
    }
}

fn input_json_with_context<T: DeserializeOwned>(
    input_json: &str,
    speech_id: &str,
    tool_call_id: &str,
) -> Result<T, IssueChatError> {
    if input_json.len() > MAX_MUTATION_INPUT_BYTES {
        return Err(invalid_request("mutation arguments exceed the size limit"));
    }
    let mut value: serde_json::Value = serde_json::from_str(input_json)
        .map_err(|_| invalid_request("mutation arguments must contain valid JSON"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid_request("mutation arguments must be a JSON object"))?;
    if ["speech_id", "tool_call_id", "speechId", "toolCallId"]
        .iter()
        .any(|field| object.contains_key(*field))
    {
        return Err(invalid_request(
            "speech and tool-call identifiers must not be included in inputJson",
        ));
    }
    object.insert("speech_id".into(), speech_id.into());
    object.insert("tool_call_id".into(), tool_call_id.into());
    serde_json::from_value(value)
        .map_err(|_| invalid_request("mutation arguments do not match the allowed input schema"))
}

fn map_validation_error(error: AppError) -> IssueChatError {
    match error {
        AppError::Config(message) => invalid_request(message),
        _ => invalid_request("mutation arguments failed validation"),
    }
}

/// Shared authorization registry for the worker facade.
///
/// Permissions are keyed by a non-secret digest of the complete protobuf
/// request and its fixed RPC name. They expire automatically and are removed
/// atomically before the corresponding Issue operation begins.
#[derive(Default)]
struct PermissionRegistry {
    pending: Mutex<HashMap<RequestDigest, Instant>>,
}

impl PermissionRegistry {
    fn grant(&self, digest: RequestDigest) -> bool {
        let now = Instant::now();
        let mut pending = self.pending.lock();
        pending.retain(|_, expires_at| *expires_at > now);
        if pending.len() >= MAX_PENDING_PERMITS || pending.contains_key(&digest) {
            return false;
        }
        pending.insert(digest, now + PERMIT_TTL);
        true
    }

    #[cfg(test)]
    fn insert_until(&self, digest: RequestDigest, expires_at: Instant) {
        self.pending.lock().insert(digest, expires_at);
    }

    fn consume(&self, digest: &RequestDigest) -> bool {
        let now = Instant::now();
        let Some(expires_at) = self.pending.lock().remove(digest) else {
            return false;
        };
        expires_at > now
    }
}

/// A loopback-only gRPC facade for chat Issue operations.
#[derive(Clone)]
pub struct IssueChatGateway {
    app: AppHandle,
    permissions: Arc<PermissionRegistry>,
}

impl IssueChatGateway {
    fn new(app: AppHandle) -> Self {
        Self {
            app,
            permissions: Arc::new(PermissionRegistry::default()),
        }
    }

    /// Authorize exactly one valid chat Issue request for at most 30 seconds.
    ///
    /// The returned SHA-256 digest is useful for non-secret approval/audit
    /// correlation. It is not an authorization token and is not accepted on
    /// the wire; the permit remains in this process's shared registry.
    pub fn permit_request_once(
        &self,
        request: &IssueChatRequest,
    ) -> Result<String, IssueChatError> {
        let digest = match request {
            IssueChatRequest::List(request) => {
                validate_read_speech(&request.speech_id)?;
                normalize_list_request(request.clone())?;
                request_digest(IssueChatMethod::ListIssues, request)
            }
            IssueChatRequest::Get(request) => {
                validate_read_speech(&request.speech_id)?;
                normalize_issue_id(&request.issue_id)?;
                validate_relation_page_token(request.relation_page_token.as_deref())?;
                request_digest(IssueChatMethod::GetIssue, request)
            }
            IssueChatRequest::Create(request) => {
                validate_mutation_input(
                    IssueChatMethod::CreateIssue,
                    &request.input_json,
                    &request.speech_id,
                    &request.tool_call_id,
                )?;
                request_digest(IssueChatMethod::CreateIssue, request)
            }
            IssueChatRequest::Update(request) => {
                validate_mutation_input(
                    IssueChatMethod::UpdateIssue,
                    &request.input_json,
                    &request.speech_id,
                    &request.tool_call_id,
                )?;
                request_digest(IssueChatMethod::UpdateIssue, request)
            }
            IssueChatRequest::Cancel(request) => {
                validate_mutation_input(
                    IssueChatMethod::CancelIssue,
                    &request.input_json,
                    &request.speech_id,
                    &request.tool_call_id,
                )?;
                request_digest(IssueChatMethod::CancelIssue, request)
            }
        };
        if !self.permissions.grant(digest) {
            return Err(chat_error(
                IssueChatErrorCode::Unavailable,
                "The chat Issue permission registry is busy",
                true,
            ));
        }
        Ok(format_digest(&digest))
    }

    async fn tracker_service(
        &self,
    ) -> Result<crate::grpc::issue_tracker::IssueTrackerGrpcService, IssueChatError> {
        let state = self
            .app
            .try_state::<AppState>()
            .ok_or_else(|| unavailable_error("Issue Tracker application state is unavailable"))?;
        state
            .issue_tracker_service()
            .await
            .map_err(|_| unavailable_error("Issue Tracker is unavailable"))
    }

    async fn list_issues_inner(
        &self,
        request: ListIssuesRequest,
    ) -> Result<IssueListPage, IssueChatError> {
        validate_read_speech(&request.speech_id)?;
        let request = normalize_list_request(request)?;
        let service = self.tracker_service().await?;
        let result = service
            .list_issues_with_sync(&next_correlation_id("list"), manager_list_args(request))
            .await
            .map_err(map_manager_error)?;
        let issues = result
            .issues
            .into_iter()
            .map(issue_summary_from_sync)
            .collect::<Result<Vec<_>, _>>()?;
        let page = result.page.unwrap_or_default();
        Ok(IssueListPage {
            issues,
            next_page_token: page.next_page_token,
            total_size: page.total_size,
        })
    }

    async fn get_issue_inner(
        &self,
        request: GetIssueRequest,
    ) -> Result<IssueDetail, IssueChatError> {
        let issue_id = normalize_issue_id(&request.issue_id)?;
        validate_read_speech(&request.speech_id)?;
        validate_relation_page_token(request.relation_page_token.as_deref())?;
        let service = self.tracker_service().await?;
        let result = service
            .get_issue_with_sync(
                &next_correlation_id("get"),
                manager_get_args(issue_id.clone()),
            )
            .await
            .map_err(map_manager_error)?;
        let detail = issue_detail_from_sync(result)?;
        let relations = service
            .get_issue_relations(
                &next_correlation_id("relations"),
                manager::GetIssueRelationsArgs {
                    issue_id,
                    page: Some(manager::PageRequest {
                        page_size: MAX_PAGE_SIZE,
                        page_token: request.relation_page_token,
                    }),
                    ..Default::default()
                },
            )
            .await
            .map_err(map_manager_error)?;
        enrich_issue_detail_with_relations(detail, relations)
    }

    async fn create_issue_inner(
        &self,
        request: CreateIssueRequest,
    ) -> Result<IssueChatMutationResult, IssueChatError> {
        let input = validate_mutation_input(
            IssueChatMethod::CreateIssue,
            &request.input_json,
            &request.speech_id,
            &request.tool_call_id,
        )?;
        self.execute_mutation(input).await
    }

    async fn update_issue_inner(
        &self,
        request: UpdateIssueRequest,
    ) -> Result<IssueChatMutationResult, IssueChatError> {
        let input = validate_mutation_input(
            IssueChatMethod::UpdateIssue,
            &request.input_json,
            &request.speech_id,
            &request.tool_call_id,
        )?;
        self.execute_mutation(input).await
    }

    async fn cancel_issue_inner(
        &self,
        request: CancelIssueRequest,
    ) -> Result<IssueChatMutationResult, IssueChatError> {
        let input = validate_mutation_input(
            IssueChatMethod::CancelIssue,
            &request.input_json,
            &request.speech_id,
            &request.tool_call_id,
        )?;
        self.execute_mutation(input).await
    }

    async fn execute_mutation(
        &self,
        input: ValidatedMutationInput,
    ) -> Result<IssueChatMutationResult, IssueChatError> {
        let state = self
            .app
            .try_state::<AppState>()
            .ok_or_else(|| unavailable_error("Issue Tracker application state is unavailable"))?;
        self.tracker_service().await?;
        let result = match input {
            ValidatedMutationInput::Create(input) => mutations::create_issue(&state, input).await,
            ValidatedMutationInput::Update(input) => mutations::update_issue(&state, input).await,
            ValidatedMutationInput::Cancel(input) => mutations::cancel_issue(&state, input).await,
        }
        .map_err(map_mutation_error)?;
        mutation_result_from_command(result)
    }
}

/// Bind the facade to loopback and start gRPC reflection plus the chat Issue RPCs.
/// A zero `port` asks the OS to choose a port; the returned handle exposes the
/// actual address for jobworkerp worker configuration.
pub async fn start(
    app: AppHandle,
    port: u16,
) -> Result<IssueChatGatewayHandle, IssueChatGatewayStartError> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let local_addr = listener.local_addr()?;
    let delegation = crate::coding_agent::delegation_rpc::DelegationRpc::new(Arc::new(
        crate::coding_agent::delegation_rpc::AppDelegationBackend(app.clone()),
    ));
    let delegation_service = issue_chat_proto::lookback_coding_agent_service_server::LookbackCodingAgentServiceServer::new(delegation);
    let gateway = Arc::new(IssueChatGateway::new(app));
    let service =
        issue_chat_proto::lookback_chat_issue_service_server::LookbackChatIssueServiceServer::new(
            gateway.as_ref().clone(),
        );
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(LOOKBACK_CHAT_ISSUE_DESCRIPTOR_SET)
        .build_v1()
        .map_err(|error| IssueChatGatewayStartError::Reflection(error.to_string()))?;
    let incoming = futures::stream::try_unfold(listener, |listener| async move {
        let (connection, _) = listener.accept().await?;
        Ok::<_, io::Error>(Some((connection, listener)))
    });
    let cancellation = CancellationToken::new();
    let shutdown = cancellation.clone();
    let task = tokio::spawn(async move {
        Server::builder()
            .add_service(reflection)
            .add_service(service)
            .add_service(delegation_service)
            .serve_with_incoming_shutdown(incoming, shutdown.cancelled_owned())
            .await
    });

    Ok(IssueChatGatewayHandle {
        gateway,
        local_addr,
        cancellation,
        task,
    })
}

/// Running server ownership and its shared state for app-side permission grants.
pub struct IssueChatGatewayHandle {
    gateway: Arc<IssueChatGateway>,
    local_addr: SocketAddr,
    cancellation: CancellationToken,
    task: JoinHandle<Result<(), tonic::transport::Error>>,
}

impl IssueChatGatewayHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn gateway_state(&self) -> Arc<IssueChatGateway> {
        Arc::clone(&self.gateway)
    }

    pub async fn shutdown(self) -> Result<(), IssueChatGatewayServerError> {
        self.cancellation.cancel();
        self.task
            .await
            .map_err(|error| IssueChatGatewayServerError::Join(error.to_string()))?
            .map_err(|error| IssueChatGatewayServerError::Serve(error.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IssueChatGatewayStartError {
    #[error("could not bind the chat Issue gateway: {0}")]
    Bind(#[from] io::Error),
    #[error("could not build chat Issue gRPC reflection service: {0}")]
    Reflection(String),
}

#[derive(Debug, thiserror::Error)]
pub enum IssueChatGatewayServerError {
    #[error("chat Issue gateway task failed: {0}")]
    Join(String),
    #[error("chat Issue gateway server failed: {0}")]
    Serve(String),
}

struct NormalizedListRequest {
    page_size: u32,
    page_token: Option<String>,
    text_query: Option<String>,
    states: Vec<i32>,
}

fn normalize_list_request(
    request: ListIssuesRequest,
) -> Result<NormalizedListRequest, IssueChatError> {
    if request.page_size > MAX_PAGE_SIZE {
        return Err(invalid_request(format!(
            "page_size must not exceed {MAX_PAGE_SIZE}"
        )));
    }
    if request.states.len() > 7
        || request
            .states
            .iter()
            .any(|state| match issue::IssueState::try_from(*state) {
                Ok(issue::IssueState::Unspecified) | Err(_) => true,
                Ok(_) => false,
            })
    {
        return Err(invalid_request("states contains an unsupported value"));
    }
    if request
        .page_token
        .as_ref()
        .is_some_and(|token| token.is_empty() || token.len() > MAX_PAGE_TOKEN_BYTES)
    {
        return Err(invalid_request(
            "page_token is empty or exceeds its size limit",
        ));
    }
    let text_query = match request.text_query {
        Some(query) if query.chars().count() > MAX_QUERY_CHARS => {
            return Err(invalid_request(format!(
                "text_query must not exceed {MAX_QUERY_CHARS} characters"
            )));
        }
        Some(query) => {
            let trimmed = query.trim().to_owned();
            (!trimmed.is_empty()).then_some(trimmed)
        }
        None => None,
    };

    Ok(NormalizedListRequest {
        page_size: if request.page_size == 0 {
            DEFAULT_PAGE_SIZE
        } else {
            request.page_size
        },
        page_token: request.page_token,
        text_query,
        states: request.states,
    })
}

fn normalize_issue_id(issue_id: &str) -> Result<String, IssueChatError> {
    let issue_id = issue_id.trim();
    if issue_id.is_empty()
        || issue_id.chars().count() > MAX_ISSUE_ID_CHARS
        || issue_id.len() > MAX_ISSUE_ID_CHARS * 4
    {
        return Err(invalid_request(
            "issue_id is empty or exceeds its size limit",
        ));
    }
    Ok(issue_id.to_owned())
}

fn validate_relation_page_token(token: Option<&str>) -> Result<(), IssueChatError> {
    if token.is_some_and(|value| value.is_empty() || value.len() > MAX_PAGE_TOKEN_BYTES) {
        Err(invalid_request(
            "relation_page_token is empty or exceeds its size limit",
        ))
    } else {
        Ok(())
    }
}

fn validate_read_speech(speech_id: &str) -> Result<(), IssueChatError> {
    if speech_id.trim().is_empty() || speech_id.len() > MAX_ISSUE_ID_CHARS {
        Err(invalid_request("chat speech ID is required and bounded"))
    } else {
        Ok(())
    }
}

fn manager_list_args(request: NormalizedListRequest) -> manager::ListIssuesWithSyncArgs {
    manager::ListIssuesWithSyncArgs {
        page: Some(manager::PageRequest {
            page_size: request.page_size,
            page_token: request.page_token,
        }),
        issue_filter: Some(issue::IssueFilter {
            states: request.states,
            text_query: request.text_query,
            ..Default::default()
        }),
        sort: issue::IssueSort::UpdatedDesc as i32,
        ..Default::default()
    }
}

fn manager_get_args(issue_id: String) -> manager::GetIssueWithSyncArgs {
    manager::GetIssueWithSyncArgs {
        issue_id,
        ..Default::default()
    }
}

fn request_digest<M: Message>(method: IssueChatMethod, request: &M) -> RequestDigest {
    let mut digest = Sha256::new();
    digest.update(b"lookback-issue-chat-request-v1\0");
    digest.update(method.rpc_name().as_bytes());
    digest.update([0]);
    digest.update(request.encode_to_vec());
    digest.finalize().into()
}

fn authorize_request<M: Message>(
    permissions: &PermissionRegistry,
    method: IssueChatMethod,
    request: &M,
) -> Result<(), IssueChatError> {
    if permissions.consume(&request_digest(method, request)) {
        Ok(())
    } else {
        Err(chat_error(
            IssueChatErrorCode::PermissionDenied,
            "This Issue request has no unused app-issued permission",
            false,
        ))
    }
}

fn format_digest(digest: &RequestDigest) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn next_correlation_id(operation: &str) -> String {
    let sequence = CORRELATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("chat-issue-{operation}-{sequence}")
}

fn issue_summary_from_sync(value: manager::IssueWithSync) -> Result<IssueSummary, IssueChatError> {
    let issue = value.issue.ok_or_else(|| {
        chat_error(
            IssueChatErrorCode::Internal,
            "Issue Tracker returned an issue row without Issue data",
            false,
        )
    })?;
    Ok(IssueSummary {
        issue_id: issue.issue_id,
        title: issue.title,
        description_excerpt: truncate_excerpt(&issue.description),
        kind: issue_kind_name(issue.kind),
        priority: issue_priority_name(issue.priority),
        state: issue_state_name(issue.state),
        labels: issue.labels,
        revision: issue.revision,
        lookback_ready: value.lookback_ready,
        sync_state: value
            .sync
            .map(|sync| sync_state_name(sync.sync_state))
            .unwrap_or_else(|| "UNSPECIFIED".into()),
    })
}

fn issue_detail_from_sync(value: manager::IssueWithSync) -> Result<IssueDetail, IssueChatError> {
    let issue = value.issue.ok_or_else(|| {
        chat_error(
            IssueChatErrorCode::Internal,
            "Issue Tracker returned no Issue data",
            false,
        )
    })?;
    Ok(IssueDetail {
        issue_id: issue.issue_id,
        title: issue.title,
        description: issue.description,
        kind: issue_kind_name(issue.kind),
        priority: issue_priority_name(issue.priority),
        state: issue_state_name(issue.state),
        labels: issue.labels,
        acceptance_criteria: issue.acceptance_criteria,
        source_evidence: issue
            .source_evidence
            .into_iter()
            .map(|source| IssueSourceEvidence {
                thread_id: source.thread_id,
                memory_id: source.memory_id,
            })
            .collect(),
        revision: issue.revision,
        lookback_ready: value.lookback_ready,
        sync_state: value
            .sync
            .map(|sync| sync_state_name(sync.sync_state))
            .unwrap_or_else(|| "UNSPECIFIED".into()),
        relations: Vec::new(),
        next_relation_page_token: None,
        confirmation_status: enum_name(
            issue::ConfirmationStatus::try_from(issue.confirmation_status)
                .map(|value| value.as_str_name()),
            "CONFIRMATION_STATUS_",
        ),
        creation_basis: enum_name(
            issue::CreationBasis::try_from(issue.creation_basis).map(|value| value.as_str_name()),
            "CREATION_BASIS_",
        ),
    })
}

fn enrich_issue_detail_with_relations(
    mut detail: IssueDetail,
    page: manager::IssueRelationsWithSync,
) -> Result<IssueDetail, IssueChatError> {
    let issue = page.issue.and_then(|row| row.issue).ok_or_else(|| {
        chat_error(
            IssueChatErrorCode::Internal,
            "Relations response is missing its Issue",
            false,
        )
    })?;
    if issue.issue_id != detail.issue_id || issue.revision != detail.revision {
        return Err(chat_error(
            IssueChatErrorCode::UpstreamError,
            "Issue changed while loading relations; retry get",
            true,
        ));
    }
    detail.relations = page
        .relations
        .into_iter()
        .map(|relation| issue_chat_proto::RelatedIssue {
            relation_id: relation.relation_id,
            kind: enum_name(
                issue::RelationKind::try_from(relation.kind).map(|kind| kind.as_str_name()),
                "RELATION_KIND_",
            ),
            source_issue_id: relation.source_issue_id,
            target_issue_id: relation.target_issue_id,
        })
        .collect();
    detail.next_relation_page_token = page.page.and_then(|page| page.next_page_token);
    Ok(detail)
}

fn issue_kind_name(value: i32) -> String {
    enum_name(
        issue::IssueKind::try_from(value).map(|value| value.as_str_name()),
        "ISSUE_KIND_",
    )
}

fn issue_priority_name(value: i32) -> String {
    enum_name(
        issue::IssuePriority::try_from(value).map(|value| value.as_str_name()),
        "ISSUE_PRIORITY_",
    )
}

fn issue_state_name(value: i32) -> String {
    enum_name(
        issue::IssueState::try_from(value).map(|value| value.as_str_name()),
        "ISSUE_STATE_",
    )
}

fn sync_state_name(value: i32) -> String {
    enum_name(
        manager::SyncState::try_from(value).map(|value| value.as_str_name()),
        "SYNC_STATE_",
    )
}

fn enum_name(value: Result<&str, impl std::fmt::Debug>, prefix: &str) -> String {
    value
        .map(|value| value.strip_prefix(prefix).unwrap_or(value).to_owned())
        .unwrap_or_else(|_| "UNKNOWN".into())
}

fn truncate_excerpt(value: &str) -> String {
    let mut characters = value.chars();
    let excerpt: String = characters
        .by_ref()
        .take(MAX_SUMMARY_DESCRIPTION_CHARS)
        .collect();
    if characters.next().is_some() {
        format!("{excerpt}…")
    } else {
        excerpt
    }
}

fn map_manager_error(error: IssueTrackerClientError) -> IssueChatError {
    match error {
        IssueTrackerClientError::InvalidRequest(message) => {
            chat_error(IssueChatErrorCode::InvalidRequest, message, false)
        }
        IssueTrackerClientError::Manager(error) => {
            let code = match manager::ManagerErrorCode::try_from(error.code).ok() {
                Some(manager::ManagerErrorCode::IssueNotFound) => IssueChatErrorCode::NotFound,
                Some(manager::ManagerErrorCode::PageTokenExpired) => {
                    IssueChatErrorCode::PageTokenExpired
                }
                Some(
                    manager::ManagerErrorCode::ManagerInvalidArgument
                    | manager::ManagerErrorCode::ResourceLimitExceeded
                    | manager::ManagerErrorCode::InvalidPageToken,
                ) => IssueChatErrorCode::InvalidRequest,
                Some(manager::ManagerErrorCode::BackendUnavailable) => {
                    IssueChatErrorCode::Unavailable
                }
                _ => IssueChatErrorCode::UpstreamError,
            };
            chat_error(code, error.message, error.retryable)
        }
        IssueTrackerClientError::Gateway(_) => chat_error(
            IssueChatErrorCode::UpstreamError,
            "Issue Tracker Gateway rejected the requested operation",
            false,
        ),
        IssueTrackerClientError::NotHealthy(_) | IssueTrackerClientError::Transport(_) => {
            unavailable_error("Issue Tracker is unavailable")
        }
        IssueTrackerClientError::UnconfirmedOperation(_) => chat_error(
            IssueChatErrorCode::UpstreamError,
            "Issue Tracker could not confirm the read outcome",
            true,
        ),
    }
}

fn map_mutation_error(error: AppError) -> IssueChatError {
    match error {
        AppError::Config(message) => invalid_request(message),
        AppError::Grpc(status)
            if matches!(
                status.code(),
                tonic::Code::Unavailable | tonic::Code::DeadlineExceeded
            ) =>
        {
            unavailable_error("Issue Tracker is unavailable")
        }
        AppError::Transport(_) => unavailable_error("Issue Tracker is unavailable"),
        _ => chat_error(
            IssueChatErrorCode::UpstreamError,
            "Issue Tracker did not confirm the requested mutation",
            true,
        ),
    }
}

fn mutation_result_from_command(
    result: mutations::IssueChatMutationResult,
) -> Result<IssueChatMutationResult, IssueChatError> {
    if result.operation_state != "COMMITTED"
        || result.issue_id.trim().is_empty()
        || result.operation_id.trim().is_empty()
    {
        return Err(chat_error(
            IssueChatErrorCode::UpstreamError,
            "Issue Tracker did not confirm a committed mutation",
            true,
        ));
    }
    Ok(IssueChatMutationResult {
        issue_id: result.issue_id,
        operation_id: result.operation_id,
        operation_state: result.operation_state,
        revision: result.revision.to_string(),
        resulting_state: result.resulting_state,
    })
}

fn mutation_response(
    outcome: Result<IssueChatMutationResult, IssueChatError>,
) -> IssueChatMutationResponse {
    IssueChatMutationResponse {
        outcome: Some(match outcome {
            Ok(result) => issue_chat_mutation_response::Outcome::Success(result),
            Err(error) => issue_chat_mutation_response::Outcome::Error(error),
        }),
    }
}

fn chat_error(
    code: IssueChatErrorCode,
    message: impl Into<String>,
    retryable: bool,
) -> IssueChatError {
    IssueChatError {
        code: code as i32,
        message: message.into(),
        retryable,
    }
}

fn invalid_request(message: impl Into<String>) -> IssueChatError {
    chat_error(IssueChatErrorCode::InvalidRequest, message, false)
}

fn unavailable_error(message: impl Into<String>) -> IssueChatError {
    chat_error(IssueChatErrorCode::Unavailable, message, true)
}

#[tonic::async_trait]
impl issue_chat_proto::lookback_chat_issue_service_server::LookbackChatIssueService
    for IssueChatGateway
{
    async fn list_issues(
        &self,
        request: Request<ListIssuesRequest>,
    ) -> Result<Response<ListIssuesResponse>, tonic::Status> {
        let request = request.into_inner();
        let outcome =
            match authorize_request(&self.permissions, IssueChatMethod::ListIssues, &request) {
                Err(error) => Err(error),
                Ok(()) => self.list_issues_inner(request).await,
            };
        let response = match outcome {
            Ok(page) => ListIssuesResponse {
                outcome: Some(list_issues_response::Outcome::Success(page)),
            },
            Err(error) => ListIssuesResponse {
                outcome: Some(list_issues_response::Outcome::Error(error)),
            },
        };
        Ok(Response::new(response))
    }

    async fn get_issue(
        &self,
        request: Request<GetIssueRequest>,
    ) -> Result<Response<GetIssueResponse>, tonic::Status> {
        let request = request.into_inner();
        let outcome =
            match authorize_request(&self.permissions, IssueChatMethod::GetIssue, &request) {
                Err(error) => Err(error),
                Ok(()) => self.get_issue_inner(request).await,
            };
        let response = match outcome {
            Ok(issue) => GetIssueResponse {
                outcome: Some(get_issue_response::Outcome::Success(issue)),
            },
            Err(error) => GetIssueResponse {
                outcome: Some(get_issue_response::Outcome::Error(error)),
            },
        };
        Ok(Response::new(response))
    }

    async fn create_issue(
        &self,
        request: Request<CreateIssueRequest>,
    ) -> Result<Response<IssueChatMutationResponse>, tonic::Status> {
        let request = request.into_inner();
        let outcome =
            match authorize_request(&self.permissions, IssueChatMethod::CreateIssue, &request) {
                Err(error) => Err(error),
                Ok(()) => self.create_issue_inner(request).await,
            };
        Ok(Response::new(mutation_response(outcome)))
    }

    async fn update_issue(
        &self,
        request: Request<UpdateIssueRequest>,
    ) -> Result<Response<IssueChatMutationResponse>, tonic::Status> {
        let request = request.into_inner();
        let outcome =
            match authorize_request(&self.permissions, IssueChatMethod::UpdateIssue, &request) {
                Err(error) => Err(error),
                Ok(()) => self.update_issue_inner(request).await,
            };
        Ok(Response::new(mutation_response(outcome)))
    }

    async fn cancel_issue(
        &self,
        request: Request<CancelIssueRequest>,
    ) -> Result<Response<IssueChatMutationResponse>, tonic::Status> {
        let request = request.into_inner();
        let outcome =
            match authorize_request(&self.permissions, IssueChatMethod::CancelIssue, &request) {
                Err(error) => Err(error),
                Ok(()) => self.cancel_issue_inner(request).await,
            };
        Ok(Response::new(mutation_response(outcome)))
    }
}

use issue_chat_proto::{
    GetIssueResponse, IssueChatError, IssueChatErrorCode, IssueDetail, IssueListPage,
    IssueSourceEvidence, IssueSummary, ListIssuesResponse, get_issue_response,
    list_issues_response,
};

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use prost_reflect::{DescriptorPool, Kind};

    use crate::grpc::{
        LOOKBACK_CHAT_ISSUE_DESCRIPTOR_SET,
        issue_chat_proto::{
            CancelIssueRequest, CreateIssueRequest, IssueChatErrorCode, ListIssuesRequest,
            UpdateIssueRequest,
        },
    };

    use super::{
        IssueChatMethod, IssueChatRequest, IssueDetail, PermissionRegistry, authorize_request,
        enrich_issue_detail_with_relations, issue, issue_detail_from_sync, issue_summary_from_sync,
        manager, manager_get_args, manager_list_args, mutation_result_from_command,
        normalize_issue_id, normalize_list_request, prepare_issue_chat_mutation, request_digest,
        validate_relation_page_token,
    };

    #[test]
    fn list_requests_enforce_page_and_query_boundaries() {
        let default_page = normalize_list_request(ListIssuesRequest::default())
            .expect("zero selects bounded default");
        assert_eq!(default_page.page_size, super::DEFAULT_PAGE_SIZE);

        for page_size in [1, super::MAX_PAGE_SIZE] {
            assert_eq!(
                normalize_list_request(ListIssuesRequest {
                    page_size,
                    ..Default::default()
                })
                .expect("inclusive page boundary")
                .page_size,
                page_size
            );
        }
        assert!(
            normalize_list_request(ListIssuesRequest {
                page_size: super::MAX_PAGE_SIZE + 1,
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            normalize_list_request(ListIssuesRequest {
                page_token: Some(String::new()),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            normalize_list_request(ListIssuesRequest {
                page_token: Some("t".repeat(super::MAX_PAGE_TOKEN_BYTES)),
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            normalize_list_request(ListIssuesRequest {
                page_token: Some("t".repeat(super::MAX_PAGE_TOKEN_BYTES + 1)),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            normalize_list_request(ListIssuesRequest {
                text_query: Some("x".repeat(super::MAX_QUERY_CHARS)),
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            normalize_list_request(ListIssuesRequest {
                text_query: Some("x".repeat(super::MAX_QUERY_CHARS + 1)),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn list_read_model_accepts_all_seven_lifecycle_states_and_rejects_unknown_or_excess() {
        let states = [
            issue::IssueState::Open,
            issue::IssueState::InProgress,
            issue::IssueState::Blocked,
            issue::IssueState::Deferred,
            issue::IssueState::Done,
            issue::IssueState::Canceled,
            issue::IssueState::AwaitingReview,
        ]
        .map(|state| state as i32)
        .to_vec();
        let normalized = normalize_list_request(ListIssuesRequest {
            page_size: 7,
            page_token: Some("bounded-next".into()),
            text_query: Some("  implementation  ".into()),
            states: states.clone(),
            ..Default::default()
        })
        .expect("all seven known states are valid");
        let manager_args = manager_list_args(normalized);
        assert_eq!(manager_args.issue_filter.unwrap().states, states);
        let page = manager_args.page.unwrap();
        assert_eq!(page.page_size, 7);
        assert_eq!(page.page_token.as_deref(), Some("bounded-next"));

        assert!(
            normalize_list_request(ListIssuesRequest {
                states: vec![99],
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            normalize_list_request(ListIssuesRequest {
                states: [states.as_slice(), &[issue::IssueState::Open as i32]].concat(),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn direct_worker_request_without_a_matching_permit_is_denied() {
        let registry = PermissionRegistry::default();
        let request = ListIssuesRequest {
            text_query: Some("release notes".into()),
            ..Default::default()
        };

        let error = authorize_request(&registry, IssueChatMethod::ListIssues, &request)
            .expect_err("direct call has no app-issued permit");
        assert_eq!(error.code, IssueChatErrorCode::PermissionDenied as i32);
        assert!(!error.retryable);
    }

    #[test]
    fn a_request_permit_is_bound_to_method_and_consumed_once() {
        let registry = PermissionRegistry::default();
        let request = ListIssuesRequest {
            page_size: 4,
            text_query: Some("lookback".into()),
            ..Default::default()
        };
        let digest = request_digest(IssueChatMethod::ListIssues, &request);
        registry.insert_until(digest, Instant::now() + Duration::from_secs(60));

        assert!(authorize_request(&registry, IssueChatMethod::ListIssues, &request).is_ok());
        assert!(authorize_request(&registry, IssueChatMethod::ListIssues, &request).is_err());
        assert!(authorize_request(&registry, IssueChatMethod::GetIssue, &request).is_err());
        let scoped = ListIssuesRequest {
            speech_id: "turn-1".into(),
            ..request
        };
        let scoped_digest = request_digest(IssueChatMethod::ListIssues, &scoped);
        registry.insert_until(scoped_digest, Instant::now() + Duration::from_secs(60));
        let other_turn = ListIssuesRequest {
            speech_id: "turn-2".into(),
            ..scoped.clone()
        };
        assert!(authorize_request(&registry, IssueChatMethod::ListIssues, &other_turn).is_err());
        assert!(authorize_request(&registry, IssueChatMethod::ListIssues, &scoped).is_ok());
    }

    #[test]
    fn direct_create_update_and_cancel_worker_calls_are_denied_without_a_permit() {
        let registry = PermissionRegistry::default();
        let create = CreateIssueRequest {
            speech_id: "speech-1".into(),
            tool_call_id: "call-1".into(),
            input_json: create_input_json(),
        };
        let update = UpdateIssueRequest {
            speech_id: "speech-1".into(),
            tool_call_id: "call-1".into(),
            input_json: update_input_json(),
        };
        let cancel = CancelIssueRequest {
            speech_id: "speech-1".into(),
            tool_call_id: "call-1".into(),
            input_json: cancel_input_json(),
        };

        for error in [
            authorize_request(&registry, IssueChatMethod::CreateIssue, &create).unwrap_err(),
            authorize_request(&registry, IssueChatMethod::UpdateIssue, &update).unwrap_err(),
            authorize_request(&registry, IssueChatMethod::CancelIssue, &cancel).unwrap_err(),
        ] {
            assert_eq!(error.code, IssueChatErrorCode::PermissionDenied as i32);
            assert!(!error.retryable);
        }
    }

    #[test]
    fn write_permits_bind_method_payload_and_injected_chat_context_and_are_one_shot() {
        let registry = PermissionRegistry::default();
        let request = CreateIssueRequest {
            speech_id: "speech-1".into(),
            tool_call_id: "call-1".into(),
            input_json: create_input_json(),
        };
        let digest = request_digest(IssueChatMethod::CreateIssue, &request);
        registry.insert_until(digest, Instant::now() + Duration::from_secs(60));

        assert!(authorize_request(&registry, IssueChatMethod::CreateIssue, &request).is_ok());
        assert!(authorize_request(&registry, IssueChatMethod::CreateIssue, &request).is_err());

        for changed in [
            CreateIssueRequest {
                speech_id: "other-speech".into(),
                ..request.clone()
            },
            CreateIssueRequest {
                tool_call_id: "other-call".into(),
                ..request.clone()
            },
            CreateIssueRequest {
                input_json: format!("{} ", request.input_json),
                ..request.clone()
            },
        ] {
            let other_registry = PermissionRegistry::default();
            other_registry.insert_until(digest, Instant::now() + Duration::from_secs(60));
            assert!(
                authorize_request(&other_registry, IssueChatMethod::CreateIssue, &changed).is_err()
            );
        }

        let other_registry = PermissionRegistry::default();
        other_registry.insert_until(digest, Instant::now() + Duration::from_secs(60));
        assert!(
            authorize_request(&other_registry, IssueChatMethod::UpdateIssue, &request).is_err()
        );

        let duplicate_registry = PermissionRegistry::default();
        assert!(duplicate_registry.grant(digest));
        assert!(!duplicate_registry.grant(digest));
    }

    #[test]
    fn expired_write_permits_are_rejected() {
        let registry = PermissionRegistry::default();
        let request = CancelIssueRequest {
            speech_id: "speech-1".into(),
            tool_call_id: "call-1".into(),
            input_json: cancel_input_json(),
        };
        let digest = request_digest(IssueChatMethod::CancelIssue, &request);
        registry.insert_until(digest, Instant::now() - Duration::from_secs(1));

        assert!(authorize_request(&registry, IssueChatMethod::CancelIssue, &request).is_err());
    }

    #[test]
    fn mutation_arguments_keep_context_out_of_llm_json_and_reject_unknown_or_reserved_fields() {
        let input = serde_json::json!({
            "actor_kind": "HUMAN",
            "title": "Improve login",
            "description": "Add passkey support",
            "kind": "TASK",
            "priority": "P2",
            "confirmation_status": "CONFIRMED",
            "rationale": "Requested explicitly"
        });
        let (request, worker_arguments) = prepare_issue_chat_mutation(
            IssueChatMethod::CreateIssue,
            &input,
            "speech-injected",
            "call-injected",
        )
        .expect("valid create arguments");
        let IssueChatRequest::Create(request) = request else {
            panic!("create method returns create request");
        };
        assert_eq!(request.speech_id, "speech-injected");
        assert_eq!(request.tool_call_id, "call-injected");
        assert_eq!(worker_arguments["speechId"], "speech-injected");
        assert_eq!(worker_arguments["toolCallId"], "call-injected");
        assert_eq!(worker_arguments["inputJson"], request.input_json);
        let payload: serde_json::Value =
            serde_json::from_str(&request.input_json).expect("inner tool payload JSON");
        assert!(payload.get("speech_id").is_none());
        assert!(payload.get("tool_call_id").is_none());

        for reserved in ["speech_id", "tool_call_id", "speechId", "toolCallId"] {
            let mut spoofed = input.clone();
            spoofed[reserved] = serde_json::json!("llm-value");
            assert!(
                prepare_issue_chat_mutation(
                    IssueChatMethod::CreateIssue,
                    &spoofed,
                    "speech-injected",
                    "call-injected",
                )
                .is_err()
            );
        }
        let mut unknown = input;
        unknown["actor_id"] = serde_json::json!("caller-selected");
        assert!(
            prepare_issue_chat_mutation(
                IssueChatMethod::CreateIssue,
                &unknown,
                "speech-injected",
                "call-injected",
            )
            .is_err()
        );
    }

    #[test]
    fn update_payload_preserves_empty_array_clears_and_rejects_unknown_patch_fields() {
        let input = serde_json::json!({
            "issue_id": "issue-1",
            "expected_revision": "18446744073709551615",
            "patch": { "labels": [] },
            "rationale": "Remove stale labels"
        });
        let (_, worker_arguments) =
            prepare_issue_chat_mutation(IssueChatMethod::UpdateIssue, &input, "speech-1", "call-1")
                .expect("empty labels are an explicit clear");
        let payload: serde_json::Value = serde_json::from_str(
            worker_arguments["inputJson"]
                .as_str()
                .expect("stringified update payload"),
        )
        .expect("valid update payload");
        assert_eq!(payload["patch"]["labels"], serde_json::json!([]));
        assert_eq!(payload["expected_revision"], "18446744073709551615");

        let mut unknown = input;
        unknown["patch"]["state"] = serde_json::json!("DONE");
        assert!(
            prepare_issue_chat_mutation(
                IssueChatMethod::UpdateIssue,
                &unknown,
                "speech-1",
                "call-1",
            )
            .is_err()
        );
    }

    #[test]
    fn mutation_input_size_limit_rejects_oversized_json() {
        let input = serde_json::json!({
            "actor_kind": "HUMAN",
            "title": "Issue title",
            "description": "Issue description",
            "kind": "TASK",
            "priority": "P2",
            "confirmation_status": "UNREVIEWED",
            "acceptance_criteria": vec!["x".repeat(4096); 20],
            "rationale": "Requested explicitly"
        });
        assert!(prepare_issue_chat_mutation(
            IssueChatMethod::CreateIssue,
            &input,
            "speech-1",
            "call-1",
        )
        .is_err());
    }

    #[test]
    fn cancel_payload_is_strict_and_unconfirmed_command_results_are_not_success() {
        let mut unknown = serde_json::from_str::<serde_json::Value>(&cancel_input_json()).unwrap();
        unknown["actor_id"] = serde_json::json!("caller-selected");
        assert!(
            prepare_issue_chat_mutation(
                IssueChatMethod::CancelIssue,
                &unknown,
                "speech-1",
                "call-1",
            )
            .is_err()
        );

        let pending = crate::commands::issue_chat_mutations::IssueChatMutationResult {
            issue_id: "issue-1".into(),
            operation_id: "operation-1".into(),
            operation_state: "PENDING".into(),
            revision: u64::MAX,
            resulting_state: None,
        };
        assert!(mutation_result_from_command(pending).is_err());

        let committed = crate::commands::issue_chat_mutations::IssueChatMutationResult {
            issue_id: "issue-1".into(),
            operation_id: "operation-1".into(),
            operation_state: "COMMITTED".into(),
            revision: u64::MAX,
            resulting_state: Some("CANCELED".into()),
        };
        let response = mutation_result_from_command(committed).unwrap();
        assert_eq!(response.revision, u64::MAX.to_string());
        assert_eq!(response.operation_state, "COMMITTED");
        assert_eq!(response.resulting_state.as_deref(), Some("CANCELED"));
    }

    #[test]
    fn list_and_get_reads_map_to_bounded_manager_requests_and_preserve_revision() {
        use crate::grpc::proto::issue_tracker::{manager::v1 as manager, v1 as issue};

        let normalized = normalize_list_request(ListIssuesRequest {
            page_size: 3,
            page_token: Some("opaque-next".into()),
            text_query: Some("  authentication  ".into()),
            states: vec![issue::IssueState::Open as i32],
            speech_id: "turn-1".into(),
        })
        .expect("valid list request");
        let manager_args = manager_list_args(normalized);
        assert_eq!(manager_args.page.as_ref().unwrap().page_size, 3);
        assert_eq!(
            manager_args.page.as_ref().unwrap().page_token.as_deref(),
            Some("opaque-next")
        );
        assert_eq!(
            manager_args
                .issue_filter
                .as_ref()
                .unwrap()
                .text_query
                .as_deref(),
            Some("authentication")
        );
        assert_eq!(manager_args.sort, issue::IssueSort::UpdatedDesc as i32);
        assert_eq!(
            manager_get_args(normalize_issue_id(" issue-42 ").unwrap()).issue_id,
            "issue-42"
        );

        let manager_issue = manager::IssueWithSync {
            issue: Some(issue::Issue {
                issue_id: "issue-42".into(),
                title: "Improve login".into(),
                description: "Add passkey support".into(),
                kind: issue::IssueKind::Task as i32,
                priority: issue::IssuePriority::P1 as i32,
                state: issue::IssueState::Open as i32,
                creation_basis: issue::CreationBasis::Explicit as i32,
                confirmation_status: issue::ConfirmationStatus::Unreviewed as i32,
                revision: 17,
                ..Default::default()
            }),
            sync: Some(manager::IssueSyncView {
                sync_state: manager::SyncState::Confirmed as i32,
                ..Default::default()
            }),
            lookback_ready: true,
            ..Default::default()
        };
        assert_eq!(
            issue_summary_from_sync(manager_issue.clone())
                .unwrap()
                .revision,
            17
        );
        let detail = issue_detail_from_sync(manager_issue).unwrap();
        assert_eq!(detail.confirmation_status, "UNREVIEWED");
        assert_eq!(detail.creation_basis, "EXPLICIT");
        assert_eq!(detail.revision, 17);
        assert_eq!(detail.issue_id, "issue-42");
        assert_eq!(detail.sync_state, "CONFIRMED");
    }

    #[test]
    fn expired_request_permits_are_rejected_and_removed() {
        let registry = PermissionRegistry::default();
        let request = ListIssuesRequest::default();
        let digest = request_digest(IssueChatMethod::ListIssues, &request);
        registry.insert_until(digest, Instant::now() - Duration::from_secs(1));

        assert!(authorize_request(&registry, IssueChatMethod::ListIssues, &request).is_err());
        assert!(authorize_request(&registry, IssueChatMethod::ListIssues, &request).is_err());
    }

    #[test]
    fn descriptor_exposes_chat_methods_and_safe_revision_types() {
        let pool = DescriptorPool::decode(LOOKBACK_CHAT_ISSUE_DESCRIPTOR_SET)
            .expect("generated descriptor set");
        let service = pool
            .get_service_by_name("lookback.chat.issue.v1.LookbackChatIssueService")
            .expect("chat Issue service descriptor");
        let mut methods: Vec<_> = service
            .methods()
            .map(|method| method.name().to_owned())
            .collect();
        methods.sort();
        assert_eq!(
            methods,
            [
                "CancelIssue",
                "CreateIssue",
                "GetIssue",
                "ListIssues",
                "UpdateIssue",
            ]
        );

        for message_name in [
            "lookback.chat.issue.v1.IssueSummary",
            "lookback.chat.issue.v1.IssueDetail",
        ] {
            let issue = pool
                .get_message_by_name(message_name)
                .expect("Issue DTO descriptor");
            assert_eq!(
                issue
                    .get_field_by_name("revision")
                    .expect("revision field")
                    .kind(),
                Kind::Uint64
            );
        }

        let mutation_result = pool
            .get_message_by_name("lookback.chat.issue.v1.IssueChatMutationResult")
            .expect("mutation result descriptor");
        assert_eq!(
            mutation_result
                .get_field_by_name("revision")
                .expect("revision field")
                .kind(),
            Kind::String
        );
    }

    #[test]
    fn worker_and_function_set_manifests_keep_issue_tools_chat_only() {
        let raw: serde_yaml::Value = serde_yaml::from_str(include_str!(
            "../../workers/lookback-issue-chat-workers.yaml"
        ))
        .expect("worker YAML remains valid before environment expansion");
        assert!(raw["workers"].as_sequence().is_some());
        let worker_yaml = include_str!("../../workers/lookback-issue-chat-workers.yaml")
            .replace("%{LOOKBACK_ISSUE_CHAT_WORKER_HOST}", "127.0.0.1")
            .replace("%{LOOKBACK_ISSUE_CHAT_WORKER_PORT}", "43123");
        let workers: serde_yaml::Value =
            serde_yaml::from_str(&worker_yaml).expect("chat Issue workers YAML");
        assert_eq!(
            workers["defaults"]["response_type"].as_str(),
            Some("DIRECT")
        );
        let worker_methods: Vec<_> = workers["workers"]
            .as_sequence()
            .expect("worker list")
            .iter()
            .map(|worker| {
                assert_eq!(worker["runner"].as_str(), Some("GRPC"));
                assert_eq!(worker["settings"]["use_reflection"].as_bool(), Some(true));
                worker["settings"]["method"]
                    .as_str()
                    .expect("fixed unary method")
                    .to_owned()
            })
            .collect();
        assert_eq!(
            worker_methods,
            [
                "lookback.chat.issue.v1.LookbackChatIssueService/ListIssues",
                "lookback.chat.issue.v1.LookbackChatIssueService/GetIssue",
                "lookback.chat.issue.v1.LookbackChatIssueService/CreateIssue",
                "lookback.chat.issue.v1.LookbackChatIssueService/UpdateIssue",
                "lookback.chat.issue.v1.LookbackChatIssueService/CancelIssue",
                "lookback.chat.issue.v1.LookbackCodingAgentService/ProposeExecution",
                "lookback.chat.issue.v1.LookbackCodingAgentService/GetExecution",
            ]
        );
        let worker_names: Vec<_> = workers["workers"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|worker| worker["name"].as_str().expect("worker name"))
            .collect();
        assert_eq!(
            worker_names,
            [
                "lookback_issue_list",
                "lookback_issue_get",
                "lookback_issue_create",
                "lookback_issue_update",
                "lookback_issue_cancel",
                "lookback_issue_agent_plan",
                "lookback_issue_agent_get",
            ]
        );
        for (worker, function_name) in workers["workers"]
            .as_sequence()
            .unwrap()
            .iter()
            .skip(2)
            .zip([
                "lookback_issue_create",
                "lookback_issue_update",
                "lookback_issue_cancel",
            ])
        {
            let description: serde_yaml::Value = serde_yaml::from_str(
                worker["description"]
                    .as_str()
                    .expect("jobworkerp description scalar"),
            )
            .expect("embedded jobworkerp description YAML");
            let parameters = description["parameters"]
                .as_mapping()
                .expect("manual mutation input schema");
            assert_eq!(parameters.len(), 1);
            assert!(parameters.contains_key(serde_yaml::Value::String("inputJson".into())));
            assert!(!parameters.contains_key(serde_yaml::Value::String("speechId".into())));
            assert!(!parameters.contains_key(serde_yaml::Value::String("toolCallId".into())));
            assert!(
                description["description"]
                    .as_str()
                    .unwrap()
                    .contains(function_name)
            );
        }

        let function_sets: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../../workers/function-sets.yaml"))
                .expect("function set YAML");
        let sets = function_sets["function_sets"].as_sequence().expect("sets");
        let targets = |name: &str| {
            sets.iter()
                .find(|set| set["name"].as_str() == Some(name))
                .and_then(|set| set["targets"].as_sequence())
                .unwrap_or_else(|| panic!("missing function set {name}"))
                .iter()
                .filter_map(|target| target["name"].as_str())
                .collect::<Vec<_>>()
        };
        let chat = targets("lookback-chat");
        assert_eq!(chat.len(), 10);
        for name in [
            "lookback_recall",
            "lookback_get_context",
            "lookback_get_reflections",
        ] {
            assert!(chat.contains(&name));
        }
        assert!(chat.contains(&"lookback_issue_list"));
        assert!(chat.contains(&"lookback_issue_get"));
        for name in [
            "lookback_issue_create",
            "lookback_issue_update",
            "lookback_issue_cancel",
        ] {
            assert!(chat.contains(&name));
            let target = sets
                .iter()
                .find(|set| set["name"].as_str() == Some("lookback-chat"))
                .and_then(|set| set["targets"].as_sequence())
                .unwrap()
                .iter()
                .find(|target| target["name"].as_str() == Some(name))
                .expect("chat mutation target");
            assert_eq!(target["using"].as_str(), Some("unary"));
        }
        for name in [
            "lookback_issue_list",
            "lookback_issue_get",
            "lookback_issue_create",
            "lookback_issue_update",
            "lookback_issue_cancel",
        ] {
            assert!(!targets("lookback-mcp").contains(&name));
            assert!(!targets("lookback-rag").contains(&name));
        }
        let mcp = targets("lookback-mcp");
        assert_eq!(
            mcp,
            [
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
            ]
        );
        assert_eq!(
            targets("lookback-rag"),
            [
                "lookback_recall",
                "lookback_get_context",
                "lookback_get_reflections",
            ]
        );
    }

    #[test]
    fn related_issue_page_must_match_detail_revision_before_returning_it() {
        let detail = IssueDetail {
            issue_id: "issue-1".into(),
            revision: 3,
            ..Default::default()
        };
        let relations = manager::IssueRelationsWithSync {
            issue: Some(manager::IssueWithSync {
                issue: Some(issue::Issue {
                    issue_id: "issue-1".into(),
                    revision: 3,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            relations: vec![issue::IssueRelation {
                relation_id: "rel-1".into(),
                source_issue_id: "issue-1".into(),
                target_issue_id: "issue-2".into(),
                kind: issue::RelationKind::Blocks as i32,
                ..Default::default()
            }],
            ..Default::default()
        };
        let actual = enrich_issue_detail_with_relations(detail.clone(), relations.clone()).unwrap();
        assert_eq!(actual.relations[0].target_issue_id, "issue-2");
        let mut stale = relations;
        stale
            .issue
            .as_mut()
            .unwrap()
            .issue
            .as_mut()
            .unwrap()
            .revision = 4;
        assert!(enrich_issue_detail_with_relations(detail, stale).is_err());
        assert!(
            validate_relation_page_token(Some(&"x".repeat(super::MAX_PAGE_TOKEN_BYTES + 1)))
                .is_err()
        );
        assert!(validate_relation_page_token(Some("next-page")).is_ok());
    }

    fn create_input_json() -> String {
        serde_json::json!({
            "actor_kind": "HUMAN",
            "title": "Improve login",
            "description": "Add passkey support",
            "kind": "TASK",
            "priority": "P2",
            "confirmation_status": "CONFIRMED",
            "rationale": "Requested explicitly"
        })
        .to_string()
    }

    fn update_input_json() -> String {
        serde_json::json!({
            "issue_id": "issue-1",
            "expected_revision": "0",
            "patch": { "description": "Updated" },
            "rationale": "Clarified in chat"
        })
        .to_string()
    }

    fn cancel_input_json() -> String {
        serde_json::json!({
            "issue_id": "issue-1",
            "expected_revision": "0",
            "classification": "OTHER",
            "rationale": "No longer needed"
        })
        .to_string()
    }
}
