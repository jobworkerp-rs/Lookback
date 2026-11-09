//! Registration of the language-specific generation workers
//! (`memories-<feature>-single-<lang>`) at sidecar startup.
//!
//! The multilingual generation batches resolve their per-thread single worker
//! by name (`workerName: "memories-<feature>-single-${output_language}"`), so
//! those workers MUST exist before the memories sidecar dispatches its first
//! generation workflow. We register them by spawning the bundled
//! `memories-import upsert-generation-workers` subcommand, which reads the
//! local-merged single YAML + `prompts/<role>.<lang>.txt` under
//! `<repo_root>/workers/<feature>/` and bakes the prompt text into each
//! worker's `settings.workflow_context`. The external-file dependency is
//! therefore confined to this one registration call (the runtime never reads
//! the prompt files).
//!
//! Failure is non-fatal: a missing binary / unreachable jobworkerp / non-zero
//! exit is surfaced as a `WorkerApplyFailed` warning (same as the YAML worker
//! apply path) so the memories sidecar still boots and the UI can degrade the
//! generation features rather than blocking startup.

use std::path::Path;
use std::process::Command;

use super::lifecycle::{SidecarWarning, SidecarWarningKind};

/// Channel the language workers are registered on. They are LLM-containing
/// single workflows, so they belong on the same `llm_workflow` channel
/// (concurrency 1) the legacy `*-single.yaml` ran on — NOT the CLI's default
/// `workflow_lang`, which is not part of agent-app's `WORKER_CHANNELS`.
pub(crate) const LANG_WORKER_CHANNEL: &str = "llm_workflow";

/// Workers `upsert-generation-workers --feature all` registers, one per
/// output language each.
pub(crate) fn generation_worker_names() -> Vec<String> {
    use crate::commands::analysis_dispatch::PERSONALITY_MERGE_WORKER_BASE;
    use crate::commands::background_jobs::{
        PERSONALITY_SINGLE_WORKER_BASE, REFLECTION_SINGLE_WORKER_BASE, SUMMARY_SINGLE_WORKER_BASES,
    };
    SUMMARY_SINGLE_WORKER_BASES
        .iter()
        .chain([
            &PERSONALITY_SINGLE_WORKER_BASE,
            &REFLECTION_SINGLE_WORKER_BASE,
            &PERSONALITY_MERGE_WORKER_BASE,
        ])
        .flat_map(|base| {
            crate::commands::SUPPORTED_LANGUAGES
                .iter()
                .map(move |language| format!("{base}-{language}"))
        })
        .collect()
}

fn generation_workers_native_log_dir(log_dir: &Path) -> std::io::Result<std::path::PathBuf> {
    crate::log_rotation::native_log_dir(log_dir, "generation-workers")
}

/// Marks the short-lived generation-worker scope closed on every exit path,
/// including cancellation while the child process is being awaited.
struct NativeGenerationLogGuard(std::path::PathBuf);

impl Drop for NativeGenerationLogGuard {
    fn drop(&mut self) {
        if let Err(error) = crate::log_rotation::mark_native_log_dir_closed(&self.0) {
            tracing::debug!(
                path = %self.0.display(),
                %error,
                "could not mark native generation log closed",
            );
        }
    }
}

/// Build the `memories-import upsert-generation-workers` command. Pure (no I/O)
/// so the argv/env contract can be unit-tested. The caller spawns it.
pub(crate) fn build_upsert_command(
    bin: &Path,
    repo_root: &Path,
    channel: &str,
    jobworkerp_url: &str,
    log_dir: &Path,
) -> Command {
    let mut cmd = Command::new(bin);
    cmd.arg("upsert-generation-workers")
        .arg("--feature")
        .arg("all")
        .arg("--language")
        .arg("all")
        .arg("--channel")
        .arg(channel)
        .arg("--timeout-sec")
        .arg("30")
        .arg("--repo-root")
        .arg(repo_root)
        // `new_by_env` reads JOBWORKERP_ADDR with `.expect()` — without it the
        // subcommand panics instead of connecting to the live jobworkerp.
        .env("JOBWORKERP_ADDR", jobworkerp_url)
        .env("RUST_LOG", "info")
        // Same tracing-init guard as `import.rs::build_command`: a
        // Finder-launched .app runs with cwd=`/`, and command-utils' tracing
        // setup writes its log relative to cwd and panics on the unwritable
        // root unless all four LOG_* vars are set (envy treats the bool fields
        // as required).
        .env("LOG_FILE_DIR", log_dir)
        .env("LOG_USE_JSON", "true")
        .env("LOG_APP_NAME", "Lookback")
        .env("LOG_USE_STDOUT", "true");
    cmd
}

/// `detail` shared by every fail-soft warning this module emits, so the UI
/// always explains the consequence (generation degrades, startup continues).
const NOT_REGISTERED: &str = "language generation workers were not registered";

/// Push a `WorkerApplyFailed` warning. Single constructor so the kind/detail
/// contract lives in one place across this module's several failure paths.
fn warn_not_registered(warnings: &mut Vec<SidecarWarning>, message: String) {
    warnings.push(SidecarWarning {
        kind: SidecarWarningKind::WorkerApplyFailed,
        message,
        detail: Some(NOT_REGISTERED.into()),
    });
}

/// Preserve the helper process output in the parent-owned canonical log. The
/// child also writes a native tracing file, but that file is only a diagnostic
/// duplicate and is isolated under `log/native/<launch-id>/`.
fn forward_generation_worker_output(stdout: &[u8], stderr: &[u8]) {
    for line in String::from_utf8_lossy(stdout).lines() {
        tracing::info!(target: "generation-workers", "{line}");
    }
    for line in String::from_utf8_lossy(stderr).lines() {
        tracing::warn!(target: "generation-workers", "{line}");
    }
}

/// Line prefixes `memories-import upsert-generation-workers` prints per
/// registered worker (stdout) and per skipped feature (stderr). They mirror
/// the stable output constants in memories
/// `agent-chat-import/src/generation_workers.rs` (`UPSERTED_PREFIX`,
/// `DRY_RUN_PREFIX`, `SKIPPED_FEATURE_PREFIX`); the fixture-based contract
/// tests below catch upstream rewording.
const REGISTERED_PREFIXES: &[&str] = &[
    "upserted generation worker: ",
    "[dry-run] would upsert generation worker: ",
];
const SKIPPED_FEATURE_PREFIX: &str = "warning: skipped generation feature ";

#[derive(Debug, Default, PartialEq, Eq)]
struct UpsertOutputSummary {
    registered: Vec<String>,
    failed_features: Vec<String>,
}

/// memories-import skips a feature whose files fail to load and still
/// registers the rest, so a non-zero exit can mean a partial registration.
fn summarize_upsert_output(stdout: &str, stderr: &str) -> UpsertOutputSummary {
    let registered = stdout
        .lines()
        .filter_map(|line| {
            REGISTERED_PREFIXES
                .iter()
                .find_map(|prefix| line.trim().strip_prefix(prefix))
        })
        .map(|name| name.trim().to_string())
        .collect();
    let failed_features = stderr
        .lines()
        .filter_map(|line| line.trim().strip_prefix(SKIPPED_FEATURE_PREFIX))
        .filter_map(|rest| {
            rest.split_once(':')
                .map(|(feature, _)| feature.trim().to_string())
        })
        .collect();
    UpsertOutputSummary {
        registered,
        failed_features,
    }
}

fn failure_message(code: Option<i32>, summary: &UpsertOutputSummary, stderr_tail: &str) -> String {
    if summary.registered.is_empty() {
        format!("registering language generation workers failed (exit {code:?}): {stderr_tail}")
    } else {
        format!(
            "language generation workers partially registered ({}); failed features: {}",
            summary.registered.len(),
            summary.failed_features.join(", ")
        )
    }
}

/// Resolve the binary + repo root and run the upsert, converting any failure
/// into a `WorkerApplyFailed` warning instead of propagating it (fail-soft).
pub(crate) async fn register_generation_workers(
    jobworkerp_url: &str,
    log_dir: &Path,
    warnings: &mut Vec<SidecarWarning>,
) {
    // Reuse the single owner of the memories-import resolution (env override +
    // bundled/fallback paths + exists check) so it can't drift from the import
    // command's `resolve_memories_import_bin`.
    let bin = match crate::resolve_memories_import_bin_path() {
        Ok(p) => p,
        Err(e) => {
            warn_not_registered(warnings, e.to_string());
            return;
        }
    };

    let repo_root = match crate::data::paths::lang_workers_repo_root() {
        Ok(p) if p.exists() => p,
        Ok(p) => {
            warn_not_registered(
                warnings,
                format!("lang-workers repo root not found at {}", p.display()),
            );
            return;
        }
        Err(e) => {
            warn_not_registered(
                warnings,
                format!("lang-workers repo root resolve failed: {e}"),
            );
            return;
        }
    };

    let native_log_dir = match generation_workers_native_log_dir(log_dir) {
        Ok(path) => path,
        Err(error) => {
            warn_not_registered(
                warnings,
                format!("could not create native generation log directory: {error}"),
            );
            return;
        }
    };
    let _native_log_guard = NativeGenerationLogGuard(native_log_dir.clone());
    let std_cmd = build_upsert_command(
        &bin,
        &repo_root,
        LANG_WORKER_CHANNEL,
        jobworkerp_url,
        &native_log_dir,
    );
    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Cancellation must terminate the short-lived child before the guard
        // closes its native scope; otherwise a detached process could still
        // append after the `.closed` marker is written.
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            // The guard closes the unowned scope when spawn fails; no child
            // PID must be recorded for a process that never started.
            warn_not_registered(
                warnings,
                format!("could not spawn memories-import: {error}"),
            );
            return;
        }
    };
    if let Some(pid) = child.id()
        && let Err(error) = crate::log_rotation::mark_native_log_dir_pid(&native_log_dir, pid)
    {
        tracing::debug!(%error, pid, "could not record native generation child PID");
    }
    let output = child.wait_with_output().await;
    match output {
        Ok(out) => {
            forward_generation_worker_output(&out.stdout, &out.stderr);
            if out.status.success() {
                tracing::info!(
                    repo_root = %repo_root.display(),
                    "language generation workers registered",
                );
                return;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            // Keep the tail — the actionable cause (e.g. an empty prompt file
            // failing `read_non_empty`) is usually the last line.
            let lines: Vec<&str> = stderr.lines().collect();
            let tail = lines[lines.len().saturating_sub(5)..].join("\n");
            let summary = summarize_upsert_output(&stdout, &stderr);
            tracing::warn!(
                status = ?out.status.code(),
                registered = summary.registered.len(),
                failed_features = ?summary.failed_features,
                stderr = %stderr,
                "upsert-generation-workers failed (continuing)",
            );
            warnings.push(SidecarWarning {
                kind: SidecarWarningKind::WorkerApplyFailed,
                message: failure_message(out.status.code(), &summary, &tail),
                detail: Some(repo_root.display().to_string()),
            });
        }
        Err(e) => {
            warnings.push(SidecarWarning {
                kind: SidecarWarningKind::WorkerApplyFailed,
                message: format!("could not spawn memories-import: {e}"),
                detail: Some(bin.display().to_string()),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn upsert_output_lists_registered_workers_and_skipped_features() {
        let stdout = "upserted generation worker: memories-thread-summary-single-ja\n\
                      upserted generation worker: memories-thread-summary-single-en\n";
        let stderr = "warning: skipped generation feature thread-group-summary: read workflow yaml /x: No such file\n\
                      Error: 1 generation feature(s) failed: thread-group-summary; 2 worker(s) registered\n";
        assert_eq!(
            summarize_upsert_output(stdout, stderr),
            UpsertOutputSummary {
                registered: vec![
                    "memories-thread-summary-single-ja".into(),
                    "memories-thread-summary-single-en".into(),
                ],
                failed_features: vec!["thread-group-summary".into()],
            }
        );
    }

    #[test]
    fn upsert_output_reads_dry_run_lines_and_ignores_unknown_ones() {
        let stdout = "[dry-run] would upsert generation worker: memories-thread-reflection-single-ja\n\
                      some unrelated log line\n";
        assert_eq!(
            summarize_upsert_output(stdout, "INFO noise\n"),
            UpsertOutputSummary {
                registered: vec!["memories-thread-reflection-single-ja".into()],
                failed_features: vec![],
            }
        );
    }

    #[test]
    fn upsert_output_reports_a_failure_without_any_registration() {
        let stderr = "warning: skipped generation feature reflection: read prompt file a\n\
                      warning: skipped generation feature thread-summary: read prompt file b\n\
                      Error: 2 generation feature(s) failed: reflection, thread-summary; 0 worker(s) registered\n";
        let summary = summarize_upsert_output("", stderr);
        assert!(summary.registered.is_empty());
        assert_eq!(
            summary.failed_features,
            vec!["reflection", "thread-summary"]
        );
    }

    // Captured from the real `memories-import upsert-generation-workers
    // --dry-run --feature all --language all --channel llm_workflow` output.
    // Regenerate by running that command against `workers/lang-workers`
    // (success) and a copy without `workers/thread-group-summary` (partial),
    // replacing the temporary repo root with `<repo-root>`.
    const SUCCESS_STDOUT: &str =
        include_str!("../../tests/fixtures/memories_upsert_generation_workers_success.stdout");
    const PARTIAL_STDOUT: &str =
        include_str!("../../tests/fixtures/memories_upsert_generation_workers_partial.stdout");
    const PARTIAL_STDERR: &str =
        include_str!("../../tests/fixtures/memories_upsert_generation_workers_partial.stderr");

    #[test]
    fn contract_parses_real_dry_run_output_of_every_lang_worker() {
        let summary = summarize_upsert_output(SUCCESS_STDOUT, "");
        assert_eq!(summary.registered.len(), 16, "{summary:?}");
        assert!(summary.failed_features.is_empty());
        for name in [
            "memories-thread-group-summary-single-ja",
            "memories-thread-group-summary-single-en",
        ] {
            assert!(summary.registered.iter().any(|r| r == name), "{name}");
        }
    }

    #[test]
    fn contract_parses_real_partial_failure_output() {
        let summary = summarize_upsert_output(PARTIAL_STDOUT, PARTIAL_STDERR);
        assert_eq!(summary.registered.len(), 14, "{summary:?}");
        assert_eq!(summary.failed_features, vec!["thread-group-summary"]);
        assert!(
            !summary
                .registered
                .iter()
                .any(|name| name.starts_with("memories-thread-group-summary-single")),
            "{summary:?}"
        );
    }

    #[test]
    fn partial_registration_names_the_failed_features() {
        let summary = UpsertOutputSummary {
            registered: vec!["a".into(), "b".into()],
            failed_features: vec!["thread-group-summary".into()],
        };
        assert_eq!(
            failure_message(Some(1), &summary, "tail"),
            "language generation workers partially registered (2); failed features: thread-group-summary"
        );
        assert_eq!(
            failure_message(Some(1), &UpsertOutputSummary::default(), "tail"),
            "registering language generation workers failed (exit Some(1)): tail"
        );
    }

    /// Pull the argv (program + args) out of a std `Command` for assertions.
    fn argv(cmd: &Command) -> Vec<String> {
        let mut v = vec![cmd.get_program().to_string_lossy().into_owned()];
        v.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
        v
    }

    fn env_of(cmd: &Command, key: &str) -> Option<String> {
        cmd.get_envs().find_map(|(k, val)| {
            if k == key {
                val.map(|s| s.to_string_lossy().into_owned())
            } else {
                None
            }
        })
    }

    #[test]
    fn upsert_command_uses_all_features_languages_and_repo_root() {
        let cmd = build_upsert_command(
            &PathBuf::from("/bin/memories-import"),
            &PathBuf::from("/data/workers/lang-workers"),
            LANG_WORKER_CHANNEL,
            "http://127.0.0.1:9123",
            &PathBuf::from("/logs"),
        );
        let args = argv(&cmd);
        assert_eq!(args[0], "/bin/memories-import");
        assert!(args.contains(&"upsert-generation-workers".to_string()));
        // Pin the flag/value pairs so a flag rename on the memories side is a
        // test failure, not a silent no-op at sidecar startup.
        for pair in [
            ("--feature", "all"),
            ("--language", "all"),
            ("--channel", LANG_WORKER_CHANNEL),
            ("--repo-root", "/data/workers/lang-workers"),
        ] {
            let i = args.iter().position(|a| a == pair.0).expect(pair.0);
            assert_eq!(args[i + 1], pair.1, "value for {}", pair.0);
        }
    }

    #[test]
    fn upsert_command_channel_is_llm_workflow_not_workflow_lang() {
        // The CLI default is `workflow_lang`, which is NOT one of agent-app's
        // registered WORKER_CHANNELS — registering there would leave the
        // workers unschedulable. Pin the explicit override.
        assert_eq!(LANG_WORKER_CHANNEL, "llm_workflow");
        let cmd = build_upsert_command(
            &PathBuf::from("/bin/memories-import"),
            &PathBuf::from("/root"),
            LANG_WORKER_CHANNEL,
            "http://127.0.0.1:9000",
            &PathBuf::from("/logs"),
        );
        let args = argv(&cmd);
        assert!(!args.iter().any(|a| a == "workflow_lang"));
    }

    #[test]
    fn upsert_command_sets_jobworkerp_addr_and_log_env() {
        let cmd = build_upsert_command(
            &PathBuf::from("/bin/memories-import"),
            &PathBuf::from("/root"),
            LANG_WORKER_CHANNEL,
            "http://127.0.0.1:9042",
            &PathBuf::from("/var/log/lookback"),
        );
        assert_eq!(
            env_of(&cmd, "JOBWORKERP_ADDR").as_deref(),
            Some("http://127.0.0.1:9042")
        );
        // All four LOG_* are required for the child's tracing dir to take.
        assert_eq!(
            env_of(&cmd, "LOG_FILE_DIR").as_deref(),
            Some("/var/log/lookback")
        );
        assert_eq!(env_of(&cmd, "LOG_USE_JSON").as_deref(), Some("true"));
        assert_eq!(env_of(&cmd, "LOG_USE_STDOUT").as_deref(), Some("true"));
        assert_eq!(env_of(&cmd, "LOG_APP_NAME").as_deref(), Some("Lookback"));
    }

    #[test]
    fn generation_worker_native_logs_are_isolated_under_native_scope() {
        let root = tempfile::tempdir().unwrap();
        let path = generation_workers_native_log_dir(root.path()).unwrap();
        assert_eq!(path.parent().unwrap(), root.path().join("native"));
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("generation-workers-"));
        assert!(path.is_dir());
    }

    #[test]
    fn generation_worker_native_log_guard_marks_scope_closed_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let path = generation_workers_native_log_dir(root.path()).unwrap();
        {
            let _guard = NativeGenerationLogGuard(path.clone());
            assert!(path.join(".active").is_file());
        }
        assert!(!path.join(".active").exists());
        assert!(path.join(".closed").is_file());
    }
}
