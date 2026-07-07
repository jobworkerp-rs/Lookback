//! Tauri commands backing the Settings page.

use std::path::PathBuf;

use serde::Serialize;
use tauri::State;
use tracing::warn;

use crate::error::AppResult;
use crate::sidecar::{SidecarErrorPayload, SidecarStartReport};

use super::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct SettingsSnapshot {
    /// Application data root; the single delete target.
    pub data_root: PathBuf,

    pub sqlite_path: PathBuf,
    pub lancedb_path: PathBuf,
    pub plugins_path: PathBuf,
    pub models_path: PathBuf,
    pub log_path: PathBuf,

    pub jobworkerp_url: Option<String>,
    pub memories_url: Option<String>,
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> AppResult<SettingsSnapshot> {
    let data = &state.data;
    let eps = state.sidecars.current_endpoints();
    Ok(SettingsSnapshot {
        data_root: data.root.clone(),
        sqlite_path: data.sqlite_path(),
        lancedb_path: data.lancedb_dir(),
        plugins_path: data.plugins_dir(),
        models_path: data.models_dir(),
        log_path: data.log_dir(),
        jobworkerp_url: eps.as_ref().map(|e| e.jobworkerp_url()),
        memories_url: eps.map(|e| e.memories_url()),
    })
}

/// Immediate snapshot of the most recent sidecar lifecycle outcome
/// (either a successful ready report, or a structured / raw failure).
/// Lets the frontend fetch status on mount instead of racing the
/// one-shot `sidecar://ready` / `sidecar://error` events — a listener
/// that mounted *after* either event already fired would otherwise stay
/// stuck on the boot spinner forever. Both fields are `None` while a
/// fresh start is still in flight.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SidecarStatusSnapshot {
    pub ready: Option<SidecarStartReport>,
    pub failure: Option<SidecarErrorPayload>,
}

#[tauri::command]
pub fn get_sidecar_status(state: State<'_, AppState>) -> AppResult<SidecarStatusSnapshot> {
    Ok(SidecarStatusSnapshot {
        ready: state.sidecars.last_report(),
        failure: state.sidecars.last_start_failure(),
    })
}

/// Outcome of `purge_all_data`. The data root deletion is the primary
/// effect and a hard failure there returns `Err`; secondary cleanups
/// (Keychain entries that live outside the data root) are best-effort,
/// and their failure messages are surfaced via `warnings` so the UI can
/// alert the user without misrepresenting that the main purge succeeded.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PurgeReport {
    pub warnings: Vec<String>,
}

fn restore_completed_bootstrap_after_purge(
    path: &std::path::Path,
    bootstrap: &crate::data::paths::BootstrapConfig,
) -> AppResult<()> {
    if bootstrap.setup_completed {
        crate::data::paths::save_bootstrap_config(path, bootstrap)?;
    }
    Ok(())
}

/// Stop sidecars and remove the entire data root.
///
/// Also deletes the macOS Keychain entry for the External LLM API key.
/// Keychain failures don't fail the whole command — by the time we reach
/// keychain cleanup the data root is already gone and the user expects
/// "delete all data" to have happened; we instead return a warning so
/// the UI can tell the user to remove the entry manually (otherwise the
/// secret would silently persist across a "full reset").
#[tauri::command]
pub async fn purge_all_data(state: State<'_, AppState>) -> AppResult<PurgeReport> {
    let bootstrap_path = crate::data::paths::bootstrap_path()?;
    let bootstrap = crate::data::paths::load_bootstrap_config(&bootstrap_path);
    state.sidecars.stop().await?;
    // Drop cached gRPC clients so nothing keeps a handle to the now-stopped
    // sidecar (mirrors retry_model_setup).
    state.invalidate_clients().await;
    crate::data::paths::purge(&state.data.root)?;
    restore_completed_bootstrap_after_purge(&bootstrap_path, &bootstrap)?;

    let mut report = PurgeReport::default();
    if let Err(msg) = super::llm_settings::delete_api_key() {
        warn!(error = %msg, "failed to delete LLM API key from Keychain during purge");
        report.warnings.push(format!(
            "Keychain の LLM API キーを削除できませんでした ({msg})。Keychain Access で \
             'lookback' / 'llm-api-key' を手動削除してください。"
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_restore_keeps_completed_bootstrap_and_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bootstrap.json");
        let config = crate::data::paths::BootstrapConfig {
            data_root_override: Some(std::path::PathBuf::from("/tmp/lookback-custom")),
            setup_completed: true,
        };
        restore_completed_bootstrap_after_purge(&path, &config).unwrap();
        assert_eq!(crate::data::paths::load_bootstrap_config(&path), config);
    }
}
