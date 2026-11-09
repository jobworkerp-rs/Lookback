//! Approval-bound, tool-free semantic judge backed by jobworkerp's native LLM runner.
//!
//! Provider routing follows the application's effective external LLM settings.

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use jobworkerp_client::{
    client::wrapper::JobworkerpClientWrapper,
    command::job::JOB_ID_HEADER_NAME,
    jobworkerp::{
        data::{
            JobExecutionOverrides, JobId, QueueType, ResponseType, ResultStatus, RetryPolicy,
            RetryType, RunnerData, RunnerId, StreamingOutputType, Worker, WorkerData, WorkerId,
            result_output_item::Item,
        },
        service::{
            FindJobResultListRequest, JobRequest, RunnerNameRequest, WorkerNameRequest, job_request,
        },
    },
    proto::JobworkerpProto,
    stream_error::stream_error_from_trailer,
};
use prost::Message as _;
use prost_reflect::MessageDescriptor;
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::runtime::Handle;
use tonic::{Code, Request};

use crate::{
    coding_agent::assessment::{
        AssessmentClock, AssessmentEngine, AssessmentError, AssessmentOutcome,
        JudgeProvider as JudgeProviderTrait, JudgeProviderError, JudgeRequest,
        MAX_JUDGE_RESPONSE_BYTES, PreparedAssessment, ProviderSelection, ReceiptLookup,
    },
    coding_agent::model::AssessmentIntentClaim,
    coding_agent::store::RunStore,
    commands::llm_settings::{LlmMode, LlmSettings},
};

const RUNNER_NAME: &str = "LLM";
const CHAT_METHOD: &str = "chat";
const WORKER_NAME_PREFIX: &str = "lookback-coding-agent-judge";
const WORKER_CHANNEL: &str = "llm_external";
const MAX_RESULT_PROTO_BYTES: usize = 512 * 1024;
const MAX_STREAM_BYTES: usize = 1024 * 1024;
const RESULT_LOOKUP_POLL_INTERVAL: Duration = Duration::from_millis(250);
const MAX_PROVIDER_OPERATION_TIMEOUT: Duration = Duration::from_secs(u32::MAX as u64);

#[derive(Clone, Debug)]
pub struct ApprovedJudgeProviderProfile {
    selection: ProviderSelection,
    model_id: String,
    base_url: Option<String>,
    worker_name: String,
    deadline_unix_ms: u64,
}

impl ApprovedJudgeProviderProfile {
    /// Construct a provider profile only when current non-secret settings match
    /// the exact provider selection frozen into the approval snapshot.
    pub fn from_settings(
        settings: &LlmSettings,
        approved_selection: ProviderSelection,
        deadline_unix_ms: u64,
    ) -> Result<Self, JudgeProviderConfigError> {
        validate_approved_selection(settings, &approved_selection)?;
        if deadline_unix_ms == 0 {
            return Err(JudgeProviderConfigError::InvalidDeadline);
        }
        let model_id = settings
            .provider_model
            .as_deref()
            .ok_or(JudgeProviderConfigError::InvalidModel)?
            .to_owned();
        let base_url = checked_base_url(settings.base_url.as_deref())?;
        let worker_name = format!("{WORKER_NAME_PREFIX}-{}", approved_selection.config_version);
        Ok(Self {
            selection: approved_selection,
            model_id,
            base_url,
            worker_name,
            deadline_unix_ms,
        })
    }

    pub fn selection(&self) -> &ProviderSelection {
        &self.selection
    }

    /// This digest covers the exact non-secret runner settings and jobworkerp
    /// worker policy; it is not an attestation of provider-side model weights.
    pub fn worker_name(&self) -> &str {
        &self.worker_name
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum JudgeProviderConfigError {
    #[error("this judge adapter requires external LLM settings")]
    UnsupportedProvider,
    #[error("selected model identifier is missing or invalid")]
    InvalidModel,
    #[error("configured provider selection differs from the approved selection")]
    SelectionMismatch,
    #[error("external LLM base URL is invalid or contains credential-bearing components")]
    UnsafeBaseUrl,
    #[error("assessment deadline is invalid or has elapsed")]
    InvalidDeadline,
}

#[derive(Debug, Error)]
pub enum JobworkerpJudgeError {
    #[error(transparent)]
    Profile(#[from] JudgeProviderConfigError),
    #[error("unable to connect to the configured jobworkerp endpoint")]
    Connect,
    #[error("jobworkerp {operation} RPC failed ({code:?})")]
    Rpc { operation: &'static str, code: Code },
    #[error("jobworkerp does not expose a compatible LLM chat runner schema")]
    UnsupportedRunnerSchema,
    #[error("the registered judge worker name is already owned by different settings")]
    WorkerNameConflict,
    #[error("jobworkerp returned an invalid judge worker registration")]
    InvalidWorker,
    #[error("the assessment engine failed")]
    Assessment(#[source] AssessmentError),
    #[error("the blocking assessment task did not complete")]
    BlockingTask,
}

#[derive(Serialize)]
struct CanonicalWorkerProfile<'a> {
    schema_version: u8,
    runner: &'static str,
    provider_id: &'a str,
    model_id: &'a str,
    model_version: &'a str,
    base_url: Option<&'a str>,
    channel: &'static str,
    queue_type: &'static str,
    response_type: &'static str,
    store_success: bool,
    store_failure: bool,
    use_static: bool,
    broadcast_results: bool,
    retries: &'static str,
}

/// Derive the only provider-selection values that this native adapter can
/// verify from the effective settings. The selected model identifier binds
/// configuration; it does not attest to immutable provider-side weights.
pub fn provider_selection_for_settings(
    settings: &LlmSettings,
) -> Result<ProviderSelection, JudgeProviderConfigError> {
    if settings.mode != LlmMode::External {
        return Err(JudgeProviderConfigError::UnsupportedProvider);
    }
    let model_id = settings
        .provider_model
        .as_deref()
        .ok_or(JudgeProviderConfigError::InvalidModel)?;
    if !valid_model_identifier(model_id) {
        return Err(JudgeProviderConfigError::InvalidModel);
    }
    let base_url = checked_base_url(settings.base_url.as_deref())?;
    let provider_id = crate::commands::llm_settings::detect_provider(model_id, base_url.as_deref())
        .env_var()
        .trim_end_matches("_API_KEY")
        .to_ascii_lowercase();
    let material = CanonicalWorkerProfile {
        schema_version: 1,
        runner: RUNNER_NAME,
        provider_id: &provider_id,
        model_id,
        model_version: model_id,
        base_url: base_url.as_deref(),
        channel: WORKER_CHANNEL,
        queue_type: "NORMAL",
        response_type: "DIRECT",
        store_success: true,
        store_failure: true,
        use_static: false,
        broadcast_results: true,
        retries: "NONE",
    };
    let bytes =
        serde_json::to_vec(&material).map_err(|_| JudgeProviderConfigError::SelectionMismatch)?;
    let config_version = format!("{:x}", Sha256::digest(bytes));
    Ok(ProviderSelection {
        provider_id,
        model_id: model_id.to_owned(),
        model_version: model_id.to_owned(),
        config_version,
    })
}

pub fn validate_approved_selection(
    settings: &LlmSettings,
    approved_selection: &ProviderSelection,
) -> Result<(), JudgeProviderConfigError> {
    let actual = provider_selection_for_settings(settings)?;
    if actual == *approved_selection {
        Ok(())
    } else {
        Err(JudgeProviderConfigError::SelectionMismatch)
    }
}

/// Build the exact two-message LLMChatArgs JSON shape. No application chat
/// system prompt, date, tool definition, or provider steering is added.
pub fn judge_chat_args(
    system_policy_bytes: &[u8],
    input_bytes: &[u8],
) -> Result<serde_json::Value, JudgeProviderError> {
    let system = std::str::from_utf8(system_policy_bytes)
        .map_err(|_| JudgeProviderError::TransportFailure)?;
    let user =
        std::str::from_utf8(input_bytes).map_err(|_| JudgeProviderError::TransportFailure)?;
    Ok(serde_json::json!({
        "messages": [
            {"role": "SYSTEM", "content": {"text": system}},
            {"role": "USER", "content": {"text": user}},
        ]
    }))
}

/// Decode only a completed, text-only LlmChatResult. A result asking for any
/// tool execution is not suitable for an acceptance judge.
pub fn decode_terminal_chat_text(bytes: &[u8]) -> Result<Vec<u8>, JudgeProviderError> {
    let chunk = decode_chat_result(bytes)?;
    if !chunk.done || has_tool_activity(&chunk) {
        return Err(JudgeProviderError::TransportFailure);
    }
    extract_text(&chunk)
        .filter(|text| text.len() <= MAX_JUDGE_RESPONSE_BYTES)
        .map(String::into_bytes)
        .ok_or(JudgeProviderError::TransportFailure)
}

/// jobworkerp unique enqueue keys only suppress overlapping jobs. They are
/// released after completion, so the judge must never replay on a missing row.
pub fn same_key_idempotent_replay_is_supported() -> bool {
    false
}

#[derive(Clone, Debug)]
pub struct ReceiptBinding {
    pub worker_id: i64,
    pub worker_name: String,
    pub uniq_key: String,
    pub expected_args: Vec<u8>,
    pub job_id: Option<i64>,
}

impl ReceiptBinding {
    pub fn matches_result_envelope(
        &self,
        worker_id: i64,
        worker_name: &str,
        uniq_key: &str,
        args: &[u8],
        job_id: i64,
    ) -> bool {
        self.worker_id == worker_id
            && self.worker_name == worker_name
            && self.uniq_key == uniq_key
            && self.expected_args == args
            && self.job_id.is_none_or(|expected| expected == job_id)
    }
}

struct RunnerSnapshot {
    id: RunnerId,
    data: RunnerData,
    args_descriptor: MessageDescriptor,
}

struct RequestBinding {
    assessment_id: String,
    input_digest: String,
    system_policy_bytes: Vec<u8>,
    input_bytes: Vec<u8>,
    uniq_key: String,
    args: Vec<u8>,
    job_id: Option<i64>,
}

impl RequestBinding {
    fn from_prepared(
        prepared: &PreparedAssessment,
        selection: &ProviderSelection,
        args_descriptor: MessageDescriptor,
    ) -> Result<Self, JobworkerpJudgeError> {
        validate_prepared_selection(prepared, selection)?;

        let args_json = judge_chat_args(prepared.system_policy_bytes(), prepared.input_bytes())
            .map_err(|_| JudgeProviderConfigError::SelectionMismatch)?;
        let args = JobworkerpProto::json_value_to_message(args_descriptor, &args_json, true, false)
            .map_err(|_| JobworkerpJudgeError::UnsupportedRunnerSchema)?;
        Ok(Self {
            assessment_id: prepared.assessment_id().to_owned(),
            input_digest: prepared.judge_input_digest().to_owned(),
            system_policy_bytes: prepared.system_policy_bytes().to_vec(),
            input_bytes: prepared.input_bytes().to_vec(),
            uniq_key: assessment_unique_key(
                prepared.assessment_id(),
                prepared.judge_input_digest(),
            ),
            args,
            job_id: None,
        })
    }

    fn receipt_binding(&self, worker: &RegisteredWorker) -> ReceiptBinding {
        ReceiptBinding {
            worker_id: worker.id.value,
            worker_name: worker.data.name.clone(),
            uniq_key: self.uniq_key.clone(),
            expected_args: self.args.clone(),
            job_id: self.job_id,
        }
    }

    fn matches_request(&self, request: &JudgeRequest<'_>) -> bool {
        request.assessment_id() == self.assessment_id
            && request.input_digest() == self.input_digest
            && request.idempotency_key() == self.uniq_key
            && request.input_bytes() == self.input_bytes
            && request.system_policy_bytes() == self.system_policy_bytes
    }
}

fn validate_prepared_selection(
    prepared: &PreparedAssessment,
    selection: &ProviderSelection,
) -> Result<(), JobworkerpJudgeError> {
    let input: serde_json::Value = serde_json::from_slice(prepared.input_bytes())
        .map_err(|_| JudgeProviderConfigError::SelectionMismatch)?;
    let embedded_selection: ProviderSelection = serde_json::from_value(
        input
            .get("provider")
            .cloned()
            .ok_or(JudgeProviderConfigError::SelectionMismatch)?,
    )
    .map_err(|_| JudgeProviderConfigError::SelectionMismatch)?;
    if embedded_selection == *selection {
        Ok(())
    } else {
        Err(JudgeProviderConfigError::SelectionMismatch.into())
    }
}

#[derive(Clone)]
struct RegisteredWorker {
    id: WorkerId,
    data: WorkerData,
}

struct ConnectedJudgeProvider {
    client: JobworkerpClientWrapper,
    runner: RunnerSnapshot,
    worker: RegisteredWorker,
    binding: RequestBinding,
}

struct JobworkerpJudgeProvider {
    endpoint: String,
    runtime: Handle,
    profile: ApprovedJudgeProviderProfile,
    prepared: PreparedAssessment,
    connected: Option<ConnectedJudgeProvider>,
    dispatch_started: bool,
}

impl JobworkerpJudgeProvider {
    fn new(
        endpoint: &str,
        profile: ApprovedJudgeProviderProfile,
        prepared: &PreparedAssessment,
    ) -> Result<Self, JobworkerpJudgeError> {
        validate_prepared_selection(prepared, &profile.selection)?;
        Ok(Self {
            endpoint: endpoint.to_owned(),
            runtime: Handle::current(),
            profile,
            prepared: prepared.clone(),
            connected: None,
            dispatch_started: false,
        })
    }

    async fn connect(
        endpoint: &str,
        profile: &ApprovedJudgeProviderProfile,
        prepared: &PreparedAssessment,
    ) -> Result<ConnectedJudgeProvider, JobworkerpJudgeError> {
        let request_timeout = request_timeout_seconds(profile.deadline_unix_ms);
        let client = JobworkerpClientWrapper::new(endpoint, Some(request_timeout))
            .await
            .map_err(|_| JobworkerpJudgeError::Connect)?;
        let runner = resolve_runner(&client).await?;
        let binding = RequestBinding::from_prepared(
            prepared,
            &profile.selection,
            runner.args_descriptor.clone(),
        )?;
        let worker = register_snapshot_worker(&client, profile, &runner).await?;
        Ok(ConnectedJudgeProvider {
            client,
            runner,
            worker,
            binding,
        })
    }

    fn ensure_connected(&mut self) -> Result<(), JudgeProviderError> {
        if self.connected.is_some() {
            return Ok(());
        }
        if current_unix_ms() >= self.profile.deadline_unix_ms {
            return Err(JudgeProviderError::TransportFailure);
        }
        let endpoint = self.endpoint.clone();
        let profile = self.profile.clone();
        let prepared = self.prepared.clone();
        let connected = self
            .runtime
            .block_on(Self::connect(&endpoint, &profile, &prepared))
            .map_err(|_| JudgeProviderError::TransportFailure)?;
        self.connected = Some(connected);
        Ok(())
    }

    fn complete_once(&mut self) -> Result<Vec<u8>, JudgeProviderError> {
        if self.dispatch_started || current_unix_ms() >= self.profile.deadline_unix_ms {
            return Err(JudgeProviderError::TransportFailure);
        }
        self.dispatch_started = true;
        let connected = self
            .connected
            .as_mut()
            .ok_or(JudgeProviderError::TransportFailure)?;
        let client = connected.client.clone();
        let profile = self.profile.clone();
        let worker = &connected.worker;
        let binding = &mut connected.binding;
        let timeout_ms = profile
            .deadline_unix_ms
            .saturating_sub(current_unix_ms())
            .min(MAX_PROVIDER_OPERATION_TIMEOUT.as_millis() as u64);
        if timeout_ms == 0 {
            return Err(JudgeProviderError::TransportFailure);
        }
        let job_request = JobRequest {
            worker: Some(job_request::Worker::WorkerId(worker.id)),
            args: binding.args.clone(),
            uniq_key: Some(binding.uniq_key.clone()),
            timeout: Some(timeout_ms),
            using: Some(CHAT_METHOD.to_owned()),
            overrides: Some(JobExecutionOverrides {
                response_type: Some(ResponseType::Direct as i32),
                store_success: Some(true),
                store_failure: Some(true),
                broadcast_results: Some(true),
                retry_policy: Some(no_retry_policy()),
            }),
            ..Default::default()
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let response = self.runtime.block_on(async {
            tokio::time::timeout_at(deadline.into(), async {
                client
                    .jobworkerp_client
                    .job_client()
                    .await
                    .max_decoding_message_size(MAX_STREAM_BYTES)
                    .enqueue_for_stream(Request::new(job_request))
                    .await
                    .map_err(|_| JudgeProviderError::TransportFailure)
            })
            .await
            .map_err(|_| JudgeProviderError::TransportFailure)?
        })?;
        let job_id = response
            .metadata()
            .get_bin(JOB_ID_HEADER_NAME)
            .and_then(|value| value.to_bytes().ok())
            .and_then(|bytes| JobId::decode(bytes.as_ref()).ok())
            .filter(|id| id.value > 0)
            .ok_or(JudgeProviderError::TransportFailure)?;
        binding.job_id = Some(job_id.value);

        let mut stream = response.into_inner();
        let remaining_ms = profile
            .deadline_unix_ms
            .saturating_sub(current_unix_ms())
            .min(MAX_PROVIDER_OPERATION_TIMEOUT.as_millis() as u64);
        if remaining_ms == 0 {
            return Err(JudgeProviderError::TransportFailure);
        }
        let text = self.runtime.block_on(async move {
            tokio::time::timeout(
                Duration::from_millis(remaining_ms),
                collect_judge_stream(&mut stream),
            )
            .await
            .map_err(|_| JudgeProviderError::TransportFailure)?
        })?;
        Ok(text)
    }

    fn lookup(&mut self, assessment_id: &str, input_digest: &str, key: &str) -> ReceiptLookup {
        let expected_key = assessment_unique_key(
            self.prepared.assessment_id(),
            self.prepared.judge_input_digest(),
        );
        if assessment_id != self.prepared.assessment_id()
            || input_digest != self.prepared.judge_input_digest()
            || key != expected_key
        {
            return ReceiptLookup::Unsupported;
        }
        if current_unix_ms() >= self.profile.deadline_unix_ms || self.ensure_connected().is_err() {
            return ReceiptLookup::Unsupported;
        }
        let Some(connected) = self.connected.as_ref() else {
            return ReceiptLookup::Unsupported;
        };
        let client = connected.client.clone();
        let expected_worker = connected.worker.clone();
        let runner_id = connected.runner.id;
        let runner_fingerprint = runner_fingerprint(&connected.runner.data);
        let verified = self.runtime.block_on(async {
            verify_registered_worker(&client, &expected_worker).await?;
            read_runner_fingerprint(&client, runner_id).await
        });
        if !matches!(verified, Ok(actual) if actual == runner_fingerprint) {
            return ReceiptLookup::Unsupported;
        }
        let receipt = connected.binding.receipt_binding(&connected.worker);
        let deadline = self.profile.deadline_unix_ms;
        self.runtime.block_on(async move {
            let mut first_query = true;
            loop {
                let matches = match find_job_results(&client, &receipt).await {
                    Ok(results) => results,
                    Err(()) => return ReceiptLookup::Unsupported,
                };
                match matches.as_slice() {
                    [result] => {
                        let Some(data) = result.data.as_ref() else {
                            return ReceiptLookup::Unsupported;
                        };
                        if data.status != ResultStatus::Success as i32
                            || data
                                .worker_id
                                .as_ref()
                                .is_none_or(|id| id.value != receipt.worker_id)
                            || data.worker_name != receipt.worker_name
                            || data.uniq_key.as_deref() != Some(receipt.uniq_key.as_str())
                            || data.args != receipt.expected_args
                            || data.using.as_deref() != Some(CHAT_METHOD)
                            || data.response_type != ResponseType::Direct as i32
                            || !data.store_success
                            || !data.store_failure
                            || !data.broadcast_results
                            || data.job_id.as_ref().is_none_or(|id| {
                                id.value <= 0
                                    || receipt.job_id.is_some_and(|known| known != id.value)
                            })
                        {
                            return ReceiptLookup::Unsupported;
                        }
                        let Some(output) = data.output.as_ref() else {
                            return ReceiptLookup::Unsupported;
                        };
                        let Ok(text) = decode_terminal_chat_text(&output.items) else {
                            return ReceiptLookup::Unsupported;
                        };
                        return ReceiptLookup::Found(text);
                    }
                    [] => {}
                    _ => return ReceiptLookup::Unsupported,
                }
                if !first_query && current_unix_ms() >= deadline {
                    return ReceiptLookup::NotFound;
                }
                first_query = false;
                let remaining_ms = deadline.saturating_sub(current_unix_ms());
                if remaining_ms == 0 {
                    return ReceiptLookup::NotFound;
                }
                tokio::time::sleep(
                    RESULT_LOOKUP_POLL_INTERVAL.min(Duration::from_millis(remaining_ms)),
                )
                .await;
            }
        })
    }
}

impl JudgeProviderTrait for JobworkerpJudgeProvider {
    fn complete_tool_free(
        &mut self,
        request: &JudgeRequest<'_>,
    ) -> Result<Vec<u8>, JudgeProviderError> {
        if request.provider() != &self.profile.selection
            || request.deadline_unix_ms() != self.profile.deadline_unix_ms
            || request.assessment_id() != self.prepared.assessment_id()
            || request.input_digest() != self.prepared.judge_input_digest()
            || request.input_bytes() != self.prepared.input_bytes()
            || request.system_policy_bytes() != self.prepared.system_policy_bytes()
            || current_unix_ms() >= self.profile.deadline_unix_ms
            || request.idempotency_key()
                != assessment_unique_key(
                    self.prepared.assessment_id(),
                    self.prepared.judge_input_digest(),
                )
        {
            return Err(JudgeProviderError::TransportFailure);
        }
        self.ensure_connected()?;
        if current_unix_ms() >= self.profile.deadline_unix_ms {
            return Err(JudgeProviderError::TransportFailure);
        }
        let Some(connected) = self.connected.as_ref() else {
            return Err(JudgeProviderError::TransportFailure);
        };
        if !connected.binding.matches_request(request) {
            return Err(JudgeProviderError::TransportFailure);
        }
        let client = connected.client.clone();
        let worker = connected.worker.clone();
        let runner_id = connected.runner.id;
        let runner_fingerprint = runner_fingerprint(&connected.runner.data);
        let current_fingerprint = self.runtime.block_on(async {
            verify_registered_worker(&client, &worker).await?;
            read_runner_fingerprint(&client, runner_id).await
        });
        let Ok(actual_fingerprint) = current_fingerprint else {
            return Err(JudgeProviderError::TransportFailure);
        };
        if actual_fingerprint != runner_fingerprint {
            return Err(JudgeProviderError::TransportFailure);
        }
        self.complete_once()
    }

    fn lookup_receipt(
        &mut self,
        assessment_id: &str,
        input_digest: &str,
        idempotency_key: &str,
    ) -> Result<ReceiptLookup, JudgeProviderError> {
        Ok(self.lookup(assessment_id, input_digest, idempotency_key))
    }

    fn supports_same_key_idempotent_replay(&self) -> bool {
        same_key_idempotent_replay_is_supported()
    }
}

/// Run the existing synchronous engine without blocking the Tauri/Tokio async
/// executor. The caller supplies the explicit endpoint and the same prepared
/// input so receipt recovery can bind to the exact system and user messages.
pub async fn assess_with_jobworkerp<C>(
    endpoint: &str,
    profile: ApprovedJudgeProviderProfile,
    engine: AssessmentEngine,
    prepared: PreparedAssessment,
    clock: C,
) -> Result<AssessmentOutcome, JobworkerpJudgeError>
where
    C: AssessmentClock + Send + Sync + 'static,
{
    let mut provider = JobworkerpJudgeProvider::new(endpoint, profile, &prepared)?;
    tokio::task::spawn_blocking(move || engine.assess(&prepared, &mut provider, &clock))
        .await
        .map_err(|_| JobworkerpJudgeError::BlockingTask)?
        .map_err(JobworkerpJudgeError::Assessment)
}

/// Resume from the recomputed approved input. The durable engine still owns
/// the stored intent and response; this bridge only locates an exact matching
/// jobworkerp result and never resubmits a missing key.
pub async fn resume_with_jobworkerp<C>(
    endpoint: &str,
    profile: ApprovedJudgeProviderProfile,
    engine: AssessmentEngine,
    prepared: PreparedAssessment,
    clock: C,
) -> Result<AssessmentOutcome, JobworkerpJudgeError>
where
    C: AssessmentClock + Send + Sync + 'static,
{
    let assessment_id = prepared.assessment_id().to_owned();
    let mut provider = JobworkerpJudgeProvider::new(endpoint, profile, &prepared)?;
    tokio::task::spawn_blocking(move || engine.resume(&assessment_id, &mut provider, &clock))
        .await
        .map_err(|_| JobworkerpJudgeError::BlockingTask)?
        .map_err(JobworkerpJudgeError::Assessment)
}

/// Dispatch a newly-created coordinator claim exactly once. Existing claims
/// are recovery-only and use the assessment engine's lookup/final path, which
/// cannot enqueue another provider request.
pub async fn assess_claimed_with_jobworkerp<C>(
    endpoint: &str,
    profile: ApprovedJudgeProviderProfile,
    engine: Arc<AssessmentEngine>,
    prepared: PreparedAssessment,
    run_store: Arc<RunStore>,
    claim: AssessmentIntentClaim,
    clock: C,
) -> Result<AssessmentOutcome, JobworkerpJudgeError>
where
    C: AssessmentClock + Send + Sync + 'static,
{
    let mut provider = JobworkerpJudgeProvider::new(endpoint, profile, &prepared)?;
    tokio::task::spawn_blocking(move || {
        if claim.newly_created {
            engine.assess_with_run_store_claim(&prepared, &mut provider, &clock, &run_store, &claim)
        } else {
            engine.resume_with_run_store_binding(
                prepared.assessment_id(),
                &mut provider,
                &clock,
                &run_store,
                &claim.binding,
            )
        }
    })
    .await
    .map_err(|_| JobworkerpJudgeError::BlockingTask)?
    .map_err(JobworkerpJudgeError::Assessment)
}

async fn resolve_runner(
    client: &JobworkerpClientWrapper,
) -> Result<RunnerSnapshot, JobworkerpJudgeError> {
    let runner = client
        .jobworkerp_client
        .runner_client()
        .await
        .find_by_name(RunnerNameRequest {
            name: RUNNER_NAME.to_owned(),
        })
        .await
        .map_err(|status| rpc_error("find runner", status.code()))?
        .into_inner()
        .data
        .ok_or(JobworkerpJudgeError::UnsupportedRunnerSchema)?;
    let id = runner
        .id
        .filter(|id| id.value > 0)
        .ok_or(JobworkerpJudgeError::UnsupportedRunnerSchema)?;
    let data = runner
        .data
        .ok_or(JobworkerpJudgeError::UnsupportedRunnerSchema)?;
    if data.name != RUNNER_NAME {
        return Err(JobworkerpJudgeError::UnsupportedRunnerSchema);
    }
    let chat = data
        .method_proto_map
        .as_ref()
        .and_then(|methods| methods.schemas.get(CHAT_METHOD))
        .ok_or(JobworkerpJudgeError::UnsupportedRunnerSchema)?;
    if chat.require_client_stream
        || !matches!(
            StreamingOutputType::try_from(chat.output_type),
            Ok(StreamingOutputType::Streaming | StreamingOutputType::Both)
        )
    {
        return Err(JobworkerpJudgeError::UnsupportedRunnerSchema);
    }
    let args_descriptor =
        JobworkerpProto::parse_job_args_schema_descriptor(&data, Some(CHAT_METHOD))
            .map_err(|_| JobworkerpJudgeError::UnsupportedRunnerSchema)?
            .filter(|descriptor| descriptor.name() == "LLMChatArgs")
            .ok_or(JobworkerpJudgeError::UnsupportedRunnerSchema)?;
    JobworkerpProto::parse_result_schema_descriptor(&data, Some(CHAT_METHOD))
        .map_err(|_| JobworkerpJudgeError::UnsupportedRunnerSchema)?
        .filter(|descriptor| descriptor.name() == "LLMChatResult")
        .ok_or(JobworkerpJudgeError::UnsupportedRunnerSchema)?;
    let settings_descriptor = JobworkerpProto::parse_runner_settings_schema_descriptor(&data)
        .map_err(|_| JobworkerpJudgeError::UnsupportedRunnerSchema)?
        .ok_or(JobworkerpJudgeError::UnsupportedRunnerSchema)?;
    if settings_descriptor.get_field_by_name("genai").is_none()
        || args_descriptor.get_field_by_name("messages").is_none()
    {
        return Err(JobworkerpJudgeError::UnsupportedRunnerSchema);
    }
    Ok(RunnerSnapshot {
        id,
        data,
        args_descriptor,
    })
}

async fn register_snapshot_worker(
    client: &JobworkerpClientWrapper,
    profile: &ApprovedJudgeProviderProfile,
    runner: &RunnerSnapshot,
) -> Result<RegisteredWorker, JobworkerpJudgeError> {
    let mut settings = serde_json::Map::new();
    settings.insert("model".to_owned(), profile.model_id.clone().into());
    if let Some(base_url) = profile.base_url.as_ref() {
        settings.insert("base_url".to_owned(), base_url.clone().into());
    }
    let settings_json = serde_json::json!({ "genai": settings });
    let settings_descriptor =
        JobworkerpProto::parse_runner_settings_schema_descriptor(&runner.data)
            .map_err(|_| JobworkerpJudgeError::UnsupportedRunnerSchema)?
            .ok_or(JobworkerpJudgeError::UnsupportedRunnerSchema)?;
    let runner_settings =
        JobworkerpProto::json_value_to_message(settings_descriptor, &settings_json, true, false)
            .map_err(|_| JobworkerpJudgeError::UnsupportedRunnerSchema)?;
    let candidate = WorkerData {
        name: profile.worker_name.clone(),
        description: "Lookback approval-bound semantic judge snapshot".to_owned(),
        runner_id: Some(runner.id),
        runner_settings,
        retry_policy: Some(RetryPolicy {
            r#type: RetryType::None as i32,
            basis: 1.0,
            ..Default::default()
        }),
        periodic_interval: 0,
        channel: Some(WORKER_CHANNEL.to_owned()),
        queue_type: QueueType::Normal as i32,
        response_type: ResponseType::Direct as i32,
        store_success: true,
        store_failure: true,
        use_static: false,
        broadcast_results: true,
    };

    let found = find_worker(client, &candidate.name).await?;
    if found.is_none() {
        let create_result = client
            .jobworkerp_client
            .worker_client()
            .await
            .create(candidate.clone())
            .await;
        if let Err(status) = create_result
            && status.code() != Code::AlreadyExists
        {
            return Err(rpc_error("create judge worker", status.code()));
        }
    }
    let actual = find_worker(client, &candidate.name)
        .await?
        .ok_or(JobworkerpJudgeError::InvalidWorker)?;
    let id = actual
        .id
        .filter(|id| id.value > 0)
        .ok_or(JobworkerpJudgeError::InvalidWorker)?;
    let data = actual.data.ok_or(JobworkerpJudgeError::InvalidWorker)?;
    if !same_worker_configuration(&candidate, &data) {
        return Err(JobworkerpJudgeError::WorkerNameConflict);
    }
    Ok(RegisteredWorker { id, data })
}

async fn find_worker(
    client: &JobworkerpClientWrapper,
    name: &str,
) -> Result<Option<Worker>, JobworkerpJudgeError> {
    client
        .jobworkerp_client
        .worker_client()
        .await
        .find_by_name(WorkerNameRequest {
            name: name.to_owned(),
        })
        .await
        .map(|response| response.into_inner().data)
        .map_err(|status| rpc_error("find judge worker", status.code()))
}

async fn verify_registered_worker(
    client: &JobworkerpClientWrapper,
    expected: &RegisteredWorker,
) -> Result<(), JobworkerpJudgeError> {
    let actual = find_worker(client, &expected.data.name)
        .await?
        .ok_or(JobworkerpJudgeError::InvalidWorker)?;
    let id = actual.id.ok_or(JobworkerpJudgeError::InvalidWorker)?;
    let data = actual.data.ok_or(JobworkerpJudgeError::InvalidWorker)?;
    if id.value <= 0
        || id.value != expected.id.value
        || !same_worker_configuration(&expected.data, &data)
    {
        return Err(JobworkerpJudgeError::WorkerNameConflict);
    }
    Ok(())
}

fn same_worker_configuration(expected: &WorkerData, actual: &WorkerData) -> bool {
    expected.name == actual.name
        && expected.description == actual.description
        && expected.runner_id == actual.runner_id
        && expected.runner_settings == actual.runner_settings
        && expected.retry_policy == actual.retry_policy
        && expected.periodic_interval == actual.periodic_interval
        && expected.channel == actual.channel
        && expected.queue_type == actual.queue_type
        && expected.response_type == actual.response_type
        && expected.store_success == actual.store_success
        && expected.store_failure == actual.store_failure
        && expected.use_static == actual.use_static
        && expected.broadcast_results == actual.broadcast_results
}

async fn read_runner_fingerprint(
    client: &JobworkerpClientWrapper,
    runner_id: RunnerId,
) -> Result<String, JobworkerpJudgeError> {
    let runner = client
        .jobworkerp_client
        .runner_client()
        .await
        .find(runner_id)
        .await
        .map_err(|status| rpc_error("verify runner", status.code()))?
        .into_inner()
        .data
        .ok_or(JobworkerpJudgeError::UnsupportedRunnerSchema)?;
    if runner
        .id
        .as_ref()
        .is_none_or(|id| id.value != runner_id.value)
    {
        return Err(JobworkerpJudgeError::UnsupportedRunnerSchema);
    }
    let runner = runner
        .data
        .ok_or(JobworkerpJudgeError::UnsupportedRunnerSchema)?;
    if runner.name != RUNNER_NAME {
        return Err(JobworkerpJudgeError::UnsupportedRunnerSchema);
    }
    Ok(runner_fingerprint(&runner))
}

fn runner_fingerprint(runner: &RunnerData) -> String {
    let chat = runner
        .method_proto_map
        .as_ref()
        .and_then(|methods| methods.schemas.get(CHAT_METHOD));
    let fields = (
        runner.name.as_str(),
        runner.runner_type,
        runner.definition.as_str(),
        runner.runner_settings_proto.as_str(),
        chat.map(|method| method.args_proto.as_str()),
        chat.map(|method| method.result_proto.as_str()),
        chat.map(|method| method.output_type),
        chat.map(|method| method.require_client_stream),
    );
    let bytes = serde_json::to_vec(&fields).unwrap_or_default();
    format!("{:x}", Sha256::digest(bytes))
}

async fn find_job_results(
    client: &JobworkerpClientWrapper,
    binding: &ReceiptBinding,
) -> Result<Vec<jobworkerp_client::jobworkerp::data::JobResult>, ()> {
    let request = FindJobResultListRequest {
        worker_ids: vec![WorkerId {
            value: binding.worker_id,
        }],
        statuses: Vec::new(),
        start_time_from: None,
        start_time_to: None,
        end_time_from: None,
        end_time_to: None,
        priorities: Vec::new(),
        uniq_key: Some(binding.uniq_key.clone()),
        limit: Some(2),
        offset: Some(0),
        sort_by: None,
        ascending: Some(false),
    };
    let mut results = client
        .jobworkerp_client
        .job_result_client()
        .await
        .max_decoding_message_size(MAX_RESULT_PROTO_BYTES)
        .find_list_by(Request::new(request))
        .await
        .map_err(|_| ())?
        .into_inner();
    let mut found = Vec::with_capacity(2);
    while let Some(result) = results.message().await.map_err(|_| ())? {
        found.push(result);
        if found.len() == 2 {
            break;
        }
    }
    Ok(found)
}

pub(crate) async fn collect_judge_stream(
    stream: &mut tonic::Streaming<jobworkerp_client::jobworkerp::data::ResultOutputItem>,
) -> Result<Vec<u8>, JudgeProviderError> {
    let mut transport_bytes = 0usize;
    let mut accumulated_text = String::new();
    let mut terminal_text: Option<String> = None;
    let mut final_collected: Option<String> = None;
    let mut saw_end = false;
    while let Some(item) = stream
        .message()
        .await
        .map_err(|_| JudgeProviderError::TransportFailure)?
    {
        match item.item {
            Some(Item::Data(bytes)) => {
                if final_collected.is_some() || terminal_text.is_some() {
                    return Err(JudgeProviderError::TransportFailure);
                }
                transport_bytes = transport_bytes.saturating_add(bytes.len());
                if transport_bytes > MAX_STREAM_BYTES {
                    return Err(JudgeProviderError::TransportFailure);
                }
                let chunk = decode_chat_result(&bytes)?;
                if has_tool_activity(&chunk) {
                    return Err(JudgeProviderError::TransportFailure);
                }
                let text = extract_text(&chunk);
                if chunk.done {
                    let text = text.ok_or(JudgeProviderError::TransportFailure)?;
                    if text == accumulated_text {
                        terminal_text = Some(text);
                    } else {
                        accumulated_text.push_str(&text);
                        terminal_text = Some(accumulated_text.clone());
                    }
                    if terminal_text
                        .as_ref()
                        .is_some_and(|value| value.len() > MAX_JUDGE_RESPONSE_BYTES)
                    {
                        return Err(JudgeProviderError::TransportFailure);
                    }
                } else if let Some(text) = text {
                    accumulated_text.push_str(&text);
                    if accumulated_text.len() > MAX_JUDGE_RESPONSE_BYTES {
                        return Err(JudgeProviderError::TransportFailure);
                    }
                }
            }
            Some(Item::FinalCollected(bytes)) => {
                if final_collected.is_some() {
                    return Err(JudgeProviderError::TransportFailure);
                }
                transport_bytes = transport_bytes.saturating_add(bytes.len());
                if transport_bytes > MAX_STREAM_BYTES {
                    return Err(JudgeProviderError::TransportFailure);
                }
                let chunk = decode_chat_result(&bytes)?;
                if has_tool_activity(&chunk) || !chunk.done {
                    return Err(JudgeProviderError::TransportFailure);
                }
                final_collected = extract_text(&chunk);
                if final_collected
                    .as_ref()
                    .is_none_or(|text| text.len() > MAX_JUDGE_RESPONSE_BYTES)
                {
                    return Err(JudgeProviderError::TransportFailure);
                }
            }
            Some(Item::End(trailer)) => {
                if stream_error_from_trailer(&trailer).is_some() {
                    return Err(JudgeProviderError::TransportFailure);
                }
                saw_end = true;
            }
            None => return Err(JudgeProviderError::TransportFailure),
        }
        if saw_end {
            if stream
                .message()
                .await
                .map_err(|_| JudgeProviderError::TransportFailure)?
                .is_some()
            {
                return Err(JudgeProviderError::TransportFailure);
            }
            break;
        }
    }
    let final_text = final_collected
        .or(terminal_text)
        .filter(|text| text.len() <= MAX_JUDGE_RESPONSE_BYTES)
        .ok_or(JudgeProviderError::TransportFailure)?;
    if !saw_end {
        return Err(JudgeProviderError::TransportFailure);
    }
    Ok(final_text.into_bytes())
}

fn decode_chat_result(
    bytes: &[u8],
) -> Result<crate::grpc::proto::jobworkerp::runner::llm::LlmChatResult, JudgeProviderError> {
    if bytes.len() > MAX_RESULT_PROTO_BYTES {
        return Err(JudgeProviderError::TransportFailure);
    }
    crate::grpc::proto::jobworkerp::runner::llm::LlmChatResult::decode(bytes)
        .map_err(|_| JudgeProviderError::TransportFailure)
}

fn extract_text(
    result: &crate::grpc::proto::jobworkerp::runner::llm::LlmChatResult,
) -> Option<String> {
    use crate::grpc::proto::jobworkerp::runner::llm::llm_chat_result::message_content::Content;
    match result
        .content
        .as_ref()
        .and_then(|content| content.content.as_ref())
    {
        Some(Content::Text(text)) => Some(text.clone()),
        Some(Content::Image(_) | Content::ToolCalls(_)) | None => None,
    }
}

fn has_tool_activity(result: &crate::grpc::proto::jobworkerp::runner::llm::LlmChatResult) -> bool {
    use crate::grpc::proto::jobworkerp::runner::llm::llm_chat_result::message_content::Content;
    result.pending_tool_calls.is_some()
        || result.requires_tool_execution.unwrap_or(false)
        || result.tool_execution_started.is_some()
        || !result.tool_execution_results.is_empty()
        || result.content.as_ref().is_some_and(|content| {
            matches!(
                content.content.as_ref(),
                Some(Content::ToolCalls(_) | Content::Image(_))
            )
        })
}

fn checked_base_url(base_url: Option<&str>) -> Result<Option<String>, JudgeProviderConfigError> {
    let Some(base_url) = base_url.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let parsed = url::Url::parse(base_url).map_err(|_| JudgeProviderConfigError::UnsafeBaseUrl)?;
    if !matches!(parsed.scheme(), "https" | "http")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(JudgeProviderConfigError::UnsafeBaseUrl);
    }
    Ok(Some(base_url.to_owned()))
}

fn valid_model_identifier(model_id: &str) -> bool {
    !model_id.is_empty()
        && model_id.len() <= 256
        && !model_id
            .chars()
            .any(|value| value.is_control() || value.is_whitespace())
}

fn assessment_unique_key(assessment_id: &str, input_digest: &str) -> String {
    format!("coding-agent-assessment-v1:{assessment_id}:{input_digest}")
}

fn no_retry_policy() -> RetryPolicy {
    RetryPolicy {
        r#type: RetryType::None as i32,
        basis: 1.0,
        ..Default::default()
    }
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn request_timeout_seconds(deadline_unix_ms: u64) -> u32 {
    let remaining = deadline_unix_ms.saturating_sub(current_unix_ms());
    let seconds = remaining.saturating_add(999) / 1000;
    u32::try_from(seconds.max(1)).unwrap_or(u32::MAX)
}

fn rpc_error(operation: &'static str, code: Code) -> JobworkerpJudgeError {
    JobworkerpJudgeError::Rpc { operation, code }
}
