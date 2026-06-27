//! Lookback Tauri core: sidecar lifecycle, gRPC clients to memories,
//! and the Tauri commands the React frontend invokes.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

pub mod commands;
pub mod crashtrace;
pub mod data;
pub mod error;
pub mod grpc;
pub mod jobworkerp;
pub mod lindera;
pub mod plugins;
pub mod serde_id;
pub mod sidecar;

use std::path::PathBuf;
use std::sync::Arc;

use tauri::{AppHandle, Manager, RunEvent};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

use crate::commands::AppState;
use crate::data::DataPaths;
use crate::error::AppError;
use crate::sidecar::{SidecarConfig, SidecarWarning, SidecarWarningKind, Sidecars};

pub fn run() {
    init_tracing();
    apply_linux_webkit_workarounds();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            commands::threads::list_threads,
            commands::threads::find_distinct_labels,
            commands::threads::find_co_occurring_labels,
            commands::threads::find_memories_by_thread_id,
            commands::threads::find_memory_position,
            commands::threads::find_memory_thread_position,
            commands::threads::count_threads,
            commands::threads::delete_thread,
            commands::summaries::list_summaries,
            commands::summaries::count_summaries,
            commands::summaries::list_summary_period_keys,
            commands::summaries::delete_summary,
            commands::summaries::resolve_summary_memory_ref,
            commands::import::start_import,
            commands::import::start_import_cancel,
            commands::settings::get_settings,
            commands::settings::get_sidecar_status,
            commands::settings::purge_all_data,
            commands::connection::get_connection_config,
            commands::connection::set_connection_config,
            commands::logs::read_sidecar_log,
            commands::reflections::list_reflections_by_thread,
            commands::reflections::search_reflections,
            commands::reflections::search_reflections_by_intent,
            commands::reflections::get_reflection_intent_index_stats,
            commands::reflections::redispatch_reflection_embeddings,
            commands::reflections::delete_reflection,
            commands::personality::get_personality,
            commands::personality::list_personality_signals,
            commands::personality::delete_personality_signal,
            commands::personality::delete_personality_profile,
            commands::personality::debug_personality_inventory,
            commands::periodic_tasks::list_periodic_tasks,
            commands::periodic_tasks::create_periodic_task,
            commands::periodic_tasks::update_periodic_task,
            commands::periodic_tasks::delete_periodic_task,
            commands::periodic_tasks::set_enabled_periodic_task,
            commands::periodic_execution::list_periodic_task_statuses,
            commands::periodic_execution::list_periodic_execution_history,
            commands::periodic_execution::cancel_periodic_execution,
            commands::search::search_memories_keyword,
            commands::search::search_memories_semantic,
            commands::search::search_memories_hybrid,
            commands::model::get_model_status,
            commands::model::retry_model_setup,
            commands::reflection_dispatch::enqueue_reflection_job,
            commands::reflection_dispatch::reflection_cancel,
            commands::analysis_dispatch::enqueue_summary_job,
            commands::analysis_dispatch::enqueue_personality_job,
            commands::analysis_dispatch::enqueue_personality_merge_job,
            commands::analysis_dispatch::enqueue_period_summary_job,
            commands::analysis_dispatch::generate_summaries,
            commands::analysis_dispatch::analysis_cancel,
            commands::chat::chat_ask,
            commands::chat::chat_cancel,
            commands::llm_settings::get_llm_settings,
            commands::llm_settings::set_llm_settings,
            commands::app_settings::get_app_settings,
            commands::app_settings::set_data_root,
            commands::app_settings::set_hf_home,
            commands::app_settings::set_output_language,
            commands::app_settings::validate_data_root,
            commands::app_settings::create_data_root,
            commands::llm_presets::list_llm_presets,
            commands::embedding_presets::list_embedding_presets,
            commands::embedding_settings::get_embedding_settings,
            commands::embedding_settings::set_embedding_settings,
            commands::mcp_settings::get_mcp_settings,
            commands::mcp_settings::set_mcp_settings,
            commands::apply_settings::apply_settings,
            commands::setup::get_setup_status,
            commands::setup::apply_setup,
            commands::setup::resume_setup,
            commands::setup::restart_for_setup,
            commands::embeddings::get_memory_embedding_stats,
            commands::embeddings::redispatch_memory_embeddings,
            commands::recovery::recover_evacuate_lancedb,
            commands::recovery::recover_purge_lancedb,
            commands::recovery::recover_reset_embedding_settings,
            commands::recovery::open_log_dir,
            commands::recovery::quit_app,
            resolve_memories_import_bin,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            // Point LOOKBACK_WORKERS_DIR at the bundled resource (prod) so
            // build_sidecar_config's llm_workers_yaml() resolves; no-op in
            // dev where the CARGO_MANIFEST_DIR fallback applies.
            crate::data::paths::stage_workers_env(&handle);
            let config = build_sidecar_config(&handle)?;
            let data = config.data.clone();
            let sidecars = Arc::new(Sidecars::new(config));

            app.manage(AppState::new(sidecars.clone(), data.clone()));

            // Spawn sidecars on a tokio task — keep the Tauri setup path
            // non-blocking so the UI shell renders immediately.
            let handle_for_task = handle.clone();
            tauri::async_runtime::spawn(async move {
                stage_and_start_sidecars(&handle_for_task, &sidecars, &data).await;
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let RunEvent::ExitRequested { .. } = event {
                tracing::info!("exit requested; stopping sidecars");
                if let Some(state) = app_handle.try_state::<AppState>() {
                    let sidecars = state.sidecars.clone();
                    tauri::async_runtime::block_on(async move {
                        let _ = sidecars.stop().await;
                    });
                }
            }
        });
}

/// Initialise tracing to stderr AND, when the data root is reachable, to
/// `<root>/log/lookback.log`. A bundled `.app` has no attached terminal, so
/// stderr-only logging meant the Rust-side logs (including the `memories-import`
/// child's stdout/stderr forwarded under `target: "memories-import"`) vanished
/// — which is exactly why a remote-import failure could not be diagnosed. The
/// file mirrors what sidecar logs already do (`<root>/log/<name>.std*.log`).
///
/// Falls back to stderr-only if the log file can't be opened; logging must
/// never block startup.
/// On Linux, WebKitGTK's DMABUF-based GPU renderer deadlocks the WebView on
/// several NVIDIA proprietary-driver setups (notably under Wayland or a
/// driver/CUDA mismatch): the window paints the first screen, then freezes the
/// whole UI the moment a later screen triggers a recomposite. The dev launcher
/// (`scripts/run-tauri.sh`) already exports these, but packaged builds
/// (AppImage/deb/rpm) have no such wrapper, so set them here BEFORE the WebView
/// initializes. Both honor an explicit user value so a Wayland user can opt
/// back in with `WEBKIT_DISABLE_DMABUF_RENDERER=0 GDK_BACKEND=wayland`.
#[cfg(target_os = "linux")]
fn apply_linux_webkit_workarounds() {
    fn set_default(key: &str, value: &str) {
        if std::env::var_os(key).is_none() {
            // Safe: single-threaded process start, before the WebView/GTK and
            // any other thread that might read the environment exists.
            unsafe { std::env::set_var(key, value) };
        }
    }
    set_default("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    set_default("GDK_BACKEND", "x11");
}

#[cfg(not(target_os = "linux"))]
fn apply_linux_webkit_workarounds() {}

fn init_tracing() {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr_layer = tracing_subscriber::fmt::layer().with_target(true);

    // Append (not truncate) so a crash-and-relaunch keeps prior context; the
    // file is line-based plain text like the sidecar logs (no rotation in the
    // MVP — same deferred item noted in commands/logs.rs).
    let file = DataPaths::resolve().ok().and_then(|data| {
        let dir = data.log_dir();
        std::fs::create_dir_all(&dir).ok()?;
        // Point the fsync-per-line crash breadcrumb at the same log dir, so the
        // External→Local hard-crash position survives the OS panic that the
        // buffered tracing appender below loses (see `crashtrace`).
        crate::crashtrace::init(dir.clone());
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(crate::commands::logs::APP_LOG_FILE))
            .ok()
    });

    let registry = tracing_subscriber::registry().with(stderr_layer.with_filter(filter()));
    let init = match file {
        Some(file) => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false) // a file is not a TTY; colour codes are noise
                    .with_target(true)
                    .with_writer(std::sync::Arc::new(file))
                    .with_filter(filter()),
            )
            .try_init(),
        None => registry.try_init(),
    };
    let _ = init;
}

/// Stage plugin dylibs then bring the sidecars up, emitting the
/// `sidecar://ready` / `sidecar://error` events the frontend listens for.
///
/// Plugin dylibs are staged *before* spawn so jobworkerp sees them when it
/// scans `PLUGINS_RUNNER_DIR` (it only scans at startup, so a dylib added
/// after boot needs a full restart — which is exactly what the retry command
/// does via `Sidecars::stop` + this function). Staging failures degrade to a
/// `PluginsStageFailed` warning rather than blocking the browse-only paths.
///
/// Shared by `setup` (initial boot) and `commands::model::retry_model_setup`
/// (user-triggered retry after a failure).
pub(crate) async fn stage_and_start_sidecars(
    app: &AppHandle,
    sidecars: &Sidecars,
    data: &DataPaths,
) {
    let plugin_warnings = match crate::plugins::stage_plugins(app, &data.plugins_dir()) {
        Ok(report) => {
            tracing::info!(
                source = %report.source.display(),
                copied = report.copied.len(),
                skipped = report.skipped_same.len(),
                "plugins staged"
            );
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(error = %e, "plugin staging failed");
            vec![SidecarWarning {
                kind: SidecarWarningKind::PluginsStageFailed,
                message: e.to_string(),
                detail: None,
            }]
        }
    };

    match sidecars.start_with_warnings(plugin_warnings).await {
        Ok(report) => {
            crate::commands::emit_event(app, "sidecar://ready", report);
        }
        Err(e) => {
            tracing::error!(error = ?e, "sidecar start failed");
            // Lift the `AppError` into the tagged payload the frontend
            // expects so a structured failure carries its `code` (and
            // recovery-actionable fields like `expected_dim`) through to
            // the BootError UI instead of collapsing to a string.
            crate::commands::emit_event(
                app,
                "sidecar://error",
                crate::sidecar::startup_error::SidecarErrorPayload::from_app_error(&e),
            );
        }
    }
}

fn build_sidecar_config(handle: &AppHandle) -> Result<SidecarConfig, Box<dyn std::error::Error>> {
    let data = DataPaths::resolve()?;
    // Ensure the data dirs exist before staging the Lindera dictionary into
    // them (sidecars also call ensure(), but staging runs first here).
    data.ensure()?;

    // Resolve sidecar binary paths. Priority:
    //   1. LOOKBACK_JOBWORKERP_BIN / LOOKBACK_MEMORIES_BIN env (dev override).
    //   2. Tauri externalBin next to the packaged executable.
    //   3. `which` lookup on PATH.
    //   4. Hard-coded relative paths inside the parent jobworkerp workspace
    //      (developer convenience while sidecar bundling is not yet wired).
    let jobworkerp_bin = resolve_bin(
        "LOOKBACK_JOBWORKERP_BIN",
        "all-in-one",
        "all-in-one",
        "../../target/release/all-in-one",
    )?;
    // memories ships its gRPC frontend as the `front` binary inside the
    // `grpc-admin` crate (`memories/grpc-admin/Cargo.toml` [[bin]] name = "front").
    // `which` looks up the more specific name `memories-front` to avoid PATH
    // collisions; the fallback resolves to the actual artifact name.
    let memories_bin = resolve_bin(
        "LOOKBACK_MEMORIES_BIN",
        "front",
        "memories-front",
        "../../memories/target/release/front",
    )?;
    let conductor_bin = resolve_bin(
        "LOOKBACK_CONDUCTOR_BIN",
        "conductor-main",
        "conductor-main",
        "../../conductor/target/release/conductor-main",
    )?;
    let protoc_bin = resolve_bin("PROTOC", "protoc", "protoc", "../../protobuf/bin/protoc")?;
    if protoc_bin.exists() {
        // SAFETY: Tauri setup performs this before the sidecar startup task is
        // spawned. In-process registration and children then share the path.
        unsafe { std::env::set_var("PROTOC", &protoc_bin) };
    }

    // Pre-register the `memories-llm` + batch named workers before
    // memories starts dispatching LLM-containing workflows. Resolution
    // errors are tolerated (the apply step handles missing files
    // non-fatally and surfaces a SidecarWarning).
    let worker_yaml_paths = data::paths::llm_workers_yaml().ok().into_iter().collect();

    // Apply the `lookback-rag` function set (used by the RAG chat to
    // narrow the LLM's callable tool surface to `lookback_recall`).
    // Kept in a separate file because the worker-YAML deserializer
    // rejects unknown keys (rag-chat-design.md DECIDE-CHAT-9).
    let function_set_yaml_paths = data::paths::function_sets_yaml().ok().into_iter().collect();

    // Resolve the bundled workflows dir so the memories embedding dispatchers
    // use agent-app's staged YAMLs instead of memories' compile-time defaults.
    // Tolerate failure: memories falls back to its own defaults, and any
    // backend mismatch surfaces in memories logs.
    let workflows_dir = data::paths::workflows_bundle_dir().ok();

    // Stage the bundled Lindera IPADIC dictionary so the lindera-feature
    // `front` build can do Japanese/Korean morphological FTS. Missing
    // source degrades to ngram (lindera_dict_staged = false).
    let lance_language_model_home = data.lance_language_model_home();
    let lindera_dict_staged = match crate::lindera::stage_lindera_dict(
        handle,
        &data.lindera_ipadic_dir(),
    ) {
        Ok(Some(report)) => {
            tracing::info!(
                source = %report.source.display(),
                copied = report.copied.len(),
                skipped = report.skipped_same.len(),
                config = %report.config_path.display(),
                "lindera dictionary staged"
            );
            true
        }
        Ok(None) => {
            tracing::warn!("lindera dictionary source not found; FTS falls back to ngram");
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, "lindera dictionary staging failed; FTS falls back to ngram");
            false
        }
    };

    // Local LLM resolution: persisted Settings (Local mode preset / custom)
    // is the authoritative source. Process env (`LOOKBACK_LLM_MODEL` etc.)
    // is a dev override that still wins when the user has not yet touched
    // Settings (i.e. `local_preset_id == None`) — once they DO save, the
    // settings file is authoritative so a stray shell env can't silently
    // re-route the next launch back to the old model. The same triple is
    // re-resolved in `Sidecars::start_inner` on a restart so a Settings
    // change takes effect without a full app relaunch (the cached
    // `SidecarConfig` is frozen at boot).
    let llm_settings = commands::llm_settings::load_llm_settings(&data.llm_settings_path());
    let (llm_model, llm_hf_repo, llm_ctx_size) =
        commands::llm_settings::resolve_local_llm_env_triple(&llm_settings, |name| {
            std::env::var(name).ok()
        });
    let llm_kv_cache_type =
        commands::llm_settings::resolve_kv_cache_type_with_env(&llm_settings, |name| {
            std::env::var(name).ok()
        })
        .runner_value()
        .to_string();

    Ok(SidecarConfig {
        jobworkerp_bin,
        memories_bin,
        conductor_bin,
        data,
        worker_yaml_paths,
        function_set_yaml_paths,
        reflection_dispatch_enabled: true,
        auto_embedding_enabled: true,
        workflows_dir,
        lance_language_model_home,
        lindera_dict_staged,
        llm_model,
        llm_hf_repo,
        llm_ctx_size,
        llm_kv_cache_type: Some(llm_kv_cache_type),
        env_file: resolve_env_file(),
    })
}

/// Locate a `.env` template to forward to the sidecars. Resolution:
///   1. `LOOKBACK_ENV_FILE` env override,
///   2. `<CARGO_MANIFEST_DIR>/../../.env` (the parent workspace template
///      that jobworkerp / memories were authored against),
///   3. None — the sidecars then run against pure defaults.
fn resolve_env_file() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LOOKBACK_ENV_FILE") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let candidate = PathBuf::from(manifest_dir).join("../../.env");
    candidate.exists().then_some(candidate)
}

/// Resolve the bundled `memories-import` binary, erroring when it can't be
/// found anywhere. Single owner of the env-override name + bundled/fallback
/// paths so the import command and the sidecar's lang-worker registration
/// (`sidecar::generation_workers`) can't drift apart.
pub(crate) fn resolve_memories_import_bin_path() -> Result<PathBuf, AppError> {
    let p = resolve_bin(
        "LOOKBACK_MEMORIES_IMPORT_BIN",
        "memories-import",
        "memories-import",
        "../../memories/target/release/memories-import",
    )
    .map_err(|e| AppError::Config(format!("memories-import resolve failed: {e}")))?;
    if !p.exists() {
        return Err(AppError::Config(format!(
            "memories-import binary not found at {} — set LOOKBACK_MEMORIES_IMPORT_BIN",
            p.display()
        )));
    }
    Ok(p)
}

/// Exposed to the frontend so the Import dialog can default its
/// `memories-import` path without baking it into the JS bundle. Returns
/// `Err` when the binary can't be found anywhere — the UI surfaces this so
/// the user knows to set `LOOKBACK_MEMORIES_IMPORT_BIN`.
#[tauri::command]
fn resolve_memories_import_bin() -> Result<PathBuf, AppError> {
    resolve_memories_import_bin_path()
}

/// Resolve a sidecar / CLI binary in this order:
///   1. `env_var` override (set in dev to point at a local cargo build),
///   2. `bundled_name` next to the running executable (Tauri `externalBin`
///      drops sidecars into `.app/Contents/MacOS/` alongside the app binary,
///      with the platform-triple suffix stripped at bundle time),
///   3. `on_path` via `which::which`,
///   4. relative fallback under `CARGO_MANIFEST_DIR`.
///
/// `CARGO_MANIFEST_DIR` resolves to `agent-app/src-tauri/`, so workspace
/// siblings live at `../../<name>/...` (NOT `../<name>/...`, which would point
/// at a non-existent `agent-app/<name>/`).
///
/// The fallback path is returned even if it doesn't exist on disk: the caller
/// decides whether to validate (sidecar startup surfaces this through its own
/// error path; the `resolve_memories_import_bin` command validates eagerly).
pub(crate) fn resolve_bin(
    env_var: &str,
    bundled_name: &str,
    on_path: &str,
    fallback_rel: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from));
    resolve_bin_from_dir(
        env_var,
        bundled_name,
        on_path,
        fallback_rel,
        exe_dir.as_deref(),
    )
}

fn resolve_bin_from_dir(
    env_var: &str,
    bundled_name: &str,
    on_path: &str,
    fallback_rel: &str,
    exe_dir: Option<&std::path::Path>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Ok(p) = std::env::var(env_var) {
        return Ok(PathBuf::from(p));
    }
    // Bundled sidecar: Tauri places `externalBin` entries next to the app
    // executable using the externalBin basename. This can differ from the
    // intentionally collision-resistant name used for PATH lookup.
    if let Some(dir) = exe_dir {
        let bundled = dir.join(bundled_name);
        if bundled.exists() {
            return Ok(bundled);
        }
    }
    if let Ok(p) = which::which(on_path) {
        return Ok(p);
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    Ok(PathBuf::from(manifest_dir).join(fallback_rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_bin_prefers_env_override() {
        // SAFETY: single-threaded test; unique env var name.
        unsafe { std::env::set_var("LOOKBACK_TEST_BIN_X", "/custom/path/to/bin") };
        let p = resolve_bin(
            "LOOKBACK_TEST_BIN_X",
            "definitely-not-bundled",
            "definitely-not-on-path",
            "fallback",
        )
        .unwrap();
        assert_eq!(p, PathBuf::from("/custom/path/to/bin"));
        unsafe { std::env::remove_var("LOOKBACK_TEST_BIN_X") };
    }

    #[test]
    fn resolve_bin_falls_back_to_manifest_relative_when_nothing_else() {
        // A name that isn't on PATH and isn't next to the test binary must
        // resolve to the CARGO_MANIFEST_DIR-relative fallback.
        unsafe { std::env::remove_var("LOOKBACK_TEST_BIN_Y") };
        let p = resolve_bin(
            "LOOKBACK_TEST_BIN_Y",
            "lookback-nonexistent-bundled-binary-zzz",
            "lookback-nonexistent-binary-zzz",
            "../../target/release/some-bin",
        )
        .unwrap();
        assert!(
            p.ends_with("../../target/release/some-bin"),
            "got {}",
            p.display()
        );
    }

    #[test]
    fn resolve_bin_uses_a_distinct_bundled_name() {
        let dir = tempfile::tempdir().unwrap();
        let bundled = dir.path().join("front");
        std::fs::write(&bundled, b"bundled sidecar").unwrap();

        unsafe { std::env::remove_var("LOOKBACK_TEST_BIN_BUNDLED_NAME") };
        let p = resolve_bin_from_dir(
            "LOOKBACK_TEST_BIN_BUNDLED_NAME",
            "front",
            "lookback-nonexistent-memories-front-zzz",
            "fallback",
            Some(dir.path()),
        )
        .unwrap();

        assert_eq!(p, bundled);
    }

    #[test]
    fn bundle_resources_use_stable_runtime_paths() {
        // Platform-agnostic resources + externalBin live in tauri.conf.json.
        // The plugin shared-library globs are platform-specific (a *.so glob
        // errors on macOS and a *.dylib glob errors on Linux, since tauri-build
        // rejects a glob that matches nothing), so they live in the
        // tauri.<platform>.conf.json files that tauri-build auto-merges.
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let resources = config["bundle"]["resources"]
            .as_object()
            .expect("bundle.resources must map source directories to stable runtime paths");

        assert_eq!(resources.get("../workers/"), Some(&"workers/".into()));
        assert_eq!(resources.get("../dict/"), Some(&"dict/".into()));
        // The plugin glob must NOT be in the shared config (would break the
        // other platform's build).
        assert!(resources.get("plugins/*.dylib").is_none());
        assert!(resources.get("plugins/*.so").is_none());

        let external_bins = config["bundle"]["externalBin"].as_array().unwrap();
        assert!(external_bins.iter().any(|entry| entry == "bin/protoc"));

        // Each platform overlay carries its own plugin glob.
        let macos: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.macos.conf.json")).unwrap();
        assert_eq!(
            macos["bundle"]["resources"].get("plugins/*.dylib"),
            Some(&"plugins/".into())
        );
        let linux: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.linux.conf.json")).unwrap();
        assert_eq!(
            linux["bundle"]["resources"].get("plugins/*.so"),
            Some(&"plugins/".into())
        );
        assert_eq!(
            linux["bundle"]["resources"].get("plugins/*.so.*"),
            Some(&"plugins/".into())
        );
    }
}
