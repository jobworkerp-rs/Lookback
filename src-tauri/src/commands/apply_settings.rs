//! Unified settings save.
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
//! — then the sidecar is brought back up on the old config. Storage remains
//! owned by memories throughout the settings transaction.

use serde::{Deserialize, Serialize};
use tauri::Emitter;

use crate::error::{AppError, AppResult};

use super::AppState;
use super::app_settings::{
    SetHfHomeRequest, SetTimezoneRequest, apply_hf_home_to_disk, apply_migration_backup_to_disk,
    apply_timezone_to_disk, validate_hf_home_request, validate_timezone_request,
};
use super::embedding_settings::{
    EmbeddingPlanPermit, EmbeddingRuntime, EmbeddingSettings, SetEmbeddingSettingsRequest,
    save_embedding_settings, validate_embedding_request,
};
use super::issue_tracker_settings::{
    IssueTrackerSettings, SetIssueTrackerSettingsRequest, load_issue_tracker_settings,
    resolve_issue_tracking, save_issue_tracker_settings,
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
use crate::sidecar::{RestartPlan, SidecarKind};

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
    #[serde(default)]
    pub timezone: Option<SetTimezoneRequest>,
    #[serde(default)]
    pub issue_tracker: Option<SetIssueTrackerSettingsRequest>,
    #[serde(default)]
    pub migration_backup: Option<crate::data::paths::MigrationBackupSettings>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplySettingsResponse {
    /// `false` ⇒ nothing meaningful changed, so the sidecar was left
    /// running untouched.
    pub restarted: bool,
    /// The embedding runtime that is now in effect (only when an embedding
    /// change was part of the batch).
    pub embedding_runtime: Option<EmbeddingRuntime>,
    /// Plugin-staging warnings surfaced by the restart, if any.
    pub warnings: Vec<crate::sidecar::SidecarWarning>,
    /// A newly rotated MCP bearer token. Returned exactly once, and only
    /// after its replacement jobworkerp listener started successfully.
    pub mcp_auth_token: Option<String>,
}

/// Snapshot of the previous on-disk state for each setting we touched, so
/// a failed restart can be rolled back.
#[derive(Default, Clone)]
struct RollbackState {
    llm: Option<LlmSettings>,
    /// Outer `Some` ⇒ the batch changed the Keychain API key, so it must be
    /// restored on rollback. Inner `Some(key)` is the old key; inner `None`
    /// means there was no stored key before (rollback deletes it).
    llm_api_key: Option<Option<String>>,
    embedding: Option<EmbeddingSettings>,
    /// Pre-batch snapshot of `app-settings.json`, captured ONCE before the
    /// first writer touches it. Both HF_HOME and timezone persist into this
    /// same file, so they share one rollback slot — restoring per-writer
    /// snapshots would double-restore and clobber the co-changed field.
    /// Restored once when `app_settings_changed`.
    app_settings_changed: bool,
    app_settings_old: Option<paths::AppSettings>,
    mcp: Option<McpSettings>,
    issue_tracker: Option<IssueTrackerSettings>,
}

impl RollbackState {
    /// Snapshot `app-settings.json` exactly once — the first app-settings
    /// writer (HF_HOME or timezone) in the batch owns the pre-batch state so
    /// a later writer's snapshot (which would already contain the first
    /// writer's change) never overwrites it.
    fn snapshot_app_settings_once(&mut self, data: &crate::data::DataPaths) {
        if !self.app_settings_changed {
            self.app_settings_old = Some(paths::load_app_settings(&data.app_settings_path()));
            self.app_settings_changed = true;
        }
    }
}

/// Result of persisting the batch to disk (Phase 1). Carries everything
/// Phase 2 (the single restart) needs.
struct PersistOutcome {
    rollback: RollbackState,
    restart_needed: bool,
    /// A prompt-only guide update changes persisted conductor inputs, not the
    /// running worker. Refresh those inputs without restarting sidecars.
    periodic_runtime_refresh_needed: bool,
    embedding_runtime: Option<EmbeddingRuntime>,
    /// Per-card change flags used to build the smallest safe restart plan.
    llm_changed: bool,
    embedding_changed: bool,
    hf_home_changed: bool,
    /// MCP enable flag / advanced overrides changed. Always requires a full
    /// sidecar restart — `MCP_ENABLED` is read at jobworkerp spawn time, so
    /// there is no hot-reload path. Gates `is_llm_only_change` so a co-changed
    /// MCP toggle can never be split into the LLM-only in-place reload.
    mcp_changed: bool,
    /// Non-persistent MCP token rotation requested with this save.
    mcp_rotate_auth_token: bool,
    /// Workflow timezone changed. Like MCP, `TZ` is read at jobworkerp spawn
    /// time (there is no hot-reload path), so a change always requires a full
    /// sidecar restart. Gates `is_llm_only_change`.
    tz_changed: bool,
    /// Kept false for LLM settings changes. Replacing a shared LLM worker in
    /// place races summary workflows between their exact count and completion.
    llm_hot_reload_safe: bool,
    /// The new LLM settings used by the restarted sidecar. `None` means the
    /// batch did not change the LLM card.
    llm_new: Option<LlmSettings>,
    /// Issue Tracker changes require the all-sidecar transaction because the
    /// worker registration and optional child are decided at startup.
    issue_tracker_changed: bool,
}

impl PersistOutcome {
    /// This remains a structural predicate so the restart planner keeps its
    /// invariants. Production persistence always records the flag as false,
    /// forcing the sidecar boundary for every LLM configuration change.
    fn is_llm_only_change(&self) -> bool {
        self.llm_changed
            && self.llm_hot_reload_safe
            && !self.embedding_changed
            && !self.hf_home_changed
            && !self.mcp_changed
            && !self.tz_changed
            && !self.issue_tracker_changed
    }

    fn requires_full_restart(&self) -> bool {
        self.issue_tracker_changed
    }

    fn restart_plan(&self) -> RestartPlan {
        let mut plan = RestartPlan::default();
        if self.llm_changed && !self.is_llm_only_change() {
            plan = plan.union(&RestartPlan::jobworkerp_only());
        }
        if self.embedding_changed {
            plan = plan.union(&RestartPlan::embedding());
        }
        if self.hf_home_changed || self.mcp_changed || self.mcp_rotate_auth_token {
            plan = plan.union(&RestartPlan::jobworkerp_only());
        }
        if self.tz_changed {
            plan = plan.union(&RestartPlan::timezone());
        }
        plan
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
#[cfg(test)]
fn persist_batch_to_disk(
    data: &crate::data::DataPaths,
    req: ApplySettingsRequest,
    spawned_key_env: Option<&str>,
) -> AppResult<PersistOutcome> {
    persist_batch_to_disk_with_build_allows(
        data,
        req,
        spawned_key_env,
        crate::experimental_features::ISSUE_TRACKING_BUILD_ALLOWS,
    )
}

#[cfg(test)]
fn persist_batch_to_disk_with_build_allows(
    data: &crate::data::DataPaths,
    req: ApplySettingsRequest,
    spawned_key_env: Option<&str>,
    build_allows: bool,
) -> AppResult<PersistOutcome> {
    persist_batch_to_disk_with_embedding_plan(data, req, spawned_key_env, build_allows, None, false)
}

#[cfg(test)]
fn persist_batch_to_disk_with_embedding_plan(
    data: &crate::data::DataPaths,
    req: ApplySettingsRequest,
    _spawned_key_env: Option<&str>,
    build_allows: bool,
    embedding_permit: Option<EmbeddingPlanPermit>,
    rebuild_pending: bool,
) -> AppResult<PersistOutcome> {
    persist_batch_to_disk_with_embedding_plan_and_env(
        data,
        req,
        _spawned_key_env,
        build_allows,
        embedding_permit,
        rebuild_pending,
        crate::commands::process_env_lookup,
    )
}

fn persist_batch_to_disk_with_embedding_plan_and_env<F>(
    data: &crate::data::DataPaths,
    req: ApplySettingsRequest,
    _spawned_key_env: Option<&str>,
    build_allows: bool,
    embedding_permit: Option<EmbeddingPlanPermit>,
    rebuild_pending: bool,
    embedding_env_lookup: F,
) -> AppResult<PersistOutcome>
where
    F: Fn(&str) -> Option<String>,
{
    let llm_path = data.llm_settings_path();
    let mut rollback = RollbackState::default();
    let mut restart_needed = false;
    let mut periodic_runtime_refresh_needed = false;
    let mut embedding_runtime: Option<EmbeddingRuntime> = None;
    let mut llm_changed = false;
    let mut llm_hot_reload_safe = false;
    let mut embedding_changed = false;
    let mut hf_home_changed = false;
    let mut mcp_changed = false;
    let mut mcp_rotate_auth_token = false;
    let mut tz_changed = false;
    let mut llm_new: Option<LlmSettings> = None;
    let mut issue_tracker_changed = false;

    // ── Validate the WHOLE batch before writing anything ──
    // A later card's validation failure must not leave an earlier card's
    // change half-saved on disk (e.g. embedding persisted, then a bad
    // custom LLM .gguf name aborts — the next launch would pick up the
    // orphaned embedding change). Validate all present requests first.
    if let Some(emb_req) = req.embedding.as_ref() {
        validate_embedding_request(emb_req)?;
        if rebuild_pending {
            super::embedding_settings::validate_embedding_settings_pending_change_with_env(
                data,
                emb_req,
                true,
                &embedding_env_lookup,
            )?;
        }
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
    if let Some(tz_req) = req.timezone.as_ref() {
        validate_timezone_request(tz_req)?;
    }

    // ── Persist (validation already passed for every request) ──
    //
    // Each persist step writes a file (and, for LLM, the Keychain). A
    // failure midway through MUST undo the steps already applied — otherwise
    // the UI reports "apply failed" while the next launch picks up a
    // partially-applied batch. `rollback` accumulates the previous on-disk
    // state as each step succeeds; on any `?` we restore from it before
    // returning. (Embedding is first, so its own failure has nothing to undo
    // — its migration guard rejects incompatible settings before persistence.)
    if let Some(emb_req) = req.embedding.as_ref() {
        let outcome = super::embedding_settings::apply_embedding_settings_to_disk_with_env(
            data,
            emb_req,
            embedding_permit,
            rebuild_pending,
            &embedding_env_lookup,
        )?;
        embedding_runtime = Some(outcome.new_runtime.clone());
        if outcome.changed {
            restart_needed = true;
            embedding_changed = true;
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
                periodic_runtime_refresh_needed = outcome.periodic_runtime_refresh_needed;
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
                    // Do not hot-reload even an otherwise compatible external
                    // worker: summary token-count and completion must observe
                    // one worker configuration for their entire workflow.
                    llm_hot_reload_safe = false;
                    // The restarted sidecar reads this committed configuration.
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
        // Snapshot app-settings.json BEFORE the first writer touches it, so
        // a co-changed timezone (same file, persisted below) shares this one
        // pre-batch snapshot rather than capturing the hf-mutated state.
        rollback.snapshot_app_settings_once(data);
        match apply_hf_home_to_disk(data, hf_req) {
            Ok(changed) => {
                if changed {
                    restart_needed = true;
                    hf_home_changed = true;
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
        mcp_rotate_auth_token = mcp_req.rotate_auth_token;
        // Rotation is a non-persistent change, but the replacement token is
        // observed only by a freshly spawned MCP listener.
        restart_needed |= mcp_rotate_auth_token;
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

    // Timezone shares `app-settings.json` with HF_HOME, so it must run AFTER
    // hf_home and reuse the single pre-batch snapshot (see
    // `snapshot_app_settings_once`) — otherwise a co-changed batch would
    // double-restore and clobber one of the two fields on rollback.
    if let Some(tz_req) = req.timezone {
        rollback.snapshot_app_settings_once(data);
        match apply_timezone_to_disk(data, tz_req) {
            Ok(changed) => {
                if changed {
                    restart_needed = true;
                    tz_changed = true;
                }
            }
            Err(e) => {
                // Undo the embedding + LLM + HF_HOME + MCP changes.
                rollback_files_on(data, &rollback);
                return Err(e);
            }
        }
    }

    // Read only by the next startup migration, so persisting it never needs
    // a restart; it shares the single app-settings.json rollback snapshot.
    if let Some(backup) = req.migration_backup {
        rollback.snapshot_app_settings_once(data);
        if let Err(e) = apply_migration_backup_to_disk(data, backup) {
            rollback_files_on(data, &rollback);
            return Err(e);
        }
    }

    if let Some(issue_tracker_req) = req.issue_tracker {
        let path = data.issue_tracker_settings_path();
        let old = load_issue_tracker_settings(&path);
        let new = IssueTrackerSettings {
            enabled: issue_tracker_req.enabled,
        };
        if old != new {
            // Capture the previous file before writing so a partial write
            // failure can restore the Issue Tracker setting as well.
            rollback.issue_tracker = Some(old.clone());
            if let Err(error) = save_issue_tracker_settings(&path, &new) {
                rollback_files_on(data, &rollback);
                return Err(error);
            }
            if issue_tracker_effective_value_changed(&old, &new, build_allows) {
                issue_tracker_changed = true;
                restart_needed = true;
            }
        }
    }

    Ok(PersistOutcome {
        rollback,
        restart_needed,
        periodic_runtime_refresh_needed,
        embedding_runtime,
        llm_changed,
        llm_hot_reload_safe,
        embedding_changed,
        hf_home_changed,
        mcp_changed,
        mcp_rotate_auth_token,
        tz_changed,
        llm_new,
        issue_tracker_changed,
    })
}

pub(crate) fn persist_settings_without_restart_with_env<F>(
    data: &crate::data::DataPaths,
    req: ApplySettingsRequest,
    embedding_env_lookup: F,
) -> AppResult<()>
where
    F: Fn(&str) -> Option<String>,
{
    persist_batch_to_disk_with_embedding_plan_and_env(
        data,
        req,
        None,
        crate::experimental_features::ISSUE_TRACKING_BUILD_ALLOWS,
        None,
        false,
        embedding_env_lookup,
    )
    .map(|_| ())
}

#[cfg(test)]
pub(crate) fn persist_settings_without_restart_approved(
    data: &crate::data::DataPaths,
    req: ApplySettingsRequest,
    permit: EmbeddingPlanPermit,
    rebuild_pending: bool,
) -> AppResult<()> {
    persist_settings_without_restart_approved_with_env(
        data,
        req,
        permit,
        rebuild_pending,
        crate::commands::process_env_lookup,
    )
}

pub(crate) fn persist_settings_without_restart_approved_with_env<F>(
    data: &crate::data::DataPaths,
    req: ApplySettingsRequest,
    permit: EmbeddingPlanPermit,
    rebuild_pending: bool,
    embedding_env_lookup: F,
) -> AppResult<()>
where
    F: Fn(&str) -> Option<String>,
{
    persist_batch_to_disk_with_embedding_plan_and_env(
        data,
        req,
        None,
        crate::experimental_features::ISSUE_TRACKING_BUILD_ALLOWS,
        Some(permit),
        rebuild_pending,
        embedding_env_lookup,
    )
    .map(|_| ())
}

fn issue_tracker_effective_value_changed(
    old: &IssueTrackerSettings,
    new: &IssueTrackerSettings,
    build_allows: bool,
) -> bool {
    resolve_issue_tracking(old, build_allows) != resolve_issue_tracking(new, build_allows)
}

#[tauri::command]
pub async fn apply_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    req: ApplySettingsRequest,
) -> AppResult<ApplySettingsResponse> {
    let _embedding_recovery = state.database_migration_recovery.lock().await;
    let _dispatch_gate = state.lock_llm_settings_dispatch_gate().await;
    if req.llm.is_some() && state.has_in_flight_dispatches().await {
        return Err(crate::error::AppError::Config(
            super::LLM_SETTINGS_CHANGE_WHILE_GENERATING_ERROR.into(),
        ));
    }
    let embedding_approval = match req.embedding.as_ref() {
        Some(embedding) if state.is_service_local(super::connection::Service::Memories) => Some(
            super::embedding_management::approve_embedding_settings_change(&state, embedding)
                .await?,
        ),
        Some(_) | None => None,
    };
    let embedding_reconcile_required = embedding_approval.is_some_and(|approval| {
        approval.decision == crate::sidecar::embedding_migration::PlanDecision::ReconcileRequired
    });

    // ── Phase 1: validate + persist to disk (no sidecar touch yet) ──
    let embedding_env_lookup = |key: &str| state.sidecars.embedding_env_lookup(key);
    let outcome = match embedding_approval {
        Some(approval) => persist_batch_to_disk_with_embedding_plan_and_env(
            &state.data,
            req,
            None,
            crate::experimental_features::ISSUE_TRACKING_BUILD_ALLOWS,
            Some(approval.permit),
            approval.rebuild_pending,
            embedding_env_lookup,
        )?,
        None => persist_batch_to_disk_with_embedding_plan_and_env(
            &state.data,
            req,
            None,
            crate::experimental_features::ISSUE_TRACKING_BUILD_ALLOWS,
            None,
            false,
            embedding_env_lookup,
        )?,
    };

    if !outcome.restart_needed {
        // Prompt-only summary guidance changes the runtime payload persisted
        // in conductor, not the live sidecar. Refresh it without interrupting
        // the worker; if sidecars are offline, normal startup refreshes it.
        if outcome.periodic_runtime_refresh_needed
            && state.sidecars.current_endpoints().is_some()
            && let Err(error) = super::periodic_tasks::refresh_lookback_periodic_runtime_from_state(
                &state,
                &std::collections::HashSet::new(),
            )
            .await
        {
            rollback_files(&state, &outcome.rollback);
            if let Err(rollback_error) =
                super::periodic_tasks::refresh_lookback_periodic_runtime_from_state(
                    &state,
                    &std::collections::HashSet::new(),
                )
                .await
            {
                tracing::warn!(error = %rollback_error, "failed to restore periodic runtime after guidance refresh failure");
            }
            return Err(error);
        }
        if embedding_reconcile_required
            && state.is_service_local(super::connection::Service::Memories)
            && state.sidecars.current_endpoints().is_some()
        {
            super::embedding_management::wait_for_registered_worker(&state).await?;
            super::embedding_management::start_reconcile(&state).await?;
        }
        return Ok(ApplySettingsResponse {
            restarted: false,
            embedding_runtime: outcome.embedding_runtime,
            warnings: Vec::new(),
            mcp_auth_token: None,
        });
    }

    // `persist_batch_to_disk` keeps `llm_hot_reload_safe` false, so this branch
    // is intentionally not taken for production requests. Retaining the
    // structural branch keeps the restart planner testable without duplicating
    // its predicate.
    if outcome.is_llm_only_change() && state.sidecars.current_endpoints().is_some() {
        return apply_llm_hot_reload(&app, &state, outcome).await;
    }

    let full_restart = outcome.requires_full_restart();
    let restart_plan = outcome.restart_plan();
    debug_assert!(full_restart || !restart_plan.is_empty());
    let paused_periodic = if full_restart || restart_plan.includes(SidecarKind::Jobworkerp) {
        match super::periodic_execution::pause_periodic_schedules_for_restart(&state).await {
            Ok(paused) => Some(paused),
            Err(error) => {
                rollback_files(&state, &outcome.rollback);
                return Err(error);
            }
        }
    } else {
        None
    };
    if full_restart {
        return apply_full_restart(
            &app,
            &state,
            outcome,
            paused_periodic.expect("full restart always pauses periodic schedules"),
            embedding_reconcile_required,
        )
        .await;
    }
    let PersistOutcome {
        rollback,
        embedding_runtime,
        llm_new,
        mcp_rotate_auth_token,
        ..
    } = outcome;

    // The previous credential is captured only after the lifecycle has
    // reserved this save's token rotation. Capturing it earlier would allow a
    // standalone rotation to race an MCP-disabled → enabled save.
    let old_mcp_token = std::sync::Arc::new(parking_lot::Mutex::new(None));
    let old_mcp_token_for_start = old_mcp_token.clone();

    // ── Phase 2b: single restart ──
    state.invalidate_clients().await;
    // Mirror `stage_and_start_sidecars` but keep the Err branch so the
    // rollback can run. The plugin-staging warning is preserved so a
    // staging regression still surfaces to the UI.
    let plugin_warnings = super::stage_plugins_for_restart(&app, &state.data.plugins_dir());

    let rotated_token = std::sync::Arc::new(parking_lot::Mutex::new(None));
    let rotated_token_for_start = rotated_token.clone();
    let restart_result = match restart_settings_plan(
        &state.sidecars,
        mcp_rotate_auth_token,
        restart_plan.clone(),
        plugin_warnings,
        move || {
            if !mcp_rotate_auth_token {
                return Ok(());
            }
            let snapshot = super::mcp_settings::snapshot_mcp_auth_token()?;
            *old_mcp_token_for_start.lock() = Some(snapshot);
            let token = super::mcp_settings::rotate_mcp_auth_token_after_stop()?;
            *rotated_token_for_start.lock() = Some(token);
            Ok(())
        },
    )
    .await
    {
        Ok(report) => {
            async {
                if embedding_reconcile_required {
                    super::embedding_management::wait_for_registered_worker(&state).await?;
                    super::embedding_management::start_reconcile(&state).await?;
                }
                if let Some(paused) = paused_periodic.as_ref() {
                    paused.resume_after_restart(&state).await?;
                }
                Ok(report)
            }
            .await
        }
        Err(error) => Err(error),
    };
    match restart_result {
        Ok(report) => {
            let warnings = report.warnings.clone();
            let _ = app.emit("sidecar://ready", &report);
            if let Some(new) = llm_new.as_ref() {
                super::llm_settings::load_local_model_after_restart(&app, &state.sidecars, new)
                    .await;
            }
            Ok(ApplySettingsResponse {
                restarted: true,
                embedding_runtime,
                warnings,
                mcp_auth_token: rotated_token.lock().take(),
            })
        }
        Err(e) => {
            tracing::warn!(error = %e, "sidecar failed to start after batch settings apply; rolling back");
            super::emit_event(
                &app,
                "sidecar://error",
                crate::sidecar::startup_error::SidecarErrorPayload::Raw {
                    message: settings_apply_failure_message(&e),
                },
            );
            // 1. Stop the replacement even when startup itself succeeded and
            // only the post-start scheduler resume failed.
            if let Err(stop_error) = state.sidecars.stop_restart_plan(restart_plan.clone()).await {
                tracing::warn!(error = %stop_error, "failed to stop sidecars after settings apply failure");
            }
            // 2. Restore the old credential before bringing the old
            // configuration back up.
            let token_restore_error = old_mcp_token.lock().take().and_then(|snapshot| {
                state
                    .sidecars
                    .with_mcp_token_mutation(|| {
                        super::mcp_settings::restore_mcp_auth_token(snapshot)
                    })
                    .err()
            });
            // 3. Restore every settings file we changed.
            rollback_files(&state, &rollback);
            if let Some(token_restore_error) = token_restore_error {
                // Do not launch the rolled-back MCP listener with an
                // unconfirmed credential: its clients only know the old
                // token, and the replacement token is never returned on a
                // failed settings save.
                return Err(crate::error::AppError::Config(format!(
                    "設定の適用に失敗しました（{e}）。元の MCP token の復元にも失敗したため、対象 sidecar は停止したままです: {token_restore_error}"
                )));
            }
            // 5. Restart the selected old configuration.
            restart_old_plan(&app, &state, restart_plan).await;
            if let Some(paused) = paused_periodic.as_ref()
                && let Err(restore_error) = paused.resume_after_restart(&state).await
            {
                return Err(crate::error::AppError::Config(format!(
                    "設定の適用に失敗しました。元の設定にロールバックしましたが、定期実行の再有効化にも失敗しました: {restore_error}"
                )));
            }
            Err(crate::error::AppError::Config(format!(
                "設定の適用に失敗しました。元の設定にロールバックしました: {e}"
            )))
        }
    }
}

/// Apply a batch that changes the Issue Tracker toggle. The lifecycle
/// transaction owns the stop/start lock and invokes the recovery continuation
/// before releasing it, so old settings are restored before another lifecycle
/// operation can observe the data root.
async fn apply_full_restart(
    app: &tauri::AppHandle,
    state: &AppState,
    outcome: PersistOutcome,
    paused_periodic: super::periodic_execution::PausedPeriodicSchedules,
    embedding_reconcile_required: bool,
) -> AppResult<ApplySettingsResponse> {
    let PersistOutcome {
        rollback,
        embedding_runtime,
        llm_new,
        mcp_rotate_auth_token,
        ..
    } = outcome;

    state.invalidate_clients().await;
    let plugin_warnings = super::stage_plugins_for_restart(app, &state.data.plugins_dir());
    let old_mcp_token = std::sync::Arc::new(parking_lot::Mutex::new(None));
    let old_mcp_token_for_start = old_mcp_token.clone();
    let rotated_token = std::sync::Arc::new(parking_lot::Mutex::new(None));
    let rotated_token_for_start = rotated_token.clone();
    let transaction_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let transaction_started_for_start = transaction_started.clone();
    let recovery_attempted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let recovery_attempted_for_recover = recovery_attempted.clone();
    let recovery_succeeded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let recovery_succeeded_for_post_start = recovery_succeeded.clone();
    let before_start = move || {
        transaction_started_for_start.store(true, std::sync::atomic::Ordering::Release);
        if !mcp_rotate_auth_token {
            return Ok(());
        }
        let snapshot = super::mcp_settings::snapshot_mcp_auth_token()?;
        *old_mcp_token_for_start.lock() = Some(snapshot);
        let token = super::mcp_settings::rotate_mcp_auth_token_after_stop()?;
        *rotated_token_for_start.lock() = Some(token);
        Ok(())
    };

    let recovery_data = state.data.clone();
    let recovery_sidecars = std::sync::Arc::clone(&state.sidecars);
    let recovery_app = app.clone();
    let recovery_rollback = rollback.clone();
    let recovery_old_mcp_token = old_mcp_token.clone();
    let recover = move || async move {
        recovery_attempted_for_recover.store(true, std::sync::atomic::Ordering::Release);
        if let Some(snapshot) = recovery_old_mcp_token.lock().as_ref().cloned() {
            recovery_sidecars.with_mcp_token_mutation(|| {
                super::mcp_settings::restore_mcp_auth_token(snapshot)
            })?;
            *recovery_old_mcp_token.lock() = None;
        }
        rollback_files_on(&recovery_data, &recovery_rollback);
        Ok(super::stage_plugins_for_restart(
            &recovery_app,
            &recovery_data.plugins_dir(),
        ))
    };

    let paused_periodic = std::sync::Arc::new(paused_periodic);
    let post_start = {
        let paused_periodic = std::sync::Arc::clone(&paused_periodic);
        let recovery_attempted = std::sync::Arc::clone(&recovery_attempted);
        move || {
            let paused_periodic = std::sync::Arc::clone(&paused_periodic);
            let recovery_succeeded = std::sync::Arc::clone(&recovery_succeeded_for_post_start);
            let recovery_attempted = std::sync::Arc::clone(&recovery_attempted);
            async move {
                if embedding_reconcile_required
                    && !recovery_attempted.load(std::sync::atomic::Ordering::Acquire)
                {
                    super::embedding_management::wait_for_registered_worker(state).await?;
                    super::embedding_management::start_reconcile(state).await?;
                }
                let result = paused_periodic.resume_after_restart(state).await;
                if result.is_ok() && recovery_attempted.load(std::sync::atomic::Ordering::Acquire) {
                    recovery_succeeded.store(true, std::sync::atomic::Ordering::Release);
                }
                result
            }
        }
    };

    let result = state
        .sidecars
        .full_restart_with_transaction(
            plugin_warnings,
            mcp_rotate_auth_token,
            before_start,
            recover,
            post_start,
        )
        .await;
    match result {
        Ok(report) => {
            let warnings = report.warnings.clone();
            let _ = app.emit("sidecar://ready", &report);
            if let Some(new) = llm_new.as_ref() {
                super::llm_settings::load_local_model_after_restart(app, &state.sidecars, new)
                    .await;
            }
            Ok(ApplySettingsResponse {
                restarted: true,
                embedding_runtime,
                warnings,
                mcp_auth_token: rotated_token.lock().take(),
            })
        }
        Err(error) => {
            super::emit_event(
                app,
                "sidecar://error",
                crate::sidecar::startup_error::SidecarErrorPayload::Raw {
                    message: settings_apply_failure_message(&error),
                },
            );
            let transaction_started =
                transaction_started.load(std::sync::atomic::Ordering::Acquire);
            let recovered = recovery_succeeded.load(std::sync::atomic::Ordering::Acquire);
            let failure_path = full_restart_failure_path(transaction_started, recovered);
            let restore_token = {
                let old_mcp_token = std::sync::Arc::clone(&old_mcp_token);
                let sidecars = std::sync::Arc::clone(&state.sidecars);
                move || {
                    if let Some(snapshot) = old_mcp_token.lock().as_ref().cloned() {
                        sidecars.with_mcp_token_mutation(|| {
                            super::mcp_settings::restore_mcp_auth_token(snapshot)
                        })?;
                        *old_mcp_token.lock() = None;
                    }
                    Ok(())
                }
            };
            let rollback_for_recovery = || rollback_files(state, &rollback);
            let restart_old_full_for_recovery = || restart_old_full(app, state);
            let resume_for_recovery = || paused_periodic.resume_after_restart(state);
            let recovery_result = recover_full_restart_failure(
                failure_path,
                rollback_for_recovery,
                restore_token,
                restart_old_full_for_recovery,
                resume_for_recovery,
            )
            .await;

            match (failure_path, recovery_result) {
                (FullRestartFailurePath::BeforeTransaction, Ok(())) => {}
                (
                    FullRestartFailurePath::BeforeTransaction,
                    Err(FullRestartRecoveryError::Resume(error)),
                ) => {
                    return Err(AppError::Config(format!(
                        "設定の適用に失敗しました。元の設定に戻しましたが、定期実行の再有効化にも失敗しました: {error}"
                    )));
                }
                (FullRestartFailurePath::RestartOldFull, Ok(())) => {}
                (
                    FullRestartFailurePath::RestartOldFull,
                    Err(FullRestartRecoveryError::Token(recovery_error)),
                ) => {
                    return Err(AppError::Config(format!(
                        "設定の適用に失敗しました（{error}）。元の MCP token の復元にも失敗したため、sidecar は停止したままです: {recovery_error}"
                    )));
                }
                (
                    FullRestartFailurePath::RestartOldFull,
                    Err(FullRestartRecoveryError::Restart(recovery_error)),
                ) => {
                    return Err(AppError::Config(format!(
                        "設定の適用に失敗しました。元の設定を復元しましたが、sidecar の再起動にも失敗しました: {recovery_error}"
                    )));
                }
                (
                    FullRestartFailurePath::RestartOldFull,
                    Err(FullRestartRecoveryError::Resume(recovery_error)),
                ) => {
                    return Err(AppError::Config(format!(
                        "設定の適用に失敗しました。元の sidecar を再起動しましたが、定期実行の再有効化にも失敗しました: {recovery_error}"
                    )));
                }
                (FullRestartFailurePath::Recovered, Ok(())) => {
                    if let Some(report) = state.sidecars.last_report() {
                        let _ = app.emit("sidecar://ready", &report);
                    }
                }
                (FullRestartFailurePath::Recovered, Err(_)) => {
                    unreachable!("a recovered transaction has no recovery actions")
                }
                (FullRestartFailurePath::BeforeTransaction, Err(_)) => {
                    unreachable!("the pre-transaction branch only resumes periodic schedules")
                }
            }
            Err(AppError::Config(format!(
                "設定の適用に失敗しました。元の設定にロールバックしました: {error}"
            )))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullRestartFailurePath {
    BeforeTransaction,
    RestartOldFull,
    Recovered,
}

#[derive(Debug)]
enum FullRestartRecoveryError {
    Token(AppError),
    Restart(AppError),
    Resume(AppError),
}

fn full_restart_failure_path(transaction_started: bool, recovered: bool) -> FullRestartFailurePath {
    if !transaction_started {
        FullRestartFailurePath::BeforeTransaction
    } else if recovered {
        FullRestartFailurePath::Recovered
    } else {
        FullRestartFailurePath::RestartOldFull
    }
}

async fn recover_full_restart_failure<R, T, S, SFut, V, VFut>(
    path: FullRestartFailurePath,
    rollback: R,
    restore_token: T,
    restart_old_full: S,
    resume: V,
) -> Result<(), FullRestartRecoveryError>
where
    R: FnOnce(),
    T: FnOnce() -> AppResult<()>,
    S: FnOnce() -> SFut,
    SFut: std::future::Future<Output = AppResult<()>>,
    V: FnOnce() -> VFut,
    VFut: std::future::Future<Output = AppResult<()>>,
{
    match path {
        FullRestartFailurePath::BeforeTransaction => {
            rollback();
            resume().await.map_err(FullRestartRecoveryError::Resume)
        }
        FullRestartFailurePath::RestartOldFull => {
            rollback();
            restore_token().map_err(FullRestartRecoveryError::Token)?;
            restart_old_full()
                .await
                .map_err(FullRestartRecoveryError::Restart)?;
            resume().await.map_err(FullRestartRecoveryError::Resume)
        }
        FullRestartFailurePath::Recovered => Ok(()),
    }
}

/// Use the restart transaction that matches the saved MCP operation while
/// keeping the shared pre-start closure in one place. Token rotation needs a
/// stronger reservation than ordinary settings changes: it spans stopping the
/// old listener, mutating the keyring, and launching the replacement.
async fn restart_settings_plan<F>(
    sidecars: &crate::sidecar::Sidecars,
    mcp_rotate_auth_token: bool,
    restart_plan: RestartPlan,
    plugin_warnings: Vec<crate::sidecar::SidecarWarning>,
    before_start: F,
) -> AppResult<crate::sidecar::SidecarStartReport>
where
    F: FnOnce() -> AppResult<()>,
{
    if mcp_rotate_auth_token {
        sidecars
            .restart_plan_with_mcp_token_mutation(restart_plan, plugin_warnings, before_start)
            .await
    } else {
        sidecars
            .restart_plan_with(restart_plan, plugin_warnings, before_start)
            .await
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
    let jobworkerp_url = endpoints.require_jobworkerp_url()?;

    // Shared connect → reload → rollback pipeline (the batch rollback restores
    // every persisted file + the Keychain key, vs the single command's file).
    let result = super::llm_settings::connect_and_reload_llm_with_rollback(
        app,
        &jobworkerp_url,
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
        embedding_runtime: outcome.embedding_runtime,
        warnings: Vec::new(),
        mcp_auth_token: None,
    })
}

fn settings_apply_failure_message(error: &AppError) -> String {
    format!("設定の適用に失敗しました: {error}; 以前の設定にロールバックします")
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
    // Single app-settings.json restore — HF_HOME and timezone both live here
    // and share one pre-batch snapshot, so restoring it once undoes whichever
    // (or both) the batch changed without a double-restore clobber.
    if rollback.app_settings_changed
        && let Some(old) = &rollback.app_settings_old
    {
        let _ = paths::save_app_settings(&data.app_settings_path(), old);
    }
    if let Some(old) = &rollback.mcp {
        let _ = save_mcp_settings(&data.mcp_settings_path(), old);
    }
    if let Some(old) = &rollback.issue_tracker {
        let _ = save_issue_tracker_settings(&data.issue_tracker_settings_path(), old);
    }
}

/// Restore each persisted settings file to its pre-apply value.
fn rollback_files(state: &AppState, rollback: &RollbackState) {
    rollback_files_on(&state.data, rollback);
}

/// Bring the sidecar back up on whatever config is currently on disk.
/// Used by the rollback path after the files have been restored.
async fn restart_old_plan(app: &tauri::AppHandle, state: &AppState, plan: RestartPlan) {
    let plugin_warnings = super::stage_plugins_for_restart(app, &state.data.plugins_dir());
    if let Ok(report) = state.sidecars.restart_plan(plan, plugin_warnings).await {
        let _ = app.emit("sidecar://ready", &report);
    }
}

/// Restart the complete old configuration after a full transaction failed.
/// Unlike the selected-child rollback helper, this path also restores the
/// Issue Tracker child and its worker registrations under the same primitive.
async fn restart_old_full(app: &tauri::AppHandle, state: &AppState) -> AppResult<()> {
    let plugin_warnings = super::stage_plugins_for_restart(app, &state.data.plugins_dir());
    let report = state
        .sidecars
        .full_restart_with(plugin_warnings, false, || Ok(()))
        .await?;
    let _ = app.emit("sidecar://ready", &report);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::app_settings::SetHfHomeRequest;
    use crate::commands::connection::ConnectionConfig;
    use crate::commands::llm_settings::LlmMode;
    use crate::commands::mcp_settings::{
        MCP_KEYRING_TEST_LOCK, configure_mcp_auth_token_for_test, delete_mcp_auth_token,
    };
    use crate::data::DataPaths;
    use crate::data::paths::{AppSettings, HfHomeMode};
    use crate::sidecar::SidecarKind;
    use std::sync::MutexGuard;

    fn data_in(tmp: &std::path::Path) -> DataPaths {
        DataPaths::with_root(tmp.to_path_buf())
    }

    #[test]
    fn settings_apply_failure_message_uses_japanese_outer_text() {
        let message = settings_apply_failure_message(&AppError::Config("write failed".into()));

        assert_eq!(
            message,
            "設定の適用に失敗しました: config: write failed; 以前の設定にロールバックします"
        );
    }

    struct McpTestTokenGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl McpTestTokenGuard {
        fn configured() -> Self {
            let lock = MCP_KEYRING_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            configure_mcp_auth_token_for_test();
            Self { _lock: lock }
        }
    }

    impl Drop for McpTestTokenGuard {
        fn drop(&mut self) {
            delete_mcp_auth_token().expect("clear MCP test token");
        }
    }

    fn without_mcp_token() -> MutexGuard<'static, ()> {
        let lock = MCP_KEYRING_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        delete_mcp_auth_token().expect("clear MCP test token");
        lock
    }

    fn external_llm(model: &str) -> SetLlmSettingsRequest {
        SetLlmSettingsRequest {
            mode: LlmMode::External,
            provider_model: Some(model.to_string()),
            api_key: None, // never touch the Keychain in unit tests
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
            model_max_output_tokens: None,
            temperature: None,
            local_preset_id: Some(preset_id.to_string()),
            local_model_file: None,
            local_hf_repo: None,
            local_ctx_size: None,
            local_kv_cache_type: None,
            summary_output_guidance: None,
        }
    }

    fn embedding_preset(id: &str) -> SetEmbeddingSettingsRequest {
        SetEmbeddingSettingsRequest {
            preset_id: Some(id.to_string()),
            model_revision: None,
            custom_model_id: None,
            custom_tokenizer_id: None,
            custom_vector_size: None,
            custom_dtype: None,
            custom_max_sequence_length: None,
            custom_is_multimodal: None,
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
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        )
        .unwrap();
        assert!(!out.restart_needed);
    }

    #[test]
    fn setup_pending_chunk_guard_rejects_before_any_target_settings_write() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let current = EmbeddingSettings::default();
        let runtime = super::super::embedding_settings::resolve_embedding_runtime(&current);
        let request = SetEmbeddingSettingsRequest {
            preset_id: current.preset_id,
            custom_model_id: current.custom_model_id,
            custom_tokenizer_id: current.custom_tokenizer_id,
            custom_vector_size: current.custom_vector_size,
            custom_dtype: current.custom_dtype,
            custom_max_sequence_length: Some(runtime.max_sequence_length + 1),
            custom_is_multimodal: current.custom_is_multimodal,
            model_revision: current.model_revision,
        };

        let result = persist_settings_without_restart_approved(
            &data,
            ApplySettingsRequest {
                llm: Some(local_llm("target-llm")),
                embedding: Some(request),
                hf_home: Some(SetHfHomeRequest {
                    mode: HfHomeMode::DataRoot,
                    path: None,
                }),
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            EmbeddingPlanPermit::NoChange,
            true,
        );

        assert!(result.is_err());
        assert!(!data.embedding_settings_path().exists());
        assert!(!data.llm_settings_path().exists());
        assert!(!data.app_settings_path().exists());
    }

    #[test]
    fn setup_adopt_plan_authorizes_a_different_model_for_an_empty_target_root() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let requested = embedding_preset("qwen3-embedding-0-6b");

        persist_settings_without_restart_approved(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: Some(requested),
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            EmbeddingPlanPermit::Adopt,
            false,
        )
        .unwrap();

        assert_eq!(
            crate::commands::embedding_settings::load_embedding_settings(
                &data.embedding_settings_path()
            )
            .preset_id
            .as_deref(),
            Some("qwen3-embedding-0-6b")
        );
    }

    #[test]
    fn guidance_only_llm_change_needs_periodic_runtime_refresh_not_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        save_llm_settings(
            &data.llm_settings_path(),
            &LlmSettings {
                mode: LlmMode::External,
                provider_model: Some("gpt-4o".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let mut llm = external_llm("gpt-4o");
        let mut guidance = super::super::llm_settings::SummaryOutputGuidance::default();
        guidance.standard.weekly.item_chars = 640;
        guidance.standard.weekly.total_chars = 36_000;
        llm.summary_output_guidance = Some(guidance);

        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(llm),
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        )
        .unwrap();
        assert!(!out.restart_needed);
        assert!(out.periodic_runtime_refresh_needed);
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
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        )
        .unwrap();
        assert!(out.restart_needed, "both changes need exactly one restart");
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
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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
    fn external_llm_only_change_with_matching_key_still_restarts() {
        // Even when the running child already has the matching provider key,
        // an in-place worker replacement could split a summary's exact count
        // and completion across different model configurations.
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(external_llm("gpt-4o")),
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            Some("OPENAI_API_KEY"),
        )
        .unwrap();
        assert!(out.restart_needed);
        assert!(
            !out.is_llm_only_change(),
            "an External LLM change must restart to preserve summary model contracts"
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
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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
        let runtime = super::super::embedding_settings::resolve_embedding_runtime(
            &EmbeddingSettings::default(),
        );
        let tuning = SetEmbeddingSettingsRequest {
            preset_id: Some(super::super::embedding_presets::CUSTOM_EMBEDDING_PRESET_ID.into()),
            custom_model_id: Some(runtime.model_id),
            custom_vector_size: Some(runtime.vector_size),
            custom_dtype: Some("BF16".into()),
            ..embedding_preset(super::super::embedding_presets::DEFAULT_EMBEDDING_PRESET_ID)
        };
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: Some(tuning),
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        )
        .unwrap();
        // Runtime tuning still requires restarting the embedding worker.
        assert!(out.restart_needed);
        assert!(!out.is_llm_only_change());
    }

    #[test]
    fn embedding_migration_rejection_preserves_the_whole_batch() {
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
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        )
        .unwrap();
        let before = std::fs::read(data.embedding_settings_path()).unwrap();
        let result = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(external_llm("new-model")),
                embedding: Some(embedding_preset("qwen3-vl-embedding-2b")),
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(data.embedding_settings_path()).unwrap(),
            before
        );
        assert!(!data.llm_settings_path().exists());
    }

    #[test]
    fn pending_rebuild_rejects_chunking_changes_before_any_batch_file_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let original =
            embedding_preset(crate::commands::embedding_presets::DEFAULT_EMBEDDING_PRESET_ID);
        persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: Some(original.clone()),
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        )
        .unwrap();
        let before_embedding = std::fs::read(data.embedding_settings_path()).unwrap();
        let mut changed_chunking = original;
        changed_chunking.preset_id =
            Some(crate::commands::embedding_presets::CUSTOM_EMBEDDING_PRESET_ID.into());
        let old_runtime = crate::commands::embedding_settings::resolve_embedding_runtime(
            &crate::commands::embedding_settings::load_embedding_settings(
                &data.embedding_settings_path(),
            ),
        );
        changed_chunking.custom_model_id = Some(old_runtime.model_id);
        changed_chunking.custom_vector_size = Some(old_runtime.vector_size);
        changed_chunking.custom_dtype = Some(old_runtime.dtype);
        changed_chunking.custom_max_sequence_length = Some(old_runtime.max_sequence_length + 1);

        let result = persist_batch_to_disk_with_embedding_plan(
            &data,
            ApplySettingsRequest {
                llm: Some(external_llm("must-not-be-saved")),
                embedding: Some(changed_chunking),
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
            crate::experimental_features::ISSUE_TRACKING_BUILD_ALLOWS,
            Some(super::super::embedding_settings::EmbeddingPlanPermit::NoChange),
            true,
        );

        assert!(
            result.is_err(),
            "pending rebuilds forbid changing chunk input"
        );
        assert_eq!(
            std::fs::read(data.embedding_settings_path()).unwrap(),
            before_embedding
        );
        assert!(!data.llm_settings_path().exists());
    }

    #[test]
    fn remote_embedding_and_llm_batch_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        // Remote browse mode still allows local embedding settings because
        // semantic searches need the query model to match the remote vectors.
        crate::commands::connection::save_connection_config(
            &data.connection_config_path(),
            &ConnectionConfig::remote_pair("http://h:9000", "http://h:9010"),
        )
        .unwrap();
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(external_llm("gpt-4o")),
                embedding: Some(embedding_preset("qwen3-vl-embedding-2b")),
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        )
        .unwrap();
        assert!(out.embedding_changed);
        assert!(out.llm_changed);
        assert!(data.llm_settings_path().exists());
        let saved_embedding = crate::commands::embedding_settings::load_embedding_settings(
            &data.embedding_settings_path(),
        );
        assert_eq!(
            saved_embedding.preset_id.as_deref(),
            Some("qwen3-vl-embedding-2b")
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
            model_max_output_tokens: None,
            temperature: None,
            local_preset_id: Some("custom".into()),
            local_model_file: Some("model.bin".into()), // not .gguf → invalid
            local_hf_repo: Some("org/repo".into()),
            local_ctx_size: None,
            local_kv_cache_type: None,
            summary_output_guidance: None,
        };
        let res = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(bad_llm),
                embedding: Some(embedding_preset("qwen3-embedding-0-6b")),
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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
                    model_max_output_tokens: None,
                    ..external_llm("gpt-4o")
                }),
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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
                    model_max_output_tokens: None,
                    ..external_llm("gpt-4o")
                }),
                embedding: None,
                hf_home: Some(SetHfHomeRequest {
                    mode: HfHomeMode::DataRoot, // valid, but the write fails
                    path: None,
                }),
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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
            allow_unauthenticated_requests: false,
            rotate_auth_token: false,
            exclude_runner_as_tool: None,
            exclude_worker_as_tool: None,
            streaming: None,
            request_timeout_sec: None,
            rag_default_period_scope:
                crate::commands::mcp_settings::McpRagDefaultPeriodScope::OverallOnly,
        }
    }

    fn issue_tracker(enabled: Option<bool>) -> SetIssueTrackerSettingsRequest {
        SetIssueTrackerSettingsRequest { enabled }
    }

    #[test]
    fn issue_tracker_noop_when_unchanged_needs_no_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        super::super::issue_tracker_settings::save_issue_tracker_settings(
            &data.issue_tracker_settings_path(),
            &super::super::issue_tracker_settings::IssueTrackerSettings {
                enabled: Some(false),
            },
        )
        .unwrap();

        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: Some(issue_tracker(Some(false))),
                migration_backup: None,
            },
            None,
        )
        .unwrap();

        assert!(!out.issue_tracker_changed);
        assert!(!out.restart_needed);
        assert!(!out.requires_full_restart());
    }

    #[test]
    fn issue_tracker_restart_compares_effective_values_not_raw_settings() {
        let unset = IssueTrackerSettings { enabled: None };
        let explicitly_enabled = IssueTrackerSettings {
            enabled: Some(true),
        };
        let explicitly_disabled = IssueTrackerSettings {
            enabled: Some(false),
        };

        assert!(!issue_tracker_effective_value_changed(
            &unset,
            &explicitly_enabled,
            true
        ));
        assert!(issue_tracker_effective_value_changed(
            &unset,
            &explicitly_disabled,
            true
        ));
        assert!(!issue_tracker_effective_value_changed(
            &unset,
            &explicitly_enabled,
            false
        ));
        assert!(!issue_tracker_effective_value_changed(
            &unset,
            &explicitly_disabled,
            false
        ));
    }

    #[test]
    fn issue_tracker_change_requires_a_full_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        super::super::issue_tracker_settings::save_issue_tracker_settings(
            &data.issue_tracker_settings_path(),
            &super::super::issue_tracker_settings::IssueTrackerSettings {
                enabled: Some(false),
            },
        )
        .unwrap();

        let out = persist_batch_to_disk_with_build_allows(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: Some(issue_tracker(Some(true))),
                migration_backup: None,
            },
            None,
            true,
        )
        .unwrap();

        assert!(out.issue_tracker_changed);
        assert!(out.restart_needed);
        assert!(out.requires_full_restart());
    }

    #[test]
    fn llm_and_issue_tracker_changes_require_one_full_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        save_issue_tracker_settings(
            &data.issue_tracker_settings_path(),
            &IssueTrackerSettings {
                enabled: Some(false),
            },
        )
        .unwrap();

        let out = persist_batch_to_disk_with_build_allows(
            &data,
            ApplySettingsRequest {
                llm: Some(external_llm("gpt-4o")),
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: Some(issue_tracker(Some(true))),
                migration_backup: None,
            },
            None,
            true,
        )
        .unwrap();

        assert!(out.llm_changed);
        assert!(out.issue_tracker_changed);
        assert!(out.restart_needed);
        assert!(out.requires_full_restart());
        assert!(!out.is_llm_only_change());
    }

    #[test]
    fn issue_tracker_change_blocks_an_otherwise_safe_llm_hot_reload() {
        let outcome = PersistOutcome {
            rollback: RollbackState::default(),
            restart_needed: true,
            periodic_runtime_refresh_needed: false,
            embedding_runtime: None,
            llm_changed: true,
            embedding_changed: false,
            hf_home_changed: false,
            mcp_changed: false,
            mcp_rotate_auth_token: false,
            tz_changed: false,
            llm_hot_reload_safe: true,
            llm_new: None,
            issue_tracker_changed: true,
        };

        assert!(!outcome.is_llm_only_change());
        assert!(outcome.requires_full_restart());
    }

    #[test]
    fn issue_tracker_save_in_disallowed_build_persists_without_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let old = IssueTrackerSettings {
            enabled: Some(false),
        };
        save_issue_tracker_settings(&data.issue_tracker_settings_path(), &old).unwrap();

        let new = IssueTrackerSettings {
            enabled: Some(true),
        };
        let out = persist_batch_to_disk_with_build_allows(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: Some(issue_tracker(new.enabled)),
                migration_backup: None,
            },
            None,
            false,
        )
        .unwrap();

        assert!(!resolve_issue_tracking(&old, false));
        assert!(!resolve_issue_tracking(&new, false));
        assert!(!out.issue_tracker_changed);
        assert!(!out.restart_needed);
        assert!(!out.requires_full_restart());
        assert_eq!(
            load_issue_tracker_settings(&data.issue_tracker_settings_path()).enabled,
            Some(true)
        );
    }

    #[test]
    fn rollback_files_restores_issue_tracker_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        super::super::issue_tracker_settings::save_issue_tracker_settings(
            &data.issue_tracker_settings_path(),
            &super::super::issue_tracker_settings::IssueTrackerSettings {
                enabled: Some(true),
            },
        )
        .unwrap();

        let rollback = RollbackState {
            issue_tracker: Some(super::super::issue_tracker_settings::IssueTrackerSettings {
                enabled: Some(false),
            }),
            ..Default::default()
        };
        rollback_files_on(&data, &rollback);

        assert_eq!(
            super::super::issue_tracker_settings::load_issue_tracker_settings(
                &data.issue_tracker_settings_path()
            )
            .enabled,
            Some(false)
        );
    }

    #[test]
    fn mcp_only_change_needs_restart_not_hot_reload() {
        // Toggling MCP needs a sidecar restart (MCP_ENABLED is spawn-time
        // env) and must NEVER be split into the LLM-only hot-reload path —
        // that path doesn't restart the child, so the toggle wouldn't apply.
        let _token = McpTestTokenGuard::configured();
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: Some(enable_mcp()),
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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
        let plan = out.restart_plan();
        assert!(plan.includes(SidecarKind::Jobworkerp));
        assert!(!plan.includes(SidecarKind::Memories));
        assert!(!plan.includes(SidecarKind::Conductor));
    }

    #[test]
    fn mcp_token_rotation_restarts_only_jobworkerp_even_without_setting_change() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        save_mcp_settings(
            &data.mcp_settings_path(),
            &McpSettings {
                enabled: true,
                ..Default::default()
            },
        )
        .unwrap();
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: Some(SetMcpSettingsRequest {
                    rotate_auth_token: true,
                    ..enable_mcp()
                }),
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        )
        .unwrap();

        assert!(out.restart_needed);
        assert!(out.mcp_rotate_auth_token);
        let plan = out.restart_plan();
        assert!(plan.includes(SidecarKind::Jobworkerp));
        assert!(!plan.includes(SidecarKind::Memories));
        assert!(!plan.includes(SidecarKind::Conductor));
    }

    #[test]
    fn mcp_enable_without_token_rejects_the_whole_batch_before_persisting() {
        let _token_lock = without_mcp_token();
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());

        let error = match persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: Some(embedding_preset("qwen3-embedding-0-6b")),
                hf_home: None,
                mcp: Some(enable_mcp()),
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        ) {
            Ok(_) => panic!("MCP enable must be rejected without a bearer token"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("not configured"));
        assert!(
            !data.embedding_settings_path().exists(),
            "MCP authentication validation must happen before another card writes"
        );
        assert!(
            !data.mcp_settings_path().exists(),
            "a rejected MCP enable request must not be persisted"
        );
    }

    #[test]
    fn unauthenticated_mcp_enable_without_token_persists_the_whole_batch() {
        let _token_lock = without_mcp_token();
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());

        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: Some(embedding_preset("qwen3-embedding-0-6b")),
                hf_home: None,
                mcp: Some(SetMcpSettingsRequest {
                    allow_unauthenticated_requests: true,
                    ..enable_mcp()
                }),
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        )
        .unwrap();

        assert!(out.mcp_changed);
        assert!(data.embedding_settings_path().exists());
        assert!(
            crate::commands::mcp_settings::load_mcp_settings(&data.mcp_settings_path())
                .allow_unauthenticated_requests
        );
    }

    #[test]
    fn llm_plus_mcp_change_is_not_hot_reload_eligible() {
        // An External LLM change that WOULD be hot-reloadable on its own
        // (matching key env) must still fall back to a full restart when an
        // MCP toggle is co-changed — the MCP env only reaches the child at
        // spawn.
        let _token = McpTestTokenGuard::configured();
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(external_llm("gpt-4o")),
                embedding: None,
                hf_home: None,
                mcp: Some(enable_mcp()),
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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
        let _token = McpTestTokenGuard::configured();
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
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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
        let _token = McpTestTokenGuard::configured();
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        crate::commands::connection::save_connection_config(
            &data.connection_config_path(),
            &ConnectionConfig::remote_pair("http://h:9000", "http://h:9010"),
        )
        .unwrap();
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: Some(enable_mcp()),
                timezone: None,
                issue_tracker: None,
                migration_backup: None,
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

    // ── Timezone ──

    fn tz(name: &str) -> SetTimezoneRequest {
        SetTimezoneRequest {
            timezone: Some(name.to_string()),
        }
    }

    #[test]
    fn timezone_only_change_needs_restart_not_hot_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: Some(tz("America/New_York")),
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        )
        .unwrap();
        assert!(out.tz_changed);
        assert!(out.restart_needed);
        assert!(
            !out.is_llm_only_change(),
            "TZ is spawn-time env — never eligible for the LLM hot-reload"
        );
        assert_eq!(
            paths::load_app_settings(&data.app_settings_path())
                .timezone
                .as_deref(),
            Some("America/New_York")
        );
    }

    #[test]
    fn timezone_noop_when_unchanged_needs_no_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        crate::commands::app_settings::apply_timezone_to_disk(&data, tz("America/New_York"))
            .unwrap();
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: Some(tz("America/New_York")),
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        )
        .unwrap();
        assert!(!out.tz_changed);
        assert!(!out.restart_needed);
    }

    #[test]
    fn migration_backup_change_persists_without_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        crate::commands::app_settings::apply_timezone_to_disk(&data, tz("Asia/Tokyo")).unwrap();
        let backup = paths::MigrationBackupSettings {
            enabled: false,
            keep: 3,
        };
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: None,
                issue_tracker: None,
                migration_backup: Some(backup),
            },
            None,
        )
        .unwrap();
        assert!(
            !out.restart_needed,
            "the setting is read by the next startup migration only"
        );
        let saved = paths::load_app_settings(&data.app_settings_path());
        assert_eq!(saved.migration_backup, backup);
        assert_eq!(saved.timezone.as_deref(), Some("Asia/Tokyo"));
    }

    #[test]
    fn llm_plus_timezone_change_is_not_hot_reload_eligible() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        // External LLM whose key already matches (hot-reload-safe on its own),
        // co-changed with a timezone — the TZ change forces a full restart.
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: Some(external_llm("gpt-4o")),
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: Some(tz("Europe/Paris")),
                issue_tracker: None,
                migration_backup: None,
            },
            Some("OPENAI_API_KEY"),
        )
        .unwrap();
        assert!(out.tz_changed);
        assert!(
            !out.is_llm_only_change(),
            "a co-changed TZ must force the full restart path"
        );
    }

    #[test]
    fn timezone_persists_in_remote_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        // TZ drives the LOCAL sidecar's summary/import workflows, which run
        // regardless of browse mode — so it is not gated on remote (unlike
        // embedding). Persisting must succeed in remote mode.
        crate::commands::connection::save_connection_config(
            &data.connection_config_path(),
            &ConnectionConfig::remote_pair("http://h:9000", "http://h:9010"),
        )
        .unwrap();
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: None,
                mcp: None,
                timezone: Some(tz("Asia/Tokyo")),
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        )
        .unwrap();
        assert!(out.tz_changed);
        assert_eq!(
            paths::load_app_settings(&data.app_settings_path())
                .timezone
                .as_deref(),
            Some("Asia/Tokyo")
        );
    }

    #[test]
    fn hf_home_plus_timezone_batch_preserves_both() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        // Both persist into app-settings.json in one batch; the final file
        // must carry BOTH the new hf_home_mode AND the new timezone (pins the
        // shared-file ordering / `..old.clone()` preservation).
        let out = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: None,
                hf_home: Some(SetHfHomeRequest {
                    mode: HfHomeMode::DataRoot,
                    path: None,
                }),
                mcp: None,
                timezone: Some(tz("Europe/Berlin")),
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        )
        .unwrap();
        assert!(out.hf_home_changed);
        assert!(out.tz_changed);
        let saved = paths::load_app_settings(&data.app_settings_path());
        assert!(matches!(saved.hf_home_mode, HfHomeMode::DataRoot));
        assert_eq!(saved.timezone.as_deref(), Some("Europe/Berlin"));
    }

    #[test]
    fn rollback_restores_timezone() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        // Seed a pre-batch app-settings state (explicit zone A).
        crate::commands::app_settings::apply_timezone_to_disk(&data, tz("Asia/Tokyo")).unwrap();
        let pre_batch = paths::load_app_settings(&data.app_settings_path());
        // Apply zone B, then roll back using the pre-batch snapshot.
        crate::commands::app_settings::apply_timezone_to_disk(&data, tz("Europe/Paris")).unwrap();
        let rollback = RollbackState {
            app_settings_changed: true,
            app_settings_old: Some(pre_batch),
            ..Default::default()
        };
        rollback_files_on(&data, &rollback);
        assert_eq!(
            paths::load_app_settings(&data.app_settings_path())
                .timezone
                .as_deref(),
            Some("Asia/Tokyo"),
            "rollback must restore the timezone to its pre-batch value"
        );
    }

    #[test]
    fn rollback_restores_both_hf_home_and_timezone_from_one_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        // Pre-batch: Global hf_home + zone A.
        let pre_batch = AppSettings {
            hf_home_mode: HfHomeMode::Global,
            timezone: Some("Asia/Tokyo".into()),
            ..Default::default()
        };
        paths::save_app_settings(&data.app_settings_path(), &pre_batch).unwrap();
        // Mutate BOTH fields (as a co-changed batch would), then roll back
        // from the single pre-batch snapshot.
        crate::commands::app_settings::apply_hf_home_to_disk(
            &data,
            SetHfHomeRequest {
                mode: HfHomeMode::DataRoot,
                path: None,
            },
        )
        .unwrap();
        crate::commands::app_settings::apply_timezone_to_disk(&data, tz("Europe/Paris")).unwrap();
        let rollback = RollbackState {
            app_settings_changed: true,
            app_settings_old: Some(pre_batch),
            ..Default::default()
        };
        rollback_files_on(&data, &rollback);
        let restored = paths::load_app_settings(&data.app_settings_path());
        assert!(
            matches!(restored.hf_home_mode, HfHomeMode::Global),
            "the single snapshot must restore hf_home too"
        );
        assert_eq!(
            restored.timezone.as_deref(),
            Some("Asia/Tokyo"),
            "the single snapshot must restore timezone too"
        );
    }

    #[test]
    fn invalid_timezone_in_batch_does_not_persist_embedding() {
        // Guard-skip when there's no zoneinfo dir (the validator is permissive
        // there, so an "invalid" name would actually be accepted).
        if crate::commands::app_settings::zoneinfo_root_for_test().is_none() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let err = persist_batch_to_disk(
            &data,
            ApplySettingsRequest {
                llm: None,
                embedding: Some(embedding_preset("qwen3-vl-embedding-2b")),
                hf_home: None,
                mcp: None,
                timezone: Some(tz("Not/AZone")),
                issue_tracker: None,
                migration_backup: None,
            },
            None,
        );
        assert!(err.is_err(), "an invalid timezone must abort the batch");
        assert!(
            !data.embedding_settings_path().exists(),
            "the up-front validation must abort before any file is written"
        );
    }

    #[test]
    fn full_restart_failure_path_distinguishes_transaction_and_recovery_state() {
        assert_eq!(
            full_restart_failure_path(false, false),
            FullRestartFailurePath::BeforeTransaction
        );
        assert_eq!(
            full_restart_failure_path(true, false),
            FullRestartFailurePath::RestartOldFull
        );
        assert_eq!(
            full_restart_failure_path(true, true),
            FullRestartFailurePath::Recovered
        );
    }

    #[tokio::test]
    async fn full_restart_failure_effects_follow_each_recovery_branch() {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = |events: &std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
                      event: &'static str| {
            events.lock().unwrap().push(event);
        };

        let rollback = std::sync::Arc::clone(&events);
        let resume = std::sync::Arc::clone(&events);
        recover_full_restart_failure(
            FullRestartFailurePath::BeforeTransaction,
            move || record(&rollback, "rollback"),
            || Ok(()),
            || async { Ok(()) },
            move || {
                record(&resume, "resume");
                async { Ok(()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(*events.lock().unwrap(), vec!["rollback", "resume"]);

        events.lock().unwrap().clear();
        let rollback = std::sync::Arc::clone(&events);
        let token = std::sync::Arc::clone(&events);
        let restart = std::sync::Arc::clone(&events);
        let resume = std::sync::Arc::clone(&events);
        recover_full_restart_failure(
            FullRestartFailurePath::RestartOldFull,
            move || record(&rollback, "rollback"),
            move || {
                record(&token, "token");
                Ok(())
            },
            move || {
                record(&restart, "restart");
                async { Ok(()) }
            },
            move || {
                record(&resume, "resume");
                async { Ok(()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec!["rollback", "token", "restart", "resume"]
        );

        events.lock().unwrap().clear();
        recover_full_restart_failure(
            FullRestartFailurePath::Recovered,
            || panic!("recovered branch must not rollback"),
            || panic!("recovered branch must not restore token"),
            || async { panic!("recovered branch must not restart") },
            || async { panic!("recovered branch must not resume") },
        )
        .await
        .unwrap();
        assert!(events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn full_restart_old_full_branch_restores_issue_tracker_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let data = data_in(tmp.path());
        let old = IssueTrackerSettings {
            enabled: Some(false),
        };
        save_issue_tracker_settings(&data.issue_tracker_settings_path(), &old).unwrap();
        save_issue_tracker_settings(
            &data.issue_tracker_settings_path(),
            &IssueTrackerSettings {
                enabled: Some(true),
            },
        )
        .unwrap();

        let rollback = RollbackState {
            issue_tracker: Some(old),
            ..Default::default()
        };
        recover_full_restart_failure(
            FullRestartFailurePath::RestartOldFull,
            || rollback_files_on(&data, &rollback),
            || Ok(()),
            || async { Ok(()) },
            || async { Ok(()) },
        )
        .await
        .unwrap();

        assert_eq!(
            load_issue_tracker_settings(&data.issue_tracker_settings_path()).enabled,
            Some(false)
        );
    }
}
