use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use coding_agent_executor::{
    CaptureRequest, EXECUTOR_VERSION, Executor, ExecutorErrorCode, ExecutorRequest, ExecutorResult,
    Operation, PrepareRequest, PushRequest, RepositoryConfig, RepositoryId, SealRequest,
    TrustedConfig,
};
use tempfile::TempDir;
use url::Url;

struct Fixture {
    temp: TempDir,
    config: TrustedConfig,
    repo: RepositoryId,
    base_commit: String,
    marker: PathBuf,
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

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("tempdir");
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
    fs::write(source.join("README.md"), "approved base\n").unwrap();
    git(&home, &source, &["add", "README.md"]);
    git(&home, &source, &["commit", "-m", "approved base"]);
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

    let marker = temp.path().join("host-command-ran");
    let workspace_root = temp.path().join("workspaces");
    let publisher_root = temp.path().join("publishers");
    let state_root = temp.path().join("state");
    let credential_root = temp.path().join("credentials");
    for path in [
        &workspace_root,
        &publisher_root,
        &state_root,
        &credential_root,
    ] {
        fs::create_dir(path).unwrap();
    }
    let repo = RepositoryId {
        host: "code.example".to_owned(),
        owner: "team".to_owned(),
        name: "sample".to_owned(),
    };
    let repository = RepositoryConfig {
        id: repo.clone(),
        clone_url: Url::from_file_path(&remote).unwrap().to_string(),
        ssh_directory: None,
        allowed_base_branches: vec!["main".to_owned()],
        read_credential_source: None,
        publish_credential_source: None,
        forge: None,
    };
    let config = TrustedConfig {
        protocol_version: 2,
        expected_executor_version: EXECUTOR_VERSION.to_owned(),
        workspace_root,
        publisher_root,
        state_root,
        credential_root,
        max_import_bytes: 16 * 1024 * 1024,
        repositories: vec![repository],
        credential_sources: vec![],
    };
    Fixture {
        temp,
        config,
        repo,
        base_commit,
        marker,
    }
}

async fn prepare(fixture: &Fixture, run_id: &str) -> PathBuf {
    let executor = Executor::new(fixture.config.clone()).expect("valid config");
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
        .expect("prepare succeeds");
    match result {
        ExecutorResult::Prepared(prepared) => PathBuf::from(prepared.workspace_path),
        other => panic!("unexpected result: {other:?}"),
    }
}

async fn commit_workspace(
    fixture: &Fixture,
    workspace: &Path,
    name: &str,
    content: &str,
) -> String {
    fs::write(workspace.join(name), content).unwrap();
    let home = fixture.temp.path().join("fixture-home");
    git(&home, workspace, &["add", "--all"]);
    git(
        &home,
        workspace,
        &[
            "-c",
            "user.name=Executor Fixture",
            "-c",
            "user.email=executor-fixture@example.invalid",
            "commit",
            "-m",
            "agent change",
        ],
    );
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let captured = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Capture(CaptureRequest {
                run_id: run_id_for_workspace(fixture, workspace),
                attempt_id: run_id_for_workspace(fixture, workspace),
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
        panic!("expected captured artifact")
    };
    let sealed = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Seal(SealRequest {
                run_id: captured.run_id,
                attempt_id: captured.attempt_id,
                manifest_digest: captured.manifest_digest,
            }),
        })
        .await
        .unwrap();
    let ExecutorResult::Sealed(sealed) = sealed else {
        panic!("expected sealed artifact")
    };
    sealed.commit
}

fn run_id_for_workspace(fixture: &Fixture, workspace: &Path) -> String {
    let relative = workspace
        .strip_prefix(&fixture.config.workspace_root)
        .unwrap();
    relative
        .components()
        .next()
        .unwrap()
        .as_os_str()
        .to_str()
        .unwrap()
        .to_owned()
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
    let receipt: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    receipt["receipt"]["artifact_digest"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn push(
    fixture: &Fixture,
    run_id: &str,
    commit: String,
) -> Result<ExecutorResult, coding_agent_executor::ExecutorError> {
    Executor::new(fixture.config.clone())?
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Push(PushRequest {
                run_id: run_id.to_owned(),
                attempt_id: run_id.to_owned(),
                repository: fixture.repo.clone(),
                branch: format!("agent/{run_id}"),
                base_branch: "main".to_owned(),
                base_commit: fixture.base_commit.clone(),
                artifact_digest: sealed_artifact_digest(fixture, run_id),
                frozen_commit: commit,
            }),
        })
        .await
}

#[tokio::test]
async fn prepare_never_runs_repository_hooks_or_filters_and_is_idempotent() {
    let fixture = fixture();
    let home = fixture.temp.path().join("fixture-home");
    let source = fixture.temp.path().join("source");
    let marker = fixture.marker.to_string_lossy().into_owned();

    // These settings and hooks exist only in the source repository. A clone must
    // not import local config, run post-checkout hooks, or activate attributes.
    git(
        &home,
        &source,
        &[
            "config",
            "filter.executor-test.smudge",
            &format!("touch {marker}"),
        ],
    );
    fs::write(
        source.join(".gitattributes"),
        "filtered.txt filter=executor-test\n",
    )
    .unwrap();
    fs::write(source.join("filtered.txt"), "filter content\n").unwrap();
    git(&home, &source, &["add", ".gitattributes", "filtered.txt"]);
    git(
        &home,
        &source,
        &["commit", "-m", "hostile filter attribute"],
    );
    git(&home, &source, &["push", "origin", "main"]);
    let new_base = String::from_utf8(git(&home, &source, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let hook_dir = source.join(".git/hooks");
    fs::write(
        hook_dir.join("post-checkout"),
        format!("#!/bin/sh\ntouch '{marker}'\n"),
    )
    .unwrap();

    // Approve the new immutable base before asking the executor to prepare it.
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let request = ExecutorRequest {
        protocol_version: 2,
        operation: Operation::Prepare(PrepareRequest {
            issue_id: None,
            run_id: "prepare-security".to_owned(),
            attempt_id: "prepare-security".to_owned(),
            repository: fixture.repo.clone(),
            base_branch: "main".to_owned(),
            base_commit: new_base,
        }),
    };
    let first = executor.execute(request.clone()).await.unwrap();
    let second = executor.execute(request).await.unwrap();
    assert_eq!(first, second);
    assert!(
        !fixture.marker.exists(),
        "repository-controlled command ran on host"
    );
}

#[tokio::test]
async fn prepare_accepts_supported_sha256_base_tree_ids() {
    let mut fixture = fixture();
    let home = fixture.temp.path().join("sha256-home");
    fs::create_dir(&home).unwrap();
    let remote = fixture.temp.path().join("sha256-remote.git");
    fs::create_dir(&remote).unwrap();
    git(
        &home,
        &remote,
        &[
            "init",
            "--bare",
            "--object-format=sha256",
            "--initial-branch=main",
        ],
    );
    let source = fixture.temp.path().join("sha256-source");
    fs::create_dir(&source).unwrap();
    git(
        &home,
        &source,
        &["init", "--object-format=sha256", "--initial-branch=main"],
    );
    git(&home, &source, &["config", "user.name", "SHA-256 Fixture"]);
    git(
        &home,
        &source,
        &["config", "user.email", "sha256-fixture@example.invalid"],
    );
    fs::create_dir(source.join("nested")).unwrap();
    fs::write(source.join("README.md"), "sha256 base\n").unwrap();
    fs::write(source.join("nested/base.txt"), "nested sha256 base\n").unwrap();
    git(&home, &source, &["add", "README.md", "nested/base.txt"]);
    git(&home, &source, &["commit", "-m", "sha256 base"]);
    let base_commit = String::from_utf8(git(&home, &source, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert_eq!(base_commit.len(), 64);
    git(
        &home,
        &source,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&home, &source, &["push", "origin", "main"]);
    fixture.config.repositories[0].clone_url = Url::from_file_path(remote).unwrap().to_string();
    fixture.base_commit = base_commit;

    let workspace = prepare(&fixture, "sha256-base").await;
    assert!(workspace.join("README.md").is_file());
    assert!(workspace.join("nested/base.txt").is_file());
}

#[tokio::test]
async fn publish_uses_only_host_clone_context_not_guest_git_config_or_hooks() {
    let fixture = fixture();
    let workspace = prepare(&fixture, "guest-metadata").await;
    let commit = commit_workspace(&fixture, &workspace, "change.txt", "safe change\n").await;

    let marker = fixture.marker.to_string_lossy().into_owned();
    fs::write(
        workspace.join(".git/config"),
        format!(
            "[core]\n\tsshCommand = touch {marker}\n[credential]\n\thelper = !touch {marker}\n[filter \"hostile\"]\n\tsmudge = touch {marker}\n"
        ),
    )
    .unwrap();
    let hooks = workspace.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-push");
    fs::write(&hook, format!("#!/bin/sh\ntouch '{marker}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let result = push(&fixture, "guest-metadata", commit).await.unwrap();
    assert!(matches!(result, ExecutorResult::Pushed(_)));
    assert!(
        !fixture.marker.exists(),
        "guest-controlled Git metadata ran on host"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sealed_publication_ignores_guest_gitfiles_and_object_alternates() {
    let fixture1 = fixture();
    let workspace = prepare(&fixture1, "gitfile").await;
    let commit = commit_workspace(&fixture1, &workspace, "change.txt", "change\n").await;
    let external_git = fixture1.temp.path().join("external.git");
    fs::rename(workspace.join(".git"), &external_git).unwrap();
    std::os::unix::fs::symlink(&external_git, workspace.join(".git")).unwrap();
    assert!(matches!(
        push(&fixture1, "gitfile", commit).await.unwrap(),
        ExecutorResult::Pushed(_)
    ));

    let fixture2 = fixture();
    let workspace = prepare(&fixture2, "alternates").await;
    let commit = commit_workspace(&fixture2, &workspace, "change.txt", "change\n").await;
    let alternates = workspace.join(".git/objects/info/alternates");
    fs::create_dir_all(alternates.parent().unwrap()).unwrap();
    fs::write(alternates, "/outside/untrusted/object/store\n").unwrap();
    assert!(matches!(
        push(&fixture2, "alternates", commit).await.unwrap(),
        ExecutorResult::Pushed(_)
    ));
}

#[tokio::test]
async fn request_schema_rejects_unknown_fields_and_non_full_commit_ids() {
    let fixture = fixture();
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let unknown: serde_json::Error = serde_json::from_str::<ExecutorRequest>(
        r#"{"protocol_version":2,"operation":{"prepare":{"run_id":"x","attempt_id":"x","repository":{"host":"code.example","owner":"team","name":"sample"},"base_branch":"main","base_commit":"0000000000000000000000000000000000000000","path":"/tmp/escape"}}}"#,
    )
    .expect_err("unknown path must not deserialize");
    let _ = unknown;

    let error = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Prepare(PrepareRequest {
                issue_id: None,
                run_id: "invalid-commit".to_owned(),
                attempt_id: "invalid-commit".to_owned(),
                repository: fixture.repo.clone(),
                base_branch: "main".to_owned(),
                base_commit: "abcd".to_owned(),
            }),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::InvalidCommit);
}

#[tokio::test]
async fn protocol_and_expected_executor_versions_are_paired_fail_closed() {
    let fixture = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_coding-agent-executor"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin")
        .arg("--capabilities")
        .output()
        .unwrap();
    assert!(output.status.success());
    let capabilities: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(capabilities["protocol_version"], 2);
    assert_eq!(capabilities["executor_version"], EXECUTOR_VERSION);
    assert!(
        capabilities["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feature| feature == "secret_scan_supported_formats_v1")
    );
    for feature in ["capture", "seal", "resolve", "artifact_only_publication"] {
        assert!(
            capabilities["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|advertised| advertised == feature)
        );
    }
    assert!(
        !capabilities["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feature| feature == "frozen_commit_import")
    );

    let mut mismatched = fixture.config.clone();
    mismatched.expected_executor_version = "0.0.0".to_owned();
    let error = match Executor::new(mismatched) {
        Ok(_) => panic!("mismatched helper config must be refused"),
        Err(error) => error,
    };
    assert_eq!(error.code, ExecutorErrorCode::IncompatibleExecutor);

    let executor = Executor::new(fixture.config.clone()).unwrap();
    let legacy = executor
        .execute(ExecutorRequest {
            protocol_version: 1,
            operation: Operation::Prepare(PrepareRequest {
                issue_id: None,
                run_id: "legacy-protocol".to_owned(),
                attempt_id: "legacy-protocol".to_owned(),
                repository: fixture.repo.clone(),
                base_branch: "main".to_owned(),
                base_commit: fixture.base_commit.clone(),
            }),
        })
        .await
        .unwrap_err();
    assert_eq!(legacy.code, ExecutorErrorCode::IncompatibleProtocol);

    let error = executor
        .execute(ExecutorRequest {
            protocol_version: 99,
            operation: Operation::Prepare(PrepareRequest {
                issue_id: None,
                run_id: "wrong-protocol".to_owned(),
                attempt_id: "wrong-protocol".to_owned(),
                repository: fixture.repo,
                base_branch: "main".to_owned(),
                base_commit: fixture.base_commit,
            }),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::IncompatibleProtocol);
}

#[test]
fn stdio_cli_returns_strict_versioned_json_errors() {
    let fixture = fixture();
    let config_path = fixture.temp.path().join("executor-config.json");
    fs::write(&config_path, serde_json::to_vec(&fixture.config).unwrap()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_coding-agent-executor"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin")
        .arg("--config")
        .arg(config_path)
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"unknown\":true}\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["protocol_version"], 2);
    assert_eq!(response["status"], "error");
    assert_eq!(response["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn prepare_receipt_rejects_same_run_id_with_changed_payload() {
    let fixture = fixture();
    let workspace = prepare(&fixture, "immutable-prepare").await;
    assert!(workspace.exists());
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let error = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Prepare(PrepareRequest {
                issue_id: None,
                run_id: "immutable-prepare".to_owned(),
                attempt_id: "immutable-prepare".to_owned(),
                repository: fixture.repo,
                base_branch: "main".to_owned(),
                base_commit: "0000000000000000000000000000000000000000".to_owned(),
            }),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::IdempotencyConflict);
}

#[tokio::test]
async fn only_exact_configured_repository_and_generated_agent_branch_are_accepted() {
    let fixture = fixture();
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let error = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Prepare(PrepareRequest {
                issue_id: None,
                run_id: "not-approved".to_owned(),
                attempt_id: "not-approved".to_owned(),
                repository: RepositoryId {
                    host: "evil.example".to_owned(),
                    owner: "team".to_owned(),
                    name: "sample".to_owned(),
                },
                base_branch: "main".to_owned(),
                base_commit: fixture.base_commit.clone(),
            }),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::RepositoryNotApproved);

    let workspace = prepare(&fixture, "branch-check").await;
    let commit = commit_workspace(&fixture, &workspace, "change.txt", "change\n").await;
    let request = PushRequest {
        run_id: "branch-check".to_owned(),
        attempt_id: "branch-check".to_owned(),
        repository: fixture.repo.clone(),
        branch: "main".to_owned(),
        base_branch: "main".to_owned(),
        base_commit: fixture.base_commit.clone(),
        artifact_digest: sealed_artifact_digest(&fixture, "branch-check"),
        frozen_commit: commit,
    };
    let error = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::Push(request.clone()),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ExecutorErrorCode::BranchNotApproved);
}

#[test]
fn explicit_ssh_directory_enables_only_a_configured_ssh_repository() {
    let fixture = fixture();
    let ssh = fixture.temp.path().join("selected-ssh");
    fs::create_dir(&ssh).unwrap();
    let mut config = serde_json::to_value(&fixture.config).unwrap();
    config["repositories"][0]["clone_url"] = "ssh://git@code.example/team/sample.git".into();
    config["repositories"][0]["ssh_directory"] = serde_json::to_value(&ssh).unwrap();
    let configured: TrustedConfig = serde_json::from_value(config.clone())
        .expect("explicit SSH configuration must be supported");
    assert!(Executor::new(configured).is_ok());

    config["repositories"][0]["clone_url"] =
        "ssh://deploy@ssh.code.example:22/team/sample.git".into();
    assert!(Executor::new(serde_json::from_value(config.clone()).unwrap()).is_ok());

    for rejected in [
        serde_json::Value::Null,
        serde_json::json!("relative/ssh"),
        serde_json::to_value(fixture.temp.path().join("missing-ssh")).unwrap(),
    ] {
        config["repositories"][0]["ssh_directory"] = rejected;
        let configured: TrustedConfig = serde_json::from_value(config.clone()).unwrap();
        assert!(Executor::new(configured).is_err());
    }
    config["repositories"][0]["ssh_directory"] = serde_json::to_value(&ssh).unwrap();
    for url in [
        "ssh://git:secret@code.example/team/sample.git",
        "ssh://bad%20user@code.example/team/sample.git",
        "ssh://git@code.example/team/sample.git?command=bad",
        "ext::command",
        "ssh://git@code.example/",
    ] {
        config["repositories"][0]["clone_url"] = url.into();
        let configured: TrustedConfig = serde_json::from_value(config.clone()).unwrap();
        assert!(Executor::new(configured).is_err(), "must reject {url}");
    }
}

#[cfg(unix)]
#[test]
fn an_explicit_ssh_directory_cannot_resolve_through_a_symlink() {
    let fixture = fixture();
    let ssh = fixture.temp.path().join("ssh");
    fs::create_dir(&ssh).unwrap();
    let link = fixture.temp.path().join("linked-ssh");
    std::os::unix::fs::symlink(&ssh, &link).unwrap();
    let mut config = serde_json::to_value(&fixture.config).unwrap();
    config["repositories"][0]["clone_url"] = "ssh://git@code.example/team/sample.git".into();
    config["repositories"][0]["ssh_directory"] = serde_json::to_value(link).unwrap();
    let configured: TrustedConfig = serde_json::from_value(config).unwrap();
    assert!(Executor::new(configured).is_err());
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires local OpenSSH server and loopback sockets; uses isolated temporary keys"]
async fn real_ssh_reads_latest_base_and_prepares_the_issue_branch() {
    use coding_agent_executor::ResolveBaseRequest;
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut fixture = fixture();
    let ssh = fixture.temp.path().join("selected ssh settings");
    fs::create_dir(&ssh).unwrap();
    let host_key = fixture.temp.path().join("server-key");
    for key in [&host_key, &ssh.join("id_ed25519")] {
        let status = Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(key)
            .status()
            .unwrap();
        assert!(status.success());
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let username = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned();
    let remote = fixture.temp.path().join("remote.git");
    let server_config = fixture.temp.path().join("sshd_config");
    fs::write(&server_config, format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile \"{}\"\nStrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nAllowUsers {username}\nForceCommand /usr/bin/git-upload-pack {}\nLogLevel ERROR\n",
        host_key.display(), fixture.temp.path().join("sshd.pid").display(),
        ssh.join("id_ed25519.pub").display(), remote.display())).unwrap();
    fs::write(
        ssh.join("config"),
        format!("Host code.example\n  HostName 127.0.0.1\n  Port {port}\n  User {username}\n"),
    )
    .unwrap();
    fs::write(
        ssh.join("known_hosts"),
        format!(
            "[127.0.0.1]:{port} {}",
            fs::read_to_string(host_key.with_extension("pub")).unwrap()
        ),
    )
    .unwrap();
    let log = fixture.temp.path().join("server.log");
    let mut server = Server(
        Command::new("/usr/sbin/sshd")
            .args(["-D", "-e", "-f"])
            .arg(&server_config)
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "isolated SSH server stopped: {}",
            fs::read_to_string(&log).unwrap()
        );
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "SSH server did not become ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    fixture.config.repositories[0].clone_url = "ssh://code.example/team/sample.git".into();
    fixture.config.repositories[0].ssh_directory = Some(ssh.clone());
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let result = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::ResolveBase(ResolveBaseRequest {
                run_id: "ssh-base".into(),
                attempt_id: "attempt-1".into(),
                operation_id: "resolve-1".into(),
                repository: fixture.repo.clone(),
                base_branch: String::new(),
            }),
        })
        .await
        .unwrap_or_else(|error| {
            panic!(
                "SSH resolution failed: {error:?}; server: {}",
                fs::read_to_string(&log).unwrap()
            )
        });
    let ExecutorResult::BaseResolved(base) = result else {
        panic!("expected base receipt")
    };
    assert_eq!(base.base_branch, "main");
    assert_eq!(base.base_commit, fixture.base_commit);
    let workspace = prepare(&fixture, "ssh-prepare").await;
    assert_eq!(
        fs::read_to_string(workspace.join("README.md")).unwrap(),
        "approved base\n"
    );

    // A different host key must fail, rather than consulting the host home or accepting it.
    fs::write(ssh.join("known_hosts"), "").unwrap();
    let rejected = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::ResolveBase(ResolveBaseRequest {
                run_id: "ssh-rejected".into(),
                attempt_id: "attempt-1".into(),
                operation_id: "resolve-2".into(),
                repository: fixture.repo.clone(),
                base_branch: "main".into(),
            }),
        })
        .await;
    assert_eq!(
        rejected.unwrap_err().code,
        ExecutorErrorCode::GitOperationFailed
    );
}
