use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::data::fsutil::copy_atomic;
use crate::error::{AppError, AppResult};
pub const DEFAULT_MIN_CONFIDENCE: f64 = 0.90;
pub const DEFAULT_RESOLUTION_MIN_CONFIDENCE: f64 = DEFAULT_MIN_CONFIDENCE;
pub const DEFAULT_EXTRACT_CAP: u32 = 5;
pub const EXTRACTION_POLICY_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AutoRegisterPriority {
    P0,
    P1,
    P2,
    P3,
    P4,
}

impl AutoRegisterPriority {
    fn ordinal(self) -> u8 {
        match self {
            Self::P0 => 0,
            Self::P1 => 1,
            Self::P2 => 2,
            Self::P3 => 3,
            Self::P4 => 4,
        }
    }

    fn wire_name(self) -> &'static str {
        match self {
            Self::P0 => "CANDIDATE_PRIORITY_P0",
            Self::P1 => "CANDIDATE_PRIORITY_P1",
            Self::P2 => "CANDIDATE_PRIORITY_P2",
            Self::P3 => "CANDIDATE_PRIORITY_P3",
            Self::P4 => "CANDIDATE_PRIORITY_P4",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AutoIssueResolutionMode {
    #[default]
    Disabled,
    Apply,
}

fn default_resolution_min_confidence() -> f64 {
    DEFAULT_RESOLUTION_MIN_CONFIDENCE
}

fn candidate_kind_wire_name(kind: &str) -> Option<&'static str> {
    match kind {
        "TASK" => Some("CANDIDATE_KIND_TASK"),
        "BUG" => Some("CANDIDATE_KIND_BUG"),
        "RESEARCH" => Some("CANDIDATE_KIND_RESEARCH"),
        "DECISION" => Some("CANDIDATE_KIND_DECISION"),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoRegisterPriorityRange {
    pub lower: AutoRegisterPriority,
    pub upper: AutoRegisterPriority,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutoIssuePolicy {
    pub extraction_enabled: bool,
    pub auto_register_min_confidence: Option<f64>,
    pub auto_register_priority_range: AutoRegisterPriorityRange,
    pub explicit_only: bool,
    pub allowed_kinds: Vec<String>,
    pub extract_cap: u32,
    #[serde(default)]
    pub resolution_mode: AutoIssueResolutionMode,
    #[serde(default = "default_resolution_min_confidence")]
    pub resolution_min_confidence: f64,
    /// Resolution from later threads other than the source. Not shown in the
    /// UI: it is enabled by hand once a dry run of the backlog looks right.
    #[serde(default)]
    pub cross_thread_resolution_mode: AutoIssueResolutionMode,
}

impl Default for AutoIssuePolicy {
    fn default() -> Self {
        Self {
            extraction_enabled: true,
            auto_register_min_confidence: Some(DEFAULT_MIN_CONFIDENCE),
            auto_register_priority_range: AutoRegisterPriorityRange {
                lower: AutoRegisterPriority::P0,
                upper: AutoRegisterPriority::P2,
            },
            explicit_only: true,
            allowed_kinds: Vec::new(),
            extract_cap: DEFAULT_EXTRACT_CAP,
            resolution_mode: AutoIssueResolutionMode::default(),
            resolution_min_confidence: DEFAULT_RESOLUTION_MIN_CONFIDENCE,
            cross_thread_resolution_mode: AutoIssueResolutionMode::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub version: u64,
    pub changed_at_ms: i64,
    pub policy: AutoIssuePolicy,
}

/// Wire-safe policy shared by every Issue workflow through `policy_json`. It
/// is a superset of IssueTracker's `ExtractionPolicy`: the extraction workflow
/// forwards only the tracker's fields, and the resolution settings stay with
/// the workflows that evaluate them. This deliberately excludes registry
/// history and the work-eligibility preference, which has its own registry.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ExtractionPolicyWire {
    pub version: u32,
    pub extraction_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_register_min_confidence: Option<f64>,
    pub auto_register_priority_range: ExtractionPriorityRangeWire,
    pub explicit_only: bool,
    pub allowed_kinds: Vec<String>,
    pub extract_cap: u32,
    pub resolution_mode: AutoIssueResolutionMode,
    pub resolution_min_confidence: f64,
    pub cross_thread_resolution_mode: AutoIssueResolutionMode,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ExtractionPriorityRangeWire {
    pub lower: &'static str,
    pub upper: &'static str,
}

impl From<&AutoIssuePolicy> for ExtractionPolicyWire {
    fn from(policy: &AutoIssuePolicy) -> Self {
        Self {
            version: EXTRACTION_POLICY_VERSION,
            extraction_enabled: policy.extraction_enabled,
            auto_register_min_confidence: policy.auto_register_min_confidence,
            auto_register_priority_range: ExtractionPriorityRangeWire {
                lower: policy.auto_register_priority_range.lower.wire_name(),
                upper: policy.auto_register_priority_range.upper.wire_name(),
            },
            explicit_only: policy.explicit_only,
            allowed_kinds: policy
                .allowed_kinds
                .iter()
                .map(|kind| {
                    candidate_kind_wire_name(kind)
                        .expect("AutoIssuePolicy is validated before wire conversion")
                        .to_owned()
                })
                .collect(),
            extract_cap: policy.extract_cap,
            resolution_mode: policy.resolution_mode,
            resolution_min_confidence: policy.resolution_min_confidence,
            cross_thread_resolution_mode: policy.cross_thread_resolution_mode,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct HistoryEntry {
    version: u64,
    changed_at_ms: i64,
    before: AutoIssuePolicy,
    after: AutoIssuePolicy,
}

pub struct PolicyRegistry {
    dir: PathBuf,
}

impl PolicyRegistry {
    pub fn open(data_root: impl AsRef<Path>) -> Self {
        Self {
            dir: data_root.as_ref().join("issue-auto-policy"),
        }
    }

    pub fn current_path(&self) -> PathBuf {
        self.dir.join("current.json")
    }

    pub fn history_path(&self) -> PathBuf {
        self.dir.join("history.jsonl")
    }

    pub fn load(&self) -> PolicySnapshot {
        if let Some(snapshot) = self.read_current() {
            return snapshot;
        }
        if let Some(snapshot) = self.last_valid_history() {
            return self.recover(snapshot.policy.clone(), Some(snapshot));
        }
        let fallback = Self::default_snapshot();
        if !self.has_persisted_storage() {
            return fallback;
        }
        self.recover(fallback.policy.clone(), Some(fallback))
    }

    pub fn update(&self, policy: AutoIssuePolicy) -> AppResult<PolicySnapshot> {
        validate(&policy)?;
        let previous = self.load();
        let snapshot = PolicySnapshot {
            version: self
                .next_persisted_version()
                .max(previous.version.saturating_add(1)),
            changed_at_ms: Utc::now().timestamp_millis(),
            policy,
        };
        let entry = HistoryEntry {
            version: snapshot.version,
            changed_at_ms: snapshot.changed_at_ms,
            before: previous.policy,
            after: snapshot.policy.clone(),
        };
        self.persist_snapshot(&snapshot, entry, "serialize policy history")?;
        Ok(snapshot)
    }

    fn read_current(&self) -> Option<PolicySnapshot> {
        let bytes = fs::read(self.current_path()).ok()?;
        let snapshot = serde_json::from_slice::<PolicySnapshot>(&bytes).ok()?;
        validate(&snapshot.policy).ok()?;
        Some(snapshot)
    }

    fn last_valid_history(&self) -> Option<PolicySnapshot> {
        let file = fs::File::open(self.history_path()).ok()?;
        let mut last = None;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<HistoryEntry>(&line)
                && validate(&entry.after).is_ok()
            {
                last = Some(PolicySnapshot {
                    version: entry.version,
                    changed_at_ms: entry.changed_at_ms,
                    policy: entry.after,
                });
            }
        }
        last
    }

    fn has_persisted_storage(&self) -> bool {
        self.current_path().exists() || self.history_path().exists()
    }

    fn next_persisted_version(&self) -> u64 {
        let current = fs::read(self.current_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value.get("version").and_then(serde_json::Value::as_u64));
        let history = fs::File::open(self.history_path())
            .ok()
            .into_iter()
            .flat_map(|file| BufReader::new(file).lines().map_while(Result::ok))
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
            .filter_map(|value| value.get("version").and_then(serde_json::Value::as_u64))
            .max();
        current
            .into_iter()
            .chain(history)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    fn default_snapshot() -> PolicySnapshot {
        PolicySnapshot {
            version: 1,
            changed_at_ms: 0,
            policy: AutoIssuePolicy::default(),
        }
    }

    fn recover(&self, policy: AutoIssuePolicy, fallback: Option<PolicySnapshot>) -> PolicySnapshot {
        let snapshot = PolicySnapshot {
            version: self.next_persisted_version(),
            changed_at_ms: Utc::now().timestamp_millis(),
            policy,
        };
        match self.persist_recovery(&snapshot) {
            Ok(()) => snapshot,
            Err(_) if self.read_current().as_ref() == Some(&snapshot) => snapshot,
            Err(_) => fallback.unwrap_or_else(Self::default_snapshot),
        }
    }

    fn persist_recovery(&self, snapshot: &PolicySnapshot) -> AppResult<()> {
        let entry = HistoryEntry {
            version: snapshot.version,
            changed_at_ms: snapshot.changed_at_ms,
            before: AutoIssuePolicy::default(),
            after: snapshot.policy.clone(),
        };
        self.persist_snapshot(snapshot, entry, "serialize policy reset")
    }

    /// Persists the current policy before its audit transition so history can
    /// never become authoritative when the current snapshot write fails.
    fn persist_snapshot(
        &self,
        snapshot: &PolicySnapshot,
        entry: HistoryEntry,
        history_error_context: &str,
    ) -> AppResult<()> {
        fs::create_dir_all(&self.dir)?;
        self.write_current(snapshot)?;
        let mut history = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.history_path())?;
        writeln!(
            history,
            "{}",
            serde_json::to_string(&entry)
                .map_err(|error| AppError::Config(format!("{history_error_context}: {error}")))?
        )?;
        Ok(())
    }

    fn write_current(&self, snapshot: &PolicySnapshot) -> AppResult<()> {
        let json = serde_json::to_string_pretty(snapshot)
            .map_err(|error| AppError::Config(format!("serialize policy: {error}")))?;
        let tmp = self.dir.join("current.json.write");
        fs::write(&tmp, json)?;
        copy_atomic(&tmp, &self.current_path())?;
        let _ = fs::remove_file(&tmp);
        Ok(())
    }
}

/// Serializes the effective policy for workflow inputs.
///
/// Keeping this beside the registry ensures every caller forwards the same
/// recovered/default snapshot while retaining ownership of its fallback policy.
pub(crate) fn snapshot_json(
    data_root: impl AsRef<Path>,
    issue_tracking_enabled: bool,
) -> Option<String> {
    let snapshot = PolicyRegistry::open(data_root).load();
    serde_json::to_string(&ExtractionPolicyWire::from(&effective_policy(
        &snapshot.policy,
        issue_tracking_enabled,
    )))
    .ok()
}

fn effective_policy(
    policy: &AutoIssuePolicy,
    experimental_issues_enabled: bool,
) -> AutoIssuePolicy {
    let mut effective = policy.clone();
    if !experimental_issues_enabled {
        effective.extraction_enabled = false;
        effective.resolution_mode = AutoIssueResolutionMode::Disabled;
        effective.cross_thread_resolution_mode = AutoIssueResolutionMode::Disabled;
    }
    effective
}

#[cfg(test)]
mod snapshot_json_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn snapshot_json_serializes_the_auto_register_default_for_an_empty_root() {
        let dir = tempdir().unwrap();

        let json = snapshot_json(dir.path(), true).expect("policy snapshots are serializable");
        let policy: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(policy["version"], EXTRACTION_POLICY_VERSION);
        assert_eq!(policy["extraction_enabled"], true,);
        assert_eq!(
            policy["auto_register_min_confidence"],
            DEFAULT_MIN_CONFIDENCE
        );
        assert_eq!(policy["resolution_mode"], "DISABLED");
        assert_eq!(
            policy["resolution_min_confidence"],
            DEFAULT_RESOLUTION_MIN_CONFIDENCE
        );
    }

    #[test]
    fn disabled_experiment_forces_extraction_off_without_changing_saved_policy() {
        let saved = AutoIssuePolicy::default();

        let effective = effective_policy(&saved, false);

        assert!(saved.extraction_enabled);
        assert!(!effective.extraction_enabled);
    }

    #[test]
    fn snapshot_json_applies_the_runtime_gate_without_changing_saved_policy() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        let policy = AutoIssuePolicy {
            resolution_mode: AutoIssueResolutionMode::Apply,
            ..Default::default()
        };
        registry.update(policy).unwrap();

        let disabled: serde_json::Value =
            serde_json::from_str(&snapshot_json(dir.path(), false).unwrap()).unwrap();
        let enabled: serde_json::Value =
            serde_json::from_str(&snapshot_json(dir.path(), true).unwrap()).unwrap();

        assert_eq!(disabled["extraction_enabled"], false);
        assert_eq!(disabled["resolution_mode"], "DISABLED");
        assert_eq!(enabled["extraction_enabled"], true);
        assert_eq!(enabled["resolution_mode"], "APPLY");
        assert!(registry.load().policy.extraction_enabled);
        assert_eq!(
            registry.load().policy.resolution_mode,
            AutoIssueResolutionMode::Apply
        );
    }
}

fn validate(policy: &AutoIssuePolicy) -> AppResult<()> {
    if let Some(min_confidence) = policy.auto_register_min_confidence
        && (!min_confidence.is_finite() || !(0.0..=1.0).contains(&min_confidence))
    {
        return Err(AppError::Config(
            "auto_register_min_confidence must be between 0.00 and 1.00".into(),
        ));
    }
    if !policy.resolution_min_confidence.is_finite()
        || !(0.0..=1.0).contains(&policy.resolution_min_confidence)
    {
        return Err(AppError::Config(
            "resolution_min_confidence must be between 0.00 and 1.00".into(),
        ));
    }
    if policy.auto_register_priority_range.lower.ordinal()
        > policy.auto_register_priority_range.upper.ordinal()
    {
        return Err(AppError::Config(
            "auto_register_priority_range lower must not exceed upper".into(),
        ));
    }
    if let Some(unknown_kind) = policy
        .allowed_kinds
        .iter()
        .find(|kind| candidate_kind_wire_name(kind).is_none())
    {
        return Err(AppError::Config(format!(
            "allowed_kinds contains an unsupported CandidateKind: {unknown_kind}"
        )));
    }
    if !(1..=20).contains(&policy.extract_cap) {
        return Err(AppError::Config(
            "extract_cap must be between 1 and 20".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::paths::tests::{evaluate_workflow_jq, find_workflow_task};
    use tempfile::tempdir;

    #[test]
    fn missing_registry_loads_the_new_default_policy() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        let snapshot = registry.load();
        assert_eq!(snapshot.version, 1);
        assert!(snapshot.policy.extraction_enabled);
        assert_eq!(
            snapshot.policy.auto_register_min_confidence,
            Some(DEFAULT_MIN_CONFIDENCE)
        );
        assert_eq!(
            snapshot.policy.resolution_mode,
            AutoIssueResolutionMode::Disabled
        );
        assert_eq!(
            snapshot.policy.resolution_min_confidence,
            DEFAULT_MIN_CONFIDENCE
        );
        assert!(snapshot.policy.explicit_only);
        assert_eq!(
            snapshot.policy.auto_register_priority_range,
            AutoRegisterPriorityRange {
                lower: AutoRegisterPriority::P0,
                upper: AutoRegisterPriority::P2,
            }
        );
        assert_eq!(snapshot.policy.extract_cap, 5);
    }

    #[test]
    fn update_increments_version_and_keeps_history() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        let next = AutoIssuePolicy {
            auto_register_min_confidence: Some(0.95),
            ..AutoIssuePolicy::default()
        };
        let updated = registry.update(next.clone()).unwrap();
        assert_eq!(updated.version, 2);
        assert_eq!(updated.policy, next);
        let reloaded = registry.load();
        assert_eq!(reloaded.version, 2);
        let history = fs::read_to_string(registry.history_path()).unwrap();
        assert!(history.contains("auto_register_min_confidence"));

        let json = snapshot_json(dir.path(), true).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["version"],
            EXTRACTION_POLICY_VERSION
        );
    }

    #[test]
    fn update_and_recovery_write_their_respective_history_transitions() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        let updated_policy = AutoIssuePolicy {
            extraction_enabled: false,
            ..AutoIssuePolicy::default()
        };
        let updated = registry.update(updated_policy.clone()).unwrap();

        fs::write(registry.current_path(), "{not json").unwrap();
        let recovered = registry.load();

        let entries = fs::read_to_string(registry.history_path())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<HistoryEntry>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].version, updated.version);
        assert_eq!(entries[0].before, AutoIssuePolicy::default());
        assert_eq!(entries[0].after, updated_policy);
        assert_eq!(entries[1].version, recovered.version);
        assert_eq!(entries[1].before, AutoIssuePolicy::default());
        assert_eq!(entries[1].after, recovered.policy);
    }

    #[test]
    fn snapshot_json_converts_allowed_kinds_to_proto_enum_names() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        registry
            .update(AutoIssuePolicy {
                allowed_kinds: vec![
                    "TASK".into(),
                    "BUG".into(),
                    "RESEARCH".into(),
                    "DECISION".into(),
                ],
                ..AutoIssuePolicy::default()
            })
            .unwrap();

        let policy: serde_json::Value =
            serde_json::from_str(&snapshot_json(dir.path(), true).unwrap()).unwrap();

        assert_eq!(
            policy["allowed_kinds"],
            serde_json::json!([
                "CANDIDATE_KIND_TASK",
                "CANDIDATE_KIND_BUG",
                "CANDIDATE_KIND_RESEARCH",
                "CANDIDATE_KIND_DECISION"
            ])
        );
    }

    #[test]
    fn broken_current_falls_back_to_last_history() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        let next = AutoIssuePolicy {
            extraction_enabled: false,
            ..AutoIssuePolicy::default()
        };
        let written = registry.update(next).unwrap();
        fs::write(registry.current_path(), "{not json").unwrap();
        let recovered = registry.load();
        assert_eq!(recovered.version, written.version + 1);
        assert!(!recovered.policy.extraction_enabled);
    }

    #[test]
    fn invalid_current_falls_back_to_the_last_valid_history() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        let written = registry.update(AutoIssuePolicy::default()).unwrap();
        let invalid = serde_json::json!({
            "version": written.version + 1,
            "changed_at_ms": written.changed_at_ms,
            "policy": {
                "extraction_enabled": true,
                "auto_register_min_confidence": 1.1,
                "auto_register_priority_range": { "lower": "P0", "upper": "P2" },
                "explicit_only": true,
                "allowed_kinds": [],
                "extract_cap": 5
            }
        });
        fs::write(registry.current_path(), invalid.to_string()).unwrap();

        let recovered = registry.load();
        assert_eq!(recovered.version, written.version + 2);
        assert_eq!(recovered.policy, written.policy);
    }

    #[test]
    fn recovery_advances_past_an_invalid_current_version() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        let history = registry.update(AutoIssuePolicy::default()).unwrap();
        let legacy_current = r#"{
            "version": 9,
            "changed_at_ms": 123,
            "policy": {
                "mode": "DISABLED",
                "min_confidence": 0.12,
                "explicit_only": false,
                "allowed_kinds": [],
                "maximum_auto_priority": "P4",
                "extract_cap": 20
            }
        }"#;
        fs::write(registry.current_path(), legacy_current).unwrap();

        let recovered = registry.load();

        assert_eq!(recovered.version, 10);
        assert_eq!(recovered.policy, history.policy);
        assert_eq!(
            registry.update(AutoIssuePolicy::default()).unwrap().version,
            11
        );
    }

    #[test]
    fn update_advances_past_a_newer_history_entry() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        registry.update(AutoIssuePolicy::default()).unwrap();
        let stale_history = HistoryEntry {
            version: 9,
            changed_at_ms: 123,
            before: AutoIssuePolicy::default(),
            after: AutoIssuePolicy::default(),
        };
        let mut history = OpenOptions::new()
            .append(true)
            .open(registry.history_path())
            .unwrap();
        writeln!(
            history,
            "{}",
            serde_json::to_string(&stale_history).unwrap()
        )
        .unwrap();

        assert_eq!(
            registry.update(AutoIssuePolicy::default()).unwrap().version,
            10
        );
    }

    #[test]
    fn persisted_policy_with_any_invalid_domain_value_falls_back_to_history() {
        let invalid_policies = [
            serde_json::json!({
                "auto_register_min_confidence": 1.1,
                "auto_register_priority_range": { "lower": "P0", "upper": "P2" },
                "extract_cap": 5,
            }),
            serde_json::json!({
                "auto_register_min_confidence": 0.9,
                "auto_register_priority_range": { "lower": "P2", "upper": "P1" },
                "extract_cap": 5,
            }),
            serde_json::json!({
                "auto_register_min_confidence": 0.9,
                "auto_register_priority_range": { "lower": "P0", "upper": "P2" },
                "extract_cap": 0,
            }),
            serde_json::json!({
                "auto_register_min_confidence": 0.9,
                "auto_register_priority_range": { "lower": "P0", "upper": "P2" },
                "extract_cap": 21,
            }),
            serde_json::json!({
                "auto_register_min_confidence": 0.9,
                "auto_register_priority_range": { "lower": "P0", "upper": "P2" },
                "allowed_kinds": ["INCIDENT"],
                "extract_cap": 5,
            }),
        ];

        for invalid_policy in invalid_policies {
            let dir = tempdir().unwrap();
            let registry = PolicyRegistry::open(dir.path());
            let history = registry.update(AutoIssuePolicy::default()).unwrap();
            let mut invalid_current = serde_json::to_value(&history).unwrap();
            for (key, value) in invalid_policy.as_object().unwrap() {
                invalid_current["policy"][key] = value.clone();
            }
            fs::write(registry.current_path(), invalid_current.to_string()).unwrap();

            assert_eq!(registry.load().policy, history.policy);
        }
    }

    #[test]
    fn rejects_out_of_range_values() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        let too_confident = AutoIssuePolicy {
            auto_register_min_confidence: Some(1.5),
            ..AutoIssuePolicy::default()
        };
        assert!(registry.update(too_confident).is_err());
        let empty_cap = AutoIssuePolicy {
            extract_cap: 0,
            ..AutoIssuePolicy::default()
        };
        assert!(registry.update(empty_cap).is_err());

        let inverted_priority_range = AutoIssuePolicy {
            auto_register_priority_range: AutoRegisterPriorityRange {
                lower: AutoRegisterPriority::P2,
                upper: AutoRegisterPriority::P1,
            },
            ..AutoIssuePolicy::default()
        };
        assert!(registry.update(inverted_priority_range).is_err());

        let unknown_kind = AutoIssuePolicy {
            allowed_kinds: vec!["INCIDENT".into()],
            ..AutoIssuePolicy::default()
        };
        assert!(registry.update(unknown_kind).is_err());
    }

    #[test]
    fn rejects_non_finite_or_out_of_range_resolution_confidence() {
        for confidence in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.01, 1.01] {
            let dir = tempdir().unwrap();
            let registry = PolicyRegistry::open(dir.path());
            let policy = AutoIssuePolicy {
                resolution_min_confidence: confidence,
                ..AutoIssuePolicy::default()
            };

            assert!(registry.update(policy).is_err(), "confidence={confidence}");
        }
    }

    #[test]
    fn accepts_resolution_confidence_boundaries() {
        for confidence in [0.0, 1.0] {
            let dir = tempdir().unwrap();
            let registry = PolicyRegistry::open(dir.path());
            let policy = AutoIssuePolicy {
                resolution_min_confidence: confidence,
                ..AutoIssuePolicy::default()
            };

            assert!(registry.update(policy).is_ok(), "confidence={confidence}");
        }
    }

    #[test]
    fn old_policy_json_without_resolution_fields_uses_the_new_defaults() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        fs::create_dir_all(&registry.dir).unwrap();
        let old_snapshot = serde_json::json!({
            "version": 4,
            "changed_at_ms": 123,
            "policy": {
                "extraction_enabled": true,
                "auto_register_min_confidence": 0.85,
                "auto_register_priority_range": { "lower": "P0", "upper": "P2" },
                "explicit_only": true,
                "allowed_kinds": [],
                "extract_cap": 5
            }
        });
        fs::write(registry.current_path(), old_snapshot.to_string()).unwrap();

        let loaded = registry.load();

        assert_eq!(loaded.version, 4);
        assert_eq!(
            loaded.policy.resolution_mode,
            AutoIssueResolutionMode::Disabled
        );
        assert_eq!(
            loaded.policy.resolution_min_confidence,
            DEFAULT_MIN_CONFIDENCE
        );
        assert_eq!(
            loaded.policy.cross_thread_resolution_mode,
            AutoIssueResolutionMode::Disabled
        );
    }

    #[test]
    fn cross_thread_resolution_is_relayed_and_gated_with_the_other_resolutions() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&snapshot_json(dir.path(), true).unwrap())
                .unwrap()["cross_thread_resolution_mode"],
            "DISABLED"
        );
        registry
            .update(AutoIssuePolicy {
                resolution_mode: AutoIssueResolutionMode::Apply,
                cross_thread_resolution_mode: AutoIssueResolutionMode::Apply,
                ..AutoIssuePolicy::default()
            })
            .unwrap();

        let enabled: serde_json::Value =
            serde_json::from_str(&snapshot_json(dir.path(), true).unwrap()).unwrap();
        let disabled: serde_json::Value =
            serde_json::from_str(&snapshot_json(dir.path(), false).unwrap()).unwrap();

        assert_eq!(enabled["cross_thread_resolution_mode"], "APPLY");
        assert_eq!(disabled["cross_thread_resolution_mode"], "DISABLED");
    }

    #[test]
    fn snapshot_json_contains_resolution_settings_without_changing_extract_settings() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        registry
            .update(AutoIssuePolicy {
                extraction_enabled: false,
                auto_register_min_confidence: None,
                resolution_mode: AutoIssueResolutionMode::Apply,
                resolution_min_confidence: 0.75,
                ..AutoIssuePolicy::default()
            })
            .unwrap();

        let policy: serde_json::Value =
            serde_json::from_str(&snapshot_json(dir.path(), true).unwrap()).unwrap();

        assert_eq!(policy["extraction_enabled"], false);
        assert_eq!(
            policy["auto_register_min_confidence"],
            serde_json::Value::Null
        );
        assert_eq!(policy["resolution_mode"], "APPLY");
        assert_eq!(policy["resolution_min_confidence"], 0.75);
    }

    #[test]
    fn legacy_current_and_history_are_ignored_in_favour_of_the_new_default() {
        let dir = tempdir().unwrap();
        let registry = PolicyRegistry::open(dir.path());
        fs::create_dir_all(&registry.dir).unwrap();
        let legacy_current = r#"{
            "version": 9,
            "changed_at_ms": 123,
            "policy": {
                "mode": "DISABLED",
                "min_confidence": 0.12,
                "explicit_only": false,
                "allowed_kinds": [],
                "maximum_auto_priority": "P4",
                "extract_cap": 20
            }
        }"#;
        fs::write(registry.current_path(), legacy_current).unwrap();
        let legacy_policy =
            serde_json::from_str::<serde_json::Value>(legacy_current).unwrap()["policy"].clone();
        fs::write(
            registry.history_path(),
            format!(
                r#"{{"version":8,"changed_at_ms":122,"before":{},"after":{}}}"#,
                legacy_policy, legacy_policy,
            ),
        )
        .unwrap();

        let snapshot = registry.load();

        assert_eq!(snapshot.version, 10);
        assert_eq!(snapshot.policy, AutoIssuePolicy::default());
        let persisted: PolicySnapshot =
            serde_json::from_slice(&fs::read(registry.current_path()).unwrap()).unwrap();
        assert_eq!(persisted, snapshot);
        assert!(
            fs::read_to_string(registry.history_path())
                .unwrap()
                .contains("\"version\":10")
        );
    }

    /// The workflow forwards the policy to IssueTracker's `ExtractionPolicy`,
    /// whose JSON decoding rejects unknown fields, while the app's policy also
    /// carries resolution settings the tracker does not know.
    fn extraction_request_policy(policy: serde_json::Value) -> serde_json::Value {
        let workflow: serde_yaml::Value =
            serde_yaml::from_str(&crate::issue_extract::shape::tests::extract_yaml()).unwrap();
        let task = find_workflow_task(&workflow, "prepareExtractionRequestPolicy")
            .expect("prepareExtractionRequestPolicy task");
        let context =
            std::collections::BTreeMap::from([("extraction_policy".to_string(), policy.into())]);
        evaluate_workflow_jq(&task["set"]["extraction_request_policy"], &context).unwrap()
    }

    fn tracker_extraction_policy_fields() -> std::collections::BTreeSet<String> {
        let proto = include_str!(
            "../../../proto/issue_tracker/manager/v1/issue_tracker_manager_common.proto"
        );
        let body = proto
            .split("message ExtractionPolicy {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("ExtractionPolicy message");
        body.lines()
            .filter_map(|line| line.split("//").next())
            .filter(|line| line.contains('=') && !line.trim_start().starts_with("reserved"))
            .filter_map(|line| line.split('=').next()?.split_whitespace().last())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn extraction_request_policy_matches_the_tracker_contract() {
        // Set because an unset confidence is omitted from the wire.
        let policy = AutoIssuePolicy {
            auto_register_min_confidence: Some(0.9),
            ..AutoIssuePolicy::default()
        };
        let request = extraction_request_policy(
            serde_json::to_value(ExtractionPolicyWire::from(&policy)).unwrap(),
        );

        let fields = request
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(fields, tracker_extraction_policy_fields());
    }

    #[test]
    fn extraction_request_policy_is_empty_without_a_policy_object() {
        assert_eq!(
            extraction_request_policy(serde_json::Value::Null),
            serde_json::json!({})
        );
    }
}
