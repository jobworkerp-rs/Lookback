use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use super::super::AppState;
use crate::error::{AppError, AppResult};

/// Completes the single maintenance scope after every source and selected
/// downstream generation step reaches a terminal outcome, including cancel
/// and early failure paths in the detached import driver.
pub(super) struct ImportMaintenanceGuard {
    pub(super) app: AppHandle,
    pub(super) registered: bool,
}

impl Drop for ImportMaintenanceGuard {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        let app = self.app.clone();
        tauri::async_runtime::spawn(async move {
            if let Some(state) = app.try_state::<AppState>()
                && let Err(error) = super::super::finish_search_index_write(&state, true).await
            {
                tracing::warn!(%error, "import terminal maintenance reconcile failed");
            }
        });
    }
}

/// Single source of truth for the reflection `prompt_version`. Bumping this
/// string makes regeneration produce fresh reflections: the tuple
/// (thread_id, prompt_version, reflector_id) is the idempotency key in
/// FinalizeReflection, so `thread-reflection-single.yaml`'s `skipIfExisting`
/// step short-circuits any thread that already has a reflection under the same
/// version. Bump this in lockstep with a reflector prompt change. Shared by
/// the import pipeline (here) and the manual dispatch (`reflection_dispatch.rs`).
pub const REFLECTION_PROMPT_VERSION: &str = "20260525-reflexion";
pub(super) const CANCELLED_ACTIVE_MESSAGE: &str = "中断";
pub(super) const CANCELLED_SKIP_MESSAGE: &str = "中断によりスキップ";
static IMPORT_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Entropy is normally supplied by the OS.  The monotonic suffix keeps an ID
/// unique within this process even on a platform where the RNG is temporarily
/// unavailable, which is preferable to collapsing independent generations
/// into the same seconds-shaped identifier.
static SUMMARY_GENERATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) fn summary_generation_id(started_at: chrono::DateTime<chrono::Utc>) -> String {
    let suffix = getrandom::u64()
        .unwrap_or_else(|_| SUMMARY_GENERATION_SEQUENCE.fetch_add(1, Ordering::Relaxed));
    format!("{}-{suffix:016x}", started_at.format("%Y%m%d%H%M%S%3f"))
}

pub(super) struct ImportExecutionGuard;

impl ImportExecutionGuard {
    pub(super) fn acquire() -> AppResult<Self> {
        IMPORT_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| AppError::Config("another import is already running".into()))?;
        Ok(Self)
    }
}

impl Drop for ImportExecutionGuard {
    fn drop(&mut self) {
        IMPORT_ACTIVE.store(false, Ordering::Release);
    }
}

/// Closes the native child-log scope after the detached import driver has
/// finished, including cancellation and early-return paths. Retention only
/// considers scopes marked closed, so this guard prevents deleting a running
/// import's native file.
pub(super) struct NativeImportLogGuard(pub(super) PathBuf);

impl Drop for NativeImportLogGuard {
    fn drop(&mut self) {
        if let Err(error) = crate::log_rotation::mark_native_log_dir_closed(&self.0) {
            tracing::debug!(path = %self.0.display(), %error, "could not mark native import log closed");
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum ImportSource {
    ClaudeCode,
    Codex,
    #[serde(rename = "opencode")]
    OpenCode,
    Plain,
}

impl ImportSource {
    pub(super) fn cli_subcommand(self) -> &'static str {
        match self {
            ImportSource::ClaudeCode => "claude-code",
            ImportSource::Codex => "codex",
            ImportSource::OpenCode => "opencode",
            ImportSource::Plain => "plain",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_import_log_guard_marks_scope_closed_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let path = crate::log_rotation::native_log_dir(root.path(), "import-test").unwrap();
        {
            let _guard = NativeImportLogGuard(path.clone());
            assert!(path.join(".active").is_file());
        }
        assert!(!path.join(".active").exists());
        assert!(path.join(".closed").is_file());
    }

    #[test]
    fn concurrent_import_guard_rejects_second_start_before_work() {
        let first = ImportExecutionGuard::acquire().unwrap();
        assert!(ImportExecutionGuard::acquire().is_err());
        drop(first);
        assert!(ImportExecutionGuard::acquire().is_ok());
    }
}
