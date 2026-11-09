//! Long-lived jobworkerp gRPC handle used by Tauri commands.
//!
//! The wrapper exists so streaming dispatch (`run_named_stream`) can
//! observe per-chunk progress without each caller re-implementing the
//! `find_worker_by_name` → `enqueue_stream_with_json` plumbing.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use jobworkerp_client::client::function_set_yaml;
use jobworkerp_client::client::helper::UseJobworkerpClientHelper;
use jobworkerp_client::client::worker_yaml;
use jobworkerp_client::client::wrapper::JobworkerpClientWrapper;
use jobworkerp_client::jobworkerp::data::{
    JobId, ProblemDetails, ResultOutputItem, ResultStatus, StreamError, Trailer, WorkerId,
    result_output_item::Item,
};
use jobworkerp_client::jobworkerp::function::data::FunctionSetId;
use jobworkerp_client::jobworkerp::service::{
    CountJobProcessingStatusRequest, CountJobProcessingStatusResponse, ListenRequest,
    LoadWorkerRequest, PurgeStaleJobsRequest, ReleaseStaticWorkerRequest, RunnerNameRequest,
    WorkerNameRequest, listen_request, load_worker_request, release_static_worker_request,
};
use jobworkerp_client::stream_error::{describe_stream_error, stream_error_from_trailer};
use prost_reflect::{DynamicMessage, MessageDescriptor, SerializeOptions};
use serde::Serialize as _;
use tokio::sync::OnceCell;
use tokio_stream::StreamExt as _;
use tonic::Streaming;
use tracing::warn;

use crate::error::{AppError, AppResult};

pub mod coding_agent;
pub mod embedding;
pub mod job_timeouts;
pub mod llm_chat;
pub mod maintenance;
pub(crate) mod semantic_judge;
pub(crate) mod workflow;

/// `JobworkerpClientWrapper` takes seconds as `u32`, while job execution
/// timeout is represented as `u64` milliseconds on the wire.
fn grpc_request_timeout_sec(job_timeout_sec: u64) -> AppResult<u32> {
    u32::try_from(job_timeout_sec).map_err(|_| {
        AppError::Jobworkerp(format!(
            "gRPC request timeout {job_timeout_sec}s exceeds client limit"
        ))
    })
}

/// A failed stream still closes with a successful gRPC status: the failure
/// rides only in the End trailer. Without lifting it into `Err`, an upstream
/// error (e.g. an LLM provider rejecting the API key) looks like a clean
/// finish and the chat UI sits at "generating…".
fn check_stream_trailer(trailer: &Trailer) -> AppResult<()> {
    match stream_error_from_trailer(trailer) {
        Some(error) => Err(stream_end_error(&error)),
        None => Ok(()),
    }
}

fn stream_end_error(error: &StreamError) -> AppError {
    AppError::Jobworkerp(describe_stream_failure(error))
}

/// `"{origin} {CODE}: {reason}"`. The structured problem, when the failed
/// component reported one, reads better than the flat diagnostic message
/// (a WORKFLOW message joins title, detail and position with bare spaces).
fn describe_stream_failure(error: &StreamError) -> String {
    let reason = error
        .problem
        .as_ref()
        .and_then(problem_reason)
        .unwrap_or_else(|| error.message.clone());
    // The client appends the full problem; the reason above already carries
    // the part worth showing, so it is dropped here.
    describe_stream_error(&StreamError {
        message: reason,
        problem: None,
        ..error.clone()
    })
}

/// User-facing reason of a typed problem. Job failures carry a generic title
/// (e.g. "Failed to execute by jobworkerp") with the cause in `detail`, so
/// both are kept. The position is left out: callers name it from the
/// workflow `position`.
fn problem_reason(problem: &ProblemDetails) -> Option<String> {
    if problem.r#type.is_empty() {
        return None;
    }
    join_problem_reason(problem.title.as_deref(), problem.detail.as_deref())
}

fn join_problem_reason(title: Option<&str>, detail: Option<&str>) -> Option<String> {
    fn present(value: Option<&str>) -> Option<&str> {
        value.filter(|value| !value.is_empty())
    }
    match (present(title), present(detail)) {
        (Some(title), Some(detail)) => Some(format!("{title}: {detail}")),
        (Some(reason), None) | (None, Some(reason)) => Some(reason.to_owned()),
        (None, None) => None,
    }
}

/// Key under which [`DispatchStream::drain`] attaches the End's problem to a
/// failed workflow terminal. `WorkflowResult` has no such field.
const TERMINAL_PROBLEM_KEY: &str = "problem";

/// Reason recorded in the problem attached to a workflow terminal.
fn workflow_problem_reason(terminal: &serde_json::Value) -> Option<String> {
    let problem = terminal.get(TERMINAL_PROBLEM_KEY)?;
    let field = |name| problem.get(name).and_then(serde_json::Value::as_str);
    join_problem_reason(field("title"), field("detail"))
}

/// JSON form of a typed problem; an untyped one identifies nothing.
fn problem_json(problem: &ProblemDetails) -> Option<serde_json::Value> {
    if problem.r#type.is_empty() {
        return None;
    }
    serde_json::to_value(problem).ok()
}

/// A streaming dispatch is successful only when jobworkerp sends an explicit
/// `End` or `FinalCollected` item. A bare transport close can mean that the
/// server-side subscription expired before it delivered the workflow failure.
fn unexpected_dispatch_stream_close() -> AppError {
    AppError::Jobworkerp("dispatch stream closed without a terminal result".to_owned())
}

fn map_enqueue_error(error: anyhow::Error, action: &str) -> AppError {
    if error
        .downcast_ref::<jobworkerp_client::error::UniqueEnqueueError>()
        .is_some()
    {
        AppError::Other(error.context(action.to_owned()))
    } else {
        AppError::Jobworkerp(format!("{action}: {error:#}"))
    }
}

/// `WorkflowResult.status` values that mean the workflow itself failed.
/// Mirrors `FAILURE_WORKFLOW_STATUSES` in the pinned jobworkerp-client: only
/// `Faulted` is a hard failure — `Cancelled` / `Waiting` are deliberate
/// non-failure terminations, and `Running` / `Pending` should never reach a
/// terminal payload but are passed through rather than turned into errors.
/// A cancelled run is still unsuccessful; see
/// [`UNSUCCESSFUL_WORKFLOW_STATUSES`] for terminals reported to the user.
const FAILURE_WORKFLOW_STATUSES: &[&str] = &["Faulted"];

/// Terminal `WorkflowResult.status` values that end a run without success.
/// [`DispatchStream::drain`] hands such a terminal over in place of a failed
/// End, so callers must report every one of them as unsuccessful.
const UNSUCCESSFUL_WORKFLOW_STATUSES: &[&str] = &["Faulted", "Cancelled"];

/// The `status` of a decoded `WorkflowResult` when the run ended without
/// success (faulted or cancelled).
pub(crate) fn workflow_unsuccessful_status(output: &serde_json::Value) -> Option<&str> {
    output
        .get("status")
        .and_then(serde_json::Value::as_str)
        .filter(|status| UNSUCCESSFUL_WORKFLOW_STATUSES.contains(status))
}

/// Why a workflow terminal failed: the problem the stream's End attached
/// (its `error_message` is a flat rendering that cannot be split), else the
/// engine error's title, else the whole message.
pub(crate) fn workflow_terminal_reason(terminal: &serde_json::Value) -> Option<String> {
    workflow_problem_reason(terminal).or_else(|| {
        workflow_error_reason(terminal)
            .map(|message| workflow_error_title(message).unwrap_or(message).to_owned())
    })
}

/// The `title` of a workflow engine error rendered with `{:?}`
/// (e.g. `Task 'x' timed out`). Older servers embed a child job's `{:?}`
/// rendering in `WorkflowResult.error_message` and send no problem.
fn workflow_error_title(message: &str) -> Option<&str> {
    const MARKER: &str = "title: Some(\"";
    let start = message.find(MARKER)? + MARKER.len();
    let end = start + message[start..].find("\")")?;
    Some(&message[start..end])
}

/// The `status` of a decoded `WorkflowResult` when it is a hard failure.
pub(crate) fn workflow_failure_status(output: &serde_json::Value) -> Option<&str> {
    output
        .get("status")
        .and_then(serde_json::Value::as_str)
        .filter(|status| FAILURE_WORKFLOW_STATUSES.contains(status))
}

/// Non-empty error message of a decoded `WorkflowResult`. Reads both the
/// proto field name and its lowerCamelCase JSON variant because
/// descriptor-driven serialization may use either.
pub(crate) fn workflow_error_reason(output: &serde_json::Value) -> Option<&str> {
    output
        .get("error_message")
        .or_else(|| output.get("errorMessage"))
        .and_then(serde_json::Value::as_str)
        .filter(|reason| !reason.is_empty())
}

/// Diagnostic for a decoded `WorkflowResult` that reports a hard failure, or
/// `None` when the payload says the workflow finished without faulting.
fn workflow_failure_message(output: &serde_json::Value) -> Option<String> {
    let status = workflow_failure_status(output)?;
    Some(match workflow_error_reason(output) {
        Some(reason) => format!("reused workflow reported {status}: {reason}"),
        None => format!("reused workflow reported {status}"),
    })
}

fn is_duplicate_enqueue_error(error: &AppError) -> bool {
    matches!(
        error,
        AppError::Other(error)
            if error
                .downcast_ref::<jobworkerp_client::error::UniqueEnqueueError>()
                .is_some_and(|error| matches!(error, jobworkerp_client::error::UniqueEnqueueError::Duplicate(_)))
    )
}

/// Owned wrapper around `JobworkerpClientWrapper` so we can hide the
/// `anyhow::Error` boundary and stream-decoding details from callers.
/// The llama.cpp plugin runner behind the local LLM worker.
pub(crate) const LLM_PROMPT_RUNNER_NAME: &str = "LLMPromptRunner";

#[derive(Clone)]
pub struct JobworkerpHandle {
    inner: JobworkerpClientWrapper,
    native_interactive: Arc<OnceCell<JobworkerpClientWrapper>>,
    /// Server-side job deadline for jobs dispatched without an explicit one.
    /// Equal to the client request deadline so a result stream is never
    /// closed by the client before the server gives up on the job.
    job_timeout_sec: u64,
}

impl JobworkerpHandle {
    /// Batch workflows (summarize/personality/reflection) can run for hours
    /// on local LLMs. The per-call timeout doubles as `request_timeout` on
    /// the underlying tonic channel, so this must be at least as long as the
    /// slowest user-facing generation step.
    const DEFAULT_JOB_TIMEOUT_SEC: u64 = 3 * 60 * 60;

    pub async fn connect(jw_url: &str) -> AppResult<Self> {
        Self::connect_with_timeout(jw_url, Self::DEFAULT_JOB_TIMEOUT_SEC).await
    }

    /// Create a client whose request deadline and default job deadline both
    /// match a long-running workflow.
    pub async fn connect_with_timeout(jw_url: &str, timeout_sec: u64) -> AppResult<Self> {
        let inner =
            JobworkerpClientWrapper::new(jw_url, Some(grpc_request_timeout_sec(timeout_sec)?))
                .await
                .map_err(|e| AppError::Jobworkerp(format!("connect {jw_url}: {e}")))?;
        Ok(Self {
            inner,
            native_interactive: Arc::new(OnceCell::new()),
            job_timeout_sec: timeout_sec,
        })
    }

    /// Lazily create the native interactive transport against the same trusted
    /// endpoint and TLS configuration as the normal client, but without a
    /// channel request timeout. Native calls add bounded lookup/enqueue-open
    /// timeouts individually; the returned result stream has no lifetime limit.
    /// In the locked jobworkerp-client version, `GrpcConnection::new` applies
    /// `Endpoint::timeout` only for `Some(duration)`; `None` does not select a
    /// fallback request deadline.
    pub(crate) async fn native_interactive_client(&self) -> AppResult<&JobworkerpClientWrapper> {
        const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

        let address = self.inner.address().to_owned();
        self.native_interactive
            .get_or_try_init(|| async move {
                match tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    JobworkerpClientWrapper::new(&address, None),
                )
                .await
                {
                    Ok(Ok(client)) => Ok(client),
                    Ok(Err(_)) => Err(AppError::Jobworkerp(
                        "native interactive client connection failed".to_owned(),
                    )),
                    Err(_) => Err(AppError::Jobworkerp(
                        "native interactive client connection timed out".to_owned(),
                    )),
                }
            })
            .await
    }

    /// Confirm that the Runner gRPC service can answer a real RPC.
    ///
    /// A listening TCP port is not enough at startup: the server may still be
    /// completing its HTTP/2 setup. A missing runner is still a successful
    /// probe because the caller only needs to know that the service replied.
    pub async fn probe_runner_service(&self) -> AppResult<()> {
        self.runner_exists(LLM_PROMPT_RUNNER_NAME)
            .await
            .map(|_| ())
            .map_err(|e| AppError::Jobworkerp(format!("probe RunnerService: {e}")))
    }

    /// Apply a function-set YAML. MUST be called AFTER
    /// [`register_workers_from_yaml`]: the upstream helper resolves each
    /// target's worker name against the live registry.
    pub async fn register_function_sets_from_yaml(
        &self,
        yaml_path: &Path,
    ) -> AppResult<HashMap<String, FunctionSetId>> {
        function_set_yaml::register_function_sets_from_yaml(
            &self.inner,
            None,
            Arc::new(HashMap::new()),
            yaml_path,
        )
        .await
        .map_err(|e| {
            AppError::WorkerRegistration(format!(
                "register function_sets {}: {e:#}",
                yaml_path.display()
            ))
        })
    }

    /// Whether a function set is registered on this jobworkerp.
    pub async fn function_set_exists(&self, name: &str) -> AppResult<bool> {
        use jobworkerp_client::jobworkerp::function::service::FindByNameRequest;
        let mut client = self.inner.jobworkerp_client.function_set_client().await;
        client
            .find_by_name(FindByNameRequest { name: name.into() })
            .await
            .map(|response| response.into_inner().data.is_some())
            .map_err(|e| AppError::Jobworkerp(format!("find function set {name}: {e}")))
    }

    /// Whether a runner (built-in or plugin) is loaded on this jobworkerp.
    pub async fn runner_exists(&self, name: &str) -> AppResult<bool> {
        let mut client = self.inner.jobworkerp_client.runner_client().await;
        client
            .find_by_name(RunnerNameRequest { name: name.into() })
            .await
            .map(|response| response.into_inner().data.is_some())
            .map_err(|e| AppError::Jobworkerp(format!("find runner {name}: {e}")))
    }

    /// Channels some running worker instance of this jobworkerp processes.
    pub async fn processed_channels(&self) -> AppResult<Vec<String>> {
        let mut client = self.inner.jobworkerp_client.worker_instance_client().await;
        client
            .find_channel_list(jobworkerp_client::jobworkerp::data::Empty {})
            .await
            .map(|response| {
                response
                    .into_inner()
                    .channels
                    .into_iter()
                    .map(|channel| channel.name)
                    .collect()
            })
            .map_err(|e| AppError::Jobworkerp(format!("find channels: {e}")))
    }

    /// Resolve a registered worker name to its stable numeric id.
    pub async fn worker_id_by_name(&self, name: &str) -> AppResult<Option<i64>> {
        self.inner
            .find_worker_by_name(None, Arc::new(HashMap::new()), name)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("find worker {name}: {e}")))
            .map(|found| found.map(|(id, _)| id.value))
    }

    /// Delete an app-owned Worker by name, treating an already-absent Worker as reconciled.
    pub async fn delete_worker_by_name_if_present(&self, name: &str) -> AppResult<()> {
        if let Some(value) = self.worker_id_by_name(name).await? {
            let deleted = self
                .inner
                .jobworkerp_client
                .worker_client()
                .await
                .delete(WorkerId { value })
                .await
                .map_err(|e| AppError::Jobworkerp(format!("delete worker {name}: {e}")))?
                .into_inner();
            if !deleted.is_success {
                return Err(AppError::Jobworkerp(format!(
                    "delete worker {name}: not deleted"
                )));
            }
        }
        Ok(())
    }

    /// Return the exact description stored for a worker.  MCP interprets the
    /// string later, so acceptance tests must verify this persistence boundary
    /// without parsing or normalizing the value on the Lookback side.
    pub async fn worker_description_by_name(&self, name: &str) -> AppResult<Option<String>> {
        let mut client = self.inner.jobworkerp_client.worker_client().await;
        client
            .find_by_name(WorkerNameRequest {
                name: name.to_owned(),
            })
            .await
            .map(|response| {
                response
                    .into_inner()
                    .data
                    .and_then(|worker| worker.data)
                    .map(|worker| worker.description)
            })
            .map_err(|error| AppError::Jobworkerp(format!("find worker {name}: {error}")))
    }

    /// Count active job_processing_status rows matching `request` on the
    /// shared channel. Reuses the same `tonic::transport::Channel` every
    /// other command goes through (`job_processing_status_client()` just
    /// clones the cell) instead of dialing a fresh connection per call —
    /// the Settings queue card issues several of these per refresh.
    pub async fn count_job_processing_status(
        &self,
        request: CountJobProcessingStatusRequest,
    ) -> AppResult<CountJobProcessingStatusResponse> {
        self.inner
            .jobworkerp_client
            .job_processing_status_client()
            .await
            .count_by_condition(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|e| AppError::Jobworkerp(format!("count_job_processing_status: {e}")))
    }

    pub async fn register_workers_from_yaml(
        &self,
        yaml_path: &Path,
    ) -> AppResult<HashMap<String, WorkerId>> {
        worker_yaml::register_workers_from_yaml(
            &self.inner,
            None,
            Arc::new(HashMap::new()),
            yaml_path,
        )
        .await
        .map_err(|e| {
            // `{e:#}` walks anyhow's cause chain — the registration helper
            // wraps server-side gRPC failures behind a generic "registering
            // worker '<name>' failed" context, so without the chain the
            // user sees no actionable detail (PR0 spike surfaced this).
            AppError::WorkerRegistration(format!("register {}: {e:#}", yaml_path.display()))
        })
    }

    /// Register workers from in-memory YAML text instead of a file path,
    /// resolving `$file:` includes against `base_dir`. Used by the LLM
    /// hot-reload path: agent-app resolves the `%{LOOKBACK_LLM_*}`
    /// placeholders into the raw text BEFORE this call (env-free, so the
    /// downstream `expand_env` finds nothing left to substitute and never
    /// touches the process environment — avoiding the multi-threaded
    /// `set_var` UB that mutating the parent env for placeholder expansion
    /// would risk). `base_dir` MUST be the committed YAML's directory so the
    /// `$file:` workflow includes still resolve.
    pub async fn register_workers_from_yaml_str(
        &self,
        raw_yaml: &str,
        base_dir: &Path,
    ) -> AppResult<HashMap<String, WorkerId>> {
        worker_yaml::register_workers_from_yaml_str(
            &self.inner,
            None,
            Arc::new(HashMap::new()),
            raw_yaml,
            base_dir,
        )
        .await
        .map_err(|e| AppError::WorkerRegistration(format!("register workers from str: {e:#}")))
    }

    /// Discard a `use_static` worker's resident runner pool WITHOUT
    /// touching its definition (`WorkerService/ReleaseStaticWorker`). The
    /// pool is lazily re-created with the worker's CURRENT `runner_settings`
    /// on the next job — so a `register_workers_from_yaml` upsert that
    /// changed the LLM model takes effect without a sidecar restart.
    ///
    /// Errors with `FailedPrecondition` for a non-static worker; callers
    /// that don't know the worker's `use_static` flag up front should only
    /// invoke this for the static LLM worker (`memories-llm`).
    pub async fn release_static_worker(&self, name: &str) -> AppResult<()> {
        let req = ReleaseStaticWorkerRequest {
            target: Some(release_static_worker_request::Target::Name(
                name.to_string(),
            )),
        };
        self.inner
            .jobworkerp_client
            .worker_client()
            .await
            .release_static_worker(req)
            .await
            .map(|_| ())
            .map_err(|e| AppError::Jobworkerp(format!("release_static_worker {name}: {e}")))
    }

    /// Run the worker's `Runner::load()` ahead of the first job
    /// (`WorkerService/Load`): validates `runner_settings` and, for a
    /// `use_static` worker, warms the runner pool (downloading + loading
    /// the LLM model). This surfaces a bad model / missing GGUF as an
    /// error here instead of on the user's first chat turn, and — paired
    /// with [`release_static_worker`] — makes a model swap take effect
    /// synchronously without a sidecar restart.
    ///
    /// `timeout_ms` bounds the wait; a first-time model download can take
    /// minutes, so callers pass a generous value.
    pub async fn load_worker(&self, name: &str, timeout_ms: Option<u64>) -> AppResult<()> {
        let req = LoadWorkerRequest {
            target: Some(load_worker_request::Target::Name(name.to_string())),
            timeout_ms,
        };
        self.inner
            .jobworkerp_client
            .worker_client()
            .await
            .load(req)
            .await
            .map(|_| ())
            .map_err(|e| AppError::Jobworkerp(format!("load_worker {name}: {e:#}")))
    }

    /// Resolve `worker_name` to its `WorkerData` then open a streaming
    /// enqueue against it. Returns a `DispatchStream` the caller drives
    /// to completion via [`DispatchStream::drain`].
    pub async fn dispatch_stream(
        &self,
        worker_name: &str,
        input_json: serde_json::Value,
        using: Option<&str>,
    ) -> AppResult<DispatchStream> {
        self.dispatch_stream_with_timeout_and_uniq_key(
            worker_name,
            input_json,
            self.job_timeout_sec,
            using,
            None,
        )
        .await
    }

    /// Streaming dispatch with a caller-selected server-side job deadline.
    /// Long-running batch workflows must opt in explicitly; ordinary UI
    /// operations retain [`Self::DEFAULT_JOB_TIMEOUT_SEC`].
    pub async fn dispatch_stream_with_timeout(
        &self,
        worker_name: &str,
        input_json: serde_json::Value,
        timeout_sec: u64,
        using: Option<&str>,
    ) -> AppResult<DispatchStream> {
        self.dispatch_stream_with_timeout_and_uniq_key(
            worker_name,
            input_json,
            timeout_sec,
            using,
            None,
        )
        .await
    }

    pub async fn dispatch_stream_with_timeout_and_uniq_key(
        &self,
        worker_name: &str,
        input_json: serde_json::Value,
        timeout_sec: u64,
        using: Option<&str>,
        uniq_key: Option<&str>,
    ) -> AppResult<DispatchStream> {
        let (_worker_id, worker_data) = self
            .inner
            .find_worker_by_name(None, Arc::new(HashMap::new()), worker_name)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("find worker {worker_name}: {e}")))?
            .ok_or_else(|| AppError::Jobworkerp(format!("worker not found: {worker_name}")))?;
        let (job_id, streaming, result_descriptor) = self
            .inner
            .enqueue_stream_with_json_and_uniq_key(
                None,
                Arc::new(HashMap::new()),
                &worker_data,
                input_json,
                timeout_sec,
                using,
                uniq_key,
            )
            .await
            .map_err(|e| map_enqueue_error(e, &format!("enqueue stream {worker_name}")))?;
        Ok(DispatchStream {
            job_id,
            inner: streaming,
            result_descriptor,
        })
    }

    /// Resolve `worker_name` and run a single non-streaming dispatch,
    /// returning the decoded result JSON.
    ///
    /// Some runners (e.g. `MultimodalEmbeddingRunner`, registered with
    /// `response_type=DIRECT`) reject the streaming enqueue path with
    /// `InvalidArgument: runner does not support streaming`. Such
    /// single-shot, non-progress dispatches MUST use this method rather
    /// than [`dispatch_stream`].
    pub async fn dispatch_unary(
        &self,
        worker_name: &str,
        input_json: serde_json::Value,
        using: Option<&str>,
    ) -> AppResult<serde_json::Value> {
        self.dispatch_unary_with_uniq_key(worker_name, input_json, using, None)
            .await
    }

    pub async fn dispatch_unary_with_uniq_key(
        &self,
        worker_name: &str,
        input_json: serde_json::Value,
        using: Option<&str>,
        uniq_key: Option<&str>,
    ) -> AppResult<serde_json::Value> {
        let (_worker_id, worker_data) = self
            .inner
            .find_worker_by_name(None, Arc::new(HashMap::new()), worker_name)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("find worker {worker_name}: {e}")))?
            .ok_or_else(|| AppError::Jobworkerp(format!("worker not found: {worker_name}")))?;
        self.inner
            .enqueue_with_json_and_uniq_key(
                None,
                Arc::new(HashMap::new()),
                &worker_data,
                input_json,
                self.job_timeout_sec,
                using,
                uniq_key,
            )
            .await
            .map_err(|e| map_enqueue_error(e, &format!("enqueue {worker_name}")))
    }

    pub async fn find_latest_job_result_by_uniq_key(
        &self,
        uniq_key: &str,
    ) -> AppResult<Option<jobworkerp_client::jobworkerp::data::JobResult>> {
        self.inner
            .find_latest_job_result_by_uniq_key(uniq_key)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("find job result by uniq_key: {e:#}")))
    }

    async fn find_latest_job_result_by_uniq_key_with_retry(
        &self,
        uniq_key: &str,
    ) -> Option<jobworkerp_client::jobworkerp::data::JobResult> {
        // `Ok(None)` polls the full window: the duplicate's terminal result is
        // published shortly after ALREADY_EXISTS arrives. `Err` retries only a
        // couple of times so a permanent failure (e.g. an older server without
        // uniq lookup support) surfaces the original enqueue error promptly.
        for attempt in 0..20 {
            match self.find_latest_job_result_by_uniq_key(uniq_key).await {
                Ok(Some(result)) => return Some(result),
                Ok(None) if attempt < 19 => {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                Err(_) if attempt < 2 => {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                Ok(None) | Err(_) => return None,
            }
        }
        None
    }

    /// Decide whether a persisted result reused after a duplicate enqueue may
    /// close the step as successful. `Ok(())` reuses it, `Err(diagnostic)`
    /// surfaces the reason as a failed step.
    ///
    /// The protobuf `status` alone is not enough: the WORKFLOW runner closes
    /// its stream cleanly while carrying its own verdict inside
    /// `WorkflowResult.status`, so a faulted run persists as `Success` with a
    /// `Faulted` payload (the same two-layer check jobworkerp-client applies
    /// in `check_workflow_output_status`). When the payload or the worker's
    /// result schema cannot be decoded we keep the envelope's verdict —
    /// refusing to reuse a finished job over a decode hiccup would
    /// re-dispatch an hours-long batch.
    async fn classify_reused_job(
        &self,
        result: &jobworkerp_client::jobworkerp::data::JobResult,
    ) -> Result<(), String> {
        let not_success = || "existing job did not succeed".to_owned();
        let Some(data) = result.data.as_ref() else {
            return Err(not_success());
        };
        if data.status != ResultStatus::Success as i32 {
            return Err(not_success());
        }
        let Some(items) = data.output.as_ref().map(|output| &output.items) else {
            return Ok(());
        };
        if items.is_empty() {
            return Ok(());
        }
        let descriptor = jobworkerp_client::proto::JobworkerpProto::resolve_result_descriptor(
            &self.inner.jobworkerp_client,
            &data.worker_name,
            data.using.as_deref(),
        )
        .await;
        let Some(descriptor) = descriptor else {
            warn!(
                worker = %data.worker_name,
                "no result schema for reused job; trusting the protobuf status"
            );
            return Ok(());
        };
        workflow_failure_message(&decode_chunk_to_json(items, Some(&descriptor)))
            .map_or(Ok(()), Err)
    }

    /// Best-effort cancellation invoked when the user dismisses the
    /// toast or the app shuts down mid-stream.
    pub async fn cancel(&self, job_id: JobId) -> AppResult<()> {
        self.inner
            .delete_job(None, Arc::new(HashMap::new()), job_id)
            .await
            .map(|_| ())
            .map_err(|e| AppError::Jobworkerp(format!("cancel: {e}")))
    }

    /// Streaming dispatch for client-side tool calling. Surfaces the
    /// response trailer's `x-job-id-bin` (via `on_job_id`) before the
    /// job finishes so chat's cancel command can `JobService/Delete`
    /// a mid-flight tool (OPEN-CHAT-2). Chunks are aggregated by
    /// [`aggregate_tool_chunks`].
    pub async fn dispatch_stream_for_tool<F, Fut>(
        &self,
        worker_name: &str,
        input_json: serde_json::Value,
        using: Option<&str>,
        on_job_id: F,
    ) -> AppResult<serde_json::Value>
    where
        F: FnOnce(JobId) -> Fut,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let stream = self.dispatch_stream(worker_name, input_json, using).await?;
        if let Some(jid) = stream.job_id {
            on_job_id(jid).await;
        }
        let descriptor = stream.result_descriptor.clone();
        let mut last_data_bytes: Option<Vec<u8>> = None;
        let mut final_collected_bytes: Option<Vec<u8>> = None;
        stream
            .drain_bytes(|chunk| match chunk {
                ListenChunk::Data(bytes) => {
                    last_data_bytes = Some(bytes);
                }
                ListenChunk::Final {
                    collected: Some(bytes),
                } => {
                    final_collected_bytes = Some(bytes);
                }
                ListenChunk::Final { collected: None } => {}
            })
            .await?;
        Ok(aggregate_tool_chunks(
            last_data_bytes,
            final_collected_bytes,
            descriptor.as_ref(),
        ))
    }

    /// Resolve a function-set to an OpenAI-compatible tools JSON array
    /// (the shape llama-cpp-plugin expects in
    /// `function_options.client_tools_json` for client-side tool calling).
    /// Targets with no parseable JSON Schema degrade to `{"type":"object"}`
    /// so a partial set does not invalidate the whole array.
    pub async fn fetch_function_set_as_tools_json(
        &self,
        function_set_name: &str,
    ) -> AppResult<String> {
        let specs = self
            .inner
            .find_function_list_by_set(None, Arc::new(HashMap::new()), function_set_name)
            .await
            .map_err(|e| {
                AppError::Jobworkerp(format!("fetch function set {function_set_name}: {e:#}"))
            })?;
        let tools: Vec<serde_json::Value> = specs.iter().map(function_specs_to_tool).collect();
        serde_json::to_string(&tools)
            .map_err(|e| AppError::Jobworkerp(format!("serialize tools_json: {e}")))
    }

    #[cfg(test)]
    pub(crate) async fn function_set_worker_ids_by_name(&self, name: &str) -> AppResult<Vec<i64>> {
        use jobworkerp_client::jobworkerp::function::data::function_id::Id;
        use jobworkerp_client::jobworkerp::function::service::FindByNameRequest;
        let set = self
            .inner
            .jobworkerp_client
            .function_set_client()
            .await
            .find_by_name(FindByNameRequest {
                name: name.to_owned(),
            })
            .await
            .map_err(|e| AppError::Jobworkerp(format!("find FunctionSet {name}: {e}")))?
            .into_inner()
            .data
            .and_then(|set| set.data)
            .ok_or_else(|| AppError::Jobworkerp(format!("FunctionSet {name} not found")))?;
        set.targets
            .into_iter()
            .map(|target| match target.function_id.and_then(|id| id.id) {
                Some(Id::WorkerId(id)) => Ok(id.value),
                _ => Err(AppError::WorkerRegistration(format!(
                    "unexpected target in FunctionSet {name}"
                ))),
            })
            .collect()
    }

    /// Subscribe to `JobResultService::ListenStream` for a job already
    /// enqueued elsewhere. Result decoding is the caller's responsibility
    /// (the chat command decodes each chunk as `LlmChatResult` directly).
    pub async fn listen_result_stream(
        &self,
        job_id: JobId,
        worker_name: &str,
        using: Option<&str>,
    ) -> AppResult<ListenStream> {
        let req = ListenRequest {
            job_id: Some(job_id),
            worker: Some(listen_request::Worker::WorkerName(worker_name.to_string())),
            timeout: None,
            using: using.map(|s| s.to_string()),
        };
        let inner = self
            .inner
            .jobworkerp_client
            .job_result_client()
            .await
            .listen_stream(req)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("listen_stream: {e}")))?
            .into_inner();
        Ok(ListenStream { inner })
    }

    pub async fn run_maintenance(
        &self,
        requests: maintenance::MaintenanceRequests,
    ) -> AppResult<maintenance::MaintenanceReport> {
        let deleted_job_results = self
            .inner
            .jobworkerp_client
            .job_result_client()
            .await
            .delete_bulk(requests.delete_bulk)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("job_result delete_bulk: {e}")))?
            .into_inner()
            .deleted_count;
        let marked_stale_statuses = self
            .inner
            .jobworkerp_client
            .job_processing_status_client()
            .await
            .purge_stale_jobs(requests.purge_stale_jobs)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("job status purge_stale_jobs: {e}")))?
            .into_inner()
            .marked_count;
        let deleted_status_rows = self
            .inner
            .jobworkerp_client
            .job_processing_status_client()
            .await
            .cleanup(requests.cleanup)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("job status cleanup: {e}")))?
            .into_inner()
            .deleted_count;
        Ok(maintenance::MaintenanceReport {
            deleted_job_results,
            marked_stale_statuses,
            deleted_status_rows,
        })
    }

    /// Logically delete status-index rows that no longer have either live or
    /// persisted job state. Used only after a local sidecar restart.
    pub async fn purge_orphaned_job_processing_status(
        &self,
        request: PurgeStaleJobsRequest,
    ) -> AppResult<u64> {
        self.inner
            .jobworkerp_client
            .job_processing_status_client()
            .await
            .purge_stale_jobs(request)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("job status startup orphan sweep: {e}")))
            .map(|response| response.into_inner().marked_count)
    }
}

/// In-flight stream returned by [`JobworkerpHandle::dispatch_stream`].
/// `job_id` is exposed for cancellation; the inner `Streaming` is hidden
/// because every consumer just wants `ProgressEvent`s.
pub struct DispatchStream {
    pub job_id: Option<JobId>,
    inner: Streaming<ResultOutputItem>,
    result_descriptor: Option<MessageDescriptor>,
}

impl std::fmt::Debug for DispatchStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchStream")
            .field("job_id", &self.job_id)
            .field(
                "result_descriptor",
                &self
                    .result_descriptor
                    .as_ref()
                    .map(|d| d.full_name().to_owned()),
            )
            .finish_non_exhaustive()
    }
}

/// Decoded view of one `ResultOutputItem` for the UI layer. Keeping the
/// raw proto out of the public surface lets us swap rendering strategy
/// (e.g. JSON-prettified vs lossy-UTF8) without touching every caller.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    Chunk {
        text: String,
    },
    /// `final_text` is the `FinalCollected` payload, or for a WORKFLOW
    /// stream the terminal `WorkflowResult` JSON (see [`DispatchStream::drain`]);
    /// `None` on a plain End of any other stream.
    End {
        final_text: Option<String>,
    },
}

/// `ListenStream` hands the caller raw bytes (no DynamicMessage decode);
/// callers that need semantic typing decode each chunk themselves.
pub struct ListenStream {
    inner: Streaming<ResultOutputItem>,
}

impl std::fmt::Debug for ListenStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListenStream").finish_non_exhaustive()
    }
}

/// `Final` arrives once, on a `FinalCollected` payload or a clean End
/// (`None`). A failed End or a close before either yields no `Final`; the
/// drain returns an error instead.
#[derive(Debug)]
pub enum ListenChunk {
    Data(Vec<u8>),
    Final { collected: Option<Vec<u8>> },
}

/// Three-state callback used by [`run_named_stream`] to insulate import
/// and reflection from the proto-layer `ProgressEvent`. Mirrors
/// `StepStatus::{Active,Done,Failed}` and skips `Waiting` (initial
/// state, not stream-driven).
pub enum StreamEvent<'a> {
    Active(Option<&'a str>),
    Done(Option<&'a str>),
    Failed(&'a str),
}

/// Drive the named worker `worker_name` to completion, invoking `emit`
/// for every state transition. Equivalent to:
///
/// ```text
///   emit(Active(Some("dispatching {worker_name}")))
///   for chunk in stream:
///     if !chunk.text.is_empty(): emit(Active(Some(chunk.text)))
///   emit(Done(final_text))   // or Failed on error
/// ```
///
/// Centralised so summary/personality/reflection don't each carry a
/// hand-rolled stream-drain block. Empty chunks are filtered here (the
/// upstream WORKFLOW runner emits keep-alive empties between LLM calls).
pub async fn run_named_stream<F>(
    handle: &JobworkerpHandle,
    worker_name: &str,
    input: serde_json::Value,
    using: Option<&str>,
    emit: F,
) where
    F: FnMut(StreamEvent<'_>),
{
    // Delegate to the cancellable variant with a never-cancelled token
    // and a no-op async `on_job_id`. Keeping the streaming logic in one
    // place — non-cancellable callers stay one-liner and the cancellable
    // callers don't have to re-implement the chunk loop.
    let cancel = tokio_util::sync::CancellationToken::new();
    run_cancellable_named_stream(
        handle,
        worker_name,
        input,
        using,
        None,
        cancel,
        |_| async {},
        emit,
    )
    .await;
}

/// Owned counterpart of [`StreamEvent`] for ferrying chunks through an
/// async channel where the borrow checker can't keep the lifetime alive
/// across `tokio::select!` branches. Only used inside
/// [`run_cancellable_named_stream`].
enum OwnedStreamEvent {
    Active(Option<String>),
    Done(Option<String>),
    Failed(String),
}

/// Cancellable variant of [`run_named_stream`]: drives the worker until
/// either the stream closes or `cancel` is fired. `on_job_id` is invoked
/// (at most once) as soon as the dispatch returns its trailer JobId so
/// the caller can park it for `JobService/Delete` later — the same
/// pattern as [`JobworkerpHandle::dispatch_stream_for_tool`], but
/// adapted for the `StreamEvent` consumer shape used by import /
/// analysis dispatches.
///
/// On cancel the in-flight stream is simply dropped. Issuing the
/// server-side `JobService/Delete` is the cancel command's
/// responsibility (it has the AppState + JobworkerpHandle handy);
/// dropping here just unblocks the local task immediately so the
/// caller's downstream emits can run.
#[allow(clippy::too_many_arguments)]
pub async fn run_cancellable_named_stream<F, J, Fut>(
    handle: &JobworkerpHandle,
    worker_name: &str,
    input: serde_json::Value,
    using: Option<&str>,
    timeout_sec: Option<u64>,
    cancel: tokio_util::sync::CancellationToken,
    on_job_id: J,
    emit: F,
) where
    F: FnMut(StreamEvent<'_>),
    J: FnOnce(JobId) -> Fut,
    Fut: std::future::Future<Output = ()> + Send,
{
    run_cancellable_named_stream_with_uniq_key(
        handle,
        worker_name,
        input,
        using,
        timeout_sec,
        None,
        cancel,
        on_job_id,
        emit,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub async fn run_cancellable_named_stream_with_uniq_key<F, J, Fut>(
    handle: &JobworkerpHandle,
    worker_name: &str,
    input: serde_json::Value,
    using: Option<&str>,
    timeout_sec: Option<u64>,
    uniq_key: Option<&str>,
    cancel: tokio_util::sync::CancellationToken,
    on_job_id: J,
    mut emit: F,
) where
    F: FnMut(StreamEvent<'_>),
    // `FnOnce` because the streaming dispatch surfaces its trailer
    // JobId exactly once — at stream-open. The callback is async so the
    // caller can park the JobId into a `tokio::Mutex` without resorting
    // to `blocking_lock` (which would panic on a runtime worker thread —
    // the bug this signature shape fixes). Mirrors the
    // `dispatch_stream_for_tool` callback contract.
    J: FnOnce(JobId) -> Fut,
    Fut: std::future::Future<Output = ()> + Send,
{
    let initial = format!("dispatching {worker_name}");
    emit(StreamEvent::Active(Some(&initial)));
    // Bail out as early as possible so a cancel that arrives before the
    // dispatch lands still surfaces as Failed("cancelled") rather than
    // spending an LLM slot first.
    if cancel.is_cancelled() {
        emit(StreamEvent::Failed("cancelled"));
        return;
    }
    let dispatched = handle
        .dispatch_stream_with_timeout_and_uniq_key(
            worker_name,
            input,
            timeout_sec.unwrap_or(handle.job_timeout_sec),
            using,
            uniq_key,
        )
        .await;
    let stream = match dispatched {
        Ok(s) => s,
        Err(e) => {
            if is_duplicate_enqueue_error(&e) {
                // The server's uniq reservation spans active jobs only: the
                // queue releases the key on dequeue and cleanup_job deletes the
                // job row right after the terminal result is published. So
                // ALREADY_EXISTS normally means a prior attempt of this same
                // operation is still queued/running (or its result landed in the
                // tiny publish→cleanup window). Only a terminal JobResult permits
                // us to close the step; otherwise leave the retry visible as a
                // failure instead of dispatching with a different key.
                if let Some(result) = handle
                    .find_latest_job_result_by_uniq_key_with_retry(
                        uniq_key.expect("duplicate enqueue requires a unique key"),
                    )
                    .await
                {
                    match handle.classify_reused_job(&result).await {
                        Ok(()) => emit(StreamEvent::Done(Some("reused existing job"))),
                        Err(diagnostic) => emit(StreamEvent::Failed(&diagnostic)),
                    }
                    return;
                }
            }
            let msg = e.to_string();
            emit(StreamEvent::Failed(&msg));
            return;
        }
    };
    if let Some(jid) = stream.job_id {
        on_job_id(jid).await;
    }
    // Ferry chunks through an mpsc so the drain future owns its
    // borrows while the cancel branch can still call `emit`. Unbounded
    // is fine: the producer is the gRPC stream (already paced by the
    // server) and the consumer drains synchronously.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<OwnedStreamEvent>();
    let drain_tx = tx.clone();
    let drain = async move {
        let mut final_text_holder: Option<String> = None;
        let res = stream
            .drain(|ev| match ev {
                ProgressEvent::Chunk { text } if !text.is_empty() => {
                    let _ = drain_tx.send(OwnedStreamEvent::Active(Some(text)));
                }
                ProgressEvent::Chunk { .. } => {}
                ProgressEvent::End { final_text } => {
                    final_text_holder = final_text;
                }
            })
            .await;
        match res {
            Err(e) => {
                let _ = drain_tx.send(OwnedStreamEvent::Failed(format!("stream error: {e}")));
            }
            Ok(()) => {
                let _ = drain_tx.send(OwnedStreamEvent::Done(final_text_holder));
            }
        }
        // Drop the producer side so the consumer's `recv()` returns None
        // on the next poll, breaking the loop below.
        drop(drain_tx);
    };
    drop(tx);
    tokio::pin!(drain);
    let mut drain_done = false;
    loop {
        tokio::select! {
            // Cancel wins ties: the cancel command has already issued
            // JobService/Delete (or is about to), so we surface Failed and
            // drop the local stream rather than waiting for the server's
            // close trailer.
            _ = cancel.cancelled(), if !drain_done => {
                emit(StreamEvent::Failed("cancelled"));
                return;
            }
            // The drain task only resolves once — guard it with
            // `!drain_done` so subsequent loop iterations skip the
            // already-completed branch.
            _ = &mut drain, if !drain_done => {
                drain_done = true;
            }
            ev = rx.recv() => match ev {
                Some(OwnedStreamEvent::Active(t)) => {
                    emit(StreamEvent::Active(t.as_deref()));
                }
                Some(OwnedStreamEvent::Done(t)) => {
                    emit(StreamEvent::Done(t.as_deref()));
                    return;
                }
                Some(OwnedStreamEvent::Failed(msg)) => {
                    emit(StreamEvent::Failed(&msg));
                    return;
                }
                None => {
                    // Channel closed without an explicit terminal event —
                    // report the broken protocol as a failure. Treating it
                    // as Done can falsely claim that a timed-out workflow
                    // produced summaries.
                    let message = unexpected_dispatch_stream_close().to_string();
                    emit(StreamEvent::Failed(&message));
                    return;
                }
            },
        }
    }
}

impl DispatchStream {
    pub(crate) fn result_descriptor(&self) -> Option<MessageDescriptor> {
        self.result_descriptor.clone()
    }

    /// Drive the stream to completion. `on_event` is invoked for every chunk
    /// and once with `End` when the drain returns `Ok`. A failed End returns
    /// `Err`, except on a WORKFLOW stream whose last result is faulted or
    /// cancelled: that result is handed over as `final_text` (with the End's
    /// problem attached), so callers must check its status before treating
    /// `End` as success.
    pub async fn drain<F>(self, on_event: F) -> AppResult<()>
    where
        F: FnMut(ProgressEvent),
    {
        drain_dispatch_items(self.inner, self.result_descriptor.as_ref(), on_event).await
    }

    /// Variant of [`drain`] that hands the caller raw bytes instead of
    /// the descriptor-decoded JSON string. The chat command needs this
    /// to decode each chunk as `LlmChatResult` directly and avoid the
    /// DynamicMessage → JSON → re-parse round trip.
    pub async fn drain_bytes<F>(self, on_chunk: F) -> AppResult<()>
    where
        F: FnMut(ListenChunk),
    {
        drain_listen_chunks(self.inner, "DispatchStream", on_chunk).await
    }

    /// Workspace effects require a real caller End and a clean subscription close.
    pub async fn drain_confirmed_bytes<F>(self, on_chunk: F) -> AppResult<()>
    where
        F: FnMut(ListenChunk),
    {
        drain_confirmed_items(self.inner, on_chunk).await
    }
}

impl ListenStream {
    /// Drive the stream to completion. `on_chunk` MUST be cheap (single
    /// Tauri `emit` or mpsc send) — it is invoked synchronously between
    /// gRPC polls, so a slow callback applies backpressure all the way
    /// to the jobworkerp server and stalls token generation.
    pub async fn drain<F>(self, on_chunk: F) -> AppResult<()>
    where
        F: FnMut(ListenChunk),
    {
        drain_listen_chunks(self.inner, "ListenStream", on_chunk).await
    }
}

/// Body of [`DispatchStream::drain`], generic over the item stream so it can
/// be exercised without a gRPC server.
async fn drain_dispatch_items<S, F>(
    mut stream: S,
    result_descriptor: Option<&MessageDescriptor>,
    mut on_event: F,
) -> AppResult<()>
where
    S: tokio_stream::Stream<Item = Result<ResultOutputItem, tonic::Status>> + Unpin,
    F: FnMut(ProgressEvent),
{
    let mut workflow_terminal = WorkflowTerminal::for_descriptor(result_descriptor);
    while let Some(item) = stream.next().await {
        let item = item.map_err(AppError::Grpc)?;
        match item.item {
            Some(Item::Data(bytes)) => {
                let text = render_chunk(&bytes, result_descriptor);
                if let Some(terminal) = workflow_terminal.as_mut() {
                    terminal.observe(bytes);
                }
                on_event(ProgressEvent::Chunk { text });
            }
            Some(Item::FinalCollected(bytes)) => {
                let text = render_chunk(&bytes, result_descriptor);
                on_event(ProgressEvent::End {
                    final_text: Some(text),
                });
                return Ok(());
            }
            Some(Item::End(trailer)) => {
                let final_text = match stream_error_from_trailer(&trailer) {
                    None => workflow_terminal.and_then(WorkflowTerminal::finish),
                    // A faulted or cancelled WorkflowResult names the failed
                    // task, which callers report; the End only adds the
                    // structured reason. Anything else is explained by the
                    // End alone.
                    Some(error) => Some(
                        workflow_terminal
                            .and_then(|terminal| terminal.finish_unsuccessful(&error))
                            .ok_or_else(|| stream_end_error(&error))?,
                    ),
                };
                on_event(ProgressEvent::End { final_text });
                return Ok(());
            }
            None => {
                warn!(
                    stream = "DispatchStream",
                    "result item with no oneof; skipping"
                );
            }
        }
    }
    Err(unexpected_dispatch_stream_close())
}

/// Body of [`DispatchStream::drain_confirmed_bytes`].
async fn drain_confirmed_items<S, F>(mut stream: S, mut on_chunk: F) -> AppResult<()>
where
    S: tokio_stream::Stream<Item = Result<ResultOutputItem, tonic::Status>> + Unpin,
    F: FnMut(ListenChunk),
{
    let mut ended = false;
    let mut collected = false;
    while let Some(item) = stream.next().await {
        let item = item.map_err(AppError::Grpc)?;
        if ended {
            return Err(AppError::Jobworkerp(
                "Unexpected result after caller End".into(),
            ));
        }
        match item.item {
            Some(Item::Data(bytes)) if !collected => on_chunk(ListenChunk::Data(bytes)),
            Some(Item::FinalCollected(bytes)) if !collected => {
                collected = true;
                on_chunk(ListenChunk::Final {
                    collected: Some(bytes),
                });
            }
            Some(Item::End(trailer)) => {
                check_stream_trailer(&trailer)?;
                ended = true;
                on_chunk(ListenChunk::Final { collected: None });
            }
            _ => {
                return Err(AppError::Jobworkerp(
                    "Invalid confirmed result sequence".into(),
                ));
            }
        }
    }
    if ended {
        Ok(())
    } else {
        Err(unexpected_dispatch_stream_close())
    }
}

/// Drive a `ResultOutputItem` stream until its terminal item. Emits exactly
/// one [`ListenChunk::Final`] on an explicit `FinalCollected` or clean `End`;
/// a failed `End` or a close before either is an error, since only the End
/// tells that the job succeeded. `label` only tags the structured log when an
/// empty oneof is observed.
async fn drain_listen_chunks<S, F>(
    mut stream: S,
    label: &'static str,
    mut on_chunk: F,
) -> AppResult<()>
where
    S: tokio_stream::Stream<Item = Result<ResultOutputItem, tonic::Status>> + Unpin,
    F: FnMut(ListenChunk),
{
    while let Some(item) = stream.next().await {
        let item = item.map_err(AppError::Grpc)?;
        match item.item {
            Some(Item::Data(bytes)) => on_chunk(ListenChunk::Data(bytes)),
            Some(Item::FinalCollected(bytes)) => {
                on_chunk(ListenChunk::Final {
                    collected: Some(bytes),
                });
                return Ok(());
            }
            Some(Item::End(trailer)) => {
                check_stream_trailer(&trailer)?;
                on_chunk(ListenChunk::Final { collected: None });
                return Ok(());
            }
            None => {
                warn!(stream = label, "result item with no oneof; skipping");
            }
        }
    }
    Err(unexpected_dispatch_stream_close())
}

/// `client_tools_json` is forwarded verbatim to llama.cpp's
/// `chat_template_kwargs.tools`, so each entry must match the OpenAI
/// Chat Completions schema. Non-parseable or absent schemas degrade to a
/// permissive `{"type":"object"}` rather than failing the whole fetch.
fn function_specs_to_tool(
    specs: &jobworkerp_client::jobworkerp::function::data::FunctionSpecs,
) -> serde_json::Value {
    let parameters = pick_arguments_schema(specs);
    serde_json::json!({
        "type": "function",
        "function": {
            "name": specs.name,
            "description": specs.description,
            "parameters": parameters,
        },
    })
}

fn pick_arguments_schema(
    specs: &jobworkerp_client::jobworkerp::function::data::FunctionSpecs,
) -> serde_json::Value {
    // Prefer "run" (single-method runners). MCP tools may use other names,
    // so fall back to the first available schema rather than hard-failing.
    let schema_str = specs.methods.as_ref().and_then(|m| {
        m.schemas
            .get("run")
            .or_else(|| m.schemas.values().next())
            .map(|s| s.arguments_schema.as_str())
    });
    schema_str
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .unwrap_or_else(|| serde_json::json!({"type": "object"}))
}

/// Resolve a WORKFLOW tool's streamed chunks into a single
/// `serde_json::Value`. Last-wins: each Data chunk is a complete
/// `WorkflowResult` snapshot (status `Running` → `Completed`), not a
/// streaming delta — merge-concat would push_str string fields together
/// and turn `Running` into `RunningRunningCompleted`. `FinalCollected`
/// (DIRECT response_type) supersedes the last Data when present. Decode
/// runs on the surviving payload only so per-chunk DynamicMessage work
/// is skipped for the in-flight Running snapshots.
fn aggregate_tool_chunks(
    last_data_bytes: Option<Vec<u8>>,
    final_collected_bytes: Option<Vec<u8>>,
    desc: Option<&MessageDescriptor>,
) -> serde_json::Value {
    let winning = final_collected_bytes.or(last_data_bytes);
    match winning {
        Some(bytes) => decode_chunk_to_json(&bytes, desc),
        None => serde_json::Value::Null,
    }
}

pub(crate) const WORKFLOW_RESULT_MESSAGE: &str = "jobworkerp.runner.WorkflowResult";

/// Keeps the last chunk of a WORKFLOW result stream. jobworkerp streams
/// `Data(WorkflowResult)* -> End` without `FinalCollected`, so the last Data
/// is the run's terminal result (Completed with its output, Faulted or
/// Cancelled).
struct WorkflowTerminal {
    descriptor: MessageDescriptor,
    last: Option<Vec<u8>>,
}

impl WorkflowTerminal {
    fn for_descriptor(descriptor: Option<&MessageDescriptor>) -> Option<Self> {
        descriptor
            .filter(|descriptor| descriptor.full_name() == WORKFLOW_RESULT_MESSAGE)
            .map(|descriptor| Self {
                descriptor: descriptor.clone(),
                last: None,
            })
    }

    fn observe(&mut self, bytes: Vec<u8>) {
        self.last = Some(bytes);
    }

    /// Keeps default-valued fields: the default serializer omits `status`
    /// for Completed (enum value 0).
    fn finish(self) -> Option<String> {
        let bytes = self.last?;
        Some(
            render_message_with_defaults(&bytes, &self.descriptor)
                .unwrap_or_else(|| render_chunk(&bytes, Some(&self.descriptor))),
        )
    }

    /// The terminal of a stream whose End reported `error`, when the last
    /// result is itself a faulted or cancelled run. The End's problem is
    /// attached under [`TERMINAL_PROBLEM_KEY`]: the run's `error_message` is
    /// a flat rendering whose title and detail cannot be split apart.
    /// `None` when the last result does not explain the failure.
    fn finish_unsuccessful(self, error: &StreamError) -> Option<String> {
        let mut terminal = DynamicMessage::decode(self.descriptor, self.last?.as_slice())
            .ok()?
            .serialize_with_options(
                serde_json::value::Serializer,
                &SerializeOptions::new().skip_default_fields(false),
            )
            .ok()?;
        workflow_unsuccessful_status(&terminal)?;
        if let (Some(fields), Some(problem)) = (
            terminal.as_object_mut(),
            error.problem.as_ref().and_then(problem_json),
        ) {
            fields.insert(TERMINAL_PROBLEM_KEY.to_owned(), problem);
        }
        Some(terminal.to_string())
    }
}

/// Decode `bytes` against `descriptor` to JSON keeping default-valued fields,
/// so enum value 0 and empty strings stay distinguishable from absent fields.
pub(crate) fn render_message_with_defaults(
    bytes: &[u8],
    descriptor: &MessageDescriptor,
) -> Option<String> {
    render_message(
        bytes,
        descriptor,
        &SerializeOptions::new().skip_default_fields(false),
    )
}

fn render_message(
    bytes: &[u8],
    descriptor: &MessageDescriptor,
    options: &SerializeOptions,
) -> Option<String> {
    let message = DynamicMessage::decode(descriptor.clone(), bytes).ok()?;
    let mut output = Vec::new();
    message
        .serialize_with_options(&mut serde_json::Serializer::new(&mut output), options)
        .ok()?;
    String::from_utf8(output).ok()
}

/// Decode `bytes` against `desc` to a `serde_json::Value` directly,
/// bypassing the String → re-parse round trip that `render_chunk` uses
/// for log-only output. Falls back to a lossy-UTF8 wrapper Value when
/// no descriptor is available or decoding fails.
fn decode_chunk_to_json(bytes: &[u8], desc: Option<&MessageDescriptor>) -> serde_json::Value {
    let lossy = || serde_json::Value::String(String::from_utf8_lossy(bytes).into_owned());
    let Some(d) = desc else { return lossy() };
    let Ok(msg) = DynamicMessage::decode(d.clone(), bytes) else {
        return lossy();
    };
    match msg.serialize(serde_json::value::Serializer) {
        Ok(v) => v,
        Err(_) => lossy(),
    }
}

/// Decode `bytes` against `desc` to a JSON string. Falls back to lossy
/// UTF-8 when no descriptor is available (workers without a registered
/// `result_proto`) or when decoding fails for any reason.
fn render_chunk(bytes: &[u8], desc: Option<&MessageDescriptor>) -> String {
    desc.and_then(|d| render_message(bytes, d, &SerializeOptions::default()))
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    use jobworkerp_client::jobworkerp::data::StreamErrorCode;
    use jobworkerp_client::jobworkerp::function::data::{
        FunctionSpecs, MethodSchema, MethodSchemaMap,
    };

    fn trailer_with_meta(pairs: &[(&str, &str)]) -> Trailer {
        let mut metadata = std::collections::HashMap::new();
        for (k, v) in pairs {
            metadata.insert((*k).to_string(), (*v).to_string());
        }
        Trailer {
            metadata,
            ..Default::default()
        }
    }

    #[test]
    fn long_server_job_deadline_keeps_safe_client_request_deadline() {
        assert_eq!(
            grpc_request_timeout_sec(72 * 24 * 60 * 60).unwrap(),
            72 * 24 * 60 * 60
        );
    }

    #[test]
    fn grpc_request_timeout_rejects_values_outside_the_client_api_range() {
        assert!(grpc_request_timeout_sec(u64::from(u32::MAX) + 1).is_err());
    }

    #[test]
    fn unique_enqueue_errors_remain_downcastable_at_the_app_boundary() {
        let error = map_enqueue_error(
            anyhow::Error::new(jobworkerp_client::error::UniqueEnqueueError::Duplicate(
                "duplicate".into(),
            )),
            "enqueue test",
        );

        assert!(
            matches!(error, AppError::Other(ref error) if error.downcast_ref::<jobworkerp_client::error::UniqueEnqueueError>().is_some())
        );
    }

    #[test]
    fn uniq_enqueue_failure_keeps_the_reason_visible() {
        // The toast renders `AppError::to_string()`; wrapping the client
        // error in an action context must not swallow the reason (an
        // ALREADY_EXISTS rejection would otherwise read as the bare
        // "enqueue stream <worker>").
        let error = map_enqueue_error(
            anyhow::Error::new(jobworkerp_client::error::UniqueEnqueueError::Duplicate(
                "job with the unique key already exists: op-key".into(),
            )),
            "enqueue stream memories-summarize-batch",
        );

        assert!(
            error
                .to_string()
                .contains("enqueue stream memories-summarize-batch")
        );
        assert!(error.to_string().contains("already exists: op-key"));
    }

    #[test]
    fn only_faulted_workflow_output_invalidates_a_reused_job() {
        assert!(workflow_failure_message(&serde_json::json!({"status": "Completed"})).is_none());
        // Non-failure terminations must not be reported as errors (they are
        // intentional suspensions / cancellations).
        assert!(workflow_failure_message(&serde_json::json!({"status": "Cancelled"})).is_none());
        assert!(workflow_failure_message(&serde_json::json!({"status": "Waiting"})).is_none());
        assert!(workflow_failure_message(&serde_json::json!({})).is_none());

        assert_eq!(
            workflow_failure_message(
                &serde_json::json!({"status": "Faulted", "errorMessage": "gemini 429"})
            ),
            Some("reused workflow reported Faulted: gemini 429".to_owned())
        );
        assert_eq!(
            workflow_failure_message(
                &serde_json::json!({"status": "Faulted", "error_message": "boom"})
            ),
            Some("reused workflow reported Faulted: boom".to_owned())
        );
        assert_eq!(
            workflow_failure_message(&serde_json::json!({"status": "Faulted"})),
            Some("reused workflow reported Faulted".to_owned())
        );
    }

    #[test]
    fn only_duplicate_enqueue_errors_are_recoverable() {
        let duplicate = AppError::Other(anyhow::Error::new(
            jobworkerp_client::error::UniqueEnqueueError::Duplicate("duplicate".into()),
        ));
        let aborted = AppError::Other(anyhow::Error::new(
            jobworkerp_client::error::UniqueEnqueueError::Aborted("aborted".into()),
        ));

        assert!(is_duplicate_enqueue_error(&duplicate));
        assert!(!is_duplicate_enqueue_error(&aborted));
    }

    #[test]
    fn check_stream_trailer_passes_clean_trailer() {
        // An End trailer with no error key is the normal successful close;
        // returning Err here would break every well-behaved dispatch.
        assert!(check_stream_trailer(&trailer_with_meta(&[])).is_ok());
        // Unrelated metadata (trace ids, durations) MUST NOT trip the check.
        assert!(
            check_stream_trailer(&trailer_with_meta(&[
                ("trace_id", "abc"),
                ("duration_ms", "42")
            ]))
            .is_ok()
        );
    }

    // jobworkerp never wrote this key; treating it as a failure would turn
    // caller-echoed metadata into a false error.
    #[test]
    fn check_stream_trailer_ignores_the_unused_legacy_key() {
        assert!(
            check_stream_trailer(&trailer_with_meta(&[("jobworkerp.stream.error", "boom")]))
                .is_ok()
        );
    }

    #[test]
    fn check_stream_trailer_surfaces_the_typed_error() {
        // Regression: a provider 4xx closes the gRPC stream successfully and
        // reports the failure only in the End trailer. Without lifting it to
        // Err the chat UI sits at "generating…".
        let trailer = failed_trailer(stream_error(
            StreamErrorCode::ProviderStreamFailed,
            "Gemini API error (HTTP 400): API key not valid.",
            "LLM",
            None,
        ));
        let message = check_stream_trailer(&trailer).unwrap_err().to_string();
        assert!(
            message.contains("LLM PROVIDER_STREAM_FAILED: Gemini API error (HTTP 400)"),
            "{message}"
        );
    }

    // Older servers, or a hop that drops the typed field, still report
    // failures through the deprecated metadata keys.
    #[test]
    fn check_stream_trailer_reads_the_deprecated_metadata_keys() {
        let json_mirror = trailer_with_meta(&[(
            "stream_error",
            r#"{"version":1,"code":"TIMEOUT","message":"idle","origin":"SANDBOX"}"#,
        )]);
        assert!(
            check_stream_trailer(&json_mirror)
                .unwrap_err()
                .to_string()
                .contains("SANDBOX TIMEOUT: idle")
        );
        let plugin_key = trailer_with_meta(&[("jobworkerp.plugin.run_stream_error", "crashed")]);
        assert!(
            check_stream_trailer(&plugin_key)
                .unwrap_err()
                .to_string()
                .contains("PLUGIN EXECUTION_FAILED: crashed")
        );
    }

    // An unknown code from a newer server is still a failure.
    #[test]
    fn check_stream_trailer_fails_on_unknown_codes() {
        let trailer = failed_trailer(StreamError {
            code: 99,
            message: "future".to_owned(),
            origin: "X".to_owned(),
            problem: None,
        });
        assert!(check_stream_trailer(&trailer).is_err());
    }

    fn stream_error(
        code: StreamErrorCode,
        message: &str,
        origin: &str,
        problem: Option<ProblemDetails>,
    ) -> StreamError {
        StreamError {
            code: code as i32,
            message: message.to_owned(),
            origin: origin.to_owned(),
            problem,
        }
    }

    fn problem(title: Option<&str>, detail: Option<&str>) -> ProblemDetails {
        ProblemDetails {
            r#type: "https://open-workflow-specification.org/spec/1.0.0/errors/runtime".to_owned(),
            status: 500,
            title: title.map(str::to_owned),
            detail: detail.map(str::to_owned),
            instance: Some("/ROOT/do/2/summarizeEach".to_owned()),
        }
    }

    #[test]
    fn problem_reason_joins_the_title_and_detail() {
        assert_eq!(
            problem_reason(&problem(Some("t"), Some("d"))).as_deref(),
            Some("t: d")
        );
        assert_eq!(
            problem_reason(&problem(Some("t"), None)).as_deref(),
            Some("t")
        );
        assert_eq!(
            problem_reason(&problem(None, Some("d"))).as_deref(),
            Some("d")
        );
        assert_eq!(problem_reason(&problem(None, None)), None);
        assert_eq!(problem_reason(&problem(Some(""), Some(""))), None);
    }

    // A problem without a type identifies nothing (the contract's rule).
    #[test]
    fn problem_reason_ignores_an_untyped_problem() {
        let untyped = ProblemDetails {
            r#type: String::new(),
            ..problem(Some("t"), Some("d"))
        };
        assert_eq!(problem_reason(&untyped), None);
    }

    #[test]
    fn describe_stream_failure_prefers_the_problem_over_the_message() {
        let error = stream_error(
            StreamErrorCode::ExecutionFailed,
            "Failed to execute workflow: Error: Failed to execute by jobworkerp runner 'X' not found /ROOT/do/0",
            "WORKFLOW",
            Some(problem(
                Some("Failed to execute by jobworkerp"),
                Some("runner 'X' not found"),
            )),
        );
        assert_eq!(
            describe_stream_failure(&error),
            "WORKFLOW EXECUTION_FAILED: Failed to execute by jobworkerp: runner 'X' not found"
        );
    }

    #[test]
    fn describe_stream_failure_falls_back_to_the_message() {
        let error = stream_error(
            StreamErrorCode::ProviderStreamFailed,
            "invalid api key",
            "LLM",
            None,
        );
        assert_eq!(
            describe_stream_failure(&error),
            "LLM PROVIDER_STREAM_FAILED: invalid api key"
        );
    }

    fn item_stream(
        items: Vec<Item>,
    ) -> impl tokio_stream::Stream<Item = Result<ResultOutputItem, tonic::Status>> + Unpin {
        futures::stream::iter(
            items
                .into_iter()
                .map(|item| Ok(ResultOutputItem { item: Some(item) })),
        )
    }

    fn failed_trailer(error: StreamError) -> Trailer {
        Trailer {
            error: Some(error),
            ..Default::default()
        }
    }

    fn failed_end(error: StreamError) -> Item {
        Item::End(failed_trailer(error))
    }

    fn workflow_data(status: i32, error_message: Option<&str>) -> Item {
        Item::Data(encode_workflow_result(
            &workflow_result_descriptor(),
            status,
            "",
            "/ROOT/do/2/summarizeEach/run/function",
            error_message,
        ))
    }

    async fn drain_workflow_items(items: Vec<Item>) -> (AppResult<()>, Vec<ProgressEvent>) {
        let descriptor = workflow_result_descriptor();
        let mut events = Vec::new();
        let result = drain_dispatch_items(item_stream(items), Some(&descriptor), |event| {
            events.push(event)
        })
        .await;
        (result, events)
    }

    fn workflow_failure_end(problem: Option<ProblemDetails>) -> Item {
        failed_end(stream_error(
            StreamErrorCode::ExecutionFailed,
            "Failed to execute workflow: Error: t d /ROOT",
            "WORKFLOW",
            problem,
        ))
    }

    fn final_text(events: &[ProgressEvent]) -> serde_json::Value {
        match events.last() {
            Some(ProgressEvent::End {
                final_text: Some(text),
            }) => serde_json::from_str(text).unwrap(),
            other => panic!("expected End with final_text, got {other:?}"),
        }
    }

    // The faulted WorkflowResult names the failed task, so callers report the
    // failure from it; the End's problem rides along as the structured reason.
    #[tokio::test]
    async fn dispatch_drain_hands_over_a_faulted_workflow_with_the_end_problem() {
        let (result, events) = drain_workflow_items(vec![
            workflow_data(4, None),
            workflow_data(1, Some("Failed to execute workflow: Error: t d /ROOT")),
            workflow_failure_end(Some(problem(Some("Task timed out"), Some("no output")))),
        ])
        .await;
        result.unwrap();
        let terminal = final_text(&events);
        assert_eq!(terminal["status"], "Faulted");
        assert_eq!(terminal["problem"]["title"], "Task timed out");
        assert_eq!(terminal["problem"]["detail"], "no output");
        assert_eq!(terminal["problem"]["status"], 500);
        assert_eq!(
            workflow_problem_reason(&terminal).as_deref(),
            Some("Task timed out: no output")
        );
    }

    // Older servers send no problem; the terminal is handed over unchanged.
    #[tokio::test]
    async fn dispatch_drain_hands_over_a_faulted_workflow_without_a_problem() {
        let (result, events) = drain_workflow_items(vec![
            workflow_data(1, Some("boom")),
            workflow_failure_end(None),
        ])
        .await;
        result.unwrap();
        let terminal = final_text(&events);
        assert_eq!(terminal["status"], "Faulted");
        assert!(terminal.get("problem").is_none());
        assert_eq!(workflow_problem_reason(&terminal), None);
    }

    #[tokio::test]
    async fn dispatch_drain_hands_over_a_cancelled_workflow() {
        let (result, events) = drain_workflow_items(vec![
            workflow_data(2, None),
            failed_end(stream_error(
                StreamErrorCode::Cancelled,
                "cancelled",
                "WORKER",
                None,
            )),
        ])
        .await;
        result.unwrap();
        assert_eq!(final_text(&events)["status"], "Cancelled");
    }

    // Nothing explains the failure (still Running, or the stream failed
    // before any result): the End's error is the only diagnostic.
    #[tokio::test]
    async fn dispatch_drain_fails_when_no_failed_workflow_result_preceded_the_end() {
        for items in [
            vec![workflow_data(4, None), workflow_failure_end(None)],
            vec![workflow_failure_end(Some(problem(
                Some("Invalid input"),
                None,
            )))],
        ] {
            let (result, events) = drain_workflow_items(items).await;
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("WORKFLOW EXECUTION_FAILED")
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, ProgressEvent::End { .. }))
            );
        }
    }

    // A close before End is not a success, even after a completed result:
    // the subscription may have expired before the failure arrived.
    #[tokio::test]
    async fn dispatch_drain_fails_when_the_stream_closes_before_end() {
        for items in [vec![workflow_data(0, None)], Vec::new()] {
            let (result, events) = drain_workflow_items(items).await;
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("closed without a terminal result")
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, ProgressEvent::End { .. }))
            );
        }
    }

    #[tokio::test]
    async fn dispatch_drain_keeps_a_completed_workflow() {
        let (result, events) =
            drain_workflow_items(vec![workflow_data(0, None), Item::End(Trailer::default())]).await;
        result.unwrap();
        let terminal = final_text(&events);
        assert_eq!(terminal["status"], "Completed");
        assert!(terminal.get("problem").is_none());
    }

    #[tokio::test]
    async fn dispatch_drain_fails_other_results_on_a_failed_end() {
        let descriptor = test_message_descriptor();
        let result = drain_dispatch_items(
            item_stream(vec![
                Item::Data(Vec::new()),
                failed_end(stream_error(
                    StreamErrorCode::ProviderStreamFailed,
                    "interrupted",
                    "LLM",
                    None,
                )),
            ]),
            Some(&descriptor),
            |_| {},
        )
        .await;
        assert!(result.is_err());
    }

    async fn drain_listen_items(
        stream: impl tokio_stream::Stream<Item = Result<ResultOutputItem, tonic::Status>> + Unpin,
    ) -> (AppResult<()>, Vec<String>) {
        let mut chunks = Vec::new();
        let result = drain_listen_chunks(stream, "test", |chunk| {
            chunks.push(match chunk {
                ListenChunk::Data(_) => "data".to_owned(),
                ListenChunk::Final { collected: None } => "final".to_owned(),
                ListenChunk::Final { collected: Some(_) } => "collected".to_owned(),
            })
        })
        .await;
        (result, chunks)
    }

    #[tokio::test]
    async fn listen_drain_ends_once_on_a_clean_end_or_final_collected() {
        let (result, chunks) = drain_listen_items(item_stream(vec![
            Item::Data(vec![1]),
            Item::End(Trailer::default()),
        ]))
        .await;
        result.unwrap();
        assert_eq!(chunks, ["data", "final"]);
        let (result, chunks) = drain_listen_items(item_stream(vec![
            Item::Data(vec![1]),
            Item::FinalCollected(vec![2]),
        ]))
        .await;
        result.unwrap();
        assert_eq!(chunks, ["data", "collected"]);
    }

    #[tokio::test]
    async fn listen_drain_fails_on_a_failed_end_without_a_final_chunk() {
        let (result, chunks) = drain_listen_items(item_stream(vec![
            Item::Data(vec![1]),
            failed_end(stream_error(
                StreamErrorCode::ProviderStreamFailed,
                "invalid api key",
                "LLM",
                None,
            )),
        ]))
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("LLM PROVIDER_STREAM_FAILED: invalid api key")
        );
        assert_eq!(chunks, ["data"]);
    }

    // A close before End is not a success (the stream error contract).
    #[tokio::test]
    async fn listen_drain_fails_when_the_stream_closes_before_end() {
        for items in [vec![Item::Data(vec![1])], Vec::new()] {
            let (result, chunks) = drain_listen_items(item_stream(items)).await;
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("closed without a terminal result")
            );
            assert!(!chunks.iter().any(|chunk| chunk == "final"));
        }
    }

    #[tokio::test]
    async fn listen_drain_surfaces_a_transport_error() {
        let (result, _) = drain_listen_items(futures::stream::iter(vec![Err(
            tonic::Status::unavailable("gone"),
        )]))
        .await;
        assert!(matches!(result, Err(AppError::Grpc(_))));
    }

    #[tokio::test]
    async fn confirmed_drain_fails_on_a_failed_end() {
        let result = drain_confirmed_items(
            item_stream(vec![
                Item::Data(vec![1]),
                failed_end(stream_error(
                    StreamErrorCode::ExecutionFailed,
                    "exit 1",
                    "COMMAND",
                    None,
                )),
            ]),
            |_| {},
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("COMMAND EXECUTION_FAILED")
        );
    }

    fn specs_with_run_schema(name: &str, description: &str, schema_json: &str) -> FunctionSpecs {
        let mut schemas = std::collections::HashMap::new();
        schemas.insert(
            "run".to_string(),
            MethodSchema {
                arguments_schema: schema_json.to_string(),
                ..Default::default()
            },
        );
        FunctionSpecs {
            name: name.into(),
            description: description.into(),
            methods: Some(MethodSchemaMap { schemas }),
            ..Default::default()
        }
    }

    #[test]
    fn function_specs_renders_openai_tool_shape() {
        let specs = specs_with_run_schema(
            "lookback_recall",
            "Recall past memories.",
            r#"{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}"#,
        );
        let tool = function_specs_to_tool(&specs);
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "lookback_recall");
        assert_eq!(tool["function"]["description"], "Recall past memories.");
        assert_eq!(tool["function"]["parameters"]["type"], "object");
        assert_eq!(
            tool["function"]["parameters"]["properties"]["query"]["type"],
            "string"
        );
    }

    #[test]
    fn function_specs_falls_back_to_permissive_object_when_schema_missing() {
        let specs = FunctionSpecs {
            name: "no_schema".into(),
            description: "".into(),
            methods: None,
            ..Default::default()
        };
        let tool = function_specs_to_tool(&specs);
        assert_eq!(tool["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn function_specs_falls_back_when_arguments_schema_is_not_json() {
        let specs = specs_with_run_schema("bad", "", "not-a-json");
        let tool = function_specs_to_tool(&specs);
        // Bad schema → permissive default rather than blowing up the
        // entire tool list.
        assert_eq!(tool["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn function_specs_picks_first_method_when_run_absent() {
        let mut schemas = std::collections::HashMap::new();
        schemas.insert(
            "fetch_html".to_string(),
            MethodSchema {
                arguments_schema: r#"{"type":"object","properties":{"url":{"type":"string"}}}"#
                    .into(),
                ..Default::default()
            },
        );
        let specs = FunctionSpecs {
            name: "html_tool".into(),
            description: "".into(),
            methods: Some(MethodSchemaMap { schemas }),
            ..Default::default()
        };
        let tool = function_specs_to_tool(&specs);
        // Single-entry map → fetched without requiring the "run" key.
        assert_eq!(
            tool["function"]["parameters"]["properties"]["url"]["type"],
            "string"
        );
    }

    fn workflow_result_descriptor() -> MessageDescriptor {
        command_utils::protobuf::ProtobufDescriptor::new(
            &include_str!("../../tests/fixtures/jobworkerp-proto/workflow_result.proto")
                .to_string(),
        )
        .unwrap()
        .get_message_by_name(WORKFLOW_RESULT_MESSAGE)
        .unwrap()
    }

    fn encode_workflow_result(
        descriptor: &MessageDescriptor,
        status: i32,
        output: &str,
        position: &str,
        error_message: Option<&str>,
    ) -> Vec<u8> {
        use prost::Message;
        use prost_reflect::Value;
        let mut message = DynamicMessage::new(descriptor.clone());
        message.set_field_by_name("id", Value::String("run-1".into()));
        message.set_field_by_name("output", Value::String(output.into()));
        message.set_field_by_name("position", Value::String(position.into()));
        message.set_field_by_name("status", Value::EnumNumber(status));
        if let Some(error) = error_message {
            message.set_field_by_name("error_message", Value::String(error.into()));
        }
        message.encode_to_vec()
    }

    const TIMEOUT_POSITION: &str = "/ROOT/do/3/threadGroupStage/do/0/runThreadGroupBatch/try/do/0/invokeThreadGroupBatch/run/function";
    const TIMEOUT_ERROR: &str = "Failed to execute workflow: Error { detail: None, instance: Some(\"/ROOT\"), status: 408, title: Some(\"Task 'runThreadGroupBatch' timed out after 3600s\"), type_: UriTemplate(\"http-error://requesttimeout\") }";

    #[test]
    fn workflow_terminal_keeps_the_last_result_with_its_completed_status() {
        let descriptor = workflow_result_descriptor();
        let mut terminal = WorkflowTerminal::for_descriptor(Some(&descriptor)).unwrap();
        terminal.observe(encode_workflow_result(
            &descriptor,
            4,
            "{}",
            "/ROOT/do/0",
            None,
        ));
        terminal.observe(encode_workflow_result(
            &descriptor,
            0,
            r#"{"completed":true}"#,
            "",
            None,
        ));
        let terminal: serde_json::Value =
            serde_json::from_str(&terminal.finish().unwrap()).unwrap();
        assert_eq!(terminal["status"], "Completed");
        assert_eq!(terminal["output"], r#"{"completed":true}"#);
    }

    #[test]
    fn workflow_terminal_is_absent_without_data_or_for_other_results() {
        let descriptor = workflow_result_descriptor();
        assert_eq!(
            WorkflowTerminal::for_descriptor(Some(&descriptor))
                .unwrap()
                .finish(),
            None
        );
        assert!(WorkflowTerminal::for_descriptor(Some(&test_message_descriptor())).is_none());
        assert!(WorkflowTerminal::for_descriptor(None).is_none());
    }

    #[test]
    fn faulted_terminal_keeps_its_failure_details() {
        let descriptor = workflow_result_descriptor();
        let mut terminal = WorkflowTerminal::for_descriptor(Some(&descriptor)).unwrap();
        terminal.observe(encode_workflow_result(
            &descriptor,
            1,
            "",
            TIMEOUT_POSITION,
            Some(TIMEOUT_ERROR),
        ));
        let terminal: serde_json::Value =
            serde_json::from_str(&terminal.finish().unwrap()).unwrap();
        assert_eq!(workflow_failure_status(&terminal), Some("Faulted"));
        assert_eq!(terminal["position"], TIMEOUT_POSITION);
        assert_eq!(workflow_error_reason(&terminal), Some(TIMEOUT_ERROR));
    }

    #[test]
    fn render_chunk_falls_back_to_lossy_utf8_when_no_descriptor() {
        assert_eq!(render_chunk(b"hello", None), "hello");
        assert_eq!(render_chunk(b"", None), "");
        // Non-UTF-8 bytes do not panic — the replacement char fills in.
        let mixed = [0xff_u8, b'a', b'b'];
        let out = render_chunk(&mixed, None);
        assert!(out.ends_with("ab"));
        assert!(out.starts_with('\u{FFFD}'));
    }

    #[test]
    fn render_chunk_lossy_when_descriptor_present_but_bytes_dont_decode() {
        // Invalid wire bytes must fall through to lossy UTF-8 instead of
        // panicking or returning empty.
        let desc = test_message_descriptor();
        let out = render_chunk(b"\xff\xfeplain text", Some(&desc));
        assert!(!out.is_empty());
    }

    fn test_message_descriptor() -> MessageDescriptor {
        use prost::Message;
        use prost_reflect::DescriptorPool;
        use prost_types::{
            DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet,
            field_descriptor_proto::Type as FieldType,
        };
        let field = FieldDescriptorProto {
            name: Some("text".into()),
            number: Some(1),
            r#type: Some(FieldType::String as i32),
            label: Some(prost_types::field_descriptor_proto::Label::Optional as i32),
            ..Default::default()
        };
        let msg = DescriptorProto {
            name: Some("TestMsg".into()),
            field: vec![field],
            ..Default::default()
        };
        let file = FileDescriptorProto {
            name: Some("test.proto".into()),
            package: Some("test".into()),
            syntax: Some("proto3".into()),
            message_type: vec![msg],
            ..Default::default()
        };
        let set = FileDescriptorSet { file: vec![file] };
        let mut buf = Vec::new();
        set.encode(&mut buf).unwrap();
        let pool = DescriptorPool::decode(buf.as_ref()).unwrap();
        pool.get_message_by_name("test.TestMsg").unwrap()
    }

    #[test]
    fn decode_chunk_to_json_lossy_string_without_descriptor() {
        let v = decode_chunk_to_json(b"hello", None);
        assert_eq!(v, serde_json::Value::String("hello".into()));
    }

    #[test]
    fn decode_chunk_to_json_lossy_string_when_bytes_dont_decode() {
        let desc = test_message_descriptor();
        // High-bit-set bytes that aren't a valid wire-format message —
        // the fallback path must surface a non-empty String value
        // rather than panicking or returning Null.
        let v = decode_chunk_to_json(b"\xff\xfeplain", Some(&desc));
        match v {
            serde_json::Value::String(s) => assert!(!s.is_empty()),
            other => panic!("expected lossy String, got {other:?}"),
        }
    }

    #[test]
    fn decode_chunk_to_json_returns_object_for_valid_proto() {
        use prost::bytes::BufMut;
        let desc = test_message_descriptor();
        // Hand-encode `TestMsg { text: "hi" }` (proto3 wire format).
        let mut buf = Vec::new();
        // tag: field 1 (string), wire type 2 → 0x0A
        buf.put_u8(0x0a);
        // length-delimited: "hi" is 2 bytes
        buf.put_u8(0x02);
        buf.extend_from_slice(b"hi");
        let v = decode_chunk_to_json(&buf, Some(&desc));
        match v {
            serde_json::Value::Object(map) => {
                assert_eq!(
                    map.get("text").and_then(|v| v.as_str()),
                    Some("hi"),
                    "Object should expose the decoded field",
                );
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    /// Build a TestMsg descriptor with a single string `text` field so
    /// `decode_chunk_to_json` produces a parseable JSON object.
    /// Hand-encodes `TestMsg { text: <payload> }`.
    fn encode_testmsg(payload: &str) -> Vec<u8> {
        use prost::bytes::BufMut;
        let mut buf = Vec::new();
        buf.put_u8(0x0a); // field 1, wire type 2 (length-delimited)
        // length varint: payload < 128 bytes so single byte
        assert!(
            payload.len() < 128,
            "test payload must fit in 1-byte varint"
        );
        buf.put_u8(payload.len() as u8);
        buf.extend_from_slice(payload.as_bytes());
        buf
    }

    #[test]
    fn aggregate_tool_chunks_picks_last_data_chunk() {
        // Drives the real `WorkflowResult` shape via the surrogate `text`
        // field: with last-wins, multiple "Running" snapshots collapse to
        // the final "Completed" one. Merge-concat would instead string-
        // concat the field into "RunningRunningCompleted" — the bug this
        // test guards against.
        let desc = test_message_descriptor();
        // Simulate `dispatch_stream_for_tool`'s drain: only the last
        // Data bytes survive into the aggregation step.
        let v = aggregate_tool_chunks(Some(encode_testmsg("Completed")), None, Some(&desc));
        let obj = v.as_object().expect("aggregated to JSON object");
        assert_eq!(obj.get("text").and_then(|v| v.as_str()), Some("Completed"));
    }

    #[test]
    fn aggregate_tool_chunks_final_collected_supersedes_data() {
        // DIRECT response_type with a FinalCollected trailer: the
        // aggregate wins over any earlier Data snapshot.
        let desc = test_message_descriptor();
        let v = aggregate_tool_chunks(
            Some(encode_testmsg("Running")),
            Some(encode_testmsg("final")),
            Some(&desc),
        );
        let obj = v.as_object().expect("aggregated to JSON object");
        assert_eq!(obj.get("text").and_then(|v| v.as_str()), Some("final"));
    }

    #[test]
    fn aggregate_tool_chunks_empty_stream_returns_null() {
        assert_eq!(
            aggregate_tool_chunks(None, None, None),
            serde_json::Value::Null
        );
    }

    #[test]
    fn default_job_timeout_allows_three_hour_generation() {
        assert_eq!(JobworkerpHandle::DEFAULT_JOB_TIMEOUT_SEC, 3 * 60 * 60);
    }

    #[test]
    fn unexpected_dispatch_stream_close_is_an_error() {
        assert_eq!(
            unexpected_dispatch_stream_close().to_string(),
            "jobworkerp: dispatch stream closed without a terminal result"
        );
    }

    /// Regression for the "Cannot block the current thread from within a
    /// runtime" panic the personality button hit: `analysis_dispatch`'s
    /// `on_job_id` used `tokio::sync::Mutex::blocking_lock`, which a
    /// runtime worker MUST not call. The callback is now `FnOnce(JobId)
    /// -> Future` so the production call sites can park via the async
    /// `.lock().await` instead.
    ///
    /// We can't drive `run_cancellable_named_stream` end-to-end without
    /// a live jobworkerp, so the test exercises the same callback shape
    /// — async closure that holds an `Arc<tokio::sync::Mutex<…>>` and
    /// writes the parked id — on a real tokio runtime. With the buggy
    /// `blocking_lock` body this test panicked under
    /// `#[tokio::test(flavor = "multi_thread")]`; with the async fix it
    /// completes cleanly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn on_job_id_callback_parks_via_async_lock_without_blocking_panic() {
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let parked: Arc<Mutex<Option<JobId>>> = Arc::new(Mutex::new(None));
        let parked_for_cb = parked.clone();
        // Mirror the production callback shape exactly — the
        // signature is `FnOnce(JobId) -> impl Future<Output=()> + Send`.
        let cb = move |jid: JobId| async move {
            *parked_for_cb.lock().await = Some(jid);
        };
        cb(JobId { value: 4242 }).await;
        assert_eq!(
            parked.lock().await.as_ref().map(|j| j.value),
            Some(4242),
            "on_job_id must park the trailer JobId so cancel can Delete it"
        );
    }
}
