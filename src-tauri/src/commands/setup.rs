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
use super::apply_settings::{
    ApplySettingsRequest, persist_settings_without_restart_approved_with_env,
    persist_settings_without_restart_with_env,
};
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
    data: &crate::data::DataPaths,
    mut settings: ApplySettingsRequest,
    _preferred_language: Option<&str>,
) -> ApplySettingsRequest {
    settings.llm.get_or_insert(SetLlmSettingsRequest {
        mode: LlmMode::Local,
        provider_model: None,
        api_key: None,
        base_url: None,
        max_tokens: None,
        model_max_output_tokens: None,
        temperature: None,
        local_preset_id: None,
        local_model_file: None,
        local_hf_repo: None,
        local_ctx_size: None,
        local_kv_cache_type: None,
        summary_output_guidance: None,
    });
    // The service has no migration API; setup must retain its current embedding space.
    let current =
        super::embedding_settings::load_embedding_settings(&data.embedding_settings_path());
    settings
        .embedding
        .get_or_insert(SetEmbeddingSettingsRequest {
            preset_id: current.preset_id,
            custom_model_id: current.custom_model_id,
            custom_tokenizer_id: current.custom_tokenizer_id,
            custom_vector_size: current.custom_vector_size,
            custom_dtype: current.custom_dtype,
            custom_max_sequence_length: current.custom_max_sequence_length,
            custom_is_multimodal: current.custom_is_multimodal,
            model_revision: current.model_revision,
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
        thread_created_after: None,
        thread_created_before: None,
        thread_updated_after: None,
        thread_updated_before: None,
        first_message_after: None,
        first_message_before: None,
        last_message_after: None,
        last_message_before: None,
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
    super::embedding_management::recover_embedding_journal_after_startup(&state).await?;
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
    super::embedding_space::load_embedding_worker(state, Some(MODEL_LOAD_TIMEOUT_MS)).await?;
    // External LLM mode has no local model to download, so only Local needs a
    // preload. The worker name comes from the same single-source-of-truth as
    // the chat/workflow dispatch sites.
    if state.llm_settings_snapshot().mode == LlmMode::Local {
        state
            .jobworkerp()
            .await?
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
    let target = req
        .data_root
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state.data.root.clone());

    if target != state.data.root {
        let _embedding_recovery = state.database_migration_recovery.lock().await;
        let _dispatch_gate = state.lock_llm_settings_dispatch_gate().await;
        super::app_settings::create_data_root(target.display().to_string())?;
        let target_data = DataPaths::with_root(&target);
        target_data.ensure()?;
        let settings = with_setup_defaults(
            &target_data,
            req.settings,
            req.preferred_language.as_deref(),
        );
        if let Some(embedding) = settings.embedding.as_ref()
            && super::connection::load_connection_config(&target_data.connection_config_path())
                .memories
                .is_local()
        {
            let approval = super::embedding_management::approve_embedding_settings_change_for_data(
                &state.sidecars,
                &target_data,
                embedding,
            )
            .await?;
            persist_settings_without_restart_approved_with_env(
                &target_data,
                settings,
                approval.permit,
                approval.rebuild_pending,
                |key| state.sidecars.embedding_env_lookup(key),
            )?;
        } else {
            persist_settings_without_restart_with_env(&target_data, settings, |key| {
                state.sidecars.embedding_env_lookup(key)
            })?;
        }
        super::app_settings::set_data_root(Some(target.display().to_string()))?;
        save_setup_completed(false)?;
        return Ok(ApplySetupResponse {
            restart_required: true,
        });
    }

    let settings =
        with_setup_defaults(&state.data, req.settings, req.preferred_language.as_deref());
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
pub fn restart_for_setup(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    restart_with_external_embedding_inputs(app, &state)
}

fn restart_with_external_embedding_inputs(app: AppHandle, state: &AppState) -> ! {
    state.sidecars.restore_embedding_inputs_for_relaunch();
    app.restart()
}

/// Leave an unrecoverable legacy data root untouched and start the first-run
/// flow against a separately selected empty directory. This is intentionally
/// stricter than the Settings data-root picker: recovery must never point at
/// another populated root by accident.
#[tauri::command]
pub fn start_fresh_setup(
    path: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let target = PathBuf::from(path.trim());
    validate_fresh_data_root(&target)?;
    super::app_settings::set_data_root(Some(target.display().to_string()))?;
    save_setup_completed(false)?;
    restart_with_external_embedding_inputs(app, &state)
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
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path());
        let settings = with_setup_defaults(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        );
        assert!(settings.llm.is_some());
        assert!(settings.embedding.is_some());
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
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path());
        let settings = with_setup_defaults(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: Some(explicit),
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        );
        assert!(matches!(
            settings.hf_home.unwrap().mode,
            paths::HfHomeMode::DataRoot
        ));
    }

    #[test]
    fn setup_defaults_are_loaded_from_selected_target_root() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let source_data = crate::data::DataPaths::with_root(source.path());
        let target_data = crate::data::DataPaths::with_root(target.path());
        super::super::embedding_settings::save_embedding_settings(
            &source_data.embedding_settings_path(),
            &super::super::embedding_settings::EmbeddingSettings {
                preset_id: Some("source-model".into()),
                ..Default::default()
            },
        )
        .unwrap();
        super::super::embedding_settings::save_embedding_settings(
            &target_data.embedding_settings_path(),
            &super::super::embedding_settings::EmbeddingSettings {
                preset_id: Some("target-model".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let settings = with_setup_defaults(
            &target_data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        );

        assert_eq!(
            settings.embedding.unwrap().preset_id.as_deref(),
            Some("target-model")
        );
    }

    #[test]
    fn setup_defaults_preserve_current_space_for_japanese_first_run() {
        let tmp = tempfile::tempdir().unwrap();
        let data = crate::data::DataPaths::with_root(tmp.path());
        let settings = with_setup_defaults(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            Some("ja"),
        );
        let outcome = super::super::embedding_settings::apply_embedding_settings_to_disk(
            &data,
            &settings.embedding.unwrap(),
        )
        .expect("Japanese setup must not require a storage migration");
        let expected = super::super::embedding_settings::resolve_embedding_runtime_with_env(
            &super::super::embedding_settings::EmbeddingSettings::default(),
            super::super::process_env_lookup,
        );
        assert_eq!(outcome.new_runtime, expected);
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
