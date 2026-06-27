//! Unified settings save (FR-CONFIG: batch apply).
//!
//! The individual `set_llm_settings` / `set_embedding_settings` /
//! `set_hf_home` commands each persist their file AND restart the sidecar.
//! Changing several at once therefore restarts the sidecar once per card,
//! and each restart can pull multi-GB models — slow and wasteful.
//!
//! `apply_settings` persists every provided setting to disk first, then
//! restarts the sidecar EXACTLY ONCE. This is safe because
//! `Sidecars::start_inner` re-reads all three files on every (re)start
//! (LLM / embedding runtime / HF_HOME), so a single restart picks up the
//! whole batch.
//!
//! Consistency: the WHOLE batch is validated (every present request) before
//! any file is written, so a later request's validation failure cannot
//! leave an earlier one half-saved on disk. On a restart failure every
//! persisted file is rolled back to its previous value — including the
//! Keychain API key, which `apply_llm_settings_to_disk` may have rewritten
//! — and the LanceDB backup (if one was taken) is restored, then the
//! sidecar is brought back up on the old config.

use serde::{Deserialize, Serialize};
use tauri::Emitter;

use crate::error::AppResult;

use super::AppState;
use super::app_settings::{SetHfHomeRequest, apply_hf_home_to_disk, validate_hf_home_request};
use super::embedding_settings::{
    EmbeddingRuntime, EmbeddingSettings, EvacuateMode, SetEmbeddingSettingsRequest,
    apply_embedding_settings_to_disk, restore_vectordb_backup, save_embedding_settings,
    validate_embedding_request,
};
use super::llm_settings::{
    LlmSettings, SetLlmSettingsRequest, apply_llm_settings_to_disk, load_api_key,
    load_llm_settings, restore_api_key, save_llm_settings, validate_llm_request,
};
use super::mcp_settings::{
    McpSettings, SetMcpSettingsRequest, apply_mcp_settings_to_disk, save_mcp_settings,
    validate_mcp_request,
};
use crate::data::paths;

/// Each field present ⇒ that setting should be saved. Absent fields are
/// left untouched.
#[derive(Debug, Clone, Deserialize)]
pub struct ApplySettingsRequest {
    #[serde(default)]
    pub llm: Option<SetLlmSettingsRequest>,
    #[serde(default)]
    pub embedding: Option<SetEmbeddingSettingsRequest>,
    #[serde(default)]
    pub hf_home: Option<SetHfHomeRequest>,
    #[serde(default)]
    pub mcp: Option<SetMcpSettingsRequest>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplySettingsResponse {
    /// `false` ⇒ nothing meaningful changed, so the sidecar was left
    /// running untouched.
    pub restarted: bool,
    /// `Some(_)` when an embedding dimension change evacuated the LanceDB.
    pub backup_path: Option<String>,
    /// The embedding runtime that is now in effect (only when an embedding
    /// change was part of the batch).
    pub embedding_runtime: Option<EmbeddingRuntime>,
    /// Plugin-staging warnings surfaced by the restart, if any.
    pub warnings: Vec<crate::sidecar::SidecarWarning>,
}

/// Snapshot of the previous on-disk state for each setting we touched, so
/// a failed restart can be rolled back.
#[derive(Default)]
struct RollbackState {
    llm: Option<LlmSettings>,
    /// Outer `Some` ⇒ the batch changed the Keychain API key, so it must be
    /// restored on rollback. Inner `Some(key)` is the old key; inner `None`
    /// means there was no stored key before (rollback deletes it).
    llm_api_key: Option<Option<String>>,
    embedding: Option<EmbeddingSettings>,
    hf_home_changed: bool,
    hf_home_old: Option<paths::AppSettings>,
    mcp: Option<McpSettings>,
}

/// Result of persisting the batch to disk (Phase 1). Carries everything
/// Phase 2 (the single restart) needs.
struct PersistOutcome {
    rollback: RollbackState,
    restart_needed: bool,
    needs_vectordb_reset: bool,
    evacuate_mode: EvacuateMode,
    embedding_runtime: Option<EmbeddingRuntime>,
    /// Per-card change flags. When ONLY `llm_changed` is set (no embedding /
    /// HF_HOME change), Phase 2 can hot-reload the LLM worker instead of
    /// restarting the sidecar — embedding and HF_HOME are the only changes
    /// that require a child-process restart (LanceDB dim / model-cache dir).
    llm_changed: bool,
    embedding_changed: bool,
    hf_home_changed: bool,
    /// MCP enable flag / advanced overrides changed. Always requires a full
    /// sidecar restart — `MCP_ENABLED` is read at jobworkerp spawn time, so
    /// there is no hot-reload path. Gates `is_llm_only_change` so a co-changed
    /// MCP toggle can never be split into the LLM-only in-place reload.
    mcp_changed: bool,
    /// Whether the LLM change (if any) can be applied via an in-place worker
    /// hot-reload rather than a sidecar restart. False for an External-side
    /// edit (mode switch / API key / provider / base_url), whose effect only
    /// reaches the running child through its spawn-time env — see
    /// [`crate::commands::llm_settings::LlmApplyOutcome::hot_reload_safe`].
    llm_hot_reload_safe: bool,
    /// The new LLM settings (for the hot-reload worker upsert) when the LLM
    /// card was part of the batch. `None` ⇒ no LLM change to reload.
    llm_new: Option<LlmSettings>,
}

impl PersistOutcome {
    /// True when the only restart-worthy change is an LLM change that is
    /// SAFE to hot-reload. Phase 2 uses this to apply the model swap in
    /// place (no sidecar restart). An External-side LLM edit (API key /
    /// provider / mode switch) sets `llm_changed` but NOT
    /// `llm_hot_reload_safe`, so it correctly falls through to the restart
    /// path where the child re-injects the key env at spawn.
    fn is_llm_only_change(&self) -> bool {
        self.llm_changed
            && self.llm_hot_reload_safe
            && !self.embedding_changed
            && !self.hf_home_changed
            && !self.mcp_changed
    }
}

/// Phase 1: validate + persist every provided setting to disk WITHOUT
/// touching the sidecar.
///
/// Embedding is processed FIRST so its remote-mode rejection / request
/// validation aborts the whole batch before any other file is written —
/// the consistency guarantee that a rejected embedding change never leaves
/// a half-saved LLM / HF_HOME change behind. Pure over `&DataPaths` so the
/// ordering contract is unit-testable without an `AppState`.
///
/// `spawned_key_env` is the provider key env the running jobworkerp child was
/// spawned with (`Sidecars::spawned_external_key_env`); it's threaded through
/// only to compute `llm_hot_reload_safe` for a switch INTO External (see
/// `LlmApplyOutcome::hot_reload_safe`). Tests pass `None`.
fn persist_batch_to_disk(
    data: &crate::data::DataPaths,
    req: ApplySettingsRequest,
    spawned_key_env: Option<&str>,
) -> AppResult<PersistOutcome> {
    let llm_path = data.llm_settings_path();
    let mut rollback = RollbackState::default();
    let mut restart_needed = false;
    let mut needs_vectordb_reset = false;
    let mut evacuate_mode = EvacuateMode::Evacuate;
    let mut embedding_runtime: Option<EmbeddingRuntime> = None;
    let mut llm_changed = false;
    let mut llm_hot_reload_safe = false;
    let mut embedding_changed = false;
    let mut hf_home_changed = false;
    let mut mcp_changed = false;
    let mut llm_new: Option<LlmSettings> = None;

    // ── Validate the WHOLE batch before writing anything ──
    // A later card's validation failure must not leave an earlier card's
    // change half-saved on disk (e.g. embedding persisted, then a bad
    // custom LLM .gguf name aborts — the next launch would pick up the
    // orphaned embedding change). Validate all present requests first.
    if let Some(emb_req) = req.embedding.as_ref() {
        validate_embedding_request(data, emb_req)?;
    }
    if let Some(llm_req) = req.llm.as_ref() {
        validate_llm_request(llm_req)?;
    }
    if let Some(hf_req) = req.hf_home.as_ref() {
        validate_hf_home_request(hf_req)?;
    }
    if let Some(mcp_req) = req.mcp.as_ref() {
        validate_mcp_request(mcp_req)?;
    }

    // ── Persist (validation already passed for every request) ──
    //
    // Each persist step writes a file (and, for LLM, the Keychain). A
    // failure midway through MUST undo the steps already applied — otherwise
    // the UI reports "apply failed" while the next launch picks up a
    // partially-applied batch. `rollback` accumulates the previous on-disk
    // state as each step succeeds; on any `?` we restore from it before
    // returning. (Embedding is first, so its own failure has nothing to undo
    // — `apply_embedding_settings_to_disk` only writes the file; the LanceDB
    // evacuation happens later in Phase 2.)
    if let Some(emb_req) = req.embedding.as_ref() {
        let outcome = apply_embedding_settings_to_disk(data, emb_req)?;
        embedding_runtime = Some(outcome.new_runtime.clone());
        if outcome.changed {
            restart_needed = true;
            embedding_changed = true;
            needs_vectordb_reset = outcome.needs_vectordb_reset;
            evacuate_mode = if emb_req.evacuate_vectordb {
                EvacuateMode::Evacuate
            } else {
                EvacuateMode::Delete
            };
            rollback.embedding = Some(outcome.old_settings);
        }
    }

    if let Some(llm_req) = req.llm {
        let old = load_llm_settings(&llm_path);
        // Capture the Keychain key BEFORE the apply rewrites it, so a
        // restart failure can restore the credential alongside the file.
        // Only when the request actually touches the key (Some = set/delete).
        let old_api_key = if llm_req.api_key.is_some() {
            Some(load_api_key())
        } else {
            None
        };
        match apply_llm_settings_to_disk(&llm_path, llm_req) {
            Ok(outcome) => {
                let changed = outcome.reload_needed;
                // `apply_llm_settings_to_disk` rewrites the settings file
                // UNCONDITIONALLY, even when `changed` is false (e.g. only
                // max_tokens / temperature — chat-only fields the sidecar
                // never reads — were edited). So the file is now at the new
                // value regardless of `changed`, and a LATER step's failure
                // (HF_HOME persist, or the Phase-2 restart) must be able to
                // roll THIS write back too. Record the rollback whenever the
                // file was touched; `changed` only gates whether a restart is
                // needed, not whether the file changed on disk.
                if changed {
                    restart_needed = true;
                    llm_changed = true;
                    // Carry the hot-reload eligibility so Phase 2 only takes the
                    // in-place reload when it can reach the running child: a
                    // Local target, or an External target whose key env the
                    // child already has. A key change / provider switch needing
                    // a different key env leaves this false → restart path.
                    llm_hot_reload_safe = outcome.hot_reload_safe(spawned_key_env);
                    // Stash the new settings so a Phase-2 hot-reload can
                    // upsert the LLM worker without re-reading the file.
                    llm_new = Some(outcome.new);
                }
                rollback.llm = Some(old);
                rollback.llm_api_key = old_api_key;
            }
            Err(e) => {
                // `apply_llm_settings_to_disk` writes the settings file
                // BEFORE the Keychain, so a Keychain failure can leave the
                // file at the new value. Record this step's own pre-apply
                // state into the rollback so it is undone alongside the
                // embedding change above.
                rollback.llm = Some(old);
                rollback.llm_api_key = old_api_key;
                rollback_files_on(data, &rollback);
                return Err(e);
            }
        }
    }

    if let Some(hf_req) = req.hf_home {
        let old = paths::load_app_settings(&data.app_settings_path());
        match apply_hf_home_to_disk(data, hf_req) {
            Ok(changed) => {
                if changed {
                    restart_needed = true;
                    hf_home_changed = true;
                    rollback.hf_home_changed = true;
                    rollback.hf_home_old = Some(old);
                }
            }
            Err(e) => {
                // Undo the embedding + LLM changes (file and Keychain).
                rollback_files_on(data, &rollback);
                return Err(e);
            }
        }
    }

    if let Some(mcp_req) = req.mcp {
        // Validated up front; `apply_mcp_settings_to_disk` re-validates but
        // that is cheap and keeps the function usable standalone.
        match apply_mcp_settings_to_disk(data, &mcp_req) {
            Ok(outcome) => {
                if outcome.changed {
                    restart_needed = true;
                    mcp_changed = true;
                    rollback.mcp = Some(outcome.old_settings);
                }
            }
            Err(e) => {
                // Undo the embedding + LLM + HF_HOME changes.
                rollback_files_on(data, &rollback);
                return Err(e);
            }
        }
    }

    Ok(PersistOutcome {
        rollback,
        restart_needed,
        needs_vectordb_reset,
        evacuate_mode,
        embedding_runtime,
        llm_changed,
        llm_hot_reload_safe,
        embedding_changed,
        hf_home_changed,
        mcp_changed,
        llm_new,
    })
}

pub(crate) fn persist_settings_without_restart(
    data: &crate::data::DataPaths,
    req: ApplySettingsRequest,
) -> AppResult<()> {
    persist_batch_to_disk(data, req, None).map(|_| ())
}

#[tauri::command]
pub async fn apply_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    req: ApplySettingsRequest,
) -> AppResult<ApplySettingsResponse> {
    // ── Phase 1: validate + persist to disk (no sidecar touch yet) ──
    // The running child's provider key env decides whether a switch INTO
    // External can hot-reload (the genai key can't be pushed to a live child).
    let spawned_key_env = state.sidecars.spawned_external_key_env();
    let outcome = persist_batch_to_disk(&state.data, req, spawned_key_env.as_deref())?;

    if !outcome.restart_needed {
        return Ok(ApplySettingsResponse {
            restarted: false,
            backup_path: None,
            embedding_runtime: outcome.embedding_runtime,
            warnings: Vec::new(),
        });
    }

    // ── Phase 2a: hot-reload (no sidecar restart) ──
    // Only an External-only LLM change qualifies (see `hot_reload_safe`): the
    // model swap is applied in place (upsert → Load). A Local target, embedding
    // (LanceDB dim) or HF_HOME (model-cache dir) change all require the restart
    // below. A Local GGUF in particular loads only after a fresh child starts;
    // an in-process Release→Load raced Metal teardown.
    if outcome.is_llm_only_change() && state.sidecars.current_endpoints().is_some() {
        return apply_llm_hot_reload(&app, &state, outcome).await;
    }

    let PersistOutcome {
        rollback,
        needs_vectordb_reset,
        evacuate_mode,
        embedding_runtime,
        llm_new,
        ..
    } = outcome;

    // ── Phase 2b: single restart ──
    state.invalidate_clients().await;
    state.sidecars.stop().await?;

    // Evacuate the LanceDB only when an embedding dimension change requires
    // it. On evacuation failure, roll back every persisted file and bring
    // the sidecar back up on the old config.
    let backup_path = if needs_vectordb_reset {
        match super::embedding_settings::evacuate_vectordb(&state.data, evacuate_mode) {
            Ok(p) => p,
            Err(e) => {
                rollback_files(&state, &rollback);
                restart_old(&app, &state).await;
                return Err(e);
            }
        }
    } else {
        None
    };

    // Mirror `stage_and_start_sidecars` but keep the Err branch so the
    // rollback can run. The plugin-staging warning is preserved so a
    // staging regression still surfaces to the UI.
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
            let warnings = report.warnings.clone();
            let _ = app.emit("sidecar://ready", &report);
            if let Some(new) = llm_new.as_ref() {
                super::llm_settings::load_local_model_after_restart(&app, &state.sidecars, new)
                    .await;
            }
            Ok(ApplySettingsResponse {
                restarted: true,
                backup_path: backup_path.map(|p| p.display().to_string()),
                embedding_runtime,
                warnings,
            })
        }
        Err(e) => {
            tracing::warn!(error = %e, "sidecar failed to start after batch settings apply; rolling back");
            super::emit_event(
                &app,
                "sidecar://error",
                crate::sidecar::startup_error::SidecarErrorPayload::Raw {
                    message: format!(
                        "settings apply failed: {e}; rolling back to previous settings"
                    ),
                },
            );
            // 1. Stop whatever partial state the failed start may have left.
            let _ = state.sidecars.stop().await;
            // 2. Restore every settings file we changed.
            rollback_files(&state, &rollback);
            // 3. Restore the lancedb backup if one was made.
            if let Some(backup) = backup_path.as_deref() {
                let _ = restore_vectordb_backup(&state.data, backup);
            }
            // 4. Restart with the old config (best-effort).
            restart_old(&app, &state).await;
            Err(crate::error::AppError::Config(format!(
                "設定の適用に失敗しました。元の設定にロールバックしました: {e}"
            )))
        }
    }
}

/// Phase 2a: apply an LLM-only change WITHOUT restarting the sidecar.
///
/// Connects to the LOCAL sidecar's jobworkerp (the LLM worker lives there
/// regardless of remote browse mode), re-registers the LLM worker YAML
/// against the new env, discards the stale static pool, then `Load`s the
/// new model. On failure the settings files are rolled back and the OLD
/// worker is reloaded so the user stays on a working model — no sidecar
/// restart in either branch.
async fn apply_llm_hot_reload(
    app: &tauri::AppHandle,
    state: &AppState,
    outcome: PersistOutcome,
) -> AppResult<ApplySettingsResponse> {
    // `is_llm_only_change` + the `current_endpoints().is_some()` guard at the
    // call site guarantee all three of these are present.
    let endpoints = state
        .require_endpoints()
        .expect("hot-reload entered only when a local sidecar is up");
    let new_settings = outcome
        .llm_new
        .as_ref()
        .expect("llm-only change carries the new settings");
    let old_settings = outcome
        .rollback
        .llm
        .as_ref()
        .expect("a hot-reloadable LLM change records its previous settings");
    let worker_yaml = crate::data::paths::llm_workers_yaml()?;

    // Shared connect → reload → rollback pipeline (the batch rollback restores
    // every persisted file + the Keychain key, vs the single command's file).
    let result = super::llm_settings::connect_and_reload_llm_with_rollback(
        app,
        &endpoints.jobworkerp_url(),
        &worker_yaml,
        new_settings,
        old_settings,
        || rollback_files(state, &outcome.rollback),
    )
    .await;
    // Drop the cached jobworkerp client either way: on success the released
    // static pool means the next dispatch should reconnect cleanly; on failure
    // the rollback re-applied the old worker, so the cache is equally stale.
    state.invalidate_clients().await;
    result.map(|()| ApplySettingsResponse {
        restarted: false,
        backup_path: None,
        embedding_runtime: outcome.embedding_runtime,
        warnings: Vec::new(),
    })
}

/// Restore each persisted settings file to its pre-apply value. Pure over
/// `&DataPaths` so the Phase-1 persistence failure path (which has no
/// `AppState`) can reuse it, and so the rollback is unit-testable.
fn rollback_files_on(data: &crate::data::DataPaths, rollback: &RollbackState) {
    if let Some(old) = &rollback.llm {
        let _ = save_llm_settings(&data.llm_settings_path(), old);
    }
    // Restore the Keychain key too — `apply_llm_settings_to_disk` may have
    // overwritten or deleted it, and the file rollback alone would leave
    // the credential disagreeing with the restored settings.
    if let Some(old_key) = &rollback.llm_api_key {
        let _ = restore_api_key(old_key.clone());
    }
    if let Some(old) = &rollback.embedding {
        let _ = save_embedding_settings(&data.embedding_settings_path(), old);
    }
    if rollback.hf_home_changed
        && let Some(old) = &rollback.hf_home_old
    {
        let _ = paths::save_app_settings(&data.app_settings_path(), old);
    }
    if let Some(old) = &rollback.mcp {
        let _ = save_mcp_settings(&data.mcp_settings_path(), old);
    }
}

/// Restore each persisted settings file to its pre-apply value.
fn rollback_files(state: &AppState, rollback: &RollbackState) {
    rollback_files_on(&state.data, rollback);
}

/// Bring the sidecar back up on whatever config is currently on disk.
/// Used by the rollback path after the files have been restored.
async fn restart_old(app: &tauri::AppHandle, state: &AppState) {
    let sidecars = state.sidecars.clone();
    let data = state.data.clone();
    crate::stage_and_start_sidecars(app, &sidecars, &data).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::app_settings::SetHfHomeRequest;
    use crate::commands::connection::{ConnectionConfig, ConnectionMode};
    use crate::commands::llm_settings::LlmMode;
    use crate::data::DataPaths;
    use crate::data::paths::HfHomeMode;

    fn data_in(tmp: &std::path::Path) -> DataPaths {
        DataPaths::with_root(tmp.to_path_buf())
    }

    fn external_llm(model: &str) -> SetLlmSettingsRequest {
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

    /// A Local-mode preset swap with no Keychain touch — the canonical
    /// hot-reload-safe LLM change.
    fn local_llm(preset_id: &str) -> SetLlmSettingsRequest {
        SetLlmSettingsRequest {
            mode: LlmMode::Local,
            provider_model: None,
            api_key: None,
            base_url: None,
            max_tokens: None,
            temperature: None,
            local_preset_id: Some(preset_id.to_string()),
            local_model_file: None,
            local_hf_repo: None,
            local_ctx_size: None,
            local_kv_cache_type: None,
        }
    }

    fn embedding_preset(id: &str) -> SetEmbeddingSettingsRequest {
        SetEmbeddingSettingsRequest {
            preset_id: Some(id.to_string()),
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
    fn empty_batch_needs_no_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: None,
            },
            None,
        )
        .unwrap();
        assert!(!out.restart_needed);
    }

    #[test]
    fn batch_persists_llm_and_hf_home_with_single_restart_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(external_llm("gpt-4o")),
                embedding: None,
                hf_home: Some(SetHfHomeRequest {
                    mode: HfHomeMode::DataRoot,
                    path: None,
                }),
                mcp: None,
            },
            None,
        )
        .unwrap();
        assert!(out.restart_needed, "both changes need exactly one restart");
        assert!(!out.needs_vectordb_reset, "no embedding change ⇒ no reset");
        // Both files were written.
        assert_eq!(
            load_llm_settings(&data.llm_settings_path())
                .provider_model
                .as_deref(),
            Some("gpt-4o")
        );
        assert_eq!(
            paths::load_app_settings(&data.app_settings_path()).hf_home_mode,
            HfHomeMode::DataRoot
        );
    }

    #[test]
    fn local_llm_only_change_takes_restart_path_not_hot_reload() {
        // A Local-mode LLM change is restart-needed but must NOT hot-reload:
        // the sidecar must restart before lazy loading (an in-process
        // Release→Load of the static Metal worker crashed macOS).
        // `is_llm_only_change` keys off
        // `llm_hot_reload_safe`, which is false for a Local target.
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(local_llm("qwen3-5-9b-ud-q4-k-xl")),
                embedding: None,
                hf_home: None,
                mcp: None,
            },
            None,
        )
        .unwrap();
        assert!(out.restart_needed);
        assert!(
            !out.is_llm_only_change(),
            "a Local-only LLM change must restart"
        );
        // The new settings remain available for the restarted sidecar config.
        assert!(out.llm_new.is_some());
    }

    #[test]
    fn external_llm_only_change_with_matching_key_is_hot_reload_eligible() {
        // An External-only change IS hot-reloadable when the running child was
        // spawned with the same provider key env (here OPENAI_API_KEY for a
        // gpt-* model) — no Metal involved, the genai worker just re-resolves.
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(external_llm("gpt-4o")),
                embedding: None,
                hf_home: None,
                mcp: None,
            },
            Some("OPENAI_API_KEY"),
        )
        .unwrap();
        assert!(out.restart_needed);
        assert!(
            out.is_llm_only_change(),
            "an External change whose key env the child already has is hot-reloadable"
        );
        assert!(out.llm_new.is_some());
    }

    #[test]
    fn external_llm_only_change_is_not_hot_reload_eligible() {
        // Regression for the P1 review finding: an External-side change (new
        // provider model) is restart-needed but must NOT take the hot-reload
        // path — the API key / provider env reaches the running child only at
        // spawn. `llm_changed` is set but `llm_hot_reload_safe` is not, so
        // `is_llm_only_change` is false and Phase 2 restarts.
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(external_llm("gpt-4o")),
                embedding: None,
                hf_home: None,
                mcp: None,
            },
            None,
        )
        .unwrap();
        assert!(
            out.restart_needed,
            "an external model change needs applying"
        );
        assert!(out.llm_changed);
        assert!(
            !out.is_llm_only_change(),
            "an External LLM change must take the restart path, not hot-reload"
        );
    }

    #[test]
    fn llm_plus_hf_home_change_is_not_hot_reload_eligible() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                // An External change that WOULD be hot-reloadable on its own
                // (matching key env) must still fall back to a restart when
                // HF_HOME is co-changed (model-cache dir).
                llm: Some(external_llm("gpt-4o")),
                embedding: None,
                hf_home: Some(SetHfHomeRequest {
                    mode: HfHomeMode::DataRoot,
                    path: None,
                }),
                mcp: None,
            },
            Some("OPENAI_API_KEY"),
        )
        .unwrap();
        // HF_HOME changes the child's model-cache dir ⇒ a restart is
        // required; the LLM change must NOT be split out into a hot-reload.
        assert!(
            !out.is_llm_only_change(),
            "a co-changed HF_HOME forces the full restart path"
        );
    }

    #[test]
    fn embedding_only_change_is_not_hot_reload_eligible() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: Some(embedding_preset("qwen3-vl-embedding-2b")),
                hf_home: None,
                mcp: None,
            },
            None,
        )
        .unwrap();
        // Switching from the unset default to a different preset is a real
        // change, so this MUST need a restart — assert it unconditionally so
        // the "never hot-reload" check below can't pass vacuously.
        assert!(out.restart_needed);
        // Embedding-only is a restart concern (LanceDB), never a hot-reload.
        assert!(!out.is_llm_only_change());
    }

    #[test]
    fn embedding_dimension_change_requests_vectordb_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        // Seed the default, then switch to a different-dim preset.
        persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: Some(embedding_preset(
                    crate::commands::embedding_presets::DEFAULT_EMBEDDING_PRESET_ID,
                )),
                hf_home: None,
                mcp: None,
            },
            None,
        )
        .unwrap();
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: Some(embedding_preset("qwen3-vl-embedding-2b")),
                hf_home: None,
                mcp: None,
            },
            None,
        )
        .unwrap();
        assert!(out.restart_needed);
        assert!(out.needs_vectordb_reset);
    }

    #[test]
    fn remote_embedding_rejection_does_not_persist_llm() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        // Mark the connection remote so the embedding apply is rejected.
        crate::commands::connection::save_connection_config(
            &data.connection_config_path(),
            &ConnectionConfig {
                mode: ConnectionMode::Remote,
                remote_jobworkerp_url: Some("http://h:9000".into()),
                remote_memories_url: Some("http://h:9010".into()),
            },
        )
        .unwrap();
        // The batch carries BOTH an LLM change and a (to-be-rejected)
        // embedding change. Because embedding is processed first, the whole
        // batch must abort and the LLM file must NOT be written.
        let res = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(external_llm("gpt-4o")),
                embedding: Some(embedding_preset("qwen3-embedding-0-6b")),
                hf_home: None,
                mcp: None,
            },
            None,
        );
        assert!(res.is_err(), "remote embedding must reject the batch");
        assert!(
            !data.llm_settings_path().exists(),
            "LLM must not be persisted when the embedding step rejects the batch"
        );
    }

    /// Regression for the "validate the whole batch before persisting"
    /// review finding: a VALID embedding change combined with an INVALID
    /// LLM change (custom local preset with a non-.gguf file) must abort
    /// the whole batch up front, leaving embedding-settings.json unwritten.
    #[test]
    fn invalid_llm_in_batch_does_not_persist_embedding() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let bad_llm = SetLlmSettingsRequest {
            mode: LlmMode::Local,
            provider_model: None,
            api_key: None,
            base_url: None,
            max_tokens: None,
            temperature: None,
            local_preset_id: Some("custom".into()),
            local_model_file: Some("model.bin".into()), // not .gguf → invalid
            local_hf_repo: Some("org/repo".into()),
            local_ctx_size: None,
            local_kv_cache_type: None,
        };
        let res = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(bad_llm),
                embedding: Some(embedding_preset("qwen3-embedding-0-6b")),
                hf_home: None,
                mcp: None,
            },
            None,
        );
        assert!(res.is_err(), "an invalid LLM request must abort the batch");
        assert!(
            !data.embedding_settings_path().exists(),
            "embedding must NOT be persisted when a later LLM validation fails"
        );
    }

    /// Regression for the "persist-phase failure leaves earlier changes
    /// applied" review finding: validation passes for the whole batch, but
    /// the LLM file WRITE then fails (its path is occupied by a directory).
    /// The embedding change persisted just before must be rolled back to its
    /// previous on-disk value — otherwise the next launch picks up a
    /// half-applied batch while the UI reports "apply failed".
    #[test]
    fn persist_failure_rolls_back_earlier_embedding_change() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());

        // Seed an initial embedding choice (value A) so we can assert the
        // rollback restores THIS value, not merely "absent".
        persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: Some(embedding_preset(
                    crate::commands::embedding_presets::DEFAULT_EMBEDDING_PRESET_ID,
                )),
                hf_home: None,
                mcp: None,
            },
            None,
        )
        .unwrap();
        let before = super::super::embedding_settings::load_embedding_settings(
            &data.embedding_settings_path(),
        );
        assert_eq!(
            before.preset_id.as_deref(),
            Some(crate::commands::embedding_presets::DEFAULT_EMBEDDING_PRESET_ID)
        );

        // Force the LLM write to fail: occupy `llm-settings.json` with a
        // DIRECTORY so `save_llm_settings`'s `fs::write` errors out after the
        // embedding file has already been re-written to value B.
        std::fs::create_dir(data.llm_settings_path()).unwrap();

        let res = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                // Switch embedding to value B (different preset) so the
                // embedding step actually writes and registers a rollback.
                embedding: Some(embedding_preset("qwen3-embedding-0-6b")),
                llm: Some(external_llm("gpt-4o")),
                hf_home: None,
                mcp: None,
            },
            None,
        );
        assert!(res.is_err(), "the LLM write failure must abort the batch");

        // The embedding file must be back at value A, not the half-applied
        // value B.
        let after = super::super::embedding_settings::load_embedding_settings(
            &data.embedding_settings_path(),
        );
        assert_eq!(
            after.preset_id.as_deref(),
            Some(crate::commands::embedding_presets::DEFAULT_EMBEDDING_PRESET_ID),
            "embedding must be rolled back to its pre-batch value on a later persist failure"
        );
    }

    /// Regression for "a restart-free LLM change is not rolled back": editing
    /// ONLY max_tokens / temperature (chat-only fields the sidecar never
    /// reads) makes `apply_llm_settings_to_disk` return `changed = false`, yet
    /// it still rewrites the settings file. If a LATER step in the batch fails
    /// (here: the HF_HOME write), the LLM file write must be undone too —
    /// otherwise the API returns failure while that one LLM edit survives,
    /// breaking the batch's atomicity.
    #[test]
    fn persist_failure_rolls_back_restart_free_llm_change() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());

        // Seed an initial LLM file (value A: max_tokens 4000) so we can assert
        // the rollback restores THIS value, not "absent".
        persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(SetLlmSettingsRequest {
                    max_tokens: Some(4000),
                    ..external_llm("gpt-4o")
                }),
                embedding: None,
                hf_home: None,
                mcp: None,
            },
            None,
        )
        .unwrap();
        let before = load_llm_settings(&data.llm_settings_path());
        assert_eq!(before.max_tokens, Some(4000));

        // Force the HF_HOME write to fail: occupy `app-settings.json` with a
        // DIRECTORY so `save_app_settings` errors out AFTER the LLM file has
        // already been re-written (value B: max_tokens 8000, changed=false
        // because only a chat-only field moved).
        std::fs::create_dir(data.app_settings_path()).unwrap();

        let res = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(SetLlmSettingsRequest {
                    max_tokens: Some(8000), // chat-only change ⇒ changed=false
                    ..external_llm("gpt-4o")
                }),
                embedding: None,
                hf_home: Some(SetHfHomeRequest {
                    mode: HfHomeMode::DataRoot, // valid, but the write fails
                    path: None,
                }),
                mcp: None,
            },
            None,
        );
        assert!(
            res.is_err(),
            "the HF_HOME write failure must abort the batch"
        );

        // The LLM file must be back at value A, not the half-applied value B —
        // even though the LLM change was restart-free (`changed = false`).
        let after = load_llm_settings(&data.llm_settings_path());
        assert_eq!(
            after.max_tokens,
            Some(4000),
            "a restart-free LLM change must still be rolled back on a later persist failure"
        );
    }

    /// Same guard for an invalid HF_HOME (custom mode without a path):
    /// the embedding change earlier in the batch must not survive.
    #[test]
    fn invalid_hf_home_in_batch_does_not_persist_embedding() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let res = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: Some(embedding_preset("qwen3-embedding-0-6b")),
                hf_home: Some(SetHfHomeRequest {
                    mode: HfHomeMode::Custom,
                    path: None, // custom without a path → invalid
                }),
                mcp: None,
            },
            None,
        );
        assert!(
            res.is_err(),
            "an invalid HF_HOME request must abort the batch"
        );
        assert!(
            !data.embedding_settings_path().exists(),
            "embedding must NOT be persisted when a later HF_HOME validation fails"
        );
    }

    // ── MCP ───────────────────────────────────────────────────────────

    fn enable_mcp() -> SetMcpSettingsRequest {
        SetMcpSettingsRequest {
            enabled: true,
            exclude_runner_as_tool: None,
            exclude_worker_as_tool: None,
            streaming: None,
            request_timeout_sec: None,
        }
    }

    #[test]
    fn mcp_only_change_needs_restart_not_hot_reload() {
        // Toggling MCP needs a sidecar restart (MCP_ENABLED is spawn-time
        // env) and must NEVER be split into the LLM-only hot-reload path —
        // that path doesn't restart the child, so the toggle wouldn't apply.
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: Some(enable_mcp()),
            },
            None,
        )
        .unwrap();
        assert!(out.restart_needed);
        assert!(out.mcp_changed);
        assert!(
            !out.is_llm_only_change(),
            "an MCP toggle must take the restart path, not hot-reload"
        );
        assert!(
            crate::commands::mcp_settings::load_mcp_settings(&data.mcp_settings_path()).enabled
        );
    }

    #[test]
    fn llm_plus_mcp_change_is_not_hot_reload_eligible() {
        // An External LLM change that WOULD be hot-reloadable on its own
        // (matching key env) must still fall back to a full restart when an
        // MCP toggle is co-changed — the MCP env only reaches the child at
        // spawn.
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(external_llm("gpt-4o")),
                embedding: None,
                hf_home: None,
                mcp: Some(enable_mcp()),
            },
            Some("OPENAI_API_KEY"),
        )
        .unwrap();
        assert!(
            !out.is_llm_only_change(),
            "a co-changed MCP toggle forces the full restart path"
        );
    }

    #[test]
    fn mcp_noop_when_unchanged_needs_no_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        // First apply enables; second apply of the same value is a no-op.
        persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: Some(enable_mcp()),
            },
            None,
        )
        .unwrap();
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: Some(enable_mcp()),
            },
            None,
        )
        .unwrap();
        assert!(!out.mcp_changed);
        assert!(!out.restart_needed);
    }

    #[test]
    fn mcp_enable_persists_in_remote_mode() {
        // MCP runs in the local sidecar regardless of remote browse mode, so a
        // batch that enables MCP while connected to a remote memories must NOT
        // be rejected — it persists like any other change.
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        crate::commands::connection::save_connection_config(
            &data.connection_config_path(),
            &ConnectionConfig {
                mode: ConnectionMode::Remote,
                remote_jobworkerp_url: Some("http://h:9000".into()),
                remote_memories_url: Some("http://h:9010".into()),
            },
        )
        .unwrap();
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: Some(enable_mcp()),
            },
            None,
        )
        .unwrap();
        assert!(out.mcp_changed);
        assert!(out.restart_needed);
        assert!(
            crate::commands::mcp_settings::load_mcp_settings(&data.mcp_settings_path()).enabled
        );
    }

    #[test]
    fn rollback_files_restores_mcp_settings() {
        // MCP is the last card persisted in the batch, so a persist-phase
        // failure can't strand it. Its rollback path is the Phase-2 restart
        // failure, which calls `rollback_files`. Pin that the MCP file is in
        // `rollback_files_on`'s restore set: enable on disk, capture the
        // prior (disabled) state in a RollbackState, run the rollback, and
        // assert the file is back to disabled.
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());

        // Disk now reflects "enabled" (the just-applied value B).
        crate::commands::mcp_settings::save_mcp_settings(
            &data.mcp_settings_path(),
            &McpSettings {
                enabled: true,
                ..Default::default()
            },
        )
        .unwrap();

        // The rollback carries the pre-apply (disabled) value A.
        let rollback = RollbackState {
            mcp: Some(McpSettings::default()),
            ..Default::default()
        };
        rollback_files_on(&data, &rollback);

        assert!(
            !crate::commands::mcp_settings::load_mcp_settings(&data.mcp_settings_path()).enabled,
            "rollback must restore the MCP file to its pre-apply (disabled) value"
        );
    }
}
