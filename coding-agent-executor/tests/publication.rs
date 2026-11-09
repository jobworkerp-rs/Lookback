use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use coding_agent_executor::{
    CaptureRequest, CreatePullRequestRequest, EXECUTOR_VERSION, Executor, ExecutorErrorCode,
    ExecutorRequest, ExecutorResult, Operation, PrepareRequest, PullRequestRequest, PushRequest,
    RepositoryConfig, RepositoryId, ResolveRequest, ResolvedArtifact, SealRequest,
    SealedArtifactReceipt, TrustedConfig,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use url::Url;

struct Fixture {
    temp: TempDir,
    config: TrustedConfig,
    repo: RepositoryId,
    base_commit: String,
    remote: PathBuf,
    credential: String,
}

fn git(home: &Path, dir: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin")
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(dir)
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

fn fixture(forge: Option<(coding_agent_executor::ForgeKind, String, String)>) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("fixture-home");
    fs::create_dir(&home).unwrap();
    let remote = temp.path().join("remote.git");
    fs::create_dir(&remote).unwrap();
    git(&home, &remote, &["init", "--bare", "--initial-branch=main"]);
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    git(&home, &source, &["init", "--initial-branch=main"]);
    git(&home, &source, &["config", "user.name", "Executor Fixture"]);
    git(
        &home,
        &source,
        &["config", "user.email", "executor-fixture@example.invalid"],
    );
    fs::write(source.join("README.md"), "base\n").unwrap();
    git(&home, &source, &["add", "--all"]);
    git(&home, &source, &["commit", "-m", "base"]);
    let base_commit = String::from_utf8(git(&home, &source, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(
        &home,
        &source,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&home, &source, &["push", "origin", "main"]);

    let roots: Vec<_> = ["workspaces", "publishers", "state", "credentials"]
        .into_iter()
        .map(|name| {
            let path = temp.path().join(name);
            fs::create_dir(&path).unwrap();
            path
        })
        .collect();
    let credential = "fixture-publish-secret-987654321".to_owned();
    let token_path = roots[3].join("publish.token");
    fs::write(&token_path, format!("{credential}\n")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let repo = RepositoryId {
        host: "forge.local".to_owned(),
        owner: "owner".to_owned(),
        name: "repo".to_owned(),
    };
    let forge =
        forge.map(
            |(kind, api_base_url, web_base_url)| coding_agent_executor::ForgeConfig {
                kind,
                api_base_url,
                web_base_url,
            },
        );
    let config = TrustedConfig {
        protocol_version: 2,
        expected_executor_version: EXECUTOR_VERSION.to_owned(),
        workspace_root: roots[0].clone(),
        publisher_root: roots[1].clone(),
        state_root: roots[2].clone(),
        credential_root: roots[3].clone(),
        max_import_bytes: 16 * 1024 * 1024,
        repositories: vec![RepositoryConfig {
            id: repo.clone(),
            clone_url: Url::from_file_path(&remote).unwrap().to_string(),
            ssh_directory: None,
            allowed_base_branches: vec!["main".to_owned()],
            read_credential_source: None,
            publish_credential_source: Some("publish".to_owned()),
            forge,
        }],
        credential_sources: vec![coding_agent_executor::CredentialSourceConfig {
            id: "publish".to_owned(),
            username: "fixture".to_owned(),
            token_file: PathBuf::from("publish.token"),
        }],
    };
    Fixture {
        temp,
        config,
        repo,
        base_commit,
        remote,
        credential,
    }
}

async fn commit_workspace(fixture: &Fixture, run_id: &str, file: &str, contents: &str) -> String {
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let result = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Prepare(PrepareRequest {
                issue_id: None,
                run_id: run_id.to_owned(),
                attempt_id: run_id.to_owned(),
                repository: fixture.repo.clone(),
                base_branch: "main".to_owned(),
                base_commit: fixture.base_commit.clone(),
            }),
        })
        .await
        .unwrap();
    let ExecutorResult::Prepared(prepared) = result else {
        panic!("expected prepared workspace")
    };
    let workspace = PathBuf::from(prepared.workspace_path);
    fs::write(workspace.join(file), contents).unwrap();
    let home = fixture.temp.path().join("fixture-home");
    git(&home, &workspace, &["add", "--all"]);
    git(
        &home,
        &workspace,
        &[
            "-c",
            "user.name=Executor Fixture",
            "-c",
            "user.email=executor-fixture@example.invalid",
            "commit",
            "-m",
            "agent result",
        ],
    );
    seal_current_workspace(fixture, run_id).await.commit
}

async fn seal_current_workspace(fixture: &Fixture, run_id: &str) -> ResolvedArtifact {
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let capture = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Capture(CaptureRequest {
                run_id: run_id.to_owned(),
                attempt_id: run_id.to_owned(),
                repository: fixture.repo.clone(),
                base_branch: "main".to_owned(),
                base_commit: fixture.base_commit.clone(),
                approved_snapshot_digest: "a".repeat(64),
                scope_digest: "b".repeat(64),
                intent_approval_digest: "c".repeat(64),
            }),
        })
        .await
        .unwrap();
    let ExecutorResult::Captured(capture) = capture else {
        panic!("expected captured artifact")
    };
    let sealed = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Seal(SealRequest {
                run_id: run_id.to_owned(),
                attempt_id: run_id.to_owned(),
                manifest_digest: capture.manifest_digest,
            }),
        })
        .await
        .unwrap();
    let ExecutorResult::Sealed(sealed) = sealed else {
        panic!("expected sealed artifact")
    };
    let resolved = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Resolve(ResolveRequest {
                run_id: run_id.to_owned(),
                attempt_id: run_id.to_owned(),
                artifact_digest: sealed.artifact_digest,
            }),
        })
        .await
        .unwrap();
    let ExecutorResult::Resolved(resolved) = resolved else {
        panic!("expected resolved artifact")
    };
    resolved
}

async fn prepare_attempt(fixture: &Fixture, run_id: &str, attempt_id: &str) -> PathBuf {
    let result = Executor::new(fixture.config.clone())
        .unwrap()
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Prepare(PrepareRequest {
                issue_id: None,
                run_id: run_id.to_owned(),
                attempt_id: attempt_id.to_owned(),
                repository: fixture.repo.clone(),
                base_branch: "main".to_owned(),
                base_commit: fixture.base_commit.clone(),
            }),
        })
        .await
        .unwrap();
    let ExecutorResult::Prepared(prepared) = result else {
        panic!("expected prepared workspace")
    };
    PathBuf::from(prepared.workspace_path)
}

async fn capture_and_seal_attempt(
    fixture: &Fixture,
    run_id: &str,
    attempt_id: &str,
) -> (coding_agent_executor::CaptureReceipt, SealedArtifactReceipt) {
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let captured = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Capture(CaptureRequest {
                run_id: run_id.to_owned(),
                attempt_id: attempt_id.to_owned(),
                repository: fixture.repo.clone(),
                base_branch: "main".to_owned(),
                base_commit: fixture.base_commit.clone(),
                approved_snapshot_digest: "a".repeat(64),
                scope_digest: "b".repeat(64),
                intent_approval_digest: "c".repeat(64),
            }),
        })
        .await
        .unwrap();
    let ExecutorResult::Captured(captured) = captured else {
        panic!("expected capture")
    };
    let sealed = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Seal(SealRequest {
                run_id: run_id.to_owned(),
                attempt_id: attempt_id.to_owned(),
                manifest_digest: captured.manifest_digest.clone(),
            }),
        })
        .await
        .unwrap();
    let ExecutorResult::Sealed(sealed) = sealed else {
        panic!("expected seal")
    };
    (captured, sealed)
}

fn sealed_artifact_digest(fixture: &Fixture, run_id: &str) -> String {
    let path = fixture
        .config
        .state_root
        .join("runs")
        .join(run_id)
        .join("attempts")
        .join(run_id)
        .join("seal.json");
    let receipt: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    receipt["receipt"]["artifact_digest"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn push_request(fixture: &Fixture, run_id: &str, commit: &str) -> PushRequest {
    push_request_for_attempt(
        fixture,
        run_id,
        run_id,
        &sealed_artifact_digest(fixture, run_id),
        commit,
    )
}

fn push_request_for_attempt(
    fixture: &Fixture,
    run_id: &str,
    attempt_id: &str,
    artifact_digest: &str,
    commit: &str,
) -> PushRequest {
    PushRequest {
        run_id: run_id.to_owned(),
        attempt_id: attempt_id.to_owned(),
        repository: fixture.repo.clone(),
        branch: format!("agent/{run_id}"),
        base_branch: "main".to_owned(),
        base_commit: fixture.base_commit.clone(),
        artifact_digest: artifact_digest.to_owned(),
        frozen_commit: commit.to_owned(),
    }
}

async fn push(
    fixture: &Fixture,
    run_id: &str,
    commit: &str,
) -> coding_agent_executor::ExecutorResult {
    Executor::new(fixture.config.clone())
        .unwrap()
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Push(push_request(fixture, run_id, commit)),
        })
        .await
        .unwrap()
}

fn cli_request(fixture: &Fixture, request: &ExecutorRequest) -> Value {
    let config_path = fixture.temp.path().join("trusted-executor.json");
    fs::write(&config_path, serde_json::to_vec(&fixture.config).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_coding-agent-executor"))
        .args(["--config"])
        .arg(&config_path)
        .arg("--stdio")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin")
        .env("HOME", fixture.temp.path().join("fixture-home"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start public executor CLI");
    let mut input = serde_json::to_vec(request).unwrap();
    input.push(b'\n');
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&input)
        .expect("send request to public executor CLI");
    let output = child.wait_with_output().expect("wait for executor CLI");
    assert!(
        output.status.success(),
        "executor CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("strict JSON response from executor CLI")
}

fn prepare_workspace_via_cli(fixture: &Fixture, run_id: &str) -> PathBuf {
    let response = cli_request(
        fixture,
        &ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Prepare(PrepareRequest {
                issue_id: None,
                run_id: run_id.to_owned(),
                attempt_id: run_id.to_owned(),
                repository: fixture.repo.clone(),
                base_branch: "main".to_owned(),
                base_commit: fixture.base_commit.clone(),
            }),
        },
    );
    assert_eq!(response["status"], "ok", "{response}");
    PathBuf::from(
        response["result"]["prepared"]["workspace_path"]
            .as_str()
            .expect("prepared workspace path"),
    )
}

fn commit_empty_workspace_via_cli(fixture: &Fixture, run_id: &str) -> String {
    let workspace = prepare_workspace_via_cli(fixture, run_id);
    let home = fixture.temp.path().join("fixture-home");
    git(
        &home,
        &workspace,
        &[
            "-c",
            "user.name=Executor Fixture",
            "-c",
            "user.email=executor-fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "empty result",
        ],
    );
    seal_current_workspace_via_cli(fixture, run_id).commit
}

fn commit_workspace_via_cli(fixture: &Fixture, run_id: &str, file: &str, contents: &str) -> String {
    let workspace = prepare_workspace_via_cli(fixture, run_id);
    fs::write(workspace.join(file), contents).unwrap();
    let home = fixture.temp.path().join("fixture-home");
    git(&home, &workspace, &["add", "--all"]);
    git(
        &home,
        &workspace,
        &[
            "-c",
            "user.name=Executor Fixture",
            "-c",
            "user.email=executor-fixture@example.invalid",
            "commit",
            "-m",
            "agent result",
        ],
    );
    seal_current_workspace_via_cli(fixture, run_id).commit
}

fn commit_reverted_workspace_via_cli(fixture: &Fixture, run_id: &str) -> String {
    let workspace = prepare_workspace_via_cli(fixture, run_id);
    let home = fixture.temp.path().join("fixture-home");
    fs::write(workspace.join("temporary.txt"), "temporary change\n").unwrap();
    git(&home, &workspace, &["add", "--all"]);
    git(
        &home,
        &workspace,
        &[
            "-c",
            "user.name=Executor Fixture",
            "-c",
            "user.email=executor-fixture@example.invalid",
            "commit",
            "-m",
            "temporary change",
        ],
    );
    fs::remove_file(workspace.join("temporary.txt")).unwrap();
    git(&home, &workspace, &["add", "--all"]);
    git(
        &home,
        &workspace,
        &[
            "-c",
            "user.name=Executor Fixture",
            "-c",
            "user.email=executor-fixture@example.invalid",
            "commit",
            "-m",
            "revert tree",
        ],
    );
    seal_current_workspace_via_cli(fixture, run_id).commit
}

fn seal_current_workspace_via_cli(fixture: &Fixture, run_id: &str) -> ResolvedArtifact {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(seal_current_workspace(fixture, run_id))
}

fn push_via_cli(fixture: &Fixture, run_id: &str, commit: &str) -> Value {
    cli_request(
        fixture,
        &ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Push(push_request(fixture, run_id, commit)),
        },
    )
}

fn create_pr_via_cli(
    fixture: &Fixture,
    run_id: &str,
    commit: &str,
    title: &str,
    body: &str,
) -> Value {
    cli_request(
        fixture,
        &ExecutorRequest {
            protocol_version: 2,
            operation: Operation::CreatePullRequest(CreatePullRequestRequest {
                run_id: run_id.to_owned(),
                attempt_id: run_id.to_owned(),
                repository: fixture.repo.clone(),
                branch: format!("agent/{run_id}"),
                base_branch: "main".to_owned(),
                base_commit: fixture.base_commit.clone(),
                artifact_digest: sealed_artifact_digest(fixture, run_id),
                frozen_commit: commit.to_owned(),
                pull_request: PullRequestRequest {
                    title: title.to_owned(),
                    body: body.to_owned(),
                },
            }),
        },
    )
}

fn remote_branch_tip(fixture: &Fixture, branch: &str) -> String {
    let output = Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin")
        .env("HOME", fixture.temp.path().join("fixture-home"))
        .args([
            "--git-dir",
            fixture.remote.to_str().unwrap(),
            "for-each-ref",
            "--format=%(objectname)",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .expect("inspect local bare fixture refs");
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[cfg(unix)]
fn update_fixture_base_with_unsupported_entry(fixture: &mut Fixture, kind: &str) -> String {
    let home = fixture.temp.path().join("fixture-home");
    let source = fixture.temp.path().join("source");
    match kind {
        "symlink" => {
            fs::remove_file(source.join("README.md")).unwrap();
            std::os::unix::fs::symlink("README.md", source.join("linked-readme")).unwrap();
            git(&home, &source, &["add", "--all"]);
        }
        "gitlink" => {
            let entry = format!("160000,{},vendor/submodule", fixture.base_commit);
            git(
                &home,
                &source,
                &["update-index", "--add", "--cacheinfo", &entry],
            );
        }
        other => panic!("unsupported test base entry: {other}"),
    }
    git(
        &home,
        &source,
        &[
            "-c",
            "user.name=Executor Fixture",
            "-c",
            "user.email=executor-fixture@example.invalid",
            "commit",
            "-m",
            "unsupported base entry",
        ],
    );
    let commit = String::from_utf8(git(&home, &source, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(&home, &source, &["push", "origin", "main"]);
    fixture.base_commit = commit.clone();
    commit
}

#[cfg(unix)]
fn prepare_request(fixture: &Fixture, run_id: &str) -> ExecutorRequest {
    ExecutorRequest {
        protocol_version: 2,
        operation: Operation::Prepare(PrepareRequest {
            issue_id: None,
            run_id: run_id.to_owned(),
            attempt_id: run_id.to_owned(),
            repository: fixture.repo.clone(),
            base_branch: "main".to_owned(),
            base_commit: fixture.base_commit.clone(),
        }),
    }
}

#[tokio::test]
async fn secret_in_frozen_commit_blocks_push_without_echoing_secret() {
    let fixture = fixture(None);
    let commit = commit_workspace(&fixture, "secret-run", "leaked.txt", &fixture.credential).await;
    let error = Executor::new(fixture.config.clone())
        .unwrap()
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Push(push_request(&fixture, "secret-run", &commit)),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::SecretDetected);
    assert!(!error.to_string().contains(&fixture.credential));
    let listing = git(
        &fixture.temp.path().join("fixture-home"),
        fixture.temp.path(),
        &[
            "--git-dir",
            fixture.remote.to_str().unwrap(),
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads/agent/secret-run",
        ],
    );
    assert!(
        listing.stdout.is_empty(),
        "secret-bearing branch was pushed"
    );
}

#[tokio::test]
async fn non_fast_forward_run_branch_conflict_is_not_force_pushed() {
    let fixture = fixture(None);
    let commit = commit_workspace(&fixture, "conflict-run", "candidate.txt", "candidate\n").await;

    let home = fixture.temp.path().join("fixture-home");
    let source = fixture.temp.path().join("source");
    git(
        &home,
        &source,
        &["checkout", "-b", "agent/conflict-run", "main"],
    );
    fs::write(source.join("other.txt"), "remote conflict\n").unwrap();
    git(&home, &source, &["add", "other.txt"]);
    git(
        &home,
        &source,
        &[
            "-c",
            "user.name=Executor Fixture",
            "-c",
            "user.email=executor-fixture@example.invalid",
            "commit",
            "-m",
            "conflicting remote head",
        ],
    );
    git(
        &home,
        &source,
        &["push", "origin", "HEAD:refs/heads/agent/conflict-run"],
    );
    let before = git(
        &home,
        fixture.temp.path(),
        &[
            "--git-dir",
            fixture.remote.to_str().unwrap(),
            "rev-parse",
            "refs/heads/agent/conflict-run",
        ],
    );
    let error = Executor::new(fixture.config.clone())
        .unwrap()
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Push(push_request(&fixture, "conflict-run", &commit)),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::RemoteBranchConflict);
    let after = git(
        &home,
        fixture.temp.path(),
        &[
            "--git-dir",
            fixture.remote.to_str().unwrap(),
            "rev-parse",
            "refs/heads/agent/conflict-run",
        ],
    );
    assert_eq!(before.stdout, after.stdout);
}

#[tokio::test]
async fn push_is_idempotent_and_pr_creation_uses_typed_mock_api() {
    let kind = coding_agent_executor::ForgeKind::Gitea;
    let (api_url, web_url, state, server) = mock_forge(kind, 0, false, false, false);
    let fixture = fixture(Some((kind, api_url, web_url.clone())));
    let commit = commit_workspace(&fixture, "review-run", "change.txt", "safe\n").await;
    state.lock().unwrap().head_commit = commit.clone();
    state.lock().unwrap().base_commit = fixture.base_commit.clone();
    state.lock().unwrap().web_url = web_url.clone();
    let first = push(&fixture, "review-run", &commit).await;
    let second = push(&fixture, "review-run", &commit).await;
    assert_eq!(first, second);
    assert!(matches!(first, ExecutorResult::Pushed(_)));

    let request = ExecutorRequest {
        protocol_version: 2,
        operation: Operation::CreatePullRequest(CreatePullRequestRequest {
            run_id: "review-run".to_owned(),
            attempt_id: "review-run".to_owned(),
            repository: fixture.repo.clone(),
            branch: "agent/review-run".to_owned(),
            base_branch: "main".to_owned(),
            base_commit: fixture.base_commit.clone(),
            artifact_digest: sealed_artifact_digest(&fixture, "review-run"),
            frozen_commit: commit.clone(),
            pull_request: PullRequestRequest {
                title: "Implement safe change".to_owned(),
                body: "Reviewed and verified.".to_owned(),
            },
        }),
    };
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let pr1 = executor.execute(request.clone()).await.unwrap();
    let pr2 = executor.execute(request).await.unwrap();
    assert_eq!(pr1, pr2);
    match pr1 {
        ExecutorResult::PullRequestCreated(ref pr) => {
            assert_eq!(pr.number, 17);
            assert_eq!(pr.head_commit, commit);
            assert_eq!(pr.url, format!("{web_url}/owner/repo/pulls/17"));
        }
        other => panic!("unexpected result: {other:?}"),
    }
    server.join().unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state.posts, 1, "retry must not create a duplicate PR");
    assert!(state.authorization_seen);
    assert!(
        !serde_json::to_string(&pr1)
            .unwrap()
            .contains(&fixture.credential)
    );
}

#[tokio::test]
async fn lost_pr_response_is_reconciled_by_run_marker_without_duplicate_create() {
    let kind = coding_agent_executor::ForgeKind::Gitea;
    let (api_url, web_url, state, server) = mock_forge(kind, 0, true, false, false);
    let fixture = fixture(Some((kind, api_url, web_url.clone())));
    let commit = commit_workspace(&fixture, "lost-response", "change.txt", "safe\n").await;
    state.lock().unwrap().head_commit = commit.clone();
    state.lock().unwrap().base_commit = fixture.base_commit.clone();
    state.lock().unwrap().web_url = web_url;
    push(&fixture, "lost-response", &commit).await;
    let request = ExecutorRequest {
        protocol_version: 2,
        operation: Operation::CreatePullRequest(CreatePullRequestRequest {
            run_id: "lost-response".to_owned(),
            attempt_id: "lost-response".to_owned(),
            repository: fixture.repo.clone(),
            branch: "agent/lost-response".to_owned(),
            base_branch: "main".to_owned(),
            base_commit: fixture.base_commit.clone(),
            artifact_digest: sealed_artifact_digest(&fixture, "lost-response"),
            frozen_commit: commit,
            pull_request: PullRequestRequest {
                title: "Lost response".to_owned(),
                body: "Retry safely.".to_owned(),
            },
        }),
    };
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let error = executor.execute(request.clone()).await.unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::ForgeUnavailable);
    let result = executor.execute(request).await.unwrap();
    assert!(matches!(result, ExecutorResult::PullRequestCreated(_)));
    server.join().unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state.posts, 1);
}

#[test]
fn gitea_retry_finds_lost_response_on_third_page_using_base_branch_filter() {
    let kind = coding_agent_executor::ForgeKind::Gitea;
    let (api_url, web_url, state, server) = mock_forge(kind, 2, true, false, false);
    let fixture = fixture(Some((kind, api_url, web_url.clone())));
    let commit = commit_workspace_via_cli(&fixture, "gitea-paged", "change.txt", "safe\n");
    {
        let mut state = state.lock().unwrap();
        state.head_commit = commit.clone();
        state.base_commit = fixture.base_commit.clone();
        state.web_url = web_url;
    }
    let pushed = push_via_cli(&fixture, "gitea-paged", &commit);
    assert_eq!(pushed["status"], "ok", "{pushed}");
    let request = ExecutorRequest {
        protocol_version: 2,
        operation: Operation::CreatePullRequest(CreatePullRequestRequest {
            run_id: "gitea-paged".to_owned(),
            attempt_id: "gitea-paged".to_owned(),
            repository: fixture.repo.clone(),
            branch: "agent/gitea-paged".to_owned(),
            base_branch: "main".to_owned(),
            base_commit: fixture.base_commit.clone(),
            artifact_digest: sealed_artifact_digest(&fixture, "gitea-paged"),
            frozen_commit: commit,
            pull_request: PullRequestRequest {
                title: "Gitea paginated request".to_owned(),
                body: "Recover a lost response.".to_owned(),
            },
        }),
    };

    let first = cli_request(&fixture, &request);
    assert_eq!(first["error"]["code"], "forge_unavailable", "{first}");
    let retry = cli_request(&fixture, &request);
    assert_eq!(retry["status"], "ok", "{retry}");
    assert_eq!(
        retry["result"]["pull_request_created"]["already_present"],
        true
    );
    server.join().unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state.posts, 1, "lost response must not create another PR");
    assert!(
        state.query_contract_valid,
        "Gitea received unsupported filters"
    );
    assert_eq!(state.list_pages, vec![1, 2, 3, 1, 2, 3]);
}

#[test]
fn github_retry_finds_closed_run_pr_on_second_page() {
    let kind = coding_agent_executor::ForgeKind::GitHub;
    let (api_url, web_url, state, server) = mock_forge(kind, 1, true, false, true);
    let fixture = fixture(Some((kind, api_url, web_url.clone())));
    let commit = commit_workspace_via_cli(&fixture, "github-paged", "change.txt", "safe\n");
    {
        let mut state = state.lock().unwrap();
        state.head_commit = commit.clone();
        state.base_commit = fixture.base_commit.clone();
        state.web_url = web_url;
    }
    let pushed = push_via_cli(&fixture, "github-paged", &commit);
    assert_eq!(pushed["status"], "ok", "{pushed}");
    let request = ExecutorRequest {
        protocol_version: 2,
        operation: Operation::CreatePullRequest(CreatePullRequestRequest {
            run_id: "github-paged".to_owned(),
            attempt_id: "github-paged".to_owned(),
            repository: fixture.repo.clone(),
            branch: "agent/github-paged".to_owned(),
            base_branch: "main".to_owned(),
            base_commit: fixture.base_commit.clone(),
            artifact_digest: sealed_artifact_digest(&fixture, "github-paged"),
            frozen_commit: commit,
            pull_request: PullRequestRequest {
                title: "GitHub paginated request".to_owned(),
                body: "Recover a lost response.".to_owned(),
            },
        }),
    };

    let first = cli_request(&fixture, &request);
    assert_eq!(first["error"]["code"], "forge_unavailable", "{first}");
    let retry = cli_request(&fixture, &request);
    assert_eq!(retry["status"], "ok", "{retry}");
    assert_eq!(
        retry["result"]["pull_request_created"]["already_present"],
        true
    );
    server.join().unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state.posts, 1, "closed existing PR must not be recreated");
    assert!(
        state.query_contract_valid,
        "GitHub filters were not preserved"
    );
    assert_eq!(state.list_pages, vec![1, 2, 1, 2]);
}

#[test]
fn malformed_circular_or_filter_dropping_links_fail_closed_before_post() {
    for (index, fault) in [
        LinkFault::Malformed,
        LinkFault::Circular,
        LinkFault::MissingFilters,
    ]
    .into_iter()
    .enumerate()
    {
        let kind = coding_agent_executor::ForgeKind::Gitea;
        let (api_url, web_url, state, server) = mock_forge(kind, 1, false, false, false);
        state.lock().unwrap().link_fault = Some(fault);
        let fixture = fixture(Some((kind, api_url, web_url.clone())));
        let run_id = format!("bad-link-{index}");
        let commit = commit_workspace_via_cli(&fixture, &run_id, "change.txt", "safe\n");
        let pushed = push_via_cli(&fixture, &run_id, &commit);
        assert_eq!(pushed["status"], "ok", "{pushed}");
        {
            let mut state = state.lock().unwrap();
            state.head_commit = commit.clone();
            state.base_commit = fixture.base_commit.clone();
            state.web_url = web_url;
        }
        let response = create_pr_via_cli(&fixture, &run_id, &commit, "Bad link", "No post.");
        assert_eq!(response["error"]["code"], "forge_unavailable", "{response}");
        server.join().unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.posts, 0);
        assert_eq!(state.list_pages, vec![1]);
    }
}

#[test]
fn truncated_or_oversized_pr_listing_never_falls_through_to_post() {
    for (run_id, oversized, truncated, expected) in [
        ("oversized-list", true, false, "response_too_large"),
        ("truncated-list", false, true, "forge_unavailable"),
    ] {
        let kind = coding_agent_executor::ForgeKind::Gitea;
        let (api_url, web_url, state, server) = mock_forge(kind, 0, false, false, false);
        {
            let mut state = state.lock().unwrap();
            state.oversized_list_response = oversized;
            state.truncated_list_response = truncated;
        }
        let fixture = fixture(Some((kind, api_url, web_url.clone())));
        let commit = commit_workspace_via_cli(&fixture, run_id, "change.txt", "safe\n");
        let pushed = push_via_cli(&fixture, run_id, &commit);
        assert_eq!(pushed["status"], "ok", "{pushed}");
        {
            let mut state = state.lock().unwrap();
            state.head_commit = commit.clone();
            state.base_commit = fixture.base_commit.clone();
            state.web_url = web_url;
        }
        let response = create_pr_via_cli(&fixture, run_id, &commit, "Incomplete list", "No post.");
        assert_eq!(response["error"]["code"], expected, "{response}");
        server.join().unwrap();
        assert_eq!(state.lock().unwrap().posts, 0);
    }

    let kind = coding_agent_executor::ForgeKind::Gitea;
    let (api_url, web_url, state, server) = mock_forge(kind, 0, false, false, false);
    state.lock().unwrap().partial_list_response = true;
    let fixture = fixture(Some((kind, api_url, web_url.clone())));
    let commit = commit_workspace_via_cli(&fixture, "partial-list", "change.txt", "safe\n");
    let pushed = push_via_cli(&fixture, "partial-list", &commit);
    assert_eq!(pushed["status"], "ok", "{pushed}");
    {
        let mut state = state.lock().unwrap();
        state.head_commit = commit.clone();
        state.base_commit = fixture.base_commit.clone();
        state.web_url = web_url;
    }
    let response = create_pr_via_cli(&fixture, "partial-list", &commit, "Partial", "No post.");
    assert_eq!(response["error"]["code"], "forge_unavailable", "{response}");
    server.join().unwrap();
    assert_eq!(state.lock().unwrap().posts, 0);
}

#[test]
fn pagination_cap_fails_closed_without_creating_a_pull_request() {
    let kind = coding_agent_executor::ForgeKind::Gitea;
    let (api_url, web_url, state, server) = mock_forge(kind, 100, false, false, false);
    let fixture = fixture(Some((kind, api_url, web_url.clone())));
    let commit = commit_workspace_via_cli(&fixture, "page-cap", "change.txt", "safe\n");
    let pushed = push_via_cli(&fixture, "page-cap", &commit);
    assert_eq!(pushed["status"], "ok", "{pushed}");
    {
        let mut state = state.lock().unwrap();
        state.head_commit = commit.clone();
        state.base_commit = fixture.base_commit.clone();
        state.web_url = web_url;
    }
    let response = create_pr_via_cli(&fixture, "page-cap", &commit, "Page cap", "No post.");
    assert_eq!(response["error"]["code"], "forge_unavailable", "{response}");
    server.join().unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state.list_pages.len(), 100);
    assert_eq!(state.list_pages.last(), Some(&100));
    assert_eq!(state.posts, 0);
}

#[test]
fn overlapping_pages_from_a_moving_listing_fail_closed_without_post() {
    let kind = coding_agent_executor::ForgeKind::Gitea;
    let (api_url, web_url, state, server) = mock_forge(kind, 2, false, false, false);
    state.lock().unwrap().overlapping_list_pages = true;
    let fixture = fixture(Some((kind, api_url, web_url.clone())));
    let commit = commit_workspace_via_cli(&fixture, "moving-list", "change.txt", "safe\n");
    let pushed = push_via_cli(&fixture, "moving-list", &commit);
    assert_eq!(pushed["status"], "ok", "{pushed}");
    {
        let mut state = state.lock().unwrap();
        state.head_commit = commit.clone();
        state.base_commit = fixture.base_commit.clone();
        state.web_url = web_url;
    }
    let response = create_pr_via_cli(&fixture, "moving-list", &commit, "Moving list", "No post.");
    assert_eq!(response["error"]["code"], "forge_unavailable", "{response}");
    server.join().unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state.list_pages, vec![1, 2]);
    assert_eq!(state.posts, 0);
}

#[test]
fn unfiltered_gitea_results_are_independently_validated_and_ambiguous_matches_fail() {
    let kind = coding_agent_executor::ForgeKind::Gitea;
    let (api_url, web_url, state, server) = mock_forge(kind, 0, false, false, false);
    let fixture = fixture(Some((kind, api_url, web_url.clone())));
    let mismatches = [
        "wrong-head-owner",
        "wrong-head-branch",
        "wrong-head-commit",
        "missing-run-marker",
        "wrong-base-branch",
        "wrong-base-owner",
    ];
    for (index, mismatch) in mismatches.into_iter().enumerate() {
        let run_id = format!("validate-pr-{index}");
        let commit = commit_workspace_via_cli(&fixture, &run_id, "change.txt", "safe\n");
        let pushed = push_via_cli(&fixture, &run_id, &commit);
        assert_eq!(pushed["status"], "ok", "{pushed}");
        let title = format!("Validate {mismatch}");
        let body = "Verify every returned run candidate.";
        let mut pull = matching_pull(
            &run_id,
            &commit,
            &title,
            body,
            &web_url,
            index as u64 + 70,
            &fixture.base_commit,
            "open",
        );
        match mismatch {
            "wrong-head-owner" => pull["head"]["repo"]["full_name"] = json!("attacker/repo"),
            "wrong-head-branch" => pull["head"]["ref"] = json!("agent/other-run"),
            "wrong-head-commit" => pull["head"]["sha"] = json!("b".repeat(40)),
            "missing-run-marker" => pull["body"] = json!(body),
            "wrong-base-branch" => pull["base"]["ref"] = json!("release"),
            "wrong-base-owner" => pull["base"]["repo"]["full_name"] = json!("owner/other"),
            _ => unreachable!(),
        }
        {
            let mut state = state.lock().unwrap();
            state.head_commit = commit.clone();
            state.base_commit = fixture.base_commit.clone();
            state.created_pr = Some(pull);
        }
        let response = create_pr_via_cli(&fixture, &run_id, &commit, &title, body);
        assert_eq!(
            response["error"]["code"], "pull_request_conflict",
            "{response}"
        );
    }

    let run_id = "duplicate-prs";
    let commit = commit_workspace_via_cli(&fixture, run_id, "duplicate.txt", "safe\n");
    let pushed = push_via_cli(&fixture, run_id, &commit);
    assert_eq!(pushed["status"], "ok", "{pushed}");
    let title = "Ambiguous candidates";
    let body = "Do not choose one of two run PRs.";
    {
        let mut state = state.lock().unwrap();
        state.head_commit = commit.clone();
        state.base_commit = fixture.base_commit.clone();
        state.created_pr = Some(matching_pull(
            run_id,
            &commit,
            title,
            body,
            &web_url,
            801,
            &fixture.base_commit,
            "open",
        ));
        state.duplicate_pr = Some(matching_pull(
            run_id,
            &commit,
            title,
            body,
            &web_url,
            802,
            &fixture.base_commit,
            "closed",
        ));
    }
    let response = create_pr_via_cli(&fixture, run_id, &commit, title, body);
    assert_eq!(
        response["error"]["code"], "pull_request_conflict",
        "{response}"
    );
    server.join().unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state.posts, 0);
    assert!(state.query_contract_valid);
}

#[test]
fn create_conflict_is_reconciled_once_without_retrying_post() {
    let kind = coding_agent_executor::ForgeKind::Gitea;
    let (api_url, web_url, state, server) = mock_forge(kind, 0, false, true, false);
    let fixture = fixture(Some((kind, api_url, web_url.clone())));
    let commit = commit_workspace_via_cli(&fixture, "post-conflict", "change.txt", "safe\n");
    let pushed = push_via_cli(&fixture, "post-conflict", &commit);
    assert_eq!(pushed["status"], "ok", "{pushed}");
    {
        let mut state = state.lock().unwrap();
        state.head_commit = commit.clone();
        state.base_commit = fixture.base_commit.clone();
        state.web_url = web_url;
    }

    let response = create_pr_via_cli(
        &fixture,
        "post-conflict",
        &commit,
        "Concurrent request",
        "Reconcile a forge conflict.",
    );
    assert_eq!(response["status"], "ok", "{response}");
    assert_eq!(
        response["result"]["pull_request_created"]["already_present"],
        true
    );
    assert!(!response.to_string().contains(&fixture.credential));
    server.join().unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state.posts, 1, "a 409 must never trigger a second POST");
    assert_eq!(
        state.list_pages,
        vec![1, 1],
        "409 gets one bounded relookup"
    );
}

#[test]
fn unresolved_create_conflict_is_not_posted_again_on_same_request_retry() {
    let kind = coding_agent_executor::ForgeKind::Gitea;
    let (api_url, web_url, state, server) = mock_forge(kind, 0, false, true, false);
    state.lock().unwrap().hide_conflicted_pr = true;
    let fixture = fixture(Some((kind, api_url, web_url.clone())));
    let commit = commit_workspace_via_cli(&fixture, "unknown-post-result", "change.txt", "safe\n");
    let pushed = push_via_cli(&fixture, "unknown-post-result", &commit);
    assert_eq!(pushed["status"], "ok", "{pushed}");
    {
        let mut state = state.lock().unwrap();
        state.head_commit = commit.clone();
        state.base_commit = fixture.base_commit.clone();
        state.web_url = web_url;
    }

    let first = create_pr_via_cli(
        &fixture,
        "unknown-post-result",
        &commit,
        "Unknown result",
        "Do not issue another POST.",
    );
    assert_eq!(first["error"]["code"], "pull_request_conflict", "{first}");
    let retry = create_pr_via_cli(
        &fixture,
        "unknown-post-result",
        &commit,
        "Unknown result",
        "Do not issue another POST.",
    );
    assert_eq!(retry["error"]["code"], "forge_unavailable", "{retry}");
    server.join().unwrap();
    let state = state.lock().unwrap();
    assert_eq!(
        state.posts, 1,
        "an unknown create result must stay lookup-only"
    );
    assert_eq!(state.list_pages, vec![1, 1, 1]);
}

#[test]
fn unknown_422_or_server_post_result_stays_lookup_only() {
    for status in [422, 500] {
        let kind = coding_agent_executor::ForgeKind::Gitea;
        let (api_url, web_url, state, server) = mock_forge(kind, 0, false, false, false);
        {
            let mut state = state.lock().unwrap();
            state.hide_conflicted_pr = true;
            state.unknown_post_status = Some(status);
        }
        let fixture = fixture(Some((kind, api_url, web_url.clone())));
        let run_id = format!("unknown-http-{status}");
        let commit = commit_workspace_via_cli(&fixture, &run_id, "change.txt", "safe\n");
        let pushed = push_via_cli(&fixture, &run_id, &commit);
        assert_eq!(pushed["status"], "ok", "{pushed}");
        {
            let mut state = state.lock().unwrap();
            state.head_commit = commit.clone();
            state.base_commit = fixture.base_commit.clone();
            state.web_url = web_url;
        }

        let first = create_pr_via_cli(&fixture, &run_id, &commit, "Unknown POST", "No replay.");
        let expected = if status == 422 {
            "pull_request_conflict"
        } else {
            "forge_unavailable"
        };
        assert_eq!(first["error"]["code"], expected, "{first}");
        assert!(!first.to_string().contains(&fixture.credential));
        let retry = create_pr_via_cli(&fixture, &run_id, &commit, "Unknown POST", "No replay.");
        assert_eq!(retry["error"]["code"], "forge_unavailable", "{retry}");
        assert!(!retry.to_string().contains(&fixture.credential));
        server.join().unwrap();
        let state = state.lock().unwrap();
        assert_eq!(
            state.posts, 1,
            "HTTP {status} must not trigger a second POST"
        );
        assert_eq!(state.list_pages.len(), if status == 422 { 3 } else { 2 });
        assert!(state.list_pages.iter().all(|page| *page == 1));
    }
}

#[test]
fn empty_and_reverted_trees_are_not_pushed_or_sent_to_the_pr_forge() {
    let kind = coding_agent_executor::ForgeKind::Gitea;
    let (api_url, web_url, state, server) = mock_forge(kind, 0, false, false, false);
    let fixture = fixture(Some((kind, api_url, web_url)));

    let same_base_commit = commit_empty_workspace_via_cli(&fixture, "same-base");
    let same_base_push = push_via_cli(&fixture, "same-base", &same_base_commit);
    assert_eq!(same_base_push["error"]["code"], "no_changes_to_publish");
    assert!(remote_branch_tip(&fixture, "agent/same-base").is_empty());

    let empty_commit = commit_empty_workspace_via_cli(&fixture, "empty-tree");
    let empty_push = push_via_cli(&fixture, "empty-tree", &empty_commit);
    assert_eq!(empty_push["error"]["code"], "no_changes_to_publish");
    assert!(remote_branch_tip(&fixture, "agent/empty-tree").is_empty());
    let empty_pr = create_pr_via_cli(
        &fixture,
        "empty-tree",
        &empty_commit,
        "No changes",
        "Nothing to publish.",
    );
    assert_eq!(empty_pr["error"]["code"], "workspace_not_prepared");

    let reverted_commit = commit_reverted_workspace_via_cli(&fixture, "reverted-tree");
    let reverted_push = push_via_cli(&fixture, "reverted-tree", &reverted_commit);
    assert_eq!(reverted_push["error"]["code"], "no_changes_to_publish");
    assert!(remote_branch_tip(&fixture, "agent/reverted-tree").is_empty());

    server.join().unwrap();
    let state = state.lock().unwrap();
    assert!(state.list_pages.is_empty());
    assert_eq!(state.posts, 0);
}

#[test]
fn actual_tree_change_still_publishes_through_the_public_cli() {
    let fixture = fixture(None);
    let commit = commit_workspace_via_cli(&fixture, "real-tree-change", "change.txt", "safe\n");
    let response = push_via_cli(&fixture, "real-tree-change", &commit);
    assert_eq!(response["status"], "ok", "{response}");
    assert_eq!(response["result"]["pushed"]["commit"], commit);
    assert_eq!(
        remote_branch_tip(&fixture, "agent/real-tree-change"),
        commit
    );
}

#[test]
fn unapproved_external_issue_close_directives_are_rejected_before_forge_access() {
    let kind = coding_agent_executor::ForgeKind::Gitea;
    let (api_url, web_url, state, server) = mock_forge(kind, 0, false, false, false);
    let fixture = fixture(Some((kind, api_url, web_url.clone())));
    let commit = commit_workspace_via_cli(&fixture, "no-external-close", "change.txt", "safe\n");
    let pushed = push_via_cli(&fixture, "no-external-close", &commit);
    assert_eq!(pushed["status"], "ok", "{pushed}");
    {
        let mut state = state.lock().unwrap();
        state.head_commit = commit.clone();
        state.base_commit = fixture.base_commit.clone();
        state.web_url = web_url;
    }

    for (title, body) in [
        ("Tracked work", "Close #42 after the change lands."),
        ("Tracked work", "Fixes acme/tracker#73."),
        ("Tracked work", "RESOLVED #9"),
        (
            "Tracked work",
            "CLOSES https://forge.local/owner/repo/issues/123",
        ),
        (
            "Tracked work",
            "fIxEs:\n  [issue #123](https://another-forge.invalid/acme/repo/issues/123)",
        ),
        (
            "Tracked work",
            "ReSoLvEs:\n\n  <https://another-forge.invalid/acme/repo/issues/123>",
        ),
        (
            "Tracked work",
            "**fIxEs [issue](https://another-forge.invalid/acme/repo/issues/124)**",
        ),
        (
            "Tracked work",
            "```markdown\nCloses https://another-forge.invalid/acme/repo/issues/126\n```",
        ),
        ("Tracked work", "cLoSeD:\n\towner/repo#42"),
        (
            "CLOSES https://forge.local/owner/repo/issues/124",
            "Ordinary body.",
        ),
        (
            "Fixes [issue](https://another-forge.invalid/acme/repo/issues/125)",
            "Ordinary body.",
        ),
        (
            "RESOLVES: <https://another-forge.invalid/acme/repo/issues/127>",
            "Ordinary body.",
        ),
    ] {
        let response = create_pr_via_cli(&fixture, "no-external-close", &commit, title, body);
        assert_eq!(response["error"]["code"], "invalid_request", "{response}");
    }
    let safe = create_pr_via_cli(
        &fixture,
        "no-external-close",
        &commit,
        "Parser fixes",
        "We fixed parser behavior; external issue #9 remains open.",
    );
    assert_eq!(safe["status"], "ok", "{safe}");
    server.join().unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state.posts, 1, "only the non-closing body is posted");
    assert_eq!(state.list_pages, vec![1]);
}

#[derive(Default)]
struct MockState {
    posts: usize,
    list_pages: Vec<usize>,
    authorization_seen: bool,
    reject_authorization: bool,
    lost_post_response: bool,
    conflict_first_post: bool,
    hide_conflicted_pr: bool,
    unknown_post_status: Option<u16>,
    close_created_pr: bool,
    query_contract_valid: bool,
    forge_kind: Option<coding_agent_executor::ForgeKind>,
    full_pages: usize,
    link_fault: Option<LinkFault>,
    oversized_list_response: bool,
    truncated_list_response: bool,
    partial_list_response: bool,
    overlapping_list_pages: bool,
    created_pr: Option<Value>,
    duplicate_pr: Option<Value>,
    head_commit: String,
    base_commit: String,
    web_url: String,
}

#[derive(Clone, Copy)]
enum LinkFault {
    Malformed,
    Circular,
    MissingFilters,
}

struct MockServer {
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    fn join(mut self) -> thread::Result<()> {
        self.shutdown.store(true, Ordering::Release);
        self.handle.take().unwrap().join()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn git_http_server(
    root: PathBuf,
    mut after_advertisement: impl FnMut() + Send + 'static,
) -> (String, MockServer) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let stop = shutdown.clone();
    let handle = thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("accept Git fixture: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut buffer = [0u8; 4096];
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
            let mut request = headers.lines().next().unwrap().split_whitespace();
            let method = request.next().unwrap();
            let (path, query) = request.next().unwrap().split_once('?').unwrap_or_else(|| {
                (
                    headers
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap(),
                    "",
                )
            });
            let header_value = |name: &str| {
                headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.trim())
                    .unwrap_or("")
            };
            let length = header_value("Content-Length").parse::<usize>().unwrap_or(0);
            while bytes.len() - header_end < length {
                let mut buffer = [0u8; 4096];
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            let mut backend = Command::new("/usr/bin/git")
                .arg("http-backend")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_PROJECT_ROOT", &root)
                .env("GIT_HTTP_EXPORT_ALL", "1")
                .env("REMOTE_USER", "fixture")
                .env("REQUEST_METHOD", method)
                .env("PATH_INFO", path)
                .env("QUERY_STRING", query)
                .env("CONTENT_TYPE", header_value("Content-Type"))
                .env("CONTENT_LENGTH", length.to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            backend
                .stdin
                .take()
                .unwrap()
                .write_all(&bytes[header_end..header_end + length])
                .unwrap();
            let output = backend.wait_with_output().unwrap();
            assert!(output.status.success());
            let split = output
                .stdout
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap()
                + 4;
            if path.ends_with("/info/refs") && query == "service=git-upload-pack" {
                after_advertisement();
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n",
                output.stdout.len() - split
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(&output.stdout).unwrap();
        }
    });
    (
        format!("http://{address}/remote.git"),
        MockServer {
            shutdown,
            handle: Some(handle),
        },
    )
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn ordinary_workspace_push_rechecks_source_and_branch_creation_after_remote_advertisement() {
    for conflict in ["source", "remote"] {
        let mut fixture = fixture(None);
        let (request, workspace, _) =
            ordinary_publication_request(&fixture, "advertisement-race").await;
        let home = fixture.temp.path().join("fixture-home");
        let publisher = fixture.config.publisher_root.join("advertisement-race");
        git(
            &home,
            fixture.temp.path(),
            &[
                "clone",
                "--no-local",
                "--no-checkout",
                fixture.remote.to_str().unwrap(),
                publisher.to_str().unwrap(),
            ],
        );
        let remote = fixture.remote.clone();
        let base = fixture.base_commit.clone();
        let marker = Arc::new(AtomicBool::new(false));
        let changed = marker.clone();
        let callback_home = home.clone();
        let mut advertisements = 0;
        let (url, server) = git_http_server(fixture.temp.path().to_owned(), move || {
            advertisements += 1;
            if advertisements == 2 {
                if conflict == "source" {
                    fs::write(workspace.join("ordinary.txt"), "unapproved drift").unwrap();
                } else {
                    git(
                        &callback_home,
                        &remote,
                        &["update-ref", "refs/heads/agent/advertisement-race", &base],
                    );
                }
                changed.store(true, Ordering::Release);
            }
        });
        git(&home, &publisher, &["remote", "set-url", "origin", &url]);
        fixture.config.repositories[0].clone_url = url;
        fixture.config.repositories[0].publish_credential_source = None;
        let response: Value = serde_json::from_slice(
            &Executor::new(fixture.config.clone())
                .unwrap()
                .handle_json(&serde_json::to_vec(&request).unwrap())
                .await,
        )
        .unwrap();
        assert!(
            marker.load(Ordering::Acquire),
            "remote advertisement must trigger the concurrent change: {response}"
        );
        assert_eq!(
            response["error"]["code"],
            if conflict == "source" {
                "unsafe_workspace"
            } else {
                "remote_branch_conflict"
            },
            "{response}"
        );
        assert_eq!(
            remote_branch_tip(&fixture, "agent/advertisement-race"),
            if conflict == "source" {
                String::new()
            } else {
                fixture.base_commit.clone()
            }
        );
        server.join().unwrap();
    }
}

fn mock_forge(
    kind: coding_agent_executor::ForgeKind,
    full_pages: usize,
    drop_first_post: bool,
    conflict_first_post: bool,
    close_created_pr: bool,
) -> (String, String, Arc<Mutex<MockState>>, MockServer) {
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let state = Arc::new(Mutex::new(MockState {
        lost_post_response: drop_first_post,
        conflict_first_post,
        close_created_pr,
        query_contract_valid: true,
        forge_kind: Some(kind),
        full_pages,
        ..MockState::default()
    }));
    let worker_state = state.clone();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = shutdown.clone();
    let handle = thread::spawn(move || {
        while !worker_shutdown.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => handle_request(stream, &worker_state),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    let origin = format!("http://{address}");
    (
        format!("{origin}/api/v1"),
        origin,
        state,
        MockServer {
            shutdown,
            handle: Some(handle),
        },
    )
}

fn handle_request(mut stream: TcpStream, state: &Arc<Mutex<MockState>>) {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end;
    loop {
        let count = stream.read(&mut chunk).unwrap_or(0);
        if count == 0 {
            return;
        }
        request.extend_from_slice(&chunk[..count]);
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = index + 4;
            break;
        }
        if request.len() > 64 * 1024 {
            return;
        }
    }
    let header = String::from_utf8_lossy(&request[..header_end]).into_owned();
    let mut request_line = header.lines().next().unwrap_or("").split_ascii_whitespace();
    let method = request_line.next().unwrap_or("");
    let target = request_line.next().unwrap_or("/");
    let host = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("host")
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default();
    let content_length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while request.len() < header_end + content_length {
        let count = stream.read(&mut chunk).unwrap_or(0);
        if count == 0 {
            return;
        }
        request.extend_from_slice(&chunk[..count]);
    }
    let body = &request[header_end..header_end + content_length];
    let mut state = state.lock().unwrap();
    state.authorization_seen |= header
        .to_ascii_lowercase()
        .contains("fixture-publish-secret-987654321");
    if state.reject_authorization {
        respond(&mut stream, "401 Unauthorized", "{}");
        return;
    }
    if method == "GET" {
        let Ok(url) = Url::parse(&format!("http://{host}{target}")) else {
            respond(&mut stream, "400 Bad Request", "{}");
            return;
        };
        let query: Vec<_> = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        let page = query
            .iter()
            .find_map(|(key, value)| {
                (key == "page")
                    .then(|| value.parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let has = |name: &str, value: &str| {
            query
                .iter()
                .any(|(query_name, query_value)| query_name == name && query_value == value)
        };
        let valid_query = match state.forge_kind {
            Some(coding_agent_executor::ForgeKind::GitHub) => {
                has("state", "all")
                    && query
                        .iter()
                        .any(|(name, value)| name == "head" && value.starts_with("owner:agent/"))
                    && has("base", "main")
                    && has("per_page", "100")
                    && !query
                        .iter()
                        .any(|(name, _)| name == "limit" || name == "base_branch")
            }
            Some(coding_agent_executor::ForgeKind::Gitea) => {
                has("state", "all")
                    && has("base_branch", "main")
                    && has("limit", "100")
                    && !query
                        .iter()
                        .any(|(name, _)| name == "head" || name == "base" || name == "per_page")
            }
            None => false,
        } && page > 0;
        state.query_contract_valid &= valid_query;
        if !valid_query {
            respond(&mut stream, "422 Unprocessable Entity", "{}");
            return;
        }
        state.list_pages.push(page);
        let mut pulls = Vec::new();
        if page <= state.full_pages {
            let result_page = if state.overlapping_list_pages && page > 1 {
                page - 1
            } else {
                page
            };
            for index in 0..100 {
                pulls.push(decoy_pull(result_page * 100 + index, &state));
            }
        } else if page == state.full_pages + 1 {
            if let Some(created) = &state.created_pr {
                pulls.push(created.clone());
            }
            if let Some(duplicate) = &state.duplicate_pr {
                pulls.push(duplicate.clone());
            }
        }
        let response_body = if state.oversized_list_response {
            json!([{"padding": "x".repeat(1_100_000)}]).to_string()
        } else {
            Value::Array(pulls).to_string()
        };
        if state.truncated_list_response {
            respond_truncated(&mut stream, "200 OK", &response_body);
            return;
        }
        if state.partial_list_response {
            respond_with_link(&mut stream, "206 Partial Content", &response_body, None);
            return;
        }
        let link = if page <= state.full_pages {
            Some(match state.link_fault {
                Some(LinkFault::Malformed) => "malformed-link".to_owned(),
                Some(LinkFault::Circular) => format!("<{url}>; rel=\"next\""),
                Some(LinkFault::MissingFilters) => {
                    let mut next = url.clone();
                    next.set_query(Some(&format!("page={}", page + 1)));
                    format!("<{next}>; rel=\"next\"")
                }
                None => {
                    let mut next_url = url.clone();
                    let retained: Vec<_> = url
                        .query_pairs()
                        .filter(|(name, _)| name != "page")
                        .map(|(name, value)| (name.into_owned(), value.into_owned()))
                        .collect();
                    next_url.set_query(None);
                    next_url
                        .query_pairs_mut()
                        .extend_pairs(
                            retained
                                .iter()
                                .map(|(name, value)| (name.as_str(), value.as_str())),
                        )
                        .append_pair("page", &(page + 1).to_string());
                    format!("<{next_url}>; rel=\"next\"")
                }
            })
        } else {
            None
        };
        respond_with_link(&mut stream, "200 OK", &response_body, link.as_deref());
    } else if method == "POST" {
        state.posts += 1;
        let body: Value = serde_json::from_slice(body).expect("valid forge POST body");
        let marker = body["body"]
            .as_str()
            .unwrap()
            .split("\n")
            .last()
            .unwrap()
            .to_owned();
        let pr = json!({
            "number": 17,
            "title": body["title"],
            "body": body["body"],
            "html_url": format!("{}/owner/repo/pulls/17", state.web_url),
            "head": {
                "ref": body["head"],
                "sha": state.head_commit.clone(),
                "repo": {"full_name": "owner/repo"}
            },
            "base": {
                "ref": "main",
                "sha": state.base_commit.clone(),
                "repo": {"full_name": "owner/repo"}
            },
            "state": if state.close_created_pr { "closed" } else { "open" },
            "marker": marker
        });
        if let Some(status) = state.unknown_post_status.take() {
            if !state.hide_conflicted_pr {
                state.created_pr = Some(pr.clone());
            }
            let status_line = match status {
                422 => "422 Unprocessable Entity",
                500 => "500 Internal Server Error",
                _ => unreachable!("mock only injects known status codes"),
            };
            respond(
                &mut stream,
                status_line,
                "{\"message\":\"fixture-publish-secret-987654321\"}",
            );
        } else if state.conflict_first_post {
            state.conflict_first_post = false;
            if !state.hide_conflicted_pr {
                state.created_pr = Some(pr.clone());
            }
            respond(
                &mut stream,
                "409 Conflict",
                "{\"message\":\"fixture-publish-secret-987654321\"}",
            );
        } else if state.lost_post_response {
            state.lost_post_response = false;
            state.created_pr = Some(pr.clone());
        } else {
            state.created_pr = Some(pr.clone());
            respond(&mut stream, "201 Created", &pr.to_string());
        }
    } else {
        respond(&mut stream, "404 Not Found", "{}");
    }
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    respond_with_link(stream, status, body, None);
}

fn respond_with_link(stream: &mut TcpStream, status: &str, body: &str, link: Option<&str>) {
    let link_header = link
        .map(|link| format!("Link: {link}\r\n"))
        .unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{link_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn respond_truncated(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len() + 32
    );
    let _ = stream.write_all(response.as_bytes());
}

fn decoy_pull(index: usize, state: &MockState) -> Value {
    json!({
        "number": index as u64 + 1,
        "title": format!("Unrelated pull request {index}"),
        "body": "No run marker.",
        "html_url": format!("{}/owner/repo/pulls/{}", state.web_url, index + 1),
        "head": {
            "ref": format!("unrelated-{index}"),
            "sha": "a".repeat(40),
            "repo": {"full_name": "owner/repo"}
        },
        "base": {
            "ref": "main",
            "sha": state.base_commit,
            "repo": {"full_name": "owner/repo"}
        },
        "state": "open"
    })
}

#[allow(clippy::too_many_arguments)]
fn matching_pull(
    run_id: &str,
    commit: &str,
    title: &str,
    body: &str,
    web_url: &str,
    number: u64,
    base_commit: &str,
    state: &str,
) -> Value {
    let body = format!("{}\n\n<!-- coding-agent-run:{run_id} -->", body.trim_end());
    json!({
        "number": number,
        "title": title,
        "body": body,
        "html_url": format!("{web_url}/owner/repo/pulls/{number}"),
        "head": {
            "ref": format!("agent/{run_id}"),
            "sha": commit,
            "repo": {"full_name": "owner/repo"}
        },
        "base": {
            "ref": "main",
            "sha": base_commit,
            "repo": {"full_name": "owner/repo"}
        },
        "state": state
    })
}

#[tokio::test]
async fn sealed_artifact_publishes_captured_bytes_after_source_workspace_is_removed() {
    let fixture = fixture(None);
    let run_id = "sealed-source-removed";
    let workspace = prepare_attempt(&fixture, run_id, run_id).await;
    fs::write(workspace.join("result.txt"), "sealed bytes\n").unwrap();
    let resolved = seal_current_workspace(&fixture, run_id).await;
    let commit = resolved.commit.clone();
    fs::write(workspace.join("result.txt"), "mutated after seal\n").unwrap();
    fs::remove_dir_all(&workspace).unwrap();

    let result = Executor::new(fixture.config.clone())
        .unwrap()
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Push(push_request_for_attempt(
                &fixture,
                run_id,
                &resolved.attempt_id,
                &resolved.artifact_digest,
                &resolved.commit,
            )),
        })
        .await
        .unwrap();
    let ExecutorResult::Pushed(receipt) = result else {
        panic!("expected artifact-only push")
    };
    assert_eq!(receipt.commit, commit);
    assert_eq!(receipt.artifact_digest, resolved.artifact_digest);
    assert_eq!(
        remote_branch_tip(&fixture, &format!("agent/{run_id}")),
        commit
    );
    let contents = git(
        &fixture.temp.path().join("fixture-home"),
        &fixture.remote,
        &[
            "--git-dir",
            fixture.remote.to_str().unwrap(),
            "show",
            &format!("{commit}:result.txt"),
        ],
    );
    assert_eq!(contents.stdout, b"sealed bytes\n");
}

#[tokio::test]
async fn capture_retry_preserves_original_untracked_bytes_and_declared_exclusions() {
    let fixture = fixture(None);
    let run_id = "capture-retry";
    let workspace = prepare_attempt(&fixture, run_id, run_id).await;
    fs::write(workspace.join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(workspace.join("ignored.txt"), "ignored by Git, captured\n").unwrap();
    fs::write(workspace.join("untracked.txt"), "first captured bytes\n").unwrap();
    fs::create_dir_all(workspace.join("nested")).unwrap();
    fs::write(workspace.join("nested/child.txt"), "nested bytes\n").unwrap();
    fs::create_dir_all(workspace.join(".coding-agent")).unwrap();
    fs::write(workspace.join(".coding-agent/report.md"), "host report\n").unwrap();
    let request = CaptureRequest {
        run_id: run_id.to_owned(),
        attempt_id: run_id.to_owned(),
        repository: fixture.repo.clone(),
        base_branch: "main".to_owned(),
        base_commit: fixture.base_commit.clone(),
        approved_snapshot_digest: "a".repeat(64),
        scope_digest: "b".repeat(64),
        intent_approval_digest: "c".repeat(64),
    };
    let mut untrusted_writer_claim = serde_json::to_value(ExecutorRequest {
        protocol_version: 2,
        operation: Operation::Capture(request.clone()),
    })
    .unwrap();
    untrusted_writer_claim["operation"]["capture"]["writer_stopped"] = json!(true);
    assert!(serde_json::from_value::<ExecutorRequest>(untrusted_writer_claim).is_err());
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let first = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Capture(request.clone()),
        })
        .await
        .unwrap();
    let ExecutorResult::Captured(first) = first else {
        panic!("expected capture")
    };
    assert_eq!(
        first.capture_guarantee,
        coding_agent_executor::CaptureGuarantee::HostIsolatedAfterSeal
    );
    assert_eq!(
        serde_json::to_value(first.capture_guarantee).unwrap(),
        json!("host_isolated_after_seal")
    );

    fs::write(workspace.join("untracked.txt"), "later bytes\n").unwrap();
    fs::write(workspace.join("later.txt"), "not in the first capture\n").unwrap();
    fs::remove_dir_all(&workspace).unwrap();
    let retry = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Capture(request),
        })
        .await
        .unwrap();
    let ExecutorResult::Captured(retry) = retry else {
        panic!("expected idempotent capture")
    };
    assert_eq!(first, retry);

    let manifest_path = fixture
        .config
        .state_root
        .join("runs")
        .join(run_id)
        .join("attempts")
        .join(run_id)
        .join("manifest.json");
    let manifest: Value = serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    let paths = manifest["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"untracked.txt"));
    assert!(paths.contains(&"ignored.txt"));
    assert!(paths.contains(&"nested/child.txt"));
    assert!(!paths.contains(&".coding-agent/report.md"));
    assert!(!paths.iter().any(|path| path.starts_with(".git/")));

    let seal_result = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Seal(SealRequest {
                run_id: run_id.to_owned(),
                attempt_id: run_id.to_owned(),
                manifest_digest: first.manifest_digest,
            }),
        })
        .await
        .unwrap();
    let ExecutorResult::Sealed(sealed) = seal_result else {
        panic!("expected sealed artifact")
    };
    assert_eq!(sealed.capture_guarantee, first.capture_guarantee);
    let pushed = push(&fixture, run_id, &sealed.commit).await;
    assert!(matches!(pushed, ExecutorResult::Pushed(_)));
    let captured_contents = git(
        &fixture.temp.path().join("fixture-home"),
        &fixture.remote,
        &[
            "--git-dir",
            fixture.remote.to_str().unwrap(),
            "show",
            &format!("{}:untracked.txt", sealed.commit),
        ],
    );
    assert_eq!(captured_contents.stdout, b"first captured bytes\n");
    let ignored_contents = git(
        &fixture.temp.path().join("fixture-home"),
        &fixture.remote,
        &[
            "--git-dir",
            fixture.remote.to_str().unwrap(),
            "show",
            &format!("{}:ignored.txt", sealed.commit),
        ],
    );
    assert_eq!(ignored_contents.stdout, b"ignored by Git, captured\n");
    let nested_contents = git(
        &fixture.temp.path().join("fixture-home"),
        &fixture.remote,
        &[
            "--git-dir",
            fixture.remote.to_str().unwrap(),
            "show",
            &format!("{}:nested/child.txt", sealed.commit),
        ],
    );
    assert_eq!(nested_contents.stdout, b"nested bytes\n");
}

#[tokio::test]
async fn artifact_selector_is_bound_to_attempt_and_receipt_digest() {
    let fixture = fixture(None);
    let run_id = "attempt-bound";
    let first_workspace = prepare_attempt(&fixture, run_id, "attempt-a").await;
    fs::write(first_workspace.join("result.txt"), "attempt a\n").unwrap();
    let (_, first) = capture_and_seal_attempt(&fixture, run_id, "attempt-a").await;
    let second_workspace = prepare_attempt(&fixture, run_id, "attempt-b").await;
    fs::write(second_workspace.join("result.txt"), "attempt b\n").unwrap();
    let (_, second) = capture_and_seal_attempt(&fixture, run_id, "attempt-b").await;

    let executor = Executor::new(fixture.config.clone()).unwrap();
    let resolution = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Resolve(ResolveRequest {
                run_id: run_id.to_owned(),
                attempt_id: "attempt-a".to_owned(),
                artifact_digest: second.artifact_digest.clone(),
            }),
        })
        .await
        .unwrap_err();
    assert_eq!(resolution.code, ExecutorErrorCode::ArtifactMismatch);

    let request = push_request_for_attempt(
        &fixture,
        run_id,
        "attempt-a",
        &second.artifact_digest,
        &second.commit,
    );
    let publication = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Push(request),
        })
        .await
        .unwrap_err();
    assert_eq!(publication.code, ExecutorErrorCode::ArtifactMismatch);
    assert!(remote_branch_tip(&fixture, &format!("agent/{run_id}")).is_empty());

    let wrong_digest = push_request_for_attempt(
        &fixture,
        run_id,
        "attempt-a",
        &"f".repeat(64),
        &first.commit,
    );
    let publication = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Push(wrong_digest),
        })
        .await
        .unwrap_err();
    assert_eq!(publication.code, ExecutorErrorCode::ArtifactMismatch);
}

#[tokio::test]
async fn missing_or_partial_seal_never_falls_back_to_guest_workspace_git() {
    let fixture = fixture(None);
    let run_id = "missing-seal";
    let workspace = prepare_attempt(&fixture, run_id, run_id).await;
    fs::write(workspace.join("candidate.txt"), "unsealed candidate\n").unwrap();
    let missing = push_request_for_attempt(
        &fixture,
        run_id,
        run_id,
        &"d".repeat(64),
        &fixture.base_commit,
    );
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let error = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Push(missing),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::ArtifactNotSealed);

    let commit = commit_workspace(&fixture, "partial-seal", "candidate.txt", "sealed\n").await;
    let partial_request = push_request(&fixture, "partial-seal", &commit);
    let partial = fixture
        .config
        .state_root
        .join("runs/partial-seal/attempts/partial-seal/seal.json");
    fs::remove_file(partial).unwrap();
    let error = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Push(partial_request),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::ArtifactNotSealed);
    assert!(remote_branch_tip(&fixture, "agent/partial-seal").is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn capture_rejects_workspace_symlinks_instead_of_following_host_paths() {
    let fixture = fixture(None);
    let run_id = "workspace-symlink";
    let workspace = prepare_attempt(&fixture, run_id, run_id).await;
    let outside = fixture.temp.path().join("outside-secret.txt");
    fs::write(&outside, "must not be read\n").unwrap();
    std::os::unix::fs::symlink(&outside, workspace.join("leak.txt")).unwrap();
    let error = Executor::new(fixture.config.clone())
        .unwrap()
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Capture(CaptureRequest {
                run_id: run_id.to_owned(),
                attempt_id: run_id.to_owned(),
                repository: fixture.repo.clone(),
                base_branch: "main".to_owned(),
                base_commit: fixture.base_commit.clone(),
                approved_snapshot_digest: "a".repeat(64),
                scope_digest: "b".repeat(64),
                intent_approval_digest: "c".repeat(64),
            }),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::UnsafeWorkspace);
    assert_eq!(fs::read_to_string(outside).unwrap(), "must not be read\n");
    assert!(
        !fixture
            .config
            .state_root
            .join("runs/workspace-symlink/attempts/workspace-symlink/capture.json")
            .exists()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn failed_capture_removes_read_only_staging_tree_and_can_be_retried() {
    let fixture = fixture(None);
    let run_id = "capture-cleanup-retry";
    let workspace = prepare_attempt(&fixture, run_id, run_id).await;
    fs::create_dir(workspace.join("nested")).unwrap();
    fs::write(
        workspace.join("nested/first.txt"),
        "copied before failure\n",
    )
    .unwrap();
    let fifo = workspace.join("zzz-fifo");
    let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // A FIFO is rejected after the lexically earlier nested directory has been copied.
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);

    let request = ExecutorRequest {
        protocol_version: 2,
        operation: Operation::Capture(CaptureRequest {
            run_id: run_id.to_owned(),
            attempt_id: run_id.to_owned(),
            repository: fixture.repo.clone(),
            base_branch: "main".to_owned(),
            base_commit: fixture.base_commit.clone(),
            approved_snapshot_digest: "a".repeat(64),
            scope_digest: "b".repeat(64),
            intent_approval_digest: "c".repeat(64),
        }),
    };
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let error = executor.execute(request.clone()).await.unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::UnsafeWorkspace);

    let attempt_dir = fixture
        .config
        .state_root
        .join("runs")
        .join(run_id)
        .join("attempts")
        .join(run_id);
    assert!(
        !attempt_dir.join(".capture-stage").exists(),
        "failed capture left a private staging tree behind"
    );
    assert!(!attempt_dir.join("capture.json").exists());

    fs::remove_file(fifo).unwrap();
    assert!(matches!(
        executor.execute(request.clone()).await.unwrap(),
        ExecutorResult::Captured(_)
    ));

    let finalized_data = attempt_dir.join("capture-data");
    let finalized_file = finalized_data.join("nested/first.txt");
    let finalized_mode = {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(&finalized_data).unwrap().permissions().mode() & 0o777
    };
    fs::remove_file(attempt_dir.join("capture.json")).unwrap();
    let error = executor.execute(request).await.unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::ArtifactIntegrityFailure);
    assert_eq!(
        fs::read_to_string(finalized_file).unwrap(),
        "copied before failure\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(finalized_data).unwrap().permissions().mode() & 0o777,
            finalized_mode,
            "retry changed finalized artifact permissions"
        );
    }
}

#[cfg(unix)]
#[test]
fn prepare_rejects_unchanged_symlink_and_gitlink_base_entries_before_checkout() {
    for (run_id, entry_kind) in [("base-symlink", "symlink"), ("base-gitlink", "gitlink")] {
        let mut fixture = fixture(None);
        let base_commit = update_fixture_base_with_unsupported_entry(&mut fixture, entry_kind);
        let response = cli_request(&fixture, &prepare_request(&fixture, run_id));

        assert_eq!(response["status"], "error", "{response}");
        assert_eq!(
            response["error"]["code"], "unsupported_base_tree",
            "{response}"
        );
        assert_eq!(
            response["error"]["message"],
            "The approved base contains unsupported tracked entry types."
        );
        assert!(
            !fixture
                .config
                .workspace_root
                .join(run_id)
                .join(run_id)
                .exists(),
            "unsupported base was checked out into a public workspace"
        );

        let source = fixture.temp.path().join("source");
        let source_head = git(
            &fixture.temp.path().join("fixture-home"),
            &source,
            &["rev-parse", "HEAD"],
        );
        assert_eq!(
            String::from_utf8(source_head.stdout).unwrap().trim(),
            base_commit
        );
        let remote_main = git(
            &fixture.temp.path().join("fixture-home"),
            &fixture.remote,
            &[
                "--git-dir",
                fixture.remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main",
            ],
        );
        assert_eq!(
            String::from_utf8(remote_main.stdout).unwrap().trim(),
            base_commit
        );
        assert!(
            remote_branch_tip(&fixture, &format!("agent/{run_id}")).is_empty(),
            "an unsupported base was published"
        );
    }
}

async fn ordinary_publication_request(fixture: &Fixture, run_id: &str) -> (Value, PathBuf, String) {
    use sha2::{Digest, Sha256};
    let workspace = prepare_attempt(fixture, run_id, run_id).await;
    fs::write(workspace.join("ordinary.txt"), "approved ordinary source\n").unwrap();
    let home = fixture.temp.path().join("fixture-home");
    git(&home, &workspace, &["add", "--all"]);
    git(
        &home,
        &workspace,
        &[
            "-c",
            "user.name=Executor Fixture",
            "-c",
            "user.email=executor-fixture@example.invalid",
            "commit",
            "-m",
            "ordinary result",
        ],
    );
    let head = String::from_utf8(git(&home, &workspace, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let source = PrepareRequest {
        issue_id: None,
        run_id: run_id.into(),
        attempt_id: run_id.into(),
        repository: fixture.repo.clone(),
        base_branch: "main".into(),
        base_commit: fixture.base_commit.clone(),
    };
    let inspected = Executor::new(fixture.config.clone())
        .unwrap()
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::InspectSource(source.clone()),
        })
        .await
        .unwrap();
    let ExecutorResult::SourceInspected(observed) = inspected else {
        panic!("source observation missing")
    };
    let digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&observed).unwrap())
    );
    (
        json!({"protocol_version":2,"operation":{"push_workspace":{"source":source,"expected_commit":head,"source_digest":digest}}}),
        workspace,
        head,
    )
}

#[tokio::test]
async fn ordinary_workspace_push_needs_no_capture_or_seal_and_reconciles_the_same_commit() {
    let fixture = fixture(None);
    let (request, workspace, head) = ordinary_publication_request(&fixture, "ordinary-push").await;
    let home = fixture.temp.path().join("fixture-home");
    let marker = fixture.temp.path().join("unapproved-git-hook-ran");
    git(
        &home,
        &workspace,
        &[
            "config",
            "uploadpack.packObjectsHook",
            &format!("touch {}", marker.display()),
        ],
    );
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let mut first = None;
    for _ in 0..2 {
        let response: Value = serde_json::from_slice(
            &executor
                .handle_json(&serde_json::to_vec(&request).unwrap())
                .await,
        )
        .unwrap();
        assert_eq!(response["status"], "ok", "{response}");
        assert_eq!(response["result"]["pushed"]["commit"], head);
        if let Some(prior) = &first {
            assert_eq!(&response, prior);
        } else {
            first = Some(response.clone());
        }
        assert_eq!(remote_branch_tip(&fixture, "agent/ordinary-push"), head);
        assert!(!marker.exists());
    }
    assert!(
        !fixture
            .config
            .state_root
            .join("runs/ordinary-push/artifact.json")
            .exists()
    );
}

#[tokio::test]
async fn ordinary_workspace_push_rejects_source_drift_and_remote_branch_conflicts() {
    for failure in ["dirty", "head", "digest", "remote", "alternates"] {
        let fixture = fixture(None);
        let (mut request, workspace, _head) =
            ordinary_publication_request(&fixture, "ordinary-conflict").await;
        let home = fixture.temp.path().join("fixture-home");
        match failure {
            "dirty" => fs::write(workspace.join("ordinary.txt"), "unapproved drift").unwrap(),
            "head" => {
                request["operation"]["push_workspace"]["expected_commit"] =
                    json!(fixture.base_commit)
            }
            "digest" => {
                request["operation"]["push_workspace"]["source_digest"] = json!("a".repeat(64))
            }
            "remote" => {
                git(
                    &home,
                    &fixture.remote,
                    &[
                        "update-ref",
                        "refs/heads/agent/ordinary-conflict",
                        &fixture.base_commit,
                    ],
                );
            }
            "alternates" => fs::write(
                workspace.join(".git/objects/info/alternates"),
                format!("{}\n", fixture.remote.join("objects").display()),
            )
            .unwrap(),
            _ => unreachable!(),
        }
        let response: Value = serde_json::from_slice(
            &Executor::new(fixture.config.clone())
                .unwrap()
                .handle_json(&serde_json::to_vec(&request).unwrap())
                .await,
        )
        .unwrap();
        assert_eq!(response["status"], "error");
        assert_eq!(
            response["error"]["code"],
            if failure == "remote" {
                "remote_branch_conflict"
            } else {
                "unsafe_workspace"
            },
            "{failure}: {response}"
        );
        assert_eq!(
            remote_branch_tip(&fixture, "agent/ordinary-conflict"),
            if failure == "remote" {
                fixture.base_commit
            } else {
                String::new()
            }
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn ordinary_workspace_push_imports_packed_objects_and_bounds_untrusted_object_data() {
    use std::os::unix::fs::symlink;
    for failure in [None, Some("symlink"), Some("limit")] {
        let mut fixture = fixture(None);
        let (request, workspace, head) =
            ordinary_publication_request(&fixture, "object-import").await;
        let home = fixture.temp.path().join("fixture-home");
        git(&home, &workspace, &["repack", "-a", "-d"]);
        match failure {
            Some("symlink") => symlink(
                fixture.remote.join("objects"),
                workspace.join(".git/objects/unapproved"),
            )
            .unwrap(),
            Some("limit") => {
                fixture.config.max_import_bytes = 1024;
                fs::write(workspace.join(".git/objects/extra-data"), vec![0u8; 2048]).unwrap();
            }
            None => {}
            _ => unreachable!(),
        }
        let response: Value = serde_json::from_slice(
            &Executor::new(fixture.config.clone())
                .unwrap()
                .handle_json(&serde_json::to_vec(&request).unwrap())
                .await,
        )
        .unwrap();
        if let Some(failure) = failure {
            assert_eq!(response["status"], "error", "{response}");
            assert_eq!(
                response["error"]["code"],
                if failure == "limit" {
                    "response_too_large"
                } else {
                    "unsafe_workspace"
                },
                "{response}"
            );
            assert!(remote_branch_tip(&fixture, "agent/object-import").is_empty());
        } else {
            assert_eq!(response["status"], "ok", "{response}");
            assert_eq!(remote_branch_tip(&fixture, "agent/object-import"), head);
        }
    }
}

#[tokio::test]
async fn lookup_workspace_push_observes_remote_without_recreating_a_missing_branch() {
    let fixture = fixture(None);
    let (request, workspace, head) = ordinary_publication_request(&fixture, "lookup-push").await;
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let response: Value = serde_json::from_slice(
        &executor
            .handle_json(&serde_json::to_vec(&request).unwrap())
            .await,
    )
    .unwrap();
    assert_eq!(response["status"], "ok", "{response}");
    let push = json!({"run_id":"lookup-push","attempt_id":"lookup-push","repository":fixture.repo,
        "branch":"agent/lookup-push","base_branch":"main","base_commit":fixture.base_commit,
        "artifact_digest":request["operation"]["push_workspace"]["source_digest"],"frozen_commit":head});
    let lookup = json!({"protocol_version":2,"operation":{"lookup_push":push}});
    fs::remove_file(
        fixture
            .config
            .state_root
            .join("runs/lookup-push/pushed.json"),
    )
    .unwrap();
    fs::write(
        workspace.join("later-local-change.txt"),
        "worktree changed after dispatch",
    )
    .unwrap();
    let response: Value = serde_json::from_slice(
        &executor
            .handle_json(&serde_json::to_vec(&lookup).unwrap())
            .await,
    )
    .unwrap();
    assert_eq!(response["status"], "ok", "{response}");
    assert_eq!(response["result"]["pushed"]["remote_head"], head);
    assert!(
        response["result"]["pushed"]["already_present"]
            .as_bool()
            .unwrap()
    );
    for (field, value) in [
        ("attempt_id", json!("another-attempt")),
        (
            "repository",
            json!({"host":"other.invalid","owner":"owner","name":"repo"}),
        ),
        ("branch", json!("agent/another-run")),
        ("base_branch", json!("trunk")),
        ("base_commit", json!("c".repeat(40))),
        ("frozen_commit", json!(fixture.base_commit)),
        ("artifact_digest", json!("d".repeat(64))),
    ] {
        let mut invalid = lookup.clone();
        invalid["operation"]["lookup_push"][field] = value;
        let response: Value = serde_json::from_slice(
            &executor
                .handle_json(&serde_json::to_vec(&invalid).unwrap())
                .await,
        )
        .unwrap();
        assert_eq!(
            response["status"], "error",
            "invalid lookup {field}: {response}"
        );
        assert_eq!(remote_branch_tip(&fixture, "agent/lookup-push"), head);
    }
    let home = fixture.temp.path().join("fixture-home");
    git(
        &home,
        &fixture.remote,
        &["update-ref", "-d", "refs/heads/agent/lookup-push"],
    );
    let response: Value = serde_json::from_slice(
        &executor
            .handle_json(&serde_json::to_vec(&lookup).unwrap())
            .await,
    )
    .unwrap();
    assert_eq!(
        response["status"], "error",
        "a lookup must not recreate a deleted remote branch: {response}"
    );
    let missing = Command::new("git")
        .args([
            "--git-dir",
            fixture.remote.to_str().unwrap(),
            "show-ref",
            "--verify",
            "refs/heads/agent/lookup-push",
        ])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert_eq!(
        fs::read_to_string(workspace.join("later-local-change.txt")).unwrap(),
        "worktree changed after dispatch"
    );
}

#[tokio::test]
async fn lookup_pull_request_never_posts_and_recovers_a_lost_response() {
    for exists in [false, true] {
        let kind = coding_agent_executor::ForgeKind::Gitea;
        let (api_url, web_url, state, server) = mock_forge(kind, 0, true, false, false);
        let fixture = fixture(Some((kind, api_url, web_url.clone())));
        let commit = commit_workspace(&fixture, "lookup-pr", "change.txt", "safe\n").await;
        state.lock().unwrap().head_commit = commit.clone();
        state.lock().unwrap().base_commit = fixture.base_commit.clone();
        state.lock().unwrap().web_url = web_url;
        push(&fixture, "lookup-pr", &commit).await;
        let request = json!({"run_id":"lookup-pr","attempt_id":"lookup-pr","repository":fixture.repo,
            "branch":"agent/lookup-pr","base_branch":"main","base_commit":fixture.base_commit,
            "artifact_digest":sealed_artifact_digest(&fixture, "lookup-pr"),"frozen_commit":commit,
            "pull_request":{"title":"Lost response","body":"Look up without another POST."}});
        let executor = Executor::new(fixture.config.clone()).unwrap();
        if exists {
            let create = json!({"protocol_version":2,"operation":{"create_pull_request":request}});
            let response: Value = serde_json::from_slice(
                &executor
                    .handle_json(&serde_json::to_vec(&create).unwrap())
                    .await,
            )
            .unwrap();
            assert_eq!(
                response["status"], "error",
                "the mock loses the actual POST response"
            );
        }
        let lookup = json!({"protocol_version":2,"operation":{"lookup_pull_request":request}});
        let response: Value = serde_json::from_slice(
            &executor
                .handle_json(&serde_json::to_vec(&lookup).unwrap())
                .await,
        )
        .unwrap();
        assert_eq!(response["status"] == "ok", exists, "{response}");
        if exists {
            assert_eq!(
                response["result"]["pull_request_created"]["head_commit"],
                commit
            );
        }
        if exists {
            let original = state.lock().unwrap().created_pr.clone().unwrap();
            for (path, value) in [
                (vec!["body"], json!("another run marker")),
                (vec!["head", "sha"], json!("c".repeat(40))),
                (vec!["base", "ref"], json!("trunk")),
            ] {
                let mut wrong = original.clone();
                let mut target = &mut wrong;
                for part in path {
                    target = &mut target[part];
                }
                *target = value;
                state.lock().unwrap().created_pr = Some(wrong);
                let response: Value = serde_json::from_slice(
                    &executor
                        .handle_json(&serde_json::to_vec(&lookup).unwrap())
                        .await,
                )
                .unwrap();
                assert_eq!(
                    response["status"], "error",
                    "a mismatched PR must not count as recovered: {response}"
                );
                assert_eq!(state.lock().unwrap().posts, 1);
            }
        }
        state.lock().unwrap().reject_authorization = true;
        let response: Value = serde_json::from_slice(
            &executor
                .handle_json(&serde_json::to_vec(&lookup).unwrap())
                .await,
        )
        .unwrap();
        assert_eq!(
            response["status"], "error",
            "authentication failure cannot recover a PR: {response}"
        );
        server.join().unwrap();
        assert_eq!(
            state.lock().unwrap().posts,
            usize::from(exists),
            "lookup never creates a PR"
        );
    }
}
