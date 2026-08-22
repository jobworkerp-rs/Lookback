//! Recovery commands invoked from the [`BootError`](../../../src/components/BootError.tsx)
//! frontend when the sidecar surfaces a structured startup failure.

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Manager};
use tokio::sync::{Mutex, MutexGuard};

use super::embedding_settings::{
    EvacuateMode, evacuate_vectordb, load_embedding_settings, save_embedding_settings,
};
use super::{AppState, embedding_presets};
use crate::error::{AppError, AppResult};

/// Result of a recovery action. `restarted` is `true` when the sidecar
/// completed its TCP-listen probe after the rewrite; `restart_error`
/// carries the message of a second failure so the UI can render
/// "applied the fix but the restart still failed: ...".
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryResult {
    pub restarted: bool,
    pub backup_path: Option<PathBuf>,
    pub restart_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DatabaseRestoreState {
    SafeStopped,
}

/// A successful database restore intentionally leaves every sidecar stopped.
/// Starting the migration gate is a separate, explicit user action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseRestoreResult {
    pub state: DatabaseRestoreState,
    pub backup_path: PathBuf,
}

async fn lock_database_migration_recovery(lock: &Mutex<()>) -> MutexGuard<'_, ()> {
    lock.lock().await
}

// Recovery mutex is intentionally acquired before Sidecars' lifecycle mutex;
// every migration recovery command follows this lock ordering.
async fn with_database_migration_recovery_lock<F, Fut, T>(lock: &Mutex<()>, operation: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    let _recovery_guard = lock_database_migration_recovery(lock).await;
    operation().await
}

/// Stop the sidecars, rename the existing lancedb tree under
/// `<data>/lancedb-backup/lancedb-<ts>/`, then re-run the standard
/// startup pipeline.
#[tauri::command]
pub async fn recover_evacuate_lancedb(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<RecoveryResult> {
    with_database_migration_recovery_lock(&state.database_migration_recovery, || async {
        run_lancedb_recovery(&app, &state, EvacuateMode::Evacuate).await
    })
    .await
}

/// Same as [`recover_evacuate_lancedb`] but deletes the existing lancedb tree.
#[tauri::command]
pub async fn recover_purge_lancedb(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<RecoveryResult> {
    with_database_migration_recovery_lock(&state.database_migration_recovery, || async {
        run_lancedb_recovery(&app, &state, EvacuateMode::Delete).await
    })
    .await
}

async fn run_lancedb_recovery(
    app: &AppHandle,
    state: &AppState,
    mode: EvacuateMode,
) -> AppResult<RecoveryResult> {
    state.invalidate_clients().await;
    state.sidecars.stop().await?;
    let backup_path = evacuate_vectordb(&state.data, mode)?;

    crate::stage_and_start_sidecars(app, &state.sidecars, &state.data).await;
    let restart_error = state.sidecars.last_start_error();
    Ok(RecoveryResult {
        restarted: restart_error.is_none(),
        backup_path,
        restart_error,
    })
}

/// Null out a `preset_id` that no longer exists in the curated list, then
/// restart without touching LanceDB.
#[tauri::command]
pub async fn recover_reset_embedding_settings(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<RecoveryResult> {
    with_database_migration_recovery_lock(&state.database_migration_recovery, || async {
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
    })
    .await
}

#[tauri::command]
pub async fn retry_database_migration(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> AppResult<RecoveryResult> {
    with_database_migration_recovery_lock(&state.database_migration_recovery, || async {
        if !state.sidecars.owns_instance_lock() {
            return Err(AppError::AnotherInstanceRunning);
        }
        state.invalidate_clients().await;
        crate::sidecar::migration_gate::request_explicit_retry(&state.data)?;
        crate::stage_and_start_sidecars(&app, &state.sidecars, &state.data).await;
        let restart_error = state.sidecars.last_start_error();
        Ok(RecoveryResult {
            restarted: restart_error.is_none(),
            backup_path: None,
            restart_error,
        })
    })
    .await
}

#[cfg(test)]
trait DatabaseRestoreRuntime {
    fn owns_instance_lock(&self) -> bool;
    async fn stop_sidecars(&self) -> AppResult<()>;
    async fn invalidate_clients(&self);
}

#[cfg(test)]
impl DatabaseRestoreRuntime for AppState {
    fn owns_instance_lock(&self) -> bool {
        self.sidecars.owns_instance_lock()
    }

    async fn stop_sidecars(&self) -> AppResult<()> {
        self.sidecars.stop_for_maintenance().await
    }

    async fn invalidate_clients(&self) {
        self.invalidate_clients().await;
    }
}

#[cfg(test)]
async fn prepare_database_restore(runtime: &impl DatabaseRestoreRuntime) -> AppResult<()> {
    if !runtime.owns_instance_lock() {
        return Err(AppError::AnotherInstanceRunning);
    }
    runtime.stop_sidecars().await?;
    runtime.invalidate_clients().await;
    Ok(())
}

#[cfg(test)]
async fn run_database_restore(
    runtime: &impl DatabaseRestoreRuntime,
    requested_backup: &Path,
    restore: impl FnOnce(&Path) -> AppResult<PathBuf>,
) -> AppResult<DatabaseRestoreResult> {
    prepare_database_restore(runtime).await?;
    let backup_path = restore(requested_backup)?;
    Ok(DatabaseRestoreResult {
        state: DatabaseRestoreState::SafeStopped,
        backup_path,
    })
}

async fn run_database_restore_with_sidecars(
    state: &AppState,
    requested_backup: &Path,
    restore: impl FnOnce(&Path) -> AppResult<PathBuf>,
) -> AppResult<DatabaseRestoreResult> {
    state
        .sidecars
        .with_maintenance_lock(|| async {
            state.invalidate_clients().await;
            let backup_path = restore(requested_backup)?;
            Ok(DatabaseRestoreResult {
                state: DatabaseRestoreState::SafeStopped,
                backup_path,
            })
        })
        .await
}

#[tauri::command]
pub async fn restore_database_migration_backup(
    state: tauri::State<'_, AppState>,
    backup_path: String,
) -> AppResult<DatabaseRestoreResult> {
    with_database_migration_recovery_lock(&state.database_migration_recovery, || async {
        run_database_restore_with_sidecars(&state, Path::new(&backup_path), |requested| {
            crate::sidecar::migration_gate::restore_backup(&state.data, requested)
        })
        .await
    })
    .await
}

fn reset_embedding_preset_inner(path: &Path) -> AppResult<()> {
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

/// Open the log directory in the OS file browser.
#[tauri::command]
pub fn open_log_dir(state: tauri::State<'_, AppState>) -> AppResult<()> {
    open_log_dir_at(&state.data.log_dir(), |path| open::that(path))
}

fn open_log_dir_at(dir: &Path, opener: impl FnOnce(&Path) -> std::io::Result<()>) -> AppResult<()> {
    if !dir.exists() {
        fs::create_dir_all(dir)
            .map_err(|e| AppError::Config(format!("create log dir {}: {e}", dir.display())))?;
    }
    opener(dir).map_err(|e| AppError::Config(format!("open log directory {}: {e}", dir.display())))
}

/// Cleanly quit the app from the BootError UI.
#[tauri::command]
pub async fn quit_app(state: tauri::State<'_, AppState>, app: AppHandle) -> AppResult<()> {
    let _ = state.sidecars.stop().await;
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::super::embedding_settings::EmbeddingSettings;
    use super::*;

    struct FakeDatabaseRestoreRuntime {
        steps: Mutex<Vec<&'static str>>,
        owns_lock: bool,
        stop_error: bool,
    }

    impl DatabaseRestoreRuntime for FakeDatabaseRestoreRuntime {
        fn owns_instance_lock(&self) -> bool {
            self.owns_lock
        }

        async fn stop_sidecars(&self) -> AppResult<()> {
            self.steps.lock().unwrap().push("stop");
            if self.stop_error {
                Err(AppError::Config("stop failed".into()))
            } else {
                Ok(())
            }
        }

        async fn invalidate_clients(&self) {
            self.steps.lock().unwrap().push("invalidate");
        }
    }

    fn write_settings(path: &Path, preset: Option<&str>) {
        let settings = EmbeddingSettings {
            preset_id: preset.map(String::from),
            ..Default::default()
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        save_embedding_settings(path, &settings).unwrap();
    }

    #[test]
    fn open_log_dir_creates_the_directory_before_invoking_the_opener() {
        let tmp = tempfile::tempdir().unwrap();
        let log_dir = tmp.path().join("log");
        let calls = std::cell::Cell::new(0);

        open_log_dir_at(&log_dir, |path| {
            calls.set(calls.get() + 1);
            assert_eq!(path, log_dir);
            assert!(path.is_dir());
            Ok(())
        })
        .unwrap();

        assert_eq!(calls.get(), 1);
        assert!(log_dir.is_dir());
    }

    #[test]
    fn open_log_dir_preserves_opener_failure_context() {
        let tmp = tempfile::tempdir().unwrap();
        let log_dir = tmp.path().join("log");

        let error = open_log_dir_at(&log_dir, |_| {
            Err(std::io::Error::other("desktop launcher unavailable"))
        })
        .unwrap_err()
        .to_string();

        assert!(log_dir.is_dir());
        assert!(error.contains("open log directory"));
        assert!(error.contains(&log_dir.display().to_string()));
        assert!(error.contains("desktop launcher unavailable"));
    }

    #[test]
    fn open_log_dir_invokes_the_opener_for_an_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let log_dir = tmp.path().join("log");
        fs::create_dir(&log_dir).unwrap();
        let calls = std::cell::Cell::new(0);

        open_log_dir_at(&log_dir, |_| {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap();

        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn reset_nulls_unknown_preset_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("embedding-settings.json");
        write_settings(&path, Some("ruri-v3-310m"));
        reset_embedding_preset_inner(&path).unwrap();
        assert!(load_embedding_settings(&path).preset_id.is_none());
    }

    #[test]
    fn reset_keeps_curated_preset_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("embedding-settings.json");
        let curated = embedding_presets::default_preset().id;
        write_settings(&path, Some(curated));
        reset_embedding_preset_inner(&path).unwrap();
        assert_eq!(
            load_embedding_settings(&path).preset_id.as_deref(),
            Some(curated)
        );
    }

    #[test]
    fn reset_keeps_custom_or_null_preset_id() {
        for preset in [Some(embedding_presets::CUSTOM_EMBEDDING_PRESET_ID), None] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("embedding-settings.json");
            write_settings(&path, preset);
            reset_embedding_preset_inner(&path).unwrap();
            assert_eq!(load_embedding_settings(&path).preset_id.as_deref(), preset);
        }
    }

    #[tokio::test]
    async fn database_restore_stops_sidecars_before_invalidating_clients() {
        let runtime = FakeDatabaseRestoreRuntime {
            steps: Mutex::new(Vec::new()),
            owns_lock: true,
            stop_error: false,
        };

        prepare_database_restore(&runtime).await.unwrap();

        assert_eq!(*runtime.steps.lock().unwrap(), ["stop", "invalidate"]);
    }

    #[tokio::test]
    async fn database_restore_does_not_invalidate_clients_when_stop_fails() {
        let runtime = FakeDatabaseRestoreRuntime {
            steps: Mutex::new(Vec::new()),
            owns_lock: true,
            stop_error: true,
        };

        assert!(
            run_database_restore(&runtime, Path::new("/backup/a"), |_| {
                runtime.steps.lock().unwrap().push("restore");
                Ok(PathBuf::from("/backup/a"))
            })
            .await
            .is_err()
        );
        assert_eq!(*runtime.steps.lock().unwrap(), ["stop"]);
    }

    #[tokio::test]
    async fn database_restore_rejects_when_this_process_does_not_hold_the_lock() {
        let runtime = FakeDatabaseRestoreRuntime {
            steps: Mutex::new(Vec::new()),
            owns_lock: false,
            stop_error: false,
        };

        assert!(matches!(
            prepare_database_restore(&runtime).await,
            Err(AppError::AnotherInstanceRunning)
        ));
        assert!(runtime.steps.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn database_restore_finishes_safe_stopped_without_starting_the_gate() {
        let runtime = FakeDatabaseRestoreRuntime {
            steps: Mutex::new(Vec::new()),
            owns_lock: true,
            stop_error: false,
        };

        let result = run_database_restore(&runtime, Path::new("/backup/a"), |requested| {
            runtime.steps.lock().unwrap().push("restore");
            Ok(requested.to_path_buf())
        })
        .await
        .unwrap();

        assert_eq!(result.state, DatabaseRestoreState::SafeStopped);
        assert_eq!(result.backup_path, PathBuf::from("/backup/a"));
        assert_eq!(
            *runtime.steps.lock().unwrap(),
            ["stop", "invalidate", "restore"]
        );
    }

    #[tokio::test]
    async fn database_restore_remains_fail_closed_when_restore_fails() {
        let runtime = FakeDatabaseRestoreRuntime {
            steps: Mutex::new(Vec::new()),
            owns_lock: true,
            stop_error: false,
        };

        let result = run_database_restore(&runtime, Path::new("/backup/a"), |_| {
            runtime.steps.lock().unwrap().push("restore");
            Err(AppError::Config("restore failed".into()))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(
            *runtime.steps.lock().unwrap(),
            ["stop", "invalidate", "restore"]
        );
    }

    #[tokio::test]
    async fn database_migration_recovery_lock_excludes_a_concurrent_action() {
        let lock = tokio::sync::Mutex::new(());
        let guard = lock_database_migration_recovery(&lock).await;

        assert!(lock.try_lock().is_err());

        drop(guard);
        assert!(lock.try_lock().is_ok());
    }

    #[tokio::test]
    async fn database_migration_recovery_actions_serialize_and_release_after_failure() {
        let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let first_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let first = {
            let lock = std::sync::Arc::clone(&lock);
            let first_started = std::sync::Arc::clone(&first_started);
            tokio::spawn(async move {
                with_database_migration_recovery_lock(&lock, || async move {
                    first_started.notify_one();
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    Err::<(), _>(AppError::Config("restore failed".into()))
                })
                .await
            })
        };
        first_started.notified().await;

        let mut second = {
            let lock = std::sync::Arc::clone(&lock);
            tokio::spawn(async move {
                with_database_migration_recovery_lock(&lock, || async { Ok::<_, AppError>(()) })
                    .await
            })
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut second)
                .await
                .is_err(),
            "a concurrent recovery action must wait for restore"
        );
        assert!(first.await.unwrap().is_err());
        assert!(second.await.unwrap().is_ok());
    }
}
