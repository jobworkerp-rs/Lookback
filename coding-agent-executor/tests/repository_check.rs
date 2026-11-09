use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use coding_agent_executor::{
    CheckRepositoryChecks, CheckRepositoryRequest, CredentialSourceConfig, EXECUTOR_VERSION,
    Executor, ExecutorRequest, ExecutorResult, ForgeConfig, ForgeKind, Operation, RepositoryConfig,
    RepositoryId, TrustedConfig,
};
use tempfile::TempDir;
use url::Url;

struct Fixture {
    config: TrustedConfig,
    repository: RepositoryId,
    token: String,
    _temp: TempDir,
}

fn git(cwd: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/local/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(cwd)
        .args(args)
        .status()
        .expect("run fixture git");
    assert!(status.success(), "fixture git failed: {args:?}");
}

fn fixture(forge: Option<ForgeConfig>) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "--bare", "--initial-branch=main"]);
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    git(&source, &["init", "--initial-branch=main"]);
    git(&source, &["config", "user.name", "fixture"]);
    git(
        &source,
        &["config", "user.email", "fixture@example.invalid"],
    );
    fs::write(source.join("README.md"), "fixture\n").unwrap();
    git(&source, &["add", "README.md"]);
    git(&source, &["commit", "-m", "fixture"]);
    git(
        &source,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&source, &["push", "origin", "main"]);

    let workspace_root = temp.path().join("workspace");
    let publisher_root = temp.path().join("publisher");
    let state_root = temp.path().join("state");
    let credential_root = temp.path().join("credentials");
    for root in [
        &workspace_root,
        &publisher_root,
        &state_root,
        &credential_root,
    ] {
        fs::create_dir(root).unwrap();
    }
    let token = "repository-check-token-not-for-output".to_owned();
    fs::write(credential_root.join("token"), format!("{token}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            credential_root.join("token"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let repository = RepositoryId {
        host: "forge.local".to_owned(),
        owner: "owner".to_owned(),
        name: "repo".to_owned(),
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
            publish_credential_source: forge.as_ref().map(|_| "publish".to_owned()),
            forge,
        }],
        credential_sources: vec![coding_agent_executor::CredentialSourceConfig {
            id: "publish".to_owned(),
            username: "fixture".to_owned(),
            token_file: PathBuf::from("token"),
        }],
    };
    Fixture {
        config,
        repository,
        token,
        _temp: temp,
    }
}

fn forge(kind: ForgeKind, api_base_url: String) -> ForgeConfig {
    ForgeConfig {
        kind,
        api_base_url,
        web_base_url: "https://forge.local/owner/repo".to_owned(),
    }
}

fn server(
    responses: Vec<(u16, &'static str)>,
) -> (String, Arc<Mutex<Vec<String>>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&requests);
    let handle = thread::spawn(move || {
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            observed.lock().unwrap().push(request);
            let phrase = match status {
                200 => "OK",
                301 => "Moved Permanently",
                401 => "Unauthorized",
                403 => "Forbidden",
                404 => "Not Found",
                _ => "Response",
            };
            write_response(&mut stream, status, phrase, body);
        }
    });
    (format!("http://{address}/api/v3"), requests, handle)
}

fn delayed_body_server() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = read_request(&mut stream);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: 20\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_secs(11));
        let _ = stream.write_all(b"{\"login\":\"late\"}");
    });
    (format!("http://{address}/api/v3"), handle)
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let count = stream.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(bytes).unwrap()
}

fn write_response(stream: &mut TcpStream, status: u16, phrase: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status} {phrase}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    stream.flush().unwrap();
}

fn check_request(repository: RepositoryId) -> ExecutorRequest {
    ExecutorRequest {
        protocol_version: 2,
        operation: Operation::CheckRepository(CheckRepositoryRequest {
            repository,
            checks: None,
        }),
    }
}

fn user_json_with_length(length: usize) -> String {
    let empty = r#"{"login":"fixture","padding":""}"#;
    let marker = "repository-check-secret-marker";
    let padding_length = length - empty.len();
    let mut padding = String::new();
    while padding.len() < padding_length {
        padding.push_str(marker);
    }
    padding.truncate(padding_length);
    let body = format!(r#"{{"login":"fixture","padding":"{padding}"}}"#);
    assert_eq!(body.len(), length);
    body
}

#[tokio::test]
async fn repository_check_disabled_stages_do_not_read_credentials_or_touch_network() {
    let (api, requests, handle) = server(vec![]);
    let fixture = fixture(Some(forge(ForgeKind::GitHub, api)));
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let result = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::CheckRepository(CheckRepositoryRequest {
                repository: fixture.repository,
                checks: Some(CheckRepositoryChecks {
                    git_read: false,
                    forge_api: false,
                }),
            }),
        })
        .await
        .unwrap();
    let ExecutorResult::RepositoryChecked(result) = result else {
        panic!("expected repository check result");
    };
    assert!(
        result
            .stages
            .iter()
            .all(|stage| stage.status == "skipped" && stage.code == "prerequisite_failed")
    );
    assert!(requests.lock().unwrap().is_empty());
    handle.join().unwrap();
}

#[tokio::test]
async fn repository_check_enforces_forge_response_size_boundary_without_returning_body() {
    for (length, expected) in [
        (64 * 1024, "forge_readable"),
        (64 * 1024 + 1, "response_too_large"),
    ] {
        let user_body = Box::leak(user_json_with_length(length).into_boxed_str());
        let responses = if length == 64 * 1024 {
            vec![
                (200, user_body as &'static str),
                (200, r#"{"name":"repo","full_name":"owner/repo"}"#),
            ]
        } else {
            vec![(200, user_body as &'static str)]
        };
        let (api, _requests, handle) = server(responses);
        let fixture = fixture(Some(forge(ForgeKind::GitHub, api)));
        let executor = Executor::new(fixture.config.clone()).unwrap();
        let result = executor
            .execute(check_request(fixture.repository))
            .await
            .unwrap();
        let ExecutorResult::RepositoryChecked(result) = result else {
            panic!("expected repository check result");
        };
        assert_eq!(result.stages[1].code, expected);
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("repository-check-secret-marker")
        );
        handle.join().unwrap();
    }
}

#[tokio::test]
async fn repository_check_git_enabled_forge_disabled_does_not_read_forge_credential() {
    let (api, requests, handle) = server(vec![]);
    let fixture = fixture(Some(forge(ForgeKind::GitHub, api)));
    let token_path = fixture.config.credential_root.join("token");
    let executor = Executor::new(fixture.config.clone()).unwrap();
    fs::remove_file(token_path).unwrap();
    let result = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::CheckRepository(CheckRepositoryRequest {
                repository: fixture.repository,
                checks: Some(CheckRepositoryChecks {
                    git_read: true,
                    forge_api: false,
                }),
            }),
        })
        .await
        .unwrap();
    let ExecutorResult::RepositoryChecked(result) = result else {
        panic!("expected repository check result");
    };
    assert_eq!(result.stages[0].code, "git_readable");
    assert_eq!(result.stages[1].code, "prerequisite_failed");
    assert!(requests.lock().unwrap().is_empty());
    handle.join().unwrap();
}

#[tokio::test]
async fn repository_check_git_disabled_forge_enabled_does_not_read_git_credential() {
    let (api, requests, handle) = server(vec![
        (200, r#"{"login":"fixture"}"#),
        (200, r#"{"name":"repo","full_name":"owner/repo"}"#),
    ]);
    let mut fixture = fixture(Some(forge(ForgeKind::GitHub, api)));
    let read_path = fixture.config.credential_root.join("read-token");
    fs::write(&read_path, "git-read-secret\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&read_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fixture.config.repositories[0].clone_url =
        "https://127.0.0.1.invalid/unreachable.git".to_owned();
    fixture.config.repositories[0].read_credential_source = Some("read".to_owned());
    fixture
        .config
        .credential_sources
        .push(CredentialSourceConfig {
            id: "read".to_owned(),
            username: "fixture".to_owned(),
            token_file: PathBuf::from("read-token"),
        });
    let executor = Executor::new(fixture.config.clone()).unwrap();
    fs::remove_file(read_path).unwrap();
    let result = executor
        .execute(ExecutorRequest {
            protocol_version: 2,
            operation: Operation::CheckRepository(CheckRepositoryRequest {
                repository: fixture.repository,
                checks: Some(CheckRepositoryChecks {
                    git_read: false,
                    forge_api: true,
                }),
            }),
        })
        .await
        .unwrap();
    let ExecutorResult::RepositoryChecked(result) = result else {
        panic!("expected repository check result");
    };
    assert_eq!(result.stages[0].code, "prerequisite_failed");
    assert_eq!(result.stages[1].code, "forge_readable");
    assert_eq!(requests.lock().unwrap().len(), 2);
    handle.join().unwrap();
}

#[tokio::test]
async fn repository_check_returns_bounded_stages_without_writes_or_secrets() {
    let (api, requests, handle) = server(vec![
        (200, r#"{"login":"fixture"}"#),
        (200, r#"{"name":"repo","full_name":"owner/repo"}"#),
    ]);
    let fixture = fixture(Some(forge(ForgeKind::GitHub, api)));
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let response = executor
        .handle_json(&serde_json::to_vec(&check_request(fixture.repository.clone())).unwrap())
        .await;
    let body = String::from_utf8(response).unwrap();
    assert!(!body.contains(&fixture.token));
    assert!(!body.contains("stderr"));
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(
        json["result"]["repository_checked"]["repository"]["name"],
        "repo"
    );
    assert_eq!(
        json["result"]["repository_checked"]["stages"][0]["stage"],
        "git_read"
    );
    assert_eq!(
        json["result"]["repository_checked"]["stages"][0]["status"],
        "passed"
    );
    assert_eq!(
        json["result"]["repository_checked"]["stages"][0]["code"],
        "git_readable"
    );
    assert_eq!(
        json["result"]["repository_checked"]["stages"][1]["stage"],
        "forge_api"
    );
    assert_eq!(
        json["result"]["repository_checked"]["stages"][1]["status"],
        "passed"
    );
    assert_eq!(
        json["result"]["repository_checked"]["stages"][1]["code"],
        "forge_readable"
    );
    assert!(!fixture.config.state_root.join("runs").exists());
    assert!(
        !fixture
            .config
            .publisher_root
            .join("repository-check")
            .exists()
    );
    let requests = requests.lock().unwrap().clone();
    assert!(requests[0].starts_with("GET /api/v3/user HTTP/1.1"));
    assert!(requests[1].starts_with("GET /api/v3/repos/owner/repo HTTP/1.1"));
    assert!(requests.iter().all(|request| request.contains("Bearer ")));
    handle.join().unwrap();
}

#[tokio::test]
async fn repository_check_runs_forge_when_git_read_fails_and_maps_auth_failure() {
    let (api, requests, handle) = server(vec![(401, r#"{"message":"nope"}"#)]);
    let mut git_failure_fixture = fixture(Some(forge(ForgeKind::GitHub, api)));
    git_failure_fixture.config.repositories[0].clone_url =
        "https://127.0.0.1.invalid/no-such.git".to_owned();
    let executor = Executor::new(git_failure_fixture.config.clone()).unwrap();
    let result = executor
        .execute(check_request(git_failure_fixture.repository.clone()))
        .await
        .unwrap();
    let ExecutorResult::RepositoryChecked(result) = result else {
        panic!("expected repository check result");
    };
    assert_eq!(result.stages[0].code, "git_failed");
    assert_eq!(result.stages[0].status, "failed");
    assert_eq!(result.stages[1].code, "authentication_failed");
    assert_eq!(result.stages[1].status, "failed");
    assert_eq!(requests.lock().unwrap().len(), 1);
    handle.join().unwrap();

    let (api, _requests, handle) = server(vec![(200, "not-json")]);
    let fixture = fixture(Some(forge(ForgeKind::Gitea, api)));
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let result = executor
        .execute(check_request(fixture.repository))
        .await
        .unwrap();
    let ExecutorResult::RepositoryChecked(result) = result else {
        panic!("expected repository check result");
    };
    assert_eq!(result.stages[1].code, "invalid_response");
    assert_eq!(result.stages[1].status, "failed");
    handle.join().unwrap();
}

#[tokio::test]
async fn repository_check_skips_unconfigured_forge_and_rejects_invalid_protocol_as_error() {
    let fixture = fixture(None);
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let result = executor
        .execute(check_request(fixture.repository.clone()))
        .await
        .unwrap();
    let ExecutorResult::RepositoryChecked(result) = result else {
        panic!("expected repository check result");
    };
    assert_eq!(result.stages[0].status, "passed");
    assert_eq!(result.stages[1].status, "skipped");
    assert_eq!(result.stages[1].code, "forge_not_configured");

    let mut request = check_request(fixture.repository);
    request.protocol_version = 1;
    let error = executor.execute(request).await.unwrap_err();
    assert_eq!(
        error.code,
        coding_agent_executor::ExecutorErrorCode::IncompatibleProtocol
    );
}

#[tokio::test]
async fn repository_check_skips_forge_when_publication_credential_is_not_configured() {
    let (api, requests, handle) = server(vec![]);
    let mut fixture = fixture(Some(forge(ForgeKind::Gitea, api)));
    fixture.config.repositories[0].publish_credential_source = None;
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let result = executor
        .execute(check_request(fixture.repository))
        .await
        .unwrap();
    let ExecutorResult::RepositoryChecked(result) = result else {
        panic!("expected repository check result");
    };
    assert_eq!(result.stages[1].status, "skipped");
    assert_eq!(result.stages[1].code, "credential_not_configured");
    assert!(requests.lock().unwrap().is_empty());
    handle.join().unwrap();
}

#[tokio::test]
async fn repository_check_maps_redirect_and_invalid_json_without_exposing_response_body() {
    let (api, requests, handle) = server(vec![(301, "redirect")]);
    let fixture = fixture(Some(forge(ForgeKind::Gitea, api)));
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let result = executor
        .execute(check_request(fixture.repository.clone()))
        .await
        .unwrap();
    let ExecutorResult::RepositoryChecked(result) = result else {
        panic!("expected repository check result");
    };
    assert_eq!(result.stages[1].code, "redirect_rejected");
    assert_eq!(result.stages[1].status, "failed");
    assert!(!serde_json::to_string(&result).unwrap().contains("Location"));
    assert_eq!(requests.lock().unwrap().len(), 1);
    handle.join().unwrap();
}

#[tokio::test]
async fn repository_check_maps_body_timeout_to_timeout_code() {
    let (api, handle) = delayed_body_server();
    let fixture = fixture(Some(forge(ForgeKind::GitHub, api)));
    let executor = Executor::new(fixture.config.clone()).unwrap();
    let result = executor
        .execute(check_request(fixture.repository))
        .await
        .unwrap();
    let ExecutorResult::RepositoryChecked(result) = result else {
        panic!("expected repository check result");
    };
    assert_eq!(result.stages[1].code, "timeout");
    assert_eq!(result.stages[1].status, "failed");
    handle.join().unwrap();
}

#[tokio::test]
async fn repository_check_rejects_empty_user_and_wrong_repository_shape() {
    let (api, _requests, handle) = server(vec![(200, "{}")]);
    let empty_user_fixture = fixture(Some(forge(ForgeKind::GitHub, api)));
    let executor = Executor::new(empty_user_fixture.config.clone()).unwrap();
    let result = executor
        .execute(check_request(empty_user_fixture.repository))
        .await
        .unwrap();
    let ExecutorResult::RepositoryChecked(result) = result else {
        panic!("expected repository check result");
    };
    assert_eq!(result.stages[1].code, "invalid_response");
    handle.join().unwrap();

    let (api, _requests, handle) = server(vec![
        (200, r#"{"login":"fixture"}"#),
        (200, r#"{"name":"repo","full_name":"other/repo"}"#),
    ]);
    let wrong_repository_fixture = fixture(Some(forge(ForgeKind::GitHub, api)));
    let executor = Executor::new(wrong_repository_fixture.config.clone()).unwrap();
    let result = executor
        .execute(check_request(wrong_repository_fixture.repository))
        .await
        .unwrap();
    let ExecutorResult::RepositoryChecked(result) = result else {
        panic!("expected repository check result");
    };
    assert_eq!(result.stages[1].code, "invalid_response");
    handle.join().unwrap();
}

#[tokio::test]
async fn repository_check_maps_repository_auth_and_access_statuses() {
    for (status, expected) in [
        (401, "authentication_failed"),
        (403, "access_denied"),
        (404, "repository_not_found"),
    ] {
        let (api, _requests, handle) = server(vec![
            (200, r#"{"login":"fixture"}"#),
            (status, r#"{"message":"hidden"}"#),
        ]);
        let fixture = fixture(Some(forge(ForgeKind::GitHub, api)));
        let executor = Executor::new(fixture.config.clone()).unwrap();
        let result = executor
            .execute(check_request(fixture.repository))
            .await
            .unwrap();
        let ExecutorResult::RepositoryChecked(result) = result else {
            panic!("expected repository check result");
        };
        assert_eq!(result.stages[1].code, expected);
        assert_eq!(result.stages[1].status, "failed");
        handle.join().unwrap();
    }
}
