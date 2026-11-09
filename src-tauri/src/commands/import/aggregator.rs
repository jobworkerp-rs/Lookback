use std::sync::atomic::{AtomicUsize, Ordering};

use tauri::AppHandle;

use super::super::{AppError, AppResult, StepStatus};
use super::guards::{CANCELLED_SKIP_MESSAGE, ImportSource};
use super::request::{ImportStep, PlainImportConfig, StartImportRequest};
use super::step_runner::emit_step;

/// Outcome of a single `memories-import` child, fed into the aggregator.
#[derive(Debug, Clone)]
pub(super) enum ChildOutcome {
    /// `summary` is the per-source `<label> summary:` block scraped from
    /// stdout. `None` when the child produced no recognisable summary
    /// block (rare — e.g. a non-zero exit before the summary printed).
    Success {
        summary: Option<String>,
    },
    PartialSuccess {
        summary: Option<String>,
        warning: String,
    },
    Failure(String),
}

/// Tracks per-job completion across all spawned `memories-import` children
/// (one per selected source). Lets us emit the `thread-import` terminal step
/// exactly once — when the last child finishes — instead of letting the
/// fastest source clobber the slower one's eventual result.
pub(super) struct ImportAggregator {
    total: usize,
    completed: AtomicUsize,
    failures: parking_lot::Mutex<Vec<String>>,
    warnings: parking_lot::Mutex<Vec<String>>,
    summaries: parking_lot::Mutex<Vec<String>>,
}

/// What the aggregator decided once the final child reported in. `None` means
/// "not the last child yet — say nothing".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AggregatedState {
    AllSucceeded {
        summaries: Vec<String>,
    },
    PartialSucceeded {
        summaries: Vec<String>,
        warnings: Vec<String>,
    },
    AnyFailed(Vec<String>),
}

fn aggregate_state(
    failures: Vec<String>,
    warnings: Vec<String>,
    summaries: Vec<String>,
) -> AggregatedState {
    match (failures.is_empty(), warnings.is_empty()) {
        (false, _) => AggregatedState::AnyFailed(failures),
        (true, false) => AggregatedState::PartialSucceeded {
            summaries,
            warnings,
        },
        (true, true) => AggregatedState::AllSucceeded { summaries },
    }
}

impl ImportAggregator {
    pub(super) fn new(total: usize) -> Self {
        Self {
            total,
            completed: AtomicUsize::new(0),
            failures: parking_lot::Mutex::new(Vec::new()),
            warnings: parking_lot::Mutex::new(Vec::new()),
            summaries: parking_lot::Mutex::new(Vec::new()),
        }
    }

    pub(super) fn record(&self, outcome: ChildOutcome) -> Option<AggregatedState> {
        match outcome {
            ChildOutcome::Failure(msg) => self.failures.lock().push(msg),
            ChildOutcome::Success { summary: Some(s) } => self.summaries.lock().push(s),
            ChildOutcome::PartialSuccess { summary, warning } => {
                if let Some(summary) = summary {
                    self.summaries.lock().push(summary);
                }
                self.warnings.lock().push(warning);
            }
            ChildOutcome::Success { summary: None } => {}
        }
        let done = self.completed.fetch_add(1, Ordering::AcqRel) + 1;
        if done < self.total {
            return None;
        }
        let failures = std::mem::take(&mut *self.failures.lock());
        let warnings = std::mem::take(&mut *self.warnings.lock());
        let summaries = std::mem::take(&mut *self.summaries.lock());
        Some(aggregate_state(failures, warnings, summaries))
    }

    /// Forced terminal extraction — used after the source loop completes
    /// regardless of whether the last `record` returned `Some` (single
    /// source case always returns `Some`; multi-source needs this to
    /// catch the silent path).
    pub(super) fn take_final(&self) -> AggregatedState {
        let failures = std::mem::take(&mut *self.failures.lock());
        let warnings = std::mem::take(&mut *self.warnings.lock());
        let summaries = std::mem::take(&mut *self.summaries.lock());
        aggregate_state(failures, warnings, summaries)
    }
}

/// Stable label for the `current_step` slot — used by tests and for the
/// log line a future deferred-item review can read off the trace.
pub(super) fn step_label(step: ImportStep) -> &'static str {
    match step {
        ImportStep::ThreadImport => "thread-import",
        ImportStep::ThreadSummary => "thread-summary",
        ImportStep::ThreadGroupSummary => "thread-group-summary",
        ImportStep::ThreadPersonality => "thread-personality",
        ImportStep::Reflection => "reflection",
        ImportStep::Daily => "daily",
        ImportStep::Weekly => "weekly",
        ImportStep::Monthly => "monthly",
    }
}

/// Cross-checks the `plain` source against its config block: presence on both
/// sides must agree, and when present the directory must exist and the
/// (optional) source name must match the CLI's charset. Pure so the dialog's
/// guard rails are unit-testable without spawning the importer.
pub(super) fn validate_plain(req: &StartImportRequest) -> AppResult<()> {
    let wants_plain = req.sources.iter().any(|s| matches!(s, ImportSource::Plain));
    let cfg = match (wants_plain, &req.plain) {
        (false, None) => return Ok(()),
        (false, Some(_)) => {
            return Err(AppError::Config(
                "plain config provided but 'plain' is not in sources".into(),
            ));
        }
        (true, None) => {
            return Err(AppError::Config(
                "plain source selected but no plain config was provided".into(),
            ));
        }
        (true, Some(cfg)) => cfg,
    };

    if let Some(name) = &cfg.source_name
        && !PlainImportConfig::source_name_is_valid(name)
    {
        return Err(AppError::Config(format!(
            "invalid plain source-name '{name}': must match ^[a-z0-9_-]{{1,32}}$"
        )));
    }
    // The importer runs as a local child against a local path, so the root is
    // on this machine — stat it here for an early, clear error.
    if !cfg.root.is_dir() {
        return Err(AppError::Config(format!(
            "plain root is not a directory: {}",
            cfg.root.display()
        )));
    }
    Ok(())
}

/// Mark the downstream summary/personality/reflection steps as
/// `Cancelled("中断によりスキップ")` when the active thread-import was
/// cancelled mid-flight (before any downstream had a chance to run).
pub(super) fn mark_downstream_skipped_by_cancel(app: &AppHandle, job_id: &str) {
    for step in [
        ImportStep::ThreadSummary,
        ImportStep::ThreadGroupSummary,
        ImportStep::ThreadPersonality,
        ImportStep::Reflection,
    ] {
        emit_step(
            app,
            job_id,
            step,
            StepStatus::Cancelled,
            Some(CANCELLED_SKIP_MESSAGE.into()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::StepStatus;
    use super::super::forward::{import_state_allows_downstream, import_terminal_step_outcome};
    use super::*;

    fn ok() -> ChildOutcome {
        ChildOutcome::Success { summary: None }
    }

    fn ok_with(s: &str) -> ChildOutcome {
        ChildOutcome::Success {
            summary: Some(s.into()),
        }
    }

    fn partial_with(s: &str, warning: &str) -> ChildOutcome {
        ChildOutcome::PartialSuccess {
            summary: Some(s.into()),
            warning: warning.into(),
        }
    }

    #[test]
    fn aggregator_emits_summaries_on_final_record() {
        let agg = ImportAggregator::new(2);
        assert!(
            agg.record(ok_with("claude-code summary:\n  Sessions: 5"))
                .is_none()
        );
        let state = agg.record(ok_with("codex summary:\n  Sessions: 3"));
        assert_eq!(
            state,
            Some(AggregatedState::AllSucceeded {
                summaries: vec![
                    "claude-code summary:\n  Sessions: 5".into(),
                    "codex summary:\n  Sessions: 3".into(),
                ]
            })
        );
    }

    #[test]
    fn aggregator_emits_failures_on_final_record() {
        let agg = ImportAggregator::new(2);
        assert!(agg.record(ChildOutcome::Failure("a".into())).is_none());
        let state = agg.record(ok());
        assert_eq!(state, Some(AggregatedState::AnyFailed(vec!["a".into()])));
    }

    #[test]
    fn aggregator_keeps_partial_success_outside_failure_collection() {
        let agg = ImportAggregator::new(1);
        assert_eq!(
            agg.record(partial_with("summary", "one session failed")),
            Some(AggregatedState::PartialSucceeded {
                summaries: vec!["summary".into()],
                warnings: vec!["one session failed".into()],
            })
        );
    }

    #[test]
    fn aggregate_state_returns_partial_for_warnings_without_failures() {
        assert_eq!(
            aggregate_state(
                Vec::new(),
                vec!["one session failed".into(), "another session failed".into()],
                vec!["summary".into()]
            ),
            AggregatedState::PartialSucceeded {
                summaries: vec!["summary".into()],
                warnings: vec!["one session failed".into(), "another session failed".into()],
            }
        );
    }

    #[test]
    fn aggregate_state_returns_failure_when_any_failure_is_present() {
        assert_eq!(
            aggregate_state(
                vec!["failed".into()],
                vec!["warning".into()],
                vec!["summary".into()]
            ),
            AggregatedState::AnyFailed(vec!["failed".into()])
        );
    }

    #[test]
    fn aggregate_state_returns_success_when_no_failures_or_warnings_exist() {
        assert_eq!(
            aggregate_state(Vec::new(), Vec::new(), vec!["summary".into()]),
            AggregatedState::AllSucceeded {
                summaries: vec!["summary".into()]
            }
        );
    }

    #[test]
    fn partial_success_maps_to_warning_and_allows_downstream() {
        let state = AggregatedState::PartialSucceeded {
            summaries: vec!["summary".into()],
            warnings: vec!["one session failed".into()],
        };
        let outcome = import_terminal_step_outcome(state.clone(), false);
        assert_eq!(outcome.status, StepStatus::Warning);
        assert_eq!(outcome.message.as_deref(), Some("one session failed"));
        let dry_run_outcome = import_terminal_step_outcome(state.clone(), true);
        assert_eq!(dry_run_outcome.status, StepStatus::Warning);
        assert_eq!(
            dry_run_outcome.message.as_deref(),
            Some("one session failed\n\nsummary")
        );
        assert!(import_state_allows_downstream(&state));
        assert!(import_state_allows_downstream(
            &AggregatedState::AllSucceeded { summaries: vec![] }
        ));
        assert!(!import_state_allows_downstream(
            &AggregatedState::AnyFailed(vec!["failed".into()])
        ));
    }

    #[test]
    fn take_final_recovers_when_record_already_drained() {
        // Defensive path: after `record` returned Some it drained the
        // buffers. A second `take_final` from the same aggregator must
        // return an empty AllSucceeded — start_import's fallback relies
        // on this when the per-source loop somehow doesn't capture the
        // first Some.
        let agg = ImportAggregator::new(1);
        let _ = agg.record(ok_with("x"));
        assert_eq!(
            agg.take_final(),
            AggregatedState::AllSucceeded { summaries: vec![] }
        );
    }
}
