//! Live-sidecar e2e for the AC-9 "delete all data" flow.
//!
//! Spawns the real `jobworkerp` + `memories` sidecars against a temp data
//! root, then replays exactly what `commands::settings::purge_all_data`
//! does — `Sidecars::stop()` followed by `data::paths::purge(root)` — and
//! asserts the processes are gone and the root is removed. The command
//! itself takes `State<AppState>` so it can't be called directly; the two
//! steps it runs ARE its logic.
//!
//! Marked `#[ignore]` so CI doesn't try to run it. Local invocation:
//!
//! ```sh
//! LOOKBACK_JOBWORKERP_BIN=<all-in-one path> \
//! LOOKBACK_MEMORIES_BIN=<memories front path> \
//! LOOKBACK_PLUGINS_SRC=<repo>/plugins \
//!   cargo test -p lookback-tauri --test purge_e2e -- --ignored --nocapture
//! ```

use std::sync::Arc;

use lookback_tauri_lib::data::DataPaths;
use lookback_tauri_lib::sidecar::{SidecarConfig, Sidecars};

mod common;
use common::{require_env, tempdir};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real jobworkerp + memories sidecars; opt in via --ignored"]
async fn purge_stops_sidecars_and_removes_root() {
    let root = tempdir("purge");
    let data = DataPaths::with_root(&root);
    data.ensure().unwrap();

    let plugins_src = require_env("LOOKBACK_PLUGINS_SRC");
    lookback_tauri_lib::plugins::stage_plugins_from(&plugins_src, &data.plugins_dir())
        .expect("stage plugins");

    let lance_home = data.lance_language_model_home();
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
        // Purge test only verifies stop+rm; keep embedding off so it doesn't
        // require the Metal embedding plugin to be present.
        auto_embedding_enabled: false,
        workflows_dir: None,
        lance_language_model_home: lance_home,
        lindera_dict_staged: false,
        llm_model: None,
        llm_hf_repo: None,
        llm_ctx_size: None,
        llm_kv_cache_type: None,
        env_file: None,
    };
    let sidecars = Arc::new(Sidecars::new(config));

    let report = sidecars.start().await.expect("sidecars start");
    eprintln!("sidecars ready: {:?}", report.endpoints);

    // After start, the sidecars created on-disk state (sqlite under db/)
    // and endpoints are live.
    assert!(
        sidecars.current_endpoints().is_some(),
        "endpoints live after start"
    );
    assert!(
        sidecars.last_report().is_some(),
        "status snapshot present after start"
    );
    assert!(root.exists(), "data root exists after start");

    // Replay the command's two steps in order.
    sidecars.stop().await.expect("stop sidecars");
    lookback_tauri_lib::data::paths::purge(&root).expect("purge root");

    // The process list is taken on stop, so endpoints are gone, and a
    // second stop is a no-op (idempotent) — proves nothing dangles. The
    // status snapshot must clear too, so get_sidecar_status can't promote
    // a re-mounted frontend back to ready against the purged data.
    assert!(
        sidecars.current_endpoints().is_none(),
        "endpoints cleared after stop"
    );
    assert!(
        sidecars.last_report().is_none(),
        "status snapshot cleared after stop"
    );
    sidecars.stop().await.expect("second stop is idempotent");

    assert!(!root.exists(), "data root removed after purge");
}
