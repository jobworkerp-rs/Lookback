use std::fs;
use std::path::{Path, PathBuf};

use coding_agent_executor::{
    EXECUTOR_VERSION, PROTOCOL_VERSION, PrepareRequest, RepositoryConfig, RepositoryId,
    ResolveBaseRequest, TrustedConfig,
};
use lookback_tauri_lib::coding_agent::{NativeExecutorClient, NativeExecutorError};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const REQUIRED_NATIVE_V2_FEATURES: &[&str] = &[
    "prepare",
    "capture",
    "seal",
    "resolve",
    "artifact_only_publication",
    "push",
    "pull_request",
    "secret_scan_supported_formats_v1",
];
const RUN_ID: &str = "interactive-run";
const ATTEMPT_ID: &str = "attempt-1";
const OPERATION_ID: &str = "base-query-1";
const BASE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

#[cfg(unix)]
struct FakeExecutor {
    _temp: TempDir,
    executable: PathBuf,
    config: PathBuf,
    invocations: PathBuf,
    repository: RepositoryId,
    config_digest: String,
}

#[cfg(unix)]
impl FakeExecutor {
    fn new(features: &[&str], response: Value) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let package = root.join("package");
        fs::create_dir(&package).unwrap();
        fs::set_permissions(&package, fs::Permissions::from_mode(0o700)).unwrap();
        let executable = package.join("executor-fixture");
        let invocations = root.join("invocations.log");
        let capabilities = json!({
            "protocol_version": PROTOCOL_VERSION,
            "executor_version": EXECUTOR_VERSION,
            "features": features,
        });
        let (config, repository) = write_config(root);
        let config_digest = sha256(&fs::read(&config).unwrap());
        let mut response = response;
        if response["result"]["base_resolved"]["trusted_config_digest"] == "CONFIG_DIGEST" {
            response["result"]["base_resolved"]["trusted_config_digest"] = json!(config_digest);
        }
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--capabilities\" ]; then\n  printf 'probe\\n' >> {}\n  printf '%s\\n' {}\nelse\n  printf 'operation\\n' >> {}\n  cat >/dev/null\n  printf '%s\\n' {}\n  printf 'private fixture stderr' >&2\nfi\n",
            shell_quote(invocations.to_str().unwrap()),
            shell_quote(&capabilities.to_string()),
            shell_quote(invocations.to_str().unwrap()),
            shell_quote(&response.to_string()),
        );
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

        Self {
            _temp: temp,
            executable,
            config,
            invocations,
            repository,
            config_digest,
        }
    }

    async fn connect(&self) -> Result<NativeExecutorClient, NativeExecutorError> {
        NativeExecutorClient::connect(
            self.executable.clone(),
            self.config.clone(),
            sha256_file(&self.executable),
        )
        .await
    }

    fn invocations(&self) -> Vec<String> {
        fs::read_to_string(&self.invocations)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

#[cfg(unix)]
#[tokio::test]
async fn resolve_base_requires_its_optional_capability_before_invoking_the_helper() {
    let fixture = FakeExecutor::new(REQUIRED_NATIVE_V2_FEATURES, json!({}));
    let client = fixture.connect().await.unwrap();

    let result = client.resolve_base(request(&fixture)).await;

    assert!(matches!(
        result,
        Err(NativeExecutorError::IncompatibleProtocol)
    ));
    assert_eq!(fixture.invocations(), vec!["probe"]);
}

#[cfg(unix)]
#[tokio::test]
async fn resolve_base_rejects_git_option_like_branch_names_before_invocation() {
    let fixture = FakeExecutor::new(
        &[REQUIRED_NATIVE_V2_FEATURES, &["resolve_base"]].concat(),
        json!({}),
    );
    let client = fixture.connect().await.unwrap();

    let result = client
        .resolve_base(ResolveBaseRequest {
            base_branch: "--upload-pack".to_owned(),
            ..request(&fixture)
        })
        .await;

    assert!(matches!(result, Err(NativeExecutorError::InvalidRequest)));
    assert_eq!(fixture.invocations(), vec!["probe"]);
}

#[cfg(unix)]
#[tokio::test]
async fn resolve_base_returns_only_a_receipt_bound_to_request_and_trusted_config() {
    let fixture = FakeExecutor::new(
        &[REQUIRED_NATIVE_V2_FEATURES, &["resolve_base"]].concat(),
        json!({
            "protocol_version": PROTOCOL_VERSION,
            "status": "ok",
            "result": {
                "base_resolved": {
                    "run_id": RUN_ID,
                    "attempt_id": ATTEMPT_ID,
                    "operation_id": OPERATION_ID,
                    "repository": {"host": "example.test", "owner": "owner", "name": "repo"},
                    "base_branch": "main",
                    "base_commit": BASE_COMMIT,
                    "trusted_config_digest": "CONFIG_DIGEST",
                    "protocol_version": PROTOCOL_VERSION,
                    "executor_version": EXECUTOR_VERSION
                }
            }
        }),
    );
    let client = fixture.connect().await.unwrap();

    let result = client.resolve_base(request(&fixture)).await;

    assert!(result.is_ok());
    let receipt = result.unwrap();
    assert_eq!(receipt.base_commit, BASE_COMMIT);
    assert_eq!(receipt.repository, fixture.repository);
    assert_eq!(receipt.operation_id, OPERATION_ID);
    assert_eq!(receipt.base_branch, "main");
    assert_eq!(receipt.protocol_version, PROTOCOL_VERSION);
    assert_eq!(receipt.executor_version, EXECUTOR_VERSION);
    assert_eq!(receipt.trusted_config_digest, fixture.config_digest);
    assert_eq!(fixture.invocations(), vec!["probe", "operation"]);
}

#[cfg(unix)]
#[tokio::test]
async fn resolve_base_rejects_a_receipt_bound_to_a_different_operation() {
    let fixture = FakeExecutor::new(
        &[REQUIRED_NATIVE_V2_FEATURES, &["resolve_base"]].concat(),
        json!({
            "protocol_version": PROTOCOL_VERSION,
            "status": "ok",
            "result": {
                "base_resolved": {
                    "run_id": RUN_ID,
                    "attempt_id": ATTEMPT_ID,
                    "operation_id": "different-operation",
                    "repository": {"host": "example.test", "owner": "owner", "name": "repo"},
                    "base_branch": "main",
                    "base_commit": BASE_COMMIT,
                    "trusted_config_digest": "CONFIG_DIGEST",
                    "protocol_version": PROTOCOL_VERSION,
                    "executor_version": EXECUTOR_VERSION
                }
            }
        }),
    );
    let client = fixture.connect().await.unwrap();

    let result = client.resolve_base(request(&fixture)).await;

    assert!(matches!(
        result,
        Err(NativeExecutorError::OutcomeUnknown {
            operation: "resolve_base"
        })
    ));
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "Set LOOKBACK_CODING_AGENT_EXECUTOR_BIN to a locally built native executor"]
async fn compiled_executor_freezes_the_resolved_remote_base_before_prepare() {
    use std::os::unix::fs::PermissionsExt;

    let executor_source = std::env::var_os("LOOKBACK_CODING_AGENT_EXECUTOR_BIN")
        .expect("the ignored test requires LOOKBACK_CODING_AGENT_EXECUTOR_BIN");
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    make_private_dir(root);
    let package = root.join("package");
    make_private_dir(&package);
    let executable = package.join("coding-agent-executor");
    fs::copy(executor_source, &executable).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

    let home = root.join("fixture-home");
    let bare = root.join("origin.git");
    let source = root.join("source");
    for directory in [&home, &bare, &source] {
        make_private_dir(directory);
    }
    native_git(
        root,
        &home,
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            bare.to_str().unwrap(),
        ],
    );
    native_git(&source, &home, &["init", "--initial-branch=main"]);
    native_git(
        &source,
        &home,
        &["config", "user.name", "Resolve Base Test"],
    );
    native_git(
        &source,
        &home,
        &["config", "user.email", "resolve-base@example.invalid"],
    );
    fs::write(source.join("base.txt"), "approved base\n").unwrap();
    native_git(&source, &home, &["add", "base.txt"]);
    native_git(&source, &home, &["commit", "-m", "approved base"]);
    let original_commit = native_git(&source, &home, &["rev-parse", "HEAD"]);
    let remote_url = url::Url::from_file_path(&bare).unwrap().to_string();
    native_git(&source, &home, &["remote", "add", "origin", &remote_url]);
    native_git(&source, &home, &["push", "origin", "main"]);

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
        make_private_dir(directory);
    }
    let repository = RepositoryId {
        host: "localhost".to_owned(),
        owner: "owner".to_owned(),
        name: "repo".to_owned(),
    };
    let config = TrustedConfig {
        protocol_version: PROTOCOL_VERSION,
        expected_executor_version: EXECUTOR_VERSION.to_owned(),
        workspace_root: workspace_root.clone(),
        publisher_root,
        state_root,
        credential_root,
        max_import_bytes: 1024 * 1024,
        repositories: vec![RepositoryConfig {
            id: repository.clone(),
            clone_url: remote_url,
            ssh_directory: None,
            allowed_base_branches: vec!["main".to_owned()],
            read_credential_source: None,
            publish_credential_source: None,
            forge: None,
        }],
        credential_sources: Vec::new(),
    };
    let trusted_config = root.join("trusted-executor.json");
    fs::write(&trusted_config, serde_json::to_vec(&config).unwrap()).unwrap();
    fs::set_permissions(&trusted_config, fs::Permissions::from_mode(0o600)).unwrap();
    let client =
        NativeExecutorClient::connect(executable.clone(), trusted_config, sha256_file(&executable))
            .await
            .unwrap();

    let request = ResolveBaseRequest {
        run_id: RUN_ID.to_owned(),
        attempt_id: ATTEMPT_ID.to_owned(),
        operation_id: OPERATION_ID.to_owned(),
        repository: repository.clone(),
        base_branch: "main".to_owned(),
    };
    let receipt = client.resolve_base(request.clone()).await.unwrap();
    assert_eq!(receipt.base_commit, original_commit);
    assert!(!workspace_root.join(RUN_ID).exists());

    fs::write(source.join("base.txt"), "new remote head\n").unwrap();
    native_git(&source, &home, &["add", "base.txt"]);
    native_git(&source, &home, &["commit", "-m", "advance remote head"]);
    native_git(&source, &home, &["push", "origin", "main"]);
    assert_eq!(client.resolve_base(request).await.unwrap(), receipt);

    let prepared = client
        .prepare(PrepareRequest {
            issue_id: None,
            run_id: RUN_ID.to_owned(),
            attempt_id: ATTEMPT_ID.to_owned(),
            repository,
            base_branch: receipt.base_branch,
            base_commit: receipt.base_commit.clone(),
        })
        .await
        .unwrap();
    assert_eq!(prepared.base_commit, original_commit);
    assert_eq!(
        native_git(
            Path::new(&prepared.workspace_path),
            &home,
            &["rev-parse", "HEAD"]
        ),
        original_commit
    );
}

#[cfg(unix)]
fn request(fixture: &FakeExecutor) -> ResolveBaseRequest {
    ResolveBaseRequest {
        run_id: RUN_ID.to_owned(),
        attempt_id: ATTEMPT_ID.to_owned(),
        operation_id: OPERATION_ID.to_owned(),
        repository: fixture.repository.clone(),
        base_branch: "main".to_owned(),
    }
}

#[cfg(unix)]
fn write_config(root: &Path) -> (PathBuf, RepositoryId) {
    use std::os::unix::fs::PermissionsExt;

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
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let repository = RepositoryId {
        host: "example.test".to_owned(),
        owner: "owner".to_owned(),
        name: "repo".to_owned(),
    };
    let config = TrustedConfig {
        protocol_version: PROTOCOL_VERSION,
        expected_executor_version: EXECUTOR_VERSION.to_owned(),
        workspace_root,
        publisher_root,
        state_root,
        credential_root,
        max_import_bytes: 1024 * 1024,
        repositories: vec![RepositoryConfig {
            id: repository.clone(),
            clone_url: "https://example.test/owner/repo.git".to_owned(),
            ssh_directory: None,
            allowed_base_branches: vec!["main".to_owned()],
            read_credential_source: None,
            publish_credential_source: None,
            forge: None,
        }],
        credential_sources: Vec::new(),
    };
    let config_path = root.join("trusted-executor.json");
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
    (config_path, repository)
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn sha256_file(path: &Path) -> String {
    sha256(&fs::read(path).unwrap())
}

#[cfg(unix)]
fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(target_os = "linux")]
fn make_private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(target_os = "linux")]
fn native_git(cwd: &Path, home: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin")
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LC_ALL", "C")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}
