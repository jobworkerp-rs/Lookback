use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use coding_agent_executor::{
    EXECUTOR_VERSION, Executor, ExecutorErrorCode, ExecutorRequest, ExecutorResult, Operation,
    PrepareRequest, RepositoryConfig, RepositoryId, ResolveBaseRequest, SourceInspection,
    TrustedConfig,
};
use tempfile::TempDir;
use url::Url;

struct Fixture {
    _temp: TempDir,
    home: PathBuf,
    source: PathBuf,
    config: TrustedConfig,
    repository: RepositoryId,
    initial_commit: String,
}

fn git(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin")
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run fixture git");
    assert!(
        output.status.success(),
        "fixture git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git_text(home: &Path, cwd: &Path, args: &[&str]) -> String {
    String::from_utf8(git(home, cwd, args).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let home = root.join("fixture-home");
    let remote = root.join("remote.git");
    let source = root.join("source");
    for directory in [&home, &remote, &source] {
        fs::create_dir(directory).unwrap();
    }
    git(&home, &remote, &["init", "--bare", "--initial-branch=main"]);
    git(&home, &source, &["init", "--initial-branch=main"]);
    git(
        &home,
        &source,
        &["config", "user.name", "Resolve Base Test"],
    );
    git(
        &home,
        &source,
        &["config", "user.email", "resolve-base@example.invalid"],
    );
    fs::write(source.join("base.txt"), "initial base\n").unwrap();
    git(&home, &source, &["add", "base.txt"]);
    git(&home, &source, &["commit", "-m", "initial base"]);
    let initial_commit = git_text(&home, &source, &["rev-parse", "HEAD"]);
    git(
        &home,
        &source,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&home, &source, &["push", "origin", "main"]);

    let workspace_root = root.join("workspaces");
    let publisher_root = root.join("publishers");
    let state_root = root.join("state");
    let credential_root = root.join("credentials");
    for directory in [
        &workspace_root,
        &publisher_root,
        &state_root,
        &credential_root,
    ] {
        fs::create_dir(directory).unwrap();
    }
    let repository = RepositoryId {
        host: "code.example".to_owned(),
        owner: "team".to_owned(),
        name: "sample".to_owned(),
    };
    let config = TrustedConfig {
        protocol_version: 2,
        expected_executor_version: EXECUTOR_VERSION.to_owned(),
        workspace_root,
        publisher_root,
        state_root,
        credential_root,
        max_import_bytes: 16 * 1024 * 1024,
        repositories: vec![RepositoryConfig {
            id: repository.clone(),
            clone_url: Url::from_file_path(&remote).unwrap().to_string(),
            ssh_directory: None,
            allowed_base_branches: vec!["main".to_owned()],
            read_credential_source: None,
            publish_credential_source: None,
            forge: None,
        }],
        credential_sources: Vec::new(),
    };
    Fixture {
        _temp: temp,
        home,
        source,
        config,
        repository,
        initial_commit,
    }
}

fn request(fixture: &Fixture, operation_id: &str) -> ResolveBaseRequest {
    ResolveBaseRequest {
        run_id: "interactive-run".to_owned(),
        attempt_id: "attempt-1".to_owned(),
        operation_id: operation_id.to_owned(),
        repository: fixture.repository.clone(),
        base_branch: "main".to_owned(),
    }
}

async fn prepare_for_source_inspection(fixture: &Fixture) -> (Executor, PrepareRequest, PathBuf) {
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let request = PrepareRequest {
        issue_id: Some("17".into()),
        run_id: "source-inspection".into(),
        attempt_id: "attempt-1".into(),
        repository: fixture.repository.clone(),
        base_branch: "main".into(),
        base_commit: fixture.initial_commit.clone(),
    };
    let result = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Prepare(request.clone()),
        })
        .await
        .unwrap();
    let ExecutorResult::Prepared(prepared) = result else {
        panic!("expected workspace");
    };
    (executor, request, PathBuf::from(prepared.workspace_path))
}

async fn inspect_source(executor: &Executor, request: &PrepareRequest) -> SourceInspection {
    let result = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::InspectSource(request.clone()),
        })
        .await
        .unwrap();
    let ExecutorResult::SourceInspected(inspection) = result else {
        panic!("expected source inspection");
    };
    inspection
}

#[tokio::test]
async fn source_inspection_includes_committed_uncommitted_and_new_files_without_mutation() {
    let fixture = fixture();
    let (executor, request, workspace) = prepare_for_source_inspection(&fixture).await;
    git(
        &fixture.home,
        &workspace,
        &["config", "user.name", "Inspection Test"],
    );
    git(
        &fixture.home,
        &workspace,
        &["config", "user.email", "inspection@example.invalid"],
    );
    fs::write(workspace.join("committed.txt"), "committed change\n").unwrap();
    git(&fixture.home, &workspace, &["add", "committed.txt"]);
    git(&fixture.home, &workspace, &["commit", "-m", "agent change"]);
    fs::remove_file(workspace.join("base.txt")).unwrap();
    fs::write(workspace.join("日本語.bin"), [0, 255, 10]).unwrap();
    fs::write(workspace.join("empty.txt"), []).unwrap();
    fs::create_dir(workspace.join(".coding-agent")).unwrap();
    fs::write(
        workspace.join(".coding-agent/result.json"),
        "private report",
    )
    .unwrap();
    let head = git_text(&fixture.home, &workspace, &["rev-parse", "HEAD"]);
    let status = git(
        &fixture.home,
        &workspace,
        &["status", "--porcelain=v1", "-z"],
    )
    .stdout;
    let index = fs::read(workspace.join(".git/index")).unwrap();
    let inspection = inspect_source(&executor, &request).await;
    assert_eq!(inspection.base_commit, fixture.initial_commit);
    assert_eq!(inspection.head_commit, head);
    assert_eq!(inspection.branch, request.expected_branch());
    let uncommitted = String::from_utf8(inspection.uncommitted_diff.unwrap()).unwrap();
    assert!(uncommitted.contains("deleted file mode"));
    assert!(!uncommitted.contains("committed change"));
    let diff = String::from_utf8(inspection.tracked_diff).unwrap();
    assert!(diff.contains("committed change"));
    assert!(diff.contains("deleted file mode"));
    assert_eq!(inspection.new_files.len(), 2);
    assert!(
        inspection
            .new_files
            .iter()
            .any(|file| file.path == "日本語.bin" && file.content == [0, 255, 10])
    );
    assert!(
        inspection
            .new_files
            .iter()
            .any(|file| file.path == "empty.txt" && file.content.is_empty())
    );
    assert_eq!(
        git_text(&fixture.home, &workspace, &["rev-parse", "HEAD"]),
        head
    );
    assert_eq!(
        git(
            &fixture.home,
            &workspace,
            &["status", "--porcelain=v1", "-z"]
        )
        .stdout,
        status
    );
    assert_eq!(fs::read(workspace.join(".git/index")).unwrap(), index);
}

#[tokio::test]
async fn source_inspection_rejects_different_base_branch_and_excessive_output() {
    let fixture = fixture();
    let (executor, request, workspace) = prepare_for_source_inspection(&fixture).await;
    let mut other_base = request.clone();
    other_base.base_commit = "a".repeat(40);
    assert_eq!(
        executor
            .execute(ExecutorRequest {
                protocol_version: 2,
                operation: Operation::InspectSource(other_base)
            })
            .await
            .unwrap_err()
            .code,
        ExecutorErrorCode::IdempotencyConflict
    );
    git(&fixture.home, &workspace, &["checkout", "-b", "unexpected"]);
    assert_eq!(
        executor
            .execute(ExecutorRequest {
                protocol_version: 2,
                operation: Operation::InspectSource(request.clone())
            })
            .await
            .unwrap_err()
            .code,
        ExecutorErrorCode::UnsafeWorkspace
    );
    git(
        &fixture.home,
        &workspace,
        &["checkout", &request.expected_branch()],
    );
    let mut limited = fixture.config.clone();
    limited.max_import_bytes = 1024;
    let limited = Executor::new(limited).unwrap();
    fs::write(workspace.join("large.bin"), vec![0; 1025]).unwrap();
    assert_eq!(
        limited
            .execute(ExecutorRequest {
                protocol_version: 2,
                operation: Operation::InspectSource(request)
            })
            .await
            .unwrap_err()
            .code,
        ExecutorErrorCode::ResponseTooLarge
    );
}

#[cfg(unix)]
#[tokio::test]
async fn source_inspection_ignores_repository_worktree_and_diff_program_overrides() {
    let fixture = fixture();
    let (executor, request, workspace) = prepare_for_source_inspection(&fixture).await;
    fs::write(workspace.join("base.txt"), "local change\n").unwrap();
    git(
        &fixture.home,
        &workspace,
        &["config", "core.worktree", fixture.source.to_str().unwrap()],
    );
    git(
        &fixture.home,
        &workspace,
        &["config", "diff.external", "/bin/false"],
    );
    let inspection = inspect_source(&executor, &request).await;
    assert!(
        String::from_utf8(inspection.tracked_diff)
            .unwrap()
            .contains("local change")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn source_inspection_includes_forced_staged_files_and_preserves_local_excludes() {
    let fixture = fixture();
    let (executor, request, workspace) = prepare_for_source_inspection(&fixture).await;
    fs::write(workspace.join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(workspace.join("ignored.txt"), "staged change\n").unwrap();
    git(&fixture.home, &workspace, &["add", "-f", "ignored.txt"]);
    fs::write(workspace.join(".git/info/exclude"), "local-cache.txt\n").unwrap();
    fs::write(workspace.join("local-cache.txt"), "not source\n").unwrap();
    let inspection = inspect_source(&executor, &request).await;
    assert!(
        String::from_utf8(inspection.tracked_diff)
            .unwrap()
            .contains("staged change")
    );
    assert!(
        !inspection
            .new_files
            .iter()
            .any(|file| file.path == "local-cache.txt")
    );
    git(
        &fixture.home,
        &workspace,
        &["update-index", "--split-index"],
    );
    let split = inspect_source(&executor, &request).await;
    assert!(
        String::from_utf8(split.tracked_diff)
            .unwrap()
            .contains("staged change")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn source_inspection_never_executes_repository_clean_or_process_filters() {
    let fixture = fixture();
    let (executor, request, workspace) = prepare_for_source_inspection(&fixture).await;
    let marker = fixture.home.join("filter-executed");
    fs::write(workspace.join("base.txt"), "local change\n").unwrap();
    fs::write(workspace.join(".gitattributes"), "base.txt filter=probe\n").unwrap();
    let clean = format!("cat; touch '{}'", marker.display());
    git(
        &fixture.home,
        &workspace,
        &["config", "filter.probe.clean", &clean],
    );
    let inspection = inspect_source(&executor, &request).await;
    assert!(
        String::from_utf8(inspection.tracked_diff)
            .unwrap()
            .contains("local change")
    );
    assert!(
        !marker.exists(),
        "repository clean filter must never execute"
    );
    git(
        &fixture.home,
        &workspace,
        &[
            "config",
            "filter.probe.process",
            &format!("touch '{}'", marker.display()),
        ],
    );
    inspect_source(&executor, &request).await;
    assert!(
        !marker.exists(),
        "repository process filter must never execute"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn source_inspection_reads_symlink_targets_without_reading_external_files() {
    let fixture = fixture();
    let (executor, request, workspace) = prepare_for_source_inspection(&fixture).await;
    let external = fixture.home.join("secret");
    fs::write(&external, "must not read this secret").unwrap();
    std::os::unix::fs::symlink(&external, workspace.join("link")).unwrap();
    let inspection = inspect_source(&executor, &request).await;
    let link = inspection
        .new_files
        .iter()
        .find(|file| file.path == "link")
        .unwrap();
    assert!(link.is_symlink);
    assert_eq!(link.content, external.to_string_lossy().as_bytes());
}

#[tokio::test]
async fn resolves_an_approved_base_once_and_prepare_uses_the_frozen_commit() {
    let fixture = fixture();
    let executor = Executor::new(fixture.config.clone()).unwrap();

    let first = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::ResolveBase(request(&fixture, "resolve-1")),
        })
        .await
        .unwrap();
    let ExecutorResult::BaseResolved(receipt) = first else {
        panic!("expected a resolved base receipt");
    };
    assert_eq!(receipt.base_commit, fixture.initial_commit);
    assert_eq!(receipt.base_branch, "main");
    assert_eq!(receipt.run_id, "interactive-run");
    assert_eq!(receipt.attempt_id, "attempt-1");
    assert_eq!(receipt.operation_id, "resolve-1");
    assert!(
        !fixture
            .config
            .workspace_root
            .join("interactive-run")
            .exists()
    );

    fs::write(fixture.source.join("base.txt"), "moved remote head\n").unwrap();
    git(&fixture.home, &fixture.source, &["add", "base.txt"]);
    git(
        &fixture.home,
        &fixture.source,
        &["commit", "-m", "move remote head"],
    );
    git(&fixture.home, &fixture.source, &["push", "origin", "main"]);
    let moved_commit = git_text(&fixture.home, &fixture.source, &["rev-parse", "HEAD"]);
    assert_ne!(moved_commit, receipt.base_commit);

    let replay = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::ResolveBase(request(&fixture, "resolve-1")),
        })
        .await
        .unwrap();
    let ExecutorResult::BaseResolved(replay) = replay else {
        panic!("expected the idempotent receipt");
    };
    assert_eq!(replay, receipt);

    let refreshed = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::ResolveBase(request(&fixture, "resolve-2")),
        })
        .await
        .unwrap();
    let ExecutorResult::BaseResolved(refreshed) = refreshed else {
        panic!("expected a refreshed base receipt");
    };
    assert_eq!(refreshed.base_commit, moved_commit);

    let prepared = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Prepare(PrepareRequest {
                issue_id: None,
                run_id: "interactive-run".to_owned(),
                attempt_id: "attempt-1".to_owned(),
                repository: fixture.repository.clone(),
                base_branch: receipt.base_branch,
                base_commit: receipt.base_commit.clone(),
            }),
        })
        .await
        .unwrap();
    let ExecutorResult::Prepared(prepared) = prepared else {
        panic!("expected a prepared workspace");
    };
    assert_eq!(prepared.base_commit, fixture.initial_commit);
    assert_eq!(
        git_text(
            &fixture.home,
            Path::new(&prepared.workspace_path),
            &["rev-parse", "HEAD"]
        ),
        fixture.initial_commit
    );
}

#[tokio::test]
async fn rejects_an_unapproved_branch_without_querying_the_remote() {
    let fixture = fixture();
    let executor = Executor::new(fixture.config).unwrap();

    let error = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::ResolveBase(ResolveBaseRequest {
                run_id: "interactive-run".to_owned(),
                attempt_id: "attempt-1".to_owned(),
                operation_id: "resolve-bad".to_owned(),
                repository: fixture.repository,
                base_branch: "--upload-pack=touch-pwned".to_owned(),
            }),
        })
        .await
        .unwrap_err();

    assert_eq!(error.code, ExecutorErrorCode::BaseBranchNotApproved);
    assert!(!fixture._temp.path().join("pwned").exists());
}

#[test]
fn trusted_configuration_rejects_a_cli_option_like_base_branch() {
    let fixture = fixture();
    let mut config = fixture.config;
    config.repositories[0].allowed_base_branches = vec!["--upload-pack".to_owned()];

    let error = Executor::new(config)
        .err()
        .expect("option-like configured refs are rejected");
    assert_eq!(error.code, ExecutorErrorCode::InvalidConfiguration);
}

#[tokio::test]
async fn rejects_a_configured_branch_without_a_remote_head_as_a_readonly_error() {
    let fixture = fixture();
    let mut config = fixture.config.clone();
    config.repositories[0].allowed_base_branches = vec!["not-yet-created".to_owned()];
    let executor = Executor::new(config).unwrap();

    let error = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::ResolveBase(ResolveBaseRequest {
                run_id: "interactive-run".to_owned(),
                attempt_id: "attempt-1".to_owned(),
                operation_id: "missing-branch".to_owned(),
                repository: fixture.repository,
                base_branch: "not-yet-created".to_owned(),
            }),
        })
        .await
        .unwrap_err();

    assert_eq!(error.code, ExecutorErrorCode::GitOperationFailed);
    assert!(
        !fixture
            .config
            .workspace_root
            .join("interactive-run")
            .exists()
    );
}

#[tokio::test]
async fn an_operation_id_cannot_be_reused_for_a_different_approved_branch() {
    let fixture = fixture();
    let mut config = fixture.config.clone();
    config.repositories[0]
        .allowed_base_branches
        .push("develop".to_owned());
    let executor = Executor::new(config).unwrap();

    let first = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::ResolveBase(request(&fixture, "resolve-fixed")),
        })
        .await
        .unwrap();
    assert!(matches!(first, ExecutorResult::BaseResolved(_)));

    let error = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::ResolveBase(ResolveBaseRequest {
                base_branch: "develop".to_owned(),
                ..request(&fixture, "resolve-fixed")
            }),
        })
        .await
        .unwrap_err();

    assert_eq!(error.code, ExecutorErrorCode::IdempotencyConflict);
}

#[tokio::test]
async fn issue_branch_is_normalized_and_bound_to_the_issue_execution() {
    let fixture = fixture();
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let request: PrepareRequest = serde_json::from_value(serde_json::json!({
        "run_id": "execution-42", "attempt_id": "attempt-1",
        "issue_id": "42/fix.. unsafe@{branch}",
        "repository": fixture.repository,
        "base_branch": "main", "base_commit": fixture.initial_commit,
    }))
    .unwrap();
    let result = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Prepare(request),
        })
        .await
        .unwrap();
    let ExecutorResult::Prepared(prepared) = result else {
        panic!("expected prepared workspace");
    };
    assert!(prepared.branch.starts_with("agent/issue-42-"));
    assert!(prepared.branch.ends_with("-execution-42"));
    let worktree = Path::new(&prepared.workspace_path);
    assert_eq!(
        git_text(
            &fixture.home,
            worktree,
            &["symbolic-ref", "--short", "HEAD"]
        ),
        prepared.branch
    );
    assert_eq!(
        git_text(&fixture.home, worktree, &["rev-parse", "HEAD"]),
        fixture.initial_commit
    );
}

#[tokio::test]
async fn repository_standard_branch_is_resolved_from_remote_head_and_its_latest_commit() {
    let mut fixture = fixture();
    let remote = fixture._temp.path().join("remote.git");
    git(&fixture.home, &fixture.source, &["checkout", "-b", "trunk"]);
    fs::write(fixture.source.join("base.txt"), "latest trunk\n").unwrap();
    git(
        &fixture.home,
        &fixture.source,
        &["commit", "-am", "new default branch"],
    );
    git(&fixture.home, &fixture.source, &["push", "origin", "trunk"]);
    git(
        &fixture.home,
        &remote,
        &["symbolic-ref", "HEAD", "refs/heads/trunk"],
    );
    fixture.config.repositories[0]
        .allowed_base_branches
        .push("trunk".into());
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let mut requested = request(&fixture, "resolve-default");
    requested.base_branch.clear();
    let ExecutorResult::BaseResolved(resolved) = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::ResolveBase(requested.clone()),
        })
        .await
        .unwrap()
    else {
        panic!("expected remote HEAD");
    };
    assert_eq!(resolved.base_branch, "trunk");
    assert_eq!(
        resolved.base_commit,
        git_text(&fixture.home, &fixture.source, &["rev-parse", "HEAD"])
    );
    assert_ne!(resolved.base_commit, fixture.initial_commit);
    let ExecutorResult::Prepared(prepared) = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Prepare(PrepareRequest {
                issue_id: Some("42".into()),
                run_id: requested.run_id,
                attempt_id: requested.attempt_id,
                repository: fixture.repository.clone(),
                base_branch: resolved.base_branch,
                base_commit: resolved.base_commit.clone(),
            }),
        })
        .await
        .unwrap()
    else {
        panic!("expected workspace");
    };
    assert_eq!(
        git_text(
            &fixture.home,
            Path::new(&prepared.workspace_path),
            &["rev-parse", "HEAD"]
        ),
        resolved.base_commit
    );
    assert!(prepared.branch.starts_with("agent/issue-42-"));
}

#[tokio::test]
async fn remote_head_cannot_bypass_approved_branch_policy() {
    let fixture = fixture();
    let remote = fixture._temp.path().join("remote.git");
    git(
        &fixture.home,
        &fixture.source,
        &["push", "origin", "main:restricted"],
    );
    git(
        &fixture.home,
        &remote,
        &["symbolic-ref", "HEAD", "refs/heads/restricted"],
    );
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let mut requested = request(&fixture, "resolve-default");
    requested.base_branch.clear();
    let error = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::ResolveBase(requested),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::BaseBranchNotApproved);
    assert!(
        !fixture
            .config
            .workspace_root
            .join("interactive-run")
            .exists()
    );
}

#[tokio::test]
async fn source_inspection_distinguishes_a_clean_commit_from_the_approved_base() {
    let fixture = fixture();
    let (executor, request, workspace) = prepare_for_source_inspection(&fixture).await;
    git(
        &fixture.home,
        &workspace,
        &["config", "user.name", "Inspection Test"],
    );
    git(
        &fixture.home,
        &workspace,
        &["config", "user.email", "inspection@example.invalid"],
    );
    fs::write(workspace.join("base.txt"), "changed and committed\n").unwrap();
    git(&fixture.home, &workspace, &["add", "base.txt"]);
    git(
        &fixture.home,
        &workspace,
        &["commit", "-m", "source change"],
    );
    let inspection = inspect_source(&executor, &request).await;
    assert!(!inspection.tracked_diff.is_empty());
    assert_eq!(inspection.uncommitted_diff, Some(Vec::new()));
    assert!(inspection.new_files.is_empty());
    assert_ne!(inspection.head_commit, inspection.base_commit);
}

#[tokio::test]
async fn source_inspection_detects_worktree_changes_hidden_by_guest_index_skip_flags() {
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        let fixture = fixture();
        let (executor, request, workspace) = prepare_for_source_inspection(&fixture).await;
        git(
            &fixture.home,
            &workspace,
            &["update-index", flag, "base.txt"],
        );
        fs::write(workspace.join("base.txt"), "hidden local change\n").unwrap();
        assert!(
            git(
                &fixture.home,
                &workspace,
                &["diff", "HEAD", "--", "base.txt"]
            )
            .stdout
            .is_empty()
        );
        let original_index = fs::read(workspace.join(".git/index")).unwrap();
        let inspection = inspect_source(&executor, &request).await;
        assert!(
            String::from_utf8(inspection.tracked_diff)
                .unwrap()
                .contains("hidden local change")
        );
        assert!(
            String::from_utf8(inspection.uncommitted_diff.unwrap())
                .unwrap()
                .contains("hidden local change")
        );
        assert_eq!(
            fs::read(workspace.join(".git/index")).unwrap(),
            original_index
        );
    }
}
