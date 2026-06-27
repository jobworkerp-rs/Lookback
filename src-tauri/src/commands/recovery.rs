//! Recovery commands invoked from the [`BootError`](../../../src/components/BootError.tsx)
//! frontend when the sidecar surfaces a structured startup failure. Each
//! command either rewrites local state (evacuate / purge lancedb,
//! null-out a stale embedding preset) and then re-runs the standard
//! sidecar startup pipeline, or — for the unrecoverable variants —
//! exposes the data dir / quits the app so the user can intervene
//! manually.
//!
//! The lancedb-touching commands reuse [`super::embedding_settings::evacuate_vectordb`]
//! so the rename-vs-delete semantics stay identical to the embedding-swap
//! path. `stage_and_start_sidecars` is called explicitly afterwards
//! rather than relying on a `Sidecars::start` shortcut, because the
//! Tauri side normally stages plugins on the same pre-start hop and the
//! restart must mirror that.

use std::path::PathBuf;

use serde::Serialize;
use tauri::{AppHandle, Manager};

use super::embedding_settings::{
    EvacuateMode, evacuate_vectordb, load_embedding_settings, save_embedding_settings,
};
use super::{AppState, embedding_presets};
use crate::error::{AppError, AppResult};

/// Result of a recovery action. `restarted` is `true` when the sidecar
/// completed its TCP-listen probe after the rewrite; `restart_error`
/// carries the message of a second failure so the UI can render
/// "applied the fix but the restart still failed: ...".
///
/// `backup_path` is `Some` only after [`recover_evacuate_lancedb`] —
/// the other commands have nothing to back up.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryResult {
    pub restarted: bool,
    pub backup_path: Option<PathBuf>,
    pub restart_error: Option<String>,
}

/// Stop the sidecars, rename the existing lancedb tree under
/// `<data>/lancedb-backup/lancedb-<ts>/`, then re-run the standard
/// startup pipeline. Use this when the user wants the dimension
/// mismatch fixed without losing the existing vectors (they can be
/// restored manually from the backup).
#[tauri::command]
pub async fn recover_evacuate_lancedb(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<RecoveryResult> {
    run_lancedb_recovery(&app, &state, EvacuateMode::Evacuate).await
}

/// Same as [`recover_evacuate_lancedb`] but `rm -rf`s the existing
/// lancedb tree instead of renaming it. Last-resort branch when the
/// disk is too full to hold a copy.
#[tauri::command]
pub async fn recover_purge_lancedb(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<RecoveryResult> {
    run_lancedb_recovery(&app, &state, EvacuateMode::Delete).await
}

async fn run_lancedb_recovery(
    app: &AppHandle,
    state: &AppState,
    mode: EvacuateMode,
) -> AppResult<RecoveryResult> {
    // Drop cached gRPC clients first — they hold a `Channel` against the
    // old sidecar's port, which becomes unusable as soon as we stop the
    // process below.
    state.invalidate_clients().await;
    state.sidecars.stop().await?;

    // Re-running the failed startup right away would re-hit the same
    // schema-mismatch panic; the evacuation step is what makes the
    // restart viable.
    let backup_path = evacuate_vectordb(&state.data, mode)?;

    crate::stage_and_start_sidecars(app, &state.sidecars, &state.data).await;
    let restart_error = state.sidecars.last_start_error();
    Ok(RecoveryResult {
        restarted: restart_error.is_none(),
        backup_path,
        restart_error,
    })
}

/// Null out a `preset_id` that no longer exists in the curated list
/// (typical after a release that retired a preset — e.g. removing Ruri
/// without `evacuate`ing first) and re-run the standard startup
/// pipeline. The lancedb directory is intentionally NOT touched; if the
/// stored vectors are also at the wrong dimension the BootError UI will
/// surface again and the user can pick the evacuate / purge action
/// next.
#[tauri::command]
pub async fn recover_reset_embedding_settings(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<RecoveryResult> {
    reset_embedding_preset_inner(&state.data.embedding_settings_path())?;
    state.invalidate_clients().await;
    state.sidecars.stop().await?;
    crate::stage_and_start_sidecars(app.app_handle(), &state.sidecars, &state.data).await;
    let restart_error = state.sidecars.last_start_error();
    Ok(RecoveryResult {
        restarted: restart_error.is_none(),
        backup_path: None,
        restart_error,
    })
}

/// Pure helper extracted so the rewrite policy ("null out only unknown
/// presets, keep null / curated / custom alone") can be unit-tested
/// without spinning up a Tauri AppHandle.
fn reset_embedding_preset_inner(path: &std::path::Path) -> AppResult<()> {
    let mut settings = load_embedding_settings(path);
    let needs_reset = match settings.preset_id.as_deref() {
        None => false,
        Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID) => false,
        Some(id) => embedding_presets::find_preset(id).is_none(),
    };
    if needs_reset {
        settings.preset_id = None;
        save_embedding_settings(path, &settings)?;
    }
    Ok(())
}

/// Open the log directory in Finder. Used as an escape hatch on the
/// BootError UI when the failure is not auto-recoverable (e.g. corrupt
/// LanceDB, env var typo) so the user can collect logs before
/// reporting. Lookback is macOS-only (see README "リリースビルド"); the
/// `open` invocation is therefore not gated behind a cross-platform
/// shim.
///
/// Uses `std::process::Command` rather than `tauri-plugin-shell::open`
/// so we don't have to widen the shell capability scope to the data
/// root.
#[tauri::command]
pub fn open_log_dir(state: tauri::State<'_, AppState>) -> AppResult<()> {
    let dir = state.data.log_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| AppError::Config(format!("create log dir {}: {e}", dir.display())))?;
    }
    let status = std::process::Command::new("open")
        .arg(&dir)
        .status()
        .map_err(|e| AppError::Config(format!("spawn `open` failed: {e}")))?;
    if !status.success() {
        return Err(AppError::Config(format!(
            "`open {}` exited with {status}",
            dir.display()
        )));
    }
    Ok(())
}

/// Cleanly quit the app from the BootError UI. Stops the sidecars
/// first so a structured-error popup doesn't leave orphan children
/// behind.
#[tauri::command]
pub async fn quit_app(state: tauri::State<'_, AppState>, app: AppHandle) -> AppResult<()> {
    // Best-effort stop: a partial start may have left some children
    // alive; subsequent launches reap orphans by PID file so a failure
    // here isn't fatal.
    let _ = state.sidecars.stop().await;
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::embedding_settings::EmbeddingSettings;
    use super::*;

    fn write_settings(path: &std::path::Path, preset: Option<&str>) {
        let s = EmbeddingSettings {
            preset_id: preset.map(String::from),
            ..Default::default()
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        save_embedding_settings(path, &s).unwrap();
    }

    #[test]
    fn reset_nulls_unknown_preset_id() {
        // Today's actual incident: a release removed `ruri-v3-310m` from
        // the curated list while a user still had it stored. Resolve
        // falls back to default (different dim) and the LanceDB schema
        // mismatch panic follows. The recovery command must rewrite the
        // saved preset to `null` so the next launch resolves to the
        // default preset cleanly.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("embedding-settings.json");
        write_settings(&p, Some("ruri-v3-310m"));
        reset_embedding_preset_inner(&p).unwrap();
        let after = load_embedding_settings(&p);
        assert!(after.preset_id.is_none());
    }

    #[test]
    fn reset_keeps_curated_preset_id() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("embedding-settings.json");
        let curated = embedding_presets::default_preset().id;
        write_settings(&p, Some(curated));
        reset_embedding_preset_inner(&p).unwrap();
        let after = load_embedding_settings(&p);
        assert_eq!(after.preset_id.as_deref(), Some(curated));
    }

    #[test]
    fn reset_keeps_custom_sentinel() {
        // "custom" routes through the free-text branch; treat it as
        // valid so a user with a deliberate Custom config isn't reset
        // out from under them.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("embedding-settings.json");
        write_settings(&p, Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID));
        reset_embedding_preset_inner(&p).unwrap();
        let after = load_embedding_settings(&p);
        assert_eq!(
            after.preset_id.as_deref(),
            Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID)
        );
    }

    #[test]
    fn reset_keeps_null_preset_id() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("embedding-settings.json");
        write_settings(&p, None);
        reset_embedding_preset_inner(&p).unwrap();
        let after = load_embedding_settings(&p);
        assert!(after.preset_id.is_none());
    }
}
