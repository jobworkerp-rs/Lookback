//! Live-sidecar e2e for the streaming dispatch path via `JobworkerpHandle`.
//!
//! Spawns the real `jobworkerp` + `memories` sidecars, registers the
//! batch workers from `agent-app/workers/llm-workers.yaml`, and exercises
//! `dispatch_stream` against `memories-summarize-batch`.
//!
//! Marked `#[ignore]` so CI doesn't try to run it. Local invocation:
//!
//! ```sh
//! LOOKBACK_JOBWORKERP_BIN=<all-in-one path> \
//! LOOKBACK_MEMORIES_BIN=<memories front path> \
//! LOOKBACK_PLUGINS_SRC=<repo>/plugins \
//!   cargo test -p lookback-tauri --test streaming_dispatch_e2e -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use lookback_tauri_lib::data::DataPaths;
use lookback_tauri_lib::jobworkerp::{JobworkerpHandle, ProgressEvent};
use lookback_tauri_lib::sidecar::{SidecarConfig, Sidecars};

mod common;
use common::{require_env, tempdir};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real jobworkerp + memories sidecars; opt in via --ignored"]
async fn streaming_dispatch_against_live_sidecars() {
    let root = tempdir("stream");
    let data = DataPaths::with_root(&root);
    data.ensure().unwrap();

    // Required: a built repo with plugins compiled. We rely on the dev
    // plugin staging path (LOOKBACK_PLUGINS_SRC env override -> data_dir
    // copy in the plugins::stage_plugins helper) so the jobworkerp
    // sidecar finds LLMPromptRunner / MultimodalEmbeddingRunner on disk.
    let plugins_src = require_env("LOOKBACK_PLUGINS_SRC");
    // Stage manually here (sidecar startup doesn't run stage_plugins —
    // that path runs only inside Tauri::setup).
    lookback_tauri_lib::plugins::stage_plugins_from(&plugins_src, &data.plugins_dir())
        .expect("stage plugins");

    // Stage the bundled Lindera dictionary so a lindera-feature `front`
    // build can build its FTS index; otherwise we fall back to ngram.
    let lance_home = data.lance_language_model_home();
    let lindera_staged = lookback_tauri_lib::lindera::stage_lindera_from(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../dict/lindera/ipadic"),
        &data.lindera_ipadic_dir(),
    )
    .is_ok();

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
        // Exercise the full embedding pipeline (Hybrid / intent search need
        // the `memories-mm-embedding` worker registered + vector store on).
        auto_embedding_enabled: true,
        workflows_dir: lookback_tauri_lib::data::paths::workflows_bundle_dir().ok(),
        lance_language_model_home: lance_home,
        lindera_dict_staged: lindera_staged,
        llm_model: None,
        llm_hf_repo: None,
        llm_ctx_size: None,
        llm_kv_cache_type: None,
        env_file: None,
    };
    let sidecars = Arc::new(Sidecars::new(config));

    let report = sidecars.start().await.expect("sidecars start");
    eprintln!("sidecars ready: {:?}", report.endpoints);
    if !report.warnings.is_empty() {
        eprintln!("warnings: {:?}", report.warnings);
    }

    // Sub-test 1: register_workers is idempotent — sidecar already ran
    // register once, calling it again should re-upsert the same names.
    let handle = JobworkerpHandle::connect(&format!(
        "http://127.0.0.1:{}",
        report.endpoints.jobworkerp_port
    ))
    .await
    .expect("connect");
    let registered = handle
        .register_workers_from_yaml(&lookback_tauri_lib::data::paths::llm_workers_yaml().unwrap())
        .await
        .expect("register");
    for expected in [
        "memories-llm",
        "memories-summarize-batch",
        "memories-personality-batch",
        "memories-reflection-batch",
    ] {
        assert!(
            registered.contains_key(expected),
            "expected worker {expected} not registered; got {:?}",
            registered.keys().collect::<Vec<_>>()
        );
    }

    // Sub-test 2: stream a no-op summary against a non-existent user so
    // the batch fans out to zero threads. The workflow must still emit
    // an End so the toast transitions to Done.
    let input = serde_json::json!({
        "user_id": 999_999,
        "memories_grpc_host": "127.0.0.1",
        "memories_grpc_port": report.endpoints.memories_port,
        "single_workflow_path": "/tmp/unused-thread-summary-single.yaml",
    });
    let stream = tokio::time::timeout(
        Duration::from_secs(60),
        handle.dispatch_stream("memories-summarize-batch", input, None),
    )
    .await
    .expect("dispatch timeout")
    .expect("dispatch ok");

    let mut got_end = false;
    let drain = tokio::time::timeout(
        Duration::from_secs(30 * 60),
        stream.drain(|ev| {
            if matches!(ev, ProgressEvent::End { .. }) {
                got_end = true;
            }
        }),
    )
    .await;
    assert!(drain.is_ok(), "stream drain timed out after 30 min");
    drain.unwrap().expect("drain ok");
    assert!(got_end, "stream must terminate with End");

    // Sub-test 3: unknown worker name surfaces as AppError::Jobworkerp.
    let err = handle
        .dispatch_stream("does-not-exist", serde_json::json!({}), None)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("worker not found") || err.contains("not found"),
        "expected not-found error, got: {err}"
    );

    // Sub-test 4: client-side query embedding via the embedding worker.
    // memories registered `memories-mm-embedding` because
    // auto_embedding_enabled = true. A non-empty query must return a
    // non-empty vector — the building block for Hybrid search.
    let vec = tokio::time::timeout(
        Duration::from_secs(120),
        lookback_tauri_lib::jobworkerp::embedding::embed_query(
            &handle,
            "how do I fix a flaky integration test",
        ),
    )
    .await
    .expect("embed timeout")
    .expect("embed ok");
    assert!(!vec.is_empty(), "embedding vector must be non-empty");
    eprintln!("embedding dim = {}", vec.len());

    let _ = sidecars.stop().await;
    let _ = std::fs::remove_dir_all(&root);
}
