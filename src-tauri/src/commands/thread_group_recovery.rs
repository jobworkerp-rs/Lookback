//! Durable retry identities for ThreadGroup summary generation.

use std::{
    fs,
    io::Write as _,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
use tauri::State;

use super::AppState;
use crate::error::{AppError, AppResult};

const FORMAT_VERSION: u32 = 1;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadGroupRecoveryRecord {
    pub memories_endpoint_id: String,
    pub owner_id: i64,
    #[serde(with = "group_id_serde")]
    pub group_id: i64,
    pub expected_membership_snapshot_digest: Option<String>,
    pub stage: String,
    pub reason_code: String,
    pub latest_generation_id: String,
    pub first_failed_at: String,
    pub last_attempt_at: String,
    pub last_trigger_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

mod group_id_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum WireId {
            String(String),
            Number(i64),
        }

        match WireId::deserialize(deserializer)? {
            WireId::String(value) => value.parse().map_err(serde::de::Error::custom),
            WireId::Number(value) => Ok(value),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ThreadGroupRecoveryIdentity<'a> {
    pub memories_endpoint_id: &'a str,
    pub group_id: i64,
    pub expected_membership_snapshot_digest: Option<&'a str>,
    pub generation_id: &'a str,
    pub trigger_kind: &'a str,
}

pub fn build_record(
    identity: ThreadGroupRecoveryIdentity<'_>,
    reason_code: &str,
    detail: Option<String>,
) -> ThreadGroupRecoveryRecord {
    build_record_at(identity, reason_code, detail)
}

fn build_record_at(
    identity: ThreadGroupRecoveryIdentity<'_>,
    reason_code: &str,
    detail: Option<String>,
) -> ThreadGroupRecoveryRecord {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    ThreadGroupRecoveryRecord {
        memories_endpoint_id: identity.memories_endpoint_id.to_string(),
        owner_id: 1,
        group_id: identity.group_id,
        expected_membership_snapshot_digest: identity
            .expected_membership_snapshot_digest
            .map(str::to_string),
        stage: "thread_group_summary".into(),
        reason_code: reason_code.into(),
        latest_generation_id: identity.generation_id.into(),
        first_failed_at: now.clone(),
        last_attempt_at: now,
        last_trigger_kind: identity.trigger_kind.into(),
        detail,
    }
}

impl ThreadGroupRecoveryRecord {
    fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("memories_endpoint_id", &self.memories_endpoint_id),
            ("stage", &self.stage),
            ("reason_code", &self.reason_code),
            ("latest_generation_id", &self.latest_generation_id),
            ("first_failed_at", &self.first_failed_at),
            ("last_attempt_at", &self.last_attempt_at),
            ("last_trigger_kind", &self.last_trigger_kind),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{name} is empty"));
            }
        }
        if self.owner_id <= 0 || self.group_id <= 0 {
            return Err("owner_id and group_id must be positive".into());
        }
        if self
            .expected_membership_snapshot_digest
            .as_deref()
            .is_some_and(str::is_empty)
        {
            return Err("expected_membership_snapshot_digest is empty".into());
        }
        Ok(())
    }

    fn identity(&self) -> (&str, i64, i64, Option<&str>) {
        (
            &self.memories_endpoint_id,
            self.owner_id,
            self.group_id,
            self.expected_membership_snapshot_digest.as_deref(),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ThreadGroupRecoveryState {
    format_version: u32,
    #[serde(default)]
    records: Vec<ThreadGroupRecoveryRecord>,
}

impl Default for ThreadGroupRecoveryState {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            records: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadGroupRecoveryHealth {
    Healthy,
    Corrupt,
    UnsupportedVersion,
    Unreadable,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ThreadGroupRecoveryStatus {
    pub health: ThreadGroupRecoveryHealth,
    pub record_count: usize,
    pub detail: Option<String>,
    pub records: Vec<ThreadGroupRecoveryRecord>,
}

enum LoadResult {
    Healthy(ThreadGroupRecoveryState),
    Corrupt(String),
    UnsupportedVersion(String),
    Unreadable(String),
}

fn load(path: &Path) -> LoadResult {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LoadResult::Healthy(ThreadGroupRecoveryState::default());
        }
        Err(error) => return LoadResult::Unreadable(error.to_string()),
    };
    let state = match serde_json::from_slice::<ThreadGroupRecoveryState>(&raw) {
        Ok(state) => state,
        Err(error) => return LoadResult::Corrupt(error.to_string()),
    };
    if state.format_version != FORMAT_VERSION {
        return LoadResult::UnsupportedVersion(format!(
            "thread group recovery state format_version={} is unsupported",
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

pub(crate) fn status_for(path: &Path) -> ThreadGroupRecoveryStatus {
    match load(path) {
        LoadResult::Healthy(state) => ThreadGroupRecoveryStatus {
            health: ThreadGroupRecoveryHealth::Healthy,
            record_count: state.records.len(),
            detail: None,
            records: state.records,
        },
        LoadResult::Corrupt(detail) => ThreadGroupRecoveryStatus {
            health: ThreadGroupRecoveryHealth::Corrupt,
            record_count: 0,
            detail: Some(detail),
            records: Vec::new(),
        },
        LoadResult::UnsupportedVersion(detail) => ThreadGroupRecoveryStatus {
            health: ThreadGroupRecoveryHealth::UnsupportedVersion,
            record_count: 0,
            detail: Some(detail),
            records: Vec::new(),
        },
        LoadResult::Unreadable(detail) => ThreadGroupRecoveryStatus {
            health: ThreadGroupRecoveryHealth::Unreadable,
            record_count: 0,
            detail: Some(detail),
            records: Vec::new(),
        },
    }
}

pub(crate) fn ensure_healthy_for_dispatch(path: &Path) -> AppResult<()> {
    let status = status_for(path);
    if status.health == ThreadGroupRecoveryHealth::Healthy {
        Ok(())
    } else {
        Err(AppError::Config(format!(
            "thread group recovery state is {:?}: {}",
            status.health,
            status.detail.unwrap_or_default()
        )))
    }
}

fn save(path: &Path, state: &ThreadGroupRecoveryState) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("thread group recovery path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".thread-group-recovery-{}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| -> AppResult<()> {
        let raw = serde_json::to_vec_pretty(state).map_err(|error| {
            AppError::Config(format!("serialize thread group recovery state: {error}"))
        })?;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&raw)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

pub fn record(path: &Path, record: ThreadGroupRecoveryRecord) -> AppResult<()> {
    record.validate().map_err(|error| {
        AppError::Config(format!("invalid thread group recovery record: {error}"))
    })?;
    let LoadResult::Healthy(mut state) = load(path) else {
        return ensure_healthy_for_dispatch(path);
    };
    if let Some(existing) = state
        .records
        .iter_mut()
        .find(|entry| entry.identity() == record.identity())
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

pub fn complete_group(path: &Path, endpoint: &str, group_id: i64) -> AppResult<()> {
    let LoadResult::Healthy(mut state) = load(path) else {
        return ensure_healthy_for_dispatch(path);
    };
    state.records.retain(|entry| {
        entry.memories_endpoint_id != endpoint || entry.owner_id != 1 || entry.group_id != group_id
    });
    save(path, &state)
}

#[tauri::command]
pub async fn get_thread_group_recovery_status(
    state: State<'_, AppState>,
) -> AppResult<ThreadGroupRecoveryStatus> {
    let _guard = state.thread_group_recovery.lock().await;
    Ok(status_for(&state.data.thread_group_recovery_path()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_recovery_is_durable_and_deduplicated_by_expected_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thread-group-recovery.json");
        let first = build_record("local", 7, "digest-a", "generation-a", "llm_failed");

        record(&path, first.clone()).unwrap();
        let mut updated = first.clone();
        updated.reason_code = "snapshot_changed".into();
        record(&path, updated).unwrap();

        let status = status_for(&path);
        assert_eq!(status.health, ThreadGroupRecoveryHealth::Healthy);
        assert_eq!(status.records.len(), 1);
        assert_eq!(status.records[0].first_failed_at, first.first_failed_at);
        assert_eq!(status.records[0].reason_code, "snapshot_changed");
    }

    #[test]
    fn completing_group_recovery_removes_only_the_group_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thread-group-recovery.json");
        let group = build_record("local", 7, "digest-a", "generation-a", "llm_failed");
        let other = build_record("local", 8, "digest-b", "generation-b", "llm_failed");

        record(&path, group.clone()).unwrap();
        record(&path, other).unwrap();
        complete_group(&path, "local", 7).unwrap();

        let status = status_for(&path);
        assert_eq!(status.records.len(), 1);
        assert_eq!(status.records[0].group_id, 8);
    }

    #[test]
    fn group_id_is_string_on_the_wire_and_accepts_legacy_numbers() {
        let record = build_record("local", 7, "digest-a", "generation-a", "llm_failed");
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["group_id"], "7");

        let decoded: ThreadGroupRecoveryRecord = serde_json::from_value(serde_json::json!({
            "memories_endpoint_id": "local",
            "owner_id": 1,
            "group_id": 7,
            "expected_membership_snapshot_digest": "digest-a",
            "stage": "thread_group_summary",
            "reason_code": "llm_failed",
            "latest_generation_id": "generation-a",
            "first_failed_at": "2026-01-01T00:00:00Z",
            "last_attempt_at": "2026-01-01T00:00:00Z",
            "last_trigger_kind": "manual"
        }))
        .unwrap();
        assert_eq!(decoded.group_id, 7);
    }

    #[test]
    fn malformed_recovery_is_not_replaced_implicitly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thread-group-recovery.json");
        std::fs::write(&path, b"{").unwrap();

        assert_eq!(status_for(&path).health, ThreadGroupRecoveryHealth::Corrupt);
        assert!(
            record(
                &path,
                build_record("local", 7, "digest-a", "generation-a", "llm_failed")
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"{");
    }

    fn build_record(
        endpoint: &str,
        group_id: i64,
        digest: &str,
        generation: &str,
        reason: &str,
    ) -> ThreadGroupRecoveryRecord {
        build_record_at(
            ThreadGroupRecoveryIdentity {
                memories_endpoint_id: endpoint,
                group_id,
                expected_membership_snapshot_digest: Some(digest),
                generation_id: generation,
                trigger_kind: "manual",
            },
            reason,
            None,
        )
    }
}
