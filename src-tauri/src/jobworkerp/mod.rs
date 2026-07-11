//! Long-lived jobworkerp gRPC handle used by Tauri commands.
//!
//! The wrapper exists so streaming dispatch (`run_named_stream`) can
//! observe per-chunk progress without each caller re-implementing the
//! `find_worker_by_name` → `enqueue_stream_with_json` plumbing.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use jobworkerp_client::client::function_set_yaml;
use jobworkerp_client::client::helper::UseJobworkerpClientHelper;
use jobworkerp_client::client::worker_yaml;
use jobworkerp_client::client::wrapper::JobworkerpClientWrapper;
use jobworkerp_client::jobworkerp::data::{
    JobId, ResultOutputItem, WorkerId, result_output_item::Item,
};
use jobworkerp_client::jobworkerp::function::data::FunctionSetId;
use jobworkerp_client::jobworkerp::service::{
    CountJobProcessingStatusRequest, CountJobProcessingStatusResponse, ListenRequest,
    LoadWorkerRequest, PurgeStaleJobsRequest, ReleaseStaticWorkerRequest, listen_request,
    load_worker_request, release_static_worker_request,
};
use prost_reflect::{DynamicMessage, MessageDescriptor};
use serde::Serialize as _;
use tokio_stream::StreamExt as _;
use tonic::Streaming;
use tracing::warn;

use crate::error::{AppError, AppResult};

pub mod embedding;
pub mod llm_chat;
pub mod maintenance;

/// jobworkerp's stream-end protocol: when a streaming runner (LLM /
/// WORKFLOW) fails mid-stream the server still closes with a successful
/// gRPC status, but the `Trailer.metadata` map carries the failure under
/// this key. Mirrors `jobworkerp_runner::runner::plugins::impls::STREAM_ERROR_META_KEY`
/// — the value is the wire contract, not an internal detail, so vendoring
/// the literal here keeps agent-app free of the runner crate dependency.
/// Without consuming this, an upstream LLM API error (e.g. Gemini 400
/// invalid key) surfaces as a silent `End { final_text: None }` and the
/// chat UI sits at "generating…" forever.
const STREAM_ERROR_META_KEY: &str = "jobworkerp.stream.error";

/// Inspect an `Item::End` trailer for a stream-level failure and lift it
/// into an `AppError::Jobworkerp` so the drain returns `Err`. Returns
/// `Ok(())` for a clean trailer (no error key, or an empty value).
fn check_stream_trailer(trailer: &jobworkerp_client::jobworkerp::data::Trailer) -> AppResult<()> {
    match trailer.metadata.get(STREAM_ERROR_META_KEY) {
        Some(msg) if !msg.is_empty() => Err(AppError::Jobworkerp(msg.clone())),
        _ => Ok(()),
    }
}

/// Owned wrapper around `JobworkerpClientWrapper` so we can hide the
/// `anyhow::Error` boundary and stream-decoding details from callers.
#[derive(Clone)]
pub struct JobworkerpHandle {
    inner: JobworkerpClientWrapper,
}

impl JobworkerpHandle {
    /// Batch workflows (summarize/personality/reflection) can run for hours
    /// on local LLMs. The per-call timeout doubles as `request_timeout` on
    /// the underlying tonic channel, so this must be at least as long as the
    /// slowest user-facing generation step.
    const DEFAULT_JOB_TIMEOUT_SEC: u32 = 3 * 60 * 60;

    pub async fn connect(jw_url: &str) -> AppResult<Self> {
        let inner = JobworkerpClientWrapper::new(jw_url, Some(Self::DEFAULT_JOB_TIMEOUT_SEC))
            .await
            .map_err(|e| AppError::Jobworkerp(format!("connect {jw_url}: {e}")))?;
        Ok(Self { inner })
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

    /// Resolve a registered worker name to its stable numeric id.
    pub async fn worker_id_by_name(&self, name: &str) -> AppResult<Option<i64>> {
        self.inner
            .find_worker_by_name(None, Arc::new(HashMap::new()), name)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("find worker {name}: {e}")))
            .map(|found| found.map(|(id, _)| id.value))
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
        let (_worker_id, worker_data) = self
            .inner
            .find_worker_by_name(None, Arc::new(HashMap::new()), worker_name)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("find worker {worker_name}: {e}")))?
            .ok_or_else(|| AppError::Jobworkerp(format!("worker not found: {worker_name}")))?;
        let (job_id, streaming, result_descriptor) = self
            .inner
            .enqueue_stream_with_json(
                None,
                Arc::new(HashMap::new()),
                &worker_data,
                input_json,
                Self::DEFAULT_JOB_TIMEOUT_SEC,
                using,
            )
            .await
            .map_err(|e| AppError::Jobworkerp(format!("enqueue stream {worker_name}: {e:#}")))?;
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
        let (_worker_id, worker_data) = self
            .inner
            .find_worker_by_name(None, Arc::new(HashMap::new()), worker_name)
            .await
            .map_err(|e| AppError::Jobworkerp(format!("find worker {worker_name}: {e}")))?
            .ok_or_else(|| AppError::Jobworkerp(format!("worker not found: {worker_name}")))?;
        self.inner
            .enqueue_with_json(
                None,
                Arc::new(HashMap::new()),
                &worker_data,
                input_json,
                Self::DEFAULT_JOB_TIMEOUT_SEC,
                using,
            )
            .await
            .map_err(|e| AppError::Jobworkerp(format!("enqueue {worker_name}: {e:#}")))
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
    /// `final_text` is set on `STREAMING_TYPE_INTERNAL` (`final_collected`)
    /// and `None` on plain `Trailer.end`.
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

/// `Final` arrives exactly once before the stream closes, with either the
/// trailing `FinalCollected` payload or `None` for a plain `Trailer.end`.
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
pub async fn run_cancellable_named_stream<F, J, Fut>(
    handle: &JobworkerpHandle,
    worker_name: &str,
    input: serde_json::Value,
    using: Option<&str>,
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
    let stream = match handle.dispatch_stream(worker_name, input, using).await {
        Ok(s) => s,
        Err(e) => {
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
                    // synthesise Done so the caller doesn't sit at Active
                    // forever.
                    emit(StreamEvent::Done(None));
                    return;
                }
            },
        }
    }
}

impl DispatchStream {
    /// Drive the stream to completion. `on_event` is invoked for every
    /// chunk and exactly once with `End` (synthesized if the stream
    /// closes silently).
    pub async fn drain<F>(mut self, mut on_event: F) -> AppResult<()>
    where
        F: FnMut(ProgressEvent),
    {
        while let Some(item) = self.inner.next().await {
            let item = item.map_err(AppError::Grpc)?;
            match item.item {
                Some(Item::Data(bytes)) => {
                    let text = render_chunk(&bytes, self.result_descriptor.as_ref());
                    on_event(ProgressEvent::Chunk { text });
                }
                Some(Item::FinalCollected(bytes)) => {
                    let text = render_chunk(&bytes, self.result_descriptor.as_ref());
                    on_event(ProgressEvent::End {
                        final_text: Some(text),
                    });
                    return Ok(());
                }
                Some(Item::End(trailer)) => {
                    // The gRPC stream closes successfully even when the
                    // upstream runner (LLM / WORKFLOW) failed mid-stream;
                    // the failure rides in `Trailer.metadata`. Surface it
                    // so the consumer can show "API key invalid" etc.
                    // instead of leaving the chat UI stuck at "generating…".
                    check_stream_trailer(&trailer)?;
                    on_event(ProgressEvent::End { final_text: None });
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
        // Stream closed without an explicit terminator — synthesize End so
        // the UI doesn't sit at Active forever.
        on_event(ProgressEvent::End { final_text: None });
        Ok(())
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

/// Drive a `ResultOutputItem` stream until the server closes it. Emits
/// exactly one [`ListenChunk::Final`] before returning — synthesised if
/// the stream ends without an explicit `End` / `FinalCollected`. `label`
/// only tags the structured log when an empty oneof is observed.
async fn drain_listen_chunks<F>(
    mut stream: Streaming<ResultOutputItem>,
    label: &'static str,
    mut on_chunk: F,
) -> AppResult<()>
where
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
                // Surface upstream stream-level errors the same way
                // `DispatchStream::drain` does — see `check_stream_trailer`.
                // Without this the chat agent loop reads a clean End even
                // when Gemini / Anthropic / OpenAI returned a 4xx, and the
                // UI never moves off "generating…".
                check_stream_trailer(&trailer)?;
                on_chunk(ListenChunk::Final { collected: None });
                return Ok(());
            }
            None => {
                warn!(stream = label, "result item with no oneof; skipping");
            }
        }
    }
    on_chunk(ListenChunk::Final { collected: None });
    Ok(())
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
    let lossy = || String::from_utf8_lossy(bytes).into_owned();
    let Some(d) = desc else { return lossy() };
    let Ok(msg) = DynamicMessage::decode(d.clone(), bytes) else {
        return lossy();
    };
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::new(&mut buf);
    if msg.serialize(&mut ser).is_err() {
        return lossy();
    }
    String::from_utf8(buf).unwrap_or_else(|_| lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    use jobworkerp_client::jobworkerp::data::Trailer;
    use jobworkerp_client::jobworkerp::function::data::{
        FunctionSpecs, MethodSchema, MethodSchemaMap,
    };

    fn trailer_with_meta(pairs: &[(&str, &str)]) -> Trailer {
        let mut metadata = std::collections::HashMap::new();
        for (k, v) in pairs {
            metadata.insert((*k).to_string(), (*v).to_string());
        }
        Trailer { metadata }
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

    #[test]
    fn check_stream_trailer_passes_empty_error_value() {
        // The server sets the key but leaves the value empty on success
        // paths of some runners — only a non-empty error message is a
        // failure signal.
        assert!(check_stream_trailer(&trailer_with_meta(&[(STREAM_ERROR_META_KEY, "")])).is_ok());
    }

    #[test]
    fn check_stream_trailer_surfaces_non_empty_error() {
        // Regression: a Gemini 4xx (or any provider-side stream error)
        // ships its message under STREAM_ERROR_META_KEY on a successful
        // gRPC close. Without lifting it to Err, the chat UI sits at
        // "generating…" because the drain returns Ok(()) and no token
        // ever arrives.
        let trailer = trailer_with_meta(&[(
            STREAM_ERROR_META_KEY,
            "Gemini API error (HTTP 400): API key not valid.",
        )]);
        let err = check_stream_trailer(&trailer).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Gemini API error"),
            "error message must contain the provider message: {msg}"
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
