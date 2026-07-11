//! Embedding-model settings: persisted preset / custom override, runtime
//! resolution, and the sidecar-restart pipeline with vectordb evacuation
//! and auto-rollback.
//!
//! Mirrors [`super::llm_settings`] in shape so the two settings cards in
//! the UI share their pattern (load → resolve → env-inject → restart). The
//! single extra responsibility on the embedding side is that changing the
//! vector dimension invalidates every existing LanceDB row, so the change
//! pipeline either renames the lancedb dir into a timestamped backup or
//! deletes it before the sidecar comes back up.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::Emitter;

use crate::data::DataPaths;
use crate::error::{AppError, AppResult};

use super::embedding_presets::{self, EmbeddingPreset};

/// Range guards obvious typos; the embedding runner enforces a stricter
/// per-model cap downstream.
const VECTOR_SIZE_MIN: u32 = 1;
const VECTOR_SIZE_MAX: u32 = 8192;
const MAX_SEQ_LEN_MIN: u32 = 64;
const MAX_SEQ_LEN_MAX: u32 = 131_072;

/// Supported dtype values. `MultimodalEmbeddingRunner` rejects anything
/// else at registration time with `UnsupportedDType`.
const SUPPORTED_DTYPES: &[&str] = &["F16", "BF16", "F32"];

/// Single source of truth for embedding env managed by Lookback. Runner
/// overrides use `LOOKBACK_EMBEDDING_*`; memories-owned retrieval prefixes
/// use `MEMORY_EMBEDDING_*` so the downstream contract stays app-agnostic.
/// Used by [`resolve_embedding_env_vars`] (producer), the sidecar
/// lifecycle's `apply_embedding_env` (consumer that clears stale keys),
/// and the test helpers in `commands::model`. Adding a key here propagates
/// it everywhere; missing one used to silently fail to clear / read.
pub const EMBEDDING_ENV_KEYS: &[&str] = &[
    "LOOKBACK_EMBEDDING_MODEL_ID",
    "LOOKBACK_EMBEDDING_TOKENIZER_ID",
    "LOOKBACK_EMBEDDING_DTYPE",
    "LOOKBACK_EMBEDDING_MAX_SEQ_LEN",
    "LOOKBACK_EMBEDDING_VECTOR_SIZE",
    "MEMORY_EMBEDDING_DOCUMENT_PREFIX",
    "MEMORY_EMBEDDING_QUERY_PREFIX",
];

/// Persisted (non-secret) embedding-model config.
///
/// All fields are `Option<…>` so a `embedding-settings.json` written before
/// this feature deserialises unchanged into `None` everywhere — the
/// resolver then falls back to the default preset, which the previous
/// release hardcoded as `Qwen/Qwen3-VL-Embedding-2B`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EmbeddingSettings {
    /// Curated preset id (see `embedding_presets::PRESETS`), the sentinel
    /// `"custom"` to take values from `custom_*`, or `None` (pre-feature
    /// settings file) which is treated as the default preset.
    #[serde(default)]
    pub preset_id: Option<String>,
    #[serde(default)]
    pub custom_model_id: Option<String>,
    #[serde(default)]
    pub custom_tokenizer_id: Option<String>,
    #[serde(default)]
    pub custom_vector_size: Option<u32>,
    #[serde(default)]
    pub custom_dtype: Option<String>,
    #[serde(default)]
    pub custom_max_sequence_length: Option<u32>,
    /// Whether the custom model is jointly text+image. `false` means image
    /// search is disabled in the UI. Has no effect on a preset selection
    /// (the preset's own flag wins).
    #[serde(default)]
    pub custom_is_multimodal: Option<bool>,
}

/// Frontend-facing response. Adds derived fields the UI uses to render
/// disabled / warning states without re-querying.
#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingSettingsResponse {
    pub preset_id: Option<String>,
    pub custom_model_id: Option<String>,
    pub custom_tokenizer_id: Option<String>,
    pub custom_vector_size: Option<u32>,
    pub custom_dtype: Option<String>,
    pub custom_max_sequence_length: Option<u32>,
    pub custom_is_multimodal: Option<bool>,
    /// Resolved runtime — the values the sidecar will actually use on the
    /// next restart. Saves the UI a second roundtrip for "what's
    /// effective right now".
    pub effective: EmbeddingRuntime,
    /// `true` when `connection.json` is in remote mode; the UI uses this
    /// to disable inputs and show the warning banner.
    pub connection_remote: bool,
}

/// Request from the frontend. `evacuate_vectordb = true` renames the
/// existing `<root>/lancedb` into `<root>/lancedb-backup/<ts>` instead of
/// deleting it. Has no effect when the new runtime matches the old.
#[derive(Debug, Clone, Deserialize)]
pub struct SetEmbeddingSettingsRequest {
    pub preset_id: Option<String>,
    #[serde(default)]
    pub custom_model_id: Option<String>,
    #[serde(default)]
    pub custom_tokenizer_id: Option<String>,
    #[serde(default)]
    pub custom_vector_size: Option<u32>,
    #[serde(default)]
    pub custom_dtype: Option<String>,
    #[serde(default)]
    pub custom_max_sequence_length: Option<u32>,
    #[serde(default)]
    pub custom_is_multimodal: Option<bool>,
    /// Whether to keep the pre-resize LanceDB as a timestamped backup
    /// (default: true) or delete it outright.
    #[serde(default = "default_evacuate")]
    pub evacuate_vectordb: bool,
}

fn default_evacuate() -> bool {
    true
}

/// Response of a successful save. `backup_path` is `Some(...)` only when a
/// dimension-changing swap happened AND `evacuate_vectordb` was true.
#[derive(Debug, Clone, Serialize)]
pub struct SetEmbeddingSettingsResponse {
    pub runtime: EmbeddingRuntime,
    pub backup_path: Option<PathBuf>,
    pub restarted: bool,
}

/// Resolved runtime values consumed by `sidecar/lifecycle.rs` and the
/// staged YAML renderer. Produced by [`resolve_embedding_runtime`] from
/// the persisted settings + the preset table.
///
/// The runner's `device` is intentionally NOT carried here. It is a
/// platform build concern: the YAML renderer emits Metal on macOS and CUDA
/// elsewhere to match the staged plugin binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EmbeddingRuntime {
    pub model_id: String,
    /// `Some(_)` ⇒ the staged YAML emits a `tokenizer_model_id` line;
    /// `None` ⇒ the line is dropped and the runner reuses `model_id`.
    pub tokenizer_id: Option<String>,
    pub vector_size: u32,
    pub dtype: String,
    pub max_sequence_length: u32,
    pub onnx_model_file: Option<String>,
    pub onnx_pooling: Option<String>,
    pub document_prefix: Option<String>,
    pub query_prefix: Option<String>,
    /// `false` ⇒ the UI shows the "画像検索は無効化されます" chip. Does
    /// not affect sidecar env on its own — the image path is gated by
    /// `MEMORY_IMAGE_SEARCH_MODE` on the memories side, which agent-app
    /// keeps unset (= the safe `none` default).
    pub is_multimodal: bool,
}

/// Map a curated preset into runtime values.
fn runtime_from_preset(preset: &EmbeddingPreset) -> EmbeddingRuntime {
    EmbeddingRuntime {
        model_id: preset.hf_repo.to_string(),
        tokenizer_id: preset.tokenizer_hf_repo.map(str::to_string),
        vector_size: preset.vector_size,
        dtype: preset.dtype.to_string(),
        max_sequence_length: preset.max_sequence_length,
        onnx_model_file: preset.onnx_model_file.map(str::to_string),
        onnx_pooling: preset.onnx_pooling.map(str::to_string),
        document_prefix: preset.document_prefix.map(str::to_string),
        query_prefix: preset.query_prefix.map(str::to_string),
        is_multimodal: preset.is_multimodal,
    }
}

/// Project `EmbeddingSettings` into the runtime values the sidecar /
/// YAML renderer needs. Pure so the precedence (custom > preset > default
/// preset) is unit-testable.
///
/// `env_lookup` lets a dev shell env (`LOOKBACK_EMBEDDING_*`) win **only**
/// when the user has not saved a preset (`preset_id == None`) — once they
/// DO save, the file is authoritative so a stray dev export can't silently
/// re-route the next launch back to the old model. Pass `|_| None` (via
/// [`resolve_embedding_runtime`]) for callers that don't want env
/// overrides at all (e.g. UI display).
///
/// Unknown / retired preset ids degrade gracefully to the default preset
/// rather than producing an empty `model_id` (which would make the
/// embedding runner refuse to register at startup).
pub fn resolve_embedding_runtime_with_env<F>(
    settings: &EmbeddingSettings,
    env_lookup: F,
) -> EmbeddingRuntime
where
    F: Fn(&str) -> Option<String>,
{
    if settings.preset_id.as_deref() == Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID) {
        let default = runtime_from_preset(embedding_presets::default_preset());
        return EmbeddingRuntime {
            model_id: settings
                .custom_model_id
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(default.model_id),
            tokenizer_id: settings
                .custom_tokenizer_id
                .clone()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            // `validate_set_request` rejects a custom save without
            // `custom_vector_size`, so the only way we reach this
            // fallback is a hand-edited / corrupt `embedding-settings.json`.
            // Fall back to the default preset's dim so the sidecar still
            // boots; the user will see the wrong dim in Settings and can
            // re-save.
            vector_size: settings.custom_vector_size.unwrap_or(default.vector_size),
            dtype: settings
                .custom_dtype
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(default.dtype),
            max_sequence_length: settings
                .custom_max_sequence_length
                .unwrap_or(default.max_sequence_length),
            onnx_model_file: None,
            onnx_pooling: None,
            document_prefix: None,
            query_prefix: None,
            // `is_multimodal` defaults to `false` for custom because we
            // cannot infer it from a free-text HF repo, and silently
            // claiming text+image support would render the image search
            // UI without a backing model.
            is_multimodal: settings.custom_is_multimodal.unwrap_or(false),
        };
    }
    let preset_runtime = runtime_from_preset(
        settings
            .preset_id
            .as_deref()
            .and_then(embedding_presets::find_preset)
            .unwrap_or_else(embedding_presets::default_preset),
    );
    if settings.preset_id.is_some() {
        return preset_runtime;
    }
    // No saved preset → honour the dev shell env overrides. Anything
    // unset falls through to the default preset values; preserving the
    // preset's `is_multimodal` flag because that's a property of the
    // weights, not something a shell var can sensibly toggle.
    EmbeddingRuntime {
        model_id: env_lookup("LOOKBACK_EMBEDDING_MODEL_ID")
            .filter(|s| !s.is_empty())
            .unwrap_or(preset_runtime.model_id),
        tokenizer_id: env_lookup("LOOKBACK_EMBEDDING_TOKENIZER_ID")
            .filter(|s| !s.is_empty())
            .or(preset_runtime.tokenizer_id),
        vector_size: env_lookup("LOOKBACK_EMBEDDING_VECTOR_SIZE")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(preset_runtime.vector_size),
        dtype: env_lookup("LOOKBACK_EMBEDDING_DTYPE")
            .filter(|s| !s.is_empty())
            .unwrap_or(preset_runtime.dtype),
        max_sequence_length: env_lookup("LOOKBACK_EMBEDDING_MAX_SEQ_LEN")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(preset_runtime.max_sequence_length),
        onnx_model_file: preset_runtime.onnx_model_file,
        onnx_pooling: preset_runtime.onnx_pooling,
        document_prefix: env_lookup("MEMORY_EMBEDDING_DOCUMENT_PREFIX")
            .filter(|s| !s.is_empty())
            .or(preset_runtime.document_prefix),
        query_prefix: env_lookup("MEMORY_EMBEDDING_QUERY_PREFIX")
            .filter(|s| !s.is_empty())
            .or(preset_runtime.query_prefix),
        is_multimodal: preset_runtime.is_multimodal,
    }
}

/// Pure projection that ignores any environment overrides. Used by the UI
/// display path (`get_embedding_settings` response, `embedding_identity`
/// for the Settings model card) where dev env overrides should not bleed
/// into "what the user thinks the saved value is".
pub fn resolve_embedding_runtime(settings: &EmbeddingSettings) -> EmbeddingRuntime {
    resolve_embedding_runtime_with_env(settings, |_| None)
}

/// Embedding env vars derived from `runtime`. The runtime
/// IS the source of truth for these values, so deriving the env from it
/// guarantees the staged YAML, `MEMORY_VECTOR_SIZE`, and the process-env-
/// driven `expand_env` placeholders all agree. Callers that already
/// resolved a runtime should pass it directly; callers starting from
/// settings can use [`resolve_embedding_env_vars`].
pub fn resolve_embedding_env_vars_from_runtime(
    runtime: &EmbeddingRuntime,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::with_capacity(EMBEDDING_ENV_KEYS.len());
    out.push(("LOOKBACK_EMBEDDING_MODEL_ID", runtime.model_id.clone()));
    if let Some(tok) = runtime.tokenizer_id.as_ref() {
        out.push(("LOOKBACK_EMBEDDING_TOKENIZER_ID", tok.clone()));
    }
    out.push(("LOOKBACK_EMBEDDING_DTYPE", runtime.dtype.clone()));
    out.push((
        "LOOKBACK_EMBEDDING_MAX_SEQ_LEN",
        runtime.max_sequence_length.to_string(),
    ));
    out.push((
        "LOOKBACK_EMBEDDING_VECTOR_SIZE",
        runtime.vector_size.to_string(),
    ));
    if let Some(prefix) = runtime.document_prefix.as_ref() {
        out.push(("MEMORY_EMBEDDING_DOCUMENT_PREFIX", prefix.clone()));
    }
    if let Some(prefix) = runtime.query_prefix.as_ref() {
        out.push(("MEMORY_EMBEDDING_QUERY_PREFIX", prefix.clone()));
    }
    out
}

/// Convenience: resolve a runtime from settings + env, then derive env
/// vars from it. Kept for callers (mainly tests) that don't already
/// have a runtime in hand.
pub fn resolve_embedding_env_vars<F>(
    settings: &EmbeddingSettings,
    env_lookup: F,
) -> Vec<(&'static str, String)>
where
    F: Fn(&str) -> Option<String>,
{
    let runtime = resolve_embedding_runtime_with_env(settings, env_lookup);
    resolve_embedding_env_vars_from_runtime(&runtime)
}

use super::is_valid_hf_repo;

pub fn is_valid_dtype(s: &str) -> bool {
    SUPPORTED_DTYPES.contains(&s)
}

/// Validate a Custom-mode request. Returns the offending field's error
/// message on failure so `set_embedding_settings` can reject without
/// restarting the sidecar.
fn validate_custom_fields(req: &SetEmbeddingSettingsRequest) -> Result<(), String> {
    let model = req
        .custom_model_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "custom preset requires `custom_model_id`".to_string())?;
    if !is_valid_hf_repo(model) {
        return Err(format!(
            "invalid custom_model_id {model:?}: expected `org/name` with [A-Za-z0-9_.-]"
        ));
    }
    if let Some(tok) = req
        .custom_tokenizer_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && !is_valid_hf_repo(tok)
    {
        return Err(format!(
            "invalid custom_tokenizer_id {tok:?}: expected `org/name` with [A-Za-z0-9_.-]"
        ));
    }
    if let Some(dtype) = req
        .custom_dtype
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && !is_valid_dtype(dtype)
    {
        return Err(format!(
            "invalid custom_dtype {dtype:?}: expected one of {SUPPORTED_DTYPES:?}"
        ));
    }
    // Required: without an explicit value the resolver would fall back
    // to the default preset's dim, mismatching the user's actual custom
    // model. memories would then open LanceDB at the wrong dim while the
    // runner emits at the model's own dim and every upsert fails.
    let vs = req
        .custom_vector_size
        .ok_or_else(|| "custom preset requires `custom_vector_size`".to_string())?;
    if !(VECTOR_SIZE_MIN..=VECTOR_SIZE_MAX).contains(&vs) {
        return Err(format!(
            "invalid custom_vector_size {vs}: must be in [{VECTOR_SIZE_MIN}, {VECTOR_SIZE_MAX}]"
        ));
    }
    if let Some(m) = req.custom_max_sequence_length
        && !(MAX_SEQ_LEN_MIN..=MAX_SEQ_LEN_MAX).contains(&m)
    {
        return Err(format!(
            "invalid custom_max_sequence_length {m}: must be in [{MAX_SEQ_LEN_MIN}, {MAX_SEQ_LEN_MAX}]"
        ));
    }
    Ok(())
}

pub fn validate_set_request(req: &SetEmbeddingSettingsRequest) -> Result<(), String> {
    if req.preset_id.as_deref() == Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID) {
        return validate_custom_fields(req);
    }
    if let Some(id) = req.preset_id.as_deref()
        && embedding_presets::find_preset(id).is_none()
    {
        return Err(format!(
            "unknown preset_id {id:?}: not in the curated preset list"
        ));
    }
    Ok(())
}

// ── persistence ──────────────────────────────────────────────────────

pub fn load_embedding_settings(path: &Path) -> EmbeddingSettings {
    let Ok(bytes) = std::fs::read(path) else {
        return EmbeddingSettings::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save_embedding_settings(path: &Path, settings: &EmbeddingSettings) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| AppError::Config(format!("serialize embedding settings: {e}")))?;
    std::fs::write(path, json)?;
    Ok(())
}

// ── vectordb evacuation ─────────────────────────────────────────────

/// Whether to keep the pre-resize LanceDB as a backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvacuateMode {
    /// Rename `<root>/lancedb` → `<root>/lancedb-backup/<ts>/`.
    Evacuate,
    /// `remove_dir_all(<root>/lancedb)`.
    Delete,
}

/// Reset the LanceDB directory ahead of a sidecar restart that will land
/// on a new vector dimension. The source is replaced with an empty
/// directory either way so memories' startup probe creates fresh tables.
///
/// Returns `Some(backup_path)` only on the `Evacuate` path; `Delete`
/// returns `None`. A missing source is a no-op (returns `None`) — this
/// happens on a fresh install where the user changes the preset before
/// the first import.
pub fn evacuate_vectordb(data: &DataPaths, mode: EvacuateMode) -> AppResult<Option<PathBuf>> {
    let src = data.lancedb_dir();
    if !src.exists() {
        return Ok(None);
    }
    match mode {
        EvacuateMode::Delete => {
            std::fs::remove_dir_all(&src).map_err(|e| {
                AppError::Config(format!("delete lancedb dir {}: {e}", src.display()))
            })?;
            std::fs::create_dir_all(&src).map_err(|e| {
                AppError::Config(format!("recreate empty lancedb dir {}: {e}", src.display()))
            })?;
            Ok(None)
        }
        EvacuateMode::Evacuate => {
            let backup_root = data.lancedb_backup_dir();
            std::fs::create_dir_all(&backup_root).map_err(|e| {
                AppError::Config(format!(
                    "create lancedb backup root {}: {e}",
                    backup_root.display()
                ))
            })?;
            let dst = backup_root.join(unique_backup_name());
            // `rename` is atomic on the same filesystem and is what we
            // want for safety. The cross-device fallback (copy + remove)
            // is documented for completeness but not implemented in this
            // initial cut — both source and target live under the same
            // data root, which is on a single volume by construction.
            std::fs::rename(&src, &dst).map_err(|e| {
                AppError::Config(format!("rename {} → {}: {e}", src.display(), dst.display()))
            })?;
            std::fs::create_dir_all(&src).map_err(|e| {
                AppError::Config(format!("recreate empty lancedb dir {}: {e}", src.display()))
            })?;
            Ok(Some(dst))
        }
    }
}

/// Backup directory leaf name. Includes nanos so two saves within the
/// same second land on distinct paths.
fn unique_backup_name() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("lancedb-{}-{}", now.as_secs(), now.subsec_nanos(),)
}

/// Whether the runtime change between `old` and `new` requires the
/// existing LanceDB to be evacuated / reset before the sidecar restarts.
/// True when either the dimension or the model id changes; tokenizer /
/// dtype / max-seq tweaks alone don't invalidate the on-disk index.
/// Pure so the env-aware-comparison contract is unit-testable without
/// having to construct an `AppState`.
pub fn needs_vectordb_reset(old: &EmbeddingRuntime, new: &EmbeddingRuntime) -> bool {
    old.vector_size != new.vector_size || old.model_id != new.model_id
}

/// Restore the most recent backup directory back to `<root>/lancedb`,
/// used by the rollback path when a sidecar restart fails.
///
/// Returns `Ok(true)` when a restore happened, `Ok(false)` when no
/// matching backup was found (already-deleted, Delete-mode swap).
pub fn restore_vectordb_backup(data: &DataPaths, backup_path: &Path) -> AppResult<bool> {
    let src = data.lancedb_dir();
    if !backup_path.exists() {
        return Ok(false);
    }
    // Make sure the destination doesn't already hold an empty staged
    // dir from the failed restart — rename refuses to clobber a
    // non-empty target on macOS.
    if src.exists() {
        std::fs::remove_dir_all(&src).map_err(|e| {
            AppError::Config(format!("remove staged lancedb dir {}: {e}", src.display()))
        })?;
    }
    std::fs::rename(backup_path, &src).map_err(|e| {
        AppError::Config(format!(
            "restore backup {} → {}: {e}",
            backup_path.display(),
            src.display()
        ))
    })?;
    Ok(true)
}

// ── Tauri commands ───────────────────────────────────────────────────

#[tauri::command]
pub fn get_embedding_settings(
    state: tauri::State<'_, super::AppState>,
) -> AppResult<EmbeddingSettingsResponse> {
    let settings = load_embedding_settings(&state.data.embedding_settings_path());
    let connection_remote = matches!(
        super::connection::load_connection_config(&state.data.connection_config_path()).mode,
        super::connection::ConnectionMode::Remote
    );
    let effective = resolve_embedding_runtime(&settings);
    Ok(EmbeddingSettingsResponse {
        preset_id: settings.preset_id,
        custom_model_id: settings.custom_model_id,
        custom_tokenizer_id: settings.custom_tokenizer_id,
        custom_vector_size: settings.custom_vector_size,
        custom_dtype: settings.custom_dtype,
        custom_max_sequence_length: settings.custom_max_sequence_length,
        custom_is_multimodal: settings.custom_is_multimodal,
        effective,
        connection_remote,
    })
}

/// Outcome of persisting embedding settings to disk WITHOUT restarting
/// the sidecar. Carries everything the caller needs to drive evacuation,
/// restart, and rollback. Returned by [`apply_embedding_settings_to_disk`].
pub struct EmbeddingApplyOutcome {
    pub new_runtime: EmbeddingRuntime,
    /// `false` ⇒ no-op (old == new); the caller should skip the restart.
    pub changed: bool,
    /// `true` ⇒ the LanceDB must be evacuated/reset before restart.
    pub needs_vectordb_reset: bool,
    /// Pre-save settings, retained so the caller can roll back the file.
    pub old_settings: EmbeddingSettings,
}

/// Validate an embedding request WITHOUT persisting. Split out so the unified
/// `apply_settings` can validate the whole batch up front (a later card's
/// failure must not leave embedding-settings.json half-saved).
pub fn validate_embedding_request(req: &SetEmbeddingSettingsRequest) -> AppResult<()> {
    validate_set_request(req).map_err(AppError::Config)?;
    Ok(())
}

/// Validate and persist `embedding-settings.json` WITHOUT evacuating the
/// vectordb or restarting the sidecar. The caller
/// (the individual command or the unified `apply_settings`) owns the
/// stop → evacuate → restart → rollback sequence so several settings can
/// share a single restart.
///
/// When `changed == false` the file is left untouched (it equals the old
/// value anyway) and the caller should not restart.
pub fn apply_embedding_settings_to_disk(
    data: &DataPaths,
    req: &SetEmbeddingSettingsRequest,
) -> AppResult<EmbeddingApplyOutcome> {
    validate_embedding_request(req)?;

    let path = data.embedding_settings_path();
    let old_settings = load_embedding_settings(&path);
    let connection_remote = matches!(
        super::connection::load_connection_config(&data.connection_config_path()).mode,
        super::connection::ConnectionMode::Remote
    );
    // Resolve OLD and NEW runtimes with the same env-aware projection
    // the sidecar lifecycle uses. Without this, a dev shell override
    // (e.g. `LOOKBACK_EMBEDDING_VECTOR_SIZE=2048`) makes the sidecar
    // run at 2048 dim while old_runtime reports the env-blind default
    // dim, so `needs_vectordb_reset` would see "no change" when
    // saving the default preset and skip evacuation — the next launch
    // then opens the existing LanceDB at the new default dim and crashes.
    // NEW also takes the env-aware path so the comparison happens in
    // the SAME projection; a saved `preset_id = Some(_)` makes
    // `resolve_embedding_runtime_with_env` ignore env anyway, so this
    // does not let the dev shell silently drive the saved value.
    let old_runtime = resolve_embedding_runtime_with_env(&old_settings, super::process_env_lookup);

    let new_settings = EmbeddingSettings {
        preset_id: req.preset_id.clone(),
        custom_model_id: req.custom_model_id.clone(),
        custom_tokenizer_id: req.custom_tokenizer_id.clone(),
        custom_vector_size: req.custom_vector_size,
        custom_dtype: req.custom_dtype.clone(),
        custom_max_sequence_length: req.custom_max_sequence_length,
        custom_is_multimodal: req.custom_is_multimodal,
    };
    let new_runtime = resolve_embedding_runtime_with_env(&new_settings, super::process_env_lookup);

    if old_runtime == new_runtime && old_settings == new_settings {
        return Ok(EmbeddingApplyOutcome {
            new_runtime,
            changed: false,
            needs_vectordb_reset: false,
            old_settings,
        });
    }

    save_embedding_settings(&path, &new_settings)?;

    let needs_reset = !connection_remote && needs_vectordb_reset(&old_runtime, &new_runtime);
    Ok(EmbeddingApplyOutcome {
        new_runtime,
        changed: true,
        needs_vectordb_reset: needs_reset,
        old_settings,
    })
}

#[tauri::command]
pub async fn set_embedding_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, super::AppState>,
    req: SetEmbeddingSettingsRequest,
) -> AppResult<SetEmbeddingSettingsResponse> {
    let path = state.data.embedding_settings_path();
    let outcome = apply_embedding_settings_to_disk(&state.data, &req)?;
    let EmbeddingApplyOutcome {
        new_runtime,
        changed,
        needs_vectordb_reset,
        old_settings,
    } = outcome;

    if !changed {
        return Ok(SetEmbeddingSettingsResponse {
            runtime: new_runtime,
            backup_path: None,
            restarted: false,
        });
    }

    let evacuate_mode = if req.evacuate_vectordb {
        EvacuateMode::Evacuate
    } else {
        EvacuateMode::Delete
    };

    state.invalidate_clients().await;
    state.sidecars.stop().await?;

    let backup_path = if needs_vectordb_reset {
        match evacuate_vectordb(&state.data, evacuate_mode) {
            Ok(p) => p,
            Err(e) => {
                // Restore settings file to old value so the failed swap
                // doesn't leave the on-disk config disagreeing with the
                // (still-old) lancedb dir on the next launch.
                let _ = save_embedding_settings(&path, &old_settings);
                // Restart the sidecar with the OLD config so the user is
                // not left with a stopped backend.
                let sidecars = state.sidecars.clone();
                let data = state.data.clone();
                crate::stage_and_start_sidecars(&app, &sidecars, &data).await;
                return Err(e);
            }
        }
    } else {
        None
    };

    // Try the new config; on failure roll back to the old settings + old
    // lancedb. The `start_with_warnings` Err path covers the cases we care
    // about (spawn failure, port pick, TCP health check timeout). A model-
    // download failure surfaces later through `get_model_status` and is
    // NOT caught here — the sidecar process IS up, just unable to load
    // the embedding model. That case is handled by the user re-opening
    // Settings.
    //
    // This path mirrors `stage_and_start_sidecars` (lib.rs) but keeps
    // control of the Err branch so the rollback can run. The plugin
    // staging warning is preserved so a regression that follows an
    // embedding swap still surfaces to the UI.
    let plugin_warnings = match crate::plugins::stage_plugins(&app, &state.data.plugins_dir()) {
        Ok(_) => Vec::new(),
        Err(e) => vec![crate::sidecar::SidecarWarning {
            kind: crate::sidecar::SidecarWarningKind::PluginsStageFailed,
            message: e.to_string(),
            detail: None,
        }],
    };
    match state.sidecars.start_with_warnings(plugin_warnings).await {
        Ok(report) => {
            let _ = app.emit("sidecar://ready", &report);
            Ok(SetEmbeddingSettingsResponse {
                runtime: new_runtime,
                backup_path,
                restarted: true,
            })
        }
        Err(e) => {
            tracing::warn!(error = %e, "sidecar failed to start with new embedding settings; rolling back");
            // Surface a typed payload so the frontend keeps a single
            // shape on `sidecar://error`. The rollback isn't a
            // structured startup-error (the *previous* settings are
            // about to come back up), so a Raw envelope is appropriate.
            super::emit_event(
                &app,
                "sidecar://error",
                crate::sidecar::startup_error::SidecarErrorPayload::Raw {
                    message: format!(
                        "embedding swap failed: {e}; rolling back to previous settings"
                    ),
                },
            );
            // 1. Stop whatever partial state the failed start may have left.
            let _ = state.sidecars.stop().await;
            // 2. Restore the settings file.
            let _ = save_embedding_settings(&path, &old_settings);
            // 3. Restore the lancedb backup if one was made.
            if let Some(backup) = backup_path.as_deref() {
                let _ = restore_vectordb_backup(&state.data, backup);
            }
            // 4. Restart with the old config. Best-effort; emit error if
            //    this also fails.
            let sidecars = state.sidecars.clone();
            let data = state.data.clone();
            crate::stage_and_start_sidecars(&app, &sidecars, &data).await;
            Err(AppError::Config(format!(
                "embedding model 適用に失敗しました。元の設定にロールバックしました: {e}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let s = load_embedding_settings(&dir.path().join("nope.json"));
        assert_eq!(s, EmbeddingSettings::default());
        assert!(s.preset_id.is_none());
    }

    #[test]
    fn load_corrupt_json_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embedding-settings.json");
        std::fs::write(&path, b"{corrupt").unwrap();
        assert_eq!(load_embedding_settings(&path), EmbeddingSettings::default());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("embedding-settings.json");
        let settings = EmbeddingSettings {
            preset_id: Some("qwen3-embedding-0-6b".into()),
            ..Default::default()
        };
        save_embedding_settings(&path, &settings).unwrap();
        assert_eq!(load_embedding_settings(&path), settings);
    }

    #[test]
    fn serde_fills_defaults_for_missing_fields() {
        let back: EmbeddingSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(back, EmbeddingSettings::default());
    }

    #[test]
    fn resolve_none_preset_returns_default_preset_values() {
        let s = EmbeddingSettings::default();
        let rt = resolve_embedding_runtime(&s);
        let default = embedding_presets::default_preset();
        assert_eq!(rt.model_id, default.hf_repo);
        assert_eq!(rt.vector_size, default.vector_size);
        assert_eq!(rt.is_multimodal, default.is_multimodal);
    }

    #[test]
    fn resolve_preset_id_returns_preset_values() {
        let s = EmbeddingSettings {
            preset_id: Some("qwen3-embedding-0-6b".into()),
            ..Default::default()
        };
        let rt = resolve_embedding_runtime(&s);
        let preset = embedding_presets::find_preset("qwen3-embedding-0-6b").unwrap();
        assert_eq!(rt.model_id, preset.hf_repo);
        assert_eq!(rt.vector_size, preset.vector_size);
        assert!(!rt.is_multimodal);
    }

    #[test]
    fn resolve_ruri_preset_emits_retrieval_prefix_env() {
        let settings = EmbeddingSettings {
            preset_id: Some("ruri-v3-310m-onnx-int8".into()),
            ..Default::default()
        };
        let runtime = resolve_embedding_runtime(&settings);
        assert_eq!(
            runtime.onnx_model_file.as_deref(),
            Some("onnx/model_int8.onnx")
        );
        assert_eq!(runtime.onnx_pooling.as_deref(), Some("ONNX_POOLING_MEAN"));
        let env: std::collections::HashMap<_, _> =
            resolve_embedding_env_vars_from_runtime(&runtime)
                .into_iter()
                .collect();
        assert_eq!(
            env.get("MEMORY_EMBEDDING_DOCUMENT_PREFIX")
                .map(String::as_str),
            Some("検索文書: ")
        );
        assert_eq!(
            env.get("MEMORY_EMBEDDING_QUERY_PREFIX").map(String::as_str),
            Some("検索クエリ: ")
        );
    }

    #[test]
    fn bundled_embedding_workflows_route_document_and_query_prefixes() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../workers/workflows");
        for relative in [
            "auto-embedding.yaml",
            "thread-reflection/auto-reflection-intent-embedding.yaml",
            "thread-reflection/auto-reflection-summary-embedding.yaml",
        ] {
            let yaml = std::fs::read_to_string(root.join(relative)).unwrap();
            assert!(
                yaml.contains("prefix: \"%{MEMORY_EMBEDDING_DOCUMENT_PREFIX:-}\""),
                "document prefix missing from {relative}"
            );
        }
        let rag = std::fs::read_to_string(root.join("rag/lookback-recall.yaml")).unwrap();
        assert!(rag.contains("prefix: \"%{MEMORY_EMBEDDING_QUERY_PREFIX:-}\""));
    }

    #[test]
    fn resolve_unknown_preset_id_falls_back_to_default() {
        // Defensive: if a future version retires a preset id but the
        // user's settings still reference it, the resolver must degrade
        // gracefully to the default rather than producing an empty
        // model_id (which would make the runner refuse to register).
        let s = EmbeddingSettings {
            preset_id: Some("retired-preset".into()),
            ..Default::default()
        };
        let rt = resolve_embedding_runtime(&s);
        assert_eq!(rt.model_id, embedding_presets::default_preset().hf_repo);
    }

    #[test]
    fn resolve_custom_returns_user_fields() {
        let s = EmbeddingSettings {
            preset_id: Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID.into()),
            custom_model_id: Some("intfloat/multilingual-e5-base".into()),
            custom_vector_size: Some(768),
            custom_dtype: Some("BF16".into()),
            custom_max_sequence_length: Some(512),
            custom_is_multimodal: Some(false),
            ..Default::default()
        };
        let rt = resolve_embedding_runtime(&s);
        assert_eq!(rt.model_id, "intfloat/multilingual-e5-base");
        assert_eq!(rt.vector_size, 768);
        assert_eq!(rt.dtype, "BF16");
        assert_eq!(rt.max_sequence_length, 512);
        assert!(!rt.is_multimodal);
    }

    #[test]
    fn resolve_custom_with_omitted_fields_falls_back_to_default_preset() {
        // Defensive: a custom selection with missing fields must NOT
        // produce zeros / empty strings, which would crash the runner.
        let s = EmbeddingSettings {
            preset_id: Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID.into()),
            custom_model_id: Some("intfloat/multilingual-e5-base".into()),
            ..Default::default()
        };
        let rt = resolve_embedding_runtime(&s);
        let default = embedding_presets::default_preset();
        assert_eq!(rt.vector_size, default.vector_size);
        assert_eq!(rt.dtype, default.dtype);
        assert_eq!(rt.max_sequence_length, default.max_sequence_length);
        // Custom always defaults to text-only when the flag is unset —
        // we cannot infer multimodality from a free-text HF repo.
        assert!(!rt.is_multimodal);
    }

    #[test]
    fn resolve_custom_empty_tokenizer_string_is_treated_as_none() {
        // The frontend may post `""` for the optional tokenizer field
        // when the user clears it. The resolver must collapse that to
        // `None` so the staged YAML renderer drops the line.
        let s = EmbeddingSettings {
            preset_id: Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID.into()),
            custom_model_id: Some("intfloat/multilingual-e5-base".into()),
            custom_tokenizer_id: Some("   ".into()),
            ..Default::default()
        };
        let rt = resolve_embedding_runtime(&s);
        assert!(rt.tokenizer_id.is_none());
    }

    #[test]
    fn resolve_env_vars_uses_preset_when_user_selected() {
        // Once the user has saved a preset, the dev shell env override is
        // ignored — file is authoritative. Mirror of the LLM-side
        // `resolve_local_llm_env_triple` pin.
        let s = EmbeddingSettings {
            preset_id: Some("qwen3-embedding-0-6b".into()),
            ..Default::default()
        };
        let preset = embedding_presets::find_preset("qwen3-embedding-0-6b").unwrap();
        let vars = resolve_embedding_env_vars(&s, |name| match name {
            "LOOKBACK_EMBEDDING_MODEL_ID" => Some("stale/override".into()),
            "LOOKBACK_EMBEDDING_VECTOR_SIZE" => Some("4096".into()),
            _ => None,
        });
        let map: std::collections::HashMap<&str, String> = vars.into_iter().collect();
        assert_eq!(
            map.get("LOOKBACK_EMBEDDING_MODEL_ID").map(String::as_str),
            Some(preset.hf_repo)
        );
        assert_eq!(
            map.get("LOOKBACK_EMBEDDING_VECTOR_SIZE")
                .map(String::as_str),
            Some(preset.vector_size.to_string().as_str())
        );
    }

    #[test]
    fn resolve_env_vars_honours_shell_env_when_user_has_not_picked() {
        let s = EmbeddingSettings::default();
        let vars = resolve_embedding_env_vars(&s, |name| match name {
            "LOOKBACK_EMBEDDING_MODEL_ID" => Some("dev/override".into()),
            "LOOKBACK_EMBEDDING_VECTOR_SIZE" => Some("4096".into()),
            _ => None,
        });
        let map: std::collections::HashMap<&str, String> = vars.into_iter().collect();
        assert_eq!(
            map.get("LOOKBACK_EMBEDDING_MODEL_ID").map(String::as_str),
            Some("dev/override")
        );
        assert_eq!(
            map.get("LOOKBACK_EMBEDDING_VECTOR_SIZE")
                .map(String::as_str),
            Some("4096")
        );
    }

    #[test]
    fn resolve_runtime_with_env_honours_shell_override_when_no_preset() {
        // Regression: lifecycle.rs used to build the runtime via
        // `resolve_embedding_runtime` (env-blind), so a `LOOKBACK_EMBEDDING_*`
        // override flowed into the process env but NOT into the staged
        // YAML / `MEMORY_VECTOR_SIZE` — the sidecar then opened LanceDB
        // at the default dimension while the runner emitted at the override's
        // dim. Calling the env-aware projection from the lifecycle fixes
        // that; this test pins it.
        let s = EmbeddingSettings::default();
        let rt = resolve_embedding_runtime_with_env(&s, |name| match name {
            "LOOKBACK_EMBEDDING_MODEL_ID" => Some("dev/override".into()),
            "LOOKBACK_EMBEDDING_VECTOR_SIZE" => Some("4096".into()),
            "LOOKBACK_EMBEDDING_DTYPE" => Some("BF16".into()),
            "LOOKBACK_EMBEDDING_MAX_SEQ_LEN" => Some("16384".into()),
            _ => None,
        });
        assert_eq!(rt.model_id, "dev/override");
        assert_eq!(rt.vector_size, 4096);
        assert_eq!(rt.dtype, "BF16");
        assert_eq!(rt.max_sequence_length, 16384);
        // is_multimodal is a property of the weights — cannot be
        // toggled via env, must stay with the preset's flag.
        assert_eq!(
            rt.is_multimodal,
            embedding_presets::default_preset().is_multimodal
        );
    }

    #[test]
    fn resolve_runtime_with_env_ignores_shell_when_preset_saved() {
        // Once the user has picked a preset, env overrides MUST NOT
        // override the saved choice. Mirror of the env_vars test pin.
        let s = EmbeddingSettings {
            preset_id: Some("qwen3-embedding-0-6b".into()),
            ..Default::default()
        };
        let preset = embedding_presets::find_preset("qwen3-embedding-0-6b").unwrap();
        let rt = resolve_embedding_runtime_with_env(&s, |name| match name {
            "LOOKBACK_EMBEDDING_MODEL_ID" => Some("stale/override".into()),
            "LOOKBACK_EMBEDDING_VECTOR_SIZE" => Some("99999".into()),
            _ => None,
        });
        assert_eq!(rt.model_id, preset.hf_repo);
        assert_eq!(rt.vector_size, preset.vector_size);
    }

    #[test]
    fn resolve_runtime_env_blind_alias_returns_preset_defaults() {
        // The UI-facing `resolve_embedding_runtime` shim must ignore env
        // overrides so the Settings card always shows what the SAVED
        // settings would resolve to — independent of dev shell state.
        // SAFETY: --test-threads=1.
        unsafe { std::env::set_var("LOOKBACK_EMBEDDING_MODEL_ID", "ghost/should-not-leak") };
        let s = EmbeddingSettings::default();
        let rt = resolve_embedding_runtime(&s);
        unsafe { std::env::remove_var("LOOKBACK_EMBEDDING_MODEL_ID") };
        assert_eq!(rt.model_id, embedding_presets::default_preset().hf_repo);
    }

    #[test]
    fn resolve_env_vars_omits_tokenizer_when_preset_has_none() {
        // Every shipped preset has `tokenizer_hf_repo: None`. The env
        // var must NOT be emitted in that case (the YAML renderer
        // interprets a missing var as "drop the line").
        let s = EmbeddingSettings::default();
        let vars = resolve_embedding_env_vars(&s, |_| None);
        let map: std::collections::HashMap<&str, String> = vars.into_iter().collect();
        assert!(!map.contains_key("LOOKBACK_EMBEDDING_TOKENIZER_ID"));
    }

    #[test]
    fn validate_accepts_preset_selection() {
        let req = SetEmbeddingSettingsRequest {
            preset_id: Some("qwen3-embedding-0-6b".into()),
            custom_model_id: None,
            custom_tokenizer_id: None,
            custom_vector_size: None,
            custom_dtype: None,
            custom_max_sequence_length: None,
            custom_is_multimodal: None,
            evacuate_vectordb: true,
        };
        assert!(validate_set_request(&req).is_ok());
    }

    #[test]
    fn validate_rejects_unknown_preset_id() {
        let req = SetEmbeddingSettingsRequest {
            preset_id: Some("does-not-exist".into()),
            custom_model_id: None,
            custom_tokenizer_id: None,
            custom_vector_size: None,
            custom_dtype: None,
            custom_max_sequence_length: None,
            custom_is_multimodal: None,
            evacuate_vectordb: true,
        };
        assert!(validate_set_request(&req).is_err());
    }

    fn custom_req(
        model: Option<&str>,
        tokenizer: Option<&str>,
        vector_size: Option<u32>,
        dtype: Option<&str>,
        max_seq: Option<u32>,
    ) -> SetEmbeddingSettingsRequest {
        SetEmbeddingSettingsRequest {
            preset_id: Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID.into()),
            custom_model_id: model.map(str::to_string),
            custom_tokenizer_id: tokenizer.map(str::to_string),
            custom_vector_size: vector_size,
            custom_dtype: dtype.map(str::to_string),
            custom_max_sequence_length: max_seq,
            custom_is_multimodal: None,
            evacuate_vectordb: true,
        }
    }

    #[test]
    fn validate_custom_accepts_minimal() {
        // Minimal valid custom request: model_id + vector_size (both
        // required because there is no sane default for either).
        let req = custom_req(Some("org/name"), None, Some(1024), None, None);
        assert!(validate_set_request(&req).is_ok());
    }

    #[test]
    fn validate_custom_rejects_missing_model_id() {
        let req = custom_req(None, None, Some(1024), None, None);
        assert!(validate_set_request(&req).is_err());
    }

    #[test]
    fn validate_custom_rejects_missing_vector_size() {
        // Regression: omitting `custom_vector_size` previously silently
        // resolved to the default preset's dim. memories then opened
        // LanceDB with that dim while the user's actual model emitted a
        // different dim, so every embedding upsert failed. The validator
        // must reject this up front.
        let req = custom_req(Some("org/name"), None, None, None, None);
        assert!(validate_set_request(&req).is_err());
    }

    #[test]
    fn validate_custom_rejects_invalid_hf_repo_shape() {
        let req = custom_req(Some("not a repo"), None, Some(1024), None, None);
        assert!(validate_set_request(&req).is_err());
    }

    #[test]
    fn validate_custom_rejects_vector_size_out_of_range() {
        let req = custom_req(Some("org/name"), None, Some(0), None, None);
        assert!(validate_set_request(&req).is_err());
        let req = custom_req(Some("org/name"), None, Some(99_999), None, None);
        assert!(validate_set_request(&req).is_err());
    }

    #[test]
    fn validate_custom_rejects_unsupported_dtype() {
        let req = custom_req(Some("org/name"), None, Some(1024), Some("INT8"), None);
        assert!(validate_set_request(&req).is_err());
    }

    #[test]
    fn validate_custom_accepts_known_dtypes() {
        for dtype in ["F16", "BF16", "F32"] {
            let req = custom_req(Some("org/name"), None, Some(1024), Some(dtype), None);
            assert!(
                validate_set_request(&req).is_ok(),
                "dtype {dtype} should be accepted"
            );
        }
    }

    #[test]
    fn validate_custom_rejects_max_seq_len_out_of_range() {
        let req = custom_req(Some("org/name"), None, Some(1024), None, Some(0));
        assert!(validate_set_request(&req).is_err());
        let req = custom_req(Some("org/name"), None, Some(1024), None, Some(10_000_000));
        assert!(validate_set_request(&req).is_err());
    }

    #[test]
    fn is_valid_hf_repo_accepts_canonical_shapes() {
        assert!(is_valid_hf_repo("Qwen/Qwen3-VL-Embedding-2B"));
        assert!(is_valid_hf_repo("cl-nagoya/ruri-v3-310m"));
        assert!(is_valid_hf_repo("a/b"));
    }

    #[test]
    fn is_valid_hf_repo_rejects_malformed() {
        assert!(!is_valid_hf_repo(""));
        assert!(!is_valid_hf_repo("no-slash"));
        assert!(!is_valid_hf_repo("/leading"));
        assert!(!is_valid_hf_repo("trailing/"));
        assert!(!is_valid_hf_repo("two/slashes/here"));
        assert!(!is_valid_hf_repo("space in/name"));
    }

    // ── evacuate_vectordb ─────────────────────────────────────────────

    fn data_paths_in(tmp: &Path) -> DataPaths {
        DataPaths::with_root(tmp.to_path_buf())
    }

    #[test]
    fn evacuate_vectordb_noop_when_source_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        // Don't create lancedb/ — simulate fresh install before first
        // import.
        assert!(
            evacuate_vectordb(&data, EvacuateMode::Evacuate)
                .unwrap()
                .is_none()
        );
        assert!(
            evacuate_vectordb(&data, EvacuateMode::Delete)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn evacuate_vectordb_renames_existing_dir_in_evacuate_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        std::fs::create_dir_all(data.lancedb_dir()).unwrap();
        std::fs::write(data.lancedb_dir().join("marker"), b"x").unwrap();

        let backup = evacuate_vectordb(&data, EvacuateMode::Evacuate)
            .unwrap()
            .expect("backup path returned");

        assert!(backup.exists(), "backup dir exists");
        assert!(backup.join("marker").exists(), "marker preserved");
        assert!(data.lancedb_dir().exists(), "source recreated empty");
        let leftover: Vec<_> = std::fs::read_dir(data.lancedb_dir()).unwrap().collect();
        assert!(leftover.is_empty(), "source must be empty after move");
    }

    #[test]
    fn evacuate_vectordb_deletes_in_delete_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        std::fs::create_dir_all(data.lancedb_dir().join("nested")).unwrap();
        std::fs::write(data.lancedb_dir().join("marker"), b"x").unwrap();

        let backup = evacuate_vectordb(&data, EvacuateMode::Delete).unwrap();
        assert!(backup.is_none(), "delete mode returns no backup path");
        assert!(data.lancedb_dir().exists(), "source recreated");
        let leftover: Vec<_> = std::fs::read_dir(data.lancedb_dir()).unwrap().collect();
        assert!(leftover.is_empty(), "source must be empty after delete");
    }

    #[test]
    fn evacuate_vectordb_two_calls_produce_distinct_backup_paths() {
        // The nanos suffix protects against a same-second double-save
        // collision. Without it the second rename would clobber the
        // first backup's marker.
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());

        std::fs::create_dir_all(data.lancedb_dir()).unwrap();
        std::fs::write(data.lancedb_dir().join("marker1"), b"1").unwrap();
        let b1 = evacuate_vectordb(&data, EvacuateMode::Evacuate)
            .unwrap()
            .unwrap();
        std::fs::write(data.lancedb_dir().join("marker2"), b"2").unwrap();
        let b2 = evacuate_vectordb(&data, EvacuateMode::Evacuate)
            .unwrap()
            .unwrap();

        assert_ne!(b1, b2, "backup paths must be unique");
        assert!(b1.join("marker1").exists());
        assert!(b2.join("marker2").exists());
    }

    #[test]
    fn restore_vectordb_backup_renames_backup_back_to_source() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        std::fs::create_dir_all(data.lancedb_dir()).unwrap();
        std::fs::write(data.lancedb_dir().join("marker"), b"x").unwrap();
        let backup = evacuate_vectordb(&data, EvacuateMode::Evacuate)
            .unwrap()
            .unwrap();

        // Source is empty after evacuation; simulate a failed restart
        // that has just been aborted by stop().
        assert!(restore_vectordb_backup(&data, &backup).unwrap());
        assert!(data.lancedb_dir().join("marker").exists());
        assert!(!backup.exists(), "backup is moved, not copied");
    }

    #[test]
    fn restore_vectordb_backup_returns_false_when_backup_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        std::fs::create_dir_all(data.lancedb_dir()).unwrap();
        let phantom = data.lancedb_backup_dir().join("does-not-exist");
        assert!(!restore_vectordb_backup(&data, &phantom).unwrap());
    }

    // ── needs_vectordb_reset + env-aware old runtime ────────────────────

    #[test]
    fn needs_vectordb_reset_true_when_vector_size_changes() {
        let mut old = runtime_from_preset(embedding_presets::default_preset());
        let new = old.clone();
        old.vector_size += 1;
        assert!(needs_vectordb_reset(&old, &new));
    }

    #[test]
    fn needs_vectordb_reset_true_when_model_id_changes() {
        let mut old = runtime_from_preset(embedding_presets::default_preset());
        let new = old.clone();
        old.model_id = "other/model".into();
        assert!(needs_vectordb_reset(&old, &new));
    }

    #[test]
    fn needs_vectordb_reset_false_when_only_dtype_or_max_seq_changes() {
        let old = runtime_from_preset(embedding_presets::default_preset());
        let new = EmbeddingRuntime {
            dtype: "BF16".into(),
            max_sequence_length: old.max_sequence_length + 1024,
            ..old.clone()
        };
        // Dtype / max_seq tweaks alone do not invalidate the LanceDB,
        // so the user's saved data must NOT be evacuated.
        assert!(!needs_vectordb_reset(&old, &new));
    }

    #[test]
    fn env_aware_old_runtime_triggers_reset_against_envless_default_save() {
        // Regression for the headline review:
        //   - sidecar runs with `LOOKBACK_EMBEDDING_VECTOR_SIZE=2048`
        //   - LanceDB exists at 2048 dim
        //   - user saves the default preset (preset_id: default)
        //
        // If we compared an env-BLIND old_runtime (which returns the
        // default preset's 1024) against the new default preset's 1024,
        // `needs_vectordb_reset` would be `false` and the next start
        // would crash trying to open the 2048-dim LanceDB at 1024.
        //
        // Asserting via the same env-aware resolver the lifecycle uses:
        // the env override must surface in old_runtime so the comparison
        // correctly detects the dimension change.
        let old_settings = EmbeddingSettings::default();
        let env_lookup = |name: &str| -> Option<String> {
            match name {
                "LOOKBACK_EMBEDDING_VECTOR_SIZE" => Some("2048".to_string()),
                _ => None,
            }
        };
        let old_runtime = resolve_embedding_runtime_with_env(&old_settings, env_lookup);
        let new_settings = EmbeddingSettings {
            preset_id: Some(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID.into()),
            ..Default::default()
        };
        let new_runtime = resolve_embedding_runtime_with_env(&new_settings, env_lookup);

        assert_eq!(old_runtime.vector_size, 2048, "env override took effect");
        assert_eq!(
            new_runtime.vector_size,
            embedding_presets::default_preset().vector_size,
            "saved preset wins over env"
        );
        assert!(
            needs_vectordb_reset(&old_runtime, &new_runtime),
            "evacuation MUST be required when env-driven old dim != new saved dim"
        );
    }

    // ── apply_embedding_settings_to_disk ──────────────────────────────

    fn preset_req(preset_id: &str) -> SetEmbeddingSettingsRequest {
        SetEmbeddingSettingsRequest {
            preset_id: Some(preset_id.to_string()),
            custom_model_id: None,
            custom_tokenizer_id: None,
            custom_vector_size: None,
            custom_dtype: None,
            custom_max_sequence_length: None,
            custom_is_multimodal: None,
            evacuate_vectordb: true,
        }
    }

    #[test]
    fn apply_embedding_to_disk_noop_when_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        // Persist the default preset, then re-apply the same value.
        let def = embedding_presets::DEFAULT_EMBEDDING_PRESET_ID;
        apply_embedding_settings_to_disk(&data, &preset_req(def)).unwrap();
        let outcome = apply_embedding_settings_to_disk(&data, &preset_req(def)).unwrap();
        assert!(!outcome.changed, "re-applying the same preset is a no-op");
        assert!(!outcome.needs_vectordb_reset);
    }

    #[test]
    fn apply_embedding_to_disk_detects_dimension_change() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        // Start at the default 0.6B preset, switch to the VL-2B multimodal
        // preset (2048 dim) — a dimension change must request a
        // vectordb reset and persist the new preset.
        apply_embedding_settings_to_disk(
            &data,
            &preset_req(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID),
        )
        .unwrap();
        let outcome =
            apply_embedding_settings_to_disk(&data, &preset_req("qwen3-vl-embedding-2b")).unwrap();
        assert!(outcome.changed);
        assert!(
            outcome.needs_vectordb_reset,
            "switching to a different-dim preset must require a reset"
        );
        let saved = load_embedding_settings(&data.embedding_settings_path());
        assert_eq!(saved.preset_id.as_deref(), Some("qwen3-vl-embedding-2b"));
    }

    #[test]
    fn apply_embedding_to_disk_allows_remote_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        // Remote browse mode still needs the local embedding model setting so
        // semantic query vectors can match the remote memories vector space.
        super::super::connection::save_connection_config(
            &data.connection_config_path(),
            &super::super::connection::ConnectionConfig {
                mode: super::super::connection::ConnectionMode::Remote,
                remote_jobworkerp_url: Some("http://h:9000".into()),
                remote_memories_url: Some("http://h:9010".into()),
            },
        )
        .unwrap();
        let outcome =
            apply_embedding_settings_to_disk(&data, &preset_req("qwen3-vl-embedding-2b")).unwrap();
        assert!(outcome.changed);
        assert!(
            !outcome.needs_vectordb_reset,
            "remote mode must not reset the local vectordb"
        );
        let saved = load_embedding_settings(&data.embedding_settings_path());
        assert_eq!(saved.preset_id.as_deref(), Some("qwen3-vl-embedding-2b"));
    }
}
