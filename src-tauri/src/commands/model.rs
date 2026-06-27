//! FR-CONFIG-5 / design IMPL-9: LLM model preparation status.
//!
//! The LLMPromptRunner plugin downloads the configured model from Hugging
//! Face on first use and caches it. We pin `HF_HOME` to the ARCH-4 data
//! root (`<data>/models`) at sidecar spawn, so a `*.gguf` appearing under
//! that directory is the observable signal that the model is cached.
//!
//! Per the spec, only THREE states are surfaced — preparing / ready /
//! failed (with retry). Byte-level progress and cancellation are out of
//! scope (the plugin API does not expose them).
//!
//! The `failed` state is driven by the sidecar start report: when worker
//! registration or plugin staging failed (e.g. the LLMPromptRunner dylib
//! is missing from `PLUGINS_RUNNER_DIR`), the model can never finish
//! downloading/loading, so we surface that as `failed` with the retry
//! affordance instead of leaving the UI stuck on `preparing` forever.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, State};

use crate::error::{AppError, AppResult};

use super::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelState {
    /// Sidecars are up but no model cache file is present yet (initial
    /// download in progress), or the sidecars are not ready yet.
    Preparing,
    /// A cached model file exists and the sidecars are serving.
    Ready,
    /// A startup/download error was reported.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelStatus {
    pub state: ModelState,
    pub error: Option<String>,
    /// The configured model identity, resolved at runtime from the worker YAML
    /// (+ env override for the LLM). Surfaced so the UI never hardcodes a model
    /// name that drifts when the bundled model is swapped. `None` only if the
    /// YAML couldn't be read.
    pub name: Option<String>,
    pub repo: Option<String>,
}

/// Combined readiness of both local models the app depends on. The LLM
/// (`LLMPromptRunner`, a `*.gguf`) powers summary/personality/reflection
/// generation; the embedding model (`MultimodalEmbeddingRunner`, a HF
/// `*.safetensors` snapshot) powers Semantic/Hybrid/intent search. Both are
/// fetched lazily on first use, so the UI surfaces each independently — an LLM
/// that's ready while embeddings are still missing explains why generation
/// works but search doesn't.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelStatusReport {
    pub llm: ModelStatus,
    pub embedding: ModelStatus,
}

/// The configured model identity (name + HF repo) attached to a status. Kept
/// separate from the readiness signals so `classify_model_status` stays a pure
/// function of observable state, with identity layered in by the caller.
#[derive(Debug, Clone, Default)]
pub struct ModelIdentity {
    pub name: Option<String>,
    pub repo: Option<String>,
}

/// Decide the model state from observable signals. Pure so the readiness
/// contract is unit-tested without touching disk or sidecars.
///
/// Precedence: an explicit error wins; otherwise a cached weight file plus
/// serving sidecars means ready; anything else is still preparing.
pub fn classify_model_status(
    weight_files: &[PathBuf],
    sidecars_ready: bool,
    last_error: Option<&str>,
    identity: ModelIdentity,
) -> ModelStatus {
    if let Some(err) = last_error {
        return ModelStatus {
            state: ModelState::Failed,
            error: Some(err.to_string()),
            name: identity.name,
            repo: identity.repo,
        };
    }
    let has_model = !weight_files.is_empty();
    let state = if has_model && sidecars_ready {
        ModelState::Ready
    } else {
        ModelState::Preparing
    };
    ModelStatus {
        state,
        error: None,
        name: identity.name,
        repo: identity.repo,
    }
}

/// Resolve the LLM model identity from `llm-settings.json` (the user's
/// preset / custom choice) or — when external mode is active — the genai
/// provider model. The Settings card uses this for its read-only "current
/// model" row, so the resolved identity MUST agree with what
/// `lib.rs::build_sidecar_config` actually injects into the jobworkerp
/// child (`LOOKBACK_LLM_MODEL` / `LOOKBACK_LLM_HF_REPO`); otherwise the
/// card drifts and reports a different model than what is loaded.
/// Failure cases (no settings, unknown preset) fall through to the default
/// preset, matching `resolve_local_runtime`.
fn llm_identity(data: &crate::data::DataPaths) -> ModelIdentity {
    let settings = super::llm_settings::load_llm_settings(&data.llm_settings_path());
    if settings.mode == super::llm_settings::LlmMode::External {
        let name = settings.provider_model.clone();
        let repo = name
            .as_deref()
            .map(|m| super::llm_settings::provider_display_name(m).to_string());
        return ModelIdentity { name, repo };
    }
    let rt = super::llm_settings::resolve_local_runtime(&settings);
    ModelIdentity {
        name: (!rt.model_file.is_empty()).then_some(rt.model_file),
        repo: (!rt.hf_repo.is_empty()).then_some(rt.hf_repo),
    }
}

/// Resolve the embedding model identity from `embedding-settings.json` (the
/// user's preset / custom choice). The settings file is authoritative — the
/// committed `auto-embedding-workers.yaml` is fallback only and would always
/// resolve to the original `Qwen/Qwen3-VL-Embedding-2B` regardless of what
/// the user picked, making the Settings card mis-report "ready" against a
/// preset whose weights aren't downloaded yet. Mirrors `llm_identity`'s
/// settings-first design.
///
/// Uses the env-aware projection (`resolve_embedding_runtime_with_env`)
/// so a dev `LOOKBACK_EMBEDDING_MODEL_ID` override — which the sidecar
/// lifecycle DOES respect when no preset is saved — also drives the
/// readiness scan. Without this, the override loads a different model
/// than the readiness card filters for, and the card stays stuck on
/// `preparing` even though the right weights are present.
fn embedding_identity(data: &crate::data::DataPaths) -> ModelIdentity {
    let settings =
        super::embedding_settings::load_embedding_settings(&data.embedding_settings_path());
    let rt = super::embedding_settings::resolve_embedding_runtime_with_env(
        &settings,
        super::process_env_lookup,
    );
    let model_id = (!rt.model_id.is_empty()).then_some(rt.model_id);
    ModelIdentity {
        name: model_id.clone(),
        repo: model_id,
    }
}

/// Collect files with extension `ext` under `dir`, walking the HF Hub snapshot
/// layout (`models--org--name/snapshots/<rev>/<file>`) up to `MAX_DEPTH`.
/// Missing directory yields an empty list (the cache isn't created yet).
/// Used for both `gguf` (LLM weights) and `safetensors` (embedding weights);
/// the resolved files live in `snapshots/<rev>/` as symlinks into `blobs/`.
fn list_model_files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_files(dir, ext, &mut out, 0);
    out
}

/// Translate a HuggingFace `org/name` repo id into the HF Hub cache
/// directory segment (`models--<org>--<name>`). Returns `None` for a
/// repo string that doesn't look like `org/name` so callers fall back
/// to the unfiltered scan rather than silently miscoupling. Pure for
/// unit-testability.
fn hf_repo_cache_segment(repo: &str) -> Option<String> {
    let (org, name) = repo.split_once('/')?;
    if org.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(format!("models--{org}--{name}"))
}

/// Keep only those `files` that live under `<…>/models--<org>--<name>/…`
/// for the given `repo`. A `repo` of `None` (or one we can't translate
/// into the HF cache segment) returns the input unchanged so we never
/// regress to "no readiness signal" for unusual identifiers.
///
/// Defense against the regression in commands/model.rs:245 — without
/// this filter, leftover weights from a previously-selected preset
/// (e.g. Qwen3-VL-Embedding-2B `.safetensors` still on disk after the
/// user switches to Qwen3-Embedding-0.6B) would keep the Settings card
/// reporting `ready` against the new, undownloaded repo.
///
/// Takes `Vec<PathBuf>` by value so the retained entries move (no
/// PathBuf clones) and the segment comparison uses `OsStr` directly to
/// skip `to_string_lossy`'s per-component Cow allocation.
fn filter_files_for_repo(files: Vec<PathBuf>, repo: Option<&str>) -> Vec<PathBuf> {
    let Some(segment) = repo.and_then(hf_repo_cache_segment) else {
        return files;
    };
    let segment = std::ffi::OsString::from(segment);
    files
        .into_iter()
        .filter(|p| p.components().any(|c| c.as_os_str() == segment.as_os_str()))
        .collect()
}

fn collect_files(dir: &Path, ext: &str, out: &mut Vec<PathBuf>, depth: usize) {
    // 5 covers the multi-part GGUF layout where shards live one level
    // deeper than the usual `snapshots/<rev>/<file>` shape (e.g.
    // `…/snapshots/<rev>/UD-IQ4_XS/Qwen3.5-122B-…-00001-of-00003.gguf`).
    const MAX_DEPTH: usize = 5;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // CRITICAL: use `entry.file_type()` (lstat — does NOT follow symlinks),
        // never `path.is_dir()` (stat — follows). The HF cache resolves each
        // weight to `snapshots/<rev>/<file>.gguf` as a SYMLINK into `blobs/`.
        // Following that link `stat`s the multi-GB blob — and when that blob is
        // the GGUF currently mmap'd + Metal-wired by a live Local model, the
        // stat trips a macOS `pmap_recycle_page` kernel panic that takes the
        // whole machine down (see `ai-docs/external-to-local-llm-crash-
        // investigation.md`). We only need the symlink's NAME (its extension),
        // so we must not touch the link target at all.
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        // A real directory: recurse. Symlinked dirs are intentionally NOT
        // followed (the HF layout never nests real weight dirs behind a
        // symlink, and following one risks the same blob stat).
        if ft.is_dir() {
            // Skip HF Hub's `blobs/` dir: it holds the (multi-GB) raw blobs
            // with hash filenames and no extension. The resolved weight file
            // lives in `snapshots/<rev>/` as a symlink into blobs, so pruning
            // blobs avoids a deep scan of the heaviest dir without missing any
            // match.
            if path.file_name().is_some_and(|n| n == "blobs") {
                continue;
            }
            if depth < MAX_DEPTH {
                collect_files(&path, ext, out, depth + 1);
            }
        } else if (ft.is_file() || ft.is_symlink())
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        {
            // A plain file OR a symlink whose NAME ends in `ext`. For the
            // symlink we match on the link name only — we deliberately do not
            // `stat` the target to confirm it resolves (that's the crash), and
            // the HF cache guarantees a `snapshots/` symlink points at a
            // downloaded blob anyway.
            out.push(path);
        }
    }
}

#[tauri::command]
pub async fn get_model_status(state: State<'_, AppState>) -> AppResult<ModelStatusReport> {
    // Polled every few seconds by Settings. Run the directory scan off the
    // UI/event-loop thread: during the initial download the HF cache root is
    // both deep and under heavy IO, so a synchronous recursive read_dir here
    // would stutter the UI. Scan once and split into LLM (gguf) / embedding
    // (safetensors) so the deep walk isn't repeated per model.
    //
    // The scan target is the EFFECTIVE `HF_HOME` (shell env / `.env`
    // template / fallback to `<data>/models`), NOT just `<data>/models`.
    // Otherwise a user who points `HF_HOME` at a shared cache would see
    // the Settings card stuck on "preparing" because the gguf the sidecar
    // is actually loading lives outside the data root. Resolved once at
    // `Sidecars::new`, so each poll just reads the cached path.
    let models_dir = state.sidecars.effective_hf_home();
    // `collect_files` scans the HF cache WITHOUT following symlinks: dereferencing
    // a snapshots/<rev>/*.gguf link stats the multi-GB blob, and when that blob is
    // the live Metal-mmap'd GGUF it trips a macOS pmap_recycle_page kernel panic
    // (see ai-docs/external-to-local-llm-crash-investigation.md).
    let (gguf, safetensors) = tauri::async_runtime::spawn_blocking(move || {
        let g = list_model_files(&models_dir, "gguf");
        let s = list_model_files(&models_dir, "safetensors");
        (g, s)
    })
    .await
    .map_err(|e| AppError::Config(format!("model dir scan join: {e}")))?;
    let sidecars_ready = state.sidecars.current_endpoints().is_some();
    // Two failure sources, fatal first:
    //   1. Hard startup failure (`last_start_error`): the sidecars never came
    //      up at all (spawn error, port pick, TCP health-check timeout). The
    //      `sidecar://error` event is one-shot, so we read the retained
    //      message here.
    //   2. Non-fatal worker/plugin warnings (`last_report.warnings`): the
    //      sidecars are up but a runner couldn't be registered (e.g. a missing
    //      dylib), so the models can never finish preparing.
    // Either blocks BOTH models (the warnings cover plugin staging, which is
    // shared), so we apply the same error to each.
    let last_error = state.sidecars.llm_blocking_error();
    // External LLM mode: no local model to download — always ready.
    let llm_id = llm_identity(&state.data);
    let embedding_id = embedding_identity(&state.data);
    let is_external = state.is_external_llm();
    let (llm_files, llm_ready, llm_error) = if is_external {
        // Sidecars still need to be up for the genai worker to function.
        (
            vec![std::path::PathBuf::from("external")],
            sidecars_ready,
            last_error.as_deref().map(String::from),
        )
    } else {
        // Narrow the unfiltered HF_HOME scan to files that actually live
        // under the SELECTED repo's cache segment. Without this, leftover
        // weights from a previously-selected preset (e.g. Qwen3.6-27B
        // GGUF still on disk) would make the Settings card report `ready`
        // against a newly-picked repo whose weights aren't downloaded.
        (
            filter_files_for_repo(gguf, llm_id.repo.as_deref()),
            sidecars_ready,
            last_error.clone(),
        )
    };
    let embedding_files = filter_files_for_repo(safetensors, embedding_id.repo.as_deref());
    Ok(ModelStatusReport {
        llm: classify_model_status(&llm_files, llm_ready, llm_error.as_deref(), llm_id),
        embedding: classify_model_status(
            &embedding_files,
            sidecars_ready,
            last_error.as_deref(),
            embedding_id,
        ),
    })
}

/// FR-CONFIG-5 retry: re-run the recovery path, not just re-read status.
///
/// The `stop()` is load-bearing: it releases the idempotent `start` guard
/// AND forces jobworkerp to re-scan `PLUGINS_RUNNER_DIR` on the next boot
/// (it only scans at startup), so a dylib the user just dropped in is picked
/// up. Re-emits `sidecar://ready` / `sidecar://error` like the initial boot,
/// so existing listeners and `get_model_status` converge with no bespoke
/// retry plumbing.
#[tauri::command]
pub async fn retry_model_setup(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    state.sidecars.stop().await?;
    // Drop cached gRPC clients between stop and start: ports are re-selected
    // and the connections to the stopped sidecar are dead, so the next
    // command must reconnect against the freshly chosen endpoints.
    state.invalidate_clients().await;
    let sidecars = state.sidecars.clone();
    let data = state.data.clone();
    crate::stage_and_start_sidecars(&app, &sidecars, &data).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn id(name: &str) -> ModelIdentity {
        ModelIdentity {
            name: Some(name.into()),
            repo: Some(format!("org/{name}")),
        }
    }

    #[test]
    fn classify_ready_when_gguf_present_and_sidecars_up() {
        let files = vec![pb("/m/model.gguf")];
        let status = classify_model_status(&files, true, None, id("m"));
        assert_eq!(status.state, ModelState::Ready);
        assert!(status.error.is_none());
        // Identity is carried through to the UI regardless of state.
        assert_eq!(status.name.as_deref(), Some("m"));
        assert_eq!(status.repo.as_deref(), Some("org/m"));
    }

    #[test]
    fn classify_preparing_when_no_gguf() {
        let status = classify_model_status(&[], true, None, ModelIdentity::default());
        assert_eq!(status.state, ModelState::Preparing);
    }

    #[test]
    fn classify_preparing_when_gguf_but_sidecars_down() {
        let files = vec![pb("/m/model.gguf")];
        let status = classify_model_status(&files, false, None, ModelIdentity::default());
        assert_eq!(status.state, ModelState::Preparing);
    }

    #[test]
    fn classify_failed_overrides_everything() {
        // Even with a cached model and serving sidecars, a reported error
        // takes precedence so the UI shows the retry affordance. Identity is
        // still carried so the card can name the model that failed.
        let files = vec![pb("/m/model.gguf")];
        let status = classify_model_status(&files, true, Some("disk full"), id("m"));
        assert_eq!(status.state, ModelState::Failed);
        assert_eq!(status.error.as_deref(), Some("disk full"));
        assert_eq!(status.name.as_deref(), Some("m"));
    }

    #[test]
    fn llm_identity_uses_default_preset_when_settings_file_absent() {
        // Missing `llm-settings.json` must show the bundled default preset
        // in the Settings card. Mirrors `lib.rs::build_sidecar_config`,
        // which injects the same identity into the jobworkerp child.
        unsafe { std::env::remove_var("LOOKBACK_LLM_MODEL") };
        unsafe { std::env::remove_var("LOOKBACK_LLM_HF_REPO") };
        let data = crate::data::DataPaths::with_root(
            std::env::temp_dir().join("lookback-model-test-no-settings"),
        );
        let id = llm_identity(&data);
        let default_preset = crate::commands::llm_presets::default_preset();
        assert_eq!(id.name.as_deref(), Some(default_preset.gguf_file));
        assert_eq!(id.repo.as_deref(), Some(default_preset.hf_repo));
    }

    #[test]
    fn llm_identity_reflects_user_selected_preset() {
        // After the user picks a non-default preset, the Settings card
        // must reflect it BEFORE the sidecar restart finishes — the
        // identity is read from `llm-settings.json` synchronously.
        unsafe { std::env::remove_var("LOOKBACK_LLM_MODEL") };
        unsafe { std::env::remove_var("LOOKBACK_LLM_HF_REPO") };
        let dir = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(dir.path().to_path_buf());
        let settings = crate::commands::llm_settings::LlmSettings {
            local_preset_id: Some("qwen3-5-9b-ud-q4-k-xl".into()),
            ..Default::default()
        };
        crate::commands::llm_settings::save_llm_settings(&data.llm_settings_path(), &settings)
            .unwrap();
        let id = llm_identity(&data);
        let preset = crate::commands::llm_presets::find_preset("qwen3-5-9b-ud-q4-k-xl").unwrap();
        assert_eq!(id.name.as_deref(), Some(preset.gguf_file));
        assert_eq!(id.repo.as_deref(), Some(preset.hf_repo));
    }

    #[test]
    fn llm_identity_reflects_custom_fields() {
        // Custom preset takes the user-typed `local_model_file` /
        // `local_hf_repo` verbatim.
        unsafe { std::env::remove_var("LOOKBACK_LLM_MODEL") };
        unsafe { std::env::remove_var("LOOKBACK_LLM_HF_REPO") };
        let dir = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(dir.path().to_path_buf());
        let settings = crate::commands::llm_settings::LlmSettings {
            local_preset_id: Some(crate::commands::llm_presets::CUSTOM_PRESET_ID.into()),
            local_model_file: Some("my-custom.gguf".into()),
            local_hf_repo: Some("me/my-custom".into()),
            ..Default::default()
        };
        crate::commands::llm_settings::save_llm_settings(&data.llm_settings_path(), &settings)
            .unwrap();
        let id = llm_identity(&data);
        assert_eq!(id.name.as_deref(), Some("my-custom.gguf"));
        assert_eq!(id.repo.as_deref(), Some("me/my-custom"));
    }

    #[test]
    fn llm_identity_external_mode_returns_provider_model() {
        // External mode regression: the Settings card surfaces the genai
        // provider model + display name, NOT the local preset (which is
        // irrelevant when the dispatch target is `memories-llm-external`).
        let dir = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(dir.path().to_path_buf());
        let settings = crate::commands::llm_settings::LlmSettings {
            mode: crate::commands::llm_settings::LlmMode::External,
            provider_model: Some("claude-sonnet-4-20250514".into()),
            ..Default::default()
        };
        crate::commands::llm_settings::save_llm_settings(&data.llm_settings_path(), &settings)
            .unwrap();
        let id = llm_identity(&data);
        assert_eq!(id.name.as_deref(), Some("claude-sonnet-4-20250514"));
        assert_eq!(id.repo.as_deref(), Some("Anthropic"));
    }

    /// Clear every `LOOKBACK_EMBEDDING_*` env var so an unrelated test
    /// (or a developer's shell) can't leak through `embedding_identity`'s
    /// env-aware resolver. SAFETY: relies on the `--test-threads=1`
    /// invariant documented in CLAUDE.md.
    fn clear_embedding_env() {
        for k in super::super::embedding_settings::EMBEDDING_ENV_KEYS {
            unsafe { std::env::remove_var(k) };
        }
    }

    #[test]
    fn embedding_identity_defaults_to_default_preset_when_settings_absent() {
        // Fresh install: no `embedding-settings.json` → resolver falls back to
        // the default preset, and the Settings card shows that preset's repo.
        clear_embedding_env();
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path().to_path_buf());
        let id = embedding_identity(&data);
        let default_preset = super::super::embedding_presets::default_preset();
        assert_eq!(id.name.as_deref(), Some(default_preset.hf_repo));
        assert_eq!(id.repo.as_deref(), Some(default_preset.hf_repo));
    }

    #[test]
    fn embedding_identity_follows_user_selected_preset() {
        // Regression: a non-default preset must surface in the Settings card so
        // the readiness check reflects what the sidecar will actually load —
        // otherwise an unrelated safetensors file in HF_HOME makes the card
        // mis-report "ready" against an unselected model.
        clear_embedding_env();
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path().to_path_buf());
        let settings = super::super::embedding_settings::EmbeddingSettings {
            preset_id: Some("qwen3-embedding-0-6b".into()),
            ..Default::default()
        };
        super::super::embedding_settings::save_embedding_settings(
            &data.embedding_settings_path(),
            &settings,
        )
        .unwrap();
        let id = embedding_identity(&data);
        assert_eq!(id.name.as_deref(), Some("Qwen/Qwen3-Embedding-0.6B"));
    }

    #[test]
    fn embedding_identity_follows_shell_env_when_no_preset_saved() {
        // Regression: dev workflows can drive the sidecar via
        // `LOOKBACK_EMBEDDING_MODEL_ID` without saving Settings. The
        // readiness scan MUST filter for the env-pointed repo too —
        // otherwise the right weights are on disk but the card filters
        // them out and stays stuck on `preparing`.
        clear_embedding_env();
        unsafe { std::env::set_var("LOOKBACK_EMBEDDING_MODEL_ID", "dev/dev-model") };
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path().to_path_buf());
        let id = embedding_identity(&data);
        clear_embedding_env();
        assert_eq!(id.name.as_deref(), Some("dev/dev-model"));
        assert_eq!(id.repo.as_deref(), Some("dev/dev-model"));
    }

    #[test]
    fn embedding_identity_ignores_shell_env_when_preset_saved() {
        // Pin the "saved settings are authoritative" contract: a stray
        // dev export must NOT override the user's saved preset choice.
        clear_embedding_env();
        unsafe { std::env::set_var("LOOKBACK_EMBEDDING_MODEL_ID", "stale/override") };
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path().to_path_buf());
        let settings = super::super::embedding_settings::EmbeddingSettings {
            preset_id: Some("qwen3-embedding-0-6b".into()),
            ..Default::default()
        };
        super::super::embedding_settings::save_embedding_settings(
            &data.embedding_settings_path(),
            &settings,
        )
        .unwrap();
        let id = embedding_identity(&data);
        clear_embedding_env();
        assert_eq!(id.name.as_deref(), Some("Qwen/Qwen3-Embedding-0.6B"));
    }

    #[test]
    fn list_model_files_finds_nested_and_ignores_others() {
        let dir = std::env::temp_dir().join(format!("lookback-gguf-test-{}", std::process::id()));
        let nested = dir.join("models--org--name").join("snapshots").join("rev");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("model.gguf"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.join("top.GGUF"), b"x").unwrap();

        let found = list_model_files(&dir, "gguf");
        // Both the nested .gguf and the case-insensitive top-level .GGUF
        // are detected; the .txt is ignored.
        assert_eq!(found.len(), 2, "found: {found:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_model_files_finds_safetensors_for_embedding() {
        // Embedding weights are HF safetensors, not gguf; the same walker must
        // pick them up under snapshots and ignore the tokenizer-only case.
        let dir = std::env::temp_dir().join(format!("lookback-st-test-{}", std::process::id()));
        let snap = dir
            .join("models--Qwen--Qwen3-VL-Embedding-2B")
            .join("snapshots")
            .join("rev");
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(snap.join("tokenizer.json"), b"x").unwrap();
        std::fs::write(snap.join("config.json"), b"x").unwrap();

        // Tokenizer/config present but no weights yet → not "ready".
        assert!(
            list_model_files(&dir, "safetensors").is_empty(),
            "weights absent must yield no safetensors match"
        );

        // Once the weights land, they're detected.
        std::fs::write(snap.join("model.safetensors"), b"x").unwrap();
        let found = list_model_files(&dir, "safetensors");
        assert_eq!(found.len(), 1, "found: {found:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hf_repo_cache_segment_translates_canonical_repo() {
        // Pin the HF Hub naming convention: `org/name` ⇒ `models--org--name`.
        // Anything else returns None so the caller falls back to the
        // unfiltered scan rather than silently matching nothing.
        assert_eq!(
            hf_repo_cache_segment("Qwen/Qwen3-Embedding-0.6B").as_deref(),
            Some("models--Qwen--Qwen3-Embedding-0.6B")
        );
        assert_eq!(
            hf_repo_cache_segment("cl-nagoya/ruri-v3-310m").as_deref(),
            Some("models--cl-nagoya--ruri-v3-310m")
        );
        assert!(hf_repo_cache_segment("no-slash").is_none());
        assert!(hf_repo_cache_segment("/leading").is_none());
        assert!(hf_repo_cache_segment("trailing/").is_none());
        assert!(hf_repo_cache_segment("two/slashes/here").is_none());
    }

    #[test]
    fn filter_files_for_repo_keeps_only_selected_repo() {
        // Regression: leftover weights from a previously-selected preset
        // must NOT count as readiness for a new preset. The filter
        // narrows by the HF cache segment so a switch from Qwen3-VL to
        // Qwen3-Embedding-0.6B doesn't mis-report `ready` against weights
        // that belong to the OLD repo.
        let files = vec![
            PathBuf::from(
                "/cache/models--Qwen--Qwen3-VL-Embedding-2B/snapshots/rev/model.safetensors",
            ),
            PathBuf::from(
                "/cache/models--Qwen--Qwen3-Embedding-0.6B/snapshots/rev/model.safetensors",
            ),
            PathBuf::from("/cache/models--unrelated--something/snapshots/rev/x.safetensors"),
        ];
        let only_new = filter_files_for_repo(files, Some("Qwen/Qwen3-Embedding-0.6B"));
        assert_eq!(only_new.len(), 1);
        assert!(
            only_new[0]
                .to_string_lossy()
                .contains("Qwen3-Embedding-0.6B")
        );
    }

    #[test]
    fn filter_files_for_repo_returns_input_unchanged_when_repo_unknown() {
        // Defensive: if the identity has no repo (e.g. external LLM
        // mode or a corrupt settings file), preserve the existing
        // readiness behaviour rather than collapsing to "preparing".
        let files = vec![PathBuf::from("/cache/models--x--y/snapshots/r/model.gguf")];
        let kept = filter_files_for_repo(files.clone(), None);
        assert_eq!(kept, files);
        // Same fallback for a malformed identifier.
        let kept = filter_files_for_repo(files.clone(), Some("not a repo"));
        assert_eq!(kept, files);
    }

    #[test]
    fn filter_files_for_repo_yields_empty_when_only_old_repo_present() {
        // The headline scenario from the review: the previous preset's
        // weights are still on disk but the new preset's directory does
        // not exist yet. Filter must report empty → Settings card stays
        // on `preparing` instead of mis-flagging `ready`.
        let files = vec![PathBuf::from(
            "/cache/models--Qwen--Qwen3-VL-Embedding-2B/snapshots/rev/model.safetensors",
        )];
        let kept = filter_files_for_repo(files, Some("Qwen/Qwen3-Embedding-0.6B"));
        assert!(kept.is_empty());
    }

    #[test]
    fn list_model_files_empty_for_missing_dir() {
        let missing = std::env::temp_dir().join("lookback-gguf-does-not-exist-xyz");
        assert!(list_model_files(&missing, "gguf").is_empty());
    }

    #[test]
    fn list_model_files_prunes_hf_blobs_dir() {
        // HF Hub stores the resolved weight under `snapshots/<rev>/`; the
        // multi-GB `blobs/` dir holds hash-named files (no extension) and a
        // stray match there must not be scanned (we skip the whole dir).
        let dir = std::env::temp_dir().join(format!("lookback-blobs-test-{}", std::process::id()));
        let blobs = dir.join("models--org--name").join("blobs");
        let snap = dir.join("models--org--name").join("snapshots").join("rev");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(blobs.join("decoy.gguf"), b"x").unwrap();
        std::fs::write(snap.join("model.gguf"), b"x").unwrap();

        let found = list_model_files(&dir, "gguf");
        assert_eq!(found.len(), 1, "found: {found:?}");
        assert!(found[0].ends_with("snapshots/rev/model.gguf"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // Regression for the macOS pmap_recycle_page crash: the scan must detect a
    // snapshots/<rev>/<file>.gguf SYMLINK by name WITHOUT following it (a
    // followed stat of the live Metal-mmap'd blob panics the kernel). A dangling
    // link is used so detection proves the target was never dereferenced.
    #[cfg(unix)]
    #[test]
    fn scan_detects_gguf_symlink_without_following_target() {
        use std::os::unix::fs::symlink;

        let dir =
            std::env::temp_dir().join(format!("lookback-symlink-test-{}", std::process::id()));
        let snap = dir.join("models--org--name").join("snapshots").join("rev");
        std::fs::create_dir_all(&snap).unwrap();
        symlink("/nonexistent/blob-deadbeef", snap.join("model.gguf")).unwrap();

        let found = list_model_files(&dir, "gguf");
        assert_eq!(
            found.len(),
            1,
            "dangling .gguf symlink must still match: {found:?}"
        );
        assert!(found[0].ends_with("snapshots/rev/model.gguf"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
