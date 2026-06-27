//! Live-sidecar check for the chat agent loop's client-side tool calling.
//! Asserts that `fetch_function_set_as_tools_json("lookback-rag")` produces
//! an OAI tool array containing `lookback_recall`, and that one hop with
//! that array dispatched against `memories-llm` surfaces a pending tool
//! call on the stream (best-effort: the model is generative).
//!
//! `#[ignore]` so CI doesn't pull the GGUF; local invocation:
//!
//! ```sh
//! LOOKBACK_JOBWORKERP_BIN=<all-in-one path> \
//! LOOKBACK_MEMORIES_BIN=<memories front path> \
//! LOOKBACK_PLUGINS_SRC=<repo>/plugins \
//!   cargo test -p lookback-tauri --test chat_agent_loop_e2e \
//!     -- --ignored --nocapture
//! ```

use std::time::Duration;

use lookback_tauri_lib::commands::chat::{CHAT_SYSTEM_PROMPT, rewrite_lookback_recall_tool_schema};
use lookback_tauri_lib::jobworkerp::JobworkerpHandle;
use lookback_tauri_lib::jobworkerp::llm_chat::decode_chunk;

mod common;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real sidecars and downloads a multi-GB GGUF on first run; opt in via --ignored"]
async fn agent_loop_dispatches_tool_call() {
    // The assertion only needs the plugin to surface a
    // pending_tool_calls chunk — it does not execute the tool — so the
    // shared `auto_embedding_enabled: false` startup is fine here.
    let (root, sidecars, report) = common::start_chat_e2e_sidecars("chat-agent-loop").await;
    eprintln!("sidecars ready: {:?}", report.endpoints);
    for w in &report.warnings {
        eprintln!("warning: {:?} {} ({:?})", w.kind, w.message, w.detail);
    }

    let handle = JobworkerpHandle::connect(&format!(
        "http://127.0.0.1:{}",
        report.endpoints.jobworkerp_port
    ))
    .await
    .expect("connect");

    // ---- (1) tools_json contract ---------------------------------------

    let tools_json = handle
        .fetch_function_set_as_tools_json("lookback-rag")
        .await
        .expect("fetch tools_json");
    eprintln!("tools_json: {tools_json}");
    let tools: serde_json::Value = serde_json::from_str(&tools_json).expect("tools_json is JSON");
    let arr = tools.as_array().expect("tools_json must be an array");
    assert!(
        !arr.is_empty(),
        "lookback-rag must expose at least one tool"
    );
    let recall = arr.iter().find(|t| {
        t.get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            == Some("lookback_recall")
    });
    let recall = recall.expect("tools_json should include lookback_recall");
    let params = recall
        .pointer("/function/parameters")
        .expect("parameters block present");
    assert!(
        params.get("type").and_then(|v| v.as_str()) == Some("object"),
        "lookback_recall parameters must be a JSON-Schema object, got {params}"
    );

    // ---- (2) one-hop dispatch surfaces a tool-call request -------------

    // Minimal client-side tool-calling request body. Matches the shape
    // commands::chat::build_chat_args produces in production (only the
    // values change). A short, direct user prompt nudges the model to
    // use the tool.
    let input = serde_json::json!({
        "messages": [
            {
                "role": "SYSTEM",
                "content": { "text": "You are Lookback's RAG assistant. Always call the `lookback_recall` tool to retrieve memories before answering. Do not rely on prior knowledge." }
            },
            {
                "role": "USER",
                "content": { "text": "あなたの過去の記憶を呼び出して、ユーザーが「cache」について話したことがあるか教えてください。" }
            }
        ],
        "options": {
            "max_tokens": 256,
            "temperature": 0.0,
        },
        "function_options": {
            "use_function_calling": false,
            "client_tools_json": tools_json,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
        }
    });

    let stream = tokio::time::timeout(
        Duration::from_secs(10 * 60),
        handle.dispatch_stream("memories-llm", input, Some("chat")),
    )
    .await
    .expect("dispatch timeout")
    .expect("dispatch ok");

    let mut saw_tool_call = false;
    let mut tool_call_fn: Option<String> = None;
    let mut saw_done = false;
    let drain = tokio::time::timeout(
        Duration::from_secs(20 * 60),
        stream.drain_bytes(|chunk| {
            use lookback_tauri_lib::jobworkerp::ListenChunk;
            let bytes = match chunk {
                ListenChunk::Data(b) => b,
                ListenChunk::Final { collected: Some(b) } => b,
                ListenChunk::Final { collected: None } => return,
            };
            let Some(decoded) = decode_chunk(&bytes) else {
                return;
            };
            if !decoded.pending_tool_calls.is_empty() {
                saw_tool_call = true;
                tool_call_fn = decoded.pending_tool_calls[0].fn_name.clone().into();
                eprintln!("pending tool call: {:?}", decoded.pending_tool_calls[0]);
            }
            if decoded.done {
                saw_done = true;
                eprintln!(
                    "done; requires_tool_execution={}",
                    decoded.requires_tool_execution
                );
            }
        }),
    )
    .await;
    assert!(drain.is_ok(), "stream drain timed out");
    drain.unwrap().expect("drain ok");

    assert!(saw_done, "stream must terminate with done=true");
    assert!(
        saw_tool_call,
        "agent loop integration: model must request at least one tool call \
         given the system prompt + client_tools_json; got none"
    );
    if let Some(name) = tool_call_fn {
        // Best-effort: surface a clear log if the model went off-rails.
        // Different models name-mangle differently, so this is not a
        // hard assertion.
        if name != "lookback_recall" {
            eprintln!("note: tool name was {name:?}, not lookback_recall");
        }
    }

    let _ = sidecars.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// Mirrors the UI's `chat_ask` payload byte-for-byte (long Japanese
/// system prompt, `max_tokens: 4000`, `temperature: 0.3`) so we can
/// reproduce the UI-only `enqueue_stream_worker_job` failure outside
/// the Tauri runtime. The other e2e uses a shorter prompt with smaller
/// `max_tokens` which is known to succeed — this test isolates whether
/// the production constants are what trips the plugin.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real sidecars and downloads a multi-GB GGUF on first run; opt in via --ignored"]
async fn agent_loop_with_production_payload() {
    let (root, sidecars, report) = common::start_chat_e2e_sidecars("chat-agent-loop-prod").await;
    eprintln!("sidecars ready: {:?}", report.endpoints);

    let handle = JobworkerpHandle::connect(&format!(
        "http://127.0.0.1:{}",
        report.endpoints.jobworkerp_port
    ))
    .await
    .expect("connect");

    let raw_tools_json = handle
        .fetch_function_set_as_tools_json("lookback-rag")
        .await
        .expect("fetch tools_json");
    let tools_json = rewrite_lookback_recall_tool_schema(&raw_tools_json);
    eprintln!("tools_json_len={}", tools_json.len());

    let input = serde_json::json!({
        "messages": [
            {
                "role": "SYSTEM",
                "content": { "text": CHAT_SYSTEM_PROMPT }
            },
            {
                "role": "USER",
                "content": { "text": "直近でキャッシュについて話したこと教えて" }
            }
        ],
        "options": {
            "max_tokens": 4000,
            "temperature": 0.3,
        },
        "function_options": {
            "use_function_calling": false,
            "client_tools_json": tools_json,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            // Qwen3 with thinking on plans tool calls inside <think> but
            // often forgets to emit them afterwards (QwenLM/Qwen3 #1817).
            // Mirror commands::chat::build_chat_args so the reproduction
            // tracks the production fix.
            "chat_template_kwargs": "{\"enable_thinking\":false}",
        }
    });

    let dispatch = tokio::time::timeout(
        Duration::from_secs(10 * 60),
        handle.dispatch_stream("memories-llm", input, Some("chat")),
    )
    .await;

    match dispatch {
        Err(_) => panic!("dispatch_stream timed out"),
        Ok(Err(e)) => panic!(
            "dispatch_stream failed (this is the UI-side blocker we're \
             reproducing): {e:#}"
        ),
        Ok(Ok(stream)) => {
            eprintln!("dispatch_stream opened, draining...");
            let mut saw_done = false;
            let mut idx = 0usize;
            let drain = tokio::time::timeout(
                Duration::from_secs(20 * 60),
                stream.drain_bytes(|chunk| {
                    use lookback_tauri_lib::jobworkerp::ListenChunk;
                    idx += 1;
                    let bytes = match chunk {
                        ListenChunk::Data(b) => b,
                        ListenChunk::Final { collected: Some(b) } => b,
                        ListenChunk::Final { collected: None } => return,
                    };
                    let Some(decoded) = decode_chunk(&bytes) else {
                        return;
                    };
                    let preview: String = decoded
                        .text
                        .as_deref()
                        .unwrap_or("")
                        .chars()
                        .take(80)
                        .collect();
                    eprintln!(
                        "chunk {idx}: text_len={} pending={} requires_tool={} done={} text={:?}",
                        decoded.text.as_ref().map(|s| s.len()).unwrap_or(0),
                        decoded.pending_tool_calls.len(),
                        decoded.requires_tool_execution,
                        decoded.done,
                        preview,
                    );
                    if decoded.done {
                        saw_done = true;
                    }
                }),
            )
            .await;
            assert!(drain.is_ok(), "stream drain timed out");
            drain.unwrap().expect("drain ok");
            assert!(saw_done, "stream must terminate with done=true");
        }
    }

    let _ = sidecars.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// Reproduce the exact user query that fails in the UI ("Lookback アプリ
/// の最近の改善点、機能追加の内容を教えて") so we can see chunk-level
/// behaviour outside the Tauri runtime. This is the *same* shape as the
/// production payload test but with the user-reported query.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real sidecars and downloads a multi-GB GGUF on first run; opt in via --ignored"]
async fn agent_loop_with_user_reported_query() {
    let (root, sidecars, report) = common::start_chat_e2e_sidecars("chat-agent-loop-userq").await;
    eprintln!("sidecars ready: {:?}", report.endpoints);

    let handle = JobworkerpHandle::connect(&format!(
        "http://127.0.0.1:{}",
        report.endpoints.jobworkerp_port
    ))
    .await
    .expect("connect");

    let raw_tools_json = handle
        .fetch_function_set_as_tools_json("lookback-rag")
        .await
        .expect("fetch tools_json");
    let tools_json = rewrite_lookback_recall_tool_schema(&raw_tools_json);

    let input = serde_json::json!({
        "messages": [
            {
                "role": "SYSTEM",
                "content": { "text": CHAT_SYSTEM_PROMPT }
            },
            {
                "role": "USER",
                "content": { "text": "Lookbackアプリの最近の改善点、機能追加の内容を教えて" }
            }
        ],
        "options": { "max_tokens": 4000, "temperature": 0.3 },
        "function_options": {
            "use_function_calling": false,
            "client_tools_json": tools_json,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "chat_template_kwargs": "{\"enable_thinking\":false}",
        }
    });

    let stream = tokio::time::timeout(
        Duration::from_secs(10 * 60),
        handle.dispatch_stream("memories-llm", input, Some("chat")),
    )
    .await
    .expect("dispatch timeout")
    .expect("dispatch ok");

    // Mirror commands::chat::run_chat_stream's drain logic: only the
    // `done=true` chunk seeds `final_chunk`, and the loop then asks
    // `drive_hop` (i.e. pending_tool_calls + requires_tool_execution)
    // to decide whether to continue. If `final_chunk.pending_tool_calls`
    // arrives empty here despite the raw chunk carrying pending=1, we've
    // found the UI mismatch.
    let mut saw_done = false;
    let mut saw_tool_call = false;
    let mut last_requires_tool = false;
    let mut final_pending_count: usize = 0;
    let mut visible_text_total = String::new();
    let mut idx = 0usize;
    let drain = tokio::time::timeout(
        Duration::from_secs(20 * 60),
        stream.drain_bytes(|chunk| {
            use lookback_tauri_lib::jobworkerp::ListenChunk;
            idx += 1;
            let bytes = match chunk {
                ListenChunk::Data(b) => b,
                ListenChunk::Final { collected: Some(b) } => b,
                ListenChunk::Final { collected: None } => return,
            };
            let Some(decoded) = decode_chunk(&bytes) else {
                eprintln!("chunk {idx}: undecodable, bytes={}", bytes.len());
                return;
            };
            let preview: String = decoded
                .text
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(120)
                .collect();
            eprintln!(
                "chunk {idx}: text_len={} pending={} requires_tool={} done={} text={:?}",
                decoded.text.as_ref().map(|s| s.len()).unwrap_or(0),
                decoded.pending_tool_calls.len(),
                decoded.requires_tool_execution,
                decoded.done,
                preview,
            );
            if let Some(t) = decoded.text.as_ref() {
                visible_text_total.push_str(t);
            }
            if !decoded.pending_tool_calls.is_empty() {
                saw_tool_call = true;
            }
            if decoded.done {
                saw_done = true;
                last_requires_tool = decoded.requires_tool_execution;
                final_pending_count = decoded.pending_tool_calls.len();
            }
        }),
    )
    .await;
    assert!(drain.is_ok(), "drain timed out");
    drain.unwrap().expect("drain ok");
    eprintln!("==========");
    eprintln!(
        "done={saw_done} tool_call_seen_anywhere={saw_tool_call} \
         final_pending={final_pending_count} \
         final_requires_tool={last_requires_tool}"
    );
    eprintln!(
        "visible_text_total ({} chars):\n{}",
        visible_text_total.len(),
        visible_text_total
    );
    eprintln!("==========");

    // We're diagnosing, not asserting — log everything and let the test
    // pass so a failing assertion doesn't truncate the output. The eyes
    // on the chunk stream are the value.
    assert!(saw_done, "must finalize with done=true");

    let _ = sidecars.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// Run the UI's actual drain path (`chunk_to_updates_for_hop` +
/// `ThinkState`) against a live stream and verify that the `captured`
/// `final_chunk` carries the same `pending_tool_calls` count as the raw
/// `decode_chunk` sees. If this asserts, we have proved the UI's
/// `run_chat_stream` *cannot* be losing `pending_tool_calls` between
/// drain and `drive_hop` — the visible UI bug is elsewhere (likely a
/// stale running binary). If it fires, we have an actual chunk-handling
/// bug to chase in `chunk_to_updates_for_hop`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real sidecars and downloads a multi-GB GGUF on first run; opt in via --ignored"]
async fn agent_loop_drain_path_preserves_pending_tool_calls() {
    use lookback_tauri_lib::commands::chat::{ThinkState, chunk_to_updates_for_hop};
    use lookback_tauri_lib::jobworkerp::ListenChunk;
    use lookback_tauri_lib::jobworkerp::llm_chat::ExtractedChunk;

    let (root, sidecars, report) =
        common::start_chat_e2e_sidecars("chat-agent-loop-drain-path").await;
    let handle = JobworkerpHandle::connect(&format!(
        "http://127.0.0.1:{}",
        report.endpoints.jobworkerp_port
    ))
    .await
    .expect("connect");

    let raw_tools_json = handle
        .fetch_function_set_as_tools_json("lookback-rag")
        .await
        .expect("fetch tools_json");
    let tools_json = rewrite_lookback_recall_tool_schema(&raw_tools_json);

    let input = serde_json::json!({
        "messages": [
            {
                "role": "SYSTEM",
                "content": { "text": CHAT_SYSTEM_PROMPT }
            },
            {
                "role": "USER",
                "content": { "text": "Lookbackアプリの最近の改善点、機能追加の内容を教えて" }
            }
        ],
        "options": { "max_tokens": 4000, "temperature": 0.3 },
        "function_options": {
            "use_function_calling": false,
            "client_tools_json": tools_json,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "chat_template_kwargs": "{\"enable_thinking\":false}",
        }
    });

    let stream = tokio::time::timeout(
        Duration::from_secs(10 * 60),
        handle.dispatch_stream("memories-llm", input, Some("chat")),
    )
    .await
    .expect("dispatch timeout")
    .expect("dispatch ok");

    let mut last_started_call: Option<String> = None;
    let mut think_state = ThinkState::default();
    let mut final_chunk: Option<ExtractedChunk> = None;
    let mut raw_final_pending: usize = 0;
    let mut idx = 0usize;
    let drain = tokio::time::timeout(
        Duration::from_secs(20 * 60),
        stream.drain_bytes(|chunk| {
            idx += 1;
            // Snapshot raw decoder result for comparison BEFORE
            // chunk_to_updates_for_hop consumes the chunk.
            let bytes_ref: Option<&[u8]> = match &chunk {
                ListenChunk::Data(b) => Some(b.as_slice()),
                ListenChunk::Final { collected: Some(b) } => Some(b.as_slice()),
                ListenChunk::Final { collected: None } => None,
            };
            let raw_pending = bytes_ref
                .and_then(decode_chunk)
                .map(|d| {
                    (
                        d.pending_tool_calls.len(),
                        d.requires_tool_execution,
                        d.done,
                    )
                })
                .unwrap_or((0, false, false));
            if raw_pending.2 {
                raw_final_pending = raw_pending.0;
                eprintln!(
                    "chunk {idx} (raw): pending={} requires_tool={} done=true",
                    raw_pending.0, raw_pending.1
                );
            }

            // Now run the UI's exact drain function. Production
            // `run_chat_stream` keeps the first `done=true` chunk and
            // ignores any subsequent ones (the plugin can emit a second
            // `LlmChatResult{done:true}` envelope right after the
            // canonical one — e.g. `FinalCollected` aggregate — with
            // `pending_tool_calls` cleared). Mirror that here so the
            // test asserts the production invariant.
            let (_updates, captured) = chunk_to_updates_for_hop(
                "test-job",
                chunk,
                &mut last_started_call,
                &mut think_state,
            );
            if let Some(c) = captured {
                eprintln!(
                    "chunk {idx} (captured): pending={} requires_tool={}",
                    c.pending_tool_calls.len(),
                    c.requires_tool_execution,
                );
                if final_chunk.is_none() {
                    final_chunk = Some(c);
                }
            }
        }),
    )
    .await;
    assert!(drain.is_ok(), "drain timed out");
    drain.unwrap().expect("drain ok");

    let captured = final_chunk.expect("final_chunk must be Some on done=true chunk");
    eprintln!(
        "==========\n\
         raw_final_pending={raw_final_pending} \
         captured_pending={} captured_requires_tool={}\n==========",
        captured.pending_tool_calls.len(),
        captured.requires_tool_execution,
    );
    assert_eq!(
        captured.pending_tool_calls.len(),
        raw_final_pending,
        "chunk_to_updates_for_hop must preserve pending_tool_calls count; \
         raw had {raw_final_pending} but captured carries {}",
        captured.pending_tool_calls.len(),
    );
    assert!(
        !captured.pending_tool_calls.is_empty() && captured.requires_tool_execution,
        "UI drain path must carry the tool call into final_chunk so \
         drive_hop continues the agent loop (got pending={} requires_tool={})",
        captured.pending_tool_calls.len(),
        captured.requires_tool_execution,
    );

    let _ = sidecars.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// Multi-hop loop: assert that after we send back a tool result the
/// model produces a final Text answer (not another tool call) — i.e.
/// that the agent loop's tool-result-feedback path is end-to-end wired.
///
/// `lookback_recall` itself depends on the embedding sidecar; this test
/// fakes the tool result instead so it can verify the loop wiring
/// without booting embedding. Embedding-level integration is covered by
/// `lookback_recall_e2e`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real sidecars and downloads a multi-GB GGUF on first run; opt in via --ignored"]
async fn agent_loop_completes_after_tool_result() {
    let (root, sidecars, report) =
        common::start_chat_e2e_sidecars("chat-agent-loop-multihop").await;
    eprintln!("sidecars ready: {:?}", report.endpoints);

    let handle = JobworkerpHandle::connect(&format!(
        "http://127.0.0.1:{}",
        report.endpoints.jobworkerp_port
    ))
    .await
    .expect("connect");

    let raw_tools_json = handle
        .fetch_function_set_as_tools_json("lookback-rag")
        .await
        .expect("fetch tools_json");
    let tools_json = rewrite_lookback_recall_tool_schema(&raw_tools_json);

    // ---- Hop 1: expect a tool_call -----------------------------------
    let hop1_messages = serde_json::json!([
        {
            "role": "SYSTEM",
            "content": { "text": CHAT_SYSTEM_PROMPT }
        },
        {
            "role": "USER",
            "content": { "text": "直近でキャッシュについて話したこと教えて" }
        }
    ]);
    let hop1_input = serde_json::json!({
        "messages": hop1_messages,
        "options": { "max_tokens": 4000, "temperature": 0.3 },
        "function_options": {
            "use_function_calling": false,
            "client_tools_json": tools_json,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "chat_template_kwargs": "{\"enable_thinking\":false}",
        }
    });

    let stream = tokio::time::timeout(
        Duration::from_secs(10 * 60),
        handle.dispatch_stream("memories-llm", hop1_input, Some("chat")),
    )
    .await
    .expect("hop1 dispatch timeout")
    .expect("hop1 dispatch ok");

    let mut pending: Vec<lookback_tauri_lib::jobworkerp::llm_chat::ExtractedToolCall> = Vec::new();
    let mut saw_done = false;
    let drain = tokio::time::timeout(
        Duration::from_secs(20 * 60),
        stream.drain_bytes(|chunk| {
            use lookback_tauri_lib::jobworkerp::ListenChunk;
            let bytes = match chunk {
                ListenChunk::Data(b) => b,
                ListenChunk::Final { collected: Some(b) } => b,
                ListenChunk::Final { collected: None } => return,
            };
            if let Some(decoded) = decode_chunk(&bytes) {
                if !decoded.pending_tool_calls.is_empty() {
                    pending = decoded.pending_tool_calls.clone();
                }
                if decoded.done {
                    saw_done = true;
                }
            }
        }),
    )
    .await;
    assert!(drain.is_ok(), "hop1 drain timed out");
    drain.unwrap().expect("hop1 drain ok");
    assert!(saw_done, "hop1 must terminate with done=true");
    assert!(
        !pending.is_empty(),
        "hop1: model must emit a tool call when enable_thinking=false"
    );
    let call = pending.into_iter().next().unwrap();
    eprintln!(
        "hop1 tool call: fn_name={} call_id={}",
        call.fn_name, call.call_id
    );

    // ---- Hop 2: send fake tool result, expect final Text -------------
    let fake_tool_result =
        "{\"raw_results\":[],\"thread_summary_results\":[],\"period_summary_results\":[]}";

    let hop2_messages = serde_json::json!([
        {
            "role": "SYSTEM",
            "content": { "text": CHAT_SYSTEM_PROMPT }
        },
        {
            "role": "USER",
            "content": { "text": "直近でキャッシュについて話したこと教えて" }
        },
        {
            "role": "ASSISTANT",
            "content": {
                "tool_calls": {
                    "calls": [{
                        "call_id": call.call_id,
                        "fn_name": call.fn_name,
                        "fn_arguments": call.fn_arguments,
                    }]
                }
            }
        },
        {
            "role": "TOOL",
            "content": {
                "tool_execution_requests": {
                    "requests": [{
                        "call_id": call.call_id,
                        "fn_name": call.fn_name,
                        "fn_arguments": fake_tool_result,
                    }]
                }
            }
        }
    ]);
    let hop2_input = serde_json::json!({
        "messages": hop2_messages,
        "options": { "max_tokens": 4000, "temperature": 0.3 },
        "function_options": {
            "use_function_calling": false,
            "client_tools_json": tools_json,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "chat_template_kwargs": "{\"enable_thinking\":false}",
        }
    });

    let stream2 = tokio::time::timeout(
        Duration::from_secs(10 * 60),
        handle.dispatch_stream("memories-llm", hop2_input, Some("chat")),
    )
    .await
    .expect("hop2 dispatch timeout")
    .expect("hop2 dispatch ok");

    let mut hop2_text_tokens: usize = 0;
    let mut hop2_pending = false;
    let mut hop2_requires_tool = false;
    let mut hop2_done = false;
    let drain2 = tokio::time::timeout(
        Duration::from_secs(20 * 60),
        stream2.drain_bytes(|chunk| {
            use lookback_tauri_lib::jobworkerp::ListenChunk;
            let bytes = match chunk {
                ListenChunk::Data(b) => b,
                ListenChunk::Final { collected: Some(b) } => b,
                ListenChunk::Final { collected: None } => return,
            };
            if let Some(decoded) = decode_chunk(&bytes) {
                if let Some(text) = decoded.text.as_ref()
                    && !text.is_empty()
                {
                    hop2_text_tokens += 1;
                }
                if !decoded.pending_tool_calls.is_empty() {
                    hop2_pending = true;
                }
                if decoded.done {
                    hop2_done = true;
                    hop2_requires_tool = decoded.requires_tool_execution;
                }
            }
        }),
    )
    .await;
    assert!(drain2.is_ok(), "hop2 drain timed out");
    drain2.unwrap().expect("hop2 drain ok");
    assert!(hop2_done, "hop2 must terminate with done=true");
    assert!(
        !hop2_requires_tool && !hop2_pending,
        "hop2 must finalize (no further tool call) — got pending={hop2_pending} \
         requires_tool_execution={hop2_requires_tool}"
    );
    assert!(
        hop2_text_tokens > 0,
        "hop2 must stream at least one visible text token as the final answer"
    );
    eprintln!("hop2 final text tokens emitted: {hop2_text_tokens}");

    let _ = sidecars.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// OPEN-CHAT-2 / DECIDE-CHAT-4: mid-flight `JobService/Delete` against a
/// chat hop's `JobId` must close the stream from the server side so the
/// drain returns shortly after the cancel rather than running to natural
/// completion. Re-uses the production payload so we exercise the same
/// `dispatch_stream` ↔ `x-job-id-bin` path the UI uses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real sidecars and downloads a multi-GB GGUF on first run; opt in via --ignored"]
async fn agent_loop_cancels_on_demand() {
    let (root, sidecars, report) = common::start_chat_e2e_sidecars("chat-agent-loop-cancel").await;
    eprintln!("sidecars ready: {:?}", report.endpoints);

    let handle = JobworkerpHandle::connect(&format!(
        "http://127.0.0.1:{}",
        report.endpoints.jobworkerp_port
    ))
    .await
    .expect("connect");

    let raw_tools_json = handle
        .fetch_function_set_as_tools_json("lookback-rag")
        .await
        .expect("fetch tools_json");
    let tools_json = rewrite_lookback_recall_tool_schema(&raw_tools_json);

    let input = serde_json::json!({
        "messages": [
            { "role": "SYSTEM", "content": { "text": CHAT_SYSTEM_PROMPT } },
            {
                "role": "USER",
                "content": {
                    "text": "あなたの過去の記憶を呼び出して、できるだけ長く、詳細に、過去のキャッシュ関連の議論を要約してください。"
                }
            }
        ],
        "options": { "max_tokens": 4000, "temperature": 0.3 },
        "function_options": {
            "use_function_calling": false,
            "client_tools_json": tools_json,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "chat_template_kwargs": "{\"enable_thinking\":false}",
        }
    });

    let stream = tokio::time::timeout(
        Duration::from_secs(5 * 60),
        handle.dispatch_stream("memories-llm", input, Some("chat")),
    )
    .await
    .expect("dispatch timeout")
    .expect("dispatch ok");

    let live_job_id = stream
        .job_id
        .expect("DispatchStream must surface the x-job-id-bin from the response trailer");
    eprintln!("live job_id: {live_job_id:?}");

    let cancel_handle = handle.clone();
    let cancel_jid = live_job_id;
    let cancel_task = tokio::spawn(async move {
        // Wait long enough for the model to start producing tokens, then
        // issue Delete — we want to verify the server-side abort path
        // rather than racing against an unstarted job.
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel_handle
            .cancel(cancel_jid)
            .await
            .expect("JobService/Delete must succeed against a live job");
        eprintln!("cancel issued");
    });

    let start = std::time::Instant::now();
    let drain = tokio::time::timeout(
        // Generous outer cap: if cancel never propagated we'd otherwise
        // run for max_tokens which can take minutes. The assertion below
        // is what actually proves "cancel was honored".
        Duration::from_secs(60),
        stream.drain_bytes(|_chunk| {
            // We don't care about content — only how soon the stream
            // closes after the Delete. Counting bytes here would be
            // brittle (chunk boundaries depend on the model).
        }),
    )
    .await;
    let elapsed = start.elapsed();
    cancel_task.await.expect("cancel task panicked");

    // Drain either returned Ok (server-side abort closed the stream
    // gracefully) or Err (transport-level abort surfaced as gRPC error).
    // Both are valid "cancel honored" outcomes; what we don't want is
    // running the job to natural completion, which on a 4000-max_tokens
    // generation would take far longer than this window.
    match &drain {
        Ok(Ok(())) => eprintln!("stream closed cleanly after cancel in {elapsed:?}"),
        Ok(Err(e)) => {
            eprintln!("stream aborted with transport error after cancel in {elapsed:?}: {e}")
        }
        Err(_) => {
            panic!("drain timed out: cancel did not propagate within 60s (elapsed {elapsed:?})")
        }
    }
    assert!(
        elapsed < Duration::from_secs(30),
        "cancel did not abort the stream quickly enough — elapsed {elapsed:?}"
    );

    let _ = sidecars.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}
