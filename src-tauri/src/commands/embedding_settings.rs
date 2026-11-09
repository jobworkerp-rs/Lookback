//! Embedding worker configuration and application-settings rollback.
//!
//! Storage migrations belong to memories. Ordinary local settings saves reject
//! embedding-space changes; only a separately CLI-approved reconcile/adopt
//! permit may authorize persistence without a rebuild.

use std::path::{Path, PathBuf};

use crate::data::DataPaths;
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};

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
/// Canonical values accepted by memories' `DistanceType` vocabulary.
const SUPPORTED_DISTANCE_TYPES: &[&str] = &["cosine", "l2", "dot"];
const DEFAULT_DISTANCE_TYPE: &str = "cosine";
const UNVERSIONED_MODEL_REVISION: &str = "unversioned";

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
    "MEMORY_EMBEDDING_MODEL_REVISION",
    "MEMORY_DISTANCE_TYPE",
    "MEMORY_EMBEDDING_DOCUMENT_PREFIX",
    "MEMORY_EMBEDDING_QUERY_PREFIX",
];

/// Persisted (non-secret) embedding-model config.
///
/// All fields are `Option<…>` so settings written before a field was added
/// deserialize unchanged. Missing model revision stays undeclared and
/// resolves to `unversioned`; the resolver does not infer a revision from
/// preset/model names or the application release.
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
    /// Explicit user declaration only. Presets do not imply a pinned revision.
    #[serde(default)]
    pub model_revision: Option<String>,
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
    pub model_revision: Option<String>,
    /// Resolved runtime — the values the sidecar will actually use on the
    /// next restart. Saves the UI a second roundtrip for "what's
    /// effective right now".
    pub effective: EmbeddingRuntime,
    /// `true` when `connection.json` is in remote mode; the UI uses this
    /// to disable inputs and show the warning banner.
    pub connection_remote: bool,
}

/// Embedding worker configuration submitted by the frontend.
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
    /// Optional explicit revision; never manufactured from preset, repository,
    /// or application version metadata.
    #[serde(default)]
    pub model_revision: Option<String>,
}

/// Result of saving application-owned embedding configuration.
#[derive(Debug, Clone, Serialize)]
pub struct SetEmbeddingSettingsResponse {
    pub runtime: EmbeddingRuntime,
    /// Retained for response compatibility; the app no longer creates storage backups.
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
    /// Explicit revision declaration, or `unversioned` when no declaration
    /// exists. This pass-through does not imply a runner-level pinning guarantee.
    pub model_revision: String,
    /// Effective memories distance type (`cosine`, `l2`, or `dot`).
    pub distance_type: String,
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
        model_revision: UNVERSIONED_MODEL_REVISION.to_string(),
        distance_type: DEFAULT_DISTANCE_TYPE.to_string(),
    }
}

/// Runner settings for an embedding space reported by Memories, which only
/// names the model and dimension. The local settings win when they describe
/// the same space so custom dtype or ONNX choices are kept; otherwise a
/// curated preset fills in the rest. `None` means the space cannot be run
/// from what this app knows.
pub fn runtime_for_embedding_space(
    model_id: &str,
    dimension: u32,
    local: &EmbeddingRuntime,
) -> Option<EmbeddingRuntime> {
    if local.model_id == model_id && local.vector_size == dimension {
        return Some(local.clone());
    }
    embedding_presets::find_preset_by_model(model_id, dimension).map(runtime_from_preset)
}

fn valid_revision_value(revision: &str) -> bool {
    !revision.trim().is_empty() && !revision.chars().any(char::is_control)
}

fn resolve_model_revision<F>(settings: &EmbeddingSettings, env_lookup: &F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(revision) = settings
        .model_revision
        .as_deref()
        .filter(|revision| valid_revision_value(revision))
    {
        return revision.trim().to_string();
    }
    env_lookup("MEMORY_EMBEDDING_MODEL_REVISION")
        .filter(|revision| valid_revision_value(revision))
        .map(|revision| revision.trim().to_string())
        .unwrap_or_else(|| UNVERSIONED_MODEL_REVISION.to_string())
}

fn resolve_distance_type<F>(env_lookup: &F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    env_lookup("MEMORY_DISTANCE_TYPE")
        .filter(|distance| SUPPORTED_DISTANCE_TYPES.contains(&distance.as_str()))
        .unwrap_or_else(|| DEFAULT_DISTANCE_TYPE.to_string())
}

fn apply_memories_identity<F>(
    mut runtime: EmbeddingRuntime,
    settings: &EmbeddingSettings,
    env_lookup: &F,
) -> EmbeddingRuntime
where
    F: Fn(&str) -> Option<String>,
{
    runtime.model_revision = resolve_model_revision(settings, env_lookup);
    runtime.distance_type = resolve_distance_type(env_lookup);
    runtime.document_prefix = env_lookup("MEMORY_EMBEDDING_DOCUMENT_PREFIX")
        .filter(|prefix| !prefix.is_empty())
        .or(runtime.document_prefix);
    runtime.query_prefix = env_lookup("MEMORY_EMBEDDING_QUERY_PREFIX")
        .filter(|prefix| !prefix.is_empty())
        .or(runtime.query_prefix);
    runtime
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
/// overrides at all (e.g. the saved-value Settings display).
///
/// Unknown / retired preset ids degrade gracefully to the default preset
/// rather than producing an empty `model_id` (which would make the
/// embedding runner refuse to register at startup). An explicit stored/env
/// revision is still passed through; otherwise runtime identity is
/// `unversioned`, not a revision inferred from the fallback preset.
pub fn resolve_embedding_runtime_with_env<F>(
    settings: &EmbeddingSettings,
    env_lookup: F,
) -> EmbeddingRuntime
where
    F: Fn(&str) -> Option<String>,
{
    if settings.preset_id.as_deref() == Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID) {
        let default = runtime_from_preset(embedding_presets::default_preset());
        let runtime = EmbeddingRuntime {
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
            model_revision: UNVERSIONED_MODEL_REVISION.to_string(),
            distance_type: DEFAULT_DISTANCE_TYPE.to_string(),
        };
        return apply_memories_identity(runtime, settings, &env_lookup);
    }
    let preset_runtime = runtime_from_preset(
        settings
            .preset_id
            .as_deref()
            .and_then(embedding_presets::find_preset)
            .unwrap_or_else(embedding_presets::default_preset),
    );
    if settings.preset_id.is_some() {
        return apply_memories_identity(preset_runtime, settings, &env_lookup);
    }
    // No saved preset → honour the dev shell env overrides. Anything
    // unset falls through to the default preset values; preserving the
    // preset's `is_multimodal` flag because that's a property of the
    // weights, not something a shell var can sensibly toggle.
    let runtime = EmbeddingRuntime {
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
        model_revision: UNVERSIONED_MODEL_REVISION.to_string(),
        distance_type: DEFAULT_DISTANCE_TYPE.to_string(),
    };
    apply_memories_identity(runtime, settings, &env_lookup)
}

/// Pure projection that ignores any environment overrides. Useful for callers
/// that intentionally want only the persisted settings and preset defaults;
/// runtime status paths should use `resolve_embedding_runtime_with_env` with
/// the Sidecars external-environment snapshot instead.
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
    out.push((
        "MEMORY_EMBEDDING_MODEL_REVISION",
        runtime.model_revision.clone(),
    ));
    out.push(("MEMORY_DISTANCE_TYPE", runtime.distance_type.clone()));
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

/// Purely project a request into the persisted shape. Callers must validate
/// the request first; this function performs no I/O and invents no revision.
pub fn settings_from_request(req: &SetEmbeddingSettingsRequest) -> EmbeddingSettings {
    EmbeddingSettings {
        preset_id: req.preset_id.clone(),
        custom_model_id: req.custom_model_id.clone(),
        custom_tokenizer_id: req.custom_tokenizer_id.clone(),
        custom_vector_size: req.custom_vector_size,
        custom_dtype: req.custom_dtype.clone(),
        custom_max_sequence_length: req.custom_max_sequence_length,
        custom_is_multimodal: req.custom_is_multimodal,
        model_revision: req
            .model_revision
            .as_deref()
            .map(str::trim)
            .map(str::to_string),
    }
}

/// Resolve a validated request into the runtime candidate without persisting
/// anything. CLI migration planning uses this exact projection so the
/// preflight identity and subsequent settings apply cannot disagree.
pub fn prepare_embedding_candidate_runtime<F>(
    req: &SetEmbeddingSettingsRequest,
    env_lookup: F,
) -> AppResult<EmbeddingRuntime>
where
    F: Fn(&str) -> Option<String>,
{
    validate_set_request(req).map_err(AppError::Config)?;
    Ok(resolve_embedding_runtime_with_env(
        &settings_from_request(req),
        env_lookup,
    ))
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
    if let Some(revision) = req.model_revision.as_deref()
        && !valid_revision_value(revision)
    {
        return Err(
            "invalid model_revision: must be non-empty and contain no control characters".into(),
        );
    }
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

/// Whether the vector identity changes. An omitted tokenizer is the empty
/// identity component and intentionally differs from explicitly naming the
/// model repo as tokenizer. Dtype and max-sequence tweaks do not alter vector
/// identity; pending chunk configuration is checked separately below.
pub fn embedding_space_changed(old: &EmbeddingRuntime, new: &EmbeddingRuntime) -> bool {
    old.model_id != new.model_id
        || old.tokenizer_id.as_deref().unwrap_or("") != new.tokenizer_id.as_deref().unwrap_or("")
        || old.model_revision != new.model_revision
        || old.vector_size != new.vector_size
        || old.distance_type != new.distance_type
}

/// The staged workers YAML maps `max_sequence_length` directly to
/// `chunking_config.max_chunk_tokens`. Changing it can affect pending chunk
/// inputs; dtype-only changes do not alter that configuration.
pub fn pending_chunk_configuration_changed(old: &EmbeddingRuntime, new: &EmbeddingRuntime) -> bool {
    old.max_sequence_length != new.max_sequence_length
}

pub fn validate_pending_chunk_configuration_change(
    old: &EmbeddingRuntime,
    new: &EmbeddingRuntime,
    rebuild_pending: bool,
) -> AppResult<()> {
    if rebuild_pending && pending_chunk_configuration_changed(old, new) {
        return Err(AppError::Config(
            "max sequence length cannot change while an embedding rebuild is pending".into(),
        ));
    }
    Ok(())
}

// ── Tauri commands ───────────────────────────────────────────────────

#[tauri::command]
pub fn get_embedding_settings(
    state: tauri::State<'_, super::AppState>,
) -> AppResult<EmbeddingSettingsResponse> {
    let settings = load_embedding_settings(&state.data.embedding_settings_path());
    let connection_remote =
        !super::connection::load_connection_config(&state.data.connection_config_path())
            .memories
            .is_local();
    let effective = resolve_embedding_runtime_with_env(&settings, |key| {
        state.sidecars.embedding_env_lookup(key)
    });
    Ok(EmbeddingSettingsResponse {
        preset_id: settings.preset_id,
        custom_model_id: settings.custom_model_id,
        custom_tokenizer_id: settings.custom_tokenizer_id,
        custom_vector_size: settings.custom_vector_size,
        custom_dtype: settings.custom_dtype,
        custom_max_sequence_length: settings.custom_max_sequence_length,
        custom_is_multimodal: settings.custom_is_multimodal,
        model_revision: settings.model_revision,
        effective,
        connection_remote,
    })
}

/// Outcome of persisting embedding settings to disk WITHOUT restarting
/// the sidecar. Carries everything the caller needs for a configuration
/// restart, and rollback. Returned by [`apply_embedding_settings_to_disk`].
pub struct EmbeddingApplyOutcome {
    pub new_runtime: EmbeddingRuntime,
    /// `false` ⇒ no-op (old == new); the caller should skip the restart.
    pub changed: bool,
    /// Pre-save settings, retained so the caller can roll back the file.
    pub old_settings: EmbeddingSettings,
}

/// CLI-approved local settings outcomes that may persist a changed embedding
/// identity. `Reembed` is intentionally absent: approved rebuilds must use
/// their dedicated save/rollback transaction, not this generic helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingPlanPermit {
    NoChange,
    Reconcile,
    Adopt,
}

/// Validate an embedding request WITHOUT persisting. Split out so the unified
/// `apply_settings` can validate the whole batch up front (a later card's
/// failure must not leave embedding-settings.json half-saved).
pub fn validate_embedding_request(req: &SetEmbeddingSettingsRequest) -> AppResult<()> {
    validate_set_request(req).map_err(AppError::Config)?;
    Ok(())
}

pub(crate) fn validate_embedding_settings_pending_change_with_env<F>(
    data: &DataPaths,
    req: &SetEmbeddingSettingsRequest,
    rebuild_pending: bool,
    env_lookup: F,
) -> AppResult<()>
where
    F: Fn(&str) -> Option<String>,
{
    validate_embedding_request(req)?;
    let old = load_embedding_settings(&data.embedding_settings_path());
    let new = settings_from_request(req);
    let old_runtime = resolve_embedding_runtime_with_env(&old, &env_lookup);
    let new_runtime = resolve_embedding_runtime_with_env(&new, &env_lookup);
    validate_pending_chunk_configuration_change(&old_runtime, &new_runtime, rebuild_pending)
}

/// Validate and persist `embedding-settings.json` without changing service
/// storage or restarting the sidecar. The caller
/// (the individual command or the unified `apply_settings`) owns the
/// stop → restart → configuration rollback sequence so several settings can
/// share a single restart.
///
/// When `changed == false` the file is left untouched (it equals the old
/// value anyway) and the caller should not restart.
pub fn apply_embedding_settings_to_disk(
    data: &DataPaths,
    req: &SetEmbeddingSettingsRequest,
) -> AppResult<EmbeddingApplyOutcome> {
    apply_embedding_settings_to_disk_with_permit(data, req, None, false)
}

/// Persist settings only after the Memories CLI has explicitly approved one
/// of the non-rebuild plans. The typed permit deliberately has no `Reembed`
/// variant; rebuild persistence belongs to the migration transaction's own
/// save/rollback path.
pub fn apply_embedding_settings_to_disk_approved(
    data: &DataPaths,
    req: &SetEmbeddingSettingsRequest,
    permit: EmbeddingPlanPermit,
) -> AppResult<EmbeddingApplyOutcome> {
    apply_embedding_settings_to_disk_with_permit(data, req, Some(permit), false)
}

fn apply_embedding_settings_to_disk_with_permit(
    data: &DataPaths,
    req: &SetEmbeddingSettingsRequest,
    permit: Option<EmbeddingPlanPermit>,
    rebuild_pending: bool,
) -> AppResult<EmbeddingApplyOutcome> {
    apply_embedding_settings_to_disk_with_env(
        data,
        req,
        permit,
        rebuild_pending,
        super::process_env_lookup,
    )
}

pub(crate) fn apply_embedding_settings_to_disk_with_env<F>(
    data: &DataPaths,
    req: &SetEmbeddingSettingsRequest,
    permit: Option<EmbeddingPlanPermit>,
    rebuild_pending: bool,
    env_lookup: F,
) -> AppResult<EmbeddingApplyOutcome>
where
    F: Fn(&str) -> Option<String>,
{
    validate_embedding_request(req)?;

    let path = data.embedding_settings_path();
    let old_settings = load_embedding_settings(&path);
    let connection_remote =
        !super::connection::load_connection_config(&data.connection_config_path())
            .memories
            .is_local();
    // Compare the same effective configuration that the child actually uses;
    // environment overrides must not bypass the embedding-space guard.
    let old_runtime = resolve_embedding_runtime_with_env(&old_settings, &env_lookup);

    let new_settings = settings_from_request(req);
    let new_runtime = resolve_embedding_runtime_with_env(&new_settings, &env_lookup);

    validate_pending_chunk_configuration_change(&old_runtime, &new_runtime, rebuild_pending)?;

    if old_runtime == new_runtime && old_settings == new_settings {
        return Ok(EmbeddingApplyOutcome {
            new_runtime,
            changed: false,
            old_settings,
        });
    }

    // The ordinary Settings/apply-settings path has no migration proof and
    // therefore remains conservative. Only a CLI-approved reconcile/adopt
    // decision can authorize changing local vector identity here.
    let permit_authorizes_local_identity_change = matches!(
        permit,
        Some(EmbeddingPlanPermit::Reconcile | EmbeddingPlanPermit::Adopt)
    );
    if !connection_remote
        && embedding_space_changed(&old_runtime, &new_runtime)
        && !permit_authorizes_local_identity_change
    {
        return Err(AppError::Config(
            "embedding model, tokenizer, revision, dimension, or distance change requires a Memories-approved migration plan; settings and data were not changed".into(),
        ));
    }
    save_embedding_settings(&path, &new_settings)?;
    Ok(EmbeddingApplyOutcome {
        new_runtime,
        changed: true,
        old_settings,
    })
}

#[tauri::command]
pub async fn set_embedding_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, super::AppState>,
    req: SetEmbeddingSettingsRequest,
) -> AppResult<SetEmbeddingSettingsResponse> {
    let response = super::apply_settings::apply_settings(
        app,
        state,
        super::apply_settings::ApplySettingsRequest {
            llm: None,
            embedding: Some(req),
            hf_home: None,
            mcp: None,
            timezone: None,
            issue_tracker: None,
            migration_backup: None,
        },
    )
    .await?;
    let runtime = response.embedding_runtime.ok_or_else(|| {
        AppError::Config("embedding settings apply did not return its effective runtime".into())
    })?;
    Ok(SetEmbeddingSettingsResponse {
        runtime,
        backup_path: None,
        restarted: response.restarted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_runtime_prefers_matching_local_settings() {
        let mut local =
            runtime_from_preset(embedding_presets::find_preset("qwen3-vl-embedding-2b").unwrap());
        local.dtype = "BF16".into();
        let runtime = runtime_for_embedding_space("Qwen/Qwen3-VL-Embedding-2B", 2048, &local)
            .expect("matching local settings identify the runtime");
        assert_eq!(runtime, local);
    }

    #[test]
    fn space_runtime_falls_back_to_the_matching_preset() {
        let local = runtime_from_preset(embedding_presets::default_preset());
        let runtime = runtime_for_embedding_space("sirasagi62/ruri-v3-310m-ONNX", 768, &local)
            .expect("a preset identifies the runtime");
        assert_eq!(runtime.model_id, "sirasagi62/ruri-v3-310m-ONNX");
        assert_eq!(runtime.vector_size, 768);
        assert_eq!(
            runtime.onnx_model_file.as_deref(),
            Some("onnx/model_int8.onnx")
        );
        assert_eq!(runtime.query_prefix.as_deref(), Some("検索クエリ: "));
    }

    #[test]
    fn space_runtime_is_unknown_without_a_matching_source() {
        let local = runtime_from_preset(embedding_presets::default_preset());
        assert!(runtime_for_embedding_space("example/unknown-embedding", 1024, &local).is_none());
        // Same repo but another dimension is a different space.
        assert!(runtime_for_embedding_space("Qwen/Qwen3-Embedding-0.6B", 768, &local).is_none());
    }

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
            model_revision: Some("stable-v2".into()),
            ..Default::default()
        };
        save_embedding_settings(&path, &settings).unwrap();
        assert_eq!(load_embedding_settings(&path), settings);
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains("\"model_revision\": \"stable-v2\""));
    }

    #[test]
    fn serde_fills_defaults_for_missing_fields() {
        let back: EmbeddingSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(back, EmbeddingSettings::default());
        assert_eq!(back.model_revision, None);
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
    fn bundled_embedding_workflows_preserve_offsets_and_route_query_prefixes() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../workers/workflows");
        for relative in [
            "auto-embedding.yaml",
            "thread-reflection/auto-reflection-intent-embedding.yaml",
            "thread-reflection/auto-reflection-summary-embedding.yaml",
        ] {
            let yaml = std::fs::read_to_string(root.join(relative)).unwrap();
            let workflow: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
            fn assert_no_document_prefix(value: &serde_yaml::Value, relative: &str) {
                match value {
                    serde_yaml::Value::Mapping(mapping) => {
                        assert!(
                            !mapping.contains_key(serde_yaml::Value::String("prefix".into())),
                            "document prefix would invalidate chunk offsets in {relative}"
                        );
                        for child in mapping.values() {
                            assert_no_document_prefix(child, relative);
                        }
                    }
                    serde_yaml::Value::Sequence(sequence) => {
                        for child in sequence {
                            assert_no_document_prefix(child, relative);
                        }
                    }
                    _ => {}
                }
            }
            assert_no_document_prefix(&workflow, relative);
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
    fn model_revision_is_explicit_and_legacy_settings_resolve_as_unversioned() {
        let legacy = EmbeddingSettings {
            preset_id: Some(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID.into()),
            ..Default::default()
        };
        let runtime = resolve_embedding_runtime(&legacy);
        assert_eq!(runtime.model_revision, "unversioned");

        let declared = EmbeddingSettings {
            model_revision: Some("release-2025-02".into()),
            ..legacy
        };
        let runtime = resolve_embedding_runtime(&declared);
        assert_eq!(runtime.model_revision, "release-2025-02");

        let malformed_legacy = EmbeddingSettings {
            model_revision: Some("bad\nrevision".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_embedding_runtime(&malformed_legacy).model_revision,
            "unversioned"
        );
    }

    #[test]
    fn saved_revision_wins_over_env_but_explicit_env_is_used_when_unset() {
        let settings = EmbeddingSettings {
            preset_id: Some(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID.into()),
            model_revision: Some("saved-ref".into()),
            ..Default::default()
        };
        let runtime = resolve_embedding_runtime_with_env(&settings, |name| match name {
            "MEMORY_EMBEDDING_MODEL_REVISION" => Some("environment-ref".into()),
            _ => None,
        });
        assert_eq!(runtime.model_revision, "saved-ref");

        let without_saved_revision = EmbeddingSettings {
            model_revision: None,
            ..settings
        };
        let runtime =
            resolve_embedding_runtime_with_env(&without_saved_revision, |name| match name {
                "MEMORY_EMBEDDING_MODEL_REVISION" => Some("environment-ref".into()),
                _ => None,
            });
        assert_eq!(runtime.model_revision, "environment-ref");
    }

    #[test]
    fn distance_type_uses_memories_vocabulary_and_is_managed_in_runtime_env() {
        let settings = EmbeddingSettings {
            preset_id: Some(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID.into()),
            ..Default::default()
        };
        for expected in ["cosine", "l2", "dot"] {
            let runtime = resolve_embedding_runtime_with_env(&settings, |name| match name {
                "MEMORY_DISTANCE_TYPE" => Some(expected.into()),
                _ => None,
            });
            assert_eq!(runtime.distance_type, expected);
            let env: std::collections::HashMap<_, _> =
                resolve_embedding_env_vars_from_runtime(&runtime)
                    .into_iter()
                    .collect();
            assert_eq!(
                env.get("MEMORY_DISTANCE_TYPE").map(String::as_str),
                Some(expected)
            );
        }
        let fallback = resolve_embedding_runtime_with_env(&settings, |_| None);
        assert_eq!(fallback.distance_type, "cosine");
    }

    #[test]
    fn settings_from_request_preserves_only_user_declared_revision() {
        let req = SetEmbeddingSettingsRequest {
            model_revision: Some("main".into()),
            ..preset_req(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID)
        };
        let settings = settings_from_request(&req);
        assert_eq!(settings.model_revision.as_deref(), Some("main"));

        let old_request = preset_req(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID);
        let settings = settings_from_request(&old_request);
        assert_eq!(settings.model_revision, None);
    }

    #[test]
    fn revision_request_rejects_blank_nul_and_control_characters() {
        for revision in ["", "  ", "bad\0ref", "bad\nref"] {
            let mut req = preset_req(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID);
            req.model_revision = Some(revision.into());
            assert!(validate_set_request(&req).is_err(), "revision {revision:?}");
        }
        let mut req = preset_req(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID);
        req.model_revision = Some("feature/retrieval-v2".into());
        assert!(validate_set_request(&req).is_ok());
    }

    #[test]
    fn embedding_identity_includes_tokenizer_revision_distance_and_dimension() {
        let old = resolve_embedding_runtime(&EmbeddingSettings {
            preset_id: Some(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID.into()),
            ..Default::default()
        });
        for changed in [
            EmbeddingRuntime {
                tokenizer_id: Some(old.model_id.clone()),
                ..old.clone()
            },
            EmbeddingRuntime {
                model_revision: "branch-a".into(),
                ..old.clone()
            },
            EmbeddingRuntime {
                distance_type: "l2".into(),
                ..old.clone()
            },
            EmbeddingRuntime {
                vector_size: old.vector_size + 1,
                ..old.clone()
            },
        ] {
            assert!(embedding_space_changed(&old, &changed));
        }
        assert!(!embedding_space_changed(
            &old,
            &EmbeddingRuntime {
                dtype: "BF16".into(),
                max_sequence_length: old.max_sequence_length + 1,
                ..old.clone()
            }
        ));
    }

    #[test]
    fn pending_chunk_configuration_guard_tracks_max_sequence_length_not_dtype() {
        let old = resolve_embedding_runtime(&EmbeddingSettings::default());
        let changed_max_sequence = EmbeddingRuntime {
            max_sequence_length: old.max_sequence_length + 1,
            ..old.clone()
        };
        let changed_dtype = EmbeddingRuntime {
            dtype: "BF16".into(),
            ..old.clone()
        };
        // The YAML renderer maps max_sequence_length to chunking_config.max_chunk_tokens.
        assert!(pending_chunk_configuration_changed(
            &old,
            &changed_max_sequence
        ));
        assert!(!pending_chunk_configuration_changed(&old, &changed_dtype));
    }

    #[test]
    fn pending_chunk_configuration_guard_allows_runtime_tuning_but_rejects_max_sequence_change() {
        let old = resolve_embedding_runtime(&EmbeddingSettings::default());
        let changed_dtype = EmbeddingRuntime {
            dtype: "BF16".into(),
            ..old.clone()
        };
        assert!(validate_pending_chunk_configuration_change(&old, &changed_dtype, true).is_ok());

        let changed_chunking = EmbeddingRuntime {
            max_sequence_length: old.max_sequence_length + 1,
            ..old.clone()
        };
        assert!(
            validate_pending_chunk_configuration_change(&old, &changed_chunking, true).is_err()
        );
        assert!(
            validate_pending_chunk_configuration_change(&old, &changed_chunking, false).is_ok()
        );
    }

    #[test]
    fn prepared_candidate_runtime_uses_the_same_env_projection_without_persisting() {
        let mut req = preset_req(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID);
        req.model_revision = Some("tag-v1".into());
        let runtime = prepare_embedding_candidate_runtime(&req, |name| match name {
            "MEMORY_DISTANCE_TYPE" => Some("dot".into()),
            "MEMORY_EMBEDDING_MODEL_REVISION" => Some("env-ref".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(runtime.model_revision, "tag-v1");
        assert_eq!(runtime.distance_type, "dot");
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
            "MEMORY_EMBEDDING_QUERY_PREFIX" => Some("env-query".into()),
            _ => None,
        });
        assert_eq!(rt.model_id, preset.hf_repo);
        assert_eq!(rt.vector_size, preset.vector_size);
        assert_eq!(rt.query_prefix.as_deref(), Some("env-query"));
    }

    #[test]
    fn resolve_runtime_env_blind_alias_returns_preset_defaults() {
        // The UI-facing `resolve_embedding_runtime` shim must ignore env
        // overrides so the Settings card always shows what the SAVED
        // settings resolve to.
        let s = EmbeddingSettings::default();
        let rt = resolve_embedding_runtime(&s);
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
            model_revision: None,
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
            model_revision: None,
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
            model_revision: None,
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

    fn data_paths_in(tmp: &Path) -> DataPaths {
        DataPaths::with_root(tmp.to_path_buf())
    }

    // ── embedding_space_changed + env-aware old runtime ────────────────────

    #[test]
    fn embedding_space_changed_true_when_vector_size_changes() {
        let mut old = runtime_from_preset(embedding_presets::default_preset());
        let new = old.clone();
        old.vector_size += 1;
        assert!(embedding_space_changed(&old, &new));
    }

    #[test]
    fn embedding_space_changed_true_when_model_id_changes() {
        let mut old = runtime_from_preset(embedding_presets::default_preset());
        let new = old.clone();
        old.model_id = "other/model".into();
        assert!(embedding_space_changed(&old, &new));
    }

    #[test]
    fn embedding_space_changed_false_when_only_dtype_or_max_seq_changes() {
        let old = runtime_from_preset(embedding_presets::default_preset());
        let new = EmbeddingRuntime {
            dtype: "BF16".into(),
            max_sequence_length: old.max_sequence_length + 1024,
            ..old.clone()
        };
        // Dtype / max_seq tweaks alone do not change vector identity.
        assert!(!embedding_space_changed(&old, &new));
    }

    #[test]
    fn env_aware_old_runtime_detects_change_against_envless_default_save() {
        // The effective environment-driven dimension must be included in the
        // old side of the safety comparison rather than assuming the preset.
        //
        // If we compared an env-BLIND old_runtime (which returns the
        // default preset's 1024) against the new default preset's 1024,
        // `embedding_space_changed` would be `false`, incorrectly treating
        // the candidate settings as the same embedding space.
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
            embedding_space_changed(&old_runtime, &new_runtime),
            "the effective env-driven old space must differ from the saved candidate"
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
            model_revision: None,
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
    }

    #[test]
    fn apply_embedding_to_disk_rejects_dimension_change_without_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        // A rejected migration must preserve the previous worker configuration.
        apply_embedding_settings_to_disk(
            &data,
            &preset_req(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID),
        )
        .unwrap();
        let before = std::fs::read(data.embedding_settings_path()).unwrap();
        assert!(
            apply_embedding_settings_to_disk(&data, &preset_req("qwen3-vl-embedding-2b")).is_err()
        );
        assert_eq!(
            std::fs::read(data.embedding_settings_path()).unwrap(),
            before
        );
    }

    #[test]
    fn apply_embedding_to_disk_allows_remote_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        // Remote browse mode still needs the local embedding model setting so
        // semantic query vectors can match the remote memories vector space.
        super::super::connection::save_connection_config(
            &data.connection_config_path(),
            &super::super::connection::ConnectionConfig::remote_pair(
                "http://h:9000",
                "http://h:9010",
            ),
        )
        .unwrap();
        let outcome =
            apply_embedding_settings_to_disk(&data, &preset_req("qwen3-vl-embedding-2b")).unwrap();
        assert!(outcome.changed);
        let saved = load_embedding_settings(&data.embedding_settings_path());
        assert_eq!(saved.preset_id.as_deref(), Some("qwen3-vl-embedding-2b"));

        // Remote mode has no local vector table to protect, so explicit
        // tokenizer identity changes are allowed as well.
        let model = "Qwen/Qwen3-VL-Embedding-2B";
        let tokenizer_change = custom_req(Some(model), Some(model), Some(2048), None, None);
        apply_embedding_settings_to_disk(&data, &tokenizer_change).unwrap();
        let saved = load_embedding_settings(&data.embedding_settings_path());
        assert_eq!(saved.custom_tokenizer_id.as_deref(), Some(model));
    }

    #[test]
    fn local_tokenizer_and_revision_changes_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        let original = preset_req(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID);
        apply_embedding_settings_to_disk(&data, &original).unwrap();
        let before = std::fs::read(data.embedding_settings_path()).unwrap();
        let model = embedding_presets::default_preset().hf_repo;
        let tokenizer_change = custom_req(Some(model), Some(model), Some(1024), None, None);
        let mut revision_change = preset_req(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID);
        revision_change.model_revision = Some("explicit-release".into());

        for request in [tokenizer_change, revision_change] {
            assert!(apply_embedding_settings_to_disk(&data, &request).is_err());
            assert_eq!(
                std::fs::read(data.embedding_settings_path()).unwrap(),
                before
            );
        }
    }

    #[test]
    fn local_identity_change_requires_explicit_cli_plan_permit() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        let original = preset_req(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID);
        apply_embedding_settings_to_disk(&data, &original).unwrap();
        let before = std::fs::read(data.embedding_settings_path()).unwrap();
        let candidate = custom_req(Some("other/model"), None, Some(1024), None, None);

        assert!(apply_embedding_settings_to_disk(&data, &candidate).is_err());
        assert!(
            apply_embedding_settings_to_disk_approved(
                &data,
                &candidate,
                EmbeddingPlanPermit::NoChange,
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(data.embedding_settings_path()).unwrap(),
            before
        );

        let outcome = apply_embedding_settings_to_disk_approved(
            &data,
            &candidate,
            EmbeddingPlanPermit::Adopt,
        )
        .unwrap();
        assert!(outcome.changed);
        assert_eq!(outcome.new_runtime.model_id, "other/model");
        let saved = load_embedding_settings(&data.embedding_settings_path());
        assert_eq!(saved.custom_model_id.as_deref(), Some("other/model"));

        let reconciled = apply_embedding_settings_to_disk_approved(
            &data,
            &original,
            EmbeddingPlanPermit::Reconcile,
        )
        .unwrap();
        assert!(reconciled.changed);
        assert_eq!(
            reconciled.new_runtime.model_id,
            embedding_presets::default_preset().hf_repo
        );
    }

    #[test]
    fn local_space_changes_preserve_settings_and_service_storage() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_paths_in(tmp.path());
        let original = preset_req(embedding_presets::DEFAULT_EMBEDDING_PRESET_ID);
        apply_embedding_settings_to_disk(&data, &original).unwrap();
        let before = std::fs::read(data.embedding_settings_path()).unwrap();
        let store = data.lancedb_dir().join("marker");
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(&store, b"service-owned").unwrap();
        let runtime =
            resolve_embedding_runtime(&load_embedding_settings(&data.embedding_settings_path()));
        for (model, dim) in [
            ("other/model", runtime.vector_size),
            (runtime.model_id.as_str(), runtime.vector_size + 1),
        ] {
            let request = SetEmbeddingSettingsRequest {
                preset_id: Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID.into()),
                custom_model_id: Some(model.into()),
                custom_vector_size: Some(dim),
                ..original.clone()
            };
            let error = apply_embedding_settings_to_disk(&data, &request)
                .err()
                .expect("local space change must be rejected");
            assert!(error.to_string().to_lowercase().contains("memories"));
            assert_eq!(
                std::fs::read(data.embedding_settings_path()).unwrap(),
                before
            );
            assert_eq!(std::fs::read(&store).unwrap(), b"service-owned");
            assert!(!data.root.join("lancedb-backup").exists());
        }
    }
}
