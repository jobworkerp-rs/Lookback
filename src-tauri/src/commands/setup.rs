use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use tokio_stream::StreamExt;

use crate::data::paths::{
    self, DataPaths, bootstrap_path, has_existing_setup_evidence, load_bootstrap_snapshot,
    save_bootstrap_config,
};
use crate::error::AppResult;
use crate::grpc::proto::llm_memory::data::UserId;
use crate::grpc::proto::llm_memory::service::FindThreadListByUserIdRequest;
use crate::grpc::proto::llm_memory::service::thread_service_client::ThreadServiceClient;

use super::AppState;
use super::app_settings::SetHfHomeRequest;
use super::apply_settings::{ApplySettingsRequest, persist_settings_without_restart};
use super::embedding_settings::SetEmbeddingSettingsRequest;
use super::llm_settings::LlmMode;
use super::llm_settings::SetLlmSettingsRequest;

const MODEL_LOAD_TIMEOUT_MS: u64 = 3_600_000;
const FORCE_SETUP_ENV: &str = "LOOKBACK_FORCE_SETUP_WIZARD";

#[derive(Debug, Clone, Serialize)]
pub struct SetupStatus {
    pub required: bool,
    pub resume_apply: bool,
    pub current_data_root: String,
    pub default_data_root: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplySetupRequest {
    pub data_root: Option<String>,
    #[serde(default)]
    pub preferred_language: Option<String>,
    pub settings: ApplySettingsRequest,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplySetupResponse {
    pub restart_required: bool,
}

fn save_setup_completed(value: bool) -> AppResult<()> {
    let path = bootstrap_path()?;
    let mut config = paths::load_bootstrap_config(&path);
    config.setup_completed = value;
    save_bootstrap_config(&path, &config)
}

fn env_flag_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn force_setup_wizard() -> bool {
    env_flag_enabled(std::env::var(FORCE_SETUP_ENV).ok().as_deref())
}

fn with_setup_defaults(
    mut settings: ApplySettingsRequest,
    preferred_language: Option<&str>,
) -> ApplySettingsRequest {
    settings.llm.get_or_insert(SetLlmSettingsRequest {
        mode: LlmMode::Local,
        provider_model: None,
        api_key: None,
        base_url: None,
        max_tokens: None,
        temperature: None,
        local_preset_id: None,
        local_model_file: None,
        local_hf_repo: None,
        local_ctx_size: None,
        local_kv_cache_type: None,
    });
    settings
        .embedding
        .get_or_insert_with(|| SetEmbeddingSettingsRequest {
            preset_id: Some(
                super::embedding_presets::default_preset_for_language(preferred_language)
                    .id
                    .into(),
            ),
            custom_model_id: None,
            custom_tokenizer_id: None,
            custom_vector_size: None,
            custom_dtype: None,
            custom_max_sequence_length: None,
            custom_is_multimodal: None,
            evacuate_vectordb: false,
        });
    settings.hf_home.get_or_insert(SetHfHomeRequest {
        mode: paths::HfHomeMode::Global,
        path: None,
    });
    settings
}

async fn has_existing_threads(state: &AppState) -> bool {
    let Ok(channel) = state.memories_channel().await else {
        return false;
    };
    let mut client = ThreadServiceClient::new(channel);
    let request = FindThreadListByUserIdRequest {
        user_id: Some(UserId { value: 1 }),
        limit: Some(1),
        offset: None,
        created_after: None,
        created_before: None,
        updated_after: None,
        updated_before: None,
        sort: None,
        memory_kinds: vec![crate::grpc::proto::llm_memory::data::MemoryKind::Raw as i32],
    };
    let Ok(response) = client.find_thread_list_by_user_id(request).await else {
        return false;
    };
    response.into_inner().next().await.is_some()
}

#[tauri::command]
pub async fn get_setup_status(state: State<'_, AppState>) -> AppResult<SetupStatus> {
    let path = bootstrap_path()?;
    let snapshot = load_bootstrap_snapshot(&path);
    let mut completed = snapshot.config.setup_completed;
    if !snapshot.setup_completed_present
        && (has_existing_setup_evidence(&state.data) || has_existing_threads(&state).await)
    {
        let mut config = snapshot.config.clone();
        config.setup_completed = true;
        save_bootstrap_config(&path, &config)?;
        completed = true;
    }
    let forced = force_setup_wizard();
    let resume_apply = !forced
        && !completed
        && snapshot.setup_completed_present
        && has_existing_setup_evidence(&state.data);
    Ok(SetupStatus {
        required: forced || !completed,
        resume_apply,
        current_data_root: state.data.root.display().to_string(),
        default_data_root: paths::default_root()?.display().to_string(),
    })
}

async fn load_setup_models(state: &AppState) -> AppResult<()> {
    let handle = state.jobworkerp().await?;
    handle
        .load_worker(
            &crate::jobworkerp::embedding::embed_worker_name(),
            Some(MODEL_LOAD_TIMEOUT_MS),
        )
        .await?;
    // External LLM mode has no local model to download, so only Local needs a
    // preload. The worker name comes from the same single-source-of-truth as
    // the chat/workflow dispatch sites.
    if state.llm_settings_snapshot().mode == LlmMode::Local {
        handle
            .load_worker(state.active_llm_worker_name(), Some(MODEL_LOAD_TIMEOUT_MS))
            .await?;
    }
    Ok(())
}

#[tauri::command]
pub async fn apply_setup(
    app: AppHandle,
    state: State<'_, AppState>,
    req: ApplySetupRequest,
) -> AppResult<ApplySetupResponse> {
    let settings = with_setup_defaults(req.settings, req.preferred_language.as_deref());
    let target = req
        .data_root
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state.data.root.clone());

    if target != state.data.root {
        super::app_settings::create_data_root(target.display().to_string())?;
        let target_data = DataPaths::with_root(&target);
        target_data.ensure()?;
        persist_settings_without_restart(&target_data, settings)?;
        super::app_settings::set_data_root(Some(target.display().to_string()))?;
        save_setup_completed(false)?;
        return Ok(ApplySetupResponse {
            restart_required: true,
        });
    }

    super::apply_settings::apply_settings(app, state.clone(), settings).await?;
    load_setup_models(&state).await?;
    save_setup_completed(true)?;
    Ok(ApplySetupResponse {
        restart_required: false,
    })
}

#[tauri::command]
pub async fn resume_setup(state: State<'_, AppState>) -> AppResult<()> {
    load_setup_models(&state).await?;
    save_setup_completed(true)
}

#[tauri::command]
pub fn restart_for_setup(app: AppHandle) -> AppResult<()> {
    app.restart();
}

/// Leave an unrecoverable legacy data root untouched and start the first-run
/// flow against a separately selected empty directory. This is intentionally
/// stricter than the Settings data-root picker: recovery must never point at
/// another populated root by accident.
#[tauri::command]
pub fn start_fresh_setup(path: String, app: AppHandle) -> AppResult<()> {
    let target = PathBuf::from(path.trim());
    validate_fresh_data_root(&target)?;
    super::app_settings::set_data_root(Some(target.display().to_string()))?;
    save_setup_completed(false)?;
    app.restart();
}

fn validate_fresh_data_root(target: &std::path::Path) -> AppResult<()> {
    if !target.is_absolute() || !target.is_dir() || !paths::is_writable(target) {
        return Err(crate::error::AppError::Config(
            "new data root must be an existing writable directory".into(),
        ));
    }
    if target.read_dir()?.next().transpose()?.is_some() {
        return Err(crate::error::AppError::Config(
            "new data root must be empty to preserve the unrecoverable migration evidence".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_defaults_materialise_every_setting() {
        let settings = with_setup_defaults(
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: None,
            },
            None,
        );
        assert!(settings.llm.is_some());
        assert_eq!(
            settings.embedding.unwrap().preset_id.as_deref(),
            Some(super::super::embedding_presets::DEFAULT_EMBEDDING_PRESET_ID)
        );
        assert!(matches!(
            settings.hf_home.unwrap().mode,
            paths::HfHomeMode::Global
        ));
    }

    #[test]
    fn setup_defaults_preserve_explicit_choices() {
        let explicit = SetHfHomeRequest {
            mode: paths::HfHomeMode::DataRoot,
            path: None,
        };
        let settings = with_setup_defaults(
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: Some(explicit),
                mcp: None,
                timezone: None,
            },
            None,
        );
        assert!(matches!(
            settings.hf_home.unwrap().mode,
            paths::HfHomeMode::DataRoot
        ));
    }

    #[test]
    fn setup_defaults_use_ruri_only_for_japanese_first_run() {
        let settings = with_setup_defaults(
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: None,
            },
            Some("ja"),
        );
        assert_eq!(
            settings.embedding.unwrap().preset_id.as_deref(),
            Some("ruri-v3-310m-onnx-int8")
        );
    }

    #[test]
    fn force_setup_env_accepts_common_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "on", " on "] {
            assert!(
                env_flag_enabled(Some(value)),
                "{value} should enable the flag"
            );
        }
    }

    #[test]
    fn force_setup_env_rejects_missing_and_falsey_values() {
        for value in [None, Some(""), Some("0"), Some("false"), Some("off")] {
            assert!(!env_flag_enabled(value));
        }
    }

    #[test]
    fn fresh_data_root_requires_an_empty_writable_directory() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(validate_fresh_data_root(tmp.path()).is_ok());
        std::fs::write(tmp.path().join("existing-evidence"), "keep").unwrap();
        assert!(
            validate_fresh_data_root(tmp.path())
                .unwrap_err()
                .to_string()
                .contains("must be empty")
        );
    }
}
