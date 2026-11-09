use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::super::StepStatus;
pub use super::super::staged_progress::{
    ImportNoticeCode, StagedProgressRegistry as ImportProgressRegistry, StagedStep as ImportStep,
    StagedStepState as ImportStepState,
};
use super::guards::ImportSource;

/// How `memories-import plain` groups discovered files into threads. Mirrors
/// the CLI's `--thread-strategy` values; serde decodes the kebab-case wire
/// form the dialog sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThreadStrategy {
    PerFile,
    PerDir,
    Single,
}

impl ThreadStrategy {
    pub(super) fn cli_value(self) -> &'static str {
        match self {
            ThreadStrategy::PerFile => "per-file",
            ThreadStrategy::PerDir => "per-dir",
            ThreadStrategy::Single => "single",
        }
    }
}

/// Plain-source-specific parameters. Unlike claude-code / codex (which
/// auto-discover their well-known log roots), the plain importer needs an
/// explicit directory plus a grouping strategy, so these travel as a separate
/// optional block referenced only when `"plain"` is among `sources`.
#[derive(Debug, Clone, Deserialize)]
pub struct PlainImportConfig {
    /// Root directory to walk recursively.
    pub root: PathBuf,
    /// Channel / external_id namespace prefix. `None` lets the CLI apply its
    /// own default (`plain`); when set it must match `^[a-z0-9_-]{1,32}$`.
    #[serde(default)]
    pub source_name: Option<String>,
    pub thread_strategy: ThreadStrategy,
}

impl PlainImportConfig {
    /// `^[a-z0-9_-]{1,32}$` without pulling in the regex crate — the same
    /// charset the memories CLI enforces on `--source-name`.
    pub(super) fn source_name_is_valid(name: &str) -> bool {
        (1..=32).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartImportRequest {
    pub sources: Vec<ImportSource>,
    /// ISO-8601 string (e.g. `2026-05-01T00:00:00Z`) passed verbatim as `--since`.
    /// `None` means "all".
    pub since: Option<String>,
    pub user_id: Option<i64>,
    pub dry_run: bool,
    pub labels: Vec<String>,
    /// Path to the `memories-import` binary. Resolved by the frontend from
    /// `get_settings()` (or built-in default).
    pub memories_import_bin: PathBuf,
    /// Post-import generation toggles. Required (not Option) so the dialog
    /// always sends its checkbox state; all-true reproduces the legacy
    /// "run everything" behaviour. Each gates the matching downstream
    /// `run_stream_step`; an unselected step is emitted as `Waiting`
    /// ("スキップ") instead of being dispatched.
    pub run_summary: bool,
    pub run_personality: bool,
    pub run_reflection: bool,
    /// Frontend-supplied dispatch id (UUID) used as the cancel key. The
    /// same id is returned in the response so the toast can wire its
    /// Stop button. Older callers may omit it; the backend then
    /// synthesizes a timestamp-shaped id as it did before cancel landed.
    #[serde(default)]
    pub dispatch_id: Option<String>,
    /// Plain-source parameters. Required when `sources` contains `"plain"`,
    /// ignored otherwise. `#[serde(default)]` keeps older request shapes (and
    /// the claude/codex-only flow) decoding without it.
    #[serde(default)]
    pub plain: Option<PlainImportConfig>,
}

/// Which post-import generation steps to run, decoded from the request
/// flags. Extracted as a pure value so the gating logic is unit-testable
/// without spawning the sidecar-dependent import task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DownstreamPlan {
    pub(super) summary: bool,
    pub(super) personality: bool,
    pub(super) reflection: bool,
}

impl DownstreamPlan {
    pub(super) fn from_request(req: &StartImportRequest) -> Self {
        Self {
            summary: req.run_summary,
            personality: req.run_personality,
            reflection: req.run_reflection,
        }
    }
}

/// Return downstream steps in their execution order with their selection state.
/// Keeping this table in one place prevents terminal no-op paths from drifting
/// away from the normal import pipeline.
pub(super) fn downstream_steps(plan: DownstreamPlan) -> [(ImportStep, bool); 4] {
    [
        (ImportStep::ThreadSummary, plan.summary),
        (ImportStep::ThreadGroupSummary, plan.summary),
        (ImportStep::ThreadPersonality, plan.personality),
        (ImportStep::Reflection, plan.reflection),
    ]
}

pub(super) fn should_run_thread_group_summary(
    summary_selected: bool,
    target_thread_ids: &[String],
) -> bool {
    summary_selected && !target_thread_ids.is_empty()
}

/// Decide the terminal state for each downstream step when no import targets
/// exist. This pure table keeps the no-op behavior testable independently of
/// Tauri event emission.
pub(super) fn no_target_downstream_decisions(
    plan: DownstreamPlan,
) -> [(ImportStep, StepStatus, &'static str); 4] {
    downstream_steps(plan).map(|(step, selected)| {
        if selected {
            (step, StepStatus::Done, "対象スレッドなし")
        } else {
            (step, StepStatus::Waiting, "スキップ")
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ManualImportSourceSelection {
    pub(super) sources: Vec<ImportSource>,
    pub(super) no_importable_log_sources: bool,
}

fn absolute_import_path(cwd: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn resolve_opencode_db_path_from_env(
    cwd: &Path,
    xdg_data_home: Option<&str>,
    home: Option<&str>,
) -> PathBuf {
    let relative_or_absolute = if let Some(xdg) = xdg_data_home.filter(|value| !value.is_empty()) {
        PathBuf::from(xdg).join("opencode/opencode.db")
    } else if let Some(home) = home.filter(|value| !value.is_empty()) {
        PathBuf::from(home).join(".local/share/opencode/opencode.db")
    } else {
        PathBuf::from("~/.local/share/opencode/opencode.db")
    };
    absolute_import_path(cwd, relative_or_absolute)
}

pub fn resolve_opencode_db_path(cwd: &Path) -> PathBuf {
    let mut env_values: HashMap<String, String> = std::env::vars().collect();
    let env_file = cwd.join(".env");
    if let Ok(entries) = dotenvy::from_path_iter(&env_file) {
        for entry in entries.flatten() {
            env_values.entry(entry.0).or_insert(entry.1);
        }
    }
    resolve_opencode_db_path_from_env(
        cwd,
        env_values.get("XDG_DATA_HOME").map(String::as_str),
        env_values.get("HOME").map(String::as_str),
    )
}

/// Filter only the well-known Claude Code / Codex roots for the manual import
/// command. Plain imports intentionally pass through untouched: their root is
/// validated by `validate_plain`, and periodic imports do not use this path.
pub(super) fn filter_manual_import_sources(
    requested: &[ImportSource],
    home_dir: Option<&Path>,
) -> ManualImportSourceSelection {
    filter_manual_import_sources_with_opencode_db(requested, home_dir, None)
}

pub(super) fn filter_manual_import_sources_with_opencode_db(
    requested: &[ImportSource],
    home_dir: Option<&Path>,
    opencode_db: Option<&Path>,
) -> ManualImportSourceSelection {
    let mut sources = Vec::with_capacity(requested.len());
    for source in requested {
        let root = match source {
            ImportSource::ClaudeCode => home_dir.map(|home| home.join(".claude/projects")),
            ImportSource::Codex => home_dir.map(|home| home.join(".codex/sessions")),
            ImportSource::OpenCode => opencode_db.map(Path::to_path_buf),
            ImportSource::Plain => None,
        };
        let importable = match source {
            ImportSource::Plain => true,
            ImportSource::OpenCode => root.as_ref().is_some_and(|path| path.is_file()),
            ImportSource::ClaudeCode | ImportSource::Codex => {
                root.as_ref().map(|path| path.is_dir()).unwrap_or(false)
            }
        };
        if importable {
            sources.push(*source);
        } else {
            match root {
                Some(path) => warn!(
                    source = source.cli_subcommand(),
                    path = %path.display(),
                    "manual import source root is missing; skipping source"
                ),
                None => warn!(
                    source = source.cli_subcommand(),
                    "manual import source root is unavailable; skipping source"
                ),
            }
        }
    }
    let no_importable_log_sources = !requested.is_empty() && sources.is_empty();
    ManualImportSourceSelection {
        sources,
        no_importable_log_sources,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StartImportResponse {
    pub job_id: String,
}

#[cfg(test)]
mod tests {
    use super::super::super::StepStatus;
    use super::super::super::staged_progress::ProgressKind;
    use super::super::aggregator::{step_label, validate_plain};
    use super::super::guards::ImportSource;
    use super::super::tests_common::{make_request, plain_request};
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn opencode_source_uses_the_wire_value_without_a_dash() {
        assert_eq!(ImportSource::OpenCode.cli_subcommand(), "opencode");
        assert_eq!(
            serde_json::to_string(&ImportSource::OpenCode).unwrap(),
            "\"opencode\""
        );
    }

    #[test]
    fn opencode_db_resolution_prefers_xdg_data_home() {
        let path =
            resolve_opencode_db_path_from_env(Path::new("/work"), Some("/xdg"), Some("/home/user"));
        assert_eq!(path, PathBuf::from("/xdg/opencode/opencode.db"));
    }

    #[test]
    fn opencode_db_resolution_uses_home_when_xdg_is_empty() {
        let path =
            resolve_opencode_db_path_from_env(Path::new("/work"), Some(""), Some("/home/user"));
        assert_eq!(
            path,
            PathBuf::from("/home/user/.local/share/opencode/opencode.db")
        );
    }

    #[test]
    fn opencode_db_resolution_keeps_literal_tilde_relative_to_cwd_without_home() {
        let path = resolve_opencode_db_path_from_env(Path::new("/work"), None, None);
        assert_eq!(
            path,
            PathBuf::from("/work/~/.local/share/opencode/opencode.db")
        );
    }

    #[test]
    fn opencode_db_resolution_treats_empty_home_like_absent_home() {
        let path = resolve_opencode_db_path_from_env(Path::new("/work"), Some(""), Some(""));
        assert_eq!(
            path,
            PathBuf::from("/work/~/.local/share/opencode/opencode.db")
        );
    }

    #[test]
    fn opencode_probe_requires_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode.db");
        assert!(
            !filter_manual_import_sources_with_opencode_db(
                &[ImportSource::OpenCode],
                None,
                Some(&db),
            )
            .sources
            .contains(&ImportSource::OpenCode)
        );
        std::fs::write(&db, b"fixture").unwrap();
        assert!(
            filter_manual_import_sources_with_opencode_db(
                &[ImportSource::OpenCode],
                None,
                Some(&db),
            )
            .sources
            .contains(&ImportSource::OpenCode)
        );
    }

    #[test]
    fn manual_source_filter_skips_only_missing_log_roots() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude/projects")).unwrap();

        let selection = filter_manual_import_sources(
            &[
                ImportSource::ClaudeCode,
                ImportSource::Codex,
                ImportSource::Plain,
            ],
            Some(home.path()),
        );

        assert_eq!(
            selection.sources,
            vec![ImportSource::ClaudeCode, ImportSource::Plain]
        );
        assert!(!selection.no_importable_log_sources);
    }

    #[test]
    fn manual_source_filter_preserves_requested_order_when_all_roots_exist() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude/projects")).unwrap();
        std::fs::create_dir_all(home.path().join(".codex/sessions")).unwrap();

        let selection = filter_manual_import_sources(
            &[
                ImportSource::Codex,
                ImportSource::Plain,
                ImportSource::ClaudeCode,
            ],
            Some(home.path()),
        );

        assert_eq!(
            selection.sources,
            vec![
                ImportSource::Codex,
                ImportSource::Plain,
                ImportSource::ClaudeCode
            ]
        );
        assert!(!selection.no_importable_log_sources);
    }

    #[test]
    fn manual_source_filter_keeps_codex_when_it_is_the_only_existing_log_root() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".codex/sessions")).unwrap();

        let selection = filter_manual_import_sources(
            &[ImportSource::ClaudeCode, ImportSource::Codex],
            Some(home.path()),
        );

        assert_eq!(selection.sources, vec![ImportSource::Codex]);
        assert!(!selection.no_importable_log_sources);
    }

    #[test]
    fn manual_source_filter_marks_notice_when_all_log_roots_are_missing() {
        let home = tempfile::tempdir().unwrap();
        let selection = filter_manual_import_sources(
            &[ImportSource::ClaudeCode, ImportSource::Codex],
            Some(home.path()),
        );

        assert!(selection.sources.is_empty());
        assert!(selection.no_importable_log_sources);
    }

    #[test]
    fn manual_source_filter_keeps_plain_and_does_not_notice_when_only_plain_is_selected() {
        let home = tempfile::tempdir().unwrap();
        let selection = filter_manual_import_sources(&[ImportSource::Plain], Some(home.path()));

        assert_eq!(selection.sources, vec![ImportSource::Plain]);
        assert!(!selection.no_importable_log_sources);
    }

    #[test]
    fn manual_source_filter_handles_missing_home_without_affecting_plain() {
        let selection = filter_manual_import_sources(
            &[
                ImportSource::ClaudeCode,
                ImportSource::Codex,
                ImportSource::Plain,
            ],
            None,
        );

        assert_eq!(selection.sources, vec![ImportSource::Plain]);
        assert!(!selection.no_importable_log_sources);
    }

    #[test]
    fn import_progress_snapshot_keeps_completed_steps_when_reflection_is_active() {
        let registry = ImportProgressRegistry::default();
        registry.start("job".into());
        registry.update_execution(
            ProgressKind::Import,
            "job",
            ImportStep::ThreadImport,
            StepStatus::Done,
            None,
            None,
        );
        registry.update_execution(
            ProgressKind::Import,
            "job",
            ImportStep::ThreadSummary,
            StepStatus::Done,
            None,
            None,
        );
        registry.update_execution(
            ProgressKind::Import,
            "job",
            ImportStep::ThreadGroupSummary,
            StepStatus::Done,
            None,
            None,
        );
        registry.update_execution(
            ProgressKind::Import,
            "job",
            ImportStep::ThreadPersonality,
            StepStatus::Done,
            None,
            None,
        );
        let snapshot = registry
            .update_execution(
                ProgressKind::Import,
                "job",
                ImportStep::Reflection,
                StepStatus::Active,
                Some("(1/2) 自省を生成中".into()),
                None,
            )
            .unwrap();
        assert_eq!(
            snapshot.steps[&ImportStep::ThreadImport].status,
            StepStatus::Done
        );
        assert_eq!(
            snapshot.steps[&ImportStep::ThreadSummary].status,
            StepStatus::Done
        );
        assert_eq!(
            snapshot.steps[&ImportStep::ThreadGroupSummary].status,
            StepStatus::Done
        );
        assert_eq!(
            snapshot.steps[&ImportStep::ThreadPersonality].status,
            StepStatus::Done
        );
        assert_eq!(
            snapshot.steps[&ImportStep::Reflection].status,
            StepStatus::Active
        );
    }

    #[test]
    fn validate_plain_accepts_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let req = plain_request(Some(PlainImportConfig {
            root: dir.path().to_path_buf(),
            source_name: Some("notes_01".into()),
            thread_strategy: ThreadStrategy::PerDir,
        }));
        assert!(validate_plain(&req).is_ok());
    }

    #[test]
    fn validate_plain_allows_no_plain_source() {
        // The claude/codex-only flow carries no plain block and must pass.
        assert!(validate_plain(&make_request(true, true, true)).is_ok());
    }

    #[test]
    fn validate_plain_rejects_config_without_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = make_request(false, false, false);
        req.plain = Some(PlainImportConfig {
            root: dir.path().to_path_buf(),
            source_name: None,
            thread_strategy: ThreadStrategy::PerFile,
        });
        assert!(validate_plain(&req).is_err());
    }

    #[test]
    fn validate_plain_rejects_source_without_config() {
        assert!(validate_plain(&plain_request(None)).is_err());
    }

    #[test]
    fn validate_plain_rejects_bad_source_name() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "Notes",
            "",
            "a".repeat(33).as_str(),
            "with space",
            "dot.name",
        ] {
            let req = plain_request(Some(PlainImportConfig {
                root: dir.path().to_path_buf(),
                source_name: Some(bad.to_string()),
                thread_strategy: ThreadStrategy::PerFile,
            }));
            assert!(
                validate_plain(&req).is_err(),
                "source-name {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_plain_rejects_missing_root() {
        let req = plain_request(Some(PlainImportConfig {
            root: PathBuf::from("/nonexistent/lookback/plain/root"),
            source_name: Some("notes".into()),
            thread_strategy: ThreadStrategy::PerFile,
        }));
        assert!(validate_plain(&req).is_err());
    }

    #[test]
    fn thread_strategy_decodes_known_values() {
        for (wire, expected) in [
            ("per-file", ThreadStrategy::PerFile),
            ("per-dir", ThreadStrategy::PerDir),
            ("single", ThreadStrategy::Single),
        ] {
            let s: ThreadStrategy =
                serde_json::from_value(serde_json::Value::String(wire.into())).unwrap();
            assert_eq!(s, expected);
            assert_eq!(s.cli_value(), wire);
        }
    }

    #[test]
    fn thread_strategy_rejects_unknown_value() {
        let r: Result<ThreadStrategy, _> =
            serde_json::from_value(serde_json::Value::String("per-week".into()));
        assert!(r.is_err());
    }

    #[test]
    fn plain_config_decodes_from_request_json() {
        let json = serde_json::json!({
            "sources": ["plain"],
            "since": null,
            "user_id": 1,
            "dry_run": false,
            "labels": [],
            "memories_import_bin": "/usr/local/bin/memories-import",
            "run_summary": false,
            "run_personality": false,
            "run_reflection": false,
            "plain": {
                "root": "/tmp/notes",
                "source_name": "notes",
                "thread_strategy": "per-dir"
            }
        });
        let req: StartImportRequest = serde_json::from_value(json).unwrap();
        let plain = req.plain.expect("plain block decoded");
        assert_eq!(plain.root, PathBuf::from("/tmp/notes"));
        assert_eq!(plain.source_name.as_deref(), Some("notes"));
        assert_eq!(plain.thread_strategy, ThreadStrategy::PerDir);
    }

    #[test]
    fn request_without_plain_defaults_to_none() {
        let json = serde_json::json!({
            "sources": ["claude-code"],
            "since": null,
            "user_id": 1,
            "dry_run": false,
            "labels": [],
            "memories_import_bin": "/usr/local/bin/memories-import",
            "run_summary": true,
            "run_personality": true,
            "run_reflection": true
        });
        let req: StartImportRequest = serde_json::from_value(json).unwrap();
        assert!(req.plain.is_none());
    }

    #[test]
    fn start_import_request_accepts_dispatch_id_for_cancel() {
        // Older callers omit dispatch_id; the backend then synthesises
        // an `import-<ts>` id. New callers send a UUID and the
        // response should carry it back so the toast's Stop click can
        // target the right entry.
        let raw = r#"{
            "sources": ["claude-code"],
            "since": null,
            "user_id": 1,
            "dry_run": true,
            "labels": [],
            "memories_import_bin": "/bin/true",
            "run_summary": true,
            "run_personality": true,
            "run_reflection": true,
            "dispatch_id": "abc-uuid"
        }"#;
        let req: StartImportRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.dispatch_id.as_deref(), Some("abc-uuid"));

        let raw_old = r#"{
            "sources": ["claude-code"],
            "since": null,
            "user_id": 1,
            "dry_run": true,
            "labels": [],
            "memories_import_bin": "/bin/true",
            "run_summary": true,
            "run_personality": true,
            "run_reflection": true
        }"#;
        let req_old: StartImportRequest = serde_json::from_str(raw_old).unwrap();
        assert!(req_old.dispatch_id.is_none());
    }

    #[test]
    fn step_label_returns_stable_strings_for_current_step_slot() {
        // The `current_step` slot's value rides the trace log; pin
        // the labels so a future refactor that moves the enum
        // can't silently change the trace contract.
        assert_eq!(step_label(ImportStep::ThreadImport), "thread-import");
        assert_eq!(step_label(ImportStep::ThreadSummary), "thread-summary");
        assert_eq!(
            step_label(ImportStep::ThreadGroupSummary),
            "thread-group-summary"
        );
        assert_eq!(
            step_label(ImportStep::ThreadPersonality),
            "thread-personality"
        );
        assert_eq!(step_label(ImportStep::Reflection), "reflection");
    }

    #[test]
    fn cancelled_status_serializes_for_import_step_events() {
        let json = serde_json::to_string(&StepStatus::Cancelled).unwrap();
        assert_eq!(json, "\"cancelled\"");
    }
}
