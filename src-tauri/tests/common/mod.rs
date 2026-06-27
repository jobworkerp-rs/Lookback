//! Shared helpers for the `#[ignore]` live-sidecar e2e tests
//! (`streaming_dispatch_e2e.rs`, `purge_e2e.rs`,
//! `llm_chat_streaming_spike.rs`, `chat_agent_loop_e2e.rs`,
//! `lookback_recall_e2e.rs`). Each test target compiles its own copy
//! via `mod common;` — Cargo's integration-test convention for
//! non-public test code.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lookback_tauri_lib::data::DataPaths;
use lookback_tauri_lib::sidecar::{SidecarConfig, SidecarStartReport, Sidecars};

/// Resolve a required env var to a binary/path. Panics with a
/// human-readable message so an undeclared `LOOKBACK_*_BIN` fails the
/// test up front rather than partway through sidecar startup.
pub fn require_env(key: &str) -> PathBuf {
    PathBuf::from(
        std::env::var(key)
            .unwrap_or_else(|_| panic!("e2e test requires {key} to point at a real binary/path")),
    )
}

/// Create a per-test temp directory under the system temp root. `label`
/// distinguishes which test owns the directory when several run in
/// sequence; the PID + millisecond timestamp suffix guarantees
/// uniqueness across concurrent / repeated runs.
pub fn tempdir(label: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "lookback-e2e-{label}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis(),
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// The same `.env` body every live-sidecar test writes into its temp
/// data root so the spawned children's dotenvy has known cache knobs.
const DEFAULT_DOTENV: &str = "MEMORY_CACHE_NUM_COUNTERS=1296000\n\
     MEMORY_CACHE_MAX_COST=1296\n\
     MEMORY_CACHE_USE_METRICS=true\n\
     WORKER_DEFAULT_CONCURRENCY=4\n";

/// Stage plugins + lindera + .env into a fresh data root and bring up
/// the sidecars used by the chat / lookback_recall e2e suites:
/// bundled `llm-workers.yaml` + `function-sets.yaml`, reflection on,
/// embedding off (the tests either fake the `embedQuery` step or assert
/// on its `WorkerNotFound` failure, so booting embedding would only
/// inflate runtime). The caller owns the returned data root and is
/// responsible for tearing it down after `sidecars.stop().await`.
pub async fn start_chat_e2e_sidecars(label: &str) -> (PathBuf, Arc<Sidecars>, SidecarStartReport) {
    let root = tempdir(label);
    let data = DataPaths::with_root(&root);
    data.ensure().unwrap();

    let plugins_src = require_env("LOOKBACK_PLUGINS_SRC");
    lookback_tauri_lib::plugins::stage_plugins_from(&plugins_src, &data.plugins_dir())
        .expect("stage plugins");

    let lance_home = data.lance_language_model_home();
    let lindera_staged = lookback_tauri_lib::lindera::stage_lindera_from(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../dict/lindera/ipadic"),
        &data.lindera_ipadic_dir(),
    )
    .is_ok();

    let env_file = root.join(".env");
    std::fs::write(&env_file, DEFAULT_DOTENV).unwrap();

    let config = SidecarConfig {
        jobworkerp_bin: require_env("LOOKBACK_JOBWORKERP_BIN"),
        memories_bin: require_env("LOOKBACK_MEMORIES_BIN"),
        conductor_bin: require_env("LOOKBACK_CONDUCTOR_BIN"),
        data: data.clone(),
        worker_yaml_paths: vec![
            lookback_tauri_lib::data::paths::llm_workers_yaml()
                .expect("workers/llm-workers.yaml resolvable via CARGO_MANIFEST_DIR"),
        ],
        function_set_yaml_paths: vec![
            lookback_tauri_lib::data::paths::function_sets_yaml()
                .expect("workers/function-sets.yaml resolvable via CARGO_MANIFEST_DIR"),
        ],
        reflection_dispatch_enabled: true,
        auto_embedding_enabled: false,
        workflows_dir: lookback_tauri_lib::data::paths::workflows_bundle_dir().ok(),
        lance_language_model_home: lance_home,
        lindera_dict_staged: lindera_staged,
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
    (root, sidecars, report)
}
