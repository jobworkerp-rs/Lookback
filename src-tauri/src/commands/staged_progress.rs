use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use super::StepStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProgressKind {
    Import,
    SummaryGeneration,
}

impl ProgressKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "import" => Some(Self::Import),
            "summary-generation" => Some(Self::SummaryGeneration),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StagedStep {
    ThreadImport,
    ThreadSummary,
    ThreadGroupSummary,
    ThreadPersonality,
    Reflection,
    Daily,
    Weekly,
    Monthly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportNoticeCode {
    NoImportableLogSources,
}

#[derive(Debug, Clone, Serialize)]
pub struct StagedStepState {
    pub status: StepStatus,
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice_code: Option<ImportNoticeCode>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StagedProgressSnapshot {
    pub kind: ProgressKind,
    pub job_id: String,
    pub sequence: u64,
    pub steps: BTreeMap<StagedStep, StagedStepState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProgressKey {
    kind: ProgressKind,
    job_id: String,
}

struct SnapshotEntry {
    snapshot: StagedProgressSnapshot,
    updated_order: u64,
}

#[derive(Default)]
struct RegistryState {
    snapshots: HashMap<ProgressKey, SnapshotEntry>,
    updated_order: u64,
}

/// In-memory latest snapshots, isolated by both operation kind and execution
/// ID. Summary execution lifecycle is represented by registry membership (and
/// ends at `finish`), independently of the currently active stage. Import
/// snapshots keep their legacy active-step visibility contract.
#[derive(Default)]
pub struct StagedProgressRegistry(parking_lot::Mutex<RegistryState>);

impl StagedProgressRegistry {
    pub fn start_execution(
        &self,
        kind: ProgressKind,
        job_id: String,
        steps: impl IntoIterator<Item = StagedStep>,
    ) -> StagedProgressSnapshot {
        let mut state = self.0.lock();
        state
            .snapshots
            .retain(|key, entry| key.kind != kind || execution_is_visible(key, entry));
        let snapshot = StagedProgressSnapshot {
            kind,
            job_id: job_id.clone(),
            sequence: 0,
            steps: steps
                .into_iter()
                .map(|step| {
                    (
                        step,
                        StagedStepState {
                            status: StepStatus::Waiting,
                            message: None,
                            notice_code: None,
                        },
                    )
                })
                .collect(),
        };
        let key = ProgressKey { kind, job_id };
        state.updated_order = state.updated_order.saturating_add(1);
        let updated_order = state.updated_order;
        state.snapshots.insert(
            key,
            SnapshotEntry {
                snapshot: snapshot.clone(),
                updated_order,
            },
        );
        snapshot
    }

    pub fn update_execution(
        &self,
        kind: ProgressKind,
        job_id: &str,
        step: StagedStep,
        status: StepStatus,
        message: Option<String>,
        notice_code: Option<ImportNoticeCode>,
    ) -> Option<StagedProgressSnapshot> {
        let mut state = self.0.lock();
        let key = ProgressKey {
            kind,
            job_id: job_id.to_string(),
        };
        let entry = state.snapshots.get(&key)?;
        let previous = entry.snapshot.steps.get(&step)?;
        if terminal_status(previous.status) && !terminal_status(status) {
            return None;
        }
        let unchanged = previous.status == status
            && previous.message == message
            && previous.notice_code == notice_code;
        state.updated_order = state.updated_order.saturating_add(1);
        let updated_order = state.updated_order;
        let entry = state.snapshots.get_mut(&key)?;
        // A repeated update still marks the execution as the latest one, but
        // republishing an identical snapshot would only cost an IPC round and
        // a full UI re-render.
        entry.updated_order = updated_order;
        if unchanged {
            return None;
        }
        entry.snapshot.sequence = entry.snapshot.sequence.saturating_add(1);
        entry.snapshot.steps.insert(
            step,
            StagedStepState {
                status,
                message,
                notice_code,
            },
        );
        Some(entry.snapshot.clone())
    }

    pub fn active_snapshot_for(
        &self,
        kind: Option<ProgressKind>,
    ) -> Option<StagedProgressSnapshot> {
        self.0
            .lock()
            .snapshots
            .iter()
            .filter(|(key, entry)| {
                kind.is_none_or(|kind| kind == key.kind) && execution_is_visible(key, entry)
            })
            .max_by_key(|(_, entry)| entry.updated_order)
            .map(|(_, entry)| entry.snapshot.clone())
    }

    pub fn finish(&self, kind: ProgressKind, job_id: &str) {
        self.0.lock().snapshots.remove(&ProgressKey {
            kind,
            job_id: job_id.to_string(),
        });
    }

    /// Compatibility wrapper for the import-only registry API.
    pub fn start(&self, job_id: String) {
        self.start_execution(
            ProgressKind::Import,
            job_id,
            [
                StagedStep::ThreadImport,
                StagedStep::ThreadSummary,
                StagedStep::ThreadGroupSummary,
                StagedStep::ThreadPersonality,
                StagedStep::Reflection,
            ],
        );
    }
}

fn terminal_status(status: StepStatus) -> bool {
    matches!(
        status,
        StepStatus::Done | StepStatus::Warning | StepStatus::Failed | StepStatus::Cancelled
    )
}

fn execution_is_visible(key: &ProgressKey, entry: &SnapshotEntry) -> bool {
    key.kind != ProgressKind::Import || has_active_step(&entry.snapshot)
}

fn has_active_step(snapshot: &StagedProgressSnapshot) -> bool {
    snapshot
        .steps
        .values()
        .any(|step| step.status == StepStatus::Active)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_snapshots_are_isolated_by_kind_and_execution_id() {
        let registry = StagedProgressRegistry::default();
        registry.start_execution(
            ProgressKind::Import,
            "import-1".into(),
            [StagedStep::ThreadImport, StagedStep::ThreadSummary],
        );
        registry.start_execution(
            ProgressKind::SummaryGeneration,
            "summary-1".into(),
            [StagedStep::ThreadSummary, StagedStep::Daily],
        );
        registry.update_execution(
            ProgressKind::Import,
            "import-1",
            StagedStep::ThreadImport,
            StepStatus::Active,
            Some("importing".into()),
            None,
        );
        registry.update_execution(
            ProgressKind::SummaryGeneration,
            "summary-1",
            StagedStep::Daily,
            StepStatus::Active,
            Some("daily".into()),
            None,
        );

        assert_eq!(
            registry
                .active_snapshot_for(Some(ProgressKind::Import))
                .unwrap()
                .job_id,
            "import-1"
        );
        let summary = registry
            .active_snapshot_for(Some(ProgressKind::SummaryGeneration))
            .unwrap();
        assert_eq!(summary.job_id, "summary-1");
        assert_eq!(
            summary.steps[&StagedStep::ThreadSummary].status,
            StepStatus::Waiting
        );
        assert_eq!(summary.steps[&StagedStep::Daily].status, StepStatus::Active);
        assert_eq!(
            registry.active_snapshot_for(None).unwrap().job_id,
            "summary-1"
        );
    }

    #[test]
    fn snapshot_wire_shape_uses_stable_stage_keys_and_sequence() {
        let registry = StagedProgressRegistry::default();
        registry.start_execution(
            ProgressKind::SummaryGeneration,
            "dispatch".into(),
            [StagedStep::ThreadSummary, StagedStep::Daily],
        );
        let snapshot = registry
            .update_execution(
                ProgressKind::SummaryGeneration,
                "dispatch",
                StagedStep::Daily,
                StepStatus::Active,
                Some("(1/3) daily".into()),
                None,
            )
            .unwrap();

        assert_eq!(snapshot.sequence, 1);
        assert_eq!(
            serde_json::to_value(snapshot).unwrap(),
            serde_json::json!({
                "kind": "summary-generation",
                "job_id": "dispatch",
                "sequence": 1,
                "steps": {
                    "thread-summary": {"status": "waiting", "message": null},
                    "daily": {"status": "active", "message": "(1/3) daily"}
                }
            })
        );
    }

    #[test]
    fn terminal_stage_updates_cannot_be_reverted_to_active() {
        let registry = StagedProgressRegistry::default();
        registry.start_execution(
            ProgressKind::Import,
            "job".into(),
            [StagedStep::ThreadImport],
        );
        let done = registry
            .update_execution(
                ProgressKind::Import,
                "job",
                StagedStep::ThreadImport,
                StepStatus::Done,
                None,
                None,
            )
            .unwrap();
        let stale = registry.update_execution(
            ProgressKind::Import,
            "job",
            StagedStep::ThreadImport,
            StepStatus::Active,
            Some("stale".into()),
            None,
        );
        assert!(stale.is_none());
        let stored = registry
            .update_execution(
                ProgressKind::Import,
                "job",
                StagedStep::ThreadImport,
                StepStatus::Failed,
                Some("late failure".into()),
                None,
            )
            .unwrap();
        assert_eq!(stored.sequence, done.sequence + 1);
    }

    #[test]
    fn unchanged_stage_updates_are_not_republished() {
        let registry = StagedProgressRegistry::default();
        registry.start_execution(
            ProgressKind::SummaryGeneration,
            "job".into(),
            [StagedStep::Daily],
        );
        let update = |message: &str| {
            registry.update_execution(
                ProgressKind::SummaryGeneration,
                "job",
                StagedStep::Daily,
                StepStatus::Active,
                Some(message.into()),
                None,
            )
        };
        let first = update("(1/3) daily").unwrap();
        assert!(update("(1/3) daily").is_none());
        let changed = update("(2/3) daily").unwrap();
        assert_eq!(changed.sequence, first.sequence + 1);
    }

    #[test]
    fn queued_and_between_step_executions_remain_hydratable_until_finished() {
        let registry = StagedProgressRegistry::default();
        registry.start_execution(
            ProgressKind::SummaryGeneration,
            "A".into(),
            [StagedStep::ThreadSummary, StagedStep::Daily],
        );
        registry.update_execution(
            ProgressKind::SummaryGeneration,
            "A",
            StagedStep::ThreadSummary,
            StepStatus::Active,
            Some("A running".into()),
            None,
        );

        registry.start_execution(
            ProgressKind::SummaryGeneration,
            "B".into(),
            [StagedStep::ThreadSummary, StagedStep::Daily],
        );
        registry.start_execution(
            ProgressKind::SummaryGeneration,
            "C".into(),
            [StagedStep::ThreadSummary, StagedStep::Daily],
        );

        let queued = registry.active_snapshot_for(Some(ProgressKind::SummaryGeneration));
        assert_eq!(
            queued.as_ref().map(|snapshot| snapshot.job_id.as_str()),
            Some("C")
        );
        assert_eq!(
            queued.unwrap().steps[&StagedStep::ThreadSummary].status,
            StepStatus::Waiting
        );

        let a_update = registry.update_execution(
            ProgressKind::SummaryGeneration,
            "A",
            StagedStep::ThreadSummary,
            StepStatus::Active,
            Some("A still running".into()),
            None,
        );
        assert_eq!(a_update.unwrap().job_id, "A");

        let b_started = registry.update_execution(
            ProgressKind::SummaryGeneration,
            "B",
            StagedStep::ThreadSummary,
            StepStatus::Active,
            Some("B running".into()),
            None,
        );
        assert_eq!(b_started.unwrap().job_id, "B");
        let b_between_steps = registry
            .update_execution(
                ProgressKind::SummaryGeneration,
                "B",
                StagedStep::ThreadSummary,
                StepStatus::Done,
                None,
                None,
            )
            .unwrap();
        assert_eq!(b_between_steps.job_id, "B");
        assert_eq!(
            b_between_steps.steps[&StagedStep::ThreadSummary].status,
            StepStatus::Done
        );
        assert_eq!(
            b_between_steps.steps[&StagedStep::Daily].status,
            StepStatus::Waiting
        );
        assert_eq!(
            registry
                .active_snapshot_for(Some(ProgressKind::SummaryGeneration))
                .unwrap()
                .job_id,
            "B"
        );

        let b_cancelled = registry
            .update_execution(
                ProgressKind::SummaryGeneration,
                "B",
                StagedStep::Daily,
                StepStatus::Cancelled,
                Some("cancelled".into()),
                None,
            )
            .unwrap();
        assert_eq!(b_cancelled.job_id, "B");
        assert_eq!(
            registry
                .active_snapshot_for(Some(ProgressKind::SummaryGeneration))
                .unwrap()
                .steps[&StagedStep::Daily]
                .status,
            StepStatus::Cancelled
        );

        registry.finish(ProgressKind::SummaryGeneration, "B");
        let c_unchanged = registry.update_execution(
            ProgressKind::SummaryGeneration,
            "C",
            StagedStep::Daily,
            StepStatus::Waiting,
            None,
            None,
        );
        assert!(c_unchanged.is_none());
        assert_eq!(
            registry
                .active_snapshot_for(Some(ProgressKind::SummaryGeneration))
                .unwrap()
                .job_id,
            "C"
        );
        registry.finish(ProgressKind::SummaryGeneration, "C");
        assert_eq!(
            registry
                .active_snapshot_for(Some(ProgressKind::SummaryGeneration))
                .unwrap()
                .job_id,
            "A"
        );
        registry.finish(ProgressKind::SummaryGeneration, "A");
        assert!(
            registry
                .active_snapshot_for(Some(ProgressKind::SummaryGeneration))
                .is_none()
        );
    }

    #[test]
    fn completed_import_execution_stays_hidden_without_explicit_finish() {
        let registry = StagedProgressRegistry::default();
        registry.start("import-1".into());
        assert!(
            registry
                .active_snapshot_for(Some(ProgressKind::Import))
                .is_none()
        );
        registry.update_execution(
            ProgressKind::Import,
            "import-1",
            StagedStep::ThreadImport,
            StepStatus::Done,
            None,
            None,
        );
        for step in [
            StagedStep::ThreadSummary,
            StagedStep::ThreadGroupSummary,
            StagedStep::ThreadPersonality,
            StagedStep::Reflection,
        ] {
            registry.update_execution(
                ProgressKind::Import,
                "import-1",
                step,
                StepStatus::Done,
                None,
                None,
            );
        }

        assert!(
            registry
                .active_snapshot_for(Some(ProgressKind::Import))
                .is_none()
        );
    }
}
