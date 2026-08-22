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
    jw_port: u16,
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
        .env("JOBWORKERP_ADDR", format!("http://127.0.0.1:{jw_port}"))
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

/// Resolve the binary + repo root and run the upsert, converting any failure
/// into a `WorkerApplyFailed` warning instead of propagating it (fail-soft).
pub(crate) async fn register_generation_workers(
    jw_port: u16,
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
        jw_port,
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
            let stderr = String::from_utf8_lossy(&out.stderr);
            // Keep the tail — the actionable cause (e.g. an empty prompt file
            // failing `read_non_empty`) is usually the last line.
            let lines: Vec<&str> = stderr.lines().collect();
            let summary = lines[lines.len().saturating_sub(5)..].join("\n");
            tracing::warn!(
                status = ?out.status.code(),
                stderr = %stderr,
                "upsert-generation-workers failed (continuing)",
            );
            warnings.push(SidecarWarning {
                kind: SidecarWarningKind::WorkerApplyFailed,
                message: format!(
                    "registering language generation workers failed (exit {:?}): {}",
                    out.status.code(),
                    summary
                ),
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
            9123,
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
            9000,
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
            9042,
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
