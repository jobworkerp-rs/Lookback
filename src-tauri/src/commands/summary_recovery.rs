//! Durable recovery state for incomplete path-scoped period summaries.
//!
//! This file intentionally stores diagnostics and retry identities only. It is
//! never a backup of Memories data, and replacing it must not mutate threads or
//! summary Memories.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use tauri::State;

use super::AppState;
use crate::error::{AppError, AppResult};

const FORMAT_VERSION: u32 = 1;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Stable reason/stage vocabulary is deliberately string based: workflows can
/// add a reason without forcing this app to reject a recovery record written by
/// a newer worker bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SummaryRecoveryRecord {
    pub memories_endpoint_id: String,
    pub owner_id: i64,
    pub source_scope_digest: String,
    pub period_kind: String,
    pub period_key: String,
    pub rollup_scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_bucket_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_bucket_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_label: Option<String>,
    pub stage: String,
    pub reason_code: String,
    pub latest_generation_id: String,
    pub first_failed_at: String,
    pub last_attempt_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_trigger_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Immutable identity shared by every recovery outcome produced for one
/// workflow generation. Keeping this separate from a record makes callers
/// specify only the outcome-specific fields, while this module owns the wire
/// format's identity, timestamps, and scope invariants.
#[derive(Debug, Clone)]
pub struct SummaryRecoveryIdentity<'a> {
    pub memories_endpoint_id: &'a str,
    pub source_scope_digest: &'a str,
    pub generation_id: &'a str,
    pub trigger_kind: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub enum SummaryRecoveryScope<'a> {
    Overall,
    Path {
        bucket_digest: &'a str,
        bucket_kind: &'a str,
        path_label: Option<&'a str>,
    },
}

pub fn build_record(
    identity: SummaryRecoveryIdentity<'_>,
    period_kind: &str,
    period_key: &str,
    scope: SummaryRecoveryScope<'_>,
    stage: &str,
    reason_code: &str,
    detail: Option<String>,
) -> SummaryRecoveryRecord {
    let (rollup_scope, path_bucket_digest, path_bucket_kind, path_label) = match scope {
        SummaryRecoveryScope::Overall => ("overall", None, None, None),
        SummaryRecoveryScope::Path {
            bucket_digest,
            bucket_kind,
            path_label,
        } => (
            "path",
            Some(bucket_digest.to_string()),
            Some(bucket_kind.to_string()),
            path_label.map(str::to_string),
        ),
    };
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    SummaryRecoveryRecord {
        memories_endpoint_id: identity.memories_endpoint_id.to_string(),
        owner_id: 1,
        source_scope_digest: identity.source_scope_digest.to_string(),
        period_kind: period_kind.to_string(),
        period_key: period_key.to_string(),
        rollup_scope: rollup_scope.to_string(),
        path_bucket_digest,
        path_bucket_kind,
        path_label,
        stage: stage.to_string(),
        reason_code: reason_code.to_string(),
        latest_generation_id: identity.generation_id.to_string(),
        first_failed_at: now.clone(),
        last_attempt_at: now,
        last_trigger_kind: Some(identity.trigger_kind.to_string()),
        detail,
    }
}

impl SummaryRecoveryRecord {
    fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("memories_endpoint_id", &self.memories_endpoint_id),
            ("source_scope_digest", &self.source_scope_digest),
            ("period_kind", &self.period_kind),
            ("period_key", &self.period_key),
            ("rollup_scope", &self.rollup_scope),
            ("stage", &self.stage),
            ("reason_code", &self.reason_code),
            ("latest_generation_id", &self.latest_generation_id),
            ("first_failed_at", &self.first_failed_at),
            ("last_attempt_at", &self.last_attempt_at),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{name} is empty"));
            }
        }
        if !matches!(self.period_kind.as_str(), "daily" | "weekly" | "monthly") {
            return Err("period_kind is invalid".into());
        }
        if !matches!(self.rollup_scope.as_str(), "overall" | "path") {
            return Err("rollup_scope is invalid".into());
        }
        if self.rollup_scope == "overall" && self.path_bucket_digest.is_some() {
            return Err("overall record has path_bucket_digest".into());
        }
        if self.rollup_scope == "path" && self.path_bucket_digest.is_none() {
            return Err("path record has no path_bucket_digest".into());
        }
        Ok(())
    }

    fn identity(&self) -> (&str, i64, &str, &str, &str, &str, Option<&str>) {
        (
            &self.memories_endpoint_id,
            self.owner_id,
            &self.source_scope_digest,
            &self.period_kind,
            &self.period_key,
            &self.rollup_scope,
            self.path_bucket_digest.as_deref(),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SummaryRecoveryState {
    format_version: u32,
    #[serde(default)]
    records: Vec<SummaryRecoveryRecord>,
}

impl Default for SummaryRecoveryState {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            records: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SummaryRecoveryHealth {
    Healthy,
    Corrupt,
    UnsupportedVersion,
    Unreadable,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SummaryRecoveryStatus {
    pub health: SummaryRecoveryHealth,
    pub record_count: usize,
    pub detail: Option<String>,
    /// Entries are exposed only after the app has parsed and validated the
    /// state file; callers never receive malformed raw recovery JSON.
    pub records: Vec<SummaryRecoveryRecord>,
}

enum LoadResult {
    Healthy(SummaryRecoveryState),
    Corrupt(String),
    UnsupportedVersion(String),
    Unreadable(String),
}

fn load(path: &Path) -> LoadResult {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LoadResult::Healthy(SummaryRecoveryState::default());
        }
        Err(error) => return LoadResult::Unreadable(error.to_string()),
    };
    let state: SummaryRecoveryState = match serde_json::from_slice(&raw) {
        Ok(state) => state,
        Err(error) => return LoadResult::Corrupt(error.to_string()),
    };
    if state.format_version > FORMAT_VERSION {
        return LoadResult::UnsupportedVersion(format!(
            "summary recovery state format_version={} is newer than supported version {FORMAT_VERSION}",
            state.format_version
        ));
    }
    if state.format_version != FORMAT_VERSION {
        return LoadResult::UnsupportedVersion(format!(
            "summary recovery state format_version={} is unsupported",
            state.format_version
        ));
    }
    for record in &state.records {
        if let Err(error) = record.validate() {
            return LoadResult::Corrupt(format!("invalid recovery record: {error}"));
        }
    }
    LoadResult::Healthy(state)
}

fn status_for(path: &Path) -> SummaryRecoveryStatus {
    match load(path) {
        LoadResult::Healthy(state) => SummaryRecoveryStatus {
            health: SummaryRecoveryHealth::Healthy,
            record_count: state.records.len(),
            detail: None,
            records: state.records,
        },
        LoadResult::Corrupt(detail) => SummaryRecoveryStatus {
            health: SummaryRecoveryHealth::Corrupt,
            record_count: 0,
            detail: Some(detail),
            records: Vec::new(),
        },
        LoadResult::UnsupportedVersion(detail) => SummaryRecoveryStatus {
            health: SummaryRecoveryHealth::UnsupportedVersion,
            record_count: 0,
            detail: Some(detail),
            records: Vec::new(),
        },
        LoadResult::Unreadable(detail) => SummaryRecoveryStatus {
            health: SummaryRecoveryHealth::Unreadable,
            record_count: 0,
            detail: Some(detail),
            records: Vec::new(),
        },
    }
}

fn save(path: &Path, state: &SummaryRecoveryState) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("summary recovery path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".summary-recovery-{}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| -> AppResult<()> {
        let raw = serde_json::to_vec_pretty(state).map_err(|error| {
            AppError::Config(format!("serialize summary recovery state: {error}"))
        })?;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&raw)?;
        file.sync_all()?;
        replace_atomically(&temp, path)?;
        sync_parent_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Promote a fully synced sibling temp file without exposing a missing or
/// partially-written destination. Unix `rename` replaces atomically. Windows
/// explicitly rejects that operation when the destination exists, so use the
/// native ReplaceFile API after the first creation.
#[cfg(not(windows))]
fn replace_atomically(temp: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(temp, path)
}

#[cfg(windows)]
fn replace_atomically(temp: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr;

    if !path.exists() {
        return fs::rename(temp, path);
    }
    let destination = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let replacement = temp
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // REPLACEFILE_WRITE_THROUGH requests that the replacement reaches stable
    // storage before success. No backup is needed: the temp file is retained
    // on an API error and the old destination remains intact.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ReplaceFileW(
            replaced_file_name: *const u16,
            replacement_file_name: *const u16,
            backup_file_name: *const u16,
            replace_flags: u32,
            exclude: *mut core::ffi::c_void,
            reserved: *mut core::ffi::c_void,
        ) -> i32;
    }
    // SAFETY: both paths are NUL-terminated UTF-16 buffers retained for the
    // call; the nullable optional parameters are explicitly null.
    let replaced = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            replacement.as_ptr(),
            ptr::null(),
            0x0000_0001, // REPLACEFILE_WRITE_THROUGH
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn sync_parent_directory(parent: &Path) -> std::io::Result<()> {
    fs::File::open(parent)?.sync_all()
}

// Windows does not expose a directory handle through `std::fs::File` that
// supports fsync. ReplaceFileW with WRITE_THROUGH provides the commit
// durability guarantee for this file, so do not turn a successful replacement
// into a spurious directory-open failure.
#[cfg(windows)]
fn sync_parent_directory(_: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Refuse a new summary dispatch when durable recovery state cannot be read
/// safely. The caller should display the status command's diagnostic and ask
/// the user to explicitly reset/restore the state; never overwrite it here.
pub fn ensure_healthy(path: &Path) -> AppResult<()> {
    match status_for(path) {
        SummaryRecoveryStatus {
            health: SummaryRecoveryHealth::Healthy,
            ..
        } => Ok(()),
        status => Err(AppError::Config(format!(
            "summary recovery state is {:?}: {}",
            status.health,
            status.detail.unwrap_or_default()
        ))),
    }
}

/// Record an incomplete dispatch atomically. Identical records are merged so
/// import/manual/periodic callers do not create duplicate retry work.
pub fn record(path: &Path, record: SummaryRecoveryRecord) -> AppResult<()> {
    record
        .validate()
        .map_err(|error| AppError::Config(format!("invalid summary recovery record: {error}")))?;
    let LoadResult::Healthy(mut state) = load(path) else {
        return ensure_healthy(path);
    };
    if let Some(existing) = state
        .records
        .iter_mut()
        .find(|existing| existing.identity() == record.identity())
    {
        let first_failed_at = existing.first_failed_at.clone();
        *existing = record;
        existing.first_failed_at = first_failed_at;
    } else {
        state.records.push(record);
    }
    state
        .records
        .sort_by(|left, right| left.identity().cmp(&right.identity()));
    save(path, &state)
}

/// Remove a recovery record only after a caller has completely reconciled the
/// corresponding identity. This function deliberately does not infer success
/// from time passing or a missing source thread.
pub fn complete(path: &Path, record: &SummaryRecoveryRecord) -> AppResult<()> {
    let LoadResult::Healthy(mut state) = load(path) else {
        return ensure_healthy(path);
    };
    state
        .records
        .retain(|existing| existing.identity() != record.identity());
    save(path, &state)
}

/// Remove every retry identity for one period only after the caller has
/// reconciled the whole period DAG. This includes path rows because an
/// overall success with no reported failed path bucket proves their planned
/// inputs were reconciled together.
pub fn complete_period(path: &Path, record: &SummaryRecoveryRecord) -> AppResult<()> {
    let LoadResult::Healthy(mut state) = load(path) else {
        return ensure_healthy(path);
    };
    state.records.retain(|existing| {
        !(existing.memories_endpoint_id == record.memories_endpoint_id
            && existing.owner_id == record.owner_id
            && existing.source_scope_digest == record.source_scope_digest
            && existing.period_kind == record.period_kind
            && existing.period_key == record.period_key)
    });
    save(path, &state)
}

#[tauri::command]
pub async fn get_summary_recovery_status(
    state: State<'_, AppState>,
) -> AppResult<SummaryRecoveryStatus> {
    let _guard = state.summary_recovery.lock().await;
    Ok(status_for(&state.data.summary_recovery_path()))
}

/// Explicit user recovery for a corrupt state. The original is moved aside
/// before an empty state is installed; unreadable and newer-version files are
/// intentionally not touched because doing so could destroy evidence or data
/// a newer app understands.
#[tauri::command]
pub async fn reset_summary_recovery_state(
    state: State<'_, AppState>,
) -> AppResult<SummaryRecoveryStatus> {
    let _guard = state.summary_recovery.lock().await;
    let path = state.data.summary_recovery_path();
    match load(&path) {
        LoadResult::Healthy(_) => return Ok(status_for(&path)),
        LoadResult::UnsupportedVersion(detail) | LoadResult::Unreadable(detail) => {
            return Err(AppError::Config(format!(
                "summary recovery state cannot be reset automatically: {detail}"
            )));
        }
        LoadResult::Corrupt(_) => {}
    }
    let backup = PathBuf::from(format!(
        "{}.{}.backup",
        path.display(),
        chrono::Utc::now().format("%Y%m%d%H%M%S%3f")
    ));
    fs::rename(&path, backup)?;
    save(&path, &SummaryRecoveryState::default())?;
    Ok(status_for(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> SummaryRecoveryRecord {
        SummaryRecoveryRecord {
            memories_endpoint_id: "local".into(),
            owner_id: 1,
            source_scope_digest: "scope".into(),
            period_kind: "daily".into(),
            period_key: "2026-09-03".into(),
            rollup_scope: "path".into(),
            path_bucket_digest: Some("bucket".into()),
            path_bucket_kind: Some("label".into()),
            path_label: Some("path:/workspace/example".into()),
            stage: "path_summary".into(),
            reason_code: "llm_execution_failed".into(),
            latest_generation_id: "20260903070632000-x".into(),
            first_failed_at: "2026-09-03T07:06:32Z".into(),
            last_attempt_at: "2026-09-03T07:06:32Z".into(),
            last_trigger_kind: Some("manual".into()),
            detail: None,
        }
    }

    #[test]
    fn builder_enforces_overall_and_path_scope_shapes() {
        let identity = SummaryRecoveryIdentity {
            memories_endpoint_id: "local",
            source_scope_digest: "scope",
            generation_id: "generation",
            trigger_kind: "manual",
        };
        let overall = build_record(
            identity.clone(),
            "daily",
            "2026-09-03",
            SummaryRecoveryScope::Overall,
            "period_batch",
            "failed",
            None,
        );
        let path = build_record(
            identity,
            "daily",
            "2026-09-03",
            SummaryRecoveryScope::Path {
                bucket_digest: "bucket",
                bucket_kind: "label",
                path_label: Some("path:/workspace/project"),
            },
            "path_summary",
            "failed",
            Some("detail".into()),
        );
        assert!(overall.validate().is_ok());
        assert!(path.validate().is_ok());
        assert_eq!(overall.rollup_scope, "overall");
        assert!(overall.path_bucket_digest.is_none());
        assert_eq!(path.path_bucket_digest.as_deref(), Some("bucket"));
    }

    #[test]
    fn record_and_complete_are_atomic_state_transitions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary-recovery-state.json");
        let entry = sample_record();
        super::record(&path, entry.clone()).unwrap();
        assert_eq!(status_for(&path).record_count, 1);
        assert_eq!(status_for(&path).records.len(), 1);
        let mut retry = entry.clone();
        retry.reason_code = "fallback_used".into();
        retry.last_attempt_at = "2026-09-03T08:00:00Z".into();
        super::record(&path, retry).unwrap();
        let LoadResult::Healthy(state) = load(&path) else {
            panic!("expected healthy state");
        };
        assert_eq!(state.records.len(), 1);
        assert_eq!(state.records[0].first_failed_at, entry.first_failed_at);
        assert_eq!(state.records[0].reason_code, "fallback_used");
        complete(&path, &entry).unwrap();
        assert_eq!(status_for(&path).record_count, 0);
    }

    #[test]
    fn complete_period_removes_path_and_overall_records_for_that_period_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary-recovery-state.json");
        let path_record = sample_record();
        let mut overall = path_record.clone();
        overall.rollup_scope = "overall".into();
        overall.path_bucket_digest = None;
        overall.path_bucket_kind = None;
        overall.path_label = None;
        record(&path, path_record).unwrap();
        record(&path, overall.clone()).unwrap();
        let mut another_period = overall.clone();
        another_period.period_key = "2026-09-04".into();
        record(&path, another_period).unwrap();

        complete_period(&path, &overall).unwrap();
        let LoadResult::Healthy(state) = load(&path) else {
            panic!("expected healthy state");
        };
        assert_eq!(state.records.len(), 1);
        assert_eq!(state.records[0].period_key, "2026-09-04");
    }

    #[test]
    fn malformed_bytes_are_never_silently_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary-recovery-state.json");
        fs::write(&path, b"{").unwrap();
        assert_eq!(status_for(&path).health, SummaryRecoveryHealth::Corrupt);
        assert!(ensure_healthy(&path).is_err());
        assert!(super::record(&path, sample_record()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{");
    }
}
