use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::Command;

use super::super::connection::ResolvedTargets;
use super::guards::ImportSource;
use super::request::PlainImportConfig;

/// Per-job invocation context shared across every source's CLI invocation.
pub(super) struct ImportPlan {
    bin: PathBuf,
    working_dir: PathBuf,
    user_id: i64,
    memories_url: String,
    jobworkerp_addr: String,
    since: Option<String>,
    dry_run: bool,
    labels: Vec<String>,
    /// Dedicated directory for this import's native tracing output. The
    /// parent-captured stdout/stderr files remain the canonical logs.
    pub(super) native_log_dir: PathBuf,
    /// Plain-source parameters, consumed by `build_command`'s plain arm.
    /// `None` for a request that doesn't select the plain source.
    plain: Option<PlainImportConfig>,
    opencode_db_path: Option<PathBuf>,
}

impl ImportPlan {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        bin: PathBuf,
        working_dir: PathBuf,
        user_id: i64,
        targets: &ResolvedTargets,
        since: Option<String>,
        dry_run: bool,
        labels: Vec<String>,
        native_log_dir: PathBuf,
        plain: Option<PlainImportConfig>,
    ) -> Self {
        Self::new_with_opencode_db(
            bin,
            working_dir,
            user_id,
            targets,
            since,
            dry_run,
            labels,
            native_log_dir,
            plain,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_with_opencode_db(
        bin: PathBuf,
        working_dir: PathBuf,
        user_id: i64,
        targets: &ResolvedTargets,
        since: Option<String>,
        dry_run: bool,
        labels: Vec<String>,
        native_log_dir: PathBuf,
        plain: Option<PlainImportConfig>,
        opencode_db_path: Option<PathBuf>,
    ) -> Self {
        Self {
            bin,
            working_dir,
            user_id,
            memories_url: targets.memories_url.clone(),
            jobworkerp_addr: targets.jobworkerp_url.clone(),
            since,
            dry_run,
            labels,
            native_log_dir,
            plain,
            opencode_db_path,
        }
    }

    #[cfg(test)]
    pub(super) fn set_opencode_db_path(&mut self, path: PathBuf) {
        self.opencode_db_path = Some(path);
    }

    #[cfg(test)]
    pub(super) fn working_dir(&self) -> &std::path::Path {
        &self.working_dir
    }

    pub(super) fn build_command(&self, source: ImportSource) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.current_dir(&self.working_dir);
        cmd.arg("--user-id")
            .arg(self.user_id.to_string())
            .arg("--server-url")
            .arg(&self.memories_url)
            .arg("--events-jsonl");

        if let Some(since) = self.since.as_deref() {
            cmd.arg("--since").arg(since);
        }
        if self.dry_run {
            cmd.arg("--dry-run");
        }

        cmd.arg(source.cli_subcommand());

        if !self.labels.is_empty() {
            cmd.arg("--labels").arg(self.labels.join(","));
        }

        match source {
            ImportSource::ClaudeCode => {
                cmd.arg("--all-projects");
            }
            ImportSource::Codex => {
                cmd.arg("--all-sessions");
            }
            ImportSource::OpenCode => {
                cmd.arg("--all-sessions");
                if let Some(path) = &self.opencode_db_path {
                    cmd.arg("--opencode-db").arg(path);
                }
            }
            ImportSource::Plain => {
                // `validate_plain` guarantees the config is present whenever
                // the plain source reaches the plan. These are subcommand-scoped
                // args, so they must follow the `plain` token emitted above.
                let cfg = self
                    .plain
                    .as_ref()
                    .expect("validated: plain config present when plain source is selected");
                cmd.arg("--root")
                    .arg(&cfg.root)
                    .arg("--thread-strategy")
                    .arg(cfg.thread_strategy.cli_value());
                if let Some(name) = &cfg.source_name {
                    cmd.arg("--source-name").arg(name);
                }
            }
        }

        cmd.env("JOBWORKERP_ADDR", &self.jobworkerp_addr)
            .env("RUST_LOG", "info")
            // Force the child's native tracing log into this import's isolated
            // data-root scope; otherwise it defaults to cwd, which is `/` for
            // a Finder-launched .app and panics on the unwritable root.
            //
            // The child reads its log config via `envy::prefixed("LOG_")` into
            // a struct whose `use_json` / `use_stdout` are plain `bool` (no
            // serde default), so envy treats them as REQUIRED: missing either
            // makes the whole deserialize fail and the importer silently falls
            // back to `current_dir()` — which is exactly why LOG_FILE_DIR alone
            // had no effect. We must set all three for the dir to take.
            .env("LOG_FILE_DIR", &self.native_log_dir)
            .env("LOG_USE_JSON", "true")
            .env("LOG_APP_NAME", "Lookback")
            .env("LOG_USE_STDOUT", "true")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::connection::ResolvedTargets;
    use super::super::guards::ImportSource;
    use super::super::request::{PlainImportConfig, ThreadStrategy};
    use super::super::tests_common::plain_cfg;
    use super::*;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    fn dummy_targets() -> ResolvedTargets {
        ResolvedTargets {
            jobworkerp_url: "http://127.0.0.1:9000".into(),
            memories_url: "http://127.0.0.1:9010".into(),
        }
    }

    fn make_plan() -> ImportPlan {
        make_plan_with_plain(None)
    }

    fn make_plan_with_plain(plain: Option<PlainImportConfig>) -> ImportPlan {
        ImportPlan::new(
            PathBuf::from("/usr/local/bin/memories-import"),
            std::env::current_dir().unwrap(),
            1,
            &dummy_targets(),
            Some("2026-05-01T00:00:00Z".into()),
            false,
            vec!["lookback".into()],
            PathBuf::from("/tmp/lookback-log/native/import-test"),
            plain,
        )
    }

    #[test]
    fn opencode_command_passes_all_sessions_and_resolved_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode.db");
        let mut plan = make_plan();
        plan.set_opencode_db_path(db.clone());
        let args = plan
            .build_command(ImportSource::OpenCode)
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.iter().any(|arg| arg == "opencode"));
        assert!(args.iter().any(|arg| arg == "--all-sessions"));
        let db_arg = db.to_string_lossy().into_owned();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--opencode-db".to_string(), db_arg.clone()])
        );
    }

    #[test]
    fn importer_command_uses_the_resolved_application_working_directory() {
        let plan = make_plan();

        assert_eq!(
            plan.build_command(ImportSource::OpenCode)
                .as_std()
                .get_current_dir(),
            Some(plan.working_dir())
        );
    }

    #[test]
    fn build_command_passes_user_id_server_url_since_and_subcommand() {
        let plan = make_plan();
        let cmd = plan.build_command(ImportSource::ClaudeCode);
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(args.windows(2).any(|w| w[0] == "--user-id" && w[1] == "1"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--server-url" && w[1] == "http://127.0.0.1:9010")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--since" && w[1] == "2026-05-01T00:00:00Z")
        );
        assert!(args.iter().any(|a| *a == "claude-code"));
        assert!(args.iter().any(|a| *a == "--all-projects"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--labels" && w[1] == "lookback")
        );
    }

    #[test]
    fn build_command_enables_versioned_events_for_every_import_source() {
        let plain = make_plan_with_plain(Some(plain_cfg(Some("notes"), ThreadStrategy::PerFile)));
        for (plan, source) in [
            (make_plan(), ImportSource::ClaudeCode),
            (make_plan(), ImportSource::Codex),
            (plain, ImportSource::Plain),
        ] {
            let command = plan.build_command(source);
            let args = command.as_std().get_args().collect::<Vec<_>>();
            assert!(
                args.iter().any(|arg| *arg == "--events-jsonl"),
                "missing event flag for {source:?}"
            );
        }
    }

    #[test]
    fn build_command_does_not_leak_summarize_or_personality_flags() {
        // Dispatch flags live on `BatchDispatch::*_input` now; the CLI
        // must stay a pure importer so its exit doesn't block on the
        // downstream workflows.
        let plan = make_plan();
        let cmd = plan.build_command(ImportSource::ClaudeCode);
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(!args.iter().any(|a| *a == "--summarize-workflow"));
        assert!(!args.iter().any(|a| *a == "--summarize-after-json"));
        assert!(!args.iter().any(|a| *a == "--personality-workflow"));
        assert!(
            !args
                .iter()
                .any(|a| *a == "--extract-personality-after-json")
        );
    }

    fn plan_dry_run() -> ImportPlan {
        ImportPlan::new(
            PathBuf::from("/usr/local/bin/memories-import"),
            std::env::current_dir().unwrap(),
            1,
            &dummy_targets(),
            Some("2026-05-01T00:00:00Z".into()),
            true,
            vec!["lookback".into()],
            PathBuf::from("/tmp/lookback-log/native/import-test"),
            None,
        )
    }

    #[test]
    fn build_command_marks_dry_run() {
        let plan = plan_dry_run();
        let cmd = plan.build_command(ImportSource::ClaudeCode);
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(args.iter().any(|a| *a == "--dry-run"));
    }

    #[test]
    fn build_command_plain_emits_root_strategy_source_name() {
        let plan = make_plan_with_plain(Some(plain_cfg(Some("notes"), ThreadStrategy::PerDir)));
        let cmd = plan.build_command(ImportSource::Plain);
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(args.iter().any(|a| *a == "plain"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--root" && w[1] == "/tmp/notes")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--thread-strategy" && w[1] == "per-dir")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--source-name" && w[1] == "notes")
        );
        // Plain must not leak the other sources' discovery flags.
        assert!(!args.iter().any(|a| *a == "--all-projects"));
        assert!(!args.iter().any(|a| *a == "--all-sessions"));
    }

    #[test]
    fn build_command_plain_omits_source_name_when_absent() {
        let plan = make_plan_with_plain(Some(plain_cfg(None, ThreadStrategy::Single)));
        let cmd = plan.build_command(ImportSource::Plain);
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(!args.iter().any(|a| *a == "--source-name"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--thread-strategy" && w[1] == "single")
        );
    }

    #[test]
    fn build_command_plain_passes_global_user_id_server_url_since() {
        // The plain source shares the global header path (user-id / server-url
        // / since) with the other sources.
        let plan = make_plan_with_plain(Some(plain_cfg(Some("notes"), ThreadStrategy::PerFile)));
        let cmd = plan.build_command(ImportSource::Plain);
        let args: Vec<&OsStr> = cmd.as_std().get_args().collect();
        assert!(args.windows(2).any(|w| w[0] == "--user-id" && w[1] == "1"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--server-url" && w[1] == "http://127.0.0.1:9010")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--since" && w[1] == "2026-05-01T00:00:00Z")
        );
    }

    #[test]
    fn build_command_pins_native_log_file_dir_to_import_scope() {
        // Regression: a Finder-launched .app inherits cwd `/`, where the
        // child's command-utils tracing init panics creating its log file.
        // LOG_FILE_DIR must point at this import's isolated native log dir — and because
        // envy treats the config's `use_json`/`use_stdout` bools as required,
        // LOG_USE_JSON/LOG_USE_STDOUT must accompany it or the whole config
        // deserialize fails and the dir is silently dropped.
        let plan = make_plan();
        let cmd = plan.build_command(ImportSource::ClaudeCode);
        let env_of = |key: &str| -> Option<std::ffi::OsString> {
            cmd.as_std()
                .get_envs()
                .find(|(k, _)| *k == OsStr::new(key))
                .and_then(|(_, v)| v)
                .map(|v| v.to_os_string())
        };
        assert_eq!(
            env_of("LOG_FILE_DIR").as_deref(),
            Some(OsStr::new("/tmp/lookback-log/native/import-test"))
        );
        assert_eq!(env_of("LOG_USE_JSON").as_deref(), Some(OsStr::new("true")));
        assert_eq!(
            env_of("LOG_USE_STDOUT").as_deref(),
            Some(OsStr::new("true"))
        );
    }
}
