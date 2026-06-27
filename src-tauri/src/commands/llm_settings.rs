//! LLM provider settings: local (llama-cpp plugin) vs external (genai).
//!
//! Follows the same pattern as `connection.rs`: a small JSON file under the
//! data root (`llm-settings.json`) persisted alongside a macOS Keychain
//! entry for the API key. The file stores only non-secret fields; the key
//! never touches disk.

use std::path::Path;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

use super::AppState;
use super::llm_presets;
use super::llm_presets::KvCacheType;

/// Range guards obvious typos; llama-cpp clamps to the model cap downstream.
const LOCAL_CTX_SIZE_MIN: u32 = 512;
const LOCAL_CTX_SIZE_MAX: u32 = 1_048_576;

const KEYRING_SERVICE: &str = "lookback";
const KEYRING_ACCOUNT: &str = "llm-api-key";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LlmMode {
    #[default]
    Local,
    External,
}

/// Persisted (non-secret) LLM provider config.
///
/// The `local_*` fields drive the user-selectable local LLM model
/// (preset + free-text custom). They are all `Option<…>` so omitted values
/// deserialize into `None`; `resolve_local_runtime` then falls back to
/// [`llm_presets::default_preset`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LlmSettings {
    #[serde(default)]
    pub mode: LlmMode,
    #[serde(default)]
    pub provider_model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Curated preset id (see `llm_presets::PRESETS`), or the sentinel
    /// `"custom"` to take values from `local_model_file` / `local_hf_repo`
    /// instead. `None` means the default preset.
    #[serde(default)]
    pub local_preset_id: Option<String>,
    /// Free-text gguf filename (custom selection only).
    #[serde(default)]
    pub local_model_file: Option<String>,
    /// Free-text HuggingFace repo (custom selection only). Must match
    /// `^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$`.
    #[serde(default)]
    pub local_hf_repo: Option<String>,
    /// User override for `ctx_size`. `None` = use the preset's
    /// recommended value (or the historical 262144 for the custom path).
    /// Range-checked at save time against [`LOCAL_CTX_SIZE_MIN`] /
    /// [`LOCAL_CTX_SIZE_MAX`].
    #[serde(default)]
    pub local_ctx_size: Option<u32>,
    /// User override for llama.cpp KV cache quantization. `None` keeps the
    /// current default from [`llm_presets::DEFAULT_KV_CACHE_TYPE`].
    #[serde(default)]
    pub local_kv_cache_type: Option<KvCacheType>,
}

/// Frontend-facing response. Never exposes the actual API key — only
/// whether one has been stored.
#[derive(Debug, Clone, Serialize)]
pub struct LlmSettingsResponse {
    pub mode: LlmMode,
    pub provider_model: Option<String>,
    pub api_key_set: bool,
    pub base_url: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub local_preset_id: Option<String>,
    pub local_model_file: Option<String>,
    pub local_hf_repo: Option<String>,
    pub local_ctx_size: Option<u32>,
    pub local_kv_cache_type: Option<KvCacheType>,
}

/// Request from the frontend. `api_key = null` means "don't change the
/// stored key"; an empty string deletes it.
#[derive(Debug, Clone, Deserialize)]
pub struct SetLlmSettingsRequest {
    pub mode: LlmMode,
    pub provider_model: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    #[serde(default)]
    pub local_preset_id: Option<String>,
    #[serde(default)]
    pub local_model_file: Option<String>,
    #[serde(default)]
    pub local_hf_repo: Option<String>,
    #[serde(default)]
    pub local_ctx_size: Option<u32>,
    #[serde(default)]
    pub local_kv_cache_type: Option<KvCacheType>,
}

/// Resolved local-LLM runtime values. Produced by [`resolve_local_runtime`]
/// from the persisted settings + the preset table; consumed by
/// `lib.rs::build_sidecar_config` (to inject env into the jobworkerp
/// child), `commands::model::llm_identity` (for the Settings card label),
/// and `commands::chat::build_chat_args` (for the thinking-mode
/// suppression kwarg).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRuntime {
    pub model_file: String,
    pub hf_repo: String,
    pub ctx_size: u32,
    pub kv_cache_type: KvCacheType,
    pub thinking_kwarg: llm_presets::ThinkingKwarg,
}

pub fn resolve_kv_cache_type(settings: &LlmSettings) -> KvCacheType {
    settings
        .local_kv_cache_type
        .unwrap_or(llm_presets::DEFAULT_KV_CACHE_TYPE)
}

pub fn resolve_kv_cache_type_with_env(
    settings: &LlmSettings,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> KvCacheType {
    if let Some(kv) = settings.local_kv_cache_type {
        return kv;
    }
    env_lookup("LOOKBACK_LLM_KV_CACHE_TYPE")
        .as_deref()
        .and_then(KvCacheType::from_runner_value)
        .unwrap_or(llm_presets::DEFAULT_KV_CACHE_TYPE)
}

/// Project `LlmSettings` into the runtime values the sidecar / chat path
/// needs. Pure so the precedence (UI custom > UI preset > default preset)
/// is unit-tested.
///
/// Custom (`local_preset_id == Some("custom")`) takes the user fields
/// verbatim. `thinking_kwarg` is forced to `ThinkingKwarg::None` on the
/// custom path because we cannot infer the model family from the
/// free-text fields, and sending the WRONG polarity (e.g. Qwen's
/// `Disable` to a Gemma 4 model) is the exact failure mode that breaks
/// RAG tool calling. A user who needs the kwarg on a custom repo should
/// pick the nearest curated preset.
pub fn resolve_local_runtime(settings: &LlmSettings) -> LocalRuntime {
    if settings.local_preset_id.as_deref() == Some(llm_presets::CUSTOM_PRESET_ID) {
        return LocalRuntime {
            model_file: settings.local_model_file.clone().unwrap_or_default(),
            hf_repo: settings.local_hf_repo.clone().unwrap_or_default(),
            // 262144 mirrors the historical hardcoded default the YAML
            // placeholder uses when no env override is set.
            ctx_size: settings.local_ctx_size.unwrap_or(262_144),
            kv_cache_type: resolve_kv_cache_type(settings),
            thinking_kwarg: llm_presets::ThinkingKwarg::None,
        };
    }
    // Unset and unknown preset ids both fall back to the default preset.
    let preset = settings
        .local_preset_id
        .as_deref()
        .and_then(llm_presets::find_preset)
        .unwrap_or_else(llm_presets::default_preset);
    LocalRuntime {
        model_file: preset.gguf_file.into(),
        hf_repo: preset.hf_repo.into(),
        ctx_size: settings
            .local_ctx_size
            .unwrap_or(preset.recommended_ctx_size),
        kv_cache_type: resolve_kv_cache_type(settings),
        thinking_kwarg: preset.thinking_kwarg,
    }
}

/// `(LOOKBACK_LLM_MODEL, LOOKBACK_LLM_HF_REPO, LOOKBACK_LLM_CTX_SIZE)` triple
/// the sidecar env injection needs, re-resolved every sidecar (re)start so
/// a Settings change takes effect without an app relaunch.
///
/// `env_lookup` is a dev override that only wins when the user has not yet
/// picked a preset (`local_preset_id == None`); once Settings is saved the
/// file is authoritative. Pass `|name| std::env::var(name).ok()` from
/// production code — tests inject a closure to keep the resolver pure.
pub fn resolve_local_llm_env_triple<F>(
    settings: &LlmSettings,
    env_lookup: F,
) -> (Option<String>, Option<String>, Option<u32>)
where
    F: Fn(&str) -> Option<String>,
{
    let runtime = resolve_local_runtime(settings);
    if settings.local_preset_id.is_some() {
        return (
            Some(runtime.model_file),
            Some(runtime.hf_repo),
            Some(runtime.ctx_size),
        );
    }
    (
        env_lookup("LOOKBACK_LLM_MODEL").or(Some(runtime.model_file)),
        env_lookup("LOOKBACK_LLM_HF_REPO").or(Some(runtime.hf_repo)),
        env_lookup("LOOKBACK_LLM_CTX_SIZE")
            .and_then(|v| v.parse().ok())
            .or(Some(runtime.ctx_size)),
    )
}

/// `chat_template_kwargs.enable_thinking` policy for the LLM the sidecar
/// is actually loading right now.
///
/// Mirrors [`resolve_local_llm_env_triple`]'s precedence: when the user
/// has saved a preset (`local_preset_id == Some(_)`) the curated kwarg is
/// authoritative. When the user has NOT saved a preset AND a dev env
/// override (`LOOKBACK_LLM_MODEL`) is in effect, the running model may
/// not be the default preset's model family, and the `enable_thinking`
/// polarity is family-specific. Resolve to
/// [`llm_presets::ThinkingKwarg::None`] in that case so the kwarg is
/// dropped entirely, matching the safety-pin used on the custom path.
///
/// Pass `|name| std::env::var(name).ok()` from production code; tests
/// inject a closure to keep the resolver pure.
pub fn thinking_kwarg_for<F>(settings: &LlmSettings, env_lookup: F) -> llm_presets::ThinkingKwarg
where
    F: Fn(&str) -> Option<String>,
{
    if settings.local_preset_id.is_none() && env_lookup("LOOKBACK_LLM_MODEL").is_some() {
        return llm_presets::ThinkingKwarg::None;
    }
    resolve_local_runtime(settings).thinking_kwarg
}

use super::is_valid_hf_repo;

/// Validate a custom-preset request. Returns the offending field's error
/// message on failure so `set_llm_settings` can reject without restarting
/// the sidecar. Pure for unit-testability.
fn validate_custom_local_fields(req: &SetLlmSettingsRequest) -> Result<(), String> {
    let repo = req
        .local_hf_repo
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "custom preset requires `local_hf_repo`".to_string())?;
    if !is_valid_hf_repo(repo) {
        return Err(format!(
            "invalid local_hf_repo {repo:?}: expected `org/name` with [A-Za-z0-9_.-]"
        ));
    }
    let file = req
        .local_model_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "custom preset requires `local_model_file`".to_string())?;
    if !file.to_ascii_lowercase().ends_with(".gguf") {
        return Err(format!(
            "invalid local_model_file {file:?}: expected a `.gguf` filename"
        ));
    }
    Ok(())
}

/// Validate the optional ctx_size override, regardless of preset/custom.
/// Pure for unit-testability.
fn validate_ctx_size(value: Option<u32>) -> Result<(), String> {
    let Some(v) = value else { return Ok(()) };
    if !(LOCAL_CTX_SIZE_MIN..=LOCAL_CTX_SIZE_MAX).contains(&v) {
        return Err(format!(
            "invalid local_ctx_size {v}: must be in [{LOCAL_CTX_SIZE_MIN}, {LOCAL_CTX_SIZE_MAX}]"
        ));
    }
    Ok(())
}

// ── persistence ──────────────────────────────────────────────────────

/// Resolve the registered jobworkerp worker name that serves `mode`.
/// The two workers (`memories-llm`, `memories-llm-external`) are registered
/// at sidecar startup by `workers/llm-workers.yaml`; this is the single
/// source of truth for "which name does the active mode dispatch to".
pub fn worker_name_for(mode: LlmMode) -> &'static str {
    match mode {
        LlmMode::Local => "memories-llm",
        LlmMode::External => "memories-llm-external",
    }
}

/// The worker a hot-reload (upsert -> load) must target for `mode`:
/// `memories-llm` (LLMPromptRunner, `use_static`) for Local, and
/// `memories-llm-external` (genai LLM runner, non-static) for External.
pub(crate) struct ReloadTarget {
    pub name: &'static str,
}

pub(crate) fn reload_target_for_mode(mode: LlmMode) -> ReloadTarget {
    match mode {
        LlmMode::Local => ReloadTarget {
            name: "memories-llm",
        },
        LlmMode::External => ReloadTarget {
            name: "memories-llm-external",
        },
    }
}

pub fn load_llm_settings(path: &Path) -> LlmSettings {
    let Ok(bytes) = std::fs::read(path) else {
        return LlmSettings::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save_llm_settings(path: &Path, settings: &LlmSettings) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| AppError::Config(format!("serialize llm settings: {e}")))?;
    std::fs::write(path, json)?;
    Ok(())
}

// ── keychain ─────────────────────────────────────────────────────────

fn keyring_entry() -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
}

/// Process-wide cache of the resolved API key so we touch the macOS
/// Keychain at most once per process. Without this, the sidecar start
/// path (`lifecycle.rs::start_inner`) and the Settings card open path
/// (`get_llm_settings`) each call `get_password()` independently, and
/// every read pops the system "allow access?" prompt for users who
/// didn't pick *Always Allow* (and unavoidably for ad-hoc-signed dev
/// builds whose codesign identity is not in the item's ACL).
///
/// Layering: outer `None` = not loaded yet; inner `None` = loaded but no
/// key stored. `save_api_key` / `delete_api_key` populate the cache from
/// the value they just wrote, so a save → restart-sidecar round-trip
/// reads the cache instead of re-prompting.
static API_KEY_CACHE: RwLock<Option<Option<String>>> = RwLock::new(None);

fn cache_store(value: Option<String>) {
    if let Ok(mut w) = API_KEY_CACHE.write() {
        *w = Some(value);
    }
}

/// Reset the cache so the next `load_api_key` re-reads the Keychain.
/// Only used by tests — production code keeps the cache live for the
/// whole process lifetime.
#[cfg(test)]
fn cache_clear() {
    if let Ok(mut w) = API_KEY_CACHE.write() {
        *w = None;
    }
}

/// Outcome of a Keychain read, classified so the caller knows whether the
/// result is safe to cache for the rest of the process lifetime.
enum KeyringRead {
    /// A key is stored (non-empty).
    Found(String),
    /// Confirmed absence — `NoEntry`, or an empty stored value. Safe to
    /// cache as "no key configured".
    NotStored,
    /// The Keychain could not be read (locked, access denied, platform
    /// failure, …). NOT cacheable — a later unlock must be retried.
    Transient,
}

/// Classify a `keyring` read result. Split out as a pure function because a
/// locked / denied Keychain (`PlatformFailure` / `NoStorageAccess`) cannot
/// be reproduced through the mock credential store, so the cache-vs-retry
/// policy is unit-tested here instead of through `load_api_key`.
fn classify_keyring_read(result: keyring::Result<String>) -> KeyringRead {
    match result {
        Ok(key) if !key.is_empty() => KeyringRead::Found(key),
        // An empty stored value is treated like "no key": same as the old
        // `.filter(|k| !k.is_empty())`.
        Ok(_) => KeyringRead::NotStored,
        // `NoEntry` is a definitive "nothing registered" — cache it.
        Err(keyring::Error::NoEntry) => KeyringRead::NotStored,
        // Locked / denied / platform error: don't cache so an unlock retries.
        Err(_) => KeyringRead::Transient,
    }
}

pub fn load_api_key() -> Option<String> {
    if let Some(cached) = API_KEY_CACHE.read().ok().and_then(|r| r.clone()) {
        return cached;
    }
    // Only cache a CONFIRMED state. A locked Keychain / denied access prompt
    // surfaces as `PlatformFailure` / `NoStorageAccess` (NOT `NoEntry`);
    // caching the resulting `None` would pin "no key configured" for the
    // whole process, so a later unlock would never be retried and the
    // External LLM would stay unauthenticated until an app relaunch.
    let entry = match keyring_entry() {
        Ok(e) => e,
        // Entry construction failure is treated as transient: don't cache.
        Err(_) => return None,
    };
    match classify_keyring_read(entry.get_password()) {
        KeyringRead::Found(key) => {
            cache_store(Some(key.clone()));
            Some(key)
        }
        KeyringRead::NotStored => {
            cache_store(None);
            None
        }
        // Leave the cache untouched so a subsequent unlock is retried.
        KeyringRead::Transient => None,
    }
}

fn save_api_key(key: &str) -> AppResult<()> {
    let entry = keyring_entry().map_err(|e| AppError::Config(format!("keyring init: {e}")))?;
    if key.is_empty() {
        // The set_llm_settings contract treats `Some("")` as an explicit
        // delete, so a failure here (locked Keychain, denied prompt) MUST
        // propagate — silently swallowing it would let the UI report
        // "deleted" while the credential is still readable. `NoEntry`
        // remains success because the goal state — no stored key — is
        // already satisfied.
        match entry.delete_credential() {
            Ok(_) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(AppError::Config(format!("keyring delete: {e}"))),
        }
        cache_store(None);
    } else {
        entry
            .set_password(key)
            .map_err(|e| AppError::Config(format!("keyring set: {e}")))?;
        cache_store(Some(key.to_string()));
    }
    Ok(())
}

/// Restore the Keychain API key to a previously-captured state, used by
/// the `apply_settings` rollback path. `Some(key)` re-stores the old key,
/// `None` deletes any current entry (the old state had no key). Best-effort
/// — errors are returned so the caller can log, but rollback proceeds.
pub fn restore_api_key(old: Option<String>) -> AppResult<()> {
    match old {
        Some(key) => save_api_key(&key),
        // Empty string is the delete contract in `save_api_key`.
        None => save_api_key(""),
    }
}

/// Best-effort delete of the stored API key. Used by `purge_all_data` so
/// the "delete all data" operation also clears the secret from the macOS
/// Keychain (which lives outside the app data root). Returns the keyring
/// error message on failure so the caller can surface it as a warning;
/// `NoEntry` is treated as success because the goal state — no stored key —
/// is already satisfied.
pub fn delete_api_key() -> Result<(), String> {
    let entry = keyring_entry().map_err(|e| format!("keyring init: {e}"))?;
    match entry.delete_credential() {
        Ok(_) | Err(keyring::Error::NoEntry) => {
            cache_store(None);
            Ok(())
        }
        Err(e) => Err(format!("keyring delete: {e}")),
    }
}

// ── provider detection ───────────────────────────────────────────────

/// Identified genai provider for routing API-key env vars and human-facing
/// labels. The genai crate auto-detects the provider from the model name
/// prefix; this enum mirrors that detection so the sidecar process and
/// the UI agree on what `model_name` means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    Anthropic,
    Gemini,
    DeepSeek,
    Cohere,
    Zai,
    Groq,
    XAi,
    OpenRouter,
    /// Unknown prefix — falls back to OpenAI-compatible behaviour so
    /// proxies (LiteLLM etc.) configured with an arbitrary model name
    /// still receive a usable API key env var.
    OpenAiCompatible,
}

/// Decide which provider `model` belongs to. A non-empty `base_url`
/// overrides the prefix detection — an explicit proxy / OpenAI-compatible
/// endpoint always reads `OPENAI_API_KEY`.
///
/// The empty-string guard is deliberate: `set_llm_settings` may persist
/// `base_url: Some("")` for legacy callers (the frontend used to send an
/// empty string for an unset field), and without it a Gemini / Anthropic
/// model would be silently re-routed to `OPENAI_API_KEY` and report
/// "API key not valid" against the real provider.
pub fn detect_provider(model: &str, base_url: Option<&str>) -> Provider {
    if base_url.is_some_and(|u| !u.is_empty()) {
        return Provider::OpenAi;
    }
    let m = model.to_ascii_lowercase();
    if m.starts_with("gpt") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") {
        Provider::OpenAi
    } else if m.starts_with("claude") {
        Provider::Anthropic
    } else if m.starts_with("gemini") {
        Provider::Gemini
    } else if m.starts_with("deepseek") {
        Provider::DeepSeek
    } else if m.starts_with("command") {
        Provider::Cohere
    } else if m.starts_with("glm") {
        Provider::Zai
    } else if m.starts_with("groq::") {
        Provider::Groq
    } else if m.starts_with("xai::") {
        Provider::XAi
    } else if m.starts_with("open_router::") {
        Provider::OpenRouter
    } else {
        Provider::OpenAiCompatible
    }
}

impl Provider {
    /// The environment variable name the provider's SDK reads its API key
    /// from. Must agree with the genai crate's per-adapter
    /// `API_KEY_DEFAULT_ENV_NAME`.
    pub fn env_var(self) -> &'static str {
        match self {
            Provider::OpenAi | Provider::OpenAiCompatible => "OPENAI_API_KEY",
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::Gemini => "GEMINI_API_KEY",
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::Cohere => "COHERE_API_KEY",
            Provider::Zai => "ZAI_API_KEY",
            Provider::Groq => "GROQ_API_KEY",
            Provider::XAi => "XAI_API_KEY",
            Provider::OpenRouter => "OPEN_ROUTER_API_KEY",
        }
    }

    /// Human-readable label shown in the Settings card.
    pub fn display_name(self) -> &'static str {
        match self {
            Provider::OpenAi => "OpenAI",
            Provider::Anthropic => "Anthropic",
            Provider::Gemini => "Google Gemini",
            Provider::DeepSeek => "DeepSeek",
            Provider::Cohere => "Cohere",
            Provider::Zai => "ZAI",
            Provider::Groq => "Groq",
            Provider::XAi => "xAI",
            Provider::OpenRouter => "OpenRouter",
            Provider::OpenAiCompatible => "OpenAI-compatible",
        }
    }
}

/// Convenience wrapper preserved for call sites that just want the env
/// var name. Equivalent to `detect_provider(model, base_url).env_var()`.
pub fn provider_env_var_for_model(model: &str, base_url: Option<&str>) -> &'static str {
    detect_provider(model, base_url).env_var()
}

/// Convenience wrapper preserved for call sites that just want the
/// display label. Equivalent to `detect_provider(model, None).display_name()`.
pub fn provider_display_name(model: &str) -> &'static str {
    detect_provider(model, None).display_name()
}

// ── Tauri commands ───────────────────────────────────────────────────

#[tauri::command]
pub fn get_llm_settings(state: tauri::State<'_, AppState>) -> AppResult<LlmSettingsResponse> {
    let settings = load_llm_settings(&state.data.llm_settings_path());
    Ok(LlmSettingsResponse {
        mode: settings.mode,
        provider_model: settings.provider_model,
        api_key_set: load_api_key().is_some(),
        base_url: settings.base_url,
        max_tokens: settings.max_tokens,
        temperature: settings.temperature,
        local_preset_id: settings.local_preset_id,
        local_model_file: settings.local_model_file,
        local_hf_repo: settings.local_hf_repo,
        local_ctx_size: settings.local_ctx_size,
        local_kv_cache_type: settings.local_kv_cache_type,
    })
}

/// Validate, persist `llm-settings.json`, and store the API key in the
/// Keychain — WITHOUT touching the sidecar. Returns whether the change
/// requires a sidecar restart (any field that the spawned child reads
/// changed; `Some("")` delete intent counts via `key_changed`).
///
/// Split out from [`set_llm_settings`] so the unified `apply_settings`
/// command can persist several settings and restart the sidecar exactly
/// once. The individual command is a thin wrapper that calls this then
/// restarts on its own.
/// Validate an LLM settings request WITHOUT persisting anything. Split out
/// so the unified `apply_settings` can validate the whole batch before any
/// file is written (a later card's validation failure must not leave an
/// earlier card's change half-saved). Validation is only meaningful in
/// Local mode — External mode ignores the local_* fields.
pub fn validate_llm_request(req: &SetLlmSettingsRequest) -> AppResult<()> {
    if req.mode == LlmMode::Local {
        if req.local_preset_id.as_deref() == Some(llm_presets::CUSTOM_PRESET_ID) {
            validate_custom_local_fields(req).map_err(AppError::Config)?;
        } else if let Some(id) = req.local_preset_id.as_deref()
            && llm_presets::find_preset(id).is_none()
        {
            return Err(AppError::Config(format!(
                "unknown local_preset_id {id:?}: not in the curated preset list"
            )));
        }
        validate_ctx_size(req.local_ctx_size).map_err(AppError::Config)?;
    }
    Ok(())
}

/// Outcome of persisting a settings change. `reload_needed` is the
/// historical "restart vs no-op" signal; `old` / `new` are surfaced so
/// the reload pipeline can roll back to the previous config (and re-apply
/// its env / worker upsert) if the new model fails to load.
pub struct LlmApplyOutcome {
    pub reload_needed: bool,
    pub old: LlmSettings,
    pub new: LlmSettings,
    /// True when this apply touched the Keychain API key (set or delete).
    /// The key is injected into the jobworkerp CHILD's env at spawn time
    /// (`OPENAI_API_KEY` etc.), NOT into the worker's `runner_settings`, so
    /// a key change can only take effect via a sidecar restart — never via
    /// an in-place worker hot-reload. Surfaced so [`hot_reload_safe`] can
    /// force the restart path.
    pub key_changed: bool,
}

impl LlmApplyOutcome {
    /// Whether this change can be applied via an in-place LLM worker
    /// hot-reload (upsert → load) WITHOUT restarting the sidecar.
    ///
    /// Hot-reload re-registers the LLM worker (upsert) and `Load`s it WITHOUT
    /// restarting the sidecar. Local GGUF targets are excluded because a
    /// completed `ReleaseStaticWorker` RPC does not guarantee that Metal-wired
    /// pages have left the process before the next allocation.
    ///
    /// `spawned_key_env` is the provider API-key env var the RUNNING jobworkerp
    /// child was spawned with (`Sidecars::spawned_external_key_env`), or `None`
    /// if none was injected. A genai API key reaches the worker ONLY through
    /// that spawn-time env (the `GenaiRunnerSettings` proto has no key field),
    /// and a running child's env can't be changed — so a switch INTO External
    /// is hot-reloadable only when the child already carries the exact env var
    /// the new provider reads.
    ///
    /// Rules:
    /// - `key_changed` ⇒ never. A new/deleted Keychain key only reaches the
    ///   child via a fresh spawn (env is fixed at spawn).
    /// - `new.mode == Local` ⇒ never. Restart the sidecar so process exit owns
    ///   Metal teardown, then eagerly load in the fresh child.
    /// - `new.mode == External` ⇒ only when
    ///   `provider_env_var_for_model(new.provider_model, new.base_url)` equals
    ///   `spawned_key_env`. The child was spawned with a key for the SAME
    ///   provider env var the new model needs (e.g. it already had
    ///   `OPENAI_API_KEY` and the new model is also OpenAI-routed). A provider
    ///   switch that changes the env var, or a child spawned without any key,
    ///   fails this and restarts so the child re-injects the right key.
    ///
    /// The frontend mirrors a conservative version of this gate in
    /// `src/pages/Settings.tsx`: it always shows the restart copy (it can't call
    /// this predicate nor see `spawned_key_env`), which is a harmless
    /// over-warning even when the backend actually hot-reloads.
    pub fn hot_reload_safe(&self, spawned_key_env: Option<&str>) -> bool {
        if self.key_changed {
            return false;
        }
        match self.new.mode {
            // Process exit is the only reliable boundary for releasing a warmed
            // llama.cpp Metal pool before the model is allocated again.
            LlmMode::Local => false,
            LlmMode::External => {
                let model = self.new.provider_model.as_deref().unwrap_or_default();
                let base_url = self.new.base_url.as_deref().filter(|u| !u.is_empty());
                let needed = provider_env_var_for_model(model, base_url);
                spawned_key_env == Some(needed)
            }
        }
    }
}

pub fn apply_llm_settings_to_disk(
    path: &Path,
    req: SetLlmSettingsRequest,
) -> AppResult<LlmApplyOutcome> {
    let old = load_llm_settings(path);

    // Validate before persisting so a bad custom entry doesn't take the
    // sidecar down on the next restart.
    validate_llm_request(&req)?;

    // Pull the api_key out (it's persisted separately via Keychain, not
    // into `llm-settings.json`) so the rest of `req` can move into
    // `new_settings` without cloning the String fields.
    let api_key = req.api_key;
    let new_settings = LlmSettings {
        mode: req.mode,
        provider_model: req.provider_model,
        base_url: req.base_url,
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        local_preset_id: req.local_preset_id,
        local_model_file: req.local_model_file,
        local_hf_repo: req.local_hf_repo,
        local_ctx_size: req.local_ctx_size,
        local_kv_cache_type: req.local_kv_cache_type,
    };
    save_llm_settings(path, &new_settings)?;

    if let Some(key) = &api_key {
        save_api_key(key)?;
    }

    // Reload is needed when any config that affects the spawned child
    // changed. `Some("")` (delete intent) counts via `key_changed`.
    let model_changed = old.provider_model != new_settings.provider_model;
    let base_url_changed = old.base_url != new_settings.base_url;
    let key_changed = api_key.is_some();
    let mode_changed = old.mode != new_settings.mode;
    let local_changed = old.local_preset_id != new_settings.local_preset_id
        || old.local_model_file != new_settings.local_model_file
        || old.local_hf_repo != new_settings.local_hf_repo
        || old.local_ctx_size != new_settings.local_ctx_size
        || old.local_kv_cache_type != new_settings.local_kv_cache_type;

    Ok(LlmApplyOutcome {
        reload_needed: model_changed
            || base_url_changed
            || key_changed
            || mode_changed
            || local_changed,
        old,
        new: new_settings,
        key_changed,
    })
}

/// Upper bound for the `Load` wait. A first-time GGUF download for a
/// 27B-class model over a slow link can take many minutes; mirror the
/// dispatch timeout (`JobworkerpHandle::DEFAULT_JOB_TIMEOUT_SEC`, 3h) so
/// a legitimately slow download is not cut short.
const LLM_LOAD_TIMEOUT_MS: u64 = 3 * 60 * 60 * 1000;

/// Build the `%{NAME:-default}` substitution map for the LLM workers YAML
/// from `settings`, WITHOUT reading or mutating the process environment.
///
/// This is the env-free counterpart of the old `set_var`-based path: the
/// hot-reload only needs these values to expand the committed YAML's
/// placeholders before re-registering the workers (the running child already
/// has its own env from spawn), so mutating the shared parent env — `unsafe`
/// and a data-race risk under Tauri/tokio's multi-threaded runtime — is
/// unnecessary. Keys absent from the map fall back to the YAML's own
/// `:-default`, so an unset External model leaves the committed `gpt-4o`
/// default in place. `LOOKBACK_REUSE_KV_PREFIX` is deliberately omitted so
/// the YAML default (`true`) wins, matching the dev-override semantics.
fn llm_placeholder_vars(settings: &LlmSettings) -> std::collections::HashMap<String, String> {
    let mut vars = std::collections::HashMap::new();
    // Local triple. `|_| None` (no env lookup): a hot-reload always has a
    // user-selected preset, so the shell override is irrelevant — and reading
    // the env here is exactly what we're avoiding.
    let (model, hf_repo, ctx_size) = resolve_local_llm_env_triple(settings, |_| None);
    if let Some(m) = model {
        vars.insert("LOOKBACK_LLM_MODEL".to_string(), m);
    }
    if let Some(r) = hf_repo {
        vars.insert("LOOKBACK_LLM_HF_REPO".to_string(), r);
    }
    if let Some(c) = ctx_size {
        vars.insert("LOOKBACK_LLM_CTX_SIZE".to_string(), c.to_string());
    }
    vars.insert(
        "LOOKBACK_LLM_KV_CACHE_TYPE".to_string(),
        resolve_kv_cache_type(settings).runner_value().to_string(),
    );
    // External worker is registered from the same YAML even on the Local
    // hot-reload path, so its placeholders must still resolve. A non-empty
    // base_url is passed through; an empty / unset one is left to the YAML
    // default (empty), which the genai runner treats as "unset".
    if let Some(m) = settings.provider_model.as_deref().filter(|s| !s.is_empty()) {
        vars.insert("LOOKBACK_EXTERNAL_LLM_MODEL".to_string(), m.to_string());
    }
    if let Some(u) = settings.base_url.as_deref().filter(|s| !s.is_empty()) {
        vars.insert("LOOKBACK_EXTERNAL_LLM_BASE_URL".to_string(), u.to_string());
    }
    vars
}

/// Resolve `%{NAME}` / `%{NAME:-default}` placeholders in `raw` from `vars`,
/// WITHOUT consulting the process environment. Mirrors the grammar of
/// jobworkerp-client's `yaml_common::expand_env`
/// (`%\{([A-Z_][A-Z0-9_]*)(?::-([^}]*))?\}`) so the output is byte-identical
/// to what an env-based expansion would produce — except the values come
/// from the explicit map. A name missing from `vars` falls back to its
/// inline `:-default`; a name with neither is left as-is (the downstream
/// `expand_env` will then resolve or reject it exactly as before).
///
/// Hand-rolled rather than pulling in the `regex` crate: the grammar is a
/// fixed two-capture shape and the YAML is small, so a single linear scan is
/// simpler than a new dependency.
fn resolve_yaml_placeholders(
    raw: &str,
    vars: &std::collections::HashMap<String, String>,
) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        // At a `%{` opener, try to resolve one placeholder; on a match advance
        // past it. Anything else (including a `%{` that isn't a valid
        // placeholder) is copied verbatim one char at a time.
        if let Some((replacement, consumed)) = try_resolve_placeholder(raw, i, vars) {
            out.push_str(&replacement);
            i += consumed;
            continue;
        }
        let ch = raw[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Try to resolve a single `%{NAME:-default}` placeholder starting at byte
/// `start` in `raw`. Returns `(replacement, bytes_consumed)` on a match, or
/// `None` when `start` is not a valid placeholder opener (so the caller emits
/// the byte literally). Split out of [`resolve_yaml_placeholders`] to keep
/// the scan loop flat (no nested `if let` chain).
fn try_resolve_placeholder(
    raw: &str,
    start: usize,
    vars: &std::collections::HashMap<String, String>,
) -> Option<(String, usize)> {
    let bytes = raw.as_bytes();
    if bytes[start] != b'%' || start + 1 >= bytes.len() || bytes[start + 1] != b'{' {
        return None;
    }
    let close = raw[start + 2..].find('}')?;
    let inner = &raw[start + 2..start + 2 + close];
    let (name, default) = match inner.split_once(":-") {
        Some((n, d)) => (n, Some(d)),
        None => (inner, None),
    };
    // Only treat it as a placeholder when the NAME matches the expand_env
    // grammar: a leading `[A-Z_]` then `[A-Z0-9_]*`. Otherwise a stray brace
    // pair (e.g. a Liquid `$${ ... }`) must pass through. Split head/tail so
    // the "first byte can't be a digit" rule reads directly.
    let is_env_name = match name.as_bytes().split_first() {
        Some((&head, tail)) => {
            (head == b'_' || head.is_ascii_uppercase())
                && tail
                    .iter()
                    .all(|&b| b == b'_' || b.is_ascii_uppercase() || b.is_ascii_digit())
        }
        None => false, // empty name
    };
    if !is_env_name {
        return None;
    }
    let consumed = 2 + close + 1;
    let replacement = match vars.get(name) {
        Some(v) => v.clone(),
        // No mapped value: use the inline default, or leave the placeholder
        // verbatim (downstream expand_env then resolves or rejects it).
        None => match default {
            Some(d) => d.to_string(),
            None => raw[start..start + consumed].to_string(),
        },
    };
    Some((replacement, consumed))
}

/// Hot-reload the External LLM worker against `settings` WITHOUT restarting
/// the sidecar: resolve the committed YAML's `%{LOOKBACK_LLM_*}` placeholders
/// against `settings` (env-free), re-register the workers from that resolved
/// text (so the upserted `runner_settings` carry the new model), then `Load`
/// the External worker to validate the selection synchronously. Local targets
/// always take the sidecar-restart path.
///
/// `handle` must point at the LOCAL sidecar's jobworkerp — the LLM worker
/// lives there even in remote browse mode.
///
/// Normal callers gate entry on [`LlmApplyOutcome::hot_reload_safe`], which
/// excludes Local targets.
/// `old` is no longer used for pool handling; it remains in the signature for
/// the rollback callers.
///
/// Unlike the sidecar (re)start path, this does NOT mutate the process
/// environment: the placeholders are resolved into the YAML text here, so the
/// downstream `expand_env` finds nothing left and never calls `getenv` —
/// sidestepping the multi-threaded `set_var` data race the restart path
/// tolerates only because it spawns a fresh child immediately after.
pub(crate) async fn reload_llm_worker(
    handle: &crate::jobworkerp::JobworkerpHandle,
    worker_yaml: &Path,
    new: &LlmSettings,
    old: &LlmSettings,
) -> AppResult<()> {
    // Read the committed YAML and resolve its LLM placeholders from `new`
    // (NOT the env). Register from the resolved text with the original YAML's
    // directory as `base_dir` so the `$file:` workflow includes still resolve.
    let raw = std::fs::read_to_string(worker_yaml).map_err(|e| {
        AppError::WorkerRegistration(format!("read {}: {e}", worker_yaml.display()))
    })?;
    let resolved = resolve_yaml_placeholders(&raw, &llm_placeholder_vars(new));
    let base_dir = worker_yaml.parent().unwrap_or_else(|| Path::new("."));
    // Re-register every worker in the YAML: the upsert is idempotent and
    // the non-LLM (WORKFLOW) workers re-resolve to the same definition, so
    // re-applying the whole file keeps the placeholder logic in one place.
    handle
        .register_workers_from_yaml_str(&resolved, base_dir)
        .await?;

    let _ = old;

    let new_target = reload_target_for_mode(new.mode);
    handle
        .load_worker(new_target.name, Some(LLM_LOAD_TIMEOUT_MS))
        .await
}

/// Warm the Local GGUF after a settings-driven sidecar restart.
///
/// The restarted child has a clean Metal pool, so loading here confirms the
/// selected model and completes any download before the Settings save returns.
/// Failures are surfaced to the UI but remain best-effort because the sidecar
/// itself is already running and can retry on the first chat.
pub(crate) async fn load_local_model_after_restart(
    app: &tauri::AppHandle,
    sidecars: &Arc<crate::sidecar::Sidecars>,
    new: &LlmSettings,
) {
    if new.mode != LlmMode::Local {
        return;
    }
    let Some(endpoints) = sidecars.current_endpoints() else {
        return;
    };
    let target = reload_target_for_mode(LlmMode::Local);
    let result = async {
        let handle =
            crate::jobworkerp::JobworkerpHandle::connect(&endpoints.jobworkerp_url()).await?;
        handle
            .load_worker(target.name, Some(LLM_LOAD_TIMEOUT_MS))
            .await
    }
    .await;
    if let Err(e) = result {
        super::emit_event(
            app,
            "sidecar://error",
            crate::sidecar::startup_error::SidecarErrorPayload::Raw {
                message: format!("モデルの読み込みに失敗しました: {e}"),
            },
        );
    }
}

/// Connect to the LOCAL sidecar's jobworkerp at `jobworkerp_url`, hot-reload
/// the LLM worker to `new`, and on ANY failure roll back to a working state:
/// run `restore_files` (caller-supplied — the single command restores just
/// the LLM settings file, the batch command restores the whole batch +
/// Keychain), re-apply the OLD worker so the user keeps a usable model, and
/// surface the failure both as a `sidecar://error` event (so the UI's error
/// rail catches it regardless of entry point) and as the returned `Err`.
///
/// Extracted so the single-command (`set_llm_settings`) and batch
/// (`apply_settings`) hot-reload paths share ONE copy of the connect →
/// reload → rollback sequence and its user-facing error string, instead of
/// the two near-identical blocks that drifted (one emitted the error event,
/// the other did not).
///
/// `restore_files` is `FnMut` so a failure can call it once for the
/// reload-failure path; the connect-failure path also calls it before
/// returning. The api_key is intentionally NOT part of the rollback (a
/// successful key write is kept — reverting it would surprise the user more
/// than leaving it) — `restore_files` only touches the non-secret files.
pub(crate) async fn connect_and_reload_llm_with_rollback(
    app: &tauri::AppHandle,
    jobworkerp_url: &str,
    worker_yaml: &Path,
    new: &LlmSettings,
    old: &LlmSettings,
    mut restore_files: impl FnMut(),
) -> AppResult<()> {
    // Connect directly to the local jobworkerp port (NOT `resolve_targets()`,
    // which would point at the remote URL in remote browse mode — the LLM
    // worker we mutate is always the LOCAL one).
    let handle = match crate::jobworkerp::JobworkerpHandle::connect(jobworkerp_url).await {
        Ok(h) => h,
        Err(e) => {
            // The settings file (and any Keychain key) is already written, but
            // we never reached the worker, so the RUNNING worker still serves
            // the OLD config. Roll the file back so the next launch doesn't
            // silently pick up an unapplied change.
            restore_files();
            return Err(e);
        }
    };

    match reload_llm_worker(&handle, worker_yaml, new, old).await {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!(error = %e, "LLM hot-reload failed; rolling back to previous settings");
            super::emit_event(
                app,
                "sidecar://error",
                crate::sidecar::startup_error::SidecarErrorPayload::Raw {
                    message: format!("LLM apply failed: {e}; rolling back to previous settings"),
                },
            );
            // Restore the on-disk settings then re-apply the OLD worker so the
            // user is left on a working model, not the half-applied new one.
            // Outgoing is `new` here: the failed attempt may have already
            // re-registered the new worker, so release its (possibly fresh)
            // static pool before reloading old.
            restore_files();
            if let Err(rollback_err) = reload_llm_worker(&handle, worker_yaml, old, new).await {
                tracing::error!(error = %rollback_err, "LLM rollback reload also failed");
            }
            Err(AppError::Config(format!(
                "LLMモデルの適用に失敗しました。元の設定にロールバックしました: {e}"
            )))
        }
    }
}

#[tauri::command]
pub async fn set_llm_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    req: SetLlmSettingsRequest,
) -> AppResult<()> {
    let path = state.data.llm_settings_path();
    let outcome = apply_llm_settings_to_disk(&path, req)?;

    if !outcome.reload_needed {
        return Ok(());
    }

    // Hot-reload is taken only for an External target whose provider key env
    // the child was already spawned with (see `hot_reload_safe`). Local targets
    // always restart so process exit releases any Metal-backed pages.
    // Hot-reload only when the change is safe AND a live local sidecar exists;
    // otherwise (unsafe change, or first-launch with no sidecar) fall back to
    // the full (re)start so the child re-injects its spawn-time env. Binding
    // `endpoints` with `let-else` here both gates the path and avoids a second
    // `current_endpoints()` lookup + an unreachable `expect` below.
    let spawned_key_env = state.sidecars.spawned_external_key_env();
    let endpoints = state.sidecars.current_endpoints();
    let Some(endpoints) = endpoints.filter(|_| outcome.hot_reload_safe(spawned_key_env.as_deref()))
    else {
        state.invalidate_clients().await;
        let sidecars = state.sidecars.clone();
        let data = state.data.clone();
        sidecars.stop().await?;
        crate::stage_and_start_sidecars(&app, &sidecars, &data).await;
        load_local_model_after_restart(&app, &sidecars, &outcome.new).await;
        return Ok(());
    };
    let worker_yaml = crate::data::paths::llm_workers_yaml()?;

    // Shared connect → reload → rollback pipeline. The single-command rollback
    // restores only this command's settings file (the batch path restores the
    // whole batch + Keychain via its own closure).
    let result = connect_and_reload_llm_with_rollback(
        &app,
        &endpoints.jobworkerp_url(),
        &worker_yaml,
        &outcome.new,
        &outcome.old,
        || {
            let _ = save_llm_settings(&path, &outcome.old);
        },
    )
    .await;
    // Drop the cached jobworkerp client on success: the released static pool
    // means the next dispatch should reconnect cleanly.
    if result.is_ok() {
        state.invalidate_clients().await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let s = load_llm_settings(&dir.path().join("nope.json"));
        assert_eq!(s, LlmSettings::default());
        assert_eq!(s.mode, LlmMode::Local);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llm-settings.json");
        let settings = LlmSettings {
            mode: LlmMode::External,
            provider_model: Some("gpt-4o".into()),
            base_url: None,
            max_tokens: Some(4000),
            temperature: Some(0.3),
            ..Default::default()
        };
        save_llm_settings(&path, &settings).unwrap();
        let back = load_llm_settings(&path);
        assert_eq!(settings, back);
    }

    #[test]
    fn serde_fills_defaults_for_missing_fields() {
        let back: LlmSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(back, LlmSettings::default());
    }

    #[test]
    fn serde_roundtrips_external() {
        let s = LlmSettings {
            mode: LlmMode::External,
            provider_model: Some("claude-sonnet-4-20250514".into()),
            base_url: Some("https://proxy.example.com/v1".into()),
            max_tokens: Some(8000),
            temperature: Some(0.7),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: LlmSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn load_corrupt_json_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llm-settings.json");
        std::fs::write(&path, b"{corrupt").unwrap();
        assert_eq!(load_llm_settings(&path), LlmSettings::default());
    }

    #[test]
    fn provider_env_var_openai_models() {
        assert_eq!(provider_env_var_for_model("gpt-4o", None), "OPENAI_API_KEY");
        assert_eq!(
            provider_env_var_for_model("gpt-4o-mini", None),
            "OPENAI_API_KEY"
        );
        assert_eq!(
            provider_env_var_for_model("o1-preview", None),
            "OPENAI_API_KEY"
        );
        assert_eq!(
            provider_env_var_for_model("o3-mini", None),
            "OPENAI_API_KEY"
        );
        assert_eq!(
            provider_env_var_for_model("o4-mini", None),
            "OPENAI_API_KEY"
        );
    }

    #[test]
    fn provider_env_var_anthropic() {
        assert_eq!(
            provider_env_var_for_model("claude-sonnet-4-20250514", None),
            "ANTHROPIC_API_KEY"
        );
        assert_eq!(
            provider_env_var_for_model("claude-3-opus-20240229", None),
            "ANTHROPIC_API_KEY"
        );
    }

    #[test]
    fn provider_env_var_gemini() {
        assert_eq!(
            provider_env_var_for_model("gemini-2.5-flash", None),
            "GEMINI_API_KEY"
        );
    }

    #[test]
    fn provider_env_var_other_providers() {
        assert_eq!(
            provider_env_var_for_model("deepseek-chat", None),
            "DEEPSEEK_API_KEY"
        );
        assert_eq!(
            provider_env_var_for_model("command-r-plus", None),
            "COHERE_API_KEY"
        );
        assert_eq!(
            provider_env_var_for_model("glm-4-plus", None),
            "ZAI_API_KEY"
        );
        assert_eq!(
            provider_env_var_for_model("groq::llama-3.3-70b", None),
            "GROQ_API_KEY"
        );
        assert_eq!(
            provider_env_var_for_model("xai::grok-3-mini", None),
            "XAI_API_KEY"
        );
        assert_eq!(
            provider_env_var_for_model("open_router::meta-llama/llama-3.3", None),
            "OPEN_ROUTER_API_KEY"
        );
    }

    #[test]
    fn provider_env_var_base_url_forces_openai() {
        assert_eq!(
            provider_env_var_for_model("anything", Some("https://my-proxy.com/v1")),
            "OPENAI_API_KEY"
        );
    }

    #[test]
    fn provider_env_var_empty_base_url_is_treated_as_unset() {
        // Regression: `set_llm_settings` may persist `base_url: Some("")`
        // and feeding that to the router silently re-pointed every model
        // at OPENAI_API_KEY — Gemini then reported "API key not valid"
        // against an empty key. Only a non-empty proxy URL should override
        // the prefix detection.
        assert_eq!(
            provider_env_var_for_model("gemini-3.1-flash-lite", Some("")),
            "GEMINI_API_KEY"
        );
        assert_eq!(
            provider_env_var_for_model("claude-sonnet-4-20250514", Some("")),
            "ANTHROPIC_API_KEY"
        );
    }

    #[test]
    fn provider_env_var_unknown_model_falls_back_to_openai() {
        assert_eq!(
            provider_env_var_for_model("unknown-model-xyz", None),
            "OPENAI_API_KEY"
        );
    }

    #[test]
    fn provider_display_name_covers_all_prefixes() {
        assert_eq!(provider_display_name("gpt-4o"), "OpenAI");
        assert_eq!(provider_display_name("claude-3-opus"), "Anthropic");
        assert_eq!(provider_display_name("gemini-2.5-flash"), "Google Gemini");
        assert_eq!(provider_display_name("deepseek-chat"), "DeepSeek");
        assert_eq!(provider_display_name("command-r-plus"), "Cohere");
        assert_eq!(provider_display_name("groq::llama"), "Groq");
        assert_eq!(provider_display_name("xai::grok"), "xAI");
        assert_eq!(provider_display_name("open_router::meta"), "OpenRouter");
        assert_eq!(provider_display_name("unknown"), "OpenAI-compatible");
    }

    /// Install the keyring crate's in-memory mock credential store so the
    /// keyring tests below never touch the real macOS Keychain — that would
    /// pop the system "let X access your keychain?" prompt on a developer
    /// machine and time out / fail in CI. `set_default_credential_builder`
    /// is process-global and one-shot, so guard with `Once` so concurrent
    /// or repeated invocations are no-ops.
    fn init_mock_keyring() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        // Reset on every call so the process-global cache cannot leak a
        // value written by an earlier test into this one. Tests run with
        // `--test-threads=1`, so racing writers aren't a concern.
        cache_clear();
    }

    #[test]
    fn delete_api_key_idempotent_when_no_entry() {
        // Calling delete twice (or on a Keychain that never held an entry)
        // must succeed — the goal state is "no stored key", which is
        // already satisfied. Runs against the mock store so CI / locked
        // login keychains don't gate the test on an interactive prompt.
        init_mock_keyring();
        let _ = delete_api_key();
        assert!(delete_api_key().is_ok());
    }

    #[test]
    fn delete_api_key_returns_ok_for_each_call() {
        // The mock store has no inter-Entry persistence (every
        // `Entry::new` returns a fresh MockCredential with no password),
        // so every `delete_api_key()` call hits the NoEntry path — which
        // must be reported as `Ok` so `purge_all_data`'s warning rail
        // stays silent for the "nothing to delete" case.
        init_mock_keyring();
        assert!(delete_api_key().is_ok());
        assert!(delete_api_key().is_ok());
    }

    #[test]
    fn save_api_key_empty_is_ok_when_no_entry_exists() {
        // `set_llm_settings` lifts `Some("")` into `save_api_key("")`.
        // With no credential stored, the underlying delete returns
        // `NoEntry`; we must report Ok so a fresh install that "saves"
        // the External card without ever entering a key doesn't spuriously
        // fail. Errors OTHER than NoEntry are propagated by the match arm
        // immediately above the Ok branch — exercised indirectly via the
        // delete_api_key tests, which share the same error mapping.
        init_mock_keyring();
        assert!(save_api_key("").is_ok());
    }

    #[test]
    fn load_api_key_returns_none_when_keychain_is_empty() {
        // Mirror of the delete test for the read path: with no stored
        // credential the function must return `None`, not propagate an
        // error — the call sites (`get_llm_settings`, `lifecycle.rs`)
        // unwrap `is_some()` and would otherwise mis-classify "no key
        // configured" as a failure.
        init_mock_keyring();
        assert!(load_api_key().is_none());
    }

    #[test]
    fn save_api_key_populates_cache_so_load_skips_keychain() {
        // Regression: the macOS "allow access?" prompt was firing twice on
        // every save (once from `set_password` → ACL touch, then again
        // when `lifecycle.rs::start_inner` called `load_api_key()` after
        // the sidecar restart). `save_api_key` must seed the cache from
        // the value it just wrote so the subsequent `load_api_key()`
        // returns it without re-reading the Keychain — the mock store has
        // no inter-Entry persistence, so a real Keychain round-trip would
        // come back `None` and prove the cache is in use.
        init_mock_keyring();
        save_api_key("sk-test-123").unwrap();
        assert_eq!(load_api_key().as_deref(), Some("sk-test-123"));
    }

    #[test]
    fn save_api_key_empty_clears_cache() {
        // `set_llm_settings` treats `Some("")` as an explicit delete. The
        // cache must follow so a subsequent `load_api_key()` reports
        // "no key" instead of returning the previously written value.
        init_mock_keyring();
        save_api_key("sk-test-123").unwrap();
        save_api_key("").unwrap();
        assert!(load_api_key().is_none());
    }

    #[test]
    fn delete_api_key_clears_cache() {
        // `purge_all_data` calls `delete_api_key()`; if the cache kept the
        // pre-purge value, an immediate `get_llm_settings()` would still
        // report `api_key_set: true` and the UI would mis-display the
        // post-purge state.
        init_mock_keyring();
        save_api_key("sk-test-123").unwrap();
        delete_api_key().unwrap();
        assert!(load_api_key().is_none());
    }

    #[test]
    fn load_api_key_caches_empty_result() {
        // First call may hit the Keychain (mock returns NoEntry); the
        // cached `None` must be visible on the second call so the sidecar
        // start path doesn't pop a second prompt for users who have not
        // configured an External LLM yet.
        init_mock_keyring();
        assert!(load_api_key().is_none());
        // Re-reading must still return `None` without touching the store
        // again — we can only check the value here, but the production
        // benefit is "no second prompt on a real Keychain".
        assert!(load_api_key().is_none());
    }

    #[test]
    fn classify_keyring_read_found_for_non_empty() {
        match classify_keyring_read(Ok("sk-abc".to_string())) {
            KeyringRead::Found(k) => assert_eq!(k, "sk-abc"),
            _ => panic!("a non-empty stored key must classify as Found"),
        }
    }

    #[test]
    fn classify_keyring_read_not_stored_for_empty_and_no_entry() {
        // An empty stored value and `NoEntry` are both "no key configured";
        // both are safe to cache for the process lifetime.
        assert!(matches!(
            classify_keyring_read(Ok(String::new())),
            KeyringRead::NotStored
        ));
        assert!(matches!(
            classify_keyring_read(Err(keyring::Error::NoEntry)),
            KeyringRead::NotStored
        ));
    }

    #[test]
    fn classify_keyring_read_transient_for_locked_or_denied() {
        // Regression: a locked Keychain / denied access prompt surfaces as
        // `PlatformFailure` / `NoStorageAccess`, NOT `NoEntry`. These must
        // classify as `Transient` so `load_api_key` does NOT cache the
        // resulting `None` — otherwise an unlock later in the session would
        // never be retried and the External LLM would stay unauthenticated
        // until an app relaunch.
        let platform = keyring::Error::PlatformFailure(
            Box::<dyn std::error::Error + Send + Sync>::from("keychain is locked"),
        );
        assert!(matches!(
            classify_keyring_read(Err(platform)),
            KeyringRead::Transient
        ));
        let denied = keyring::Error::NoStorageAccess(
            Box::<dyn std::error::Error + Send + Sync>::from("user denied access"),
        );
        assert!(matches!(
            classify_keyring_read(Err(denied)),
            KeyringRead::Transient
        ));
    }

    // ── Local LLM preset resolution & validation ─────────────────────

    #[test]
    fn load_settings_without_local_fields_returns_none_for_preset_id() {
        // `#[serde(default)]` must materialise omitted local fields as
        // `None` (NOT error), so sparse settings files keep loading.
        let s: LlmSettings = serde_json::from_str("{}").unwrap();
        assert!(s.local_preset_id.is_none());
        assert!(s.local_model_file.is_none());
        assert!(s.local_hf_repo.is_none());
        assert!(s.local_ctx_size.is_none());
        assert!(s.local_kv_cache_type.is_none());
        assert_eq!(resolve_kv_cache_type(&s), KvCacheType::Q4_0);
    }

    #[test]
    fn resolve_runtime_none_preset_returns_default_preset_values() {
        let s = LlmSettings::default();
        let rt = resolve_local_runtime(&s);
        let default_preset = llm_presets::default_preset();
        assert_eq!(rt.model_file, default_preset.gguf_file);
        assert_eq!(rt.hf_repo, default_preset.hf_repo);
        assert_eq!(rt.ctx_size, default_preset.recommended_ctx_size);
        assert_eq!(rt.thinking_kwarg, default_preset.thinking_kwarg);
        assert_eq!(rt.kv_cache_type, KvCacheType::Q4_0);
    }

    #[test]
    fn resolve_runtime_preset_id_returns_preset_values() {
        let s = LlmSettings {
            local_preset_id: Some("qwen3-5-9b-ud-q4-k-xl".into()),
            ..Default::default()
        };
        let rt = resolve_local_runtime(&s);
        let preset = llm_presets::find_preset("qwen3-5-9b-ud-q4-k-xl").unwrap();
        assert_eq!(rt.model_file, preset.gguf_file);
        assert_eq!(rt.hf_repo, preset.hf_repo);
        assert_eq!(rt.ctx_size, preset.recommended_ctx_size);
        assert_eq!(rt.thinking_kwarg, llm_presets::ThinkingKwarg::Disable);
        assert_eq!(rt.kv_cache_type, KvCacheType::Q4_0);
    }

    #[test]
    fn resolve_runtime_uses_user_kv_cache_type_override() {
        let s = LlmSettings {
            local_preset_id: Some("qwen3-5-9b-ud-q4-k-xl".into()),
            local_kv_cache_type: Some(KvCacheType::Q8_0),
            ..Default::default()
        };
        let rt = resolve_local_runtime(&s);
        assert_eq!(rt.kv_cache_type, KvCacheType::Q8_0);
    }

    #[test]
    fn resolve_kv_cache_type_with_env_honours_shell_when_unset() {
        let s = LlmSettings::default();
        let kv = resolve_kv_cache_type_with_env(&s, |name| match name {
            "LOOKBACK_LLM_KV_CACHE_TYPE" => Some("KV_CACHE_TYPE_Q8_0".into()),
            _ => None,
        });
        assert_eq!(kv, KvCacheType::Q8_0);
    }

    #[test]
    fn resolve_kv_cache_type_with_env_prefers_saved_setting() {
        let s = LlmSettings {
            local_kv_cache_type: Some(KvCacheType::Q5_1),
            ..Default::default()
        };
        let kv = resolve_kv_cache_type_with_env(&s, |name| match name {
            "LOOKBACK_LLM_KV_CACHE_TYPE" => Some("KV_CACHE_TYPE_Q8_0".into()),
            _ => None,
        });
        assert_eq!(kv, KvCacheType::Q5_1);
    }

    #[test]
    fn resolve_runtime_gemma4_preset_returns_enable() {
        // Regression for "Gemma 4 RAG tool calls never fire" — the kwarg
        // polarity here is the OPPOSITE of Qwen3. If this assertion ever
        // becomes `Disable` again, Gemma 4 will silently route every
        // tool call into the `<|channel>thought` prefix and the model
        // will answer "no records found" without ever calling
        // `lookback_recall`.
        let s = LlmSettings {
            local_preset_id: Some("gemma-4-26b-a4b-it-ud-iq4-nl".into()),
            ..Default::default()
        };
        let rt = resolve_local_runtime(&s);
        assert_eq!(rt.thinking_kwarg, llm_presets::ThinkingKwarg::Enable);
    }

    #[test]
    fn resolve_runtime_gemma4_e2b_preset_returns_artifact_values() {
        let s = LlmSettings {
            local_preset_id: Some("gemma-4-e2b-it-ud-q4-k-xl".into()),
            ..Default::default()
        };
        let rt = resolve_local_runtime(&s);
        assert_eq!(rt.model_file, "gemma-4-E2B-it-UD-Q4_K_XL.gguf");
        assert_eq!(rt.hf_repo, "unsloth/gemma-4-E2B-it-GGUF");
        assert_eq!(rt.ctx_size, 131_072);
        assert_eq!(rt.thinking_kwarg, llm_presets::ThinkingKwarg::Enable);
    }

    #[test]
    fn resolve_runtime_gemma4_e4b_uses_model_context_limit() {
        let s = LlmSettings {
            local_preset_id: Some("gemma-4-e4b-it-ud-q4-k-xl".into()),
            ..Default::default()
        };
        let rt = resolve_local_runtime(&s);
        assert_eq!(rt.ctx_size, 131_072);
        assert_eq!(rt.thinking_kwarg, llm_presets::ThinkingKwarg::Enable);
    }

    #[test]
    fn thinking_kwarg_for_uses_preset_when_user_selected() {
        // The shell env override is ignored once Settings is saved — the
        // file is authoritative for both the env triple AND the kwarg.
        let s = LlmSettings {
            local_preset_id: Some("gemma-4-26b-a4b-it-ud-iq4-nl".into()),
            ..Default::default()
        };
        let kwarg = thinking_kwarg_for(&s, |name| match name {
            "LOOKBACK_LLM_MODEL" => Some("Qwen3.5-9B-something.gguf".into()),
            _ => None,
        });
        assert_eq!(kwarg, llm_presets::ThinkingKwarg::Enable);
    }

    #[test]
    fn thinking_kwarg_for_drops_kwarg_when_env_overrides_unset_preset() {
        // Regression: a dev-env `LOOKBACK_LLM_MODEL` can route the sidecar
        // to ANY family (Qwen, Gemma 4, …) while `local_preset_id == None`
        // leaves the resolver on the default preset. The default kwarg
        // polarity may be wrong for the overridden family, so skip it
        // entirely; this matches the custom-path safety pin.
        let s = LlmSettings::default();
        let kwarg = thinking_kwarg_for(&s, |name| match name {
            "LOOKBACK_LLM_MODEL" => Some("gemma-4-26B-A4B-it-UD-IQ4_NL.gguf".into()),
            _ => None,
        });
        assert_eq!(kwarg, llm_presets::ThinkingKwarg::None);
    }

    #[test]
    fn thinking_kwarg_for_falls_back_to_default_preset_when_nothing_set() {
        // No saved preset, no env override → the sidecar is actually
        // running the default preset, so its policy is correct.
        let s = LlmSettings::default();
        let kwarg = thinking_kwarg_for(&s, |_| None);
        assert_eq!(kwarg, llm_presets::default_preset().thinking_kwarg);
    }

    #[test]
    fn thinking_kwarg_for_custom_path_is_none_even_without_env() {
        // Custom path always returns None — the resolver can't infer the
        // family from free-text gguf names. Pin this so a future change
        // to the env-override branch above doesn't accidentally fall
        // through to the default preset for the custom case.
        let s = LlmSettings {
            local_preset_id: Some(llm_presets::CUSTOM_PRESET_ID.into()),
            local_model_file: Some("Qwen3-something.gguf".into()),
            local_hf_repo: Some("unsloth/Qwen3-something".into()),
            ..Default::default()
        };
        let kwarg = thinking_kwarg_for(&s, |_| None);
        assert_eq!(kwarg, llm_presets::ThinkingKwarg::None);
    }

    #[test]
    fn resolve_runtime_unknown_preset_id_falls_back_to_default() {
        // Defensive: if a future version retires a preset id but the
        // user's settings still reference it, the resolver must degrade
        // gracefully to the default rather than producing an empty
        // model_file (which would make llama-cpp fail to load anything).
        let s = LlmSettings {
            local_preset_id: Some("retired-preset-id".into()),
            ..Default::default()
        };
        let rt = resolve_local_runtime(&s);
        assert_eq!(rt.model_file, llm_presets::default_preset().gguf_file);
    }

    #[test]
    fn resolve_runtime_custom_returns_user_fields() {
        let s = LlmSettings {
            local_preset_id: Some(llm_presets::CUSTOM_PRESET_ID.into()),
            local_model_file: Some("my-model.gguf".into()),
            local_hf_repo: Some("me/my-repo".into()),
            local_ctx_size: Some(8192),
            ..Default::default()
        };
        let rt = resolve_local_runtime(&s);
        assert_eq!(rt.model_file, "my-model.gguf");
        assert_eq!(rt.hf_repo, "me/my-repo");
        assert_eq!(rt.ctx_size, 8192);
    }

    #[test]
    fn resolve_local_llm_env_triple_uses_preset_when_user_selected() {
        // The regression: a Settings change to e.g. Qwen3.6-35B-A3B
        // must yield the 35B-A3B triple on the NEXT `start_inner`. The
        // shell env override is ignored once the user has picked a preset
        // (`local_preset_id == Some(...)`) so a leftover dev `LOOKBACK_LLM_*`
        // cannot drag a re-deployed Settings change back to the old model.
        let s = LlmSettings {
            local_preset_id: Some("qwen3-6-35b-a3b-ud-iq4-nl".into()),
            ..Default::default()
        };
        let preset = llm_presets::find_preset("qwen3-6-35b-a3b-ud-iq4-nl").unwrap();
        let (model, repo, ctx) = resolve_local_llm_env_triple(&s, |name| {
            // Even a shell env override must NOT win over a user-selected preset.
            match name {
                "LOOKBACK_LLM_MODEL" => Some("stale-shell-override.gguf".into()),
                "LOOKBACK_LLM_HF_REPO" => Some("stale/shell-override".into()),
                "LOOKBACK_LLM_CTX_SIZE" => Some("4096".into()),
                _ => None,
            }
        });
        assert_eq!(model.as_deref(), Some(preset.gguf_file));
        assert_eq!(repo.as_deref(), Some(preset.hf_repo));
        assert_eq!(ctx, Some(preset.recommended_ctx_size));
    }

    #[test]
    fn resolve_local_llm_env_triple_honours_shell_env_when_user_has_not_picked() {
        // Pre-feature users (no `local_preset_id`) can still drive the
        // sidecar from a dev shell env. Once they save a Settings choice,
        // the above test pins that the override is ignored.
        let s = LlmSettings::default();
        let (model, repo, ctx) = resolve_local_llm_env_triple(&s, |name| match name {
            "LOOKBACK_LLM_MODEL" => Some("dev-override.gguf".into()),
            "LOOKBACK_LLM_HF_REPO" => Some("dev/override".into()),
            "LOOKBACK_LLM_CTX_SIZE" => Some("8192".into()),
            _ => None,
        });
        assert_eq!(model.as_deref(), Some("dev-override.gguf"));
        assert_eq!(repo.as_deref(), Some("dev/override"));
        assert_eq!(ctx, Some(8192));
    }

    #[test]
    fn resolve_local_llm_env_triple_picks_up_updated_settings_file() {
        // Regression for the bug where a `set_llm_settings` restart kept
        // shipping the boot-time preset to `memories-llm`: the sidecar
        // restart path is now `load_llm_settings(path) ->
        // resolve_local_llm_env_triple(&settings, env_lookup)`, so a
        // settings file overwrite between two calls MUST surface the new
        // preset on the second call without recreating the resolver.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llm-settings.json");

        let old = LlmSettings {
            local_preset_id: Some(llm_presets::DEFAULT_PRESET_ID.into()),
            ..Default::default()
        };
        save_llm_settings(&path, &old).unwrap();
        let (old_model, _, _) = resolve_local_llm_env_triple(&load_llm_settings(&path), |_| None);
        assert_eq!(
            old_model.as_deref(),
            Some(llm_presets::default_preset().gguf_file)
        );

        let new = LlmSettings {
            local_preset_id: Some("qwen3-6-35b-a3b-ud-iq4-nl".into()),
            ..Default::default()
        };
        save_llm_settings(&path, &new).unwrap();
        let (new_model, new_repo, _) =
            resolve_local_llm_env_triple(&load_llm_settings(&path), |_| None);
        let preset = llm_presets::find_preset("qwen3-6-35b-a3b-ud-iq4-nl").unwrap();
        assert_eq!(new_model.as_deref(), Some(preset.gguf_file));
        assert_eq!(new_repo.as_deref(), Some(preset.hf_repo));
    }

    #[test]
    fn resolve_local_llm_env_triple_falls_back_to_default_preset_when_nothing_set() {
        // No user selection, no shell override → the default preset
        // values. Guards against an empty triple, which would let the YAML
        // placeholder's own fallback silently mask a missing wiring.
        let s = LlmSettings::default();
        let default_preset = llm_presets::default_preset();
        let (model, repo, ctx) = resolve_local_llm_env_triple(&s, |_| None);
        assert_eq!(model.as_deref(), Some(default_preset.gguf_file));
        assert_eq!(repo.as_deref(), Some(default_preset.hf_repo));
        assert_eq!(ctx, Some(default_preset.recommended_ctx_size));
    }

    #[test]
    fn resolve_runtime_custom_forces_thinking_kwarg_none() {
        // Safety pin: the custom path cannot infer the model family, so
        // sending the WRONG polarity (Qwen's `Disable` to a Gemma 4, or
        // Gemma's `Enable` to a Qwen3) is the exact failure mode that
        // breaks RAG tool calls. Custom MUST resolve to `None` — users
        // who need a specific polarity should pick the nearest curated
        // preset.
        let s = LlmSettings {
            local_preset_id: Some(llm_presets::CUSTOM_PRESET_ID.into()),
            local_model_file: Some("Qwen3-something.gguf".into()),
            local_hf_repo: Some("unsloth/Qwen3-something".into()),
            ..Default::default()
        };
        let rt = resolve_local_runtime(&s);
        assert_eq!(rt.thinking_kwarg, llm_presets::ThinkingKwarg::None);
    }

    #[test]
    fn resolve_runtime_user_ctx_size_overrides_preset_recommended() {
        let s = LlmSettings {
            local_preset_id: Some(llm_presets::DEFAULT_PRESET_ID.into()),
            local_ctx_size: Some(4096),
            ..Default::default()
        };
        let rt = resolve_local_runtime(&s);
        assert_eq!(rt.ctx_size, 4096);
    }

    #[test]
    fn resolve_runtime_custom_with_no_ctx_size_falls_back_to_262144() {
        // The custom path has no preset to lean on; fall back to the
        // historical default that matches the YAML's hardcoded
        // `LOOKBACK_LLM_CTX_SIZE:-262144`.
        let s = LlmSettings {
            local_preset_id: Some(llm_presets::CUSTOM_PRESET_ID.into()),
            local_model_file: Some("x.gguf".into()),
            local_hf_repo: Some("org/x".into()),
            local_kv_cache_type: Some(KvCacheType::Q5_1),
            ..Default::default()
        };
        let rt = resolve_local_runtime(&s);
        assert_eq!(rt.ctx_size, 262_144);
        assert_eq!(rt.kv_cache_type, KvCacheType::Q5_1);
    }

    // Validation: custom-mode required fields & format checks.

    fn custom_request_with(file: Option<&str>, repo: Option<&str>) -> SetLlmSettingsRequest {
        SetLlmSettingsRequest {
            mode: LlmMode::Local,
            provider_model: None,
            api_key: None,
            base_url: None,
            max_tokens: None,
            temperature: None,
            local_preset_id: Some(llm_presets::CUSTOM_PRESET_ID.into()),
            local_model_file: file.map(str::to_string),
            local_hf_repo: repo.map(str::to_string),
            local_ctx_size: None,
            local_kv_cache_type: None,
        }
    }

    #[test]
    fn validate_custom_accepts_valid_inputs() {
        let req = custom_request_with(Some("model.gguf"), Some("unsloth/Qwen3-8B-GGUF"));
        assert!(validate_custom_local_fields(&req).is_ok());
    }

    #[test]
    fn validate_custom_rejects_missing_repo() {
        let req = custom_request_with(Some("model.gguf"), None);
        assert!(validate_custom_local_fields(&req).is_err());
    }

    #[test]
    fn validate_custom_rejects_empty_repo() {
        let req = custom_request_with(Some("model.gguf"), Some("   "));
        assert!(validate_custom_local_fields(&req).is_err());
    }

    #[test]
    fn validate_custom_rejects_repo_without_slash() {
        let req = custom_request_with(Some("model.gguf"), Some("just-a-name"));
        assert!(validate_custom_local_fields(&req).is_err());
    }

    #[test]
    fn validate_custom_rejects_repo_with_two_slashes() {
        // Defensive: `org/name/extra` is not a valid HF identifier.
        let req = custom_request_with(Some("model.gguf"), Some("org/name/extra"));
        assert!(validate_custom_local_fields(&req).is_err());
    }

    #[test]
    fn validate_custom_rejects_repo_with_illegal_char() {
        // Space, `:` etc. are not in the HF allowed set.
        let req = custom_request_with(Some("model.gguf"), Some("org/na me"));
        assert!(validate_custom_local_fields(&req).is_err());
    }

    #[test]
    fn validate_custom_rejects_missing_model_file() {
        let req = custom_request_with(None, Some("org/name"));
        assert!(validate_custom_local_fields(&req).is_err());
    }

    #[test]
    fn validate_custom_rejects_non_gguf_extension() {
        let req = custom_request_with(Some("model.safetensors"), Some("org/name"));
        assert!(validate_custom_local_fields(&req).is_err());
    }

    #[test]
    fn validate_custom_accepts_uppercase_gguf_extension() {
        let req = custom_request_with(Some("Model.GGUF"), Some("org/name"));
        assert!(validate_custom_local_fields(&req).is_ok());
    }

    #[test]
    fn validate_ctx_size_accepts_none() {
        // Omitting the override = use preset default; must be accepted.
        assert!(validate_ctx_size(None).is_ok());
    }

    #[test]
    fn validate_ctx_size_rejects_below_min() {
        assert!(validate_ctx_size(Some(LOCAL_CTX_SIZE_MIN - 1)).is_err());
    }

    #[test]
    fn validate_ctx_size_rejects_above_max() {
        assert!(validate_ctx_size(Some(LOCAL_CTX_SIZE_MAX + 1)).is_err());
    }

    #[test]
    fn validate_ctx_size_accepts_boundary_values() {
        // Boundary inclusivity is part of the public contract — error
        // messages quote `[MIN, MAX]` so both endpoints must be valid.
        assert!(validate_ctx_size(Some(LOCAL_CTX_SIZE_MIN)).is_ok());
        assert!(validate_ctx_size(Some(LOCAL_CTX_SIZE_MAX)).is_ok());
    }

    #[test]
    fn is_valid_hf_repo_accepts_canonical_shapes() {
        assert!(is_valid_hf_repo("unsloth/Qwen3.6-27B-GGUF"));
        assert!(is_valid_hf_repo("bartowski/google_gemma-3-12b-it-GGUF"));
        assert!(is_valid_hf_repo("a/b"));
    }

    #[test]
    fn is_valid_hf_repo_rejects_malformed_shapes() {
        assert!(!is_valid_hf_repo(""));
        assert!(!is_valid_hf_repo("no-slash"));
        assert!(!is_valid_hf_repo("/leading-slash"));
        assert!(!is_valid_hf_repo("trailing-slash/"));
        assert!(!is_valid_hf_repo("two/slashes/here"));
        assert!(!is_valid_hf_repo("space in/name"));
    }

    #[test]
    fn save_and_load_roundtrips_local_preset_fields() {
        // Persistence: the new fields must travel through the JSON file
        // unchanged so a `set_llm_settings` followed by a process restart
        // yields the same runtime values.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llm-settings.json");
        let settings = LlmSettings {
            mode: LlmMode::Local,
            local_preset_id: Some("qwen3-5-9b-ud-q4-k-xl".into()),
            local_ctx_size: Some(16384),
            local_kv_cache_type: Some(KvCacheType::Q8_0),
            ..Default::default()
        };
        save_llm_settings(&path, &settings).unwrap();
        let back = load_llm_settings(&path);
        assert_eq!(
            back.local_preset_id.as_deref(),
            Some("qwen3-5-9b-ud-q4-k-xl")
        );
        assert_eq!(back.local_ctx_size, Some(16384));
        assert_eq!(back.local_kv_cache_type, Some(KvCacheType::Q8_0));
    }

    #[test]
    fn serde_ignores_unknown_extra_field() {
        // Forward-compat: a JSON written by a future version with extra
        // keys must still deserialise so a downgrade doesn't wipe the
        // user's recognised settings.
        let json = r#"{
            "mode": "local",
            "local_preset_id": "qwen3-5-9b-ud-q4-k-xl",
            "future_unknown_field": 42
        }"#;
        let s: LlmSettings = serde_json::from_str(json).unwrap();
        assert_eq!(s.local_preset_id.as_deref(), Some("qwen3-5-9b-ud-q4-k-xl"));
    }

    // ── apply_llm_settings_to_disk ────────────────────────────────────

    fn external_req(model: &str) -> SetLlmSettingsRequest {
        SetLlmSettingsRequest {
            mode: LlmMode::External,
            provider_model: Some(model.to_string()),
            api_key: None, // never touch the Keychain in unit tests
            base_url: None,
            max_tokens: None,
            temperature: None,
            local_preset_id: None,
            local_model_file: None,
            local_hf_repo: None,
            local_ctx_size: None,
            local_kv_cache_type: None,
        }
    }

    /// Build an outcome with explicit modes, key-change flag, and the new
    /// mode's provider model (only meaningful when `new_mode == External`).
    fn outcome_pm(
        old_mode: LlmMode,
        new_mode: LlmMode,
        key_changed: bool,
        new_provider_model: Option<&str>,
    ) -> LlmApplyOutcome {
        LlmApplyOutcome {
            reload_needed: true,
            old: LlmSettings {
                mode: old_mode,
                ..Default::default()
            },
            new: LlmSettings {
                mode: new_mode,
                provider_model: new_provider_model.map(str::to_string),
                ..Default::default()
            },
            key_changed,
        }
    }

    fn outcome(old_mode: LlmMode, new_mode: LlmMode, key_changed: bool) -> LlmApplyOutcome {
        outcome_pm(old_mode, new_mode, key_changed, None)
    }

    #[test]
    fn hot_reload_unsafe_for_local_target_without_key_change() {
        // Local always restarts because only process exit reliably tears down
        // a warmed llama.cpp Metal pool before another model allocation.
        assert!(!outcome(LlmMode::Local, LlmMode::Local, false).hot_reload_safe(None));
        assert!(
            !outcome(LlmMode::Local, LlmMode::Local, false).hot_reload_safe(Some("OPENAI_API_KEY"))
        );
        assert!(!outcome(LlmMode::External, LlmMode::Local, false).hot_reload_safe(None));
        assert!(!outcome(LlmMode::External, LlmMode::Local, true).hot_reload_safe(None));
    }

    #[test]
    fn hot_reload_unsafe_when_key_changes_regardless_of_mode() {
        // A key set/delete only reaches the child via a fresh spawn — never via
        // an in-place reload — so any key change forces a restart.
        assert!(!outcome(LlmMode::Local, LlmMode::Local, true).hot_reload_safe(None));
        assert!(
            !outcome_pm(LlmMode::External, LlmMode::External, true, Some("gpt-4o"))
                .hot_reload_safe(Some("OPENAI_API_KEY"))
        );
    }

    #[test]
    fn hot_reload_safe_for_local_to_external_when_child_has_matching_key_env() {
        // Local→External IS hot-reloadable when the running child was spawned
        // with the key env the new provider needs (e.g. it already had
        // OPENAI_API_KEY because a provider model was persisted at boot). The
        // caller then frees the outgoing memories-llm static pool.
        let out = outcome_pm(LlmMode::Local, LlmMode::External, false, Some("gpt-4o"));
        assert!(out.hot_reload_safe(Some("OPENAI_API_KEY")));
    }

    #[test]
    fn hot_reload_unsafe_for_local_to_external_when_child_has_no_key() {
        // A pure Local user (child spawned without any provider key) switching
        // to External must restart so the child injects the key at spawn.
        let out = outcome_pm(LlmMode::Local, LlmMode::External, false, Some("gpt-4o"));
        assert!(!out.hot_reload_safe(None));
    }

    #[test]
    fn hot_reload_unsafe_for_external_to_external_provider_switch_needing_other_key() {
        // OpenAI→Gemini changes the needed key env (OPENAI→GEMINI). The child
        // only carries OPENAI_API_KEY, so it must restart to inject GEMINI's.
        let out = outcome_pm(
            LlmMode::External,
            LlmMode::External,
            false,
            Some("gemini-2.5-flash"),
        );
        assert!(!out.hot_reload_safe(Some("OPENAI_API_KEY")));
    }

    #[test]
    fn hot_reload_safe_for_external_to_external_same_provider_family() {
        // gpt-4o→gpt-4o-mini both route to OPENAI_API_KEY, which the child
        // already has — so a same-provider model swap is hot-reloadable.
        let out = outcome_pm(
            LlmMode::External,
            LlmMode::External,
            false,
            Some("gpt-4o-mini"),
        );
        assert!(out.hot_reload_safe(Some("OPENAI_API_KEY")));
    }

    #[test]
    fn apply_llm_to_disk_external_change_without_child_key_is_not_hot_reload_safe() {
        // End-to-end through the real persist path: switching to an external
        // model is reload-needed; with NO key env on the child (None) it is NOT
        // hot-reload safe and lands on the restart path.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llm-settings.json");
        let out = apply_llm_settings_to_disk(&path, external_req("gpt-4o")).unwrap();
        assert!(out.reload_needed);
        assert!(!out.key_changed);
        assert!(
            !out.hot_reload_safe(None),
            "default(Local)→External with no child key must restart"
        );
        // …but WITH the matching key env already on the child, it IS safe.
        assert!(out.hot_reload_safe(Some("OPENAI_API_KEY")));
    }

    #[test]
    fn apply_llm_to_disk_local_model_change_requires_restart() {
        // A Local-mode preset swap is reload-needed but never hot-reload safe.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llm-settings.json");
        let req = SetLlmSettingsRequest {
            mode: LlmMode::Local,
            provider_model: None,
            api_key: None,
            base_url: None,
            max_tokens: None,
            temperature: None,
            local_preset_id: Some("qwen3-5-9b-ud-q4-k-xl".into()),
            local_model_file: None,
            local_hf_repo: None,
            local_ctx_size: None,
            local_kv_cache_type: None,
        };
        let out = apply_llm_settings_to_disk(&path, req).unwrap();
        assert!(out.reload_needed);
        assert!(!out.key_changed);
        assert!(!out.hot_reload_safe(None));
        assert!(!out.hot_reload_safe(Some("OPENAI_API_KEY")));
    }

    #[test]
    fn apply_llm_to_disk_reports_restart_on_external_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llm-settings.json");
        let outcome = apply_llm_settings_to_disk(&path, external_req("gpt-4o")).unwrap();
        assert!(
            outcome.reload_needed,
            "switching to a new external model needs a reload"
        );
        // The outcome carries both ends of the swap so the reload pipeline
        // can roll back: old is the pre-write default, new is what landed.
        assert_eq!(outcome.old, LlmSettings::default());
        assert_eq!(outcome.new.provider_model.as_deref(), Some("gpt-4o"));
        let saved = load_llm_settings(&path);
        assert_eq!(saved.provider_model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn apply_llm_to_disk_noop_reports_no_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llm-settings.json");
        apply_llm_settings_to_disk(&path, external_req("gpt-4o")).unwrap();
        let outcome = apply_llm_settings_to_disk(&path, external_req("gpt-4o")).unwrap();
        assert!(
            !outcome.reload_needed,
            "re-saving the same value must not reload"
        );
    }

    #[test]
    fn reload_target_resolves_local_worker_name() {
        // Local maps to the pooled llama.cpp worker used by lazy chat loading.
        let t = reload_target_for_mode(LlmMode::Local);
        assert_eq!(t.name, "memories-llm");
        // Stays consistent with the dispatch-side worker name resolver.
        assert_eq!(t.name, worker_name_for(LlmMode::Local));
    }

    #[test]
    fn reload_target_resolves_external_worker_name() {
        // External is the genai worker: no resident model, so Release is
        // skipped (it would FailedPrecondition on a non-static worker).
        let t = reload_target_for_mode(LlmMode::External);
        assert_eq!(t.name, "memories-llm-external");
        assert_eq!(t.name, worker_name_for(LlmMode::External));
    }

    #[test]
    fn llm_load_timeout_matches_three_hour_dispatch_budget() {
        assert_eq!(LLM_LOAD_TIMEOUT_MS, 3 * 60 * 60 * 1000);
    }

    #[test]
    fn placeholder_vars_for_local_carry_the_triple_and_leave_external_unmapped() {
        // The env-free replacement for the old `apply_llm_env` path: a Local
        // custom preset produces the LOOKBACK_LLM_* triple in the map and
        // leaves the External keys unmapped (so the YAML's own `:-default`
        // stays in effect) — and crucially WITHOUT touching the process env.
        let settings = LlmSettings {
            mode: LlmMode::Local,
            local_preset_id: Some(llm_presets::CUSTOM_PRESET_ID.to_string()),
            local_model_file: Some("my-model.gguf".to_string()),
            local_hf_repo: Some("acme/my-model-GGUF".to_string()),
            local_ctx_size: Some(8192),
            ..Default::default()
        };
        let vars = llm_placeholder_vars(&settings);
        assert_eq!(
            vars.get("LOOKBACK_LLM_MODEL").map(String::as_str),
            Some("my-model.gguf")
        );
        assert_eq!(
            vars.get("LOOKBACK_LLM_HF_REPO").map(String::as_str),
            Some("acme/my-model-GGUF")
        );
        assert_eq!(
            vars.get("LOOKBACK_LLM_CTX_SIZE").map(String::as_str),
            Some("8192")
        );
        assert_eq!(
            vars.get("LOOKBACK_LLM_KV_CACHE_TYPE").map(String::as_str),
            Some("KV_CACHE_TYPE_Q4_0")
        );
        // No provider model ⇒ External keys absent ⇒ YAML default (gpt-4o) wins.
        assert!(!vars.contains_key("LOOKBACK_EXTERNAL_LLM_MODEL"));
        assert!(!vars.contains_key("LOOKBACK_EXTERNAL_LLM_BASE_URL"));
    }

    #[test]
    fn placeholder_vars_for_external_carry_model_and_base_url() {
        let settings = LlmSettings {
            mode: LlmMode::External,
            provider_model: Some("claude-3-5-sonnet".to_string()),
            base_url: Some("https://proxy.example/v1".to_string()),
            ..Default::default()
        };
        let vars = llm_placeholder_vars(&settings);
        assert_eq!(
            vars.get("LOOKBACK_EXTERNAL_LLM_MODEL").map(String::as_str),
            Some("claude-3-5-sonnet")
        );
        assert_eq!(
            vars.get("LOOKBACK_EXTERNAL_LLM_BASE_URL")
                .map(String::as_str),
            Some("https://proxy.example/v1")
        );
    }

    #[test]
    fn placeholder_vars_omit_empty_base_url_so_yaml_default_wins() {
        // A reverted (None) or empty base_url must NOT be mapped — leaving it
        // unmapped lets the YAML's empty `:-` default through, which the genai
        // runner treats as "unset" (auto-detection restored). The old code
        // achieved this by clearing the env var; we now simply omit the key.
        let settings = LlmSettings {
            mode: LlmMode::External,
            provider_model: Some("gpt-4o".to_string()),
            base_url: None,
            ..Default::default()
        };
        let vars = llm_placeholder_vars(&settings);
        assert!(!vars.contains_key("LOOKBACK_EXTERNAL_LLM_BASE_URL"));
        // And an explicitly empty string is treated the same as None.
        let settings_empty = LlmSettings {
            base_url: Some(String::new()),
            ..settings
        };
        assert!(
            !llm_placeholder_vars(&settings_empty).contains_key("LOOKBACK_EXTERNAL_LLM_BASE_URL")
        );
    }

    #[test]
    fn resolve_placeholders_substitutes_mapped_value_over_default() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("LOOKBACK_LLM_MODEL".to_string(), "chosen.gguf".to_string());
        let out = resolve_yaml_placeholders("model: %{LOOKBACK_LLM_MODEL:-fallback.gguf}\n", &vars);
        assert_eq!(out, "model: chosen.gguf\n");
    }

    #[test]
    fn resolve_placeholders_uses_inline_default_when_unmapped() {
        let vars = std::collections::HashMap::new();
        let out = resolve_yaml_placeholders("ctx: %{LOOKBACK_LLM_CTX_SIZE:-262144}\n", &vars);
        assert_eq!(out, "ctx: 262144\n");
    }

    #[test]
    fn resolve_placeholders_empty_inline_default_yields_empty() {
        // `%{NAME:-}` with no mapped value resolves to the empty string,
        // matching expand_env's behaviour for the base_url placeholder.
        let vars = std::collections::HashMap::new();
        let out =
            resolve_yaml_placeholders("url: \"%{LOOKBACK_EXTERNAL_LLM_BASE_URL:-}\"\n", &vars);
        assert_eq!(out, "url: \"\"\n");
    }

    #[test]
    fn resolve_placeholders_leaves_unknown_without_default_intact() {
        // A name with neither a mapped value nor an inline default is left
        // verbatim so the downstream expand_env decides (resolve or reject).
        let vars = std::collections::HashMap::new();
        let out = resolve_yaml_placeholders("host: %{MEMORY_GRPC_HOST}\n", &vars);
        assert_eq!(out, "host: %{MEMORY_GRPC_HOST}\n");
    }

    #[test]
    fn resolve_placeholders_leaves_non_env_braces_untouched() {
        // A lowercase / non-grammar brace pair is not a placeholder; emit it
        // literally rather than swallowing it.
        let vars = std::collections::HashMap::new();
        let out = resolve_yaml_placeholders("liquid: $${ user.name }\n", &vars);
        assert_eq!(out, "liquid: $${ user.name }\n");
    }

    #[test]
    fn resolve_placeholders_resolves_all_llm_yaml_placeholders_without_env() {
        // End-to-end against the real committed LLM workers YAML: after
        // resolution there must be NO `%{LOOKBACK_*}` placeholder left, so
        // the downstream expand_env has nothing to look up in the process env.
        // (REUSE_KV_PREFIX is deliberately left to its YAML default.)
        let yaml_path =
            crate::data::paths::llm_workers_yaml().expect("committed YAML must resolve");
        let raw = std::fs::read_to_string(&yaml_path).expect("read committed YAML");
        let settings = LlmSettings {
            mode: LlmMode::Local,
            local_preset_id: Some("qwen3-5-9b-ud-q4-k-xl".into()),
            ..Default::default()
        };
        let resolved = resolve_yaml_placeholders(&raw, &llm_placeholder_vars(&settings));
        assert!(
            !resolved.contains("%{LOOKBACK_LLM_MODEL")
                && !resolved.contains("%{LOOKBACK_LLM_HF_REPO")
                && !resolved.contains("%{LOOKBACK_LLM_CTX_SIZE")
                && !resolved.contains("%{LOOKBACK_LLM_KV_CACHE_TYPE")
                && !resolved.contains("%{LOOKBACK_EXTERNAL_LLM_MODEL"),
            "all LLM placeholders with defaults must be resolved env-free"
        );
        // The preset's gguf must appear in the resolved text.
        let preset = llm_presets::find_preset("qwen3-5-9b-ud-q4-k-xl").unwrap();
        assert!(resolved.contains(preset.gguf_file));
    }

    #[test]
    fn apply_llm_to_disk_rejects_invalid_custom_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("llm-settings.json");
        // Custom local preset with a non-.gguf file is invalid.
        let req = custom_request_with(Some("model.bin"), Some("unsloth/Qwen3-8B-GGUF"));
        assert!(apply_llm_settings_to_disk(&path, req).is_err());
        assert!(
            !path.exists(),
            "an invalid request must not persist a settings file"
        );
    }
}
