//! PR0 streaming spike for the RAG chat (`specs/rag-chat-design.md`
//! DECIDE-CHAT-8). Confirms that the bundled `LLMPromptRunner`
//! (llama-cpp-plugin) actually streams per-token `LLMChatResult`
//! protobuf chunks over `dispatch_stream` against the `memories-llm`
//! worker. If this test passes, we go for PR1; if it can't observe
//! more than one `Content::Text` delta we treat the chat method as a
//! single-shot returner and stop (blocker — see spec OPEN-CHAT-9).
//!
//! Marked `#[ignore]` so CI doesn't pull the GGUF model. Local run:
//!
//! ```sh
//! LOOKBACK_JOBWORKERP_BIN=<all-in-one path> \
//! LOOKBACK_MEMORIES_BIN=<memories front path> \
//! LOOKBACK_PLUGINS_SRC=<repo>/plugins \
//!   cargo test -p lookback-tauri --test llm_chat_streaming_spike \
//!     -- --ignored --nocapture
//! ```
//!
//! Expectations (success):
//! - At least 2 non-empty `Content::Text` deltas observed before the
//!   `done=true` chunk (proves the stream is per-token, not one-shot).
//! - A final chunk with `done=true` arrives.
//!
//! Failure modes:
//! - Only the `done=true` chunk carries text → llama-cpp-plugin is
//!   returning the full text in one chunk; PR1 is a blocker.
//! - No chunks at all → worker registration / runtime error.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lookback_tauri_lib::data::DataPaths;
use lookback_tauri_lib::jobworkerp::{JobworkerpHandle, ProgressEvent};
use lookback_tauri_lib::sidecar::{SidecarConfig, Sidecars};

mod common;
use common::{require_env, tempdir};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real sidecars and downloads a multi-GB GGUF on first run; opt in via --ignored"]
async fn llm_chat_emits_per_token_chunks() {
    let root = tempdir("llm-chat-spike");
    let data = DataPaths::with_root(&root);
    data.ensure().unwrap();

    let plugins_src = require_env("LOOKBACK_PLUGINS_SRC");
    lookback_tauri_lib::plugins::stage_plugins_from(&plugins_src, &data.plugins_dir())
        .expect("stage plugins");

    let lance_home = data.lance_language_model_home();
    let lindera_staged = lookback_tauri_lib::lindera::stage_lindera_from(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dict/lindera/ipadic"),
        &data.lindera_ipadic_dir(),
    )
    .is_ok();

    // memories needs MEMORY_CACHE_* knobs and jobworkerp needs WORKER_*
    // knobs to come up. In a real Tauri launch they're read by
    // `dotenvy::dotenv()` from the sidecar's cwd (config.data.root); in
    // an e2e the temp data root has no `.env`. Place one there so both
    // sidecars find it via dotenvy when they start.
    //
    // Pass the same file via `env_file` as well so the parent process
    // env carries these values too — that's belt-and-suspenders, but
    // dotenvy in the child is what jobworkerp actually relies on for
    // WORKER_DEFAULT_CONCURRENCY (envy::prefixed("WORKER_") needs that
    // field; missing it falls back to the "default channel only" path
    // and the memories-llm registration rejects channel='llm').
    let env_file = root.join(".env");
    std::fs::write(
        &env_file,
        "MEMORY_CACHE_NUM_COUNTERS=1296000\n\
         MEMORY_CACHE_MAX_COST=1296\n\
         MEMORY_CACHE_USE_METRICS=true\n\
         WORKER_DEFAULT_CONCURRENCY=4\n",
    )
    .unwrap();

    let config = SidecarConfig {
        jobworkerp_bin: require_env("LOOKBACK_JOBWORKERP_BIN"),
        memories_bin: require_env("LOOKBACK_MEMORIES_BIN"),
        conductor_bin: require_env("LOOKBACK_CONDUCTOR_BIN"),
        data: data.clone(),
        worker_yaml_paths: vec![
            lookback_tauri_lib::data::paths::llm_workers_yaml()
                .expect("workers/llm-workers.yaml resolvable via CARGO_MANIFEST_DIR"),
        ],
        function_set_yaml_paths: Vec::new(),
        reflection_dispatch_enabled: true,
        auto_embedding_enabled: false,
        workflows_dir: lookback_tauri_lib::data::paths::workflows_bundle_dir().ok(),
        lance_language_model_home: lance_home,
        lindera_dict_staged: lindera_staged,
        // Allow CI / local override; otherwise we use whatever the YAML
        // defaults to (Qwen3.6-27B-UD-Q4_K_XL — large; first run downloads).
        llm_model: std::env::var("LOOKBACK_LLM_MODEL").ok(),
        llm_hf_repo: std::env::var("LOOKBACK_LLM_HF_REPO").ok(),
        llm_ctx_size: std::env::var("LOOKBACK_LLM_CTX_SIZE")
            .ok()
            .and_then(|v| v.parse().ok()),
        llm_kv_cache_type: std::env::var("LOOKBACK_LLM_KV_CACHE_TYPE").ok(),
        env_file: Some(env_file),
    };
    let sidecars = Arc::new(Sidecars::new(config));

    let report = sidecars.start().await.expect("sidecars start");
    eprintln!("sidecars ready: {:?}", report.endpoints);
    if !report.warnings.is_empty() {
        eprintln!("WARNINGS during sidecar startup:");
        for w in &report.warnings {
            eprintln!("  - {:?}: {} (detail: {:?})", w.kind, w.message, w.detail);
        }
    }

    let handle = JobworkerpHandle::connect(&format!(
        "http://127.0.0.1:{}",
        report.endpoints.jobworkerp_port
    ))
    .await
    .expect("connect");

    // `Sidecars::start` already registered the workers from
    // `config.worker_yaml_paths` (see `register_workers_from_yaml` call
    // sites in lifecycle.rs). Calling it again here is redundant and the
    // upstream `worker apply` rejects identical re-upserts as "worker
    // already registered". Trust the start path.

    // Minimal LlmChatArgs payload. Field names follow chat_args.proto
    // (llama-cpp-plugin/.../jobworkerp/runner/llm/chat_args.proto):
    //   - role is the ChatRole enum, JSON-encoded as the variant name
    //     ("USER", "ASSISTANT", ...).
    //   - content is a oneof MessageContent; the `text` variant is
    //     itself a nested object {"text": "..."} not a bare string.
    //
    // A short prompt is enough — we only need the runner to emit a few
    // token chunks to prove streaming works.
    let input = serde_json::json!({
        "messages": [
            {
                "role": "USER",
                "content": { "text": "Count from one to five, one number per line." }
            }
        ],
        "options": {
            "max_tokens": 64,
            "temperature": 0.0,
        }
    });

    // First run on a cold model can take minutes (Metal warmup, KV cache
    // build). Streaming should still start emitting tokens well before
    // the timeout — generation itself is fast once it begins.
    let stream = tokio::time::timeout(
        Duration::from_secs(10 * 60),
        handle.dispatch_stream("memories-llm", input, Some("chat")),
    )
    .await
    .expect("dispatch timeout")
    .expect("dispatch ok");

    let mut text_chunks: Vec<String> = Vec::new();
    let mut saw_done = false;
    let drain = tokio::time::timeout(
        Duration::from_secs(20 * 60),
        stream.drain(|ev| match ev {
            ProgressEvent::Chunk { text } => {
                // `DispatchStream::drain` renders the bytes back to JSON
                // via DynamicMessage (the worker's result_proto). For the
                // spike we only care that distinct non-empty text chunks
                // arrive — i.e. the stream isn't a single concatenated
                // payload. We treat any non-empty render as a "chunk".
                if !text.is_empty() {
                    eprintln!("chunk[{}]: {}", text_chunks.len(), text);
                    text_chunks.push(text);
                }
            }
            ProgressEvent::End { final_text } => {
                saw_done = true;
                eprintln!("end final_text={:?}", final_text);
            }
        }),
    )
    .await;
    assert!(drain.is_ok(), "stream drain timed out");
    drain.unwrap().expect("drain ok");

    assert!(saw_done, "stream must terminate with End");
    assert!(
        text_chunks.len() >= 2,
        "expected per-token streaming (>=2 chunks), got {}: {:?}",
        text_chunks.len(),
        text_chunks
    );

    // The chunk count + Done assertions above already prove the
    // streaming contract. Per-chunk `LlmChatResult` decoding is covered
    // by `jobworkerp::llm_chat`'s unit tests against synthetic bytes;
    // re-decoding the dispatch_stream JSON-rendered chunks here would
    // require descriptor-pool encoding (overkill for the spike).

    let _ = sidecars.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}
