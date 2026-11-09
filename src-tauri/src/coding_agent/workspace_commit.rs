use super::one_shot_workflow::{
    OneShotWorkflowError, OneShotWorkflowExecution, OneShotWorkflowResult,
};
use crate::jobworkerp::{JobworkerpHandle, coding_agent::NativeWorkerMatch};
use serde_json::json;

#[derive(Debug, Clone, serde::Serialize)]
pub struct LocalCommitStart {
    pub parent_job_id: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LocalCommitObservation {
    pub workflow_result: OneShotWorkflowResult,
    pub source: coding_agent_executor::SourceInspection,
}

pub async fn commit_workspace(
    handle: &JobworkerpHandle,
    matched: &NativeWorkerMatch,
    cwd: &str,
    branch: &str,
    message: &str,
) -> Result<OneShotWorkflowResult, OneShotWorkflowError> {
    start_commit_workspace(handle, matched, cwd, branch, message)
        .await?
        .finish()
        .await
}

pub(crate) async fn start_commit_workspace(
    handle: &JobworkerpHandle,
    matched: &NativeWorkerMatch,
    cwd: &str,
    branch: &str,
    message: &str,
) -> Result<OneShotWorkflowExecution, OneShotWorkflowError> {
    if matched.using() != "run"
        || !cwd.starts_with(&format!("{}/", matched.scope().guest_root()))
        || !branch.starts_with("agent/issue-")
        || branch.chars().any(char::is_control)
        || message.is_empty()
        || message.len() > 4096
        || message.contains('\0')
    {
        return Err(OneShotWorkflowError::InvalidRequest);
    }
    handle
        .revalidate_native_version_probe_match(matched)
        .await
        .map_err(|_| OneShotWorkflowError::WorkerChanged)?;
    let workflow = json!({
        "document":{"dsl":"1.0.0-jobworkerp","namespace":"lookback","name":"coding-agent-local-commit","version":"1.0.0"},
        "do":[{"commitWorkspace":{"timeout":{"after":"PT125S"},"run":{"function":{
            "workerName":matched.worker_name(),"using":"run","arguments":{
                "command":"sh","args":["-c",commit_script(),"coding-agent-commit",
                    "${ $workflow.input.branch }","${ $workflow.input.message }"],
                "working_dir":"${ $workflow.input.cwd }","timeout_ms":120000,
                "tty":false,"treat_nonzero_as_error":false,
            }
        }}}}],"output":{"as":"${ . }"}
    });
    let stream = super::preparation_workflow::start_workflow(
        handle,
        workflow,
        json!({"cwd":cwd,"branch":branch,"message":message}),
        "local-commit",
        120,
    )
    .await
    .map_err(|_| OneShotWorkflowError::OutcomeUnknown)?;
    OneShotWorkflowExecution::from_stream(stream, 120)
}

fn commit_script() -> &'static str {
    r#"set -eu
git() { command git --git-dir="$PWD/.git" --work-tree="$PWD" -c core.worktree="$PWD" "$@"; }
current=$(git symbolic-ref --quiet --short HEAD)
[ "$current" = "$1" ] || exit 3
case "$1" in agent/issue-*) ;; *) exit 3 ;; esac
git -c core.fsmonitor=false ls-files -z | git -c core.fsmonitor=false update-index --no-assume-unchanged --no-skip-worktree -z --stdin
git -c core.hooksPath=/dev/null -c core.fsmonitor=false add -A -- . ':(exclude).coding-agent' ':(exclude).coding-agent/**'
git -c core.fsmonitor=false reset --quiet HEAD -- .coding-agent
status=0
git -c core.fsmonitor=false diff --cached --quiet --exit-code || status=$?
case "$status" in
0) exit 0 ;;
1) git -c core.hooksPath=/dev/null -c core.fsmonitor=false -c commit.gpgSign=false commit -m "$2" ;;
*) exit "$status" ;;
esac
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, process::Command};
    fn git(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    fn workspace() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        git(
            root.path(),
            &["init", "--initial-branch=agent/issue-42-run"],
        );
        git(root.path(), &["config", "user.name", "Test User"]);
        git(
            root.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        std::fs::write(root.path().join("tracked"), "base").unwrap();
        git(root.path(), &["add", "tracked"]);
        git(root.path(), &["commit", "-m", "base"]);
        root
    }
    fn run(root: &Path, branch: &str) -> std::process::Output {
        Command::new("sh")
            .args([
                "-c",
                commit_script(),
                "coding-agent-commit",
                branch,
                r#"Issue 日本語 $(never) ' " result"#,
            ])
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap()
    }
    #[test]
    fn commits_actual_changes_without_reports_or_empty_commits() {
        let root = workspace();
        let original = git(root.path(), &["rev-parse", "HEAD"]);
        assert!(run(root.path(), "agent/issue-42-run").status.success());
        assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), original);
        std::fs::remove_file(root.path().join("tracked")).unwrap();
        std::fs::write(root.path().join("new-file"), "actual change").unwrap();
        std::fs::create_dir(root.path().join(".coding-agent")).unwrap();
        std::fs::write(root.path().join(".coding-agent/report.json"), "report").unwrap();
        git(root.path(), &["add", "-f", ".coding-agent/report.json"]);
        assert!(run(root.path(), "agent/issue-42-run").status.success());
        assert_ne!(git(root.path(), &["rev-parse", "HEAD"]), original);
        assert_eq!(
            git(root.path(), &["ls-tree", "--name-only", "HEAD"]),
            "new-file"
        );
        assert_eq!(
            git(root.path(), &["log", "-1", "--format=%s"]),
            r#"Issue 日本語 $(never) ' " result"#
        );
        let committed = git(root.path(), &["rev-parse", "HEAD"]);
        assert!(run(root.path(), "agent/issue-42-run").status.success());
        assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), committed);
    }
    #[test]
    fn repository_config_cannot_redirect_the_committed_worktree() {
        let root = workspace();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("tracked"), "approved source").unwrap();
        std::fs::write(other.path().join("tracked"), "another source").unwrap();
        git(
            root.path(),
            &["config", "core.worktree", other.path().to_str().unwrap()],
        );
        assert!(run(root.path(), "agent/issue-42-run").status.success());
        assert_eq!(
            git(root.path(), &["show", "HEAD:tracked"]),
            "approved source"
        );
    }

    #[test]
    fn wrong_branch_and_missing_author_do_not_commit_or_report_success() {
        let root = workspace();
        let original = git(root.path(), &["rev-parse", "HEAD"]);
        std::fs::write(root.path().join("tracked"), "changed").unwrap();
        assert!(!run(root.path(), "agent/issue-other-run").status.success());
        assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), original);
        git(root.path(), &["config", "--unset", "user.name"]);
        git(root.path(), &["config", "--unset", "user.email"]);
        let output = Command::new("sh")
            .args([
                "-c",
                commit_script(),
                "commit",
                "agent/issue-42-run",
                "result",
            ])
            .current_dir(root.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), original);
    }
}
