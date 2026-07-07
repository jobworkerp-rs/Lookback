//! Tauri command for the RAG chat. Dispatches `memories-llm` directly,
//! runs the client-side tool-calling agent loop on the Tauri side, and
//! translates `LlmChatResult` chunks into `chat://step` events.

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::error::AppResult;
use crate::jobworkerp::llm_chat::{ExtractedToolCall, ExtractedToolResult};

use super::connection::MemoriesCallback;
use super::{AppState, ChatCancelEntry, emit_event};

/// Surfaced when the user hits Stop while the agent loop is mid-flight.
/// Emitted as `phase=done` so the frontend's existing terminal-state
/// handling (busy → false, last turn settles) just works without a new
/// enum variant (OPEN-CHAT-2, DECIDE-CHAT-4).
const CANCELLED_MESSAGE: &str = "ユーザー操作により中断しました";

// We talk to the LLM worker directly rather than wrap it in a workflow:
// `workflow.dispatch_stream` aggregates the inner LLM's per-token chunks
// into one FinalCollected blob (only the in-process
// `execute_workflow_with_events` relays per-token, and that isn't on gRPC).
// The concrete worker name is resolved at dispatch time from the active
// LLM settings (local `memories-llm` or external `memories-llm-external`).

const CHAT_METHOD: &str = "chat";
const CHAT_FUNCTION_SET: &str = "lookback-rag";
/// Single RAG-retrieval tool wired into the chat agent loop. The tool
/// name is the worker name registered by `workers/llm-workers.yaml`.
pub(crate) const LOOKBACK_RECALL_TOOL: &str = "lookback_recall";

const DEFAULT_MAX_TOOL_HOPS: usize = 4;
const TOOL_RUN_METHOD: &str = "run";
const MAX_HOPS_REACHED_MESSAGE: &str = "tool 呼び出し回数の上限に達しました";

/// System prompt steering the LLM toward grounded answers + citation
/// honesty. Embedded here (not in a workflow YAML) because the chat
/// dispatch is a direct worker call — there's no document to host it.
///
/// This is the STATIC base; `dated_system_prompt` appends the current
/// local date at dispatch time so the model can resolve relative time
/// expressions. The agent-loop e2e harness uses this base verbatim (it
/// builds its own request), so keep the date out of this constant.
///
/// Exposed for integration tests in `tests/chat_agent_loop_e2e.rs` so
/// the production prompt is the single source of truth for the
/// reproduction harness.
#[doc(hidden)]
pub const CHAT_SYSTEM_PROMPT: &str = "\
You are Lookback's RAG assistant. The user asks about their own past \
conversations and work. Always call the `lookback_recall` tool to \
retrieve relevant memories before answering — do NOT rely on prior \
knowledge for factual claims about the user's history. The tool returns \
hits from both the summary layer (per-thread / daily / weekly / monthly) \
and the raw-message layer in a single call; use summary hits to locate \
the right thread or period and raw hits when you need concrete wording.\n\
\n\
When the question targets a specific KIND or PERIOD of summary, narrow the \
search with the tool's `summary_labels` filter instead of relying on \
semantic similarity alone — it is far more reliable. The summary layer is \
tagged with these thread labels:\n\
- \"summary\" (per-thread), \"daily_summary\", \"weekly_summary\", \"monthly_summary\" — the kind\n\
- \"date:YYYY-MM-DD\" — the day a daily summary covers\n\
- \"iso_week:YYYY-Www\" — the ISO week a weekly summary covers\n\
- \"month:YYYY-MM\" — the month a monthly summary covers\n\
Resolve relative dates (\"yesterday\", \"last week\") against the current date \
given below into absolute tags, then pass them with summary_label_match=\"ALL\" \
to require every label. For example, \"what did I do yesterday?\" on 2026-05-30 \
→ summary_labels=[\"daily_summary\", \"date:2026-05-29\"], summary_label_match=\"ALL\". \
Omit summary_labels for open-ended questions that aren't tied to a kind or period.\n\
\n\
If the tool returns no hits, say so plainly (\"該当する記憶が見つかりませんでした\") — \
do not invent details. Cite the memory_id of the sources you actually used. \
Respond in the language the user is using.";

const CHAT_EVENT: &str = "chat://step";

/// `role` is one of `"user"` / `"assistant"` / `"system"`; mapped onto
/// the llama-cpp-plugin `ChatRole` enum by [`chat_role_for_proto`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatAskRequest {
    pub messages: Vec<ChatMessage>,
    /// Correlation key chosen by the frontend so a Start emitted
    /// synchronously during dispatch lands on a turn it has already
    /// registered (closes the early-event-drop race).
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatAskResponse {
    pub job_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatPhase {
    Start,
    Searching,
    Source,
    Token,
    Done,
    Error,
}

/// IDs go on the wire as JSON strings via `serde_id`: memories'
/// snowflakes overflow `Number.MAX_SAFE_INTEGER` and a numeric round
/// trip through Tauri IPC would silently truncate.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "source_kind", rename_all = "snake_case")]
pub enum ChatSource {
    RawMemory {
        #[serde(with = "crate::serde_id")]
        memory_id: i64,
        #[serde(with = "crate::serde_id")]
        source_thread_id: i64,
        snippet: String,
        score: f32,
    },
    ThreadSummary {
        #[serde(with = "crate::serde_id")]
        memory_id: i64,
        #[serde(with = "crate::serde_id")]
        source_thread_id: i64,
        snippet: String,
        score: f32,
    },
    PeriodSummary {
        #[serde(with = "crate::serde_id")]
        memory_id: i64,
        period_key: String,
        scope_key: String,
        snippet: String,
        score: f32,
    },
}

/// Wire shape of every `chat://step` event. Only fields relevant to
/// the current phase are populated; the rest are skipped so the
/// frontend can pattern-match on `phase` without seeing `null` leftovers.
#[derive(Debug, Clone, Serialize)]
pub struct ChatStepUpdate {
    pub job_id: String,
    pub phase: ChatPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_delta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<ChatSource>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ChatStepUpdate {
    fn bare(job_id: String, phase: ChatPhase) -> Self {
        Self {
            job_id,
            phase,
            token_delta: None,
            sources: None,
            message: None,
        }
    }

    fn searching(job_id: String, message: String) -> Self {
        Self {
            message: Some(message),
            ..Self::bare(job_id, ChatPhase::Searching)
        }
    }

    fn token(job_id: String, delta: String) -> Self {
        Self {
            token_delta: Some(delta),
            ..Self::bare(job_id, ChatPhase::Token)
        }
    }

    /// Empty `sources` is legal: a tool may legitimately return zero hits,
    /// and the frontend renders that as the
    /// "該当する記憶が見つかりませんでした" state.
    fn source(job_id: String, sources: Vec<ChatSource>) -> Self {
        Self {
            sources: Some(sources),
            ..Self::bare(job_id, ChatPhase::Source)
        }
    }

    fn done_with_message(job_id: String, message: String) -> Self {
        Self {
            message: Some(message),
            ..Self::bare(job_id, ChatPhase::Done)
        }
    }

    fn error(job_id: String, message: String) -> Self {
        Self {
            message: Some(message),
            ..Self::bare(job_id, ChatPhase::Error)
        }
    }
}

/// Dispatch the RAG chat workflow and return immediately. The actual
/// stream is drained on a detached task that emits `chat://step`
/// events.
#[tauri::command]
pub async fn chat_ask(
    app: AppHandle,
    state: State<'_, AppState>,
    req: ChatAskRequest,
) -> AppResult<ChatAskResponse> {
    // The RAG chat's only tool (`lookback_recall`) embeds the query and runs
    // a HybridSearch, so it can't function while the local vector store is
    // degraded. MVP gates the whole turn (local mode only); a recall-skip
    // fallback that still answers from the LLM is tracked as a follow-up.
    state.ensure_local_embedding_available()?;
    let handle = state.jobworkerp().await?;
    // One read of `llm-settings.json` for the three values the dispatch
    // needs (worker name + external flag + chat-only generation overrides).
    let llm = state.llm_settings_snapshot();
    let worker_name = super::llm_settings::worker_name_for(llm.mode).to_string();
    let external = llm.mode == super::llm_settings::LlmMode::External;
    // Resolve once per turn: the active local preset decides what value
    // (if any) to send for `chat_template_kwargs.enable_thinking`. The
    // polarity differs by family:
    //   - Qwen3 → `false` (suppresses `<think>` that swallows tool calls)
    //   - Gemma 4 → `true`  (suppresses `<|channel>thought` prefix that
    //     blocks the PEG-Gemma4 tool grammar)
    //   - other / custom / dev-env override → don't send the kwarg.
    // The env_lookup closure feeds the same `LOOKBACK_LLM_MODEL`
    // precedence as the sidecar env injection (`resolve_local_llm_env_triple`),
    // so a user who runs a Gemma GGUF via env override without saving a
    // preset doesn't get Qwen's kwarg polarity. See
    // `llm_presets::ThinkingKwarg`. Cached in the spawned task; the user
    // can't change presets mid-turn without a sidecar restart.
    let thinking_kwarg =
        super::llm_settings::thinking_kwarg_for(&llm, |name| std::env::var(name).ok());
    // Resolve the memories gRPC endpoint the same way every other
    // workflow dispatch does, so RAG retrieval hits the SAME DB the
    // browse clients and the citation jump use. In remote mode this is
    // the configured remote URL (incl. HTTPS) — NOT the local sidecar —
    // which is what keeps the returned thread_id/memory_id resolvable by
    // `find_memory_position` instead of dangling against a different DB.
    let memories_callback = state.resolve_targets()?.memories_callback()?;
    let ChatAskRequest {
        messages: initial_messages,
        job_id,
    } = req;

    // Tools_json is fetched eagerly: it rides on every hop's request body,
    // and surfacing a failure here lets the caller's await reject the
    // command instead of having to subscribe to `chat://step` to learn the
    // chat could not start.
    let raw_tools_json = handle
        .fetch_function_set_as_tools_json(CHAT_FUNCTION_SET)
        .await?;
    // The FunctionService surfaces the *runner's* args schema, which for
    // WORKFLOW workers is `{workflowContext, workflowData, input, ...}`
    // — not the workflow YAML's `input.schema.document`. The LLM then
    // either fabricates `workflowContext`/`workflowData` or, worst case,
    // sticks the user's query into a workflow_data field and the runner
    // tries to re-load it as YAML/JSON and explodes ("Failed to load
    // workflow from json=document:..."). Patch the lookback_recall entry
    // to the workflow's actual user-facing schema so the LLM gets to see
    // `{query, limit_per_layer}` and nothing else.
    let tools_json = rewrite_lookback_recall_tool_schema(&raw_tools_json);

    // Surface Start synchronously so the frontend's pre-registered turn
    // sees the transition without an event-vs-state race.
    emit_event(
        &app,
        CHAT_EVENT,
        ChatStepUpdate::bare(job_id.clone(), ChatPhase::Start),
    );

    // Register the cancel handle BEFORE the detached task starts so a
    // (theoretical) racing `chat_cancel` invoked between this command
    // returning and the loop's first hop can already flip the token.
    let cancel_entry = state.chat_register(&job_id).await;

    let app_for_task = app;
    let job_id_for_task = job_id.clone();
    tokio::spawn(async move {
        // Guard the chat_take in the spawned task's Drop so a panic in
        // run_chat_stream still purges the in-flight entry. The release
        // profile sets `panic = "abort"` so this only kicks in for dev /
        // test panics; the equivalent prod safety net is the next
        // `chat_register` for the same jobId overwriting any stale entry.
        let _guard = ChatInFlightGuard::new(app_for_task.clone(), job_id_for_task.clone());
        run_chat_stream(
            handle,
            app_for_task,
            job_id_for_task,
            initial_messages,
            tools_json,
            memories_callback,
            cancel_entry,
            worker_name,
            external,
            thinking_kwarg,
            llm.max_tokens,
            llm.temperature,
        )
        .await;
    });

    Ok(ChatAskResponse { job_id })
}

/// RAII purger for `AppState::chat_in_flight` — see `chat_ask`.
struct ChatInFlightGuard {
    app: AppHandle,
    job_id: String,
}

impl ChatInFlightGuard {
    fn new(app: AppHandle, job_id: String) -> Self {
        Self { app, job_id }
    }
}

impl Drop for ChatInFlightGuard {
    fn drop(&mut self) {
        // Bare `tokio::spawn` panics when the runtime has already shut
        // down (e.g. RunEvent::ExitRequested has dropped it and the loop
        // task is being aborted). The map vanishes with the process
        // anyway, so a missed cleanup here is harmless.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let app = self.app.clone();
        let job_id = std::mem::take(&mut self.job_id);
        handle.spawn(async move {
            if let Some(state) = app.try_state::<AppState>() {
                let _ = state.chat_take(&job_id).await;
            }
        });
    }
}

/// Outcome of one LLM hop. `ContinueWithToolCalls(_)` is always non-empty.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HopOutcome {
    Done,
    ContinueWithToolCalls(Vec<ExtractedToolCall>),
}

/// Emit the cancelled-terminal event. Centralises the user-facing text
/// so the UI doesn't need a new `ChatPhase` variant (DECIDE-CHAT-4).
fn emit_cancelled_done(app: &AppHandle, job_id: &str) {
    emit_event(
        app,
        CHAT_EVENT,
        ChatStepUpdate::done_with_message(job_id.to_string(), CANCELLED_MESSAGE.to_string()),
    );
}

/// Drive the LLM stream + client-side tool-execution loop. On budget
/// exhaustion the loop emits Done (not Error) so any partial assistant
/// text stays as the user's answer.
#[allow(clippy::too_many_arguments)]
async fn run_chat_stream(
    handle: crate::jobworkerp::JobworkerpHandle,
    app: AppHandle,
    job_id: String,
    initial_messages: Vec<ChatMessage>,
    tools_json: String,
    memories_callback: MemoriesCallback,
    cancel_entry: ChatCancelEntry,
    worker_name: String,
    external: bool,
    thinking_kwarg: super::llm_presets::ThinkingKwarg,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
) {
    let cancel = &cancel_entry.token;
    let current_job_id = &cancel_entry.current_job_id;
    let mut messages: Vec<serde_json::Value> =
        initial_messages.iter().map(text_message_to_proto).collect();
    let mut last_started_call: Option<String> = None;
    let max_hops = max_tool_hops();
    // One date stamp per turn: hops are bounded by `max_tool_hops` and run
    // milliseconds apart, so anchoring at turn start keeps every hop's
    // SYSTEM message consistent and avoids reformatting the ~1KB prompt on
    // each iteration (Efficiency review, 2026-05-30).
    let system_text = dated_system_prompt(chrono::Local::now());

    tracing::info!(
        job_id = %job_id,
        msg_count = messages.len(),
        max_hops,
        "chat agent loop started"
    );

    for hop in 0..max_hops {
        if cancel.is_cancelled() {
            tracing::info!(job_id = %job_id, hop, "agent loop cancelled before hop start");
            emit_cancelled_done(&app, &job_id);
            return;
        }
        let args = build_chat_args(
            &messages,
            &tools_json,
            &system_text,
            external,
            thinking_kwarg,
            max_tokens,
            temperature,
        );
        tracing::trace!(job_id = %job_id, hop, args = %args, "build_chat_args");

        let stream = match handle
            .dispatch_stream(&worker_name, args, Some(CHAT_METHOD))
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(job_id = %job_id, hop, err = %e, "dispatch_stream failed");
                emit_event(
                    &app,
                    CHAT_EVENT,
                    ChatStepUpdate::error(job_id, e.to_string()),
                );
                return;
            }
        };
        // Park the live JobId so chat_cancel can issue JobService/Delete
        // against it — the streaming enqueue's response trailer carries
        // it via `x-job-id-bin` (DECIDE-CHAT-4 / docs/client-streaming-spec.md).
        *current_job_id.lock().await = stream.job_id;
        tracing::debug!(job_id = %job_id, hop, "dispatch_stream opened");

        let mut final_chunk: Option<crate::jobworkerp::llm_chat::ExtractedChunk> = None;
        // Fresh think-tag tracker per hop: each LLM turn opens its own
        // `<think>...</think>` block, so cross-hop state would spuriously
        // drop the first visible token of the next turn.
        let mut think_state = ThinkState::default();
        let mut chunk_count: usize = 0;
        let mut token_count: usize = 0;
        let drain_result = stream
            .drain_bytes(|chunk| {
                chunk_count += 1;
                let (updates, captured) = chunk_to_updates_for_hop(
                    &job_id,
                    chunk,
                    &mut last_started_call,
                    &mut think_state,
                );
                for update in updates {
                    if update.phase == ChatPhase::Token {
                        token_count += 1;
                    }
                    emit_event(&app, CHAT_EVENT, update);
                }
                // First `final-shaped` chunk wins. The tool-call chunk
                // (carrying `pending_tool_calls`) precedes the `done=true`
                // terminator on the separated wire shape, so without this
                // guard the terminator would clobber the tool calls and
                // the loop would exit Done with nothing to execute. See
                // `chunk_to_updates_for_hop` for the shape rationale.
                if let Some(c) = captured
                    && final_chunk.is_none()
                {
                    final_chunk = Some(c);
                }
            })
            .await;
        *current_job_id.lock().await = None;

        // A drain error following Delete is the expected shape of
        // "server closed the stream on cancel" — collapse both paths
        // here so the error branch below only fires on real failures.
        if cancel.is_cancelled() {
            if let Err(e) = drain_result {
                tracing::warn!(job_id = %job_id, hop, err = %e, "drain failed during cancel");
            }
            emit_cancelled_done(&app, &job_id);
            return;
        }
        if let Err(e) = drain_result {
            tracing::warn!(job_id = %job_id, hop, err = %e, "stream drain failed");
            emit_event(
                &app,
                CHAT_EVENT,
                ChatStepUpdate::error(job_id, format!("stream error: {e}")),
            );
            return;
        }
        tracing::info!(
            job_id = %job_id,
            hop,
            chunk_count,
            token_emit_count = token_count,
            has_final = final_chunk.is_some(),
            "hop stream drained"
        );

        match drive_hop(final_chunk.unwrap_or_default()) {
            HopOutcome::Done => {
                tracing::info!(job_id = %job_id, hop, "chat agent loop done");
                emit_event(
                    &app,
                    CHAT_EVENT,
                    ChatStepUpdate::bare(job_id, ChatPhase::Done),
                );
                return;
            }
            HopOutcome::ContinueWithToolCalls(calls) => {
                let names: Vec<&str> = calls.iter().map(|c| c.fn_name.as_str()).collect();
                tracing::info!(
                    job_id = %job_id,
                    hop,
                    tool_count = calls.len(),
                    tools = ?names,
                    "agent loop continuing with tool calls"
                );
                // Replay the assistant's tool-call turn so the next hop's
                // template sees its own invocation (oai_chat.rs requires
                // this pairing for the TOOL message that follows).
                messages.push(assistant_tool_calls_proto(&calls));

                for tc in &calls {
                    if cancel.is_cancelled() {
                        tracing::info!(job_id = %job_id, hop, "agent loop cancelled before tool");
                        emit_cancelled_done(&app, &job_id);
                        return;
                    }
                    let llm_args: serde_json::Value =
                        serde_json::from_str(&tc.fn_arguments).unwrap_or(serde_json::json!({}));
                    // WORKFLOW runners' `run` takes `{ "input": "<json>" }`;
                    // the chat LLM only sees the workflow's user schema (see
                    // rewrite_lookback_recall_tool_schema), so wrap before
                    // dispatch and inject the memories gRPC endpoint the
                    // workflow's HybridSearch must dial (resolved from the
                    // active connection). Other (non-WORKFLOW) tools — should
                    // the set grow later — would skip the wrap.
                    let dispatch_args = if tc.fn_name == LOOKBACK_RECALL_TOOL {
                        lookback_recall_dispatch_args(&llm_args, &memories_callback)
                    } else {
                        llm_args.clone()
                    };
                    // Streaming dispatch — even though the WORKFLOW runner's
                    // result is a single aggregate, going through the
                    // streaming enqueue exposes the JobId via the response
                    // trailer (`x-job-id-bin`) so `chat_cancel` can Delete
                    // a mid-flight `lookback_recall` (OPEN-CHAT-2).
                    let current_job_id_for_tool = current_job_id.clone();
                    let tool_result = handle
                        .dispatch_stream_for_tool(
                            &tc.fn_name,
                            dispatch_args,
                            Some(TOOL_RUN_METHOD),
                            move |jid| async move {
                                *current_job_id_for_tool.lock().await = Some(jid);
                            },
                        )
                        .await;
                    *current_job_id.lock().await = None;
                    match tool_result {
                        Ok(result_value) => {
                            let (tool_text, sources) =
                                build_tool_response(&tc.fn_name, result_value);
                            tracing::debug!(
                                job_id = %job_id,
                                fn_name = %tc.fn_name,
                                sources = sources.len(),
                                tool_text_len = tool_text.len(),
                                "tool dispatch_stream_for_tool ok"
                            );
                            emit_event(
                                &app,
                                CHAT_EVENT,
                                ChatStepUpdate::source(job_id.clone(), sources),
                            );
                            messages.push(tool_result_proto(
                                &tc.call_id,
                                &tc.fn_name,
                                &tool_text,
                                false,
                            ));
                        }
                        Err(e) => {
                            tracing::warn!(
                                job_id = %job_id,
                                fn_name = %tc.fn_name,
                                err = %e,
                                "tool dispatch_stream_for_tool failed"
                            );
                            // A failure right after cancel almost certainly
                            // means the user-triggered Delete closed the
                            // tool stream; treat that as the cancelled
                            // terminal state rather than yet another
                            // retry-bait error fed back to the LLM.
                            if cancel.is_cancelled() {
                                emit_cancelled_done(&app, &job_id);
                                return;
                            }
                            // Feed the failure to the LLM as a TOOL result so
                            // it can apologise or retry on the next hop.
                            // `is_error=true` is the structured signal (see
                            // `specs/tool-result-message-content-spec.md`
                            // Pass the raw error text so the
                            // provider-side rendering isn't double-wrapped.
                            let err_text = e.to_string();
                            emit_event(
                                &app,
                                CHAT_EVENT,
                                ChatStepUpdate::source(job_id.clone(), Vec::new()),
                            );
                            messages.push(tool_result_proto(
                                &tc.call_id,
                                &tc.fn_name,
                                &err_text,
                                true,
                            ));
                        }
                    }
                }
            }
        }
    }

    tracing::warn!(job_id = %job_id, max_hops, "chat agent loop hit hop budget");
    emit_event(
        &app,
        CHAT_EVENT,
        ChatStepUpdate::done_with_message(job_id, MAX_HOPS_REACHED_MESSAGE.to_string()),
    );
}

/// Streaming filter that drops in-band reasoning blocks from the visible
/// token stream.
///
/// Two reasoning-block conventions surface as raw tags in the per-token
/// stream and must be stripped before reaching the UI:
///
///   - **Qwen3** `<think>…</think>`. The plugin's chat template injects an
///     empty `<think>\n\n</think>\n\n` block at the start of the
///     generation even with `enable_thinking: false`, so the tags reach
///     the stream verbatim.
///   - **Gemma 4** `<|channel>thought\n…<channel|>`. The jinja template
///     injects `<|channel>thought\n<channel|>` as an assistant prefix
///     when `enable_thinking` is unset / false, and the 26B-A4B variant
///     in particular often writes content INSIDE that span before its
///     final answer (model card: "the model will still generate the tags
///     but with an empty thought block"). Same UX impact as the Qwen
///     case — without the strip the raw tag leaks into the chat bubble.
///
/// Both share the same shape (an open marker, free-form body, a close
/// marker), so the state machine is parameterised over the active marker
/// pair: on a hop start we don't know which family the live model is, so
/// the FIRST open marker the stream produces selects the pair and the
/// matching close terminates the span. Switching marker pairs mid-span
/// would mis-handle a model whose body happens to contain the OTHER
/// family's open tag literal.
///
/// Tags can also straddle chunk boundaries — `<thi` in chunk N and `nk>`
/// in chunk N+1, or `<|chan` and `nel>thought` — which is why a trailing
/// fragment that *could* still grow into ANY tracked open marker stays
/// pending until the next chunk arrives.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct ThinkState {
    /// The marker pair we are currently between, or `None` outside any
    /// reasoning span. Latched on the first open marker we observe; only
    /// that pair's matching close marker can clear it back to `None`.
    active: Option<&'static MarkerPair>,
    /// Bytes deferred from the previous chunk because they could still
    /// turn into a tag boundary on the next chunk's prefix.
    pending: String,
}

/// One reasoning-block shape: open marker → body → close marker.
#[derive(Debug)]
struct MarkerPair {
    open: &'static str,
    close: &'static str,
}

/// Reasoning-block markers tracked by [`ThinkState`]. New families
/// extend this slice; the open markers MUST be distinguishable byte
/// sequences (no shared prefix) so the open-marker scan in
/// `find_first_open` is deterministic.
const REASONING_MARKERS: &[MarkerPair] = &[
    MarkerPair {
        open: "<think>",
        close: "</think>",
    },
    // Gemma 4 channel tags. We strip the thought channel only — the
    // tool-response channel (`<|channel>tool_response`) is consumed by
    // the plugin's tool-call parser before the token stream reaches us,
    // so it would not appear here anyway; keeping the marker tight to
    // `<|channel>thought` avoids accidentally hiding any future
    // user-visible channel that reuses the `<|channel>` envelope.
    MarkerPair {
        open: "<|channel>thought",
        close: "<channel|>",
    },
];

impl ThinkState {
    fn strip(&mut self, delta: &str) -> String {
        let mut buf = std::mem::take(&mut self.pending);
        buf.push_str(delta);
        let mut visible = String::new();
        let mut cursor = 0;
        while cursor < buf.len() {
            if let Some(active) = self.active {
                if let Some(rel) = buf[cursor..].find(active.close) {
                    cursor += rel + active.close.len();
                    self.active = None;
                } else if let Some(safe) = max_safe_emit(&buf[cursor..], active.close) {
                    cursor += safe;
                    self.pending = buf[cursor..].to_string();
                    return visible;
                } else {
                    self.pending = buf[cursor..].to_string();
                    return visible;
                }
            } else if let Some((rel, pair)) = find_first_open(&buf[cursor..]) {
                visible.push_str(&buf[cursor..cursor + rel]);
                cursor += rel + pair.open.len();
                self.active = Some(pair);
            } else if let Some(safe) = max_safe_emit_any_open(&buf[cursor..]) {
                visible.push_str(&buf[cursor..cursor + safe]);
                cursor += safe;
                self.pending = buf[cursor..].to_string();
                return visible;
            } else {
                visible.push_str(&buf[cursor..]);
                return visible;
            }
        }
        visible
    }
}

/// First open marker present in `s`. Returns `(position, marker pair)` so
/// the caller advances by the matched marker's length. When two markers
/// appear at different positions we pick the earlier one; ties (same
/// position) prefer the longer literal so a future addition that shares
/// a strict prefix with an existing marker is still selected correctly.
fn find_first_open(s: &str) -> Option<(usize, &'static MarkerPair)> {
    let mut best: Option<(usize, &'static MarkerPair)> = None;
    for pair in REASONING_MARKERS {
        if let Some(pos) = s.find(pair.open) {
            match best {
                Some((bp, _)) if bp < pos => {}
                Some((bp, bpair)) if bp == pos && bpair.open.len() >= pair.open.len() => {}
                _ => best = Some((pos, pair)),
            }
        }
    }
    best
}

/// Longest "safe to emit now" prefix when the tail might still grow into
/// ANY tracked open marker. Returns `None` when nothing in the tail
/// matches a prefix of any open marker (caller may emit `s` whole).
fn max_safe_emit_any_open(s: &str) -> Option<usize> {
    let mut shortest: Option<usize> = None;
    for pair in REASONING_MARKERS {
        if let Some(safe) = max_safe_emit(s, pair.open) {
            shortest = Some(shortest.map_or(safe, |cur| cur.min(safe)));
        }
    }
    shortest
}

/// If `s` ends with a prefix of `marker`, return the length safe to emit
/// without splitting the marker — otherwise `None` (caller may emit all).
///
/// `marker` is ASCII (`<think>` / `</think>` / `<|channel>thought` /
/// `<channel|>`), but `s` is the model's raw token stream and can carry
/// multi-byte UTF-8 (e.g. `申` = 3 bytes). Slicing at `s.len() -
/// prefix_len` blindly panics when the boundary falls inside a codepoint;
/// skip non-boundary positions so the loop only considers safe suffixes.
fn max_safe_emit(s: &str, marker: &str) -> Option<usize> {
    // Compare from the longest possible suffix-prefix down to length 1.
    for prefix_len in (1..marker.len().min(s.len()) + 1).rev() {
        let cut = s.len() - prefix_len;
        if !s.is_char_boundary(cut) {
            continue;
        }
        let tail = &s[cut..];
        if marker.starts_with(tail) {
            return Some(cut);
        }
    }
    None
}

/// `LOOKBACK_CHAT_MAX_TOOL_HOPS` override; non-numeric falls back to default.
fn max_tool_hops() -> usize {
    std::env::var("LOOKBACK_CHAT_MAX_TOOL_HOPS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_TOOL_HOPS)
}

/// Pure transition so the agent loop's branching is testable without an
/// `AppHandle` or a live worker. llama-cpp-plugin's streaming contract
/// guarantees that the terminal `done=true` chunk carries the canonical
/// `pending_tool_calls` (its internal `ToolCallAccumulator` re-finalizes
/// the per-chunk `MessageContent::ToolCalls` deltas) — we rely on that
/// canonical channel and ignore the partial `assistant_tool_calls`
/// stream variant. If a future backend stops emitting the canonical
/// pending list, the contract has to be re-discussed; the
/// fallback-on-partial path was buggy (non-final partials were lost in
/// `chunk_to_updates_for_hop`) and is intentionally not present here.
fn drive_hop(decoded: crate::jobworkerp::llm_chat::ExtractedChunk) -> HopOutcome {
    if decoded.pending_tool_calls.is_empty() || !decoded.requires_tool_execution {
        return HopOutcome::Done;
    }
    HopOutcome::ContinueWithToolCalls(decoded.pending_tool_calls)
}

/// Per-hop variant of the chunk → events conversion. Suppresses the
/// per-hop Done event: the agent loop emits Done exactly once when the
/// conversation actually ends, after deciding the hop is the last one.
/// Returns the decoded chunk on `done=true` so the loop can branch on
/// `requires_tool_execution` / `pending_tool_calls` without re-decoding.
///
/// `think_state` carries the across-chunk state for stripping Qwen3-style
/// `<think>...</think>` reasoning blocks; the agent loop resets it per hop
/// (the model opens a fresh think block on each turn).
///
/// Exposed for integration tests in `tests/chat_agent_loop_e2e.rs` so we
/// can prove the UI's drain path produces the same `final_chunk` that the
/// raw `decode_chunk` path sees; not part of the stable API.
#[doc(hidden)]
pub fn chunk_to_updates_for_hop(
    job_id: &str,
    chunk: crate::jobworkerp::ListenChunk,
    last_started_call: &mut Option<String>,
    think_state: &mut ThinkState,
) -> (
    Vec<ChatStepUpdate>,
    Option<crate::jobworkerp::llm_chat::ExtractedChunk>,
) {
    use crate::jobworkerp::ListenChunk;
    use crate::jobworkerp::llm_chat::ExtractedChunk;

    let bytes = match &chunk {
        ListenChunk::Data(b) => b.as_slice(),
        ListenChunk::Final { collected: Some(b) } => b.as_slice(),
        ListenChunk::Final { collected: None } => {
            // Stream closed without a final chunk: surface an empty
            // ExtractedChunk so the loop sees "no tool calls" and emits Done.
            return (Vec::new(), Some(ExtractedChunk::default()));
        }
    };

    // Non-`LlmChatResult` bytes do leak in occasionally — the workflow
    // engine emits a `WorkflowResult` envelope on the final chunk of
    // some runners. Skip silently; the real `done` arrives on its own chunk.
    let Some(decoded) = crate::jobworkerp::llm_chat::decode_chunk(bytes) else {
        return (Vec::new(), None);
    };

    let ExtractedChunk {
        text,
        started,
        results,
        done,
        pending_tool_calls,
        requires_tool_execution,
    } = decoded;

    let mut updates = Vec::new();

    if let Some(started) = started {
        // Dedupe: a long tool call may surface the started field on several
        // chunks (the plugin keeps it set until the tool returns). Emit
        // `Searching` only on the transition.
        if last_started_call.as_deref() != Some(started.call_id.as_str()) {
            *last_started_call = Some(started.call_id.clone());
            updates.push(ChatStepUpdate::searching(
                job_id.to_string(),
                format!("searching memories ({})", started.fn_name),
            ));
        }
    }

    for result in &results {
        let sources = parse_lookback_sources(result);
        updates.push(ChatStepUpdate::source(job_id.to_string(), sources));
    }

    if let Some(delta) = text {
        // Qwen3 emits `<think>...</think>` reasoning even with
        // `enable_thinking: false` (the template injects an empty block).
        // Strip it from the per-token stream so the UI sees only the user-
        // facing answer.
        let visible = think_state.strip(&delta);
        if !visible.is_empty() {
            updates.push(ChatStepUpdate::token(job_id.to_string(), visible));
        }
    }

    // A "final chunk" worth handing to `drive_hop` is one that carries
    // the loop's decision data. Both plugins (llama-cpp and genai) now
    // emit a separated wire shape (specs/llama-cpp-plugin-streaming-tool-call-spec.md):
    //
    //   - Tool-call path:
    //       chunk M  : { done: false, pending_tool_calls: Some([...]),
    //                    requires_tool_execution: Some(true) }   ← THIS
    //       chunk M+1: { done: true,  ..Default::default() }      ← terminator
    //
    //   - Plain-text path:
    //       chunk N-1: { done: false, content: text-delta }
    //       chunk N  : { done: true,  content: <final text or None> }  ← THIS
    //
    // The OR keeps both terminal flavours alive: tool-call shape is
    // recognised by the signal fields before `done` is set; the plain-
    // text shape uses the conventional `done=true` terminator. The
    // first-wins guard in `run_chat_stream` keeps the tool-call chunk
    // from being overwritten by the subsequent `done=true` terminator.
    let has_tool_signal = !pending_tool_calls.is_empty() || requires_tool_execution;
    let final_chunk = (done || has_tool_signal).then_some(ExtractedChunk {
        text: None,
        started: None,
        results,
        done,
        pending_tool_calls,
        requires_tool_execution,
    });
    (updates, final_chunk)
}

/// Replace `lookback_recall`'s parameters block with the workflow's
/// user-facing input schema (`{query, limit_per_layer}`), keeping other
/// tools (if the function set grows later) untouched. We deliberately
/// hardcode the schema here instead of parsing the YAML at runtime so
/// the failure mode is a compile/test error if the contract changes,
/// not a silent drift between YAML and the LLM's tool description.
///
/// Exposed for integration tests in `tests/chat_agent_loop_e2e.rs`.
#[doc(hidden)]
pub fn rewrite_lookback_recall_tool_schema(tools_json: &str) -> String {
    let Ok(mut tools) = serde_json::from_str::<serde_json::Value>(tools_json) else {
        return tools_json.to_string();
    };
    let Some(arr) = tools.as_array_mut() else {
        return tools_json.to_string();
    };
    let user_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Natural-language search query in the user's language. \
                    The same string drives BM25 full-text search and is embedded for \
                    the vector branch."
            },
            "limit_per_layer": {
                "type": "integer",
                "default": 5,
                "description": "Max hits per layer (summary, raw). Default 5."
            },
            "summary_labels": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional thread-label filter for the summary layer. \
                    Use it when the query targets a specific kind or period instead \
                    of semantic similarity alone. Representative labels: \
                    \"daily_summary\" / \"weekly_summary\" / \"monthly_summary\" / \
                    \"summary\" (per-thread), plus period tags \"date:YYYY-MM-DD\" \
                    (daily), \"iso_week:YYYY-Www\" (weekly), and \"month:YYYY-MM\" \
                    (monthly). Resolve relative dates \
                    against the current date in the system prompt before filling this. \
                    Omit to search every summary kind."
            },
            "summary_label_match": {
                "type": "string",
                "enum": ["ANY", "ALL"],
                "default": "ALL",
                "description": "How summary_labels combine. \"ALL\" (default) requires \
                    every label — pin a kind AND a period, e.g. yesterday's daily \
                    summary on 2026-05-29 → summary_labels=[\"daily_summary\", \
                    \"date:2026-05-29\"], summary_label_match=\"ALL\". \"ANY\" matches \
                    summaries carrying at least one. Ignored when summary_labels is omitted."
            }
        },
        "required": ["query"]
    });
    for tool in arr.iter_mut() {
        let name_matches =
            tool.pointer("/function/name").and_then(|n| n.as_str()) == Some(LOOKBACK_RECALL_TOOL);
        if !name_matches {
            continue;
        }
        if let Some(func) = tool.get_mut("function").and_then(|f| f.as_object_mut()) {
            func.insert("parameters".to_string(), user_schema.clone());
        }
    }
    serde_json::to_string(&tools).unwrap_or_else(|_| tools_json.to_string())
}

/// Append the current local date to the base system prompt so the model
/// can resolve relative time expressions ("yesterday", "last week",
/// "this month", and their equivalents in the user's language) — the
/// retrieval is grounded in the user's history but the LLM otherwise has
/// no clock, so a "what did I do yesterday?" question has no anchor
/// without this line.
///
/// Generic over the timezone so the production call passes `Local::now()`
/// while tests pin a `FixedOffset` instant. The weekday and `+09:00`-style
/// offset are included because "last week" / "this week" need the
/// day-of-week and day boundaries are timezone-dependent.
fn dated_system_prompt<Tz: chrono::TimeZone>(now: chrono::DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    // e.g. "2026-05-30 (Sat) +09:00". RFC-style fragments the model is
    // already trained to parse. The instruction stays English to match
    // the base steering; the examples are illustrative (not strings to
    // emit), so English keeps the prompt internally consistent while
    // `Respond in the language the user is using` still drives the reply
    // language. The model resolves the user's own-language relatives
    // (「昨日」/"yesterday"/etc.) against this anchor regardless.
    let stamp = now.format("%Y-%m-%d (%a) %:z");
    format!(
        "{CHAT_SYSTEM_PROMPT}\n\nCurrent date and time: {stamp}. Resolve relative time expressions (e.g. \"yesterday\", \"last week\", \"this month\") against this before searching."
    )
}

/// Chat defaults applied when Settings hasn't overridden them. `max_tokens`
/// is the per-turn *output* budget (not the model's context size, which
/// belongs to the runner-side settings); 4000 covers a long-form answer
/// plus tool-call JSON without burning the GPU on a runaway generation.
/// `max_tokens` / `temperature` flow through as `Option`s so the chat
/// path is the ONLY consumer of the Settings UI values — summary /
/// personality / reflection workflows tune output ceilings per use-case
/// in YAML (e.g. monthly summary = 50k) and MUST NOT inherit a chat-tuned
/// clamp.
const CHAT_DEFAULT_MAX_TOKENS: u32 = 4000;
const CHAT_DEFAULT_TEMPERATURE: f32 = 0.3;

/// Build the `LlmChatArgs` payload for one hop. The chat path uses the
/// shared `client_tools_json` contract (`use_function_calling=false`) for
/// BOTH backends so the same schema rewrite
/// (`rewrite_lookback_recall_tool_schema`) and the same pending-tool loop
/// drive every provider:
///
/// - **Local (llama-cpp-plugin)**: rejects server-side auto-calling with
///   a hard `bail!`; the plugin instead parses tool calls from
///   `client_tools_json` and surfaces them as `pending_tool_calls`.
/// - **External (genai)**: upstream wired `client_tools_json` through to
///   the genai SDK in the same shape (see
///   `specs/external-llm-tool-calling-spec.md`).
///
/// Both plugins emit the separated streaming wire shape
/// (`pending_tool_calls` on a `done=false` chunk followed by a `done=true`
/// terminator — see `specs/llama-cpp-plugin-streaming-tool-call-spec.md`),
/// so the downstream `chunk_to_updates_for_hop` + first-wins guard handles
/// them identically.
///
/// `parallel_tool_calls: false` matches `chunk_to_updates_for_hop`'s
/// assumption that Searching/Source come from one call at a time. genai
/// 0.6 ignores this knob but it stays in the payload so the local path
/// keeps honouring it.
///
/// `chat_template_kwargs.enable_thinking` is local-only — external API
/// providers don't run jinja chat templates server-side, and the upstream
/// genai adapter drops the field. Per-preset polarity, gated by
/// [`super::llm_presets::ThinkingKwarg`]: `Disable`
/// (`enable_thinking:false`) is the Qwen3 path — it suppresses the
/// `<think>…</think>` block that swallows the tool call (QwenLM/Qwen3
/// #1817 + ggml-org/llama.cpp #20837). `Enable` (`enable_thinking:true`)
/// is the Gemma 4 path — it suppresses the
/// `<|channel>thought\n<channel|>` assistant prefix the jinja injects
/// otherwise, so the PEG-Gemma4 grammar can fire `<|tool_call>call:…` at
/// position 0; the plugin's `apply_oai_template_with_tools` mirrors the
/// kwarg into the C++ `OpenAIChatTemplateParams.enable_thinking` bool
/// that drives the grammar/parser pair. `None` skips the kwarg entirely
/// (models whose template does not branch on `enable_thinking`, plus the
/// custom free-text path where the family is unknown).
fn build_chat_args(
    messages: &[serde_json::Value],
    tools_json: &str,
    system_text: &str,
    external: bool,
    thinking_kwarg: super::llm_presets::ThinkingKwarg,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
) -> serde_json::Value {
    // LLMChatArgs has no `system_prompt` field; the proto-JSON
    // converter on jobworkerp's side drops unknown keys silently. The
    // only way the system steering reaches the model is as a SYSTEM
    // role ChatMessage at the head of the conversation.
    let mut all_messages: Vec<serde_json::Value> = Vec::with_capacity(messages.len() + 1);
    all_messages.push(serde_json::json!({
        "role": chat_role_for_proto("system"),
        "content": { "text": system_text },
    }));
    all_messages.extend_from_slice(messages);

    let mut function_options = serde_json::json!({
        "use_function_calling": false,
        "client_tools_json": tools_json,
        "tool_choice": "auto",
        "parallel_tool_calls": false,
    });
    // `chat_template_kwargs.enable_thinking` is llama-cpp/jinja specific —
    // see the build_chat_args doc above for the per-family polarity.
    // External providers (genai) ignore jinja chat templates so the kwarg
    // is always dropped on that path regardless of preset.
    if !external {
        use super::llm_presets::ThinkingKwarg;
        match thinking_kwarg {
            ThinkingKwarg::None => {}
            ThinkingKwarg::Disable => {
                function_options["chat_template_kwargs"] =
                    serde_json::json!(r#"{"enable_thinking":false}"#);
            }
            ThinkingKwarg::Enable => {
                function_options["chat_template_kwargs"] =
                    serde_json::json!(r#"{"enable_thinking":true}"#);
            }
        }
    }

    serde_json::json!({
        "messages": all_messages,
        "options": {
            "max_tokens": max_tokens.unwrap_or(CHAT_DEFAULT_MAX_TOKENS),
            "temperature": temperature.unwrap_or(CHAT_DEFAULT_TEMPERATURE),
        },
        "function_options": function_options,
    })
}

fn text_message_to_proto(m: &ChatMessage) -> serde_json::Value {
    serde_json::json!({
        "role": chat_role_for_proto(&m.role),
        "content": { "text": m.content },
    })
}

/// The `MessageContent.content` oneof can't carry Text and ToolCalls
/// simultaneously; assistant content is rendered as `null` by the plugin
/// in that case anyway.
fn assistant_tool_calls_proto(calls: &[ExtractedToolCall]) -> serde_json::Value {
    let calls_json: Vec<serde_json::Value> = calls
        .iter()
        .map(|c| {
            serde_json::json!({
                "call_id": c.call_id,
                "fn_name": c.fn_name,
                "fn_arguments": c.fn_arguments,
            })
        })
        .collect();
    serde_json::json!({
        "role": chat_role_for_proto("assistant"),
        "content": { "tool_calls": { "calls": calls_json } },
    })
}

/// Build a `ChatRole::TOOL` message carrying a client-executed tool result
/// (`specs/tool-result-message-content-spec.md`).
///
/// We emit a single-element `tool_results.results` array per hop — the spec
/// (§5) reserves multi-result batching for a future `parallel_tool_calls=true`
/// rollout. `fn_name` is always supplied (drawn from the ASSISTANT ToolCall
/// that triggered the hop) so the server-side reverse-scan fallback
/// never runs; Gemini in particular requires it on the wire.
fn tool_result_proto(
    call_id: &str,
    fn_name: &str,
    content: &str,
    is_error: bool,
) -> serde_json::Value {
    serde_json::json!({
        "role": chat_role_for_proto("tool"),
        "content": {
            "tool_results": {
                "results": [
                    {
                        "call_id": call_id,
                        "fn_name": fn_name,
                        "content": content,
                        "is_error": is_error,
                    }
                ]
            }
        },
    })
}

/// Build the `lookback_recall` dispatch args, injecting the memories gRPC
/// endpoint the workflow's `HybridSearch` calls must dial.
///
/// The LLM only ever sees `{query, limit_per_layer}` (the function-set
/// schema is rewritten down to that by `rewrite_lookback_recall_tool_schema`),
/// so the endpoint can't ride in as a tool argument — the model would
/// fabricate or omit a Snowflake-shaped host/port. Instead we resolve it
/// here from the active connection (`resolve_targets().memories_callback()`)
/// and merge `memories_grpc_{host,port,tls}` into the workflow input before
/// wrapping. This keeps RAG retrieval pointed at the SAME memories DB the
/// browse clients and the citation jump (`find_memory_position`) use — in
/// remote mode that's the configured remote URL, not the local sidecar.
/// Mirrors the `memories_grpc_*` input every summary/reflection/import batch
/// already passes (see `import.rs::callback_fields`).
///
/// Pure so the injected wire-shape is unit-tested without a live worker.
fn lookback_recall_dispatch_args(
    llm_args: &serde_json::Value,
    callback: &MemoriesCallback,
) -> serde_json::Value {
    // Start from whatever the LLM filled in (`{query, limit_per_layer,
    // summary_labels?, ...}`), then overlay the endpoint via
    // `MemoriesCallback::inject_into` so a hallucinated `memories_grpc_*`
    // key can never win over the resolved value. A non-object arg (the
    // model emitting a bare string) degrades to just the endpoint fields.
    let mut obj = llm_args.as_object().cloned().unwrap_or_default();
    callback.inject_into(&mut obj);
    super::wrap_workflow_run_args(&serde_json::Value::Object(obj))
}

/// Unwrap the WORKFLOW runner's `WorkflowResult` envelope (defined in
/// `app-wrapper/src/workflow/runner/unified.rs`) — its `output` field is
/// a JSON-encoded string carrying the workflow's real return. Other
/// shapes pass through, so the helper is idempotent.
fn unwrap_workflow_output(value: &serde_json::Value) -> serde_json::Value {
    let Some(obj) = value.as_object() else {
        return value.clone();
    };
    // Envelope marker: a string `output` AND one of the other
    // WorkflowResult fields — guards against a workflow whose real
    // output happens to be `{ "output": "..." }`.
    let output_is_string = obj.get("output").is_some_and(|v| v.is_string());
    let has_envelope_marker =
        obj.contains_key("id") || obj.contains_key("position") || obj.contains_key("status");
    if !(output_is_string && has_envelope_marker) {
        return value.clone();
    }
    let output_str = obj.get("output").and_then(|v| v.as_str()).unwrap_or("");
    serde_json::from_str(output_str).unwrap_or_else(|_| value.clone())
}

/// Returns `(text_for_llm, sources_for_ui)`. lookback_recall returns are
/// unwrapped from the WORKFLOW envelope before either consumer sees them
/// — without that the LLM reads `{id,output,position,...}` as "search
/// returned nothing" and re-calls the tool until the hop budget runs out.
fn build_tool_response(
    fn_name: &str,
    result_value: serde_json::Value,
) -> (String, Vec<ChatSource>) {
    let is_lookback = fn_name == LOOKBACK_RECALL_TOOL;
    let payload = if is_lookback {
        unwrap_workflow_output(&result_value)
    } else {
        result_value
    };
    let sources = if is_lookback {
        extract_lookback_sources(&payload)
    } else {
        Vec::new()
    };
    let tool_text = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    (tool_text, sources)
}

/// Normalize the lowercase role the UI sends ("user" / "assistant" /
/// "system") onto the protobuf `ChatRole` enum name. Unknown roles
/// fall back to USER so a typo'd role doesn't silently drop a message
/// the chat loop has already committed to history.
fn chat_role_for_proto(role: &str) -> &'static str {
    match role {
        "user" => "USER",
        "assistant" => "ASSISTANT",
        "system" => "SYSTEM",
        "tool" => "TOOL",
        _ => "USER",
    }
}

/// Adapter for the existing `ExtractedToolResult`-shaped contract that
/// `chunk_to_updates_for_hop` still needs (it iterates the per-chunk
/// `ToolExecutionResult`s that come from the streaming path). Skips
/// failed executions and non-lookback tools to keep the citation panel
/// quiet on errors — the LLM recovers on the next hop.
fn parse_lookback_sources(result: &ExtractedToolResult) -> Vec<ChatSource> {
    if !result.success || result.fn_name != LOOKBACK_RECALL_TOOL {
        return Vec::new();
    }
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&result.result) else {
        return Vec::new();
    };
    extract_lookback_sources(&payload)
}

/// Project the `sources` array of a `lookback_recall` result payload
/// into typed `ChatSource` entries. Malformed rows are dropped silently
/// — lets the frontend render zero hits as
/// "該当する記憶が見つかりませんでした".
fn extract_lookback_sources(payload: &serde_json::Value) -> Vec<ChatSource> {
    payload
        .get("sources")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| parse_one_source(entry.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Decode one entry from the `lookback_recall` `sources` array. Each
/// entry is the `projectHits` projection from
/// `workflows/rag/lookback-recall.yaml` — see the YAML for the field
/// contract. Returns `None` for any required field missing so a
/// malformed row doesn't poison the whole `Source` event.
fn parse_one_source(entry: serde_json::Value) -> Option<ChatSource> {
    let kind = entry.get("source_kind")?.as_str()?.to_string();
    let memory_id = parse_id(entry.get("memory_id"))?;
    let snippet = entry
        .get("snippet")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let score = entry
        .get("score")
        .and_then(|v| v.as_f64())
        .map(|f| f as f32)
        .unwrap_or(0.0);
    match kind.as_str() {
        "raw_memory" => {
            let source_thread_id = parse_id(entry.get("source_thread_id"))?;
            Some(ChatSource::RawMemory {
                memory_id,
                source_thread_id,
                snippet,
                score,
            })
        }
        "thread_summary" => {
            let source_thread_id = parse_id(entry.get("source_thread_id"))?;
            Some(ChatSource::ThreadSummary {
                memory_id,
                source_thread_id,
                snippet,
                score,
            })
        }
        "period_summary" => {
            let period_key = entry
                .get("period_key")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let scope_key = entry
                .get("scope_key")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Some(ChatSource::PeriodSummary {
                memory_id,
                period_key,
                scope_key,
                snippet,
                score,
            })
        }
        _ => None,
    }
}

/// Snowflake IDs come over the wire either as a JSON number (when the
/// YAML jq path lands on a numeric ID) or as a string (when it goes
/// through `.value` on a `*Id` protobuf message and protobuf-json
/// stringifies the int64). Accept both — narrowing to one shape now
/// would lock us into whichever path the current YAML happens to take.
fn parse_id(v: Option<&serde_json::Value>) -> Option<i64> {
    let v = v?;
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(s) = v.as_str() {
        return s.parse::<i64>().ok();
    }
    None
}

/// Cancel an in-flight RAG chat agent loop (OPEN-CHAT-2 / DECIDE-CHAT-4).
///
/// Flips the cancel token so the loop's between-hop checks bail out, and —
/// if a jobworkerp job is currently in flight — issues `JobService/Delete`
/// against it so the server side aborts immediately instead of running to
/// natural completion (which would keep the GPU busy).
///
/// Idempotent: an unknown or already-completed `job_id` is logged and
/// silently ignored so the UI can fire-and-forget on every Stop click.
#[tauri::command]
pub async fn chat_cancel(state: State<'_, AppState>, job_id: String) -> AppResult<()> {
    chat_cancel_inner(&state, &job_id).await
}

/// Body of [`chat_cancel`] split out so the no-live-jobworkerp paths can
/// be unit-tested against a synthetic [`AppState`] without a real
/// jobworkerp sidecar — the `live_jid = Some(_)` path stays e2e-only.
/// Delegates to the shared [`cancel_dispatch_inner`](super::cancel_dispatch_inner)
/// so chat / import / analysis run through the same cancel implementation.
pub(crate) async fn chat_cancel_inner(state: &AppState, job_id: &str) -> AppResult<()> {
    super::cancel_dispatch_inner(state, job_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grpc::proto::jobworkerp::runner::llm::{
        LlmChatResult, PendingToolCalls, ToolCallRequest, ToolExecutionResult,
        ToolExecutionStarted,
        llm_chat_result::{MessageContent, message_content::Content},
    };
    use crate::jobworkerp::ListenChunk;
    use crate::jobworkerp::llm_chat::ExtractedChunk;
    use prost::Message;

    fn user(msg: &str) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            content: msg.into(),
        }
    }

    fn proto(msgs: &[ChatMessage]) -> Vec<serde_json::Value> {
        msgs.iter().map(text_message_to_proto).collect()
    }

    /// Production-shaped system text the agent loop would pass to
    /// `build_chat_args` — keeps the `Local::now()` lookup off the test's
    /// hot path. Tests that need a pinned date use `dated_system_prompt`
    /// directly with a `FixedOffset` instant.
    fn sys_text() -> String {
        dated_system_prompt(chrono::Local::now())
    }

    /// Default `build_chat_args` call shape (`external=false`,
    /// `thinking_kwarg=Disable` — matches the historical bundled default
    /// preset, no generation overrides). Most tests only care about the
    /// message / tools shape; this helper keeps them from re-spelling the
    /// trailing arg block whenever the signature grows.
    fn build_default_chat_args(
        messages: &[serde_json::Value],
        tools_json: &str,
    ) -> serde_json::Value {
        build_chat_args(
            messages,
            tools_json,
            &sys_text(),
            false,
            super::super::llm_presets::ThinkingKwarg::Disable,
            None,
            None,
        )
    }

    /// Two canonical `MemoriesCallback` fixtures so the dispatch-args tests
    /// only spell out the cases that genuinely vary.
    fn local_cb() -> MemoriesCallback {
        MemoriesCallback {
            host: "127.0.0.1".into(),
            port: 9010,
            tls: false,
        }
    }

    /// Convenience helper for hop-level tests: returns just the events
    /// (the agent loop owns the captured final chunk).
    fn hop_updates(
        job_id: &str,
        chunk: ListenChunk,
        last: &mut Option<String>,
    ) -> Vec<ChatStepUpdate> {
        let mut think = ThinkState::default();
        chunk_to_updates_for_hop(job_id, chunk, last, &mut think).0
    }

    fn encode_text(text: &str, done: bool) -> Vec<u8> {
        LlmChatResult {
            content: Some(MessageContent {
                content: Some(Content::Text(text.to_string())),
            }),
            done,
            ..Default::default()
        }
        .encode_to_vec()
    }

    fn encode_started(call_id: &str, fn_name: &str, job_id: i64) -> Vec<u8> {
        LlmChatResult {
            tool_execution_started: Some(ToolExecutionStarted {
                call_id: call_id.into(),
                fn_name: fn_name.into(),
                job_id,
                fn_arguments: "{}".into(),
            }),
            ..Default::default()
        }
        .encode_to_vec()
    }

    fn encode_tool_result(call_id: &str, fn_name: &str, result_json: &str) -> Vec<u8> {
        LlmChatResult {
            tool_execution_results: vec![ToolExecutionResult {
                call_id: call_id.into(),
                fn_name: fn_name.into(),
                result: result_json.into(),
                error: None,
                success: true,
                job_id: Some(99),
            }],
            ..Default::default()
        }
        .encode_to_vec()
    }

    #[test]
    fn dated_system_prompt_appends_current_date_line() {
        use chrono::TimeZone;
        // A fixed instant so the assertion is deterministic regardless of
        // when the test runs. 2026-05-30 is a Saturday.
        let now = chrono::FixedOffset::east_opt(9 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 5, 30, 14, 5, 0)
            .unwrap();
        let prompt = dated_system_prompt(now);
        // The base steering must still be present...
        assert!(
            prompt.starts_with(CHAT_SYSTEM_PROMPT),
            "the date line is appended, not a replacement of the base prompt"
        );
        // ...followed by an absolute, parseable current-date line carrying
        // the weekday and offset so the model can resolve "yesterday" /
        // "last week".
        assert!(
            prompt.contains("2026-05-30"),
            "must carry the ISO date so relative terms resolve; got: {prompt}"
        );
        assert!(
            prompt.contains("Sat"),
            "weekday lets the model resolve last-week/this-week; got: {prompt}"
        );
        assert!(
            prompt.contains("+09:00"),
            "timezone offset disambiguates day boundaries; got: {prompt}"
        );
    }

    #[test]
    fn build_chat_args_system_message_carries_current_date() {
        // The SYSTEM message the model actually receives must include the
        // current date, otherwise a "what did I do yesterday?" question
        // has no anchor. The user query stays Japanese on purpose: the
        // date anchor must work regardless of the query's language.
        let v = build_default_chat_args(&proto(&[user("昨日やったことは?")]), "[]");
        let sys = v["messages"][0]["content"]["text"]
            .as_str()
            .expect("system text");
        assert!(
            sys.starts_with(CHAT_SYSTEM_PROMPT),
            "system message must keep the base RAG steering"
        );
        // A 4-digit year proves a concrete date was injected (not just the
        // static prompt). We don't pin the value — it's `Local::now()`.
        let has_year = sys.matches(char::is_numeric).count() >= 4 && sys.contains("-");
        assert!(
            has_year,
            "system message must carry an injected date: {sys}"
        );
    }

    #[test]
    fn build_chat_args_prepends_system_message_and_keeps_conversation_order() {
        let msgs = vec![
            user("what did we decide about caching?"),
            ChatMessage {
                role: "assistant".into(),
                content: "we tabled it last week".into(),
            },
            user("which week?"),
        ];
        let v = build_default_chat_args(&proto(&msgs), "[]");
        let arr = v["messages"].as_array().expect("messages array");
        // LLMChatArgs has no `system_prompt`, so the steering must
        // arrive as the SYSTEM-role first message.
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0]["role"], "SYSTEM");
        // The base steering is prefixed; a current-date line is appended
        // (see `build_chat_args_system_message_carries_current_date`).
        assert!(
            arr[0]["content"]["text"]
                .as_str()
                .is_some_and(|s| s.starts_with(CHAT_SYSTEM_PROMPT))
        );
        assert_eq!(arr[1]["role"], "USER");
        assert_eq!(
            arr[1]["content"]["text"],
            "what did we decide about caching?"
        );
        assert_eq!(arr[2]["role"], "ASSISTANT");
        assert_eq!(arr[3]["content"]["text"], "which week?");
        // No leftover `system_prompt` top-level field that would be
        // silently dropped by the proto-JSON converter.
        assert!(v.get("system_prompt").is_none());
    }

    #[test]
    fn build_chat_args_uses_defaults_when_no_overrides() {
        // Defaults exist so a Settings save with the fields left blank still
        // produces a usable chat dispatch; pinning them here prevents a
        // silent drift in the chat output budget.
        let v = build_default_chat_args(&proto(&[user("hi")]), "[]");
        assert_eq!(v["options"]["max_tokens"], CHAT_DEFAULT_MAX_TOKENS);
        // serde_json serializes f32 0.3 as the literal 0.3 — compare via the
        // f64 view to avoid representation pitfalls in case the default
        // changes precision.
        assert!(
            (v["options"]["temperature"].as_f64().unwrap() - CHAT_DEFAULT_TEMPERATURE as f64).abs()
                < 1e-6
        );
    }

    #[test]
    fn build_chat_args_applies_overrides_from_settings() {
        // Settings UI's max_tokens / temperature flow into the
        // chat dispatch. The bug fixed here was that build_chat_args
        // hard-coded 4000 / 0.3 and ignored the persisted overrides.
        let v = build_chat_args(
            &proto(&[user("hi")]),
            "[]",
            &sys_text(),
            false,
            super::super::llm_presets::ThinkingKwarg::Disable,
            Some(8000),
            Some(0.7),
        );
        assert_eq!(v["options"]["max_tokens"], 8000);
        assert!((v["options"]["temperature"].as_f64().unwrap() - 0.7).abs() < 1e-6);
    }

    #[test]
    fn chat_args_enable_client_side_tool_calling() {
        // Server-side auto-calling is OFF: the plugin rejects
        // `use_function_calling=true` with a `bail!`, so the agent loop
        // executes tools itself and feeds results back as TOOL-role
        // messages. Losing any of these `function_options` collapses
        // the chat from RAG to a plain LLM with no retrieval.
        let tools = "[{\"type\":\"function\",\"function\":{\"name\":\"lookback_recall\"}}]";
        let v = build_default_chat_args(&proto(&[user("hi")]), tools);
        let fo = &v["function_options"];
        assert_eq!(fo["use_function_calling"], false);
        assert_eq!(fo["client_tools_json"], tools);
        assert_eq!(fo["tool_choice"], "auto");
        assert_eq!(fo["parallel_tool_calls"], false);
    }

    #[test]
    fn chat_args_external_keeps_client_tools_json_path() {
        // External (genai) backend MUST receive client_tools_json with the
        // same shape as local: the upstream genai adapter now reads this
        // field and forwards it to the provider verbatim (see
        // `specs/external-llm-tool-calling-spec.md`). Regressing to
        // `use_function_calling=true` would route through the server-side
        // Function Set path and expose the WORKFLOW runner's internal
        // schema to the LLM, breaking lookback_recall arguments.
        let tools = "[{\"type\":\"function\",\"function\":{\"name\":\"lookback_recall\"}}]";
        let v = build_chat_args(
            &proto(&[user("hi")]),
            tools,
            &sys_text(),
            true,
            super::super::llm_presets::ThinkingKwarg::Disable,
            None,
            None,
        );
        let fo = &v["function_options"];
        assert_eq!(fo["use_function_calling"], false);
        assert_eq!(fo["client_tools_json"], tools);
        assert_eq!(fo["tool_choice"], "auto");
        assert_eq!(fo["parallel_tool_calls"], false);
    }

    #[test]
    fn chat_args_external_omits_chat_template_kwargs() {
        // External API providers don't run a jinja chat template
        // server-side, and the upstream genai adapter drops the field.
        // Keeping it out of the payload prevents a future genai version
        // that DOES inspect the value from picking up a local-only
        // toggle that doesn't apply to GPT/Claude/Gemini.
        // (`Disable` is force-set here to prove the `external` gate
        // alone is enough to drop it.)
        let v = build_chat_args(
            &proto(&[user("hi")]),
            "[]",
            &sys_text(),
            true,
            super::super::llm_presets::ThinkingKwarg::Disable,
            None,
            None,
        );
        assert!(
            v["function_options"].get("chat_template_kwargs").is_none(),
            "chat_template_kwargs must be absent on the external path: {}",
            v["function_options"]
        );
    }

    #[test]
    fn chat_args_local_omits_chat_template_kwargs_when_none() {
        // A preset whose chat template does NOT branch on `enable_thinking`
        // (or the custom free-text path, which can't infer the template)
        // MUST NOT receive the kwarg — llama-cpp errors on an unknown
        // chat-template kwarg, and the historical bug we're guarding
        // against is silently leaving the flag on for whatever model the
        // user just switched to.
        let v = build_chat_args(
            &proto(&[user("hi")]),
            "[]",
            &sys_text(),
            false,
            super::super::llm_presets::ThinkingKwarg::None,
            None,
            None,
        );
        assert!(
            v["function_options"].get("chat_template_kwargs").is_none(),
            "chat_template_kwargs must be absent when thinking_kwarg is None: {}",
            v["function_options"]
        );
    }

    #[test]
    fn chat_args_local_emits_enable_thinking_false_when_preset_disables_thinking() {
        // Qwen3 path: regression that drops the kwarg would let `<think>`
        // swallow the planned tool call (QwenLM/Qwen3 #1817).
        let v = build_chat_args(
            &proto(&[user("hi")]),
            "[]",
            &sys_text(),
            false,
            super::super::llm_presets::ThinkingKwarg::Disable,
            None,
            None,
        );
        let ctk = v["function_options"]
            .get("chat_template_kwargs")
            .expect("kwarg must be present for Disable presets");
        assert_eq!(
            ctk.as_str().unwrap(),
            "{\"enable_thinking\":false}",
            "kwarg payload must be the JSON-string form llama-cpp expects",
        );
    }

    #[test]
    fn chat_args_local_emits_enable_thinking_true_when_preset_enables_thinking() {
        // Gemma 4 path (regression for "Gemma RAG never fires"): we MUST
        // send `enable_thinking:true` so the jinja omits the
        // `<|channel>thought\n<channel|>` prefix and the PEG-Gemma4
        // grammar can emit `<|tool_call>call:…` at position 0. If this
        // ever flips back to `false`, Gemma 4 silently stops calling
        // tools and replies "no records found".
        let v = build_chat_args(
            &proto(&[user("hi")]),
            "[]",
            &sys_text(),
            false,
            super::super::llm_presets::ThinkingKwarg::Enable,
            None,
            None,
        );
        let ctk = v["function_options"]
            .get("chat_template_kwargs")
            .expect("kwarg must be present for Enable presets");
        assert_eq!(
            ctk.as_str().unwrap(),
            "{\"enable_thinking\":true}",
            "kwarg payload must enable thinking for Gemma 4 family",
        );
    }

    #[test]
    fn rewrite_lookback_recall_tool_schema_replaces_parameters() {
        // The FunctionService surfaces the WORKFLOW runner's args
        // schema (`workflowContext`, `workflowData`, `input`, etc.).
        // The rewrite must collapse that into the workflow's
        // user-facing input contract for lookback_recall so the LLM
        // sees only the fields it should actually fill in.
        let raw = r#"[{
            "type": "function",
            "function": {
                "name": "lookback_recall",
                "description": "...",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "workflowContext": { "type": "string" },
                        "workflowData": { "type": "string" },
                        "input": { "type": "string" }
                    }
                }
            }
        }]"#;
        let rewritten = rewrite_lookback_recall_tool_schema(raw);
        let v: serde_json::Value = serde_json::from_str(&rewritten).unwrap();
        let params = &v[0]["function"]["parameters"];
        assert_eq!(params["type"], "object");
        let props = params["properties"].as_object().unwrap();
        assert!(props.contains_key("query"), "must expose query");
        assert!(
            props.contains_key("limit_per_layer"),
            "must expose limit_per_layer"
        );
        // The label-filter params must be visible so the LLM can narrow
        // the summary layer by kind/period.
        assert!(
            props.contains_key("summary_labels"),
            "must expose summary_labels so the LLM can filter by kind/period"
        );
        assert_eq!(
            props["summary_labels"]["type"], "array",
            "summary_labels must be an array of label strings"
        );
        assert!(
            props.contains_key("summary_label_match"),
            "must expose summary_label_match (ANY/ALL)"
        );
        assert!(
            !props.contains_key("workflowContext"),
            "WORKFLOW runner internals must not leak to the LLM"
        );
        // Only `query` is required; the label filter is optional so
        // open-ended questions still work without it.
        assert_eq!(params["required"][0], "query");
        assert_eq!(
            params["required"].as_array().map(|a| a.len()),
            Some(1),
            "label params must stay optional"
        );
    }

    #[test]
    fn rewrite_lookback_recall_tool_schema_leaves_other_tools_alone() {
        let raw = r#"[{
            "type": "function",
            "function": {
                "name": "some_other_tool",
                "parameters": { "type": "object", "properties": { "x": { "type": "string" } } }
            }
        }]"#;
        let rewritten = rewrite_lookback_recall_tool_schema(raw);
        // Unchanged content but accept any JSON reformatting.
        let original: serde_json::Value = serde_json::from_str(raw).unwrap();
        let after: serde_json::Value = serde_json::from_str(&rewritten).unwrap();
        assert_eq!(original, after);
    }

    #[test]
    fn lookback_recall_dispatch_args_injects_local_endpoint() {
        // The LLM only fills in {query, limit_per_layer}; the memories
        // gRPC endpoint is injected from the resolved connection so RAG
        // retrieval hits the same DB the citation jump resolves against.
        let llm_args = serde_json::json!({ "query": "cache", "limit_per_layer": 3 });
        let dispatch = lookback_recall_dispatch_args(&llm_args, &local_cb());
        // WORKFLOW `run` shape: { "input": "<json string>" }.
        let input_str = dispatch["input"].as_str().expect("input is a string");
        let input: serde_json::Value = serde_json::from_str(input_str).expect("input parses");
        assert_eq!(input["query"], "cache");
        assert_eq!(input["limit_per_layer"], 3);
        assert_eq!(input["memories_grpc_host"], "127.0.0.1");
        assert_eq!(input["memories_grpc_port"], 9010);
        assert_eq!(input["memories_grpc_tls"], false);
    }

    #[test]
    fn lookback_recall_dispatch_args_passes_through_summary_label_filter() {
        // The LLM-supplied label filter must reach the workflow input
        // untouched (the endpoint injection only overlays memories_grpc_*),
        // so "yesterday's daily summary" narrows the summary layer instead
        // of relying on semantic similarity.
        let llm_args = serde_json::json!({
            "query": "昨日やったこと",
            "summary_labels": ["daily_summary", "date:2026-05-29"],
            "summary_label_match": "ALL"
        });
        let dispatch = lookback_recall_dispatch_args(&llm_args, &local_cb());
        let input_str = dispatch["input"].as_str().expect("input is a string");
        let input: serde_json::Value = serde_json::from_str(input_str).expect("input parses");
        assert_eq!(
            input["summary_labels"],
            serde_json::json!(["daily_summary", "date:2026-05-29"]),
            "label filter must survive the endpoint merge verbatim"
        );
        assert_eq!(input["summary_label_match"], "ALL");
        // And the endpoint is still injected alongside it.
        assert_eq!(input["memories_grpc_host"], "127.0.0.1");
    }

    #[test]
    fn lookback_recall_dispatch_args_propagates_remote_https() {
        // Remote mode with an HTTPS memories URL must surface tls=true and
        // the remote host/port, NOT the local sidecar — this is the fix
        // for "all citation links dangle in remote mode" (search hit the
        // local DB while the jump resolved against the remote DB).
        let llm_args = serde_json::json!({ "query": "decisions" });
        let callback = MemoriesCallback {
            host: "memories.example.com".into(),
            port: 8443,
            tls: true,
        };
        let dispatch = lookback_recall_dispatch_args(&llm_args, &callback);
        let input_str = dispatch["input"].as_str().expect("input is a string");
        let input: serde_json::Value = serde_json::from_str(input_str).expect("input parses");
        assert_eq!(input["memories_grpc_host"], "memories.example.com");
        assert_eq!(input["memories_grpc_port"], 8443);
        assert_eq!(input["memories_grpc_tls"], true);
        // The LLM-supplied query survives the merge.
        assert_eq!(input["query"], "decisions");
    }

    #[test]
    fn lookback_recall_dispatch_args_overrides_hallucinated_endpoint() {
        // If the model ever emits a memories_grpc_* key (it shouldn't —
        // the rewritten schema hides them), the resolved endpoint must
        // win so a fabricated host can't redirect the search.
        let llm_args = serde_json::json!({
            "query": "x",
            "memories_grpc_host": "evil.example.com",
            "memories_grpc_port": 1,
            "memories_grpc_tls": false,
        });
        let callback = MemoriesCallback {
            host: "real.example.com".into(),
            port: 9010,
            tls: true,
        };
        let dispatch = lookback_recall_dispatch_args(&llm_args, &callback);
        let input_str = dispatch["input"].as_str().unwrap();
        let input: serde_json::Value = serde_json::from_str(input_str).unwrap();
        assert_eq!(input["memories_grpc_host"], "real.example.com");
        assert_eq!(input["memories_grpc_port"], 9010);
        assert_eq!(input["memories_grpc_tls"], true);
    }

    #[test]
    fn build_tool_response_unwraps_workflow_envelope_for_lookback_recall() {
        // WORKFLOW runner returns its inner output wrapped in a
        // WorkflowResult envelope where `output` is a JSON string. If
        // we don't unwrap, the LLM sees the envelope and thinks the
        // tool returned nothing useful, then re-calls until hop budget.
        let envelope = serde_json::json!({
            "id": "01HABCxyz",
            "output": "{\"sources\":[{\"source_kind\":\"raw_memory\",\"memory_id\":\"42\",\"source_thread_id\":\"7\",\"snippet\":\"hello\",\"score\":0.9}]}",
            "position": "/ROOT/do/3/projectHits",
            "status": 1
        });
        let (text, sources) = build_tool_response("lookback_recall", envelope);
        assert_eq!(
            sources.len(),
            1,
            "must extract sources from unwrapped payload"
        );
        // The LLM-visible text is the inner workflow output, NOT the
        // envelope — that's what tells it the search actually returned
        // something.
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            parsed.get("sources").is_some(),
            "tool_text must carry `sources`"
        );
        assert!(
            parsed.get("id").is_none(),
            "envelope `id` must not leak to the LLM"
        );
        assert!(
            parsed.get("position").is_none(),
            "envelope `position` must not leak"
        );
    }

    #[test]
    fn build_tool_response_passes_through_when_not_an_envelope() {
        // Test fixtures (and any future tool that returns its workflow
        // output unwrapped) must still work.
        let raw = serde_json::json!({"sources": []});
        let (text, sources) = build_tool_response("lookback_recall", raw.clone());
        assert_eq!(sources.len(), 0);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, raw);
    }

    #[test]
    fn unwrap_workflow_output_leaves_non_workflow_tools_alone() {
        // Other tools (not lookback_recall) might legitimately have an
        // `output` field on their result. Be conservative: require BOTH
        // `output: string` AND at least one envelope marker (id/position/
        // status) before unwrapping.
        let other = serde_json::json!({"output": "literal-string-no-envelope"});
        assert_eq!(unwrap_workflow_output(&other), other);
    }

    #[test]
    fn chat_args_thinking_kwarg_wire_shape_is_json_string() {
        // The plugin passes `function_options.chat_template_kwargs`
        // through to the jinja template verbatim, so the value must be a
        // JSON-object *string* (not a nested JSON object) — otherwise
        // the jinja kwargs binder rejects it and the chat request errors.
        // build_default_chat_args uses `Disable`, so we also assert the
        // payload encodes `enable_thinking:false`.
        let v = build_default_chat_args(&proto(&[user("hi")]), "[]");
        let kwargs = &v["function_options"]["chat_template_kwargs"];
        let s = kwargs
            .as_str()
            .expect("chat_template_kwargs must be a JSON string");
        let parsed: serde_json::Value =
            serde_json::from_str(s).expect("chat_template_kwargs must parse as JSON");
        assert_eq!(parsed["enable_thinking"], false);
    }

    #[test]
    fn chat_role_for_proto_normalizes_known_roles() {
        assert_eq!(chat_role_for_proto("user"), "USER");
        assert_eq!(chat_role_for_proto("assistant"), "ASSISTANT");
        assert_eq!(chat_role_for_proto("system"), "SYSTEM");
        assert_eq!(chat_role_for_proto("tool"), "TOOL");
        // Unknown roles should still produce a valid ChatRole so the
        // message isn't dropped before the LLM sees it.
        assert_eq!(chat_role_for_proto("weird"), "USER");
    }

    #[test]
    fn chat_phase_serializes_kebab_case() {
        // The frontend pattern-matches on the kebab-cased phase strings.
        // Pin them so a future #[serde(rename_all)] flip doesn't
        // silently break the UI.
        for (p, expected) in [
            (ChatPhase::Start, "\"start\""),
            (ChatPhase::Searching, "\"searching\""),
            (ChatPhase::Source, "\"source\""),
            (ChatPhase::Token, "\"token\""),
            (ChatPhase::Done, "\"done\""),
            (ChatPhase::Error, "\"error\""),
        ] {
            assert_eq!(serde_json::to_string(&p).unwrap(), expected);
        }
    }

    // -- ChatSource serde -------------------------------------------------

    #[test]
    fn chat_source_raw_memory_serializes_with_discriminator() {
        let s = ChatSource::RawMemory {
            memory_id: 7_462_752_159_340_220_411,
            source_thread_id: 7_462_752_159_340_220_412,
            snippet: "the cache decision".into(),
            score: 0.87,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["source_kind"], "raw_memory");
        // IDs go on the wire as strings (snowflakes overflow JS number).
        assert_eq!(v["memory_id"], "7462752159340220411");
        assert_eq!(v["source_thread_id"], "7462752159340220412");
        assert_eq!(v["snippet"], "the cache decision");
    }

    #[test]
    fn chat_source_thread_summary_uses_correct_kind() {
        let s = ChatSource::ThreadSummary {
            memory_id: 100,
            source_thread_id: 200,
            snippet: "summary".into(),
            score: 0.5,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["source_kind"], "thread_summary");
        assert_eq!(v["source_thread_id"], "200");
    }

    #[test]
    fn chat_source_period_summary_omits_source_thread_id() {
        // period_summary deliberately has no
        // source_thread_id; the discriminated union enforces this at
        // the type level, but pin the serde shape too.
        let s = ChatSource::PeriodSummary {
            memory_id: 300,
            period_key: "2026-05-24".into(),
            scope_key: "user-1".into(),
            snippet: "daily summary".into(),
            score: 0.7,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["source_kind"], "period_summary");
        assert_eq!(v["period_key"], "2026-05-24");
        assert_eq!(v["scope_key"], "user-1");
        assert!(
            v.get("source_thread_id").is_none(),
            "period_summary must not carry source_thread_id; got {v}"
        );
    }

    // -- chunk_to_updates -------------------------------------------------

    #[test]
    fn token_chunk_emits_single_token_update() {
        let mut last: Option<String> = None;
        let updates = hop_updates(
            "chat-1",
            ListenChunk::Data(encode_text("Hello", false)),
            &mut last,
        );
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].phase, ChatPhase::Token);
        assert_eq!(updates[0].token_delta.as_deref(), Some("Hello"));
        assert!(last.is_none(), "text chunks must not touch the dedupe key");
    }

    #[test]
    fn empty_text_with_done_suppresses_per_hop_done() {
        // Per-hop Done emit is the agent loop's responsibility, not the
        // chunk translator's. The hop translator must keep the wire
        // quiet on the terminal chunk so the loop can emit Done exactly
        // once at the end of the conversation.
        let mut last: Option<String> = None;
        let mut think = ThinkState::default();
        let (updates, captured) = chunk_to_updates_for_hop(
            "chat-1",
            ListenChunk::Data(encode_text("", true)),
            &mut last,
            &mut think,
        );
        assert!(
            updates.is_empty(),
            "terminal chunk must not produce events for the hop"
        );
        let captured = captured.expect("final chunk should be captured");
        assert!(captured.done);
    }

    #[test]
    fn started_then_repeat_emits_searching_once() {
        // Searching is the enter-edge for the tool call.
        // A long-running tool may surface tool_execution_started on
        // multiple chunks; we emit Searching only on the call_id
        // transition.
        let mut last: Option<String> = None;
        let first = hop_updates(
            "chat-1",
            ListenChunk::Data(encode_started("call-1", "lookback_recall", 42)),
            &mut last,
        );
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].phase, ChatPhase::Searching);
        assert!(
            first[0]
                .message
                .as_deref()
                .unwrap()
                .contains("lookback_recall")
        );

        let repeat = hop_updates(
            "chat-1",
            ListenChunk::Data(encode_started("call-1", "lookback_recall", 42)),
            &mut last,
        );
        assert!(
            repeat.is_empty(),
            "repeated started for same call_id must not re-emit Searching"
        );

        let next_call = hop_updates(
            "chat-1",
            ListenChunk::Data(encode_started("call-2", "lookback_recall", 43)),
            &mut last,
        );
        assert_eq!(next_call.len(), 1);
        assert_eq!(next_call[0].phase, ChatPhase::Searching);
    }

    #[test]
    fn tool_result_emits_source_with_parsed_entries() {
        // lookback_recall returns { sources: [...] } per its YAML.
        // Each ToolExecutionResult chunk produces exactly one Source
        // event carrying the parsed entries (zero or more).
        let payload = serde_json::json!({
            "sources": [
                {
                    "source_kind": "raw_memory",
                    "memory_id": "111",
                    "source_thread_id": "222",
                    "snippet": "hello",
                    "score": 0.9
                },
                {
                    "source_kind": "thread_summary",
                    "memory_id": "333",
                    "source_thread_id": "444",
                    "snippet": "summary",
                    "score": 0.8
                }
            ]
        });
        let bytes = encode_tool_result("call-1", "lookback_recall", &payload.to_string());
        let mut last: Option<String> = None;
        let updates = hop_updates("chat-1", ListenChunk::Data(bytes), &mut last);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].phase, ChatPhase::Source);
        let sources = updates[0].sources.as_ref().expect("sources populated");
        assert_eq!(sources.len(), 2);
        assert!(matches!(
            sources[0],
            ChatSource::RawMemory { memory_id: 111, .. }
        ));
        assert!(matches!(
            sources[1],
            ChatSource::ThreadSummary { memory_id: 333, .. }
        ));
    }

    #[test]
    fn unknown_tool_result_yields_empty_sources() {
        // A tool we don't recognise (none in MVP, but kept future-proof)
        // still produces a Source phase event so the frontend can
        // surface "tool ran but no citations to render".
        let bytes = encode_tool_result(
            "call-1",
            "some_other_tool",
            r#"{"sources":[{"source_kind":"raw_memory","memory_id":"1","source_thread_id":"2","snippet":"","score":0.0}]}"#,
        );
        let mut last: Option<String> = None;
        let updates = hop_updates("chat-1", ListenChunk::Data(bytes), &mut last);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].phase, ChatPhase::Source);
        assert_eq!(updates[0].sources.as_ref().unwrap().len(), 0);
    }

    #[test]
    fn final_with_no_collected_yields_empty_chunk_for_loop_to_finish() {
        // Defensive: a server that closes without a `done=true` chunk
        // gives the agent loop an empty ExtractedChunk so it can branch
        // (no tool calls → Done). The per-hop translator emits no
        // events here; the loop is in charge of the final Done.
        let mut last: Option<String> = None;
        let mut think = ThinkState::default();
        let (updates, captured) = chunk_to_updates_for_hop(
            "chat-1",
            ListenChunk::Final { collected: None },
            &mut last,
            &mut think,
        );
        assert!(updates.is_empty());
        let captured = captured.expect("loop needs a chunk to branch on");
        assert!(!captured.requires_tool_execution);
        assert!(captured.pending_tool_calls.is_empty());
    }

    #[test]
    fn invalid_bytes_are_dropped_silently() {
        // The WORKFLOW engine occasionally interleaves non-LlmChatResult
        // envelopes on the stream (`WorkflowResult` etc.). Drop them
        // rather than emitting Error — the real done chunk still
        // arrives separately.
        let mut last: Option<String> = None;
        let updates = hop_updates(
            "chat-1",
            ListenChunk::Data(b"\x01\x02not protobuf".to_vec()),
            &mut last,
        );
        assert!(updates.is_empty());
    }

    // -- parse_lookback_sources -------------------------------------------

    #[test]
    fn parse_lookback_sources_accepts_numeric_ids() {
        // The YAML jq path can land on numeric IDs (`.value` is int64
        // in protobuf-json) — parse_id handles both.
        let result = ExtractedToolResult {
            call_id: "c1".into(),
            fn_name: "lookback_recall".into(),
            result: r#"{"sources":[{"source_kind":"raw_memory","memory_id":111,"source_thread_id":222,"snippet":"x","score":0.5}]}"#.into(),
            error: None,
            success: true,
            job_id: Some(1),
        };
        let parsed = parse_lookback_sources(&result);
        assert!(matches!(
            parsed.first(),
            Some(ChatSource::RawMemory {
                memory_id: 111,
                source_thread_id: 222,
                ..
            })
        ));
    }

    #[test]
    fn parse_lookback_sources_returns_empty_on_failed_tool() {
        let result = ExtractedToolResult {
            call_id: "c1".into(),
            fn_name: "lookback_recall".into(),
            result: r#"{"sources":[{"source_kind":"raw_memory","memory_id":1,"source_thread_id":2,"snippet":"","score":0.0}]}"#.into(),
            error: Some("boom".into()),
            success: false,
            job_id: Some(1),
        };
        assert!(parse_lookback_sources(&result).is_empty());
    }

    // -- agent loop transition --------------------------------------------

    fn make_pending_call(call_id: &str, fn_name: &str, args: &str) -> ExtractedToolCall {
        ExtractedToolCall {
            call_id: call_id.into(),
            fn_name: fn_name.into(),
            fn_arguments: args.into(),
        }
    }

    #[test]
    fn drive_hop_continues_when_requires_tool_execution() {
        // Plugin signal: requires_tool_execution=true + non-empty
        // pending_tool_calls → loop must run those tools next.
        let decoded = ExtractedChunk {
            done: true,
            requires_tool_execution: true,
            pending_tool_calls: vec![make_pending_call("c1", "lookback_recall", "{\"q\":\"x\"}")],
            ..Default::default()
        };
        match drive_hop(decoded) {
            HopOutcome::ContinueWithToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].call_id, "c1");
                assert_eq!(calls[0].fn_name, "lookback_recall");
            }
            other => panic!("expected ContinueWithToolCalls, got {other:?}"),
        }
    }

    #[test]
    fn drive_hop_ignores_message_content_tool_calls_variant() {
        // `MessageContent::ToolCalls` partials are dropped at decode
        // time now (the plugin's accumulator re-finalizes them onto
        // the canonical `pending_tool_calls` of the terminal chunk).
        // A decoded chunk with empty pending must therefore short-
        // circuit to Done regardless of any other flag state.
        let decoded = ExtractedChunk {
            done: true,
            requires_tool_execution: true,
            pending_tool_calls: Vec::new(),
            ..Default::default()
        };
        assert_eq!(drive_hop(decoded), HopOutcome::Done);
    }

    #[test]
    fn drive_hop_done_when_no_tool_calls() {
        // Final chunk with no tool calls and requires_tool_execution=false
        // (or absent) → loop should emit Done and exit.
        let decoded = ExtractedChunk {
            done: true,
            requires_tool_execution: false,
            ..Default::default()
        };
        assert_eq!(drive_hop(decoded), HopOutcome::Done);
    }

    #[test]
    fn drive_hop_done_when_pending_present_but_flag_false() {
        // The plugin's contract: pending_tool_calls is only actionable
        // when requires_tool_execution=true. If the flag is false the
        // calls are advisory (or stale) and the loop terminates.
        let decoded = ExtractedChunk {
            done: true,
            requires_tool_execution: false,
            pending_tool_calls: vec![make_pending_call("c1", "lookback_recall", "{}")],
            ..Default::default()
        };
        assert_eq!(drive_hop(decoded), HopOutcome::Done);
    }

    #[test]
    fn drive_hop_done_on_default_chunk() {
        // Server-closed-without-final case: an empty ExtractedChunk
        // must be treated as "no more work" so the user always sees Done.
        assert_eq!(drive_hop(ExtractedChunk::default()), HopOutcome::Done);
    }

    // -- ThinkState (Qwen3 reasoning strip) -------------------------------

    #[test]
    fn think_state_strips_empty_block() {
        let mut s = ThinkState::default();
        // Qwen3 default: empty <think>\n\n</think>\n\n at the start.
        let out = s.strip("<think>\n\n</think>\n\nhello");
        assert_eq!(out, "\n\nhello");
        assert!(s.active.is_none());
        assert!(s.pending.is_empty());
    }

    #[test]
    fn think_state_strips_block_spanning_chunks() {
        let mut s = ThinkState::default();
        assert_eq!(s.strip("<th"), "");
        assert_eq!(s.strip("ink>thoughts</thi"), "");
        // The remainder of </think> arrives + visible payload.
        assert_eq!(s.strip("nk>visible"), "visible");
        assert!(s.active.is_none());
    }

    #[test]
    fn think_state_passes_text_when_no_tags() {
        let mut s = ThinkState::default();
        assert_eq!(s.strip("hello "), "hello ");
        assert_eq!(s.strip("world"), "world");
    }

    #[test]
    fn think_state_handles_text_before_open() {
        let mut s = ThinkState::default();
        assert_eq!(
            s.strip("answer: <think>reasoning</think>done"),
            "answer: done"
        );
    }

    #[test]
    fn think_state_buffers_potential_open_at_end() {
        let mut s = ThinkState::default();
        // "ab<" — the "<" could still grow into "<think>", so we buffer it.
        assert_eq!(s.strip("ab<"), "ab");
        // Next chunk does not extend to "<think>"; flush as-is.
        assert_eq!(s.strip("br>"), "<br>");
    }

    #[test]
    fn think_state_unterminated_block_is_buffered() {
        let mut s = ThinkState::default();
        assert_eq!(s.strip("<think>still thinking"), "");
        assert!(s.active.is_some());
        // No </think> in this batch either; still suppressed.
        assert_eq!(s.strip(" more"), "");
        assert!(s.active.is_some());
    }

    // -- ThinkState (Gemma 4 channel-tag strip) ---------------------------

    #[test]
    fn think_state_strips_gemma_channel_thought_inline() {
        // Gemma 4 26B-A4B's jinja primes generations with
        // `<|channel>thought\n<channel|>`. The model often writes its
        // actual answer right after the close marker; the UI must not
        // see either tag.
        let mut s = ThinkState::default();
        let out = s.strip("<|channel>thought\n<channel|>申し訳ありません");
        assert_eq!(out, "申し訳ありません");
        assert!(s.active.is_none());
    }

    #[test]
    fn think_state_strips_gemma_channel_with_body() {
        // The 26B-A4B variant sometimes writes private reasoning between
        // the open and close (the model card's "empty thought block" is
        // the happy path; the failure mode is non-empty). Whatever it is,
        // must not surface in the chat bubble.
        let mut s = ThinkState::default();
        let out = s.strip("<|channel>thought\nlet me search<channel|>final answer");
        assert_eq!(out, "final answer");
        assert!(s.active.is_none());
    }

    #[test]
    fn think_state_strips_gemma_channel_spanning_chunks() {
        // Same boundary-split guarantee as the Qwen3 path: open / close
        // markers may straddle chunk boundaries.
        let mut s = ThinkState::default();
        assert_eq!(s.strip("<|chan"), "");
        assert_eq!(s.strip("nel>thought\nbody<chan"), "");
        assert_eq!(s.strip("nel|>visible"), "visible");
        assert!(s.active.is_none());
    }

    #[test]
    fn think_state_does_not_swallow_unrelated_pipe_tags() {
        // A plain "<|" prefix that does NOT extend into the Gemma open
        // marker must flush eventually — otherwise unrelated content
        // could be buffered forever. Two-chunk reproduction: first
        // partial that COULD become `<|channel>thought`, then a chunk
        // that conclusively diverges.
        let mut s = ThinkState::default();
        assert_eq!(s.strip("ok <|"), "ok ");
        // `<|note>` is a bystander tag; once the prefix can no longer
        // grow into `<|channel>thought`, the buffer flushes.
        assert_eq!(s.strip("note>tail"), "<|note>tail");
        assert!(s.active.is_none());
    }

    #[test]
    fn think_state_handles_multibyte_chars_at_chunk_boundary() {
        // Regression: max_safe_emit used to slice the buffer at byte
        // offsets that could fall inside a Japanese codepoint
        // (`申` = 0xE7 0x94 0xB3, 3 bytes), panicking with
        // "byte index ... is not a char boundary". The fix is to skip
        // non-boundary cut positions when probing for a trailing
        // marker prefix.
        let mut s = ThinkState::default();
        // Plain Japanese text without any tag should pass through
        // unchanged regardless of how chunk boundaries fall.
        assert_eq!(s.strip("申し訳"), "申し訳");
        // Even when the buffer is small enough that the would-be
        // suffix probe lands mid-codepoint, the strip call must not
        // panic.
        let mut s2 = ThinkState::default();
        assert_eq!(s2.strip("\n\n申し訳"), "\n\n申し訳");
    }

    #[test]
    fn max_safe_emit_skips_non_char_boundary() {
        // `申` is 3 bytes (0xE7 0x94 0xB3). With marker="<" (or any
        // ASCII marker), prefix_len=1 would slice `s[s.len()-1..]`
        // which lands inside the codepoint — must not panic.
        assert_eq!(max_safe_emit("申", "<think>"), None);
        // A real trailing "<" still gets detected as a possible opener.
        assert_eq!(max_safe_emit("hi<", "<think>"), Some(2));
    }

    // -- agent loop hop budget --------------------------------------------

    #[test]
    fn max_tool_hops_reads_env_override() {
        // Use a fresh key per scope to avoid colliding with other tests
        // run in the same process. The DEFAULT path is exercised by the
        // happy-path drive_hop tests above.
        // SAFETY: these tests must run with --test-threads=1, as
        // documented in the workspace CLAUDE.md.
        unsafe { std::env::set_var("LOOKBACK_CHAT_MAX_TOOL_HOPS", "0") };
        assert_eq!(max_tool_hops(), 0);
        unsafe { std::env::set_var("LOOKBACK_CHAT_MAX_TOOL_HOPS", "9") };
        assert_eq!(max_tool_hops(), 9);
        // Non-numeric → falls back to the compile-time default rather
        // than panicking the chat command.
        unsafe { std::env::set_var("LOOKBACK_CHAT_MAX_TOOL_HOPS", "not-a-number") };
        assert_eq!(max_tool_hops(), DEFAULT_MAX_TOOL_HOPS);
        unsafe { std::env::remove_var("LOOKBACK_CHAT_MAX_TOOL_HOPS") };
    }

    #[test]
    fn done_with_message_carries_max_hops_text() {
        // Budget exhaustion surfaces as Done (not Error) so any partial
        // assistant text the user has already seen remains usable.
        let update =
            ChatStepUpdate::done_with_message("job-1".into(), MAX_HOPS_REACHED_MESSAGE.to_string());
        assert_eq!(update.phase, ChatPhase::Done);
        assert_eq!(update.message.as_deref(), Some(MAX_HOPS_REACHED_MESSAGE));
    }

    // -- proto-json constructors ------------------------------------------

    #[test]
    fn assistant_tool_calls_proto_uses_tool_calls_oneof() {
        let calls = vec![make_pending_call("c1", "lookback_recall", "{\"q\":\"a\"}")];
        let v = assistant_tool_calls_proto(&calls);
        assert_eq!(v["role"], "ASSISTANT");
        let arr = v["content"]["tool_calls"]["calls"]
            .as_array()
            .expect("calls array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["call_id"], "c1");
        assert_eq!(arr[0]["fn_name"], "lookback_recall");
        assert_eq!(arr[0]["fn_arguments"], "{\"q\":\"a\"}");
        // Text oneof must NOT also be present — protobuf-json rejects
        // setting multiple oneof variants.
        assert!(v["content"].get("text").is_none());
    }

    #[test]
    fn tool_result_proto_uses_tool_results_oneof() {
        // Both the llama-cpp-plugin OAI converter and the genai adapter
        // consume the `tool_results` oneof.
        // We emit a single-element results array per hop (spec §5).
        let v = tool_result_proto("c1", "lookback_recall", "{\"sources\":[]}", false);
        assert_eq!(v["role"], "TOOL");
        let results = v["content"]["tool_results"]["results"]
            .as_array()
            .expect("results array");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["call_id"], "c1");
        assert_eq!(results[0]["fn_name"], "lookback_recall");
        assert_eq!(results[0]["content"], "{\"sources\":[]}");
        assert_eq!(results[0]["is_error"], false);
        // oneof exclusivity: the legacy `tool_execution_requests` variant
        // must not also be set, otherwise protobuf-json rejects it.
        assert!(v["content"].get("tool_execution_requests").is_none());
    }

    #[test]
    fn tool_result_proto_marks_failures_with_is_error() {
        let v = tool_result_proto("c1", "lookback_recall", "{\"error\":\"boom\"}", true);
        let results = v["content"]["tool_results"]["results"]
            .as_array()
            .expect("results array");
        assert_eq!(results[0]["is_error"], true);
        assert_eq!(results[0]["content"], "{\"error\":\"boom\"}");
    }

    #[test]
    fn build_tool_response_extracts_sources_only_for_lookback_recall() {
        let payload = serde_json::json!({
            "sources": [{
                "source_kind": "raw_memory",
                "memory_id": "1",
                "source_thread_id": "2",
                "snippet": "x",
                "score": 0.5
            }]
        });
        let (text, sources) = build_tool_response("lookback_recall", payload.clone());
        assert!(
            !text.is_empty(),
            "tool text must be the JSON-stringified result"
        );
        assert_eq!(sources.len(), 1);

        // Unknown tools (forward-compat) get no citations.
        let (other_text, other_sources) = build_tool_response("some_other_tool", payload);
        assert!(!other_text.is_empty());
        assert!(other_sources.is_empty());
    }

    #[test]
    fn pending_chunk_captured_on_separated_wire_shape() {
        // Both plugins (llama-cpp and genai) now use the separated wire
        // shape (specs/llama-cpp-plugin-streaming-tool-call-spec.md): a
        // `done=false` chunk carries `pending_tool_calls` +
        // `requires_tool_execution=true`, then a separate `done=true`
        // terminator follows. The intermediate chunk must produce a
        // final-shaped result so the agent loop can dispatch the tool;
        // the pre-fix code only marked a chunk final on `done=true` and
        // dropped the tool calls between chunks, leaving the UI stuck at
        // "generating…".
        let bytes = LlmChatResult {
            pending_tool_calls: Some(PendingToolCalls {
                calls: vec![ToolCallRequest {
                    call_id: "call-x".into(),
                    fn_name: "lookback_recall".into(),
                    fn_arguments: "{\"q\":\"hi\"}".into(),
                }],
            }),
            requires_tool_execution: Some(true),
            done: false,
            ..Default::default()
        }
        .encode_to_vec();
        let mut last: Option<String> = None;
        let mut think = ThinkState::default();
        let (updates, captured) =
            chunk_to_updates_for_hop("chat-1", ListenChunk::Data(bytes), &mut last, &mut think);
        assert!(
            updates.is_empty(),
            "no Token/Source/Searching from a pure pending chunk"
        );
        let decoded = captured.expect("intermediate pending chunk must produce a final chunk");
        match drive_hop(decoded) {
            HopOutcome::ContinueWithToolCalls(c) => assert_eq!(c[0].call_id, "call-x"),
            other => panic!("expected Continue, got {other:?}"),
        }
    }

    #[test]
    fn legacy_done_true_with_pending_still_captured() {
        // The wire-shape spec leaves the door open for a legacy
        // `done=true` chunk that ALSO carries `pending_tool_calls`
        // (see specs/llama-cpp-plugin-streaming-tool-call-spec.md
        // §5 Phase 1). The OR condition in `chunk_to_updates_for_hop`
        // must continue to capture that chunk for older / Phase-1
        // plugin builds.
        let bytes = LlmChatResult {
            pending_tool_calls: Some(PendingToolCalls {
                calls: vec![ToolCallRequest {
                    call_id: "call-legacy".into(),
                    fn_name: "lookback_recall".into(),
                    fn_arguments: "{}".into(),
                }],
            }),
            requires_tool_execution: Some(true),
            done: true,
            ..Default::default()
        }
        .encode_to_vec();
        let mut last: Option<String> = None;
        let mut think = ThinkState::default();
        let (_updates, captured) =
            chunk_to_updates_for_hop("chat-1", ListenChunk::Data(bytes), &mut last, &mut think);
        let decoded = captured.expect("done=true + pending must still produce a final chunk");
        match drive_hop(decoded) {
            HopOutcome::ContinueWithToolCalls(c) => assert_eq!(c[0].call_id, "call-legacy"),
            other => panic!("expected Continue, got {other:?}"),
        }
    }

    #[test]
    fn plain_text_chunk_without_done_does_not_synthesise_final_chunk() {
        // Counter-test for the genai pending-without-done branch: a
        // garden-variety streaming text token chunk must NOT be treated
        // as final, otherwise per-token rendering collapses into one big
        // "final" chunk and the agent loop terminates after the first
        // token.
        let bytes = LlmChatResult {
            content: Some(MessageContent {
                content: Some(Content::Text("hello".into())),
            }),
            done: false,
            ..Default::default()
        }
        .encode_to_vec();
        let mut last: Option<String> = None;
        let mut think = ThinkState::default();
        let (updates, captured) =
            chunk_to_updates_for_hop("chat-1", ListenChunk::Data(bytes), &mut last, &mut think);
        assert!(
            updates.iter().any(|u| u.phase == ChatPhase::Token),
            "the text delta MUST still emit as a Token"
        );
        assert!(
            captured.is_none(),
            "a streaming text delta must NOT short-circuit drive_hop"
        );
    }

    #[test]
    fn parse_lookback_sources_skips_malformed_rows() {
        // A row missing source_thread_id must drop just that row, not
        // poison the rest.
        let result = ExtractedToolResult {
            call_id: "c1".into(),
            fn_name: "lookback_recall".into(),
            result: r#"{"sources":[
                {"source_kind":"raw_memory","memory_id":1,"snippet":"x","score":0.5},
                {"source_kind":"raw_memory","memory_id":2,"source_thread_id":3,"snippet":"y","score":0.6}
            ]}"#.into(),
            error: None,
            success: true,
            job_id: Some(1),
        };
        let parsed = parse_lookback_sources(&result);
        assert_eq!(parsed.len(), 1);
        assert!(matches!(
            parsed[0],
            ChatSource::RawMemory {
                memory_id: 2,
                source_thread_id: 3,
                ..
            }
        ));
    }

    fn dummy_state_for_cancel() -> AppState {
        use crate::data::DataPaths;
        use crate::sidecar::{SidecarConfig, Sidecars};
        let data = DataPaths::with_root("/tmp/lookback-chat-cancel-test");
        let lance_home = data.lance_language_model_home();
        let sidecars = std::sync::Arc::new(Sidecars::new(SidecarConfig {
            jobworkerp_bin: std::path::PathBuf::from("/bin/true"),
            memories_bin: std::path::PathBuf::from("/bin/true"),
            conductor_bin: std::path::PathBuf::from("/bin/true"),
            data: data.clone(),
            worker_yaml_paths: Vec::new(),
            function_set_yaml_paths: Vec::new(),
            reflection_dispatch_enabled: false,
            auto_embedding_enabled: false,
            workflows_dir: None,
            lance_language_model_home: lance_home,
            lindera_dict_staged: false,
            llm_model: None,
            llm_hf_repo: None,
            llm_ctx_size: None,
            llm_kv_cache_type: None,
            env_file: None,
        }));
        AppState::new(sidecars, data)
    }

    #[tokio::test]
    async fn chat_cancel_inner_is_noop_for_unknown_job_id() {
        // Idempotent contract (OPEN-CHAT-2): a late Stop click against a
        // jobId the agent loop already cleaned up must resolve without
        // touching jobworkerp — a Some-jobworkerp call would need a live
        // sidecar and the user just wants the click to disappear.
        let state = dummy_state_for_cancel();
        let result = chat_cancel_inner(&state, "never-registered").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn chat_cancel_inner_flips_token_without_touching_jobworkerp() {
        // Between-hop window: the loop has cleared `current_job_id` but
        // the entry is still registered. Cancel must flip the token (so
        // the loop bails before the next hop opens) without attempting a
        // `JobService/Delete` — without a live sidecar a Some-JobId path
        // would error on `state.jobworkerp().await`. We confirm Ok(())
        // plus the token flip; the implicit "did not connect" guarantee
        // comes from the test running without a real jobworkerp endpoint.
        let state = dummy_state_for_cancel();
        let entry = state.chat_register("turn-cancel").await;
        assert!(!entry.token.is_cancelled());
        let result = chat_cancel_inner(&state, "turn-cancel").await;
        assert!(result.is_ok());
        assert!(entry.token.is_cancelled());
    }
}
